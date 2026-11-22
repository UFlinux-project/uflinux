//! The only place that knows gpu-screen-recorder's command line.
//!
//! The argument builder is pure and unit-tested so the preset mapping can be
//! checked without a GPU. The recorder itself is a plain child process: the tool
//! is its parent, which is what keeps the backend daemonless.
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use gtk::glib;
use settings_core::{AudioSource, CaptureQuality, ScreenCapture};

/// Program name installed by the gpu-screen-recorder package.
pub const BACKEND: &str = "gpu-screen-recorder";

const EXIT_OK: i32 = 0;
const EXIT_NO_VIDEO: i32 = 55;

/// How long to wait for the recorder to close the file after a stop request.
/// The backend answers `stop` only once the muxer is done, which can take a
/// moment on a long recording.
const STOP_TIMEOUT: Duration = Duration::from_secs(30);

/// Pause and resume are answered immediately, so a short timeout keeps a wedged
/// socket from freezing the UI.
const QUICK_TIMEOUT: Duration = Duration::from_millis(500);

#[derive(Clone, Debug)]
pub struct Region {
    pub output: gtk::gdk::Rectangle,
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

#[derive(Clone, Debug)]
pub enum Target {
    Screen(String),
    Area(String, Region),
    Window(String),
}

impl Target {
    fn recording_source(&self) -> Option<String> {
        match self {
            Self::Screen(connector) => Some(connector.clone()),
            Self::Area(_, region) => {
                // GSR takes the region in global Wayland logical coordinates.
                let (x, y) = (region.output.x() + region.x, region.output.y() + region.y);
                Some(format!("{}x{}+{x}+{y}", region.width, region.height))
            }
            Self::Window(_) => None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Job {
    pub target: Target,
    pub path: PathBuf,
    pub prefs: ScreenCapture,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Saved(PathBuf),
    Failed(String),
}

fn no_yes(flag: bool) -> &'static str {
    if flag { "no" } else { "yes" }
}

fn capture_flags(job: &Job, source: String) -> Vec<String> {
    vec![
        "-w".to_string(),
        source,
        "-o".to_string(),
        job.path.to_string_lossy().into_owned(),
        "-cursor".to_string(),
        no_yes(!job.prefs.include_cursor).to_string(),
        // "-v" is the FPS/damage readout, not verbosity; it would spam stderr.
        "-v".to_string(),
        "no".to_string(),
    ]
}

/// Quality layer, mirroring `add_quality_command_line_args` in the official GTK
/// frontend: named quality, or a constant bitrate when the user picks "Bitrate".
fn quality_flags(prefs: &ScreenCapture) -> Vec<String> {
    match prefs.quality {
        CaptureQuality::ConstantBitrate => vec![
            "-bm".to_string(),
            "cbr".to_string(),
            "-q".to_string(),
            prefs.bitrate_kbps.to_string(),
        ],
        quality => vec!["-q".to_string(), quality.id().to_string()],
    }
}

/// Advanced layer. A `None`/`Auto` value means "Backend default" and is left off
/// the command line entirely, which keeps the tool honest when the backend
/// changes its own defaults.
fn advanced_flags(prefs: &ScreenCapture) -> Vec<String> {
    let mut argv = Vec::new();
    if let Some(codec) = prefs.video_codec.as_deref() {
        argv.extend(["-k".to_string(), codec.to_string()]);
    }
    if let Some(mode) = prefs.framerate_mode.backend() {
        argv.extend(["-fm".to_string(), mode.to_string()]);
    }
    if let Some(range) = prefs.color_range.backend() {
        argv.extend(["-cr".to_string(), range.to_string()]);
    }
    if let Some(container) = prefs.container.as_deref() {
        argv.extend(["-c".to_string(), container.to_string()]);
    }
    if let Some(codec) = prefs.audio_codec.as_ref() {
        argv.extend(["-ac".to_string(), codec.id().to_string()]);
    }
    argv
}

/// Audio layer. `-a` is a repeatable list argument, so each source is its own
/// pair of flags.
fn audio_flags(prefs: &ScreenCapture) -> Vec<String> {
    let mut argv = Vec::new();
    match prefs.audio_source {
        AudioSource::None => {}
        AudioSource::Output | AudioSource::Input => {
            if let Some(source) = prefs.audio_source.backend() {
                argv.extend(["-a".to_string(), source.to_string()]);
            }
        }
        AudioSource::OutputAndInput => {
            argv.extend(["-a".to_string(), "default_output".to_string()]);
            argv.extend(["-a".to_string(), "default_input".to_string()]);
        }
    }
    argv
}

/// Flags for a video recording. `socket` enables the IPC control channel; when
/// it is `None` the recorder can only be stopped with a signal.
fn recording_args(job: &Job, source: String, socket: Option<&Path>) -> Vec<String> {
    let mut argv = capture_flags(job, source);
    argv.extend(["-f".to_string(), job.prefs.fps.to_string()]);
    // Keep keyframes close together, including on drivers with buffered output.
    argv.extend(["-keyint".to_string(), "1".to_string()]);
    argv.extend(quality_flags(&job.prefs));
    argv.extend(audio_flags(&job.prefs));
    argv.extend(advanced_flags(&job.prefs));
    if let Some(socket) = socket {
        argv.extend(["-ipc".to_string(), socket.to_string_lossy().into_owned()]);
    }
    argv
}

fn program() -> Option<PathBuf> {
    glib::find_program_in_path(BACKEND)
}

/// True when the recorder is installed. The overlay uses this to hide recording
/// rather than failing after the user presses the button.
pub fn backend_available() -> bool {
    program().is_some()
}

/// A running recorder. The child is reaped in a worker thread and the result is
/// delivered back on the main context, so the HUD stays responsive for the whole
/// recording.
#[derive(Clone, Debug)]
pub struct Recorder {
    pid: u32,
    socket: Option<PathBuf>,
    path: PathBuf,
    stopping: Arc<AtomicBool>,
    exited: Arc<AtomicBool>,
}

pub struct RecorderExit {
    code: Option<i32>,
    detail: Option<String>,
}

impl Recorder {
    /// Spawns the recorder and reports its exit on the GTK main context.
    pub fn start(
        job: &Job,
        socket: Option<&Path>,
        on_exit: impl FnOnce(RecorderExit) + Send + 'static,
    ) -> Result<Self, String> {
        let Some(program) = program() else {
            return Err(format!("{BACKEND} is not installed"));
        };
        let source = job
            .target
            .recording_source()
            .ok_or("Window recording is unavailable on labwc")?;
        let mut child = Command::new(program)
            .args(recording_args(job, source, socket))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| format!("Cannot start recorder: {error}"))?;
        let pid = child.id();
        let exited = Arc::new(AtomicBool::new(false));
        let worker_exited = exited.clone();
        let stderr = child.stderr.take().expect("piped recorder stderr");
        let context = glib::MainContext::default();
        std::thread::spawn(move || {
            let mut detail = None;
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                if let Some(error) = line.strip_prefix("gsr error: ") {
                    detail = Some(error.to_owned());
                }
            }
            let code = child.wait().ok().and_then(|status| status.code());
            worker_exited.store(true, Ordering::Release);
            context.spawn(async move { on_exit(RecorderExit { code, detail }) });
        });
        Ok(Self {
            pid,
            socket: socket.map(Path::to_path_buf),
            path: job.path.clone(),
            stopping: Arc::new(AtomicBool::new(false)),
            exited,
        })
    }

