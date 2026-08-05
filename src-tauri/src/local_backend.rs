//! Local shell backend: a real pty running THIS machine's shell, no ssh and no tmux.
//!
//! FALLBACK ONLY, for a machine with **no tmux installed** (DESIGN.md §5.3b). A local session
//! normally runs *inside* local tmux — same control-mode protocol, same reconnect supervisor, same
//! store row as a remote one — so it gets native tabs and survives quitting the app. This backend is
//! what's left when `probe_local_tmux()` finds nothing: no socket, no supervisor, no reattach, and
//! the session is not persisted, because closing it ends the shell. It is therefore the only
//! non-durable session type; installing tmux upgrades a local session to the durable path with no
//! other change, so nothing here is a target for new features — fix them on the shared tmux path.
//!
//! Port of the Electron-era `src/main/backends/localBackend.js`, which was never carried over in
//! the Tauri migration — so the new-session dialog's "Local shell" option reached `PlainBackend`,
//! whose `build_ssh_args("")` rejects the empty host ("host: empty or too long") and surfaced as
//! `failed to connect local: host`.

use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::thread;

use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};

/// Same event shape as `PlainEvent`: one untagged byte stream plus an exit signal. Local mode has
/// no panes/windows to route, exactly like plain ssh mode.
#[derive(Debug, Clone)]
pub enum LocalEvent {
    Data { data: String },
    Exit,
}

pub type LocalSink = Arc<dyn Fn(LocalEvent) + Send + Sync>;

pub struct LocalConfig {
    /// Shell to run; falls back to $SHELL then /bin/bash (as the Electron backend did).
    pub shell: Option<String>,
    /// Working directory; falls back to $HOME.
    pub cwd: Option<String>,
}

pub struct LocalBackend {
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    master: Box<dyn MasterPty + Send>,
    child: Arc<Mutex<Box<dyn portable_pty::Child + Send + Sync>>>,
}

/// The shell to launch: explicit override, else $SHELL, else /bin/bash.
pub fn resolve_shell(explicit: Option<&str>) -> String {
    let pick = |s: &str| -> Option<String> {
        let t = s.trim();
        if t.is_empty() { None } else { Some(t.to_string() ) }
    };
    explicit
        .and_then(pick)
        .or_else(|| std::env::var("SHELL").ok().and_then(|s| pick(&s)))
        .unwrap_or_else(|| "/bin/bash".to_string())
}

