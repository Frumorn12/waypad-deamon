//! The seam between the platform-neutral Waypad host and the machine it runs on.
//!
//! Everything above this file — the protocol, the encrypted control channel,
//! pairing, discovery, stream framing — is identical on every host. Everything
//! below it is a Wayland portal, a Win32 call, or a subprocess. A platform crate
//! implements [`PlatformHost`] and the daemon binary picks one at compile time,
//! which is why neither platform's dependencies ever reach the other's build.

use crate::{
    audio::{AudioFormat, AudioStreamOptions},
    capability::{AudioCaptureCapability, Capabilities},
    config::Config,
    protocol::{BrightnessAction, ButtonState, MediaAction, PointerButton, VolumeAction},
    stream::{FrameGeometry, ScreenSource, StreamTuning},
};
use async_trait::async_trait;
use std::sync::Arc;
use tokio::sync::RwLock;

/// The single entry point a platform crate exports.
///
/// The backends are handed out one at a time rather than as one struct because
/// they have different lifetimes: the input backend is rebuilt whenever
/// capabilities are re-detected, since a portal approval can appear at any
/// moment, while capture, audio, and system control are stable for the process.
#[async_trait]
pub trait PlatformHost: Send + Sync + 'static {
    /// Short identifier for logs, e.g. `linux-wayland` or `windows`.
    fn name(&self) -> &'static str;

    /// Best-effort hostname for discovery replies and invites.
    fn hostname(&self) -> String;

    /// The address a QR invite should advertise on the local network.
    async fn primary_lan_address(&self) -> Option<String>;

    /// The live capability cell.
    ///
    /// Backends handed out by this host read from this exact cell, so a
    /// refresh has to be published here rather than into a copy the server
    /// keeps privately: two cells would drift, and the capture backend would go
    /// on listing sources for a portal that is no longer there.
    fn capabilities(&self) -> Arc<RwLock<Capabilities>>;

    /// Probes the host and publishes the result into [`Self::capabilities`].
    async fn detect_capabilities(&self, config: &Config) -> Capabilities;

    /// Builds the input backend matching `capabilities`.
    ///
    /// Called again after every capability refresh, so implementations must be
    /// cheap and must not assume they are the only live instance: the previous
    /// backend is dropped only once the new one exists.
    async fn input_backend(&self, capabilities: &Capabilities) -> Box<dyn InputBackend>;

    fn controller_backend(&self, capabilities: &Capabilities) -> Box<dyn ControllerBackend>;

    fn capture_backend(&self) -> Arc<dyn CaptureBackend>;

    fn audio_backend(&self) -> Arc<dyn AudioBackend>;

    fn system_backend(&self) -> Arc<dyn SystemBackend>;
}

/// Pointer and keyboard injection.
///
/// Every method takes `&self` except [`InputBackend::prepare`], because the
/// server holds one backend behind a mutex and injection must not queue behind
/// an approval flow that blocks for as long as a user takes to click a dialog.
#[async_trait]
pub trait InputBackend: Send + Sync {
    /// Backend identifier reported in capabilities and discovery, e.g.
    /// `wayland-portal`, `hyprland-hyprctl`, `windows-sendinput`.
    fn name(&self) -> &'static str;

    /// Runs whatever approval or session setup the platform needs before input
    /// can be injected, reporting what happened as JSON for the client.
    ///
    /// On Wayland this opens the RemoteDesktop portal dialog. On Windows there
    /// is nothing to approve, so it reports readiness immediately.
    async fn prepare(&mut self) -> anyhow::Result<serde_json::Value>;

    async fn pointer_move(&self, dx: f64, dy: f64) -> anyhow::Result<()>;

    /// Moves to a desktop coordinate. The caller has already translated
    /// source-local coordinates into desktop space.
    async fn pointer_move_absolute(&self, x: f64, y: f64) -> anyhow::Result<()>;

    async fn pointer_button(&self, button: PointerButton, state: ButtonState)
    -> anyhow::Result<()>;

    async fn scroll(&self, dx: f64, dy: f64, finish: bool) -> anyhow::Result<()>;

    /// Presses or releases an X11 keysym. Keysyms are the wire format on every
    /// platform; a backend that speaks something else translates here.
    async fn key(&self, keysym: u32, state: ButtonState) -> anyhow::Result<()>;

