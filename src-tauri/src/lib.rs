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
pub mod probe;
pub mod remote_file;
pub mod supervisor;
pub mod tunnel;
pub mod host_history;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde::Serialize;
use serde_json::json;
use tauri::{AppHandle, Emitter, State};

use control_backend::{BackendConfig, BackendEvent};
use plain_backend::{PlainBackend, PlainConfig, PlainEvent};
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
}

impl Backend {
    fn write(&self, data: &str) {
        match self { Backend::Supervised(s) => s.write(data), Backend::Plain(b) => b.write(data) }
    }
    fn resize(&self, cols: u16, rows: u16) {
        match self { Backend::Supervised(s) => s.resize(cols, rows), Backend::Plain(b) => b.resize(cols, rows) }
    }
    fn kill(&self) {
        // Intentional teardown: the supervisor stops respawning (close), plain just kills.
        match self { Backend::Supervised(s) => s.close(), Backend::Plain(b) => b.kill() }
    }
    // Control-only window ops (no-op on plain).
    fn new_window(&self) { if let Backend::Supervised(s) = self { s.new_window(); } }
    fn select_window(&self, win: &str) { if let Backend::Supervised(s) = self { s.select_window(win); } }
    fn kill_window(&self, win: &str) { if let Backend::Supervised(s) = self { s.kill_window(win); } }
    fn capture_window(&self, win: &str) { if let Backend::Supervised(s) = self { s.capture_window(win); } }
    fn retry(&self) { if let Backend::Supervised(s) = self { s.retry(); } }
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
    config: AppConfig,
}

