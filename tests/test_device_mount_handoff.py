"""Exercise read-only root mount transfers with the real Linux mount syscalls."""
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
import builder


@unittest.skipUnless(os.environ.get("UFLINUX_TEST_MOUNT_HANDOFF")
                     and shutil.which("bwrap") and shutil.which("unshare"),
                     "requires user namespaces, bwrap and UFLINUX_TEST_MOUNT_HANDOFF=1")
class MountHandoffTests(unittest.TestCase):
    def test_runtime_mounts_move_into_a_read_only_root_snapshot(self):
        with tempfile.TemporaryDirectory() as temporary:
            source = Path(temporary) / "source"
            for name in ("dev", "proc", "sys", "run"):
                (source / name).mkdir(parents=True)
                (source / name / "build-only").touch()
            for restore_mountpoints in (False, True):
                with self.subTest(restore_mountpoints=restore_mountpoints):
                    destination = Path(temporary) / str(restore_mountpoints)
                    builder.device_snapshot(source, destination, rootfs=restore_mountpoints)
                    # As in Alpine's handoff: create destinations, then move
                    # mounted runtime filesystems. The target is a read-only
                    # bind of the real snapshot, not a mocked directory test.
                    script = '''
set -eu
mkdir -p /tmp/old
for name in dev proc sys run; do
    mkdir -p "/tmp/old/$name"
    mount -t tmpfs tmpfs "/tmp/old/$name"
    touch "/tmp/old/$name/runtime-marker"
    mkdir -p "/tmp/new/$name"
    mount --move "/tmp/old/$name" "/tmp/new/$name"
    test -f "/tmp/new/$name/runtime-marker"
done
'''
                    result = subprocess.run([
                        "unshare", "--user", "--map-auto", "--map-root-user",
                        "bwrap", "--ro-bind", "/", "/", "--tmpfs", "/tmp",
                        "--ro-bind", str(destination), "/tmp/new", "--dev", "/dev",
                        "--proc", "/proc", "--setenv", "LC_ALL", "C",
                        "/bin/sh", "-c", script,
                    ], text=True, capture_output=True, timeout=30)
                    if restore_mountpoints:
                        self.assertEqual(result.returncode, 0, result.stderr)
                    else:
                        self.assertNotEqual(result.returncode, 0)
                        self.assertIn("Read-only file system", result.stderr)


if __name__ == "__main__":
    unittest.main()
