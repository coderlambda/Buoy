'use strict';
// Reproduces the USER'S EXACT flow with the real GUI + real backend + live host:
//   1) create a native session   2) run `ls`   3) "close the app" (destroy window+supers)
//   4) "reopen the app" (fresh window, restore from persisted list, click to mount)
//   5) assert the previous `ls` command AND its output are on the xterm screen.
// Usage: HOST=user@host TMUX=/path node_modules/.bin/electron test/gui-lifecycle.js
const { app, BrowserWindow, ipcMain } = require('electron');
const { Supervisor } = require('../src/main/supervisor');
const { ControlModeBackend } = require('../src/main/backends/controlModeBackend');
const { execFileSync } = require('child_process');
const path = require('path');

const HOST = process.env.HOST;
const TMUX = process.env.TMUX || '/home/yitong/.local/bin/tmux';
const SOCK = 'dtcc3';
const SESSION = 'gllife';
let failures = 0;
const check = (c, m) => { console.log((c ? 'ok   ' : 'FAIL ') + m); if (!c) failures++; };
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

function ssh(cmd) {
  return execFileSync('ssh', ['-o', 'BatchMode=yes', '--', HOST, cmd],
    { env: { ...process.env, PATH: '/opt/homebrew/bin:' + process.env.PATH }, encoding: 'utf8', timeout: 15000 });
}

// Build one "app instance": a window + its own supervisor map + IPC bound to THIS window.
function makeAppInstance() {
  const supers = new Map();
  const win = new BrowserWindow({ show: false, webPreferences: {
    preload: path.join(__dirname, '../src/preload/preload.js'),
    contextIsolation: true, sandbox: true, offscreen: true } });
  const handler = {
    create() {
      const s = new Supervisor({ makeBackend: () => new ControlModeBackend({ host: HOST, session: SESSION, tmuxPath: TMUX, tmuxVersion: [3, 5] }), opts: { connectTimeoutMs: 3000 } });
      s.on('data', (d) => { if (!win.isDestroyed()) win.webContents.send('session:data', typeof d === 'string' ? { id: 'g1', data: d } : { id: 'g1', pane: d.pane, data: d.data }); });
      s.on('state', (x) => { if (!win.isDestroyed()) win.webContents.send('session:state', { id: 'g1', state: x }); });
      s.on('window', (w) => { if (!win.isDestroyed()) win.webContents.send('session:window', { id: 'g1', ...w }); });
      s.on('ready', () => { if (!win.isDestroyed()) win.webContents.send('session:ready', { id: 'g1' }); });
      supers.set('g1', s);
      s.start({ cols: 90, rows: 30 });
      return { id: 'g1', session: SESSION };
    },
    destroy() { for (const s of supers.values()) s.close(); supers.clear(); if (!win.isDestroyed()) win.destroy(); },
    supers,
  };
  return { win, handler };
}

// IPC is process-global; point it at the "current" instance.
let current = null;
function bindIpcOnce() {
  ipcMain.handle('sessions:list', () => [{ id: 'g1', host: HOST, session: SESSION, transport: 'ssh', mode: 'control', tmuxPath: TMUX, tmuxVersion: [3, 5], title: 'gui', order: 0 }]);
  ipcMain.handle('session:create', () => current.handler.create());
  ipcMain.on('session:input', (_e, { data }) => { console.log('   IPC session:input:', JSON.stringify(data)); const s = current.handler.supers.get('g1'); if (s) s.write(data); else console.log('   (no supervisor for g1!)'); });
  ['session:resize', 'session:ack', 'session:close', 'session:retry'].forEach((c) => ipcMain.on(c, () => {}));
  ipcMain.handle('session:rename', () => ({ ok: true }));
  ipcMain.handle('session:kill', () => ({ ok: true }));
}

app.disableHardwareAcceleration();
app.whenReady().then(async () => {
  if (!HOST) { console.log('FAIL set HOST'); return app.exit(2); }
  ssh(`${TMUX} -L ${SOCK} kill-session -t ${SESSION} 2>/dev/null; true`);   // clean slate
  bindIpcOnce();

  // ---- 1) create a native session (first app instance) ----
  current = makeAppInstance();
  await current.win.loadFile(path.join(__dirname, '../src/renderer/index.html'));
  await current.win.webContents.executeJavaScript(`
    document.getElementById('f-kind').value='remote';
    document.getElementById('f-host').value=${JSON.stringify(HOST)};
    document.getElementById('f-control').checked=true;
    const ok=document.getElementById('f-ok');
    document.getElementById('form').dispatchEvent(Object.assign(new Event('submit',{cancelable:true}),{submitter:ok}));
  `);
  await sleep(6000);   // connect + attach

  // ---- 2) run `ls` ----
  const MARK = 'LSMARK_' + Math.floor(Date.now() / 1000);
  // create a unique file so `ls` output is identifiable, then run ls
  await current.win.webContents.executeJavaScript(`window.__testType('touch ${MARK}.txt && ls ${MARK}.txt\\n')`);
  // poll up to 10s for the marker to render (avoids fixed-wait flakiness on slow links)
  let before = '';
  for (let i = 0; i < 20; i++) {
    await sleep(500);
    before = await current.win.webContents.executeJavaScript('window.__testReadBuffer()');
    if (before.includes(MARK)) break;
  }
  // diagnostics: did input reach the host shell? what's the backend's active window?
  const bp = current.handler.supers.get('g1');
  const reg = bp && bp.backend && bp.backend.reg;
  console.log('   backend activeWindow:', reg && reg.activeWindow, '| ready:', bp && bp.backend && bp.backend._ready);
  const hostPane = ssh(`${TMUX} -L ${SOCK} capture-pane -p -t ${SESSION} 2>/dev/null | tail -4`);
  console.log('   HOST pane says:', JSON.stringify(hostPane.slice(-160)));
  console.log('   xterm buffer tail:', JSON.stringify(before.slice(-160)));
  check(before.includes(MARK), `[step2] ls output visible before close (found ${MARK}: ${before.includes(MARK)})`);

  // ---- 3) close the app ----
  current.handler.destroy();
  await sleep(2500);

  // ---- 4) reopen the app: fresh instance, restore from list, click to mount ----
  current = makeAppInstance();
  await current.win.loadFile(path.join(__dirname, '../src/renderer/index.html'));
  await sleep(1500);   // let init() populate the sidebar from sessions:list
  await current.win.webContents.executeJavaScript(`document.querySelector('#sessions .session').click()`);
  await sleep(7000);   // mount + capture-on-attach back-fill

  // ---- 5) check the previous ls command + result are there ----
  const after = await current.win.webContents.executeJavaScript('window.__testReadBuffer()');
  check(after.includes(MARK), `[step5] previous ls + result present after reopen (found ${MARK}: ${after.includes(MARK)})`);
  if (!after.includes(MARK)) console.log('   after buffer tail:', JSON.stringify(after.slice(-300)));

  ssh(`${TMUX} -L ${SOCK} kill-session -t ${SESSION} 2>/dev/null; rm -f ${MARK}.txt 2>/dev/null; true`);
  console.log(failures === 0 ? '\nLIFECYCLE PASS' : `\nLIFECYCLE FAIL (${failures})`);
  app.exit(failures === 0 ? 0 : 1);
});
