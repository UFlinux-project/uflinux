//! ConnMan VPN client (net.connman.vpn, served by connman-vpnd). Mirrors
//! network.rs: event-driven cache, coalesced reload, published Vpn view. When
//! vpnd is not running the view stays unavailable and the UI hides the tab.
//!
//! The API (verified against ConnMan 2.0): Manager.Create(a{sv}) is async and
//! replies with the connection object path; unknown string keys in the dict
//! (WireGuard.*) are stored verbatim on the provider, so an import is a single
//! Create call. Connections expose GetProperties/SetProperty/Connect/Disconnect;
//! removal goes through Manager.Remove, which also deletes vpnd's own storage
//! file, so no root helper is involved.
use glib::variant::{ObjectPath, ToVariant};
use settings_core::{
    bus::{self, Properties},
    *,
};
use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    rc::Rc,
};

const NAME: &str = "net.connman.vpn";
const MANAGER: &str = "net.connman.vpn.Manager";
const CONNECTION: &str = "net.connman.vpn.Connection";
// The Manager interface is registered at "/" on the VPN bus name.
const MANAGER_PATH: &str = "/";
const KEYS: &[&str] = &[
    "WireGuard.PrivateKey",
    "WireGuard.PublicKey",
    "WireGuard.AllowedIPs",
    "WireGuard.Address",
    "WireGuard.PresharedKey",
    "WireGuard.ListenPort",
    "WireGuard.EndpointPort",
    "WireGuard.PersistentKeepalive",
];

pub struct Client {
    subscriptions: RefCell<Vec<gio::SignalSubscription>>,
    bus: gio::DBusConnection,
    view: RefCell<Vpn>,
    scheduled: Cell<bool>,
    again: Cell<bool>,
    emit: Rc<dyn Fn(Vpn)>,
}
impl Client {
    pub async fn new(emit: Rc<dyn Fn(Vpn)>) -> Result<Rc<Self>> {
        let bus = gio::bus_get_future(gio::BusType::System)
            .await
            .map_err(|e| e.to_string())?;
        let this = Rc::new(Self {
            subscriptions: RefCell::new(Vec::new()),
            bus,
            view: RefCell::new(Vpn::default()),
            scheduled: Cell::new(false),
            again: Cell::new(false),
            emit,
        });
        let weak = Rc::downgrade(&this);
        let subscription = this.bus.subscribe_to_signal(
            Some(NAME),
            None,
            None,
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
            *self.view.borrow_mut() = Vpn::default();
            self.publish();
            return;
        }
        let _ = self.reload().await;
    }
    async fn manager(&self, method: &str, args: Option<&glib::Variant>) -> Result<glib::Variant> {
        bus::call(&self.bus, NAME, MANAGER_PATH, MANAGER, method, args).await
    }
    async fn connection_call(&self, path: &str, method: &str) -> Result<()> {
        validate::vpn_connection(path)?;
        if !self.view.borrow().tunnels.iter().any(|t| t.path == path) {
            return Err("The VPN connection is no longer available".into());
        }
        bus::call(&self.bus, NAME, path, CONNECTION, method, None)
            .await
            .map(|_| ())
    }
    pub async fn reload(&self) -> Result<()> {
        let reply = self.manager("GetConnections", None).await?;
        let Some(connections) = reply.get::<(Vec<(ObjectPath, Properties)>,)>() else {
            return Err("Invalid VPN connection list".into());
        };
        let view = Vpn {
            available: true,
            tunnels: connections
                .0
                .into_iter()
                .map(|(path, p)| VpnTunnel {
                    path: path.to_string(),
                    name: bus::text(&p, "Name"),
                    kind: bus::text(&p, "Type"),
                    host: bus::text(&p, "Host"),
                    state: bus::text(&p, "State"),
                    interface: bus::text(&p, "Index"),
                })
                .collect(),
        };
        *self.view.borrow_mut() = view;
        self.publish();
        Ok(())
    }
    pub async fn execute(&self, action: &Action) -> Result<()> {
        match action {
            Action::VpnConnect { tunnel } => {
                validate::vpn_connection(tunnel)?;
                if !self.view.borrow().tunnels.iter().any(|t| t.path == *tunnel) {
                    return Err("The VPN connection is no longer available".into());
                }
                bus::call(&self.bus, NAME, tunnel, CONNECTION, "Connect", None).await?;
            }
            Action::VpnDisconnect { tunnel } => {
                self.connection_call(tunnel, "Disconnect").await?;
            }
            Action::VpnRemove { tunnel } => {
                validate::vpn_connection(tunnel)?;
                if !self.view.borrow().tunnels.iter().any(|t| t.path == *tunnel) {
                    return Err("The VPN connection is no longer available".into());
                }
                let path =
                    ObjectPath::try_from(tunnel.clone()).map_err(|_| "Invalid VPN connection")?;
                self.manager("Remove", Some(&(path,).to_variant())).await?;
            }
            Action::VpnImport { name, host, keys } => {
                if name.trim().is_empty() || name.len() > 64 {
                    return Err("Give the VPN connection a name up to 64 characters".into());
                }
                if !validate::server(host) {
                    return Err("The VPN endpoint is not a valid host name or address".into());
                }
                let mut dict: BTreeMap<String, String> = BTreeMap::new();
                for key in KEYS {
                    let value = keys
                        .get(*key)
                        .map(|v| v.trim().to_string())
                        .unwrap_or_default();
                    if value.is_empty() {
                        continue;
                    }
                    if value.len() > 512 || value.contains(['\n', '\r', '\0']) {
                        return Err(format!("{key} is not a valid value"));
                    }
                    dict.insert((*key).to_string(), value);
                }
                for required in [
                    "WireGuard.PrivateKey",
                    "WireGuard.PublicKey",
                    "WireGuard.AllowedIPs",
                    "WireGuard.Address",
                ] {
                    if !dict.contains_key(required) {
                        return Err(format!("The configuration is missing {required}"));
                    }
                }
                let mut create: Properties =
                    dict.into_iter().map(|(k, v)| (k, v.to_variant())).collect();
                create.insert("Type".into(), "wireguard".to_variant());
                create.insert("Name".to_string(), name.to_variant());
                create.insert("Host".into(), host.to_variant());
                // Manager.Create wants a plain (a{sv}) tuple body; VariantDict
                // serializes to a{sv} and tuple_from_iter keeps that type.
                let body = glib::VariantDict::new(None);
                for (key, value) in create {
                    body.insert_value(&key, &value);
                }
                let args = glib::Variant::tuple_from_iter([glib::Variant::from(body)]);
                bus::call(
                    &self.bus,
                    NAME,
                    MANAGER_PATH,
                    MANAGER,
                    "Create",
                    Some(&args),
                )
                .await?;
            }
            _ => return Err("Not a VPN action".into()),
        }
        Ok(())
    }
}
