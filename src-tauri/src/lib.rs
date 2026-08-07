//! Tauri app entry: owns the sessions, exposes the command surface the renderer calls, and
//! forwards backend events to the webview as Tauri events. This replaces the Electron main
//! process + preload IPC (DESIGN.md §4, §6.3).

#[macro_use]
pub mod dlog;
pub mod control_parser;
pub mod window_registry;
pub mod reply_channel;
pub mod tmux_keys;
pub mod tmux_socket;
pub mod validation;
pub mod session_store;
pub mod control_backend;
pub mod plain_backend;
pub mod local_backend;
pub mod transport;
pub mod probe;
pub mod remote_file;
pub mod html_preview;
pub mod supervisor;
pub mod tunnel;
pub mod host_history;
pub mod claude_integration;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde::Serialize;
use serde_json::json;
use tauri::{AppHandle, Emitter, Manager, State};

use control_backend::{BackendConfig, BackendEvent};
use plain_backend::{PlainBackend, PlainConfig, PlainEvent};
use local_backend::{LocalBackend, LocalConfig, LocalEvent};
use session_store::{SessionMeta, SessionStore};

/// Augment PATH like env.js (also used by backends/probe).
pub fn augmented_path() -> String {
    let mut paths: Vec<String> = std::env::var("PATH")
        .unwrap_or_default()
        .split(':')
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect();
    let home = std::env::var("HOME").unwrap_or_default();
    for p in [
        "/opt/homebrew/bin".to_string(),
        "/usr/local/bin".to_string(),
        "/opt/local/bin".to_string(),
        format!("{}/.local/bin", home),
    ] {
        if !p.is_empty() && !paths.contains(&p) {
            paths.push(p);
        }
    }
    paths.join(":")
}

// Control-mode sessions run under a reconnect Supervisor (respawns ssh + reattaches tmux on a
// network drop). Plain sessions keep the single-spawn backend (zero-install fallback).
enum Backend {
    Supervised(Arc<supervisor::Supervisor>),
    Plain(PlainBackend),
    /// A shell on THIS machine: no ssh, no tmux, no reconnect (see local_backend.rs).
    Local(LocalBackend),
}

impl Backend {
    fn write(&self, data: &str, win: Option<&str>) {
        match self {
            Backend::Supervised(s) => s.write_to(data, win),
            Backend::Plain(b) => b.write(data),
            Backend::Local(b) => b.write(data),
        }
    }
    fn resize(&self, cols: u16, rows: u16) {
        match self {
            Backend::Supervised(s) => s.resize(cols, rows),
            Backend::Plain(b) => b.resize(cols, rows),
            Backend::Local(b) => b.resize(cols, rows),
        }
    }
    fn kill(&self) {
        // Intentional teardown: the supervisor stops respawning (close), plain/local just kill.
        match self {
            Backend::Supervised(s) => s.close(),
            Backend::Plain(b) => b.kill(),
            Backend::Local(b) => b.kill(),
        }
    }
    // Control-only window ops (no-op on plain).
    fn new_window(&self) { if let Backend::Supervised(s) = self { s.new_window(); } }
    fn select_window(&self, win: &str) { if let Backend::Supervised(s) = self { s.select_window(win); } }
    fn kill_window(&self, win: &str) { if let Backend::Supervised(s) = self { s.kill_window(win); } }
    fn rename_window(&self, win: &str, title: &str) { if let Backend::Supervised(s) = self { s.rename_window(win, title); } }
    fn capture_window(&self, win: &str) { if let Backend::Supervised(s) = self { s.capture_window(win); } }
    fn retry(&self) { if let Backend::Supervised(s) = self { s.retry(); } }
    fn force_reconnect(&self) { if let Backend::Supervised(s) = self { s.force_reconnect(); } }
}

struct Session {
    backend: Backend,
    meta: SessionMeta,
}

struct AppState {
    sessions: Mutex<HashMap<String, Session>>,
    store: SessionStore,
    tunnels: tunnel::TunnelRegistry,
    hosts: host_history::HostHistory,
    config: Mutex<AppConfig>,
    /// Documents the user opted into running scripts for (§16 HTML preview), served over the
    /// `buoyhtml:` scheme. Empty until an explicit "Enable scripts" click.
    previews: html_preview::PreviewStore,
}

// Small app config loaded from config.json in the app data dir (§18). No settings UI yet.
#[derive(Clone, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct AppConfig {
    #[serde(default = "default_loopback_hosts")]
    loopback_hosts: Vec<String>,
    // §20: last-active project id, restored + focused on next app open.
    #[serde(default)]
    last_active: Option<String>,
}
fn default_loopback_hosts() -> Vec<String> { vec!["localhost".into(), "127.0.0.1".into()] }
impl Default for AppConfig {
    fn default() -> Self { AppConfig { loopback_hosts: default_loopback_hosts(), last_active: None } }
}
fn load_config() -> AppConfig {
    let path = user_data_dir().join("config.json");
    std::fs::read_to_string(&path).ok()
        .and_then(|s| serde_json::from_str::<AppConfig>(&s).ok())
        .unwrap_or_default()
}
fn save_config(cfg: &AppConfig) {
    let dir = user_data_dir();
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(json) = serde_json::to_string_pretty(cfg) {
        let path = dir.join("config.json");
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, json).is_ok() { let _ = std::fs::rename(&tmp, &path); }
    }
}

// §20: validate a user-supplied accent color: "#" + 3/6 hex digits, else None (clears it).
fn sanitize_color(c: Option<&str>) -> Option<String> {
    let c = c?.trim();
    if c.is_empty() { return None; }
    let hex = c.strip_prefix('#')?;
    if (hex.len() == 3 || hex.len() == 6) && hex.chars().all(|d| d.is_ascii_hexdigit()) {
        Some(format!("#{}", hex.to_lowercase()))
    } else {
        None
    }
}

#[derive(Serialize, Clone)]
struct DataPayload { id: String, window: Option<String>, data: String }

#[derive(Serialize, Clone)]
struct WindowPayload {
    id: String,
    action: String,
    window: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    order: Option<Vec<String>>,
}

// --- helpers -------------------------------------------------------------------------------

fn user_data_dir() -> std::path::PathBuf {
    // NOTE: kept as "durable-terminal" (the pre-rename name) so existing users' sessions.json /
    // config.json are NOT orphaned by the rename to Buoy. This dir name isn't user-visible.
    let base = dirs::data_dir().unwrap_or_else(|| std::env::temp_dir());
    base.join("durable-terminal")
}

