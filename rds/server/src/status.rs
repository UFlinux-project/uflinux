//! Status channel back to the broker.
//!
//! The broker created an `AF_UNIX`/`SOCK_DGRAM` socketpair, kept one end, and
//! handed the other to us as fd 4. Each datagram carries one JSON object whose
//! field names match `uflinux_rds_broker`'s `SessionUpdate` (that type is
//! `pub(crate)`, so the shape is mirrored here rather than shared; the broker
//! ignores unknown fields and tolerates several frames in one datagram).
//!
//! Reporting is best-effort by design: a session must keep serving a user just
//! because the broker stopped reading its roster channel, so every write
//! failure is logged once and then ignored.

use std::os::unix::io::{FromRawFd, RawFd};
use std::os::unix::net::UnixDatagram;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::Serialize;

/// Longest frame we will emit. The broker caps a datagram at the same size.
const FRAME_MAX: usize = 4096;

/// A status frame. Absent fields are left alone by the broker, which is why
/// every field is optional and skipped when `None`.
#[derive(Debug, Clone, Default, Serialize)]
pub struct StatusUpdate {
    /// The user whose desktop this is.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    /// `"rdp"` or `"web"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transport: Option<String>,
    /// Codec label the Settings UI maps to a tier: `"h264-vaapi"`,
    /// `"h264-software"`, `"rfx"`, `"qoi"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub codec: Option<String>,
    /// Encoder device, e.g. `/dev/dri/renderD128`.
    ///
    /// The double option is deliberate and mirrors the broker: an absent field
    /// means "leave whatever you already have", while an explicit JSON `null`
    /// means "the session no longer has a device" (which happens when a
    /// hardware encoder dies and the session falls back to software).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub codec_device: Option<Option<String>>,
    /// Desktop width in pixels.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub width: Option<u16>,
    /// Desktop height in pixels.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub height: Option<u16>,
    /// Whether the session has seen input recently. The broker computes
    /// `idle_seconds` itself from this.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idle: Option<bool>,
}

impl StatusUpdate {
    /// Serialise as one JSON line, without the trailing newline.
    ///
    /// serde already handles the double option the way the broker reads it:
    /// `Some(Some(device))` becomes a quoted string, `Some(None)` becomes an
    /// explicit `null` ("the session no longer has a device"), and the outer
    /// `None` is dropped by `skip_serializing_if` ("leave what you have").
    pub fn to_line(&self) -> String {
        match serde_json::to_string(self) {
            Ok(text) => text,
            Err(e) => {
                tracing::warn!(error = %e, "status frame did not serialise");
                String::from("{}")
            }
        }
    }
}

/// A session's handle on fd 4.
#[derive(Debug)]
pub struct StatusReporter {
    socket: Option<std::os::unix::net::UnixDatagram>,
    /// Set after the first write failure so the log does not fill up.
    broken: AtomicBool,
}

impl StatusReporter {
    /// Takes ownership of `fd`. A `None`, negative, or unusable descriptor
    /// yields a reporter that silently swallows every update.
    pub fn new(fd: Option<RawFd>) -> Self {
        let socket = fd
            .filter(|fd| *fd >= 0)
            .filter(|fd| descriptor_is_open(*fd))
            .map(
                // SAFETY: the broker dup2'd the child end of a socketpair onto this
                // descriptor and closed its own copy, so this process uniquely owns
                // it and is responsible for closing it.
                |fd| unsafe { UnixDatagram::from_raw_fd(fd) },
            );
        if socket.is_none() {
            tracing::debug!("no status channel; the broker will not see live updates");
        }
        Self {
            socket,
            broken: AtomicBool::new(false),
        }
    }

    /// Sends one frame, ignoring failures after logging the first.
    pub fn send(&self, update: &StatusUpdate) {
        let Some(socket) = self.socket.as_ref() else {
            return;
        };
        if self.broken.load(Ordering::Relaxed) {
            return;
        }
        let line = update.to_line();
        if line.len() > FRAME_MAX {
            tracing::debug!(len = line.len(), "status frame too long, dropped");
            return;
        }
        match socket.send(line.as_bytes()) {
            Ok(_) => {}
            Err(e) => {
                self.broken.store(true, Ordering::Relaxed);
                tracing::debug!(error = %e, "status channel closed; reporting stopped");
            }
        }
    }

    /// Reports that the session is (or is no longer) idle.
    pub fn send_idle(&self, idle: bool) {
        self.send(&StatusUpdate {
            idle: Some(idle),
            ..Default::default()
        });
    }
}

