//! Save compositor frames in the configured screenshot format.
use std::path::Path;
use std::sync::atomic::AtomicBool;

use settings_core::CaptureFormat;

use crate::backend::{Outcome, Target};
use crate::wayland_capture::Client;

pub struct Window {
    pub identifier: String,
    pub label: String,
}

pub fn windows() -> Result<Vec<Window>, String> {
    let client = Client::connect(&AtomicBool::new(false))?;
    let mut windows: Vec<_> = client
        .windows()?
        .filter(|window| !window.identifier.is_empty())
        .map(|window| Window {
            identifier: window.identifier.clone(),
            label: match (window.title.is_empty(), window.app.is_empty()) {
                (false, false) => format!("{} — {}", window.title, window.app),
                (false, true) => window.title.clone(),
                (true, false) => window.app.clone(),
                (true, true) => "Untitled window".into(),
            },
        })
        .collect();
    windows.sort_by(|a, b| a.label.cmp(&b.label).then(a.identifier.cmp(&b.identifier)));
    Ok(windows)
}

pub fn capture(
    target: &Target,
    path: &Path,
    format: CaptureFormat,
    include_cursor: bool,
) -> Outcome {
    match capture_image(target, include_cursor).and_then(|image| save(&image, path, format)) {
        Ok(()) => Outcome::Saved(path.to_owned()),
        Err(error) => Outcome::Failed(error),
    }
}

fn capture_image(target: &Target, cursor: bool) -> Result<gdk_pixbuf::Pixbuf, String> {
    let cancel = AtomicBool::new(false);
    let mut client = Client::connect(&cancel)?;
    let (width, height, pixels) = match target {
        Target::Window(identifier) => {
            let window = client
                .windows()?
                .find(|window| window.identifier == *identifier)
                .cloned()
                .ok_or("The selected window was closed")?;
            client.window_frame(&window, cursor, None, &cancel)?
        }
        Target::Screen(monitor) | Target::Area(monitor, _) => {
            client.output_frame(monitor, cursor, &cancel)?
        }
    };
    let image = gdk_pixbuf::Pixbuf::from_bytes(
        &glib::Bytes::from_owned(pixels),
        gdk_pixbuf::Colorspace::Rgb,
        true,
        8,
        width,
        height,
        width * 4,
    );
    if let Target::Area(_, region) = target {
        let x = |logical| scale_edge(logical, width, region.output.width());
        let y = |logical| scale_edge(logical, height, region.output.height());
        let left = x(region.x);
        let top = y(region.y);
        let crop_width = x(region.x + region.width) - left;
        let crop_height = y(region.y + region.height) - top;
        if crop_width <= 0 || crop_height <= 0 {
            return Err("The selected area is empty".into());
        }
        Ok(image.new_subpixbuf(left, top, crop_width, crop_height))
    } else {
        Ok(image)
    }
}

fn scale_edge(edge: i32, pixels: i32, logical: i32) -> i32 {
    ((f64::from(edge) * f64::from(pixels) / f64::from(logical)).round() as i32).clamp(0, pixels)
}

fn save(image: &gdk_pixbuf::Pixbuf, path: &Path, format: CaptureFormat) -> Result<(), String> {
    let bytes = match format {
        CaptureFormat::Png => image
            .save_to_bufferv("png", &[])
            .map_err(|error| error.to_string())?,
        CaptureFormat::Jpeg => image
            .save_to_bufferv("jpeg", &[("quality", "95")])
            .map_err(|error| error.to_string())?,
        CaptureFormat::Webp => {
            let mut output = std::ptr::null_mut();
            let pixels = image.read_pixel_bytes();
            // The pixbuf owns valid RGBA rows for the duration of the call.
            let len = unsafe {
                WebPEncodeLosslessRGBA(
                    pixels.as_ref().as_ptr(),
                    image.width(),
                    image.height(),
                    image.rowstride(),
                    &mut output,
                )
            };
            if len == 0 {
                return Err("Could not encode WebP screenshot".into());
            }
            let bytes = unsafe { std::slice::from_raw_parts(output, len) }.to_vec();
            unsafe { WebPFree(output.cast()) };
            bytes
        }
    };
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| error.to_string())?;
    if let Err(error) = std::io::Write::write_all(&mut file, &bytes) {
        drop(file);
        let _ = std::fs::remove_file(path);
        return Err(error.to_string());
    }
    Ok(())
}

#[link(name = "webp")]
unsafe extern "C" {
    fn WebPEncodeLosslessRGBA(
        rgba: *const u8,
        width: i32,
        height: i32,
        stride: i32,
        output: *mut *mut u8,
    ) -> usize;
    fn WebPFree(pointer: *mut libc::c_void);
}
