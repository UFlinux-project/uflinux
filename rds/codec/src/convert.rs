//! Input frame type and the pure RGBA8888 → NV12 conversion used by the
//! out-of-process VA-API helper.
//!
//! ffmpeg's `h264_vaapi` encoder is fed NV12 on stdin, so the conversion has to
//! happen in this crate. It uses the integer BT.601 (studio range)
//! approximation that every RDP encoder uses, with 2x2 chroma subsampling:
//!
//! ```text
//! Y =  16 + (( 66R + 129G +  25B + 128) >> 8)
//! U = 128 + ((-38R -  74G + 112B + 128) >> 8)
//! V = 128 + ((112R -  94G -  18B + 128) >> 8)
//! ```
//!
//! Because U and V are linear in RGB, averaging the four pixels of a 2x2 block
//! before converting is equivalent to converting each and averaging the
//! result, and it is four times cheaper.

use crate::{CodecError, Result};

/// One RGBA8888 frame: `data.len() == width * height * 4`, even dimensions.
///
/// This is the crate's own input type so that the crate compiles without the
/// `h264` feature. It maps one-to-one onto `ironrdp_egfx::encode::EncodeFrame`
/// in [`crate::software`].
#[derive(Clone, Copy)]
pub struct RgbaFrame<'a> {
    /// Row-major RGBA8888 pixels.
    pub data: &'a [u8],
    /// Width in pixels. Must be even and non-zero.
    pub width: u32,
    /// Height in pixels. Must be even and non-zero.
    pub height: u32,
}

impl core::fmt::Debug for RgbaFrame<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("RgbaFrame")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("len", &self.data.len())
            .finish()
    }
}

impl<'a> RgbaFrame<'a> {
    /// Wraps a pixel buffer.
    pub fn new(data: &'a [u8], width: u32, height: u32) -> Self {
        Self {
            data,
            width,
            height,
        }
    }

    /// Even, non-zero dimensions and a buffer that matches them exactly.
    pub fn validate(&self) -> Result<()> {
        if self.width == 0 || self.height == 0 {
            return Err(CodecError::InvalidFrame(format!(
                "{}x{} is empty",
                self.width, self.height
            )));
        }
        if !self.width.is_multiple_of(2) || !self.height.is_multiple_of(2) {
            return Err(CodecError::InvalidFrame(format!(
                "{}x{} must have even dimensions",
                self.width, self.height
            )));
        }
        let expected = (self.width as usize) * (self.height as usize) * 4;
        if self.data.len() != expected {
            return Err(CodecError::InvalidFrame(format!(
                "buffer is {} bytes, expected {} for {}x{}",
                self.data.len(),
                expected,
                self.width,
                self.height
            )));
        }
        Ok(())
    }

    /// Size of the NV12 buffer this frame converts to: `w * h * 3 / 2`.
    pub fn nv12_len(&self) -> usize {
        // w * h * 4 / 2 + w * h / 2 == w * h * 3 / 2, without the intermediate.
        (self.width as usize) * (self.height as usize) * 3 / 2
    }
}

/// Converts `frame` into a freshly allocated NV12 buffer (Y plane followed by
/// the interleaved UV plane).
pub fn rgba_to_nv12(frame: &RgbaFrame<'_>) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    write_nv12(frame, &mut out)?;
    Ok(out)
}

/// Converts `frame` into `dst`, reusing its allocation.
pub fn write_nv12(frame: &RgbaFrame<'_>, dst: &mut Vec<u8>) -> Result<()> {
    frame.validate()?;

    let width = frame.width as usize;
    let height = frame.height as usize;
    let luma_len = width * height;

    dst.clear();
    dst.resize(luma_len + luma_len / 2, 0);
    let (luma, chroma) = dst.split_at_mut(luma_len);

    for block_row in 0..(height / 2) {
        for block_col in 0..(width / 2) {
            let mut red = 0u32;
            let mut green = 0u32;
            let mut blue = 0u32;

            for sub_row in 0..2 {
                let y = block_row * 2 + sub_row;
                for sub_col in 0..2 {
                    let x = block_col * 2 + sub_col;
                    let px = &frame.data[(y * width + x) * 4..];
                    let (r, g, b) = (u32::from(px[0]), u32::from(px[1]), u32::from(px[2]));

                    luma[y * width + x] = luma_from_rgb(r, g, b);

                    red += r;
                    green += g;
                    blue += b;
                }
            }

            // Average of the four pixels; see the module comment.
            let uv = (block_row * width) + block_col * 2;
            chroma[uv] = chroma_from_u(red, green, blue, -38, -74, 112);
            chroma[uv + 1] = chroma_from_u(red, green, blue, 112, -94, -18);
        }
    }

    Ok(())
}

fn luma_from_rgb(red: u32, green: u32, blue: u32) -> u8 {
    let value = 16 + ((66 * red + 129 * green + 25 * blue + 128) >> 8);
    value.clamp(16, 235) as u8
}

