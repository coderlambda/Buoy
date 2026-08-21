//! Control-mode backend: attaches a remote tmux session with `-CC` over ssh (via portable-pty)
//! and coordinates the pure units (parser, registry, reply channel, tmux_keys) into a stream of
//! app-level events. Port of src/main/backends/controlModeBackend.js.
//!
//! Threading model: the pty reader runs on its own thread and feeds bytes to the parser; parsed
//! events are handled under a Mutex-guarded `Inner` so writes (input/commands) and reads don't
//! race. App-level events are delivered through a `BackendSink` callback (the session layer wires
//! this to Tauri events).

use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use portable_pty::{CommandBuilder, PtySize, native_pty_system, MasterPty};

use crate::control_parser::{ControlEvent, ControlModeParser};
use crate::reply_channel::{ReplyChannel, ReplyKind};
use crate::session_store::RecoveryWindow;
use crate::tmux_keys::encode_send_keys;
use crate::tmux_socket::socket_name;
use crate::transport::{self, Transport};
use crate::validation;
use crate::window_registry::{PaneRow, WindowRegistry};

// Tabs keep names/paths with spaces intact. Newlines and tabs in persisted values are stripped by
// SessionStore before they can become part of a recovery recipe.
const LIST_FMT: &str = "#{window_id}\t#{pane_id}\t#{pane_active}\t#{window_active}\t#{window_name}\t#{pane_current_path}\t#{pane_current_command}";
const MAX_HISTORY: u32 = 2000;
const MAX_BUFFER: usize = 4096;

/// App-level events the backend emits (consumed by the session layer -> Tauri events).
#[derive(Debug, Clone)]
pub enum BackendEvent {
    /// Terminal output tagged with the window it belongs to.
    /// `repaint` distinguishes a capture-pane snapshot from live PTY output. The frontend uses it
    /// for snapshot-only normalization that must not alter live terminal protocol traffic.
    Data { window: String, data: String, repaint: bool },
    WindowAdd { window: String, order: Vec<String> },
    WindowClose { window: String, order: Vec<String> },
    WindowRename { window: String, name: String },
    WindowActive { window: String, order: Vec<String> },
    /// Latest active-pane cwd/command per window. The session layer persists this without exposing
    /// it as a renderer event; it is used only if the tmux server later disappears.
    RecoverySnapshot { windows: Vec<RecoveryWindow> },
    Ready,
    Exit,
}

pub type BackendSink = Arc<dyn Fn(BackendEvent) + Send + Sync>;

#[derive(Clone)]
pub struct BackendConfig {
    pub host: String,
    pub session: String,
    pub tmux_path: String,
    pub tmux_version: Option<(u32, u32)>,
    /// Resolved socket name. Imported sessions use `default`; Buoy-owned sessions use a private,
    /// versioned socket computed by the session layer.
    pub socket: String,
    pub recovery_windows: Vec<RecoveryWindow>,
    pub base_args: Vec<String>,
    /// ssh to `host`, or a tmux on THIS machine (kind:'local'). Everything downstream — parser,
    /// registry, supervisor, reattach — is identical either way; see transport.rs.
    pub transport: Transport,
}

impl Default for BackendConfig {
    fn default() -> Self {
        BackendConfig {
            host: String::new(), session: String::new(), tmux_path: "tmux".into(),
            tmux_version: None, socket: String::new(), recovery_windows: vec![],
            base_args: vec![], transport: Transport::Ssh,
        }
    }
}

struct Inner {
    parser: ControlModeParser,
    reg: WindowRegistry,
    reply: ReplyChannel,
    writer: Box<dyn Write + Send>,
    sink: BackendSink,
    session: String,
    managed: bool,
    ready: bool,
    attached: bool,
    // Input can arrive before the control client is ready. Preserve its originating window while
    // buffering; otherwise a tab switch during startup/reconnect can replay protocol replies into
    // whichever window happens to be active later.
    pending_input: Vec<(String, Option<String>)>,
    // A capture is a two-reply transaction (cells, then cursor coordinates). Input for that window
    // waits until the repaint is emitted, preventing the first typed command from racing between
    // the snapshot and its cursor query and being echoed on a separate visual row.
    pending_captures: std::collections::BTreeSet<String>,
    pending_output: std::collections::BTreeMap<String, Vec<String>>,
    // Per-pane carry of trailing incomplete UTF-8 bytes. tmux can split a multi-byte char across
    // two %output events for the SAME pane; carrying the partial tail here (keyed by pane, since
    // interleaved panes must not corrupt each other) reassembles it on the next event.
    utf8_carry: std::collections::BTreeMap<String, Vec<u8>>,
    // tmux/screen's terminal-title protocol is `ESC k title ESC \\`. xterm.js does not recognize
    // it and visibly prints `title`, so filter it as a per-pane byte stream (the sequence can split
    // across %output events).
    title_filters: std::collections::BTreeMap<String, TmuxTitleFilter>,
    // Coalescing output buffer: window -> accumulated text awaiting the next flush. A redrawing
    // TUI produces ~750 %output events/sec; emitting one Tauri IPC message each floods and crashes
    // the webview. We batch here and a flush thread emits ONE message per window at ~60fps, which
    // the webview (and xterm) can absorb. Ordering is preserved (append in arrival order).
    out_buf: std::collections::BTreeMap<String, String>,
    refresh_queued: bool,
    stopped: bool,   // set on kill() so the flush thread exits
}

pub struct ControlBackend {
    inner: Arc<Mutex<Inner>>,
    _master: Box<dyn MasterPty + Send>,
    child: Arc<Mutex<Box<dyn portable_pty::Child + Send + Sync>>>,
}

