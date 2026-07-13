'use strict';
// Remote backend: mosh + tmux. Same ConnectionBackend contract as et/local/fake.
// Rationale (DESIGN §3 pluggable transport): mosh has no persistent daemon and no
// privileged listening port — launched per-connection over SSH, roams networks (UDP SSP),
// and tmux owns persistence + scrollback (neutralizing mosh's no-scrollback weakness).
// Easier auto-onboarding than et; the tradeoff is UDP-hostile firewalls (fall back to et/ssh).
//
// End-to-end behavior (exit code on detach, UDP reachability) is gated on Milestone-0 live
// verification — see TEST_PLAN TC-M0. All input safety is in validation.buildMoshArgs().
const { EventEmitter } = require('events');
const { buildMoshArgs } = require('../../shared/validation');
const { spawnEnv } = require('../env');

class MoshTmuxBackend extends EventEmitter {
  constructor({ host, session, baseArgs }) {
    super();
    // Throws ValidationError on any bad field — fail before spawning anything.
    this.built = buildMoshArgs({ host, session, baseArgs });
    this.pty = null;
  }

  spawn({ cols = 80, rows = 24 } = {}) {
    const nodePty = require('@homebridge/node-pty-prebuilt-multiarch');
    this.pty = nodePty.spawn('mosh', this.built.args, {
      name: 'xterm-256color', cols, rows, env: spawnEnv(),
    });
    this.pty.onData((d) => this.emit('data', d));
    this.pty.onExit(({ exitCode }) => this.emit('exit', exitCode));
  }

  write(data) { if (this.pty) this.pty.write(data); }
  resize(cols, rows) { if (this.pty) this.pty.resize(cols, rows); }
  kill() { if (this.pty) { try { this.pty.kill(); } catch (_) {} } }
}

module.exports = { MoshTmuxBackend };
