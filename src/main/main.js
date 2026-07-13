'use strict';
// Electron main process (DESIGN.md §4, §6.3). Owns backends/supervisors and the pty;
// the renderer only talks to it over a narrow contextBridge IPC surface.
const { app, BrowserWindow, ipcMain, shell, clipboard } = require('electron');
const path = require('path');
const { execFileSync, execFile } = require('child_process');
const { spawnEnv } = require('./env');
const { Supervisor } = require('./supervisor');
const { LocalBackend } = require('./backends/localBackend');
const { EtTmuxBackend } = require('./backends/etTmuxBackend');
const { MoshTmuxBackend } = require('./backends/moshTmuxBackend');
const { SshTmuxBackend } = require('./backends/sshTmuxBackend');
const { ControlModeBackend } = require('./backends/controlModeBackend');
const { probeTmux } = require('./probeTmux');
const { SessionStore } = require('./sessionStore');
const { Backpressure } = require('../shared/backpressure');
const { ValidationError, buildKillArgs } = require('../shared/validation');
const { socketName } = require('../shared/tmuxSocket');

const sessions = new Map(); // id -> { supervisor, backpressure, meta }
let win = null;
let store = null;

// Which LOCAL binary each transport needs. Missing binary => node-pty silently exits 1,
// which the supervisor would misread as a dead host; preflight it to give a real message.
function requiredBinary(meta) {
  if (meta.kind === 'local') return null;
  const t = meta.transport || 'ssh';
  if (t === 'mosh') return 'mosh';
  if (t === 'et') return 'et';
  return 'ssh';   // ssh transport: ssh is essentially always present
}

function haveBinary(name) {
  // Use the augmented PATH so Homebrew/MacPorts/~/.local binaries are found even when
  // Electron was launched from Finder with a minimal PATH.
  try {
    execFileSync('/bin/sh', ['-c', `command -v ${name}`], { stdio: 'ignore', env: spawnEnv() });
    return true;
  } catch (_) { return false; }
}

function makeBackendFor(meta, id) {
  if (meta.kind === 'local') {
    return () => new LocalBackend({ shell: meta.shell });
  }
  // remote: pick transport. Validation happens inside each backend (throws on bad input).
  const transport = meta.transport || 'ssh';   // 'ssh' | 'mosh' | 'et'
  if (transport === 'mosh') {
    return () => new MoshTmuxBackend({
      host: meta.host, session: meta.session, baseArgs: meta.baseArgs || [],
    });
  }
  if (transport === 'et') {
    return () => new EtTmuxBackend({
      host: meta.host, session: meta.session, id,
      remoteUser: meta.remoteUser, baseArgs: meta.baseArgs || [],
    });
  }
  // Control mode ('native tabs') — only offered when the probe found tmux >= 3.2.
  if (meta.mode === 'control') {
    return () => new ControlModeBackend({
      host: meta.host, session: meta.session, baseArgs: meta.baseArgs || [],
      tmuxPath: meta.tmuxPath, tmuxVersion: meta.tmuxVersion,
    });
  }
  return () => new SshTmuxBackend({
    host: meta.host, session: meta.session, baseArgs: meta.baseArgs || [],
    tmuxPath: meta.tmuxPath,        // resolved by the probe at create (see session:create)
    tmuxVersion: meta.tmuxVersion,  // [maj,min] → version-tagged socket (avoids stale clash)
  });
}

// The tmux socket a session's backend uses (must match sshTmuxBackend / controlModeBackend);
// the naming rule lives in one place (shared/tmuxSocket) so it can't drift between them.
function socketFor(meta) {
  return socketName(meta.mode, meta.tmuxVersion);
}

