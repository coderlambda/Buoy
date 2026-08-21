//! Buoy-scoped Claude Code notification integration.
//!
//! Claude Code's default `auto` notification channel only emits terminal notifications for a
//! small terminal allowlist. Buoy deliberately does not pretend to be one of those terminals and
//! does not mutate the user's global Claude settings. Instead, a Buoy-owned shell integration runs
//! after user startup files and makes a scoped `claude` shim authoritative. The shim delegates to
//! the real binary with an app-owned plugin whose lifecycle hooks emit a generic OSC 777 request to
//! the Claude process's terminal pane.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

const CLAUDE_PLUGIN_DIR_NAME: &str = "buoy-claude-notifications";
const SHELL_LAUNCHER_NAME: &str = "buoy-shell";
const SHELL_INTEGRATION_DIR_NAME: &str = "buoy-shell-integration";

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
tmux_command=
if [ -n "${BUOY_TMUX_BIN:-}" ] && [ -x "$BUOY_TMUX_BIN" ]; then
  tmux_command=$BUOY_TMUX_BIN
elif command -v tmux >/dev/null 2>&1; then
  tmux_command=$(command -v tmux 2>/dev/null) || tmux_command=
fi
if [ -n "${TMUX:-}" ] && [ -n "${TMUX_PANE:-}" ] && [ -n "$tmux_command" ]; then
  tty_target=$("$tmux_command" display-message -p -t "$TMUX_PANE" '#{pane_tty}' 2>/dev/null) ||
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

/// Shell entrypoint used as tmux's initial default shell and its later default command.
///
/// The launcher runs before user startup files. It creates a private, per-pane `claude` delegator
/// and then selects a shell-specific post-startup integration. The delegator scrubs every Buoy
/// temporary shim from PATH before calling the persistent launcher, so nested/reconnected shells
/// cannot bounce between generated shims.
const SHELL_LAUNCHER: &str = r#"#!/bin/sh
set -uf

launcher_path=$0
case "$launcher_path" in
  */*) ;;
  *) launcher_path=$(command -v "$launcher_path" 2>/dev/null || printf '%s\n' "$launcher_path") ;;
esac
launcher_dir=${launcher_path%/*}
[ "$launcher_dir" != "$launcher_path" ] || launcher_dir=.
launcher_dir=$(CDPATH= cd "$launcher_dir" 2>/dev/null && pwd) || launcher_dir=.

integration_dir=$launcher_dir/buoy-shell-integration
claude_launcher=$launcher_dir/claude
BUOY_SHELL_INTEGRATION_DIR=$integration_dir
BUOY_CLAUDE_LAUNCHER=$claude_launcher
export BUOY_SHELL_INTEGRATION_DIR BUOY_CLAUDE_LAUNCHER

real_shell=${BUOY_REAL_SHELL:-${SHELL:-/bin/sh}}
case "$real_shell" in
  /*) [ -x "$real_shell" ] || real_shell=/bin/sh ;;
  *) real_shell=/bin/sh ;;
esac
if [ "$real_shell" -ef "$launcher_path" ] 2>/dev/null; then
  real_shell=/bin/sh
fi
SHELL=$real_shell
export SHELL

# TMUX_PANE is exact when tmux launches us. The process id keeps the no-tmux fallback isolated.
surface=${BUOY_SESSION_ID:-shell}-${TMUX_PANE:-$$}
shim_root=${TMPDIR:-/tmp}/buoy-cli-shims/$surface
shim_path=$shim_root/claude
if mkdir -p "$shim_root" 2>/dev/null && chmod 700 "$shim_root" 2>/dev/null; then
  shim_tmp=$shim_path.$$.tmp
  rm -f "$shim_tmp" 2>/dev/null || true
  old_umask=$(umask)
  umask 077
  if cat > "$shim_tmp" <<'BUOY_CLAUDE_SHIM'
#!/bin/sh
set -uf

clean_path=
old_ifs=$IFS
IFS=:
for entry in ${PATH:-}; do
  case "$entry" in
    */buoy-cli-shims/*|*/buoy-cli-shims) continue ;;
    */cmux-cli-shims/*|*/cmux-cli-shims) continue ;;
  esac
  # The persistent Buoy launcher is invoked by absolute path below. Keep its directory out of the
  # delegated PATH too, otherwise another tool's Claude wrapper can resolve back to Buoy and form
  # a wrapper cycle.
  if [ -n "${BUOY_CLAUDE_LAUNCHER:-}" ] && [ "$entry" = "${BUOY_CLAUDE_LAUNCHER%/*}" ]; then
    continue
  fi
  if [ -z "$clean_path" ]; then clean_path=$entry; else clean_path=$clean_path:$entry; fi
done
IFS=$old_ifs
PATH=$clean_path
export PATH

if [ -x "${BUOY_CLAUDE_LAUNCHER:-}" ]; then
  exec "$BUOY_CLAUDE_LAUNCHER" "$@"
fi
exec claude "$@"
BUOY_CLAUDE_SHIM
  then
    chmod 700 "$shim_tmp" 2>/dev/null || true
    if mv -f "$shim_tmp" "$shim_path" 2>/dev/null; then
      BUOY_CLAUDE_SHIM=$shim_path
      BUOY_CLAUDE_SHIM_ROOT=$shim_root
      export BUOY_CLAUDE_SHIM BUOY_CLAUDE_SHIM_ROOT
    else
      rm -f "$shim_tmp" 2>/dev/null || true
    fi
  fi
  umask "$old_umask"
fi

