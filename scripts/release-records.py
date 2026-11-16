#!/usr/bin/env python3
"""Archive audit records and generate checksums for release ISOs."""
import argparse
from pathlib import Path
import subprocess
import os

from builder import LOCK, sha

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--version", required=True)
args = parser.parse_args()
out = Path("out")
epoch = os.environ.get("SOURCE_DATE_EPOCH", str(LOCK["source_date_epoch"]))
archive = out / "uflinux-build-records.tar.gz"
tar = subprocess.Popen(["tar", "--sort=name", "--mtime=@" + epoch, "--owner=0", "--group=0", "--numeric-owner",
                        "-C", "out", "-cf", "-", "metadata", "checks", "vm"], stdout=subprocess.PIPE)
with archive.open("wb") as stream:
    subprocess.run(["gzip", "-9n"], stdin=tar.stdout, stdout=stream, check=True)
tar.stdout.close()
if tar.wait():
    raise RuntimeError("Failed to create build record archive")
assets = sorted(out.glob("images/*/*.iso"))
if not assets:
    raise ValueError("No release ISOs found")
if len({p.name for p in assets}) != len(assets):
    raise ValueError("Release asset basenames must be unique")
if any(p.stat().st_size >= 2 * 1024**3 for p in assets):
    raise ValueError("An asset exceeds GitHub Releases' per-file size limit")
checksums = "".join(sha(p) + "  " + p.name + "\n" for p in sorted(assets))
(out / "SHA256SUMS").write_text(checksums)
