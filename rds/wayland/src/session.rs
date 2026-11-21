//! The Wayland event loop and the public [`DesktopBackend`].
//!
//! A single background thread owns the [`Connection`] and its [`EventQueue`];
//! see the crate-level documentation for the threading model. This module is
//! the only place in the crate that talks Wayland.

use std::ffi::CString;
use std::fs;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::mpsc::{
    Receiver, RecvTimeoutError, Sender, SyncSender, TrySendError, channel, sync_channel,
};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::Duration;

use nix::errno::Errno;
use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
use nix::unistd::Pid;
use wayland_client::globals::{Global, GlobalList, GlobalListContents, registry_queue_init};
use wayland_client::protocol::wl_buffer::WlBuffer;
use wayland_client::protocol::wl_output::WlOutput;
use wayland_client::protocol::wl_pointer::{Axis, ButtonState};
use wayland_client::protocol::wl_registry::{self, WlRegistry};
use wayland_client::protocol::wl_seat::WlSeat;
use wayland_client::protocol::wl_shm::{Format as ShmFormat, WlShm};
use wayland_client::protocol::wl_shm_pool::WlShmPool;
use wayland_client::{
    Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum, delegate_noop, event_created_child,
};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1;
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1;
use wayland_protocols_wlr::output_management::v1::client::zwlr_output_configuration_head_v1::ZwlrOutputConfigurationHeadV1;
use wayland_protocols_wlr::output_management::v1::client::zwlr_output_configuration_v1::{
    self as cfg_evt, ZwlrOutputConfigurationV1,
};
use wayland_protocols_wlr::output_management::v1::client::zwlr_output_head_v1::{
    self as head_evt, ZwlrOutputHeadV1,
};
use wayland_protocols_wlr::output_management::v1::client::zwlr_output_manager_v1::{
    self as manager_evt, ZwlrOutputManagerV1,
};
use wayland_protocols_wlr::output_management::v1::client::zwlr_output_mode_v1::ZwlrOutputModeV1;
use wayland_protocols_wlr::screencopy::v1::client::zwlr_screencopy_frame_v1::{
    self as frame_evt, Flags, ZwlrScreencopyFrameV1,
};
use wayland_protocols_wlr::screencopy::v1::client::zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1;
use wayland_protocols_wlr::virtual_pointer::v1::client::zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1;
use wayland_protocols_wlr::virtual_pointer::v1::client::zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1;

use crate::compositor::Compositor;
use crate::error::BackendError;
use crate::frame::{Frame, dirty_regions};
use crate::input::{KeyboardEvent, KeyboardState, PointerEvent, event_time};
use crate::spec::DesktopSpec;

/// XKB keymap (US layout, evdev keycodes) handed to the compositor so it can
/// translate the `KEY_*` codes injected through the virtual keyboard.
///
/// Must be the *text* form: `wlr_virtual_keyboard_v1` reads the fd with
/// `xkb_keymap_new_from_string(..., XKB_KEYMAP_FORMAT_TEXT_V1, ...)`, so a
/// compiled (`mkx`) keymap is rejected with a parse error. The `include`
/// directives are resolved by the compositor against the system xkb rules.
const KEYMAP: &[u8] = include_bytes!("keymap/us.xkb");
/// `zwp_virtual_keyboard_v1::keymap` format of an XKB text keymap
/// (`XKB_KEYMAP_FORMAT_TEXT_V1`).
const KEYMAP_FORMAT_XKB_V1: u32 = 1;
/// How long the event loop blocks in `poll` before re-checking the command
/// channel. Bounds shutdown latency without busy-waiting.
const POLL_TIMEOUT_MS: i32 = 16;
/// Depth of the frame rendezvous channel.
const FRAME_CHANNEL_DEPTH: usize = 1;

/// Outbound messages from the API thread to the event-loop thread.
enum Command {
    Keyboard(KeyboardEvent),
    Pointer(PointerEvent),
    Resize(u32, u32),
    Shutdown,
}

/// State shared between the API thread and the event-loop thread.
struct Shared {
    size: Mutex<(u32, u32)>,
    error: Mutex<Option<BackendError>>,
}

/// An in-flight screencopy capture, from `capture_output` until `ready`.
struct Capture {
    frame: ZwlrScreencopyFrameV1,
    buffer: Option<WlBuffer>,
    shm_fd: Option<OwnedFd>,
    addr: *mut u8,
    map_len: usize,
    width: u32,
    height: u32,
    stride: u32,
    order: PixelOrder,
    inverted: bool,
}

// The only non-`Send` member is the raw `mmap` pointer, which the event-loop
// thread owns exclusively.
unsafe impl Send for Capture {}

impl Capture {
    fn new(frame: ZwlrScreencopyFrameV1) -> Self {
        Self {
            frame,
            buffer: None,
            shm_fd: None,
            addr: std::ptr::null_mut(),
            map_len: 0,
            width: 0,
            height: 0,
            stride: 0,
            order: [2, 1, 0],
            inverted: false,
        }
    }

    fn is_ready_to_copy(&self) -> bool {
        !self.addr.is_null()
    }

    fn unmap(&mut self) {
        if !self.addr.is_null() {
            // SAFETY: `addr`/`map_len` come from a successful `mmap` and are
            // cleared immediately afterwards.
            unsafe {
                libc::munmap(self.addr as *mut libc::c_void, self.map_len);
            }
            self.addr = std::ptr::null_mut();
            self.map_len = 0;
        }
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.unmap();
        if let Some(buffer) = self.buffer.take() {
            buffer.destroy();
        }
        // `frame` is dropped without an explicit `destroy()`; the compositor
        // reaps the object when the capture is abandoned.
        let _ = &self.frame;
    }
}

