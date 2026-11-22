//! D-Bus activation, polkit authorization, and privileged execution.

use crate::{RdpSystemOperation, Result, SYSTEM_NAME, SYSTEM_PATH, bus, certificate};
use glib::variant::ToVariant;
use std::{
    cell::Cell,
    collections::HashMap,
    io::Read,
    os::unix::process::CommandExt,
    path::Path,
    process::{Command, Stdio},
    rc::Rc,
    time::{Duration, Instant},
};

const SERVICE: &str = "uflinux-rds";
const RUNLEVEL: &str = "default";
const MAX_ACTIVE: u32 = 8;

pub fn run() -> Result<()> {
    // The caller must be root. D-Bus activation enforces this, while direct
    // execution must fail before it can claim the well-known name.
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } != 0 {
        return Err("The system service must be D-Bus activated as root".into());
    }
    let resident = std::env::args().any(|argument| argument == "--resident");
    let context = glib::MainContext::default();
    let (_connection, mainloop) = context.block_on(async {
        let connection = gio::bus_get_future(gio::BusType::System)
            .await
            .map_err(|error| error.to_string())?;
        let mainloop = glib::MainLoop::new(None, false);
        let active = Rc::new(Cell::new(0u32));
        let last = Rc::new(Cell::new(Instant::now()));
        let info = gio::DBusNodeInfo::for_xml(bus::SYSTEM_XML)
            .map_err(|error| error.to_string())?
            .interfaces()[0]
            .clone();
        let live = Rc::clone(&active);
        let recent = Rc::clone(&last);
        // D-Bus forwards the activating call as soon as the name is acquired.
        // Export the object first so that the first request cannot race startup.
        connection
            .register_object(SYSTEM_PATH, &info)
            .method_call(
                move |connection, sender, _, _, method, parameters, invocation| {
                    if method != "Execute" {
                        invocation.return_dbus_error(
                            "org.freedesktop.DBus.Error.UnknownMethod",
                            "Unknown operation",
                        );
                        return;
                    }
                    let Some((raw,)) = parameters.get::<(String,)>() else {
                        invocation.return_dbus_error(
                            "org.uflinux.Rds.System1.Error.Invalid",
                            "Invalid request",
                        );
                        return;
                    };
                    if raw.len() > crate::MAX_REQUEST || live.get() >= MAX_ACTIVE {
                        invocation.return_dbus_error(
                            "org.uflinux.Rds.System1.Error.Busy",
                            "Request too large or service busy",
                        );
                        return;
                    }
                    let operation = match serde_json::from_str::<RdpSystemOperation>(&raw) {
                        Ok(operation) => operation,
                        Err(_) => {
                            invocation.return_dbus_error(
                                "org.uflinux.Rds.System1.Error.Invalid",
                                "Unknown typed operation",
                            );
                            return;
                        }
                    };
                    let Some(sender) = sender.map(str::to_string) else {
                        return;
                    };
                    let connection = connection.clone();
                    let live = Rc::clone(&live);
                    let recent = Rc::clone(&recent);
                    live.set(live.get() + 1);
                    recent.set(Instant::now());
                    glib::MainContext::default().spawn_local(async move {
                        let result = async {
                            authorized(&connection, &sender, &operation).await?;
                            execute(&operation).await
                        }
                        .await;
                        match result {
                            Ok(value) => {
                                invocation.return_value(Some(&(value.to_string(),).to_variant()))
                            }
                            Err(error) => invocation
                                .return_dbus_error("org.uflinux.Rds.System1.Error.Failed", &error),
                        }
                        live.set(live.get() - 1);
                        recent.set(Instant::now());
                    });
                },
            )
            .build()
            .map_err(|error| error.to_string())?;
        bus::request_name(&connection, SYSTEM_NAME)?;

        // A normal helper exits after one request. --resident keeps it hot for
        // the Settings daemon, but still releases an idle daemon connection.
        let quit = mainloop.clone();
        glib::timeout_add_local(Duration::from_secs(10), move || {
            if !resident && active.get() == 0 && last.get().elapsed() > Duration::from_secs(45) {
                quit.quit();
                return glib::ControlFlow::Break;
            }
            glib::ControlFlow::Continue
        });
        Ok::<_, String>((connection, mainloop))
    })?;
    mainloop.run();
    Ok(())
}

