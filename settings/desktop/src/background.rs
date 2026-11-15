//! Wallpaper drawn by the shell itself. The service still validates and stores
//! the appearance, but the pixels belong to this process, so a wallpaper no
//! longer costs the session a second compositor client and its own heap.
use gtk::{gdk::prelude::GdkCairoContextExt, prelude::*};
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use settings_core::Appearance;
use std::{cell::RefCell, rc::Rc};

/// Longest edge a wallpaper is decoded to. The service already refuses images
/// above sixteen megapixels; decoding to the size the output actually has,
/// rather than the size the file has, keeps the surface proportional to the
/// monitor instead of to whatever image the user picked.
const MAX_EDGE: i32 = 4096;
/// Colour painted before the image, and left showing through the letterbox of
/// a fitted one. Used on its own when the appearance names no wallpaper.
const FALLBACK: (f64, f64, f64) = (0x10 as f64 / 255., 0x2c as f64 / 255., 0x46 as f64 / 255.);

/// One output's wallpaper.
struct Surface {
    /// The output's name, which decides the entry this surface shows. A
    /// monitor re-created by a hotplug announces itself before its name
    /// arrives, so this starts empty and is filled in when it does.
    output: RefCell<String>,
    monitor: gtk::gdk::Monitor,
    window: gtk::ApplicationWindow,
    canvas: gtk::DrawingArea,
    color: RefCell<String>,
    /// The wallpaper entry currently painted on this output, if any.
    entry: RefCell<Option<(String, String)>>,
    /// The decoded image behind that entry, keyed by everything that decides
    /// its size, so a new file, fit mode or resolution re-decodes.
    image: RefCell<Option<(String, gdk_pixbuf::Pixbuf)>>,
    /// The file that last failed to decode, reported through the status action.
    failed: RefCell<Option<String>>,
}

pub struct Background {
    application: gtk::Application,
    surfaces: RefCell<Vec<Rc<Surface>>>,
    appearance: RefCell<Appearance>,
    /// Run after a surface is created or destroyed, so the shell re-reports
    /// what is on screen. A monitor being re-created replaces its surface
    /// without a new appearance ever arriving, and whatever reads the reported
    /// state would otherwise keep the one taken while the surface was gone.
    on_change: RefCell<Option<Rc<dyn Fn()>>>,
}

impl Background {
    pub fn new(application: &gtk::Application) -> Rc<Self> {
        let background = Rc::new(Self {
            application: application.clone(),
            surfaces: RefCell::new(Vec::new()),
            appearance: RefCell::new(Appearance::default()),
            on_change: RefCell::new(None),
        });
        if let Some(display) = gtk::gdk::Display::default() {
            let monitors = display.monitors();
            let weak = Rc::downgrade(&background);
            monitors.connect_items_changed(move |monitors, _, _, _| {
                if let Some(background) = weak.upgrade() {
                    background.sync(monitors);
                }
            });
            background.sync(&monitors);
        }
        background
    }

    /// Paints the appearance the service published. Called once the service
    /// state has arrived and again whenever `Changed` carries a different one.
    pub fn set(self: &Rc<Self>, appearance: Appearance) {
        *self.appearance.borrow_mut() = appearance;
        let appearance = self.appearance.borrow();
        for surface in self.surfaces.borrow().iter() {
            surface.retarget(&appearance);
        }
    }

    /// Installs the callback run once the surfaces no longer match the monitors.
    pub fn set_on_change(&self, on_change: Rc<dyn Fn()>) {
        *self.on_change.borrow_mut() = Some(on_change);
    }

    /// Points one surface back at the current appearance and lets the shell
    /// re-report what is on screen, for when its output's name arrives only
    /// after the surface was made.
    fn refresh(self: &Rc<Self>, surface: &Rc<Surface>) {
        surface.retarget(&self.appearance.borrow());
        let on_change = self.on_change.borrow().clone();
        if let Some(on_change) = on_change {
            on_change();
        }
    }

    /// What is on screen, so a test can wait for a wallpaper instead of sleeping
    /// and so a broken image is visible without staring at the monitor.
    pub fn status(&self) -> serde_json::Value {
        let surfaces = self.surfaces.borrow();
        serde_json::json!({
            "outputs": surfaces
                .iter()
                .map(|surface| {
                    let entry = surface.entry.borrow().clone();
                    serde_json::json!({
                        "output": surface.output.borrow().clone(),
                        "file": entry.as_ref().map(|entry| entry.0.clone()),
                        "fit": entry.as_ref().map(|entry| entry.1.clone()),
                    })
                })
                .collect::<Vec<_>>(),
            "failed": surfaces
                .iter()
                .filter_map(|surface| surface.failed.borrow().clone())
                .collect::<Vec<_>>(),
        })
    }

