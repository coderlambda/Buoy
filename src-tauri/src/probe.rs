//! Probe a remote host (once, at session create) to choose the best tmux binary.
//! Port of src/main/probeTmux.js. Non-interactive ssh so it doesn't trigger login-shell auto-tmux.

use std::process::Command;

use crate::validation::{base64_encode, parse_host};

const CANDIDATES: [&str; 3] = ["$HOME/.local/bin/tmux", "/usr/local/bin/tmux", "/usr/bin/tmux"];
const MIN_MODERN: (u32, u32) = (3, 2);

pub struct ProbeResult {
    pub tmux_path: String,
    pub version: Option<(u32, u32)>,
    #[allow(dead_code)] // whether an actual probe ran (vs fallback); surfaced to UI in fuller build
    pub probed: bool,
}

/// Parse "tmux 3.5a" / "tmux 1.8" / "tmux next-3.4" -> (major, minor).
pub fn parse_version(s: &str) -> Option<(u32, u32)> {
    // find "tmux", optional "next-", then <maj>.<min>
    let idx = s.find("tmux ")?;
    let mut rest = &s[idx + 5..];
    if let Some(r) = rest.strip_prefix("next-") { rest = r; }
    let mut chars = rest.chars().peekable();
    let mut maj = String::new();
    while let Some(&c) = chars.peek() {
        if c.is_ascii_digit() { maj.push(c); chars.next(); } else { break; }
    }
    if chars.peek() != Some(&'.') || maj.is_empty() { return None; }
    chars.next(); // '.'
    let mut min = String::new();
    while let Some(&c) = chars.peek() {
        if c.is_ascii_digit() { min.push(c); chars.next(); } else { break; }
    }
    if min.is_empty() { return None; }
    Some((maj.parse().ok()?, min.parse().ok()?))
}

fn gte(a: (u32, u32), b: (u32, u32)) -> bool {
    if a.0 != b.0 { a.0 > b.0 } else { a.1 >= b.1 }
}

/// Probe over ssh. Never errors hard — falls back to bare "tmux" so session creation isn't blocked.
pub fn probe_tmux(raw_host: &str, base_args: &[String]) -> ProbeResult {
    let parts = match parse_host(raw_host) {
        Ok(p) => p,
        Err(_) => return fallback(),
    };
    let target = match &parts.user {
        Some(u) => format!("{}@{}", u, parts.host),
        None => parts.host.clone(),
    };

    // POSIX probe script, base64-wrapped so the login shell (often zsh) can't mangle it.
    let script = CANDIDATES.iter()
        .map(|c| format!("test -x \"{c}\" && printf '%s\\t%s\\n' \"{c}\" \"$(\"{c}\" -V 2>/dev/null)\""))
        .collect::<Vec<_>>()
        .join("; ");
    let b64 = base64_encode(script.as_bytes());
    let remote = format!("echo {} | base64 -d | /bin/sh", b64);

    let mut args: Vec<String> = Vec::new();
    if let Some(p) = parts.port { args.push("-p".into()); args.push(p.to_string()); }
    args.extend(["-o".into(), "BatchMode=yes".into(), "-o".into(), "ConnectTimeout=8".into()]);
    args.extend(base_args.iter().cloned());
    args.extend(["--".into(), target, remote]);

    let output = Command::new("ssh")
        .args(&args)
        .env("PATH", crate::augmented_path())
        .output();

    let stdout = match output {
        Ok(o) => String::from_utf8_lossy(&o.stdout).to_string(),
        Err(_) => return fallback(),
    };

    let mut found: Vec<(String, (u32, u32))> = Vec::new();
    for line in stdout.lines() {
        let mut it = line.splitn(2, '\t');
        if let (Some(path), Some(vstr)) = (it.next(), it.next()) {
            if let Some(ver) = parse_version(vstr) {
                found.push((path.trim().to_string(), ver));
            }
        }
    }
    if found.is_empty() {
        return fallback();
    }
    // Prefer modern (>=3.2), highest version; else highest available.
    let modern: Vec<_> = found.iter().filter(|(_, v)| gte(*v, MIN_MODERN)).cloned().collect();
    let mut pool = if !modern.is_empty() { modern } else { found };
    pool.sort_by(|a, b| b.1.cmp(&a.1));
    let (path, ver) = pool.into_iter().next().unwrap();
    ProbeResult { tmux_path: path, version: Some(ver), probed: true }
}

