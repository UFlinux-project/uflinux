//! Layer-shell surfaces for the capture overlay and shutter flash.
use gtk::prelude::*;
use gtk::{gdk, glib};
use gtk4_layer_shell::{Edge, Layer, LayerShell};

pub use gtk4_layer_shell::KeyboardMode as Keyboard;

const CSS: &str = "
window.uflinux-capture { background: transparent; box-shadow: none; }
.uflinux-capture-dim { background: alpha(black, 0.38); }
.uflinux-capture-card {
    background: @theme_bg_color; color: @theme_fg_color;
    border: 1px solid alpha(@theme_fg_color, 0.18);
    border-radius: 14px; padding: 12px;
    box-shadow: 0 4px 16px alpha(black, 0.25);
}
";

pub fn popup(window: &gtk::Window, monitor: &gdk::Monitor, keyboard: Keyboard) {
    window.set_decorated(false);
    let geometry = monitor.geometry();
    window.set_default_size(geometry.width(), geometry.height());
    window.add_css_class("uflinux-capture");
    window.init_layer_shell();
    window.set_namespace(Some("uflinux-screencapture"));
    window.set_layer(Layer::Overlay);
    window.set_exclusive_zone(-1);
    for edge in [Edge::Left, Edge::Right, Edge::Top, Edge::Bottom] {
        window.set_anchor(edge, true);
    }
    window.set_keyboard_mode(keyboard);
    window.set_monitor(Some(monitor));
    let weak = window.downgrade();
    monitor.connect_invalidate(move |_| {
        if let Some(window) = weak.upgrade() {
            window.close();
        }
    });
}

pub fn add_css() {
    let Some(display) = gdk::Display::default() else {
        return;
    };
    let provider = gtk::CssProvider::new();
    provider.load_from_string(CSS);
    gtk::style_context_add_provider_for_display(
        &display,
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
}

pub fn add_escape_key(window: &gtk::Window) {
    let controller = gtk::EventControllerKey::new();
    controller.set_propagation_phase(gtk::PropagationPhase::Capture);
    controller.connect_key_pressed(|controller, key, _, _| {
        if key == gdk::Key::Escape {
            if let Some(window) = controller.widget().and_downcast::<gtk::Window>() {
                window.close();
            }
            glib::Propagation::Stop
        } else {
            glib::Propagation::Proceed
        }
    });
    window.add_controller(controller);
}

/// The monitor under the pointer when GDK knows its surface, or the first output.
pub fn pointed_monitor() -> Option<gdk::Monitor> {
    let display = gdk::Display::default()?;
    if let Some(surface) = display
        .default_seat()
        .and_then(|seat| seat.pointer())
        .and_then(|pointer| pointer.surface_at_position().0)
    {
        if let Some(monitor) = display.monitor_at_surface(&surface) {
            return Some(monitor);
        }
    }
    let monitors = display.monitors();
    (0..monitors.n_items())
        .filter_map(|index| monitors.item(index).and_downcast::<gdk::Monitor>())
        .find(|monitor| monitor.is_valid())
}