/// The user-data object dispatched on the event-loop thread.
struct Session {
    globals: GlobalList,
    qhandle: QueueHandle<Session>,
    shm: WlShm,
    screencopy: ZwlrScreencopyManagerV1,
    output_manager: Option<ZwlrOutputManagerV1>,
    keyboard_manager: Option<ZwpVirtualKeyboardManagerV1>,
    pointer_manager: Option<ZwlrVirtualPointerManagerV1>,
    seat: Option<WlSeat>,
    keyboard: Option<ZwpVirtualKeyboardV1>,
    pointer: Option<ZwlrVirtualPointerV1>,
    keymap_fd: Option<OwnedFd>,
    keyboard_state: KeyboardState,
    outputs: Vec<WlOutput>,
    bound_outputs: Vec<u32>,
    heads: Vec<ZwlrOutputHeadV1>,
    capture: Option<Capture>,
    previous: Vec<u8>,
    frames_tx: SyncSender<Frame>,
    shared: Arc<Shared>,
    finished: bool,
    config_serial: u32,
    pending_config: Option<ZwlrOutputConfigurationV1>,
}

impl Session {
    #[allow(clippy::too_many_arguments)]
    fn new(
        globals: GlobalList,
        qhandle: QueueHandle<Session>,
        shm: WlShm,
        screencopy: ZwlrScreencopyManagerV1,
        output_manager: Option<ZwlrOutputManagerV1>,
        keyboard_manager: Option<ZwpVirtualKeyboardManagerV1>,
        pointer_manager: Option<ZwlrVirtualPointerManagerV1>,
        frames_tx: SyncSender<Frame>,
        shared: Arc<Shared>,
    ) -> Self {
        let mut session = Self {
            globals,
            qhandle,
            shm,
            screencopy,
            output_manager,
            keyboard_manager,
            pointer_manager,
            seat: None,
            keyboard: None,
            pointer: None,
            keymap_fd: None,
            keyboard_state: KeyboardState::empty(),
            outputs: Vec::new(),
            bound_outputs: Vec::new(),
            heads: Vec::new(),
            capture: None,
            previous: Vec::new(),
            frames_tx,
            shared,
            finished: false,
            config_serial: 0,
            pending_config: None,
        };
        session.scan_globals();
        session
    }

    /// Bind every advertised global we care about and (re)create the virtual
    /// input devices once their prerequisites are present.
    fn scan_globals(&mut self) {
        let list = self.globals.contents().clone_list();
        for global in &list {
            self.handle_global(global);
        }
        self.maybe_create_devices();
    }

    fn handle_global(&mut self, global: &Global) {
        match global.interface.as_str() {
            "wl_seat" if self.seat.is_none() => {
                self.seat = Some(self.globals.registry().bind(
                    global.name,
                    global.version.min(10),
                    &self.qhandle,
                    (),
                ));
            }
            "wl_output" if !self.bound_outputs.contains(&global.name) => {
                let output: WlOutput = self.globals.registry().bind(
                    global.name,
                    global.version.min(4),
                    &self.qhandle,
                    (),
                );
                self.bound_outputs.push(global.name);
                self.outputs.push(output);
            }
            _ => {}
        }
    }

    fn maybe_create_devices(&mut self) {
        if self.keyboard.is_none() {
            let created = match (&self.seat, &self.keyboard_manager) {
                (Some(seat), Some(manager)) => {
                    Some(manager.create_virtual_keyboard(seat, &self.qhandle, ()))
                }
                _ => None,
            };
            if let Some(keyboard) = created {
                self.upload_keymap(&keyboard);
                keyboard.modifiers(0, 0, 0, 0);
                self.keyboard = Some(keyboard);
            }
        }
        if self.pointer.is_none() {
            let created = match (&self.seat, &self.pointer_manager) {
                (Some(seat), Some(manager)) => {
                    Some(manager.create_virtual_pointer(Some(seat), &self.qhandle, ()))
                }
                _ => None,
            };
            if let Some(pointer) = created {
                self.pointer = Some(pointer);
            }
        }
    }

    /// Write the compiled keymap into a memfd and hand it to the compositor.
    fn upload_keymap(&mut self, keyboard: &ZwpVirtualKeyboardV1) {
        let fd = match memfd("uflinux-rds-keymap") {
            Ok(fd) => fd,
            Err(e) => {
                tracing::warn!(error = %e, "could not create the keymap memfd");
                return;
            }
        };
        let len = KEYMAP.len();
        if write_all(&fd, KEYMAP).is_err()
            || nix::unistd::ftruncate(&fd, len as libc::off_t).is_err()
        {
            tracing::warn!("could not write the keymap memfd");
            return;
        }
        keyboard.keymap(KEYMAP_FORMAT_XKB_V1, fd.as_fd(), len as u32);
        // The compositor maps the fd itself; keep it alive for the session.
        self.keymap_fd = Some(fd);
    }

    /// Start a fresh full-output capture when none is in flight.
    fn ensure_capture(&mut self) {
        if self.finished || self.capture.is_some() {
            return;
        }
        let Some(output) = self.outputs.first().cloned() else {
            return;
        };
        let frame = self
            .screencopy
            .capture_output(0, &output, &self.qhandle, ());
        self.capture = Some(Capture::new(frame));
    }

