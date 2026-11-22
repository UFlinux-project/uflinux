//! Out-of-process VA-API H.264 backend.
//!
//! `ffmpeg -c:v h264_vaapi` runs as a long-lived child process: NV12 goes in
//! through stdin, Annex-B comes out through stdout. A subprocess is used
//! instead of Rust bindings on purpose — no `libav*` to link, and a driver that
//! wedges takes only the child with it.
//!
//! The frame-delimiting half ([`AnnexBSplitter`]) and the process plumbing
//! ([`EncoderProcess`]/[`EncoderSpawner`]) are separate so the state machine can
//! be driven by a fake process in tests; nothing here requires ffmpeg or a GPU.

use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::convert::{RgbaFrame, write_nv12};
use crate::probe::{DEFAULT_VAAPI_DEVICE, ffmpeg_on_path, is_char_device};
use crate::{CodecError, Result};

/// VA-API encoders reject frames smaller than this; the caller should
/// [`crate::downgrade`] instead of failing the session.
pub const MIN_DIMENSION: u32 = 128;

/// Bound the wait for a submitted frame if the driver stops producing output.
const FRAME_TIMEOUT: Duration = Duration::from_secs(1);

/// The encoder shape this crate expects from any H.264 backend.
///
/// Mirrors `ironrdp_egfx::encode::H264Encoder` (including
/// `request_key_frame`, not `request_idr`) so the crate builds without the
/// `h264` feature and the server can hold a `Box<dyn H264Backend>` regardless of
/// which tier it picked. Output is Annex-B with in-band SPS/PPS on the IDR,
/// ready for `GraphicsPipelineServer::try_send_avc420_frame`.
pub trait H264Backend: Send {
    /// Encodes one frame. An empty `Vec` means "nothing to send yet", matching
    /// egfx's convention that an empty frame is skipped.
    fn encode(&mut self, frame: &RgbaFrame<'_>) -> Result<Vec<u8>>;

    /// Ask for the next frame to be an IDR.
    fn request_key_frame(&mut self);

    /// Drop encoder state; the next frame starts a fresh stream.
    fn reset(&mut self);
}

/// Knobs for the ffmpeg helper.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VaapiConfig {
    /// DRM render node, e.g. `/dev/dri/renderD128`.
    pub device: String,
    /// Force-IDR interval in frames (`-g`). 0 keeps the driver default.
    pub gop: u32,
    /// Passed to ffmpeg as `-preset`; ignored by `h264_vaapi` but harmless and
    /// keeps the command line comparable with software encodes.
    pub preset: String,
}

impl Default for VaapiConfig {
    fn default() -> Self {
        Self {
            device: DEFAULT_VAAPI_DEVICE.to_owned(),
            gop: 60,
            preset: "ultrafast".to_owned(),
        }
    }
}

impl VaapiConfig {
    /// Config for the device reported by [`crate::detect_vaapi`].
    pub fn from_hardware(hw: &crate::HardwareCaps) -> Option<Self> {
        hw.device.clone().map(|device| Self {
            device,
            ..Self::default()
        })
    }
}

/// Builds the ffmpeg command line. Pure, so the flags are covered by a test.
fn ffmpeg_args(config: &VaapiConfig, width: u32, height: u32) -> Vec<String> {
    let mut args = vec![
        "-hide_banner".to_owned(),
        "-loglevel".to_owned(),
        "error".to_owned(),
        "-vaapi_device".to_owned(),
        config.device.clone(),
        "-f".to_owned(),
        "rawvideo".to_owned(),
        "-pix_fmt".to_owned(),
        "nv12".to_owned(),
        "-s".to_owned(),
        format!("{width}x{height}"),
        "-i".to_owned(),
        "pipe:0".to_owned(),
        "-vf".to_owned(),
        "format=nv12,hwupload".to_owned(),
        "-c:v".to_owned(),
        "h264_vaapi".to_owned(),
        // A static desktop may submit only one frame. Encode and flush it
        // without waiting for more input or for B-frame reordering.
        "-async_depth".to_owned(),
        "1".to_owned(),
        "-bf".to_owned(),
        "0".to_owned(),
        "-flush_packets".to_owned(),
        "1".to_owned(),
    ];
    if !config.preset.is_empty() {
        args.extend(["-preset".to_owned(), config.preset.clone()]);
    }
    if config.gop > 0 {
        args.extend(["-g".to_owned(), config.gop.to_string()]);
    }
    args.extend(["-f".to_owned(), "h264".to_owned(), "pipe:1".to_owned()]);
    args
}

