//! Privileged CUPS and SANE administration behind the Printing panel.
//!
//! Everything here runs as root inside the D-Bus activated system service.
//! Alpine ships cupsd with `PeerCred on` and `SystemGroup lpadmin`, and the
//! root account is not a member of that group out of the box, so even uid 0 is
//! refused by the scheduler. Enabling the feature therefore adds root to
//! `lpadmin` once; from then on the stock command line tools are the whole API,
//! which keeps this helper free of libcups and of any CUPS version assumption.
//!
//! The same holds for scanning: `scanimage` is the only SANE client used, and
//! it is pointed at a generated configuration directory because two of the
//! stock backends abort instead of reporting "no devices" when no session bus
//! is reachable.

use crate::operations::command;
use serde_json::{Value, json};
use settings_core::{
    DeviceKind, DiscoveredDevice, PrintingObservation, PrinterQueue, PrinterState,
    PrintingSystemAction, Result, ScannerDevice, ScannerKind, split_scanner_description,
};
use std::{
    collections::BTreeSet,
    ffi::CString,
    fs,
    io::Read,
    os::unix::fs::{PermissionsExt, symlink},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

/// Generated SANE configuration directory. Rebuilt whenever the installed
/// backend list changes, so an upgrade that adds a backend is picked up by the
/// next probe.
const SANE_DIR: &str = "/run/uflinux-printing/sane.d";
const SANE_STAGING: &str = "/run/uflinux-printing/sane.d.new";
const SANE_SYSTEM_DIR: &str = "/etc/sane.d";
/// Backends that abort instead of failing cleanly without a session bus.
const SANE_UNSAFE_BACKENDS: [&str; 2] = ["gphoto2", "hpaio"];
const TEST_PAGE: &str = "/usr/share/cups/data/default-testpage.pdf";
const SKEL: &str = "/etc/skel";
const DESKTOP_ID: &str = "org.gnome.SimpleScan.desktop";
const SYSTEM_DESKTOP: &str = "/usr/share/applications/org.gnome.SimpleScan.desktop";
const CUPS_SERVICE: &str = "cupsd";
const AVAHI_SERVICE: &str = "avahi-daemon";
/// OpenRC drops one empty file per running service here. The daemon reads the
/// very same marker, so both halves of the surface agree on what "running" is.
const STARTED_DIR: &str = "/run/openrc/started";
/// Groups that make a locally attached device usable from a desktop session.
/// Scheduler administration stays with the helper alone.
const USER_GROUPS: [&str; 2] = ["lp", "scanner"];
/// Words SANE uses for a transport in a device description. Anything else has
/// to stay, because it is the beginning of the maker name.
const TRANSPORT_WORDS: [&str; 5] = ["network", "usb", "scsi", "parallel", "serial"];

/// One account from /etc/passwd, the only account database Alpine uses here.
struct Account {
    name: String,
    uid: u32,
    gid: u32,
    home: PathBuf,
}

/// Runs one privileged verb. The caller already authorised it through polkit.
pub fn execute(action: PrintingSystemAction) -> Result<Value> {
    use PrintingSystemAction::*;
    match action {
        Status {} => serde_json::to_value(observe()).map_err(|e| e.to_string()),
        Enable {} => {
            enable()?;
            Ok(json!({"ok": true}))
        }
        Disable {} => {
            disable()?;
            Ok(json!({"ok": true}))
        }
        Start {} => {
            service(AVAHI_SERVICE, "start");
            service(CUPS_SERVICE, "start");
            Ok(json!({"ok": true}))
        }
        Stop {} => {
            service(CUPS_SERVICE, "stop");
            service(AVAHI_SERVICE, "stop");
            Ok(json!({"ok": true}))
        }
        SetShared { shared } => {
            let flag = if shared {
                "--share-printers"
            } else {
                "--no-share-printers"
            };
            command("/usr/sbin/cupsctl", &[flag], 10)?;
            Ok(json!({"ok": true}))
        }
        AddPrinter { name, uri, driver } => {
            valid_printer(&name)?;
            valid_uri(&uri)?;
            add_printer(&name, &uri, &driver)?;
            Ok(json!({"ok": true}))
        }
        RemovePrinter { name } => {
            valid_printer(&name)?;
            command("/usr/sbin/lpadmin", &["-x", &name], 20)?;
            Ok(json!({"ok": true}))
        }
        SetDefaultPrinter { name } => {
            let target = if name.is_empty() {
                "none".to_string()
            } else {
                valid_printer(&name)?;
                name
            };
            command("/usr/sbin/lpadmin", &["-d", &target], 20)?;
            Ok(json!({"ok": true}))
        }
        SetPrinterEnabled { name, enabled } => {
            valid_printer(&name)?;
            let (state, flow) = if enabled {
                ("/usr/sbin/cupsenable", "/usr/sbin/cupsaccept")
            } else {
                ("/usr/sbin/cupsdisable", "/usr/sbin/cupsreject")
            };
            command(state, &[&name], 20)?;
            // A queue that stops accepting jobs is the difference between a
            // paused printer and one that silently fills its error queue.
            let _ = command(flow, &[&name], 20);
            Ok(json!({"ok": true}))
        }
        TestPage { name } => {
            valid_printer(&name)?;
            if !queue_names().contains(&name) {
                return Err(format!("{name} is not a configured printer"));
            }
            if !Path::new(TEST_PAGE).exists() {
                return Err("The CUPS test page is missing from this image".into());
            }
            command(
                "/usr/bin/lp",
                &["-d", &name, "-t", "UFLinux test page", TEST_PAGE],
                20,
            )?;
            Ok(json!({"ok": true}))
        }
    }
}

/// CUPS itself is stricter about some characters than it documents, and the
/// name also becomes a PPD filename, so keep it to a conservative subset.
fn valid_printer(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > 127 {
        return Err("Use a printer name of 1 to 127 characters".into());
    }
    if !name.starts_with(|first: char| first.is_ascii_alphanumeric()) {
        return Err("A printer name must start with a letter or a digit".into());
    }
    if !name
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
    {
        return Err(
            "Use only letters, digits, dot, underscore or hyphen in a printer name".into(),
        );
    }
    Ok(())
}

/// Only schemes CUPS can actually reach, so a stray `file:` cannot make the
/// scheduler open an arbitrary local path.
fn valid_uri(uri: &str) -> Result<()> {
    if uri.is_empty() {
        return Ok(());
    }
    if uri.len() > 1024 || uri.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err("The device address is not usable".into());
    }
    let scheme = uri
        .split_once(':')
        .map(|(scheme, _)| scheme)
        .unwrap_or("");
    if !matches!(
        scheme,
        "ipp" | "ipps"
            | "https"
            | "http"
            | "socket"
            | "lpd"
            | "smb"
            | "dnssd"
            | "beh"
            | "usb"
            | "hp"
            | "hpnp"
            | "hpusb"
            | "usbhp"
            | "direct"
            | "ippusb"
            | "serial"
            | "parallel"
    ) {
        return Err("Choose a supported device address from the list".into());
    }
    Ok(())
}

