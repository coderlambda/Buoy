use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use russh::{client, ChannelMsg};

use crate::model::RecoveryTab;

pub const DOWNLOAD_CAP: usize = 5 * 1024 * 1024;

fn safe_tmux_path(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= 512
        && path.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '/' | '-')
        })
}

struct ExecOutput {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    status: Option<u32>,
}

async fn execute<H: client::Handler>(
    ssh: &client::Handle<H>,
    command: String,
    cap: usize,
) -> Result<ExecOutput, String> {
    let mut channel = ssh
        .channel_open_session()
        .await
        .map_err(|error| format!("open SSH exec channel failed: {error}"))?;
    channel
        .exec(true, command)
        .await
        .map_err(|error| format!("SSH exec failed: {error}"))?;
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut status = None;
    while let Some(message) = channel.wait().await {
        match message {
            ChannelMsg::Data { data } => {
                if stdout.len() + data.len() > cap {
                    return Err("remote command output exceeded its safety limit".into());
                }
                stdout.extend_from_slice(&data);
            }
            ChannelMsg::ExtendedData { data, .. } => {
                if stderr.len() + data.len() <= 64 * 1024 {
                    stderr.extend_from_slice(&data);
                }
            }
            ChannelMsg::ExitStatus { exit_status } => status = Some(exit_status),
            // RFC 4254 allows the exit-status request after EOF. Keep draining until Close/None;
            // otherwise successful OpenSSH execs can look like `status: None` on mobile.
            ChannelMsg::Eof => {}
            ChannelMsg::Close => break,
            _ => {}
        }
    }
    Ok(ExecOutput {
        stdout,
        stderr,
        status,
    })
}

pub async fn probe_tmux<H: client::Handler>(
    ssh: &client::Handle<H>,
) -> Result<(String, Option<Vec<u32>>), String> {
    // SSH exec sessions do not necessarily inherit the interactive login PATH. In particular,
    // macOS Remote Login commonly omits Homebrew even though tmux is installed there.
    let command = "p=$(command -v tmux 2>/dev/null || true); for p in \"$p\" \"$HOME/.local/bin/tmux\" /opt/homebrew/bin/tmux /usr/local/bin/tmux /opt/local/bin/tmux /usr/bin/tmux; do if [ -n \"$p\" ] && [ -x \"$p\" ]; then printf '%s\\n' \"$p\"; \"$p\" -V; exit 0; fi; done; printf 'checked PATH=%s\\n' \"$PATH\" >&2; exit 127";
    let output = execute(ssh, command.into(), 16 * 1024).await?;
    let text = String::from_utf8_lossy(&output.stdout);
    let mut lines = text.lines();
    let path = lines.next().unwrap_or_default().trim().to_string();
    let version = lines.next().and_then(parse_tmux_version);
    // Validated output proves the command ran successfully even if an SSH server omits or sends
    // exit-status after EOF. Both fields are required so banners cannot masquerade as a path.
    if safe_tmux_path(&path) && version.is_some() {
        return Ok((path, version));
    }
    let diagnostic = String::from_utf8_lossy(&output.stderr);
    let diagnostic = diagnostic.trim();
    Err(if diagnostic.is_empty() {
        format!(
            "tmux is required for durable mobile sessions (probe status {:?})",
            output.status
        )
    } else {
        format!(
            "tmux is required for durable mobile sessions ({diagnostic}; status {:?})",
            output.status
        )
    })
}

fn parse_tmux_version(value: &str) -> Option<Vec<u32>> {
    let value = value.trim().strip_prefix("tmux ").unwrap_or(value.trim());
    let mut parts = value.split(['.', '-']);
    let major = parts.next()?.parse().ok()?;
    let minor_text = parts.next().unwrap_or("0");
    let minor = minor_text
        .chars()
        .take_while(|character| character.is_ascii_digit())
        .collect::<String>()
        .parse()
        .unwrap_or(0);
    Some(vec![major, minor])
}

