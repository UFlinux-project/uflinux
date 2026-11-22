//! The capture overlay shown on Print Screen.
use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk::gdk;
use gtk::prelude::*;
use settings_core::CaptureTarget;

use crate::backend::{Region, Target};
use crate::{selection, shell, still};

pub enum Choice {
    Still(Target),
    Video(Target),
    Cancel,
}

struct Controls {
    screen: gtk::ToggleButton,
    window: gtk::ToggleButton,
    area: gtk::ToggleButton,
    record: gtk::ToggleButton,
    dim: gtk::Box,
    selection: gtk::DrawingArea,
    area_hint: gtk::Label,
    window_list: gtk::Box,
    windows: gtk::ListBox,
    identifiers: RefCell<Vec<String>>,
    region: RefCell<Option<Region>>,
    capture: gtk::Button,
    monitor: String,
}

impl Controls {
    fn target(&self) -> Option<Target> {
        if self.area.is_active() {
            return self
                .region
                .borrow()
                .clone()
                .map(|region| Target::Area(self.monitor.clone(), region));
        }
        if self.window.is_active() {
            let index = self.windows.selected_row()?.index() as usize;
            return self
                .identifiers
                .borrow()
                .get(index)
                .cloned()
                .map(Target::Window);
        }
        Some(Target::Screen(self.monitor.clone()))
    }

    fn update(&self) {
        let record = self.record.is_active();
        self.window.set_sensitive(!record);
        if record && self.window.is_active() {
            self.screen.set_active(true);
        }
        let area = self.area.is_active();
        self.selection.set_visible(area);
        self.area_hint.set_visible(area);
        self.window_list.set_visible(self.window.is_active());
        self.dim.set_visible(!area);
        self.capture
            .set_label(if record { "Start Recording" } else { "Capture" });
        self.capture.set_sensitive(self.target().is_some());
    }
}

