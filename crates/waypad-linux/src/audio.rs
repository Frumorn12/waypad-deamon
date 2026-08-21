//! Desktop audio capture on Linux: PulseAudio/PipeWire monitor into Opus.
//!
//! Captures the monitor of the *current* default sink — what the speakers play,
//! not what the microphone hears. Everything from the Opus packet onwards
//! (envelope framing, muting, restart policy, metrics) lives in
//! `waypad_core::audio`; this file only produces the packets.

use crate::platform::{command_exists, command_output};
use anyhow::{Context, bail};
use async_trait::async_trait;
use std::collections::VecDeque;
use tokio::{
    io::AsyncReadExt,
    process::{Child, ChildStdout, Command},
    task::JoinHandle,
};
use tracing::{debug, warn};
use waypad_core::{
    audio::{
        AUDIO_CHANNELS, AUDIO_CODEC, AUDIO_SAMPLE_RATE, AudioFormat, AudioStreamOptions,
        MAX_PACKETS_PER_BATCH, stale_packet_count,
    },
    backend::{AudioBackend, AudioProducer},
    capability::AudioCaptureCapability,
};

/// GStreamer elements the capture pipeline needs. Probed up front so a missing
/// plugin is reported as a capability instead of failing at stream time.
const REQUIRED_ELEMENTS: &[&str] = &[
    "pulsesrc",
    "audioconvert",
    "audioresample",
    "opusenc",
    "rtpopuspay",
    "rtpstreampay",
];

/// Largest RFC 4571 frame accepted before the reader assumes it lost sync.
const MAX_RTP_FRAME_BYTES: usize = 8 * 1024;

#[derive(Debug, Default)]
pub struct LinuxAudioBackend;

impl LinuxAudioBackend {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl AudioBackend for LinuxAudioBackend {
    fn name(&self) -> &'static str {
        "pulse-monitor-opus"
    }

    async fn probe(&self) -> AudioCaptureCapability {
        let probe = probe_desktop_audio();
        let supported = probe.supported();
        AudioCaptureCapability {
            supported,
            backend: if supported {
                "pulse-monitor-opus".into()
            } else {
                "noop".into()
            },
            codec: supported.then(|| AUDIO_CODEC.to_string()),
            sample_rate: AUDIO_SAMPLE_RATE,
            channels: AUDIO_CHANNELS,
            default_sink: probe.default_sink.clone(),
            monitor_source: probe.monitor_source.clone(),
            pactl_available: probe.pactl_available,
            gstreamer_opus_available: probe.gstreamer_available,
            missing_elements: probe
                .missing_elements
                .iter()
                .map(|element| (*element).to_string())
                .collect(),
            reason: Some(probe.reason()),
        }
    }

    async fn open(&self, options: AudioStreamOptions) -> anyhow::Result<Box<dyn AudioProducer>> {
        // Resolved here rather than at detection time so switching output
        // device between attempts is picked up instead of recording silence.
        let device = resolve_monitor_device()?;
        let mut child = spawn_audio_pipeline(&device.monitor, options)?;
        let stderr_task = child.stderr.take().map(|stderr| {
            tokio::spawn(async move {
                let mut stderr = stderr;
                let mut buffer = [0u8; 2048];
                while let Ok(n) = stderr.read(&mut buffer).await {
                    if n == 0 {
                        break;
                    }
                    let text = String::from_utf8_lossy(&buffer[..n]).trim().to_string();
                    if !text.is_empty() {
                        warn!(producer = "gstreamer-audio", stderr = %text, "desktop audio producer stderr");
                    }
                }
            })
        });
        let stdout = child
            .stdout
            .take()
            .context("GStreamer audio stdout unavailable")?;
        Ok(Box::new(GstAudioProducer {
            child,
            stdout,
            stderr_task,
            reader: RtpStreamReader::new(),
            pending: VecDeque::new(),
            source: device.monitor,
            format: options.format(),
        }))
    }
}

/// One `gst-launch-1.0` pipeline, read as RFC 4571 frames.
struct GstAudioProducer {
    child: Child,
    stdout: ChildStdout,
    stderr_task: Option<JoinHandle<()>>,
    reader: RtpStreamReader,
    /// Packets decoded from the last pipe read but not yet handed out.
    pending: VecDeque<Vec<u8>>,
    source: String,
    format: AudioFormat,
}

#[async_trait]
impl AudioProducer for GstAudioProducer {
    fn format(&self) -> AudioFormat {
        self.format
    }

