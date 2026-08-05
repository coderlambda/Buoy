//! Control-mode backend: attaches a remote tmux session with `-CC` over ssh (via portable-pty)
//! and coordinates the pure units (parser, registry, reply channel, tmux_keys) into a stream of
//! app-level events. Port of src/main/backends/controlModeBackend.js.
//!
//! Threading model: the pty reader runs on its own thread and feeds bytes to the parser; parsed
//! events are handled under a Mutex-guarded `Inner` so writes (input/commands) and reads don't
//! race. App-level events are delivered through a `BackendSink` callback (the session layer wires
//! this to Tauri events).

use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use portable_pty::{CommandBuilder, PtySize, native_pty_system, MasterPty};

use crate::control_parser::{ControlEvent, ControlModeParser};
use crate::reply_channel::{ReplyChannel, ReplyKind};
use crate::tmux_keys::encode_send_keys;
use crate::tmux_socket::socket_name;
use crate::transport::{self, Transport};
use crate::validation;
use crate::window_registry::{PaneRow, WindowRegistry};

const LIST_FMT: &str = "#{window_id} #{pane_id} #{pane_active} #{window_active} #{window_name}";
const MAX_HISTORY: u32 = 2000;
const MAX_BUFFER: usize = 4096;

/// App-level events the backend emits (consumed by the session layer -> Tauri events).
#[derive(Debug, Clone)]
pub enum BackendEvent {
    /// Terminal output tagged with the window it belongs to.
    Data { window: String, data: String },
    WindowAdd { window: String, order: Vec<String> },
    WindowClose { window: String, order: Vec<String> },
    WindowRename { window: String, name: String },
    WindowActive { window: String, order: Vec<String> },
    Ready,
    Exit,
}

pub type BackendSink = Arc<dyn Fn(BackendEvent) + Send + Sync>;

#[derive(Clone)]
pub struct BackendConfig {
    pub host: String,
    pub session: String,
    pub tmux_path: String,
    pub tmux_version: Option<(u32, u32)>,
    pub base_args: Vec<String>,
    /// ssh to `host`, or a tmux on THIS machine (kind:'local'). Everything downstream — parser,
    /// registry, supervisor, reattach — is identical either way; see transport.rs.
    pub transport: Transport,
}

impl Default for BackendConfig {
    fn default() -> Self {
        BackendConfig {
            host: String::new(), session: String::new(), tmux_path: "tmux".into(),
            tmux_version: None, base_args: vec![], transport: Transport::Ssh,
        }
    }
}

struct Inner {
    parser: ControlModeParser,
    reg: WindowRegistry,
    reply: ReplyChannel,
    writer: Box<dyn Write + Send>,
    sink: BackendSink,
    session: String,
    ready: bool,
    attached: bool,
    pending_input: Vec<String>,
    pending_output: std::collections::BTreeMap<String, Vec<String>>,
    // Per-pane carry of trailing incomplete UTF-8 bytes. tmux can split a multi-byte char across
    // two %output events for the SAME pane; carrying the partial tail here (keyed by pane, since
    // interleaved panes must not corrupt each other) reassembles it on the next event.
    utf8_carry: std::collections::BTreeMap<String, Vec<u8>>,
    // Coalescing output buffer: window -> accumulated text awaiting the next flush. A redrawing
    // TUI produces ~750 %output events/sec; emitting one Tauri IPC message each floods and crashes
    // the webview. We batch here and a flush thread emits ONE message per window at ~60fps, which
    // the webview (and xterm) can absorb. Ordering is preserved (append in arrival order).
    out_buf: std::collections::BTreeMap<String, String>,
    refresh_queued: bool,
    stopped: bool,   // set on kill() so the flush thread exits
}

pub struct ControlBackend {
    inner: Arc<Mutex<Inner>>,
    _master: Box<dyn MasterPty + Send>,
    child: Arc<Mutex<Box<dyn portable_pty::Child + Send + Sync>>>,
}