impl ControlBackend {
    /// Build ssh argv, spawn the -CC attach, and start the reader thread. Returns the backend
    /// handle; app events flow through `sink`.
    pub fn spawn(cfg: BackendConfig, sink: BackendSink, cols: u16, rows: u16)
        -> Result<Self, validation::ValidationError>
    {
        let socket = if cfg.socket.is_empty() {
            socket_name("control", cfg.tmux_version, &cfg.session)
        } else {
            cfg.socket.clone()
        };
        let (lc_all, lang) = transport::current_locale();
        let spec = transport::spawn_spec_with_recovery(
            cfg.transport, true, &cfg.host, &cfg.session, &cfg.tmux_path, &socket,
            &cfg.base_args, lc_all.as_deref(), lang.as_deref(), &cfg.recovery_windows,
        )?;
        crate::dlog!("ControlBackend.spawn: socket={} {} {}", socket, spec.program, spec.args.join(" "));

        // Every fallible step below returns Err rather than panicking: this runs on the supervisor's
        // detached backoff thread, where a panic would skip the Ok/Err match that schedules the next
        // retry — wedging the session in Connecting with no error and no working Reconnect button.
        let pty = native_pty_system();
        let pair = pty.openpty(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
            .map_err(|e| validation::ValidationError::spawn("openpty", e))?;

        let mut cmd = CommandBuilder::new(&spec.program);
        cmd.args(&spec.args);
        for (k, v) in &spec.env { cmd.env(k, v); }

        let child = pair.slave.spawn_command(cmd)
            .map_err(|e| validation::ValidationError::spawn("tmux client spawn", e))?;
        drop(pair.slave);

        let writer = pair.master.take_writer()
            .map_err(|e| validation::ValidationError::spawn("pty writer", e))?;
        let mut reader = pair.master.try_clone_reader()
            .map_err(|e| validation::ValidationError::spawn("pty reader", e))?;

        let mut reply = ReplyChannel::new();
        reply.start(); // seed handshake handler

        let inner = Arc::new(Mutex::new(Inner {
            parser: ControlModeParser::new(),
            reg: WindowRegistry::new(),
            reply,
            writer,
            sink: sink.clone(),
            session: cfg.session.clone(),
            managed: socket != "default",
            ready: false,
            attached: false,
            pending_input: Vec::new(),
            pending_captures: std::collections::BTreeSet::new(),
            pending_output: std::collections::BTreeMap::new(),
            utf8_carry: std::collections::BTreeMap::new(),
            title_filters: std::collections::BTreeMap::new(),
            out_buf: std::collections::BTreeMap::new(),
            refresh_queued: false,
            stopped: false,
        }));

        // Reader thread: pump pty bytes -> parser -> event handling.
        {
            let inner = inner.clone();
            let sink = sink.clone();
            thread::spawn(move || {
                crate::dlog!("reader: thread started");
                let mut buf = [0u8; 8192];
                let mut total = 0usize;
                // Decode the control stream as LATIN1 (each byte -> char 0x00..0xFF, lossless and
                // never split). We MUST NOT UTF-8-decode here: tmux can split a multi-byte char
                // (e.g. box-drawing '─' = E2 94 80) ACROSS two %output events — its bytes are not
                // adjacent in the raw stream (there's "\r\n%output %P " between them), so no
                // byte-level carry can rejoin them. Real UTF-8 reassembly happens per-pane at the
                // emit point (route_output), which is the only place a char's bytes are contiguous.
                // '\n'/'\r' are never UTF-8 continuation bytes, so latin1 line-splitting is safe.
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) => { crate::dlog!("reader: EOF after {} bytes", total); break; }
                        Ok(n) => {
                            total += n;
                            let chunk: String = buf[..n].iter().map(|&b| b as char).collect();
                            let events = {
                                let mut g = inner.lock().unwrap();
                                g.parser.write(&chunk)
                            };
                            for ev in events {
                                Inner::handle_event(&inner, ev);
                            }
                        }
                        Err(e) => { crate::dlog!("reader: read error: {}", e); break; }
                    }
                }
                // Reader ended (EOF / error / ssh died): stop the flush thread and flush any tail,
                // then signal exit. Without setting `stopped` here the flush thread would leak for
                // every backend the supervisor spawns over a session's lifetime.
                { let mut g = inner.lock().unwrap(); g.stopped = true; g.flush_output(); }
                sink(BackendEvent::Exit);
            });
        }

        // Output flush thread: coalesce buffered %output into one Tauri message per window at
        // ~60fps. This is what keeps a redrawing TUI (~750 events/sec) from flooding and crashing
        // the webview. Exits when the pty is gone (reader set an exit flag by dropping — we detect
        // via a weak count check: once the only Arc left is ours, stop).
        {
            let inner = inner.clone();
            thread::spawn(move || {
                loop {
                    thread::sleep(Duration::from_millis(16));
                    // Stop when the backend has been dropped (no external owners besides this loop
                    // and the other worker threads' clones would keep it alive; use a killed flag).
                    let mut g = inner.lock().unwrap();
                    if g.stopped { break; }
                    g.flush_output();
                }
            });
        }

        // `cd` does not necessarily trigger a tmux topology event. Poll at a deliberately low
        // cadence so a host-restart recipe eventually captures the current directory without
        // coupling discovery to terminal output volume (a busy TUI can emit hundreds/sec).
        {
            let inner = inner.clone();
            thread::spawn(move || loop {
                thread::sleep(Duration::from_secs(15));
                let mut g = inner.lock().unwrap();
                if g.stopped { break; }
                g.refresh_now();
            });
        }

        // Spawn-time ready fallback (backend owns input gating): guarantee ready even if
        // %session-changed never arrives, so buffered input can't strand.
        {
            let inner = inner.clone();
            thread::spawn(move || {
                thread::sleep(Duration::from_millis(5000));
                Inner::mark_ready(&inner);
            });
        }

        Ok(ControlBackend {
            inner,
            _master: pair.master,
            child: Arc::new(Mutex::new(child)),
        })
    }

    /// Shell input -> send-keys addressed to the active window (buffered until ready).
    pub fn write(&self, data: &str) {
        self.write_to(data, None);
    }

    /// Input from a particular xterm must return to that same tmux window. xterm emits terminal
    /// protocol replies through the same onData path as keyboard input; the selected-window state
    /// can be briefly stale while tabs switch.
    pub fn write_to(&self, data: &str, target: Option<&str>) {
        let target = target.filter(|w| is_win_id(w)).map(str::to_owned);
        self.inner.lock().unwrap().write_input(data, target);
    }

    pub fn resize(&self, cols: u16, rows: u16) {
        let mut g = self.inner.lock().unwrap();
        g.send(format!("refresh-client -C {}x{}", cols, rows), ReplyKind::Ignore);
        let _ = self._master.resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 });
    }

    pub fn new_window(&self) {
        let mut g = self.inner.lock().unwrap();
        let session = g.session.clone();
        g.send(format!("new-window -t {} -c \"#{{pane_current_path}}\"", session), ReplyKind::Ignore);
    }

    pub fn select_window(&self, win: &str) {
        if !is_win_id(win) { return; }
        let mut g = self.inner.lock().unwrap();
        g.send(format!("select-window -t {}", win), ReplyKind::Ignore);
    }

    pub fn kill_window(&self, win: &str) {
        if !is_win_id(win) { return; }
        let mut g = self.inner.lock().unwrap();
        g.send(format!("kill-window -t {}", win), ReplyKind::Ignore);
    }

    /// Manually rename a window. tmux's `rename-window` also disables automatic-rename for that
    /// window, so a user-set title sticks (stops following the pane title). The title is sanitized
    /// to a safe single-line subset since it is interpolated into a control-mode command.
    pub fn rename_window(&self, win: &str, title: &str) {
        if !is_win_id(win) { return; }
        let clean = sanitize_window_name(title);
        let mut g = self.inner.lock().unwrap();
        if clean.is_empty() {
            // empty title -> re-enable automatic-rename so it follows the pane title again
            g.send(format!("set-window-option -t {} automatic-rename on", win), ReplyKind::Ignore);
        } else {
            g.send(format!("rename-window -t {} \"{}\"", win, clean), ReplyKind::Ignore);
        }
    }

    pub fn capture_window(&self, win: &str) {
        if !is_win_id(win) { return; }
        let mut g = self.inner.lock().unwrap();
        // One backfill per window at a time. The renderer also guards this, but keeping the
        // transaction invariant here protects reconnect races and direct command callers.
        if !g.pending_captures.insert(win.to_string()) { return; }
        g.send(
            // Keep physical pane rows physical. `-J` joins wrapped rows and `-N` preserves padding;
            // replaying that text can reflow to a different height in xterm and separate the prompt
            // from the cursor even when both terminals have the same dimensions.
            format!("capture-pane -p -e -q -S -{} -t {}", MAX_HISTORY, win),
            ReplyKind::Capture { window: Some(win.to_string()) },
        );
    }

    pub fn kill(&self) {
        { let mut g = self.inner.lock().unwrap(); g.stopped = true; g.flush_output(); }
        if let Ok(mut c) = self.child.lock() {
            let _ = c.kill();
        }
    }
}

