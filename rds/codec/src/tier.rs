//! Pure codec-tier decision. No I/O, no environment reads, no clocks.

use serde::{Deserialize, Serialize};
use uflinux_rds_common::H264Policy;

use crate::CodecError;

/// A graphics codec the server can drive, ordered best-first.
///
/// Serde uses `snake_case` to match `H264Policy` and the protocol enums in
/// `uflinux-rds-common`: `"avc420_hardware"`, `"avc420_software"`,
/// `"avc444"`, `"remote_fx"`, `"qoi"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CodecTier {
    /// H.264 4:2:0 encoded by the GPU through the out-of-process ffmpeg
    /// VA-API helper.
    Avc420Hardware,
    /// H.264 4:2:0 encoded on the CPU by OpenH264 (cargo feature `h264`).
    Avc420Software,
    /// H.264 4:4:4, egfx's in-tree encoder. CPU only, wider client support
    /// than AVC420.
    Avc444,
    /// RemoteFX (MS-RDPRFX). Always available in ironrdp-server.
    RemoteFx,
    /// Lossless QOI still images. The terminal fallback.
    Qoi,
}

impl CodecTier {
    /// Position on the ladder, 0 = best. Used by [`downgrade`].
    const fn rank(self) -> u8 {
        match self {
            Self::Avc420Hardware => 0,
            Self::Avc420Software => 1,
            Self::Avc444 => 2,
            Self::RemoteFx => 3,
            Self::Qoi => 4,
        }
    }

    /// One rung down the ladder. [`CodecTier::Qoi`] is terminal.
    pub fn downgraded(self) -> Self {
        match self.rank() {
            0 => Self::Avc420Software,
            1 => Self::Avc444,
            2 => Self::RemoteFx,
            3 => Self::Qoi,
            _ => Self::Qoi,
        }
    }

    /// True for the tiers that need a working H.264 encoder.
    pub fn requires_h264(self) -> bool {
        matches!(
            self,
            Self::Avc420Hardware | Self::Avc420Software | Self::Avc444
        )
    }
}

impl core::fmt::Display for CodecTier {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let name = match self {
            Self::Avc420Hardware => "avc420_hardware",
            Self::Avc420Software => "avc420_software",
            Self::Avc444 => "avc444",
            Self::RemoteFx => "remote_fx",
            Self::Qoi => "qoi",
        };
        f.write_str(name)
    }
}

/// The codec capabilities a client actually negotiated. The caller fills this
/// in from the EGFX caps PDU plus the legacy RDPGFX/RemoteFX negotiation, so
/// these are agreed facts, not wishes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ClientCaps {
    /// The client speaks the graphics pipeline at all. Every AVC tier needs it.
    pub egfx: bool,
    /// `AVC420_ENABLED` (EGFX caps V10_7 and later).
    pub avc420: bool,
    /// `AVC444_ENABLED` (EGFX caps V8_1 and later).
    pub avc444: bool,
    /// RemoteFX surface bits were accepted.
    pub remotefx: bool,
    /// The client accepts UFLinux's QOI surface updates.
    pub qoi: bool,
}

impl ClientCaps {
    /// An mstsc-style client: EGFX with both AVC profiles and RemoteFX.
    pub const fn native_h264_client() -> Self {
        Self {
            egfx: true,
            avc420: true,
            avc444: true,
            remotefx: true,
            qoi: true,
        }
    }

    /// The wasm web client at this pin: no EGFX, no AVC, RemoteFX and QOI.
    pub const fn web_client() -> Self {
        Self {
            egfx: false,
            avc420: false,
            avc444: false,
            remotefx: true,
            qoi: true,
        }
    }

    /// Can this client take an AVC frame at all?
    pub fn supports_avc(self) -> bool {
        self.egfx && (self.avc420 || self.avc444)
    }
}

/// What the host can do, as reported by [`crate::detect_vaapi`].
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct HardwareCaps {
    /// A usable VA-API render node was found.
    pub vaapi: bool,
    /// The render node that was found, for logging and for the helper command.
    pub device: Option<String>,
}

impl HardwareCaps {
    /// No GPU encode available.
    pub const NONE: Self = Self {
        vaapi: false,
        device: None,
    };

    /// No GPU encode available.
    pub const fn none() -> Self {
        Self::NONE
    }
}

