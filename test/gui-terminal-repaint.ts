
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
    const backend = await js(`({ kind: window.__testRendererKind(), repaints: window.__testRepaintCount() })`);
    check(backend.kind === 'canvas', `TC-R1 mounted pane uses the Canvas renderer (got ${JSON.stringify(backend)})`);
    check(backend.repaints >= 2,
      `TC-R3 Canvas attach and first reveal both used the full repaint primitive (got ${JSON.stringify(backend)})`);
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
    check((await js('window.__testReadBuffer()')).includes('history 1'),
      'TC-R3 captured content is present after Canvas attach and repaint');
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

  it('repaints same-size reveals and foreground recovery events without repainting on output', async () => {
    const { check, finish } = createChecks();
    await fire('window', { id: 's1', action: 'add', window: '@1', order: ['@0', '@1'] });
    await fire('window', { id: 's1', action: 'active', window: '@1', order: ['@0', '@1'] });
    await browser.pause(50);

    // @0 stays alive but has display:none. Write while hidden, then reveal it at the same webview
    // dimensions: FitAddon has no resize-driven dirty rows to hide a missing explicit refresh.
    await fire('data', { id: 's1', window: '@0', data: '\r\nBACKGROUND_MARK\r\n' });
    await browser.pause(50);
    const beforeReveal = await js(`({
      count: window.__testRepaintCount(),
      state: window.__testTerminalState(),
    })`);
    await fire('window', { id: 's1', action: 'active', window: '@0', order: ['@0', '@1'] });
    await browser.pause(50);
    const afterReveal = await js(`({
      buffer: window.__testReadBuffer(),
      count: window.__testRepaintCount(),
      state: window.__testTerminalState(),
    })`);
    check(afterReveal.buffer.includes('BACKGROUND_MARK'),
      'TC-P1 output written while the tab was hidden is present immediately after reveal');
    check(afterReveal.state.cols === beforeReveal.state.cols && afterReveal.state.rows === beforeReveal.state.rows,
      `TC-P2 reveal kept identical layout dimensions (got ${JSON.stringify({ beforeReveal, afterReveal })})`);
    check(afterReveal.count === beforeReveal.count + 1,
      `TC-P2 same-size reveal forced exactly one full repaint (got ${beforeReveal.count} -> ${afterReveal.count})`);

    // Run both recovery events synchronously inside the page so WebDriver focus bookkeeping cannot
    // add noise. Two live tabs exist, so +1 per event also proves hidden panes were not swept.
    const foreground = await js(`(() => {
      const before = window.__testRepaintCount();
      const ownDescriptor = Object.getOwnPropertyDescriptor(document, 'visibilityState');
      Object.defineProperty(document, 'visibilityState', { configurable: true, value: 'visible' });
      document.dispatchEvent(new Event('visibilitychange'));
      const afterVisibility = window.__testRepaintCount();
      if (ownDescriptor) Object.defineProperty(document, 'visibilityState', ownDescriptor);
      else delete document.visibilityState;
      window.dispatchEvent(new Event('focus'));
      return { before, afterVisibility, afterFocus: window.__testRepaintCount() };
    })()`);
    check(foreground.afterVisibility === foreground.before + 1
      && foreground.afterFocus === foreground.afterVisibility + 1,
    `TC-P3 visibility and focus repaint only the active pane (got ${JSON.stringify(foreground)})`);

    const beforeOutput = await js('window.__testRepaintCount()');
    await fire('data', { id: 's1', window: '@0', data: 'ordinary output' });
    await browser.pause(50);
    const afterOutput = await js('window.__testRepaintCount()');
    check(afterOutput === beforeOutput,
      `TC-P5 ordinary output does not trigger a recovery repaint (got ${beforeOutput} -> ${afterOutput})`);

    // A constructor/load failure must preserve correctness through xterm's built-in DOM renderer.
    // Replace the whole UMD namespace (its class export is getter-backed), then restore it after the
    // new terminal has attempted its one-time renderer attach.
    await js(`(() => {
      window.__savedCanvasAddon = window.CanvasAddon;
      window.CanvasAddon = { CanvasAddon: class { constructor() { throw new Error('forced Canvas failure'); } } };
    })()`);
    await fire('window', { id: 's1', action: 'add', window: '@2', order: ['@0', '@1', '@2'] });
    await fire('window', { id: 's1', action: 'active', window: '@2', order: ['@0', '@1', '@2'] });
    await browser.pause(50);
    const fallback = await js(`({ kind: window.__testRendererKind(), state: window.__testTerminalState() })`);
    await js(`(() => { window.CanvasAddon = window.__savedCanvasAddon; delete window.__savedCanvasAddon; })()`);
    await fire('data', { id: 's1', window: '@2', data: 'DOM_FALLBACK_MARK' });
    await browser.pause(50);
    fallback.buffer = await js('window.__testReadBuffer()');
    check(fallback.kind === 'dom' && fallback.state.rows > 5 && fallback.buffer.includes('DOM_FALLBACK_MARK'),
      `TC-R2 Canvas failure keeps a working DOM-rendered pane (got ${JSON.stringify(fallback)})`);
    finish();
  });
});
