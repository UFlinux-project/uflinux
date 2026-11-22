//! Shared configuration and control-protocol types for UFLinux RDS.

mod config;
mod protocol;

pub mod openrc;

pub use config::{
    CertificateSource, CodecConfig, Config, ConfigError, DEFAULT_CONFIG_PATH,
    DEFAULT_IDLE_TIMEOUT_SECS, DEFAULT_MAX_FPS, DEFAULT_MAX_SESSIONS, DEFAULT_RDP_PORT,
    DEFAULT_SESSION_COMMAND, DEFAULT_WEB_PORT, H264Policy, WebConfig,
};
pub use protocol::{BrokerState, Reply, Request, SessionInfo, SystemAction};

/// Configuration file, honouring `UFLINUX_RDS_CONFIG` exactly like the broker's
/// own `--config` handling, so every writer patches the file the broker reads.
pub fn config_file_path() -> std::path::PathBuf {
    match std::env::var_os("UFLINUX_RDS_CONFIG") {
        Some(path) if !path.is_empty() => std::path::PathBuf::from(path),
        _ => std::path::Path::new(DEFAULT_CONFIG_PATH).to_path_buf(),
    }
}

/// Socket the broker listens on for control traffic.
///
/// The broker is a system service, so the socket lives in a directory it owns
/// rather than in the caller's `$XDG_RUNTIME_DIR`.
pub fn control_socket_path() -> std::path::PathBuf {
    match std::env::var_os("UFLINUX_RDS_CONTROL_SOCKET") {
        Some(path) if !path.is_empty() => std::path::PathBuf::from(path),
        _ => std::path::Path::new(CONTROL_SOCKET).to_path_buf(),
    }
}

pub const CONTROL_SOCKET: &str = "/run/uflinux-rds/control.sock";

/// Socket the broker listens on for web backends, i.e. the connections the
/// gateway splices into.
///
/// A second listener rather than a flag on the TCP one: a browser session is
/// already encrypted by the gateway, so the session host must not ask for a
/// TLS handshake inside it, and the transport has to be decided by where a
/// connection arrived rather than by what the client claims.
pub fn web_socket_path() -> std::path::PathBuf {
    match std::env::var_os(ENV_WEB_SOCKET) {
        Some(path) if !path.is_empty() => std::path::PathBuf::from(path),
        _ => std::path::Path::new(WEB_SOCKET).to_path_buf(),
    }
}

pub const WEB_SOCKET: &str = "/run/uflinux-rds/web.sock";

/// Override for [`WEB_SOCKET`], honoured by both the broker and the gateway.
pub const ENV_WEB_SOCKET: &str = "UFLINUX_RDS_WEB_SOCKET";

/// State directory used for generated certificates and logs.
pub const STATE_DIR: &str = "/var/lib/uflinux-rds";
