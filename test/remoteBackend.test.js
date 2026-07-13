'use strict';
// Construction-time validation tests for the remote backends (no et/mosh binary needed).
// Verifies each backend builds a safe argv and refuses bad input BEFORE spawning anything.
const { test } = require('node:test');
const assert = require('node:assert');
const { EtTmuxBackend } = require('../src/main/backends/etTmuxBackend');
const { MoshTmuxBackend } = require('../src/main/backends/moshTmuxBackend');
const { SshTmuxBackend } = require('../src/main/backends/sshTmuxBackend');
const { ControlModeBackend } = require('../src/main/backends/controlModeBackend');
const { ValidationError } = require('../src/shared/validation');

// TC-R1 et backend builds a valid argv for good input
test('TC-R1 EtTmuxBackend constructs with valid argv', () => {
  const b = new EtTmuxBackend({ host: 'me@h:22', session: 'dev', id: 'x1' });
  assert.ok(Array.isArray(b.built.args));
  assert.ok(b.built.args.indexOf('-c') < b.built.args.indexOf('--'), '-c before --');
});

// TC-R2 et backend refuses injection at construction (before any spawn)
test('TC-R2 EtTmuxBackend refuses bad input', () => {
  assert.throws(() => new EtTmuxBackend({ host: '-x', session: 'dev', id: 'x' }), ValidationError);
  assert.throws(() => new EtTmuxBackend({ host: 'h', session: 'a;b', id: 'x' }), ValidationError);
});

// TC-R3 mosh backend builds a valid argv for good input
test('TC-R3 MoshTmuxBackend constructs with valid argv', () => {
  const b = new MoshTmuxBackend({ host: 'me@h:22', session: 'dev' });
  const dd = b.built.args.indexOf('--');
  assert.equal(b.built.args[dd + 1], 'me@h');
  assert.equal(b.built.args[dd + 2], 'sh');
});

// TC-R4 mosh backend refuses injection at construction
test('TC-R4 MoshTmuxBackend refuses bad input', () => {
  assert.throws(() => new MoshTmuxBackend({ host: '-x', session: 'dev' }), ValidationError);
  assert.throws(() => new MoshTmuxBackend({ host: 'h', session: '-X' }), ValidationError);
});

// TC-R5 ssh backend builds a valid argv: keepalives, isolated socket, newer tmux path
test('TC-R5 SshTmuxBackend constructs with valid argv + keepalives + isolated socket', () => {
  const b = new SshTmuxBackend({ host: 'me@h', session: 'dev' });
  assert.equal(b.built.args[0], '-tt');
  assert.ok(b.built.args.join(' ').includes('ServerAliveInterval=15'), 'has keepalive');
  const dd = b.built.args.indexOf('--');
  assert.equal(b.built.args[dd + 1], 'me@h');
  // defaults: newer tmux in ~/.local/bin on an isolated socket (avoids old-tmux version clash)
  assert.deepEqual(b.built.args.slice(dd + 2),
    ['.local/bin/tmux', '-L', 'dtapp', 'new-session', '-A', '-s', 'dev']);
});

// TC-R6 ssh backend refuses injection at construction
test('TC-R6 SshTmuxBackend refuses bad input', () => {
  assert.throws(() => new SshTmuxBackend({ host: '-x', session: 'dev' }), ValidationError);
  assert.throws(() => new SshTmuxBackend({ host: 'h', session: 'a;b' }), ValidationError);
});

// TC-R7 control-mode backend injects -CC and uses a version-tagged socket
test('TC-R7 ControlModeBackend builds -CC argv on a cc socket', () => {
  const b = new ControlModeBackend({ host: 'me@h', session: 'dev', tmuxPath: '/home/u/.local/bin/tmux', tmuxVersion: [3,5] });
  const dd = b.built.args.indexOf('--');
  const tail = b.built.args.slice(dd + 1);
  assert.equal(tail[0], 'me@h');
  assert.equal(tail[1], '/home/u/.local/bin/tmux');
  assert.equal(tail[2], '-CC', '-CC injected right after tmux binary');
  assert.ok(tail.includes('-L') && tail[tail.indexOf('-L') + 1] === 'dtcc3-5', 'cc socket tagged by major-minor');
  // new-session -D -A -s <name>: -D detaches lingering clients (avoids doubled output)
  assert.deepEqual(tail.slice(-5), ['new-session', '-D', '-A', '-s', 'dev']);
});

// TC-R8 control-mode backend refuses bad input at construction
test('TC-R8 ControlModeBackend refuses bad input', () => {
  assert.throws(() => new ControlModeBackend({ host: '-x', session: 'dev' }), ValidationError);
  assert.throws(() => new ControlModeBackend({ host: 'h', session: 'a;b' }), ValidationError);
});

// Helper: a spawned backend whose replies correlate POSITIONALLY. Commands are recorded; `reply`
// feeds the next reply block to the head handler (matching tmux: one reply per command, in
// order). `spawn()` seeds the handshake handler, so the first reply after spawn is the handshake.
function spawnBackend() {
  const b = new ControlModeBackend({ host: 'me@h', session: 'dev', tmuxPath: '/t', tmuxVersion: [3,5] });
  const sent = [];
  b.pty = { write: (s) => sent.push(s.trim()), kill: () => {} };
  b.reply.start();   // seed the handshake handler (no real ssh spawn)
  return { b, sent, reply: (body, ok = true) => b._onEvent({ type: 'reply', ok, body }) };
}

