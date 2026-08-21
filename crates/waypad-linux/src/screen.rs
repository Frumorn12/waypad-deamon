//! Screen capture on Linux: XDG ScreenCast portal, Hyprland `grim`, X11 grab.
//!
//! The session lifetime, envelope framing and pump live in
//! `waypad_core::stream`. What is here is only the part that turns a Wayland
//! desktop into encoded pictures, which is three quite different mechanisms
//! wearing one interface.
//!
//! The fallback order matters and is decided in [`LinuxCaptureBackend::open`]
//! rather than mid-stream: nothing has been written to the client at that
//! point, so the codec is still free to change. Once a frame has gone out, the
//! handshake line has fixed the codec and falling back is no longer possible.

use crate::platform::command_output;
use anyhow::{Context, bail};
use async_trait::async_trait;
use futures_util::StreamExt;
use serde::Deserialize;
use std::{
    collections::{HashMap, VecDeque},
    os::fd::AsRawFd,
    sync::{Arc, OnceLock},
    time::Duration,
};
use tokio::{
    io::AsyncReadExt,
    process::{Child, ChildStderr, ChildStdout, Command},
    sync::{Mutex, RwLock},
    time::{Instant, timeout},
};
use tracing::{debug, error, info, warn};
use uuid::Uuid;
use waypad_core::{
    backend::{CaptureBackend, EncodedUnit, FrameProducer, StreamEncoding},
    capability::Capabilities,
    state::StatePaths,
    stream::{
        AnnexBStreamReader, FrameGeometry, JpegStreamReader, ScreenSource, StreamTuning,
        annexb::AccessUnit, capture_scale, even_dimension, keyframe_interval, resolve_bitrate_kbps,
        target_dimensions,
    },
};
use zbus::zvariant::{OwnedFd, OwnedObjectPath, OwnedValue, Value};

/// How long a freshly spawned pipeline gets to produce its first picture before
/// it is written off and the fallback takes over. Portal approval has already
/// happened by then, so this only covers encoder negotiation.
const FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(15);

/// Encoder respawns are cheap but not free, so a client that asks for keyframes
/// repeatedly cannot make the pipeline thrash.
const KEYFRAME_RESTART_MIN_INTERVAL: Duration = Duration::from_secs(1);

/// `grim` screenshots the whole monitor per frame and is far slower than the
/// PipeWire path, so the fallback caps resolution hard rather than trying and
/// delivering two frames a second.
const GRIM_MAX_SCALE: f64 = 0.4;
/// Above this the screenshot cost dominates; the fallback is about staying
/// usable, not about looking good.
const GRIM_MAX_QUALITY: u8 = 35;

pub struct LinuxCaptureBackend {
    capabilities: Arc<RwLock<Capabilities>>,
    paths: Arc<StatePaths>,
    /// Why the portal pipeline last fell back. Surfaced through the control
    /// channel so a permanently broken fast path cannot hide behind a fallback
    /// that merely looks slow.
    last_error: Mutex<Option<String>>,
}

impl std::fmt::Debug for LinuxCaptureBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LinuxCaptureBackend")
            .finish_non_exhaustive()
    }
}

impl LinuxCaptureBackend {
    pub fn new(capabilities: Arc<RwLock<Capabilities>>, paths: StatePaths) -> Self {
        Self {
            capabilities,
            paths: Arc::new(paths),
            last_error: Mutex::new(None),
        }
    }
}

