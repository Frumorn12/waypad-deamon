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
    time::{interval, timeout},
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
}

#[derive(Debug)]
struct RunningStream {
    stop: oneshot::Sender<()>,
    task: JoinHandle<()>,
}

impl ScreenManager {
    pub fn new(capabilities: Arc<RwLock<Capabilities>>, stream_port: u16) -> Self {
        Self {
            capabilities,
            stream_port,
            sessions: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub async fn list_sources(&self) -> anyhow::Result<Vec<ScreenSource>> {
        let capabilities = self.capabilities.read().await.clone();
        let mut sources = Vec::new();
        if capabilities.capture.portal_screencast_available
            && capabilities.capture.pipewire_runtime_available
            && capabilities.capture.gstreamer_pipewire_available
        {
            sources.push(ScreenSource {
                id: "portal:chooser".into(),
                label: "Portal picker".into(),
                kind: "chooser".into(),
                backend: "wayland-screencast-portal".into(),
                width: 0,
                height: 0,
                x: 0,
                y: 0,
                scale: 1.0,
                focused: false,
            });
        }
        if capabilities.capture.hyprland_grim_available {
            sources.extend(hyprland_monitor_sources().await.unwrap_or_else(|err| {
                warn!(%err, "failed to enumerate Hyprland monitors");
                Vec::new()
            }));
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
        let fps = options.max_fps.unwrap_or(10).clamp(1, 30);
        let quality = options.jpeg_quality.unwrap_or(70).clamp(35, 90);
        let session_id = Uuid::new_v4().to_string();
        let token = Uuid::new_v4().to_string();
        self.sessions.lock().await.insert(
            session_id.clone(),
            StreamSession::Pending(PendingStream {
                token: token.clone(),
                source: source.clone(),
                fps,
                quality,
            }),
        );
        info!(
            %session_id,
            stream_port = self.stream_port,
            source_id = %source.id,
            backend = %source.backend,
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
        let (stop_tx, stop_rx) = oneshot::channel();
        let task_sessions = self.sessions.clone();
        let task_session = session_id.clone();
        let source = pending.source.clone();
        let task = tokio::spawn(async move {
            let result = if source.backend == "wayland-screencast-portal" {
                run_portal_stream(
                    socket,
                    task_session.clone(),
                    source,
                    pending.fps,
                    pending.quality,
                    stop_rx,
                )
                .await
            } else {
                run_grim_stream(
                    socket,
                    task_session.clone(),
                    source,
                    pending.fps,
                    pending.quality,
                    stop_rx,
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

async fn run_grim_stream(
    mut socket: TcpStream,
    session_id: String,
    source: ScreenSource,
    fps: u32,
    quality: u8,
    mut stop_rx: oneshot::Receiver<()>,
) -> anyhow::Result<()> {
    let mut ticker = interval(Duration::from_millis((1000 / fps.max(1)) as u64));
    let mut seq = 0u64;
    info!(%session_id, source_id = %source.id, fps, quality, "grim stream started");
    loop {
        tokio::select! {
            _ = &mut stop_rx => break,
            _ = ticker.tick() => {
                let jpeg = capture_grim_frame(&source, quality).await?;
                send_frame(&mut socket, seq, source.width, source.height, &jpeg).await?;
                seq += 1;
            }
        }
    }
    debug!(%session_id, "grim stream stopped");
    Ok(())
}

async fn run_portal_stream(
    mut socket: TcpStream,
    session_id: String,
    _selected_source: ScreenSource,
    fps: u32,
    quality: u8,
    mut stop_rx: oneshot::Receiver<()>,
) -> anyhow::Result<()> {
    info!(%session_id, fps, quality, "portal stream client connected; starting ScreenCast approval");
    let portal = PortalScreenCastSession::start().await?;
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
    let mut child = spawn_gstreamer_pipewire(portal, fps, quality)?;
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
    info!(%session_id, source_id = %source.id, "portal stream started");
    loop {
        tokio::select! {
            _ = &mut stop_rx => break,
            read = stdout.read(&mut buffer) => {
                let n = read?;
                if n == 0 {
                    warn!(%session_id, "portal stream producer closed stdout");
                    break;
                }
                for frame in reader.push(&buffer[..n]) {
                    send_frame(&mut socket, seq, source.width, source.height, &frame).await?;
                    seq += 1;
                }
            }
        }
    }
    let _ = child.kill().await;
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

async fn send_frame(
    socket: &mut TcpStream,
    seq: u64,
    width: u32,
    height: u32,
    jpeg: &[u8],
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
    socket
        .write_all(&(header.len() as u32).to_be_bytes())
        .await?;
    socket.write_all(&(jpeg.len() as u32).to_be_bytes()).await?;
    socket.write_all(header).await?;
    socket.write_all(jpeg).await?;
    Ok(())
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

async fn capture_grim_frame(source: &ScreenSource, quality: u8) -> anyhow::Result<Vec<u8>> {
    let mut command = Command::new("grim");
    command.args(["-t", "jpeg", "-q", &quality.to_string(), "-c"]);
    if let Some(output) = source.id.strip_prefix("hyprland:monitor:") {
        command.args(["-o", output]);
    }
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
}

impl PortalScreenCastSession {
    async fn start() -> anyhow::Result<Self> {
        let connection = zbus::Connection::session().await?;
        let proxy = zbus::Proxy::new(
            &connection,
            "org.freedesktop.portal.Desktop",
            "/org/freedesktop/portal/desktop",
            "org.freedesktop.portal.ScreenCast",
        )
        .await
        .context("ScreenCast portal not available")?;

        let session_token = format!("waypad_screen_{}", portal_token());
        let mut create_options = HashMap::<&str, OwnedValue>::new();
        create_options.insert(
            "handle_token",
            Value::from(format!("waypad_create_{}", portal_token())).try_into()?,
        );
        create_options.insert(
            "session_handle_token",
            Value::from(session_token).try_into()?,
        );
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

        let mut select_options = HashMap::<&str, OwnedValue>::new();
        select_options.insert("types", Value::from(1u32 | 2u32).try_into()?);
        select_options.insert("multiple", Value::from(false).try_into()?);
        select_options.insert("cursor_mode", Value::from(2u32).try_into()?);
        select_options.insert("persist_mode", Value::from(1u32).try_into()?);
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

        let open_options = HashMap::<&str, OwnedValue>::new();
        let pipewire_fd: OwnedFd = proxy
            .call("OpenPipeWireRemote", &(&session_handle, open_options))
            .await
            .context("PipeWire capture could not be initialized")?;
        Ok(Self {
            stream_id,
            width,
            height,
            pipewire_fd,
        })
    }
}

fn spawn_gstreamer_pipewire(
    session: PortalScreenCastSession,
    fps: u32,
    quality: u8,
) -> anyhow::Result<tokio::process::Child> {
    let fd = session.pipewire_fd.as_raw_fd();
    let mut command = Command::new("gst-launch-1.0");
    command
        .arg("-q")
        .arg("pipewiresrc")
        .arg("fd=3")
        .arg(format!("path={}", session.stream_id))
        .arg("do-timestamp=true")
        .arg("!")
        .arg("videorate")
        .arg("!")
        .arg(format!("video/x-raw,framerate={}/1", fps))
        .arg("!")
        .arg("videoconvert")
        .arg("!")
        .arg("jpegenc")
        .arg(format!("quality={}", quality))
        .arg("!")
        .arg("fdsink")
        .arg("fd=1")
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

#[cfg(test)]
mod tests {
    use super::{JpegStreamReader, find_marker, is_client_disconnect};

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
}
