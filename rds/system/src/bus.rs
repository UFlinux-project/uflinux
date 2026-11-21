//! Minimal system-bus plumbing, inlined from `settings-core::bus` so this
//! crate stays independent of the Settings tree.

use crate::Result;
use glib::variant::ToVariant;

pub const SYSTEM_XML: &str = r#"<node><interface name="org.uflinux.Rds.System1">
<method name="Execute"><arg type="s" direction="in"/><arg type="s" direction="out"/></method>
</interface></node>"#;

pub async fn call(
    connection: &gio::DBusConnection,
    name: &str,
    path: &str,
    interface: &str,
    method: &str,
    parameters: Option<&glib::Variant>,
) -> Result<glib::Variant> {
    connection
        .call_future(
            Some(name),
            path,
            interface,
            method,
            parameters,
            None,
            gio::DBusCallFlags::NONE,
            120_000,
        )
        .await
        .map_err(|e| e.message().to_string())
}

pub fn request_name(connection: &gio::DBusConnection, name: &str) -> Result<()> {
    let v = connection
        .call_sync(
            Some("org.freedesktop.DBus"),
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
            "RequestName",
            Some(&(name, 4u32).to_variant()),
            None,
            gio::DBusCallFlags::NONE,
            5000,
            gio::Cancellable::NONE,
        )
        .map_err(|e| e.to_string())?;
    if v.get::<(u32,)>() != Some((1,)) {
        return Err("RDS system service is already running".into());
    }
    Ok(())
}
