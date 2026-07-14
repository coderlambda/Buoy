'use strict';
// Renderer: sidebar + one xterm view, wired to the main process over window.terminalAPI.
// The terminal engine (xterm.js) is deliberately kept behind a thin usage so the transport
// contract (data/input/resize/ack/state) is what the rest of the UI depends on (DESIGN §7).
/* global Terminal, FitAddon, DTPlugins, DTBuiltinPlugins, DTTerminalTab, DTFileViewerTab */

const api = window.terminalAPI;
const sessionsEl = document.getElementById('sessions');
const statusEl = document.getElementById('status');
const termHost = document.getElementById('term');

const views = new Map();   // id -> { term, fit, meta, state }
let activeId = null;
let _lastSize = { cols: 0, rows: 0 };   // last size sent for the active view (resize debounce)

// --- plugin framework (§13): link matchers turn URLs/paths (and custom patterns) into
// clickable links. URL + path are built-in plugins; third parties register more via
// window.dtPlugins.registerLink({ name, regex, onClick(text, ctx) }). ---
const registry = new DTPlugins.PluginRegistry();
DTBuiltinPlugins.builtinLinkPlugins().forEach((p) => registry.registerLink(p));
// Built-in 'terminal' tab-kind (§14/§15): tabs are polymorphic content providers, so future
// kinds (markdown, browser, ...) register the same way with no renderer changes.
registry.registerTabKind({ kind: 'terminal', create: (spec, ctx) => DTTerminalTab.createTerminalTab(spec, ctx) });
// 'fileviewer' tab-kind (§16): app-local tab (no tmux window) that previews a clicked path.
registry.registerTabKind({ kind: 'fileviewer', create: (spec, ctx) => DTFileViewerTab.createFileViewerTab(spec, ctx) });
// Public extension API (stable surface for user/third-party plugins).
window.dtPlugins = {
  registerLink: (p) => registry.registerLink(p),   // returns an unregister() fn
  registerTabKind: (p) => registry.registerTabKind(p),   // { kind, create(spec,ctx) } -> unregister
};

// Debug -> main -> /tmp/dt-debug.log + browser console (so both are available for diagnosis).
function dbg(...a) {
  const msg = a.map((x) => (typeof x === 'string' ? x : JSON.stringify(x))).join(' ');
  try { console.log('[DT ui]', msg); } catch (_) {}
  try { api.log(msg); } catch (_) {}
}

function setStatus(t) { setStatusRaw(t); }
function setStatusRaw(t) { statusEl.textContent = t; }

// §18: loopback host set (from the host config; default localhost/127.0.0.1). Loaded at startup.
let loopbackHosts = ['localhost', '127.0.0.1'];
// Does this URL point at a configured remote-loopback host (so it needs an ssh -L tunnel)?
function isLoopbackUrl(url) {
  const m = /^(?:https?:\/\/)?([^\s/:]+):(\d{1,5})(?:[/?]|$)/.exec(url);
  return !!(m && loopbackHosts.includes(m[1]) && +m[2] >= 1 && +m[2] <= 65535);
}

// A VIEW is a project: one connection, potentially many tabs (tmux windows, §14). Each tab is
// a polymorphic TabContent (§15) — today always a 'terminal'. A single-window project behaves
// exactly like the old single-session view.
function makeView(meta) {
  const v = {
    meta, state: 'idle', started: false,
    inputReady: meta.mode !== 'control',   // non-control ready immediately
    tabs: new Map(),                        // winId '@N' -> tab
    activeWindow: null,                     // '@N' (authoritative: set by backend 'active' event)
    el: null,                               // container in #term holding tab elements
    tmuxVersion: meta.tmuxVersion,
    tunnels: [],                            // §18: [{remote, local}] forwarded ports (sidebar list)
  };
  views.set(meta.id, v);
  // For non-control (plain/local) there are no tmux window events; use a single implicit tab.
  if (meta.mode !== 'control') ensureTab(v, '@single');
  // Flush any data that arrived before this view existed (reconnect race).
  if (pendingData[meta.id]) { v.pending = (v.pending || []).concat(pendingData[meta.id]); delete pendingData[meta.id]; }
  return v;
}

