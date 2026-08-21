//! The Windows capture backend: duplication and encoding behind the core traits.
//!
//! Capture and encoding both block, and both touch COM objects with thread
//! affinity, so the whole pipeline runs on a dedicated thread and reaches the
//! async world through a channel. That keeps the runtime free of blocking calls
//! and sidesteps having to reason about whether every Media Foundation
//! interface is `Send`.

use crate::{
    capture::{DuplicatedOutput, OutputInfo, enumerate_outputs, resolve_output},
    encoder::{EncoderSettings, H264Encoder},
    nv12::{Nv12Buffer, bgra_to_nv12},
};
use anyhow::{Context, bail};
use async_trait::async_trait;
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, info, warn};
use waypad_core::{
    backend::{CaptureBackend, EncodedUnit, FrameProducer, StreamEncoding},
    stream::{
        AnnexBStreamReader, FrameGeometry, ScreenSource, StreamTuning, even_dimension,
        keyframe_interval, resolve_bitrate_kbps, target_dimensions,
    },
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CPU_ACCESS_READ, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_STAGING, ID3D11Texture2D,
};

/// How long a capture waits for the desktop to change before looping. Short
/// enough to notice a stop request promptly, long enough not to spin.
const ACQUIRE_TIMEOUT_MS: u32 = 100;

/// Encoded units held between the capture thread and the async side. Two is
/// enough to absorb a scheduling hiccup; more would only add latency, since a
/// stale frame of someone's desktop is worth nothing.
const UNIT_QUEUE_DEPTH: usize = 2;

#[derive(Debug, Default)]
pub struct WindowsCaptureBackend {
    last_error: Mutex<Option<String>>,
}

impl WindowsCaptureBackend {
    pub fn new() -> Self {
        Self::default()
    }

    fn record_error(&self, detail: String) {
        if let Ok(mut slot) = self.last_error.lock() {
            *slot = Some(detail);
        }
    }
}

#[async_trait]
impl CaptureBackend for WindowsCaptureBackend {
    fn name(&self) -> &'static str {
        "windows-dxgi"
    }

    async fn list_sources(&self) -> anyhow::Result<Vec<ScreenSource>> {
        Ok(enumerate_outputs()?
            .iter()
            .map(OutputInfo::to_source)
            .collect())
    }

    async fn open(
        &self,
        source: &ScreenSource,
        tuning: StreamTuning,
    ) -> anyhow::Result<Box<dyn FrameProducer>> {
        match DxgiProducer::start(&source.id, tuning).await {
            Ok(producer) => {
                if let Ok(mut slot) = self.last_error.lock() {
                    *slot = None;
                }
                Ok(Box::new(producer))
            }
            Err(err) => {
                self.record_error(format!("{err:#}"));
                Err(err)
            }
        }
    }

    async fn last_error(&self) -> Option<String> {
        self.last_error.lock().ok().and_then(|slot| slot.clone())
    }

    async fn announced_codec(&self, _source: &ScreenSource) -> String {
        "h264".into()
    }

    fn source_origin(&self, source: &ScreenSource) -> Option<(i32, i32)> {
        // Duplication captures one monitor, so its frames are in that monitor's
        // own coordinates. Without the origin added back, pointing at a
        // secondary monitor silently lands the pointer on the primary one.
        Some((source.x, source.y))
    }
}

/// A capture thread and the channel it feeds.
struct DxgiProducer {
    units: mpsc::Receiver<anyhow::Result<EncodedUnit>>,
    keyframe: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl DxgiProducer {
    async fn start(source_id: &str, tuning: StreamTuning) -> anyhow::Result<Self> {
        let output = resolve_output(source_id)?;
        let (target_width, target_height) = target_dimensions(
            output.width,
            output.height,
            tuning.max_width,
            tuning.max_height,
        );
        // H.264 macroblocks are 16x16 with subsampled chroma, so both
        // dimensions have to stay even or the encoder refuses to negotiate.
        let width = even_dimension(target_width.unwrap_or(output.width));
        let height = even_dimension(target_height.unwrap_or(output.height));
        let settings = EncoderSettings {
            width,
            height,
            fps: tuning.fps,
            bitrate_kbps: resolve_bitrate_kbps(
                tuning.bitrate_kbps,
                width,
                height,
                tuning.fps,
                tuning.quality,
            ),
            gop_size: keyframe_interval(tuning.fps),
        };
        let geometry = FrameGeometry {
            width,
            height,
            source_width: output.width,
            source_height: output.height,
        };

        let keyframe = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let (unit_tx, units) = mpsc::channel(UNIT_QUEUE_DEPTH);
        let (ready_tx, ready) = oneshot::channel();

        let worker = {
            let keyframe = keyframe.clone();
            let stop = stop.clone();
            std::thread::Builder::new()
                .name("waypad-capture".into())
                .spawn(move || {
                    capture_loop(
                        output, settings, geometry, tuning, keyframe, stop, unit_tx, ready_tx,
                    )
                })
                .context("failed to start the capture thread")?
        };

        // Setup runs on the worker so its COM objects stay on one thread, but
        // the caller still finds out synchronously whether it worked: a failure
        // here has to be reported as an error the client can read, not as a
        // stream that opens and then produces nothing.
        match ready.await {
            Ok(Ok(())) => Ok(Self {
                units,
                keyframe,
                stop,
                worker: Some(worker),
            }),
            Ok(Err(err)) => {
                stop.store(true, Ordering::Relaxed);
                let _ = worker.join();
                Err(anyhow::anyhow!(err))
            }
            Err(_) => {
                stop.store(true, Ordering::Relaxed);
                let _ = worker.join();
                bail!("the capture thread stopped before reporting readiness")
            }
        }
    }
}

#[async_trait]
impl FrameProducer for DxgiProducer {
    fn encoding(&self) -> StreamEncoding {
        StreamEncoding::H264
    }

