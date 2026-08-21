//! Desktop audio for the screen stream.
//!
//! Captures what the speakers are playing — not what the microphone hears —
//! encodes it to Opus and interleaves the packets with the video envelopes on
//! the same stream socket. Where the samples come from is the platform's
//! problem, reached through [`AudioBackend`]; everything from the packet
//! onwards is here and identical on every host.
//!
//! Audio is strictly optional: every failure in here is logged at `error` and
//! ends the audio task alone. The video producer runs in its own task and never
//! observes an audio error, which is a hard requirement of this module.

use crate::{
    backend::{AudioBackend, AudioProducer},
    stream::StreamSocket,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{sync::oneshot, task::JoinHandle, time::timeout};
use tracing::{debug, error, info, warn};

/// Codec name written into every audio envelope header.
pub const AUDIO_CODEC: &str = "opus";
/// Opus only ever runs at 48 kHz internally, so the wire format is fixed.
pub const AUDIO_SAMPLE_RATE: u32 = 48_000;
pub const AUDIO_CHANNELS: u16 = 2;
/// libopus reports a 6.5 ms encoder lookahead at 48 kHz (312 samples). Raw Opus
/// packets carry no pre-skip of their own, so the value travels in the header
/// and the phone folds it into the `csd-1` buffer `MediaCodec` insists on.
pub const OPUS_PRE_SKIP_SAMPLES: u16 = 312;

const DEFAULT_BITRATE_KBPS: u32 = 96;
const DEFAULT_FRAME_MS: u32 = 20;
const MIN_BITRATE_KBPS: u32 = 32;
const MAX_BITRATE_KBPS: u32 = 256;

/// How long an audio envelope waits for the socket before it is discarded. The
/// video producer holds the same lock for the length of one frame write, so
/// audio yields instead of queueing: a late packet is worth less than the video
/// frame it would delay.
const AUDIO_LOCK_TIMEOUT: Duration = Duration::from_millis(150);
/// Once the lock is held the envelope is written whole — a partial write would
/// desynchronise the framing for the rest of the session.
const AUDIO_WRITE_TIMEOUT: Duration = Duration::from_secs(5);
/// Packets a producer should keep buffered before discarding the oldest. More
/// than this means the socket is draining slower than real time.
pub const MAX_PACKETS_PER_BATCH: usize = 8;
const RESTART_ATTEMPTS: u32 = 3;
const RESTART_BACKOFF: Duration = Duration::from_millis(750);

/// Encoder knobs a client may negotiate per session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AudioStreamOptions {
    pub bitrate_kbps: u32,
    pub frame_ms: u32,
}

impl Default for AudioStreamOptions {
    fn default() -> Self {
        Self {
            bitrate_kbps: DEFAULT_BITRATE_KBPS,
            frame_ms: DEFAULT_FRAME_MS,
        }
    }
}

impl AudioStreamOptions {
    /// Clamps client input. `frame_ms` snaps to the two short Opus frame sizes:
    /// anything longer trades latency for a bitrate saving this stream does not
    /// need.
    pub fn new(bitrate_kbps: Option<u32>, frame_ms: Option<u32>) -> Self {
        Self {
            bitrate_kbps: bitrate_kbps
                .unwrap_or(DEFAULT_BITRATE_KBPS)
                .clamp(MIN_BITRATE_KBPS, MAX_BITRATE_KBPS),
            frame_ms: match frame_ms.unwrap_or(DEFAULT_FRAME_MS) {
                value if value <= 15 => 10,
                _ => 20,
            },
        }
    }

    pub fn format(self) -> AudioFormat {
        AudioFormat {
            sample_rate: AUDIO_SAMPLE_RATE,
            channels: AUDIO_CHANNELS,
            frame_ms: self.frame_ms,
            pre_skip: OPUS_PRE_SKIP_SAMPLES,
        }
    }
}

/// Everything the phone needs to build its Opus decoder, carried on every
/// envelope so a client that joins late never waits for an init packet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AudioFormat {
    pub sample_rate: u32,
    pub channels: u16,
    pub frame_ms: u32,
    pub pre_skip: u16,
}

/// What the control channel reports back about a session's audio.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AudioStreamStatus {
    pub running: bool,
    pub muted: bool,
    pub codec: String,
    pub sample_rate: u32,
    pub channels: u16,
    pub frame_ms: u32,
    pub bitrate_kbps: u32,
    pub monitor_source: Option<String>,
    pub packets_sent: u64,
    pub packets_dropped: u64,
    pub reason: Option<String>,
}

