//! Error type for the broker.
//!
//! Every failure the broker can hit is reported to the operator in one of two
//! ways: logged, or turned into [`uflinux_rds_common::Reply::Error`] on the
//! control socket. Both paths want a human-readable single-line message, which
//! is exactly what `thiserror` produces, so there is no error hierarchy beyond
//! this — the broker never needs to branch on a specific cause.

use std::path::PathBuf;

/// Anything that can stop the broker from starting, serving or obeying a
/// control request.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// I/O against the control socket, the RDP listener, a child pipe or the
    /// config file.
    #[error("I/O error: {message}")]
    Io {
        /// What was being attempted, e.g. "bind RDP listener".
        action: &'static str,
        /// Raw OS message.
        message: String,
    },

    /// The config file could not be read, parsed or written.
    #[error("configuration error: {0}")]
    Config(#[from] uflinux_rds_common::ConfigError),

    /// An operation did not finish inside its deadline. Kept separate from
    /// [`Error::Io`] because a timeout has no OS message worth showing.
    #[error("{0} timed out")]
    Timeout(&'static str),

    /// The command line is not usable.
    #[error("invalid arguments: {0}")]
    Usage(String),

    /// Another broker already owns the control socket.
    #[error("another broker is already listening on {0}")]
    AlreadyRunning(PathBuf),
}

impl Error {
    /// Wrap an OS error that happened while doing `action`.
    pub(crate) fn io(action: &'static str, e: &std::io::Error) -> Self {
        Self::Io {
            action,
            message: e.to_string(),
        }
    }

    /// The single-line text a client should see.
    pub(crate) fn reply_message(&self) -> String {
        self.to_string()
    }
}

/// Convenience alias used throughout the crate.
pub(crate) type Result<T> = std::result::Result<T, Error>;
