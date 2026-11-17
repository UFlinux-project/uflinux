"""Device resizing must fit into Alpine's filesystem startup ordering."""
from graphlib import TopologicalSorter
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]


class DeviceBootOrderTests(unittest.TestCase):
    def test_resize_does_not_deadlock_filesystem_startup(self):
        # Relevant edges from Alpine's modules, hwclock, fsck, root,
        # localmount and bootmisc services. 'clock' is provided by hwclock.
        graph = {
            "modules": set(),
            "clock": {"modules"},
            "fsck": {"clock", "modules"},
            "root": {"clock", "fsck"},
            "localmount": {"fsck", "root", "clock", "modules"},
            "bootmisc": {"localmount", "clock", "root"},
            "uflinux-device": set(),
        }
        # Source the real service with dependency commands stubbed out; start()
        # is only defined, so no disk or filesystem commands are executed.
        script = '''
need() { printf 'after %s\\n' "$@"; }
after() { printf 'after %s\\n' "$@"; }
before() { printf 'before %s\\n' "$@"; }
. "$1"
depend
'''
        edges = subprocess.check_output(
            ["sh", "-c", script, "sh", str(ROOT / "aports/uflinux/uflinux-device.initd")],
            text=True)
        for line in edges.splitlines():
            direction, service = line.split()
            if direction == "after":
                graph["uflinux-device"].add(service)
            else:
                graph.setdefault(service, set()).add("uflinux-device")
        order = list(TopologicalSorter(graph).static_order())
        self.assertLess(order.index("fsck"), order.index("uflinux-device"))
        self.assertLess(order.index("uflinux-device"), order.index("root"))
        self.assertLess(order.index("uflinux-device"), order.index("localmount"))

    def test_initramfs_hands_over_a_read_only_root_for_checks(self):
        profile = (ROOT / "profiles/device-rootfs.sh").read_text()
        assignment = next(line for line in profile.splitlines() if line.startswith('rootargs='))
        for rootfs in ("f2fs", "xfs"):
            with self.subTest(rootfs=rootfs):
                args = subprocess.check_output(
                    ["sh", "-c", 'root_partuuid=test; rootfs="$1"; ' + assignment
                     + '; printf "%s" "$rootargs"', "sh", rootfs], text=True).split()
                self.assertIn("rootflags=ro", args)
                self.assertIn("root=UUID=test", args)

    def test_f2fs_root_is_not_resized_or_marked_complete(self):
        # Run the real start function, with destructive commands replaced by
        # traps. A mounted F2FS root must return before even probing growpart.
        with tempfile.TemporaryDirectory() as temporary:
            stage = Path(temporary)
            (stage / "device").write_text("qcs6490\n")
            commands = stage / "bin"
            commands.mkdir()
            for name in ("growpart", "resize.f2fs", "mount"):
                trap = commands / name
                trap.write_text('#!/bin/sh\necho called >> "$TEST_FORBIDDEN"\nexit 1\n')
                trap.chmod(0o755)
            script = '''
set -eu
. "$1"
device_marker="$2/device"
state_dir="$2/state"
state_file="$state_dir/device-resized"
ebegin() { :; }
eend() { :; }
ewarn() { :; }
findmnt() {
    case "$*" in
        *FSTYPE*) printf 'f2fs\\n' ;;
        *) printf '/dev/mmcblk1p2\\n' ;;
    esac
}
split_root_device() { echo reached-resize-path >> "$TEST_FORBIDDEN"; return 1; }
start
'''
            subprocess.run(["sh", "-c", script, "sh",
                            str(ROOT / "aports/uflinux/uflinux-device.initd"), str(stage)],
                           env=dict(os.environ, PATH=str(commands) + os.pathsep + os.environ['PATH'],
                                    TEST_FORBIDDEN=str(stage / "forbidden")),
                           check=True)
            self.assertFalse((stage / "forbidden").exists())
            self.assertFalse((stage / "state/device-resized").exists())


if __name__ == "__main__":
    unittest.main()
