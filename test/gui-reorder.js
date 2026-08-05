'use strict';
// GUI test for drag-to-reorder (sidebar projects + control-mode tabs), driven by REAL OS-level
// mouse press/move/release in a real Chromium against the REAL ui/renderer.js and ui/index.html.
//
// Why a full-GUI test: the shipped bug was that HTML5 drag-and-drop never fires in the Tauri
// webview at all (wry overrides WKWebView's NSDraggingDestination methods for file-drop and answers
// every drag "copy" without forwarding to WebKit — hence the reported "+" icon and immovable card).
// The fix replaces dragstart/dragover/drop with pointer events, and the only way to know pointer
// events actually produce a reorder is to deliver real ones: a synthetic
// dispatchEvent(new PointerEvent('pointermove')) would exercise our own handlers while proving
// nothing about whether the gesture is reachable, and — worse — would have PASSED against the old
// HTML5 code too if written with dispatchEvent(new DragEvent(...)).
//
// Usage: node_modules/.bin/electron test/gui-reorder.js
const { app, BrowserWindow } = require('electron');
const fs = require('fs');
const path = require('path');

let failures = 0;
const check = (c, m) => { console.log((c ? 'ok   ' : 'FAIL ') + m); if (!c) failures++; };

const UI = path.join(__dirname, '..', 'ui');

