//! Hardware discovery. Cheap, non-fatal, and never implicit on a hot path.

use std::ffi::OsStr;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::tier::HardwareCaps;

/// Render node probed when the operator did not configure one.
pub const DEFAULT_VAAPI_DEVICE: &str = "/dev/dri/renderD128";

/// Name of the out-of-process encoder helper.
pub const FFMPEG_BINARY: &str = "ffmpeg";

/// Encoder the capability probe looks for in the helper's encoder list.
const ENCODER_NAME: &str = "h264_vaapi";

/// Finds a usable VA-API render node.
///
/// A device counts as usable when the path exists *and* is a character device —
/// that is what a DRM render node is, and it avoids a libva FFI round trip (and
/// its ability to block or abort) for a decision we only need once per
/// connection. `configured` is `CodecConfig::vaapi_device`; `None` probes
/// [`DEFAULT_VAAPI_DEVICE`].
///
/// Never panics and never touches the GPU.
pub fn detect_vaapi(configured: Option<&Path>) -> HardwareCaps {
    let device = configured.unwrap_or_else(|| Path::new(DEFAULT_VAAPI_DEVICE));
    if !is_char_device(device) {
        if configured.is_some() {
            tracing::debug!(?device, "configured VA-API device is not a render node");
        }
        return HardwareCaps::none();
    }
    HardwareCaps {
        vaapi: true,
        device: Some(device.to_string_lossy().into_owned()),
    }
}

/// `true` when `path` exists and is a character device (i.e. a DRM render
/// node). One `stat` call rather than a libva probe.
pub(crate) fn is_char_device(path: &Path) -> bool {
    let Ok(c_path) = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()) else {
        return false;
    };

    // SAFETY: `statbuf` is writable local storage and `c_path` is a valid
    // NUL-terminated path that outlives the call. `stat` writes through the
    // pointer and returns 0 on success, after which reading `st_mode` is
    // defined; on failure the buffer is left unread.
    unsafe {
        let mut statbuf: libc::stat = std::mem::zeroed();
        libc::stat(c_path.as_ptr(), std::ptr::addr_of_mut!(statbuf)) == 0
            && (statbuf.st_mode & libc::S_IFMT) == libc::S_IFCHR
    }
}

/// `which`-style scan of `$PATH` for the encoder helper. No subprocess is
/// spawned, so this is safe to call while deciding whether to build a backend.
pub fn ffmpeg_on_path() -> Option<PathBuf> {
    first_executable_in(
        std::env::var_os("PATH").as_deref(),
        OsStr::new(FFMPEG_BINARY),
    )
}

/// Pure half of [`ffmpeg_on_path`]: the first directory of `path` (a `PATH`
/// style list) that holds an executable `binary`.
fn first_executable_in(path: Option<&OsStr>, binary: &OsStr) -> Option<PathBuf> {
    let path = path?;
    std::env::split_paths(path)
        .map(|dir| dir.join(binary))
        .find(|candidate| is_executable(candidate))
}

/// Whether the out-of-process helper is present and therefore whether the
/// hardware tier is even worth trying.
///
/// Deliberately only checks that `ffmpeg` exists on `$PATH`: the real encoder
/// probe spawns a process, so it lives in [`probe_ffmpeg_h264_vaapi`] and is
/// never called implicitly.
pub fn ffmpeg_supports_h264_vaapi() -> bool {
    ffmpeg_on_path().is_some()
}

/// Runs `ffmpeg -hide_banner -encoders` and looks for `h264_vaapi`.
///
/// This spawns a process with a hard timeout, so call it once at startup (or
/// from a health check), not per frame. Any failure — missing binary, timeout,
/// non-zero exit, unreadable output — counts as "not supported".
pub fn probe_ffmpeg_h264_vaapi() -> bool {
    let program = ffmpeg_on_path().unwrap_or_else(|| PathBuf::from(FFMPEG_BINARY));
    probe_ffmpeg_h264_vaapi_with(&program, Duration::from_millis(2_000))
}