pub async fn read_file<H: client::Handler>(
    ssh: &client::Handle<H>,
    path: &str,
    session: &str,
    socket: &str,
    tmux_path: &str,
) -> Result<(Vec<u8>, bool), String> {
    if !safe_tmux_path(tmux_path) {
        return Err("remote tmux path was invalid".into());
    }
    let encoded_path = STANDARD.encode(path.as_bytes());
    let fetch = DOWNLOAD_CAP + 1;
    let resolve = format!(
        "case \"$p\" in /*) : ;; \"~\") p=\"$HOME\" ;; \"~/\"*) p=\"$HOME/${{p#\\~/}}\" ;; *) cwd=$({tmux_path} -L {socket} display-message -p -t {session} '#{{pane_current_path}}' 2>/dev/null); [ -n \"$cwd\" ] && p=\"$cwd/$p\" ;; esac;"
    );
    let script = format!(
        "p=$(printf %s {encoded_path} | base64 -d); {resolve} if [ ! -f \"$p\" ] || [ ! -r \"$p\" ]; then echo BUOY_NOT_A_FILE >&2; exit 3; fi; head -c {fetch} -- \"$p\" | base64"
    );
    let encoded_script = STANDARD.encode(script.as_bytes());
    let output = execute(
        ssh,
        format!("printf %s {encoded_script} | base64 -d | /bin/sh"),
        (DOWNLOAD_CAP + 1) * 2,
    )
    .await?;
    if output.status != Some(0) && !(output.status.is_none() && !output.stdout.is_empty()) {
        let error = String::from_utf8_lossy(&output.stderr);
        return Err(if error.contains("BUOY_NOT_A_FILE") {
            "not a regular readable file".into()
        } else if error.trim().is_empty() {
            "remote file read failed".into()
        } else {
            error.trim().to_string()
        });
    }
    let encoded = String::from_utf8_lossy(&output.stdout);
    let mut bytes = STANDARD
        .decode(encoded.split_whitespace().collect::<String>())
        .map_err(|error| format!("bad base64 from remote: {error}"))?;
    let truncated = bytes.len() > DOWNLOAD_CAP;
    bytes.truncate(DOWNLOAD_CAP);
    Ok((bytes, truncated))
}

