//! What the client turned out to support, and which codec we will therefore use.
//!
//! Two steps, kept apart so both can be tested without a client:
//!
//! 1. [`Negotiation`] collects the facts the server learned while the client
//!    was coming up — the EGFX capability set that was agreed, whether the
//!    RemoteFX surface bits survived negotiation, whether a QOI codec was
//!    accepted — and reduces them to a `uflinux_rds_codec::ClientCaps`.
//! 2. [`decide`] feeds those caps, the hardware we could actually find, and the
//!    operator's `H264Policy` into `select_tier`, then applies the one thing the
//!    codec crate cannot know: this build has no AVC444 *encoder*.
//!
//! The RemoteFX/QOI half of the negotiation is not visible to an embedder as a
//! callback at this IronRDP revision — `ironrdp-server` consumes the client's
//! `BitmapCodecs` capability itself (`server.rs:5262-5300`) and only lets us
//! steer it through builder options — so `remotefx`/`qoi` here are derived from
//! what we advertised and the policy that gated it, not read back from the
//! client.

use uflinux_rds_codec::{ClientCaps, CodecTier, HardwareCaps, select_tier};
use uflinux_rds_common::H264Policy;

/// The EGFX half of the negotiation, once the graphics pipeline has agreed a
/// capability set. Mirrors the two fields of
/// `ironrdp_egfx::server::CodecCapabilities` that decide a codec tier; that
/// struct is `#[non_exhaustive]`, so it is copied out rather than stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct EgxFacts {
    /// The client will take AVC420 (H.264 4:2:0) frames.
    pub avc420: bool,
    /// The client will take AVC444 (H.264 4:4:4) frames.
    pub avc444: bool,
}

/// Everything known about the client's graphics support once it is up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Negotiation {
    /// `Some` when the client opened the graphics pipeline channel and a
    /// capability set was agreed. `None` means no EGFX at all, which rules out
    /// every AVC tier.
    pub egfx: Option<EgxFacts>,
    /// RemoteFX surface commands were accepted by both sides.
    pub remotefx: bool,
    /// A QOI (or QOIz) bitmap codec was accepted.
    pub qoi: bool,
}

impl Negotiation {
    /// Reduces the negotiation to the input `select_tier` expects.
    pub fn client_caps(&self) -> ClientCaps {
        ClientCaps {
            egfx: self.egfx.is_some(),
            avc420: self.egfx.is_some_and(|facts| facts.avc420),
            avc444: self.egfx.is_some_and(|facts| facts.avc444),
            remotefx: self.remotefx,
            qoi: self.qoi,
        }
    }

    /// Reads the EGFX facts out of a negotiated capability set.
    #[cfg(feature = "egfx")]
    pub fn set_egfx(&mut self, caps: &ironrdp_egfx::server::CodecCapabilities) {
        self.egfx = Some(EgxFacts {
            avc420: caps.avc420,
            avc444: caps.avc444,
        });
    }

    /// A client that never reached the graphics pipeline: RemoteFX/QOI only.
    pub const fn without_egfx(remotefx: bool, qoi: bool) -> Self {
        Self {
            egfx: None,
            remotefx,
            qoi,
        }
    }
}

/// Why a codec decision refused the connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecideError {
    /// `h264 = "force"` and this client cannot do H.264. Refusing is the
    /// operator's instruction, so the session must not silently fall back.
    ClientWithoutH264,
    /// The policy demands H.264 and this build has no encoder for it.
    NoEncoder,
}

impl core::fmt::Display for DecideError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::ClientWithoutH264 => {
                f.write_str("client does not support H.264 but the configuration requires it")
            }
            Self::NoEncoder => f.write_str(
                "H.264 is required but this build has no H.264 encoder (enable h264-vaapi or h264-software)",
            ),
        }
    }
}

impl std::error::Error for DecideError {}

/// The outcome of a codec decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    /// The tier to drive for the rest of the session.
    pub tier: CodecTier,
    /// The encoder device to report to the broker, if the tier uses one.
    pub device: Option<String>,
    /// Why the tier is lower than the client and hardware would allow, for the
    /// log. Informational only.
    pub note: Option<String>,
}

