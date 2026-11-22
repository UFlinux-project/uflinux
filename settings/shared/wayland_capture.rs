//! One-shot image capture through the compositor.
use std::{
    collections::HashMap,
    fs::File,
    os::fd::{AsFd, AsRawFd, FromRawFd},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};
use wayland_client::{
    Connection, Dispatch, EventQueue, Proxy, QueueHandle,
    protocol::{wl_buffer, wl_output, wl_registry, wl_shm, wl_shm_pool},
};
use wayland_protocols::ext::{
    foreign_toplevel_list::v1::client::{
        ext_foreign_toplevel_handle_v1 as handle, ext_foreign_toplevel_list_v1 as list,
    },
    image_capture_source::v1::client::{
        ext_foreign_toplevel_image_capture_source_manager_v1 as source_manager,
        ext_image_capture_source_v1 as source,
        ext_output_image_capture_source_manager_v1 as output_manager,
    },
    image_copy_capture::v1::client::{
        ext_image_copy_capture_frame_v1 as frame, ext_image_copy_capture_manager_v1 as manager,
        ext_image_copy_capture_session_v1 as session,
    },
};
#[derive(Clone, Default)]
pub struct Toplevel {
    proxy: Option<handle::ExtForeignToplevelHandleV1>,
    pub app: String,
    pub title: String,
    pub identifier: String,
    done: bool,
}
#[derive(Default)]
struct State {
    list: Option<list::ExtForeignToplevelListV1>,
    sources: Option<source_manager::ExtForeignToplevelImageCaptureSourceManagerV1>,
    output_sources: Option<output_manager::ExtOutputImageCaptureSourceManagerV1>,
    outputs: Vec<(wl_output::WlOutput, String)>,
    manager: Option<manager::ExtImageCopyCaptureManagerV1>,
    shm: Option<wl_shm::WlShm>,
    windows: HashMap<u32, Toplevel>,
    size: (u32, u32),
    format: Option<wl_shm::Format>,
    constraints: bool,
    complete: bool,
    failed: bool,
    transform: u32,
}
pub struct Client {
    connection: Connection,
    queue: EventQueue<State>,
    state: State,
}

impl Client {
    pub fn connect(cancel: &AtomicBool) -> Result<Self, String> {
        let connection = Connection::connect_to_env().map_err(|e| e.to_string())?;
        let queue = connection.new_event_queue::<State>();
        connection.display().get_registry(&queue.handle(), ());
        let mut client = Self {
            connection,
            queue,
            state: State::default(),
        };
        // Bind the registry globals, then read the output and toplevel metadata.
        client.refresh(cancel)?;
        client.refresh(cancel)?;
        Ok(client)
    }

    pub fn refresh(&mut self, cancel: &AtomicBool) -> Result<(), String> {
        sync(&self.connection, &mut self.queue, &mut self.state, cancel)
    }

    pub fn windows(&self) -> Result<impl Iterator<Item = &Toplevel>, String> {
        if self.state.list.is_none() || self.state.sources.is_none() || self.state.manager.is_none()
        {
            return Err("Window capture is unavailable on this compositor".into());
        }
        Ok(self.state.windows.values().filter(|window| window.done))
    }

    pub fn window_frame(
        &mut self,
        window: &Toplevel,
        cursor: bool,
        limit: Option<(u32, u32)>,
        cancel: &AtomicBool,
    ) -> Result<(i32, i32, Vec<u8>), String> {
        let sources = self
            .state
            .sources
            .as_ref()
            .ok_or("Window capture unavailable")?;
        let target = window
            .proxy
            .as_ref()
            .ok_or("The selected window was closed")?;
        let source = sources.create_source(target, &self.queue.handle(), ());
        self.frame(source, cursor, limit, cancel)
    }

    #[allow(dead_code)] // Output frames are used by the screenshot tool.
    pub fn output_frame(
        &mut self,
        name: &str,
        cursor: bool,
        cancel: &AtomicBool,
    ) -> Result<(i32, i32, Vec<u8>), String> {
        let sources = self
            .state
            .output_sources
            .as_ref()
            .ok_or("Screen capture unavailable")?;
        let (output, _) = self
            .state
            .outputs
            .iter()
            .find(|(_, connector)| connector == name)
            .ok_or("The selected screen is unavailable")?;
        let source = sources.create_source(output, &self.queue.handle(), ());
        self.frame(source, cursor, None, cancel)
    }

