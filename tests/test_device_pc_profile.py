"""The profile's pc boot branch has to produce configs both loaders accept."""
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
PROFILE = ROOT / "profiles/device-rootfs.sh"


class DevicePcProfileTests(unittest.TestCase):
    """Run the real boot branch against a fixture rootfs instead of a chroot."""

    def setUp(self):
        shutil.which("sh") or self.skipTest("requires a POSIX shell")

    def run_branch(self, stage, arch, rootargs, ucode=()):
        """Run the profile from systemd_boot_suffix() to its end and return the ESP."""
        profile = PROFILE.read_text()
        branch = profile[profile.index("systemd_boot_suffix() {"):]
        # Redirect the paths the branch reads from the chroot itself. Only the
        # quoted occurrences are rewritten, so the paths it writes into the
        # configs stay as they will appear on the disk.
        fake = str(stage / "fake") + os.sep
        # Anchored on the reads, so the paths the branch stores in its
        # variables and prints into the configs stay untouched.
        for old, new in [('[ -f "/boot/', '[ -f "' + fake + "boot/"),
                         ('cp -a "/boot/', 'cp -a "' + fake + "boot/"),
                         ('[ -f "/usr/share/syslinux/', '[ -f "' + fake + "usr/share/syslinux/"),
                         ('cp -a "/usr/share/syslinux/', 'cp -a "' + fake + "usr/share/syslinux/"),
                         ('installer_efi="/usr/lib/systemd/',
                          'installer_efi="' + fake + "usr/lib/systemd/"),
                         ("sed -i 's/^disable_trigger=.*/disable_trigger=1/' "
                          "/etc/update-extlinux.conf",
                          "sed -i 's/^disable_trigger=.*/disable_trigger=1/' "
                          + str(stage / "fake/etc/update-extlinux.conf"))]:
            self.assertIn(old, branch, old)
            branch = branch.replace(old, new)
        script = stage / "branch.sh"
        script.write_text(branch)

        fake = stage / "fake"
        (fake / "etc").mkdir(parents=True)
        (fake / "etc/update-extlinux.conf").write_text("disable_trigger=0\n")
        (fake / "usr/share/syslinux").mkdir(parents=True)
        for module in ("libcom32.c32", "libutil.c32"):
            (fake / "usr/share/syslinux" / module).write_bytes(b"com32")
        suffix = {"x86": "ia32", "x86_64": "x64"}[arch]
        efi = fake / "usr/lib/systemd/boot/efi"
        efi.mkdir(parents=True)
        (efi / ("systemd-boot%s.efi" % suffix)).write_bytes(b"MZ")
        (fake / "boot").mkdir(parents=True)
        for image in ucode:
            (fake / "boot" / (image + ".img")).write_bytes(image.encode())
        esp = stage / "esp"
        # The profile creates this before the boot branch; the branch itself
        # only ever copies into it.
        (esp / "boot").mkdir(parents=True)
        subprocess.run(
            ["sh", "-c", 'set -eu\n. "$1"', "sh", str(script)],
            check=True,
            env=dict(os.environ, arch=arch, boot="pc", esp=str(esp), version="0.1.0",
                     device="pc-test", serial="ttyS0", consoleargs="",
                     cmdline="quiet console=tty0 console=ttyS0,115200n8", rootargs=rootargs))
        return esp

    def test_pc_profile_writes_a_config_for_each_loader(self):
        with tempfile.TemporaryDirectory() as temporary:
            esp = self.run_branch(
                Path(temporary), "x86_64",
                "root=UUID=abc rootwait rootfstype=f2fs rootflags=ro,"
                "compress_algorithm=lz4,compress_mode=fs",
                ucode=("amd-ucode", "intel-ucode"))
            self.assertEqual(sorted(path.relative_to(esp).as_posix()
                                    for path in esp.rglob("*") if path.is_file()),
                             ["EFI/boot/bootx64.efi", "boot/amd-ucode.img", "boot/intel-ucode.img",
                              "libcom32.c32", "libutil.c32", "loader/entries/uflinux.conf",
                              "loader/loader.conf", "syslinux.cfg"])
            # syslinux wants one comma-separated INITRD with the microcode first.
            syslinux = (esp / "syslinux.cfg").read_text()
            self.assertIn("\tINITRD /boot/amd-ucode.img,/boot/intel-ucode.img,"
                          "/boot/initramfs\n", syslinux)
            self.assertIn("DEFAULT uflinux\n", syslinux)
            self.assertIn("APPEND quiet console=tty0 console=ttyS0,115200n8 root=UUID=abc "
                          "rootwait rootfstype=f2fs rootflags=ro,compress_algorithm=lz4,"
                          "compress_mode=fs\n", syslinux)
            # systemd-boot wants one initrd line per image and the same options.
            entry = (esp / "loader/entries/uflinux.conf").read_text()
            self.assertEqual(entry.splitlines()[1:5],
                             ["linux\t/boot/vmlinuz", "initrd\t/boot/amd-ucode.img",
                              "initrd\t/boot/intel-ucode.img", "initrd\t/boot/initramfs"])
            self.assertEqual(entry.splitlines()[-1],
                             "options\tquiet console=tty0 console=ttyS0,115200n8 root=UUID=abc "
                             "rootwait rootfstype=f2fs rootflags=ro,compress_algorithm=lz4,"
                             "compress_mode=fs")
            # The package trigger would rewrite the config on the next kernel
            # transaction, so the branch has to switch it off.
            self.assertEqual((Path(temporary) / "fake/etc/update-extlinux.conf").read_text(),
                             "disable_trigger=1\n")

    def test_pc_profile_without_microcode_names_only_the_initramfs(self):
        with tempfile.TemporaryDirectory() as temporary:
            esp = self.run_branch(Path(temporary), "x86_64",
                                  "root=UUID=abc rootwait rootfstype=xfs rootflags=ro")
            self.assertIn("\tINITRD /boot/initramfs\n", (esp / "syslinux.cfg").read_text())
            entry = (esp / "loader/entries/uflinux.conf").read_text()
            self.assertEqual(entry.count("initrd\t"), 1)
            self.assertNotIn("ucode", entry)

    def test_pc_profile_uses_the_ia32_loader_on_i386(self):
        with tempfile.TemporaryDirectory() as temporary:
            esp = self.run_branch(Path(temporary), "x86",
                                  "root=UUID=abc rootwait rootfstype=xfs rootflags=ro")
            self.assertTrue((esp / "EFI/boot/bootia32.efi").is_file())


if __name__ == "__main__":
    unittest.main()
