//! The Linux implementation of `waypad_core::backend::PlatformHost`.

use crate::{
    audio::LinuxAudioBackend, capability, gamepad::ControllerInputManager, input::InputManager,
    platform::command_output, screen::LinuxCaptureBackend, system_control::LinuxSystemBackend,
};
use async_trait::async_trait;
use std::sync::Arc;
use tokio::sync::RwLock;
use waypad_core::{
    backend::{
        AudioBackend, CaptureBackend, ControllerBackend, InputBackend, PlatformHost, SystemBackend,
    },
    capability::Capabilities,
    config::Config,
    state::StatePaths,
};

pub struct LinuxHost {
    capabilities: Arc<RwLock<Capabilities>>,
    capture: Arc<LinuxCaptureBackend>,
    audio: Arc<LinuxAudioBackend>,
    system: Arc<LinuxSystemBackend>,
}

impl LinuxHost {
    pub fn new(paths: StatePaths) -> Self {
        let capabilities = Arc::new(RwLock::new(Capabilities::default()));
        Self {
            capture: Arc::new(LinuxCaptureBackend::new(capabilities.clone(), paths)),
            audio: Arc::new(LinuxAudioBackend::new()),
            system: Arc::new(LinuxSystemBackend::new()),
            capabilities,
        }
    }
}

#[async_trait]
impl PlatformHost for LinuxHost {
    fn name(&self) -> &'static str {
        "linux-wayland"
    }

    fn hostname(&self) -> String {
        waypad_core::discovery::hostname()
    }

    async fn primary_lan_address(&self) -> Option<String> {
        // Asking the routing table which source address would reach the
        // internet is the only reliable way to pick one interface on a host
        // with a VPN, a bridge, and a docker0 all claiming to be local.
        command_output("ip", &["-4", "route", "get", "1.1.1.1"])
            .and_then(|raw| parse_ip_route_src(&raw))
            .or_else(|| {
                command_output("hostname", &["-I"]).and_then(|raw| {
                    raw.split_whitespace()
                        .find(|part| part.contains('.') && *part != "127.0.0.1")
                        .map(str::to_string)
                })
            })
    }

    fn capabilities(&self) -> Arc<RwLock<Capabilities>> {
        self.capabilities.clone()
    }

    async fn detect_capabilities(&self, config: &Config) -> Capabilities {
        let detected = capability::detect(config).await;
        *self.capabilities.write().await = detected.clone();
        detected
    }

    async fn input_backend(&self, capabilities: &Capabilities) -> Box<dyn InputBackend> {
        Box::new(InputManager::from_capabilities(capabilities).await)
    }

    fn controller_backend(&self, capabilities: &Capabilities) -> Box<dyn ControllerBackend> {
        Box::new(ControllerInputManager::new(
            capabilities.external_input.controller,
            capabilities
                .external_input
                .reason
                .clone()
                .unwrap_or_else(|| "Controller forwarding unsupported on this host".into()),
        ))
    }

    fn capture_backend(&self) -> Arc<dyn CaptureBackend> {
        self.capture.clone()
    }

    fn audio_backend(&self) -> Arc<dyn AudioBackend> {
        self.audio.clone()
    }

    fn system_backend(&self) -> Arc<dyn SystemBackend> {
        self.system.clone()
    }

    fn autostart_enabled(&self) -> anyhow::Result<bool> {
        crate::autostart::is_enabled()
    }

    fn set_autostart(&self, enabled: bool) -> anyhow::Result<()> {
        crate::autostart::set_enabled(enabled)
    }
}

/// Pulls the `src` address out of `ip -4 route get`, ignoring a loopback
/// answer: a host that would route to the internet over `lo` has nothing worth
/// advertising in an invite.
fn parse_ip_route_src(raw: &str) -> Option<String> {
    let mut parts = raw.split_whitespace();
    while let Some(part) = parts.next() {
        if part == "src" {
            return parts
                .next()
                .filter(|value| value.contains('.') && *value != "127.0.0.1")
                .map(str::to_string);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_route_source_address_for_invites() {
        let raw = "1.1.1.1 via 192.168.1.1 dev wlan0 src 192.168.1.40 uid 1000";
        assert_eq!(parse_ip_route_src(raw), Some("192.168.1.40".to_string()));
        assert_eq!(
            parse_ip_route_src("local 127.0.0.1 dev lo src 127.0.0.1"),
            None
        );
    }
}
