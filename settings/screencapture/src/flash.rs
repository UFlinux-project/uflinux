//! Brief shutter flash after the compositor has returned the screenshot.
use std::time::Duration;

use gtk::prelude::*;
use gtk::{gdk, glib};

use crate::shell;

pub const DURATION: Duration = Duration::from_millis(120);

pub fn show(monitor: &gdk::Monitor) {
    if !monitor.is_valid() {
        return;
    }
    let window = gtk::Window::new();
    shell::popup(&window, monitor, shell::Keyboard::None);
    let area = gtk::DrawingArea::new();
    area.set_draw_func(|_, cr, _, _| {
        cr.set_source_rgb(1.0, 1.0, 1.0);
        let _ = cr.paint();
    });
    window.set_child(Some(&area));
    window.connect_realize(|window| {
        if let Some(surface) = window.surface() {
            surface.set_input_region(Some(&gtk::cairo::Region::create()));
        }
    });
    window.present();
    glib::timeout_add_local_once(DURATION, move || window.close());
}
