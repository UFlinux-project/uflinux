//! Experimental panel surface; the installed session remains on Ironbar.
use crate::{app::App, catalog, notifications, tray::Tray};
use gtk::prelude::*;
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    rc::{Rc, Weak},
};
use uflinux_desktop::{
    taskbar::{Activation, Application, Group, Taskbar, WindowId},
    toplevel::{Client, Command},
};

/// Persisted taskbar favorites, so pins and drag-and-drop order survive a
/// logout. A missing or damaged file falls back to the default apps.
#[derive(serde::Deserialize, serde::Serialize)]
#[serde(default)]
struct TaskbarConfig {
    pinned: Vec<String>,
}
impl Default for TaskbarConfig {
    fn default() -> Self {
        Self {
            pinned: vec!["foot.desktop".into(), "org.uflinux.Files.desktop".into()],
        }
    }
}
fn taskbar_config_path() -> std::path::PathBuf {
    settings_core::files::config_dir().join("taskbar.json")
}

/// Starts (or restarts) the short countdown that closes a preview card after
/// the pointer leaves either the task button or the card itself.
fn arm_preview_grace(grace: &Rc<std::cell::Cell<Option<glib::SourceId>>>, card: &gtk::Popover) {
    if let Some(id) = grace.take() {
        id.remove();
    }
    let card = card.clone();
    let elapsed = grace.clone();
    let id = glib::timeout_add_local_once(
        std::time::Duration::from_millis(TASK_PREVIEW_GRACE_MS),
        move || {
            elapsed.take();
            card.popdown();
        },
    );
    grace.set(Some(id));
}

/// Panel logo size in device-independent pixels.
const LOGO_PIXELS: i32 = 28;
/// Layout cell one tray icon occupies in the overflow flyout, so the grid can be
/// sized from the output width instead of overflowing a small monitor.
const TRAY_CELL: i32 = 44;
/// The flyout keeps a usable shape on a tiny output and stops widening at eight
/// columns on a large one, trading a little width for a shorter popup.
const TRAY_MIN_COLUMNS: i32 = 4;
const TRAY_MAX_COLUMNS: i32 = 8;
/// Beyond this many icons deep the flyout scrolls rather than growing upwards
/// past the panel and off the screen.
const TRAY_FLYOUT_MAX_HEIGHT: i32 = 240;
/// How long the pointer rests on a task before its window list previews itself.
const TASK_PREVIEW_DELAY_MS: u64 = 350;
/// How long the preview stays up after the pointer leaves the task, so the
/// pointer can travel from the button into the card and close a window there.
const TASK_PREVIEW_GRACE_MS: u64 = 250;

