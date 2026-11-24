#!/usr/bin/env python3
"""Check declared desktop/installer dependencies against every target repository."""
import concurrent.futures
import importlib
import io
import json
import os
from pathlib import Path
import re
import shlex
import sys
import tarfile
import time
import urllib.error
import urllib.request

sys.path.insert(0, str(Path(__file__).resolve().parent))
from builder import browser_packages, device_packages, firmware_packages

# ci-matrix owns the single definition of what counts as a release.
ci_matrix = importlib.import_module("ci-matrix")

root = Path(__file__).resolve().parents[1]
lock = json.loads((root / "config/sources.lock.json").read_text())
text = (root / "aports/uflinux/APKBUILD").read_text()
required = set()
for block in re.findall(r'\bdepends="([^"]*)"', text):
    for item in shlex.split(block):
        name = re.split(r"[=<>~]", item)[0]
        if not name.startswith(("uflinux", "$")):
            required.add(name)
required.update({"linux-lts", "xfsprogs", "alpine-conf", "rust", "cargo", "cargo-auditable",
                 "gtk4.0-dev", "gtk4-layer-shell-dev", "pulseaudio-dev", "openssl-dev", "abuild"})
required.update({"polkit-dev", "eudev-dev", "vulkan-loader-dev", "mesa-dev", "pkgconf",
                 "ddcutil", "tzdata", "vulkan-loader", "mesa-vulkan-ati"})
required.update({"rsvg-convert", "glycin-thumbnailer", "libx11-dev", "make"})
required.update({"tlp", "tlp-openrc", "tlp-udev"})
required.update({"py3-atspi", "wtype", "gcc", "musl-dev", "wayland-dev", "libxkbcommon-dev", "desktop-file-utils", "wlr-randr"})
required.update({"autoconf", "automake", "dbus-dev", "glib-dev", "libmnl-dev", "libnftnl-dev", "libtool", "readline-dev", "linux-headers"})
# aports/uflinux-rds: bindgen against libpam, aws-lc-sys' CMake builder, the wasm
# browser client, and the out-of-process ffmpeg H.264 encoder used by the vaapi
# subpackage. Only aports/uflinux/APKBUILD is scanned above, so the RDS-specific
# names are listed here instead.
required.update({"clang22-dev", "linux-pam-dev", "cmake", "perl", "rust-wasm", "wasm-bindgen",
                 "zlib-dev", "zstd-dev", "openh264", "ffmpeg", "libva",
                 "llvm22", "llvm22-dev", "llvm22-static", "llvm22-gtest", "clang22", "samurai",
                 "libxml2-dev", "coreutils", "font-dejavu"})

# Packages built from this repository's own aports tree are not in the Alpine
# repositories; they are compiled by the build jobs before the image is assembled.
local = set()
for recipe in (root / "aports").glob("*/APKBUILD"):
    text = recipe.read_text()
    name = re.search(r'^pkgname="?([^"\s]+)"?$', text, re.M)
    if name:
        local.add(name.group(1))
    # Subpackages (e.g. connman-vpn) are published by the same job as their
    # pkgname, so a dependency on one is satisfied locally too.
    for block in re.findall(r'^subpackages="([^"]*)"', text, re.M):
        for item in shlex.split(block):
            local.add(item.split(":")[0].replace("$pkgname", name.group(1) if name else "$pkgname"))
required -= local


# Every index below is a live URL, so a mirror briefly returning 503 has to be
# retried rather than fail the whole gate. A 404 is a real problem and is
# reported immediately.
RETRIES = 3
RETRY_DELAY = 10


def index_entries(url):
    for attempt in range(RETRIES):
        try:
            with urllib.request.urlopen(url, timeout=60) as response:
                data = response.read()
            break
        except urllib.error.HTTPError as error:
            if error.code < 500 or attempt + 1 == RETRIES:
                raise
        except urllib.error.URLError:
            if attempt + 1 == RETRIES:
                raise
        time.sleep(RETRY_DELAY * (attempt + 1))
    with tarfile.open(fileobj=io.BytesIO(data)) as archive:
        return archive.extractfile("APKINDEX").read().decode()


def alpine_index(arch):
    found = set()
    versions = {}
    for repository in ("main", "community"):
        url = f"{lock['alpine']['mirror']}/{lock['alpine']['branch']}/{repository}/{arch}/APKINDEX.tar.gz"
        index = index_entries(url)
        found.update(re.findall(r"^P:(.+)$", index, re.M))
        for entry in index.split("\n\n"):
            fields=dict(line.split(":",1) for line in entry.splitlines() if ":" in line)
            if "P" in fields and "V" in fields: versions[fields["P"]]=fields["V"]
    return found, versions


