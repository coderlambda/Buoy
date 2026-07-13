'use strict';
// Live GUI verification of the projects/multi-tab feature (§14): real Electron + renderer +
// backend against a live host. Steps: create project -> run cmd in tab A -> open tab B (+) ->
// run cmd in tab B -> switch back to A -> assert each tab shows ONLY its own output ->
// close tab B -> assert it's gone.
// Usage: HOST=user@host TMUX=/path node_modules/.bin/electron test/gui-multitab.js
const { app, BrowserWindow, ipcMain } = require('electron');
const { Supervisor } = require('../src/main/supervisor');
const { ControlModeBackend } = require('../src/main/backends/controlModeBackend');
const { execFileSync } = require('child_process');
const path = require('path');

const HOST = process.env.HOST;
const TMUX = process.env.TMUX || '/home/yitong/.local/bin/tmux';
const TVER = [3, 7];               // host tmux is 3.7b
const SOCK = `dtcc${TVER[0]}-${TVER[1]}`;  // must match ControlModeBackend's derived socket
const SESSION = 'gmt';
let failures = 0;
const check = (c, m) => { console.log((c ? 'ok   ' : 'FAIL ') + m); if (!c) failures++; };
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const ssh = (cmd) => execFileSync('ssh', ['-o', 'BatchMode=yes', '--', HOST, cmd],
  { env: { ...process.env, PATH: '/opt/homebrew/bin:' + process.env.PATH }, encoding: 'utf8', timeout: 15000 });

app.disableHardwareAcceleration();
app.whenReady().then(async () => {
  if (!HOST) { console.log('FAIL set HOST'); return app.exit(2); }
  ssh(`${TMUX} -L ${SOCK} kill-session -t ${SESSION} 2>/dev/null; true`);

  let sup;
  const win = new BrowserWindow({ show: false, webPreferences: {
    preload: path.join(__dirname, '../src/preload/preload.js'),
    contextIsolation: true, sandbox: true, offscreen: true } });
  const backendOf = () => sup && sup.backend;
  ipcMain.on('dt:log', () => {});
  ipcMain.handle('sessions:list', () => [{ id: 'g1', host: HOST, session: SESSION, transport: 'ssh', mode: 'control', tmuxPath: TMUX, tmuxVersion: TVER, title: 'proj', order: 0 }]);
  ipcMain.handle('session:create', () => {
    sup = new Supervisor({ makeBackend: () => new ControlModeBackend({ host: HOST, session: SESSION, tmuxPath: TMUX, tmuxVersion: TVER }), opts: { connectTimeoutMs: 3000 } });
    sup.on('data', (d) => win.webContents.send('session:data', typeof d === 'string' ? { id: 'g1', data: d } : { id: 'g1', window: d.window, pane: d.pane, data: d.data }));
    sup.on('state', (x) => win.webContents.send('session:state', { id: 'g1', state: x }));
    sup.on('window', (w) => win.webContents.send('session:window', { id: 'g1', ...w }));
    sup.on('ready', () => win.webContents.send('session:ready', { id: 'g1' }));
    sup.start({ cols: 90, rows: 30 });
    return { id: 'g1', session: SESSION };
  });
  ipcMain.on('session:input', (_e, { data }) => { if (sup) sup.write(data); });
  ipcMain.on('session:resize', (_e, { cols, rows }) => { if (sup) sup.resize(cols, rows); });
  ['session:ack', 'session:close', 'session:retry'].forEach((c) => ipcMain.on(c, () => {}));
  ipcMain.on('tab:new', () => { const b = backendOf(); if (b) b.newWindow(); });
  ipcMain.on('tab:select', (_e, { win: w }) => { const b = backendOf(); if (b) b.selectWindow(w); });
  ipcMain.on('tab:close', (_e, { win: w }) => { const b = backendOf(); if (b) b.killWindow(w); });
  ipcMain.on('tab:capture', (_e, { win: w }) => { const b = backendOf(); if (b) b.captureWindow(w); });
  ipcMain.handle('session:rename', () => ({ ok: true }));
  ipcMain.handle('session:kill', () => ({ ok: true }));
  ipcMain.handle('shell:openExternal', () => ({ ok: true }));
  ipcMain.handle('clipboard:write', () => ({ ok: true }));

  const timeout = setTimeout(() => { console.log('FAIL timeout'); app.exit(1); }, 50000);
  await win.loadFile(path.join(__dirname, '../src/renderer/index.html'));

  // helpers to drive the renderer
  const js = (code) => win.webContents.executeJavaScript(code);
  const activeBuf = () => js('window.__testReadBuffer()');
  const type = (s) => js(`window.__testType(${JSON.stringify(s)})`);

  // 1) open the project
  await sleep(1200);
  await js(`document.querySelector('#sessions .session').click()`);
  await sleep(5000);
  check(await js(`window.__testInputReady()`), 'project connected + input ready');

  // 2) run commands in tab A (ls + a unique mark)
  await type('ls\n');
  await sleep(800);
  await type('echo TAB_A_MARK\n');
  await sleep(1500);
  let a = await activeBuf();
  check(/TAB_A_MARK/.test(a), 'tab A shows its own output');

  // 3) open tab B via '+'
  await js(`document.querySelector('#tabs .plus').click()`);
  await sleep(2500);
  const tabCount = await js(`document.querySelectorAll('#tabs .tab:not(.plus)').length`);
  check(tabCount === 2, `two tabs present (got ${tabCount})`);

  // 4) run distinct commands in tab B (now active): ls + a unique mark
  await type('ls\n');
  await sleep(800);
  await type('echo TAB_B_MARK\n');
  await sleep(1500);
  let b = await activeBuf();
  check(/TAB_B_MARK/.test(b), 'tab B shows its own output');
  check(!/TAB_A_MARK/.test(b), 'tab B does NOT show tab A output (isolation)');

  // 5) switch back to tab A (first tab) and verify its content is intact + isolated
  const firstWin = await js(`(function(){ const v=[...views.values()][0]; return [...v.tabs.keys()][0]; })()`);
  await js(`(function(){ const v=[...views.values()][0]; switchTab(v, ${JSON.stringify(firstWin)}); })()`);
  await sleep(1500);
  a = await activeBuf();
  check(/TAB_A_MARK/.test(a), 'switched back to tab A; its output is intact');
  check(!/TAB_B_MARK/.test(a), 'tab A does NOT show tab B output (isolation)');

  // 6) close tab B
  const secondWin = await js(`(function(){ const v=[...views.values()][0]; return [...v.tabs.keys()][1]; })()`);
  await js(`window.terminalAPI.tabClose('g1', ${JSON.stringify(secondWin)})`);
  await sleep(2000);
  const after = await js(`document.querySelectorAll('#tabs .tab:not(.plus)').length`);
  check(after === 1, `tab B closed; one tab remains (got ${after})`);

  ssh(`${TMUX} -L ${SOCK} kill-session -t ${SESSION} 2>/dev/null; true`);
  clearTimeout(timeout);
  console.log(failures === 0 ? '\nMULTITAB PASS' : `\nMULTITAB FAIL (${failures})`);
  app.exit(failures === 0 ? 0 : 1);
});