/// Injectable form of [`probe_ffmpeg_h264_vaapi`]: the helper is replaced by
/// `program` and `timeout` bounds the wait.
pub fn probe_ffmpeg_h264_vaapi_with(program: &Path, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    let Some(mut child) = spawn_helper(program, deadline) else {
        tracing::debug!("cannot run encoder helper for the capability probe");
        return false;
    };

    // Drain stdout on a helper thread: ffmpeg's encoder list is bigger than a
    // pipe buffer, so reading it inline can deadlock against a helper that is
    // still writing, and reading it only after `try_wait` would defeat the
    // timeout.
    let receiver = child.stdout.take().map(|stdout| {
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let mut output = String::new();
            let mut stdout = stdout;
            let _ = stdout.read_to_string(&mut output);
            let _ = sender.send(output);
        });
        receiver
    });

    let output = match &receiver {
        Some(receiver) => recv_until(receiver, deadline).unwrap_or_default(),
        None => String::new(),
    };

    if !reap(child, deadline) {
        tracing::debug!("encoder helper capability probe failed or timed out");
        return false;
    }

    // ffmpeg prints e.g. ` V....D h264_vaapi           H.264/AVC (VAAPI)
    // (codec h264)`, so the encoder name is one whitespace-separated field.
    output
        .lines()
        .any(|line| line.split_whitespace().any(|field| field == ENCODER_NAME))
}

/// Spawns the helper, retrying while the deadline allows.
///
/// `execve` fails with `ETXTBSY` while the binary still has pages being written
/// back, which is exactly what happens when the package manager replaces ffmpeg
/// underneath a running daemon. Retrying briefly turns that into a slow probe
/// instead of a false "no hardware encode".
fn spawn_helper(program: &Path, deadline: Instant) -> Option<std::process::Child> {
    loop {
        match std::process::Command::new(program)
            .args(["-hide_banner", "-encoders"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            Ok(child) => return Some(child),
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock
                    || error.raw_os_error() == Some(libc::ETXTBSY) =>
            {
                if Instant::now() >= deadline {
                    return None;
                }
                std::thread::sleep(Duration::from_millis(2));
            }
            Err(error) => {
                tracing::debug!(%error, "cannot run encoder helper for the capability probe");
                return None;
            }
        }
    }
}

/// `recv_deadline` is still unstable, so poll with short timeouts instead.
fn recv_until(receiver: &mpsc::Receiver<String>, deadline: Instant) -> Option<String> {
    loop {
        let now = Instant::now();
        if now >= deadline {
            return None;
        }
        let slice = (deadline - now).min(Duration::from_millis(20));
        match receiver.recv_timeout(slice) {
            Ok(output) => return Some(output),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return None,
        }
    }
}

/// Reaps `child`, treating a process that outlives `deadline` as a failure.
fn reap(child: std::process::Child, deadline: Instant) -> bool {
    let mut child = child;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return false;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(error) => {
                tracing::debug!(%error, "encoder helper capability probe failed");
                return false;
            }
        }
    }
}

