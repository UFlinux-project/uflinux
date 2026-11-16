//! On-demand window capture. No Wayland connection or timer survives the popup.
use std::{
    collections::HashMap,
    fs::File,
    os::fd::{AsFd, AsRawFd, FromRawFd},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use uflinux_desktop::taskbar::{Window, WindowId};
use wayland_client::{
    Connection, Dispatch, Proxy, QueueHandle,
    protocol::{wl_buffer, wl_registry, wl_shm, wl_shm_pool},
};
use wayland_protocols::ext::{
    foreign_toplevel_list::v1::client::{
        ext_foreign_toplevel_handle_v1 as handle, ext_foreign_toplevel_list_v1 as list,
    },
    image_capture_source::v1::client::{
        ext_foreign_toplevel_image_capture_source_manager_v1 as source_manager,
        ext_image_capture_source_v1 as source,
    },
    image_copy_capture::v1::client::{
        ext_image_copy_capture_frame_v1 as frame, ext_image_copy_capture_manager_v1 as manager,
        ext_image_copy_capture_session_v1 as session,
    },
};
#[derive(Clone)]
pub struct Image {
    pub id: WindowId,
    pub app_id: String,
    pub title: String,
    pub width: i32,
    pub height: i32,
    pub pixels: Vec<u8>,
}
pub struct Request(Arc<AtomicBool>);
impl Drop for Request {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}
pub fn start(windows: Vec<Window>) -> (Request, async_channel::Receiver<Image>) {
    let cancel = Arc::new(AtomicBool::new(false));
    let (tx, rx) = async_channel::bounded(4);
    let flag = cancel.clone();
    let _ = std::thread::Builder::new()
        .name("window-previews".into())
        .spawn(move || {
            let _ = run(windows, flag, tx);
        });
    (Request(cancel), rx)
}
#[derive(Default)]
struct Toplevel {
    proxy: Option<handle::ExtForeignToplevelHandleV1>,
    app: String,
    title: String,
    done: bool,
}
#[derive(Default)]
struct State {
    list: Option<list::ExtForeignToplevelListV1>,
    sources: Option<source_manager::ExtForeignToplevelImageCaptureSourceManagerV1>,
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
fn run(
    windows: Vec<Window>,
    cancel: Arc<AtomicBool>,
    tx: async_channel::Sender<Image>,
) -> Result<(), String> {
    if windows.is_empty() {
        return Ok(());
    }
    // Serialize workers across outputs and popup replacement. Cancellation is
    // asynchronous, so a previous worker may still be releasing its frame.
    static CAPTURE: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _exclusive = loop {
        if cancel.load(Ordering::Relaxed) {
            return Ok(());
        }
        match CAPTURE.try_lock() {
            Ok(guard) => break guard,
            Err(std::sync::TryLockError::WouldBlock) => {
                std::thread::sleep(Duration::from_millis(20))
            }
            Err(_) => return Err("Capture worker stopped".into()),
        }
    };
    let connection = Connection::connect_to_env().map_err(|e| e.to_string())?;
    let mut queue = connection.new_event_queue::<State>();
    let qh = queue.handle();
    let _registry = connection.display().get_registry(&qh, ());
    let mut state = State::default();
    // A sync callback makes enumeration bounded even on an unresponsive server.
    sync(&connection, &mut queue, &mut state, &cancel)?;
    sync(&connection, &mut queue, &mut state, &cancel)?;
    let sources = state.sources.clone().ok_or("Window capture unavailable")?;
    let manager = state.manager.clone().ok_or("Window capture unavailable")?;
    let shm = state.shm.clone().ok_or("Shared memory unavailable")?;
    let mut index = 0;
    while !cancel.load(Ordering::Relaxed) {
        let started = Instant::now();
        sync(&connection, &mut queue, &mut state, &cancel)?;
        queue
            .dispatch_pending(&mut state)
            .map_err(|e| e.to_string())?;
        let window = &windows[index % windows.len()];
        index += 1;
        let unique = windows
            .iter()
            .filter(|w| w.app_id == window.app_id && w.title == window.title)
            .count()
            == 1;
        let candidates: Vec<_> = state
            .windows
            .values()
            .filter(|w| w.done && w.app == window.app_id && w.title == window.title)
            .collect();
        if unique && candidates.len() == 1 {
            let target = candidates[0].proxy.clone().unwrap();
            state.size = (0, 0);
            state.format = None;
            state.constraints = false;
            state.failed = false;
            state.complete = false;
            state.transform = 0;
            let source = sources.create_source(&target, &qh, ());
            let session = manager.create_session(&source, manager::Options::empty(), &qh, ());
            let result = capture(&connection, &mut queue, &mut state, &cancel, &session, &shm);
            session.destroy();
            source.destroy();
            if let Ok((width, height, pixels)) = result {
                // Metadata may have changed while capture was pending.
                let valid = state
                    .windows
                    .get(&target.id().protocol_id())
                    .is_some_and(|w| w.done && w.app == window.app_id && w.title == window.title)
                    && state
                        .windows
                        .values()
                        .filter(|w| w.done && w.app == window.app_id && w.title == window.title)
                        .count()
                        == 1;
                if valid && !cancel.load(Ordering::Relaxed) {
                    let _ = tx.try_send(Image {
                        id: window.id,
                        app_id: window.app_id.clone(),
                        title: window.title.clone(),
                        width,
                        height,
                        pixels,
                    });
                }
            }
        }
        // <= 4 captures/second globally, <= 2 per window. Sleep in cancellable chunks.
        let interval = Duration::from_millis(if windows.len() == 1 { 500 } else { 250 });
        while started.elapsed() < interval && !cancel.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    Ok(())
}
fn capture(
    connection: &Connection,
    queue: &mut wayland_client::EventQueue<State>,
    state: &mut State,
    cancel: &AtomicBool,
    session: &session::ExtImageCopyCaptureSessionV1,
    shm: &wl_shm::WlShm,
) -> Result<(i32, i32, Vec<u8>), String> {
    wait(connection, queue, state, cancel, |s| {
        s.constraints || s.failed
    })?;
    let (width, height) = state.size;
    let len = buffer_len(width, height).ok_or("Source exceeds preview limit")?;
    let format = state.format.ok_or("Unsupported capture format")?;
    if state.failed {
        return Err("Capture stopped".into());
    }
    let fd = unsafe { libc::memfd_create(c"uflinux-preview".as_ptr(), libc::MFD_CLOEXEC) };
    if fd < 0 {
        return Err("Cannot allocate preview".into());
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
        return Err("Cannot map preview buffer".into());
    }
    let pixels = unsafe { std::slice::from_raw_parts(mapped.cast::<u8>(), len) };
    let image = thumbnail(pixels, width, height, format, state.transform);
    unsafe {
        libc::munmap(mapped, len);
    }
    image.ok_or("Unsupported capture transform".into())
}
fn buffer_len(w: u32, h: u32) -> Option<usize> {
    let pixels = (w as u64).checked_mul(h as u64)?;
    if w == 0 || h == 0 || pixels > 16_777_216 {
        return None;
    }
    usize::try_from(pixels.checked_mul(4)?).ok()
}
fn thumbnail(
    bytes: &[u8],
    w: u32,
    h: u32,
    format: wl_shm::Format,
    transform: u32,
) -> Option<(i32, i32, Vec<u8>)> {
    if transform > 7 || bytes.len() != buffer_len(w, h)? {
        return None;
    }
    let (tw, th) = if transform % 2 == 1 { (h, w) } else { (w, h) };
    let scale = (320.0 / tw as f64).min(180.0 / th as f64).min(1.0);
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
            out.extend_from_slice(&[
                (if bgr { p & 255 } else { (p >> 16) & 255 }) as u8,
                ((p >> 8) & 255) as u8,
                (if bgr { (p >> 16) & 255 } else { p & 255 }) as u8,
                if matches!(format, wl_shm::Format::Argb8888 | wl_shm::Format::Abgr8888) {
                    (p >> 24) as u8
                } else {
                    255
                },
            ]);
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
            name, interface, ..
        } = e
        {
            match interface.as_str() {
                "ext_foreign_toplevel_list_v1" => s.list = Some(r.bind(name, 1, q, ())),
                "ext_foreign_toplevel_image_capture_source_manager_v1" => {
                    s.sources = Some(r.bind(name, 1, q, ()))
                }
                "ext_image_copy_capture_manager_v1" => s.manager = Some(r.bind(name, 1, q, ())),
                "wl_shm" => s.shm = Some(r.bind(name, 1, q, ())),
                _ => {}
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
    manager::ExtImageCopyCaptureManagerV1
);

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_unbounded_source_allocations() {
        assert_eq!(buffer_len(0, 12), None);
        assert_eq!(buffer_len(u32::MAX, u32::MAX), None);
        assert_eq!(buffer_len(4096, 4096), Some(64 * 1024 * 1024));
        assert_eq!(buffer_len(4097, 4096), None);
    }
    #[test]
    fn scales_and_transforms_pixel_content() {
        let pixels = [0xff112233u32, 0xff445566, 0xff778899, 0xffaabbcc];
        let bytes: Vec<_> = pixels.into_iter().flat_map(u32::to_ne_bytes).collect();
        let (_, _, normal) = thumbnail(&bytes, 2, 2, wl_shm::Format::Xrgb8888, 0).unwrap();
        assert_eq!(&normal[..4], &[0x11, 0x22, 0x33, 255]);
        let (_, _, bgr) = thumbnail(&bytes, 2, 2, wl_shm::Format::Xbgr8888, 0).unwrap();
        assert_eq!(&bgr[..4], &[0x33, 0x22, 0x11, 255]);
        let (_, _, rotated) = thumbnail(&bytes, 2, 2, wl_shm::Format::Argb8888, 2).unwrap();
        assert_eq!(&rotated[..4], &[0xaa, 0xbb, 0xcc, 255]);
        for transform in 0..8 {
            assert!(thumbnail(&bytes, 2, 2, wl_shm::Format::Xrgb8888, transform).is_some());
        }
        assert!(thumbnail(&bytes, 2, 2, wl_shm::Format::Xrgb8888, 8).is_none());
    }
    #[test]
    fn rectangular_transform_orientation() {
        let source = (1u32..=6).flat_map(u32::to_ne_bytes).collect::<Vec<_>>();
        let expected = [
            [1, 2, 3, 4, 5, 6],
            [3, 6, 2, 5, 1, 4],
            [6, 5, 4, 3, 2, 1],
            [4, 1, 5, 2, 6, 3],
            [3, 2, 1, 6, 5, 4],
            [1, 4, 2, 5, 3, 6],
            [4, 5, 6, 1, 2, 3],
            [6, 3, 5, 2, 4, 1],
        ];
        for (transform, expected) in expected.into_iter().enumerate() {
            let (w, h, out) =
                thumbnail(&source, 3, 2, wl_shm::Format::Xrgb8888, transform as u32).unwrap();
            assert_eq!((w, h), if transform % 2 == 0 { (3, 2) } else { (2, 3) });
            assert_eq!(
                out.chunks_exact(4)
                    .map(|pixel| pixel[2])
                    .collect::<Vec<_>>(),
                expected
            );
        }
    }
    #[test]
    #[ignore = "requires a disposable labwc session and foot"]
    fn captures_real_window_and_cancels() {
        let mut child = std::process::Command::new("foot")
            .args([
                "--app-id=uflinux-preview-test",
                "--title=Preview fixture",
                "-o",
                "colors-dark.background=123456",
                "sh",
                "-c",
                "sleep 20",
            ])
            .spawn()
            .unwrap();
        let (client, _) = uflinux_desktop::toplevel::Client::start().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let window = loop {
            if let Some(w) = client
                .snapshot()
                .windows
                .into_iter()
                .find(|w| w.app_id == "uflinux-preview-test")
            {
                break w;
            }
            assert!(Instant::now() < deadline, "fixture not tracked");
            std::thread::sleep(Duration::from_millis(20));
        };
        let (request, rx) = start(vec![window.clone()]);
        let deadline = Instant::now() + Duration::from_secs(5);
        let image = loop {
            if let Ok(image) = rx.try_recv() {
                break image;
            }
            assert!(Instant::now() < deadline, "no captured thumbnail");
            std::thread::sleep(Duration::from_millis(20));
        };
        assert!(image.width <= 320 && image.height <= 180);
        assert!(
            image
                .pixels
                .chunks_exact(4)
                .filter(|p| p[..3] == [0x12, 0x34, 0x56])
                .count()
                > image.pixels.len() / 8
        );
        let sample = Instant::now();
        let mut frames = 0;
        while sample.elapsed() < Duration::from_secs(2) {
            while rx.try_recv().is_ok() {
                frames += 1;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(frames <= 4, "single-window refresh exceeded 2 fps");
        drop(request);
        let deadline = Instant::now() + Duration::from_secs(2);
        while !rx.is_closed() {
            assert!(
                Instant::now() < deadline,
                "capture worker survived cancellation"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        let (duplicates, rx) = start(vec![
            window.clone(),
            Window {
                id: window.id + 100,
                ..window
            },
        ]);
        std::thread::sleep(Duration::from_millis(1100));
        assert!(rx.try_recv().is_err(), "ambiguous window was captured");
        drop(duplicates);
        let _ = child.kill();
        let _ = child.wait();
    }
}