impl ControlBackend {
    /// Build ssh argv, spawn the -CC attach, and start the reader thread. Returns the backend
    /// handle; app events flow through `sink`.
    pub fn spawn(cfg: BackendConfig, sink: BackendSink, cols: u16, rows: u16)
        -> Result<Self, validation::ValidationError>
    {
        let socket = socket_name("control", cfg.tmux_version, &cfg.session);
        let (lc_all, lang) = transport::current_locale();
        let spec = transport::spawn_spec(
            cfg.transport, true, &cfg.host, &cfg.session, &cfg.tmux_path, &socket,
            &cfg.base_args, lc_all.as_deref(), lang.as_deref(),
        )?;
        crate::dlog!("ControlBackend.spawn: socket={} {} {}", socket, spec.program, spec.args.join(" "));

        // Every fallible step below returns Err rather than panicking: this runs on the supervisor's
        // detached backoff thread, where a panic would skip the Ok/Err match that schedules the next
        // retry — wedging the session in Connecting with no error and no working Reconnect button.
        let pty = native_pty_system();
        let pair = pty.openpty(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
            .map_err(|e| validation::ValidationError::spawn("openpty", e))?;

        let mut cmd = CommandBuilder::new(&spec.program);
        cmd.args(&spec.args);
        for (k, v) in &spec.env { cmd.env(k, v); }

        let child = pair.slave.spawn_command(cmd)
            .map_err(|e| validation::ValidationError::spawn("tmux client spawn", e))?;
        drop(pair.slave);

        let writer = pair.master.take_writer()
            .map_err(|e| validation::ValidationError::spawn("pty writer", e))?;
        let mut reader = pair.master.try_clone_reader()
            .map_err(|e| validation::ValidationError::spawn("pty reader", e))?;

        let mut reply = ReplyChannel::new();
        reply.start(); // seed handshake handler

        let inner = Arc::new(Mutex::new(Inner {
            parser: ControlModeParser::new(),
            reg: WindowRegistry::new(),
            reply,
            writer,
            sink: sink.clone(),
            session: cfg.session.clone(),
            ready: false,
            attached: false,
            pending_input: Vec::new(),
            pending_output: std::collections::BTreeMap::new(),
            utf8_carry: std::collections::BTreeMap::new(),
            out_buf: std::collections::BTreeMap::new(),
            refresh_queued: false,
            stopped: false,
        }));

        // Reader thread: pump pty bytes -> parser -> event handling.
        {
            let inner = inner.clone();
            let sink = sink.clone();
            thread::spawn(move || {
                crate::dlog!("reader: thread started");
                let mut buf = [0u8; 8192];
                let mut total = 0usize;
                // Decode the control stream as LATIN1 (each byte -> char 0x00..0xFF, lossless and
                // never split). We MUST NOT UTF-8-decode here: tmux can split a multi-byte char
                // (e.g. box-drawing '─' = E2 94 80) ACROSS two %output events — its bytes are not
                // adjacent in the raw stream (there's "\r\n%output %P " between them), so no
                // byte-level carry can rejoin them. Real UTF-8 reassembly happens per-pane at the
                // emit point (route_output), which is the only place a char's bytes are contiguous.
                // '\n'/'\r' are never UTF-8 continuation bytes, so latin1 line-splitting is safe.
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) => { crate::dlog!("reader: EOF after {} bytes", total); break; }
                        Ok(n) => {
                            total += n;
                            let chunk: String = buf[..n].iter().map(|&b| b as char).collect();
                            let events = {
                                let mut g = inner.lock().unwrap();
                                g.parser.write(&chunk)
                            };
                            for ev in events {
                                Inner::handle_event(&inner, ev);
                            }
                        }
                        Err(e) => { crate::dlog!("reader: read error: {}", e); break; }
                    }
                }
                // Reader ended (EOF / error / ssh died): stop the flush thread and flush any tail,
                // then signal exit. Without setting `stopped` here the flush thread would leak for
                // every backend the supervisor spawns over a session's lifetime.
                { let mut g = inner.lock().unwrap(); g.stopped = true; g.flush_output(); }
                sink(BackendEvent::Exit);
            });
        }

        // Output flush thread: coalesce buffered %output into one Tauri message per window at
        // ~60fps. This is what keeps a redrawing TUI (~750 events/sec) from flooding and crashing
        // the webview. Exits when the pty is gone (reader set an exit flag by dropping — we detect
        // via a weak count check: once the only Arc left is ours, stop).
        {
            let inner = inner.clone();
            thread::spawn(move || {
                loop {
                    thread::sleep(Duration::from_millis(16));
                    // Stop when the backend has been dropped (no external owners besides this loop
                    // and the other worker threads' clones would keep it alive; use a killed flag).
                    let mut g = inner.lock().unwrap();
                    if g.stopped { break; }
                    g.flush_output();
                }
            });
        }

        // Spawn-time ready fallback (backend owns input gating): guarantee ready even if
        // %session-changed never arrives, so buffered input can't strand.
        {
            let inner = inner.clone();
            thread::spawn(move || {
                thread::sleep(Duration::from_millis(5000));
                Inner::mark_ready(&inner);
            });
        }

        Ok(ControlBackend {
            inner,
            _master: pair.master,
            child: Arc::new(Mutex::new(child)),
        })
    }

    /// Shell input -> send-keys addressed to the active window (buffered until ready).
    pub fn write(&self, data: &str) {
        self.inner.lock().unwrap().write_input(data);
    }

    pub fn resize(&self, cols: u16, rows: u16) {
        let mut g = self.inner.lock().unwrap();
        g.send(format!("refresh-client -C {}x{}", cols, rows), ReplyKind::Ignore);
        let _ = self._master.resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 });
    }

    pub fn new_window(&self) {
        let mut g = self.inner.lock().unwrap();
        let session = g.session.clone();
        g.send(format!("new-window -t {} -c \"#{{pane_current_path}}\"", session), ReplyKind::Ignore);
    }

    pub fn select_window(&self, win: &str) {
        if !is_win_id(win) { return; }
        let mut g = self.inner.lock().unwrap();
        g.send(format!("select-window -t {}", win), ReplyKind::Ignore);
    }

    pub fn kill_window(&self, win: &str) {
        if !is_win_id(win) { return; }
        let mut g = self.inner.lock().unwrap();
        g.send(format!("kill-window -t {}", win), ReplyKind::Ignore);
    }

    /// Manually rename a window. tmux's `rename-window` also disables automatic-rename for that
    /// window, so a user-set title sticks (stops following the pane title). The title is sanitized
    /// to a safe single-line subset since it is interpolated into a control-mode command.
    pub fn rename_window(&self, win: &str, title: &str) {
        if !is_win_id(win) { return; }
        let clean = sanitize_window_name(title);
        let mut g = self.inner.lock().unwrap();
        if clean.is_empty() {
            // empty title -> re-enable automatic-rename so it follows the pane title again
            g.send(format!("set-window-option -t {} automatic-rename on", win), ReplyKind::Ignore);
        } else {
            g.send(format!("rename-window -t {} \"{}\"", win, clean), ReplyKind::Ignore);
        }
    }

    pub fn capture_window(&self, win: &str) {
        if !is_win_id(win) { return; }
        let mut g = self.inner.lock().unwrap();
        g.send(
            format!("capture-pane -p -e -q -J -N -S -{} -t {}", MAX_HISTORY, win),
            ReplyKind::Capture { window: Some(win.to_string()) },
        );
    }

    pub fn kill(&self) {
        { let mut g = self.inner.lock().unwrap(); g.stopped = true; g.flush_output(); }
        if let Ok(mut c) = self.child.lock() {
            let _ = c.kill();
        }
    }
}

