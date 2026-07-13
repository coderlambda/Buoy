'use strict';
const { test } = require('node:test');
const assert = require('node:assert');
const { validateSession, parseHost, buildEtArgs, buildMoshArgs, buildSshArgs, buildKillArgs, ValidationError } = require('../src/shared/validation');

// TC-V1
test('TC-V1 valid session names accepted', () => {
  for (const s of ['dev', 'web_1', 'a-b', 'S3', 'x']) {
    assert.equal(validateSession(s), s);
  }
});

// TC-V2 leading-dash flag injection
test('TC-V2 reject leading dash', () => {
  for (const s of ['-X', '-D', '-', '-rf']) {
    assert.throws(() => validateSession(s), ValidationError, `should reject ${s}`);
  }
});

// TC-V3 shell metacharacters
test('TC-V3 reject shell metacharacters', () => {
  for (const s of ['a;b', 'a b', '$(id)', 'a`b`', 'a|b', 'a&b', 'a>b', 'a\\b', "a'b", 'a"b']) {
    assert.throws(() => validateSession(s), ValidationError, `should reject ${s}`);
  }
});

// TC-V4 dot (tmux target collision)
test('TC-V4 reject dot in session', () => {
  for (const s of ['a.b', '.hidden', 'sess.win']) {
    assert.throws(() => validateSession(s), ValidationError);
  }
});

// TC-V5 empty / too long
test('TC-V5 reject empty and too-long', () => {
  assert.throws(() => validateSession(''), ValidationError);
  assert.throws(() => validateSession('a'.repeat(65)), ValidationError);
});

// TC-V6 host accept
test('TC-V6 host: accept common forms', () => {
  assert.deepEqual(parseHost('example.com'), { user: null, host: 'example.com', port: null, isIPv6: false });
  assert.deepEqual(parseHost('10.0.0.1'), { user: null, host: '10.0.0.1', port: null, isIPv6: false });
  assert.deepEqual(parseHost('user@host'), { user: 'user', host: 'host', port: null, isIPv6: false });
  assert.deepEqual(parseHost('host:2022'), { user: null, host: 'host', port: 2022, isIPv6: false });
  assert.deepEqual(parseHost('me@host:22'), { user: 'me', host: 'host', port: 22, isIPv6: false });
});

// TC-V7 host leading-dash
test('TC-V7 host: reject leading dash in host and user', () => {
  assert.throws(() => parseHost('-x'), ValidationError);
  assert.throws(() => parseHost('-x@host'), ValidationError);
  assert.throws(() => parseHost('user@-x'), ValidationError);
  assert.throws(() => parseHost('--kill-other-sessions'), ValidationError);
});

// TC-V8 port range
test('TC-V8 host: port range', () => {
  assert.throws(() => parseHost('h:0'), ValidationError);
  assert.throws(() => parseHost('h:99999'), ValidationError);
  assert.equal(parseHost('h:1').port, 1);
  assert.equal(parseHost('h:65535').port, 65535);
});

// TC-V9 IPv6
test('TC-V9 IPv6 accepted and normalized', () => {
  assert.deepEqual(parseHost('::1'), { user: null, host: '::1', port: null, isIPv6: true });
  assert.deepEqual(parseHost('2001:db8::1'), { user: null, host: '2001:db8::1', port: null, isIPv6: true });
  const b = parseHost('[::1]:2022');
  assert.equal(b.host, '::1');       // stripped to bare
  assert.equal(b.port, 2022);        // routed to port (-> -p)
  assert.equal(b.isIPv6, true);
});