/// Best-effort variant for idempotent setup steps that may legitimately fail,
/// such as adding a group the image already provides.
fn best_effort(executable: &str, args: &[&str]) {
    let _ = command(executable, args, 15);
}

fn service(name: &str, verb: &str) {
    let _ = command("/sbin/rc-service", &[name, verb], 60);
}

fn group_members(group: &str) -> Option<BTreeSet<String>> {
    let text = fs::read_to_string("/etc/group").ok()?;
    let prefix = format!("{group}:");
    text.lines().find_map(|line| line.strip_prefix(&prefix)).map(|rest| {
        let mut members = BTreeSet::new();
        if let Some(list) = rest.split(':').nth(1) {
            members.extend(list.split(',').filter(|m| !m.is_empty()).map(str::to_string));
        }
        members
    })
}

fn group_exists(group: &str) -> bool {
    group_members(group).is_some()
}

fn add_to_group(user: &str, group: &str) {
    if group_members(group).is_some_and(|members| members.contains(user)) {
        return;
    }
    best_effort("/usr/sbin/adduser", &[user, group]);
}

/// Interactive accounts: everything at or above Alpine's first user uid.
fn accounts() -> Vec<Account> {
    let Ok(text) = fs::read_to_string("/etc/passwd") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for line in text.lines() {
        let fields: Vec<&str> = line.split(':').collect();
        if fields.len() < 7 {
            continue;
        }
        let (Ok(uid), Ok(gid)) = (fields[2].parse::<u32>(), fields[3].parse::<u32>()) else {
            continue;
        };
        if uid < 1000 {
            continue;
        }
        out.push(Account {
            name: fields[0].to_string(),
            uid,
            gid,
            home: PathBuf::from(fields[5]),
        });
    }
    out
}

