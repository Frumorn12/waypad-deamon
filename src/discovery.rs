use crate::{
    capability::Capabilities,
    config::Config,
    crypto::HostIdentity,
    protocol::{DISCOVERY_MAGIC, PROTOCOL_VERSION},
};
use serde::Serialize;
use std::{net::SocketAddr, sync::Arc};
use tokio::net::UdpSocket;
use tracing::{debug, warn};

#[derive(Debug, Serialize)]
struct DiscoveryReply<'a> {
    service: &'static str,
    protocol: u16,
    host_name: String,
    control_port: u16,
    host_fingerprint: &'a str,
    input_backend: String,
    input_supported: bool,
    capture_backend: String,
    capture_supported: bool,
}

pub async fn run_discovery(
    config: Config,
    identity: Arc<HostIdentity>,
    capabilities: Capabilities,
) -> anyhow::Result<()> {
    let bind = format!("0.0.0.0:{}", config.discovery_port);
    let socket = UdpSocket::bind(&bind).await?;
    socket.set_broadcast(true)?;
    tracing::info!("Waypad discovery listening on udp://{bind}");

    let mut buf = [0u8; 1024];
    loop {
        let (len, peer) = socket.recv_from(&mut buf).await?;
        if &buf[..len] != DISCOVERY_MAGIC {
            debug!(%peer, "ignoring non-Waypad discovery packet");
            continue;
        }
        if config.require_private_lan && !crate::server::is_private_or_local(peer) {
            warn!(%peer, "rejecting discovery from non-local address");
            continue;
        }
        let reply = DiscoveryReply {
            service: "dev.waypad.daemon",
            protocol: PROTOCOL_VERSION,
            host_name: hostname(),
            control_port: config.control_port,
            host_fingerprint: &identity.fingerprint,
            input_backend: capabilities.input.backend.clone(),
            input_supported: capabilities.input.supported,
            capture_backend: capabilities.capture.backend.clone(),
            capture_supported: capabilities.capture.supported,
        };
        let raw = serde_json::to_vec(&reply)?;
        socket
            .send_to(&raw, reply_addr(peer, config.discovery_port))
            .await?;
    }
}

fn reply_addr(peer: SocketAddr, fallback_port: u16) -> SocketAddr {
    if peer.port() == 0 {
        SocketAddr::new(peer.ip(), fallback_port)
    } else {
        peer
    }
}

pub fn hostname() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| {
            std::fs::read_to_string("/etc/hostname")
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| "linux-host".into())
}
