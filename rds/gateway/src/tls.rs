//! TLS: certificate material and the axum TLS listener.
//!
//! The ring provider is used everywhere (musl friendly, no `aws-lc-rs`).

use std::io::Error as IoError;
use std::net::SocketAddr;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsAcceptor;
use uflinux_rds_common::{CertificateSource, Config};

use crate::{Error, Result};

/// File name of the self-signed certificate inside the state dir.
pub const CERT_FILE: &str = "web-cert.pem";
/// File name of the self-signed private key inside the state dir.
pub const KEY_FILE: &str = "web-key.pem";

/// A certificate plus its private key, both in DER.
#[derive(Debug)]
pub struct Identity {
    /// Leaf first, then intermediates.
    pub cert_chain: Vec<CertificateDer<'static>>,
    /// PKCS#8 private key.
    pub key: PrivateKeyDer<'static>,
}

/// Host names and addresses that a self-signed certificate must cover.
///
/// `bind` may be a wildcard, an IPv4/IPv6 literal or a host name.
pub fn self_signed_sans(bind: &str) -> Vec<String> {
    let mut sans: Vec<String> = Vec::new();
    let mut push = |name: String| {
        if !sans.contains(&name) {
            sans.push(name);
        }
    };
    let trimmed = bind.trim_matches(['[', ']']);
    if trimmed == "0.0.0.0" || trimmed == "::" || trimmed.is_empty() {
        push("localhost".to_string());
        push("127.0.0.1".to_string());
        push("::1".to_string());
    } else if trimmed.parse::<std::net::IpAddr>().is_ok() {
        push(trimmed.to_string());
        push("localhost".to_string());
    } else {
        push(trimmed.to_string());
        push("localhost".to_string());
        push("127.0.0.1".to_string());
    }
    sans
}

/// PEM-encoded self-signed material as written to the state dir.
#[derive(Debug, Clone)]
pub struct SelfSigned {
    /// Certificate chain PEM (leaf).
    pub cert_pem: String,
    /// PKCS#8 private key PEM.
    pub key_pem: String,
}

/// Build a fresh self-signed certificate for `bind`.
///
/// ECDSA P-256. rcgen's default validity is used (1975..4096), which comfortably
/// exceeds the ten years asked for; the `time` crate is not available in this
/// workspace, so the dates are not computed here.
pub fn generate_self_signed(bind: &str) -> Result<SelfSigned> {
    let params = rcgen::CertificateParams::new(self_signed_sans(bind))
        .map_err(|e| Error::Tls(format!("self-signed subject names: {e}")))?;
    let key_pair = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256)
        .map_err(|e| Error::Tls(format!("self-signed key: {e}")))?;
    let cert = params
        .self_signed(&key_pair)
        .map_err(|e| Error::Tls(format!("self-signed certificate: {e}")))?;
    Ok(SelfSigned {
        cert_pem: cert.pem(),
        key_pem: key_pair.serialize_pem(),
    })
}

/// Load `state_dir/web-cert.pem` + `web-key.pem`, generating and writing them
/// when absent or unparsable.
pub fn self_signed_material(state_dir: &Path, bind: &str) -> Result<SelfSigned> {
    let cert_path = state_dir.join(CERT_FILE);
    let key_path = state_dir.join(KEY_FILE);
    if let (Ok(cert_pem), Ok(key_pem)) = (read_to_string(&cert_path), read_to_string(&key_path)) {
        if pem_identities(&cert_pem, &key_pem, &cert_path, &key_path).is_ok() {
            return Ok(SelfSigned { cert_pem, key_pem });
        }
        tracing::warn!(
            ?cert_path,
            ?key_path,
            "existing self-signed material unusable, generating a new certificate"
        );
    }
    let fresh = generate_self_signed(bind)?;
    write_material(state_dir, &fresh)?;
    Ok(fresh)
}

