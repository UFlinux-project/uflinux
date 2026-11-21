//! Captured desktop frames and the software frame diff.

use crate::error::BackendError;

/// A rectangle of a [`Frame`] whose contents changed since the previous frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DirtyRect {
    /// Left edge, in pixels.
    pub x: u16,
    /// Top edge, in pixels.
    pub y: u16,
    /// Width, in pixels (always > 0).
    pub width: u16,
    /// Height, in pixels (always > 0).
    pub height: u16,
}

impl DirtyRect {
    fn from_bounds(x0: u32, y0: u32, x1: u32, y1: u32) -> Self {
        // Resolutions above 65535px are clamped rather than wrapped; RDP
        // monitors are u16 anyway.
        let width = (x1 - x0).min(u16::MAX as u32) as u16;
        let height = (y1 - y0).min(u16::MAX as u32) as u16;
        Self {
            x: x0.min(u16::MAX as u32) as u16,
            y: y0.min(u16::MAX as u32) as u16,
            width,
            height,
        }
    }
}

/// One captured desktop image.
#[derive(Clone, PartialEq, Eq)]
pub struct Frame {
    /// Width of `pixels`, in pixels.
    pub width: u32,
    /// Height of `pixels`, in pixels.
    pub height: u32,
    /// RGBA8888, row-major, top-down, no padding, exactly `width * height * 4`
    /// bytes.
    pub pixels: Vec<u8>,
    /// Regions that differ from the previously delivered frame. An empty list
    /// means "nothing changed" and the frame can be skipped.
    pub dirty: Vec<DirtyRect>,
}

impl Frame {
    pub(crate) fn new(
        width: u32,
        height: u32,
        pixels: Vec<u8>,
        dirty: Vec<DirtyRect>,
    ) -> Result<Self, BackendError> {
        let expected = (width as usize)
            .checked_mul(height as usize)
            .and_then(|n| n.checked_mul(4))
            .ok_or_else(|| {
                BackendError::Capture(format!("frame {width}x{height} overflows usize"))
            })?;
        if pixels.len() != expected {
            return Err(BackendError::Capture(format!(
                "frame {width}x{height} needs {expected} bytes, got {}",
                pixels.len()
            )));
        }
        Ok(Self {
            width,
            height,
            pixels,
            dirty,
        })
    }
}

impl std::fmt::Debug for Frame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Frame")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("len", &self.pixels.len())
            .field("dirty", &self.dirty)
            .finish()
    }
}

/// Above this number of dirty bands the diff collapses into a single bounding
/// rectangle: a fragmented update costs more to encode than a coarse one.
const MAX_BANDS: usize = 32;

/// Number of changed pixels after which a frame is reported as one bounding
/// rectangle instead of per-band rectangles.
const FULL_UPDATE_PIXELS: u64 = 1_000_000;