    async fn text(&self, text: &str) -> anyhow::Result<()>;

    /// Releases every key and button this backend believes is held.
    ///
    /// Called when a client disconnects mid-gesture. Without it a connection
    /// dropped during a drag leaves the host with a stuck mouse button and no
    /// way to notice, so this is best effort and never reports failure.
    fn release_all(&self);
}

/// Virtual gamepad injection for controllers attached to the phone.
///
/// Not async: every implementation writes to an already-open device handle.
pub trait ControllerBackend: Send + Sync {
    fn name(&self) -> &'static str;

    fn available(&self) -> bool;

    /// Why forwarding is unavailable, shown verbatim to the user.
    fn reason(&self) -> String;

    fn device_connected(&mut self, device_id: &str, name: &str) -> anyhow::Result<()>;

    fn device_disconnected(&mut self, device_id: &str) -> anyhow::Result<()>;

    fn button(&mut self, button: &str, state: ButtonState) -> anyhow::Result<()>;

    fn axis(&mut self, axis: &str, value: f64) -> anyhow::Result<()>;

    /// Commits buffered events as one atomic controller state update.
    fn flush_pending(&mut self) -> anyhow::Result<()>;
}

/// Volume, media, brightness, clipboard, and session actions.
#[async_trait]
pub trait SystemBackend: Send + Sync {
    async fn media(&self, action: MediaAction) -> anyhow::Result<()>;
    async fn volume(&self, action: VolumeAction) -> anyhow::Result<()>;
    async fn brightness(&self, action: BrightnessAction) -> anyhow::Result<()>;
    async fn clipboard_set(&self, text: &str) -> anyhow::Result<()>;
    async fn lock(&self) -> anyhow::Result<()>;
    /// Callers check `config.allow_suspend` first; this only performs it.
    async fn suspend(&self) -> anyhow::Result<()>;
}

/// Screen enumeration and encoded-frame production.
#[async_trait]
pub trait CaptureBackend: Send + Sync {
    fn name(&self) -> &'static str;

    async fn list_sources(&self) -> anyhow::Result<Vec<ScreenSource>>;

    /// Opens a producer for `source`, once per attached client.
    ///
    /// Setup failures — including falling back to a slower pipeline — belong
    /// here rather than mid-stream, because nothing has been written to the
    /// client yet at this point and the codec is still free to change.
    async fn open(
        &self,
        source: &ScreenSource,
        tuning: StreamTuning,
    ) -> anyhow::Result<Box<dyn FrameProducer>>;

    /// Why the preferred pipeline last fell back to a slower one, if it did.
    ///
    /// Surfaced in `start_screen_stream` so a broken fast path cannot sit
    /// unnoticed behind a fallback that merely looks slow.
    async fn last_error(&self) -> Option<String>;

    /// The codec `start_screen_stream` should advertise for `source`.
    ///
    /// Advisory only, and the protocol says so: the producer may still fall
    /// back before it writes the handshake line, and that line is what actually
    /// decides how the client must decode.
    async fn announced_codec(&self, source: &ScreenSource) -> String;

    /// The desktop-space origin to add to coordinates that arrive in `source`'s
    /// own space, or `None` when this backend captures in desktop space already.
    ///
    /// A backend that hands out monitor-local frames must report the monitor's
    /// origin here, otherwise pointing at a secondary monitor silently lands the
    /// pointer on the primary one.
    fn source_origin(&self, source: &ScreenSource) -> Option<(i32, i32)>;
}

/// What a producer emits, and therefore how the client must frame it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamEncoding {
    H264,
    Jpeg,
}

impl StreamEncoding {
    /// The handshake line written once, lazily, before the first envelope.
    pub fn magic(self) -> &'static [u8] {
        match self {
            Self::H264 => b"WAYPAD_STREAM_V2\n",
            Self::Jpeg => b"WAYPAD_STREAM_V1\n",
        }
    }

    pub fn codec(self) -> &'static str {
        match self {
            Self::H264 => "h264",
            Self::Jpeg => "jpeg",
        }
    }
}

