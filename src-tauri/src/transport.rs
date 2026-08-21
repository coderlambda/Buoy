//! How a tmux client is launched: over ssh to another machine, or directly on this one.
//!
//! Both backends (control and plain) attach a tmux session over a pty; the ONLY difference between a
//! remote and a local session is the argv and environment of that child process. Isolating that here
//! means `kind:'local'` gets the identical durability machinery as a remote session — the same
//! control-mode parser, the same supervisor, the same reattach-by-socket behavior (DESIGN.md §5.3b)
//! — instead of a parallel implementation that would drift.
//!
//! Remote: `ssh -tt [-p port] <opts> -- host <base64 bootstrap + exec tmux>`
//! Local:  `tmux -L sock new-session -A -s name`, with the environment set on the child directly.

use crate::validation::{
    self, build_control_mode_ssh_args_with_recovery, build_local_control_mode_args,
    build_local_tmux_args, build_ssh_args_with_recovery, local_tmux_lc_all,
};

/// Which machine a session's tmux server lives on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    /// tmux on another host, reached with ssh.
    Ssh,
    /// tmux on THIS machine, exec'd directly (no ssh, no network).
    Local,
}

/// A ready-to-spawn child: program plus argv, and the environment to set on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnSpec {
    pub program: String,
    pub args: Vec<String>,
    /// Extra env for the child, applied on top of the inherited environment.
    pub env: Vec<(String, String)>,
}

/// ssh options shared by both backends: fail fast on connect, and notice a dead link within ~45s so
/// the supervisor can reconnect instead of sitting on a half-open socket.
fn ssh_opts(base_args: &[String]) -> Vec<String> {
    let mut opts: Vec<String> = vec![
        "-o".into(), "ConnectTimeout=8".into(),
        "-o".into(), "ServerAliveInterval=15".into(),
        "-o".into(), "ServerAliveCountMax=3".into(),
    ];
    opts.extend(base_args.iter().cloned());
    opts
}

/// Build the spawn spec for a tmux attach. `control` selects `-CC` control mode.
///
/// `lc_all` / `lang` are the app process's current locale (passed in rather than read here so this
/// stays a pure function): a LOCAL tmux server inherits our environment, and tmux only stores UTF-8
/// text when its own locale is UTF-8, so a non-UTF-8 environment gets C.UTF-8 forced on the child.
pub fn spawn_spec(
    transport: Transport,
    control: bool,
    host: &str,
    session: &str,
    tmux_path: &str,
    socket: &str,
    base_args: &[String],
    lc_all: Option<&str>,
    lang: Option<&str>,
) -> Result<SpawnSpec, validation::ValidationError> {
    spawn_spec_with_recovery(
        transport, control, host, session, tmux_path, socket, base_args, lc_all, lang, &[],
    )
}

