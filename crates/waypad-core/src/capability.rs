//! The capability model the Android client reads to decide what to show.
//!
//! These are the wire structs and nothing else: detection lives in the platform
//! crates, because "is there a RemoteDesktop portal" and "is there a DXGI
//! output" are not the same question asked twice.
//!
//! Every field is deliberately reported even when unsupported, with a `reason`
//! the client shows verbatim. A capability that is merely absent leaves the user
//! with a control that does nothing and no explanation; a capability reported as
//! false with a reason tells them what to install or approve.
//!
//! The shape is frozen by the shipped client: fields may be added, but existing
//! names and types must keep serialising the same way.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Capabilities {
    pub session: SessionInfo,
    pub portal: PortalCapability,
    pub input: InputCapability,
    pub external_input: ExternalInputCapability,
    pub connectivity: ConnectivityCapability,
    pub capture: CaptureCapability,
    pub audio_capture: AudioCaptureCapability,
    pub system: SystemCapabilities,
}

/// What kind of graphical session the daemon found itself in.
///
/// The Wayland-shaped fields stay in the struct on every platform and are simply
/// `None` off Linux: the client already tolerates absent values, and changing
/// the shape would break clients that are already installed. Windows reports
/// `session_type: "windows"` and `compositor_hint: "windows-dwm"`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub session_type: String,
    pub wayland_display: Option<String>,
    pub x11_display: Option<String>,
    pub current_desktop: Option<String>,
    pub desktop_session: Option<String>,
    pub hyprland_instance_signature: Option<String>,
    pub compositor_hint: String,
    pub hyprctl_version: Option<String>,
}

impl Default for SessionInfo {
    fn default() -> Self {
        Self {
            session_type: "unknown".into(),
            wayland_display: None,
            x11_display: None,
            current_desktop: None,
            desktop_session: None,
            hyprland_instance_signature: None,
            compositor_hint: "unknown".into(),
            hyprctl_version: None,
        }
    }
}

/// Portal detection results. Entirely a Linux concern; left at its default of
/// "nothing available" on hosts that have no such thing.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PortalCapability {
    pub xdg_desktop_portal_available: bool,
    pub remote_desktop_available: bool,
    pub remote_desktop_version: Option<u32>,
    pub available_device_types: Vec<String>,
    pub libei_advertised_by_portal: bool,
    pub libei_runtime_available: bool,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InputCapability {
    pub supported: bool,
    pub backend: String,
    /// Whether the user must approve something locally before input works.
    /// False on Windows: `SendInput` needs no consent step.
    pub requires_user_approval: bool,
    pub reason: Option<String>,
}

impl Default for InputCapability {
    fn default() -> Self {
        Self {
            supported: false,
            backend: "noop".into(),
            requires_user_approval: false,
            reason: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExternalInputCapability {
    pub pointer: bool,
    pub keyboard: bool,
    pub controller: bool,
    pub backend: String,
    pub reason: Option<String>,
}

impl Default for ExternalInputCapability {
    fn default() -> Self {
        Self {
            pointer: false,
            keyboard: false,
            controller: false,
            backend: "noop".into(),
            reason: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectivityCapability {
    pub lan_direct: bool,
    pub public_direct: bool,
    pub public_pairing_allowed: bool,
    pub relay: bool,
    pub signaling: bool,
    pub stun: bool,
    pub turn: bool,
    pub backend: String,
    pub reason: Option<String>,
}

impl Default for ConnectivityCapability {
    fn default() -> Self {
        Self {
            lan_direct: true,
            public_direct: false,
            public_pairing_allowed: false,
            relay: false,
            signaling: false,
            stun: false,
            turn: false,
            backend: "direct".into(),
            reason: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaptureCapability {
    pub supported: bool,
    pub backend: String,
    pub requires_user_approval: bool,
    pub reason: Option<String>,
    pub portal_screencast_available: bool,
    pub screencast_version: Option<u32>,
    pub available_source_types: Vec<String>,
    pub available_cursor_modes: Vec<String>,
    pub pipewire_runtime_available: bool,
    pub gstreamer_pipewire_available: bool,
    pub h264_encoder: Option<String>,
    pub hyprland_grim_available: bool,
}

impl Default for CaptureCapability {
    fn default() -> Self {
        Self {
            supported: false,
            backend: "noop".into(),
            requires_user_approval: false,
            reason: None,
            portal_screencast_available: false,
            screencast_version: None,
            available_source_types: Vec::new(),
            available_cursor_modes: Vec::new(),
            pipewire_runtime_available: false,
            gstreamer_pipewire_available: false,
            h264_encoder: None,
            hyprland_grim_available: false,
        }
    }
}

/// Whether the desktop's own output can be streamed alongside the picture.
///
/// Reported separately from [`CaptureCapability`] on purpose: audio is optional
/// and a host that cannot capture it still streams video, so the two must be
/// able to disagree.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioCaptureCapability {
    pub supported: bool,
    pub backend: String,
    pub codec: Option<String>,
    pub sample_rate: u32,
    pub channels: u16,
    /// Resolved at detection time only for diagnostics; the stream re-resolves
    /// the default output when it starts, so switching device is picked up.
    pub default_sink: Option<String>,
    pub monitor_source: Option<String>,
    pub pactl_available: bool,
    pub gstreamer_opus_available: bool,
    pub missing_elements: Vec<String>,
    pub reason: Option<String>,
}

impl Default for AudioCaptureCapability {
    fn default() -> Self {
        Self {
            supported: false,
            backend: "noop".into(),
            codec: None,
            sample_rate: crate::audio::AUDIO_SAMPLE_RATE,
            channels: crate::audio::AUDIO_CHANNELS,
            default_sink: None,
            monitor_source: None,
            pactl_available: false,
            gstreamer_opus_available: false,
            missing_elements: Vec::new(),
            reason: None,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SystemCapabilities {
    pub volume: bool,
    pub media: bool,
    pub brightness: bool,
    pub clipboard: bool,
    pub lock: bool,
    pub suspend: bool,
}

/// Explains external-device forwarding in one sentence covering both halves.
///
/// Pointer and keyboard follow whatever the input backend can do, while
/// controller support is independent, so a host commonly supports one and not
/// the other and the reason has to say which.
pub fn external_input_reason(input_supported: bool, controller_reason: &str) -> String {
    if input_supported {
        format!("Pointer and keyboard forwarding follow the active input backend. {controller_reason}")
    } else {
        format!("Pointer and keyboard forwarding are unavailable because remote input is unavailable. {controller_reason}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_describe_a_host_that_supports_nothing() {
        let capabilities = Capabilities::default();
        assert!(!capabilities.input.supported);
        assert_eq!(capabilities.input.backend, "noop");
        assert!(!capabilities.capture.supported);
        assert!(!capabilities.audio_capture.supported);
        // LAN direct is the one thing every host can do: it is how the daemon
        // is reached in the first place.
        assert!(capabilities.connectivity.lan_direct);
    }

    #[test]
    fn serialises_with_the_field_names_the_android_client_reads() {
        let raw = serde_json::to_string(&Capabilities::default()).unwrap();
        for field in [
            "session",
            "portal",
            "input",
            "external_input",
            "connectivity",
            "capture",
            "audio_capture",
            "system",
            "requires_user_approval",
            "public_pairing_allowed",
            "h264_encoder",
        ] {
            assert!(raw.contains(field), "missing {field} in {raw}");
        }
    }
}
