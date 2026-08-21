//! Captures a few seconds of desktop, encodes it, and checks the bitstream.
//!
//! Validated with the same `AnnexBStreamReader` the daemon uses on both
//! platforms, so this exercises the exact code that will split the stream for a
//! phone rather than a second opinion that might disagree with it.

#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    use waypad_core::stream::AnnexBStreamReader;
    use waypad_windows::{
        capture::{DuplicatedOutput, enumerate_outputs},
        encoder::{EncoderSettings, H264Encoder},
        nv12::{Nv12Buffer, bgra_to_nv12},
    };
    use windows::Win32::Graphics::Direct3D11::{
        D3D11_CPU_ACCESS_READ, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_TEXTURE2D_DESC,
        D3D11_USAGE_STAGING, ID3D11Texture2D,
    };

    let frames_wanted: usize = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(60);
    let outputs = enumerate_outputs()?;
    let target = outputs
        .iter()
        .find(|o| o.primary)
        .unwrap_or(&outputs[0])
        .clone();

    // Downscaled, which is what a phone actually asks for.
    let (out_w, out_h) = (1280u32, 720u32);
    let mut duplicated = DuplicatedOutput::open(&target)?;
    let mut encoder = H264Encoder::new(EncoderSettings {
        width: out_w,
        height: out_h,
        fps: 30,
        bitrate_kbps: 6000,
        gop_size: 60,
    })?;
    println!("encoder: {}", encoder.name());

    let mut nv12 = Nv12Buffer::new(out_w, out_h);
    let mut reader = AnnexBStreamReader::new();
    let mut bitstream = Vec::new();
    let mut units = Vec::new();
    let mut staging: Option<ID3D11Texture2D> = None;
    let started = std::time::Instant::now();
    let mut captured = 0usize;

    while captured < frames_wanted && started.elapsed().as_secs() < 20 {
        let Some(frame) = duplicated.acquire(200)? else {
            continue;
        };
        unsafe {
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            frame.GetDesc(&mut desc);
            if staging.is_none() {
                let staging_desc = D3D11_TEXTURE2D_DESC {
                    Usage: D3D11_USAGE_STAGING,
                    BindFlags: 0,
                    CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                    MiscFlags: 0,
                    ..desc
                };
                let mut created = None;
                duplicated
                    .device()
                    .CreateTexture2D(&staging_desc, None, Some(&mut created))?;
                staging = created;
            }
            let staging = staging.as_ref().expect("staging texture");
            duplicated.context().CopyResource(staging, &frame);

            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            duplicated
                .context()
                .Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
            let src = std::slice::from_raw_parts(
                mapped.pData as *const u8,
                mapped.RowPitch as usize * desc.Height as usize,
            );
            bgra_to_nv12(
                src,
                desc.Width,
                desc.Height,
                mapped.RowPitch as usize,
                &mut nv12,
            )?;
            duplicated.context().Unmap(staging, 0);
        }
        // Released promptly: duplication hands out one surface at a time and
        // holding it stalls the desktop for everyone.
        duplicated.release()?;

        let encoded = encoder.encode(&nv12)?;
        if !encoded.is_empty() {
            bitstream.extend_from_slice(&encoded);
            units.extend(reader.push(&encoded));
        }
        captured += 1;
        if captured == 5 {
            encoder.request_key_frame()?;
            println!("asked for a keyframe after 5 frames");
        }
    }

    let elapsed = started.elapsed().as_secs_f64();
    let keyframes = units.iter().filter(|u| u.key_frame).count();
    let with_params = units.iter().filter(|u| u.parameter_sets.is_some()).count();
    println!(
        "captured {captured} frames in {elapsed:.1}s, {} bytes of bitstream",
        bitstream.len()
    );
    println!(
        "{} access units, {keyframes} keyframes, {with_params} carrying SPS/PPS",
        units.len()
    );
    // Against the video's own duration, not the wall clock: frames are encoded
    // as fast as they arrive here, so wall time would report a bitrate several
    // times the real one and make a correct encoder look broken.
    let video_seconds = captured as f64 / 30.0;
    println!(
        "average {:.0} kbit/s of video ({:.1}s of content encoded in {elapsed:.1}s wall)",
        (bitstream.len() as f64 * 8.0 / 1000.0) / video_seconds.max(0.001),
        video_seconds
    );

    anyhow::ensure!(!units.is_empty(), "the encoder produced no access units");
    anyhow::ensure!(keyframes > 0, "the stream contains no keyframe at all");
    anyhow::ensure!(
        units[0].key_frame,
        "the stream does not open on a keyframe, so a decoder joining it shows nothing"
    );
    anyhow::ensure!(
        units[0].parameter_sets.is_some(),
        "the first keyframe carries no SPS/PPS, so a decoder cannot be configured"
    );

    if let Some(path) = std::env::args().nth(2) {
        std::fs::write(&path, &bitstream)?;
        println!("wrote {path}");
    }
    println!("bitstream looks well formed");
    Ok(())
}

#[cfg(not(windows))]
fn main() -> anyhow::Result<()> {
    Ok(())
}
