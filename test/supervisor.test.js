'use strict';
const { test } = require('node:test');
const assert = require('node:assert');
const { Supervisor, States } = require('../src/main/supervisor');
const { FakeBackend } = require('../src/main/backends/fakeBackend');

// Deterministic fake clock: manually advance time to fire timers.
function makeClock() {
  let now = 0;
  let seq = 0;
  const timers = new Map();
  return {
    now: () => now,
    setTimeout: (fn, ms) => { const id = ++seq; timers.set(id, { at: now + ms, fn }); return id; },
    clearTimeout: (id) => { timers.delete(id); },
    advance(ms) {
      const target = now + ms;
      // fire due timers in time order until we reach target
      let guard = 0;
      while (true) {
        let next = null;
        for (const [id, t] of timers) if (t.at <= target && (next === null || t.at < next.at)) next = { id, ...t };
        if (!next) break;
        now = next.at;
        timers.delete(next.id);
        next.fn();
        if (++guard > 10000) throw new Error('timer loop');
      }
      now = target;
    },
  };
}

function setup(script, opts) {
  const clock = makeClock();
  let backend;
  const backends = [];
  const sup = new Supervisor({
    makeBackend: () => { backend = new FakeBackend(script); backends.push(backend); return backend; },
    clock,
    opts,
  });
  const states = [];
  sup.on('state', (s) => states.push(s));
  return { sup, clock, states, backends, current: () => backend };
}

// TC-S1
test('TC-S1 spawn -> connecting -> connected on timeout confirm', () => {
  const { sup, clock, states } = setup({}, { connectTimeoutMs: 3000 });
  sup.start({ cols: 80, rows: 24 });
  assert.equal(sup.state, States.CONNECTING);
  clock.advance(3000);
  assert.equal(sup.state, States.CONNECTED);
  assert.deepEqual(states, [States.CONNECTING, States.CONNECTED]);
});

// TC-S2 clean exit 0 => closed, no respawn
test('TC-S2 clean exit 0 => closed, no respawn', () => {
  const { sup, clock, backends, current } = setup({}, { connectTimeoutMs: 3000 });
  sup.start();
  clock.advance(3000);                 // connected
  current().forceExit(0);          // user detach
  assert.equal(sup.state, States.CLOSED);
  clock.advance(60000);                // give any (wrong) respawn time to fire
  assert.equal(backends.length, 1, 'must NOT respawn on clean exit');
});

// TC-S3 non-zero exit => reconnecting => respawn after backoff
test('TC-S3 non-zero exit => reconnecting => respawn', () => {
  const { sup, clock, backends, current } = setup({}, { connectTimeoutMs: 3000, backoffBaseMs: 1000 });
  sup.start();
  clock.advance(3000);                 // connected
  current().forceExit(1);
  assert.equal(sup.state, States.RECONNECTING);
  assert.equal(backends.length, 1);
  clock.advance(1000);                 // first backoff
  assert.equal(backends.length, 2, 'respawned');
  assert.equal(sup.state, States.CONNECTING);
});

// TC-S4 exponential capped backoff
test('TC-S4 backoff exponential and capped', () => {
  const { sup, clock, backends, current } = setup({}, { connectTimeoutMs: 999999, backoffBaseMs: 1000, backoffMaxMs: 30000, lifetimeAttemptCap: 100 });
  sup.start();                         // connecting (won't auto-connect: timeout huge)
  const delays = [];
  // Force a sequence of failures and measure the gap until respawn each time.
  let prevCount = backends.length;
  for (let i = 0; i < 8; i++) {
    current().forceExit(1);
    // find the delay by advancing in small steps until a respawn happens
    let waited = 0;
    while (backends.length === prevCount && waited < 60000) { clock.advance(250); waited += 250; }
    delays.push(waited);
    prevCount = backends.length;
  }
  // 1000,2000,4000,8000,16000,30000(cap),30000,30000
  assert.deepEqual(delays.slice(0, 6).map((d) => Math.round(d / 250) * 250),
    [1000, 2000, 4000, 8000, 16000, 30000]);
  assert.ok(delays[6] <= 30000 && delays[7] <= 30000, 'capped at 30s');
});

// TC-S5 dead host: bounded attempts, no hot-loop
test('TC-S5 dead host -> dead after cap, no hot-loop', () => {
  const { sup, clock, backends, current } = setup({}, { connectTimeoutMs: 999999, backoffBaseMs: 1, backoffMaxMs: 4, lifetimeAttemptCap: 5 });
  sup.start();                          // connecting, backend #1
  // Simulate a dead host: every attempt fails immediately with a non-zero code.
  for (let i = 0; i < 20 && sup.state !== States.DEAD; i++) {
    current().forceExit(1);             // -> reconnecting (or DEAD past cap)
    clock.advance(4);                   // fire capped backoff -> respawn
  }
  assert.equal(sup.state, States.DEAD);
  // spawns = initial + at most `cap` respawns; strictly bounded (no infinite hot-loop).
  assert.ok(backends.length <= 6, `bounded spawns, got ${backends.length}`);
});

