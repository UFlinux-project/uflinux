"""The compressed swap policy must follow the installed RAM and the CPU."""
from graphlib import TopologicalSorter
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
SERVICE = ROOT / "aports/uflinux/uflinux-zram.initd"

# The service is sourced with every path and message helper replaced, so the
# real start() runs against a fake /proc and a fake sysfs. Commands it reaches
# for outside the shell are trap scripts on PATH that record their arguments.
HARNESS = r'''
set -eu
. "$1"
root="$2"
meminfo="$root/meminfo"
cpuinfo="$root/cpuinfo"
mounts="$root/mounts"
zram_control="$root/sys/zram-control/hot_add"
zram_block="$root/sys/block"
zswap_module="$root/sys/zswap/parameters"
conf_file="$root/confd"
SWAPFILE="$root/swapfile"
ebegin() { :; }
eend() { [ "$1" = 0 ] || echo "failed $*" >> "$root/log"; }
ewarn() { echo "warn $*" >> "$root/log"; }
start
'''


class ZramPolicyTests(unittest.TestCase):
    @staticmethod
    def record(root, commands, name, extra=''):
        trap = commands / name
        trap.write_text('#!/bin/sh\nprintf "%s %%s\\n" "$*" >> "%s/log"\n%s'
                        % (name, root, extra))
        trap.chmod(0o755)

    def stage(self, root, *, memory_kib, processors, machine, fstype='ext4',
              confd=None, zswap=True, zram_control=True):
        """Build a fake /proc, sysfs and mount table for one scenario."""
        root = Path(root)
        (root / "meminfo").write_text("MemTotal:       %d kB\n" % memory_kib)
        (root / "cpuinfo").write_text(
            "".join("processor\t: %d\n" % index for index in range(processors)))
        (root / "mounts").write_text("/dev/root / %s rw 0 0\n" % fstype)
        (root / "log").write_text("")
        if confd is not None:
            (root / "confd").write_text(confd)
        block = root / "sys/block/zram0"
        block.mkdir(parents=True)
        for attribute in ("disksize", "comp_algorithm", "mem_limit", "reset"):
            (block / attribute).write_text("")
        control = root / "sys/zram-control"
        control.mkdir(parents=True)
        if zram_control:
            (control / "hot_add").write_text("0\n")
        parameters = root / "sys/zswap/parameters"
        parameters.mkdir(parents=True)
        if zswap:
            for name in ("enabled", "compressor", "max_pool_percent",
                         "accept_threshold_percent"):
                (parameters / name).write_text("N\n")
        commands = root / "bin"
        commands.mkdir()
        for name in ("swapon", "swapoff", "mkswap", "modprobe"):
            self.record(root, commands, name)
        # A real fallocate and a real dd both leave the sized file behind, and
        # the service only formats what its own allocation produced. The size
        # after -l is an argument too, so it must not be taken for the target.
        self.record(root, commands, "fallocate",
                    'skip=0\nfor argument in "$@"; do\n'
                    '  if [ "$skip" = 1 ]; then skip=0; continue; fi\n'
                    '  case "$argument" in -l) skip=1 ;; -*) ;; *) : > "$argument" ;; esac\n'
                    'done\n')
        self.record(root, commands, "dd",
                    'for argument in "$@"; do case "$argument" in of=*) : > "${argument#of=}";; '
                    'esac; done\n')
        uname = commands / "uname"
        uname.write_text('#!/bin/sh\nprintf "%s\\n"\n' % machine)
        uname.chmod(0o755)
        return commands

    def run_service(self, *, expect_success=True, **scenario):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            commands = self.stage(root, **scenario)
            completed = subprocess.run(
                ["sh", "-c", HARNESS, "sh", str(SERVICE), str(root)],
                env=dict(os.environ,
                         PATH=str(commands) + os.pathsep + os.environ['PATH']))
            self.assertEqual(completed.returncode == 0, expect_success)
            attributes = {}
            for name in ("disksize", "comp_algorithm", "mem_limit"):
                attributes[name] = (root / "sys/block/zram0" / name).read_text().strip()
            zswap = {}
            for name in ("enabled", "compressor", "max_pool_percent"):
                path = root / "sys/zswap/parameters" / name
                zswap[name] = path.read_text().strip() if path.is_file() else None
            return {"log": (root / "log").read_text().splitlines(),
                    "attributes": attributes,
                    "zswap": zswap,
                    "swapfile": str(root / "swapfile") if (root / "swapfile").exists() else None}

    def test_small_board_with_weak_cpu_gets_lz4_zram_only(self):
        result = self.run_service(memory_kib=3_612_672, processors=4, machine="riscv64")
        self.assertEqual(result["attributes"]["comp_algorithm"], "lz4")
        self.assertEqual(result["attributes"]["disksize"], "1764M")
        self.assertEqual(result["attributes"]["mem_limit"], "882M")
        # Below the split there is no second layer: no swapfile, no zswap.
        self.assertIsNone(result["swapfile"])
        self.assertEqual(result["zswap"]["enabled"], "N")
        self.assertEqual([line for line in result["log"] if line.startswith("swapon -p 10 ")], [])
        self.assertEqual(result["log"], ["swapon -p 100 /dev/zram0"])

    def test_large_board_gets_zstd_zram_swapfile_and_zswap(self):
        result = self.run_service(memory_kib=16_777_216, processors=8, machine="x86_64")
        self.assertEqual(result["attributes"]["comp_algorithm"], "zstd")
        # Both zram sizes are clamped, so a 16 GiB machine is not asked to hold
        # half of its RAM in a compressed device.
        self.assertEqual(result["attributes"]["disksize"], "2048M")
        self.assertEqual(result["attributes"]["mem_limit"], "1024M")
        swapfile = result["swapfile"]
        self.assertIsNotNone(swapfile)
        self.assertEqual(result["zswap"], {"enabled": "1", "compressor": "zstd",
                                          "max_pool_percent": "20"})
        self.assertEqual(result["log"], ["swapon -p 100 /dev/zram0",
                                         "fallocate -l 8192M %s" % swapfile,
                                         "mkswap -L uflinux-swap %s" % swapfile,
                                         "swapon -p 10 %s" % swapfile])
        self.assertEqual([line for line in result["log"] if line.startswith("failed")], [])

    def test_two_core_64_bit_board_counts_as_low_end(self):
        result = self.run_service(memory_kib=16_777_216, processors=2, machine="aarch64")
        self.assertEqual(result["attributes"]["comp_algorithm"], "lz4")

    def test_confdd_overrides_win(self):
        result = self.run_service(
            memory_kib=16_777_216, processors=8, machine="x86_64",
            confd='ZRAM_COMP=lz4\nZRAM_DISKSIZE=512M\nZSWAP_COMP=lz4\nRAM_SPLIT_MIB=65536\n')
        self.assertEqual(result["attributes"]["comp_algorithm"], "lz4")
        self.assertEqual(result["attributes"]["disksize"], "512M")
        self.assertIsNone(result["swapfile"])

    def test_f2fs_root_writes_the_swapfile_instead_of_allocating_it(self):
        result = self.run_service(memory_kib=16_777_216, processors=8, machine="x86_64",
                                  fstype="f2fs")
        self.assertIsNotNone(result["swapfile"])
        self.assertEqual([line for line in result["log"] if line.startswith("fallocate")], [])
        self.assertTrue(any(line.startswith("dd if=/dev/zero") for line in result["log"]))

    def test_missing_zswap_only_costs_the_second_layer(self):
        result = self.run_service(memory_kib=16_777_216, processors=8, machine="x86_64",
                                  zswap=False)
        self.assertIsNotNone(result["swapfile"])
        self.assertEqual([line for line in result["log"] if line.startswith("failed")], [])

    def test_a_machine_without_zram_reports_a_failure(self):
        result = self.run_service(memory_kib=16_777_216, processors=8, machine="x86_64",
                                  zram_control=False, expect_success=False)
        self.assertIn("failed 1 cannot create a zram device", result["log"])

    def test_service_fits_alpines_boot_order(self):
        # Relevant edges from Alpine's own sysinit and boot services. 'clock' is
        # provided by hwclock, and hwdrivers already carries a need on sysfs.
        graph = {
            "modules": set(),
            "sysfs": set(),
            "clock": {"modules"},
            "fsck": {"clock", "modules"},
            "root": {"clock", "fsck"},
            "localmount": {"fsck", "root", "clock", "modules", "sysfs"},
            "uflinux-zram": set(),
        }
        script = '''
need() { printf 'after %s\\n' "$@"; }
after() { printf 'after %s\\n' "$@"; }
before() { printf 'before %s\\n' "$@"; }
keyword() { :; }
. "$1"
depend
'''
        edges = subprocess.check_output(["sh", "-c", script, "sh", str(SERVICE)], text=True)
        for line in edges.splitlines():
            direction, service = line.split()
            if direction == "after":
                graph["uflinux-zram"].add(service)
            else:
                graph.setdefault(service, set()).add("uflinux-zram")
        order = list(TopologicalSorter(graph).static_order())
        self.assertLess(order.index("sysfs"), order.index("uflinux-zram"))
        self.assertLess(order.index("modules"), order.index("uflinux-zram"))
        self.assertLess(order.index("localmount"), order.index("uflinux-zram"))


if __name__ == "__main__":
    unittest.main()
