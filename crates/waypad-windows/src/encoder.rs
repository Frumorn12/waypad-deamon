//! H.264 encoding through a Media Foundation Transform.
//!
//! Media Foundation rather than a bundled ffmpeg, for two reasons that both
//! show up in what the user gets: the installer stays around 6 MB instead of
//! 80, and forcing a keyframe is a property set on a live encoder rather than a
//! pipeline respawn. The Linux backend has to respawn `gst-launch` for that and
//! loses a few hundred milliseconds of stream each time; this does not.
//!
//! Only synchronous transforms are used so far. Most hardware encoders expose
//! themselves as asynchronous MFTs, which need the event-driven model and a
//! good deal more code; until that is written this runs on the Microsoft
//! software encoder, which handles 1080p30 comfortably and is reported honestly
//! through the capability reason rather than quietly.

use crate::nv12::Nv12Buffer;
use anyhow::{Context, bail};
use std::sync::OnceLock;
use tracing::{debug, info};
use windows::{
    Win32::Media::MediaFoundation::{
        CODECAPI_AVEncCommonMeanBitRate, CODECAPI_AVEncCommonRateControlMode,
        CODECAPI_AVEncMPVGOPSize, CODECAPI_AVEncVideoForceKeyFrame, ICodecAPI, IMFActivate,
        IMFSample, IMFTransform, MF_MT_AVG_BITRATE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE,
        MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE, MF_MT_MPEG2_PROFILE, MF_MT_PIXEL_ASPECT_RATIO,
        MF_MT_SUBTYPE, MF_VERSION, MFCreateMediaType, MFCreateMemoryBuffer, MFCreateSample,
        MFMediaType_Video, MFSTARTUP_NOSOCKET, MFStartup, MFT_CATEGORY_VIDEO_ENCODER,
        MFT_ENUM_FLAG_SORTANDFILTER, MFT_ENUM_FLAG_SYNCMFT, MFT_ENUM_FLAG_TRANSCODE_ONLY,
        MFT_FRIENDLY_NAME_Attribute, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING,
        MFT_MESSAGE_NOTIFY_END_OF_STREAM, MFT_MESSAGE_NOTIFY_START_OF_STREAM,
        MFT_OUTPUT_DATA_BUFFER, MFT_OUTPUT_STREAM_PROVIDES_SAMPLES, MFT_REGISTER_TYPE_INFO,
        MFTEnumEx, MFVideoFormat_H264, MFVideoFormat_NV12, MFVideoInterlace_Progressive,
        eAVEncCommonRateControlMode_CBR, eAVEncH264VProfile_Main,
    },
    Win32::System::Variant::{VARIANT, VT_UI4},
    core::{Interface, PWSTR},
};

/// Media Foundation must be started once per process, and starting it twice is
/// harmless but pointless.
fn ensure_mf_started() -> anyhow::Result<()> {
    static STARTED: OnceLock<Result<(), String>> = OnceLock::new();
    STARTED
        .get_or_init(|| {
            // SAFETY: documented as callable once per process before any other
            // Media Foundation call.
            unsafe { MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET) }.map_err(|err| err.to_string())
        })
        .clone()
        .map_err(|err| anyhow::anyhow!("MFStartup failed: {err}"))
}

/// Settings an encoder is created with. Changing any of them means a new
/// encoder, which is why they are taken together rather than as setters.
#[derive(Debug, Clone, Copy)]
pub struct EncoderSettings {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    pub gop_size: u32,
}

pub struct H264Encoder {
    transform: IMFTransform,
    codec_api: Option<ICodecAPI>,
    input_stream: u32,
    output_stream: u32,
    settings: EncoderSettings,
    /// True when the transform allocates its own output samples, which the
    /// caller must not then do.
    provides_samples: bool,
    output_size: u32,
    frame_index: i64,
    name: String,
}

impl std::fmt::Debug for H264Encoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("H264Encoder")
            .field("encoder", &self.name)
            .field("width", &self.settings.width)
            .field("height", &self.settings.height)
            .finish_non_exhaustive()
    }
}