fn emit_backend_event(app: &AppHandle, id: &str, ev: BackendEvent) {
    match ev {
        BackendEvent::Data { window, data } => {
            let _ = app.emit("session:data", DataPayload { id: id.into(), window: Some(window), data });
        }
        BackendEvent::WindowAdd { window, order } => {
            let _ = app.emit("session:window", WindowPayload {
                id: id.into(), action: "add".into(), window, name: None, order: Some(order) });
        }
        BackendEvent::WindowClose { window, order } => {
            let _ = app.emit("session:window", WindowPayload {
                id: id.into(), action: "close".into(), window, name: None, order: Some(order) });
        }
        BackendEvent::WindowRename { window, name } => {
            let _ = app.emit("session:window", WindowPayload {
                id: id.into(), action: "rename".into(), window, name: Some(name), order: None });
        }
        BackendEvent::WindowActive { window, order } => {
            let _ = app.emit("session:window", WindowPayload {
                id: id.into(), action: "active".into(), window, name: None, order: Some(order) });
        }
        BackendEvent::Ready => { let _ = app.emit("session:ready", json!({ "id": id })); }
        BackendEvent::Exit => { let _ = app.emit("session:exit", json!({ "id": id })); }
    }
}

/// Record that this session's cached tmuxPath/tmuxVersion demonstrably attached, so the next
/// create_session reuses them instead of re-probing (a re-probe can pick a different version, and
/// the version tags the socket, which would strand the live remote server). `once` guards the store
/// write so it happens on the FIRST sign of life only — this runs on a backend event thread, and
/// plain mode calls it for every chunk of output.
/// Which backend a session gets, as the string the renderer keys its UI off:
///   "control" — tmux -CC: native tabs, reconnect supervisor. Needs tmux >= 3.2.
///   "plain"   — tmux raw stream: one implicit tab, still durable (tmux holds the session).
///   "local"   — NO tmux on this machine: a bare pty. No tabs, no reconnect, not persisted.
///
/// A local session is treated exactly like a remote one here (DESIGN.md §5.3b): its tmux speaks the
/// same control protocol, so it earns native tabs and durability on the same version test. Only the
/// no-tmux case is special, and it is a fallback, not the local default.
///
/// The returned mode must be what the backend ACTUALLY runs, because the renderer waits for
/// %window events in control mode — claiming control for a session that can't deliver them hangs the
/// tab strip forever.
fn choose_mode(is_local: bool, local_tmux_found: bool, want_control: bool,
               tmux_version: Option<(u32, u32)>) -> &'static str {
    if is_local && !local_tmux_found { return "local"; }
    if !want_control { return "plain"; }
    match tmux_version {
        Some((maj, min)) if maj > 3 || (maj == 3 && min >= 2) => "control",
        _ => "plain",
    }
}

/// Should a `Connected` state change trigger re-opening this session's port-forward tunnels (§18)?
///
/// Extracted from `create_session`'s state sink so the policy is unit-testable — the sink itself
/// closes over a live `AppHandle` and can't be. Three conditions, each load-bearing:
///   * `Connected` only — Connecting/Reconnecting have no usable link yet, so an ssh -L would just
///     fail; the supervisor will report Connected once the reattach lands.
///   * NOT the first Connected — the initial attach must leave persisted-but-closed ports inactive
///     and re-openable (§18), not silently spawn an ssh per remembered port at startup. `seen`
///     latches on the first call, so this is true only for a genuine RE-connect.
///   * a non-empty host — a local session has no remote to forward from.
fn should_restore_tunnels(state: supervisor::State, seen: &std::sync::atomic::AtomicBool,
                          host: &str) -> bool {
    use std::sync::atomic::Ordering;
    if state != supervisor::State::Connected { return false; }
    // swap must run for every Connected (that's what latches the first one), so keep it ahead of
    // the host check rather than short-circuiting past it.
    let was_connected_before = seen.swap(true, Ordering::Relaxed);
    was_connected_before && !host.is_empty()
}

fn mark_attach_proven(app: &AppHandle, id: &str, once: &std::sync::atomic::AtomicBool) {
    use std::sync::atomic::Ordering;
    if once.swap(true, Ordering::Relaxed) { return; }
    if let Some(state) = app.try_state::<AppState>() {
        state.store.set_attach_ok(id, true);
    }
}

// --- Tauri commands (the renderer's IPC surface; replaces preload.js) ----------------------

#[tauri::command]
fn list_sessions(state: State<AppState>) -> Vec<SessionMeta> {
    state.store.load()
}

#[derive(serde::Deserialize)]
struct CreateArgs {
    id: Option<String>,
    kind: Option<String>,
    host: String,
    session: Option<String>,
    title: Option<String>,
    mode: Option<String>,
    #[serde(default, rename = "tmuxPath")]
    tmux_path: Option<String>,
    #[serde(default, rename = "tmuxVersion")]
    tmux_version: Option<(u32, u32)>,
    transport: Option<String>,
}

