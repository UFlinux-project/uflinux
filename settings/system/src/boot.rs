//! Root-side view and rewrite of the boot-time CPU mitigation setting.
//!
//! `/etc/uflinux/boot.conf` is what the user chose, and the boot entries are
//! what the firmware actually reads, so both are updated together and the
//! rewrite is rolled back when the entries do not come back as expected. A
//! device image ships without its boot partition mounted, so the helper
//! mounts it for the duration of one operation and unmounts it afterwards.
use crate::operations::command;
use settings_core::{
    boot::{arguments_value, parse_policy, read_policy, render_policy, rewrite_arguments, POLICY},
    files, *,
};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::Mutex,
};

static LOCK: Mutex<()> = Mutex::new(());
/// Scratch mount point for a boot partition that is not mounted at boot.
const MOUNT_POINT: &str = "/run/uflinux-settings/esp";
const FAT: &[&str] = &["vfat", "fat32", "msdos"];
const MOUNT: &str = "/usr/bin/mount";
const UMOUNT: &str = "/usr/bin/umount";

/// One kernel command line per file kind: GRUB and extlinux keep their boot
/// arguments on labelled lines, a Raspberry Pi command line is the whole file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Bare,
    Grub,
    Extlinux,
}
fn kind(path: &Path) -> Kind {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    if name.starts_with("cmdline") && name.ends_with(".txt") {
        return Kind::Bare;
    }
    if name == "extlinux.conf" {
        return Kind::Extlinux;
    }
    Kind::Grub
}
fn command_line(line: &str, kind: Kind) -> bool {
    let first = line.split_whitespace().next();
    match kind {
        Kind::Bare => !line.trim().is_empty(),
        Kind::Grub => matches!(first, Some("linux") | Some("linuxefi")),
        Kind::Extlinux => first.is_some_and(|word| word.eq_ignore_ascii_case("append")),
    }
}
/// Applies the policy to every command line of one boot-entry file.
fn rewrite_text(text: &str, kind: Kind, wanted: Mitigations) -> String {
    let mut updated = String::with_capacity(text.len());
    for (index, line) in text.split('\n').enumerate() {
        if index > 0 {
            updated.push('\n');
        }
        updated.push_str(
            &command_line(line, kind)
                .then(|| rewrite_arguments(line, wanted))
                .flatten()
                .unwrap_or_else(|| line.to_string()),
        );
    }
    updated
}

/// The boot-entry files a boot partition carries, whichever layout it uses.
fn entries(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for relative in [
        "grub/grub.cfg",
        "boot/grub/grub.cfg",
        "EFI/uflinux/grub.cfg",
        "extlinux/extlinux.conf",
    ] {
        let path = root.join(relative);
        if path.is_file() {
            found.push(path);
        }
    }
    for entry in fs::read_dir(root).into_iter().flatten().flatten() {
        let path = entry.path();
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default();
        if path.is_file() && name.starts_with("cmdline") && name.ends_with(".txt") {
            found.push(path);
        }
    }
    found.sort();
    found
}

/// A boot partition holding at least one boot entry, mounted if it was not.
struct Directory {
    root: PathBuf,
    temporary: bool,
}
impl Drop for Directory {
    fn drop(&mut self) {
        if self.temporary {
            let _ = command(UMOUNT, &[MOUNT_POINT], 10);
        }
    }
}
fn mounted(writable: bool) -> Option<Directory> {
    let text = fs::read_to_string("/proc/mounts").unwrap_or_default();
    let mut candidates = Vec::new();
    for line in text.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 4 || !FAT.contains(&fields[2]) {
            continue;
        }
        let readonly = fields[3].split(',').any(|option| option == "ro");
        // A read-only partition is enough to report the setting; a caller that
        // intends to write gets it remounted first.
        if readonly && writable && command(MOUNT, &["-o", "remount,rw", fields[1]], 10).is_err() {
            continue;
        }
        candidates.push(fields[1]);
    }
    for candidate in ["/boot", "/boot/efi", "/efi", MOUNT_POINT]
        .into_iter()
        .chain(candidates)
    {
        let root = Path::new(candidate);
        if !entries(root).is_empty() {
            return Some(Directory {
                root: root.to_path_buf(),
                temporary: false,
            });
        }
    }
    None
}
fn unmounted() -> Option<Directory> {
    fs::create_dir_all(MOUNT_POINT).ok()?;
    let _ = command(UMOUNT, &[MOUNT_POINT], 10);
    let listing = command(
        "lsblk",
        &[
            "--json",
            "--nodeps",
            "--paths",
            "--output",
            "PATH,TYPE,FSTYPE,MOUNTPOINTS",
        ],
        10,
    )
    .ok()?;
    let devices = serde_json::from_str::<serde_json::Value>(&listing)
        .ok()?
        .get("blockdevices")?
        .as_array()?
        .clone();
    for device in devices {
        if device.get("type").and_then(|value| value.as_str()) != Some("part")
            || !FAT.contains(
                &device
                    .get("fstype")
                    .and_then(|value| value.as_str())
                    .unwrap_or(""),
            )
            || device
                .get("mountpoints")
                .and_then(|value| value.as_array())
                .is_some_and(|list| !list.is_empty())
        {
            continue;
        }
        let Some(source) = device.get("path").and_then(|value| value.as_str()) else {
            continue;
        };
        if command(MOUNT, &["-t", "vfat", source, MOUNT_POINT], 10).is_err() {
            continue;
        }
        if entries(Path::new(MOUNT_POINT)).is_empty() {
            let _ = command(UMOUNT, &[MOUNT_POINT], 10);
            continue;
        }
        return Some(Directory {
            root: MOUNT_POINT.into(),
            temporary: true,
        });
    }
    None
}
fn locate(writable: bool) -> Result<Directory> {
    mounted(writable).or_else(unmounted).ok_or_else(|| {
        "The boot partition could not be found, or it is mounted read-only".to_string()
    })
}

