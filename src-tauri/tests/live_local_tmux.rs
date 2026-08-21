//! Live integration test for LOCAL tmux sessions (kind:'local', DESIGN.md §5.3b): the same
//! ControlBackend + Supervisor a remote session uses, but with Transport::Local so tmux is exec'd on
//! THIS machine with no ssh and no network.
//!
//! Not #[ignore]d, unlike the other live_* tests: it needs no remote host and no credentials, only a
//! local tmux (skips cleanly when absent). That makes the local path's durability — reattach to the
//! same server, state reported as connected, output survives a client death — checkable in CI.
//!
//!   cargo test --test live_local_tmux -- --nocapture

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use buoy_lib::control_backend::{BackendConfig, BackendEvent, ControlBackend};
use buoy_lib::plain_backend::{PlainBackend, PlainConfig, PlainEvent};
use buoy_lib::supervisor::{self, State, Supervisor, SupervisorOpts, real_backend_factory};
use buoy_lib::transport::Transport;

fn sleep_ms(ms: u64) { thread::sleep(Duration::from_millis(ms)); }

/// The local tmux this machine offers, or None (test then skips).
fn local_tmux() -> Option<(String, Option<(u32, u32)>)> {
    let r = buoy_lib::probe::probe_local_tmux();
    if r.probed { Some((r.tmux_path, r.version)) } else { None }
}

/// Remove a leftover local tmux server for `session` so each run starts clean.
fn kill_server(tmux: &str, mode: &str, ver: Option<(u32, u32)>, session: &str) {
    let socket = buoy_lib::tmux_socket::socket_name(mode, ver, session);
    let _ = std::process::Command::new(tmux)
        .args(["-L", &socket, "kill-server"])
        .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null())
        .status();
}

#[derive(Default)]
struct Rec {
    by_window: HashMap<String, String>,
    windows: Vec<String>,
    ready: bool,
    exits: u32,
}

fn sink(rec: Arc<Mutex<Rec>>) -> buoy_lib::control_backend::BackendSink {
    Arc::new(move |ev: BackendEvent| {
        let mut r = rec.lock().unwrap();
        match ev {
            BackendEvent::Data { window, data, .. } => { r.by_window.entry(window).or_default().push_str(&data); }
            BackendEvent::WindowAdd { order, .. } => { r.windows = order; }
            BackendEvent::WindowClose { order, .. } => { r.windows = order; }
            BackendEvent::WindowActive { order, .. } => { r.windows = order; }
            BackendEvent::WindowRename { .. } => {}
            BackendEvent::RecoverySnapshot { .. } => {}
            BackendEvent::Ready => { r.ready = true; }
            BackendEvent::Exit => { r.exits += 1; }
        }
    })
}

fn all_text(rec: &Arc<Mutex<Rec>>) -> String {
    rec.lock().unwrap().by_window.values().cloned().collect::<Vec<_>>().join("\n")
}

/// TC-LT1 a LOCAL session speaks full control mode: %window-add arrives (so it gets native tabs),
/// Ready fires, and a command's output comes back — all with no ssh process anywhere.
#[test]
fn tc_lt1_local_control_mode_attaches_and_runs() {
    let Some((tmux, ver)) = local_tmux() else { eprintln!("SKIP: no local tmux"); return };
    let session = "buoylt1";
    kill_server(&tmux, "control", ver, session);

    let rec = Arc::new(Mutex::new(Rec::default()));
    let backend = ControlBackend::spawn(
        BackendConfig {
            host: String::new(),               // a local session has NO host
            session: session.into(),
            tmux_path: tmux.clone(), tmux_version: ver, socket: String::new(),
            recovery_windows: vec![], base_args: vec![],
            transport: Transport::Local,
        },
        sink(rec.clone()), 90, 30,
    ).expect("local control backend spawns");

    for _ in 0..40 { if rec.lock().unwrap().ready { break; } sleep_ms(250); }
    assert!(rec.lock().unwrap().ready, "local control session became ready");
    assert!(!rec.lock().unwrap().windows.is_empty(),
        "tmux reported its window topology (this is what native tabs are built from)");

    backend.write("echo LOCAL_CC_MARK\n");
    for _ in 0..40 { if all_text(&rec).contains("LOCAL_CC_MARK") { break; } sleep_ms(250); }
    assert!(all_text(&rec).contains("LOCAL_CC_MARK"), "command output came back: {}", all_text(&rec));

    // The tmux server really exists on this machine, on the version-tagged socket.
    let socket = buoy_lib::tmux_socket::socket_name("control", ver, session);
    let out = std::process::Command::new(&tmux)
        .args(["-L", &socket, "list-sessions"]).output().expect("list-sessions");
    let listed = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(listed.contains(session), "local tmux server is live: {listed:?}");

    backend.kill();
    kill_server(&tmux, "control", ver, session);
}