shell_name=${real_shell##*/}
case "$shell_name" in
  zsh)
    if [ "${ZDOTDIR+x}" = x ]; then
      BUOY_REAL_ZDOTDIR=$ZDOTDIR
      BUOY_REAL_ZDOTDIR_SET=1
      export BUOY_REAL_ZDOTDIR
    else
      unset BUOY_REAL_ZDOTDIR
      BUOY_REAL_ZDOTDIR_SET=0
    fi
    export BUOY_REAL_ZDOTDIR_SET
    ZDOTDIR=$integration_dir/zsh
    export ZDOTDIR
    exec "$real_shell" -l "$@"
    ;;
  bash)
    exec "$real_shell" --rcfile "$integration_dir/bash/bootstrap.bash" -i "$@"
    ;;
  fish)
    exec "$real_shell" -l --init-command 'source "$BUOY_SHELL_INTEGRATION_DIR/fish/config.fish"' "$@"
    ;;
  sh|dash|ksh|mksh|yash)
    ENV=$integration_dir/posix-integration.sh
    export ENV
    exec "$real_shell" -l "$@"
    ;;
  *)
    exec "$real_shell" -l "$@"
    ;;
esac
"#;

/// zsh reads this through a temporary ZDOTDIR. It restores the original directory before zsh
/// chooses `.zprofile`/`.zshrc`, then schedules Buoy's repair for the first prompt after both files.
const ZSH_ENV: &str = r#"# Buoy zsh bootstrap: preserve all user startup files without editing them.
if [[ "${BUOY_REAL_ZDOTDIR_SET:-0}" == 1 ]]; then
  builtin export ZDOTDIR="${BUOY_REAL_ZDOTDIR-}"
else
  builtin unset ZDOTDIR
fi
builtin unset BUOY_REAL_ZDOTDIR BUOY_REAL_ZDOTDIR_SET

{
  builtin typeset _buoy_real_zshenv="${ZDOTDIR:-$HOME}/.zshenv"
  builtin typeset _buoy_own_zshenv="${BUOY_SHELL_INTEGRATION_DIR:-}/zsh/.zshenv"
  if [[ "$_buoy_real_zshenv" != "$_buoy_own_zshenv" && -r "$_buoy_real_zshenv" ]]; then
    builtin source -- "$_buoy_real_zshenv"
  fi
} always {
  if [[ -o interactive && -r "${BUOY_SHELL_INTEGRATION_DIR:-}/zsh/integration.zsh" ]]; then
    builtin source -- "${BUOY_SHELL_INTEGRATION_DIR}/zsh/integration.zsh"
  fi
  builtin unset _buoy_real_zshenv _buoy_own_zshenv
}
"#;

const ZSH_INTEGRATION: &str = r#"# Repair Claude lookup once, after the user's zsh startup files.
_buoy_fix_claude_lookup() {
  if [[ -x "${BUOY_CLAUDE_SHIM:-}" ]]; then
    local shim_dir="${BUOY_CLAUDE_SHIM:h}"
    local entry
    local -a cleaned_path
    cleaned_path=()
    for entry in "${path[@]}"; do
      [[ "$entry" == */buoy-cli-shims/* || "$entry" == */buoy-cli-shims ]] && continue
      cleaned_path+=("$entry")
    done
    path=("$shim_dir" "${cleaned_path[@]}")
    builtin export PATH
    builtin unalias claude >/dev/null 2>&1 || true
    claude() { "${BUOY_CLAUDE_SHIM}" "$@"; }
    rehash >/dev/null 2>&1 || true
  fi
  add-zsh-hook -d precmd _buoy_fix_claude_lookup
}

autoload -Uz add-zsh-hook
add-zsh-hook precmd _buoy_fix_claude_lookup
"#;

/// bash's rcfile manually reproduces login-profile loading because `--rcfile` and login startup
/// are mutually exclusive in bash. The Buoy repair runs after the user's selected login profile.
const BASH_BOOTSTRAP: &str = r#"# Buoy bash bootstrap: reproduce interactive login startup, then integrate.
if [ -z "${BUOY_BASH_BOOTSTRAP_ACTIVE:-}" ]; then
  BUOY_BASH_BOOTSTRAP_ACTIVE=1
  export BUOY_BASH_BOOTSTRAP_ACTIVE
  [ ! -r /etc/profile ] || . /etc/profile
  if [ -r "$HOME/.bash_profile" ]; then
    . "$HOME/.bash_profile"
  elif [ -r "$HOME/.bash_login" ]; then
    . "$HOME/.bash_login"
  elif [ -r "$HOME/.profile" ]; then
    . "$HOME/.profile"
  fi
  [ ! -r "${BUOY_SHELL_INTEGRATION_DIR:-}/posix-integration.sh" ] ||
    . "${BUOY_SHELL_INTEGRATION_DIR}/posix-integration.sh"
  unset BUOY_BASH_BOOTSTRAP_ACTIVE
fi
"#;

/// Shared by bash and POSIX shells whose `ENV` hook runs after login startup.
const POSIX_INTEGRATION: &str = r#"# Buoy post-startup integration for Bourne-family shells.
if [ -x "${BUOY_CLAUDE_SHIM:-}" ]; then
  buoy_shim_dir=${BUOY_CLAUDE_SHIM%/*}
  buoy_clean_path=
  buoy_old_ifs=$IFS
  IFS=:
  for buoy_entry in ${PATH:-}; do
    case "$buoy_entry" in
      */buoy-cli-shims/*|*/buoy-cli-shims) continue ;;
    esac
    if [ -z "$buoy_clean_path" ]; then
      buoy_clean_path=$buoy_entry
    else
      buoy_clean_path=$buoy_clean_path:$buoy_entry
    fi
  done
  IFS=$buoy_old_ifs
  PATH=$buoy_shim_dir${buoy_clean_path:+:$buoy_clean_path}
  export PATH
  unalias claude >/dev/null 2>&1 || true
  # Parse the function only after unalias runs. Interactive bash expands aliases while parsing a
  # compound `if`, so a literal `claude() { ...; }` here can be corrupted by the user's alias.
  eval 'claude() { "$BUOY_CLAUDE_SHIM" "$@"; }'
  hash -r >/dev/null 2>&1 || true
  unset buoy_shim_dir buoy_clean_path buoy_old_ifs buoy_entry
