"""Check the release package conversion without fetching external assets."""
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest
import zlib

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
from kernel_packages import deflate_package


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
