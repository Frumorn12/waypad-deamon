//! Drives one [`FrameProducer`] into one client socket.
//!
//! This is the loop that used to exist twice — once for H.264 off a GStreamer
//! pipe, once for JPEG off a screenshot pipe — and platform-specific in both
//! cases. It is now written once against the producer trait, so the envelope
//! sequence, the keyframe gate, and the reopen-on-keyframe dance behave
//! identically no matter which operating system produced the pictures.

use crate::{
    backend::{CaptureBackend, FrameProducer, KeyFrameOutcome, StreamEncoding},
    stream::{
        ScreenSource,
        socket::{StreamSocket, send_h264_frame, send_jpeg_frame},
        tuning::StreamTuning,
    },
};
use std::time::Duration;
use tokio::{
    sync::{mpsc, oneshot},
    time::Instant,
};
use tracing::{debug, info, warn};

/// Encoder restarts are cheap but not free, so a client that asks repeatedly
/// cannot make the pipeline thrash.
const KEYFRAME_RESTART_MIN_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Debug, Default)]
pub struct StreamCounters {
    /// Counts envelopes, not pictures: a config frame consumes a seq of its own.
    pub seq: u64,
    pub frames: u64,
}

#[derive(Debug, PartialEq, Eq)]
enum PumpOutcome {
    Finished,
    /// The producer could not force a keyframe, so it must be replaced by a
    /// fresh one whose first picture is an IDR by construction.
    ReopenForKeyFrame,
}

/// Streams `source` to `socket` until the client leaves or `stop_rx` fires.
///
/// Reopens the producer whenever a keyframe is requested and the platform
/// cannot inject one in place. The counters survive a reopen so the client sees
/// one continuous envelope sequence across the gap.
pub async fn pump_stream(
    socket: &StreamSocket,
    capture: &dyn CaptureBackend,
    source: &ScreenSource,
    tuning: StreamTuning,
    session_id: &str,
    mut stop_rx: oneshot::Receiver<()>,
    mut keyframe_rx: mpsc::Receiver<()>,
) -> anyhow::Result<()> {
    let mut counters = StreamCounters::default();
    loop {
        let mut producer = capture.open(source, tuning).await?;
        info!(
            %session_id,
            backend = capture.name(),
            codec = producer.encoding().codec(),
            fps = tuning.fps,
            "screen stream producer open"
        );
        let outcome = pump_producer(
            socket,
            producer.as_mut(),
            &mut stop_rx,
            &mut keyframe_rx,
            &mut counters,
            session_id,
            tuning.fps,
        )
        .await;
        producer.shutdown().await;
        match outcome? {
            PumpOutcome::Finished => return Ok(()),
            PumpOutcome::ReopenForKeyFrame => {
                debug!(%session_id, "reopening encoder to serve a keyframe request");
            }
        }
    }
}

async fn pump_producer(
    socket: &StreamSocket,
    producer: &mut dyn FrameProducer,
    stop_rx: &mut oneshot::Receiver<()>,
    keyframe_rx: &mut mpsc::Receiver<()>,
    counters: &mut StreamCounters,
    session_id: &str,
    fps: u32,
) -> anyhow::Result<PumpOutcome> {
    let encoding = producer.encoding();
    let started = Instant::now();
    // A decoder that joins on a P-frame shows nothing until the next IDR, so
    // nothing is forwarded before the first keyframe of the fresh producer.
    // JPEG frames are all keyframes, so the gate opens on the first one.
    let mut waiting_for_keyframe = encoding == StreamEncoding::H264;
    let mut keyframe_channel_open = true;
    let mut pending_keyframe = false;
    let mut frame_count = 0u64;
    let mut throughput_start = Instant::now();

    loop {
        if pending_keyframe {
            pending_keyframe = false;
            match producer.request_key_frame().await? {
                KeyFrameOutcome::Delivered => {
                    debug!(%session_id, "encoder forced a keyframe in place");
                }
                KeyFrameOutcome::RequiresReopen => {
                    if started.elapsed() >= KEYFRAME_RESTART_MIN_INTERVAL {
                        return Ok(PumpOutcome::ReopenForKeyFrame);
                    }
                    debug!(%session_id, "keyframe request ignored; the encoder just restarted");
                }
            }
        }

        let unit = tokio::select! {
            // Draining the producer always wins over a keyframe request, so a
            // burst of requests can never starve the picture path.
            biased;
            _ = &mut *stop_rx => return Ok(PumpOutcome::Finished),
            next = producer.next_unit() => match next? {
                Some(unit) => Some(unit),
                None => {
                    warn!(%session_id, "screen stream producer finished");
                    return Ok(PumpOutcome::Finished);
                }
            },
            request = keyframe_rx.recv(), if keyframe_channel_open => {
                if request.is_none() {
                    // Sender dropped: the session is being torn down and
                    // stop_rx is about to fire.
                    keyframe_channel_open = false;
                } else {
                    pending_keyframe = true;
                }
                None
            }
        };

        let Some(unit) = unit else {
            continue;
        };

        if waiting_for_keyframe {
            if !unit.key_frame {
                continue;
            }
            waiting_for_keyframe = false;
        }

        // Written lazily, once something has actually been encoded: a platform
        // may still fall back to another codec while the socket is untouched.
        socket.send_magic(encoding.magic()).await?;

        match encoding {
            StreamEncoding::H264 => {
                if let Some(parameter_sets) = unit.parameter_sets.as_deref() {
                    send_h264_frame(
                        socket,
                        counters.seq,
                        unit.geometry,
                        parameter_sets,
                        false,
                        true,
                    )
                    .await?;
                    counters.seq += 1;
                    counters.frames += 1;
                }
                send_h264_frame(
                    socket,
                    counters.seq,
                    unit.geometry,
                    &unit.data,
                    unit.key_frame,
                    false,
                )
                .await?;
            }
            StreamEncoding::Jpeg => {
                // The JPEG header reports the desktop size, not the encoded
                // size: the client only ever maps touches in desktop space.
                send_jpeg_frame(
                    socket,
                    counters.seq,
                    unit.geometry.source_width,
                    unit.geometry.source_height,
                    &unit.data,
                )
                .await?;
            }
        }
        counters.seq += 1;
        counters.frames += 1;
        frame_count += 1;

        let elapsed = throughput_start.elapsed().as_secs_f64();
        if elapsed >= 2.0 {
            let measured = frame_count as f64 / elapsed;
            debug!(
                %session_id,
                codec = encoding.codec(),
                fps_measured = measured,
                fps_target = fps,
                frames = frame_count,
                "screen stream throughput"
            );
            frame_count = 0;
            throughput_start = Instant::now();
        }
    }
}