/// What each kernel command line of one partition asks for. A command line
/// without the token runs with the kernel's own choice, which is a third state
/// next to the two the setting offers, so it is kept rather than guessed.
fn observed(root: &Path) -> Result<Vec<Option<Mitigations>>> {
    let mut values = Vec::new();
    for path in entries(root) {
        let text = fs::read_to_string(&path).map_err(|e| e.to_string())?;
        let kind = kind(&path);
        values.extend(
            text.split('\n')
                .filter(|line| command_line(line, kind))
                .map(arguments_value),
        );
    }
    Ok(values)
}
fn view(root: Option<&Path>, configured: Mitigations, failure: Option<String>) -> Boot {
    let (values, reason) = match root.filter(|_| failure.is_none()) {
        Some(root) => match observed(root) {
            Err(error) => (Vec::new(), error),
            Ok(values) => (values, String::new()),
        },
        None => (Vec::new(), failure.unwrap_or_default()),
    };
    let applied = values
        .first()
        .filter(|first| values.iter().all(|value| value == *first))
        .copied()
        .flatten();
    let reason = if applied.is_some() {
        String::new()
    } else if values.iter().any(|value| value.is_some()) {
        "The boot entries disagree about the mitigation setting".to_string()
    } else if !values.is_empty() {
        "The boot entries do not mention CPU mitigations".to_string()
    } else if !reason.is_empty() {
        reason
    } else {
        "No boot entries were found on the boot partition".to_string()
    };
    Boot {
        available: applied.is_some(),
        configured,
        applied,
        reboot_required: applied.is_some_and(|applied| applied != configured),
        reason,
    }
}

pub fn status() -> Boot {
    let configured = read_policy(Path::new(POLICY)).mitigations;
    match locate(false) {
        Ok(directory) => view(Some(&directory.root), configured, None),
        Err(error) => view(None, configured, Some(error)),
    }
}

/// Rewrites one boot-entry file, returning its previous contents when changed.
fn rewrite(path: &Path, wanted: Mitigations) -> Result<Option<Vec<u8>>> {
    let original = fs::read(path).map_err(|e| e.to_string())?;
    let text = String::from_utf8(original.clone())
        .map_err(|_| format!("{} is not text", path.to_string_lossy()))?;
    let updated = rewrite_text(&text, kind(path), wanted);
    if updated == text {
        return Ok(None);
    }
    files::atomic(path, updated.as_bytes(), 0o644)
        .map_err(|e| format!("The boot partition rejected the change: {e}"))?;
    Ok(Some(original))
}
fn revert(changed: &[(PathBuf, Vec<u8>)], previous: BootPolicy) {
    for (path, original) in changed {
        let _ = files::atomic(path, original, 0o644);
    }
    let _ = files::atomic(Path::new(POLICY), render_policy(previous).as_bytes(), 0o644);
}

pub fn set(enabled: bool) -> Result<Boot> {
    let _lock = LOCK.lock().map_err(|_| "Boot configuration is busy")?;
    let wanted = if enabled {
        Mitigations::Auto
    } else {
        Mitigations::Off
    };
    let previous = read_policy(Path::new(POLICY));
    let directory = locate(true)?;
    if entries(&directory.root).is_empty() {
        return Err("No boot entries were found on the boot partition".into());
    }
    files::atomic(
        Path::new(POLICY),
        render_policy(BootPolicy {
            mitigations: wanted,
        })
        .as_bytes(),
        0o644,
    )?;
    let mut changed: Vec<(PathBuf, Vec<u8>)> = Vec::new();
    let mut failure = None;
    for path in entries(&directory.root) {
        match rewrite(&path, wanted) {
            Ok(Some(original)) => changed.push((path, original)),
            Ok(None) => {}
            Err(error) => {
                failure = Some(error);
                break;
            }
        }
    }
    let values = observed(&directory.root).unwrap_or_default();
    if failure.is_none()
        && (values.is_empty() || !values.iter().all(|value| *value == Some(wanted)))
    {
        failure = Some("The boot loader did not accept the mitigation setting".into());
    }
    if let Some(error) = failure {
        revert(&changed, previous);
        return Err(error);
    }
    Ok(view(Some(&directory.root), wanted, None))
}