pub fn show(
    monitor: &gdk::Monitor,
    initial_target: CaptureTarget,
    recording_allowed: bool,
    direct_recording: Option<bool>,
    on_choice: impl FnOnce(Choice) + 'static,
) {
    let surface = gtk::Window::new();
    shell::popup(&surface, monitor, shell::Keyboard::Exclusive);
    shell::add_escape_key(&surface);

    let overlay = gtk::Overlay::new();
    let dim = gtk::Box::new(gtk::Orientation::Vertical, 0);
    dim.add_css_class("uflinux-capture-dim");
    overlay.set_child(Some(&dim));

    let card = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(10)
        .halign(gtk::Align::Center)
        .valign(gtk::Align::End)
        .margin_bottom(32)
        .css_classes(["uflinux-capture-card"])
        .build();

    let screenshot = gtk::ToggleButton::with_label("Screenshot");
    let record = gtk::ToggleButton::with_label("Record");
    record.set_group(Some(&screenshot));
    record.set_sensitive(recording_allowed && direct_recording.is_none());
    if direct_recording == Some(true) {
        record.set_active(true);
    } else {
        screenshot.set_active(true);
    }

    let screen = gtk::ToggleButton::with_label("Screen");
    let window = gtk::ToggleButton::with_label("Window");
    let area = gtk::ToggleButton::with_label("Area");
    window.set_group(Some(&screen));
    area.set_group(Some(&screen));
    match initial_target {
        CaptureTarget::Screen => screen.set_active(true),
        CaptureTarget::Window => window.set_active(true),
        CaptureTarget::Area => area.set_active(true),
    }

    if direct_recording.is_none() {
        let modes = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        modes.add_css_class("linked");
        modes.set_halign(gtk::Align::Center);
        modes.append(&screenshot);
        modes.append(&record);
        card.append(&modes);
    }
    if direct_recording != Some(false) {
        let targets = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        targets.add_css_class("linked");
        targets.set_halign(gtk::Align::Center);
        targets.append(&screen);
        targets.append(&window);
        targets.append(&area);
        card.append(&targets);
    }

    let area_hint = gtk::Label::new(Some("Drag to select an area"));
    card.append(&area_hint);

    let window_list = gtk::Box::new(gtk::Orientation::Vertical, 4);
    let window_hint = gtk::Label::new(Some("Finding windows…"));
    let windows = gtk::ListBox::new();
    windows.set_selection_mode(gtk::SelectionMode::Single);
    windows.add_css_class("boxed-list");
    let scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .min_content_width(monitor.geometry().width().min(280))
        .max_content_height((monitor.geometry().height() / 3).max(48))
        .propagate_natural_height(true)
        .child(&windows)
        .visible(false)
        .build();
    window_list.append(&window_hint);
    window_list.append(&scroller);
    card.append(&window_list);

    let actions = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    actions.set_halign(gtk::Align::Center);
    let cancel = gtk::Button::with_label("Cancel");
    let capture = gtk::Button::with_label("Capture");
    capture.add_css_class("suggested-action");
    actions.append(&cancel);
    actions.append(&capture);
    card.append(&actions);
    surface.set_default_widget(Some(&capture));

    let controls = Rc::new_cyclic(|weak: &std::rc::Weak<Controls>| {
        let weak = weak.clone();
        let selection = selection::area(monitor.geometry(), move |region| {
            if let Some(controls) = weak.upgrade() {
                controls.region.replace(region);
                controls.update();
            }
        });
        Controls {
            screen,
            window,
            area,
            record,
            dim,
            selection,
            area_hint,
            window_list,
            windows,
            identifiers: RefCell::new(Vec::new()),
            region: RefCell::new(None),
            capture,
            monitor: monitor.connector().map(String::from).unwrap_or_default(),
        }
    });
    overlay.add_overlay(&controls.selection);
    overlay.add_overlay(&card);
    surface.set_child(Some(&overlay));

    let once = Rc::new(RefCell::new(Some(on_choice)));
    let cancel_choice = once.clone();
    let keep_alive = controls.clone();
    surface.connect_close_request(move |_| {
        // Keep the controls alive until the surface closes.
        let _ = &keep_alive;
        if let Some(done) = cancel_choice.borrow_mut().take() {
            done(Choice::Cancel);
        }
        glib::Propagation::Proceed
    });
    cancel.connect_clicked({
        let surface = surface.downgrade();
        move |_| {
            if let Some(surface) = surface.upgrade() {
                surface.close();
            }
        }
    });
    controls.capture.connect_clicked({
        let surface = surface.downgrade();
        let controls = Rc::downgrade(&controls);
        move |_| {
            let (Some(surface), Some(controls)) = (surface.upgrade(), controls.upgrade()) else {
                return;
            };
            let Some(target) = controls.target() else {
                return;
            };
            let Some(done) = once.borrow_mut().take() else {
                return;
            };
            let choice = if controls.record.is_active() {
                Choice::Video(target)
            } else {
                Choice::Still(target)
            };
            surface.close();
            // The worker grabs a frame on a separate Wayland connection.
            gtk::prelude::WidgetExt::display(&surface).sync();
            done(choice);
        }
    });

    for toggle in [
        &controls.screen,
        &controls.window,
        &controls.area,
        &controls.record,
    ] {
        let weak = Rc::downgrade(&controls);
        toggle.connect_toggled(move |_| {
            if let Some(controls) = weak.upgrade() {
                controls.update();
            }
        });
    }
    controls.windows.connect_row_selected({
        let weak = Rc::downgrade(&controls);
        move |_, _| {
            if let Some(controls) = weak.upgrade() {
                controls.update();
            }
        }
    });

    let loaded = Cell::new(false);
    let load_windows = {
        let weak = Rc::downgrade(&controls);
        move || {
            if loaded.replace(true) {
                return;
            }
            let weak = weak.clone();
            let window_hint = window_hint.clone();
            let scroller = scroller.clone();
            glib::spawn_future_local(async move {
                let result = gio::spawn_blocking(still::windows)
                    .await
                    .unwrap_or_else(|_| Err("Window discovery was interrupted".into()));
                let Some(controls) = weak.upgrade() else {
                    return;
                };
                match result {
                    Ok(windows) if windows.is_empty() => {
                        window_hint.set_text("No windows available")
                    }
                    Ok(windows) => {
                        for window in windows {
                            let label = gtk::Label::builder()
                                .label(&window.label)
                                .ellipsize(gtk::pango::EllipsizeMode::End)
                                .max_width_chars(45)
                                .xalign(0.0)
                                .margin_top(6)
                                .margin_bottom(6)
                                .margin_start(8)
                                .margin_end(8)
                                .build();
                            label.set_tooltip_text(Some(&window.label));
                            controls.identifiers.borrow_mut().push(window.identifier);
                            controls.windows.append(&label);
                        }
                        window_hint.set_visible(false);
                        scroller.set_visible(true);
                    }
                    Err(error) => window_hint.set_text(&error),
                }
            });
        }
    };
    if controls.window.is_active() {
        load_windows();
    }
    controls.window.connect_toggled(move |window| {
        if window.is_active() {
            load_windows();
        }
    });
    controls.update();
    surface.present();
}
