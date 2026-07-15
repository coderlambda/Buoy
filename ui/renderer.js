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
    // §20: persisted customization (from the store via list_sessions).
    color: meta.color || null,              // project accent color
    savedTabOrder: Array.isArray(meta.tabOrder) ? meta.tabOrder.slice() : [],  // custom tab order
    tabColors: meta.tabColors || {},        // winId -> color
    lastTab: meta.lastTab || null,          // last-active tab (persisted; updated as tabs switch)
    restoreTab: meta.lastTab || null,       // one-shot: tab to reveal on first connect (see onWindow)
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

// §18: pull the authoritative forwarded-port status (persisted + live, each probed) into the view
// and re-render the sidebar. Safe to call on mount, reconnect, or a periodic tick.
function refreshTunnels(id) {
  api.listTunnels(id).then((t) => {
    const v = views.get(id);
    if (!v) return;
    v.tunnels = Array.isArray(t) ? t : [];
    renderSidebar();
  }).catch(() => {});
}

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
      dbg('openForwardedUrl click id=' + meta.id + ' url=' + url);
      setStatus('forwarding ' + url + '…');
      try {
        const res = await api.openForwardedUrl(meta.id, url);
        dbg('openForwardedUrl result=' + JSON.stringify(res));
        setStatus(res && res.localUrl ? ('opened ' + res.localUrl) : ('could not forward ' + url));
      } catch (e) { dbg('openForwardedUrl error=' + (e && e.message || e)); setStatus('forward failed: ' + (e && e.message || e)); }
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
  if (v.meta.kind !== 'local') api.setLastActive(id).catch(() => {});   // §20: restore-on-open target
  showActiveTab(v);        // mount + reveal the active tab's content
  renderTabs(v);
  renderSidebar();
  // §18: pull the forwarded-port status (persisted + live, probed) for this session.
  refreshTunnels(id);
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
    li.draggable = true;                       // §20: drag to reorder
    li.dataset.id = id;
    if (v.color) li.style.setProperty('--accent-bar', v.color);   // left accent bar (CSS uses it)
    li.classList.toggle('has-color', !!v.color);
    const sub = v.meta.host ? escapeHtml(v.meta.host) + (v.tmuxVersion ? ` · tmux ${v.tmuxVersion.join('.')}` : '') : (v.meta.kind || 'local');
    // §18: forwarded ports under the project name. Active rows show the local port and open on
    // click; inactive (grey) rows are persisted-but-not-serving — click re-opens the tunnel.
    // String is short: ":<remote> → :<local>" (local shows "—" when not currently mapped).
    const tunnelRows = (v.tunnels || []).map((t) => {
      const active = !!t.active;
      const localTxt = t.local ? `:${t.local}` : '—';
      const title = active ? `open http://localhost:${t.local}/` : `port ${t.remote} inactive — click to re-open`;
      // "same" marker when the local port matches the remote (forced same-port mapping).
      const same = t.local && t.local === t.remote;
      return `<span class="tunnel${active ? '' : ' inactive'}" data-remote="${t.remote}" title="${title}">
         <span class="tport">:${t.remote}</span><span class="tarrow">→</span>
         <span class="tlocal${same ? ' same' : ''}">${localTxt}</span>
         <span class="tforce" title="force map to the same local port (:${t.remote})">⇄</span>
         <span class="tclose" title="close tunnel">×</span>
       </span>`;
    }).join('');
    // The action icons live INSIDE the first (name) row, so hovering shifts only that row — the
    // sub line and tunnel rows below keep full width.
    li.innerHTML = `<span class="dot ${v.state}"></span>
      <span class="body">
        <span class="name-row">
          <span class="name" title="double-click to rename">${escapeHtml(v.meta.title || v.meta.session || v.meta.kind)}</span>
          <span class="controls">
            <span class="retry">retry</span>
            <span class="act detach" title="Detach (keeps running on the remote)">⤫</span>
            <span class="act kill" title="Kill (ends the remote session)">⏻</span>
          </span>
        </span>
        <span class="sub">${sub}</span>
        ${tunnelRows ? `<span class="tunnels">${tunnelRows}</span>` : ''}
      </span>`;
    const nameEl = li.querySelector('.name');
    // Double-click the name to rename (display title only; tmux session name unchanged).
    nameEl.ondblclick = (e) => { e.stopPropagation(); startRename(id, nameEl); };
    li.querySelector('.detach').onclick = (e) => { e.stopPropagation(); detachSession(id); };
    li.querySelector('.kill').onclick = (e) => { e.stopPropagation(); killSession(id); };
    // tunnel rows: active -> open the local URL; inactive -> re-open; ⇄ force same-port; × close.
    li.querySelectorAll('.tunnel').forEach((el) => {
      const remote = Number(el.getAttribute('data-remote'));
      el.querySelector('.tclose').onclick = (e) => { e.stopPropagation(); api.closeTunnel(id, remote); };
      // ⇄ force-map to the SAME local port; alert if that local port is already taken.
      el.querySelector('.tforce').onclick = (e) => {
        e.stopPropagation();
        setStatus('mapping port ' + remote + ' -> localhost:' + remote + '…');
        api.forceForward(id, remote)
          .then(() => { setStatus('mapped localhost:' + remote); refreshTunnels(id); })
          .catch((err) => { const m = (err && err.message) || String(err); setStatus('⚠ ' + m); alert('Could not map port ' + remote + ':\n' + m); });
      };
      el.onclick = (e) => {
        if (e.target.classList.contains('tclose') || e.target.classList.contains('tforce')) return;
        e.stopPropagation();
        const t = (v.tunnels || []).find((x) => x.remote === remote);
        if (t && t.active && t.local) {
          api.openExternal('http://localhost:' + t.local + '/');
        } else {
          // inactive/persisted: re-open the tunnel to the remote port (recreates ssh -L + opens).
          setStatus('re-opening port ' + remote + '…');
          api.openForwardedUrl(id, 'http://localhost:' + remote + '/')
            .then(() => refreshTunnels(id)).catch(() => {});
        }
      };
    });
    li.onclick = (e) => {
      if (e.target.classList.contains('retry')) { api.retry(id); return; }
      if (nameEl.querySelector('input')) return;   // ignore clicks while editing
      mount(id);
    };
    // §20: right-click opens the color palette for this project.
    li.oncontextmenu = (e) => { e.preventDefault(); openColorMenu(e, v.color, (c) => setProjectColor(id, c)); };
    // §20: drag-to-reorder (project list).
    wireSidebarDnD(li, id);
    sessionsEl.appendChild(li);
  }
}