fi
"#;

const FISH_INTEGRATION: &str = r#"# Buoy fish integration is sourced by --init-command after config.fish.
if status is-interactive; and test -x "$BUOY_CLAUDE_SHIM"
    set -l buoy_shim_dir (string replace -r '/[^/]*$' '' -- "$BUOY_CLAUDE_SHIM")
    set -l buoy_clean_path
    for buoy_entry in $PATH
        if not string match -q '*/buoy-cli-shims/*' -- "$buoy_entry"
            set -a buoy_clean_path "$buoy_entry"
        end
    end
    set -gx PATH "$buoy_shim_dir" $buoy_clean_path
    abbr --erase claude >/dev/null 2>&1
    functions -e claude 2>/dev/null
    function claude --description 'Claude Code with Buoy notifications'
        command "$BUOY_CLAUDE_SHIM" $argv
    end
end
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
  case "$candidate" in
    */buoy-cli-shims/*/claude|*/buoy-cli-shims/claude) continue ;;
  esac
  [ -x "$candidate" ] || continue
  [ "$candidate" -ef "$0" ] 2>/dev/null && continue
  is_cmux_claude=
  if [ -n "${CMUX_CLAUDE_WRAPPER_SHIM:-}" ] &&
     [ -e "$CMUX_CLAUDE_WRAPPER_SHIM" ] &&
     [ "$candidate" -ef "$CMUX_CLAUDE_WRAPPER_SHIM" ] 2>/dev/null; then
    is_cmux_claude=1
  elif [ -n "${CMUX_CLAUDE_WRAPPER_SHIM_ROOT:-}" ] &&
       [ "$candidate" = "${CMUX_CLAUDE_WRAPPER_SHIM_ROOT%/}/claude" ]; then
    is_cmux_claude=1
  else
    case "$candidate" in
      */cmux-cli-shims/*/claude|*/cmux-cli-shims/claude) is_cmux_claude=1 ;;
    esac
  fi
  if [ -n "$is_cmux_claude" ]; then
    # A cmux surface owns its wrapper; a Buoy pane owns this one. Skip cmux's managed shim and
    # continue to the actual Claude executable so the two integrations never nest.
    continue
  fi
  real_claude=$candidate
  break
done
IFS=$old_ifs

# Outside Buoy, on a nested Buoy invocation, or after cmux has already injected its settings, skip
# every wrapper and go straight to Claude. Current cmux exports both the lowercase re-exec guard and
# CMUX_AGENT_LAUNCH_KIND before it enters another shim; either marker supports older PATH layouts.
if [ "${BUOY_TERMINAL:-}" != 1 ] || [ "${BUOY_CLAUDE_SHIM_ACTIVE:-}" = 1 ] ||
   [ -n "${cmux_claude_wrapper_reexec_guard:-}" ] ||
   [ "${CMUX_AGENT_LAUNCH_KIND:-}" = claude ]; then
  if [ -z "$real_claude" ]; then
    echo "buoy: could not find the real claude executable on PATH" >&2
    exit 127
  fi
  exec "$real_claude" "$@"
fi

if [ -z "$real_claude" ]; then
  echo "buoy: could not find the real claude executable on PATH" >&2
  exit 127
fi

# An explicit opt-out disables Buoy's plugin when there is no existing owner.
if [ "${BUOY_CLAUDE_NOTIFICATIONS_DISABLED:-}" = 1 ]; then
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

pub fn local_shell_launcher() -> io::Result<PathBuf> {
    Ok(local_shim_dir()?.join(SHELL_LAUNCHER_NAME))
}

/// Resolve the real user shell before `SHELL` is replaced with Buoy's launcher for the first tmux
/// pane. Only an absolute executable is safe for tmux's `default-shell`; otherwise use `/bin/sh`.
pub fn local_real_shell() -> String {
    for key in ["BUOY_REAL_SHELL", "SHELL"] {
        let Some(value) = std::env::var_os(key).filter(|value| !value.is_empty()) else {
            continue;
        };
        let path = PathBuf::from(value);
        if path.is_absolute() && path.is_file() {
            return path.to_string_lossy().into_owned();
        }
    }
    "/bin/sh".to_string()
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

    let shell_integration = dir.join(SHELL_INTEGRATION_DIR_NAME);
    install_file(
        &shell_integration.join("zsh").join(".zshenv"),
        ZSH_ENV.as_bytes(),
        false,
    )?;
    install_file(
        &shell_integration.join("zsh").join("integration.zsh"),
        ZSH_INTEGRATION.as_bytes(),
        false,
    )?;
    install_file(
        &shell_integration.join("bash").join("bootstrap.bash"),
        BASH_BOOTSTRAP.as_bytes(),
        false,
    )?;
    install_file(
        &shell_integration.join("fish").join("config.fish"),
        FISH_INTEGRATION.as_bytes(),
        false,
    )?;
    install_file(
        &shell_integration.join("posix-integration.sh"),
        POSIX_INTEGRATION.as_bytes(),
        false,
    )?;

    // Publish the Claude launcher after its plugin, then publish the shell launcher after every
    // dependency it may expose. A concurrently opened pane therefore sees either the old complete
    // bundle or the new complete bundle, never a generated shim pointing at a missing launcher.
    let launcher = dir.join("claude");
    install_file(&launcher, CLAUDE_WRAPPER.as_bytes(), true)?;
    install_file(
        &dir.join(SHELL_LAUNCHER_NAME),
        SHELL_LAUNCHER.as_bytes(),
        true,
    )?;
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
    remote_tmux_script_inner(tmux_path, socket, session, control, "")
}

