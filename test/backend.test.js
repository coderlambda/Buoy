'use strict';
// Integration tests for the LocalBackend against a REAL node-pty (TEST_PLAN TC-L).
// Exercises the ConnectionBackend contract end-to-end on this machine (no remote needed).
const { test } = require('node:test');
const assert = require('node:assert');
const { LocalBackend } = require('../src/main/backends/localBackend');

function collectUntil(backend, marker, timeoutMs = 5000) {
  return new Promise((resolve, reject) => {
    let buf = '';
    const to = setTimeout(() => reject(new Error(`timeout waiting for ${marker}; got: ${JSON.stringify(buf)}`)), timeoutMs);
    backend.on('data', (d) => {
      buf += d;
      if (buf.includes(marker)) { clearTimeout(to); resolve(buf); }
    });
  });
}

// TC-L1 spawn + data
test('TC-L1 local shell emits output via onData', async () => {
  const b = new LocalBackend({ shell: '/bin/echo', args: ['MARKER_L1'] });
  const p = collectUntil(b, 'MARKER_L1');
  b.spawn({ cols: 80, rows: 24 });
  const out = await p;
  assert.match(out, /MARKER_L1/);
  b.kill();
});

// TC-L2 write reaches the shell (round-trip a marker through an interactive shell)
test('TC-L2 write reaches the shell', async () => {
  const b = new LocalBackend({ shell: '/bin/sh', args: [] });
  const p = collectUntil(b, 'ROUNDTRIP_OK');
  b.spawn({ cols: 80, rows: 24 });
  b.write('echo ROUNDTRIP_OK\n');
  const out = await p;
  assert.match(out, /ROUNDTRIP_OK/);
  b.kill();
});

// TC-L3 resize changes the reported terminal size
test('TC-L3 resize propagates to the pty', async () => {
  const b = new LocalBackend({ shell: '/bin/sh', args: [], env: { ...process.env, TERM: 'xterm' } });
  b.spawn({ cols: 80, rows: 24 });
  // let the shell start, then resize, then read the pty winsize directly via `stty size`
  // (rows cols) which reflects the kernel winsize without needing terminfo.
  await new Promise((r) => setTimeout(r, 300));
  b.resize(123, 40);
  await new Promise((r) => setTimeout(r, 200));
  // Split the marker so it appears ONLY in the output, not the echoed command line.
  const p = collectUntil(b, 'RSZ' + '=');
  b.write('printf "R""SZ=%s\\n" "$(stty size)"\n');
  const out = await p;
  const m = out.match(/RSZ=(\d+)\s+(\d+)/);
  assert.ok(m, `got a SIZE= line; out=${JSON.stringify(out.slice(-120))}`);
  assert.equal(Number(m[1]), 40, 'pty rows reflect resize');
  assert.equal(Number(m[2]), 123, 'pty cols reflect resize');
  b.kill();
});

// TC-L4 shell exit => onExit with code
test('TC-L4 shell exit surfaces onExit with code', async () => {
  const b = new LocalBackend({ shell: '/bin/sh', args: [] });
  const exited = new Promise((resolve) => b.on('exit', resolve));
  b.spawn({ cols: 80, rows: 24 });
  b.write('exit 7\n');
  const code = await exited;
  assert.equal(code, 7);
});

// TC-L5 kill terminates the child
test('TC-L5 kill terminates the child', async () => {
  const b = new LocalBackend({ shell: '/bin/sh', args: [] });
  const exited = new Promise((resolve) => b.on('exit', resolve));
  b.spawn({ cols: 80, rows: 24 });
  await new Promise((r) => setTimeout(r, 100));
  b.kill();
  await exited; // resolves => child terminated
  assert.ok(true);
});
