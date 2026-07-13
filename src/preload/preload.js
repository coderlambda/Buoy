'use strict';
// Preload: the ONLY bridge between renderer and main (DESIGN.md §6.3).
// Narrow surface — no free-form spawn(argv), no node access in the renderer.
const { contextBridge, ipcRenderer } = require('electron');

contextBridge.exposeInMainWorld('terminalAPI', {
  // session CRUD keyed by {host, session} / {kind:'local'} — main builds all argv.
  listSessions: () => ipcRenderer.invoke('sessions:list'),
  createSession: (meta) => ipcRenderer.invoke('session:create', meta),
  input: (id, data) => ipcRenderer.send('session:input', { id, data }),
  resize: (id, cols, rows) => ipcRenderer.send('session:resize', { id, cols, rows }),
  ack: (id, bytes) => ipcRenderer.send('session:ack', { id, bytes }),
  close: (id) => ipcRenderer.send('session:close', { id }),   // detach (remote keeps running)
  kill: (id) => ipcRenderer.invoke('session:kill', { id }),   // terminate remote tmux session
  retry: (id) => ipcRenderer.send('session:retry', { id }),
  rename: (id, title) => ipcRenderer.invoke('session:rename', { id, title }),
  openExternal: (url) => ipcRenderer.invoke('shell:openExternal', url),  // link plugins (§13)
  copyText: (text) => ipcRenderer.invoke('clipboard:write', text),        // link plugins (§13)
  // project tabs (§14) — native/control mode: manipulate tmux windows
  tabNew: (id) => ipcRenderer.send('tab:new', { id }),
  tabSelect: (id, win) => ipcRenderer.send('tab:select', { id, win }),
  tabClose: (id, win) => ipcRenderer.send('tab:close', { id, win }),
  tabCapture: (id, win) => ipcRenderer.send('tab:capture', { id, win }),   // lazy scrollback (by window)

  // events main -> renderer
  onData: (cb) => ipcRenderer.on('session:data', (_e, p) => cb(p)),
  onState: (cb) => ipcRenderer.on('session:state', (_e, p) => cb(p)),
  onError: (cb) => ipcRenderer.on('session:error', (_e, p) => cb(p)),
  onInfo: (cb) => ipcRenderer.on('session:info', (_e, p) => cb(p)),
  onIntentionalExit: (cb) => ipcRenderer.on('session:intentional-exit', (_e, p) => cb(p)),
  onWindow: (cb) => ipcRenderer.on('session:window', (_e, p) => cb(p)),   // control-mode tabs
  onReady: (cb) => ipcRenderer.on('session:ready', (_e, p) => cb(p)),     // control-mode input-ready
  log: (msg) => ipcRenderer.send('dt:log', msg),                          // debug -> /tmp/dt-debug.log
});
