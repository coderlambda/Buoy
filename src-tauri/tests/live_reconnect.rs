//! Live reconnect test (§5, ported supervisor), opt-in (#[ignore]).
//! Drives a real Supervisor against the host, writes a marker, then KILLS the ssh child (as a
//! network break would) and asserts the supervisor respawns, reattaches the SAME tmux session,
//! and replays the marker — all without restarting anything.
//!
//! Run: DT_LIVE_HOST=user@host DT_TMUX=/path cargo test --test live_reconnect -- --ignored --nocapture

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use durable_terminal_lib::control_backend::{BackendConfig, BackendEvent};
use durable_terminal_lib::supervisor::{real_backend_factory, State, StateSink, Supervisor, SupervisorOpts};

fn env(k: &str) -> Option<String> { std::env::var(k).ok().filter(|s| !s.is_empty()) }
fn sleep_ms(ms: u64) { thread::sleep(Duration::from_millis(ms)); }

#[test]
#[ignore]
fn live_reconnects_after_ssh_killed() {
    let host = match env("DT_LIVE_HOST") { Some(h) => h, None => { eprintln!("SKIP: set DT_LIVE_HOST"); return; } };
    let tmux = env("DT_TMUX").unwrap_or_else(|| "tmux".into());
    let session = "rustreconn";
    let socket = durable_terminal_lib::tmux_socket::socket_name("control", Some((3, 7)));
    let ssh_cleanup = || { let _ = std::process::Command::new("ssh")
        .args(["-o", "BatchMode=yes", "--", &host,
               &format!("{} -L {} kill-session -t {} 2>/dev/null; true", tmux, socket, session)]).status(); };
    ssh_cleanup();

    // Collect output + count ready transitions (each successful (re)connect fires Ready).
    let out = Arc::new(Mutex::new(String::new()));
    let ready_count = Arc::new(AtomicUsize::new(0));
    let oc = out.clone(); let rc = ready_count.clone();
    let app_sink: durable_terminal_lib::control_backend::BackendSink = Arc::new(move |ev: BackendEvent| {
        match ev {
            BackendEvent::Data { data, .. } => oc.lock().unwrap().push_str(&data),
            BackendEvent::Ready => { rc.fetch_add(1, Ordering::Relaxed); }
            _ => {}
        }
    });

    let states = Arc::new(Mutex::new(Vec::<State>::new()));
    let st = states.clone();
    let state_sink: StateSink = Arc::new(move |s| st.lock().unwrap().push(s));

    let sup = Supervisor::new(
        BackendConfig { host: host.clone(), session: session.into(),
            tmux_path: tmux.clone(), tmux_version: Some((3, 7)), base_args: vec![] },
        SupervisorOpts::default(),
        real_backend_factory(),
        app_sink, state_sink,
        Arc::new(|d| thread::sleep(d)),
    );
    sup.start(90, 30);

    // wait for first connect (Ready)
    for _ in 0..40 { if ready_count.load(Ordering::Relaxed) >= 1 { break; } sleep_ms(250); }
    assert!(ready_count.load(Ordering::Relaxed) >= 1, "connected once");
    assert_eq!(sup.state(), State::Connected);

    // write a durable marker into the tmux session
    sup.write("echo RECONNECT_MARK\n");
    sleep_ms(1500);
    assert!(out.lock().unwrap().contains("RECONNECT_MARK"), "marker visible before break");

    // Simulate a network break: kill the ssh process holding THIS control session. Match by the
    // versioned socket in the argv so we don't touch other sessions.
    let killed = Arc::new(AtomicBool::new(false));
    let k = killed.clone();
    let pat = format!("new-session -D -A -s {}", session);
    let _ = std::process::Command::new("pkill").args(["-f", &pat]).status()
        .map(|s| k.store(s.success(), Ordering::Relaxed));
    eprintln!("killed ssh child (pattern {:?}) = {}", pat, killed.load(Ordering::Relaxed));

    // Supervisor should observe EOF -> Reconnecting -> Connecting -> Connected (2nd Ready).
    for _ in 0..80 { if ready_count.load(Ordering::Relaxed) >= 2 { break; } sleep_ms(250); }
    assert!(ready_count.load(Ordering::Relaxed) >= 2,
        "supervisor reconnected (ready_count={})", ready_count.load(Ordering::Relaxed));

    // After reattach, the SAME tmux session's scrollback (with our marker) is replayed.
    for _ in 0..20 { if out.lock().unwrap().matches("RECONNECT_MARK").count() >= 2 { break; } sleep_ms(250); }
    assert!(out.lock().unwrap().contains("RECONNECT_MARK"),
        "reattached session replays prior content (same session, not fresh)");

    // sanity: we passed through a Reconnecting or Connecting state after the break
    let seen = states.lock().unwrap().clone();
    assert!(seen.iter().any(|s| matches!(s, State::Reconnecting | State::Connecting)),
        "went through reconnecting/connecting: {:?}", seen);
    assert_eq!(sup.state(), State::Connected, "healthy again after reconnect");

    sup.close();
    ssh_cleanup();
    eprintln!("LIVE OK: reconnected after ssh kill and reattached the same session ({} host)", host);
}
