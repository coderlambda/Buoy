//! Plain ssh+tmux backend: `ssh -tt ... tmux -L <sock> new-session -A -s <name>` with a raw
//! byte stream (no control mode). Port of src/main/backends/sshTmuxBackend.js. Durability comes
//! from tmux server-side + the supervisor respawning ssh; there is no per-pane routing.

use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::thread;

use portable_pty::{CommandBuilder, PtySize, native_pty_system, MasterPty};

use crate::tmux_socket::socket_name;
use crate::transport::{self, Transport};
use crate::validation;
use crate::session_store::RecoveryWindow;

/// Events: plain mode has no window tagging — data is a single stream, window is empty.
#[derive(Debug, Clone)]
pub enum PlainEvent {
    Data { data: String },
    Exit,
}

pub type PlainSink = Arc<dyn Fn(PlainEvent) + Send + Sync>;

pub struct PlainConfig {
    pub host: String,
    pub session: String,
    pub tmux_path: String,
    pub tmux_version: Option<(u32, u32)>,
    pub socket: String,
    pub recovery_windows: Vec<RecoveryWindow>,
    pub base_args: Vec<String>,
    /// ssh to `host`, or tmux on THIS machine (kind:'local'). See transport.rs.
    pub transport: Transport,
}

impl Default for PlainConfig {
    fn default() -> Self {
        PlainConfig {
            host: String::new(), session: String::new(), tmux_path: "tmux".into(),
            tmux_version: None, socket: String::new(), recovery_windows: vec![],
            base_args: vec![], transport: Transport::Ssh,
        }
    }
}

pub struct PlainBackend {
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    master: Box<dyn MasterPty + Send>,
    child: Arc<Mutex<Box<dyn portable_pty::Child + Send + Sync>>>,
}

impl PlainBackend {
    pub fn spawn(cfg: PlainConfig, sink: PlainSink, cols: u16, rows: u16)
        -> Result<Self, validation::ValidationError>
    {
        let socket = if cfg.socket.is_empty() {
            socket_name("plain", cfg.tmux_version, &cfg.session)
        } else {
            cfg.socket.clone()
        };
        let (lc_all, lang) = transport::current_locale();
        let spec = transport::spawn_spec_with_recovery(
            cfg.transport, false, &cfg.host, &cfg.session, &cfg.tmux_path, &socket,
            &cfg.base_args, lc_all.as_deref(), lang.as_deref(), &cfg.recovery_windows,
        )?;

        let pty = native_pty_system();
        let pair = pty.openpty(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
            .expect("openpty failed");
        let mut cmd = CommandBuilder::new(&spec.program);
        cmd.args(&spec.args);
        for (k, v) in &spec.env { cmd.env(k, v); }
        let child = pair.slave.spawn_command(cmd).expect("tmux client spawn failed");
        drop(pair.slave);

        let writer = pair.master.take_writer().expect("pty writer");
        let mut reader = pair.master.try_clone_reader().expect("pty reader");

        {
            let sink = sink.clone();
            thread::spawn(move || {
                let mut buf = [0u8; 8192];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            let data = String::from_utf8_lossy(&buf[..n]).to_string();
                            sink(PlainEvent::Data { data });
                        }
                        Err(_) => break,
                    }
                }
                sink(PlainEvent::Exit);
            });
        }

        Ok(PlainBackend {
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
        if let Ok(mut c) = self.child.lock() {
            let _ = c.kill();
        }
    }
}
