use crate::{
    capability::Capabilities,
    protocol::{ButtonState, PointerButton},
};
use anyhow::{Context, bail};
use futures_util::StreamExt;
use serde_json::json;
use std::collections::HashMap;
use tokio::time::{Duration, timeout};
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

#[derive(Debug)]
pub enum InputManager {
    Noop { reason: String },
    Portal(WaylandPortalInputBackend),
}

impl InputManager {
    pub async fn from_capabilities(capabilities: &Capabilities) -> Self {
        if capabilities.input.backend != "wayland-portal" {
            return Self::Noop {
                reason: capabilities
                    .input
                    .reason
                    .clone()
                    .unwrap_or_else(|| "Remote input unsupported on this host".into()),
            };
        }
        match WaylandPortalInputBackend::new().await {
            Ok(backend) => Self::Portal(backend),
            Err(err) => Self::Noop {
                reason: format!("Remote input unavailable: {err}"),
            },
        }
    }

    pub async fn prepare(&mut self) -> anyhow::Result<serde_json::Value> {
        match self {
            Self::Noop { reason } => bail!("{reason}"),
            Self::Portal(backend) => backend.prepare().await,
        }
    }

    pub async fn pointer_move(&self, dx: f64, dy: f64) -> anyhow::Result<()> {
        match self {
            Self::Noop { reason } => bail!("{reason}"),
            Self::Portal(backend) => backend.pointer_move(dx, dy).await,
        }
    }

    pub async fn pointer_button(
        &self,
        button: PointerButton,
        state: ButtonState,
    ) -> anyhow::Result<()> {
        match self {
            Self::Noop { reason } => bail!("{reason}"),
            Self::Portal(backend) => backend.pointer_button(button, state).await,
        }
    }

    pub async fn scroll(&self, dx: f64, dy: f64, finish: bool) -> anyhow::Result<()> {
        match self {
            Self::Noop { reason } => bail!("{reason}"),
            Self::Portal(backend) => backend.scroll(dx, dy, finish).await,
        }
    }

    pub async fn key(&self, keysym: u32, state: ButtonState) -> anyhow::Result<()> {
        match self {
            Self::Noop { reason } => bail!("{reason}"),
            Self::Portal(backend) => backend.key(keysym, state).await,
        }
    }
}

#[derive(Debug)]
pub struct WaylandPortalInputBackend {
    connection: zbus::Connection,
    session_handle: Option<OwnedObjectPath>,
    granted_devices: u32,
}

impl WaylandPortalInputBackend {
    pub async fn new() -> anyhow::Result<Self> {
        let connection = zbus::Connection::session().await?;
        Ok(Self {
            connection,
            session_handle: None,
            granted_devices: 0,
        })
    }

    pub async fn prepare(&mut self) -> anyhow::Result<serde_json::Value> {
        if self.session_handle.is_some() {
            return Ok(json!({
                "backend": "wayland-portal",
                "status": "ready",
                "devices": self.granted_devices
            }));
        }

        let proxy = self.proxy().await?;
        let mut create_options = HashMap::<&str, OwnedValue>::new();
        create_options.insert(
            "handle_token",
            Value::from(format!("waypad_create_{}", unique_token())).try_into()?,
        );
        create_options.insert(
            "session_handle_token",
            Value::from(format!("waypad_session_{}", unique_token())).try_into()?,
        );
        let create_handle: OwnedObjectPath = proxy.call("CreateSession", &(create_options)).await?;
        let create_response = wait_request(&self.connection, &create_handle).await?;
        if create_response.response != 0 {
            bail!("Input injection requires portal approval on this session");
        }
        let session_handle_string = create_response
            .results
            .get("session_handle")
            .and_then(owned_value_to_string)
            .context("RemoteDesktop portal did not return a session handle")?;
        let session_handle = OwnedObjectPath::try_from(session_handle_string.as_str())?;

        let mut select_options = HashMap::<&str, OwnedValue>::new();
        select_options.insert("types", Value::from(1u32 | 2u32).try_into()?);
        select_options.insert("persist_mode", Value::from(1u32).try_into()?);
        select_options.insert(
            "handle_token",
            Value::from(format!("waypad_select_{}", unique_token())).try_into()?,
        );
        let select_handle: OwnedObjectPath = proxy
            .call("SelectDevices", &(&session_handle, select_options))
            .await?;
        let select_response = wait_request(&self.connection, &select_handle).await?;
        if select_response.response != 0 {
            bail!("RemoteDesktop device selection was denied by the portal");
        }

        let mut start_options = HashMap::<&str, OwnedValue>::new();
        start_options.insert(
            "handle_token",
            Value::from(format!("waypad_start_{}", unique_token())).try_into()?,
        );
        let start_handle: OwnedObjectPath = proxy
            .call("Start", &(&session_handle, "", start_options))
            .await?;
        let start_response = wait_request(&self.connection, &start_handle).await?;
        if start_response.response != 0 {
            bail!("RemoteDesktop portal approval was denied or cancelled");
        }
        let devices = start_response
            .results
            .get("devices")
            .and_then(owned_value_to_u32)
            .unwrap_or(0);
        if devices & (1 | 2) == 0 {
            bail!("RemoteDesktop portal approved no keyboard or pointer devices");
        }

        self.session_handle = Some(session_handle);
        self.granted_devices = devices;
        Ok(json!({
            "backend": "wayland-portal",
            "status": "ready",
            "devices": devices
        }))
    }

