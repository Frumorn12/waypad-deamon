use crate::{
    config::Config,
    gamepad::detect_virtual_gamepad_support,
    platform::{
        SessionInfo, command_exists, command_output, detect_session, hyprland_ipc_available,
    },
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Capabilities {
    pub session: SessionInfo,
    pub portal: PortalCapability,
    pub input: InputCapability,
    pub external_input: ExternalInputCapability,
    pub connectivity: ConnectivityCapability,
    pub capture: CaptureCapability,
    pub system: SystemCapabilities,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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
    pub requires_user_approval: bool,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExternalInputCapability {
    pub pointer: bool,
    pub keyboard: bool,
    pub controller: bool,
    pub backend: String,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectivityCapability {
    pub lan_direct: bool,
    pub public_direct: bool,
    pub relay: bool,
    pub signaling: bool,
    pub stun: bool,
    pub turn: bool,
    pub backend: String,
    pub reason: Option<String>,
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
    pub hyprland_grim_available: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemCapabilities {
    pub volume: bool,
    pub media: bool,
    pub brightness: bool,
    pub clipboard: bool,
    pub lock: bool,
    pub suspend: bool,
}

impl Capabilities {
    pub async fn detect(config: &Config) -> Self {
        let session = detect_session();
        let portal = detect_portal().await;
        let screencast = detect_screencast_portal().await;
        let pointer = portal.available_device_types.iter().any(|d| d == "pointer");
        let keyboard = portal
            .available_device_types
            .iter()
            .any(|d| d == "keyboard");
        let wayland = session.session_type == "wayland";
        let portal_input_supported =
            wayland && portal.remote_desktop_available && (pointer || keyboard);
        let hyprland_ipc_fallback = wayland
            && !portal.remote_desktop_available
            && session.compositor_hint == "hyprland"
            && hyprland_ipc_available();
        let input_supported = portal_input_supported || hyprland_ipc_fallback;
        let (controller_supported, controller_reason) = detect_virtual_gamepad_support();
        let (input_backend, requires_user_approval, input_reason) = if !wayland {
            (
                "noop",
                false,
                Some("Remote input is only enabled for Wayland sessions in this daemon".into()),
            )
        } else if portal_input_supported {
            (
                "wayland-portal",
                true,
                Some(
                    "Input injection requires RemoteDesktop portal approval on this session".into(),
                ),
            )
        } else if hyprland_ipc_fallback {
            (
                "hyprland-ipc",
                false,
                Some(
                    "RemoteDesktop portal unavailable; using Hyprland IPC fallback for pointer, buttons, scroll, shortcuts, direct ASCII text, and clipboard-backed text for unsupported characters."
                        .into(),
                ),
            )
        } else if !portal.xdg_desktop_portal_available {
            (
                "noop",
                false,
                Some("Remote input unavailable: xdg-desktop-portal is not running".into()),
            )
        } else if !portal.remote_desktop_available {
            (
                "noop",
                false,
                Some(
                    "Remote input unavailable: org.freedesktop.portal.RemoteDesktop not available"
                        .into(),
                ),
            )
        } else {
            (
                "noop",
                false,
                Some(
                    "Remote input unavailable: portal exposes no pointer or keyboard devices"
                        .into(),
                ),
            )
        };

        let pipewire_runtime_available = command_exists("pipewire") || command_exists("pw-cli");
        let gstreamer_pipewire_available = command_exists("gst-launch-1.0")
            && command_output("gst-inspect-1.0", &["pipewiresrc"]).is_some()
            && command_output("gst-inspect-1.0", &["jpegenc"]).is_some();
        let hyprland_grim_available = wayland
            && session.compositor_hint == "hyprland"
            && command_exists("grim")
            && command_output("hyprctl", &["monitors", "-j"]).is_some();
        let portal_capture_supported = wayland
            && screencast.available
            && pipewire_runtime_available
            && gstreamer_pipewire_available;
        let capture_supported = portal_capture_supported || hyprland_grim_available;
        let (capture_backend, capture_requires_approval, capture_reason) = if !wayland {
            (
                "noop",
                false,
                Some("Screen capture is only enabled for Wayland sessions in this daemon".into()),
            )
        } else if portal_capture_supported {
            (
                "wayland-screencast-portal",
                true,
                Some(
                    "Screen capture uses XDG Desktop Portal ScreenCast and PipeWire approval"
                        .into(),
                ),
            )
        } else if hyprland_grim_available {
            (
                "hyprland-grim",
                false,
                Some(
                    "ScreenCast portal streaming dependencies are incomplete; using isolated Hyprland grim monitor capture fallback"
                        .into(),
                ),
            )
        } else if !screencast.xdg_desktop_portal_available {
            (
                "noop",
                false,
                Some("Screen capture unavailable: xdg-desktop-portal is not running".into()),
            )
        } else if !screencast.available {
            (
                "noop",
                false,
                Some("Screen capture unavailable: ScreenCast portal not available".into()),
            )
        } else if !pipewire_runtime_available {
            (
                "noop",
                false,
                Some("Screen capture unavailable: PipeWire runtime tools are missing".into()),
            )
        } else {
            (
                "noop",
                false,
                Some(
                    "Screen capture unavailable: GStreamer PipeWire/JPEG pipeline is missing"
                        .into(),
                ),
            )
        };

        Self {
            session,
            portal,
            input: InputCapability {
                supported: input_supported,
                backend: input_backend.into(),
                requires_user_approval,
                reason: input_reason,
            },
            external_input: ExternalInputCapability {
                pointer: input_supported,
                keyboard: input_supported,
                controller: controller_supported,
                backend: if input_supported && controller_supported {
                    format!("{input_backend}+uinput")
                } else if controller_supported {
                    "uinput".into()
                } else if input_supported {
                    input_backend.into()
                } else {
                    "noop".into()
                },
                reason: Some(external_input_reason(input_supported, &controller_reason)),
            },
            connectivity: ConnectivityCapability {
                lan_direct: true,
                public_direct: !config.require_private_lan,
                relay: false,
                signaling: false,
                stun: false,
                turn: false,
                backend: "direct-tcp-invite".into(),
                reason: Some(
                    "LAN and manually advertised public/direct endpoints are supported through QR invites. WebRTC ICE/STUN/TURN relay is not bundled in this daemon build."
                        .into(),
                ),
            },
            capture: CaptureCapability {
                supported: capture_supported,
                backend: capture_backend.into(),
                requires_user_approval: capture_requires_approval,
                reason: capture_reason,
                portal_screencast_available: screencast.available,
                screencast_version: screencast.version,
                available_source_types: screencast.available_source_types,
                available_cursor_modes: screencast.available_cursor_modes,
                pipewire_runtime_available,
                gstreamer_pipewire_available,
                hyprland_grim_available,
            },
            system: SystemCapabilities {
                volume: command_exists("wpctl") || command_exists("pactl"),
                media: command_exists("playerctl"),
                brightness: command_exists("brightnessctl"),
                clipboard: command_exists("wl-copy") && command_exists("wl-paste"),
                lock: command_exists("loginctl"),
                suspend: config.allow_suspend && command_exists("systemctl"),
            },
        }
    }
}

fn external_input_reason(input_supported: bool, controller_reason: &str) -> String {
    match input_supported {
        true => format!(
            "Android external mouse and keyboard events are forwarded through the active pointer/keyboard input backend. {controller_reason}"
        ),
        false => format!(
            "External mouse/keyboard forwarding requires a supported pointer/keyboard input backend on the host. {controller_reason}"
        ),
    }
}

#[derive(Debug)]
struct ScreenCastPortalInfo {
    xdg_desktop_portal_available: bool,
    available: bool,
    version: Option<u32>,
    available_source_types: Vec<String>,
    available_cursor_modes: Vec<String>,
}

async fn detect_screencast_portal() -> ScreenCastPortalInfo {
    let Ok(conn) = zbus::Connection::session().await else {
        return ScreenCastPortalInfo {
            xdg_desktop_portal_available: false,
            available: false,
            version: None,
            available_source_types: vec![],
            available_cursor_modes: vec![],
        };
    };

    let has_portal = match zbus::fdo::DBusProxy::new(&conn).await {
        Ok(proxy) => {
            let Ok(name) = zbus::names::BusName::try_from("org.freedesktop.portal.Desktop") else {
                return ScreenCastPortalInfo {
                    xdg_desktop_portal_available: false,
                    available: false,
                    version: None,
                    available_source_types: vec![],
                    available_cursor_modes: vec![],
                };
            };
            proxy.name_has_owner(name).await.unwrap_or(false)
        }
        Err(_) => false,
    };
    if !has_portal {
        return ScreenCastPortalInfo {
            xdg_desktop_portal_available: false,
            available: false,
            version: None,
            available_source_types: vec![],
            available_cursor_modes: vec![],
        };
    }

    let proxy = match zbus::Proxy::new(
        &conn,
        "org.freedesktop.portal.Desktop",
        "/org/freedesktop/portal/desktop",
        "org.freedesktop.portal.ScreenCast",
    )
    .await
    {
        Ok(proxy) => proxy,
        Err(_) => {
            return ScreenCastPortalInfo {
                xdg_desktop_portal_available: true,
                available: false,
                version: None,
                available_source_types: vec![],
                available_cursor_modes: vec![],
            };
        }
    };

    let version: Option<u32> = proxy.get_property("version").await.ok();
    let source_mask: u32 = proxy
        .get_property("AvailableSourceTypes")
        .await
        .unwrap_or(0);
    let cursor_mask: u32 = proxy
        .get_property("AvailableCursorModes")
        .await
        .unwrap_or(0);

    ScreenCastPortalInfo {
        xdg_desktop_portal_available: true,
        available: version.is_some(),
        version,
        available_source_types: source_type_names(source_mask),
        available_cursor_modes: cursor_mode_names(cursor_mask),
    }
}

fn source_type_names(mask: u32) -> Vec<String> {
    let mut names = Vec::new();
    if mask & 1 != 0 {
        names.push("monitor".into());
    }
    if mask & 2 != 0 {
        names.push("window".into());
    }
    if mask & 4 != 0 {
        names.push("virtual".into());
    }
    names
}

fn cursor_mode_names(mask: u32) -> Vec<String> {
    let mut names = Vec::new();
    if mask & 1 != 0 {
        names.push("hidden".into());
    }
    if mask & 2 != 0 {
        names.push("embedded".into());
    }
    if mask & 4 != 0 {
        names.push("metadata".into());
    }
    names
}

#[cfg(test)]
mod tests {
    use super::{cursor_mode_names, source_type_names};

    #[test]
    fn parses_screencast_source_type_mask() {
        assert_eq!(
            source_type_names(1 | 2 | 4),
            vec!["monitor", "window", "virtual"]
        );
        assert!(source_type_names(0).is_empty());
    }

    #[test]
    fn parses_screencast_cursor_mode_mask() {
        assert_eq!(cursor_mode_names(1 | 2), vec!["hidden", "embedded"]);
        assert_eq!(cursor_mode_names(4), vec!["metadata"]);
    }
}

async fn detect_portal() -> PortalCapability {
    let libei_runtime_available = command_exists("ei-debug-events")
        || std::process::Command::new("pkg-config")
            .args(["--exists", "libei-1.0"])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);

    let Ok(conn) = zbus::Connection::session().await else {
        return PortalCapability {
            xdg_desktop_portal_available: false,
            remote_desktop_available: false,
            remote_desktop_version: None,
            available_device_types: vec![],
            libei_advertised_by_portal: false,
            libei_runtime_available,
            reason: Some("Cannot connect to the D-Bus session bus".into()),
        };
    };

    let has_portal = match zbus::fdo::DBusProxy::new(&conn).await {
        Ok(proxy) => {
            let Ok(name) = zbus::names::BusName::try_from("org.freedesktop.portal.Desktop") else {
                return PortalCapability {
                    xdg_desktop_portal_available: false,
                    remote_desktop_available: false,
                    remote_desktop_version: None,
                    available_device_types: vec![],
                    libei_advertised_by_portal: false,
                    libei_runtime_available,
                    reason: Some("Invalid portal bus name".into()),
                };
            };
            proxy.name_has_owner(name).await.unwrap_or(false)
        }
        Err(_) => false,
    };
    if !has_portal {
        return PortalCapability {
            xdg_desktop_portal_available: false,
            remote_desktop_available: false,
            remote_desktop_version: None,
            available_device_types: vec![],
            libei_advertised_by_portal: false,
            libei_runtime_available,
            reason: Some("org.freedesktop.portal.Desktop has no owner on the session bus".into()),
        };
    }

    let proxy = match zbus::Proxy::new(
        &conn,
        "org.freedesktop.portal.Desktop",
        "/org/freedesktop/portal/desktop",
        "org.freedesktop.portal.RemoteDesktop",
    )
    .await
    {
        Ok(proxy) => proxy,
        Err(err) => {
            return PortalCapability {
                xdg_desktop_portal_available: true,
                remote_desktop_available: false,
                remote_desktop_version: None,
                available_device_types: vec![],
                libei_advertised_by_portal: false,
                libei_runtime_available,
                reason: Some(format!("RemoteDesktop proxy unavailable: {err}")),
            };
        }
    };

    let version: Option<u32> = proxy.get_property("version").await.ok();
    let device_mask: Option<u32> = proxy.get_property("AvailableDeviceTypes").await.ok();
    let mut available_device_types = Vec::new();
    let mask = device_mask.unwrap_or(0);
    if mask & 1 != 0 {
        available_device_types.push("keyboard".into());
    }
    if mask & 2 != 0 {
        available_device_types.push("pointer".into());
    }
    if mask & 4 != 0 {
        available_device_types.push("touchscreen".into());
    }

    PortalCapability {
        xdg_desktop_portal_available: true,
        remote_desktop_available: version.is_some(),
        remote_desktop_version: version,
        available_device_types,
        libei_advertised_by_portal: version.unwrap_or(0) >= 2,
        libei_runtime_available,
        reason: None,
    }
}
