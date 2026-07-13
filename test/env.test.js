'use strict';
const { test } = require('node:test');
const assert = require('node:assert');
const { augmentedPath, spawnEnv, EXTRA_PATHS } = require('../src/main/env');

// TC-E1 augmented PATH includes common install dirs
test('TC-E1 augmentedPath adds Homebrew/MacPorts/local paths', () => {
  const p = augmentedPath().split(':');
  assert.ok(p.includes('/opt/homebrew/bin'));
  assert.ok(p.includes('/usr/local/bin'));
});

// TC-E2 does not duplicate paths already present
test('TC-E2 no duplicate entries', () => {
  const p = augmentedPath().split(':');
  const seen = new Set();
  for (const e of p) { assert.ok(!seen.has(e), `duplicate ${e}`); seen.add(e); }
});

// TC-E3 spawnEnv preserves base env and overrides PATH
test('TC-E3 spawnEnv keeps base env, augments PATH', () => {
  const env = spawnEnv({ FOO: 'bar', PATH: '/usr/bin' });
  assert.equal(env.FOO, 'bar');
  assert.ok(env.PATH.includes('/usr/bin'));
  assert.ok(env.PATH.includes('/opt/homebrew/bin'));
});
