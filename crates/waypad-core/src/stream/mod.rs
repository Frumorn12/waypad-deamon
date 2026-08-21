//! Screen stream sessions: negotiation, client attach, and lifetime.
//!
//! The session state machine, the envelope framing, and the pump are the same
//! on every host. What differs is only where the encoded pictures come from,
//! which arrives through [`CaptureBackend`](crate::backend::CaptureBackend).

pub mod annexb;
pub mod jpeg;
pub mod pump;
pub mod socket;
pub mod tuning;

pub use annexb::{AccessUnit, AnnexBStreamReader};
pub use jpeg::JpegStreamReader;
pub use socket::{FrameGeometry, StreamSocket, frame_envelope, is_client_disconnect};
pub use tuning::{
    StreamTuning, capture_scale, even_dimension, keyframe_interval, resolve_bitrate_kbps,
    target_dimensions,
};

use crate::{
    audio::{AudioStreamOptions, AudioStreamStatus, DesktopAudioStream},
    backend::{AudioBackend, CaptureBackend, InputBackend},
    capability::Capabilities,
    state::StatePaths,
};
use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::{
    net::TcpStream,
    sync::{Mutex, RwLock, mpsc, oneshot},
    task::JoinHandle,
    time::timeout,
};
use tracing::{debug, info, warn};
use uuid::Uuid;

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
    /// Whether desktop audio rides along on the same socket. Absent means yes,
    /// so a client that never learned about audio still gets it.
    pub audio: Option<bool>,
    pub audio_bitrate_kbps: Option<u32>,
    pub audio_frame_ms: Option<u32>,
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
    /// Why the preferred capture pipeline last fell back to a slower one.
    /// Surfaced so a broken fast path cannot sit unnoticed behind a fallback
    /// that merely looks slow.
    pub portal_last_error: Option<String>,
    /// What the session will do about desktop audio. `running` only turns true
    /// once a client attaches, because the audio producer is bound to the
    /// stream socket.
    pub audio: AudioStreamStatus,
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
    audio: bool,
    audio_options: AudioStreamOptions,
}

#[derive(Debug)]
struct RunningStream {
    stop: oneshot::Sender<()>,
    keyframe: mpsc::Sender<()>,
    task: JoinHandle<()>,
    /// Kept so audio can be started, stopped, or muted after the fact without
    /// disturbing the video producer already writing to it.
    socket: Arc<StreamSocket>,
    audio: Option<DesktopAudioStream>,
    audio_options: AudioStreamOptions,
}

pub struct ScreenManager {
    capabilities: Arc<RwLock<Capabilities>>,
    capture: Arc<dyn CaptureBackend>,
    audio_backend: Arc<dyn AudioBackend>,
    stream_port: u16,
    sessions: Arc<Mutex<HashMap<String, StreamSession>>>,
    paths: Arc<StatePaths>,
}

impl std::fmt::Debug for ScreenManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScreenManager")
            .field("capture", &self.capture.name())
            .field("audio_backend", &self.audio_backend.name())
            .field("stream_port", &self.stream_port)
            .finish_non_exhaustive()
    }
}

impl ScreenManager {
    pub fn new(
        capabilities: Arc<RwLock<Capabilities>>,
        capture: Arc<dyn CaptureBackend>,
        audio_backend: Arc<dyn AudioBackend>,
        stream_port: u16,
        paths: StatePaths,
    ) -> Self {
        Self {
            capabilities,
            capture,
            audio_backend,
            stream_port,
            sessions: Arc::new(Mutex::new(HashMap::new())),
            paths: Arc::new(paths),
        }
    }