#[tauri::command]
fn create_session(app: AppHandle, state: State<AppState>, meta: CreateArgs) -> Result<serde_json::Value, String> {
    dlog!("create_session: host={:?} session={:?} mode={:?} tmuxPath={:?} tmuxVersion={:?}",
        meta.host, meta.session, meta.mode, meta.tmux_path, meta.tmux_version);
    let id = meta.id.clone().unwrap_or_else(|| {
        // millis-since-epoch id
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis().to_string()).unwrap_or_else(|_| "session".into())
    });
    // A webview reload (notably Tauri dev's frontend hot reload) calls create_session again while
    // the Rust process and its AppState remain alive. Replacing the HashMap entry without closing
    // its backend leaves the old Supervisor running on its worker Arcs; old and new `-D` tmux
    // clients then detach each other forever and both can emit the same shell echo to the renderer.
    // Stop a same-id backend before building its replacement. Keep tunnels alive: this is a client
    // reattach, not the user's Close/Kill action.
    let replaced = { state.sessions.lock().unwrap().remove(&id) };
    if let Some(existing) = replaced {
        dlog!("create_session: replacing live backend id={}", id);
        existing.backend.kill();
    }
    // App owns the tmux session name: derive a charset-safe one from the id when not supplied.
    let session = match &meta.session {
        Some(s) if validation::validate_session(s).is_ok() => s.clone(),
        _ => {
            let cleaned: String = id.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
            let tail: String = cleaned.chars().rev().take(12).collect::<Vec<_>>().into_iter().rev().collect();
            format!("dt-{}", if tail.is_empty() { "main".into() } else { tail })
        }
    };

    // Probe for the best tmux (ssh transport). The result is cached in the store, because
    // tmux_version also picks the version-tagged socket: re-probing a WORKING session after a remote
    // upgrade would move it to a new socket and strand the live server. So the cache is trusted only
    // while it is PROVEN — `attach_ok` is set when a backend reaches Ready and cleared on every new
    // attempt. An unproven cache (last attach never produced output, e.g. the remote tmux was
    // removed/downgraded and the path no longer exists) is re-probed instead of pinning the session
    // to a binary that isn't there.
    let (mut tmux_path, mut tmux_version) =
        (meta.tmux_path.clone().unwrap_or_else(|| "tmux".into()), meta.tmux_version);
    let want_control = meta.mode.as_deref() == Some("control");
    // A local session runs tmux on THIS machine (DESIGN.md §5.3b), so it gets the same durability as
    // a remote one: probe locally instead of over ssh (probe_tmux would try to ssh to "" and burn its
    // timeout). `local_tmux` is None when this machine has no tmux at all — the one case that falls
    // back to a bare pty with no durability.
    let is_local = meta.kind.as_deref() == Some("local");
    if is_local {
        if let Err(error) = claude_integration::ensure_local_shim() {
            // Notification integration is an enhancement, not a prerequisite for opening a shell.
            // Keep the session usable on a read-only or unusually configured home directory.
            dlog!("create_session: could not install Claude notification shim: {}", error);
        }
    }
    let mut local_tmux: Option<(String, Option<(u32, u32)>)> = None;
    let cache_proven = meta.tmux_path.is_some()
        && state.store.load().iter().any(|s| s.id == id && s.attach_ok);
    if is_local {
        // Always re-probe locally: it costs one exec of `tmux -V` (no network, no 8s timeout), and
        // unlike the ssh path there is no risk of stranding a server we can't reach afterwards — a
        // socket on this machine is always reachable, and a tmux upgrade should move us to the new
        // version-tagged socket rather than pinning us to a path that may no longer exist.
        let res = probe::probe_local_tmux();
        dlog!("create_session: local probe -> path={} version={:?} probed={}",
            res.tmux_path, res.version, res.probed);
        if res.probed {
            tmux_path = res.tmux_path.clone();
            tmux_version = res.version;
            local_tmux = Some((res.tmux_path, res.version));
        } else {
            dlog!("create_session: no local tmux -> raw pty (session will NOT be durable)");
        }
    } else if !cache_proven {
        let why = if meta.tmux_path.is_none() { "no tmuxPath supplied" } else { "cached tmuxPath unproven" };
        dlog!("create_session: {} -> probing {}", why, meta.host);
        let res = probe::probe_tmux(&meta.host, &[]);
        dlog!("create_session: probe -> path={} version={:?} probed={}", res.tmux_path, res.version, res.probed);
        // A failed probe falls back to bare "tmux"/None. Don't let that DISCARD a cached path we
        // already have: an unreachable host at this instant is not evidence the cache is wrong.
        if res.probed {
            tmux_path = res.tmux_path;
            tmux_version = res.version;
        } else {
            dlog!("create_session: probe failed -> keeping tmuxPath={} version={:?}", tmux_path, tmux_version);
        }
    } else {
        dlog!("create_session: reusing proven tmuxPath={} version={:?} (no probe)", tmux_path, tmux_version);
    }

    let no_local_tmux = is_local && local_tmux.is_none();
    let mode = choose_mode(is_local, local_tmux.is_some(), want_control, tmux_version);
    let session_transport = if is_local { transport::Transport::Local } else { transport::Transport::Ssh };

    let session_meta = SessionMeta {
        id: id.clone(),
        host: meta.host.clone(),
        session: session.clone(),
        // A local session's transport is "local", not ssh: session_kill uses this to decide whether
        // to tear the tmux server down over ssh or with a direct local `tmux kill-session`.
        transport: if is_local { "local".into() } else { meta.transport.clone().unwrap_or_else(|| "ssh".into()) },
        mode: mode.into(),
        tmux_path: Some(tmux_path.clone()),
        tmux_version,
        title: meta.title.clone().or_else(|| Some(meta.host.clone())),
        order: 0,
        attach_ok: false,
        color: None, last_tab: None, tab_order: vec![], tab_colors: Default::default(),
    };

    // Persist (dedupe by id). Reconnecting an EXISTING session must NOT move it — preserve its
    // stored order + user customizations (color/tab prefs) so the sidebar order is stable across
    // restarts and click-to-reconnect. Only a genuinely new session is appended at the end.
    //
    // Local sessions ARE persisted now (§5.3b): their tmux server outlives the app, so the store row
    // is the only way back to it after a restart — exactly as for a remote session. The one exception
    // is the no-tmux fallback, whose shell dies with the app, so a stored row would resurrect nothing.
    let persist = !no_local_tmux;
    if persist {
        let mut list = state.store.load();
        let mut m = session_meta.clone();
        if let Some(existing) = list.iter().find(|s| s.id == id) {
            m.order = existing.order;
            m.color = existing.color.clone();
            m.last_tab = existing.last_tab.clone();
            m.tab_order = existing.tab_order.clone();
            m.tab_colors = existing.tab_colors.clone();
            if m.title.is_none() { m.title = existing.title.clone(); }
        } else {
            m.order = list.iter().map(|s| s.order).max().map_or(0, |mx| mx + 1);
        }
        list.retain(|s| s.id != id);
        list.push(m);
        list.sort_by_key(|s| s.order);
        state.store.save(&list);
    }

    // The store row was just written with attachOk=false (see SessionMeta::attach_ok). The first
    // sign of life from the backend flips it to true, marking the tmuxPath/tmuxVersion pair proven
    // so the next create_session reuses it instead of re-probing (which could move the socket).
    // Once per Session object: the flag only needs setting on the first success, and the store
    // write must not run per data event.
    let proven = Arc::new(std::sync::atomic::AtomicBool::new(false));

    // Spawn the backend.
    let backend = if no_local_tmux {
        // FALLBACK ONLY: this machine has no tmux, so there is nothing to attach to. A bare pty on a
        // local shell — no socket, no supervisor, no reattach (see local_backend.rs). Installing tmux
        // upgrades a local session to the durable path above with no other change.
        let app_for_sink = app.clone();
        let id_for_sink = id.clone();
        let sink: local_backend::LocalSink = Arc::new(move |ev| match ev {
            LocalEvent::Data { data } => {
                let _ = app_for_sink.emit("session:data",
                    DataPayload { id: id_for_sink.clone(), window: None, data });
            }
            // The shell exited (the user typed `exit`, or it crashed). Same event the renderer
            // already handles for plain mode, so the session closes instead of hanging "connected".
            LocalEvent::Exit => {
                let _ = app_for_sink.emit("session:exit", json!({ "id": id_for_sink }));
            }
        });
        let b = LocalBackend::spawn(LocalConfig { shell: None, cwd: None }, sink, 90, 30)?;
        Backend::Local(b)
    } else if mode == "control" {
        // Control mode runs under the reconnect Supervisor: it respawns ssh and reattaches the
        // SAME tmux session on a network drop, emitting session:state so the UI reflects
        // connecting/reconnecting/dead instead of a dead "closed".
        let app_for_sink = app.clone();
        let id_for_sink = id.clone();
        let proven_for_sink = proven.clone();
        let app_sink: control_backend::BackendSink = Arc::new(move |ev| {
            if matches!(ev, BackendEvent::Ready) {
                mark_attach_proven(&app_for_sink, &id_for_sink, &proven_for_sink);
            }
            emit_backend_event(&app_for_sink, &id_for_sink, ev);
        });
        let app_for_state = app.clone();
        let id_for_state = id.clone();
        // §18: the port-forward tunnels are SEPARATE ssh processes, so a network drop kills them along
        // with the control channel — and nothing used to bring them back. The user was left with greyed
        // rows and dead browser tabs until they re-clicked every port. Re-open them (on their original
        // local ports) whenever the session RECONNECTS.
        let host_for_state = meta.host.clone();
        // Only on a RE-connect: the first Connected of a session's life is the initial attach, where
        // persisted-but-closed ports are meant to stay inactive-and-re-openable (§18) rather than
        // silently spawning ssh at startup. `swap` tells us whether we'd already been connected once.
        let seen_connected = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let state_sink: supervisor::StateSink = Arc::new(move |st: supervisor::State| {
            let _ = app_for_state.emit("session:state", json!({ "id": id_for_state, "state": st.as_str() }));
            if should_restore_tunnels(st, &seen_connected, &host_for_state) {
                // Off-thread: each forward spawns an ssh child, and this sink runs on the supervisor's
                // own thread — blocking it would stall the state machine it's reporting for.
                let app2 = app_for_state.clone();
                let id2 = id_for_state.clone();
                let host2 = host_for_state.clone();
                std::thread::spawn(move || {
                    // try_state, not state(): this thread is detached and can outlive app teardown,
                    // where state() would panic (same reason the buoyhtml handler uses try_state).
                    let state = match app2.try_state::<AppState>() { Some(s) => s, None => return };
                    let restored = state.tunnels.reestablish(&id2, &host2, &[]);
                    dlog!("reconnect: restored {} tunnel(s) for {}: {:?}", restored.len(), id2, restored);
                    // Repaint the sidebar rows (grey -> live) without waiting for the 5s probe tick.
                    emit_tunnels(&app2, &state, &id2);
                });
            }
        });
        let sup = supervisor::Supervisor::new(
            BackendConfig {
                host: meta.host.clone(), session: session.clone(),
                tmux_path: tmux_path.clone(), tmux_version, base_args: vec![],
                transport: session_transport,
            },
            supervisor::SupervisorOpts::default(),
            supervisor::real_backend_factory(),
            app_sink, state_sink,
            Arc::new(|d| std::thread::sleep(d)),
            // Monotonic clock in millis for the stable-connection check (Instant can't panic like
            // SystemTime and is immune to wall-clock jumps).
            {
                let base = std::time::Instant::now();
                Arc::new(move || base.elapsed().as_millis() as u64)
            },
        );
        sup.start(90, 30);
        Backend::Supervised(sup)
    } else {
        let app_for_sink = app.clone();
        let id_for_sink = id.clone();
        let proven_for_sink = proven.clone();
        let sink: plain_backend::PlainSink = Arc::new(move |ev| {
            match ev {
                PlainEvent::Data { data } => {
                    // Plain mode has no Ready event; the first byte of output is the equivalent
                    // proof that this tmux path/version actually attached.
                    mark_attach_proven(&app_for_sink, &id_for_sink, &proven_for_sink);
                    let _ = app_for_sink.emit("session:data", DataPayload { id: id_for_sink.clone(), window: None, data });
                }
                PlainEvent::Exit => { let _ = app_for_sink.emit("session:exit", json!({ "id": id_for_sink })); }
            }
        });
        let b = PlainBackend::spawn(
            PlainConfig {
                host: meta.host.clone(), session: session.clone(),
                tmux_path: tmux_path.clone(), tmux_version, base_args: vec![],
                transport: session_transport,
            }, sink, 90, 30,
        ).map_err(|e| e.to_string())?;
        Backend::Plain(b)
    };

    dlog!("create_session: spawned backend id={} session={} mode={}", id, session, mode);
    if !meta.host.is_empty() { state.hosts.remember(&meta.host); }   // host history for the dialog
    state.sessions.lock().unwrap().insert(id.clone(), Session { backend, meta: session_meta });
    // Return the tmux path/version actually used, not the ones asked for: a re-probe (unproven
    // cache) may have picked different ones, and `mode` may have been downgraded to plain. The
    // renderer's cached copy has to follow or its next createSession would re-send the stale pair.
    Ok(json!({
        "id": id, "session": session, "mode": mode,
        "tmuxPath": tmux_path, "tmuxVersion": tmux_version,
    }))
}

