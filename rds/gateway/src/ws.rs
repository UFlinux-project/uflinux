//! WebSocket transport: RDCleanPath handshake, then a raw byte splice.
//!
//! The contract implemented here (verified against the browser client):
//!
//! 1. The client opens the socket and sends the RDCleanPath request as the
//!    first **binary** message. Fewer than 9 buffered bytes are not enough to
//!    size the frame, so the reader keeps buffering until
//!    `RDCleanPathPdu::detect` is decisive.
//! 2. `proxy_auth` is validated against the single-use token store.
//! 3. The gateway connects to the broker's **web backend socket** — the unix
//!    socket whose connections are served with TLS already terminated — and
//!    forwards the **unwrapped** X.224 connect request (raw TPKT, no DER
//!    wrapper). `session.target` names the session for the response and the
//!    log, not the address we dial: the browser's RDP listener is the broker's
//!    TCP port, which the gateway deliberately does not use.
//! 4. Exactly one binary message comes back: `RDCleanPathPdu::new_response`
//!    carrying the server's X.224 confirm. The certificate chain is empty for
//!    v1 — in this mode the browser does not validate the TLS of the inner
//!    channel, because the gateway terminates TLS on the outer connection and
//!    the client trusts that instead. The session host on the other end agrees:
//!    it negotiates an empty `SecurityProtocol` and never starts a TLS
//!    handshake, so the bytes spliced below are plain RDP.
//! 5. Everything after that is spliced both ways. WebSocket binary messages are
//!    a byte stream in both directions. The gateway reassembles server PDUs
//!    across reads, including both TPKT and Fast-Path packets.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::time::Instant;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, State};
use axum::response::Response;
use ironrdp_rdcleanpath::GENERAL_ERROR_CODE;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::UnixStream;

use crate::rdcleanpath::{
    FrameBuffer, FrameError, Request, error_pdu, expired_token_pdu, invalid_token_pdu,
    negotiation_error_pdu, response_pdu, target_unreachable_pdu, tpkt_frame_len,
};
use crate::state::AppState;
use crate::tokens::TokenError;

/// Maximum RDCleanPath handshake data the gateway is willing to buffer.
pub const MAX_HANDSHAKE_BYTES: usize = 64 * 1024;
/// Maximum number of whole RDP packets buffered on the way to the browser.
pub const MAX_PENDING_FRAMES: usize = 32;
/// Size of a single read from the session host.
const READ_CHUNK: usize = 16 * 1024;

/// Why a session ended in an error PDU.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Failure {
    InvalidToken,
    ExpiredToken,
    Unreachable,
    Relay,
}

/// Upgrade hook for `GET /ws`.
pub async fn upgrade(
    socket: WebSocketUpgrade,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    State(state): State<AppState>,
) -> Response {
    // `on_upgrade` is `#[must_use]`: it spawns the session and returns the
    // response that completes the handshake.
    socket.on_upgrade(move |socket| session(socket, state, peer))
}

/// One gateway session: handshake, then splice.
async fn session(mut socket: WebSocket, state: AppState, peer: SocketAddr) {
    let mut buffer = FrameBuffer::new(MAX_HANDSHAKE_BYTES);
    let request = match read_request(&mut socket, &mut buffer).await {
        Ok(request) => request,
        Err(error) => {
            tracing::debug!(%error, %peer, "bad RDCleanPath request");
            let _ = send(&mut socket, &error_pdu(GENERAL_ERROR_CODE)).await;
            let _ = socket.send(Message::Close(None)).await;
            return;
        }
    };

    match serve(&mut socket, &state, &request).await {
        Ok(()) => tracing::debug!(%peer, destination = request.destination, "session closed"),
        Err(failure) => {
            tracing::debug!(?failure, %peer, destination = request.destination, "session failed");
            let pdu = match failure {
                Failure::InvalidToken => invalid_token_pdu(),
                Failure::ExpiredToken => expired_token_pdu(),
                Failure::Unreachable => target_unreachable_pdu(),
                Failure::Relay => error_pdu(GENERAL_ERROR_CODE),
            };
            let _ = send(&mut socket, &pdu).await;
            let _ = socket.send(Message::Close(None)).await;
        }
    }
}

/// Buffer messages until a complete RDCleanPath request arrives.
async fn read_request(
    socket: &mut WebSocket,
    buffer: &mut FrameBuffer,
) -> Result<Request, FrameError> {
    loop {
        if let Some(request) = buffer.take_request()? {
            return Ok(request);
        }
        match socket.recv().await {
            Some(Ok(Message::Binary(bytes))) => buffer.push(&bytes)?,
            Some(Ok(Message::Ping(bytes))) => {
                let _ = socket.send(Message::Pong(bytes)).await;
            }
            Some(Ok(Message::Text(_) | Message::Pong(_))) => {}
            Some(Ok(Message::Close(_))) | None => {
                return Err(FrameError::Closed);
            }
            Some(Err(error)) => return Err(FrameError::Transport(error.to_string())),
        }
    }
}