/// Write self-signed material into `state_dir` (created if needed).
pub fn write_material(state_dir: &Path, material: &SelfSigned) -> Result<()> {
    std::fs::create_dir_all(state_dir)
        .map_err(|e| Error::io(state_dir.display().to_string(), &e))?;
    let cert_path = state_dir.join(CERT_FILE);
    let key_path = state_dir.join(KEY_FILE);
    write(&cert_path, material.cert_pem.as_bytes())?;
    write(&key_path, material.key_pem.as_bytes())?;
    // 0640: readable by the service's group, which owns the RDS state dir.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let _ = std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o640));
    }
    tracing::info!(
        ?cert_path,
        "using self-signed certificate (regenerate by deleting it)"
    );
    Ok(())
}

/// Resolve the configured [`CertificateSource`] into a rustls server config.
pub fn server_config(config: &Config, state_dir: &Path) -> Result<rustls::ServerConfig> {
    let identity = match &config.web.certificate {
        CertificateSource::SelfSigned => {
            let material = self_signed_material(state_dir, &config.web.bind)?;
            pem_identity(
                &material.cert_pem,
                &material.key_pem,
                &state_dir.join(CERT_FILE),
                &state_dir.join(KEY_FILE),
            )?
        }
        CertificateSource::Custom {
            certificate,
            private_key,
            chain,
        } => {
            let cert_pem = read_to_string(certificate)?;
            let key_pem = read_to_string(private_key)?;
            let chain_pem = match chain {
                Some(path) => Some(read_to_string(path)?),
                None => None,
            };
            custom_identity(
                certificate,
                private_key,
                chain.as_deref(),
                &cert_pem,
                &key_pem,
                chain_pem.as_deref(),
            )?
        }
    };

    let provider = Arc::new(rustls::crypto::ring::default_provider());
    rustls::ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13, &rustls::version::TLS12])
        .map_err(|e| Error::Tls(format!("protocol versions: {e}")))?
        .with_no_client_auth()
        .with_single_cert(identity.cert_chain, identity.key)
        .map_err(|e| Error::Tls(format!("certificate and key rejected: {e}")))
}

/// Install the ring crypto provider as the process default (idempotent).
pub fn install_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

fn custom_identity(
    cert_path: &Path,
    key_path: &Path,
    chain_path: Option<&Path>,
    cert_pem: &str,
    key_pem: &str,
    chain_pem: Option<&str>,
) -> Result<Identity> {
    let mut identity = pem_identity(cert_pem, key_pem, cert_path, key_path)?;
    if let Some(chain_pem) = chain_pem {
        let extra = certificates(chain_pem, chain_path.unwrap_or(cert_path))?;
        if extra.is_empty() {
            return Err(Error::Tls(format!(
                "no certificate found in {}",
                display(chain_path.unwrap_or(cert_path))
            )));
        }
        identity.cert_chain.extend(extra);
    }
    Ok(identity)
}

fn pem_identity(
    cert_pem: &str,
    key_pem: &str,
    cert_path: &Path,
    key_path: &Path,
) -> Result<Identity> {
    let chain = certificates(cert_pem, cert_path)?;
    if chain.is_empty() {
        return Err(Error::Tls(format!(
            "no certificate found in {}",
            display(cert_path)
        )));
    }
    let key = private_key(key_pem, key_path)?;
    Ok(Identity {
        cert_chain: chain,
        key,
    })
}

/// PEM chain -> DER, reporting `path` on failure.
pub fn certificates(pem: &str, path: &Path) -> Result<Vec<CertificateDer<'static>>> {
    let mut reader = std::io::BufReader::new(pem.as_bytes());
    let chain = rustls_pemfile::certs(&mut reader)
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|e| Error::Tls(format!("{}: {e}", display(path))))?;
    Ok(chain)
}

/// PEM private key (PKCS#8, SEC1 EC or PKCS#1 RSA) -> DER, reporting `path`.
pub fn private_key(pem: &str, path: &Path) -> Result<PrivateKeyDer<'static>> {
    let mut reader = std::io::BufReader::new(pem.as_bytes());
    let item = rustls_pemfile::read_one(&mut reader)
        .map_err(|e| Error::Tls(format!("{}: {e}", display(path))))?
        .ok_or_else(|| {
            Error::Tls(format!(
                "{}: no private key (expected a PEM PRIVATE KEY block)",
                display(path)
            ))
        })?;
    match item {
        rustls_pemfile::Item::Pkcs8Key(key) => Ok(PrivateKeyDer::Pkcs8(key)),
        rustls_pemfile::Item::Sec1Key(key) => Ok(PrivateKeyDer::Sec1(key)),
        rustls_pemfile::Item::Pkcs1Key(key) => Ok(PrivateKeyDer::Pkcs1(key)),
        _ => Err(Error::Tls(format!(
            "{}: unsupported PEM item, expected a PKCS#8, SEC1 EC or PKCS#1 key",
            display(path)
        ))),
    }
}

