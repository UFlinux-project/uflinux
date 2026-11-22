//! Publish recording controls on the session bus for the desktop panel.
use gio::prelude::*;

use crate::{backend::Recorder, recording_bus};

pub struct Controls {
    bus: gio::DBusConnection,
    export: Option<gio::ActionGroupExportId>,
    group: gio::SimpleActionGroup,
}

impl Controls {
    pub fn claim() -> Result<Self, String> {
        let bus = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE)
            .map_err(|error| error.to_string())?;
        let group = gio::SimpleActionGroup::new();
        let export = bus
            .export_action_group(recording_bus::PATH, &group)
            .map_err(|error| error.to_string())?;
        let controls = Self {
            bus,
            export: Some(export),
            group,
        };
        // DO_NOT_QUEUE. A second shortcut press must not start a recorder later.
        let result = controls
            .bus
            .call_sync(
                Some("org.freedesktop.DBus"),
                "/org/freedesktop/DBus",
                "org.freedesktop.DBus",
                "RequestName",
                Some(&(recording_bus::NAME, 4u32).to_variant()),
                None,
                gio::DBusCallFlags::NONE,
                2000,
                gio::Cancellable::NONE,
            )
            .map_err(|error| error.to_string())?;
        if result.get::<(u32,)>() != Some((1,)) {
            return Err("A screen recording is already running".into());
        }
        Ok(controls)
    }

    pub fn attach(&self, recorder: &Recorder) {
        let group = &self.group;
        let stop = gio::SimpleAction::new("stop", None);
        let pause = gio::SimpleAction::new_stateful("pause", None, &false.to_variant());
        let started =
            gio::SimpleAction::new_stateful("started", None, &glib::monotonic_time().to_variant());
        started.set_enabled(false);
        stop.connect_activate({
            let recorder = recorder.clone();
            let pause = pause.clone();
            move |action, _| {
                action.set_enabled(false);
                pause.set_enabled(false);
                recorder.stop();
            }
        });
        pause.connect_change_state({
            let recorder = recorder.clone();
            move |action, state| {
                let Some(paused) = state.and_then(|state| state.get::<bool>()) else {
                    return;
                };
                if recorder.set_paused(paused).is_ok() {
                    action.set_state(&paused.to_variant());
                }
            }
        });
        group.add_action(&started);
        group.add_action(&pause);
        group.add_action(&stop);
    }
}

impl Drop for Controls {
    fn drop(&mut self) {
        // The process exits after releasing the exported controls. D-Bus
        // removes the well-known name when the connection closes.
        if let Some(export) = self.export.take() {
            self.bus.unexport_action_group(export);
        }
    }
}

pub fn stop() -> Result<(), String> {
    let bus = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE)
        .map_err(|error| error.to_string())?;
    bus.call_sync(
        Some(recording_bus::NAME),
        recording_bus::PATH,
        "org.gtk.Actions",
        "Activate",
        Some(
            &(
                "stop",
                Vec::<glib::Variant>::new(),
                std::collections::HashMap::<String, glib::Variant>::new(),
            )
                .to_variant(),
        ),
        None,
        gio::DBusCallFlags::NO_AUTO_START,
        2000,
        gio::Cancellable::NONE,
    )
    .map_err(|error| error.to_string())?;
    Ok(())
}