/// `shift`ed chroma from the *sum* of four pixels, so no division per channel.
fn chroma_from_u(red: u32, green: u32, blue: u32, cr: i32, cg: i32, cb: i32) -> u8 {
    // `red`/`green`/`blue` are the sums of four pixels, so the divisor is
    // 256 * 4 = 1024 and the rounding constant is half of it. That yields
    // exactly the same result as averaging first and then rounding.
    let sum = i64::from(cr) * i64::from(red)
        + i64::from(cg) * i64::from(green)
        + i64::from(cb) * i64::from(blue);
    let value = 128 + ((sum + 512) >> 10);
    value.clamp(16, 240) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(width: u32, height: u32, rgba: [u8; 4]) -> Vec<u8> {
        let mut data = Vec::with_capacity((width * height * 4) as usize);
        for _ in 0..(width * height) {
            data.extend_from_slice(&rgba);
        }
        data
    }

    fn planes(nv12: &[u8], width: usize, height: usize) -> (&[u8], &[u8]) {
        nv12.split_at(width * height)
    }

    #[test]
    fn black_maps_to_studio_black() {
        let data = solid(2, 2, [0, 0, 0, 255]);
        let nv12 = rgba_to_nv12(&RgbaFrame::new(&data, 2, 2)).unwrap();
        let (luma, chroma) = planes(&nv12, 2, 2);
        assert!(luma.iter().all(|&v| v == 16), "luma was {luma:?}");
        assert_eq!(&chroma[..2], &[128, 128]);
    }

    #[test]
    fn white_maps_to_studio_white() {
        let data = solid(2, 2, [255, 255, 255, 255]);
        let nv12 = rgba_to_nv12(&RgbaFrame::new(&data, 2, 2)).unwrap();
        let (luma, chroma) = planes(&nv12, 2, 2);
        assert!(luma.iter().all(|&v| v == 235), "luma was {luma:?}");
        assert_eq!(&chroma[..2], &[128, 128]);
    }

    #[test]
    fn red_maps_to_bt601_chroma_red() {
        let data = solid(2, 2, [255, 0, 0, 255]);
        let nv12 = rgba_to_nv12(&RgbaFrame::new(&data, 2, 2)).unwrap();
        let (luma, chroma) = planes(&nv12, 2, 2);
        // BT.601 red: Y = 16 + (66*255 + 128)/256 = 82, V saturates at 240.
        assert!(luma.iter().all(|&v| v == 82), "luma was {luma:?}");
        assert_eq!(chroma[0], 90, "U");
        assert_eq!(chroma[1], 240, "V");
    }

    #[test]
    fn mid_grey_is_neutral() {
        let data = solid(4, 4, [128, 128, 128, 255]);
        let nv12 = rgba_to_nv12(&RgbaFrame::new(&data, 4, 4)).unwrap();
        let (luma, chroma) = planes(&nv12, 4, 4);
        assert!(luma.iter().all(|&v| v == 126), "luma was {luma:?}");
        assert!(chroma.iter().all(|&v| v == 128), "chroma was {chroma:?}");
    }

    #[test]
    fn output_layout_is_luma_then_interleaved_chroma() {
        let data = solid(4, 2, [10, 20, 30, 255]);
        let frame = RgbaFrame::new(&data, 4, 2);
        let nv12 = rgba_to_nv12(&frame).unwrap();
        assert_eq!(nv12.len(), frame.nv12_len());
        assert_eq!(nv12.len(), 4 * 2 * 3 / 2);
        // Both chroma samples of a uniform block are identical, and the two
        // rows share one chroma row (2x2 subsampling).
        let chroma = &nv12[8..];
        assert_eq!(chroma.len(), 4);
        assert_eq!(&chroma[0..2], &chroma[2..4]);
    }

    #[test]
    fn rejects_odd_dimensions_and_wrong_lengths() {
        let data = solid(3, 2, [0, 0, 0, 255]);
        let err = RgbaFrame::new(&data, 3, 2).validate().unwrap_err();
        assert!(
            matches!(&err, CodecError::InvalidFrame(msg) if msg.contains("even")),
            "{err}"
        );

        let short = solid(2, 2, [0, 0, 0, 255]);
        let err = RgbaFrame::new(&short[..15], 2, 2).validate().unwrap_err();
        assert!(
            matches!(&err, CodecError::InvalidFrame(msg) if msg.contains("expected 16")),
            "{err}"
        );

        let err = RgbaFrame::new(&[], 0, 0).validate().unwrap_err();
        assert!(
            matches!(&err, CodecError::InvalidFrame(msg) if msg.contains("empty")),
            "{err}"
        );
    }

    #[test]
    fn write_nv12_reuses_the_destination_buffer() {
        let data = solid(2, 2, [0, 0, 0, 255]);
        let mut dst = vec![0xAAu8; 4096];
        let capacity_before = dst.capacity();
        write_nv12(&RgbaFrame::new(&data, 2, 2), &mut dst).unwrap();
        assert_eq!(dst.len(), 6);
        assert_eq!(dst.capacity(), capacity_before);
    }
}
