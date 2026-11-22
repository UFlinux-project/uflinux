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

/// State directory used for generated certificates and logs.
pub const STATE_DIR: &str = "/var/lib/uflinux-rds";
