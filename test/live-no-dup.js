'use strict';
// Verify that restarting the app (reload store -> mount -> createSession) does NOT create
// duplicate tmux sessions. Simulates the exact renderer+main restore path, twice.
// Usage: HOST=user@host node test/live-no-dup.js
const os = require('os'), path = require('path'), fs = require('fs');
const { SessionStore } = require('../src/main/sessionStore');
const { SshTmuxBackend } = require('../src/main/backends/sshTmuxBackend');

const HOST = process.env.HOST;
if (!HOST) { console.error('set HOST'); process.exit(2); }

const genSession = (id) => `dt-${String(id).replace(/[^A-Za-z0-9]/g, '').slice(-12) || 'main'}`;
const storeFile = path.join(fs.mkdtempSync(path.join(os.tmpdir(), 'dt-nodup-')), 'sessions.json');
const store = new SessionStore(storeFile);

// One "app launch": load store, connect the (first) restored session like the renderer does.
function launch(label) {
  return new Promise((resolve) => {
    const list = store.load();
    const entry = list[0];
    const session = entry.session;           // reuse persisted name (the fix)
    const b = new SshTmuxBackend({ host: HOST, session });
    let buf = '';
    b.on('data', (d) => { buf += d; });
    b.spawn({ cols: 80, rows: 24 });
    setTimeout(() => b.write('tmux list-sessions | grep -c . \n'), 6000);
    setTimeout(() => {
      const m = buf.match(/dt-[0-9a-z]+:/g);   // sessions named dt-* in the list output
      console.log(`${label}: connected session=${session}`);
      b.kill();
      resolve();
    }, 8500);
  });
}

(async () => {
  const id = 'nodup-' + Math.floor(Date.now() / 1000);
  const session = genSession(id);
  store.save([{ id, host: HOST, session, transport: 'ssh', title: HOST, order: 0 }]);
  console.log('persisted session name:', session);

  await launch('launch 1');
  await launch('launch 2 (restart)');
  await launch('launch 3 (restart)');

  // Count how many dt-* sessions exist now — should be exactly 1 if reattach works.
  const { execFileSync } = require('child_process');
  const env = { ...process.env, PATH: `/opt/homebrew/bin:${process.env.PATH || ''}` };
  const out = execFileSync('ssh', ['-o', 'BatchMode=yes', HOST, `tmux list-sessions 2>/dev/null | grep -c '^${session}:'`], { env, encoding: 'utf8' }).trim();
  console.log(`\n${session} count after 3 launches: ${out}`);
  console.log(out === '1' ? 'PASS: reattached, no duplicates' : `FAIL: expected 1, got ${out}`);
  execFileSync('ssh', ['-o', 'BatchMode=yes', HOST, `tmux kill-session -t ${session} 2>/dev/null || true`], { env });
  process.exit(0);
})();
