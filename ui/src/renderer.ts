
// Renderer: sidebar + one xterm view, wired to the main process over window.terminalAPI.
// The terminal engine (xterm.js) is deliberately kept behind a thin usage so the transport
// contract (data/input/resize/ack/state) is what the rest of the UI depends on (DESIGN §7).
import { PluginRegistry } from './plugins.js';
import type { LinkContext } from './plugins.js';
import * as DTBuiltinPlugins from './builtinPlugins.js';
import {
  armTerminalInputLatency,
  createTerminalTab,
  getTerminalInputLatency,
  getTerminalRepaintCount,
} from './terminalTab.js';
import type { TerminalTabContext, TerminalTabSpec } from './terminalTab.js';
import { createFileViewerTab } from './fileViewerTab.js';
import type { FileViewerTabContext, FileViewerTabSpec } from './fileViewerTab.js';
import type {
  CreateSessionMeta,
  OscNotificationParser,
  RecoveryTabSnapshot,
  SessionMeta,
  SessionState,
  TabContent,
  TuiActivityTracker,
  TunnelInfo,
} from './types.js';

const DTPlugins = { PluginRegistry };
const DTTerminalTab = { createTerminalTab };
const DTFileViewerTab = { createFileViewerTab };

function requiredElement<T extends HTMLElement>(id: string): T {
  const element = document.getElementById(id);
  if (!element) throw new Error(`missing required element #${id}`);
  return element as T;
}

function requiredDescendant<T extends Element>(parent: ParentNode, selector: string): T {
  const element = parent.querySelector(selector);
  if (!element) throw new Error(`missing required descendant ${selector}`);
  return element as T;
}

type RenameSelection = [number | null, number | null];

interface AppTab {
  winId: string;
  title: string;
  content: TabContent;
  mounted: boolean;
  viewer?: boolean;
  unreadNotification?: boolean;
  notificationParser?: OscNotificationParser;
  tuiTracker?: TuiActivityTracker;
  tuiReportedActive?: boolean;
  pre?: string[] | null;
  backfilled?: boolean;
  closing?: boolean;
  renaming?: boolean;
  renameDraft?: string | null;
  renameSel?: RenameSelection | null;
  renameFocus?: boolean;
  commandDraft?: string;
  lastCommand?: string;
}

interface View {
  meta: SessionMeta;
  state: SessionState;
  started: boolean;
  inputReady: boolean;
  tabs: Map<string, AppTab>;
  activeWindow: string | null;
  el: HTMLDivElement | null;
  tmuxVersion: number[] | undefined;
  tunnels: TunnelInfo[];
  color: string | null;
  savedTabOrder: string[];
  tabOrder?: string[];
  tabColors: Record<string, string>;
  lastTab: string | null;
  restoreTab: string | null;
  linkMap: Map<string, string>;
  pending?: string[] | null;
  renaming?: boolean;
  renameDraft?: string | null;
  renameSel?: RenameSelection | null;
  renameFocus?: boolean;
  remoteOpen?: boolean | null;
}

type DragAxis = 'x' | 'y';
interface DragState {
  el: HTMLElement;
  container: HTMLElement;
  items: HTMLElement[];
  axis: DragAxis;
  commit(from: number, to: number): void;
  from: number;
  to: number;
  startX: number;
  startY: number;
  started: boolean;
  pointerId: number;
  rects: DOMRect[];
  slot: number;
  onMove: (event: PointerEvent) => void;
  onUp: (event: PointerEvent) => void;
  onCancel: () => void;
}

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

const api = window.terminalAPI;
const sessionsEl = requiredElement<HTMLElement>('sessions');
const historyEl = requiredElement<HTMLElement>('history');
const historyPanel = requiredElement<HTMLElement>('history-panel');
const statusEl = requiredElement<HTMLElement>('status');
const termHost = requiredElement<HTMLElement>('term');
const recoverButton = requiredElement<HTMLButtonElement>('recover');

function trackCommandInput(tab: AppTab | null, data: string): void {
  if (!tab || !data) return;
  // Remove terminal protocol and arrow-key escape sequences. Human text and paste content remain;
  // xterm device replies must never become a recoverable shell command.
  const text = data
    .replace(/\x1b\][^\x07]*(?:\x07|\x1b\\)/g, '')
    .replace(/\x1b\[[0-?]*[ -/]*[@-~]/g, '');
  let draft = tab.commandDraft || '';
  for (const character of Array.from(text)) {
    if (character === '\r' || character === '\n') {
      const command = draft.trim();
      if (command) tab.lastCommand = command.slice(0, 4096);
      draft = '';
    } else if (character === '\x7f' || character === '\b') {
      draft = Array.from(draft).slice(0, -1).join('');
    } else if (character === '\x15' || character === '\x03') {
      draft = '';
    } else if (character >= ' ' && character !== '\x7f') {
      draft += character;
      if (draft.length > 4096) draft = draft.slice(-4096);
    }
  }
  tab.commandDraft = draft;
}

// §22: console gate overlay — a blurred, non-interactive scrim shown while the active session is
// connecting or its link is broken, so the user can't type into a session that can't receive it.
const termGate = document.createElement('div');
termGate.className = 'term-gate';
const termGateBadge = document.createElement('div');
termGateBadge.className = 'gate-badge';
termGate.appendChild(termGateBadge);
termHost.appendChild(termGate);

// Reconnect in-flight guard (§5): every manual reconnect path (gate badge, sidebar retry, force
// reconnect) tears down the backend and spawns a fresh ssh. Two overlapping spawns evict each other
// via `new-session -D`, so a double-click used to cause the very flap the supervisor tries to avoid.
// Cleared when the session next reports a state (connected/reconnecting/dead) or after a timeout, so
// a lost event can't wedge the control permanently.
const reconnectPending = new Map<string, ReturnType<typeof setTimeout>>();   // id -> timeout handle
function reconnectBusy(id: string): boolean { return reconnectPending.has(id); }
function markReconnecting(id: string): void {
  clearReconnect(id);
  reconnectPending.set(id, setTimeout(() => reconnectPending.delete(id), 10000));
}
function clearReconnect(id: string): void {
  const t = reconnectPending.get(id);
  if (t !== undefined) { clearTimeout(t); reconnectPending.delete(id); }
}

// §22: when a session has given up auto-reconnecting (Dead, after the attempt cap), the center
// badge becomes a Reconnect button — clicking it calls the backend's manual retry (fresh budget).
termGateBadge.addEventListener('click', () => {
  const v = activeId != null ? views.get(activeId) : null;
  if (v && v.state === 'dead' && !reconnectBusy(v.meta.id)) {
    markReconnecting(v.meta.id);
    api.retry(v.meta.id);
    renderSidebar();
  }
});

// §22: is the console fully "live" — connected AND (control mode) past the attach settle? When
// false we blur the console. This is the VISUAL gate.
function isConsoleLive(v: View | null | undefined): boolean {
  if (!v) return false;
  if (v.meta.mode === 'control') return v.state === 'connected' && v.inputReady;
  return v.state === 'connected' || v.state === 'idle';   // plain/local: no reconnect lifecycle
}

// Whether keystrokes should be DROPPED (vs. forwarded to the backend). Note this is intentionally
// NARROWER than isConsoleLive: during a control session's *initial* connect the backend already
// buffers input and replays it on ready, so we let it through and only drop when the link is
// genuinely broken (reconnecting after a drop / dead / closed) — where the backend would silently
// discard it anyway and a replay-later surprise is unwanted.
function shouldDropInput(v: View | null | undefined): boolean {
  if (!v) return true;
  return v.state === 'reconnecting' || v.state === 'dead' || v.state === 'closed';
}

// Reflect the ACTIVE session's connection state onto the console (blur while not live).
function updateConsoleGate() {
  const v = activeId != null ? views.get(activeId) : null;
  const gated = !!v && !isConsoleLive(v);
  termHost.classList.toggle('gated', gated);
  termHost.classList.toggle('dead', gated && v && (v.state === 'dead' || v.state === 'closed'));
  // The badge is a clickable Reconnect button only when Dead (auto-reconnect exhausted).
  const dead = gated && v && v.state === 'dead';
  termGateBadge.classList.toggle('clickable', !!dead);
  if (gated) {
    const label = v.state === 'dead' ? '⟳ Reconnect'
      : v.state === 'closed' ? 'disconnected'
      : v.state === 'reconnecting' ? 'reconnecting…'
      : 'connecting…';
    termGateBadge.textContent = label;
    // Never leave a blurred terminal holding focus/keystrokes while the link is broken.
    if (shouldDropInput(v)) {
      const t = activeTab(v);
      if (t && t.content && t.content.term) { try { t.content.term.blur(); } catch (_) {} }
    }
  }
}

const views = new Map<string, View>();   // id -> project view
let activeId: string | null = null;
let _lastSize = { cols: 0, rows: 0 };   // last size sent for the active view (resize debounce)

// --- plugin framework (§13): link matchers turn URLs/paths (and custom patterns) into
// clickable links. URL + path are built-in plugins; third parties register more via
// window.dtPlugins.registerLink({ name, regex, onClick(text, ctx) }). ---
const registry = new DTPlugins.PluginRegistry();
DTBuiltinPlugins.builtinLinkPlugins().forEach((p) => registry.registerLink(p));
// Built-in 'terminal' tab-kind (§14/§15): tabs are polymorphic content providers, so future
// kinds (markdown, browser, ...) register the same way with no renderer changes.
registry.registerTabKind({
  kind: 'terminal',
  create: (spec, ctx) => DTTerminalTab.createTerminalTab(spec as TerminalTabSpec, ctx as TerminalTabContext),
});
// 'fileviewer' tab-kind (§16): app-local tab (no tmux window) that previews a clicked path.
registry.registerTabKind({
  kind: 'fileviewer',
  create: (spec, ctx) => DTFileViewerTab.createFileViewerTab(spec as FileViewerTabSpec, ctx as FileViewerTabContext),
});
// Public extension API (stable surface for user/third-party plugins).
window.dtPlugins = {
  registerLink: (p) => registry.registerLink(p),   // returns an unregister() fn
  registerTabKind: (p) => registry.registerTabKind(p),   // { kind, create(spec,ctx) } -> unregister
};

// Debug -> main -> /tmp/dt-debug.log + browser console (so both are available for diagnosis).
function dbg(...a: unknown[]): void {
  const msg = a.map((x) => (typeof x === 'string' ? x : JSON.stringify(x))).join(' ');
  try { console.log('[DT ui]', msg); } catch (_) {}
  try { api.log(msg); } catch (_) {}
}

function setStatus(t: string): void { setStatusRaw(t); }
function setStatusRaw(t: string): void { statusEl.textContent = t; }

// §18: loopback host set (from the host config; default localhost/127.0.0.1). Loaded at startup.
let loopbackHosts = ['localhost', '127.0.0.1'];
// Does this URL point at a configured remote-loopback host (so it needs an ssh -L tunnel)?
function isLoopbackUrl(url: string): boolean {
  const m = /^(?:https?:\/\/)?([^\s/:]+):(\d{1,5})(?:[/?]|$)/.exec(url);
  const host = m?.[1];
  const port = Number(m?.[2]);
  return !!(host && loopbackHosts.includes(host) && port >= 1 && port <= 65535);
}

// A VIEW is a project: one connection, potentially many tabs (tmux windows, §14). Each tab is
// a polymorphic TabContent (§15) — today always a 'terminal'. A single-window project behaves
// exactly like the old single-session view.
function makeView(meta: SessionMeta): View {
  const v: View = {
    meta, state: 'idle', started: false,
    inputReady: meta.mode !== 'control',   // non-control ready immediately
    tabs: new Map<string, AppTab>(),        // winId '@N' -> tab
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
    // §21: OSC 8 file-link map — display text (e.g. "README.md") -> absolute remote path harvested
    // from the raw output stream (xterm drops the hyperlink from scrollback, so we capture it here).
    linkMap: new Map<string, string>(),
  };
  views.set(meta.id, v);
  // For non-control (plain/local) there are no tmux window events; use a single implicit tab.
  if (meta.mode !== 'control') ensureTab(v, '@single');
  // Flush any data that arrived before this view existed (reconnect race).
  const pending = pendingData[meta.id];
  if (pending) { v.pending = (v.pending || []).concat(pending); delete pendingData[meta.id]; }
  return v;
}