/// Token check, backend connect, X.224 relay, splice.
async fn serve(socket: &mut WebSocket, state: &AppState, request: &Request) -> Result<(), Failure> {
    // The token is consumed before anything is forwarded: a second use of the
    // same token must not reach the session host.
    let session = state
        .tokens()
        .consume_detailed(&request.proxy_auth, Instant::now())
        .map_err(|error| match error {
            TokenError::Expired => Failure::ExpiredToken,
            TokenError::Unknown => Failure::InvalidToken,
        })?;
    tracing::info!(
        user = session.user,
        target = session.target,
        destination = request.destination,
        preconnection_blob = request.preconnection_blob.as_ref().map_or(0, String::len),
        "RDCleanPath token accepted"
    );

    // Every browser session arrives on the same unix socket, and the broker
    // decides the session from the connection itself, not from an address.
    let backend = uflinux_rds_common::web_socket_path();
    let mut backend = match UnixStream::connect(&backend).await {
        Ok(backend) => backend,
        Err(error) => {
            tracing::debug!(%error, socket = %backend.display(), "web backend unreachable");
            return Err(Failure::Unreachable);
        }
    };

    backend
        .write_all(&request.x224_connection_request)
        .await
        .map_err(|_| Failure::Relay)?;

    let confirm = match read_x224(&mut backend).await {
        Ok(confirm) => confirm,
        Err(error) => {
            tracing::debug!(%error, target = session.target, "no usable X.224 confirm");
            // The session host answered, but not with a confirm we can relay.
            let _ = send(socket, &negotiation_error_pdu(&[])).await;
            return Err(Failure::Relay);
        }
    };

    let response = response_pdu(&session.target, &confirm, &[]).map_err(|_| Failure::Relay)?;
    send(socket, &response).await.map_err(|_| Failure::Relay)?;

    splice(socket, backend).await
}

/// Read one TPKT-framed X.224 confirm.
async fn read_x224<B: AsyncRead + Unpin>(backend: &mut B) -> std::io::Result<Vec<u8>> {
    let mut frame = vec![0u8; 4];
    backend.read_exact(&mut frame).await?;
    let total = match tpkt_frame_len(&frame) {
        Some(len) if len >= 4 => len,
        _ => return Err(std::io::Error::other("backend reply is not a TPKT frame")),
    };
    frame.resize(total, 0);
    backend.read_exact(&mut frame[4..]).await?;
    Ok(frame)
}

/// Splice in both directions until either side closes.
async fn splice<B>(socket: &mut WebSocket, mut backend: B) -> Result<(), Failure>
where
    B: AsyncRead + AsyncWrite + Unpin,
{
    let mut pending = Pending::default();
    let mut chunk = vec![0u8; READ_CHUNK];
    let mut backend_open = true;
    let mut ws_open = true;

    loop {
        // Drain whatever is queued before waiting again: a burst of small reads
        // still ends up as few WebSocket messages as possible.
        if ws_open && !pending.to_backend.is_empty() {
            let n = pending.to_backend.len().min(READ_CHUNK);
            match backend.write_all(&pending.to_backend[..n]).await {
                Ok(()) => pending.to_backend.drain(..n),
                Err(error) => {
                    tracing::debug!(%error, "backend write failed");
                    return Err(Failure::Relay);
                }
            };
            continue;
        }
        if let Some(frame) = pending.to_ws.pop_front() {
            if send(socket, &frame).await.is_err() {
                return Ok(());
            }
            continue;
        }
        if !backend_open || !ws_open {
            return Ok(());
        }

        tokio::select! {
            biased;

            received = socket.recv(), if ws_open => match received {
                Some(Ok(Message::Binary(bytes))) => pending.to_backend.extend_from_slice(&bytes),
                Some(Ok(Message::Ping(bytes))) => {
                    let _ = socket.send(Message::Pong(bytes)).await;
                }
                Some(Ok(Message::Close(_))) => ws_open = false,
                Some(Ok(Message::Text(_) | Message::Pong(_))) => {}
                Some(Err(error)) => {
                    tracing::debug!(%error, "websocket read failed");
                    return Ok(());
                }
                None => ws_open = false,
            },

            read = backend.read(&mut chunk),
                if backend_open && pending.to_ws.len() < MAX_PENDING_FRAMES => {
                match read {
                    Ok(0) => backend_open = false,
                    Ok(n) => pending.ingest(&chunk[..n]).map_err(|error| {
                        tracing::debug!(%error, "invalid RDP packet from backend");
                        Failure::Relay
                    })?,
                    Err(error) => {
                        tracing::debug!(%error, "backend read failed");
                        return Err(Failure::Relay);
                    }
                }
            }
        }
    }
}