fn pem_identities(
    cert_pem: &str,
    key_pem: &str,
    cert_path: &Path,
    key_path: &Path,
) -> Result<Identity> {
    pem_identity(cert_pem, key_pem, cert_path, key_path)
}

fn read_to_string(path: &Path) -> Result<String> {
    std::fs::read_to_string(path).map_err(|e| Error::io(display(path), &e))
}

fn write(path: &Path, bytes: &[u8]) -> Result<()> {
    std::fs::write(path, bytes).map_err(|e| Error::io(display(path), &e))
}

fn display(path: &Path) -> String {
    path.display().to_string()
}

/// How long a client may take to finish the TLS handshake.
///
/// Without a deadline a client that opens a socket and sends nothing holds the
/// handshake slot (and, before this bound existed, the accept loop) for as
/// long as the kernel's TCP timeout. Five seconds is generous for a full TLS
/// 1.3 handshake plus client-auth round trips on a congested link, and short
/// enough that a stalled or malicious client cannot sit on a slot for minutes.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

/// How many handshakes may be in flight at once.
///
/// A handshake is CPU work (signature verification) on a shared runtime, and
/// every in-flight one is a client that has not yet become a request. Thirty
/// two is far more than a console needs — a page load opens a handful of
/// sockets — while keeping the cost of a connection flood bounded instead of
/// letting it starve the sessions already being served.
const MAX_CONCURRENT_HANDSHAKES: usize = 32;

/// A [`TcpListener`] that hands out accepted TLS connections.
///
/// Implements [`axum::serve::Listener`] so `axum::serve` can be used without
/// `axum-server`.
///
/// Handshakes run in their own tasks, bounded by [`MAX_CONCURRENT_HANDSHAKES`]
/// and bounded in time by [`HANDSHAKE_TIMEOUT`], so one stalled client cannot
/// delay anybody else's connection. Completed handshakes arrive on `done`.
pub struct TlsListener {
    listener: TcpListener,
    acceptor: TlsAcceptor,
    handshakes: Arc<tokio::sync::Semaphore>,
    done: tokio::sync::mpsc::UnboundedReceiver<(TlsStream, SocketAddr)>,
    /// Kept so the channel never closes while the listener is alive.
    completed: tokio::sync::mpsc::UnboundedSender<(TlsStream, SocketAddr)>,
    handshake_timeout: Duration,
}

/// Accepted TLS connection.
pub type TlsStream = tokio_rustls::server::TlsStream<TcpStream>;

impl axum::serve::Listener for TlsListener {
    type Io = TlsStream;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            tokio::select! {
                // Prefer delivering a finished handshake over taking another
                // connection: the queue is what backpressure is measured on.
                biased;
                finished = self.done.recv() => match finished {
                    Some(accepted) => return accepted,
                    // Only closed by dropping the sender we hold ourselves.
                    None => continue,
                },
                accepted = self.listener.accept() => match accepted {
                    Ok((stream, peer)) => self.spawn_handshake(stream, peer),
                    Err(e) => {
                        tracing::warn!("accept failed: {e}");
                        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                    }
                },
            }
        }
    }

    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        self.listener.local_addr()
    }
}