/// Chooses the tier for a session.
///
/// `h264_available` says whether *this binary* can encode H.264 at all: the
/// codec crate's ladder assumes an encoder exists once the client and policy
/// allow AVC, but the encoders live behind the `h264-vaapi` / `h264-software`
/// cargo features, so under `H264Policy::Force` a feature-less build has to
/// refuse rather than promise frames it cannot produce.
///
/// `CodecTier::Avc444` is remapped downward: `ironrdp-egfx` ships an AVC444
/// *decoder* (`ironrdp-egfx/src/avc444.rs`) but no encoder, so there is nothing
/// to feed the frames to.
pub fn decide(
    negotiation: &Negotiation,
    hardware: &HardwareCaps,
    policy: H264Policy,
    allow_remotefx: bool,
    h264_available: bool,
) -> Result<Decision, DecideError> {
    let caps = negotiation.client_caps();
    if policy == H264Policy::Force && !caps.supports_avc() {
        return Err(DecideError::ClientWithoutH264);
    }
    if policy == H264Policy::Force && !h264_available {
        return Err(DecideError::NoEncoder);
    }

    // A build without an encoder must not be offered the AVC rungs at all;
    // pretending the hardware is absent is exactly how `H264Policy::Software`
    // is handled inside the ladder, so reuse that path.
    let effective_hardware = if h264_available {
        hardware.clone()
    } else {
        HardwareCaps::NONE
    };

    let tier = select_tier(&caps, &effective_hardware, policy, allow_remotefx)
        .map_err(|_| DecideError::NoEncoder)?;

    // The ladder's software rung is gated on the *codec* crate's `h264`
    // feature, which `h264-software` turns on; a server build that enables
    // egfx without an encoder must still not be handed an AVC tier it cannot
    // drive, so `h264_available` has the final say here.
    if !h264_available && tier.requires_h264() {
        let mut fallback = tier;
        while fallback.requires_h264() {
            fallback = uflinux_rds_codec::downgrade(fallback);
        }
        return Ok(Decision {
            tier: fallback,
            device: None,
            note: Some(format!(
                "this build has no H.264 encoder; using {fallback} instead of {tier}"
            )),
        });
    }

    if tier == CodecTier::Avc444 {
        // `ironrdp-egfx` ships an AVC444 decoder (`ironrdp-egfx/src/avc444.rs`)
        // and no encoder, so the tier cannot be driven. Under Force that is a
        // refusal — the operator said "never below AVC" — otherwise it is a
        // logged substitution.
        if policy == H264Policy::Force {
            return Err(DecideError::NoEncoder);
        }
        let fallback = uflinux_rds_codec::downgrade(CodecTier::Avc444);
        return Ok(Decision {
            tier: fallback,
            device: None,
            note: Some(format!(
                "client offered AVC444 but this build has no AVC444 encoder; using {fallback}"
            )),
        });
    }

    let device = match tier {
        CodecTier::Avc420Hardware => hardware.device.clone(),
        _ => None,
    };
    Ok(Decision {
        tier,
        device,
        note: None,
    })
}