#[tauri::command]
fn ui_log(msg: String) {
    dlog::log(&format!("[ui] {}", msg));
}

#[tauri::command]
fn session_input(state: State<AppState>, id: String, data: String, win: Option<String>) {
    if let Some(s) = state.sessions.lock().unwrap().get(&id) {
        s.backend.write(&data, win.as_deref());
    }
}

#[tauri::command]
fn session_resize(state: State<AppState>, id: String, cols: u16, rows: u16) {
    if let Some(s) = state.sessions.lock().unwrap().get(&id) { s.backend.resize(cols, rows); }
}

#[tauri::command]
fn session_close(state: State<AppState>, id: String) {
    // Detach: stop the local client, leave the remote tmux running. Its port-forward tunnels die
    // with the client (§18).
    //
    // The persisted store row is deliberately KEPT — that is the whole difference between detach and
    // kill. The row holds the host, tmux session name, path and title, which is the only way back to
    // the still-running remote session: there is no remote-session discovery, so deleting it here
    // (as this used to) stranded a live tmux session with no way to reattach from the UI.
    if let Some(s) = state.sessions.lock().unwrap().remove(&id) { s.backend.kill(); }
    state.tunnels.close_session(&id);
}

#[tauri::command]
fn session_kill(state: State<AppState>, id: String) -> serde_json::Value {
    // Kill: terminate the remote tmux session and remove it.
    let meta = {
        let mut sessions = state.sessions.lock().unwrap();
        sessions.remove(&id).map(|s| { s.backend.kill(); s.meta })
    }.or_else(|| state.store.load().into_iter().find(|s| s.id == id));
    state.tunnels.forget_session(&id);   // kill removes the session -> forget its persisted ports

    let mut killed_remote = false;
    if let Some(m) = meta {
        let socket = tmux_socket::socket_name(&m.mode, m.tmux_version, &m.session);
        let tmux_path = m.tmux_path.clone().unwrap_or_else(|| "tmux".into());
        if m.transport == "local" {
            // Local tmux server: kill it directly. Without this a "Kill" on a local session would
            // only drop the client and leave the tmux session running, so the row vanished from the
            // sidebar while its server lived on with no way back to it.
            if let Ok(args) = validation::build_local_kill_args(&m.session, &socket) {
                killed_remote = std::process::Command::new(&tmux_path)
                    .args(&args).env("PATH", augmented_path()).status()
                    .map(|s| s.success()).unwrap_or(false);
            }
        } else if m.transport == "ssh" {
            if let Ok(args) = validation::build_kill_args(&m.host, &m.session, &tmux_path, &socket, &[]) {
                let ok = std::process::Command::new("ssh")
                    .args(&args).env("PATH", augmented_path()).status()
                    .map(|s| s.success()).unwrap_or(false);
                killed_remote = ok;
            }
        }
    }
    let mut list = state.store.load();
    list.retain(|s| s.id != id);
    state.store.save(&list);
    json!({ "ok": true, "killedRemote": killed_remote })
}

