'use strict';
// Control-mode backend: attaches a remote tmux session with `-CC` and speaks the control
// protocol (DESIGN.md §12) instead of piping a raw tmux screen. This is what makes the
// session look like a NATIVE terminal (no tmux status bar / prefix); tmux windows become
// tabs and panes become splits at the app layer.
//
// This class is a thin COORDINATOR. The mechanisms it orchestrates live in focused units:
//   - ControlModeParser  : bytes            -> structured control events
//   - ReplyChannel       : commands         <-> their reply blocks (positional correlation)
//   - WindowRegistry     : list-panes rows  -> authoritative topology + diff
//   - tmuxKeys           : shell input      -> send-keys command lines
//   - tmuxSocket/validation : launch argv
// Its own job: on ANY topology signal, refresh+reconcile the registry and emit exactly the
// window add/close/rename/active events that changed; tag each 'data' chunk with the WINDOW its
// pane belongs to (so the renderer is a dumb view keyed by window and never guesses).
//
// Input & capture address a WINDOW (send-keys -t @win / capture-pane -t @win); tmux resolves to
// that window's active pane. This removes the async "query the pane id first" step whose gap let
// keystrokes land in the previously-active window.
//
// Contract (ConnectionBackend + control-mode extras):
//   'data'  ({window, pane, data})               pane output, tagged with its window
//   'window'({action,window,name,order})          topology: add/close/rename/active
//   'ready' ()                                    input-ready after attach settle
//   'exit'  (code)
const { EventEmitter } = require('events');
const fs = require('fs');
const util = require('util');
const { buildSshArgs } = require('../../shared/validation');
const { socketName } = require('../../shared/tmuxSocket');
const { ControlModeParser } = require('../../shared/controlModeParser');
const { WindowRegistry } = require('../windowRegistry');
const { ReplyChannel } = require('../replyChannel');
const { encodeSendKeys } = require('../tmuxKeys');
const { spawnEnv } = require('../env');

// Debug logging: writes to BOTH the main-process stderr (visible in the `npm start` terminal)
// AND a log file at /tmp/dt-debug.log (so it can be read after reproducing a bug). Always on;
// set DT_DEBUG=0 to silence. The file is line-appended and self-timestamped.
const DT_LOG = '/tmp/dt-debug.log';
function dlog(...args) {
  if (process.env.DT_DEBUG === '0') return;
  const line = '[DT cc] ' + util.format(...args);
  process.stderr.write(line + '\n');
  try { fs.appendFileSync(DT_LOG, new Date().toISOString() + ' ' + line + '\n'); } catch (_) {}
}

const DEFAULT_SSH_OPTS = [
  '-o', 'ConnectTimeout=8', '-o', 'ServerAliveInterval=15', '-o', 'ServerAliveCountMax=3',
];

// Fields for the authoritative topology listing (one line per pane).
const LIST_FMT = "#{window_id} #{pane_id} #{pane_active} #{window_active} #{window_name}";
const LIST_RE = /^(@\d+) (%\d+) ([01]) ([01])(?: (.*))?$/;

// Bound the "buffered until known" queues so a pane that never maps (or input before a ready
// that never comes) can't grow without limit; drop-oldest keeps the most recent bytes.
const MAX_BUFFER = 4096;
function pushBounded(arr, item) {
  arr.push(item);
  if (arr.length > MAX_BUFFER) arr.shift();
}

class ControlModeBackend extends EventEmitter {
  constructor({ host, session, baseArgs, tmuxPath, socket, tmuxVersion }) {
    super();
    this.session = session;
    this.tmuxPath = tmuxPath || '.local/bin/tmux';
    this.socket = socket || socketName('control', tmuxVersion);
    this.built = buildControlModeSshArgs({
      host, session, baseArgs: [...DEFAULT_SSH_OPTS, ...(baseArgs || [])],
      tmuxPath: this.tmuxPath, socket: this.socket,
    });

    this.pty = null;
    this.parser = new ControlModeParser((ev) => this._onEvent(ev));
    this.reply = new ReplyChannel((line) => { if (this.pty) this.pty.write(line); });
    this.reg = new WindowRegistry();  // THE source of truth for window/pane topology
    this.maxHistory = 2000;           // scrollback lines to back-fill (matches tmux default)

    this._ready = false;              // input un-gated after attach settles
    this._pendingInput = [];          // input buffered until ready
    this._pendingOutput = new Map();  // pane -> [data] buffered until its window is known
    this._refreshQueued = false;      // coalesce bursts of topology signals into one refresh
  }

