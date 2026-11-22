//! On-demand window capture. No Wayland connection or timer survives the popup.
use crate::wayland_capture::Client;
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use uflinux_desktop::taskbar::{Window, WindowId};
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
    let mut client = Client::connect(&cancel)?;
    let mut index = 0;
    while !cancel.load(Ordering::Relaxed) {
        let started = Instant::now();
        client.refresh(&cancel)?;
        let window = &windows[index % windows.len()];
        index += 1;
        let unique = windows
            .iter()
            .filter(|w| w.app_id == window.app_id && w.title == window.title)
            .count()
            == 1;
        let mut candidates = client
            .windows()?
            .filter(|w| w.app == window.app_id && w.title == window.title);
        let target = candidates.next().cloned();
        let unambiguous = candidates.next().is_none();
        drop(candidates);
        if let Some(target) = target.filter(|_| unique && unambiguous) {
            if let Ok((width, height, pixels)) =
                client.window_frame(&target, false, Some((320, 180)), &cancel)
            {
                // Metadata may have changed while capture was pending.
                let mut matching = client
                    .windows()?
                    .filter(|w| w.app == window.app_id && w.title == window.title);
                let valid = matching
                    .next()
                    .is_some_and(|w| w.identifier == target.identifier)
                    && matching.next().is_none();
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
#[cfg(test)]
mod tests {
    use super::*;
    use crate::wayland_capture::{buffer_len, rgba_pixels};
    use wayland_client::protocol::wl_shm;

    fn thumbnail(
        bytes: &[u8],
        width: u32,
        height: u32,
        format: wl_shm::Format,
        transform: u32,
    ) -> Option<(i32, i32, Vec<u8>)> {
        rgba_pixels(bytes, width, height, format, transform, Some((320, 180)))
    }
    #[test]
    fn rejects_unbounded_source_allocations() {
        assert_eq!(buffer_len(0, 12, true), None);
        assert_eq!(buffer_len(u32::MAX, u32::MAX, true), None);
        assert_eq!(buffer_len(4096, 4096, true), Some(64 * 1024 * 1024));
        assert_eq!(buffer_len(4097, 4096, true), None);
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