impl H264Encoder {
    pub fn new(settings: EncoderSettings) -> anyhow::Result<Self> {
        ensure_mf_started()?;
        // SAFETY: every COM call is checked and the transform outlives this
        // function inside the returned struct.
        unsafe {
            let (transform, name) = find_encoder()?;
            let (input_stream, output_stream) = stream_ids(&transform)?;

            // Output type first. An H.264 encoder cannot validate an input type
            // before it knows what it is being asked to produce, and setting
            // them the other way round fails with an unhelpful error.
            let output_type = MFCreateMediaType()?;
            output_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            output_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
            output_type.SetUINT32(&MF_MT_AVG_BITRATE, settings.bitrate_kbps * 1000)?;
            output_type.SetUINT64(
                &MF_MT_FRAME_SIZE,
                pack_pair(settings.width, settings.height),
            )?;
            output_type.SetUINT64(&MF_MT_FRAME_RATE, pack_pair(settings.fps, 1))?;
            output_type.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack_pair(1, 1))?;
            output_type.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            // Main rather than High: every Android decoder supports it, and the
            // compression difference at this bitrate is not worth the risk of a
            // phone that cannot play the stream at all.
            output_type.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_Main.0 as u32)?;
            transform
                .SetOutputType(output_stream, &output_type, 0)
                .context("the H.264 encoder rejected the requested output format")?;