#[tauri::command]
fn session_rename(state: State<AppState>, id: String, title: String) -> serde_json::Value {
    let clean: String = title.trim().chars().take(80).collect();
    if clean.is_empty() { return json!({ "ok": false }); }
    if let Some(s) = state.sessions.lock().unwrap().get_mut(&id) {
        s.meta.title = Some(clean.clone());
    }
    let mut list = state.store.load();
    if let Some(e) = list.iter_mut().find(|s| s.id == id) {
        e.title = Some(clean.clone());
        state.store.save(&list);
    }
    json!({ "ok": true, "title": clean })
}

// §20: persist a new project ORDER (array of session ids, top-to-bottom). Reorders the store
// to match; unknown ids are ignored, missing ones keep their relative order at the end.
#[tauri::command]
fn reorder_sessions(state: State<AppState>, ids: Vec<String>) {
    let mut list = state.store.load();
    let rank: std::collections::HashMap<&String, usize> =
        ids.iter().enumerate().map(|(i, id)| (id, i)).collect();
    // stable sort: known ids by their new rank, unknown ids after (keeping prior order).
    list.sort_by_key(|s| rank.get(&s.id).copied().unwrap_or(usize::MAX));
    // save() reassigns .order by index, so the array position IS the persisted order.
    state.store.save(&list);
}

// §20: set (or clear, when color is empty) a project's accent color.
#[tauri::command]
fn set_session_color(state: State<AppState>, id: String, color: Option<String>) {
    let clean = sanitize_color(color.as_deref());
    let mut list = state.store.load();
    if let Some(e) = list.iter_mut().find(|s| s.id == id) {
        e.color = clean;
        state.store.save(&list);
    }
}

// §20: remember which project was last active (restored + focused on next app open).
#[tauri::command]
fn set_last_active(state: State<AppState>, id: String) {
    let mut cfg = state.config.lock().unwrap();
    cfg.last_active = Some(id);
    save_config(&cfg);
}

// §20: remember a project's last-active tab (tmux window id), restored when it's reopened.
#[tauri::command]
fn set_last_tab(state: State<AppState>, id: String, win: String) {
    let mut list = state.store.load();
    if let Some(e) = list.iter_mut().find(|s| s.id == id) {
        e.last_tab = Some(win);
        state.store.save(&list);
    }
}

// §20: persist a project's tab order and/or a single tab's color.
#[tauri::command]
fn set_tab_prefs(state: State<AppState>, id: String, tab_order: Option<Vec<String>>,
                 tab_color: Option<(String, Option<String>)>) {
    let mut list = state.store.load();
    if let Some(e) = list.iter_mut().find(|s| s.id == id) {
        if let Some(order) = tab_order { e.tab_order = order; }
        if let Some((win, color)) = tab_color {
            match sanitize_color(color.as_deref()) {
                Some(c) => { e.tab_colors.insert(win, c); }
                None => { e.tab_colors.remove(&win); }
            }
        }
        state.store.save(&list);
    }
}

// Project tab ops (control mode only; no-op on plain).
#[tauri::command]
fn tab_new(state: State<AppState>, id: String) {
    if let Some(s) = state.sessions.lock().unwrap().get(&id) { s.backend.new_window(); }
}
#[tauri::command]
fn tab_select(state: State<AppState>, id: String, win: String) {
    if let Some(s) = state.sessions.lock().unwrap().get(&id) { s.backend.select_window(&win); }
}
#[tauri::command]
fn tab_close(state: State<AppState>, id: String, win: String) {
    if let Some(s) = state.sessions.lock().unwrap().get(&id) { s.backend.kill_window(&win); }
}
#[tauri::command]
fn tab_capture(state: State<AppState>, id: String, win: String) {
    if let Some(s) = state.sessions.lock().unwrap().get(&id) { s.backend.capture_window(&win); }
}
#[tauri::command]
fn tab_rename(state: State<AppState>, id: String, win: String, title: String) {
    if let Some(s) = state.sessions.lock().unwrap().get(&id) { s.backend.rename_window(&win, &title); }
}

// User-initiated reconnect from a dead session (renderer 'retry').
#[tauri::command]
fn session_retry(state: State<AppState>, id: String) {
    if let Some(s) = state.sessions.lock().unwrap().get(&id) { s.backend.retry(); }
}

// User-initiated FORCE reconnect from any state (renderer 'forceReconnect') — reattach now even if
// the session currently looks connected (e.g. a wedged/half-open link after a network change).
#[tauri::command]
fn session_force_reconnect(state: State<AppState>, id: String) {
    if let Some(s) = state.sessions.lock().unwrap().get(&id) { s.backend.force_reconnect(); }
}

// Largest file we'll transport for the viewer's Download-to-local path (DESIGN.md §16). Render
// caps (text 1MB / image 5MB) are enforced renderer-side; this bounds the fetch itself.
const DOWNLOAD_CAP: usize = 50 * 1024 * 1024;

#[derive(Serialize, Clone)]
struct FilePayload { data_b64: String, size: usize, truncated: bool }

