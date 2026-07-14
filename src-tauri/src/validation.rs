//! Input validation + safe argv construction for the ssh/tmux launch command (DESIGN.md §6.1).
//! Port of src/shared/validation.js. Two injection surfaces are closed here:
//!   (a) remote shell injection via the tmux session name (lands in a `-c`/`kill` shell string)
//!   (b) argv flag-injection via host/user/port (they are positional/option args)
//! Every renderer-supplied value is validated HERE before any argv is built.

use std::fmt;

const MAX_LEN: usize = 64;

#[derive(Debug, Clone)]
pub struct ValidationError {
    pub field: String,
    pub why: String,
}

impl ValidationError {
    fn new(field: &str, why: &str) -> Self {
        ValidationError { field: field.into(), why: why.into() }
    }
}

impl fmt::Display for ValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid {}: {}", self.field, self.why)
    }
}
impl std::error::Error for ValidationError {}

type Result<T> = std::result::Result<T, ValidationError>;

// --- charset predicates (mirror the JS regexes) ---------------------------------------------

fn is_alnum(c: char) -> bool { c.is_ascii_alphanumeric() }

// SESSION_RE: ^[A-Za-z0-9][A-Za-z0-9_-]*$
fn matches_session(s: &str) -> bool {
    let mut it = s.chars();
    match it.next() {
        Some(c) if is_alnum(c) => {}
        _ => return false,
    }
    it.all(|c| is_alnum(c) || c == '_' || c == '-')
}

// USER_RE: ^[A-Za-z0-9][A-Za-z0-9._-]*$
fn matches_user(s: &str) -> bool {
    let mut it = s.chars();
    match it.next() {
        Some(c) if is_alnum(c) => {}
        _ => return false,
    }
    it.all(|c| is_alnum(c) || c == '.' || c == '_' || c == '-')
}

// HOST_RE: ^[A-Za-z0-9][A-Za-z0-9.-]*$
fn matches_host(s: &str) -> bool {
    let mut it = s.chars();
    match it.next() {
        Some(c) if is_alnum(c) => {}
        _ => return false,
    }
    it.all(|c| is_alnum(c) || c == '.' || c == '-')
}

// IPV6_RE: ^[0-9A-Fa-f:]+$
fn matches_ipv6(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_hexdigit() || c == ':')
}

// tmuxPath charset for buildSshArgs: ^[A-Za-z0-9._/-]+$
fn matches_tmux_path(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| is_alnum(c) || matches!(c, '.' | '_' | '/' | '-'))
}
// tmuxPath charset for kill (also allows '$'): ^[A-Za-z0-9._/$-]+$
fn matches_tmux_path_kill(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| is_alnum(c) || matches!(c, '.' | '_' | '/' | '$' | '-'))
}

// socket charset: ^[A-Za-z0-9_-]+$
fn matches_socket(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| is_alnum(c) || c == '_' || c == '-')
}

pub fn validate_session(s: &str) -> Result<String> {
    if s.is_empty() || s.len() > MAX_LEN {
        return Err(ValidationError::new("session", "empty or too long"));
    }
    if !matches_session(s) {
        return Err(ValidationError::new(
            "session",
            "must start alphanumeric and contain only [A-Za-z0-9_-] (no dot, no leading dash, no shell metacharacters)",
        ));
    }
    Ok(s.to_string())
}

#[derive(Debug, Clone, PartialEq)]
pub struct HostParts {
    pub user: Option<String>,
    pub host: String,
    pub port: Option<u16>,
    pub is_ipv6: bool,
}

