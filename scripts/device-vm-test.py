#!/usr/bin/env python3
"""Boot a built device disk through its packaged GRUB loader."""
import argparse
from pathlib import Path
import shutil
import subprocess
import tempfile

import pexpect

from kernel_packages import kernels
from vm_control import SHELL_PROMPT

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--arch", choices=("x86_64", "aarch64"), required=True)
parser.add_argument("--mode", choices=("bios", "uefi"), default="uefi")
parser.add_argument("--device", choices=("vm", "pc-xfs", "pc-f2fs"), default="vm")
parser.add_argument("--image", type=Path, required=True)
parser.add_argument("--log", type=Path, required=True)
args = parser.parse_args()

target = f"virt-{args.arch}" if args.device == "vm" else args.arch
filesystem = "f2fs" if args.device == "pc-f2fs" else "xfs"
if args.mode == "bios" and args.arch != "x86_64":
    parser.error("BIOS testing requires x86_64")
version = kernels()["packages"][args.arch][target]["name"].removeprefix(
    f"linux-uflinux-{target}-").rsplit("-r", 1)[0]
abi = f"{version}-uflinux-{target}"
args.log.parent.mkdir(parents=True, exist_ok=True)
with tempfile.TemporaryDirectory(prefix="uflinux-device-vm-") as temporary:
    disk = Path(temporary) / "disk.raw"
    if args.arch == "aarch64":
        platform = ["-machine", "virt", "-cpu", "cortex-a57",
                    "-bios", "/usr/share/qemu-efi-aarch64/QEMU_EFI.fd"]
    elif args.mode == "bios":
        platform = ["-machine", "q35"]
    else:
        variables = disk.with_name("vars.fd")
        shutil.copyfile("/usr/share/OVMF/OVMF_VARS_4M.fd", variables)
        platform = ["-machine", "q35", "-drive",
                    "if=pflash,format=raw,unit=0,readonly=on,file=/usr/share/OVMF/OVMF_CODE_4M.fd",
                    "-drive", f"if=pflash,format=raw,unit=1,file={variables}"]
    with disk.open("wb") as output:
        subprocess.run(["zstd", "-dc", args.image], stdout=output, check=True)
    command = ["qemu-system-" + args.arch, *platform, "-m", "2048", "-smp", "2",
               "-nographic", "-no-reboot", "-snapshot",
               "-drive", f"if=virtio,format=raw,file={disk}"]
    with args.log.open("w") as log, pexpect.spawn(
        command[0], command[1:], encoding="utf-8", codec_errors="replace", timeout=300
    ) as guest:
        guest.logfile_read = log
        guest.expect("login:")
        guest.sendline("user")
        guest.expect("Password:")
        guest.sendline("user")
        guest.expect(SHELL_PROMPT)
        guest.sendline(f'test "$(uname -r)" = "{abi}" && test -b /dev/vda && '
                       f'test "$(findmnt -no FSTYPE /)" = {filesystem} && printf "UFLINUX_%s\\n" BOOT_OK')
        guest.expect("UFLINUX_BOOT_OK")