/// `access(path, X_OK)` on a path that exists.
fn is_executable(path: &Path) -> bool {
    let Ok(c_path) = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()) else {
        return false;
    };
    // SAFETY: `c_path` is a valid NUL-terminated path that outlives the call;
    // `access` only reads the string and the filesystem.
    unsafe { libc::access(c_path.as_ptr(), libc::X_OK) == 0 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// A path inside the project directory (never `/tmp`), removed on drop.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let dir = scratch_dir();
            Self(dir.join(name))
        }

        fn with_contents(&self, bytes: &[u8], mode: u32) {
            std::fs::write(&self.0, bytes).unwrap();
            std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(mode)).unwrap();
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    fn scratch_dir() -> PathBuf {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("test-tmp");
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_regular_file_is_not_a_render_node() {
        let scratch = Scratch::new("regular-file");
        scratch.with_contents(b"not a dri node", 0o644);
        assert!(!is_char_device(&scratch.0));
        assert_eq!(detect_vaapi(Some(&scratch.0)), HardwareCaps::none());
    }

    #[test]
    fn a_missing_path_is_not_a_render_node() {
        let scratch = Scratch::new("does-not-exist");
        assert!(!is_char_device(&scratch.0));
        assert_eq!(detect_vaapi(Some(&scratch.0)), HardwareCaps::none());
    }

    #[test]
    fn a_character_device_is_accepted() {
        // /dev/null is a char device on every Linux box, which exercises the
        // positive branch without a GPU.
        assert!(is_char_device(Path::new("/dev/null")));
        let caps = detect_vaapi(Some(Path::new("/dev/null")));
        assert!(caps.vaapi);
        assert_eq!(caps.device.as_deref(), Some("/dev/null"));
    }

    #[test]
    fn a_directory_is_not_a_render_node() {
        assert!(!is_char_device(Path::new("/dev")));
        assert_eq!(detect_vaapi(Some(Path::new("/dev"))), HardwareCaps::none());
    }

    #[test]
    fn default_probe_never_panics_and_stays_self_consistent() {
        let caps = detect_vaapi(None);
        assert_eq!(caps.vaapi, caps.device.is_some());
        if caps.vaapi {
            assert_eq!(caps.device.as_deref(), Some(DEFAULT_VAAPI_DEVICE));
        }
    }

    #[test]
    fn path_scan_skips_non_executables_and_missing_directories() {
        let dir = scratch_dir();
        let decoy = Scratch::new("ffmpeg");
        decoy.with_contents(b"not executable", 0o644);

        let bogus = dir.join("no-such-dir");
        assert_eq!(
            first_executable_in(Some(bogus.as_os_str()), OsStr::new("ffmpeg")),
            None,
            "missing directory matched"
        );
        assert_eq!(
            first_executable_in(Some(dir.as_os_str()), OsStr::new("ffmpeg")),
            None,
            "non-executable file matched"
        );

        decoy.with_contents(b"#!/bin/sh\n", 0o755);
        assert_eq!(
            first_executable_in(Some(dir.as_os_str()), OsStr::new("ffmpeg")),
            Some(decoy.0.clone()),
            "executable file was not found"
        );
    }

    #[test]
    fn path_scan_without_a_path_variable_finds_nothing() {
        assert_eq!(
            first_executable_in(None, OsStr::new("ffmpeg")),
            None,
            "empty PATH matched"
        );
    }

    #[test]
    fn capability_probe_with_a_missing_program_is_false() {
        assert!(
            !probe_ffmpeg_h264_vaapi_with(
                Path::new("/definitely/not/ffmpeg"),
                Duration::from_millis(50)
            ),
            "missing helper reported as supported"
        );
    }

    #[test]
    fn capability_probe_reads_the_encoder_list_from_stdout() {
        let fake = Scratch::new("fake-ffmpeg");
        fake.with_contents(
            b"#!/bin/sh\nprintf ' V. .A .... h264_vaapi           H.264/AVC (h264_vaapi)\\n'\n",
            0o755,
        );
        assert!(
            probe_ffmpeg_h264_vaapi_with(&fake.0, Duration::from_millis(5_000)),
            "fake helper listing h264_vaapi was rejected"
        );
    }

    #[test]
    fn capability_probe_rejects_a_helper_without_vaapi_support() {
        let fake = Scratch::new("fake-ffmpeg-no-vaapi");
        fake.with_contents(
            b"#!/bin/sh\nprintf ' V. .A .... libx264                H.264 / AVC (libx264)\\n'\n",
            0o755,
        );
        assert!(
            !probe_ffmpeg_h264_vaapi_with(&fake.0, Duration::from_millis(5_000)),
            "helper without h264_vaapi reported as supported"
        );
    }

    #[test]
    fn capability_probe_gives_up_when_the_helper_hangs() {
        let fake = Scratch::new("fake-ffmpeg-hang");
        fake.with_contents(b"#!/bin/sh\nsleep 30\n", 0o755);
        let started = Instant::now();
        assert!(
            !probe_ffmpeg_h264_vaapi_with(&fake.0, Duration::from_millis(100)),
            "hanging helper reported as supported"
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "probe did not honour its timeout"
        );
    }

    #[test]
    fn helper_presence_matches_the_path_scan() {
        assert_eq!(ffmpeg_supports_h264_vaapi(), ffmpeg_on_path().is_some());
    }
}
