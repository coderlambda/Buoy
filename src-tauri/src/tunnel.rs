//! On-demand `ssh -L` port forwarding for localhost URLs in remote output (DESIGN.md §18).
//! A URL like http://localhost:3000 points at the REMOTE host's loopback; to open it in the local
//! browser we forward a free LOCAL port to the remote's localhost:<port> and hand the browser the
//! local URL. Tunnels are separate ssh processes (never the -CC channel), tracked per session and
//! reused across clicks to the same remote port, and torn down when the session closes.

use std::collections::HashMap;
use std::net::TcpListener;
use std::process::{Child, Command};
use std::sync::Mutex;

use crate::validation::{parse_host, ValidationError};

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

/// Per-session set of live tunnels, keyed by remote port (so repeat clicks reuse one).
pub struct TunnelRegistry {
    // session id -> (remote_port -> Tunnel)
    by_session: Mutex<HashMap<String, HashMap<u16, Tunnel>>>,
}

impl Default for TunnelRegistry {
    fn default() -> Self { Self::new() }
}

impl TunnelRegistry {
    pub fn new() -> Self {
        TunnelRegistry { by_session: Mutex::new(HashMap::new()) }
    }

    /// Ensure a tunnel exists for (session, remote_port); return the LOCAL port. Reuses a live one.
    /// `host` is the [user@]host[:sshport] connection string; `base_args` extra ssh opts.
    pub fn ensure(&self, session_id: &str, host: &str, remote_port: u16, base_args: &[String])
        -> Result<u16, String>
    {
        let mut map = self.by_session.lock().unwrap();
        let per = map.entry(session_id.to_string()).or_default();

        // Reuse if the existing tunnel's ssh is still alive.
        if let Some(t) = per.get_mut(&remote_port) {
            match t.child.try_wait() {
                Ok(None) => return Ok(t.local_port),           // still running
                _ => { let _ = t.child.kill(); per.remove(&remote_port); }  // died -> respawn
            }
        }

        let parts = parse_host(host).map_err(|e: ValidationError| e.to_string())?;
        let target = match &parts.user {
            Some(u) => format!("{}@{}", u, parts.host),
            None => parts.host.clone(),
        };
        let local_port = free_local_port().map_err(|e| e.to_string())?;

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
        per.insert(remote_port, Tunnel { local_port, remote_port, child });
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

    /// Close ONE tunnel (by remote port) for a session. Returns true if one was closed.
    pub fn close(&self, session_id: &str, remote_port: u16) -> bool {
        let mut map = self.by_session.lock().unwrap();
        if let Some(per) = map.get_mut(session_id) {
            if let Some(mut t) = per.remove(&remote_port) {
                let _ = t.child.kill();
                crate::dlog!("tunnel: closed local {} (remote {}) for {}", t.local_port, remote_port, session_id);
                return true;
            }
        }
        false
    }

    /// Tear down all tunnels for a session (called on session close/kill).
    pub fn close_session(&self, session_id: &str) {
        if let Some(mut per) = self.by_session.lock().unwrap().remove(session_id) {
            for (_, mut t) in per.drain() {
                let _ = t.child.kill();
                crate::dlog!("tunnel: closed local {} (remote {})", t.local_port, t.remote_port);
            }
        }
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
}
