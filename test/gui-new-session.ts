
// Creation, validation, and visual regressions for the New session form, driven through Buoy's
// real Tauri webview. Computed styles therefore come from the platform webview that ships to users.
import {
  createChecks, js, loadFixture, screenshotIfRequested, session,
} from './tauri-ui-harness.js';

const baseSessions = () => [
  session(1, 'native project'),
  session(2, 'plain project', 'plain'),
];

describe('Tauri UI: new session dialog', () => {
  const openDialog = () => js(`document.getElementById('new').click()`);
  const submitCreate = () => js(`document.getElementById('f-ok').click()`);
  const commandCalls = (command: string): Promise<UiTestInvocation[]> => js(
    `window.__invocations.filter((call) => call[0] === ${JSON.stringify(command)})`,
  );
  const newSessionCalls = async () => (await commandCalls('create_session')).filter(
    (call: UiTestInvocation) => !(call[1] && call[1].meta && call[1].meta.id),
  );

  beforeEach(async () => {
    await browser.setWindowSize(1000, 700);
    await loadFixture(baseSessions());
  });

  it('keeps the native select themed and functional', async () => {
    const { check, finish } = createChecks();
    await openDialog();

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
        selectFocused: select.matches(':focus'),
        focusBorderMatches: selectStyle.borderColor === arrowStyle.borderRightColor,
        radiusMatches: selectStyle.borderRadius === inputStyle.borderRadius,
        fontMatches: selectStyle.fontFamily === inputStyle.fontFamily
          && selectStyle.fontSize === inputStyle.fontSize,
        selectSurface: {
          background: selectStyle.backgroundColor,
          border: selectStyle.borderColor,
          radius: selectStyle.borderRadius,
          fontFamily: selectStyle.fontFamily,
          fontSize: selectStyle.fontSize,
        },
        inputSurface: {
          background: inputStyle.backgroundColor,
          border: inputStyle.borderColor,
          radius: inputStyle.borderRadius,
          fontFamily: inputStyle.fontFamily,
          fontSize: inputStyle.fontSize,
        },
        arrow: arrowStyle.content !== 'none' && arrowStyle.pointerEvents === 'none',
      };
    })()`);

    check(state.open, 'TC-NS1 New session dialog opens');
    check(state.appearance === 'none',
      `TC-NS1 Type field removes native select chrome (got ${state.appearance})`);
    check(Math.abs(state.selectHeight - state.inputHeight) < 0.5,
      `TC-NS1 Type and Host fields have the same height (${state.selectHeight}px / ${state.inputHeight}px)`);
    check(state.backgroundMatches && state.radiusMatches && state.fontMatches
      && (state.borderMatches || (state.selectFocused && state.focusBorderMatches)),
    `TC-NS1 Type field matches text inputs and keeps its focused accent border (${JSON.stringify({ select: state.selectSurface, input: state.inputSurface, selectFocused: state.selectFocused })})`);
    check(state.arrow, 'TC-NS1 Type field shows a non-interactive theme chevron');

    if (process.env.BUOY_GUI_SCREENSHOT) {
      await browser.pause(100);
      await screenshotIfRequested(null);
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
      'TC-NS1 styled native select still switches the form to a local session');

    const errors = await js('window.__errs || []');
    check(errors.length === 0, `TC-NS1 no renderer errors (got ${JSON.stringify(errors)})`);
    finish();
  });

  it('validates, cancels, and resets transient dialog state', async () => {
    const { check, finish } = createChecks();
    await openDialog();
    await submitCreate();
    await browser.pause(50);

    const invalid = await js(`(() => ({
      open: document.getElementById('dialog').open,
      message: document.getElementById('f-err').textContent,
    }))()`);
    check(invalid.open, 'TC-NS2 blank remote host keeps the dialog open');
    check(invalid.message === 'Enter a host (user@host).',
      `TC-NS2 blank remote host shows a useful inline error (got ${JSON.stringify(invalid.message)})`);
    check((await newSessionCalls()).length === 0,
      'TC-NS2 validation prevents the backend create command');

    await js(`(() => {
      document.getElementById('f-control').click();
      document.querySelector('#form button[value="cancel"]').click();
    })()`);
    await browser.pause(50);
    check(await js(`document.getElementById('dialog').open`) === false,
      'TC-NS3 Cancel closes the dialog');
    check((await newSessionCalls()).length === 0,
      'TC-NS3 Cancel creates no session');

    await openDialog();
    const reopened = await js(`({
      native: document.getElementById('f-control').getAttribute('aria-checked'),
      message: document.getElementById('f-err').textContent,
    })`);
    check(reopened.native === 'true', 'TC-NS3 reopening restores Native tabs to its default');
    check(reopened.message === '', 'TC-NS3 reopening clears the previous validation error');
    finish();
  });

  it('filters host history and selects an entry before blur', async () => {
    const { check, finish } = createChecks();
    await loadFixture(baseSessions(), {}, {
      hosts: ['alice@production.example', 'bob@staging.example', 'ops@backup.example'],
    });
    await openDialog();
    await js(`document.getElementById('f-host').focus()`);
    await browser.waitUntil(async () => js(
      `document.querySelectorAll('#host-history.on li').length === 3`));
    check(JSON.stringify(await js(
      `Array.from(document.querySelectorAll('#host-history li')).map((node) => node.textContent)`))
      === JSON.stringify(['alice@production.example', 'bob@staging.example', 'ops@backup.example']),
    'TC-NS4 focusing Host shows backend history in recency order');

    await js(`(() => {
      const input = document.getElementById('f-host');
      input.value = 'staging';
      input.dispatchEvent(new Event('input', { bubbles: true }));
    })()`);
    await browser.waitUntil(async () => js(
      `document.querySelectorAll('#host-history.on li').length === 1`));
    check(await js(`document.querySelector('#host-history li').textContent`)
      === 'bob@staging.example', 'TC-NS4 typing filters the host history');

    await js(`document.querySelector('#host-history li').dispatchEvent(new MouseEvent('mousedown', {
      bubbles: true, cancelable: true, button: 0,
    }))`);
    const selected = await js(`({
      value: document.getElementById('f-host').value,
      visible: document.getElementById('host-history').classList.contains('on'),
      focused: document.activeElement === document.getElementById('f-host'),
    })`);
    check(selected.value === 'bob@staging.example' && !selected.visible && selected.focused,
      `TC-NS4 selecting history fills, hides, and refocuses Host (got ${JSON.stringify(selected)})`);
    check((await commandCalls('list_hosts')).length >= 2,
      'TC-NS4 focus and filtering both use the Tauri list_hosts adapter');
    finish();
  });

  it('creates a trimmed remote session and adopts the backend result', async () => {
    const { check, finish } = createChecks();
    await loadFixture(baseSessions(), {}, {
      createSessionResult: {
        id: 'remote-created', session: 'dt-remote-created', mode: 'plain',
        tmuxPath: '/opt/homebrew/bin/tmux', tmuxVersion: [3, 5],
      },
    });
    await openDialog();
    await js(`(() => {
      document.getElementById('f-host').value = '  dev@example.test:2222  ';
      document.getElementById('f-title').value = '  deploy box  ';
      document.getElementById('f-control').click();
    })()`);
    await submitCreate();
    await browser.waitUntil(async () => js(
      `!!document.querySelector('#sessions .session[data-id="remote-created"]')`));

    const creates = await newSessionCalls();
    const sent = creates[0] && creates[0][1] && creates[0][1].meta;
    check(creates.length === 1, 'TC-NS5 remote creation invokes the backend exactly once');
    check(sent && sent.kind === 'remote' && sent.transport === 'ssh' && sent.mode === 'plain'
      && sent.title === 'deploy box' && sent.host === 'dev@example.test:2222'
      && Object.keys(sent).length === 5,
    `TC-NS5 remote metadata is trimmed and mapped correctly (got ${JSON.stringify(sent)})`);

    const ui = await js(`(() => { const row = document.querySelector(
      '#sessions .session[data-id="remote-created"]'); return {
        dialogOpen: document.getElementById('dialog').open,
        rows: document.querySelectorAll('#sessions .session').length,
        active: row.classList.contains('active'),
        title: row.querySelector('.name').textContent,
        sub: row.querySelector('.sub').textContent,
        tabsVisible: document.getElementById('tabs').classList.contains('on'),
        terminalMounted: !!document.querySelector('#term .xterm'),
      }; })()`);
    check(!ui.dialogOpen && ui.rows === 3 && ui.active,
      `TC-NS5 success closes the dialog and activates one new row (got ${JSON.stringify(ui)})`);
    check(ui.title === 'deploy box' && ui.sub.includes('dev@example.test:2222')
      && ui.sub.includes('tmux 3.5'),
    `TC-NS5 sidebar adopts the returned tmux metadata (got ${JSON.stringify(ui)})`);
    check(!ui.tabsVisible && ui.terminalMounted,
      'TC-NS5 backend plain-mode downgrade mounts one terminal without native tabs');
    finish();
  });

  it('creates a local session without leaking remote metadata', async () => {
    const { check, finish } = createChecks();
    await loadFixture(baseSessions(), {}, {
      createSessionResult: {
        id: 'local-created', session: 'dt-local-created', mode: 'local',
        tmuxPath: null, tmuxVersion: null,
      },
    });
    await openDialog();
    await js(`(() => {
      const kind = document.getElementById('f-kind');
      kind.value = 'local';
      kind.dispatchEvent(new Event('change', { bubbles: true }));
      document.getElementById('f-host').value = 'must-not-leak.example';
      document.getElementById('f-title').value = '';
    })()`);
    await submitCreate();
    await browser.waitUntil(async () => js(
      `!!document.querySelector('#sessions .session[data-id="local-created"]')`));

    const creates = await newSessionCalls();
    const sent = creates[0] && creates[0][1] && creates[0][1].meta;
    check(sent && sent.kind === 'local' && sent.transport === 'local' && sent.mode === 'control'
      && sent.title === 'local' && sent.host === '' && Object.keys(sent).length === 5,
    `TC-NS6 local metadata reaches the Tauri adapter (got ${JSON.stringify(sent)})`);
    const ui = await js(`(() => { const row = document.querySelector(
      '#sessions .session[data-id="local-created"]'); return {
        title: row.querySelector('.name').textContent,
        sub: row.querySelector('.sub').textContent,
        tabsVisible: document.getElementById('tabs').classList.contains('on'),
        terminalMounted: !!document.querySelector('#term .xterm'),
      }; })()`);
    check(ui.title === 'local' && ui.sub === 'local shell',
      `TC-NS6 backend bare-pty downgrade renders as a local shell (got ${JSON.stringify(ui)})`);
    check(!ui.tabsVisible && ui.terminalMounted,
      'TC-NS6 bare local mode mounts one terminal without a tab strip');
    finish();
  });

  it('discovers and imports a session from the default tmux server', async () => {
    const { check, finish } = createChecks();
    const alreadyOpen = {
      ...session(3, 'existing shell'),
      host: 'dev@example.test',
      session: 'existing',
      socketName: 'default',
    };
    await loadFixture([...baseSessions(), alreadyOpen], {}, {
      discovery: {
        tmuxPath: '/home/dev/.local/bin/tmux', tmuxVersion: [3, 7],
        sessions: [
          { name: 'existing', windows: 2, attached: 1, created: 30 },
          { name: 'work', windows: 3, attached: 1, created: 20 },
          { name: 'notes', windows: 1, attached: 0, created: 10 },
        ],
      },
      createSessionResult: { id: 'imported-work', session: 'work', mode: 'control',
        tmuxPath: '/home/dev/.local/bin/tmux', tmuxVersion: [3, 7], socketName: 'default' },
    });
    await openDialog();
    await js(`document.getElementById('f-host').value = 'dev@example.test'`);
    await js(`document.getElementById('f-discover').click()`);
    await browser.waitUntil(async () => js(
      `document.querySelectorAll('#tmux-discovery .discovered-session').length === 2`));

    const discovered = await js(`Array.from(document.querySelectorAll(
      '#tmux-discovery .discovered-session')).map((node) => node.textContent)`);
    check(discovered.length === 2 && !discovered.some((text: string) => text.includes('existing'))
      && discovered[0].includes('work') && discovered[0].includes('3 windows')
      && discovered[0].includes('1 attached'),
    `TC-NS7 discovery skips already-open sessions and shows the remaining topology (got ${JSON.stringify(discovered)})`);

    await js(`document.querySelector('#tmux-discovery .discovered-session').click()`);
    check(await js(`document.getElementById('f-ok').textContent`) === 'Import',
      'TC-NS7 selecting an existing session changes the primary action to Import');
    await submitCreate();
    await browser.waitUntil(async () => js(
      `!!document.querySelector('#sessions .session[data-id="imported-work"]')`));

    const discovers = await commandCalls('discover_tmux_sessions');
    const creates = await newSessionCalls();
    const sent = creates[0]?.[1]?.meta;
    check(discovers.length === 1 && discovers[0]?.[1]?.kind === 'remote'
      && discovers[0]?.[1]?.host === 'dev@example.test',
    `TC-NS7 discovery targets the selected SSH host (got ${JSON.stringify(discovers)})`);
    check(sent?.session === 'work' && sent?.socketName === 'default'
      && sent?.tmuxPath === '/home/dev/.local/bin/tmux'
      && JSON.stringify(sent?.tmuxVersion) === JSON.stringify([3, 7]),
    `TC-NS7 import preserves the existing name/default socket and probed tmux (got ${JSON.stringify(sent)})`);
    finish();
  });

  it('shows an empty import state when every discovered session is already open', async () => {
    const { check, finish } = createChecks();
    const openSession = (n: number, name: string) => ({
      ...session(n, name),
      host: 'dev@example.test',
      session: name,
      socketName: 'default',
    });
    await loadFixture([
      ...baseSessions(),
      openSession(3, 'work'),
      openSession(4, 'notes'),
    ], {}, {
      discovery: {
        tmuxPath: '/usr/bin/tmux', tmuxVersion: [3, 6],
        sessions: [
          { name: 'work', windows: 3, attached: 1, created: 20 },
          { name: 'notes', windows: 1, attached: 0, created: 10 },
        ],
      },
    });
    await openDialog();
    await js(`document.getElementById('f-host').value = 'dev@example.test'`);
    await js(`document.getElementById('f-discover').click()`);
    await browser.waitUntil(async () => js(
      `document.getElementById('tmux-discovery').textContent.includes('already open')`));

    const state = await js(`({
      options: document.querySelectorAll('#tmux-discovery .discovered-session').length,
      message: document.getElementById('tmux-discovery').textContent,
      action: document.getElementById('f-ok').textContent,
    })`);
    check(state.options === 0
      && state.message === 'No new sessions to import. All discovered sessions are already open.'
      && state.action === 'Create',
    `TC-NS8 all-open discovery has no import choices (got ${JSON.stringify(state)})`);
    finish();
  });

  it('keeps a backend creation failure visible and allows retry', async () => {
    const { check, finish } = createChecks();
    await loadFixture(baseSessions(), {}, { reject: { create_session: 'ssh spawn denied' } });
    await openDialog();
    await js(`document.getElementById('f-host').value = 'broken@example.test'`);
    await submitCreate();
    await browser.waitUntil(async () => js(
      `document.getElementById('f-err').textContent.includes('ssh spawn denied')`));

    let state = await js(`({
      open: document.getElementById('dialog').open,
      message: document.getElementById('f-err').textContent,
      rows: document.querySelectorAll('#sessions .session').length,
      rendererErrors: window.__errs.slice(),
    })`);
    check(state.open && state.rows === 2,
      `TC-NS9 failed creation stays open and adds no row (got ${JSON.stringify(state)})`);
    check(state.message === 'Could not create session: ssh spawn denied',
      `TC-NS9 failed creation shows the backend reason (got ${JSON.stringify(state.message)})`);
    check(state.rendererErrors.length === 0,
      `TC-NS9 the handled rejection produces no uncaught renderer error (got ${JSON.stringify(state.rendererErrors)})`);

    await js(`delete window.__BUOY_UI_TEST__.fixture.backend.reject.create_session`);
    await submitCreate();
    await browser.waitUntil(async () => js(
      `!!document.querySelector('#sessions .session[data-id="created-1"]')`));
    state = await js(`({
      open: document.getElementById('dialog').open,
      rows: document.querySelectorAll('#sessions .session').length,
      active: document.querySelector('#sessions .session.active').dataset.id,
    })`);
    check(!state.open && state.rows === 3 && state.active === 'created-1',
      `TC-NS9 retry succeeds without reopening the dialog (got ${JSON.stringify(state)})`);
    check((await newSessionCalls()).length === 2,
      'TC-NS9 one failed attempt plus one retry produced exactly two backend calls');
    finish();
  });
});
