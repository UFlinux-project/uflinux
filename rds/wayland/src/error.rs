//! Errors produced by the desktop backend.

use std::path::PathBuf;

/// Everything that can go wrong while running a desktop session.
#[derive(Debug, thiserror::Error)]
pub enum BackendError {
    /// The compositor could not be started (missing binary, permission
    /// problem, ...). The message always contains the command that was
    /// attempted.
    #[error("failed to spawn compositor `{command}`: {reason}")]
    Spawn {
        /// The command that could not be started.
        command: String,
        /// Human readable reason.
        reason: String,
    },

    /// The compositor process exited unexpectedly.
    #[error("compositor `{command}` exited unexpectedly: {reason}")]
    Died {
        /// The command that died.
        command: String,
        /// Human readable reason.
        reason: String,
    },

    /// The wayland socket never showed up / could not be connected to.
    #[error("could not connect to wayland socket `{path}`: {reason}")]
    Connect {
        /// Socket path that was tried.
        path: PathBuf,
        /// Human readable reason.
        reason: String,
    },

    /// A wayland protocol / dispatch failure.
    #[error("wayland error: {0}")]
    Wayland(String),

    /// The desktop cannot be captured (no screencopy manager, unsupported
    /// buffer format, shm allocation failure, ...).
    #[error("capture error: {0}")]
    Capture(String),

    /// Input injection failed, usually because the compositor is gone.
    #[error("input error: {0}")]
    Input(String),

    /// The backend was shut down (either explicitly or because the
    /// compositor is terminating).
    #[error("desktop backend shut down")]
    ShutDown,

    /// Underlying I/O error.
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
}

impl BackendError {
    pub(crate) fn spawn(command: impl Into<String>, reason: impl Into<String>) -> Self {
        Self::Spawn {
            command: command.into(),
            reason: reason.into(),
        }
    }

    pub(crate) fn wayland(err: impl std::fmt::Display) -> Self {
        Self::Wayland(err.to_string())
    }
}
