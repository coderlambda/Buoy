//! On-demand `ssh -L` port forwarding for localhost URLs in remote output (DESIGN.md §18).
//! A URL like http://localhost:3000 points at the REMOTE host's loopback; to open it in the local
//! browser we forward a free LOCAL port to the remote's localhost:<port> and hand the browser the
//! local URL. Tunnels are separate ssh processes (never the -CC channel), tracked per session and
//! reused across clicks to the same remote port, and torn down when the session closes.

use std::collections::{BTreeMap, HashMap};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::sync::Mutex;
use std::time::Duration;

use crate::validation::{parse_host, ValidationError};

/// One forwarded port's status for the sidebar: the remote port, its local port if a tunnel is
/// currently open, and whether the forward is ACTIVE (something is really answering on the remote).
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct TunnelStatus {
    pub remote: u16,
    pub local: Option<u16>,
    pub active: bool,
}

/// A parsed loopback URL: the remote port to reach and the path (incl. query) to open.
#[derive(Debug, PartialEq)]
pub struct LoopbackUrl {
    pub port: u16,
    pub path: String, // begins with '/', includes any ?query; '' -> "/"
}

/// Parse a clicked URL into (host, LoopbackUrl) when it targets a loopback host in `loopback_hosts`.
/// Accepts `host:port`, `http://host:port/path`, `https://…`. Returns None for non-loopback or
/// unparseable input. The returned host is the matched loopback token (for logging only).
pub fn classify_loopback(url: &str, loopback_hosts: &[String]) -> Option<(String, LoopbackUrl)> {
    // Strip scheme if present.
    let rest = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))
        .unwrap_or(url);
    // authority is up to the first '/', '?' or end.
    let auth_end = rest.find(|c| c == '/' || c == '?').unwrap_or(rest.len());
    let authority = &rest[..auth_end];
    let mut tail = &rest[auth_end..];
    if tail.is_empty() { tail = "/"; }

    // authority = host:port
    let (host, port_str) = authority.rsplit_once(':')?;
    if !loopback_hosts.iter().any(|h| h == host) {
        return None;
    }
    let port: u16 = port_str.parse().ok()?;
    if port == 0 { return None; }

    // sanitize path: keep only a leading-'/' path + query, reject control/space (untrusted text).
    let path = if tail.starts_with('/') { tail.to_string() } else { format!("/{}", tail) };
    if path.chars().any(|c| c.is_control() || c == ' ') {
        return None;
    }
    Some((host.to_string(), LoopbackUrl { port, path }))
}

/// Pick a free local TCP port by binding to 127.0.0.1:0 and reading the assigned port. There's an
/// inherent TOCTOU gap (the port could be taken before ssh binds it) but it's tiny and ssh's
/// ExitOnForwardFailure surfaces a collision as a spawn we can detect.
pub fn free_local_port() -> std::io::Result<u16> {
    let l = TcpListener::bind("127.0.0.1:0")?;
    Ok(l.local_addr()?.port())
}

struct Tunnel {
    local_port: u16,
    remote_port: u16,
    child: Child,
}

/// Probe a local forwarded port: connect + send a minimal HTTP HEAD and see if we get ANY bytes
/// back. A plain TCP connect is NOT enough — ssh's local listener accepts before forwarding, so a
/// dead remote port still "connects" (verified). Only an actual round-trip distinguishes them.
pub fn probe_local_port(local_port: u16) -> bool {
    let addr = format!("127.0.0.1:{}", local_port);
    let stream = TcpStream::connect_timeout(
        &addr.parse().unwrap(), Duration::from_millis(600));
    let mut s = match stream { Ok(s) => s, Err(_) => return false };
    let _ = s.set_read_timeout(Some(Duration::from_millis(700)));
    let _ = s.set_write_timeout(Some(Duration::from_millis(500)));
    // A bare HEAD; we don't care about the status, only that SOMETHING answers (dead remote ->
    // ssh closes/refuses the forwarded channel -> read returns 0/err).
    if s.write_all(b"HEAD / HTTP/1.0\r\n\r\n").is_err() { return false; }
    let mut buf = [0u8; 16];
    matches!(s.read(&mut buf), Ok(n) if n > 0)
}

/// Per-session set of live tunnels, keyed by remote port (so repeat clicks reuse one). Also
/// persists the set of remote ports per session to disk, so after an app restart the sidebar can
/// show the (inactive) rows and the user can re-open them.
pub struct TunnelRegistry {
    // session id -> (remote_port -> Tunnel)
    by_session: Mutex<HashMap<String, HashMap<u16, Tunnel>>>,
    // session id -> set of remote ports ever forwarded (persisted); source of truth for the list.
    persisted: Mutex<BTreeMap<String, Vec<u16>>>,
    store_path: Option<PathBuf>,
}

