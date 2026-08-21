//! The Waypad host control panel.
//!
//! A daemon with no window is a daemon nobody can use. Everything the CLI does
//! — pairing codes, invites, trusted devices, diagnostics, login start — is
//! served here as one page on the loopback interface, so the answer to "how do
//! I pair my phone" stops being "open a terminal".
//!
//! Deliberately a page in the user's own browser rather than an embedded
//! webview: no runtime to install, no extra megabytes in the installer, the
//! same panel on Linux and Windows, and it can be opened from the phone on the
//! same LAN if that is ever useful.
//!
//! # Access
//!
//! It binds to `127.0.0.1` only, on a port the operating system picks, and
//! every route requires a token generated at startup. Loopback alone would not
//! be enough: any process on the machine can reach it, and any website the user
//! visits can make their browser POST to it. Neither can guess the token.

pub mod http;
pub mod logs;
mod page;

pub use logs::{LogBuffer, LogBufferLayer};

use anyhow::Context;
use http::{Request, Response};
use serde_json::json;
use std::sync::Arc;
use tokio::net::{TcpListener, TcpStream};
use tracing::{debug, info, warn};
use waypad_core::{
    backend::PlatformHost,
    config::Config,
    crypto::{HostIdentity, random_url_token},
    invite::{Invite, InviteRoute, qr_svg},
    state::{StatePaths, create_pairing_code, load_trusted_devices, save_trusted_devices},
};

/// Everything the panel needs to answer for the daemon it belongs to.
pub struct PanelContext {
    pub config: Config,
    pub paths: StatePaths,
    pub identity: Arc<HostIdentity>,
    pub host: Arc<dyn PlatformHost>,
    pub logs: LogBuffer,
}

/// A running panel.
pub struct ControlPanel {
    url: String,
}

impl ControlPanel {
    /// The address to open, token included.
    pub fn url(&self) -> &str {
        &self.url
    }
}

/// Binds the panel and serves it until the process ends.
pub async fn start(context: PanelContext) -> anyhow::Result<ControlPanel> {
    // Port zero: the operating system picks a free one. A fixed port would
    // collide with whatever else the user runs and would also let a local
    // process find the panel without being told where it is.
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .context("could not bind the control panel to the loopback interface")?;
    let port = listener.local_addr()?.port();
    let token = random_url_token().map_err(|err| anyhow::anyhow!("{err}"))?;
    let url = format!("http://127.0.0.1:{port}/?token={token}");
    info!(port, "control panel listening on the loopback interface");

    let shared = Arc::new(Panel {
        context,
        token: token.clone(),
    });
    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, _peer)) => {
                    let panel = shared.clone();
                    tokio::spawn(async move {
                        if let Err(err) = panel.serve(stream).await {
                            debug!(%err, "control panel connection closed");
                        }
                    });
                }
                Err(err) => {
                    warn!(%err, "control panel listener stopped");
                    return;
                }
            }
        }
    });
    Ok(ControlPanel { url })
}

struct Panel {
    context: PanelContext,
    token: String,
}

impl Panel {
    async fn serve(&self, mut stream: TcpStream) -> anyhow::Result<()> {
        let request = http::read_request(&mut stream).await?;
        let response = self.route(&request).await;
        http::write_response(&mut stream, response).await
    }

    async fn route(&self, request: &Request) -> Response {
        // Checked before anything is dispatched, including the page itself, so
        // no route can be reached without it.
        if !request
            .token()
            .is_some_and(|token| http::secret_eq(token, &self.token))
        {
            return Response::error(403, "Missing or invalid panel token");
        }
        match (request.method.as_str(), request.path.as_str()) {
            ("GET", "/") => Response::html(page::render()),
            ("GET", "/api/status") => self.status().await,
            ("GET", "/api/logs") => self.logs(request),
            ("POST", "/api/pair-code") => self.pair_code().await,
            ("POST", "/api/devices/revoke") => self.revoke(request),
            ("POST", "/api/autostart") => self.set_autostart(request),
            ("GET", _) | ("POST", _) => Response::error(404, "No such route"),
            _ => Response::error(405, "Method not allowed"),
        }
    }

