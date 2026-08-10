# Buoy

A durable remote terminal: a session-management **sidebar** around remote sessions that
persist server-side (tmux) and **resurface after network drops** — like a buoy pushed under
by a wave, the connection always pops back up. ssh + tmux control mode for native tabs,
clickable paths/URLs, and localhost port-forwarding.

The app is **Tauri v2 + Rust** (`src-tauri/`) with a strict TypeScript frontend (`ui/src/`),
bundled by Vite. See
`DESIGN.md` for the full, adversarially-reviewed design,
`OSC_NOTIFICATIONS_DESIGN.md` for terminal notification behavior, `TAURI_MIGRATION.md` for
the history of the port from the original Electron MVP (deleted from this branch; it
survives in git history and on `main`), and `TEST_PLAN.md` for the test matrix.

## What it does

- **Sessions that survive.** Remote sessions are `ssh` + `tmux`; local sessions are `tmux`
  on this machine. Either way the session lives outside the app, so it survives network
  drops, app restarts, and laptop sleeps — the supervisor reattaches the SAME tmux session
  (no duplicates). Session names are **app-generated** (`dt-*`); users pick a host, not a
  session id.
- **Native tabs** via tmux control mode (`-CC`, needs tmux ≥ 3.2), with a plain non-control
  fallback (zero server-side requirements beyond tmux itself).
- **Reconnect supervisor** (`src-tauri/src/supervisor.rs`): exit-0 = intentional (no
  respawn, gated on no-respawn-in-flight), capped exponential backoff, lifetime
  auth-attempt cap (no ssh-lockout storm), `-D` double-attach avoidance.
- **Input validation / safe argv** (`src-tauri/src/validation.rs`): closes remote-shell
  injection (session charset) and argv flag-injection (host decompose + `--`), IPv6 handling.
- **Persistence** (`src-tauri/src/session_store.rs`): disk-backed, re-validated on load.
- **Clickable paths and URLs** in terminal output: remote file preview/download (§16–17),
  and loopback URLs open through a sticky `ssh -L` tunnel that survives reconnects (§18).
- **Agent notification dots** on the emitting tab and its session. Codex works through its terminal
  bell fallback; Claude Code gets a Buoy-scoped hook plugin without changing global settings.
- The app augments PATH (`/opt/homebrew/bin`, etc.) so `tmux` is found even when the app
  is launched from Finder; a missing binary surfaces a clear error, not a silent failure.

## Run

```bash
npm run tauri:dev      # develop (Vite hot reload + the Rust/Tauri backend)
npm run tauri:build    # release bundle -> src-tauri/target/release/bundle/
                       #   macos/Buoy.app + dmg/Buoy_<version>_<arch>.dmg
```

Create a **Local shell** session to try it immediately — it runs your shell inside a local
`tmux`, so it gets native tabs and survives quitting the app (without `tmux` installed it
still works, as a plain non-persistent pty). **Remote** sessions need `ssh` access and
`tmux` on the host.

## Test

```bash
npm run tauri:test     # Rust unit tests (132): validation, supervisor state machine,
                       # persistence, control-mode parser + reply routing, window registry,
                       # tunnels/sticky ports, local pty backend, Claude launcher integration
npm run typecheck      # strict TypeScript checks for UI, tests, config, and test bridge
npm test               # TypeScript unit tests for the ui/ frontend modules (clipboard, file
                       # viewer, link plugins, TUI activity detection)
npm run test:ui        # all full-GUI suites in the real Tauri platform webview
npm run gui-rename     # full-GUI inline-rename suite (single-suite shortcut)
npm run gui-reorder    # full-GUI drag-to-reorder suite (single-suite shortcut)
npm run gui-notifications # full-GUI OSC/BEL notification-dot and acknowledgement suite
npm run gui-terminal-repaint # reconnect repaint/cursor and command-echo ordering in real xterm
npm run measure:renderer  # opt-in Canvas-vs-DOM WKWebView measurements (fresh process each)
```

The `gui-*` suites build a test-only Tauri binary and drive the Vite build of `ui/index.html` +
`ui/src/renderer.ts` in WKWebView/WebView2/WebKitGTK through WebdriverIO's embedded Tauri driver.
The embedded driver and fixture bridge are excluded from production builds. `cd src-tauri &&
cargo test` additionally has `#[ignore]`d live-host suites — see TEST_PLAN.md.

## Debug logging

Control-mode (`-CC`) has intricate attach timing, so debug logging is built in and writes to
a file you (or a helper) can read after reproducing an issue:

- **File:** `/tmp/dt-debug.log` — both backend (`[DT cc]`) and renderer (`[DT ui]`) lines.
- **Opt-in:** silent unless the app is launched with `DT_DEBUG=1` (so it never costs a
  normal run anything).
- **Reproduce workflow:** `rm -f /tmp/dt-debug.log`, launch with `DT_DEBUG=1`, trigger the
  issue, then read
  `/tmp/dt-debug.log`. It shows attach, pane resolution, capture size, and what's painted.
