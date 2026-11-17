"""Device resizing must fit into Alpine's filesystem startup ordering."""
from graphlib import TopologicalSorter
from pathlib import Path
import subprocess
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

    def test_initramfs_hands_over_a_read_only_root_for_resize(self):
        profile = (ROOT / "profiles/device-rootfs.sh").read_text()
        assignment = next(line for line in profile.splitlines() if line.startswith('rootargs='))
        for rootfs in ("f2fs", "xfs"):
            with self.subTest(rootfs=rootfs):
                args = subprocess.check_output(
                    ["sh", "-c", 'root_partuuid=test; rootfs="$1"; ' + assignment
                     + '; printf "%s" "$rootargs"', "sh", rootfs], text=True).split()
                self.assertIn("rootflags=ro", args)
                self.assertIn("root=UUID=test", args)


if __name__ == "__main__":
    unittest.main()
