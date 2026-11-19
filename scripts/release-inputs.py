#!/usr/bin/env python3
"""Stage one architecture's rebuild inputs without collecting all targets on one runner."""
import argparse
import json
from pathlib import Path

from builder import ARCHES, CARGO_ORIGINS, PACKAGE_NAMES, check_asset_size, link_or_copy, sha

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--arch", choices=ARCHES, required=True)
parser.add_argument("--version", required=True)
args = parser.parse_args()
out = Path("out")
directory = out / "inputs" / args.arch
inputs = sorted(directory.rglob("*.tar.gz"))
if (directory / "build-apks.tar.gz").exists():
    expected = {"build-apks.tar.gz", "image-apks.tar.gz", "cargo-sources.tar.gz", "sources.tar.gz"}
else:
    expected = {"image-apks.tar.gz"}
    for origin in PACKAGE_NAMES:
        expected.update({origin + "/build-apks.tar.gz", origin + "/sources.tar.gz"})
        if origin in CARGO_ORIGINS:
            expected.add(origin + "/cargo-sources.tar.gz")
    # Non-Rust jobs may retain an existing Cargo cache during local builds.
    expected.update(str(p.relative_to(directory)) for p in inputs
                    if p.name == "cargo-sources.tar.gz" and p.parent.name in PACKAGE_NAMES)
record = out / "metadata" / args.arch / "package-shards" / "uflinux-settings" / "compile.json"
if record.is_file() and json.loads(record.read_text()).get("settings_pgo"):
    expected.add("uflinux-settings/settings-pgo-profiles.tar.gz")
if {str(p.relative_to(directory)) for p in inputs} != expected:
    raise RuntimeError("The complete rebuild input set is required")
checksums = []
for source in inputs:
    # Rebuild inputs travel as workflow artifacts, where the 2 GiB limit is
    # GitHub's upload cap rather than a release-asset one, so no warning applies.
    check_asset_size(source, "Rebuild input", warn=False)
    name = "-".join(source.relative_to(directory).parts)
    target = out / f"uflinux-{args.version}-{args.arch}-{name}"
    link_or_copy(source, target)
    checksums.append(sha(target) + "  " + target.name + "\n")
(out / f"SHA256SUMS-{args.arch}-inputs").write_text("".join(checksums))