    fn source_label(&self) -> Option<String> {
        Some(self.source.clone())
    }

    async fn next_packet(&mut self) -> anyhow::Result<Option<Vec<u8>>> {
        loop {
            if let Some(packet) = self.pending.pop_front() {
                return Ok(Some(packet));
            }
            let mut buffer = vec![0u8; 16 * 1024];
            let n = self
                .stdout
                .read(&mut buffer)
                .await
                .context("failed to read the desktop audio encoder pipe")?;
            if n == 0 {
                return Ok(None);
            }
            let mut packets = self.reader.push(&buffer[..n])?;
            // Reading more than one batch in a single wakeup means the socket
            // is draining slower than real time. Audio cannot catch up by
            // playing faster, so the backlog is cut from the front.
            let stale = stale_packet_count(packets.len(), MAX_PACKETS_PER_BATCH);
            if stale > 0 {
                packets.drain(..stale);
                debug!(
                    dropped = stale,
                    "desktop audio backlog trimmed to the freshest packets"
                );
            }
            self.pending.extend(packets);
        }
    }

    async fn shutdown(mut self: Box<Self>) {
        let _ = self.child.kill().await;
        if let Some(task) = self.stderr_task.take() {
            task.abort();
        }
    }
}

fn spawn_audio_pipeline(monitor: &str, options: AudioStreamOptions) -> anyhow::Result<Child> {
    let args = gstreamer_audio_pipeline_args(monitor, options);
    debug!(pipeline = %args.join(" "), "launching GStreamer desktop audio pipeline");
    Command::new("gst-launch-1.0")
        .args(args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        // A cancelled task must not leave an encoder holding the monitor source.
        .kill_on_drop(true)
        .spawn()
        .context("failed to launch the GStreamer desktop audio pipeline")
}

/// Builds the capture pipeline.
///
/// `rtpopuspay ! rtpstreampay` is not about RTP as a transport: it is the only
/// stock way to get *framed* Opus off `fdsink`. Raw Opus packets are not
/// self-delimiting, so a bare `opusenc ! fdsink` would emit a byte soup no
/// reader can split back into packets. RFC 4571 prefixes every RTP packet with
/// its length, which the daemon strips again before the envelope is sent.
fn gstreamer_audio_pipeline_args(monitor: &str, options: AudioStreamOptions) -> Vec<String> {
    vec![
        "-q".into(),
        "pulsesrc".into(),
        format!("device={monitor}"),
        "do-timestamp=true".into(),
        // Defaults are 200 ms of device buffering; that alone would dominate the
        // latency budget of a 20 ms frame.
        "buffer-time=40000".into(),
        "latency-time=10000".into(),
        "!".into(),
        // Backpressure may only cost freshness, never unbounded delay: raw audio
        // is disposable, an encoded packet still on the wire is not.
        "queue".into(),
        "max-size-buffers=8".into(),
        "max-size-time=0".into(),
        "max-size-bytes=0".into(),
        "leaky=downstream".into(),
        "!".into(),
        "audioconvert".into(),
        "!".into(),
        "audioresample".into(),
        "!".into(),
        format!(
            "audio/x-raw,format=S16LE,rate={AUDIO_SAMPLE_RATE},channels={AUDIO_CHANNELS},layout=interleaved"
        ),
        "!".into(),
        "opusenc".into(),
        format!("bitrate={}", options.bitrate_kbps * 1000),
        // Constrained VBR, not CBR. A sink monitor never stops producing samples, so a silent
        // desktop is still a full stream of digital silence; measured on this host, 10 s of it
        // costs 127 kB at `bitrate-type=cbr` and 7.5 kB at `constrained-vbr`, while real content
        // costs the same 102 kbps either way. CBR would spend the full bitrate on nothing.
        "bitrate-type=constrained-vbr".into(),
        // Discontinuous transmission trims a further ~11 % off silence. It is deliberately paired
        // with the line above and not used alone: libopus ignores DTX under hard CBR, which is
        // measurable — with `bitrate-type=cbr` this flag changes the output by exactly zero bytes.
        "dtx=true".into(),
        format!("frame-size={}", options.frame_ms),
        // "generic" is opusenc's name for OPUS_APPLICATION_AUDIO; "voice" would
        // apply speech shaping to desktop audio, "restricted-lowdelay" drops the
        // SILK layer entirely.
        "audio-type=generic".into(),
        // Full complexity costs measurable CPU per frame for an inaudible gain
        // at this bitrate.
        "complexity=5".into(),
        "!".into(),
        "rtpopuspay".into(),
        "!".into(),
        "rtpstreampay".into(),
        "!".into(),
        "fdsink".into(),
        "fd=1".into(),
        "sync=false".into(),
    ]
}

/// The monitor source that carries what the default sink is playing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioDevice {
    pub sink: Option<String>,
    pub monitor: String,
}