/// Pick the codec for one connection.
///
/// Rules (locked design):
///
/// * [`H264Policy::Off`] never yields an AVC tier: RemoteFX when allowed and
///   supported, otherwise QOI.
/// * An AVC tier needs `caps.egfx` plus `caps.avc420` (or `caps.avc444` for
///   [`CodecTier::Avc444`]).
/// * [`H264Policy::Auto`] walks the ladder: hardware AVC420 when
///   `hw.vaapi`, else software AVC420, else AVC444 when supported, else
///   RemoteFX when `allow_remotefx && caps.remotefx`, else QOI.
/// * [`H264Policy::Software`] is `Auto` with the GPU ignored: the same ladder
///   with `hw` replaced by [`HardwareCaps::none`].
/// * [`H264Policy::Force`] picks the best AVC tier and never falls below AVC.
///   A client without AVC support is [`CodecError::ClientWithoutH264`] — the
///   only error this function can return.
pub fn select_tier(
    caps: &ClientCaps,
    hw: &HardwareCaps,
    policy: H264Policy,
    allow_remotefx: bool,
) -> Result<CodecTier, CodecError> {
    let remotefx = allow_remotefx && caps.remotefx;

    match policy {
        H264Policy::Off => Ok(if remotefx {
            CodecTier::RemoteFx
        } else {
            CodecTier::Qoi
        }),
        H264Policy::Auto | H264Policy::Software => {
            // "Software only" is the automatic ladder with the GPU ignored.
            let hw = if policy == H264Policy::Software {
                &HardwareCaps::NONE
            } else {
                hw
            };
            if !caps.egfx {
                return Ok(fallback(caps, remotefx));
            }
            if caps.avc420 {
                // Software AVC420 is only a tier when the crate was built with
                // the OpenH264 backend; without it, egfx's AVC444 is the next
                // thing the server can actually drive.
                return Ok(if hw.vaapi {
                    CodecTier::Avc420Hardware
                } else if cfg!(feature = "h264") {
                    CodecTier::Avc420Software
                } else if caps.avc444 {
                    CodecTier::Avc444
                } else {
                    fallback(caps, remotefx)
                });
            }
            if caps.avc444 {
                return Ok(CodecTier::Avc444);
            }
            Ok(fallback(caps, remotefx))
        }
        H264Policy::Force => {
            if !caps.egfx || !(caps.avc420 || caps.avc444) {
                return Err(CodecError::ClientWithoutH264);
            }
            Ok(if caps.avc420 && hw.vaapi {
                CodecTier::Avc420Hardware
            } else if caps.avc420 && cfg!(feature = "h264") {
                CodecTier::Avc420Software
            } else if caps.avc420 {
                // AVC420 negotiated but no AVC420 encoder in this build:
                // egfx's AVC444 still satisfies "never below AVC" when the
                // client can take it, otherwise the hardware/software choice
                // is simply unavailable.
                if caps.avc444 {
                    CodecTier::Avc444
                } else {
                    return Err(CodecError::ClientWithoutH264);
                }
            } else {
                CodecTier::Avc444
            })
        }
    }
}

fn fallback(caps: &ClientCaps, remotefx: bool) -> CodecTier {
    if remotefx {
        CodecTier::RemoteFx
    } else if caps.qoi {
        CodecTier::Qoi
    } else {
        // Nothing negotiated: QOI is still what the server can always send.
        CodecTier::Qoi
    }
}

/// One step down the ladder, for an encoder that failed mid-session.
pub fn downgrade(tier: CodecTier) -> CodecTier {
    tier.downgraded()
}

