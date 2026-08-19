import { createChecks, fire, js, loadFixture, session } from './tauri-ui-harness.js';

describe('Tauri UI: durable session lifecycle', () => {
  it('distinguishes Detach from Close and reconstructs a closed session from History', async () => {
    const checks = createChecks();
    await loadFixture([session(1, 'Work session')]);
    await fire('window', { id: 's1', action: 'add', window: '@0', name: 'shell', order: ['@0'] });
    await fire('window', { id: 's1', action: 'active', window: '@0', order: ['@0'] });
    await fire('ready', { id: 's1' });

    await js(`window.__testType('echo recover-this-tab\\r')`);
    await $('.act.detach').click();
    await browser.waitUntil(async () => js(`document.querySelector('#sessions .session .sub').textContent.includes('detached')`));
    const detached = await js(`({
      active: document.querySelectorAll('#sessions .session').length,
      history: document.querySelectorAll('#history .session').length,
      invoked: window.__invocations.some(([name]) => name === 'session_detach'),
    })`);
    checks.check(detached.active === 1 && detached.history === 0 && detached.invoked,
      `TC-SH1 Detach leaves tmux in Sessions (got ${JSON.stringify(detached)})`);

    await $('#recover').click();
    await browser.waitUntil(async () => js(`document.querySelector('.chooser-title')?.textContent === 'Open detached sessions'`));
    const found = await js(`({
      label: document.querySelector('.chooser-item')?.textContent,
      checked: window.__invocations.some(([name]) => name === 'check_open_sessions'),
    })`);
    checks.check(found.checked && found.label?.includes('Work session'),
      `TC-SH2 Check open sessions offers the detached tmux session (got ${JSON.stringify(found)})`);
    await $('.chooser-item').click();
    await fire('window', { id: 's1', action: 'add', window: '@0', name: 'shell', order: ['@0'] });
    await fire('window', { id: 's1', action: 'active', window: '@0', order: ['@0'] });
    await fire('ready', { id: 's1' });

    await js(`window.confirm = () => true`);
    await $('.act.kill').click();
    await browser.waitUntil(async () => js(`document.querySelectorAll('#history .session').length === 1`));
    const closed = await js(`(() => {
      const call = window.__invocations.filter(([name]) => name === 'session_close').pop();
      return {
        active: document.querySelectorAll('#sessions .session').length,
        history: document.querySelectorAll('#history .session').length,
        lastCommand: call?.[1]?.tabs?.[0]?.lastCommand,
      };
    })()`);
    checks.check(closed.active === 0 && closed.history === 1 && closed.lastCommand === 'echo recover-this-tab',
      `TC-SH3 Close archives a per-tab recovery snapshot (got ${JSON.stringify(closed)})`);

    await $('#history .resume').click();
    await browser.waitUntil(async () => js(`document.querySelectorAll('#sessions .session').length === 1 && document.querySelectorAll('#history .session').length === 0`));
    const resumed = await js(`({
      resumed: window.__invocations.some(([name]) => name === 'session_resume'),
      reconnects: window.__invocations.filter(([name]) => name === 'create_session').length,
    })`);
    checks.check(resumed.resumed && resumed.reconnects >= 3,
      `TC-SH4 Resume reconstructs and reconnects the closed session (got ${JSON.stringify(resumed)})`);
    checks.finish();
  });

  it('restores archived rows into History without reconnecting them at launch', async () => {
    const checks = createChecks();
    const archived = {
      ...session(1, 'Archived session'),
      archived: true,
      archivedAt: 1234,
      restorePending: true,
      recoveryTabs: [{ window: '@0', title: 'shell', cwd: '/tmp', lastCommand: 'pwd' }],
    };
    await loadFixture([archived]);
    const restored = await js(`({
      active: document.querySelectorAll('#sessions .session').length,
      history: document.querySelectorAll('#history .session').length,
      creates: window.__invocations.filter(([name]) => name === 'create_session').length,
    })`);
    checks.check(restored.active === 0 && restored.history === 1,
      `TC-SH5 archived rows launch in History (got ${JSON.stringify(restored)})`);
    checks.check(restored.creates === 0,
      `TC-SH5 archived rows do not auto-connect (got ${JSON.stringify(restored)})`);
    checks.finish();
  });
});
