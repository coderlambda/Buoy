# Buoy Mobile Architecture

## Decision

Buoy Mobile is a separate **application package**, not a repository fork:

```text
@buoy/workspace
├── apps/desktop                 desktop package facade; current source stays at repository root
├── apps/mobile                  mobile package + independent Tauri configuration and app shell
├── packages/contracts          shared TypeScript runtime/session API
├── crates/buoy-core            shared Rust capabilities and input validation
├── ui                           desktop shell + shared session controller and terminal modules
└── src-tauri                   desktop-only process/PTY runtime
```

Separate packages keep store identifiers, permissions, release cadence, native projects, app-shell
markup, and platform dependencies independent. Mobile builds `apps/mobile/ui/index.html` and its own
native-style visual system; it does not build the desktop document or stylesheet. The transport-
neutral session controller, terminal engine, contracts, and remote bootstrap remain shared so the
protocol behavior does not fork. The desktop source can move under `apps/desktop` later; the facade
avoids a noisy move while the boundary is being proven.

## Connection model

Desktop continues to spawn the operating system's `ssh` and `tmux` through a local PTY. Mobile
cannot assume those executables exist, so `apps/mobile/src-tauri` uses `russh` in-process:

```text
shared xterm UI
  -> TerminalAPI invoke/events
  -> mobile Rust runtime
  -> SSH connection over the active VPN route
  -> remote PTY
  -> remote tmux attach (control mode when tmux >= 3.2, plain attach otherwise)
```

The initial command prefers the durable path:

```sh
tmux new-session -D -A -s <Buoy-generated-session>
```

The session name and `[user@]host[:port]` input are validated in `buoy-core` before connecting or
entering a fixed remote command. Mobile requires `user@host`, because it does not read desktop
OpenSSH configuration.

## Session lifecycle and recovery

Detach and Close are intentionally different operations:

- **Detach** stops Buoy's SSH/control client and persisted tunnels but leaves the remote tmux server
  and its processes running. The row stays in Sessions with a detached state and can be reattached.
- **Close** first snapshots every tmux window's title, current working directory, shell, active tab,
  and the last command observed from Buoy input. It then ends the remote tmux session and moves the
  persisted row to History.
- **Resume** reconstructs a new tmux session from the snapshot before the normal attach. Each window
  starts in its saved directory. For bash and zsh, Buoy inserts the saved command into that window's
  interactive history without executing it, so Up recalls it.

“Check open sessions” probes every non-closed session already known to the local store. It marks
detached rows as open/missing and offers one-tap reattach for live tmux sessions. This is deliberately
bounded to known Buoy rows and hosts; it does not scan arbitrary remote `/tmp` socket directories.
On mobile, an already-connected runtime can reuse its in-memory credential without another prompt.
A detached host that accepts SSH `none` authentication can be checked directly; password-only hosts
must be opened first because credentials are never persisted.

## Authentication boundary

There is no Buoy account, cloud session, token, or relay. VPN supplies reachability only; it does
not replace SSH authentication.

Mobile supports an ephemeral SSH password. It crosses the Tauri invoke boundary once, stays only in
the Rust process for reconnects, is excluded from `SessionMeta`, and is cleared from the form. A
restored session tries the SSH `none` method first and, if the server rejects it, asks again through
a masked password field. SSH keys, agents, and credential persistence are intentionally outside this
slice.

Server host keys use persisted trust on first use (TOFU), keyed by host and port. The first observed
SHA-256 fingerprint is stored in `mobile-state.json`; a later mismatch is rejected. Passwords are
never written to that file.

## Capability matrix

| Capability | Desktop | Mobile |
|---|---:|---:|
| Remote SSH terminal | yes | yes |
| Local shell | yes | no |
| Remote tmux durability | yes | yes; tmux is required |
| Native tmux tabs/control mode | yes | yes with tmux >= 3.2; plain fallback |
| Background connection | desktop lifecycle | no; expect suspension |
| Port forwarding | yes | yes; in-process `direct-tcpip` |
| Remote file preview/download | yes | yes |
| Host-key verification | OpenSSH policy | persisted TOFU |
| Credential persistence | OpenSSH/agent | no; by design for this slice |

The renderer reads this matrix from `get_runtime_capabilities`; UI features are hidden by capability,
not by `navigator.userAgent` or target checks scattered through features.

## Mobile UI

The mobile-owned app shell has two navigation states:

1. A full-screen session list.
2. A full-screen terminal with native navigation, compact connection status, horizontally scrolling
   tmux tabs, and an Esc/Ctrl-C/Tab/arrow keyboard accessory row.

The layout uses `100dvh`, safe-area insets, 44-point touch targets, card-based sessions, native-style
bottom sheets, and action sheets instead of hover-only controls. It retains direct terminal typing
and iOS dictation reconciliation rather than introducing a mandatory composer. Local session
creation is removed when the mobile runtime reports it as unsupported. Session behavior such as
tabs, forwarding, preview, download, notifications, reconnect, rename, color, and ordering remains
implemented by shared controller modules.

Terminal output is decoded as a stream at every SSH/tmux boundary. Live `%output`, restored
`capture-pane` data, tmux window names, and plain-mode SSH packets all reassemble UTF-8 before events
cross into the WebView. The mobile terminal uses SF Mono with CJK, symbol, and emoji fallbacks so
box-drawing and international glyphs do not depend on a single installed font.

## Runtime implementation

`apps/mobile/src-tauri` owns the platform-specific transport and persistence:

- `control.rs`: async tmux `-CC` parsing, topology, tab commands, capture/repaint, UTF-8 carry, and
  input gating. Rust-to-WebView terminal output is coalesced at roughly 60 fps.
- `tunnel.rs`: loopback listeners and SSH `direct-tcpip` bridges with sticky local ports.
- `remote.rs`: tmux probing, remote file reads, session discovery, and Close/Resume snapshots.
- `store.rs`: atomic session/preferences/tunnel/TOFU persistence, excluding credentials.
- `preview.rs`: one-document, isolated `buoyhtml:` origins for scripts-enabled HTML previews.

Connections run in the foreground with keepalives and capped exponential reconnect. Persisted
tunnels are reopened after a successful reconnect. Mobile operating systems may suspend sockets
when Buoy is backgrounded, so `backgroundConnection` remains false and foregrounding relies on the
same reconnect path.

## Build and verification

```bash
npm install
npm run mobile:check
npm run mobile:test
npm run tauri:test
npm test
npm run gui-mobile-shell
```

The iOS Xcode project is generated at `apps/mobile/src-tauri/gen/apple`. Running it additionally
requires the iOS Rust targets and an Apple development team/signing certificate:

```bash
npm run mobile:ios:dev
```

Android generation requires Android Studio, SDK, NDK, and the Rust Android targets, then:

```bash
npm run mobile:android:init
npm run mobile:android:dev
```

## Remaining production slices

1. Add SSH key/agent support or optional device Keychain/Keystore credential storage if product
   requirements expand beyond the current no-credential-persistence boundary.
2. Extract the shared remote tmux/bootstrap implementation fully into `buoy-core`; Mobile currently
   compiles the canonical Desktop bootstrap source directly so behavior cannot drift.
3. Add Android generated projects and real-device VPN, keyboard, file-export, and lifecycle test
   matrices. Validate iOS signing and physical-device behavior in the same pass.