// Fetch a clicked path's bytes for the file viewer (§16). Connection params come from the
// VALIDATED store (never the renderer). Remote sessions read over a separate ssh exec; local
// sessions read the local file. base64 in/out keeps it injection- and binary-safe.
//
// `async` + spawn_blocking for the same reason as save_file: a synchronous command body runs on the
// macOS main/UI thread, and this one shells out to ssh (up to an 8s connect timeout, then up to
// DOWNLOAD_CAP bytes of transfer). On the main thread that freezes the entire window — no repaint,
// no input — for the whole fetch. Takes AppHandle rather than State because a borrowed State guard
// can't be held across an await.
#[tauri::command]
async fn read_remote_file(app: AppHandle, id: String, path: String) -> Result<FilePayload, String> {
    // Resolve the session's host/transport from a running session or the persisted store. Done
    // BEFORE the await so no lock guard is held across it.
    let meta = {
        let state = app.state::<AppState>();
        let running = {
            let sessions = state.sessions.lock().unwrap();
            sessions.get(&id).map(|s| s.meta.clone())
        };
        running.or_else(|| state.store.load().into_iter().find(|s| s.id == id))
    }.ok_or_else(|| "unknown session".to_string())?;

    let path_for_task = path.clone();
    let rf = tauri::async_runtime::spawn_blocking(move || {
        if meta.host.is_empty() {
            // Local session: resolve a relative clicked path against the LOCAL tmux pane's cwd, the
            // same §17 behavior remote sessions get (a bare "notes.md" in the terminal means the file
            // in the directory the shell is actually sitting in, not the app's cwd).
            // mode "local" is the no-tmux fallback: there is no server to ask, so leave the ctx
            // empty (which skips the cwd query) rather than shelling out to a socket that can't exist.
            let ctx = if meta.mode == "local" {
                remote_file::TmuxCtx::default()
            } else {
                remote_file::TmuxCtx {
                    tmux_path: meta.tmux_path.clone().unwrap_or_default(),
                    socket: tmux_socket::socket_name(&meta.mode, meta.tmux_version, &meta.session),
                    session: meta.session.clone(),
                }
            };
            let resolved = remote_file::resolve_local_path(&path_for_task, &ctx);
            remote_file::read_local_file(&resolved, DOWNLOAD_CAP)
        } else {
            // Resolve a relative clicked path against the session's active-pane cwd (§17): pass the
            // tmux socket/session so the remote script can query #{pane_current_path}.
            let ctx = remote_file::TmuxCtx {
                tmux_path: meta.tmux_path.clone().unwrap_or_default(),
                socket: tmux_socket::socket_name(&meta.mode, meta.tmux_version, &meta.session),
                session: meta.session.clone(),
            };
            remote_file::read_remote_file(&meta.host, &path_for_task, DOWNLOAD_CAP, &ctx, &[])
        }
    })
    .await
    .map_err(|e| format!("file read task failed: {e}"))??;

    dlog!("read_remote_file: id={} path={:?} size={} truncated={}", id, path, rf.data.len(), rf.truncated);
    Ok(FilePayload {
        data_b64: validation::base64_encode(&rf.data),
        size: rf.data.len(),
        truncated: rf.truncated,
    })
}

// Opt a single HTML document into a SCRIPTS-ENABLED preview (§16), returning the one-shot
// `buoyhtml://localhost/<token>` URL the viewer iframe should load.
//
// Called only from the viewer's explicit "Enable scripts" button — never automatically — because
// running a remote file's JavaScript is a decision the user has to make per file. The bytes come
// from the renderer (it already fetched them via read_remote_file) rather than being re-read here,
// so what runs is exactly what the user previewed and opted into, with no second ssh round-trip.
//
// See html_preview.rs for why this is a separate origin rather than a looser CSP on our own.
#[tauri::command]
fn enable_html_scripts(app: AppHandle, data_b64: String) -> Result<serde_json::Value, String> {
    let bytes = validation::base64_decode(&data_b64).ok_or_else(|| "bad base64".to_string())?;
    let token = app.state::<AppState>().previews.put(bytes);
    dlog!("enable_html_scripts: registered scripted preview token={}", token);
    // `localhost` is a placeholder authority; only the path token selects the document.
    Ok(json!({ "url": format!("{}://localhost/{}", html_preview::SCHEME, token) }))
}

// Download-to-local (§16): show a native save dialog seeded with `suggested_name`, then write the
// decoded bytes. Returns { ok, path?, canceled? }.
//
// This command MUST stay `async`, and MUST NOT use the dialog plugin's `blocking_*` API. Both halves
// of that matter, and getting either wrong hangs the whole app (the bug this shape fixes):
//
//   1. A SYNCHRONOUS #[tauri::command] runs on the thread that delivered the IPC message. On macOS
//      that is wry's WKWebView script-message delegate, which is #[thread_kind = MainThreadOnly] —
//      i.e. the main/UI thread. Declaring the fn `async` makes the macro pick ExecutionContext::Async
//      and dispatch through respond_async_serialized -> async_runtime::spawn, off the main thread.
//   2. `blocking_save_file()` posts the picker via `AppHandle::run_on_main_thread` and then parks on
//      a rendezvous channel waiting for the user's choice. Called FROM the main thread, the closure
//      it just queued can never run — the thread that would drain the event loop is the one blocked
//      on the channel. Deadlock, with the picker never painted: the app appears frozen on click.
//      Spawning a scoped std::thread did not help, because std::thread::scope JOINS before
//      returning, so the main thread still blocked.
//
// So: `async` gets us off the main thread, and the callback form (`save_file(cb)`) leaves the main
// thread free to pump the event loop and actually display the picker. We bridge the callback back to
// this async fn with a oneshot channel, awaited (never blocked on).
#[tauri::command]
async fn save_file(app: AppHandle, data_b64: String, suggested_name: String) -> Result<serde_json::Value, String> {
    use tauri_plugin_dialog::DialogExt;
    let bytes = validation::base64_decode(&data_b64).ok_or_else(|| "bad base64".to_string())?;
    let name = if suggested_name.trim().is_empty() { "download".to_string() } else { suggested_name };

    let (tx, rx) = std::sync::mpsc::channel();
    app.dialog().file().set_file_name(&name).save_file(move |picked| {
        // Fires on whatever thread rfd completes on; just hand the result over. A send error means
        // the receiver was dropped (command canceled), which is fine to ignore.
        let _ = tx.send(picked);
    });

    // Await the picker without blocking a runtime worker: the recv itself is blocking, so it goes on
    // a blocking-pool thread. The picker can take minutes (the user browsing folders).
    let picked = tauri::async_runtime::spawn_blocking(move || rx.recv().ok().flatten())
        .await
        .map_err(|e| format!("save dialog task failed: {e}"))?;

    match picked {
        Some(fp) => {
            let p = fp.into_path().map_err(|e| e.to_string())?;
            std::fs::write(&p, &bytes).map_err(|e| e.to_string())?;
            dlog!("save_file: wrote {} bytes to {:?}", bytes.len(), p);
            Ok(json!({ "ok": true, "path": p.to_string_lossy() }))
        }
        None => {
            dlog!("save_file: canceled by user");
            Ok(json!({ "ok": false, "canceled": true }))
        }
    }
}

// Open a URL in the OS default browser via the opener plugin (scheme-validated — terminal text
// is untrusted, so only safe schemes reach the OS handler).
#[tauri::command]
fn open_external(app: AppHandle, url: String) -> serde_json::Value {
    let ok = url.starts_with("http://") || url.starts_with("https://")
        || url.starts_with("ftp://") || url.starts_with("file:") || url.starts_with("mailto:");
    if ok {
        use tauri_plugin_opener::OpenerExt;
        let _ = app.opener().open_url(&url, None::<&str>);
    }
    json!({ "ok": ok })
}

// Expose config to the renderer (loopback host set for URL classification, §18).
#[tauri::command]
fn get_config(state: State<AppState>) -> serde_json::Value {
    let cfg = state.config.lock().unwrap();
    json!({ "loopbackHosts": cfg.loopback_hosts, "lastActive": cfg.last_active })
}