/// Resolves the capture device at *runtime*, never at build or start time: the
/// user can switch output devices while a stream is running, and a hardcoded
/// monitor would then record silence with no error anywhere.
pub fn resolve_monitor_device() -> anyhow::Result<AudioDevice> {
    if !command_exists("pactl") {
        bail!("pactl is not installed, so the default sink cannot be resolved");
    }
    let sources = command_output("pactl", &["list", "short", "sources"])
        .context("pactl listed no PipeWire/PulseAudio sources")?;
    let default_sink = command_output("pactl", &["get-default-sink"]);
    let monitor = pick_monitor_source(default_sink.as_deref(), &sources).with_context(|| {
        format!(
            "no monitor source found for the default sink {}",
            default_sink.as_deref().unwrap_or("(unknown)")
        )
    })?;
    Ok(AudioDevice {
        sink: default_sink,
        monitor,
    })
}

/// Picks the monitor of `default_sink` out of `pactl list short sources`.
///
/// Falls back to the first monitor source in the list: a host whose default sink
/// has no monitor (a null sink, a filtered chain) can still stream *some*
/// desktop audio, which beats streaming none.
pub fn pick_monitor_source(default_sink: Option<&str>, sources: &str) -> Option<String> {
    let names: Vec<&str> = sources
        .lines()
        .filter_map(|line| line.split('\t').nth(1).map(str::trim))
        .filter(|name| !name.is_empty())
        .collect();
    if let Some(sink) = default_sink.map(str::trim).filter(|sink| !sink.is_empty()) {
        let expected = format!("{sink}.monitor");
        if let Some(name) = names.iter().find(|name| **name == expected) {
            return Some((*name).to_string());
        }
        // Some setups make the monitor itself the default "sink" name.
        if sink.ends_with(".monitor") && names.contains(&sink) {
            return Some(sink.to_string());
        }
    }
    names
        .into_iter()
        .find(|name| name.ends_with(".monitor"))
        .map(ToOwned::to_owned)
}

/// What capability detection needs to decide whether desktop audio is offered.
#[derive(Clone, Debug)]
pub struct DesktopAudioProbe {
    pub pactl_available: bool,
    pub gstreamer_available: bool,
    pub missing_elements: Vec<&'static str>,
    pub default_sink: Option<String>,
    pub monitor_source: Option<String>,
}

impl DesktopAudioProbe {
    pub fn supported(&self) -> bool {
        self.gstreamer_available && self.monitor_source.is_some()
    }

    /// Why audio is unavailable, in the same "what is missing and why" style the
    /// other capabilities use.
    pub fn reason(&self) -> String {
        if self.supported() {
            return format!(
                "Desktop audio is captured from the monitor of the current default sink ({}) and encoded to Opus",
                self.monitor_source.as_deref().unwrap_or("unknown")
            );
        }
        if !self.gstreamer_available {
            return format!(
                "Desktop audio unavailable: GStreamer is missing {}",
                if self.missing_elements.is_empty() {
                    "gst-launch-1.0".to_string()
                } else {
                    self.missing_elements.join(", ")
                }
            );
        }
        if !self.pactl_available {
            return "Desktop audio unavailable: pactl is not installed, so the default sink cannot be resolved".into();
        }
        "Desktop audio unavailable: no monitor source is exposed for any output device".into()
    }
}

pub fn probe_desktop_audio() -> DesktopAudioProbe {
    let pactl_available = command_exists("pactl");
    let missing_elements: Vec<&'static str> = if command_exists("gst-launch-1.0") {
        REQUIRED_ELEMENTS
            .iter()
            .copied()
            .filter(|element| command_output("gst-inspect-1.0", &[element]).is_none())
            .collect()
    } else {
        REQUIRED_ELEMENTS.to_vec()
    };
    let gstreamer_available = command_exists("gst-launch-1.0") && missing_elements.is_empty();
    let device = pactl_available
        .then(resolve_monitor_device)
        .and_then(Result::ok);
    DesktopAudioProbe {
        pactl_available,
        gstreamer_available,
        missing_elements,
        default_sink: device.as_ref().and_then(|device| device.sink.clone()),
        monitor_source: device.map(|device| device.monitor),
    }
}

