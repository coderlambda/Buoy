
import * as fs from 'node:fs';
import * as path from 'node:path';
import type { AppConfig, SessionMeta, SessionMode } from '../ui/src/types.js';

export const session = (n: number, title: string, mode: SessionMode = 'control'): SessionMeta => ({
  id: `s${n}`,
  host: `me@host-${n}`,
  session: `dt-s${n}`,
  transport: 'ssh',
  mode,
  title,
  tmuxPath: '/usr/bin/tmux',
  tmuxVersion: [3, 6],
  order: n - 1,
  color: null,
  lastTab: null,
  tabOrder: [],
  tabColors: {},
});

export async function loadFixture(
  sessions: SessionMeta[],
  config: Partial<AppConfig> = {},
  backend: Record<string, unknown> = {},
): Promise<void> {
  const token = `${Date.now()}-${Math.random()}`;
  const fixture = {
    token,
    sessions,
    config: { loopbackHosts: ['localhost'], lastActive: sessions[0] ? sessions[0].id : null, ...config },
    backend,
  };
  await browser.waitUntil(async () => browser.execute(
    () => typeof window.__BUOY_UI_TEST__ === 'object' && typeof window.__testReset === 'function',
  ), { timeout: 10000, timeoutMsg: 'Tauri UI test bridge did not initialize' });
  await browser.execute(`
    window.__BUOY_UI_TEST__.setFixture(${JSON.stringify(fixture)});
    return window.__testReset();
  `);
  await browser.waitUntil(async () => browser.execute(
    (expected) => window.__BUOY_UI_TEST_FIXTURE_TOKEN__ === expected,
    token,
  ), { timeout: 10000, timeoutMsg: 'Tauri UI fixture was not installed' });
  await browser.waitUntil(async () => browser.execute(
    (count) => document.querySelectorAll('#sessions .session').length === count,
    sessions.length,
  ), { timeout: 10000, timeoutMsg: 'renderer did not mount the expected fake sessions' });
  await browser.pause(100);
}

// WebDriver's execute/sync treats a source string as a function body, so preserve the suites'
// compact expression-style helpers explicitly.
export const js = (code: string): Promise<any> => browser.execute(`return (${code});`) as Promise<any>;
export const fire = (event: string, payload: Record<string, unknown>): Promise<unknown> => browser.execute(
  (name, value) => window.__fire(name, value), event, payload,
);

export function createChecks() {
  const failures: string[] = [];
  return {
    check(condition: unknown, message: string): void {
      console.log((condition ? 'ok   ' : 'FAIL ') + message);
      if (!condition) failures.push(message);
    },
    finish(): void {
      if (failures.length) throw new Error(`${failures.length} check(s) failed:\n- ${failures.join('\n- ')}`);
    },
  };
}

export async function screenshotIfRequested(file: string | null, envName = 'BUOY_GUI_SCREENSHOT'): Promise<void> {
  const target = process.env[envName];
  if (!target) return;
  const out = file ? path.join(target, file) : target;
  fs.mkdirSync(path.dirname(out), { recursive: true });
  await browser.saveScreenshot(out);
}
