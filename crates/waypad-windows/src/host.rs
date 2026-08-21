//! The Windows implementation of `waypad_core::backend::PlatformHost`.
//!
//! Input and system control are real. Screen capture and desktop audio are not
//! written yet, and say so: the capability model exists precisely so a host can
//! be honest about what it cannot do instead of offering a control that fails
//! with a shrug.

use crate::{input::SendInputBackend, screen::WindowsCaptureBackend, system::WindowsSystemBackend};
use anyhow::bail;
use async_trait::async_trait;
use std::sync::Arc;
use tokio::sync::RwLock;
use waypad_core::{
    audio::AudioStreamOptions,
    backend::{
        AudioBackend, AudioProducer, CaptureBackend, ControllerBackend, InputBackend, PlatformHost,
        SystemBackend, UnsupportedController,
    },
    capability::{
        AudioCaptureCapability, Capabilities, CaptureCapability, ConnectivityCapability,
        ExternalInputCapability, InputCapability, SessionInfo, external_input_reason,
    },
    config::Config,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN,
};

const CONTROLLER_REASON: &str =
    "Controller forwarding needs the ViGEmBus driver, which Waypad does not install yet.";
const CAPTURE_REASON: &str = "Screen capture is not implemented on Windows yet; DXGI Desktop Duplication and the Media \
     Foundation H.264 encoder are still to be written.";
const AUDIO_REASON: &str = "Desktop audio is not implemented on Windows yet; WASAPI loopback capture is still to be \
     written.";

pub struct WindowsHost {
    capabilities: Arc<RwLock<Capabilities>>,
    capture: Arc<WindowsCaptureBackend>,
    audio: Arc<UnimplementedAudio>,
    system: Arc<WindowsSystemBackend>,
}

impl WindowsHost {
    pub fn new() -> Self {
        Self {
            capabilities: Arc::new(RwLock::new(Capabilities::default())),
            capture: Arc::new(WindowsCaptureBackend::new()),
            audio: Arc::new(UnimplementedAudio),
            system: Arc::new(WindowsSystemBackend::new()),
        }
    }
}

impl Default for WindowsHost {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl PlatformHost for WindowsHost {
    fn name(&self) -> &'static str {
        "windows"
    }

    fn hostname(&self) -> String {
        waypad_core::discovery::hostname()
    }

    async fn primary_lan_address(&self) -> Option<String> {
        // Opening a UDP socket towards a public address makes the routing table
        // pick the outgoing interface without sending a packet, which is the
        // one method that survives a host with a VPN and several adapters all
        // claiming to be local.
        let socket = tokio::net::UdpSocket::bind("0.0.0.0:0").await.ok()?;
        socket.connect("1.1.1.1:80").await.ok()?;
        let address = socket.local_addr().ok()?.ip().to_string();
        (address != "0.0.0.0" && !address.starts_with("127.")).then_some(address)
    }

    fn capabilities(&self) -> Arc<RwLock<Capabilities>> {
        self.capabilities.clone()
    }

    async fn detect_capabilities(&self, config: &Config) -> Capabilities {
        // SAFETY: GetSystemMetrics reads a global and cannot fail.
        let (width, height) = unsafe {
            (
                GetSystemMetrics(SM_CXVIRTUALSCREEN),
                GetSystemMetrics(SM_CYVIRTUALSCREEN),
            )
        };
        let input_supported = width > 0 && height > 0;
        // Enumeration is cheap and is the only honest way to answer: a session
        // with no attached output cannot be duplicated no matter what else is
        // true of the machine.
        let capture_supported = crate::capture::enumerate_outputs()
            .map(|outputs| !outputs.is_empty())
            .unwrap_or(false);
        let detected = Capabilities {
            session: SessionInfo {
                session_type: "windows".into(),
                compositor_hint: "windows-dwm".into(),
                ..SessionInfo::default()
            },
            input: InputCapability {
                supported: input_supported,
                backend: if input_supported {
                    "windows-sendinput".into()
                } else {
                    "noop".into()
                },
                // Windows lets a process in the user's own session synthesise
                // input with no consent step, so there is nothing to approve.
                requires_user_approval: false,
                reason: Some(if input_supported {
                    format!(
                        "Input is injected with SendInput across a {width}x{height} virtual desktop"
                    )
                } else {
                    "Remote input unavailable: Windows reports no virtual desktop, which usually \
                     means this session has no interactive display attached"
                        .into()
                }),
            },
            external_input: ExternalInputCapability {
                pointer: input_supported,
                keyboard: input_supported,
                controller: false,
                backend: if input_supported {
                    "windows-sendinput".into()
                } else {
                    "noop".into()
                },
                reason: Some(external_input_reason(input_supported, CONTROLLER_REASON)),
            },
            connectivity: ConnectivityCapability {
                public_direct: !config.require_private_lan,
                public_pairing_allowed: !config.require_private_lan || config.allow_public_pairing,
                backend: "direct-tcp-invite".into(),
                reason: Some(
                    "LAN and manually advertised public/direct endpoints are supported through QR \
                     invites. WebRTC ICE/STUN/TURN relay is not bundled in this daemon build."
                        .into(),
                ),
                ..ConnectivityCapability::default()
            },
            capture: CaptureCapability {
                supported: capture_supported,
                backend: if capture_supported {
                    "windows-dxgi".into()
                } else {
                    "noop".into()
                },
                // Nothing to approve: duplication is granted to any process in
                // the user's own session.
                requires_user_approval: false,
                reason: Some(if capture_supported {
                    CAPTURE_REASON.into()
                } else {
                    "Screen capture unavailable: Windows reports no monitor attached to this                      session"
                        .to_string()
                }),
                h264_encoder: capture_supported.then(|| "media-foundation".to_string()),
                ..CaptureCapability::default()
            },
            audio_capture: AudioCaptureCapability {
                supported: false,
                backend: "noop".into(),
                reason: Some(AUDIO_REASON.into()),
                ..AudioCaptureCapability::default()
            },
            system: crate::system::detect_system_capabilities(config.allow_suspend),
            ..Capabilities::default()
        };
        *self.capabilities.write().await = detected.clone();
        detected
    }