impl Default for TunnelRegistry {
    fn default() -> Self { Self::new() }
}

impl TunnelRegistry {
    pub fn new() -> Self {
        TunnelRegistry {
            by_session: Mutex::new(HashMap::new()),
            persisted: Mutex::new(BTreeMap::new()),
            store_path: None,
        }
    }

    /// Registry backed by a JSON file at `path` (tunnels.json); loads any existing persisted ports.
    pub fn with_store(path: PathBuf) -> Self {
        let persisted = std::fs::read_to_string(&path).ok()
            .and_then(|s| serde_json::from_str::<BTreeMap<String, Vec<u16>>>(&s).ok())
            .unwrap_or_default();
        TunnelRegistry {
            by_session: Mutex::new(HashMap::new()),
            persisted: Mutex::new(persisted),
            store_path: Some(path),
        }
    }

    fn save_persisted(&self, map: &BTreeMap<String, Vec<u16>>) {
        if let Some(path) = &self.store_path {
            if let Some(dir) = path.parent() { let _ = std::fs::create_dir_all(dir); }
            if let Ok(json) = serde_json::to_string_pretty(map) {
                let tmp = path.with_extension("json.tmp");
                if std::fs::write(&tmp, json).is_ok() { let _ = std::fs::rename(&tmp, path); }
            }
        }
    }

    fn remember(&self, session_id: &str, remote_port: u16) {
        let mut p = self.persisted.lock().unwrap();
        let ports = p.entry(session_id.to_string()).or_default();
        if !ports.contains(&remote_port) { ports.push(remote_port); ports.sort(); }
        let snapshot = p.clone();
        drop(p);
        self.save_persisted(&snapshot);
    }

    fn forget(&self, session_id: &str, remote_port: u16) {
        let mut p = self.persisted.lock().unwrap();
        if let Some(ports) = p.get_mut(session_id) {
            ports.retain(|&x| x != remote_port);
            if ports.is_empty() { p.remove(session_id); }
        }
        let snapshot = p.clone();
        drop(p);
        self.save_persisted(&snapshot);
    }

    /// The persisted+live status of a session's forwarded ports, for the sidebar. Each remote port
    /// (persisted or currently live) is probed: `active` is true only if the forward really answers.
    /// Reuses a live tunnel's local port; a persisted-but-not-open port has `local: None, active:false`.
    pub fn status(&self, session_id: &str) -> Vec<TunnelStatus> {
        // union of persisted ports and currently-live ones
        let persisted = self.persisted.lock().unwrap().get(session_id).cloned().unwrap_or_default();
        let live: Vec<(u16, u16)> = self.list(session_id); // prunes dead ssh
        let live_map: HashMap<u16, u16> = live.iter().cloned().collect();

        let mut ports: Vec<u16> = persisted.clone();
        for (rp, _) in &live { if !ports.contains(rp) { ports.push(*rp); } }
        ports.sort();

        ports.into_iter().map(|remote| {
            let local = live_map.get(&remote).copied();
            let active = match local { Some(lp) => probe_local_port(lp), None => false };
            TunnelStatus { remote, local, active }
        }).collect()
    }

    /// Ensure a tunnel exists for (session, remote_port); return the LOCAL port. Reuses a live one,
    /// else opens one on a random free local port.
    pub fn ensure(&self, session_id: &str, host: &str, remote_port: u16, base_args: &[String])
        -> Result<u16, String>
    {
        // Reuse a live tunnel.
        {
            let mut map = self.by_session.lock().unwrap();
            if let Some(per) = map.get_mut(session_id) {
                if let Some(t) = per.get_mut(&remote_port) {
                    match t.child.try_wait() {
                        Ok(None) => return Ok(t.local_port),
                        _ => { let _ = t.child.kill(); per.remove(&remote_port); }
                    }
                }
            }
        }
        let local_port = free_local_port().map_err(|e| e.to_string())?;
        self.spawn_tunnel(session_id, host, local_port, remote_port, base_args)
    }

    /// FORCE a tunnel that binds the LOCAL side to the SAME port number as the remote (so a remote
    /// localhost:3000 becomes localhost:3000 locally — matches apps that hardcode their port in
    /// redirects). Errors if that local port is already in use (the UI alerts). Replaces any
    /// existing tunnel for this remote port.
    pub fn force_same_port(&self, session_id: &str, host: &str, remote_port: u16, base_args: &[String])
        -> Result<u16, String>
    {
        // Is the local port free? (bind test — the same check ssh would fail on, surfaced early.)
        if TcpListener::bind(("127.0.0.1", remote_port)).is_err() {
            return Err(format!("local port {} is already in use", remote_port));
        }
        // Drop any existing tunnel for this remote port so we can rebind to the fixed local port.
        {
            let mut map = self.by_session.lock().unwrap();
            if let Some(per) = map.get_mut(session_id) {
                if let Some(mut t) = per.remove(&remote_port) { let _ = t.child.kill(); }
            }
        }
        self.spawn_tunnel(session_id, host, remote_port, remote_port, base_args)
    }

