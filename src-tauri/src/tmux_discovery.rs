//! Discover sessions on a user's ordinary (`-L default`) tmux server and rebuild a saved window
//! recipe when a server disappeared after a host reboot. All remote scripts are base64-wrapped;
//! renderer values never become shell syntax.

use std::process::{Command, Output};

use serde::Serialize;

use crate::probe::{self, ProbeResult};
use crate::session_store::RecoveryWindow;
use crate::transport::Transport;
use crate::validation::{self, base64_encode, parse_host, validate_session};

const DEFAULT_SOCKET: &str = "default";
const LIST_FORMAT: &str =
    "#{session_name}\t#{session_windows}\t#{session_attached}\t#{session_created}";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscoveredSession {
    pub name: String,
    pub windows: u32,
    pub attached: u32,
    pub created: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscoveryResult {
    pub tmux_path: String,
    pub tmux_version: Option<(u32, u32)>,
    pub sessions: Vec<DiscoveredSession>,
}

fn ssh_target(raw_host: &str) -> Result<(Vec<String>, String), String> {
    let parts = parse_host(raw_host).map_err(|e| e.to_string())?;
    let mut args = Vec::new();
    if let Some(port) = parts.port {
        args.extend(["-p".into(), port.to_string()]);
    }
    args.extend([
        "-o".into(),
        "BatchMode=yes".into(),
        "-o".into(),
        "ConnectTimeout=8".into(),
        "--".into(),
    ]);
    let target = match parts.user {
        Some(user) => format!("{user}@{}", parts.host),
        None => parts.host,
    };
    Ok((args, target))
}

fn remote_output(raw_host: &str, script: &str) -> Result<Output, String> {
    let (mut args, target) = ssh_target(raw_host)?;
    args.push(target);
    args.push(format!(
        "echo {} | base64 -d | /bin/sh",
        base64_encode(script.as_bytes())
    ));
    Command::new("ssh")
        .args(&args)
        .env("PATH", crate::augmented_path())
        .output()
        .map_err(|e| format!("could not run ssh: {e}"))
}

fn local_output(tmux_path: &str, args: &[&str]) -> Result<Output, String> {
    Command::new(tmux_path)
        .args(args)
        .env("PATH", crate::augmented_path())
        .output()
        .map_err(|e| format!("could not run tmux: {e}"))
}

fn parse_sessions(stdout: &[u8]) -> Vec<DiscoveredSession> {
    let text = String::from_utf8_lossy(stdout);
    let mut sessions = Vec::new();
    for line in text.lines() {
        let mut fields = line.splitn(4, '\t');
        let Some(name) = fields.next() else { continue };
        // Imported names flow through the same remote bootstrap as created names. Keep only the
        // narrow, injection-safe subset Buoy already supports and ignore exotic tmux names.
        if validate_session(name).is_err() {
            continue;
        }
        let Some(windows) = fields.next().and_then(|v| v.parse().ok()) else {
            continue;
        };
        let Some(attached) = fields.next().and_then(|v| v.parse().ok()) else {
            continue;
        };
        let Some(created) = fields.next().and_then(|v| v.parse().ok()) else {
            continue;
        };
        sessions.push(DiscoveredSession {
            name: name.into(),
            windows,
            attached,
            created,
        });
    }
    sessions.sort_by(|a, b| b.created.cmp(&a.created).then_with(|| a.name.cmp(&b.name)));
    sessions
}

pub fn discover(transport: Transport, host: &str) -> Result<DiscoveryResult, String> {
    let ProbeResult {
        tmux_path,
        version,
        probed,
    } = match transport {
        Transport::Local => probe::probe_local_tmux(),
        Transport::Ssh => probe::probe_tmux(host, &[]),
    };
    if !probed {
        return Err("tmux was not found on this host".into());
    }

    let output = match transport {
        Transport::Local => local_output(
            &tmux_path,
            &["-L", DEFAULT_SOCKET, "list-sessions", "-F", LIST_FORMAT],
        )?,
        Transport::Ssh => {
            let script = format!(
                "exec {tmux_path} -L {DEFAULT_SOCKET} list-sessions -F '{}'",
                LIST_FORMAT,
            );
            remote_output(host, &script)?
        }
    };
    // tmux exits 1 when no server exists. That is a valid empty discovery result; ssh exits 255
    // for connection/auth failures, which should be visible instead of masquerading as no sessions.
    if !output.status.success() && output.status.code() != Some(1) {
        let error = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if error.is_empty() {
            "could not list tmux sessions".into()
        } else {
            error
        });
    }
    Ok(DiscoveryResult {
        tmux_path,
        tmux_version: version,
        sessions: parse_sessions(&output.stdout),
    })
}