/// Parse and validate [user@]host[:port] with IPv6 support (mirrors parseHost in JS).
pub fn parse_host(input: &str) -> Result<HostParts> {
    if input.is_empty() || input.len() > 255 {
        return Err(ValidationError::new("host", "empty or too long"));
    }
    let mut rest = input;
    let mut user: Option<String> = None;

    if let Some(at) = rest.find('@') {
        let u = &rest[..at];
        rest = &rest[at + 1..];
        if !matches_user(u) {
            return Err(ValidationError::new("user", "invalid username (no leading dash / slash / metacharacters)"));
        }
        user = Some(u.to_string());
    }

    let host: String;
    let mut port: Option<u16> = None;
    let mut is_ipv6 = false;

    if let Some(stripped) = rest.strip_prefix('[') {
        // Bracketed IPv6: [addr] or [addr]:port
        let close = stripped.find(']').ok_or_else(|| ValidationError::new("host", "unterminated IPv6 bracket"))?;
        host = stripped[..close].to_string();
        is_ipv6 = true;
        let tail = &stripped[close + 1..];
        if let Some(p) = tail.strip_prefix(':') {
            port = Some(parse_port(p)?);
        } else if !tail.is_empty() {
            return Err(ValidationError::new("host", "garbage after IPv6 bracket"));
        }
    } else if rest.matches(':').count() >= 2 {
        // Bare IPv6 (2+ colons): no port possible; route port via -p separately.
        host = rest.to_string();
        is_ipv6 = true;
    } else if let Some(colon) = rest.find(':') {
        port = Some(parse_port(&rest[colon + 1..])?);
        host = rest[..colon].to_string();
    } else {
        host = rest.to_string();
    }

    if is_ipv6 {
        if !matches_ipv6(&host) {
            return Err(ValidationError::new("host", "invalid IPv6 address"));
        }
    } else if !matches_host(&host) {
        return Err(ValidationError::new("host", "invalid host (no leading dash / metacharacters)"));
    }

    Ok(HostParts { user, host, port, is_ipv6 })
}

fn parse_port(p: &str) -> Result<u16> {
    if p.is_empty() || p.len() > 5 || !p.chars().all(|c| c.is_ascii_digit()) {
        return Err(ValidationError::new("port", "not numeric"));
    }
    let n: u32 = p.parse().map_err(|_| ValidationError::new("port", "not numeric"))?;
    if n < 1 || n > 65535 {
        return Err(ValidationError::new("port", "out of range 1..65535"));
    }
    Ok(n as u16)
}

fn host_token(parts: &HostParts) -> String {
    match &parts.user {
        Some(u) => format!("{}@{}", u, parts.host),
        None => parts.host.clone(),
    }
}

/// Build ssh argv for a session (mirrors buildSshArgs):
///   ssh -tt [-p port] [baseArgs] -- <user@host> <tmuxPath> -L <socket> new-session -A -s <name>
pub fn build_ssh_args(
    raw_host: &str,
    raw_session: &str,
    base_args: &[String],
    tmux_path: &str,
    socket: &str,
) -> Result<Vec<String>> {
    let session = validate_session(raw_session)?;
    let parts = parse_host(raw_host)?;
    if !matches_tmux_path(tmux_path) {
        return Err(ValidationError::new("tmuxPath", "invalid path"));
    }
    if !matches_socket(socket) {
        return Err(ValidationError::new("socket", "invalid socket name"));
    }

    let mut args: Vec<String> = vec!["-tt".into()];
    if let Some(p) = parts.port {
        args.push("-p".into());
        args.push(p.to_string());
    }
    args.extend(base_args.iter().cloned());
    args.push("--".into());
    args.push(host_token(&parts));
    args.extend([
        tmux_path.to_string(),
        "-L".into(),
        socket.to_string(),
        "new-session".into(),
        "-A".into(),
        "-s".into(),
        session,
    ]);
    Ok(args)
}

/// Build the ssh argv for a control-mode (-CC) attach, inserting -CC after the tmux binary and
/// -D on new-session (mirrors buildControlModeSshArgs in the JS backend).
pub fn build_control_mode_ssh_args(
    raw_host: &str,
    raw_session: &str,
    base_args: &[String],
    tmux_path: &str,
    socket: &str,
) -> Result<Vec<String>> {
    let mut args = build_ssh_args(raw_host, raw_session, base_args, tmux_path, socket)?;
    // args = [... "--", host, tmux, "-L", sock, "new-session", "-A", "-s", name]
    if let Some(dd) = args.iter().position(|a| a == "--") {
        // dd+1 host, dd+2 tmux binary -> insert -CC after the binary
        args.insert(dd + 3, "-CC".into());
    }
    if let Some(ns) = args.iter().position(|a| a == "new-session") {
        args.insert(ns + 1, "-D".into());
    }
    Ok(args)
}