fn fallback() -> ProbeResult {
    ProbeResult { tmux_path: "tmux".into(), version: None, probed: false }
}

/// Probe THIS machine for tmux (kind:'local' — DESIGN.md §5.3b). Same selection rule as the ssh
/// probe (prefer >= 3.2, then highest version) but no ssh and no shell: each candidate is exec'd
/// directly, so nothing here can be influenced by the user's login-shell config.
///
/// Returns `probed: false` with no version when tmux is absent — the caller then falls back to a
/// raw pty (a local shell with no tmux at all), which is the one case where a local session cannot
/// be durable.
pub fn probe_local_tmux() -> ProbeResult {
    let home = std::env::var("HOME").unwrap_or_default();
    // PATH first (respects a user's chosen tmux, e.g. a newer one earlier in PATH), then the same
    // absolute candidates the ssh probe uses, so a Finder-launched app with a bare PATH still finds
    // Homebrew/MacPorts installs.
    let mut candidates: Vec<String> = Vec::new();
    for dir in augmented_path_dirs() {
        candidates.push(format!("{dir}/tmux"));
    }
    for c in ["/opt/homebrew/bin/tmux", "/usr/local/bin/tmux", "/opt/local/bin/tmux", "/usr/bin/tmux"] {
        candidates.push(c.to_string());
    }
    if !home.is_empty() { candidates.push(format!("{home}/.local/bin/tmux")); }

    let mut found: Vec<(String, (u32, u32))> = Vec::new();
    let mut seen: std::collections::BTreeSet<String> = Default::default();
    for path in candidates {
        if !seen.insert(path.clone()) { continue; }
        // A path with characters the argv builder would reject is unusable even if it exists, so
        // skip it here rather than discovering that at spawn time.
        if !crate::validation::is_safe_tmux_path(&path) { continue; }
        if !std::path::Path::new(&path).is_file() { continue; }
        let out = match Command::new(&path).arg("-V").output() {
            Ok(o) => o,
            Err(_) => continue,
        };
        let text = String::from_utf8_lossy(&out.stdout).to_string();
        if let Some(ver) = parse_version(&text) { found.push((path, ver)); }
    }
    if found.is_empty() { return ProbeResult { tmux_path: "tmux".into(), version: None, probed: false }; }
    let modern: Vec<_> = found.iter().filter(|(_, v)| gte(*v, MIN_MODERN)).cloned().collect();
    let mut pool = if !modern.is_empty() { modern } else { found };
    pool.sort_by(|a, b| b.1.cmp(&a.1));
    let (path, ver) = pool.into_iter().next().unwrap();
    ProbeResult { tmux_path: path, version: Some(ver), probed: true }
}

/// PATH entries (already augmented for a Finder-launched app), in order.
fn augmented_path_dirs() -> Vec<String> {
    crate::augmented_path().split(':').filter(|s| !s.is_empty()).map(|s| s.to_string()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_versions() {
        assert_eq!(parse_version("tmux 3.5a"), Some((3, 5)));
        assert_eq!(parse_version("tmux 1.8"), Some((1, 8)));
        assert_eq!(parse_version("tmux next-3.4"), Some((3, 4)));
        assert_eq!(parse_version("garbage"), None);
    }

    #[test]
    fn gte_check() {
        assert!(gte((3, 7), (3, 2)));
        assert!(gte((4, 0), (3, 9)));
        assert!(!gte((3, 1), (3, 2)));
    }

    // TC-P-L1 the local probe finds THIS machine's tmux (when installed) and reports a usable
    // absolute path + version, with no ssh involved. A machine without tmux is a valid outcome
    // (probed:false -> the raw-pty fallback), so both branches are asserted rather than requiring
    // tmux in CI.
    #[test]
    fn tc_p_l1_probe_local_tmux() {
        let r = probe_local_tmux();
        if r.probed {
            let v = r.version.expect("a probed local tmux reports its version");
            assert!(v.0 >= 1, "plausible major version, got {v:?}");
            assert!(std::path::Path::new(&r.tmux_path).is_file(),
                "probed path exists: {}", r.tmux_path);
            assert!(crate::validation::is_safe_tmux_path(&r.tmux_path),
                "probed path passes the argv charset: {}", r.tmux_path);
            // it must be the real binary, not the bare-name fallback
            assert_ne!(r.tmux_path, "tmux", "a successful probe resolves an absolute path");
        } else {
            assert_eq!(r.tmux_path, "tmux");
            assert_eq!(r.version, None);
        }
    }
}