fn encode_value(value: &str) -> String {
    base64_encode(value.as_bytes())
}

fn is_shell(command: &str) -> bool {
    matches!(
        command.rsplit('/').next().unwrap_or(command),
        "sh" | "bash" | "zsh" | "fish" | "dash" | "ksh" | "tcsh" | "nu"
    )
}

fn recovery_label(window: &RecoveryWindow) -> String {
    if !window.command.is_empty() && !is_shell(&window.command) {
        window.command.clone()
    } else if !window.name.is_empty() {
        window.name.clone()
    } else if !window.command.is_empty() {
        window.command.clone()
    } else {
        "shell".into()
    }
}

/// Build a shell program whose only interpolated tokens have already passed narrow validation.
/// Paths/titles travel as base64 data and are decoded into quoted argv values.
pub fn recovery_shell_block(
    tmux_path: &str,
    socket: &str,
    session: &str,
    windows: &[RecoveryWindow],
) -> Result<String, String> {
    if !validation::is_safe_tmux_path(tmux_path) {
        return Err("invalid tmux path".into());
    }
    validate_session(session).map_err(|e| e.to_string())?;
    if socket.is_empty()
        || !socket
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
    {
        return Err("invalid tmux socket".into());
    }
    if windows.is_empty() {
        return Err("empty recovery recipe".into());
    }

    let mut script =
        format!("if ! {tmux_path} -L {socket} has-session -t {session} 2>/dev/null; then\n");
    for (index, window) in windows.iter().enumerate() {
        let cwd = encode_value(&window.cwd);
        let label = encode_value(&recovery_label(window));
        script.push_str(&format!(
            "  buoy_cwd=$(printf '%s' {cwd} | base64 -d) || exit 20\n\
             buoy_name=$(printf '%s' {label} | base64 -d) || exit 21\n\
             [ -d \"$buoy_cwd\" ] && [ -x \"$buoy_cwd\" ] || buoy_cwd=${{HOME:-/}}\n"
        ));
        if index == 0 {
            script.push_str(&format!(
                "  buoy_win=$({tmux_path} -L {socket} new-session -d -P -F '#{{window_id}}' -s {session} -c \"$buoy_cwd\" -n \"$buoy_name\") || exit 22\n"
            ));
        } else {
            script.push_str(&format!(
                "  buoy_win=$({tmux_path} -L {socket} new-window -d -P -F '#{{window_id}}' -t {session} -c \"$buoy_cwd\" -n \"$buoy_name\") || exit 23\n"
            ));
        }
        if window.active {
            script.push_str("  buoy_active=$buoy_win\n");
        }
    }
    script.push_str(&format!(
        "  [ -n \"$buoy_active\" ] || buoy_active=$({tmux_path} -L {socket} list-windows -t {session} -F '#{{window_id}}' | head -n 1)\n\
           {tmux_path} -L {socket} select-window -t \"$buoy_active\" || exit 24\n\
           printf '%s\\n' BUOY_RESTORED\n\
         fi\n"
    ));
    Ok(script)
}