// Small app config loaded from config.json in the app data dir (§18). No settings UI yet.
#[derive(Clone, serde::Deserialize)]
struct AppConfig {
    #[serde(default = "default_loopback_hosts")]
    loopback_hosts: Vec<String>,
}
fn default_loopback_hosts() -> Vec<String> { vec!["localhost".into(), "127.0.0.1".into()] }
impl Default for AppConfig {
    fn default() -> Self { AppConfig { loopback_hosts: default_loopback_hosts() } }
}
fn load_config() -> AppConfig {
    let path = user_data_dir().join("config.json");
    std::fs::read_to_string(&path).ok()
        .and_then(|s| serde_json::from_str::<AppConfig>(&s).ok())
        .unwrap_or_default()
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
    // App owns the tmux session name: derive a charset-safe one from the id when not supplied.
    let session = match &meta.session {
        Some(s) if validation::validate_session(s).is_ok() => s.clone(),
        _ => {
            let cleaned: String = id.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
            let tail: String = cleaned.chars().rev().take(12).collect::<Vec<_>>().into_iter().rev().collect();
            format!("dt-{}", if tail.is_empty() { "main".into() } else { tail })
        }
    };

    // Probe once for the best tmux (ssh transport) unless a path was already persisted.
    let (mut tmux_path, mut tmux_version) =
        (meta.tmux_path.clone().unwrap_or_else(|| "tmux".into()), meta.tmux_version);
    let want_control = meta.mode.as_deref() == Some("control");
    if meta.tmux_path.is_none() {
        dlog!("create_session: no tmuxPath supplied -> probing {}", meta.host);
        let res = probe::probe_tmux(&meta.host, &[]);
        dlog!("create_session: probe -> path={} version={:?} probed={}", res.tmux_path, res.version, res.probed);
        tmux_path = res.tmux_path;
        tmux_version = res.version;
    } else {
        dlog!("create_session: reusing persisted tmuxPath={} version={:?} (no probe)", tmux_path, tmux_version);
    }

    // Control mode needs tmux >= 3.2; downgrade to plain if older/unknown.
    let mode = if want_control {
        match tmux_version {
            Some((maj, min)) if maj > 3 || (maj == 3 && min >= 2) => "control",
            _ => "plain",
        }
    } else { "plain" };

    let session_meta = SessionMeta {
        id: id.clone(),
        host: meta.host.clone(),
        session: session.clone(),
        transport: meta.transport.clone().unwrap_or_else(|| "ssh".into()),
        mode: mode.into(),
        tmux_path: Some(tmux_path.clone()),
        tmux_version,
        title: meta.title.clone().or_else(|| Some(meta.host.clone())),
        order: 0,
    };

    // Persist (dedupe by id).
    if meta.kind.as_deref() != Some("local") {
        let mut list = state.store.load();
        list.retain(|s| s.id != id);
        let mut m = session_meta.clone();
        m.order = list.len() as i64;
        list.push(m);
        state.store.save(&list);
    }

    // Spawn the backend.
    let backend = if mode == "control" {
        // Control mode runs under the reconnect Supervisor: it respawns ssh and reattaches the
        // SAME tmux session on a network drop, emitting session:state so the UI reflects
        // connecting/reconnecting/dead instead of a dead "closed".
        let app_for_sink = app.clone();
        let id_for_sink = id.clone();
        let app_sink: control_backend::BackendSink = Arc::new(move |ev| {
            emit_backend_event(&app_for_sink, &id_for_sink, ev);
        });
        let app_for_state = app.clone();
        let id_for_state = id.clone();
        let state_sink: supervisor::StateSink = Arc::new(move |st: supervisor::State| {
            let _ = app_for_state.emit("session:state", json!({ "id": id_for_state, "state": st.as_str() }));
        });
        let sup = supervisor::Supervisor::new(
            BackendConfig {
                host: meta.host.clone(), session: session.clone(),
                tmux_path: tmux_path.clone(), tmux_version, base_args: vec![],
            },
            supervisor::SupervisorOpts::default(),
            supervisor::real_backend_factory(),
            app_sink, state_sink,
            Arc::new(|d| std::thread::sleep(d)),
        );
        sup.start(90, 30);
        Backend::Supervised(sup)
    } else {
        let app_for_sink = app.clone();
        let id_for_sink = id.clone();
        let sink: plain_backend::PlainSink = Arc::new(move |ev| {
            match ev {
                PlainEvent::Data { data } => {
                    let _ = app_for_sink.emit("session:data", DataPayload { id: id_for_sink.clone(), window: None, data });
                }
                PlainEvent::Exit => { let _ = app_for_sink.emit("session:exit", json!({ "id": id_for_sink })); }
            }
        });
        let b = PlainBackend::spawn(
            PlainConfig {
                host: meta.host.clone(), session: session.clone(),
                tmux_path: tmux_path.clone(), tmux_version, base_args: vec![],
            }, sink, 90, 30,
        ).map_err(|e| e.to_string())?;
        Backend::Plain(b)
    };

    dlog!("create_session: spawned backend id={} session={} mode={}", id, session, mode);
    if !meta.host.is_empty() { state.hosts.remember(&meta.host); }   // host history for the dialog
    state.sessions.lock().unwrap().insert(id.clone(), Session { backend, meta: session_meta });
    Ok(json!({ "id": id, "session": session, "mode": mode }))
}

#[tauri::command]
fn ui_log(msg: String) {
    dlog::log(&format!("[ui] {}", msg));
}

#[tauri::command]
fn session_input(state: State<AppState>, id: String, data: String) {
    if let Some(s) = state.sessions.lock().unwrap().get(&id) { s.backend.write(&data); }
}

#[tauri::command]
fn session_resize(state: State<AppState>, id: String, cols: u16, rows: u16) {
    if let Some(s) = state.sessions.lock().unwrap().get(&id) { s.backend.resize(cols, rows); }
}

