//! Live test for ssh -L port forwarding (§18), opt-in (#[ignore]).
//! Starts an HTTP server on the remote's loopback, opens a tunnel via TunnelRegistry, and curls
//! the LOCAL forwarded port to confirm it serves the remote content. Verifies reuse + teardown.
//! Run: DT_LIVE_HOST=user@host DT_TMUX=/path cargo test --test live_tunnel -- --ignored --nocapture

use std::time::Duration;
use buoy_lib::tunnel::{classify_loopback, TunnelRegistry};

fn env(k: &str) -> Option<String> { std::env::var(k).ok().filter(|s| !s.is_empty()) }
fn sleep_ms(ms: u64) { std::thread::sleep(Duration::from_millis(ms)); }

// curl a local URL, return the body (empty on failure).
fn curl(url: &str) -> String {
    std::process::Command::new("curl")
        .args(["-s", "--max-time", "5", url])
        .output().ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        .unwrap_or_default()
}

#[test]
#[ignore]
fn live_tunnel_forwards_remote_loopback() {
    let host = match env("DT_LIVE_HOST") { Some(h) => h, None => { eprintln!("SKIP: set DT_LIVE_HOST"); return; } };
    let tmux = env("DT_TMUX").unwrap_or_else(|| "tmux".into());
    let rport = 18077u16;

    // classify sanity
    let (_, lb) = classify_loopback(&format!("http://localhost:{}/", rport),
        &["localhost".into(), "127.0.0.1".into()]).expect("classify");
    assert_eq!(lb.port, rport);

    // Start an HTTP server on the remote's loopback, hosted in a detached tmux window so it
    // survives the ssh command's exit. Serves a dir that contains a known marker file.
    let sh = |cmd: &str| { let _ = std::process::Command::new("ssh")
        .args(["-o", "BatchMode=yes", "--", &host, cmd]).status(); };
    sh(&format!("{t} -L tunhttp kill-server 2>/dev/null; \
                 mkdir -p /tmp/dt_tun && printf 'TUNNEL_OK' > /tmp/dt_tun/index.html && \
                 {t} -L tunhttp new-session -d -s h 'cd /tmp/dt_tun && python3 -m http.server {p} --bind 127.0.0.1'",
                t = tmux, p = rport));
    sleep_ms(2000);

    let reg = TunnelRegistry::new();
    let local = reg.ensure("sessX", &host, rport, &[]).expect("ensure tunnel");
    sleep_ms(1500); // let ssh -L establish

    // curl the LOCAL forwarded port -> should serve the remote content
    let body = curl(&format!("http://localhost:{}/", local));
    assert!(body.contains("TUNNEL_OK"), "local tunnel serves remote content (got {:?})", body);

    // reuse: a second ensure for the same (session, port) returns the SAME local port (no respawn)
    let local2 = reg.ensure("sessX", &host, rport, &[]).expect("ensure reuse");
    assert_eq!(local, local2, "tunnel reused for the same remote port");

    // list(): the sidebar's port list — one live tunnel (remote, local).
    let listed = reg.list("sessX");
    assert_eq!(listed, vec![(rport, local)], "list() reports the live tunnel");

    // status(): with a real server behind it, the port probes ACTIVE.
    let st = reg.status("sessX");
    assert_eq!(st.len(), 1);
    assert_eq!(st[0].remote, rport);
    assert_eq!(st[0].local, Some(local));
    assert!(st[0].active, "a served port probes active");

    // Stop the remote server -> the SAME tunnel now probes INACTIVE (ssh accepts locally but the
    // remote connect fails), and the row remains (persisted) so the user can close/re-open it.
    sh(&format!("{t} -L tunhttp kill-server 2>/dev/null", t = tmux));
    sleep_ms(1200);
    let st2 = reg.status("sessX");
    assert_eq!(st2.len(), 1, "port still listed after the server stops");
    assert!(!st2[0].active, "stopped server -> inactive (grey)");
    // restart the server for the remaining assertions
    sh(&format!("{t} -L tunhttp new-session -d -s h 'cd /tmp/dt_tun && python3 -m http.server {p} --bind 127.0.0.1'", t = tmux, p = rport));
    sleep_ms(1500);

    // close() ONE tunnel by remote port -> gone from the list and stops forwarding.
    assert!(reg.close("sessX", rport), "close() returns true for a live tunnel");
    assert!(reg.list("sessX").is_empty(), "list() empty after closing the only tunnel");
    sleep_ms(800);
    let after_one = curl(&format!("http://localhost:{}/", local));
    assert!(!after_one.contains("TUNNEL_OK"), "single-tunnel close stops forwarding");

    // teardown: close_session on a fresh tunnel also stops forwarding
    let local3 = reg.ensure("sessX", &host, rport, &[]).expect("re-ensure");
    sleep_ms(1200);
    reg.close_session("sessX");
    sleep_ms(800);
    let after = curl(&format!("http://localhost:{}/", local3));
    assert!(!after.contains("TUNNEL_OK"), "tunnel torn down on session close (got {:?})", after);

    // cleanup remote
    sh(&format!("{t} -L tunhttp kill-server 2>/dev/null; rm -rf /tmp/dt_tun", t = tmux));
    eprintln!("LIVE OK: ssh -L forward serves remote loopback, reuses, and tears down ({} host)", host);
}