    fn inject_keyboard(&mut self, event: KeyboardEvent) {
        let Some(keyboard) = self.keyboard.clone() else {
            return;
        };
        let Some(keycode) = event.keycode() else {
            return;
        };
        keyboard.key(event_time(), keycode, event.state().as_u32());
        if self.keyboard_state.apply(keycode, event.state()).changed {
            let state = self.keyboard_state;
            keyboard.modifiers(state.depressed, state.latched, state.locked, state.group);
        }
    }

    fn inject_pointer(&mut self, event: PointerEvent) {
        let Some(pointer) = self.pointer.clone() else {
            return;
        };
        let (width, height) = self.size();
        let extent_x = width.max(1);
        let extent_y = height.max(1);
        let time = event_time();
        match event {
            PointerEvent::Move { x, y } => {
                pointer.motion_absolute(time, x as u32, y as u32, extent_x, extent_y);
            }
            PointerEvent::Button { button, pressed } => {
                let state = if pressed {
                    ButtonState::Pressed
                } else {
                    ButtonState::Released
                };
                pointer.button(time, button as u32, state);
            }
            PointerEvent::Wheel { axis_x, axis_y } => {
                if axis_y != 0 {
                    pointer.axis_discrete(time, Axis::VerticalScroll, axis_y as f64, axis_y);
                }
                if axis_x != 0 {
                    pointer.axis_discrete(time, Axis::HorizontalScroll, axis_x as f64, axis_x);
                }
            }
        }
        pointer.frame();
    }

    /// Ask the compositor to resize through wlr-output-management. When the
    /// protocol or the heads are unavailable we still advertise the new size
    /// so that [`DesktopBackend::size`] stays truthful.
    fn apply_resize(&mut self, width: u32, height: u32) {
        let manager = self.output_manager.clone();
        let heads = self.heads.clone();
        if let Some(manager) = manager.filter(|_| !heads.is_empty()) {
            // The serial of the most recent `done` event, per the protocol.
            let config = manager.create_configuration(self.config_serial, &self.qhandle, ());
            for head in &heads {
                let config_head = config.enable_head(head, &self.qhandle, ());
                config_head.set_custom_mode(width as i32, height as i32, 0);
            }
            config.apply();
            self.pending_config = Some(config);
        }
        self.set_size(width, height);
    }

    /// Diff a freshly captured frame against the previous one and queue it.
    fn deliver(&mut self, rgba: Vec<u8>, width: u32, height: u32) {
        self.set_size(width, height);
        let dirty = dirty_regions(&self.previous, &rgba, width, height);
        self.previous = rgba.clone();
        if dirty.is_empty() {
            return;
        }
        let Ok(frame) = Frame::new(width, height, rgba, dirty) else {
            return;
        };
        match self.frames_tx.try_send(frame) {
            Ok(()) => {}
            // A frame is already queued; drop this one. The receiver will get
            // the next diff computed against this updated baseline.
            Err(TrySendError::Full(_)) => {}
            Err(TrySendError::Disconnected(_)) => self.finished = true,
        }
    }

    fn fail(&mut self, error: BackendError) {
        {
            let mut slot = self
                .shared
                .error
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if slot.is_none() {
                *slot = Some(error);
            }
        }
        self.finished = true;
    }

    fn set_size(&self, width: u32, height: u32) {
        *self.shared_size() = (width, height);
    }

    fn size(&self) -> (u32, u32) {
        *self.shared_size()
    }

    fn shared_size(&self) -> MutexGuard<'_, (u32, u32)> {
        self.shared
            .size
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Allocate a shm buffer for the capture and issue `copy_with_damage`.
    fn setup_buffer(
        &mut self,
        proxy: &ZwlrScreencopyFrameV1,
        width: u32,
        height: u32,
        stride: u32,
        format: ShmFormat,
    ) {
        let Some(order) = pixel_order(format) else {
            tracing::warn!(?format, "screencopy offered an unsupported format");
            self.abandon_capture(proxy);
            return;
        };
        let size = (stride as u64 * height as u64) as usize;
        if size == 0 {
            return;
        }
        let fd = match memfd("uflinux-rds-capture") {
            Ok(fd) => fd,
            Err(e) => {
                tracing::warn!(error = %e, "capture memfd failed");
                return;
            }
        };
        if nix::unistd::ftruncate(&fd, size as libc::off_t).is_err() {
            tracing::warn!("capture ftruncate failed");
            return;
        }
        // `create_pool`/`create_buffer` only borrow `self.shm`/`self.qhandle`
        // for the duration of the call, so the mutable borrow of
        // `self.capture` below does not overlap them.
        let pool = self
            .shm
            .create_pool(fd.as_fd(), size as i32, &self.qhandle, ());
        let buffer = pool.create_buffer(
            0,
            width as i32,
            height as i32,
            stride as i32,
            format,
            &self.qhandle,
            (),
        );
        pool.destroy();
        // SAFETY: `fd` is a live file descriptor of length `size`.
        let addr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                size,
                libc::PROT_READ,
                libc::MAP_SHARED,
                fd.as_raw_fd(),
                0,
            )
        };
        if addr == libc::MAP_FAILED {
            tracing::warn!(error = %std::io::Error::last_os_error(), "capture mmap failed");
            buffer.destroy();
            return;
        }
        match self.capture.as_mut() {
            Some(capture) => {
                capture.buffer = Some(buffer.clone());
                capture.shm_fd = Some(fd);
                capture.addr = addr as *mut u8;
                capture.map_len = size;
                capture.width = width;
                capture.height = height;
                capture.stride = stride;
                capture.order = order;
            }
            None => {
                buffer.destroy();
                return;
            }
        }
        proxy.copy_with_damage(&buffer);
    }

    fn abandon_capture(&mut self, proxy: &ZwlrScreencopyFrameV1) {
        self.capture = None;
        proxy.destroy();
    }
}

