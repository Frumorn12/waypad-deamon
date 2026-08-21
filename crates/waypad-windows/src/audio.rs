//! Desktop audio capture through WASAPI loopback, encoded to Opus.
//!
//! Loopback capture reads back what the default output device is playing, which
//! is the same thing the Linux backend gets from a PulseAudio monitor source.
//! What it does *not* offer is a choice of format: shared mode hands over the
//! mix format the audio engine is already running, so the converting and
//! resampling has to happen here rather than being asked for.
//!
//! Like capture, this runs on its own thread. WASAPI wants a COM apartment and
//! blocks waiting for audio, neither of which belongs on an async runtime.

use anyhow::{Context, bail};
use async_trait::async_trait;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, info};
use waypad_core::{
    audio::{
        AUDIO_CHANNELS, AUDIO_CODEC, AUDIO_SAMPLE_RATE, AudioFormat, AudioStreamOptions,
        MAX_PACKETS_PER_BATCH, OPUS_PRE_SKIP_SAMPLES, stale_packet_count,
    },
    backend::{AudioBackend, AudioProducer},
    capability::AudioCaptureCapability,
};
use windows::Win32::{
    Media::Audio::{
        AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_LOOPBACK,
        IAudioCaptureClient, IAudioClient, IMMDeviceEnumerator, MMDeviceEnumerator, WAVEFORMATEX,
        WAVEFORMATEXTENSIBLE, eConsole, eRender,
    },
    Media::Multimedia::{KSDATAFORMAT_SUBTYPE_IEEE_FLOAT, WAVE_FORMAT_IEEE_FLOAT},
    System::Com::{
        CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
        CoUninitialize, STGM_READ,
    },
};

/// One second of buffer. Loopback is polled rather than event driven, so this
/// only has to be comfortably longer than the polling interval.
const BUFFER_DURATION_100NS: i64 = 10_000_000;

/// How long the capture thread sleeps between polls. Short against the 10 ms
/// Opus frame so a packet is never held back waiting for the next wakeup.
const POLL_INTERVAL_MS: u64 = 4;

const PACKET_QUEUE_DEPTH: usize = 16;

/// WAVE_FORMAT_EXTENSIBLE, which almost every modern mix format actually is.
const WAVE_FORMAT_EXTENSIBLE_TAG: u16 = 0xFFFE;

#[derive(Debug, Default)]
pub struct WindowsAudioBackend;

impl WindowsAudioBackend {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl AudioBackend for WindowsAudioBackend {
    fn name(&self) -> &'static str {
        "wasapi-loopback-opus"
    }

    async fn probe(&self) -> AudioCaptureCapability {
        let (supported, device, reason) = match default_render_device_name() {
            Ok(name) => (
                true,
                Some(name.clone()),
                format!(
                    "Desktop audio is captured from the current default output device ({name}) \
                     with WASAPI loopback and encoded to Opus"
                ),
            ),
            Err(err) => (
                false,
                None,
                format!("Desktop audio unavailable: {}", format_args!("{err:#}")),
            ),
        };
        AudioCaptureCapability {
            supported,
            backend: if supported {
                "wasapi-loopback-opus".into()
            } else {
                "noop".into()
            },
            codec: supported.then(|| AUDIO_CODEC.to_string()),
            sample_rate: AUDIO_SAMPLE_RATE,
            channels: AUDIO_CHANNELS,
            default_sink: device.clone(),
            // Windows has no separate monitor device: loopback reads the render
            // endpoint itself, so the two names are deliberately the same.
            monitor_source: device,
            // Both are PulseAudio and GStreamer concepts with no counterpart
            // here, and are reported false rather than left to imply a
            // half-configured host.
            pactl_available: false,
            gstreamer_opus_available: false,
            missing_elements: Vec::new(),
            reason: Some(reason),
        }
    }

    async fn open(&self, options: AudioStreamOptions) -> anyhow::Result<Box<dyn AudioProducer>> {
        Ok(Box::new(WasapiOpusProducer::start(options).await?))
    }
}

struct WasapiOpusProducer {
    packets: mpsc::Receiver<anyhow::Result<Vec<u8>>>,
    stop: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
    format: AudioFormat,
    device: String,
}