/// One complete picture, plus the parameter sets that must precede it.
pub struct EncodedUnit {
    /// A whole JPEG, or one Annex-B H.264 access unit.
    pub data: Vec<u8>,
    /// True for every JPEG (each is independent) and for an H.264 IDR.
    pub key_frame: bool,
    /// SPS/PPS to send as a separate `config: true` envelope ahead of `data`.
    ///
    /// Repeated before every keyframe, which is what lets an Android client
    /// rebuild its decoder mid-stream when the app returns to the foreground.
    pub parameter_sets: Option<Vec<u8>>,
    pub geometry: FrameGeometry,
}

/// A live encoder attached to one client.
#[async_trait]
pub trait FrameProducer: Send {
    /// Decided once the producer is open, because a platform may only discover
    /// mid-setup that hardware encoding is unavailable and it must fall back.
    fn encoding(&self) -> StreamEncoding;

    /// Yields the next unit, or `None` once the producer is finished.
    ///
    /// Two guarantees the pump relies on, because it forwards units blindly:
    ///
    /// - The very first unit is decodable on its own — an IDR for H.264, and
    ///   trivially so for JPEG. A decoder joining a stream on a P-frame shows
    ///   nothing at all until the next IDR.
    /// - The same holds after any internal restart. A producer that rebuilds
    ///   its encoder must not emit the new pipeline's leading non-IDR frames.
    ///
    /// Must be cancel safe: the pump races this against a stop signal and a
    /// keyframe request, so a dropped future must not lose buffered data.
    async fn next_unit(&mut self) -> anyhow::Result<Option<EncodedUnit>>;

    /// Makes the next unit a keyframe, however this platform manages that.
    ///
    /// Some encoders take a force-keyframe flag; others have to be respawned.
    /// Either way it is the producer's business, including rate limiting a
    /// client that asks repeatedly — the cost of honouring a request differs by
    /// orders of magnitude between the two, so a single policy in the pump
    /// would be wrong for one of them.
    async fn request_key_frame(&mut self) -> anyhow::Result<()>;

    /// Releases the encoder and any capture session. Failures are logged, not
    /// propagated: the client is already gone by the time this runs.
    async fn shutdown(self: Box<Self>);
}

/// Desktop audio capture, streamed alongside the picture on the same socket.
#[async_trait]
pub trait AudioBackend: Send + Sync {
    fn name(&self) -> &'static str;

    /// Reports whether desktop audio can be captured, and why not when it
    /// cannot. Cheap enough to call during capability detection.
    async fn probe(&self) -> AudioCaptureCapability;

    async fn open(&self, options: AudioStreamOptions) -> anyhow::Result<Box<dyn AudioProducer>>;
}

/// A live Opus encoder fed by the desktop's own output.
#[async_trait]
pub trait AudioProducer: Send {
    fn format(&self) -> AudioFormat;

    /// The device the samples are coming from, for diagnostics.
    fn source_label(&self) -> Option<String>;

    /// Yields one bare Opus packet — no container, no RTP header.
    async fn next_packet(&mut self) -> anyhow::Result<Option<Vec<u8>>>;

    async fn shutdown(self: Box<Self>);
}

/// Stand-in for a controller backend on a host that cannot inject one.
///
/// Reporting the capability as false with a reason always beats omitting it:
/// the Android client shows the reason instead of leaving the user with a
/// control that silently does nothing.
pub struct UnsupportedController {
    reason: String,
}

impl UnsupportedController {
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }
}

impl ControllerBackend for UnsupportedController {
    fn name(&self) -> &'static str {
        "unsupported"
    }

    fn available(&self) -> bool {
        false
    }

    fn reason(&self) -> String {
        self.reason.clone()
    }

    fn device_connected(&mut self, _device_id: &str, _name: &str) -> anyhow::Result<()> {
        anyhow::bail!("{}", self.reason)
    }

    fn device_disconnected(&mut self, _device_id: &str) -> anyhow::Result<()> {
        // Unplugging a device the host never took is not an error: the phone
        // always reports the disconnect, whether or not it was ever forwarded.
        Ok(())
    }

    fn button(&mut self, _button: &str, _state: ButtonState) -> anyhow::Result<()> {
        anyhow::bail!("{}", self.reason)
    }

    fn axis(&mut self, _axis: &str, _value: f64) -> anyhow::Result<()> {
        anyhow::bail!("{}", self.reason)
    }

    fn flush_pending(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
}