impl TlsListener {
    /// Wrap a bound listener with TLS.
    pub fn new(listener: TcpListener, config: rustls::ServerConfig) -> Self {
        let (completed, done) = tokio::sync::mpsc::unbounded_channel();
        Self {
            listener,
            acceptor: TlsAcceptor::from(Arc::new(config)),
            handshakes: Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_HANDSHAKES)),
            done,
            completed,
            handshake_timeout: HANDSHAKE_TIMEOUT,
        }
    }

    /// Start the handshake for one connection, or drop the connection if no
    /// slot is free. Dropping `stream` closes the socket: a client that cannot
    /// be served now can reconnect.
    fn spawn_handshake(&self, stream: TcpStream, peer: SocketAddr) {
        let Ok(permit) = Arc::clone(&self.handshakes).try_acquire_owned() else {
            tracing::debug!(%peer, "TLS handshake limit reached, dropping connection");
            return;
        };
        let acceptor = self.acceptor.clone();
        let completed = self.completed.clone();
        let timeout = self.handshake_timeout;
        tokio::spawn(async move {
            // The permit lives exactly as long as the handshake.
            let _permit = permit;
            match tokio::time::timeout(timeout, acceptor.accept(stream)).await {
                Ok(Ok(tls)) => {
                    let _ = completed.send((tls, peer));
                }
                Ok(Err(e)) => {
                    // Handshake failures (scans, wrong scheme, expired client
                    // probes) are routine; keep listening.
                    tracing::debug!(%peer, "TLS handshake failed: {e}");
                }
                Err(_) => tracing::debug!(%peer, "TLS handshake timed out"),
            }
        });
    }
}

/// Bind `bind:port` and wrap it in TLS when `config.web.https`.
pub enum Server {
    /// Plain HTTP.
    Plain(TcpListener),
    /// HTTPS.
    Tls(TlsListener),
}

impl Server {
    /// Bind and configure.
    pub fn bind(config: &Config, state_dir: &Path) -> Result<Self> {
        let addr = bind_addr(&config.web.bind, config.web.port)
            .map_err(|e| Error::Config(format!("web.bind {:?}: {e}", config.web.bind)))?;
        let std_listener = std::net::TcpListener::bind(addr)
            .map_err(|e| Error::io(format!("cannot listen on {addr}"), &e))?;
        std_listener
            .set_nonblocking(true)
            .map_err(|e| Error::io(format!("cannot listen on {addr}"), &e))?;
        let listener = TcpListener::from_std(std_listener)
            .map_err(|e| Error::io(format!("cannot listen on {addr}"), &e))?;
        if !config.web.https {
            return Ok(Self::Plain(listener));
        }
        let rustls_config = server_config(config, state_dir)?;
        Ok(Self::Tls(TlsListener::new(listener, rustls_config)))
    }

    /// Bound address.
    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        match self {
            Self::Plain(l) => l.local_addr(),
            Self::Tls(l) => l.listener.local_addr(),
        }
    }

    /// Convert into an `axum::serve` listener.
    pub fn into_listener(self) -> Listener {
        match self {
            Self::Plain(l) => Listener::Plain(l),
            Self::Tls(l) => Listener::Tls(l),
        }
    }
}

/// The listener `axum::serve` runs on.
pub enum Listener {
    /// Plain HTTP.
    Plain(TcpListener),
    /// HTTPS.
    Tls(TlsListener),
}

impl axum::serve::Listener for Listener {
    type Io = BoxedIo;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        match self {
            Self::Plain(l) => {
                let (io, addr) = l.accept().await;
                (BoxedIo::new(io), addr)
            }
            Self::Tls(l) => {
                let (io, addr) = l.accept().await;
                (BoxedIo::new(io), addr)
            }
        }
    }

    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        match self {
            Self::Plain(l) => l.local_addr(),
            Self::Tls(l) => l.listener.local_addr(),
        }
    }
}

/// Type-erased IO for the two listener kinds.
pub struct BoxedIo {
    inner: Pin<Box<dyn ReadWrite>>,
}

/// Both halves of a duplex stream, boxed.
trait ReadWrite: AsyncRead + AsyncWrite + Unpin + Send + 'static {}

impl<T: AsyncRead + AsyncWrite + Unpin + Send + 'static> ReadWrite for T {}

impl BoxedIo {
    fn new<S: AsyncRead + AsyncWrite + Unpin + Send + 'static>(stream: S) -> Self {
        Self {
            inner: Box::pin(stream),
        }
    }
}

impl AsyncRead for BoxedIo {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        self.inner.as_mut().poll_read(cx, buf)
    }
}

impl AsyncWrite for BoxedIo {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        self.inner.as_mut().poll_write(cx, buf)
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        self.inner.as_mut().poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        self.inner.as_mut().poll_shutdown(cx)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> std::task::Poll<std::io::Result<usize>> {
        self.inner.as_mut().poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }
}

