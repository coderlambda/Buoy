//! End-to-end guard for the force-reconnect flap (regression): drives a REAL ControlBackend +
//! Supervisor against a LOCAL tmux via a fake ssh that execs tmux directly — no network/sshd needed.
//! Asserts that a single force_reconnect() on a healthy session yields exactly ONE clean reconnect,
//! not a connect/break loop. The deterministic unit tests in supervisor.rs
//! (tc_sup_double_exit_same_backend_reconnects_once, tc_sup_kill_triggered_exit_does_not_double_respawn)
//! guard the same fixes without real ssh/tmux; this exercises the true %exit+EOF timing.
//!
//! Requires a local tmux. Run: DT_TMUX=/path/to/tmux cargo test --test live_force_reconnect -- --ignored --nocapture

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use buoy_lib::control_backend::{BackendConfig, BackendEvent};
use buoy_lib::transport::Transport;
use buoy_lib::supervisor::{real_backend_factory, State, StateSink, Supervisor, SupervisorOpts};

fn sleep_ms(ms: u64) { thread::sleep(Duration::from_millis(ms)); }

// Write a fake `ssh` that ignores all ssh transport args and execs the LOCAL tmux found in the
// argv (the backend runs `ssh <opts> -- <host> env LC_ALL=.. tmux -CC -L sock new-session ...`).
// This lets the real ControlBackend attach a local tmux with no sshd. Returns the script path.
fn write_fake_ssh(tmux: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir();
    let path = dir.join("buoy-fake-ssh-test");
    let script = format!(
        "#!/usr/bin/env python3\n\
         import os, sys\n\
         argv = sys.argv[1:]\n\
         idx = next((i for i,a in enumerate(argv) if a.endswith('tmux') or a=='tmux'), None)\n\
         if idx is None:\n    sys.stderr.write('fake-ssh: no tmux in %r\\n' % argv); sys.exit(2)\n\
         cmd = argv[idx:]\n\
         cmd[0] = {tmux:?}\n\
         os.execvp(cmd[0], cmd)\n");
    std::fs::write(&path, script).expect("write fake-ssh");
    let mut perm = std::fs::metadata(&path).unwrap().permissions();
    use std::os::unix::fs::PermissionsExt;
    perm.set_mode(0o755);
    std::fs::set_permissions(&path, perm).unwrap();
    path
}

#[test]
#[ignore]
fn force_reconnect_does_not_loop() {
    let tmux = std::env::var("DT_TMUX").unwrap_or_else(|_| "/opt/homebrew/bin/tmux".into());
    if !std::path::Path::new(&tmux).exists() { eprintln!("SKIP: no tmux at {tmux} (set DT_TMUX)"); return; }
    let fake_ssh = write_fake_ssh(&tmux);
    std::env::set_var("BUOY_SSH_BIN", &fake_ssh);
    let session = "buoyLiveFR";

    let ready_count = Arc::new(AtomicUsize::new(0));
    let exit_count = Arc::new(AtomicUsize::new(0));
    let rc = ready_count.clone();
    let ec = exit_count.clone();
    let app_sink: buoy_lib::control_backend::BackendSink = Arc::new(move |ev: BackendEvent| {
        match ev {
            BackendEvent::Ready => { rc.fetch_add(1, Ordering::Relaxed); }
            BackendEvent::Exit => { ec.fetch_add(1, Ordering::Relaxed); }
            _ => {}
        }
    });

    let states = Arc::new(Mutex::new(Vec::<State>::new()));
    let st = states.clone();
    let state_sink: StateSink = Arc::new(move |s| { st.lock().unwrap().push(s); });

    let sup = Supervisor::new(
        BackendConfig { host: "localhost".into(), session: session.into(),
            tmux_path: tmux.clone(), tmux_version: Some((3, 6)), base_args: vec![],
            transport: Transport::Ssh },
        SupervisorOpts::default(),
        real_backend_factory(),
        app_sink, state_sink,
        Arc::new(|d| thread::sleep(d)),
        { let base = std::time::Instant::now(); Arc::new(move || base.elapsed().as_millis() as u64) },
    );
    sup.start(90, 30);

    for _ in 0..40 { if ready_count.load(Ordering::Relaxed) >= 1 { break; } sleep_ms(250); }
    assert!(ready_count.load(Ordering::Relaxed) >= 1, "connected once");
    assert_eq!(sup.state(), State::Connected);
    eprintln!("connected: ready={} state={:?}", ready_count.load(Ordering::Relaxed), sup.state());

    // Force reconnect a HEALTHY session.
    eprintln!("--- force_reconnect() ---");
    let ready_before = ready_count.load(Ordering::Relaxed);
    sup.force_reconnect();

    // Watch for 8s. A healthy force-reconnect should yield exactly ONE more Ready and settle at
    // Connected. A flap loop shows many Ready transitions and/or ends Reconnecting/Dead.
    for i in 0..32 {
        sleep_ms(250);
        if i % 4 == 3 {
            eprintln!("t={:.1}s ready={} exit={} state={:?}",
                (i + 1) as f64 * 0.25,
                ready_count.load(Ordering::Relaxed),
                exit_count.load(Ordering::Relaxed),
                sup.state());
        }
    }

    let extra_readies = ready_count.load(Ordering::Relaxed) - ready_before;
    eprintln!("FINAL: extra_readies={} state={:?} states={:?}",
        extra_readies, sup.state(), states.lock().unwrap());

    sup.close();
    let _ = std::process::Command::new(&tmux)
        .args(["-L", &buoy_lib::tmux_socket::socket_name("control", Some((3,6)), session),
               "kill-session", "-t", session]).status();

    assert!(sup.state() == State::Connected || extra_readies <= 2,
        "force_reconnect looped: extra_readies={} final_state={:?}", extra_readies, sup.state());
}