// TC-R9 output is TAGGED with the window that owns its pane (topology reconciled from list-panes).
// Unmapped-pane output is buffered, then flushed on reconcile.
test('TC-R9 output tagged with its window; unmapped output buffered then flushed', () => {
  const { b, reply } = spawnBackend();
  const got = [];
  b.on('data', (d) => got.push(d));
  reply([]);                                 // consume the seeded handshake
  b._onEvent({ type: 'output', pane: '%9', data: 'hi' });  // triggers a coalesced refresh
  assert.equal(got.length, 0, 'output buffered until pane->window known');
  b._refreshWindows();                       // registers the topology handler
  reply(['@1 %9 1 1 zsh']);                  // topology reply -> maps %9->@1, flushes buffer
  assert.equal(got.length, 1);
  assert.deepEqual({ window: got[0].window, data: got[0].data }, { window: '@1', data: 'hi' });
  b._onEvent({ type: 'output', pane: '%9', data: 'more' });  // now mapped -> emitted immediately
  assert.equal(got[1].window, '@1');
});

// TC-R10 write addresses the ACTIVE WINDOW with send-keys -l (literal), buffering until ready.
test('TC-R10 write buffers until ready, then send-keys -l to the active window', () => {
  const { b, sent, reply } = spawnBackend();
  reply([]);                                 // handshake
  b.write('a');
  assert.ok(!sent.some((s) => /send-keys/.test(s)), 'input buffered before ready');
  b._refreshWindows(); reply(['@2 %2 1 1 zsh']);  // @2 active
  b._markReady();
  assert.ok(sent.some((s) => /send-keys -t @2 -l "a"/.test(s)), 'literal send-keys to active window @2');
});

// TC-R11 Enter is sent as a key, text via -l (not a literal newline)
test('TC-R11 newline becomes Enter key', () => {
  const { b, sent, reply } = spawnBackend();
  reply([]); b._refreshWindows(); reply(['@2 %2 1 1 zsh']); b._ready = true;
  b.write('ls\n');
  const keys = sent.filter((s) => /send-keys/.test(s));
  assert.match(keys[0], /send-keys -t @2 -l "ls"/);
  assert.match(keys[1], /send-keys -t @2 Enter/);
});

// TC-R12 attach: topology reply reconciles windows/active; the capture reply paints the ACTIVE
// window. Replies correlate positionally, so the capture never lands in the wrong window.
test('TC-R12 attach reconciles topology and paints active window scrollback', () => {
  const { b, reply } = spawnBackend();
  const wins = []; const painted = [];
  b.on('window', (w) => wins.push(w));
  b.on('data', (d) => painted.push(d));
  reply([]);            // handshake
  b._onAttach();        // sends: list-panes (topology handler), capture (paint handler)
  reply(['@0 %0 1 0 vim', '@1 %1 1 1 zsh']);  // topology
  assert.deepEqual(wins.filter((w) => w.action === 'add').map((w) => w.window), ['@0', '@1']);
  assert.equal(wins.find((w) => w.action === 'active').window, '@1');
  reply(['$ ls', 'file.txt']);                // capture -> painted to active @1
  assert.ok(painted.some((d) => d.window === '@1' && /file\.txt/.test(d.data)), 'scrollback painted to @1');
});

// TC-R13 THE re-visit bug: a fresh window's capture reply is EMPTY. Positional correlation still
// binds each capture reply to the window it was requested for, so a later capture of a DIFFERENT
// window never paints into the wrong tab (the old content-guessing desynced here).
test('TC-R13 empty capture reply does not desync later captures', () => {
  const { b, reply } = spawnBackend();
  const painted = [];
  b.on('data', (d) => painted.push(d));
  reply([]);                                  // handshake
  // topology: two windows exist
  b._refreshWindows(); reply(['@0 %0 1 1 zsh', '@1 %1 1 0 zsh']);
  // lazy-capture @1 (fresh -> EMPTY reply), then lazy-capture @0 (has content)
  b.captureWindow('@1');
  b.captureWindow('@0');
  reply([]);                                  // @1 capture: empty
  reply(['AAA_line1', 'AAA_line2']);          // @0 capture: content
  // @0's content must paint into @0, NOT @1
  assert.ok(painted.some((d) => d.window === '@0' && /AAA_line1/.test(d.data)), '@0 content painted to @0');
  assert.ok(!painted.some((d) => d.window === '@1' && /AAA_line1/.test(d.data)), '@0 content NOT painted into @1');
});

// TC-R14 reconcile is idempotent: replaying the same topology yields no duplicate add events.
test('TC-R14 repeated identical topology does not re-emit adds', () => {
  const { b, reply } = spawnBackend();
  const adds = [];
  b.on('window', (w) => { if (w.action === 'add') adds.push(w.window); });
  reply([]);
  b._refreshWindows(); reply(['@0 %0 1 1 zsh', '@1 %1 1 0 vim']);
  b._refreshWindows(); reply(['@0 %0 1 1 zsh', '@1 %1 1 0 vim']);
  assert.deepEqual(adds, ['@0', '@1'], 'each window added exactly once');
});
