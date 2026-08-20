use crate::{
    capability::Capabilities,
    input::InputManager,
    platform::{command_exists, command_output},
};
use anyhow::{Context, bail};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    collections::HashMap,
    os::fd::AsRawFd,
    sync::{Arc, OnceLock},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    process::{ChildStderr, ChildStdout, Command},
    sync::{Mutex, RwLock, mpsc, oneshot},
    task::JoinHandle,
    time::timeout,
};
use tracing::{debug, error, info, warn};
use uuid::Uuid;
use zbus::zvariant::{OwnedFd, OwnedObjectPath, OwnedValue, Value};

const STREAM_MAGIC_V1: &[u8] = b"WAYPAD_STREAM_V1\n";
const STREAM_MAGIC_V2: &[u8] = b"WAYPAD_STREAM_V2\n";

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
    pub bitrate_kbps: Option<u32>,
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
    pub actual_fps: u32,
    pub actual_quality: u8,
    pub actual_bitrate_kbps: u32,
    /// Why the fast PipeWire pipeline last fell back to the slow `grim`
    /// screenshot backend. Surfaced so a broken fast path cannot sit unnoticed
    /// behind a fallback that merely looks slow.
    pub portal_last_error: Option<String>,
}

/// Encoder knobs the Android client negotiates per session.
#[derive(Clone, Copy, Debug)]
struct StreamTuning {
    fps: u32,
    quality: u8,
    bitrate_kbps: Option<u32>,
    max_width: Option<u32>,
    max_height: Option<u32>,
}

#[derive(Debug)]
pub struct ScreenManager {
    capabilities: Arc<RwLock<Capabilities>>,
    stream_port: u16,
    sessions: Arc<Mutex<HashMap<String, StreamSession>>>,
    paths: Arc<super::state::StatePaths>,
    last_portal_error: Arc<Mutex<Option<String>>>,
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
    tuning: StreamTuning,
}

#[derive(Debug)]
struct RunningStream {
    stop: oneshot::Sender<()>,
    keyframe: mpsc::Sender<()>,
    task: JoinHandle<()>,
}

impl ScreenManager {
    pub fn new(
        capabilities: Arc<RwLock<Capabilities>>,
        stream_port: u16,
        paths: super::state::StatePaths,
    ) -> Self {
        Self {
            capabilities,
            stream_port,
            sessions: Arc::new(Mutex::new(HashMap::new())),
            paths: Arc::new(paths),
            last_portal_error: Arc::new(Mutex::new(None)),
        }
    }

