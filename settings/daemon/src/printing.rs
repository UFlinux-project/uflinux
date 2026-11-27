//! Printing and scanning panel support.
//!
//! The daemon never links libcups or libsane. Everything that inspects CUPS
//! goes through the Settings system service: the package installs every
//! configuration file 0640 root:lp, and cupsd's default policy refuses
//! `lpinfo -v` to anyone outside the printer-administrator group, so a session
//! process cannot enumerate anything on its own.
//!
//! The cheap facts, i.e. whether the stack is installed, enabled and running,
//! come from OpenRC's own world-readable state files, so a closed panel costs
//! a handful of stats and nothing is ever started by looking at it.

use glib::variant::ToVariant;
use serde_json::Value;
use settings_core::{
    Printing, PrintingObservation, PrintingSystemAction, bus,
};

/// Presence of the package. Nothing is probed while the scheduler is missing.
const CUPSD_BINARY: &str = "/usr/sbin/cupsd";
/// OpenRC keeps one symlink per service in a runlevel directory and one empty
/// file per service in a state directory, both world readable.
const RUNLEVEL_DIR: &str = "/etc/runlevels/default";
const STARTED_DIR: &str = "/run/openrc/started";
const CUPS_SERVICE: &str = "cupsd";
const AVAHI_SERVICE: &str = "avahi-daemon";

fn exists(path: String) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

/// True while the service is in the default runlevel, i.e. the toggle is on.
pub fn is_enabled() -> bool {
    exists(format!("{RUNLEVEL_DIR}/{CUPS_SERVICE}"))
}

fn started(service: &str) -> bool {
    exists(format!("{STARTED_DIR}/{service}"))
}

/// Builds the panel state. The device lists are only gathered while cupsd is
/// actually up, so a disabled feature never runs lpstat, lpinfo or scanimage.
pub async fn query(system: &gio::DBusConnection) -> Printing {
    if !exists(CUPSD_BINARY.to_owned()) {
        return Printing::default();
    }
    let base = Printing {
        installed: true,
        enabled: is_enabled(),
        running: started(CUPS_SERVICE),
        discovery: started(AVAHI_SERVICE),
        ..Default::default()
    };
    if !base.running {
        return base;
    }
    match observe(system).await {
        Ok(observed) => apply(base, observed),
        Err(error) => Printing {
            error: Some(error),
            ..base
        },
    }
}

/// Merges what the helper saw into the flags the daemon gathered itself.
fn apply(mut state: Printing, observed: PrintingObservation) -> Printing {
    state.shared = observed.shared;
    state.default = observed.default;
    state.queues = observed.queues;
    state.devices = observed.devices;
    state.scanners = observed.scanners;
    state
}

/// One read-only round trip to the helper.
async fn observe(system: &gio::DBusConnection) -> settings_core::Result<PrintingObservation> {
    let value = crate::printing::system(system, &PrintingSystemAction::Status {}).await?;
    serde_json::from_value(value)
        .map_err(|_| "The printing helper reported an unreadable state".into())
}

/// Forwards one privileged verb and returns the helper's reply.
pub async fn system(
    system: &gio::DBusConnection,
    operation: &PrintingSystemAction,
) -> settings_core::Result<Value> {
    // The helper speaks SystemAction, so the printing verb travels inside the
    // wrapper variant that owns its own polkit action.
    let request = serde_json::to_string(&settings_core::SystemAction::Printing {
        printing: operation.clone(),
    })
    .map_err(|_| "Cannot encode the printing operation")?;
    let reply = bus::call(
        system,
        settings_core::SYSTEM_NAME,
        settings_core::SYSTEM_PATH,
        settings_core::SYSTEM_NAME,
        "Execute",
        Some(&(request,).to_variant()),
    )
    .await?;
    let raw = reply
        .get::<(String,)>()
        .ok_or("Invalid printing system response")?
        .0;
    serde_json::from_str(&raw).map_err(|_| "Invalid printing system response".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_absent_helper_degrades_to_an_empty_panel() {
        let observed: PrintingObservation = serde_json::from_value(serde_json::json!({})).unwrap();
        let state = apply(
            Printing {
                installed: true,
                running: true,
                ..Default::default()
            },
            observed,
        );
        assert!(state.queues.is_empty());
        assert!(state.error.is_none());
        assert!(!state.shared);
    }

    #[test]
    fn a_failed_probe_keeps_the_service_flags() {
        let state = Printing {
            installed: true,
            enabled: true,
            running: true,
            error: Some("denied".into()),
            ..Default::default()
        };
        assert!(state.enabled && state.running);
    }
}