// Create (once) a tab for a window id, backed by a 'terminal' TabContent via the registry.
function ensureTab(v, winId) {
  if (v.tabs.has(winId)) return v.tabs.get(winId);
  const linkProvider = makeLinkProvider(() => tab.content.term, v.meta);
  const ctx = {
    // Always forward: the backend owns input gating (buffers until ready, then replays in order)
    // and addresses keystrokes to the active window. The renderer no longer buffers input itself.
    input: (data) => api.input(v.meta.id, data),
    ack: (bytes) => api.ack(v.meta.id, bytes),
  };
  const content = registry.createTabContent('terminal', { id: v.meta.id, meta: v.meta, linkProvider }, ctx);
  const tab = { winId, title: winId, content, mounted: false };
  v.tabs.set(winId, tab);
  return tab;
}

// A real tmux window id ('@N'). Viewer tabs use synthetic 'view:N' ids and must NOT drive tmux
// window commands (select-window/kill-window) — gate every such call on this.
function isWindowTab(winId) { return /^@\d+$/.test(winId); }

let _viewerSeq = 0;

// Open a file-viewer tab for a clicked path (§16). App-local tab (no tmux window): synthetic id,
// tmux commands are gated off it. Fetches its own content on mount.
function openViewer(sessionId, path) {
  const v = views.get(sessionId) || views.get(activeId);
  if (!v) return;
  const winId = 'view:' + (++_viewerSeq);
  const ctx = { setStatus: (m) => setStatus(m) };
  const content = registry.createTabContent('fileviewer',
    { id: v.meta.id, path, api }, ctx);
  const tab = { winId, title: baseName(path), content, mounted: false, viewer: true };
  v.tabs.set(winId, tab);
  v.activeWindow = winId;
  if (v.meta.id === activeId) { showActiveTab(v); renderTabs(v); }
  setStatus('opening ' + baseName(path) + '…');
}

function baseName(p) { return (String(p).split('/').pop() || 'file'); }

