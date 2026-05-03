use crate::{capability::Capabilities, input::InputManager, platform::command_output};
use anyhow::{Context, bail};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    collections::HashMap,
    os::fd::AsRawFd,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    process::{ChildStderr, Command},
    sync::{Mutex, RwLock, oneshot},
    task::JoinHandle,
    time::{MissedTickBehavior, interval, timeout},
};
use tracing::{debug, info, warn};
use uuid::Uuid;
use zbus::zvariant::{OwnedFd, OwnedObjectPath, OwnedValue, Value};

const STREAM_MAGIC: &[u8] = b"WAYPAD_STREAM_V1\n";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ScreenSource {
    pub id: String,
    pub label: String,
    pub kind: String,
    pub backend: String,
    pub width: u32,
    pub height: u32,
    pub x: i32,
    pub y: i32,
    pub scale: f64,
    pub focused: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StreamStartOptions {
    pub source_id: Option<String>,
    pub max_fps: Option<u32>,
    pub jpeg_quality: Option<u8>,
    pub max_width: Option<u32>,
    pub max_height: Option<u32>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StreamStartResponse {
    pub session_id: String,
    pub stream_port: u16,
    pub token: String,
    pub codec: String,
    pub transport: String,
    pub source: ScreenSource,
}

#[derive(Debug)]
pub struct ScreenManager {
    capabilities: Arc<RwLock<Capabilities>>,
    stream_port: u16,
    sessions: Arc<Mutex<HashMap<String, StreamSession>>>,
    paths: Arc<super::state::StatePaths>,
}

#[derive(Debug)]
enum StreamSession {
    Pending(PendingStream),
    Running(RunningStream),
}

#[derive(Debug)]
struct PendingStream {
    token: String,
    source: ScreenSource,
    fps: u32,
    quality: u8,
    max_width: Option<u32>,
    max_height: Option<u32>,
}

#[derive(Debug)]
struct RunningStream {
    stop: oneshot::Sender<()>,
    task: JoinHandle<()>,
}

impl ScreenManager {
    pub fn new(capabilities: Arc<RwLock<Capabilities>>, stream_port: u16, paths: super::state::StatePaths) -> Self {
        Self {
            capabilities,
            stream_port,
            sessions: Arc::new(Mutex::new(HashMap::new())),
            paths: Arc::new(paths),
        }
    }

    pub async fn list_sources(&self) -> anyhow::Result<Vec<ScreenSource>> {
        let capabilities = self.capabilities.read().await.clone();
        let portal_available = capabilities.capture.portal_screencast_available
            && capabilities.capture.pipewire_runtime_available
            && capabilities.capture.gstreamer_pipewire_available;
        let has_restore_token = crate::state::load_portal_restore_token(&self.paths).is_some();

        let mut sources = Vec::new();

        if portal_available {
            sources.push(ScreenSource {
                id: "portal:chooser".into(),
                label: "Portal picker (60 FPS, one-time approval)".into(),
                kind: "chooser".into(),
                backend: "wayland-screencast-portal".into(),
                width: 0,
                height: 0,
                x: 0,
                y: 0,
                scale: 1.0,
                focused: true, // Default: portal is preferred
            });
        }
        if capabilities.capture.hyprland_grim_available {
            let monitors = hyprland_monitor_sources().await.unwrap_or_else(|err| {
                warn!(%err, "failed to enumerate Hyprland monitors");
                Vec::new()
            });
            for monitor in monitors {
                sources.push(monitor);
            }
        }
        if sources.is_empty() {
            bail!(
                "{}",
                capabilities
                    .capture
                    .reason
                    .clone()
                    .unwrap_or_else(|| "Screen capture unavailable on this host".into())
            );
        }
        Ok(sources)
    }

    pub async fn source_by_id(
        &self,
        source_id: Option<&str>,
    ) -> anyhow::Result<Option<ScreenSource>> {
        if source_id.is_none_or(str::is_empty) {
            return Ok(None);
        }
        self.select_source(source_id).await.map(Some)
    }

    pub async fn start_stream(
        &self,
        options: StreamStartOptions,
    ) -> anyhow::Result<StreamStartResponse> {
        let source = self.select_source(options.source_id.as_deref()).await?;

        // If portal is selected but never approved, silently switch to grim
        let source = if source.backend == "wayland-screencast-portal"
            && crate::state::load_portal_restore_token(&self.paths).is_none()
        {
            // Portal might work with app_id now; let it try but warn
            warn!("portal selected without restore_token; will attempt portal (app_id now provided)");
            source
        } else {
            source
        };

        let is_grim = source.backend == "hyprland-grim";
        let fps = options.max_fps.unwrap_or(30).clamp(1, 60);
        let fps = if is_grim { fps.min(30) } else { fps };
        let quality = options.jpeg_quality.unwrap_or(70).clamp(35, 92);
        let max_width = options.max_width.map(|value| value.clamp(480, 3840));
        let max_height = options.max_height.map(|value| value.clamp(480, 3840));
        let session_id = Uuid::new_v4().to_string();
        let token = Uuid::new_v4().to_string();
        // Save the selected source for future sessions
        if let Err(err) = crate::state::save_preferred_source(&self.paths, &source.id) {
            warn!(%err, source_id = %source.id, "failed to save preferred source");
        }
        self.sessions.lock().await.insert(
            session_id.clone(),
            StreamSession::Pending(PendingStream {
                token: token.clone(),
                source: source.clone(),
                fps,
                quality,
                max_width,
                max_height,
            }),
        );
        info!(
            %session_id,
            stream_port = self.stream_port,
            source_id = %source.id,
            backend = %source.backend,
            fps,
            quality,
            ?max_width,
            ?max_height,
            "screen stream session pending client attach"
        );
        Ok(StreamStartResponse {
            session_id,
            stream_port: self.stream_port,
            token,
            codec: "jpeg".into(),
            transport: "waypad-control-port-stream-v2".into(),
            source,
        })
    }

    pub async fn stop_stream(&self, session_id: &str) -> anyhow::Result<()> {
        let Some(session) = self.sessions.lock().await.remove(session_id) else {
            debug!(%session_id, "screen stream stop ignored because session is already closed");
            return Ok(());
        };
        match session {
            StreamSession::Pending(_) => {
                info!(%session_id, "pending screen stream session stopped before client attach");
            }
            StreamSession::Running(mut running) => {
                info!(%session_id, "screen stream stop requested");
                let _ = running.stop.send(());
                match timeout(Duration::from_secs(2), &mut running.task).await {
                    Ok(Ok(())) => {}
                    Ok(Err(err)) if err.is_cancelled() => {}
                    Ok(Err(err)) => {
                        warn!(%session_id, %err, "screen stream task ended with join error")
                    }
                    Err(_) => {
                        warn!(%session_id, "screen stream task did not stop gracefully; aborting");
                        running.task.abort();
                    }
                }
            }
        }
        Ok(())
    }

    pub async fn attach_stream_client(
        &self,
        token: &str,
        mut socket: TcpStream,
    ) -> anyhow::Result<()> {
        let (session_id, pending) = {
            let mut sessions = self.sessions.lock().await;
            let session_id = sessions
                .iter()
                .find_map(|(session_id, session)| match session {
                    StreamSession::Pending(pending) if pending.token == token => {
                        Some(session_id.clone())
                    }
                    _ => None,
                })
                .context("Unknown or expired screen stream token")?;
            let Some(StreamSession::Pending(pending)) = sessions.remove(&session_id) else {
                bail!("Screen stream token is not pending");
            };
            (session_id, pending)
        };

        let peer = socket.peer_addr().ok();
        socket.write_all(STREAM_MAGIC).await?;
        info!(
            %session_id,
            ?peer,
            source_id = %pending.source.id,
            backend = %pending.source.backend,
            "screen stream client attached"
        );
        let (stop_tx, mut stop_rx) = oneshot::channel();
        let task_sessions = self.sessions.clone();
        let task_session = session_id.clone();
        let source = pending.source.clone();
        let task_paths = self.paths.clone();
        let task = tokio::spawn(async move {
            let result = if source.backend == "wayland-screencast-portal" {
                let portal_result = run_portal_stream(
                    &mut socket,
                    task_session.clone(),
                    source.clone(),
                    pending.fps,
                    pending.quality,
                    pending.max_width,
                    pending.max_height,
                    &mut stop_rx,
                    task_paths.as_ref().clone(),
                )
                .await;
                match portal_result {
                    Ok(()) => Ok(()),
                    Err(portal_err) => {
                        if is_client_disconnect(&portal_err) {
                            Err(portal_err)
                        } else {
                            warn!(session_id = %task_session, %portal_err, "portal stream failed; falling back to grim");
                            // Use grim with the same connection
                            run_grim_stream_on_open(
                                &mut socket,
                                task_session.clone(),
                                source,
                                pending.fps,
                                pending.quality,
                                pending.max_width,
                                pending.max_height,
                                &mut stop_rx,
                            )
                            .await
                        }
                    }
                }
            } else {
                run_grim_stream_on_open(
                    &mut socket,
                    task_session.clone(),
                    source,
                    pending.fps,
                    pending.quality,
                    pending.max_width,
                    pending.max_height,
                    &mut stop_rx,
                )
                .await
            };
            if let Err(err) = result {
                if is_client_disconnect(&err) {
                    info!(session_id = %task_session, %err, "screen stream client disconnected");
                } else {
                    warn!(session_id = %task_session, %err, "screen stream stopped with error");
                }
            }
            task_sessions.lock().await.remove(&task_session);
            debug!(session_id = %task_session, "screen stream session removed from registry");
        });
        self.sessions.lock().await.insert(
            session_id,
            StreamSession::Running(RunningStream {
                stop: stop_tx,
                task,
            }),
        );
        Ok(())
    }

    async fn select_source(&self, requested: Option<&str>) -> anyhow::Result<ScreenSource> {
        let sources = self.list_sources().await?;
        if let Some(id) = requested.filter(|value| !value.is_empty()) {
            sources
                .into_iter()
                .find(|source| source.id == id)
                .with_context(|| format!("Screen source not found: {id}"))
        } else {
            // Try preferred source first, then focused, then first
            let preferred = crate::state::load_preferred_source(&self.paths);
            if let Some(ref pref_id) = preferred {
                if let Some(source) = sources.iter().find(|s| s.id == *pref_id) {
                    info!(source_id = %pref_id, "restored preferred screen source");
                    return Ok(source.clone());
                }
            }
            sources
                .iter()
                .find(|source| source.focused)
                .cloned()
                .or_else(|| sources.first().cloned())
                .context("No screen sources available")
        }
    }
}

pub async fn pointer_move_absolute(
    input: &InputManager,
    source: Option<ScreenSource>,
    x: f64,
    y: f64,
) -> anyhow::Result<()> {
    if !x.is_finite()
        || !y.is_finite()
        || x < -100_000.0
        || y < -100_000.0
        || x > 100_000.0
        || y > 100_000.0
    {
        bail!("Absolute pointer coordinate rejected as invalid");
    }
    match source {
        Some(source) if source.backend == "hyprland-grim" => {
            input
                .pointer_move_absolute(source.x as f64 + x, source.y as f64 + y)
                .await
        }
        _ => input.pointer_move_absolute(x, y).await,
    }
}

async fn run_grim_stream_on_open(
    socket: &mut TcpStream,
    session_id: String,
    source: ScreenSource,
    fps: u32,
    quality: u8,
    max_width: Option<u32>,
    max_height: Option<u32>,
    stop_rx: &mut oneshot::Receiver<()>,
) -> anyhow::Result<()> {
    run_grim_stream_impl(socket, session_id, source, fps, quality, max_width, max_height, stop_rx).await
}

async fn run_grim_stream_impl(
    socket: &mut TcpStream,
    session_id: String,
    source: ScreenSource,
    fps: u32,
    quality: u8,
    max_width: Option<u32>,
    max_height: Option<u32>,
    stop_rx: &mut oneshot::Receiver<()>,
) -> anyhow::Result<()> {
    let mut ticker = interval(Duration::from_secs_f64(1.0 / f64::from(fps.max(1))));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut seq = 0u64;
    // Force aggressive scale for grim (screenshot tool is slow at full res)
    let requested_scale = capture_scale(source.width, source.height, max_width, max_height);
    let scale = requested_scale.min(0.4); // Never capture above 40% resolution for grim
    info!(%session_id, source_id = %source.id, fps, quality, scale, requested_scale, "grim stream started");
    let mut frame_count = 0u64;
    let mut throughput_start = tokio::time::Instant::now();
    loop {
        tokio::select! {
            _ = &mut *stop_rx => break,
            _ = ticker.tick() => {
                let jpeg = capture_grim_frame(&source, quality, scale).await?;
                send_frame_grim(&mut *socket, seq, source.width, source.height, &jpeg).await?;
                seq += 1;
                frame_count += 1;
                let elapsed = throughput_start.elapsed().as_secs_f64();
                if elapsed >= 2.0 {
                    let measured = frame_count as f64 / elapsed;
                    debug!(%session_id, fps_measured = measured, fps_target = fps, frames = frame_count, "grim stream throughput");
                    frame_count = 0;
                    throughput_start = tokio::time::Instant::now();
                }
            }
        }
    }
    debug!(%session_id, "grim stream stopped");
    Ok(())
}

async fn run_portal_stream(
    socket: &mut TcpStream,
    session_id: String,
    _selected_source: ScreenSource,
    fps: u32,
    quality: u8,
    max_width: Option<u32>,
    max_height: Option<u32>,
    stop_rx: &mut oneshot::Receiver<()>,
    paths: super::state::StatePaths,
) -> anyhow::Result<()> {
    info!(%session_id, fps, quality, "portal stream client connected; starting ScreenCast approval");

    let restore_token = crate::state::load_portal_restore_token(&paths);
    let portal = match PortalScreenCastSession::start(restore_token).await {
        Ok(portal) => portal,
        Err(first_err) => {
            let had_restore = crate::state::load_portal_restore_token(&paths).is_some();
            if had_restore {
                warn!(%session_id, %first_err, "portal restore failed; retrying without restore token");
                PortalScreenCastSession::start(None).await?
            } else {
                return Err(first_err);
            }
        }
    };
    if let Some(ref token) = portal.restore_token {
        if let Err(err) = crate::state::save_portal_restore_token(&paths, token) {
            warn!(%session_id, %err, "failed to save portal restore token");
        }
    }
    let source = ScreenSource {
        id: format!("portal:stream:{}", portal.stream_id),
        label: "Portal-selected source".into(),
        kind: "portal-stream".into(),
        backend: "wayland-screencast-portal".into(),
        width: portal.width.unwrap_or(0),
        height: portal.height.unwrap_or(0),
        x: 0,
        y: 0,
        scale: 1.0,
        focused: true,
    };
    let (target_width, target_height) =
        target_dimensions(source.width, source.height, max_width, max_height);
    let mut child = spawn_gstreamer_pipewire(portal, fps, quality, target_width, target_height)?;
    if let Some(stderr) = child.stderr.take() {
        tokio::spawn(log_child_stderr(session_id.clone(), "gstreamer", stderr));
    }
    let mut stdout = child
        .stdout
        .take()
        .context("GStreamer stdout unavailable")?;
    let mut reader = JpegStreamReader::new();
    let mut buffer = [0u8; 32 * 1024];
    let mut seq = 0u64;
    let mut frame_count = 0u64;
    let mut throughput_start = tokio::time::Instant::now();
    info!(%session_id, source_id = %source.id, ?target_width, ?target_height, "portal stream started");
    loop {
        tokio::select! {
            _ = &mut *stop_rx => break,
            read = stdout.read(&mut buffer) => {
                let n = read?;
                if n == 0 {
                    warn!(%session_id, "portal stream producer closed stdout");
                    break;
                }
                for frame in reader.push(&buffer[..n]) {
                    send_frame(&mut *socket, seq, source.width, source.height, &frame).await?;
                    seq += 1;
                    frame_count += 1;
                }
                let elapsed = throughput_start.elapsed().as_secs_f64();
                if elapsed >= 2.0 {
                    let measured = frame_count as f64 / elapsed;
                    debug!(%session_id, fps_measured = measured, fps_target = fps, frames = frame_count, "portal stream throughput");
                    frame_count = 0;
                    throughput_start = tokio::time::Instant::now();
                }
            }
        }
    }
    let _ = child.kill().await;
    if seq == 0 {
        // GStreamer pipeline failed before producing any frames
        // Kill child and return error so the grim fallback can take over
        anyhow::bail!("Portal GStreamer pipeline produced no frames (PipeWire format may be incompatible)");
    }
    debug!(%session_id, "portal stream stopped");
    Ok(())
}

async fn log_child_stderr(session_id: String, label: &'static str, mut stderr: ChildStderr) {
    let mut buffer = [0u8; 2048];
    loop {
        match stderr.read(&mut buffer).await {
            Ok(0) => break,
            Ok(n) => {
                let text = String::from_utf8_lossy(&buffer[..n]).trim().to_string();
                if !text.is_empty() {
                    warn!(%session_id, producer = label, stderr = %text, "screen stream producer stderr");
                }
            }
            Err(err) => {
                warn!(%session_id, producer = label, %err, "failed to read screen stream producer stderr");
                break;
            }
        }
    }
}

const SEND_FRAME_DEADLINE_MS: u64 = 12;

async fn send_frame(
    socket: &mut TcpStream,
    seq: u64,
    width: u32,
    height: u32,
    jpeg: &[u8],
) -> anyhow::Result<()> {
    send_frame_deadline(socket, seq, width, height, jpeg, SEND_FRAME_DEADLINE_MS).await
}

async fn send_frame_grim(
    socket: &mut TcpStream,
    seq: u64,
    width: u32,
    height: u32,
    jpeg: &[u8],
) -> anyhow::Result<()> {
    // Grim frames are large JPEG screenshots — no deadline, send at TCP speed
    let header = json!({
        "seq": seq,
        "timestamp_ms": now_millis(),
        "codec": "jpeg",
        "width": width,
        "height": height
    })
    .to_string();
    let header = header.as_bytes();
    let header_len = (header.len() as u32).to_be_bytes();
    let payload_len = (jpeg.len() as u32).to_be_bytes();

    let total = 4 + 4 + header.len() + jpeg.len();
    let mut buf = Vec::with_capacity(total);
    buf.extend_from_slice(&header_len);
    buf.extend_from_slice(&payload_len);
    buf.extend_from_slice(header);
    buf.extend_from_slice(jpeg);

    socket.write_all(&buf).await?;
    Ok(())
}

async fn send_frame_deadline(
    socket: &mut TcpStream,
    seq: u64,
    width: u32,
    height: u32,
    jpeg: &[u8],
    deadline_ms: u64,
) -> anyhow::Result<()> {
    let header = json!({
        "seq": seq,
        "timestamp_ms": now_millis(),
        "codec": "jpeg",
        "width": width,
        "height": height
    })
    .to_string();
    let header = header.as_bytes();
    let header_len = (header.len() as u32).to_be_bytes();
    let payload_len = (jpeg.len() as u32).to_be_bytes();

    let total = 4 + 4 + header.len() + jpeg.len();
    let mut buf = Vec::with_capacity(total);
    buf.extend_from_slice(&header_len);
    buf.extend_from_slice(&payload_len);
    buf.extend_from_slice(header);
    buf.extend_from_slice(jpeg);

    let result = timeout(Duration::from_millis(deadline_ms), async {
        let mut offset = 0;
        while offset < buf.len() {
            offset += socket.write(&buf[offset..]).await?;
        }
        Ok::<_, std::io::Error>(())
    })
    .await;

    match result {
        Ok(Ok(())) => Ok(()),
        Ok(Err(io_err)) => Err(anyhow::Error::new(io_err)),
        Err(_elapsed) => {
            debug!(seq, "dropping frame: send deadline exceeded");
            Err(anyhow::anyhow!("frame send deadline exceeded (dropped)"))
        }
    }
}

fn now_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn is_client_disconnect(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        cause.downcast_ref::<std::io::Error>().is_some_and(|io| {
            matches!(
                io.kind(),
                std::io::ErrorKind::BrokenPipe
                    | std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::ConnectionAborted
                    | std::io::ErrorKind::UnexpectedEof
            )
        })
    })
}

