use crate::{
    capability::Capabilities,
    config::Config,
    crypto::{HostIdentity, SecureChannel},
    discovery,
    gamepad::ControllerInputManager,
    input::InputManager,
    protocol::{
        ApiError, ClientSecureMessage, Command, ExternalDeviceType, ExternalInputEvent,
        PROTOCOL_VERSION, response_empty, response_error, response_ok,
    },
    screen::{self, ScreenManager, StreamStartOptions},
    state::{
        StatePaths, TrustedDevice, TrustedDevices, load_trusted_devices, save_trusted_devices,
        validate_pairing_code,
    },
    system_control,
};
use anyhow::Context;
use serde_json::json;
use std::{
    collections::{HashMap, VecDeque},
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    net::{TcpListener, TcpStream},
    sync::{Mutex, RwLock},
    time::timeout,
};
use tracing::{debug, info, warn};

#[derive(Clone)]
pub struct AppState {
    pub config: Config,
    pub paths: StatePaths,
    pub identity: Arc<HostIdentity>,
    pub devices: Arc<Mutex<TrustedDevices>>,
    pub input: Arc<Mutex<InputManager>>,
    pub gamepad: Arc<Mutex<ControllerInputManager>>,
    pub capabilities: Arc<RwLock<Capabilities>>,
    pub screen: Arc<ScreenManager>,
    pub rate_limiter: Arc<Mutex<PairRateLimiter>>,
}

#[derive(Debug, Default)]
pub struct PairRateLimiter {
    attempts: HashMap<IpAddr, VecDeque<Instant>>,
}

impl PairRateLimiter {
    pub fn allow(&mut self, ip: IpAddr, max_per_minute: u32) -> bool {
        let now = Instant::now();
        let bucket = self.attempts.entry(ip).or_default();
        while bucket
            .front()
            .map(|t| now.duration_since(*t) > Duration::from_secs(60))
            .unwrap_or(false)
        {
            bucket.pop_front();
        }
        if bucket.len() >= max_per_minute as usize {
            return false;
        }
        bucket.push_back(now);
        true
    }
}

fn controller_manager_from_capabilities(capabilities: &Capabilities) -> ControllerInputManager {
    ControllerInputManager::new(
        capabilities.external_input.controller,
        capabilities
            .external_input
            .reason
            .clone()
            .unwrap_or_else(|| "Controller forwarding unsupported on this host".into()),
    )
}

async fn refresh_controller_manager(state: &AppState, capabilities: &Capabilities) {
    state.gamepad.lock().await.refresh(
        capabilities.external_input.controller,
        capabilities
            .external_input
            .reason
            .clone()
            .unwrap_or_else(|| "Controller forwarding unsupported on this host".into()),
    );
}

pub async fn run(config: Config, paths: StatePaths, identity: HostIdentity) -> anyhow::Result<()> {
    let devices = load_trusted_devices(&paths)?;
    let capabilities = Capabilities::detect(&config).await;
    let input = InputManager::from_capabilities(&capabilities).await;
    let gamepad = controller_manager_from_capabilities(&capabilities);
    let capabilities = Arc::new(RwLock::new(capabilities.clone()));
    let screen = Arc::new(ScreenManager::new(
        capabilities.clone(),
        config.control_port,
    ));
    let identity = Arc::new(identity);
    let state = AppState {
        config: config.clone(),
        paths,
        identity: identity.clone(),
        devices: Arc::new(Mutex::new(devices)),
        input: Arc::new(Mutex::new(input)),
        gamepad: Arc::new(Mutex::new(gamepad)),
        capabilities: capabilities.clone(),
        screen,
        rate_limiter: Arc::new(Mutex::new(PairRateLimiter::default())),
    };

    let discovery_config = config.clone();
    let discovery_identity = identity;
    tokio::spawn(async move {
        if let Err(err) = discovery::run_discovery(
            discovery_config,
            discovery_identity,
            capabilities.read().await.clone(),
        )
        .await
        {
            warn!(%err, "discovery listener stopped");
        }
    });

    let bind = format!("{}:{}", config.bind_address, config.control_port);
    let listener = TcpListener::bind(&bind).await?;
    info!("Waypad daemon listening on tcp://{bind}");
    loop {
        let (stream, peer) = listener.accept().await?;
        let state = state.clone();
        tokio::spawn(async move {
            if let Err(err) = handle_connection(stream, peer, state).await {
                debug!(%peer, %err, "connection closed");
            }
        });
    }
}

