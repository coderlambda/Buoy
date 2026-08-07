'use strict';
// Visual/interaction regression test for the New session form. It loads the real UI in Chromium
// so computed styles catch WebKit/Chromium select defaults that a source-only assertion would miss.
//
// Usage: node_modules/.bin/electron test/gui-new-session.js
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
  if (stripped === html) throw new Error('tauri-api.js script tag not found in ui/index.html');
  const out = path.join(UI, '.gui-new-session-test.html');
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

  await win.loadFile(page);
  await sleep(500);
  await js(`document.getElementById('new').click()`);

  const state = await js(`(() => {
    const select = document.getElementById('f-kind');
    const input = document.getElementById('f-host');
    const wrap = select.closest('.dialog-select');
    const selectStyle = getComputedStyle(select);
    const inputStyle = getComputedStyle(input);
    const arrowStyle = getComputedStyle(wrap, '::after');
    return {
      open: document.getElementById('dialog').open,
      appearance: selectStyle.appearance || selectStyle.webkitAppearance,
      selectHeight: select.getBoundingClientRect().height,
      inputHeight: input.getBoundingClientRect().height,
      backgroundMatches: selectStyle.backgroundColor === inputStyle.backgroundColor,
      borderMatches: selectStyle.borderColor === inputStyle.borderColor,
      radiusMatches: selectStyle.borderRadius === inputStyle.borderRadius,
      fontMatches: selectStyle.fontFamily === inputStyle.fontFamily
        && selectStyle.fontSize === inputStyle.fontSize,
      arrow: arrowStyle.content !== 'none' && arrowStyle.pointerEvents === 'none',
    };
  })()`);

  check(state.open, 'New session dialog opens');
  check(state.appearance === 'none', `Type field removes native select chrome (got ${state.appearance})`);
  check(Math.abs(state.selectHeight - state.inputHeight) < 0.5,
    `Type and Host fields have the same height (${state.selectHeight}px / ${state.inputHeight}px)`);
  check(state.backgroundMatches && state.borderMatches && state.radiusMatches && state.fontMatches,
    'Type field uses the same surface, border, radius, and typography as text inputs');
  check(state.arrow, 'Type field shows a non-interactive theme chevron');

  if (process.env.BUOY_GUI_SCREENSHOT) {
    await sleep(100);
    fs.writeFileSync(process.env.BUOY_GUI_SCREENSHOT, await win.capturePage().then((img) => img.toPNG()));
  }

  const behavior = await js(`(() => {
    const select = document.getElementById('f-kind');
    select.value = 'local';
    select.dispatchEvent(new Event('change', { bubbles: true }));
    return {
      remoteHidden: getComputedStyle(document.getElementById('remote-fields')).display === 'none',
      localShown: getComputedStyle(document.getElementById('local-hint')).display !== 'none',
    };
  })()`);
  check(behavior.remoteHidden && behavior.localShown,
    'styled native select still switches the form to a local session');

  const errors = await js('window.__errs || []');
  check(errors.length === 0, `no renderer errors (got ${JSON.stringify(errors)})`);

  cleanup();
  console.log(failures ? `\n${failures} check(s) FAILED` : '\nall ok');
  app.exit(failures ? 1 : 0);
});
