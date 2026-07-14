'use strict';
// Tauri adapter: recreates the `window.terminalAPI` surface the renderer expects (formerly
// provided by Electron's preload.js), implemented over Tauri's invoke() + event listen().
// This is the ONE place that knows we're on Tauri; renderer.js is unchanged.
const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

// Bridge a Tauri event -> the callback shape renderer.js registers (it expects the payload obj).
function on(event, cb) {
  listen(event, (e) => cb(e.payload));
}

window.terminalAPI = {
  // session CRUD — the Rust side owns all argv/validation.
  listSessions: () => invoke('list_sessions'),
  createSession: (meta) => invoke('create_session', { meta }),
  input: (id, data) => invoke('session_input', { id, data }),
  resize: (id, cols, rows) => invoke('session_resize', { id, cols, rows }),
  ack: (_id, _bytes) => {},   // backpressure ACK is a no-op in the Tauri port (deferred)
  close: (id) => invoke('session_close', { id }),   // detach (remote keeps running)
  kill: (id) => invoke('session_kill', { id }),     // terminate remote tmux session
  retry: (_id) => {},                                // supervisor retry deferred
  rename: (id, title) => invoke('session_rename', { id, title }),
  openExternal: (url) => invoke('open_external', { url }),
  copyText: (text) => { try { return navigator.clipboard.writeText(String(text == null ? '' : text)); } catch (_) { return Promise.resolve(); } },
  // file viewer (§16): fetch a clicked path's bytes; save bytes to a local file via native dialog
  readRemoteFile: (id, path) => invoke('read_remote_file', { id, path }),   // -> { data_b64, size, truncated }
  saveFile: (dataB64, suggestedName) => invoke('save_file', { dataB64, suggestedName }),
  // project tabs (§14) — native/control mode
  tabNew: (id) => invoke('tab_new', { id }),
  tabSelect: (id, win) => invoke('tab_select', { id, win }),
  tabClose: (id, win) => invoke('tab_close', { id, win }),
  tabCapture: (id, win) => invoke('tab_capture', { id, win }),

  // events main -> renderer
  onData: (cb) => on('session:data', cb),
  onState: (cb) => on('session:state', cb),
  onError: (cb) => on('session:error', cb),
  onInfo: (cb) => on('session:info', cb),
  onIntentionalExit: (cb) => on('session:exit', cb),
  onWindow: (cb) => on('session:window', cb),
  onReady: (cb) => on('session:ready', cb),
  log: (msg) => { try { console.log('[DT ui]', msg); } catch (_) {} try { invoke('ui_log', { msg: String(msg) }); } catch (_) {} },
};

// Surface uncaught renderer errors to the log file too (a thrown handler would otherwise be
// invisible and look like "stuck connecting").
window.addEventListener('error', (e) => {
  try { invoke('ui_log', { msg: 'window.error: ' + (e && e.message) + ' @ ' + (e && e.filename) + ':' + (e && e.lineno) }); } catch (_) {}
});
window.addEventListener('unhandledrejection', (e) => {
  try { invoke('ui_log', { msg: 'unhandledrejection: ' + (e && e.reason && (e.reason.message || e.reason)) }); } catch (_) {}
});