#[async_trait]
impl CaptureBackend for LinuxCaptureBackend {
    fn name(&self) -> &'static str {
        "linux-screencast"
    }

    async fn list_sources(&self) -> anyhow::Result<Vec<ScreenSource>> {
        let capabilities = self.capabilities.read().await.clone();
        let portal_available = capabilities.capture.portal_screencast_available
            && capabilities.capture.pipewire_runtime_available
            && capabilities.capture.gstreamer_pipewire_available;

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
            for (index, mut monitor) in monitors.into_iter().enumerate() {
                monitor.focused = index == 0 && sources.iter().all(|source| !source.focused);
                sources.push(monitor);
            }
        }
        // The X11 grab path needs no portal approval and is fast, so it is
        // offered whenever the tools for it exist.
        if std::env::var("DISPLAY").is_ok()
            && crate::platform::command_exists("xrandr")
            && crate::platform::command_exists("ffmpeg")
        {
            match list_x11_monitors().await {
                Ok(monitors) => {
                    for (index, mut monitor) in monitors.into_iter().enumerate() {
                        monitor.focused =
                            index == 0 && sources.iter().all(|source| !source.focused);
                        sources.push(monitor);
                    }
                }
                Err(err) => warn!(%err, "failed to enumerate X11 monitors"),
            }
        }
        Ok(sources)
    }

    async fn open(
        &self,
        source: &ScreenSource,
        tuning: StreamTuning,
    ) -> anyhow::Result<Box<dyn FrameProducer>> {
        match source.backend.as_str() {
            "wayland-screencast-portal" => {
                match PortalProducer::start(tuning, self.paths.as_ref()).await {
                    Ok(producer) => {
                        *self.last_error.lock().await = None;
                        Ok(Box::new(producer))
                    }
                    Err(err) => {
                        let detail = format!("{err:#}");
                        *self.last_error.lock().await = Some(detail.clone());
                        // Loud on purpose: the grim fallback still produces a
                        // picture, so a permanently broken fast path is
                        // invisible unless the reason is reported.
                        error!(
                            error = %detail,
                            "portal screen pipeline failed; falling back to the grim screenshot backend, which caps out near 6 FPS"
                        );
                        Ok(Box::new(GrimProducer::start(source.clone(), tuning)))
                    }
                }
            }
            "x11-ffmpeg" => Ok(Box::new(X11Producer::start(source.clone(), tuning).await?)),
            _ => Ok(Box::new(GrimProducer::start(source.clone(), tuning))),
        }
    }

    async fn last_error(&self) -> Option<String> {
        self.last_error.lock().await.clone()
    }

    async fn announced_codec(&self, source: &ScreenSource) -> String {
        // Hardware H.264 only rides on the PipeWire pipeline; the grim and X11
        // fallbacks stay on JPEG, so the announced codec follows the backend.
        let h264 = source.backend == "wayland-screencast-portal" && detect_h264_encoder().is_some();
        if h264 { "h264".into() } else { "jpeg".into() }
    }

    fn source_origin(&self, source: &ScreenSource) -> Option<(i32, i32)> {
        // `grim -o <output>` captures one monitor, so its frames are in that
        // monitor's own coordinates and need the origin added back. The portal
        // and X11 paths already deliver desktop coordinates.
        (source.backend == "hyprland-grim").then_some((source.x, source.y))
    }
}

/// The PipeWire pipeline behind the ScreenCast portal.
///
/// Owns the portal session for its whole life: the PipeWire file descriptor has
/// to stay open so the encoder can be respawned for a keyframe request without
/// asking the user to approve anything again.
struct PortalProducer {
    portal: PortalScreenCastSession,
    fps: u32,
    encoding: PipelineEncoding,
    target: (Option<u32>, Option<u32>),
    geometry: FrameGeometry,
    child: Child,
    stdout: ChildStdout,
    stderr_task: Option<tokio::task::JoinHandle<()>>,
    producer_error: ProducerError,
    annexb: AnnexBStreamReader,
    jpeg: JpegStreamReader,
    pending: VecDeque<EncodedUnit>,
    /// A decoder joining on a P-frame shows nothing until the next IDR, so the
    /// leading non-IDR frames of a fresh pipeline are dropped rather than sent.
    waiting_for_keyframe: bool,
    last_start: Instant,
    /// Half a frame interval, bounded, so the pipe is unambiguously idle before
    /// a buffered picture is released early.
    flush_idle: Duration,
}

