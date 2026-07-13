# MVP Test Plan

Scope: the MVP subset from DESIGN.md (macOS, timeout-based connect, no OOB channel,
no RSS watchdog). Tests are split by what can run **headless & deterministically** here
vs. what needs a **live remote** (deferred to manual Milestone-0 verification).

## What each layer is and how it's tested

| Layer | Module | Test kind | Runnable here? |
|---|---|---|---|
| Input validation / argv build | `src/shared/validation.js` | pure unit | ✅ |
| Backpressure watermarks | `src/shared/backpressure.js` | pure unit | ✅ |
| Reconnect supervisor (state machine) | `src/main/supervisor.js` | unit w/ fake backend + fake clock | ✅ |
| Connection backend contract | `src/main/backends/*` | fake (unit) + local (integration, real pty) | ✅ |
| Session persistence | `src/main/sessionStore.js` | unit (tmp dir) | ✅ |
| Electron wiring (main/preload/renderer) | `src/main`, `src/renderer` | launch smoke test | ⚠️ GUI needs a display |
| Real et+tmux end-to-end | `EtTmuxBackend` | manual Milestone 0 | ❌ needs live etserver |

---

## Automated test cases (run with `npm test`)

### TC-V — Validation (`validation.test.js`)
- **TC-V1** valid session names accepted: `dev`, `web_1`, `a-b`, `S3`.
- **TC-V2** reject leading `-` (flag-injection): `-X`, `-D`, `-`.
- **TC-V3** reject shell metacharacters: `a;b`, `a b`, `$(id)`, `` a`b` ``, `a|b`, `a&b`.
- **TC-V4** reject `.` (tmux target collision): `a.b`, `.hidden`.
- **TC-V5** reject empty / too long.
- **TC-V6** host: accept `example.com`, `10.0.0.1`, `user@host`, `host:2022`.
- **TC-V7** host: reject leading `-` in host and user (`-x`, `-x@h`).
- **TC-V8** host: reject port out of range (`h:0`, `h:99999`), accept 1..65535.
- **TC-V9** IPv6: accept `::1`, `[::1]:2022`, `2001:db8::1`; output strips to bare addr + routes port to `-p`.
- **TC-V10** `buildEtArgs`: `-c` precedes `--`; host is the sole token after `--`; `-t sock:sock` present; `-u`/`-p` decomposed; the tmux payload contains `mkdir -m 700`, `new-session -A -D`, `set-option window-size latest`, and the validated session.
- **TC-V11** `buildEtArgs` refuses to build when any field fails validation (no partial/unsafe argv).

### TC-B — Backpressure (`backpressure.test.js`)
- **TC-B1** stays "flowing" below HIGH watermark.
- **TC-B2** crosses HIGH → emits `pause`; unacked bytes tracked.
- **TC-B3** ACK drains below LOW → emits `resume`.
- **TC-B4** no spurious pause/resume flapping between LOW and HIGH.
- **TC-B5** never drops data (all bytes accounted; write is buffered, not discarded).

### TC-S — Supervisor state machine (`supervisor.test.js`, fake backend + fake clock)
- **TC-S1** spawn → `connecting`; OOB/timeout confirm → `connected`.
- **TC-S2** clean exit 0 → `closed`, **no respawn** (intentional detach).
- **TC-S3** non-zero exit → `reconnecting` → respawn after backoff.
- **TC-S4** backoff is exponential and capped (1,2,4,…,≤30s).
- **TC-S5** dead host: repeated non-zero exits → `dead` after lifetime attempt cap; **no hot-loop** (assert bounded spawn count).
- **TC-S6** intentional close cancels a pending backoff timer (no respawn fires after close).
- **TC-S7** exit-0 rule gated on "no respawn in flight": a `-D`-detached client's exit 0 during a supervisor-triggered reconnect does **not** mark `closed`.
- **TC-S8** connecting timeout → optimistic `connected` (MVP fallback for the deferred OOB channel).
- **TC-S9** user "retry" from `dead` respects the ≥ floor between attempts.
- **TC-S10** every state reachable and exitable; no dead-end.

### TC-P — Persistence (`sessionStore.test.js`)
- **TC-P1** round-trip save/load of `[{id,host,session,title,order}]`.
- **TC-P2** re-validate on load: a tampered/invalid host or session in the file is rejected, not used.
- **TC-P3** corrupt/missing file → empty list, no throw.

### TC-L — Local backend integration (`backend.test.js`, real node-pty)
- **TC-L1** spawn a local shell backend; `echo` output arrives via `onData`.
- **TC-L2** `write` reaches the shell (round-trip a marker).
- **TC-L3** `resize` changes reported `$COLUMNS`/`stty size`.
- **TC-L4** shell `exit` → backend `onExit` fires with the exit code.
- **TC-L5** `kill` terminates the child; `onExit` fires.

---

## Manual / deferred (documented, not run here)

### TC-M0 — Milestone-0 live characterization (needs a real host + etserver)
- **TC-M0.1** `Ctrl-b d` in-terminal detach → confirm `et -c` exit code (drives TC-S2/S7 rule).
- **TC-M0.2** ssh auth fail / host down / tmux-missing → exit codes + streams.
- **TC-M0.3** `tmux -S <fwd.sock>` control queries survive et `-t` byte-forward (OOB channel).

### TC-G — Electron GUI (launch smoke; full GUI needs a display)
- **TC-G1** main process boots, creates a BrowserWindow, preload exposes `terminalAPI` only.
- **TC-G2** renderer mounts xterm + sidebar; a local-backend session renders and echoes input.
- **TC-G3** resize on window change propagates to the pty.