// TC-V10 buildEtArgs ordering + payload
test('TC-V10 buildEtArgs: correct argv order and payload', () => {
  const { args, sock } = buildEtArgs({ host: 'me@server:2022', session: 'dev', id: 'abc123', remoteUser: 'me' });
  const dashDash = args.indexOf('--');
  const cIdx = args.indexOf('-c');
  assert.ok(cIdx !== -1 && dashDash !== -1, 'has -c and --');
  assert.ok(cIdx < dashDash, '-c MUST precede --');
  assert.equal(args[dashDash + 1], 'server', 'host is sole token after --');
  assert.equal(args.length, dashDash + 2, 'nothing after host');
  assert.ok(args.includes('-t'), 'has socket forward');
  assert.ok(args.includes('-u') && args[args.indexOf('-u') + 1] === 'me');
  assert.ok(args.includes('-p') && args[args.indexOf('-p') + 1] === '2022');
  const payload = args[cIdx + 1];
  assert.match(payload, /mkdir -m 700/);
  assert.match(payload, /new-session -A -D -s dev/);
  assert.match(payload, /set-option window-size latest/);
  assert.match(sock, /^\/tmp\/et-me\/et-abc123\.sock$/);
});

// TC-V11 refuse unsafe build
test('TC-V11 buildEtArgs refuses invalid fields', () => {
  assert.throws(() => buildEtArgs({ host: 'good', session: 'a;b', id: 'x' }), ValidationError);
  assert.throws(() => buildEtArgs({ host: '-x', session: 'dev', id: 'x' }), ValidationError);
  assert.throws(() => buildEtArgs({ host: 'good', session: 'dev', id: 'bad id!' }), ValidationError);
});

// TC-V12 buildMoshArgs: host+command ordering and payload
test('TC-V12 buildMoshArgs: host after --, command trails, safe payload', () => {
  const { args, host } = buildMoshArgs({ host: 'me@server:2022', session: 'dev' });
  const dd = args.indexOf('--');
  assert.ok(dd !== -1, 'has --');
  assert.equal(args[dd + 1], 'me@server', 'user@host is the sole positional after --');
  // trailing remote command
  assert.equal(args[dd + 2], 'sh');
  assert.equal(args[dd + 3], '-c');
  const payload = args[dd + 4];
  assert.match(payload, /new-session -A -D -s dev/);
  assert.match(payload, /set-option window-size latest/);
  // ssh port routed via --ssh, not a positional :port
  assert.ok(args.some((a) => a === '--ssh=ssh -p 2022'), 'ssh port via --ssh');
  assert.equal(host, 'server');
  // NO socket-forward / mkdir (mosh has no OOB channel)
  assert.ok(!args.includes('-t'), 'no -t socket forward');
  assert.ok(!/mkdir/.test(payload), 'no mkdir dance');
});

// TC-V13 buildMoshArgs closes the same injection surfaces as et
test('TC-V13 buildMoshArgs refuses invalid fields', () => {
  assert.throws(() => buildMoshArgs({ host: 'good', session: 'a;b' }), ValidationError);
  assert.throws(() => buildMoshArgs({ host: 'good', session: '-X' }), ValidationError);
  assert.throws(() => buildMoshArgs({ host: '-x', session: 'dev' }), ValidationError);
  assert.throws(() => buildMoshArgs({ host: 'user@-evil', session: 'dev' }), ValidationError);
  assert.throws(() => buildMoshArgs({ host: 'h:99999', session: 'dev' }), ValidationError);
});

// TC-V14 buildMoshArgs: no-port and no-user forms
test('TC-V14 buildMoshArgs: minimal host', () => {
  const { args } = buildMoshArgs({ host: 'server', session: 'web_1' });
  const dd = args.indexOf('--');
  assert.equal(args[dd + 1], 'server');           // no user@ prefix
  assert.ok(!args.some((a) => a.startsWith('--ssh=')), 'no --ssh when no port');
});

