//! BlueZ client. Mirrors network.rs: an event-driven property cache that
//! publishes a Bluetooth view into the settings snapshot. When org.bluez has
//! no owner the view stays unavailable and costs nothing beyond one signal
//! subscription on NameOwnerChanged.
use glib::variant::{ObjectPath, ToVariant};
use settings_core::{
    bus::{self, Properties},
    *,
};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};

const NAME: &str = "org.bluez";
const ADAPTER: &str = "org.bluez.Adapter1";
const DEVICE: &str = "org.bluez.Device1";

pub struct Client {
    subscriptions: RefCell<Vec<gio::SignalSubscription>>,
    bus: gio::DBusConnection,
    view: RefCell<Bluetooth>,
    /// Adapter paths from the last reload, so power/scan need no extra round trip.
    adapters: RefCell<Vec<String>>,
    scheduled: Cell<bool>,
    again: Cell<bool>,
    emit: Rc<dyn Fn(Bluetooth)>,
}
impl Client {
    pub async fn new(emit: Rc<dyn Fn(Bluetooth)>) -> Result<Rc<Self>> {
        let bus = gio::bus_get_future(gio::BusType::System)
            .await
            .map_err(|e| e.to_string())?;
        let this = Rc::new(Self {
            subscriptions: RefCell::new(Vec::new()),
            bus,
            view: RefCell::new(Bluetooth::default()),
            adapters: RefCell::new(Vec::new()),
            scheduled: Cell::new(false),
            again: Cell::new(false),
            emit,
        });
        // BlueZ reports everything through these three signals; subscribing
        // narrowly (instead of every org.bluez signal) keeps reloads rare.
        for (interface, member) in [
            ("org.freedesktop.DBus.Properties", "PropertiesChanged"),
            ("org.freedesktop.DBus.ObjectManager", "InterfacesAdded"),
            ("org.freedesktop.DBus.ObjectManager", "InterfacesRemoved"),
        ] {
            let weak = Rc::downgrade(&this);
            let subscription = this.bus.subscribe_to_signal(
                Some(NAME),
                Some(interface),
                Some(member),
                None,
                None,
                gio::DBusSignalFlags::NONE,
                move |_| {
                    if let Some(this) = weak.upgrade() {
                        this.schedule();
                    }
                },
            );
            this.subscriptions.borrow_mut().push(subscription);
        }
        let weak = Rc::downgrade(&this);
        let subscription = this.bus.subscribe_to_signal(
            Some("org.freedesktop.DBus"),
            Some("org.freedesktop.DBus"),
            Some("NameOwnerChanged"),
            Some("/org/freedesktop/DBus"),
            Some(NAME),
            gio::DBusSignalFlags::NONE,
            move |_| {
                if let Some(this) = weak.upgrade() {
                    let task = this.clone();
                    glib::MainContext::default().spawn_local(async move {
                        task.initialize().await;
                    });
                }
            },
        );
        this.subscriptions.borrow_mut().push(subscription);
        this.initialize().await;
        Ok(this)
    }
    fn publish(&self) {
        (self.emit)(self.view.borrow().clone());
    }
    fn schedule(self: &Rc<Self>) {
        if self.scheduled.replace(true) {
            self.again.set(true);
            return;
        }
        let this = self.clone();
        glib::idle_add_local_once(move || {
            glib::MainContext::default().spawn_local(async move {
                let _ = this.reload().await;
                this.scheduled.set(false);
                if this.again.replace(false) {
                    this.schedule();
                }
            });
        });
    }
    async fn initialize(self: &Rc<Self>) {
        let owner = bus::call(
            &self.bus,
            "org.freedesktop.DBus",
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
            "GetNameOwner",
            Some(&(NAME,).to_variant()),
        )
        .await;
        if owner.is_err() {
            self.adapters.borrow_mut().clear();
            *self.view.borrow_mut() = Bluetooth::default();
            self.publish();
            return;
        }
        let _ = self.reload().await;
    }
    async fn managed_objects(&self) -> Result<glib::Variant> {
        bus::call(
            &self.bus,
            NAME,
            "/",
            "org.freedesktop.DBus.ObjectManager",
            "GetManagedObjects",
            None,
        )
        .await
    }
    fn adapter_paths(&self) -> Vec<String> {
        self.adapters.borrow().clone()
    }
    pub async fn reload(&self) -> Result<()> {
        let reply = self.managed_objects().await?;
        let Some(objects) = reply.get::<(
            std::collections::HashMap<ObjectPath, std::collections::HashMap<String, Properties>>,
        )>() else {
            return Err("Invalid BlueZ object list".into());
        };
        let mut view = Bluetooth::default();
        let mut adapters = Vec::new();
        for (path, interfaces) in objects.0 {
            let path = path.to_string();
            if let Some(adapter) = interfaces.get(ADAPTER) {
                adapters.push(path.clone());
                view.powered |= bus::boolean(adapter, "Powered");
                view.discovering |= bus::boolean(adapter, "Discovering");
            }
            let Some(device) = interfaces.get(DEVICE) else {
                continue;
            };
            let battery = interfaces
                .get("org.bluez.Battery1")
                .and_then(|p| p.get("Percentage"))
                .and_then(|v| v.get::<u8>())
                .or_else(|| device.get("Battery").and_then(|v| v.get::<u8>()));
            view.devices.push(BluetoothDevice {
                path,
                name: bus::text(device, "Name"),
                alias: bus::text(device, "Alias"),
                paired: bus::boolean(device, "Paired"),
                trusted: bus::boolean(device, "Trusted"),
                connected: bus::boolean(device, "Connected"),
                rssi: device.get("RSSI").and_then(|v| v.get::<i16>()),
                battery,
            });
        }
        view.available = !adapters.is_empty();
        view.devices.sort_by(|a, b| {
            (b.connected, b.paired, a.name.to_lowercase()).cmp(&(
                a.connected,
                a.paired,
                b.name.to_lowercase(),
            ))
        });
        view.devices.truncate(64);
        *self.adapters.borrow_mut() = adapters;
        *self.view.borrow_mut() = view;
        self.publish();
        Ok(())
    }
    async fn device_call(&self, path: &str, method: &str) -> Result<()> {
        validate::bluetooth_device(path)?;
        if !self.view.borrow().devices.iter().any(|d| d.path == path) {
            return Err("The Bluetooth device is no longer available".into());
        }
        bus::call(&self.bus, NAME, path, DEVICE, method, None)
            .await
            .map(|_| ())
    }
    pub async fn execute(&self, action: &Action) -> Result<()> {
        match action {
            Action::BluetoothPower { enabled } => {
                let adapters = self.adapter_paths();
                if adapters.is_empty() {
                    return Err("No Bluetooth adapter is available".into());
                }
                for path in adapters {
                    bus::call(
                        &self.bus,
                        NAME,
                        &path,
                        ADAPTER,
                        "SetProperty",
                        Some(&("Powered", enabled.to_variant()).to_variant()),
                    )
                    .await?;
                }
            }
            Action::BluetoothScan => {
                // BlueZ answers StartDiscovery with org.bluez.Error.InProgress
                // when a discovery is already running, and refuses outright
                // when the adapter is off. Telling those apart keeps an
                // already-scanning adapter from reporting "turn Bluetooth on".
                let adapters = self.adapter_paths();
                if adapters.is_empty() {
                    return Err("No Bluetooth adapter is available".into());
                }
                if !self.view.borrow().powered {
                    return Err("Turn Bluetooth on before scanning".into());
                }
                let mut failure = None;
                for path in adapters {
                    if let Err(error) =
                        bus::call(&self.bus, NAME, &path, ADAPTER, "StartDiscovery", None).await
                    {
                        // Already discovering counts as a started scan; the
                        // PropertiesChanged signal refreshes the view anyway.
                        if !error.contains("org.bluez.Error.InProgress") {
                            failure.get_or_insert(error);
                        }
                    }
                }
                // A scan may already be running on every adapter, in which
                // case the cached view says so even though each call failed.
                if self.view.borrow().discovering {
                    return Ok(());
                }
                if let Some(error) = failure {
                    return Err(error);
                }
            }
            Action::BluetoothPair { device } => self.device_call(device, "Pair").await?,
            Action::BluetoothUnpair { device } => {
                validate::bluetooth_device(device)?;
                if !self.view.borrow().devices.iter().any(|d| d.path == *device) {
                    return Err("The Bluetooth device is no longer available".into());
                }
                let _ = bus::call(&self.bus, NAME, device, DEVICE, "Disconnect", None).await;
                let parent = device
                    .rsplit_once('/')
                    .map(|(parent, _)| parent.to_string())
                    .unwrap_or_default();
                if !self.adapters.borrow().iter().any(|a| *a == parent) {
                    return Err("Invalid Bluetooth device".into());
                }
                let path =
                    ObjectPath::try_from(device.clone()).map_err(|_| "Invalid Bluetooth device")?;
                bus::call(
                    &self.bus,
                    NAME,
                    &parent,
                    ADAPTER,
                    "RemoveDevice",
                    Some(&(path,).to_variant()),
                )
                .await
                .map(|_| ())?;
            }
            Action::BluetoothTrust { device, enabled } => {
                validate::bluetooth_device(device)?;
                if !self.view.borrow().devices.iter().any(|d| d.path == *device) {
                    return Err("The Bluetooth device is no longer available".into());
                }
                bus::call(
                    &self.bus,
                    NAME,
                    device,
                    DEVICE,
                    "SetProperty",
                    Some(&("Trusted", enabled.to_variant()).to_variant()),
                )
                .await
                .map(|_| ())?;
            }
            Action::BluetoothConnect { device } => self.device_call(device, "Connect").await?,
            Action::BluetoothDisconnect { device } => {
                self.device_call(device, "Disconnect").await?
            }
            _ => return Err("Not a Bluetooth action".into()),
        }
        Ok(())
    }
}