    /// Spawn `ssh -L 127.0.0.1:<local>:localhost:<remote>` and record it. `host` is
    /// [user@]host[:sshport]; `base_args` extra ssh opts.
    fn spawn_tunnel(&self, session_id: &str, host: &str, local_port: u16, remote_port: u16, base_args: &[String])
        -> Result<u16, String>
    {
        let parts = parse_host(host).map_err(|e: ValidationError| e.to_string())?;
        let target = match &parts.user {
            Some(u) => format!("{}@{}", u, parts.host),
            None => parts.host.clone(),
        };

        let mut args: Vec<String> = Vec::new();
        if let Some(p) = parts.port { args.push("-p".into()); args.push(p.to_string()); }
        args.extend([
            "-o".into(), "BatchMode=yes".into(),
            "-o".into(), "ExitOnForwardFailure=yes".into(),
            "-o".into(), "ServerAliveInterval=30".into(),
            "-N".into(),
            // bind the LOCAL side to 127.0.0.1 so only this machine can use the forward.
            "-L".into(), format!("127.0.0.1:{}:localhost:{}", local_port, remote_port),
        ]);
        args.extend(base_args.iter().cloned());
        args.push("--".into());
        args.push(target);

        let child = Command::new("ssh")
            .args(&args)
            .env("PATH", crate::augmented_path())
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|e| format!("ssh -L failed to start: {}", e))?;

