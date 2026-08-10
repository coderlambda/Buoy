# Test Plan

The app is Tauri + Rust (`src-tauri/`) with a strict TypeScript frontend (`ui/src/`). The Electron MVP
and its original JavaScript test suites were deleted when the migration completed (git history has both; the
old plan's TC-V/TC-B/TC-S/TC-P/TC-L case lists went with them — their Rust ports below carry
their own case IDs). Tests are split by what runs **headless & deterministically** here vs.
what needs a **live remote** (opt-in `#[ignore]`d suites).

## What each layer is and how it's tested

| Layer | Module | Test kind | Runnable here? |
|---|---|---|---|
| Input validation / argv build | `src-tauri/src/validation.rs` | pure unit | ✅ |
| Reconnect supervisor (state machine) | `src-tauri/src/supervisor.rs` | unit w/ fake backend + fake clock | ✅ |
| Control-mode protocol + topology | `control_parser.rs`, `window_registry.rs`, `reply_channel.rs`, `tmux_keys.rs` | pure unit | ✅ |
| Session persistence | `src-tauri/src/session_store.rs` | unit (tmp dir) | ✅ |
| Local tmux backend | `src-tauri/src/local_backend.rs` | integration (real pty + real tmux) | ✅ |
| Tunnels / sticky ports | `src-tauri/src/tunnel.rs` | unit (real sockets, no remote) | ✅ |
| Frontend modules (clipboard, file viewer, link plugins) | `ui/src/*.ts` | TypeScript unit (`npm test`) | ✅ |
| Full GUI (rename, reorder, notifications, new-session form, reconnect/reveal repaint, Canvas fallback) | `ui/index.html` + `ui/src/renderer.ts` | real Tauri platform webview + WebDriver | ✅ |
| Real ssh+tmux end-to-end | `src-tauri/tests/live_*.rs` | live host, `#[ignore]`d | ❌ needs `DT_LIVE_HOST` |

Deferred with the feature: backpressure watermarks (the `ack` bridge call is a no-op in the
Tauri port; the Electron-era module and its TC-B suite are in git history).

---

## TypeScript unit tests (`npm test` — the `ui/src/` frontend modules)

- **clipboard.test.ts** — OSC 52 / clipboard handling in `ui/src/terminalTab.ts`.
- **fileViewer.test.ts** — markdown/table/HTML-preview rendering in `ui/src/fileViewerTab.ts` (§16).
- **plugins.test.ts** — `ui/src/plugins.ts` registry + `ui/src/builtinPlugins.ts` link detection
  (URLs, paths, OSC 8, loopback-URL routing, §17–18), plus streaming OSC 9/99/777
  notification parsing (`OSC_NOTIFICATIONS_DESIGN.md`).

---

## Rust unit tests (`cd src-tauri && cargo test --lib`)

### TC-TP — Sticky `ssh -L` local ports (`src/tunnel.rs`, DESIGN.md §18)
A forwarded URL names one specific `localhost:<local>`. These pin the rule that the number never
moves, so a browser tab opened before a network drop still works after it. TC-TP3/4/5 use the
unresolvable host `dt-sticky-test.invalid` (RFC 6761), so each `ssh` spawns and exits immediately —
a deterministic "the connection broke" with no network dependency.

