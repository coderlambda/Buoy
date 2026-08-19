//! Buoy's mobile runtime. It shares the renderer contract with Desktop, but implements every
//! remote capability in-process: SSH, tmux control mode, reconnect supervision, direct-tcpip
//! forwarding, remote file reads, native export, and persisted session preferences.

mod control;
mod model;
mod preview;
mod remote;
#[cfg(test)]
mod ssh_tests;
mod store;
mod tunnel;

// The Claude/shell bootstrap is identical on Desktop and Mobile because it runs on the remote
// host. Keep one production source until it is moved wholesale into buoy-core; the test build uses
// a tiny deterministic stand-in so Desktop's platform-specific integration tests are not imported.
#[cfg(not(test))]
#[allow(dead_code)]
#[path = "../../../../src-tauri/src/claude_integration.rs"]
mod claude_integration;
#[cfg(test)]
mod claude_integration {
    pub fn remote_tmux_script(
        tmux_path: &str,
        socket: &str,
        session: &str,
        control: bool,
    ) -> String {
        let control = if control { " -CC" } else { "" };
        format!("exec {tmux_path}{control} -L {socket} new-session -D -A -s {session}")
    }
}

// Required by the shared remote bootstrap source. It deliberately exposes only the encoding
// primitive that source needs; Mobile never imports Desktop's argv/process validation layer.
mod validation {
    use base64::Engine;

    #[allow(dead_code)]
    pub fn base64_encode(bytes: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }
}

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use control::{Action as ControlAction, ControlEngine};
use model::{CreateArgs, RecoveryTab, SessionMeta};
use russh::keys::ssh_key::HashAlg;
use russh::{Channel, ChannelMsg, Disconnect, client};
use serde::Serialize;
use serde_json::json;
use store::MobileStore;
use tauri::{AppHandle, Emitter, Manager, State};
use tokio::sync::{mpsc, oneshot};
use tunnel::{ActiveTunnel, TunnelBook};

#[derive(Clone)]
struct ConnectionSpec {
    meta: SessionMeta,
    password: Option<String>,
}

enum SessionCommand {
    Input {
        data: String,
        window: Option<String>,
    },
    Resize {
        cols: u32,
        rows: u32,
    },
    Detach,
    KillRemote,
    CloseRemote {
        hints: Vec<RecoveryTab>,
        reply: oneshot::Sender<Result<(), String>>,
    },
    TabNew,
    TabSelect(String),
    TabClose(String),
    TabCapture(String),
    TabRename {
        window: String,
        title: String,
    },
    EnsureTunnel {
        remote: u16,
        prefer_same_port: bool,
        reply: oneshot::Sender<Result<u16, String>>,
    },
    CloseTunnel(u16),
    ReadFile {
        path: String,
        reply: oneshot::Sender<Result<(Vec<u8>, bool), String>>,
    },
}

struct MobileSession {
    runtime_id: u64,
    spec: ConnectionSpec,
    commands: mpsc::UnboundedSender<SessionCommand>,
}

struct AppState {
    sessions: Mutex<HashMap<String, MobileSession>>,
    store: Arc<MobileStore>,
    tunnels: Arc<TunnelBook>,
    previews: preview::PreviewStore,
    next_runtime_id: AtomicU64,
}

#[derive(Clone, Serialize)]
struct DataPayload {
    id: String,
    window: Option<String>,
    data: String,
}

#[derive(Clone)]
struct SshHandler {
    store: Arc<MobileStore>,
    endpoint: String,
}

impl client::Handler for SshHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &russh::keys::PublicKey,
    ) -> Result<bool, Self::Error> {
        let fingerprint = server_public_key.fingerprint(HashAlg::Sha256).to_string();
        Ok(self
            .store
            .check_or_remember_host_key(&self.endpoint, &fingerprint)
            .unwrap_or(false))
    }
}

fn emit_state(app: &AppHandle, id: &str, state: &str) {
    let _ = app.emit("session:state", json!({ "id": id, "state": state }));
}

fn emit_data(app: &AppHandle, id: &str, window: Option<String>, data: impl Into<String>) {
    let _ = app.emit(
        "session:data",
        DataPayload {
            id: id.into(),
            window,
            data: data.into(),
        },
    );
}

fn queue_data(
    pending: &mut BTreeMap<Option<String>, String>,
    window: Option<String>,
    data: impl AsRef<str>,
) {
    pending.entry(window).or_default().push_str(data.as_ref());
}

fn flush_data(app: &AppHandle, id: &str, pending: &mut BTreeMap<Option<String>, String>) {
    for (window, data) in std::mem::take(pending) {
        if !data.is_empty() {
            emit_data(app, id, window, data);
        }
    }
}

fn emit_window(
    app: &AppHandle,
    id: &str,
    action: &str,
    window: String,
    name: Option<String>,
    order: Option<Vec<String>>,
) {
    let _ = app.emit(
        "session:window",
        json!({ "id": id, "action": action, "window": window, "name": name, "order": order }),
    );
}

fn emit_tunnels(app: &AppHandle, state: &AppState, id: &str) {
    let _ = app.emit(
        "session:tunnels",
        json!({ "id": id, "tunnels": state.tunnels.list(id) }),
    );
}

async fn authenticate(
    mut ssh: client::Handle<SshHandler>,
    user: String,
    password: Option<&str>,
) -> Result<client::Handle<SshHandler>, String> {
    let result = match password.filter(|password| !password.is_empty()) {
        Some(password) => ssh
            .authenticate_password(user, password)
            .await
            .map_err(|error| format!("SSH authentication failed: {error}"))?,
        None => ssh
            .authenticate_none(user)
            .await
            .map_err(|error| format!("SSH authentication failed: {error}"))?,
    };
    if result.success() {
        Ok(ssh)
    } else {
        Err("SSH authentication was rejected; reconnect with the host's SSH password".into())
    }
}

