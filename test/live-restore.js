'use strict';
// Manual live verification of the REOPEN-THE-APP reconnect (the reported bug).
// Simulates: launch 1 creates a session + persists it; app "closes"; launch 2 loads the
// persisted list and reconnects by calling session:create with the SAME id+session.
// Usage: HOST=user@host node test/live-restore.js
const os = require('os');
const path = require('path');
const fs = require('fs');
const { SessionStore } = require('../src/main/sessionStore');
const { SshTmuxBackend } = require('../src/main/backends/sshTmuxBackend');

const HOST = process.env.HOST;
if (!HOST) { console.error('set HOST=user@host'); process.exit(2); }

const storeFile = path.join(fs.mkdtempSync(path.join(os.tmpdir(), 'dt-restore-')), 'sessions.json');
const store = new SessionStore(storeFile);

// derive session name the way main.js does
function genSession(id) {
  return `dt-${String(id).replace(/[^A-Za-z0-9]/g, '').slice(-12) || 'main'}`;
}

function connect(id, session, steps) {
  return new Promise((resolve) => {
    const b = new SshTmuxBackend({ host: HOST, session });
    let buf = '';
    b.on('data', (d) => { buf += d; });
    b.on('exit', () => resolve(buf));
    b.spawn({ cols: 80, rows: 24 });
    let t = 6000;
    for (const cmd of steps) { setTimeout(() => b.write(cmd + '\n'), t); t += 2500; }
    setTimeout(() => { b.kill(); resolve(buf); }, t + 1500);
  });
}

(async () => {
  const id = 'launch-' + Math.floor(Date.now() / 1000);
  const session = genSession(id);
  const marker = 'RESTORE_' + Math.floor(Date.now() / 1000);

  console.log('LAUNCH 1: create session, persist it, write a marker, then "close app"...');
  store.save([{ id, host: HOST, session, transport: 'ssh', title: HOST, order: 0 }]);
  await connect(id, session, [`echo ${marker} > /tmp/dt_restore`, 'echo done']);

  console.log('LAUNCH 2: reload persisted list, reconnect with SAME id+session...');
  const restored = store.load();
  console.log('  persisted:', JSON.stringify(restored[0]));
  const entry = restored[0];
  const buf = await connect(entry.id, entry.session, ['cat /tmp/dt_restore', `rm -f /tmp/dt_restore; tmux kill-session -t ${entry.session}`]);

  if (buf.includes(marker)) console.log(`\nPASS: reopened app reconnected the SAME session; work survived (${marker})`);
  else console.log(`\nFAIL: did not reattach. tail=${JSON.stringify(buf.slice(-200))}`);
  process.exit(0);
})();