    pub async fn pointer_move(&self, dx: f64, dy: f64) -> anyhow::Result<()> {
        let session = self.session()?;
        self.proxy()
            .await?
            .call::<_, _, ()>("NotifyPointerMotion", &(session, empty_options(), dx, dy))
            .await?;
        Ok(())
    }

    pub async fn pointer_button(
        &self,
        button: PointerButton,
        state: ButtonState,
    ) -> anyhow::Result<()> {
        let session = self.session()?;
        self.proxy()
            .await?
            .call::<_, _, ()>(
                "NotifyPointerButton",
                &(
                    session,
                    empty_options(),
                    button.evdev_code(),
                    state.portal_state(),
                ),
            )
            .await?;
        Ok(())
    }

    pub async fn scroll(&self, dx: f64, dy: f64, finish: bool) -> anyhow::Result<()> {
        let session = self.session()?;
        let mut options = HashMap::<&str, OwnedValue>::new();
        options.insert("finish", Value::from(finish).try_into()?);
        self.proxy()
            .await?
            .call::<_, _, ()>("NotifyPointerAxis", &(session, options, dx, dy))
            .await?;
        Ok(())
    }

    pub async fn key(&self, keysym: u32, state: ButtonState) -> anyhow::Result<()> {
        let session = self.session()?;
        self.proxy()
            .await?
            .call::<_, _, ()>(
                "NotifyKeyboardKeysym",
                &(
                    session,
                    empty_options(),
                    keysym as i32,
                    state.portal_state(),
                ),
            )
            .await?;
        Ok(())
    }

    async fn proxy(&self) -> anyhow::Result<zbus::Proxy<'_>> {
        Ok(zbus::Proxy::new(
            &self.connection,
            "org.freedesktop.portal.Desktop",
            "/org/freedesktop/portal/desktop",
            "org.freedesktop.portal.RemoteDesktop",
        )
        .await?)
    }

    fn session(&self) -> anyhow::Result<&OwnedObjectPath> {
        self.session_handle
            .as_ref()
            .context("Input injection requires portal approval on this session")
    }
}

#[derive(Debug)]
struct PortalResponse {
    response: u32,
    results: HashMap<String, OwnedValue>,
}

async fn wait_request(
    connection: &zbus::Connection,
    handle: &OwnedObjectPath,
) -> anyhow::Result<PortalResponse> {
    let proxy = zbus::Proxy::new(
        connection,
        "org.freedesktop.portal.Desktop",
        handle.as_str(),
        "org.freedesktop.portal.Request",
    )
    .await?;
    let mut stream = proxy.receive_signal("Response").await?;
    let message = timeout(Duration::from_secs(120), stream.next())
        .await
        .context("Timed out waiting for portal response")?
        .context("Portal request signal stream ended")?;
    let body = message.body();
    let (response, results): (u32, HashMap<String, OwnedValue>) = body.deserialize()?;
    Ok(PortalResponse { response, results })
}

fn empty_options() -> HashMap<&'static str, OwnedValue> {
    HashMap::new()
}

fn unique_token() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

fn owned_value_to_string(value: &OwnedValue) -> Option<String> {
    <&str>::try_from(value).map(|s| s.to_string()).ok()
}

fn owned_value_to_u32(value: &OwnedValue) -> Option<u32> {
    u32::try_from(value).ok()
}