  spawn({ cols = 80, rows = 24 } = {}) {
    const nodePty = require('@homebridge/node-pty-prebuilt-multiarch');
    this.cols = cols; this.rows = rows;
    this.reply.start();   // seed the handler for tmux's unsolicited connect handshake block
    this.pty = nodePty.spawn('ssh', this.built.args, {
      name: 'xterm-256color', cols, rows, env: spawnEnv(),
    });
    this.pty.onData((d) => this.parser.write(d));
    this.pty.onExit(({ exitCode }) => this.emit('exit', exitCode));
    // Backend OWNS input gating: buffer input until ready, then replay. Attach normally un-gates
    // (fast path, 500ms after %session-changed); this spawn-time fallback guarantees ready even
    // if that signal never arrives, so buffered input can't be stranded (no renderer-side timer).
    this._readyFallback = setTimeout(() => this._markReady(), 5000);
  }

  // --- control-protocol event handling ---------------------------------------------------
  _onEvent(ev) {
    switch (ev.type) {
      case 'output':
        this._routeOutput(ev.pane, ev.data);
        break;
      // Any of these mean the topology MAY have changed — reconcile against truth rather than
      // trying to apply each signal's partial info (which is what raced before).
      case 'window-add':
      case 'window-close':
      case 'window-renamed':
      case 'window-pane-changed':
      case 'session-window-changed':
      case 'layout-change':
        this._queueRefresh();
        break;
      case 'session-changed':
        // Reliable "attached" signal (fires on resume even with no new output). Control mode
        // does NOT replay the existing screen — reconcile topology, then back-fill scrollback.
        this.emit('control', ev);
        this._onAttach();
        break;
      case 'exit':
        this.emit('exit', 0);
        break;
      case 'reply':
        if (!this.reply.onReply(ev)) this.emit('control', ev);   // unexpected extra reply
        break;
      default:
        this.emit('control', ev);
    }
  }

  // --- topology: one reconcile path ------------------------------------------------------
  // Coalesce a burst of topology signals into a single list-panes round-trip. Multiple signals
  // often arrive back-to-back (new-window emits session-window-changed + window-add + rename);
  // one refresh after the burst reflects the final state and avoids redundant queries.
  _queueRefresh() {
    if (this._refreshQueued) return;
    this._refreshQueued = true;
    setImmediate(() => { this._refreshQueued = false; this._refreshWindows(); });
  }

  _refreshWindows() {
    if (!this.pty) return;
    this.reply.send(`list-panes -s -t ${this.session} -F '${LIST_FMT}'`,
      (ev) => this._applyTopology((ev.body || []).filter((l) => l.trim() !== '')));
  }

  // Parse a topology listing and reconcile the registry; emit only what actually changed.
  _applyTopology(lines) {
    const rows = [];
    for (const raw of lines) {
      const m = LIST_RE.exec(raw.trim());
      if (m) rows.push({ win: m[1], pane: m[2], paneActive: m[3] === '1', winActive: m[4] === '1', name: m[5] || '' });
    }
    if (!rows.length) return;
    const diff = this.reg.reconcile(rows);
    dlog('topology: wins=%j active=%s +%j -%j', this.reg.order, this.reg.activeWindow, diff.added, diff.removed);

    diff.added.forEach((win) => this.emit('window', { action: 'add', window: win, order: this.reg.order }));
    diff.renamed.forEach((r) => this.emit('window', { action: 'rename', window: r.win, name: r.name }));
    diff.removed.forEach((win) => this.emit('window', { action: 'close', window: win, order: this.reg.order }));
    if (diff.activeChanged && diff.active) {
      this.emit('window', { action: 'active', window: diff.active, order: this.reg.order });
    }
    // Newly-mapped panes may have had %output buffered before their window was known — flush it.
    diff.newlyMappedPanes.forEach((pane) => this._flushPaneBuffer(pane));
  }

  // --- output routing --------------------------------------------------------------------
  // Tag each chunk with the window that owns its pane. If the pane isn't mapped yet (%output can
  // race ahead of the topology listing on a brand-new window), buffer BY PANE and flush when the
  // reconcile learns the mapping — never guess a window (that mixed tabs' output).
  _routeOutput(pane, data) {
    const win = this.reg.winForPane(pane);
    if (!win) {
      if (!this._pendingOutput.has(pane)) { this._pendingOutput.set(pane, []); this._queueRefresh(); }
      pushBounded(this._pendingOutput.get(pane), data);
      return;
    }
    this.emit('data', { window: win, pane, data });
  }

  _flushPaneBuffer(pane) {
    const buf = this._pendingOutput.get(pane);
    if (!buf) return;
    this._pendingOutput.delete(pane);
    const win = this.reg.winForPane(pane);
    buf.forEach((data) => this.emit('data', { window: win, pane, data }));
  }