impl Dispatch<WlRegistry, GlobalListContents> for Session {
    fn event(
        session: &mut Session,
        _proxy: &WlRegistry,
        event: wl_registry::Event,
        _data: &GlobalListContents,
        _conn: &Connection,
        _qhandle: &QueueHandle<Session>,
    ) {
        match event {
            wl_registry::Event::Global { .. } => session.scan_globals(),
            wl_registry::Event::GlobalRemove { .. } => {}
            _ => {}
        }
    }
}

impl Dispatch<ZwlrScreencopyFrameV1, ()> for Session {
    fn event(
        session: &mut Session,
        proxy: &ZwlrScreencopyFrameV1,
        event: frame_evt::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Session>,
    ) {
        match event {
            frame_evt::Event::Buffer {
                format,
                width,
                height,
                stride,
            } => {
                // wlroots offers the output's native format. `setup_buffer`
                // rejects anything we cannot turn into RGBA8888.
                let offered = match format {
                    WEnum::Value(format) => Some(format),
                    WEnum::Unknown(other) => {
                        tracing::warn!(format = other, "screencopy offered an unknown format");
                        None
                    }
                };
                match offered {
                    Some(format) => session.setup_buffer(proxy, width, height, stride, format),
                    None => session.abandon_capture(proxy),
                }
            }
            frame_evt::Event::Flags { flags } => {
                let inverted = match flags {
                    WEnum::Value(flags) => flags.contains(Flags::YInvert),
                    WEnum::Unknown(value) => (value & 1) != 0,
                };
                if let Some(capture) = session.capture.as_mut() {
                    capture.inverted = inverted;
                }
            }
            frame_evt::Event::Ready { .. } => {
                let captured = match session.capture.as_mut() {
                    Some(capture) if capture.is_ready_to_copy() => {
                        // SAFETY: `addr`/`map_len` describe the readable mmap
                        // installed by `setup_buffer`.
                        let src =
                            unsafe { std::slice::from_raw_parts(capture.addr, capture.map_len) };
                        let rgba = pixels_to_rgba(
                            src,
                            capture.width,
                            capture.height,
                            capture.stride,
                            capture.inverted,
                            capture.order,
                        );
                        (rgba, capture.width, capture.height)
                    }
                    _ => return,
                };
                session.capture = None;
                proxy.destroy();
                session.deliver(captured.0, captured.1, captured.2);
            }
            frame_evt::Event::Failed => session.abandon_capture(proxy),
            _ => {}
        }
    }
}

impl Dispatch<ZwlrOutputManagerV1, ()> for Session {
    fn event(
        session: &mut Session,
        _proxy: &ZwlrOutputManagerV1,
        event: manager_evt::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Session>,
    ) {
        match event {
            manager_evt::Event::Done { serial } => session.config_serial = serial,
            manager_evt::Event::Finished => session.heads.clear(),
            _ => {}
        }
    }

    event_created_child!(Session, ZwlrOutputManagerV1, [
        manager_evt::EVT_HEAD_OPCODE => (ZwlrOutputHeadV1, ())
    ]);
}

impl Dispatch<ZwlrOutputHeadV1, ()> for Session {
    fn event(
        session: &mut Session,
        proxy: &ZwlrOutputHeadV1,
        event: head_evt::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Session>,
    ) {
        match event {
            // `mode`/`current_mode` reference a new object we own; the
            // compositor destroys modes with their head, so we only track it.
            head_evt::Event::Mode { .. } | head_evt::Event::CurrentMode { .. } => {
                if !session.heads.iter().any(|head| head.id() == proxy.id()) {
                    session.heads.push(proxy.clone());
                }
            }
            head_evt::Event::Finished => session.heads.retain(|head| head.id() != proxy.id()),
            _ => {}
        }
    }

    event_created_child!(Session, ZwlrOutputHeadV1, [
        head_evt::EVT_MODE_OPCODE => (ZwlrOutputModeV1, ())
    ]);
}

impl Dispatch<ZwlrOutputConfigurationV1, ()> for Session {
    fn event(
        session: &mut Session,
        proxy: &ZwlrOutputConfigurationV1,
        event: cfg_evt::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Session>,
    ) {
        match event {
            cfg_evt::Event::Succeeded => tracing::debug!("output configuration applied"),
            cfg_evt::Event::Failed => tracing::warn!("output configuration failed"),
            cfg_evt::Event::Cancelled => tracing::warn!("output configuration cancelled"),
            _ => {}
        }
        proxy.destroy();
        session.pending_config = None;
    }
}