#[derive(Debug, Default)]
struct AudioMetrics {
    sent: AtomicU64,
    dropped: AtomicU64,
}

/// A running desktop-audio producer bound to one screen stream socket.
///
/// Dropping the handle stops the capture: the stop signal is sent from [`Drop`],
/// so a session that disappears from the registry never leaves a capture running.
#[derive(Debug)]
pub struct DesktopAudioStream {
    options: AudioStreamOptions,
    monitor: Arc<std::sync::Mutex<Option<String>>>,
    muted: Arc<AtomicBool>,
    metrics: Arc<AudioMetrics>,
    failure: Arc<std::sync::Mutex<Option<String>>>,
    stop: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<()>>,
}

impl DesktopAudioStream {
    pub fn spawn(
        session_id: String,
        socket: Arc<StreamSocket>,
        backend: Arc<dyn AudioBackend>,
        options: AudioStreamOptions,
    ) -> Self {
        let (stop_tx, stop_rx) = oneshot::channel();
        let muted = Arc::new(AtomicBool::new(false));
        let metrics = Arc::new(AudioMetrics::default());
        let monitor = Arc::new(std::sync::Mutex::new(None));
        let failure = Arc::new(std::sync::Mutex::new(None));
        let task = tokio::spawn(run_audio_stream(
            session_id,
            socket,
            backend,
            options,
            muted.clone(),
            metrics.clone(),
            monitor.clone(),
            failure.clone(),
            stop_rx,
        ));
        Self {
            options,
            monitor,
            muted,
            metrics,
            failure,
            stop: Some(stop_tx),
            task: Some(task),
        }
    }

    pub fn set_muted(&self, muted: bool) {
        self.muted.store(muted, Ordering::Relaxed);
    }

    pub fn is_muted(&self) -> bool {
        self.muted.load(Ordering::Relaxed)
    }

    /// True while the capture task is still alive.
    pub fn is_running(&self) -> bool {
        self.task.as_ref().is_some_and(|task| !task.is_finished())
    }

    pub fn status(&self) -> AudioStreamStatus {
        AudioStreamStatus {
            running: self.is_running(),
            muted: self.is_muted(),
            codec: AUDIO_CODEC.into(),
            sample_rate: AUDIO_SAMPLE_RATE,
            channels: AUDIO_CHANNELS,
            frame_ms: self.options.frame_ms,
            bitrate_kbps: self.options.bitrate_kbps,
            monitor_source: self.monitor.lock().ok().and_then(|slot| slot.clone()),
            packets_sent: self.metrics.sent.load(Ordering::Relaxed),
            packets_dropped: self.metrics.dropped.load(Ordering::Relaxed),
            reason: self.failure.lock().ok().and_then(|slot| slot.clone()),
        }
    }

    pub async fn stop(mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(task) = self.task.take()
            && timeout(Duration::from_secs(2), task).await.is_err()
        {
            warn!("desktop audio task did not stop within two seconds");
        }
    }
}

impl Drop for DesktopAudioStream {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }
}