    fn frame(
        &mut self,
        source: source::ExtImageCaptureSourceV1,
        cursor: bool,
        limit: Option<(u32, u32)>,
        cancel: &AtomicBool,
    ) -> Result<(i32, i32, Vec<u8>), String> {
        let qh = self.queue.handle();
        let manager = self
            .state
            .manager
            .as_ref()
            .ok_or("Image capture unavailable")?;
        let shm = self.state.shm.clone().ok_or("Shared memory unavailable")?;
        self.state.size = (0, 0);
        self.state.format = None;
        self.state.constraints = false;
        self.state.failed = false;
        self.state.complete = false;
        self.state.transform = 0;
        let options = if cursor {
            manager::Options::PaintCursors
        } else {
            manager::Options::empty()
        };
        let session = manager.create_session(&source, options, &qh, ());
        let result = capture(
            &self.connection,
            &mut self.queue,
            &mut self.state,
            cancel,
            &session,
            &shm,
            limit,
        );
        session.destroy();
        source.destroy();
        result
    }
}

fn capture(
    connection: &Connection,
    queue: &mut wayland_client::EventQueue<State>,
    state: &mut State,
    cancel: &AtomicBool,
    session: &session::ExtImageCopyCaptureSessionV1,
    shm: &wl_shm::WlShm,
    limit: Option<(u32, u32)>,
) -> Result<(i32, i32, Vec<u8>), String> {
    wait(connection, queue, state, cancel, |s| {
        s.constraints || s.failed
    })?;
    let (width, height) = state.size;
    let len =
        buffer_len(width, height, limit.is_some()).ok_or("Source exceeds capture size limit")?;
    let format = state.format.ok_or("Unsupported capture format")?;
    if state.failed {
        return Err("Capture stopped".into());
    }
    let fd = unsafe { libc::memfd_create(c"uflinux-image-capture".as_ptr(), libc::MFD_CLOEXEC) };
    if fd < 0 {
        return Err("Cannot allocate capture buffer".into());
    }
    let file = unsafe { File::from_raw_fd(fd) };
    file.set_len(len as u64).map_err(|e| e.to_string())?;
    let qh = queue.handle();
    let pool = shm.create_pool(file.as_fd(), len as i32, &qh, ());
    let buffer = pool.create_buffer(
        0,
        width as i32,
        height as i32,
        width as i32 * 4,
        format,
        &qh,
        (),
    );
    pool.destroy();
    let frame = session.create_frame(&qh, ());
    frame.attach_buffer(&buffer);
    frame.damage_buffer(0, 0, width as i32, height as i32);
    frame.capture();
    let result = wait(connection, queue, state, cancel, |s| s.complete || s.failed);
    frame.destroy();
    buffer.destroy();
    result?;
    if state.failed {
        return Err("Capture failed".into());
    }
    // The compositor has finished writing. Map the source instead of making
    // another full-size copy while scaling it down.
    let mapped = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            len,
            libc::PROT_READ,
            libc::MAP_SHARED,
            file.as_raw_fd(),
            0,
        )
    };
    if mapped == libc::MAP_FAILED {
        return Err("Cannot map capture buffer".into());
    }
    let pixels = unsafe { std::slice::from_raw_parts(mapped.cast::<u8>(), len) };
    let image = rgba_pixels(pixels, width, height, format, state.transform, limit);
    unsafe {
        libc::munmap(mapped, len);
    }
    image.ok_or("Unsupported capture transform".into())
}
pub fn buffer_len(w: u32, h: u32, preview: bool) -> Option<usize> {
    let max_pixels = if preview { 4096 * 4096 } else { 8192 * 4320 };
    let pixels = u64::from(w) * u64::from(h);
    if w == 0 || h == 0 || pixels > max_pixels {
        return None;
    }
    usize::try_from(pixels.checked_mul(4)?).ok()
}
pub fn rgba_pixels(
    bytes: &[u8],
    w: u32,
    h: u32,
    format: wl_shm::Format,
    transform: u32,
    limit: Option<(u32, u32)>,
) -> Option<(i32, i32, Vec<u8>)> {
    if transform > 7 || bytes.len() != buffer_len(w, h, limit.is_some())? {
        return None;
    }
    let (tw, th) = if transform % 2 == 1 { (h, w) } else { (w, h) };
    let scale = limit.map_or(1.0, |(max_width, max_height)| {
        (max_width as f64 / tw as f64)
            .min(max_height as f64 / th as f64)
            .min(1.0)
    });
    let (ow, oh) = (
        (tw as f64 * scale).max(1.0) as u32,
        (th as f64 * scale).max(1.0) as u32,
    );
    let mut out = Vec::with_capacity((ow * oh * 4) as usize);
    for y in 0..oh {
        for x in 0..ow {
            let (tx, ty) = (x * tw / ow, y * th / oh);
            let (sx, sy) = match transform {
                0 => (tx, ty),
                1 => (w - 1 - ty, tx),
                2 => (w - 1 - tx, h - 1 - ty),
                3 => (ty, h - 1 - tx),
                4 => (w - 1 - tx, ty),
                5 => (ty, tx),
                6 => (tx, h - 1 - ty),
                _ => (w - 1 - ty, h - 1 - tx),
            };
            let i = ((sy as usize * w as usize) + sx as usize) * 4;
            let p = u32::from_ne_bytes(bytes[i..i + 4].try_into().ok()?);
            let bgr = matches!(format, wl_shm::Format::Xbgr8888 | wl_shm::Format::Abgr8888);
            let mut pixel = [
                (if bgr { p & 255 } else { (p >> 16) & 255 }) as u8,
                ((p >> 8) & 255) as u8,
                (if bgr { (p >> 16) & 255 } else { p & 255 }) as u8,
                if matches!(format, wl_shm::Format::Argb8888 | wl_shm::Format::Abgr8888) {
                    (p >> 24) as u8
                } else {
                    255
                },
            ];
            // wl_shm stores premultiplied alpha. Image encoders and GDK's
            // R8g8b8a8 texture format use straight alpha.
            let alpha = u32::from(pixel[3]);
            if alpha != 0 && alpha != 255 {
                for channel in &mut pixel[..3] {
                    *channel = ((u32::from(*channel) * 255 + alpha / 2) / alpha).min(255) as u8;
                }
            }
            out.extend_from_slice(&pixel);
        }
    }
    Some((ow as i32, oh as i32, out))
}
fn wait(
    connection: &Connection,
    queue: &mut wayland_client::EventQueue<State>,
    state: &mut State,
    cancel: &AtomicBool,
    ready: impl Fn(&State) -> bool,
) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_millis(750);
    loop {
        queue.dispatch_pending(state).map_err(|e| e.to_string())?;
        if cancel.load(Ordering::Relaxed) {
            return Err("Cancelled".into());
        }
        if ready(state) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err("Capture timed out".into());
        }
        connection.flush().map_err(|e| e.to_string())?;
        let Some(guard) = queue.prepare_read() else {
            continue;
        };
        let mut fd = libc::pollfd {
            fd: connection.backend().poll_fd().as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let n = unsafe { libc::poll(&mut fd, 1, 50) };
        if n < 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        if fd.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
            return Err("Wayland disconnected".into());
        }
        if fd.revents & libc::POLLIN != 0 {
            guard.read().map_err(|e| e.to_string())?;
        } else {
            drop(guard);
        }
    }
}
fn sync(
    connection: &Connection,
    queue: &mut wayland_client::EventQueue<State>,
    state: &mut State,
    cancel: &AtomicBool,
) -> Result<(), String> {
    state.complete = false;
    connection.display().sync(&queue.handle(), ());
    wait(connection, queue, state, cancel, |s| s.complete)
}
impl Dispatch<wayland_client::protocol::wl_callback::WlCallback, ()> for State {
    fn event(
        s: &mut Self,
        _: &wayland_client::protocol::wl_callback::WlCallback,
        _: wayland_client::protocol::wl_callback::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        s.complete = true;
    }
}
impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        s: &mut Self,
        r: &wl_registry::WlRegistry,
        e: wl_registry::Event,
        _: &(),
        _: &Connection,
        q: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = e
        {
            match interface.as_str() {
                "ext_foreign_toplevel_list_v1" => s.list = Some(r.bind(name, 1, q, ())),
                "ext_foreign_toplevel_image_capture_source_manager_v1" => {
                    s.sources = Some(r.bind(name, 1, q, ()))
                }
                "ext_image_copy_capture_manager_v1" => s.manager = Some(r.bind(name, 1, q, ())),
                "ext_output_image_capture_source_manager_v1" => {
                    s.output_sources = Some(r.bind(name, 1, q, ()));
                }
                "wl_output" if version >= 4 => {
                    s.outputs.push((r.bind(name, 4, q, ()), String::new()))
                }
                "wl_shm" => s.shm = Some(r.bind(name, 1, q, ())),
                _ => {}
            }
        }
    }
}
impl Dispatch<wl_output::WlOutput, ()> for State {
    fn event(
        s: &mut Self,
        output: &wl_output::WlOutput,
        event: wl_output::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Name { name } = event {
            if let Some((_, connector)) = s.outputs.iter_mut().find(|(proxy, _)| proxy == output) {
                *connector = name;
            }
        }
    }
}

