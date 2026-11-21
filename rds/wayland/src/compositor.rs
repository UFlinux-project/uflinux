//! Nested wlroots compositor lifecycle: fork, exec, supervise, tear down.
//!
//! One [`Compositor`] runs per RDP session. It is spawned with
//! [`Compositor::spawn`], which returns only once the compositor's private
//! Wayland socket exists, and is killed when it is dropped.

use std::ffi::CString;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};
use std::process;
use std::time::{Duration, Instant};

use nix::errno::Errno;
use nix::fcntl::OFlag;
use nix::sys::signal::{Signal, kill};
use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
use nix::unistd::{
    ForkResult, Group, Pid, User, execvpe, fork, initgroups, pipe2, setgid, setsid, setuid, write,
};

use crate::error::BackendError;
use crate::spec::DesktopSpec;

/// How long to wait for the compositor to create its socket.
const SOCKET_TIMEOUT: Duration = Duration::from_secs(10);
/// How long the compositor gets to exit after `SIGTERM`.
const TERM_GRACE: Duration = Duration::from_secs(2);
/// Poll interval while waiting for the socket / graceful exit.
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// A running nested compositor.
///
/// Dropping it terminates the child (`SIGTERM`, then `SIGKILL` after
/// [`TERM_GRACE`]) and reaps it, so no session compositor is ever leaked —
/// including when [`DesktopBackend::spawn`](crate::DesktopBackend::spawn)
/// fails after the child was already started.
#[derive(Debug)]
pub(crate) struct Compositor {
    pid: Pid,
    command: String,
    runtime_dir: PathBuf,
    socket_path: PathBuf,
}

impl Compositor {
    /// Fork and exec the compositor described by `spec`, then wait for its
    /// socket to appear.
    pub(crate) fn spawn(spec: &DesktopSpec) -> Result<Self, BackendError> {
        spec.validate()?;
        let command = spec.effective_command().to_owned();
        let runtime_dir = spec.runtime_dir.clone();

        // A stale socket from a previous crash would look like a ready
        // compositor, so make sure the private runtime dir is clean.
        prune_stale_sockets(&runtime_dir);

        // Everything the child needs is prepared *before* the fork: after a
        // fork in a multi-threaded process only async-signal-safe work is
        // allowed, so the child must not allocate, read /etc/passwd or walk
        // the environment.
        let target = match spec.user.as_deref() {
            Some(name) => {
                Some(resolve_user(name).map_err(|e| BackendError::spawn(name.to_owned(), e))?)
            }
            None => None,
        };
        let env = child_env(&runtime_dir, target.as_ref());
        let args = [CString::new(command.as_str())
            .unwrap_or_else(|_| CString::new("labwc").expect("static"))];
        let filename = CString::new(command.as_str())
            .unwrap_or_else(|_| CString::new("labwc").expect("static"));

        // CLOEXEC error pipe: `exec` closes the child's end, so an immediate
        // EOF in the parent means the exec succeeded. Anything the child
        // writes back is an errno explaining why it could not exec.
        let (read_end, write_end) = pipe2(OFlag::O_CLOEXEC)
            .map_err(|e| BackendError::spawn(command.clone(), format!("pipe2: {e}")))?;

        let pid = match unsafe { fork() }
            .map_err(|e| BackendError::spawn(command.clone(), format!("fork: {e}")))?
        {
            ForkResult::Child => child_main(&write_end, target.as_ref(), &filename, &args, &env),
            ForkResult::Parent { child } => {
                drop(write_end);
                match read_exec_error(read_end.as_fd()) {
                    Ok(()) => {}
                    Err(reason) => {
                        // The child is either already gone or about to exit.
                        let _ = waitpid(child, Some(WaitPidFlag::WNOHANG));
                        let _ = kill(child, Signal::SIGKILL);
                        let _ = waitpid(child, None);
                        return Err(BackendError::spawn(command, reason));
                    }
                }
                child
            }
        };

        let mut compositor = Self {
            pid,
            command,
            socket_path: runtime_dir.clone(),
            runtime_dir,
        };
        match compositor.discover_socket(SOCKET_TIMEOUT) {
            Ok(()) => Ok(compositor),
            Err(e) => {
                drop(compositor);
                Err(e)
            }
        }
    }