pub async fn kill_tmux_session<H: client::Handler>(
    ssh: &client::Handle<H>,
    session: &str,
    socket: &str,
    tmux_path: &str,
) -> Result<(), String> {
    if !safe_tmux_path(tmux_path) {
        return Err("remote tmux path was invalid".into());
    }
    let output = execute(
        ssh,
        format!("{tmux_path} -L {socket} kill-session -t {session}"),
        16 * 1024,
    )
    .await?;
    if output.status == Some(0) {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn decode_field(value: &str) -> String {
    STANDARD
        .decode(value)
        .ok()
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .unwrap_or_default()
}

pub async fn snapshot_and_kill<H: client::Handler>(
    ssh: &client::Handle<H>,
    session: &str,
    socket: &str,
    tmux_path: &str,
    hints: &[RecoveryTab],
) -> Result<Vec<RecoveryTab>, String> {
    if !safe_tmux_path(tmux_path) {
        return Err("remote tmux path was invalid".into());
    }
    let script = format!(
        "t={}; s={}; n={}; \
         \"$t\" -L \"$s\" has-session -t \"$n\" 2>/dev/null || exit 44; \
         \"$t\" -L \"$s\" list-windows -t \"$n\" -F '#{{window_id}}' | while IFS= read -r w; do \
           title=$(\"$t\" -L \"$s\" display-message -p -t \"$w\" '#{{window_name}}'); \
           cwd=$(\"$t\" -L \"$s\" display-message -p -t \"$w\" '#{{pane_current_path}}'); \
           pid=$(\"$t\" -L \"$s\" display-message -p -t \"$w\" '#{{pane_pid}}'); \
           shell=$(ps -p \"$pid\" -o comm= 2>/dev/null | sed 's/^[[:space:]]*//;s/[[:space:]]*$//'); \
           printf '%s\\t' \"$w\"; \
           printf %s \"$title\" | base64 | tr -d '\\n'; printf '\\t'; \
           printf %s \"$cwd\" | base64 | tr -d '\\n'; printf '\\t'; \
           printf %s \"$shell\" | base64 | tr -d '\\n'; printf '\\n'; \
         done; \
         \"$t\" -L \"$s\" kill-session -t \"$n\"",
        shell_quote(tmux_path), shell_quote(socket), shell_quote(session),
    );
    let encoded = STANDARD.encode(script.as_bytes());
    let output = execute(
        ssh,
        format!("printf %s {encoded} | base64 -d | /bin/sh"),
        256 * 1024,
    )
    .await?;
    // Some SSH servers omit exit-status after a successful exec. A non-empty validated snapshot
    // proves the command ran and reached the window loop; accept that same compatibility case as
    // file reads/probing instead of reporting failure after tmux was already closed.
    if output.status != Some(0) && !(output.status.is_none() && !output.stdout.is_empty()) {
        return Err(if output.status == Some(44) {
            "remote tmux session is not running".into()
        } else {
            let error = String::from_utf8_lossy(&output.stderr).trim().to_string();
            if error.is_empty() {
                "could not snapshot and close tmux".into()
            } else {
                error
            }
        });
    }
    let by_window: std::collections::BTreeMap<_, _> = hints
        .iter()
        .map(|hint| (hint.window.as_str(), hint))
        .collect();
    let mut tabs = Vec::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let mut fields = line.splitn(4, '\t');
        let Some(window) = fields.next().filter(|window| window.starts_with('@')) else {
            continue;
        };
        let title = decode_field(fields.next().unwrap_or_default());
        let cwd = decode_field(fields.next().unwrap_or_default());
        let shell = decode_field(fields.next().unwrap_or_default());
        let hint = by_window.get(window).copied().or_else(|| {
            (by_window.len() == 1)
                .then(|| by_window.values().next().copied())
                .flatten()
        });
        tabs.push(RecoveryTab {
            window: window.into(),
            title: if title.is_empty() {
                hint.map(|hint| hint.title.clone()).unwrap_or_default()
            } else {
                title
            },
            cwd,
            shell,
            last_command: hint
                .map(|hint| hint.last_command.clone())
                .unwrap_or_default(),
        });
    }
    if tabs.is_empty() {
        Err("tmux closed but its window snapshot was empty".into())
    } else {
        Ok(tabs)
    }
}

pub async fn restore_tmux_session<H: client::Handler>(
    ssh: &client::Handle<H>,
    session: &str,
    socket: &str,
    tmux_path: &str,
    tabs: &[RecoveryTab],
    last_tab: Option<&str>,
) -> Result<(), String> {
    if tabs.is_empty() {
        return Ok(());
    }
    if !safe_tmux_path(tmux_path) {
        return Err("remote tmux path was invalid".into());
    }
    let mut script = format!(
        "t={}; s={}; n={}; if \"$t\" -L \"$s\" has-session -t \"$n\" 2>/dev/null; then exit 0; fi; ",
        shell_quote(tmux_path), shell_quote(socket), shell_quote(session),
    );
    for (index, tab) in tabs.iter().enumerate() {
        let cwd = if tab.cwd.is_empty() {
            "$HOME".into()
        } else {
            shell_quote(&tab.cwd)
        };
        let title = if tab.title.is_empty() {
            "shell"
        } else {
            &tab.title
        };
        let verb = if index == 0 {
            "new-session -d -s \"$n\""
        } else {
            "new-window -d -t \"$n\""
        };
        script.push_str(&format!(
            "\"$t\" -L \"$s\" {verb} -n {} -c {}; ",
            shell_quote(title),
            cwd,
        ));
    }
    script.push_str("sleep 0.2; ");
    for (index, tab) in tabs.iter().enumerate() {
        if tab.last_command.is_empty() {
            continue;
        }
        let history = if tab.shell.to_ascii_lowercase().contains("zsh") {
            format!(" print -s -- {}", shell_quote(&tab.last_command))
        } else {
            format!(" history -s -- {}", shell_quote(&tab.last_command))
        };
        script.push_str(&format!(
            "\"$t\" -L \"$s\" send-keys -t \"$n\":{} -l -- {}; \
             \"$t\" -L \"$s\" send-keys -t \"$n\":{} Enter; ",
            index,
            shell_quote(&history),
            index,
        ));
    }
    let active = last_tab
        .and_then(|window| tabs.iter().position(|tab| tab.window == window))
        .unwrap_or(0);
    script.push_str(&format!(
        "\"$t\" -L \"$s\" select-window -t \"$n\":{active}"
    ));
    let encoded = STANDARD.encode(script.as_bytes());
    let output = execute(
        ssh,
        format!("printf %s {encoded} | base64 -d | /bin/sh"),
        64 * 1024,
    )
    .await?;
    if output.status == Some(0) || (output.status.is_none() && output.stderr.is_empty()) {
        Ok(())
    } else {
        let error = String::from_utf8_lossy(&output.stderr).trim().to_string();
        Err(if error.is_empty() {
            "could not reconstruct tmux session".into()
        } else {
            error
        })
    }
}

pub async fn has_tmux_session<H: client::Handler>(
    ssh: &client::Handle<H>,
    session: &str,
    socket: &str,
    tmux_path: &str,
) -> Result<bool, String> {
    if !safe_tmux_path(tmux_path) {
        return Err("remote tmux path was invalid".into());
    }
    let output = execute(
        ssh,
        format!(
            "if {tmux_path} -L {} has-session -t {} 2>/dev/null; then printf BUOY_OPEN; exit 0; else exit 1; fi",
            shell_quote(socket), shell_quote(session),
        ),
        16 * 1024,
    ).await?;
    Ok(output.status == Some(0) || output.stdout == b"BUOY_OPEN")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_tmux_versions_used_for_control_mode() {
        assert_eq!(parse_tmux_version("tmux 3.4"), Some(vec![3, 4]));
        assert_eq!(parse_tmux_version("tmux 3.2a"), Some(vec![3, 2]));
        assert_eq!(parse_tmux_version("unexpected"), None);
        assert!(safe_tmux_path("/opt/homebrew/bin/tmux"));
        assert!(!safe_tmux_path("/tmp/tmux;touch /tmp/bad"));
    }
}
