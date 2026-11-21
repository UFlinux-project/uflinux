//! The descriptor contract, exercised on the real binary.
//!
//! The broker hands the client socket over as an inherited descriptor, so the
//! failure mode that matters is "the descriptor is not there / is not a
//! socket": the session host must say so on one line and exit non-zero. A
//! panic would look like a broker bug in the journal, and a silent exit 0
//! would leave the client hanging on an open connection.

use std::io::Read;
use std::process::{Command, Stdio};

struct Run {
    code: Option<i32>,
    stderr: String,
    stdout: String,
}

/// Runs the session host with the given arguments.
fn session_host(argv: &[&str]) -> Run {
    let mut child = Command::new(env!("CARGO_BIN_EXE_uflinux-rdp-session"))
        // A configuration file that does not exist is legal and yields the
        // defaults, which keeps the test independent of the machine's /etc.
        .args(["--config", "/nonexistent/uflinux-rds-test.json"])
        .args(argv)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the session host binary was built");
    let mut stderr = String::new();
    let mut stdout = String::new();
    if let Some(mut pipe) = child.stderr.take() {
        let _ = pipe.read_to_string(&mut stderr);
    }
    if let Some(mut pipe) = child.stdout.take() {
        let _ = pipe.read_to_string(&mut stdout);
    }
    let status = child.wait().expect("the session host did not exit");
    Run {
        code: exit_code(status.code(), &stderr),
        stderr,
        stdout,
    }
}

/// Turns an exit status into a code, failing the test with the captured output
/// when the process was killed by a signal, which is what a panic looks like.
fn exit_code(code: Option<i32>, stderr: &str) -> Option<i32> {
    assert!(
        !stderr.contains("panicked"),
        "the session host panicked:\n{stderr}"
    );
    code
}

/// A descriptor number the process certainly does not have open.
const CLOSED_FD: &str = "42";

#[test]
fn a_closed_client_descriptor_exits_non_zero_with_one_message() {
    let run = session_host(&["--client-fd", CLOSED_FD]);
    assert_eq!(Some(1), run.code, "stderr was:\n{}", run.stderr);
    assert!(
        run.stderr.contains("not a connected client TCP socket"),
        "expected an fd explanation, got:\n{}",
        run.stderr
    );
    assert!(
        run.stderr.contains(&format!("fd {CLOSED_FD}")),
        "the message should name the descriptor:\n{}",
        run.stderr
    );
    assert_eq!(
        1,
        run.stderr.lines().count(),
        "exactly one line should reach the journal:\n{}",
        run.stderr
    );
}

#[test]
fn a_negative_client_descriptor_on_the_command_line_is_a_usage_error() {
    let run = session_host(&["--client-fd", "-7"]);
    assert_eq!(Some(2), run.code, "stderr was:\n{}", run.stderr);
    assert!(
        run.stderr.contains("--client-fd"),
        "the message should name the flag:\n{}",
        run.stderr
    );
}

#[test]
fn a_negative_client_descriptor_from_the_environment_is_rejected_by_number() {
    // argv rejects a negative number as a bad value, but the environment is
    // copied verbatim by the broker, and `FromRawFd` must not be handed one.
    let mut child = Command::new(env!("CARGO_BIN_EXE_uflinux-rdp-session"))
        .args(["--config", "/nonexistent/uflinux-rds-test.json"])
        .env("UFLINUX_RDS_CLIENT_FD", "-7")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the session host binary was built");
    let mut stderr = String::new();
    if let Some(mut pipe) = child.stderr.take() {
        let _ = pipe.read_to_string(&mut stderr);
    }
    let status = child.wait().expect("the session host did not exit");
    assert_eq!(
        Some(1),
        exit_code(status.code(), &stderr),
        "stderr was:\n{stderr}"
    );
    assert!(
        stderr.contains("not a valid file descriptor"),
        "expected a descriptor-range message, got:\n{stderr}"
    );
}

#[test]
fn a_missing_client_descriptor_says_how_to_supply_one() {
    let run = session_host(&[]);
    assert_eq!(Some(1), run.code, "stderr was:\n{}", run.stderr);
    assert!(
        run.stderr.contains("--client-fd"),
        "the message should name the flag:\n{}",
        run.stderr
    );
    assert!(
        !run.stderr.contains("status"),
        "a missing status fd must not be fatal:\n{}",
        run.stderr
    );
}

#[test]
fn a_descriptor_that_is_not_a_socket_is_reported_as_such() {
    // stdin is /dev/null here: a real descriptor of the wrong kind, which is
    // what a mis-wired `pre_exec` in the broker would hand us.
    let run = session_host(&["--client-fd", "0"]);
    assert_eq!(Some(1), run.code, "stderr was:\n{}", run.stderr);
    assert!(
        run.stderr.contains("not a connected client TCP socket"),
        "expected an fd explanation, got:\n{}",
        run.stderr
    );
}

#[test]
fn the_status_descriptor_may_be_a_closed_one() {
    // The reporter swallows every frame rather than failing the session, so
    // the client socket is what stops the process.
    let run = session_host(&["--client-fd", CLOSED_FD, "--status-fd", "41"]);
    assert_eq!(Some(1), run.code, "stderr was:\n{}", run.stderr);
    assert!(
        run.stderr.contains("client"),
        "the client socket, not the status channel, should be what failed:\n{}",
        run.stderr
    );
}

#[test]
fn an_unknown_flag_exits_with_the_usage_code() {
    let run = session_host(&["--nonsense"]);
    assert_eq!(Some(2), run.code, "stderr was:\n{}", run.stderr);
    assert!(
        run.stderr.contains("see --help"),
        "expected a usage hint, got:\n{}",
        run.stderr
    );
}

#[test]
fn help_exits_cleanly_and_documents_the_descriptor_contract() {
    let run = session_host(&["--help"]);
    assert_eq!(Some(0), run.code, "stderr was:\n{}", run.stderr);
    assert!(
        run.stdout.contains("--client-fd"),
        "stdout was:\n{}",
        run.stdout
    );
    assert!(
        run.stdout.contains("--status-fd"),
        "stdout was:\n{}",
        run.stdout
    );
}