/// Status reported for a session that has no audio producer attached.
pub fn idle_status(options: AudioStreamOptions, reason: Option<String>) -> AudioStreamStatus {
    AudioStreamStatus {
        running: false,
        muted: false,
        codec: AUDIO_CODEC.into(),
        sample_rate: AUDIO_SAMPLE_RATE,
        channels: AUDIO_CHANNELS,
        frame_ms: options.frame_ms,
        bitrate_kbps: options.bitrate_kbps,
        monitor_source: None,
        packets_sent: 0,
        packets_dropped: 0,
        reason,
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_audio_stream(
    session_id: String,
    socket: Arc<StreamSocket>,
    backend: Arc<dyn AudioBackend>,
    options: AudioStreamOptions,
    muted: Arc<AtomicBool>,
    metrics: Arc<AudioMetrics>,
    monitor_slot: Arc<std::sync::Mutex<Option<String>>>,
    failure: Arc<std::sync::Mutex<Option<String>>>,
    mut stop_rx: oneshot::Receiver<()>,
) {
    // The capture is not started until the video producer has opened the stream.
    // Portal approval can take a minute, and an encoder spun up before it would
    // hold the audio device the whole time producing packets nobody may
    // receive — and would keep holding it if the approval never arrives.
    if !wait_for_stream_open(&socket, &mut stop_rx).await {
        debug!(%session_id, "desktop audio cancelled before the video stream opened");
        return;
    }

    let mut seq = 0u64;
    for attempt in 1..=RESTART_ATTEMPTS {
        if stop_rx.try_recv().is_ok() {
            break;
        }
        // The device is re-resolved on every attempt: the user may switch
        // output between attempts, and a stale one would capture silence.
        let producer = match backend.open(options).await {
            Ok(producer) => producer,
            Err(err) => {
                record_failure(&failure, format!("{err:#}"));
                error!(
                    %session_id,
                    error = %format!("{err:#}"),
                    "desktop audio capture unavailable; the screen stream keeps running without sound"
                );
                return;
            }
        };
        if let Ok(mut slot) = monitor_slot.lock() {
            *slot = producer.source_label();
        }
        info!(
            %session_id,
            attempt,
            backend = backend.name(),
            source = producer.source_label().as_deref().unwrap_or("unknown"),
            bitrate_kbps = options.bitrate_kbps,
            frame_ms = options.frame_ms,
            "desktop audio capture starting"
        );

        match pump_audio_stream(
            &session_id,
            &socket,
            producer,
            options,
            &muted,
            &metrics,
            &mut seq,
            &mut stop_rx,
        )
        .await
        {
            Ok(AudioOutcome::Stopped) => {
                debug!(%session_id, "desktop audio capture stopped on request");
                return;
            }
            Ok(AudioOutcome::SocketClosed) => {
                debug!(%session_id, "desktop audio capture stopped because the stream socket closed");
                return;
            }
            Ok(AudioOutcome::ProducerEnded) | Err(_) if attempt >= RESTART_ATTEMPTS => {
                let detail = failure
                    .lock()
                    .ok()
                    .and_then(|slot| slot.clone())
                    .unwrap_or_else(|| "the encoder stopped producing packets".into());
                error!(
                    %session_id,
                    attempts = attempt,
                    error = %detail,
                    "desktop audio capture gave up; the screen stream keeps running without sound"
                );
                return;
            }
            Ok(AudioOutcome::ProducerEnded) => {
                warn!(%session_id, attempt, "desktop audio producer ended; restarting");
            }
            Err(err) => {
                let detail = format!("{err:#}");
                record_failure(&failure, detail.clone());
                warn!(%session_id, attempt, error = %detail, "desktop audio capture failed; restarting");
            }
        }

        tokio::select! {
            _ = &mut stop_rx => return,
            _ = tokio::time::sleep(RESTART_BACKOFF) => {}
        }
    }
}

/// Blocks until the video producer has written the handshake line, or the stream
/// is torn down. There is no timeout on purpose: the video task owns the
/// session's lifetime, and when it gives up it drops this task's handle, which
/// fires the stop signal below.
async fn wait_for_stream_open(socket: &StreamSocket, stop_rx: &mut oneshot::Receiver<()>) -> bool {
    while !socket.handshake_sent() {
        tokio::select! {
            _ = &mut *stop_rx => return false,
            _ = tokio::time::sleep(Duration::from_millis(100)) => {}
        }
    }
    true
}

#[derive(Debug, PartialEq, Eq)]
enum AudioOutcome {
    Stopped,
    SocketClosed,
    ProducerEnded,
}

#[allow(clippy::too_many_arguments)]
async fn pump_audio_stream(
    session_id: &str,
    socket: &StreamSocket,
    mut producer: Box<dyn AudioProducer>,
    options: AudioStreamOptions,
    muted: &AtomicBool,
    metrics: &AudioMetrics,
    seq: &mut u64,
    stop_rx: &mut oneshot::Receiver<()>,
) -> anyhow::Result<AudioOutcome> {
    let format = options.format();
    let mut packets_this_second = 0u64;
    let mut bytes_this_second = 0u64;
    let mut window_start = tokio::time::Instant::now();
    let outcome = loop {
        let packet = tokio::select! {
            _ = &mut *stop_rx => break AudioOutcome::Stopped,
            next = producer.next_packet() => match next? {
                Some(packet) => packet,
                None => break AudioOutcome::ProducerEnded,
            },
        };

        // The handshake line belongs to the video producer, and a client that
        // has not read it yet cannot frame anything: audio silently waits
        // instead of writing into a stream that has not opened.
        if muted.load(Ordering::Relaxed) || !socket.handshake_sent() {
            metrics.dropped.fetch_add(1, Ordering::Relaxed);
            continue;
        }
        let header = audio_frame_header(*seq, now_millis(), format);
        match socket
            .try_write_envelope(&header, &packet, AUDIO_LOCK_TIMEOUT, AUDIO_WRITE_TIMEOUT)
            .await
        {
            Ok(true) => {
                *seq += 1;
                metrics.sent.fetch_add(1, Ordering::Relaxed);
                packets_this_second += 1;
                bytes_this_second += packet.len() as u64;
            }
            Ok(false) => {
                metrics.dropped.fetch_add(1, Ordering::Relaxed);
            }
            Err(err) => {
                debug!(%session_id, error = %format!("{err:#}"), "desktop audio socket write failed");
                producer.shutdown().await;
                return Ok(AudioOutcome::SocketClosed);
            }
        }

        let elapsed = window_start.elapsed().as_secs_f64();
        if elapsed >= 5.0 {
            debug!(
                %session_id,
                packets = packets_this_second,
                kbps = (bytes_this_second as f64 * 8.0 / elapsed / 1000.0).round(),
                "desktop audio throughput"
            );
            packets_this_second = 0;
            bytes_this_second = 0;
            window_start = tokio::time::Instant::now();
        }
    };

    producer.shutdown().await;
    Ok(outcome)
}

fn record_failure(slot: &Arc<std::sync::Mutex<Option<String>>>, detail: String) {
    if let Ok(mut guard) = slot.lock() {
        *guard = Some(detail);
    }
}

pub fn audio_frame_header(seq: u64, timestamp_ms: u128, format: AudioFormat) -> String {
    json!({
        "seq": seq,
        "timestamp_ms": timestamp_ms,
        "codec": AUDIO_CODEC,
        "sample_rate": format.sample_rate,
        "channels": format.channels,
        "frame_ms": format.frame_ms,
        "pre_skip": format.pre_skip,
        "key_frame": false,
        "config": false
    })
    .to_string()
}

/// How many leading packets of a backlog are stale enough to discard.
///
/// A producer holding more than one batch means the socket is draining slower
/// than the encoder produces. Audio cannot "catch up" by playing faster, so the
/// backlog is cut from the front and only the freshest packets survive.
pub fn stale_packet_count(available: usize, max_batch: usize) -> usize {
    available.saturating_sub(max_batch.max(1))
}

fn now_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_header_never_claims_a_key_frame_or_a_config_payload() {
        // The phone prunes congested batches on these two flags without looking
        // at the codec, so an audio envelope setting either would drop video.
        let header = audio_frame_header(
            7,
            1_700_000_000_123,
            AudioFormat {
                sample_rate: 48_000,
                channels: 2,
                frame_ms: 20,
                pre_skip: 312,
            },
        );
        let parsed: serde_json::Value = serde_json::from_str(&header).unwrap();
        assert_eq!(parsed["seq"], 7);
        assert_eq!(parsed["timestamp_ms"], 1_700_000_000_123u64);
        assert_eq!(parsed["codec"], "opus");
        assert_eq!(parsed["sample_rate"], 48_000);
        assert_eq!(parsed["channels"], 2);
        assert_eq!(parsed["frame_ms"], 20);
        assert_eq!(parsed["pre_skip"], 312);
        assert_eq!(parsed["key_frame"], false);
        assert_eq!(parsed["config"], false);
    }

    #[test]
    fn clamps_client_supplied_encoder_options() {
        assert_eq!(
            AudioStreamOptions::new(None, None),
            AudioStreamOptions {
                bitrate_kbps: 96,
                frame_ms: 20
            }
        );
        assert_eq!(AudioStreamOptions::new(Some(1), None).bitrate_kbps, 32);
        assert_eq!(AudioStreamOptions::new(Some(999), None).bitrate_kbps, 256);
        assert_eq!(AudioStreamOptions::new(None, Some(10)).frame_ms, 10);
        assert_eq!(AudioStreamOptions::new(None, Some(2)).frame_ms, 10);
        // Anything longer than the requested 10-20 ms window snaps back to 20.
        assert_eq!(AudioStreamOptions::new(None, Some(60)).frame_ms, 20);
    }

    #[test]
    fn trims_only_the_backlog_that_exceeds_one_batch() {
        assert_eq!(stale_packet_count(1, 8), 0);
        assert_eq!(stale_packet_count(8, 8), 0);
        assert_eq!(stale_packet_count(20, 8), 12);
        assert_eq!(stale_packet_count(3, 0), 2);
    }

    #[test]
    fn the_format_carried_on_every_packet_matches_the_negotiated_options() {
        let format = AudioStreamOptions::new(Some(64), Some(10)).format();
        assert_eq!(format.sample_rate, AUDIO_SAMPLE_RATE);
        assert_eq!(format.channels, AUDIO_CHANNELS);
        assert_eq!(format.frame_ms, 10);
        assert_eq!(format.pre_skip, OPUS_PRE_SKIP_SAMPLES);
    }
}