/// Whether `tier` needs a working H.264 encoder.
pub fn requires_h264(tier: CodecTier) -> bool {
    tier.requires_h264()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gpu() -> HardwareCaps {
        HardwareCaps {
            vaapi: true,
            device: Some("/dev/dri/renderD128".to_owned()),
        }
    }

    #[test]
    fn auto_prefers_hardware_avc420() {
        let caps = ClientCaps::native_h264_client();
        assert_eq!(
            select_tier(&caps, &gpu(), H264Policy::Auto, true).unwrap(),
            CodecTier::Avc420Hardware
        );
    }

    #[test]
    fn auto_without_gpu_uses_software_avc420_when_built_in() {
        let caps = ClientCaps::native_h264_client();
        let tier = select_tier(&caps, &HardwareCaps::none(), H264Policy::Auto, true).unwrap();
        // Default features have no OpenH264 backend, so the ladder continues
        // to AVC444; with `h264` it stops at the software encoder.
        if cfg!(feature = "h264") {
            assert_eq!(tier, CodecTier::Avc420Software);
        } else {
            assert_eq!(tier, CodecTier::Avc444);
        }
    }

    #[test]
    fn auto_falls_to_avc444_when_client_lacks_avc420() {
        let caps = ClientCaps {
            avc420: false,
            ..ClientCaps::native_h264_client()
        };
        assert_eq!(
            select_tier(&caps, &gpu(), H264Policy::Auto, true).unwrap(),
            CodecTier::Avc444
        );
    }

    #[test]
    fn auto_without_egfx_never_picks_avc_even_with_gpu() {
        let caps = ClientCaps::web_client();
        assert_eq!(
            select_tier(&caps, &gpu(), H264Policy::Auto, true).unwrap(),
            CodecTier::RemoteFx
        );
    }

    #[test]
    fn auto_without_remotefx_lands_on_qoi() {
        let caps = ClientCaps {
            remotefx: false,
            ..ClientCaps::web_client()
        };
        assert_eq!(
            select_tier(&caps, &HardwareCaps::none(), H264Policy::Auto, true).unwrap(),
            CodecTier::Qoi
        );
        // RemoteFX disallowed by the operator has the same effect.
        let caps = ClientCaps::web_client();
        assert_eq!(
            select_tier(&caps, &HardwareCaps::none(), H264Policy::Auto, false).unwrap(),
            CodecTier::Qoi
        );
    }

    #[test]
    fn off_never_picks_avc_even_with_everything_available() {
        let caps = ClientCaps::native_h264_client();
        assert_eq!(
            select_tier(&caps, &gpu(), H264Policy::Off, true).unwrap(),
            CodecTier::RemoteFx
        );
        let caps = ClientCaps {
            remotefx: false,
            ..ClientCaps::native_h264_client()
        };
        assert_eq!(
            select_tier(&caps, &gpu(), H264Policy::Off, true).unwrap(),
            CodecTier::Qoi
        );
    }

    #[test]
    fn software_policy_ignores_the_gpu_but_keeps_h264() {
        let caps = ClientCaps::native_h264_client();
        let tier = select_tier(&caps, &gpu(), H264Policy::Software, true).unwrap();
        if cfg!(feature = "h264") {
            assert_eq!(tier, CodecTier::Avc420Software);
        } else {
            assert_eq!(tier, CodecTier::Avc444);
        }
        // A client without AVC still falls off the ladder entirely.
        let web = ClientCaps::web_client();
        assert_eq!(
            select_tier(&web, &gpu(), H264Policy::Software, true).unwrap(),
            CodecTier::RemoteFx
        );
    }

    #[test]
    fn force_picks_hardware_then_software() {
        let caps = ClientCaps::native_h264_client();
        assert_eq!(
            select_tier(&caps, &gpu(), H264Policy::Force, true).unwrap(),
            CodecTier::Avc420Hardware
        );
        let tier = select_tier(&caps, &HardwareCaps::none(), H264Policy::Force, true).unwrap();
        if cfg!(feature = "h264") {
            assert_eq!(tier, CodecTier::Avc420Software);
        } else {
            assert_eq!(tier, CodecTier::Avc444);
        }
    }

    #[test]
    fn force_with_only_avc444_picks_avc444() {
        let caps = ClientCaps {
            avc420: false,
            ..ClientCaps::native_h264_client()
        };
        assert_eq!(
            select_tier(&caps, &HardwareCaps::none(), H264Policy::Force, false).unwrap(),
            CodecTier::Avc444
        );
    }

    #[test]
    fn force_on_client_without_h264_is_an_error() {
        let caps = ClientCaps::web_client();
        assert!(matches!(
            select_tier(&caps, &gpu(), H264Policy::Force, true),
            Err(CodecError::ClientWithoutH264)
        ));
        // EGFX but no AVC profile at all is the same error.
        let caps = ClientCaps {
            egfx: true,
            avc420: false,
            avc444: false,
            remotefx: true,
            qoi: true,
        };
        assert!(matches!(
            select_tier(&caps, &gpu(), H264Policy::Force, true),
            Err(CodecError::ClientWithoutH264)
        ));
    }

    #[test]
    fn downgrade_walks_the_whole_ladder() {
        let mut tier = CodecTier::Avc420Hardware;
        let mut seen = vec![tier];
        for _ in 0..5 {
            tier = downgrade(tier);
            seen.push(tier);
        }
        assert_eq!(
            seen,
            vec![
                CodecTier::Avc420Hardware,
                CodecTier::Avc420Software,
                CodecTier::Avc444,
                CodecTier::RemoteFx,
                CodecTier::Qoi,
                CodecTier::Qoi,
            ]
        );
    }

    #[test]
    fn requires_h264_matches_tier_kind() {
        assert!(requires_h264(CodecTier::Avc420Hardware));
        assert!(requires_h264(CodecTier::Avc420Software));
        assert!(requires_h264(CodecTier::Avc444));
        assert!(!requires_h264(CodecTier::RemoteFx));
        assert!(!requires_h264(CodecTier::Qoi));
    }

    #[test]
    fn tier_serde_uses_snake_case() {
        assert_eq!(
            serde_json::to_string(&CodecTier::Avc420Hardware).unwrap(),
            "\"avc420_hardware\""
        );
        assert_eq!(
            serde_json::to_string(&CodecTier::RemoteFx).unwrap(),
            "\"remote_fx\""
        );
        assert_eq!(serde_json::to_string(&CodecTier::Qoi).unwrap(), "\"qoi\"");
        assert_eq!(
            serde_json::from_str::<CodecTier>("\"avc420_software\"").unwrap(),
            CodecTier::Avc420Software
        );
    }

    #[test]
    fn supports_avc_needs_egfx() {
        assert!(ClientCaps::native_h264_client().supports_avc());
        assert!(!ClientCaps::web_client().supports_avc());
        let no_egfx_avc = ClientCaps {
            egfx: false,
            avc420: true,
            ..ClientCaps::web_client()
        };
        assert!(!no_egfx_avc.supports_avc());
    }
}
