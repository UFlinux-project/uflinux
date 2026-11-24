"""The same GRUB menu drives both PC firmware modes."""
from pathlib import Path
import subprocess
import unittest

ROOT = Path(__file__).resolve().parents[1]
MENU = ROOT / "aports/uflinux/uflinux-boot-menu"


class DevicePcProfileTests(unittest.TestCase):
    def test_all_modes_keep_root_and_microcode(self):
        initrds = "/boot/amd-ucode.img /boot/intel-ucode.img /boot/initramfs"
        command = ["sh", str(MENU), "/boot/vmlinuz", initrds, "",
                   "root=UUID=abc rootfstype=f2fs quiet console=ttyS0,115200", "/boot/grub"]
        result = subprocess.check_output(command, text=True)
        self.assertEqual(result.count(f"initrd {initrds}"), 3)
        self.assertEqual(result.count("root=UUID=abc"), 3)
        self.assertEqual(result.count("quiet splash"), 1)
        self.assertIn("loglevel=7 ignore_loglevel debug_init plymouth.enable=0", result)
        self.assertIn("plymouth.enable=0 rc_runlevel=tty", result)
        self.assertIn("fwsetup --is-supported", result)
