'use strict';
const { test } = require('node:test');
const assert = require('node:assert');
const { parseVersion, gte, MIN_MODERN, CANDIDATES } = require('../src/main/probeTmux');

// TC-PR1 version parsing
test('TC-PR1 parseVersion handles tmux -V output', () => {
  assert.deepEqual(parseVersion('tmux 3.5a'), [3, 5]);
  assert.deepEqual(parseVersion('tmux 1.8'), [1, 8]);
  assert.deepEqual(parseVersion('tmux next-3.4'), [3, 4]);
  assert.equal(parseVersion('garbage'), null);
  assert.equal(parseVersion(''), null);
  assert.equal(parseVersion(undefined), null);
});

// TC-PR2 version comparison
test('TC-PR2 gte compares [maj,min] correctly', () => {
  assert.ok(gte([3, 5], MIN_MODERN));   // 3.5 >= 3.2
  assert.ok(gte([3, 2], MIN_MODERN));   // equal
  assert.ok(!gte([3, 1], MIN_MODERN));  // 3.1 < 3.2
  assert.ok(!gte([1, 8], MIN_MODERN));  // old
  assert.ok(gte([4, 0], [3, 2]));
});

// TC-PR3 candidate order prefers the user-local newer path first
test('TC-PR3 candidate order', () => {
  assert.equal(CANDIDATES[0], '$HOME/.local/bin/tmux', 'user-local newer tmux tried first');
  assert.ok(CANDIDATES.includes('/usr/bin/tmux'), 'system tmux as fallback');
});

// TC-PR4 selection logic (mirror of probeTmux's chooser) — modern preferred, else highest
test('TC-PR4 chooser prefers >=3.2 highest, else highest available', () => {
  const choose = (found) => {
    const modern = found.filter((f) => gte(f.version, MIN_MODERN));
    const pool = modern.length ? modern : found;
    pool.sort((a, b) => (b.version[0] - a.version[0]) || (b.version[1] - a.version[1]));
    return pool[0];
  };
  // 3.5a beats 1.8
  assert.equal(choose([{ tmuxPath: 'tmux', version: [1, 8] }, { tmuxPath: '.local/bin/tmux', version: [3, 5] }]).tmuxPath, '.local/bin/tmux');
  // only old tmux -> still pick it (fallback)
  assert.equal(choose([{ tmuxPath: 'tmux', version: [1, 8] }]).tmuxPath, 'tmux');
  // two modern -> highest
  assert.deepEqual(choose([{ tmuxPath: 'a', version: [3, 2] }, { tmuxPath: 'b', version: [3, 5] }]).version, [3, 5]);
});
