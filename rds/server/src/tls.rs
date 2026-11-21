//! Per-session TLS identity.
//!
//! RDP clients (and the web gateway) do not have a reason to trust a
//! certificate pinned to a hostname they did not choose, so each session
//! generates — or reuses from the previous session of the same user — a
//! self-signed ECDSA P-256 certificate. The private key is written 0640 inside
//! the RDS state directory.
//!
//! The certificate's public key also has to be handed to IronRDP in the form
//! CredSSP compares it against, see [`Identity::spki`].
//!
//! This module is a deliberate copy of the ~60 relevant lines of
//! `rds/gateway/src/tls.rs`. Depending on the gateway crate instead would pull
//! axum and the whole web front end into a binary that is forked per client,
//! which is a worse trade than the duplication.

use std::path::Path;

use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio_rustls::TlsAcceptor;

/// File name of the self-signed certificate inside the state dir.
pub const CERT_FILE: &str = "session-cert.pem";
/// File name of the self-signed private key inside the state dir.
pub const KEY_FILE: &str = "session-key.pem";

/// Install the ring crypto provider as the process default (idempotent).
///
/// Must run before any rustls config is built. The workspace avoids `aws-lc-rs`
/// because it does not build for musl, and IronRDP's `TlsAcceptor` picks up the
/// process default rather than a provider we pass it.
pub fn install_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// Why certificate material could not be produced.
#[derive(Debug, thiserror::Error)]
pub enum TlsError {
    /// rcgen refused the subject names or the key.
    #[error("cannot create a self-signed certificate: {0}")]
    Generate(String),
    /// A file could not be read or written.
    #[error("{path}: {source}")]
    Io {
        /// The file involved.
        path: String,
        /// The OS error.
        source: std::io::Error,
    },
    /// rustls rejected the chain or key.
    #[error("TLS identity rejected: {0}")]
    Rustls(String),
}

/// A certificate plus its private key, both in DER.
#[derive(Debug)]
pub struct Identity {
    /// Leaf first, then intermediates.
    pub cert_chain: Vec<CertificateDer<'static>>,
    /// PKCS#8, SEC1 or PKCS#1 private key.
    pub key: PrivateKeyDer<'static>,
    /// DER of the certificate's public key.
    pub spki: Vec<u8>,
}

impl Identity {
    /// A rustls acceptor for this identity.
    pub fn acceptor(&self) -> Result<TlsAcceptor, TlsError> {
        let provider = std::sync::Arc::new(rustls::crypto::ring::default_provider());
        let config = rustls::ServerConfig::builder_with_provider(provider)
            .with_protocol_versions(&[&rustls::version::TLS13, &rustls::version::TLS12])
            .map_err(|e| TlsError::Rustls(format!("protocol versions: {e}")))?
            .with_no_client_auth()
            .with_single_cert(self.cert_chain.clone(), self.key.clone_key())
            .map_err(|e| TlsError::Rustls(format!("certificate and key rejected: {e}")))?;
        Ok(TlsAcceptor::from(std::sync::Arc::new(config)))
    }

    /// The bytes IronRDP's `RdpServerSecurity::Hybrid` expects.
    ///
    /// CredSSP echoes the server's public key back through the TLS channel
    /// binding, and IronRDP compares it with the value it was given at build
    /// time. That value must be the contents of the certificate's
    /// `SubjectPublicKeyInfo` BIT STRING — exactly what rcgen's
    /// `KeyPair::public_key_raw()` returns for the EC key generated below,
    /// which is why no X.509 parser is needed here.
    pub fn spki(&self) -> &[u8] {
        &self.spki
    }
}

/// Host names and addresses a self-signed session certificate must cover.
///
/// RDP clients check the binding loosely at best, but a certificate with no
/// subject alternative names at all is rejected outright by some, so a
/// wildcard bind still gets a usable set.
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

/// PEM certificate and key as written to the state dir.
#[derive(Debug, Clone)]
pub struct SelfSigned {
    /// Leaf certificate PEM.
    pub cert_pem: String,
    /// Private key PEM.
    pub key_pem: String,
    /// DER of the certificate's public key, for CredSSP.
    pub spki: Vec<u8>,
}

