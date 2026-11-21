//! UFLinux RDS web gateway.
//!
//! HTTPS-by-default axum application that serves the wasm web client and
//! proxies RDP over WebSocket using the Microsoft **RDCleanPath** protocol
//! (`ironrdp-rdcleanpath`).
//!
//! Layout:
//!
//! - [`config`]: command line / environment overrides
//! - [`state`]: shared application state
//! - [`auth`]: authentication backends and the user/group allow-list
//! - [`tokens`]: single-use, expiring WebSocket tokens (pure)
//! - [`http`]: routes, JSON API and security headers
//! - [`ws`]: the RDP-over-WebSocket splice
//! - [`rdcleanpath`]: RDCleanPath framing helpers (pure)
//! - [`tls`]: self-signed / custom certificates and the TLS listener
//! - [`static_assets`]: embedded HTML/CSS

pub mod auth;
pub mod config;
pub mod http;
pub mod rdcleanpath;
pub mod state;
pub mod static_assets;
pub mod tls;
pub mod tokens;
pub mod ws;

/// Gateway error type.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// I/O while reading or writing files (certificates, state dir).
    #[error("I/O error on {path}: {message}")]
    Io {
        /// Path that failed.
        path: String,
        /// Operating-system message.
        message: String,
    },
    /// The configuration is unusable for the web gateway.
    #[error("invalid configuration: {0}")]
    Config(String),
    /// A TLS certificate could not be built or loaded.
    #[error("TLS error: {0}")]
    Tls(String),
    /// The shared config is not valid.
    #[error(transparent)]
    Common(#[from] uflinux_rds_common::ConfigError),
    /// Anything else.
    #[error("{0}")]
    Other(String),
}

impl Error {
    /// I/O error against a named path.
    pub fn io(path: impl Into<String>, source: &std::io::Error) -> Self {
        Self::Io {
            path: path.into(),
            message: source.to_string(),
        }
    }

    /// Anything else, rendered from a `Display` value.
    pub fn other(source: impl std::fmt::Display) -> Self {
        Self::Other(source.to_string())
    }
}

/// Gateway result alias.
pub type Result<T> = std::result::Result<T, Error>;