    fn sync(self: &Rc<Self>, monitors: &gio::ListModel) {
        let current: Vec<_> = (0..monitors.n_items())
            .filter_map(|index| monitors.item(index)?.downcast::<gtk::gdk::Monitor>().ok())
            .collect();
        let mut changed = false;
        self.surfaces.borrow_mut().retain(|surface| {
            let keep = current.contains(&surface.monitor);
            if !keep {
                surface.window.destroy();
                changed = true;
            }
            keep
        });
        for monitor in current {
            let known = self
                .surfaces
                .borrow()
                .iter()
                .any(|surface| surface.monitor == monitor);
            if known {
                continue;
            }
            if let Some(surface) = self.create(&monitor) {
                surface.retarget(&self.appearance.borrow());
                // A monitor that has just re-appeared often has no name yet,
                // because the compositor announces it before sending the
                // output's details. Watch for the name so the surface can
                // match a per-output entry and report what it really shows.
                let weak = Rc::downgrade(self);
                let weak_surface = Rc::downgrade(&surface);
                monitor.connect_notify_local(Some("connector"), move |monitor, _| {
                    let (Some(background), Some(surface)) =
                        (weak.upgrade(), weak_surface.upgrade())
                    else {
                        return;
                    };
                    let Some(name) = monitor.connector() else {
                        return;
                    };
                    let name = name.to_string();
                    if name.is_empty() || *surface.output.borrow() == name {
                        return;
                    }
                    *surface.output.borrow_mut() = name;
                    background.refresh(&surface);
                });
                self.surfaces.borrow_mut().push(surface);
                changed = true;
            }
        }
        // A surface that replaced the one lost with its monitor is only visible
        // to the rest of the shell once the reported state is taken again.
        if changed {
            let on_change = self.on_change.borrow().clone();
            if let Some(on_change) = on_change {
                on_change();
            }
        }
    }

    fn create(self: &Rc<Self>, monitor: &gtk::gdk::Monitor) -> Option<Rc<Surface>> {
        let window = gtk::ApplicationWindow::builder()
            .application(&self.application)
            .title("UFLinux wallpaper")
            .decorated(false)
            .build();
        if gtk4_layer_shell::is_supported() {
            window.init_layer_shell();
            window.set_namespace(Some("uflinux-desktop-wallpaper"));
            window.set_layer(Layer::Background);
            window.set_monitor(Some(monitor));
            window.set_anchor(Edge::Left, true);
            window.set_anchor(Edge::Right, true);
            window.set_anchor(Edge::Top, true);
            window.set_anchor(Edge::Bottom, true);
            // The wallpaper covers the whole output, so it reserves nothing and
            // never takes focus; the panel keeps the exclusive zone.
            window.set_exclusive_zone(0);
            window.set_keyboard_mode(KeyboardMode::None);
        }
        let canvas = gtk::DrawingArea::new();
        window.set_child(Some(&canvas));
        let surface = Rc::new(Surface {
            output: RefCell::new(
                monitor
                    .connector()
                    .map(|name| name.to_string())
                    .unwrap_or_default(),
            ),
            monitor: monitor.clone(),
            window,
            canvas,
            color: RefCell::new(String::new()),
            entry: RefCell::new(None),
            image: RefCell::new(None),
            failed: RefCell::new(None),
        });
        let weak = Rc::downgrade(&surface);
        surface.canvas.set_draw_func(move |_, cr, width, height| {
            if let Some(surface) = weak.upgrade() {
                surface.paint(cr, width, height);
            }
        });
        surface.window.present();
        Some(surface)
    }
}

impl Surface {
    /// Points this output at whatever the appearance now says it should show,
    /// dropping the decoded image so the next draw decodes the new one.
    fn retarget(&self, appearance: &Appearance) {
        let output = self.output.borrow();
        let entry = resolve(appearance, &output);
        let unchanged = entry == *self.entry.borrow() && *self.color.borrow() == appearance.color;
        *self.entry.borrow_mut() = entry;
        *self.color.borrow_mut() = appearance.color.clone();
        self.failed.borrow_mut().take();
        if !unchanged {
            self.image.borrow_mut().take();
        }
        self.canvas.queue_draw();
    }

    fn paint(&self, cr: &gtk::cairo::Context, width: i32, height: i32) {
        let (red, green, blue) = parse_color(&self.color.borrow());
        cr.set_source_rgb(red, green, blue);
        let _ = cr.paint();
        let Some((file, fit)) = self.entry.borrow().clone() else {
            self.image.borrow_mut().take();
            return;
        };
        let Some(pixbuf) = self.decoded(&file, &fit, width, height) else {
            return;
        };
        // Every mode but stretch keeps the image's own aspect ratio and is
        // therefore centred; fill overflows the output and the surface clips it
        // away, fit leaves the colour showing as a letterbox. A tiled image is
        // anchored at the corner instead, so its grid does not shift with the
        // size of the output.
        let (x, y) = if fit == "tile" {
            (0., 0.)
        } else {
            (
                f64::from(width - pixbuf.width()) / 2.,
                f64::from(height - pixbuf.height()) / 2.,
            )
        };
        cr.set_source_pixbuf(&pixbuf, x, y);
        let pattern = cr.source();
        pattern.set_filter(if fit == "center" || fit == "tile" {
            gtk::cairo::Filter::Fast
        } else {
            gtk::cairo::Filter::Bilinear
        });
        pattern.set_extend(if fit == "tile" {
            gtk::cairo::Extend::Repeat
        } else {
            gtk::cairo::Extend::None
        });
        cr.rectangle(0., 0., f64::from(width), f64::from(height));
        cr.clip();
        let _ = cr.paint();
    }

