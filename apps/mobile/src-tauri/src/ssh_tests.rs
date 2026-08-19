use std::collections::HashMap;
use std::sync::Arc;

use rand::rng;
use russh::keys::{Algorithm, PrivateKey};
use russh::server::{Auth, Server as _, Session};
use russh::{Channel, ChannelId, client, server};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;

#[derive(Clone, Default)]
struct FixtureServer {
    channels: Arc<Mutex<HashMap<ChannelId, Channel<server::Msg>>>>,
}

impl server::Server for FixtureServer {
    type Handler = Self;

    fn new_client(&mut self, _peer_addr: Option<std::net::SocketAddr>) -> Self {
        self.clone()
    }
}

impl server::Handler for FixtureServer {
    type Error = russh::Error;

    async fn auth_password(&mut self, user: &str, password: &str) -> Result<Auth, Self::Error> {
        Ok(if user == "alice" && password == "secret" {
            Auth::Accept
        } else {
            Auth::reject()
        })
    }

    async fn channel_open_session(
        &mut self,
        channel: Channel<server::Msg>,
        reply: server::ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.channels.lock().await.insert(channel.id(), channel);
        reply.accept().await;
        Ok(())
    }

    async fn channel_open_direct_tcpip(
        &mut self,
        channel: Channel<server::Msg>,
        _host_to_connect: &str,
        _port_to_connect: u32,
        _originator_address: &str,
        _originator_port: u32,
        reply: server::ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.channels.lock().await.insert(channel.id(), channel);
        reply.accept().await;
        Ok(())
    }

    async fn exec_request(
        &mut self,
        channel: ChannelId,
        command: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        let command = String::from_utf8_lossy(command);
        session.channel_success(channel)?;
        let output = if command.contains("/opt/homebrew/bin/tmux") {
            b"/opt/homebrew/bin/tmux\ntmux 3.7b\n".to_vec()
        } else if command.contains("base64 -d | /bin/sh") {
            b"aGVsbG8tbW9iaWxl\n".to_vec()
        } else {
            Vec::new()
        };
        session.data(channel, output)?;
        // Exercise OpenSSH's legal EOF-before-exit-status ordering.
        session.eof(channel)?;
        session.exit_status_request(channel, 0)?;
        session.close(channel)?;
        Ok(())
    }

    async fn data(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        session.data(channel, data.to_vec())?;
        Ok(())
    }
}

struct FixtureClient;

impl client::Handler for FixtureClient {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        _server_public_key: &russh::keys::PublicKey,
    ) -> Result<bool, Self::Error> {
        Ok(true)
    }
}

async fn fixture() -> (
    client::Handle<FixtureClient>,
    russh::server::RunningServerHandle,
) {
    let config = Arc::new(server::Config {
        auth_rejection_time: std::time::Duration::ZERO,
        auth_rejection_time_initial: Some(std::time::Duration::ZERO),
        keys: vec![PrivateKey::random(&mut rng(), Algorithm::Ed25519).unwrap()],
        ..Default::default()
    });
    let (ready, started) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let mut fixture = FixtureServer::default();
        let running = fixture.run_on_socket(config, &listener);
        let handle = running.handle();
        let _ = ready.send((address, handle));
        let _ = running.await;
    });
    let (address, server_handle) = started.await.unwrap();
    let mut client = client::connect(Arc::new(client::Config::default()), address, FixtureClient)
        .await
        .unwrap();
    assert!(
        client
            .authenticate_password("alice", "secret")
            .await
            .unwrap()
            .success()
    );
    (client, server_handle)
}

#[tokio::test]
async fn in_process_ssh_exec_supports_tmux_probe_and_binary_file_contract() {
    let (client, server) = fixture().await;
    let (path, version) = crate::remote::probe_tmux(&client).await.unwrap();
    assert_eq!(path, "/opt/homebrew/bin/tmux");
    assert_eq!(version, Some(vec![3, 7]));

    let (bytes, truncated) = crate::remote::read_file(
        &client,
        "notes.md",
        "dt-mobile",
        "buoy-mobile-dt-mobile",
        "/opt/homebrew/bin/tmux",
    )
    .await
    .unwrap();
    assert_eq!(bytes, b"hello-mobile");
    assert!(!truncated);
    server.shutdown("done".into());
}

#[tokio::test]
async fn direct_tcpip_channel_bridges_a_local_socket_bidirectionally() {
    let (client, server) = fixture().await;
    let local_listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let local_address = local_listener.local_addr().unwrap();
    let mut caller = TcpStream::connect(local_address).await.unwrap();
    let (bridge_side, origin) = local_listener.accept().await.unwrap();
    let channel = client
        .channel_open_direct_tcpip(
            "127.0.0.1",
            3000,
            origin.ip().to_string(),
            origin.port().into(),
        )
        .await
        .unwrap();
    let bridge = tokio::spawn(crate::tunnel::bridge(bridge_side, channel));
    caller.write_all(b"through-vpn").await.unwrap();
    let mut received = vec![0_u8; 11];
    tokio::time::timeout(
        std::time::Duration::from_secs(3),
        caller.read_exact(&mut received),
    )
    .await
    .expect("direct-tcpip echo timed out")
    .unwrap();
    assert_eq!(received, b"through-vpn");
    caller.shutdown().await.unwrap();
    bridge.abort();
    server.shutdown("done".into());
}
