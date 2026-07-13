'use strict';
// Session supervisor: the reliability core (DESIGN.md §5.1, §5.3 state machine).
// Owns one connection backend and the reconnect policy. Clock/timers are injectable so
// the whole state machine is testable deterministically (TEST_PLAN TC-S).
const { EventEmitter } = require('events');

const States = Object.freeze({
  IDLE: 'idle',
  CONNECTING: 'connecting',
  CONNECTED: 'connected',
  RECONNECTING: 'reconnecting',
  THROTTLED: 'throttled',   // reserved for RSS watchdog (deferred past MVP)
  DEAD: 'dead',
  CLOSED: 'closed',
});

const DEFAULTS = {
  backoffBaseMs: 1000,
  backoffMaxMs: 30000,
  lifetimeAttemptCap: 8,     // total reconnect attempts before DEAD (ssh-lockout guard)
  connectTimeoutMs: 3000,    // MVP: optimistic connect fallback (deferred OOB channel, §5.3)
  retryFloorMs: 30000,       // min gap between user-initiated retries from DEAD
};

// Injectable clock: real by default, fake in tests.
const realClock = {
  now: () => Date.now(),
  setTimeout: (fn, ms) => setTimeout(fn, ms),
  clearTimeout: (t) => clearTimeout(t),
};

class Supervisor extends EventEmitter {
  // makeBackend: () => ConnectionBackend (fresh each spawn). opts overrides DEFAULTS.
  constructor({ makeBackend, clock = realClock, opts = {} } = {}) {
    super();
    this.makeBackend = makeBackend;
    this.clock = clock;
    this.opts = { ...DEFAULTS, ...opts };
    this.state = States.IDLE;
    this.attempts = 0;              // reconnect attempts since last success
    this.backend = null;
    this._backoffTimer = null;
    this._connectTimer = null;
    this._respawnInFlight = false;  // gates the exit-0 rule (§5.1 / TC-S7)
    this._intentionalClose = false;
    this._lastRetryAt = -Infinity;
    this.cols = 80;
    this.rows = 24;
  }

  _set(state) {
    if (this.state !== state) {
      this.state = state;
      this.emit('state', state);
    }
  }

  start({ cols = 80, rows = 24 } = {}) {
    this.cols = cols; this.rows = rows;
    this._intentionalClose = false;
    this.attempts = 0;
    this._spawn();
  }

  _spawn() {
    this._clearConnectTimer();
    // Tear down any previous backend FIRST. Otherwise its ssh/pty stays alive and its
    // 'data' listeners keep firing this.emit('data') — so after a reconnect the OLD and NEW
    // clients both stream output, doubling every echoed keystroke (the "typed letters
    // double" bug in control mode, where two tmux control clients were attached at once).
    this._teardownBackend();
    this._set(States.CONNECTING);
    const backend = this.makeBackend();
    this.backend = backend;

    backend.on('data', (d) => {
      // Normalize to a uniform shape so downstream layers never type-sniff: plain backends emit a
      // raw string; control mode emits { window, pane, data }. Wrap the string here.
      // NOTE: connect confirmation must NOT key off first data (that's the et banner, §5.4).
      this.emit('data', typeof d === 'string' ? { data: d } : d);
    });
    backend.on('exit', (code) => this._onExit(code));
    // Control-mode backends emit richer events (windows→tabs); forward them.
    backend.on('window', (w) => this.emit('window', w));
    backend.on('control', (c) => this.emit('control', c));
    backend.on('ready', () => this.emit('ready'));   // control mode: input-ready after attach settle

    backend.spawn({ cols: this.cols, rows: this.rows });

    // MVP connect confirmation: optimistic timeout (DESIGN §5.3 fallback for deferred OOB).
    this._connectTimer = this.clock.setTimeout(() => {
      if (this.state === States.CONNECTING) {
        this._respawnInFlight = false;
        this.attempts = 0;                 // a stable connection resets the attempt budget
        this._set(States.CONNECTED);
      }
    }, this.opts.connectTimeoutMs);
  }

  _onExit(code) {
    this._clearConnectTimer();

    // Rule (§5.1): clean exit 0 = user left on purpose => no respawn.
    // BUT gate on "no respawn in flight": a -D-detached prior client exiting 0 during a
    // supervisor-triggered reconnect must NOT be read as user intent (TC-S7).
    if (code === 0 && !this._respawnInFlight && !this._intentionalClose) {
      this.emit('intentional-exit');       // observable (§5.1 / §11)
      this._set(States.CLOSED);
      return;
    }
    if (this._intentionalClose) {
      this._set(States.CLOSED);
      return;
    }

    // Non-zero (or in-flight) exit => reconnect candidate.
    this.attempts += 1;
    if (this.attempts > this.opts.lifetimeAttemptCap) {
      this._set(States.DEAD);              // stop; no hot-loop, no auth storm (TC-S5)
      return;
    }
    this._set(States.RECONNECTING);
    const delay = Math.min(
      this.opts.backoffBaseMs * Math.pow(2, this.attempts - 1),
      this.opts.backoffMaxMs,
    );
    this._respawnInFlight = true;
    this._backoffTimer = this.clock.setTimeout(() => {
      this._backoffTimer = null;
      if (this._intentionalClose) return;  // TC-S6: close cancels pending respawn
      this._spawn();
    }, delay);
  }

  // User-initiated retry from DEAD, respecting a floor between attempts (TC-S9).
  retry() {
    if (this.state !== States.DEAD) return false;
    const now = this.clock.now();
    if (now - this._lastRetryAt < this.opts.retryFloorMs) return false;
    this._lastRetryAt = now;
    this.attempts = 0;
    this._spawn();
    return true;
  }

  write(data) { if (this.backend) this.backend.write(data); }

  resize(cols, rows) {
    this.cols = cols; this.rows = rows;
    if (this.backend) this.backend.resize(cols, rows);
  }

  // Intentional close/kill: suppress respawn AND cancel pending backoff (TC-S6).
  close() {
    this._intentionalClose = true;
    this._clearBackoff();
    this._clearConnectTimer();
    this._teardownBackend();
    this._set(States.CLOSED);
  }

  // Kill and fully detach the current backend so it can't keep emitting after replacement.
  _teardownBackend() {
    if (this.backend) {
      try { this.backend.removeAllListeners(); } catch (_) {}
      try { this.backend.kill(); } catch (_) {}
      this.backend = null;
    }
  }

  _clearBackoff() {
    if (this._backoffTimer) { this.clock.clearTimeout(this._backoffTimer); this._backoffTimer = null; }
  }
  _clearConnectTimer() {
    if (this._connectTimer) { this.clock.clearTimeout(this._connectTimer); this._connectTimer = null; }
  }
}

module.exports = { Supervisor, States, DEFAULTS, realClock };
