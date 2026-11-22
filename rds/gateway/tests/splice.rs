//! End-to-end test: a real WebSocket client, the gateway, and a fake session
//! host on the broker's web backend socket. No test-only WebSocket crate
//! exists in the workspace, so the client side is a hand-rolled RFC 6455
//! handshake plus a few frames.
//!
//! The gateway runs on a multi-thread runtime because the client below uses
//! blocking I/O: on a current-thread runtime it would starve the server task.
//!
//! The fake session host is a plain `AF_UNIX` listener with no TLS anywhere,
//! which is the point: a browser session is already encrypted end to end, so
//! the session host must answer X.224 with an empty `SecurityProtocol` and
//! must not start a TLS handshake inside the spliced connection.

use std::collections::BTreeMap;
use std::io::Read as _;
use std::io::Write as _;
use std::net::{SocketAddr, TcpStream};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use axum::serve::ListenerExt as _;
use ironrdp_rdcleanpath::{RDCleanPathMessage, RDCleanPathPdu, TARGET_UNREACHABLE_ERROR_CODE};
use uflinux_rds_common::{Config, WebConfig};
use uflinux_rds_gateway::auth::ConfiguredAuthenticator;
use uflinux_rds_gateway::http::router;
use uflinux_rds_gateway::state::AppState;
use uflinux_rds_gateway::tls::Server;

/// How long a test waits for something that should have happened.
const WAIT: Duration = Duration::from_secs(10);

/// X.224 connect request the gateway must relay to the session host.
const X224_REQUEST: &[u8] = &[
    0x03, 0x00, 0x00, 0x13, 0x0e, 0xe0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x08, 0x00, 0x03, 0x00,
    0x08, 0x00, 0x00,
];
/// Canned X.224 confirm the fake session host answers with (TPKT total length
/// 11). LI is 6, so the TPDU header is exactly its fixed part: no RDP_NEG_REQ,
/// which is an empty `SecurityProtocol`, i.e. standard RDP security.
const X224_CONFIRM: &[u8] = &[
    0x03, 0x00, 0x00, 0x0b, 0x06, 0xd0, 0x00, 0x12, 0x34, 0x00, 0x00,
];
/// Offset of the X.224 length indicator inside [`X224_CONFIRM`].
const LI: usize = 4;
/// Offset of the TPDU code inside [`X224_CONFIRM`].
const CODE: usize = 5;
/// DT_TPDU, the code of a connect confirm.
const TPDU_CONFIRM: u8 = 0xd0;
/// A post-handshake PDU the client pushes through the splice. It is a real
/// TPKT frame because the gateway only hands whole TPKT frames to the browser.
const PAYLOAD: &[u8] = &[0x03, 0x00, 0x00, 0x0a, b'u', b'f', b'l', b'i', b'n', b'x'];
/// First byte of a TLS record. If the session host still expected a TLS
/// handshake inside the connection, this is what the backend would see next.
const TLS_HANDSHAKE_RECORD_TYPE: u8 = 0x16;
/// Only used as the RDCleanPath destination label; nothing dials TCP any more.
const RDP_LABEL: &str = "127.0.0.1:3389";

/// `UFLINUX_RDS_WEB_SOCKET` is process-wide and the gateway reads it when a
/// browser arrives, so the tests take turns.
static BACKEND_ENV: Mutex<()> = Mutex::new(());

/// Point `UFLINUX_RDS_WEB_SOCKET` at `path` and keep it there: the returned
/// guard both holds the lock and is dropped after the test has finished
/// talking to the gateway.
fn use_backend_socket(path: &Path) -> MutexGuard<'static, ()> {
    let guard = BACKEND_ENV
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    // SAFETY: every test in this binary takes `BACKEND_ENV` before changing
    // the environment and holds it until it is done, so no other thread of
    // this process reads the environment concurrently.
    unsafe { std::env::set_var(uflinux_rds_common::ENV_WEB_SOCKET, path) };
    guard
}

/// What the fake session host saw on the socket.
struct BackendSighting {
    /// The X.224 connect request the gateway relayed.
    forwarded: Vec<u8>,
    /// The first bytes that arrive after the confirm. Whole RDP here, or a TLS
    /// record header if the session host wrongly negotiated TLS.
    after_confirm: Vec<u8>,
}