    async fn status(&self) -> Response {
        let capabilities = self
            .context
            .host
            .detect_capabilities(&self.context.config)
            .await;
        let devices = load_trusted_devices(&self.context.paths)
            .map(|devices| devices.devices)
            .unwrap_or_default();
        // A failure here is reported as unknown rather than as an error for the
        // whole page: not being able to read the Run key should not stop
        // someone from seeing their pairing code.
        let autostart = self.context.host.autostart_enabled();
        Response::json(
            json!({
                "host_name": self.context.host.hostname(),
                "platform": self.context.host.name(),
                "fingerprint": self.context.identity.fingerprint,
                "control_port": self.context.config.control_port,
                "discovery_port": self.context.config.discovery_port,
                "require_private_lan": self.context.config.require_private_lan,
                "allow_public_pairing": self.context.config.allow_public_pairing,
                "allow_suspend": self.context.config.allow_suspend,
                "autostart": autostart.as_ref().ok(),
                "autostart_error": autostart.as_ref().err().map(|err| format!("{err:#}")),
                "capabilities": capabilities,
                "devices": devices,
            })
            .to_string(),
        )
    }

    fn logs(&self, request: &Request) -> Response {
        let since = request
            .query
            .get("since")
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(0);
        Response::json(json!({ "lines": self.context.logs.since(since) }).to_string())
    }

    async fn pair_code(&self) -> Response {
        let code = match create_pairing_code(&self.context.config, &self.context.paths) {
            Ok(code) => code,
            Err(err) => return Response::error(500, format!("{err:#}")),
        };
        let address = self
            .context
            .host
            .primary_lan_address()
            .await
            .unwrap_or_else(|| "127.0.0.1".into());
        let invite = Invite {
            host: self.context.host.hostname(),
            address: address.clone(),
            lan_address: address,
            remote_address: None,
            port: self.context.config.control_port,
            fingerprint: self.context.identity.fingerprint.clone(),
            code: code.code.clone(),
            expires_at: code.expires_at,
            route: InviteRoute::DirectLan,
            policy: "lan-only",
            allow_public_pairing: self.context.config.allow_public_pairing,
        };
        let payload = invite.payload();
        let qr = match qr_svg(&payload) {
            Ok(svg) => svg,
            Err(err) => return Response::error(500, format!("{err:#}")),
        };
        Response::json(
            json!({
                "code": code.code,
                "expires_at": code.expires_at,
                "invite": payload,
                "qr_svg": qr,
            })
            .to_string(),
        )
    }

    fn revoke(&self, request: &Request) -> Response {
        let Some(id) = request.query.get("id").filter(|id| !id.is_empty()) else {
            return Response::error(400, "Missing device id");
        };
        let mut devices = match load_trusted_devices(&self.context.paths) {
            Ok(devices) => devices,
            Err(err) => return Response::error(500, format!("{err:#}")),
        };
        if !devices.revoke(id) {
            return Response::error(404, "No trusted device with that id");
        }
        if let Err(err) = save_trusted_devices(&self.context.paths, &devices) {
            return Response::error(500, format!("{err:#}"));
        }
        info!(device_id = %id, "revoked a device from the control panel");
        Response::json(r#"{"ok":true}"#)
    }

    fn set_autostart(&self, request: &Request) -> Response {
        let enabled = request
            .query
            .get("enabled")
            .map(|value| value == "true")
            .unwrap_or(false);
        match self.context.host.set_autostart(enabled) {
            Ok(()) => {
                info!(enabled, "login start changed from the control panel");
                Response::json(json!({ "ok": true, "enabled": enabled }).to_string())
            }
            Err(err) => Response::error(500, format!("{err:#}")),
        }
    }
}

/// Opens the panel in whatever the user's default browser is.
///
/// Best effort by design: a host with no browser, or none the daemon may
/// launch, still runs perfectly well and the URL is printed for the user.
pub fn open_in_browser(url: &str) -> anyhow::Result<()> {
    #[cfg(windows)]
    let result = std::process::Command::new("cmd")
        // The empty string is the window title `start` expects first; without
        // it a quoted URL is taken as the title and nothing opens.
        .args(["/C", "start", "", url])
        .spawn();
    #[cfg(not(windows))]
    let result = std::process::Command::new("xdg-open").arg(url).spawn();

    result
        .map(|_| ())
        .context("could not launch a browser for the control panel")
}