            let input_type = MFCreateMediaType()?;
            input_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            input_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
            input_type.SetUINT64(
                &MF_MT_FRAME_SIZE,
                pack_pair(settings.width, settings.height),
            )?;
            input_type.SetUINT64(&MF_MT_FRAME_RATE, pack_pair(settings.fps, 1))?;
            input_type.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack_pair(1, 1))?;
            input_type.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            transform
                .SetInputType(input_stream, &input_type, 0)
                .context("the H.264 encoder rejected NV12 input")?;

            let codec_api: Option<ICodecAPI> = transform.cast().ok();
            if let Some(api) = &codec_api {
                // CBR keeps the bandwidth a phone on Wi-Fi has to absorb
                // predictable; a variable rate spikes exactly when the screen
                // changes most, which is when the link is already busiest.
                let _ = api.SetValue(
                    &CODECAPI_AVEncCommonRateControlMode,
                    &u32_variant(eAVEncCommonRateControlMode_CBR.0 as u32),
                );
                let _ = api.SetValue(
                    &CODECAPI_AVEncCommonMeanBitRate,
                    &u32_variant(settings.bitrate_kbps * 1000),
                );
                let _ = api.SetValue(&CODECAPI_AVEncMPVGOPSize, &u32_variant(settings.gop_size));
            }

            let stream_info = transform.GetOutputStreamInfo(output_stream)?;
            let provides_samples =
                stream_info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0;

            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;

            info!(
                encoder = %name,
                width = settings.width,
                height = settings.height,
                fps = settings.fps,
                bitrate_kbps = settings.bitrate_kbps,
                provides_samples,
                "H.264 encoder ready"
            );
            Ok(Self {
                transform,
                codec_api,
                input_stream,
                output_stream,
                settings,
                provides_samples,
                // A frame that compresses badly can exceed the suggested size,
                // so the buffer is generous rather than exact.
                output_size: stream_info
                    .cbSize
                    .max(settings.width * settings.height * 2)
                    .max(1 << 20),
                frame_index: 0,
                name,
            })
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn settings(&self) -> EncoderSettings {
        self.settings
    }

    /// Feeds one picture and returns whatever Annex-B bytes came out.
    ///
    /// An encoder may return nothing for a frame — it buffers to make rate
    /// control work — so an empty result is normal and not an error.
    pub fn encode(&mut self, frame: &Nv12Buffer) -> anyhow::Result<Vec<u8>> {
        // 100-nanosecond units, which is Media Foundation's time base
        // everywhere. Derived from the frame index rather than a clock so the
        // encoder sees the constant rate its rate control was configured for.
        let ticks_per_frame = 10_000_000i64 / i64::from(self.settings.fps.max(1));
        let timestamp = self.frame_index * ticks_per_frame;
        self.frame_index += 1;

        // SAFETY: the sample and its buffer are owned here; the transform only
        // borrows them for the duration of ProcessInput.
        unsafe {
            let sample = self.make_sample(&frame.data, timestamp, ticks_per_frame)?;
            self.transform
                .ProcessInput(self.input_stream, &sample, 0)
                .context("the H.264 encoder refused a frame")?;
            self.drain()
        }
    }

    /// Makes the next output frame a keyframe.
    ///
    /// A property on the running encoder, so unlike the GStreamer path this
    /// costs nothing and leaves no gap in the stream.
    pub fn request_key_frame(&mut self) -> anyhow::Result<()> {
        let Some(api) = &self.codec_api else {
            bail!("this encoder exposes no ICodecAPI, so a keyframe cannot be forced");
        };
        // SAFETY: the variant is owned by this frame and copied by SetValue.
        unsafe { api.SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &u32_variant(1)) }
            .context("forcing a keyframe failed")?;
        debug!("forced an H.264 keyframe in place");
        Ok(())
    }

    unsafe fn make_sample(
        &self,
        data: &[u8],
        timestamp: i64,
        duration: i64,
    ) -> anyhow::Result<IMFSample> {
        unsafe {
            let buffer = MFCreateMemoryBuffer(data.len() as u32)?;
            let mut target: *mut u8 = std::ptr::null_mut();
            let mut max_len = 0u32;
            buffer.Lock(&mut target, Some(&mut max_len), None)?;
            if (max_len as usize) < data.len() {
                let _ = buffer.Unlock();
                bail!("media buffer is {max_len} bytes, needs {}", data.len());
            }
            std::ptr::copy_nonoverlapping(data.as_ptr(), target, data.len());
            buffer.Unlock()?;
            buffer.SetCurrentLength(data.len() as u32)?;

            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime(timestamp)?;
            sample.SetSampleDuration(duration)?;
            Ok(sample)
        }
    }

    /// Pulls every output the transform is holding.
    unsafe fn drain(&mut self) -> anyhow::Result<Vec<u8>> {
        let mut encoded = Vec::new();
        loop {
            let mut buffers = [MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: self.output_stream,
                pSample: std::mem::ManuallyDrop::new(None),
                dwStatus: 0,
                pEvents: std::mem::ManuallyDrop::new(None),
            }];
            if !self.provides_samples {
                // SAFETY: the sample is handed to the transform and reclaimed
                // below in the same iteration.
                let sample = unsafe {
                    let buffer = MFCreateMemoryBuffer(self.output_size)?;
                    let sample = MFCreateSample()?;
                    sample.AddBuffer(&buffer)?;
                    sample
                };
                buffers[0].pSample = std::mem::ManuallyDrop::new(Some(sample));
            }

            let mut status = 0u32;
            // SAFETY: the buffer array is valid for the call and its samples
            // are released before it is reused.
            let result = unsafe { self.transform.ProcessOutput(0, &mut buffers, &mut status) };
            match result {
                Ok(()) => {}
                Err(err) if err.code() == windows::Win32::Media::MediaFoundation::MF_E_TRANSFORM_NEED_MORE_INPUT => {
                    // The ordinary way a drain ends: the encoder wants another
                    // picture before it will produce anything more.
                    release_buffer(&mut buffers[0]);
                    break;
                }
                Err(err) => {
                    release_buffer(&mut buffers[0]);
                    bail!("the H.264 encoder failed to produce output: {err}");
                }
            }

            // SAFETY: both fields are ManuallyDrop wrappers this loop filled in
            // and has not taken from yet on this iteration.
            let (sample, events) = unsafe {
                (
                    std::mem::ManuallyDrop::take(&mut buffers[0].pSample),
                    std::mem::ManuallyDrop::take(&mut buffers[0].pEvents),
                )
            };
            drop(events);
            let Some(sample) = sample else { continue };

            // SAFETY: the sample is owned here and unlocked before it drops.
            unsafe {
                let buffer = sample.ConvertToContiguousBuffer()?;
                let mut data: *mut u8 = std::ptr::null_mut();
                let mut length = 0u32;
                buffer.Lock(&mut data, None, Some(&mut length))?;
                encoded.extend_from_slice(std::slice::from_raw_parts(data, length as usize));
                buffer.Unlock()?;
            }
        }
        Ok(encoded)
    }
}

impl Drop for H264Encoder {
    fn drop(&mut self) {
        // SAFETY: the transform is still alive; this only tells it no further
        // input is coming so it can release its own resources.
        unsafe {
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
        }
    }
}

