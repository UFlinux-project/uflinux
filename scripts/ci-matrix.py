#!/usr/bin/env python3
import json
import hashlib
import os
from pathlib import Path
import re
import subprocess

from builder import PACKAGE_NAMES

root = Path(__file__).resolve().parents[1]
arches = json.loads((root / "config/architectures.json").read_text())
devices = json.loads((root / "config/devices.json").read_text())
lock = json.loads((root / "config/sources.lock.json").read_text())
def configuration(event, ref, full=False, device_builds=False):
    release = event == "push" and ref.startswith("refs/tags/v")
    version = ref.removeprefix("refs/tags/v") if release else lock["distribution"]["version"]
    if not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+(?:_(?:alpha|beta|rc)[0-9]+)?", version):
        raise ValueError("Tag must be v followed by an Alpine-compatible version")
    # Ordinary commits smoke-test x86_64 only; releases and a full-matrix manual
    # dispatch cover every architecture.
    selected = list(arches) if release or full else ["x86_64"]
    matrix = {"include": [{"arch": arch, "runner": arches[arch]["runner"],
                           "cargo_jobs": arches[arch]["jobs"]} for arch in selected]}
    packages = {"include": [{"arch": arch, "runner": arches[arch]["runner"], "package": package,
                             "cargo_jobs": arches[arch]["jobs"],
                             "settings_pgo": release and package == "uflinux-settings" and arch in {"x86_64", "aarch64"},
                             "rds_pgo": release and package == "uflinux-rds" and arch in {"x86_64", "aarch64"}}
                             for arch in selected for package in PACKAGE_NAMES]}
    # Only the x86_64 ISO is boot-tested, in both firmware modes, whatever the
    # build matrix covers. Other architectures are built but never started.
    vm = {"include": [{"arch": "x86_64", "mode": mode} for mode in ("bios", "uefi")]}
    # Device images are supplementary artifacts: they build on release tags and on an
    # explicit manual request, and never gate the release itself.
    enabled = release or (full and device_builds)
    device_matrix = {"include": [{"device": device_id, "arch": arch, "runner": arches[arch]["runner"],
                                  "status": device["status"], "experimental": str(device["status"] == "experimental").lower()}
                                 for device_id, device in sorted(devices.items())
                                 for arch in device["arches"] if enabled]}
    return {"matrix": matrix, "packages": packages, "vm": vm, "devices": device_matrix,
            "devices_enabled": str(enabled).lower(),
            "version": version, "release": str(release).lower()}

def main():
    epoch = subprocess.check_output(["git", "log", "-1", "--format=%ct"], text=True).strip()
    # Cache dependencies across changes to our deterministic source bundle. Cargo's
    # fingerprints and package-scoped clean in prepare() still rebuild project code.
    dependencies = hashlib.sha256(json.dumps({k: lock[k] for k in ("alpine", "rootfs", "sources")}, sort_keys=True).encode())
    dependencies.update((root / "aports/musl/APKBUILD").read_bytes())
    for name in ("settings/Cargo.lock", "settings/Cargo.toml", "rds/Cargo.lock", "rds/Cargo.toml", "aports/connman/APKBUILD", "aports/gpu-screen-recorder/APKBUILD", "aports/uflinux-regreet/APKBUILD", "aports/uflinux-settings/APKBUILD", "aports/uflinux-rds/APKBUILD", "aports/xdg-desktop-portal-gtk4/APKBUILD", "aports/simp1e-cursors/APKBUILD"):
        dependencies.update((root / name).read_bytes().split(b'sha512sums=')[0])
    with open(os.environ["GITHUB_OUTPUT"], "a") as output:
        for key, value in {**configuration(os.environ.get("EVENT", ""),os.environ.get("REF", ""),
                           os.environ.get("FULL", "").lower() in ("true", "1"),
                           os.environ.get("DEVICES", "").lower() in ("true", "1")),
                           "epoch": epoch, "cache": dependencies.hexdigest()}.items():
            output.write(f"{key}={json.dumps(value) if isinstance(value,dict) else value}\n")


if __name__ == "__main__":
    main()