/// Builds a fresh self-signed certificate for `bind`.
pub fn generate_self_signed(bind: &str) -> Result<SelfSigned, TlsError> {
    let params = rcgen::CertificateParams::new(self_signed_sans(bind))
        .map_err(|e| TlsError::Generate(format!("subject names: {e}")))?;
    let key_pair = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256)
        .map_err(|e| TlsError::Generate(format!("key: {e}")))?;
    let cert = params
        .self_signed(&key_pair)
        .map_err(|e| TlsError::Generate(format!("certificate: {e}")))?;
    Ok(SelfSigned {
        cert_pem: cert.pem(),
        key_pem: key_pair.serialize_pem(),
        spki: key_pair.public_key_raw().to_vec(),
    })
}

/// Loads `state_dir/session-cert.pem` + `session-key.pem`, generating and
/// writing them when absent or unparsable.
///
/// Reusing the material matters for more than tidiness: a client that has
/// already accepted the key once (mstsc caches the fingerprint per server) is
/// shown a warning on every session otherwise.
pub fn self_signed_identity(state_dir: &Path, bind: &str) -> Result<Identity, TlsError> {
    let cert_path = state_dir.join(CERT_FILE);
    let key_path = state_dir.join(KEY_FILE);
    let material = match (read_to_string(&cert_path), read_to_string(&key_path)) {
        (Ok(cert_pem), Ok(key_pem)) if pem_is_usable(&cert_pem, &key_pem) => {
            // Re-reading the SPKI from the stored key keeps CredSSP working
            // across restarts of the same certificate.
            let spki = spki_from_pem(&key_pem)?;
            SelfSigned {
                cert_pem,
                key_pem,
                spki,
            }
        }
        _ => {
            let fresh = generate_self_signed(bind)?;
            write_material(state_dir, &fresh)?;
            fresh
        }
    };
    let cert_chain = certificates(&material.cert_pem, &cert_path)?;
    if cert_chain.is_empty() {
        return Err(TlsError::Generate(format!(
            "no certificate in {}",
            cert_path.display()
        )));
    }
    let key = private_key(&material.key_pem, &key_path)?;
    Ok(Identity {
        cert_chain,
        key,
        spki: material.spki,
    })
}

/// Writes the pair into `state_dir`, creating it first.
pub fn write_material(state_dir: &Path, material: &SelfSigned) -> Result<(), TlsError> {
    std::fs::create_dir_all(state_dir).map_err(|e| TlsError::Io {
        path: state_dir.display().to_string(),
        source: e,
    })?;
    let cert_path = state_dir.join(CERT_FILE);
    let key_path = state_dir.join(KEY_FILE);
    write(&cert_path, material.cert_pem.as_bytes())?;
    write(&key_path, material.key_pem.as_bytes())?;
    // 0640: readable by the service group that owns the RDS state dir.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let _ = std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o640));
    }
    tracing::info!(cert = ?cert_path, "using a self-signed session certificate");
    Ok(())
}

/// PEM chain -> DER.
pub fn certificates(pem: &str, path: &Path) -> Result<Vec<CertificateDer<'static>>, TlsError> {
    let mut reader = std::io::BufReader::new(pem.as_bytes());
    rustls_pemfile::certs(&mut reader)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| TlsError::Generate(format!("{}: {e}", path.display())))
}

/// PEM private key (PKCS#8, SEC1 EC or PKCS#1 RSA) -> DER.
pub fn private_key(pem: &str, path: &Path) -> Result<PrivateKeyDer<'static>, TlsError> {
    let mut reader = std::io::BufReader::new(pem.as_bytes());
    let item = rustls_pemfile::read_one(&mut reader)
        .map_err(|e| TlsError::Generate(format!("{}: {e}", path.display())))?
        .ok_or_else(|| TlsError::Generate(format!("{}: no private key block", path.display())))?;
    match item {
        rustls_pemfile::Item::Pkcs8Key(key) => Ok(PrivateKeyDer::Pkcs8(key)),
        rustls_pemfile::Item::Sec1Key(key) => Ok(PrivateKeyDer::Sec1(key)),
        rustls_pemfile::Item::Pkcs1Key(key) => Ok(PrivateKeyDer::Pkcs1(key)),
        _ => Err(TlsError::Generate(format!(
            "{}: unsupported PEM item",
            path.display()
        ))),
    }
}

