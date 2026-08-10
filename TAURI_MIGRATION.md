# Tauri migration (branch `tauri-migration`)

Ports the durable terminal from **Electron + Node main process** to **Tauri v2 + a Rust
backend**, keeping the exact same UX and the xterm.js renderer.

> **The migration is complete and the Electron app has been deleted from this branch**
> (`src/` and its tests; recoverable from git history). The `test/gui-*.ts` suites now run
> inside the real Tauri platform webview through the test-only embedded WebDriver feature;
> Electron is no longer a dependency. This file is kept as the record of what moved where;
> the tables below use the old `src/` paths as historical references.

## Why xterm.js stays

Tauri renders in the OS webview (WKWebView / WebView2 / WebKitGTK), so the terminal UI is still
an HTML/JS surface. Rust terminal crates (`alacritty_terminal`, `vte`, …) are emulation *cores*
that produce a grid, not pixels — using one would mean serializing the grid across IPC and writing
our own canvas renderer, re-implementing the hardest part of xterm for no gain. So the renderer is
unchanged; only the **backend** moved to Rust.

## What moved to Rust (`src-tauri/src/`)

| Rust module | Ports from (JS) | Notes |
|---|---|---|
| `validation.rs` | `shared/validation.js` | host/session charset, ssh/-CC/kill argv, base64 |
| `control_parser.rs` | `shared/controlModeParser.js` | `-CC` protocol -> `ControlEvent` enum |
| `window_registry.rs` | `main/windowRegistry.js` | topology reconcile + diff |
| `reply_channel.rs` | `main/replyChannel.js` | positional reply correlation (`ReplyKind` tags) |
| `tmux_keys.rs` | `main/tmuxKeys.js` | shell input -> `send-keys` lines |
| `tmux_socket.rs` | `shared/tmuxSocket.js` | version-tagged socket name |
| `session_store.rs` | `main/sessionStore.js` | untrusted-on-load session list |
| `probe.rs` | `main/probeTmux.js` | pick best remote tmux |
| `control_backend.rs` | `main/backends/controlModeBackend.js` | ssh via `portable-pty`, reader thread, event coordinator |
| `plain_backend.rs` | `main/backends/sshTmuxBackend.js` | raw ssh+tmux stream |
| `lib.rs` | `main/main.js` + `preload/preload.js` | Tauri commands + event emission |

The renderer was initially copied verbatim, then migrated to strict TypeScript under `ui/src/`.
`ui/src/tauri-api.ts` recreates `window.terminalAPI` over Tauri `invoke`/`listen`; Vite bundles the
frontend while xterm remains vendored under `ui/vendor/` (CSP is `'self'`).

## IPC surface (Tauri commands)

`list_sessions, create_session, session_input, session_resize, session_close, session_kill,
session_rename, tab_new, tab_select, tab_close, tab_capture, open_external`.
Events emitted to the webview: `session:data`, `session:window`, `session:ready`, `session:exit`.

Data is emitted as `{ id, window?, data }` — control mode sets `window`; plain mode omits it.

## Build & run

Prereqs: Rust (rustup) + `cargo install tauri-cli`.

```
npm run tauri:dev      # run the app (compiles Rust, opens the webview)
npm run tauri:build    # release bundle
npm run tauri:test     # Rust unit tests
# live end-to-end (opt-in, needs a reachable host with tmux >= 3.2):
DT_LIVE_HOST=user@host DT_TMUX=/home/u/.local/bin/tmux \
  (cd src-tauri && cargo test --test live_control_mode -- --ignored --nocapture)
```

## Status / verification

- **132 Rust unit tests pass** (parser, registry, reply channel, tmux keys, socket, validation,
  probe, supervisor, session store, local backend, tunnels, PATH augmentation) — ports of the
  JS suites plus the features added since (see DESIGN.md §16–§24).
- **Live suites** (`#[ignore]`d, opt-in with `DT_LIVE_HOST`): control-mode end-to-end,
  reconnect, force-reconnect, remote file, relative paths, tunnels; `live_local_tmux` runs
  unconditionally (needs only a local tmux).
- Release bundle builds, launches, and quits cleanly (`Buoy.app` + `.dmg`, DESIGN.md §24 era).

## Since ported (originally deferred)

- `supervisor.rs` — reconnect/backoff state machine, including `retry`/`force_reconnect`.
- `local_backend.rs` — local tmux sessions (§5.3b).

## Still not ported (deferred)

- `backpressure` ACK flow — `ack` is a no-op in `ui/src/tauri-api.ts` (webview keeps up for
  interactive use). The Electron implementation is in git history (`src/shared/backpressure.js`).
- mosh/et transports — ssh only for now (argv-verified JS backends are in git history).