/// Splits the RFC 4571 stream `rtpstreampay` writes into bare Opus packets.
///
/// Reads off the pipe land on arbitrary boundaries, so a frame is only released
/// once all of its bytes have arrived.
#[derive(Debug, Default)]
struct RtpStreamReader {
    buffer: Vec<u8>,
}

impl RtpStreamReader {
    fn new() -> Self {
        Self::default()
    }

    fn push(&mut self, chunk: &[u8]) -> anyhow::Result<Vec<Vec<u8>>> {
        self.buffer.extend_from_slice(chunk);
        let mut packets = Vec::new();
        loop {
            if self.buffer.len() < 2 {
                break;
            }
            let length = u16::from_be_bytes([self.buffer[0], self.buffer[1]]) as usize;
            if length == 0 || length > MAX_RTP_FRAME_BYTES {
                bail!("desktop audio producer emitted an invalid RTP frame length: {length}");
            }
            if self.buffer.len() < 2 + length {
                break;
            }
            let frame = self.buffer[2..2 + length].to_vec();
            self.buffer.drain(..2 + length);
            match rtp_payload(&frame) {
                Some(payload) if !payload.is_empty() => packets.push(payload.to_vec()),
                // An RTP packet without an Opus payload is not a stream error:
                // it is skipped so a stray control packet cannot kill the audio.
                _ => debug!(bytes = frame.len(), "skipping an RTP frame with no payload"),
            }
        }
        Ok(packets)
    }
}

