
// Tauri adapter: recreates the `window.terminalAPI` surface the renderer expects (formerly
// provided by Electron's preload.js), implemented over Tauri's invoke() + event listen().
// This is the ONE place that knows we're on Tauri; renderer.ts stays transport-agnostic.
// A feature-gated Tauri test binary installs this bridge as a webview initialization script. It
// supplies deterministic invoke/event behavior before this production adapter loads, so UI tests
// exercise the real adapter and native webview without touching live ssh/tmux sessions.
import type { TerminalAPI } from './types.js';

const uiTestBridge = window.__BUOY_UI_TEST__;
const invoke: TauriCore['invoke'] = uiTestBridge
  ? uiTestBridge.invoke.bind(uiTestBridge)
  : window.__TAURI__.core.invoke.bind(window.__TAURI__.core);
const listen: TauriEvents['listen'] = uiTestBridge
  ? uiTestBridge.listen.bind(uiTestBridge)
  : window.__TAURI__.event.listen.bind(window.__TAURI__.event);

// Bridge a Tauri event -> the callback shape renderer.ts registers (it expects the payload obj).
function on<T>(event: string, cb: (payload: T) => void): void {
  void listen<T>(event, (e) => cb(e.payload));
}

export const terminalAPI: TerminalAPI = {
  getCapabilities: () => invoke('get_runtime_capabilities'),
  // session CRUD — the Rust side owns all argv/validation.
  listSessions: () => invoke('list_sessions'),
  createSession: (meta) => invoke('create_session', { meta }),
  // `win` identifies the xterm instance that produced the bytes. This matters for terminal
  // replies (for example OSC 10/11 colour-query responses): during a tab switch tmux's active
  // window event can lag behind the webview, so routing only by session can feed a reply to the
  // wrong program and make its payload appear as typed text.
  input: (id, data, win) => invoke('session_input', { id, data, win }),
  resize: (id, cols, rows) => invoke('session_resize', { id, cols, rows }),
  ack: (_id: string, _bytes: number) => {},   // backpressure ACK is a no-op in the Tauri port (deferred)
  detach: (id) => invoke('session_detach', { id }), // client only; tmux keeps running
  close: (id, tabs) => invoke('session_close', { id, tabs }), // snapshot + end tmux + History
  resume: (id) => invoke('session_resume', { id }), // move from History before reattaching
  checkOpenSessions: () => invoke('check_open_sessions'),
  kill: (id) => invoke('session_kill', { id }),     // terminate tmux session
  retry: (id) => invoke('session_retry', { id }),   // manual reconnect from a dead session
  forceReconnect: (id) => invoke('session_force_reconnect', { id }),   // reconnect now from any state
  rename: (id, title) => invoke('session_rename', { id, title }),
  openExternal: (url) => invoke('open_external', { url }),
  copyText: (text) => { try { return navigator.clipboard.writeText(String(text == null ? '' : text)); } catch (_) { return Promise.resolve(); } },
  // file viewer (§16): fetch a clicked path's bytes; save bytes to a local file via native dialog
  readRemoteFile: (id, path) => invoke('read_remote_file', { id, path }),   // -> { data_b64, size, truncated }
  saveFile: (dataB64, suggestedName) => invoke('save_file', { dataB64, suggestedName }),
  // §16: opt ONE html document into a scripts-enabled preview -> { url } on the buoyhtml: scheme.
  // Explicit user action only (the viewer's "Enable scripts" button), never automatic.
  enableHtmlScripts: (dataB64) => invoke('enable_html_scripts', { dataB64 }),
  // §18: open a remote-loopback URL via an ssh -L tunnel -> { ok, localUrl }
  openForwardedUrl: (id, url) => invoke('open_forwarded_url', { id, url }),
  getConfig: () => invoke('get_config'),   // { loopbackHosts }
  listTunnels: (id) => invoke('list_tunnels', { id }),        // -> [{ remote, local, active }]
  closeTunnel: (id, remote) => invoke('close_tunnel', { id, remote }),
  forceForward: (id, remote) => invoke('force_forward', { id, remote }),   // same local port; errors if taken
  listHosts: () => invoke('list_hosts'),                      // host history (most-recent-first)
  rememberHost: (host) => invoke('remember_host', { host }),
  // project tabs (§14) — native/control mode
  tabNew: (id) => invoke('tab_new', { id }),
  tabSelect: (id, win) => invoke('tab_select', { id, win }),
  tabClose: (id, win) => invoke('tab_close', { id, win }),
  tabCapture: (id, win) => invoke('tab_capture', { id, win }),
  tabRename: (id, win, title) => invoke('tab_rename', { id, win, title }),   // manual tab rename (empty -> auto)
  // §20: sidebar/tab persistence
  reorderSessions: (ids) => invoke('reorder_sessions', { ids }),             // new project order (ids top->bottom)
  setSessionColor: (id, color) => invoke('set_session_color', { id, color }),// project accent (null clears)
  setLastActive: (id) => invoke('set_last_active', { id }),                  // remember last-focused project
  setLastTab: (id, win) => invoke('set_last_tab', { id, win }),              // remember a project's last tab
  setTabPrefs: (id, tabOrder, tabColor) => invoke('set_tab_prefs', { id, tabOrder, tabColor }), // tabColor=[win,color|null]

  // events main -> renderer
  onData: (cb) => on('session:data', cb),
  onState: (cb) => on('session:state', cb),
  // NOTE: no onError/onInfo. The Rust backend emits no session:error or session:info — those were
  // Electron-era events, and subscribing to them left the failure path looking handled when it
  // wasn't. Spawn failures surface as a createSession REJECTION (see mount()); tmux path/version
  // come back in the createSession result.
  onIntentionalExit: (cb) => on('session:exit', cb),
  onWindow: (cb) => on('session:window', cb),
  onReady: (cb) => on('session:ready', cb),
  onTunnels: (cb) => on('session:tunnels', cb),   // §18: { id, tunnels:[{remote,local}] }
  log: (msg) => { try { console.log('[DT ui]', msg); } catch (_) {} try { void invoke('ui_log', { msg: String(msg) }); } catch (_) {} },
};
window.terminalAPI = terminalAPI;

// Surface uncaught renderer errors to the log file too (a thrown handler would otherwise be
// invisible and look like "stuck connecting").
window.addEventListener('error', (e) => {
  try { void invoke('ui_log', { msg: 'window.error: ' + e.message + ' @ ' + e.filename + ':' + e.lineno }); } catch (_) {}
});
window.addEventListener('unhandledrejection', (e) => {
  const reason = e.reason instanceof Error ? e.reason.message : String(e.reason);
  try { void invoke('ui_log', { msg: 'unhandledrejection: ' + reason }); } catch (_) {}
});
