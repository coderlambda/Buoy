'use strict';
// Backend stub for gui-notifications.js. It exposes the same event surface as ui/tauri-api.js and
// provides one native-tab session plus one plain/single-tab session.
window.__errs = [];
window.__inputs = [];
window.__terminalCalls = [];
window.addEventListener('error', (e) => window.__errs.push(String(e.message)));

const listeners = {};
window.__fire = (ev, payload) => (listeners[ev] || []).forEach((cb) => cb(payload));

const SESSIONS = [
  { id: 's1', host: 'me@native', session: 'dt-s1', transport: 'ssh', mode: 'control',
    title: 'native project', tmuxPath: '/usr/bin/tmux', tmuxVersion: [3, 6], order: 0,
    color: null, lastTab: null, tabOrder: [], tabColors: {} },
  { id: 's2', host: 'me@plain', session: 'dt-s2', transport: 'ssh', mode: 'plain',
    title: 'plain project', tmuxPath: '/usr/bin/tmux', tmuxVersion: [3, 6], order: 1,
    color: null, lastTab: null, tabOrder: [], tabColors: {} },
];

window.terminalAPI = {
  listSessions: async () => SESSIONS.map((s) => ({ ...s })),
  getConfig: async () => ({ loopbackHosts: ['localhost'], lastActive: 's1' }),
  createSession: async (m) => ({ id: m.id, session: m.session, mode: m.mode,
    tmuxPath: '/usr/bin/tmux', tmuxVersion: [3, 6] }),
  rename: async (id, title) => ({ ok: true, title }),
  input(...args) { window.__inputs.push(args); },
  resize(...args) { window.__terminalCalls.push(['resize', ...args]); },
  ack() {}, close() {}, kill() {}, retry() {}, forceReconnect() {},
  openExternal() {}, copyText: async () => {},
  readRemoteFile: async () => ({}), saveFile: async () => ({}), enableHtmlScripts: async () => ({}),
  openForwardedUrl: async () => ({}), listTunnels: async () => [], closeTunnel() {},
  forceForward: async () => {}, listHosts: async () => [], rememberHost() {},
  tabNew() {}, tabSelect() {}, tabClose() {},
  tabCapture(...args) { window.__terminalCalls.push(['capture', ...args]); },
  tabRename() {},
  reorderSessions: async () => {}, setSessionColor: async () => {},
  setLastActive: async () => {}, setLastTab: async () => {}, setTabPrefs: async () => {},
  onData: (cb) => (listeners.data = listeners.data || []).push(cb),
  onState: (cb) => (listeners.state = listeners.state || []).push(cb),
  onWindow: (cb) => (listeners.window = listeners.window || []).push(cb),
  onReady: (cb) => (listeners.ready = listeners.ready || []).push(cb),
  onIntentionalExit: (cb) => (listeners.exit = listeners.exit || []).push(cb),
  onTunnels: (cb) => (listeners.tunnels = listeners.tunnels || []).push(cb),
  log: () => {},
};
