//! Live integration test for the Rust ControlBackend against a real host (opt-in, #[ignore]).
//! Drives the backend event stream the same way the Electron gui-multitab/gui-revisit tests did:
//! connect -> open a 2nd tab -> run a distinct marker in each -> assert per-window isolation ->
//! re-select the first tab -> assert it still shows its own content (the capture-correlation bug).
//!
//! Run only when DT_LIVE_HOST is set:
//!   DT_LIVE_HOST=user@host DT_TMUX=/home/u/.local/bin/tmux \
//!     cargo test --test live_control_mode -- --ignored --nocapture

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use durable_terminal_lib::control_backend::{BackendConfig, BackendEvent, ControlBackend};

fn env(k: &str) -> Option<String> {
    std::env::var(k).ok().filter(|s| !s.is_empty())
}

/// Accumulates per-window output text and the active-window/order from backend events.
#[derive(Default)]
struct Recorder {
    by_window: HashMap<String, String>,
    windows: Vec<String>,
    active: Option<String>,
    ready: bool,
}

fn sink(rec: Arc<Mutex<Recorder>>) -> durable_terminal_lib::control_backend::BackendSink {
    Arc::new(move |ev: BackendEvent| {
        let mut r = rec.lock().unwrap();
        match ev {
            BackendEvent::Data { window, data } => {
                r.by_window.entry(window).or_default().push_str(&data);
            }
            BackendEvent::WindowAdd { window, order } => { r.windows = order; let _ = window; }
            BackendEvent::WindowClose { order, .. } => { r.windows = order; }
            BackendEvent::WindowActive { window, order } => { r.active = Some(window); r.windows = order; }
            BackendEvent::WindowRename { .. } => {}
            BackendEvent::Ready => { r.ready = true; }
            BackendEvent::Exit => {}
        }
    })
}

fn sleep_ms(ms: u64) { thread::sleep(Duration::from_millis(ms)); }

#[test]
#[ignore]
fn live_tab_isolation_and_revisit() {
    let host = match env("DT_LIVE_HOST") { Some(h) => h, None => { eprintln!("SKIP: set DT_LIVE_HOST"); return; } };
    let tmux = env("DT_TMUX").unwrap_or_else(|| "tmux".into());
    let session = "rusttest";

    // Clean any prior session on the versioned socket so we start fresh.
    let socket = durable_terminal_lib::tmux_socket::socket_name("control", Some((3, 7)));
    let _ = std::process::Command::new("ssh")
        .args(["-o", "BatchMode=yes", "--", &host,
               &format!("{} -L {} kill-session -t {} 2>/dev/null; true", tmux, socket, session)])
        .status();

    let rec = Arc::new(Mutex::new(Recorder::default()));
    let backend = ControlBackend::spawn(
        BackendConfig {
            host: host.clone(), session: session.into(),
            tmux_path: tmux.clone(), tmux_version: Some((3, 7)), base_args: vec![],
        },
        sink(rec.clone()), 90, 30,
    ).expect("spawn");

    // wait for ready
    for _ in 0..40 { if rec.lock().unwrap().ready { break; } sleep_ms(250); }
    assert!(rec.lock().unwrap().ready, "backend never became ready");

    let first = rec.lock().unwrap().windows.first().cloned().expect("first window");

    // run a marker in tab A
    backend.write("echo AAA_MARK\n");
    sleep_ms(1500);
    assert!(rec.lock().unwrap().by_window.get(&first).map(|s| s.contains("AAA_MARK")).unwrap_or(false),
        "tab A shows its own output");

    // open tab B
    backend.new_window();
    sleep_ms(2500);
    let wins = rec.lock().unwrap().windows.clone();
    assert_eq!(wins.len(), 2, "two windows after new-window (got {})", wins.len());
    let second = rec.lock().unwrap().active.clone().expect("active after new-window");
    assert_ne!(second, first, "new tab is active");

    // run a distinct marker in tab B
    backend.write("echo BBB_MARK\n");
    sleep_ms(1500);
    {
        let r = rec.lock().unwrap();
        let b = r.by_window.get(&second).cloned().unwrap_or_default();
        assert!(b.contains("BBB_MARK"), "tab B shows its own output");
        assert!(!b.contains("AAA_MARK"), "tab B is isolated from tab A");
    }

    // switch back to A, then re-select B; B must still be B (revisit bug)
    backend.select_window(&first);
    sleep_ms(1200);
    backend.capture_window(&first);
    sleep_ms(1200);
    backend.select_window(&second);
    sleep_ms(1200);
    backend.capture_window(&second);
    sleep_ms(1500);
    {
        let r = rec.lock().unwrap();
        let b = r.by_window.get(&second).cloned().unwrap_or_default();
        assert!(b.contains("BBB_MARK"), "re-visited tab B keeps its own output");
        assert!(!b.contains("AAA_MARK"), "re-visited tab B not showing tab A (revisit bug)");
    }

    // cleanup
    backend.kill();
    let _ = std::process::Command::new("ssh")
        .args(["-o", "BatchMode=yes", "--", &host,
               &format!("{} -L {} kill-session -t {} 2>/dev/null; true", tmux, socket, session)])
        .status();
    eprintln!("LIVE OK: tab isolation + revisit verified against {}", host);
}