// Reuse across "restart": open a tunnel, then build a NEW registry from the same store file
// (simulating an app relaunch). The still-alive orphan is ADOPTED and reused on the SAME local
// port — not duplicated. Closing then kills the adopted orphan (no leak).
#[test]
#[ignore]
fn live_tunnel_reused_across_restart() {
    let host = match env("DT_LIVE_HOST") { Some(h) => h, None => { eprintln!("SKIP: set DT_LIVE_HOST"); return; } };
    let tmux = env("DT_TMUX").unwrap_or_else(|| "tmux".into());
    let rport = 18066u16;
    let store = std::env::temp_dir().join(format!("dt_tun_reuse_{}.json", std::process::id()));
    let _ = std::fs::remove_file(&store);
    let sh = |cmd: &str| { let _ = std::process::Command::new("ssh")
        .args(["-o", "BatchMode=yes", "--", &host, cmd]).status(); };

    // remote server on rport
    sh(&format!("{t} -L tunreuse kill-server 2>/dev/null; mkdir -p /tmp/dt_reuse && printf 'REUSE_OK' > /tmp/dt_reuse/index.html && {t} -L tunreuse new-session -d -s h 'cd /tmp/dt_reuse && python3 -m http.server {p} --bind 127.0.0.1'", t = tmux, p = rport));
    for _ in 0..20 { sleep_ms(500);
        let ok = std::process::Command::new("ssh").args(["-o","BatchMode=yes","--",&host,
            &format!("curl -s -o /dev/null -w '%{{http_code}}' --max-time 2 http://127.0.0.1:{}/", rport)])
            .output().ok().map(|o| String::from_utf8_lossy(&o.stdout).trim()=="200").unwrap_or(false);
        if ok { break; } }

    // "run 1": open a tunnel, note the local port. Poll until the local forward establishes.
    let reg1 = TunnelRegistry::with_store(store.clone());
    let local = reg1.ensure("s", &host, rport, &[]).expect("ensure");
    let mut served = false;
    for _ in 0..16 { sleep_ms(500); if curl(&format!("http://localhost:{}/", local)).contains("REUSE_OK") { served = true; break; } }
    assert!(served, "run1 serves on local {}", local);
    // IMPORTANT: don't close — leave the tunnel running (simulates app quit without teardown).
    std::mem::forget(reg1);   // drop without running Drop (there is none, but be explicit about intent)

    // "run 2": fresh registry from the same store -> adopts the live orphan, reuses SAME local port.
    let reg2 = TunnelRegistry::with_store(store.clone());
    let st = reg2.status("s");
    assert_eq!(st.len(), 1, "one port after restart");
    assert_eq!(st[0].remote, rport);
    assert_eq!(st[0].local, Some(local), "adopted the SAME local port (reused, not duplicated)");
    assert!(st[0].active, "adopted orphan still serves -> active");
    // ensure() returns the adopted local port (no new ssh spawned)
    assert_eq!(reg2.ensure("s", &host, rport, &[]).expect("reuse"), local, "reuse returns same port");

    // close -> kills the adopted orphan (no leftover on that port)
    reg2.close("s", rport);
    sleep_ms(800);
    assert!(!curl(&format!("http://localhost:{}/", local)).contains("REUSE_OK"), "closed orphan stops forwarding");

    sh(&format!("{t} -L tunreuse kill-server 2>/dev/null; rm -rf /tmp/dt_reuse", t = tmux));
    let _ = std::fs::remove_file(&store);
    eprintln!("LIVE OK: tunnel adopted + reused across restart, then cleanly closed ({} host)", host);
}