    /// Path of the Wayland socket the compositor is (or was) listening on.
    pub(crate) fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// The command line of the compositor, for diagnostics.
    pub(crate) fn command(&self) -> &str {
        &self.command
    }

    /// The pid of the compositor child process.
    pub(crate) fn pid(&self) -> Pid {
        self.pid
    }

    /// Block until the compositor creates its socket, or the child dies.
    ///
    /// wlroots compositors ignore `WAYLAND_DISPLAY` when they create their
    /// listening socket — `wl_display_add_socket_auto` always picks the first
    /// free `wayland-N` name in `$XDG_RUNTIME_DIR` — so the socket is
    /// *discovered* by scanning the private runtime directory rather than
    /// predicted from the spec.
    fn discover_socket(&mut self, timeout: Duration) -> Result<(), BackendError> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if let Some(path) = find_socket(&self.runtime_dir) {
                self.socket_path = path;
                return Ok(());
            }
            if let Some(status) = self.try_exit_status()? {
                let reason = match status {
                    WaitStatus::Exited(_, code) => format!("exited with code {code}"),
                    WaitStatus::Signaled(_, sig, _) => format!("killed by signal {sig}"),
                    _ => "exited".to_string(),
                };
                return Err(BackendError::Died {
                    command: self.command.clone(),
                    reason: format!(
                        "compositor {reason} before creating a socket in {}",
                        self.runtime_dir.display()
                    ),
                });
            }
            std::thread::sleep(POLL_INTERVAL);
        }
        Err(BackendError::Connect {
            path: self.runtime_dir.join("wayland-N"),
            reason: format!(
                "{} did not create a socket in {} within {}s",
                self.command,
                self.runtime_dir.display(),
                timeout.as_secs()
            ),
        })
    }

    /// `Some` once the child has been reaped, `None` while it is alive.
    ///
    /// Uses `WNOHANG`, so this never blocks and never consumes the exit status
    /// of a child that is still running.
    fn try_exit_status(&self) -> Result<Option<WaitStatus>, BackendError> {
        match waitpid(self.pid, Some(WaitPidFlag::WNOHANG)) {
            Ok(status @ (WaitStatus::Exited(..) | WaitStatus::Signaled(..))) => Ok(Some(status)),
            Ok(WaitStatus::StillAlive) => Ok(None),
            // ECHILD: reaped elsewhere, or already gone. Treat as dead.
            Err(Errno::ECHILD) => Ok(Some(WaitStatus::Exited(self.pid, -1))),
            Ok(other) => {
                tracing::debug!(?other, "unexpected waitpid status");
                Ok(None)
            }
            Err(e) => Err(BackendError::Io(e.into())),
        }
    }

    /// `true` while the compositor is still running.
    pub(crate) fn is_alive(&self) -> bool {
        self.try_exit_status().is_ok_and(|s| s.is_none())
    }

    /// Ask the compositor to exit, escalating to `SIGKILL`, and reap it.
    fn terminate(&self) {
        if kill(self.pid, Signal::SIGTERM).is_err() {
            // Already gone.
            let _ = waitpid(self.pid, Some(WaitPidFlag::WNOHANG));
            return;
        }
        let deadline = Instant::now() + TERM_GRACE;
        while Instant::now() < deadline {
            if self.try_exit_status().is_ok_and(|s| s.is_some()) {
                return;
            }
            std::thread::sleep(POLL_INTERVAL);
        }
        tracing::warn!(pid = self.pid.as_raw(), command = %self.command, "compositor ignored SIGTERM, sending SIGKILL");
        let _ = kill(self.pid, Signal::SIGKILL);
        // Reap so we do not leave a zombie behind.
        let _ = waitpid(self.pid, None);
    }
}