    pub async fn list_sources(&self) -> anyhow::Result<Vec<ScreenSource>> {
        let sources = self.capture.list_sources().await?;
        if sources.is_empty() {
            bail!(
                "{}",
                self.capabilities
                    .read()
                    .await
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
        let tuning = StreamTuning::resolve(
            options.max_fps,
            options.jpeg_quality,
            options.bitrate_kbps,
            options.max_width,
            options.max_height,
        );
        let session_id = Uuid::new_v4().to_string();
        let token = Uuid::new_v4().to_string();
        let audio_options =
            AudioStreamOptions::new(options.audio_bitrate_kbps, options.audio_frame_ms);
        let audio_capability = self.capabilities.read().await.audio_capture.clone();
        // Audio is opt-out rather than opt-in: it costs a fraction of the video
        // bandwidth and a client that never heard of it still gets sound.
        let audio_requested = options.audio.unwrap_or(true) && audio_capability.supported;
        let audio_reason = if options.audio == Some(false) {
            Some("Desktop audio was disabled by the client for this session".into())
        } else {
            audio_capability.reason.clone()
        };
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
        // Advisory only: the handshake line the producer writes is what
        // actually decides how payloads must be decoded, and the producer may
        // still fall back before it writes one.
        let codec = self.capture.announced_codec(&source).await;

        if let Err(err) = crate::state::save_preferred_source(&self.paths, &source.id) {
            warn!(%err, source_id = %source.id, "failed to save preferred source");
        }
        self.sessions.lock().await.insert(
            session_id.clone(),
            StreamSession::Pending(PendingStream {
                token: token.clone(),
                source: source.clone(),
                tuning,
                audio: audio_requested,
                audio_options,
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
            %codec,
            max_width = ?tuning.max_width,
            max_height = ?tuning.max_height,
            audio = audio_requested,
            "screen stream session pending client attach"
        );
        Ok(StreamStartResponse {
            session_id,
            stream_port: self.stream_port,
            token,
            codec,
            transport: "waypad-control-port-stream-v2".into(),
            source,
            actual_fps: tuning.fps,
            actual_quality: tuning.quality,
            actual_bitrate_kbps: bitrate_kbps,
            portal_last_error: self.capture.last_error().await,
            audio: crate::audio::idle_status(audio_options, audio_reason),
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

    pub async fn attach_stream_client(&self, token: &str, socket: TcpStream) -> anyhow::Result<()> {
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
        let (stop_tx, stop_rx) = oneshot::channel();
        // Depth 1: repeated keyframe requests before the encoder reopens are
        // the same request, so they coalesce instead of queueing restarts.
        let (keyframe_tx, keyframe_rx) = mpsc::channel(1);
        // The handshake line names the codec, and the codec is only known once a
        // producer actually emits a frame, so it is written lazily.
        let socket = Arc::new(StreamSocket::new(socket));

        let task_sessions = self.sessions.clone();
        let task_session = session_id.clone();
        let task_capture = self.capture.clone();
        let task_socket = socket.clone();
        let source = pending.source.clone();
        let tuning = pending.tuning;

        // The registry lock is taken before the producer is spawned and held
        // until the session is registered. A pipeline that fails instantly
        // would otherwise deregister a session that has not been registered
        // yet, and the entry inserted afterwards would never be cleaned up: it
        // owns the audio producer and the socket, so the encoder would keep
        // capturing for a stream that no longer exists.
        let mut sessions = self.sessions.lock().await;
        let task = tokio::spawn(async move {
            let result = pump::pump_stream(
                &task_socket,
                task_capture.as_ref(),
                &source,
                tuning,
                &task_session,
                stop_rx,
                keyframe_rx,
            )
            .await;
            if let Err(err) = result {
                if is_client_disconnect(&err) {
                    info!(session_id = %task_session, %err, "screen stream client disconnected");
                } else {
                    warn!(session_id = %task_session, %err, "screen stream stopped with error");
                }
            }
            // Dropping the registry entry also drops the audio handle, which
            // signals its task to stop and reaps the encoder.
            task_sessions.lock().await.remove(&task_session);
            debug!(session_id = %task_session, "screen stream session removed from registry");
        });
        // Audio is spawned in its own task so a failing pipeline can only ever
        // silence itself: the video producer above never observes an audio
        // error.
        let audio = pending.audio.then(|| {
            DesktopAudioStream::spawn(
                session_id.clone(),
                socket.clone(),
                self.audio_backend.clone(),
                pending.audio_options,
            )
        });
        sessions.insert(
            session_id,
            StreamSession::Running(RunningStream {
                stop: stop_tx,
                keyframe: keyframe_tx,
                task,
                socket,
                audio,
                audio_options: pending.audio_options,
            }),
        );
        drop(sessions);
        Ok(())
    }

    /// Starts desktop audio on a session that is already streaming video, or
    /// reports the status of the producer that is already running.
    pub async fn start_audio(
        &self,
        session_id: &str,
        options: Option<AudioStreamOptions>,
    ) -> anyhow::Result<AudioStreamStatus> {
        let supported = self.capabilities.read().await.audio_capture.clone();
        if !supported.supported {
            bail!(
                "{}",
                supported
                    .reason
                    .unwrap_or_else(|| "Desktop audio capture is unavailable on this host".into())
            );
        }
        let mut sessions = self.sessions.lock().await;
        match sessions.get_mut(session_id) {
            Some(StreamSession::Running(running)) => {
                if let Some(audio) = running.audio.as_ref()
                    && audio.is_running()
                {
                    audio.set_muted(false);
                    return Ok(audio.status());
                }
                let options = options.unwrap_or(running.audio_options);
                running.audio_options = options;
                let audio = DesktopAudioStream::spawn(
                    session_id.to_string(),
                    running.socket.clone(),
                    self.audio_backend.clone(),
                    options,
                );
                let status = audio.status();
                running.audio = Some(audio);
                info!(%session_id, "desktop audio started for screen stream session");
                Ok(status)
            }
            // The producer is bound to the socket, so it can only start once a
            // client attaches; remember the request until then.
            Some(StreamSession::Pending(pending)) => {
                pending.audio = true;
                if let Some(options) = options {
                    pending.audio_options = options;
                }
                Ok(crate::audio::idle_status(
                    pending.audio_options,
                    Some("Desktop audio starts as soon as the stream client attaches".into()),
                ))
            }
            None => bail!("Unknown or expired screen stream session"),
        }
    }

    pub async fn stop_audio(&self, session_id: &str) -> anyhow::Result<AudioStreamStatus> {
        let mut sessions = self.sessions.lock().await;
        match sessions.get_mut(session_id) {
            Some(StreamSession::Running(running)) => {
                let options = running.audio_options;
                match running.audio.take() {
                    Some(audio) => {
                        audio.stop().await;
                        info!(%session_id, "desktop audio stopped for screen stream session");
                    }
                    None => debug!(%session_id, "desktop audio stop ignored; no producer running"),
                }
                Ok(crate::audio::idle_status(options, None))
            }
            Some(StreamSession::Pending(pending)) => {
                pending.audio = false;
                Ok(crate::audio::idle_status(pending.audio_options, None))
            }
            None => bail!("Unknown or expired screen stream session"),
        }
    }

    /// Mutes at the source: the encoder keeps running so unmuting is instant,
    /// but no envelope leaves the host, so a muted stream costs no bandwidth.
    pub async fn set_audio_mute(
        &self,
        session_id: &str,
        muted: bool,
    ) -> anyhow::Result<AudioStreamStatus> {
        let sessions = self.sessions.lock().await;
        match sessions.get(session_id) {
            Some(StreamSession::Running(running)) => match running.audio.as_ref() {
                Some(audio) => {
                    audio.set_muted(muted);
                    debug!(%session_id, muted, "desktop audio mute changed");
                    Ok(audio.status())
                }
                None => bail!("Desktop audio is not running for this session"),
            },
            Some(StreamSession::Pending(pending)) => Ok(crate::audio::idle_status(
                pending.audio_options,
                Some("Desktop audio has not started yet for this session".into()),
            )),
            None => bail!("Unknown or expired screen stream session"),
        }
    }

    pub async fn audio_status(&self, session_id: &str) -> anyhow::Result<AudioStreamStatus> {
        let sessions = self.sessions.lock().await;
        match sessions.get(session_id) {
            Some(StreamSession::Running(running)) => Ok(running
                .audio
                .as_ref()
                .map(DesktopAudioStream::status)
                .unwrap_or_else(|| crate::audio::idle_status(running.audio_options, None))),
            Some(StreamSession::Pending(pending)) => {
                Ok(crate::audio::idle_status(pending.audio_options, None))
            }
            None => bail!("Unknown or expired screen stream session"),
        }
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
                    // Full means a request is already pending; the client gets
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

    /// The desktop-space origin to add to coordinates for `source`, if its
    /// frames are not already in desktop space.
    pub async fn source_origin(&self, source: &ScreenSource) -> Option<(i32, i32)> {
        self.capture.source_origin(source)
    }

    async fn select_source(&self, requested: Option<&str>) -> anyhow::Result<ScreenSource> {
        let sources = self.list_sources().await?;
        if let Some(id) = requested.filter(|value| !value.is_empty()) {
            return sources
                .into_iter()
                .find(|source| source.id == id)
                .with_context(|| format!("Screen source not found: {id}"));
        }
        if let Some(preferred) = crate::state::load_preferred_source(&self.paths)
            && let Some(source) = sources.iter().find(|source| source.id == preferred)
        {
            info!(source_id = %preferred, "restored preferred screen source");
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

/// Moves the pointer to a coordinate expressed in a stream source's own space.
///
/// A capture backend that hands out monitor-local frames reports an origin, and
/// it is added here; one that already captures in desktop space reports none.
/// Getting this wrong is silent — the pointer simply lands on the wrong monitor
/// — so the decision belongs to the backend that produced the frames.
pub async fn pointer_move_absolute(
    input: &dyn InputBackend,
    origin: Option<(i32, i32)>,
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
    match origin {
        Some((origin_x, origin_y)) => {
            input
                .pointer_move_absolute(f64::from(origin_x) + x, f64::from(origin_y) + y)
                .await
        }
        None => input.pointer_move_absolute(x, y).await,
    }
}