/// Turns the feature on: groups first so the scheduler can be administered,
/// then the runlevel, then the services, then application visibility.
fn enable() -> Result<()> {
    for group in USER_GROUPS {
        if !group_exists(group) {
            best_effort("/usr/sbin/addgroup", &["-S", group]);
        }
    }
    // cupsd authenticates administrative requests against lpadmin, root
    // included, and this helper is the only thing that administers CUPS.
    add_to_group("root", "lpadmin");
    // Every desktop account gets device access. The new membership lands at the
    // next login, which is also when a freshly attached device shows up.
    for account in accounts() {
        for group in USER_GROUPS {
            if group_exists(group) {
                add_to_group(&account.name, group);
            }
        }
    }
    best_effort("/sbin/rc-update", &["add", CUPS_SERVICE, "default"]);
    best_effort("/sbin/rc-update", &["add", AVAHI_SERVICE, "default"]);
    service(AVAHI_SERVICE, "start");
    service(CUPS_SERVICE, "start");
    sync_document_scanner(true);
    Ok(())
}

/// Turns the feature off. Group memberships are kept: they grant nothing while
/// cupsd is down, and dropping them would needlessly surprise a later re-enable.
fn disable() -> Result<()> {
    service(CUPS_SERVICE, "stop");
    service(AVAHI_SERVICE, "stop");
    best_effort("/sbin/rc-update", &["del", CUPS_SERVICE, "default"]);
    best_effort("/sbin/rc-update", &["del", AVAHI_SERVICE, "default"]);
    sync_document_scanner(false);
    Ok(())
}

/// Shows or hides GNOME Document Scanner without touching the package.
///
/// The start menu reads a per-user desktop entry in preference to the system
/// one, so the override is written into every interactive home directory and
/// into /etc/skel for accounts created later. A full copy of the entry is
/// written rather than a stub, because the entry that wins is used verbatim.
fn sync_document_scanner(enabled: bool) {
    let template = Path::new(SYSTEM_DESKTOP);
    sync_desktop(Path::new(SKEL), template, enabled, None);
    for account in accounts() {
        if account.home.as_os_str().is_empty() || account.home == Path::new("/") {
            continue;
        }
        sync_desktop(&account.home, template, enabled, Some((account.uid, account.gid)));
    }
}

fn sync_desktop(home: &Path, template: &Path, enabled: bool, owner: Option<(u32, u32)>) {
    let directory = home.join(".local/share/applications");
    let override_path = directory.join(DESKTOP_ID);
    if enabled {
        let _ = fs::remove_file(&override_path);
        return;
    }
    let Ok(body) = fs::read(template) else {
        return;
    };
    let mut text = String::from_utf8_lossy(&body).into_owned();
    if !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str("NoDisplay=true\n");
    if fs::create_dir_all(&directory).is_err() {
        return;
    }
    let _ = fs::set_permissions(&directory, fs::Permissions::from_mode(0o755));
    if fs::write(&override_path, text.as_bytes()).is_err() {
        return;
    }
    let _ = fs::set_permissions(&override_path, fs::Permissions::from_mode(0o644));
    if let Some((uid, gid)) = owner {
        chown(&directory, uid, gid);
        chown(&override_path, uid, gid);
    }
}

fn chown(path: &Path, uid: u32, gid: u32) {
    let Ok(name) = CString::new(path.as_os_str().as_encoded_bytes()) else {
        return;
    };
    unsafe { libc::chown(name.as_ptr(), uid, gid) };
}

/// Reads the queue names only, used to reject a test page for an unknown
/// printer without paying for a full observation.
fn queue_names() -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    if let Ok(text) = command("/usr/bin/lpstat", &["-p"], 10) {
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("printer ")
                && let Some((name, _)) = rest.split_once(' ')
            {
                names.insert(name.to_string());
            }
        }
    }
    names
}