impl Drop for Compositor {
    fn drop(&mut self) {
        self.terminate();
    }
}

/// Look up `name` in the system password database.
fn resolve_user(name: &str) -> Result<TargetUser, String> {
    let user = User::from_name(name)
        .map_err(|e| format!("lookup of user {name}: {e}"))?
        .ok_or_else(|| format!("unknown user {name}"))?;
    // Sanity-check the primary group so a broken passwd entry fails in the
    // parent, where the error is reportable, rather than in the child.
    let _group = Group::from_gid(user.gid)
        .map_err(|e| format!("lookup of group {} for {name}: {e}", user.gid.as_raw()))?
        .ok_or_else(|| format!("user {name} has no primary group {}", user.gid.as_raw()))?;
    Ok(TargetUser {
        uid: user.uid,
        gid: user.gid,
        name: CString::new(name).unwrap_or_else(|_| CString::new("nobody").expect("static")),
        home: user.dir,
    })
}

/// The user the child is switched to, resolved before the fork.
struct TargetUser {
    uid: nix::unistd::Uid,
    gid: nix::unistd::Gid,
    name: CString,
    home: PathBuf,
}

/// `true` for the socket names a wlroots compositor may choose via
/// `wl_display_add_socket_auto`, i.e. `wayland`, `wayland-0`, `wayland-71`.
fn is_socket_name(name: &std::ffi::OsStr) -> bool {
    let name = name.to_string_lossy();
    name == "wayland"
        || (name.starts_with("wayland-")
            && name["wayland-".len()..].chars().all(|c| c.is_ascii_digit()))
}

/// The socket the compositor has published in its private runtime dir, if any.
fn find_socket(runtime_dir: &Path) -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = std::fs::read_dir(runtime_dir)
        .ok()?
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_type()
                .is_ok_and(|file_type| file_type.is_socket() && is_socket_name(&entry.file_name()))
        })
        .map(|entry| entry.path())
        .collect();
    // Deterministic when a compositor ever publishes more than one.
    candidates.sort();
    candidates.into_iter().next()
}