async fn send_control_actions(
    app: &AppHandle,
    id: &str,
    channel: &mut Channel<russh::client::Msg>,
    actions: Vec<ControlAction>,
    pending_data: &mut BTreeMap<Option<String>, String>,
) -> Result<bool, String> {
    for action in actions {
        match action {
            ControlAction::Send(command) => {
                channel
                    .data(format!("{command}\n").as_bytes())
                    .await
                    .map_err(|error| format!("tmux control write failed: {error}"))?
            }
            ControlAction::Data { window, data } => {
                queue_data(pending_data, Some(window), data);
            }
            ControlAction::WindowAdd { window, order } => {
                flush_data(app, id, pending_data);
                emit_window(app, id, "add", window, None, Some(order));
            }
            ControlAction::WindowClose { window, order } => {
                flush_data(app, id, pending_data);
                emit_window(app, id, "close", window, None, Some(order));
            }
            ControlAction::WindowRename { window, name } => {
                flush_data(app, id, pending_data);
                emit_window(app, id, "rename", window, Some(name), None);
            }
            ControlAction::WindowActive { window, order } => {
                flush_data(app, id, pending_data);
                emit_window(app, id, "active", window, None, Some(order));
            }
            ControlAction::Ready => {
                flush_data(app, id, pending_data);
                let _ = app.emit("session:ready", json!({ "id": id }));
            }
            ControlAction::Exit => {
                flush_data(app, id, pending_data);
                return Ok(true);
            }
        }
    }
    Ok(false)
}

struct TunnelContext<'a> {
    app: &'a AppHandle,
    state: &'a AppState,
    id: &'a str,
    accepted: &'a mpsc::UnboundedSender<tunnel::AcceptedTunnel>,
}

impl TunnelContext<'_> {
    async fn ensure(
        &self,
        remote: u16,
        preferred: Option<u16>,
        strict_preferred: bool,
        active: &mut BTreeMap<u16, ActiveTunnel>,
    ) -> Result<u16, String> {
        if let Some(tunnel) = active.get(&remote) {
            if !strict_preferred || preferred == Some(tunnel.local) {
                return Ok(tunnel.local);
            }
        }
        if let Some(tunnel) = active.remove(&remote) {
            tunnel.close();
            self.state.tunnels.deactivate(self.id, remote);
        }
        let tunnel =
            tunnel::start_listener(remote, preferred, strict_preferred, self.accepted.clone())
                .await?;
        let local = tunnel.local;
        self.state.tunnels.activate(self.id, remote, local)?;
        active.insert(remote, tunnel);
        emit_tunnels(self.app, self.state, self.id);
        Ok(local)
    }
}