/// Parse `bind` (host name or IP literal) into a socket address.
pub fn bind_addr(bind: &str, port: u16) -> std::io::Result<SocketAddr> {
    let host = bind.trim_matches(['[', ']']);
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        return Ok(SocketAddr::new(ip, port));
    }
    std::net::ToSocketAddrs::to_socket_addrs(&format!("{host}:{port}"))?
        .next()
        .ok_or_else(|| IoError::other(format!("cannot resolve {bind}")))
}

/// Silence a dead-code warning for the unused re-export in some feature sets.
#[allow(dead_code)]
type _Pkcs8 = PrivatePkcs8KeyDer<'static>;

#[cfg(test)]
mod tests {
    use super::*;
    use axum::serve::Listener as _;

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("uflinux-rds-gw-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn wildcard_bind_covers_localhost_and_loopback() {
        assert_eq!(
            self_signed_sans("0.0.0.0"),
            ["localhost", "127.0.0.1", "::1"].map(String::from)
        );
        assert_eq!(
            self_signed_sans("::"),
            ["localhost", "127.0.0.1", "::1"].map(String::from)
        );
    }

    #[test]
    fn ip_bind_keeps_itself_plus_localhost() {
        assert_eq!(
            self_signed_sans("10.1.2.3"),
            ["10.1.2.3", "localhost"].map(String::from)
        );
        assert_eq!(
            self_signed_sans("[::2]"),
            ["::2", "localhost"].map(String::from)
        );
    }

    #[test]
    fn dns_bind_keeps_name_plus_loopback() {
        assert_eq!(
            self_signed_sans("rds.example"),
            ["rds.example", "localhost", "127.0.0.1"].map(String::from)
        );
    }

