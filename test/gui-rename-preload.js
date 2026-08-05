'use strict';
// Preload for test/gui-rename.js: stands in for ui/tauri-api.js (which needs window.__TAURI__ and
// so can't run outside the Tauri shell). Provides the exact `window.terminalAPI` surface renderer.js
// consumes, backed by two fake persisted sessions. Rename is the only thing under test, so
// backend calls are just recorded — no ssh, no tmux, no pty.
window.__errs = [];
window.addEventListener('error', (e) => window.__errs.push(String(e.message)));

const listeners = {};
window.__fire = (ev, payload) => (listeners[ev] || []).forEach((cb) => cb(payload));
window.__renames = [];        // [[id, title], …] project renames sent to the backend
window.__tabRenames = [];     // [[id, win, title], …] tab renames sent to tmux
window.__setLastActive = [];  // ids passed to setLastActive (proves whether a project was switched)
window.__tabSelects = [];     // window ids passed to tabSelect (proves whether a tab was switched)

const SESSIONS = [
  { id: 's1', host: 'me@host-one', session: 'dt-s1', transport: 'ssh', mode: 'control',
    title: 'project one', tmuxPath: '/usr/bin/tmux', tmuxVersion: [3, 6], order: 0,
    color: null, lastTab: null, tabOrder: [], tabColors: {} },
  { id: 's2', host: 'me@host-two', session: 'dt-s2', transport: 'ssh', mode: 'control',
    title: 'project two', tmuxPath: '/usr/bin/tmux', tmuxVersion: [3, 6], order: 1,
    color: null, lastTab: null, tabOrder: [], tabColors: {} },
];

window.terminalAPI = {
  listSessions: async () => SESSIONS.map((s) => ({ ...s })),
  getConfig: async () => ({ loopbackHosts: ['localhost'], lastActive: 's1' }),
  createSession: async (m) => ({ id: m.id, session: m.session, mode: 'control',
    tmuxPath: '/usr/bin/tmux', tmuxVersion: [3, 6] }),
  rename: async (id, title) => { window.__renames.push([id, title]); return { ok: true, title }; },
  tabRename: (id, win, title) => { window.__tabRenames.push([id, win, title]); },
  setLastActive: async (id) => { window.__setLastActive.push(id); },
  input() {}, resize() {}, ack() {}, close() {}, kill() {}, retry() {}, forceReconnect() {},
  openExternal() {}, copyText: async () => {},
  readRemoteFile: async () => ({}), saveFile: async () => ({}), enableHtmlScripts: async () => ({}),
  openForwardedUrl: async () => ({}), listTunnels: async () => [], closeTunnel() {},
  forceForward: async () => {}, listHosts: async () => [], rememberHost() {},
  tabNew() {}, tabSelect: (id, win) => { window.__tabSelects.push(win); },
  tabClose() {}, tabCapture() {},
  reorderSessions: async () => {}, setSessionColor: async () => {},
  setLastTab: async () => {}, setTabPrefs: async () => {},
  onData: (cb) => (listeners.data = listeners.data || []).push(cb),
  onState: (cb) => (listeners.state = listeners.state || []).push(cb),
  onWindow: (cb) => (listeners.window = listeners.window || []).push(cb),
  onReady: (cb) => (listeners.ready = listeners.ready || []).push(cb),
  onIntentionalExit: (cb) => (listeners.exit = listeners.exit || []).push(cb),
  onTunnels: (cb) => (listeners.tunnels = listeners.tunnels || []).push(cb),
  log: () => {},
};