        crate::dlog!("tunnel: session={} local {} -> remote localhost:{}", session_id, local_port, remote_port);
        self.by_session.lock().unwrap()
            .entry(session_id.to_string()).or_default()
            .insert(remote_port, Tunnel { local_port, remote_port, child });
        self.remember(session_id, remote_port);   // persist so it survives restart
        Ok(local_port)
    }

    /// List a session's live tunnels as (remote_port, local_port), pruning any whose ssh died.
    /// Sorted by remote port for stable display.
    pub fn list(&self, session_id: &str) -> Vec<(u16, u16)> {
        let mut map = self.by_session.lock().unwrap();
        let per = match map.get_mut(session_id) { Some(p) => p, None => return Vec::new() };
        // prune dead ones (ssh exited) in one mutable pass, collecting the live ones.
        let mut dead: Vec<u16> = Vec::new();
        let mut out: Vec<(u16, u16)> = Vec::new();
        for (rp, t) in per.iter_mut() {
            if matches!(t.child.try_wait(), Ok(None)) { out.push((*rp, t.local_port)); }
            else { dead.push(*rp); }
        }
        for rp in dead { if let Some(mut t) = per.remove(&rp) { let _ = t.child.kill(); } }
        out.sort_by_key(|(rp, _)| *rp);
        out
    }

    /// Close ONE tunnel (by remote port) for a session AND forget it from disk — this is the user
    /// explicitly dismissing the row, so it should not reappear on next launch. Returns true if a
    /// live tunnel was killed (also succeeds/forgets for a persisted-but-not-open port).
    pub fn close(&self, session_id: &str, remote_port: u16) -> bool {
        let killed = {
            let mut map = self.by_session.lock().unwrap();
            match map.get_mut(session_id).and_then(|per| per.remove(&remote_port)) {
                Some(mut t) => { let _ = t.child.kill();
                    crate::dlog!("tunnel: closed local {} (remote {}) for {}", t.local_port, remote_port, session_id);
                    true }
                None => false,
            }
        };
        self.forget(session_id, remote_port);
        killed
    }

    /// Tear down all LIVE tunnels for a session but KEEP the persisted port list (detach: the
    /// session survives, so its ports should still show — inactive — on next launch/reconnect).
    pub fn close_session(&self, session_id: &str) {
        if let Some(mut per) = self.by_session.lock().unwrap().remove(session_id) {
            for (_, mut t) in per.drain() {
                let _ = t.child.kill();
                crate::dlog!("tunnel: closed local {} (remote {})", t.local_port, t.remote_port);
            }
        }
    }

    /// Kill a session for good: tear down tunnels AND forget its persisted ports.
    pub fn forget_session(&self, session_id: &str) {
        self.close_session(session_id);
        let mut p = self.persisted.lock().unwrap();
        p.remove(session_id);
        let snapshot = p.clone();
        drop(p);
        self.save_persisted(&snapshot);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lh() -> Vec<String> { vec!["localhost".into(), "127.0.0.1".into()] }

    #[test]
    fn classify_bare_host_port() {
        assert_eq!(classify_loopback("localhost:3000", &lh()),
            Some(("localhost".into(), LoopbackUrl { port: 3000, path: "/".into() })));
        assert_eq!(classify_loopback("127.0.0.1:8080", &lh()),
            Some(("127.0.0.1".into(), LoopbackUrl { port: 8080, path: "/".into() })));
    }

    #[test]
    fn classify_with_scheme_and_path() {
        assert_eq!(classify_loopback("http://localhost:5173/app?x=1", &lh()),
            Some(("localhost".into(), LoopbackUrl { port: 5173, path: "/app?x=1".into() })));
        assert_eq!(classify_loopback("https://127.0.0.1:443/", &lh()),
            Some(("127.0.0.1".into(), LoopbackUrl { port: 443, path: "/".into() })));
    }

    #[test]
    fn classify_rejects_non_loopback_and_junk() {
        assert_eq!(classify_loopback("https://github.com/x", &lh()), None);
        assert_eq!(classify_loopback("http://example.com:80", &lh()), None);
        assert_eq!(classify_loopback("localhost", &lh()), None);        // no port
        assert_eq!(classify_loopback("localhost:0", &lh()), None);      // port 0
        assert_eq!(classify_loopback("localhost:99999", &lh()), None);  // out of u16 range
        // 0.0.0.0 not in the default loopback set
        assert_eq!(classify_loopback("0.0.0.0:3000", &lh()), None);
    }

    #[test]
    fn classify_respects_configurable_hosts() {
        let hosts = vec!["localhost".into(), "0.0.0.0".into()];
        assert!(classify_loopback("0.0.0.0:3000", &hosts).is_some());
        assert!(classify_loopback("127.0.0.1:3000", &hosts).is_none());  // not configured here
    }

    #[test]
    fn classify_rejects_control_chars_in_path() {
        assert_eq!(classify_loopback("localhost:3000/a\nb", &lh()), None);
    }

    #[test]
    fn free_port_is_usable() {
        let p = free_local_port().unwrap();
        assert!(p > 0);
        // can bind again after the picker dropped its listener
        assert!(TcpListener::bind(("127.0.0.1", p)).is_ok());
    }

    // probe: a real local HTTP-ish listener that answers -> active; nothing listening -> inactive.
    #[test]
    fn probe_active_vs_inactive() {
        use std::io::{Read, Write};
        // a listener that reads the request and writes a minimal response = "active"
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let h = std::thread::spawn(move || {
            if let Ok((mut s, _)) = l.accept() {
                let mut b = [0u8; 64]; let _ = s.read(&mut b);
                let _ = s.write_all(b"HTTP/1.0 200 OK\r\n\r\nok");
            }
        });
        assert!(probe_local_port(port), "a listener that answers is active");
        let _ = h.join();

        // a port with nothing listening -> inactive
        let dead = free_local_port().unwrap();
        assert!(!probe_local_port(dead), "no listener -> inactive");
    }

    // force_same_port refuses when the local port is already taken (surfaces as the UI alert).
    #[test]
    fn force_same_port_errors_when_local_taken() {
        // hold the port so force_same_port's bind-test fails; no ssh is ever spawned.
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let reg = TunnelRegistry::new();
        let err = reg.force_same_port("s", "me@h", port, &[]).unwrap_err();
        assert!(err.contains("already in use"), "reports port-in-use: {}", err);
    }

    // persistence: remembered ports survive a reload; status() shows them inactive (no live tunnel).
    #[test]
    fn persists_and_reports_inactive() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("dt_tun_test_{}.json", std::process::id()));
        let _ = std::fs::remove_file(&path);

        let reg = TunnelRegistry::with_store(path.clone());
        reg.remember("sess", 3000);
        reg.remember("sess", 5173);

        // a fresh registry from the same file sees the persisted ports as inactive rows
        let reg2 = TunnelRegistry::with_store(path.clone());
        let st = reg2.status("sess");
        assert_eq!(st.iter().map(|s| s.remote).collect::<Vec<_>>(), vec![3000, 5173]);
        assert!(st.iter().all(|s| s.local.is_none() && !s.active), "persisted-only -> inactive");

        // close() forgets one; forget_session clears the rest
        reg2.close("sess", 3000);
        let reg3 = TunnelRegistry::with_store(path.clone());
        assert_eq!(reg3.status("sess").iter().map(|s| s.remote).collect::<Vec<_>>(), vec![5173]);
        reg3.forget_session("sess");
        assert!(TunnelRegistry::with_store(path.clone()).status("sess").is_empty());

        let _ = std::fs::remove_file(&path);
    }
}