fn capture_scale(width: u32, height: u32, max_width: Option<u32>, max_height: Option<u32>) -> f64 {
    let width_scale = max_width
        .filter(|_| width > 0)
        .map(|value| f64::from(value) / f64::from(width))
        .unwrap_or(1.0);
    let height_scale = max_height
        .filter(|_| height > 0)
        .map(|value| f64::from(value) / f64::from(height))
        .unwrap_or(1.0);
    width_scale.min(height_scale).min(1.0).max(0.1)
}

fn target_dimensions(
    width: u32,
    height: u32,
    max_width: Option<u32>,
    max_height: Option<u32>,
) -> (Option<u32>, Option<u32>) {
    if width == 0 || height == 0 {
        return (None, None);
    }
    let scale = capture_scale(width, height, max_width, max_height);
    if scale >= 0.999 {
        (None, None)
    } else {
        (
            Some((f64::from(width) * scale).round().max(2.0) as u32),
            Some((f64::from(height) * scale).round().max(2.0) as u32),
        )
    }
}

async fn capture_grim_frame(
    source: &ScreenSource,
    quality: u8,
    scale: f64,
) -> anyhow::Result<Vec<u8>> {
    let mut command = Command::new("grim");
    // Use much lower quality for streaming speed (cap at 35)
    let stream_quality = quality.min(35);
    command.args(["-t", "jpeg", "-q", &stream_quality.to_string()]);
    if scale < 0.999 {
        command.args(["-s", &format!("{scale:.4}")]);
    }
    if let Some(output) = source.id.strip_prefix("hyprland:monitor:") {
        command.args(["-o", output]);
    }
    // Output to stdout (no file, no cursor for speed)
    command.arg("-");
    let output = command.output().await.context("failed to run grim")?;
    if !output.status.success() {
        bail!(
            "grim capture failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(output.stdout)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct HyprlandMonitor {
    name: String,
    description: String,
    width: u32,
    height: u32,
    x: i32,
    y: i32,
    scale: f64,
    focused: bool,
    disabled: bool,
}

async fn hyprland_monitor_sources() -> anyhow::Result<Vec<ScreenSource>> {
    let raw = command_output("hyprctl", &["monitors", "-j"])
        .context("hyprctl monitors -j unavailable")?;
    let monitors: Vec<HyprlandMonitor> = serde_json::from_str(&raw)?;
    Ok(monitors
        .into_iter()
        .filter(|monitor| !monitor.disabled)
        .map(|monitor| ScreenSource {
            id: format!("hyprland:monitor:{}", monitor.name),
            label: format!("{} ({})", monitor.name, monitor.description),
            kind: "monitor".into(),
            backend: "hyprland-grim".into(),
            width: monitor.width,
            height: monitor.height,
            x: monitor.x,
            y: monitor.y,
            scale: monitor.scale,
            focused: monitor.focused,
        })
        .collect())
}

struct PortalScreenCastSession {
    stream_id: u32,
    width: Option<u32>,
    height: Option<u32>,
    pipewire_fd: OwnedFd,
    restore_token: Option<String>,
    connection: Option<zbus::Connection>,
    session_handle: Option<OwnedObjectPath>,
}

impl PortalScreenCastSession {
    async fn start(restore_token: Option<String>) -> anyhow::Result<Self> {
        let connection = zbus::Connection::session().await?;
        let proxy = zbus::Proxy::new(
            &connection,
            "org.freedesktop.portal.Desktop",
            "/org/freedesktop/portal/desktop",
            "org.freedesktop.portal.ScreenCast",
        )
        .await
        .context("ScreenCast portal not available")?;

        let mut create_options = HashMap::<&str, OwnedValue>::new();
        create_options.insert(
            "handle_token",
            Value::from(format!("waypad_create_{}", portal_token())).try_into()?,
        );
        let session_token = format!("waypad_screen_{}", portal_token());
        create_options.insert(
            "session_handle_token",
            Value::from(session_token).try_into()?,
        );
        
        if let Some(ref token) = restore_token {
            create_options.insert("restore_token", Value::from(token.as_str()).try_into()?);
            info!("portal restore_token provided, attempting session restoration");
        }
        let create_handle: OwnedObjectPath = proxy.call("CreateSession", &(create_options)).await?;
        let create_response = wait_request(&connection, &create_handle).await?;
        if create_response.response != 0 {
            bail!("Portal permission denied while creating ScreenCast session");
        }
        let session_handle_string = create_response
            .results
            .get("session_handle")
            .and_then(owned_value_to_string)
            .context("ScreenCast portal did not return a session handle")?;
        let session_handle = OwnedObjectPath::try_from(session_handle_string.as_str())?;

        let new_restore_token = if restore_token.is_some() {
            info!("portal session restored from token; reusing previous source selection");
            None
        } else {
            let mut select_options = HashMap::<&str, OwnedValue>::new();
            select_options.insert("types", Value::from(1u32 | 2u32).try_into()?);
            select_options.insert("multiple", Value::from(false).try_into()?);
            select_options.insert("cursor_mode", Value::from(2u32).try_into()?);
            select_options.insert("persist_mode", Value::from(2u32).try_into()?);
            select_options.insert(
                "handle_token",
                Value::from(format!("waypad_select_{}", portal_token())).try_into()?,
            );
            let select_handle: OwnedObjectPath = proxy
                .call("SelectSources", &(&session_handle, select_options))
                .await?;
            let select_response = wait_request(&connection, &select_handle).await?;
            if select_response.response != 0 {
                bail!("ScreenCast source selection was denied by the portal");
            }
            None
        };

        let mut start_options = HashMap::<&str, OwnedValue>::new();
        start_options.insert(
            "handle_token",
            Value::from(format!("waypad_start_{}", portal_token())).try_into()?,
        );
        let start_handle: OwnedObjectPath = proxy
            .call("Start", &(&session_handle, "", start_options))
            .await?;
        let start_response = wait_request(&connection, &start_handle).await?;
        if start_response.response != 0 {
            bail!("ScreenCast portal approval was denied or cancelled");
        }
        let streams = start_response
            .results
            .get("streams")
            .and_then(owned_value_to_streams)
            .context("ScreenCast portal returned no streams")?;
        let (stream_id, properties) = streams
            .into_iter()
            .next()
            .context("ScreenCast portal returned an empty stream list")?;
        let (width, height) = stream_size(&properties);

        let saved_token = new_restore_token.or_else(|| {
            start_response
                .results
                .get("restore_token")
                .and_then(owned_value_to_string)
        });

        let open_options = HashMap::<&str, OwnedValue>::new();
        let pipewire_fd: OwnedFd = proxy
            .call("OpenPipeWireRemote", &(&session_handle, open_options))
            .await
            .context("PipeWire capture could not be initialized")?;

        if let Some(_token) = &saved_token {
            info!("portal restore_token saved for future sessions");
        }

        Ok(Self {
            stream_id,
            width,
            height,
            pipewire_fd,
            restore_token: saved_token,
            connection: Some(connection),
            session_handle: Some(session_handle),
        })
    }
}

impl Drop for PortalScreenCastSession {
    fn drop(&mut self) {
        if let (Some(connection), Some(handle)) = (self.connection.take(), self.session_handle.take()) {
            tokio::spawn(async move {
                let proxy = zbus::Proxy::new(
                    &connection,
                    "org.freedesktop.portal.Desktop",
                    "/org/freedesktop/portal/desktop",
                    "org.freedesktop.portal.ScreenCast",
                )
                .await;
                match proxy {
                    Ok(proxy) => {
                        let _: Result<(), _> = proxy
                            .call::<_, _, ()>("CloseSession", &(handle.as_str()))
                            .await;
                    }
                    Err(_) => {}
                }
            });
        }
    }
}

#[derive(Debug)]
struct PortalResponse {
    response: u32,
    results: HashMap<String, OwnedValue>,
}
fn spawn_gstreamer_pipewire(
    session: PortalScreenCastSession,
    fps: u32,
    quality: u8,
    target_width: Option<u32>,
    target_height: Option<u32>,
) -> anyhow::Result<tokio::process::Child> {
    let fd = session.pipewire_fd.as_raw_fd();
    let mut command = Command::new("gst-launch-1.0");
    command
        .arg("-q")
        .arg("pipewiresrc")
        .arg("fd=3")
        .arg(format!("path={}", session.stream_id))
        .arg("do-timestamp=true")
        .arg("keepalive-time=1000")
        .arg("!")
        // DMA-BUF → CPU: use videoconvert which supports DMA_DRM format
        .arg("videoconvert")
        .arg("!")
        .arg("videoscale")
        .arg("!")
        .arg("videorate")
        .arg("drop-only=true")
        .arg("skip-to-first=true")
        .arg("!")
        .arg(match (target_width, target_height) {
            (Some(width), Some(height)) => {
                format!("video/x-raw,width={width},height={height},framerate={fps}/1")
            }
            _ => format!("video/x-raw,framerate={fps}/1"),
        })
        .arg("!")
        .arg("jpegenc")
        .arg(format!("quality={}", quality))
        .arg("snapshot=false")
        .arg("!")
        .arg("fdsink")
        .arg("fd=1")
        .arg("sync=false")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    unsafe {
        command.pre_exec(move || {
            if libc_dup2(fd, 3) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    command
        .spawn()
        .context("failed to launch GStreamer PipeWire pipeline")
}

#[cfg(unix)]
fn libc_dup2(old_fd: i32, new_fd: i32) -> i32 {
    unsafe extern "C" {
        fn dup2(oldfd: i32, newfd: i32) -> i32;
    }
    unsafe { dup2(old_fd, new_fd) }
}

struct JpegStreamReader {
    buffer: Vec<u8>,
}

impl JpegStreamReader {
    fn new() -> Self {
        Self { buffer: Vec::new() }
    }

    fn push(&mut self, chunk: &[u8]) -> Vec<Vec<u8>> {
        self.buffer.extend_from_slice(chunk);
        let mut frames = Vec::new();
        loop {
            let Some(start) = find_marker(&self.buffer, [0xff, 0xd8], 0) else {
                self.buffer.clear();
                break;
            };
            if start > 0 {
                self.buffer.drain(..start);
            }
            let Some(end) = find_marker(&self.buffer, [0xff, 0xd9], 2) else {
                break;
            };
            let frame_end = end + 2;
            frames.push(self.buffer[..frame_end].to_vec());
            self.buffer.drain(..frame_end);
        }
        frames
    }
}

fn find_marker(buffer: &[u8], marker: [u8; 2], from: usize) -> Option<usize> {
    buffer
        .windows(2)
        .enumerate()
        .skip(from)
        .find_map(|(index, window)| (window == marker).then_some(index))
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
    let message = timeout(Duration::from_secs(60), stream.next())
        .await
        .context("Timed out waiting for portal response")?
        .context("Portal request closed before emitting Response")?;
    let (response, results): (u32, HashMap<String, OwnedValue>) = message.body().deserialize()?;
    Ok(PortalResponse { response, results })
}

fn portal_token() -> String {
    Uuid::new_v4().simple().to_string()
}

fn owned_value_to_string(value: &OwnedValue) -> Option<String> {
    <&str>::try_from(value).map(ToOwned::to_owned).ok()
}

fn owned_value_to_streams(value: &OwnedValue) -> Option<Vec<(u32, HashMap<String, OwnedValue>)>> {
    value.try_clone().ok()?.try_into().ok()
}

fn stream_size(properties: &HashMap<String, OwnedValue>) -> (Option<u32>, Option<u32>) {
    let Some(value) = properties.get("size") else {
        return (None, None);
    };
    if let Ok((width, height)) = value.try_clone().and_then(TryInto::try_into) {
        return (Some(width), Some(height));
    }
    (None, None)
}

// ============================================================
// X11 capture backend (ffmpeg x11grab — no portal needed)
// ============================================================

async fn list_x11_monitors() -> anyhow::Result<Vec<ScreenSource>> {
    let display = std::env::var("DISPLAY").unwrap_or_else(|_| ":1".into());
    let output = tokio::process::Command::new("xrandr")
        .arg("--display")
        .arg(&display)
        .output()
        .await
        .context("xrandr not available")?;
    if !output.status.success() {
        anyhow::bail!("xrandr failed");
    }
    let raw = String::from_utf8_lossy(&output.stdout);
    let mut monitors = Vec::new();
    for line in raw.lines() {
        if !line.contains(" connected") {
            continue;
        }
        // Format: "HDMI-A-1 connected 1920x1080+1920+0 ..."
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 3 {
            continue;
        }
        let name = parts[0].to_string();
        let geom_str = parts[2];
        if !geom_str.contains('x') || !geom_str.contains('+') {
            continue;
        }
        // Parse "WIDTHxHEIGHT+X+Y"
        let main_parts: Vec<&str> = geom_str.split('+').collect();
        if main_parts.len() < 3 {
            continue;
        }
        let res_str = main_parts[0];
        let res_parts: Vec<&str> = res_str.split('x').collect();
        if res_parts.len() != 2 {
            continue;
        }
        let Ok(w) = res_parts[0].parse::<u32>() else { continue };
        let Ok(h) = res_parts[1].parse::<u32>() else { continue };
        let Ok(x) = main_parts[1].parse::<i32>() else { continue };
        let Ok(y) = main_parts[2].parse::<i32>() else { continue };

        monitors.push(ScreenSource {
            id: format!("x11:{}", name),
            label: format!("{} (X11 – 60 FPS, no approval)", name),
            kind: "monitor".into(),
            backend: "x11-ffmpeg".into(),
            width: w,
            height: h,
            x,
            y,
            scale: 1.0,
            focused: monitors.is_empty(),
        });
    }
    if monitors.is_empty() {
        anyhow::bail!("no connected monitors found via xrandr");
    }
    Ok(monitors)
}

async fn run_x11_stream(
    socket: &mut TcpStream,
    session_id: String,
    source: ScreenSource,
    fps: u32,
    quality: u8,
    max_width: Option<u32>,
    max_height: Option<u32>,
    stop_rx: &mut oneshot::Receiver<()>,
) -> anyhow::Result<()> {
    let display = std::env::var("DISPLAY").unwrap_or_else(|_| ":1".into());
    let (target_w, target_h) =
        target_dimensions(source.width, source.height, max_width, max_height);
    let cap_w = target_w.unwrap_or(source.width);
    let cap_h = target_h.unwrap_or(source.height);

    info!(%session_id, source_id = %source.id, fps, quality, cap_w, cap_h, "x11 stream starting with ffmpeg");

    // ffmpeg -f x11grab -framerate 60 -video_size 1920x1080 -i :1.0+1920,0
    //   -vf "scale=W:H" -c:v mjpeg -q:v Q -f mjpeg pipe:1
    let input_spec = format!("{}.0+{},{}", display, source.x, source.y);
    let mut cmd = tokio::process::Command::new("ffmpeg");
    cmd.args([
        "-y", "-hide_banner", "-loglevel", "error",
        "-f", "x11grab",
        "-framerate", &fps.to_string(),
        "-video_size", &format!("{cap_w}x{cap_h}"),
        "-i", &input_spec,
    ]);
    // Scale if needed
    if target_w.is_some() || target_h.is_some() {
        cmd.args(["-vf", &format!("scale={}:{}", cap_w, cap_h)]);
    }
    cmd.args([
        "-c:v", "mjpeg",
        "-q:v", &quality.to_string(),
        "-f", "mjpeg",
        "pipe:1",
    ])
    .stdout(std::process::Stdio::piped())
    .stderr(std::process::Stdio::piped());

    let mut child = cmd.spawn().context("failed to spawn ffmpeg for X11 capture")?;
    if let Some(stderr) = child.stderr.take() {
        tokio::spawn(log_child_stderr(session_id.clone(), "ffmpeg", stderr));
    }
    let mut stdout = child.stdout.take().context("ffmpeg stdout unavailable")?;
    let mut reader = JpegStreamReader::new();
    let mut buffer = [0u8; 32 * 1024];
    let mut seq = 0u64;
    let mut frame_count = 0u64;
    let mut throughput_start = tokio::time::Instant::now();

    loop {
        tokio::select! {
            _ = &mut *stop_rx => break,
            read = stdout.read(&mut buffer) => {
                let n = read?;
                if n == 0 {
                    warn!(%session_id, "ffmpeg x11 producer closed stdout");
                    break;
                }
                for frame in reader.push(&buffer[..n]) {
                    send_frame(&mut *socket, seq, source.width, source.height, &frame).await?;
                    seq += 1;
                    frame_count += 1;
                }
                let elapsed = throughput_start.elapsed().as_secs_f64();
                if elapsed >= 2.0 {
                    let measured = frame_count as f64 / elapsed;
                    info!(%session_id, fps_measured = measured, fps_target = fps, frames = frame_count, "x11 stream throughput");
                    frame_count = 0;
                    throughput_start = tokio::time::Instant::now();
                }
            }
        }
    }
    let _ = child.kill().await;
    if seq == 0 {
        anyhow::bail!("ffmpeg x11 produced no frames");
    }
    info!(%session_id, total_frames = seq, "x11 stream stopped");
    Ok(())
}

pub async fn authorize_portal() -> anyhow::Result<String> {
    let connection = zbus::Connection::session().await?;
    let proxy = zbus::Proxy::new(
        &connection,
        "org.freedesktop.portal.Desktop",
        "/org/freedesktop/portal/desktop",
        "org.freedesktop.portal.ScreenCast",
    )
    .await
    .context("ScreenCast portal not available")?;

    let mut create_options = HashMap::<&str, OwnedValue>::new();
    create_options.insert(
        "handle_token",
        Value::from(format!("waypad_auth_create_{}", portal_token())).try_into()?,
    );
    create_options.insert(
        "session_handle_token",
        Value::from(format!("waypad_auth_session_{}", portal_token())).try_into()?,
    );
    
    let create_handle: OwnedObjectPath = proxy.call("CreateSession", &(create_options)).await?;
    let create_response = wait_request(&connection, &create_handle).await?;
    if create_response.response != 0 {
        bail!("Portal permission denied while creating ScreenCast authorization");
    }
    let session_handle_string = create_response
        .results
        .get("session_handle")
        .and_then(owned_value_to_string)
        .context("ScreenCast portal did not return a session handle")?;
    let session_handle = OwnedObjectPath::try_from(session_handle_string.as_str())?;

    let mut select_options = HashMap::<&str, OwnedValue>::new();
    select_options.insert("types", Value::from(1u32 | 2u32).try_into()?);
    select_options.insert("multiple", Value::from(false).try_into()?);
    select_options.insert("cursor_mode", Value::from(2u32).try_into()?);
    select_options.insert("persist_mode", Value::from(2u32).try_into()?);
    select_options.insert(
        "handle_token",
        Value::from(format!("waypad_auth_select_{}", portal_token())).try_into()?,
    );
    let select_handle: OwnedObjectPath = proxy
        .call("SelectSources", &(&session_handle, select_options))
        .await?;
    let select_response = wait_request(&connection, &select_handle).await?;
    if select_response.response != 0 {
        bail!("ScreenCast source selection was denied");
    }

    let mut start_options = HashMap::<&str, OwnedValue>::new();
    start_options.insert(
        "handle_token",
        Value::from(format!("waypad_auth_start_{}", portal_token())).try_into()?,
    );
    let start_handle: OwnedObjectPath = proxy
        .call("Start", &(&session_handle, "", start_options))
        .await?;
    let start_response = wait_request(&connection, &start_handle).await?;
    if start_response.response != 0 {
        bail!("ScreenCast authorization was denied or cancelled. Approve the dialog on your desktop.");
    }

    let restore_token = start_response
        .results
        .get("restore_token")
        .and_then(owned_value_to_string);

    match restore_token {
        Some(token) => {
            let _: Result<(), _> = proxy
                .call::<_, _, ()>("CloseSession", &(session_handle.as_str()))
                .await;
            Ok(token)
        }
        None => {
            warn!("portal authorization succeeded but no restore_token returned; persist_mode may not be supported by this backend");
            let _: Result<(), _> = proxy
                .call::<_, _, ()>("CloseSession", &(session_handle.as_str()))
                .await;
            anyhow::bail!("Portal authorization completed but restore_token not available. The portal should now be approved for this session. Try streaming immediately.")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        JpegStreamReader, capture_scale, find_marker, is_client_disconnect, target_dimensions,
    };

    #[test]
    fn parses_concatenated_jpeg_frames() {
        let mut reader = JpegStreamReader::new();
        let frames = reader.push(&[0xff, 0xd8, 1, 2, 0xff, 0xd9, 0xff, 0xd8]);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0], vec![0xff, 0xd8, 1, 2, 0xff, 0xd9]);
        let frames = reader.push(&[3, 4, 0xff, 0xd9]);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0], vec![0xff, 0xd8, 3, 4, 0xff, 0xd9]);
    }

    #[test]
    fn finds_markers_after_offset() {
        assert_eq!(find_marker(&[0, 0xff, 0xd8], [0xff, 0xd8], 0), Some(1));
        assert_eq!(find_marker(&[0xff, 0xd8], [0xff, 0xd8], 1), None);
    }

    #[test]
    fn classifies_client_disconnect_io_errors() {
        let broken_pipe = anyhow::Error::new(std::io::Error::from(std::io::ErrorKind::BrokenPipe));
        assert!(is_client_disconnect(&broken_pipe));

        let permission =
            anyhow::Error::new(std::io::Error::from(std::io::ErrorKind::PermissionDenied));
        assert!(!is_client_disconnect(&permission));
    }

    #[test]
    fn computes_stream_downscale_dimensions() {
        assert_eq!(capture_scale(3840, 2160, Some(1920), Some(1080)), 0.5);
        assert_eq!(
            target_dimensions(3840, 2160, Some(1280), Some(1280)),
            (Some(1280), Some(720)),
        );
        assert_eq!(
            target_dimensions(1280, 720, Some(2400), Some(2400)),
            (None, None)
        );
    }
}
