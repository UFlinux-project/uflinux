#!/usr/bin/env python3
"""Run the real graphical installer backend inside its disposable QEMU guest."""
import json
from pathlib import Path
import subprocess
import sys

BACKEND = "/usr/libexec/uflinux-install-gui-backend"
DISK = "/dev/vda"
FALLBACK = Path("/mnt/EFI/BOOT/BOOTX64.EFI")
MARKER = b"UFLinux shared ESP preservation test"


def run(*args, **kwargs):
    return subprocess.run(args, check=True, **kwargs)


def main(mode):
    prepared = mode == "uefi"
    if prepared:
        run("sfdisk", DISK, input="label: gpt\nsize=1G,type=U\ntype=L\n", text=True)
        run("udevadm", "settle")
        run("mkfs.vfat", "-F32", DISK + "1")
        run("mkfs.xfs", "-f", DISK + "2")
        run("mount", DISK + "1", "/mnt")
        try:
            FALLBACK.parent.mkdir(parents=True)
            FALLBACK.write_bytes(MARKER)
        finally:
            run("umount", "/mnt")
    discovery = json.loads(subprocess.check_output([BACKEND, "--discover"]))
    entry = next(item for item in discovery["disks"] if item["device"]["path"] == DISK)
    request = {
        "version": 1,
        "storage": {"mode": "prepared" if prepared else "erase", "disk": DISK,
                    "fingerprint": entry["fingerprint"], "boot": DISK + "1" if prepared else "",
                    "root": DISK + "2" if prepared else ""},
        "account": {"name": "Alice", "user": "alice", "hostname": "uflinux-gui",
                    "password": "uflinux-test-password"},
        "locale": "en_US.UTF-8", "keyboard": "us", "timezone": "Etc/UTC", "online": False,
    }
    run(BACKEND, "--install", input=json.dumps(request), text=True)
    run("mount", DISK + ("1" if prepared else "2"), "/mnt")
    try:
        assert (Path("/mnt/grub/grub.cfg")).is_file()
        if prepared and FALLBACK.read_bytes() != MARKER:
            raise RuntimeError("GRUB replaced the existing UEFI fallback file")
    finally:
        run("umount", "/mnt")


if __name__ == "__main__":
    main(sys.argv[1])
