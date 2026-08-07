# Terminal Notification Dots

**Status:** Implemented · **Date:** 2026-08-06

## 1. Goal

Buoy surfaces terminal notification escape sequences and standalone BEL attention signals as
unread dots:

- the tmux-window tab that emitted the notification shows a dot;
- its containing session card shows one rollup dot;
- clicking that exact tab clears its unread state;
- the session dot remains while any other tab in the session is unread;
- after acknowledgement, ordinary output and UI rerenders do not restore the dot—a new,
  complete notification sequence is required.

The UI intentionally shows no notification title, body, history, toast, or native macOS banner.
This feature is an attention locator, not a notification inbox.

## 2. Supported protocols

Buoy recognizes the common terminal notification protocols in the raw PTY stream:

| Protocol | Shape | Completion rule |
| --- | --- | --- |
| OSC 9 | `ESC ] 9 ; message BEL/ST` | A non-empty, terminated message |
| OSC 777 | `ESC ] 777 ; notify ; title ; body BEL/ST` | A terminated `notify` command |
| OSC 99 | `ESC ] 99 ; metadata ; payload ST` | A title/body chunk whose `d` value is not `0` |
| BEL fallback | `BEL` | A standalone terminal bell reported by xterm |

Both 7-bit (`ESC ]`, `ESC \\`) and 8-bit (`0x9d`, `0x9c`) OSC/ST forms are accepted. BEL is
also accepted as a terminator.

OSC 99 is a chunked protocol. A `d=0` title/body fragment is incomplete and does not create an
unread dot. The final fragment (`d=1`, or omitted because `1` is the protocol default) creates one.
Control and response payloads—`p=close`, `p=alive`, `p=?`, icons, and buttons—do not create dots.

Codex enables TUI notifications by default. Its `auto` method uses OSC 9 only for a small terminal
allowlist and otherwise emits a standalone BEL, normally only after receiving a focus-lost event.
Buoy enables tmux `focus-events` on every attach, so Codex receives the focus reports it expects;
xterm's `onBell` event then feeds the same unread state as OSC notifications. No `~/.codex` change
or Codex-specific wrapper is required.

Claude Code also defaults to an `auto` notification channel, but an unrecognized terminal receives
no terminal notification. Buoy does not identify itself as another terminal or depend on Claude's
native terminal allowlist. Instead, it installs an app-owned launcher at
`$HOME/.cache/buoy/bin/claude` and puts that directory first on PATH inside Buoy sessions. The same
bundle contains a small Claude plugin whose `Notification` and `Stop` hooks write one generic OSC
777 request to the terminal pane running Claude. Both lifecycle events intentionally collapse to
the same boolean unread state; Buoy does not inspect, retain, or display Claude's hook payload.

The launcher adds the plugin with Claude's repeatable `--plugin-dir` option. Plugin hooks are merged
by Claude independently from settings-file hooks, so existing user/project hooks and every explicit
`--settings` argument remain unchanged. This avoids relying on the undocumented precedence of
multiple `--settings` flags and avoids requiring Node, Python, or `jq` on SSH hosts. A user's native
Claude notification preference may coexist with the plugin; duplicate terminal requests are
idempotent while the tab's unread state is already true.

The launcher is deliberately scoped and conservative:

- it is active only when `BUOY_TERMINAL=1` is inherited from a Buoy shell;
- it delegates to the real `claude` executable found later on PATH;
- explicit `--settings` and user `--plugin-dir` arguments are preserved;
- `--safe-mode`, `--bare`, print, help, and version invocations pass through because they either
  disable customizations intentionally or do not represent an interactive agent session;
- `BUOY_CLAUDE_NOTIFICATIONS_DISABLED=1` is an environment-level opt-out;
- Buoy never edits `~/.claude/settings.json`.

Local sessions install the launcher and plugin before spawning the shell. Every SSH attach transfers
the same small bundle through the existing connection, then records PATH and `BUOY_TERMINAL` in
tmux's global environment for subsequently created windows. Claude launches command hooks in a new
session with pipe-backed stdio, so the hook cannot use `/dev/tty` even though the Claude parent has a
controlling terminal. The hook instead resolves the exact `#{pane_tty}` from inherited `TMUX` and
`TMUX_PANE` metadata. Outside tmux, or if the matching tmux binary is unavailable, it walks a
bounded ancestor chain with POSIX `ps` to find the nearest real tty. Write/lookup failures remain
non-fatal and report to hook stderr when `DT_DEBUG=1`.

Writing to the resolved pane tty makes tmux naturally attribute the OSC bytes to the pane running
Claude; no Buoy socket, surface ID, or event-routing service is needed. A shell that was already
running before this feature cannot have its process environment rewritten; open a new Buoy tab or
recreate the session once after upgrading. An already-running Claude process must be restarted to
load the plugin.

## 3. Why OSC parsing happens before xterm

The backend already tags control-mode output with its authoritative tmux window ID. The renderer
therefore scans each raw chunk before forwarding it to xterm:

```text
tmux pane output
  -> Rust control backend resolves pane -> window
  -> session:data { session id, window id, bytes }
  -> tab's OSC notification parser
  -> unread state update
  -> unchanged bytes continue into xterm
```

Parsing inside xterm would be too late for tabs that have not been mounted. Background tabs buffer
display data, but their notification dots must appear immediately. The scan is observational: it
does not remove or rewrite bytes.