async fn run_connection(
    app: AppHandle,
    state: Arc<AppState>,
    mut spec: ConnectionSpec,
    commands: &mut mpsc::UnboundedReceiver<SessionCommand>,
    startup: &mut Option<oneshot::Sender<Result<SessionMeta, String>>>,
) -> Result<bool, String> {
    let id = spec.meta.id.clone();
    let target = buoy_core::parse_ssh_target(&spec.meta.host)?;
    let user = target
        .user
        .ok_or_else(|| "mobile SSH requires user@host".to_string())?;
    let endpoint = format!("{}:{}", target.host, target.port);

    emit_state(&app, &id, "connecting");
    let config = client::Config {
        inactivity_timeout: Some(Duration::from_secs(90)),
        keepalive_interval: Some(Duration::from_secs(15)),
        keepalive_max: 3,
        nodelay: true,
        ..Default::default()
    };
    let ssh = tokio::time::timeout(
        Duration::from_secs(12),
        client::connect(
            Arc::new(config),
            (target.host.as_str(), target.port),
            SshHandler {
                store: state.store.clone(),
                endpoint,
            },
        ),
    )
    .await
    .map_err(|_| "SSH connection timed out".to_string())?
    .map_err(|error| format!("SSH connect failed: {error}"))?;
    let ssh = tokio::time::timeout(
        Duration::from_secs(20),
        authenticate(ssh, user, spec.password.as_deref()),
    )
    .await
    .map_err(|_| "SSH authentication timed out".to_string())??;

    let (tmux_path, tmux_version) =
        tokio::time::timeout(Duration::from_secs(10), remote::probe_tmux(&ssh))
            .await
            .map_err(|_| "remote tmux probe timed out".to_string())??;
    spec.meta.tmux_path = tmux_path;
    spec.meta.tmux_version = tmux_version;
    let control_supported = spec.meta.tmux_version.as_deref().is_some_and(|version| {
        version.first().copied().unwrap_or(0) > 3
            || (version.first() == Some(&3) && version.get(1).copied().unwrap_or(0) >= 2)
    });
    let use_control = spec.meta.mode == "control" && control_supported;
    spec.meta.mode = if use_control { "control" } else { "plain" }.into();
    state.store.upsert_session(spec.meta.clone())?;
    if let Ok(mut sessions) = state.sessions.lock() {
        if let Some(session) = sessions.get_mut(&id) {
            session.spec.meta = spec.meta.clone();
        }
    }

    let socket = format!("buoy-mobile-{}", spec.meta.session);
    if spec.meta.restore_pending {
        remote::restore_tmux_session(
            &ssh,
            &spec.meta.session,
            &socket,
            &spec.meta.tmux_path,
            &spec.meta.recovery_tabs,
            spec.meta.last_tab.as_deref(),
        ).await?;
        spec.meta.restore_pending = false;
        spec.meta.detached = false;
        state.store.upsert_session(spec.meta.clone())?;
    }

    let mut channel = ssh
        .channel_open_session()
        .await
        .map_err(|error| format!("open SSH session failed: {error}"))?;
    channel
        .request_pty(false, "xterm-256color", 90, 30, 0, 0, &[])
        .await
        .map_err(|error| format!("request remote PTY failed: {error}"))?;
    let command = claude_integration::remote_tmux_script(
        &spec.meta.tmux_path,
        &socket,
        &spec.meta.session,
        use_control,
    );
    channel
        .exec(true, command)
        .await
        .map_err(|error| format!("start remote tmux failed: {error}"))?;

    emit_state(&app, &id, "connected");
    let mut control = use_control.then(|| ControlEngine::new(spec.meta.session.clone()));
    if control.is_none() {
        let _ = app.emit("session:ready", json!({ "id": id }));
        if let Some(startup) = startup.take() {
            let _ = startup.send(Ok(spec.meta.clone()));
        }
    }

    let (accepted_tx, mut accepted_rx) = mpsc::unbounded_channel();
    let mut active_tunnels = BTreeMap::new();
    let mut pending_data = BTreeMap::new();
    // Plain-mode SSH data has arbitrary packet boundaries. Keep an incomplete trailing UTF-8
    // sequence until the next ChannelMsg instead of replacing each split byte with U+FFFD.
    let mut plain_utf8_carry = Vec::new();
    let mut output_tick = tokio::time::interval(Duration::from_millis(16));
    output_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let startup_timeout = tokio::time::sleep(Duration::from_secs(15));
    tokio::pin!(startup_timeout);
    let tunnel_context = TunnelContext {
        app: &app,
        state: &state,
        id: &id,
        accepted: &accepted_tx,
    };
    for persisted in state.store.tunnels(&id) {
        let _ = tunnel_context
            .ensure(
                persisted.remote,
                Some(persisted.local),
                false,
                &mut active_tunnels,
            )
            .await;
    }

    let mut intentional = false;
    loop {
        tokio::select! {
            _ = &mut startup_timeout, if startup.is_some() => {
                return Err("remote tmux attach timed out before becoming ready".into());
            }
            command = commands.recv() => match command {
                Some(SessionCommand::Input { data, window }) => {
                    if let Some(engine) = control.as_mut() {
                        let actions = engine.input(data, window);
                        if send_control_actions(&app, &id, &mut channel, actions, &mut pending_data).await? { break; }
                    } else {
                        channel.data_bytes(data.into_bytes()).await.map_err(|error| format!("SSH write failed: {error}"))?;
                    }
                }
                Some(SessionCommand::Resize { cols, rows }) => {
                    channel.window_change(cols.max(1), rows.max(1), 0, 0).await
                        .map_err(|error| format!("SSH resize failed: {error}"))?;
                    if let Some(engine) = control.as_mut() {
                        let actions = engine.resize(cols, rows);
                        if send_control_actions(&app, &id, &mut channel, actions, &mut pending_data).await? { break; }
                    }
                }
                Some(SessionCommand::TabNew) => if let Some(engine) = control.as_mut() {
                    let actions = engine.new_window();
                    if send_control_actions(&app, &id, &mut channel, actions, &mut pending_data).await? { break; }
                },
                Some(SessionCommand::TabSelect(window)) => if let Some(engine) = control.as_mut() {
                    let actions = engine.select_window(&window);
                    if send_control_actions(&app, &id, &mut channel, actions, &mut pending_data).await? { break; }
                },
                Some(SessionCommand::TabClose(window)) => if let Some(engine) = control.as_mut() {
                    let actions = engine.close_window(&window);
                    if send_control_actions(&app, &id, &mut channel, actions, &mut pending_data).await? { break; }
                },
                Some(SessionCommand::TabCapture(window)) => if let Some(engine) = control.as_mut() {
                    let actions = engine.capture_window(&window);
                    if send_control_actions(&app, &id, &mut channel, actions, &mut pending_data).await? { break; }
                },
                Some(SessionCommand::TabRename { window, title }) => if let Some(engine) = control.as_mut() {
                    let actions = engine.rename_window(&window, &title);
                    if send_control_actions(&app, &id, &mut channel, actions, &mut pending_data).await? { break; }
                },
                Some(SessionCommand::EnsureTunnel { remote, prefer_same_port, reply }) => {
                    let stored = state.tunnels.list(&id).into_iter().find(|tunnel| tunnel.remote == remote);
                    let preferred = if prefer_same_port { Some(remote) } else { stored.map(|tunnel| tunnel.local) };
                    let result = tunnel_context.ensure(remote, preferred, prefer_same_port, &mut active_tunnels).await;
                    let _ = reply.send(result);
                }
                Some(SessionCommand::CloseTunnel(remote)) => {
                    if let Some(tunnel) = active_tunnels.remove(&remote) { tunnel.close(); }
                    let _ = state.tunnels.forget(&id, remote);
                    emit_tunnels(&app, &state, &id);
                }
                Some(SessionCommand::ReadFile { path, reply }) => {
                    let result = remote::read_file(
                        &ssh,
                        &path,
                        &spec.meta.session,
                        &socket,
                        &spec.meta.tmux_path,
                    ).await;
                    let _ = reply.send(result);
                }
                Some(SessionCommand::KillRemote) => {
                    let _ = remote::kill_tmux_session(
                        &ssh,
                        &spec.meta.session,
                        &socket,
                        &spec.meta.tmux_path,
                    ).await;
                    let _ = state.store.remove_session(&id);
                    intentional = true;
                    let _ = channel.close().await;
                    break;
                }
                Some(SessionCommand::CloseRemote { hints, reply }) => {
                    let result = remote::snapshot_and_kill(
                        &ssh,
                        &spec.meta.session,
                        &socket,
                        &spec.meta.tmux_path,
                        &hints,
                    ).await.and_then(|recovery_tabs| {
                        let archived_at = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|duration| duration.as_millis() as u64)
                            .unwrap_or(0);
                        state.store.update_session(&id, |saved| {
                            saved.archived = true;
                            saved.archived_at = Some(archived_at);
                            saved.detached = false;
                            saved.recovery_tabs = recovery_tabs;
                            saved.restore_pending = true;
                        }).map(|_| ())
                    });
                    if result.is_err() {
                        let _ = state.store.update_session(&id, |saved| saved.detached = true);
                    }
                    let _ = reply.send(result);
                    intentional = true;
                    let _ = channel.close().await;
                    break;
                }
                Some(SessionCommand::Detach) | None => {
                    intentional = true;
                    let _ = channel.close().await;
                    break;
                }
            },
            accepted = accepted_rx.recv() => if let Some(accepted) = accepted {
                match ssh.channel_open_direct_tcpip(
                    "127.0.0.1",
                    accepted.remote.into(),
                    accepted.origin.ip().to_string(),
                    accepted.origin.port().into(),
                ).await {
                    Ok(channel) => {
                        tauri::async_runtime::spawn(async move {
                            let _ = tunnel::bridge(accepted.stream, channel).await;
                        });
                    }
                    Err(error) => queue_data(&mut pending_data, None, format!("\r\nBuoy tunnel: {error}\r\n")),
                }
            },
            _ = output_tick.tick(), if !pending_data.is_empty() => {
                flush_data(&app, &id, &mut pending_data);
            }
            message = channel.wait() => match message {
                Some(ChannelMsg::Data { data }) | Some(ChannelMsg::ExtendedData { data, .. }) => {
                    if let Some(engine) = control.as_mut() {
                        let actions = engine.feed(&data);
                        let became_ready = actions.iter().any(|action| matches!(action, ControlAction::Ready));
                        if send_control_actions(&app, &id, &mut channel, actions, &mut pending_data).await? { break; }
                        if became_ready {
                            if let Some(startup) = startup.take() {
                                let _ = startup.send(Ok(spec.meta.clone()));
                            }
                        }
                    } else {
                        plain_utf8_carry.extend_from_slice(&data);
                        let (text, tail) = control::decode_utf8_prefix(&plain_utf8_carry);
                        plain_utf8_carry = tail;
                        queue_data(&mut pending_data, None, text);
                    }
                }
                Some(ChannelMsg::ExitStatus { .. }) | Some(ChannelMsg::Eof) | Some(ChannelMsg::Close) | None => break,
                _ => {}
            }
        }
    }

    // EOF in the middle of a code point is malformed output, but surface one replacement glyph
    // rather than silently dropping the bytes. Normal streams leave this carry empty.
    if !plain_utf8_carry.is_empty() {
        queue_data(
            &mut pending_data,
            None,
            String::from_utf8_lossy(&plain_utf8_carry),
        );
    }
    flush_data(&app, &id, &mut pending_data);
    for (_, tunnel) in active_tunnels {
        tunnel.close();
    }
    state.tunnels.deactivate_session(&id);
    emit_tunnels(&app, &state, &id);
    let _ = ssh
        .disconnect(Disconnect::ByApplication, "", "English")
        .await;
    Ok(intentional)
}

