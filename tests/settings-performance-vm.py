#!/usr/bin/env python3
"""Verify the boot-time CPU mitigation toggle against the real boot entries."""
import json
import os
from pathlib import Path
import pwd
import re
import subprocess
import time

assert os.geteuid() == 0 and "QEMU" in Path("/sys/class/dmi/id/sys_vendor").read_text()
user = pwd.getpwnam("alice")
FAT = ("vfat", "fat32", "msdos")
MOUNT_POINT = "/mnt"
POLICY = Path("/etc/uflinux/boot.conf")


def operation(value):
    reply = subprocess.run(
        ["uflinux-settingsctl", "system-request"],
        input=json.dumps(value),
        text=True,
        capture_output=True,
        timeout=60,
    )
    assert reply.returncode == 0, reply.stderr
    return json.loads(reply.stdout)


def environment():
    for path in Path("/proc").glob("[0-9]*/exe"):
        try:
            if (
                path.resolve().name == "uflinux-settingsd"
                and path.parent.stat().st_uid == user.pw_uid
            ):
                return dict(
                    item.split("=", 1)
                    for item in (path.parent / "environ").read_bytes().decode().split("\0")
                    if "=" in item
                )
        except OSError:
            pass
    raise AssertionError("Graphical Settings service is missing")


def demote():
    os.initgroups(user.pw_name, user.pw_gid)
    os.setgid(user.pw_gid)
    os.setuid(user.pw_uid)


def shared_boot():
    """The page's value, as the signed-in user's service sees it."""
    return json.loads(
        subprocess.check_output(
            ["uflinux-settingsctl", "status"],
            env=environment(),
            preexec_fn=demote,
            text=True,
            timeout=10,
        )
    )["boot"]


def grub_entry(root):
    for relative in ("boot/grub/grub.cfg", "grub/grub.cfg", "EFI/uflinux/grub.cfg"):
        path = Path(root, relative)
        if path.is_file():
            return path
    return None


def boot_partition():
    """The boot partition holding the GRUB entries, mounted here if needed."""
    for line in Path("/proc/mounts").read_text().splitlines():
        fields = line.split()
        if len(fields) > 3 and fields[2] in FAT and grub_entry(fields[1]):
            return fields[1], False
    listing = json.loads(
        subprocess.run(
            ["lsblk", "--json", "--nodeps", "--paths", "--output", "PATH,FSTYPE"],
            capture_output=True,
            text=True,
            timeout=30,
        ).stdout
    )
    for device in listing.get("blockdevices", []):
        if device.get("fstype") not in FAT:
            continue
        subprocess.run(["mount", device["path"], MOUNT_POINT], check=True, timeout=30)
        if grub_entry(MOUNT_POINT):
            return MOUNT_POINT, True
        subprocess.run(["umount", MOUNT_POINT], timeout=30)
    return None, False


def tokens(path):
    found = set()
    for line in path.read_text().splitlines():
        found.update(re.findall(r"(?:^|\s)mitigations=(\S+)", line))
    return found


def wait_for_configured(wanted):
    for _ in range(100):
        if shared_boot().get("configured") == wanted:
            return
        time.sleep(0.1)
    raise AssertionError("The running service still reports the old mitigation policy")


root, temporary = boot_partition()
assert root, "No boot partition with GRUB entries was found"
entry = grub_entry(root)
original = operation({"operation": "boot-status"})
try:
    assert original["available"], original
    assert original["configured"] == "off", original
    assert original["applied"] == "off", original
    assert original["reboot_required"] is False, original
    assert tokens(entry) == {"off"}, entry
    assert POLICY.read_text().count("mitigations=off") == 1, POLICY.read_text()

    enabled = operation({"operation": "set-mitigations", "enabled": True})
    assert enabled["configured"] == "auto", enabled
    assert enabled["applied"] == "off", enabled
    assert enabled["reboot_required"] is True, enabled
    assert "mitigations=auto" in POLICY.read_text(), POLICY.read_text()
    assert tokens(entry) == {"auto"}, entry
    wait_for_configured("auto")

    operation({"operation": "set-mitigations", "enabled": False})
    assert tokens(entry) == {"off"}, entry
    assert operation({"operation": "boot-status"}) == original
    wait_for_configured("off")
    print(
        "UFLINUX_PERFORMANCE_VM:"
        + json.dumps(
            {
                "default_policy": "off",
                "boot_entries_rewritten": str(entry),
                "reboot_required_reported": True,
                "settings_service_synchronized": True,
            }
        ),
        flush=True,
    )
finally:
    if temporary:
        subprocess.run(["umount", MOUNT_POINT], timeout=30)