/// TC-LT2 THE DURABILITY PROPERTY, and the whole reason local sessions use tmux: work survives the
/// death of the client. Write a marker, kill the backend (as an app crash/quit would), reattach a
/// NEW backend to the same session, and the marker is still on screen.
#[test]
fn tc_lt2_local_session_survives_client_death() {
    let Some((tmux, ver)) = local_tmux() else { eprintln!("SKIP: no local tmux"); return };
    let session = "buoylt2";
    kill_server(&tmux, "control", ver, session);

    let cfg = || BackendConfig {
        host: String::new(), session: session.into(),
        tmux_path: tmux.clone(), tmux_version: ver, socket: String::new(),
        recovery_windows: vec![], base_args: vec![],
        transport: Transport::Local,
    };

    // First client: leave a durable marker in the shell's scrollback.
    let rec1 = Arc::new(Mutex::new(Rec::default()));
    let b1 = ControlBackend::spawn(cfg(), sink(rec1.clone()), 90, 30).expect("spawn 1");
    for _ in 0..40 { if rec1.lock().unwrap().ready { break; } sleep_ms(250); }
    assert!(rec1.lock().unwrap().ready, "first client ready");
    b1.write("echo SURVIVOR_MARK\n");
    for _ in 0..40 { if all_text(&rec1).contains("SURVIVOR_MARK") { break; } sleep_ms(250); }
    assert!(all_text(&rec1).contains("SURVIVOR_MARK"), "marker ran");

    // Client dies (app quit / crash). The tmux SERVER keeps running.
    b1.kill();
    sleep_ms(1000);

    // Second client attaches to the SAME session and sees the earlier output replayed.
    let rec2 = Arc::new(Mutex::new(Rec::default()));
    let b2 = ControlBackend::spawn(cfg(), sink(rec2.clone()), 90, 30).expect("spawn 2");
    for _ in 0..40 { if rec2.lock().unwrap().ready { break; } sleep_ms(250); }
    assert!(rec2.lock().unwrap().ready, "reattached client ready");
    let win = rec2.lock().unwrap().windows.first().cloned().expect("reattached window");
    b2.capture_window(&win); // the renderer does this after fitting/resizing the real xterm
    for _ in 0..40 { if all_text(&rec2).contains("SURVIVOR_MARK") { break; } sleep_ms(250); }
    assert!(all_text(&rec2).contains("SURVIVOR_MARK"),
        "a local session's work SURVIVED its client dying (this is why local uses tmux); got: {}",
        all_text(&rec2));

    b2.kill();
    kill_server(&tmux, "control", ver, session);
}

