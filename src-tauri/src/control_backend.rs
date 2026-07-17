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
use crate::tmux_keys::encode_send_keys;
use crate::tmux_socket::socket_name;
use crate::validation::{self, build_control_mode_ssh_args};
use crate::window_registry::{PaneRow, WindowRegistry};

const LIST_FMT: &str = "#{window_id} #{pane_id} #{pane_active} #{window_active} #{window_name}";
const MAX_HISTORY: u32 = 2000;
const MAX_BUFFER: usize = 4096;

/// App-level events the backend emits (consumed by the session layer -> Tauri events).
#[derive(Debug, Clone)]
pub enum BackendEvent {
    /// Terminal output tagged with the window it belongs to.
    Data { window: String, data: String },
    WindowAdd { window: String, order: Vec<String> },
    WindowClose { window: String, order: Vec<String> },
    WindowRename { window: String, name: String },
    WindowActive { window: String, order: Vec<String> },
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
    pub base_args: Vec<String>,
}

struct Inner {
    parser: ControlModeParser,
    reg: WindowRegistry,
    reply: ReplyChannel,
    writer: Box<dyn Write + Send>,
    sink: BackendSink,
    session: String,
    ready: bool,
    attached: bool,
    pending_input: Vec<String>,
    pending_output: std::collections::BTreeMap<String, Vec<String>>,
    // Per-pane carry of trailing incomplete UTF-8 bytes. tmux can split a multi-byte char across
    // two %output events for the SAME pane; carrying the partial tail here (keyed by pane, since
    // interleaved panes must not corrupt each other) reassembles it on the next event.
    utf8_carry: std::collections::BTreeMap<String, Vec<u8>>,
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
        let socket = socket_name("control", cfg.tmux_version, &cfg.session);
        let mut default_opts: Vec<String> = vec![
            "-o".into(), "ConnectTimeout=8".into(),
            "-o".into(), "ServerAliveInterval=15".into(),
            "-o".into(), "ServerAliveCountMax=3".into(),
        ];
        default_opts.extend(cfg.base_args.iter().cloned());
        let ssh_args = build_control_mode_ssh_args(
            &cfg.host, &cfg.session, &default_opts, &cfg.tmux_path, &socket,
        )?;
        crate::dlog!("ControlBackend.spawn: socket={} ssh {}", socket, ssh_args.join(" "));

