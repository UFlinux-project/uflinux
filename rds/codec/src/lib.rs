//! Codec-tier selection and H.264 backends for the UFLinux RDS host.
//!
//! # The ladder
//!
//! Every connection gets exactly one graphics codec, chosen once at connect
//! time by [`select_tier`] and walked *downwards* if the encoder later fails:
//!
//! ```text
//! avc420_hardware  ->  avc420_software  ->  avc444  ->  remote_fx  ->  qoi
//! ```
//!
//! [`CodecTier`] is the rung, [`ClientCaps`] is what the client negotiated,
//! [`HardwareCaps`] is what [`detect_vaapi`] found on the host, and
//! [`uflinux_rds_common::H264Policy`] is the operator's instruction. The
//! decision function is pure — no I/O, no environment reads — so it can be
//! tested and replayed from a captured capability set.
//!
//! When an encoder dies mid-session, [`downgrade`] returns the next rung and the
//! server re-enters the pipeline with a codec it can certainly drive. `qoi` is
//! terminal: it is a still-image path that cannot fail in a way that needs
//! another codec.
//!
//! # Web clients never get an AVC tier
//!
//! The browser client is the wasm build of this workspace, and at the pinned
//! IronRDP revision (`b13befa0db`, `ironrdp-egfx` 0.5.0) neither `ironrdp-egfx`
//! nor `ironrdp-server` ships a wasm-compatible EGFX/H.264 *decoder*: the AVC
//! paths need native surfaces that a canvas-based client does not have. The web
//! client therefore negotiates no EGFX and no AVC profile, which lands it on
//! QOI or RemoteFX by construction — see [`ClientCaps::web_client`]. It is not a
//! fallback we rely on; it is the only honest answer at this pin.
//!
//! # Serde
//!
//! Enum variants serialise as `snake_case` (`"avc420_hardware"`,
//! `"avc420_software"`, `"avc444"`, `"remote_fx"`, `"qoi"`), matching
//! `H264Policy` and the protocol enums in `uflinux-rds-common` so the Settings
//! UI can render every codec-related field with one convention.
//!
//! # Feature `h264`
//!
//! The base package builds with default features, i.e. without any OpenH264
//! binding: [`CodecTier::Avc420Software`] is then not offered by [`select_tier`],
//! and the ladder continues to `avc444`. Hardware encode needs no cargo feature
//! at all because it runs the out-of-process `ffmpeg -c:v h264_vaapi` helper in
//! [`vaapi`]; only the runtime ffmpeg package matters there.

mod convert;
mod probe;
mod remotefx;
mod tier;
mod vaapi;

#[cfg(feature = "h264")]
pub mod software;

pub use convert::{RgbaFrame, rgba_to_nv12, write_nv12};
pub use probe::{
    DEFAULT_VAAPI_DEVICE, FFMPEG_BINARY, detect_vaapi, ffmpeg_on_path, ffmpeg_supports_h264_vaapi,
    probe_ffmpeg_h264_vaapi, probe_ffmpeg_h264_vaapi_with,
};
pub use remotefx::{Entropy, RemoteFxPolicy, pick_entropy, quant_subbands};
pub use tier::{ClientCaps, CodecTier, HardwareCaps, downgrade, requires_h264, select_tier};
pub use vaapi::{
    EncoderProcess, EncoderSpawner, FfmpegSpawner, H264Backend, MIN_DIMENSION, VaapiConfig,
    VaapiH264Encoder,
};

/// Everything that can go wrong while choosing or driving a codec.
#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    /// `H264Policy::Force` was configured but the client negotiated no AVC
    /// profile. Reported rather than silently downgraded, so the session can
    /// tell the user why it was refused.
    #[error("H.264 is forced but the client negotiated no AVC profile")]
    ClientWithoutH264,
    /// The VA-API driver refuses frames this small; downgrade instead.
    #[error("frame is below the {min}px minimum size of the VA-API encoder")]
    FrameTooSmall {
        /// The smallest dimension the helper accepts.
        min: u32,
    },
    /// A frame does not satisfy the encoder's input contract.
    #[error("invalid frame: {0}")]
    InvalidFrame(String),
    /// The encoder helper process died. The next call respawns it.
    #[error("encoder backend died")]
    BackendDied,
    /// The backend can never work here: no ffmpeg, no OpenH264 blob, …
    #[error("encoder backend unavailable: {0}")]
    BackendUnavailable(String),
    /// The configured device exists but is not a DRM render node.
    #[error("{0:?} is not a usable DRM render node")]
    DeviceNotUsable(std::path::PathBuf),
    /// Underlying I/O failure.
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
}

/// Crate-local result alias.
pub type Result<T> = std::result::Result<T, CodecError>;