async fn handle_connection(
    mut stream: TcpStream,
    peer: SocketAddr,
    state: AppState,
) -> anyhow::Result<()> {
    if let Some(token) = maybe_accept_stream_attach(&mut stream).await? {
        info!(%peer, "screen stream attach request received on control port");
        state.screen.attach_stream_client(&token, stream).await?;
        return Ok(());
    }
    let (reader, writer) = stream.into_split();
    let mut channel = SecureChannel::server(reader, writer, &state.identity).await?;
    let mut authenticated: Option<TrustedDevice> = None;
    info!(%peer, "secure channel established");

    loop {
        let message: ClientSecureMessage = channel.recv().await?;
        match message {
            ClientSecureMessage::PairRequest {
                request_id,
                device_name,
                pairing_code,
                app_version,
            } => {
                let (response, device) = handle_pair(
                    &state,
                    peer,
                    request_id,
                    device_name,
                    pairing_code,
                    app_version,
                )
                .await;
                if let Some(device) = device {
                    authenticated = Some(device);
                }
                channel.send(&response).await?;
            }
            ClientSecureMessage::AuthRequest {
                request_id,
                device_id,
                session_token,
                app_version,
            } => {
                let (response, device) =
                    handle_auth(&state, request_id, device_id, session_token, app_version).await;
                if let Some(device) = device {
                    authenticated = Some(device);
                }
                channel.send(&response).await?;
            }
            ClientSecureMessage::Command {
                request_id,
                command,
            } => {
                if authenticated.is_none() {
                    channel
                        .send(&response_error(
                            request_id,
                            ApiError::new(
                                "unauthenticated",
                                "Authenticate before sending control commands",
                                false,
                            ),
                        ))
                        .await?;
                    continue;
                }
                let response = handle_command(&state, request_id, command).await;
                channel.send(&response).await?;
            }
            ClientSecureMessage::Ping { request_id } => {
                channel
                    .send(&response_ok(
                        request_id,
                        json!({
                            "pong": true,
                            "protocol": PROTOCOL_VERSION
                        }),
                    ))
                    .await?;
            }
        }
    }
}

async fn maybe_accept_stream_attach(stream: &mut TcpStream) -> anyhow::Result<Option<String>> {
    let mut peek = [0u8; 256];
    let n = timeout(Duration::from_secs(5), stream.peek(&mut peek)).await??;
    let preview = String::from_utf8_lossy(&peek[..n]);
    if !preview.contains("\"stream_connect\"") {
        return Ok(None);
    }
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    timeout(Duration::from_secs(5), reader.read_line(&mut line)).await??;
    let value: serde_json::Value = serde_json::from_str(&line)?;
    if value.get("type").and_then(serde_json::Value::as_str) != Some("stream_connect") {
        anyhow::bail!("Invalid stream attach preface");
    }
    let token = value
        .get("token")
        .and_then(serde_json::Value::as_str)
        .filter(|token| !token.is_empty())
        .context("Missing stream attach token")?
        .to_string();
    drop(reader);
    Ok(Some(token))
}

