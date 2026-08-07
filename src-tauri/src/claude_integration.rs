//! Buoy-scoped Claude Code notification integration.
//!
//! Claude Code's default `auto` notification channel only emits terminal notifications for a
//! small terminal allowlist. Buoy deliberately does not pretend to be one of those terminals and
//! does not mutate the user's global Claude settings. Instead, shells launched by Buoy see a small
//! `claude` shim first on PATH. The shim delegates to the real binary with an app-owned plugin
//! whose lifecycle hooks emit a generic OSC 777 request to the Claude process's controlling tty.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

const CLAUDE_PLUGIN_DIR_NAME: &str = "buoy-claude-notifications";

const CLAUDE_PLUGIN_MANIFEST: &str = r#"{
  "name": "buoy-notifications",
  "version": "1.0.0",
  "description": "Generic terminal attention notifications for Buoy",
  "author": {
    "name": "Buoy"
  }
}
"#;

const CLAUDE_PLUGIN_HOOKS: &str = r#"{
  "description": "Notify Buoy when Claude needs attention or finishes a response",
  "hooks": {
    "Notification": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "sh \"${CLAUDE_PLUGIN_ROOT}/scripts/notify.sh\"",
            "timeout": 5
          }
        ]
      }
    ],
    "Stop": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "sh \"${CLAUDE_PLUGIN_ROOT}/scripts/notify.sh\"",
            "timeout": 5
          }
        ]
      }
    ]
  }
}
"#;

const CLAUDE_PLUGIN_NOTIFY: &str = r#"#!/bin/sh
# Claude sends hook context on stdin. It is intentionally ignored: Buoy displays one generic dot.
debug() {
  if [ "${DT_DEBUG:-}" = 1 ]; then
    printf 'buoy: Claude notification: %s\n' "$*" >&2
  fi
}

if [ "${BUOY_TERMINAL:-}" != 1 ]; then
  exit 0
fi

tty_target=

# Claude starts hooks in a fresh session with pipe-backed stdio, so /dev/tty cannot be opened even
# though the Claude parent has a controlling terminal. tmux provides the exact tty for this pane.
if [ -n "${TMUX:-}" ] && [ -n "${TMUX_PANE:-}" ] && command -v tmux >/dev/null 2>&1; then
  tty_target=$(tmux display-message -p -t "$TMUX_PANE" '#{pane_tty}' 2>/dev/null) ||
    tty_target=
fi

# Outside tmux, or when the server's tmux binary is not on PATH, find the nearest ancestor that
# still owns a real tty. ps -o is specified by POSIX and works with both BSD and procps variants.
if [ -z "$tty_target" ] || [ ! -w "$tty_target" ]; then
  tty_target=
  pid=$PPID
  n=0
  while [ -n "$pid" ] && [ "$pid" -gt 1 ] 2>/dev/null && [ "$n" -lt 10 ]; do
    tty_name=$(ps -o tty= -p "$pid" 2>/dev/null | tr -d '[:space:]')
    case "$tty_name" in
      ''|'?'|'??') ;;
      *)
        if [ -w "/dev/$tty_name" ]; then
          tty_target=/dev/$tty_name
          break
        fi
        ;;
    esac
    pid=$(ps -o ppid= -p "$pid" 2>/dev/null | tr -d '[:space:]')
    n=$((n + 1))
  done
fi

if [ -z "$tty_target" ]; then
  debug "could not resolve the Claude pane tty"
elif ! printf '\033]777;notify;Buoy;\007' > "$tty_target" 2>/dev/null; then
  debug "could not write to $tty_target"
fi
exit 0
"#;

/// POSIX `sh` wrapper installed as `$HOME/.cache/buoy/bin/claude`.
///
/// Keep the bundle dependency-light: remote installation needs only POSIX shell plus `base64`;
/// hook delivery uses the already-required tmux, with POSIX `ps`/`tr` as its fallback.
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

# Safe/bare modes intentionally disable customizations. Non-interactive and informational
# invocations do not need an attention signal, so leave them equivalent to invoking Claude directly.
for arg in "$@"; do
  case "$arg" in
    --safe-mode|--bare|-p|--print|-h|--help|-v|--version)
      exec "$real_claude" "$@"
      ;;
  esac
done

