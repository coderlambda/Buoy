import { createChecks, js, loadFixture, screenshotIfRequested, session } from './tauri-ui-harness.js';

describe('Tauri UI: mobile application shell', () => {
  it('uses capability-driven navigation and remote-only session creation', async () => {
    const checks = createChecks();
    // WKWebView test windows use a 2x backing scale on this host. These physical dimensions give
    // the document the same ~393x852 CSS viewport as a modern iPhone in portrait.
    await browser.setWindowSize(786, 1704);
    await loadFixture([], {}, {
      capabilities: {
        platform: 'mobile',
        localShell: false,
        nativeTabs: true,
        portForwarding: true,
        backgroundConnection: false,
        fileDownload: true,
        sshHostKeyVerification: true,
      },
      createSessionResult: { id: 'mobile-1', session: 'dt-mobile-1', mode: 'control', ready: true },
    });

    const shell = await js(`(() => {
      const display = (id) => getComputedStyle(document.getElementById(id)).display;
      const local = document.getElementById('f-kind-local');
      return {
        platform: document.documentElement.dataset.platform,
        view: document.documentElement.dataset.mobileView,
        sidebar: display('sidebar'),
        main: display('main'),
        localHidden: local.hidden,
        localDisabled: local.disabled,
        warning: display('mobile-security'),
      };
    })()`);
    checks.check(shell.platform === 'mobile', `TC-M1 runtime selected mobile UI (got ${JSON.stringify(shell)})`);
    checks.check(shell.view === 'sessions' && shell.sidebar !== 'none' && shell.main === 'none',
      'TC-M1 mobile starts on the full-screen session list');
    checks.check(shell.localHidden && shell.localDisabled, 'TC-M1 local shell is unavailable on mobile');
    checks.check(shell.warning === 'none', 'TC-M1 verified host-key policy removes the prototype warning');
    await screenshotIfRequested('01-empty.png', 'BUOY_MOBILE_SCREENSHOT_DIR');

    await $('#new').click();
    const dialog = await js(`(() => ({
      open: document.getElementById('dialog').open,
      passwordHidden: document.getElementById('mobile-ssh-fields').hidden,
      tabsRow: getComputedStyle(document.getElementById('native-tabs-row')).display,
    }))()`);
    checks.check(dialog.open && !dialog.passwordHidden, 'TC-M2 mobile new-session dialog exposes ephemeral SSH credentials');
    checks.check(dialog.tabsRow !== 'none', 'TC-M2 mobile exposes tmux native-tabs mode');
    await screenshotIfRequested('02-connect-sheet.png', 'BUOY_MOBILE_SCREENSHOT_DIR');

    await $('#f-host').setValue('alice@vpn-host');
    await $('#f-ssh-password').setValue('one-use-secret');
    await $('#f-title').setValue('VPN shell');
    await $('#f-ok').click();
    await browser.waitUntil(async () => browser.execute(
      () => document.querySelectorAll('#sessions .session').length === 1,
    ), { timeout: 5000 });

    const created = await js(`(() => {
      const create = window.__invocations.find(([name]) => name === 'create_session');
      const display = (id) => getComputedStyle(document.getElementById(id)).display;
      return {
        meta: create && create[1].meta,
        view: document.documentElement.dataset.mobileView,
        sidebar: display('sidebar'),
        main: display('main'),
        bar: display('mobile-bar'),
        keys: display('mobile-keys'),
        terminals: document.querySelectorAll('#term .xterm').length,
        state: document.querySelector('#sessions .session .dot')?.className,
        gate: document.getElementById('term').classList.contains('gated'),
        passwordAfter: document.getElementById('f-ssh-password').value,
      };
    })()`);
    checks.check(created.meta?.kind === 'remote' && created.meta?.mode === 'control',
      `TC-M3 mobile creates a remote tmux control-mode session (got ${JSON.stringify(created.meta)})`);
    checks.check(created.meta?.sshPassword === 'one-use-secret' && created.passwordAfter === '',
      'TC-M3 password crosses the invoke boundary once and is cleared from the form');
    checks.check(created.view === 'terminal' && created.sidebar === 'none' && created.main !== 'none',
      'TC-M3 successful creation navigates to the terminal');
    checks.check(created.bar !== 'none' && created.keys !== 'none' && created.terminals === 1,
      'TC-M3 terminal shows mobile header, key row, and xterm');
    checks.check(created.state?.includes('connected') && !created.gate,
      `TC-M3 a ready create result cannot remain visually connecting (got ${JSON.stringify(created)})`);
    await screenshotIfRequested('03-terminal.png', 'BUOY_MOBILE_SCREENSHOT_DIR');

    const dictated = await js(`(async () => {
      const textarea = document.querySelector('#term .xterm-helper-textarea');
      const before = window.__inputs.length;
      const composition = (name, data) => textarea.dispatchEvent(new CompositionEvent(name, {
        bubbles: true, cancelable: true, data,
      }));
      composition('compositionstart', '');
      textarea.value = '狠狠q';
      composition('compositionupdate', '狠狠q');
      await new Promise((resolve) => setTimeout(resolve, 0));
      composition('compositionend', '狠狠');
      // iOS can begin the next dictation/IME composition before xterm's deferred finalizer runs.
      // The new composition's start is the authoritative end of the text just committed.
      textarea.value = '狠狠';
      composition('compositionstart', '');
      textarea.value = '狠狠q';
      composition('compositionupdate', 'q');
      await new Promise((resolve) => setTimeout(resolve, 10));
      composition('compositionend', 'q');
      await new Promise((resolve) => setTimeout(resolve, 10));
      return window.__inputs.slice(before).map((call) => call[1]).join('');
    })()`);
    checks.check(dictated === '狠狠q',
      `TC-M3b consecutive mobile dictation corrections are committed once (got ${JSON.stringify(dictated)})`);

    const streamed = await js(`(async () => {
      const textarea = document.querySelector('#term .xterm-helper-textarea');
      const before = window.__inputs.length;
      const update = (data) => {
        textarea.dispatchEvent(new InputEvent('beforeinput', {
          bubbles: true, cancelable: true, data, inputType: 'insertText',
        }));
        textarea.value = data;
        textarea.dispatchEvent(new InputEvent('input', {
          bubbles: true, cancelable: true, data, inputType: 'insertText',
        }));
      };
      update('summarize');
      update('summarize the content');
      update('summarize the counting');
      update('summarize the counting current');
      update('summarize the counting current package');
      textarea.dispatchEvent(new InputEvent('beforeinput', {
        bubbles: true, cancelable: true, data: '!', inputType: 'insertText',
      }));
      textarea.value += '!';
      textarea.dispatchEvent(new InputEvent('input', {
        bubbles: true, cancelable: true, data: '!', inputType: 'insertText',
      }));
      await new Promise((resolve) => setTimeout(resolve, 0));
      const chunks = window.__inputs.slice(before).map((call) => call[1]);
      let rendered = '';
      for (const character of Array.from(chunks.join(''))) {
        rendered = character === '\\x7f' ? Array.from(rendered).slice(0, -1).join('') : rendered + character;
      }
      return { chunks, rendered };
    })()`);
    checks.check(streamed.rendered === 'summarize the counting current package!',
      `TC-M3c streaming iOS dictation replaces corrected tails instead of appending snapshots (got ${JSON.stringify(streamed)})`);

    // The close snapshot receives the most recent command per tab. This input is tracked locally
    // but remains ordinary terminal input; the backend is still the authority for execution.
    await js(`window.__testType('\\x15echo recover-this-tab\\r')`);

    await $('#mobile-back').click();
    const back = await js(`({
      view: document.documentElement.dataset.mobileView,
      sidebar: getComputedStyle(document.getElementById('sidebar')).display,
      main: getComputedStyle(document.getElementById('main')).display,
    })`);
    checks.check(back.view === 'sessions' && back.sidebar !== 'none' && back.main === 'none',
      'TC-M4 Back returns to the session list without detaching SSH');

    // Detach is now distinct from Close: it leaves the row active and the remote tmux alive.
    await js(`document.querySelector('#sessions .session .more').click()`);
    await js(`Array.from(document.querySelectorAll('.chooser-item')).find((item) => item.textContent.includes('Detach')).click()`);
    await browser.waitUntil(async () => js(`document.querySelector('#sessions .session .sub').textContent.includes('detached')`));
    const detached = await js(`({
      active: document.querySelectorAll('#sessions .session').length,
      history: document.querySelectorAll('#history .session').length,
      invoked: window.__invocations.some(([name]) => name === 'session_detach'),
    })`);
    checks.check(detached.active === 1 && detached.history === 0 && detached.invoked,
      `TC-M4b Detach keeps the session out of History (got ${JSON.stringify(detached)})`);

    // Check all known sessions verifies the remote tmux and offers a one-tap reattach.
    await $('#recover').click();
    await browser.waitUntil(async () => js(`document.querySelector('.chooser-title')?.textContent === 'Open detached sessions'`));
    const recoveryChoice = await js(`({
      label: document.querySelector('.chooser-item')?.textContent,
      checked: window.__invocations.some(([name]) => name === 'check_open_sessions'),
    })`);
    checks.check(recoveryChoice.checked && recoveryChoice.label?.includes('VPN shell'),
      `TC-M4c remote check finds the detached session (got ${JSON.stringify(recoveryChoice)})`);
    await $('.chooser-item').click();
    await browser.waitUntil(async () => js(`document.documentElement.dataset.mobileView === 'terminal'`));
    await $('#mobile-back').click();

    // Close ends tmux and moves a reconstructable snapshot to History; Resume invokes the restore
    // path and returns the row to active Sessions.
    await js(`window.confirm = () => true`);
    await js(`document.querySelector('#sessions .session .more').click()`);
    await js(`Array.from(document.querySelectorAll('.chooser-item')).find((item) => item.textContent.includes('Close and')).click()`);
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
      `TC-M4d Close archives a per-tab recovery snapshot (got ${JSON.stringify(closed)})`);
    await $('#history .resume').click();
    await browser.waitUntil(async () => js(`document.querySelectorAll('#sessions .session').length === 1 && document.querySelectorAll('#history .session').length === 0`));
    const resumed = await js(`({
      resumed: window.__invocations.some(([name]) => name === 'session_resume'),
      reattached: window.__invocations.filter(([name]) => name === 'create_session').length >= 3,
    })`);
    checks.check(resumed.resumed && resumed.reattached,
      `TC-M4e Resume reconstructs and reattaches the closed session (got ${JSON.stringify(resumed)})`);
    checks.finish();
  });

  it('requests a restored session password in a masked, one-shot field', async () => {
    const checks = createChecks();
    await browser.setWindowSize(786, 1704);
    const restored = session(1, 'Restored VPN session');
    await loadFixture([restored], {}, {
      capabilities: {
        platform: 'mobile', localShell: false, nativeTabs: true, portForwarding: true,
        backgroundConnection: false, fileDownload: true, sshHostKeyVerification: true,
      },
      rejectCreateWithoutPassword: true,
      createSessionResult: { ready: true },
    });
    await screenshotIfRequested('04-restored-session.png', 'BUOY_MOBILE_SCREENSHOT_DIR');

    await $('#sessions .session').click();
    await browser.waitUntil(async () => js(`document.getElementById('mobile-auth-dialog').open`));
    await screenshotIfRequested('05-auth-sheet.png', 'BUOY_MOBILE_SCREENSHOT_DIR');
    const prompt = await js(`({
      type: document.getElementById('mobile-auth-password').type,
      host: document.getElementById('mobile-auth-host').textContent,
    })`);
    checks.check(prompt.type === 'password' && prompt.host === restored.host,
      `TC-M5 restored credentials use a masked host-specific prompt (got ${JSON.stringify(prompt)})`);

    await $('#mobile-auth-password').setValue('restore-secret');
    await $('#mobile-auth-dialog button[value="ok"]').click();
    await browser.waitUntil(async () => js(`window.__invocations.filter(([name]) => name === 'create_session').length === 2`));
    const attempts = await js(`(() => {
      const calls = window.__invocations.filter(([name]) => name === 'create_session');
      return {
        firstPassword: calls[0][1].meta.sshPassword,
        secondPassword: calls[1][1].meta.sshPassword,
        fieldAfter: document.getElementById('mobile-auth-password').value,
        open: document.getElementById('mobile-auth-dialog').open,
      };
    })()`);
    checks.check(attempts.firstPassword == null && attempts.secondPassword === 'restore-secret',
      `TC-M5 password is sent only on the authentication retry (got ${JSON.stringify(attempts)})`);
    checks.check(attempts.fieldAfter === '' && !attempts.open,
      'TC-M5 the restored-session password field is cleared after use');
    checks.finish();
  });
});
