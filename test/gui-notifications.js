'use strict';
// Full renderer regression test for OSC/BEL notification dots. It exercises real ui/index.html
// and ui/renderer.js in Chromium while driving the same backend events Tauri delivers.
//
// Usage: node_modules/.bin/electron test/gui-notifications.js
const { app, BrowserWindow } = require('electron');
const fs = require('fs');
const path = require('path');

let failures = 0;
const check = (condition, message) => {
  console.log((condition ? 'ok   ' : 'FAIL ') + message);
  if (!condition) failures++;
};

const UI = path.join(__dirname, '..', 'ui');
const SCREENSHOT_DIR = process.env.BUOY_GUI_SCREENSHOT_DIR || '';

function writeTestPage() {
  const html = fs.readFileSync(path.join(UI, 'index.html'), 'utf8');
  const stripped = html.replace(/\s*<script src="tauri-api\.js"><\/script>/, '');
  if (stripped === html) throw new Error('tauri-api.js script tag not found in ui/index.html');
  const out = path.join(UI, '.gui-notifications-test.html');
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
  const snapshot = async (name) => {
    if (!SCREENSHOT_DIR) return;
    fs.mkdirSync(SCREENSHOT_DIR, { recursive: true });
    fs.writeFileSync(path.join(SCREENSHOT_DIR, name), (await wc.capturePage()).toPNG());
  };

  function dotState() {
    return js(`(() => ({
      sessionS1: document.querySelectorAll('#sessions .session[data-id="s1"] .notification-dot').length,
      sessionS2: document.querySelectorAll('#sessions .session[data-id="s2"] .notification-dot').length,
      sessionDotInStatusColumn: (() => {
        const row = document.querySelector('#sessions .session[data-id="s1"]');
        const stack = row && row.querySelector('.status-dots');
        const notice = row && row.querySelector('.notification-dot');
        const connection = stack && stack.querySelector('.dot');
        if (!(stack && notice && connection && notice.parentElement === stack
          && stack.firstElementChild === connection)) return false;
        const noticeBox = notice.getBoundingClientRect();
        const connectionBox = connection.getBoundingClientRect();
        const noticeCenter = noticeBox.left + noticeBox.width / 2;
        const connectionCenter = connectionBox.left + connectionBox.width / 2;
        return Math.abs(noticeCenter - connectionCenter) <= 0.5
          && noticeBox.top >= connectionBox.bottom + 4;
      })(),
      tabDots: Array.from(document.querySelectorAll('#tabs .tab:not(.plus)')).filter(
        (tab) => tab.querySelector('.notification-dot')).map(
        (tab) => (tab.querySelector('.ttext') || {}).textContent)
    }))()`);
  }

  const clickTab = (title) => js(`(() => {
    const tab = Array.from(document.querySelectorAll('#tabs .tab:not(.plus)')).find(
      (node) => (node.querySelector('.ttext') || {}).textContent === ${JSON.stringify(title)});
    if (!tab) return false;
    tab.querySelector('.tlabel').click();
    return true;
  })()`);

  const clickVisibleTerminal = () => js(`(() => {
    const terminal = Array.from(document.querySelectorAll('#term .xterm')).find((node) => {
      const rect = node.getBoundingClientRect();
      return rect.width > 0 && rect.height > 0;
    });
    if (!terminal) return false;
    terminal.dispatchEvent(new PointerEvent('pointerdown', { bubbles: true, button: 0 }));
    return true;
  })()`);

  const keyInVisibleTerminal = () => js(`(() => {
    const terminal = Array.from(document.querySelectorAll('#term .xterm')).find((node) => {
      const rect = node.getBoundingClientRect();
      return rect.width > 0 && rect.height > 0;
    });
    const target = terminal && (terminal.querySelector('textarea') || terminal);
    if (!target) return false;
    target.dispatchEvent(new KeyboardEvent('keydown', {
      bubbles: true, cancelable: true, key: 'x', code: 'KeyX'
    }));
    return true;
  })()`);

  await win.loadFile(page);
  await sleep(800);

  check((await js('window.__errs || []')).length === 0, 'no uncaught renderer errors');

  // Build the native tmux tab strip exactly as control-mode backend events do.
  await fire('window', { id: 's1', action: 'add', window: '@0', order: ['@0'] });
  await fire('window', { id: 's1', action: 'rename', window: '@0', name: 'shell' });
  await fire('window', { id: 's1', action: 'add', window: '@1', order: ['@0', '@1'] });
  await fire('window', { id: 's1', action: 'rename', window: '@1', name: 'agent' });
  await fire('window', { id: 's1', action: 'active', window: '@0', order: ['@0', '@1'] });
  await sleep(100);

  let state = await dotState();
  check(state.sessionS1 === 0 && state.tabDots.length === 0,
    'TC-N1 starts with no session or tab notification dots');

  // xterm answers OSC colour queries through onData, the same path as keyboard input. The reply
  // must carry the xterm's own window id: while switching tabs, the backend's active-window event
  // can still name the previous tab, which used to inject `10;rgb:...` into that other program.
  await js('window.__inputs = []');
  await fire('data', { id: 's1', window: '@0', data: '\u001b]10;?\u0007' });
  await sleep(100);
  const colourReplies = await js('window.__inputs || []');
  check(colourReplies.some((args) => args[0] === 's1' && args[2] === '@0'
      && String(args[1]).includes(']10;rgb:')),
    `TC-N1b OSC colour reply is addressed to its source tab (got ${JSON.stringify(colourReplies)})`);

  // A notification from the terminal the user is already viewing is not unread work.
  await fire('data', { id: 's1', window: '@0', data: '\u001b]9;Visible work finished\u0007' });
  state = await dotState();
  check(state.sessionS1 === 0 && state.tabDots.length === 0,
    'TC-N1c a notification from the visible active tab is ignored');

  // Split OSC 777 across chunks: no dot before the terminator, then one on the emitting tab and
  // one rollup on its session.
  await fire('data', { id: 's1', window: '@1', data: '\u001b]777;notify;Agent;Needs input' });
  state = await dotState();
  check(state.sessionS1 === 0 && state.tabDots.length === 0,
    'TC-N2 an incomplete/split OSC does not notify early');
  await fire('data', { id: 's1', window: '@1', data: '\u0007' });
  state = await dotState();
  check(state.sessionS1 === 1 && JSON.stringify(state.tabDots) === JSON.stringify(['agent']),
    `TC-N2 completed OSC marks only agent + its session (got ${JSON.stringify(state)})`);
  check(state.sessionDotInStatusColumn,
    'TC-N2 session notification dot sits below the connection status dot');
  await snapshot('osc-notifications-unread.png');

  // A backend active-window report changes what is displayed but is not a user acknowledgement.
  // After it reveals the unread agent tab, a notification from the now-background shell gives us
  // two unread children but still one session dot.
  await fire('state', { id: 's1', state: 'connected' });
  await fire('window', { id: 's1', action: 'active', window: '@1', order: ['@0', '@1'] });
  state = await dotState();
  check(state.sessionS1 === 1 && JSON.stringify(state.tabDots) === JSON.stringify(['agent']),
    'TC-N3 backend-driven activation does not acknowledge the unread tab');
  await fire('data', { id: 's1', window: '@0', data: '\u001b]9;Shell done\u001b\\' });
  state = await dotState();
  check(state.sessionS1 === 1 && state.tabDots.length === 2,
    `TC-N3 two unread tabs roll up to one persistent session dot (got ${JSON.stringify(state)})`);

  check(await clickTab('agent'), 'TC-N4 could click the notified agent tab');
  state = await dotState();
  check(state.sessionS1 === 1 && JSON.stringify(state.tabDots) === JSON.stringify(['shell']),
    `TC-N4 clicking agent clears only agent; shell keeps the session unread (got ${JSON.stringify(state)})`);
  await snapshot('osc-notifications-partial-clear.png');

  check(await clickTab('shell'), 'TC-N5 could click the remaining notified shell tab');
  state = await dotState();
  check(state.sessionS1 === 0 && state.tabDots.length === 0,
    'TC-N5 clearing the last unread tab clears the session rollup');
  await snapshot('osc-notifications-cleared.png');

  // New notifications from the active shell remain ignored, including OSC 99.
  await fire('data', { id: 's1', window: '@0', data: '\u001b]99;;Visible work finished\u001b\\' });
  state = await dotState();
  check(state.sessionS1 === 0 && state.tabDots.length === 0,
    'TC-N6 a new notification from the active tab stays acknowledged');

  // Make shell unread in the background, then let tmux report it active. That automatic report
  // preserves the dot; clicking the already-visible tab header is the explicit acknowledgement.
  await fire('window', { id: 's1', action: 'active', window: '@1', order: ['@0', '@1'] });
  await fire('data', { id: 's1', window: '@0', data: '\u001b]99;;New work finished\u001b\\' });
  await fire('window', { id: 's1', action: 'active', window: '@0', order: ['@0', '@1'] });
  state = await dotState();
  check(state.sessionS1 === 1 && JSON.stringify(state.tabDots) === JSON.stringify(['shell']),
    'TC-N6b backend-driven activation preserves an existing shell dot');
  check(await clickTab('shell'), 'TC-N6b could click the already-active shell tab');
  state = await dotState();
  check(state.sessionS1 === 0 && state.tabDots.length === 0,
    'TC-N6b clicking the already-active notified tab clears it');

  // Clicking in the terminal is also an acknowledgement, even when the click emits no pty bytes.
  await fire('window', { id: 's1', action: 'active', window: '@1', order: ['@0', '@1'] });
  await fire('data', { id: 's1', window: '@0', data: '\u001b]777;notify;Shell;Click me\u0007' });
  await fire('window', { id: 's1', action: 'active', window: '@0', order: ['@0', '@1'] });
  check(await clickVisibleTerminal(), 'TC-N6c could click the visible terminal');
  state = await dotState();
  check(state.sessionS1 === 0 && state.tabDots.length === 0,
    'TC-N6c clicking inside the terminal clears its existing dot');

  // Keyboard/paste input uses the same acknowledgement path.
  await fire('window', { id: 's1', action: 'active', window: '@1', order: ['@0', '@1'] });
  await fire('data', { id: 's1', window: '@0', data: '\u001b]9;Type to acknowledge\u0007' });
  await fire('window', { id: 's1', action: 'active', window: '@0', order: ['@0', '@1'] });
  await fire('data', { id: 's1', window: '@0', data: '\u001b]10;?\u0007' });
  await sleep(100);
  state = await dotState();
  check(state.sessionS1 === 1 && JSON.stringify(state.tabDots) === JSON.stringify(['shell']),
    'TC-N6d an active xterm protocol reply is not mistaken for user input');
  check(await keyInVisibleTerminal(), 'TC-N6d could type in the visible terminal');
  state = await dotState();
  check(state.sessionS1 === 0 && state.tabDots.length === 0,
    'TC-N6d input in the visible terminal clears its existing dot');

  // The same rule applies to a mounted background xterm.
  await fire('data', { id: 's1', window: '@1', data: '\u001b]9;Background work\u0007' });
  await fire('data', { id: 's1', window: '@1', data: '\u001b]10;?\u0007' });
  await sleep(100);
  state = await dotState();
  check(state.sessionS1 === 1 && JSON.stringify(state.tabDots) === JSON.stringify(['agent']),
    'TC-N6e a background xterm protocol reply does not acknowledge its dot');
  await clickTab('agent');
  await clickTab('shell');

  // Kitty multipart/control semantics: d=0 waits, final body notifies, p=close does not.
  await fire('data', { id: 's1', window: '@1', data: '\u001b]99;i=x:d=0;p=title;Build\u001b\\' });
  state = await dotState();
  check(state.sessionS1 === 0, 'TC-N7 unfinished Kitty title does not notify');
  await fire('data', { id: 's1', window: '@1', data: '\u001b]99;i=x;p=body;Done\u001b\\' });
  state = await dotState();
  check(state.sessionS1 === 1 && JSON.stringify(state.tabDots) === JSON.stringify(['agent']),
    'TC-N7 final Kitty body marks the emitting tab');
  await clickTab('agent');
  await fire('data', { id: 's1', window: '@1', data: '\u001b]99;i=x:p=close;\u001b\\' });
  state = await dotState();
  check(state.sessionS1 === 0 && state.tabDots.length === 0,
    'TC-N7 Kitty close/control traffic does not create a new unread dot');

  // A plain session has no inner header. Its session card is the only viewing gesture, so clicking
  // that card acknowledges its sole implicit tab.
  await fire('data', { id: 's2', window: null, data: '\u001b]777;notify;Plain;Done\u0007' });
  state = await dotState();
  check(state.sessionS2 === 1, 'TC-N8 a plain/single-tab session receives a session dot');
  await js(`document.querySelector('#sessions .session[data-id="s2"]').click()`);
  await sleep(100);
  state = await dotState();
  check(state.sessionS2 === 0, 'TC-N8 clicking the plain session acknowledges its implicit tab');

  // Codex defaults to notification_method=auto. Buoy is not on Codex's OSC-9 terminal allowlist,
  // so its zero-config fallback is a standalone BEL; xterm reports that through onBell rather than
  // leaving the byte in the raw output parser.
  await fire('data', { id: 's1', window: '@1', data: '\u0007' });
  await sleep(100);
  state = await dotState();
  check(state.sessionS1 === 1, 'TC-N9 a standalone BEL marks its emitting session without config');
  await js(`document.querySelector('#sessions .session[data-id="s1"]').click()`);
  await sleep(100);
  check(await clickTab('agent'), 'TC-N9 could acknowledge the BEL-marked tab');
  state = await dotState();
  check(state.sessionS1 === 0, 'TC-N9 acknowledging the BEL-marked tab clears its session dot');

  const errors = await js('window.__errs || []');
  check(errors.length === 0, `no renderer errors after notification interactions (got ${JSON.stringify(errors)})`);

  cleanup();
  console.log(failures ? `\n${failures} check(s) FAILED` : '\nall ok');
  app.exit(failures ? 1 : 0);
});