/// Recreate missing windows before the normal attach. Returns true only when a recipe was applied.
/// An existing tmux session is never changed.
pub fn restore_if_missing(
    transport: Transport,
    host: &str,
    tmux_path: &str,
    socket: &str,
    session: &str,
    windows: &[RecoveryWindow],
) -> Result<bool, String> {
    if windows.is_empty() {
        return Ok(false);
    }
    let script = recovery_shell_block(tmux_path, socket, session, windows)?;
    let output = match transport {
        Transport::Local => Command::new("/bin/sh")
            .args(["-c", &script])
            .env("PATH", crate::augmented_path())
            .output()
            .map_err(|e| format!("could not run recovery: {e}"))?,
        Transport::Ssh => remote_output(host, &script)?,
    };
    if !output.status.success() {
        let error = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if error.is_empty() {
            "tmux recovery failed".into()
        } else {
            error
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .any(|line| line == "BUOY_RESTORED"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_sorts_discovery_rows() {
        let rows = parse_sessions(b"old\t1\t0\t10\nnew\t3\t2\t20\nbad.name\t1\t0\t30\n");
        assert_eq!(
            rows.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            vec!["new", "old"]
        );
        assert_eq!(rows[0].windows, 3);
        assert_eq!(rows[0].attached, 2);
    }

    #[test]
    fn recovery_script_uses_data_not_shell_syntax() {
        let windows = vec![
            RecoveryWindow {
                name: "shell".into(),
                cwd: "/tmp/a b'$HOME".into(),
                command: "codex".into(),
                active: false,
            },
            RecoveryWindow {
                name: "logs".into(),
                cwd: "/var/log".into(),
                command: "zsh".into(),
                active: true,
            },
        ];
        let script = recovery_shell_block("/usr/bin/tmux", "default", "work", &windows).unwrap();
        assert!(
            !script.contains("/tmp/a b"),
            "cwd is encoded, never shell syntax"
        );
        assert!(
            script.contains(&encode_value("codex")),
            "foreground command becomes the label"
        );
        assert!(script.contains("buoy_active=$buoy_win"));
        assert!(script.contains("select-window -t \"$buoy_active\""));
        assert!(
            script.contains("has-session"),
            "existing sessions are untouched"
        );
    }

    #[test]
    fn local_recovery_rebuilds_tabs_once_and_preserves_cwds() {
        let probe = probe::probe_local_tmux();
        if !probe.probed {
            return;
        }
        let socket = format!("buoy-recovery-test-{}", std::process::id());
        let session = "buoy_recovery_test";
        let _ = Command::new(&probe.tmux_path)
            .args(["-L", &socket, "kill-server"])
            .output();
        let windows = vec![
            RecoveryWindow {
                name: "one".into(),
                cwd: "/tmp".into(),
                command: "codex".into(),
                active: false,
            },
            RecoveryWindow {
                name: "two".into(),
                cwd: "/var".into(),
                command: "zsh".into(),
                active: true,
            },
        ];
        assert!(
            restore_if_missing(
                Transport::Local,
                "",
                &probe.tmux_path,
                &socket,
                session,
                &windows,
            )
            .unwrap(),
            "a missing server is rebuilt"
        );
        assert!(
            !restore_if_missing(
                Transport::Local,
                "",
                &probe.tmux_path,
                &socket,
                session,
                &windows,
            )
            .unwrap(),
            "a live session is never modified a second time"
        );

        let output = Command::new(&probe.tmux_path)
            .args([
                "-L",
                &socket,
                "list-windows",
                "-t",
                session,
                "-F",
                "#{window_name}\t#{pane_current_path}\t#{window_active}",
            ])
            .output()
            .unwrap();
        let listing = String::from_utf8_lossy(&output.stdout);
        assert_eq!(
            listing.lines().count(),
            2,
            "one restored tab per saved window: {listing}"
        );
        assert!(
            listing
                .lines()
                .any(|line| line.starts_with("codex\t") && line.ends_with("/tmp\t0")),
            "non-shell command is displayed as the recovered tab name: {listing}"
        );
        assert!(
            listing
                .lines()
                .any(|line| line.starts_with("two\t") && line.ends_with("/var\t1")),
            "shell tab keeps its prior name/cwd and active state: {listing}"
        );

        let _ = Command::new(&probe.tmux_path)
            .args(["-L", &socket, "kill-server"])
            .output();
    }
}
