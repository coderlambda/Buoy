'use strict';
// Probe a remote host (once, at session create) to choose the best tmux binary.
// Uses a NON-INTERACTIVE ssh command (no pty) so it does NOT trigger login-shell auto-tmux.
// Result is persisted on the session so reconnects skip the probe.
//
// Preference order: highest version that is >= 3.2 (control-mode / modern -A capable),
// else any tmux found (even old 1.8, which still reattaches by name on an isolated socket).
const { execFile } = require('child_process');
const { spawnEnv } = require('./env');
const { parseHost } = require('../shared/validation');

// Candidate remote tmux paths, best-first, as absolute $HOME-based / absolute paths.
// IMPORTANT lessons from a real Amazon dev-dsk (verified):
//  - the host's login shell is zsh, and it interprets the ssh remote command → a plain
//    `sh`-style script hit `zsh: parse error`. So we EXPLICITLY invoke `/bin/sh -c` on the
//    remote (see the ssh args below) to get a predictable POSIX interpreter.
//  - `$HOME` IS set in that /bin/sh context; use it (quoted) rather than ~ which doesn't
//    expand inside quotes.
// The chosen tmuxPath is returned as an EXPANDED absolute path (via pwd-free `$HOME` echo)
// so the backend can pass it to ssh as a literal token that the remote /bin/sh re-expands.
const CANDIDATES = ['$HOME/.local/bin/tmux', '/usr/local/bin/tmux', '/usr/bin/tmux'];
const MIN_MODERN = [3, 2]; // control-mode-friendly floor

function parseVersion(s) {
  // Handles "tmux 3.5a", "tmux 1.8", and dev builds like "tmux next-3.4".
  const m = /tmux\s+(?:next-)?(\d+)\.(\d+)/.exec(s || '');
  return m ? [Number(m[1]), Number(m[2])] : null;
}
function gte(a, b) { return a[0] !== b[0] ? a[0] > b[0] : a[1] >= b[1]; }

// Returns { tmuxPath, version:[maj,min]|null } — never rejects; falls back to 'tmux'.
function probeTmux(rawHost, { baseArgs = [], timeoutMs = 12000 } = {}) {
  const { user, host, port } = parseHost(rawHost);   // also validates (throws on bad input)
  const target = (user != null ? `${user}@` : '') + host;

  // Build a POSIX probe script, then send it BASE64-ENCODED and decode+run under /bin/sh on
  // the remote. This is the only robust approach: ssh concatenates the remote command into a
  // single string that the user's LOGIN SHELL (zsh on Amazon dev-dsk) parses FIRST — so any
  // raw quoting/`$(...)` gets mangled before /bin/sh sees it (verified: it failed every
  // which way). A base64 blob has no shell metacharacters, so nothing can mangle it.
  const script = CANDIDATES.map((c) =>
    `test -x "${c}" && printf '%s\\t%s\\n' "${c}" "$("${c}" -V 2>/dev/null)"`
  ).join('; ');
  const b64 = Buffer.from(script, 'utf8').toString('base64');
  // `echo <b64> | base64 -d | /bin/sh` — echo/base64/sh are universally available; the only
  // token the login shell sees is the inert base64 string.
  const remote = `echo ${b64} | base64 -d | /bin/sh`;
  const sshArgs = [];
  if (port != null) sshArgs.push('-p', String(port));
  sshArgs.push('-o', 'BatchMode=yes', '-o', 'ConnectTimeout=8', ...baseArgs, '--', target, remote);

  return new Promise((resolve) => {
    execFile('ssh', sshArgs, { env: spawnEnv(), timeout: timeoutMs }, (err, stdout) => {
      const found = [];
      for (const line of String(stdout || '').split('\n')) {
        const [path, ...rest] = line.split('\t');
        const ver = parseVersion(rest.join('\t'));
        if (path && ver) found.push({ tmuxPath: path.trim(), version: ver });
      }
      if (!found.length) {
        // Probe failed (unreachable/auth) or no tmux — default to bare 'tmux' and let the
        // connection surface the real error. Do NOT block session creation on the probe.
        return resolve({ tmuxPath: 'tmux', version: null, probed: false });
      }
      // prefer a modern (>=3.2) one, highest version; else the highest available.
      const modern = found.filter((f) => gte(f.version, MIN_MODERN));
      const pool = modern.length ? modern : found;
      pool.sort((a, b) => (b.version[0] - a.version[0]) || (b.version[1] - a.version[1]));
      resolve({ ...pool[0], probed: true });
    });
  });
}

module.exports = { probeTmux, parseVersion, gte, CANDIDATES, MIN_MODERN };