        let pty = native_pty_system();
        let pair = pty.openpty(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
            .expect("openpty failed");

        let mut cmd = CommandBuilder::new("ssh");
        cmd.args(&ssh_args);
        // Augment PATH so a Finder-launched app still finds ssh/tmux (mirrors env.js).
        cmd.env("PATH", crate::augmented_path());

        let child = pair.slave.spawn_command(cmd).expect("ssh spawn failed");
        drop(pair.slave);

        let writer = pair.master.take_writer().expect("pty writer");
        let mut reader = pair.master.try_clone_reader().expect("pty reader");

        let mut reply = ReplyChannel::new();
        reply.start(); // seed handshake handler

        let inner = Arc::new(Mutex::new(Inner {
            parser: ControlModeParser::new(),
            reg: WindowRegistry::new(),
            reply,
            writer,
            sink: sink.clone(),
            session: cfg.session.clone(),
            ready: false,
            attached: false,
            pending_input: Vec::new(),
            pending_output: std::collections::BTreeMap::new(),
            utf8_carry: std::collections::BTreeMap::new(),
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
        let mut g = self.inner.lock().unwrap();
        let target = g.reg.active_window.clone();
        match target {
            Some(t) if g.ready => {
                for line in encode_send_keys(data, &t) {
                    g.send(line, ReplyKind::Ignore);
                }
            }
            _ => {
                if g.pending_input.len() >= MAX_BUFFER {
                    g.pending_input.remove(0);
                }
                g.pending_input.push(data.to_string());
            }
        }
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
        g.send(
            format!("capture-pane -p -e -q -J -N -S -{} -t {}", MAX_HISTORY, win),
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
                let mut g = inner.lock().unwrap();
                g.route_output(pane, data);
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
                Inner::on_reply(inner, body);
            }
            ControlEvent::Begin { .. } => {}
            other => { crate::dlog!("event: unhandled {:?}", other); }
        }
    }

    fn on_reply(inner: &Arc<Mutex<Inner>>, body: Vec<String>) {
        let kind = { inner.lock().unwrap().reply.take() };
        crate::dlog!("on_reply: kind={:?} bodyLines={}", kind, body.len());
        match kind {
            Some(ReplyKind::Topology) => {
                let lines: Vec<String> = body.into_iter().filter(|l| !l.trim().is_empty()).collect();
                Inner::apply_topology(inner, lines);
            }
            Some(ReplyKind::Capture { window }) => {
                Inner::paint_capture(inner, window, body);
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
        if rows.is_empty() { return; }
        let diff = g.reg.reconcile(&rows);
        let order = g.reg.order();
        crate::dlog!("apply_topology: added={:?} removed={:?} active={:?}", diff.added, diff.removed, diff.active);

        // Emit any buffered output first so window add/active can't be reordered ahead of it.
        g.flush_output();
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
    }

    fn route_output(&mut self, pane: String, data: String) {
        // `data` is LATIN1 (one char per raw byte, from the reader + octal unescape). Convert back
        // to bytes, prepend this pane's carried incomplete UTF-8 tail, decode the valid prefix, and
        // stash any new incomplete tail. This is where a char split across two %output events for
        // the same pane is reassembled.
        let mut bytes: Vec<u8> = Vec::new();
        if let Some(carry) = self.utf8_carry.remove(&pane) { bytes.extend_from_slice(&carry); }
        bytes.extend(data.chars().map(|c| c as u8));
        let (text, tail) = decode_utf8_prefix(&bytes);
        if !tail.is_empty() { self.utf8_carry.insert(pane.clone(), tail); }
        if text.is_empty() { return; }

        match self.reg.win_for_pane(&pane) {
            // Buffer into out_buf; the flush thread emits one coalesced message per window ~60fps.
            Some(win) => { self.out_buf.entry(win).or_default().push_str(&text); }
            None => {
                let buf = self.pending_output.entry(pane).or_default();
                if buf.len() >= MAX_BUFFER { buf.remove(0); }
                buf.push(text);
                // trigger a refresh so the mapping is learned (inline, avoids nested lock)
                if !self.refresh_queued {
                    self.refresh_queued = true;
                    self.refresh_now();
                    self.refresh_queued = false;
                }
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
                (self.sink)(BackendEvent::Data { window, data });
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
            crate::dlog!("on_attach: refreshing topology + capturing scrollback");
            // Tab titles follow the pane title (OSC 2 / `\e]2;…`), the standard, program-agnostic
            // way any tool (Claude Code, vim, a shell PROMPT_COMMAND, …) names its tab. Fall back to
            // the running command when the title is just tmux's default (== the host name), so an
            // untitled shell shows "zsh" rather than the hostname. This drives %window-renamed,
            // which the renderer already maps to the tab label. A manual rename turns automatic-
            // rename off for that window (tmux does this itself when you set-option the name).
            g.send("set-option -g automatic-rename on".into(), ReplyKind::Ignore);
            g.send(
                "set-option -g automatic-rename-format \
                 '#{?#{==:#{pane_title},#{host}},#{pane_current_command},#{pane_title}}'".into(),
                ReplyKind::Ignore,
            );
            g.refresh_now();
            let session = g.session.clone();
            g.send(
                format!("capture-pane -p -e -q -J -N -S -{} -t {}", MAX_HISTORY, session),
                ReplyKind::Capture { window: None },
            );
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
        if g.ready { return; }
        g.ready = true;
        crate::dlog!("mark_ready: active={:?} pending_input={}", g.reg.active_window, g.pending_input.len());
        let queued: Vec<String> = std::mem::take(&mut g.pending_input);
        if let Some(target) = g.reg.active_window.clone() {
            for d in queued {
                for line in encode_send_keys(&d, &target) {
                    g.send(line, ReplyKind::Ignore);
                }
            }
        }
        g.emit(BackendEvent::Ready);
    }

    fn paint_capture(inner: &Arc<Mutex<Inner>>, window: Option<String>, body: Vec<String>) {
        let mut b = body;
        while b.last().map(|l| l.is_empty()).unwrap_or(false) {
            b.pop();
        }
        let mut g = inner.lock().unwrap();
        g.flush_output();   // paint scrollback AFTER any buffered live output, never before
        let target = window.or_else(|| g.reg.active_window.clone());
        if let Some(t) = target {
            if !b.is_empty() {
                // Capture body lines are LATIN1 (like %output); a whole capture line's bytes are
                // contiguous, so decode each line latin1 -> UTF-8 (no cross-line carry needed).
                let joined = b.join("\r\n");
                let bytes: Vec<u8> = joined.chars().map(|c| c as u8).collect();
                let (text, _tail) = decode_utf8_prefix(&bytes);
                let data = format!("\x1b[H\x1b[2J{}\r\n", text);
                g.emit(BackendEvent::Data { window: t, data });
            }
        }
    }
}

// "@win %pane paneActive winActive name"
fn parse_list_row(line: &str) -> Option<PaneRow> {
    let mut parts = line.splitn(5, ' ');
    let win = parts.next()?;
    let pane = parts.next()?;
    let pane_active = parts.next()?;
    let win_active = parts.next()?;
    let name = parts.next().unwrap_or("");
    if !win.starts_with('@') || !pane.starts_with('%') { return None; }
    if pane_active != "0" && pane_active != "1" { return None; }
    if win_active != "0" && win_active != "1" { return None; }
    Some(PaneRow {
        win: win.to_string(),
        pane: pane.to_string(),
        pane_active: pane_active == "1",
        win_active: win_active == "1",
        name: name.to_string(),
    })
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
fn sanitize_window_name(s: &str) -> String {
    let mut out = String::new();
    let mut prev_space = false;
    for c in s.chars() {
        if c.is_control() || c == '"' || c == '\\' { continue; }
        if c == ' ' {
            if prev_space { continue; }
            prev_space = true;
        } else {
            prev_space = false;
        }
        out.push(c);
        if out.chars().count() >= 100 { break; }
    }
    out.trim().to_string()
}

/// Decode the longest valid UTF-8 prefix of `bytes`. Returns the decoded string and any trailing
/// bytes that form an INCOMPLETE (but so-far-valid) multi-byte sequence, to be carried into the
/// next read. Genuinely invalid bytes (not just truncated) are passed through lossily so we never
/// stall. This is what stops box-drawing/other multi-byte chars from being corrupted to U+FFFD
/// when they straddle a read boundary.
fn decode_utf8_prefix(bytes: &[u8]) -> (String, Vec<u8>) {
    match std::str::from_utf8(bytes) {
        Ok(s) => (s.to_string(), Vec::new()),
        Err(e) => {
            let valid = e.valid_up_to();
            // SAFETY: bytes[..valid] is valid UTF-8 by definition of valid_up_to().
            let good = unsafe { std::str::from_utf8_unchecked(&bytes[..valid]) }.to_string();
            match e.error_len() {
                // None => the tail is a truncated-but-valid sequence: carry it for the next read.
                None => (good, bytes[valid..].to_vec()),
                // Some(len) => genuinely invalid bytes: emit the replacement char and skip them,
                // then keep decoding the remainder (so one bad byte can't wedge the stream).
                Some(len) => {
                    let mut out = good;
                    out.push('\u{FFFD}');
                    let (rest_str, rest_carry) = decode_utf8_prefix(&bytes[valid + len..]);
                    out.push_str(&rest_str);
                    (out, rest_carry)
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_list_row_ok() {
        let r = parse_list_row("@1 %9 1 1 zsh").unwrap();
        assert_eq!(r.win, "@1");
        assert_eq!(r.pane, "%9");
        assert!(r.pane_active && r.win_active);
        assert_eq!(r.name, "zsh");
    }

    #[test]
    fn parse_list_row_rejects_capture_text() {
        assert!(parse_list_row("$ ls -la").is_none());
        assert!(parse_list_row("total 40").is_none());
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
}