// Load the REAL index.html minus the <script src="tauri-api.js"> tag (it needs window.__TAURI__ and
// would overwrite the preload's stub). Kept in ui/ so relative script/stylesheet srcs still resolve.
function writeTestPage() {
  const html = fs.readFileSync(path.join(UI, 'index.html'), 'utf8');
  const stripped = html.replace(/\s*<script src="tauri-api\.js"><\/script>/, '');
  if (stripped === html) throw new Error('tauri-api.js script tag not found in ui/index.html');
  const out = path.join(UI, '.gui-reorder-test.html');
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
      preload: path.join(__dirname, 'gui-reorder-preload.js') },
  });
  const wc = win.webContents;
  const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
  const js = (code) => wc.executeJavaScript(code);

  await win.loadFile(page);
  await sleep(1000);   // let init() + mount() settle

  const errs = await js('window.__errs || []');
  check(errs.length === 0, `no uncaught renderer errors (got ${JSON.stringify(errs)})`);

  // Deliver a REAL drag: mouseDown, several mouseMoves (a single jump wouldn't clear the 4px
  // threshold in a lifelike way, and we want the intermediate frames to assert against), mouseUp.
  // `steps` matters: the renderer only starts dragging on a move past DRAG_THRESHOLD, so a
  // down/up with no move in between must stay a click — asserted separately below.
  async function dragBy(from, dx, dy, opts = {}) {
    const steps = opts.steps || 8;
    wc.sendInputEvent({ type: 'mouseDown', x: from.x, y: from.y, button: 'left', clickCount: 1 });
    await sleep(30);
    for (let i = 1; i <= steps; i++) {
      wc.sendInputEvent({ type: 'mouseMove',
        x: Math.round(from.x + (dx * i) / steps), y: Math.round(from.y + (dy * i) / steps) });
      await sleep(25);
      if (opts.onMove) await opts.onMove(i);
    }
    if (opts.beforeUp) await opts.beforeUp();
    if (opts.abandon) return;               // caller wants to inspect the mid-drag state and clean up
    wc.sendInputEvent({ type: 'mouseUp',
      x: Math.round(from.x + dx), y: Math.round(from.y + dy), button: 'left', clickCount: 1 });
    await sleep(350);                       // commit + re-render + transition
  }

  const centerOf = (sel) => js(
    `(() => { const n = document.querySelector(${JSON.stringify(sel)}); if (!n) return null;
       const r = n.getBoundingClientRect(); if (!r.width && !r.height) return null;
       return { x: Math.round(r.left + r.width / 2), y: Math.round(r.top + r.height / 2) }; })()`);
  const rowIds = () => js(
    `Array.from(document.querySelectorAll('#sessions .session')).map((n) => n.dataset.id)`);
  const tabLabels = () => js(
    `Array.from(document.querySelectorAll('#tabs .tab:not(.plus) .tlabel')).map((n) => n.textContent)`);

  // ---- preconditions -------------------------------------------------------------------------
  const ids0 = await rowIds();
  check(JSON.stringify(ids0) === JSON.stringify(['s1', 's2', 's3']),
    `sidebar rendered three projects in order (got ${JSON.stringify(ids0)})`);
  // The shipped bug's own fingerprint: the rows must NOT be native-draggable any more. Leaving
  // draggable=true hands the gesture straight back to the machinery that swallows it.
  const draggables = await js(
    `Array.from(document.querySelectorAll('#sessions .session')).map((n) => n.draggable)`);
  check(draggables.every((d) => d === false),
    `TC-D0 rows are not HTML5-draggable (that path is dead in this webview; got ${JSON.stringify(draggables)})`);

  // ---- TC-D1 a real pointer drag downward reorders the list ----------------------------------
  const p1 = await centerOf('#sessions .session[data-id="s1"]');
  const p2 = await centerOf('#sessions .session[data-id="s2"]');
  check(!!p1 && !!p2, 'TC-D1 the first two rows are visible');
  const rowPitch = p2.y - p1.y;
  check(rowPitch > 0, `TC-D1 rows are laid out top-to-bottom (pitch ${rowPitch}px)`);

  // Drag s1 down PAST s2's midpoint but not past s3's -> s1 lands in slot 1: [s2, s1, s3].
  // Deliberately a few px beyond the pitch: travelling exactly one pitch lands the pointer exactly
  // ON s2's midpoint, and the landing-slot rule ("have I passed this item's midpoint?") is a strict
  // comparison, so that single pixel column is ambiguous by construction. Real drags don't land
  // pixel-exact; the test shouldn't depend on which way a tie breaks.
  await dragBy(p1, 0, rowPitch + 6);
  const ids1 = await rowIds();
  check(JSON.stringify(ids1) === JSON.stringify(['s2', 's1', 's3']),
    `TC-D1 dragging the top row down one slot reorders to [s2,s1,s3] (got ${JSON.stringify(ids1)})`);
  // The reorder must be PERSISTED, not just repainted — this is what survives a restart.
  const persisted1 = await js('window.__reorders');
  check(persisted1.length === 1 && JSON.stringify(persisted1[0]) === JSON.stringify(['s2', 's1', 's3']),
    `TC-D1 the new order was persisted to the backend once (got ${JSON.stringify(persisted1)})`);

  // ---- TC-D2 dragging back up restores it, proving the move is symmetric --------------------
  // Upward is the opposite branch of the shift computation (to < from), so it needs its own case.
  const p1b = await centerOf('#sessions .session[data-id="s1"]');
  await dragBy(p1b, 0, -(rowPitch + 6));
  const ids2 = await rowIds();
  check(JSON.stringify(ids2) === JSON.stringify(['s1', 's2', 's3']),
    `TC-D2 dragging it back up restores [s1,s2,s3] (got ${JSON.stringify(ids2)})`);

  // ---- TC-D3 mid-drag the UI must show where the card will land ------------------------------
  // The user-visible requirement: "either show a placeholder of the target slot or move other cards
  // dynamically". We do the latter, so assert the dragged card is lifted and following the pointer
  // AND that the displaced card has actually shifted — a gap the size of a row IS the placeholder.
  const p1c = await centerOf('#sessions .session[data-id="s1"]');
  let mid = null;
  await dragBy(p1c, 0, rowPitch + 6, {
    abandon: true,
    beforeUp: async () => {
      mid = await js(`(() => {
        const dragged = document.querySelector('#sessions .session[data-id="s1"]');
        const other = document.querySelector('#sessions .session[data-id="s2"]');
        const cs = getComputedStyle(dragged);
        return { hasClass: dragged.classList.contains('dragging'),
                 containerFlag: document.getElementById('sessions').classList.contains('reordering'),
                 draggedTransform: cs.transform,
                 // INLINE style for the displaced card, not computed: the slide is a CSS transition,
                 // so a computed read taken mid-transition reports an intermediate value (or 0 if it
                 // hasn't ticked yet). The inline value is the target the renderer committed to.
                 otherTransform: other.style.transform,
                 // pointer-events must be off on the lifted card or it eats its own pointermoves.
                 pointerEvents: cs.pointerEvents }; })()`);
    },
  });
  check(mid && mid.hasClass === true, 'TC-D3 the dragged card is marked .dragging while in flight');
  check(mid && mid.containerFlag === true,
    'TC-D3 the container is in .reordering (this is what enables the slide transition)');
  check(mid && mid.pointerEvents === 'none',
    `TC-D3 the lifted card ignores pointer events, so it can't swallow its own moves (got ${mid && mid.pointerEvents})`);
  // A non-identity transform on the dragged card = it moved with the cursor (the reported symptom
  // was "the card not movable").
  check(mid && /matrix/.test(mid.draggedTransform) && mid.draggedTransform !== 'none',
    `TC-D3 the dragged card FOLLOWS the pointer (transform applied; got ${mid && mid.draggedTransform})`);
  const dyM = mid && (mid.draggedTransform.match(/matrix\(([^)]+)\)/) || [])[1];
  const draggedShift = dyM ? Number(dyM.split(',')[5]) : 0;
  // Roughly the pointer's travel: the last mouseMove lands one step short of the full delta, so
  // allow a step's slack rather than demanding exactness.
  check(Math.abs(draggedShift - rowPitch) <= rowPitch / 4,
    `TC-D3 …by roughly the pointer's travel (translateY ${draggedShift}px, expected ~${rowPitch})`);
  // And the displaced neighbour slid UP to open the target slot below it.
  const oy = mid && (mid.otherTransform.match(/translateY\((-?[\d.]+)px\)/) || [])[1];
  const otherShift = oy ? Number(oy) : 0;
  check(otherShift < 0,
    `TC-D3 the displaced card MOVED OUT OF THE WAY, opening the target slot (translateY ${otherShift}px)`);
  check(Math.abs(Math.abs(otherShift) - rowPitch) <= 2,
    `TC-D3 …by exactly one slot, so the gap matches the dragged card (${otherShift}px vs pitch ${rowPitch}px)`);

  // ---- TC-D4 releasing the pointer outside any new slot / cancelling must not persist junk ---
  // Finish the abandoned drag above with a pointercancel and assert the strip is restored and
  // NOTHING was persisted. A half-applied reorder would be worse than none.
  const beforeCancelReorders = (await js('window.__reorders')).length;
  await js(`(() => { const el = document.querySelector('#sessions .session[data-id="s1"]');
     el.dispatchEvent(new PointerEvent('pointercancel', { bubbles: true, pointerId: 1 })); })()`);
  await sleep(200);
  const afterCancel = await js(`(() => {
    const el = document.querySelector('#sessions .session[data-id="s1"]');
    return { transform: el.style.transform, cls: el.classList.contains('dragging'),
             container: document.getElementById('sessions').classList.contains('reordering'),
             ids: Array.from(document.querySelectorAll('#sessions .session')).map((n) => n.dataset.id) }; })()`);
  check(afterCancel.cls === false && afterCancel.container === false,
    'TC-D4 a cancelled drag clears the drag classes');
  check(afterCancel.transform === '',
    `TC-D4 a cancelled drag clears the inline transform (got ${JSON.stringify(afterCancel.transform)})`);
  check(JSON.stringify(afterCancel.ids) === JSON.stringify(['s1', 's2', 's3']),
    `TC-D4 a cancelled drag leaves the order untouched (got ${JSON.stringify(afterCancel.ids)})`);
  check((await js('window.__reorders')).length === beforeCancelReorders,
    'TC-D4 a cancelled drag persists nothing');

  // ---- TC-D5 a plain click must still be a click, not a 0px drag -----------------------------
  // The threshold exists for exactly this: mount-on-click is the primary gesture and must survive.
  await js('window.__setLastActive.length = 0');
  const p3 = await centerOf('#sessions .session[data-id="s3"]');
  wc.sendInputEvent({ type: 'mouseDown', x: p3.x, y: p3.y, button: 'left', clickCount: 1 });
  wc.sendInputEvent({ type: 'mouseUp', x: p3.x, y: p3.y, button: 'left', clickCount: 1 });
  await sleep(300);
  check(JSON.stringify(await js('window.__setLastActive')) === JSON.stringify(['s3']),
    `TC-D5 a press-release with no movement still selects the project (got ${JSON.stringify(await js('window.__setLastActive'))})`);
  check(JSON.stringify(await rowIds()) === JSON.stringify(['s1', 's2', 's3']),
    'TC-D5 …and did not reorder anything');
  // And a click with a couple of pixels of hand jitter — which is what real clicks look like — must
  // ALSO stay a click: it must not lift the card or leave a stray transform behind. This is what
  // DRAG_THRESHOLD buys; without it every click on a project would flicker into a 2px drag.
  await js('window.__setLastActive.length = 0');
  const p2j = await centerOf('#sessions .session[data-id="s2"]');
  wc.sendInputEvent({ type: 'mouseDown', x: p2j.x, y: p2j.y, button: 'left', clickCount: 1 });
  await sleep(20);
  wc.sendInputEvent({ type: 'mouseMove', x: p2j.x + 1, y: p2j.y + 2 });   // under the 4px threshold
  await sleep(20);
  const jitter = await js(`(() => { const el = document.querySelector('#sessions .session[data-id="s2"]');
     return { lifted: el.classList.contains('dragging'), transform: el.style.transform,
              reordering: document.getElementById('sessions').classList.contains('reordering') }; })()`);
  check(jitter.lifted === false && jitter.reordering === false && jitter.transform === '',
    `TC-D5 a sub-threshold jitter does not start a drag (got ${JSON.stringify(jitter)})`);
  wc.sendInputEvent({ type: 'mouseUp', x: p2j.x + 1, y: p2j.y + 2, button: 'left', clickCount: 1 });
  await sleep(300);
  check(JSON.stringify(await js('window.__setLastActive')) === JSON.stringify(['s2']),
    `TC-D5 …and still selects the project (got ${JSON.stringify(await js('window.__setLastActive'))})`);
  check(JSON.stringify(await rowIds()) === JSON.stringify(['s1', 's2', 's3']),
    'TC-D5 …and still reorders nothing');

  // ---- TC-D6 a completed drag must NOT also select the row it landed on ----------------------
  // A real drag ends in a click on the dragged element, whose onclick calls mount(). Reordering is
  // not selecting; without the one-shot click swallow, every reorder would also switch project
  // (and on an unconnected project, spawn a backend).
  await js('window.__setLastActive.length = 0');
  const pd = await centerOf('#sessions .session[data-id="s1"]');
  await dragBy(pd, 0, rowPitch + 6);
  check(JSON.stringify(await rowIds()) === JSON.stringify(['s2', 's1', 's3']),
    'TC-D6 precondition: the drag reordered the list');
  check(JSON.stringify(await js('window.__setLastActive')) === JSON.stringify([]),
    `TC-D6 the drag did NOT also switch project (got ${JSON.stringify(await js('window.__setLastActive'))})`);

  // ---- TC-D10 dragging must not select the text on the cards it passes over ------------------
  // Reported after the reorder itself worked: "while moving, the text on other cards will be
  // selected". A press-and-move over text is *also* the native gesture for extending a selection, so
  // the drag has to suppress it. Two independent things are asserted, because either alone leaves the
  // bug reachable: the labels are unselectable AT ALL TIMES (a `.reordering`-only rule is applied too
  // late — the anchor is placed on pointerdown, before the 4px threshold has classified the gesture),
  // and the drag drops any anchor that press already set.
  const selState = () => js(`(() => { const s = window.getSelection();
    return { text: s ? s.toString() : '', type: s ? s.type : null, ranges: s ? s.rangeCount : 0,
             sessionsUS: getComputedStyle(document.getElementById('sessions')).userSelect,
             tabsUS: getComputedStyle(document.getElementById('tabs')).userSelect }; })()`);
  const us = await selState();
  check(us.sessionsUS === 'none',
    `TC-D10 the project list is not selectable (got user-select:${us.sessionsUS})`);
  check(us.tabsUS === 'none',
    `TC-D10 the tab strip is not selectable (got user-select:${us.tabsUS})`);
  // Start the press ON a text label (the left edge of the name, where a text caret would land) and
  // drag DOWN ACROSS the other rows — the exact gesture that produced the highlight.
  const labelPt = await js(`(() => { const n = document.querySelector('#sessions .session[data-id="s1"] .name');
     const r = n.getBoundingClientRect();
     return { x: Math.round(r.left + 4), y: Math.round(r.top + r.height / 2) }; })()`);
  check(!!labelPt, 'TC-D10 the first row has a visible text label to press on');
  const selDuring = [];
  await dragBy(labelPt, 30, rowPitch * 2 + 6, {
    steps: 10,
    onMove: async () => { selDuring.push(await selState()); },
  });
  const selected = selDuring.filter((s) => s.text.trim() !== '');
  check(selected.length === 0,
    `TC-D10 no text was selected at any point during the drag (got ${JSON.stringify(selected.map((s) => s.text))})`);
  const selAfter = await selState();
  check(selAfter.text.trim() === '',
    `TC-D10 …nor left selected after release (got ${JSON.stringify(selAfter.text)})`);
  // Guard the other half: the rename editors must still be selectable, or §23 would regress (you
  // could no longer select the text you're editing).
  const inputUS = await js(`(() => { const li = document.querySelector('#sessions .session');
     const i = document.createElement('input'); li.appendChild(i);
     const v = getComputedStyle(i).userSelect; i.remove(); return v; })()`);
  check(inputUS === 'text',
    `TC-D10 a rename editor inside a row IS still selectable (got user-select:${inputUS})`);
  // …and the converse: a selection the user made ELSEWHERE (terminal output, a file preview) is
  // theirs, and a reorder must not clear it. Caught for real — the first cut called a bare
  // removeAllRanges() and wiped exactly this.
  await js(`(() => {
    const d = document.createElement('div');
    d.id = 'probe-outside'; d.textContent = 'OUTPUT THE USER SELECTED';
    document.getElementById('term').appendChild(d);
    const r = document.createRange(); r.selectNodeContents(d);
    const s = window.getSelection(); s.removeAllRanges(); s.addRange(r); })()`);
  const outsideBefore = await js(`window.getSelection().toString()`);
  check(outsideBefore === 'OUTPUT THE USER SELECTED',
    `TC-D10 precondition: a selection exists outside the strips (got ${JSON.stringify(outsideBefore)})`);
  const p1s = await centerOf('#sessions .session:first-child');
  await dragBy(p1s, 0, rowPitch + 6, {});
  const outsideAfter = await js(`window.getSelection().toString()`);
  check(outsideAfter === 'OUTPUT THE USER SELECTED',
    `TC-D10 a reorder leaves a selection made elsewhere alone (got ${JSON.stringify(outsideAfter)})`);
  await js(`(() => { const n = document.getElementById('probe-outside'); if (n) n.remove();
    window.getSelection().removeAllRanges(); })()`);

  // These drags really did reorder (that's not what's under test here), so clear the recorders. TC-D9
  // below reads the order dynamically rather than assuming one.
  await js(`window.__reorders.length = 0; window.__setLastActive.length = 0;`);

  // ---- TC-D9 a row being RENAMED must not be draggable ---------------------------------------
  // The two gestures share the row. `li.onclick` already ignores clicks while renaming, and the drag
  // path has to agree: pressing the row's sub-line mid-rename used to lift and reorder it (the
  // `input, .controls, …` exemption only covers a press ON the editor, not elsewhere in the row).
  const nameEl = await centerOf('#sessions .session:first-child .name');
  for (const clickCount of [1, 2]) {   // real double-click, as TC-R does
    wc.sendInputEvent({ type: 'mouseDown', x: nameEl.x, y: nameEl.y, button: 'left', clickCount });
    wc.sendInputEvent({ type: 'mouseUp', x: nameEl.x, y: nameEl.y, button: 'left', clickCount });
    await sleep(40);
  }
  await sleep(300);
  check(await js(`!!document.querySelector('#sessions .session:first-child .name input')`) === true,
    'TC-D9 precondition: a rename editor is open on the first row');
  const orderBeforeRename = await rowIds();
  const subPt = await js(`(() => { const s = document.querySelector('#sessions .session:first-child .sub');
     if (!s) return null; const r = s.getBoundingClientRect();
     return { x: Math.round(r.left + 10), y: Math.round(r.top + r.height / 2) }; })()`);
  check(!!subPt, 'TC-D9 the row has a visible sub-line to press');
  let renameMid = null;
  await dragBy(subPt, 0, rowPitch + 6, {
    abandon: true,
    beforeUp: async () => {
      renameMid = await js(`({ lifted: !!document.querySelector('#sessions .session.dragging'),
        editing: !!document.querySelector('#sessions .session .name input') })`);
    },
  });
  check(renameMid && renameMid.lifted === false,
    'TC-D9 dragging elsewhere in a row that is being renamed does NOT lift it');
  check(renameMid && renameMid.editing === true, 'TC-D9 …and the editor is still open');
  wc.sendInputEvent({ type: 'mouseUp', x: subPt.x, y: subPt.y + rowPitch + 6, button: 'left', clickCount: 1 });
  await sleep(350);
  check(JSON.stringify(await rowIds()) === JSON.stringify(orderBeforeRename),
    `TC-D9 …and nothing was reordered (got ${JSON.stringify(await rowIds())})`);
  // Close the editor so it can't interfere with the tab cases below.
  wc.sendInputEvent({ type: 'keyDown', keyCode: 'Escape' });
  wc.sendInputEvent({ type: 'keyUp', keyCode: 'Escape' });
  await sleep(250);

  // ---- TC-D7 the tab strip reorders horizontally ---------------------------------------------
  // Same mechanism, other axis — and the axis maths (clientX, translateX) is separate code.
  const proj = await js(`document.querySelector('#sessions .session.active').dataset.id`);
  const fire = (ev) => js(`window.__fire('window', Object.assign({ id: ${JSON.stringify(proj)} }, ${JSON.stringify(ev)}))`);
  await fire({ action: 'add', window: '@0', order: ['@0'] });
  await fire({ action: 'rename', window: '@0', name: 'shell' });
  await fire({ action: 'add', window: '@1', order: ['@0', '@1'] });
  await fire({ action: 'rename', window: '@1', name: 'logs' });
  await fire({ action: 'add', window: '@2', order: ['@0', '@1', '@2'] });
  await fire({ action: 'rename', window: '@2', name: 'build' });
  await fire({ action: 'active', window: '@0', order: ['@0', '@1', '@2'] });
  await sleep(350);
  const labels0 = await tabLabels();
  check(JSON.stringify(labels0) === JSON.stringify(['shell', 'logs', 'build']),
    `TC-D7 three tmux windows became three tabs in order (got ${JSON.stringify(labels0)})`);
  const t0 = await centerOf('#tabs .tab:not(.plus):nth-of-type(1)');
  const t1 = await centerOf('#tabs .tab:not(.plus):nth-of-type(2)');
  const tabPitch = t1.x - t0.x;
  check(tabPitch > 0, `TC-D7 tabs are laid out left-to-right (pitch ${tabPitch}px)`);
  await dragBy(t0, tabPitch + 6, 0);
  const labels1 = await tabLabels();
  check(JSON.stringify(labels1) === JSON.stringify(['logs', 'shell', 'build']),
    `TC-D7 dragging the first tab right one slot reorders to [logs,shell,build] (got ${JSON.stringify(labels1)})`);
  const tabPrefs = await js('window.__tabPrefs');
  check(tabPrefs.length >= 1 && JSON.stringify(tabPrefs[tabPrefs.length - 1][1]) === JSON.stringify(['@1', '@0', '@2']),
    `TC-D7 the new tab order was persisted (got ${JSON.stringify(tabPrefs[tabPrefs.length - 1])})`);
  // Dragging a tab must not also switch to it (same click-swallow rule as TC-D6).
  check(JSON.stringify(await js('window.__tabSelects')) === JSON.stringify([]),
    `TC-D7 the tab drag did not also select the tab (got ${JSON.stringify(await js('window.__tabSelects'))})`);

  // ---- TC-D8 the trailing "+" is not part of the reorderable set -----------------------------
  // It isn't a tab: it must neither be displaced by a drag nor be a landing slot. If it were in the
  // item list, dropping past the last tab would compute an index the tab order can't represent.
  const plusBefore = await js(`document.querySelector('#tabs .tab.plus').getBoundingClientRect().x`);
  const tLast = await centerOf('#tabs .tab:not(.plus):nth-of-type(3)');
  let plusMid = null;
  await dragBy(tLast, tabPitch + 6, 0, {
    abandon: true,
    beforeUp: async () => {
      plusMid = await js(`(() => { const p = document.querySelector('#tabs .tab.plus');
         return { transform: getComputedStyle(p).transform }; })()`);
    },
  });
  check(plusMid && (plusMid.transform === 'none' || /matrix\(1, 0, 0, 1, 0, 0\)/.test(plusMid.transform)),
    `TC-D8 the "+" button does not shift during a tab drag (got ${plusMid && plusMid.transform})`);
  // Release past the end: the last tab is already last, so nothing changes and nothing is persisted.
  const prefsBefore = (await js('window.__tabPrefs')).length;
  wc.sendInputEvent({ type: 'mouseUp', x: tLast.x + tabPitch + 6, y: tLast.y, button: 'left', clickCount: 1 });
  await sleep(350);
  check(JSON.stringify(await tabLabels()) === JSON.stringify(['logs', 'shell', 'build']),
    `TC-D8 dragging the last tab further right changes nothing (got ${JSON.stringify(await tabLabels())})`);
  check((await js('window.__tabPrefs')).length === prefsBefore,
    'TC-D8 …and persists nothing (no no-op write)');
  const plusAfter = await js(`document.querySelector('#tabs .tab.plus').getBoundingClientRect().x`);
  check(Math.abs(plusAfter - plusBefore) < 1,
    `TC-D8 the "+" stayed put (${plusBefore} -> ${plusAfter})`);

  cleanup();
  console.log(failures ? `\n${failures} check(s) FAILED` : '\nall ok');
  app.exit(failures ? 1 : 0);
});