// Create (once) a tab for a window id, backed by a 'terminal' TabContent via the registry.
function ensureTab(v: View, winId: string): AppTab {
  const existing = v.tabs.get(winId);
  if (existing) return existing;
  let tab: AppTab;
  const { provider: linkProvider, linkHandler } = makeLinkProvider(() => tab.content.term, v.meta);
  const ctx = {
    // Forward keystrokes to the backend, which owns the real input gating (control mode buffers
    // during the initial attach settle and replays on ready). We only DROP input when the link is
    // genuinely broken (reconnecting/dead/closed) — where it would be silently discarded anyway —
    // so the blurred-but-broken console can't swallow keystrokes (§22).
    // Address input to the xterm/tmux window that generated it. Besides keyboard input, xterm's
    // onData carries protocol replies (OSC colour queries, device attributes, focus events). A
    // tab switch updates the UI before tmux's echoed active-window event arrives, so relying on
    // the session-wide active window can deliver those replies to the neighbouring tab.
    input: (data: string) => {
      if (shouldDropInput(v)) return;
      trackCommandInput(tab, data);
      void api.input(v.meta.id, data, winId);
    },
    ack: (bytes: number) => api.ack(v.meta.id, bytes),
    // Clipboard: xterm ignores OSC 52 by default and there's no built-in Cmd+C, so the terminal
    // tab wires both through here. copyText -> system clipboard; setStatus -> status line feedback.
    copyText: (text: string) => void api.copyText(text),
    setStatus: (m: string) => setStatus(m),
    // A standalone terminal BEL is Codex's zero-config fallback when its auto notification backend
    // does not recognize the terminal. xterm consumes BEL, so receive it through term.onBell.
    onBell: () => markTabNotification(v, tab),
    // Pointer, keyboard, and paste gestures inside the terminal explicitly acknowledge it.
    onInteract: () => acknowledgeTerminalInteraction(v, tab),
  };
  const content = registry.createTabContent('terminal', { id: v.meta.id, meta: v.meta, linkProvider, linkHandler }, ctx);
  tab = {
    winId, title: winId, content, mounted: false,
    unreadNotification: false,
    lastCommand: v.meta.recoveryTabs?.find((saved) => saved.window === winId)?.lastCommand || '',
    // Notification OSCs can span PTY chunks, so each terminal/window owns a streaming parser.
    // Keeping it on the tab also prevents an unfinished OSC in one tmux window from consuming
    // bytes from another window.
    notificationParser: DTBuiltinPlugins.createOscNotificationParser(),
    // TUI repaint state is per tmux window: one Claude/vim tab must not mark sibling shells active.
    tuiTracker: DTBuiltinPlugins.createTuiActivityTracker(),
    tuiReportedActive: false,
  };
  v.tabs.set(winId, tab);
  return tab;
}

// A real tmux window id ('@N'). Viewer tabs use synthetic 'view:N' ids and must NOT drive tmux
// window commands (select-window/kill-window) — gate every such call on this.
function isWindowTab(winId: string): boolean { return /^@\d+$/.test(winId); }

let _viewerSeq = 0;

// §21: scan a raw output chunk for OSC 8 file:// hyperlinks and record display-text -> absolute
// remote path in the project's linkMap. xterm strips the hyperlink from scrollback cells, so this
// raw-stream scan (BEFORE term.write) is our only capture point. Agents (Claude Code) emit the
// absolute path in the URI while the display text is a short/relative path.
function harvestOsc8FileLinks(v: View | null | undefined, data: string): void {
  if (!v) return;
  for (const { shown, path } of DTBuiltinPlugins.extractOsc8FileLinks(data)) {
    v.linkMap.set(shown, path);   // newest wins (a path can move; last seen is freshest)
  }
  // Bound memory on a long-lived session: drop oldest entries past a cap.
  while (v.linkMap.size > 500) {
    const oldest = v.linkMap.keys().next();
    if (oldest.done) break;
    v.linkMap.delete(oldest.value);
  }
}

// Scan raw output BEFORE xterm consumes it. A notification belongs to the tmux window/tab that
// emitted it; the session card derives its dot from whether ANY child tab remains unread.
function harvestOscNotifications(v: View, tab: AppTab | null | undefined, data: string): void {
  if (!v || !tab || !tab.notificationParser) return;
  if (tab.notificationParser.write(data) < 1) return;
  markTabNotification(v, tab);
}

// Observe repaint-in-place activity on the resolved tab. Expiry is deliberately passive: the next
// output after the 10s decay records the lapsed transition, avoiding a wake-up timer per tab solely
// for diagnostics. A future live consumer can query tuiTracker.active() at its own scheduling seam.
function trackTuiActivity(v: View, tab: AppTab, data: string): void {
  const tracker = tab.tuiTracker;
  if (!tracker) return;
  if (tab.tuiReportedActive && !tracker.active()) {
    tab.tuiReportedActive = false;
    dbg(`tui inactive session=${v.meta.id} window=${tab.winId}`);
  }
  if (tracker.write(data) && !tab.tuiReportedActive) {
    tab.tuiReportedActive = true;
    dbg(`tui active session=${v.meta.id} window=${tab.winId}`);
  }
}

// Common endpoint for explicit notification OSCs and xterm's standalone BEL event. BEL is the
// standards-based fallback used by Codex in `auto` mode; keeping the state transition here gives it
// the same per-tab ownership and acknowledgement behavior as OSC 9/99/777.
function markTabNotification(v: View, tab: AppTab): void {
  if (!v || !tab || tab.unreadNotification) return;
  // The visible tab already has the user's attention. Consume the event without manufacturing an
  // unread state that can only be cleared by leaving and returning to the same tab.
  if (v.meta.id === activeId && activeTab(v) === tab) return;
  tab.unreadNotification = true;
  renderSidebar();
  if (v.meta.id === activeId) renderTabs(v);
}

function sessionHasUnreadNotification(v: View): boolean {
  for (const [, tab] of v.tabs) if (tab.unreadNotification) return true;
  return false;
}

// A read acknowledgement is deliberately user-driven. Merely receiving output, restoring the
// last active tab, or tmux changing its active window must not clear a dot behind the user's back.
function clearTabNotification(v: View, tab: AppTab | null | undefined): boolean {
  if (!tab || !tab.unreadNotification) return false;
  tab.unreadNotification = false;
  renderSidebar();
  if (v.meta.id === activeId) renderTabs(v);
  return true;
}

// Terminal activity acknowledges only the terminal the user can currently see. xterm also emits
// automatic protocol replies through onData, so terminalTab reports real DOM gestures separately.
function acknowledgeTerminalInteraction(v: View | null | undefined, tab: AppTab | null | undefined): boolean {
  if (!v || !tab || v.meta.id !== activeId || activeTab(v) !== tab) return false;
  return clearTabNotification(v, tab);
}

// Open a file-viewer tab for a clicked path (§16). App-local tab (no tmux window): synthetic id,
// tmux commands are gated off it. Fetches its own content on mount.
function openViewer(sessionId: string, path: string): void {
  const v = views.get(sessionId) || (activeId ? views.get(activeId) : undefined);
  if (!v) return;
  const winId = 'view:' + (++_viewerSeq);
  const ctx = { setStatus: (m: string) => setStatus(m) };
  const content = registry.createTabContent('fileviewer',
    { id: v.meta.id, path, api }, ctx);
  const tab: AppTab = { winId, title: baseName(path), content, mounted: false, viewer: true };
  v.tabs.set(winId, tab);
  v.activeWindow = winId;
  if (v.meta.id === activeId) { showActiveTab(v); renderTabs(v); }
  setStatus('opening ' + baseName(path) + '…');
}

function baseName(p: string): string { return (String(p).split('/').pop() || 'file'); }

// §18: pull the authoritative forwarded-port status (persisted + live, each probed) into the view
// and re-render the sidebar. Safe to call on mount, reconnect, or a periodic tick.
function refreshTunnels(id: string): void {
  api.listTunnels(id).then((t) => {
    const v = views.get(id);
    if (!v) return;
    v.tunnels = Array.isArray(t) ? t : [];
    renderSidebar();
  }).catch(() => {});
}

// Generic action-chooser modal: a title + a list of [label, fn] buttons, dismissed on
// backdrop-click or Escape. Reused by the URL chooser (§18) and the file chooser (§21).
type ChooserItem = readonly [label: string, action: () => unknown];

function showChooser(titleText: string, items: readonly ChooserItem[]): void {
  const back = document.createElement('div');
  back.className = 'chooser-back';
  const box = document.createElement('div');
  box.className = 'chooser';
  const title = document.createElement('div'); title.className = 'chooser-title'; title.textContent = titleText;
  box.appendChild(title);
  // Unregister the key listener in close() itself, not in the Escape branch: close() is also reached
  // from the backdrop and from every item button, and those paths used to leave the listener (and the
  // DOM nodes it captured) attached to `document` forever — one leak per chooser use.
  const esc = (e: KeyboardEvent) => { if (e.key === 'Escape') close(); };
  const close = () => {
    document.removeEventListener('keydown', esc);
    if (back.parentNode) back.parentNode.removeChild(back);
  };
  items.forEach(([label, fn]) => {
    const b = document.createElement('button');
    b.className = 'chooser-item'; b.textContent = label;
    b.onclick = () => { close(); try { fn(); } catch (_) {} };
    box.appendChild(b);
  });
  back.appendChild(box);
  back.onclick = (e) => { if (e.target === back) close(); };
  document.addEventListener('keydown', esc);
  document.body.appendChild(back);
}

// §18: Shift+Cmd chooser — pick where to open a URL. Loopback URLs offer tunnel-open; all URLs
// offer copy and open-plain.
function chooseOpen(sessionId: string, url: string): void {
  const loop = isLoopbackUrl(url);
  const items: ChooserItem[] = [];
  if (loop) items.push(['Open in local browser (tunnel)', () => api.openForwardedUrl(sessionId, url)]);
  items.push(['Open in browser', () => api.openExternal(loop && !/^https?:\/\//.test(url) ? 'http://' + url : url)]);
  items.push(['Copy URL', () => api.copyText(url)]);
  showChooser(url, items);
}

// §21: Shift+Cmd chooser for a remote FILE path (from an OSC 8 file:// link) — preview in-app or
// copy the absolute path. The path is on the REMOTE host, so there's no "open locally" option.
function chooseOpenFile(sessionId: string, path: string): void {
  showChooser(path, [
    ['Preview in app', () => openViewer(sessionId, path)],
    ['Copy path', () => api.copyText(path)],
  ]);
}

// The active tab (or the sole tab for single-window/plain views).
function activeTab(v: View): AppTab | null {
  if (v.activeWindow) {
    const current = v.tabs.get(v.activeWindow);
    if (current) return current;
  }
  const first = v.tabs.values().next();
  return first.done ? null : first.value;
}

// Build an xterm ILinkProvider that asks the plugin registry for matches on each line.
// getTerm() returns the tab's xterm lazily (the term is created inside the TabContent).
function makeLinkProvider(
  getTerm: () => XtermTerminal | undefined,
  meta: SessionMeta,
): { provider: XtermLinkProvider; linkHandler: { activate(event: MouseEvent, uri: string): void } } {
  const ctx: LinkContext = {
    meta,
    openExternal: (url: string) => void api.openExternal(url),
    copyText: (text: string) => void api.copyText(text),
    setStatus: (msg: string) => setStatus(msg),
    // §16/§21: open a clicked path in an in-app file-viewer tab. If the SAME display text was seen
    // as an OSC 8 file:// link (agents like Claude Code emit the absolute path that way), prefer
    // that authoritative absolute path — the bare relative text often can't be located from the
    // pane cwd alone (Claude's paths are relative to its project root, not the shell's cwd).
    openViewer: (path: string) => {
      const v = views.get(meta.id);
      const abs = v && v.linkMap.get(String(path).trim());
      openViewer(meta.id, abs || path);
    },
    // §18: is this URL a remote-loopback URL (needs an ssh -L tunnel to reach)?
    isLoopback: (url: string) => isLoopbackUrl(url),
    // §18: open a loopback URL via a tunnel (host forwards + opens the local URL).
    openForwardedUrl: async (url: string) => {
      dbg('openForwardedUrl click id=' + meta.id + ' url=' + url);
      setStatus('forwarding ' + url + '…');
      try {
        const res = await api.openForwardedUrl(meta.id, url);
        dbg('openForwardedUrl result=' + JSON.stringify(res));
        setStatus(res && res.localUrl ? ('opened ' + res.localUrl) : ('could not forward ' + url));
      } catch (e) { dbg('openForwardedUrl error=' + errorMessage(e)); setStatus('forward failed: ' + errorMessage(e)); }
    },
    // §18: Shift+Cmd chooser — where to open a URL.
    chooseOpen: (url: string) => chooseOpen(meta.id, url),
  };
  const provider: XtermLinkProvider = {
    provideLinks(lineNumber: number, callback: (links: XtermLink[] | undefined) => void) {
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
        activate: (event: MouseEvent) => {
          // Pass modifier state so handlers can offer a chooser (Shift+Cmd) vs the smart default.
          const mods = { shift: !!(event && event.shiftKey), meta: !!(event && (event.metaKey || event.ctrlKey)), alt: !!(event && event.altKey) };
          try { m.plugin.onClick(m.text, ctx, mods); }
          catch (e) { setStatus('link handler error: ' + errorMessage(e)); }
        },
      }));
      callback(links);
    },
  };
  // §21: OSC 8 hyperlinks (`\e]8;;URI\e\\text\e]8;;\e\\`) are underlined natively by xterm, but the
  // click does NOTHING unless a linkHandler is set (default is null -> a blocked window.open in the
  // Tauri webview). Claude Code (and others) emit these for file-path tool calls with the ABSOLUTE
  // remote path as a file:// URI (display text is the short relative path). Route by scheme:
  //   file:// -> the path is on the REMOTE host, so preview it in-app via the ssh fetch (NOT
  //             openExternal, which would wrongly try to open it on the local Mac). Shift-click
  //             offers a chooser (preview / copy path).
  //   else    -> openUrlSmart (loopback tunnel / browser / URL chooser), unchanged.
  const linkHandler = {
    activate(event: MouseEvent, uri: string) {
      const mods = { shift: !!(event && event.shiftKey), meta: !!(event && (event.metaKey || event.ctrlKey)), alt: !!(event && event.altKey) };
      const filePath = DTBuiltinPlugins.parseFileUri(uri);
      if (filePath) {
        if (mods.shift) chooseOpenFile(meta.id, filePath);
        else openViewer(meta.id, filePath);
        return;
      }
      DTBuiltinPlugins.openUrlSmart(uri, ctx, mods);
    },
  };
  return { provider, linkHandler };
}