impl Inner {
    fn send(&mut self, line: String, kind: ReplyKind) {
        self.reply.expect(kind);
        let _ = self.writer.write_all(line.as_bytes());
        let _ = self.writer.write_all(b"\n");
        let _ = self.writer.flush();
    }

    fn emit(&self, ev: BackendEvent) {
        (self.sink)(ev);
    }

    fn handle_event(inner: &Arc<Mutex<Inner>>, ev: ControlEvent) {
        match ev {
            ControlEvent::Output { pane, data } => {
                let need_refresh = {
                    let mut g = inner.lock().unwrap();
                    g.route_output(pane, data)
                };
                // Output from a pane we have no window mapping for: schedule a topology refresh to
                // learn it. Done HERE, outside the lock, so it can go through the coalescing
                // queue_refresh (which needs the lock to arm its timer) instead of firing a
                // synchronous list-panes per event — an unmapped pane redrawing at ~750 events/sec
                // used to send ~750 list-panes commands, one per event.
                if need_refresh { Inner::queue_refresh(inner); }
            }
            ControlEvent::WindowAdd { .. }
            | ControlEvent::WindowClose { .. }
            | ControlEvent::WindowRenamed { .. }
            | ControlEvent::WindowPaneChanged { .. }
            | ControlEvent::SessionWindowChanged { .. }
            | ControlEvent::LayoutChange { .. } => {
                crate::dlog!("event: {:?} -> queue refresh", ev);
                Inner::queue_refresh(inner);
            }
            ControlEvent::SessionChanged { .. } => {
                crate::dlog!("event: {:?} -> on_attach", ev);
                Inner::on_attach(inner);
            }
            ControlEvent::Exit { .. } => {
                crate::dlog!("event: {:?} -> emit Exit", ev);
                inner.lock().unwrap().emit(BackendEvent::Exit);
            }
            ControlEvent::Reply { ok, body, .. } => {
                crate::dlog!("event: Reply ok={} bodyLines={}", ok, body.len());
                Inner::on_reply(inner, ok, body);
            }
            ControlEvent::Begin { .. } => {}
            other => { crate::dlog!("event: unhandled {:?}", other); }
        }
    }

    /// Consume one reply block. `ok` is false for a tmux `%error` block, whose body is a DIAGNOSTIC
    /// ("can't find window: @9"), not command output. We must still `take()` the queued kind to keep
    /// the FIFO aligned with the commands we sent (tmux emits exactly one block per command, error or
    /// not), but the body must NOT be interpreted: routing an error body to paint_capture used to
    /// clear-screen the terminal and paint the tmux diagnostic as counterfeit scrollback, and routing
    /// it to apply_topology would parse it as pane rows.
    fn on_reply(inner: &Arc<Mutex<Inner>>, ok: bool, body: Vec<String>) {
        let kind = { inner.lock().unwrap().reply.take() };
        crate::dlog!("on_reply: kind={:?} ok={} bodyLines={}", kind, ok, body.len());
        if !ok {
            crate::dlog!("on_reply: %error for kind={:?}, body discarded: {:?}", kind, body);
            return;
        }
        match kind {
            Some(ReplyKind::Topology) => {
                let lines: Vec<String> = body.into_iter().filter(|l| !l.trim().is_empty()).collect();
                Inner::apply_topology(inner, lines);
            }
            Some(ReplyKind::Capture { window }) => {
                Inner::paint_capture(inner, window, body);
            }
            _ => {}
        }
    }

