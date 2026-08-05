'use strict';
// Preload for test/gui-reorder.js: stands in for ui/tauri-api.js (which needs window.__TAURI__ and
// so can't run outside the Tauri shell). Provides the exact `window.terminalAPI` surface
// renderer.js consumes, backed by THREE fake persisted sessions — three is the minimum that
// distinguishes "moved one slot" from "swapped with the neighbour", and lets a drag land in a
// middle slot. Reorder is the only thing under test, so backend calls are just recorded.
window.__errs = [];
window.addEventListener('error', (e) => window.__errs.push(String(e.message)));

const listeners = {};
window.__fire = (ev, payload) => (listeners[ev] || []).forEach((cb) => cb(payload));
window.__reorders = [];       // [[id,…], …] project orders persisted via reorderSessions
window.__tabPrefs = [];       // [[id, windowOrder, colorPair], …] persisted via setTabPrefs
window.__setLastActive = [];  // ids passed to setLastActive (proves whether a project was switched)
window.__tabSelects = [];     // window ids passed to tabSelect (proves whether a tab was switched)

const mk = (n, title) => ({
  id: 's' + n, host: 'me@host-' + n, session: 'dt-s' + n, transport: 'ssh', mode: 'control',
  title, tmuxPath: '/usr/bin/tmux', tmuxVersion: [3, 6], order: n - 1,
  color: null, lastTab: null, tabOrder: [], tabColors: {},
});
const SESSIONS = [mk(1, 'project one'), mk(2, 'project two'), mk(3, 'project three')];

window.terminalAPI = {
  listSessions: async () => SESSIONS.map((s) => ({ ...s })),
  getConfig: async () => ({ loopbackHosts: ['localhost'], lastActive: 's1' }),
  createSession: async (m) => ({ id: m.id, session: m.session, mode: 'control',
    tmuxPath: '/usr/bin/tmux', tmuxVersion: [3, 6] }),
  rename: async (id, title) => ({ ok: true, title }),
  tabRename() {},
  setLastActive: async (id) => { window.__setLastActive.push(id); },
  reorderSessions: async (ids) => { window.__reorders.push(ids.slice()); },
  setTabPrefs: async (id, order, color) => {
    window.__tabPrefs.push([id, order ? order.slice() : null, color]);
  },
  input() {}, resize() {}, ack() {}, close() {}, kill() {}, retry() {}, forceReconnect() {},
  openExternal() {}, copyText: async () => {},
  readRemoteFile: async () => ({}), saveFile: async () => ({}), enableHtmlScripts: async () => ({}),
  openForwardedUrl: async () => ({}), listTunnels: async () => [], closeTunnel() {},
  forceForward: async () => {}, listHosts: async () => [], rememberHost() {},
  tabNew() {}, tabSelect: (id, win) => { window.__tabSelects.push(win); },
  tabClose() {}, tabCapture() {},
  setSessionColor: async () => {}, setLastTab: async () => {},
  onData: (cb) => (listeners.data = listeners.data || []).push(cb),
  onState: (cb) => (listeners.state = listeners.state || []).push(cb),
  onWindow: (cb) => (listeners.window = listeners.window || []).push(cb),
  onReady: (cb) => (listeners.ready = listeners.ready || []).push(cb),
  onIntentionalExit: (cb) => (listeners.exit = listeners.exit || []).push(cb),
  onTunnels: (cb) => (listeners.tunnels = listeners.tunnels || []).push(cb),
  log: () => {},
};