async fn handle_pair(
    state: &AppState,
    peer: SocketAddr,
    request_id: String,
    device_name: String,
    pairing_code: String,
    app_version: Option<String>,
) -> (crate::protocol::ServerSecureMessage, Option<TrustedDevice>) {
    if device_name.trim().is_empty() || device_name.len() > 80 {
        return (
            response_error(
                request_id,
                ApiError::new(
                    "invalid_device_name",
                    "Device name must be 1-80 characters",
                    false,
                ),
            ),
            None,
        );
    }
    if !state
        .rate_limiter
        .lock()
        .await
        .allow(peer.ip(), state.config.max_pair_attempts_per_minute)
    {
        return (
            response_error(
                request_id,
                ApiError::new(
                    "rate_limited",
                    "Too many pairing attempts; wait one minute",
                    true,
                ),
            ),
            None,
        );
    }
    if !can_pair_publicly(&state.config, peer) {
        warn!(%peer, "rejecting public pairing attempt because require_private_lan=true and allow_public_pairing=false");
        return (
            response_error(
                request_id,
                ApiError::new(
                    "public_pairing_denied",
                    "This host blocks pairing from public networks. To pair remotely, the host admin must set allow_public_pairing=true or require_private_lan=false in the daemon config.",
                    false,
                ),
            ),
            None,
        );
    }
    match validate_pairing_code(&state.paths, &pairing_code) {
        Ok(true) => {}
        Ok(false) => {
            return (
                response_error(
                    request_id,
                    ApiError::new(
                        "pairing_denied",
                        "Pairing code is missing, expired, or incorrect",
                        true,
                    ),
                ),
                None,
            );
        }
        Err(err) => {
            return (
                response_error(
                    request_id,
                    ApiError::new("pairing_state_error", err.to_string(), false),
                ),
                None,
            );
        }
    }

    let mut devices = state.devices.lock().await;
    let (device, token) = match devices.pair_device(device_name, app_version) {
        Ok(value) => value,
        Err(err) => {
            return (
                response_error(
                    request_id,
                    ApiError::new("pairing_failed", err.to_string(), false),
                ),
                None,
            );
        }
    };
    if let Err(err) = save_trusted_devices(&state.paths, &devices) {
        return (
            response_error(
                request_id,
                ApiError::new("pairing_store_failed", err.to_string(), false),
            ),
            None,
        );
    }
    let device_id = device.id.clone();
    let device_name = device.name.clone();
    info!(device_id = %device_id, device_name = %device_name, "paired trusted device");
    let capabilities = state.capabilities.read().await.clone();
    (
        response_ok(
            request_id,
            json!({
                "device_id": device_id,
                "session_token": token,
                "host_name": discovery::hostname(),
                "host_fingerprint": state.identity.fingerprint,
                "capabilities": capabilities
            }),
        ),
        Some(device),
    )
}

async fn handle_auth(
    state: &AppState,
    request_id: String,
    device_id: String,
    session_token: String,
    _app_version: Option<String>,
) -> (crate::protocol::ServerSecureMessage, Option<TrustedDevice>) {
    let mut devices = state.devices.lock().await;
    if let Some(device) = devices.authenticate(&device_id, &session_token) {
        if let Err(err) = save_trusted_devices(&state.paths, &devices) {
            return (
                response_error(
                    request_id,
                    ApiError::new("auth_store_failed", err.to_string(), false),
                ),
                None,
            );
        }
        info!(device_id = %device.id, device_name = %device.name, "authenticated device");
        let capabilities = state.capabilities.read().await.clone();
        (
            response_ok(
                request_id,
                json!({
                    "authenticated": true,
                    "host_name": discovery::hostname(),
                    "capabilities": capabilities
                }),
            ),
            Some(device),
        )
    } else {
        (
            response_error(
                request_id,
                ApiError::new(
                    "auth_failed",
                    "Unknown, revoked, or invalid device token",
                    false,
                ),
            ),
            None,
        )
    }
}