/// Extracts the payload of an RTP packet, honouring CSRC list, header extension
/// and padding so a payloader that ever starts using them cannot corrupt a
/// packet silently.
fn rtp_payload(packet: &[u8]) -> Option<&[u8]> {
    if packet.len() < 12 || packet[0] >> 6 != 2 {
        return None;
    }
    let csrc_count = (packet[0] & 0x0f) as usize;
    let has_extension = packet[0] & 0x10 != 0;
    let has_padding = packet[0] & 0x20 != 0;
    let mut offset = 12 + csrc_count * 4;
    if has_extension {
        if packet.len() < offset + 4 {
            return None;
        }
        let words = u16::from_be_bytes([packet[offset + 2], packet[offset + 3]]) as usize;
        offset += 4 + words * 4;
    }
    let mut end = packet.len();
    if has_padding {
        let padding = *packet.last()? as usize;
        end = end.checked_sub(padding)?;
    }
    if offset > end {
        return None;
    }
    Some(&packet[offset..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    const SOURCES: &str = "64\talsa_output.pci-0000_00_1f.3.analog-stereo.monitor\tPipeWire\ts32le 2ch 48000Hz\tSUSPENDED\n\
65\talsa_input.pci-0000_00_1f.3.analog-stereo\tPipeWire\ts32le 2ch 48000Hz\tSUSPENDED\n\
913\talsa_output.pci-0000_01_00.1.hdmi-stereo.monitor\tPipeWire\ts32le 2ch 48000Hz\tSUSPENDED";

    #[test]
    fn resolves_the_monitor_of_the_current_default_sink() {
        assert_eq!(
            pick_monitor_source(Some("alsa_output.pci-0000_00_1f.3.analog-stereo"), SOURCES)
                .as_deref(),
            Some("alsa_output.pci-0000_00_1f.3.analog-stereo.monitor")
        );
        // Switching the default output has to follow, not stay pinned to the
        // first monitor in the list.
        assert_eq!(
            pick_monitor_source(Some("alsa_output.pci-0000_01_00.1.hdmi-stereo"), SOURCES)
                .as_deref(),
            Some("alsa_output.pci-0000_01_00.1.hdmi-stereo.monitor")
        );
    }

    #[test]
    fn falls_back_to_any_monitor_when_the_default_sink_has_none() {
        assert_eq!(
            pick_monitor_source(Some("some_null_sink"), SOURCES).as_deref(),
            Some("alsa_output.pci-0000_00_1f.3.analog-stereo.monitor")
        );
        assert_eq!(
            pick_monitor_source(None, SOURCES).as_deref(),
            Some("alsa_output.pci-0000_00_1f.3.analog-stereo.monitor")
        );
    }

    #[test]
    fn reports_no_device_when_no_source_is_a_monitor() {
        let inputs = "63\talsa_input.usb-Razer.mono-fallback\tPipeWire\ts16le\tSUSPENDED";
        assert_eq!(pick_monitor_source(Some("sink"), inputs), None);
        assert_eq!(pick_monitor_source(None, ""), None);
    }

    #[test]
    fn pipeline_captures_the_monitor_and_encodes_low_latency_opus() {
        let pipeline = gstreamer_audio_pipeline_args(
            "alsa_output.analog-stereo.monitor",
            AudioStreamOptions::new(Some(128), Some(10)),
        )
        .join(" ");

        assert!(
            pipeline.contains("device=alsa_output.analog-stereo.monitor"),
            "{pipeline}"
        );
        assert!(pipeline.contains("bitrate=128000"), "{pipeline}");
        assert!(pipeline.contains("frame-size=10"), "{pipeline}");
        assert!(pipeline.contains("audio-type=generic"), "{pipeline}");
        // A monitor source never goes quiet, it produces silence, so the encoder must be allowed
        // to spend nothing on it. Hard CBR would send the full bitrate for an idle desktop and
        // would additionally make the dtx flag a no-op.
        assert!(
            pipeline.contains("bitrate-type=constrained-vbr"),
            "{pipeline}"
        );
        assert!(!pipeline.contains("bitrate-type=cbr"), "{pipeline}");
        assert!(pipeline.contains("dtx=true"), "{pipeline}");
        // Framing is what makes the packets recoverable from a byte pipe.
        assert!(pipeline.contains("rtpstreampay"), "{pipeline}");
        // The device buffer has to be short or it dominates the latency budget.
        assert!(pipeline.contains("buffer-time=40000"), "{pipeline}");
        let encoder = pipeline.find("opusenc").expect("encoder is present");
        for converter in ["audioconvert", "audioresample"] {
            assert!(
                pipeline[..encoder].contains(converter),
                "{converter} missing upstream of opusenc in: {pipeline}"
            );
        }
    }

    /// One RFC 4571 frame: `[u16_be length][RTP header][payload]`.
    fn rtp_frame(sequence: u16, payload: &[u8]) -> Vec<u8> {
        let mut packet = vec![0x80, 0xe0];
        packet.extend_from_slice(&sequence.to_be_bytes());
        packet.extend_from_slice(&[0, 0, 0, 0]); // timestamp
        packet.extend_from_slice(&[1, 2, 3, 4]); // ssrc
        packet.extend_from_slice(payload);
        let mut framed = (packet.len() as u16).to_be_bytes().to_vec();
        framed.extend_from_slice(&packet);
        framed
    }

    #[test]
    fn splits_opus_packets_across_arbitrary_pipe_reads() {
        let mut stream = rtp_frame(1, &[0xfc, 1, 2, 3]);
        stream.extend_from_slice(&rtp_frame(2, &[0xfc, 4, 5]));
        stream.extend_from_slice(&rtp_frame(3, &[0xfc, 6]));

        for split in 1..stream.len() {
            let mut reader = RtpStreamReader::new();
            let mut packets = reader.push(&stream[..split]).unwrap();
            packets.extend(reader.push(&stream[split..]).unwrap());
            assert_eq!(
                packets,
                vec![vec![0xfc, 1, 2, 3], vec![0xfc, 4, 5], vec![0xfc, 6]],
                "split at {split}"
            );
        }
    }

    #[test]
    fn strips_csrc_extension_and_padding_from_rtp_packets() {
        // Version 2, one CSRC, extension present, padding present.
        let mut packet = vec![0xb1, 0xe0, 0, 1];
        packet.extend_from_slice(&[0, 0, 0, 0]); // timestamp
        packet.extend_from_slice(&[1, 2, 3, 4]); // ssrc
        packet.extend_from_slice(&[9, 9, 9, 9]); // one CSRC
        packet.extend_from_slice(&[0xbe, 0xde, 0, 1]); // extension, one word
        packet.extend_from_slice(&[7, 7, 7, 7]);
        packet.extend_from_slice(&[0xfc, 42]); // payload
        packet.extend_from_slice(&[0, 0, 3]); // three padding bytes

        assert_eq!(rtp_payload(&packet), Some(&[0xfc, 42][..]));
        assert_eq!(rtp_payload(&[0u8; 4]), None);
    }

    #[test]
    fn rejects_a_producer_that_lost_framing() {
        let mut reader = RtpStreamReader::new();
        assert!(reader.push(&[0xff, 0xff, 1, 2, 3]).is_err());
    }
}
