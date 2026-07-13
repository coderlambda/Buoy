'use strict';
// Remote backend: et + tmux (DESIGN.md §5.1). Same ConnectionBackend contract.
// Spawns node-pty running the canonical, validated `et` argv. End-to-end behavior
// (exit-0-on-detach, OOB channel) is gated on Milestone-0 live verification — see TEST_PLAN
// TC-M0. This class is deliberately thin: all safety is in validation.buildEtArgs().
const { EventEmitter } = require('events');
const { buildEtArgs } = require('../../shared/validation');
const { spawnEnv } = require('../env');

class EtTmuxBackend extends EventEmitter {
  constructor({ host, session, id, remoteUser, baseArgs }) {
    super();
    // Throws ValidationError on any bad field — fail before spawning anything.
    this.built = buildEtArgs({ host, session, id, remoteUser, baseArgs });
    this.pty = null;
  }

  spawn({ cols = 80, rows = 24 } = {}) {
    const nodePty = require('@homebridge/node-pty-prebuilt-multiarch');
    this.pty = nodePty.spawn('et', this.built.args, {
      name: 'xterm-256color', cols, rows, env: spawnEnv(),
    });
    this.pty.onData((d) => this.emit('data', d));
    this.pty.onExit(({ exitCode }) => this.emit('exit', exitCode));
  }

  write(data) { if (this.pty) this.pty.write(data); }
  resize(cols, rows) { if (this.pty) this.pty.resize(cols, rows); }
  kill() { if (this.pty) { try { this.pty.kill(); } catch (_) {} } }
}

module.exports = { EtTmuxBackend };
