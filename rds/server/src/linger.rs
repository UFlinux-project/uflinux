//! The linger window.
//!
//! When a client drops — a laptop lid, a dropped VPN, a browser tab closed by
//! mistake — the desktop it was driving should survive long enough for the same
//! user to reconnect and find their windows where they left them. `linger_secs`
//! is that grace period: the session host keeps the compositor running after
//! the RDP connection is gone and exits when nobody comes back.
//!
//! True session roaming (a *different* process reattaching to a live desktop)
//! is a broker concern and out of scope here: this binary can only delay its
//! own teardown, and the broker is the component that knows whether a reconnect
//! is arriving.

use std::time::Duration;

/// What the session does when the client goes away.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AfterDisconnect {
    /// Tear the desktop down immediately and exit.
    Terminate,
    /// Keep the desktop for `wait` and exit if no client returns.
    Linger {
        /// How long to wait.
        wait: Duration,
    },
}

/// Decides what happens the moment the RDP connection closes.
///
/// `linger_secs == 0` (the default) means the desktop dies with its client,
/// which is what a multi-user box wants: an abandoned desktop holds memory and
/// a Wayland socket forever otherwise. A non-zero value is a grace period, and
/// `resident` overrides the whole thing for developer use — a session started
/// by hand stays up so it can be reconnected to repeatedly.
pub fn plan(linger_secs: u64, resident: bool, client_ever_connected: bool) -> AfterDisconnect {
    if resident {
        // A resident session waits forever, which the caller expresses as the
        // largest representable window rather than a second enum variant.
        return AfterDisconnect::Linger {
            wait: Duration::from_secs(u64::MAX / 2),
        };
    }
    if !client_ever_connected {
        // A client that vanished during negotiation is not a reconnect candidate;
        // holding a desktop for it would let a port scan pin memory.
        return AfterDisconnect::Terminate;
    }
    if linger_secs == 0 {
        AfterDisconnect::Terminate
    } else {
        AfterDisconnect::Linger {
            wait: Duration::from_secs(linger_secs),
        }
    }
}

/// Whether a reconnect that arrives `elapsed` after the disconnect is still
/// inside the window.
///
/// Boundary is inclusive: a reconnect exactly at the deadline is honoured, so a
/// client that takes the full grace period is not cut off by a rounding
/// difference between two clocks.
pub fn within_window(elapsed: Duration, linger: Duration) -> bool {
    elapsed <= linger
}

/// The deadline a linger timer should be set to, `None` when there is no
/// window to keep.
pub fn deadline(now: Duration, linger_secs: u64) -> Option<Duration> {
    match linger_secs {
        0 => None,
        secs => Some(now + Duration::from_secs(secs)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_linger_kills_the_desktop_with_the_client() {
        assert_eq!(
            plan(0, false, true),
            AfterDisconnect::Terminate,
            "the default must not leak desktops"
        );
    }

    #[test]
    fn a_non_zero_linger_becomes_a_window() {
        assert_eq!(
            plan(120, false, true),
            AfterDisconnect::Linger {
                wait: Duration::from_secs(120)
            }
        );
    }

    #[test]
    fn a_client_that_never_finished_negotiating_is_not_worth_a_desktop() {
        assert_eq!(
            plan(600, false, false),
            AfterDisconnect::Terminate,
            "otherwise a connect/disconnect scan pins memory"
        );
    }

    #[test]
    fn a_resident_session_outlives_everything() {
        let AfterDisconnect::Linger { wait } = plan(0, true, false) else {
            panic!("resident must linger even with linger_secs = 0");
        };
        assert!(wait > Duration::from_secs(3600));
    }

    #[test]
    fn the_window_boundary_is_inclusive() {
        let window = Duration::from_secs(60);
        assert!(within_window(Duration::from_secs(59), window));
        assert!(
            within_window(Duration::from_secs(60), window),
            "exactly on the deadline still counts as a reconnect"
        );
        assert!(!within_window(Duration::from_millis(60_001), window));
    }

    #[test]
    fn a_deadline_is_only_set_when_there_is_a_window() {
        assert_eq!(deadline(Duration::from_secs(10), 0), None);
        assert_eq!(
            deadline(Duration::from_secs(10), 5),
            Some(Duration::from_secs(15))
        );
    }
}