// --- §20: project drag-and-drop reorder -------------------------------------------------------
let _dragId = null;
function wireSidebarDnD(li, id) {
  li.ondragstart = (e) => { _dragId = id; li.classList.add('dragging'); try { e.dataTransfer.effectAllowed = 'move'; e.dataTransfer.setData('text/plain', id); } catch (_) {} };
  li.ondragend = () => { _dragId = null; li.classList.remove('dragging'); sessionsEl.querySelectorAll('.drop-before,.drop-after').forEach((n) => n.classList.remove('drop-before', 'drop-after')); };
  li.ondragover = (e) => {
    if (_dragId == null || _dragId === id) return;
    e.preventDefault();
    const r = li.getBoundingClientRect();
    const after = e.clientY > r.top + r.height / 2;
    li.classList.toggle('drop-after', after);
    li.classList.toggle('drop-before', !after);
  };
  li.ondragleave = () => li.classList.remove('drop-before', 'drop-after');
  li.ondrop = (e) => {
    e.preventDefault();
    const after = li.classList.contains('drop-after');
    li.classList.remove('drop-before', 'drop-after');
    if (_dragId != null && _dragId !== id) reorderProject(_dragId, id, after);
  };
}

// Move dragged project to before/after the target, rebuild the views Map in the new order, persist.
function reorderProject(dragId, targetId, after) {
  if (!views.has(dragId) || !views.has(targetId)) return;   // a project vanished mid-drag
  const ids = [...views.keys()].filter((x) => x !== dragId);
  const at = ids.indexOf(targetId) + (after ? 1 : 0);
  ids.splice(at, 0, dragId);
  const reordered = new Map();
  for (const id of ids) { const v = views.get(id); if (v) reordered.set(id, v); }
  views.clear();
  for (const [id, v] of reordered) views.set(id, v);
  renderSidebar();
  api.reorderSessions([...views.keys()]).catch(() => {});
}

