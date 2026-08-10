
// Real-xterm regression for reconnect backfill in the native Tauri webview: captured history and
// screen rows are repainted, the tmux cursor is restored, then shell echo stays on the prompt row.
import {
  createChecks, fire, js, loadFixture, session,
} from './tauri-ui-harness.js';

describe('Tauri UI: terminal reconnect repaint', () => {
  before(async () => {
    await browser.setWindowSize(1000, 700);
    await loadFixture([
      session(1, 'native project'),
      session(2, 'plain project', 'plain'),
    ]);
  });

  it('restores the captured cursor before new output arrives', async () => {
    const { check, finish } = createChecks();
    await fire('window', { id: 's1', action: 'add', window: '@0', order: ['@0'] });
    await fire('window', { id: 's1', action: 'active', window: '@0', order: ['@0'] });
    // Wait through the native webview's next paint. The fit/capture work is intentionally queued
    // with requestAnimationFrame when a tab becomes visible.
    await browser.execute(async () => new Promise((resolve) => {
      requestAnimationFrame(() => resolve(true));
    }));
    await fire('state', { id: 's1', state: 'connected' });
    await fire('ready', { id: 's1' });
    await browser.pause(100);

    const calls = await js('window.__terminalCalls || []');
    const resizeAt = calls.findIndex((call: unknown[]) => call[0] === 'resize');
    const captureAt = calls.findIndex((call: unknown[]) => call[0] === 'capture');
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
    await browser.pause(100);

    let state = await js('window.__testTerminalState()');
    check(state.cursorY === cursorY && state.cursorX === prompt.length,
      `TC-CR2 restored cursor to tmux coordinates (got ${JSON.stringify(state)})`);
    check(state.line === prompt,
      `TC-CR2 prompt occupies the cursor row (got ${JSON.stringify(state)})`);

    await fire('data', { id: 's1', window: '@0', data: 'echo MARK' });
    await browser.pause(100);
    state = await js('window.__testTerminalState()');
    check(state.line === '$ echo MARK' && !state.next.includes('echo MARK'),
      `TC-CR3 command echo remains beside prompt, not on following row (got ${JSON.stringify(state)})`);
    finish();
  });
});
