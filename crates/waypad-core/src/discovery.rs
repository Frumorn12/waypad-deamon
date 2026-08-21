use crate::{
    capability::Capabilities,
    config::Config,
    crypto::HostIdentity,
    protocol::{DISCOVERY_MAGIC, PROTOCOL_VERSION},
};
use serde::Serialize;
use std::{net::SocketAddr, sync::Arc};
use tokio::{net::UdpSocket, sync::RwLock};
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

/// Answers discovery probes for as long as the daemon runs.
///
/// Takes the live capability cell rather than a snapshot, and reads it per
/// reply. A snapshot would go stale the moment a portal was approved or a
/// monitor was unplugged, and would advertise a backend the host no longer has.
pub async fn run_discovery(
    config: Config,
    identity: Arc<HostIdentity>,
    capabilities: Arc<RwLock<Capabilities>>,
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
        // Read and released within one reply. Holding this across an await
        // would block every writer for the life of the daemon.
        let current = capabilities.read().await.clone();
        let reply = DiscoveryReply {
            service: "dev.waypad.daemon",
            protocol: PROTOCOL_VERSION,
            host_name: hostname(),
            control_port: config.control_port,
            host_fingerprint: &identity.fingerprint,
            input_backend: current.input.backend.clone(),
            input_supported: current.input.supported,
            capture_backend: current.capture.backend.clone(),
            capture_supported: current.capture.supported,
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

/// The name shown in the phone's host list.
///
/// Falls back rather than failing: a host with no resolvable name is still
/// perfectly usable, and a blank entry in the client's list is worse than a
/// generic one.
pub fn hostname() -> String {
    let from_env = if cfg!(windows) {
        std::env::var("COMPUTERNAME")
    } else {
        std::env::var("HOSTNAME")
    };
    from_env
        .ok()
        .filter(|value| !value.is_empty())
        .or_else(read_system_hostname)
        .unwrap_or_else(|| "waypad-host".into())
}

#[cfg(unix)]
fn read_system_hostname() -> Option<String> {
    // `HOSTNAME` is a shell variable and is usually not exported to a systemd
    // user unit, so the file is the reliable source on Linux.
    std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

#[cfg(not(unix))]
fn read_system_hostname() -> Option<String> {
    None
}