async fn handle_command(
    state: &AppState,
    request_id: String,
    command: Command,
) -> crate::protocol::ServerSecureMessage {
    let result: anyhow::Result<Option<serde_json::Value>> = async {
        match command {
            Command::GetHealth => Ok(Some(json!({
                "ok": true,
                "service": "waypad-daemon",
                "protocol": PROTOCOL_VERSION
            }))),
            Command::GetHostInfo => Ok(Some(json!({
                "host_name": discovery::hostname(),
                "host_fingerprint": state.identity.fingerprint,
                "protocol": PROTOCOL_VERSION
            }))),
            Command::GetCapabilities => {
                let capabilities = Capabilities::detect(&state.config).await;
                *state.capabilities.write().await = capabilities.clone();
                refresh_controller_manager(state, &capabilities).await;
                Ok(Some(json!(capabilities)))
            }
            Command::PrepareInput => {
                let mut input = state.input.lock().await;
                match input.prepare().await {
                    Ok(value) => Ok(Some(value)),
                    Err(first_error) => {
                        let capabilities = Capabilities::detect(&state.config).await;
                        *state.capabilities.write().await = capabilities.clone();
                        refresh_controller_manager(state, &capabilities).await;
                        *input = InputManager::from_capabilities(&capabilities).await;
                        input.prepare().await.map(Some).map_err(|second_error| {
                            anyhow::anyhow!("{first_error}; after refresh: {second_error}")
                        })
                    }
                }
            }
            Command::PointerMove { dx, dy } => {
                validate_delta(dx, dy)?;
                state.input.lock().await.pointer_move(dx, dy).await?;
                Ok(None)
            }
            Command::PointerMoveAbsolute { source_id, x, y } => {
                validate_absolute(x, y)?;
                let source = state.screen.source_by_id(source_id.as_deref()).await?;
                let input = state.input.lock().await;
                screen::pointer_move_absolute(&input, source, x, y).await?;
                Ok(None)
            }
            Command::PointerButton { button, state: st } => {
                state.input.lock().await.pointer_button(button, st).await?;
                Ok(None)
            }
            Command::Scroll { dx, dy, finish } => {
                validate_delta(dx, dy)?;
                state.input.lock().await.scroll(dx, dy, finish).await?;
                Ok(None)
            }
            Command::ExternalInput {
                device_id,
                device_type,
                event,
            } => handle_external_input(state, device_id, device_type, event)
                .await
                .map(|_| None),
            Command::Key { keysym, state: st } => {
                state.input.lock().await.key(keysym, st).await?;
                Ok(None)
            }
            Command::Text { text } => send_text(state, text).await.map(|_| None),
            Command::Shortcut { keys } => send_shortcut(state, keys).await.map(|_| None),
            Command::Media { action } => system_control::media(action).await.map(|_| None),
            Command::Volume { action } => system_control::volume(action).await.map(|_| None),
            Command::Brightness { action } => {
                system_control::brightness(action).await.map(|_| None)
            }
            Command::ClipboardSet { text } => {
                system_control::clipboard_set(&text).await.map(|_| None)
            }
            Command::ListScreenSources => Ok(Some(json!({
                "sources": state.screen.list_sources().await?
            }))),
            Command::StartScreenStream {
                source_id,
                max_fps,
                jpeg_quality,
                max_width,
                max_height,
            } => Ok(Some(json!(
                state
                    .screen
                    .start_stream(StreamStartOptions {
                        source_id,
                        max_fps,
                        jpeg_quality,
                        max_width,
                        max_height,
                    })
                    .await?
            ))),
            Command::StopScreenStream { session_id } => {
                state.screen.stop_stream(&session_id).await?;
                Ok(None)
            }
            Command::System { action } => system_control::system(&state.config, action)
                .await
                .map(|_| None),
        }
    }
    .await;

    match result {
        Ok(Some(data)) => response_ok(request_id, data),
        Ok(None) => response_empty(request_id),
        Err(err) => response_error(
            request_id,
            ApiError::new("command_failed", err.to_string(), true),
        ),
    }
}