// Kill the REMOTE tmux session for `meta`. Runs `tmux -L <sock> kill-session -t <name>` over
// ssh, base64-encoded under /bin/sh (the host login shell is zsh and mangles raw quoting —
// same technique as probeTmux). Validated fields only (host/session already charset-checked
// when the session was created; re-validate defensively).
function killRemoteTmux(meta) {
  const tmuxPath = (typeof meta.tmuxPath === 'string' && meta.tmuxPath) ? meta.tmuxPath : 'tmux';
  const { args } = buildKillArgs({ host: meta.host, session: meta.session, tmuxPath, socket: socketFor(meta) });
  return new Promise((resolve, reject) => {
    execFile('ssh', args, { env: spawnEnv(), timeout: 12000 }, (err, _out, stderr) => {
      // tmux exits non-zero if the session was already gone — treat "can't find session" as success.
      if (err && !/can't find session|no server running/i.test(String(stderr))) reject(new Error(String(stderr || err.message).trim().slice(0, 120)));
      else resolve();
    });
  });
}

function startSession(meta) {
  const id = meta.id;

  // Preflight: is the required local binary installed? (mosh/et)
  const bin = requiredBinary(meta);
  if (bin && !haveBinary(bin)) {
    send('session:error', { id, error: `'${bin}' is not installed locally. Install it, or pick the other transport in the New-session dialog.` });
    return;
  }

  let makeBackend;
  try {
    makeBackend = makeBackendFor(meta, id);
    // Build once eagerly for remote to surface validation errors before spawning.
    if (meta.kind !== 'local') makeBackend();
  } catch (e) {
    if (e instanceof ValidationError) {
      send('session:error', { id, error: e.message });
      return;
    }
    throw e;
  }

  const supervisor = new Supervisor({ makeBackend });
  const bp = new Backpressure({
    onPause: () => { /* main pauses reads via backend if supported */ },
    onResume: () => {},
  });

  supervisor.on('state', (state) => send('session:state', { id, state }));
  supervisor.on('data', ({ data, window, pane }) => {
    // Uniform shape from the supervisor: { data, window?, pane? } (window/pane set in control mode).
    bp.onData(Buffer.byteLength(data));
    send('session:data', { id, window, pane, data });
  });
  supervisor.on('window', (w) => send('session:window', { id, ...w }));
  supervisor.on('ready', () => send('session:ready', { id }));
  supervisor.on('intentional-exit', () => send('session:intentional-exit', { id }));

  sessions.set(id, { supervisor, bp, meta });
  supervisor.start({ cols: meta.cols || 80, rows: meta.rows || 24 });
}

function send(channel, payload) {
  if (win && !win.isDestroyed()) win.webContents.send(channel, payload);
}

function registerIpc() {
  // Renderer debug logs -> same file as backend (/tmp/dt-debug.log) + main stderr.
  ipcMain.on('dt:log', (_e, msg) => {
    if (process.env.DT_DEBUG === '0') return;
    const line = '[DT ui] ' + String(msg);
    process.stderr.write(line + '\n');
    try { require('fs').appendFileSync('/tmp/dt-debug.log', new Date().toISOString() + ' ' + line + '\n'); } catch (_) {}
  });

  // Link-plugin actions (§13). Scheme-validate before opening — terminal text is untrusted,
  // so only http/https/ftp/file/mailto reach the OS handler (no javascript:, file exec, etc.).
  ipcMain.handle('shell:openExternal', (_e, url) => {
    if (typeof url === 'string' && /^(https?|ftp|file|mailto):/i.test(url)) { shell.openExternal(url); return { ok: true }; }
    return { ok: false };
  });
  ipcMain.handle('clipboard:write', (_e, text) => { clipboard.writeText(String(text == null ? '' : text)); return { ok: true }; });

  ipcMain.handle('sessions:list', () => store.load());

  ipcMain.handle('session:create', async (_e, meta) => {
    const id = meta.id || String(Date.now());
    // The app OWNS the tmux session name — users pick a host, not a session id.
    // Derive a stable name FROM the id so a restored session reattaches the SAME tmux
    // session (never creates a duplicate). Charset-safe by construction (§6.1).
    const session = meta.session && /^[A-Za-z0-9][A-Za-z0-9_-]*$/.test(meta.session)
      ? meta.session
      : `dt-${String(id).replace(/[^A-Za-z0-9]/g, '').slice(-12) || 'main'}`;
    const transport = meta.transport || 'ssh';

    // Per-host tmux resolution (ssh transport only): probe ONCE to pick the best remote
    // tmux (prefer >= 3.2 for modern behavior; fall back to whatever exists). Persist the
    // chosen path so reconnects skip the probe. Reuse a previously-persisted path if present.
    let tmuxPath = meta.tmuxPath;
    let tmuxVersion = meta.tmuxVersion || null;
    if (transport === 'ssh' && !tmuxPath && meta.kind !== 'local') {
      try {
        const res = await probeTmux(meta.host, { baseArgs: meta.baseArgs || [] });
        tmuxPath = res.tmuxPath;
        tmuxVersion = res.version;
        if (res.probed && res.version) {
          send('session:info', {
            id,
            info: `remote tmux ${res.version.join('.')} (${tmuxPath})`,
            tmuxVersion: res.version,
            tmuxPath,
          });
        }
      } catch (_) { /* validation error surfaces later in startSession */ }
    }

    // Control mode needs tmux >= 3.2. If the probe found older (or unknown), downgrade to
    // plain and tell the user, rather than failing to connect.
    let mode = meta.mode === 'control' ? 'control' : 'plain';
    if (mode === 'control' && transport === 'ssh' && meta.kind !== 'local') {
      const ok = Array.isArray(tmuxVersion) && (tmuxVersion[0] > 3 || (tmuxVersion[0] === 3 && tmuxVersion[1] >= 2));
      if (!ok) {
        mode = 'plain';
        send('session:info', { id, info: `native tabs need tmux >= 3.2; using plain mode` });
      }
    }

    const full = { ...meta, id, session, transport, mode, tmuxPath, tmuxVersion };
    const list = store.load().filter((s) => s.id !== id);
    if (full.kind !== 'local') {
      list.push({ id, host: full.host, session, transport, mode, tmuxPath: tmuxPath || null,
                  tmuxVersion: tmuxVersion || null,
                  title: full.title || full.host, order: list.length });
      store.save(list);
    }
    startSession(full);
    return { id, session };
  });

  ipcMain.on('session:input', (_e, { id, data }) => {
    const s = sessions.get(id);
    if (s) s.supervisor.write(data);
  });

  ipcMain.on('session:resize', (_e, { id, cols, rows }) => {
    const s = sessions.get(id);
    if (s) s.supervisor.resize(cols, rows);
  });

  ipcMain.on('session:ack', (_e, { id, bytes }) => {
    const s = sessions.get(id);
    if (s) s.bp.ack(bytes);
  });

  // Detach: stop the local client, leave the remote tmux session RUNNING (can reattach later).
  ipcMain.on('session:close', (_e, { id }) => {
    const s = sessions.get(id);
    if (s) { s.supervisor.close(); sessions.delete(id); }
    const list = store.load().filter((x) => x.id !== id);
    store.save(list);
  });

  // Kill: terminate the REMOTE tmux session (its processes end) AND remove it. Not reversible.
  ipcMain.handle('session:kill', async (_e, { id }) => {
    // Grab meta from a running session or the persisted store.
    const running = sessions.get(id);
    const meta = running ? running.meta : store.load().find((x) => x.id === id);
    // Stop the local client first.
    if (running) { running.supervisor.close(); sessions.delete(id); }

    let killedRemote = false;
    if (meta && meta.kind !== 'local' && meta.transport === 'ssh' && meta.host && meta.session) {
      try { await killRemoteTmux(meta); killedRemote = true; }
      catch (e) { send('session:info', { id, info: `could not kill remote session: ${e.message}` }); }
    }
    const list = store.load().filter((x) => x.id !== id);
    store.save(list);
    return { ok: true, killedRemote };
  });

  ipcMain.on('session:retry', (_e, { id }) => {
    const s = sessions.get(id);
    if (s) s.supervisor.retry();
  });

  // Project tab ops (§14) — control mode only; forwarded to the backend's window commands.
  const backendOf = (id) => { const s = sessions.get(id); return s && s.supervisor && s.supervisor.backend; };
  ipcMain.on('tab:new', (_e, { id }) => { const b = backendOf(id); if (b && b.newWindow) b.newWindow(); });
  ipcMain.on('tab:select', (_e, { id, win }) => { const b = backendOf(id); if (b && b.selectWindow) b.selectWindow(win); });
  ipcMain.on('tab:close', (_e, { id, win }) => { const b = backendOf(id); if (b && b.killWindow) b.killWindow(win); });
  ipcMain.on('tab:capture', (_e, { id, win }) => { const b = backendOf(id); if (b && b.captureWindow) b.captureWindow(win); });

  // Rename = change the DISPLAY TITLE only. The tmux session name is the reattach key and
  // must never change, so it is deliberately left alone here.
  ipcMain.handle('session:rename', (_e, { id, title }) => {
    const clean = String(title == null ? '' : title).trim().slice(0, 80);
    if (!clean) return { ok: false };
    // update in-memory meta (if running) and the persisted store
    const s = sessions.get(id);
    if (s) s.meta.title = clean;
    const list = store.load();
    const entry = list.find((x) => x.id === id);
    if (entry) { entry.title = clean; store.save(list); }
    return { ok: true, title: clean };
  });
}

function createWindow() {
  win = new BrowserWindow({
    width: 1100, height: 700,
    webPreferences: {
      preload: path.join(__dirname, '../preload/preload.js'),
      contextIsolation: true,
      nodeIntegration: false,
      sandbox: true,
    },
  });
  win.loadFile(path.join(__dirname, '../renderer/index.html'));
}

app.whenReady().then(() => {
  store = new SessionStore(path.join(app.getPath('userData'), 'sessions.json'));
  registerIpc();
  createWindow();
  app.on('activate', () => { if (BrowserWindow.getAllWindows().length === 0) createWindow(); });
});

app.on('window-all-closed', () => { if (process.platform !== 'darwin') app.quit(); });