    /// Asks the recorder to finish the file. The blocking IPC call runs in a
    /// worker thread; the exit callback is what reports the outcome.
    pub fn stop(&self) {
        if self.stopping.swap(true, Ordering::Relaxed) {
            return;
        }
        let Some(socket) = self.socket.clone() else {
            kill(self.pid, libc::SIGINT);
            return;
        };
        let recorder = self.clone();
        std::thread::spawn(move || {
            if let Err(error) = ipc_request(&socket, "stop", None, STOP_TIMEOUT) {
                if !recorder.exited.load(Ordering::Acquire) {
                    eprintln!("uflinux-screencapture: IPC stop failed: {error}, trying SIGINT");
                    kill(recorder.pid, libc::SIGINT);
                }
            }
        });
    }

    /// Pauses or resumes the capture. Falls back to SIGUSR2, which the backend
    /// handles as a pause toggle, when there is no control socket.
    pub fn set_paused(&self, paused: bool) -> Result<(), String> {
        match &self.socket {
            Some(socket) => {
                let data = if paused { "true" } else { "false" };
                let reply = ipc_request(socket, "set-paused", Some(data), QUICK_TIMEOUT)?;
                if reply.contains("\"result\":\"ok\"") {
                    Ok(())
                } else {
                    Err(ipc_error(&reply))
                }
            }
            None => {
                kill(self.pid, libc::SIGUSR2);
                Ok(())
            }
        }
    }