fn spawn_connection(
    app: AppHandle,
    state: Arc<AppState>,
    spec: ConnectionSpec,
    report_startup: bool,
    remove_persisted_on_startup_failure: bool,
) -> Option<oneshot::Receiver<Result<SessionMeta, String>>> {
    let id = spec.meta.id.clone();
    let runtime_id = state.next_runtime_id.fetch_add(1, Ordering::Relaxed);
    let (commands_tx, mut commands_rx) = mpsc::unbounded_channel();
    let (startup_tx, startup_rx) = oneshot::channel();
    let mut startup = report_startup.then_some(startup_tx);
    if let Ok(mut sessions) = state.sessions.lock() {
        sessions.insert(
            id.clone(),
            MobileSession {
                runtime_id,
                spec: spec.clone(),
                commands: commands_tx,
            },
        );
    }
    tauri::async_runtime::spawn(async move {
        let mut attempt = 0_u32;
        loop {
            let mut fatal = false;
            match run_connection(
                app.clone(),
                state.clone(),
                spec.clone(),
                &mut commands_rx,
                &mut startup,
            )
            .await
            {
                Ok(true) => {
                    let replaced = state
                        .sessions
                        .lock()
                        .ok()
                        .and_then(|sessions| sessions.get(&id).map(|session| session.runtime_id))
                        .is_some_and(|current| current != runtime_id);
                    if !replaced {
                        emit_state(&app, &id, "closed");
                        let _ = app.emit("session:exit", json!({ "id": id }));
                    }
                    break;
                }
                Ok(false) => {
                    attempt = 0;
                    emit_state(&app, &id, "reconnecting");
                }
                Err(message) => {
                    if let Some(startup) = startup.take() {
                        let _ = startup.send(Err(message.clone()));
                        if let Ok(mut sessions) = state.sessions.lock() {
                            if sessions
                                .get(&id)
                                .is_some_and(|session| session.runtime_id == runtime_id)
                            {
                                sessions.remove(&id);
                            }
                        }
                        if remove_persisted_on_startup_failure {
                            let _ = state.store.remove_session(&id);
                        }
                        break;
                    }
                    emit_data(&app, &id, None, format!("\r\nBuoy: {message}\r\n"));
                    emit_state(&app, &id, "dead");
                    let lower = message.to_ascii_lowercase();
                    fatal = lower.contains("authentication")
                        || lower.contains("unknown key")
                        || lower.contains("key changed")
                        || lower.contains("tmux is required");
                }
            }
            attempt = attempt.saturating_add(1);
            if fatal || attempt >= 10 {
                loop {
                    match commands_rx.recv().await {
                        Some(SessionCommand::Detach) | Some(SessionCommand::KillRemote) | None => {
                            break;
                        }
                        Some(SessionCommand::CloseRemote { reply, .. }) => {
                            let _ = reply.send(Err("reconnect this session before closing it".into()));
                        }
                        _ => {}
                    }
                }
                let replaced = state
                    .sessions
                    .lock()
                    .ok()
                    .and_then(|sessions| sessions.get(&id).map(|session| session.runtime_id))
                    .is_some_and(|current| current != runtime_id);
                if !replaced {
                    emit_state(&app, &id, "closed");
                    let _ = app.emit("session:exit", json!({ "id": id }));
                }
                break;
            }
            let delay = Duration::from_secs((1_u64 << attempt.min(5)).min(30));
            tokio::select! {
                _ = tokio::time::sleep(delay) => emit_state(&app, &id, "reconnecting"),
                command = commands_rx.recv() => match command {
                    Some(SessionCommand::Detach) | Some(SessionCommand::KillRemote) | None => {
                        let replaced = state
                            .sessions
                            .lock()
                            .ok()
                            .and_then(|sessions| sessions.get(&id).map(|session| session.runtime_id))
                            .is_some_and(|current| current != runtime_id);
                        if !replaced {
                            emit_state(&app, &id, "closed");
                            let _ = app.emit("session:exit", json!({ "id": id }));
                        }
                        break;
                    }
                    Some(SessionCommand::CloseRemote { reply, .. }) => {
                        let _ = reply.send(Err("session is reconnecting; try Close again when connected".into()));
                    }
                    _ => {}
                }
            }
        }
    });
    report_startup.then_some(startup_rx)
}