/// Pending bytes in both directions of a splice.
#[derive(Debug, Default)]
struct Pending {
    /// Browser → session host. An opaque byte stream.
    to_backend: Vec<u8>,
    /// Session host → browser, one whole RDP packet per WebSocket message.
    to_ws: VecDeque<Vec<u8>>,
    /// A packet that straddles two reads.
    partial: Vec<u8>,
}

impl Pending {
    /// Absorb a backend read, emitting only whole RDP packets.
    fn ingest(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.partial.extend_from_slice(bytes);
        while let Some(packet) =
            ironrdp_pdu::find_size(&self.partial).map_err(std::io::Error::other)?
        {
            if self.partial.len() < packet.length {
                break;
            }
            let frame = self.partial.drain(..packet.length).collect();
            self.to_ws.push_back(frame);
        }
        Ok(())
    }
}

/// Send one binary WebSocket message.
async fn send(socket: &mut WebSocket, bytes: &[u8]) -> std::io::Result<()> {
    socket
        .send(Message::Binary(bytes.to_vec().into()))
        .await
        .map_err(std::io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_backend_read_becomes_one_message_per_tpkt_frame() {
        let mut pending = Pending::default();
        let mut stream = vec![0x03u8, 0x00, 0x00, 0x08, 1, 2, 3, 4];
        stream.extend_from_slice(&[0x03, 0x00, 0x00, 0x07, 5, 6, 7]);
        pending.ingest(&stream).unwrap();
        assert_eq!(pending.to_ws.len(), 2);
        assert_eq!(pending.to_ws[0].len(), 8);
        assert_eq!(pending.to_ws[1].len(), 7);
        assert!(pending.partial.is_empty());
    }

    #[test]
    fn a_tpkt_frame_split_across_reads_stays_in_one_message() {
        let mut pending = Pending::default();
        pending.ingest(&[0x03, 0x00, 0x00, 0x0a, 1, 2]).unwrap();
        assert_eq!(pending.to_ws.len(), 0, "nothing complete yet");
        assert_eq!(pending.partial.len(), 6, "partial frame is buffered");
        pending.ingest(&[3, 4, 5, 6]).unwrap();
        assert_eq!(pending.to_ws.len(), 1);
        assert_eq!(
            pending.to_ws[0],
            vec![0x03, 0x00, 0x00, 0x0a, 1, 2, 3, 4, 5, 6]
        );
        assert!(pending.partial.is_empty());
    }

    #[test]
    fn a_trickle_feed_yields_exactly_one_message_per_frame() {
        let mut pending = Pending::default();
        let mut stream = vec![0x03u8, 0x00, 0x00, 0x07, 7, 8, 9];
        stream.extend_from_slice(&[0x03, 0x00, 0x00, 0x07, 42, 1, 2]);
        for byte in stream.chunks(1) {
            pending.ingest(byte).unwrap();
        }
        assert_eq!(pending.to_ws.len(), 2);
        assert_eq!(pending.to_ws[0].len(), 7);
        assert_eq!(pending.to_ws[1].len(), 7);
    }

    #[test]
    fn a_fast_path_update_does_not_block_following_tpkt_packets() {
        let mut pending = Pending::default();
        pending.ingest(&[0x00, 0x80]).unwrap();
        assert!(pending.to_ws.is_empty());
        pending
            .ingest(&[0x05, 1, 2, 0x03, 0, 0, 7, 3, 4, 5])
            .unwrap();
        assert_eq!(pending.to_ws[0], [0x00, 0x80, 0x05, 1, 2]);
        assert_eq!(pending.to_ws[1], [0x03, 0, 0, 7, 3, 4, 5]);
    }

    #[test]
    fn invalid_packets_are_rejected() {
        let mut pending = Pending::default();
        assert!(pending.ingest(&[0xff, 0xff, 0, 0]).is_err());
        assert!(pending.to_ws.is_empty());
    }

    #[test]
    fn inbound_bytes_accumulate_as_a_byte_stream() {
        let mut pending = Pending::default();
        for chunk in [b"abc".as_slice(), b"def", b"ghi"] {
            pending.to_backend.extend_from_slice(chunk);
        }
        assert_eq!(pending.to_backend, b"abcdefghi");
    }
}
