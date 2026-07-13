'use strict';
const { test } = require('node:test');
const assert = require('node:assert');
const { Backpressure } = require('../src/shared/backpressure');

function make() {
  const events = [];
  const bp = new Backpressure({
    high: 1000, low: 100,
    onPause: () => events.push('pause'),
    onResume: () => events.push('resume'),
  });
  return { bp, events };
}

// TC-B1
test('TC-B1 stays flowing below HIGH', () => {
  const { bp, events } = make();
  bp.onData(500);
  bp.onData(400);
  assert.equal(bp.paused, false);
  assert.deepEqual(events, []);
});

// TC-B2
test('TC-B2 crosses HIGH emits pause and tracks unacked', () => {
  const { bp, events } = make();
  bp.onData(600);
  bp.onData(500); // 1100 >= 1000
  assert.equal(bp.paused, true);
  assert.equal(bp.unacked, 1100);
  assert.deepEqual(events, ['pause']);
});

// TC-B3
test('TC-B3 ack drains below LOW emits resume', () => {
  const { bp, events } = make();
  bp.onData(1100);          // pause
  bp.ack(1050);             // unacked 50 <= 100 low
  assert.equal(bp.paused, false);
  assert.deepEqual(events, ['pause', 'resume']);
});

// TC-B4 no flapping
test('TC-B4 no flapping between LOW and HIGH', () => {
  const { bp, events } = make();
  bp.onData(1100);          // pause (unacked 1100)
  bp.ack(600);              // unacked 500: between low(100) and high(1000) -> still paused
  assert.equal(bp.paused, true);
  bp.onData(200);           // 700 still paused, no new pause event
  assert.deepEqual(events, ['pause']);
  bp.ack(650);              // unacked 50 -> resume
  assert.deepEqual(events, ['pause', 'resume']);
});

// TC-B5 never drops data (all bytes accounted)
test('TC-B5 never drops: unacked accounting is exact', () => {
  const { bp } = make();
  let sent = 0, acked = 0;
  for (let i = 0; i < 100; i++) { bp.onData(37); sent += 37; }
  for (let i = 0; i < 100; i++) { bp.ack(37); acked += 37; }
  assert.equal(sent, acked);
  assert.equal(bp.unacked, 0);
  assert.equal(bp.paused, false);
});

// LOW must be < HIGH
test('TC-B6 rejects LOW >= HIGH', () => {
  assert.throws(() => new Backpressure({ high: 100, low: 100 }));
});