# The plugin is installed beside this shim. Resolve an absolute directory even if a caller invokes
# the shim through a relative PATH entry.
shim_path=$0
case "$shim_path" in
  */*) ;;
  *) shim_path=$(command -v "$shim_path" 2>/dev/null || printf '%s\n' "$shim_path") ;;
esac
shim_dir=${shim_path%/*}
[ "$shim_dir" != "$shim_path" ] || shim_dir=.
shim_dir=$(CDPATH= cd "$shim_dir" 2>/dev/null && pwd) || shim_dir=.
plugin_dir=$shim_dir/buoy-claude-notifications

# A partial/failed install must never prevent Claude from launching.
if [ ! -f "$plugin_dir/.claude-plugin/plugin.json" ] ||
   [ ! -f "$plugin_dir/hooks/hooks.json" ] ||
   [ ! -f "$plugin_dir/scripts/notify.sh" ]; then
  exec "$real_claude" "$@"
fi

BUOY_CLAUDE_SHIM_ACTIVE=1
export BUOY_CLAUDE_SHIM_ACTIVE
exec "$real_claude" --plugin-dir "$plugin_dir" "$@"
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

/// Install or update the local launcher/plugin bundle atomically per file. Failure is non-fatal to
/// session creation: the caller logs it and the terminal continues without Claude notifications.
pub fn ensure_local_shim() -> io::Result<PathBuf> {
    ensure_local_shim_in(&local_shim_dir()?)
}

fn ensure_local_shim_in(dir: &Path) -> io::Result<PathBuf> {
    fs::create_dir_all(dir)?;
    let plugin = dir.join(CLAUDE_PLUGIN_DIR_NAME);
    install_file(
        &plugin.join(".claude-plugin").join("plugin.json"),
        CLAUDE_PLUGIN_MANIFEST.as_bytes(),
        false,
    )?;
    install_file(
        &plugin.join("hooks").join("hooks.json"),
        CLAUDE_PLUGIN_HOOKS.as_bytes(),
        false,
    )?;
    install_file(
        &plugin.join("scripts").join("notify.sh"),
        CLAUDE_PLUGIN_NOTIFY.as_bytes(),
        true,
    )?;

    // Publish the launcher last so a concurrent shell never observes a new shim before its plugin.
    let launcher = dir.join("claude");
    install_file(&launcher, CLAUDE_WRAPPER.as_bytes(), true)?;
    Ok(launcher)
}

fn install_file(destination: &Path, contents: &[u8], executable: bool) -> io::Result<()> {
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    if fs::read(destination).ok().as_deref() == Some(contents) {
        if executable {
            make_executable(destination)?;
        }
        return Ok(());
    }

    let name = destination
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("bundle");
    let temporary = destination.with_file_name(format!(".{name}.{}.tmp", std::process::id()));
    fs::write(&temporary, contents)?;
    if executable {
        make_executable(&temporary)?;
    }
    if let Err(error) = fs::rename(&temporary, destination) {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    Ok(())
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
    let manifest_b64 = crate::validation::base64_encode(CLAUDE_PLUGIN_MANIFEST.as_bytes());
    let hooks_b64 = crate::validation::base64_encode(CLAUDE_PLUGIN_HOOKS.as_bytes());
    let notify_b64 = crate::validation::base64_encode(CLAUDE_PLUGIN_NOTIFY.as_bytes());
    let cc = if control { " -CC" } else { "" };
    let detach = if control { " -D" } else { "" };
    format!(
        r#"buoy_bin="${{XDG_CACHE_HOME:-$HOME/.cache}}/buoy/bin"
buoy_claude="$buoy_bin/claude"
buoy_plugin="$buoy_bin/{CLAUDE_PLUGIN_DIR_NAME}"
buoy_install() {{
  buoy_data=$1
  buoy_dest=$2
  buoy_mode=$3
  buoy_parent=${{buoy_dest%/*}}
  mkdir -p "$buoy_parent" 2>/dev/null || return
  buoy_tmp="$buoy_dest.$$.tmp"
  if printf '%s' "$buoy_data" | base64 -d > "$buoy_tmp" 2>/dev/null; then
    chmod "$buoy_mode" "$buoy_tmp" 2>/dev/null || true
    if [ -f "$buoy_dest" ] && cmp -s "$buoy_tmp" "$buoy_dest"; then
      rm -f "$buoy_tmp"
      [ "$buoy_mode" = 755 ] && chmod 755 "$buoy_dest" 2>/dev/null || true
    elif ! mv -f "$buoy_tmp" "$buoy_dest" 2>/dev/null; then
      rm -f "$buoy_tmp"
    fi
  else
    rm -f "$buoy_tmp"
  fi
}}
if mkdir -p "$buoy_bin" 2>/dev/null; then
  buoy_install {manifest_b64} "$buoy_plugin/.claude-plugin/plugin.json" 644
  buoy_install {hooks_b64} "$buoy_plugin/hooks/hooks.json" 644
  buoy_install {notify_b64} "$buoy_plugin/scripts/notify.sh" 755
  buoy_install {wrapper_b64} "$buoy_claude" 755
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
        let plugin = bin.join(CLAUDE_PLUGIN_DIR_NAME);
        let manifest = plugin.join(".claude-plugin").join("plugin.json");
        let hooks = plugin.join("hooks").join("hooks.json");
        let notify = plugin.join("scripts").join("notify.sh");
        assert_eq!(fs::read_to_string(&shim).unwrap(), CLAUDE_WRAPPER);
        assert_eq!(
            fs::read_to_string(&manifest).unwrap(),
            CLAUDE_PLUGIN_MANIFEST
        );
        assert_eq!(fs::read_to_string(&hooks).unwrap(), CLAUDE_PLUGIN_HOOKS);
        assert_eq!(
            fs::read_to_string(&notify).unwrap(),
            CLAUDE_PLUGIN_NOTIFY
        );
        serde_json::from_str::<serde_json::Value>(CLAUDE_PLUGIN_MANIFEST).unwrap();
        serde_json::from_str::<serde_json::Value>(CLAUDE_PLUGIN_HOOKS).unwrap();
        ensure_local_shim_in(&bin).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_ne!(fs::metadata(&shim).unwrap().permissions().mode() & 0o111, 0);
            assert_ne!(
                fs::metadata(&notify).unwrap().permissions().mode() & 0o111,
                0
            );
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn tc_cn2_injects_plugin_and_preserves_explicit_arguments() {
        use std::os::unix::fs::PermissionsExt;

        let root = temp_dir("behavior with spaces");
        let shim_dir = root.join("shim");
        let real_dir = root.join("real");
        fs::create_dir_all(&real_dir).unwrap();
        let shim = ensure_local_shim_in(&shim_dir).unwrap();
        let real = real_dir.join("claude");
        fs::write(&real, b"#!/bin/sh\nprintf '<%s>\\n' \"$@\"\n").unwrap();
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

        let injected = run(&[
            "--settings",
            "{\"theme\":\"dark\"}",
            "--plugin-dir",
            "/tmp/user plugin",
            "hello world",
        ]);
        assert!(injected.status.success());
        assert_eq!(
            String::from_utf8(injected.stdout).unwrap(),
            format!(
                "<--plugin-dir>\n<{}>\n<--settings>\n<{{\"theme\":\"dark\"}}>\n<--plugin-dir>\n</tmp/user plugin>\n<hello world>\n",
                shim_dir.join(CLAUDE_PLUGIN_DIR_NAME).display()
            )
        );

        for args in [
            vec!["--safe-mode", "hello"],
            vec!["--bare", "hello"],
            vec!["--print", "hello"],
            vec!["--help"],
            vec!["--version"],
        ] {
            let passthrough = run(&args);
            let expected = args
                .iter()
                .map(|arg| format!("<{arg}>\n"))
                .collect::<String>();
            assert_eq!(String::from_utf8(passthrough.stdout).unwrap(), expected);
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn tc_cn3_remote_script_bootstraps_the_same_launcher_and_tmux_environment() {
        let script = remote_tmux_script(".local/bin/tmux", "dtcc3-7", "dev", true);
        assert!(script.contains("base64 -d > \"$buoy_tmp\""));
        assert!(script.contains("buoy_plugin=\"$buoy_bin/buoy-claude-notifications\""));
        assert!(script.contains("$buoy_plugin/.claude-plugin/plugin.json"));
        assert!(script.contains("$buoy_plugin/hooks/hooks.json"));
        assert!(script.contains("$buoy_plugin/scripts/notify.sh"));
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
        let remote_bin = root.join("cache").join("buoy").join("bin");
        let remote_plugin = remote_bin.join(CLAUDE_PLUGIN_DIR_NAME);
        let installed_bundle = fs::read(remote_bin.join("claude")).ok().as_deref()
            == Some(CLAUDE_WRAPPER.as_bytes())
            && fs::read(remote_plugin.join(".claude-plugin").join("plugin.json"))
                .ok()
                .as_deref()
                == Some(CLAUDE_PLUGIN_MANIFEST.as_bytes())
            && fs::read(remote_plugin.join("hooks").join("hooks.json"))
                .ok()
                .as_deref()
                == Some(CLAUDE_PLUGIN_HOOKS.as_bytes())
            && fs::read(remote_plugin.join("scripts").join("notify.sh"))
                .ok()
                .as_deref()
                == Some(CLAUDE_PLUGIN_NOTIFY.as_bytes());
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
        assert!(installed_bundle, "remote bootstrap did not install the Claude plugin bundle");
    }

    #[cfg(unix)]
    #[test]
    fn tc_cn5_hook_osc_survives_real_tmux_control_mode() {
        use portable_pty::{native_pty_system, CommandBuilder, PtySize};
        use std::io::Read;
        use std::sync::mpsc;
        use std::time::{Duration, Instant};

        let root = temp_dir("hook-pty");
        let bin = root.join("bin");
        ensure_local_shim_in(&bin).unwrap();
        let hook = bin
            .join(CLAUDE_PLUGIN_DIR_NAME)
            .join("scripts")
            .join("notify.sh");

        let probe = crate::probe::probe_local_tmux();
        if !probe.probed {
            eprintln!("SKIP TC-CN5 tmux leg: no local tmux");
            let _ = fs::remove_dir_all(root);
            return;
        }

        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
            % 1_000_000;
        let session = format!("buoycn5{}{}", std::process::id(), nonce);
        let socket = format!("bcn5-{}-{}", std::process::id(), nonce);
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("open tmux hook pty");
        let mut command = CommandBuilder::new(&probe.tmux_path);
        command.args([
            "-CC",
            "-L",
            socket.as_str(),
            "new-session",
            "-D",
            "-s",
            session.as_str(),
            "\"$BUOY_TEST_HOOK\"; sleep 1",
        ]);
        command.env("BUOY_TEST_HOOK", &hook);
        command.env("BUOY_TERMINAL", "1");
        command.env("TERM", "xterm-256color");
        let mut child = pair
            .slave
            .spawn_command(command)
            .expect("spawn hook through tmux control mode");
        drop(pair.slave);
        let mut writer = pair.master.take_writer().expect("tmux pty writer");
        let mut reader = pair.master.try_clone_reader().expect("tmux pty reader");
        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        std::thread::spawn(move || {
            let mut buffer = [0u8; 4096];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(count) => {
                        if tx.send(buffer[..count].to_vec()).is_err() {
                            break;
                        }
                    }
                }
            }
        });

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut tmux_output = Vec::new();
        while Instant::now() < deadline {
            match rx.recv_timeout(Duration::from_millis(250)) {
                Ok(chunk) => tmux_output.extend(chunk),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
            if String::from_utf8_lossy(&tmux_output)
                .contains("\\033]777;notify;Buoy;\\007")
            {
                break;
            }
        }
        let text = String::from_utf8_lossy(&tmux_output).into_owned();
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

        assert!(
            text.contains("%output") && text.contains("\\033]777;notify;Buoy;\\007"),
            "tmux control mode did not preserve the hook OSC: {text:?}"
        );
    }

    /// Claude launches command hooks in a new session with pipe-backed stdio. Reproduce that
    /// topology instead of giving the hook a controlling pty: it must resolve the inherited tmux
    /// pane explicitly and write the OSC to that pane's tty.
    #[cfg(unix)]
    #[test]
    fn tc_cn6_detached_hook_targets_the_inherited_tmux_pane() {
        use portable_pty::{native_pty_system, CommandBuilder, PtySize};
        use std::io::{Read, Write};
        use std::os::unix::fs::symlink;
        use std::os::unix::process::CommandExt;
        use std::sync::mpsc;
        use std::time::{Duration, Instant};

        let probe = crate::probe::probe_local_tmux();
        if !probe.probed {
            eprintln!("SKIP TC-CN6: no local tmux");
            return;
        }

        let root = temp_dir("detached-hook");
        let bin = root.join("bin");
        ensure_local_shim_in(&bin).unwrap();
        let hook = bin
            .join(CLAUDE_PLUGIN_DIR_NAME)
            .join("scripts")
            .join("notify.sh");
        // The production hook intentionally invokes bare `tmux`. Pin that name to the exact binary
        // selected by Buoy's local probe so the test does not depend on the test runner's PATH.
        symlink(&probe.tmux_path, bin.join("tmux")).expect("link the probed tmux into hook PATH");

        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
            % 1_000_000;
        let session = format!("buoycn6{}{}", std::process::id(), nonce);
        let socket = format!("bcn6-{}-{}", std::process::id(), nonce);
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("open detached-hook tmux pty");
        let mut tmux_command = CommandBuilder::new(&probe.tmux_path);
        tmux_command.args([
            "-CC",
            "-L",
            socket.as_str(),
            "new-session",
            "-D",
            "-s",
            session.as_str(),
            "sleep 10",
        ]);
        tmux_command.env("TERM", "xterm-256color");
        let mut tmux_child = pair
            .slave
            .spawn_command(tmux_command)
            .expect("spawn control client for detached hook");
        drop(pair.slave);
        let mut writer = pair.master.take_writer().expect("detached-hook pty writer");
        let mut reader = pair
            .master
            .try_clone_reader()
            .expect("detached-hook pty reader");
        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        std::thread::spawn(move || {
            let mut buffer = [0u8; 4096];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(count) => {
                        if tx.send(buffer[..count].to_vec()).is_err() {
                            break;
                        }
                    }
                }
            }
        });

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut control_output = Vec::new();
        while Instant::now() < deadline {
            match rx.recv_timeout(Duration::from_millis(250)) {
                Ok(chunk) => control_output.extend(chunk),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
            if String::from_utf8_lossy(&control_output).contains("%session-changed") {
                break;
            }
        }

        let target = format!("{session}:");
        let pane_metadata = Command::new(&probe.tmux_path)
            .args([
                "-L",
                &socket,
                "display-message",
                "-p",
                "-t",
                &target,
                "#{pane_id}\t#{socket_path},#{pid},0\t#{pane_tty}",
            ])
            .output()
            .expect("query the scratch tmux pane");
        assert!(
            pane_metadata.status.success(),
            "could not query scratch pane: {}",
            String::from_utf8_lossy(&pane_metadata.stderr)
        );
        let metadata = String::from_utf8_lossy(&pane_metadata.stdout);
        let fields = metadata.trim().split('\t').collect::<Vec<_>>();
        assert_eq!(fields.len(), 3, "unexpected pane metadata: {metadata:?}");
        let pane_id = fields[0];
        let tmux_env = fields[1];
        let pane_tty = fields[2];
        assert!(pane_tty.starts_with("/dev/"), "real pane tty: {pane_tty:?}");

        let inherited_path = std::env::var("PATH").unwrap_or_default();
        let mut hook_command = Command::new("/bin/sh");
        hook_command
            .arg(&hook)
            .env("BUOY_TERMINAL", "1")
            .env("DT_DEBUG", "1")
            .env("TMUX", tmux_env)
            .env("TMUX_PANE", pane_id)
            .env("PATH", format!("{}:{inherited_path}", bin.display()));
        // SAFETY: pre_exec runs only async-signal-safe setsid in the freshly forked child. This is
        // the essential regression condition: /dev/tty must be unavailable exactly as it is for a
        // real Claude hook subprocess.
        unsafe {
            hook_command.pre_exec(|| {
                if libc::setsid() == -1 {
                    Err(std::io::Error::last_os_error())
                } else {
                    Ok(())
                }
            });
        }
        let hook_output = hook_command.output().expect("run detached hook");
        assert!(
            hook_output.status.success(),
            "detached hook failed: {}",
            String::from_utf8_lossy(&hook_output.stderr)
        );

        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            match rx.recv_timeout(Duration::from_millis(250)) {
                Ok(chunk) => control_output.extend(chunk),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
            if String::from_utf8_lossy(&control_output)
                .contains("\\033]777;notify;Buoy;\\007")
            {
                break;
            }
        }
        let text = String::from_utf8_lossy(&control_output).into_owned();
        let _ = writer.write_all(b"detach-client\n");
        let _ = writer.flush();
        std::thread::sleep(Duration::from_millis(100));
        let _ = tmux_child.kill();
        let _ = Command::new(&probe.tmux_path)
            .args(["-L", &socket, "kill-server"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        let _ = fs::remove_dir_all(root);

        assert!(
            text.contains("%output") && text.contains("\\033]777;notify;Buoy;\\007"),
            "detached hook did not reach pane {pane_id} on {pane_tty}; output={text:?}, stderr={:?}",
            String::from_utf8_lossy(&hook_output.stderr)
        );
    }
}
