"""Validate QCS6490 inputs against the real downloaded kernel/firmware APKs.

Set UFLINUX_TEST_QCOM_KERNEL_APK and UFLINUX_TEST_QCOM_FIRMWARE_APK to run
these offline integration checks; no host kernel module is loaded.
"""
import gzip
import json
import os
from pathlib import Path
import shutil
import subprocess
import tarfile
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]


class QcomKernelInputTests(unittest.TestCase):
    @unittest.skipUnless(os.environ.get("UFLINUX_TEST_QCOM_INITRAMFS")
                         and os.environ.get("UFLINUX_TEST_QCOM_FIRMWARE_APK")
                         and shutil.which("cpio") and shutil.which("zstd"),
                         "requires built ARM64 initramfs, firmware APK, cpio and zstd")
    def test_built_arm64_initramfs_contains_the_actual_early_firmware(self):
        raw = gzip.decompress(Path(os.environ["UFLINUX_TEST_QCOM_INITRAMFS"]).read_bytes())
        listing = subprocess.check_output(["cpio", "-it", "--quiet"], input=raw).decode()
        names = set(listing.splitlines())

        def contents(name):
            return subprocess.check_output(["cpio", "-i", "--to-stdout", "--quiet", name],
                                           input=raw)

        # Inspect the generated artifact, rather than only config strings.
        busybox = contents("usr/bin/busybox")
        self.assertEqual(busybox[:6], b"\x7fELF\x02\x01")
        self.assertEqual(int.from_bytes(busybox[18:20], "little"), 183)  # EM_AARCH64
        verbose = subprocess.check_output(["cpio", "-itv", "--quiet"], input=raw).decode()
        self.assertTrue(any(line.endswith(" lib -> usr/lib") for line in verbose.splitlines()))
        device = json.loads((ROOT / "config/devices.json").read_text())["qcs6490"]
        with tarfile.open(os.environ["UFLINUX_TEST_QCOM_FIRMWARE_APK"],
                          mode="r:gz", ignore_zeros=True) as firmware:
            for name in device["initramfs_files"]:
                with self.subTest(path=name):
                    archived = "usr/" + name
                    self.assertIn(archived, names)
                    blob = contents(archived)
                    self.assertEqual(blob, firmware.extractfile(name).read())
                    subprocess.run(["zstd", "-tq"], input=blob, check=True)

    @unittest.skipUnless(os.environ.get("UFLINUX_TEST_QCOM_KERNEL_APK")
                         and shutil.which("depmod"), "requires kernel APK and depmod")
    def test_profile_regenerates_wireguard_dependencies_from_real_modules(self):
        with tempfile.TemporaryDirectory() as temporary:
            stage = Path(temporary)
            with tarfile.open(os.environ["UFLINUX_TEST_QCOM_KERNEL_APK"],
                              mode="r:gz", ignore_zeros=True) as archive:
                for member in archive:
                    path = Path(member.name)
                    if (not member.isfile() or ".." in path.parts
                            or not member.name.startswith("usr/lib/modules/")):
                        continue
                    target = stage / path
                    target.parent.mkdir(parents=True, exist_ok=True)
                    target.write_bytes(archive.extractfile(member).read())
            versions = list((stage / "usr/lib/modules").iterdir())
            self.assertEqual(len(versions), 1)
            version = versions[0].name
            (stage / "lib").mkdir()
            (stage / "lib/modules").symlink_to("../usr/lib/modules")
            # Run the profile's actual kernel-preparation section. Only ls
            # and the depmod base directory are redirected to our fixture.
            profile = (ROOT / "profiles/device-rootfs.sh").read_text()
            section = profile[profile.index("# One kernel per device image;"):]
            section = section.split("# The marker", 1)[0]
            script = '''
set -eu
device=qcs6490
ls() { command ls "$TEST_MODULE_ROOT/lib/modules"; }
depmod() { command depmod -b "$TEST_MODULE_ROOT" "$@"; }
''' + section
            subprocess.run(["sh", "-c", script], check=True,
                           env=dict(os.environ, TEST_MODULE_ROOT=str(stage)))
            depfile = stage / "lib/modules" / version / "modules.dep"
            wireguard = next(line for line in depfile.read_text().splitlines()
                             if "wireguard.ko" in line.split(":", 1)[0])
            for dependency in ("libcurve25519", "libchacha20poly1305",
                               "udp_tunnel", "ip6_udp_tunnel"):
                self.assertIn(dependency, wireguard.split(":", 1)[1])
            self.assertTrue((depfile.parent / "modules.dep.bin").is_file())
            ethernet = next(line for line in depfile.read_text().splitlines()
                            if line.split(":", 1)[0].endswith("/r8152.ko.zst"))
            self.assertIn("kernel/drivers/net/mii.ko.zst", ethernet.split(":", 1)[1])

    @unittest.skipUnless(os.environ.get("UFLINUX_TEST_QCOM_FIRMWARE_APK"),
                         "requires firmware APK")
    def test_early_firmware_paths_resolve_in_real_alpine_package(self):
        device = json.loads((ROOT / "config/devices.json").read_text())["qcs6490"]
        with tarfile.open(os.environ["UFLINUX_TEST_QCOM_FIRMWARE_APK"],
                          mode="r:gz", ignore_zeros=True) as archive:
            for name in device["initramfs_files"]:
                with self.subTest(path=name):
                    # extractfile follows the firmware package's relative
                    # symlink for qcs6490/a660_zap.mbn.zst too.
                    contents = archive.extractfile(name).read()
                    self.assertTrue(contents.startswith(b"\x28\xb5\x2f\xfd"))


if __name__ == "__main__":
    unittest.main()
