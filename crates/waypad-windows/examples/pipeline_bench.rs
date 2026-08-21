//! Measures where a frame's time actually goes, stage by stage.
//!
//! "It only manages 24 fps" is not actionable; "the staging copy costs 18 ms
//! and the encoder 6 ms" is. Run it while something is moving on screen — a
//! still desktop produces no frames and measures nothing.

#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    use std::time::{Duration, Instant};
    use waypad_windows::{
        capture::{DuplicatedOutput, enumerate_outputs},
        encoder::{EncoderSettings, H264Encoder},
        nv12::{Nv12Buffer, bgra_to_nv12},
    };
    use windows::Win32::Graphics::Direct3D11::{
        D3D11_CPU_ACCESS_READ, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_TEXTURE2D_DESC,
        D3D11_USAGE_STAGING, ID3D11Texture2D,
    };

    let width: u32 = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(1280);
    let height: u32 = std::env::args()
        .nth(2)
        .and_then(|a| a.parse().ok())
        .unwrap_or(720);
    let seconds: u64 = std::env::args()
        .nth(3)
        .and_then(|a| a.parse().ok())
        .unwrap_or(6);

    let outputs = enumerate_outputs()?;
    let target = outputs
        .iter()
        .find(|o| o.primary)
        .unwrap_or(&outputs[0])
        .clone();
    let mut duplicated = DuplicatedOutput::open(&target)?;
    let mut encoder = H264Encoder::new(EncoderSettings {
        width,
        height,
        fps: 60,
        bitrate_kbps: 8000,
        gop_size: 120,
    })?;
    println!(
        "source {}x{} -> encode {width}x{height}, encoder: {}",
        target.width,
        target.height,
        encoder.name()
    );

    let mut nv12 = Nv12Buffer::new(width, height);
    let mut staging: Option<ID3D11Texture2D> = None;
    let (mut acquire, mut copy) = (Vec::new(), Vec::new());
    let mut pixels: Vec<u8> = Vec::new();
    let (mut src_w, mut src_h, mut src_stride) = (0u32, 0u32, 0usize);

    // Phase one: real frames, however many the desktop happens to produce.
    // This is the only way to time the parts that need the GPU, and it is also
    // why the loop gives up rather than waiting forever on a still screen.
    let deadline = Instant::now() + Duration::from_secs(seconds);
    while Instant::now() < deadline {
        let t0 = Instant::now();
        let Some(frame) = duplicated.acquire(200)? else {
            continue;
        };
        acquire.push(t0.elapsed());
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
            let staging = staging.as_ref().unwrap();

            let t1 = Instant::now();
            duplicated.context().CopyResource(staging, &frame);
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            duplicated
                .context()
                .Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
            copy.push(t1.elapsed());

            let bytes = mapped.RowPitch as usize * desc.Height as usize;
            let src = std::slice::from_raw_parts(mapped.pData as *const u8, bytes);
            // Kept so phase two can run without holding a duplication surface,
            // which would stall the desktop for every application on the machine.
            pixels = src.to_vec();
            (src_w, src_h, src_stride) = (desc.Width, desc.Height, mapped.RowPitch as usize);
            duplicated.context().Unmap(staging, 0);
        }
        duplicated.release()?;
        if copy.len() >= 4 {
            break;
        }
    }

    // The GPU-to-CPU readback measured above gets at most a handful of samples,
    // because it needs the desktop to actually change. Timing the same
    // operation on a texture of our own gives the cost of moving an
    // uncompressed frame across the bus without waiting for someone to move a
    // window, which is the number that decides whether 60 fps is reachable.
    let mut readback = Vec::new();
    unsafe {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: target.width,
            Height: target.height,
            MipLevels: 1,
            ArraySize: 1,
            Format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: windows::Win32::Graphics::Direct3D11::D3D11_USAGE_DEFAULT,
            ..Default::default()
        };
        let mut source = None;
        duplicated
            .device()
            .CreateTexture2D(&desc, None, Some(&mut source))?;
        let source = source.unwrap();
        let staging_desc = D3D11_TEXTURE2D_DESC {
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
            ..desc
        };
        let mut sink = None;
        duplicated
            .device()
            .CreateTexture2D(&staging_desc, None, Some(&mut sink))?;
        let sink = sink.unwrap();

        for _ in 0..120 {
            let t = Instant::now();
            duplicated.context().CopyResource(&sink, &source);
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            duplicated
                .context()
                .Map(&sink, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
            // Touched so the read is not optimised into nothing and the pages
            // are actually faulted in, which is what the real path does.
            let probe = std::ptr::read_volatile(mapped.pData as *const u8);
            std::hint::black_box(probe);
            duplicated.context().Unmap(&sink, 0);
            readback.push(t.elapsed());
        }
    }

    if pixels.is_empty() {
        anyhow::bail!(
            "the desktop never changed, so nothing was captured. Run this with something moving on screen."
        );
    }

    // Phase two: the CPU stages, timed on a fixed frame so the numbers do not
    // depend on how busy the screen was.
    const ROUNDS: usize = 240;
    let mut convert = Vec::with_capacity(ROUNDS);
    let mut encode = Vec::with_capacity(ROUNDS);
    let mut bytes = 0usize;
    for _ in 0..ROUNDS {
        let t = Instant::now();
        bgra_to_nv12(&pixels, src_w, src_h, src_stride, &mut nv12)?;
        convert.push(t.elapsed());
        let t = Instant::now();
        bytes += encoder.encode(&nv12)?.len();
        encode.push(t.elapsed());
    }

    let report = |name: &str, mut samples: Vec<Duration>| -> f64 {
        if samples.is_empty() {
            println!("{name:>16}: no samples");
            return 0.0;
        }
        samples.sort();
        let ms = |d: Duration| d.as_secs_f64() * 1000.0;
        // The mean skips the first few: an encoder's first frames include its
        // own warm-up and a keyframe, neither of which recurs.
        let warm = &samples[samples.len() / 10..];
        let mean = warm.iter().map(|d| ms(*d)).sum::<f64>() / warm.len() as f64;
        println!(
            "{name:>16}: mean {mean:6.2} ms   p50 {:6.2} ms   p95 {:6.2} ms   ({} samples)",
            ms(samples[samples.len() / 2]),
            ms(samples[samples.len() * 95 / 100]),
            samples.len()
        );
        mean
    };

    println!(
        "
source {src_w}x{src_h}, stride {src_stride} bytes
"
    );
    let a = report("acquire", acquire);
    let _ = report("gpu->cpu (live)", copy);
    let c = report("gpu->cpu copy", readback);
    let m = report("bgra->nv12", convert);
    let e = report("h264 encode", encode);
    let total = a + c + m + e;
    println!(
        "
{:>16}: {total:6.2} ms  ->  ceiling {:.0} fps",
        "per frame",
        1000.0 / total.max(0.001)
    );
    println!(
        "{:>16}: {:.0} kbit/s if fed at 60 fps",
        "bitrate",
        bytes as f64 / ROUNDS as f64 * 8.0 * 60.0 / 1000.0
    );
    Ok(())
}

#[cfg(not(windows))]
fn main() -> anyhow::Result<()> {
    Ok(())
}
