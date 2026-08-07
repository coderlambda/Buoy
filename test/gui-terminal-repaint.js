'use strict';
// Real-xterm regression for reconnect backfill: captured history/screen rows are repainted, the
// tmux cursor is restored above a footer, then shell echo must remain on that prompt row.
const { app, BrowserWindow } = require('electron');
const fs = require('fs');
const path = require('path');

let failures = 0;
const check = (condition, message) => {
  console.log((condition ? 'ok   ' : 'FAIL ') + message);
  if (!condition) failures++;
};
const UI = path.join(__dirname, '..', 'ui');

function writeTestPage() {
  const html = fs.readFileSync(path.join(UI, 'index.html'), 'utf8');
  const stripped = html.replace(/\s*<script src="tauri-api\.js"><\/script>/, '');
  const out = path.join(UI, '.gui-terminal-repaint-test.html');
  fs.writeFileSync(out, stripped);
  return out;
}

app.disableHardwareAcceleration();
app.whenReady().then(async () => {
  const page = writeTestPage();
  const cleanup = () => { try { fs.unlinkSync(page); } catch (_) {} };
  const win = new BrowserWindow({
    width: 1000, height: 700, show: false,
    webPreferences: { offscreen: true, contextIsolation: false,
      preload: path.join(__dirname, 'gui-notifications-preload.js') },
  });
  const wc = win.webContents;
  const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
  const js = (code) => wc.executeJavaScript(code);
  const fire = (event, payload) => js(
    `window.__fire(${JSON.stringify(event)}, ${JSON.stringify(payload)})`);

  await win.loadFile(page);
  await sleep(700);
  await fire('window', { id: 's1', action: 'add', window: '@0', order: ['@0'] });
  await fire('window', { id: 's1', action: 'active', window: '@0', order: ['@0'] });
  await fire('state', { id: 's1', state: 'connected' });
  await fire('ready', { id: 's1' });
  await sleep(100);

  const calls = await js('window.__terminalCalls || []');
  const resizeAt = calls.findIndex((call) => call[0] === 'resize');
  const captureAt = calls.findIndex((call) => call[0] === 'capture');
  check(resizeAt >= 0 && captureAt > resizeAt,
    `TC-CR1 resize is queued before reconnect capture (got ${JSON.stringify(calls)})`);
  const initial = await js('window.__testTerminalState()');
  check(!!initial && initial.rows > 5, 'TC-CR1 mounted and fitted a measurable real xterm');
  const cursorY = initial.rows - 3;
  const prompt = '$ ';
  const screen = Array.from({ length: initial.rows }, () => '');
  screen[cursorY] = prompt;
  screen[initial.rows - 1] = 'status footer';
  const history = Array.from({ length: 12 }, (_, i) => `history ${i + 1}`);
  const repaint = '\x1b[H\x1b[2J' + history.concat(screen).join('\r\n')
    + `\x1b[${cursorY + 1};${prompt.length + 1}H`;
  await fire('data', { id: 's1', window: '@0', data: repaint });
  await sleep(100);

  let state = await js('window.__testTerminalState()');
  check(state.cursorY === cursorY && state.cursorX === prompt.length,
    `TC-CR2 restored cursor to tmux coordinates (got ${JSON.stringify(state)})`);
  check(state.line === prompt,
    `TC-CR2 prompt occupies the cursor row (got ${JSON.stringify(state)})`);

  await fire('data', { id: 's1', window: '@0', data: 'echo MARK' });
  await sleep(100);
  state = await js('window.__testTerminalState()');
  check(state.line === '$ echo MARK' && !state.next.includes('echo MARK'),
    `TC-CR3 command echo remains beside prompt, not on following row (got ${JSON.stringify(state)})`);

  cleanup();
  console.log(failures ? `\n${failures} check(s) FAILED` : '\nall ok');
  app.exit(failures ? 1 : 0);
});
