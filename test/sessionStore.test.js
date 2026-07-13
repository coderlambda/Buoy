'use strict';
const { test } = require('node:test');
const assert = require('node:assert');
const fs = require('fs');
const os = require('os');
const path = require('path');
const { SessionStore } = require('../src/main/sessionStore');

function tmpFile() {
  return path.join(fs.mkdtempSync(path.join(os.tmpdir(), 'sesstore-')), 'sessions.json');
}

// TC-P1
test('TC-P1 round-trip save/load', () => {
  const fp = tmpFile();
  const store = new SessionStore(fp);
  const sessions = [
    { id: '1', host: 'me@a.com:22', session: 'dev', title: 'Dev', order: 0 },
    { id: '2', host: 'b.com', session: 'web_1', title: 'Web', order: 1 },
  ];
  store.save(sessions);
  const loaded = store.load();
  assert.equal(loaded.length, 2);
  assert.equal(loaded[0].session, 'dev');
  assert.equal(loaded[1].host, 'b.com');
});

// TC-P2 re-validate on load
test('TC-P2 invalid entries dropped on load', () => {
  const fp = tmpFile();
  fs.writeFileSync(fp, JSON.stringify([
    { id: '1', host: 'good.com', session: 'ok' },
    { id: '2', host: '-x', session: 'evil' },          // bad host
    { id: '3', host: 'good.com', session: 'a;b' },      // bad session
    { id: '4', host: 'good.com', session: 'valid' },
  ]));
  const loaded = new SessionStore(fp).load();
  const sessions = loaded.map((s) => s.session);
  assert.deepEqual(sessions.sort(), ['ok', 'valid']);
});

// TC-P4 transport persisted and whitelisted
test('TC-P4 transport round-trips and is whitelisted', () => {
  const fp = tmpFile();
  const store = new SessionStore(fp);
  store.save([
    { id: '1', host: 'a.com', session: 'dev', transport: 'mosh' },
    { id: '2', host: 'b.com', session: 'ops', transport: 'et' },
    { id: '3', host: 'c.com', session: 'x', transport: 'bogus' }, // -> defaults to ssh
    { id: '4', host: 'd.com', session: 'y', transport: 'ssh' },
  ]);
  const loaded = store.load();
  const byId = Object.fromEntries(loaded.map((s) => [s.id, s.transport]));
  assert.equal(byId['1'], 'mosh');
  assert.equal(byId['2'], 'et');
  assert.equal(byId['3'], 'ssh', 'unknown transport defaults to ssh');
  assert.equal(byId['4'], 'ssh');
});

// TC-P5 rename mutates title only; tmux session name (reattach key) is preserved
test('TC-P5 rename changes title, not session name', () => {
  const fp = tmpFile();
  const store = new SessionStore(fp);
  store.save([{ id: '1', host: 'a.com', session: 'dt-abc', transport: 'ssh', title: 'a.com', order: 0 }]);
  // replicate the main.js session:rename mutation
  const list = store.load();
  const entry = list.find((x) => x.id === '1');
  entry.title = 'My Prod Box';
  store.save(list);
  const reloaded = store.load();
  assert.equal(reloaded[0].title, 'My Prod Box', 'title updated');
  assert.equal(reloaded[0].session, 'dt-abc', 'tmux session name UNCHANGED (reattach key)');
});

// TC-P6 tmuxPath persists and is validated on load
test('TC-P6 tmuxPath round-trips; unsafe path dropped', () => {
  const fp = tmpFile();
  const store = new SessionStore(fp);
  store.save([
    { id: '1', host: 'a.com', session: 'dt-a', transport: 'ssh', tmuxPath: '.local/bin/tmux' },
    { id: '2', host: 'b.com', session: 'dt-b', transport: 'ssh', tmuxPath: 'tmux; rm -rf ~' }, // unsafe -> dropped
  ]);
  const loaded = store.load();
  const byId = Object.fromEntries(loaded.map((s) => [s.id, s.tmuxPath]));
  assert.equal(byId['1'], '.local/bin/tmux');
  assert.equal(byId['2'], null, 'unsafe tmuxPath dropped -> will re-probe');
});

// TC-P3 corrupt / missing
test('TC-P3 corrupt or missing file => empty, no throw', () => {
  const fp = tmpFile();
  assert.deepEqual(new SessionStore(fp).load(), []); // missing
  fs.writeFileSync(fp, '{ not json');
  assert.deepEqual(new SessionStore(fp).load(), []); // corrupt
  fs.writeFileSync(fp, '"a string not array"');
  assert.deepEqual(new SessionStore(fp).load(), []);
});