async function mount(id: string, userInitiated = false): Promise<void> {
  const v = views.get(id);
  if (!v) return;
  if (v.meta.archived) { await resumeSession(id); return; }
  v.meta.detached = false;

  // Ensure the project has a container in #term (one per project; tabs live inside it).
  if (!v.el) {
    v.el = document.createElement('div');
    v.el.style.width = '100%'; v.el.style.height = '100%';
    termHost.appendChild(v.el);
  }
  // Show this project's container, hide others (never re-open a live xterm — that blanks it).
  for (const [, other] of views) { if (other.el) other.el.style.display = (other === v) ? 'block' : 'none'; }

  // Claim active BEFORE any await: session:data/state/ready/window handlers all gate on
  // `id === activeId`, so leaving this until after the connect round-trip meant every event arriving
  // during the connect was dropped for UI purposes — the console gate and tab strip stayed stale
  // over a live terminal until some later unrelated event re-rendered them.
  activeId = id;
  // Non-control sessions have no inner tab strip to click. Treat clicking their session card as
  // viewing/acknowledging the sole implicit tab. Native-tab sessions keep each dot until that exact
  // tab header is clicked, so a session-level rollup never clears unrelated child notifications.
  if (userInitiated && v.meta.mode !== 'control') clearTabNotification(v, activeTab(v));
  // §20: restore-on-open target. Local sessions count too: they run under a local tmux whose server
  // outlives the app (§5.3b), so they are persisted and reopenable exactly like remote ones.
  api.setLastActive(id).catch(() => {});
  showActiveTab(v);        // mount + reveal the active tab's content
  renderTabs(v);
  renderSidebar();
  updateConsoleGate();     // §22: blur/allow the console per this session's connection state
  // §18: pull the forwarded-port status (persisted + live, probed) for this session.
  refreshTunnels(id);

  // Connect the project once (reattaches the SAME tmux session; tmux replays windows -> tabs).
  if (!v.started) {
    v.started = true;
    // control mode: 'ready' arrives from the backend once attach settles (it buffers input until
    // then). inputReady here is a DISPLAY flag only — the backend owns the actual gating.
    if (v.meta.mode === 'control') v.inputReady = false;
    setStatus(`connecting ${v.meta.title || v.meta.host || 'session'}…`);
    dbg('mount->createSession id=' + v.meta.id + ' host=' + v.meta.host + ' session=' + v.meta.session + ' mode=' + v.meta.mode + ' tmuxPath=' + v.meta.tmuxPath + ' tmuxVersion=' + JSON.stringify(v.meta.tmuxVersion));
    // createSession REJECTS on a spawn failure (bad host, ssh missing). Unhandled, that rejection
    // escaped this async fn and left the UI on "connecting…" forever with no error shown, since the
    // backend has no session:error event to fall back on.
    try {
      const createMeta: CreateSessionMeta = {
        id: v.meta.id, kind: v.meta.kind || 'remote', transport: v.meta.transport,
        host: v.meta.host, session: v.meta.session, title: v.meta.title ?? '', mode: v.meta.mode,
      };
      if (v.meta.tmuxPath) createMeta.tmuxPath = v.meta.tmuxPath;
      if (v.meta.tmuxVersion) createMeta.tmuxVersion = v.meta.tmuxVersion;
      const res = await api.createSession(createMeta);
      dbg('mount->createSession returned ' + JSON.stringify(res));
      // The backend may have re-probed (unproven tmux path) or downgraded control -> plain. Adopt
      // what it actually used so the next createSession doesn't re-send the stale pair, and so the
      // sidebar shows the real tmux version.
      if (res) {
        if (res.tmuxPath) v.meta.tmuxPath = res.tmuxPath;
        if (res.tmuxVersion) { v.meta.tmuxVersion = res.tmuxVersion; v.tmuxVersion = res.tmuxVersion; }
        if (res.mode && res.mode !== v.meta.mode) {
          dbg('mount->createSession mode changed ' + v.meta.mode + ' -> ' + res.mode);
          v.meta.mode = res.mode;
        }
        renderSidebar();
      }
    } catch (e) {
      v.started = false;          // allow a retry by clicking the session again
      v.state = 'dead';
      const msg = errorMessage(e) || 'unknown error';
      dbg('mount->createSession FAILED: ' + msg);
      setStatus(`failed to connect ${v.meta.title || v.meta.host || 'session'}: ${msg}`);
      if (id === activeId) updateConsoleGate();
      renderSidebar();
    }
  }
}

// Mount (if needed) and reveal the active tab's content; hide the project's other tabs.
function showActiveTab(v: View): void {
  const tab = activeTab(v);
  if (!tab || !v.el) return;
  if (!tab.mounted) {
    tab.content.mount(v.el);
    tab.mounted = true;
    dbg('mount tab ' + tab.winId + ' for project ' + v.meta.id);
    // flush data buffered before this tab was mounted (project-level + tab-level)
    if (v.pending && v.pending.length) { const b = v.pending; v.pending = null; b.forEach((d: string) => tab.content.onData(d)); }
    if (tab.pre && tab.pre.length) { const b = tab.pre; tab.pre = null; b.forEach((d: string) => tab.content.onData(d)); }
  }
  // Reveal with '' (clear the inline value), NOT 'block': an inline display:block OVERRIDES the
  // stylesheet, and the fileviewer's root is `display:flex` (.fv-root). Forcing it to block killed
  // the flex column, so `.fv-body { flex:1 }` no longer stretched and its iframe collapsed to the
  // CSS default 150px inside a 618px tab. Terminal tabs are plain divs, so '' resolves to the same
  // block they were already getting. Each tab kind's own CSS decides its display; this only toggles
  // VISIBILITY.
  for (const [, t] of v.tabs) { const el = t.content.element && t.content.element(); if (el) el.style.display = (t === tab) ? '' : 'none'; }
  requestAnimationFrame(() => {
    const size = tab.content.fit();
    // A same-size reveal does not make FitAddon dirty any rows. Force a full-buffer repaint after
    // layout settles so a pane that received writes while display:none cannot remain blank/stale.
    tab.content.repaintAllRows?.();
    // Always re-assert size on show (a switched-to session/tab must be told its dimensions), and
    // keep _lastSize in sync so the debounced window-resize handler's change check stays correct.
    let resizeResult = null;
    if (size) {
      resizeResult = api.resize(v.meta.id, size.cols, size.rows);
      if (v.meta.id === activeId) _lastSize = size;
    }
    // Capture only AFTER xterm is fitted and the backend has accepted the matching tmux resize.
    // Capturing earlier snapshots (often at the attach-time 80x24) and then painting them into a
    // larger xterm put the prompt on one row and tmux's real cursor on the next. Tauri invokes
    // resolve after resize() has queued refresh-client, so the following capture is FIFO behind it.
    const captureAfterResize = () => {
      if (isWindowTab(tab.winId)) lazyCapture(v, tab.winId);
    };
    if (resizeResult && typeof resizeResult.then === 'function') {
      resizeResult.then(captureAfterResize, captureAfterResize);
    } else {
      captureAfterResize();
    }
    // Don't focus a broken console — it must not capture keystrokes (§22). (A control session
    // still-settling on initial connect keeps focus so early input buffers as designed.)
    if (!shouldDropInput(v)) tab.content.focus();
  });
}

// Repaint only the visible pane when the app becomes visible/focused again. Hidden tabs receive the
// same recovery when showActiveTab reveals them, so walking every live terminal would be wasted work.
function repaintActivePane(): void {
  const v = activeId ? views.get(activeId) : undefined;
  if (!v) return;
  activeTab(v)?.content.repaintAllRows?.();
}

document.addEventListener('visibilitychange', () => {
  if (document.visibilityState === 'visible') repaintActivePane();
});
window.addEventListener('focus', () => repaintActivePane());

// Timers do not fire while the machine sleeps. A substantially late tick is therefore a wake signal
// even when the document stayed visible throughout suspend; defer until the webview paints again.
const WAKE_POLL_MS = 10_000;
const WAKE_GAP_MS = WAKE_POLL_MS + 5_000;
let lastWakeTick = Date.now();
setInterval(() => {
  const now = Date.now();
  if (now - lastWakeTick > WAKE_GAP_MS) requestAnimationFrame(() => repaintActivePane());
  lastWakeTick = now;
}, WAKE_POLL_MS);

function renderHistorySession(id: string, v: View): void {
  const li = document.createElement('li');
  li.className = 'session history-session';
  li.dataset.id = id;
  const closed = v.meta.archivedAt && Number.isFinite(v.meta.archivedAt)
    ? `Closed ${new Date(v.meta.archivedAt).toLocaleDateString(undefined, { month: 'short', day: 'numeric' })}`
    : 'Closed';
  const location = v.meta.host || 'local shell';
  li.innerHTML = `<span class="status-dots"><span class="dot closed"></span></span>
    <span class="body">
      <span class="name-row"><span class="name">${escapeHtml(v.meta.title || v.meta.session)}</span></span>
      <span class="sub">${escapeHtml(closed)} · ${escapeHtml(location)}</span>
    </span>
    <span class="history-actions">
      <button class="resume" type="button">Resume</button>
      <button class="delete" type="button" title="Delete this saved history entry">Delete</button>
    </span>`;
  requiredDescendant<HTMLElement>(li, '.resume').onclick = (event) => {
    event.stopPropagation();
    void resumeSession(id);
  };
  requiredDescendant<HTMLElement>(li, '.delete').onclick = (event) => {
    event.stopPropagation();
    void killSession(id);
  };
  li.onclick = () => { void resumeSession(id); };
  historyEl.appendChild(li);
}

