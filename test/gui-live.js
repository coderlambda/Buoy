'use strict';
// Full-GUI integration test against a LIVE host, exercising the REAL app path:
// persisted session -> sidebar -> CLICK to mount -> capture-on-attach back-fills scrollback.
// Boots real Electron + real renderer.js + real xterm; main-side uses the real Supervisor +
// ControlModeBackend. Reads xterm's ACTUAL buffer to verify what the user sees.
// Usage: HOST=user@host TMUX=/path node_modules/.bin/electron test/gui-live.js
const { app, BrowserWindow, ipcMain } = require('electron');
const { Supervisor } = require('../src/main/supervisor');
const { ControlModeBackend } = require('../src/main/backends/controlModeBackend');
const { execFileSync } = require('child_process');
const path = require('path');

const HOST = process.env.HOST;
const TMUX = process.env.TMUX || '/home/yitong/.local/bin/tmux';
const SOCK = 'dtcc3';
const SESSION = 'guilive';
let failures = 0;
const check = (c, m) => { console.log((c ? 'ok   ' : 'FAIL ') + m); if (!c) failures++; };

// Seed a session on the host with 50 history lines (outside the app), so "open app" has
// something to back-fill — exactly the "reopen and see previous output" scenario.
function ssh(cmd) {
  return execFileSync('ssh', ['-o', 'BatchMode=yes', '--', HOST, cmd],
    { env: { ...process.env, PATH: '/opt/homebrew/bin:' + process.env.PATH }, encoding: 'utf8', timeout: 15000 });
}

app.disableHardwareAcceleration();
app.whenReady().then(async () => {
  if (!HOST) { console.log('FAIL set HOST'); return app.exit(2); }
  ssh(`${TMUX} -L ${SOCK} kill-session -t ${SESSION} 2>/dev/null; ${TMUX} -L ${SOCK} new-session -A -D -d -s ${SESSION} -x 90 -y 30; ${TMUX} -L ${SOCK} send-keys -t ${SESSION} 'for i in $(seq 1 50); do echo GLINE_$i; done' Enter; sleep 1; true`);

  const win = new BrowserWindow({ show: false, webPreferences: {
    preload: path.join(__dirname, '../src/preload/preload.js'),
    contextIsolation: true, sandbox: true, offscreen: true } });

  ipcMain.handle('sessions:list', () => [{ id: 'g1', host: HOST, session: SESSION, transport: 'ssh', mode: 'control', tmuxPath: TMUX, tmuxVersion: [3, 5], title: 'gui', order: 0 }]);
  ipcMain.handle('session:create', () => {
    const s = new Supervisor({ makeBackend: () => new ControlModeBackend({ host: HOST, session: SESSION, tmuxPath: TMUX, tmuxVersion: [3, 5] }), opts: { connectTimeoutMs: 3000 } });
    s.on('data', (d) => win.webContents.send('session:data', typeof d === 'string' ? { id: 'g1', data: d } : { id: 'g1', pane: d.pane, data: d.data }));
    s.on('state', (x) => win.webContents.send('session:state', { id: 'g1', state: x }));
    s.on('window', (w) => win.webContents.send('session:window', { id: 'g1', ...w }));
    s.start({ cols: 90, rows: 30 });
    return { id: 'g1', session: SESSION };
  });
  ['session:input', 'session:resize', 'session:ack', 'session:close', 'session:retry'].forEach((c) => ipcMain.on(c, () => {}));
  ipcMain.handle('session:rename', () => ({ ok: true }));
  ipcMain.handle('session:kill', () => ({ ok: true }));

  const timeout = setTimeout(() => { console.log('FAIL timeout'); app.exit(1); }, 40000);
  await win.loadFile(path.join(__dirname, '../src/renderer/index.html'));
  check(true, 'GUI booted with real renderer');

  // Real path: init() populates the sidebar from sessions:list; click the session to mount.
  await new Promise((r) => setTimeout(r, 1500));
  await win.webContents.executeJavaScript(`document.querySelector('#sessions .session').click()`);
  await new Promise((r) => setTimeout(r, 7000));

  const buf = await win.webContents.executeJavaScript('window.__testReadBuffer()');
  const n = (buf.match(/GLINE_\d+/g) || []).length;
  check(n >= 50, `reopen+click back-fills full scrollback in xterm (got ${n}/50)`);
  if (n < 50) console.log('   buffer tail:', JSON.stringify(buf.slice(-200)));

  ssh(`${TMUX} -L ${SOCK} kill-session -t ${SESSION} 2>/dev/null; true`);
  clearTimeout(timeout);
  console.log(failures === 0 ? '\nGUI-LIVE PASS' : `\nGUI-LIVE FAIL (${failures})`);
  app.exit(failures === 0 ? 0 : 1);
});