    async fn next_unit(&mut self) -> anyhow::Result<Option<EncodedUnit>> {
        match self.units.recv().await {
            Some(Ok(unit)) => Ok(Some(unit)),
            Some(Err(err)) => Err(err),
            None => Ok(None),
        }
    }

    async fn request_key_frame(&mut self) -> anyhow::Result<()> {
        // Media Foundation takes this as a property on the running encoder, so
        // unlike the GStreamer path it costs nothing and leaves no gap. The
        // flag is read by the capture thread on its next iteration.
        self.keyframe.store(true, Ordering::Relaxed);
        Ok(())
    }

    async fn shutdown(mut self: Box<Self>) {
        self.stop.store(true, Ordering::Relaxed);
        // Dropping the receiver unblocks a worker parked on a full channel.
        self.units.close();
        if let Some(worker) = self.worker.take() {
            let _ = tokio::task::spawn_blocking(move || worker.join()).await;
        }
    }
}

impl Drop for DxgiProducer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

#[allow(clippy::too_many_arguments)]
fn capture_loop(
    output: OutputInfo,
    settings: EncoderSettings,
    geometry: FrameGeometry,
    tuning: StreamTuning,
    keyframe: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    units: mpsc::Sender<anyhow::Result<EncodedUnit>>,
    ready: oneshot::Sender<Result<(), String>>,
) {
    let mut state = match CaptureState::new(&output, settings) {
        Ok(state) => {
            let _ = ready.send(Ok(()));
            state
        }
        Err(err) => {
            let _ = ready.send(Err(format!("{err:#}")));
            return;
        }
    };
    info!(
        output = %output.device_name,
        width = settings.width,
        height = settings.height,
        fps = settings.fps,
        bitrate_kbps = settings.bitrate_kbps,
        encoder = state.encoder.name(),
        "windows capture started"
    );

    let frame_interval = Duration::from_secs_f64(1.0 / f64::from(tuning.fps.max(1)));
    // Duplication is the clock. It returns only when the desktop has actually
    // changed, so the loop is already paced by the display — and sleeping a
    // frame interval on top of that does not slow the stream to the target, it
    // halves it: the sleep ends just as an update lands, the acquire misses it,
    // and the next one is a whole refresh away. `frame_interval` is used below
    // as a ceiling on how often a frame is *kept*, never as a delay before
    // asking for one.
    //
    // Dated one interval into the past so the first frame is never held back.
    let mut last_kept = Instant::now() - frame_interval;
    // A decoder joining on a P-frame shows nothing until the next IDR, so the
    // stream is held back until the encoder's first keyframe.
    let mut waiting_for_keyframe = true;

    while !stop.load(Ordering::Relaxed) {
        if keyframe.swap(false, Ordering::Relaxed)
            && let Err(err) = state.encoder.request_key_frame()
        {
            warn!(%err, "could not force a keyframe; the stream continues without one");
        }

        let encoded = match state.next_encoded(frame_interval, &mut last_kept) {
            Ok(Some(encoded)) => encoded,
            // Nothing changed on screen. Sending nothing is right: the client
            // holds the last picture and the link stays quiet.
            Ok(None) => continue,
            Err(err) => {
                let _ = units.blocking_send(Err(err));
                return;
            }
        };

        for unit in state.reader.push(&encoded) {
            if waiting_for_keyframe {
                if !unit.key_frame {
                    continue;
                }
                waiting_for_keyframe = false;
                debug!("windows capture reached its first keyframe");
            }
            let unit = EncodedUnit {
                data: unit.data,
                key_frame: unit.key_frame,
                parameter_sets: unit.parameter_sets,
                geometry,
            };
            // Blocks when the consumer is behind, which is the backpressure
            // that keeps latency bounded instead of queueing stale desktops.
            if units.blocking_send(Ok(unit)).is_err() {
                debug!("windows capture stopping: the stream consumer went away");
                return;
            }
        }
    }
    debug!("windows capture loop stopped");
}

/// Everything the capture thread owns. Kept together so its COM objects never
/// leave the thread they were created on.
struct CaptureState {
    duplicated: DuplicatedOutput,
    encoder: H264Encoder,
    nv12: Nv12Buffer,
    reader: AnnexBStreamReader,
    staging: Option<ID3D11Texture2D>,
}

impl CaptureState {
    fn new(output: &OutputInfo, settings: EncoderSettings) -> anyhow::Result<Self> {
        Ok(Self {
            duplicated: DuplicatedOutput::open(output)?,
            encoder: H264Encoder::new(settings)?,
            nv12: Nv12Buffer::new(settings.width, settings.height),
            reader: AnnexBStreamReader::new(),
            staging: None,
        })
    }