impl PortalProducer {
    async fn start(tuning: StreamTuning, paths: &StatePaths) -> anyhow::Result<Self> {
        let restore_token = waypad_core::state::load_portal_restore_token(paths);
        let portal = match PortalScreenCastSession::start(restore_token).await {
            Ok(portal) => portal,
            Err(first_err) => {
                // A stale restore token is worth one retry without it, but only
                // if there was one: otherwise this would just ask twice.
                if waypad_core::state::load_portal_restore_token(paths).is_some() {
                    warn!(%first_err, "portal restore failed; retrying without restore token");
                    PortalScreenCastSession::start(None).await?
                } else {
                    return Err(first_err);
                }
            }
        };
        if let Some(ref token) = portal.restore_token
            && let Err(err) = waypad_core::state::save_portal_restore_token(paths, token)
        {
            warn!(%err, "failed to save portal restore token");
        }

        let source_width = portal.width.unwrap_or(0);
        let source_height = portal.height.unwrap_or(0);
        let (mut target_width, mut target_height) = target_dimensions(
            source_width,
            source_height,
            tuning.max_width,
            tuning.max_height,
        );
        let encoding = match detect_h264_encoder() {
            Some(encoder) => {
                // H.264 macroblocks are 16x16 and chroma is subsampled, so both
                // dimensions have to stay even or the encoder refuses to
                // negotiate.
                target_width = target_width.map(even_dimension);
                target_height = target_height.map(even_dimension);
                PipelineEncoding::H264 {
                    encoder,
                    bitrate_kbps: resolve_bitrate_kbps(
                        tuning.bitrate_kbps,
                        target_width.unwrap_or(source_width),
                        target_height.unwrap_or(source_height),
                        tuning.fps,
                        tuning.quality,
                    ),
                    gop_size: keyframe_interval(tuning.fps),
                }
            }
            None => PipelineEncoding::Jpeg {
                quality: tuning.quality,
            },
        };
        let geometry = FrameGeometry {
            width: target_width.unwrap_or(source_width),
            height: target_height.unwrap_or(source_height),
            source_width,
            source_height,
        };
        info!(
            encoder = encoding.label(),
            codec = encoding.codec(),
            bitrate_kbps = encoding.bitrate_kbps(),
            width = geometry.width,
            height = geometry.height,
            "portal stream started"
        );

        let producer_error: ProducerError = Arc::new(std::sync::Mutex::new(None));
        let target = (target_width, target_height);
        let pipeline = spawn_pipeline(&portal, tuning.fps, encoding, target, &producer_error)?;
        let mut producer = Self {
            portal,
            fps: tuning.fps,
            encoding,
            target,
            geometry,
            child: pipeline.child,
            stdout: pipeline.stdout,
            stderr_task: pipeline.stderr_task,
            producer_error,
            annexb: AnnexBStreamReader::new(),
            jpeg: JpegStreamReader::new(),
            pending: VecDeque::new(),
            waiting_for_keyframe: matches!(encoding, PipelineEncoding::H264 { .. }),
            last_start: Instant::now(),
            flush_idle: Duration::from_millis((500 / u64::from(tuning.fps.max(1))).clamp(4, 12)),
        };

        // Prove the pipeline actually produces before reporting success. A
        // pipeline that dies during negotiation must fall back to grim, and
        // that decision can only be made before anything reaches the client.
        match timeout(FIRST_FRAME_TIMEOUT, producer.pull_unit()).await {
            Ok(Ok(Some(unit))) => {
                producer.pending.push_front(unit);
                Ok(producer)
            }
            Ok(Ok(None)) | Err(_) | Ok(Err(_)) => {
                let detail = producer
                    .producer_error
                    .lock()
                    .ok()
                    .and_then(|slot| slot.clone());
                producer.kill_child().await;
                match detail {
                    Some(detail) => {
                        bail!("Portal GStreamer pipeline produced no frames: {detail}")
                    }
                    None => bail!(
                        "Portal GStreamer pipeline produced no frames (PipeWire format may be incompatible)"
                    ),
                }
            }
        }
    }

