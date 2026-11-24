#!/usr/bin/env python3
"""Assemble and sign a complete Pages repository; retain two previous releases."""
import argparse
import json
from pathlib import Path
import shutil
import subprocess
import sys
import tarfile
import tempfile

from builder import (ARCHES, Builder, ISO_EXCLUDED_PACKAGES, LOCK, PROJECT, SIGNING_KEY, sha,
                     unsigned_digest, PACKAGE_NAMES)
from sbom import apk_metadata
from kernel_packages import KEY, KEY_FILE, KERNELS, stage_kernels

ALLOWED = set().union(*PACKAGE_NAMES.values())
if not ISO_EXCLUDED_PACKAGES <= ALLOWED:
    raise ValueError("Excluded packages must be custom packages: " + ", ".join(sorted(ISO_EXCLUDED_PACKAGES - ALLOWED)))


def safe_extract(archive, target):
    with tarfile.open(archive) as tar:
        for member in tar.getmembers():
            if not (member.isfile() or member.isdir()):
                raise ValueError("Repository snapshots must contain only regular files and directories")
        tar.extractall(target, filter="data")


def selection(current, history):
    retained = [release for release in history if release["version"] != current["version"]][:2]
    return [current, *(dict(release, packages={arch: release["packages"][arch] for arch in ARCHES})
                       for release in retained)]


# Firmware modes whose boot record gates release publication.
BOOT_MODES = {"x86_64": ("bios", "uefi"), "aarch64": ("uefi",),
              "ppc64le": ("native",), "riscv64": ("uefi",)}


def boot_modes(arch):
    """Boot modes that must have a verified record for a release architecture."""
    return BOOT_MODES.get(arch, ())