/// Build ssh argv to KILL a remote tmux session (mirrors buildKillArgs): base64-wrapped so the
/// host login shell (often zsh) can't mangle quoting.
pub fn build_kill_args(
    raw_host: &str,
    raw_session: &str,
    tmux_path: &str,
    socket: &str,
    base_args: &[String],
) -> Result<Vec<String>> {
    let session = validate_session(raw_session)?;
    let parts = parse_host(raw_host)?;
    if !matches_tmux_path_kill(tmux_path) {
        return Err(ValidationError::new("tmuxPath", "invalid path"));
    }
    if !matches_socket(socket) {
        return Err(ValidationError::new("socket", "invalid socket name"));
    }
    let target = host_token(&parts);
    let script = format!("{} -L {} kill-session -t {}", tmux_path, socket, session);
    let b64 = base64_encode(script.as_bytes());
    let remote = format!("echo {} | base64 -d | /bin/sh", b64);

    let mut args: Vec<String> = Vec::new();
    if let Some(p) = parts.port {
        args.push("-p".into());
        args.push(p.to_string());
    }
    args.extend([
        "-o".into(), "BatchMode=yes".into(),
        "-o".into(), "ConnectTimeout=8".into(),
    ]);
    args.extend(base_args.iter().cloned());
    args.extend(["--".into(), target, remote]);
    Ok(args)
}

/// Minimal standard base64 encoder (avoids pulling a crate for one call).
pub fn base64_encode(input: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(T[((n >> 18) & 63) as usize] as char);
        out.push(T[((n >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 { T[((n >> 6) & 63) as usize] as char } else { '=' });
        out.push(if chunk.len() > 2 { T[(n & 63) as usize] as char } else { '=' });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tc_v_session_charset() {
        assert!(validate_session("dev").is_ok());
        assert!(validate_session("dt-main_1").is_ok());
        assert!(validate_session("-x").is_err(), "leading dash rejected (flag-injection)");
        assert!(validate_session("a;b").is_err(), "metachars rejected");
        assert!(validate_session("a.b").is_err(), "dot rejected (tmux target syntax)");
        assert!(validate_session("").is_err());
    }

    #[test]
    fn tc_v_parse_host() {
        let p = parse_host("me@host.example:2222").unwrap();
        assert_eq!(p.user.as_deref(), Some("me"));
        assert_eq!(p.host, "host.example");
        assert_eq!(p.port, Some(2222));
        assert!(!p.is_ipv6);
        assert!(parse_host("-h").is_err(), "leading-dash host rejected");
        assert!(parse_host("a;b").is_err(), "metachar host rejected");
    }

    #[test]
    fn tc_v_ipv6() {
        let p = parse_host("[::1]:22").unwrap();
        assert!(p.is_ipv6);
        assert_eq!(p.host, "::1");
        assert_eq!(p.port, Some(22));
        let bare = parse_host("fe80::1").unwrap();
        assert!(bare.is_ipv6);
        assert_eq!(bare.port, None);
    }

    #[test]
    fn tc_v_build_ssh_args() {
        let a = build_ssh_args("me@h", "dev", &[], ".local/bin/tmux", "dtapp3-7").unwrap();
        let dd = a.iter().position(|x| x == "--").unwrap();
        assert_eq!(&a[dd + 1..], &[
            "me@h", ".local/bin/tmux", "-L", "dtapp3-7", "new-session", "-A", "-s", "dev"
        ]);
        assert!(build_ssh_args("me@h", "a;b", &[], "tmux", "s").is_err());
    }

    #[test]
    fn tc_v_build_control_mode_args() {
        let a = build_control_mode_ssh_args("me@h", "dev", &[], "/t", "dtcc3-7").unwrap();
        let dd = a.iter().position(|x| x == "--").unwrap();
        // -CC right after the tmux binary
        assert_eq!(a[dd + 2], "/t");
        assert_eq!(a[dd + 3], "-CC");
        // new-session -D -A -s dev
        let tail: Vec<&str> = a.iter().rev().take(5).rev().map(|s| s.as_str()).collect();
        assert_eq!(tail, ["new-session", "-D", "-A", "-s", "dev"]);
    }

    #[test]
    fn tc_v_kill_args_base64() {
        let a = build_kill_args("me@h:22", "dt-x", "/home/u/.local/bin/tmux", "dtcc3", &[]).unwrap();
        let remote = a.last().unwrap();
        assert!(remote.starts_with("echo "));
        assert!(remote.ends_with("| base64 -d | /bin/sh"));
    }

    #[test]
    fn tc_v_base64() {
        assert_eq!(base64_encode(b"hello"), "aGVsbG8=");
        assert_eq!(base64_encode(b"tmux -L s"), "dG11eCAtTCBz");
    }
}
