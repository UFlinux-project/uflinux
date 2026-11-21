//! RDCleanPath framing: buffering, request parsing and reply construction.
//!
//! Everything here is pure so the protocol handling can be unit-tested without
//! a socket.

use ironrdp_rdcleanpath::{
    DetectionResult, EXPIRED_ERROR_CODE, GENERAL_ERROR_CODE, IMPLEMENTATION_SPECIFIC_ERROR_CODES,
    RDCleanPathErr, RDCleanPathMessage, RDCleanPathPdu, TARGET_UNREACHABLE_ERROR_CODE,
};

/// Gateway-assigned implementation-specific error codes (16384..=32767).
pub const INVALID_TOKEN_ERROR_CODE: u16 = 16_384;
/// The gateway refused the request's `destination` (it must be the local listener).
pub const DESTINATION_NOT_ALLOWED_ERROR_CODE: u16 = 16_385;

/// The parts of a client request the gateway needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    /// `destination` as sent by the client (`host:port`).
    pub destination: String,
    /// `proxy_auth`: the one-time token.
    pub proxy_auth: String,
    /// Optional Pre-Connection Blob.
    pub preconnection_blob: Option<String>,
    /// Raw, unwrapped TPKT + X.224 Connection Request.
    pub x224_connection_request: Vec<u8>,
}

/// Incremental decoder for RDCleanPath PDUs arriving over a byte stream.
///
/// `RDCleanPathPdu::detect` needs at least 9 bytes before it can tell anything
/// about the frame, so callers keep feeding bytes until [`FrameBuffer::next`]
/// yields a PDU.
#[derive(Debug, Default)]
pub struct FrameBuffer {
    buf: Vec<u8>,
    limit: usize,
}

impl FrameBuffer {
    /// Buffer with a `max` byte cap; exceeding it is a protocol error.
    pub fn new(max: usize) -> Self {
        Self {
            buf: Vec::new(),
            limit: max,
        }
    }

    /// Bytes currently buffered.
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    /// Whether nothing is buffered yet.
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// Append bytes received from the peer.
    pub fn push(&mut self, bytes: &[u8]) -> Result<(), FrameError> {
        if self.buf.len() + bytes.len() > self.limit {
            return Err(FrameError::TooLarge {
                limit: self.limit,
                have: self.buf.len() + bytes.len(),
            });
        }
        self.buf.extend_from_slice(bytes);
        Ok(())
    }

    /// Try to take one complete PDU out of the buffer.
    ///
    /// `Ok(None)` means "needs more bytes"; the caller must keep buffering.
    pub fn take_pdu(&mut self) -> Result<Option<RDCleanPathPdu>, FrameError> {
        match RDCleanPathPdu::detect(&self.buf) {
            DetectionResult::NotEnoughBytes => Ok(None),
            DetectionResult::Failed => Err(FrameError::NotRdcleanpath),
            DetectionResult::Detected { total_length, .. } => {
                if self.buf.len() < total_length {
                    return Ok(None);
                }
                let bytes = self.buf.drain(..total_length).collect::<Vec<_>>();
                let pdu =
                    RDCleanPathPdu::from_der(&bytes).map_err(|e| FrameError::Der(e.to_string()))?;
                Ok(Some(pdu))
            }
        }
    }

    /// Decode the buffered bytes as exactly one PDU (the common case: the first
    /// WebSocket message holds the whole request).
    pub fn take_request(&mut self) -> Result<Option<Request>, FrameError> {
        let Some(pdu) = self.take_pdu()? else {
            return Ok(None);
        };
        Ok(Some(request_from_pdu(&pdu)?))
    }
}

