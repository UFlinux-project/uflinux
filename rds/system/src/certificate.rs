//! TLS material for the RDS broker.
//!
//! Every path is injectable through [`Paths`] so the unit tests never touch
//! `/etc`; production uses [`Paths::system`].

use crate::Result;
use std::{
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

const CERT_MODE: u32 = 0o644;
const KEY_MODE: u32 = 0o600;
const DIR_MODE: u32 = 0o755;
const STATE_MODE: u32 = 0o750;
const VALID_DAYS: i64 = 825;

/// Filesystem layout used by the helper. Tests point every field at a
/// temporary directory.
#[derive(Debug, Clone)]
pub struct Paths {
    /// `/etc/uflinux-rds/tls`
    pub tls_dir: PathBuf,
    /// `/var/lib/uflinux-rds`
    pub state_dir: PathBuf,
    /// `/etc/runlevels/default`
    pub runlevel_dir: PathBuf,
    /// `/run/openrc/started`
    pub started_dir: PathBuf,
}

impl Paths {
    pub fn system() -> Self {
        Self {
            tls_dir: PathBuf::from("/etc/uflinux-rds/tls"),
            state_dir: PathBuf::from("/var/lib/uflinux-rds"),
            runlevel_dir: PathBuf::from("/etc/runlevels/default"),
            started_dir: PathBuf::from("/run/openrc/started"),
        }
    }
    fn cert(&self) -> PathBuf {
        self.tls_dir.join("cert.pem")
    }
    fn key(&self) -> PathBuf {
        self.tls_dir.join("key.pem")
    }
    fn marker(&self) -> PathBuf {
        self.tls_dir.join(".source")
    }
    fn ensure_dirs(&self) -> Result<()> {
        create_dir(&self.tls_dir, DIR_MODE)?;
        create_dir(&self.state_dir, STATE_MODE)
    }
}

/// Installs a caller-supplied PEM pair after validating it.
pub fn install_custom(paths: &Paths, certificate: &str, key: &str) -> Result<serde_json::Value> {
    let certificate_path = validate_supplied(certificate)?;
    let key_path = validate_supplied(key)?;
    let cert_pem =
        fs::read(&certificate_path).map_err(|e| format!("Certificate unreadable: {e}"))?;
    let key_pem = fs::read(&key_path).map_err(|e| format!("Key unreadable: {e}"))?;
    validate_pem(&cert_pem, &key_pem)?;
    paths.ensure_dirs()?;
    atomic(&paths.cert(), &cert_pem, CERT_MODE)?;
    atomic(&paths.key(), &key_pem, KEY_MODE)?;
    atomic(&paths.marker(), b"custom", CERT_MODE)?;
    Ok(serde_json::json!({"ok":true,"certificate":"custom"}))
}

/// Generates a fresh self-signed RSA-2048 certificate for this host.
pub fn self_signed(paths: &Paths) -> Result<serde_json::Value> {
    let host = hostname();
    let (cert_pem, key_pem) = generate(&host)?;
    paths.ensure_dirs()?;
    atomic(&paths.cert(), cert_pem.as_bytes(), CERT_MODE)?;
    atomic(&paths.key(), key_pem.as_bytes(), KEY_MODE)?;
    atomic(&paths.marker(), b"self-signed", CERT_MODE)?;
    Ok(serde_json::json!({"ok":true,"certificate":"self-signed"}))
}

/// Read-only status document.
pub fn status(paths: &Paths) -> Result<serde_json::Value> {
    let enabled = std::fs::symlink_metadata(paths.runlevel_dir.join("uflinux-rds")).is_ok();
    let running = std::fs::symlink_metadata(paths.started_dir.join("uflinux-rds")).is_ok();
    let cert = paths.cert();
    let certificate = match fs::read(&cert) {
        Ok(_) => match fs::read_to_string(paths.marker()) {
            Ok(source) if source.trim() == "self-signed" => "self-signed",
            _ => "custom",
        },
        Err(_) => "none",
    };
    let expires = fs::read(&cert)
        .ok()
        .and_then(|pem| expiry_rfc3339(&pem).ok());
    Ok(serde_json::json!({
        "enabled": enabled,
        "running": running,
        "certificate": certificate,
        "certificate_expires": expires,
    }))
}

fn create_dir(path: &Path, mode: u32) -> Result<()> {
    fs::create_dir_all(path).map_err(|e| format!("Cannot create {}: {e}", path.display()))?;
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).map_err(|e| e.to_string())
}

