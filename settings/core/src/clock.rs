//! ConnMan SNTP is the only clock writer. Read its policy over the system bus and
//! take synchronization truth from the kernel, never from a daemon claiming success.
use crate::{DateTime, Result, bus};
use glib::variant::{ObjectPath, ToVariant};
use std::{fs, path::Path};
pub const CLOCK_INTERFACE: &str = "net.connman.Clock";
const MAIN_CONF: &str = "/etc/connman/main.conf";
const SETTINGS: &str = "/var/lib/connman/settings";
/// Values of a `key=value` entry inside one section of a ConnMan key file.
/// Comma-separated lists are expanded, matching ConnMan's own parser.
fn key_file_values(path: &Path, section: &str, key: &str) -> Vec<String> {
    let text = fs::read_to_string(path).unwrap_or_default();
    let mut current = "";
    let mut found = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            current = name;
            continue;
        }
        let Some((name, value)) = line.split_once('=') else {
            continue;
        };
        if current != section || name.trim() != key {
            continue;
        }
        found.extend(
            value
                .split(',')
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map(str::to_string),
        );
    }
    found
}
/// ConnMan's persisted policy, used when ConnMan itself cannot be reached.
fn persisted_auto(key: &str) -> bool {
    key_file_values(Path::new(SETTINGS), "global", key)
        .last()
        .is_some_and(|v| v == "auto")
}
fn push_timeservers<'a>(servers: &mut Vec<String>, candidates: impl Iterator<Item = &'a str>) {
    for server in candidates {
        if crate::validate::server(server) && !servers.iter().any(|s| s == server) {
            servers.push(server.to_string());
        }
    }
}
/// Timeservers advertised by online services, or an empty list when ConnMan cannot answer.
fn service_timeservers_sync(bus: Option<&gio::DBusConnection>) -> Vec<String> {
    let Some(bus) = bus else {
        return Vec::new();
    };
    let Ok(reply) = bus.call_sync(
        Some("net.connman"),
        "/",
        "net.connman.Manager",
        "GetServices",
        None,
        None,
        gio::DBusCallFlags::NONE,
        2000,
        gio::Cancellable::NONE,
    ) else {
        return Vec::new();
    };
    service_timeservers(&reply)
}
fn service_timeservers(reply: &glib::Variant) -> Vec<String> {
    let mut servers = Vec::new();
    for (_, properties) in reply
        .get::<(Vec<(ObjectPath, bus::Properties)>,)>()
        .unwrap_or_default()
        .0
    {
        if !["ready", "online"].contains(&bus::text(&properties, "State").as_str()) {
            continue;
        }
        push_timeservers(
            &mut servers,
            bus::strings(&properties, "Timeservers")
                .iter()
                .map(String::as_str),
        );
    }
    servers
}
/// Everything the clock reports that does not need the bus, read away from the main context.
struct Local {
    timezone: String,
    persisted_auto: bool,
    global: Vec<String>,
    fallback: Vec<String>,
    kernel: i32,
    unsync: bool,
    utc_seconds: i64,
}
fn read_local() -> Local {
    let mut time: libc::timex = unsafe { std::mem::zeroed() };
    let kernel = unsafe { libc::adjtimex(&mut time) };
    Local {
        timezone: fs::read_to_string("/etc/timezone")
            .unwrap_or_else(|_| "UTC".into())
            .trim()
            .to_string(),
        persisted_auto: persisted_auto("TimeUpdates"),
        global: key_file_values(Path::new(SETTINGS), "global", "Timeservers"),
        fallback: key_file_values(Path::new(MAIN_CONF), "General", "FallbackTimeservers"),
        kernel,
        // STA_UNSYNC: the kernel clears it once a time source disciplines the clock.
        unsync: time.status & 0x40 != 0,
        utc_seconds: unsafe { libc::time(std::ptr::null_mut()) } as i64,
    }
}
/// Joins the kernel and ConnMan halves of the report. `service_servers` holds the
/// DHCP-advertised servers, which outrank the persisted ones.
fn assemble(
    local: Local,
    clock: Option<&bus::Properties>,
    service_servers: Vec<String>,
) -> DateTime {
    let enabled = clock
        .map(|clock| bus::text(clock, "TimeUpdates") == "auto")
        .unwrap_or(local.persisted_auto);
    let mut servers = Vec::new();
    push_timeservers(&mut servers, service_servers.iter().map(String::as_str));
    push_timeservers(&mut servers, local.global.iter().map(String::as_str));
    push_timeservers(
        &mut servers,
        clock
            .map(|clock| bus::strings(clock, "Timeservers"))
            .unwrap_or_default()
            .iter()
            .map(String::as_str),
    );
    push_timeservers(&mut servers, local.fallback.iter().map(String::as_str));
    DateTime {
        timezone: local.timezone,
        time_sync_enabled: enabled,
        // ConnMan only queries a timeserver while its Clock interface answers and
        // its policy is automatic, so this is the synchronizer's real state.
        sntp_running: clock.is_some() && enabled,
        synchronized: if local.kernel >= 0 && enabled {
            Some(!local.unsync)
        } else {
            None
        },
        timeservers: servers,
        utc_seconds: local.utc_seconds,
    }
}
pub fn status() -> DateTime {
    let bus = gio::bus_get_sync(gio::BusType::System, gio::Cancellable::NONE).ok();
    let clock = bus.as_ref().and_then(|bus| clock_properties_sync(bus));
    let services = service_timeservers_sync(bus.as_ref());
    assemble(read_local(), clock.as_ref(), services)
}
/// [`status`] without blocking the caller: both ConnMan round trips are asynchronous
/// and the kernel and configuration reads run on the blocking pool. The caller supplies
/// the system bus it already holds, so a refresh cannot stall the main context for the
/// seconds an unresponsive ConnMan takes to time out.
pub async fn status_async(bus: &gio::DBusConnection) -> DateTime {
    let clock = clock_properties(bus).await;
    let services = match bus
        .call_future(
            Some("net.connman"),
            "/",
            "net.connman.Manager",
            "GetServices",
            None,
            None,
            gio::DBusCallFlags::NONE,
            2000,
        )
        .await
    {
        Ok(reply) => service_timeservers(&reply),
        Err(_) => Vec::new(),
    };
    let local = match gio::spawn_blocking(read_local).await {
        Ok(local) => local,
        // The blocking pool only drops the task if its own thread died; read inline.
        Err(_) => read_local(),
    };
    assemble(local, clock.as_ref(), services)
}
fn clock_properties_sync(bus: &gio::DBusConnection) -> Option<bus::Properties> {
    bus.call_sync(
        Some("net.connman"),
        "/",
        CLOCK_INTERFACE,
        "GetProperties",
        None,
        None,
        gio::DBusCallFlags::NONE,
        2000,
        gio::Cancellable::NONE,
    )
    .ok()
    // GetProperties returns the dictionary wrapped in a one-element tuple.
    .and_then(|reply| reply.get::<(bus::Properties,)>().map(|v| v.0))
}
async fn clock_properties(bus: &gio::DBusConnection) -> Option<bus::Properties> {
    bus.call_future(
        Some("net.connman"),
        "/",
        CLOCK_INTERFACE,
        "GetProperties",
        None,
        None,
        gio::DBusCallFlags::NONE,
        2000,
    )
    .await
    .ok()
    .and_then(|reply| reply.get::<(bus::Properties,)>().map(|v| v.0))
}
/// Flip ConnMan's own policy. ConnMan persists it and re-synchronizes immediately.
pub fn set_time_sync(enabled: bool) -> Result<()> {
    let bus = gio::bus_get_sync(gio::BusType::System, gio::Cancellable::NONE)
        .map_err(|e| e.message().to_string())?;
    let value = if enabled { "auto" } else { "manual" };
    bus.call_sync(
        Some("net.connman"),
        "/",
        CLOCK_INTERFACE,
        "SetProperty",
        Some(&("TimeUpdates", value.to_variant()).to_variant()),
        None,
        gio::DBusCallFlags::NONE,
        10_000,
        gio::Cancellable::NONE,
    )
    .map_err(|e| e.message().to_string())?;
    Ok(())
}
