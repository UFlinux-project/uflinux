//! Software H.264 backend: IronRDP's OpenH264 encoder.
//!
//! Only built with the `h264` feature. The egfx crate is compiled with
//! `openh264-libloading`, so OpenH264 is loaded at runtime from a shared object
//! rather than linked in.

use std::path::{Path, PathBuf};

use ironrdp_egfx::encode::{EncodeFrame, H264Encoder as EgfxH264Encoder, OpenH264Encoder};

use crate::convert::RgbaFrame;
use crate::vaapi::H264Backend;
use crate::{CodecError, Result};

/// Where `from_system_library` looks, in order.
pub const SYSTEM_LIBRARY_CANDIDATES: &[&str] = &[
    "/usr/lib64/libopenh264.so.2.6.0",
    "/usr/lib/x86_64-linux-gnu/libopenh264.so.2.6.0",
    "/usr/lib/libopenh264.so.2.6.0",
    "/usr/lib64/libopenh264.so.8",
    "/usr/lib/x86_64-linux-gnu/libopenh264.so.8",
    "libopenh264.so.8",
    "libopenh264.so",
];

/// CPU-side H.264 encoder used when no VA-API node is available.
pub struct SoftwareH264Encoder {
    inner: OpenH264Encoder,
    library: Option<PathBuf>,
}

// `OpenH264Encoder` is opaque and not `Debug`, so hand-write one; it is needed
// by `Result::expect_err` in the tests.
impl core::fmt::Debug for SoftwareH264Encoder {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SoftwareH264Encoder")
            .field("library", &self.library)
            .finish_non_exhaustive()
    }
}

impl SoftwareH264Encoder {
    /// Loads OpenH264 from an explicit path.
    ///
    /// egfx verifies the blob against the SHA-256 hashes of the releases it was
    /// tested with and rejects anything else, so point this at a supported
    /// `libopenh264.so` (e.g. the 2.6.0 build shipped by UF Linux) rather than an
    /// arbitrary SONAME link.
    pub fn from_library_path(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let inner = OpenH264Encoder::from_library_path(&path)
            .map_err(|error| CodecError::BackendUnavailable(format!("{path:?}: {error}")))?;
        Ok(Self {
            inner,
            library: Some(path),
        })
    }

    /// Tries the usual system locations, returning the first that loads.
    ///
    /// There is no bundled/statically-linked OpenH264 in this configuration, so
    /// a path is always required one way or the other.
    pub fn from_system_library() -> Result<Self> {
        let mut last: Option<String> = None;
        for candidate in SYSTEM_LIBRARY_CANDIDATES {
            let path = Path::new(candidate);
            if !path.exists() {
                continue;
            }
            match Self::from_library_path(path) {
                Ok(encoder) => return Ok(encoder),
                Err(error) => last = Some(error.to_string()),
            }
        }
        Err(CodecError::BackendUnavailable(last.unwrap_or_else(|| {
            format!(
                "no supported libopenh264 in {SYSTEM_LIBRARY_CANDIDATES:?}; \
                 configure a path with SoftwareH264Encoder::from_library_path"
            )
        })))
    }

    /// The shared object this encoder was loaded from, when known.
    pub fn library_path(&self) -> Option<&Path> {
        self.library.as_deref()
    }
}

impl H264Backend for SoftwareH264Encoder {
    fn encode(&mut self, frame: &RgbaFrame<'_>) -> Result<Vec<u8>> {
        frame.validate()?;
        self.inner
            .encode(EncodeFrame {
                data: frame.data,
                width: frame.width,
                height: frame.height,
            })
            .map_err(|error| CodecError::BackendUnavailable(error.to_string()))
    }

    fn request_key_frame(&mut self) {
        self.inner.request_key_frame();
    }

    fn reset(&mut self) {
        self.inner.reset();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An encoder is only constructible when a supported blob is installed, so
    /// every test here degrades to a skip.
    fn available() -> Option<SoftwareH264Encoder> {
        SoftwareH264Encoder::from_system_library().ok()
    }

    #[test]
    fn a_missing_library_is_reported_not_panicked() {
        let error = SoftwareH264Encoder::from_library_path("/nonexistent/libopenh264.so.0")
            .expect_err("a missing blob must not look like a working encoder");
        assert!(
            matches!(error, CodecError::BackendUnavailable(_)),
            "{error}"
        );
    }

    #[test]
    fn invalid_frames_never_reach_the_encoder() {
        let Some(mut encoder) = available() else {
            eprintln!("skipping: no libopenh264 installed");
            return;
        };
        let data = vec![0u8; 4 * 4 * 4];
        assert!(matches!(
            encoder.encode(&RgbaFrame::new(&data, 4, 3)),
            Err(CodecError::InvalidFrame(_))
        ));
    }

    #[test]
    fn an_even_frame_encodes_to_something_or_is_skipped() {
        let Some(mut encoder) = available() else {
            eprintln!("skipping: no libopenh264 installed");
            return;
        };
        let data = vec![0x80u8; 64 * 64 * 4];
        let out = encoder
            .encode(&RgbaFrame::new(&data, 64, 64))
            .expect("encode");
        if !out.is_empty() {
            assert!(
                out.windows(3).any(|w| w == [0x00, 0x00, 0x01]),
                "output is not Annex-B"
            );
        }
        encoder.request_key_frame();
        encoder.reset();
    }
}
