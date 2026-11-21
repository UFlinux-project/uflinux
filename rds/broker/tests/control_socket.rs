//! End-to-end test: the real `uflinux-rdsd` binary, a real config file, a real
//! control socket.
//!
//! Nothing here reaches into the crate — the test speaks the same two
//! newline-delimited JSON dialects as the Settings daemon and
//! `uflinux-rdsctl`, so it fails if the shipped binary and the documented
//! protocol ever drift apart. The daemon is started with ports nobody else
//! holds, needs no root, and never touches `/etc/uflinux`.

use std::io::{Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use uflinux_rds_common::Reply;

/// How long a test waits for something that should have happened.
const WAIT: Duration = Duration::from_secs(10);

/// Tests run in threads of one process, so the pid alone cannot name a
/// scratch directory.
static SEQ: AtomicU32 = AtomicU32::new(0);

/// A running broker and the scratch directory it was given.
struct Broker {
    dir: PathBuf,
    socket: PathBuf,
    child: Child,
}

impl Broker {
    /// Start `uflinux-rdsd` with `config` written to a private file.
    ///
    /// With `use_env` the socket is passed through
    /// `UFLINUX_RDS_CONTROL_SOCKET` instead of `--socket`, which is how the
    /// OpenRC unit starts it.
    fn start(name: &str, config: Value, use_env: bool) -> Self {
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        // `$XDG_RUNTIME_DIR` is where the daemon really keeps its socket; fall
        // back to a temp directory for a bare `cargo test` with no session.
        let root = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .unwrap_or_else(std::env::temp_dir);
        let dir = root.join(format!(
            "uflinux-rdsd-it-{}-{name}-{seq}",
            std::process::id()
        ));
        // A previous run that was killed outright can leave the directory and
        // the socket behind, and the broker would then refuse to start.
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("test directory");

        let config_path = dir.join("rds.json");
        std::fs::write(
            &config_path,
            serde_json::to_vec_pretty(&config).expect("config json"),
        )
        .expect("config file");

        let socket = dir.join("control.sock");
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_uflinux-rdsd"));
        cmd.arg("--config")
            .arg(&config_path)
            .arg("--session-binary")
            .arg("/bin/false")
            .arg("--web-binary")
            .arg("/bin/true")
            .arg("--resident")
            .arg("--take-over")
            .stdin(Stdio::null())
            // Logs stay attached so a hang shows up in the captured output
            // rather than as a mystery.
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if !use_env {
            cmd.arg("--socket").arg(&socket);
        }
        // A cleared environment is the point: `UFLINUX_RDS_*` overrides from a
        // developer's shell must not decide what is being tested. `env_clear`
        // discards assignments made before it, so the socket goes in after.
        cmd.env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", &dir);
        if use_env {
            cmd.env("UFLINUX_RDS_CONTROL_SOCKET", &socket);
        }

        let child = cmd.spawn().expect("the broker starts");
        let broker = Self { dir, socket, child };
        broker.wait_for_listener();
        broker
    }

    /// One request line in, one reply line out, as a raw JSON value.
    fn request(&self, request: &str) -> Value {
        let mut stream = UnixStream::connect(&self.socket).expect("the socket is connectable");
        stream.set_read_timeout(Some(WAIT)).expect("read timeout");
        stream
            .write_all(request.as_bytes())
            .and_then(|()| stream.flush())
            .expect("request written");
        let mut reply = String::new();
        stream
            .read_to_string(&mut reply)
            .expect("the broker answers");
        let line = reply
            .lines()
            .next()
            .unwrap_or_else(|| panic!("no reply line in {reply:?}"));
        serde_json::from_str(line).unwrap_or_else(|error| panic!("{line:?} is not JSON: {error}"))
    }

    /// `{"op":"get_state"}` parsed into the type the Settings daemon uses.
    fn typed_state(&self) -> Reply {
        let raw = self.request(concat!(r#"{"op":"get_state"}"#, "\n"));
        serde_json::from_value(raw).expect("get_state answers with a Reply")
    }

    /// The `state` object of a successful `get_state`.
    fn state(&self) -> Value {
        match self.typed_state() {
            Reply::Ok {
                state: Some(state), ..
            } => serde_json::to_value(state).expect("a snapshot re-serialises"),
            other => panic!("expected an ok reply with a state, got {other:?}"),
        }
    }

    /// Wait until the broker answers and the RDP listener is up.
    fn wait_for_listener(&self) {
        let deadline = Instant::now() + WAIT;
        loop {
            match UnixStream::connect(&self.socket) {
                Ok(_) => break,
                Err(_) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(error) => panic!("no control socket at {:?}: {error}", self.socket),
            }
        }
        loop {
            let state = self.state();
            if state["rdp_listening"] == serde_json::json!(true) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "the listener never came up: {state}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Ask the broker to stop the way a service manager does, and check it
    /// cleaned up after itself.
    fn stop(mut self) {
        let pid = i32::try_from(self.child.id()).expect("a 32-bit pid");
        // SAFETY: signalling our own child with a valid pid.
        let sent = unsafe { libc::kill(pid, libc::SIGTERM) };
        assert_eq!(sent, 0, "SIGTERM reaches the broker");

        let deadline = Instant::now() + WAIT;
        loop {
            match self.child.try_wait().expect("waitpid works") {
                Some(status) => {
                    assert!(status.success(), "the broker exits cleanly: {status:?}");
                    break;
                }
                None if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
                None => {
                    let _ = self.child.kill();
                    panic!("the broker ignored SIGTERM");
                }
            }
        }
        assert!(
            !self.socket.exists(),
            "the control socket is removed on shutdown"
        );
        std::fs::remove_dir_all(&self.dir).expect("scratch directory removed");
    }
}

/// A port the OS just handed out, free at the moment we ask.
fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("an ephemeral port");
    listener.local_addr().expect("a local address").port()
}

/// Configuration that keeps the broker entirely inside its scratch directory.
/// `web.enabled` is false so the supervisor wants no child process at all.
fn test_config() -> Value {
    json!({
        "enabled": true,
        "rdp_bind": "127.0.0.1",
        "rdp_port": free_port(),
        "max_sessions": 2,
        "web": { "enabled": false, "port": free_port() }
    })
}

#[test]
fn the_broker_answers_the_three_read_only_verbs() {
    let config = test_config();
    let rdp_port = config["rdp_port"].as_u64().expect("a port") as u16;
    let broker = Broker::start("verbs", config, false);

    // get_state: the whole snapshot the Settings tab renders. Reaching this
    // point already proves the reply was `{"status":"ok", "state":{...}}`.
    let state = broker.state();
    assert_eq!(state["enabled"], serde_json::json!(true));
    assert_eq!(state["running"], serde_json::json!(true));
    assert_eq!(state["rdp_listening"], serde_json::json!(true));
    // The web child is not wanted while `web.enabled` is false.
    assert_eq!(state["web_listening"], serde_json::json!(false));
    assert_eq!(state["sessions"], serde_json::json!([]));
    assert!(
        state.get("fault").is_none(),
        "a healthy broker reports no fault: {state}"
    );
    // The daemon prefers `effective`, so it has to be there and agree.
    assert!(state.get("effective").is_some(), "effective is present");
    assert_eq!(state["effective"], state["config"]);
    assert_eq!(state["effective"]["rdp_port"], serde_json::json!(rdp_port));

    // list_sessions: the roster on its own.
    let reply = broker.request(concat!(r#"{"op":"list_sessions"}"#, "\n"));
    assert_eq!(reply["status"], serde_json::json!("ok"));
    assert_eq!(reply["sessions"], serde_json::json!([]));
    assert!(
        reply.get("state").is_none(),
        "list_sessions does not pay for a full snapshot"
    );

    // disconnect on an id nobody has: an error, not a silent success.
    let reply = broker.request(concat!(r#"{"op":"disconnect","session":999}"#, "\n"));
    assert_eq!(reply["status"], serde_json::json!("error"));
    let message = reply["message"].as_str().expect("a message");
    assert!(
        message.contains("999"),
        "the error names the session: {message}"
    );

    // Last, so the session this connection spawns cannot race the roster
    // assertions above: the broker forks `/bin/false`, which exits at once.
    // What matters is that the advertised port is the one that answers.
    TcpStream::connect(format!("127.0.0.1:{rdp_port}"))
        .expect("the advertised RDP port accepts connections");

    broker.stop();
}

#[test]
fn the_control_socket_path_can_come_from_the_environment() {
    // This is how the OpenRC unit starts the daemon, so the fallback is part of
    // the contract and not a convenience.
    let broker = Broker::start("env-socket", test_config(), true);
    let state = broker.state();
    assert_eq!(state["enabled"], serde_json::json!(true));
    assert_eq!(state["rdp_listening"], serde_json::json!(true));
    broker.stop();
}

#[test]
fn a_request_that_is_not_json_gets_an_error_and_the_broker_survives() {
    let broker = Broker::start("garbage", test_config(), false);
    let reply = broker.request("this is not a request\n");
    assert_eq!(reply["status"], serde_json::json!("error"));
    assert!(
        reply["message"]
            .as_str()
            .expect("a message")
            .contains("request")
    );

    // Same socket, same process: the next verb still works.
    assert!(broker.state().get("effective").is_some());
    broker.stop();
}

#[test]
fn a_read_only_conversation_leaves_the_config_file_alone() {
    let config = test_config();
    let broker = Broker::start("read-only", config.clone(), false);
    let path = broker.dir.join("rds.json");
    let before = std::fs::read_to_string(&path).expect("the config file");
    let _ = broker.state();
    let _ = broker.request(concat!(r#"{"op":"list_sessions"}"#, "\n"));
    let after = std::fs::read_to_string(&path).expect("the config file");
    assert_eq!(before, after, "reading state must not rewrite the config");
    assert_eq!(
        serde_json::from_str::<Value>(&after).expect("still valid json"),
        config
    );
    broker.stop();
}
