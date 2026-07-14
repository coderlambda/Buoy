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
    refresh_queued: bool,
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
        let socket = socket_name("control", cfg.tmux_version);
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
            refresh_queued: false,
        }));

        // Reader thread: pump pty bytes -> parser -> event handling.
        {
            let inner = inner.clone();
            let sink = sink.clone();
            thread::spawn(move || {
                crate::dlog!("reader: thread started");
                let mut buf = [0u8; 8192];
                let mut total = 0usize;
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) => { crate::dlog!("reader: EOF after {} bytes", total); break; }
                        Ok(n) => {
                            total += n;
                            let chunk = String::from_utf8_lossy(&buf[..n]).to_string();
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
                sink(BackendEvent::Exit);
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

    pub fn capture_window(&self, win: &str) {
        if !is_win_id(win) { return; }
        let mut g = self.inner.lock().unwrap();
        g.send(
            format!("capture-pane -p -e -q -J -N -S -{} -t {}", MAX_HISTORY, win),
            ReplyKind::Capture { window: Some(win.to_string()) },
        );
    }

    pub fn kill(&self) {
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

        for win in &diff.added {
            g.emit(BackendEvent::WindowAdd { window: win.clone(), order: order.clone() });
        }
        for (win, name) in &diff.renamed {
            g.emit(BackendEvent::WindowRename { window: win.clone(), name: name.clone() });
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
        match self.reg.win_for_pane(&pane) {
            Some(win) => self.emit(BackendEvent::Data { window: win, data }),
            None => {
                let buf = self.pending_output.entry(pane).or_default();
                if buf.len() >= MAX_BUFFER { buf.remove(0); }
                buf.push(data);
                // trigger a refresh so the mapping is learned (inline, avoids nested lock)
                if !self.refresh_queued {
                    self.refresh_queued = true;
                    self.refresh_now();
                    self.refresh_queued = false;
                }
            }
        }
    }

    fn flush_pane_buffer(&mut self, pane: &str) {
        if let Some(buf) = self.pending_output.remove(pane) {
            if let Some(win) = self.reg.win_for_pane(pane) {
                for data in buf {
                    self.emit(BackendEvent::Data { window: win.clone(), data });
                }
            }
        }
    }

    fn on_attach(inner: &Arc<Mutex<Inner>>) {
        {
            let mut g = inner.lock().unwrap();
            if g.attached { crate::dlog!("on_attach: already attached, skip"); return; }
            g.attached = true;
            crate::dlog!("on_attach: refreshing topology + capturing scrollback");
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
        let g = inner.lock().unwrap();
        let target = window.or_else(|| g.reg.active_window.clone());
        if let Some(t) = target {
            if !b.is_empty() {
                let data = format!("\x1b[H\x1b[2J{}\r\n", b.join("\r\n"));
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
}
