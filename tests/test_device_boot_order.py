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

    def test_the_generated_kernel_command_line_keeps_the_root_and_policy(self):
        # Raspberry Pi writes boot_arguments output straight into cmdline.txt,
        # so the hand-over arguments and the mitigation policy have to survive
        # that trip together.
        profile = (ROOT / "profiles/device-rootfs.sh").read_text()
        assignment = next(line for line in profile.splitlines() if line.startswith('rootargs='))
        script = '''
root_partuuid=test; rootfs="$1"; %s
. "$2"
boot_arguments standard "console=ttyS0,115200n8 $rootargs"
''' % assignment
        line = subprocess.check_output(
            ["sh", "-c", script, "sh", "f2fs", str(ROOT / "aports/uflinux/uflinux-boot-menu")],
            text=True).split()
        self.assertIn("root=UUID=test", line)
        self.assertIn("rootflags=ro", line)
        self.assertIn("mitigations=off", line)

    def test_f2fs_root_is_not_resized_but_marked_complete(self):
        # Run the real start function, with destructive commands replaced by
        # traps. The initramfs already grew an F2FS root offline, so the
        # service must only record the state file and never touch the
        # partition table, resize the filesystem, or remount anything.
        with tempfile.TemporaryDirectory() as temporary:
            stage = Path(temporary)
            (stage / "device").write_text("qcs6490\n")
            commands = stage / "bin"
            commands.mkdir()
            for name in ("growpart", "resize.f2fs", "mount", "xfs_growfs", "resize2fs"):
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
einfo() { :; }
ewarn() { :; }
findmnt() {
    case "$*" in
        *FSTYPE*) printf 'f2fs\\n' ;;
        *) printf '/dev/mmcblk1p2\\n' ;;
    esac
}
start
'''
            subprocess.run(["sh", "-c", script, "sh",
                            str(ROOT / "aports/uflinux/uflinux-device.initd"), str(stage)],
                           env=dict(os.environ, PATH=str(commands) + os.pathsep + os.environ['PATH'],
                                    TEST_FORBIDDEN=str(stage / "forbidden")),
                           check=True)
            self.assertFalse((stage / "forbidden").exists())
            state = (stage / "state/device-resized").read_text()
            self.assertIn("disk=/dev/mmcblk1", state)
            self.assertIn("partition=2", state)
            self.assertIn("filesystem=f2fs", state)

    def test_resize_hook_stays_quiet_outside_f2fs(self):
        # The hook runs on every device boot. It must exit without touching
        # anything for other filesystems, unresolvable roots, and roots that
        # are not partitions, even though the traps below would happily
        # "succeed".
        hook = ROOT / "aports/uflinux/uflinux-initramfs-resize"
        with tempfile.TemporaryDirectory() as temporary:
            stage = Path(temporary)
            commands = stage / "bin"
            commands.mkdir()
            for name in ("growpart", "resize.f2fs"):
                trap = commands / name
                trap.write_text('#!/bin/sh\necho called >> "$TEST_FORBIDDEN"\nexit 0\n')
                trap.chmod(0o755)
            # An unresolvable volume name, like a root= that points at a
            # volume the image writer never formatted.
            findfs = commands / "findfs"
            findfs.write_text('#!/bin/sh\nexit 1\n')
            findfs.chmod(0o755)
            for number, args in enumerate([["ext4", "UUID=abcd"],
                                           ["f2fs", "UUID=no-such-volume"],
                                           ["f2fs", "/dev/not-a-partition"],
                                           ["f2fs", "/dev/mmcblk1p2"]]):
                with self.subTest(args=args):
                    forbidden = stage / ("forbidden-%d" % number)
                    subprocess.run(["sh", str(hook)] + args, check=True,
                                   env=dict(os.environ,
                                            PATH=str(commands) + os.pathsep + os.environ['PATH'],
                                            TEST_FORBIDDEN=str(forbidden)))
                    self.assertFalse(forbidden.exists())


if __name__ == "__main__":
    unittest.main()
