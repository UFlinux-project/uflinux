//! Frame pacing.
//!
//! `config.codec.max_fps` is a ceiling on how often the desktop is *sent*, not
//! a timer the compositor obeys: `labwc` repaints as fast as it likes and the
//! backend queues frames. Rather than drop frames on the floor and then spin a
//! second timer, the loop asks this gate whether the next frame may go out.
//!
//! The gate is a plain struct over an injectable clock so the arithmetic can be
//! tested without sleeping.

use std::time::{Duration, Instant};

/// Allows at most one frame per `interval`.
#[derive(Debug, Clone, Copy)]
pub struct Pacer {
    interval: Duration,
    next_allowed: Option<Instant>,
}

impl Pacer {
    /// A pacer for `fps` frames per second. A `fps` of zero means "no limit",
    /// which is what a compositor-driven session wants when the operator has
    /// not said otherwise.
    pub fn new(fps: u32, _now: Instant) -> Self {
        Self {
            interval: if fps == 0 {
                Duration::ZERO
            } else {
                Duration::from_nanos(1_000_000_000 / u64::from(fps))
            },
            // The first frame always goes out: a client that has waited for the
            // desktop to come up should not wait for a tick as well.
            next_allowed: None,
        }
    }

    /// The configured ceiling, for logs and status.
    pub fn fps(&self) -> u32 {
        if self.interval.is_zero() {
            0
        } else {
            (1_000_000_000 / self.interval.as_nanos().max(1)) as u32
        }
    }

    /// Whether a frame may be sent at `now`, consuming the slot if so.
    ///
    /// A frame that is not sent is simply not taken from the queue yet — the
    /// backend's channel holds it, and the newest frame wins when several have
    /// piled up (see [`Self::drain_to_newest`]).
    pub fn poll(&mut self, now: Instant) -> bool {
        match self.next_allowed {
            None => {
                self.next_allowed = Some(now + self.interval);
                true
            }
            Some(due) if now >= due => {
                // Re-space from `now`, not from `due`: after a stall (a client
                // that stopped reading, a resize) sending a burst to "catch up"
                // would spike the encoder and buy nothing.
                self.next_allowed = Some(now + self.interval);
                true
            }
            Some(_) => false,
        }
    }

    /// How long until the next slot, `None` when unlimited or immediately due.
    pub fn until_next(&self, now: Instant) -> Option<Duration> {
        self.next_allowed
            .and_then(|due| due.checked_duration_since(now))
            .filter(|wait| !wait.is_zero())
    }

    /// Keeps only the newest of the frames that arrived while the gate was
    /// closed.
    ///
    /// Screen updates are absolute, not incremental: sending an intermediate
    /// frame that the next one already supersedes costs encoder time and
    /// bandwidth for a picture nobody will see.
    pub fn drain_to_newest<T, I: Iterator<Item = T>>(frames: I) -> Option<T> {
        frames.reduce(|_older, newer| newer)
    }
}

/// How long to wait before the next frame, given the gate.
///
/// Split out so the caller can hand the value straight to a tokio timeout
/// without the pacer owning the sleep.
pub fn sleep_until_next(pacer: &Pacer, now: Instant, cap: Duration) -> Duration {
    pacer
        .until_next(now)
        .map_or(Duration::ZERO, |wait| wait.min(cap))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(millis: u64) -> Instant {
        // Instants have no origin, but offsets from one base are consistent,
        // which is all the arithmetic here depends on.
        Instant::now() + Duration::from_millis(millis)
    }

    #[test]
    fn the_first_frame_always_goes_out() {
        let mut pacer = Pacer::new(30, at(0));
        assert!(pacer.poll(at(0)));
    }

    #[test]
    fn a_30fps_gate_opens_every_33ms() {
        let mut pacer = Pacer::new(30, at(0));
        assert!(pacer.poll(at(0)), "first frame");
        assert!(!pacer.poll(at(10)), "too early");
        assert!(!pacer.poll(at(20)), "still too early");
        assert!(pacer.poll(at(34)), "one interval has passed");
        assert!(!pacer.poll(at(50)), "the slot was consumed");
        assert!(pacer.poll(at(70)));
    }

    #[test]
    fn a_zero_fps_gate_never_blocks() {
        let mut pacer = Pacer::new(0, at(0));
        assert_eq!(pacer.fps(), 0);
        for millis in [0, 0, 0, 1] {
            assert!(pacer.poll(at(millis)));
        }
        assert_eq!(pacer.until_next(at(5)), None);
    }

    #[test]
    fn the_reported_rate_round_trips() {
        for fps in [1u32, 5, 30, 60, 120, 240] {
            assert_eq!(Pacer::new(fps, at(0)).fps(), fps);
        }
    }

    #[test]
    fn after_a_stall_the_gate_re_spaces_instead_of_bursting() {
        let mut pacer = Pacer::new(30, at(0));
        assert!(pacer.poll(at(0)));
        // Nothing sent for a second: exactly one frame may go out, not thirty.
        assert!(pacer.poll(at(1000)));
        assert!(!pacer.poll(at(1010)));
        assert!(!pacer.poll(at(1020)));
        assert!(pacer.poll(at(1034)));
    }

    #[test]
    fn until_next_counts_down_to_the_slot() {
        let mut pacer = Pacer::new(10, at(0));
        assert!(pacer.poll(at(0)));
        let wait = pacer.until_next(at(20)).expect("a slot is pending");
        assert!(wait > Duration::from_millis(70) && wait < Duration::from_millis(81));
        assert_eq!(pacer.until_next(at(200)), None, "already due");
    }

    #[test]
    fn the_sleep_is_capped_so_a_dead_client_cannot_stall_shutdown() {
        let mut pacer = Pacer::new(1, at(0));
        assert!(pacer.poll(at(0)));
        let cap = Duration::from_millis(50);
        assert_eq!(sleep_until_next(&pacer, at(10), cap), cap);
        assert_eq!(
            sleep_until_next(&Pacer::new(0, at(0)), at(0), cap),
            Duration::ZERO
        );
    }

    #[test]
    fn only_the_newest_queued_frame_survives() {
        assert_eq!(Pacer::drain_to_newest([1, 2, 3].into_iter()), Some(3));
        assert_eq!(Pacer::drain_to_newest([7].into_iter()), Some(7));
        assert_eq!(Pacer::drain_to_newest(std::iter::empty::<u32>()), None);
    }
}
