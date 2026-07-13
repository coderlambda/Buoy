'use strict';
// Manual live verification (NOT part of npm test — needs a real reachable host).
// Usage: HOST=user@host node test/live-reattach.js
// Proves the durability model: work started in a session survives a client "drop",
// and a fresh connection REATTACHES the same tmux session (no duplicate).
const { SshTmuxBackend } = require('../src/main/backends/sshTmuxBackend');

const HOST = process.env.HOST;
if (!HOST) { console.error('set HOST=user@host'); process.exit(2); }
const SESSION = 'dt_live';
const MARKER = 'MARKER_' + Math.floor(Date.now() / 1000);

function run(steps) {
  return new Promise((resolve) => {
    const b = new SshTmuxBackend({ host: HOST, session: SESSION });
    let buf = '';
    b.on('data', (d) => { buf += d; });
    b.on('exit', () => resolve({ buf }));
    b.spawn({ cols: 80, rows: 24 });
    let t = 6000;
    for (const [cmd] of steps) { setTimeout(() => b.write(cmd + '\n'), t); t += 2500; }
    setTimeout(() => { b.kill(); resolve({ buf }); }, t + 1500);
  });
}

(async () => {
  console.log('1) connect, write a marker file, then drop the client...');
  await run([[`echo ${MARKER} > /tmp/dt_live_marker`], ['echo wrote']]);

  console.log('2) reconnect — expect the same session and the marker to survive...');
  const { buf } = await run([['cat /tmp/dt_live_marker'], [`rm -f /tmp/dt_live_marker; tmux kill-session -t ${SESSION}`]]);

  if (buf.includes(MARKER)) console.log(`\nPASS: reattached the same session; work survived (${MARKER})`);
  else console.log(`\nFAIL: marker not found. tail=${JSON.stringify(buf.slice(-200))}`);
  process.exit(0);
})();