    async fn input_backend(&self, _capabilities: &Capabilities) -> Box<dyn InputBackend> {
        Box::new(SendInputBackend::new())
    }

    fn controller_backend(&self, _capabilities: &Capabilities) -> Box<dyn ControllerBackend> {
        Box::new(UnsupportedController::new(CONTROLLER_REASON))
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
}

/// Placeholder until WASAPI loopback capture is written.
#[derive(Debug)]
struct UnimplementedAudio;

#[async_trait]
impl AudioBackend for UnimplementedAudio {
    fn name(&self) -> &'static str {
        "windows-unimplemented"
    }

    async fn probe(&self) -> AudioCaptureCapability {
        AudioCaptureCapability {
            supported: false,
            backend: "noop".into(),
            reason: Some(AUDIO_REASON.into()),
            ..AudioCaptureCapability::default()
        }
    }

    async fn open(&self, _options: AudioStreamOptions) -> anyhow::Result<Box<dyn AudioProducer>> {
        bail!("{AUDIO_REASON}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn reports_input_as_supported_and_capture_as_an_explained_gap() {
        let host = WindowsHost::new();
        let capabilities = host.detect_capabilities(&Config::default()).await;

        assert!(capabilities.input.supported);
        assert_eq!(capabilities.input.backend, "windows-sendinput");
        assert!(!capabilities.input.requires_user_approval);
        assert_eq!(capabilities.session.session_type, "windows");

        // The remaining gaps are reported, not hidden: a client that sees
        // `supported: false` with no reason has nothing to show the user.
        assert!(!capabilities.audio_capture.supported);
        assert!(capabilities.audio_capture.reason.is_some());
        assert!(!capabilities.external_input.controller);
        assert!(
            capabilities
                .external_input
                .reason
                .as_deref()
                .unwrap_or_default()
                .contains("ViGEmBus")
        );

        // What does work is reported as working.
        assert!(capabilities.system.volume);
        assert!(capabilities.system.media);
        assert!(capabilities.system.clipboard);
        assert!(capabilities.external_input.pointer);
        assert!(capabilities.external_input.keyboard);
    }

    #[tokio::test]
    async fn the_capabilities_cell_is_shared_not_copied() {
        // The capture backend reads this cell, so a refresh has to land in the
        // same one the server holds or the two silently drift apart.
        let host = WindowsHost::new();
        let cell = host.capabilities();
        assert_eq!(cell.read().await.input.backend, "noop");
        host.detect_capabilities(&Config::default()).await;
        assert_eq!(cell.read().await.input.backend, "windows-sendinput");
    }

    #[tokio::test]
    async fn capture_is_reported_as_working_now_that_it_is() {
        let host = WindowsHost::new();
        let capabilities = host.detect_capabilities(&Config::default()).await;
        assert!(capabilities.capture.supported);
        assert_eq!(capabilities.capture.backend, "windows-dxgi");
        assert!(!capabilities.capture.requires_user_approval);
        assert_eq!(
            capabilities.capture.h264_encoder.as_deref(),
            Some("media-foundation")
        );
    }
}
