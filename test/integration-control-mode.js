'use strict';
// Integration test for control-mode reconnect (needs the live host; NOT in `npm test`).
// Usage: HOST=user@host TMUX=/home/u/.local/bin/tmux node test/integration-control-mode.js
//
// Reproduces the EXACT app flow that main.js runs, at the session-orchestration layer:
//   create control session -> run a marker command -> "close app" (teardown) ->
//   "reopen app" (new supervisor+backend, SAME session/socket) -> collect the session:data
//   payloads a renderer would receive, and assert the prior content is back-filled.
const { Supervisor } = require('../src/main/supervisor');
const { ControlModeBackend } = require('../src/main/backends/controlModeBackend');

const HOST = process.env.HOST;
const TMUX = process.env.TMUX || '/home/yitong/.local/bin/tmux';
if (!HOST) { console.error('set HOST=user@host'); process.exit(2); }

const SESSION = 'itest-cc';
const MARKER = 'ITEST_MARKER_' + Math.floor(Date.now() / 1000);

// Mirror main.js startSession's control-mode wiring: supervisor -> normalized session:data.
function makeSession() {
  const rxData = [];   // what the renderer's onData would receive: {pane?, data}
  const sup = new Supervisor({
    makeBackend: () => new ControlModeBackend({
      host: HOST, session: SESSION, tmuxPath: TMUX, tmuxVersion: [3, 5],
    }),
    opts: { connectTimeoutMs: 3000 },
  });
  sup.on('data', (d) => {
    // main.js normalization: control-mode data is {pane,data}
    if (typeof d === 'string') rxData.push({ data: d });
    else rxData.push({ pane: d.pane, data: d.data });
  });
  return { sup, rxData };
}

function delay(ms) { return new Promise((r) => setTimeout(r, ms)); }

(async () => {
  console.log(`HOST=${HOST}`);
  console.log(`marker=${MARKER}`);

  // --- "First launch": create the session, run a command that leaves content on screen ---
  const s1 = makeSession();
  s1.sup.start({ cols: 90, rows: 30 });
  await delay(4000);
  s1.sup.write(`printf '${MARKER}\\n'\n`);
  await delay(2500);
  const firstText = s1.rxData.map((d) => d.data).join('');
  console.log(`[launch1] saw marker live: ${/ITEST_MARKER_\d+/.test(firstText) ? 'YES' : 'no'}`);
  // "Close the app": tear the supervisor down (kills local client; remote tmux persists)
  s1.sup.close();
  await delay(2000);

  // --- "Reopen app": brand-new supervisor+backend attaching the SAME session ---
  const s2 = makeSession();
  s2.sup.start({ cols: 90, rows: 30 });
  await delay(5000);   // let it attach + capture-on-attach back-fill
  const reText = s2.rxData.map((d) => d.data).join('');
  const backfilled = reText.includes(MARKER);
  s2.sup.close();
  await delay(1500);

  console.log('\n===== RESULT =====');
  console.log(`reconnect received ${s2.rxData.length} data events`);
  console.log(`reconnect back-filled the marker: ${backfilled ? 'YES ✅' : 'NO ❌'}`);
  if (!backfilled) {
    console.log('--- what the reconnect DID receive (last 400 chars) ---');
    console.log(JSON.stringify(reText.slice(-400)));
  }
  process.exit(backfilled ? 0 : 1);
})().catch((e) => { console.error('ERROR', e); process.exit(3); });
