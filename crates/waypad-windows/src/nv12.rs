//! BGRA to NV12 conversion, with downscaling folded in.
//!
//! Every H.264 encoder on Windows wants NV12; Desktop Duplication produces
//! BGRA. Something has to convert, and the two obvious candidates are the
//! Video Processor MFT and a D3D11 video processor, both of which would do it
//! on the GPU and neither of which can be checked without looking at pictures.
//!
//! This does it on the CPU instead, which costs a few milliseconds a frame at
//! 1080p and buys something worth more at this stage: the maths is ordinary
//! Rust and every coefficient is pinned by a test. Moving it onto the GPU later
//! is a contained change, and these tests become the reference for it.
//!
//! BT.709 limited range, because that is what a decoder assumes for HD when
//! nothing says otherwise, and what the media type is tagged with.

/// A destination buffer sized for one NV12 frame.
///
/// Reused across frames: at 1080p this is 3 MB, and reallocating it sixty times
/// a second is pure waste.
#[derive(Debug, Clone)]
pub struct Nv12Buffer {
    pub data: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

impl Nv12Buffer {
    /// H.264 needs even dimensions, and so does 4:2:0 chroma subsampling: an
    /// odd size has no whole chroma sample to describe its last row or column.
    pub fn new(width: u32, height: u32) -> Self {
        let width = width.max(2) & !1;
        let height = height.max(2) & !1;
        Self {
            data: vec![0; nv12_len(width, height)],
            width,
            height,
        }
    }