impl Inner {
    fn send(&mut self, line: String, kind: ReplyKind) {
        self.reply.expect(kind);
        let _ = self.writer.write_all(line.as_bytes());
        let _ = self.writer.write_all(b"\n");
        let _ = self.writer.flush();
    }

    fn emit(&self, ev: BackendEvent) {
        (self.sink)(ev);
    }

    fn handle_event(inner: &Arc<Mutex<Inner>>, ev: ControlEvent) {
        match ev {
            ControlEvent::Output { pane, data } => {
                let need_refresh = {
                    let mut g = inner.lock().unwrap();
                    g.route_output(pane, data)
                };
                // Output from a pane we have no window mapping for: schedule a topology refresh to
                // learn it. Done HERE, outside the lock, so it can go through the coalescing
                // queue_refresh (which needs the lock to arm its timer) instead of firing a
                // synchronous list-panes per event — an unmapped pane redrawing at ~750 events/sec
                // used to send ~750 list-panes commands, one per event.
                if need_refresh { Inner::queue_refresh(inner); }
            }
            ControlEvent::WindowAdd { .. }
            | ControlEvent::WindowClose { .. }
            | ControlEvent::WindowRenamed { .. }
            | ControlEvent::WindowPaneChanged { .. }
            | ControlEvent::SessionWindowChanged { .. }
            | ControlEvent::LayoutChange { .. } => {
                crate::dlog!("event: {:?} -> queue refresh", ev);
                Inner::queue_refresh(inner);
            }
            ControlEvent::SessionChanged { .. } => {
                crate::dlog!("event: {:?} -> on_attach", ev);
                Inner::on_attach(inner);
            }
            ControlEvent::Exit { .. } => {
                crate::dlog!("event: {:?} -> emit Exit", ev);
                inner.lock().unwrap().emit(BackendEvent::Exit);
            }
            ControlEvent::Reply { ok, body, .. } => {
                crate::dlog!("event: Reply ok={} bodyLines={}", ok, body.len());
                Inner::on_reply(inner, ok, body);
            }
            ControlEvent::Begin { .. } => {}
            other => { crate::dlog!("event: unhandled {:?}", other); }
        }
    }

    /// Consume one reply block. `ok` is false for a tmux `%error` block, whose body is a DIAGNOSTIC
    /// ("can't find window: @9"), not command output. We must still `take()` the queued kind to keep
    /// the FIFO aligned with the commands we sent (tmux emits exactly one block per command, error or
    /// not), but the body must NOT be interpreted: routing an error body to paint_capture used to
    /// clear-screen the terminal and paint the tmux diagnostic as counterfeit scrollback, and routing
    /// it to apply_topology would parse it as pane rows.
    fn on_reply(inner: &Arc<Mutex<Inner>>, ok: bool, body: Vec<String>) {
        let kind = { inner.lock().unwrap().reply.take() };
        let kind_name = match kind.as_ref() {
            Some(ReplyKind::Ignore) => "ignore",
            Some(ReplyKind::Topology) => "topology",
            Some(ReplyKind::Capture { .. }) => "capture",
            Some(ReplyKind::CaptureCursor { .. }) => "capture-cursor",
            None => "unexpected",
        };
        // Do not Debug-print `kind`: CaptureCursor deliberately carries up to MAX_HISTORY captured
        // rows while awaiting the coordinate reply, which would flood the opt-in diagnostic log.
        crate::dlog!("on_reply: kind={} ok={} bodyLines={}", kind_name, ok, body.len());
        if !ok {
            crate::dlog!("on_reply: %error for kind={}, body discarded: {:?}", kind_name, body);
            let failed_window = match kind.as_ref() {
                Some(ReplyKind::Capture { window: Some(window) }) => Some(window.clone()),
                Some(ReplyKind::CaptureCursor { window, .. }) => Some(window.clone()),
                _ => None,
            };
            if let Some(window) = failed_window {
                inner.lock().unwrap().finish_capture(&window);
            }
            return;
        }
        match kind {
            Some(ReplyKind::Topology) => {
                let lines: Vec<String> = body.into_iter().filter(|l| !l.trim().is_empty()).collect();
                Inner::apply_topology(inner, lines);
            }
            Some(ReplyKind::Capture { window }) => {
                Inner::request_capture_cursor(inner, window, body);
            }
            Some(ReplyKind::CaptureCursor { window, body: capture }) => {
                if let Some((x, y)) = parse_cursor_position(&body) {
                    Inner::paint_capture(inner, window, capture, x, y);
                } else {
                    inner.lock().unwrap().finish_capture(&window);
                }
            }
            _ => {}
        }
    }