    /// Starts a fresh `gst-launch-1.0`, leaving the portal session untouched.
    ///
    /// Keeping the portal alive across the restart is the whole reason the
    /// session is owned here: reopening it would be far slower and, without a
    /// restore token, would ask the user to approve sharing all over again.
    async fn respawn(&mut self) -> anyhow::Result<()> {
        self.kill_child().await;
        let pipeline = spawn_pipeline(
            &self.portal,
            self.fps,
            self.encoding,
            self.target,
            &self.producer_error,
        )?;
        self.child = pipeline.child;
        self.stdout = pipeline.stdout;
        self.stderr_task = pipeline.stderr_task;
        self.annexb = AnnexBStreamReader::new();
        self.jpeg = JpegStreamReader::new();
        self.waiting_for_keyframe = matches!(self.encoding, PipelineEncoding::H264 { .. });
        self.last_start = Instant::now();
        Ok(())
    }

    async fn kill_child(&mut self) {
        let _ = self.child.kill().await;
        // Killing the child closes the stderr pipe, so draining it now costs
        // almost nothing and makes the producer's own diagnostic available.
        if let Some(task) = self.stderr_task.take() {
            let _ = timeout(Duration::from_millis(500), task).await;
        }
    }

    fn push_access_unit(&mut self, unit: AccessUnit) {
        if self.waiting_for_keyframe {
            if !unit.key_frame {
                return;
            }
            self.waiting_for_keyframe = false;
        }
        self.pending.push_back(EncodedUnit {
            data: unit.data,
            key_frame: unit.key_frame,
            parameter_sets: unit.parameter_sets,
            geometry: self.geometry,
        });
    }

    /// Reads until at least one unit is available, or the pipeline ends.
    async fn pull_unit(&mut self) -> anyhow::Result<Option<EncodedUnit>> {
        let is_h264 = matches!(self.encoding, PipelineEncoding::H264 { .. });
        let mut buffer = vec![0u8; 64 * 1024];
        loop {
            if let Some(unit) = self.pending.pop_front() {
                return Ok(Some(unit));
            }
            let flushed = tokio::select! {
                // Draining the pipe always wins over the idle flush, so a stall
                // in this task can never cut an access unit that has arrived.
                biased;
                read = self.stdout.read(&mut buffer) => {
                    let n = read.context("failed to read the screen encoder pipe")?;
                    if n == 0 {
                        return Ok(None);
                    }
                    if is_h264 {
                        for unit in self.annexb.push(&buffer[..n]) {
                            self.push_access_unit(unit);
                        }
                    } else {
                        for frame in self.jpeg.push(&buffer[..n]) {
                            self.pending.push_back(EncodedUnit {
                                data: frame,
                                key_frame: true,
                                parameter_sets: None,
                                geometry: self.geometry,
                            });
                        }
                    }
                    None
                }
                _ = tokio::time::sleep(self.flush_idle),
                    if is_h264 && self.annexb.has_pending_picture() =>
                {
                    self.annexb.flush_pending()
                }
            };
            if let Some(unit) = flushed {
                self.push_access_unit(unit);
            }
        }
    }
}

#[async_trait]
impl FrameProducer for PortalProducer {
    fn encoding(&self) -> StreamEncoding {
        match self.encoding {
            PipelineEncoding::H264 { .. } => StreamEncoding::H264,
            PipelineEncoding::Jpeg { .. } => StreamEncoding::Jpeg,
        }
    }

    async fn next_unit(&mut self) -> anyhow::Result<Option<EncodedUnit>> {
        self.pull_unit().await
    }

    async fn request_key_frame(&mut self) -> anyhow::Result<()> {
        // Every JPEG frame is already a keyframe.
        if matches!(self.encoding, PipelineEncoding::Jpeg { .. }) {
            return Ok(());
        }
        // Driving GStreamer through gst-launch-1.0 leaves no way to push a
        // force-key-unit event into a running pipeline, so the keyframe is
        // served by respawning the encoder: a fresh pipeline always opens on an
        // IDR with its parameter sets.
        if self.last_start.elapsed() < KEYFRAME_RESTART_MIN_INTERVAL {
            debug!("keyframe request ignored; the encoder just restarted");
            return Ok(());
        }
        info!("restarting encoder pipeline to serve a keyframe request");
        self.respawn().await
    }

