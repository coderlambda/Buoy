'use strict';
const { test } = require('node:test');
const assert = require('node:assert');
const { ReplyChannel } = require('../src/main/replyChannel');

function harness() {
  const writes = [];
  const rc = new ReplyChannel((line) => writes.push(line));
  return { rc, writes };
}

// TC-RC1 send writes the command line (newline-terminated) and enqueues a handler
test('TC-RC1 send writes the command with a newline', () => {
  const { rc, writes } = harness();
  rc.send('list-panes');
  assert.deepEqual(writes, ['list-panes\n']);
  assert.equal(rc.pending, 1);
});

// TC-RC2 replies dispatch to handlers in submission order (positional correlation)
test('TC-RC2 replies dispatch FIFO in submission order', () => {
  const { rc } = harness();
  const got = [];
  rc.send('a', (ev) => got.push('a:' + ev.body));
  rc.send('b', (ev) => got.push('b:' + ev.body));
  rc.onReply({ body: '1' });
  rc.onReply({ body: '2' });
  assert.deepEqual(got, ['a:1', 'b:2']);
});

// TC-RC3 start() seeds a handshake handler that absorbs the first unsolicited reply, keeping
// later commands aligned with their own replies
test('TC-RC3 start() absorbs the unsolicited handshake block', () => {
  const { rc } = harness();
  rc.start();
  let handled = null;
  rc.send('real', (ev) => { handled = ev.body; });
  rc.onReply({ body: 'HANDSHAKE' });   // consumed by the seed, NOT by 'real'
  assert.equal(handled, null);
  rc.onReply({ body: 'REALREPLY' });   // now 'real' gets its reply
  assert.equal(handled, 'REALREPLY');
});

// TC-RC4 start() is idempotent (double-start doesn't seed twice)
test('TC-RC4 start() is idempotent', () => {
  const { rc } = harness();
  rc.start(); rc.start();
  assert.equal(rc.pending, 1);
});

// TC-RC5 a fire-and-forget send (no handler) still consumes its reply slot, so a following
// command with a handler stays aligned
test('TC-RC5 handler-less send still consumes its reply slot', () => {
  const { rc } = harness();
  let got = null;
  rc.send('fire-and-forget');            // no handler
  rc.send('query', (ev) => { got = ev.body; });
  rc.onReply({ body: 'ack' });           // belongs to fire-and-forget
  rc.onReply({ body: 'result' });        // belongs to query
  assert.equal(got, 'result');
});

// TC-RC6 an unexpected extra reply (empty queue) returns false so the caller can surface it
test('TC-RC6 extra reply with empty queue returns false', () => {
  const { rc } = harness();
  assert.equal(rc.onReply({ body: 'x' }), false);
});
