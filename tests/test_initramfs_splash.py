"""Exercise the early-init wrapper without mounting or starting a daemon."""
import re
import subprocess
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
WRAPPER = ROOT / "aports/uflinux/uflinux-initramfs-init"
DEVICE_PROFILE = ROOT / "profiles/device-rootfs.sh"


class InitramfsWrapperTest(unittest.TestCase):
    def run_wrapper(self, source):
        with tempfile.TemporaryDirectory() as tmp:
            directory = Path(tmp)
            original = directory / "alpine-init"
            patched = directory / "patched-init"
            busybox = directory / "busybox"
            original.write_text(source)
            busybox.write_text('#!/bin/sh\nexec "$@"\n')
            busybox.chmod(0o755)
            wrapper = WRAPPER.read_text().replace('/bin/busybox', str(busybox))
            wrapper = wrapper.replace('/usr/share/mkinitfs/initramfs-init', str(original))
            wrapper = wrapper.replace('/tmp/uflinux-init', str(patched))
            subprocess.run(['sh', '-c', wrapper], check=True)
            return patched.read_text() if patched.exists() else None

    def test_injects_both_boot_paths_and_handoffs(self):
        script = '''exit 0
# shared memory area (later system will need it)
# read the kernel options.
# workaround for vmware
if [ "$SINGLEMODE" = "yes" ]; then
 :
fi
cat "$ROOT"/proc/mounts 2>/dev/null | while read -r _dev DIR _type _opts ; do :; done
if [ "$SINGLEMODE" = "yes" ]; then
 :
fi
\tif echo "$KOPT_modules $rootfstype" | grep -qw btrfs; then
\t\t/sbin/btrfs device scan >/dev/null || \\
\t\t\techo "Failed to scan devices for btrfs filesystem."
\tfi
cat "$ROOT"/proc/mounts 2>/dev/null | while read -r _dev DIR _type _opts ; do :; done
'''
        patched = self.run_wrapper(script)
        self.assertEqual(patched.count('uflinux-initramfs-splash prepare'), 1)
        self.assertEqual(patched.count('uflinux-initramfs-splash start'), 3)
        self.assertEqual(patched.count('uflinux-initramfs-splash handoff'), 2)
        self.assertLess(patched.index('uflinux-initramfs-splash prepare'),
                        patched.index('uflinux-initramfs-splash start'))
        # The F2FS resize has to run before the root is mounted, which is what
        # the btrfs scan right above the mount marks: injecting anywhere else
        # would resize a mounted filesystem.
        self.assertEqual(patched.count('uflinux-initramfs-resize'), 1)
        self.assertLess(patched.index('uflinux-initramfs-resize'),
                        patched.index('/sbin/btrfs device scan'))

    def test_unknown_alpine_init_boots_without_injection(self):
        self.assertIsNone(self.run_wrapper('exit 0\n# upstream structure changed\n'))

    def test_device_profile_ships_what_the_wrapper_execs(self):
        # mkinitfs -i replaces /init but never copies Alpine's initramfs-init,
        # and device images do not use the feature that normally provides it,
        # so the profile has to list the wrapper's own dependencies.
        profile = DEVICE_PROFILE.read_text()
        listed = re.search(r'^initramfs_boot_files="([^"]*)"', profile, re.M)
        self.assertIsNotNone(listed, "profile must name the wrapper's files")
        files = listed.group(1).split()
        self.assertIn("usr/share/mkinitfs/initramfs-init", files)
        for hook in re.findall(r"/usr/libexec/[a-z-]+", WRAPPER.read_text()):
            self.assertIn(hook.lstrip("/"), files, hook)
        # Listing them is only half of it: mkinitfs drops missing entries
        # silently, so the profile has to fail the build instead.
        self.assertIn("$initramfs_boot_files", profile.split("for file in")[1])

    def test_f2fs_resize_feature_is_wired_for_both_build_paths(self):
        # The hook only works if mkinitfs can find the resize binaries. The
        # device profile enables the feature for F2FS roots only, and the
        # recipe installs the feature file so that an mkinitfs run inside the
        # image finds it too.
        profile = DEVICE_PROFILE.read_text()
        self.assertIn('if [ "$rootfs" = f2fs ]', profile)
        self.assertIn('features="$features uflinux-f2fs-resize"', profile)
        recipe = (ROOT / "aports/uflinux/APKBUILD").read_text()
        self.assertIn("uflinux-f2fs-resize.files", recipe)
        self.assertIn("/etc/mkinitfs/features.d/uflinux-f2fs-resize.files", recipe)
        feature = (ROOT / "aports/uflinux/uflinux-f2fs-resize.files").read_text()
        paths = [line for line in feature.splitlines() if not line.startswith("#")]
        self.assertEqual(sorted(paths), sorted([
            "/usr/sbin/resize.f2fs", "/usr/bin/growpart",
            "/sbin/sfdisk", "/usr/sbin/partx"]))

    def test_device_profile_ships_early_modules(self):
        # The initramfs only loads what /etc/modules names, and no package owns
        # that file, so a board whose display chain is made of loadable drivers
        # depends on the profile writing it and handing it to mkinitfs.
        profile = DEVICE_PROFILE.read_text()
        self.assertIn("UFLINUX_INITRAMFS_MODULES", profile)
        self.assertIn("initramfs_generated_files=\"etc/modules\"", profile)
        self.assertIn("$initramfs_generated_files", profile.split("custom_files=")[1])
        self.assertIn("$initramfs_generated_files", profile.split("for file in")[1])
        # The generated file has to exist before mkinitfs runs, or it is skipped.
        self.assertLess(profile.index("initramfs_generated_files=\"etc/modules\""),
                        profile.index("/sbin/mkinitfs"))


if __name__ == '__main__':
    unittest.main()