/// One read-only sweep. Nothing here is fatal: a scheduler that refuses, a
/// backend that aborts, or a tool that is simply absent all yield empty lists,
/// because the panel has to stay usable enough to turn the feature back off.
fn observe() -> PrintingObservation {
    let mut observed = PrintingObservation::default();
    if !Path::new("/usr/sbin/cupsd").exists() {
        return observed;
    }
    // Every probe below is a client request to the scheduler, and each one
    // waits out its whole timeout while cupsd is down. The stack ships
    // installed but inert, so the marker is what keeps one read-only request
    // from turning into minutes of blocking; the daemon applies the same rule
    // before it forwards a status request at all.
    if !scheduler_running() {
        return observed;
    }
    ensure_root_admin();
    if let Ok(text) = command("/usr/bin/lpstat", &["-p", "-d"], 10) {
        parse_queues(&text, &mut observed);
    }
    if let Ok(text) = command("/usr/bin/lpstat", &["-v"], 10) {
        parse_devices(&text, &mut observed);
    }
    for queue in observed.queues.iter_mut() {
        queue.driver = ppd_model(&queue.name);
    }
    if let Ok(text) = command("/usr/sbin/lpinfo", &["-l", "-v"], 25) {
        parse_discovered(&text, &mut observed);
    }
    if let Ok(text) = command("/usr/sbin/cupsctl", &[], 10) {
        observed.shared = text
            .lines()
            .any(|line| line.starts_with("_share_printers=") && !line.ends_with("=0"));
    }
    observed.scanners = scan_scanners();
    observed
}

/// True while OpenRC reports cupsd as running. The daemon refuses to ask for a
/// fresh observation before this holds, and the check is repeated here so that
/// a direct status request stays cheap as well.
fn scheduler_running() -> bool {
    fs::symlink_metadata(format!("{STARTED_DIR}/{CUPS_SERVICE}")).is_ok()
}

/// cupsd refuses every administrative verb to a uid outside `lpadmin`, root
/// included, so the helper makes sure it is a member before it asks anything.
fn ensure_root_admin() {
    if !group_members("lpadmin").is_some_and(|members| members.contains("root")) {
        add_to_group("root", "lpadmin");
    }
}

/// Creates a queue. `everywhere` is the modern driverless path, but it fails
/// against a host that does not answer IPP, so a raw queue is the fallback: a
/// queue the user can see and remove beats an error with no explanation.
fn add_printer(name: &str, uri: &str, driver: &str) -> Result<()> {
    let driver = driver.trim();
    let mut arguments = vec!["-p", name, "-E", "-v", uri];
    if !driver.is_empty() {
        arguments.extend(["-m", driver]);
    } else if uri.contains(':') {
        arguments.extend(["-m", "everywhere"]);
    }
    if command("/usr/sbin/lpadmin", &arguments, 60).is_ok() {
        return Ok(());
    }
    if driver.is_empty() && uri.contains(':') {
        command("/usr/sbin/lpadmin", &["-p", name, "-E", "-v", uri, "-m", "raw"], 60)?;
        return Ok(());
    }
    Err("CUPS rejected the printer. Check the address and try again.".into())
}