struct TaskButton {
    button: gtk::Button,
    image: gtk::Image,
    icon: String,
    menu: gtk::Popover,
    preview: gtk::Popover,
}
impl Drop for TaskButton {
    fn drop(&mut self) {
        // GTK aborts when a visible popover is destroyed, and a task button is
        // dropped as soon as its app stops advertising a window.
        self.menu.popdown();
        self.preview.popdown();
        self.menu.unparent();
        self.preview.unparent();
    }
}
struct TrayButton {
    button: gtk::Button,
    image: gtk::Image,
    popover: gtk::Popover,
    content: gtk::Box,
    revision: u64,
    /// Grid cell the icon currently occupies, so a render only re-attaches a
    /// button whose position actually moved.
    cell: (i32, i32),
}
struct Surface {
    overflow: gtk::Button,
    overflow_popover: gtk::Popover,
    overflow_scroll: gtk::ScrolledWindow,
    overflow_grid: gtk::Grid,
    overflow_columns: i32,
    tray_buttons: HashMap<String, TrayButton>,
    monitor: gtk::gdk::Monitor,
    window: gtk::ApplicationWindow,
    tasks: gtk::Box,
    buttons: HashMap<String, TaskButton>,
    clock: gtk::Popover,
    clock_label: gtk::Label,
    clock_time: gtk::Label,
}
pub struct Panel {
    previews_enabled: std::cell::Cell<bool>,
    thumbnail_cache: RefCell<HashMap<WindowId, (std::time::Instant, crate::thumbnails::Image)>>,
    app: Weak<App>,
    surfaces: RefCell<Vec<Surface>>,
    taskbar: RefCell<Taskbar>,
    tracker: RefCell<Option<Client>>,
    tray: RefCell<Option<Rc<Tray>>>,
    window_ids: RefCell<HashSet<WindowId>>,
    clock_timer: RefCell<Option<glib::SourceId>>,
    clock_watch: RefCell<Option<crate::clock_watch::ClockWatch>>,
    /// Favorite currently under a drag, so a drop is only accepted from
    /// another taskbar button and not from an arbitrary text drag.
    task_drag: RefCell<Option<String>>,
}
impl Panel {
    pub fn set_previews_enabled(&self, enabled: bool) {
        if self.previews_enabled.replace(enabled) != enabled {
            self.hide_task_menus(None);
            self.thumbnail_cache.borrow_mut().clear();
        }
    }
    pub fn new(app: &Rc<App>) -> Rc<Self> {
        let panel = Rc::new(Self {
            previews_enabled: std::cell::Cell::new(true),
            thumbnail_cache: RefCell::new(HashMap::new()),
            app: Rc::downgrade(app),
            surfaces: RefCell::new(Vec::new()),
            taskbar: RefCell::new(Taskbar::new(
                settings_core::files::read_json::<TaskbarConfig>(&taskbar_config_path()).pinned,
            )),
            tracker: RefCell::new(None),
            tray: RefCell::new(None),
            window_ids: RefCell::new(HashSet::new()),
            clock_timer: RefCell::new(None),
            clock_watch: RefCell::new(None),
            task_drag: RefCell::new(None),
        });
        if let Some(display) = gtk::gdk::Display::default() {
            let monitors = display.monitors();
            let weak = Rc::downgrade(&panel);
            monitors.connect_items_changed(move |monitors, _, _, _| {
                if let Some(panel) = weak.upgrade() {
                    panel.sync(monitors);
                    if let Some(app) = panel.app.upgrade() {
                        app.quick.desktop_outputs_changed();
                    }
                }
            });
            panel.sync(&monitors);
        }
        if let Ok((tracker, notifications)) = Client::start() {
            panel.tracker.replace(Some(tracker));
            let weak = Rc::downgrade(&panel);
            glib::MainContext::default().spawn_local(async move {
                while notifications.recv().await.is_ok() {
                    let Some(panel) = weak.upgrade() else { break };
                    panel.update_windows();
                }
            });
        }
        let weak = Rc::downgrade(&panel);
        glib::MainContext::default().spawn_local(async move {
            let changed = weak.clone();
            match Tray::start(move || {
                if let Some(panel) = changed.upgrade() {
                    panel.render_tray();
                }
            })
            .await
            {
                Ok(tray) => {
                    if let Some(panel) = weak.upgrade() {
                        panel.tray.replace(Some(tray));
                        panel.render_tray();
                    }
                }
                Err(error) => eprintln!("Desktop tray unavailable: {error}"),
            }
        });
        panel.schedule_clock();
        if let Ok((watch, notifications)) = crate::clock_watch::ClockWatch::start() {
            panel.clock_watch.replace(Some(watch));
            let weak = Rc::downgrade(&panel);
            glib::MainContext::default().spawn_local(async move {
                while notifications.recv().await.is_ok() {
                    let Some(panel) = weak.upgrade() else { break };
                    panel.schedule_clock();
                }
            });
        }
        let weak = Rc::downgrade(&panel);
        glib::MainContext::default().spawn_local(async move {
            if let Ok(bus) = gio::bus_get_future(gio::BusType::System).await {
                bus.signal_subscribe(
                    Some("org.freedesktop.login1"),
                    Some("org.freedesktop.login1.Manager"),
                    Some("PrepareForSleep"),
                    Some("/org/freedesktop/login1"),
                    None,
                    gio::DBusSignalFlags::NONE,
                    move |_, _, _, _, _, parameters| {
                        if parameters.get::<(bool,)>() == Some((false,)) {
                            if let Some(panel) = weak.upgrade() {
                                panel.schedule_clock();
                            }
                        }
                    },
                );
            }
        });
        panel
    }
    pub fn set_catalog(self: &Rc<Self>, catalog: &catalog::Catalog) {
        let applications = catalog
            .entries
            .iter()
            .map(|entry| Application {
                desktop_id: entry.id.clone(),
                name: entry.name.clone(),
                icon: entry.icon.clone(),
                startup_wm_class: entry.startup_wm_class.clone(),
            })
            .collect();
        self.taskbar.borrow_mut().set_applications(applications);
        self.render();
    }
    fn update_clock(&self) {
        if let Ok(now) = glib::DateTime::now_local() {
            let panel = now.format("%H:%M\n%d/%m/%Y").ok();
            let header = now.format("%H:%M").ok();
            for surface in self.surfaces.borrow().iter() {
                if let Some(text) = &panel {
                    surface.clock_label.set_text(text);
                }
                if let Some(text) = &header {
                    surface.clock_time.set_text(text);
                }
            }
        }
    }
    fn schedule_clock(self: &Rc<Self>) {
        if let Some(timer) = self.clock_timer.borrow_mut().take() {
            timer.remove();
        }
        self.update_clock();
        let wait = glib::DateTime::now_local()
            .map(|n| 60_000_000 - i64::from(n.second()) * 1_000_000 - i64::from(n.microsecond()))
            .unwrap_or(60_000_000)
            .max(1);
        let weak = Rc::downgrade(self);
        let timer = glib::timeout_add_local_once(
            std::time::Duration::from_micros(wait as u64),
            move || {
                if let Some(panel) = weak.upgrade() {
                    panel.clock_timer.borrow_mut().take();
                    panel.schedule_clock();
                }
            },
        );
        self.clock_timer.replace(Some(timer));
    }
    pub fn hide_clocks(&self, except: Option<&gtk::Popover>) {
        let clocks: Vec<_> = self
            .surfaces
            .borrow()
            .iter()
            .map(|s| s.clock.clone())
            .collect();
        for clock in clocks {
            if except != Some(&clock) {
                clock.popdown();
            }
        }
    }
    pub fn hide_trays(&self, except: Option<&gtk::Popover>) {
        let popovers: Vec<_> = self
            .surfaces
            .borrow()
            .iter()
            .flat_map(|s| {
                std::iter::once(s.overflow_popover.clone())
                    .chain(s.tray_buttons.values().map(|item| item.popover.clone()))
            })
            .collect();
        for popover in popovers {
            // An item menu lives inside the flyout, so the flyout is an
            // ancestor of it and has to stay open while that menu is up;
            // popping it would take the menu down with it.
            let keep =
                except.is_some_and(|except| except == &popover || except.is_ancestor(&popover));
            if !keep {
                popover.popdown();
            }
        }
    }
    pub fn hide_task_menus(&self, except: Option<&gtk::Popover>) {
        let menus: Vec<_> = self
            .surfaces
            .borrow()
            .iter()
            .flat_map(|surface| {
                surface
                    .buttons
                    .values()
                    .flat_map(|item| [item.menu.clone(), item.preview.clone()])
            })
            .collect();
        for menu in menus {
            // Only a shown popover has a hide handler to run, so skipping the
            // rest keeps a close from reporting a status change that never was.
            if except != Some(&menu) && menu.is_visible() {
                menu.popdown();
            }
        }
    }
    /// Column count the flyout grid should use for an output of this width. The
    /// popup is clamped to the monitor so a handful of icons never stretch a
    /// small display's flyout past its edge.
    fn flyout_columns(monitor: &gtk::gdk::Monitor) -> i32 {
        let width = monitor.geometry().width();
        let available = (width / TRAY_CELL).min(TRAY_MAX_COLUMNS);
        available.max(TRAY_MIN_COLUMNS)
    }
    fn render_tray(self: &Rc<Self>) {
        let items = self
            .tray
            .borrow()
            .as_ref()
            .map(|tray| tray.items())
            .unwrap_or_default();
        let wanted: HashSet<_> = items.iter().map(|item| item.key.as_str()).collect();
        for surface in self.surfaces.borrow_mut().iter_mut() {
            surface.tray_buttons.retain(|id, item| {
                if wanted.contains(id.as_str()) {
                    true
                } else {
                    item.popover.unparent();
                    surface.overflow_grid.remove(&item.button);
                    false
                }
            });
            // Nothing to hide means no chevron at all, which keeps the panel at
            // the width it had before any tray app started.
            surface.overflow.set_visible(!items.is_empty());
            if items.is_empty() {
                surface.overflow_popover.popdown();
            }
            let label = match items.len() {
                1 => "Show 1 hidden icon".to_string(),
                count => format!("Show {count} hidden icons"),
            };
            surface.overflow.set_tooltip_text(Some(&label));
            surface
                .overflow
                .update_property(&[gtk::accessible::Property::Label(&label)]);
            let columns = Self::flyout_columns(&surface.monitor);
            if columns != surface.overflow_columns {
                surface.overflow_columns = columns;
            }
            let columns = surface.overflow_columns;
            // A popover sizes to its content's natural width, which a
            // homogeneous grid reports as only a few columns. Pin the grid to
            // the width its icons actually need so a flyout of several icons is
            // never squeezed into a single clipped column.
            let used = (items.len() as i32).clamp(1, columns);
            surface.overflow_grid.set_size_request(used * TRAY_CELL, -1);
            surface
                .overflow_scroll
                .set_max_content_width(columns * TRAY_CELL);
            for (index, item) in items.iter().enumerate() {
                let cell = ((index as i32) % columns, (index as i32) / columns);
                let button = surface
                    .tray_buttons
                    .entry(item.key.clone())
                    .or_insert_with(|| {
                        let button = gtk::Button::new();
                        let image = gtk::Image::new();
                        image.set_pixel_size(18);
                        button.set_child(Some(&image));
                        button.add_css_class("uflinux-tray-icon");
                        let popover = gtk::Popover::new();
                        let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
                        popover.set_child(Some(&content));
                        popover.set_parent(&button);
                        let weak = self.app.clone();
                        let output = surface
                            .monitor
                            .connector()
                            .map(|s| s.to_string())
                            .unwrap_or_default();
                        let target = popover.downgrade();
                        popover.connect_show(move |_| {
                            if let (Some(app), Some(popover)) = (weak.upgrade(), target.upgrade()) {
                                app.show_tray(&popover, &output);
                            }
                        });
                        let weak = self.app.clone();
                        popover.connect_hide(move |_| {
                            if let Some(app) = weak.upgrade() {
                                app.tray_closed();
                            }
                        });
                        let key = item.key.clone();
                        let weak = Rc::downgrade(self);
                        let menu = popover.clone();
                        let contents = content.clone();
                        button.connect_clicked(move |_| {
                            let Some(panel) = weak.upgrade() else { return };
                            if let Some(tray) = panel.tray.borrow().as_ref() {
                                if let Some(item) =
                                    tray.items().into_iter().find(|item| item.key == key)
                                {
                                    if item.item_is_menu {
                                        tray.show_menu(&item, &menu, &contents);
                                    } else {
                                        tray.activate(&item, "Activate", 0, 0);
                                    }
                                }
                            }
                        });
                        let key = item.key.clone();
                        let weak = Rc::downgrade(self);
                        let menu = popover.clone();
                        let contents = content.clone();
                        let click = gtk::GestureClick::new();
                        click.set_button(3);
                        click.connect_pressed(move |_, _, _, _| {
                            let Some(panel) = weak.upgrade() else { return };
                            if let Some(tray) = panel.tray.borrow().as_ref() {
                                if let Some(item) =
                                    tray.items().into_iter().find(|item| item.key == key)
                                {
                                    tray.show_menu(&item, &menu, &contents);
                                }
                            }
                        });
                        button.add_controller(click);
                        let key = item.key.clone();
                        let weak = Rc::downgrade(self);
                        let scroll = gtk::EventControllerScroll::new(
                            gtk::EventControllerScrollFlags::BOTH_AXES
                                | gtk::EventControllerScrollFlags::DISCRETE,
                        );
                        scroll.connect_scroll(move |_, dx, dy| {
                            let Some(panel) = weak.upgrade() else {
                                return glib::Propagation::Proceed;
                            };
                            if let Some(tray) = panel.tray.borrow().as_ref() {
                                if let Some(item) =
                                    tray.items().into_iter().find(|item| item.key == key)
                                {
                                    let horizontal = dx.abs() > dy.abs();
                                    let delta = if horizontal { dx } else { dy };
                                    tray.scroll(&item, (delta * 120.0) as i32, horizontal);
                                }
                            }
                            glib::Propagation::Stop
                        });
                        button.add_controller(scroll);
                        TrayButton {
                            button,
                            image,
                            popover,
                            content,
                            revision: item.menu_revision,
                            cell,
                        }
                    });
                if button.revision != item.menu_revision {
                    button.revision = item.menu_revision;
                    if button.popover.is_visible() {
                        if let Some(tray) = self.tray.borrow().as_ref() {
                            tray.show_menu(item, &button.popover, &button.content);
                        }
                    }
                }
                button.button.set_tooltip_text(Some(&item.tooltip));
                button
                    .button
                    .update_property(&[gtk::accessible::Property::Label(&item.title)]);
                if let Some(texture) = &item.pixmap {
                    button.image.set_paintable(Some(texture));
                } else {
                    button.image.set_icon_name(Some(&item.icon));
                }
                // Attaching a button that is already in the grid moves it, so a
                // registration or an output change re-lays the icons out without
                // rebuilding their menus or losing an open one.
                if button.cell != cell || button.button.parent().is_none() {
                    surface
                        .overflow_grid
                        .attach(&button.button, cell.0, cell.1, 1, 1);
                    button.cell = cell;
                }
            }
        }
    }
    fn update_windows(self: &Rc<Self>) {
        let snapshot = self.tracker.borrow().as_ref().map(Client::snapshot);
        let Some(snapshot) = snapshot else { return };
        let ids: HashSet<_> = snapshot.windows.iter().map(|w| w.id).collect();
        {
            let mut model = self.taskbar.borrow_mut();
            for id in self.window_ids.borrow().difference(&ids) {
                model.remove_window(*id);
            }
            for window in snapshot.windows {
                model.upsert_window(window);
            }
        }
        self.window_ids.replace(ids);
        let windows = self
            .taskbar
            .borrow()
            .groups()
            .into_iter()
            .flat_map(|g| g.windows)
            .collect::<Vec<_>>();
        self.thumbnail_cache.borrow_mut().retain(|id, (_, image)| {
            windows
                .iter()
                .any(|w| w.id == *id && w.app_id == image.app_id && w.title == image.title)
        });
        self.render();
    }
    fn render(self: &Rc<Self>) {
        let groups = self.taskbar.borrow().groups();
        let wanted: HashSet<_> = groups.iter().map(|group| group.id.as_str()).collect();
        for surface in self.surfaces.borrow_mut().iter_mut() {
            surface.buttons.retain(|id, item| {
                if wanted.contains(id.as_str()) {
                    true
                } else {
                    // Take a menu or preview down before the button that owns
                    // it disappears; GTK aborts on a visible popover.
                    item.menu.popdown();
                    item.preview.popdown();
                    surface.tasks.remove(&item.button);
                    false
                }
            });
            let mut previous: Option<gtk::Button> = None;
            for group in &groups {
                let item = surface.buttons.entry(group.id.clone()).or_insert_with(|| {
                    let button = gtk::Button::new();
                    let menu = gtk::Popover::new();
                    menu.set_parent(&button);
                    let output = surface
                        .monitor
                        .connector()
                        .map(|s| s.to_string())
                        .unwrap_or_default();
                    let weak = Rc::downgrade(self);
                    menu.connect_hide(move |_| {
                        if let Some(panel) = weak.upgrade() {
                            if let Some(app) = panel.app.upgrade() {
                                app.task_closed();
                            }
                        }
                    });
                    // The hover preview is its own popover so the right-click
                    // menu and the card can never fight over one child.
                    let preview = gtk::Popover::new();
                    preview.set_parent(&button);
                    let weak = Rc::downgrade(self);
                    preview.connect_hide(move |popover| {
                        popover.set_child(None::<&gtk::Widget>);
                        if let Some(panel) = weak.upgrade() {
                            if let Some(app) = panel.app.upgrade() {
                                app.task_closed();
                            }
                        }
                    });
                    let click = gtk::GestureClick::new();
                    click.set_button(3);
                    let context_id = group.id.clone();
                    let context_menu = menu.clone();
                    let context_preview = preview.clone();
                    let context_output = output.clone();
                    let weak = Rc::downgrade(self);
                    click.connect_pressed(move |_, _, _, _| {
                        let Some(panel) = weak.upgrade() else { return };
                        context_menu.popdown();
                        context_preview.popdown();
                        let Some(group) = panel
                            .taskbar
                            .borrow()
                            .groups()
                            .into_iter()
                            .find(|g| g.id == context_id)
                        else {
                            return;
                        };
                        let Some(column) = panel.task_menu(&context_menu, &group) else {
                            return;
                        };
                        if let Some(app) = panel.app.upgrade() {
                            app.hide_except_task(&context_menu);
                            app.task_opened(&context_output);
                        }
                        context_menu.set_child(Some(&column));
                        context_menu.popup();
                    });
                    button.add_controller(click);
                    // Resting the pointer on a running app previews its windows
                    // in a card of its own, so a hover costs one timer and one
                    // small box rather than a screenshot.
                    let hover = gtk::EventControllerMotion::new();
                    let pending = Rc::new(std::cell::Cell::new(None::<glib::SourceId>));
                    let grace = Rc::new(std::cell::Cell::new(None::<glib::SourceId>));
                    hover.connect_enter({
                        let pending = pending.clone();
                        let grace = grace.clone();
                        let menu = menu.clone();
                        let card = preview.clone();
                        let preview_id = group.id.clone();
                        let weak = Rc::downgrade(self);
                        let preview_output = output.clone();
                        move |_, _, _| {
                            if menu.is_visible() || card.is_visible() || weak.upgrade().is_none() {
                                return;
                            }
                            if let Some(id) = pending.take() {
                                id.remove();
                            }
                            if let Some(id) = grace.take() {
                                id.remove();
                            }
                            let weak = weak.clone();
                            let card = card.clone();
                            let grace = grace.clone();
                            let id = preview_id.clone();
                            let output = preview_output.clone();
                            let elapsed = pending.clone();
                            let source = glib::timeout_add_local_once(
                                std::time::Duration::from_millis(TASK_PREVIEW_DELAY_MS),
                                move || {
                                    elapsed.take();
                                    let Some(panel) = weak.upgrade() else { return };
                                    // The task may have ended while the timer ran,
                                    // which unparents the popover; popping up an
                                    // orphaned popover is a fatal GTK critical.
                                    if card.parent().is_none() {
                                        return;
                                    }
                                    let Some(group) = panel
                                        .taskbar
                                        .borrow()
                                        .groups()
                                        .into_iter()
                                        .find(|g| g.id == id)
                                    else {
                                        return;
                                    };
                                    if group.windows.is_empty() {
                                        return;
                                    }
                                    let Some(column) = panel.task_preview(&card, &group, &grace)
                                    else {
                                        return;
                                    };
                                    if let Some(app) = panel.app.upgrade() {
                                        app.hide_except_task(&card);
                                        app.task_opened(&output);
                                    }
                                    card.set_child(Some(&column));
                                    card.popup();
                                },
                            );
                            pending.set(Some(source));
                        }
                    });
                    hover.connect_leave({
                        let pending = pending;
                        let grace = grace.clone();
                        let card = preview.clone();
                        move |_| {
                            if let Some(id) = pending.take() {
                                id.remove();
                            }
                            if card.is_visible() {
                                arm_preview_grace(&grace, &card);
                            }
                        }
                    });
                    button.add_controller(hover);
                    // Favorites can be rearranged by dragging one onto another.
                    if group.pinned {
                        let drag = gtk::DragSource::new();
                        drag.set_actions(gtk::gdk::DragAction::MOVE);
                        let dragged = group.id.clone();
                        let weak = Rc::downgrade(self);
                        drag.connect_prepare(move |_, _, _| {
                            let panel = weak.upgrade()?;
                            *panel.task_drag.borrow_mut() = Some(dragged.clone());
                            Some(gtk::gdk::ContentProvider::for_value(&glib::Value::from(
                                dragged.as_str(),
                            )))
                        });
                        let weak = Rc::downgrade(self);
                        drag.connect_drag_end(move |_, _, _| {
                            if let Some(panel) = weak.upgrade() {
                                *panel.task_drag.borrow_mut() = None;
                            }
                        });
                        button.add_controller(drag);
                        let drop_target =
                            gtk::DropTarget::new(String::static_type(), gtk::gdk::DragAction::MOVE);
                        let target_id = group.id.clone();
                        let accept_id = target_id.clone();
                        let weak = Rc::downgrade(self);
                        drop_target.connect_accept(move |_, _| {
                            weak.upgrade().is_some_and(|panel| {
                                panel.task_drag.borrow().as_deref() != Some(accept_id.as_str())
                            })
                        });
                        let weak = Rc::downgrade(self);
                        drop_target.connect_drop(move |_, value, _, _| {
                            let Some(panel) = weak.upgrade() else {
                                return false;
                            };
                            let Ok(dragged) = value.get::<String>() else {
                                return false;
                            };
                            let moved = {
                                let mut taskbar = panel.taskbar.borrow_mut();
                                let (Some(from), Some(target)) = (
                                    taskbar.pinned_index(&dragged),
                                    taskbar.pinned_index(&target_id),
                                ) else {
                                    return false;
                                };
                                if from == target {
                                    return true;
                                }
                                taskbar.move_pinned(&dragged, target);
                                true
                            };
                            if moved {
                                panel.save_pinned();
                                panel.render();
                            }
                            true
                        });
                        button.add_controller(drop_target);
                    }
                    let image = gtk::Image::new();
                    image.set_pixel_size(24);
                    button.set_child(Some(&image));
                    let group_id = group.id.clone();
                    let weak = Rc::downgrade(self);
                    button.connect_clicked(move |_| {
                        let Some(panel) = weak.upgrade() else { return };
                        let action = panel
                            .taskbar
                            .borrow()
                            .groups()
                            .into_iter()
                            .find(|g| g.id == group_id)
                            .and_then(|g| g.activate());
                        match action {
                            Some(Activation::Launch(id)) => {
                                if let Some(info) = gio::DesktopAppInfo::new(&id) {
                                    let _ = info.launch(&[], None::<&gio::AppLaunchContext>);
                                }
                            }
                            Some(Activation::Activate(id)) => {
                                if let Some(tracker) = panel.tracker.borrow_mut().as_mut() {
                                    let _ = tracker.send(Command::Activate(id));
                                }
                            }
                            Some(Activation::Minimize(id)) => {
                                if let Some(tracker) = panel.tracker.borrow_mut().as_mut() {
                                    let _ = tracker.send(Command::Minimize(id));
                                }
                            }
                            None => {}
                        }
                    });
                    surface.tasks.append(&button);
                    TaskButton {
                        button,
                        image,
                        icon: String::new(),
                        menu,
                        preview,
                    }
                });
                if item.icon != group.icon {
                    if let Ok(icon) = gio::Icon::for_string(&group.icon) {
                        item.image.set_from_gicon(&icon);
                    } else {
                        item.image.set_icon_name(Some("application-x-executable"));
                    }
                    item.icon.clone_from(&group.icon);
                }
                item.button.set_tooltip_text(Some(&group.name));
                if group.windows.is_empty() {
                    item.button.remove_css_class("open");
                } else {
                    item.button.add_css_class("open");
                }
                if group.windows.iter().any(|w| w.focused) {
                    item.button.add_css_class("focused");
                } else {
                    item.button.remove_css_class("focused");
                }
                surface
                    .tasks
                    .reorder_child_after(&item.button, previous.as_ref());
                previous = Some(item.button.clone());
            }
        }
    }
    /// Persist the favorites order so the taskbar survives a restart.
    fn save_pinned(&self) {
        let pinned = self.taskbar.borrow().pinned().to_vec();
        let _ = settings_core::files::save_json(
            &taskbar_config_path(),
            &TaskbarConfig { pinned },
            0o644,
        );
    }

