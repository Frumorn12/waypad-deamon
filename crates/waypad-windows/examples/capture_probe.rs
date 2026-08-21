//! Grabs one frame off DXGI Desktop Duplication and writes it as a BMP.
//!
//! Capture is the one part that cannot be judged from a return code: a pipeline
//! that reports success while handing over a black or shifted buffer looks
//! identical to a working one until someone looks at the picture.

#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    use waypad_windows::capture::{DuplicatedOutput, enumerate_outputs};
    use windows::Win32::Graphics::Direct3D11::{
        D3D11_CPU_ACCESS_READ, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_TEXTURE2D_DESC,
        D3D11_USAGE_STAGING, ID3D11Texture2D,
    };

    let outputs = enumerate_outputs()?;
    for output in &outputs {
        println!(
            "{}  {}x{} at ({},{}) primary={}",
            output.device_name, output.width, output.height, output.x, output.y, output.primary
        );
    }
    let target = outputs
        .iter()
        .find(|o| o.primary)
        .unwrap_or(&outputs[0])
        .clone();
    println!("duplicating {}", target.device_name);

    let mut duplicated = DuplicatedOutput::open(&target)?;

    // A still desktop produces no frames at all, so the first acquire commonly
    // times out. Nudging the mouse is not an option here; retrying is.
    let mut frame = None;
    for attempt in 1..=40 {
        if let Some(texture) = duplicated.acquire(250)? {
            println!("got a frame on attempt {attempt}");
            frame = Some(texture);
            break;
        }
    }
    let Some(frame) = frame else {
        anyhow::bail!(
            "no frame in 10s: the desktop never changed, which duplication reports as a timeout rather than an error"
        );
    };

    unsafe {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        frame.GetDesc(&mut desc);
        println!(
            "texture {}x{} format={:?} usage={:?}",
            desc.Width, desc.Height, desc.Format.0, desc.Usage.0
        );

        // Duplication surfaces live on the GPU with no CPU access, so reading
        // pixels needs a staging copy.
        let staging_desc = D3D11_TEXTURE2D_DESC {
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
            ..desc
        };
        let mut staging: Option<ID3D11Texture2D> = None;
        duplicated
            .device()
            .CreateTexture2D(&staging_desc, None, Some(&mut staging))?;
        let staging = staging.expect("staging texture");
        duplicated.context().CopyResource(&staging, &frame);

        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        duplicated
            .context()
            .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
        let width = desc.Width as usize;
        let height = desc.Height as usize;
        let stride = mapped.RowPitch as usize;
        let src = std::slice::from_raw_parts(mapped.pData as *const u8, stride * height);

        // 32bpp BMP, rows bottom-up, which is what the format wants anyway.
        let row_bytes = width * 4;
        let pixel_bytes = row_bytes * height;
        let mut bmp = Vec::with_capacity(54 + pixel_bytes);
        bmp.extend_from_slice(b"BM");
        bmp.extend_from_slice(&((54 + pixel_bytes) as u32).to_le_bytes());
        bmp.extend_from_slice(&0u32.to_le_bytes());
        bmp.extend_from_slice(&54u32.to_le_bytes());
        bmp.extend_from_slice(&40u32.to_le_bytes());
        bmp.extend_from_slice(&(width as i32).to_le_bytes());
        bmp.extend_from_slice(&(height as i32).to_le_bytes());
        bmp.extend_from_slice(&1u16.to_le_bytes());
        bmp.extend_from_slice(&32u16.to_le_bytes());
        bmp.extend_from_slice(&0u32.to_le_bytes());
        bmp.extend_from_slice(&(pixel_bytes as u32).to_le_bytes());
        bmp.extend_from_slice(&[0u8; 16]);

        let mut non_black = 0u64;
        for row in (0..height).rev() {
            let start = row * stride;
            let line = &src[start..start + row_bytes];
            non_black += line
                .chunks_exact(4)
                .filter(|p| p[0] | p[1] | p[2] != 0)
                .count() as u64;
            bmp.extend_from_slice(line);
        }
        duplicated.context().Unmap(&staging, 0);

        let path = std::env::args()
            .nth(1)
            .unwrap_or_else(|| "capture.bmp".into());
        std::fs::write(&path, &bmp)?;
        println!(
            "wrote {path} ({} bytes); {non_black} of {} pixels are not black",
            bmp.len(),
            width * height
        );
    }
    Ok(())
}

#[cfg(not(windows))]
fn main() -> anyhow::Result<()> {
    Ok(())
}
