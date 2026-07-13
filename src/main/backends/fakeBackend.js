'use strict';
// Fake connection backend for deterministic testing of the supervisor (DESIGN.md §11).
// Implements the ConnectionBackend contract:
//   spawn() -> starts; emits 'data'(str), 'exit'(code); has write/resize/kill.
// Tests drive exit codes/timing directly, with no real process/network.
const { EventEmitter } = require('events');

class FakeBackend extends EventEmitter {
  // script: optional { exitAfterMs, exitCode, dataOnAttach } to simulate behavior.
  constructor(script = {}) {
    super();
    this.script = script;
    this.spawned = 0;
    this.killed = false;
    this.lastResize = null;
    this.writes = [];
    this._timers = [];
  }

  spawn({ cols, rows } = {}) {
    this.spawned += 1;
    this.killed = false;
    this.lastResize = { cols, rows };
    const s = this.script;
    if (s.dataOnAttach != null) {
      queueMicrotask(() => this.emit('data', s.dataOnAttach));
    }
    if (s.exitAfterMs != null) {
      const t = setTimeout(() => {
        if (!this.killed) this.emit('exit', s.exitCode == null ? 1 : s.exitCode);
      }, s.exitAfterMs);
      this._timers.push(t);
    }
  }

  // Test helper: force an exit now.
  forceExit(code) {
    if (!this.killed) this.emit('exit', code);
  }

  write(data) { this.writes.push(data); }
  resize(cols, rows) { this.lastResize = { cols, rows }; }
  kill() {
    this.killed = true;
    this._timers.forEach(clearTimeout);
    this._timers = [];
  }
}

module.exports = { FakeBackend };