/// A running encoder helper.
pub trait EncoderProcess: Send {
    /// Feeds one NV12 frame to the helper.
    fn write_frame(&mut self, nv12: &[u8]) -> Result<()>;

    /// Collects Annex-B output after submitting a frame.
    ///
    /// `Ok(None)` means the helper closed its stdout (it exited or was told to
    /// finish); `Ok(Some(empty))` means "nothing yet";
    /// `Err(CodecError::BackendDied)` means the helper died mid-stream.
    fn poll_output(&mut self) -> Result<Option<Vec<u8>>>;

    /// Kills the helper.
    fn shutdown(&mut self);
}

/// Starts [`EncoderProcess`] instances.
pub trait EncoderSpawner: Send {
    fn spawn(
        &self,
        config: &VaapiConfig,
        width: u32,
        height: u32,
    ) -> Result<Box<dyn EncoderProcess>>;
}

/// The production spawner: a real ffmpeg child process.
pub struct FfmpegSpawner {
    /// Path to the helper binary.
    pub program: PathBuf,
}

impl FfmpegSpawner {
    /// Uses `ffmpeg` from `$PATH`.
    pub fn new() -> Result<Self> {
        ffmpeg_on_path()
            .map(|program| Self { program })
            .ok_or_else(|| CodecError::BackendUnavailable("ffmpeg not found on PATH".to_owned()))
    }
}

impl Default for FfmpegSpawner {
    fn default() -> Self {
        Self {
            program: PathBuf::from("ffmpeg"),
        }
    }
}

impl EncoderSpawner for FfmpegSpawner {
    fn spawn(
        &self,
        config: &VaapiConfig,
        width: u32,
        height: u32,
    ) -> Result<Box<dyn EncoderProcess>> {
        let args = ffmpeg_args(config, width, height);
        tracing::debug!(program = ?self.program, ?args, "starting VA-API encoder helper");
        let mut child = Command::new(&self.program)
            .args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| CodecError::BackendUnavailable("helper has no stdin".to_owned()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| CodecError::BackendUnavailable("helper has no stdout".to_owned()))?;

        // A pump thread keeps the pipe drained so `poll_output` never blocks and
        // a stalled encoder can never deadlock us on a full pipe buffer.
        let (sender, receiver) = mpsc::channel::<Vec<u8>>();
        let reader = std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            let mut chunk = vec![0u8; 64 * 1024];
            loop {
                match reader.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(read) => {
                        if sender.send(chunk[..read].to_vec()).is_err() {
                            break;
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(_) => break,
                }
            }
        });

        Ok(Box::new(FfmpegProcess {
            child,
            stdin: Some(stdin),
            receiver,
            reader: Some(reader),
        }))
    }
}

struct FfmpegProcess {
    child: Child,
    /// `None` once the pipe has been closed, which is how the helper is told
    /// that the stream ended.
    stdin: Option<ChildStdin>,
    receiver: Receiver<Vec<u8>>,
    reader: Option<JoinHandle<()>>,
}

impl EncoderProcess for FfmpegProcess {
    fn write_frame(&mut self, nv12: &[u8]) -> Result<()> {
        let Some(stdin) = self.stdin.as_mut() else {
            return Err(CodecError::BackendDied);
        };
        match stdin.write_all(nv12).and_then(|()| stdin.flush()) {
            Ok(()) => Ok(()),
            Err(error) => {
                // A closed pipe means the helper died, most often because the
                // driver rejected the frame.
                tracing::debug!(%error, "VA-API helper stdin failed");
                self.shutdown();
                Err(CodecError::BackendDied)
            }
        }
    }

