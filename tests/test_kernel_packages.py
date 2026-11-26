"""Check the release package conversion without fetching external assets."""
from pathlib import Path
import shutil
import struct
import subprocess
import sys
import tempfile
import unittest
import zlib

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
from kernel_packages import CONFIG, deflate_package, describe


def release_document(tag="kernels-fixture", digest="sha256:" + "0" * 64, skip=(), blind=()):
    """A release document shaped like the GitHub API answer for our targets."""
    assets = [{"name": f"linux-uflinux-{target}-1.0-r1.apk",
               "digest": "" if target in blind else digest}
              for targets in CONFIG["targets"].values() for target in targets
              if target not in skip]
    return {"tag_name": tag, "assets": assets}


class KernelResolutionTests(unittest.TestCase):
    def test_every_required_target_comes_from_the_release(self):
        index = describe(release_document())
        self.assertEqual(index["tag"], "kernels-fixture")
        self.assertEqual(index["base"],
                         "https://github.com/UFlinux-project/kernel/releases/download/kernels-fixture")
        self.assertEqual(index["key"], CONFIG["key"])
        self.assertEqual({arch: sorted(targets) for arch, targets in index["packages"].items()},
                         {arch: sorted(targets) for arch, targets in CONFIG["targets"].items()})
        for targets in index["packages"].values():
            for target, item in targets.items():
                self.assertEqual(item["name"], f"linux-uflinux-{target}-1.0-r1.apk")
                self.assertEqual(item["sha256"], "0" * 64)
                self.assertEqual(sorted(item), ["name", "sha256"])

    def test_a_release_without_a_required_target_is_refused(self):
        with self.assertRaisesRegex(RuntimeError, "no aarch64/rpi package"):
            describe(release_document(skip=("rpi",)))

    def test_a_release_without_a_digest_is_refused(self):
        with self.assertRaisesRegex(RuntimeError, "no checksum for linux-uflinux-rpi"):
            describe(release_document(blind=("rpi",)))


@unittest.skipUnless(shutil.which("zstd"), "zstd is required")
class KernelCompressionTests(unittest.TestCase):
    """The conversion keeps the signed stream and repairs the package owner."""

    @staticmethod
    def package(payload):
        """A minimal zstd wrapped APK whose descriptor block holds *payload*."""
        descriptor = struct.pack("<II", len(payload) + 8, 0) + payload
        contents = b"ADB.pckg" + descriptor + bytes(range(256)) * 1024
        compressed = subprocess.run(["zstd", "-q", "-c"], input=contents,
                                    capture_output=True, check=True).stdout
        return b"ADBc\x02\x09" + compressed, contents

    def convert(self, apk):
        with tempfile.TemporaryDirectory() as directory:
            original = Path(directory) / "original.apk"
            converted = Path(directory) / "converted.apk"
            original.write_bytes(apk)
            deflate_package(original, converted)
            return converted.read_bytes()

    def test_the_signed_adb_stream_is_rewrapped_as_deflate(self):
        apk, contents = self.package(b"\x00" * 32 + b"\x06nobody\x00" + b"\x00" * 32)
        result = self.convert(apk)
        self.assertEqual(result[:4], b"ADBd")
        self.assertEqual(zlib.decompress(result[4:], wbits=-15),
                         contents.replace(b"\x06nobody\x00", b"\x04root\x00\x00\x00"))

    def test_the_package_owner_becomes_root_without_changing_the_length(self):
        apk, contents = self.package(b"\x00" * 32 + b"\x06nobody\x00" + b"\x00" * 32)
        result = self.convert(apk)
        payload = zlib.decompress(result[4:], wbits=-15)
        self.assertEqual(len(payload), len(contents))
        self.assertNotIn(b"\x06nobody\x00", payload)
        self.assertIn(b"\x04root\x00\x00\x00", payload)
        # Only the owner name differs, so the rest of the signed stream stands.
        offset = contents.index(b"\x06nobody\x00")
        self.assertEqual(payload[:offset], contents[:offset])
        self.assertEqual(payload[offset + 8:], contents[offset + 8:])

    def test_a_descriptor_without_the_release_owner_is_refused(self):
        apk, _ = self.package(b"\x00" * 64)
        with self.assertRaisesRegex(ValueError, "Unexpected kernel APK ownership"):
            self.convert(apk)

    def test_a_package_without_an_adb_descriptor_is_refused(self):
        apk, _ = self.package(b"\x00" * 64)
        broken = b"ADBc\x02\x09" + subprocess.run(
            ["zstd", "-q", "-c"], input=b"not-an-apk" + b"\x00" * 64,
            capture_output=True, check=True).stdout
        with self.assertRaisesRegex(ValueError, "Unexpected kernel APK descriptor"):
            self.convert(broken)
        self.assertEqual(apk[:6], b"ADBc\x02\x09")