/// TC-LT3 the fix for "local session is keep 'connecting'": under the supervisor a local session
/// must report Connected. The stuck-connecting bug was a local session that had no supervisor at
/// all, so no session:state event was ever emitted and the UI kept its pre-connect status forever.
#[test]
fn tc_lt3_local_session_reports_connected_state() {
    let Some((tmux, ver)) = local_tmux() else { eprintln!("SKIP: no local tmux"); return };
    // Control mode needs >= 3.2; on an older local tmux this path isn't used (plain is), so skip.
    match ver { Some((maj, min)) if maj > 3 || (maj == 3 && min >= 2) => {},
                _ => { eprintln!("SKIP: local tmux < 3.2"); return } }
    let session = "buoylt3";
    kill_server(&tmux, "control", ver, session);

    let states = Arc::new(Mutex::new(Vec::<State>::new()));
    let st = states.clone();
    let state_sink: supervisor::StateSink = Arc::new(move |s: State| st.lock().unwrap().push(s));
    let rec = Arc::new(Mutex::new(Rec::default()));

    let sup = Supervisor::new(
        BackendConfig {
            host: String::new(), session: session.into(),
            tmux_path: tmux.clone(), tmux_version: ver, socket: String::new(),
            recovery_windows: vec![], base_args: vec![],
            transport: Transport::Local,
        },
        SupervisorOpts::default(),
        real_backend_factory(),
        sink(rec.clone()), state_sink,
        Arc::new(|d| thread::sleep(d)),
        { let base = std::time::Instant::now(); Arc::new(move || base.elapsed().as_millis() as u64) },
    );
    sup.start(90, 30);

    for _ in 0..60 { if sup.state() == State::Connected { break; } sleep_ms(250); }
    let seen = states.lock().unwrap().clone();
    assert_eq!(sup.state(), State::Connected,
        "a local session reaches Connected (not stuck at Connecting); states={seen:?}");
    assert!(seen.contains(&State::Connected),
        "a session:state event carrying 'connected' was emitted; states={seen:?}");

    sup.close();
    kill_server(&tmux, "control", ver, session);
}

/// TC-LT4 local PLAIN mode (the path taken when the user turns Native tabs off, or on tmux < 3.2):
/// still a real local tmux, still durable, just an untagged byte stream.
#[test]
fn tc_lt4_local_plain_mode_runs_under_tmux() {
    let Some((tmux, ver)) = local_tmux() else { eprintln!("SKIP: no local tmux"); return };
    let session = "buoylt4";
    kill_server(&tmux, "plain", ver, session);

    let seen = Arc::new(Mutex::new(String::new()));
    let exited = Arc::new(AtomicBool::new(false));
    let (s2, e2) = (seen.clone(), exited.clone());
    let psink: buoy_lib::plain_backend::PlainSink = Arc::new(move |ev| match ev {
        PlainEvent::Data { data } => s2.lock().unwrap().push_str(&data),
        PlainEvent::Exit => e2.store(true, Ordering::Relaxed),
    });

    let b = PlainBackend::spawn(
        PlainConfig {
            host: String::new(), session: session.into(),
            tmux_path: tmux.clone(), tmux_version: ver, socket: String::new(),
            recovery_windows: vec![], base_args: vec![],
            transport: Transport::Local,
        }, psink, 80, 24,
    ).expect("local plain backend spawns");

    b.write("echo LOCAL_PLAIN_MARK\n");
    for _ in 0..40 {
        if seen.lock().unwrap().contains("LOCAL_PLAIN_MARK") { break; }
        sleep_ms(250);
    }
    let text = seen.lock().unwrap().clone();
    assert!(text.contains("LOCAL_PLAIN_MARK"), "plain local output came back; got: {text:?}");

    // It is genuinely inside tmux (a plain-socket server exists), not a bare shell.
    let socket = buoy_lib::tmux_socket::socket_name("plain", ver, session);
    let out = std::process::Command::new(&tmux)
        .args(["-L", &socket, "list-sessions"]).output().expect("list-sessions");
    assert!(String::from_utf8_lossy(&out.stdout).contains(session),
        "local plain mode really runs under tmux");

    b.kill();
    kill_server(&tmux, "plain", ver, session);
}