// §18: Shift+Cmd chooser — a small popup letting the user pick where to open a URL. Loopback URLs
// offer tunnel-open; all URLs offer copy and open-plain.
function chooseOpen(sessionId, url) {
  const loop = isLoopbackUrl(url);
  const items = [];
  if (loop) items.push(['Open in local browser (tunnel)', () => api.openForwardedUrl(sessionId, url)]);
  items.push(['Open in browser', () => api.openExternal(loop && !/^https?:\/\//.test(url) ? 'http://' + url : url)]);
  items.push(['Copy URL', () => api.copyText(url)]);

  const back = document.createElement('div');
  back.className = 'chooser-back';
  const box = document.createElement('div');
  box.className = 'chooser';
  const title = document.createElement('div'); title.className = 'chooser-title'; title.textContent = url;
  box.appendChild(title);
  const close = () => { if (back.parentNode) back.parentNode.removeChild(back); };
  items.forEach(([label, fn]) => {
    const b = document.createElement('button');
    b.className = 'chooser-item'; b.textContent = label;
    b.onclick = () => { close(); try { fn(); } catch (_) {} };
    box.appendChild(b);
  });
  back.appendChild(box);
  back.onclick = (e) => { if (e.target === back) close(); };
  document.addEventListener('keydown', function esc(e) { if (e.key === 'Escape') { close(); document.removeEventListener('keydown', esc); } });
  document.body.appendChild(back);
}

// The active tab (or the sole tab for single-window/plain views).
function activeTab(v) {
  if (v.activeWindow && v.tabs.has(v.activeWindow)) return v.tabs.get(v.activeWindow);
  const first = v.tabs.values().next();
  return first.done ? null : first.value;
}

// Build an xterm ILinkProvider that asks the plugin registry for matches on each line.
// getTerm() returns the tab's xterm lazily (the term is created inside the TabContent).
function makeLinkProvider(getTerm, meta) {
  const ctx = {
    meta,
    openExternal: (url) => api.openExternal(url),
    copyText: (text) => api.copyText(text),
    setStatus: (msg) => setStatus(msg),
    // §16: open a clicked path in an in-app file-viewer tab of this session.
    openViewer: (path) => openViewer(meta.id, path),
    // §18: is this URL a remote-loopback URL (needs an ssh -L tunnel to reach)?
    isLoopback: (url) => isLoopbackUrl(url),
    // §18: open a loopback URL via a tunnel (host forwards + opens the local URL).
    openForwardedUrl: async (url) => {
      setStatus('forwarding ' + url + '…');
      try {
        const res = await api.openForwardedUrl(meta.id, url);
        setStatus(res && res.localUrl ? ('opened ' + res.localUrl) : ('could not forward ' + url));
      } catch (e) { setStatus('forward failed: ' + (e && e.message || e)); }
    },
    // §18: Shift+Cmd chooser — where to open a URL.
    chooseOpen: (url) => chooseOpen(meta.id, url),
  };
  return {
    provideLinks(lineNumber, callback) {
      const term = getTerm();
      if (!term) { callback(undefined); return; }
      // Read the wrapped logical line content for the given buffer row.
      const buf = term.buffer.active;
      const row = buf.getLine(lineNumber - 1);   // xterm passes 1-based line numbers
      if (!row) { callback(undefined); return; }
      const text = row.translateToString(true);
      const matches = registry.findMatches(text);
      if (!matches.length) { callback(undefined); return; }
      const links = matches.map((m) => ({
        // xterm ranges are 1-based, inclusive-ish: [start.x, end.x) with y = line number.
        range: { start: { x: m.start + 1, y: lineNumber }, end: { x: m.end + 1, y: lineNumber } },
        text: m.text,
        decorations: { underline: true, pointerCursor: true },
        activate: (event) => {
          // Pass modifier state so handlers can offer a chooser (Shift+Cmd) vs the smart default.
          const mods = { shift: !!(event && event.shiftKey), meta: !!(event && (event.metaKey || event.ctrlKey)), alt: !!(event && event.altKey) };
          try { m.plugin.onClick(m.text, ctx, mods); }
          catch (e) { setStatus('link handler error: ' + (e && e.message)); }
        },
      }));
      callback(links);
    },
  };
}

async function mount(id) {
  const v = views.get(id);
  if (!v) return;

  // Ensure the project has a container in #term (one per project; tabs live inside it).
  if (!v.el) {
    v.el = document.createElement('div');
    v.el.style.width = '100%'; v.el.style.height = '100%';
    termHost.appendChild(v.el);
  }
  // Show this project's container, hide others (never re-open a live xterm — that blanks it).
  for (const [, other] of views) { if (other.el) other.el.style.display = (other === v) ? 'block' : 'none'; }

  // Connect the project once (reattaches the SAME tmux session; tmux replays windows -> tabs).
  if (!v.started) {
    v.started = true;
    // control mode: 'ready' arrives from the backend once attach settles (it buffers input until
    // then). inputReady here is a DISPLAY flag only — the backend owns the actual gating.
    if (v.meta.mode === 'control') v.inputReady = false;
    setStatus(`connecting ${v.meta.title || v.meta.host || 'session'}…`);
    dbg('mount->createSession id=' + v.meta.id + ' host=' + v.meta.host + ' session=' + v.meta.session + ' mode=' + v.meta.mode + ' tmuxPath=' + v.meta.tmuxPath + ' tmuxVersion=' + JSON.stringify(v.meta.tmuxVersion));
    const res = await api.createSession({
      id: v.meta.id, kind: v.meta.kind || 'remote', transport: v.meta.transport,
      host: v.meta.host, session: v.meta.session, title: v.meta.title, mode: v.meta.mode,
      tmuxPath: v.meta.tmuxPath, tmuxVersion: v.meta.tmuxVersion,
    });
    dbg('mount->createSession returned ' + JSON.stringify(res));
  }

  activeId = id;
  showActiveTab(v);        // mount + reveal the active tab's content
  renderTabs(v);
  renderSidebar();
  // §18: pull any live forwarded ports for this session (persist across mount/reconnect).
  api.listTunnels(id).then((t) => { v.tunnels = Array.isArray(t) ? t : []; renderSidebar(); }).catch(() => {});
}

// Mount (if needed) and reveal the active tab's content; hide the project's other tabs.
function showActiveTab(v) {
  const tab = activeTab(v);
  if (!tab) return;
  if (!tab.mounted) {
    tab.content.mount(v.el);
    tab.mounted = true;
    dbg('mount tab ' + tab.winId + ' for project ' + v.meta.id);
    // flush data buffered before this tab was mounted (project-level + tab-level)
    if (v.pending && v.pending.length) { const b = v.pending; v.pending = null; b.forEach((d) => tab.content.onData(d)); }
    if (tab.pre && tab.pre.length) { const b = tab.pre; tab.pre = null; b.forEach((d) => tab.content.onData(d)); }
  }
  for (const [, t] of v.tabs) { const el = t.content.element && t.content.element(); if (el) el.style.display = (t === tab) ? 'block' : 'none'; }
  requestAnimationFrame(() => {
    const size = tab.content.fit ? tab.content.fit() : null;
    // Always re-assert size on show (a switched-to session/tab must be told its dimensions), and
    // keep _lastSize in sync so the debounced window-resize handler's change check stays correct.
    if (size) { api.resize(v.meta.id, size.cols, size.rows); if (v.meta.id === activeId) _lastSize = size; }
    if (tab.content.focus) tab.content.focus();
  });
}

function renderSidebar() {
  sessionsEl.innerHTML = '';
  for (const [id, v] of views) {
    const li = document.createElement('li');
    li.className = 'session' + (id === activeId ? ' active' : '') + (v.state === 'dead' ? ' dead' : '');
    const sub = v.meta.host ? escapeHtml(v.meta.host) + (v.tmuxVersion ? ` · tmux ${v.tmuxVersion.join('.')}` : '') : (v.meta.kind || 'local');
    // §18: forwarded ports, listed under the project name. Each row: "remote → local" + close.
    const tunnelRows = (v.tunnels || []).map((t) =>
      `<span class="tunnel" data-remote="${t.remote}" title="click to open http://localhost:${t.local}/">
         <span class="tport">:${t.remote}</span><span class="tarrow">→</span><span class="tlocal">localhost:${t.local}</span>
         <span class="tclose" title="close tunnel">×</span>
       </span>`).join('');
    li.innerHTML = `<span class="dot ${v.state}"></span>
      <span class="body">
        <span class="name" title="double-click to rename">${escapeHtml(v.meta.title || v.meta.session || v.meta.kind)}</span>
        <span class="sub">${sub}</span>
        ${tunnelRows ? `<span class="tunnels">${tunnelRows}</span>` : ''}
      </span>
      <span class="retry">retry</span>
      <span class="act detach" title="Detach (keeps running on the remote)">⤫</span>
      <span class="act kill" title="Kill (ends the remote session)">⏻</span>`;
    const nameEl = li.querySelector('.name');
    // Double-click the name to rename (display title only; tmux session name unchanged).
    nameEl.ondblclick = (e) => { e.stopPropagation(); startRename(id, nameEl); };
    li.querySelector('.detach').onclick = (e) => { e.stopPropagation(); detachSession(id); };
    li.querySelector('.kill').onclick = (e) => { e.stopPropagation(); killSession(id); };
    // tunnel rows: click opens the local URL; the × closes just that tunnel.
    li.querySelectorAll('.tunnel').forEach((el) => {
      const remote = Number(el.getAttribute('data-remote'));
      el.querySelector('.tclose').onclick = (e) => { e.stopPropagation(); api.closeTunnel(id, remote); };
      el.onclick = (e) => {
        if (e.target.classList.contains('tclose')) return;
        e.stopPropagation();
        const t = (v.tunnels || []).find((x) => x.remote === remote);
        if (t) api.openExternal('http://localhost:' + t.local + '/');
      };
    });
    li.onclick = (e) => {
      if (e.target.classList.contains('retry')) { api.retry(id); return; }
      if (nameEl.querySelector('input')) return;   // ignore clicks while editing
      mount(id);
    };
    sessionsEl.appendChild(li);
  }
}

function escapeHtml(s) { return String(s).replace(/[&<>"]/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' }[c])); }

// Remove a project view from the UI (dispose all its tabs). Does NOT touch the remote.
function removeView(id) {
  const v = views.get(id);
  if (!v) return;
  for (const [, t] of v.tabs) { try { t.content.dispose(); } catch (_) {} }
  if (v.el && v.el.parentNode) v.el.parentNode.removeChild(v.el);
  views.delete(id);
  if (activeId === id) {
    activeId = null;
    tabsEl.className = ''; tabsEl.innerHTML = '';
    const next = views.keys().next();
    if (!next.done) mount(next.value); else { termHost.innerHTML = ''; setStatus('no sessions — create one'); }
  }
  renderSidebar();
}

// Detach: stop the local client; the remote tmux session keeps running (reattach later).
function detachSession(id) { api.close(id); removeView(id); }

// Kill: terminate the remote tmux session (ends its processes). Irreversible → confirm.
async function killSession(id) {
  const v = views.get(id);
  const label = (v && (v.meta.title || v.meta.session)) || 'this session';
  if (!confirm(`Kill "${label}"?\n\nThis ENDS the remote tmux session and everything running in it. This cannot be undone.`)) return;
  setStatus(`killing ${label}…`);
  try {
    const res = await api.kill(id);
    setStatus(res && res.killedRemote ? `killed ${label}` : `removed ${label}`);
  } catch (_) { setStatus(`removed ${label}`); }
  removeView(id);
}

// Inline-edit the session's display title. On commit, persist via main and update the view.
function startRename(id, nameEl) {
  const v = views.get(id);
  if (!v) return;
  const current = v.meta.title || v.meta.session || '';
  const input = document.createElement('input');
  input.type = 'text';
  input.value = current;
  input.className = 'rename-input';
  nameEl.textContent = '';
  nameEl.appendChild(input);
  input.focus();
  input.select();

  let done = false;
  const commit = async (save) => {
    if (done) return; done = true;
    if (save) {
      const next = input.value.trim();
      if (next && next !== current) {
        const res = await api.rename(id, next);
        if (res && res.ok) v.meta.title = res.title;
      }
    }
    renderSidebar();
  };
  input.onkeydown = (e) => {
    if (e.key === 'Enter') { e.preventDefault(); commit(true); }
    else if (e.key === 'Escape') { e.preventDefault(); commit(false); }
  };
  input.onblur = () => commit(true);
}

// --- events from main ---
// Control-mode data is tagged with the WINDOW it belongs to (the backend owns pane->window
// resolution). Plain/local data has no window -> the single tab. The renderer never maps panes.
api.onData(({ id, data, window }) => {
  const v = views.get(id);
  if (!v) { dbg('onData: NO VIEW id=' + id + ' (buffering)'); (pendingData[id] = pendingData[id] || []).push(data); return; }
  const tab = (v.meta.mode === 'control') ? (window ? ensureTab(v, window) : activeTab(v)) : activeTab(v);
  deliver(v, tab, data);
});

const pendingData = {};   // id -> [data] buffered before the view exists

// Deliver a data chunk to a resolved tab (mounting/revealing it if it's the active one).
function deliver(v, tab, data) {
  if (!tab) { (v.pending = v.pending || []).push(data); return; }
  // If this tab isn't mounted yet but its project is the active one, mount it now so the
  // data (e.g. scrollback back-fill on reattach) is displayed, not just buffered.
  if (!tab.mounted && v.meta.id === activeId && v.el) { showActiveTab(v); renderTabs(v); }
  if (!tab.mounted) { (tab.pre = tab.pre || []).push(data); return; }
  tab.content.onData(data);
}

// --- test hooks (used by test/gui-live.js to drive/inspect the real xterm) ---
// Forward like a real keystroke; the backend buffers until ready, so tests need no gate here.
window.__testType = (s) => { if (activeId != null) api.input(activeId, s); };
window.__testInputReady = () => { const v = views.get(activeId); return !!(v && v.inputReady); };
window.__testMount = (id) => {
  if (!views.has(id)) makeView({ id, kind: 'remote', mode: 'control' });
  const v = views.get(id); if (v) v.started = true;   // main already started it in the test
  mount(id);
};
window.__testDispose = (id) => { removeView(id); };
window.__testReadBuffer = () => {
  const v = views.get(activeId);
  if (!v) return '';
  const tab = activeTab(v);
  return tab && tab.content.readBuffer ? tab.content.readBuffer() : '';
};
api.onState(({ id, state }) => {
  const v = views.get(id);
  if (!v) return;
  v.state = state;
  if (id === activeId) setStatus(statusLine(v, state));
  renderSidebar();
});

// Persistent status line: "<title> — <state> · tmux <ver>" so the probed tmux stays visible.
function statusLine(v, state) {
  const name = v.meta.title || v.meta.session || 'local';
  const ver = v.tmuxVersion ? ` · tmux ${v.tmuxVersion.join('.')}` : '';
  const mode = v.meta.mode === 'control' ? ' · native' : '';
  // control-mode: show "connecting" until input is un-gated (ready), so the user knows
  // keystrokes are buffered during the brief attach settle.
  const gating = (v.meta.mode === 'control' && state === 'connected' && !v.inputReady) ? ' · connecting…' : '';
  return `${name} — ${state}${ver}${mode}${gating}`;
}

// --- control mode: tmux windows become native tabs of the project (§14) ---
// The backend reconciles topology against tmux truth and emits clean add/close/rename/active
// events (each with the full window `order`). The renderer just mirrors them into tabs — it
// holds no pane/topology state of its own.
const tabsEl = document.getElementById('tabs');
api.onWindow(({ id, action, window, name, order }) => {
  dbg('onWindow id=' + id + ' action=' + action + ' window=' + window + ' order=' + JSON.stringify(order));
  const v = views.get(id);
  if (!v) { dbg('onWindow: NO VIEW for id=' + id); return; }
  if (action === 'add') {
    ensureTab(v, window);
    if (!v.activeWindow) v.activeWindow = window;   // first window = active until told otherwise
  } else if (action === 'close') {
    const t = v.tabs.get(window);
    if (t) { try { t.content.dispose(); } catch (_) {} v.tabs.delete(window); }
    if (v.activeWindow === window) { const first = v.tabs.keys().next(); v.activeWindow = first.done ? null : first.value; if (id === activeId) showActiveTab(v); }
  } else if (action === 'rename') {
    ensureTab(v, window).title = name || window;
  } else if (action === 'active') {
    // tmux switched the active window (e.g. after opening a new tab). Follow it so the shown tab
    // matches where input goes. Lazy-load the newly-focused tab's scrollback on first view.
    ensureTab(v, window);
    if (v.activeWindow !== window) {
      v.activeWindow = window;
      if (id === activeId) { showActiveTab(v); lazyCapture(v, window); }
    }
  }
  if (Array.isArray(order)) v.tabOrder = order;      // keep tab strip in tmux's window order
  if (id === activeId) renderTabs(v);
});

// Lazy scrollback back-fill: ask the backend to capture a window's screen the first time it is
// shown (the §14 "others on focus" decision). Idempotent per tab.
function lazyCapture(v, winId) {
  const tab = v.tabs.get(winId);
  if (tab && !tab.backfilled) { tab.backfilled = true; api.tabCapture(v.meta.id, winId); }
}

// Render the tab strip for the active control-mode project (hidden for plain/single).
function renderTabs(v) {
  if (!v || v.meta.mode !== 'control') { tabsEl.className = ''; tabsEl.innerHTML = ''; return; }
  tabsEl.className = 'on';
  tabsEl.innerHTML = '';
  // Show tabs in tmux's window order when known, else insertion order.
  const order = (v.tabOrder || []).filter((w) => v.tabs.has(w));
  for (const w of v.tabs.keys()) if (!order.includes(w)) order.push(w);
  for (const wid of order) {
    const tab = v.tabs.get(wid);
    const el = document.createElement('div');
    el.className = 'tab' + (wid === v.activeWindow ? ' active' : '');
    el.innerHTML = `<span class="tlabel">${escapeHtml(tab.title || wid)}</span><span class="tclose" title="close">×</span>`;
    el.querySelector('.tlabel').onclick = () => switchTab(v, wid);
    el.querySelector('.tclose').onclick = (e) => { e.stopPropagation(); closeTab(v, wid); };
    tabsEl.appendChild(el);
  }
  // '+' new session in this project
  const plus = document.createElement('div');
  plus.className = 'tab plus'; plus.textContent = '+'; plus.title = 'New session in this project';
  plus.onclick = () => api.tabNew(v.meta.id);
  tabsEl.appendChild(plus);
}

// Switch the active tab. For a tmux window, tell tmux to select it (its reconcile echoes an
// 'active' event; we reveal now for responsiveness). A viewer tab is app-local — reveal it WITHOUT
// any tmux command.
function switchTab(v, winId) {
  if (v.activeWindow === winId) return;
  v.activeWindow = winId;
  if (isWindowTab(winId)) { api.tabSelect(v.meta.id, winId); lazyCapture(v, winId); }
  showActiveTab(v);
  renderTabs(v);
}

// Close a tab. Terminal tabs -> tmux kill-window (backend removes it, emits close). Viewer tabs
// are app-local -> dispose locally, no tmux command, and re-focus a remaining tab.
function closeTab(v, winId) {
  if (isWindowTab(winId)) { api.tabClose(v.meta.id, winId); return; }
  const t = v.tabs.get(winId);
  if (t) { try { t.content.dispose(); } catch (_) {} v.tabs.delete(winId); }
  if (v.activeWindow === winId) {
    const first = v.tabs.keys().next();
    v.activeWindow = first.done ? null : first.value;
    if (v.meta.id === activeId) showActiveTab(v);
  }
  if (v.meta.id === activeId) renderTabs(v);
}
api.onError(({ id, error }) => {
  setStatus(`⚠ ${error}`);
  statusEl.style.color = '#f38ba8';
  // also write it into the active terminal so it's unmissable
  const v = views.get(id) || views.get(activeId);
  const tab = v && activeTab(v);
  if (tab && tab.mounted && tab.content.term) tab.content.term.writeln(`\r\n\x1b[31m[error] ${error}\x1b[0m`);
  else alert(error);
});
api.onIntentionalExit(({ id }) => { setStatus('session closed (detached)'); });
api.onReady(({ id }) => {
  dbg('onReady id=' + id + ' activeId=' + activeId);
  const v = views.get(id);
  if (!v) { dbg('onReady: NO VIEW for id=' + id); return; }
  v.inputReady = true;   // display flag only; the backend already flushed its buffered input
  if (id === activeId) setStatus(statusLine(v, v.state));
});
// §18: the backend pushes the updated forwarded-port list; mirror it into the view + sidebar.
api.onTunnels(({ id, tunnels }) => {
  const v = views.get(id);
  if (!v) return;
  v.tunnels = Array.isArray(tunnels) ? tunnels : [];
  renderSidebar();
});
api.onInfo(({ id, info, tmuxVersion }) => {
  const v = views.get(id);
  if (v && tmuxVersion) { v.tmuxVersion = tmuxVersion; v.meta.tmuxVersion = tmuxVersion; }
  setStatus(info);
  statusEl.style.color = 'var(--muted)';
  renderSidebar();
});

function byteLength(s) { return new TextEncoder().encode(s).length; }

// --- resize ---
// The window 'resize' event fires continuously during a drag (dozens/sec). Each fit()+resize
// makes tmux repaint the WHOLE screen via refresh-client -C, so firing per-event floods the
// terminal with full-screen repaints that fight each other (the resize flicker/garble). Debounce:
// coalesce the burst and send ONE resize once the drag settles. Only tell the backend when the
// grid size (cols/rows) actually CHANGED — a pixel drag that doesn't cross a cell boundary needs
// no tmux round-trip.
let _resizeTimer = null;
function applyResize() {
  const v = views.get(activeId);
  if (!v) return;
  const tab = activeTab(v);
  if (!tab || !tab.mounted || !tab.content.fit) return;
  const size = tab.content.fit();   // fit() also resizes the local xterm grid immediately
  if (size && (size.cols !== _lastSize.cols || size.rows !== _lastSize.rows)) {
    _lastSize = size;
    api.resize(activeId, size.cols, size.rows);
  }
}
window.addEventListener('resize', () => {
  clearTimeout(_resizeTimer);
  _resizeTimer = setTimeout(applyResize, 120);   // settle window before the single tmux resize
});

// --- new session dialog ---
const dialog = document.getElementById('dialog');
const fKind = document.getElementById('f-kind');
const remoteFields = document.getElementById('remote-fields');
document.getElementById('new').onclick = () => { document.getElementById('f-err').textContent = ''; dialog.showModal(); };
const updateFields = () => { remoteFields.style.display = fKind.value === 'remote' ? 'block' : 'none'; };
fKind.onchange = updateFields;
document.getElementById('new').addEventListener('click', updateFields);

document.getElementById('form').addEventListener('submit', async (e) => {
  // form method=dialog closes automatically; only act on OK
  const ok = e.submitter && e.submitter.value === 'ok';
  if (!ok) return;
  const kind = fKind.value;
  const host = document.getElementById('f-host').value.trim();
  if (kind === 'remote' && !host) {   // guard: remote needs a host
    e.preventDefault();               // keep the dialog open
    document.getElementById('f-err').textContent = 'Enter a host (user@host).';
    return;
  }
  const meta = {
    kind,
    transport: 'ssh',   // ssh is the only remote transport
    mode: document.getElementById('f-control').checked ? 'control' : 'plain',
    title: document.getElementById('f-title').value.trim() || (kind === 'local' ? 'local' : host),
    host,
    // NOTE: no session name from the user — main.js generates & owns it.
  };
  const { id, session } = await api.createSession(meta);
  meta.id = id;
  meta.session = session;   // remember the app-assigned name for the sidebar/label
  const v = makeView(meta);
  v.started = true;         // createSession already ran; mount() must not start it again
  mount(id);
});

// --- restore persisted sessions on launch (lazy: create views, connect on click) ---
(async function init() {
  // §18: load the loopback host set (for URL classification) before wiring links.
  try { const cfg = await api.getConfig(); if (cfg && Array.isArray(cfg.loopbackHosts)) loopbackHosts = cfg.loopbackHosts; } catch (_) {}
  const persisted = await api.listSessions();
  dbg('init: ' + persisted.length + ' persisted; first=' + JSON.stringify(persisted[0] || null));
  for (const meta of persisted) {
    // meta already has {id, host, session, transport, title} from the store.
    const v = makeView({ ...meta, kind: 'remote' });
    v.started = false;   // not connected yet; mount() will start (reattach) on click
  }
  renderSidebar();
  setStatus(persisted.length ? `${persisted.length} session(s) restored — click to reconnect` : 'no sessions — create one');
  // Auto-reconnect the first restored session so reopening the app "just works".
  if (persisted.length) mount(persisted[0].id);
})();
