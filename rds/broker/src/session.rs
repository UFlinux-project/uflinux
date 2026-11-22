//! Session spawning, the child↔broker status channel, and the roster plumbing.
//!
//! # The contract the session binary implements
//!
//! The broker owns no RDP protocol code. For every accepted TCP connection it
//! fork/execs the session binary and hands it exactly two descriptors:
//!
//! * **fd 3** — the accepted client socket, in blocking mode. The child speaks
//!   RDP on it and must not assume anything about the listening socket: it is a
//!   TCP socket for a native RDP connection and an `AF_UNIX` socket for one the
//!   web gateway relayed.
//! * **fd 4** — one end of an `AF_UNIX` `SOCK_DGRAM` socketpair. The other end
//!   stays with the broker. Writes are optional: a child that never writes
//!   still appears in the roster, just with less detail.
//!
//! On fd 4 the child sends **newline-delimited JSON objects**, one per
//! datagram, shaped like [`SessionUpdate`]. Every field is optional and the
//! broker merges whatever arrives into the row for that session, so a child can
//! send a single frame after negotiating, or a frame per state change. The
//! broker stamps `connected_seconds`/`idle_seconds` itself, so a child that
//! never reports activity still ages correctly.
//!
//! The child also receives the roster id and the paths it needs as argv flags
//! and `UFLINUX_RDS_*` environment variables (see [`spawn`]), which is how a
//! session binary can re-read `/etc/uflinux/rds.json` for codec policy without
//! knowing anything about the broker's internals.

use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};
use std::path::Path;
use std::process::Stdio;

use serde::Deserialize;
use serde::Serialize;
use tokio::io::Interest;
use tokio::net::UnixDatagram;
use tokio::process::Command;

use crate::error::{Error, Result};
use crate::state::State;

/// Size of the buffer used to read child status frames.
const FRAME_MAX: usize = 4096;

/// Which listener a connection arrived on, told to the child as `--transport`.
///
/// The labels are the same strings the roster and the Settings UI use, and the
/// same ones the session binary's own `Transport` parses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Transport {
    /// The TCP RDP listener: the session host terminates TLS itself.
    Rdp,
    /// The unix socket the web gateway relays into: TLS is already done.
    Web,
}

impl Transport {
    /// The label passed to the child and shown in the roster.
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Rdp => "rdp",
            Self::Web => "web",
        }
    }
}

/// One status frame from a session child.
///
/// This is the wire contract described in the module documentation: a
/// newline-delimited JSON object on fd 4. Unknown fields are ignored so the
/// session binary can add fields later without breaking an older broker, and
/// missing fields simply leave the previous values alone.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct SessionUpdate {
    /// The unix user the session authenticated as.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) username: Option<String>,
    /// `"rdp"` or `"web"`. Lets one binary serve both transports.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) transport: Option<String>,
    /// Codec label the Settings UI maps to a tier: `"h264-vaapi"`, `"h264"`,
    /// `"rfx"`, `"qoi-z"`, `"qoi"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) codec: Option<String>,
    /// Concrete device or library behind `codec`, shown as `codec_detail`.
    /// A present-but-null value (`"codec_device": null`) is a request to clear
    /// a device the child had reported earlier, which is why the outer option
    /// needs its own deserializer: plain `Option` cannot tell "absent" from
    /// "explicit null".
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_codec_device"
    )]
    pub(crate) codec_device: Option<Option<String>>,
    /// Negotiated desktop width.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) width: Option<u16>,
    /// Negotiated desktop height.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) height: Option<u16>,
    /// `true` marks the session as active right now, which resets its idle
    /// age. Use `idle: true` to say "nothing is happening" without lying about
    /// the timestamp.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) activity: Option<bool>,
    /// Alternative spelling of [`SessionUpdate::activity`]: `idle: false` means
    /// active. Both are accepted because a codec loop finds one wording more
    /// natural than the other.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) idle: Option<bool>,
}

/// Maps an explicit `null` to `Some(None)` and a string to `Some(Some(..))`.
/// An absent field never reaches here: serde uses `default` (`None`) instead.
fn deserialize_codec_device<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<Option<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    <Option<String> as serde::Deserialize>::deserialize(deserializer).map(Option::Some)
}