pub fn spawn_spec_with_recovery(
    transport: Transport,
    control: bool,
    host: &str,
    session: &str,
    tmux_path: &str,
    socket: &str,
    base_args: &[String],
    lc_all: Option<&str>,
    lang: Option<&str>,
    recovery_windows: &[crate::session_store::RecoveryWindow],
) -> Result<SpawnSpec, validation::ValidationError> {
    match transport {
        Transport::Ssh => {
            let opts = ssh_opts(base_args);
            let args = if control {
                build_control_mode_ssh_args_with_recovery(
                    host, session, &opts, tmux_path, socket, recovery_windows,
                )?
            } else {
                build_ssh_args_with_recovery(
                    host, session, &opts, tmux_path, socket, recovery_windows,
                )?
            };
            // Overridable so a test can inject a fake transport that execs a LOCAL tmux (no
            // network/sshd) and exercise the real backend + supervisor end to end.
            let program = std::env::var("BUOY_SSH_BIN").unwrap_or_else(|_| "ssh".into());
            Ok(SpawnSpec {
                program,
                args,
                // Augment PATH so a Finder-launched app still finds ssh (mirrors env.js).
                env: vec![("PATH".into(), crate::augmented_path())],
            })
        }
        Transport::Local => {
            let mut args = if control {
                build_local_control_mode_args(session, tmux_path, socket)?
            } else {
                build_local_tmux_args(session, tmux_path, socket)?
            };
            // An imported default-server session belongs to the user. Attach without changing its
            // global shell, PATH, or default-command; those settings affect every other session on
            // that server. Child-only locale/TERM are still safe and needed by the tmux client.
            if socket == "default" {
                let mut env = vec![
                    ("PATH".into(), crate::augmented_path()),
                    ("TERM".into(), "xterm-256color".into()),
                ];
                if let Some(v) = local_tmux_lc_all(lc_all, lang) {
                    env.push(("LC_ALL".into(), v.into()));
                }
                return Ok(SpawnSpec { program: tmux_path.to_string(), args, env });
            }
            let path = crate::claude_integration::path_with_local_shim(&crate::augmented_path());
            let shell_launcher = crate::claude_integration::local_shell_launcher()
                .unwrap_or_else(|_| std::path::PathBuf::from("/bin/sh"));
            let shell_launcher = shell_launcher.to_string_lossy().into_owned();
            let real_shell = crate::claude_integration::local_real_shell();
            let tmux_bin = std::fs::canonicalize(tmux_path)
                .unwrap_or_else(|_| std::path::PathBuf::from(tmux_path))
                .to_string_lossy()
                .into_owned();
            let initial_shell = if std::path::Path::new(&shell_launcher).is_file() {
                shell_launcher.clone()
            } else {
                real_shell.clone()
            };
            let default_command = concat!(
                "if [ -x \"$BUOY_SHELL_LAUNCHER\" ]; then exec \"$BUOY_SHELL_LAUNCHER\"; ",
                "else exec \"$BUOY_REAL_SHELL\" -l; fi"
            );
            // The first pane gets the shell launcher through SHELL. After `new-session` starts it,
            // restore tmux's default shell and use an explicit launcher command for future panes.
            // Updating only tmux's PATH cannot change a shell after its rc files have run.
            args.extend([
                ";".into(), "set-environment".into(), "-g".into(), "PATH".into(), path.clone(),
                ";".into(), "set-environment".into(), "-g".into(), "BUOY_TERMINAL".into(), "1".into(),
                ";".into(), "set-environment".into(), "-g".into(), "BUOY_SESSION_ID".into(), session.into(),
                ";".into(), "set-environment".into(), "-g".into(), "BUOY_SHELL_LAUNCHER".into(), shell_launcher.clone(),
                ";".into(), "set-environment".into(), "-g".into(), "BUOY_REAL_SHELL".into(), real_shell.clone(),
                ";".into(), "set-environment".into(), "-g".into(), "BUOY_TMUX_BIN".into(), tmux_bin.clone(),
                ";".into(), "set-environment".into(), "-g".into(), "SHELL".into(), real_shell.clone(),
                ";".into(), "set-option".into(), "-g".into(), "default-shell".into(), real_shell.clone(),
                ";".into(), "set-option".into(), "-g".into(), "default-command".into(), default_command.into(),
            ]);
            let mut env: Vec<(String, String)> = vec![
                ("PATH".into(), path),
                ("BUOY_TERMINAL".into(), "1".into()),
                ("BUOY_SESSION_ID".into(), session.into()),
                ("BUOY_SHELL_LAUNCHER".into(), shell_launcher),
                ("BUOY_REAL_SHELL".into(), real_shell),
                ("BUOY_TMUX_BIN".into(), tmux_bin),
                ("SHELL".into(), initial_shell),
                // The tmux CLIENT draws into our pty (plain mode) and the SERVER hands TERM to the
                // shells it starts. portable_pty does not reliably inherit TERM, and an unset TERM
                // leaves the shell thinking it's dumb: no colors, broken full-screen editors.
                ("TERM".into(), "xterm-256color".into()),
            ];
            if let Some(v) = local_tmux_lc_all(lc_all, lang) {
                env.push(("LC_ALL".into(), v.into()));
            }
            Ok(SpawnSpec { program: tmux_path.to_string(), args, env })
        }
    }
}

