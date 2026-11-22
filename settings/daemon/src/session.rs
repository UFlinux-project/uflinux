use crate::{audio, bluetooth, capture, devices, display, network, rdp, vpn};
use futures_util::future::join3;
use gio::prelude::*;
use glib::variant::ToVariant;
use settings_core::{bus, *};
use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::Command,
    rc::Rc,
    time::Duration,
};

struct Pending {
    id: u64,
    previous: Vec<OutputConfig>,
    hdr: bool,
    caller: String,
    previous_xml: Option<Vec<u8>>,
}
struct Applying<'a>(&'a Cell<bool>);
impl Drop for Applying<'_> {
    fn drop(&mut self) {
        self.0.set(false);
    }
}
struct App {
    bus: gio::DBusConnection,
    system: gio::DBusConnection,
    view: RefCell<Snapshot>,
    network: RefCell<Option<Rc<network::Client>>>,
    bluetooth: RefCell<Option<Rc<bluetooth::Client>>>,
    vpn: RefCell<Option<Rc<vpn::Client>>>,
    audio: RefCell<Option<Rc<audio::Client>>>,
    audio_retry: Cell<bool>,
    display: RefCell<Option<display::Client>>,
    pending: RefCell<Option<Pending>>,
    applying: Cell<bool>,
    publishing: Cell<bool>,
    restored: Cell<bool>,
    brightness_pending: Cell<bool>,
    ddc_pending: Cell<bool>,
    ddc_busy: Cell<bool>,
    ddc_dirty: Cell<bool>,
    /// A compositor reload is already queued behind the coalescing delay.
    reconfigure_pending: Cell<bool>,
    /// Reloads requested while a reload was queued or in flight.
    reconfigure_dirty: Cell<bool>,
    /// A clock poll is already in flight; the next one waits for it.
    clock_pending: Cell<bool>,
    /// A power read is in flight; events during it are retried once.
    power_pending: Cell<bool>,
    /// A power event arrived while a read was in flight.
    power_dirty: Cell<bool>,
}
pub fn run() -> Result<()> {
    if std::env::var_os("XDG_RUNTIME_DIR").is_none() {
        return Err("A user login session is required".into());
    }
    let (_app,_power_monitors,_device_monitor,_subscriptions,mainloop)=glib::MainContext::default().block_on(async {
        let connection=gio::bus_get_future(gio::BusType::Session).await.map_err(|e|e.to_string())?;
        let system=gio::bus_get_future(gio::BusType::System).await.map_err(|e|e.to_string())?;
        let appearance=files::read_json(&files::config_dir().join("appearance.json"));
        let screen_capture:ScreenCapture=files::read_json(&files::config_dir().join("screen-capture.json"));
        let screen_capture=capture::validate(&screen_capture).map(|()|screen_capture).unwrap_or_default();
        // The snapshot starts from defaults: About, clock, power and brightness are
        // probed off the main context below and published as soon as they arrive, so
        // acquiring the bus name never waits for a sysfs or /proc fan-out.
        let app=Rc::new(App{bus:connection.clone(),system,view:RefCell::new(Snapshot{appearance,screen_capture,..Snapshot::default()}),
            network:RefCell::new(None),bluetooth:RefCell::new(None),vpn:RefCell::new(None),audio:RefCell::new(None),audio_retry:Cell::new(false),display:RefCell::new(None),pending:RefCell::new(None),applying:Cell::new(false),publishing:Cell::new(false),restored:Cell::new(false),brightness_pending:Cell::new(false),ddc_pending:Cell::new(false),ddc_busy:Cell::new(false),ddc_dirty:Cell::new(false),reconfigure_pending:Cell::new(false),reconfigure_dirty:Cell::new(false),clock_pending:Cell::new(false),power_pending:Cell::new(false),power_dirty:Cell::new(false)});
        app.view.borrow_mut().displays.renderer=std::env::var("UFLINUX_RENDERER").unwrap_or_else(|_|"User-selected / compositor default".into());
        app.view.borrow_mut().displays.hdr_requested=fs::read_to_string(hdr_path()).ok().and_then(|s|files::hdr_requested(&s)).unwrap_or(false);
        app.view.borrow_mut().displays.hdr_status="Labwc applies HDR automatically where supported. Per-monitor activation is not reported by this protocol; mirroring is unavailable.".into();
        let info=gio::DBusNodeInfo::for_xml(bus::SESSION_XML).map_err(|e|e.to_string())?.interfaces()[0].clone();
        let weak=Rc::downgrade(&app);
        connection.register_object(SESSION_PATH,&info).method_call(move|_,sender,_,_,method,parameters,invocation|{
            let Some(app)=weak.upgrade() else{return;};
            if method=="GetState" {
                invocation.return_value(Some(&(serde_json::to_string(&*app.view.borrow()).unwrap_or_default(),).to_variant()));return;
            }
            if method!="Execute"{invocation.return_dbus_error("org.freedesktop.DBus.Error.UnknownMethod","Unknown operation");return;}
            let Some((request,))=parameters.get::<(String,)>() else{invocation.return_dbus_error("org.uflinux.Settings.Error.Invalid","Invalid request");return;};
            if request.len()>MAX_REQUEST{invocation.return_dbus_error("org.uflinux.Settings.Error.Invalid","Request too large");return;}
            let Ok(action)=serde_json::from_str::<Action>(&request) else{invocation.return_dbus_error("org.uflinux.Settings.Error.Invalid","Unknown typed operation");return;};
            let sender=sender.unwrap_or_default().to_string();
            glib::MainContext::default().spawn_local(async move {
                app.view.borrow_mut().error=None;app.publish();
                let result=app.execute(action,&sender).await;
                match result{
                    Ok(value)=>invocation.return_value(Some(&(value.to_string(),).to_variant())),
                    Err(error)=>{app.view.borrow_mut().error=Some(error.clone());app.publish();invocation.return_dbus_error("org.uflinux.Settings.Error.Failed",&error);}
                }
            });
        }).build().map_err(|e|e.to_string())?;
        bus::request_name(&connection,SESSION_NAME)?;
        let weak=Rc::downgrade(&app);
        let owner_subscription = connection.subscribe_to_signal(Some("org.freedesktop.DBus"),Some("org.freedesktop.DBus"),Some("NameOwnerChanged"),None,None,gio::DBusSignalFlags::NONE,move |gio::DBusSignalRef { parameters: params, .. }|{
            if let Some((name,_,new))=params.get::<(String,String,String)>() {
                if new.is_empty(){if let Some(app)=weak.upgrade(){let revert=app.pending.borrow().as_ref().is_some_and(|p|p.caller==name);if revert{let task=app.clone();glib::MainContext::default().spawn_local(async move{let _=task.revert().await;});}}}
            }
        });
        let weak=Rc::downgrade(&app);
        let network_client=network::Client::new(&app.system,Rc::new(move|network|if let Some(app)=weak.upgrade(){app.view.borrow_mut().network=network;app.publish();}));
        let weak=Rc::downgrade(&app);
        let bluetooth_client=bluetooth::Client::new(&app.system,Rc::new(move|bluetooth|if let Some(app)=weak.upgrade(){app.view.borrow_mut().bluetooth=bluetooth;app.publish();}));
        let weak=Rc::downgrade(&app);
        let vpn_client=vpn::Client::new(&app.system,Rc::new(move|vpn|if let Some(app)=weak.upgrade(){app.view.borrow_mut().vpn=vpn;app.publish();}));
        // Each client only registers itself with the system bus, so the three
        // round trips overlap instead of queueing behind each other.
        let (network,bluetooth,vpn)=join3(network_client,bluetooth_client,vpn_client).await;
        app.network.replace(Some(network?));app.start_audio();
        if let Ok(client)=bluetooth {app.bluetooth.replace(Some(client));}
        if let Ok(client)=vpn {app.vpn.replace(Some(client));}
        // The About page, the backlight list and the clock are not needed to answer
        // the first request; reading them inline would hold the bus name and the
        // first Get for as long as udev, /proc, timedatectl and ConnMan take.
        {
            let app = app.clone();
            glib::MainContext::default().spawn_local(async move {
                let (about, brightness, datetime) = join3(
                    gio::spawn_blocking(information),
                    gio::spawn_blocking(backlights),
                    clock::status_async(&app.system),
                )
                .await;
                {
                    let mut view = app.view.borrow_mut();
                    view.about = about.unwrap_or_else(|_| information());
                    view.displays.brightness = brightness.unwrap_or_else(|_| backlights());
                    view.datetime = datetime;
                }
                app.publish();
            });
        }
        let (send,receive)=async_channel::bounded(16);
        match display::Client::start(send){Ok(client)=>{app.display.replace(Some(client));},Err(error)=>app.view.borrow_mut().error=Some(error)}
        let weak=Rc::downgrade(&app);
        glib::MainContext::default().spawn_local(async move {
            while let Ok(event)=receive.recv().await {let Some(app)=weak.upgrade() else{break;};match event{
                display::Event::Outputs(outputs)=>{
                    let topology=|values:&[Output]|values.iter().map(|o|(o.name.clone(),o.identity(),o.modes.clone())).collect::<Vec<_>>();
                    let changed=topology(&app.view.borrow().displays.outputs)!=topology(&outputs);
                    {let mut view=app.view.borrow_mut();view.displays.outputs=outputs;view.displays.available=true;}
                    let initial=!app.restored.replace(true);
                    if changed||initial{app.queue_brightness(true);let task=app.clone();glib::MainContext::default().spawn_local(async move{if task.pending.borrow().is_some(){let _=task.revert().await;}let _=task.restore_displays().await;});}
                    app.publish();
                }
                display::Event::Unavailable(reason)=>{app.view.borrow_mut().displays.available=false;app.view.borrow_mut().displays.hdr_status=reason;app.publish();}
            }}
        });
        let mut power_monitors=Vec::new();
        for path in ["/run/tlp","/etc/uflinux"]{
            if let Ok(monitor)=gio::File::for_path(path).monitor_directory(gio::FileMonitorFlags::NONE,gio::Cancellable::NONE){
                let weak=Rc::downgrade(&app);monitor.connect_changed(move|_,_,_,_|if let Some(app)=weak.upgrade(){app.refresh_power();});power_monitors.push(monitor);
            }
        }
        let weak=Rc::downgrade(&app);
        let _device_monitor=devices::Monitor::new(move|event|if let Some(app)=weak.upgrade(){
            if event.backlight||event.drm{app.queue_brightness(event.drm);}
            if event.power{app.refresh_power();}
        }).map_err(|error| eprintln!("uflinux-settingsd: {error}")).ok();
        let weak=Rc::downgrade(&app);
        let resume_subscription = app.system.subscribe_to_signal(Some("org.freedesktop.login1"),Some("org.freedesktop.login1.Manager"),Some("PrepareForSleep"),None,None,gio::DBusSignalFlags::NONE,move |gio::DBusSignalRef { parameters: params, .. }|{
            if params.get::<(bool,)>()==Some((false,)){if let Some(app)=weak.upgrade(){app.queue_brightness(true);app.refresh_power();let task=app.clone();glib::MainContext::default().spawn_local(async move{let _=task.restore_displays().await;});}}
        });
        let initial=app.clone();glib::MainContext::default().spawn_local(async move{
            let appearance=initial.view.borrow().appearance.clone();
            let enabled = appearance.window_previews;
            match write_window_previews(enabled) {
                Ok(true) => initial.schedule_reconfigure(),
                Ok(false) => {}
                Err(error) => {
                    initial.view.borrow_mut().error = Some(error);
                    initial.publish();
                }
            }
            let _=initial.set_appearance(appearance,false).await;
        });
        let probed=app.clone();glib::MainContext::default().spawn_local(async move{
            let caps=capture::probe().await;
            probed.view.borrow_mut().capture_capabilities=caps;
            probed.publish();
        });
        let probed=app.clone();glib::MainContext::default().spawn_local(async move{
            probed.view.borrow_mut().rdp=rdp::query().await;
            probed.publish();
        });
        let weak=Rc::downgrade(&app);
        glib::timeout_add_local(Duration::from_secs(15),move||{
            let Some(app)=weak.upgrade() else{return glib::ControlFlow::Break;};
            // timedatectl and ConnMan are both subprocesses, so the poll runs as a
            // task; a slow clock daemon must not stall the bus for fifteen seconds.
            if !app.clock_pending.replace(true) {
                let task = app.clone();
                glib::MainContext::default().spawn_local(async move {
                    task.refresh_clock().await;
                    task.clock_pending.set(false);
                });
            }
            app.refresh_power();
            glib::ControlFlow::Continue
        });
        let mainloop=glib::MainLoop::new(None,false);
        for signal in [libc::SIGTERM,libc::SIGINT]{let quit=mainloop.clone();glib_unix::unix_signal_add_local(signal,move||{quit.quit();glib::ControlFlow::Break});}
        Ok::<_,String>((app,power_monitors,_device_monitor,[owner_subscription,resume_subscription],mainloop))
    })?;
    mainloop.run();
    let _ = _app;
    Ok(())
}
impl App {
    fn queue_brightness(self: &Rc<Self>, ddc: bool) {
        if ddc {
            self.queue_ddc();
        }
        if self.brightness_pending.replace(true) {
            return;
        }
        let weak = Rc::downgrade(self);
        glib::timeout_add_local_once(Duration::from_millis(120), move || {
            let Some(app) = weak.upgrade() else {
                return;
            };
            app.brightness_pending.set(false);
            let mut next = backlights();
            next.extend(
                app.view
                    .borrow()
                    .displays
                    .brightness
                    .iter()
                    .filter(|b| b.ddc)
                    .cloned(),
            );
            if next != app.view.borrow().displays.brightness {
                app.view.borrow_mut().displays.brightness = next;
                app.publish();
            }
        });
    }
    fn queue_ddc(self: &Rc<Self>) {
        if self.ddc_busy.get() {
            self.ddc_dirty.set(true);
            return;
        }
        if self.ddc_pending.replace(true) {
            return;
        }
        let weak = Rc::downgrade(self);
        glib::timeout_add_local_once(Duration::from_millis(250), move || {
            let Some(app) = weak.upgrade() else {
                return;
            };
            app.ddc_pending.set(false);
            let connected = fs::read_dir("/sys/class/drm")
                .into_iter()
                .flatten()
                .flatten()
                .any(|entry| {
                    entry.path().join("ddc").exists()
                        && fs::read_to_string(entry.path().join("status"))
                            .is_ok_and(|s| s.trim() == "connected")
                });
            if !connected {
                let mut view = app.view.borrow_mut();
                view.displays.brightness.retain(|b| !b.ddc);
                view.displays.brightness_status.clear();
                drop(view);
                app.publish();
                return;
            }
            app.ddc_busy.set(true);
            glib::MainContext::default().spawn_local(async move {
                let result=app.system(&SystemAction::DdcDiscover).await.and_then(|value|
                    serde_json::from_value::<Vec<Brightness>>(value).map_err(|_|"Invalid brightness reply".into()));
                {
                    let mut view=app.view.borrow_mut();
                    view.displays.brightness.retain(|b|!b.ddc);
                    match result {
                        Ok(devices)=>{view.displays.brightness.extend(devices);view.displays.brightness_status.clear();},
                        Err(_)=>view.displays.brightness_status="External-display brightness is unavailable. An active local session and DDC/CI support are required.".into(),
                    }
                }
                app.ddc_busy.set(false); app.publish();
                if app.ddc_dirty.replace(false) { app.queue_ddc(); }
            });
        });
    }
    /// Reloads the compositor once for a burst of requests. The saved file is the
    /// source of truth, so a failed reload is reported but cannot undo a save.
    fn schedule_reconfigure(self: &Rc<Self>) {
        self.reconfigure_dirty.set(true);
        if self.reconfigure_pending.replace(true) {
            return;
        }
        let weak = Rc::downgrade(self);
        glib::timeout_add_local_once(Duration::from_millis(40), move || {
            let Some(app) = weak.upgrade() else {
                return;
            };
            app.reconfigure_pending.set(false);
            if !app.reconfigure_dirty.replace(false) {
                return;
            }
            glib::MainContext::default().spawn_local(async move {
                let result =
                    match gio::spawn_blocking(|| reconfigure_compositor(compositor_pid())).await {
                        Ok(result) => result,
                        Err(_) => reconfigure_compositor(compositor_pid()),
                    };
                if let Err(error) = result {
                    eprintln!("uflinux-settingsd: {error}");
                }
            });
        });
    }
    /// Re-reads the clock without blocking the main context on timedatectl or ConnMan.
    async fn refresh_clock(self: &Rc<Self>) {
        let next = clock::status_async(&self.system).await;
        let changed = {
            let view = self.view.borrow();
            let old = &view.datetime;
            old.timezone != next.timezone
                || old.time_sync_enabled != next.time_sync_enabled
                || old.sntp_running != next.sntp_running
                || old.synchronized != next.synchronized
                || old.timeservers != next.timeservers
        };
        self.view.borrow_mut().datetime = next;
        if changed {
            self.publish();
        }
    }
    fn refresh_power(self: &Rc<Self>) {
        // A udev burst, a resume or a TLP daemon restart can raise a dozen events;
        // the sysfs fan-out runs once for all of them, away from the main context.
        if self.power_pending.replace(true) {
            self.power_dirty.set(true);
            return;
        }
        let app = self.clone();
        glib::MainContext::default().spawn_local(async move {
            loop {
                let state = match gio::spawn_blocking(power::status).await {
                    Ok(state) => state,
                    // The blocking pool only drops a task if its own thread died.
                    Err(_) => power::status(),
                };
                if app.view.borrow().power != state {
                    app.view.borrow_mut().power = state;
                    app.publish();
                }
                // An event raised while this read ran still needs fresh values, so
                // the flag is cleared only after the read it refers to has finished.
                if !app.power_dirty.replace(false) {
                    app.power_pending.set(false);
                    return;
                }
            }
        });
    }
    fn publish(self: &Rc<Self>) {
        if self.publishing.replace(true) {
            return;
        }
        let this = self.clone();
        glib::timeout_add_local_once(Duration::from_millis(40), move || {
            this.publishing.set(false);
            let view = this.view.borrow().clone();
            let raw = serde_json::to_string(&view).unwrap_or_default();
            let _ = this.bus.emit_signal(
                None,
                SESSION_PATH,
                SESSION_NAME,
                "Changed",
                Some(&(raw,).to_variant()),
            );
        });
    }
    fn start_audio(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        match audio::Client::new(Rc::new(move |audio| {
            if let Some(app) = weak.upgrade() {
                let unavailable = !audio.available;
                app.view.borrow_mut().audio = audio;
                app.publish();
                if unavailable {
                    app.retry_audio();
                }
            }
        })) {
            Ok(audio) => {
                self.audio.replace(Some(audio));
            }
            Err(_) => {}
        }
        self.retry_audio();
    }
    fn retry_audio(self: &Rc<Self>) {
        if self.audio_retry.replace(true) {
            return;
        }
        let weak = Rc::downgrade(self);
        glib::timeout_add_local_once(Duration::from_secs(3), move || {
            if let Some(app) = weak.upgrade() {
                app.audio_retry.set(false);
                if !app.audio.borrow().as_ref().is_some_and(|a| a.ready()) {
                    app.start_audio();
                }
            }
        });
    }
    async fn system(&self, operation: &SystemAction) -> Result<serde_json::Value> {
        let request =
            serde_json::to_string(operation).map_err(|_| "Cannot encode system operation")?;
        let reply = bus::call(
            &self.system,
            SYSTEM_NAME,
            SYSTEM_PATH,
            SYSTEM_NAME,
            "Execute",
            Some(&(request,).to_variant()),
        )
        .await?;
        let raw = reply.get::<(String,)>().ok_or("Invalid system response")?.0;
        serde_json::from_str(&raw).map_err(|_| "Invalid system response".into())
    }
    /// Sends one RDP mutation to the broker, then re-reads the published state
    /// so the panel refreshes without the UI asking for it.
    async fn rdp_apply(self: &Rc<Self>, request: serde_json::Value) -> Result<()> {
        rdp::execute(request).await?;
        self.view.borrow_mut().rdp = rdp::query().await;
        self.publish();
        Ok(())
    }
    async fn execute(self: &Rc<Self>, action: Action, caller: &str) -> Result<serde_json::Value> {
        match &action {
            Action::Refresh => {
                self.view.borrow_mut().error = None;
                // About reads /proc and several sysfs trees; keep it off the main context.
                let about = match gio::spawn_blocking(information).await {
                    Ok(about) => about,
                    Err(_) => information(),
                };
                self.view.borrow_mut().about = about;
                if let Some(network) = self.network.borrow().clone() {
                    let _ = network.reload().await;
                }
                if let Some(bluetooth) = self.bluetooth.borrow().clone() {
                    let _ = bluetooth.reload().await;
                }
                if let Some(vpn) = self.vpn.borrow().clone() {
                    let _ = vpn.reload().await;
                }
                self.refresh_clock().await;
                self.refresh_power();
                self.queue_brightness(true);
                self.publish();
            }
            Action::SetPowerProfile { profile } => {
                let value = self
                    .system(&SystemAction::SetPowerProfile { profile: *profile })
                    .await?;
                self.view.borrow_mut().power =
                    serde_json::from_value(value).map_err(|_| "Invalid power state")?;
                self.publish();
            }
            Action::Networking { .. }
            | Action::Wifi { .. }
            | Action::Scan
            | Action::Connect { .. }
            | Action::Disconnect { .. }
            | Action::Forget { .. }
            | Action::AutoConnect { .. }
            | Action::ConfigureConnection { .. }
            | Action::AuthReply { .. }
            | Action::AuthCancel { .. } => {
                let network = self.network.borrow().clone().ok_or("ConnMan unavailable")?;
                let forgotten = if let Action::Forget { service } = &action {
                    network
                        .view
                        .borrow()
                        .services
                        .iter()
                        .find(|s| s.path == *service && s.kind == "wifi")
                        .map(|s| s.name.clone())
                } else {
                    None
                };
                network.execute(&action).await?;
                if let Some(ssid) = forgotten {
                    self.system(&SystemAction::ForgetCredentials { ssid })
                        .await?;
                }
            }
            Action::ConnectHidden { ssid, passphrase } => {
                self.system(&SystemAction::HiddenProfile {
                    ssid: ssid.clone(),
                    passphrase: passphrase.clone(),
                })
                .await?;
                let network = self.network.borrow().clone().ok_or("ConnMan unavailable")?;
                // iwd's protected Hidden profile adds the SSID to directed scans.
                // The first scan can be passive; it must find the hidden BSS
                // before iwd includes provisioned SSIDs in a directed scan.
                let mut found = None;
                for attempt in 0..80 {
                    if attempt % 12 == 0 {
                        if let Err(error) = network.execute(&Action::Scan).await {
                            if !["Busy", "InProgress", "in progress"]
                                .iter()
                                .any(|word| error.contains(word))
                            {
                                return Err(error);
                            }
                        }
                    }
                    network.reload().await?;
                    found = network
                        .view
                        .borrow()
                        .services
                        .iter()
                        .find(|s| s.name == *ssid && s.kind == "wifi")
                        .map(|s| s.path.clone());
                    if found.is_some() {
                        break;
                    }
                    glib::timeout_future(Duration::from_millis(250)).await;
                }
                let found=found.ok_or("Hidden network not found. Verify its name, signal and radio state, then scan again.")?;
                network.execute(&Action::Connect { service: found }).await?;
            }
            Action::BluetoothPower { .. }
            | Action::BluetoothScan
            | Action::BluetoothPair { .. }
            | Action::BluetoothUnpair { .. }
            | Action::BluetoothTrust { .. }
            | Action::BluetoothConnect { .. }
            | Action::BluetoothDisconnect { .. } => {
                let bluetooth = self
                    .bluetooth
                    .borrow()
                    .clone()
                    .ok_or("Bluetooth is unavailable")?;
                bluetooth.execute(&action).await?;
            }
            Action::VpnConnect { .. }
            | Action::VpnDisconnect { .. }
            | Action::VpnImport { .. }
            | Action::VpnRemove { .. } => {
                let vpn = self.vpn.borrow().clone().ok_or("VPN is unavailable")?;
                vpn.execute(&action).await?;
            }
            Action::SetVolume { .. }
            | Action::SetMute { .. }
            | Action::SetDefault { .. }
            | Action::SetBalance { .. }
            | Action::SetPort { .. }
            | Action::SetProfile { .. }
            | Action::SetStream { .. } => {
                let audio = self.audio.borrow().clone().ok_or("Audio is unavailable")?;
                audio.execute(&action).await?;
            }
            Action::AdjustVolume { delta } => {
                if !delta.is_finite() || delta.abs() > 100. {
                    return Err("Invalid volume adjustment".into());
                }
                let audio = self.audio.borrow().clone().ok_or("Audio is unavailable")?;
                let device = audio.device(false, "")?;
                let volume = audio
                    .view
                    .borrow()
                    .outputs
                    .iter()
                    .find(|d| d.name == device)
                    .map(|d| d.volume)
                    .unwrap_or(0.);
                audio
                    .execute(&Action::SetVolume {
                        input: false,
                        device,
                        percent: (volume + delta).clamp(0., 100.),
                    })
                    .await?;
            }
            Action::ToggleMute => {
                let audio = self.audio.borrow().clone().ok_or("Audio is unavailable")?;
                let device = audio.device(false, "")?;
                let muted = !audio
                    .view
                    .borrow()
                    .outputs
                    .iter()
                    .find(|d| d.name == device)
                    .map(|d| d.muted)
                    .unwrap_or(false);
                audio
                    .execute(&Action::SetMute {
                        input: false,
                        device,
                        muted,
                    })
                    .await?;
            }
            Action::Brightness { id, percent } => {
                self.brightness(id, *percent).await?;
            }
            Action::AdjustBrightness { delta } => {
                if !delta.is_finite() || delta.abs() > 100. {
                    return Err("Invalid brightness adjustment".into());
                }
                let b = self
                    .view
                    .borrow()
                    .displays
                    .brightness
                    .first()
                    .cloned()
                    .ok_or("No controllable brightness device")?;
                self.brightness(&b.id, (b.percent + delta).clamp(1., 100.))
                    .await?;
            }
            Action::ApplyDisplays { outputs, hdr } => {
                return self.apply_displays(outputs.clone(), *hdr, caller).await;
            }
            Action::ConfirmDisplays { id } => {
                if !self
                    .pending
                    .borrow()
                    .as_ref()
                    .is_some_and(|p| p.id == *id && p.caller == caller)
                {
                    return Err("Display confirmation expired or belongs to another window".into());
                }
                self.pending.borrow_mut().take();
                self.view.borrow_mut().displays.pending = None;
                let outputs = self.view.borrow().displays.outputs.clone();
                files::save_json(&files::config_dir().join("displays.json"), &outputs, 0o600)?;
                self.publish();
            }
            Action::RevertDisplays => {
                self.revert().await?;
            }
            Action::SetAppearance { appearance } => {
                self.set_appearance(appearance.clone(), true).await?;
            }
            Action::SetScreenCapture { screen_capture } => {
                capture::validate(screen_capture)?;
                files::save_json(
                    &files::config_dir().join("screen-capture.json"),
                    screen_capture,
                    0o600,
                )?;
                self.view.borrow_mut().screen_capture = screen_capture.clone();
                self.publish();
            }
            Action::ResetScreenCapture => {
                let defaults = ScreenCapture::default();
                files::save_json(
                    &files::config_dir().join("screen-capture.json"),
                    &defaults,
                    0o600,
                )?;
                self.view.borrow_mut().screen_capture = defaults;
                self.publish();
            }
            Action::ScreenCaptureCapabilities => {
                let caps = self.view.borrow().capture_capabilities.clone();
                if caps.backend_available {
                    return Ok(serde_json::to_value(caps).map_err(|_| "Invalid capture state")?);
                }
                let caps = capture::probe().await;
                let value = serde_json::to_value(&caps).map_err(|_| "Invalid capture state")?;
                self.view.borrow_mut().capture_capabilities = caps;
                self.publish();
                return Ok(value);
            }
            Action::RdpStatus => {
                let rdp = rdp::query().await;
                let value = serde_json::to_value(&rdp).map_err(|_| "Invalid RDP state")?;
                self.view.borrow_mut().rdp = rdp;
                self.publish();
                return Ok(value);
            }
            Action::SetRdpPorts { rdp_port, web_port } => {
                self.rdp_apply(rdp::ports_request(
                    rdp::disk_config(),
                    *rdp_port,
                    *web_port,
                )?)
                .await?;
            }
            Action::SetRdpWeb { enabled, port } => {
                self.rdp_apply(rdp::web_request(rdp::disk_config(), *enabled, *port)?)
                    .await?;
            }
            Action::SetRdpHttps { enabled } => {
                self.rdp_apply(rdp::https_request(rdp::disk_config(), *enabled)?)
                    .await?;
            }
            Action::SetRdpCertificate {
                mode,
                certificate_path,
                key_path,
            } => {
                let request = rdp::certificate_request(
                    rdp::disk_config(),
                    *mode,
                    certificate_path.as_deref(),
                    key_path.as_deref(),
                )?;
                self.rdp_apply(request).await?;
            }
            Action::SetRdpCodecs {
                allow_h264,
                allow_remotefx,
                hardware_acceleration,
            } => {
                let request = rdp::codecs_request(
                    rdp::disk_config(),
                    *allow_h264,
                    *allow_remotefx,
                    *hardware_acceleration,
                )?;
                self.rdp_apply(request).await?;
            }
            Action::SetRdpMaxSessions { max_sessions } => {
                self.rdp_apply(rdp::max_sessions_request(
                    rdp::disk_config(),
                    *max_sessions,
                )?)
                .await?;
            }
            Action::RdpDisconnect { session_id } => {
                self.rdp_apply(rdp::disconnect_request(session_id)?).await?;
            }
            Action::RdpSystem { operation } => {
                let current = self.view.borrow().rdp.clone();
                let expected = match operation {
                    RdpSystemAction::Enable => Some(true),
                    RdpSystemAction::Disable => Some(false),
                    _ => None,
                };
                let value = rdp::system(&self.system, operation, &current).await?;
                let mut state = rdp::query().await;
                // The broker notices the rewritten file on SIGHUP and handles that
                // signal asynchronously, so the first read can still carry the old
                // flag. Read again before the panel is redrawn.
                if expected.is_some_and(|want| state.enabled != want) {
                    let _ = glib::timeout_future(Duration::from_millis(600)).await;
                    state = rdp::query().await;
                }
                self.view.borrow_mut().rdp = state;
                self.publish();
                return Ok(value);
            }
            Action::System { operation } => {
                if matches!(operation, SystemAction::DdcDiscover) {
                    self.queue_brightness(true);
                    return Ok(serde_json::json!([]));
                }
                let value = match self.system(operation).await {
                    Ok(value) => value,
                    Err(_) if matches!(operation, SystemAction::DdcDiscover) => {
                        self.view.borrow_mut().displays.brightness_status="External-display brightness is unavailable. An active local session and DDC/CI support are required.".into();
                        self.publish();
                        return Ok(serde_json::json!([]));
                    }
                    Err(error) => return Err(error),
                };
                if matches!(
                    operation,
                    SystemAction::PowerStatus
                        | SystemAction::SetPowerButtons { .. }
                        | SystemAction::SetPowerProfile { .. }
                ) {
                    self.view.borrow_mut().power =
                        serde_json::from_value(value.clone()).map_err(|_| "Invalid power state")?;
                    self.publish();
                }
                if matches!(
                    operation,
                    SystemAction::SetTimeSync { .. }
                        | SystemAction::SetTimezone { .. }
                        | SystemAction::SetTime { .. }
                ) {
                    if let Ok(value) = self.system(&SystemAction::Status).await {
                        if let Ok(status) = serde_json::from_value(value) {
                            self.view.borrow_mut().datetime = status;
                            self.publish();
                        }
                    }
                }
                return Ok(value);
            }
        }
        Ok(serde_json::json!({"ok":true}))
    }
    async fn brightness(self: &Rc<Self>, id: &str, percent: f64) -> Result<()> {
        validate::percent(percent)?;
        if self
            .view
            .borrow()
            .displays
            .brightness
            .iter()
            .any(|b| b.id == id && b.ddc)
        {
            self.system(&SystemAction::DdcBrightness {
                id: id.into(),
                percent,
            })
            .await?;
        } else {
            let device = id
                .strip_prefix("backlight:")
                .ok_or("Unknown brightness device")?;
            let available = backlights();
            if !available.iter().any(|b| b.id == id) {
                return Err("Backlight disappeared".into());
            }
            let maximum = fs::read_to_string(
                Path::new("/sys/class/backlight")
                    .join(device)
                    .join("max_brightness"),
            )
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok())
            .ok_or("Invalid backlight range")?;
            let value = ((percent * maximum as f64 / 100.).round() as u32).clamp(1, maximum);
            bus::call(
                &self.system,
                "org.freedesktop.login1",
                "/org/freedesktop/login1/session/auto",
                "org.freedesktop.login1.Session",
                "SetBrightness",
                Some(&("backlight", device, value).to_variant()),
            )
            .await?;
        }
        if let Some(b) = self
            .view
            .borrow_mut()
            .displays
            .brightness
            .iter_mut()
            .find(|b| b.id == id)
        {
            b.percent = percent;
        }
        self.publish();
        Ok(())
    }
    async fn apply_displays(
        self: &Rc<Self>,
        outputs: Vec<OutputConfig>,
        hdr: bool,
        caller: &str,
    ) -> Result<serde_json::Value> {
        if self.applying.replace(true) {
            return Err("A display change is already being tested".into());
        }
        let _applying = Applying(&self.applying);
        if self.pending.borrow().is_some() {
            return Err("Confirm or revert the previous display change first".into());
        }
        validate::displays(&outputs, &self.view.borrow().displays.outputs)?;
        let client = self
            .display
            .borrow()
            .clone()
            .ok_or("Output management is unavailable")?;
        let previous = self
            .view
            .borrow()
            .displays
            .outputs
            .iter()
            .map(Output::config)
            .collect();
        let previous_hdr = self.view.borrow().displays.hdr_requested;
        let path = hdr_path();
        let previous_xml = fs::read(&path).ok();
        if hdr != previous_hdr {
            if hdr && self.view.borrow().displays.renderer != "vulkan" {
                return Err(
                    "HDR needs the Vulkan renderer; log in using hardware Vulkan first".into(),
                );
            }
            if write_hdr(hdr)? {
                self.schedule_reconfigure();
            }
        }
        if let Err(error) = client.apply(outputs).await {
            restore_xml(&path, &previous_xml)?;
            let _ = Command::new("labwc").arg("--reconfigure").status();
            return Err(error);
        }
        let id = files::unique();
        self.pending.replace(Some(Pending {
            id,
            previous,
            hdr: previous_hdr,
            caller: caller.into(),
            previous_xml,
        }));
        {
            let mut v = self.view.borrow_mut();
            v.displays.pending = Some(id);
            v.displays.hdr_requested = hdr;
        }
        self.publish();
        let owner = bus::call(
            &self.bus,
            "org.freedesktop.DBus",
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
            "NameHasOwner",
            Some(&(caller,).to_variant()),
        )
        .await?;
        if owner.get::<(bool,)>() != Some((true,)) {
            self.revert().await?;
            return Err(
                "The requesting application closed; the display change was reverted".into(),
            );
        }
        let weak = Rc::downgrade(self);
        glib::timeout_add_local_once(Duration::from_secs(15), move || {
            if let Some(app) = weak.upgrade() {
                if app.pending.borrow().as_ref().is_some_and(|p| p.id == id) {
                    let task = app.clone();
                    glib::MainContext::default().spawn_local(async move {
                        let _ = task.revert().await;
                    });
                }
            }
        });
        Ok(serde_json::json!({"id":id,"confirmation_seconds":15}))
    }
    async fn revert(self: &Rc<Self>) -> Result<()> {
        let Some(pending) = self.pending.borrow_mut().take() else {
            return Ok(());
        };
        restore_xml(&hdr_path(), &pending.previous_xml)?;
        let _ = Command::new("labwc").arg("--reconfigure").status();
        let current = self.view.borrow().displays.outputs.clone();
        let mut configs = current
            .iter()
            .map(|o| {
                pending
                    .previous
                    .iter()
                    .find(|c| c.name == o.name)
                    .cloned()
                    .unwrap_or_else(|| o.config())
            })
            .collect::<Vec<_>>();
        if !configs.iter().any(|c| c.enabled) {
            if let Some(c) = configs.first_mut() {
                c.enabled = true;
                c.mode = current[0]
                    .modes
                    .iter()
                    .find(|m| m.preferred)
                    .or(current[0].modes.first())
                    .cloned();
            }
        }
        {
            let mut v = self.view.borrow_mut();
            v.displays.pending = None;
            v.displays.hdr_requested = pending.hdr;
        }
        let client = self
            .display
            .borrow()
            .clone()
            .ok_or("Output management disappeared")?;
        let result = client.apply(configs).await;
        self.publish();
        result
    }
    async fn restore_displays(self: &Rc<Self>) -> Result<()> {
        if self.applying.get() {
            return Ok(());
        }
        let saved = files::read_json::<Vec<Output>>(&files::config_dir().join("displays.json"));
        let current = self.view.borrow().displays.outputs.clone();
        if saved.is_empty() && current.iter().any(|o| o.enabled) {
            return Ok(());
        }
        if current.is_empty() {
            return Ok(());
        }
        let configs = layout::restore(&saved, &current)?;
        let client = self
            .display
            .borrow()
            .clone()
            .ok_or("Output management unavailable")?;
        if client.apply(configs).await.is_err() {
            // A compositor can reject an individually valid saved combination.
            // Retry once with a single preferred-mode display, never in a loop.
            client
                .apply(layout::fallback(&self.view.borrow().displays.outputs)?)
                .await?;
        }
        Ok(())
    }

    async fn set_appearance(self: &Rc<Self>, appearance: Appearance, persist: bool) -> Result<()> {
        // The desktop draws the wallpaper, so this only has to decide whether
        // the request is safe to hand over. Nothing is spawned: a renderer that
        // is a child of the service dies with it, and the shell outlives it.
        let background_changed = {
            let view = self.view.borrow();
            view.appearance.wallpapers != appearance.wallpapers
                || view.appearance.color != appearance.color
        };
        let copy = appearance.clone();
        if !persist || background_changed {
            gio::spawn_blocking(move || {
                if copy.wallpapers.len() > 16
                    || copy.color.len() != 7
                    || !copy.color.starts_with('#')
                    || !copy.color[1..].bytes().all(|b| b.is_ascii_hexdigit())
                {
                    return Err("Invalid background settings".to_string());
                }
                for wallpaper in &copy.wallpapers {
                    if !["fill", "fit", "stretch", "center", "tile"]
                        .contains(&wallpaper.fit.as_str())
                        || wallpaper.output.len() > 128
                        || wallpaper.output.contains('\0')
                    {
                        return Err("Invalid wallpaper layout".into());
                    }
                    let path = Path::new(&wallpaper.file);
                    let metadata = path
                        .metadata()
                        .map_err(|_| "Wallpaper file is unavailable")?;
                    if !metadata.is_file() || metadata.len() > 32 * 1024 * 1024 {
                        return Err("Wallpaper must be a regular image smaller than 32 MiB".into());
                    }
                    let dimensions = settings_core::image::probe_file(path)?;
                    if let settings_core::image::ImageDimensions::Pixels {
                        width,
                        height,
                    } = dimensions
                    {
                        if u64::from(width) * u64::from(height) > 16_777_216 {
                            return Err(
                                "Wallpaper exceeds the 16 megapixel memory limit".into()
                            );
                        }
                    }
                }
                Ok(())
            })
            .await
            .map_err(|_| "Wallpaper worker failed")??;
        }
        let path = hdr_path();
        let previous = fs::read(&path).ok();
        let update_switcher =
            persist && self.view.borrow().appearance.window_previews != appearance.window_previews;
        if update_switcher {
            let enabled = appearance.window_previews;
            if write_window_previews(enabled)? {
                self.schedule_reconfigure();
            }
        }
        if persist {
            if let Err(error) = files::save_json(
                &files::config_dir().join("appearance.json"),
                &appearance,
                0o600,
            ) {
                if update_switcher {
                    let _ = restore_xml(&path, &previous);
                    let _ = Command::new("labwc").arg("--reconfigure").status();
                }
                return Err(error);
            }
        }
        self.view.borrow_mut().appearance = appearance;
        self.publish();
        Ok(())
    }
}
fn hdr_path() -> PathBuf {
    files::config_dir().parent().unwrap().join("labwc/rc.xml")
}
/// Rewrites the thumbnail switcher setting. Returns whether the file changed and
/// the compositor should be reloaded; the reload itself is coalesced by the caller.
fn write_window_previews(enabled: bool) -> Result<bool> {
    let path = hdr_path();
    let previous = fs::read(&path).ok();
    let source = match fs::read_to_string(&path) {
        Ok(source) => source,
        // The distribution configuration already enables thumbnails. Avoid
        // creating a personal configuration just to repeat the default.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && enabled => return Ok(false),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => "<labwc_config/>".into(),
        Err(_) => return Err("Cannot read your labwc configuration".into()),
    };
    let xml = files::set_window_previews_xml(&source, enabled)?;
    if xml == source {
        return Ok(false);
    }
    if let Err(error) = files::atomic(&path, xml.as_bytes(), 0o600) {
        let _ = restore_xml(&path, &previous);
        return Err(error);
    }
    Ok(true)
}
fn write_hdr(enabled: bool) -> Result<bool> {
    let path = hdr_path();
    let previous = fs::read(&path).ok();
    let xml = match fs::read_to_string(&path) {
        Ok(xml) => xml,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            "<labwc_config></labwc_config>".into()
        }
        Err(_) => return Err("Cannot read your labwc configuration".into()),
    };
    let xml = files::set_hdr_xml(&xml, enabled)?;
    if let Err(error) = files::atomic(&path, xml.as_bytes(), 0o600) {
        let _ = restore_xml(&path, &previous);
        return Err(error);
    }
    Ok(true)
}
/// labwc reloads its configuration when it is signalled through the identifier
/// a session exports at startup, so a missing or stale identifier means there
/// is no compositor to notify rather than a rejected configuration. The saved
/// file still applies to the next session.
fn compositor_pid() -> Option<libc::pid_t> {
    live_compositor(std::env::var("LABWC_PID").ok())
}
fn live_compositor(published: Option<String>) -> Option<libc::pid_t> {
    published?
        .parse::<libc::pid_t>()
        .ok()
        .filter(|pid| *pid > 0 && unsafe { libc::kill(*pid, 0) } == 0)
}
fn reconfigure_compositor(compositor: Option<libc::pid_t>) -> Result<()> {
    if compositor.is_none() {
        eprintln!("uflinux-settingsd: no compositor to reload; applying at the next session");
        return Ok(());
    }
    match Command::new("labwc").arg("--reconfigure").status() {
        Ok(status) if status.success() => Ok(()),
        _ => Err("Cannot reload compositor settings".into()),
    }
}
fn restore_xml(path: &Path, previous: &Option<Vec<u8>>) -> Result<()> {
    match previous {
        Some(bytes) => files::atomic(path, bytes, 0o600),
        None => {
            let _ = fs::remove_file(path);
            Ok(())
        }
    }
}
fn backlights() -> Vec<Brightness> {
    devices::backlights(Path::new("/sys/class/backlight"))
}