/// Fake session host on the broker's web backend socket. It never sees a TLS
/// handshake and never sends one. Sends nothing at all if the gateway does not
/// connect.
fn fake_backend(path: &Path) -> Receiver<BackendSighting> {
    let listener = UnixListener::bind(path).unwrap();
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        let Ok((mut stream, _)) = listener.accept() else {
            return; // the gateway never connected
        };
        let _ = stream.set_read_timeout(Some(WAIT));
        let mut sighting = BackendSighting {
            forwarded: Vec::new(),
            after_confirm: Vec::new(),
        };
        // Read the forwarded connect request as exactly one TPKT frame.
        let mut header = [0u8; 4];
        if stream.read_exact(&mut header).is_ok() {
            sighting.forwarded.extend_from_slice(&header);
            let total = u16::from_be_bytes([header[2], header[3]]) as usize;
            if total > 4 {
                let mut rest = vec![0u8; total - 4];
                if stream.read_exact(&mut rest).is_ok() {
                    sighting.forwarded.extend_from_slice(&rest);
                }
            }
        }
        if stream.write_all(X224_CONFIRM).is_ok() {
            let _ = stream.flush();
            // Echo whatever the splice forwards until the peer goes away, and
            // remember the first read: that is where a TLS ClientHello would
            // show up if the session host expected an inner handshake.
            let mut buf = [0u8; 1024];
            while let Ok(n) = stream.read(&mut buf) {
                if n == 0 {
                    break;
                }
                if sighting.after_confirm.is_empty() {
                    sighting.after_confirm.extend_from_slice(&buf[..n]);
                }
                if stream.write_all(&buf[..n]).is_err() {
                    break;
                }
                let _ = stream.flush();
            }
        }
        let _ = tx.send(sighting);
    });
    rx
}

/// Minimal WebSocket client: handshake, binary frames both ways.
struct Client {
    stream: TcpStream,
}

impl Client {
    fn connect(addr: SocketAddr) -> Self {
        let mut stream = TcpStream::connect(addr).unwrap();
        stream.set_read_timeout(Some(WAIT)).unwrap();
        let request = format!(
            "GET /ws HTTP/1.1\r\nHost: {addr}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
             Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n"
        );
        stream.write_all(request.as_bytes()).unwrap();
        stream.flush().unwrap();

        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            stream.read_exact(&mut byte).unwrap();
            head.push(byte[0]);
        }
        let head = String::from_utf8_lossy(&head).into_owned();
        assert!(
            head.starts_with("HTTP/1.1 101"),
            "expected 101, got {head:?}"
        );
        Self { stream }
    }

    fn send_binary(&mut self, payload: &[u8]) {
        let mut frame = vec![0x82]; // FIN + binary
        let len = payload.len();
        if len < 126 {
            frame.push(0x80 | len as u8);
        } else if len <= usize::from(u16::MAX) {
            frame.push(0x80 | 126);
            frame.extend_from_slice(&(len as u16).to_be_bytes());
        } else {
            frame.push(0x80 | 127);
            frame.extend_from_slice(&(len as u64).to_be_bytes());
        }
        let mask = [0x37, 0xfa, 0x21, 0x1d];
        frame.extend_from_slice(&mask);
        frame.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
        self.stream.write_all(&frame).unwrap();
        self.stream.flush().unwrap();
    }

    /// Next binary payload, or `None` on close/EOF.
    fn recv_binary(&mut self) -> Option<Vec<u8>> {
        loop {
            let mut head = [0u8; 2];
            if self.stream.read_exact(&mut head).is_err() {
                return None;
            }
            let opcode = head[0] & 0x0f;
            assert_eq!(head[1] & 0x80, 0, "server frames must not be masked");
            let mut len = usize::from(head[1] & 0x7f);
            if len == 126 {
                let mut ext = [0u8; 2];
                self.stream.read_exact(&mut ext).ok()?;
                len = u16::from_be_bytes(ext) as usize;
            } else if len == 127 {
                let mut ext = [0u8; 8];
                self.stream.read_exact(&mut ext).ok()?;
                len = u64::from_be_bytes(ext) as usize;
            }
            let mut payload = vec![0u8; len];
            if self.stream.read_exact(&mut payload).is_err() {
                return None;
            }
            match opcode {
                0x2 => return Some(payload),
                0x8 => return None,
                // Ping/pong/text: keep looking for the payload we care about.
                _ => {}
            }
        }
    }
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("uflinux-rds-gw-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn test_config() -> Config {
    Config {
        enabled: true,
        allowed_users: ["tester".to_owned()].into_iter().collect(),
        web: WebConfig {
            enabled: true,
            // Ephemeral port; the caller asks the listener for its address.
            port: 0,
            bind: "127.0.0.1".into(),
            https: false,
            ..WebConfig::default()
        },
        ..Config::default()
    }
}

fn state_for(config: &Config, dir: &Path) -> AppState {
    AppState::new(
        config.clone(),
        dir.to_path_buf(),
        dir.join("control.sock"),
        Arc::new(ConfiguredAuthenticator {
            users: BTreeMap::from([("tester".to_owned(), "hunter2".to_owned())]),
        }),
    )
}

/// Serve `state` and return its bound address.
fn spawn_gateway(config: &Config, dir: &Path, state: AppState) -> SocketAddr {
    let server = Server::bind(config, dir).unwrap();
    let addr = server.local_addr().unwrap();
    tokio::spawn(async move {
        let app = router(state).into_make_service_with_connect_info::<SocketAddr>();
        axum::serve(server.into_listener().tap_io(|_| ()), app)
            .await
            .unwrap();
    });
    addr
}