    fn queue_refresh(inner: &Arc<Mutex<Inner>>) {
        {
            let mut g = inner.lock().unwrap();
            if g.refresh_queued { return; }
            g.refresh_queued = true;
        }
        // Coalesce a burst of topology signals into one list-panes round-trip.
        let inner2 = inner.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(10));
            let mut g = inner2.lock().unwrap();
            g.refresh_queued = false;
            let session = g.session.clone();
            g.send(format!("list-panes -s -t {} -F '{}'", session, LIST_FMT), ReplyKind::Topology);
        });
    }

    fn refresh_now(&mut self) {
        let session = self.session.clone();
        self.send(format!("list-panes -s -t {} -F '{}'", session, LIST_FMT), ReplyKind::Topology);
    }

    fn apply_topology(inner: &Arc<Mutex<Inner>>, lines: Vec<String>) {
        let mut g = inner.lock().unwrap();
        let mut rows = Vec::new();
        for raw in &lines {
            if let Some(r) = parse_list_row(raw.trim()) {
                rows.push(r);
            }
        }
        crate::dlog!("apply_topology: {} rows parsed from {} lines", rows.len(), lines.len());
        if rows.is_empty() {
            // Nothing to reconcile, but still drain buffered input: this early return used to skip
            // the flush_pending_input() at the end of this fn, which is the documented safety net for
            // "mark_ready raced ahead of topology". An empty/unparseable reply must not strand keys.
            g.flush_pending_input();
            return;
        }
        let diff = g.reg.reconcile(&rows);
        let order = g.reg.order();
        crate::dlog!("apply_topology: added={:?} removed={:?} active={:?}", diff.added, diff.removed, diff.active);

        // Emit any buffered output first so window add/active can't be reordered ahead of it.
        g.flush_output();
        for win in &diff.added {
            // A newly-added window already has a name in tmux (auto-derived or a manual rename that
            // persisted server-side). `added` alone carries only the id, so emit its name too —
            // otherwise a reconnect/app-reopen shows the tab as "@N" instead of its real title.
            let name = g.reg.name_of(win).filter(|n| n != win);
            g.emit(BackendEvent::WindowAdd { window: win.clone(), order: order.clone() });
            if let Some(name) = name {
                g.emit(BackendEvent::WindowRename { window: win.clone(), name: latin1_to_utf8(&name) });
            }
        }
        for (win, name) in &diff.renamed {
            g.emit(BackendEvent::WindowRename { window: win.clone(), name: latin1_to_utf8(name) });
        }
        for win in &diff.removed {
            g.emit(BackendEvent::WindowClose { window: win.clone(), order: order.clone() });
        }
        if diff.active_changed {
            if let Some(active) = &diff.active {
                g.emit(BackendEvent::WindowActive { window: active.clone(), order: order.clone() });
            }
        }
        // Flush output buffered before its window was known.
        for pane in &diff.newly_mapped_panes {
            g.flush_pane_buffer(pane);
        }
        // If we became ready before the active window was known (mark_ready ran first on a slow
        // reconnect), input piled up in pending_input with no drain path. Now that reconcile has set
        // active_window, flush it — otherwise the session shows "connected" but ignores keystrokes.
        g.flush_pending_input();
    }

    /// Returns true if the pane has no window mapping yet, meaning the CALLER should schedule a
    /// topology refresh (see handle_event). We can't do it here: queue_refresh takes the same lock
    /// this method is already holding.
    fn route_output(&mut self, pane: String, data: String) -> bool {
        // `data` is LATIN1 (one char per raw byte, from the reader + octal unescape). Convert back
        // to bytes, prepend this pane's carried incomplete UTF-8 tail, decode the valid prefix, and
        // stash any new incomplete tail. This is where a char split across two %output events for
        // the same pane is reassembled.
        let mut bytes: Vec<u8> = Vec::new();
        if let Some(carry) = self.utf8_carry.remove(&pane) { bytes.extend_from_slice(&carry); }
        bytes.extend(data.chars().map(|c| c as u8));
        let (text, tail) = decode_utf8_prefix(&bytes);
        if !tail.is_empty() { self.utf8_carry.insert(pane.clone(), tail); }
        if text.is_empty() { return false; }

        match self.reg.win_for_pane(&pane) {
            // Buffer into out_buf; the flush thread emits one coalesced message per window ~60fps.
            Some(win) => { self.out_buf.entry(win).or_default().push_str(&text); false }
            None => {
                let buf = self.pending_output.entry(pane).or_default();
                if buf.len() >= MAX_BUFFER { buf.remove(0); }
                buf.push(text);
                true
            }
        }
    }

    // Emit all buffered per-window output as one message each, then clear. Called on the flush
    // timer (coalescing the %output firehose) and before any ordered event (window/capture) so
    // buffered output can't jump ahead of a window add/active/scrollback paint.
    fn flush_output(&mut self) {
        if self.out_buf.is_empty() { return; }
        let batch = std::mem::take(&mut self.out_buf);
        for (window, data) in batch {
            if !data.is_empty() {
                (self.sink)(BackendEvent::Data { window, data });
            }
        }
    }

    fn flush_pane_buffer(&mut self, pane: &str) {
        if let Some(buf) = self.pending_output.remove(pane) {
            if let Some(win) = self.reg.win_for_pane(pane) {
                // Route through the coalescing buffer (same ordering guarantees as live output).
                self.out_buf.entry(win).or_default().push_str(&buf.concat());
            }
        }
    }

    fn on_attach(inner: &Arc<Mutex<Inner>>) {
        {
            let mut g = inner.lock().unwrap();
            if g.attached { crate::dlog!("on_attach: already attached, skip"); return; }
            g.attached = true;
            crate::dlog!("on_attach: refreshing topology + capturing scrollback");
            // Tab titles follow the pane title (OSC 2 / `\e]2;…`), the standard, program-agnostic
            // way any tool (Claude Code, vim, a shell PROMPT_COMMAND, …) names its tab. Fall back to
            // the running command when the title is just tmux's default (== the host name), so an
            // untitled shell shows "zsh" rather than the hostname. This drives %window-renamed,
            // which the renderer already maps to the tab label. A manual rename turns automatic-
            // rename off for that window (tmux does this itself when you set-option the name).
            g.send("set-option -g automatic-rename on".into(), ReplyKind::Ignore);
            g.send(
                "set-option -g automatic-rename-format \
                 '#{?#{==:#{pane_title},#{host}},#{pane_current_command},#{pane_title}}'".into(),
                ReplyKind::Ignore,
            );
            g.refresh_now();
            let session = g.session.clone();
            g.send(
                format!("capture-pane -p -e -q -J -N -S -{} -t {}", MAX_HISTORY, session),
                ReplyKind::Capture { window: None },
            );
        }
        // fast-path ready shortly after attach (topology reply has landed)
        let inner2 = inner.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(500));
            Inner::mark_ready(&inner2);
        });
    }

    fn mark_ready(inner: &Arc<Mutex<Inner>>) {
        let mut g = inner.lock().unwrap();
        // A killed/dead backend must never announce Ready. The 5s spawn-time fallback timer is
        // detached and uncancellable, so without this check a backend killed at t=1s still emits
        // Ready at t=5s — and the supervisor would apply it to whatever session is live by then
        // (flipping Dead -> Connected and bricking the Reconnect button, which requires Dead).
        if g.stopped || g.ready { return; }
        g.ready = true;
        crate::dlog!("mark_ready: active={:?} pending_input={}", g.reg.active_window, g.pending_input.len());
        g.flush_pending_input();
        // If we went ready WITHOUT knowing the active window yet (a timer fired before the topology
        // reply landed, or %session-changed was missed on a degraded reconnect), any input the user
        // types now would be buffered with no way to drain it — write()'s send path needs an active
        // window, and mark_ready won't run again. Force a topology refresh so active_window gets set;
        // the resulting apply_topology drains pending_input. Without this the session looks
        // "connected" but silently eats keystrokes.
        if g.reg.active_window.is_none() {
            crate::dlog!("mark_ready: no active window yet -> forcing topology refresh");
            g.refresh_now();
        }
        g.emit(BackendEvent::Ready);
    }

    /// Route shell input: send-keys to the active window when ready, else buffer it (replayed by
    /// flush_pending_input once BOTH ready and the active window are known). Sending requires an
    /// active window even when ready, so a Ready that raced ahead of topology still buffers.
    fn write_input(&mut self, data: &str) {
        match self.reg.active_window.clone() {
            Some(t) if self.ready => {
                for line in encode_send_keys(data, &t) { self.send(line, ReplyKind::Ignore); }
            }
            _ => {
                if self.pending_input.len() >= MAX_BUFFER { self.pending_input.remove(0); }
                self.pending_input.push(data.to_string());
            }
        }
    }

    /// Send any buffered input to the active window. No-op unless we're ready AND know the active
    /// window (send-keys must target a window). Called from mark_ready and, to cover the race where
    /// topology lands after we're already ready, from apply_topology.
    fn flush_pending_input(&mut self) {
        if !self.ready || self.pending_input.is_empty() { return; }
        let Some(target) = self.reg.active_window.clone() else { return };
        let queued: Vec<String> = std::mem::take(&mut self.pending_input);
        crate::dlog!("flush_pending_input: {} chunks -> {}", queued.len(), target);
        for d in queued {
            for line in encode_send_keys(&d, &target) {
                self.send(line, ReplyKind::Ignore);
            }
        }
    }

    fn paint_capture(inner: &Arc<Mutex<Inner>>, window: Option<String>, body: Vec<String>) {
        let mut b = body;
        while b.last().map(|l| l.is_empty()).unwrap_or(false) {
            b.pop();
        }
        let mut g = inner.lock().unwrap();
        g.flush_output();   // paint scrollback AFTER any buffered live output, never before
        let target = window.or_else(|| g.reg.active_window.clone());
        if let Some(t) = target {
            if !b.is_empty() {
                // Capture body lines are LATIN1 (like %output); a whole capture line's bytes are
                // contiguous, so decode each line latin1 -> UTF-8 (no cross-line carry needed).
                let joined = b.join("\r\n");
                let bytes: Vec<u8> = joined.chars().map(|c| c as u8).collect();
                let (text, _tail) = decode_utf8_prefix(&bytes);
                let data = format!("\x1b[H\x1b[2J{}\r\n", text);
                g.emit(BackendEvent::Data { window: t, data });
            }
        }
    }
}

