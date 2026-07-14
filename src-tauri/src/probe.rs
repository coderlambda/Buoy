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
}
