//! Fetch a remote file's bytes over a SEPARATE ssh exec (DESIGN.md §16). This never touches the
//! session's -CC control channel (that's a command protocol; `cat`-ing a file into it would
//! corrupt it) — it's a fresh non-interactive ssh, like probe.rs.
//!
//! Injection-safety: the clicked PATH is untrusted terminal text, so we base64-encode it and have
//! the remote decode it into a shell var — the path never appears as unescaped shell tokens. The
//! CONTENT is base64-encoded on the wire so binary and UTF-8 files both survive. We fetch at most
//! `cap + 1` bytes (`head -c`) so we can report truncation without pulling a whole huge file.

use std::process::Command;

use crate::validation::{base64_decode, base64_encode, parse_host, HostParts, ValidationError};

/// Result of a remote read: the raw bytes and whether the file was larger than the cap.
pub struct RemoteFile {
    pub data: Vec<u8>,
    pub truncated: bool,
}

/// tmux context used to resolve a RELATIVE clicked path against the session's active-pane cwd
/// (§17). Absolute (`/…`) and `~/…` paths ignore this. Empty socket/session => no cwd resolution
/// (a relative path is then read as-is, relative to the ssh login dir).
#[derive(Clone, Default)]
pub struct TmuxCtx {
    pub tmux_path: String,
    pub socket: String,
    pub session: String,
}

/// Build the sh snippet that resolves $p: absolute stays; `~/…` -> $HOME/…; otherwise join against
/// the session's active-pane cwd (queried from tmux). The kernel resolves any `.`/`..` at open
/// time, so no path normalization is needed here. tmux_path/socket/session are charset-validated
/// upstream (session name in the store, tmux path/socket by socketName), so they're safe to inline.
fn resolve_snippet(ctx: &TmuxCtx) -> String {
    // NB: the `~` in the `${p#\~/}` pattern MUST be backslash-escaped — in POSIX sh an unescaped
    // leading `~` in a #-pattern is taken literally and won't strip (verified: `${p#~/}` left the
    // `~/` in place, producing $HOME/~/file). `${p#\~/}` strips correctly.
    if ctx.socket.is_empty() || ctx.session.is_empty() {
        // No tmux context: expand ~ only; leave relative as-is.
        return String::from(
            "case \"$p\" in /*) : ;; \"~\") p=\"$HOME\" ;; \"~/\"*) p=\"$HOME/${p#\\~/}\" ;; esac; ");
    }
    let tmux = if ctx.tmux_path.is_empty() { "tmux" } else { ctx.tmux_path.as_str() };
    format!(
        "case \"$p\" in \
           /*) : ;; \
           \"~\") p=\"$HOME\" ;; \
           \"~/\"*) p=\"$HOME/${{p#\\~/}}\" ;; \
           *) cwd=$({tmux} -L {sock} display-message -p -t {sess} '#{{pane_current_path}}' 2>/dev/null); \
              [ -n \"$cwd\" ] && p=\"$cwd/$p\" ;; \
         esac; ",
        tmux = tmux, sock = ctx.socket, sess = ctx.session,
    )
}