function escapeHtml(s) { return String(s).replace(/[&<>"]/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' }[c])); }

// --- §20: color palette --------------------------------------------------------------------
// A small fixed palette (Catppuccin-ish accents) + a "none" chip. Reused for projects and tabs.
const PALETTE = ['#f38ba8', '#fab387', '#f9e2af', '#a6e3a1', '#94e2d5', '#89b4fa', '#cba6f7', '#f5c2e7'];
function openColorMenu(ev, current, onPick) {
  closeColorMenu();
  const menu = document.createElement('div');
  menu.className = 'color-menu';
  menu.id = 'color-menu';
  for (const c of PALETTE) {
    const sw = document.createElement('button');
    sw.className = 'color-swatch' + (current === c ? ' sel' : '');
    sw.style.background = c;
    sw.title = c;
    sw.onclick = (e) => { e.stopPropagation(); closeColorMenu(); onPick(c); };
    menu.appendChild(sw);
  }
  const none = document.createElement('button');
  none.className = 'color-swatch none' + (current ? '' : ' sel');
  none.textContent = '⌀'; none.title = 'no color';
  none.onclick = (e) => { e.stopPropagation(); closeColorMenu(); onPick(null); };
  menu.appendChild(none);
  document.body.appendChild(menu);
  // position near the cursor, clamped to the viewport
  const mw = menu.offsetWidth, mh = menu.offsetHeight;
  menu.style.left = Math.min(ev.clientX, window.innerWidth - mw - 8) + 'px';
  menu.style.top = Math.min(ev.clientY, window.innerHeight - mh - 8) + 'px';
  setTimeout(() => {
    const off = (e) => { if (!menu.contains(e.target)) { closeColorMenu(); document.removeEventListener('mousedown', off); } };
    document.addEventListener('mousedown', off);
  }, 0);
}
function closeColorMenu() { const m = document.getElementById('color-menu'); if (m && m.parentNode) m.parentNode.removeChild(m); }

function setProjectColor(id, color) {
  const v = views.get(id);
  if (!v) return;
  v.color = color;
  renderSidebar();
  api.setSessionColor(id, color).catch(() => {});
}

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

// Inline-edit a tmux-window tab's title. Sends the new name to tmux (rename-window, which also
// pins it by disabling automatic-rename); an empty value clears the manual name so it follows the
// pane title again. tmux echoes %window-renamed, which updates tab.title authoritatively.
function startTabRename(v, wid, labelEl) {
  const tab = v.tabs.get(wid);
  if (!tab) return;
  const current = tab.title && tab.title !== wid ? tab.title : '';
  const input = document.createElement('input');
  input.type = 'text';
  input.value = current;
  input.className = 'tab-rename-input';
  labelEl.textContent = '';
  labelEl.appendChild(input);
  input.focus();
  input.select();
  input.onclick = (e) => e.stopPropagation();   // don't switch tabs while editing

  let done = false;
  const commit = (save) => {
    if (done) return; done = true;
    if (save) {
      const next = input.value.trim();
      if (next !== current) api.tabRename(v.meta.id, wid, next);
    }
    renderTabs(v);   // repaint; tmux's %window-renamed echo will settle the final label
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
    // Don't overwrite the saved last-tab from tmux's initial active event while a restore is still
    // pending — otherwise we'd clobber the tab we're about to restore to.
    if (!v.restoreTab) rememberLastTab(v, window);
  }
  if (Array.isArray(order)) v.tabOrder = order;      // keep tab strip in tmux's window order
  // §20: on first connect, reveal the saved last-active tab once it exists (one-shot). Uses a
  // separate `restoreTab` snapshot so the live `lastTab` updates above don't clobber the target.
  if (v.restoreTab && v.tabs.has(v.restoreTab)) {
    const target = v.restoreTab;
    v.restoreTab = null;
    if (target !== v.activeWindow) switchTab(v, target);
  }
  if (id === activeId) renderTabs(v);
});

// Lazy scrollback back-fill: ask the backend to capture a window's screen the first time it is
// shown (the §14 "others on focus" decision). Idempotent per tab.
function lazyCapture(v, winId) {
  const tab = v.tabs.get(winId);
  if (tab && !tab.backfilled) { tab.backfilled = true; api.tabCapture(v.meta.id, winId); }
}

// Compute the tab display order: the user's saved custom order first (§20), then any tabs not in
// it (new windows) in tmux's window order, then anything else (viewer tabs) in insertion order.
function tabDisplayOrder(v) {
  const order = [];
  for (const w of (v.savedTabOrder || [])) if (v.tabs.has(w) && !order.includes(w)) order.push(w);
  for (const w of (v.tabOrder || [])) if (v.tabs.has(w) && !order.includes(w)) order.push(w);
  for (const w of v.tabs.keys()) if (!order.includes(w)) order.push(w);
  return order;
}

// Render the tab strip for the active control-mode project (hidden for plain/single).
function renderTabs(v) {
  if (!v || v.meta.mode !== 'control') { tabsEl.className = ''; tabsEl.innerHTML = ''; return; }
  tabsEl.className = 'on';
  tabsEl.innerHTML = '';
  for (const wid of tabDisplayOrder(v)) {
    const tab = v.tabs.get(wid);
    const el = document.createElement('div');
    el.className = 'tab' + (wid === v.activeWindow ? ' active' : '');
    const color = v.tabColors[wid];
    if (color) { el.style.setProperty('--tab-color', color); el.classList.add('has-color'); }
    el.innerHTML = `<span class="tlabel" title="double-click to rename">${escapeHtml(tab.title || wid)}</span><span class="tclose" title="close">×</span>`;
    const label = el.querySelector('.tlabel');
    label.onclick = () => switchTab(v, wid);
    // Double-click a real tmux-window tab to rename it. A manual rename sticks (tmux disables
    // automatic-rename for that window); clearing it re-enables auto-rename.
    if (isWindowTab(wid)) label.ondblclick = (e) => { e.stopPropagation(); startTabRename(v, wid, label); };
    el.querySelector('.tclose').onclick = (e) => { e.stopPropagation(); closeTab(v, wid); };
    // §20: right-click a tab -> color palette; drag to reorder within the strip.
    el.oncontextmenu = (e) => { e.preventDefault(); openColorMenu(e, v.tabColors[wid], (c) => setTabColor(v, wid, c)); };
    wireTabDnD(el, v, wid);
    tabsEl.appendChild(el);
  }
  // '+' new session in this project
  const plus = document.createElement('div');
  plus.className = 'tab plus'; plus.textContent = '+'; plus.title = 'New session in this project';
  plus.onclick = () => api.tabNew(v.meta.id);
  tabsEl.appendChild(plus);
}

function setTabColor(v, wid, color) {
  if (color) v.tabColors[wid] = color; else delete v.tabColors[wid];
  renderTabs(v);
  api.setTabPrefs(v.meta.id, null, [wid, color || null]).catch(() => {});
}

// §20: tab drag-and-drop reorder (horizontal). Persists the custom order for this project.
let _dragTab = null;
function wireTabDnD(el, v, wid) {
  el.draggable = true;
  el.ondragstart = (e) => { _dragTab = wid; el.classList.add('dragging'); try { e.dataTransfer.effectAllowed = 'move'; } catch (_) {} };
  el.ondragend = () => { _dragTab = null; el.classList.remove('dragging'); tabsEl.querySelectorAll('.drop-before,.drop-after').forEach((n) => n.classList.remove('drop-before', 'drop-after')); };
  el.ondragover = (e) => {
    if (_dragTab == null || _dragTab === wid) return;
    e.preventDefault();
    const r = el.getBoundingClientRect();
    const after = e.clientX > r.left + r.width / 2;
    el.classList.toggle('drop-after', after);
    el.classList.toggle('drop-before', !after);
  };
  el.ondragleave = () => el.classList.remove('drop-before', 'drop-after');
  el.ondrop = (e) => {
    e.preventDefault();
    const after = el.classList.contains('drop-after');
    el.classList.remove('drop-before', 'drop-after');
    if (_dragTab != null && _dragTab !== wid) reorderTab(v, _dragTab, wid, after);
  };
}

function reorderTab(v, dragWid, targetWid, after) {
  const order = tabDisplayOrder(v).filter((w) => w !== dragWid);
  const at = order.indexOf(targetWid) + (after ? 1 : 0);
  order.splice(at, 0, dragWid);
  // Persist + track only real tmux-window ids (viewer tabs are app-local, never restored), so the
  // in-memory savedTabOrder can't diverge from what's stored.
  const windowOrder = order.filter(isWindowTab);
  v.savedTabOrder = windowOrder;
  renderTabs(v);
  api.setTabPrefs(v.meta.id, windowOrder, null).catch(() => {});
}

// Switch the active tab. For a tmux window, tell tmux to select it (its reconcile echoes an
// 'active' event; we reveal now for responsiveness). A viewer tab is app-local — reveal it WITHOUT
// any tmux command.
function switchTab(v, winId) {
  if (v.activeWindow === winId) return;
  v.activeWindow = winId;
  if (isWindowTab(winId)) { api.tabSelect(v.meta.id, winId); lazyCapture(v, winId); rememberLastTab(v, winId); }
  showActiveTab(v);
  renderTabs(v);
}

// §20: persist a project's last-active tab so it's restored when the project is reopened.
function rememberLastTab(v, winId) {
  if (!isWindowTab(winId) || v.lastTab === winId) return;
  v.lastTab = winId;
  api.setLastTab(v.meta.id, winId).catch(() => {});
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
  // §18: on (re)connect, refresh the forwarded-port status (active/inactive).
  refreshTunnels(id);
});
// §18: the backend pushes the updated forwarded-port list; mirror it into the view + sidebar.
api.onTunnels(({ id, tunnels }) => {
  dbg('onTunnels id=' + id + ' tunnels=' + JSON.stringify(tunnels) + ' hasView=' + views.has(id));
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
const fHost = document.getElementById('f-host');
const fControl = document.getElementById('f-control');   // native-mode toggle button
const hostHistoryEl = document.getElementById('host-history');

// Native mode is a toggle; default ON. Click flips it.
function setNative(on) { fControl.classList.toggle('on', !!on); fControl.setAttribute('aria-checked', on ? 'true' : 'false'); }
fControl.onclick = () => setNative(!fControl.classList.contains('on'));

const updateFields = () => { remoteFields.style.display = fKind.value === 'remote' ? 'block' : 'none'; };
fKind.onchange = updateFields;

document.getElementById('new').addEventListener('click', () => {
  document.getElementById('f-err').textContent = '';
  setNative(true);                    // default to native mode each time the dialog opens
  updateFields();
  hideHostHistory();
  dialog.showModal();
});

// --- host history dropdown ---
let _hostHistory = [];
function hideHostHistory() { hostHistoryEl.className = ''; hostHistoryEl.innerHTML = ''; }
async function showHostHistory() {
  try { _hostHistory = await api.listHosts(); } catch (_) { _hostHistory = []; }
  const typed = fHost.value.trim().toLowerCase();
  const items = _hostHistory.filter((h) => !typed || h.toLowerCase().includes(typed));
  if (!items.length) { hideHostHistory(); return; }
  hostHistoryEl.innerHTML = '';
  items.forEach((h) => {
    const li = document.createElement('li');
    li.textContent = h;
    // mousedown (not click) so it fires before the input's blur hides the list.
    li.onmousedown = (e) => { e.preventDefault(); fHost.value = h; hideHostHistory(); fHost.focus(); };
    hostHistoryEl.appendChild(li);
  });
  hostHistoryEl.className = 'on';
}
fHost.addEventListener('focus', showHostHistory);
fHost.addEventListener('click', showHostHistory);
fHost.addEventListener('input', showHostHistory);
fHost.addEventListener('blur', () => setTimeout(hideHostHistory, 120));   // allow mousedown to land

document.getElementById('form').addEventListener('submit', async (e) => {
  // form method=dialog closes automatically; only act on OK
  const ok = e.submitter && e.submitter.value === 'ok';
  if (!ok) return;
  const kind = fKind.value;
  const host = fHost.value.trim();
  if (kind === 'remote' && !host) {   // guard: remote needs a host
    e.preventDefault();               // keep the dialog open
    document.getElementById('f-err').textContent = 'Enter a host (user@host).';
    return;
  }
  const meta = {
    kind,
    transport: 'ssh',   // ssh is the only remote transport
    mode: fControl.classList.contains('on') ? 'control' : 'plain',
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
  // §18/§20: load config (loopback hosts + last-active project) before wiring.
  let lastActive = null;
  try {
    const cfg = await api.getConfig();
    if (cfg && Array.isArray(cfg.loopbackHosts)) loopbackHosts = cfg.loopbackHosts;
    if (cfg && cfg.lastActive) lastActive = cfg.lastActive;
  } catch (_) {}
  const persisted = await api.listSessions();
  dbg('init: ' + persisted.length + ' persisted; lastActive=' + lastActive);
  for (const meta of persisted) {
    // meta already has {id, host, session, transport, title, color, lastTab, tabOrder, tabColors}.
    const v = makeView({ ...meta, kind: 'remote' });
    v.started = false;   // not connected yet; mount() will start (reattach) on click
  }
  renderSidebar();
  // §18: show persisted forwarded ports for every restored session up front (greyed until
  // re-opened), so the list survives an app restart without waiting for a connect.
  for (const meta of persisted) refreshTunnels(meta.id);
  setStatus(persisted.length ? `${persisted.length} session(s) restored — click to reconnect` : 'no sessions — create one');
  // §20: reopen the LAST-USED project (fall back to the first) so the app resumes where you left off.
  // The project restores its own last-active tab once its windows arrive (see onWindow).
  if (persisted.length) {
    const target = (lastActive && views.has(lastActive)) ? lastActive : persisted[0].id;
    mount(target);
  }
})();

// §18: periodically re-probe the active session's tunnels so a stopped dev server goes grey (and
// a restarted one goes active) without a manual refresh. Light: one call every 5s for the shown one.
setInterval(() => { if (activeId != null && views.has(activeId)) refreshTunnels(activeId); }, 5000);