    #[test]
    fn self_signed_cert_is_generated_then_reused() {
        let dir = temp_dir("selfsigned");
        let first = self_signed_material(&dir, "0.0.0.0").unwrap();
        assert!(first.cert_pem.contains("BEGIN CERTIFICATE"));
        assert!(first.key_pem.contains("BEGIN PRIVATE KEY"));
        assert!(dir.join(CERT_FILE).is_file());
        assert!(dir.join(KEY_FILE).is_file());

        // Second call must reuse the file on disk, not regenerate it.
        let second = self_signed_material(&dir, "0.0.0.0").unwrap();
        assert_eq!(first.cert_pem, second.cert_pem, "certificate is reused");

        // And the material really is a usable identity.
        let identity = pem_identity(
            &second.cert_pem,
            &second.key_pem,
            &dir.join(CERT_FILE),
            &dir.join(KEY_FILE),
        )
        .unwrap();
        assert_eq!(identity.cert_chain.len(), 1);

        // The SANs we asked for are in the DER: "localhost" as an ASCII string
        // and 127.0.0.1 as four raw bytes.
        let der = identity.cert_chain[0].as_ref();
        assert!(der.windows(b"localhost".len()).any(|w| w == b"localhost"));
        assert!(der.windows(4).any(|w| w == [127, 0, 0, 1]));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn corrupt_self_signed_material_is_replaced() {
        let dir = temp_dir("corrupt");
        std::fs::write(dir.join(CERT_FILE), "not pem").unwrap();
        std::fs::write(dir.join(KEY_FILE), "not pem").unwrap();
        let material = self_signed_material(&dir, "0.0.0.0").unwrap();
        assert!(material.cert_pem.contains("BEGIN CERTIFICATE"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn server_config_from_self_signed() {
        let dir = temp_dir("cfg-selfsigned");
        let mut config = Config::default();
        config.web.https = true;
        config.web.bind = "127.0.0.1".into();
        let sc = Arc::new(server_config(&config, &dir).unwrap());
        let conn = rustls::ServerConnection::new(sc.clone()).unwrap();
        assert!(conn.is_handshaking(), "a fresh server starts mid-handshake");
        assert!(sc.alpn_protocols.is_empty(), "no ALPN is advertised");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn custom_cert_errors_name_the_path() {
        let dir = temp_dir("cfg-custom");
        let missing = dir.join("nope-cert.pem");
        let mut config = Config::default();
        config.web.https = true;
        config.web.certificate = CertificateSource::Custom {
            certificate: missing.clone(),
            private_key: dir.join("nope-key.pem"),
            chain: None,
        };
        let err = server_config(&config, &dir).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("nope-cert.pem"),
            "error must name the path: {msg}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn custom_cert_chain_is_loaded() {
        let dir = temp_dir("cfg-custom-ok");
        let material = generate_self_signed("localhost").unwrap();
        let cert = dir.join("cert.pem");
        let key = dir.join("key.pem");
        let chain = dir.join("chain.pem");
        std::fs::write(&cert, &material.cert_pem).unwrap();
        std::fs::write(&key, &material.key_pem).unwrap();
        std::fs::write(&chain, &material.cert_pem).unwrap();

        let mut config = Config::default();
        config.web.https = true;
        config.web.certificate = CertificateSource::Custom {
            certificate: cert.clone(),
            private_key: key.clone(),
            chain: Some(chain.clone()),
        };
        let cert_pem = std::fs::read_to_string(&cert).unwrap();
        let key_pem = std::fs::read_to_string(&key).unwrap();
        let chain_pem = std::fs::read_to_string(&chain).unwrap();
        let identity = custom_identity(
            &cert,
            &key,
            Some(&chain),
            &cert_pem,
            &key_pem,
            Some(&chain_pem),
        )
        .unwrap();
        assert_eq!(
            identity.cert_chain.len(),
            2,
            "leaf plus the extra chain file"
        );
        assert!(server_config(&config, &dir).is_ok());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn bind_addr_accepts_literals_and_wildcards() {
        assert_eq!(
            bind_addr("0.0.0.0", 3390).unwrap(),
            SocketAddr::from(([0, 0, 0, 0], 3390))
        );
        assert_eq!(
            bind_addr("127.0.0.1", 3391).unwrap(),
            SocketAddr::from(([127, 0, 0, 1], 3391))
        );
        assert_eq!(bind_addr("[::1]", 3392).unwrap().port(), 3392);
        assert!(bind_addr("no-such-host.invalid", 3390).is_err());
    }

    #[tokio::test]
    async fn tls_listener_binds() {
        let dir = temp_dir("listener");
        let mut config = Config::default();
        config.web.https = true;
        config.web.bind = "127.0.0.1".into();
        config.web.port = 0;
        let server = Server::bind(&config, &dir).unwrap();
        let addr = server.local_addr().unwrap();
        assert_ne!(addr.port(), 0);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Verifier that accepts any certificate: the gateway uses a self-signed
    /// certificate, and these tests care about the handshake, not the chain.
    #[derive(Debug)]
    struct AnyCert;

    impl rustls::client::danger::ServerCertVerifier for AnyCert {
        fn verify_server_cert(
            &self,
            _end_entity: &CertificateDer<'_>,
            _intermediates: &[CertificateDer<'_>],
            _server_name: &rustls::pki_types::ServerName<'_>,
            _ocsp: &[u8],
            _now: rustls::pki_types::UnixTime,
        ) -> std::result::Result<rustls::client::danger::ServerCertVerified, rustls::Error>
        {
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        }

        fn verify_tls12_signature(
            &self,
            message: &[u8],
            cert: &CertificateDer<'_>,
            dss: &rustls::DigitallySignedStruct,
        ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error>
        {
            rustls::crypto::verify_tls12_signature(message, cert, dss, &signature_algorithms())
        }

        fn verify_tls13_signature(
            &self,
            message: &[u8],
            cert: &CertificateDer<'_>,
            dss: &rustls::DigitallySignedStruct,
        ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error>
        {
            rustls::crypto::verify_tls13_signature(message, cert, dss, &signature_algorithms())
        }

        fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
            signature_algorithms().supported_schemes()
        }
    }

    fn signature_algorithms() -> rustls::crypto::WebPkiSupportedAlgorithms {
        rustls::crypto::ring::default_provider().signature_verification_algorithms
    }

    fn tls_connector() -> tokio_rustls::TlsConnector {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .expect("ring supports every protocol version")
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AnyCert))
            .with_no_client_auth();
        tokio_rustls::TlsConnector::from(Arc::new(config))
    }

    /// A TLS listener on port 0, plus its address.
    async fn tls_listener(tag: &str) -> (TlsListener, SocketAddr, std::path::PathBuf) {
        let dir = temp_dir(tag);
        let mut config = Config::default();
        config.web.https = true;
        config.web.bind = "127.0.0.1".into();
        config.web.port = 0;
        let server = Server::bind(&config, &dir).unwrap();
        let addr = server.local_addr().unwrap();
        let Listener::Tls(listener) = server.into_listener() else {
            panic!("https config must produce a TLS listener");
        };
        (listener, addr, dir)
    }

    #[tokio::test]
    async fn a_stalled_client_does_not_hold_up_the_next_one() {
        let (mut listener, addr, dir) = tls_listener("listener-stalled").await;
        // Long enough that a sequential implementation could not accidentally
        // finish inside the window we measure.
        listener.handshake_timeout = Duration::from_secs(2);

        // A client that opens a socket and then says nothing at all.
        let stalled = TcpStream::connect(addr).await.unwrap();
        let stalled_peer = stalled.peer_addr().unwrap();

        let client = TcpStream::connect(addr).await.unwrap();
        let expected = client.local_addr().unwrap();
        let name = rustls::pki_types::ServerName::try_from("localhost".to_owned()).unwrap();
        let handshake = async move { tls_connector().connect(name, client).await };

        let started = std::time::Instant::now();
        let (accepted, client_side) = tokio::join!(listener.accept(), handshake);
        let elapsed = started.elapsed();

        assert!(client_side.is_ok(), "the client handshake should succeed");
        assert_eq!(accepted.1, expected, "the live client is served first");
        assert_ne!(accepted.1, stalled_peer);
        assert!(
            elapsed < Duration::from_secs(1),
            "a stalled client held the listener for {elapsed:?} \
             (handshake timeout is two seconds)"
        );

        drop(stalled);
        drop(listener);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn handshake_permits_come_back_when_a_handshake_times_out() {
        let (mut listener, addr, dir) = tls_listener("listener-permits").await;
        listener.handshake_timeout = Duration::from_millis(50);
        let permits = Arc::clone(&listener.handshakes);

        // Two clients that connect and then say nothing.
        let stalled_a = TcpStream::connect(addr).await.unwrap();
        let stalled_b = TcpStream::connect(addr).await.unwrap();

        // The accept loop never returns: no handshake completes.
        let serving = tokio::spawn(async move {
            let mut listener = listener;
            loop {
                listener.accept().await;
            }
        });

        let mut held = false;
        for _ in 0..100 {
            if permits.available_permits() < MAX_CONCURRENT_HANDSHAKES {
                held = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(
            held,
            "the two stalled clients should be holding handshake slots"
        );

        let mut freed = false;
        for _ in 0..100 {
            if permits.available_permits() == MAX_CONCURRENT_HANDSHAKES {
                freed = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(
            freed,
            "a timed-out handshake must release its permit, leaving {} slots used",
            MAX_CONCURRENT_HANDSHAKES - permits.available_permits()
        );

        serving.abort();
        drop((stalled_a, stalled_b));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn a_full_handshake_pool_refuses_without_stalling_the_listener() {
        let (mut listener, addr, dir) = tls_listener("listener-full").await;
        listener.handshake_timeout = Duration::from_secs(30);
        // Everything is taken, so the next connection is dropped rather than
        // queued behind the clients already handshaking.
        listener.handshakes = Arc::new(tokio::sync::Semaphore::new(0));

        let noisy = TcpStream::connect(addr).await.unwrap();
        let probe = async { listener.accept().await };
        tokio::select! {
            _ = probe => panic!("no handshake can complete"),
            () = tokio::time::sleep(Duration::from_millis(100)) => {}
        }
        // The socket was accepted and then closed by the dropped stream.
        let mut buf = [0u8; 1];
        use tokio::io::AsyncReadExt;
        let mut noisy = noisy;
        let read = noisy.read(&mut buf).await.unwrap();
        assert_eq!(read, 0, "the over-limit connection is closed");

        drop(listener);
        std::fs::remove_dir_all(&dir).ok();
    }
}