fn parse_queues(text: &str, observed: &mut PrintingObservation) {
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix('\t') {
            let note = rest.trim();
            if note.is_empty() {
                continue;
            }
            if let Some(queue) = observed.queues.last_mut() {
                if !queue.detail.is_empty() {
                    queue.detail.push(' ');
                }
                queue.detail.push_str(note);
                let lowered = note.to_lowercase();
                if lowered.contains("rejecting") || lowered.contains("not accepting") {
                    queue.accepting = false;
                }
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("system default destination:") {
            observed.default = rest.trim().to_string();
            continue;
        }
        let Some(rest) = line.strip_prefix("printer ") else {
            continue;
        };
        let Some((name, tail)) = rest.split_once(' ') else {
            continue;
        };
        let tail = tail.trim_start_matches("is ");
        let state = if tail.starts_with("idle") {
            PrinterState::Idle
        } else if tail.starts_with("printing") || tail.starts_with("processing") {
            PrinterState::Processing
        } else if tail.starts_with("stopped") || tail.starts_with("disabled") {
            PrinterState::Stopped
        } else {
            PrinterState::Unknown
        };
        observed.queues.push(PrinterQueue {
            name: name.to_string(),
            state,
            uri: String::new(),
            driver: String::new(),
            is_default: false,
            accepting: true,
            detail: String::new(),
            kind: DeviceKind::Other,
        });
    }
    for queue in observed.queues.iter_mut() {
        queue.is_default = !observed.default.is_empty() && queue.name == observed.default;
    }
}

fn parse_devices(text: &str, observed: &mut PrintingObservation) {
    for line in text.lines() {
        let Some(rest) = line.strip_prefix("device for ") else {
            continue;
        };
        let Some((name, uri)) = rest.split_once(": ") else {
            continue;
        };
        let uri = uri.trim();
        if let Some(queue) = observed.queues.iter_mut().find(|queue| queue.name == name) {
            queue.uri = uri.to_string();
            queue.kind = DeviceKind::from_uri(uri);
        }
    }
}

/// The PPD is the only place a queue's driver is recorded, and it is 0640
/// root:lp, which is exactly why the probe cannot run in the user session.
fn ppd_model(name: &str) -> String {
    let Ok(text) = fs::read_to_string(format!("/etc/cups/ppd/{name}.ppd")) else {
        return String::new();
    };
    for line in text.lines().take(200) {
        let Some(rest) = line.strip_prefix("*ModelName:") else {
            continue;
        };
        let model = rest.trim().trim_matches('"').trim();
        if !model.is_empty() && !model.eq_ignore_ascii_case("generic") {
            return model.chars().take(120).collect();
        }
    }
    String::new()
}

fn parse_discovered(text: &str, observed: &mut PrintingObservation) {
    let taken: BTreeSet<String> = observed
        .queues
        .iter()
        .map(|queue| queue.uri.clone())
        .filter(|uri| !uri.is_empty())
        .collect();
    let mut current: Option<DiscoveredDevice> = None;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("Device: uri = ") {
            if let Some(device) = current.take() {
                push_device(observed, &taken, device);
            }
            let uri = rest.trim().to_string();
            current = Some(DiscoveredDevice {
                kind: DeviceKind::from_uri(&uri),
                uri,
                description: String::new(),
            });
            continue;
        }
        let Some(device) = current.as_mut() else {
            continue;
        };
        let note = line.trim();
        if let Some(info) = note.strip_prefix("make-and-model = ") {
            let info = clean_cups_field(info);
            if !info.is_empty() {
                device.description = info;
            }
        } else if let Some(info) = note.strip_prefix("info = ") {
            let info = clean_cups_field(info);
            if device.description.is_empty() && !info.is_empty() {
                device.description = info;
            }
        }
    }
    if let Some(device) = current {
        push_device(observed, &taken, device);
    }
}

/// `lpinfo` prints unset metadata as the literal `(null)`.
fn clean_cups_field(value: &str) -> String {
    let value = value.trim();
    if value.is_empty() || value == "(null)" {
        String::new()
    } else {
        value.to_string()
    }
}

fn push_device(
    observed: &mut PrintingObservation,
    taken: &BTreeSet<String>,
    device: DiscoveredDevice,
) {
    // A backend that supports network printing advertises itself with a line
    // such as `Device: uri = network ipp`, which is a hint about how to add
    // something rather than a device, and always duplicates a real entry. A
    // usable device URI always carries a scheme.
    if device.description.is_empty() || !device.uri.contains(':') || taken.contains(&device.uri) {
        return;
    }
    if !observed.devices.iter().any(|found| found.uri == device.uri) {
        observed.devices.push(device);
    }
}

