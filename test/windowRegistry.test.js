'use strict';
const { test } = require('node:test');
const assert = require('node:assert');
const { WindowRegistry } = require('../src/main/windowRegistry');

const row = (win, pane, paneActive, winActive, name) => ({ win, pane, paneActive, winActive, name });

// TC-WR1 first reconcile adds all windows and maps panes
test('TC-WR1 initial reconcile adds windows and maps panes', () => {
  const r = new WindowRegistry();
  const d = r.reconcile([row('@0', '%0', true, false, 'vim'), row('@1', '%1', true, true, 'zsh')]);
  assert.deepEqual(d.added, ['@0', '@1']);
  assert.deepEqual(d.removed, []);
  assert.equal(d.active, '@1');
  assert.ok(d.activeChanged);
  assert.equal(r.winForPane('%0'), '@0');
  assert.equal(r.winForPane('%1'), '@1');
  assert.deepEqual(d.newlyMappedPanes.sort(), ['%0', '%1']);
});

// TC-WR2 idempotent: replaying identical rows yields an empty diff
test('TC-WR2 identical reconcile is a no-op diff', () => {
  const r = new WindowRegistry();
  const rows = [row('@0', '%0', true, true, 'zsh')];
  r.reconcile(rows);
  const d = r.reconcile(rows);
  assert.deepEqual(d.added, []);
  assert.deepEqual(d.removed, []);
  assert.deepEqual(d.renamed, []);
  assert.equal(d.activeChanged, false);
  assert.deepEqual(d.newlyMappedPanes, []);
});

// TC-WR3 adding a window (new tab) is diffed as added + active change, only the new pane mapped
test('TC-WR3 new window diffed as add + active + newly-mapped pane', () => {
  const r = new WindowRegistry();
  r.reconcile([row('@0', '%0', true, true, 'zsh')]);
  const d = r.reconcile([row('@0', '%0', true, false, 'zsh'), row('@1', '%1', true, true, 'zsh')]);
  assert.deepEqual(d.added, ['@1']);
  assert.equal(d.active, '@1');
  assert.ok(d.activeChanged);
  assert.deepEqual(d.newlyMappedPanes, ['%1'], 'only the new pane is newly mapped');
});

// TC-WR4 closing a window is diffed as removed; active falls back sanely
test('TC-WR4 closing a window removes it and re-picks active', () => {
  const r = new WindowRegistry();
  r.reconcile([row('@0', '%0', true, false, 'zsh'), row('@1', '%1', true, true, 'zsh')]);
  const d = r.reconcile([row('@0', '%0', true, true, 'zsh')]);
  assert.deepEqual(d.removed, ['@1']);
  assert.equal(r.winForPane('%1'), null, 'closed window pane unmapped');
  assert.equal(r.activeWindow, '@0');
});

// TC-WR5 rename diffed as renamed only
test('TC-WR5 rename diffed as renamed', () => {
  const r = new WindowRegistry();
  r.reconcile([row('@0', '%0', true, true, 'zsh')]);
  const d = r.reconcile([row('@0', '%0', true, true, 'node build')]);
  assert.deepEqual(d.renamed, [{ win: '@0', name: 'node build' }]);
  assert.deepEqual(d.added, []);
});

// TC-WR6 a split window (two panes, same window) maps both panes to the window
test('TC-WR6 multiple panes in one window both map to it', () => {
  const r = new WindowRegistry();
  r.reconcile([row('@0', '%0', true, true, 'zsh'), row('@0', '%1', false, true, 'zsh')]);
  assert.equal(r.winForPane('%0'), '@0');
  assert.equal(r.winForPane('%1'), '@0');
  assert.equal(r.windows.get('@0').activePane, '%0', 'active pane tracked');
});
