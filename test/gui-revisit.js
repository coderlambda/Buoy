'use strict';
// Reproduce: create tab B, switch back to A, then RE-VISIT B -> B must still be B (not A).
// The reported bug hits the FIRST new tab on the second visit. Tab A runs a full-screen app
// (claude-like) so any capture-correlation desync paints A's content into B.
// Usage: HOST=user@host TMUX=/path node_modules/.bin/electron test/gui-revisit.js
const { app, BrowserWindow, ipcMain } = require('electron');
const { Supervisor } = require('../src/main/supervisor');
const { ControlModeBackend } = require('../src/main/backends/controlModeBackend');
const { execFileSync } = require('child_process');
const path = require('path');

const HOST = process.env.HOST;
const TMUX = process.env.TMUX || '/home/yitong/.local/bin/tmux';
const TVER = [3, 7];
const SOCK = `dtcc${TVER[0]}-${TVER[1]}`;
const SESSION = 'grv';
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

  const timeout = setTimeout(() => { console.log('FAIL timeout'); app.exit(1); }, 60000);
  await win.loadFile(path.join(__dirname, '../src/renderer/index.html'));
  const js = (code) => win.webContents.executeJavaScript(code);
  const type = (s) => js(`window.__testType(${JSON.stringify(s)})`);
  const bufOfWin = (w) => js(`(function(){const v=[...views.values()][0];const t=v.tabs.get(${JSON.stringify(w)});return t&&t.content.readBuffer?t.content.readBuffer():'';})()`);
  const winIds = () => js('(function(){const v=[...views.values()][0];return [...v.tabs.keys()];})()');
  const switchTo = (w) => js(`(function(){const v=[...views.values()][0];switchTab(v,${JSON.stringify(w)});})()`);

  // 1) connect; run a full-screen (alt-screen) marker app in tab A
  await sleep(1000);
  await js(`document.querySelector('#sessions .session').click()`);
  await sleep(5000);
  check(await js(`window.__testInputReady()`), 'connected');
  await type(`printf '\\033[?1049h'; i=0; while true; do printf '\\033[H\\033[2JAAA_FULLSCREEN %d' $i; i=$((i+1)); sleep 0.2; done\n`);
  await sleep(2000);
  const aWin = (await winIds())[0];
  check(/AAA_FULLSCREEN/.test(await bufOfWin(aWin)), 'tab A shows its full-screen app');

  // 2) open tab B (first new tab) and run a distinct marker
  await js(`document.querySelector('#tabs .plus').click()`);
  await sleep(2500);
  const wins = await winIds();
  const bWin = wins[wins.length - 1];
  check(wins.length === 2, `two tabs (got ${wins.length})`);
  await type('echo BBB_ONLY\n');
  await sleep(1500);
  check(/BBB_ONLY/.test(await bufOfWin(bWin)), 'tab B shows its own marker');
  check(!/AAA_FULLSCREEN/.test(await bufOfWin(bWin)), 'tab B clean of A right after creation');

  // 3) switch back to A
  await switchTo(aWin);
  await sleep(1500);
  check(/AAA_FULLSCREEN/.test(await bufOfWin(aWin)), 'tab A intact after switch back');

  // 4) THE BUG: re-visit tab B — it must still be B, not "become the old one" (A)
  await switchTo(bWin);
  await sleep(1500);
  const b2 = await bufOfWin(bWin);
  check(/BBB_ONLY/.test(b2), 're-visited tab B still shows its own marker');
  check(!/AAA_FULLSCREEN/.test(b2), 're-visited tab B is NOT showing tab A (the reported bug)');

  ssh(`${TMUX} -L ${SOCK} kill-session -t ${SESSION} 2>/dev/null; true`);
  clearTimeout(timeout);
  console.log(failures === 0 ? '\nREVISIT PASS' : `\nREVISIT FAIL (${failures})`);
  app.exit(failures === 0 ? 0 : 1);
});
