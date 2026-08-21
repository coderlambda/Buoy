// Deterministic, privacy-safe product screenshots for README.md. This suite is intentionally
// excluded from wdio.conf.ts: run `npm run screenshots:readme` only when the documented UI changes.
import type { SessionMeta } from '../ui/src/types.js';
import {
  fire, js, loadFixture, screenshotIfRequested, session,
} from './tauri-ui-harness.js';

const SCREENSHOT_ENV = 'BUOY_README_SCREENSHOT_DIR';

function workspaceSessions(): SessionMeta[] {
  return [
    {
      ...session(1, 'Buoy development'),
      host: 'dev@workstation.example',
      color: '#89b4fa',
    },
    {
      ...session(2, 'Production'),
      host: 'ops@production.example',
      color: '#a6e3a1',
    },
    {
      ...session(3, 'Local tools'),
      host: '',
      kind: 'local',
      transport: 'local',
      color: '#cba6f7',
    },
  ];
}

async function mountProductWorkspace(): Promise<void> {
  await loadFixture(workspaceSessions(), {}, {
    tunnels: { s1: [{ remote: 3000, local: 3000, active: true }] },
  });
  await fire('window', { id: 's1', action: 'add', window: '@0', name: 'shell', order: ['@0'] });
  await fire('window', { id: 's1', action: 'rename', window: '@0', name: 'shell' });
  await fire('window', { id: 's1', action: 'add', window: '@1', name: 'codex', order: ['@0', '@1'] });
  await fire('window', { id: 's1', action: 'rename', window: '@1', name: 'codex' });
  await fire('window', { id: 's1', action: 'add', window: '@2', name: 'logs', order: ['@0', '@1', '@2'] });
  await fire('window', { id: 's1', action: 'rename', window: '@2', name: 'logs' });
  await fire('window', { id: 's1', action: 'active', window: '@0', order: ['@0', '@1', '@2'] });
  await fire('state', { id: 's1', state: 'connected' });
  await fire('state', { id: 's2', state: 'reconnecting' });
  await fire('state', { id: 's3', state: 'connected' });
  await fire('ready', { id: 's1' });
  await fire('data', {
    id: 's1',
    window: '@0',
    data: [
      '\u001b[2J\u001b[H',
      '\u001b[38;5;110mBuoy development\u001b[0m',
      '',
      'dev@workstation \u001b[38;5;75m~/projects/buoy\u001b[0m',
      '$ git status --short',
      ' M README.md',
      '$ npm test',
      '\u001b[32m✓ 52 tests passed\u001b[0m',
      '',
      '$ ',
    ].join('\r\n'),
  });
  await fire('data', {
    id: 's1',
    window: '@1',
    data: '\u001b]777;notify;Codex;Ready for review\u0007',
  });
  await browser.pause(250);
}

describe('README product screenshots', () => {
  before(async () => {
    await browser.setWindowSize(1200, 760);
  });

  it('captures the durable workspace and native tmux tabs', async () => {
    await mountProductWorkspace();
    await screenshotIfRequested('workspace-overview.png', SCREENSHOT_ENV);
  });

  it('captures discovery and import of existing tmux sessions', async () => {
    await browser.setWindowSize(1200, 900);
    const alreadyOpen = {
      ...session(1, 'Current workspace'),
      host: 'dev@example.com',
      session: 'already-open',
      socketName: 'default',
    };
    await loadFixture([alreadyOpen], {}, {
      discovery: {
        tmuxPath: '/usr/bin/tmux',
        tmuxVersion: [3, 6],
        sessions: [
          { name: 'already-open', windows: 2, attached: 1, created: 30 },
          { name: 'product-work', windows: 4, attached: 1, created: 20 },
          { name: 'infrastructure', windows: 3, attached: 0, created: 10 },
        ],
      },
    });
    await js(`document.getElementById('new').click()`);
    await js(`document.getElementById('f-host').value = 'dev@example.com'`);
    await js(`document.getElementById('f-discover').click()`);
    await browser.waitUntil(async () => js(
      `document.querySelectorAll('#tmux-discovery .discovered-session').length === 2`,
    ));
    await js(`document.querySelector('#tmux-discovery .discovered-session').click()`);
    await browser.pause(150);
    await screenshotIfRequested('import-existing-sessions.png', SCREENSHOT_ENV);
  });

  it('captures closed-session History and recovery', async () => {
    const archived: SessionMeta[] = [
      {
        ...session(4, 'API'),
        host: 'dev@staging.example',
        archived: true,
        archivedAt: Date.UTC(2026, 7, 18, 12),
        restorePending: true,
        recoveryTabs: [
          { window: '@0', title: 'editor', cwd: '/srv/api', lastCommand: 'nvim' },
          { window: '@1', title: 'server', cwd: '/srv/api', lastCommand: 'npm run dev' },
        ],
      },
      {
        ...session(5, 'Build'),
        host: '',
        kind: 'local',
        transport: 'local',
        archived: true,
        archivedAt: Date.UTC(2026, 7, 14, 12),
        restorePending: true,
        recoveryTabs: [
          { window: '@0', title: 'build', cwd: '/projects/buoy', lastCommand: 'npm test' },
        ],
      },
    ];
    await browser.setWindowSize(1200, 760);
    await loadFixture([...workspaceSessions().slice(0, 2), ...archived]);
    await fire('window', { id: 's1', action: 'add', window: '@0', name: 'shell', order: ['@0'] });
    await fire('window', { id: 's1', action: 'rename', window: '@0', name: 'shell' });
    await fire('window', { id: 's1', action: 'active', window: '@0', order: ['@0'] });
    await fire('state', { id: 's1', state: 'connected' });
    await fire('state', { id: 's2', state: 'connected' });
    await fire('ready', { id: 's1' });
    await fire('data', {
      id: 's1',
      window: '@0',
      data: '\u001b[2J\u001b[HWorkspace is still running in tmux.\r\n\r\n$ ',
    });
    await browser.pause(200);
    await screenshotIfRequested('session-history.png', SCREENSHOT_ENV);
  });
});
