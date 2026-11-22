//! Header-only image size probing.
//!
//! The daemon rejects oversized wallpapers, and reading their pixel dimensions
//! with gdk-pixbuf would make the resident daemon map gdk-pixbuf and Alpine's
//! glycin loaders. Every raster format the Settings wallpaper chooser accepts
//! stores its dimensions in a fixed header, so a few KiB of reads is enough.

use crate::Result;
use std::fs::File;
use std::io::{Read, Seek};
use std::path::Path;

/// Bytes read before the whole file is reread. A JPEG carrying a large embedded
/// thumbnail can hide its frame marker well past the first kilobyte.
const PROBE_WINDOW: usize = 64 * 1024;

/// Dimensions of an image header, as far as they are knowable without decoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageDimensions {
    /// Raster width and height in pixels.
    Pixels { width: u32, height: u32 },
    /// A vector document: it has no intrinsic size, so no pixel budget applies.
    Scalable,
}

/// Parses image dimensions from the leading bytes of a file without decoding
/// it. Returns `None` for an unknown, unsupported or truncated header.
pub fn dimensions_from_header(header: &[u8]) -> Option<ImageDimensions> {
    if header.len() >= 8 && header[..8] == *b"\x89PNG\r\n\x1a\n" {
        return png(header);
    }
    if header.starts_with(b"\xff\xd8") {
        return jpeg(header);
    }
    if header.len() >= 12 && header[..4] == *b"RIFF" && header[8..12] == *b"WEBP" {
        return webp(header);
    }
    if header.starts_with(b"GIF87a") || header.starts_with(b"GIF89a") {
        return gif(header);
    }
    if header.starts_with(b"BM") {
        return bmp(header);
    }
    if is_svg(header) {
        return Some(ImageDimensions::Scalable);
    }
    None
}

/// Reads just enough of `path` to report its dimensions.
pub fn probe_file(path: &Path) -> Result<ImageDimensions> {
    const UNAVAILABLE: &str = "Wallpaper file is unavailable";
    let mut file = File::open(path).map_err(|_| UNAVAILABLE.to_string())?;
    let mut header = vec![0u8; PROBE_WINDOW];
    let read =
        read_up_to(&mut file, &mut header).map_err(|_| UNAVAILABLE.to_string())?;
    header.truncate(read);
    if let Some(dimensions) = dimensions_from_header(&header) {
        return Ok(dimensions);
    }
    // A frame marker past the window: reread the whole file, which the caller
    // has already capped, rather than rejecting a valid image.
    if read == PROBE_WINDOW {
        let mut whole = Vec::new();
        file.rewind().map_err(|_| UNAVAILABLE.to_string())?;
        file.read_to_end(&mut whole)
            .map_err(|_| UNAVAILABLE.to_string())?;
        if let Some(dimensions) = dimensions_from_header(&whole) {
            return Ok(dimensions);
        }
    }
    Err("Unsupported wallpaper image".into())
}