    pub async fn list_sources(&self) -> anyhow::Result<Vec<ScreenSource>> {
        let capabilities = self.capabilities.read().await.clone();
        let portal_available = capabilities.capture.portal_screencast_available
            && capabilities.capture.pipewire_runtime_available
            && capabilities.capture.gstreamer_pipewire_available;
        let _has_restore_token = crate::state::load_portal_restore_token(&self.paths).is_some();

        let mut sources = Vec::new();

        if portal_available {
            sources.push(ScreenSource {
                id: "portal:chooser".into(),
                label: "Portal picker (PipeWire screencast — 30–60 FPS)".into(),
                kind: "chooser".into(),
                backend: "wayland-screencast-portal".into(),
                width: 0,
                height: 0,
                x: 0,
                y: 0,
                scale: 1.0,
                focused: true,
            });
        }
        if capabilities.capture.hyprland_grim_available {
            let monitors = hyprland_monitor_sources().await.unwrap_or_else(|err| {
                warn!(%err, "failed to enumerate Hyprland monitors");
                Vec::new()
            });
            for (i, mut monitor) in monitors.into_iter().enumerate() {
                monitor.focused = i == 0 && sources.iter().all(|s| !s.focused);
                sources.push(monitor);
            }
        }
        // X11 ffmpeg backend — high performance, no portal approval needed
        if std::env::var("DISPLAY").is_ok() && command_exists("xrandr") && command_exists("ffmpeg")
        {
            match list_x11_monitors().await {
                Ok(monitors) => {
                    for (i, mut monitor) in monitors.into_iter().enumerate() {
                        monitor.focused = i == 0 && sources.iter().all(|s| !s.focused);
                        sources.push(monitor);
                    }
                }
                Err(err) => {
                    warn!(%err, "failed to enumerate X11 monitors");
                }
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

        let _is_grim = source.backend == "hyprland-grim";
        let tuning = StreamTuning {
            fps: options.max_fps.unwrap_or(30).clamp(1, 60),
            quality: options.jpeg_quality.unwrap_or(70).clamp(35, 92),
            bitrate_kbps: options.bitrate_kbps,
            max_width: options.max_width.map(|value| value.clamp(480, 3840)),
            max_height: options.max_height.map(|value| value.clamp(480, 3840)),
        };
        let session_id = Uuid::new_v4().to_string();
        let token = Uuid::new_v4().to_string();
        // Hardware H.264 only rides on the PipeWire pipeline; the grim and X11
        // fallbacks stay on JPEG, so the announced codec follows the backend.
        let h264 = source.backend == "wayland-screencast-portal" && detect_h264_encoder().is_some();
        let (target_width, target_height) = target_dimensions(
            source.width,
            source.height,
            tuning.max_width,
            tuning.max_height,
        );
        let bitrate_kbps = resolve_bitrate_kbps(
            tuning.bitrate_kbps,
            target_width.unwrap_or(source.width),
            target_height.unwrap_or(source.height),
            tuning.fps,
            tuning.quality,
        );
        // Save the selected source for future sessions
        if let Err(err) = crate::state::save_preferred_source(&self.paths, &source.id) {
            warn!(%err, source_id = %source.id, "failed to save preferred source");
        }
        self.sessions.lock().await.insert(
            session_id.clone(),
            StreamSession::Pending(PendingStream {
                token: token.clone(),
                source: source.clone(),
                tuning,
            }),
        );
        info!(
            %session_id,
            stream_port = self.stream_port,
            source_id = %source.id,
            backend = %source.backend,
            requested_fps = options.max_fps,
            actual_fps = tuning.fps,
            actual_quality = tuning.quality,
            actual_bitrate_kbps = bitrate_kbps,
            codec = if h264 { "h264" } else { "jpeg" },
            max_width = ?tuning.max_width,
            max_height = ?tuning.max_height,
            "screen stream session pending client attach"
        );
        Ok(StreamStartResponse {
            session_id,
            stream_port: self.stream_port,
            token,
            codec: if h264 { "h264".into() } else { "jpeg".into() },
            transport: "waypad-control-port-stream-v2".into(),
            source,
            actual_fps: tuning.fps,
            actual_quality: tuning.quality,
            actual_bitrate_kbps: bitrate_kbps,
            portal_last_error: self.last_portal_error.lock().await.clone(),
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
        info!(
            %session_id,
            ?peer,
            source_id = %pending.source.id,
            backend = %pending.source.backend,
            "screen stream client attached"
        );
        let (stop_tx, mut stop_rx) = oneshot::channel();
        // Depth 1: repeated keyframe requests before the encoder restarts are
        // the same request, so they coalesce instead of queueing restarts.
        let (keyframe_tx, mut keyframe_rx) = mpsc::channel(1);
        let task_sessions = self.sessions.clone();
        let task_session = session_id.clone();
        let source = pending.source.clone();
        let task_paths = self.paths.clone();
        let task_last_error = self.last_portal_error.clone();
        let task = tokio::spawn(async move {
            // The handshake line names the codec, and the codec is only known
            // once a producer actually emits a frame, so it is written lazily.
            // That also keeps the grim fallback usable: it may only take over
            // while the socket is still untouched.
            let mut magic_sent = false;
            let result = if source.backend == "wayland-screencast-portal" {
                let portal_result = run_portal_stream(
                    &mut socket,
                    &mut magic_sent,
                    task_session.clone(),
                    source.clone(),
                    pending.tuning,
                    &mut stop_rx,
                    &mut keyframe_rx,
                    task_paths.as_ref().clone(),
                )
                .await;
                match portal_result {
                    Ok(()) => {
                        *task_last_error.lock().await = None;
                        Ok(())
                    }
                    Err(portal_err) => {
                        let detail = format!("{portal_err:#}");
                        *task_last_error.lock().await = Some(detail.clone());
                        if is_client_disconnect(&portal_err) || magic_sent {
                            Err(portal_err)
                        } else {
                            // Loud on purpose: the grim fallback still produces
                            // a picture, so a permanently broken fast path is
                            // invisible unless the reason is reported.
                            error!(
                                session_id = %task_session,
                                error = %detail,
                                "portal screen pipeline failed; falling back to the grim screenshot backend, which caps out near 6 FPS"
                            );
                            // Use grim with the same connection
                            run_grim_stream_on_open(
                                &mut socket,
                                &mut magic_sent,
                                task_session.clone(),
                                source,
                                pending.tuning,
                                &mut stop_rx,
                            )
                            .await
                        }
                    }
                }
            } else if source.backend == "x11-ffmpeg" {
                run_x11_stream(
                    &mut socket,
                    &mut magic_sent,
                    task_session.clone(),
                    source,
                    pending.tuning,
                    &mut stop_rx,
                )
                .await
            } else {
                run_grim_stream_on_open(
                    &mut socket,
                    &mut magic_sent,
                    task_session.clone(),
                    source,
                    pending.tuning,
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
                keyframe: keyframe_tx,
                task,
            }),
        );
        Ok(())
    }

    /// Forces an immediate IDR on a running session. Android recreates its
    /// decoder whenever the app returns to the foreground and the `SurfaceView`
    /// is rebuilt, and it stays black until it sees SPS/PPS plus an IDR.
    pub async fn request_key_frame(&self, session_id: &str) -> anyhow::Result<()> {
        let sessions = self.sessions.lock().await;
        match sessions.get(session_id) {
            Some(StreamSession::Running(running)) => {
                match running.keyframe.try_send(()) {
                    Ok(()) => debug!(%session_id, "keyframe requested for screen stream"),
                    // Full means a restart is already pending; the client gets
                    // its keyframe from that one.
                    Err(mpsc::error::TrySendError::Full(_)) => {
                        debug!(%session_id, "keyframe request coalesced into a pending one")
                    }
                    Err(mpsc::error::TrySendError::Closed(_)) => {
                        debug!(%session_id, "keyframe request ignored; stream is shutting down")
                    }
                }
                Ok(())
            }
            // The producer has not started yet, and it always opens on an IDR.
            Some(StreamSession::Pending(_)) => Ok(()),
            None => bail!("Unknown or expired screen stream session"),
        }
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
            if let Some(ref pref_id) = preferred
                && let Some(source) = sources.iter().find(|s| s.id == *pref_id)
            {
                info!(source_id = %pref_id, "restored preferred screen source");
                return Ok(source.clone());
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
    magic_sent: &mut bool,
    session_id: String,
    source: ScreenSource,
    tuning: StreamTuning,
    stop_rx: &mut oneshot::Receiver<()>,
) -> anyhow::Result<()> {
    run_grim_stream_impl(socket, magic_sent, session_id, source, tuning, stop_rx).await
}

async fn run_grim_stream_impl(
    socket: &mut TcpStream,
    magic_sent: &mut bool,
    session_id: String,
    source: ScreenSource,
    tuning: StreamTuning,
    stop_rx: &mut oneshot::Receiver<()>,
) -> anyhow::Result<()> {
    let StreamTuning {
        fps,
        quality,
        max_width,
        max_height,
        ..
    } = tuning;
    let target_interval = Duration::from_secs_f64(1.0 / f64::from(fps.max(1)));
    let mut seq = 0u64;
    // Force aggressive scale for grim (screenshot tool is slow at full res)
    let requested_scale = capture_scale(source.width, source.height, max_width, max_height);
    let scale = requested_scale.min(0.4); // Never capture above 40% resolution for grim
    info!(%session_id, source_id = %source.id, fps, quality, scale, requested_scale, "grim stream started");
    let mut frame_count = 0u64;
    let mut throughput_start = tokio::time::Instant::now();
    loop {
        let frame_start = tokio::time::Instant::now();
        tokio::select! {
            _ = &mut *stop_rx => break,
            jpeg = capture_grim_frame(&source, quality, scale) => {
                let jpeg = jpeg?;
                send_stream_magic(&mut *socket, magic_sent, STREAM_MAGIC_V1).await?;
                send_frame_grim(&mut *socket, seq, source.width, source.height, &jpeg).await?;
                seq += 1;
                frame_count += 1;
                let elapsed = throughput_start.elapsed().as_secs_f64();
                if elapsed >= 2.0 {
                    let measured = frame_count as f64 / elapsed;
                    info!(%session_id, fps_measured = measured, fps_target = fps, frames = frame_count, "grim stream throughput");
                    frame_count = 0;
                    throughput_start = tokio::time::Instant::now();
                }
                // Sleep remaining time to hit target fps
                let capture_elapsed = frame_start.elapsed();
                if let Some(remaining) = target_interval.checked_sub(capture_elapsed) {
                    tokio::select! {
                        _ = &mut *stop_rx => break,
                        _ = tokio::time::sleep(remaining) => {}
                    }
                }
            }
        }
    }
    debug!(%session_id, "grim stream stopped");
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn run_portal_stream(
    socket: &mut TcpStream,
    magic_sent: &mut bool,
    session_id: String,
    _selected_source: ScreenSource,
    tuning: StreamTuning,
    stop_rx: &mut oneshot::Receiver<()>,
    keyframe_rx: &mut mpsc::Receiver<()>,
    paths: super::state::StatePaths,
) -> anyhow::Result<()> {
    let StreamTuning {
        fps,
        quality,
        max_width,
        max_height,
        ..
    } = tuning;
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
    if let Some(ref token) = portal.restore_token
        && let Err(err) = crate::state::save_portal_restore_token(&paths, token)
    {
        warn!(%session_id, %err, "failed to save portal restore token");
    }
    let source = ScreenSource {
        id: format!("portal:stream:{}", portal.stream_id),
        label: "Portal-selected source".into(),
        kind: "portal-stream".into(),
        backend: "wayland-screencast-portal".into(),
        width: portal.width.unwrap_or(0),
        height: portal.height.unwrap_or(0),
        x: portal.position.0,
        y: portal.position.1,
        scale: 1.0,
        focused: true,
    };
    let (mut target_width, mut target_height) =
        target_dimensions(source.width, source.height, max_width, max_height);
    let encoding = match detect_h264_encoder() {
        Some(encoder) => {
            // H.264 macroblocks are 16x16 and chroma is subsampled, so both
            // dimensions have to stay even or the encoder refuses to negotiate.
            target_width = target_width.map(even_dimension);
            target_height = target_height.map(even_dimension);
            PipelineEncoding::H264 {
                encoder,
                bitrate_kbps: resolve_bitrate_kbps(
                    tuning.bitrate_kbps,
                    target_width.unwrap_or(source.width),
                    target_height.unwrap_or(source.height),
                    fps,
                    quality,
                ),
                gop_size: keyframe_interval(fps),
            }
        }
        None => PipelineEncoding::Jpeg { quality },
    };
    let width = target_width.unwrap_or(source.width);
    let height = target_height.unwrap_or(source.height);
    info!(
        %session_id,
        source_id = %source.id,
        encoder = encoding.label(),
        codec = encoding.codec(),
        bitrate_kbps = encoding.bitrate_kbps(),
        width,
        height,
        "portal stream started"
    );
    let producer_error: ProducerError = Arc::new(std::sync::Mutex::new(None));
    let mut counters = StreamCounters::default();
    // Restart loop. Shelling out to gst-launch leaves no way to push a
    // force-key-unit event into a running pipeline, so an on-demand keyframe is
    // served by respawning the encoder: a fresh pipeline always opens on an IDR
    // with its parameter sets.
    let pumped = loop {
        let mut child =
            spawn_gstreamer_pipewire(&portal, fps, encoding, target_width, target_height)?;
        let stderr_task = child.stderr.take().map(|stderr| {
            tokio::spawn(log_child_stderr(
                session_id.clone(),
                "gstreamer",
                stderr,
                producer_error.clone(),
            ))
        });
        let mut stdout = child
            .stdout
            .take()
            .context("GStreamer stdout unavailable")?;
        let pumped = match encoding {
            PipelineEncoding::H264 { .. } => {
                pump_h264_stream(
                    socket,
                    magic_sent,
                    &mut stdout,
                    stop_rx,
                    keyframe_rx,
                    &mut counters,
                    &session_id,
                    FrameGeometry {
                        width,
                        height,
                        source_width: source.width,
                        source_height: source.height,
                    },
                    fps,
                )
                .await
            }
            PipelineEncoding::Jpeg { .. } => {
                // Every JPEG frame is already a keyframe, so this path ignores
                // the request channel and never restarts.
                pump_jpeg_stream(
                    socket,
                    magic_sent,
                    &mut stdout,
                    stop_rx,
                    &mut counters,
                    &session_id,
                    source.width,
                    source.height,
                    fps,
                    "portal",
                )
                .await
                .map(|()| PumpOutcome::Finished)
            }
        };
        let _ = child.kill().await;
        // Killing the child closes the stderr pipe, so draining it now costs
        // almost nothing and makes the producer's own diagnostic available.
        if let Some(task) = stderr_task {
            let _ = timeout(Duration::from_millis(500), task).await;
        }
        match pumped {
            Ok(PumpOutcome::KeyFrameRequested) => {
                info!(%session_id, "restarting encoder pipeline to serve a keyframe request");
                continue;
            }
            other => break other,
        }
    };
    pumped?;
    let frames = counters.frames;
    if frames == 0 {
        // GStreamer pipeline failed before producing any frames. Return an error
        // so the grim fallback can take over, carrying the real reason instead
        // of a guess.
        let detail = producer_error.lock().ok().and_then(|slot| slot.clone());
        match detail {
            Some(detail) => {
                anyhow::bail!("Portal GStreamer pipeline produced no frames: {detail}")
            }
            None => anyhow::bail!(
                "Portal GStreamer pipeline produced no frames (PipeWire format may be incompatible)"
            ),
        }
    }
    debug!(%session_id, frames, "portal stream stopped");
    Ok(())
}

/// Envelope counters that survive an encoder restart, so `seq` stays monotonic
/// across a keyframe-driven respawn.
#[derive(Debug, Default)]
struct StreamCounters {
    seq: u64,
    frames: u64,
}

#[derive(Debug, PartialEq, Eq)]
enum PumpOutcome {
    Finished,
    KeyFrameRequested,
}

/// Encoder restarts are cheap but not free, so a client that asks repeatedly
/// cannot make the pipeline thrash.
const KEYFRAME_RESTART_MIN_INTERVAL: Duration = Duration::from_secs(1);

/// Reads Annex-B access units off the encoder pipe and forwards them as
/// `WAYPAD_STREAM_V2` frames.
#[allow(clippy::too_many_arguments)]
async fn pump_h264_stream(
    socket: &mut TcpStream,
    magic_sent: &mut bool,
    stdout: &mut ChildStdout,
    stop_rx: &mut oneshot::Receiver<()>,
    keyframe_rx: &mut mpsc::Receiver<()>,
    counters: &mut StreamCounters,
    session_id: &str,
    geometry: FrameGeometry,
    fps: u32,
) -> anyhow::Result<PumpOutcome> {
    let mut reader = AnnexBStreamReader::new();
    let mut buffer = vec![0u8; 64 * 1024];
    let mut frame_count = 0u64;
    let mut throughput_start = tokio::time::Instant::now();
    let started = tokio::time::Instant::now();
    // A decoder that joins on a P-frame shows nothing until the next IDR, so
    // nothing is forwarded before the first keyframe of the fresh pipeline.
    let mut waiting_for_keyframe = true;
    let mut keyframe_channel_open = true;
    // Half a frame interval, bounded so the pipe is unambiguously idle before
    // the buffered picture is released early.
    let flush_idle = Duration::from_millis((500 / u64::from(fps.max(1))).clamp(4, 12));
    loop {
        let units = tokio::select! {
            // Draining the pipe always wins over the idle flush, so a stall in
            // this task can never cut an access unit that has already arrived.
            biased;
            _ = &mut *stop_rx => break,
            read = stdout.read(&mut buffer) => {
                let n = read?;
                if n == 0 {
                    warn!(%session_id, "portal stream producer closed stdout");
                    break;
                }
                reader.push(&buffer[..n])
            }
            request = keyframe_rx.recv(), if keyframe_channel_open => {
                if request.is_none() {
                    // Sender dropped: the session is being torn down and
                    // stop_rx is about to fire.
                    keyframe_channel_open = false;
                } else if started.elapsed() >= KEYFRAME_RESTART_MIN_INTERVAL {
                    return Ok(PumpOutcome::KeyFrameRequested);
                } else {
                    debug!(%session_id, "keyframe request ignored; the encoder just restarted");
                }
                Vec::new()
            }
            _ = tokio::time::sleep(flush_idle), if reader.has_pending_picture() => {
                reader.flush_pending().into_iter().collect()
            }
        };
        for unit in units {
            if waiting_for_keyframe {
                if !unit.key_frame {
                    continue;
                }
                waiting_for_keyframe = false;
            }
            send_stream_magic(&mut *socket, magic_sent, STREAM_MAGIC_V2).await?;
            if let Some(parameter_sets) = unit.parameter_sets.as_deref() {
                send_h264_frame(
                    &mut *socket,
                    counters.seq,
                    geometry,
                    parameter_sets,
                    false,
                    true,
                )
                .await?;
                counters.seq += 1;
                counters.frames += 1;
            }
            send_h264_frame(
                &mut *socket,
                counters.seq,
                geometry,
                &unit.data,
                unit.key_frame,
                false,
            )
            .await?;
            counters.seq += 1;
            counters.frames += 1;
            frame_count += 1;
        }
        let elapsed = throughput_start.elapsed().as_secs_f64();
        if elapsed >= 2.0 {
            let measured = frame_count as f64 / elapsed;
            debug!(%session_id, fps_measured = measured, fps_target = fps, frames = frame_count, "h264 stream throughput");
            frame_count = 0;
            throughput_start = tokio::time::Instant::now();
        }
    }
    Ok(PumpOutcome::Finished)
}

/// Reads concatenated JPEG frames off a producer pipe and forwards them as
/// `WAYPAD_STREAM_V1` frames.
#[allow(clippy::too_many_arguments)]
async fn pump_jpeg_stream(
    socket: &mut TcpStream,
    magic_sent: &mut bool,
    stdout: &mut ChildStdout,
    stop_rx: &mut oneshot::Receiver<()>,
    counters: &mut StreamCounters,
    session_id: &str,
    width: u32,
    height: u32,
    fps: u32,
    producer: &'static str,
) -> anyhow::Result<()> {
    let mut reader = JpegStreamReader::new();
    let mut buffer = [0u8; 32 * 1024];
    let mut frame_count = 0u64;
    let mut throughput_start = tokio::time::Instant::now();
    loop {
        tokio::select! {
            _ = &mut *stop_rx => break,
            read = stdout.read(&mut buffer) => {
                let n = read?;
                if n == 0 {
                    warn!(%session_id, producer, "jpeg stream producer closed stdout");
                    break;
                }
                for frame in reader.push(&buffer[..n]) {
                    send_stream_magic(&mut *socket, magic_sent, STREAM_MAGIC_V1).await?;
                    send_frame(&mut *socket, counters.seq, width, height, &frame).await?;
                    counters.seq += 1;
                    counters.frames += 1;
                    frame_count += 1;
                }
                let elapsed = throughput_start.elapsed().as_secs_f64();
                if elapsed >= 2.0 {
                    let measured = frame_count as f64 / elapsed;
                    debug!(%session_id, producer, fps_measured = measured, fps_target = fps, frames = frame_count, "jpeg stream throughput");
                    frame_count = 0;
                    throughput_start = tokio::time::Instant::now();
                }
            }
        }
    }
    Ok(())
}

/// Producer stderr is both logged and kept, so a pipeline that dies before its
/// first frame can report why instead of silently degrading to the fallback.
type ProducerError = Arc<std::sync::Mutex<Option<String>>>;

async fn log_child_stderr(
    session_id: String,
    label: &'static str,
    mut stderr: ChildStderr,
    sink: ProducerError,
) {
    let mut buffer = [0u8; 2048];
    loop {
        match stderr.read(&mut buffer).await {
            Ok(0) => break,
            Ok(n) => {
                let text = String::from_utf8_lossy(&buffer[..n]).trim().to_string();
                if !text.is_empty() {
                    warn!(%session_id, producer = label, stderr = %text, "screen stream producer stderr");
                    if let Ok(mut slot) = sink.lock() {
                        // Keep the first real error: later lines are usually
                        // teardown noise that hides the actual cause.
                        let first_error = text
                            .lines()
                            .find(|line| line.contains("ERROR"))
                            .map(str::trim)
                            .map(ToOwned::to_owned);
                        if slot.is_none() || first_error.is_some() && !slot_has_error(&slot) {
                            *slot = first_error.or(Some(text));
                        }
                    }
                }
            }
            Err(err) => {
                warn!(%session_id, producer = label, %err, "failed to read screen stream producer stderr");
                break;
            }
        }
    }
}

fn slot_has_error(slot: &Option<String>) -> bool {
    slot.as_deref().is_some_and(|text| text.contains("ERROR"))
}

const SEND_FRAME_DEADLINE_MS: u64 = 12;
const H264_SEND_TIMEOUT_SECS: u64 = 10;

async fn send_stream_magic(
    socket: &mut TcpStream,
    magic_sent: &mut bool,
    magic: &[u8],
) -> anyhow::Result<()> {
    if *magic_sent {
        return Ok(());
    }
    socket.write_all(magic).await?;
    *magic_sent = true;
    Ok(())
}

fn frame_envelope(header: &str, payload: &[u8]) -> Vec<u8> {
    let header = header.as_bytes();
    let mut buf = Vec::with_capacity(4 + 4 + header.len() + payload.len());
    buf.extend_from_slice(&(header.len() as u32).to_be_bytes());
    buf.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    buf.extend_from_slice(header);
    buf.extend_from_slice(payload);
    buf
}

/// Encoded size of the picture together with the size of the desktop it came from.
///
/// The two differ whenever the client asks for a smaller stream. Only the encoded
/// size describes the pixels on the wire, but the client maps touches onto desktop
/// coordinates, so it needs the source size too: mapping against the encoded size
/// would confine the pointer to a corner of the screen, silently and with no error.
#[derive(Debug, Clone, Copy)]
pub struct FrameGeometry {
    pub width: u32,
    pub height: u32,
    pub source_width: u32,
    pub source_height: u32,
}

impl FrameGeometry {
    fn header_fields(&self) -> serde_json::Map<String, serde_json::Value> {
        let mut fields = serde_json::Map::new();
        fields.insert("width".into(), self.width.into());
        fields.insert("height".into(), self.height.into());
        fields.insert("source_width".into(), self.source_width.into());
        fields.insert("source_height".into(), self.source_height.into());
        fields
    }
}

async fn send_h264_frame(
    socket: &mut TcpStream,
    seq: u64,
    geometry: FrameGeometry,
    payload: &[u8],
    key_frame: bool,
    config: bool,
) -> anyhow::Result<()> {
    let mut header_object = geometry.header_fields();
    header_object.insert("seq".into(), seq.into());
    header_object.insert("timestamp_ms".into(), json!(now_millis()));
    header_object.insert("codec".into(), "h264".into());
    header_object.insert("key_frame".into(), key_frame.into());
    header_object.insert("config".into(), config.into());
    let header = serde_json::Value::Object(header_object).to_string();
    let buf = frame_envelope(&header, payload);
    // Unlike JPEG, an H.264 frame is referenced by everything that follows it,
    // so partial or dropped writes would corrupt the rest of the session:
    // frames are always written whole and only a wedged client aborts.
    timeout(
        Duration::from_secs(H264_SEND_TIMEOUT_SECS),
        socket.write_all(&buf),
    )
    .await
    .context("screen stream client stalled while receiving an H.264 frame")??;
    Ok(())
}

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
    let buf = frame_envelope(&jpeg_frame_header(seq, width, height), jpeg);
    socket.write_all(&buf).await?;
    Ok(())
}

/// The JPEG path reports the source size as the frame size: the picture may be
/// downscaled on the wire, but the client only ever needs desktop coordinates.
fn jpeg_frame_header(seq: u64, width: u32, height: u32) -> String {
    json!({
        "seq": seq,
        "timestamp_ms": now_millis(),
        "codec": "jpeg",
        "width": width,
        "height": height,
        "source_width": width,
        "source_height": height
    })
    .to_string()
}

async fn send_frame_deadline(
    socket: &mut TcpStream,
    seq: u64,
    width: u32,
    height: u32,
    jpeg: &[u8],
    deadline_ms: u64,
) -> anyhow::Result<()> {
    let buf = frame_envelope(&jpeg_frame_header(seq, width, height), jpeg);

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
    /// Origin of the captured source on the desktop, for absolute pointing.
    position: (i32, i32),
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

        let create_token = format!("waypad_create_{}", portal_token());
        let mut create_stream = subscribe_portal_request(&connection, &create_token).await?;
        let mut create_options = HashMap::<&str, OwnedValue>::new();
        create_options.insert("handle_token", Value::from(create_token).try_into()?);
        let session_token = format!("waypad_screen_{}", portal_token());
        create_options.insert(
            "session_handle_token",
            Value::from(session_token).try_into()?,
        );

        if let Some(ref token) = restore_token {
            create_options.insert("restore_token", Value::from(token.as_str()).try_into()?);
            info!("portal restore_token provided, attempting session restoration");
        }
        let _: OwnedObjectPath = proxy.call("CreateSession", &(create_options)).await?;
        let create_response = await_portal_response(&mut create_stream).await?;
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
            let select_token = format!("waypad_select_{}", portal_token());
            let mut select_stream = subscribe_portal_request(&connection, &select_token).await?;
            select_options.insert("handle_token", Value::from(select_token).try_into()?);
            let _: OwnedObjectPath = proxy
                .call("SelectSources", &(&session_handle, select_options))
                .await?;
            let select_response = await_portal_response(&mut select_stream).await?;
            if select_response.response != 0 {
                bail!("ScreenCast source selection was denied by the portal");
            }
            None
        };

        let start_token = format!("waypad_start_{}", portal_token());
        let mut start_stream = subscribe_portal_request(&connection, &start_token).await?;
        let mut start_options = HashMap::<&str, OwnedValue>::new();
        start_options.insert("handle_token", Value::from(start_token).try_into()?);
        let _: OwnedObjectPath = proxy
            .call("Start", &(&session_handle, "", start_options))
            .await?;
        let start_response = await_portal_response(&mut start_stream).await?;
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
        let position = stream_position(&properties);

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
            position,
            pipewire_fd,
            restore_token: saved_token,
            connection: Some(connection),
            session_handle: Some(session_handle),
        })
    }
}

impl Drop for PortalScreenCastSession {
    fn drop(&mut self) {
        if let (Some(connection), Some(handle)) =
            (self.connection.take(), self.session_handle.take())
        {
            tokio::spawn(async move {
                let proxy = zbus::Proxy::new(
                    &connection,
                    "org.freedesktop.portal.Desktop",
                    "/org/freedesktop/portal/desktop",
                    "org.freedesktop.portal.ScreenCast",
                )
                .await;
                if let Ok(proxy) = proxy {
                    let _: Result<(), _> = proxy
                        .call::<_, _, ()>("CloseSession", &(handle.as_str()))
                        .await;
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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum H264Encoder {
    Nvenc,
    X264,
}

impl H264Encoder {
    fn element(self) -> &'static str {
        match self {
            Self::Nvenc => "nvh264enc",
            Self::X264 => "x264enc",
        }
    }

    /// Pixel format fed to the encoder: NVENC uploads NV12 to the GPU with the
    /// least PCIe traffic, x264 encodes I420 without an internal conversion.
    fn raw_format(self) -> &'static str {
        match self {
            Self::Nvenc => "NV12",
            Self::X264 => "I420",
        }
    }

    fn settings(self, bitrate_kbps: u32, gop_size: u32) -> Vec<String> {
        match self {
            Self::Nvenc => vec![
                format!("bitrate={bitrate_kbps}"),
                format!("max-bitrate={bitrate_kbps}"),
                "rc-mode=cbr".into(),
                "preset=p4".into(),
                "tune=ultra-low-latency".into(),
                "zerolatency=true".into(),
                "bframes=0".into(),
                format!("gop-size={gop_size}"),
                "repeat-sequence-header=true".into(),
                "aud=true".into(),
            ],
            Self::X264 => vec![
                "tune=zerolatency".into(),
                "speed-preset=veryfast".into(),
                format!("bitrate={bitrate_kbps}"),
                format!("key-int-max={gop_size}"),
                "bframes=0".into(),
                "byte-stream=true".into(),
                "aud=true".into(),
            ],
        }
    }
}

static H264_ENCODER: OnceLock<Option<H264Encoder>> = OnceLock::new();

/// Picks the H.264 encoder once per process. `gst-inspect-1.0` only proves the
/// plugin is installed, while NVENC additionally needs a usable CUDA context on
/// this hybrid GPU, so each candidate is exercised on a one-buffer pipeline
/// before it is trusted. Falls through to `x264enc` and, when neither works, to
/// the JPEG paths.
fn detect_h264_encoder() -> Option<H264Encoder> {
    *H264_ENCODER.get_or_init(|| {
        let selected = [H264Encoder::Nvenc, H264Encoder::X264]
            .into_iter()
            .find(|candidate| probe_h264_encoder(*candidate));
        match selected {
            Some(encoder) => info!(encoder = encoder.element(), "H.264 screen encoder selected"),
            None => info!("no usable H.264 encoder found; screen streaming stays on JPEG"),
        }
        selected
    })
}

fn probe_h264_encoder(encoder: H264Encoder) -> bool {
    if command_output("gst-inspect-1.0", &[encoder.element()]).is_none() {
        debug!(encoder = encoder.element(), "H.264 encoder element missing");
        return false;
    }
    let args = [
        "-q".into(),
        "videotestsrc".into(),
        "num-buffers=1".into(),
        "!".into(),
        "video/x-raw,width=320,height=240,framerate=30/1".into(),
        "!".into(),
        "videoconvert".into(),
        "!".into(),
        format!("video/x-raw,format={}", encoder.raw_format()),
        "!".into(),
        encoder.element().into(),
        "!".into(),
        "h264parse".into(),
        "!".into(),
        "fakesink".into(),
    ];
    let ok = std::process::Command::new("gst-launch-1.0")
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false);
    if !ok {
        warn!(
            encoder = encoder.element(),
            "H.264 encoder element is installed but failed to initialise"
        );
    }
    ok
}

pub fn h264_encoder_name() -> Option<&'static str> {
    detect_h264_encoder().map(H264Encoder::element)
}

#[derive(Clone, Copy, Debug)]
enum PipelineEncoding {
    H264 {
        encoder: H264Encoder,
        bitrate_kbps: u32,
        gop_size: u32,
    },
    Jpeg {
        quality: u8,
    },
}

impl PipelineEncoding {
    fn codec(self) -> &'static str {
        match self {
            Self::H264 { .. } => "h264",
            Self::Jpeg { .. } => "jpeg",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::H264 { encoder, .. } => encoder.element(),
            Self::Jpeg { .. } => "jpegenc",
        }
    }

    fn bitrate_kbps(self) -> u32 {
        match self {
            Self::H264 { bitrate_kbps, .. } => bitrate_kbps,
            Self::Jpeg { .. } => 0,
        }
    }
}

const DEFAULT_BITRATE_WIDTH: u32 = 1920;
const DEFAULT_BITRATE_HEIGHT: u32 = 1080;

/// Resolves the CBR target. `bitrate_kbps` wins when the client sends it;
/// otherwise the legacy `jpeg_quality` knob is mapped onto bits per pixel so
/// older clients still get a sane H.264 stream (1080p30 at quality 70 lands
/// around 7 Mbit/s instead of the 45-90 Mbit/s the MJPEG path used).
fn resolve_bitrate_kbps(
    requested: Option<u32>,
    width: u32,
    height: u32,
    fps: u32,
    quality: u8,
) -> u32 {
    if let Some(kbps) = requested {
        return kbps.clamp(500, 40_000);
    }
    let width = if width == 0 {
        DEFAULT_BITRATE_WIDTH
    } else {
        width
    };
    let height = if height == 0 {
        DEFAULT_BITRATE_HEIGHT
    } else {
        height
    };
    let quality = f64::from(quality.clamp(35, 92));
    let bits_per_pixel = 0.05 + (quality - 35.0) / 57.0 * 0.11;
    let bits = f64::from(width) * f64::from(height) * f64::from(fps.max(1)) * bits_per_pixel;
    ((bits / 1000.0).round() as u32).clamp(800, 25_000)
}

/// Keyframe every two seconds: short enough that a reconnecting client recovers
/// quickly, long enough that IDR spikes do not dominate the bitrate.
fn keyframe_interval(fps: u32) -> u32 {
    (fps.max(1) * 2).clamp(15, 120)
}

fn even_dimension(value: u32) -> u32 {
    value.max(2) & !1
}

/// Borrows the portal session rather than consuming it: the PipeWire fd has to
/// stay open for the whole stream so the encoder can be respawned for a
/// keyframe request, and dropping the session would also close the portal.
fn spawn_gstreamer_pipewire(
    session: &PortalScreenCastSession,
    fps: u32,
    encoding: PipelineEncoding,
    target_width: Option<u32>,
    target_height: Option<u32>,
) -> anyhow::Result<tokio::process::Child> {
    let fd = session.pipewire_fd.as_raw_fd();
    let args = gstreamer_pipeline_args(
        session.stream_id,
        fps,
        encoding,
        target_width,
        target_height,
    );

    debug!(pipeline = %args.join(" "), "launching GStreamer PipeWire pipeline");
    let mut command = Command::new("gst-launch-1.0");
    command
        .env("PIPEWIRE_VIDEO_BUFFER_TYPE", "mem")
        .args(args)
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

fn gstreamer_pipeline_args(
    stream_id: u32,
    fps: u32,
    encoding: PipelineEncoding,
    target_width: Option<u32>,
    target_height: Option<u32>,
) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "-q".into(),
        "pipewiresrc".into(),
        "fd=3".into(),
        format!("path={stream_id}"),
        "do-timestamp=true".into(),
        "keepalive-time=1000".into(),
        "!".into(),
        "queue".into(),
        "max-size-buffers=4".into(),
        "leaky=downstream".into(),
        "!".into(),
        // A capsfilter constrains, it never converts. Everything the capsfilter
        // below asks for that videoconvert cannot do itself — frame rate and
        // frame size — must have its own converter here, or the constraint
        // travels back up to pipewiresrc, which then demands a format the
        // portal cannot deliver and fails with "error set output format: -22"
        // before a single frame is produced.
        "videorate".into(),
        // drop-only never duplicates frames, so a still screen costs no
        // bandwidth, and it never holds a buffer back to pace the output.
        "drop-only=true".into(),
        format!("max-rate={fps}"),
        "!".into(),
        "videoscale".into(),
        "!".into(),
        "videoconvert".into(),
        "n-threads=4".into(),
        "!".into(),
    ];

    // Pixel format and frame size only. The frame rate is already bounded by
    // videorate, and pinning it here would over-constrain the source for no
    // gain. Square pixels are requested explicitly because some sources
    // otherwise negotiate a non-square aspect the phone cannot correct.
    let mut caps = String::from("video/x-raw");
    if let PipelineEncoding::H264 { encoder, .. } = encoding {
        caps.push_str(&format!(",format={}", encoder.raw_format()));
    }
    if let (Some(width), Some(height)) = (target_width, target_height) {
        caps.push_str(&format!(",width={width},height={height}"));
    }
    caps.push_str(",pixel-aspect-ratio=1/1");
    args.push(caps);
    args.push("!".into());

    match encoding {
        PipelineEncoding::H264 {
            encoder,
            bitrate_kbps,
            gop_size,
        } => {
            args.push(encoder.element().into());
            args.extend(encoder.settings(bitrate_kbps, gop_size));
            args.push("!".into());
            args.push("video/x-h264,profile=high".into());
            args.push("!".into());
            // config-interval=-1 guarantees SPS/PPS in front of every IDR even
            // for encoders that cannot repeat them on their own.
            args.push("h264parse".into());
            args.push("config-interval=-1".into());
            args.push("!".into());
            args.push("video/x-h264,stream-format=byte-stream,alignment=au".into());
            args.push("!".into());
            // No leaky queue after the encoder: dropping an encoded frame here
            // would break every frame that references it. Backpressure instead
            // travels upstream to the leaky queue in front of videoconvert.
            args.push("queue".into());
            args.push("max-size-buffers=8".into());
        }
        PipelineEncoding::Jpeg { quality } => {
            args.push("jpegenc".into());
            args.push(format!("quality={}", quality.min(75)));
            args.push("idct-method=1".into());
            args.push("!".into());
            args.push("queue".into());
            args.push("max-size-buffers=8".into());
            args.push("leaky=downstream".into());
        }
    }

    args.push("!".into());
    args.push("fdsink".into());
    args.push("fd=1".into());
    args.push("sync=false".into());
    args
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

/// Largest Annex-B payload kept while looking for the next access unit. A 1080p
/// IDR stays far below this, so hitting the cap means the producer is emitting
/// something that is not a byte stream and the reader resynchronises.
const MAX_ANNEX_B_BUFFER: usize = 8 * 1024 * 1024;

#[derive(Debug)]
struct AccessUnit {
    data: Vec<u8>,
    key_frame: bool,
    /// SPS/PPS copied out of the access unit, sent ahead of it as a config
    /// frame. They stay inline in `data` as well so a decoder that ignores
    /// config frames still finds them before the IDR.
    parameter_sets: Option<Vec<u8>>,
}

#[derive(Clone, Copy, Debug)]
struct NalRef {
    /// Offset of the leading byte of the start code, not of the NAL header.
    start: usize,
    kind: u8,
    /// `first_mb_in_slice == 0`, only meaningful for slice NAL units.
    first_slice: bool,
}

impl NalRef {
    fn is_slice(self) -> bool {
        matches!(self.kind, 1..=5)
    }

    fn starts_access_unit(self, has_slice: bool) -> bool {
        match self.kind {
            // Access unit delimiter: always opens a picture.
            9 => true,
            // Parameter sets and SEI belong to the picture that follows them.
            6 | 7 | 8 | 13 | 14 | 15 => has_slice,
            // A slice opens a new picture only when it restarts the macroblock
            // scan, which keeps multi-slice frames (x264 sliced threads) whole.
            1..=5 => has_slice && self.first_slice,
            _ => false,
        }
    }
}

/// Splits the encoder's Annex-B byte stream into whole access units. Reads from
/// the producer pipe land on arbitrary boundaries, so NAL units are only cut
/// once the start code of the following one has actually been seen.
struct AnnexBStreamReader {
    buffer: Vec<u8>,
    nals: Vec<NalRef>,
    scan_from: usize,
}

impl AnnexBStreamReader {
    fn new() -> Self {
        Self {
            buffer: Vec::new(),
            nals: Vec::new(),
            scan_from: 0,
        }
    }

    fn push(&mut self, chunk: &[u8]) -> Vec<AccessUnit> {
        self.buffer.extend_from_slice(chunk);
        self.scan();
        let mut units = Vec::new();
        while let Some(boundary) = self.next_boundary() {
            units.push(self.take_access_unit(boundary));
        }
        if self.buffer.len() > MAX_ANNEX_B_BUFFER {
            warn!(
                bytes = self.buffer.len(),
                "annex-b buffer overflow; resynchronising on the next start code"
            );
            self.buffer.clear();
            self.nals.clear();
            self.scan_from = 0;
        }
        units
    }

    fn scan(&mut self) {
        // A start code plus the two bytes needed to classify the NAL span five
        // bytes, so scanning resumes far enough back to complete a pattern that
        // was still truncated on the previous read.
        let mut index = self.scan_from;
        while index + 5 <= self.buffer.len() {
            if self.buffer[index] != 0 || self.buffer[index + 1] != 0 || self.buffer[index + 2] != 1
            {
                index += 1;
                continue;
            }
            // Four-byte start codes are three-byte ones with a leading zero;
            // that zero can never be payload because emulation prevention
            // forbids `00 00 00` inside a NAL.
            let start = if index > 0 && self.buffer[index - 1] == 0 {
                index - 1
            } else {
                index
            };
            self.nals.push(NalRef {
                start,
                kind: self.buffer[index + 3] & 0x1f,
                first_slice: self.buffer[index + 4] & 0x80 != 0,
            });
            index += 3;
        }
        self.scan_from = self.buffer.len().saturating_sub(4);
    }

    fn next_boundary(&self) -> Option<usize> {
        let mut has_slice = false;
        for (index, nal) in self.nals.iter().enumerate() {
            if index > 0 && nal.starts_access_unit(has_slice) {
                return Some(index);
            }
            if nal.is_slice() {
                has_slice = true;
            }
        }
        None
    }

    fn has_pending_picture(&self) -> bool {
        self.nals.iter().any(|nal| nal.is_slice())
    }

    /// Releases the buffered access unit without waiting for the start code of
    /// the next one, which would otherwise cost a full frame interval of
    /// latency. Only safe once the producer pipe has gone idle: the encoder
    /// writes one access unit per pipe write, so an idle pipe means the picture
    /// is complete.
    fn flush_pending(&mut self) -> Option<AccessUnit> {
        if !self.has_pending_picture() {
            return None;
        }
        let end = self.buffer.len();
        Some(self.take_access_unit_before(self.nals.len(), end))
    }

    fn take_access_unit(&mut self, boundary: usize) -> AccessUnit {
        let end = self.nals[boundary].start;
        self.take_access_unit_before(boundary, end)
    }

    fn take_access_unit_before(&mut self, boundary: usize, end: usize) -> AccessUnit {
        let begin = self.nals[0].start;
        let key_frame = self.nals[..boundary].iter().any(|nal| nal.kind == 5);
        let mut parameter_sets = Vec::new();
        for (index, nal) in self.nals[..boundary].iter().enumerate() {
            if !matches!(nal.kind, 7 | 8) {
                continue;
            }
            let stop = self.nals.get(index + 1).map_or(end, |next| next.start);
            parameter_sets.extend_from_slice(&self.buffer[nal.start..stop]);
        }
        let data = self.buffer[begin..end].to_vec();
        self.buffer.drain(..end);
        self.nals.drain(..boundary);
        for nal in &mut self.nals {
            nal.start -= end;
        }
        self.scan_from = self.scan_from.saturating_sub(end);
        AccessUnit {
            data,
            key_frame,
            parameter_sets: (!parameter_sets.is_empty()).then_some(parameter_sets),
        }
    }
}

/// Subscribes to a portal request's `Response` before the request exists.
///
/// The portal can emit `Response` before the method call that creates the
/// request has even returned, and does exactly that once a permission has been
/// persisted and no dialog needs to be shown. Subscribing afterwards misses the
/// signal and then waits out the full timeout for something that already
/// happened. The request path is therefore derived from the token up front,
/// which is the reason the portal API accepts a `handle_token` at all.
async fn subscribe_portal_request(
    connection: &zbus::Connection,
    token: &str,
) -> anyhow::Result<zbus::proxy::SignalStream<'static>> {
    let unique = connection
        .unique_name()
        .map(|name| name.as_str().to_string())
        .unwrap_or_default();
    let sender = unique.trim_start_matches(':').replace('.', "_");
    let path = format!("/org/freedesktop/portal/desktop/request/{sender}/{token}");
    let proxy = zbus::Proxy::new(
        connection,
        "org.freedesktop.portal.Desktop",
        path,
        "org.freedesktop.portal.Request",
    )
    .await?;
    Ok(proxy.receive_signal("Response").await?)
}

async fn await_portal_response(
    stream: &mut zbus::proxy::SignalStream<'static>,
) -> anyhow::Result<PortalResponse> {
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

/// Reads the source size the portal reports for a stream.
///
/// The portal declares `size` as a pair of *signed* 32-bit integers, so asking
/// zvariant for `(u32, u32)` fails on a signature mismatch and silently drops
/// the dimensions. That is not a cosmetic loss: the client would receive a 0x0
/// source size, leaving the decoder with no format to configure and the touch
/// mapping with nothing to map against.
fn stream_size(properties: &HashMap<String, OwnedValue>) -> (Option<u32>, Option<u32>) {
    match stream_int_pair(properties, "size") {
        Some((width, height)) if width > 0 && height > 0 => {
            (Some(width as u32), Some(height as u32))
        }
        _ => (None, None),
    }
}

/// Reads where the captured source sits on the desktop.
///
/// Absolute pointer commands are in desktop coordinates, so a stream of the
/// second monitor needs its origin added or every touch lands on the first one.
fn stream_position(properties: &HashMap<String, OwnedValue>) -> (i32, i32) {
    stream_int_pair(properties, "position").unwrap_or((0, 0))
}

fn stream_int_pair(properties: &HashMap<String, OwnedValue>, key: &str) -> Option<(i32, i32)> {
    let value = properties.get(key)?;
    if let Ok(pair) = value.try_clone().ok()?.try_into() {
        return Some(pair);
    }
    let (width, height): (u32, u32) = value.try_clone().ok()?.try_into().ok()?;
    Some((width as i32, height as i32))
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
        let Ok(w) = res_parts[0].parse::<u32>() else {
            continue;
        };
        let Ok(h) = res_parts[1].parse::<u32>() else {
            continue;
        };
        let Ok(x) = main_parts[1].parse::<i32>() else {
            continue;
        };
        let Ok(y) = main_parts[2].parse::<i32>() else {
            continue;
        };

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
    magic_sent: &mut bool,
    session_id: String,
    source: ScreenSource,
    tuning: StreamTuning,
    stop_rx: &mut oneshot::Receiver<()>,
) -> anyhow::Result<()> {
    let StreamTuning {
        fps,
        quality,
        max_width,
        max_height,
        ..
    } = tuning;
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
        "-y",
        "-hide_banner",
        "-loglevel",
        "error",
        "-f",
        "x11grab",
        "-framerate",
        &fps.to_string(),
        "-video_size",
        &format!("{cap_w}x{cap_h}"),
        "-i",
        &input_spec,
    ]);
    // Scale if needed
    if target_w.is_some() || target_h.is_some() {
        cmd.args(["-vf", &format!("scale={}:{}", cap_w, cap_h)]);
    }
    cmd.args([
        "-c:v",
        "mjpeg",
        "-q:v",
        &quality.to_string(),
        "-f",
        "mjpeg",
        "pipe:1",
    ])
    .stdout(std::process::Stdio::piped())
    .stderr(std::process::Stdio::piped());

    let mut child = cmd
        .spawn()
        .context("failed to spawn ffmpeg for X11 capture")?;
    if let Some(stderr) = child.stderr.take() {
        tokio::spawn(log_child_stderr(
            session_id.clone(),
            "ffmpeg",
            stderr,
            Arc::new(std::sync::Mutex::new(None)),
        ));
    }
    let mut stdout = child.stdout.take().context("ffmpeg stdout unavailable")?;
    let mut counters = StreamCounters::default();
    let pumped = pump_jpeg_stream(
        socket,
        magic_sent,
        &mut stdout,
        stop_rx,
        &mut counters,
        &session_id,
        source.width,
        source.height,
        fps,
        "ffmpeg",
    )
    .await;
    let _ = child.kill().await;
    pumped?;
    let frames = counters.frames;
    if frames == 0 {
        anyhow::bail!("ffmpeg x11 produced no frames");
    }
    info!(%session_id, total_frames = frames, "x11 stream stopped");
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

    let create_token = format!("waypad_auth_create_{}", portal_token());
    let mut create_stream = subscribe_portal_request(&connection, &create_token).await?;
    let mut create_options = HashMap::<&str, OwnedValue>::new();
    create_options.insert("handle_token", Value::from(create_token).try_into()?);
    create_options.insert(
        "session_handle_token",
        Value::from(format!("waypad_auth_session_{}", portal_token())).try_into()?,
    );

    let _: OwnedObjectPath = proxy.call("CreateSession", &(create_options)).await?;
    let create_response = await_portal_response(&mut create_stream).await?;
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
    let select_token = format!("waypad_auth_select_{}", portal_token());
    let mut select_stream = subscribe_portal_request(&connection, &select_token).await?;
    select_options.insert("handle_token", Value::from(select_token).try_into()?);
    let _: OwnedObjectPath = proxy
        .call("SelectSources", &(&session_handle, select_options))
        .await?;
    let select_response = await_portal_response(&mut select_stream).await?;
    if select_response.response != 0 {
        bail!("ScreenCast source selection was denied");
    }

    let start_token = format!("waypad_auth_start_{}", portal_token());
    let mut start_stream = subscribe_portal_request(&connection, &start_token).await?;
    let mut start_options = HashMap::<&str, OwnedValue>::new();
    start_options.insert("handle_token", Value::from(start_token).try_into()?);
    let _: OwnedObjectPath = proxy
        .call("Start", &(&session_handle, "", start_options))
        .await?;
    let start_response = await_portal_response(&mut start_stream).await?;
    if start_response.response != 0 {
        bail!(
            "ScreenCast authorization was denied or cancelled. Approve the dialog on your desktop."
        );
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
            warn!(
                "portal authorization succeeded but no restore_token returned; persist_mode may not be supported by this backend"
            );
            let _: Result<(), _> = proxy
                .call::<_, _, ()>("CloseSession", &(session_handle.as_str()))
                .await;
            anyhow::bail!(
                "Portal authorization completed but restore_token not available. The portal should now be approved for this session. Try streaming immediately."
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AnnexBStreamReader, FrameGeometry, H264Encoder, JpegStreamReader, PipelineEncoding,
        capture_scale, even_dimension, find_marker, gstreamer_pipeline_args, is_client_disconnect,
        jpeg_frame_header, keyframe_interval, resolve_bitrate_kbps, stream_position, stream_size,
        target_dimensions,
    };
    use std::collections::HashMap;
    use zbus::zvariant::{OwnedValue, Value};

    fn portal_properties(key: &str, pair: (i32, i32)) -> HashMap<String, OwnedValue> {
        let mut properties = HashMap::new();
        let value = Value::from((pair.0, pair.1));
        properties.insert(key.to_string(), OwnedValue::try_from(value).unwrap());
        properties
    }

    #[test]
    fn reads_the_signed_integer_pair_the_screencast_portal_actually_sends() {
        // The portal declares size and position as (ii). Asking zvariant for
        // (u32, u32) fails the signature check and silently yields nothing,
        // which left the client with a 0x0 source and a black picture.
        let properties = portal_properties("size", (1920, 1080));
        assert_eq!(stream_size(&properties), (Some(1920), Some(1080)));
    }

    #[test]
    fn treats_a_degenerate_portal_size_as_unknown() {
        assert_eq!(
            stream_size(&portal_properties("size", (0, 0))),
            (None, None)
        );
        assert_eq!(stream_size(&HashMap::new()), (None, None));
    }

    #[test]
    fn reads_the_source_origin_for_absolute_pointing() {
        // A stream of the second monitor reports a non-zero origin; without it
        // every absolute touch would land on the first monitor.
        assert_eq!(
            stream_position(&portal_properties("position", (1920, 0))),
            (1920, 0)
        );
        assert_eq!(stream_position(&HashMap::new()), (0, 0));
    }

    #[test]
    fn frame_header_carries_the_desktop_size_next_to_the_encoded_size() {
        // A phone asking for a smaller stream still points at desktop pixels, so
        // both sizes have to travel: mapping touches against the encoded size
        // would silently confine the pointer to a corner of the screen.
        let geometry = FrameGeometry {
            width: 1600,
            height: 900,
            source_width: 1920,
            source_height: 1080,
        };
        let fields = geometry.header_fields();
        assert_eq!(fields["width"], 1600);
        assert_eq!(fields["height"], 900);
        assert_eq!(fields["source_width"], 1920);
        assert_eq!(fields["source_height"], 1080);
    }

    #[test]
    fn jpeg_header_reports_the_desktop_size_as_the_source() {
        let header = jpeg_frame_header(7, 1920, 1080);
        let parsed: serde_json::Value = serde_json::from_str(&header).unwrap();
        assert_eq!(parsed["source_width"], 1920);
        assert_eq!(parsed["source_height"], 1080);
        assert_eq!(parsed["codec"], "jpeg");
    }

    fn pipeline(encoding: PipelineEncoding, width: Option<u32>, height: Option<u32>) -> String {
        gstreamer_pipeline_args(42, 30, encoding, width, height).join(" ")
    }

    fn h264(encoder: H264Encoder) -> PipelineEncoding {
        PipelineEncoding::H264 {
            encoder,
            bitrate_kbps: 6000,
            gop_size: 60,
        }
    }

    /// A capsfilter constrains but never converts, so every property it pins
    /// which `videoconvert` cannot change on its own needs its own converter
    /// upstream. Without `videorate` the framerate constraint alone travels
    /// back to `pipewiresrc`, which then fails to negotiate with the portal and
    /// the stream silently degrades to the grim fallback.
    #[test]
    fn pipeline_can_convert_everything_the_capsfilter_pins() {
        let cases = [
            pipeline(h264(H264Encoder::Nvenc), None, None),
            pipeline(h264(H264Encoder::Nvenc), Some(1280), Some(720)),
            pipeline(h264(H264Encoder::X264), Some(1280), Some(720)),
            pipeline(PipelineEncoding::Jpeg { quality: 70 }, None, None),
            pipeline(PipelineEncoding::Jpeg { quality: 70 }, Some(960), Some(540)),
        ];
        for pipeline in cases {
            let caps = pipeline
                .find("video/x-raw")
                .expect("raw capsfilter is present");
            let upstream = &pipeline[..caps];
            for converter in ["videorate", "videoscale", "videoconvert"] {
                assert!(
                    upstream.contains(converter),
                    "{converter} missing upstream of the capsfilter in: {pipeline}"
                );
            }
            // The frame rate is bounded by videorate instead, so pinning it in
            // the capsfilter would over-constrain the source for nothing.
            assert!(
                !pipeline.contains("framerate="),
                "capsfilter must not pin a framerate: {pipeline}"
            );
            assert!(pipeline.contains("max-rate=30"), "{pipeline}");
            assert!(pipeline.contains("pixel-aspect-ratio=1/1"), "{pipeline}");
        }
    }

    #[test]
    fn h264_pipeline_never_drops_encoded_frames() {
        let nvenc = pipeline(h264(H264Encoder::Nvenc), None, None);
        let encoder = nvenc.find("nvh264enc").expect("encoder is present");
        // Backpressure may only drop raw frames, never encoded ones: a dropped
        // encoded frame breaks every frame that references it.
        assert!(nvenc[..encoder].contains("leaky=downstream"), "{nvenc}");
        assert!(!nvenc[encoder..].contains("leaky="), "{nvenc}");
        // Annex-B with SPS/PPS ahead of every IDR is what the phone decodes.
        assert!(nvenc.contains("repeat-sequence-header=true"), "{nvenc}");
        assert!(nvenc.contains("h264parse config-interval=-1"), "{nvenc}");
        assert!(
            nvenc.contains("video/x-h264,stream-format=byte-stream,alignment=au"),
            "{nvenc}"
        );
        assert!(nvenc.contains("bframes=0"), "{nvenc}");
        assert!(nvenc.contains("format=NV12"), "{nvenc}");
        assert!(pipeline(h264(H264Encoder::X264), None, None).contains("format=I420"));
    }

    /// Annex-B NAL unit with a four-byte start code. `header` is the raw NAL
    /// header byte, `first` the byte after it (its top bit carries
    /// `first_mb_in_slice == 0` for slices).
    fn nal(header: u8, first: u8, payload: &[u8]) -> Vec<u8> {
        let mut unit = vec![0, 0, 0, 1, header, first];
        unit.extend_from_slice(payload);
        unit
    }

    fn short_nal(header: u8, first: u8, payload: &[u8]) -> Vec<u8> {
        let mut unit = vec![0, 0, 1, header, first];
        unit.extend_from_slice(payload);
        unit
    }

    fn aud() -> Vec<u8> {
        nal(0x09, 0x10, &[])
    }

    fn sps() -> Vec<u8> {
        nal(0x67, 0x64, &[0x00, 0x28])
    }

    fn pps() -> Vec<u8> {
        nal(0x68, 0xeb, &[0xe3, 0xcb])
    }

    fn idr() -> Vec<u8> {
        nal(0x65, 0x88, &[1, 2, 3, 4])
    }

    fn slice() -> Vec<u8> {
        nal(0x41, 0x9a, &[5, 6, 7, 8])
    }

    #[test]
    fn splits_annex_b_access_units() {
        let mut reader = AnnexBStreamReader::new();
        let mut stream = Vec::new();
        stream.extend_from_slice(&aud());
        stream.extend_from_slice(&sps());
        stream.extend_from_slice(&pps());
        stream.extend_from_slice(&idr());
        let keyframe_len = stream.len();
        stream.extend_from_slice(&aud());
        stream.extend_from_slice(&slice());

        let units = reader.push(&stream);
        // The trailing access unit stays buffered until the next one starts.
        assert_eq!(units.len(), 1);
        assert!(units[0].key_frame);
        assert_eq!(units[0].data.len(), keyframe_len);
        let mut parameter_sets = sps();
        parameter_sets.extend_from_slice(&pps());
        assert_eq!(
            units[0].parameter_sets.as_deref(),
            Some(&parameter_sets[..])
        );

        let mut tail = aud();
        tail.extend_from_slice(&slice());
        let units = reader.push(&tail);
        assert_eq!(units.len(), 1);
        assert!(!units[0].key_frame);
        assert!(units[0].parameter_sets.is_none());
    }

    #[test]
    fn flushes_the_buffered_picture_when_the_producer_goes_idle() {
        let mut reader = AnnexBStreamReader::new();
        let mut stream = aud();
        stream.extend_from_slice(&sps());
        stream.extend_from_slice(&pps());
        stream.extend_from_slice(&idr());

        assert!(reader.push(&stream).is_empty());
        assert!(reader.has_pending_picture());
        let unit = reader.flush_pending().expect("buffered keyframe");
        assert!(unit.key_frame);
        assert_eq!(unit.data, stream);
        assert!(unit.parameter_sets.is_some());

        // Nothing is left to flush, and a parameter-set-only tail is never
        // mistaken for a picture.
        assert!(!reader.has_pending_picture());
        assert!(reader.flush_pending().is_none());
        assert!(reader.push(&sps()).is_empty());
        assert!(!reader.has_pending_picture());
        assert!(reader.flush_pending().is_none());
    }

    #[test]
    fn splits_annex_b_units_across_buffer_reads() {
        let mut stream = Vec::new();
        stream.extend_from_slice(&aud());
        stream.extend_from_slice(&sps());
        stream.extend_from_slice(&pps());
        stream.extend_from_slice(&idr());
        let keyframe = stream.clone();
        stream.extend_from_slice(&aud());
        stream.extend_from_slice(&slice());
        let second = stream[keyframe.len()..].to_vec();
        stream.extend_from_slice(&aud());
        stream.extend_from_slice(&slice());

        // Every possible split point must yield the same two access units,
        // including cuts that land in the middle of a start code.
        for split in 1..stream.len() {
            let mut reader = AnnexBStreamReader::new();
            let mut units = reader.push(&stream[..split]);
            units.extend(reader.push(&stream[split..]));
            assert_eq!(units.len(), 2, "split at {split}");
            assert_eq!(units[0].data, keyframe, "split at {split}");
            assert!(units[0].key_frame, "split at {split}");
            assert_eq!(units[1].data, second, "split at {split}");
            assert!(!units[1].key_frame, "split at {split}");
        }
    }

    #[test]
    fn accepts_three_and_four_byte_start_codes() {
        let mut reader = AnnexBStreamReader::new();
        let mut stream = short_nal(0x67, 0x64, &[0x00, 0x28]);
        stream.extend_from_slice(&short_nal(0x68, 0xeb, &[0xe3, 0xcb]));
        stream.extend_from_slice(&short_nal(0x65, 0x88, &[1, 2, 3, 4]));
        let keyframe_len = stream.len();
        stream.extend_from_slice(&sps());
        stream.extend_from_slice(&idr());

        let units = reader.push(&stream);
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].data.len(), keyframe_len);
        assert!(units[0].key_frame);
        let mut parameter_sets = short_nal(0x67, 0x64, &[0x00, 0x28]);
        parameter_sets.extend_from_slice(&short_nal(0x68, 0xeb, &[0xe3, 0xcb]));
        assert_eq!(
            units[0].parameter_sets.as_deref(),
            Some(&parameter_sets[..])
        );
    }

    #[test]
    fn keeps_multi_slice_pictures_in_one_access_unit() {
        let mut reader = AnnexBStreamReader::new();
        let mut stream = nal(0x41, 0x9a, &[1, 2]);
        // Second slice of the same picture: first_mb_in_slice != 0.
        stream.extend_from_slice(&nal(0x41, 0x0a, &[3, 4]));
        stream.extend_from_slice(&nal(0x41, 0x0a, &[5, 6]));
        let picture_len = stream.len();
        stream.extend_from_slice(&nal(0x41, 0x9a, &[7, 8]));
        stream.extend_from_slice(&nal(0x41, 0x0a, &[9, 10]));

        let units = reader.push(&stream);
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].data.len(), picture_len);
    }

    #[test]
    fn ignores_leading_bytes_before_the_first_start_code() {
        let mut reader = AnnexBStreamReader::new();
        let mut stream = vec![0xde, 0xad, 0xbe, 0xef];
        stream.extend_from_slice(&aud());
        stream.extend_from_slice(&idr());
        let keyframe = stream[4..].to_vec();
        stream.extend_from_slice(&aud());
        stream.extend_from_slice(&slice());

        let units = reader.push(&stream);
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].data, keyframe);
    }

    #[test]
    fn maps_legacy_quality_and_explicit_bitrate() {
        assert_eq!(resolve_bitrate_kbps(Some(4500), 1920, 1080, 30, 70), 4500);
        assert_eq!(
            resolve_bitrate_kbps(Some(90_000), 1920, 1080, 30, 70),
            40_000
        );
        // Unknown portal geometry falls back to 1080p so the estimate stays sane.
        assert_eq!(
            resolve_bitrate_kbps(None, 0, 0, 30, 70),
            resolve_bitrate_kbps(None, 1920, 1080, 30, 70)
        );
        let quality_70 = resolve_bitrate_kbps(None, 1920, 1080, 30, 70);
        assert!((5_000..=9_000).contains(&quality_70), "{quality_70}");
        assert!(resolve_bitrate_kbps(None, 1920, 1080, 30, 92) > quality_70);
        assert!(resolve_bitrate_kbps(None, 1920, 1080, 30, 35) < quality_70);
    }

    #[test]
    fn clamps_keyframe_interval_and_dimensions() {
        assert_eq!(keyframe_interval(30), 60);
        assert_eq!(keyframe_interval(1), 15);
        assert_eq!(keyframe_interval(60), 120);
        assert_eq!(even_dimension(1081), 1080);
        assert_eq!(even_dimension(1080), 1080);
        assert_eq!(even_dimension(1), 2);
    }

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