impl WasapiOpusProducer {
    async fn start(options: AudioStreamOptions) -> anyhow::Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let (packet_tx, packets) = mpsc::channel(PACKET_QUEUE_DEPTH);
        let (ready_tx, ready) = oneshot::channel();
        let worker = {
            let stop = stop.clone();
            std::thread::Builder::new()
                .name("waypad-audio".into())
                .spawn(move || capture_loop(options, stop, packet_tx, ready_tx))
                .context("failed to start the audio capture thread")?
        };
        match ready.await {
            Ok(Ok(device)) => Ok(Self {
                packets,
                stop,
                worker: Some(worker),
                format: options.format(),
                device,
            }),
            Ok(Err(err)) => {
                stop.store(true, Ordering::Relaxed);
                let _ = worker.join();
                Err(anyhow::anyhow!(err))
            }
            Err(_) => {
                stop.store(true, Ordering::Relaxed);
                let _ = worker.join();
                bail!("the audio capture thread stopped before reporting readiness")
            }
        }
    }
}

#[async_trait]
impl AudioProducer for WasapiOpusProducer {
    fn format(&self) -> AudioFormat {
        self.format
    }

    fn source_label(&self) -> Option<String> {
        Some(self.device.clone())
    }

    async fn next_packet(&mut self) -> anyhow::Result<Option<Vec<u8>>> {
        match self.packets.recv().await {
            Some(Ok(packet)) => Ok(Some(packet)),
            Some(Err(err)) => Err(err),
            None => Ok(None),
        }
    }

    async fn shutdown(mut self: Box<Self>) {
        self.stop.store(true, Ordering::Relaxed);
        self.packets.close();
        if let Some(worker) = self.worker.take() {
            let _ = tokio::task::spawn_blocking(move || worker.join()).await;
        }
    }
}

impl Drop for WasapiOpusProducer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// The friendly name of the device loopback will read, for diagnostics.
fn default_render_device_name() -> anyhow::Result<String> {
    let _com = ComApartment::enter()?;
    // SAFETY: the enumerator and device live only inside this function.
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                .context("could not open the audio device enumerator")?;
        let device = enumerator
            .GetDefaultAudioEndpoint(eRender, eConsole)
            .context("this session has no default audio output device")?;
        let store = device
            .OpenPropertyStore(STGM_READ)
            .context("could not read the output device properties")?;
        let name = store
            .GetValue(&windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName)
            .ok()
            .and_then(|value| value.to_string().into())
            .unwrap_or_else(|| "default output".to_string());
        Ok(name)
    }
}

