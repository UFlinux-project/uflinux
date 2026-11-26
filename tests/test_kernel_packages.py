"""Check the release package conversion without fetching external assets."""
from pathlib import Path
import shutil
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
    def test_the_signed_adb_stream_is_preserved(self):
        contents = b"ADB.pckg" + bytes(range(256)) * 1024
        compressed = subprocess.run(["zstd", "-q", "-c"], input=contents,
                                    capture_output=True, check=True).stdout
        with tempfile.TemporaryDirectory() as directory:
            original = Path(directory) / "original.apk"
            converted = Path(directory) / "converted.apk"
            original.write_bytes(b"ADBc\x02\x09" + compressed)
            deflate_package(original, converted)
            result = converted.read_bytes()
        self.assertEqual(result[:4], b"ADBd")
        self.assertEqual(zlib.decompress(result[4:], wbits=-15), contents)