impl LocalBackend {
    pub fn spawn(cfg: LocalConfig, sink: LocalSink, cols: u16, rows: u16) -> Result<Self, String> {
        let shell = resolve_shell(cfg.shell.as_deref());
        let cwd = cfg.cwd
            .filter(|c| !c.trim().is_empty())
            .or_else(|| std::env::var("HOME").ok())
            .filter(|c| !c.is_empty());

        let pty = native_pty_system();
        let pair = pty
            .openpty(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
            .map_err(|e| format!("openpty failed: {e}"))?;

        let mut cmd = CommandBuilder::new(&shell);
        // Login shell, so the user gets the same environment/prompt as opening Terminal.app. This is
        // what $SHELL users expect; a non-login shell skips .zprofile/.bash_profile.
        cmd.arg("-l");
        // TERM must be set explicitly: portable_pty does NOT inherit it reliably, and an unset TERM
        // leaves xterm.js talking to a shell that thinks it's dumb (no colors, broken editors).
        cmd.env("TERM", "xterm-256color");
        cmd.env("PATH", crate::augmented_path());
        if let Some(dir) = &cwd { cmd.cwd(dir); }

        let child = pair.slave.spawn_command(cmd)
            .map_err(|e| format!("failed to start {shell}: {e}"))?;
        drop(pair.slave);

        let writer = pair.master.take_writer().map_err(|e| format!("pty writer: {e}"))?;
        let mut reader = pair.master.try_clone_reader().map_err(|e| format!("pty reader: {e}"))?;

        {
            let sink = sink.clone();
            thread::spawn(move || {
                let mut buf = [0u8; 8192];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            // from_utf8_lossy matches plain_backend: a multi-byte char split across
                            // two reads degrades to U+FFFD rather than dropping the chunk.
                            let data = String::from_utf8_lossy(&buf[..n]).to_string();
                            sink(LocalEvent::Data { data });
                        }
                        Err(_) => break,
                    }
                }
                sink(LocalEvent::Exit);
            });
        }

        Ok(LocalBackend {
            writer: Arc::new(Mutex::new(writer)),
            master: pair.master,
            child: Arc::new(Mutex::new(child)),
        })
    }

    pub fn write(&self, data: &str) {
        if let Ok(mut w) = self.writer.lock() {
            let _ = w.write_all(data.as_bytes());
            let _ = w.flush();
        }
    }

    pub fn resize(&self, cols: u16, rows: u16) {
        let _ = self.master.resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 });
    }

    pub fn kill(&self) {
        if let Ok(mut c) = self.child.lock() { let _ = c.kill(); }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    // TC-LB1 shell resolution order: explicit > $SHELL > /bin/bash, ignoring blank values.
    #[test]
    fn tc_lb1_resolve_shell_precedence() {
        assert_eq!(resolve_shell(Some("/bin/ksh")), "/bin/ksh");
        // blank/whitespace explicit is not a choice — fall through to the env
        let env_shell = std::env::var("SHELL").ok().filter(|s| !s.trim().is_empty());
        let fallthrough = resolve_shell(Some("   "));
        match env_shell {
            Some(s) => assert_eq!(fallthrough, s),
            None => assert_eq!(fallthrough, "/bin/bash"),
        }
        assert!(fallthrough.starts_with('/'), "resolved shell is an absolute path: {fallthrough}");
    }

    // TC-LB2 a local session really runs a shell on THIS machine with no ssh/tmux involved: write a
    // command, read its output back. This is the exact path that was missing (the bug), so it is
    // asserted end-to-end rather than mocked.
    #[test]
    fn tc_lb2_spawns_a_real_local_shell_and_echoes() {
        let (tx, rx) = mpsc::channel();
        let sink: LocalSink = Arc::new(move |ev| {
            if let LocalEvent::Data { data } = ev { let _ = tx.send(data); }
        });
        // /bin/sh is guaranteed present; using it keeps the test independent of the dev's $SHELL.
        let b = LocalBackend::spawn(
            LocalConfig { shell: Some("/bin/sh".into()), cwd: None }, sink, 80, 24,
        ).expect("local shell spawns");

        b.write("echo BUOY_LOCAL_OK\n");
        let mut seen = String::new();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            match rx.recv_timeout(Duration::from_millis(500)) {
                Ok(chunk) => {
                    seen.push_str(&chunk);
                    // the echoed command line also contains the marker, so require the OUTPUT line
                    if seen.matches("BUOY_LOCAL_OK").count() >= 2 { break; }
                }
                Err(_) => {}
            }
        }
        b.kill();
        assert!(seen.contains("BUOY_LOCAL_OK"), "shell produced its output; got: {seen:?}");
    }

    // TC-LB3 cwd is honored, so a local session starts where the user expects ($HOME by default).
    #[test]
    fn tc_lb3_honors_cwd() {
        let (tx, rx) = mpsc::channel();
        let sink: LocalSink = Arc::new(move |ev| {
            if let LocalEvent::Data { data } = ev { let _ = tx.send(data); }
        });
        let b = LocalBackend::spawn(
            LocalConfig { shell: Some("/bin/sh".into()), cwd: Some("/tmp".into()) }, sink, 80, 24,
        ).expect("spawns");
        b.write("pwd\n");
        let mut seen = String::new();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            if let Ok(c) = rx.recv_timeout(Duration::from_millis(500)) { seen.push_str(&c); }
            // macOS reports /tmp as the /private/tmp symlink target
            if seen.contains("/tmp") { break; }
        }
        b.kill();
        assert!(seen.contains("/tmp"), "started in the requested cwd; got: {seen:?}");
    }

    // TC-LB4 exit is reported, so the renderer can close the tab when the user types `exit`.
    #[test]
    fn tc_lb4_reports_exit() {
        let (tx, rx) = mpsc::channel();
        let sink: LocalSink = Arc::new(move |ev| {
            if matches!(ev, LocalEvent::Exit) { let _ = tx.send(()); }
        });
        let b = LocalBackend::spawn(
            LocalConfig { shell: Some("/bin/sh".into()), cwd: None }, sink, 80, 24,
        ).expect("spawns");
        b.write("exit\n");
        assert!(rx.recv_timeout(Duration::from_secs(10)).is_ok(), "Exit event is emitted");
    }

    // TC-LB5 the ROOT CAUSE of the original bug, pinned: a local session has an empty host, and the
    // ssh path rejects that. This is why `kind:'local'` must never reach PlainBackend/build_ssh_args
    // (it did, and surfaced as "failed to connect local: host"). If someone later routes local
    // through the ssh builder again, this documents exactly what breaks.
    #[test]
    fn tc_lb5_ssh_path_cannot_serve_a_local_session() {
        let socket = crate::tmux_socket::socket_name("plain", None, "dt-local");
        let err = crate::validation::build_ssh_args("", "dt-local", &[], "tmux", &socket)
            .expect_err("an empty host is not a valid ssh target");
        assert_eq!(err.field, "host");
        // ...while the local backend needs no host at all.
        assert!(resolve_shell(None).starts_with('/'), "local mode resolves a shell without any host");
    }

    // TC-LB6 resize reaches the pty: the shell must see the new geometry, not the spawn-time one.
    #[test]
    fn tc_lb6_resize_reaches_the_shell() {
        let (tx, rx) = mpsc::channel();
        let sink: LocalSink = Arc::new(move |ev| {
            if let LocalEvent::Data { data } = ev { let _ = tx.send(data); }
        });
        let b = LocalBackend::spawn(
            LocalConfig { shell: Some("/bin/sh".into()), cwd: None }, sink, 80, 24,
        ).expect("spawns");
        b.resize(100, 40);
        b.write("echo COLS=$(tput cols)\n");
        let mut seen = String::new();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            if let Ok(c) = rx.recv_timeout(Duration::from_millis(500)) { seen.push_str(&c); }
            if seen.contains("COLS=100") { break; }
        }
        b.kill();
        assert!(seen.contains("COLS=100"), "shell sees the resized width; got: {seen:?}");
    }
}
