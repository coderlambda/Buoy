'use strict';
// Headless launch smoke test (TEST_PLAN TC-G). Boots Electron offscreen, loads the real
// renderer, creates a LOCAL session, and asserts end-to-end data flow main<->renderer<->pty.
// Run via: npm run smoke   (uses xvfb-run or Electron's offscreen if no display).
const { app, BrowserWindow } = require('electron');
const path = require('path');

// Reuse the app's own IPC/session wiring by requiring main? No — main auto-creates a window.
// Instead we drive a minimal harness that mounts the SAME preload + renderer and checks the
// bridge is wired and a local pty streams data to the renderer.
const { Supervisor } = require('../src/main/supervisor');
const { LocalBackend } = require('../src/main/backends/localBackend');
const { ipcMain } = require('electron');

let failures = 0;
function check(cond, msg) { console.log((cond ? 'ok   ' : 'FAIL ') + msg); if (!cond) failures++; }

app.disableHardwareAcceleration();

app.whenReady().then(async () => {
  // Wire a minimal IPC that the renderer expects (subset).
  const sessions = new Map();
  ipcMain.handle('sessions:list', () => []);
  ipcMain.handle('session:create', (_e, meta) => {
    const id = 'smoke-1';
    const sup = new Supervisor({ makeBackend: () => new LocalBackend({ shell: '/bin/sh' }) });
    sup.on('data', (data) => win.webContents.send('session:data', { id, data }));
    sup.on('state', (state) => win.webContents.send('session:state', { id, state }));
    sessions.set(id, sup);
    sup.start({ cols: 80, rows: 24 });
    // drive a marker through the shell
    setTimeout(() => sup.write('echo SMOKE_OK\n'), 300);
    return { id };
  });
  ipcMain.on('session:input', () => {});
  ipcMain.on('session:resize', () => {});
  ipcMain.on('session:ack', () => {});
  ipcMain.on('session:close', () => {});
  ipcMain.on('session:retry', () => {});
  ipcMain.handle('session:rename', (_e, { title }) => ({ ok: true, title: String(title).trim().slice(0, 80) }));

  const win = new BrowserWindow({
    show: false, width: 900, height: 600,
    webPreferences: {
      preload: path.join(__dirname, '../src/preload/preload.js'),
      contextIsolation: true, nodeIntegration: false, sandbox: true,
      offscreen: true,
    },
  });

  const timeout = setTimeout(() => { console.log('FAIL timeout'); app.exit(1); }, 15000);

  await win.loadFile(path.join(__dirname, '../src/renderer/index.html'));
  check(true, 'TC-G1 main booted, window loaded renderer');

  // TC-G1: preload exposed only terminalAPI (no node globals)
  const bridgeOk = await win.webContents.executeJavaScript(
    "typeof window.terminalAPI === 'object' && typeof window.require === 'undefined' && typeof window.process === 'undefined'");
  check(bridgeOk, 'TC-G1 preload exposes terminalAPI only (no node in renderer)');

  // TC-G2: renderer mounted xterm + sidebar
  const uiOk = await win.webContents.executeJavaScript(
    "!!document.querySelector('#sessions') && !!window.Terminal && !!window.FitAddon");
  check(uiOk, 'TC-G2 renderer mounted (sidebar + xterm engine present)');

  // TC-G2 data flow: drive the renderer's OWN create flow (dialog form) so a real sidebar
  // item + mounted view exist — exercising renderer.js mount(), not just the IPC.
  const dataOk = await win.webContents.executeJavaScript(`
    (async () => {
      document.getElementById('f-kind').value = 'local';
      const form = document.getElementById('form');
      // dispatch submit with an "ok" submitter
      const ok = document.getElementById('f-ok');
      form.dispatchEvent(Object.assign(new Event('submit', { cancelable: true }), { submitter: ok }));
      return await new Promise((resolve) => {
        let buf = '';
        window.terminalAPI.onData(({ data }) => { buf += data; if (buf.includes('SMOKE_OK')) resolve(true); });
        setTimeout(() => resolve(false), 8000);
      });
    })()
  `);
  check(dataOk, 'TC-G2 local session data flows via renderer create flow (SMOKE_OK rendered)');

  // TC-G3: clicking a session must NOT blank the terminal. Simulate mount() being called
  // repeatedly (the renderer's click handler) and assert an xterm screen stays in the DOM.
  const clickOk = await win.webContents.executeJavaScript(`
    (async () => {
      // find the renderer's mount by clicking the sidebar item, then clicking again
      const li = document.querySelector('#sessions .session');
      if (!li) return 'no sidebar item';
      li.click();                                   // first click (already active -> no-op)
      await new Promise(r => setTimeout(r, 200));
      li.click();                                   // second click (the crash-repro case)
      await new Promise(r => setTimeout(r, 200));
      const screen = document.querySelector('#term .xterm-screen, #term .xterm');
      const visible = screen && screen.offsetParent !== null;
      return visible ? 'ok' : 'terminal blanked';
    })()
  `);
  check(clickOk === 'ok', 'TC-G3 clicking a session keeps the terminal visible (was: disappeared) [' + clickOk + ']');

  // TC-G4: rename via double-click -> type -> Enter updates the sidebar label.
  const renameOk = await win.webContents.executeJavaScript(`
    (async () => {
      const nameEl = document.querySelector('#sessions .session .name');
      if (!nameEl) return 'no name element';
      nameEl.dispatchEvent(new MouseEvent('dblclick', { bubbles: true }));
      await new Promise(r => setTimeout(r, 100));
      const input = nameEl.querySelector('input');
      if (!input) return 'no rename input appeared';
      input.value = 'Renamed Box';
      input.dispatchEvent(new KeyboardEvent('keydown', { bubbles: true, key: 'Enter' }));
      await new Promise(r => setTimeout(r, 300));
      const label = document.querySelector('#sessions .session .name').textContent.trim();
      return label === 'Renamed Box' ? 'ok' : ('label=' + label);
    })()
  `);
  check(renameOk === 'ok', 'TC-G4 rename updates the sidebar label [' + renameOk + ']');

  clearTimeout(timeout);
  console.log(failures === 0 ? '\nSMOKE PASS' : `\nSMOKE FAIL (${failures})`);
  app.exit(failures === 0 ? 0 : 1);
});