    /// Captures one frame if the desktop changed, and returns whatever the
    /// encoder produced from it.
    ///
    /// A desktop that updates faster than the client asked for has its extra
    /// frames dropped here, before anything is paid for them: the check sits
    /// between the acquire and the conversion because converting and encoding
    /// a frame nobody will receive is the most expensive way to do nothing.
    fn next_encoded(
        &mut self,
        min_interval: Duration,
        last_kept: &mut Instant,
    ) -> anyhow::Result<Option<Vec<u8>>> {
        let Some(frame) = self.duplicated.acquire(ACQUIRE_TIMEOUT_MS)? else {
            return Ok(None);
        };
        if last_kept.elapsed() < min_interval {
            self.duplicated.release()?;
            return Ok(None);
        }
        *last_kept = Instant::now();
        let result = self.convert(&frame);
        // Released before anything else can fail: duplication hands out one
        // surface at a time, and holding it stalls the desktop for every
        // application on the machine, not just this one.
        self.duplicated.release()?;
        result?;
        self.encoder.encode(&self.nv12).map(Some)
    }

    fn convert(&mut self, frame: &ID3D11Texture2D) -> anyhow::Result<()> {
        // SAFETY: the texture is alive for this call, the staging copy is owned
        // here, and the mapping is released on every path out.
        unsafe {
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            frame.GetDesc(&mut desc);
            if self.staging.is_none() {
                let staging_desc = D3D11_TEXTURE2D_DESC {
                    Usage: D3D11_USAGE_STAGING,
                    BindFlags: 0,
                    CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                    MiscFlags: 0,
                    ..desc
                };
                let mut created = None;
                self.duplicated
                    .device()
                    .CreateTexture2D(&staging_desc, None, Some(&mut created))
                    .context("failed to create the capture staging texture")?;
                self.staging = created;
            }
            let staging = self
                .staging
                .as_ref()
                .context("staging texture unavailable")?;
            self.duplicated.context().CopyResource(staging, frame);

            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.duplicated
                .context()
                .Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .context("failed to map the capture staging texture")?;
            let source = std::slice::from_raw_parts(
                mapped.pData as *const u8,
                mapped.RowPitch as usize * desc.Height as usize,
            );
            let converted = bgra_to_nv12(
                source,
                desc.Width,
                desc.Height,
                mapped.RowPitch as usize,
                &mut self.nv12,
            );
            self.duplicated.context().Unmap(staging, 0);
            converted
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn lists_the_monitors_as_h264_sources() {
        crate::skip_without_desktop!();
        let backend = WindowsCaptureBackend::new();
        let sources = backend.list_sources().await.unwrap();
        assert!(!sources.is_empty(), "a desktop host has a monitor");
        for source in &sources {
            assert_eq!(source.backend, "windows-dxgi");
            assert_eq!(backend.announced_codec(source).await, "h264");
            // Every monitor's frames arrive in its own coordinates, so every
            // one reports an origin — including the primary, whose origin is
            // (0,0) and therefore a harmless no-op.
            assert_eq!(backend.source_origin(source), Some((source.x, source.y)));
        }
    }

    #[tokio::test]
    #[ignore = "opens desktop duplication and an H.264 encoder"]
    async fn opening_an_unknown_source_falls_back_rather_than_failing() {
        // A client reconnecting after the monitor layout changed should get a
        // stream of something, not an error it cannot act on.
        let backend = WindowsCaptureBackend::new();
        let mut source = backend.list_sources().await.unwrap()[0].clone();
        source.id = "windows:monitor:\\\\.\\GONE".into();
        let tuning = StreamTuning::resolve(Some(15), None, Some(2000), Some(640), Some(480));
        let producer = backend.open(&source, tuning).await;
        assert!(producer.is_ok(), "{:?}", producer.err());
        producer.unwrap().shutdown().await;
    }
}