// "@win %pane paneActive winActive name"
fn parse_list_row(line: &str) -> Option<PaneRow> {
    let mut parts = line.splitn(5, ' ');
    let win = parts.next()?;
    let pane = parts.next()?;
    let pane_active = parts.next()?;
    let win_active = parts.next()?;
    let name = parts.next().unwrap_or("");
    if !win.starts_with('@') || !pane.starts_with('%') { return None; }
    if pane_active != "0" && pane_active != "1" { return None; }
    if win_active != "0" && win_active != "1" { return None; }
    Some(PaneRow {
        win: win.to_string(),
        pane: pane.to_string(),
        pane_active: pane_active == "1",
        win_active: win_active == "1",
        name: name.to_string(),
    })
}

fn is_win_id(s: &str) -> bool {
    s.starts_with('@') && s.len() > 1 && s[1..].chars().all(|c| c.is_ascii_digit())
}

/// Re-decode a latin1 string (as produced by the control-stream reader, one char per byte) back to
/// UTF-8, so a window name / title with multibyte chars renders correctly. Lossy on invalid seqs.
fn latin1_to_utf8(s: &str) -> String {
    let bytes: Vec<u8> = s.chars().map(|c| c as u8).collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Sanitize a user-supplied window title for safe interpolation into a `rename-window "…"` command:
/// strip control chars (incl. newlines) and the two chars that break the double-quoted argument
/// (`"` and `\`), collapse whitespace, and cap the length. Keeps everything else (spaces, `*`,
/// unicode) so titles like "* Building widget" survive intact.
///
/// `#` is DOUBLED (`##`), which is tmux's literal escape for it. Left bare, tmux expands the title
/// as a format string: a remote program setting its pane title to `#{pane_current_path}` would put
/// the resolved path in the tab label instead of the literal text (verified against tmux 3.6a;
/// `#()` did not execute there, but doubling closes the whole class rather than betting on version
/// behaviour). Length is capped on the INPUT chars so doubling can't push the result past tmux's
/// limits.
fn sanitize_window_name(s: &str) -> String {
    let mut out = String::new();
    let mut kept = 0usize;
    let mut prev_space = false;
    for c in s.chars() {
        if c.is_control() || c == '"' || c == '\\' { continue; }
        if c == ' ' {
            if prev_space { continue; }
            prev_space = true;
        } else {
            prev_space = false;
        }
        if c == '#' { out.push('#'); }   // tmux literal escape: '#' -> '##'
        out.push(c);
        kept += 1;
        if kept >= 100 { break; }
    }
    // Trailing '#' would be a dangling escape; it can only appear as a complete '##' pair here.
    out.trim().to_string()
}

/// Decode the longest valid UTF-8 prefix of `bytes`. Returns the decoded string and any trailing
/// bytes that form an INCOMPLETE (but so-far-valid) multi-byte sequence, to be carried into the
/// next read. Genuinely invalid bytes (not just truncated) are passed through lossily so we never
/// stall. This is what stops box-drawing/other multi-byte chars from being corrupted to U+FFFD
/// when they straddle a read boundary.
/// ITERATIVE, not recursive: a pane spewing binary (`cat some.jpg`) delivers a long run of invalid
/// bytes, and one stack frame per bad byte overflows the reader thread's 2 MiB stack and ABORTS the
/// process (SIGABRT — not a catchable panic, so it takes every session down with it). Measured
/// thresholds before this was a loop: ~2–4k invalid bytes in debug, ~10–100k in release, against
/// 8192-byte reads. Keep this loop-shaped.
fn decode_utf8_prefix(bytes: &[u8]) -> (String, Vec<u8>) {
    let mut out = String::new();
    let mut rest = bytes;
    loop {
        match std::str::from_utf8(rest) {
            Ok(s) => {
                out.push_str(s);
                return (out, Vec::new());
            }
            Err(e) => {
                let valid = e.valid_up_to();
                // SAFETY: rest[..valid] is valid UTF-8 by definition of valid_up_to().
                out.push_str(unsafe { std::str::from_utf8_unchecked(&rest[..valid]) });
                match e.error_len() {
                    // None => the tail is a truncated-but-valid sequence: carry it for the next read.
                    None => return (out, rest[valid..].to_vec()),
                    // Some(len) => genuinely invalid bytes: emit the replacement char and skip them,
                    // then keep decoding the remainder (so one bad byte can't wedge the stream).
                    Some(len) => {
                        out.push('\u{FFFD}');
                        rest = &rest[valid + len..];
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    // A Write that records everything sent to it (the control-mode commands the backend flushes).
    #[derive(Clone)]
    struct CaptureWriter(Arc<Mutex<Vec<u8>>>);
    impl std::io::Write for CaptureWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> { self.0.lock().unwrap().extend_from_slice(buf); Ok(buf.len()) }
        fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
    }

    // Build an Inner with a capturing writer and a no-op sink, for driving the input/ready/topology
    // state machine directly (no ssh/pty).
    fn test_inner() -> (Arc<Mutex<Inner>>, Arc<Mutex<Vec<u8>>>) {
        let sent = Arc::new(Mutex::new(Vec::<u8>::new()));
        let mut reply = ReplyChannel::new();
        reply.start();
        let inner = Arc::new(Mutex::new(Inner {
            parser: ControlModeParser::new(),
            reg: WindowRegistry::new(),
            reply,
            writer: Box::new(CaptureWriter(sent.clone())),
            sink: Arc::new(|_| {}),
            session: "s".into(),
            ready: false,
            attached: false,
            pending_input: Vec::new(),
            pending_output: std::collections::BTreeMap::new(),
            utf8_carry: std::collections::BTreeMap::new(),
            out_buf: std::collections::BTreeMap::new(),
            refresh_queued: false,
            stopped: false,
        }));
        (inner, sent)
    }

    fn sent_str(sent: &Arc<Mutex<Vec<u8>>>) -> String {
        String::from_utf8_lossy(&sent.lock().unwrap()).into_owned()
    }

    // REGRESSION: "connected but can't input" after a slow reconnect. mark_ready can fire (from its
    // timer) BEFORE the topology reply lands, so active_window is still None. Input typed in that
    // window is buffered into pending_input, and since mark_ready won't run again the buffer would
    // strand — the session looks connected but eats keystrokes. apply_topology (once the reply
    // arrives) must drain pending_input.
    #[test]
    fn tc_cb_ready_before_topology_drains_input_after_topology() {
        let (inner, sent) = test_inner();
        // Ready arrives with no active window known yet.
        Inner::mark_ready(&inner);
        assert!(inner.lock().unwrap().ready, "ready set");
        assert!(inner.lock().unwrap().reg.active_window.is_none(), "no active window yet");

        // User types while "connected but not yet topologized" -> buffered, nothing sent as input.
        sent.lock().unwrap().clear();
        inner.lock().unwrap().write_input("echo hi\n");
        assert!(!sent_str(&sent).contains("send-keys"), "input must NOT be sent before topology");
        assert_eq!(inner.lock().unwrap().pending_input.len(), 1, "input buffered");

        // Topology reply lands -> active window becomes @0 -> buffered input flushes as send-keys.
        Inner::apply_topology(&inner, vec!["@0 %0 1 1 zsh".to_string()]);
        assert_eq!(inner.lock().unwrap().reg.active_window.as_deref(), Some("@0"));
        assert!(sent_str(&sent).contains("send-keys"), "pending input flushed after topology");
        assert!(inner.lock().unwrap().pending_input.is_empty(), "pending_input drained");
    }

    // The reverse order (topology first, then Ready) must also end up sending buffered input.
    #[test]
    fn tc_cb_topology_before_ready_drains_input_on_ready() {
        let (inner, sent) = test_inner();
        Inner::apply_topology(&inner, vec!["@0 %0 1 1 zsh".to_string()]);
        // Not ready yet: input buffers even though we know the window.
        inner.lock().unwrap().write_input("ls\n");
        assert!(!sent_str(&sent).contains("send-keys"), "not sent before ready");
        sent.lock().unwrap().clear();
        Inner::mark_ready(&inner);
        assert!(sent_str(&sent).contains("send-keys"), "flushed on ready");
        assert!(inner.lock().unwrap().pending_input.is_empty());
    }

    #[test]
    fn parse_list_row_ok() {
        let r = parse_list_row("@1 %9 1 1 zsh").unwrap();
        assert_eq!(r.win, "@1");
        assert_eq!(r.pane, "%9");
        assert!(r.pane_active && r.win_active);
        assert_eq!(r.name, "zsh");
    }

    #[test]
    fn parse_list_row_rejects_capture_text() {
        assert!(parse_list_row("$ ls -la").is_none());
        assert!(parse_list_row("total 40").is_none());
    }

    #[test]
    fn is_win_id_checks() {
        assert!(is_win_id("@12"));
        assert!(!is_win_id("@"));
        assert!(!is_win_id("%3"));
        assert!(!is_win_id("@1;rm"));
    }

    #[test]
    fn sanitize_window_name_cases() {
        // keeps spaces, '*', unicode; caps handled elsewhere
        assert_eq!(sanitize_window_name("* Building widget"), "* Building widget");
        // strips the quote/backslash that would break the "…" arg, and control chars/newlines
        assert_eq!(sanitize_window_name("a\"b\\c"), "abc");
        assert_eq!(sanitize_window_name("line1\nline2\t!"), "line1line2!");
        // collapses runs of spaces and trims
        assert_eq!(sanitize_window_name("  a   b  "), "a b");
        // empty stays empty (signals "re-enable auto-rename")
        assert_eq!(sanitize_window_name("   "), "");
        // length cap
        assert!(sanitize_window_name(&"x".repeat(500)).chars().count() <= 100);
        // '#' is doubled so tmux treats it literally instead of expanding a format. Verified against
        // tmux 3.6a: bare '#{pane_current_path}' renames the window to the resolved path; '##{...}'
        // renders the literal text.
        assert_eq!(sanitize_window_name("#{pane_current_path}"), "##{pane_current_path}");
        assert_eq!(sanitize_window_name("#(echo hi)"), "##(echo hi)");
        assert_eq!(sanitize_window_name("C# builds"), "C## builds");
        // the cap counts INPUT chars, so a title of all '#' can at most double in length
        assert!(sanitize_window_name(&"#".repeat(500)).chars().count() <= 200);
    }

    #[test]
    fn utf8_complete_input_has_no_carry() {
        let (s, carry) = decode_utf8_prefix("a─b".as_bytes());
        assert_eq!(s, "a─b");
        assert!(carry.is_empty());
    }

    #[test]
    fn utf8_split_multibyte_is_carried_not_corrupted() {
        // '─' (box drawing) = E2 94 80. Split it across two reads at every interior boundary and
        // verify we reassemble the exact char with NO U+FFFD.
        let full = "x─y".as_bytes().to_vec(); // 78 E2 94 80 79
        for cut in 1..full.len() {
            let (s1, carry) = decode_utf8_prefix(&full[..cut]);
            let mut combined = carry;
            combined.extend_from_slice(&full[cut..]);
            let (s2, tail) = decode_utf8_prefix(&combined);
            let joined = format!("{}{}", s1, s2);
            assert_eq!(joined, "x─y", "cut at {} must reassemble cleanly", cut);
            assert!(tail.is_empty(), "no leftover carry at cut {}", cut);
            assert!(!joined.contains('\u{FFFD}'), "no replacement char at cut {}", cut);
        }
    }

    #[test]
    fn utf8_truncated_tail_becomes_carry() {
        // Just the first 2 bytes of '─' (E2 94): incomplete -> empty decode + 2-byte carry.
        let (s, carry) = decode_utf8_prefix(&[0xE2, 0x94]);
        assert_eq!(s, "");
        assert_eq!(carry, vec![0xE2, 0x94]);
    }

    #[test]
    fn utf8_genuinely_invalid_byte_is_replaced_not_stalled() {
        // A lone 0xFF is invalid (not truncated) -> replacement char, decoding continues.
        let (s, carry) = decode_utf8_prefix(&[b'a', 0xFF, b'b']);
        assert_eq!(s, "a\u{FFFD}b");
        assert!(carry.is_empty());
    }

    // Output from an UNMAPPED pane must ask for ONE coalesced topology refresh, not one list-panes
    // per event. route_output used to call refresh_now() inline with a same-call-scope guard that
    // could never coalesce anything, so a redrawing unmapped pane (~750 %output/sec) sent ~750
    // list-panes commands. It now reports "needs refresh" and handle_event routes that through
    // queue_refresh's 10ms coalescing window.
    #[test]
    fn tc_cb_unmapped_pane_output_does_not_flood_list_panes() {
        let (inner, sent) = test_inner();
        sent.lock().unwrap().clear();

        // 50 events from a pane with no window mapping, through the real event path.
        for i in 0..50 {
            Inner::handle_event(&inner, ControlEvent::Output {
                pane: "%9".into(), data: format!("line{}\r\n", i),
            });
        }
        // Nothing is sent synchronously: queue_refresh arms a timer thread instead.
        let immediate = sent_str(&sent).matches("list-panes").count();
        assert_eq!(immediate, 0, "no synchronous list-panes per output event, got {}", immediate);

        // After the coalescing window, exactly ONE list-panes has gone out for the whole burst.
        std::thread::sleep(Duration::from_millis(80));
        let total = sent_str(&sent).matches("list-panes").count();
        assert_eq!(total, 1, "burst of 50 unmapped-pane events must coalesce to 1 refresh, got {}", total);

        // The output itself is still buffered for replay once the mapping is learned.
        assert_eq!(inner.lock().unwrap().pending_output.get("%9").map(|b| b.len()), Some(50));
    }

    // A MAPPED pane's output must not request a refresh at all — the mapping is already known.
    #[test]
    fn tc_cb_mapped_pane_output_requests_no_refresh() {
        let (inner, sent) = test_inner();
        Inner::apply_topology(&inner, vec!["@0 %0 1 1 zsh".to_string()]);
        sent.lock().unwrap().clear();

        let needs = inner.lock().unwrap().route_output("%0".into(), "hello".into());
        assert!(!needs, "mapped pane must not ask for a topology refresh");
        std::thread::sleep(Duration::from_millis(40));
        assert_eq!(sent_str(&sent).matches("list-panes").count(), 0, "no refresh for a mapped pane");
    }
}