fn send_session(state: &AppState, id: &str, command: SessionCommand) -> Result<(), String> {
    state
        .sessions
        .lock()
        .map_err(|_| "session state lock poisoned")?
        .get(id)
        .ok_or_else(|| "session is not connected".to_string())?
        .commands
        .send(command)
        .map_err(|_| "session runtime stopped".to_string())
}

#[tauri::command]
fn get_runtime_capabilities() -> buoy_core::RuntimeCapabilities {
    buoy_core::RuntimeCapabilities::mobile()
}

#[tauri::command]
fn list_sessions(state: State<Arc<AppState>>) -> Vec<SessionMeta> {
    state.store.sessions()
}

#[tauri::command]
async fn create_session(
    app: AppHandle,
    state: State<'_, Arc<AppState>>,
    meta: CreateArgs,
) -> Result<serde_json::Value, String> {
    let target = buoy_core::parse_ssh_target(&meta.host)?;
    if target.user.is_none() {
        return Err("mobile SSH requires user@host".into());
    }
    let id = meta.id.unwrap_or_else(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_millis().to_string())
            .unwrap_or_else(|_| "mobile".into())
    });
    let session = meta.session.unwrap_or_else(|| {
        let tail = id
            .chars()
            .filter(|character| character.is_ascii_alphanumeric())
            .rev()
            .take(12)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<String>();
        format!("dt-{}", if tail.is_empty() { "mobile" } else { &tail })
    });
    buoy_core::validate_session_name(&session)?;

    let previous = state
        .sessions
        .lock()
        .map_err(|_| "session state lock poisoned")?
        .remove(&id);
    if let Some(previous) = &previous {
        let _ = previous.commands.send(SessionCommand::Detach);
    }
    let password = meta
        .ssh_password
        .or_else(|| previous.and_then(|session| session.spec.password));
    let persisted = state.store.session(&id);
    let remove_on_failure = persisted.is_none();
    let order = persisted
        .as_ref()
        .map(|session| session.order)
        .unwrap_or_else(|| state.store.sessions().len() as i64);
    let session_meta = SessionMeta {
        id: id.clone(),
        host: meta.host.clone(),
        session: session.clone(),
        kind: "remote".into(),
        transport: "ssh".into(),
        mode: meta
            .mode
            .filter(|mode| mode == "plain" || mode == "control")
            .unwrap_or_else(|| "control".into()),
        title: meta
            .title
            .filter(|title| !title.trim().is_empty())
            .unwrap_or_else(|| meta.host.clone()),
        tmux_path: meta.tmux_path.unwrap_or_else(|| "tmux".into()),
        tmux_version: meta.tmux_version,
        order,
        color: persisted.as_ref().and_then(|session| session.color.clone()),
        last_tab: persisted
            .as_ref()
            .and_then(|session| session.last_tab.clone()),
        tab_order: persisted
            .as_ref()
            .map(|session| session.tab_order.clone())
            .unwrap_or_default(),
        tab_colors: persisted
            .as_ref()
            .map(|session| session.tab_colors.clone())
            .unwrap_or_default(),
        archived: false,
        archived_at: None,
        detached: false,
        recovery_tabs: persisted
            .as_ref()
            .map(|session| session.recovery_tabs.clone())
            .unwrap_or_default(),
        restore_pending: persisted
            .as_ref()
            .is_some_and(|session| session.restore_pending),
    };
    state.store.upsert_session(session_meta.clone())?;
    state.store.remember_host(&meta.host)?;
    let runtime_state = state.inner().clone();
    let startup = spawn_connection(
        app,
        runtime_state,
        ConnectionSpec {
            meta: session_meta.clone(),
            password,
        },
        true,
        remove_on_failure,
    )
    .ok_or_else(|| "mobile runtime did not create a startup channel".to_string())?;
    let connected_meta = startup
        .await
        .map_err(|_| "mobile SSH runtime stopped during startup".to_string())??;
    Ok(json!({
        "id": id,
        "session": session,
        "mode": connected_meta.mode,
        "tmuxPath": connected_meta.tmux_path,
        "tmuxVersion": connected_meta.tmux_version,
        "ready": true,
    }))
}