impl SessionUpdate {
    /// Parse one datagram. A frame that is not valid JSON, or not an object of
    /// this shape, is reported to the caller so it can be logged once instead
    /// of silently dropped forever.
    pub(crate) fn parse(bytes: &[u8]) -> std::result::Result<Self, String> {
        let text = String::from_utf8_lossy(bytes);
        let text = text.trim();
        if text.is_empty() {
            return Err("empty frame".to_owned());
        }
        // Tolerate a child that packs several frames into one datagram.
        let first = text
            .lines()
            .find(|line| !line.trim().is_empty())
            .unwrap_or_default();
        serde_json::from_str(first).map_err(|e| e.to_string())
    }
}

/// Everything the broker hands a session child.
pub(crate) struct Spawned {
    /// Child process, reaped by [`watch`].
    pub(crate) child: tokio::process::Child,
    /// Broker's end of the status socketpair.
    pub(crate) status: UnixDatagram,
}

/// An accepted connection the broker can hand a child: either listener's
/// socket, since the child only ever sees a descriptor.
///
/// A trait rather than an enum because the two tokio socket types share no
/// constructor, and the broker needs nothing from them but the descriptor.
pub(crate) trait ClientSocket: Send + 'static {
    /// Detach from the reactor and give up ownership of the descriptor.
    fn into_client_fd(self) -> std::io::Result<RawFd>;
}

impl ClientSocket for tokio::net::TcpStream {
    fn into_client_fd(self) -> std::io::Result<RawFd> {
        self.into_std().map(IntoRawFd::into_raw_fd)
    }
}

impl ClientSocket for tokio::net::UnixStream {
    fn into_client_fd(self) -> std::io::Result<RawFd> {
        self.into_std().map(IntoRawFd::into_raw_fd)
    }
}

/// Fork/exec the session binary for an already-accepted connection.
///
/// `config_path` is passed through so the child can read the codec policy
/// itself; `session_command` is the program the child should put on the
/// display, forwarded verbatim; `transport` names the listener the connection
/// arrived on, which is the only thing the child cannot learn from the socket
/// itself and what decides whether it terminates TLS. The caller registers the
/// child in the roster using the returned pid and then hands the whole bundle
/// to [`watch`].
///
/// # File descriptors
///
/// The child's fd 3 is the client socket and fd 4 is the status channel. Both
/// are installed with `dup2` inside `pre_exec`, which tokio runs after the
/// stdio redirections, so stdin/stdout/stderr stay inherited from the broker
/// (i.e. the daemon's log) and only the two protocol descriptors are added.
pub(crate) fn spawn<S>(
    binary: &Path,
    config_path: &Path,
    session_command: &str,
    stream: S,
    transport: Transport,
) -> Result<Spawned>
where
    S: ClientSocket,
{
    // The accepted socket leaves the tokio reactor here and becomes a plain
    // descriptor for the child.
    let client_fd = stream
        .into_client_fd()
        .map_err(|e| Error::io("hand off client socket", &e))?;

    let mut pair = [0 as RawFd; 2];
    // SAFETY: `pair` is a two-element array matching the length `socketpair`
    // writes, and the domain/type are the supported unix datagram ones.
    let rc = unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_DGRAM, 0, pair.as_mut_ptr()) };
    if rc != 0 {
        let e = std::io::Error::last_os_error();
        return Err(Error::io("create status channel", &e));
    }
    let (broker_fd, child_fd) = (pair[0], pair[1]);

    // The broker's end must be non-blocking before tokio is willing to poll it.
    // SAFETY: `broker_fd` is a freshly created descriptor we now own.
    let std_status =
        unsafe { <std::os::unix::net::UnixDatagram as FromRawFd>::from_raw_fd(broker_fd) };
    let status = match std_status
        .set_nonblocking(true)
        .and_then(|()| UnixDatagram::from_std(std_status))
    {
        Ok(socket) => socket,
        Err(e) => {
            // SAFETY: child_fd has not been handed to anyone yet.
            unsafe { drop(<std::os::unix::net::UnixDatagram as FromRawFd>::from_raw_fd(child_fd)) };
            return Err(Error::io("register status channel", &e));
        }
    };

    let mut command = Command::new(binary);
    command
        .arg("--socket-fd")
        .arg("3")
        .arg("--status-fd")
        .arg("4")
        .arg("--config")
        .arg(config_path)
        .arg("--session-command")
        .arg(session_command)
        .arg("--transport")
        .arg(transport.as_str())
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .env("UFLINUX_RDS_CLIENT_FD", "3")
        .env("UFLINUX_RDS_STATUS_FD", "4")
        .env("UFLINUX_RDS_CONFIG", config_path)
        .env("UFLINUX_RDS_SESSION_COMMAND", session_command)
        .env("UFLINUX_RDS_TRANSPORT", transport.as_str());

    // SAFETY: the closure only calls async-signal-safe functions (`fcntl`,
    // `dup2`) on descriptors owned by the child, and tokio runs it after the
    // fork and after the stdio redirections.
    unsafe {
        command.pre_exec(move || {
            {
                // SAFETY: fcntl/dup2/close on descriptors we own, in the child.
                let flags = libc::fcntl(client_fd, libc::F_GETFL);
                if flags < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                // The acceptor leaves the socket non-blocking; a session binary
                // using plain blocking reads must not see `EAGAIN`.
                if libc::fcntl(client_fd, libc::F_SETFL, flags & !libc::O_NONBLOCK) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::dup2(client_fd, 3) < 0 || libc::dup2(child_fd, 4) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                // Leave the child exactly fds 3 and 4, not the originals too.
                if client_fd > 4 {
                    libc::close(client_fd);
                }
                if child_fd > 4 {
                    libc::close(child_fd);
                }
            }
            Ok(())
        });
    }

    let child = match command.spawn() {
        Ok(child) => child,
        Err(e) => {
            // A `pre_exec` failure surfaces here. The child never ran, so the
            // fds are still ours to release.
            // SAFETY: client_fd was never handed to a child process.
            unsafe {
                drop(<std::os::unix::net::UnixStream as FromRawFd>::from_raw_fd(
                    client_fd,
                ))
            };
            // SAFETY: child_fd was never handed to a child process.
            unsafe { drop(<std::os::unix::net::UnixDatagram as FromRawFd>::from_raw_fd(child_fd)) };
            return Err(Error::io("spawn session binary", &e));
        }
    };
    // The child has its own copy of fd 4 now; the broker's copy must go, or the
    // channel would never signal that the child is gone.
    // SAFETY: child_fd is not used again after this point.
    unsafe { drop(<std::os::unix::net::UnixDatagram as FromRawFd>::from_raw_fd(child_fd)) };

    Ok(Spawned { child, status })
}

