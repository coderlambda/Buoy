'use strict';
// GUI test for inline rename (sidebar project + control-mode tab), driven by REAL OS-level
// double-clicks in a real Chromium against the REAL ui/renderer.js and ui/index.html.
//
// Why a full-GUI test and not a unit test: the bug is an event-ORDERING one. A double-click delivers
// click, click, dblclick — and the renderer's row `onclick` calls mount(), which calls
// renderSidebar(), which rebuilds the list with `innerHTML = ''`. By the time dblclick fires, the
// node its handler closed over has already been discarded, so the rename <input> is appended to an
// orphan: created, "focused", and invisible. A synthetic dispatchEvent(new MouseEvent('dblclick'))
// skips the two click events entirely and PASSES against the broken code — only a real click
// sequence reproduces it. Hence sendInputEvent with clickCount 1 then 2, as the OS delivers.
//
// Usage: node_modules/.bin/electron test/gui-rename.js
const { app, BrowserWindow } = require('electron');
const fs = require('fs');
const path = require('path');

let failures = 0;
const check = (c, m) => { console.log((c ? 'ok   ' : 'FAIL ') + m); if (!c) failures++; };

const UI = path.join(__dirname, '..', 'ui');

// Load the REAL index.html, minus the <script src="tauri-api.js"> line — that file needs
// window.__TAURI__ (absent outside the Tauri shell) and would overwrite the preload's stub. The copy
// lives in the ui/ dir so every other relative script/stylesheet src still resolves.
function writeTestPage() {
  const html = fs.readFileSync(path.join(UI, 'index.html'), 'utf8');
  const stripped = html.replace(/\s*<script src="tauri-api\.js"><\/script>/, '');
  if (stripped === html) throw new Error('tauri-api.js script tag not found in ui/index.html');
  const out = path.join(UI, '.gui-rename-test.html');
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
      preload: path.join(__dirname, 'gui-rename-preload.js') },
  });
  const wc = win.webContents;
  const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
  const js = (code) => wc.executeJavaScript(code);

  await win.loadFile(page);
  await sleep(1000);   // let init() + mount() settle

  const errs = await js('window.__errs || []');
  check(errs.length === 0, `no uncaught renderer errors (got ${JSON.stringify(errs)})`);

  // Deliver a REAL double-click at (x, y): down/up clickCount 1, then down/up clickCount 2.
  // Chromium turns this into click, click, dblclick — the ordering the bug depends on.
  async function dblclickAt(x, y) {
    for (const clickCount of [1, 2]) {
      wc.sendInputEvent({ type: 'mouseDown', x, y, button: 'left', clickCount });
      wc.sendInputEvent({ type: 'mouseUp', x, y, button: 'left', clickCount });
      await sleep(40);
    }
    await sleep(250);
  }
  const centerOf = (sel) => js(
    `(() => { const n = document.querySelector(${JSON.stringify(sel)}); if (!n) return null;
       const r = n.getBoundingClientRect(); if (!r.width && !r.height) return null;
       return { x: Math.round(r.left + r.width / 2), y: Math.round(r.top + r.height / 2) }; })()`);
  // The state that decides whether the user can actually rename: an input that exists but isn't
  // connected to the document is exactly the reported symptom ("rename not enabled").
  const editorState = (scope) => js(
    `(() => { const inp = document.querySelector(${JSON.stringify(scope)});
       const li = inp && inp.closest('.session');
       return { present: !!inp, connected: inp ? inp.isConnected : null,
                visible: inp ? !!(inp.offsetWidth || inp.offsetHeight) : null,
                value: inp ? inp.value : null,
                focused: !!inp && document.activeElement === inp,
                rowId: li ? li.dataset.id : null,
                activeRow: (document.querySelector('#sessions .session.active') || {}).dataset
                  ? document.querySelector('#sessions .session.active').dataset.id : null }; })()`);
  const pressKey = async (keyCode) => {
    wc.sendInputEvent({ type: 'keyDown', keyCode });
    if (keyCode === 'Enter') wc.sendInputEvent({ type: 'char', keyCode });
    wc.sendInputEvent({ type: 'keyUp', keyCode });
    await sleep(300);
  };
  const typeInto = (sel, text) => js(
    `(() => { const i = document.querySelector(${JSON.stringify(sel)}); if (!i) return false;
       i.value = ${JSON.stringify(text)}; i.dispatchEvent(new Event('input', { bubbles: true }));
       return true; })()`);

  const rows = await js(`document.querySelectorAll('#sessions .session').length`);
  check(rows === 2, `sidebar rendered both sessions (got ${rows})`);

  // ---- TC-R1 double-click the ACTIVE project's name opens a USABLE rename editor -------------
  // 'project one' is lastActive so it is already mounted/active — precisely the reported case.
  const activePt = await centerOf('#sessions .session.active .name');
  check(!!activePt, 'the active session row has a visible name');
  await dblclickAt(activePt.x, activePt.y);
  const r1 = await editorState('#sessions .session.active .name input');
  check(r1.present === true, 'TC-R1 a rename input exists in the active row after a double-click');
  check(r1.connected === true,
    'TC-R1 the input is CONNECTED to the live document (the bug: it was built on a row renderSidebar() had already discarded)');
  check(r1.visible === true, 'TC-R1 the input is actually visible to the user');
  check(r1.value === 'project one', `TC-R1 seeded with the current title (got ${JSON.stringify(r1.value)})`);
  check(r1.focused === true, 'TC-R1 the input has focus, so typing goes into it');

  // ---- TC-R2 type + Enter commits, reaches the backend, and repaints the label ---------------
  check(await typeInto('#sessions .session.active .name input', 'renamed one'), 'TC-R2 could type into the editor');
  await pressKey('Enter');
  const r2 = await js(`({ renames: window.__renames,
    label: (document.querySelector('#sessions .session.active .name') || {}).textContent,
    editing: !!document.querySelector('#sessions .session.active .name input') })`);
  check(JSON.stringify(r2.renames) === JSON.stringify([['s1', 'renamed one']]),
    `TC-R2 Enter sent the new title to the backend exactly once (got ${JSON.stringify(r2.renames)})`);
  check(r2.editing === false, 'TC-R2 the editor closed after commit');
  check((r2.label || '').trim() === 'renamed one',
    `TC-R2 the sidebar shows the new title (got ${JSON.stringify(r2.label)})`);

  // ---- TC-R3 Escape abandons the edit and sends nothing --------------------------------------
  const pt3 = await centerOf('#sessions .session.active .name');
  await dblclickAt(pt3.x, pt3.y);
  check(await js(`!!document.querySelector('#sessions .session.active .name input')`) === true,
    'TC-R3 a second rename can be started (the first cleaned its state up)');
  await typeInto('#sessions .session.active .name input', 'discard me');
  await pressKey('Escape');
  const r3 = await js(`({ renames: window.__renames,
    label: (document.querySelector('#sessions .session.active .name') || {}).textContent,
    editing: !!document.querySelector('#sessions .session.active .name input') })`);
  check(r3.editing === false, 'TC-R3 Escape closes the editor');
  check(JSON.stringify(r3.renames) === JSON.stringify([['s1', 'renamed one']]),
    `TC-R3 Escape sent NOTHING new to the backend (got ${JSON.stringify(r3.renames)})`);
  check((r3.label || '').trim() === 'renamed one', 'TC-R3 the title is unchanged after Escape');

  // ---- TC-R4 an INACTIVE row renames too -----------------------------------------------------
  // Note the deliberate behavior: the FIRST click of the double-click mounts that project, exactly
  // as a single click does, so renaming an inactive row also switches to it. Suppressing that would
  // mean delaying EVERY row click by the double-click threshold to see whether a second one arrives
  // — a latency cost on the common gesture to save a click on the rare one. What matters, and is
  // asserted here, is that the editor still opens on the row that was double-clicked (before the
  // fix, this row's re-render on mount was itself what destroyed the editor).
  const inactivePt = await centerOf('#sessions .session:not(.active) .name');
  check(!!inactivePt, 'there is an inactive session row');
  await js('window.__setLastActive.length = 0');   // count mounts caused by this gesture alone
  await dblclickAt(inactivePt.x, inactivePt.y);
  const r4 = await editorState('#sessions .session .name input');
  check(r4.present && r4.connected === true && r4.visible === true,
    'TC-R4 an INACTIVE row also gets a live, visible rename input');
  check(r4.focused === true, 'TC-R4 that input has focus');
  check(r4.value === 'project two', `TC-R4 seeded with its OWN title (got ${JSON.stringify(r4.value)})`);
  check(r4.rowId === 's2', `TC-R4 the editor is on the row that was double-clicked (got ${r4.rowId})`);
  check(r4.activeRow === 's2', `TC-R4 that row is now the active project (got ${r4.activeRow})`);
  // …but only ONCE. The first click mounts (accepted, above); the second click of the double-click is
  // the rename gesture, so it must not mount again — a second mount() is a duplicate setLastActive
  // round-trip and, on an unconnected project, a duplicate connect. Measured: without the
  // `e.detail >= 2` guard in li.onclick this is ["s2","s2"].
  const mounts = await js('window.__setLastActive');
  check(JSON.stringify(mounts) === JSON.stringify(['s2']),
    `TC-R4 the double-click mounted the row exactly once, not once per click (got ${JSON.stringify(mounts)})`);
  // Committing must target the double-clicked row, not whatever happens to be active.
  await typeInto('#sessions .session .name input', 'renamed two');
  await pressKey('Enter');
  const r4b = await js('window.__renames');
  check(JSON.stringify(r4b) === JSON.stringify([['s1', 'renamed one'], ['s2', 'renamed two']]),
    `TC-R4 the commit went to s2, not the previously-active s1 (got ${JSON.stringify(r4b)})`);

  // ---- TC-R5 the tab strip shares the defect, so cover tab rename too -----------------------
  // Feed the ACTIVE project (s2 after TC-R4) two tmux windows, as the control backend would.
  const proj = await js(`document.querySelector('#sessions .session.active').dataset.id`);
  const fire = (ev) => js(`window.__fire('window', Object.assign({ id: ${JSON.stringify(proj)} }, ${JSON.stringify(ev)}))`);
  await fire({ action: 'add', window: '@0', order: ['@0'] });
  await fire({ action: 'rename', window: '@0', name: 'shell' });
  await fire({ action: 'add', window: '@1', order: ['@0', '@1'] });
  await fire({ action: 'rename', window: '@1', name: 'logs' });
  await fire({ action: 'active', window: '@0', order: ['@0', '@1'] });
  await sleep(350);
  const tabs = await js(`Array.from(document.querySelectorAll('#tabs .tab:not(.plus)'))
    .map((t) => ({ label: t.querySelector('.tlabel').textContent, active: t.classList.contains('active') }))`);
  check(tabs.length === 2, `TC-R5 two tmux windows became two tabs (got ${tabs.length})`);
  // The NON-active tab is the interesting one: its click handler calls switchTab() -> renderTabs(),
  // which rebuilds the strip the same way renderSidebar() rebuilds the sidebar.
  const idx = tabs.findIndex((t) => !t.active);
  check(idx >= 0, 'TC-R5 one tab is not active');
  const tabPt = await centerOf(`#tabs .tab:not(.plus):nth-of-type(${idx + 1}) .tlabel`);
  check(!!tabPt, 'TC-R5 the non-active tab label is visible');
  await js('window.__tabSelects.length = 0');
  await dblclickAt(tabPt.x, tabPt.y);
  const r5 = await editorState('#tabs .tab input');
  check(r5.present === true, 'TC-R5 a tab rename input exists after a real double-click');
  check(r5.connected === true, 'TC-R5 the tab rename input is CONNECTED to the live document');
  check(r5.visible === true, 'TC-R5 the tab rename input is visible');
  check(r5.focused === true, 'TC-R5 the tab rename input has focus');
  // Same "mount once" rule as TC-R4: the first click switches to the tab, the second is the rename
  // gesture and must not re-issue tmux select-window (switchTab's own early-out only covers the case
  // where the tab is ALREADY active).
  const selects = await js('window.__tabSelects');
  check(selects.length === 1,
    `TC-R5 the double-click selected the tab exactly once (got ${JSON.stringify(selects)})`);

  // ---- TC-R6 committing a tab rename reaches tmux -------------------------------------------
  await typeInto('#tabs .tab input', 'build');
  await pressKey('Enter');
  const tabRen = await js('window.__tabRenames');
  check(tabRen.length === 1 && tabRen[0][0] === proj && tabRen[0][2] === 'build',
    `TC-R6 Enter sent the tab rename to tmux (got ${JSON.stringify(tabRen)})`);
  check(await js(`!!document.querySelector('#tabs .tab input')`) === false,
    'TC-R6 the tab editor closed after commit');

  // ---- TC-R7 a re-render arriving MID-TYPING must not eat the draft or move the caret ----------
  // The rename editor is long-lived from the user's point of view, and renderSidebar() runs on things
  // the user didn't do: session:state events, the 5s tunnel refresh, a reconnect. api.onState calls
  // renderSidebar() unconditionally, so it stands in for all of them. Without the draft+caret
  // mirroring in mountRenameInput, the rebuilt input would come back seeded from v.meta.title —
  // silently discarding what was typed — or with the caret slammed to the end.
  const pt7 = await centerOf('#sessions .session.active .name');
  await dblclickAt(pt7.x, pt7.y);
  check(await js(`!!document.querySelector('#sessions .session.active .name input')`) === true,
    'TC-R7 a rename editor is open');
  // Type a partial value and park the caret mid-string (as if the user is still editing).
  await js(`(() => { const i = document.querySelector('#sessions .session.active .name input');
     i.value = 'half-typed'; i.dispatchEvent(new Event('input', { bubbles: true }));
     i.setSelectionRange(4, 4); i.dispatchEvent(new Event('select', { bubbles: true })); })()`);
  await sleep(60);
  const before = await js(`(() => { const i = document.querySelector('#sessions .session.active .name input');
     return { value: i.value, start: i.selectionStart }; })()`);
  check(before.value === 'half-typed' && before.start === 4,
    `TC-R7 precondition: draft typed with the caret parked at 4 (got ${JSON.stringify(before)})`);
  // Now fire the unrelated re-render. Deliberately for the OTHER project: renderSidebar() rebuilds the
  // whole list, so an event about a session the user isn't editing still blows away their editor.
  await js(`window.__fire('state', { id: 's1', state: 'connected' })`);
  await sleep(300);
  const r7 = await js(`(() => { const i = document.querySelector('#sessions .session.active .name input');
     if (!i) return { present: false };
     return { present: true, connected: i.isConnected, value: i.value,
              start: i.selectionStart, end: i.selectionEnd,
              focused: document.activeElement === i }; })()`);
  check(r7.present === true && r7.connected === true,
    'TC-R7 the editor survives an unrelated re-render (still live in the document)');
  check(r7.value === 'half-typed',
    `TC-R7 the typed draft survived the re-render (got ${JSON.stringify(r7.value)})`);
  check(r7.start === 4 && r7.end === 4,
    `TC-R7 the caret stayed where the user left it, not jumped to the end (got ${r7.start}..${r7.end})`);
  check(r7.focused === true, 'TC-R7 focus came back, so typing continues uninterrupted');
  // And the edit still commits normally afterwards.
  await pressKey('Enter');
  const r7b = await js('window.__renames');
  // The active project is s2 by now (TC-R4 switched to it), and the state event above was for s1.
  check(r7b.length === 3 && r7b[2][0] === 's2' && r7b[2][1] === 'half-typed',
    `TC-R7 the edit still commits after the re-render (got ${JSON.stringify(r7b[2])})`);

  cleanup();
  console.log(failures ? `\n${failures} check(s) FAILED` : '\nall ok');
  app.exit(failures ? 1 : 0);
});
