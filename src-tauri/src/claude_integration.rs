//! Buoy-scoped Claude Code notification integration.
//!
//! Claude Code's default `auto` notification channel only emits terminal notifications for a
//! small terminal allowlist. Buoy deliberately does not pretend to be one of those terminals and
//! does not mutate the user's global Claude settings. Instead, shells launched by Buoy see a small
//! `claude` shim first on PATH. The shim delegates to the real binary and supplies Claude's
//! Ghostty-compatible OSC 777 channel only when the user has not already made an explicit choice.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// POSIX `sh` wrapper installed as `$HOME/.cache/buoy/bin/claude`.
///
/// Keep this dependency-free: the same bytes are installed on remote hosts during the ssh attach,
/// where only a POSIX shell, `grep`, and the already-required `base64` utility are assumed.
pub const CLAUDE_WRAPPER: &str = r#"#!/bin/sh
set -uf

real_claude=
old_ifs=$IFS
IFS=:
for dir in ${PATH:-}; do
  [ -n "$dir" ] || dir=.
  candidate=$dir/claude
  [ -x "$candidate" ] || continue
  [ "$candidate" -ef "$0" ] 2>/dev/null && continue
  real_claude=$candidate
  break
done
IFS=$old_ifs

if [ -z "$real_claude" ]; then
  echo "buoy: could not find the real claude executable on PATH" >&2
  exit 127
fi

# Never change behavior outside Buoy, on a nested shim invocation, or after an explicit opt-out.
if [ "${BUOY_TERMINAL:-}" != 1 ] || [ "${BUOY_CLAUDE_SHIM_ACTIVE:-}" = 1 ] || \
   [ "${BUOY_CLAUDE_NOTIFICATIONS_DISABLED:-}" = 1 ]; then
  exec "$real_claude" "$@"
fi

# CLI settings and safe/bare modes have intentional precedence. Non-interactive invocations do not
# need an attention signal, so leave those byte-for-byte equivalent to invoking Claude directly.
for arg in "$@"; do
  case "$arg" in
    --settings|--settings=*|--safe-mode|--bare|-p|--print|-h|--help|-v|--version)
      exec "$real_claude" "$@"
      ;;
  esac
done