async fn send_text(state: &AppState, text: String) -> anyhow::Result<()> {
    if text.len() > 4096 {
        anyhow::bail!("Text input rejected: maximum length is 4096 bytes");
    }
    state.input.lock().await.text(&text).await
}

async fn handle_external_input(
    state: &AppState,
    device_id: String,
    device_type: ExternalDeviceType,
    event: ExternalInputEvent,
) -> anyhow::Result<()> {
    match event {
        ExternalInputEvent::DeviceConnected { name, classes } => {
            info!(
                %device_id,
                ?device_type,
                %name,
                ?classes,
                "android external input device connected"
            );
            if is_controller_device(&device_type) || classes.iter().any(is_controller_device) {
                let mut gamepad = state.gamepad.lock().await;
                gamepad.device_connected(&device_id, &name)?;
                gamepad.flush_pending()?;
            }
            Ok(())
        }
        ExternalInputEvent::DeviceDisconnected => {
            info!(%device_id, ?device_type, "android external input device disconnected");
            if is_controller_device(&device_type) {
                let mut gamepad = state.gamepad.lock().await;
                gamepad.device_disconnected(&device_id)?;
                gamepad.flush_pending()?;
            }
            Ok(())
        }
        ExternalInputEvent::PointerMove { dx, dy } => {
            validate_delta(dx, dy)?;
            state.input.lock().await.pointer_move(dx, dy).await
        }
        ExternalInputEvent::PointerButton { button, state: st } => {
            state.input.lock().await.pointer_button(button, st).await
        }
        ExternalInputEvent::PointerScroll { dx, dy, finish } => {
            validate_delta(dx, dy)?;
            state.input.lock().await.scroll(dx, dy, finish).await
        }
        ExternalInputEvent::KeyboardKey {
            keysym,
            state: st,
            repeat,
        } => {
            debug!(%device_id, ?device_type, keysym, repeat, "android external keyboard key");
            state.input.lock().await.key(keysym, st).await
        }
        ExternalInputEvent::ControllerButton { button, state: st } => {
            debug!(%device_id, ?device_type, %button, ?st, "android external controller button");
            let mut gamepad = state.gamepad.lock().await;
            gamepad.button(&button, st)?;
            gamepad.flush_pending()
        }
        ExternalInputEvent::ControllerAxis { axis, value } => {
            if !value.is_finite() || !(-1.0..=1.0).contains(&value) {
                anyhow::bail!("Controller axis value out of range for {axis}: {value}");
            }
            debug!(%device_id, ?device_type, %axis, value, "android external controller axis");
            let mut gamepad = state.gamepad.lock().await;
            gamepad.axis(&axis, value)?;
            gamepad.flush_pending()
        }
    }
}

fn is_controller_device(device_type: &ExternalDeviceType) -> bool {
    matches!(
        device_type,
        ExternalDeviceType::Gamepad | ExternalDeviceType::Joystick
    )
}

async fn send_shortcut(state: &AppState, keys: Vec<String>) -> anyhow::Result<()> {
    if keys.is_empty() || keys.len() > 6 {
        anyhow::bail!("Shortcut must contain 1-6 keys");
    }
    let keysyms = keys
        .iter()
        .map(|key| shortcut_key_to_keysym(key))
        .collect::<anyhow::Result<Vec<_>>>()?;
    let input = state.input.lock().await;
    for keysym in &keysyms {
        input
            .key(*keysym, crate::protocol::ButtonState::Pressed)
            .await?;
    }
    for keysym in keysyms.iter().rev() {
        input
            .key(*keysym, crate::protocol::ButtonState::Released)
            .await?;
    }
    Ok(())
}