    fn queue_refresh(inner: &Arc<Mutex<Inner>>) {
        {
            let mut g = inner.lock().unwrap();
            if g.refresh_queued { return; }
            g.refresh_queued = true;
        }
        // Coalesce a burst of topology signals into one list-panes round-trip.
        let inner2 = inner.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(10));
            let mut g = inner2.lock().unwrap();
            g.refresh_queued = false;
            let session = g.session.clone();
            g.send(format!("list-panes -s -t {} -F '{}'", session, LIST_FMT), ReplyKind::Topology);
        });
    }

    fn refresh_now(&mut self) {
        let session = self.session.clone();
        self.send(format!("list-panes -s -t {} -F '{}'", session, LIST_FMT), ReplyKind::Topology);
    }

    fn apply_topology(inner: &Arc<Mutex<Inner>>, lines: Vec<String>) {
        let mut g = inner.lock().unwrap();
        let mut rows = Vec::new();
        for raw in &lines {
            if let Some(r) = parse_list_row(raw.trim()) {
                rows.push(r);
            }
        }
        crate::dlog!("apply_topology: {} rows parsed from {} lines", rows.len(), lines.len());
        if rows.is_empty() {
            // Nothing to reconcile, but still drain buffered input: this early return used to skip
            // the flush_pending_input() at the end of this fn, which is the documented safety net for
            // "mark_ready raced ahead of topology". An empty/unparseable reply must not strand keys.
            g.flush_pending_input();
            return;
        }
        let diff = g.reg.reconcile(&rows);
        let order = g.reg.order();
        crate::dlog!("apply_topology: added={:?} removed={:?} active={:?}", diff.added, diff.removed, diff.active);

        // Emit any buffered output first so window add/active can't be reordered ahead of it.
        g.flush_output();
        let recovery = recovery_snapshot(&rows, &order, g.reg.active_window.as_deref());
        if !recovery.is_empty() {
            g.emit(BackendEvent::RecoverySnapshot { windows: recovery });
        }
        for win in &diff.added {
            // A newly-added window already has a name in tmux (auto-derived or a manual rename that
            // persisted server-side). `added` alone carries only the id, so emit its name too —
            // otherwise a reconnect/app-reopen shows the tab as "@N" instead of its real title.
            let name = g.reg.name_of(win).filter(|n| n != win);
            g.emit(BackendEvent::WindowAdd { window: win.clone(), order: order.clone() });
            if let Some(name) = name {
                g.emit(BackendEvent::WindowRename { window: win.clone(), name: latin1_to_utf8(&name) });
            }
        }
        for (win, name) in &diff.renamed {
            g.emit(BackendEvent::WindowRename { window: win.clone(), name: latin1_to_utf8(name) });
        }
        for win in &diff.removed {
            g.emit(BackendEvent::WindowClose { window: win.clone(), order: order.clone() });
        }
        if diff.active_changed {
            if let Some(active) = &diff.active {
                g.emit(BackendEvent::WindowActive { window: active.clone(), order: order.clone() });
            }
        }
        // Flush output buffered before its window was known.
        for pane in &diff.newly_mapped_panes {
            g.flush_pane_buffer(pane);
        }
        // If we became ready before the active window was known (mark_ready ran first on a slow
        // reconnect), input piled up in pending_input with no drain path. Now that reconcile has set
        // active_window, flush it — otherwise the session shows "connected" but ignores keystrokes.
        g.flush_pending_input();
    }

    /// Returns true if the pane has no window mapping yet, meaning the CALLER should schedule a
    /// topology refresh (see handle_event). We can't do it here: queue_refresh takes the same lock
    /// this method is already holding.
    fn route_output(&mut self, pane: String, data: String) -> bool {
        // `data` is LATIN1 (one char per raw byte, from the reader + octal unescape). Convert back
        // to bytes, prepend this pane's carried incomplete UTF-8 tail, decode the valid prefix, and
        // stash any new incomplete tail. This is where a char split across two %output events for
        // the same pane is reassembled.
        let raw: Vec<u8> = data.chars().map(|c| c as u8).collect();
        let filtered = self.title_filters.entry(pane.clone()).or_default().push(&raw);
        let mut bytes: Vec<u8> = Vec::new();
        if let Some(carry) = self.utf8_carry.remove(&pane) { bytes.extend_from_slice(&carry); }
        bytes.extend(filtered);
        let (text, tail) = decode_utf8_prefix(&bytes);
        if !tail.is_empty() { self.utf8_carry.insert(pane.clone(), tail); }
        if text.is_empty() { return false; }

        match self.reg.win_for_pane(&pane) {
            // Buffer into out_buf; the flush thread emits one coalesced message per window ~60fps.
            Some(win) => { self.out_buf.entry(win).or_default().push_str(&text); false }
            None => {
                let buf = self.pending_output.entry(pane).or_default();
                if buf.len() >= MAX_BUFFER { buf.remove(0); }
                buf.push(text);
                true
            }
        }
    }

    // Emit all buffered per-window output as one message each, then clear. Called on the flush
    // timer (coalescing the %output firehose) and before any ordered event (window/capture) so
    // buffered output can't jump ahead of a window add/active/scrollback paint.
    fn flush_output(&mut self) {
        if self.out_buf.is_empty() { return; }
        let batch = std::mem::take(&mut self.out_buf);
        for (window, data) in batch {
            if !data.is_empty() {
                (self.sink)(BackendEvent::Data { window, data, repaint: false });
            }
        }
    }

    fn flush_pane_buffer(&mut self, pane: &str) {
        if let Some(buf) = self.pending_output.remove(pane) {
            if let Some(win) = self.reg.win_for_pane(pane) {
                // Route through the coalescing buffer (same ordering guarantees as live output).
                self.out_buf.entry(win).or_default().push_str(&buf.concat());
            }
        }
    }

    fn on_attach(inner: &Arc<Mutex<Inner>>) {
        {
            let mut g = inner.lock().unwrap();
            if g.attached { crate::dlog!("on_attach: already attached, skip"); return; }
            g.attached = true;
            crate::dlog!("on_attach: refreshing topology");
            // Tab titles follow the pane title (OSC 2 / `\e]2;…`), the standard, program-agnostic
            // way any tool (Claude Code, vim, a shell PROMPT_COMMAND, …) names its tab. Fall back to
            // the running command when the title is just tmux's default (== the host name), so an
            // untitled shell shows "zsh" rather than the hostname. This drives %window-renamed,
            // which the renderer already maps to the tab label. A manual rename turns automatic-
            // rename off for that window (tmux does this itself when you set-option the name).
            if g.managed {
                g.send("set-option -g automatic-rename on".into(), ReplyKind::Ignore);
                g.send(
                    "set-option -g automatic-rename-format \
                     '#{?#{==:#{pane_title},#{host}},#{pane_current_command},#{pane_title}}'".into(),
                    ReplyKind::Ignore,
                );
            }
            g.refresh_now();
        }
        // fast-path ready shortly after attach (topology reply has landed)
        let inner2 = inner.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(500));
            Inner::mark_ready(&inner2);
        });
    }

    fn mark_ready(inner: &Arc<Mutex<Inner>>) {
        let mut g = inner.lock().unwrap();
        // A killed/dead backend must never announce Ready. The 5s spawn-time fallback timer is
        // detached and uncancellable, so without this check a backend killed at t=1s still emits
        // Ready at t=5s — and the supervisor would apply it to whatever session is live by then
        // (flipping Dead -> Connected and bricking the Reconnect button, which requires Dead).
        if g.stopped || g.ready { return; }
        g.ready = true;
        crate::dlog!("mark_ready: active={:?} pending_input={}", g.reg.active_window, g.pending_input.len());
        g.flush_pending_input();
        // If we went ready WITHOUT knowing the active window yet (a timer fired before the topology
        // reply landed, or %session-changed was missed on a degraded reconnect), any input the user
        // types now would be buffered with no way to drain it — write()'s send path needs an active
        // window, and mark_ready won't run again. Force a topology refresh so active_window gets set;
        // the resulting apply_topology drains pending_input. Without this the session looks
        // "connected" but silently eats keystrokes.
        if g.reg.active_window.is_none() {
            crate::dlog!("mark_ready: no active window yet -> forcing topology refresh");
            g.refresh_now();
        }
        g.emit(BackendEvent::Ready);
    }

    /// Route shell/xterm input to its explicit originating window when provided, otherwise to the
    /// active window. Buffer until ready; an unaddressed item also waits for topology.
    fn write_input(&mut self, data: &str, target: Option<String>) {
        match target.clone().or_else(|| self.reg.active_window.clone()) {
            Some(t) if self.ready && !self.pending_captures.contains(&t) => {
                for line in encode_send_keys(data, &t) { self.send(line, ReplyKind::Ignore); }
            }
            _ => {
                if self.pending_input.len() >= MAX_BUFFER { self.pending_input.remove(0); }
                self.pending_input.push((data.to_string(), target));
            }
        }
    }

    /// Send buffered input to its explicit window, or the active window for legacy/unaddressed
    /// items. Called from mark_ready and, to cover the race where topology lands after we're
    /// already ready, from apply_topology.
    fn flush_pending_input(&mut self) {
        if !self.ready || self.pending_input.is_empty() { return; }
        let active = self.reg.active_window.clone();
        let queued = std::mem::take(&mut self.pending_input);
        crate::dlog!("flush_pending_input: {} chunks active={:?}", queued.len(), active);
        for (data, explicit) in queued {
            let target = explicit.clone().or_else(|| active.clone());
            if let Some(target) = target.filter(|t| !self.pending_captures.contains(t)) {
                for line in encode_send_keys(&data, &target) {
                    self.send(line, ReplyKind::Ignore);
                }
            } else {
                // Topology may still be unknown, or this window's capture/cursor transaction may
                // still be pending. Preserve the original addressing until it becomes sendable.
                self.pending_input.push((data, explicit));
            }
        }
    }

    fn finish_capture(&mut self, window: &str) {
        self.pending_captures.remove(window);
        self.flush_pending_input();
    }

    /// `capture-pane` does not include the pane's cursor position. Query it immediately after the
    /// capture and carry the captured rows in the reply tag so the two FIFO replies stay paired.
    fn request_capture_cursor(
        inner: &Arc<Mutex<Inner>>,
        window: Option<String>,
        body: Vec<String>,
    ) {
        let mut g = inner.lock().unwrap();
        let target = window.or_else(|| g.reg.active_window.clone());
        if let Some(t) = target {
            g.send(
                format!("display-message -p -t {} '#{{cursor_x}} #{{cursor_y}}'", t),
                ReplyKind::CaptureCursor { window: t, body },
            );
        }
    }

    fn paint_capture(
        inner: &Arc<Mutex<Inner>>,
        window: String,
        body: Vec<String>,
        cursor_x: usize,
        cursor_y: usize,
    ) {
        let data = capture_repaint(&body, cursor_x, cursor_y);
        let mut g = inner.lock().unwrap();
        g.flush_output();   // paint scrollback AFTER any buffered live output, never before
        g.emit(BackendEvent::Data { window: window.clone(), data, repaint: true });
        // The repaint event is emitted before buffered input reaches tmux, so its later echo is
        // ordered after the restored screen/cursor in the frontend.
        g.finish_capture(&window);
    }
}