  // --- attach: reconcile topology, un-gate input, back-fill the active window's scrollback ---
  _onAttach() {
    if (this._attached) { dlog('session-changed again; already attached'); return; }
    this._attached = true;
    dlog('attach: refreshing topology + capturing active scrollback (session=%s)', this.session);
    this._refreshWindows();
    // Back-fill the active window's screen (control mode doesn't replay it). Target the SESSION
    // (its active pane); the paint handler resolves the window (active) when the reply lands —
    // by then the topology reply above has arrived.
    this.reply.send(`capture-pane -p -e -q -J -N -S -${this.maxHistory} -t ${this.session}`,
      (ev) => this._paintCapture(null, ev.body || []));
    // Un-gate input shortly after attach (send-keys races shell readiness); the topology reply
    // has arrived by then, so the active window is known.
    setTimeout(() => this._markReady(), 500);
  }

  _markReady() {
    if (this._ready) return;
    this._ready = true;
    clearTimeout(this._readyFallback);
    const q = this._pendingInput; this._pendingInput = [];
    q.forEach((d) => this.write(d));
    dlog('ready: input un-gated, active=%s', this.reg.activeWindow);
    this.emit('ready');
  }

  // Paint a captured screen as the initial content of `win` (or the active window for the
  // attach capture, whose target win is resolved now that topology is known).
  _paintCapture(win, rawBody) {
    const body = rawBody.slice();
    while (body.length && body[body.length - 1] === '') body.pop();
    const target = win || this.reg.activeWindow;
    if (!target || !body.length) return;
    dlog('painting %d lines to window %s', body.length, target);
    this.emit('data', { window: target, data: '\x1b[H\x1b[2J' + body.join('\r\n') + '\r\n' });
  }

  // --- input -----------------------------------------------------------------------------
  // Shell input goes through send-keys addressed to the ACTIVE WINDOW (tmux resolves the
  // window's active pane). Addressing the window — not a pane id — removes the async
  // pane-resolution step whose gap let keystrokes land in the previously-active window.
  // Buffered until ready (attach settled + active window known), then replayed in order.
  write(data, win) {
    if (!this.pty) return;
    const target = win || this.reg.activeWindow;
    if (!target || (!this._ready && !win)) { pushBounded(this._pendingInput, data); return; }
    encodeSendKeys(data, target).forEach((line) => this.reply.send(line));
  }

  // Resize the control client so panes aren't stuck at 80x24 (§12, verified on 3.5a).
  resize(cols, rows) {
    this.cols = cols; this.rows = rows;
    this.reply.send(`refresh-client -C ${cols}x${rows}`);
  }

  // --- Window (tab) operations for PROJECTS (§14). Each drives a tmux command; the resulting
  // topology signals trigger a reconcile that emits the tab add/close/rename/active events. ---

  newWindow() { this.reply.send(`new-window -t ${this.session} -c "#{pane_current_path}"`); }
  selectWindow(win) { if (/^@\d+$/.test(win)) this.reply.send(`select-window -t ${win}`); }
  killWindow(win) { if (/^@\d+$/.test(win)) this.reply.send(`kill-window -t ${win}`); }
  renameWindow(win, name) {
    if (!/^@\d+$/.test(win)) return;
    const safe = String(name).replace(/[^\w .\-/]/g, '').slice(0, 60);
    this.reply.send(`rename-window -t ${win} "${safe}"`);
  }

  // Back-fill a window's scrollback on demand (lazy load when a tab is first shown). Addressed
  // by WINDOW; the reply's handler is bound to THIS window, so concurrent captures never clobber
  // each other's target regardless of whether any reply body is empty.
  captureWindow(win) {
    if (!/^@\d+$/.test(win)) return;
    this.reply.send(`capture-pane -p -e -q -J -N -S -${this.maxHistory} -t ${win}`,
      (ev) => this._paintCapture(win, ev.body || []));
  }

  kill() { clearTimeout(this._readyFallback); if (this.pty) { try { this.pty.kill(); } catch (_) {} } }
}

// Build the ssh argv for a control-mode attach: `ssh ... -- host <tmux> -CC -L <sock>
// new-session -D -A -s <name>`. Starts from buildSshArgs (which validates host/session/etc.)
// and inserts the control-mode-only flags: `-CC` after the tmux binary, and `-D` on new-session
// (detaches any lingering control client from a dropped connection — the doubled-output guard).
function buildControlModeSshArgs({ host, session, baseArgs, tmuxPath, socket }) {
  const built = buildSshArgs({ host, session, baseArgs, tmuxPath, socket });
  // built.args = [... '--', host, <tmux>, '-L', <sock>, 'new-session','-A','-s',<name>]
  const dd = built.args.indexOf('--');
  built.args.splice(dd + 3, 0, '-CC');   // dd+1 host, dd+2 tmux binary -> insert after binary
  const ns = built.args.indexOf('new-session');
  if (ns !== -1) built.args.splice(ns + 1, 0, '-D');
  return built;
}

module.exports = { ControlModeBackend };