/// Enumerates scanners through SANE. The generated configuration directory is
/// what makes this safe: `gphoto2` and `hpaio` both use a D-Bus connection that
/// does not exist in a bus-less system service and abort the process rather
/// than report an empty list.
fn scan_scanners() -> Vec<ScannerDevice> {
    let Some(directory) = prepare_sane_config() else {
        return Vec::new();
    };
    let Ok(mut child) = Command::new("/usr/bin/scanimage")
        .arg("-L")
        .env("LC_ALL", "C")
        .env("SANE_CONFIG_DIR", &directory)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    else {
        return Vec::new();
    };
    let began = Instant::now();
    loop {
        if let Ok(Some(status)) = child.try_wait() {
            // A backend that aborts means "no usable device list", never a
            // failure the user can act on.
            if !status.success() {
                return Vec::new();
            }
            let Some(mut stdout) = child.stdout.take() else {
                return Vec::new();
            };
            let mut text = String::new();
            let _ = stdout.by_ref().take(64 * 1024).read_to_string(&mut text);
            return parse_scanners(&text);
        }
        if began.elapsed() > Duration::from_secs(20) {
            let _ = child.kill();
            let _ = child.wait();
            return Vec::new();
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn parse_scanners(text: &str) -> Vec<ScannerDevice> {
    let mut scanners: Vec<ScannerDevice> = Vec::new();
    for line in text.lines() {
        let Some(rest) = line.strip_prefix("device `") else {
            continue;
        };
        let Some((name, tail)) = rest.split_once('\'') else {
            continue;
        };
        // SANE describes a device as "is a <transport> <vendor> <model>", but
        // the leading word is the vendor whenever the backend does not name a
        // transport, so only a real transport word may be dropped.
        let description = match tail.split_once(" is a ") {
            Some((_, words)) => {
                let words = words.trim();
                match words.split_once(char::is_whitespace) {
                    Some((first, rest))
                        if TRANSPORT_WORDS
                            .iter()
                            .any(|word| first.eq_ignore_ascii_case(word)) =>
                    {
                        rest
                    }
                    _ => words,
                }
            }
            None => "",
        };
        let (vendor, model) = split_scanner_description(description);
        let device = ScannerDevice {
            name: name.to_string(),
            vendor,
            model,
            kind: ScannerKind::from_backend(name),
        };
        if !device.name.is_empty() && !scanners.iter().any(|found| found.name == device.name) {
            scanners.push(device);
        }
    }
    scanners
}

/// Mirrors /etc/sane.d except for the backends that cannot be trusted to fail
/// cleanly, and returns the directory only when it is usable.
fn prepare_sane_config() -> Option<PathBuf> {
    let root = Path::new(SANE_DIR);
    let system = Path::new(SANE_SYSTEM_DIR);
    let entries = fs::read_dir(system.join("dll.d")).ok()?;
    let mut backends: Vec<(String, PathBuf)> = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let backend = name.strip_suffix(".conf").unwrap_or(&name).to_string();
        if backend.is_empty()
            || SANE_UNSAFE_BACKENDS.contains(&backend.as_str())
            || !entry.path().is_file()
        {
            continue;
        }
        backends.push((backend, entry.path()));
    }
    backends.sort();
    let wanted: String = backends
        .iter()
        .map(|(backend, _)| format!("{backend}\n"))
        .collect();
    if fs::read_to_string(root.join("dll.conf"))
        .ok()
        .as_deref()
        != Some(wanted.as_str())
    {
        rebuild_sane_dir(root, system, &backends, &wanted).ok()?;
    }
    Some(root.to_path_buf())
}

fn rebuild_sane_dir(
    root: &Path,
    system: &Path,
    backends: &[(String, PathBuf)],
    wanted: &str,
) -> Result<()> {
    let parent = root.parent().unwrap_or(Path::new("/run"));
    fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    let _ = fs::set_permissions(parent, fs::Permissions::from_mode(0o755));
    let staging = Path::new(SANE_STAGING);
    let _ = fs::remove_dir_all(staging);
    fs::create_dir_all(staging.join("dll.d")).map_err(|e| e.to_string())?;
    let _ = fs::set_permissions(staging, fs::Permissions::from_mode(0o755));
    // Every sibling configuration file has to resolve, so mirror the whole
    // directory rather than only the backend list.
    if let Ok(entries) = fs::read_dir(system) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() {
                let _ = symlink(path.as_os_str(), staging.join(entry.file_name()));
            }
        }
    }
    for (backend, source) in backends {
        let _ = symlink(source.as_os_str(), staging.join("dll.d").join(backend));
    }
    fs::write(staging.join("dll.conf"), wanted.as_bytes()).map_err(|e| e.to_string())?;
    let _ = fs::remove_dir_all(root);
    if fs::rename(staging, root).is_err() {
        let _ = fs::remove_dir_all(staging);
        return Err("Cannot prepare the scanner configuration".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn queues(text: &str) -> PrintingObservation {
        let mut observed = PrintingObservation::default();
        parse_queues(text, &mut observed);
        observed
    }

    #[test]
    fn an_idle_queue_and_a_default_are_recognised() {
        let observed = queues(
            "printer NetQ is idle.  enabled since Thu Nov 26 22:28:46 2026\n\
             system default destination: NetQ\n",
        );
        assert_eq!(observed.default, "NetQ");
        assert_eq!(observed.queues.len(), 1);
        assert_eq!(observed.queues[0].state, PrinterState::Idle);
        assert!(observed.queues[0].accepting);
        assert!(observed.queues[0].is_default);
    }

    #[test]
    fn a_paused_and_a_rejecting_queue_are_reported() {
        let observed = queues(
            "printer OffQ disabled since Thu Nov 26 22:28:46 2026 -\n\tPaused\n\
             printer RejQ is idle.  enabled since Thu Nov 26 22:28:46 2026 -\n\
             \tRejecting Jobs\n",
        );
        assert_eq!(observed.queues.len(), 2);
        assert_eq!(observed.queues[0].state, PrinterState::Stopped);
        assert_eq!(observed.queues[0].detail, "Paused");
        assert_eq!(observed.queues[1].state, PrinterState::Idle);
        assert!(!observed.queues[1].accepting);
    }

    #[test]
    fn an_empty_scheduler_is_not_an_error() {
        let observed = queues("lpstat: No destinations added.\nno system default destination\n");
        assert!(observed.queues.is_empty());
        assert!(observed.default.is_empty());
    }

    #[test]
    fn a_device_line_fills_the_uri_and_the_transport() {
        let mut observed = queues("printer NetQ is idle.  enabled since now\n");
        parse_devices("device for NetQ: ipp://192.0.2.10/ipp/print\n", &mut observed);
        assert_eq!(observed.queues[0].uri, "ipp://192.0.2.10/ipp/print");
        assert_eq!(observed.queues[0].kind, DeviceKind::Network);
    }

    #[test]
    fn discovered_devices_keep_the_description_and_drop_bare_backends() {
        let mut observed = queues("printer NetQ is idle.  enabled since now\n");
        parse_devices("device for NetQ: ipp://192.0.2.10/ipp/print\n", &mut observed);
        parse_discovered(
            "Device: uri = ipp://192.0.2.10/ipp/print\n        class = network\n\
             \x20       info = IPP Print Queue\n        make-and-model = EPSON WF-4820\n\
             Device: uri = network ipp\n        class = network\n\
             \x20       info = Internet Printing Protocol\n\
             Device: uri = hp:/net/EPSON?ip=192.0.2.20\n        class = network\n\
             \x20       make-and-model = EPSON WF-4820\n",
            &mut observed,
        );
        assert_eq!(observed.devices.len(), 1);
        assert_eq!(observed.devices[0].uri, "hp:/net/EPSON?ip=192.0.2.20");
        assert_eq!(observed.devices[0].kind, DeviceKind::Network);
    }

    #[test]
    fn scanner_lines_split_the_transport_from_the_description() {
        let scanners = parse_scanners(
            "device `airscan:e0:EPSON WorkForce Pro WF-4820' is a network EPSON WorkForce Pro WF-4820\n\
             device `epson2:libusb:001:003' is a usb Epson GT-X800\n",
        );
        assert_eq!(scanners.len(), 2);
        assert_eq!(scanners[0].kind, ScannerKind::Network);
        assert_eq!(scanners[0].vendor, "EPSON");
        assert_eq!(scanners[0].model, "WorkForce Pro WF-4820");
        assert_eq!(scanners[1].kind, ScannerKind::Usb);
        assert_eq!(scanners[1].title(), "Epson GT-X800");
    }

    #[test]
    fn names_and_addresses_are_checked_before_reaching_cups() {
        assert!(valid_printer("Office-Jet_1").is_ok());
        assert!(valid_printer("").is_err());
        assert!(valid_printer("-x").is_err());
        assert!(valid_printer("two words").is_err());
        assert!(valid_uri("ipp://192.0.2.1/ipp/print").is_ok());
        assert!(valid_uri("http://host:631/printers/Q").is_ok());
        assert!(valid_uri("hp:/net/EPSON?ip=192.0.2.20").is_ok());
        assert!(valid_uri("file:/etc/shadow").is_err());
        assert!(valid_uri("ipp://host print").is_err());
        assert!(valid_uri("").is_ok());
    }
}
