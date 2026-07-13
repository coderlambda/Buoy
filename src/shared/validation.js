'use strict';
// Input validation + safe argv construction for the `et`/`tmux` launch command.
// This is the security core (DESIGN.md §6.1). Two injection surfaces are closed here:
//   (a) remote shell injection via the tmux session name (it lands in the `-c` shell string)
//   (b) argv flag-injection via host/user/port (they are et positional/option args)
// Every value the renderer supplies is validated HERE, in the main process, before any
// argv is built. The renderer never passes raw argv.

// Session: first char alphanumeric (rejects leading `-` flag-injection), then [A-Za-z0-9_-].
// `.` excluded (collides with tmux target syntax session:window.pane). No shell metachars.
const SESSION_RE = /^[A-Za-z0-9][A-Za-z0-9_-]*$/;
// User: same shape, but `.` allowed (user.name is common); still no `/` or metachars.
const USER_RE = /^[A-Za-z0-9][A-Za-z0-9._-]*$/;
// Hostname / IPv4: leading alphanumeric, dots/hyphens allowed.
const HOST_RE = /^[A-Za-z0-9][A-Za-z0-9.-]*$/;
// IPv6 (bare): hex groups and colons, must contain a colon, no leading `-`.
const IPV6_RE = /^[0-9A-Fa-f:]+$/;

const MAX_LEN = 64;

function validateSession(s) {
  if (typeof s !== 'string' || s.length === 0 || s.length > MAX_LEN) {
    throw new ValidationError('session', s, 'empty or too long');
  }
  if (!SESSION_RE.test(s)) {
    throw new ValidationError('session', s,
      'must start alphanumeric and contain only [A-Za-z0-9_-] (no dot, no leading dash, no shell metacharacters)');
  }
  return s;
}

// Parse and validate a connection target of the form [user@]host[:port], with IPv6 support.
// Returns { user|null, host, port|null, isIPv6 } with each sub-field validated.
function parseHost(input) {
  if (typeof input !== 'string' || input.length === 0 || input.length > 255) {
    throw new ValidationError('host', input, 'empty or too long');
  }
  let rest = input;
  let user = null;

  const at = rest.indexOf('@');
  if (at !== -1) {
    user = rest.slice(0, at);
    rest = rest.slice(at + 1);
    if (!USER_RE.test(user)) {
      throw new ValidationError('user', user, 'invalid username (no leading dash / slash / metacharacters)');
    }
  }

  let host, port = null, isIPv6 = false;

  if (rest.startsWith('[')) {
    // Bracketed IPv6: [addr] or [addr]:port
    const close = rest.indexOf(']');
    if (close === -1) throw new ValidationError('host', input, 'unterminated IPv6 bracket');
    host = rest.slice(1, close);
    isIPv6 = true;
    const tail = rest.slice(close + 1);
    if (tail.startsWith(':')) port = parsePort(tail.slice(1));
    else if (tail.length) throw new ValidationError('host', input, 'garbage after IPv6 bracket');
  } else if ((rest.match(/:/g) || []).length >= 2) {
    // Bare IPv6 (2+ colons) — no port possible in bare form; route port via -p separately.
    host = rest;
    isIPv6 = true;
  } else {
    // hostname / IPv4, optional :port
    const colon = rest.indexOf(':');
    if (colon !== -1) {
      port = parsePort(rest.slice(colon + 1));
      host = rest.slice(0, colon);
    } else {
      host = rest;
    }
  }

  if (isIPv6) {
    if (!IPV6_RE.test(host)) throw new ValidationError('host', input, 'invalid IPv6 address');
  } else {
    if (!HOST_RE.test(host)) {
      throw new ValidationError('host', input, 'invalid host (no leading dash / metacharacters)');
    }
  }
  return { user, host, port, isIPv6 };
}

function parsePort(p) {
  if (!/^[0-9]{1,5}$/.test(p)) throw new ValidationError('port', p, 'not numeric');
  const n = Number(p);
  if (n < 1 || n > 65535) throw new ValidationError('port', p, 'out of range 1..65535');
  return n;
}

