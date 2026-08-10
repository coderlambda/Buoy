// Opt-in Canvas-vs-DOM measurement for RENDERER_BACKEND_DESIGN.md §6.6. Each invocation measures
// one backend in a fresh Tauri process; `npm run measure:renderer` runs Canvas and DOM separately so
// allocator/cache retention from the first backend cannot bias the second backend's RSS delta.
import { execFileSync } from 'node:child_process';
import { fire, js, loadFixture, session } from './tauri-ui-harness.js';

type BackendKind = 'canvas' | 'dom';

function appRssKb(): number | null {
  if (process.platform !== 'darwin') return null;
  try {
    const pid = execFileSync('/usr/sbin/lsof', [
      '-nP', '-iTCP:4445', '-sTCP:LISTEN', '-t',
    ], { encoding: 'utf8' }).trim().split('\n')[0];
    if (!pid) return null;
    const value = execFileSync('/bin/ps', ['-o', 'rss=', '-p', pid], { encoding: 'utf8' }).trim();
    const rss = Number(value);
    return Number.isFinite(rss) ? rss : null;
  } catch (_) {
    return null;
  }
}

describe('Tauri UI: renderer backend measurement', () => {
  it('measures the requested backend with 16 live terminals', async function () {
    this.timeout(300_000);
    const requested = process.env.BUOY_RENDERER_BENCHMARK_BACKEND === 'dom' ? 'dom' : 'canvas';
    await browser.setWindowSize(1000, 700);
    await loadFixture([session(1, `${requested} benchmark`)], {}, { echoInput: true });

    if (requested === 'dom') {
      await js(`(() => {
        window.__savedCanvasAddon = window.CanvasAddon;
        window.CanvasAddon = { CanvasAddon: class { constructor() { throw new Error('benchmark DOM fallback'); } } };
      })()`);
    }

    const rssBeforeKb = appRssKb();
    const order: string[] = [];
    for (let index = 0; index < 16; index++) {
      const windowId = `@${index}`;
      order.push(windowId);
      await fire('window', { id: 's1', action: 'add', window: windowId, order: order.slice() });
      await fire('window', { id: 's1', action: 'active', window: windowId, order: order.slice() });
    }
    await fire('state', { id: 's1', state: 'connected' });
    await fire('ready', { id: 's1' });
    await fire('window', { id: 's1', action: 'active', window: '@15', order });
    await browser.pause(200);

    if (requested === 'dom') {
      await js(`(() => { window.CanvasAddon = window.__savedCanvasAddon; delete window.__savedCanvasAddon; })()`);
    }

    const actual = await js('window.__testRendererKind()') as BackendKind | null;
    if (actual !== requested) throw new Error(`requested ${requested} renderer, got ${String(actual)}`);

    const rssAfterKb = appRssKb();
    console.log(`RENDERER_MEASUREMENT_PHASE ${requested} mounted`);
    const tuiFrames = await js('window.__testBenchmarkFrames(5)') as RendererFrameBenchmark;
    console.log(`RENDERER_MEASUREMENT_PHASE ${requested} frames`);

    // Measure xterm's user-input boundary -> Tauri adapter -> test backend echo -> xterm write +
    // scheduled paint. The embedded driver marks the document hidden and cannot deliver trusted OS
    // key events, so use xterm's public input(data, true) API and label the result accordingly.
    await js(`(() => { window.__testArmInputLatency(); window.__testSendInput('z'); })()`);
    await browser.waitUntil(async () => (await js('window.__testInputLatency()')) !== null, {
      timeout: 5000,
      timeoutMsg: 'keypress did not reach an xterm render',
    });
    const inputLatencyMs = await js('window.__testInputLatency()') as number;
    console.log(`RENDERER_MEASUREMENT_PHASE ${requested} input`);

    // Run the destructive scrollback workload last: its completed write can still leave deferred
    // renderer work queued, which would contaminate the per-frame and input-latency samples.
    const throughput = await js('window.__testBenchmarkWrite(50000)') as RendererWriteBenchmark;
    console.log(`RENDERER_MEASUREMENT_PHASE ${requested} throughput`);

    const result = {
      backend: actual,
      terminals: 16,
      rssBeforeMb: rssBeforeKb == null ? null : rssBeforeKb / 1024,
      rssAfterMb: rssAfterKb == null ? null : rssAfterKb / 1024,
      rssDeltaMb: rssBeforeKb == null || rssAfterKb == null ? null : (rssAfterKb - rssBeforeKb) / 1024,
      throughput,
      tuiFrames,
      xtermInputLatencyMs: inputLatencyMs,
    };
    console.log(`RENDERER_MEASUREMENT ${JSON.stringify(result)}`);
  });
});