// Host history for the new-session dialog dropdown (most-recent-first).
#[tauri::command]
fn list_hosts(state: State<AppState>) -> Vec<String> {
    state.hosts.list()
}
#[tauri::command]
fn remember_host(state: State<AppState>, host: String) {
    state.hosts.remember(&host);
}

// Open a remote-loopback URL (localhost:PORT) via an ssh -L tunnel, then open the LOCAL tunnel URL
// in the browser (§18). Reuses a live tunnel for the same (session, remote port). If the URL isn't
// a configured loopback URL, this is a no-op error (the renderer opens plain URLs directly).
#[tauri::command]
fn open_forwarded_url(app: AppHandle, state: State<AppState>, id: String, url: String)
    -> Result<serde_json::Value, String>
{
    let loopback_hosts = state.config.lock().unwrap().loopback_hosts.clone();
    let (_, lb) = tunnel::classify_loopback(&url, &loopback_hosts)
        .ok_or_else(|| "not a loopback URL".to_string())?;
    // Connection params from the VALIDATED store (never the renderer).
    let meta = {
        let sessions = state.sessions.lock().unwrap();
        sessions.get(&id).map(|s| s.meta.clone())
    }.or_else(|| state.store.load().into_iter().find(|s| s.id == id))
     .ok_or_else(|| "unknown session".to_string())?;
    if meta.host.is_empty() {
        return Err("local session has no remote to forward".into());
    }
    let local_port = state.tunnels.ensure(&id, &meta.host, lb.port, &[])?;
    let local_url = format!("http://localhost:{}{}", local_port, lb.path);
    dlog!("open_forwarded_url: {} -> {}", url, local_url);
    emit_tunnels(&app, &state, &id);   // refresh the sidebar's port list
    // Give ssh a beat to establish the forward before the browser hits it.
    std::thread::sleep(std::time::Duration::from_millis(300));
    use tauri_plugin_opener::OpenerExt;
    let _ = app.opener().open_url(&local_url, None::<&str>);
    Ok(json!({ "ok": true, "localUrl": local_url }))
}

// A session's forwarded ports as [{ remote, local, active }] — persisted + live, each probed
// (§18). Inactive/persisted-only ports appear too (local:null, active:false) so the sidebar can
// show them greyed after a restart and let the user re-open or close them.
fn tunnels_json(state: &AppState, id: &str) -> serde_json::Value {
    let list: Vec<serde_json::Value> = state.tunnels.status(id).into_iter()
        .map(|s| json!({ "remote": s.remote, "local": s.local, "active": s.active }))
        .collect();
    json!(list)
}
fn emit_tunnels(app: &AppHandle, state: &AppState, id: &str) {
    let _ = app.emit("session:tunnels", json!({ "id": id, "tunnels": tunnels_json(state, id) }));
}

// List a session's live tunnels (renderer pulls this on demand, e.g. after mount/reconnect).
#[tauri::command]
fn list_tunnels(state: State<AppState>, id: String) -> serde_json::Value {
    tunnels_json(&state, &id)
}

// Close ONE tunnel (by remote port) for a session; re-emit the updated list.
#[tauri::command]
fn close_tunnel(app: AppHandle, state: State<AppState>, id: String, remote: u16) {
    state.tunnels.close(&id, remote);
    emit_tunnels(&app, &state, &id);
}