/// The app process's own locale, for `spawn_spec`.
pub fn current_locale() -> (Option<String>, Option<String>) {
    (std::env::var("LC_ALL").ok(), std::env::var("LANG").ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn remote_script(args: &[String]) -> String {
        let dd = args.iter().position(|a| a == "--").unwrap();
        let command = &args[dd + 2];
        let encoded = command
            .strip_prefix("exec /bin/sh -c \"$(echo ").unwrap()
            .strip_suffix(" | base64 -d)\"").unwrap();
        String::from_utf8(crate::validation::base64_decode(encoded).unwrap()).unwrap()
    }

    // TC-T1 remote control mode is unchanged by the local refactor: still ssh, still -tt, still the
    // env LC_ALL prefix and -CC before -L. This is the regression guard for the shared path.
    #[test]
    fn tc_t1_ssh_control_spec() {
        let s = spawn_spec(Transport::Ssh, true, "me@h", "dt-x", "/t", "dtcc3-7", &[], None, None).unwrap();
        assert_eq!(s.program, "ssh");
        assert_eq!(s.args[0], "-tt");
        let dd = s.args.iter().position(|a| a == "--").unwrap();
        assert_eq!(s.args[dd + 1], "me@h");
        let script = remote_script(&s.args);
        assert!(script.contains("LC_ALL=C.UTF-8"));
        assert!(script.contains("exec /t -CC -L dtcc3-7"));
        // ssh options are present
        assert!(s.args.windows(2).any(|w| w == ["-o", "ConnectTimeout=8"]));
        // the app's locale must NOT leak into the remote child's env (the remote gets its LC_ALL via
        // the argv prefix instead)
        assert!(!s.env.iter().any(|(k, _)| k == "LC_ALL"), "no LC_ALL in ssh child env: {:?}", s.env);
    }

    #[test]
    fn remote_recovery_runs_inside_the_actual_interactive_ssh_bootstrap() {
        let windows = vec![crate::session_store::RecoveryWindow {
            name: "work".into(), cwd: "/tmp/project".into(),
            command: "codex".into(), active: true,
        }];
        let s = spawn_spec_with_recovery(
            Transport::Ssh, true, "me@h", "dt-x", "/t", "dtcc3-7-dt-x", &[],
            None, None, &windows,
        ).unwrap();
        let script = remote_script(&s.args);
        let check = script.find("if ! /t -L dtcc3-7-dt-x has-session -t dt-x").unwrap();
        let attach = script.find("exec /t -CC -L dtcc3-7-dt-x new-session").unwrap();
        assert!(check < attach, "rebuild decision happens after the one SSH login and before attach");
        assert!(!script.contains("/tmp/project"), "cwd remains base64 data in the remote shell");
        assert!(script.contains("BUOY_RESTORED"));
    }

    // TC-T2 local mode: the program IS tmux, there is no ssh and no host anywhere in the argv.
    #[test]
    fn tc_t2_local_control_spec() {
        let s = spawn_spec(Transport::Local, true, "", "dt-x", "/opt/homebrew/bin/tmux",
                           "dtcc3-6-dt-x", &[], Some("en_US.UTF-8"), None).unwrap();
        let launcher = crate::claude_integration::local_shell_launcher().unwrap()
            .to_string_lossy().into_owned();
        let real_shell = crate::claude_integration::local_real_shell();
        let tmux_bin = std::fs::canonicalize("/opt/homebrew/bin/tmux")
            .unwrap_or_else(|_| std::path::PathBuf::from("/opt/homebrew/bin/tmux"))
            .to_string_lossy().into_owned();
        let default_command = "if [ -x \"$BUOY_SHELL_LAUNCHER\" ]; then exec \"$BUOY_SHELL_LAUNCHER\"; else exec \"$BUOY_REAL_SHELL\" -l; fi";
        assert_eq!(s.program, "/opt/homebrew/bin/tmux");
        assert_eq!(s.args, [
            "-CC", "-L", "dtcc3-6-dt-x", "new-session", "-D", "-A", "-s", "dt-x", ";",
            "set-option", "-g", "focus-events", "on", ";", "set-environment", "-g", "PATH",
            &crate::claude_integration::path_with_local_shim(&crate::augmented_path()), ";",
            "set-environment", "-g", "BUOY_TERMINAL", "1", ";", "set-environment", "-g",
            "BUOY_SESSION_ID", "dt-x", ";", "set-environment", "-g", "BUOY_SHELL_LAUNCHER",
            &launcher, ";", "set-environment", "-g", "BUOY_REAL_SHELL", &real_shell, ";",
            "set-environment", "-g", "BUOY_TMUX_BIN", &tmux_bin, ";",
            "set-environment", "-g", "SHELL", &real_shell, ";", "set-option", "-g",
            "default-shell", &real_shell, ";", "set-option", "-g", "default-command",
            default_command,
        ]);
        assert!(!s.args.iter().any(|a| a.contains("ssh") || a == "-tt" || a == "--"),
            "no ssh scaffolding: {:?}", s.args);
        // TERM is always set; LC_ALL is not, because the environment is already UTF-8
        assert!(s.env.iter().any(|(k, v)| k == "TERM" && v == "xterm-256color"));
        assert!(s.env.iter().any(|(k, v)| k == "BUOY_TERMINAL" && v == "1"));
        assert!(s.env.iter().any(|(k, v)| k == "BUOY_SESSION_ID" && v == "dt-x"));
        assert!(s.env.iter().any(|(k, v)| k == "BUOY_SHELL_LAUNCHER" && v == &launcher));
        assert!(s.env.iter().any(|(k, v)| k == "BUOY_REAL_SHELL" && v == &real_shell));
        assert!(s.env.iter().any(|(k, v)| k == "BUOY_TMUX_BIN" && v == &tmux_bin));
        assert!(s.env.iter().any(|(k, v)| {
            k == "PATH" && v.split(':').next() == crate::claude_integration::local_shim_dir().ok()
                .as_deref().and_then(|p| p.to_str())
        }));
        assert!(!s.env.iter().any(|(k, _)| k == "LC_ALL"), "UTF-8 locale left alone: {:?}", s.env);
    }

    // TC-T3 local plain mode: same, without -CC/-D.
    #[test]
    fn tc_t3_local_plain_spec() {
        let s = spawn_spec(Transport::Local, false, "", "dt-x", "tmux", "dtapp3-6", &[], None, None).unwrap();
        let launcher = crate::claude_integration::local_shell_launcher().unwrap()
            .to_string_lossy().into_owned();
        let real_shell = crate::claude_integration::local_real_shell();
        let default_command = "if [ -x \"$BUOY_SHELL_LAUNCHER\" ]; then exec \"$BUOY_SHELL_LAUNCHER\"; else exec \"$BUOY_REAL_SHELL\" -l; fi";
        assert_eq!(s.program, "tmux");
        assert_eq!(s.args, [
            "-L", "dtapp3-6", "new-session", "-A", "-s", "dt-x", ";", "set-option", "-g",
            "focus-events", "on", ";", "set-environment", "-g", "PATH",
            &crate::claude_integration::path_with_local_shim(&crate::augmented_path()), ";",
            "set-environment", "-g", "BUOY_TERMINAL", "1", ";", "set-environment", "-g",
            "BUOY_SESSION_ID", "dt-x", ";", "set-environment", "-g", "BUOY_SHELL_LAUNCHER",
            &launcher, ";", "set-environment", "-g", "BUOY_REAL_SHELL", &real_shell, ";",
            "set-environment", "-g", "BUOY_TMUX_BIN", "tmux", ";", "set-environment", "-g",
            "SHELL", &real_shell, ";", "set-option", "-g",
            "default-shell", &real_shell, ";", "set-option", "-g", "default-command",
            default_command,
        ]);
        // no locale at all -> force C.UTF-8, or tmux mangles every non-ASCII byte to '_'
        assert!(s.env.iter().any(|(k, v)| k == "LC_ALL" && v == "C.UTF-8"), "{:?}", s.env);
    }

    // TC-T4 a local session needs no host, and validation still rejects a hostile session name on
    // the local path (it reaches tmux's -s and the kill target).
    #[test]
    fn tc_t4_local_needs_no_host_but_still_validates() {
        assert!(spawn_spec(Transport::Local, false, "", "dt-x", "tmux", "dtapp", &[], None, None).is_ok(),
            "empty host is fine locally");
        let bad = spawn_spec(Transport::Local, false, "", "a;rm -rf /", "tmux", "dtapp", &[], None, None);
        assert_eq!(bad.unwrap_err().field, "session");
        // ...whereas the ssh path still requires one
        let no_host = spawn_spec(Transport::Ssh, false, "", "dt-x", "tmux", "dtapp", &[], None, None);
        assert_eq!(no_host.unwrap_err().field, "host");
    }
}
