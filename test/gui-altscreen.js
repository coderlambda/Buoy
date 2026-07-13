'use strict';
// Reproduce the reported bug: run a FULL-SCREEN (alternate-screen) app in tab A — like claude
// or vim — then open tab B via '+'. Tab B must be a CLEAN shell, NOT showing tab A's app.
// Usage: HOST=user@host TMUX=/path node_modules/.bin/electron test/gui-altscreen.js
const { app, BrowserWindow, ipcMain } = require('electron');
const { Supervisor } = require('../src/main/supervisor');
const { ControlModeBackend } = require('../src/main/backends/controlModeBackend');
const { execFileSync } = require('child_process');
const path = require('path');

const HOST = process.env.HOST;
const TMUX = process.env.TMUX || '/home/yitong/.local/bin/tmux';
const TVER = [3, 7];
const SOCK = `dtcc${TVER[0]}-${TVER[1]}`;
const SESSION = 'gas';
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
  const js = (code) => win.webContents.executeJavaScript(code);
  const type = (s) => js(`window.__testType(${JSON.stringify(s)})`);
  const bufOfWin = (w) => js(`(function(){const v=[...views.values()][0];const t=v.tabs.get(${JSON.stringify(w)});return t&&t.content.readBuffer?t.content.readBuffer():'';})()`);
  const winIds = () => js('(function(){const v=[...views.values()][0];return [...v.tabs.keys()];})()');
  const activeWin = () => js('(function(){const v=[...views.values()][0];return v.activeWindow;})()');

  // 1) connect
  await sleep(1000);
  await js(`document.querySelector('#sessions .session').click()`);
  await sleep(5000);
  check(await js(`window.__testInputReady()`), 'connected + input ready');

  // 2) launch a FULL-SCREEN alt-screen app in tab A that CONTINUOUSLY redraws a unique marker
  //    (faithfully mimics claude/vim: constant %output %<paneA> flood). \033[?1049h enters the
  //    alternate screen buffer; the loop keeps repainting so output floods while we open tab B.
  await type(`printf '\\033[?1049h'; i=0; while true; do printf '\\033[H\\033[2JFULLSCREEN_CLAUDE_MARK %d' $i; i=$((i+1)); sleep 0.2; done\n`);
  await sleep(2500);
  const firstWin = (await winIds())[0];
  const aBuf = await bufOfWin(firstWin);
  check(/FULLSCREEN_CLAUDE_MARK/.test(aBuf), 'tab A shows the full-screen app');

  // 2b) REATTACH: dispose the view (like closing the app) and reconnect, WHILE claude runs.
  // This is the real-world case: claude keeps running server-side; a fresh client reattaches
  // (enumerate + capture the alt-screen), then the user opens a new tab.
  await js(`window.__testDispose('g1')`);
  await sleep(1000);
  await js(`if(!views.has('g1')) makeView({id:'g1',kind:'remote',mode:'control',host:${JSON.stringify(HOST)},session:${JSON.stringify(SESSION)},tmuxPath:${JSON.stringify(TMUX)},tmuxVersion:[3,7],title:'proj'}); const v=views.get('g1'); v.started=false; mount('g1');`);
  await sleep(6000);
  check(await js(`window.__testInputReady()`), 'reattached while claude running');
  const reWin = (await winIds())[0];
  check(/FULLSCREEN_CLAUDE_MARK/.test(await bufOfWin(reWin)), 'reattached tab A still shows claude');

  // 3) open tab B while the alt-screen app is still running in A (post-reattach)
  await js(`document.querySelector('#tabs .plus').click()`);
  await sleep(3000);
  const wins = await winIds();
  check(wins.length === 2, `two tabs present (got ${wins.length})`);
  const bWin = await activeWin();
  check(bWin !== firstWin, 'new tab B is the active window');

  // 4) THE BUG CHECK: tab B must be a clean shell, NOT showing tab A's full-screen app
  const bBuf = await bufOfWin(bWin);
  check(!/FULLSCREEN_CLAUDE_MARK/.test(bBuf), 'tab B does NOT show tab A full-screen app (the reported bug)');

  // 5) type in tab B; it should run there, and A's marker must stay out of B
  await type('echo IN_TAB_B\n');
  await sleep(1500);
  const bBuf2 = await bufOfWin(bWin);
  check(/IN_TAB_B/.test(bBuf2), 'tab B runs its own command');
  check(!/FULLSCREEN_CLAUDE_MARK/.test(bBuf2), 'tab B still clean of tab A app after typing');

  ssh(`${TMUX} -L ${SOCK} kill-session -t ${SESSION} 2>/dev/null; true`);
  clearTimeout(timeout);
  console.log(failures === 0 ? '\nALTSCREEN PASS' : `\nALTSCREEN FAIL (${failures})`);
  app.exit(failures === 0 ? 0 : 1);
});