#[tauri::command]
fn session_input(state: State<Arc<AppState>>, id: String, data: String, win: Option<String>) {
    let _ = send_session(&state, &id, SessionCommand::Input { data, window: win });
}

#[tauri::command]
fn session_resize(state: State<Arc<AppState>>, id: String, cols: u32, rows: u32) {
    let _ = send_session(&state, &id, SessionCommand::Resize { cols, rows });
}

#[tauri::command]
fn session_detach(state: State<Arc<AppState>>, id: String) -> Result<(), String> {
    let updated = state.store.update_session(&id, |session| session.detached = true)?;
    if updated.is_none() {
        return Err("unknown session".into());
    }
    if let Ok(mut sessions) = state.sessions.lock() {
        if let Some(session) = sessions.remove(&id) {
            let _ = session.commands.send(SessionCommand::Detach);
        }
    }
    if state.store.last_active().as_deref() == Some(id.as_str()) {
        state.store.set_last_active(None)?;
    }
    Ok(())
}

#[tauri::command]
async fn session_close(
    state: State<'_, Arc<AppState>>,
    id: String,
    tabs: Vec<RecoveryTab>,
) -> Result<(), String> {
    let (reply, response) = oneshot::channel();
    send_session(&state, &id, SessionCommand::CloseRemote { hints: tabs, reply })?;
    let result = tokio::time::timeout(Duration::from_secs(20), response)
        .await
        .map_err(|_| "timed out while snapshotting and closing tmux".to_string())?
        .map_err(|_| "session stopped while closing tmux".to_string())?;
    if let Ok(mut sessions) = state.sessions.lock() { sessions.remove(&id); }
    if state.store.last_active().as_deref() == Some(id.as_str()) {
        state.store.set_last_active(None)?;
    }
    result
}

#[tauri::command]
fn session_resume(state: State<Arc<AppState>>, id: String) -> Result<(), String> {
    let updated = state.store.update_session(&id, |session| {
        session.archived = false;
        session.archived_at = None;
        session.detached = false;
    })?;
    if updated.is_some() { Ok(()) } else { Err("unknown session".into()) }
}

#[tauri::command]
fn session_kill(state: State<Arc<AppState>>, id: String) -> serde_json::Value {
    if let Ok(mut sessions) = state.sessions.lock() {
        if let Some(session) = sessions.remove(&id) {
            let _ = session.commands.send(SessionCommand::KillRemote);
        }
    }
    let _ = state.store.remove_session(&id);
    json!({ "killedRemote": true })
}

fn reconnect(app: AppHandle, state: &Arc<AppState>, id: &str) -> Result<(), String> {
    let previous = state
        .sessions
        .lock()
        .map_err(|_| "session state lock poisoned")?
        .remove(id);
    let spec = match previous {
        Some(previous) => {
            let _ = previous.commands.send(SessionCommand::Detach);
            previous.spec
        }
        None => ConnectionSpec {
            meta: state
                .store
                .session(id)
                .ok_or_else(|| "unknown session".to_string())?,
            password: None,
        },
    };
    let _ = spawn_connection(app, state.clone(), spec, false, false);
    Ok(())
}

#[tauri::command]
fn session_retry(app: AppHandle, state: State<Arc<AppState>>, id: String) {
    let _ = reconnect(app, &state, &id);
}

#[tauri::command]
fn session_force_reconnect(app: AppHandle, state: State<Arc<AppState>>, id: String) {
    let _ = reconnect(app, &state, &id);
}

#[tauri::command]
fn session_rename(state: State<Arc<AppState>>, id: String, title: String) -> serde_json::Value {
    let clean: String = title
        .chars()
        .filter(|character| !character.is_control())
        .take(120)
        .collect();
    let _ = state
        .store
        .update_session(&id, |session| session.title = clean.clone());
    if let Ok(mut sessions) = state.sessions.lock() {
        if let Some(session) = sessions.get_mut(&id) {
            session.spec.meta.title = clean.clone();
        }
    }
    json!({ "ok": true, "title": clean })
}

#[tauri::command]
fn list_hosts(state: State<Arc<AppState>>) -> Vec<String> {
    state.store.hosts()
}

#[tauri::command]
fn remember_host(state: State<Arc<AppState>>, host: String) {
    let _ = state.store.remember_host(&host);
}