/// Why a frame could not be handled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameError {
    /// Not a RDCleanPath PDU (bad DER, unsupported version).
    NotRdcleanpath,
    /// DER decoding failed.
    Der(String),
    /// A required field was missing.
    MissingField(&'static str),
    /// The frame is bigger than the configured limit.
    TooLarge {
        /// Configured cap.
        limit: usize,
        /// Size that would result.
        have: usize,
    },
    /// The peer closed the connection before a complete frame arrived.
    Closed,
    /// The transport failed.
    Transport(String),
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotRdcleanpath => f.write_str("not a RDCleanPath PDU"),
            Self::Der(msg) => write!(f, "RDCleanPath DER error: {msg}"),
            Self::MissingField(field) => write!(f, "RDCleanPath request missing {field}"),
            Self::TooLarge { limit, have } => {
                write!(f, "RDCleanPath frame too large ({have} > {limit} bytes)")
            }
            Self::Closed => f.write_str("peer closed before a complete frame arrived"),
            Self::Transport(msg) => write!(f, "websocket error: {msg}"),
        }
    }
}

impl std::error::Error for FrameError {}

/// Turn a decoded PDU into a [`Request`].
pub fn request_from_pdu(pdu: &RDCleanPathPdu) -> Result<Request, FrameError> {
    match pdu
        .clone()
        .into_message()
        .map_err(|e| FrameError::MissingField(field_of(&e)))?
    {
        RDCleanPathMessage::Request {
            destination,
            proxy_auth,
            preconnection_blob,
            x224_connection_request,
            ..
        } => Ok(Request {
            destination,
            proxy_auth,
            preconnection_blob,
            x224_connection_request: x224_connection_request.as_bytes().to_vec(),
        }),
        RDCleanPathMessage::VmConnectRequest { .. } => Err(FrameError::MissingField(
            "x224_connection_pdu (VMConnect not supported)",
        )),
        _ => Err(FrameError::MissingField("request fields")),
    }
}

/// `MissingRDCleanPathField` carries a `&'static str` description; keep the
/// error type small by storing only its first word.
fn field_of(err: &ironrdp_rdcleanpath::MissingRDCleanPathField) -> &'static str {
    // The crate's Display is "RDCleanPath is missing <field> field"; the field
    // name itself is not exposed, so report the generic marker.
    let _ = err;
    "request fields"
}

/// Response PDU for a successful handshake.
///
/// `cert_chain` may be empty: in RDCleanPath v1 the browser does not run TLS on
/// the inner channel, it trusts the gateway's TLS connection instead.
pub fn response_pdu(
    server_addr: &str,
    x224_connection_confirm: &[u8],
    cert_chain: &[Vec<u8>],
) -> Result<Vec<u8>, FrameError> {
    let pdu = RDCleanPathPdu::new_response(
        server_addr.to_string(),
        x224_connection_confirm.to_vec(),
        cert_chain.to_vec(),
    )
    .map_err(|e| FrameError::Der(e.to_string()))?;
    pdu.to_der().map_err(|e| FrameError::Der(e.to_string()))
}

/// Error PDU with an arbitrary code.
pub fn error_pdu(error_code: u16) -> Vec<u8> {
    let pdu = RDCleanPathPdu {
        error: Some(RDCleanPathErr {
            error_code,
            http_status_code: None,
            wsa_last_error: None,
            tls_alert_code: None,
        }),
        ..RDCleanPathPdu::default()
    };
    pdu.to_der().unwrap_or_else(|_| {
        RDCleanPathPdu::new_general_error()
            .to_der()
            .unwrap_or_default()
    })
}

/// The client presented an unknown, already-used or otherwise unusable token.
pub fn invalid_token_pdu() -> Vec<u8> {
    debug_assert!(IMPLEMENTATION_SPECIFIC_ERROR_CODES.contains(&INVALID_TOKEN_ERROR_CODE));
    error_pdu(INVALID_TOKEN_ERROR_CODE)
}

/// The token existed but had expired.
pub fn expired_token_pdu() -> Vec<u8> {
    error_pdu(EXPIRED_ERROR_CODE)
}

/// The gateway could not reach the local RDP listener.
pub fn target_unreachable_pdu() -> Vec<u8> {
    error_pdu(TARGET_UNREACHABLE_ERROR_CODE)
}

/// Plain general error.
pub fn general_error_pdu() -> Vec<u8> {
    RDCleanPathPdu::new_general_error()
        .to_der()
        .unwrap_or_default()
}

