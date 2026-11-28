#!/usr/bin/env python3
"""Validate non-x86 release ISO boot paths on their matching firmware."""
import argparse
import importlib.util
import json
from pathlib import Path
import shlex
import shutil
import tempfile

import pexpect

from builder import LOCK, sha
from vm_control import SHELL_PROMPT, root_filesystem

spec = importlib.util.spec_from_file_location("iso_vm", Path(__file__).with_name("vm-test.py"))
vm = importlib.util.module_from_spec(spec)
spec.loader.exec_module(vm)


def boot(command, disk, iso, log):
    args = command + ["-m", "4096", "-smp", "2", "-nographic", "-no-reboot"]
    if disk is not None:
        args += ["-drive", f"if=none,id=target,format=raw,file={disk}",
                 "-device", "virtio-blk-pci,drive=target,bootindex=2"]
    if iso is not None:
        args += ["-device", "virtio-scsi-pci,id=scsi",
                 "-drive", f"if=none,id=cd,format=raw,readonly=on,file={iso}",
                 "-device", "scsi-cd,drive=cd,bus=scsi.0,bootindex=1"]
    child = pexpect.spawn(args[0], args[1:], encoding="utf-8",
                          codec_errors="replace", timeout=900)
    child.logfile_read = log
    return child


def run_install(child, mode):
    vm.login(child, "live", "live")
    if mode == "uefi":
        layout = ["size=1G,type=U", "type=L"]
        boot_part, root_part = 1, 2
        mkboot = "mkfs.vfat -F32 /dev/vda1"
    else:
        layout = ["size=8M,type=9e1a2d38-c612-4316-aa26-8b49521e5a8b", "size=1G,type=L", "type=L"]
        boot_part, root_part = 2, 3
        mkboot = "mkfs.ext4 -F /dev/vda2"
    table = " ".join(map(shlex.quote, ["label: gpt", *layout]))
    vm.guest(child, f"printf '%s\\n' {table} | sfdisk /dev/vda; "
             f"{mkboot}; mkfs.xfs -f /dev/vda{root_part}; "
             f"mount /dev/vda{root_part} /mnt; mkdir /mnt/boot; mount /dev/vda{boot_part} /mnt/boot")
    child.sendline(f"doas uflinux-install --boot-mode {mode} --boot-disk /dev/vda --target /mnt --user alice")
    child.expect(r"Type INSTALL ([^ ]+) to install:")
    child.sendline("INSTALL " + child.match.group(1))
    child.expect("Password for alice:")
    child.sendline("uflinux-test-password")
    child.expect("Repeat password:")
    child.sendline("uflinux-test-password")
    child.expect(SHELL_PROMPT)
    vm.guest(child, "test -s /mnt/boot/grub/grub.cfg; sync; umount -R /mnt")
    child.sendline("doas poweroff")
    child.expect(pexpect.EOF)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--arch", choices=("aarch64", "ppc64le", "riscv64"), required=True)
    parser.add_argument("--iso", type=Path, required=True)
    args = parser.parse_args()
    mode = "native" if args.arch == "ppc64le" else "uefi"
    out = Path("out/vm") / args.arch / mode
    out.mkdir(parents=True, exist_ok=True)
    record = out / "verified.json"
    record.unlink(missing_ok=True)
    digest = sha(args.iso)
    result = {"arch": args.arch, "mode": mode, "iso_sha256": digest}
    with tempfile.TemporaryDirectory(prefix="uflinux-arch-vm-") as temporary, (out / "console.log").open("w") as log:
        disk = Path(temporary) / "target.img"
        with disk.open("wb") as stream:
            stream.truncate(10 * 1024**3)
        if args.arch == "riscv64":
            firmware = vm.locate(["/usr/share/qemu-efi-riscv64/RISCV_VIRT_CODE.fd",
                                  "/usr/share/qemu-efi-riscv64/*.fd"], "RISC-V UEFI firmware")
            command = ["qemu-system-riscv64", "-machine", "virt", "-bios", "default",
                       "-drive", f"if=pflash,unit=0,format=raw,readonly=on,file={firmware}"]
            with boot(command, None, args.iso, log) as child:
                child.expect_exact("UFLinux (Standard)")
            result["grub_menu"] = True
        else:
            if args.arch == "aarch64":
                firmware = vm.locate(["/usr/share/AAVMF/AAVMF_CODE.fd"], "Arm64 UEFI firmware")
                variables = disk.with_name("vars.fd")
                shutil.copyfile(vm.locate(["/usr/share/AAVMF/AAVMF_VARS.fd"], "Arm64 UEFI variables"),
                                variables)
                command = ["qemu-system-aarch64", "-machine", "virt", "-cpu", "cortex-a57",
                           "-drive", f"if=pflash,unit=0,format=raw,readonly=on,file={firmware}",
                           "-drive", f"if=pflash,unit=1,format=raw,file={variables}"]
            else:
                command = ["qemu-system-ppc64", "-machine", "pseries"]
            with boot(command, disk, args.iso, log) as child:
                run_install(child, mode)
            with boot(command, disk, None, log) as child:
                vm.login(child, "alice", "uflinux-test-password")
                musl = LOCK["musl"]["version"] + "-r" + LOCK["musl"]["revision"]
                vm.guest(child, f"apk info -e musl={musl} musl-utils={musl}; "
                         + root_filesystem("xfs"), admin=False)
            result.update(xfs_install_reboot=True, musl=musl)
    if sha(args.iso) != digest:
        raise RuntimeError("ISO changed during boot verification")
    record.write_text(json.dumps(result, indent=2) + "\n")


if __name__ == "__main__":
    main()