/// Whether `fd` refers to an open descriptor.
///
/// `FromRawFd` promises the descriptor is alive; taking a dead one and letting
/// `UnixDatagram` close it is an IO-safety violation that aborts the process in
/// a debug build. `F_GETFD` is the cheapest way to find out before that.
fn descriptor_is_open(fd: RawFd) -> bool {
    // Safety: `fcntl` with `F_GETFD` only reads the descriptor's flags and has
    // no preconditions beyond a plain integer argument.
    unsafe { libc::fcntl(fd, libc::F_GETFD) != -1 }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn json(update: &StatusUpdate) -> serde_json::Value {
        serde_json::from_str(&update.to_line()).expect("frame is JSON")
    }

    #[test]
    fn an_empty_frame_is_an_empty_object() {
        assert_eq!(StatusUpdate::default().to_line(), "{}");
    }

    #[test]
    fn a_full_frame_carries_every_field_the_broker_reads() {
        let frame = json(&StatusUpdate {
            username: Some("panos".to_owned()),
            transport: Some("rdp".to_owned()),
            codec: Some("h264-vaapi".to_owned()),
            codec_device: Some(Some("/dev/dri/renderD128".to_owned())),
            width: Some(1920),
            height: Some(1080),
            idle: Some(false),
        });
        assert_eq!(
            frame,
            serde_json::json!({
                "username": "panos",
                "transport": "rdp",
                "codec": "h264-vaapi",
                "codec_device": "/dev/dri/renderD128",
                "width": 1920,
                "height": 1080,
                "idle": false
            })
        );
    }

    #[test]
    fn a_partial_frame_omits_untouched_fields() {
        let frame = json(&StatusUpdate {
            codec: Some("rfx".to_owned()),
            ..Default::default()
        });
        assert_eq!(frame, serde_json::json!({"codec": "rfx"}));
        assert!(frame.get("codec_device").is_none());
    }

    #[test]
    fn clearing_the_device_emits_an_explicit_null() {
        // The broker's deserializer distinguishes "absent" from "null"; only an
        // explicit null tells it the session no longer has a GPU device.
        let frame = json(&StatusUpdate {
            codec: Some("h264-software".to_owned()),
            codec_device: Some(None),
            ..Default::default()
        });
        assert_eq!(
            frame,
            serde_json::json!({"codec": "h264-software", "codec_device": null})
        );
        assert!(frame["codec_device"].is_null());
    }

    #[test]
    fn a_frame_survives_the_brokers_parser() {
        // Same shape the broker's `SessionUpdate::parse` consumes: one object
        // per line, unknown fields ignored.
        let line = StatusUpdate {
            username: Some("root".to_owned()),
            ..Default::default()
        }
        .to_line();
        let value: serde_json::Value = serde_json::from_str(&line).expect("one object");
        assert_eq!(value["username"], "root");
        assert!(!line.contains('\n'));
    }

    #[test]
    fn a_missing_descriptor_reports_without_panicking() {
        let reporter = StatusReporter::new(None);
        reporter.send(&StatusUpdate {
            username: Some("x".to_owned()),
            ..Default::default()
        });
        reporter.send_idle(true);
    }

    #[test]
    fn a_closed_descriptor_is_refused_rather_than_adopted() {
        // `FromRawFd` on a dead fd is an IO-safety violation: the eventual
        // second `close` aborts the process in a debug build.
        let reporter = StatusReporter::new(Some(1_000_000));
        reporter.send(&StatusUpdate {
            username: Some("x".to_owned()),
            ..Default::default()
        });
        assert!(reporter.socket.is_none());
    }

    #[test]
    fn a_real_datagram_reaches_the_other_end_of_a_socketpair() {
        let mut pair = [0 as std::os::unix::io::RawFd; 2];
        // SAFETY: `pair` is two slots, exactly what socketpair writes, and the
        // domain/type are the supported unix datagram ones.
        let rc = unsafe {
            libc::socketpair(
                libc::AF_UNIX,
                libc::SOCK_DGRAM | libc::SOCK_CLOEXEC,
                0,
                pair.as_mut_ptr(),
            )
        };
        assert_eq!(rc, 0, "socketpair: {}", std::io::Error::last_os_error());

        let reporter = StatusReporter::new(Some(pair[0]));
        reporter.send(&StatusUpdate {
            codec: Some("qoi".to_owned()),
            ..Default::default()
        });

        let mut buf = [0u8; 512];
        // SAFETY: `pair[1]` is the other half of the pair created above and is
        // read exactly once into a 512-byte buffer.
        let n = unsafe { libc::read(pair[1], buf.as_mut_ptr().cast(), buf.len()) };
        assert!(n > 0, "expected a datagram, got {n}");
        assert_eq!(
            std::str::from_utf8(&buf[..n as usize]).expect("utf-8 frame"),
            "{\"codec\":\"qoi\"}"
        );
    }

    #[test]
    fn a_closed_channel_stops_reporting_rather_than_failing_the_session() {
        let mut pair = [0 as std::os::unix::io::RawFd; 2];
        // SAFETY: as above; the pair is created and then immediately closed.
        let rc = unsafe {
            libc::socketpair(
                libc::AF_UNIX,
                libc::SOCK_DGRAM | libc::SOCK_CLOEXEC,
                0,
                pair.as_mut_ptr(),
            )
        };
        assert_eq!(rc, 0);
        // SAFETY: both descriptors were created by the socketpair call above
        // and are not used again after these closes.
        unsafe {
            libc::close(pair[1]);
        }
        let reporter = StatusReporter::new(Some(pair[0]));
        // Nobody is reading, but a unix datagram socket buffers, so the first
        // send succeeds; what matters is that no path panics or blocks.
        reporter.send(&StatusUpdate {
            username: Some("x".to_owned()),
            ..Default::default()
        });
        reporter.send_idle(true);
    }
}
