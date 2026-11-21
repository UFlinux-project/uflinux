//! Surviving an encoder that dies mid-session.
//!
//! The GPU path is an out-of-process ffmpeg helper and the software path is a
//! dynamically loaded OpenH264; either can disappear under a running session —
//! driver reset, OOM killer, a library unmounted by an update. Killing the
//! user's session for that is unkind, so the first failure steps one rung down
//! the codec ladder and carries on. A second failure means the fallback is
//! broken too, and the session ends rather than spinning on an encoder that
//! will not come back.

use uflinux_rds_codec::{CodecTier, downgrade};

/// What the frame loop should do after an encoder failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Keep the session: the tier to use from now on.
    DegradeTo(CodecTier),
    /// End the session; `from` is the tier that was in use.
    GiveUp {
        /// Tier that failed for the second time.
        from: CodecTier,
    },
}

/// One-shot degradation ladder for a single session.
#[derive(Debug, Clone)]
pub struct Degrader {
    tier: CodecTier,
    degraded: bool,
    failures: u32,
}

impl Degrader {
    /// Starts tracking `tier`, the tier chosen at connect time.
    pub const fn new(tier: CodecTier) -> Self {
        Self {
            tier,
            degraded: false,
            failures: 0,
        }
    }

    /// The tier in use right now.
    pub fn tier(&self) -> CodecTier {
        self.tier
    }

    /// Whether the session has already stepped down once.
    pub fn has_degraded(&self) -> bool {
        self.degraded
    }

    /// Total encoder failures seen, for the log line at exit.
    pub fn failures(&self) -> u32 {
        self.failures
    }

    /// Records a failure and decides what the loop does next.
    ///
    /// QOI is terminal: it is a pure function over the framebuffer with no
    /// process or library to die, so a failure reported while on QOI is a bug
    /// or a dead client, not something a downgrade can fix.
    pub fn failure(&mut self) -> Action {
        self.failures += 1;
        if self.degraded || self.tier == CodecTier::Qoi {
            return Action::GiveUp { from: self.tier };
        }
        let next = downgrade(self.tier);
        self.tier = next;
        self.degraded = true;
        Action::DegradeTo(next)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_failure_steps_one_rung_down() {
        let mut d = Degrader::new(CodecTier::Avc420Hardware);
        assert_eq!(d.failure(), Action::DegradeTo(CodecTier::Avc420Software));
        assert_eq!(d.tier(), CodecTier::Avc420Software);
        assert!(d.has_degraded());
    }

    #[test]
    fn a_second_failure_ends_the_session() {
        let mut d = Degrader::new(CodecTier::Avc420Hardware);
        d.failure();
        assert_eq!(
            d.failure(),
            Action::GiveUp {
                from: CodecTier::Avc420Software
            }
        );
        assert_eq!(d.failures(), 2);
    }

    #[test]
    fn every_tier_has_a_terminal_end() {
        // Walk each starting tier to destruction: the machine must always stop,
        // never loop forever between two rungs.
        for start in [
            CodecTier::Avc420Hardware,
            CodecTier::Avc420Software,
            CodecTier::Avc444,
            CodecTier::RemoteFx,
            CodecTier::Qoi,
        ] {
            let mut d = Degrader::new(start);
            let mut steps = 0;
            loop {
                steps += 1;
                match d.failure() {
                    Action::GiveUp { .. } => break,
                    Action::DegradeTo(_) => {}
                }
                assert!(steps < 5, "{start:?} never settled");
            }
            // QOI is already the bottom rung, so it gives up on the first
            // failure; everything above it gets exactly one step down.
            let expected = if start == CodecTier::Qoi { 1 } else { 2 };
            assert_eq!(steps, expected, "{start:?} should stop after {expected}");
        }
    }

    #[test]
    fn a_failure_while_on_qoi_gives_up_immediately() {
        // QOI has no encoder to restart, so there is nothing left to fall back to.
        let mut d = Degrader::new(CodecTier::Qoi);
        assert_eq!(
            d.failure(),
            Action::GiveUp {
                from: CodecTier::Qoi
            }
        );
    }

    #[test]
    fn a_healthy_session_never_touches_the_ladder() {
        let d = Degrader::new(CodecTier::RemoteFx);
        assert_eq!(d.tier(), CodecTier::RemoteFx);
        assert!(!d.has_degraded());
        assert_eq!(d.failures(), 0);
    }
}