/// TC-LT5 unicode window names survive on a local tmux. tmux only stores UTF-8 when its own locale
/// is UTF-8, else it replaces every non-ASCII byte with '_' — which turned agent tab titles like
/// "✳ task" into "_ task". The ssh path forces LC_ALL in its argv; the local path sets it on the
/// child's environment, so this asserts that substitute actually works.
#[test]
fn tc_lt5_local_tmux_keeps_unicode_window_names() {
    let Some((tmux, ver)) = local_tmux() else { eprintln!("SKIP: no local tmux"); return };
    let session = "buoylt5";
    kill_server(&tmux, "control", ver, session);

    let rec = Arc::new(Mutex::new(Rec::default()));
    let backend = ControlBackend::spawn(
        BackendConfig {
            host: String::new(), session: session.into(),
            tmux_path: tmux.clone(), tmux_version: ver, socket: String::new(),
            recovery_windows: vec![], base_args: vec![],
            transport: Transport::Local,
        },
        sink(rec.clone()), 90, 30,
    ).expect("spawn");
    for _ in 0..40 { if rec.lock().unwrap().ready { break; } sleep_ms(250); }
    assert!(rec.lock().unwrap().ready, "ready");

    let win = rec.lock().unwrap().windows.first().cloned().expect("a window");
    backend.rename_window(&win, "✳ task");
    sleep_ms(1000);

    let socket = buoy_lib::tmux_socket::socket_name("control", ver, session);
    let out = std::process::Command::new(&tmux)
        .args(["-L", &socket, "display-message", "-p", "-t", session, "#{window_name}"])
        .output().expect("display-message");
    let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
    assert_eq!(name, "✳ task", "unicode window name stored intact (not mangled to '_')");

    backend.kill();
    kill_server(&tmux, "control", ver, session);
}

/// TC-LT6 reconnect/backfill must restore tmux's actual pane cursor. Full-screen TUIs such as
/// Codex and Claude Code keep an input cursor above a footer, so the final captured text row is not
/// the cursor row. This uses a real tmux pane positioned at row 5, column 7 and verifies the repaint
/// ends with that exact CSI position rather than a synthetic trailing newline.
#[test]
fn tc_lt6_capture_restores_real_cursor_position() {
    let Some((tmux, ver)) = local_tmux() else { eprintln!("SKIP: no local tmux"); return };
    let session = "buoylt6";
    kill_server(&tmux, "control", ver, session);

    let rec = Arc::new(Mutex::new(Rec::default()));
    let backend = ControlBackend::spawn(
        BackendConfig {
            host: String::new(), session: session.into(),
            tmux_path: tmux.clone(), tmux_version: ver, socket: String::new(),
            recovery_windows: vec![], base_args: vec![],
            transport: Transport::Local,
        },
        sink(rec.clone()), 90, 30,
    ).expect("spawn");
    for _ in 0..40 { if rec.lock().unwrap().ready { break; } sleep_ms(250); }
    assert!(rec.lock().unwrap().ready, "ready");
    let win = rec.lock().unwrap().windows.first().cloned().expect("a window");

    // Clear, draw one cell, place the cursor at 1-based row 5/column 7, then remain quiet while the
    // capture and cursor query complete. The captured cell stream itself contains no cursor CSI.
    backend.write("printf '\\033[2J\\033[Htop\\033[5;7H'; sleep 3\n");
    sleep_ms(500);
    rec.lock().unwrap().by_window.insert(win.clone(), String::new());
    backend.capture_window(&win);
    sleep_ms(500);

    let repaint = rec.lock().unwrap().by_window.get(&win).cloned().unwrap_or_default();
    assert!(repaint.ends_with("\x1b[5;7H"),
        "capture repaint restores tmux cursor and adds no newline; tail={:?}",
        repaint.chars().rev().take(32).collect::<String>().chars().rev().collect::<String>());

    backend.kill();
    kill_server(&tmux, "control", ver, session);
}
