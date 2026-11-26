"""The same GRUB menu drives both PC firmware modes."""
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
MENU = ROOT / "aports/uflinux/uflinux-boot-menu"
INITRDS = "/boot/amd-ucode.img /boot/intel-ucode.img /boot/initramfs"
# The generator reads the policy from this path only, so the tests point it at
# a temporary file by way of the copy made below.
POLICY = MENU.read_text().replace("/etc/uflinux/boot.conf", "$TEST_BOOT_CONF")


class DevicePcProfileTests(unittest.TestCase):
    def menu(self, options="", policy=None, environment=None):
        """Render a menu with the given inherited options and boot.conf policy."""
        with tempfile.TemporaryDirectory() as temporary:
            conf = Path(temporary) / "boot.conf"
            if policy is not None:
                conf.write_text(policy)
            # The generator only runs when invoked under its own name, so the
            # copy has to keep it.
            script = Path(temporary) / "uflinux-boot-menu"
            script.write_text(POLICY)
            env = dict(os.environ, TEST_BOOT_CONF=str(conf))
            env.pop("UFLINUX_MITIGATIONS", None)
            env.update(environment or {})
            return subprocess.check_output(
                ["sh", str(script), "/boot/vmlinuz", INITRDS, "", options, "/boot/grub"],
                text=True, env=env)

    def command_lines(self, result):
        return [line.strip() for line in result.splitlines() if line.strip().startswith("linux ")]

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

    def test_every_entry_boots_with_mitigations_off(self):
        # The default has to reach all three entries, or the entry the user
        # picks silently keeps the slower kernel default.
        result = self.menu("root=UUID=abc rootfstype=f2fs console=ttyS0,115200")
        lines = self.command_lines(result)
        self.assertEqual(len(lines), 3)
        for line in lines:
            self.assertEqual(line.count("mitigations="), 1, line)
            self.assertIn("mitigations=off", line)

    def test_a_missing_policy_file_still_means_off(self):
        self.assertIn("mitigations=off", self.menu("root=UUID=abc", policy=""))

    def test_the_installed_policy_enables_the_workarounds(self):
        result = self.menu("root=UUID=abc", policy="# comment\nmitigations=auto\n")
        for line in self.command_lines(result):
            self.assertEqual(line.count("mitigations=auto"), 1, line)

    def test_an_unreadable_policy_value_falls_back_to_off(self):
        # Only the two documented states exist; a typo must not boot with the
        # kernel default, which is the expensive choice.
        for value in ("nosync", "", "OFF"):
            with self.subTest(value=value):
                result = self.menu("root=UUID=abc", policy="mitigations=%s\n" % value)
                self.assertIn("mitigations=off", " ".join(self.command_lines(result)))

    def test_an_inherited_token_is_replaced_not_duplicated(self):
        # A vendor command line, or a boot.conf that changed after the entry was
        # first written, must not leave two mitigations words behind.
        result = self.menu("mitigations=auto mitigations=none root=UUID=abc",
                           policy="mitigations=auto\n")
        for line in self.command_lines(result):
            self.assertEqual(line.count("mitigations="), 1, line)
            self.assertIn("mitigations=auto", line)
            self.assertIn("root=UUID=abc", line)

    def test_the_caller_override_wins_over_the_policy_file(self):
        # The installer and the image build pass the target root's policy here,
        # because they run before it is the running system's.
        result = self.menu("root=UUID=abc", policy="mitigations=auto\n",
                           environment={"UFLINUX_MITIGATIONS": "off"})
        for line in self.command_lines(result):
            self.assertEqual(line.count("mitigations="), 1, line)
            self.assertIn("mitigations=off", line)