has_notification_preference() {
  if [ -n "${CLAUDE_CONFIG_DIR:-}" ]; then
    if [ -f "$CLAUDE_CONFIG_DIR/settings.json" ] &&
       grep -Eq '"preferredNotifChannel"[[:space:]]*:' "$CLAUDE_CONFIG_DIR/settings.json"; then
      return 0
    fi
  elif [ -n "${HOME:-}" ] && [ -f "$HOME/.claude/settings.json" ] &&
       grep -Eq '"preferredNotifChannel"[[:space:]]*:' "$HOME/.claude/settings.json"; then
    return 0
  fi

  # Claude also reads project settings. Walk from the current directory to the filesystem root so
  # Buoy never silently overrides a repository's explicit notification policy.
  scan_dir=${PWD:-.}
  while :; do
    for settings_file in "$scan_dir/.claude/settings.local.json" "$scan_dir/.claude/settings.json"; do
      if [ -f "$settings_file" ] &&
         grep -Eq '"preferredNotifChannel"[[:space:]]*:' "$settings_file"; then
        return 0
      fi
    done
    [ "$scan_dir" = / ] && break
    parent=${scan_dir%/*}
    [ -n "$parent" ] || parent=/
    [ "$parent" = "$scan_dir" ] && break
    scan_dir=$parent
  done
  return 1
}

if has_notification_preference; then
  exec "$real_claude" "$@"
fi

BUOY_CLAUDE_SHIM_ACTIVE=1
export BUOY_CLAUDE_SHIM_ACTIVE
exec "$real_claude" --settings '{"preferredNotifChannel":"ghostty"}' "$@"
"#;

fn home_dir() -> io::Result<PathBuf> {
    std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME is not set"))
}

pub fn local_shim_dir() -> io::Result<PathBuf> {
    Ok(home_dir()?.join(".cache").join("buoy").join("bin"))
}

/// Put Buoy's app-owned launcher first without duplicating it on reconnect.
pub fn path_with_local_shim(base_path: &str) -> String {
    let Ok(dir) = local_shim_dir() else {
        return base_path.to_string();
    };
    let dir = dir.to_string_lossy();
    if base_path.split(':').any(|entry| entry == dir) {
        base_path.to_string()
    } else if base_path.is_empty() {
        dir.into_owned()
    } else {
        format!("{dir}:{base_path}")
    }
}

/// Install or update the local launcher atomically. Failure is non-fatal to session creation: the
/// caller logs it and the terminal continues without Claude-specific notification negotiation.
pub fn ensure_local_shim() -> io::Result<PathBuf> {
    ensure_local_shim_in(&local_shim_dir()?)
}

fn ensure_local_shim_in(dir: &Path) -> io::Result<PathBuf> {
    fs::create_dir_all(dir)?;
    let destination = dir.join("claude");

    if fs::read(&destination).ok().as_deref() == Some(CLAUDE_WRAPPER.as_bytes()) {
        make_executable(&destination)?;
        return Ok(destination);
    }

    let temporary = dir.join(format!(".claude.{}.tmp", std::process::id()));
    fs::write(&temporary, CLAUDE_WRAPPER.as_bytes())?;
    make_executable(&temporary)?;
    if let Err(error) = fs::rename(&temporary, &destination) {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    Ok(destination)
}

#[cfg(unix)]
fn make_executable(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) -> io::Result<()> {
    Ok(())
}

/// Remote attach script. Its dynamic fields are validated against shell-safe character sets before
/// this function is called. The outer ssh command base64-wraps the complete script and passes the
/// decoded bytes as `/bin/sh -c`'s argument, so tmux inherits ssh's pty stdin rather than a pipe.
pub fn remote_tmux_script(tmux_path: &str, socket: &str, session: &str, control: bool) -> String {
    let wrapper_b64 = crate::validation::base64_encode(CLAUDE_WRAPPER.as_bytes());
    let cc = if control { " -CC" } else { "" };
    let detach = if control { " -D" } else { "" };
    format!(
        r#"buoy_bin="${{XDG_CACHE_HOME:-$HOME/.cache}}/buoy/bin"
buoy_claude="$buoy_bin/claude"
if mkdir -p "$buoy_bin" 2>/dev/null; then
  buoy_tmp="$buoy_bin/.claude.$$"
  if echo {wrapper_b64} | base64 -d > "$buoy_tmp" 2>/dev/null; then
    chmod 755 "$buoy_tmp" 2>/dev/null || true
    if [ -f "$buoy_claude" ] && cmp -s "$buoy_tmp" "$buoy_claude"; then
      rm -f "$buoy_tmp"
    elif ! mv -f "$buoy_tmp" "$buoy_claude" 2>/dev/null; then
      rm -f "$buoy_tmp"
    fi
  else
    rm -f "$buoy_tmp"
  fi
fi
PATH="$buoy_bin:$PATH"
export PATH
BUOY_TERMINAL=1
export BUOY_TERMINAL
LC_ALL=C.UTF-8
export LC_ALL
exec {tmux_path}{cc} -L {socket} new-session{detach} -A -s {session} \; set-option -g focus-events on \; set-environment -g PATH "$PATH" \; set-environment -g BUOY_TERMINAL 1"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(label: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "buoy-claude-{label}-{}-{nonce}",
            std::process::id()
        ))
    }

    #[test]
    fn tc_cn1_installs_an_executable_launcher_idempotently() {
        let root = temp_dir("install");
        let bin = root.join("bin");
        let shim = ensure_local_shim_in(&bin).unwrap();
        assert_eq!(fs::read_to_string(&shim).unwrap(), CLAUDE_WRAPPER);
        ensure_local_shim_in(&bin).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_ne!(fs::metadata(&shim).unwrap().permissions().mode() & 0o111, 0);
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn tc_cn2_injects_osc_777_but_respects_explicit_preferences() {
        use std::os::unix::fs::PermissionsExt;

        let root = temp_dir("behavior");
        let shim_dir = root.join("shim");
        let real_dir = root.join("real");
        let config_dir = root.join("home").join(".claude");
        fs::create_dir_all(&real_dir).unwrap();
        fs::create_dir_all(&config_dir).unwrap();
        let shim = ensure_local_shim_in(&shim_dir).unwrap();
        let real = real_dir.join("claude");
        fs::write(&real, b"#!/bin/sh\nprintf '%s\\n' \"$@\"\n").unwrap();
        fs::set_permissions(&real, fs::Permissions::from_mode(0o755)).unwrap();
        let path = format!(
            "{}:{}:/usr/bin:/bin",
            shim_dir.display(),
            real_dir.display()
        );

        let run = |args: &[&str]| {
            Command::new(&shim)
                .args(args)
                .env("PATH", &path)
                .env("HOME", root.join("home"))
                .env("BUOY_TERMINAL", "1")
                .current_dir(&root)
                .output()
                .unwrap()
        };

        let injected = run(&["hello"]);
        assert!(injected.status.success());
        assert_eq!(
            String::from_utf8(injected.stdout).unwrap(),
            "--settings\n{\"preferredNotifChannel\":\"ghostty\"}\nhello\n"
        );

        fs::write(
            config_dir.join("settings.json"),
            b"{\"preferredNotifChannel\":\"terminal_bell\"}\n",
        )
        .unwrap();
        let explicit = run(&["hello"]);
        assert_eq!(String::from_utf8(explicit.stdout).unwrap(), "hello\n");

        let cli = run(&["--settings", "{}", "hello"]);
        assert_eq!(
            String::from_utf8(cli.stdout).unwrap(),
            "--settings\n{}\nhello\n"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn tc_cn3_remote_script_bootstraps_the_same_launcher_and_tmux_environment() {
        let script = remote_tmux_script(".local/bin/tmux", "dtcc3-7", "dev", true);
        assert!(script.contains("base64 -d > \"$buoy_tmp\""));
        assert!(script.contains("PATH=\"$buoy_bin:$PATH\""));
        assert!(script.contains("BUOY_TERMINAL=1"));
        assert!(script.contains("exec .local/bin/tmux -CC -L dtcc3-7 new-session -D -A -s dev"));
        assert!(script.contains("set-environment -g PATH \"$PATH\""));
        assert!(script.contains("set-environment -g BUOY_TERMINAL 1"));
    }

    /// Run the production SSH command shape through a real pty. A string-only assertion cannot
    /// catch the decoder pipeline stealing stdin from tmux: piping the script into `/bin/sh` emits
    /// `tcgetattr failed` and never reaches the control-mode handshake.
    #[cfg(unix)]
    #[test]
    fn tc_cn4_encoded_remote_bootstrap_keeps_tmux_stdin_on_the_pty() {
        use portable_pty::{native_pty_system, CommandBuilder, PtySize};
        use std::io::{Read, Write};
        use std::sync::mpsc;
        use std::time::{Duration, Instant};

        let probe = crate::probe::probe_local_tmux();
        if !probe.probed {
            eprintln!("SKIP TC-CN4: no local tmux");
            return;
        }

        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos() % 1_000_000;
        let session = format!("buoycn4{}{}", std::process::id(), nonce);
        let socket = format!("bcn4-{}-{}", std::process::id(), nonce);
        let args = crate::validation::build_control_mode_ssh_args(
            "example.invalid", &session, &[], &probe.tmux_path, &socket,
        ).expect("build the production remote attach command");
        let remote = args.last().expect("remote command after host").clone();

        let root = temp_dir("remote-pty");
        fs::create_dir_all(root.join("home")).unwrap();
        let pair = native_pty_system().openpty(PtySize {
            rows: 24, cols: 80, pixel_width: 0, pixel_height: 0,
        }).expect("open test pty");
        let mut cmd = CommandBuilder::new("/bin/sh");
        cmd.args(["-c", remote.as_str()]);
        cmd.env("HOME", root.join("home"));
        cmd.env("XDG_CACHE_HOME", root.join("cache"));
        cmd.env("TERM", "xterm-256color");
        let mut child = pair.slave.spawn_command(cmd).expect("spawn encoded bootstrap");
        drop(pair.slave);
        let mut writer = pair.master.take_writer().expect("pty writer");
        let mut reader = pair.master.try_clone_reader().expect("pty reader");
        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => { if tx.send(buf[..n].to_vec()).is_err() { break; } }
                }
            }
        });

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut output = Vec::new();
        while Instant::now() < deadline {
            match rx.recv_timeout(Duration::from_millis(250)) {
                Ok(chunk) => output.extend(chunk),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
            if String::from_utf8_lossy(&output).contains("%session-changed") { break; }
        }

        let text = String::from_utf8_lossy(&output).into_owned();
        let reached_control_mode = text.contains("%begin") && text.contains("%session-changed");
        let _ = writer.write_all(b"detach-client\n");
        let _ = writer.flush();
        std::thread::sleep(Duration::from_millis(100));
        let _ = child.kill();
        let _ = Command::new(&probe.tmux_path)
            .args(["-L", &socket, "kill-server"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        let _ = fs::remove_dir_all(root);

        assert!(reached_control_mode,
            "encoded bootstrap did not preserve tty stdin; output={text:?}");
    }
}