#[cfg(test)]
mod tests {
    use super::*;

    const GRUB: &str = "\
menuentry 'UFLinux' {
\tlinux /boot/vmlinuz root=UUID=1 rw quiet mitigations=off
\tinitrd /boot/initramfs
}";

    #[test]
    fn only_grub_kernel_lines_are_rewritten() {
        let updated = rewrite_text(GRUB, Kind::Grub, Mitigations::Auto);
        assert_eq!(
            updated.match_indices("mitigations=").count(),
            1,
            "the initrd line is left alone"
        );
        assert!(updated.contains("linux /boot/vmlinuz root=UUID=1 rw quiet mitigations=auto"));
        assert!(updated.contains("initrd /boot/initramfs"));
        assert_eq!(
            rewrite_text(&updated, Kind::Grub, Mitigations::Auto),
            updated
        );
    }

    #[test]
    fn an_extlinux_append_line_gains_the_token() {
        let source = "LABEL Linux\n\tLINUX /vmlinuz\n\tAPPEND root=/dev/mmcblk0p2 rw";
        assert_eq!(
            rewrite_text(source, Kind::Extlinux, Mitigations::Off),
            "LABEL Linux\n\tLINUX /vmlinuz\n\tAPPEND root=/dev/mmcblk0p2 rw mitigations=off"
        );
    }

    #[test]
    fn a_raspberry_pi_command_line_is_rewritten_whole() {
        let source = "dwc_otg.lpm_enable=0 console=serial0,115200 root=/dev/mmcblk0p7 rootwait";
        let updated = rewrite_text(source, Kind::Bare, Mitigations::Auto);
        assert_eq!(arguments_value(&updated), Some(Mitigations::Auto));
        assert_eq!(
            rewrite_text(&updated, Kind::Bare, Mitigations::Auto),
            updated
        );
    }

    #[test]
    fn each_layout_is_recognised_by_its_own_name() {
        assert_eq!(kind(Path::new("/boot/cmdline.txt")), Kind::Bare);
        assert_eq!(kind(Path::new("/boot/cmdline-debug.txt")), Kind::Bare);
        assert_eq!(
            kind(Path::new("/boot/extlinux/extlinux.conf")),
            Kind::Extlinux
        );
        assert_eq!(kind(Path::new("/boot/grub/grub.cfg")), Kind::Grub);
    }

    #[test]
    fn a_partition_is_only_useful_once_an_entry_is_found() {
        let root = std::env::temp_dir().join(format!("uflinux-boot-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("lost+found")).unwrap();
        assert!(entries(&root).is_empty());
        fs::create_dir_all(root.join("boot/grub")).unwrap();
        fs::write(root.join("boot/grub/grub.cfg"), GRUB).unwrap();
        fs::write(root.join("cmdline.txt"), "root=/dev/mmcblk0p7\n").unwrap();
        assert_eq!(
            entries(&root),
            vec![root.join("boot/grub/grub.cfg"), root.join("cmdline.txt")]
        );
        // The Raspberry Pi command line carries no token, so the entries of
        // this partition disagree and the setting cannot be verified.
        let boot = view(Some(&root), Mitigations::Off, None);
        assert!(!boot.available);
        assert!(boot.reason.contains("disagree"));
        fs::write(
            root.join("cmdline.txt"),
            "root=/dev/mmcblk0p7 mitigations=off\n",
        )
        .unwrap();
        let boot = view(Some(&root), Mitigations::Off, None);
        assert!(boot.available);
        assert_eq!(boot.applied, Some(Mitigations::Off));
        assert!(!boot.reboot_required);
        assert_eq!(
            view(Some(&root), Mitigations::Auto, None).reboot_required,
            true
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_missing_partition_is_reported_without_an_applied_value() {
        let boot = view(
            None,
            Mitigations::Off,
            Some("The boot partition is missing".into()),
        );
        assert!(!boot.available);
        assert_eq!(boot.applied, None);
        assert!(!boot.reboot_required);
        assert_eq!(boot.reason, "The boot partition is missing");
    }

    #[test]
    fn a_policy_record_still_parses_after_a_partial_write() {
        assert_eq!(parse_policy("mitigations=au"), Mitigations::Off);
        assert_eq!(parse_policy("mitigations=auto"), Mitigations::Auto);
    }
}
