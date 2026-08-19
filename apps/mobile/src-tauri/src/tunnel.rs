use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use russh::{Channel, ChannelMsg};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, watch};

use crate::model::TunnelStatus;
use crate::store::MobileStore;

pub struct AcceptedTunnel {
    pub remote: u16,
    pub stream: TcpStream,
    pub origin: SocketAddr,
}

pub struct ActiveTunnel {
    pub local: u16,
    cancel: watch::Sender<bool>,
}

impl ActiveTunnel {
    pub fn close(self) {
        let _ = self.cancel.send(true);
    }
}

pub struct TunnelBook {
    store: Arc<MobileStore>,
    live: Mutex<BTreeMap<String, BTreeMap<u16, TunnelStatus>>>,
}

impl TunnelBook {
    pub fn new(store: Arc<MobileStore>) -> Self {
        Self {
            store,
            live: Mutex::new(BTreeMap::new()),
        }
    }

    pub fn list(&self, id: &str) -> Vec<TunnelStatus> {
        let mut merged: BTreeMap<u16, TunnelStatus> = self
            .store
            .tunnels(id)
            .into_iter()
            .map(|tunnel| (tunnel.remote, tunnel))
            .collect();
        if let Ok(live) = self.live.lock() {
            if let Some(tunnels) = live.get(id) {
                merged.extend(tunnels.clone());
            }
        }
        merged.into_values().collect()
    }

    pub fn activate(&self, id: &str, remote: u16, local: u16) -> Result<(), String> {
        let status = TunnelStatus {
            remote,
            local,
            active: true,
        };
        self.store.upsert_tunnel(id, status.clone())?;
        self.live
            .lock()
            .map_err(|_| "tunnel state lock poisoned")?
            .entry(id.into())
            .or_default()
            .insert(remote, status);
        Ok(())
    }

    pub fn deactivate(&self, id: &str, remote: u16) {
        if let Ok(mut live) = self.live.lock() {
            if let Some(tunnels) = live.get_mut(id) {
                tunnels.remove(&remote);
            }
        }
    }

    pub fn deactivate_session(&self, id: &str) {
        if let Ok(mut live) = self.live.lock() {
            live.remove(id);
        }
    }

    pub fn forget(&self, id: &str, remote: u16) -> Result<(), String> {
        self.deactivate(id, remote);
        self.store.remove_tunnel(id, remote)
    }
}

pub async fn start_listener(
    remote: u16,
    preferred_local: Option<u16>,
    strict_preferred: bool,
    accepted: mpsc::UnboundedSender<AcceptedTunnel>,
) -> Result<ActiveTunnel, String> {
    let preferred = preferred_local.filter(|port| *port > 0).unwrap_or(0);
    let listener = match TcpListener::bind(("127.0.0.1", preferred)).await {
        Ok(listener) => listener,
        Err(error) if preferred > 0 && strict_preferred => {
            return Err(format!("local port {preferred} is unavailable: {error}"));
        }
        Err(_) if preferred > 0 => TcpListener::bind(("127.0.0.1", 0))
            .await
            .map_err(|error| format!("bind tunnel listener failed: {error}"))?,
        Err(error) => return Err(format!("bind tunnel listener failed: {error}")),
    };
    let local = listener
        .local_addr()
        .map_err(|error| error.to_string())?
        .port();
    let (cancel, mut cancellation) = watch::channel(false);
    tauri::async_runtime::spawn(async move {
        loop {
            tokio::select! {
                changed = cancellation.changed() => {
                    if changed.is_err() || *cancellation.borrow() {
                        break;
                    }
                }
                incoming = listener.accept() => match incoming {
                    Ok((stream, origin)) => {
                        if accepted.send(AcceptedTunnel { remote, stream, origin }).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        }
    });
    Ok(ActiveTunnel { local, cancel })
}

pub async fn bridge(
    mut stream: TcpStream,
    mut channel: Channel<russh::client::Msg>,
) -> Result<(), String> {
    let mut local_closed = false;
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        tokio::select! {
            read = stream.read(&mut buffer), if !local_closed => match read {
                Ok(0) => {
                    local_closed = true;
                    channel.eof().await.map_err(|error| error.to_string())?;
                }
                Ok(count) => channel.data(&buffer[..count]).await.map_err(|error| error.to_string())?,
                Err(error) => return Err(error.to_string()),
            },
            message = channel.wait() => match message {
                Some(ChannelMsg::Data { data }) | Some(ChannelMsg::ExtendedData { data, .. }) => {
                    stream.write_all(&data).await.map_err(|error| error.to_string())?;
                }
                Some(ChannelMsg::Eof) | Some(ChannelMsg::Close) | None => break,
                _ => {}
            }
        }
    }
    let _ = stream.shutdown().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn listener_uses_a_sticky_port_or_falls_back_safely() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let first = start_listener(3000, None, false, tx.clone()).await.unwrap();
        assert!(first.local > 0);
        let second = start_listener(3001, Some(first.local), false, tx)
            .await
            .unwrap();
        assert_ne!(first.local, second.local);
        first.close();
        second.close();
    }

    #[tokio::test]
    async fn strict_listener_rejects_an_occupied_port() {
        let occupied = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = occupied.local_addr().unwrap().port();
        let (tx, _rx) = mpsc::unbounded_channel();
        let error = start_listener(3000, Some(port), true, tx)
            .await
            .err()
            .expect("strict binding should fail");
        assert!(error.contains(&port.to_string()));
    }
}