    async fn shutdown(mut self: Box<Self>) {
        self.kill_child().await;
    }
}

/// Whole-monitor screenshots through `grim`, one per frame.
///
/// The slow path, used when the portal pipeline is unavailable or broken. Every
/// frame is an independent JPEG, so it needs no keyframe machinery at all.
struct GrimProducer {
    source: ScreenSource,
    quality: u8,
    scale: f64,
    interval: Duration,
    geometry: FrameGeometry,
    next_frame: Instant,
}

impl GrimProducer {
    fn start(source: ScreenSource, tuning: StreamTuning) -> Self {
        let requested = capture_scale(
            source.width,
            source.height,
            tuning.max_width,
            tuning.max_height,
        );
        let scale = requested.min(GRIM_MAX_SCALE);
        let quality = tuning.quality.min(GRIM_MAX_QUALITY);
        info!(
            source_id = %source.id,
            fps = tuning.fps,
            quality,
            scale,
            requested_scale = requested,
            "grim stream started"
        );
        Self {
            geometry: FrameGeometry::unscaled(source.width, source.height),
            source,
            quality,
            scale,
            interval: Duration::from_secs_f64(1.0 / f64::from(tuning.fps.max(1))),
            next_frame: Instant::now(),
        }
    }
}

#[async_trait]
impl FrameProducer for GrimProducer {
    fn encoding(&self) -> StreamEncoding {
        StreamEncoding::Jpeg
    }

    async fn next_unit(&mut self) -> anyhow::Result<Option<EncodedUnit>> {
        tokio::time::sleep_until(self.next_frame).await;
        let jpeg = capture_grim_frame(&self.source, self.quality, self.scale).await?;
        // Paced from now rather than from the last deadline: a capture slower
        // than the frame interval must not build a backlog of missed ticks.
        self.next_frame = Instant::now() + self.interval;
        Ok(Some(EncodedUnit {
            data: jpeg,
            key_frame: true,
            parameter_sets: None,
            geometry: self.geometry,
        }))
    }

    async fn request_key_frame(&mut self) -> anyhow::Result<()> {
        Ok(())
    }

    async fn shutdown(self: Box<Self>) {}
}

/// `ffmpeg -f x11grab`, emitting MJPEG on a pipe.
struct X11Producer {
    child: Child,
    stdout: ChildStdout,
    reader: JpegStreamReader,
    pending: VecDeque<Vec<u8>>,
    geometry: FrameGeometry,
}

impl X11Producer {
    async fn start(source: ScreenSource, tuning: StreamTuning) -> anyhow::Result<Self> {
        let display = std::env::var("DISPLAY").unwrap_or_else(|_| ":1".into());
        let (target_width, target_height) = target_dimensions(
            source.width,
            source.height,
            tuning.max_width,
            tuning.max_height,
        );
        let capture_width = target_width.unwrap_or(source.width);
        let capture_height = target_height.unwrap_or(source.height);
        info!(
            source_id = %source.id,
            fps = tuning.fps,
            quality = tuning.quality,
            capture_width,
            capture_height,
            "x11 stream starting with ffmpeg"
        );

        let input_spec = format!("{}.0+{},{}", display, source.x, source.y);
        let mut command = Command::new("ffmpeg");
        command.args([
            "-y",
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "x11grab",
            "-framerate",
            &tuning.fps.to_string(),
            "-video_size",
            &format!("{capture_width}x{capture_height}"),
            "-i",
            &input_spec,
        ]);
        if target_width.is_some() || target_height.is_some() {
            command.args(["-vf", &format!("scale={capture_width}:{capture_height}")]);
        }
        command
            .args([
                "-c:v",
                "mjpeg",
                "-q:v",
                &tuning.quality.to_string(),
                "-f",
                "mjpeg",
                "pipe:1",
            ])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);