// Force-forward a remote port onto the SAME local port (localhost:<remote> -> localhost:<remote>).
// Errors (e.g. "local port N is already in use") propagate so the renderer can alert.
#[tauri::command]
fn force_forward(app: AppHandle, state: State<AppState>, id: String, remote: u16) -> Result<serde_json::Value, String> {
    let meta = {
        let sessions = state.sessions.lock().unwrap();
        sessions.get(&id).map(|s| s.meta.clone())
    }.or_else(|| state.store.load().into_iter().find(|s| s.id == id))
     .ok_or_else(|| "unknown session".to_string())?;
    if meta.host.is_empty() { return Err("local session has no remote to forward".into()); }
    let local = state.tunnels.force_same_port(&id, &meta.host, remote, &[])?;
    emit_tunnels(&app, &state, &id);
    Ok(json!({ "ok": true, "local": local }))
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let store = SessionStore::new(user_data_dir().join("sessions.json"));
    let config = load_config();
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .manage(AppState {
            sessions: Mutex::new(HashMap::new()), store,
            tunnels: tunnel::TunnelRegistry::with_store(user_data_dir().join("tunnels.json")),
            hosts: host_history::HostHistory::load(user_data_dir().join("hosts.json")),
            config: Mutex::new(config),
            previews: html_preview::PreviewStore::default(),
        })
        // §16: serve scripts-enabled HTML previews on their OWN origin. Nothing is reachable here
        // until enable_html_scripts registers a token, and each response carries a CSP scoped to
        // that document (see html_preview.rs for the isolation argument).
        .register_uri_scheme_protocol(html_preview::SCHEME, |ctx, req| {
            use tauri::Manager;
            let path = req.uri().path().to_string();
            let (status, csp, body) = match ctx.app_handle().try_state::<AppState>() {
                Some(state) => html_preview::respond(&state.previews, &path),
                None => (500, "default-src 'none'", Vec::new()),
            };
            tauri::http::Response::builder()
                .status(status)
                .header("Content-Type", "text/html; charset=utf-8")
                .header("Content-Security-Policy", csp)
                // Belt and braces: no referrer leakage to the CDNs the page pulls from, and never
                // let the response be sniffed into another type.
                .header("Referrer-Policy", "no-referrer")
                .header("X-Content-Type-Options", "nosniff")
                .body(body)
                .unwrap_or_else(|_| tauri::http::Response::new(Vec::new()))
        })
        .invoke_handler(tauri::generate_handler![
            list_sessions, create_session, session_input, session_resize,
            session_close, session_kill, session_rename,
            reorder_sessions, set_session_color, set_last_active, set_last_tab, set_tab_prefs,
            tab_new, tab_select, tab_close, tab_capture, tab_rename, open_external, ui_log,
            read_remote_file, save_file, enable_html_scripts, session_retry, session_force_reconnect,
            open_forwarded_url, get_config, list_tunnels, close_tunnel, force_forward,
            list_hosts, remember_host
        ])
        // Tunnels are deliberately NOT killed on exit — they keep forwarding, and the next launch
        // ADOPTS the still-alive ones (reuse across restarts). Dead orphans are cleared on load;
        // explicit close/kill/force is the only teardown.
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[cfg(test)]
mod tests {
    use super::*;

    // TC-CM1 the local mode matrix (§5.3b). `choose_mode` is the single branch that decides whether a
    // session gets native tabs + a reconnect supervisor, plain tmux, or the non-durable raw pty — and
    // it's extracted precisely because `create_session` needs a live AppHandle and can't be unit-tested.
    #[test]
    fn tc_cm1_local_mode_matrix() {
        // No tmux on this machine: the raw-pty FALLBACK, regardless of the Native-tabs toggle. This is
        // the only non-durable session type left, so it must not be reachable when tmux IS present.
        assert_eq!(choose_mode(true, false, true, None), "local");
        assert_eq!(choose_mode(true, false, false, None), "local");
        // Even a bogus version can't upgrade a machine with no tmux found.
        assert_eq!(choose_mode(true, false, true, Some((3, 6))), "local");

        // Local + tmux >= 3.2 + toggle on: control mode (native tabs, supervisor, durable).
        assert_eq!(choose_mode(true, true, true, Some((3, 6))), "control");
        assert_eq!(choose_mode(true, true, true, Some((3, 2))), "control", "3.2 is the floor, inclusive");
        assert_eq!(choose_mode(true, true, true, Some((4, 0))), "control");

        // Local + tmux older than 3.2: plain — still tmux, so still durable, just no native tabs.
        assert_eq!(choose_mode(true, true, true, Some((3, 1))), "plain");
        assert_eq!(choose_mode(true, true, true, Some((2, 9))), "plain");
        // Version unknown (tmux -V unparseable) is treated as too old, not assumed capable.
        assert_eq!(choose_mode(true, true, true, None), "plain");

        // Local + tmux + Native tabs OFF: plain, NOT the raw pty — the user opted out of tabs, not
        // out of durability.
        assert_eq!(choose_mode(true, true, false, Some((3, 6))), "plain");
    }

    // TC-CM2 the remote path is unchanged by the local work (regression guard). Note `is_local=false`
    // ignores `local_tmux_found` entirely: a remote session's mode depends only on the toggle and the
    // version probed on the REMOTE host.
    #[test]
    fn tc_cm2_remote_mode_unchanged() {
        assert_eq!(choose_mode(false, false, true, Some((3, 6))), "control");
        assert_eq!(choose_mode(false, false, true, Some((3, 2))), "control");
        assert_eq!(choose_mode(false, false, true, Some((3, 1))), "plain");
        assert_eq!(choose_mode(false, false, true, None), "plain", "unprobed remote falls back to plain");
        assert_eq!(choose_mode(false, false, false, Some((3, 6))), "plain");
        // A remote session can NEVER get the raw-pty local backend, whatever the local tmux state.
        for found in [true, false] {
            for want in [true, false] {
                for v in [None, Some((3, 6)), Some((2, 0))] {
                    assert_ne!(choose_mode(false, found, want, v), "local",
                        "remote never picks the local raw-pty backend");
                }
            }
        }
    }

    // TC-TR1 §18: when a Connected state change should re-open the session's port forwards. The
    // tunnels are separate ssh processes that die with the network, so a reconnect has to bring them
    // back — but NOT on the first connect, where persisted-but-closed ports must stay inactive.
    #[test]
    fn tc_tr1_restore_tunnels_only_on_a_genuine_reconnect() {
        use std::sync::atomic::AtomicBool;
        use supervisor::State;

        let seen = AtomicBool::new(false);
        // First Connected = the initial attach: don't spawn ssh for remembered ports.
        assert!(!should_restore_tunnels(State::Connected, &seen, "me@host"),
            "the first connect must not auto-open persisted ports");
        // Every later Connected is a reconnect (the link dropped and came back).
        assert!(should_restore_tunnels(State::Connected, &seen, "me@host"));
        assert!(should_restore_tunnels(State::Connected, &seen, "me@host"),
            "still restores after repeated drops, not just the first one");

        // Non-Connected states have no usable link yet — an ssh -L now would just fail. They must
        // also NOT consume the first-connect latch.
        let seen2 = AtomicBool::new(false);
        for st in [State::Connecting, State::Reconnecting, State::Dead, State::Closed] {
            assert!(!should_restore_tunnels(st, &seen2, "me@host"), "{:?} does not restore", st);
        }
        assert!(!should_restore_tunnels(State::Connected, &seen2, "me@host"),
            "those states left the first-connect latch untouched");
        assert!(should_restore_tunnels(State::Connected, &seen2, "me@host"));

        // A local session has no remote to forward from. The latch still advances (so this can't
        // masquerade as a first connect later), it just never restores.
        let seen3 = AtomicBool::new(false);
        assert!(!should_restore_tunnels(State::Connected, &seen3, ""));
        assert!(!should_restore_tunnels(State::Connected, &seen3, ""),
            "a hostless session never restores tunnels");
    }

    // TC-EP — augmented_path (ported from the Electron-era test/env.test.js, which was deleted with
    // src/). This is live code: local_backend and the tmux probe both spawn with it, and a GUI app
    // launched from Finder inherits a minimal PATH that lacks /opt/homebrew/bin — which is exactly
    // where tmux lives on Apple Silicon.
    #[test]
    fn tc_ep1_augmented_path_adds_the_common_install_dirs() {
        let p = augmented_path();
        let parts: Vec<&str> = p.split(':').collect();
        for want in ["/opt/homebrew/bin", "/usr/local/bin", "/opt/local/bin"] {
            assert!(parts.contains(&want), "augmented PATH is missing {want}: {p}");
        }
    }

    #[test]
    fn tc_ep2_augmented_path_never_duplicates_an_entry_it_adds() {
        // The inherited PATH is the machine's and may itself contain duplicates (it does on this
        // dev box, which is what made the old JS TC-E2 fail) — that is not ours to fix. What must
        // hold is that we never ADD a directory that was already there.
        let out = augmented_path();
        let parts: Vec<&str> = out.split(':').collect();
        // Each dir we add appears exactly once, whether or not the inherited PATH already had it.
        for dir in ["/opt/homebrew/bin", "/usr/local/bin", "/opt/local/bin"] {
            let n = parts.iter().filter(|p| **p == dir).count();
            assert_eq!(n, 1, "{dir} appears {n} times in {out}");
        }
    }

    #[test]
    fn tc_ep3_augmented_path_preserves_the_inherited_entries_and_their_order() {
        let inherited: Vec<String> =
            std::env::var("PATH").unwrap_or_default().split(':')
                .filter(|s| !s.is_empty()).map(str::to_string).collect();
        let out = augmented_path();
        let parts: Vec<&str> = out.split(':').collect();
        // Every inherited dir survives...
        for dir in &inherited {
            assert!(parts.contains(&dir.as_str()), "dropped inherited PATH entry {dir}");
        }
        // ...and our additions go on the END, so the user's own tools still win.
        let first_inherited = inherited.first().map(|s| s.as_str());
        assert_eq!(parts.first().copied(), first_inherited,
            "the inherited PATH must stay in front; we only append");
        assert!(!out.contains("::") && !out.starts_with(':') && !out.ends_with(':'),
            "no empty PATH segments: {out}");
    }
}
