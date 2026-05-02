use crate::platform::{SessionInfo, command_exists, detect_session};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Capabilities {
    pub session: SessionInfo,
    pub portal: PortalCapability,
    pub input: InputCapability,
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
pub struct SystemCapabilities {
    pub volume: bool,
    pub media: bool,
    pub brightness: bool,
    pub clipboard: bool,
    pub lock: bool,
    pub suspend: bool,
}

impl Capabilities {
    pub async fn detect(allow_suspend: bool) -> Self {
        let session = detect_session();
        let portal = detect_portal().await;
        let pointer = portal.available_device_types.iter().any(|d| d == "pointer");
        let keyboard = portal
            .available_device_types
            .iter()
            .any(|d| d == "keyboard");
        let wayland = session.session_type == "wayland";
        let input_supported = wayland && portal.remote_desktop_available && (pointer || keyboard);
        let input_reason = if !wayland {
            Some("Remote input is only enabled for Wayland sessions in this daemon".into())
        } else if !portal.xdg_desktop_portal_available {
            Some("Remote input unavailable: xdg-desktop-portal is not running".into())
        } else if !portal.remote_desktop_available {
            Some(
                "Remote input unavailable: org.freedesktop.portal.RemoteDesktop not available"
                    .into(),
            )
        } else if !(pointer || keyboard) {
            Some("Remote input unavailable: portal exposes no pointer or keyboard devices".into())
        } else {
            Some("Input injection requires RemoteDesktop portal approval on this session".into())
        };

        Self {
            session,
            portal,
            input: InputCapability {
                supported: input_supported,
                backend: if input_supported {
                    "wayland-portal".into()
                } else {
                    "noop".into()
                },
                requires_user_approval: input_supported,
                reason: input_reason,
            },
            system: SystemCapabilities {
                volume: command_exists("wpctl") || command_exists("pactl"),
                media: command_exists("playerctl"),
                brightness: command_exists("brightnessctl"),
                clipboard: command_exists("wl-copy") && command_exists("wl-paste"),
                lock: command_exists("loginctl"),
                suspend: allow_suspend && command_exists("systemctl"),
            },
        }
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
