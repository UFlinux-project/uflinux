//! End-to-end test: a real WebSocket client, the gateway, and a fake RDP
//! listener. No test-only WebSocket crate exists in the workspace, so the
//! client side is a hand-rolled RFC 6455 handshake plus a few frames.
//!
//! The gateway runs on a multi-thread runtime because the client below uses
//! blocking I/O: on a current-thread runtime it would starve the server task.

use std::collections::BTreeMap;
use std::io::Read as _;
use std::io::Write as _;
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, channel};
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

/// X.224 connect request the gateway must relay to the fake listener.
const X224_REQUEST: &[u8] = &[
    0x03, 0x00, 0x00, 0x13, 0x0e, 0xe0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x08, 0x00, 0x03, 0x00,
    0x08, 0x00, 0x00,
];
/// Canned X.224 confirm the fake listener answers with (TPKT total length 11).
const X224_CONFIRM: &[u8] = &[
    0x03, 0x00, 0x00, 0x0b, 0x06, 0xd0, 0x00, 0x12, 0x34, 0x00, 0x00,
];
/// A post-handshake PDU the client pushes through the splice. It is a real
/// TPKT frame because the gateway only hands whole TPKT frames to the browser.
const PAYLOAD: &[u8] = &[0x03, 0x00, 0x00, 0x0a, b'u', b'f', b'l', b'i', b'n', b'x'];

/// Fake RDP listener. Sends the bytes the gateway forwarded once the session
/// ends; sends nothing at all if the gateway never connects.
fn fake_listener() -> (SocketAddr, Receiver<Vec<u8>>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        let Ok((mut stream, _)) = listener.accept() else {
            return; // the gateway never connected
        };
        let mut received = Vec::new();
        // Read the forwarded connect request as exactly one TPKT frame.
        let mut header = [0u8; 4];
        if stream.read_exact(&mut header).is_ok() {
            received.extend_from_slice(&header);
            let total = u16::from_be_bytes([header[2], header[3]]) as usize;
            if total > 4 {
                let mut rest = vec![0u8; total - 4];
                if stream.read_exact(&mut rest).is_ok() {
                    received.extend_from_slice(&rest);
                }
            }
        }
        if stream.write_all(X224_CONFIRM).is_ok() {
            let _ = stream.flush();
            // Echo whatever the splice forwards until the peer goes away.
            let mut buf = [0u8; 1024];
            while let Ok(n) = stream.read(&mut buf) {
                if n == 0 || stream.write_all(&buf[..n]).is_err() {
                    break;
                }
                let _ = stream.flush();
            }
        }
        let _ = tx.send(received);
    });
    (addr, rx)
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

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("uflinux-rds-gw-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn test_config(rdp_port: u16) -> Config {
    Config {
        enabled: true,
        rdp_port,
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

fn state_for(config: &Config, dir: &std::path::Path) -> AppState {
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
fn spawn_gateway(config: &Config, dir: &std::path::Path, state: AppState) -> SocketAddr {
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
async fn a_full_session_splices_websocket_to_the_rdp_listener() {
    let (rdp_addr, forwarded) = fake_listener();
    let dir = temp_dir("splice");
    let config = test_config(rdp_addr.port());
    let state = state_for(&config, &dir);
    // The token carries the destination, so point it at the fake listener.
    let token = state
        .tokens()
        .issue("tester", &rdp_addr.to_string(), Instant::now());
    let addr = spawn_gateway(&config, &dir, state);

    let mut client = Client::connect(addr);
    client.send_binary(&request_pdu(
        X224_REQUEST,
        &rdp_addr.to_string(),
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
                "v1 sends no certificate chain"
            );
            assert_eq!(server_addr, rdp_addr.to_string());
        }
        other => panic!("expected a Response, got {other:?}"),
    }

    // After the handshake the socket is a raw byte pipe in both directions.
    client.send_binary(PAYLOAD);
    assert_eq!(client.recv_binary().as_deref(), Some(PAYLOAD));

    drop(client);
    assert_eq!(
        forwarded.recv_timeout(WAIT).as_deref(),
        Ok(X224_REQUEST),
        "the gateway relayed the unwrapped X.224 CR"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reused_token_gets_an_error_pdu_and_never_reaches_the_listener() {
    let (rdp_addr, forwarded) = fake_listener();
    let dir = temp_dir("replay");
    let config = test_config(rdp_addr.port());
    let state = state_for(&config, &dir);
    let token = state
        .tokens()
        .issue("tester", &rdp_addr.to_string(), Instant::now());
    // Burn the token before the browser ever shows up.
    assert!(
        state
            .tokens()
            .consume(&token.secret, Instant::now())
            .is_some()
    );
    let addr = spawn_gateway(&config, &dir, state);

    let mut client = Client::connect(addr);
    client.send_binary(&request_pdu(
        X224_REQUEST,
        &rdp_addr.to_string(),
        &token.secret,
        None,
    ));

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
        forwarded.recv_timeout(Duration::from_millis(500)).is_err(),
        "a stale token must not reach the listener"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unreachable_listener_reports_target_unreachable() {
    // Bind and immediately drop: connections to the port are refused.
    let dir = temp_dir("unreachable");
    let spare = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let dead_addr = spare.local_addr().unwrap();
    drop(spare);

    let config = test_config(dead_addr.port());
    let state = state_for(&config, &dir);
    let token = state
        .tokens()
        .issue("tester", &dead_addr.to_string(), Instant::now());
    let addr = spawn_gateway(&config, &dir, state);

    let mut client = Client::connect(addr);
    client.send_binary(&request_pdu(
        X224_REQUEST,
        &dead_addr.to_string(),
        &token.secret,
        None,
    ));

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