fn validate_supplied(raw: &str) -> Result<PathBuf> {
    let path = Path::new(raw);
    if !path.is_absolute() {
        return Err("Certificate and key paths must be absolute".into());
    }
    if !path.is_file() {
        return Err(format!("{} is not a readable file", path.display()));
    }
    Ok(path.to_path_buf())
}

/// Cheap structural validation: at least one certificate and one private key,
/// both PEM-decodable.
fn validate_pem(cert_pem: &[u8], key_pem: &[u8]) -> Result<()> {
    let mut reader = std::io::BufReader::new(cert_pem);
    let count = rustls_pemfile::certs(&mut reader)
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|e| format!("Certificate PEM is invalid: {e}"))?
        .len();
    if count == 0 {
        return Err("Certificate PEM contains no X.509 certificate".into());
    }
    let mut reader = std::io::BufReader::new(key_pem);
    match rustls_pemfile::private_key(&mut reader) {
        Ok(Some(_)) => Ok(()),
        Ok(None) => Err("Key PEM contains no private key".into()),
        Err(e) => Err(format!("Key PEM is invalid: {e}")),
    }
}

/// RSA-2048 key generation plus an rcgen self-signature. rcgen's ring backend
/// can sign RSA but cannot generate the key, so `rsa` supplies the pair and
/// rcgen only builds and signs the certificate.
fn generate(host: &str) -> Result<(String, String)> {
    let key_pem = generate_rsa_pem()?;
    let key_pair = rcgen::KeyPair::from_pem_and_sign_algo(&key_pem, &rcgen::PKCS_RSA_SHA256)
        .map_err(|e| format!("Generated key was rejected: {e}"))?;
    let mut params = rcgen::CertificateParams::new(vec![
        "localhost".to_string(),
        host.to_string(),
        "127.0.0.1".to_string(),
    ])
    .map_err(|e| e.to_string())?;
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, host.to_string());
    params.is_ca = rcgen::IsCa::NoCa;
    params.key_usages = vec![
        rcgen::KeyUsagePurpose::DigitalSignature,
        rcgen::KeyUsagePurpose::KeyEncipherment,
    ];
    params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth];
    let now = time::OffsetDateTime::now_utc();
    params.not_before = now;
    params.not_after = now + time::Duration::days(VALID_DAYS);
    let certificate = params
        .self_signed(&key_pair)
        .map_err(|e| format!("Self-signature failed: {e}"))?;
    Ok((certificate.pem(), key_pem))
}

fn generate_rsa_pem() -> Result<String> {
    use rsa::pkcs8::EncodePrivateKey;

    let mut rng = rsa::rand_core::UnwrapErr(getrandom::SysRng);
    let private = rsa::RsaPrivateKey::new(&mut rng, 2048).map_err(|e| e.to_string())?;
    let pem = private
        .to_pkcs8_pem(rsa::pkcs8::LineEnding::LF)
        .map_err(|error| error.to_string())?;
    Ok(pem.to_string())
}

fn expiry_rfc3339(cert_pem: &[u8]) -> Result<String> {
    let (_, pem) = x509_parser::pem::parse_x509_pem(cert_pem)
        .map_err(|_| "Certificate PEM is unreadable".to_string())?;
    let certificate = pem
        .parse_x509()
        .map_err(|_| "Certificate is unreadable".to_string())?;
    let stamp = certificate.validity().not_after.timestamp();
    time::OffsetDateTime::from_unix_timestamp(stamp)
        .map_err(|e| e.to_string())?
        .format(&time::format_description::well_known::Rfc3339)
        .map_err(|e| e.to_string())
}

fn hostname() -> String {
    // SAFETY: uname writes into a zeroed, fully sized utsname and only the
    // NUL-terminated nodename is read back.
    unsafe {
        let mut name: libc::utsname = std::mem::zeroed();
        if libc::uname(&mut name) != 0 {
            return "localhost".into();
        }
        let bytes =
            std::slice::from_raw_parts(name.nodename.as_ptr().cast::<u8>(), name.nodename.len());
        let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
        match std::str::from_utf8(&bytes[..end]) {
            Ok(host) if !host.is_empty() => host.to_string(),
            _ => "localhost".into(),
        }
    }
}