// Build the et argv for a session. Throws if anything fails validation — never returns a
// partial/unsafe argv. DESIGN.md §5.1 canonical command:
//   et [baseArgs] -t sock:sock -c '<payload>' -- <host>
// with -c BEFORE -- (verified: et drops -c placed after --), host the sole token after --.
function buildEtArgs({ host: rawHost, session: rawSession, id, remoteUser, baseArgs = [] }) {
  const session = validateSession(rawSession);
  const { user, host, port } = parseHost(rawHost);
  if (typeof id !== 'string' || !/^[A-Za-z0-9_-]+$/.test(id)) {
    throw new ValidationError('id', id, 'invalid session id');
  }
  // Socket dir is namespaced per remote user. remoteUser defaults to the parsed user.
  const ru = remoteUser != null ? remoteUser : (user != null ? user : 'default');
  if (!USER_RE.test(ru)) throw new ValidationError('remoteUser', ru, 'invalid');

  const dir = `/tmp/et-${ru}`;
  const sock = `${dir}/et-${id}.sock`;

  // Payload: create the socket dir atomically & safely (mkdir -m 700, no -p, then verify
  // it's our own real dir — not a foreign/symlinked squat, §6.1), then start/attach tmux
  // and pin window-size. `\;` is a tmux command separator (survives the shell, verified).
  const payload =
    `{ mkdir -m 700 ${dir} 2>/dev/null || [ -O ${dir} -a ! -L ${dir} ]; } && ` +
    `tmux -S ${sock} new-session -A -D -s ${session} \\; set-option window-size latest`;

  const args = [...baseArgs, '-t', `${sock}:${sock}`];
  if (user != null) args.push('-u', user);
  if (port != null) args.push('-p', String(port));
  args.push('-c', payload);   // -c MUST precede --
  args.push('--', host);      // host is the sole positional after --
  return { args, sock, dir, host, session };
}

// Build the mosh argv for a session. mosh's model differs from et's (verified against
// mosh docs; exact form gated on Milestone-0 against a real mosh install, TEST_PLAN TC-M0):
//   - the remote command is the TRAILING args after the host: `mosh [opts] -- <host> <cmd…>`
//   - so `-- <host>` both prevents leading-`-` host flag-injection AND precedes the command
//   - SSH port is passed via `--ssh="ssh -p <port>"`, NOT a positional :port
//   - user is `user@host` (mosh has no -u); we keep it in the host token, validated separately
//   - NO socket forwarding (no OOB channel) → no -S/mkdir dance; tmux uses its default socket
// The remote command is run via `sh -c '<payload>'` so the tmux `\;` separator behaves the
// same as the et path. `<session>` is charset-validated, so the payload is not injectable.
function buildMoshArgs({ host: rawHost, session: rawSession, baseArgs = [] }) {
  const session = validateSession(rawSession);
  const { user, host, port, isIPv6 } = parseHost(rawHost);

  const payload =
    `tmux new-session -A -D -s ${session} \\; set-option window-size latest`;

  const args = [...baseArgs];
  if (port != null) args.push(`--ssh=ssh -p ${port}`);   // ssh port, not mosh's UDP port
  args.push('--');
  // mosh takes [user@]host as one positional; rebuild it from validated sub-fields.
  const hostToken = (user != null ? `${user}@` : '') + host;
  args.push(hostToken);
  // trailing remote command
  args.push('sh', '-c', payload);
  return { args, host, session, isIPv6 };
}