/// Recovers the CredSSP public key from a stored key PEM.
///
/// rcgen can rebuild a key pair from PEM and reports the same raw public key
/// for it, so a reused certificate keeps a matching channel binding.
fn spki_from_pem(pem: &str) -> Result<Vec<u8>, TlsError> {
    let key_pair = rcgen::KeyPair::from_pem(pem)
        .map_err(|e| TlsError::Generate(format!("stored key: {e}")))?;
    Ok(key_pair.public_key_raw().to_vec())
}

fn pem_is_usable(cert_pem: &str, key_pem: &str) -> bool {
    let mut reader = std::io::BufReader::new(cert_pem.as_bytes());
    let has_cert = rustls_pemfile::certs(&mut reader).next().is_some();
    has_cert && rcgen::KeyPair::from_pem(key_pem).is_ok()
}

fn read_to_string(path: &Path) -> Result<String, TlsError> {
    std::fs::read_to_string(path).map_err(|e| TlsError::Io {
        path: path.display().to_string(),
        source: e,
    })
}

fn write(path: &Path, bytes: &[u8]) -> Result<(), TlsError> {
    std::fs::write(path, bytes).map_err(|e| TlsError::Io {
        path: path.display().to_string(),
        source: e,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_wildcard_bind_still_gets_usable_names() {
        let sans = self_signed_sans("0.0.0.0");
        assert!(sans.contains(&"localhost".to_string()));
        assert!(sans.contains(&"127.0.0.1".to_string()));
        assert_eq!(sans.len(), 3);
    }

    #[test]
    fn an_address_bind_becomes_a_subject_name() {
        let sans = self_signed_sans("10.0.0.5");
        assert_eq!(sans, vec!["10.0.0.5".to_string(), "localhost".to_string()]);
        // Brackets around an IPv6 literal are not part of the name.
        assert!(self_signed_sans("[::1]").contains(&"::1".to_string()));
    }

    #[test]
    fn a_generated_identity_has_everything_hybrid_needs() {
        let material = generate_self_signed("localhost").expect("generate");
        let chain = certificates(&material.cert_pem, Path::new("test")).expect("chain");
        assert_eq!(chain.len(), 1, "a self-signed leaf only");
        let key = private_key(&material.key_pem, Path::new("test")).expect("key");
        // rcgen's `serialize_pem()` emits an PKCS#8 `EC PRIVATE KEY`-style
        // block, so this is the variant a Hybrid (CredSSP) handshake will be
        // handed. What matters is that a usable key came out, not the label.
        assert!(matches!(key, PrivateKeyDer::Pkcs8(_)), "got {key:?}");
        // An EC P-256 public key is an uncompressed point: 0x04 + 2*32 bytes.
        assert_eq!(material.spki.len(), 65);
        assert_eq!(material.spki[0], 0x04);
    }

    #[test]
    fn material_round_trips_through_the_state_dir() {
        let dir = std::env::temp_dir().join(format!("uflinux-rds-tls-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let first = self_signed_identity(&dir, "localhost").expect("first identity");
        let bytes = std::fs::read(dir.join(CERT_FILE)).expect("cert written");
        assert!(bytes.starts_with(b"-----BEGIN CERTIFICATE-----"));

        // The second call must reuse, not regenerate: the SPKI has to match.
        let second = self_signed_identity(&dir, "localhost").expect("reused identity");
        assert_eq!(first.spki, second.spki, "certificate was regenerated");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unparsable_stored_material_is_replaced() {
        let dir = std::env::temp_dir().join(format!("uflinux-rds-tls-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        std::fs::write(dir.join(CERT_FILE), "not a certificate").expect("write junk");
        std::fs::write(dir.join(KEY_FILE), "not a key").expect("write junk");
        let identity = self_signed_identity(&dir, "localhost").expect("regenerated");
        assert_eq!(identity.cert_chain.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_acceptor_can_be_built_from_the_identity() {
        install_crypto_provider();
        let material = generate_self_signed("localhost").expect("generate");
        let identity = Identity {
            cert_chain: certificates(&material.cert_pem, Path::new("test")).expect("chain"),
            key: private_key(&material.key_pem, Path::new("test")).expect("key"),
            spki: material.spki,
        };
        identity.acceptor().expect("server config");
    }
}