// Interfaces whose events we never act on.
delegate_noop!(Session: ignore WlShm);
delegate_noop!(Session: ignore WlShmPool);
delegate_noop!(Session: ignore WlBuffer);
delegate_noop!(Session: ignore WlOutput);
delegate_noop!(Session: ignore WlSeat);
delegate_noop!(Session: ignore ZwlrScreencopyManagerV1);
delegate_noop!(Session: ignore ZwlrOutputModeV1);
delegate_noop!(Session: ignore ZwlrOutputConfigurationHeadV1);
delegate_noop!(Session: ignore ZwlrVirtualPointerManagerV1);
delegate_noop!(Session: ignore ZwlrVirtualPointerV1);
delegate_noop!(Session: ignore ZwpVirtualKeyboardManagerV1);
delegate_noop!(Session: ignore ZwpVirtualKeyboardV1);

/// Convert an XRGB8888 buffer (with `stride`) into tightly packed RGBA8888,
/// flipping vertically when the capture is y-inverted.
/// Byte offsets of R, G and B inside one 32bpp `wl_shm` pixel.
///
/// `wl_shm` formats are documented as `[31:0] A:R:G:B`-style bitfields in
/// little-endian order, so the first byte in memory is the *lowest* field.
type PixelOrder = [usize; 3];

/// The R/G/B byte order of a supported 32bpp shared-memory format, or `None`
/// for formats we cannot turn into RGBA8888.
fn pixel_order(format: ShmFormat) -> Option<PixelOrder> {
    let order = match format {
        // [31:0] A:R:G:B / x:R:G:B -> memory B, G, R, A
        ShmFormat::Argb8888 | ShmFormat::Xrgb8888 => [2, 1, 0],
        // [31:0] A:B:G:R / x:B:G:R -> memory R, G, B, A
        ShmFormat::Abgr8888 | ShmFormat::Xbgr8888 => [0, 1, 2],
        // [31:0] B:G:R:A / B:G:R:x -> memory A:R:G:B
        ShmFormat::Bgra8888 | ShmFormat::Bgrx8888 => [1, 2, 3],
        // [31:0] R:G:B:A / R:G:B:x -> memory A, B, G, R
        ShmFormat::Rgba8888 | ShmFormat::Rgbx8888 => [3, 2, 1],
        _ => return None,
    };
    Some(order)
}

fn pixels_to_rgba(
    src: &[u8],
    width: u32,
    height: u32,
    stride: u32,
    flip: bool,
    order: PixelOrder,
) -> Vec<u8> {
    let stride = stride as usize;
    let width = width as usize;
    let height = height as usize;
    let mut out = vec![0u8; width * height * 4];
    if src.len() < stride * height {
        return out;
    }
    let [red, green, blue] = order;
    for (out_row, row) in out.chunks_mut(width * 4).enumerate() {
        let src_row = if flip { height - 1 - out_row } else { out_row };
        let base = src_row * stride;
        let line = &src[base..base + width * 4];
        for (pixel, source) in row.chunks_mut(4).zip(line.chunks_exact(4)) {
            // RDP ignores alpha, and a freshly captured desktop is opaque.
            pixel[0] = source[red];
            pixel[1] = source[green];
            pixel[2] = source[blue];
            pixel[3] = 0xff;
        }
    }
    out
}