impl Dispatch<list::ExtForeignToplevelListV1, ()> for State {
    fn event(
        s: &mut Self,
        _: &list::ExtForeignToplevelListV1,
        e: list::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let list::Event::Toplevel { toplevel } = e {
            s.windows.insert(
                toplevel.id().protocol_id(),
                Toplevel {
                    proxy: Some(toplevel),
                    ..Default::default()
                },
            );
        }
    }
    wayland_client::event_created_child!(State,list::ExtForeignToplevelListV1,[list::EVT_TOPLEVEL_OPCODE=>(handle::ExtForeignToplevelHandleV1,())]);
}
impl Dispatch<handle::ExtForeignToplevelHandleV1, ()> for State {
    fn event(
        s: &mut Self,
        p: &handle::ExtForeignToplevelHandleV1,
        e: handle::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let id = p.id().protocol_id();
        if let handle::Event::Closed = e {
            s.windows.remove(&id);
            p.destroy();
            return;
        }
        if let Some(w) = s.windows.get_mut(&id) {
            match e {
                handle::Event::Title { title } => {
                    w.title = title;
                    w.done = false;
                }
                handle::Event::AppId { app_id } => {
                    w.app = app_id;
                    w.done = false;
                }
                handle::Event::Identifier { identifier } => w.identifier = identifier,
                handle::Event::Done => w.done = true,
                _ => {}
            }
        }
    }
}
impl Dispatch<session::ExtImageCopyCaptureSessionV1, ()> for State {
    fn event(
        s: &mut Self,
        _: &session::ExtImageCopyCaptureSessionV1,
        e: session::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match e {
            session::Event::BufferSize { width, height } => s.size = (width, height),
            session::Event::ShmFormat { format } => {
                if let wayland_client::WEnum::Value(f) = format {
                    if matches!(
                        f,
                        wl_shm::Format::Xrgb8888
                            | wl_shm::Format::Argb8888
                            | wl_shm::Format::Xbgr8888
                            | wl_shm::Format::Abgr8888
                    ) {
                        s.format = Some(f);
                    }
                }
            }
            session::Event::Done => s.constraints = true,
            session::Event::Stopped => s.failed = true,
            _ => {}
        }
    }
}
impl Dispatch<frame::ExtImageCopyCaptureFrameV1, ()> for State {
    fn event(
        s: &mut Self,
        _: &frame::ExtImageCopyCaptureFrameV1,
        e: frame::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match e {
            frame::Event::Ready => s.complete = true,
            frame::Event::Failed { .. } => s.failed = true,
            frame::Event::Transform { transform } => s.transform = transform.into(),
            _ => {}
        }
    }
}
macro_rules! noop {($($ty:ty),*)=>{$(wayland_client::delegate_noop!(State:ignore $ty);)*};}
noop!(
    wl_shm::WlShm,
    wl_shm_pool::WlShmPool,
    wl_buffer::WlBuffer,
    source_manager::ExtForeignToplevelImageCaptureSourceManagerV1,
    source::ExtImageCaptureSourceV1,
    manager::ExtImageCopyCaptureManagerV1,
    output_manager::ExtOutputImageCaptureSourceManagerV1
);