- **TC-TP1** `pick_local_port` hands a free remembered port straight back; falls back when something
  else holds it; treats `None` and `Some(0)` as "no memory" (0 is serde's default, not port 0).
- **TC-TP2** the remembered port survives the two events that used to lose it — the ssh dying
  (`close_session` clears `pid`, **keeps** `local`) and an app restart (reload from disk) — while an
  explicit `close()` *does* forget it. Also asserts the row reads inactive-but-known meanwhile.
- **TC-TP3** `ensure()` returns the **same** local port after the previous tunnel died (the reported
  bug end-to-end, minus a live remote), and a different remote port still gets its own local port.
- **TC-TP4** `reestablish()` re-opens every persisted port on its original local port, and is a
  no-op for a session that never forwarded anything.
- **TC-TP5** a port the user explicitly closed is **not** resurrected by `reestablish()`.
- **TC-TP6** the chosen port reaches the argv that's actually executed:
  `-L 127.0.0.1:<local>:localhost:<remote>`, loopback-bound, `ExitOnForwardFailure=yes` present,
  `--` before the target, `:port` routed to `-p`, and a flag-shaped host rejected. (Asserted through
  `tunnel_argv`, split out of `spawn_tunnel` so this needs no ssh spawn.)

### TC-TR — Reconnect restore policy (`src/lib.rs`, DESIGN.md §18)
- **TC-TR1** `should_restore_tunnels` returns true only on the **2nd and later** `Connected` (the
  first is the initial attach, where persisted-but-closed ports must stay inactive and re-openable),
  never for `Connecting`/`Reconnecting`/`Closed`/`Dead`, and never for an empty host (a local
  session has no remote).

Mutation-verified — see the table in DESIGN.md §18 for the six mutations and what caught each.

### TC-LT — Live tunnel (`tests/live_tunnel.rs`, `#[ignore]`, needs a real host)
```
DT_LIVE_HOST=user@host cargo test --test live_tunnel -- --ignored --nocapture
```
- **TC-LT-sticky** (`live_tunnel_keeps_local_port_across_a_break`) two servers on the remote
  loopback (18055, 18056); open both; `close_session` as the break; assert both really stopped and
  every row reads inactive; `reestablish`; assert both came back on their **pre-break** local ports
  and the pre-break URLs still serve the same content; then that a `close()`d port stays gone.

  Compiles but was **not run** during development — no live host was available (`ssh localhost` →
  Connection refused). This is the only end-to-end claim for §18 that unit tests can't reach.

---

## Automated GUI tests (real Tauri webview; NOT in the `npm test` glob)

Build the test-only Tauri binary and run every GUI suite with:

```
npm run test:ui
npm run measure:renderer     # opt-in Canvas-vs-DOM measurement; not a regression gate
```

The `ui-test` Cargo feature embeds Tauri's WebDriver plugin and a deterministic backend fixture.
It is absent from production binaries. WebdriverIO launches the actual app webview for the host
platform, so the suites exercise the shipped engine rather than Electron/Chromium.

### TC-R — Inline rename (`test/gui-rename.ts`, DESIGN.md §23)
Run this suite alone with:

```
npm run gui-rename
```

Loads the real Vite build of `ui/index.html` + `ui/src/renderer.ts` through the production Tauri adapter and drives the
first click through native WebDriver input. The embedded WKWebView driver does not synthesize the
second click/double-click detail, so the harness completes that OS event sequence in-page after the
native first click; this retains the click/rerender/double-click ordering that pins the original bug.

- **TC-R1** double-clicking the active project's name yields an editor that is present, **connected to
  the live document**, visible, seeded with the current title, and focused. (`connected` is the
  assertion that pins the shipped bug: the editor used to be built on a row `renderSidebar()` had
  already discarded.)
- **TC-R2** type + Enter commits to the backend exactly once, closes the editor, repaints the label.
- **TC-R3** Escape closes the editor and sends nothing; the title is unchanged.
- **TC-R4** an inactive row renames too: editor and commit both target the double-clicked row, and the
  gesture mounts that row exactly once (no per-click duplicate `mount()`).
- **TC-R5** tab strip: the tab editor is live, visible, focused; the tab is selected exactly once.
- **TC-R6** Enter sends `rename-window` to tmux and closes the editor.
- **TC-R7** a `session:state` event for another project arriving mid-typing preserves the draft, the
  caret position, and focus; the edit still commits.

Mutation-verified — see the table in DESIGN.md §23 for the six mutations and the failures each
produced.

### TC-D — Drag-to-reorder (`test/gui-reorder.ts`, DESIGN.md §24)
```
npm run gui-reorder
```
Same Tauri harness as TC-R, with **three** fake projects — the minimum that distinguishes "moved one
slot" from "swapped with the neighbour". Drags are WebDriver input sequences: down, eight move
steps, up. On macOS the embedded driver injects native mouse events but does not promote them back to
PointerEvents, so this suite bridges that automation-only gap in-page while preserving native
hit-testing. Every drag travels `pitch + 6`px because `dropIndexAt` compares strictly and travelling
*exactly* one pitch lands on the neighbour's midpoint (see §24).

- **TC-D0** the rows are **not** HTML5-draggable. This is the shipped bug's fingerprint: `draggable`
  hands the gesture back to the native machinery that swallows it (`+` badge, immovable card).
- **TC-D1** dragging the top row down one slot reorders to `[s2,s1,s3]` and persists it **once**.
- **TC-D2** dragging it back up restores `[s1,s2,s3]` — exercises the opposite shift branch.
- **TC-D3** mid-drag the UI shows where the card will land: `.dragging` on the card, `.reordering` on
  the container, `pointer-events:none` on the lifted card, its transform ≈ the pointer's travel, and
  the displaced card shifted by **exactly one slot** (that gap is the placeholder). Reads the *inline*
  transform, not the computed one — `getComputedStyle` mid-transition reports an intermediate value.
- **TC-D4** `pointercancel` clears the classes and inline transform, leaves the order untouched, and
  persists nothing.
- **TC-D5** a press-release with no movement still selects the project; and a **sub-threshold jitter**
  (1–2px) neither lifts the card nor reorders, and still selects. (The jitter case exists because
  removing `DRAG_THRESHOLD` originally passed.)
- **TC-D6** a completed drag does **not** also switch project.
- **TC-D7** the tab strip reorders horizontally, persists `['@1','@0','@2']`, and doesn't select the
  dragged tab.
- **TC-D8** the trailing `+` is not in the reorderable set: it never shifts, dragging the last tab
  further right changes nothing, and no no-op write is persisted.
- **TC-D10** dragging selects no text (second report: "while moving, the text on other cards will be
  selected"). Asserts both halves, because either alone leaves the bug reachable: `user-select` is
  `none` on both strips **at rest** (a `.reordering`-only rule applies too late — the anchor is placed
  on pointerdown, before the threshold classifies the gesture), and no selection appears at any of the
  10 move steps or after release. Plus the two things that must NOT regress: a rename editor inside a
  row is still selectable (`user-select:text`), and a selection made **elsewhere** (planted in `#term`,
  standing in for terminal output) survives a reorder untouched.
- **TC-D9** a row with a live rename editor is not draggable — pressing its **sub-line** (outside the
  editor, so not covered by the `input, .controls, …` exemption) must not lift or reorder it, and the
  editor stays open. Found by probing §23 against §24, not by reasoning.

Mutation-verified — see the table in DESIGN.md §24 for the eleven mutations, including the three that
passed and what was corrected in response.

### TC-NS — new-session dialog (`test/gui-new-session.ts`)

```
npm run gui-new-session
```

- **TC-NS1** the native Type select uses the themed input geometry/chrome and switches between
  remote fields and the local-shell explanation.
- **TC-NS2** a blank remote host keeps the dialog open, shows an inline validation message, and
  never invokes `create_session`.
- **TC-NS3** Cancel creates nothing; reopening clears stale errors and restores Native tabs to its
  default enabled state.
- **TC-NS4** host history loads through the Tauri adapter, preserves recency order, filters as the
  user types, and selects on mousedown before input blur hides the menu.
- **TC-NS5** remote values are trimmed and mapped to `{ transport:'ssh' }`; a backend downgrade to
  plain mode and returned tmux path/version are adopted without a duplicate create call.
- **TC-NS6** local creation maps to `{ transport:'local' }`, drops a stale hidden Host value, uses
  the default `local` title, and adopts a bare-pty downgrade without showing native tabs.
- **TC-NS7** a rejected Rust create command keeps the dialog open with the backend reason, produces
  no unhandled rejection or phantom row, and can be retried successfully in place.

### TC-N — terminal notification dots (`test/gui-notifications.ts`, `OSC_NOTIFICATIONS_DESIGN.md`)
```
npm run gui-notifications
```
Loads the real UI with one native-tab session and one plain/single-tab session. Backend events are
injected through the same adapter boundary used by Tauri.

- **TC-N1** the UI starts with no session or tab notification dots.
- **TC-N1b** xterm's device-attributes reply is tagged with the tmux window that emitted the query,
  preventing a tab-switch race from injecting a protocol reply into a neighbouring tab.
- **TC-N2** an OSC 777 split across data chunks creates no dot until its terminator, then marks only
  the emitting tab and its session.
- **TC-N3** two unread tabs roll up to one session dot and survive unrelated rerenders.
- **TC-N4** clicking one unread tab clears only that tab; the session remains unread while another
  child is unread.
- **TC-N5** clicking the last unread tab clears the session rollup.
- **TC-N6** a new notification after acknowledgement restores the dots; an already-active tab can
  still be clicked to acknowledge it.
- **TC-N7** Kitty OSC 99 `d=0` fragments wait for completion, final title/body chunks notify, and
  close/control traffic does not.
- **TC-N8** clicking a plain session card acknowledges its sole implicit tab, which has no header.
- **TC-N9** a standalone BEL (Codex's zero-config `auto` fallback) marks the emitting tab/session
  through xterm's bell event and can be acknowledged normally.

The Rust suite adds six Claude Code provisioning guards:

- **TC-CN1** the Buoy-owned launcher is installed executable and repeated installs are idempotent.
- **TC-CN2** interactive Claude gets the Buoy hook plugin while arbitrary arguments, explicit
  `--settings`, and user `--plugin-dir` values remain unchanged; intentional pass-through modes do
  not load it.
- **TC-CN3** SSH bootstrap installs the same launcher/plugin bundle and persists
  PATH/`BUOY_TERMINAL` in tmux.
- **TC-CN4** the exact encoded SSH bootstrap runs under a real pty and reaches tmux control mode;
  this catches accidentally leaving the final tmux process connected to the decoder pipe.
- **TC-CN5** the installed hook script writes one complete OSC 777 notification from inside a real
  tmux pane, and that sequence survives the control-mode `%output` frame.
- **TC-CN6** the actual hook script runs after `setsid()` with pipe-backed stdio, reproducing
  Claude's no-controlling-tty hook process; inherited `TMUX`/`TMUX_PANE` still route OSC 777 to the
  exact pane and through the control-mode client.

---

## Live suites (`src-tauri/tests/live_*.rs`, real hosts, opt-in)

`live_local_tmux` runs unconditionally in `cargo test` (needs only a local tmux). The rest
are `#[ignore]`d and need `DT_LIVE_HOST=user@host` (and where noted `DT_TMUX=/path/to/tmux`):

```
cd src-tauri && DT_LIVE_HOST=user@host cargo test --test <name> -- --ignored --nocapture
```

- **live_control_mode** — connect, second tab, per-window output isolation, tab re-visit.
- **live_local_tmux TC-LT6** — capture/backfill restores tmux's exact cursor row/column without an
  added newline (the Codex/Claude Code reconnect cursor regression).
- **gui-terminal-repaint TC-CR1–3** — the real xterm is fitted/resized before backfill, restores the
  tmux cursor, and echoes the first command beside its prompt instead of on the following row.
- **gui-terminal-repaint TC-P1–5 / TC-R1–4** — hidden-tab writes repaint on same-size reveal;
  visibility/focus recover only the active pane; ordinary output does not invoke recovery; Canvas
  attaches and repaints, while a forced addon failure retains a working DOM pane.
- **control_backend tmux-title regressions** — the exact physical-Enter burst containing
  `ESC k ls ESC \\` never exposes `ls` as terminal text, at every possible chunk split; unrelated
  escape sequences remain byte-for-byte intact.
- **Tauri dev hot reload** — with `DT_DEBUG=1 npx --yes @tauri-apps/cli@2 dev`, reload the frontend
  twice and run `ls`; each reload logs one `replacing live backend`, leaves one tmux client, and the
  command/output are delivered once without a reconnect loop.
- **Second app launch** — start the built Buoy binary, then launch that same binary again. The second
  process exits promptly, the existing main window is focused, there remains one Buoy process and
  one tmux control client per session, and the first process logs no detach/reconnect transition.
- **live_reconnect / live_force_reconnect** — the supervisor reattaches the SAME session.
- **live_remote_file / live_relative_path** — clicked-path preview + cwd resolution (§16–17).
- **live_tunnel** — sticky `ssh -L` ports across a break (§18; see TC-LT above).

## Manual / deferred (documented, not run here)

- mosh / Eternal Terminal transports and their Milestone-0 characterization (TC-M0 in the
  old plan) went to git history with the Electron backends — re-add the cases if/when a
  Rust `et`/`mosh` transport is built.
- WKWebView-specific gesture behavior (the native selection-drag and drag-DnD differences
  documented in DESIGN.md §24) can only be confirmed by hand in the shipped `Buoy.app` —
  the GUI suites run in Chromium.
