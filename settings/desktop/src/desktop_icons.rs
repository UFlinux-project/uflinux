//! Desktop1 client. The file manager owns the files and their placements.
use glib::variant::ToVariant;
use gtk::{cairo, gdk, prelude::*};
use gtk4_layer_shell::{KeyboardMode, LayerShell};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, VecDeque},
    path::Path,
    rc::Rc,
    time::Duration,
};

const NAME: &str = "org.uflinux.FileManager.Desktop1";
const PATH: &str = "/org/uflinux/FileManager/Desktop";
const RENDERER: &str = "org.uflinux.FileManager.Desktop.Renderer";
const DBUS: &str = "org.freedesktop.DBus";
const DBUS_PATH: &str = "/org/freedesktop/DBus";
const CELL: i32 = 96;
const INSET: i32 = 8;
const ICON: i32 = 48;
const CALL_TIMEOUT: i32 = 2000;

#[derive(Clone)]
pub struct Output {
    pub connector: String,
    pub window: gtk::ApplicationWindow,
    pub grid: gtk::Fixed,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
struct Geometry {
    monitor: u8,
    columns: u16,
    rows: u16,
    cell_w: i32,
    cell_h: i32,
}

impl Geometry {
    fn for_size(monitor: u8, width: i32, height: i32) -> Option<Self> {
        let columns = ((width - INSET) / CELL).min(64);
        let rows = ((height - INSET) / CELL).min(512);
        (columns > 0 && rows > 0).then_some(Self {
            monitor,
            columns: columns as u16,
            rows: rows as u16,
            cell_w: CELL,
            cell_h: CELL,
        })
    }

    fn contains(self, cell: Slot) -> bool {
        cell.monitor == self.monitor && cell.col < self.columns && cell.row < self.rows
    }