/// The pid of a spawned child, for the roster.
pub(crate) fn pid_of(child: &tokio::process::Child) -> u32 {
    child.id().unwrap_or(0)
}

/// Remove a web listener's socket file. Called when the listener stops; a
/// stale file left behind is unlinked by the next bind.
pub(crate) fn remove_socket(path: &Path) {
    let _ = std::fs::remove_file(path);
}

/// Track a spawned child: read its status frames and remove the roster row when
/// it exits.
///
/// The reaper is a task awaiting `Child::wait` rather than a `SIGCHLD` handler
/// because tokio already reaps children it started; a handler would race with
/// that and reap rows the broker does not know about. Both awaited futures are
/// cancel-safe, so they can share one `select!`.
pub(crate) async fn watch(mut spawned: Spawned, state: std::sync::Arc<State>, id: u32) {
    let mut buffer = [0_u8; FRAME_MAX];
    loop {
        tokio::select! {
            frame = spawned.status.recv(&mut buffer) => match frame {
                Ok(len) => match SessionUpdate::parse(&buffer[..len]) {
                    Ok(update) => state.apply_update(id, &update),
                    Err(reason) => {
                        tracing::debug!(session = id, reason = %reason, "ignoring malformed status frame");
                    }
                },
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    // Spurious readiness; wait for the next event.
                    let _ = spawned.status.ready(Interest::READABLE).await;
                }
                Err(_) => break,
            },
            status = spawned.child.wait() => {
                match status {
                    Ok(exit) => {
                        tracing::debug!(session = id, code = ?exit.code(), "session exited");
                    }
                    Err(e) => tracing::debug!(session = id, error = %e, "cannot reap session"),
                }
                break;
            }
        }
    }
    state.remove_session(id);
}

/// Keep a kernel reference to the child for delayed disconnect escalation.
pub(crate) fn open_pidfd(pid: u32) -> std::io::Result<OwnedFd> {
    let pid = i32::try_from(pid)
        .ok().filter(|pid| *pid > 0)
        .ok_or_else(|| std::io::Error::other("invalid session PID"))?;
    // The child is still owned by the caller and has yet to be reaped.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0u32) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd as RawFd) })
}

fn signal_session(fd: &OwnedFd, signal: nix::sys::signal::Signal) -> std::io::Result<()> {
    let result = unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            fd.as_raw_fd(),
            signal as i32,
            std::ptr::null::<libc::siginfo_t>(),
            0u32,
        )
    };
    if result == 0 { Ok(()) } else { Err(std::io::Error::last_os_error()) }
}

