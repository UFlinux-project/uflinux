//! Policy knobs for the RemoteFX tier.
//!
//! ironrdp-server always compiles RemoteFX in and owns the actual encoder, so
//! this crate contributes only the decisions: which entropy coder to use and
//! how the operator's `rfx_quant` maps onto the wire.
//!
//! # Mapping onto the ironrdp types
//!
//! `ironrdp-pdu` is not a dependency of this crate, so the server crate does the
//! last step:
//!
//! * [`Entropy`] maps onto
//!   `ironrdp_pdu::codecs::rfx::frame::EntropyBits` — `Rlgr1` is the value 1
//!   (`ENTROPY_RLGR1`), `Rlgr3` is 4 (`ENTROPY_RLGR3`).
//! * [`quant_subbands`] maps the single `CodecConfig::rfx_quant` byte onto the
//!   ten `ironrdp_pdu::codecs::rfx::data_messages::Quant` subbands. `Quant` only
//!   accepts 6..=15 (`Quant::VALID_RANGE`, enforced by `Quant::try_new`), so the
//!   0..=63 config value is clamped; the returned tuple is in `try_new`'s
//!   parameter order.

use serde::{Deserialize, Serialize};

/// RemoteFX entropy coding, as negotiated in the FRAME_BITS message.
///
/// Serde uses `snake_case` (`"rlgr1"`, `"rlgr3"`), matching the enums in
/// `uflinux-rds-common`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Entropy {
    /// RLGR mode 1. Supported by every RemoteFX client.
    Rlgr1,
    /// RLGR mode 3. Better compression, needs `entropy_bits` support.
    Rlgr3,
}

impl Entropy {
    /// Parses `rlgr1`/`rlgr3`, case-insensitively, also accepting `1`/`3`.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "rlgr1" | "1" => Some(Self::Rlgr1),
            "rlgr3" | "3" => Some(Self::Rlgr3),
            _ => None,
        }
    }

    /// The value used on the wire in `FRAME_BITS`.
    pub const fn entropy_bits(self) -> u8 {
        match self {
            Self::Rlgr1 => 1,
            Self::Rlgr3 => 4,
        }
    }
}

/// Everything this crate decides about the RemoteFX tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteFxPolicy {
    /// The operator's single quantisation byte, straight from
    /// `CodecConfig::rfx_quant` (0..=63).
    pub quant: u8,
    /// The entropy coder chosen for this connection.
    pub entropy: Entropy,
}

impl RemoteFxPolicy {
    /// Builds the policy from the shared config plus the negotiated coder.
    pub fn new(quant: u8, entropy: Entropy) -> Self {
        Self { quant, entropy }
    }

    /// The subband quantisation values to hand to `Quant::try_new`.
    pub fn subbands(&self) -> [u8; 10] {
        quant_subbands(self.quant)
    }
}

/// Picks the entropy coder: the preferred one when the client offered it,
/// otherwise the first offered coder, otherwise `None`.
///
/// Mirrors `pick_remotefx_entropy_coder` in ironrdp-server, kept here so the
/// choice can be unit-tested without a live connection.
pub fn pick_entropy(preferred: Option<Entropy>, offered: &[Entropy]) -> Option<Entropy> {
    preferred
        .filter(|want| offered.contains(want))
        .or_else(|| offered.first().copied())
}

/// Expands the single config byte into the ten `Quant` subbands, clamped into
/// the range the wire format allows.
///
/// The shape follows the Windows server defaults (low frequencies quantised
/// more finely than high frequencies): the config byte is treated as a bias
/// around the middle of the valid range, so `rfx_quant` 20 (the default) lands
/// on the Windows values.
///
/// Returned in `Quant::try_new` order: `ll3, lh3, hl3, hh3, lh2, hl2, hh2,
/// lh1, hl1, hh1`.
pub fn quant_subbands(quant: u8) -> [u8; 10] {
    // Valid wire values are 6..=15; the Windows default pattern is
    // 6,6,6,6 7,7,8 8,8,9. Bias that pattern by (quant - 20) / 5 so the
    // operator's 0..=63 byte slides the whole set within the legal window.
    let bias = (i16::from(quant) - 20) / 5;
    const BASE: [i16; 10] = [6, 6, 6, 6, 7, 7, 8, 8, 8, 9];
    let mut out = [6u8; 10];
    for (index, base) in BASE.iter().enumerate() {
        out[index] = (base + bias).clamp(6, 15) as u8;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preferred_coder_wins_when_offered() {
        use Entropy::{Rlgr1, Rlgr3};
        assert_eq!(
            pick_entropy(Some(Rlgr3), &[Rlgr1, Rlgr3]),
            Some(Rlgr3),
            "preferred coder was ignored"
        );
        assert_eq!(pick_entropy(Some(Rlgr1), &[Rlgr1, Rlgr3]), Some(Rlgr1));
    }

    #[test]
    fn first_offered_coder_wins_when_the_preference_is_unavailable() {
        use Entropy::{Rlgr1, Rlgr3};
        assert_eq!(
            pick_entropy(Some(Rlgr3), &[Rlgr1]),
            Some(Rlgr1),
            "unavailable preference did not fall back"
        );
        assert_eq!(pick_entropy(None, &[Rlgr3, Rlgr1]), Some(Rlgr3));
    }

    #[test]
    fn nothing_offered_means_no_coder() {
        assert_eq!(pick_entropy(Some(Entropy::Rlgr1), &[]), None);
        assert_eq!(pick_entropy(None, &[]), None);
    }

    #[test]
    fn entropy_parses_the_names_the_settings_ui_writes() {
        assert_eq!(Entropy::parse("rlgr3"), Some(Entropy::Rlgr3));
        assert_eq!(Entropy::parse(" RLGR1 "), Some(Entropy::Rlgr1));
        assert_eq!(Entropy::parse("3"), Some(Entropy::Rlgr3));
        assert_eq!(Entropy::parse("rlgr2"), None);
        assert_eq!(Entropy::parse(""), None);
    }

    #[test]
    fn entropy_bits_match_the_wire_format() {
        assert_eq!(Entropy::Rlgr1.entropy_bits(), 1);
        assert_eq!(Entropy::Rlgr3.entropy_bits(), 4);
    }

    #[test]
    fn default_quant_reproduces_the_windows_subbands() {
        assert_eq!(
            quant_subbands(20),
            [6, 6, 6, 6, 7, 7, 8, 8, 8, 9],
            "default quant did not map to the Windows pattern"
        );
    }

    #[test]
    fn extreme_quant_values_stay_inside_the_legal_range() {
        for quant in [0u8, 1, 10, 20, 30, 63] {
            for subband in quant_subbands(quant) {
                assert!((6..=15).contains(&subband), "{quant} -> {subband}");
            }
        }
        // The floor saturates completely, the ceiling saturates the coarse
        // high-frequency bands first; the pattern never leaves 6..=15.
        assert!(quant_subbands(0).iter().all(|&v| v == 6));
        assert!(quant_subbands(63).iter().all(|&v| v >= 14));
        assert_eq!(quant_subbands(63)[9], 15);
        assert_eq!(quant_subbands(64), quant_subbands(63));
    }

    #[test]
    fn policy_serde_uses_snake_case() {
        let policy = RemoteFxPolicy::new(20, Entropy::Rlgr3);
        let json = serde_json::to_string(&policy).unwrap();
        assert_eq!(json, r#"{"quant":20,"entropy":"rlgr3"}"#);
        assert_eq!(
            serde_json::from_str::<Entropy>("\"rlgr1\"").unwrap(),
            Entropy::Rlgr1
        );
    }
}