def check(arch):
    found, versions = alpine_index(arch)
    baseline = lock["musl"]
    upstream = f"{baseline['upstream_version']}-r{baseline['upstream_revision']}"
    if versions.get("musl") != upstream:
        raise RuntimeError(f"{arch}: Alpine musl is {versions.get('musl')}, but UFLinux is based on {upstream}. Rebase the recipe.")
    # The firmware list replaces the metapackage, so it has to keep up with
    # Alpine: every published subpackage except the Snapdragon-only qcom and the
    # empty none. Anything new that Alpine ships would otherwise go missing.
    shipped = {name for name in found if name.startswith("linux-firmware")}
    expected = shipped - {"linux-firmware", "linux-firmware-none", "linux-firmware-qcom"}
    if set(firmware_packages()) != expected:
        raise RuntimeError(f"{arch}: firmware list is out of date: "
                           f"missing {', '.join(sorted(expected - set(firmware_packages()))) or '-'}; "
                           f"stale {', '.join(sorted(set(firmware_packages()) - expected)) or '-'}")
    boots = {"grub-ieee1275"} if arch == "ppc64le" else {"grub-efi"}
    if arch in {"x86_64", "aarch64", "riscv64"}:
        boots.add("efibootmgr")
    if arch == "x86_64":
        boots.update({"grub-bios", "amd-ucode", "intel-ucode"})
    if arch == "x86_64":
        boots.update({"mesa-vulkan-intel", "mesa-vulkan-nouveau", "mesa-vulkan-virtio"})
    if arch == "aarch64":
        boots.update({"mesa-vulkan-asahi", "mesa-vulkan-broadcom", "mesa-vulkan-freedreno",
                      "mesa-vulkan-imagination", "mesa-vulkan-intel", "mesa-vulkan-nouveau",
                      "mesa-vulkan-panfrost", "mesa-vulkan-virtio"})
    wanted = required | boots | set(browser_packages(arch)) | set(firmware_packages())
    missing = wanted - found
    if missing:
        raise RuntimeError(f"{arch}: unavailable packages: {', '.join(sorted(missing))}")
    if not versions.get("tlp", "").startswith("1.10."):
        raise RuntimeError(f"{arch}: Settings expects the tested TLP 1.10 series")
    return f"{arch}: {len(wanted)} required Alpine packages available"


with concurrent.futures.ThreadPoolExecutor(max_workers=4) as pool:
    for result in pool.map(check, lock["rootfs"]):
        print(result)


# uflinux-desktop-meta depends on uflinux-files, which only the apps repository
# carries, so its index has to stay reachable and keep providing it. The index
# is deliberately not pinned by hash: the repository is republished whenever its
# packages are rebuilt, so the URL itself is the stable input and the packages
# are checked against the live index (same policy as the Nura suite).
#
# Only a release tag is gated on the result. The apps repository is published
# independently of this one, so an ordinary check build must not fail because it
# is briefly unavailable or mid-republish; there the outcome is a warning. The
# image stage still needs the packages, so a release candidate cannot ship an
# image that could not resolve them.
apps = lock["apps"]
APPS_REQUIRED = ci_matrix.is_release(os.environ.get("EVENT", ""), os.environ.get("REF", ""))


def check_apps(arch):
    url = f"{apps['url']}/{arch}/APKINDEX.tar.gz"
    try:
        index = index_entries(url)
    except urllib.error.HTTPError as error:
        if error.code == 404:
            raise RuntimeError(
                f"{arch}: apps repository index is missing at {url}; "
                "restore the architecture in the apps repository before building"
            ) from error
        raise
    found = set(re.findall(r"^P:(.+)$", index, re.M))
    missing = {"uflinux-files", "uflinux-files-desktop", "uflinux-files-searchd"} - found
    if missing:
        raise RuntimeError(f"{arch}: apps packages unavailable: {', '.join(sorted(missing))}")
    return f"{arch}: apps index provides {len(found)} packages"


with concurrent.futures.ThreadPoolExecutor(max_workers=4) as pool:
    futures = {arch: pool.submit(check_apps, arch) for arch in apps["arches"]}
    for arch, future in futures.items():
        try:
            print(future.result())
        except Exception as error:
            if APPS_REQUIRED:
                raise
            print(f"warning: {error} (the apps repository is only required for releases)")


devices = json.loads((root / "config/devices.json").read_text())
device_lock = json.loads((root / "config/devices.lock.json").read_text())["nura"]


def check_device(device_id):
    device = devices[device_id]
    messages = []
    for arch in device["arches"]:
        found, _ = alpine_index(arch)
        # The UFLinux subpackages are built from this repository's own aports tree,
        # and the Nura extras are validated against the pinned Nura index below.
        installed = [name for name in device_packages(device, arch)
                     if not name.startswith(("uflinux", "linux-uflinux-")) and name not in device["nura_packages"]]
        missing = set(installed) - found - local
        if missing:
            raise RuntimeError(f"{device_id} ({arch}): unavailable device packages: {', '.join(sorted(missing))}")
        messages.append(f"{device_id} ({arch}): {len(installed)} device packages available")
    if device["nura"]:
        arch = device["arches"][0]
        # The Nura index is deliberately not pinned: postmarketOS rebuilds a
        # release branch's index regularly, so the pinned branch is the stable
        # input and the packages are checked against the branch's live index.
        url = f"{device_lock['mirror']}/{device_lock['branch']}/{arch}/APKINDEX.tar.gz"
        found = set(re.findall(r"^P:(.+)$", index_entries(url), re.M))
        missing = set(device["nura_packages"]) - found
        if missing:
            raise RuntimeError(f"{device_id}: unavailable Nura packages: {', '.join(sorted(missing))}")
        messages.append(f"{device_id}: Nura {device_lock['branch']} index provides {len(device['nura_packages'])} extra packages")
    return messages


with concurrent.futures.ThreadPoolExecutor(max_workers=4) as pool:
    for result in pool.map(check_device, sorted(devices)):
        for line in result:
            print(line)