def required_in_iso(pkgname):
    """Whether the live ISO must carry a published custom package.

    Debug, documentation and development subpackages are optional additions, and
    the device-only set is installed by device images from the published
    repository rather than by the live system, so neither is in the ISO inventory.
    """
    return not pkgname.endswith(("-dbg", "-doc", "-dev", "-libintl")) and pkgname not in ISO_EXCLUDED_PACKAGES


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--outdir", default="out")
    parser.add_argument("--version", required=True)
    parser.add_argument("--previous")
    args = parser.parse_args()
    out = Path(args.outdir).resolve()
    args.arch = "x86_64"
    args.workdir = str(PROJECT / ".state/publisher")
    args.cache = str(PROJECT / ".cache/publisher")
    args.repository_url = None
    args.release = True
    builder = Builder(args)
    try:
        builder.bootstrap()
        with builder.mounted(), tempfile.TemporaryDirectory(prefix="uflinux-history-") as temporary:
            builder.tools("zstd")
            public = builder.signing_key(release=True)
            pages = out / "pages"
            if pages.exists():
                shutil.rmtree(pages)
            branch = pages / "apk" / LOCK["alpine"]["branch"] / "uflinux"
            branch.mkdir(parents=True)
            history = []
            previous = Path(temporary)
            if args.previous:
                if Path(args.previous).is_dir():
                    previous = Path(args.previous).resolve()
                else:
                    safe_extract(args.previous, previous)
                history = json.loads((previous / "releases.json").read_text())
                if history:
                    order = builder.chroot("apk", "version", "-t", args.version, history[0]["version"], stdout=subprocess.PIPE).stdout.decode().strip()
                    if order == "<":
                        raise ValueError("Refusing to replace a newer repository with an older release")
            current = {"version": args.version, "packages": {}}
            for arch in ARCHES:
                # Publication cannot accidentally succeed on a partial matrix.
                check = json.loads((out / "checks" / arch / "verified.json").read_text())
                if check["version"] != args.version or check["signing"] != "release":
                    raise ValueError("Unverified release package set for " + arch)
                if check.get("verified_packages") != {p.name: sha(p) for p in (out / "repository" / arch).glob("*.apk")}:
                    raise ValueError("Verification does not cover the current package archives for " + arch)
                image_record = json.loads((out / "metadata" / arch / "image.json").read_text())
                iso = out / "images" / arch / f"uflinux-{args.version}-{arch}.iso"
                if image_record.get("iso_sha256") != sha(iso):
                    raise ValueError("Image metadata does not describe the current ISO for " + arch)
                inventory = json.loads((out / "metadata" / arch / f"uflinux-{args.version}-{arch}.spdx.json").read_text())
                included = {(package["name"], package["versionInfo"]): package["checksums"][0]["checksumValue"] for package in inventory["packages"]}
                names = set()
                for package in (out / "repository" / arch).glob("*.apk"):
                    info = apk_metadata(package)
                    names.add(info["pkgname"])
                    if required_in_iso(info["pkgname"]) and included.get((info["pkgname"], info["pkgver"])) != sha(package):
                        raise ValueError("The ISO does not contain the current custom package: " + package.name)
                if names != ALLOWED:
                    raise ValueError("Incomplete or unexpected custom package set for " + arch)
                for mode in boot_modes(arch):
                    result = json.loads((out / "vm" / arch / mode / "verified.json").read_text())
                    iso = out / "images" / arch / f"uflinux-{args.version}-{arch}.iso"
                    if arch == "riscv64":
                        if not result.get("grub_menu") or result.get("iso_sha256") != sha(iso):
                            raise ValueError("Missing RISC-V GRUB menu verification")
                        continue
                    if result.get("musl") != LOCK["musl"]["version"] + "-r" + LOCK["musl"]["revision"]:
                        raise ValueError("Missing installed musl verification for " + arch)
                    if not result.get("xfs_install_reboot") or result.get("iso_sha256") != sha(iso):
                        raise ValueError("Missing installed-system boot verification for " + arch)
                    if arch == "x86_64":
                        if not all(result.get(key) for key in ("grub_usb_tty", "grub_installed_debug", "gui_install_reboot")):
                            raise ValueError("Missing GRUB boot-mode verification for " + mode)
                        memory = result.get("settings_512m") or {}
                        if memory.get("ram_mib") != 512 or memory.get("swap") is not False or memory.get("oom") is not False:
                            raise ValueError("Missing installed 512 MiB Settings acceptance for " + mode)
                        if mode == "bios" and not result.get("network_clock"):
                            raise ValueError("Missing real ConnMan+iwd and ConnMan SNTP verification")
                        power = result.get("power") or {}
                        if not power.get("gtk_authorization_cancel_and_grant") or not power.get("physical_key_do_nothing") or not power.get("physical_key_poweroff") or set(power.get("profiles", [])) != {"power-saver", "balanced", "performance"}:
                            raise ValueError("Missing TLP profiles and BusyBox power-button verification")
                current["packages"][arch] = sorted(p.name for p in (out / "repository" / arch).glob("*.apk"))
                if not current["packages"][arch]:
                    raise ValueError("Empty release architecture: " + arch)
                if arch == "x86_64":
                    proof = json.loads((out / "metadata" / arch / "reproducibility.json").read_text())
                    payloads = {p.name: unsigned_digest(p) for p in (out / "repository" / arch).glob("*.apk")}
                    if not proof.get("matched") or proof.get("baseline") != payloads or proof.get("rebuilt") != payloads:
                        raise ValueError("The reproducibility record does not cover the current native package set")
            releases = selection(current, history)
            for arch in ARCHES:
                staging = builder.root / "repo" / arch
                if staging.exists():
                    shutil.rmtree(staging)
                staging.mkdir(parents=True, exist_ok=True)
                names = set()
                for release in releases:
                    for name in release["packages"][arch]:
                        if Path(name).name != name or not name.endswith(".apk"):
                            raise ValueError("Invalid APK snapshot filename")
                        if name in names:
                            if name.startswith("musl-"):
                                retained = previous / arch / name
                                if unsigned_digest(retained) != unsigned_digest(staging / name):
                                    raise ValueError("Bump the revision for changed musl package " + name)
                            continue
                        names.add(name)
                        source = (out / "repository" / arch if release is current else previous / arch) / name
                        if apk_metadata(source)["pkgname"] not in ALLOWED:
                            raise ValueError("Unexpected custom package namespace: " + name)
                        shutil.copy2(source, staging / name)
                        builder.chroot("apk", "verify", f"/repo/{arch}/{name}")
                builder.index(staging)
                stage_kernels(builder, arch, staging)
                shutil.copytree(staging, branch / arch)
            shutil.copy2(KEY_FILE, branch / KEY)
            (branch / "kernels.json").write_text(json.dumps(KERNELS, indent=2) + "\n")
            shutil.copy2(public, branch / (SIGNING_KEY + ".pub"))
            (branch / "releases.json").write_text(json.dumps(releases, indent=2) + "\n")
            (pages / ".nojekyll").touch()
            (pages / "index.html").write_text('<!doctype html><html lang="en"><meta charset="utf-8"><title>UFLinux APK repository</title><h1>UFLinux</h1><p>Signed Alpine packages. Download installation images from this project’s GitHub Releases.</p></html>\n')
            size = sum(p.stat().st_size for p in pages.rglob("*") if p.is_file())
            if size > 900 * 1024**2:
                raise ValueError("Pages repository exceeds the 900 MiB safety budget")
        return 0
    finally:
        (builder.root / "var/lib/uflinux/keys" / SIGNING_KEY).unlink(missing_ok=True)


if __name__ == "__main__":
    sys.exit(main())