/// Compare two RGBA8888 buffers of `width * height * 4` bytes and return the
/// changed regions.
///
/// Changed rows are grouped into contiguous horizontal bands, and each band is
/// reported as one rectangle spanning its left-most and right-most changed
/// pixel. Identical buffers produce an empty list, a single changed pixel
/// produces a 1x1 rectangle. A change covering more than
/// [`FULL_UPDATE_PIXELS`] pixels, or a diff that fragments into more than
/// [`MAX_BANDS`] bands, degrades to a single coarse rectangle bounding every
/// change (a documented full-frame update).
///
/// Buffers of different lengths are treated as fully changed.
pub fn dirty_regions(previous: &[u8], current: &[u8], width: u32, height: u32) -> Vec<DirtyRect> {
    let row_bytes = match (width as usize)
        .checked_mul(height as usize)
        .and_then(|n| n.checked_mul(4))
    {
        Some(n) if n == previous.len() && n == current.len() => n,
        _ => {
            return vec![DirtyRect::from_bounds(0, 0, width, height)];
        }
    };
    if row_bytes == 0 {
        return Vec::new();
    }

    let mut changed_rows: Vec<(u32, u32, u32)> = Vec::new(); // (y, first_x, last_x_exclusive)
    let mut changed_pixels: u64 = 0;
    let stride = (width as usize) * 4;

    for (y, (prev_row, cur_row)) in previous
        .chunks_exact(stride)
        .zip(current.chunks_exact(stride))
        .enumerate()
    {
        if prev_row == cur_row {
            continue;
        }
        let row = y as u32;
        let mut first = u32::MAX;
        let mut last = 0u32;
        for (x, (prev, cur)) in prev_row
            .chunks_exact(4)
            .zip(cur_row.chunks_exact(4))
            .enumerate()
        {
            if prev != cur {
                changed_pixels += 1;
                let x = x as u32;
                first = first.min(x);
                last = x + 1;
            }
        }
        changed_rows.push((row, first, last));
    }

    if changed_rows.is_empty() {
        return Vec::new();
    }
    if changed_pixels > FULL_UPDATE_PIXELS || changed_rows.len() > MAX_BANDS {
        let (min_y, max_y) = (
            changed_rows[0].0,
            changed_rows[changed_rows.len() - 1].0 + 1,
        );
        let min_x = changed_rows.iter().map(|r| r.1).min().unwrap_or(0);
        let max_x = changed_rows.iter().map(|r| r.2).max().unwrap_or(width);
        return vec![DirtyRect::from_bounds(min_x, min_y, max_x, max_y)];
    }

    // Merge adjacent rows into bands.
    let mut bands: Vec<(u32, u32, u32, u32)> = Vec::new(); // (y0, y1_exclusive, x0, x1_exclusive)
    for &(row, x0, x1) in &changed_rows {
        match bands.last_mut() {
            Some(band) if band.1 == row => {
                band.1 = row + 1;
                band.2 = band.2.min(x0);
                band.3 = band.3.max(x1);
            }
            _ => bands.push((row, row + 1, x0, x1)),
        }
    }

    bands
        .into_iter()
        .map(|(y0, y1, x0, x1)| DirtyRect::from_bounds(x0, y0, x1, y1))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(width: u32, height: u32, color: [u8; 4]) -> Vec<u8> {
        let mut buf = vec![0u8; (width * height * 4) as usize];
        for chunk in buf.chunks_exact_mut(4) {
            chunk.copy_from_slice(&color);
        }
        buf
    }

    fn put(buf: &mut [u8], width: u32, x: u32, y: u32, color: [u8; 4]) {
        let off = ((y * width + x) * 4) as usize;
        buf[off..off + 4].copy_from_slice(&color);
    }

    #[test]
    fn identical_frames_produce_no_dirty_regions() {
        let a = solid(16, 16, [10, 20, 30, 255]);
        let b = a.clone();
        assert!(dirty_regions(&a, &b, 16, 16).is_empty());
    }

    #[test]
    fn single_changed_pixel_yields_one_pixel_rect() {
        let a = solid(16, 16, [10, 20, 30, 255]);
        let mut b = a.clone();
        put(&mut b, 16, 5, 7, [10, 20, 31, 255]);
        assert_eq!(
            dirty_regions(&a, &b, 16, 16),
            vec![DirtyRect {
                x: 5,
                y: 7,
                width: 1,
                height: 1
            }]
        );
    }

    #[test]
    fn alpha_only_change_counts() {
        let a = solid(4, 4, [1, 2, 3, 255]);
        let mut b = a.clone();
        put(&mut b, 4, 0, 0, [1, 2, 3, 0]);
        assert_eq!(dirty_regions(&a, &b, 4, 4).len(), 1);
    }

    #[test]
    fn adjacent_rows_merge_into_one_band() {
        let a = solid(32, 32, [0, 0, 0, 255]);
        let mut b = a.clone();
        for y in 4..8 {
            put(&mut b, 32, 3, y, [255, 255, 255, 255]);
        }
        assert_eq!(
            dirty_regions(&a, &b, 32, 32),
            vec![DirtyRect {
                x: 3,
                y: 4,
                width: 1,
                height: 4
            }]
        );
    }

    #[test]
    fn separated_rows_stay_separate() {
        let a = solid(32, 32, [0, 0, 0, 255]);
        let mut b = a.clone();
        put(&mut b, 32, 0, 0, [1, 0, 0, 255]);
        put(&mut b, 32, 31, 31, [1, 0, 0, 255]);
        assert_eq!(
            dirty_regions(&a, &b, 32, 32),
            vec![
                DirtyRect {
                    x: 0,
                    y: 0,
                    width: 1,
                    height: 1
                },
                DirtyRect {
                    x: 31,
                    y: 31,
                    width: 1,
                    height: 1
                },
            ]
        );
    }

    #[test]
    fn full_screen_change_covers_everything() {
        let a = solid(64, 48, [0, 0, 0, 255]);
        let b = solid(64, 48, [255, 255, 255, 255]);
        assert_eq!(
            dirty_regions(&a, &b, 64, 48),
            vec![DirtyRect {
                x: 0,
                y: 0,
                width: 64,
                height: 48
            }]
        );
    }

    #[test]
    fn fragmented_diff_collapses_to_bounding_box() {
        let a = solid(64, 128, [0, 0, 0, 255]);
        let mut b = a.clone();
        // 64 isolated changed rows on alternating rows -> more than MAX_BANDS.
        for y in (0..128u32).step_by(2) {
            put(&mut b, 64, 10 + (y % 3), y, [9, 9, 9, 255]);
        }
        let rects = dirty_regions(&a, &b, 64, 128);
        assert_eq!(rects.len(), 1);
        let r = rects[0];
        assert_eq!((r.x, r.y), (10, 0));
        // Changed rows are 0, 2, ..., 126, so the coarse rectangle spans
        // rows 0..=126.
        assert!(
            u32::from(r.width) >= 3 && u32::from(r.height) == 127,
            "{r:?}"
        );
    }

    #[test]
    fn mismatched_lengths_are_a_full_update() {
        let a = solid(8, 8, [0, 0, 0, 255]);
        let rects = dirty_regions(&a, &[0, 0, 0, 0], 8, 8);
        assert_eq!(
            rects,
            vec![DirtyRect {
                x: 0,
                y: 0,
                width: 8,
                height: 8
            }]
        );
    }

    #[test]
    fn zero_sized_frame_has_no_regions() {
        assert!(dirty_regions(&[], &[], 0, 0).is_empty());
    }

    #[test]
    fn frame_rejects_wrong_pixel_count() {
        assert!(Frame::new(2, 2, vec![0u8; 4 * 2 * 2], Vec::new()).is_ok());
        assert!(Frame::new(2, 2, vec![0u8; 10], Vec::new()).is_err());
    }
}