/// Remove sockets left behind in a reused runtime dir by a crashed session;
/// they would otherwise look like a ready compositor.
fn prune_stale_sockets(runtime_dir: &Path) {
    let Ok(entries) = std::fs::read_dir(runtime_dir) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let name = entry.file_name();
        let stale_socket = entry
            .file_type()
            .is_ok_and(|file_type| file_type.is_socket() && is_socket_name(&name));
        // wlroots also drops a `<socket>.lock` next to the socket.
        let stale_lock = name.to_string_lossy().starts_with("wayland-")
            && name.to_string_lossy().ends_with(".lock");
        if stale_socket || stale_lock {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Build the compositor's environment: ours, plus the headless backend and
/// the private runtime directory.
///
/// `WAYLAND_DISPLAY` is deliberately *removed*: the compositor creates its
/// socket via `wl_display_add_socket_auto`, which picks its own `wayland-N`
/// name inside `XDG_RUNTIME_DIR`, and a leftover value would point clients at
/// the parent session's display.
fn child_env(runtime_dir: &Path, target: Option<&TargetUser>) -> Vec<CString> {
    let mut env: Vec<CString> = std::env::vars_os()
        .filter_map(|(key, value)| {
            let key = key.to_string_lossy();
            let value = value.to_string_lossy();
            if key.contains('\0') || value.contains('\0') {
                return None;
            }
            CString::new(format!("{key}={value}")).ok()
        })
        .collect();

    let overrides = [
        ("WLR_BACKENDS", "headless".to_string()),
        ("WLR_LIBINPUT_NO_DEVICES", "1".to_string()),
        ("XDG_RUNTIME_DIR", runtime_dir.display().to_string()),
        // A nested compositor must not inherit a parent display.
        ("WAYLAND_DISPLAY", String::new()),
        ("DISPLAY", String::new()),
        ("WAYLAND_SOCKET", String::new()),
    ];
    let mut overrides: Vec<(String, String)> = overrides
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
    if let Some(target) = target {
        overrides.push((
            "USER".to_string(),
            target.name.to_string_lossy().into_owned(),
        ));
        overrides.push((
            "LOGNAME".to_string(),
            target.name.to_string_lossy().into_owned(),
        ));
        overrides.push(("HOME".to_string(), target.home.display().to_string()));
    }
    for (key, value) in overrides {
        env.retain(|entry| {
            entry
                .to_string_lossy()
                .split_once('=')
                .is_none_or(|(k, _)| k != key)
        });
        if let Ok(entry) = CString::new(format!("{key}={value}")) {
            env.push(entry);
        }
    }
    env
}

/// Read the child's exec error from the pipe. `Ok(())` means the pipe closed
/// without data, i.e. `exec` replaced the child before it could write.
fn read_exec_error(fd: BorrowedFd<'_>) -> Result<(), String> {
    let raw = fd.as_raw_fd();
    let mut buf = [0u8; 4];
    let received = loop {
        let n = unsafe { libc::read(raw, buf.as_mut_ptr().cast(), buf.len()) };
        if n < 0 {
            let errno = Errno::last();
            if errno == Errno::EINTR {
                continue;
            }
            return Err(format!("reading exec status: {errno}"));
        }
        break n as usize;
    };
    if received == 0 {
        return Ok(());
    }
    if received == 4 {
        let code = i32::from_ne_bytes(buf);
        let errno = Errno::from_raw(code);
        Err(format!("exec failed: {errno} ({code})"))
    } else {
        Err(format!("exec failed: unexpected {received} byte status"))
    }
}

/// The child half of the fork: detach, drop privileges, exec. Never returns.
fn child_main(
    errpipe: &OwnedFd,
    target: Option<&TargetUser>,
    filename: &CString,
    args: &[CString],
    env: &[CString],
) -> ! {
    fn fail(errpipe: &OwnedFd, errno: Errno) -> ! {
        let code = errno as i32;
        let _ = write(errpipe, &code.to_ne_bytes());
        process::exit(1);
    }

    // New session so the compositor is not killed with the RDP server's
    // controlling terminal, and so it owns its own process group.
    if let Err(e) = setsid() {
        fail(errpipe, e);
    }
    if let Some(target) = target {
        // initgroups before setgid, or the supplementary group list is
        // computed with insufficient privileges.
        if let Err(e) = initgroups(&target.name, target.gid) {
            fail(errpipe, e);
        }
        if let Err(e) = setgid(target.gid) {
            fail(errpipe, e);
        }
        if let Err(e) = setuid(target.uid) {
            fail(errpipe, e);
        }
    }
    // The runtime dir is owned by the session user; a nested compositor is
    // happier with it as its working directory than with whatever the RDP
    // server happened to start in.
    match execvpe(filename, args, env) {
        Ok(infallible) => match infallible {},
        Err(e) => fail(errpipe, e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::ffi::OsStr;
    use std::os::unix::net::UnixListener;

    fn spec_in(dir: &Path) -> DesktopSpec {
        DesktopSpec::new(
            "definitely-not-a-real-binary-xyz",
            800,
            600,
            dir.to_path_buf(),
        )
    }

    #[test]
    fn missing_binary_reports_spawn_error() {
        let dir = std::env::temp_dir().join(format!("uflinux-rds-spawn-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("runtime dir");
        let err = Compositor::spawn(&spec_in(&dir)).expect_err("binary does not exist");
        assert!(
            matches!(err, BackendError::Spawn { .. }),
            "unexpected error: {err}"
        );
        let message = err.to_string();
        assert!(
            message.contains("definitely-not-a-real-binary-xyz"),
            "error should name the command: {message}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unknown_user_is_reported_before_fork() {
        let dir = std::env::temp_dir().join(format!("uflinux-rds-user-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("runtime dir");
        let mut spec = spec_in(&dir);
        spec.user = Some("uflinux-no-such-user-xyz".to_string());
        let err = Compositor::spawn(&spec).expect_err("user does not exist");
        assert!(
            matches!(err, BackendError::Spawn { .. }),
            "unexpected error: {err}"
        );
        assert!(err.to_string().contains("uflinux-no-such-user-xyz"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn env_overrides_replace_inherited_values() {
        let env = child_env(Path::new("/run/user/1000"), None);
        let text: Vec<String> = env
            .iter()
            .map(|e| e.to_string_lossy().into_owned())
            .collect();
        assert!(text.iter().any(|e| e == "WLR_BACKENDS=headless"));
        assert!(text.iter().any(|e| e == "XDG_RUNTIME_DIR=/run/user/1000"));
        // The compositor picks its own socket name; a stale inherited one
        // would point clients at the parent session's display.
        assert!(text.iter().any(|e| e == "WAYLAND_DISPLAY="));
        assert_eq!(
            text.iter()
                .filter(|e| e.starts_with("WAYLAND_DISPLAY="))
                .count(),
            1,
            "overrides must not duplicate an inherited variable: {text:?}"
        );
    }

    #[test]
    fn socket_names_must_look_like_wlroots_auto_names() {
        assert!(is_socket_name(OsStr::new("wayland")));
        assert!(is_socket_name(OsStr::new("wayland-0")));
        assert!(is_socket_name(OsStr::new("wayland-42")));
        assert!(!is_socket_name(OsStr::new("wayland-uflinux-1-0")));
        assert!(!is_socket_name(OsStr::new("wayland-0.lock")));
        assert!(!is_socket_name(OsStr::new("X11")));
    }

    #[test]
    fn find_socket_ignores_non_sockets_and_foreign_names() {
        let dir = std::env::temp_dir().join(format!("uflinux-rds-find-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("runtime dir");
        std::fs::write(dir.join("wayland-0.lock"), b"").expect("lock file");
        std::fs::write(dir.join("not-a-socket"), b"").expect("plain file");
        assert!(find_socket(&dir).is_none());

        // A bound unix socket is exactly what a running compositor leaves
        // behind, so bind one under a wlroots-style name.
        let _listener = UnixListener::bind(dir.join("wayland-3")).expect("bind");
        assert_eq!(find_socket(&dir), Some(dir.join("wayland-3")));

        // A foreign socket name must not be mistaken for our display.
        let _foreign = UnixListener::bind(dir.join("pipewire-0")).expect("bind");
        assert_eq!(find_socket(&dir), Some(dir.join("wayland-3")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stale_sockets_are_pruned() {
        let dir = std::env::temp_dir().join(format!("uflinux-rds-prune-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("runtime dir");
        let _listener = UnixListener::bind(dir.join("wayland-1")).expect("bind");
        std::fs::write(dir.join("wayland-1.lock"), b"").expect("lock file");
        std::fs::write(dir.join("keep-me"), b"").expect("unrelated file");

        prune_stale_sockets(&dir);

        assert!(!dir.join("wayland-1").exists());
        assert!(!dir.join("wayland-1.lock").exists());
        assert!(dir.join("keep-me").exists());
        // Pruning an absent directory is not an error.
        prune_stale_sockets(Path::new("/definitely/not/a/runtime/dir"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn spawning_true_creates_no_socket_but_survives_drop() {
        // `true` exits immediately: spawn must report a clean error rather
        // than hang, and drop must not panic on an already-reaped child.
        let dir = std::env::temp_dir().join(format!("uflinux-rds-true-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("runtime dir");
        let mut spec = spec_in(&dir);
        spec.command = "/bin/true".to_string();
        let err = Compositor::spawn(&spec).expect_err("/bin/true creates no socket");
        assert!(
            matches!(err, BackendError::Died { .. }),
            "unexpected error: {err}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