// TC-V15 buildSshArgs: -tt, host after --, tmux DIRECT on isolated socket
test('TC-V15 buildSshArgs: direct tmux command, isolated socket, portable flags', () => {
  const { args } = buildSshArgs({ host: 'me@server:2222', session: 'dev' });
  assert.equal(args[0], '-tt', 'forces a pty');
  assert.ok(args.includes('-p') && args[args.indexOf('-p') + 1] === '2222', 'ssh port via -p');
  const dd = args.indexOf('--');
  assert.equal(args[dd + 1], 'me@server', 'user@host is the sole target after --');
  // tmux runs DIRECTLY (bypasses login-shell auto-tmux) on an isolated socket (-L)
  assert.deepEqual(args.slice(dd + 2), ['tmux', '-L', 'dtapp', 'new-session', '-A', '-s', 'dev']);
  assert.ok(!args.includes('sh'), 'no login-shell wrapper (would trigger auto-tmux)');
  assert.ok(!args.join(' ').includes('window-size'), 'no window-size flag in argv');
  assert.ok(!args.includes('-D'), 'no -D (buggy on old tmux attach path)');
});

// TC-V16 buildSshArgs: injection defenses + minimal host
test('TC-V16 buildSshArgs: refuses bad input; minimal host', () => {
  assert.throws(() => buildSshArgs({ host: '-x', session: 'dev' }), ValidationError);
  assert.throws(() => buildSshArgs({ host: 'h', session: 'a;b' }), ValidationError);
  const { args } = buildSshArgs({ host: 'server', session: 'main' });
  const dd = args.indexOf('--');
  assert.equal(args[dd + 1], 'server');   // no user@ prefix, no -p
  assert.ok(!args.includes('-p'));
  assert.deepEqual(args.slice(dd + 2), ['tmux', '-L', 'dtapp', 'new-session', '-A', '-s', 'main']);
});

// TC-V17 buildSshArgs: custom tmuxPath (newer tmux) + socket; rejects unsafe values
test('TC-V17 buildSshArgs: tmuxPath and socket', () => {
  const { args } = buildSshArgs({ host: 'h', session: 'dev', tmuxPath: '.local/bin/tmux', socket: 'dtapp' });
  const dd = args.indexOf('--');
  assert.deepEqual(args.slice(dd + 2), ['.local/bin/tmux', '-L', 'dtapp', 'new-session', '-A', '-s', 'dev']);
  assert.throws(() => buildSshArgs({ host: 'h', session: 'dev', socket: 'bad;name' }), ValidationError);
  assert.throws(() => buildSshArgs({ host: 'h', session: 'dev', tmuxPath: 'tmux; rm -rf ~' }), ValidationError);
});

// TC-V18 buildKillArgs: base64-encoded kill-session command, validated fields
test('TC-V18 buildKillArgs builds a safe base64 kill command', () => {
  const { args, script } = buildKillArgs({ host: 'me@h:22', session: 'dt-x', tmuxPath: '/home/u/.local/bin/tmux', socket: 'dtcc3' });
  assert.equal(script, '/home/u/.local/bin/tmux -L dtcc3 kill-session -t dt-x');
  const dd = args.indexOf('--');
  assert.equal(args[dd + 1], 'me@h', 'target after --');
  assert.ok(args.includes('-p') && args[args.indexOf('-p') + 1] === '22');
  const remote = args[dd + 2];
  assert.match(remote, /^echo [A-Za-z0-9+/=]+ \| base64 -d \| \/bin\/sh$/, 'base64-wrapped');
  // decode the payload and confirm it matches the script (no shell metacharacters leaked)
  const b64 = remote.split(' ')[1];
  assert.equal(Buffer.from(b64, 'base64').toString('utf8'), script);
});

// TC-V19 buildKillArgs refuses injection
test('TC-V19 buildKillArgs refuses bad input', () => {
  assert.throws(() => buildKillArgs({ host: 'h', session: 'a;b' }), ValidationError);
  assert.throws(() => buildKillArgs({ host: '-x', session: 'dt' }), ValidationError);
  assert.throws(() => buildKillArgs({ host: 'h', session: 'dt', socket: 'bad;sock' }), ValidationError);
  assert.throws(() => buildKillArgs({ host: 'h', session: 'dt', tmuxPath: 'tmux; rm -rf ~' }), ValidationError);
});