Each terminal tab owns its own streaming parser. PTY chunk boundaries are arbitrary, so a sequence
may start in one chunk and end in another. Parser state cannot be shared between tabs because an
unfinished sequence from one tmux window must never consume output from another.

Standalone BEL is the exception: xterm consumes it as a terminal control and reports `onBell`, so
`ui/terminalTab.js` forwards that event to the same per-tab unread transition. A BEL that terminates
an OSC is consumed as the OSC terminator and is not reported as a second standalone bell.

Unterminated input is bounded to 16 KiB. This prevents a hostile or malformed OSC from retaining
an unlimited buffer. A nested OSC introducer recovers parsing at the newer sequence.

## 4. State model

Unread state is ephemeral renderer state; it is not persisted across app restarts.

```text
tab.unreadNotification = false
          |
          | complete OSC 9/99/777 notification or standalone BEL
          v
tab.unreadNotification = true
          |
          | user clicks that tab header
          v
tab.unreadNotification = false
```

Repeated notifications while a tab is already unread keep the same boolean dot. Once the user
clears it, a later complete notification moves it back to unread.

The session does not store a second mutable unread flag. Its value is derived every render:

```text
session has unread = any child tab has unreadNotification
```

Derivation prevents the session and its tabs from drifting out of sync. Closing an unread tab
automatically removes its contribution to the session rollup.

## 5. Acknowledgement semantics

Acknowledgement is caused by an explicit user viewing gesture:

- Clicking a native tmux tab clears only that tab, including when it is already active.
- Clicking a session card does not clear native-tab notifications. It reveals the project, after
  which the user can choose the notified tab. This preserves notifications on other tabs.
- Plain/local fallback sessions have one implicit tab and no visible tab strip. Clicking their
  session card acknowledges that sole tab so their dot cannot become impossible to clear.
- Automatic restore, backend-driven active-window changes, incoming output, rename, reconnect,
  and unrelated rerenders never acknowledge a notification.

No timer clears unread state. A dot remains until its user acknowledgement or until the emitting
tab/session is closed.

## 6. Visual design

Notification state uses a 7 px circular dot in the existing accent blue:

- inside the session name row for the session rollup;
- inside the tab label for the per-tab state.

The existing session connection-status dot remains separate. Connection state answers whether the
transport is healthy; the notification dot answers whether a child terminal asked for attention.
No message content is rendered or exposed in a tooltip.

## 7. Trust and security

Terminal output is untrusted. Any process that can write an OSC notification or BEL to the pane can
create a dot. Therefore a notification:

- may only affect ephemeral unread presentation state;
- does not run a command;
- does not grant or deny an agent permission;
- does not send input back to the terminal;
- does not display attacker-controlled title/body text in Buoy chrome.

Future actionable approvals require an authenticated, request-correlated agent channel. They must
not reuse this OSC dot as proof that a real agent issued a permission request.

## 8. Implementation map

- `ui/builtinPlugins.js` owns the pure streaming parser and OSC classification.
- `ui/terminalTab.js` forwards xterm's standalone BEL event.
- `ui/renderer.js` owns per-tab unread state, session aggregation, and acknowledgement behavior.
- `src-tauri/src/validation.rs` enables tmux focus events on every local/remote attach.
- `src-tauri/src/claude_integration.rs` provisions the scoped Claude Code launcher/plugin bundle
  locally and over SSH while preserving explicit user configuration. The remote bootstrap is
  decoded inside command substitution and passed as `/bin/sh -c`'s argument—not piped into the
  shell's stdin—so the final tmux `exec` retains the pty allocated by `ssh -tt` and can start its
  control handshake.
- `src-tauri/src/transport.rs` exposes the launcher PATH and Buoy marker to local tmux windows.
- `ui/index.html` owns the shared dot styling.
- `test/plugins.test.js` covers protocol classification and arbitrary chunk boundaries.
- `test/gui-notifications.js` covers the real renderer behavior and DOM rollup/clearing rules.

## 9. Verification matrix

| Case | Expected result |
| --- | --- |
| Split OSC 777 before terminator | No dot until the terminator arrives |
| Notification from background tmux window | Dot on that tab and its session |
| Two unread tabs | Two tab dots, one session dot |
| Click one of two unread tabs | Clicked dot clears; session dot remains |
| Click last unread tab | Tab and session dots clear |
| Click already-active unread tab | Dot clears |
| Plain output after acknowledgement | Dot stays cleared |
| New notification after acknowledgement | Dot returns |
| OSC 99 `d=0` fragment | No dot |
| OSC 99 final title/body fragment | Dot appears |
| OSC 99 close/query/control message | No dot |
| Plain/single-tab session card click | Its implicit tab is acknowledged |
| Standalone BEL | Dot appears through xterm's bell event |
| Codex default config, background pane | tmux forwards focus loss; Codex BEL creates a dot |
| Claude Code notification/response completion in a new Buoy shell | Scoped hook writes OSC 777; dot appears |
| Claude Code with explicit `--settings` or another `--plugin-dir` | User arguments are unchanged; Buoy hook also loads |
| Claude Code with an explicit native notification channel | Native channel and idempotent Buoy hook may coexist |
| Encoded SSH bootstrap under a real pty | Final tmux process retains tty stdin and emits its control handshake |

## 10. Non-goals

- Persisting notification history or unread state.
- Native operating-system notifications.
- Parsing agent prose or terminal prompts heuristically.
- Showing notification contents.
- Approving agent actions from an OSC event.
- Deduplicating application-level notification IDs across distinct completed emissions.