/// `memfd_create` wrapped in an [`OwnedFd`].
fn memfd(name: &str) -> std::io::Result<OwnedFd> {
    let cname = CString::new(name)?;
    // SAFETY: `cname` is a valid NUL-terminated string and stays alive.
    let fd = unsafe { libc::memfd_create(cname.as_ptr(), libc::MFD_CLOEXEC) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `memfd_create` returned a fresh, owned descriptor.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// Write all of `buf` to `fd` at the current offset.
fn write_all(fd: impl AsFd, buf: &[u8]) -> std::io::Result<()> {
    let mut offset = 0;
    while offset < buf.len() {
        offset += nix::unistd::write(fd.as_fd(), &buf[offset..])?;
    }
    Ok(())
}

/// Non-blocking check whether the compositor child has exited.
fn child_exited(pid: Pid) -> Option<String> {
    match waitpid(pid, Some(WaitPidFlag::WNOHANG)) {
        Ok(WaitStatus::Exited(_, code)) => Some(format!("exit code {code}")),
        Ok(WaitStatus::Signaled(_, signal, _)) => Some(format!("killed by signal {signal}")),
        Ok(WaitStatus::StillAlive) => None,
        Err(Errno::ECHILD) => Some("no such process".to_owned()),
        Ok(other) => {
            tracing::trace!(?other, "unexpected waitpid status");
            None
        }
        Err(_) => None,
    }
}

/// The event-loop thread body: drain commands, keep one capture in flight,
/// wait for compositor events and notice an early child exit.
fn event_loop(
    mut queue: EventQueue<Session>,
    session: &mut Session,
    commands: &Receiver<Command>,
    pid: Pid,
    command: &str,
    shared: &Shared,
) {
    while !session.finished {
        while let Ok(command) = commands.try_recv() {
            match command {
                Command::Shutdown => session.finished = true,
                Command::Keyboard(event) => session.inject_keyboard(event),
                Command::Pointer(event) => session.inject_pointer(event),
                Command::Resize(width, height) => session.apply_resize(width, height),
            }
        }
        if session.finished {
            break;
        }

        session.ensure_capture();
        if let Err(e) = queue.flush() {
            session.fail(BackendError::wayland(e));
            break;
        }

        if let Some(guard) = queue.prepare_read() {
            let mut poll_fd = libc::pollfd {
                fd: guard.connection_fd().as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: one `pollfd` holding a live connection descriptor.
            let ready = unsafe { libc::poll(&mut poll_fd, 1, POLL_TIMEOUT_MS) };
            if ready > 0 {
                let _ = guard.read();
            }
            // Otherwise the guard is dropped, which cancels the pending read.
        }

        if let Err(e) = queue.dispatch_pending(session) {
            session.fail(BackendError::wayland(e));
            break;
        }

        if let Some(reason) = child_exited(pid) {
            session.fail(BackendError::Died {
                command: command.to_owned(),
                reason,
            });
            break;
        }
    }

    // Always leave a terminal cause behind so `next_frame` can explain itself.
    {
        let mut slot = shared
            .error
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if slot.is_none() {
            *slot = Some(BackendError::ShutDown);
        }
    }
    // Dropping `session` drops `frames_tx`, unblocking a waiting receiver.
}

/// The pieces produced by a successful handshake with the compositor.
struct Started {
    commands: Sender<Command>,
    frames: Receiver<Frame>,
    loop_thread: JoinHandle<()>,
    compositor: Compositor,
    shared: Arc<Shared>,
}

/// A headless desktop session: a nested compositor plus screen capture and
/// virtual input devices.
///
/// See the crate-level documentation for the threading model.
pub struct DesktopBackend {
    commands: Sender<Command>,
    frames: Receiver<Frame>,
    loop_thread: Option<JoinHandle<()>>,
    compositor: Option<Compositor>,
    runtime_dir: PathBuf,
    shared: Arc<Shared>,
}

impl DesktopBackend {
    /// Spawn the compositor described by `spec`, connect to it and start the
    /// event-loop thread.
    ///
    /// A missing or failing compositor binary yields [`BackendError::Spawn`];
    /// a compositor that starts but never publishes its socket yields
    /// [`BackendError::Connect`]. On any error the child process is terminated.
    pub fn spawn(spec: DesktopSpec) -> Result<Self, BackendError> {
        fs::create_dir_all(&spec.runtime_dir)?;
        let runtime_dir = spec.runtime_dir.clone();
        match Self::start(spec) {
            Ok(started) => Ok(Self {
                commands: started.commands,
                frames: started.frames,
                loop_thread: Some(started.loop_thread),
                compositor: Some(started.compositor),
                runtime_dir,
                shared: started.shared,
            }),
            Err(e) => {
                let _ = fs::remove_dir_all(&runtime_dir);
                Err(e)
            }
        }
    }

    fn start(spec: DesktopSpec) -> Result<Started, BackendError> {
        // Dropping `compositor` on any error path terminates the child.
        let compositor = Compositor::spawn(&spec)?;
        let socket_path = compositor.socket_path().to_path_buf();
        let pid = compositor.pid();
        let command = compositor.command().to_owned();

        let stream = UnixStream::connect(&socket_path).map_err(|e| BackendError::Connect {
            path: socket_path.clone(),
            reason: e.to_string(),
        })?;
        let conn = Connection::from_socket(stream).map_err(|e| BackendError::Connect {
            path: socket_path.clone(),
            reason: e.to_string(),
        })?;
        let (globals, queue) =
            registry_queue_init::<Session>(&conn).map_err(BackendError::wayland)?;
        let qhandle = queue.handle();

        let shm: WlShm = globals
            .bind(&qhandle, 1..=1, ())
            .map_err(|e| BackendError::Capture(format!("wl_shm: {e}")))?;
        let screencopy: ZwlrScreencopyManagerV1 = globals
            .bind(&qhandle, 1..=3, ())
            .map_err(|e| BackendError::Capture(format!("zwlr_screencopy_manager_v1: {e}")))?;
        // Optional: without them the session still streams frames, it just
        // cannot resize or inject the corresponding input.
        let output_manager: Option<ZwlrOutputManagerV1> = globals.bind(&qhandle, 1..=4, ()).ok();
        let keyboard_manager: Option<ZwpVirtualKeyboardManagerV1> =
            globals.bind(&qhandle, 1..=1, ()).ok();
        let pointer_manager: Option<ZwlrVirtualPointerManagerV1> =
            globals.bind(&qhandle, 1..=2, ()).ok();

        let shared = Arc::new(Shared {
            size: Mutex::new((spec.width, spec.height)),
            error: Mutex::new(None),
        });
        let (frames_tx, frames) = sync_channel(FRAME_CHANNEL_DEPTH);
        let (commands, command_rx) = channel();

        let session = Session::new(
            globals,
            qhandle,
            shm,
            screencopy,
            output_manager,
            keyboard_manager,
            pointer_manager,
            frames_tx,
            Arc::clone(&shared),
        );

        let loop_shared = Arc::clone(&shared);
        let loop_thread = std::thread::Builder::new()
            .name("uflinux-rds-wayland".to_owned())
            .spawn(move || {
                let mut session = session;
                event_loop(
                    queue,
                    &mut session,
                    &command_rx,
                    pid,
                    &command,
                    &loop_shared,
                );
            })
            .map_err(BackendError::wayland)?;

        Ok(Started {
            commands,
            frames,
            loop_thread,
            compositor,
            shared,
        })
    }

    /// Block until a new frame is available or `timeout` elapses.
    ///
    /// Returns `Ok(None)` on timeout. Once the session has ended (compositor
    /// died, or the backend was shut down) the terminal cause is returned
    /// instead.
    pub fn next_frame(&mut self, timeout: Duration) -> Result<Option<Frame>, BackendError> {
        match self.frames.recv_timeout(timeout) {
            Ok(frame) => Ok(Some(frame)),
            Err(RecvTimeoutError::Timeout) => Ok(None),
            Err(RecvTimeoutError::Disconnected) => Err(self.terminal_error()),
        }
    }

    /// Request a new desktop size.
    ///
    /// Asynchronous: it becomes visible on a later frame, while [`Self::size`]
    /// updates as soon as the event loop picks the request up.
    pub fn resize(&mut self, width: u32, height: u32) -> Result<(), BackendError> {
        if width == 0 || height == 0 {
            return Err(BackendError::Input("resize to a zero dimension".to_owned()));
        }
        self.send(Command::Resize(width, height))
    }

    /// Inject a key press or release, expressed as an X11 keysym.
    ///
    /// Keysyms with no evdev keycode are rejected synchronously with
    /// [`BackendError::Input`].
    pub fn inject_keyboard(&mut self, event: KeyboardEvent) -> Result<(), BackendError> {
        if event.keycode().is_none() {
            return Err(BackendError::Input(format!(
                "no keycode for keysym {:#x}",
                event.keysym
            )));
        }
        self.send(Command::Keyboard(event))
    }

    /// Inject a pointer action.
    pub fn inject_pointer(&mut self, event: PointerEvent) -> Result<(), BackendError> {
        self.send(Command::Pointer(event))
    }

    /// The current desktop size in pixels.
    pub fn size(&self) -> (u32, u32) {
        *self
            .shared
            .size
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The pid of the compositor child process, or `None` once shut down.
    pub fn pid(&self) -> Option<u32> {
        self.compositor.as_ref().map(|c| c.pid().as_raw() as u32)
    }

    /// `true` while the compositor process is running.
    pub fn is_alive(&self) -> bool {
        self.compositor.as_ref().is_some_and(Compositor::is_alive)
    }

    /// Stop the event loop, terminate the compositor and remove the private
    /// runtime directory.
    pub fn shutdown(mut self) -> Result<(), BackendError> {
        self.teardown();
        Ok(())
    }

    fn send(&self, command: Command) -> Result<(), BackendError> {
        self.commands
            .send(command)
            .map_err(|_| self.terminal_error())
    }

    fn terminal_error(&self) -> BackendError {
        if let Some(error) = self
            .shared
            .error
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
        {
            return error;
        }
        if let Some(compositor) = &self.compositor
            && !compositor.is_alive()
        {
            return BackendError::Died {
                command: compositor.command().to_owned(),
                reason: "compositor exited".to_owned(),
            };
        }
        BackendError::ShutDown
    }

    fn teardown(&mut self) {
        let _ = self.commands.send(Command::Shutdown);
        if let Some(thread) = self.loop_thread.take() {
            let _ = thread.join();
        }
        // Dropping the compositor terminates the child (SIGTERM, then SIGKILL).
        self.compositor = None;
        let _ = fs::remove_dir_all(&self.runtime_dir);
    }
}

impl Drop for DesktopBackend {
    fn drop(&mut self) {
        self.teardown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const XRGB: PixelOrder = [2, 1, 0];

    #[test]
    fn pixels_to_rgba_reorders_xrgb_to_rgba() {
        // One XRGB pixel in memory is B, G, R, pad.
        let src = [10u8, 20, 30, 0];
        assert_eq!(
            pixels_to_rgba(&src, 1, 1, 4, false, XRGB),
            vec![30, 20, 10, 255]
        );
    }

    #[test]
    fn pixels_to_rgba_reorders_xbgr_to_rgba() {
        // cage/headless offers XBGR8888: the same pixel is R, G, B, pad.
        let src = [30u8, 20, 10, 0];
        let order = pixel_order(ShmFormat::Xbgr8888).unwrap();
        assert_eq!(
            pixels_to_rgba(&src, 1, 1, 4, false, order),
            vec![30, 20, 10, 255]
        );
    }

    #[test]
    fn pixels_to_rgba_flips_vertically() {
        // 1x2: top row blue-ish, bottom row red-ish.
        let src = [0u8, 0, 255, 0, 255, 0, 0, 0];
        let out = pixels_to_rgba(&src, 1, 2, 4, true, XRGB);
        assert_eq!(&out[0..4], &[0, 0, 255, 255]);
        assert_eq!(&out[4..8], &[255, 0, 0, 255]);
    }

    #[test]
    fn pixels_to_rgba_honours_stride() {
        // 1x1 with an 8-byte stride: only the first four bytes are the pixel.
        let src = [10u8, 20, 30, 0, 0xAA, 0xAA, 0xAA, 0xAA];
        assert_eq!(
            pixels_to_rgba(&src, 1, 1, 8, false, XRGB),
            vec![30, 20, 10, 255]
        );
    }

    #[test]
    fn pixels_to_rgba_zeroes_undersized_input() {
        let out = pixels_to_rgba(&[0u8; 4], 2, 2, 8, false, XRGB);
        assert_eq!(out, vec![0u8; 2 * 2 * 4]);
    }

    #[test]
    fn every_supported_32bpp_format_has_a_pixel_order() {
        let supported = [
            ShmFormat::Argb8888,
            ShmFormat::Xrgb8888,
            ShmFormat::Abgr8888,
            ShmFormat::Xbgr8888,
            ShmFormat::Bgra8888,
            ShmFormat::Bgrx8888,
            ShmFormat::Rgba8888,
            ShmFormat::Rgbx8888,
        ];
        for format in supported {
            let order = pixel_order(format).expect("supported format");
            assert_eq!(order.len(), 3);
            assert!(order.iter().all(|&offset| offset < 4));
            let [a, b, c] = order;
            assert!(a != b && a != c && b != c, "{format:?} reuses a byte");
        }
    }

    #[test]
    fn unsupported_formats_have_no_pixel_order() {
        for format in [ShmFormat::Rgb888, ShmFormat::Nv12, ShmFormat::C8] {
            assert!(
                pixel_order(format).is_none(),
                "{format:?} should be rejected"
            );
        }
    }

    #[test]
    fn pixel_order_matches_documented_byte_layouts() {
        // A pixel whose memory bytes are 0,1,2,3 must yield the documented
        // component for each supported format.
        let src = [0u8, 1, 2, 3];
        let expect = |format: ShmFormat, rgb: [u8; 3]| {
            let order = pixel_order(format).unwrap();
            let out = pixels_to_rgba(&src, 1, 1, 4, false, order);
            assert_eq!(&out[0..3], &rgb[..], "{format:?}");
            assert_eq!(out[3], 255);
        };
        // [31:0] A:R:G:B / x:R:G:B -> memory B, G, R, pad
        expect(ShmFormat::Argb8888, [2, 1, 0]);
        expect(ShmFormat::Xrgb8888, [2, 1, 0]);
        // [31:0] A:B:G:R / x:B:G:R -> memory R, G, B, pad
        expect(ShmFormat::Abgr8888, [0, 1, 2]);
        expect(ShmFormat::Xbgr8888, [0, 1, 2]);
        // [31:0] B:G:R:A / B:G:R:x -> memory A, R, G, B
        expect(ShmFormat::Bgra8888, [1, 2, 3]);
        expect(ShmFormat::Bgrx8888, [1, 2, 3]);
        // [31:0] R:G:B:A / R:G:B:x -> memory A, B, G, R
        expect(ShmFormat::Rgba8888, [3, 2, 1]);
        expect(ShmFormat::Rgbx8888, [3, 2, 1]);
    }

    #[test]
    fn keymap_is_text_xkb() {
        // wlroots parses the fd with XKB_KEYMAP_FORMAT_TEXT_V1, so the shipped
        // keymap must be text, not a compiled (`mkx` / 0x0f magic) blob.
        assert!(!KEYMAP.is_empty());
        assert_eq!(&KEYMAP[..10], b"xkb_keymap");
        assert!(KEYMAP.is_ascii());
    }

    #[test]
    fn spawn_reports_a_spawn_error_for_a_missing_compositor() {
        let dir = std::env::temp_dir().join(format!("uflinux-rds-missing-{}", std::process::id()));
        let spec = DesktopSpec::new("uflinux-rds-does-not-exist", 640, 480, dir.clone());
        let err = match DesktopBackend::spawn(spec) {
            Err(err) => err,
            Ok(_) => panic!("a missing binary must not spawn"),
        };
        match &err {
            BackendError::Spawn { command, .. } => {
                assert_eq!(command, "uflinux-rds-does-not-exist")
            }
            other => panic!("expected BackendError::Spawn, got {other:?}"),
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn spec_rejects_zero_dimensions() {
        let dir = std::env::temp_dir().join(format!("uflinux-rds-resize-{}", std::process::id()));
        assert!(
            DesktopSpec::new("labwc", 0, 480, dir.clone())
                .validate()
                .is_err()
        );
        assert!(
            DesktopSpec::new("labwc", 640, 480, dir.clone())
                .validate()
                .is_ok()
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// Spawns a real headless compositor (`labwc`, falling back to `cage`) and
    /// drives one frame plus some input. Run with:
    /// `cargo test -p uflinux-rds-wayland -- --ignored --nocapture`
    #[test]
    #[ignore = "spawns a real headless compositor; run with `cargo test -- --ignored`"]
    fn end_to_end_desktop_session() {
        let dir = std::env::temp_dir().join(format!("uflinux-rds-e2e-{}", std::process::id()));
        let command = if which("labwc") { "labwc" } else { "cage" };
        let spec = DesktopSpec::new(command, 1280, 720, dir.clone());
        let mut backend = DesktopBackend::spawn(spec).expect("spawn the compositor");
        assert_eq!(backend.size(), (1280, 720));
        assert!(backend.pid().is_some());

        let frame = backend
            .next_frame(Duration::from_secs(10))
            .expect("first frame")
            .expect("first frame is not a timeout");
        assert_eq!(frame.width, 1280);
        assert_eq!(frame.height, 720);
        assert_eq!(frame.pixels.len(), 1280 * 720 * 4);

        backend
            .inject_pointer(PointerEvent::Move { x: 100, y: 100 })
            .expect("pointer move");
        backend
            .inject_keyboard(KeyboardEvent {
                keysym: 0x20, // SPACE
                pressed: true,
            })
            .expect("key press");
        let _ = backend.next_frame(Duration::from_millis(500));
        backend.shutdown().expect("shutdown");
    }

    fn which(binary: &str) -> bool {
        std::env::var_os("PATH")
            .map(|paths| std::env::split_paths(&paths).any(|dir| dir.join(binary).is_file()))
            .unwrap_or(false)
    }
}