fn request_pdu(x224: &[u8], destination: &str, token: &str, pcb: Option<&str>) -> Vec<u8> {
    RDCleanPathPdu::new_request(
        x224.to_vec(),
        destination.to_owned(),
        token.to_owned(),
        pcb.map(str::to_owned),
    )
    .unwrap()
    .to_der()
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_full_session_splices_websocket_to_the_web_backend() {
    let dir = temp_dir("splice");
    let _env = use_backend_socket(&dir.join("backend.sock"));
    let backend = fake_backend(&dir.join("backend.sock"));

    let config = test_config();
    let state = state_for(&config, &dir);
    // The token still carries a destination; it labels the response.
    let token = state.tokens().issue("tester", RDP_LABEL, Instant::now());
    let addr = spawn_gateway(&config, &dir, state);

    let mut client = Client::connect(addr);
    client.send_binary(&request_pdu(
        X224_REQUEST,
        RDP_LABEL,
        &token.secret,
        Some("pcb"),
    ));

    // Exactly one message comes back: the RDCleanPath response with the confirm.
    let response = client.recv_binary().expect("a response PDU");
    let pdu = RDCleanPathPdu::from_der(&response).expect("DER response");
    match pdu.into_message().unwrap() {
        RDCleanPathMessage::Response {
            x224_connection_response,
            server_cert_chain,
            server_addr,
        } => {
            assert_eq!(x224_connection_response.as_bytes(), X224_CONFIRM);
            assert!(
                server_cert_chain.is_empty(),
                "the backend transport is already confidential, so there is no chain to relay"
            );
            assert_eq!(server_addr, RDP_LABEL);
        }
        other => panic!("expected a Response, got {other:?}"),
    }
    // The confirm that was relayed really does select standard RDP security:
    // LI covers only the fixed part, so there is no room for an RDP_NEG_REQ.
    assert_eq!(X224_CONFIRM[LI], 6, "LI: no negotiation field");
    assert_eq!(X224_CONFIRM[CODE], TPDU_CONFIRM, "TPDU code: DT");

    // After the handshake the socket is a raw byte pipe in both directions.
    client.send_binary(PAYLOAD);
    assert_eq!(client.recv_binary().as_deref(), Some(PAYLOAD));

    drop(client);
    let sighting = backend
        .recv_timeout(WAIT)
        .expect("the session host was reached");
    assert_eq!(
        sighting.forwarded, X224_REQUEST,
        "the gateway relayed the unwrapped X.224 CR"
    );
    assert_eq!(
        sighting.after_confirm.first(),
        Some(&PAYLOAD[0]),
        "the browser's first bytes must arrive unchanged, not as a TLS record: got {:?}",
        sighting.after_confirm
    );
    assert_ne!(
        sighting.after_confirm.first(),
        Some(&TLS_HANDSHAKE_RECORD_TYPE),
        "the session host must not start a TLS handshake inside the splice"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reused_token_gets_an_error_pdu_and_never_reaches_the_session_host() {
    let dir = temp_dir("replay");
    let _env = use_backend_socket(&dir.join("backend.sock"));
    let backend = fake_backend(&dir.join("backend.sock"));

    let config = test_config();
    let state = state_for(&config, &dir);
    let token = state.tokens().issue("tester", RDP_LABEL, Instant::now());
    // Burn the token before the browser ever shows up.
    assert!(
        state
            .tokens()
            .consume(&token.secret, Instant::now())
            .is_some()
    );
    let addr = spawn_gateway(&config, &dir, state);

    let mut client = Client::connect(addr);
    client.send_binary(&request_pdu(X224_REQUEST, RDP_LABEL, &token.secret, None));

    let reply = client.recv_binary().expect("an error PDU");
    let pdu = RDCleanPathPdu::from_der(&reply).unwrap();
    match pdu.into_message().unwrap() {
        RDCleanPathMessage::GeneralErr(err) => {
            // 16384: gateway-specific "invalid or already used token".
            assert_eq!(err.error_code, 16_384);
        }
        other => panic!("expected a GeneralErr, got {other:?}"),
    }
    assert_eq!(
        client.recv_binary(),
        None,
        "the gateway closes after the error PDU"
    );

    drop(client);
    assert!(
        backend.recv_timeout(Duration::from_millis(500)).is_err(),
        "a stale token must not reach the session host"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unreachable_backend_reports_target_unreachable() {
    let dir = temp_dir("unreachable");
    // A socket path nothing ever binds: the connect fails with ENOENT.
    let _env = use_backend_socket(&dir.join("nowhere.sock"));

    let config = test_config();
    let state = state_for(&config, &dir);
    let token = state.tokens().issue("tester", RDP_LABEL, Instant::now());
    let addr = spawn_gateway(&config, &dir, state);

    let mut client = Client::connect(addr);
    client.send_binary(&request_pdu(X224_REQUEST, RDP_LABEL, &token.secret, None));

    let reply = client.recv_binary().expect("an error PDU");
    let pdu = RDCleanPathPdu::from_der(&reply).unwrap();
    match pdu.into_message().unwrap() {
        RDCleanPathMessage::GeneralErr(err) => {
            assert_eq!(err.error_code, TARGET_UNREACHABLE_ERROR_CODE)
        }
        other => panic!("expected a GeneralErr, got {other:?}"),
    }
    std::fs::remove_dir_all(&dir).ok();
}