/// Negotiation error carrying the server's X.224 confirm.
pub fn negotiation_error_pdu(server_x224_response: &[u8]) -> Vec<u8> {
    RDCleanPathPdu::new_negotiation_error(server_x224_response.to_vec())
        .and_then(|pdu| pdu.to_der())
        .unwrap_or_else(|_| error_pdu(GENERAL_ERROR_CODE))
}

/// Read one TPKT frame (RDP always uses the 4-byte TPKT header).
///
/// Returns the frame length, or `None` when fewer than 4 bytes are available.
pub fn tpkt_frame_len(src: &[u8]) -> Option<usize> {
    if src.len() < 4 {
        return None;
    }
    // TPKT: version(1) reserved(1) length(2, big endian, includes the header).
    let len = u16::from_be_bytes([src[2], src[3]]) as usize;
    if len < 4 {
        return Some(0); // malformed
    }
    Some(len)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 86-byte RDCleanPath request captured from a real web client.
    const SAMPLE: &str = "3054a00402020d3ea2100c0e77696e31312e6c61623a33333839a30e0c0c6a77742d6f722d746f6b656ea5130c11016d545350726f746f636f6c0153544407a6150413030000130ee000000000000108000300080000";

    fn sample() -> Vec<u8> {
        let hex: String = SAMPLE.chars().filter(|c| c.is_ascii_hexdigit()).collect();
        assert_eq!(hex.len() % 2, 0, "hex literal must be byte aligned");
        hex.as_bytes()
            .chunks(2)
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
            .collect()
    }

    #[test]
    fn detects_and_parses_a_real_request() {
        let bytes = sample();
        assert_eq!(bytes.len(), 86);
        match RDCleanPathPdu::detect(&bytes) {
            DetectionResult::Detected {
                version,
                total_length,
            } => {
                assert_eq!(version, ironrdp_rdcleanpath::VERSION_1);
                assert_eq!(total_length, 86);
            }
            other => panic!("expected a detected PDU, got {other:?}"),
        }

        let mut buf = FrameBuffer::new(64 * 1024);
        buf.push(&bytes).unwrap();
        let request = buf.take_request().unwrap().expect("one complete frame");
        assert_eq!(request.destination, "win11.lab:3389");
        assert_eq!(request.proxy_auth, "jwt-or-token");
        assert_eq!(
            request.preconnection_blob.as_deref(),
            Some("\x01mTSProtocol\x01STD\x07")
        );
        assert_eq!(
            request.x224_connection_request,
            &[
                0x03, 0x00, 0x00, 0x13, 0x0e, 0xe0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x08, 0x00,
                0x03, 0x00, 0x08, 0x00, 0x00
            ][..]
        );
        assert!(buf.is_empty(), "the whole frame was consumed");
    }

    #[test]
    fn buffering_is_required_below_nine_bytes() {
        let bytes = sample();
        for n in 1..9 {
            let mut partial = FrameBuffer::new(64 * 1024);
            partial.push(&bytes[..n]).unwrap();
            assert_eq!(
                partial.take_pdu().unwrap(),
                None,
                "{n} bytes must not be reported as a frame"
            );
        }
        // Feeding it in one byte at a time also works.
        let mut trickle = FrameBuffer::new(64 * 1024);
        let mut frames = 0;
        for chunk in bytes.chunks(1) {
            trickle.push(chunk).unwrap();
            if trickle.take_pdu().unwrap().is_some() {
                frames += 1;
            }
        }
        assert_eq!(
            frames, 1,
            "exactly one frame, emitted once the last byte lands"
        );
        assert!(trickle.is_empty());
    }

    #[test]
    fn split_frame_is_reassembled() {
        let bytes = sample();
        let mut buf = FrameBuffer::new(64 * 1024);
        buf.push(&bytes[..30]).unwrap();
        assert_eq!(buf.take_pdu().unwrap(), None);
        buf.push(&bytes[30..]).unwrap();
        let pdu = buf.take_pdu().unwrap().expect("reassembled");
        let request = request_from_pdu(&pdu).unwrap();
        assert_eq!(request.destination, "win11.lab:3389");
    }

    #[test]
    fn garbage_is_rejected_not_buffered_forever() {
        // A wrong RDCleanPath version is decisive: no more buffering.
        let mut buf = FrameBuffer::new(64 * 1024);
        buf.push(&[0x30, 0x07, 0xa0, 0x03, 0x02, 0x01, 0x02])
            .unwrap(); // SEQUENCE { version [0] INTEGER 2 }
        assert_eq!(buf.take_pdu().unwrap_err(), FrameError::NotRdcleanpath);

        // An HTTP request line is not decisive on its own, but the buffer is
        // capped, so garbage cannot grow without bound.
        let mut capped = FrameBuffer::new(16);
        capped.push(b"GET / HTTP/1.1\r\n").unwrap();
        assert_eq!(capped.take_pdu().unwrap(), None);
        assert!(matches!(
            capped.push(b"GET / HTTP/1.1\r\n"),
            Err(FrameError::TooLarge { .. })
        ));
    }

    #[test]
    fn oversized_frame_is_rejected() {
        let mut buf = FrameBuffer::new(8);
        assert!(matches!(
            buf.push(&[0u8; 16]),
            Err(FrameError::TooLarge { limit: 8, have: 16 })
        ));
    }

    #[test]
    fn error_pdus_round_trip() {
        for (bytes, code) in [
            (invalid_token_pdu(), INVALID_TOKEN_ERROR_CODE),
            (expired_token_pdu(), EXPIRED_ERROR_CODE),
            (target_unreachable_pdu(), TARGET_UNREACHABLE_ERROR_CODE),
            (general_error_pdu(), GENERAL_ERROR_CODE),
        ] {
            let pdu = RDCleanPathPdu::from_der(&bytes).expect("decodable error PDU");
            match pdu.into_message().unwrap() {
                RDCleanPathMessage::GeneralErr(err) => assert_eq!(err.error_code, code),
                other => panic!("expected GeneralErr({code}), got {other:?}"),
            }
        }
    }

    #[test]
    fn response_pdu_carries_the_confirm_and_addr() {
        let cc = [0x03u8, 0x00, 0x00, 0x0b, 0x06, 0xd0, 0x00, 0x12, 0x34];
        let der = response_pdu("127.0.0.1:3389", &cc, &[]).unwrap();
        let pdu = RDCleanPathPdu::from_der(&der).unwrap();
        match pdu.into_message().unwrap() {
            RDCleanPathMessage::Response {
                x224_connection_response,
                server_cert_chain,
                server_addr,
            } => {
                assert_eq!(x224_connection_response.as_bytes(), &cc[..]);
                assert!(server_cert_chain.is_empty());
                assert_eq!(server_addr, "127.0.0.1:3389");
            }
            other => panic!("expected a Response, got {other:?}"),
        }
    }

    #[test]
    fn negotiation_error_keeps_the_server_confirm() {
        let cc = vec![0x03u8, 0x00, 0x00, 0x0b, 0x06, 0xd0, 0x00, 0x12, 0x34];
        let der = negotiation_error_pdu(&cc);
        let pdu = RDCleanPathPdu::from_der(&der).unwrap();
        match pdu.into_message().unwrap() {
            RDCleanPathMessage::NegotiationErr {
                x224_connection_response,
            } => assert_eq!(x224_connection_response, cc),
            other => panic!("expected NegotiationErr, got {other:?}"),
        }
    }

    #[test]
    fn tpkt_lengths_are_read_big_endian() {
        assert_eq!(tpkt_frame_len(&[0x03, 0x00, 0x00, 0x13]), Some(19));
        assert_eq!(tpkt_frame_len(&[0x03, 0x00, 0x01, 0x04]), Some(260));
        assert_eq!(tpkt_frame_len(&[0x03, 0x00]), None);
        assert_eq!(
            tpkt_frame_len(&[0x03, 0x00, 0x00, 0x02]),
            Some(0),
            "malformed"
        );
    }
}