fn capture_loop(
    options: AudioStreamOptions,
    stop: Arc<AtomicBool>,
    packets: mpsc::Sender<anyhow::Result<Vec<u8>>>,
    ready: oneshot::Sender<Result<String, String>>,
) {
    let _com = match ComApartment::enter() {
        Ok(guard) => guard,
        Err(err) => {
            let _ = ready.send(Err(format!("{err:#}")));
            return;
        }
    };
    let mut session = match LoopbackSession::open(options) {
        Ok(session) => session,
        Err(err) => {
            let _ = ready.send(Err(format!("{err:#}")));
            return;
        }
    };
    let _ = ready.send(Ok(session.device.clone()));
    info!(
        device = %session.device,
        source_rate = session.source_rate,
        source_channels = session.source_channels,
        bitrate_kbps = options.bitrate_kbps,
        frame_ms = options.frame_ms,
        "desktop audio capture started"
    );

    while !stop.load(Ordering::Relaxed) {
        match session.poll() {
            Ok(mut ready_packets) => {
                // More than a batch means the consumer is behind. Audio cannot
                // catch up by playing faster, so the backlog is cut from the
                // front and only the freshest packets survive.
                let stale = stale_packet_count(ready_packets.len(), MAX_PACKETS_PER_BATCH);
                if stale > 0 {
                    ready_packets.drain(..stale);
                    debug!(dropped = stale, "desktop audio backlog trimmed");
                }
                for packet in ready_packets {
                    if packets.blocking_send(Ok(packet)).is_err() {
                        debug!("desktop audio stopping: the consumer went away");
                        return;
                    }
                }
            }
            Err(err) => {
                let _ = packets.blocking_send(Err(err));
                return;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(POLL_INTERVAL_MS));
    }
    debug!("desktop audio capture loop stopped");
}

/// A live loopback capture and the encoder behind it.
struct LoopbackSession {
    client: IAudioClient,
    capture: IAudioCaptureClient,
    encoder: opus::Encoder,
    device: String,
    source_rate: u32,
    source_channels: usize,
    source_is_float: bool,
    /// Interleaved stereo at 48 kHz, waiting to fill a whole Opus frame.
    pending: Vec<f32>,
    /// Fractional read position for the resampler, carried between polls so a
    /// buffer boundary does not introduce a click.
    resample_cursor: f64,
    /// Last stereo frame of the previous buffer, so interpolation across a
    /// boundary has something to interpolate from.
    previous: [f32; 2],
    samples_per_frame: usize,
}

impl LoopbackSession {
    fn open(options: AudioStreamOptions) -> anyhow::Result<Self> {
        // SAFETY: every interface is created here and released with the struct.
        unsafe {
            let enumerator: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                    .context("could not open the audio device enumerator")?;
            // Resolved now rather than cached at detection time, so switching
            // output device between attempts is picked up instead of recording
            // from a device nobody is listening to.
            let device = enumerator
                .GetDefaultAudioEndpoint(eRender, eConsole)
                .context("this session has no default audio output device")?;
            let name = device
                .OpenPropertyStore(STGM_READ)
                .ok()
                .and_then(|store| {
                    store
                        .GetValue(
                            &windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName,
                        )
                        .ok()
                })
                .map(|value| value.to_string())
                .unwrap_or_else(|| "default output".into());

            let client: IAudioClient = device
                .Activate(CLSCTX_ALL, None)
                .context("could not activate the audio client")?;
            let mix = client
                .GetMixFormat()
                .context("could not read the audio engine mix format")?;
            let format = *mix;
            let (is_float, channels, rate) = describe_format(mix);

            client
                .Initialize(
                    AUDCLNT_SHAREMODE_SHARED,
                    AUDCLNT_STREAMFLAGS_LOOPBACK,
                    BUFFER_DURATION_100NS,
                    0,
                    mix,
                    None,
                )
                .context("could not open the output device for loopback capture")?;
            CoTaskMemFree(Some(mix as *const _));
            let _ = format;

            let capture: IAudioCaptureClient = client
                .GetService()
                .context("could not obtain the loopback capture service")?;
            client.Start().context("could not start loopback capture")?;

            let mut encoder = opus::Encoder::new(
                AUDIO_SAMPLE_RATE,
                opus::Channels::Stereo,
                // "Audio" rather than "Voip": this is a desktop, not a voice
                // call, and speech shaping would mangle music.
                opus::Application::Audio,
            )
            .context("could not create the Opus encoder")?;
            encoder.set_bitrate(opus::Bitrate::Bits((options.bitrate_kbps * 1000) as i32))?;
            // Constrained VBR, matching the Linux path. A loopback device never
            // goes quiet — it produces digital silence — so hard CBR would
            // spend the entire bitrate on nothing at all.
            encoder.set_vbr(true)?;
            encoder.set_vbr_constraint(true)?;

            let samples_per_frame = (AUDIO_SAMPLE_RATE as usize / 1000) * options.frame_ms as usize;
            Ok(Self {
                client,
                capture,
                encoder,
                device: name,
                source_rate: rate,
                source_channels: channels,
                source_is_float: is_float,
                pending: Vec::with_capacity(samples_per_frame * 4),
                resample_cursor: 0.0,
                previous: [0.0; 2],
                samples_per_frame,
            })
        }
    }

    /// Drains whatever the engine has ready and returns any whole Opus frames.
    fn poll(&mut self) -> anyhow::Result<Vec<Vec<u8>>> {
        // SAFETY: every buffer obtained is released before the next iteration.
        unsafe {
            loop {
                let available = self
                    .capture
                    .GetNextPacketSize()
                    .context("loopback capture failed while checking for audio")?;
                if available == 0 {
                    break;
                }
                let mut data: *mut u8 = std::ptr::null_mut();
                let mut frames = 0u32;
                let mut flags = 0u32;
                self.capture
                    .GetBuffer(&mut data, &mut frames, &mut flags, None, None)
                    .context("loopback capture failed while reading audio")?;

                if frames > 0 {
                    // A silent buffer carries no data at all, only a flag, and
                    // has to be materialised as zeros: skipping it would
                    // compress the timeline and drift the audio ahead of the
                    // picture.
                    if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 {
                        self.push_silence(frames as usize);
                    } else {
                        let bytes = frames as usize
                            * self.source_channels
                            * if self.source_is_float { 4 } else { 2 };
                        let raw = std::slice::from_raw_parts(data, bytes);
                        self.push_samples(raw, frames as usize);
                    }
                }
                self.capture
                    .ReleaseBuffer(frames)
                    .context("loopback capture failed while releasing a buffer")?;
            }
        }
        self.drain_frames()
    }

    fn push_silence(&mut self, frames: usize) {
        for _ in 0..frames {
            self.accept_stereo([0.0, 0.0]);
        }
    }

    fn push_samples(&mut self, raw: &[u8], frames: usize) {
        for frame in 0..frames {
            let stereo = read_stereo_frame(raw, frame, self.source_channels, self.source_is_float);
            self.accept_stereo(stereo);
        }
    }

    /// Feeds one source frame through the resampler into the pending buffer.
    fn accept_stereo(&mut self, current: [f32; 2]) {
        if self.source_rate == AUDIO_SAMPLE_RATE {
            self.pending.push(current[0]);
            self.pending.push(current[1]);
            self.previous = current;
            return;
        }
        // Linear interpolation. Good enough for a rate ratio that is nearly
        // always 44.1k to 48k, and far cheaper than a windowed sinc for a
        // difference nobody will hear over a screen share.
        let step = f64::from(self.source_rate) / f64::from(AUDIO_SAMPLE_RATE);
        while self.resample_cursor < 1.0 {
            let t = self.resample_cursor as f32;
            self.pending
                .push(self.previous[0] + (current[0] - self.previous[0]) * t);
            self.pending
                .push(self.previous[1] + (current[1] - self.previous[1]) * t);
            self.resample_cursor += step;
        }
        self.resample_cursor -= 1.0;
        self.previous = current;
    }

    fn drain_frames(&mut self) -> anyhow::Result<Vec<Vec<u8>>> {
        let per_frame = self.samples_per_frame * 2;
        let mut packets = Vec::new();
        let mut output = vec![0u8; 4000];
        while self.pending.len() >= per_frame {
            let frame: Vec<f32> = self.pending.drain(..per_frame).collect();
            let written = self
                .encoder
                .encode_float(&frame, &mut output)
                .context("the Opus encoder rejected a frame")?;
            packets.push(output[..written].to_vec());
        }
        Ok(packets)
    }
}

impl Drop for LoopbackSession {
    fn drop(&mut self) {
        // SAFETY: the client is still alive and Stop is idempotent.
        unsafe {
            let _ = self.client.Stop();
        }
    }
}

/// Reads one interleaved source frame and folds it down to stereo.
///
/// Extra channels beyond the first two are dropped rather than mixed. A desktop
/// mix is stereo in practice, and a surround setup losing its rear channels is
/// a far smaller problem than the phase smearing a naive downmix would add.
fn read_stereo_frame(raw: &[u8], frame: usize, channels: usize, is_float: bool) -> [f32; 2] {
    let sample_bytes = if is_float { 4 } else { 2 };
    let base = frame * channels * sample_bytes;
    let read = |channel: usize| -> f32 {
        let at = base + channel * sample_bytes;
        if at + sample_bytes > raw.len() {
            return 0.0;
        }
        if is_float {
            f32::from_le_bytes([raw[at], raw[at + 1], raw[at + 2], raw[at + 3]])
        } else {
            f32::from(i16::from_le_bytes([raw[at], raw[at + 1]])) / 32768.0
        }
    };
    match channels {
        0 => [0.0, 0.0],
        // Mono is duplicated rather than left silent on one side.
        1 => {
            let mono = read(0);
            [mono, mono]
        }
        _ => [read(0), read(1)],
    }
}

/// Pulls the sample format out of a mix format, seeing through
/// WAVE_FORMAT_EXTENSIBLE.
///
/// Shared-mode WASAPI almost always reports extensible with a float subformat,
/// and reading `wFormatTag` alone would classify that as PCM and interpret
/// 32-bit floats as 16-bit integers — which produces noise, not silence, so it
/// is worth getting right.
fn describe_format(format: *const WAVEFORMATEX) -> (bool, usize, u32) {
    // SAFETY: the pointer comes from GetMixFormat and is valid until freed.
    unsafe {
        let base = &*format;
        let channels = base.nChannels as usize;
        let rate = base.nSamplesPerSec;
        let is_float = if base.wFormatTag == WAVE_FORMAT_EXTENSIBLE_TAG {
            // WAVEFORMATEXTENSIBLE is byte packed, so the GUID is copied out
            // rather than referenced: taking a reference to a misaligned field
            // is undefined behaviour even when it is never dereferenced.
            let subformat =
                std::ptr::addr_of!((*(format as *const WAVEFORMATEXTENSIBLE)).SubFormat)
                    .read_unaligned();
            subformat == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT
        } else {
            base.wFormatTag == WAVE_FORMAT_IEEE_FLOAT as u16
        };
        (is_float, channels, rate)
    }
}

/// Initialises COM for the calling thread and undoes it on the way out.
struct ComApartment;

impl ComApartment {
    fn enter() -> anyhow::Result<Self> {
        // SAFETY: paired with the CoUninitialize in Drop. A thread already in a
        // compatible apartment returns S_FALSE, which is not an error.
        unsafe {
            let result = CoInitializeEx(None, COINIT_MULTITHREADED);
            if result.is_err() {
                bail!("could not enter a COM apartment: {result:?}");
            }
        }
        Ok(Self)
    }
}

impl Drop for ComApartment {
    fn drop(&mut self) {
        // SAFETY: paired with the CoInitializeEx above.
        unsafe { CoUninitialize() };
    }
}

/// The format every packet declares. Fixed, because Opus only runs at 48 kHz
/// and the client rebuilds its decoder from these fields on every packet.
pub fn wire_format(options: AudioStreamOptions) -> AudioFormat {
    AudioFormat {
        sample_rate: AUDIO_SAMPLE_RATE,
        channels: AUDIO_CHANNELS,
        frame_ms: options.frame_ms,
        pre_skip: OPUS_PRE_SKIP_SAMPLES,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_float_and_integer_frames_alike() {
        // Float32 stereo, the usual shared-mode mix format.
        let mut raw = Vec::new();
        raw.extend_from_slice(&0.5f32.to_le_bytes());
        raw.extend_from_slice(&(-0.25f32).to_le_bytes());
        let frame = read_stereo_frame(&raw, 0, 2, true);
        assert!((frame[0] - 0.5).abs() < 1e-6);
        assert!((frame[1] + 0.25).abs() < 1e-6);

        // 16-bit PCM, normalised to the same range.
        let mut pcm = Vec::new();
        pcm.extend_from_slice(&16384i16.to_le_bytes());
        pcm.extend_from_slice(&(-16384i16).to_le_bytes());
        let frame = read_stereo_frame(&pcm, 0, 2, false);
        assert!((frame[0] - 0.5).abs() < 1e-4);
        assert!((frame[1] + 0.5).abs() < 1e-4);
    }

    #[test]
    fn mono_is_duplicated_rather_than_left_on_one_side() {
        let mut raw = Vec::new();
        raw.extend_from_slice(&0.75f32.to_le_bytes());
        assert_eq!(read_stereo_frame(&raw, 0, 1, true), [0.75, 0.75]);
    }

    #[test]
    fn surround_keeps_the_front_pair() {
        // 5.1: front left, front right, centre, LFE, rear left, rear right.
        let mut raw = Vec::new();
        for value in [0.1f32, 0.2, 0.3, 0.4, 0.5, 0.6] {
            raw.extend_from_slice(&value.to_le_bytes());
        }
        let frame = read_stereo_frame(&raw, 0, 6, true);
        assert!((frame[0] - 0.1).abs() < 1e-6);
        assert!((frame[1] - 0.2).abs() < 1e-6);
    }

    #[test]
    fn a_truncated_buffer_reads_as_silence_rather_than_out_of_bounds() {
        // The engine should never hand over a short buffer, but reading past
        // one would be undefined behaviour rather than a wrong sample.
        assert_eq!(read_stereo_frame(&[0u8; 2], 0, 2, true), [0.0, 0.0]);
        assert_eq!(read_stereo_frame(&[], 5, 2, false), [0.0, 0.0]);
    }

    #[test]
    fn the_wire_format_is_what_the_client_rebuilds_its_decoder_from() {
        let format = wire_format(AudioStreamOptions::new(Some(96), Some(10)));
        assert_eq!(format.sample_rate, 48_000);
        assert_eq!(format.channels, 2);
        assert_eq!(format.frame_ms, 10);
        assert_eq!(format.pre_skip, OPUS_PRE_SKIP_SAMPLES);
    }

    #[tokio::test]
    async fn probe_reports_the_default_output_or_says_why_not() {
        let backend = WindowsAudioBackend::new();
        let capability = backend.probe().await;
        assert_eq!(capability.sample_rate, 48_000);
        assert_eq!(capability.channels, 2);
        // Either way there is a sentence for the user; a bare false with no
        // reason is the one outcome that is never acceptable.
        assert!(capability.reason.is_some());
        if capability.supported {
            assert_eq!(capability.backend, "wasapi-loopback-opus");
            assert_eq!(capability.codec.as_deref(), Some("opus"));
            assert!(capability.monitor_source.is_some());
        }
    }
}