#[derive(Default)]
struct TmuxTitleFilter {
    state: TitleFilterState,
}

#[derive(Default)]
enum TitleFilterState {
    #[default]
    Normal,
    SawEsc,
    InTitle,
    InTitleSawEsc,
}

impl TmuxTitleFilter {
    /// Remove tmux/screen's `ESC k title ESC \\` (or BEL-terminated) title sequence while
    /// preserving every other byte exactly. State is retained so any byte boundary is safe.
    fn push(&mut self, input: &[u8]) -> Vec<u8> {
        use TitleFilterState::*;
        let mut out = Vec::with_capacity(input.len());
        for &byte in input {
            self.state = match self.state {
                Normal if byte == 0x1b => SawEsc,
                Normal => { out.push(byte); Normal }
                SawEsc if byte == b'k' => InTitle,
                SawEsc if byte == 0x1b => { out.push(0x1b); SawEsc }
                SawEsc => { out.extend_from_slice(&[0x1b, byte]); Normal }
                InTitle if byte == 0x07 => Normal,
                InTitle if byte == 0x1b => InTitleSawEsc,
                InTitle => InTitle,
                InTitleSawEsc if byte == b'\\' => Normal,
                InTitleSawEsc if byte == 0x1b => InTitleSawEsc,
                InTitleSawEsc => InTitle,
            };
        }
        out
    }
}

/// Decode tmux's `display-message '#{cursor_x} #{cursor_y}'` reply.
fn parse_cursor_position(body: &[String]) -> Option<(usize, usize)> {
    let mut fields = body.first()?.split_whitespace();
    let x = fields.next()?.parse().ok()?;
    let y = fields.next()?.parse().ok()?;
    if fields.next().is_some() { return None; }
    Some((x, y))
}

/// Repaint a tmux capture and finish at tmux's real cursor instead of after the final text row.
/// Trailing blank screen rows are significant and deliberately preserved.
fn capture_repaint(body: &[String], cursor_x: usize, cursor_y: usize) -> String {
    // Capture body lines are LATIN1 (like %output); a whole capture line's bytes are contiguous, so
    // decode each line latin1 -> UTF-8 (no cross-line carry needed).
    let joined = body.join("\r\n");
    let bytes: Vec<u8> = joined.chars().map(|c| c as u8).collect();
    let (text, _tail) = decode_utf8_prefix(&bytes);
    format!(
        "\x1b[H\x1b[2J{}\x1b[{};{}H",
        text,
        cursor_y.saturating_add(1),
        cursor_x.saturating_add(1),
    )
}

// "@win<TAB>%pane<TAB>paneActive<TAB>winActive<TAB>name<TAB>cwd<TAB>command"
fn parse_list_row(line: &str) -> Option<PaneRow> {
    let mut parts = line.splitn(7, '\t');
    let win = parts.next()?;
    let pane = parts.next()?;
    let pane_active = parts.next()?;
    let win_active = parts.next()?;
    let name = parts.next().unwrap_or("");
    let cwd = parts.next().unwrap_or("");
    let command = parts.next().unwrap_or("");
    if !win.starts_with('@') || !pane.starts_with('%') { return None; }
    if pane_active != "0" && pane_active != "1" { return None; }
    if win_active != "0" && win_active != "1" { return None; }
    Some(PaneRow {
        win: win.to_string(),
        pane: pane.to_string(),
        pane_active: pane_active == "1",
        win_active: win_active == "1",
        name: name.to_string(),
        cwd: cwd.to_string(),
        command: command.to_string(),
    })
}

fn recovery_snapshot(rows: &[PaneRow], order: &[String], active: Option<&str>) -> Vec<RecoveryWindow> {
    order.iter().filter_map(|window| {
        let row = rows.iter().find(|row| &row.win == window && row.pane_active)
            .or_else(|| rows.iter().find(|row| &row.win == window))?;
        Some(RecoveryWindow {
            name: latin1_to_utf8(&row.name),
            cwd: latin1_to_utf8(&row.cwd),
            command: latin1_to_utf8(&row.command),
            active: active == Some(window.as_str()),
        })
    }).collect()
}

fn is_win_id(s: &str) -> bool {
    s.starts_with('@') && s.len() > 1 && s[1..].chars().all(|c| c.is_ascii_digit())
}