        let mut child = command
            .spawn()
            .context("failed to spawn ffmpeg for X11 capture")?;
        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(log_child_stderr(
                "x11".to_string(),
                "ffmpeg",
                stderr,
                Arc::new(std::sync::Mutex::new(None)),
            ));
        }
        let stdout = child.stdout.take().context("ffmpeg stdout unavailable")?;
        Ok(Self {
            child,
            stdout,
            reader: JpegStreamReader::new(),
            pending: VecDeque::new(),
            geometry: FrameGeometry::unscaled(source.width, source.height),
        })
    }
}

#[async_trait]
impl FrameProducer for X11Producer {
    fn encoding(&self) -> StreamEncoding {
        StreamEncoding::Jpeg
    }

    async fn next_unit(&mut self) -> anyhow::Result<Option<EncodedUnit>> {
        let mut buffer = vec![0u8; 32 * 1024];
        loop {
            if let Some(frame) = self.pending.pop_front() {
                return Ok(Some(EncodedUnit {
                    data: frame,
                    key_frame: true,
                    parameter_sets: None,
                    geometry: self.geometry,
                }));
            }
            let n = self
                .stdout
                .read(&mut buffer)
                .await
                .context("failed to read the ffmpeg pipe")?;
            if n == 0 {
                return Ok(None);
            }
            self.pending.extend(self.reader.push(&buffer[..n]));
        }
    }

    async fn request_key_frame(&mut self) -> anyhow::Result<()> {
        Ok(())
    }

    async fn shutdown(mut self: Box<Self>) {
        let _ = self.child.kill().await;
    }
}

/// Producer stderr is both logged and kept, so a pipeline that dies before its
/// first frame can report why instead of silently degrading to the fallback
/// with no explanation anywhere.
type ProducerError = Arc<std::sync::Mutex<Option<String>>>;

/// A running encoder pipeline and the pipes attached to it.
struct SpawnedPipeline {
    child: Child,
    stdout: ChildStdout,
    stderr_task: Option<tokio::task::JoinHandle<()>>,
}

fn spawn_pipeline(
    portal: &PortalScreenCastSession,
    fps: u32,
    encoding: PipelineEncoding,
    target: (Option<u32>, Option<u32>),
    producer_error: &ProducerError,
) -> anyhow::Result<SpawnedPipeline> {
    let mut child = spawn_gstreamer_pipewire(portal, fps, encoding, target.0, target.1)?;
    let stderr_task = child.stderr.take().map(|stderr| {
        tokio::spawn(log_child_stderr(
            "portal".to_string(),
            "gstreamer",
            stderr,
            producer_error.clone(),
        ))
    });
    let stdout = child
        .stdout
        .take()
        .context("GStreamer stdout unavailable")?;
    Ok(SpawnedPipeline {
        child,
        stdout,
        stderr_task,
    })
}
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
    /// Where the shared stream sits on the desktop. Read from the portal but
    /// not applied to pointer coordinates: portal streams already arrive in
    /// desktop space, which is why `source_origin` reports none for them.
    #[allow(dead_code, reason = "reported by the portal; kept for diagnostics")]
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

        // SelectSources is not optional, ever. The restore token belongs in *its* options, not
        // in CreateSession's, and skipping the call because a token exists is what made every
        // restore fail with "Sources not selected" and fall back to showing the picker again.
        let mut select_options = HashMap::<&str, OwnedValue>::new();
        select_options.insert("types", Value::from(1u32 | 2u32).try_into()?);
        select_options.insert("multiple", Value::from(false).try_into()?);
        select_options.insert("cursor_mode", Value::from(2u32).try_into()?);
        select_options.insert("persist_mode", Value::from(2u32).try_into()?);
        if let Some(ref token) = restore_token {
            select_options.insert("restore_token", Value::from(token.as_str()).try_into()?);
            info!("portal restore_token provided; the picker should stay closed");
        }
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
        let new_restore_token: Option<String> = None;

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
        .stderr(std::process::Stdio::piped())
        // The producer is normally killed explicitly after the pump returns. This
        // covers the path that cannot run that code: a cancelled task drops the
        // handle without awaiting, which used to leave an encoder capturing the
        // screen for a session nobody owns any more.
        .kill_on_drop(true);
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