    /// Decodes the wallpaper for this output's current size, at most once per
    /// file, fit mode and geometry.
    fn decoded(
        &self,
        file: &str,
        fit: &str,
        width: i32,
        height: i32,
    ) -> Option<gdk_pixbuf::Pixbuf> {
        let key = format!("{file}\u{0}{fit}\u{0}{width}x{height}");
        if let Some((cached, pixbuf)) = self.image.borrow().as_ref() {
            if *cached == key {
                return Some(pixbuf.clone());
            }
        }
        let Some((w, h)) = decode_target(file, fit, width, height) else {
            *self.failed.borrow_mut() = Some(file.to_string());
            self.image.borrow_mut().take();
            return None;
        };
        match gdk_pixbuf::Pixbuf::from_file_at_scale(file, w, h, false) {
            Ok(pixbuf) => {
                *self.image.borrow_mut() = Some((key, pixbuf.clone()));
                self.failed.borrow_mut().take();
                Some(pixbuf)
            }
            Err(error) => {
                eprintln!("Could not draw the wallpaper {file}: {error}");
                *self.failed.borrow_mut() = Some(file.to_string());
                self.image.borrow_mut().take();
                None
            }
        }
    }
}

/// The wallpaper an output shows: the last entry naming it exactly, or the last
/// entry for every output. The Settings page keeps the wildcard entries first,
/// so a per-output choice overrides the one for all displays.
fn resolve(appearance: &Appearance, output: &str) -> Option<(String, String)> {
    let mut wildcard = None;
    let mut exact = None;
    for wallpaper in &appearance.wallpapers {
        let entry = (wallpaper.file.clone(), wallpaper.fit.clone());
        if wallpaper.output == output {
            exact = Some(entry);
        } else if wallpaper.output == "*" {
            wildcard = Some(entry);
        }
    }
    exact.or(wildcard)
}

/// The size to decode a wallpaper at so the requested mode can be drawn without
/// resampling the same image twice. `preserve_aspect` is false, so the decoded
/// buffer is exactly this size and the drawing code never has to scale again.
fn decode_target(file: &str, fit: &str, width: i32, height: i32) -> Option<(i32, i32)> {
    let (_, native_width, native_height) = gdk_pixbuf::Pixbuf::file_info(file)?;
    let native = (native_width.max(1), native_height.max(1));
    // Center and tile draw the image at its own scale, so it must not be shrunk
    // to the output; both are clipped or repeated instead.
    if fit == "center" || fit == "tile" {
        return Some(clamp(native));
    }
    if fit == "stretch" {
        return Some(clamp((width, height)));
    }
    let (w, h) = (f64::from(width), f64::from(height));
    let (iw, ih) = (f64::from(native.0), f64::from(native.1));
    // Fit shows the whole image, so the smaller scale wins; fill covers the
    // whole output, so the larger one does.
    let scale = if fit == "fit" {
        (w / iw).min(h / ih)
    } else {
        (w / iw).max(h / ih)
    };
    if !scale.is_finite() || scale <= 0. {
        return Some(clamp(native));
    }
    Some(clamp((
        (iw * scale).round() as i32,
        (ih * scale).round() as i32,
    )))
}

/// Shrinks a decode request to `MAX_EDGE` without changing its aspect ratio.
fn clamp(size: (i32, i32)) -> (i32, i32) {
    let (w, h) = (size.0.max(1), size.1.max(1));
    let longest = w.max(h);
    if longest <= MAX_EDGE {
        return (w, h);
    }
    let scale = f64::from(MAX_EDGE) / f64::from(longest);
    (
        (f64::from(w) * scale).round().max(1.) as i32,
        (f64::from(h) * scale).round().max(1.) as i32,
    )
}

/// The colour behind the image. The service only accepts `#rrggbb`, but a
/// release build aborts on a panic and this process is the whole session.
fn parse_color(color: &str) -> (f64, f64, f64) {
    let digits = match color.strip_prefix('#') {
        Some(digits) if digits.len() == 6 => digits,
        _ => return FALLBACK,
    };
    let channel = |range: std::ops::Range<usize>| {
        u8::from_str_radix(&digits[range], 16)
            .map(|value| f64::from(value) / 255.)
            .ok()
    };
    match (channel(0..2), channel(2..4), channel(4..6)) {
        (Some(red), Some(green), Some(blue)) => (red, green, blue),
        _ => FALLBACK,
    }
}
