'use strict';
// Local connection backend: a real node-pty shell on this machine. Same ConnectionBackend
// contract as FakeBackend / (future) EtTmuxBackend, so the app runs and the pty path is
// exercised without a remote host. Also the MVP's "local session" feature.
const { EventEmitter } = require('events');

class LocalBackend extends EventEmitter {
  constructor({ shell, args = [], cwd, env } = {}) {
    super();
    this.shell = shell || process.env.SHELL || '/bin/bash';
    this.args = args;
    this.cwd = cwd || process.env.HOME;
    this.env = env || process.env;
    this.pty = null;
  }

  spawn({ cols = 80, rows = 24 } = {}) {
    // Lazy require so unit tests that don't touch the local backend don't need the native module.
    const nodePty = require('@homebridge/node-pty-prebuilt-multiarch');
    this.pty = nodePty.spawn(this.shell, this.args, {
      name: 'xterm-256color',
      cols, rows, cwd: this.cwd, env: this.env,
    });
    this.pty.onData((d) => this.emit('data', d));
    this.pty.onExit(({ exitCode }) => this.emit('exit', exitCode));
  }

  write(data) { if (this.pty) this.pty.write(data); }
  resize(cols, rows) { if (this.pty) this.pty.resize(cols, rows); }
  kill() { if (this.pty) { try { this.pty.kill(); } catch (_) {} } }
}

module.exports = { LocalBackend };
