//! The session host with a client socket that is really there.
//!
//! `closed_fd.rs` covers the ways the inherited descriptor can be wrong, and
//! every one of those fails before the host touches the evented half of the
//! socket. This file covers the case where the descriptor is a live connected
//! stream, which is what the broker hands over on every connection.
//!
//! The regression under test: registering the accepted socket with the tokio
//! reactor has to happen inside the runtime. Called from outside,
//! `TcpStream::from_std` panics with "there is no reactor running", the child
//! dies with a panic instead of a one-line error, and the client sees a
//! connection closed mid-handshake on every single connection.

use std::fs::File;
use std::io::Read;
use std::os::fd::FromRawFd;
use std::os::fd::IntoRawFd;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

/// How long to wait for the child to act on the socket.
const DEADLINE: Duration = Duration::from_secs(20);

fn binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_uflinux-rdp-session"))
}

/// The packaged configuration, wherever this checkout keeps it.
fn config() -> PathBuf {
    if let Ok(path) = std::env::var("UFLINUX_RDS_CONFIG") {
        let path = PathBuf::from(path);
        if path.is_file() {
            return path;
        }
    }
    for candidate in [
        PathBuf::from("/etc/uflinux/rds.json"),
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../data/rds.json"),
    ] {
        if candidate.is_file() {
            return candidate;
        }
    }
    panic!("no rds.json to run a session against");
}

fn invocation() -> Vec<String> {
    vec![
        "--config".to_owned(),
        config().to_string_lossy().into_owned(),
        "--client-fd".to_owned(),
        "0".to_owned(),
        // A compositor that exits at once: the point is the socket, not the
        // desktop, and whatever happens afterwards must still be graceful.
        "--session-command".to_owned(),
        "/bin/true".to_owned(),
    ]
}

#[test]
fn a_live_client_socket_is_registered_without_panicking() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("local addr");
    let mut client = std::net::TcpStream::connect(addr).expect("connect");
    let (accepted, peer) = listener.accept().expect("accept");
    assert_eq!(
        peer.ip(),
        client.local_addr().expect("client addr").ip(),
        "the accepted socket must be the other end of the client's connection"
    );
    drop(listener);

    // Hand the accepted socket to the child as its stdin, so the descriptor
    // survives exec without relying on platform-specific inherit flags, and
    // tell the host to use it. `into_raw_fd` keeps it open until the spawn.
    let raw = accepted.into_raw_fd();
    // Safety: `raw` is the descriptor `accepted` was holding, and `File` now
    // owns it; the child inherits it as fd 0.
    let socket = Stdio::from(unsafe { File::from_raw_fd(raw) });
    let mut child = Command::new(binary())
        .args(invocation())
        .env("RUST_LOG", "off")
        .stdin(socket)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn the session host");

    // The client side must not just sit there: the host takes the socket over
    // and closes it when the session gives up, so a read ends rather than
    // hanging forever. A timeout here means the child never reached the socket.
    client
        .set_read_timeout(Some(DEADLINE))
        .expect("read timeout on the client side");
    let mut buf = [0u8; 64];
    let _ = client.read(&mut buf);

    let output = child.wait_with_output().expect("wait for the session host");
    let stderr = String::from_utf8_lossy(&output.stderr);
    let code = output.status.code();

    assert!(
        !stderr.contains("panicked"),
        "the host panicked instead of failing cleanly (exit {code:?}):\n{stderr}"
    );
    assert!(
        !stderr.contains("no reactor running"),
        "the client socket was registered outside the runtime:\n{stderr}"
    );
    assert!(
        matches!(code, Some(0) | Some(1)),
        "expected a graceful exit (1) or a clean session end (0), got {code:?}:\n{stderr}"
    );
    assert!(
        code == Some(0) || stderr.contains("uflinux-rdp-session:"),
        "a session that cannot complete a handshake must say why (exit {code:?}):\n{stderr}"
    );
}

#[test]
fn a_client_that_hangs_up_is_not_reported_as_a_panic() {
    // Same socket, but the client closes immediately: the host must report a
    // dead peer, not a crash. This is the shape of every "client could not
    // connect" complaint the broker can pass on to the journal.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("local addr");
    // The kernel completes the handshake into the accept backlog, so the
    // connect has to come first: by the time the host reads the peer address
    // the client is already gone.
    let client = std::net::TcpStream::connect(addr).expect("connect");
    drop(client);
    let accepted = listener
        .accept()
        .expect("the connection was in the backlog")
        .0;
    let raw = accepted.into_raw_fd();
    // Safety: as above — the descriptor is live and `File` takes it over.
    let socket = Stdio::from(unsafe { File::from_raw_fd(raw) });

    let output = Command::new(binary())
        .args(invocation())
        .env("RUST_LOG", "off")
        .stdin(socket)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("run the session host");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("panicked"),
        "a client that vanished panicked the host:\n{stderr}"
    );
}