function renderSidebar() {
  sessionsEl.innerHTML = '';
  historyEl.innerHTML = '';
  const activeViews = [...views].filter(([, view]) => !view.meta.archived);
  const archivedViews = [...views]
    .filter(([, view]) => !!view.meta.archived)
    .sort(([, a], [, b]) => (b.meta.archivedAt || 0) - (a.meta.archivedAt || 0));
  for (const [id, v] of activeViews) {
    const li = document.createElement('li');
    li.className = 'session' + (id === activeId ? ' active' : '') + (v.state === 'dead' ? ' dead' : '');
    // NB: deliberately NOT `draggable = true` — reordering is pointer-driven (§24). Setting it would
    // hand the gesture back to the native drag machinery that swallows it in the first place.
    li.dataset.id = id;
    if (v.color) li.style.setProperty('--accent-bar', v.color);   // left accent bar (CSS uses it)
    li.classList.toggle('has-color', !!v.color);
    // Subtitle: the host for a remote session, "local shell" for a local one. Both append the tmux
    // version when known — a local session runs a real tmux too (§5.3b), so it earns the same badge;
    // its absence is the visible signal that this machine has no tmux and the session isn't durable.
    const ver = v.tmuxVersion ? ` · tmux ${v.tmuxVersion.join('.')}` : '';
    const detachedState = v.meta.detached
      ? ` · detached${v.remoteOpen === true ? ' · open' : v.remoteOpen === false ? ' · not found' : ''}`
      : '';
    const sub = (v.meta.host ? escapeHtml(v.meta.host) : 'local shell') + ver + detachedState;
    // §18: forwarded ports under the project name. Active rows show the local port and open on
    // click; inactive (grey) rows are persisted-but-not-serving — click re-opens the tunnel.
    // String is short: ":<remote> → :<local>" (local shows "—" when not currently mapped).
    // These values are ports (numbers) today, so nothing here can currently break out of the
    // attribute — but this is an innerHTML sink, so escape like every other one (the tab-title and
    // host sinks below/above do). Keeps one schema change from turning this into attribute XSS.
    const tunnelRows = (v.tunnels || []).map((t) => {
      const active = !!t.active;
      const remote = escapeHtml(t.remote);
      const localTxt = t.local ? `:${escapeHtml(t.local)}` : '—';
      const title = escapeHtml(active ? `open http://localhost:${t.local}/` : `port ${t.remote} inactive — click to re-open`);
      // "same" marker when the local port matches the remote (forced same-port mapping).
      const same = t.local && t.local === t.remote;
      return `<span class="tunnel${active ? '' : ' inactive'}" data-remote="${remote}" title="${title}">
         <span class="tport">:${remote}</span><span class="tarrow">→</span>
         <span class="tlocal${same ? ' same' : ''}">${localTxt}</span>
         <span class="tforce" title="force map to the same local port (:${remote})">⇄</span>
         <span class="tclose" title="close tunnel">×</span>
       </span>`;
    }).join('');
    // The action icons live INSIDE the first (name) row, so hovering shifts only that row — the
    // sub line and tunnel rows below keep full width.
    li.innerHTML = `<span class="status-dots">
        <span class="dot ${v.state}"></span>
        ${sessionHasUnreadNotification(v) ? '<span class="notification-dot" aria-label="Unread notification"></span>' : ''}
      </span>
      <span class="body">
        <span class="name-row">
          <span class="name" title="double-click to rename">${escapeHtml(v.meta.title || v.meta.session || v.meta.kind)}</span>
          <span class="controls">
            <span class="retry${reconnectBusy(id) ? ' busy' : ''}">retry</span>
            <span class="act reconnect${reconnectBusy(id) ? ' busy' : ''}" title="${reconnectBusy(id) ? 'Reconnecting…' : 'Force reconnect now'}">⟳</span>
            <span class="act detach" title="Detach (keeps tmux running)">⤫</span>
            <span class="act kill" title="Close (ends tmux and saves recovery state)">⏻</span>
          </span>
        </span>
        <span class="sub">${sub}</span>
        ${tunnelRows ? `<span class="tunnels">${tunnelRows}</span>` : ''}
      </span>`;
    const nameEl = requiredDescendant<HTMLElement>(li, '.name');
    // Rename editor is rebuilt from view state on every render, so it survives the re-renders the
    // double-click itself triggers (see startRename).
    if (v.renaming) mountRenameInput(v, nameEl, id);
    // Double-click the name to rename (display title only; tmux session name unchanged).
    nameEl.ondblclick = (e) => { e.stopPropagation(); startRename(id); };
    requiredDescendant<HTMLElement>(li, '.reconnect').onclick = (e) => { e.stopPropagation(); forceReconnect(id); };
    requiredDescendant<HTMLElement>(li, '.detach').onclick = (e) => { e.stopPropagation(); void detachSession(id); };
    requiredDescendant<HTMLElement>(li, '.kill').onclick = (e) => { e.stopPropagation(); void closeSession(id); };
    // tunnel rows: active -> open the local URL; inactive -> re-open; ⇄ force same-port; × close.
    li.querySelectorAll<HTMLElement>('.tunnel').forEach((el) => {
      const remote = Number(el.getAttribute('data-remote'));
      requiredDescendant<HTMLElement>(el, '.tclose').onclick = (e) => { e.stopPropagation(); void api.closeTunnel(id, remote); };
      // ⇄ force-map to the SAME local port; alert if that local port is already taken.
      requiredDescendant<HTMLElement>(el, '.tforce').onclick = (e) => {
        e.stopPropagation();
        setStatus('mapping port ' + remote + ' -> localhost:' + remote + '…');
        api.forceForward(id, remote)
          .then(() => { setStatus('mapped localhost:' + remote); refreshTunnels(id); })
          .catch((err: unknown) => { const m = errorMessage(err); setStatus('⚠ ' + m); alert('Could not map port ' + remote + ':\n' + m); });
      };
      el.onclick = (e) => {
        if (e.target instanceof Element && (e.target.classList.contains('tclose') || e.target.classList.contains('tforce'))) return;
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
      if (e.target instanceof Element && e.target.classList.contains('retry')) {
        if (!reconnectBusy(id)) { markReconnecting(id); api.retry(id); renderSidebar(); }
        return;
      }
      if (v.renaming) return;                     // ignore clicks while editing this row
      // A double-click on the NAME is the rename gesture, so don't also treat its clicks as
      // "switch to this project": mounting an unconnected project spawns a backend, and the second
      // click's re-render is what used to destroy the rename editor. detail >= 2 is the second click
      // of a double-click; a single click anywhere in the row still mounts as before.
      if (e.detail >= 2 && e.target instanceof Node && nameEl.contains(e.target)) return;
      mount(id, true);
    };
    // §20: right-click opens the color palette for this project.
    li.oncontextmenu = (e) => { e.preventDefault(); openColorMenu(e, v.color, (c) => setProjectColor(id, c)); };
    // §20: drag-to-reorder (project list).
    wireSidebarDnD(li, id);
    sessionsEl.appendChild(li);
  }
  for (const [id, view] of archivedViews) renderHistorySession(id, view);
  historyPanel.toggleAttribute('hidden', archivedViews.length === 0);
  sessionsEl.toggleAttribute('hidden', activeViews.length === 0);
}

// --- §20/§24: drag-to-reorder, on POINTER events (not HTML5 drag-and-drop) --------------------
// HTML5 DnD (draggable + dragstart/dragover/drop) does not work inside this webview at all. wry
// subclasses WKWebView to implement file-drop and overrides the NSDraggingDestination methods; its
// `dragging_updated` returns NSDragOperationCopy whenever Tauri's handler returns true, and Tauri's
// handler unconditionally does (tauri-runtime-wry/src/lib.rs). So every drag over the webview is
// answered "copy" and NEVER forwarded to WebKit: the user sees a "+" badge, the card doesn't move,
// and dragover/drop never fire in JS. Nothing we can do from the frontend makes that path work —
// the interception is above us in the native view.
//
// So we implement dragging ourselves with pointer events, which are ordinary input the native
// drag machinery never sees. Bonus: it's what gives us the live "cards move out of the way"
// feedback that HTML5 DnD can't express (it can only paint a static insertion line).
//
// Shared by the project list (vertical) and the tab strip (horizontal) via `axis`. The gesture:
//   pointerdown  -> arm, remember the origin, but do NOT start yet (a click must stay a click)
//   pointermove  -> once past DRAG_THRESHOLD px, start: lift the element and follow the pointer
//   pointerup    -> commit the pending index, or fall through to the click handler if never started
const DRAG_THRESHOLD = 4;   // px of travel before a press becomes a drag, so clicks still click
let _drag: DragState | null = null;           // the in-flight gesture, or null

// Where would the dragged element land if the pointer stopped here? Counts the OTHER items whose
// midpoint the pointer has passed, so the landing slot tracks real geometry instead of an assumed
// uniform row height (sidebar rows genuinely differ in height — a project with forwarded ports is
// taller).
//
// Fed rects SNAPSHOTTED at drag start rather than live ones. We shift the other items with CSS
// transforms and getBoundingClientRect reports transformed boxes, so live reads would measure the
// layout our own feedback just changed — and mid-transition, so the same pointer position could
// answer differently frame to frame. (Measured: for these one- and two-slot drags live rects happen
// to give the same answer, since displaced items move AWAY from the pointer. The snapshot is the
// version whose correctness doesn't depend on that coincidence.)
function dropIndexAt(rects: readonly DOMRect[], dragIndex: number, coord: number, axis: DragAxis): number {
  let index = 0;
  for (let i = 0; i < rects.length; i++) {
    if (i === dragIndex) continue;
    const r = rects[i];
    if (!r) continue;
    const mid = axis === 'y' ? r.top + r.height / 2 : r.left + r.width / 2;
    if (coord > mid) index++;
  }
  return index;
}

// How far the other items shift when the dragged one leaves its slot: its own extent PLUS the gap
// between items, measured from a neighbour's snapshotted rect so it matches whatever the CSS gap /
// margin actually is rather than hardcoding one.
function slotSize(d: DragState): number {
  const me = d.rects[d.from];
  if (!me) return 0;
  const own = d.axis === 'y' ? me.height : me.width;
  // Measure to whichever neighbour exists — the dragged item may be first or last. Compute the gap
  // edge-to-edge (not top-to-top), so rows of DIFFERENT heights still yield the true gap: a project
  // with forwarded ports is taller than one without.
  const next = d.rects[d.from + 1], prev = d.rects[d.from - 1];
  let gap = 0;
  if (next) gap = d.axis === 'y' ? next.top - me.bottom : next.left - me.right;
  else if (prev) gap = d.axis === 'y' ? me.top - prev.bottom : me.left - prev.right;
  // Overlapping or wrapped layout gives a nonsense gap; fall back to the bare extent.
  return own + (gap > 0 && gap < own ? gap : 0);
}

// Make `el` draggable-by-pointer within `container`. `itemSel` selects the reorderable siblings,
// `commit(fromIndex, toIndex)` applies the move (indexes over the itemSel list — the standard
// "remove at from, insert at to" pair).
//
// Only `pointerdown` is bound to the element; move/up/cancel live on `window`, installed for the
// duration of one gesture. Measured, not assumed (see /tmp probes in the §24 notes): element-level
// pointermove STOPS as soon as the dragged card gets `pointer-events:none` — which it must have, or
// it sits under the cursor and hit-tests itself — and `setPointerCapture` does not hold in this
// webview (`hasPointerCapture` reads back false). Window listeners receive every move in all cases.
// They also survive the element being replaced by a re-render mid-gesture, which element listeners
// would not.
function wirePointerDrag(
  el: HTMLElement,
  container: HTMLElement,
  itemSel: string,
  axis: DragAxis,
  commit: (from: number, to: number) => void,
): void {
  el.addEventListener('pointerdown', (e) => {
    if (e.button !== 0) return;                       // left button only; right = context menu
    if (_drag) return;                                // a gesture is already in flight
    // Don't hijack a press on something interactive: the action icons, the port rows, the ×, or a
    // live rename editor. Those are clicks/edits (or text selection), not drags.
    if (e.target instanceof Element && e.target.closest('input, .controls, .tunnel, .tclose, .plus')) return;
    // Nor while THIS item is being renamed. The editor survives a reorder (it's rebuilt from view
    // state, §23), but dragging a row you're mid-way through naming isn't a gesture anyone intends,
    // and `li.onclick` already ignores clicks while renaming — the two should agree.
    if (el.querySelector('input')) return;
    const items = [...container.querySelectorAll<HTMLElement>(itemSel)];
    const from = items.indexOf(el);
    if (items.length < 2 || from < 0) return;          // nothing to reorder against
    const d: DragState = {
      el, container, items, axis, commit, from, to: from,
      startX: e.clientX, startY: e.clientY, started: false, pointerId: e.pointerId,
      rects: [], slot: 0,
      onMove: () => {}, onUp: () => {}, onCancel: () => {},
    };
    _drag = d;
    // One gesture's worth of window listeners, removed together in `end` — so nothing outlives the
    // drag and a later re-render can't leave a stale handler behind.
    d.onMove = (ev) => onDragMove(d, ev);
    d.onUp = (ev) => endDrag(d, ev, true);
    d.onCancel = () => endDrag(d, null, false);
    window.addEventListener('pointermove', d.onMove);
    window.addEventListener('pointerup', d.onUp);
    window.addEventListener('pointercancel', d.onCancel);
  });
}

function onDragMove(d: DragState, e: PointerEvent): void {
  if (_drag !== d || e.pointerId !== d.pointerId) return;
  const dx = e.clientX - d.startX, dy = e.clientY - d.startY;
  if (!d.started) {
    if (Math.hypot(dx, dy) < DRAG_THRESHOLD) return;
    d.started = true;
    // Snapshot geometry BEFORE any transform is applied — see dropIndexAt.
    d.rects = d.items.map((it) => it.getBoundingClientRect());
    d.slot = slotSize(d);
    // Lift the card: it follows the pointer, and CSS gives .dragging pointer-events:none so it
    // doesn't hit-test itself while sitting under the cursor.
    d.el.classList.add('dragging');
    d.container.classList.add('reordering');
    // Freeze the extent so the space the card vacates stays exactly its own size.
    const r = d.rects[d.from];
    if (!r) return;
    if (d.axis === 'y') d.el.style.height = r.height + 'px'; else d.el.style.width = r.width + 'px';
    // Belt-and-braces for the selection: drop an anchor the press may have placed INSIDE this strip.
    // `user-select:none` (see index.html) is what actually prevents the reported highlight, and in
    // Chromium it's sufficient — measured, no caret is set at all, and removing this block fails no
    // test. It's kept for WKWebView, which ships and can't be exercised here.
    //
    // Deliberately scoped to `d.container`, NOT a bare removeAllRanges(): a selection anywhere else
    // is the user's (terminal output, a file preview) and clearing it would be a bug of our own. That
    // is not hypothetical — the unscoped version wiped a planted terminal selection when probed.
    // Text fields keep their own selection too, so an open rename editor's caret (§23) survives.
    const sel = window.getSelection();
    if (sel && sel.rangeCount && !(document.activeElement instanceof HTMLInputElement)
        && d.container.contains(sel.anchorNode)) {
      sel.removeAllRanges();
    }
  }
  e.preventDefault();
  // Follow the pointer along the drag axis only — sideways drift would just look broken.
  d.el.style.transform = d.axis === 'y' ? `translateY(${dy}px)` : `translateX(${dx}px)`;
  d.to = dropIndexAt(d.rects, d.from, d.axis === 'y' ? e.clientY : e.clientX, d.axis);
  // "Move other cards dynamically": every item between the old and new slot shifts by one slot, so
  // the gap that opens up IS the placeholder. Pure transforms — no reflow and no re-render, so the
  // dragged element keeps its identity (and an open rename editor elsewhere is untouched).
  for (let i = 0; i < d.items.length; i++) {
    if (i === d.from) continue;
    let shift = 0;
    if (d.from < d.to && i > d.from && i <= d.to) shift = -d.slot;
    else if (d.to < d.from && i >= d.to && i < d.from) shift = d.slot;
    const item = d.items[i];
    if (!item) continue;
    item.style.transform = shift
      ? (d.axis === 'y' ? `translateY(${shift}px)` : `translateX(${shift}px)`) : '';
  }
}

// End of gesture. `commitMove` false = cancelled (a system gesture, the window losing the pointer):
// abandon WITHOUT persisting. A half-applied reorder would be worse than none.
function endDrag(d: DragState, e: PointerEvent | null, commitMove: boolean): void {
  if (_drag !== d || (e && e.pointerId !== d.pointerId)) return;
  _drag = null;
  window.removeEventListener('pointermove', d.onMove);
  window.removeEventListener('pointerup', d.onUp);
  window.removeEventListener('pointercancel', d.onCancel);
  // Never started = the user just clicked. Leave everything alone so the click handler runs.
  if (!d.started) return;
  clearDragStyles(d);
  // A completed drag can still be followed by a `click`, and the row/tab click handlers switch
  // project / switch tab — reordering must not also select. Swallowed at the WINDOW, capture phase,
  // because that's where the click actually lands: measured in Chromium, the post-drag click targets
  // the CONTAINER, not the dragged element (the lifted card has pointer-events:none so it is never
  // the mouseup target, which puts the down/up common ancestor at the container). A listener on the
  // element would therefore never see it.
  //
  // On that measurement this guard is redundant in Chromium — no row handler receives the click
  // either way, and removing it fails no test. It's kept as insurance for the shipping engine,
  // WKWebView, which is not Chromium and which nothing here can exercise; the cost is one
  // single-shot listener. Do not read the tests as proof that it's load-bearing.
  const swallow = (ev: MouseEvent) => { ev.stopPropagation(); ev.preventDefault(); };
  window.addEventListener('click', swallow, { capture: true, once: true });
  // Armed for one turn of the event loop only. With `once` alone, a drag that produced no click at
  // all would leave the listener primed to eat an unrelated click later.
  setTimeout(() => window.removeEventListener('click', swallow, { capture: true }), 0);
  if (commitMove && d.to !== d.from) d.commit(d.from, d.to);
}

// Undo every inline style/class the drag applied. The subsequent re-render would drop them anyway
// (both strips rebuild from innerHTML), but a cancelled drag doesn't re-render — so this is what
// makes cancel actually restore the strip.
function clearDragStyles(d: DragState): void {
  d.el.classList.remove('dragging');
  d.container.classList.remove('reordering');
  d.el.style.transform = ''; d.el.style.height = ''; d.el.style.width = '';
  for (const item of d.items) item.style.transform = '';
}

function wireSidebarDnD(li: HTMLElement, _id: string): void {
  wirePointerDrag(li, sessionsEl, '.session', 'y', (from, to) => reorderProjectByIndex(from, to));
}

// Move the project at `from` to index `to`, rebuild the views Map in the new order, persist.
// Index-based (not "before/after target id") because that's what the pointer drag knows: the
// landing slot, which may be one past the last row — an id-relative form can't name that position.
function reorderProjectByIndex(from: number, to: number): void {
  const ids = [...views].filter(([, view]) => !view.meta.archived).map(([id]) => id);
  if (from < 0 || from >= ids.length || to < 0 || to >= ids.length) return;   // list changed mid-drag
  const [moved] = ids.splice(from, 1);
  if (!moved) return;
  ids.splice(to, 0, moved);
  const reordered = new Map<string, View>();
  for (const id of ids) { const v = views.get(id); if (v) reordered.set(id, v); }
  for (const [id, view] of views) if (view.meta.archived) reordered.set(id, view);
  views.clear();
  for (const [id, v] of reordered) views.set(id, v);
  renderSidebar();
  api.reorderSessions([...views.keys()]).catch(() => {});
}

function escapeHtml(s: unknown): string {
  const entities: Record<string, string> = { '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' };
  return String(s).replace(/[&<>"]/g, (c) => entities[c] ?? c);
}

// --- §20: color palette --------------------------------------------------------------------
// A small fixed palette (Catppuccin-ish accents) + a "none" chip. Reused for projects and tabs.
const PALETTE = ['#f38ba8', '#fab387', '#f9e2af', '#a6e3a1', '#94e2d5', '#89b4fa', '#cba6f7', '#f5c2e7'];
function openColorMenu(ev: MouseEvent, current: string | null | undefined, onPick: (color: string | null) => void): void {
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
  // Register the dismiss listener so closeColorMenu() unhooks it on EVERY close path. Picking a
  // swatch calls closeColorMenu() directly, which used to remove the menu but leave this listener
  // attached — one leaked handler per color pick.
  setTimeout(() => {
    const off = (e: MouseEvent) => { if (!(e.target instanceof Node) || !menu.contains(e.target)) closeColorMenu(); };
    colorMenuDismiss = off;
    document.addEventListener('mousedown', off);
  }, 0);
}
let colorMenuDismiss: ((event: MouseEvent) => void) | null = null;   // the outside-click handler for the open menu, if any
function closeColorMenu(): void {
  if (colorMenuDismiss) { document.removeEventListener('mousedown', colorMenuDismiss); colorMenuDismiss = null; }
  const m = document.getElementById('color-menu');
  if (m && m.parentNode) m.parentNode.removeChild(m);
}

function setProjectColor(id: string, color: string | null): void {
  const v = views.get(id);
  if (!v) return;
  v.color = color;
  renderSidebar();
  api.setSessionColor(id, color).catch(() => {});
}

// Tear down a view's live UI (tabs + container) without deciding its fate. Shared by removeView
// (session is gone) and detachSession (session survives on the remote and stays in the sidebar).
function teardownViewUi(v: View): void {
  for (const [, t] of v.tabs) { try { t.content.dispose(); } catch (_) {} }
  v.tabs.clear();
  if (v.el && v.el.parentNode) v.el.parentNode.removeChild(v.el);
  v.el = null;
  v.activeWindow = null;
  v.pending = null;
}

function firstActiveViewId(excluding?: string): string | null {
  for (const [id, view] of views) {
    if (id !== excluding && !view.meta.archived) return id;
  }
  return null;
}

// Remove a project view from the UI (dispose all its tabs). Does NOT touch the remote.
function removeView(id: string): void {
  const v = views.get(id);
  if (!v) return;
  teardownViewUi(v);
  views.delete(id);
  // Drop any output buffered for an id that will never get a view again — makeView is the only
  // drain, so leaving this would grow without bound for the app's lifetime.
  delete pendingData[id];
  if (activeId === id) {
    activeId = null;
    tabsEl.className = ''; tabsEl.innerHTML = '';
    const next = firstActiveViewId(id);
    if (next) void mount(next); else { termHost.innerHTML = ''; setStatus('no active sessions'); }
  }
  renderSidebar();
}

// Explicit Detach: stop only Buoy's client. The tmux server and all of its processes stay alive.
// The persisted row remains in Sessions and is marked detached until the next mount.
async function detachSession(id: string): Promise<void> {
  const v = views.get(id);
  if (!v) return;
  try {
    await api.detach(id);
  } catch (error) {
    setStatus(`detach failed: ${errorMessage(error)}`);
    return;
  }
  // Keep locally observed per-tab commands across the detach/reattach UI teardown. The tmux
  // window ids remain stable, so ensureTab can put them back on reconstructed AppTab objects.
  v.meta.recoveryTabs = recoveryHints(v);
  teardownViewUi(v);
  delete pendingData[id];          // late output for the connection we just dropped
  v.started = false;               // mount() will reattach on the next click
  v.state = 'idle';
  v.meta.detached = true;
  v.remoteOpen = null;
  v.inputReady = v.meta.mode !== 'control';
  v.tunnels = [];
  v.restoreTab = v.lastTab || null;   // reveal the same tab again on reattach
  if (v.meta.mode !== 'control') ensureTab(v, '@single');
  if (activeId === id) {
    activeId = null;
    tabsEl.className = ''; tabsEl.innerHTML = '';
    const next = firstActiveViewId(id);
    if (next) void mount(next); else { termHost.innerHTML = ''; setStatus('detached — click the session to reattach'); }
  } else {
    setStatus('detached (still running on the remote)');
  }
  renderSidebar();
  updateConsoleGate();
}

function recoveryHints(v: View): RecoveryTabSnapshot[] {
  return tabDisplayOrder(v).flatMap((window) => {
    const tab = v.tabs.get(window);
    if (!tab || tab.viewer) return [];
    return [{ window, title: tab.title, lastCommand: tab.lastCommand || '' }];
  });
}

// Close snapshots each tmux window, ends the tmux session, and moves the durable metadata to
// History. Resume later reconstructs windows/cwds and seeds bash/zsh history with lastCommand.
async function closeSession(id: string): Promise<void> {
  const v = views.get(id);
  if (!v) return;
  if (v.meta.mode === 'local') {
    if (!confirm('Close this local shell?\n\nIt is not backed by tmux and cannot be resumed.')) return;
    try { await api.kill(id); } catch (_) {}
    removeView(id);
    return;
  }
  const label = v.meta.title || v.meta.session;
  const location = v.meta.transport === 'local' ? 'local' : 'remote';
  if (!confirm(`Close "${label}"?\n\nThis ends the ${location} tmux session. Buoy will save each tab's working directory and last command so it can be reconstructed from History.`)) return;

  if (!v.started) {
    await mount(id);
    if (!v.started || v.state === 'dead') {
      setStatus('connect the session before closing it');
      return;
    }
  }
  const hints = recoveryHints(v);
  setStatus(`saving and closing ${label}…`);
  try {
    await api.close(id, hints);
  } catch (error) {
    // The backend detaches first to prevent the reconnect supervisor from recreating tmux.
    teardownViewUi(v);
    v.started = false;
    v.state = 'idle';
    v.meta.detached = true;
    setStatus(`close failed; session detached instead: ${errorMessage(error)}`);
    renderSidebar();
    return;
  }
  teardownViewUi(v);
  delete pendingData[id];
  v.started = false;
  v.state = 'idle';
  v.inputReady = false;
  v.meta.archived = true;
  v.meta.archivedAt = Date.now();
  v.meta.detached = false;
  v.meta.recoveryTabs = hints;
  v.meta.restorePending = true;
  if (activeId === id) {
    activeId = null;
    tabsEl.className = ''; tabsEl.innerHTML = '';
    const next = firstActiveViewId(id);
    if (next) void mount(next); else termHost.innerHTML = '';
  }
  setStatus(`${label} closed and moved to History`);
  renderSidebar();
  updateConsoleGate();
}

async function resumeSession(id: string): Promise<void> {
  const v = views.get(id);
  if (!v || !v.meta.archived) return;
  const label = v.meta.title || v.meta.session;
  setStatus(`restoring ${label}…`);
  try {
    await api.resume(id);
  } catch (error) {
    setStatus(`could not resume ${label}: ${errorMessage(error)}`);
    return;
  }
  v.meta.archived = false;
  v.meta.archivedAt = null;
  v.meta.detached = false;
  v.started = false;
  v.state = 'idle';
  v.restoreTab = v.lastTab || null;
  renderSidebar();
  await mount(id, true);
}

async function checkOpenSessions(): Promise<void> {
  recoverButton.disabled = true;
  setStatus('checking tmux sessions…');
  try {
    const results = await api.checkOpenSessions();
    const recoverable: Array<[string, View]> = [];
    let errors = 0;
    for (const result of results) {
      const view = views.get(result.id);
      if (!view) continue;
      view.remoteOpen = result.error ? null : result.open;
      if (result.error) errors++;
      if (result.open && view.meta.detached) recoverable.push([result.id, view]);
    }
    renderSidebar();
    if (recoverable.length) {
      showChooser('Open detached sessions', recoverable.map(([sessionId, view]) => [
        `Reattach ${view.meta.title || view.meta.host || view.meta.session}`,
        () => void mount(sessionId, true),
      ] as const));
      setStatus(`${recoverable.length} detached session${recoverable.length === 1 ? '' : 's'} found`);
    } else if (errors) {
      setStatus(`check finished with ${errors} host error${errors === 1 ? '' : 's'}`);
    } else {
      const open = results.filter((result) => result.open).length;
      setStatus(`${open} tmux session${open === 1 ? '' : 's'} open; none need recovery`);
    }
  } catch (error) {
    setStatus(`could not check sessions: ${errorMessage(error)}`);
  } finally {
    recoverButton.disabled = false;
  }
}

recoverButton.onclick = () => { void checkOpenSessions(); };

// Force reconnect: tear down and reattach the SAME session now, even if it currently looks
// connected (e.g. a wedged/half-open link after a network change). The backend resets its retry
// budget and respawns; we reset the display gate so the console blurs to "connecting…" until the
// new attach settles (Ready). tmux keeps the session alive, so windows/scrollback come back.
function forceReconnect(id: string): void {
  const v = views.get(id);
  if (!v || v.meta.mode !== 'control') return;   // only supervised control sessions reconnect
  if (reconnectBusy(id)) { setStatus('reconnect already in progress…'); return; }
  markReconnecting(id);
  v.inputReady = false;
  if (id === activeId) updateConsoleGate();
  setStatus('reconnecting…');
  api.forceReconnect(id);
  renderSidebar();
}

// Permanent delete: active sessions lose tmux and metadata; archived sessions lose the snapshot.
async function killSession(id: string): Promise<void> {
  const v = views.get(id);
  const label = (v && (v.meta.title || v.meta.session)) || 'this session';
  const archived = !!v?.meta.archived;
  const message = archived
    ? `Delete "${label}" from History?\n\nThe tmux session was already closed. This removes Buoy's recovery snapshot and cannot be undone.`
    : `Delete "${label}" permanently?\n\nThis ends the tmux session and everything running in it, then removes its saved entry. This cannot be undone.`;
  if (!confirm(message)) return;
  setStatus(archived ? `deleting ${label} from History…` : `deleting ${label}…`);
  try {
    const res = await api.kill(id);
    setStatus(res && res.killedRemote ? `killed ${label}` : `removed ${label}`);
  } catch (_) { setStatus(`removed ${label}`); }
  removeView(id);
}

// §23: inline-edit the session's display title.
//
// The editor is STATE-DRIVEN (v.renaming on the view), NOT a node this handler mutates directly.
// That's load-bearing: a double-click delivers click, click, dblclick — and the row's click handler
// calls mount(), which calls renderSidebar(), which rebuilds the whole list with `innerHTML = ''`.
// So by the time dblclick fires, the node it closed over is already detached, and appending an
// <input> to it produced an editor that was created and "focused" but not in the document —
// invisible and untypable ("rename not enabled"). Holding the intent in the view instead means any
// re-render (the clicks themselves, a session:state event, the 5s tunnel refresh) REBUILDS the
// editor in the live row rather than destroying it. Covered by test/gui-rename.ts, which drives
// real OS-level double-clicks — a synthetic dblclick event skips the two clicks and can't see this.
function startRename(id: string): void {
  const v = views.get(id);
  if (!v || v.renaming) return;
  v.renaming = true;
  v.renameDraft = v.meta.title || v.meta.session || '';
  v.renameSel = null;       // caret, preserved across re-renders (see mountRenameInput)
  v.renameFocus = true;     // focus once, on the next render
  renderSidebar();
}

async function commitRename(id: string, save: boolean): Promise<void> {
  const v = views.get(id);
  // Guard: blur fires after Enter/Escape already committed, so the second call must be a no-op
  // (otherwise a committed rename would be sent twice).
  if (!v || !v.renaming) return;
  const current = v.meta.title || v.meta.session || '';
  const next = (v.renameDraft || '').trim();
  v.renaming = false; v.renameDraft = null; v.renameSel = null; v.renameFocus = false;
  renderSidebar();
  if (save && next && next !== current) {
    const res = await api.rename(id, next);
    if (res && res.ok) { v.meta.title = res.title; renderSidebar(); }
  }
}

// Build the rename editor inside the row's (live) name node. Called from renderSidebar, so it runs
// again on every re-render while the edit is open.
function mountRenameInput(v: View, nameEl: HTMLElement, id: string): void {
  const input = document.createElement('input');
  input.type = 'text';
  input.className = 'rename-input';
  input.value = v.renameDraft != null ? v.renameDraft : '';
  nameEl.textContent = '';
  nameEl.appendChild(input);
  // Mirror value AND caret into the view on every change, so a re-render mid-typing neither resets
  // the field nor jumps the cursor.
  const remember = () => {
    v.renameDraft = input.value;
    v.renameSel = [input.selectionStart, input.selectionEnd];
  };
  input.oninput = remember;
  input.onkeyup = remember;
  input.onselect = remember;
  input.onclick = (e) => e.stopPropagation();      // don't switch project while editing
  input.ondblclick = (e) => e.stopPropagation();
  input.onkeydown = (e) => {
    if (e.key === 'Enter') { e.preventDefault(); commitRename(id, true); }
    else if (e.key === 'Escape') { e.preventDefault(); commitRename(id, false); }
  };
  input.onblur = () => commitRename(id, true);
  if (v.renameFocus) {
    v.renameFocus = false;
    // Focus on the next frame: the node is in the tree now, but a rAF also survives the second
    // click of the double-click re-rendering the row underneath us.
    requestAnimationFrame(() => { if (input.isConnected) { input.focus(); input.select(); } });
  } else {
    // A later re-render (state event, tunnel tick): restore focus + caret so typing is unbroken.
    if (v.renameSel) { try { input.setSelectionRange(v.renameSel[0], v.renameSel[1]); } catch (_) {} }
    requestAnimationFrame(() => {
      if (input.isConnected && document.activeElement !== input) {
        input.focus();
        if (v.renameSel) { try { input.setSelectionRange(v.renameSel[0], v.renameSel[1]); } catch (_) {} }
      }
    });
  }
}

// Inline-edit a tmux-window tab's title. Sends the new name to tmux (rename-window, which also
// pins it by disabling automatic-rename); an empty value clears the manual name so it follows the
// pane title again. tmux echoes %window-renamed, which updates tab.title authoritatively.
//
// §23: state-driven for the same reason as the sidebar rename (see startRename): the tab's click handler
// calls switchTab() -> renderTabs(), so the two clicks of a double-click rebuilt the strip before
// dblclick fired and the editor landed on a detached node. The intent lives on the tab.
function startTabRename(v: View, wid: string): void {
  const tab = v.tabs.get(wid);
  if (!tab || tab.renaming) return;
  tab.renaming = true;
  tab.renameDraft = tab.title && tab.title !== wid ? tab.title : '';
  tab.renameSel = null;
  tab.renameFocus = true;
  renderTabs(v);
}

function commitTabRename(v: View, wid: string, save: boolean): void {
  const tab = v.tabs.get(wid);
  if (!tab || !tab.renaming) return;      // blur after Enter/Escape must not re-send
  const current = tab.title && tab.title !== wid ? tab.title : '';
  const next = (tab.renameDraft || '').trim();
  tab.renaming = false; tab.renameDraft = null; tab.renameSel = null; tab.renameFocus = false;
  // An EMPTY value is meaningful here (it clears the manual name so tmux resumes auto-rename), so
  // this sends on any change from current — unlike the project rename, where empty means "cancel".
  if (save && next !== current) api.tabRename(v.meta.id, wid, next);
  renderTabs(v);   // repaint; tmux's %window-renamed echo settles the final label
}

// Build the tab's rename editor inside the (live) label node; re-run on every renderTabs.
function mountTabRenameInput(v: View, wid: string, labelEl: HTMLElement): void {
  const tab = v.tabs.get(wid);
  if (!tab) return;
  const input = document.createElement('input');
  input.type = 'text';
  input.className = 'tab-rename-input';
  input.value = tab.renameDraft != null ? tab.renameDraft : '';
  labelEl.textContent = '';
  labelEl.appendChild(input);
  const remember = () => {
    tab.renameDraft = input.value;
    tab.renameSel = [input.selectionStart, input.selectionEnd];
  };
  input.oninput = remember;
  input.onkeyup = remember;
  input.onselect = remember;
  input.onclick = (e) => e.stopPropagation();     // don't switch tabs while editing
  input.ondblclick = (e) => e.stopPropagation();
  input.onkeydown = (e) => {
    if (e.key === 'Enter') { e.preventDefault(); commitTabRename(v, wid, true); }
    else if (e.key === 'Escape') { e.preventDefault(); commitTabRename(v, wid, false); }
  };
  input.onblur = () => commitTabRename(v, wid, true);
  if (tab.renameFocus) {
    tab.renameFocus = false;
    requestAnimationFrame(() => { if (input.isConnected) { input.focus(); input.select(); } });
  } else {
    if (tab.renameSel) { try { input.setSelectionRange(tab.renameSel[0], tab.renameSel[1]); } catch (_) {} }
    requestAnimationFrame(() => {
      if (input.isConnected && document.activeElement !== input) {
        input.focus();
        if (tab.renameSel) { try { input.setSelectionRange(tab.renameSel[0], tab.renameSel[1]); } catch (_) {} }
      }
    });
  }
}

// --- events from main ---
// Control-mode data is tagged with the WINDOW it belongs to (the backend owns pane->window
// resolution). Plain/local data has no window -> the single tab. The renderer never maps panes.
api.onData(({ id, data, window, repaint }) => {
  // A reconnect snapshot is one complete backend payload, so remove only its OSC 8 wrappers before
  // buffering/rendering. tmux's capture still retains colors and every other text attribute.
  const renderedData = repaint ? DTBuiltinPlugins.sanitizeReconnectSnapshot(data) : data;
  const v = views.get(id);
  if (!v) { dbg('onData: NO VIEW id=' + id + ' (buffering)'); (pendingData[id] = pendingData[id] || []).push(renderedData); return; }
  const tab = (v.meta.mode === 'control') ? (window ? ensureTab(v, window) : activeTab(v)) : activeTab(v);
  deliver(v, tab, data, renderedData);
});

const pendingData: Record<string, string[]> = {};   // id -> [data] buffered before the view exists

// Deliver a data chunk to a resolved tab (mounting/revealing it if it's the active one).
function deliver(v: View, tab: AppTab | null, data: string, renderedData = data): void {
  // §21: harvest OSC 8 file:// links from the raw stream into the project's path map BEFORE xterm
  // consumes the data (xterm strips the hyperlink from scrollback, so this is our only capture point).
  harvestOsc8FileLinks(v, data);
  harvestOscNotifications(v, tab, data);
  if (!tab) { (v.pending = v.pending || []).push(renderedData); return; }
  trackTuiActivity(v, tab, data);
  // If this tab isn't mounted yet but its project is the active one, mount it now so the
  // data (e.g. scrollback back-fill on reattach) is displayed, not just buffered.
  if (!tab.mounted && v.meta.id === activeId && v.el) { showActiveTab(v); renderTabs(v); }
  if (!tab.mounted) { (tab.pre = tab.pre || []).push(renderedData); return; }
  tab.content.onData(renderedData);
}

// --- test hooks (used by the TypeScript GUI suites to drive/inspect the real xterm) ---
// Forward like a real keystroke; the backend buffers until ready, so tests need no gate here.
window.__testType = (s: string) => {
  if (activeId == null) return;
  const v = views.get(activeId);
  acknowledgeTerminalInteraction(v, v && activeTab(v));
  trackCommandInput(v ? activeTab(v) : null, s);
  api.input(activeId, s, v && v.activeWindow);
};
window.__testInputReady = () => { const v = activeId ? views.get(activeId) : undefined; return !!(v && v.inputReady); };
window.__testMount = (id: string) => {
  if (!views.has(id)) makeView({ id, host: 'test', session: id, transport: 'ssh', kind: 'remote', mode: 'control' });
  const v = views.get(id); if (v) v.started = true;   // main already started it in the test
  mount(id);
};
window.__testDispose = (id: string) => { removeView(id); };
window.__testReadBuffer = () => {
  const v = activeId ? views.get(activeId) : undefined;
  if (!v) return '';
  const tab = activeTab(v);
  return tab ? tab.content.readBuffer() : '';
};
window.__testTextIsUnderlined = (text: string) => {
  const term = activeTerminalForTest();
  const buf = term.buffer.active;
  for (let y = 0; y < buf.length; y++) {
    const line = buf.getLine(y);
    if (!line) continue;
    const start = line.translateToString(false).indexOf(text);
    if (start < 0) continue;
    for (let x = start; x < start + text.length; x++) {
      if (line.getCell(x)?.isUnderline()) return true;
    }
    return false;
  }
  return null;
};
window.__testLinkPath = (text: string) => {
  const v = activeId ? views.get(activeId) : undefined;
  return v?.linkMap.get(text) ?? null;
};
window.__testRepaintCount = getTerminalRepaintCount;
window.__testRendererKind = () => {
  const v = activeId ? views.get(activeId) : undefined;
  const tab = v && activeTab(v);
  return tab?.content.rendererKind?.() ?? null;
};

function activeTerminalForTest(): XtermTerminal {
  const v = activeId ? views.get(activeId) : undefined;
  const term = v && activeTab(v)?.content.term;
  if (!term) throw new Error('no active terminal');
  return term;
}

const afterTestPaint = (): Promise<void> => new Promise((resolve) => {
  requestAnimationFrame(() => requestAnimationFrame(() => resolve()));
});

// Opt-in renderer measurements. These bypass the backend so both renderer kinds receive identical
// bytes and the timing covers xterm parsing plus the webview's scheduled paint, not IPC variance.
window.__testBenchmarkWrite = async (lines: number) => {
  const term = activeTerminalForTest();
  const count = Math.max(1, Math.floor(lines));
  const data = 'renderer benchmark 0123456789 abcdefghijklmnopqrstuvwxyz\r\n'.repeat(count);
  const bytes = byteLength(data);
  const started = performance.now();
  const parsedAt = await new Promise<number>((resolve) => {
    term.write(data, () => resolve(performance.now()));
  });
  await afterTestPaint();
  return { bytes, lines: count, parseMs: parsedAt - started, totalMs: performance.now() - started };
};

window.__testBenchmarkFrames = async (frames: number) => {
  const term = activeTerminalForTest();
  const count = Math.max(1, Math.floor(frames));
  const samples: number[] = [];
  for (let frame = 0; frame < count; frame++) {
    let data = '';
    for (let row = 0; row < term.rows; row++) {
      const line = `frame ${frame.toString().padStart(3, '0')} row ${row.toString().padStart(2, '0')} `
        .padEnd(term.cols, String(frame % 10));
      data += `\x1b[${row + 1};1H${line.slice(0, term.cols)}`;
    }
    const started = performance.now();
    await new Promise<void>((resolve) => term.write(data, () => resolve()));
    await afterTestPaint();
    samples.push(performance.now() - started);
  }
  const sorted = samples.slice().sort((a, b) => a - b);
  const p95Index = Math.min(sorted.length - 1, Math.floor(sorted.length * 0.95));
  return {
    frames: count,
    meanMs: samples.reduce((sum, sample) => sum + sample, 0) / samples.length,
    p95Ms: sorted[p95Index] ?? 0,
    maxMs: sorted[sorted.length - 1] ?? 0,
  };
};

window.__testArmInputLatency = armTerminalInputLatency;
window.__testSendInput = (data: string) => activeTerminalForTest().input(data, true);
window.__testInputLatency = getTerminalInputLatency;
// Exact xterm cursor/viewport inspection for reconnect-repaint regressions. The public app never
// calls this; GUI tests use it to distinguish a backend cursor from where xterm will echo input.
window.__testTerminalState = () => {
  const v = activeId ? views.get(activeId) : undefined;
  const tab = v && activeTab(v);
  const term = tab && tab.content && tab.content.term;
  if (!term) return null;
  const buf = term.buffer.active;
  const absoluteY = buf.baseY + buf.cursorY;
  const textAt = (y: number): string => {
    const line = buf.getLine(y);
    return line ? line.translateToString(true) : '';
  };
  return {
    cols: term.cols, rows: term.rows,
    cursorX: buf.cursorX, cursorY: buf.cursorY, baseY: buf.baseY,
    line: textAt(absoluteY), previous: textAt(absoluteY - 1), next: textAt(absoluteY + 1),
  };
};
api.onState(({ id, state }) => {
  // Logged like onWindow/onReady: a session stuck showing "connecting" is exactly a missing/late
  // state event, and without this line the log can't distinguish "never emitted" from "ignored".
  dbg('onState id=' + id + ' state=' + state + ' mode=' + (views.get(id)?.meta.mode));
  const v = views.get(id);
  if (!v) return;
  v.state = state;
  if (state === 'connecting' && v.meta.mode === 'control') {
    // A fresh backend needs a fresh, post-resize capture even when the existing xterm/tab objects
    // survive a supervisor reconnect. The backend gates input until that repaint completes.
    v.inputReady = false;
    for (const [, tab] of v.tabs) if (isWindowTab(tab.winId)) tab.backfilled = false;
  }
  // The backend has acted on the reconnect request (it always reports a state after a spawn), so
  // release the in-flight guard and let the control be clicked again.
  if (state !== 'connecting') clearReconnect(id);
  if (id === activeId) { setStatus(statusLine(v, state)); updateConsoleGate(); }
  renderSidebar();
});

// Persistent status line: "<title> — <state> · tmux <ver>" so the probed tmux stays visible.
function statusLine(v: View, state: SessionState): string {
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
const tabsEl = requiredElement<HTMLElement>('tabs');
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
    // Closing an unread tab can clear the session-level rollup dot.
    renderSidebar();
  } else if (action === 'rename') {
    ensureTab(v, window).title = name || window;
  } else if (action === 'active') {
    // tmux switched the active window (e.g. after opening a new tab). Follow it so the shown tab
    // matches where input goes. Lazy-load the newly-focused tab's scrollback on first view.
    ensureTab(v, window);
    if (v.activeWindow !== window) {
      v.activeWindow = window;
    }
    // The first `add` provisionally sets activeWindow, so the initial authoritative `active` event
    // can name the same id. Still mount/fit here: backend attach no longer performs a premature
    // 80x24 capture, and this post-fit path owns the one correctly-sized backfill.
    if (id === activeId) showActiveTab(v);
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
function lazyCapture(v: View, winId: string): void {
  const tab = v.tabs.get(winId);
  if (tab && !tab.backfilled) { tab.backfilled = true; api.tabCapture(v.meta.id, winId); }
}

// Compute the tab display order: the user's saved custom order first (§20), then any tabs not in
// it (new windows) in tmux's window order, then anything else (viewer tabs) in insertion order.
function tabDisplayOrder(v: View): string[] {
  const order: string[] = [];
  for (const w of (v.savedTabOrder || [])) if (v.tabs.has(w) && !order.includes(w)) order.push(w);
  for (const w of (v.tabOrder || [])) if (v.tabs.has(w) && !order.includes(w)) order.push(w);
  for (const w of v.tabs.keys()) if (!order.includes(w)) order.push(w);
  return order;
}

// Render the tab strip for the active control-mode project (hidden for plain/single).
function renderTabs(v: View | null | undefined): void {
  if (!v || v.meta.mode !== 'control') { tabsEl.className = ''; tabsEl.innerHTML = ''; return; }
  tabsEl.className = 'on';
  tabsEl.innerHTML = '';
  for (const wid of tabDisplayOrder(v)) {
    const tab = v.tabs.get(wid);
    if (!tab) continue;
    const el = document.createElement('div');
    el.className = 'tab' + (wid === v.activeWindow ? ' active' : '') + (tab.closing ? ' closing' : '');
    const color = v.tabColors[wid];
    if (color) { el.style.setProperty('--tab-color', color); el.classList.add('has-color'); }
    el.innerHTML = `<span class="tlabel" title="double-click to rename">${tab.unreadNotification ? '<span class="notification-dot" aria-label="Unread notification"></span>' : ''}<span class="ttext">${escapeHtml(tab.title || wid)}</span></span><span class="tclose" title="close">×</span>`;
    const label = requiredDescendant<HTMLElement>(el, '.tlabel');
    // Editor rebuilt from tab state each render, so it survives the double-click's own re-renders.
    if (tab.renaming) mountTabRenameInput(v, wid, label);
    label.onclick = (e) => {
      // The second click of a double-click is the rename gesture, not a tab switch (switchTab's
      // renderTabs is what used to destroy the editor). Belt-and-braces: switchTab's own
      // `activeWindow === winId` early-out also swallows the second click here, since the first one
      // just made this tab active — either guard alone is enough (mutation-tested), but the intent is
      // clearer stated at the gesture than inferred from a downstream no-op.
      if (tab.renaming || e.detail >= 2) return;
      switchTab(v, wid, true);
    };
    // Double-click a real tmux-window tab to rename it. A manual rename sticks (tmux disables
    // automatic-rename for that window); clearing it re-enables auto-rename.
    if (isWindowTab(wid)) label.ondblclick = (e) => { e.stopPropagation(); startTabRename(v, wid); };
    requiredDescendant<HTMLElement>(el, '.tclose').onclick = (e) => { e.stopPropagation(); closeTab(v, wid); };
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

function setTabColor(v: View, wid: string, color: string | null): void {
  if (color) v.tabColors[wid] = color; else delete v.tabColors[wid];
  renderTabs(v);
  api.setTabPrefs(v.meta.id, null, [wid, color || null]).catch(() => {});
}

// §20/§24: tab reorder (horizontal), same pointer-drag mechanism as the sidebar — HTML5 DnD is
// unavailable in this webview (see wirePointerDrag). `:not(.plus)` keeps the trailing "+" button out
// of the reorderable set: it isn't a tab and must not be displaced or landed on.
function wireTabDnD(el: HTMLElement, v: View, _wid: string): void {
  wirePointerDrag(el, tabsEl, '.tab:not(.plus)', 'x', (from, to) => reorderTabByIndex(v, from, to));
}

// Move the tab at display index `from` to index `to`, then persist. Indexes are over
// tabDisplayOrder(v), which is exactly the DOM order wirePointerDrag measured.
function reorderTabByIndex(v: View, from: number, to: number): void {
  const order = tabDisplayOrder(v);
  if (from < 0 || from >= order.length || to < 0 || to >= order.length) return;  // strip changed mid-drag
  const [moved] = order.splice(from, 1);
  if (!moved) return;
  order.splice(to, 0, moved);
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
function switchTab(v: View, winId: string, userInitiated = false): void {
  const tab = v.tabs.get(winId);
  // Clicking the already-active tab is still the acknowledgement gesture, so clear before the
  // same-tab early return. A later, genuinely new OSC will set the flag again.
  if (userInitiated) clearTabNotification(v, tab);
  if (v.activeWindow === winId) return;
  v.activeWindow = winId;
  if (isWindowTab(winId)) { api.tabSelect(v.meta.id, winId); rememberLastTab(v, winId); }
  showActiveTab(v);
  renderTabs(v);
}

// §20: persist a project's last-active tab so it's restored when the project is reopened.
function rememberLastTab(v: View, winId: string): void {
  if (!isWindowTab(winId) || v.lastTab === winId) return;
  v.lastTab = winId;
  api.setLastTab(v.meta.id, winId).catch(() => {});
}

// Close a tab. Terminal tabs -> tmux kill-window (backend removes it, emits close). Viewer tabs
// are app-local -> dispose locally, no tmux command, and re-focus a remaining tab.
function closeTab(v: View, winId: string): void {
  if (isWindowTab(winId)) {
    // The tab disappears only when tmux confirms via the window-close event, so give immediate
    // feedback and ignore repeat clicks — otherwise the unchanged tab invites a second kill-window.
    const t = v.tabs.get(winId);
    if (t) {
      if (t.closing) return;
      t.closing = true;
      if (v.meta.id === activeId) renderTabs(v);   // reflect the pending close in the strip
    }
    api.tabClose(v.meta.id, winId);
    return;
  }
  const t = v.tabs.get(winId);
  if (t) { try { t.content.dispose(); } catch (_) {} v.tabs.delete(winId); }
  if (v.activeWindow === winId) {
    const first = v.tabs.keys().next();
    v.activeWindow = first.done ? null : first.value;
    if (v.meta.id === activeId) showActiveTab(v);
  }
  if (v.meta.id === activeId) renderTabs(v);
}
api.onIntentionalExit(({ id }) => { setStatus('session closed (detached)'); });
api.onReady(({ id }) => {
  dbg('onReady id=' + id + ' activeId=' + activeId);
  const v = views.get(id);
  if (!v) { dbg('onReady: NO VIEW for id=' + id); return; }
  v.inputReady = true;   // display flag only; the backend already flushed its buffered input
  if (id === activeId) { setStatus(statusLine(v, v.state)); updateConsoleGate(); }
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

function byteLength(s: string): number { return new TextEncoder().encode(s).length; }

// --- resize ---
// The window 'resize' event fires continuously during a drag (dozens/sec). Each fit()+resize
// makes tmux repaint the WHOLE screen via refresh-client -C, so firing per-event floods the
// terminal with full-screen repaints that fight each other (the resize flicker/garble). Debounce:
// coalesce the burst and send ONE resize once the drag settles. Only tell the backend when the
// grid size (cols/rows) actually CHANGED — a pixel drag that doesn't cross a cell boundary needs
// no tmux round-trip.
let _resizeTimer: ReturnType<typeof setTimeout> | null = null;
function applyResize(): void {
  const v = activeId ? views.get(activeId) : undefined;
  if (!v) return;
  const tab = activeTab(v);
  if (!tab || !tab.mounted) return;
  const size = tab.content.fit();   // fit() also resizes the local xterm grid immediately
  if (size && (size.cols !== _lastSize.cols || size.rows !== _lastSize.rows)) {
    _lastSize = size;
    if (activeId) void api.resize(activeId, size.cols, size.rows);
  }
}
window.addEventListener('resize', () => {
  if (_resizeTimer) clearTimeout(_resizeTimer);
  _resizeTimer = setTimeout(applyResize, 120);   // settle window before the single tmux resize
});

// --- new session dialog ---
const dialog = requiredElement<HTMLDialogElement>('dialog');
const fKind = requiredElement<HTMLSelectElement>('f-kind');
const remoteFields = requiredElement<HTMLElement>('remote-fields');
const fHost = requiredElement<HTMLInputElement>('f-host');
const fControl = requiredElement<HTMLButtonElement>('f-control');   // native-mode toggle button
const hostHistoryEl = requiredElement<HTMLElement>('host-history');
const localHint = requiredElement<HTMLElement>('local-hint');
const errorEl = requiredElement<HTMLElement>('f-err');
const titleInput = requiredElement<HTMLInputElement>('f-title');
const sessionForm = requiredElement<HTMLFormElement>('form');

// Native mode is a toggle; default ON. Click flips it.
function setNative(on: boolean): void { fControl.classList.toggle('on', on); fControl.setAttribute('aria-checked', on ? 'true' : 'false'); }
fControl.onclick = () => setNative(!fControl.classList.contains('on'));

const updateFields = () => {
  const remote = fKind.value === 'remote';
  remoteFields.style.display = remote ? 'block' : 'none';
  // The local blurb replaces the ssh one; the Native-tabs toggle sits outside both and applies to
  // either kind, since a local session runs a real tmux too.
  if (localHint) localHint.style.display = remote ? 'none' : 'block';
};
fKind.onchange = updateFields;

requiredElement<HTMLButtonElement>('new').addEventListener('click', () => {
  errorEl.textContent = '';
  setNative(true);                    // default to native mode each time the dialog opens
  updateFields();
  hideHostHistory();
  dialog.showModal();
});

// --- host history dropdown ---
let _hostHistory: string[] = [];
function hideHostHistory(): void { hostHistoryEl.className = ''; hostHistoryEl.innerHTML = ''; }
async function showHostHistory(): Promise<void> {
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

sessionForm.addEventListener('submit', async (event) => {
  const e = event as SubmitEvent;
  // form method=dialog closes automatically; only act on OK
  const ok = e.submitter instanceof HTMLButtonElement && e.submitter.value === 'ok';
  if (!ok) return;
  const kind = fKind.value === 'local' ? 'local' : 'remote';
  const host = fHost.value.trim();
  if (kind === 'remote' && !host) {   // guard: remote needs a host
    e.preventDefault();               // keep the dialog open
    errorEl.textContent = 'Enter a host (user@host).';
    return;
  }
  // A submit event does not await an async listener. Prevent the method=dialog default close now,
  // then close explicitly only after Rust confirms creation; failures remain visible and retryable.
  e.preventDefault();
  errorEl.textContent = '';
  const meta: CreateSessionMeta = {
    kind,
    // ssh is the only REMOTE transport; a local session's tmux is exec'd directly (no ssh).
    transport: kind === 'local' ? 'local' : 'ssh',
    mode: fControl.classList.contains('on') ? 'control' : 'plain',
    title: titleInput.value.trim() || (kind === 'local' ? 'local' : host),
    // The hidden Host input can retain a previous remote value after switching Type. Never persist
    // that stale value onto a local session or the sidebar will mislabel it as remote.
    host: kind === 'local' ? '' : host,
    // NOTE: no session name from the user — main.js generates & owns it.
  };
  let res;
  try {
    res = await api.createSession(meta);
  } catch (err) {
    const message = errorMessage(err) || 'unknown error';
    errorEl.textContent = 'Could not create session: ' + message;
    return;
  }
  const { id, session } = res;
  const viewMeta: SessionMeta = { ...meta, id, session };
  // Adopt what the backend ACTUALLY used. A local session is downgraded control -> plain on tmux
  // < 3.2, and all the way to mode 'local' (a bare pty, no tabs) when tmux isn't installed; the view
  // must know that or it would wait for %window events that never arrive.
  if (res.mode) viewMeta.mode = res.mode;
  if (res.tmuxPath) viewMeta.tmuxPath = res.tmuxPath;
  if (res.tmuxVersion) viewMeta.tmuxVersion = res.tmuxVersion;
  dialog.close('ok');
  const v = makeView(viewMeta);
  v.started = true;         // createSession already ran; mount() must not start it again
  mount(id);
});

// --- restore persisted sessions on launch (lazy: create views, connect on click) ---
async function init(reset = false): Promise<void> {
  if (reset) {
    for (const [, view] of views) teardownViewUi(view);
    views.clear();
    activeId = null;
    for (const id of Object.keys(pendingData)) delete pendingData[id];
    tabsEl.className = '';
    tabsEl.innerHTML = '';
    if (dialog.open) dialog.close();
    sessionForm.reset();
    errorEl.textContent = '';
    setNative(true);
    updateFields();
    _hostHistory = [];
    hideHostHistory();
    updateConsoleGate();
  }
  // §18/§20: load config (loopback hosts + last-active project) before wiring.
  let lastActive: string | null = null;
  try {
    const cfg = await api.getConfig();
    if (cfg && Array.isArray(cfg.loopbackHosts)) loopbackHosts = cfg.loopbackHosts;
    if (cfg && cfg.lastActive) lastActive = cfg.lastActive;
  } catch (_) {}
  const persisted = await api.listSessions();
  dbg('init: ' + persisted.length + ' persisted; lastActive=' + lastActive);
  for (const meta of persisted) {
    // meta already has {id, host, session, transport, title, color, lastTab, tabOrder, tabColors}.
    // kind comes from the persisted transport: a 'local' row must NOT be restored as 'remote', or
    // reconnecting it would build ssh args for an empty host instead of attaching the local tmux.
    const v = makeView({ ...meta, kind: meta.transport === 'local' ? 'local' : 'remote' });
    v.started = false;   // not connected yet; mount() will start (reattach) on click
  }
  renderSidebar();
  // §18: show persisted forwarded ports for every restored session up front (greyed until
  // re-opened), so the list survives an app restart without waiting for a connect.
  for (const meta of persisted) if (!meta.archived) refreshTunnels(meta.id);
  const activePersisted = persisted.filter((meta) => !meta.archived);
  const historyCount = persisted.length - activePersisted.length;
  setStatus(activePersisted.length
    ? `${activePersisted.length} session(s) restored${historyCount ? ` · ${historyCount} in History` : ''}`
    : historyCount ? `${historyCount} closed session(s) in History` : 'no sessions — create one');
  // §20: reopen the LAST-USED project (fall back to the first) so the app resumes where you left off.
  // The project restores its own last-active tab once its windows arrive (see onWindow).
  const autoOpen = activePersisted.filter((meta) => !meta.detached);
  if (autoOpen.length) {
    const first = autoOpen[0];
    if (first) {
      const remembered = lastActive ? views.get(lastActive) : undefined;
      const target = remembered && !remembered.meta.archived && !remembered.meta.detached ? lastActive! : first.id;
      void mount(target);
    }
  }
}

window.__testReset = () => init(true);
void init();

// §18: periodically re-probe the active session's tunnels so a stopped dev server goes grey (and
// a restarted one goes active) without a manual refresh. Light: one call every 5s for the shown one.
setInterval(() => { if (activeId != null && views.has(activeId)) refreshTunnels(activeId); }, 5000);
