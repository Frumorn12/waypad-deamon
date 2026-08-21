//! The encoder knobs a client negotiates, and the clamps that keep them sane.
//!
//! Every bound here is part of the published protocol (`docs/PROTOCOL.md`), so
//! a platform backend reads these values rather than inventing its own: two
//! hosts answering the same `start_screen_stream` must agree on what they were
//! asked for.

/// Resolved per-session encoder settings, after clamping.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StreamTuning {
    pub fps: u32,
    pub quality: u8,
    pub bitrate_kbps: Option<u32>,
    pub max_width: Option<u32>,
    pub max_height: Option<u32>,
}

impl StreamTuning {
    /// Applies the protocol clamps to what the client asked for.
    ///
    /// Absent values take the documented defaults rather than the source's
    /// native ones: a client that omits `max_fps` wants a sensible stream, not
    /// an uncapped one.
    pub fn resolve(
        max_fps: Option<u32>,
        jpeg_quality: Option<u8>,
        bitrate_kbps: Option<u32>,
        max_width: Option<u32>,
        max_height: Option<u32>,
    ) -> Self {
        Self {
            fps: max_fps.unwrap_or(30).clamp(1, 60),
            quality: jpeg_quality.unwrap_or(70).clamp(35, 92),
            bitrate_kbps,
            max_width: max_width.map(|value| value.clamp(480, 3840)),
            max_height: max_height.map(|value| value.clamp(480, 3840)),
        }
    }
}

const DEFAULT_BITRATE_WIDTH: u32 = 1920;
const DEFAULT_BITRATE_HEIGHT: u32 = 1080;

/// Resolves the CBR target. `bitrate_kbps` wins when the client sends it;
/// otherwise the legacy `jpeg_quality` knob is mapped onto bits per pixel so
/// older clients still get a sane H.264 stream (1080p30 at quality 70 lands
/// around 7 Mbit/s instead of the 45-90 Mbit/s the MJPEG path used).
pub fn resolve_bitrate_kbps(
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
pub fn keyframe_interval(fps: u32) -> u32 {
    (fps.max(1) * 2).clamp(15, 120)
}

/// H.264 encoders want even dimensions; odd ones are rounded down.
pub fn even_dimension(value: u32) -> u32 {
    value.max(2) & !1
}

pub fn capture_scale(
    width: u32,
    height: u32,
    max_width: Option<u32>,
    max_height: Option<u32>,
) -> f64 {
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

/// The size to encode at, or `(None, None)` when the source already fits.
pub fn target_dimensions(
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

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn resolve_applies_the_documented_protocol_clamps() {
        let tuning = StreamTuning::resolve(Some(240), Some(200), None, Some(99), Some(9999));
        assert_eq!(tuning.fps, 60);
        assert_eq!(tuning.quality, 92);
        assert_eq!(tuning.max_width, Some(480));
        assert_eq!(tuning.max_height, Some(3840));

        // A client that sends nothing gets the documented defaults.
        let default = StreamTuning::resolve(None, None, None, None, None);
        assert_eq!(default.fps, 30);
        assert_eq!(default.quality, 70);
        assert_eq!(default.max_width, None);
    }
}
