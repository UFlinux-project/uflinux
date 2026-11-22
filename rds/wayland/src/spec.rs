//! Description of the desktop a [`DesktopBackend`](crate::DesktopBackend) should run.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use crate::error::BackendError;

/// Counter used to give concurrent sessions distinct wayland socket names.
static SOCKET_SEQ: AtomicU32 = AtomicU32::new(0);

/// Everything the backend needs to know in order to start one desktop.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DesktopSpec {
    /// Session program to execute, e.g. `uflinux-session` or `labwc`.
    ///
    /// A bare name is looked up in `$PATH`; a value containing `/` is used as
    /// a path. `None` in [`Config::session_command`] maps to the default
    /// compositor, see [`DesktopSpec::default_command`].
    ///
    /// [`Config::session_command`]: uflinux_rds_common::config::Config::session_command
    pub command: String,
    /// Initial horizontal resolution of the desktop, in pixels.
    pub width: u32,
    /// Initial vertical resolution of the desktop, in pixels.
    pub height: u32,
    /// Drop privileges to this user before executing the compositor.
    /// `None` inherits the credentials of the RDS process.
    pub user: Option<String>,
    /// Per-session runtime directory. The private `WAYLAND_DISPLAY` socket is
    /// created in here, and the directory is exported as `XDG_RUNTIME_DIR`.
    pub runtime_dir: PathBuf,
}

impl DesktopSpec {
    /// Session launcher used when the session command is not configured.
    ///
    /// This is the distro launcher rather than a bare compositor: it starts
    /// `dbus-run-session` and `uflinux-compositor`, so the shell and everything
    /// else in `/etc/xdg/labwc/autostart` get a session bus.
    pub const DEFAULT_COMMAND: &'static str = "uflinux-session";

    /// Compositor fallbacks tried when [`Self::command`] cannot be executed.
    pub const FALLBACK_COMMANDS: &'static [&'static str] = &["cage"];

    /// A spec for `command` at `width`x`height`, running as the calling user
    /// below `runtime_dir`.
    pub fn new(
        command: impl Into<String>,
        width: u32,
        height: u32,
        runtime_dir: impl Into<PathBuf>,
    ) -> Self {
        Self {
            command: command.into(),
            width,
            height,
            user: None,
            runtime_dir: runtime_dir.into(),
        }
    }

    /// Run the compositor as `user` (setuid/setgid in the forked child).
    pub fn with_user(mut self, user: impl Into<String>) -> Self {
        self.user = Some(user.into());
        self
    }

    /// The compositor that will actually be executed, falling back to
    /// [`Self::DEFAULT_COMMAND`] for an empty command.
    pub fn effective_command(&self) -> &str {
        if self.command.trim().is_empty() {
            Self::DEFAULT_COMMAND
        } else {
            self.command.trim()
        }
    }

    /// A unique socket name for this session.
    ///
    /// Informational: wlroots compositors create their listening socket with
    /// `wl_display_add_socket_auto`, which picks its own `wayland-N` name
    /// inside [`Self::runtime_dir`], so the name actually used is discovered
    /// after startup rather than dictated by it.
    pub fn socket_name(&self) -> String {
        let seq = SOCKET_SEQ.fetch_add(1, Ordering::Relaxed);
        format!("wayland-uflinux-{}-{seq}", std::process::id())
    }

    /// Path of `socket_name` (or any other socket name) inside
    /// [`Self::runtime_dir`].
    pub fn socket_path(&self, socket_name: &str) -> PathBuf {
        self.runtime_dir.join(socket_name)
    }

    pub(crate) fn validate(&self) -> Result<(), BackendError> {
        if self.width == 0 || self.height == 0 {
            return Err(BackendError::spawn(
                self.effective_command().to_owned(),
                format!("invalid desktop size {}x{}", self.width, self.height),
            ));
        }
        if self.runtime_dir.as_os_str().is_empty() {
            return Err(BackendError::spawn(
                self.effective_command().to_owned(),
                "runtime_dir must not be empty",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_command_falls_back_to_default() {
        let spec = DesktopSpec::new("", 1280, 720, "/run/user/1000");
        assert_eq!(spec.effective_command(), DesktopSpec::DEFAULT_COMMAND);
        assert_eq!(DesktopSpec::DEFAULT_COMMAND, "uflinux-session");
    }

    #[test]
    fn whitespace_is_trimmed_but_not_swallowed() {
        let spec = DesktopSpec::new("  cage  ", 800, 600, "/tmp/x");
        assert_eq!(spec.effective_command(), "cage");
    }

    #[test]
    fn socket_names_are_unique() {
        let spec = DesktopSpec::new("labwc", 64, 64, "/tmp/x");
        assert_ne!(spec.socket_name(), spec.socket_name());
    }

    #[test]
    fn socket_path_is_under_runtime_dir() {
        let spec = DesktopSpec::new("labwc", 64, 64, "/run/user/1000/sess");
        let path = spec.socket_path("wayland-1");
        assert_eq!(path, PathBuf::from("/run/user/1000/sess/wayland-1"));
    }

    #[test]
    fn zero_sized_desktop_is_rejected() {
        let err = DesktopSpec::new("labwc", 0, 720, "/tmp/x")
            .validate()
            .unwrap_err();
        match err {
            BackendError::Spawn { command, reason } => {
                assert_eq!(command, "labwc");
                assert!(reason.contains("0x720"), "{reason}");
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn empty_runtime_dir_is_rejected() {
        let spec = DesktopSpec::new("labwc", 64, 64, "");
        assert!(spec.validate().is_err());
    }
}