/// Same-directory, no-follow temporary file; readers see complete old or new data.
fn atomic(path: &Path, data: &[u8], mode: u32) -> Result<()> {
    let parent = path.parent().ok_or("Missing parent directory")?;
    fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    let temporary = parent.join(format!(".uflinux-rds-{}-{}", std::process::id(), unique()));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&temporary)
            .map_err(|e| e.to_string())?;
        file.write_all(data)
            .and_then(|_| file.sync_all())
            .map_err(|e| e.to_string())?;
        file.set_permissions(fs::Permissions::from_mode(mode))
            .map_err(|e| e.to_string())?;
        fs::rename(&temporary, path).map_err(|e| e.to_string())?;
        fs::File::open(parent)
            .and_then(|d| d.sync_all())
            .map_err(|e| e.to_string())
    })();
    let _ = fs::remove_file(temporary);
    result
}

fn unique() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// Acknowledgement used by every mutating operation.
pub fn ok() -> serde_json::Value {
    serde_json::json!({"ok":true})
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Temp(PathBuf);

    impl Temp {
        fn new(name: &str) -> Self {
            let root = std::env::temp_dir()
                .join(format!("uflinux-rds-test-{}-{name}", std::process::id()));
            let _ = fs::remove_dir_all(&root);
            fs::create_dir_all(&root).expect("temp dir");
            Self(root)
        }
        fn paths(&self) -> Paths {
            Paths {
                tls_dir: self.0.join("etc/uflinux-rds/tls"),
                state_dir: self.0.join("var/lib/uflinux-rds"),
                runlevel_dir: self.0.join("etc/runlevels/default"),
                started_dir: self.0.join("run/openrc/started"),
            }
        }
    }

    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn self_signed_writes_parseable_pem() {
        let temp = Temp::new("self-signed");
        let paths = temp.paths();
        let reply = self_signed(&paths).expect("generation");
        assert_eq!(reply["certificate"], "self-signed");
        let cert = fs::read(paths.cert()).expect("cert.pem");
        let key = fs::read(paths.key()).expect("key.pem");
        assert!(String::from_utf8_lossy(&cert).starts_with("-----BEGIN CERTIFICATE-----"));
        assert!(String::from_utf8_lossy(&key).starts_with("-----BEGIN PRIVATE KEY-----"));
        validate_pem(&cert, &key).expect("generated pair validates");
        let (_, pem) = x509_parser::pem::parse_x509_pem(&cert).expect("x509 pem");
        let certificate = pem.parse_x509().expect("x509 body");
        assert!(
            certificate
                .validity()
                .not_after
                .timestamp()
                .gt(&time::OffsetDateTime::now_utc().unix_timestamp())
        );
        assert_eq!(
            fs::read_to_string(paths.marker()).expect("marker"),
            "self-signed"
        );
        let mode = fs::metadata(paths.key())
            .expect("key metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, KEY_MODE);
    }

    #[test]
    fn status_reports_no_certificate_and_missing_service() {
        let temp = Temp::new("status-empty");
        let paths = temp.paths();
        let reply = status(&paths).expect("status");
        assert_eq!(reply["enabled"], serde_json::json!(false));
        assert_eq!(reply["running"], serde_json::json!(false));
        assert_eq!(reply["certificate"], "none");
        assert!(reply["certificate_expires"].is_null());
        let text = reply.to_string();
        for key in ["enabled", "running", "certificate", "certificate_expires"] {
            assert!(text.contains(key), "missing {key} in {text}");
        }
    }

    #[test]
    fn status_reads_markers_and_probe_paths() {
        let temp = Temp::new("status-full");
        let paths = temp.paths();
        self_signed(&paths).expect("generation");
        fs::create_dir_all(&paths.runlevel_dir).expect("runlevel dir");
        fs::create_dir_all(&paths.started_dir).expect("started dir");
        std::os::unix::fs::symlink(
            "/etc/init.d/uflinux-rds",
            paths.runlevel_dir.join("uflinux-rds"),
        )
        .expect("runlevel symlink");
        fs::File::create(paths.started_dir.join("uflinux-rds")).expect("started file");
        let reply = status(&paths).expect("status");
        assert_eq!(reply["enabled"], serde_json::json!(true));
        assert_eq!(reply["running"], serde_json::json!(true));
        assert_eq!(reply["certificate"], "self-signed");
        let expires = reply["certificate_expires"]
            .as_str()
            .expect("expiry stamp")
            .to_string();
        time::OffsetDateTime::parse(&expires, &time::format_description::well_known::Rfc3339)
            .expect("RFC 3339");
    }

    #[test]
    fn custom_install_rejects_bad_input() {
        let temp = Temp::new("custom");
        let paths = temp.paths();
        assert!(install_custom(&paths, "relative.pem", "/tmp/k.pem").is_err());
        assert!(install_custom(&paths, "/nonexistent/c.pem", "/nonexistent/k.pem").is_err());
    }
}