/// Releases whatever a `ProcessOutput` attempt left in the buffer.
fn release_buffer(buffer: &mut MFT_OUTPUT_DATA_BUFFER) {
    // SAFETY: both fields are ManuallyDrop wrappers this code put there.
    unsafe {
        drop(std::mem::ManuallyDrop::take(&mut buffer.pSample));
        drop(std::mem::ManuallyDrop::take(&mut buffer.pEvents));
    }
}

/// Media Foundation packs two 32-bit values into one 64-bit attribute, high
/// half first. Used for frame size, frame rate, and aspect ratio alike.
fn pack_pair(high: u32, low: u32) -> u64 {
    (u64::from(high) << 32) | u64::from(low)
}

fn u32_variant(value: u32) -> VARIANT {
    let mut variant = VARIANT::default();
    // SAFETY: writing the two fields a VT_UI4 variant consists of. No
    // allocation is involved, so no VariantClear is needed either.
    unsafe {
        let inner = &mut variant.Anonymous.Anonymous;
        inner.vt = VT_UI4;
        inner.Anonymous.ulVal = value;
    }
    variant
}

/// Finds a synchronous H.264 encoder, preferring whatever the system ranks
/// first once filtered.
unsafe fn find_encoder() -> anyhow::Result<(IMFTransform, String)> {
    unsafe {
        let input = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: MFVideoFormat_NV12,
        };
        let output = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: MFVideoFormat_H264,
        };
        let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut count = 0u32;
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_TRANSCODE_ONLY | MFT_ENUM_FLAG_SORTANDFILTER,
            Some(&input),
            Some(&output),
            &mut activates,
            &mut count,
        )
        .context("no H.264 encoder is registered on this system")?;
        if count == 0 || activates.is_null() {
            bail!(
                "Windows registered no synchronous H.264 encoder. Most hardware encoders are \
                 asynchronous transforms, which Waypad does not drive yet."
            );
        }

        let found = std::slice::from_raw_parts(activates, count as usize);
        let mut chosen = None;
        for candidate in found.iter().flatten() {
            let name = encoder_name(candidate);
            if chosen.is_none()
                && let Ok(transform) = candidate.ActivateObject::<IMFTransform>()
            {
                debug!(encoder = %name, "selected H.264 encoder");
                chosen = Some((transform, name));
            } else {
                debug!(encoder = %name, "skipping H.264 encoder");
            }
        }
        windows::Win32::System::Com::CoTaskMemFree(Some(activates as *const _));
        chosen.context("every registered H.264 encoder refused to activate")
    }
}

/// Reads an MFT's friendly name, freeing the string Media Foundation allocated.
///
/// Purely for logs and the capability reason, so an unreadable name is reported
/// as unknown rather than failing the encoder selection.
unsafe fn encoder_name(activate: &IMFActivate) -> String {
    unsafe {
        let mut text = PWSTR::null();
        let mut len = 0u32;
        if activate
            .GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut text, &mut len)
            .is_err()
            || text.is_null()
        {
            return "unnamed encoder".into();
        }
        let name = String::from_utf16_lossy(std::slice::from_raw_parts(text.0, len as usize));
        windows::Win32::System::Com::CoTaskMemFree(Some(text.0 as *const _));
        name
    }
}

/// Encoders normally expose exactly one input and one output stream, but the
/// ids are not required to be zero.
unsafe fn stream_ids(transform: &IMFTransform) -> anyhow::Result<(u32, u32)> {
    unsafe {
        let mut input_count = 0u32;
        let mut output_count = 0u32;
        transform.GetStreamCount(&mut input_count, &mut output_count)?;
        if input_count == 0 || output_count == 0 {
            bail!(
                "the H.264 encoder exposes {input_count} input and {output_count} output streams"
            );
        }
        let mut inputs = vec![0u32; input_count as usize];
        let mut outputs = vec![0u32; output_count as usize];
        match transform.GetStreamIDs(&mut inputs, &mut outputs) {
            Ok(()) => Ok((inputs[0], outputs[0])),
            // E_NOTIMPL here is not a failure: it means the ids are simply the
            // stream indices, which is what most encoders do.
            Err(_) => Ok((0, 0)),
        }
    }
}