    fn poll_output(&mut self) -> Result<Option<Vec<u8>>> {
        let mut out = match self.receiver.recv_timeout(FRAME_TIMEOUT) {
            Ok(chunk) => chunk,
            Err(RecvTimeoutError::Disconnected) => {
                self.shutdown();
                return Ok(None);
            }
            Err(RecvTimeoutError::Timeout) => {
                tracing::debug!("VA-API helper timed out waiting for a frame");
                self.shutdown();
                return Err(CodecError::BackendDied);
            }
        };
        for chunk in self.receiver.try_iter() {
            out.extend_from_slice(&chunk);
        }
        Ok(Some(out))
    }

    fn shutdown(&mut self) {
        // Dropping stdin is what tells the helper that the stream ended.
        if let Some(mut stdin) = self.stdin.take() {
            let _ = stdin.flush();
        }
        if let Some(reader) = self.reader.take() {
            drop(reader);
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Hardware H.264 backend driven through ffmpeg's `h264_vaapi` encoder.
pub struct VaapiH264Encoder {
    config: VaapiConfig,
    spawner: Box<dyn EncoderSpawner>,
    process: Option<Box<dyn EncoderProcess>>,
    splitter: AnnexBSplitter,
    nv12: Vec<u8>,
    size: Option<(u32, u32)>,
    restart: bool,
}

impl VaapiH264Encoder {
    /// Starts against the configured render node. Fails with
    /// [`CodecError::BackendUnavailable`] when ffmpeg is missing and
    /// [`CodecError::DeviceNotUsable`] when the device is not a render node.
    ///
    /// The child process is not spawned until the first frame, because the
    /// command line needs the frame size.
    pub fn new(config: VaapiConfig) -> Result<Self> {
        let device = Path::new(&config.device);
        if !is_char_device(device) {
            return Err(CodecError::DeviceNotUsable(device.to_path_buf()));
        }
        Ok(Self::with_spawner(config, Box::new(FfmpegSpawner::new()?)))
    }

    /// Injectable form of [`Self::new`], used by the tests and by callers that
    /// want a custom helper command.
    pub fn with_spawner(config: VaapiConfig, spawner: Box<dyn EncoderSpawner>) -> Self {
        Self {
            config,
            spawner,
            process: None,
            splitter: AnnexBSplitter::default(),
            nv12: Vec::new(),
            size: None,
            restart: false,
        }
    }

    fn stop(&mut self) {
        if let Some(mut process) = self.process.take() {
            process.shutdown();
        }
        self.splitter.clear();
        self.size = None;
    }
}

impl H264Backend for VaapiH264Encoder {
    fn encode(&mut self, frame: &RgbaFrame<'_>) -> Result<Vec<u8>> {
        frame.validate()?;
        if frame.width < MIN_DIMENSION || frame.height < MIN_DIMENSION {
            return Err(CodecError::FrameTooSmall { min: MIN_DIMENSION });
        }

        write_nv12(frame, &mut self.nv12)?;
        let size = (frame.width, frame.height);

        // A key-frame request, a resolution change, or a dead helper all mean the
        // stream has to start over; a fresh helper begins with SPS/PPS + IDR,
        // which is exactly what a forced IDR looks like here.
        if self.restart || self.size != Some(size) {
            self.stop();
            self.restart = false;
        }

        if self.process.is_none() {
            self.process = Some(self.spawner.spawn(&self.config, size.0, size.1)?);
            self.size = Some(size);
        }

        let process = self.process.as_mut().ok_or(CodecError::BackendDied)?;
        match process.write_frame(&self.nv12) {
            Ok(()) => {}
            Err(CodecError::BackendDied) => {
                self.process = None;
                self.size = None;
                return Err(CodecError::BackendDied);
            }
            Err(error) => return Err(error),
        }

        let chunk = match process.poll_output() {
            Ok(chunk) => chunk,
            Err(CodecError::BackendDied) => {
                self.process = None;
                self.size = None;
                return Err(CodecError::BackendDied);
            }
            Err(error) => return Err(error),
        };

        let mut access_units = Vec::new();
        if let Some(chunk) = chunk {
            self.splitter.push(&chunk);
            while let Some(unit) = self.splitter.next_unit() {
                access_units.extend_from_slice(&unit);
            }
            while let Some(unit) = self.splitter.flush() {
                access_units.extend_from_slice(&unit);
            }
        } else {
            // stdout closed: the helper exited after its last frame.
            self.splitter.clear();
            self.process = None;
            self.size = None;
            return Err(CodecError::BackendDied);
        }
        Ok(access_units)
    }

    fn request_key_frame(&mut self) {
        self.restart = true;
    }

    fn reset(&mut self) {
        self.stop();
        self.restart = false;
    }
}

impl Drop for VaapiH264Encoder {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Splits a byte stream into complete Annex-B access units.
///
/// Start codes are `00 00 01` and `00 00 00 01`; a start code that arrives
/// across two reads is handled because the search resumes a few bytes before the
/// end of the buffer. Each returned unit keeps its own leading start code.
#[derive(Default)]
pub(crate) struct AnnexBSplitter {
    buffer: Vec<u8>,
    in_unit: bool,
    scan_from: usize,
    min_scan: usize,
}

/// Smallest number of bytes that can still be the beginning of a start code.
const START_CODE_TAIL: usize = 3;

impl AnnexBSplitter {
    fn push(&mut self, bytes: &[u8]) {
        self.buffer.extend_from_slice(bytes);
    }

    fn clear(&mut self) {
        self.buffer.clear();
        self.in_unit = false;
        self.scan_from = 0;
        self.min_scan = 0;
    }

    /// The next complete access unit, or `None` while it is still incomplete.
    ///
    /// A unit is complete when the *next* start code arrives; use [`Self::flush`]
    /// at a read boundary for the unit that no following start code terminated.
    fn next_unit(&mut self) -> Option<Vec<u8>> {
        loop {
            if !self.in_unit {
                // Discard anything before the first start code.
                let Some(index) = find_start_code(&self.buffer, self.scan_from) else {
                    self.rewind_scan(self.buffer.len(), 0);
                    return None;
                };
                let start = start_code_begin(&self.buffer, index);
                self.buffer.drain(..start);
                self.min_scan = start_code_len(&self.buffer);
                self.in_unit = true;
                self.scan_from = self.min_scan;
                continue;
            }

            match find_start_code(&self.buffer, self.scan_from) {
                Some(index) => {
                    let end = start_code_begin(&self.buffer, index);
                    let unit = self.buffer[..end].to_vec();
                    self.buffer.drain(..end);
                    self.in_unit = false;
                    self.min_scan = 0;
                    self.scan_from = 0;
                    return Some(unit);
                }
                None => {
                    self.rewind_scan(self.buffer.len(), self.min_scan);
                    return None;
                }
            }
        }
    }

    /// Everything buffered so far, emitted at a read boundary.
    ///
    /// Strict start-code delimiting only completes a unit when the *next* start
    /// code arrives, which would hold the last NAL of every frame until the
    /// frame after it. ffmpeg writes one frame's NALs per write, so bytes that
    /// have already arrived are treated as complete. Up to two trailing zero
    /// bytes are held back because they may be a start code that straddles the
    /// read boundary; tearing one in half would corrupt the unit.
    fn flush(&mut self) -> Option<Vec<u8>> {
        if self.buffer.is_empty() {
            self.clear();
            return None;
        }
        if !self.in_unit {
            // Adopt a unit that no following start code has terminated.
            let index = find_start_code(&self.buffer, self.scan_from)?;
            self.buffer.drain(..start_code_begin(&self.buffer, index));
            self.min_scan = start_code_len(&self.buffer);
            self.in_unit = true;
            self.scan_from = self.min_scan;
        }
        let held = trailing_zeros(&self.buffer).min(START_CODE_TAIL - 1);
        if self.buffer.len() <= self.min_scan + held {
            // Nothing but a start code (plus possible zeros) so far.
            self.scan_from = self.buffer.len();
            return None;
        }
        let keep = self.buffer.len() - held;
        let unit = self.buffer[..keep].to_vec();
        self.buffer.drain(..keep);
        self.in_unit = false;
        self.min_scan = 0;
        self.scan_from = 0;
        Some(unit)
    }

    /// A start code may straddle two reads, so resume a few bytes early — but
    /// never back into the start code of the unit being scanned, whose trailing
    /// `00 00 01` would otherwise match itself.
    fn rewind_scan(&mut self, len: usize, floor: usize) {
        self.scan_from = len.saturating_sub(START_CODE_TAIL).max(floor);
    }
}

/// Number of `00` bytes at the end of `buffer`.
fn trailing_zeros(buffer: &[u8]) -> usize {
    buffer.iter().rev().take_while(|&&byte| byte == 0).count()
}

/// Index of the first `00 00 01` at or after `from`. Never matches inside a
/// 4-byte start code, because `from` is expected to sit past it.
fn find_start_code(buffer: &[u8], from: usize) -> Option<usize> {
    if buffer.len() < from + 3 {
        return None;
    }
    (from..=buffer.len() - 3).find(|&i| buffer[i] == 0 && buffer[i + 1] == 0 && buffer[i + 2] == 1)
}

/// Expands a `00 00 01` match to include a leading zero byte (4-byte start
/// code).
fn start_code_begin(buffer: &[u8], index: usize) -> usize {
    if index > 0 && buffer[index - 1] == 0 {
        index - 1
    } else {
        index
    }
}

fn start_code_len(buffer: &[u8]) -> usize {
    if buffer.len() >= 4 && buffer[0] == 0 && buffer[1] == 0 && buffer[2] == 0 && buffer[3] == 1 {
        4
    } else {
        3
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    fn frame_of(width: u32, height: u32) -> Vec<u8> {
        vec![0x40u8; (width * height * 4) as usize]
    }

    #[test]
    fn command_line_carries_the_device_size_and_encoder() {
        let args = ffmpeg_args(&VaapiConfig::default(), 1920, 1080);
        let joined = args.join(" ");
        for expected in [
            "-hide_banner",
            "-loglevel error",
            "-vaapi_device /dev/dri/renderD128",
            "-pix_fmt nv12",
            "-s 1920x1080",
            "-i pipe:0",
            "-vf format=nv12,hwupload",
            "-c:v h264_vaapi",
            "-async_depth 1",
            "-bf 0",
            "-flush_packets 1",
            "-g 60",
            "-f h264 pipe:1",
        ] {
            assert!(joined.contains(expected), "missing {expected} in {joined}");
        }
    }

    #[test]
    fn command_line_omits_a_zero_gop() {
        let config = VaapiConfig {
            gop: 0,
            ..VaapiConfig::default()
        };
        let joined = ffmpeg_args(&config, 128, 128).join(" ");
        assert!(!joined.contains(" -g "), "{joined}");
    }

    #[test]
    fn splitter_handles_a_start_code_split_across_reads() {
        let mut splitter = AnnexBSplitter::default();
        splitter.push(&[0x00, 0x00, 0x00, 0x01, 0x67, 0xAA, 0xBB]);
        assert!(splitter.next_unit().is_none(), "unit ended too early");
        splitter.push(&[0x00]);
        assert!(
            splitter.next_unit().is_none(),
            "unit ended on a partial code"
        );
        splitter.push(&[0x00, 0x01, 0x68]);
        let unit = splitter.next_unit().expect("first unit");
        assert_eq!(unit, vec![0x00, 0x00, 0x00, 0x01, 0x67, 0xAA, 0xBB]);
        assert_eq!(
            splitter.flush().expect("second unit at the read boundary"),
            vec![0x00, 0x00, 0x01, 0x68]
        );
    }

    #[test]
    fn splitter_keeps_three_and_four_byte_start_codes() {
        let mut splitter = AnnexBSplitter::default();
        splitter.push(&[
            0x00, 0x00, 0x01, 0x67, 0x11, 0x00, 0x00, 0x00, 0x01, 0x68, 0x22,
        ]);
        splitter.push(&[0x00, 0x00, 0x01, 0x65, 0x33]);
        assert_eq!(
            splitter.next_unit().expect("3-byte code unit"),
            vec![0x00, 0x00, 0x01, 0x67, 0x11]
        );
        assert_eq!(
            splitter.next_unit().expect("4-byte code unit"),
            vec![0x00, 0x00, 0x00, 0x01, 0x68, 0x22]
        );
        assert_eq!(
            splitter
                .flush()
                .expect("trailing unit at the read boundary"),
            vec![0x00, 0x00, 0x01, 0x65, 0x33]
        );
    }

    #[test]
    fn splitter_drops_garbage_before_the_first_start_code() {
        let mut splitter = AnnexBSplitter::default();
        splitter.push(&[0xAA, 0xBB, 0x00, 0x00, 0x01, 0x67, 0x01]);
        assert!(splitter.next_unit().is_none());
        splitter.push(&[0x00, 0x00, 0x01, 0x68]);
        assert_eq!(
            splitter.next_unit().expect("unit"),
            vec![0x00, 0x00, 0x01, 0x67, 0x01]
        );
    }

    #[test]
    fn splitter_returns_only_complete_units() {
        let mut splitter = AnnexBSplitter::default();
        splitter.push(&[0x00, 0x00, 0x01, 0x67, 0x01, 0x02]);
        assert!(splitter.next_unit().is_none());
        splitter.push(&[0x03, 0x04]);
        assert!(splitter.next_unit().is_none(), "no second start code yet");
        assert_eq!(
            splitter.flush().expect("flush"),
            vec![0x00, 0x00, 0x01, 0x67, 0x01, 0x02, 0x03, 0x04]
        );
        assert!(splitter.next_unit().is_none());
    }

    #[test]
    fn splitter_holds_back_a_possible_split_start_code() {
        let mut splitter = AnnexBSplitter::default();
        // Trailing zeros may be the beginning of the next unit's start code.
        splitter.push(&[0x00, 0x00, 0x01, 0x65, 0x07, 0x00, 0x00]);
        assert_eq!(
            splitter.flush().expect("unit"),
            vec![0x00, 0x00, 0x01, 0x65, 0x07]
        );
        splitter.push(&[0x00, 0x01, 0x68, 0x09]);
        assert!(
            splitter.next_unit().is_none(),
            "strict delimiting still needs a following start code"
        );
        assert_eq!(
            splitter.flush().expect("continued start code"),
            vec![0x00, 0x00, 0x00, 0x01, 0x68, 0x09]
        );
    }

    /// Records what the encoder writes and replays scripted output, so the
    /// state machine can be tested without ffmpeg.
    struct FakeProcess {
        frames: Arc<Mutex<Vec<Vec<u8>>>>,
        outputs: Arc<Mutex<VecDeque<Vec<u8>>>>,
        alive: Arc<Mutex<bool>>,
    }

    impl EncoderProcess for FakeProcess {
        fn write_frame(&mut self, nv12: &[u8]) -> Result<()> {
            if !*self.alive.lock().unwrap() {
                return Err(CodecError::BackendDied);
            }
            self.frames.lock().unwrap().push(nv12.to_vec());
            Ok(())
        }

        fn poll_output(&mut self) -> Result<Option<Vec<u8>>> {
            if !*self.alive.lock().unwrap() {
                // stdout closed: the helper exited.
                return Ok(None);
            }
            Ok(Some(
                self.outputs.lock().unwrap().pop_front().unwrap_or_default(),
            ))
        }

        fn shutdown(&mut self) {
            *self.alive.lock().unwrap() = false;
        }
    }

    #[derive(Clone)]
    struct FakeSpawner {
        spawns: Arc<Mutex<Vec<(u32, u32)>>>,
        frames: Arc<Mutex<Vec<Vec<u8>>>>,
        outputs: Arc<Mutex<VecDeque<Vec<u8>>>>,
        alive: Arc<Mutex<bool>>,
    }

    impl FakeSpawner {
        fn new() -> Self {
            Self {
                spawns: Arc::new(Mutex::new(Vec::new())),
                frames: Arc::new(Mutex::new(Vec::new())),
                outputs: Arc::new(Mutex::new(VecDeque::new())),
                alive: Arc::new(Mutex::new(true)),
            }
        }

        fn queue(&self, bytes: Vec<u8>) {
            self.outputs.lock().unwrap().push_back(bytes);
        }

        fn kill(&self) {
            *self.alive.lock().unwrap() = false;
        }

        fn spawn_count(&self) -> usize {
            self.spawns.lock().unwrap().len()
        }

        fn written(&self) -> Vec<Vec<u8>> {
            self.frames.lock().unwrap().clone()
        }
    }

    impl EncoderSpawner for FakeSpawner {
        fn spawn(
            &self,
            _config: &VaapiConfig,
            width: u32,
            height: u32,
        ) -> Result<Box<dyn EncoderProcess>> {
            self.spawns.lock().unwrap().push((width, height));
            *self.alive.lock().unwrap() = true;
            Ok(Box::new(FakeProcess {
                frames: Arc::clone(&self.frames),
                outputs: Arc::clone(&self.outputs),
                alive: Arc::clone(&self.alive),
            }))
        }
    }

    fn encoder(spawner: FakeSpawner) -> VaapiH264Encoder {
        VaapiH264Encoder::with_spawner(VaapiConfig::default(), Box::new(spawner))
    }

    const IDR: [u8; 6] = [0x00, 0x00, 0x00, 0x01, 0x65, 0x99];

    #[test]
    fn first_encode_spawns_the_helper_and_feeds_it_nv12() {
        let spawner = FakeSpawner::new();
        spawner.queue(IDR.to_vec());
        let mut enc = encoder(spawner.clone());
        let data = frame_of(128, 128);
        let out = enc
            .encode(&RgbaFrame::new(&data, 128, 128))
            .expect("encode");
        assert_eq!(out, IDR.to_vec());
        assert_eq!(spawner.spawn_count(), 1);
        assert_eq!(spawner.written()[0].len(), 128 * 128 * 3 / 2);
    }

    #[test]
    fn frames_below_the_minimum_size_ask_for_a_downgrade() {
        let spawner = FakeSpawner::new();
        let mut enc = encoder(spawner.clone());
        let data = frame_of(64, 64);
        let error = enc
            .encode(&RgbaFrame::new(&data, 64, 64))
            .expect_err("64x64 should be rejected");
        assert!(
            matches!(error, CodecError::FrameTooSmall { min: MIN_DIMENSION }),
            "{error}"
        );
        assert_eq!(
            spawner.spawn_count(),
            0,
            "helper started for a rejected frame"
        );
    }

    #[test]
    fn invalid_frames_are_rejected_before_the_helper_is_used() {
        let spawner = FakeSpawner::new();
        let mut enc = encoder(spawner.clone());
        let data = frame_of(129, 128);
        assert!(matches!(
            enc.encode(&RgbaFrame::new(&data, 129, 128)),
            Err(CodecError::InvalidFrame(_))
        ));
        assert_eq!(spawner.spawn_count(), 0);
    }

    #[test]
    fn a_dead_helper_reports_backend_died_and_the_next_call_respawns_it() {
        let spawner = FakeSpawner::new();
        spawner.queue(IDR.to_vec());
        let mut enc = encoder(spawner.clone());
        let data = frame_of(160, 160);
        enc.encode(&RgbaFrame::new(&data, 160, 160)).unwrap();

        spawner.kill();
        let error = enc
            .encode(&RgbaFrame::new(&data, 160, 160))
            .expect_err("dead helper must not look healthy");
        assert!(matches!(error, CodecError::BackendDied), "{error}");

        spawner.queue(IDR.to_vec());
        assert_eq!(
            enc.encode(&RgbaFrame::new(&data, 160, 160)).unwrap(),
            IDR.to_vec(),
            "encoder did not recover"
        );
        assert_eq!(spawner.spawn_count(), 2);
    }

    #[test]
    fn a_key_frame_request_or_reset_restarts_the_helper() {
        let spawner = FakeSpawner::new();
        spawner.queue(IDR.to_vec());
        let mut enc = encoder(spawner.clone());
        let data = frame_of(128, 128);
        enc.encode(&RgbaFrame::new(&data, 128, 128)).unwrap();

        enc.request_key_frame();
        spawner.queue(IDR.to_vec());
        enc.encode(&RgbaFrame::new(&data, 128, 128)).unwrap();
        assert_eq!(spawner.spawn_count(), 2, "key frame did not restart");

        enc.reset();
        spawner.queue(IDR.to_vec());
        enc.encode(&RgbaFrame::new(&data, 128, 128)).unwrap();
        assert_eq!(spawner.spawn_count(), 3, "reset did not restart");
    }

    #[test]
    fn a_resolution_change_restarts_the_helper() {
        let spawner = FakeSpawner::new();
        spawner.queue(IDR.to_vec());
        let mut enc = encoder(spawner.clone());
        let small = frame_of(128, 128);
        enc.encode(&RgbaFrame::new(&small, 128, 128)).unwrap();
        let big = frame_of(256, 256);
        spawner.queue(IDR.to_vec());
        enc.encode(&RgbaFrame::new(&big, 256, 256)).unwrap();
        assert_eq!(&*spawner.spawns.lock().unwrap(), &[(128, 128), (256, 256)]);
    }

    #[test]
    fn output_without_a_start_code_yields_nothing() {
        let spawner = FakeSpawner::new();
        // A read that only carried the beginning of a start code must not
        // produce a frame.
        spawner.queue(vec![0x00, 0x00]);
        let mut enc = encoder(spawner);
        let data = frame_of(128, 128);
        assert!(
            enc.encode(&RgbaFrame::new(&data, 128, 128))
                .expect("encode")
                .is_empty(),
            "partial start code was emitted"
        );
    }

    #[test]
    fn a_unit_that_arrived_in_full_is_returned_on_the_same_frame() {
        let spawner = FakeSpawner::new();
        // One write from the helper is one frame's NALs, so it is handed back
        // immediately instead of waiting for the next frame's start code.
        spawner.queue(vec![0x00, 0x00, 0x00, 0x01, 0x65, 0x01, 0x02]);
        let mut enc = encoder(spawner);
        let data = frame_of(128, 128);
        assert_eq!(
            enc.encode(&RgbaFrame::new(&data, 128, 128))
                .expect("encode"),
            vec![0x00, 0x00, 0x00, 0x01, 0x65, 0x01, 0x02]
        );
    }

    #[test]
    fn a_spawner_failure_surfaces_as_backend_unavailable() {
        struct Broken;
        impl EncoderSpawner for Broken {
            fn spawn(
                &self,
                _config: &VaapiConfig,
                _width: u32,
                _height: u32,
            ) -> Result<Box<dyn EncoderProcess>> {
                Err(CodecError::BackendUnavailable("no helper".to_owned()))
            }
        }
        let mut enc = VaapiH264Encoder::with_spawner(VaapiConfig::default(), Box::new(Broken));
        let data = frame_of(128, 128);
        assert!(matches!(
            enc.encode(&RgbaFrame::new(&data, 128, 128)),
            Err(CodecError::BackendUnavailable(_))
        ));
    }

    #[test]
    fn new_needs_a_real_render_node() {
        let config = VaapiConfig {
            device: "/definitely/not/a/render/node".to_owned(),
            ..VaapiConfig::default()
        };
        assert!(matches!(
            VaapiH264Encoder::new(config),
            Err(CodecError::DeviceNotUsable(_))
        ));
    }

    /// Needs ffmpeg and a VA-API driver; run with `cargo test -- --ignored`.
    #[test]
    #[ignore = "requires ffmpeg and a VA-API render node"]
    fn encodes_a_frame_with_the_real_helper() {
        let mut enc = match VaapiH264Encoder::new(VaapiConfig::default()) {
            Ok(enc) => enc,
            Err(error) => {
                eprintln!("skipping, no VA-API backend here: {error}");
                return;
            }
        };
        let data = frame_of(256, 256);
        let out = enc.encode(&RgbaFrame::new(&data, 256, 256)).unwrap();
        assert!(
            out.windows(4)
                .any(|bytes| bytes[..3] == [0, 0, 1] && bytes[3] & 0x1f == 5),
            "first submitted frame did not produce an Annex-B IDR"
        );
    }
}