// Build the ssh argv for a session. The simplest, most-portable transport: no daemon, no
// extra install — just `ssh -tt` running tmux on the remote. Durability comes entirely from
// tmux (server-side) + the supervisor respawning ssh on exit; there is NO live-connection
// resume (that's mosh/et's extra), which the user explicitly does not need.
//   ssh -tt [-p <port>] [baseArgs] -- <user@host> tmux new-session -A -s <name>
//   - -tt forces a pty (needed for tmux)
//   - `--` ends ssh options; <user@host> is the sole target token (flag-injection safe)
//   - CRITICAL: run tmux as the DIRECT ssh command — NOT via `sh -lc`. Many hosts
//     (e.g. Amazon dev-dsk) auto-start tmux from the interactive login shell's rc files;
//     wrapping in a login shell makes that auto-start fire FIRST, creating a numbered
//     session and swallowing our `-s <name>` → duplicate sessions on every launch that
//     never reattach. Running tmux directly bypasses the interactive rc entirely.
//     (Verified against a live Amazon Linux 2 host: direct form reattaches the SAME
//     named session across launches; the `sh -lc` form did not.)
//   - `-A` = attach if the named session exists, else create (reattach, no duplicate).
//   - ISOLATED SOCKET (`-L <socket>`): the app runs tmux on its OWN socket, separate from
//     the host's default tmux (and its auto-tmux server). This is CRITICAL on hosts where:
//       (a) an old system tmux server is running — a newer client on the default socket
//           gets "protocol version mismatch"; a private socket avoids the clash entirely.
//       (b) the login shell auto-starts tmux — our socket is untouched by that.
//     Verified against a live Amazon Linux 2 host: `-L dtapp` + a self-built tmux 3.5a in
//     ~/.local/bin reattaches the SAME named session across launches, no dupes, no clash.
//   - tmuxPath lets us point at a newer tmux (e.g. ~/.local/bin/tmux) when the system one
//     is too old (Amazon Linux 2 ships 1.8, which mis-handles `-A`/`-s`). Default 'tmux'.
//   - tmux argv is passed as SEPARATE tokens (not a shell string); <session> is validated.
function buildSshArgs({ host: rawHost, session: rawSession, baseArgs = [],
                        tmuxPath = 'tmux', socket = 'dtapp' } = {}) {
  const session = validateSession(rawSession);
  const { user, host, port } = parseHost(rawHost);
  if (!/^[A-Za-z0-9._/-]+$/.test(tmuxPath)) throw new ValidationError('tmuxPath', tmuxPath, 'invalid path');
  if (!/^[A-Za-z0-9_-]+$/.test(socket)) throw new ValidationError('socket', socket, 'invalid socket name');

  const args = ['-tt'];
  if (port != null) args.push('-p', String(port));
  args.push(...baseArgs, '--');
  args.push((user != null ? `${user}@` : '') + host);
  // tmux invoked directly (bypasses login-shell auto-tmux), on an isolated socket:
  args.push(tmuxPath, '-L', socket, 'new-session', '-A', '-s', session);
  return { args, host, session };
}

// Build ssh argv to KILL a remote tmux session: `tmux -L <socket> kill-session -t <session>`.
// The remote command is base64-encoded and run under /bin/sh (the host login shell may be
// zsh, which mangles raw quoting — verified). All fields validated; throws on bad input.
function buildKillArgs({ host: rawHost, session: rawSession, tmuxPath = 'tmux', socket = 'dtapp', baseArgs = [] }) {
  const session = validateSession(rawSession);
  const { user, host, port } = parseHost(rawHost);
  if (!/^[A-Za-z0-9._/$-]+$/.test(tmuxPath)) throw new ValidationError('tmuxPath', tmuxPath, 'invalid path');
  if (!/^[A-Za-z0-9_-]+$/.test(socket)) throw new ValidationError('socket', socket, 'invalid socket name');
  const target = (user != null ? `${user}@` : '') + host;
  const script = `${tmuxPath} -L ${socket} kill-session -t ${session}`;
  const b64 = Buffer.from(script, 'utf8').toString('base64');
  const remote = `echo ${b64} | base64 -d | /bin/sh`;
  const args = [];
  if (port != null) args.push('-p', String(port));
  args.push('-o', 'BatchMode=yes', '-o', 'ConnectTimeout=8', ...baseArgs, '--', target, remote);
  return { args, script, socket };
}

class ValidationError extends Error {
  constructor(field, value, why) {
    super(`invalid ${field}: ${why}`);
    this.name = 'ValidationError';
    this.field = field;
    this.value = value;
  }
}

module.exports = { validateSession, parseHost, buildEtArgs, buildMoshArgs, buildSshArgs, buildKillArgs, ValidationError, SESSION_RE };