fn information() -> BTreeMap<String, String> {
    let mut data = BTreeMap::new();
    let os = fs::read_to_string("/etc/os-release").unwrap_or_default();
    for line in os.lines() {
        if let Some((key, value)) = line.split_once('=') {
            if ["PRETTY_NAME", "VERSION_ID"].contains(&key) {
                data.insert(key.into(), value.trim_matches('"').into());
            }
        }
    }
    data.insert("Architecture".into(), std::env::consts::ARCH.into());
    data.insert(
        "Kernel".into(),
        fs::read_to_string("/proc/sys/kernel/osrelease")
            .unwrap_or_default()
            .trim()
            .into(),
    );
    data.insert(
        "Hostname".into(),
        fs::read_to_string("/proc/sys/kernel/hostname")
            .unwrap_or_default()
            .trim()
            .into(),
    );
    if let Some(cpu) = fs::read_to_string("/proc/cpuinfo")
        .unwrap_or_default()
        .lines()
        .find_map(|l| {
            l.split_once(':')
                .filter(|(k, _)| ["model name", "Processor", "cpu"].contains(&k.trim()))
                .map(|(_, v)| v.trim().to_string())
        })
    {
        data.insert("Processor".into(), cpu);
    }
    for line in fs::read_to_string("/proc/meminfo")
        .unwrap_or_default()
        .lines()
    {
        if let Some((key, value)) = line.split_once(':') {
            if ["MemTotal", "MemAvailable"].contains(&key) {
                data.insert(key.into(), value.trim().into());
            }
        }
    }
    let mut storage: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c"/".as_ptr(), &mut storage) } == 0 {
        data.insert(
            "Root storage".into(),
            format!(
                "{:.1} GiB available / {:.1} GiB total",
                storage.f_bavail as f64 * storage.f_frsize as f64 / 1073741824.,
                storage.f_blocks as f64 * storage.f_frsize as f64 / 1073741824.
            ),
        );
    }
    data.insert("Session".into(), "labwc / Wayland".into());
    data.insert("Settings".into(), env!("CARGO_PKG_VERSION").into());
    data
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unpublished_or_stale_compositor_is_not_notified() {
        assert_eq!(live_compositor(None), None);
        assert_eq!(live_compositor(Some("".into())), None);
        assert_eq!(live_compositor(Some("not-a-pid".into())), None);
        assert_eq!(live_compositor(Some("0".into())), None);
        assert_eq!(live_compositor(Some("-1".into())), None);
        assert_eq!(live_compositor(Some(libc::pid_t::MAX.to_string())), None);
        let self_pid = std::process::id() as libc::pid_t;
        assert_eq!(live_compositor(Some(self_pid.to_string())), Some(self_pid));
    }

    #[test]
    fn settings_still_persist_without_a_running_compositor() {
        assert!(reconfigure_compositor(None).is_ok());
    }
}
