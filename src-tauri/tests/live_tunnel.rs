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

// STICKY LOCAL PORT across a connection BREAK (§18/§23b). Distinct from the adopt-across-restart case
// above: there the ssh is still alive and gets reused. Here it DIES — the network-drop case — and the
// point is that the re-opened forward comes back on the SAME local port, so a browser tab already
// pointing at http://localhost:<local>/ keeps working instead of 404ing on a fresh random port.
// Also covers reestablish(), the hook the supervisor calls when the session reconnects.
#[test]
#[ignore]
fn live_tunnel_keeps_local_port_across_a_break() {
    let host = match env("DT_LIVE_HOST") { Some(h) => h, None => { eprintln!("SKIP: set DT_LIVE_HOST"); return; } };
    let tmux = env("DT_TMUX").unwrap_or_else(|| "tmux".into());
    let (rport_a, rport_b) = (18055u16, 18056u16);
    let store = std::env::temp_dir().join(format!("dt_tun_sticky_{}.json", std::process::id()));
    let _ = std::fs::remove_file(&store);
    let sh = |cmd: &str| { let _ = std::process::Command::new("ssh")
        .args(["-o", "BatchMode=yes", "--", &host, cmd]).status(); };

    // Two remote servers, so we can prove per-port stickiness (not just "some port came back").
    sh(&format!("{t} -L tunsticky kill-server 2>/dev/null; \
                 mkdir -p /tmp/dt_stickA /tmp/dt_stickB && \
                 printf 'STICKY_A' > /tmp/dt_stickA/index.html && \
                 printf 'STICKY_B' > /tmp/dt_stickB/index.html && \
                 {t} -L tunsticky new-session -d -s a 'cd /tmp/dt_stickA && python3 -m http.server {pa} --bind 127.0.0.1' && \
                 {t} -L tunsticky new-window -t a 'cd /tmp/dt_stickB && python3 -m http.server {pb} --bind 127.0.0.1'",
                t = tmux, pa = rport_a, pb = rport_b));
    // Wait for BOTH remote servers to answer on the remote's own loopback.
    for _ in 0..24 { sleep_ms(500);
        let ok = std::process::Command::new("ssh").args(["-o","BatchMode=yes","--",&host,
            &format!("curl -s -o /dev/null --max-time 2 http://127.0.0.1:{}/ && \
                      curl -s -o /dev/null --max-time 2 http://127.0.0.1:{}/", rport_a, rport_b)])
            .status().ok().map(|s| s.success()).unwrap_or(false);
        if ok { break; } }

    let reg = TunnelRegistry::with_store(store.clone());
    let serves = |local: u16, marker: &str| {
        let mut ok = false;
        for _ in 0..16 { sleep_ms(500);
            if curl(&format!("http://localhost:{}/", local)).contains(marker) { ok = true; break; } }
        ok
    };

    // Open both forwards; remember the local ports the user's browser now knows about.
    let a1 = reg.ensure("s", &host, rport_a, &[]).expect("ensure A");
    let b1 = reg.ensure("s", &host, rport_b, &[]).expect("ensure B");
    assert!(serves(a1, "STICKY_A"), "A serves on local {}", a1);
    assert!(serves(b1, "STICKY_B"), "B serves on local {}", b1);
    assert_ne!(a1, b1, "two remote ports get two distinct local ports");

    // BREAK the connection the way the network does: kill the tunnels (session survives remotely).
    // close_session is exactly what lib.rs runs on a drop/detach — it clears pids, keeps the ports.
    reg.close_session("s");
    sleep_ms(1000);
    assert!(!curl(&format!("http://localhost:{}/", a1)).contains("STICKY_A"), "A really stopped forwarding");
    assert!(!curl(&format!("http://localhost:{}/", b1)).contains("STICKY_B"), "B really stopped forwarding");
    // The rows survive the break, greyed (this is what the sidebar shows while disconnected).
    let during = reg.status("s");
    assert_eq!(during.iter().map(|s| s.remote).collect::<Vec<_>>(), vec![rport_a, rport_b]);
    assert!(during.iter().all(|s| !s.active), "no forward is active during the break");

    // RECONNECT: exactly what the supervisor's state sink now calls on Connected.
    let restored = reg.reestablish("s", &host, &[]);
    assert_eq!(restored, vec![(rport_a, a1), (rport_b, b1)],
        "both forwards came back on their ORIGINAL local ports (was: a new random port each time)");
    // And the URLs the browser already had still work — the whole point.
    assert!(serves(a1, "STICKY_A"), "the pre-break URL localhost:{} still serves A after reconnect", a1);
    assert!(serves(b1, "STICKY_B"), "the pre-break URL localhost:{} still serves B after reconnect", b1);
    let after = reg.status("s");
    assert!(after.iter().all(|s| s.active), "both rows are live again after the reconnect");

    // A port the user explicitly dismissed must NOT be resurrected by a later reconnect.
    reg.close("s", rport_a);
    sleep_ms(600);
    let restored2 = reg.reestablish("s", &host, &[]);
    assert_eq!(restored2, vec![(rport_b, b1)], "a closed port stays closed across reconnects");

    reg.forget_session("s");
    sleep_ms(600);
    sh(&format!("{t} -L tunsticky kill-server 2>/dev/null; rm -rf /tmp/dt_stickA /tmp/dt_stickB", t = tmux));
    let _ = std::fs::remove_file(&store);
    eprintln!("LIVE OK: local ports A={} B={} survived a break and reconnect ({} host)", a1, b1, host);
}