    fn slot(self, x: f64, y: f64) -> Option<Slot> {
        let col = ((x - f64::from(INSET)) / f64::from(CELL)).floor();
        let row = ((y - f64::from(INSET)) / f64::from(CELL)).floor();
        if !(0.0..f64::from(self.columns)).contains(&col)
            || !(0.0..f64::from(self.rows)).contains(&row)
        {
            return None;
        }
        Some(Slot {
            monitor: self.monitor,
            col: col as u16,
            row: row as u16,
        })
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
struct Slot {
    monitor: u8,
    col: u16,
    row: u16,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
struct Item {
    id: String,
    name: String,
    #[serde(default)]
    icon: String,
    #[serde(default)]
    kind: Kind,
    tooltip: Option<String>,
    at: Option<Slot>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum Kind {
    #[default]
    File,
    Directory,
    Launcher,
}

impl Kind {
    fn fallback(self) -> &'static str {
        match self {
            Self::File => "text-x-generic",
            Self::Directory => "folder",
            Self::Launcher => "application-x-executable",
        }
    }
}

#[derive(Deserialize)]
#[serde(tag = "reply", rename_all = "lowercase")]
enum Reply {
    Ok,
    Model {
        version: u32,
        generation: u64,
        desktop: String,
        #[serde(default)]
        writable: bool,
        #[serde(default)]
        thumbnails: bool,
        thumbnail_size: Option<u32>,
        #[serde(default)]
        items: Vec<Item>,
    },
    Thumbnail {
        format: String,
        fd: Option<i32>,
    },
}

struct Tile {
    item: Item,
    grid: gtk::Fixed,
    widget: gtk::Box,
    thumbnail: bool,
    writable: bool,
}

impl Drop for Tile {
    fn drop(&mut self) {
        self.grid.remove(&self.widget);
    }
}

struct Thumbnail {
    image: glib::WeakRef<gtk::Image>,
    path: String,
    request_size: u32,
    display_size: i32,
}

type Subscription = (gio::DBusConnection, gio::SignalSubscriptionId);

#[derive(Default)]
pub struct DesktopIcons {
    bus: RefCell<Option<gio::DBusConnection>>,
    subscriptions: RefCell<Vec<Subscription>>,
    outputs: RefCell<Vec<Output>>,
    monitor_ids: RefCell<HashMap<String, u8>>,
    grids: RefCell<Vec<Geometry>>,
    sent: RefCell<Vec<Geometry>>,
    generation: Cell<Option<u64>>,
    epoch: Cell<u64>,
    timer: RefCell<Option<glib::SourceId>>,
    busy: Cell<bool>,
    dirty: Cell<bool>,
    tiles: RefCell<HashMap<String, Tile>>,
    thumbnails: RefCell<VecDeque<Thumbnail>>,
    decoding: Cell<usize>,
}

impl DesktopIcons {
    pub fn connect(self: &Rc<Self>) {
        if !gtk4_layer_shell::is_supported() {
            return;
        }
        let weak = Rc::downgrade(self);
        glib::MainContext::default().spawn_local(async move {
            let Ok(bus) = gio::bus_get_future(gio::BusType::Session).await else {
                return;
            };
            // REPLACE_EXISTING | DO_NOT_QUEUE. The Files fallback allows replacement.
            let Ok(reply) = bus
                .call_future(
                    Some(DBUS),
                    DBUS_PATH,
                    DBUS,
                    "RequestName",
                    Some(&(RENDERER, 6u32).to_variant()),
                    None,
                    gio::DBusCallFlags::NONE,
                    CALL_TIMEOUT,
                )
                .await
            else {
                return;
            };
            if !matches!(reply.get::<(u32,)>(), Some((1 | 4,))) {
                return;
            }
            if let Some(icons) = weak.upgrade() {
                icons.subscribe(&bus);
                icons.bus.replace(Some(bus));
                icons.schedule();
            }
        });
    }

    fn subscribe(self: &Rc<Self>, bus: &gio::DBusConnection) {
        let weak = Rc::downgrade(self);
        let changed = bus.signal_subscribe(
            Some(NAME),
            Some(NAME),
            Some("Changed"),
            Some(PATH),
            None,
            gio::DBusSignalFlags::NONE,
            move |_, _, _, _, _, parameters| {
                #[derive(Deserialize)]
                struct Change {
                    generation: u64,
                }
                let Some(icons) = weak.upgrade() else { return };
                let Some((body,)) = parameters.get::<(String,)>() else {
                    return;
                };
                let Ok(change) = serde_json::from_str::<Change>(&body) else {
                    return;
                };
                if icons
                    .generation
                    .get()
                    .is_none_or(|drawn| change.generation > drawn)
                {
                    icons.schedule();
                }
            },
        );
        let weak = Rc::downgrade(self);
        let owner = bus.signal_subscribe(
            Some(DBUS),
            Some(DBUS),
            Some("NameOwnerChanged"),
            Some(DBUS_PATH),
            Some(NAME),
            gio::DBusSignalFlags::NONE,
            move |_, _, _, _, _, parameters| {
                let Some(icons) = weak.upgrade() else { return };
                let Some((_, previous, current)) = parameters.get::<(String, String, String)>()
                else {
                    return;
                };
                if !previous.is_empty() {
                    icons.epoch.set(icons.epoch.get().wrapping_add(1));
                    icons.generation.set(None);
                    icons.sent.borrow_mut().clear();
                    icons.tiles.borrow_mut().clear();
                    icons.update_input_regions();
                    icons.thumbnails.borrow_mut().clear();
                }
                if !current.is_empty() || !previous.is_empty() {
                    icons.schedule();
                }
            },
        );
        self.subscriptions
            .borrow_mut()
            .extend([(bus.clone(), changed), (bus.clone(), owner)]);
    }

    pub fn set_outputs(self: &Rc<Self>, outputs: Vec<Output>) {
        self.outputs.replace(outputs);
        self.schedule();
    }

    pub fn schedule(self: &Rc<Self>) {
        if self.bus.borrow().is_none() {
            return;
        }
        let mut timer = self.timer.borrow_mut();
        if let Some(old) = timer.take() {
            old.remove();
        }
        let weak = Rc::downgrade(self);
        *timer = Some(glib::timeout_add_local_once(
            Duration::from_millis(100),
            move || {
                if let Some(icons) = weak.upgrade() {
                    icons.timer.borrow_mut().take();
                    icons.refresh();
                }
            },
        ));
    }

    fn geometries(&self) -> Vec<Geometry> {
        let mut ids = self.monitor_ids.borrow_mut();
        let mut outputs = self.outputs.borrow().clone();
        outputs.sort_by(|a, b| a.connector.cmp(&b.connector));
        outputs
            .into_iter()
            .filter_map(|output| {
                let monitor = if let Some(id) = ids.get(&output.connector) {
                    *id
                } else {
                    let id = (0..=u8::MAX).find(|id| !ids.values().any(|used| used == id))?;
                    ids.insert(output.connector, id);
                    id
                };
                Geometry::for_size(monitor, output.window.width(), output.window.height())
            })
            .take(8)
            .collect()
    }

    fn refresh(self: &Rc<Self>) {
        let grids = self.geometries();
        self.grids.replace(grids.clone());
        self.tiles.borrow_mut().retain(|_, tile| {
            self.outputs
                .borrow()
                .iter()
                .any(|output| output.grid == tile.grid)
                && tile
                    .item
                    .at
                    .is_some_and(|slot| grids.iter().any(|g| g.contains(slot)))
        });
        self.update_input_regions();
        if self.busy.get() {
            self.dirty.set(true);
            return;
        }
        let Some(bus) = self.bus.borrow().clone() else {
            return;
        };
        if grids.is_empty() {
            return;
        }
        self.busy.set(true);
        let send_geometry = *self.sent.borrow() != grids;
        let epoch = self.epoch.get();
        let weak = Rc::downgrade(self);
        glib::MainContext::default().spawn_local(async move {
            let geometry_ok = !send_geometry
                || matches!(
                    call(
                        &bus,
                        "SetGeometry",
                        json!({"method":"set-geometry","monitors":grids})
                    )
                    .await,
                    Ok(Reply::Ok)
                );
            let model = if geometry_ok {
                call(&bus, "GetModel", json!({"method":"get-model"}))
                    .await
                    .ok()
            } else {
                None
            };
            let Some(icons) = weak.upgrade() else { return };
            icons.busy.set(false);
            if icons.epoch.get() == epoch {
                if geometry_ok {
                    icons.sent.replace(grids.clone());
                }
                if *icons.grids.borrow() == grids {
                    if let Some(model) = model {
                        icons.paint(model);
                    }
                }
            }
            if icons.dirty.replace(false) {
                icons.schedule();
            }
        });
    }

    fn paint(self: &Rc<Self>, reply: Reply) {
        let Reply::Model {
            version: 1,
            generation,
            desktop,
            writable,
            thumbnails,
            thumbnail_size,
            items,
        } = reply
        else {
            return;
        };
        if self
            .generation
            .get()
            .is_some_and(|drawn| generation < drawn)
        {
            return;
        }
        self.generation.set(Some(generation));
        let grids = self.grids.borrow();
        let ids = self.monitor_ids.borrow();
        let outputs = self.outputs.borrow();
        let mut previous = std::mem::take(&mut *self.tiles.borrow_mut());
        let mut next = HashMap::new();
        for item in items.into_iter().take(2048) {
            let Some(slot) = item
                .at
                .filter(|&slot| grids.iter().any(|grid| grid.contains(slot)))
            else {
                continue;
            };
            let Some(output) = outputs
                .iter()
                .find(|output| ids.get(&output.connector) == Some(&slot.monitor))
            else {
                continue;
            };
            let mut components = Path::new(&item.id).components();
            if !matches!(components.next(), Some(std::path::Component::Normal(name)) if name == item.id.as_str())
                || components.next().is_some()
                || next.contains_key(&item.id)
            {
                continue;
            }
            let old = previous.remove(&item.id);
            let tile = match old {
                Some(tile)
                    if tile.item == item
                        && tile.grid == output.grid
                        && tile.thumbnail == thumbnails
                        && tile.writable == writable =>
                {
                    tile
                }
                _ => self.create_tile(
                    item,
                    slot,
                    output,
                    writable,
                    thumbnails.then_some((&desktop, thumbnail_size.unwrap_or(128))),
                ),
            };
            next.insert(tile.item.id.clone(), tile);
        }
        self.tiles.replace(next);
        drop(previous);
        self.update_input_regions();
        self.load_thumbnails();
    }

    fn update_input_regions(&self) {
        let tiles = self.tiles.borrow();
        for output in self.outputs.borrow().iter() {
            let Some(surface) = output.window.surface() else {
                continue;
            };
            let rectangles: Vec<_> = tiles
                .values()
                .filter(|tile| tile.grid == output.grid)
                .filter_map(|tile| tile.item.at)
                .map(|slot| {
                    cairo::RectangleInt::new(
                        INSET + i32::from(slot.col) * CELL,
                        INSET + i32::from(slot.row) * CELL,
                        CELL,
                        CELL,
                    )
                })
                .collect();
            let keyboard = if rectangles.is_empty() {
                KeyboardMode::None
            } else {
                KeyboardMode::OnDemand
            };
            output.window.set_keyboard_mode(keyboard);
            surface.set_input_region(Some(&cairo::Region::create_rectangles(&rectangles)));
        }
    }

    fn create_tile(
        self: &Rc<Self>,
        item: Item,
        slot: Slot,
        output: &Output,
        writable: bool,
        thumbnail: Option<(&String, u32)>,
    ) -> Tile {
        let name = if item.name.is_empty() {
            &item.id
        } else {
            &item.name
        };
        let widget = gtk::Box::new(gtk::Orientation::Vertical, 2);
        widget.add_css_class("desktop-icon");
        widget.set_focusable(true);
        widget.set_size_request(CELL, CELL);
        widget.set_valign(gtk::Align::Center);
        widget.set_tooltip_text(Some(item.tooltip.as_deref().unwrap_or(name)));
        widget.update_property(&[gtk::accessible::Property::Label(name)]);
        let image = gtk::Image::new();
        image.set_pixel_size(ICON);
        if item.icon.starts_with('/') {
            let icon = gio::FileIcon::new(&gio::File::for_path(&item.icon));
            image.set_from_gicon(&icon);
        } else {
            let icon = if item.icon.is_empty() {
                gio::ThemedIcon::new(item.kind.fallback())
            } else {
                gio::ThemedIcon::from_names(&[item.icon.as_str(), item.kind.fallback()])
            };
            image.set_from_gicon(&icon);
        }
        widget.append(&image);
        let label = gtk::Label::new(Some(name));
        label.set_wrap(true);
        label.set_lines(2);
        label.set_max_width_chars(10);
        label.set_ellipsize(gtk::pango::EllipsizeMode::End);
        widget.append(&label);
        let x = f64::from(INSET + i32::from(slot.col) * CELL);
        let y = f64::from(INSET + i32::from(slot.row) * CELL);
        output.grid.put(&widget, x, y);

        let click = gtk::GestureClick::new();
        click.set_button(1);
        let id = item.id.clone();
        let weak = Rc::downgrade(self);
        click.connect_pressed(move |gesture, _, _, _| {
            if let Some(widget) = gesture.widget() {
                widget.grab_focus();
            }
        });
        click.connect_released(move |_, count, _, _| {
            if count == 2 {
                if let Some(icons) = weak.upgrade() {
                    icons.action("Launch", json!({"method":"launch","id":id}));
                }
            }
        });
        widget.add_controller(click);
        let keys = gtk::EventControllerKey::new();
        let id = item.id.clone();
        let weak = Rc::downgrade(self);
        keys.connect_key_pressed(move |_, key, _, _| {
            if matches!(key, gdk::Key::Return | gdk::Key::KP_Enter) {
                if let Some(icons) = weak.upgrade() {
                    icons.action("Launch", json!({"method":"launch","id":id}));
                }
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        widget.add_controller(keys);
        let context = gtk::GestureClick::new();
        context.set_button(3);
        let id = item.id.clone();
        let weak = Rc::downgrade(self);
        context.connect_pressed(move |_, count, _, _| {
            if count == 1 {
                if let Some(icons) = weak.upgrade() {
                    icons.action("ContextMenu", json!({"method":"context-menu","id":id}));
                }
            }
        });
        widget.add_controller(context);
        if writable {
            let drag = gtk::GestureDrag::new();
            drag.set_button(1);
            drag.connect_drag_update(|gesture, dx, dy| {
                if dx.abs().max(dy.abs()) >= 8.0 {
                    gesture.set_state(gtk::EventSequenceState::Claimed);
                }
            });
            let id = item.id.clone();
            let weak = Rc::downgrade(self);
            drag.connect_drag_end(move |gesture, dx, dy| {
                if dx.abs().max(dy.abs()) < 8.0 {
                    return;
                }
                let Some((sx, sy)) = gesture.start_point() else {
                    return;
                };
                let Some(icons) = weak.upgrade() else { return };
                let target = icons
                    .grids
                    .borrow()
                    .iter()
                    .find(|g| g.monitor == slot.monitor)
                    .and_then(|grid| grid.slot(x + sx + dx, y + sy + dy));
                if let Some(target) = target.filter(|target| *target != slot) {
                    icons.action(
                        "Place",
                        json!({
                            "method":"place", "id":id, "monitor":target.monitor,
                            "col":target.col, "row":target.row,
                        }),
                    );
                }
            });
            widget.add_controller(drag);
        }
        if let Some((desktop, size)) = thumbnail.filter(|_| item.kind == Kind::File) {
            let path = Path::new(desktop).join(&item.id);
            if path.is_absolute() {
                self.thumbnails.borrow_mut().push_back(Thumbnail {
                    image: image.downgrade(),
                    path: path.to_string_lossy().into_owned(),
                    request_size: size.clamp(128, 512),
                    display_size: (ICON * output.window.scale_factor()).clamp(ICON, 512),
                });
            }
        }
        Tile {
            item,
            grid: output.grid.clone(),
            widget,
            thumbnail: thumbnail.is_some(),
            writable,
        }
    }

    fn action(&self, method: &'static str, request: serde_json::Value) {
        let Some(bus) = self.bus.borrow().clone() else {
            return;
        };
        glib::MainContext::default().spawn_local(async move {
            if let Err(error) = call(&bus, method, request).await {
                eprintln!("Desktop icons {method}: {error}");
            }
        });
    }

    fn load_thumbnails(self: &Rc<Self>) {
        let Some(bus) = self.bus.borrow().clone() else {
            return;
        };
        while self.decoding.get() < 4 {
            let Some(job) = self.thumbnails.borrow_mut().pop_front() else {
                break;
            };
            if job.image.upgrade().is_none() {
                continue;
            }
            self.decoding.set(self.decoding.get() + 1);
            let weak = Rc::downgrade(self);
            let bus = bus.clone();
            glib::MainContext::default().spawn_local(async move {
                if let Some(picture) = thumbnail(&bus, &job).await {
                    if let Some(image) = job.image.upgrade() {
                        image.set_paintable(Some(&gdk::Texture::for_pixbuf(&picture)));
                    }
                }
                if let Some(icons) = weak.upgrade() {
                    icons.decoding.set(icons.decoding.get() - 1);
                    icons.load_thumbnails();
                }
            });
        }
    }
}

impl Drop for DesktopIcons {
    fn drop(&mut self) {
        if let Some(timer) = self.timer.get_mut().take() {
            timer.remove();
        }
        for (bus, id) in self.subscriptions.get_mut().drain(..) {
            bus.signal_unsubscribe(id);
        }
        if let Some(bus) = self.bus.get_mut().take() {
            bus.call(
                Some(DBUS),
                DBUS_PATH,
                DBUS,
                "ReleaseName",
                Some(&(RENDERER,).to_variant()),
                None,
                gio::DBusCallFlags::NONE,
                CALL_TIMEOUT,
                gio::Cancellable::NONE,
                |_| {},
            );
        }
    }
}

async fn call(
    bus: &gio::DBusConnection,
    method: &str,
    body: serde_json::Value,
) -> Result<Reply, String> {
    let result = bus
        .call_future(
            Some(NAME),
            PATH,
            NAME,
            method,
            Some(&(body.to_string(),).to_variant()),
            None,
            gio::DBusCallFlags::NONE,
            CALL_TIMEOUT,
        )
        .await
        .map_err(|error| error.to_string())?;
    decode(&result).ok_or_else(|| "Invalid desktop provider reply".into())
}

fn decode(reply: &glib::Variant) -> Option<Reply> {
    let (body,) = reply.get::<(String,)>()?;
    if body.len() > 2 * 1024 * 1024 {
        return None;
    }
    serde_json::from_str(&body).ok()
}

async fn thumbnail(bus: &gio::DBusConnection, job: &Thumbnail) -> Option<gdk_pixbuf::Pixbuf> {
    let body = json!({"method":"request-thumbnail","path":job.path,"size":job.request_size});
    let (reply, descriptors) = bus
        .call_with_unix_fd_list_future(
            Some(NAME),
            PATH,
            NAME,
            "RequestThumbnail",
            Some(&(body.to_string(),).to_variant()),
            None,
            gio::DBusCallFlags::NONE,
            CALL_TIMEOUT,
            None::<&gio::UnixFDList>,
        )
        .await
        .ok()?;
    let Reply::Thumbnail {
        format,
        fd: Some(index),
    } = decode(&reply)?
    else {
        return None;
    };
    if format != "png" {
        return None;
    }
    let stream = gio_unix::InputStream::take_fd(descriptors?.get(index).ok()?);
    gdk_pixbuf::Pixbuf::from_stream_at_scale_future(
        &stream,
        job.display_size,
        job.display_size,
        true,
    )
    .await
    .ok()
}