fn shortcut_key_to_keysym(key: &str) -> anyhow::Result<u32> {
    let lower = key.to_ascii_lowercase();
    let value = match lower.as_str() {
        "ctrl" | "control" => 0xffe3,
        "alt" => 0xffe9,
        "shift" => 0xffe1,
        "super" | "meta" | "win" => 0xffeb,
        "enter" | "return" => 0xff0d,
        "esc" | "escape" => 0xff1b,
        "tab" => 0xff09,
        "space" => 0x0020,
        "backspace" => 0xff08,
        "delete" => 0xffff,
        "left" => 0xff51,
        "up" => 0xff52,
        "right" => 0xff53,
        "down" => 0xff54,
        single if single.chars().count() == 1 => single.chars().next().unwrap() as u32,
        _ => anyhow::bail!("Unsupported shortcut key: {key}"),
    };
    Ok(value)
}

fn validate_delta(dx: f64, dy: f64) -> anyhow::Result<()> {
    if !dx.is_finite() || !dy.is_finite() || dx.abs() > 10_000.0 || dy.abs() > 10_000.0 {
        anyhow::bail!("Pointer delta rejected as invalid");
    }
    Ok(())
}

fn validate_absolute(x: f64, y: f64) -> anyhow::Result<()> {
    if !x.is_finite()
        || !y.is_finite()
        || x < -100_000.0
        || y < -100_000.0
        || x > 100_000.0
        || y > 100_000.0
    {
        anyhow::bail!("Absolute pointer coordinate rejected as invalid");
    }
    Ok(())
}

pub fn is_private_or_local(addr: SocketAddr) -> bool {
    match addr.ip() {
        IpAddr::V4(ip) => is_private_ipv4(ip),
        IpAddr::V6(ip) => is_private_ipv6(ip),
    }
}

fn is_private_ipv4(ip: Ipv4Addr) -> bool {
    ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || (ip.octets()[0] == 100 && (64..=127).contains(&ip.octets()[1]))
}

fn is_private_ipv6(ip: Ipv6Addr) -> bool {
    ip.is_loopback() || ip.is_unique_local() || ip.is_unicast_link_local()
}

fn can_pair_publicly(config: &Config, peer: SocketAddr) -> bool {
    if !config.require_private_lan {
        return true;
    }
    if config.allow_public_pairing {
        return true;
    }
    is_private_or_local(peer)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_lan_filter_rejects_public_addresses() {
        assert!(is_private_or_local("192.168.1.2:10".parse().unwrap()));
        assert!(is_private_or_local("127.0.0.1:10".parse().unwrap()));
        assert!(!is_private_or_local("8.8.8.8:10".parse().unwrap()));
    }

    #[test]
    fn can_pair_publicly_with_default_config_blocks_public() {
        let config = Config::default();
        assert!(!can_pair_publicly(&config, "8.8.8.8:10".parse().unwrap()));
        assert!(can_pair_publicly(&config, "192.168.1.2:10".parse().unwrap()));
        assert!(can_pair_publicly(&config, "127.0.0.1:10".parse().unwrap()));
    }

    #[test]
    fn can_pair_publicly_with_allow_public_pairing_allows_public() {
        let mut config = Config::default();
        config.allow_public_pairing = true;
        assert!(can_pair_publicly(&config, "8.8.8.8:10".parse().unwrap()));
        assert!(can_pair_publicly(&config, "192.168.1.2:10".parse().unwrap()));
    }

    #[test]
    fn can_pair_publicly_with_require_private_lan_false_allows_all() {
        let mut config = Config::default();
        config.require_private_lan = false;
        assert!(can_pair_publicly(&config, "8.8.8.8:10".parse().unwrap()));
        assert!(can_pair_publicly(&config, "192.168.1.2:10".parse().unwrap()));
    }

    #[test]
    fn shortcut_map_rejects_unknown_keys() {
        assert_eq!(shortcut_key_to_keysym("ctrl").unwrap(), 0xffe3);
        assert!(shortcut_key_to_keysym("definitely-not-a-key").is_err());
    }
}