async fn execute(operation: &RdpSystemOperation) -> Result<serde_json::Value> {
    match operation {
        RdpSystemOperation::Enable => {
            // The runlevel decides whether the broker starts, but the broker
            // reads its own state out of the configuration file, so both have
            // to move together or the switch flips back in the UI.
            persist_enabled(true).await?;
            rc_update(&["add", SERVICE, RUNLEVEL]).await?;
            rc_service(&[SERVICE, "start"]).await?;
            // A broker that was already running has not seen the file.
            let _ = rc_service(&[SERVICE, "reload"]).await;
            Ok(certificate::ok())
        }
        RdpSystemOperation::Disable => {
            persist_enabled(false).await?;
            // A broker that is already running has not seen the file; tell it
            // before the stop, in case the stop itself is what fails.
            let _ = rc_service(&[SERVICE, "reload"]).await;
            // Best effort: stopping an already-stopped service is not an error.
            let _ = rc_service(&[SERVICE, "stop"]).await;
            rc_update(&["del", SERVICE, RUNLEVEL]).await
        }
        RdpSystemOperation::Start => rc_service(&[SERVICE, "start"]).await,
        RdpSystemOperation::Stop => rc_service(&[SERVICE, "stop"]).await,
        RdpSystemOperation::ApplyCertificate {
            mode,
            certificate_path,
            key_path,
        } => {
            let paths = certificate::Paths::system();
            let mode = mode.clone();
            match mode.as_str() {
                "self-signed" => gio::spawn_blocking(move || certificate::self_signed(&paths))
                    .await
                    .map_err(|_| "System worker failed".to_string())?,
                "custom" => {
                    let Some(certificate_path) = certificate_path.clone() else {
                        return Err("custom mode requires certificate_path".into());
                    };
                    let Some(key_path) = key_path.clone() else {
                        return Err("custom mode requires key_path".into());
                    };
                    gio::spawn_blocking(move || {
                        certificate::install_custom(&paths, &certificate_path, &key_path)
                    })
                    .await
                    .map_err(|_| "System worker failed".to_string())?
                }
                other => Err(format!(
                    "Unknown certificate mode {other:?}; expected self-signed or custom"
                )),
            }
        }
        RdpSystemOperation::Status => certificate::status(&certificate::Paths::system()),
    }
}

/// OpenRC through the same argv-only boundary as every other helper command.
///
/// `executable` is an absolute path: this process is a polkit-spawned helper,
/// so its `PATH` has no `/sbin` and a bare `rc-update` would not be found.
fn command(executable: &Path, args: &[&str], timeout: u64) -> Result<String> {
    let name = executable
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| executable.to_string_lossy().into_owned());
    let mut command = Command::new(executable);
    command.args(args).env("LC_ALL", "C");
    let mut child = unsafe {
        command.pre_exec(|| {
            // SAFETY: umask is async-signal-safe and only affects this child.
            libc::umask(0o022);
            Ok(())
        })
    }
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::null())
    .spawn()
    .map_err(|_| format!("Cannot start {name}"))?;
    let started = Instant::now();
    loop {
        if let Some(status) = child
            .try_wait()
            .map_err(|_| "Process status unavailable".to_string())?
        {
            let mut output = String::new();
            if let Some(mut stdout) = child.stdout.take() {
                stdout
                    .by_ref()
                    .take(128 * 1024)
                    .read_to_string(&mut output)
                    .map_err(|_| "Process output unavailable".to_string())?;
            }
            return if status.success() {
                Ok(output)
            } else {
                Err(format!("{name} rejected the operation"))
            };
        }
        if started.elapsed() > Duration::from_secs(timeout) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("{name} timed out"));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

async fn rc_update(args: &[&'static str]) -> Result<serde_json::Value> {
    run_openrc(uflinux_rds_common::openrc::rc_update(), args).await
}

async fn rc_service(args: &[&'static str]) -> Result<serde_json::Value> {
    run_openrc(uflinux_rds_common::openrc::rc_service(), args).await
}

async fn run_openrc(
    executable: std::path::PathBuf,
    args: &[&'static str],
) -> Result<serde_json::Value> {
    let args = args.to_vec();
    gio::spawn_blocking(move || command(&executable, &args, 30).map(|_| certificate::ok()))
        .await
        .map_err(|_| "System worker failed".to_string())?
}

/// Writes the master switch into the configuration file the broker reads.
///
/// Failures are fatal for the operation: a service in the default runlevel
/// whose configuration still says `enabled: false` would start and immediately
/// refuse to listen, which is harder to explain than a rejected click.
async fn persist_enabled(enabled: bool) -> Result<()> {
    let path = uflinux_rds_common::config_file_path();
    gio::spawn_blocking(move || {
        uflinux_rds_common::Config::set_enabled_on_disk(&path, enabled)
            .map_err(|error| format!("Cannot update {}: {error}", path.display()))
    })
    .await
    .map_err(|_| "System worker failed".to_string())?
}

async fn authorized(
    bus: &gio::DBusConnection,
    sender: &str,
    operation: &RdpSystemOperation,
) -> Result<()> {
    let action = match operation {
        RdpSystemOperation::Status => return Ok(()),
        RdpSystemOperation::ApplyCertificate { .. } => "org.uflinux.rds.certificate",
        _ => "org.uflinux.rds.configure",
    };
    let details: HashMap<String, String> = HashMap::new();
    let subject = HashMap::from([("name".to_string(), sender.to_variant())]);
    let parameters = (("system-bus-name", subject), action, details, 1u32, "").to_variant();
    let reply = bus::call(
        bus,
        "org.freedesktop.PolicyKit1",
        "/org/freedesktop/PolicyKit1/Authority",
        "org.freedesktop.PolicyKit1.Authority",
        "CheckAuthorization",
        Some(&parameters),
    )
    .await?;
    let authorized = reply
        .get::<((bool, bool, HashMap<String, String>),)>()
        .map(|reply| reply.0.0)
        .unwrap_or(false);
    if authorized {
        Ok(())
    } else {
        Err("Administrator authorization was denied or canceled".into())
    }
}