    /// Asks the recorder to finish the file and forgets about it, so that this
    /// process can exit while the recorder writes out the last frames. Used when
    /// the session terminates us in the middle of a recording.
    pub fn terminate(self) {
        self.stopping.store(true, Ordering::Relaxed);
        kill(self.pid, libc::SIGINT);
    }

    /// Reports the result after the backend has flushed and closed the file.
    pub fn outcome(&self, exit: RecorderExit) -> Outcome {
        if let Some(socket) = &self.socket {
            let _ = std::fs::remove_file(socket);
        }
        let metadata = self.path.metadata().ok();
        let empty_file = metadata.is_some_and(|metadata| metadata.len() == 0);
        if exit.code == Some(EXIT_NO_VIDEO) || empty_file {
            let _ = std::fs::remove_file(&self.path);
            return Outcome::Failed(
                exit.detail
                    .unwrap_or_else(|| "Recorder did not write any video".into()),
            );
        }
        match exit.code {
            Some(EXIT_OK) if self.path.is_file() => Outcome::Saved(self.path.clone()),
            Some(EXIT_OK) => Outcome::Failed("Recorder did not write any video".into()),
            Some(code) => Outcome::Failed(
                exit.detail
                    .unwrap_or_else(|| format!("Recorder failed (exit {code})")),
            ),
            None => Outcome::Failed("Recording was interrupted".into()),
        }
    }
}

fn kill(pid: u32, signal: i32) -> bool {
    // SAFETY: kill(2) on our own child is always safe; failure just means it is
    // already gone.
    unsafe { libc::kill(pid as libc::pid_t, signal) == 0 }
}

fn ipc_request(
    socket: &Path,
    name: &str,
    data: Option<&str>,
    timeout: Duration,
) -> Result<String, String> {
    let mut stream = UnixStream::connect(socket).map_err(|error| error.to_string())?;
    stream
        .set_read_timeout(Some(timeout))
        .map_err(|error| error.to_string())?;
    let payload = match data {
        Some(data) => format!("{{\"id\":1,\"name\":\"{name}\",\"data\":{data}}}\n"),
        None => format!("{{\"id\":1,\"name\":\"{name}\"}}\n"),
    };
    stream
        .write_all(payload.as_bytes())
        .map_err(|error| error.to_string())?;
    stream.flush().map_err(|error| error.to_string())?;
    read_reply(&mut stream)
}

/// Reads the reply to a request. Deferred names such as `stop` are answered only
/// after the muxer has closed the file.
fn read_reply(stream: &mut UnixStream) -> Result<String, String> {
    let mut buffer = String::new();
    let mut chunk = [0u8; 4096];
    loop {
        let read = stream.read(&mut chunk).map_err(|error| error.to_string())?;
        if read == 0 {
            break;
        }
        buffer.push_str(&String::from_utf8_lossy(&chunk[..read]));
        if buffer.contains('\n') {
            break;
        }
    }
    if buffer.trim().is_empty() {
        return Err("Recorder exited before the request finished".to_string());
    }
    Ok(buffer)
}

fn ipc_error(reply: &str) -> String {
    reply_text(reply).unwrap_or_else(|| format!("Unexpected recorder reply: {reply}"))
}

fn reply_text(reply: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(reply.trim())
        .ok()?
        .get("data")
        .and_then(|data| data.as_str())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use settings_core::{ColorRange, FramerateMode};

    fn job(extension: &str) -> Job {
        Job {
            target: Target::Screen("DP-1".into()),
            path: PathBuf::from(format!("/tmp/shot.{extension}")),
            prefs: ScreenCapture::default(),
        }
    }

    fn flag<'a>(argv: &'a [String], key: &str) -> Option<&'a str> {
        argv.iter()
            .position(|arg| arg == key)
            .and_then(|index| argv.get(index + 1))
            .map(String::as_str)
    }

    fn args(job: &Job, socket: Option<&Path>) -> Vec<String> {
        recording_args(job, job.target.recording_source().unwrap(), socket)
    }

    #[test]
    fn area_uses_global_logical_coordinates() {
        let mut j = job("mp4");
        j.target = Target::Area(
            "DP-1".into(),
            Region {
                output: gtk::gdk::Rectangle::new(-500, 100, 500, 500),
                x: 10,
                y: 20,
                width: 300,
                height: 400,
            },
        );
        assert_eq!(flag(&args(&j, None), "-w"), Some("300x400+-490+120"));
    }

    #[test]
    fn cursor_can_be_dropped() {
        let mut j = job("mp4");
        j.prefs.include_cursor = false;
        assert_eq!(flag(&args(&j, None), "-cursor"), Some("no"));
    }

    #[test]
    fn recording_defaults_match_backend() {
        let argv = args(&job("mp4"), None);
        assert_eq!(flag(&argv, "-f"), Some("60"));
        assert_eq!(flag(&argv, "-keyint"), Some("1"));
        assert_eq!(flag(&argv, "-q"), Some("very_high"));
        assert_eq!(flag(&argv, "-c"), None);
        assert_eq!(flag(&argv, "-k"), None);
        assert_eq!(flag(&argv, "-fm"), None);
        assert_eq!(flag(&argv, "-cr"), None);
        assert_eq!(flag(&argv, "-a"), None);
        assert_eq!(flag(&argv, "-ac"), None);
    }

    #[test]
    fn named_quality_maps_to_backend_names() {
        for (quality, name) in [
            (CaptureQuality::Medium, "medium"),
            (CaptureQuality::High, "high"),
            (CaptureQuality::VeryHigh, "very_high"),
            (CaptureQuality::Ultra, "ultra"),
        ] {
            let mut j = job("mp4");
            j.prefs.quality = quality;
            let argv = args(&j, None);
            assert_eq!(flag(&argv, "-q"), Some(name));
            assert_eq!(flag(&argv, "-bm"), None);
        }
    }

    #[test]
    fn bitrate_quality_switches_to_cbr() {
        let mut j = job("mp4");
        j.prefs.quality = CaptureQuality::ConstantBitrate;
        j.prefs.bitrate_kbps = 25000;
        let argv = args(&j, None);
        assert_eq!(flag(&argv, "-bm"), Some("cbr"));
        assert_eq!(flag(&argv, "-q"), Some("25000"));
    }

    #[test]
    fn audio_sources_are_repeated() {
        let mut j = job("mp4");
        j.prefs.audio_source = AudioSource::Output;
        assert_eq!(flag(&args(&j, None), "-a"), Some("default_output"));
        j.prefs.audio_source = AudioSource::Input;
        assert_eq!(flag(&args(&j, None), "-a"), Some("default_input"));
        j.prefs.audio_source = AudioSource::OutputAndInput;
        let argv = args(&j, None);
        let sources: Vec<&str> = argv
            .iter()
            .enumerate()
            .filter(|(_, arg)| *arg == "-a")
            .filter_map(|(index, _)| argv.get(index + 1))
            .map(String::as_str)
            .collect();
        assert_eq!(sources, vec!["default_output", "default_input"]);
        j.prefs.audio_source = AudioSource::None;
        assert!(!args(&j, None).iter().any(|arg| arg == "-a"));
    }

    #[test]
    fn advanced_values_are_passed_through() {
        let mut j = job("mkv");
        j.prefs.video_codec = Some("hevc".to_string());
        j.prefs.framerate_mode = FramerateMode::Constant;
        j.prefs.color_range = ColorRange::Full;
        j.prefs.container = Some("mkv".to_string());
        let argv = args(&j, None);
        assert_eq!(flag(&argv, "-k"), Some("hevc"));
        assert_eq!(flag(&argv, "-fm"), Some("cfr"));
        assert_eq!(flag(&argv, "-cr"), Some("full"));
        assert_eq!(flag(&argv, "-c"), Some("mkv"));
    }

    #[test]
    fn ipc_socket_is_last() {
        let socket = Path::new("/run/user/1000/uflinux-screencapture-1.sock");
        let argv = args(&job("mp4"), Some(socket));
        assert_eq!(
            argv.last().map(String::as_str),
            Some(socket.to_str().unwrap())
        );
        assert_eq!(&argv[argv.len() - 2], "-ipc");
    }

    #[test]
    fn error_reply_data_is_the_message() {
        assert_eq!(
            ipc_error("{\"id\":1,\"result\":\"error\",\"data\":\"already stopping\"}"),
            "already stopping"
        );
    }

    #[test]
    fn rejects_container_with_no_video_packets() {
        let path = std::env::temp_dir().join(format!(
            "uflinux-empty-recording-test-{}.mp4",
            std::process::id()
        ));
        std::fs::write(&path, b"container header").unwrap();
        let recorder = Recorder {
            pid: 0,
            socket: None,
            path: path.clone(),
            stopping: Arc::new(AtomicBool::new(true)),
            exited: Arc::new(AtomicBool::new(true)),
        };
        assert_eq!(
            recorder.outcome(RecorderExit {
                code: Some(EXIT_NO_VIDEO),
                detail: None
            }),
            Outcome::Failed("Recorder did not write any video".into())
        );
        assert!(!path.exists());
    }
}