    /// Shared contents for a task button's right-click menu and hover preview:
    /// one row per window plus the pin/launch/close-all actions. Returns `None`
    /// when there is nothing to show so the popover stays closed. The whole
    /// menu is rebuilt on demand and thrown away with the popover, keeping the
    /// panel's steady-state cost independent of how many menus a session opens.
    fn task_menu(self: &Rc<Self>, menu: &gtk::Popover, group: &Group) -> Option<gtk::Box> {
        let column = gtk::Box::new(gtk::Orientation::Vertical, 0);
        column.style_context().add_class("uflinux-task-menu");
        let heading = gtk::Label::new(Some(&group.name));
        heading.set_halign(gtk::Align::Start);
        heading.style_context().add_class("uflinux-task-heading");
        column.append(&heading);
        let application = group.pinned || group.id.contains('.');
        let mut windows = Vec::with_capacity(group.windows.len());
        for window in &group.windows {
            windows.push((window.id, window.title.clone()));
        }
        for (window_id, title) in &windows {
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 4);
            let activate =
                gtk::Button::with_label(if title.is_empty() { &group.name } else { title });
            if let Some(label) = activate.child().and_downcast::<gtk::Label>() {
                label.set_ellipsize(gtk::pango::EllipsizeMode::End);
                label.set_max_width_chars(28);
            }
            activate.set_hexpand(true);
            activate.set_halign(gtk::Align::Fill);
            activate.style_context().add_class("flat");
            activate.style_context().add_class("uflinux-task-row");
            let weak = Rc::downgrade(self);
            let popup = menu.clone();
            let window_id = *window_id;
            activate.connect_clicked(move |_| {
                popup.popdown();
                if let Some(panel) = weak.upgrade() {
                    if let Some(tracker) = panel.tracker.borrow_mut().as_mut() {
                        let _ = tracker.send(Command::Activate(window_id));
                    }
                }
            });
            row.append(&activate);
            let close = gtk::Button::from_icon_name("window-close-symbolic");
            close.set_tooltip_text(Some("Close window"));
            close.style_context().add_class("flat");
            let weak = Rc::downgrade(self);
            let popup = menu.clone();
            close.connect_clicked(move |_| {
                popup.popdown();
                if let Some(panel) = weak.upgrade() {
                    if let Some(tracker) = panel.tracker.borrow_mut().as_mut() {
                        let _ = tracker.send(Command::Close(window_id));
                    }
                }
            });
            row.append(&close);
            column.append(&row);
        }
        if application {
            let launch = gtk::Button::with_label("Launch a new window");
            launch.set_halign(gtk::Align::Fill);
            launch.style_context().add_class("flat");
            launch.style_context().add_class("uflinux-task-row");
            let weak = Rc::downgrade(self);
            let popup = menu.clone();
            let id = group.id.clone();
            launch.connect_clicked(move |_| {
                if let Some(app) = weak.upgrade().and_then(|panel| panel.app.upgrade()) {
                    app.hide();
                }
                if let Some(info) = gio::DesktopAppInfo::new(&id) {
                    let _ = info.launch(&[], None::<&gio::AppLaunchContext>);
                }
                popup.popdown();
            });
            column.append(&launch);

            let pin = gtk::Button::with_label(if group.pinned {
                "Unpin from taskbar"
            } else {
                "Pin to taskbar"
            });
            pin.set_halign(gtk::Align::Fill);
            pin.style_context().add_class("flat");
            pin.style_context().add_class("uflinux-task-row");
            let weak = Rc::downgrade(self);
            let popup = menu.clone();
            let id = group.id.clone();
            pin.connect_clicked(move |_| {
                popup.popdown();
                let Some(panel) = weak.upgrade() else { return };
                if panel.taskbar.borrow().is_pinned(&id) {
                    panel.taskbar.borrow_mut().unpin(&id);
                } else {
                    panel.taskbar.borrow_mut().pin(&id);
                }
                panel.save_pinned();
                panel.render();
            });
            column.append(&pin);
        }
        if windows.len() > 1 {
            let close_all = gtk::Button::with_label("Close all windows");
            close_all.set_halign(gtk::Align::Fill);
            close_all.style_context().add_class("flat");
            close_all.style_context().add_class("uflinux-task-row");
            let weak = Rc::downgrade(self);
            let popup = menu.clone();
            let ids = windows.iter().map(|(id, _)| *id).collect::<Vec<_>>();
            close_all.connect_clicked(move |_| {
                popup.popdown();
                if let Some(panel) = weak.upgrade() {
                    if let Some(tracker) = panel.tracker.borrow_mut().as_mut() {
                        for window_id in &ids {
                            let _ = tracker.send(Command::Close(*window_id));
                        }
                    }
                }
            });
            column.append(&close_all);
        }
        if windows.is_empty() && !application {
            return None;
        }
        Some(column)
    }

    /// Contents for the hover preview: a small card with the app's name and one
    /// row per window. Deliberately not the right-click menu, so resting the
    /// pointer on a task never looks like a context menu appeared. Like the
    /// menu, the card is rebuilt on demand and thrown away when it closes.
    fn task_preview(
        self: &Rc<Self>,
        card: &gtk::Popover,
        group: &Group,
        grace: &Rc<std::cell::Cell<Option<glib::SourceId>>>,
    ) -> Option<gtk::Box> {
        if group.windows.is_empty() {
            return None;
        }
        let column = gtk::Box::new(gtk::Orientation::Vertical, 0);
        column.style_context().add_class("uflinux-popup-card");
        column.style_context().add_class("uflinux-task-preview");
        let header = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let icon = gtk::Image::new();
        icon.set_pixel_size(16);
        if let Ok(gicon) = gio::Icon::for_string(&group.icon) {
            icon.set_from_gicon(&gicon);
        } else {
            icon.set_icon_name(Some("application-x-executable"));
        }
        header.append(&icon);
        let title = gtk::Label::new(Some(&group.name));
        title.set_halign(gtk::Align::Start);
        title.set_ellipsize(gtk::pango::EllipsizeMode::End);
        title.set_max_width_chars(28);
        title.style_context().add_class("uflinux-task-heading");
        header.append(&title);
        column.append(&header);
        let cards = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let available_width = self
            .surfaces
            .borrow()
            .iter()
            .find(|s| s.buttons.values().any(|b| b.preview == *card))
            .map(|s| (s.monitor.geometry().width() - 32).clamp(160, 1100))
            .unwrap_or(800);
        let scroll = gtk::ScrolledWindow::builder()
            .child(&cards)
            .hscrollbar_policy(gtk::PolicyType::Automatic)
            .vscrollbar_policy(gtk::PolicyType::Never)
            .propagate_natural_height(true)
            .min_content_width(available_width.min(260))
            .max_content_width(available_width)
            .propagate_natural_width(true)
            .build();
        column.append(&scroll);
        let mut pictures = Vec::new();
        for window in &group.windows {
            let window_id = window.id;
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 4);
            let activate = gtk::Button::with_label(if window.title.is_empty() {
                &group.name
            } else {
                &window.title
            });
            if let Some(label) = activate.child().and_downcast::<gtk::Label>() {
                label.set_ellipsize(gtk::pango::EllipsizeMode::End);
                label.set_max_width_chars(28);
            }
            activate.set_hexpand(true);
            activate.set_halign(gtk::Align::Fill);
            activate.set_tooltip_text(Some(&window.title));
            activate.style_context().add_class("flat");
            activate.style_context().add_class("uflinux-task-row");
            let weak = Rc::downgrade(self);
            let popup = card.clone();
            activate.connect_clicked(move |_| {
                popup.popdown();
                if let Some(panel) = weak.upgrade() {
                    if let Some(tracker) = panel.tracker.borrow_mut().as_mut() {
                        let _ = tracker.send(Command::Activate(window_id));
                    }
                }
            });
            let tile = gtk::Box::new(gtk::Orientation::Vertical, 4);
            tile.set_width_request(260);
            if self.previews_enabled.get() {
                let picture = gtk::Picture::new();
                picture.set_size_request(256, 144);
                picture.set_content_fit(gtk::ContentFit::Contain);
                picture.set_can_shrink(true);
                picture.update_property(&[gtk::accessible::Property::Label(&format!(
                    "Preview of {}",
                    window.title
                ))]);
                let fallback = gtk::Image::new();
                fallback.set_pixel_size(48);
                if let Ok(icon) = gio::Icon::for_string(&group.icon) {
                    fallback.set_from_gicon(&icon);
                } else {
                    fallback.set_icon_name(Some("application-x-executable"));
                }
                let stack = gtk::Stack::new();
                stack.add_named(&fallback, Some("fallback"));
                stack.add_named(&picture, Some("image"));
                stack.set_visible_child_name("fallback");
                let weak_stack = stack.downgrade();
                picture.connect_paintable_notify(move |picture| {
                    if let Some(stack) = weak_stack.upgrade() {
                        stack.set_visible_child_name(if picture.paintable().is_some() {
                            "image"
                        } else {
                            "fallback"
                        });
                    }
                });
                let image_button = gtk::Button::builder().child(&stack).build();
                let target = activate.clone();
                image_button.connect_clicked(move |_| target.emit_clicked());
                tile.append(&image_button);
                pictures.push((window.clone(), picture.downgrade()));
            }
            row.append(&activate);
            let close = gtk::Button::from_icon_name("window-close-symbolic");
            close.set_tooltip_text(Some("Close window"));
            close.style_context().add_class("flat");
            let weak = Rc::downgrade(self);
            let popup = card.clone();
            close.connect_clicked(move |_| {
                popup.popdown();
                if let Some(panel) = weak.upgrade() {
                    if let Some(tracker) = panel.tracker.borrow_mut().as_mut() {
                        let _ = tracker.send(Command::Close(window_id));
                    }
                }
            });
            row.append(&close);
            tile.append(&row);
            cards.append(&tile);
        }
        if !pictures.is_empty() {
            let weak = Rc::downgrade(self);
            let content = column.downgrade();
            let scroll = scroll.downgrade();
            let popup = card.downgrade();
            // Each popup owns its worker; unmapping cancels it independently of
            // another popup opening during deferred teardown.
            let request = Rc::new(RefCell::new(None::<crate::thumbnails::Request>));
            let cancel = request.clone();
            let cache_owner = Rc::downgrade(self);
            column.connect_unmap(move |_| {
                cancel.borrow_mut().take();
                let owner = cache_owner.clone();
                glib::timeout_add_local_once(std::time::Duration::from_secs(30), move || {
                    if let Some(panel) = owner.upgrade() {
                        panel
                            .thumbnail_cache
                            .borrow_mut()
                            .retain(|_, (at, _)| at.elapsed().as_secs() < 30);
                    }
                });
            });
            glib::spawn_future_local(async move {
                let mut receiver = None::<async_channel::Receiver<crate::thumbnails::Image>>;
                let mut active = Vec::new();
                let paint = |picture: &gtk::Picture, image: &crate::thumbnails::Image| {
                    let bytes = glib::Bytes::from_owned(image.pixels.clone());
                    let texture = gtk::gdk::MemoryTexture::new(
                        image.width,
                        image.height,
                        gtk::gdk::MemoryFormat::R8g8b8a8Premultiplied,
                        &bytes,
                        image.width as usize * 4,
                    );
                    picture.set_paintable(Some(&texture));
                };
                loop {
                    glib::timeout_future(std::time::Duration::from_millis(100)).await;
                    let (Some(panel), Some(content), Some(scroll)) =
                        (weak.upgrade(), content.upgrade(), scroll.upgrade())
                    else {
                        break;
                    };
                    if !content.is_mapped() || !panel.previews_enabled.get() {
                        break;
                    }
                    let all = panel
                        .taskbar
                        .borrow()
                        .groups()
                        .into_iter()
                        .flat_map(|g| g.windows)
                        .collect::<Vec<_>>();
                    if pictures.iter().any(|(window, _)| {
                        !all.iter().any(|w| {
                            w.id == window.id
                                && w.app_id == window.app_id
                                && w.title == window.title
                        })
                    }) {
                        if let Some(popup) = popup.upgrade() {
                            popup.popdown();
                        }
                        break;
                    }
                    let valid = |window: &uflinux_desktop::taskbar::Window| {
                        all.iter().any(|w| {
                            w.id == window.id
                                && w.app_id == window.app_id
                                && w.title == window.title
                        }) && all
                            .iter()
                            .filter(|w| w.app_id == window.app_id && w.title == window.title)
                            .count()
                            == 1
                    };
                    let mut visible = Vec::new();
                    for (window, picture) in &pictures {
                        let Some(picture) = picture.upgrade() else {
                            continue;
                        };
                        if !valid(window) {
                            picture.set_paintable(None::<&gtk::gdk::Texture>);
                            continue;
                        }
                        if picture
                            .parent()
                            .and_then(|stack| stack.compute_bounds(&scroll))
                            .is_some_and(|r| {
                                r.x() < scroll.width() as f32 && r.x() + r.width() > 0.0
                            })
                        {
                            visible.push(window.clone());
                            if let Some((at, image)) = panel
                                .thumbnail_cache
                                .borrow_mut()
                                .get_mut(&window.id)
                                .filter(|(at, image)| {
                                    at.elapsed().as_secs() < 30
                                        && image.app_id == window.app_id
                                        && image.title == window.title
                                })
                            {
                                *at = std::time::Instant::now();
                                if picture.paintable().is_none() {
                                    paint(&picture, image);
                                }
                            }
                        } else {
                            picture.set_paintable(None::<&gtk::gdk::Texture>);
                        }
                    }
                    let ids = visible.iter().map(|w| w.id).collect::<Vec<_>>();
                    if ids != active {
                        request.borrow_mut().take();
                        receiver = None;
                        if !visible.is_empty() {
                            let (worker, rx) = crate::thumbnails::start(visible);
                            request.replace(Some(worker));
                            receiver = Some(rx);
                        }
                        active = ids;
                    }
                    if let Some(rx) = &receiver {
                        while let Ok(image) = rx.try_recv() {
                            if let Some((window, picture)) = pictures.iter().find(|(w, _)| {
                                w.id == image.id
                                    && w.app_id == image.app_id
                                    && w.title == image.title
                            }) {
                                if valid(window) {
                                    if let Some(picture) = picture.upgrade() {
                                        paint(&picture, &image);
                                    }
                                    let mut cache = panel.thumbnail_cache.borrow_mut();
                                    cache.retain(|_, (at, _)| at.elapsed().as_secs() < 30);
                                    while cache.len() >= 64 {
                                        if let Some(id) = cache
                                            .iter()
                                            .min_by_key(|(_, (at, _))| *at)
                                            .map(|(id, _)| *id)
                                        {
                                            cache.remove(&id);
                                        } else {
                                            break;
                                        }
                                    }
                                    cache.insert(image.id, (std::time::Instant::now(), image));
                                }
                            }
                        }
                    }
                }
                request.borrow_mut().take();
            });
        }
        // The pointer has to be able to cross from the task button into the
        // card, so the card gets its own copy of the leave countdown.
        let motion = gtk::EventControllerMotion::new();
        let grace_enter = grace.clone();
        motion.connect_enter(move |_, _, _| {
            if let Some(id) = grace_enter.take() {
                id.remove();
            }
        });
        let card_leave = card.clone();
        let grace_leave = grace.clone();
        motion.connect_leave(move |_| arm_preview_grace(&grace_leave, &card_leave));
        column.add_controller(motion);
        Some(column)
    }

    fn sync(self: &Rc<Self>, monitors: &gio::ListModel) {
        let current: Vec<_> = (0..monitors.n_items())
            .filter_map(|i| monitors.item(i)?.downcast::<gtk::gdk::Monitor>().ok())
            // A display whose compositor connection broke mid-hotplug keeps
            // the old monitor objects in the list, marked invalid, until the
            // whole list is rebuilt. Building a surface for one of those
            // addresses a wl_output the compositor already destroyed, which
            // the protocol answers by killing the client.
            .filter(|monitor| monitor.is_valid())
            .collect();
        self.surfaces.borrow_mut().retain(|surface| {
            let keep = current.contains(&surface.monitor);
            if !keep {
                for item in surface.tray_buttons.values() {
                    item.popover.unparent();
                }
                surface.overflow_popover.unparent();
                surface.window.close();
            }
            keep
        });
        for monitor in current {
            if self.surfaces.borrow().iter().any(|s| s.monitor == monitor) {
                continue;
            }
            if let Some(surface) = self.create(&monitor) {
                self.surfaces.borrow_mut().push(surface);
            }
        }
        self.render();
        self.render_tray();
    }
    fn create(self: &Rc<Self>, monitor: &gtk::gdk::Monitor) -> Option<Surface> {
        let app = self.app.upgrade()?;
        let window = gtk::ApplicationWindow::builder()
            .application(&app.application)
            .title("UFLinux panel")
            .decorated(false)
            .build();
        if gtk4_layer_shell::is_supported() {
            window.init_layer_shell();
            window.set_namespace(Some("uflinux-desktop-panel"));
            window.set_layer(Layer::Top);
            window.set_monitor(Some(monitor));
            window.set_anchor(Edge::Bottom, true);
            window.set_anchor(Edge::Left, true);
            window.set_anchor(Edge::Right, true);
            window.set_exclusive_zone(crate::popup::PANEL_HEIGHT);
            window.set_keyboard_mode(KeyboardMode::None);
        }
        window.add_css_class("uflinux-panel");
        window.connect_realize(|window| {
            if window
                .renderer()
                .is_some_and(|renderer| renderer.is::<gtk::gsk::CairoRenderer>())
            {
                window.add_css_class("software-renderer");
            }
        });
        window.set_height_request(crate::popup::PANEL_HEIGHT);
        let root = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        let start = gtk::Button::new();
        let start_content = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let logo = gtk::Image::new();
        logo.set_pixel_size(LOGO_PIXELS);
        // The installed logo is a scalable document whose intrinsic size is far
        // larger than the panel. Decoding it at the pixel size actually shown
        // keeps the texture tiny; a file paintable would ignore pixel size.
        let scale = window.scale_factor().max(1);
        match gdk_pixbuf::Pixbuf::from_file_at_scale(
            "/usr/share/icons/hicolor/scalable/apps/uflinux.svg",
            LOGO_PIXELS * scale,
            LOGO_PIXELS * scale,
            true,
        ) {
            Ok(pixbuf) => logo.set_paintable(Some(&gtk::gdk::Texture::for_pixbuf(&pixbuf))),
            Err(error) => eprintln!("Could not load the panel logo: {error}"),
        }
        start_content.append(&logo);
        start_content.append(&gtk::Label::new(Some("Menu")));
        start.set_child(Some(&start_content));
        start.set_tooltip_text(Some("Start"));
        let weak = self.app.clone();
        let output = monitor
            .connector()
            .map(|s| s.to_string())
            .unwrap_or_default();
        start.connect_clicked(move |_| {
            if let Some(app) = weak.upgrade() {
                app.toggle(&output);
            }
        });
        root.append(&start);
        let tasks = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        root.append(&tasks);
        let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        spacer.set_hexpand(true);
        root.append(&spacer);
        // StatusNotifier icons live behind a single chevron, as on Windows, so a
        // dozen background apps do not push the clock off a small panel. The
        // chevron stays hidden until an item registers.
        let grid = gtk::Grid::new();
        grid.set_column_homogeneous(true);
        grid.set_row_homogeneous(true);
        grid.set_column_spacing(2);
        grid.set_row_spacing(2);
        grid.add_css_class("uflinux-tray-flyout");
        let scroll = gtk::ScrolledWindow::builder()
            .child(&grid)
            // The grid is at most TRAY_MAX_COLUMNS wide, so it always fits
            // horizontally and only ever needs to scroll vertically. An
            // External/Always policy here would clip the last column instead.
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vscrollbar_policy(gtk::PolicyType::Automatic)
            .propagate_natural_width(true)
            .propagate_natural_height(true)
            .max_content_height(TRAY_FLYOUT_MAX_HEIGHT)
            .build();
        scroll.add_css_class("uflinux-tray-scroll");
        let overflow_popover = gtk::Popover::builder()
            .child(&scroll)
            .position(gtk::PositionType::Top)
            .has_arrow(false)
            .build();
        overflow_popover.add_css_class("uflinux-tray-popover");
        // A plain button rather than a MenuButton: GTK exposes a MenuButton as
        // a combo box with no accessibility action, which would leave the
        // chevron unreachable to assistive tech and to the a11y test harness.
        let overflow = gtk::Button::from_icon_name("pan-up-symbolic");
        overflow.add_css_class("uflinux-tray-chevron");
        overflow.set_tooltip_text(Some("Hidden icons"));
        overflow_popover.set_parent(&overflow);
        overflow.set_visible(false);
        let weak = self.app.clone();
        let output = monitor
            .connector()
            .map(|s| s.to_string())
            .unwrap_or_default();
        let target = overflow_popover.downgrade();
        overflow_popover.connect_show(move |_| {
            if let (Some(app), Some(popover)) = (weak.upgrade(), target.upgrade()) {
                app.show_tray(&popover, &output);
            }
        });
        let weak = self.app.clone();
        overflow_popover.connect_hide(move |_| {
            if let Some(app) = weak.upgrade() {
                app.tray_closed();
            }
        });
        let open = overflow.clone();
        overflow_popover.connect_show(move |_| open.add_css_class("open"));
        let closed = overflow.clone();
        overflow_popover.connect_hide(move |_| closed.remove_css_class("open"));
        let toggle = overflow_popover.clone();
        overflow.connect_clicked(move |_| {
            if toggle.is_visible() {
                toggle.popdown();
            } else {
                toggle.popup();
            }
        });
        root.append(&overflow);
        let overflow_columns = Self::flyout_columns(monitor);
        scroll.set_max_content_width(overflow_columns * TRAY_CELL);
        for (index, name, fallback) in [
            (0, "network", "network-offline-symbolic"),
            (1, "sound", "audio-volume-muted-symbolic"),
            (2, "power", "thunderbolt-symbolic"),
        ] {
            let button = gtk::Button::from_icon_name(fallback);
            button.set_tooltip_text(Some(name));
            let target = button.downgrade();
            app.quick.bind_for(
                uflinux_settings::app::Surface::Full,
                &button,
                move |snapshot| {
                    if let Some(button) = target.upgrade() {
                        let values = settings_core::panel::status(snapshot);
                        button.set_icon_name(&values[index].0);
                        button.set_tooltip_text(Some(&values[index].1));
                        button
                            .update_property(&[gtk::accessible::Property::Label(&values[index].1)]);
                    }
                },
            );
            let weak = self.app.clone();
            let output = monitor
                .connector()
                .map(|s| s.to_string())
                .unwrap_or_default();
            button.connect_clicked(move |_| {
                if let Some(app) = weak.upgrade() {
                    app.open_quick(name, &output);
                }
            });
            root.append(&button);
        }
        let clock_label = gtk::Label::new(None);
        clock_label.add_css_class("desktop-clock");
        let clock_time = gtk::Label::new(None);
        if let Ok(now) = glib::DateTime::now_local() {
            if let Ok(text) = now.format("%H:%M\n%d/%m/%Y") {
                clock_label.set_text(&text);
            }
            if let Ok(text) = now.format("%H:%M") {
                clock_time.set_text(&text);
            }
        }
        let weak_panel = Rc::downgrade(self);
        let last_zone = RefCell::new(String::new());
        app.quick.bind_for(
            uflinux_settings::app::Surface::Full,
            &clock_label,
            move |snapshot| {
                if *last_zone.borrow() != snapshot.datetime.timezone {
                    last_zone.replace(snapshot.datetime.timezone.clone());
                    if let Some(panel) = weak_panel.upgrade() {
                        panel.update_clock();
                    }
                }
            },
        );
        let button = gtk::MenuButton::new();
        button.set_child(Some(&clock_label));
        button.set_tooltip_text(Some("Date and time"));
        // GNOME-style clock popover: date header, month-navigable calendar and
        // a notification centre fed by Mako only while the popover is open.
        let clock = gtk::Popover::builder()
            .has_arrow(false)
            .position(gtk::PositionType::Top)
            .build();
        clock.add_css_class("uflinux-clock-popover");
        let clock_card = gtk::Box::new(gtk::Orientation::Vertical, 8);
        clock_card.add_css_class("uflinux-popup-card");
        clock_card.add_css_class("uflinux-clock-card");
        clock_card.set_size_request(360, -1);
        let today = glib::DateTime::now_local().ok();
        let format_date = |date: &glib::DateTime| {
            date.format("%A, %d %B %Y")
                .ok()
                .map(|text| text.to_string())
                .unwrap_or_else(|| "Calendar".to_string())
        };
        let clock_header = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let heading = gtk::Label::new(Some(
            &today
                .as_ref()
                .map_or_else(|| "Calendar".to_string(), &format_date),
        ));
        heading.add_css_class("uflinux-clock-heading");
        heading.set_hexpand(true);
        heading.set_xalign(0.0);
        clock_header.append(&heading);
        clock_time.add_css_class("uflinux-clock-time");
        clock_header.append(&clock_time);
        clock_card.append(&clock_header);
        let nav_row = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        let previous = gtk::Button::from_icon_name("pan-start-symbolic");
        previous.set_tooltip_text(Some("Previous month"));
        previous.add_css_class("flat");
        // The Calendar heading is hidden in favour of the popover header;
        // month navigation is driven manually because the Calendar month
        // setters need the gtk v4_14 feature, which this image lacks.
        let calendar = gtk::Calendar::builder()
            .show_heading(false)
            .show_day_names(true)
            .build();
        if let Some(today) = today.as_ref() {
            calendar.set_date(Some(today));
        }
        calendar.set_hexpand(true);
        let next = gtk::Button::from_icon_name("pan-end-symbolic");
        next.set_tooltip_text(Some("Next month"));
        next.add_css_class("flat");
        let today_button = gtk::Button::with_label("Today");
        today_button.add_css_class("flat");
        nav_row.append(&previous);
        nav_row.append(&calendar);
        nav_row.append(&next);
        nav_row.append(&today_button);
        clock_card.append(&nav_row);
        // Displayed month as (year, month from zero); the Calendar date
        // getter returns the selected day, so the buttons track it here.
        let month_state = std::rc::Rc::new(std::cell::Cell::new((
            today.as_ref().map_or(2025, |date| date.year()),
            today.as_ref().map_or(0, |date| date.month() as i32 - 1),
        )));
        let show_date = {
            let calendar = calendar.clone();
            let heading = heading.clone();
            let month_state = month_state.clone();
            move |year: i32, month: i32, day: i32| {
                let Ok(date) =
                    glib::DateTime::new(&glib::TimeZone::local(), year, month + 1, day, 0, 0, 0.0)
                else {
                    return;
                };
                month_state.set((year, month));
                calendar.set_date(Some(&date));
                heading.set_label(&format_date(&date));
            }
        };
        {
            let show_date = show_date.clone();
            let month_state = month_state.clone();
            previous.connect_clicked(move |_| {
                let (year, month) = month_state.get();
                let (year, month) = if month == 0 {
                    (year - 1, 11)
                } else {
                    (year, month - 1)
                };
                show_date(year, month, 1);
            });
        }
        {
            let show_date = show_date.clone();
            let month_state = month_state.clone();
            next.connect_clicked(move |_| {
                let (year, month) = month_state.get();
                let (year, month) = if month == 11 {
                    (year + 1, 0)
                } else {
                    (year, month + 1)
                };
                show_date(year, month, 1);
            });
        }
        today_button.connect_clicked(move |_| {
            let Some(now) = glib::DateTime::now_local().ok() else {
                return;
            };
            show_date(now.year(), now.month() as i32 - 1, now.day_of_month());
        });
        calendar.connect_day_selected({
            let calendar = calendar.clone();
            let heading = heading.clone();
            let month_state = month_state.clone();
            move |_| {
                // Clicking an adjacent-month day moves the selection there.
                let date = calendar.date();
                month_state.set((date.year(), date.month() as i32 - 1));
                heading.set_label(&format_date(&date));
            }
        });
        clock_card.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        let section = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let section_title = gtk::Label::new(Some("Notifications"));
        section_title.add_css_class("uflinux-settings-heading");
        section_title.set_hexpand(true);
        section_title.set_xalign(0.0);
        section.append(&section_title);
        let notification_list = gtk::Box::new(gtk::Orientation::Vertical, 4);
        let placeholder = gtk::Label::new(Some("No notifications"));
        placeholder.add_css_class("dim-label");
        notification_list.append(&placeholder);
        // The dismiss callbacks need the centre, which needs the callback.
        let center_ref: std::rc::Rc<std::cell::RefCell<Option<notifications::Center>>> =
            std::rc::Rc::new(std::cell::RefCell::new(None));
        let center = notifications::Center::new({
            let notification_list = notification_list.clone();
            let placeholder = placeholder.clone();
            let center_ref = center_ref.clone();
            Box::new(
                move |event: std::result::Result<Vec<notifications::Notification>, String>| {
                    while let Some(child) = notification_list.first_child() {
                        notification_list.remove(&child);
                    }
                    let unavailable = event.is_err();
                    let items = event.unwrap_or_default();
                    placeholder.set_text(if unavailable {
                        "Notifications unavailable"
                    } else {
                        "No notifications"
                    });
                    placeholder.set_visible(items.is_empty());
                    notification_list.append(&placeholder);
                    let center = center_ref.borrow().clone();
                    for item in items {
                        let notification = gtk::Box::new(gtk::Orientation::Horizontal, 8);
                        notification.add_css_class("uflinux-notification");
                        let icon = gtk::Image::from_icon_name(if item.icon.is_empty() {
                            "dialog-information-symbolic"
                        } else {
                            &item.icon
                        });
                        icon.set_pixel_size(24);
                        notification.append(&icon);
                        let text = gtk::Box::new(gtk::Orientation::Vertical, 0);
                        text.set_hexpand(true);
                        let name = gtk::Label::new(Some(if item.title.is_empty() {
                            &item.app
                        } else {
                            &item.title
                        }));
                        name.set_xalign(0.0);
                        name.set_wrap(true);
                        name.set_lines(2);
                        name.set_ellipsize(gtk::pango::EllipsizeMode::End);
                        text.append(&name);
                        if !item.body.is_empty() {
                            let body = gtk::Label::new(Some(&item.body));
                            body.add_css_class("dim-label");
                            body.set_xalign(0.0);
                            body.set_wrap(true);
                            body.set_lines(2);
                            body.set_ellipsize(gtk::pango::EllipsizeMode::End);
                            text.append(&body);
                        }
                        notification.append(&text);
                        let dismiss = gtk::Button::from_icon_name("window-close-symbolic");
                        dismiss.add_css_class("flat");
                        dismiss.set_tooltip_text(Some("Dismiss"));
                        if let Some(center) = center.clone() {
                            let center = center.clone();
                            let id = item.id;
                            dismiss.connect_clicked(move |_| center.dismiss(id));
                        }
                        notification_list.append(&notification);
                    }
                },
            )
        });
        center_ref.replace(Some(center.clone()));
        let clear = gtk::Button::with_label("Clear all");
        clear.add_css_class("flat");
        let center_clear = center.clone();
        clear.connect_clicked(move |_| center_clear.clear());
        section.append(&clear);
        clock_card.append(&section);
        let notification_scroll = gtk::ScrolledWindow::builder()
            .child(&notification_list)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vscrollbar_policy(gtk::PolicyType::Automatic)
            .propagate_natural_width(true)
            .propagate_natural_height(true)
            .max_content_height(240)
            .build();
        clock_card.append(&notification_scroll);
        clock.set_child(Some(&clock_card));
        button.set_popover(Some(&clock));
        let weak = self.app.clone();
        let clock_weak = clock.downgrade();
        let output = monitor
            .connector()
            .map(|s| s.to_string())
            .unwrap_or_default();
        let center_show = center.clone();
        clock.connect_show(move |_| {
            if let Some(app) = weak.upgrade() {
                if let Some(clock) = clock_weak.upgrade() {
                    app.show_clock(&clock, &output);
                }
            }
            center_show.start();
        });
        let weak = self.app.clone();
        clock.connect_hide(move |_| {
            center.stop();
            if let Some(app) = weak.upgrade() {
                app.clock_closed();
            }
        });
        root.append(&button);
        window.set_child(Some(&root));
        window.present();
        Some(Surface {
            overflow,
            overflow_popover,
            overflow_scroll: scroll,
            overflow_grid: grid,
            overflow_columns,
            tray_buttons: HashMap::new(),
            monitor: monitor.clone(),
            window,
            tasks,
            buttons: HashMap::new(),
            clock,
            clock_label,
            clock_time,
        })
    }
}
