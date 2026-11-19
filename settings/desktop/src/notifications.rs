// Mako notification center client.
//
// Mako owns org.freedesktop.Notifications on the session bus and exports its
// management interface at /fr/emersion/Mako. The notification list is a
// property that emits invalidation, so the client stays connected only while
// the calendar popover is open: it fetches the list on demand and refreshes
// when Mako signals a property change. No polling, no resident connection.
use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};

use glib::Variant;

const SERVICE: &str = "org.freedesktop.Notifications";
const PATH: &str = "/fr/emersion/Mako";
const INTERFACE: &str = "fr.emersion.Mako";
const TIMEOUT: i32 = 2000;

pub struct Notification {
    pub id: u32,
    pub app: String,
    pub icon: String,
    pub title: String,
    pub body: String,
}

#[derive(Clone)]
pub struct Center {
    connection: Rc<RefCell<Option<gio::DBusConnection>>>,
    subscription: Rc<RefCell<Option<gio::SignalSubscription>>>,
    shared: Rc<Shared>,
}

struct Shared {
    // Bumped on every start and stop, so a reply that arrives after the
    // popover closed is discarded instead of resurrecting the subscription.
    generation: Cell<u64>,
    listing: Cell<bool>,
    changed: Box<dyn Fn(std::result::Result<Vec<Notification>, String>)>,
}

impl Center {
    pub fn new(changed: impl Fn(std::result::Result<Vec<Notification>, String>) + 'static) -> Self {
        Self {
            connection: Rc::new(RefCell::new(None)),
            subscription: Rc::new(RefCell::new(None)),
            shared: Rc::new(Shared {
                generation: Cell::new(0),
                listing: Cell::new(false),
                changed: Box::new(changed),
            }),
        }
    }

    // Open the session bus connection and start tracking Mako. When Mako is
    // not running the center reports itself unavailable and retries on the
    // next open.
    pub fn start(&self) {
        let generation = self.shared.generation.get() + 1;
        self.shared.generation.set(generation);
        let connection = Rc::downgrade(&self.connection);
        let subscription = Rc::downgrade(&self.subscription);
        let shared = self.shared.clone();
        glib::spawn_future_local(async move {
            let Ok(bus) = gio::bus_get_future(gio::BusType::Session).await else {
                if shared.generation.get() == generation {
                    (shared.changed)(Err(String::new()));
                }
                return;
            };
            let Ok(reply) = call(&bus, "ListNotifications", None).await else {
                if shared.generation.get() == generation {
                    (shared.changed)(Err(String::new()));
                }
                return;
            };
            // The popover closed while the list was in flight; nothing has
            // been stored yet, so there is nothing to tear down either.
            if shared.generation.get() != generation {
                return;
            }
            let (Some(connection_cell), Some(subscription_cell)) =
                (connection.upgrade(), subscription.upgrade())
            else {
                return;
            };
            let watch_shared = Rc::downgrade(&shared);
            let handle = bus.subscribe_to_signal(
                Some(SERVICE),
                Some("org.freedesktop.DBus.Properties"),
                Some("PropertiesChanged"),
                Some(PATH),
                None,
                gio::DBusSignalFlags::NONE,
                move |_| {
                    if let Some(shared) = watch_shared.upgrade() {
                        refresh(connection.clone(), shared);
                    }
                },
            );
            connection_cell.replace(Some(bus));
            subscription_cell.replace(Some(handle));
            (shared.changed)(Ok(parse(&reply)));
        });
    }

    // Drop the bus connection and the property-change subscription.
    pub fn stop(&self) {
        self.shared.generation.set(self.shared.generation.get() + 1);
        drop(self.subscription.borrow_mut().take());
        self.connection.borrow_mut().take();
    }

    fn call(&self, method: &str, parameters: Variant) {
        let Some(bus) = self.connection.borrow().clone() else {
            return;
        };
        let method = method.to_string();
        glib::spawn_future_local(async move {
            let _ = call(&bus, &method, Some(&parameters)).await;
        });
    }

    pub fn dismiss(&self, id: u32) {
        let dict = glib::VariantDict::new(None);
        dict.insert("id", &id);
        self.call("DismissNotifications", dict.end());
    }

    pub fn clear(&self) {
        let dict = glib::VariantDict::new(None);
        dict.insert("all", &true);
        self.call("DismissNotifications", dict.end());
    }
}

async fn call(
    bus: &gio::DBusConnection,
    method: &str,
    parameters: Option<&Variant>,
) -> std::result::Result<Variant, String> {
    bus.call_future(
        Some(SERVICE),
        PATH,
        INTERFACE,
        method,
        parameters,
        None,
        gio::DBusCallFlags::NONE,
        TIMEOUT,
    )
    .await
    .map_err(|error| error.to_string())
}

// Re-fetch the list after an invalidation, coalescing requests while one is in
// flight: a signal that arrives mid-fetch is dropped, since Mako re-emits the
// invalidation whenever its state actually changes again.
fn refresh(connection: Weak<RefCell<Option<gio::DBusConnection>>>, shared: Rc<Shared>) {
    if shared.listing.replace(true) {
        return;
    }
    let Some(bus) = connection.upgrade().and_then(|cell| cell.borrow().clone()) else {
        shared.listing.set(false);
        return;
    };
    glib::spawn_future_local(async move {
        let generation = shared.generation.get();
        let result = call(&bus, "ListNotifications", None).await;
        shared.listing.set(false);
        if shared.generation.get() == generation {
            (shared.changed)(result.map(|reply| parse(&reply)));
        }
    });
}

fn string_of(dict: &glib::VariantDict, key: &str) -> String {
    dict.lookup_value(key, None)
        .and_then(|value| value.str().map(str::to_string))
        .unwrap_or_default()
}

fn parse(reply: &Variant) -> Vec<Notification> {
    let mut entries = Vec::new();
    // GDBus wraps method return values in a tuple: (aa{sv}).
    if reply.type_().as_str() != "(aa{sv})" {
        return entries;
    }
    for item in reply.child_value(0).iter() {
        let dict = glib::VariantDict::new(Some(&item));
        let Ok(Some(id)) = dict.lookup::<u32>("id") else {
            continue;
        };
        entries.push(Notification {
            id,
            app: string_of(&dict, "app-name"),
            icon: string_of(&dict, "app-icon"),
            title: string_of(&dict, "summary"),
            body: string_of(&dict, "body"),
        });
    }
    entries
}

#[cfg(test)]
mod tests {
    use super::*;
    use glib::variant::ToVariant;

    #[test]
    fn parses_mako_method_return_tuple() {
        let dict = glib::VariantDict::new(None);
        dict.insert("id", 42u32);
        dict.insert("summary", "Hello");
        let array = glib::Variant::array_from_iter_with_type(
            glib::VariantTy::new("a{sv}").unwrap(),
            [dict.end()],
        );
        let reply = glib::Variant::tuple_from_iter([array]);
        let items = parse(&reply);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, 42);
        assert_eq!(items[0].title, "Hello");
        assert!(parse(&().to_variant()).is_empty());
    }
}