    fn luma_len(&self) -> usize {
        (self.width as usize) * (self.height as usize)
    }
}

pub fn nv12_len(width: u32, height: u32) -> usize {
    let luma = (width as usize) * (height as usize);
    // The chroma plane is one interleaved sample pair per 2x2 block, so half
    // the luma size.
    luma + luma / 2
}

/// Converts a BGRA image into `dst`, scaling if the sizes differ.
///
/// `src_stride` is in bytes and is usually wider than `src_width * 4`: GPU
/// staging buffers pad their rows, and reading past the padding is what turns a
/// correct converter into a sheared picture.
///
/// Scaling is nearest-neighbour, chosen for cost rather than quality. The
/// picture is about to be thrown at a lossy encoder at a bitrate that dominates
/// the visible result, and a box filter here would cost more than it returns.
pub fn bgra_to_nv12(
    src: &[u8],
    src_width: u32,
    src_height: u32,
    src_stride: usize,
    dst: &mut Nv12Buffer,
) -> anyhow::Result<()> {
    if src_width == 0 || src_height == 0 {
        anyhow::bail!("source image is empty ({src_width}x{src_height})");
    }
    let needed = src_stride * (src_height as usize);
    if src.len() < needed {
        anyhow::bail!(
            "source buffer is {} bytes, needs {needed} for {src_height} rows of stride {src_stride}",
            src.len()
        );
    }
    if src_stride < (src_width as usize) * 4 {
        anyhow::bail!("source stride {src_stride} is narrower than {src_width} BGRA pixels");
    }

    let dst_width = dst.width as usize;
    let dst_height = dst.height as usize;
    let luma_len = dst.luma_len();
    let (luma, chroma) = dst.data.split_at_mut(luma_len);

    for y in 0..dst_height {
        // Sampled at the centre of the destination pixel rather than its corner,
        // which keeps a downscaled picture from drifting half a pixel up and left.
        let src_y =
            ((y * 2 + 1) * (src_height as usize) / (dst_height * 2)).min(src_height as usize - 1);
        let src_row = &src[src_y * src_stride..];
        let luma_row = &mut luma[y * dst_width..(y + 1) * dst_width];

        for (x, luma_out) in luma_row.iter_mut().enumerate() {
            let src_x =
                ((x * 2 + 1) * (src_width as usize) / (dst_width * 2)).min(src_width as usize - 1);
            let pixel = &src_row[src_x * 4..src_x * 4 + 4];
            let (b, g, r) = (pixel[0] as i32, pixel[1] as i32, pixel[2] as i32);
            *luma_out = luma_709(r, g, b);

            // One chroma pair per 2x2 block, taken from the block's top-left
            // pixel. Averaging the four would be more correct and is not worth
            // the read amplification at this bitrate.
            if y % 2 == 0 && x % 2 == 0 {
                let index = (y / 2) * dst_width + x;
                chroma[index] = chroma_u_709(r, g, b);
                chroma[index + 1] = chroma_v_709(r, g, b);
            }
        }
    }
    Ok(())
}

// BT.709 limited range in fixed point, 1/256ths.
//
// The luma weights round to 220 where the limited-range span is 219; the
// truncation in the shift removes the extra count, so black and white still
// land exactly on 16 and 235. Each chroma triple sums to exactly zero, which is what
// makes a grey pixel come out at precisely 128 rather than a count off. That
// constraint is why the blue U weight is 113 and not the 112 that naive
// rounding of 0.4392 gives: with 112 the triple sums to -1, and every neutral
// colour in the picture picks up a faint chroma tint.
const Y_R: i32 = 47;
const Y_G: i32 = 157;
const Y_B: i32 = 16;
const U_R: i32 = -26;
const U_G: i32 = -87;
const U_B: i32 = 113;
const V_R: i32 = 112;
const V_G: i32 = -102;
const V_B: i32 = -10;

fn luma_709(r: i32, g: i32, b: i32) -> u8 {
    (((Y_R * r + Y_G * g + Y_B * b + 128) >> 8) + 16).clamp(16, 235) as u8
}

fn chroma_u_709(r: i32, g: i32, b: i32) -> u8 {
    (((U_R * r + U_G * g + U_B * b + 128) >> 8) + 128).clamp(16, 240) as u8
}

fn chroma_v_709(r: i32, g: i32, b: i32) -> u8 {
    (((V_R * r + V_G * g + V_B * b + 128) >> 8) + 128).clamp(16, 240) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a BGRA image of one solid colour, with padded rows so the stride
    /// handling is exercised on every test rather than only the one that means to.
    fn solid(width: u32, height: u32, r: u8, g: u8, b: u8) -> (Vec<u8>, usize) {
        let stride = (width as usize) * 4 + 64;
        let mut data = vec![0u8; stride * height as usize];
        for y in 0..height as usize {
            for x in 0..width as usize {
                let at = y * stride + x * 4;
                data[at] = b;
                data[at + 1] = g;
                data[at + 2] = r;
                data[at + 3] = 255;
            }
        }
        (data, stride)
    }

    fn convert(width: u32, height: u32, r: u8, g: u8, b: u8) -> Nv12Buffer {
        let (src, stride) = solid(width, height, r, g, b);
        let mut dst = Nv12Buffer::new(width, height);
        bgra_to_nv12(&src, width, height, stride, &mut dst).unwrap();
        dst
    }

    #[test]
    fn black_and_white_hit_the_limited_range_endpoints() {
        // Limited range is the whole reason these are not 0 and 255. A decoder
        // told the range is limited stretches 16..235 back out to full black
        // and full white; feeding it 0 would come back as crushed black.
        let black = convert(16, 16, 0, 0, 0);
        assert_eq!(black.data[0], 16);
        let white = convert(16, 16, 255, 255, 255);
        assert_eq!(white.data[0], 235);

        // Neutral colours carry no chroma, so both components sit at the
        // midpoint. A converter with swapped coefficients fails here first.
        let luma_len = 16 * 16;
        assert_eq!(black.data[luma_len], 128);
        assert_eq!(black.data[luma_len + 1], 128);
        assert_eq!(white.data[luma_len], 128);
        assert_eq!(white.data[luma_len + 1], 128);
    }

    #[test]
    fn primaries_land_where_bt709_says() {
        let luma_len = 16 * 16;

        // Green is the brightest primary under BT.709 and red is brighter than
        // blue. Getting the coefficients in the wrong order still produces a
        // picture, just one with the wrong contrast, so the ordering is pinned.
        let red = convert(16, 16, 255, 0, 0);
        let green = convert(16, 16, 0, 255, 0);
        let blue = convert(16, 16, 0, 0, 255);
        assert!(
            green.data[0] > red.data[0] && red.data[0] > blue.data[0],
            "luma order: green {} red {} blue {}",
            green.data[0],
            red.data[0],
            blue.data[0]
        );

        // Blue pushes U up and red pushes V up. Swapping the two planes is the
        // classic NV12 mistake and turns skin tones blue.
        assert!(
            blue.data[luma_len] > 200,
            "blue U = {}",
            blue.data[luma_len]
        );
        assert!(
            red.data[luma_len + 1] > 200,
            "red V = {}",
            red.data[luma_len + 1]
        );
        assert!(blue.data[luma_len + 1] < 128);
        assert!(red.data[luma_len] < 128);
    }

    #[test]
    fn the_chroma_weights_cancel_so_grey_stays_grey() {
        // Stated directly rather than only observed through a converted pixel:
        // a triple that does not sum to zero tints every neutral colour, which
        // is subtle enough on a screenshot to survive review.
        assert_eq!(U_R + U_G + U_B, 0);
        assert_eq!(V_R + V_G + V_B, 0);
        // Luma has no such exact identity to assert: the weights round to 220
        // rather than the 219 of the limited-range span, and the truncation in
        // the shift takes the extra count back off. So the endpoints are
        // asserted directly, which is the property that actually matters.
        assert_eq!(luma_709(0, 0, 0), 16);
        assert_eq!(luma_709(255, 255, 255), 235);
    }

    #[test]
    fn every_grey_converts_without_a_chroma_tint() {
        let luma_len = 16 * 16;
        for level in [0u8, 32, 64, 128, 192, 255] {
            let grey = convert(16, 16, level, level, level);
            assert_eq!(grey.data[luma_len], 128, "U at grey {level}");
            assert_eq!(grey.data[luma_len + 1], 128, "V at grey {level}");
        }
    }

    #[test]
    fn buffer_is_exactly_the_size_nv12_requires() {
        let buffer = Nv12Buffer::new(1920, 1080);
        assert_eq!(buffer.data.len(), 1920 * 1080 * 3 / 2);
        assert_eq!(buffer.data.len(), nv12_len(1920, 1080));
    }

    #[test]
    fn odd_dimensions_are_rounded_down_to_even() {
        // 4:2:0 has no way to describe an odd last row or column, and an
        // encoder handed one refuses to negotiate.
        let buffer = Nv12Buffer::new(1919, 1081);
        assert_eq!((buffer.width, buffer.height), (1918, 1080));
        assert_eq!(buffer.data.len(), nv12_len(1918, 1080));
    }

    #[test]
    fn downscaling_preserves_a_solid_colour() {
        let (src, stride) = solid(1920, 1080, 200, 100, 50);
        let mut dst = Nv12Buffer::new(640, 360);
        bgra_to_nv12(&src, 1920, 1080, stride, &mut dst).unwrap();
        let reference = convert(16, 16, 200, 100, 50);
        // Every sample of a solid image must come out identical no matter how
        // far it was scaled, which catches an index that walks off its row.
        assert!(
            dst.data[..640 * 360]
                .iter()
                .all(|y| *y == reference.data[0]),
            "luma plane is not uniform after downscaling"
        );
    }

    #[test]
    fn samples_the_right_pixels_when_scaling_by_half() {
        // A two-colour image: left half red, right half blue. Halved, the
        // result must still be half red and half blue — an off-by-one in the
        // sampling index shows up as a column of the wrong colour at the seam.
        let (width, height) = (64u32, 4u32);
        let stride = (width as usize) * 4 + 16;
        let mut src = vec![0u8; stride * height as usize];
        for y in 0..height as usize {
            for x in 0..width as usize {
                let at = y * stride + x * 4;
                let red = x < (width as usize) / 2;
                src[at] = if red { 0 } else { 255 };
                src[at + 2] = if red { 255 } else { 0 };
                src[at + 3] = 255;
            }
        }
        let mut dst = Nv12Buffer::new(32, 2);
        bgra_to_nv12(&src, width, height, stride, &mut dst).unwrap();

        let red_luma = luma_709(255, 0, 0);
        let blue_luma = luma_709(0, 0, 255);
        for x in 0..32usize {
            let expected = if x < 16 { red_luma } else { blue_luma };
            assert_eq!(dst.data[x], expected, "column {x}");
        }
    }

    #[test]
    fn rejects_a_buffer_that_cannot_hold_the_image_it_claims() {
        // A short buffer would otherwise be read past the end, which is the one
        // failure here that is not merely a wrong picture.
        let mut dst = Nv12Buffer::new(16, 16);
        let err = bgra_to_nv12(&[0u8; 16], 1920, 1080, 1920 * 4, &mut dst).unwrap_err();
        assert!(err.to_string().contains("needs"), "{err}");

        let (src, stride) = solid(16, 16, 0, 0, 0);
        let err = bgra_to_nv12(&src, 16, 16, stride.min(8), &mut dst).unwrap_err();
        assert!(err.to_string().contains("stride"), "{err}");
    }
}