/// Re-decode a latin1 string (as produced by the control-stream reader, one char per byte) back to
/// UTF-8, so a window name / title with multibyte chars renders correctly. Lossy on invalid seqs.
fn latin1_to_utf8(s: &str) -> String {
    let bytes: Vec<u8> = s.chars().map(|c| c as u8).collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Sanitize a user-supplied window title for safe interpolation into a `rename-window "…"` command:
/// strip control chars (incl. newlines) and the two chars that break the double-quoted argument
/// (`"` and `\`), collapse whitespace, and cap the length. Keeps everything else (spaces, `*`,
/// unicode) so titles like "* Building widget" survive intact.
///
/// `#` is DOUBLED (`##`), which is tmux's literal escape for it. Left bare, tmux expands the title
/// as a format string: a remote program setting its pane title to `#{pane_current_path}` would put
/// the resolved path in the tab label instead of the literal text (verified against tmux 3.6a;
/// `#()` did not execute there, but doubling closes the whole class rather than betting on version
/// behaviour). Length is capped on the INPUT chars so doubling can't push the result past tmux's
/// limits.
fn sanitize_window_name(s: &str) -> String {
    let mut out = String::new();
    let mut kept = 0usize;
    let mut prev_space = false;
    for c in s.chars() {
        if c.is_control() || c == '"' || c == '\\' { continue; }
        if c == ' ' {
            if prev_space { continue; }
            prev_space = true;
        } else {
            prev_space = false;
        }
        if c == '#' { out.push('#'); }   // tmux literal escape: '#' -> '##'
        out.push(c);
        kept += 1;
        if kept >= 100 { break; }
    }
    // Trailing '#' would be a dangling escape; it can only appear as a complete '##' pair here.
    out.trim().to_string()
}

/// Decode the longest valid UTF-8 prefix of `bytes`. Returns the decoded string and any trailing
/// bytes that form an INCOMPLETE (but so-far-valid) multi-byte sequence, to be carried into the
/// next read. Genuinely invalid bytes (not just truncated) are passed through lossily so we never
/// stall. This is what stops box-drawing/other multi-byte chars from being corrupted to U+FFFD
/// when they straddle a read boundary.
/// ITERATIVE, not recursive: a pane spewing binary (`cat some.jpg`) delivers a long run of invalid
/// bytes, and one stack frame per bad byte overflows the reader thread's 2 MiB stack and ABORTS the
/// process (SIGABRT — not a catchable panic, so it takes every session down with it). Measured
/// thresholds before this was a loop: ~2–4k invalid bytes in debug, ~10–100k in release, against
/// 8192-byte reads. Keep this loop-shaped.
fn decode_utf8_prefix(bytes: &[u8]) -> (String, Vec<u8>) {
    let mut out = String::new();
    let mut rest = bytes;
    loop {
        match std::str::from_utf8(rest) {
            Ok(s) => {
                out.push_str(s);
                return (out, Vec::new());
            }
            Err(e) => {
                let valid = e.valid_up_to();
                // SAFETY: rest[..valid] is valid UTF-8 by definition of valid_up_to().
                out.push_str(unsafe { std::str::from_utf8_unchecked(&rest[..valid]) });
                match e.error_len() {
                    // None => the tail is a truncated-but-valid sequence: carry it for the next read.
                    None => return (out, rest[valid..].to_vec()),
                    // Some(len) => genuinely invalid bytes: emit the replacement char and skip them,
                    // then keep decoding the remainder (so one bad byte can't wedge the stream).
                    Some(len) => {
                        out.push('\u{FFFD}');
                        rest = &rest[valid + len..];
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    // A Write that records everything sent to it (the control-mode commands the backend flushes).
    #[derive(Clone)]
    struct CaptureWriter(Arc<Mutex<Vec<u8>>>);
    impl std::io::Write for CaptureWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> { self.0.lock().unwrap().extend_from_slice(buf); Ok(buf.len()) }
        fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
    }

    // Build an Inner with a capturing writer and a no-op sink, for driving the input/ready/topology
    // state machine directly (no ssh/pty).
    fn test_inner_with_sink(sink: BackendSink) -> (Arc<Mutex<Inner>>, Arc<Mutex<Vec<u8>>>) {
        let sent = Arc::new(Mutex::new(Vec::<u8>::new()));
        let mut reply = ReplyChannel::new();
        reply.start();
        let inner = Arc::new(Mutex::new(Inner {
            parser: ControlModeParser::new(),
            reg: WindowRegistry::new(),
            reply,
            writer: Box::new(CaptureWriter(sent.clone())),
            sink,
            session: "s".into(),
            managed: true,
            ready: false,
            attached: false,
            pending_input: Vec::new(),
            pending_captures: std::collections::BTreeSet::new(),
            pending_output: std::collections::BTreeMap::new(),
            utf8_carry: std::collections::BTreeMap::new(),
            title_filters: std::collections::BTreeMap::new(),
            out_buf: std::collections::BTreeMap::new(),
            refresh_queued: false,
            stopped: false,
        }));
        (inner, sent)
    }

    fn test_inner() -> (Arc<Mutex<Inner>>, Arc<Mutex<Vec<u8>>>) {
        test_inner_with_sink(Arc::new(|_| {}))
    }

    fn sent_str(sent: &Arc<Mutex<Vec<u8>>>) -> String {
        String::from_utf8_lossy(&sent.lock().unwrap()).into_owned()
    }

    // REGRESSION: "connected but can't input" after a slow reconnect. mark_ready can fire (from its
    // timer) BEFORE the topology reply lands, so active_window is still None. Input typed in that
    // window is buffered into pending_input, and since mark_ready won't run again the buffer would
    // strand — the session looks connected but eats keystrokes. apply_topology (once the reply
    // arrives) must drain pending_input.
    #[test]
    fn tc_cb_ready_before_topology_drains_input_after_topology() {
        let (inner, sent) = test_inner();
        // Ready arrives with no active window known yet.
        Inner::mark_ready(&inner);
        assert!(inner.lock().unwrap().ready, "ready set");
        assert!(inner.lock().unwrap().reg.active_window.is_none(), "no active window yet");

        // User types while "connected but not yet topologized" -> buffered, nothing sent as input.
        sent.lock().unwrap().clear();
        inner.lock().unwrap().write_input("echo hi\n", None);
        assert!(!sent_str(&sent).contains("send-keys"), "input must NOT be sent before topology");
        assert_eq!(inner.lock().unwrap().pending_input.len(), 1, "input buffered");

        // Topology reply lands -> active window becomes @0 -> buffered input flushes as send-keys.
        Inner::apply_topology(&inner, vec!["@0\t%0\t1\t1\tzsh\t/tmp\tzsh".to_string()]);
        assert_eq!(inner.lock().unwrap().reg.active_window.as_deref(), Some("@0"));
        assert!(sent_str(&sent).contains("send-keys"), "pending input flushed after topology");
        assert!(inner.lock().unwrap().pending_input.is_empty(), "pending_input drained");
    }

    // The reverse order (topology first, then Ready) must also end up sending buffered input.
    #[test]
    fn tc_cb_topology_before_ready_drains_input_on_ready() {
        let (inner, sent) = test_inner();
        Inner::apply_topology(&inner, vec!["@0\t%0\t1\t1\tzsh\t/tmp\tzsh".to_string()]);
        // Not ready yet: input buffers even though we know the window.
        inner.lock().unwrap().write_input("ls\n", None);
        assert!(!sent_str(&sent).contains("send-keys"), "not sent before ready");
        sent.lock().unwrap().clear();
        Inner::mark_ready(&inner);
        assert!(sent_str(&sent).contains("send-keys"), "flushed on ready");
        assert!(inner.lock().unwrap().pending_input.is_empty());
    }

    // REGRESSION: xterm answers terminal queries through onData. While switching tabs, the
    // registry's active window can still be the tab we just left; the reply must follow the xterm
    // that produced it rather than that stale active-window value.
    #[test]
    fn tc_cb_explicit_input_target_wins_over_active_window() {
        let (inner, sent) = test_inner();
        Inner::apply_topology(&inner, vec![
            "@0\t%0\t1\t1\tcodex\t/tmp/a\tcodex".to_string(),
            "@1\t%1\t1\t0\tcodex\t/tmp/b\tcodex".to_string(),
        ]);
        Inner::mark_ready(&inner);
        assert_eq!(inner.lock().unwrap().reg.active_window.as_deref(), Some("@0"));

        sent.lock().unwrap().clear();
        inner.lock().unwrap().write_input("\x1b]10;rgb:cdcd/d6d6/f4f4\x1b\\", Some("@1".into()));
        let commands = sent_str(&sent);
        assert!(commands.contains("send-keys -t @1"), "reply routed to its source tab: {commands}");
        assert!(!commands.contains("send-keys -t @0"), "stale active tab must receive no reply: {commands}");
    }

    #[test]
    fn tc_cb_buffer_preserves_explicit_input_target() {
        let (inner, sent) = test_inner();
        inner.lock().unwrap().write_input("query reply", Some("@7".into()));
        assert_eq!(inner.lock().unwrap().pending_input.len(), 1);

        // Ready can precede topology. Explicitly addressed input needs no active-window lookup and
        // can be delivered immediately without being stranded or redirected.
        Inner::mark_ready(&inner);
        let commands = sent_str(&sent);
        assert!(commands.contains("send-keys -t @7 -l \"query reply\""),
            "buffer retained explicit target: {commands}");
        assert!(inner.lock().unwrap().pending_input.is_empty());
    }

    #[test]
    fn tc_cb_capture_repaint_restores_tmux_cursor_without_extra_line() {
        // Codex commonly keeps its input cursor above a footer. The capture's final text row is
        // therefore not a usable cursor proxy, and the old synthetic trailing CRLF was one row
        // worse again. tmux coordinates are zero-based; CSI H is one-based.
        let data = capture_repaint(
            &["history".into(), "> prompt".into(), "footer".into(), "".into()],
            2,
            1,
        );
        assert_eq!(
            data,
            "\x1b[H\x1b[2Jhistory\r\n> prompt\r\nfooter\r\n\x1b[2;3H"
        );
        assert!(!data.ends_with("\r\n"), "repaint must finish at the reported cursor");
    }

    #[test]
    fn tc_cb_capture_is_tagged_as_repaint_but_live_output_is_not() {
        let events = Arc::new(Mutex::new(Vec::<BackendEvent>::new()));
        let captured = events.clone();
        let (inner, _) = test_inner_with_sink(Arc::new(move |event| {
            captured.lock().unwrap().push(event);
        }));

        {
            let mut g = inner.lock().unwrap();
            g.out_buf.insert("@4".into(), "live".into());
            g.flush_output();
        }
        Inner::paint_capture(&inner, "@4".into(), vec!["history".into()], 0, 0);

        let events = events.lock().unwrap();
        assert!(matches!(events.first(), Some(BackendEvent::Data { repaint: false, .. })));
        assert!(matches!(events.get(1), Some(BackendEvent::Data { repaint: true, .. })));
    }

    #[test]
    fn tc_cb_capture_queues_cursor_query_with_original_rows() {
        let (inner, sent) = test_inner();
        assert_eq!(inner.lock().unwrap().reply.take(), Some(ReplyKind::Ignore));
        let rows = vec!["input".to_string(), "status".to_string()];

        Inner::request_capture_cursor(&inner, Some("@4".into()), rows.clone());
        assert!(sent_str(&sent).contains(
            "display-message -p -t @4 '#{cursor_x} #{cursor_y}'"
        ));
        assert_eq!(
            inner.lock().unwrap().reply.take(),
            Some(ReplyKind::CaptureCursor { window: "@4".into(), body: rows })
        );
    }

    #[test]
    fn tc_cb_parse_cursor_position_is_strict() {
        assert_eq!(parse_cursor_position(&["2 37".into()]), Some((2, 37)));
        assert_eq!(parse_cursor_position(&["2".into()]), None);
        assert_eq!(parse_cursor_position(&["2 37 extra".into()]), None);
        assert_eq!(parse_cursor_position(&["x 37".into()]), None);
    }

    #[test]
    fn tc_cb_tmux_title_sequence_never_becomes_visible_text() {
        // This is the exact burst zsh/tmux emitted after a physically typed Enter. xterm.js does
        // not implement ESC-k and displayed its payload (`ls`) on the row before command output.
        let input = b"\x1b[?1l\x1b>\x1b[?2004l\r\r\n\x1bkls\x1b\\";
        let expected = b"\x1b[?1l\x1b>\x1b[?2004l\r\r\n";
        for split in 0..=input.len() {
            let mut filter = TmuxTitleFilter::default();
            let mut output = filter.push(&input[..split]);
            output.extend(filter.push(&input[split..]));
            assert_eq!(output, expected, "split at byte {split}");
        }
    }

    #[test]
    fn tc_cb_tmux_title_filter_preserves_other_escapes_and_bel_terminator() {
        let mut filter = TmuxTitleFilter::default();
        let mut output = filter.push(b"a\x1b");
        output.extend(filter.push(b"[31mred\x1bkhidden\x07z"));
        assert_eq!(output, b"a\x1b[31mredz");
    }

    #[test]
    fn tc_cb_input_waits_for_capture_repaint() {
        let (inner, sent) = test_inner();
        assert_eq!(inner.lock().unwrap().reply.take(), Some(ReplyKind::Ignore));
        Inner::apply_topology(&inner, vec!["@0\t%0\t1\t1\tzsh\t/tmp\tzsh".into()]);
        Inner::mark_ready(&inner);
        sent.lock().unwrap().clear();

        {
            let mut g = inner.lock().unwrap();
            g.pending_captures.insert("@0".into());
            g.write_input("echo AFTER_REPAINT\n", Some("@0".into()));
            assert_eq!(g.pending_input.len(), 1, "input is held during capture");
        }
        assert!(!sent_str(&sent).contains("send-keys"), "nothing reaches tmux early");

        inner.lock().unwrap().finish_capture("@0");
        assert!(inner.lock().unwrap().pending_input.is_empty());
        let commands = sent_str(&sent);
        assert!(commands.contains("send-keys -t @0 -l \"echo AFTER_REPAINT\""));
        assert!(commands.contains("send-keys -t @0 Enter"));
    }

    #[test]
    fn parse_list_row_ok() {
        let r = parse_list_row("@1\t%9\t1\t1\tzsh\t/tmp/a b\tcodex").unwrap();
        assert_eq!(r.win, "@1");
        assert_eq!(r.pane, "%9");
        assert!(r.pane_active && r.win_active);
        assert_eq!(r.name, "zsh");
        assert_eq!(r.cwd, "/tmp/a b");
        assert_eq!(r.command, "codex");
    }

    #[test]
    fn parse_list_row_rejects_capture_text() {
        assert!(parse_list_row("$ ls -la").is_none());
        assert!(parse_list_row("total 40").is_none());
    }

    #[test]
    fn recovery_snapshot_uses_each_windows_active_pane() {
        let rows = vec![
            parse_list_row("@0\t%0\t0\t1\twork\t/tmp/wrong\ttail").unwrap(),
            parse_list_row("@0\t%1\t1\t1\twork\t/tmp/right\tcodex").unwrap(),
            parse_list_row("@1\t%2\t1\t0\tlogs\t/var/log\tzsh").unwrap(),
        ];
        let snapshot = recovery_snapshot(&rows, &["@0".into(), "@1".into()], Some("@0"));
        assert_eq!(snapshot.len(), 2);
        assert_eq!(snapshot[0].cwd, "/tmp/right");
        assert_eq!(snapshot[0].command, "codex");
        assert!(snapshot[0].active);
        assert_eq!(snapshot[1].cwd, "/var/log");
        assert!(!snapshot[1].active);
    }

    #[test]
    fn is_win_id_checks() {
        assert!(is_win_id("@12"));
        assert!(!is_win_id("@"));
        assert!(!is_win_id("%3"));
        assert!(!is_win_id("@1;rm"));
    }

    #[test]
    fn sanitize_window_name_cases() {
        // keeps spaces, '*', unicode; caps handled elsewhere
        assert_eq!(sanitize_window_name("* Building widget"), "* Building widget");
        // strips the quote/backslash that would break the "…" arg, and control chars/newlines
        assert_eq!(sanitize_window_name("a\"b\\c"), "abc");
        assert_eq!(sanitize_window_name("line1\nline2\t!"), "line1line2!");
        // collapses runs of spaces and trims
        assert_eq!(sanitize_window_name("  a   b  "), "a b");
        // empty stays empty (signals "re-enable auto-rename")
        assert_eq!(sanitize_window_name("   "), "");
        // length cap
        assert!(sanitize_window_name(&"x".repeat(500)).chars().count() <= 100);
        // '#' is doubled so tmux treats it literally instead of expanding a format. Verified against
        // tmux 3.6a: bare '#{pane_current_path}' renames the window to the resolved path; '##{...}'
        // renders the literal text.
        assert_eq!(sanitize_window_name("#{pane_current_path}"), "##{pane_current_path}");
        assert_eq!(sanitize_window_name("#(echo hi)"), "##(echo hi)");
        assert_eq!(sanitize_window_name("C# builds"), "C## builds");
        // the cap counts INPUT chars, so a title of all '#' can at most double in length
        assert!(sanitize_window_name(&"#".repeat(500)).chars().count() <= 200);
    }

    #[test]
    fn utf8_complete_input_has_no_carry() {
        let (s, carry) = decode_utf8_prefix("a─b".as_bytes());
        assert_eq!(s, "a─b");
        assert!(carry.is_empty());
    }

    #[test]
    fn utf8_split_multibyte_is_carried_not_corrupted() {
        // '─' (box drawing) = E2 94 80. Split it across two reads at every interior boundary and
        // verify we reassemble the exact char with NO U+FFFD.
        let full = "x─y".as_bytes().to_vec(); // 78 E2 94 80 79
        for cut in 1..full.len() {
            let (s1, carry) = decode_utf8_prefix(&full[..cut]);
            let mut combined = carry;
            combined.extend_from_slice(&full[cut..]);
            let (s2, tail) = decode_utf8_prefix(&combined);
            let joined = format!("{}{}", s1, s2);
            assert_eq!(joined, "x─y", "cut at {} must reassemble cleanly", cut);
            assert!(tail.is_empty(), "no leftover carry at cut {}", cut);
            assert!(!joined.contains('\u{FFFD}'), "no replacement char at cut {}", cut);
        }
    }

    #[test]
    fn utf8_truncated_tail_becomes_carry() {
        // Just the first 2 bytes of '─' (E2 94): incomplete -> empty decode + 2-byte carry.
        let (s, carry) = decode_utf8_prefix(&[0xE2, 0x94]);
        assert_eq!(s, "");
        assert_eq!(carry, vec![0xE2, 0x94]);
    }

    #[test]
    fn utf8_genuinely_invalid_byte_is_replaced_not_stalled() {
        // A lone 0xFF is invalid (not truncated) -> replacement char, decoding continues.
        let (s, carry) = decode_utf8_prefix(&[b'a', 0xFF, b'b']);
        assert_eq!(s, "a\u{FFFD}b");
        assert!(carry.is_empty());
    }

    // Output from an UNMAPPED pane must ask for ONE coalesced topology refresh, not one list-panes
    // per event. route_output used to call refresh_now() inline with a same-call-scope guard that
    // could never coalesce anything, so a redrawing unmapped pane (~750 %output/sec) sent ~750
    // list-panes commands. It now reports "needs refresh" and handle_event routes that through
    // queue_refresh's 10ms coalescing window.
    #[test]
    fn tc_cb_unmapped_pane_output_does_not_flood_list_panes() {
        let (inner, sent) = test_inner();
        sent.lock().unwrap().clear();

        // 50 events from a pane with no window mapping, through the real event path.
        for i in 0..50 {
            Inner::handle_event(&inner, ControlEvent::Output {
                pane: "%9".into(), data: format!("line{}\r\n", i),
            });
        }
        // Nothing is sent synchronously: queue_refresh arms a timer thread instead.
        let immediate = sent_str(&sent).matches("list-panes").count();
        assert_eq!(immediate, 0, "no synchronous list-panes per output event, got {}", immediate);

        // After the coalescing window, exactly ONE list-panes has gone out for the whole burst.
        std::thread::sleep(Duration::from_millis(80));
        let total = sent_str(&sent).matches("list-panes").count();
        assert_eq!(total, 1, "burst of 50 unmapped-pane events must coalesce to 1 refresh, got {}", total);

        // The output itself is still buffered for replay once the mapping is learned.
        assert_eq!(inner.lock().unwrap().pending_output.get("%9").map(|b| b.len()), Some(50));
    }

    // A MAPPED pane's output must not request a refresh at all — the mapping is already known.
    #[test]
    fn tc_cb_mapped_pane_output_requests_no_refresh() {
        let (inner, sent) = test_inner();
        Inner::apply_topology(&inner, vec!["@0\t%0\t1\t1\tzsh\t/tmp\tzsh".to_string()]);
        sent.lock().unwrap().clear();

        let needs = inner.lock().unwrap().route_output("%0".into(), "hello".into());
        assert!(!needs, "mapped pane must not ask for a topology refresh");
        std::thread::sleep(Duration::from_millis(40));
        assert_eq!(sent_str(&sent).matches("list-panes").count(), 0, "no refresh for a mapped pane");
    }
}