#[tauri::command]
fn get_config(state: State<Arc<AppState>>) -> serde_json::Value {
    json!({
        "loopbackHosts": ["localhost", "127.0.0.1", "::1"],
        "lastActive": state.store.last_active(),
    })
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SessionCheckResult {
    id: String,
    open: bool,
    error: Option<String>,
}

async fn check_mobile_session(state: &Arc<AppState>, meta: &SessionMeta) -> Result<bool, String> {
    // Reuse only the ephemeral credential already held by a live runtime. Detached rows have
    // deliberately dropped it, so password-only hosts report an auth error rather than persisting
    // a secret merely to make discovery convenient.
    let password = state.sessions.lock().ok().and_then(|sessions| {
        sessions
            .get(&meta.id)
            .and_then(|session| session.spec.password.clone())
    });
    let target = buoy_core::parse_ssh_target(&meta.host)?;
    let user = target.user.ok_or_else(|| "mobile SSH requires user@host".to_string())?;
    let endpoint = format!("{}:{}", target.host, target.port);
    let config = client::Config {
        inactivity_timeout: Some(Duration::from_secs(12)),
        keepalive_interval: Some(Duration::from_secs(5)),
        keepalive_max: 1,
        nodelay: true,
        ..Default::default()
    };
    let ssh = tokio::time::timeout(
        Duration::from_secs(8),
        client::connect(
            Arc::new(config),
            (target.host.as_str(), target.port),
            SshHandler { store: state.store.clone(), endpoint },
        ),
    ).await.map_err(|_| "SSH check timed out".to_string())?
        .map_err(|error| format!("SSH check failed: {error}"))?;
    let ssh = tokio::time::timeout(
        Duration::from_secs(10),
        authenticate(ssh, user, password.as_deref()),
    )
    .await
    .map_err(|_| "SSH authentication timed out".to_string())??;
    let socket = format!("buoy-mobile-{}", meta.session);
    let open = remote::has_tmux_session(&ssh, &meta.session, &socket, &meta.tmux_path).await?;
    let _ = ssh.disconnect(Disconnect::ByApplication, "", "English").await;
    Ok(open)
}

#[tauri::command]
async fn check_open_sessions(state: State<'_, Arc<AppState>>) -> Result<Vec<SessionCheckResult>, String> {
    let sessions: Vec<_> = state.store.sessions().into_iter()
        .filter(|session| !session.archived)
        .collect();
    let mut results = Vec::with_capacity(sessions.len());
    for session in sessions {
        match check_mobile_session(&state, &session).await {
            Ok(open) => results.push(SessionCheckResult { id: session.id, open, error: None }),
            Err(error) => results.push(SessionCheckResult { id: session.id, open: false, error: Some(error) }),
        }
    }
    Ok(results)
}

#[tauri::command]
fn set_last_active(state: State<Arc<AppState>>, id: String) {
    let _ = state.store.set_last_active(Some(id));
}

#[tauri::command]
fn open_external(app: AppHandle, url: String) -> serde_json::Value {
    let allowed = ["http://", "https://", "ftp://", "file:", "mailto:"]
        .iter()
        .any(|prefix| url.starts_with(prefix));
    if allowed {
        use tauri_plugin_opener::OpenerExt;
        let _ = app.opener().open_url(&url, None::<&str>);
    }
    json!({ "ok": allowed })
}

#[tauri::command]
fn list_tunnels(state: State<Arc<AppState>>, id: String) -> Vec<model::TunnelStatus> {
    state.tunnels.list(&id)
}

async fn request_tunnel(
    state: &Arc<AppState>,
    id: &str,
    remote: u16,
    prefer_same_port: bool,
) -> Result<u16, String> {
    let (reply, response) = oneshot::channel();
    send_session(
        state,
        id,
        SessionCommand::EnsureTunnel {
            remote,
            prefer_same_port,
            reply,
        },
    )?;
    response
        .await
        .map_err(|_| "session stopped while opening tunnel".to_string())?
}

#[tauri::command]
fn close_tunnel(app: AppHandle, state: State<Arc<AppState>>, id: String, remote: u16) {
    let _ = send_session(&state, &id, SessionCommand::CloseTunnel(remote));
    let _ = state.tunnels.forget(&id, remote);
    emit_tunnels(&app, &state, &id);
}

#[tauri::command]
async fn force_forward(
    app: AppHandle,
    state: State<'_, Arc<AppState>>,
    id: String,
    remote: u16,
) -> Result<serde_json::Value, String> {
    let local = request_tunnel(&state, &id, remote, true).await?;
    emit_tunnels(&app, &state, &id);
    Ok(json!({ "ok": true, "local": local }))
}

#[tauri::command]
async fn open_forwarded_url(
    app: AppHandle,
    state: State<'_, Arc<AppState>>,
    id: String,
    url: String,
) -> Result<serde_json::Value, String> {
    let candidate = if url.contains("://") {
        url.clone()
    } else {
        format!("http://{url}")
    };
    let parsed = url::Url::parse(&candidate).map_err(|error| error.to_string())?;
    if !matches!(parsed.scheme(), "http" | "https")
        || !matches!(parsed.host_str(), Some("localhost" | "127.0.0.1" | "::1"))
    {
        return Err("not a loopback HTTP URL".into());
    }
    let remote = parsed
        .port_or_known_default()
        .ok_or_else(|| "loopback URL has no port".to_string())?;
    let local = request_tunnel(&state, &id, remote, false).await?;
    let mut local_url = parsed;
    local_url
        .set_host(Some("127.0.0.1"))
        .map_err(|_| "invalid local tunnel URL".to_string())?;
    local_url
        .set_port(Some(local))
        .map_err(|_| "invalid local tunnel port".to_string())?;
    tokio::time::sleep(Duration::from_millis(150)).await;
    use tauri_plugin_opener::OpenerExt;
    let _ = app.opener().open_url(local_url.as_str(), None::<&str>);
    emit_tunnels(&app, &state, &id);
    Ok(json!({ "ok": true, "localUrl": local_url.as_str() }))
}

#[tauri::command]
async fn read_remote_file(
    state: State<'_, Arc<AppState>>,
    id: String,
    path: String,
) -> Result<serde_json::Value, String> {
    let (reply, response) = oneshot::channel();
    send_session(&state, &id, SessionCommand::ReadFile { path, reply })?;
    let (bytes, truncated) = response
        .await
        .map_err(|_| "session stopped while reading the file".to_string())??;
    Ok(json!({
        "data_b64": STANDARD.encode(&bytes),
        "size": bytes.len(),
        "truncated": truncated,
    }))
}

#[tauri::command]
async fn save_file(
    app: AppHandle,
    data_b64: String,
    suggested_name: String,
) -> Result<serde_json::Value, String> {
    use tauri_plugin_dialog::DialogExt;
    let bytes = STANDARD
        .decode(data_b64)
        .map_err(|error| format!("bad base64: {error}"))?;
    let name = if suggested_name.trim().is_empty() {
        "download".to_string()
    } else {
        suggested_name
    };
    let (sender, receiver) = std::sync::mpsc::channel();
    app.dialog()
        .file()
        .set_file_name(&name)
        .save_file(move |path| {
            let _ = sender.send(path);
        });
    let selected = tauri::async_runtime::spawn_blocking(move || receiver.recv().ok().flatten())
        .await
        .map_err(|error| format!("save dialog failed: {error}"))?;
    match selected {
        Some(path) => {
            let path = path.into_path().map_err(|error| error.to_string())?;
            std::fs::write(&path, bytes).map_err(|error| error.to_string())?;
            Ok(json!({ "ok": true, "path": path.to_string_lossy() }))
        }
        None => Ok(json!({ "ok": false, "canceled": true })),
    }
}

#[tauri::command]
fn enable_html_scripts(
    state: State<Arc<AppState>>,
    data_b64: String,
) -> Result<serde_json::Value, String> {
    let bytes = STANDARD
        .decode(data_b64)
        .map_err(|error| format!("bad base64: {error}"))?;
    let token = state.previews.put(bytes);
    Ok(json!({ "url": format!("{}://localhost/{token}", preview::SCHEME) }))
}

#[tauri::command]
fn reorder_sessions(state: State<Arc<AppState>>, ids: Vec<String>) {
    let _ = state.store.reorder_sessions(&ids);
}

fn clean_color(color: Option<String>) -> Option<String> {
    color.filter(|color| {
        color.len() == 7
            && color.starts_with('#')
            && color[1..]
                .chars()
                .all(|character| character.is_ascii_hexdigit())
    })
}

#[tauri::command]
fn set_session_color(state: State<Arc<AppState>>, id: String, color: Option<String>) {
    let color = clean_color(color);
    let _ = state
        .store
        .update_session(&id, |session| session.color = color.clone());
}

#[tauri::command]
fn set_last_tab(state: State<Arc<AppState>>, id: String, win: String) {
    let tab = win.starts_with('@').then_some(win);
    let _ = state
        .store
        .update_session(&id, |session| session.last_tab = tab);
}

#[tauri::command]
fn set_tab_prefs(
    state: State<Arc<AppState>>,
    id: String,
    tab_order: Option<Vec<String>>,
    tab_color: Option<(String, Option<String>)>,
) {
    let _ = state.store.update_session(&id, |session| {
        if let Some(order) = tab_order {
            session.tab_order = order
                .into_iter()
                .filter(|window| window.starts_with('@'))
                .collect();
        }
        if let Some((window, color)) = tab_color {
            if let Some(color) = clean_color(color) {
                session.tab_colors.insert(window, color);
            } else {
                session.tab_colors.remove(&window);
            }
        }
    });
}

#[tauri::command]
fn tab_new(state: State<Arc<AppState>>, id: String) {
    let _ = send_session(&state, &id, SessionCommand::TabNew);
}

#[tauri::command]
fn tab_select(state: State<Arc<AppState>>, id: String, win: String) {
    let _ = send_session(&state, &id, SessionCommand::TabSelect(win));
}

#[tauri::command]
fn tab_close(state: State<Arc<AppState>>, id: String, win: String) {
    let _ = send_session(&state, &id, SessionCommand::TabClose(win));
}

#[tauri::command]
fn tab_capture(state: State<Arc<AppState>>, id: String, win: String) {
    let _ = send_session(&state, &id, SessionCommand::TabCapture(win));
}

#[tauri::command]
fn tab_rename(state: State<Arc<AppState>>, id: String, win: String, title: String) {
    let _ = send_session(
        &state,
        &id,
        SessionCommand::TabRename { window: win, title },
    );
}

#[tauri::command]
fn ui_log(msg: String) {
    eprintln!("[Buoy mobile] {msg}");
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            let data_dir = app
                .path()
                .app_data_dir()
                .map_err(|error| error.to_string())?;
            let store = Arc::new(MobileStore::load(&data_dir)?);
            app.manage(Arc::new(AppState {
                sessions: Mutex::new(HashMap::new()),
                tunnels: Arc::new(TunnelBook::new(store.clone())),
                store,
                previews: preview::PreviewStore::default(),
                next_runtime_id: AtomicU64::new(1),
            }));
            Ok(())
        })
        .register_uri_scheme_protocol(preview::SCHEME, |context, request| {
            let path = request.uri().path();
            let (status, csp, body) = context
                .app_handle()
                .try_state::<Arc<AppState>>()
                .map(|state| state.previews.response(path))
                .unwrap_or((500, "default-src 'none'", Vec::new()));
            tauri::http::Response::builder()
                .status(status)
                .header("Content-Type", "text/html; charset=utf-8")
                .header("Content-Security-Policy", csp)
                .header("Referrer-Policy", "no-referrer")
                .header("X-Content-Type-Options", "nosniff")
                .body(body)
                .unwrap_or_else(|_| tauri::http::Response::new(Vec::new()))
        })
        .invoke_handler(tauri::generate_handler![
            get_runtime_capabilities,
            list_sessions,
            create_session,
            session_input,
            session_resize,
            session_detach,
            session_close,
            session_resume,
            session_kill,
            session_retry,
            session_force_reconnect,
            session_rename,
            list_hosts,
            remember_host,
            get_config,
            check_open_sessions,
            set_last_active,
            open_external,
            list_tunnels,
            close_tunnel,
            force_forward,
            open_forwarded_url,
            read_remote_file,
            save_file,
            enable_html_scripts,
            reorder_sessions,
            set_session_color,
            set_last_tab,
            set_tab_prefs,
            tab_new,
            tab_select,
            tab_close,
            tab_capture,
            tab_rename,
            ui_log,
        ])
        .run(tauri::generate_context!())
        .expect("error while running Buoy mobile");
}