#[tauri::command]
fn session_close(state: State<AppState>, id: String) {
    // Detach: stop the local client, leave the remote tmux running. Its port-forward tunnels die
    // with the client (§18).
    if let Some(s) = state.sessions.lock().unwrap().remove(&id) { s.backend.kill(); }
    state.tunnels.close_session(&id);
    let mut list = state.store.load();
    list.retain(|s| s.id != id);
    state.store.save(&list);
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
        if m.transport == "ssh" {
            let socket = tmux_socket::socket_name(&m.mode, m.tmux_version, &m.session);
            let tmux_path = m.tmux_path.unwrap_or_else(|| "tmux".into());
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

// User-initiated reconnect from a dead session (renderer 'retry').
#[tauri::command]
fn session_retry(state: State<AppState>, id: String) {
    if let Some(s) = state.sessions.lock().unwrap().get(&id) { s.backend.retry(); }
}

// Largest file we'll transport for the viewer's Download-to-local path (DESIGN.md §16). Render
// caps (text 1MB / image 5MB) are enforced renderer-side; this bounds the fetch itself.
const DOWNLOAD_CAP: usize = 50 * 1024 * 1024;

#[derive(Serialize, Clone)]
struct FilePayload { data_b64: String, size: usize, truncated: bool }

// Fetch a clicked path's bytes for the file viewer (§16). Connection params come from the
// VALIDATED store (never the renderer). Remote sessions read over a separate ssh exec; local
// sessions read the local file. base64 in/out keeps it injection- and binary-safe.
#[tauri::command]
fn read_remote_file(state: State<AppState>, id: String, path: String) -> Result<FilePayload, String> {
    // Resolve the session's host/transport from a running session or the persisted store.
    let meta = {
        let sessions = state.sessions.lock().unwrap();
        sessions.get(&id).map(|s| s.meta.clone())
    }.or_else(|| state.store.load().into_iter().find(|s| s.id == id))
     .ok_or_else(|| "unknown session".to_string())?;

    let rf = if meta.host.is_empty() {
        remote_file::read_local_file(&path, DOWNLOAD_CAP)?
    } else {
        // Resolve a relative clicked path against the session's active-pane cwd (§17): pass the
        // tmux socket/session so the remote script can query #{pane_current_path}.
        let ctx = remote_file::TmuxCtx {
            tmux_path: meta.tmux_path.clone().unwrap_or_default(),
            socket: tmux_socket::socket_name(&meta.mode, meta.tmux_version, &meta.session),
            session: meta.session.clone(),
        };
        remote_file::read_remote_file(&meta.host, &path, DOWNLOAD_CAP, &ctx, &[])?
    };
    dlog!("read_remote_file: id={} path={:?} size={} truncated={}", id, path, rf.data.len(), rf.truncated);
    Ok(FilePayload {
        data_b64: validation::base64_encode(&rf.data),
        size: rf.data.len(),
        truncated: rf.truncated,
    })
}

// Download-to-local (§16): show a native save dialog seeded with `suggested_name`, then write the
// decoded bytes. Returns { ok, path?, canceled? }. Runs the dialog on a worker thread (the
// blocking picker must not run on the main/UI thread).
#[tauri::command]
fn save_file(app: AppHandle, data_b64: String, suggested_name: String) -> Result<serde_json::Value, String> {
    use tauri_plugin_dialog::DialogExt;
    let bytes = validation::base64_decode(&data_b64).ok_or_else(|| "bad base64".to_string())?;
    let name = if suggested_name.trim().is_empty() { "download".to_string() } else { suggested_name };

    // blocking_save_file blocks until the user picks; run it off the main thread.
    let path = std::thread::scope(|s| {
        s.spawn(|| app.dialog().file().set_file_name(&name).blocking_save_file()).join().ok().flatten()
    });
    match path {
        Some(fp) => {
            let p = fp.into_path().map_err(|e| e.to_string())?;
            std::fs::write(&p, &bytes).map_err(|e| e.to_string())?;
            dlog!("save_file: wrote {} bytes to {:?}", bytes.len(), p);
            Ok(json!({ "ok": true, "path": p.to_string_lossy() }))
        }
        None => Ok(json!({ "ok": false, "canceled": true })),
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
    json!({ "loopbackHosts": state.config.loopback_hosts })
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
    let (_, lb) = tunnel::classify_loopback(&url, &state.config.loopback_hosts)
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
            config,
        })
        .invoke_handler(tauri::generate_handler![
            list_sessions, create_session, session_input, session_resize,
            session_close, session_kill, session_rename,
            tab_new, tab_select, tab_close, tab_capture, open_external, ui_log,
            read_remote_file, save_file, session_retry,
            open_forwarded_url, get_config, list_tunnels, close_tunnel,
            list_hosts, remember_host
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