/// The label the Settings UI maps to a codec tier.
///
/// Deliberately not `CodecTier`'s own `Display`: the daemon's mapping
/// (`settings/daemon/src/rdp.rs::codec_tier`) expects `"h264-vaapi"` and
/// `"h264-software"`, and `"remote_fx"` would land on the QOI bucket.
pub fn codec_label(tier: CodecTier) -> &'static str {
    match tier {
        CodecTier::Avc420Hardware => "h264-vaapi",
        CodecTier::Avc420Software => "h264-software",
        // Reported as the software tier: what the user sees is CPU H.264, and
        // the AVC444 note in the log explains the substitution.
        CodecTier::Avc444 => "h264-software",
        CodecTier::RemoteFx => "rfx",
        CodecTier::Qoi => "qoi",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hw() -> HardwareCaps {
        HardwareCaps {
            vaapi: true,
            device: Some("/dev/dri/renderD128".to_owned()),
        }
    }

    fn mstsc() -> Negotiation {
        Negotiation {
            egfx: Some(EgxFacts {
                avc420: true,
                avc444: true,
            }),
            remotefx: true,
            qoi: true,
        }
    }

    fn web() -> Negotiation {
        Negotiation {
            egfx: None,
            remotefx: true,
            qoi: true,
        }
    }

    fn decide_ok(n: &Negotiation, hw: &HardwareCaps, policy: H264Policy) -> Decision {
        decide(n, hw, policy, true, true).expect("decide")
    }

    #[test]
    fn caps_are_reduced_from_the_negotiation_record() {
        let caps = mstsc().client_caps();
        assert!(caps.egfx && caps.avc420 && caps.avc444 && caps.remotefx && caps.qoi);
        assert!(caps.supports_avc());

        let no_egfx = Negotiation::without_egfx(true, true).client_caps();
        assert!(!no_egfx.egfx);
        assert!(!no_egfx.supports_avc(), "no pipeline means no AVC");

        let avc444_only = Negotiation {
            egfx: Some(EgxFacts {
                avc420: false,
                avc444: true,
            }),
            remotefx: false,
            qoi: false,
        }
        .client_caps();
        assert!(avc444_only.supports_avc());
    }

    #[test]
    fn auto_policy_with_a_gpu_and_mstsc_picks_the_hardware_tier() {
        let decision = decide_ok(&mstsc(), &hw(), H264Policy::Auto);
        assert_eq!(decision.tier, CodecTier::Avc420Hardware);
        assert_eq!(
            decision.device.as_deref(),
            Some("/dev/dri/renderD128"),
            "the device has to reach the broker so the UI can name the GPU"
        );
        assert_eq!(codec_label(decision.tier), "h264-vaapi");
    }

    #[test]
    #[cfg(feature = "h264-software")]
    fn software_policy_never_reports_a_device() {
        // `Software` means "H.264 is fine, keep the GPU out of it", so the
        // ladder must be walked as if there were no VA-API even though there is.
        let decision = decide_ok(&mstsc(), &hw(), H264Policy::Software);
        assert_eq!(decision.tier, CodecTier::Avc420Software);
        assert_eq!(decision.device, None);
        assert_eq!(codec_label(decision.tier), "h264-software");
    }

    #[test]
    #[cfg(not(feature = "h264-software"))]
    fn software_policy_never_reports_a_device() {
        // Same rule, but this build has no software encoder at all (the ladder's
        // software rung is compiled out), so the honest answer is the best tier
        // the binary can actually drive. Still never the GPU.
        let decision = decide_ok(&mstsc(), &hw(), H264Policy::Software);
        assert_eq!(decision.tier, CodecTier::RemoteFx);
        assert_eq!(decision.device, None);
        assert_eq!(codec_label(decision.tier), "rfx");
    }

    #[test]
    fn off_policy_skips_every_avc_tier_even_for_a_perfect_client() {
        let decision = decide_ok(&mstsc(), &hw(), H264Policy::Off);
        assert_eq!(decision.tier, CodecTier::RemoteFx);
        assert_eq!(codec_label(decision.tier), "rfx");
    }

    #[test]
    fn force_policy_refuses_a_client_without_h264() {
        let err = decide(&web(), &hw(), H264Policy::Force, true, true)
            .expect_err("a web client cannot satisfy Force");
        assert_eq!(err, DecideError::ClientWithoutH264);
    }

    #[test]
    fn force_policy_refuses_a_build_without_an_encoder() {
        // Better a clear refusal at connect time than a session that renders
        // nothing because the encoder was compiled out.
        let err = decide(&mstsc(), &hw(), H264Policy::Force, true, false)
            .expect_err("no encoder in this build");
        assert_eq!(err, DecideError::NoEncoder);
    }

    #[test]
    fn a_build_without_an_encoder_falls_back_under_auto() {
        let decision = decide(&mstsc(), &hw(), H264Policy::Auto, true, false).expect("decide");
        assert_eq!(decision.tier, CodecTier::RemoteFx);
    }

    #[test]
    fn avc444_is_substituted_because_there_is_no_encoder_for_it() {
        // Client offers only AVC444 (EGFX V8.1 style), so the ladder lands on
        // Avc444; we have nothing that can produce those bytes.
        let only_444 = Negotiation {
            egfx: Some(EgxFacts {
                avc420: false,
                avc444: true,
            }),
            remotefx: true,
            qoi: true,
        };
        let decision = decide_ok(&only_444, &HardwareCaps::NONE, H264Policy::Auto);
        assert_eq!(decision.tier, CodecTier::RemoteFx);
        assert!(
            decision.note.is_some(),
            "the substitution must be explained in the log"
        );
    }

    #[test]
    fn a_web_client_gets_a_codec_it_can_decode() {
        // The wasm client negotiates no EGFX, so H.264 is impossible no matter
        // what the operator asked for (unless Force, which refuses it).
        for policy in [H264Policy::Auto, H264Policy::Software, H264Policy::Off] {
            let decision = decide_ok(&web(), &hw(), policy);
            assert!(
                matches!(decision.tier, CodecTier::RemoteFx | CodecTier::Qoi),
                "{policy:?} gave {}",
                decision.tier
            );
        }
    }

    #[test]
    fn disabling_remotefx_leaves_qoi_as_the_only_fallback() {
        let decision =
            decide(&web(), &HardwareCaps::NONE, H264Policy::Off, false, true).expect("decide");
        assert_eq!(decision.tier, CodecTier::Qoi);
        assert_eq!(codec_label(decision.tier), "qoi");
    }

    #[test]
    fn a_client_with_no_gpu_and_no_avc_decoder_still_gets_a_tier() {
        let decision = decide_ok(&mstsc(), &HardwareCaps::NONE, H264Policy::Auto);
        // No VA-API and no h264 feature-independent software path in the ladder:
        // the answer must be something drivable, not an error.
        assert!(!decision.tier.requires_h264() || decision.tier == CodecTier::Avc420Software);
    }
}
