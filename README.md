# Buoy

A durable remote terminal: a session-management **sidebar** around remote sessions that
persist server-side (tmux) and **resurface after network drops** — like a buoy pushed under
by a wave, the connection always pops back up. ssh + tmux control mode for native tabs,
clickable paths/URLs, and localhost port-forwarding.

The current app is the **Tauri + Rust** port (`src-tauri/` + `ui/`); the original Electron
MVP lives under `src/`. See `DESIGN.md` for the full, adversarially-reviewed design,
`TAURI_MIGRATION.md` for the port, and `TEST_PLAN.md` for the test matrix.

## Status: MVP

Implemented (macOS; timeout-based connect; no OOB channel / no RSS watchdog — deferred):

- **Electron + xterm.js + node-pty** shell, sidebar, one active terminal view.
- **Pluggable connection backends** behind one contract:
  - `LocalBackend` — a real local shell (works today, no remote needed).
  - `SshTmuxBackend` — **plain `ssh` + `tmux` (DEFAULT; zero install).** Verified
    end-to-end against a live Amazon Linux 2 host (tmux 1.8): work survives a client drop
    and a fresh connection reattaches the same session. This delivers the actual goal —
    "reattach my tmux session when the network resumes or the app restarts" — with no
    server daemon. Durability = tmux (server-side) + supervisor respawn.
  - `MoshTmuxBackend` — `mosh` + `tmux`. Adds live-connection resume (no dropped keystrokes
    mid-blip) and network roaming; needs `mosh-server` on the remote. Argv verified against
    mosh 1.4.0; remote reachability gated on Milestone 0.
  - `EtTmuxBackend` — `et` + `tmux` (TCP-only firewalls; needs `etserver`). Milestone-0 gated.
  - `FakeBackend` — for deterministic supervisor tests.
  - Transport is chosen per remote session in the "New session" dialog and persisted.
  - Session names are **app-generated** (`dt-*`); users pick a host, not a session id.
    A restored session reattaches the SAME name (no duplicates).
  - The app augments PATH (`/opt/homebrew/bin`, etc.) so `mosh`/`et` are found even when
    Electron is launched from Finder; a missing binary surfaces a clear error, not a silent
    failure.
- **Reconnect supervisor** (`src/main/supervisor.js`): exit-0 = intentional (no respawn,
  gated on no-respawn-in-flight), capped exponential backoff, lifetime auth-attempt cap
  (no ssh-lockout storm), `-D` double-attach avoidance, optimistic connect timeout.
- **Input validation / safe argv** (`src/shared/validation.js`): closes remote-shell
  injection (session charset) and argv flag-injection (host decompose + `--`), IPv6 handling.
- **Backpressure** (`src/shared/backpressure.js`): watermark pause/resume, never discards.
- **Persistence** (`src/main/sessionStore.js`): disk-backed, re-validated on load.

## Run

```bash
npm install
npm start          # launch the app
```

Create a **Local shell** session to try it immediately. **Remote** sessions need `tmux`
on the host (+`etserver` if you pick the et transport, or `mosh`+`mosh-server` for the mosh
transport). Mosh is the easier setup (no daemon, no privileged port); et wins on TCP-only
networks. See Milestone 0.

## Test

```bash
npm test           # 78 unit + integration tests (validation, backpressure, supervisor
                   # state machine, persistence, real-node-pty backend, control-mode parser
                   # + reply routing)
npm run smoke      # headless Electron end-to-end (main<->preload<->renderer<->pty)
npm run itest      # live control-mode reconnect (needs HOST=user@host TMUX=/path)
npm run gui-live   # full-GUI reconnect against a live host (HOST/TMUX env)
```

All tests pass. Tests use a fake clock + fake backend for the supervisor (deterministic),
and a real `node-pty` shell for the backend integration tests.

## Debug logging

Control-mode (`-CC`) has intricate attach timing, so debug logging is built in and writes to
a file you (or a helper) can read after reproducing an issue:

- **File:** `/tmp/dt-debug.log` — both main-process (`[DT cc]`) and renderer (`[DT ui]`) lines.
- **Console:** the same renderer lines also appear in DevTools (Cmd+Opt+I → Console).
- **Silence it:** run with `DT_DEBUG=0 npm start`.
- **Reproduce workflow:** `rm -f /tmp/dt-debug.log`, `npm start`, trigger the issue, then read
  `/tmp/dt-debug.log`. It shows attach, pane resolution, capture size, and what's painted.

## Milestone 0 (do before trusting the remote path)

Two behaviors need a live `etserver` host and can't be verified offline
(`TEST_PLAN.md` TC-M0):

1. Does `et -c` propagate tmux's **exit 0** on an in-terminal `Ctrl-b d` detach?
   (Drives the "exit-0 = intentional, no respawn" rule. Fallback: `-e/--noexit`.)
2. Do `tmux -S` control queries survive et's `-t` byte-forward? (Needed only for the
   deferred OOB channel; MVP uses the optimistic connect timeout instead.)

## Deferred past MVP (designed, not built — see DESIGN.md)

- OOB control channel (forwarded tmux socket) for precise connect + lost-session detection.
- RSS watchdog / OOM backstop and the `throttled` state.
- Cross-platform (Windows lacks a local tmux; macOS/Linux first).
- Squat-DoS-hardened socket dir already specced in the canonical command.

## Decision gate (from DESIGN.md §1)

Before investing further, run the **`tmux -CC` control-mode spike**: if it delivers a
clickable session list, this whole app may be unnecessary. The sidebar is the only feature
the plain `et + tmux` baseline can't provide.