pub fn remote_tmux_script_with_recovery(
    tmux_path: &str,
    socket: &str,
    session: &str,
    control: bool,
    windows: &[crate::session_store::RecoveryWindow],
) -> Result<String, String> {
    let recovery = if windows.is_empty() {
        String::new()
    } else {
        crate::tmux_discovery::recovery_shell_block(tmux_path, socket, session, windows)?
    };
    Ok(remote_tmux_script_inner(tmux_path, socket, session, control, &recovery))
}

fn remote_tmux_script_inner(
    tmux_path: &str,
    socket: &str,
    session: &str,
    control: bool,
    recovery: &str,
) -> String {
    // Importing a user's existing default-server session must be observational: do not install a
    // shell wrapper, replace tmux's global default-command, or use `-D` (which would evict their
    // other clients). The control client can coexist with ordinary attached clients.
    if socket == "default" {
        let cc = if control { " -CC" } else { "" };
        return format!(
            "LC_ALL=C.UTF-8\nexport LC_ALL\n{recovery}exec {tmux_path}{cc} -L default attach-session -t {session} \\; set-option -g focus-events on"
        );
    }
    let wrapper_b64 = crate::validation::base64_encode(CLAUDE_WRAPPER.as_bytes());
    let manifest_b64 = crate::validation::base64_encode(CLAUDE_PLUGIN_MANIFEST.as_bytes());
    let hooks_b64 = crate::validation::base64_encode(CLAUDE_PLUGIN_HOOKS.as_bytes());
    let notify_b64 = crate::validation::base64_encode(CLAUDE_PLUGIN_NOTIFY.as_bytes());
    let shell_launcher_b64 = crate::validation::base64_encode(SHELL_LAUNCHER.as_bytes());
    let zsh_env_b64 = crate::validation::base64_encode(ZSH_ENV.as_bytes());
    let zsh_integration_b64 = crate::validation::base64_encode(ZSH_INTEGRATION.as_bytes());
    let bash_bootstrap_b64 = crate::validation::base64_encode(BASH_BOOTSTRAP.as_bytes());
    let posix_integration_b64 = crate::validation::base64_encode(POSIX_INTEGRATION.as_bytes());
    let fish_integration_b64 = crate::validation::base64_encode(FISH_INTEGRATION.as_bytes());
    let cc = if control { " -CC" } else { "" };
    let detach = if control { " -D" } else { "" };
    format!(
        r#"buoy_bin="${{XDG_CACHE_HOME:-$HOME/.cache}}/buoy/bin"
buoy_claude="$buoy_bin/claude"
buoy_plugin="$buoy_bin/{CLAUDE_PLUGIN_DIR_NAME}"
buoy_shell="$buoy_bin/{SHELL_LAUNCHER_NAME}"
buoy_shell_integration="$buoy_bin/{SHELL_INTEGRATION_DIR_NAME}"
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
  buoy_install {zsh_env_b64} "$buoy_shell_integration/zsh/.zshenv" 644
  buoy_install {zsh_integration_b64} "$buoy_shell_integration/zsh/integration.zsh" 644
  buoy_install {bash_bootstrap_b64} "$buoy_shell_integration/bash/bootstrap.bash" 644
  buoy_install {fish_integration_b64} "$buoy_shell_integration/fish/config.fish" 644
  buoy_install {posix_integration_b64} "$buoy_shell_integration/posix-integration.sh" 644
  buoy_install {wrapper_b64} "$buoy_claude" 755
  buoy_install {shell_launcher_b64} "$buoy_shell" 755
fi
PATH="$buoy_bin:$PATH"
export PATH
BUOY_TERMINAL=1
BUOY_SESSION_ID={session}
BUOY_SHELL_LAUNCHER=$buoy_shell
BUOY_REAL_SHELL=${{SHELL:-/bin/sh}}
BUOY_TMUX_BIN={tmux_path}
case "$BUOY_REAL_SHELL" in
  /*) [ -x "$BUOY_REAL_SHELL" ] || BUOY_REAL_SHELL=/bin/sh ;;
  *) BUOY_REAL_SHELL=/bin/sh ;;
esac
buoy_tmux_candidate=$(command -v "$BUOY_TMUX_BIN" 2>/dev/null) || buoy_tmux_candidate=
if [ -n "$buoy_tmux_candidate" ]; then
  case "$buoy_tmux_candidate" in
    /*) BUOY_TMUX_BIN=$buoy_tmux_candidate ;;
    */*)
      buoy_tmux_dir=${{buoy_tmux_candidate%/*}}
      buoy_tmux_name=${{buoy_tmux_candidate##*/}}
      buoy_tmux_dir=$(CDPATH= cd "$buoy_tmux_dir" 2>/dev/null && pwd) || buoy_tmux_dir=
      [ -z "$buoy_tmux_dir" ] || BUOY_TMUX_BIN=$buoy_tmux_dir/$buoy_tmux_name
      ;;
  esac
fi
if [ -x "$BUOY_SHELL_LAUNCHER" ]; then
  SHELL=$BUOY_SHELL_LAUNCHER
fi
export BUOY_TERMINAL BUOY_SESSION_ID BUOY_SHELL_LAUNCHER BUOY_REAL_SHELL BUOY_TMUX_BIN SHELL
LC_ALL=C.UTF-8
export LC_ALL
{recovery}
exec {tmux_path}{cc} -L {socket} new-session{detach} -A -s {session} \; set-option -g focus-events on \; set-environment -g PATH "$PATH" \; set-environment -g BUOY_TERMINAL 1 \; set-environment -g BUOY_SESSION_ID "$BUOY_SESSION_ID" \; set-environment -g BUOY_SHELL_LAUNCHER "$BUOY_SHELL_LAUNCHER" \; set-environment -g BUOY_REAL_SHELL "$BUOY_REAL_SHELL" \; set-environment -g BUOY_TMUX_BIN "$BUOY_TMUX_BIN" \; set-environment -g SHELL "$BUOY_REAL_SHELL" \; set-option -g default-shell "$BUOY_REAL_SHELL" \; set-option -g default-command 'if [ -x "$BUOY_SHELL_LAUNCHER" ]; then exec "$BUOY_SHELL_LAUNCHER"; else exec "$BUOY_REAL_SHELL" -l; fi'"#
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

    #[cfg(unix)]
    fn run_interactive_shell(
        launcher: &Path,
        real_shell: &str,
        home: &Path,
        temporary: &Path,
        real_dir: &Path,
        extra_env: &[(&str, &Path)],
        input: &str,
    ) -> String {
        use portable_pty::{native_pty_system, CommandBuilder, PtySize};
        use std::io::{Read, Write};
        use std::sync::mpsc;
        use std::time::{Duration, Instant};

        let pair = native_pty_system()
            .openpty(PtySize { rows: 24, cols: 100, pixel_width: 0, pixel_height: 0 })
            .expect("open shell-integration pty");
        let mut command = CommandBuilder::new(launcher);
        command.env("HOME", home);
        command.env("TMPDIR", temporary);
        command.env("TERM", "xterm-256color");
        command.env("PATH", "/usr/bin:/bin");
        command.env("SHELL", real_shell);
        command.env("BUOY_REAL_SHELL", real_shell);
        command.env("BUOY_TERMINAL", "1");
        command.env("BUOY_SESSION_ID", "startup-race");
        command.env("BUOY_TEST_REAL_DIR", real_dir);
        for (key, value) in extra_env {
            command.env(key, value);
        }
        let mut child = pair.slave.spawn_command(command).expect("spawn integrated shell");
        drop(pair.slave);
        let mut writer = pair.master.take_writer().expect("shell pty writer");
        let mut reader = pair.master.try_clone_reader().expect("shell pty reader");
        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        std::thread::spawn(move || {
            let mut buffer = [0u8; 4096];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(count) => {
                        if tx.send(buffer[..count].to_vec()).is_err() { break; }
                    }
                }
            }
        });

        writer.write_all(input.as_bytes()).expect("write shell commands");
        writer.flush().expect("flush shell commands");
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut output = Vec::new();
        while Instant::now() < deadline {
            match rx.recv_timeout(Duration::from_millis(250)) {
                Ok(chunk) => output.extend(chunk),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
            // A pty echoes the command line before the shell prints the marker itself.
            if String::from_utf8_lossy(&output).matches("__BUOY_SHELL_DONE__").count() >= 2 {
                break;
            }
        }
        let _ = child.kill();
        String::from_utf8_lossy(&output).into_owned()
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
        let shell_launcher = bin.join(SHELL_LAUNCHER_NAME);
        let shell_integration = bin.join(SHELL_INTEGRATION_DIR_NAME);
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
        assert_eq!(fs::read_to_string(&shell_launcher).unwrap(), SHELL_LAUNCHER);
        assert_eq!(
            fs::read_to_string(shell_integration.join("zsh").join(".zshenv")).unwrap(),
            ZSH_ENV
        );
        assert_eq!(
            fs::read_to_string(shell_integration.join("zsh").join("integration.zsh")).unwrap(),
            ZSH_INTEGRATION
        );
        assert_eq!(
            fs::read_to_string(shell_integration.join("bash").join("bootstrap.bash")).unwrap(),
            BASH_BOOTSTRAP
        );
        assert_eq!(
            fs::read_to_string(shell_integration.join("fish").join("config.fish")).unwrap(),
            FISH_INTEGRATION
        );
        assert_eq!(
            fs::read_to_string(shell_integration.join("posix-integration.sh")).unwrap(),
            POSIX_INTEGRATION
        );
        serde_json::from_str::<serde_json::Value>(CLAUDE_PLUGIN_MANIFEST).unwrap();
        serde_json::from_str::<serde_json::Value>(CLAUDE_PLUGIN_HOOKS).unwrap();
        assert!(Command::new("/bin/sh").args(["-n", shell_launcher.to_str().unwrap()])
            .status().unwrap().success());
        assert!(Command::new("/bin/sh")
            .args(["-n", shell_integration.join("posix-integration.sh").to_str().unwrap()])
            .status().unwrap().success());
        assert!(Command::new("/bin/bash")
            .args(["-n", shell_integration.join("bash").join("bootstrap.bash").to_str().unwrap()])
            .status().unwrap().success());
        if Path::new("/bin/zsh").is_file() {
            assert!(Command::new("/bin/zsh")
                .args(["-n", shell_integration.join("zsh").join(".zshenv").to_str().unwrap()])
                .status().unwrap().success());
            assert!(Command::new("/bin/zsh")
                .args(["-n", shell_integration.join("zsh").join("integration.zsh").to_str().unwrap()])
                .status().unwrap().success());
        }
        if Command::new("fish").arg("--version").output().is_ok() {
            assert!(Command::new("fish")
                .args(["-n", shell_integration.join("fish").join("config.fish").to_str().unwrap()])
                .status().unwrap().success());
        }
        ensure_local_shim_in(&bin).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_ne!(fs::metadata(&shim).unwrap().permissions().mode() & 0o111, 0);
            assert_ne!(fs::metadata(&shell_launcher).unwrap().permissions().mode() & 0o111, 0);
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

        // cmux installs a per-surface `claude` shim and injects its own hooks/settings. Inside a
        // Buoy terminal, skip that foreign terminal wrapper and inject only Buoy's plugin.
        let cmux_dir = root.join("cmux-cli-shims").join("surface-1");
        fs::create_dir_all(&cmux_dir).unwrap();
        let cmux = cmux_dir.join("claude");
        fs::write(&cmux, b"#!/bin/sh\nfor arg do printf 'CMUX_ARG=<%s>\\n' \"$arg\"; done\n")
            .unwrap();
        fs::set_permissions(&cmux, fs::Permissions::from_mode(0o755)).unwrap();
        let cmux_path = format!(
            "{}:{}:{}:/usr/bin:/bin",
            cmux_dir.display(),
            shim_dir.display(),
            real_dir.display()
        );
        let cmux_present = Command::new(&shim)
            .arg("hello")
            .env("PATH", &cmux_path)
            .env("HOME", root.join("home"))
            .env("BUOY_TERMINAL", "1")
            .env("CMUX_CLAUDE_WRAPPER_SHIM", &cmux)
            .output()
            .unwrap();
        assert!(cmux_present.status.success());
        let cmux_present_output = String::from_utf8(cmux_present.stdout).unwrap();
        assert!(!cmux_present_output.contains("CMUX_ARG="),
            "Buoy incorrectly entered cmux's wrapper: {cmux_present_output:?}");
        assert_eq!(cmux_present_output.matches("<--plugin-dir>\n").count(), 1,
            "Buoy must inject exactly one plugin: {cmux_present_output:?}");
        assert!(cmux_present_output.contains("<hello>\n"),
            "user argv did not reach the real Claude binary: {cmux_present_output:?}");

        // If an older PATH arrangement makes cmux bounce into Buoy after cmux has already injected
        // its settings, the chain marker reaches the real binary unchanged and does not loop.
        let bounced = Command::new(&shim)
            .arg("hello")
            .env("PATH", &cmux_path)
            .env("HOME", root.join("home"))
            .env("BUOY_TERMINAL", "1")
            .env("cmux_claude_wrapper_reexec_guard", "1")
            .env("CMUX_CLAUDE_WRAPPER_SHIM", &cmux)
            .output()
            .unwrap();
        assert!(bounced.status.success());
        assert_eq!(
            String::from_utf8(bounced.stdout).unwrap(),
            "<hello>\n",
            "a cmux-to-Buoy bounce reaches the real Claude binary exactly once"
        );
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
        assert!(script.contains("buoy_shell=\"$buoy_bin/buoy-shell\""));
        assert!(script.contains("$buoy_shell_integration/zsh/.zshenv"));
        assert!(script.contains("$buoy_shell_integration/bash/bootstrap.bash"));
        assert!(script.contains("$buoy_shell_integration/fish/config.fish"));
        assert!(script.contains("$buoy_shell_integration/posix-integration.sh"));
        assert!(script.contains("PATH=\"$buoy_bin:$PATH\""));
        assert!(script.contains("BUOY_TERMINAL=1"));
        assert!(script.contains("BUOY_SESSION_ID=dev"));
        assert!(script.contains("BUOY_TMUX_BIN=.local/bin/tmux"));
        assert!(script.contains("SHELL=$BUOY_SHELL_LAUNCHER"));
        assert!(script.contains("exec .local/bin/tmux -CC -L dtcc3-7 new-session -D -A -s dev"));
        assert!(script.contains("set-environment -g PATH \"$PATH\""));
        assert!(script.contains("set-environment -g BUOY_TERMINAL 1"));
        assert!(script.contains("set-environment -g BUOY_TMUX_BIN \"$BUOY_TMUX_BIN\""));
        assert!(script.contains("set-option -g default-shell \"$BUOY_REAL_SHELL\""));
        assert!(script.contains("set-option -g default-command 'if [ -x \"$BUOY_SHELL_LAUNCHER\" ]"));
    }

    #[test]
    fn imported_default_server_is_not_reconfigured_or_detached() {
        let script = remote_tmux_script("/usr/bin/tmux", "default", "work", true);
        assert!(script.contains("-CC -L default attach-session -t work"));
        assert!(!script.contains("new-session"));
        assert!(!script.contains(" -D"));
        assert!(!script.contains("default-command"));
        assert!(!script.contains("buoy_install"));
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
        cmd.env("SHELL", "/bin/zsh");
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
        let remote_shell = remote_bin.join(SHELL_INTEGRATION_DIR_NAME);
        let installed_bundle = fs::read(remote_bin.join("claude")).ok().as_deref()
            == Some(CLAUDE_WRAPPER.as_bytes())
            && fs::read(remote_bin.join(SHELL_LAUNCHER_NAME)).ok().as_deref()
                == Some(SHELL_LAUNCHER.as_bytes())
            && fs::read(remote_shell.join("zsh").join(".zshenv")).ok().as_deref()
                == Some(ZSH_ENV.as_bytes())
            && fs::read(remote_shell.join("zsh").join("integration.zsh")).ok().as_deref()
                == Some(ZSH_INTEGRATION.as_bytes())
            && fs::read(remote_shell.join("bash").join("bootstrap.bash")).ok().as_deref()
                == Some(BASH_BOOTSTRAP.as_bytes())
            && fs::read(remote_shell.join("fish").join("config.fish")).ok().as_deref()
                == Some(FISH_INTEGRATION.as_bytes())
            && fs::read(remote_shell.join("posix-integration.sh")).ok().as_deref()
                == Some(POSIX_INTEGRATION.as_bytes())
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

        // Reconnects keep this server. A later tab must use its configured default command and get
        // the same post-startup repair without help from the original client's environment.
        let new_window = Command::new(&probe.tmux_path)
            .args(["-L", &socket, "new-window", "-d", "-t", &session, "-n", "postrc"])
            .status()
            .expect("create a post-bootstrap tmux window");
        assert!(new_window.success(), "new tmux window uses configured default command");
        let target = format!("{session}:postrc");
        let send = Command::new(&probe.tmux_path)
            .args([
                "-L", &socket, "send-keys", "-t", &target, "-l",
                "whence -w claude; printf '__BUOY_NEW_WINDOW_DONE__\\n'",
            ])
            .status()
            .expect("send lookup command to new window");
        assert!(send.success(), "send lookup command");
        let _ = Command::new(&probe.tmux_path)
            .args(["-L", &socket, "send-keys", "-t", &target, "Enter"])
            .status();
        let window_deadline = Instant::now() + Duration::from_secs(5);
        let mut new_window_text = String::new();
        while Instant::now() < window_deadline {
            let capture = Command::new(&probe.tmux_path)
                .args(["-L", &socket, "capture-pane", "-p", "-t", &target])
                .output()
                .expect("capture integrated new window");
            new_window_text = String::from_utf8_lossy(&capture.stdout).into_owned();
            if new_window_text.contains("__BUOY_NEW_WINDOW_DONE__")
                && new_window_text.contains("claude: function")
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
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
        assert!(new_window_text.contains("claude: function"),
            "new tmux windows did not retain post-rc integration: {new_window_text:?}");
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

        let mut hook_command = Command::new("/bin/sh");
        hook_command
            .arg(&hook)
            .env("BUOY_TERMINAL", "1")
            .env("DT_DEBUG", "1")
            .env("TMUX", tmux_env)
            .env("TMUX_PANE", pane_id)
            .env("BUOY_TMUX_BIN", &probe.tmux_path)
            // Deliberately exclude Homebrew/other tmux locations: exact routing must survive an rc
            // file that completely rebuilt PATH.
            .env("PATH", "/usr/bin:/bin");
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

    /// The original bug: Buoy prepended its launcher before zsh started, then the user's rc files
    /// rebuilt PATH and selected another `claude`. Run the real launcher under a real pty with a
    /// custom ZDOTDIR, a hostile PATH rewrite, and noclobber enabled.
    #[cfg(unix)]
    #[test]
    fn tc_cn7_zsh_repairs_claude_lookup_after_user_startup() {
        use std::os::unix::fs::PermissionsExt;

        if !Path::new("/bin/zsh").is_file() {
            eprintln!("SKIP TC-CN7: no /bin/zsh");
            return;
        }
        let root = temp_dir("zsh startup race with spaces");
        let bin = root.join("bin");
        let home = root.join("home");
        let zdotdir = root.join("custom zdotdir");
        let temporary = root.join("temporary files");
        let real_dir = root.join("competing claude");
        for dir in [&home, &zdotdir, &temporary, &real_dir] {
            fs::create_dir_all(dir).unwrap();
        }
        ensure_local_shim_in(&bin).unwrap();
        let real_claude = real_dir.join("claude");
        fs::write(&real_claude, b"#!/bin/sh\nfor arg do printf 'ARG=<%s>\\n' \"$arg\"; done\n").unwrap();
        fs::set_permissions(&real_claude, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(zdotdir.join(".zshenv"), b"print USER_ZSHENV\n").unwrap();
        fs::write(zdotdir.join(".zprofile"), b"print USER_ZPROFILE\n").unwrap();
        fs::write(
            zdotdir.join(".zshrc"),
            b"export PATH=\"$BUOY_TEST_REAL_DIR:/usr/bin:/bin\"\nalias claude='printf WRONG_ALIAS'\nsetopt noclobber\nPS1='BUOY_ZSH> '\nprint USER_ZSHRC\n",
        ).unwrap();

        let output = run_interactive_shell(
            &bin.join(SHELL_LAUNCHER_NAME), "/bin/zsh", &home, &temporary, &real_dir,
            &[("ZDOTDIR", &zdotdir)],
            "whence -w claude\nclaude hello\nprintf '__BUOY_SHELL_DONE__\\n'\nexit\n",
        );
        let plugin = bin.join(CLAUDE_PLUGIN_DIR_NAME);
        assert_eq!(output.matches("USER_ZSHENV").count(), 1, "custom .zshenv loaded once: {output:?}");
        assert_eq!(output.matches("USER_ZPROFILE").count(), 1, "custom .zprofile loaded once: {output:?}");
        assert_eq!(output.matches("USER_ZSHRC").count(), 1, "custom .zshrc loaded once: {output:?}");
        assert!(output.contains("claude: function"), "post-rc function wins lookup: {output:?}");
        assert!(output.contains("ARG=<--plugin-dir>"), "Buoy plugin argument was injected: {output:?}");
        assert!(output.contains(&format!("ARG=<{}>", plugin.display())), "exact plugin loaded: {output:?}");
        assert!(output.contains("ARG=<hello>"), "user argument preserved: {output:?}");
        assert!(temporary.join("buoy-cli-shims").is_dir(), "private shim root created");
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn tc_cn8_bash_repairs_claude_lookup_after_user_profile() {
        use std::os::unix::fs::PermissionsExt;

        if !Path::new("/bin/bash").is_file() {
            eprintln!("SKIP TC-CN8: no /bin/bash");
            return;
        }
        let root = temp_dir("bash startup race with spaces");
        let bin = root.join("bin");
        let home = root.join("home");
        let temporary = root.join("temporary files");
        let real_dir = root.join("competing claude");
        for dir in [&home, &temporary, &real_dir] {
            fs::create_dir_all(dir).unwrap();
        }
        ensure_local_shim_in(&bin).unwrap();
        let real_claude = real_dir.join("claude");
        fs::write(&real_claude, b"#!/bin/sh\nfor arg do printf 'ARG=<%s>\\n' \"$arg\"; done\n").unwrap();
        fs::set_permissions(&real_claude, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(
            home.join(".bash_profile"),
            b"printf 'USER_BASH_PROFILE\\n'\nexport PATH=\"$BUOY_TEST_REAL_DIR:/usr/bin:/bin\"\nalias claude='printf WRONG_ALIAS'\nset -o noclobber\nPS1='BUOY_BASH> '\n",
        ).unwrap();

        let output = run_interactive_shell(
            &bin.join(SHELL_LAUNCHER_NAME), "/bin/bash", &home, &temporary, &real_dir, &[],
            "type -t claude\nclaude hello\nprintf '__BUOY_SHELL_DONE__\\n'\nexit\n",
        );
        let plugin = bin.join(CLAUDE_PLUGIN_DIR_NAME);
        assert_eq!(output.matches("USER_BASH_PROFILE").count(), 1, "profile loaded once: {output:?}");
        assert!(output.contains("function"), "post-profile function wins lookup: {output:?}");
        assert!(output.contains("ARG=<--plugin-dir>"), "Buoy plugin argument was injected: {output:?}");
        assert!(output.contains(&format!("ARG=<{}>", plugin.display())), "exact plugin loaded: {output:?}");
        assert!(output.contains("ARG=<hello>"), "user argument preserved: {output:?}");
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn tc_cn9_posix_env_repairs_claude_lookup_after_profile() {
        use std::os::unix::fs::PermissionsExt;

        let root = temp_dir("posix startup race with spaces");
        let bin = root.join("bin");
        let home = root.join("home");
        let temporary = root.join("temporary files");
        let real_dir = root.join("competing claude");
        for dir in [&home, &temporary, &real_dir] {
            fs::create_dir_all(dir).unwrap();
        }
        ensure_local_shim_in(&bin).unwrap();
        let real_claude = real_dir.join("claude");
        fs::write(&real_claude, b"#!/bin/sh\nfor arg do printf 'ARG=<%s>\\n' \"$arg\"; done\n").unwrap();
        fs::set_permissions(&real_claude, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(
            home.join(".profile"),
            b"printf 'USER_POSIX_PROFILE\\n'\nexport PATH=\"$BUOY_TEST_REAL_DIR:/usr/bin:/bin\"\nalias claude='printf WRONG_ALIAS'\nPS1='BUOY_SH> '\n",
        ).unwrap();

        let output = run_interactive_shell(
            &bin.join(SHELL_LAUNCHER_NAME), "/bin/sh", &home, &temporary, &real_dir, &[],
            "type claude\nclaude hello\nprintf '__BUOY_SHELL_DONE__\\n'\nexit\n",
        );
        let plugin = bin.join(CLAUDE_PLUGIN_DIR_NAME);
        assert_eq!(output.matches("USER_POSIX_PROFILE").count(), 1, "profile loaded once: {output:?}");
        assert!(output.contains("function"), "ENV function wins lookup: {output:?}");
        assert!(output.contains("ARG=<--plugin-dir>"), "Buoy plugin argument was injected: {output:?}");
        assert!(output.contains(&format!("ARG=<{}>", plugin.display())), "exact plugin loaded: {output:?}");
        assert!(output.contains("ARG=<hello>"), "user argument preserved: {output:?}");
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn tc_cn10_zsh_replaces_cmux_wrapper_inside_a_buoy_terminal() {
        use std::os::unix::fs::PermissionsExt;

        if !Path::new("/bin/zsh").is_file() {
            eprintln!("SKIP TC-CN10: no /bin/zsh");
            return;
        }
        let root = temp_dir("cmux coexistence");
        let bin = root.join("bin");
        let home = root.join("home");
        let zdotdir = root.join("zdotdir");
        let temporary = root.join("temporary");
        let real_dir = root.join("real");
        let cmux_dir = root.join("cmux managed");
        for dir in [&home, &zdotdir, &temporary, &real_dir, &cmux_dir] {
            fs::create_dir_all(dir).unwrap();
        }
        ensure_local_shim_in(&bin).unwrap();
        let real_claude = real_dir.join("claude");
        fs::write(
            &real_claude,
            b"#!/bin/sh\nfor arg do printf 'ARG=<%s>\\n' \"$arg\"; done\n",
        )
        .unwrap();
        fs::set_permissions(&real_claude, fs::Permissions::from_mode(0o755)).unwrap();
        let cmux_shim = cmux_dir.join("claude");
        fs::write(
            &cmux_shim,
            b"#!/bin/sh\nfor arg do printf 'CMUX_ARG=<%s>\\n' \"$arg\"; done\n",
        )
        .unwrap();
        fs::set_permissions(&cmux_shim, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(
            zdotdir.join(".zshrc"),
            b"export PATH=\"$BUOY_TEST_REAL_DIR:/usr/bin:/bin\"\nexport CMUX_CLAUDE_WRAPPER_SHIM=\"$BUOY_TEST_CMUX_SHIM\"\nclaude() { \"$CMUX_CLAUDE_WRAPPER_SHIM\" \"$@\"; }\nPS1='BUOY_CMUX> '\n",
        )
        .unwrap();

        let output = run_interactive_shell(
            &bin.join(SHELL_LAUNCHER_NAME),
            "/bin/zsh",
            &home,
            &temporary,
            &real_dir,
            &[("ZDOTDIR", &zdotdir), ("BUOY_TEST_CMUX_SHIM", &cmux_shim)],
            "functions claude\nclaude hello\nprintf '__BUOY_SHELL_DONE__\\n'\nexit\n",
        );
        assert!(
            output.contains("BUOY_CLAUDE_SHIM"),
            "Buoy did not take ownership from cmux inside its terminal: {output:?}"
        );
        assert!(
            !output.contains("CMUX_ARG="),
            "the invocation still entered cmux's wrapper: {output:?}"
        );
        assert!(
            output.matches("ARG=<--plugin-dir>").count() == 1 && output.contains("ARG=<hello>"),
            "the real Claude binary did not receive exactly one Buoy plugin: {output:?}"
        );
        let _ = fs::remove_dir_all(root);
    }
}
