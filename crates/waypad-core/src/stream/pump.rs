//! Drives one [`FrameProducer`] into one client socket.
//!
//! This is the loop that used to exist twice — once for H.264 off a GStreamer
//! pipe, once for JPEG off a screenshot pipe — and platform-specific in both
//! cases. It is now written once against the producer trait, so the envelope
//! sequence behaves identically no matter which operating system produced the
//! pictures.

use crate::{
    backend::{CaptureBackend, FrameProducer, StreamEncoding},
    stream::{
        ScreenSource,
        socket::{StreamSocket, send_h264_frame, send_jpeg_frame},
        tuning::StreamTuning,
    },
};
use tokio::{
    sync::{mpsc, oneshot},
    time::Instant,
};
use tracing::{debug, info, warn};

#[derive(Debug, Default)]
pub struct StreamCounters {
    /// Counts envelopes, not pictures: a config frame consumes a seq of its own.
    pub seq: u64,
    pub frames: u64,
}

/// Streams `source` to `socket` until the client leaves or `stop_rx` fires.
pub async fn pump_stream(
    socket: &StreamSocket,
    capture: &dyn CaptureBackend,
    source: &ScreenSource,
    tuning: StreamTuning,
    session_id: &str,
    mut stop_rx: oneshot::Receiver<()>,
    mut keyframe_rx: mpsc::Receiver<()>,
) -> anyhow::Result<()> {
    let mut producer = capture.open(source, tuning).await?;
    let encoding = producer.encoding();
    info!(
        %session_id,
        backend = capture.name(),
        codec = encoding.codec(),
        fps = tuning.fps,
        "screen stream producer open"
    );
    let result = pump_producer(
        socket,
        producer.as_mut(),
        &mut stop_rx,
        &mut keyframe_rx,
        session_id,
        encoding,
        tuning.fps,
    )
    .await;
    producer.shutdown().await;
    result
}

async fn pump_producer(
    socket: &StreamSocket,
    producer: &mut dyn FrameProducer,
    stop_rx: &mut oneshot::Receiver<()>,
    keyframe_rx: &mut mpsc::Receiver<()>,
    session_id: &str,
    encoding: StreamEncoding,
    fps: u32,
) -> anyhow::Result<()> {
    let mut counters = StreamCounters::default();
    let mut keyframe_channel_open = true;
    let mut pending_keyframe = false;
    let mut frame_count = 0u64;
    let mut throughput_start = Instant::now();

    loop {
        if pending_keyframe {
            pending_keyframe = false;
            // Whether this costs a flag or a pipeline respawn is the producer's
            // problem, and so is rate limiting a client that asks repeatedly.
            producer.request_key_frame().await?;
        }

        let unit = tokio::select! {
            // Draining the producer always wins over a keyframe request, so a
            // burst of requests can never starve the picture path.
            biased;
            _ = &mut *stop_rx => return Ok(()),
            next = producer.next_unit() => match next? {
                Some(unit) => unit,
                None => {
                    warn!(%session_id, "screen stream producer finished");
                    return Ok(());
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
                continue;
            }
        };

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
