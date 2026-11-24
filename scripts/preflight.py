#!/usr/bin/env python3
"""Fail before long builds if runner labels, binfmt or disk space are unsuitable."""
import argparse
import json
from pathlib import Path
import platform
import shutil
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]
arches = json.loads((ROOT / "config/architectures.json").read_text())
parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--arch", choices=arches, required=True)
args = parser.parse_args()
for command in ("python3", "tar", "mount", "umount", "chroot"):
    if not shutil.which(command):
        sys.exit("Missing build tool: " + command)
if args.arch != platform.machine():
    name = "qemu-" + arches[args.arch]["qemu_user"]
    entries = list(Path("/proc/sys/fs/binfmt_misc").glob(name + "*"))
    if not entries or not any("enabled" in entry.read_text() and "F" in entry.read_text().split("flags:")[-1] for entry in entries):
        sys.exit(f"Register {name} with binfmt_misc and the F (fix-binary) flag before building")
if shutil.disk_usage(ROOT).free < 10 * 1024**3:
    sys.exit("At least 10 GiB of free build storage is required")
print(f"Runner preflight passed for {args.arch} on {platform.machine()}")