fn read_up_to(file: &mut File, buffer: &mut [u8]) -> std::io::Result<usize> {
    let mut filled = 0;
    while filled < buffer.len() {
        match file.read(&mut buffer[filled..]) {
            Ok(0) => break,
            Ok(read) => filled += read,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(filled)
}

fn pixels(width: u32, height: u32) -> Option<ImageDimensions> {
    if width == 0 || height == 0 {
        return None;
    }
    Some(ImageDimensions::Pixels { width, height })
}

fn be16(header: &[u8], offset: usize) -> Option<u32> {
    header
        .get(offset..offset + 2)
        .map(|b| u16::from_be_bytes([b[0], b[1]]) as u32)
}

fn be32(header: &[u8], offset: usize) -> Option<u32> {
    header
        .get(offset..offset + 4)
        .map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}

fn le16(header: &[u8], offset: usize) -> Option<u32> {
    header
        .get(offset..offset + 2)
        .map(|b| u16::from_le_bytes([b[0], b[1]]) as u32)
}

fn le32(header: &[u8], offset: usize) -> Option<u32> {
    header
        .get(offset..offset + 4)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

fn le24(header: &[u8], offset: usize) -> Option<u32> {
    header
        .get(offset..offset + 3)
        .map(|b| (b[0] as u32) | (b[1] as u32) << 8 | (b[2] as u32) << 16)
}

fn png(header: &[u8]) -> Option<ImageDimensions> {
    if header.get(12..16) != Some(&b"IHDR"[..]) {
        return None;
    }
    pixels(be32(header, 16)?, be32(header, 20)?)
}

/// Walks the marker segments until a start-of-frame marker reveals the size.
fn jpeg(header: &[u8]) -> Option<ImageDimensions> {
    let mut offset = 2;
    while offset < header.len() {
        if header[offset] != 0xff {
            offset += 1;
            continue;
        }
        while offset < header.len() && header[offset] == 0xff {
            offset += 1;
        }
        let marker = *header.get(offset)?;
        // `segment` addresses the two-byte length that follows the marker code.
        let segment = offset + 1;
        offset = segment;
        // Standalone markers carry no length and no frame.
        if marker == 0x01 || (0xd0..=0xd9).contains(&marker) {
            continue;
        }
        // Start of frame: baseline through progressive, minus the DHT, JPEG
        // extension and reserved markers inside the same range. The payload
        // begins after the length field: precision, height, width.
        if (0xc0..=0xcf).contains(&marker) && !matches!(marker, 0xc4 | 0xc8 | 0xcc) {
            return pixels(be16(header, segment + 5)?, be16(header, segment + 3)?);
        }
        if marker == 0xda {
            return None; // Start of scan without a preceding frame header.
        }
        offset = segment + be16(header, segment)? as usize;
    }
    None
}

fn webp(header: &[u8]) -> Option<ImageDimensions> {
    let mut offset = 12;
    while offset + 8 <= header.len() {
        let kind = &header[offset..offset + 4];
        let size = le32(header, offset + 4)? as usize;
        let data = offset + 8;
        let body = header.get(data..data.checked_add(size)?)?;
        match kind {
            b"VP8 " if body.len() >= 10 && body[3..6] == *b"\x9d\x01\x2a" => {
                return pixels(le16(body, 6)? & 0x3fff, le16(body, 8)? & 0x3fff);
            }
            b"VP8L" if body.first() == Some(&0x21) && body.len() >= 5 => {
                let bits = le32(body, 1)?;
                return pixels((bits & 0x3fff) + 1, ((bits >> 14) & 0x3fff) + 1);
            }
            b"VP8X" if body.len() >= 10 => {
                return pixels(le24(body, 4)? + 1, le24(body, 7)? + 1);
            }
            _ => {}
        }
        // Chunks are padded to an even size.
        offset = data + size + (size & 1);
    }
    None
}

fn gif(header: &[u8]) -> Option<ImageDimensions> {
    pixels(le16(header, 6)?, le16(header, 8)?)
}

fn bmp(header: &[u8]) -> Option<ImageDimensions> {
    let width = le32(header, 18).map(|value| value as i32)?.unsigned_abs();
    // A negative height means top-down rows, so only the magnitude is the size.
    let height = le32(header, 22).map(|value| value as i32)?.unsigned_abs();
    pixels(width, height)
}

/// SVG carries no raster size, and a viewBox-only document has no intrinsic one
/// at all, so vector files are accepted and left to the caller's file size cap.
fn is_svg(header: &[u8]) -> bool {
    let head = &header[..header.len().min(8 * 1024)];
    // An SVG may be cut mid-declaration by the read window; the valid prefix is
    // enough, since the root element always opens within the first few bytes.
    let text = match std::str::from_utf8(head) {
        Ok(text) => text,
        Err(error) => {
            std::str::from_utf8(&head[..error.valid_up_to()]).unwrap_or_default()
        }
    };
    let text = text.trim_start();
    text.starts_with("<?xml") || text.starts_with("<svg")
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIZE: ImageDimensions = ImageDimensions::Pixels {
        width: 3840,
        height: 2160,
    };

    fn png_header(width: u32, height: u32) -> Vec<u8> {
        let mut header = b"\x89PNG\r\n\x1a\n".to_vec();
        header.extend_from_slice(&13u32.to_be_bytes());
        header.extend_from_slice(b"IHDR");
        header.extend_from_slice(&width.to_be_bytes());
        header.extend_from_slice(&height.to_be_bytes());
        header.extend_from_slice(&[0; 5]);
        header
    }

    fn webp_chunk(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut header = b"RIFF\x00\x00\x00\x00WEBP".to_vec();
        header.extend_from_slice(kind);
        header.extend_from_slice(&(body.len() as u32).to_le_bytes());
        header.extend_from_slice(body);
        header
    }

    #[test]
    fn reads_png() {
        assert_eq!(dimensions_from_header(&png_header(3840, 2160)), Some(SIZE));
        assert_eq!(dimensions_from_header(&png_header(0, 2160)), None);
        assert_eq!(
            dimensions_from_header(b"\x89PNG\r\n\x1a\n\x00\x00\x00\x00"),
            None
        );
    }

    #[test]
    fn reads_jpeg_past_app_segments() {
        let mut header = vec![0xff, 0xd8, 0xff, 0xe0];
        header.extend_from_slice(&[0x00, 0x06]); // segment length, payload follows
        header.extend_from_slice(b"JFIF");
        header.extend_from_slice(&[0xff, 0xc0]);
        header.extend_from_slice(&[0x00, 0x11]);
        header.push(8); // sample precision
        header.extend_from_slice(&[0x08, 0x70]); // 2160 rows
        header.extend_from_slice(&[0x0f, 0x00]); // 3840 columns
        assert_eq!(dimensions_from_header(&header), Some(SIZE));
    }

    #[test]
    fn reads_webp_variants() {
        let mut lossy = vec![0, 0, 0];
        lossy.extend_from_slice(b"\x9d\x01\x2a");
        lossy.extend_from_slice(&3840u16.to_le_bytes());
        lossy.extend_from_slice(&2160u16.to_le_bytes());
        assert_eq!(dimensions_from_header(&webp_chunk(b"VP8 ", &lossy)), Some(SIZE));

        let mut lossless = vec![0x21];
        lossless.extend_from_slice(&(((2160u32 - 1) << 14) | (3840 - 1)).to_le_bytes());
        assert_eq!(
            dimensions_from_header(&webp_chunk(b"VP8L", &lossless)),
            Some(SIZE)
        );

        let mut extended = vec![0, 0, 0, 0]; // flags and reserved padding
        extended.extend_from_slice(&3839u32.to_le_bytes()[..3]);
        extended.extend_from_slice(&2159u32.to_le_bytes()[..3]);
        assert_eq!(
            dimensions_from_header(&webp_chunk(b"VP8X", &extended)),
            Some(SIZE)
        );
    }

    #[test]
    fn reads_gif_and_bmp() {
        let mut gif = b"GIF89a".to_vec();
        gif.extend_from_slice(&3712u16.to_le_bytes());
        gif.extend_from_slice(&2160u16.to_le_bytes());
        assert_eq!(
            dimensions_from_header(&gif),
            Some(ImageDimensions::Pixels {
                width: 3712,
                height: 2160
            })
        );

        let mut bmp = b"BM".to_vec();
        bmp.extend_from_slice(&[0; 12]); // rest of the file header
        bmp.extend_from_slice(&40u32.to_le_bytes()); // DIB header size
        bmp.extend_from_slice(&3840i32.to_le_bytes());
        bmp.extend_from_slice(&(-2160i32).to_le_bytes()); // top-down rows
        assert_eq!(dimensions_from_header(&bmp), Some(SIZE));
    }

    #[test]
    fn accepts_scalable_and_rejects_unknown() {
        assert_eq!(
            dimensions_from_header(b"<svg xmlns='http://www.w3.org/2000/svg'>"),
            Some(ImageDimensions::Scalable)
        );
        assert_eq!(
            dimensions_from_header(b"<?xml version='1.0'?>\n<svg viewBox='0 0 8 8'>"),
            Some(ImageDimensions::Scalable)
        );
        assert_eq!(dimensions_from_header(b"GIMP brush"), None);
        assert_eq!(dimensions_from_header(b""), None);
    }
}
