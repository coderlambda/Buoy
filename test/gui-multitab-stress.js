'use strict';
// Stress verification of projects/multi-tab (§14) under FAST interactive use + REATTACH —
// the conditions the user reported broken (mixed output, new tab reusing another's screen,
// un-closable tabs). Uses short sleeps and reattach to a session that already has windows.
// Usage: HOST=user@host TMUX=/path node_modules/.bin/electron test/gui-multitab-stress.js
const { app, BrowserWindow, ipcMain } = require('electron');
const { Supervisor } = require('../src/main/supervisor');
const { ControlModeBackend } = require('../src/main/backends/controlModeBackend');
const { execFileSync } = require('child_process');
const path = require('path');

const HOST = process.env.HOST;
const TMUX = process.env.TMUX || '/home/yitong/.local/bin/tmux';
const TVER = [3, 7];               // host tmux is 3.7b
const SOCK = `dtcc${TVER[0]}-${TVER[1]}`;  // must match ControlModeBackend's derived socket
const SESSION = 'gmts';
let failures = 0;
const check = (c, m) => { console.log((c ? 'ok   ' : 'FAIL ') + m); if (!c) failures++; };
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const ssh = (cmd) => execFileSync('ssh', ['-o', 'BatchMode=yes', '--', HOST, cmd],
  { env: { ...process.env, PATH: '/opt/homebrew/bin:' + process.env.PATH }, encoding: 'utf8', timeout: 15000 });

app.disableHardwareAcceleration();
app.whenReady().then(async () => {
  if (!HOST) { console.log('FAIL set HOST'); return app.exit(2); }
  // Pre-create a session with TWO windows, each with a UNIQUE mark, so this run REATTACHES to
  // an existing multi-window session (the case tmux sends no window events for).
  ssh(`${TMUX} -L ${SOCK} kill-session -t ${SESSION} 2>/dev/null; true`);
  ssh(`${TMUX} -L ${SOCK} new-session -d -s ${SESSION}; ${TMUX} -L ${SOCK} send-keys -t ${SESSION} 'echo PRE_W1' Enter`);
  ssh(`${TMUX} -L ${SOCK} new-window -t ${SESSION}; ${TMUX} -L ${SOCK} send-keys -t ${SESSION} 'echo PRE_W2' Enter`);
  await sleep(600);

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
  const activeBuf = () => js('window.__testReadBuffer()');
  const type = (s) => js(`window.__testType(${JSON.stringify(s)})`);
  // buffer of a specific window's tab (not just the active one)
  const bufOfWin = (w) => js(`(function(){const v=[...views.values()][0];const t=v.tabs.get(${JSON.stringify(w)});return t&&t.content.readBuffer?t.content.readBuffer():'';})()`);
  const winIds = () => js('(function(){const v=[...views.values()][0];return [...v.tabs.keys()];})()');
  const activeWin = () => js('(function(){const v=[...views.values()][0];return v.activeWindow;})()');

  // 1) REATTACH: open the project (existing 2-window session)
  await sleep(1000);
  await js(`document.querySelector('#sessions .session').click()`);
  await sleep(5000);
  check(await js(`window.__testInputReady()`), 'reattach connected + input ready');

  // 2) both pre-existing windows became tabs
  let wins = await winIds();
  check(wins.length === 2, `reattach shows 2 pre-existing tabs (got ${wins.length})`);

  // 3) reattach isolation: switch to EACH pre-existing tab (triggers lazy scrollback capture,
  // the §14 "others on focus" design) and verify it shows ONLY its own pre-mark.
  const marks = [];
  for (const w of wins) {
    await js(`(function(){const v=[...views.values()][0];switchTab(v,${JSON.stringify(w)});})()`);
    await sleep(1500);
    marks.push(await bufOfWin(w));
  }
  const hasW1 = marks.map((b) => /PRE_W1/.test(b));
  const hasW2 = marks.map((b) => /PRE_W2/.test(b));
  // exactly one tab has W1 (and not W2), exactly one has W2 (and not W1)
  const cleanW1 = marks.filter((b) => /PRE_W1/.test(b) && !/PRE_W2/.test(b)).length === 1;
  const cleanW2 = marks.filter((b) => /PRE_W2/.test(b) && !/PRE_W1/.test(b)).length === 1;
  check(cleanW1 && cleanW2, `reattached tabs isolated (W1 in ${hasW1.filter(Boolean).length}, W2 in ${hasW2.filter(Boolean).length})`);

  // 4) FAST tab creation: open 3 new tabs in quick succession (short sleeps expose races)
  for (let i = 0; i < 3; i++) { await js(`document.querySelector('#tabs .plus').click()`); await sleep(500); }
  await sleep(2500);
  wins = await winIds();
  check(wins.length === 5, `rapid-open yields 5 tabs (got ${wins.length})`);

  // 5) type a UNIQUE mark in the (new) active tab, verify it lands there and NOWHERE else
  const act = await activeWin();
  await type('echo NEWTAB_MARK\n');
  await sleep(1500);
  const actBuf = await bufOfWin(act);
  check(/NEWTAB_MARK/.test(actBuf), 'new tab shows its own typed output');
  // no OTHER tab should contain NEWTAB_MARK (reused-screen bug)
  let leaked = 0;
  for (const w of wins) { if (w === act) continue; if (/NEWTAB_MARK/.test(await bufOfWin(w))) leaked++; }
  check(leaked === 0, `no other tab reused the new tab's output (leaked into ${leaked})`);

  // 6) rapid switching between all tabs, then verify active buffer matches the switched tab
  for (const w of wins) { await js(`(function(){const v=[...views.values()][0];switchTab(v,${JSON.stringify(w)});})()`); await sleep(250); }
  await sleep(800);
  const finalActive = await activeWin();
  check(finalActive === wins[wins.length - 1], 'rapid switch ends on the last-selected tab');

  // 7) close EVERY closable tab except one; each close must actually remove it (closability bug)
  let remaining = await winIds();
  while (remaining.length > 1) {
    const target = remaining[remaining.length - 1];
    await js(`window.terminalAPI.tabClose('g1', ${JSON.stringify(target)})`);
    await sleep(1200);
    const now = await winIds();
    if (now.length !== remaining.length - 1) { check(false, `tab ${target} did not close (still ${now.length})`); break; }
    remaining = now;
  }
  check(remaining.length === 1, `all tabs closable down to 1 (got ${remaining.length})`);

  ssh(`${TMUX} -L ${SOCK} kill-session -t ${SESSION} 2>/dev/null; true`);
  clearTimeout(timeout);
  console.log(failures === 0 ? '\nSTRESS PASS' : `\nSTRESS FAIL (${failures})`);
  app.exit(failures === 0 ? 0 : 1);
});