/// Fetch up to `cap` bytes of `path` from `host` over ssh, resolving a relative `path` against the
/// session's active-pane cwd via `ctx` (§17). `base_args` are extra ssh options. Returns an error
/// string suitable for surfacing to the UI.
pub fn read_remote_file(host: &str, path: &str, cap: usize, ctx: &TmuxCtx, base_args: &[String])
    -> Result<RemoteFile, String>
{
    let parts = parse_host(host).map_err(|e: ValidationError| e.to_string())?;
    let target = host_token(&parts);

    // Remote script: decode the path from base64 into $p, resolve relative -> cwd, verify it's a
    // regular readable file, then stream at most cap+1 bytes as base64. `--` guards head against a
    // path that starts '-'.
    let b64path = base64_encode(path.as_bytes());
    let fetch_n = cap + 1; // one extra byte distinguishes "exactly cap" from "more than cap"
    let script = format!(
        "p=$(echo {b64path} | base64 -d); {resolve}\
         if [ ! -f \"$p\" ]; then echo DT_NOT_A_FILE >&2; exit 3; fi; \
         head -c {fetch_n} -- \"$p\" | base64",
        resolve = resolve_snippet(ctx),
    );
    let b64script = base64_encode(script.as_bytes());
    let remote = format!("echo {b64script} | base64 -d | /bin/sh");

    let mut args: Vec<String> = Vec::new();
    if let Some(p) = parts.port { args.push("-p".into()); args.push(p.to_string()); }
    args.extend([
        "-o".into(), "BatchMode=yes".into(),
        "-o".into(), "ConnectTimeout=8".into(),
    ]);
    args.extend(base_args.iter().cloned());
    args.extend(["--".into(), target, remote]);

    let out = Command::new("ssh")
        .args(&args)
        .env("PATH", crate::augmented_path())
        .output()
        .map_err(|e| format!("ssh failed to start: {}", e))?;

    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        if stderr.contains("DT_NOT_A_FILE") {
            return Err("not a regular file (or not readable)".into());
        }
        let msg = stderr.trim();
        return Err(if msg.is_empty() { "remote read failed".into() } else { msg.to_string() });
    }

    let b64 = String::from_utf8_lossy(&out.stdout);
    let bytes = base64_decode(&b64).ok_or_else(|| "bad base64 from remote".to_string())?;

    // We asked for cap+1: if we got more than cap, the file is larger than the cap -> truncate.
    if bytes.len() > cap {
        Ok(RemoteFile { data: bytes[..cap].to_vec(), truncated: true })
    } else {
        Ok(RemoteFile { data: bytes, truncated: false })
    }
}

/// Read a LOCAL file (for local shell sessions). Same cap/truncation contract.
pub fn read_local_file(path: &str, cap: usize) -> Result<RemoteFile, String> {
    use std::io::Read;
    let meta = std::fs::metadata(path).map_err(|e| e.to_string())?;
    if !meta.is_file() {
        return Err("not a regular file".into());
    }
    let mut f = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut buf = vec![0u8; cap + 1];
    let n = f.read(&mut buf).map_err(|e| e.to_string())?;
    buf.truncate(n);
    if buf.len() > cap {
        Ok(RemoteFile { data: buf[..cap].to_vec(), truncated: true })
    } else {
        Ok(RemoteFile { data: buf, truncated: false })
    }
}

fn host_token(parts: &HostParts) -> String {
    match &parts.user {
        Some(u) => format!("{}@{}", u, parts.host),
        None => parts.host.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_local_file_and_truncation() {
        let dir = std::env::temp_dir();
        let path = dir.join("dt_rf_test.txt");
        std::fs::write(&path, b"hello world").unwrap();
        let p = path.to_string_lossy().to_string();

        let full = read_local_file(&p, 100).unwrap();
        assert_eq!(full.data, b"hello world");
        assert!(!full.truncated);

        let cut = read_local_file(&p, 5).unwrap();
        assert_eq!(cut.data, b"hello");
        assert!(cut.truncated, "reading 5 of 11 bytes must report truncated");

        // exactly-cap is NOT truncated (we fetch cap+1 and only flag when > cap)
        let exact = read_local_file(&p, 11).unwrap();
        assert_eq!(exact.data, b"hello world");
        assert!(!exact.truncated);

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn read_local_file_rejects_missing_and_dir() {
        assert!(read_local_file("/no/such/file/xyz", 100).is_err());
        assert!(read_local_file(&std::env::temp_dir().to_string_lossy(), 100).is_err());
    }

    #[test]
    fn resolve_snippet_handles_abs_home_relative() {
        let ctx = TmuxCtx { tmux_path: "/t".into(), socket: "dtcc3-7".into(), session: "s".into() };
        let s = resolve_snippet(&ctx);
        // absolute is left alone; ~/ expands to $HOME; relative queries the pane cwd and joins.
        assert!(s.contains("/*) : ;;"), "absolute untouched");
        assert!(s.contains("$HOME/${p#\\~/}"), "~/ expands to $HOME (with escaped ~ in the pattern)");
        assert!(s.contains("pane_current_path"), "relative joins the queried cwd");
        assert!(s.contains("-t s"), "queries the right session");
        assert!(s.contains("/t -L dtcc3-7"), "uses the session's tmux + socket");
    }

    #[test]
    fn resolve_snippet_without_tmux_only_expands_home() {
        let s = resolve_snippet(&TmuxCtx::default());
        assert!(!s.contains("pane_current_path"), "no cwd query without a session");
        assert!(s.contains("$HOME"), "still expands ~");
    }
}