/// Ask a session to leave and escalate only against the same process.
pub(crate) fn disconnect(state: &State, id: u32, grace: std::time::Duration) -> bool {
    let Some(target) = state.session_pidfd(id) else {
        return false;
    };
    let _ = signal_session(&target, nix::sys::signal::Signal::SIGTERM);
    tokio::spawn(async move {
        tokio::time::sleep(grace).await;
        if signal_session(&target, nix::sys::signal::Signal::SIGKILL).is_ok() {
            tracing::debug!(session = id, "escalated disconnect to SIGKILL");
        }
    });
    true
}

/// Send a signal to the gateway child, ignoring "already gone".
pub(crate) fn signal(pid: u32, signal: nix::sys::signal::Signal) {
    if let Ok(pid) = i32::try_from(pid) && pid > 0 {
        let _ = nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), signal);
    }
}

/// Terminate every session in the roster: SIGTERM, wait up to `grace`, SIGKILL.
pub(crate) async fn terminate_all(state: &std::sync::Arc<State>, grace: std::time::Duration) {
    let targets: Vec<_> = state.sessions().into_iter()
        .filter_map(|row| state.session_pidfd(row.id))
        .collect();
    for target in &targets {
        let _ = signal_session(target, nix::sys::signal::Signal::SIGTERM);
    }
    let started = std::time::Instant::now();
    while state.session_count() > 0 && started.elapsed() < grace {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    for target in &targets {
        let _ = signal_session(target, nix::sys::signal::Signal::SIGKILL);
    }
}

/// Whether `path` looks like something we can actually exec.
pub(crate) fn is_executable(path: &Path) -> bool {
    match std::fs::metadata(path) {
        Ok(meta) => {
            if !meta.is_file() {
                return false;
            }
            use std::os::unix::fs::PermissionsExt;
            meta.permissions().mode() & 0o111 != 0
        }
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_transport_labels_are_what_the_session_host_expects() {
        // `--transport` is parsed by the session host's `Transport`, so these
        // strings are a wire contract between the two binaries.
        assert_eq!(Transport::Rdp.as_str(), "rdp");
        assert_eq!(Transport::Web.as_str(), "web");
    }

    #[test]
    fn a_partial_frame_leaves_the_other_fields_alone() {
        let frame = SessionUpdate::parse(br#"{"codec":"h264"}"#).unwrap();
        assert_eq!(frame.codec.as_deref(), Some("h264"));
        assert!(frame.username.is_none());
        assert!(frame.codec_device.is_none());
        assert!(frame.width.is_none());
    }

    #[test]
    fn an_empty_frame_is_an_error() {
        assert!(SessionUpdate::parse(b"").is_err());
        assert!(SessionUpdate::parse(b"  \n ").is_err());
    }

    #[test]
    fn a_malformed_or_non_object_frame_is_an_error() {
        assert!(SessionUpdate::parse(b"{").is_err());
        assert!(SessionUpdate::parse(b"null").is_err());
        assert!(SessionUpdate::parse(b"42").is_err());
        assert!(SessionUpdate::parse(b"hello").is_err());
    }

    #[test]
    fn unknown_fields_are_ignored() {
        // The session binary must be able to grow its frames without an
        // older broker rejecting them.
        let frame = SessionUpdate::parse(br#"{"codec":"rfx","future":{"a":1}}"#).unwrap();
        assert_eq!(frame.codec.as_deref(), Some("rfx"));
    }

    #[test]
    fn only_the_first_line_of_a_datagram_is_used() {
        let frame = SessionUpdate::parse(b"\n{\"username\":\"ana\"}\n{\"username\":\"ignored\"}\n")
            .unwrap();
        assert_eq!(frame.username.as_deref(), Some("ana"));
    }

    #[test]
    fn an_explicit_null_device_is_a_clear() {
        let frame = SessionUpdate::parse(br#"{"codec_device":null}"#).unwrap();
        assert_eq!(frame.codec_device, Some(None));
    }

    #[test]
    fn both_activity_spellings_are_accepted() {
        let active = SessionUpdate::parse(br#"{"activity":true}"#).unwrap();
        assert_eq!(active.activity, Some(true));
        let busy = SessionUpdate::parse(br#"{"idle":false}"#).unwrap();
        assert_eq!(busy.idle, Some(false));
    }

    #[test]
    fn the_running_test_binary_counts_as_executable() {
        let me = std::env::current_exe().unwrap();
        assert!(is_executable(&me));
        assert!(!is_executable(Path::new("/definitely/not/here")));
        assert!(
            !is_executable(Path::new("/")),
            "a directory is not executable"
        );
    }
}
