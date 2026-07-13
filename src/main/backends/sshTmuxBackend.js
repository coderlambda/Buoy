'use strict';
// Remote backend: plain ssh + tmux. Same ConnectionBackend contract as the others.
// This is the ZERO-INSTALL transport — needs only ssh locally and tmux on the remote
// (both already present on a typical Amazon dev-dsk). Durability model:
//   - tmux (server-side) keeps the session alive across disconnects/app restarts
//   - the supervisor respawns `ssh` on non-zero exit and reattaches the SAME tmux session
// There is no live-connection resume (mosh/et's extra); the user explicitly only needs
// "reattach my tmux session when the network resumes or the app restarts", which this does.
// Verified end-to-end against a real Amazon Linux 2 host (tmux 1.8).
const { EventEmitter } = require('events');
const { buildSshArgs } = require('../../shared/validation');
const { socketName } = require('../../shared/tmuxSocket');
const { spawnEnv } = require('../env');

// Non-interactive, resilient ssh defaults. BatchMode avoids password hangs; keepalives
// detect dead links; connect timeout bounds the "connecting" state.
const DEFAULT_SSH_OPTS = [
  '-o', 'ConnectTimeout=8',
  '-o', 'ServerAliveInterval=15',
  '-o', 'ServerAliveCountMax=3',
];

class SshTmuxBackend extends EventEmitter {
  // tmuxPath: remote tmux binary (resolved absolute path from the probe; §probeTmux).
  //   Defaults to '.local/bin/tmux' (relative to remote $HOME) when no probe ran.
  // socket: isolated tmux socket. Defaults to a MAJOR-VERSION-TAGGED name so a stale
  //   wrong-version server (e.g. an old system tmux) can never squat our socket and cause
  //   "protocol version mismatch" — a different tmux version simply uses a different socket.
  constructor({ host, session, baseArgs, tmuxPath, socket, tmuxVersion }) {
    super();
    const path = tmuxPath || '.local/bin/tmux';
    this.built = buildSshArgs({
      host, session,
      baseArgs: [...DEFAULT_SSH_OPTS, ...(baseArgs || [])],
      tmuxPath: path,
      socket: socket || socketName('plain', tmuxVersion),
    });
    this.pty = null;
  }

  spawn({ cols = 80, rows = 24 } = {}) {
    const nodePty = require('@homebridge/node-pty-prebuilt-multiarch');
    this.pty = nodePty.spawn('ssh', this.built.args, {
      name: 'xterm-256color', cols, rows, env: spawnEnv(),
    });
    this.pty.onData((d) => this.emit('data', d));
    this.pty.onExit(({ exitCode }) => this.emit('exit', exitCode));
  }

  write(data) { if (this.pty) this.pty.write(data); }
  resize(cols, rows) { if (this.pty) this.pty.resize(cols, rows); }
  kill() { if (this.pty) { try { this.pty.kill(); } catch (_) {} } }
}

module.exports = { SshTmuxBackend };