// TC-S6 intentional close cancels pending backoff
test('TC-S6 close cancels pending respawn', () => {
  const { sup, clock, backends, current } = setup({}, { connectTimeoutMs: 999999, backoffBaseMs: 5000 });
  sup.start();
  current().forceExit(1);          // schedule respawn in 5s
  assert.equal(sup.state, States.RECONNECTING);
  sup.close();                         // user closes during backoff
  assert.equal(sup.state, States.CLOSED);
  clock.advance(60000);
  assert.equal(backends.length, 1, 'no respawn after close');
});

// TC-S7 exit-0 during in-flight respawn is NOT treated as intentional
test('TC-S7 exit-0 gated on no-respawn-in-flight', () => {
  const { sup, clock, backends, current } = setup({}, { connectTimeoutMs: 999999, backoffBaseMs: 1000 });
  sup.start();
  current().forceExit(1);          // -> reconnecting, respawnInFlight=true
  clock.advance(1000);                 // respawn (backend #2), still CONNECTING, inFlight
  assert.equal(backends.length, 2);
  current().forceExit(0);          // a -D-detached prior client style exit-0 while in flight
  assert.notEqual(sup.state, States.CLOSED, 'must not treat as intentional while in flight');
});

// TC-S8 connecting timeout => optimistic connected
test('TC-S8 connecting timeout => optimistic connected', () => {
  const { sup, clock } = setup({}, { connectTimeoutMs: 2500 });
  sup.start();
  clock.advance(2499);
  assert.equal(sup.state, States.CONNECTING);
  clock.advance(1);
  assert.equal(sup.state, States.CONNECTED);
});

// TC-S9 retry floor from dead
test('TC-S9 retry respects floor', () => {
  const { sup, clock, backends, current } = setup({}, { connectTimeoutMs: 999999, backoffBaseMs: 1, backoffMaxMs: 1, lifetimeAttemptCap: 1, retryFloorMs: 30000 });
  sup.start();
  current().forceExit(1);              // attempt 1 fails -> reconnecting
  clock.advance(1);                    // respawn (attempt used)
  current().forceExit(1);              // exceeds cap(1) -> DEAD
  assert.equal(sup.state, States.DEAD);
  const countAtDead = backends.length;
  // First manual retry is allowed (floor is BETWEEN clicks, not before the first).
  assert.equal(sup.retry(), true, 'first retry allowed');
  assert.ok(backends.length > countAtDead, 'retry respawned');
  current().forceExit(1);              // fails again -> DEAD
  clock.advance(1);
  current().forceExit(1);
  assert.equal(sup.state, States.DEAD);
  const countAtDead2 = backends.length;
  // A second retry immediately after is rejected by the floor.
  assert.equal(sup.retry(), false, 'rapid re-click rejected (< 30s floor)');
  assert.equal(backends.length, countAtDead2, 'no respawn while floored');
  clock.advance(30000);
  assert.equal(sup.retry(), true, 'retry allowed after floor elapses');
});

// TC-S11 respawn tears down the OLD backend (no lingering listeners → no doubled output)
test('TC-S11 old backend killed + unbound on respawn', () => {
  const { sup, clock, backends, current } = setup({}, { connectTimeoutMs: 999999, backoffBaseMs: 1000 });
  sup.start();
  const first = current();
  sup.on('data', () => {});           // ensure supervisor forwards
  first.forceExit(1);                 // -> reconnecting
  clock.advance(1000);                // respawn -> backend #2
  assert.equal(backends.length, 2);
  assert.ok(first.killed, 'old backend was killed');
  // old backend emitting after teardown must NOT reach the supervisor
  let leaked = 0;
  sup.on('data', () => { leaked++; });
  first.emit('data', 'ghost');        // listeners were removed → no forward
  assert.equal(leaked, 0, 'no output leaks from the torn-down backend');
});

// TC-S10 all states reachable/exitable (smoke over the reachable set)
test('TC-S10 states reachable', () => {
  const seen = new Set();
  const { sup, clock, current } = setup({}, { connectTimeoutMs: 1000, backoffBaseMs: 500, backoffMaxMs: 500, lifetimeAttemptCap: 2 });
  sup.on('state', (s) => seen.add(s));
  sup.start();                         // connecting
  clock.advance(1000);                 // connected
  current().forceExit(1);              // reconnecting (attempt 1)
  clock.advance(500);                  // connecting again
  current().forceExit(1);              // reconnecting (attempt 2)
  clock.advance(500);                  // connecting again
  current().forceExit(1);              // attempt 3 > cap(2) -> dead
  assert.ok(seen.has(States.CONNECTING));
  assert.ok(seen.has(States.CONNECTED));
  assert.ok(seen.has(States.RECONNECTING));
  assert.ok(seen.has(States.DEAD));
});
