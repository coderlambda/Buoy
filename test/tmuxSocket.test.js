'use strict';
const { test } = require('node:test');
const assert = require('node:assert');
const { socketName } = require('../src/shared/tmuxSocket');

// TC-TS1 control vs plain use distinct prefixes, tagged by major-minor
test('TC-TS1 control/plain prefixes tagged by major-minor', () => {
  assert.equal(socketName('control', [3, 7]), 'dtcc3-7');
  assert.equal(socketName('plain', [3, 7]), 'dtapp3-7');
});

// TC-TS2 different minor versions get different sockets (the upgrade-safety invariant)
test('TC-TS2 minor version bump changes the socket', () => {
  assert.notEqual(socketName('control', [3, 5]), socketName('control', [3, 7]));
});

// TC-TS3 unknown version -> empty tag (best-effort, matches pre-versioning behavior)
test('TC-TS3 unknown version yields an untagged socket', () => {
  assert.equal(socketName('control', null), 'dtcc');
  assert.equal(socketName('plain', undefined), 'dtapp');
});
