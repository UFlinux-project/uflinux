#!/usr/bin/env python3
"""Alpine chroot build stages. Root is needed only on the disposable build host."""
import argparse
import contextlib
import gzip
import hashlib
import io
import json
import os
from pathlib import Path
import platform
import re
import shlex
import shutil
import subprocess
import struct
import sys
import tarfile
import tempfile
import time
import urllib.parse
import urllib.request
import zlib
import zipfile

from kernel_packages import (KEY as KERNEL_KEY, LIVE_TARGET, kernel_index, kernels,
                             kernel_fingerprint, live_flavor, stage_kernels)

PROJECT = Path(__file__).resolve().parents[1]
LOCK = json.loads((PROJECT / "config/sources.lock.json").read_text())
ARCHES = json.loads((PROJECT / "config/architectures.json").read_text())
DEVICES = json.loads((PROJECT / "config/devices.json").read_text())
DEVICE_LOCK = json.loads((PROJECT / "config/devices.lock.json").read_text())
ESP_START = 2048
ESP_SECTORS = 1048576
SECTOR = 512
GPT_ESP_TYPE = "C12A7328-F81F-11D2-BA4B-00A0C93EC9B0"
GPT_LINUX_TYPE = "0FC63DAF-8483-477B-8E3F-43138715FD6D"
# The device rootfs is assembled from a throwaway chroot whose transient mounts
# and build inputs must not become part of the installed filesystem.
DEVICE_ROOT_EXCLUDES = ("proc", "sys", "dev", "run", "cache", "repo", "nura", "project",
                        "device-esp", "device-esp.img", "device-image",
                        ".uflinux-bootstrap", "etc/apk/cache", "opt/uflinux/grub-2.14",
                        "opt/uflinux/grub-q6a", "opt/uflinux/memtest86plus")
DEVICE_ESP = "/device-esp"
DEVICE_PROJECT_DIRS = ("aports", "profiles", "config", "scripts", "patches")
PROJECT_DIRS = ("aports", "profiles", "config", "scripts", "tests", "patches", "settings")
EFI_SUFFIX = {"aarch64": "AA64", "x86_64": "X64"}
GPT_BIOS_TYPE = "21686148-6449-6E6F-744E-656564454649"
# A device image is several gigabytes of already-compressed packages, so the
# highest level would cost hours of CPU for a few percent of size.
DEVICE_IMAGE_ZSTD_LEVEL = 12
# GitHub Releases rejects an individual asset at 2 GiB, and an ISO that reaches
# it fails the release after the repository has already been deployed. Warn
# while a routine package update is still survivable news.
RELEASE_ASSET_LIMIT = 2 * 1024**3
RELEASE_ASSET_WARNING = 1900 * 1024**2
# Stage names as they appear on the command line, mapped to Builder methods.
STAGE_METHODS = {"packages": "package", "device": "device_image"}


def device_target(devices, name, arch):
    device = devices[name]
    if arch not in device["arches"]:
        raise ValueError(f"device {name} does not support {arch}")
    return device


def device_image_name(version, name, arch):
    return f"uflinux-{version}-{name}-{arch}.img.zst"


def device_uuids(seed):
    """Deterministic disk, partition and filesystem identifiers for one build."""
    def uuid(role):
        digest = hashlib.sha256(f"{seed}:{role}".encode()).hexdigest()[:32]
        return f"{digest[0:8]}-{digest[8:12]}-{digest[12:16]}-{digest[16:20]}-{digest[20:32]}".upper()
    serial = hashlib.sha256(f"{seed}:serial".encode()).hexdigest()[0:8].lower()
    return {"disk": uuid("disk"), "esp": uuid("esp"), "root": uuid("root"), "serial": serial}


def device_partitions(size_mib, bios=False):
    """ESP and root partition geometry in 512 byte sectors."""
    total = size_mib * (1024 * 1024 // SECTOR)
    if size_mib * 1024 * 1024 % SECTOR or total < ESP_SECTORS + 4096:
        raise ValueError(f"invalid image size {size_mib} MiB")
    bios_start = ESP_START + ESP_SECTORS
    root_start = bios_start + (2048 if bios else 0)
    return {"esp_start": ESP_START, "esp_size": ESP_SECTORS, "root_start": root_start,
            "root_size": total - root_start - ESP_START, "total": total, "bios_start": bios_start}


def device_partition_script(layout, uuids, bios=False):
    """Serialize the ESP, optional BIOS boot partition, and final root partition."""
    bios_entry = (f"start={layout['bios_start']}, size=2048, type={GPT_BIOS_TYPE}, name=BIOS\n"
                  if bios else "")
    return (
        "label: gpt\n"
        f"label-id: {uuids['disk']}\n"
        f"start={layout['esp_start']}, size={layout['esp_size']}, type={GPT_ESP_TYPE}, "
        f"uuid={uuids['esp']}, name=ESP\n"
        + bios_entry +
        f"start={layout['root_start']}, size={layout['root_size']}, type={GPT_LINUX_TYPE}, "
        f"uuid={uuids['root']}, name=ROOT\n"
    )


def device_repositories(source, arch):
    """Package sources for a device rootfs: the UFLinux repo, Alpine, then apps.

    *source* is the UFLinux repository line: the path of the copied signed
    repository while building, or the published URL in a shipped image. The
    pinned Nura suite is deliberately not listed here: apk solves against every
    repository in the file, and Nura's postmarketos-mkinitfs conflicts with the
    Alpine mkinitfs that uflinux-initramfs requires, so a visible Nura line
    makes any install that pulls in a kernel unresolvable. The profile reaches
    Nura through `apk fetch --repository` instead (device_nura_repository).
    The apps repository is listed, because uflinux-desktop-meta depends on
    uflinux-files, which only it carries. It indexes few architectures and apk 3
    fails the whole transaction on a line whose index 404s, so the line is
    omitted elsewhere.
    """
    alpine = LOCK["alpine"]
    lines = [source, f"{alpine['mirror']}/{alpine['branch']}/main",
             f"{alpine['mirror']}/{alpine['branch']}/community"]
    if arch in LOCK["apps"]["arches"]:
        lines.append(LOCK["apps"]["url"])
    if arch in kernels()["packages"]:
        lines.append(kernel_index(source, arch))
    return "\n".join(lines) + "\n"


def device_nura_repository():
    """The pinned Nura suite URL. apk appends the architecture itself."""
    nura = DEVICE_LOCK["nura"]
    return f"{nura['mirror']}/{nura['branch']}"


def browser_packages(arch):
    """The web browser set an architecture's image can actually install.

    Alpine builds firefox for every architecture we target except riscv64,
    where only the Extended Support Release is published. ublock-origin is a
    noarch xpi that installs into /usr/lib/firefox/browser/extensions, so it
    only rides along with firefox and not with firefox-esr.
    """
    if arch == "riscv64":
        return ["firefox-esr"]
    return ["firefox", "ublock-origin"]


FIRMWARE_PACKAGES = tuple(
    line for line in (PROJECT / "config/firmware-packages.txt").read_text().splitlines()
    if line and not line.startswith("#"))


def firmware_packages():
    """The Alpine firmware subpackages an image installs.

    Alpine's linux-firmware metapackage pins every vendor, so asking for it
    always drags in linux-firmware-qcom: 163 MiB of Snapdragon board blobs that
    no PC uses and that pushed the release ISO past GitHub's per-asset limit.
    Listing the subpackages keeps that space for the Arrow Dragon Q6A image,
    which names linux-firmware-qcom itself in config/devices.json.
    """
    return list(FIRMWARE_PACKAGES)


def device_packages(device, arch):
    """Everything the device rootfs needs beyond the live system's package set."""
    target = device["kernel"].format(arch=arch)
    item = kernels()["packages"][arch][target]
    kernel = "linux-uflinux-" + target
    version = item["name"].removeprefix(kernel + "-").removesuffix(".apk")
    packages = ["uflinux-desktop-meta", "uflinux-device", f"{kernel}={version}"] + list(device["packages"])
    # The same desktop the live ISO installs, minus the live-only pieces.
    packages += ["musl-locales", "musl-locales-lang", "glib-lang", "gtk4.0-lang",
                 "labwc-lang", "vulkan-loader", "xfsprogs"]
    packages += browser_packages(arch)
    packages += ["alpine-base", "musl", "musl-utils", "apk-tools", "zstd", "util-linux", "dosfstools", "mtools",
                 "e2fsprogs", "e2fsprogs-extra", "cloud-utils-growpart"]
    packages += firmware_packages()
    # f2fs-tools provides resize.f2fs, which the initramfs runs offline to
    # expand an F2FS root before mounting it (see the uflinux-f2fs-resize
    # feature), and keeps fsck.f2fs available for diagnosis.
    if device.get("rootfs", "ext4") == "f2fs":
        packages.append("f2fs-tools")
    if arch in ("x86_64", "aarch64") and "efibootmgr" not in packages:
        packages.append("efibootmgr")
    return sorted(set(packages))


def device_environment(name, device, arch, version, root_partuuid, esp):
    """Contract consumed by profiles/device-rootfs.sh."""
    env = {"UFLINUX_KERNEL_FLAVOR": "uflinux-" + device["kernel"].format(arch=arch),
           "UFLINUX_DEVICE": name, "UFLINUX_ARCH": arch,
           "UFLINUX_BOOT": device["boot"], "UFLINUX_SERIAL": device["serial"],
           "UFLINUX_INITRAMFS_COMPRESSION": device.get("initramfs_compression", "zstd"),
           "UFLINUX_CMDLINE": device["cmdline"], "UFLINUX_VERSION": version,
           "UFLINUX_ROOTFS": device.get("rootfs", "ext4"),
           "UFLINUX_ROOT_PARTUUID": root_partuuid, "UFLINUX_ESP": esp,
           "UFLINUX_PACKAGES": " ".join(device_packages(device, arch))}
    if name == "vm" and arch == "aarch64":
        env.update(UFLINUX_SERIAL="ttyAMA0", UFLINUX_CMDLINE="quiet console=ttyAMA0,115200n8")
    if device["nura"]:
        env["UFLINUX_NURA_PACKAGES"] = " ".join(device["nura_packages"])
        env["UFLINUX_NURA_REPOSITORY"] = device_nura_repository()
        # Keep the optional Nura driver on the ESP for firmware registration.
        # GRUB loads the board DTB directly from the kernel package.
        env["UFLINUX_DTBLOADER"] = "/usr/share/dtbloader/dtbloader.efi"
    # Firmware a built-in driver needs before the root is mounted has to be
    # named explicitly: mkinitfs collects firmware from the .ko files it copies
    # and a built-in driver has none. The profile fails the build if a listed
    # file is not installed, so a firmware repackaging cannot pass silently.
    if device.get("initramfs_files"):
        env["UFLINUX_INITRAMFS_FILES"] = " ".join(device["initramfs_files"])
    # Display-chain drivers the board needs before the root is mounted, so the
    # initramfs can bring up a console on machines with no built-in simpledrm.
    if device.get("initramfs_modules"):
        env["UFLINUX_INITRAMFS_MODULES"] = " ".join(device["initramfs_modules"])
    if device["boot"] == "extlinux":
        env["UFLINUX_DTB"] = device["dtb"]
    elif device.get("dtb"):
        env["UFLINUX_DEVICETREE"] = device["dtb"]
    # F2FS compression has to be requested by the kernel at mount time, so the
    # algorithm reaches the rootfs profile, which puts it in the kernel cmdline.
    if device.get("f2fs_compress"):
        env["UFLINUX_F2FS_COMPRESS"] = device["f2fs_compress"]
    return env


def device_snapshot(source, destination, *, rootfs=False):
    """Copy an installed rootfs into an image staging tree, minus build artefacts."""
    destination.mkdir(parents=True, exist_ok=True)
    excludes = [f"--exclude=./{name}" for name in DEVICE_ROOT_EXCLUDES]
    # The GPU screen recorder's KMS helper relies on a file capability.
    xattrs = ("--xattrs", "--xattrs-include=security.capability") if rootfs else ()
    reader = subprocess.Popen(("tar", "-C", str(source), *excludes, *xattrs, "-p", "-f", "-", "-c", "."),
                              stdout=subprocess.PIPE)
    writer = subprocess.Popen(("tar", "-C", str(destination), "--numeric-owner",
                               *xattrs, "-xpf", "-"), stdin=reader.stdout)
    reader.stdout.close()
    writer.communicate()
    reader.wait()
    if reader.returncode or writer.returncode:
        raise RuntimeError(f"failed to snapshot {source}")
    if rootfs:
        # Excluding build-time mounts also excludes their directories. Alpine
        # must move /dev, /proc and /sys into the new root before switch_root;
        # with a read-only root it cannot create missing destinations at boot.
        # Keep this out of ESP snapshots, which share the copying helper.
        for name in ("dev", "proc", "sys", "run"):
            mountpoint = destination / name
            mountpoint.mkdir(mode=0o755, parents=True, exist_ok=True)
            mountpoint.chmod(0o755)
    return destination


def device_extract(image, destination, esp_start=ESP_START):
    """Read the ESP of an assembled disk image without mounting anything."""
    destination.mkdir(parents=True, exist_ok=True)
    env = dict(os.environ, MTOOLS_SKIP_CHECK="1")
    # mtools parses a trailing "::" on the -i operand as a drive spec and then
    # fails with "non DOS media", so the offset form carries no colons and the
    # source is given as its own operand.
    run(["mcopy", "-s", "-n", "-i", f"{image}@@{esp_start}s", "::/", str(destination) + "/"],
        env=env)
    return destination


def device_layout(image):
    """Return the GPT geometry of *image* as reported by sfdisk."""
    result = run(["sfdisk", "--dump", "--json", str(image)], stdout=subprocess.PIPE, text=True)
    table = json.loads(result.stdout).get("partitiontable", {})
    if table.get("unit") != "sectors" or int(table.get("sectorsize", 0)) != SECTOR:
        raise ValueError(f"{image} is not a {SECTOR}-byte sector disk")
    partitions = table.get("partitions", [])
    if len(partitions) not in {2, 3}:
        raise ValueError(f"{image} must have an ESP and a root partition")
    esp, root = partitions[0], partitions[-1]
    if esp.get("type", "").upper() != GPT_ESP_TYPE or root.get("type", "").upper() != GPT_LINUX_TYPE:
        raise ValueError(f"{image} has the wrong partition types")
    return {"disk": table.get("id", "").upper(), "esp": esp, "root": root, "bios": partitions[1] if len(partitions) == 3 else None}


# Filesystem superblock signatures, as (offset, magic, description) triples
# measured from the start of the root partition.
ROOTFS_SIGNATURES = {
    "ext4": (1080, b"\x53\xef", "the ext4 superblock signature"),
    "f2fs": (1024, b"\x10\x20\xf5\xf2", "the F2FS superblock magic"),
    "xfs": (0, b"XFSB", "the XFS superblock magic"),
}

# F2FS incompat feature bitmap inside the superblock, and the two bits the
# kernel needs before it accepts compress_algorithm= on the mount line.
F2FS_FEATURES_OFFSET = 0xc84
F2FS_FEATURE_EXTRA_ATTR = 1 << 3
F2FS_FEATURE_COMPRESSION = 1 << 13


def device_verify_rootfs(image, root_start, rootfs):
    """Raise unless the root partition holds a *rootfs* superblock."""
    signature = ROOTFS_SIGNATURES[rootfs]
    with image.open("rb") as handle:
        handle.seek(root_start * SECTOR + signature[0])
        magic = handle.read(len(signature[1]))
    if magic != signature[1]:
        raise ValueError(f"{image}: the root partition is not {rootfs}: missing "
                         + signature[2])


def device_verify_f2fs_compression(image, root_start):
    """Raise unless the F2FS volume advertises the compression feature.

    dump.f2fs prints nothing about features, so the superblock is read directly.
    """
    with image.open("rb") as handle:
        handle.seek(root_start * SECTOR + F2FS_FEATURES_OFFSET)
        features = int.from_bytes(handle.read(4), "little")
    wanted = F2FS_FEATURE_EXTRA_ATTR | F2FS_FEATURE_COMPRESSION
    if features & wanted != wanted:
        raise ValueError(f"{image}: the F2FS root has no compression features "
                         f"(found {features:#x}, expected {wanted:#x} to be set)")


def device_write_rootfs(rootfs, disk, snapshot, staging, layout, uuids, epoch,
                        f2fs_compress=None):
    """Format an f2fs or XFS root volume, fill it from *staging* and dd it onto *disk*.

    Unlike ext4, neither mkfs can write the tree into the disk image at a
    partition offset, so the volume is built as a standalone file first. The
    file is exactly one partition long and the ESP offset is MiB-aligned, so a
    single aligned dd places it at its final sector.
    """
    volume = snapshot / ("root." + rootfs)
    run(["truncate", "-s", str(layout["root_size"] * SECTOR), volume])
    if rootfs == "f2fs":
        mkfs = ["mkfs.f2fs", "-q", "-f", "-l", "UFLINUX_ROOT", "-U", uuids["root"].lower(),
                "-R", "0:0", "-t", "0", "-T", epoch]
        if f2fs_compress:
            # Alpine's sload.f2fs is built without compression support, so the
            # seeded tree is written uncompressed and the kernel compresses what
            # it writes from then on. Enabling the on-disk feature here is what
            # lets the mount option in the kernel cmdline take effect; the
            # kernel refuses it on a volume formatted without these bits.
            mkfs += ["-O", "compression,extra_attr"]
        mkfs += [str(volume), str(layout["root_size"])]
        run(mkfs)
        # sload prints one line per directory; the log only needs its failures.
        run(["sload.f2fs", "-f", staging, "-t", "/", "-T", epoch, "-P", str(volume)],
            stdout=subprocess.DEVNULL)
    elif rootfs == "xfs":
        # -p populates the root inode straight from the staging tree, keeping
        # content, modes, ownership, timestamps and xattrs for every file type.
        run(["mkfs.xfs", "-q", "-f", "-m", "uuid=" + uuids["root"].lower(),
             "-L", "UFLINUX_ROOT", "-p", str(staging), str(volume)])
    else:
        raise ValueError(f"unsupported root filesystem {rootfs}")
    run(["dd", f"if={volume}", f"of={disk}", "bs=1M",
         f"seek={layout['root_start'] * SECTOR // (1024 * 1024)}",
         "conv=notrunc", "status=none"])
    volume.unlink()


def device_verify_image(image, device, arch, uuids, layout):
    """Check the assembled disk, including the board's initramfs compression."""
    rootfs = device.get("rootfs", "ext4")
    if rootfs not in ROOTFS_SIGNATURES:
        raise ValueError(f"{image}: unknown root filesystem {rootfs}")
    compression = device.get("initramfs_compression", "zstd")
    signatures = {"gzip": (b"\x1f\x8b\x08", "gzip stream"),
                  "zstd": (b"\x28\xb5\x2f\xfd", "Zstandard frame")}
    if compression not in signatures:
        raise ValueError(f"{image}: unsupported initramfs compression {compression}")
    actual = device_layout(image)
    for name, expected in (("ESP", layout["esp_start"]), ("ROOT", layout["root_start"])):
        found = (actual["esp"] if name == "ESP" else actual["root"])["start"]
        if int(found) != expected:
            raise ValueError(f"{image}: {name} starts at sector {found}, expected {expected}")
    for key, role in (("esp", "ESP"), ("root", "ROOT")):
        if actual[key].get("uuid", "").upper() != uuids[key]:
            raise ValueError(f"{image}: {role} partition UUID changed")
    if actual["disk"] != uuids["disk"]:
        raise ValueError(f"{image}: disk GUID changed")
    device_verify_rootfs(image, layout["root_start"], rootfs)
    if device.get("f2fs_compress"):
        device_verify_f2fs_compression(image, layout["root_start"])
    stage = image.parent / (image.name + "-esp")
    device_extract(image, stage)
    boot = {}
    for name in ("boot/vmlinuz", "boot/initramfs"):
        path = stage / name
        if not path.is_file():
            raise ValueError(f"{image}: the ESP has no {name}")
        boot[name.split("/", 1)[1] + "_bytes"] = path.stat().st_size
    with (stage / "boot/initramfs").open("rb") as handle:
        magic = handle.read(4)
    signature, description = signatures[compression]
    if not magic.startswith(signature):
        raise ValueError(f"{image}: the initramfs is not a {description}")
    if device["boot"] in {"efi", "pc"}:
        bootloader = "boot/grub/grub.cfg"
        efi = stage / "EFI/BOOT" / ("BOOT" + EFI_SUFFIX[arch] + ".EFI")
        if not efi.is_file():
            raise ValueError(f"{image}: missing {efi.name}")
        with efi.open("rb") as handle:
            if handle.read(2) != b"MZ":
                raise ValueError(f"{image}: invalid GRUB EFI executable {efi.name}")
        if device.get("dtb") and not (stage / "dtbs" / device["dtb"]).is_file():
            raise ValueError(f"{image}: missing the EFI device tree")
        if device["boot"] == "pc":
            bios = actual["bios"]
            if not bios or bios.get("type", "").upper() != GPT_BIOS_TYPE:
                raise ValueError(f"{image}: missing GRUB BIOS boot partition")
            with image.open("rb") as handle:
                if not any(handle.read(440)):
                    raise ValueError(f"{image}: missing GRUB MBR code")
                handle.seek(bios["start"] * SECTOR)
                if not any(handle.read(4096)):
                    raise ValueError(f"{image}: missing embedded GRUB core")
    elif device["boot"] == "rpi":
        bootloader = "cmdline.txt"
        if not (stage / bootloader).is_file():
            raise ValueError(f"{image}: the ESP has no {bootloader}")
        for name in ("kernel8.img", "overlays", "bcm2710-rpi-3-b.dtb",
                     "bcm2711-rpi-4-b.dtb", "bcm2712-rpi-5-b.dtb"):
            if not (stage / name).exists():
                raise ValueError(f"{image}: the Raspberry Pi boot partition lacks {name}")
    elif device["boot"] == "extlinux":
        bootloader = "extlinux/extlinux.conf"
        for dtb in (device["dtb"], "ky/x1_orangepi-r2s.dtb"):
            if not (stage / "dtbs" / dtb).is_file():
                raise ValueError(f"{image}: missing extlinux device tree {dtb}")
    else:
        raise ValueError(f"{image}: unsupported boot mode {device['boot']}")
    if not (stage / bootloader).is_file():
        raise ValueError(f"{image}: the ESP has no {bootloader}")
    remove_tree(stage)
    return {"device_image_format": "gpt-" + rootfs, "initramfs_compression": compression,
            "arch": arch, "boot": device["boot"], "disk_guid": actual["disk"],
            "partitions": {"esp": {"start": layout["esp_start"], "size": layout["esp_size"],
                                   "uuid": uuids["esp"]},
                           "root": {"start": layout["root_start"], "size": layout["root_size"],
                                    "uuid": uuids["root"]}},
            **boot}
# apk v2 has no ECDSA signature: apk-tools 3.0.9 knows only the stream names
# RSA512, RSA256, RSA and DSA, and resolves a trusted key by its filename from a
# plain public-key PEM (an X.509 certificate is not accepted). UFLinux therefore
# signs with an EC P-256 or P-384 key carried in the SHA-256 stream, whose name
# is fixed by that format limitation rather than by the algorithm actually used.
SIGN_STREAM = "RSA256"
SIGNING_KEY = "uflinux.ec"
SIGNING_PRIVATE = "/var/lib/uflinux/keys/" + SIGNING_KEY
SIGNING_PUBLIC = SIGNING_PRIVATE + ".pub"
TRUSTED_KEY = "etc/apk/keys/" + SIGNING_KEY + ".pub"
BUILD_KEY = "build.ec.pub"
# The apps repository is signed by a separate key of the same role, published
# under this file name; apk trusts a key by filename, so it must be installed
# under exactly this name.
APPS_KEY = "uflinux3.ec384.pub"
APPS_TRUSTED_KEY = "etc/apk/keys/" + APPS_KEY
APPS_BUNDLED_KEY = PROJECT / ("aports/uflinux/" + APPS_KEY)
MUSL_BOLT_ARCHES = {"x86_64", "aarch64", "riscv64"}
CLANG_APORT_BOLT_ARCHES = {"x86_64", "aarch64"}
CLANG_APORTS = {"connman", "gpu-screen-recorder"}
CLANG_APORT_PROFILES = "clang-aport-profiles.tar.gz"
PACKAGE_NAMES = {
    "musl": {"musl", "musl-dev", "musl-dbg", "musl-utils", "musl-libintl"},
    "connman": {"connman", "connman-dbg", "connman-dev", "connman-doc", "connman-openrc",
                "connman-vpn", "connman-vpn-openrc"},
    "uflinux-regreet": {"uflinux-regreet"},
    "xdg-desktop-portal-gtk4": {"xdg-desktop-portal-gtk4"},
    "gpu-screen-recorder": {"gpu-screen-recorder", "gpu-screen-recorder-dbg",
                            "gpu-screen-recorder-dev", "gpu-screen-recorder-doc"},
    "uflinux-settings": {"uflinux-settings", "uflinux-settings-daemon", "uflinux-settings-dbg",
                         "uflinux-settings-doc", "uflinux-settings-system", "uflinux-settings-system-openrc", "uflinux-installer-ui", "uflinux-desktop", "uflinux-screencapture"},
    "uflinux-rds": {"uflinux-rds", "uflinux-rds-dbg", "uflinux-rds-doc", "uflinux-rds-server",
                    "uflinux-rds-web", "uflinux-rds-system", "uflinux-rds-vaapi"},
    "uflinux": {"uflinux", "uflinux-defaults", "uflinux-desktop-meta", "uflinux-installer",
                "uflinux-live", "uflinux-live-openrc", "uflinux-release", "uflinux-bootsplash-openrc",
                "uflinux-initramfs", "uflinux-device", "uflinux-zram"},
    "simp1e-cursors": {"simp1e-cursors", "simp1e-cursors-adw-dark"},
}
CARGO_ORIGINS = {"uflinux-regreet", "uflinux-settings", "uflinux-rds", "xdg-desktop-portal-gtk4"}


def package_version(origin, distribution_version):
    if origin == "uflinux":
        return distribution_version
    if origin == "uflinux-settings":
        return LOCK["settings"]["version"]
    return LOCK[origin]["version"]


def package_revision(origin):
    recipe = (PROJECT / "aports" / origin / "APKBUILD").read_text()
    return re.search(r"^pkgrel=([0-9]+)$", recipe, re.M)[1]


def stage_musl_aport(archive, destination):
    """Supply the recipe's unchanged Alpine files from the pinned aports archive."""
    recipe = (destination / "APKBUILD").read_text()
    checksums = re.search(r'^sha512sums="([^"]+)"', recipe, re.M)[1]
    names = {line.split()[1] for line in checksums.splitlines() if line.strip()}
    missing = {name for name in names if not name.endswith(".tar.gz") and not (destination / name).is_file()}
    with tarfile.open(archive) as source:
        for member in source:
            parts = Path(member.name).parts
            if len(parts) == 4 and parts[1:3] == ("main", "musl") and parts[-1] in missing and member.isfile():
                member.name = parts[-1]
                source.extract(member, destination, filter="data")
                missing.remove(parts[-1])
                if not missing:
                    break
    if missing:
        raise RuntimeError("Pinned Alpine aports is missing musl sources: " + ", ".join(sorted(missing)))


# Custom packages that are published to the signed repository but are not part of
# the live ISO, so they cannot appear in the ISO's SPDX inventory. A device disk
# image installs them from the published repository instead (device_packages).
ISO_EXCLUDED_PACKAGES = {"uflinux-device"}


def omit_modloop_firmware(update_kernel):
    """Adjust the image-only helper after its existing compression patch."""
    marker = "# Refuse an image that its selected kernel cannot decompress."
    if update_kernel.count(marker) != 1:
        raise ValueError("Unexpected update-kernel compression section")
    cleanup = r'''# Firmware is supplied by the live root's linux-firmware packages. Preserve
# wireless-regdb, which is a separate package and not in linux-firmware.
if [ -d "$MODLOOP/modules/firmware" ]; then
    find "$MODLOOP/modules/firmware" -mindepth 1 -maxdepth 1 \
        ! -name 'regulatory.db*' -exec rm -rf {} +
fi

'''
    return update_kernel.replace(marker, cleanup + marker, 1)


def check_modloop_firmware_listing(listing):
    """Only the separately packaged wireless regulatory database may remain."""
    prefix = "squashfs-root/modules/firmware/"
    for line in listing.splitlines():
        if prefix not in line:
            continue
        relative = line.split(prefix, 1)[1].split(" -> ", 1)[0]
        if "/" in relative or not relative.startswith("regulatory.db"):
            raise ValueError("Device firmware remains in the modloop: " + relative)


def image_compression(initramfs, modloop):
    with Path(initramfs).open("rb") as stream:
        if stream.read(4) != b"\x28\xb5\x2f\xfd":
            raise ValueError("Initramfs is not a Zstandard frame")
    with Path(modloop).open("rb") as stream:
        header = stream.read(32)
    if len(header) < 32 or header[:4] != b"hsqs":
        raise ValueError("Modloop is not a SquashFS filesystem")
    if struct.unpack_from("<H", header, 20)[0] != 6:
        raise ValueError("Modloop does not use Zstandard compression")
    if struct.unpack_from("<I", header, 12)[0] != LOCK["image"]["modloop_block_bytes"]:
        raise ValueError("Modloop block size differs from the image lock")
    return {"initramfs_compression": "zstd", "modloop_compression": "zstd",
            "modloop_block_bytes": LOCK["image"]["modloop_block_bytes"],
            "initramfs_bytes": Path(initramfs).stat().st_size,
            "modloop_bytes": Path(modloop).stat().st_size}


def package_output(out, arch, package=None):
    out = Path(out).resolve()
    return out / "package-shards" / arch / package if package else out



def run(args, **kwargs):
    print("+", shlex.join(str(a) for a in args), flush=True)
    return subprocess.run([str(a) for a in args], check=True, **kwargs)


def sha(path, algorithm="sha256"):
    digest = hashlib.new(algorithm)
    with Path(path).open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def check_asset_size(path, kind, warn=True):
    """Return a release asset's size, naming it if GitHub would reject it."""
    path = Path(path)
    size = path.stat().st_size
    if size >= RELEASE_ASSET_LIMIT:
        raise ValueError(f"{kind} {path.name} is {size} bytes, at or above GitHub "
                         f"Releases' {RELEASE_ASSET_LIMIT}-byte per-file limit")
    if warn and size >= RELEASE_ASSET_WARNING:
        # A workflow command, so the next package update that pushes an ISO over
        # the line shows up as annotation instead of a failed release.
        print(f"::warning title=Release asset size::{path.name} is {size} bytes, "
              f"within {RELEASE_ASSET_LIMIT - size} of the per-file limit")
    return size


def link_or_copy(source, destination):
    destination = Path(destination)
    destination.unlink(missing_ok=True)
    try:
        os.link(source, destination)
    except OSError:
        shutil.copy2(source, destination)


def mounted_paths():
    result = []
    for line in Path("/proc/self/mountinfo").read_text().splitlines():
        field = line.split()[4]
        result.append(Path(re.sub(r"\\([0-7]{3})", lambda match: chr(int(match[1], 8)), field)))
    return result


def remove_tree(path):
    """Never traverse a build directory that still contains bind mounts."""
    path = Path(path)
    if path.is_symlink():
        path.unlink()
        return
    if not path.exists():
        return
    resolved = path.resolve()
    if any(mount == resolved or mount.is_relative_to(resolved) for mount in mounted_paths()):
        raise RuntimeError("Refusing to remove a directory with active mounts: " + str(path))
    shutil.rmtree(path)


@contextlib.contextmanager
def temporary_tree(**kwargs):
    path = Path(tempfile.mkdtemp(**kwargs))
    try:
        yield path
    finally:
        remove_tree(path)


def archive_tree(source, destination, epoch, names=None):
    destination.parent.mkdir(parents=True, exist_ok=True)
    command = ["tar", "--sort=name", "--mtime=@" + str(epoch), "--owner=0", "--group=0", "--numeric-owner",
               "-C", str(source), "-cf", "-", "--", *(names or ["."])]
    tar = subprocess.Popen(command, stdout=subprocess.PIPE)
    try:
        with destination.open("wb") as stream:
            subprocess.run(["gzip", "-9n"], stdin=tar.stdout, stdout=stream, check=True)
    finally:
        tar.stdout.close()
    if tar.wait():
        raise RuntimeError("Failed to archive " + str(source))


def download(spec, cache):
    algorithm = "sha512" if "sha512" in spec else "sha256"
    expected = spec[algorithm]
    return download_pinned(spec["url"], expected, algorithm, cache / (expected + ".tar.gz"))


def git_source_archive(spec, prefix, cache):
    """Fetch a locked Git commit and retain the exact archive used by abuild."""
    commit, expected = spec["commit"], spec["sha512"]
    cache.mkdir(parents=True, exist_ok=True)
    destination = cache / (expected + ".tar.gz")
    if not destination.exists():
        with tempfile.TemporaryDirectory(prefix="git-source-", dir=cache) as directory:
            root = Path(directory)
            repository = root / "repo"
            run(["git", "init", "--bare", repository])
            git = ["git", "-C", repository]
            run([*git, "fetch", "--depth=1", "--no-tags", spec["git"], commit])
            resolved = run([*git, "rev-parse", "FETCH_HEAD^{commit}"], capture_output=True, text=True).stdout.strip()
            if resolved != commit:
                raise RuntimeError(f"Git source resolved to {resolved}, expected {commit}")
            archive = root / "source.tar"
            compressed = root / "source.tar.gz"
            with archive.open("wb") as stream:
                run([*git, "-c", "tar.umask=0002", "archive", "--format=tar",
                     "--prefix=" + prefix + "/", commit], stdout=stream)
            with compressed.open("wb") as stream:
                run(["gzip", "-n", "-9", "-c", archive], stdout=stream)
            if sha(compressed, "sha512") != expected:
                raise RuntimeError("Git source archive checksum mismatch: " + spec["git"])
            compressed.replace(destination)
    if sha(destination, "sha512") != expected:
        raise RuntimeError("Corrupt cached Git source: " + str(destination))
    return destination


def download_pinned(url, expected, algorithm, path):
    """Fetch one file into *path*, accepting an already cached copy only if it verifies."""
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    if not path.exists():
        temporary = path.with_suffix(".partial")
        for attempt in range(3):
            try:
                with urllib.request.urlopen(url, timeout=120) as response, temporary.open("wb") as stream:
                    shutil.copyfileobj(response, stream)
                if sha(temporary, algorithm) != expected:
                    raise RuntimeError("Checksum mismatch: " + url)
                temporary.replace(path)
                break
            except OSError:
                temporary.unlink(missing_ok=True)
                if attempt == 2:
                    raise
                time.sleep(2)
    if sha(path, algorithm) != expected:
        raise RuntimeError("Corrupt cached source: " + str(path))
    return path


def gzip_members(data):
    """APK v2 signatures, control, and data are distinct gzip streams."""
    while data:
        decoder = zlib.decompressobj(31)
        raw = decoder.decompress(data)
        if not decoder.eof:
            raise ValueError("Truncated APK gzip member")
        consumed = len(data) - len(decoder.unused_data)
        yield data[:consumed], raw
        data = decoder.unused_data


def gzip_file_members(path):
    """Read APK control streams without loading or expanding the data payload."""
    with Path(path).open("rb") as stream:
        pending = stream.read(65536)
        while pending:
            decoder = zlib.decompressobj(31)
            compressed, raw = bytearray(), bytearray()
            while not decoder.eof:
                chunk = pending or stream.read(65536)
                if not chunk:
                    raise ValueError("Truncated APK gzip member")
                raw.extend(decoder.decompress(chunk))
                used = len(chunk) - len(decoder.unused_data)
                compressed.extend(chunk[:used])
                pending = decoder.unused_data
            yield bytes(compressed), bytes(raw)
            if not pending:
                pending = stream.read(65536)


def promote_apk(path, cache):
    """Recover APKv2 cache names from the first eight hex digits of the control SHA-1."""
    with path.open("rb") as stream:
        if stream.read(4) in (b"ADB.", b"ADBd", b"ADBc"):
            # APKv3 packages use their own index and remain in the image input archive.
            return
    for compressed, raw in gzip_file_members(path):
        with tarfile.open(fileobj=io.BytesIO(raw)) as archive:
            if ".PKGINFO" not in archive.getnames():
                continue
        suffix = hashlib.sha1(compressed).hexdigest()[:8]
        name = re.sub(r"\.[0-9a-f]{8}$", "", path.stem) + "." + suffix + ".apk"
        target = cache / name
        if not target.exists():
            link_or_copy(path, target)
        return
    raise ValueError("No APK control stream: " + str(path))


def split_apk(path):
    members = list(gzip_members(Path(path).read_bytes()))
    control = None
    for index, (_, raw) in enumerate(members):
        with tarfile.open(fileobj=io.BytesIO(raw)) as archive:
            names = archive.getnames()
        if ".PKGINFO" in names:
            control = index
            break
        if not names or any(not name.startswith(".SIGN.") for name in names):
            raise ValueError("Unexpected APK member before control archive")
    if control is None or control + 2 != len(members):
        raise ValueError("Expected APK v2 signature/control/data streams")
    return members[control][0], members[control + 1][0]


def unsigned_digest(path):
    control, data = split_apk(path)
    return hashlib.sha256(gzip.decompress(control) + gzip.decompress(data)).hexdigest()


def signature_names(path):
    """The .SIGN.<stream>.<key> names carried by an APK, in file order."""
    names = []
    for _, raw in gzip_members(Path(path).read_bytes()):
        with tarfile.open(fileobj=io.BytesIO(raw)) as archive:
            names.extend(name for name in archive.getnames() if name.startswith(".SIGN."))
    return names


# The distribution repository is published on GitHub Pages with a custom
# domain; unless APK_REPOSITORY_URL overrides it (e.g. a fork on github.io),
# images point here by default.
DEFAULT_REPOSITORY_HOST = "repo.uflinux.org"


def repository_url(value):
    if not value:
        repo = os.environ.get("GITHUB_REPOSITORY", "")
        if repo and repo.split("/", 1)[0].lower() != "uflinux-project":
            owner, name = repo.split("/", 1)
            suffix = "" if name.lower() == owner.lower() + ".github.io" else "/" + name
            value = f"https://{owner.lower()}.github.io{suffix}/apk/{LOCK['alpine']['branch']}/uflinux"
        else:
            value = f"https://{DEFAULT_REPOSITORY_HOST}/apk/{LOCK['alpine']['branch']}/uflinux"
    parsed = urllib.parse.urlsplit(value)
    if parsed.scheme != "https" or not parsed.netloc or parsed.username is not None or parsed.password is not None or parsed.query or parsed.fragment:
        raise ValueError("APK_REPOSITORY_URL must be an HTTPS directory URL")
    if any(c.isspace() for c in value) or any(c in value for c in "'\"\\`$"):
        raise ValueError("Unsafe repository URL")
    return value.rstrip("/")


class Builder:
    def __init__(self, args):
        self.args = args
        self.root = Path(args.workdir).resolve() / args.arch / "root"
        self.cache = Path(args.cache).resolve()
        self.out = package_output(args.outdir, args.arch, getattr(args, "package", None))
        self.packages_dir = self.out / "packages" / args.arch
        self.epoch = str(os.environ.get("SOURCE_DATE_EPOCH", LOCK["source_date_epoch"]))
        self.url = repository_url(args.repository_url)
        self.mounts = []
        self.env = {"PATH": "/usr/sbin:/usr/bin:/sbin:/bin", "HOME": "/root", "TERM": "dumb",
                    "SOURCE_DATE_EPOCH": self.epoch, "TZ": "UTC", "LC_ALL": "C.UTF-8",
                    "UFLINUX_REPOSITORY_URL": self.url, "UFLINUX_APK_CACHE": "/cache/image-apk",
                    "sigtype": SIGN_STREAM, "ARCH": args.arch}
        github_repository = os.environ.get("GITHUB_REPOSITORY", "")
        if re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", github_repository):
            self.env["UFLINUX_PROJECT_URL"] = "https://github.com/" + github_repository

    def chroot(self, *command, **kwargs):
        env = dict(self.env, **kwargs.pop("extra_env", {}))
        return run(["chroot", self.root, "/usr/bin/env", "-i", *[f"{k}={v}" for k, v in env.items()], *command], **kwargs)

    def put(self, name, content, mode=0o644):
        path = self.root / name.lstrip("/")
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(content)
        path.chmod(mode)
        return path

    def bind(self, source, destination):
        path = self.root / destination.lstrip("/")
        if Path(source).is_dir():
            path.mkdir(parents=True, exist_ok=True)
        else:
            path.parent.mkdir(parents=True, exist_ok=True)
            path.touch(exist_ok=True)
        run(["mount", "--bind", source, path])
        self.mounts.append(path)

    def bootstrap(self):
        fingerprint = hashlib.sha256(json.dumps(LOCK, sort_keys=True).encode()).hexdigest()
        marker = self.root / ".uflinux-bootstrap"
        if marker.exists() and marker.read_text() != fingerprint:
            raise RuntimeError("Build inputs changed: use a new --workdir")
        if not marker.exists():
            if self.root.exists() and any(self.root.iterdir()):
                raise RuntimeError("Incomplete rootfs: remove only this build's workdir and retry")
            archive = download(LOCK["rootfs"][self.args.arch], self.cache / "downloads")
            self.root.mkdir(parents=True, exist_ok=True)
            run(["tar", "--numeric-owner", "-xpf", archive, "-C", self.root])
            marker.write_text(fingerprint)
        resolv = self.root / "etc/resolv.conf"
        resolv.unlink(missing_ok=True)
        resolv.write_bytes(Path("/etc/resolv.conf").read_bytes())
        self.put("etc/apk/repositories", "\n".join(f"{LOCK['alpine']['mirror']}/{LOCK['alpine']['branch']}/{repo}" for repo in ("main", "community")) + "\n")
        # Executing apk is also a check of the kernel's binfmt registration.
        self.chroot("apk", "--print-arch")
        for name in PROJECT_DIRS:
            destination = self.root / "project" / name
            if destination.exists():
                shutil.rmtree(destination)
            shutil.copytree(PROJECT / name, destination, ignore=shutil.ignore_patterns("__pycache__", "*.pyc"))
        (self.root / "project/LICENSE").write_bytes((PROJECT / "LICENSE").read_bytes())
        self.put("project/repository-url", self.url + "\n")

    @contextlib.contextmanager
    def mounted(self):
        try:
            self.bind("/dev", "dev")
            # Do not propagate nested device binds into the host's /dev or other
            # build roots when the runner uses shared mount propagation.
            run(["mount", "--make-private", self.root / "dev"])
            # Container runtimes may bind these nodes individually over placeholders.
            for device in ("null", "zero", "full", "random", "urandom", "tty"):
                self.bind("/dev/" + device, "dev/" + device)
            self.bind("/dev/pts", "dev/pts")
            self.bind("/dev/shm", "dev/shm")
            # Builds do not need access to the host's sysfs/device topology.
            (self.root / "proc").mkdir(exist_ok=True)
            run(["mount", "-t", "proc", "proc", self.root / "proc"])
            self.mounts.append(self.root / "proc")
            for name in ("apk", "cargo", "target", "image-apk"):
                host = self.cache / self.args.arch / name
                host.mkdir(parents=True, exist_ok=True)
                self.bind(host, "cache/" + name)
            cache_link = self.root / "etc/apk/cache"
            if not cache_link.exists() and not cache_link.is_symlink():
                cache_link.symlink_to("/cache/apk")
            yield
        finally:
            for path in reversed(self.mounts):
                result = subprocess.run(["umount", str(path)], check=False)
                if result.returncode:
                    subprocess.run(["umount", "--lazy", str(path)], check=False)
            self.mounts.clear()

    def tools(self, *packages):
        self.chroot("apk", "add", "--no-progress", "abuild", "alpine-conf", "alpine-keys", "openssl", "tar", "gzip", "python3", *packages)

    def signing_key(self, release=False, owner=0):
        directory = self.root / "var/lib/uflinux/keys"
        directory.mkdir(parents=True, exist_ok=True, mode=0o700)
        private = directory / SIGNING_KEY
        public = directory / (SIGNING_KEY + ".pub")
        if release:
            raw = os.environ.get("APK_SIGNING_PRIVATE_KEY", "")
            expected = os.environ.get("APK_SIGNING_PUBLIC_KEY", "").replace("\r\n", "\n").strip()
            if not raw or not expected:
                raise RuntimeError("Release signing requires APK_SIGNING_PRIVATE_KEY and APK_SIGNING_PUBLIC_KEY PEM secrets")
            # Never overwrite a compilation key's inode: an unprivileged child
            # could still hold an open descriptor to that old development key.
            os.chown(directory, 0, 0)
            directory.chmod(0o700)
            private.unlink(missing_ok=True)
            fd = os.open(private, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
            with os.fdopen(fd, "w") as stream:
                stream.write(raw.strip() + "\n")
            actual = self.chroot("openssl", "pkey", "-in", SIGNING_PRIVATE, "-pubout", stdout=subprocess.PIPE).stdout.decode().strip()
            if actual != expected:
                raise RuntimeError("Release public and private signing keys do not match")
            # apk resolves a trusted key by filename alone, so a key published as
            # uflinux.ec.pub has to be the algorithm that name advertises. Both
            # NIST prime curves are carried unchanged in the SHA-256 stream.
            described = self.chroot("openssl", "pkey", "-in", SIGNING_PRIVATE,
                                    "-text_pub", "-noout", stdout=subprocess.PIPE).stdout.decode()
            if "ASN1 OID: prime256v1" not in described and "ASN1 OID: secp384r1" not in described:
                raise RuntimeError("The release signing key is not an EC P-256 or P-384 key")
            public.write_text(expected + "\n")
        elif not private.exists():
            self.chroot("openssl", "genpkey", "-algorithm", "EC",
                        "-pkeyopt", "ec_paramgen_curve:P-256", "-out", SIGNING_PRIVATE)
            result = self.chroot("openssl", "pkey", "-in", SIGNING_PRIVATE, "-pubout", stdout=subprocess.PIPE)
            public.write_bytes(result.stdout)
        for path in (directory, private, public):
            os.chown(path, owner, owner)
        private.chmod(0o600)
        public.chmod(0o644)
        shutil.copy2(public, self.root / TRUSTED_KEY)
        self.env.update(PACKAGER_PRIVKEY=SIGNING_PRIVATE, PACKAGER_PUBKEY=SIGNING_PUBLIC)
        return public

    def build_origin(self, origin, env):
        # QEMU user emulation cannot elevate through a target setuid binary.
        # Resolve dependencies as root, but run every compilation/check/package
        # hook unprivileged. Plain abuild still checks that dependencies exist.
        directory = f"cd /project/aports/{origin} && "
        if origin == "musl":
            env = dict(env, SOURCE_DATE_EPOCH=str(LOCK["musl"]["source_date_epoch"]))
        if origin in CLANG_APORTS:
            env = dict(env, UFLINUX_APORT_PROFILES=f"/home/builder/aport-profiles/{origin}")
        dependency_env = dict(env, SUDO_APK="apk", REPODEST="/home/builder/packages")
        self.chroot("sh", "-ec", directory + "abuild -F deps", extra_env=dependency_env)
        try:
            if origin == "musl" and self.args.arch in MUSL_BOLT_ARCHES:
                self.chroot("su", "-s", "/bin/sh", "builder", "-c",
                            directory + "abuild validate clean unpack prepare build", extra_env=env)
                build = self.root / "project/aports/musl/src" / ("musl-" + LOCK["musl"]["upstream_version"])
                profile = self.root / "tmp/musl-bolt-profile"
                remove_tree(profile)
                replay = getattr(self.args, "musl_bolt_profiles", None)
                if not replay and not getattr(self.args, "refresh_musl_bolt_profile", False):
                    pinned = PROJECT / "profiles/musl-bolt" / (self.args.arch + ".tar.gz")
                    if pinned.is_file():
                        replay = pinned
                if replay:
                    profile.mkdir()
                    with tarfile.open(replay, "r:gz") as archive:
                        archive.extractall(profile, filter="data")
                run([sys.executable, PROJECT / "scripts/musl-bolt.py", self.args.arch,
                     self.root, build, profile, *(["--replay"] if replay else [])])
                relative = "/" + build.relative_to(self.root).as_posix()
                self.chroot("su", "-s", "/bin/sh", "builder", "-c",
                            directory + f"{relative}/lib/libc.bolt.so {relative}/allocator-test && abuild rootpkg",
                            extra_env=env)
            else:
                self.chroot("su", "-s", "/bin/sh", "builder", "-c",
                            directory + "abuild", extra_env=env)
        finally:
            self.chroot("sh", "-ec", directory + "abuild -F undeps", extra_env=dependency_env)

    def build_origins(self, selected, env):
        try:
            for origin in selected:
                self.build_origin(origin, env)
        finally:
            # abuild writes the shared caches as uid 1000 with mode 640, so the host
            # runner cannot read them back when Actions saves or copies the cache.
            self.chroot("sh", "-ec", "chmod -R a+rX /cache/cargo /cache/target")

    def build_clang_aport_bolt(self):
        spec = LOCK["sources"]["llvm-bolt"]
        archive = download(spec, self.cache / "downloads")
        staged = "/var/cache/distfiles/" + Path(spec["url"]).name
        shutil.copyfile(archive, self.root / staged.lstrip("/"))
        work = "/home/builder/aport-bolt"
        self.chroot("install", "-d", "-o", "builder", "-g", "builder", work)
        self.chroot("su", "-s", "/bin/sh", "builder", "-c",
                    f"tar -xf {staged} -C {work} && "
                    f"sh /project/settings/scripts/build-bolt.sh {work}",
                    extra_env={"JOBS": str(ARCHES[self.args.arch]["jobs"])})
        return work + "/pgo-tools/bin"

    def package(self):
        self.tools("git", "shadow", "sudo", "alpine-sdk")
        self.chroot("sh", "-ec", "id builder >/dev/null 2>&1 || adduser -D -u 1000 builder; addgroup builder abuild")
        self.signing_key(owner=1000)
        source = self.root / "project/aports"
        # Only stage the Alpine port for builds that include musl.
        selected = (self.args.package,) if getattr(self.args, "package", None) else tuple(PACKAGE_NAMES)
        if "musl" in selected:
            archive = download(LOCK["sources"]["aports"], self.cache / "downloads")
            stage_musl_aport(archive, source / "musl")
        for path in (source, *source.rglob("*")):
            os.chown(path, 1000, 1000)
        # Restored Actions caches belong to the host runner, not uid 1000 here.
        self.chroot("chown", "-R", "builder:builder", "/cache/cargo", "/cache/target")
        self.chroot("install", "-d", "-o", "builder", "-g", "builder", "/home/builder/packages", "/home/builder/.abuild")
        self.put("home/builder/.abuild/abuild.conf", 'PACKAGER="UFLinux contributors <noreply@example.invalid>"\nPACKAGER_PRIVKEY="' + SIGNING_PRIVATE + '"\nPACKAGER_PUBKEY="' + SIGNING_PUBLIC + '"\n')
        apkbuild = source / "uflinux/APKBUILD"
        apkbuild.write_text(re.sub(r"^pkgver=.*$", "pkgver=" + self.args.version, apkbuild.read_text(), flags=re.M))
        # Only fetch the selected origin's sources in a package shard.
        for origin in ("connman", "uflinux-regreet", "gpu-screen-recorder", "xdg-desktop-portal-gtk4"):
            if origin in selected:
                archive = download(LOCK["sources"][origin], self.cache / "downloads")
                suffix = "tar.xz" if origin == "connman" else "tar.gz"
                shutil.copy2(archive, self.root / "var/cache/distfiles" / f"{origin}-{LOCK[origin]['version']}.{suffix}")
        if "musl" in selected:
            archive = download(LOCK["sources"]["musl"], self.cache / "downloads")
            shutil.copy2(archive, self.root / "var/cache/distfiles" /
                         f"musl-{LOCK['musl']['upstream_version']}.tar.gz")
            mimalloc = "mimalloc-" + LOCK["musl"]["mimalloc_version"]
            archive = git_source_archive(LOCK["sources"]["mimalloc"], mimalloc, self.cache / "downloads")
            shutil.copy2(archive, source / "musl" / (mimalloc + ".tar.gz"))
            if self.args.arch in {"x86_64", "aarch64"}:
                archive = download(LOCK["sources"]["glibc-string"], self.cache / "downloads")
                shutil.copy2(archive, self.root / "var/cache/distfiles" /
                             f"glibc-string-{LOCK['musl']['glibc_string_version']}.tar.gz")
        if "uflinux" in selected:
            artwork = download(LOCK["sources"]["gnome-backgrounds"], self.cache / "downloads")
            shutil.copy2(artwork, self.root / "var/cache/distfiles/gnome-backgrounds-3.38.0.tar.xz")
            theme = download(LOCK["sources"]["labwc-themes"], self.cache / "downloads")
            shutil.copy2(theme, self.root / "var/cache/distfiles/labwc-themes-6a2e3634.tar.gz")
        if "simp1e-cursors" in selected:
            # The recipe has two archives, so the generic naming rule above does
            # not apply and both distfiles names are written out literally.
            for origin, target in (("simp1e-cursors", "simp1e-cursors-f8f8f3c09dd0aa31cc9bc5499c683aad025984be.tar.gz"),
                                   ("cursor-generator", "cursor-generator-dd4a730c4a45c73c580220658bd1f21f141f8a55.tar.gz")):
                archive = download(LOCK["sources"][origin], self.cache / "downloads")
                shutil.copy2(archive, self.root / "var/cache/distfiles" / target)
        if "uflinux-settings" in selected:
            import importlib.util
            spec = importlib.util.spec_from_file_location("settings_source", PROJECT / "scripts/settings-source.py")
            source_builder = importlib.util.module_from_spec(spec)
            spec.loader.exec_module(source_builder)
            settings_version, settings_source = source_builder.bundle()
            if settings_version != LOCK["settings"]["version"]:
                raise RuntimeError("Settings version differs from the package version")
            settings_digest = hashlib.sha512(settings_source).hexdigest()
            recipe = source / "uflinux-settings/APKBUILD"
            checksums = f"{settings_digest}  uflinux-settings-{settings_version}.tar\n"
            if getattr(self.args, "settings_pgo", False):
                llvm_name = "llvm-project-22.1.3.src.tar.xz"
                llvm_digest = ("3557a955d55471671ae2f7b9c809affd59a29a6fb1e70a2a5d040dc1c6376246de"
                               "b0635be8ca36cae09112981760e9afb128c822e5554bd722589fb8dee3f0df")
                llvm_archive = download_pinned(
                    "https://github.com/llvm/llvm-project/releases/download/llvmorg-22.1.3/" + llvm_name,
                    llvm_digest, "sha512", self.cache / "downloads" / llvm_name)
                shutil.copy2(llvm_archive, self.root / "var/cache/distfiles" / llvm_name)
                shutil.copy2(llvm_archive, recipe.parent / llvm_name)
                checksums += f"{llvm_digest}  {llvm_name}\n"
            recipe.write_text(re.sub(r'^sha512sums="[^"]*"', f'sha512sums="\n{checksums}"',
                                     recipe.read_text(), flags=re.M))
            (self.root / "var/cache/distfiles" / f"uflinux-settings-{settings_version}.tar").write_bytes(settings_source)
            (source / "uflinux-settings" / f"uflinux-settings-{settings_version}.tar").write_bytes(settings_source)
            (self.cache / "downloads" / (settings_digest + ".tar")).write_bytes(settings_source)
        if "uflinux-rds" in selected:
            import importlib.util
            spec = importlib.util.spec_from_file_location("rds_source", PROJECT / "scripts/rds-source.py")
            source_builder = importlib.util.module_from_spec(spec)
            spec.loader.exec_module(source_builder)
            rds_version, rds_source = source_builder.bundle()
            if rds_version != LOCK["uflinux-rds"]["version"]:
                raise RuntimeError("Remote Desktop version differs from the package version")
            rds_digest = hashlib.sha512(rds_source).hexdigest()
            recipe = source / "uflinux-rds/APKBUILD"
            checksums = f"{rds_digest}  uflinux-rds-{rds_version}.tar\n"
            if getattr(self.args, "rds_pgo", False):
                llvm_name = "llvm-project-22.1.3.src.tar.xz"
                llvm_digest = ("3557a955d55471671ae2f7b9c809affd59a29a6fb1e70a2a5d040dc1c6376246de"
                               "b0635be8ca36cae09112981760e9afb128c822e5554bd722589fb8dee3f0df")
                llvm_archive = download_pinned(
                    "https://github.com/llvm/llvm-project/releases/download/llvmorg-22.1.3/" + llvm_name,
                    llvm_digest, "sha512", self.cache / "downloads" / llvm_name)
                shutil.copy2(llvm_archive, self.root / "var/cache/distfiles" / llvm_name)
                shutil.copy2(llvm_archive, recipe.parent / llvm_name)
                checksums += f"{llvm_digest}  {llvm_name}\n"
            recipe.write_text(re.sub(r'^sha512sums="[^"]*"', f'sha512sums="\n{checksums}"',
                                     recipe.read_text(), flags=re.M))
            (self.root / "var/cache/distfiles" / f"uflinux-rds-{rds_version}.tar").write_bytes(rds_source)
            (source / "uflinux-rds" / f"uflinux-rds-{rds_version}.tar").write_bytes(rds_source)
            (self.cache / "downloads" / (rds_digest + ".tar")).write_bytes(rds_source)
        env = {"CARGO_HOME": "/cache/cargo", "CARGO_TARGET_DIR": "/cache/target",
               # Emulated architectures pay a large per-CRU cost under QEMU, so the
               # fan-out is configured per architecture rather than globally.
               "CARGO_BUILD_JOBS": os.environ.get(
                   "CARGO_BUILD_JOBS", str(ARCHES[self.args.arch]["jobs"]))}
        if getattr(self.args, "settings_pgo", False):
            env["UFLINUX_SETTINGS_PGO"] = "1"
            profiles = "/cache/target/settings-training"
            profile_dir = self.cache / self.args.arch / "target/settings-training"
            if profile_dir.exists():
                remove_tree(profile_dir)
            self.chroot("install", "-d", "-o", "builder", "-g", "builder", profiles)
            if self.args.settings_pgo_profiles:
                with tarfile.open(self.args.settings_pgo_profiles, "r:gz") as archive:
                    archive.extractall(profile_dir, filter="data")
                self.chroot("chown", "-R", "builder:builder", profiles)
                env["UFLINUX_SETTINGS_PGO_REPLAY"] = "1"
        if getattr(self.args, "rds_pgo", False):
            env["UFLINUX_RDS_PGO"] = "1"
            rds_profiles = "/cache/target/rds-training"
            rds_profile_dir = self.cache / self.args.arch / "target/rds-training"
            if rds_profile_dir.exists():
                remove_tree(rds_profile_dir)
            self.chroot("install", "-d", "-o", "builder", "-g", "builder", rds_profiles)
            if self.args.rds_pgo_profiles:
                with tarfile.open(self.args.rds_pgo_profiles, "r:gz") as archive:
                    archive.extractall(rds_profile_dir, filter="data")
                self.chroot("chown", "-R", "builder:builder", rds_profiles)
                env["UFLINUX_RDS_PGO_REPLAY"] = "1"
        clang_origins = CLANG_APORTS.intersection(selected)
        if clang_origins:
            clang_profile_dir = self.root / "home/builder/aport-profiles"
            remove_tree(clang_profile_dir)
            clang_profile_dir.mkdir()
            replay = getattr(self.args, "clang_aport_profiles", None)
            if replay:
                with tarfile.open(replay, "r:gz") as archive:
                    archive.extractall(clang_profile_dir, filter="data")
                if {path.name for path in clang_profile_dir.iterdir()} != clang_origins:
                    raise ValueError("Clang profiles do not match the selected aports")
                env["UFLINUX_APORT_PROFILE_REPLAY"] = "1"
            self.chroot("chown", "-R", "builder:builder", "/home/builder/aport-profiles")
        if self.args.arch in CLANG_APORT_BOLT_ARCHES and clang_origins:
            self.chroot("apk", "add", "--no-progress", "--virtual", ".uflinux-aport-bolt",
                        "clang22", "cmake", "samurai", "llvm22-dev", "llvm22-static",
                        "llvm22-gtest", "zlib-dev", "zstd-dev", "libxml2-dev")
            try:
                env["UFLINUX_APORT_BOLT_DIR"] = self.build_clang_aport_bolt()
                self.build_origins(selected, env)
            finally:
                self.chroot("apk", "del", "--no-progress", ".uflinux-aport-bolt")
        else:
            self.build_origins(selected, env)
        from sbom import apk_metadata
        versions = {}
        for origin in selected:
            version = package_version(origin, self.args.version)
            recipe = (source / origin / "APKBUILD").read_text()
            revision = re.search(r"^pkgrel=([0-9]+)$", recipe, re.M).group(1)
            versions[origin] = version + "-r" + revision
        if self.packages_dir.exists():
            shutil.rmtree(self.packages_dir)
        self.packages_dir.mkdir(parents=True, exist_ok=True)
        for package in (self.root / "home/builder/packages").rglob("*.apk"):
            info = apk_metadata(package)
            if versions.get(info.get("origin")) == info["pkgver"]:
                shutil.copy2(package, self.packages_dir / package.name)
        (self.out / "metadata" / self.args.arch / "reproducibility.json").unlink(missing_ok=True)
        shutil.copy2(self.root / TRUSTED_KEY, self.packages_dir / BUILD_KEY)
        self.manifest("compile")
        dependency_record = self.cache / self.args.arch / "target/uflinux-build-packages.txt"
        if dependency_record.exists():
            shutil.copy2(dependency_record, self.out / "metadata" / self.args.arch / "cargo-build-packages.txt")
        inputs = self.out / "inputs" / self.args.arch
        inputs.mkdir(parents=True, exist_ok=True)
        archive_tree(self.cache / self.args.arch / "apk", inputs / "build-apks.tar.gz", self.epoch)
        archive_tree(self.cache / "downloads", inputs / "sources.tar.gz", self.epoch)
        training_archive = inputs / "settings-pgo-profiles.tar.gz"
        training_archive.unlink(missing_ok=True)
        if getattr(self.args, "settings_pgo", False):
            archive_tree(profile_dir, training_archive, self.epoch)
        rds_training_archive = inputs / "rds-pgo-profiles.tar.gz"
        rds_training_archive.unlink(missing_ok=True)
        if getattr(self.args, "rds_pgo", False):
            archive_tree(rds_profile_dir, rds_training_archive, self.epoch)
        if "musl" in selected and self.args.arch in MUSL_BOLT_ARCHES:
            archive_tree(self.root / "tmp/musl-bolt-profile", inputs / "musl-bolt-profiles.tar.gz", self.epoch)
        if clang_origins:
            archive_tree(clang_profile_dir, inputs / CLANG_APORT_PROFILES, self.epoch)
        cargo = self.cache / self.args.arch / "cargo"
        names = [str(p.relative_to(cargo)) for p in (cargo / "registry/cache", cargo / "registry/index", cargo / "git/db") if p.exists()]
        if names:
            archive_tree(cargo, inputs / "cargo-sources.tar.gz", self.epoch, names)

    def sign(self):
        if not list(self.packages_dir.glob("*.apk")):
            raise RuntimeError("No package artifacts; run the packages stage first")
        self.tools()
        incoming = self.root / "incoming"
        incoming.mkdir(exist_ok=True)
        key_manifest = self.packages_dir / "build-keys.json"
        keys = json.loads(key_manifest.read_text()) if key_manifest.exists() else None
        if keys is not None and set(keys) != {p.name for p in self.packages_dir.glob("*.apk")}:
            raise ValueError("Build keys do not cover the complete package set")
        shutil.copy2(self.packages_dir / BUILD_KEY, incoming / BUILD_KEY)
        # The original signature names the key by its published filename.
        (incoming / (SIGNING_KEY + ".pub")).write_bytes((incoming / BUILD_KEY).read_bytes())
        public = self.signing_key(release=self.args.release)
        repo = self.root / "repo" / self.args.arch
        if repo.exists():
            shutil.rmtree(repo)
        repo.mkdir(parents=True, exist_ok=True)
        for package in sorted(self.packages_dir.glob("*.apk")):
            if keys is not None:
                (incoming / (SIGNING_KEY + ".pub")).write_text(keys[package.name])
            copied = self.root / "incoming/package.apk"
            shutil.copy2(package, copied)
            self.chroot("apk", "--keys-dir", "/incoming", "verify", "/incoming/package.apk")
            control, data = split_apk(copied)
            control_path = self.root / "incoming/control.tar.gz"
            control_path.write_bytes(control)
            self.chroot("abuild-sign", "-t", SIGN_STREAM, "/incoming/control.tar.gz")
            (repo / package.name).write_bytes(control_path.read_bytes() + data)
            self.chroot("apk", "verify", "/repo/" + self.args.arch + "/" + package.name)
        self.index(repo)
        final = self.out / "repository" / self.args.arch
        if final.exists():
            shutil.rmtree(final)
        shutil.copytree(repo, final)
        shutil.copy2(public, self.out / "repository" / (SIGNING_KEY + ".pub"))
        self.manifest("sign")

    def index(self, repo):
        relative = "/" + str(repo.relative_to(self.root))
        self.chroot("apk", "index", "--description", "UFLinux " + self.args.version,
                    "--rewrite-arch", repo.name, "--no-warnings", "--output", relative + "/APKINDEX.tar.gz",
                    *[relative + "/" + p.name for p in sorted(repo.glob("*.apk"))])
        self.chroot("abuild-sign", "-t", SIGN_STREAM, relative + "/APKINDEX.tar.gz")

    def build_q6a_grub(self):
        archive = download(LOCK["sources"]["grub-q6a"], self.cache / "downloads")
        source = self.root / "opt/uflinux/grub-2.14"
        if source.exists():
            remove_tree(source)
        source.parent.mkdir(parents=True, exist_ok=True)
        with tarfile.open(archive) as tar:
            tar.extractall(source.parent, filter="data")
        self.chroot("apk", "add", "--virtual", ".uflinux-q6a-build", "build-base", "bison", "flex", "python3", "gawk",
                    "linux-headers", "patch", "gettext-dev", "xz-dev")
        self.chroot("patch", "--batch", "--fuzz=0", "-p1", "-d", "/opt/uflinux/grub-2.14",
                    "-i", "/project/patches/grub-q6a-efi-fixup.patch")
        build = (self.root / "opt/uflinux/grub-q6a")
        if build.exists():
            remove_tree(build)
        build.mkdir()
        self.chroot("sh", "-ec",
                    "cd /opt/uflinux/grub-q6a; ../grub-2.14/configure "
                    "--with-platform=efi --disable-werror --disable-nls "
                    "--disable-grub-mkfont --disable-efiemu; make -j2")
        self.chroot("apk", "del", ".uflinux-q6a-build")

    def stage_memtest(self):
        arch = self.args.arch
        variant = {"x86_64": "x86_64", "aarch64": "arm64"}.get(arch)
        if not variant:
            return
        archive = download(LOCK["sources"]["memtest86plus"], self.cache / "downloads")
        with zipfile.ZipFile(archive) as payload:
            target = self.root / "opt/uflinux/memtest86plus"
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_bytes(payload.read("mt86p_900_" + variant))

    def image(self):
        arch = self.args.arch
        platforms = {"x86_64": ("grub-bios", "grub-efi"), "ppc64le": ("grub-ieee1275",)}
        self.tools("xorriso", "mtools", "squashfs-tools", "zstd", "util-linux", "grub",
                   *platforms.get(arch, ("grub-efi",)))
        self.stage_memtest()
        for name, digest in LOCK["image"]["patches"].items():
            if hashlib.sha512((PROJECT / "patches" / name).read_bytes()).hexdigest() != digest:
                raise RuntimeError("Image compression patch differs from the source lock: " + name)
        # Keep Alpine's installed helper intact; use a patched image-only copy.
        image_tools = self.root / "opt/uflinux/bin"
        image_tools.mkdir(parents=True, exist_ok=True)
        shutil.copy2(self.root / "usr/sbin/update-kernel", image_tools / "update-kernel")
        self.chroot("patch", "--batch", "--fuzz=0", "-d", "/opt/uflinux/bin", "-p1",
                    "-i", "/project/patches/update-kernel-zstd.patch")
        update_kernel = image_tools / "update-kernel"
        update_kernel.write_text(omit_modloop_firmware(update_kernel.read_text()))
        # mkimage builds its kernel in a separate root. Select our init wrapper
        # without changing Alpine's installed mkinitfs or its configuration.
        wrapper = image_tools / "mkinitfs"
        wrapper.write_text('#!/bin/sh\nexec /sbin/mkinitfs -i /project/aports/uflinux/uflinux-initramfs-init "$@"\n')
        wrapper.chmod(0o755)
        self.signing_key(release=self.args.release)
        final = self.out / "repository" / self.args.arch
        if not final.is_dir():
            raise RuntimeError("Missing signed repository; run the sign stage first")
        repo = self.root / "repo" / self.args.arch
        if repo.exists():
            shutil.rmtree(repo)
        shutil.copytree(final, repo)
        public = self.out / "repository" / (SIGNING_KEY + ".pub")
        if public.read_bytes() != (self.root / TRUSTED_KEY).read_bytes():
            raise RuntimeError("Signing key changed between signing and ISO assembly")
        archive = download(LOCK["sources"]["aports"], self.cache / "downloads")
        upstream = self.root / "opt/aports"
        upstream.mkdir(parents=True, exist_ok=True)
        with tarfile.open(archive) as source:
            for member in source.getmembers():
                parts = Path(member.name).parts
                if len(parts) > 1 and parts[1] == "scripts":
                    member.name = str(Path(*parts[1:]))
                    source.extract(member, upstream, filter="data")
        self.chroot("patch", "--batch", "--forward", "-d", "/opt/aports", "-p1", "-i", "/project/patches/mkimage-apk3-root.patch")
        plugins = self.root / "root/.mkimage"
        plugins.mkdir(parents=True, exist_ok=True)
        shutil.copy2(PROJECT / "profiles/mkimg.uflinux.sh", plugins / "mkimg.uflinux.sh")
        (self.root / "image-out").mkdir(exist_ok=True)
        # mkimage treats an existing APKROOT as initialized, even after a failed run.
        old_cache = self.root / "image-work" / ("apkroot-" + self.args.arch) / "etc/apk/cache"
        if old_cache.is_dir() and not old_cache.is_symlink():
            for path in old_cache.iterdir():
                if path.is_file():
                    link_or_copy(path, self.cache / self.args.arch / "image-apk" / path.name)
        for path in (self.root / "image-work").rglob("*.apk"):
            if path.is_file() and not path.is_symlink():
                promote_apk(path, self.cache / self.args.arch / "image-apk")
        # A development key may change while APK control hashes stay the same.
        # Always fetch custom packages from the freshly signed local repository.
        local_prefixes = ("uflinux-", "connman-", "gpu-screen-recorder-",
                          "xdg-desktop-portal-gtk4-", "simp1e-cursors-")
        for path in (self.cache / self.args.arch / "image-apk").glob("*.apk"):
            if path.name.startswith(local_prefixes) or (
                    path.name.startswith("musl-") and "-" + LOCK["musl"]["version"] + "-r" in path.name):
                path.unlink()
        if (self.root / "image-work").exists():
            shutil.rmtree(self.root / "image-work")
        command = ["sh", "/opt/aports/scripts/mkimage.sh", "--arch", self.args.arch,
                   "--profile", "uflinux", "--tag", self.args.version,
                   "--outdir", "/image-out", "--workdir", "/image-work", "--checksum",
                   "--repository", "/repo", "--repository", f"{LOCK['alpine']['mirror']}/{LOCK['alpine']['branch']}/main",
                   "--repository", f"{LOCK['alpine']['mirror']}/{LOCK['alpine']['branch']}/community"]
        if self.args.arch in LOCK["apps"]["arches"]:
            # uflinux-desktop-meta pulls uflinux-files, which only the apps
            # repository carries. mkimage seeds the APKROOT keyring from the
            # chroot, so trust the apps key there for the run; uflinux-release
            # ships the same file in the installed system.
            self.put(APPS_TRUSTED_KEY, APPS_BUNDLED_KEY.read_text())
            command += ["--repository", LOCK["apps"]["url"], "--hostkeys"]
        arch = self.args.arch
        if arch in LIVE_TARGET:
            directory = self.root / "image-kernels" / arch
            if directory.exists():
                shutil.rmtree(directory)
            stage_kernels(self, arch, directory, {LIVE_TARGET[arch]})
            command += ["--repository", kernel_index("/image-kernels", arch)]
        self.chroot(*command, extra_env={"UFLINUX_KERNEL_FLAVOR": live_flavor(arch),
                                         "PATH": "/opt/uflinux/bin:" + self.env["PATH"],
                                         "complevel_zstd": "-" + str(LOCK["image"]["initramfs_level"]),
                                         "features_dir": "/project/aports/uflinux /etc/mkinitfs/features.d"})
        target = self.out / "images" / self.args.arch
        target.mkdir(parents=True, exist_ok=True)
        for path in (self.root / "image-out").glob("*"):
            link_or_copy(path, target / path.name)
        caches = self.out / "apk-cache" / self.args.arch
        if caches.exists():
            shutil.rmtree(caches)
        caches.mkdir(parents=True, exist_ok=True)
        contents = {}
        for path in [*(self.cache / self.args.arch / "image-apk").glob("*.apk"), *(self.root / "image-work").rglob("*.apk")]:
            if path.is_symlink() or not path.is_file():
                continue
            if not (caches / path.name).exists():
                digest = sha(path)
                destination = caches / path.name
                link_or_copy(contents.get(digest, path), destination)
                contents[digest] = destination
        self.manifest("image")
        for path in caches.glob("*.apk"):
            promote_apk(path, self.cache / self.args.arch / "image-apk")
        archive_tree(caches, self.out / "inputs" / self.args.arch / "image-apks.tar.gz", self.epoch)
        self.verify_image_compression()
        self.verify_iso_grub()
        extracted = self.root / "iso-apks"
        if extracted.exists():
            shutil.rmtree(extracted)
        self.chroot("xorriso", "-osirrox", "on", "-indev", "/image-out/uflinux-" + self.args.version + "-" + self.args.arch + ".iso",
                    "-extract", "/apks/" + self.args.arch, "/iso-apks")
        self.chroot("apk", "verify", *["/iso-apks/" + path.name for path in sorted(extracted.glob("*.apk"))])
        if arch in LIVE_TARGET:
            media = self.root / "iso-kernel"
            if media.exists():
                shutil.rmtree(media)
            self.chroot("xorriso", "-osirrox", "on", "-indev",
                        f"/image-out/uflinux-{self.args.version}-{arch}.iso",
                        "-extract", f"/apks/kernels/{arch}", "/iso-kernel")
            expected = kernels()["packages"][arch][LIVE_TARGET[arch]]["name"]
            for name in ("Packages.adb", expected):
                if sha(media / name) != sha(directory / name):
                    raise RuntimeError(f"The ISO's offline kernel repository differs: {name}")
            self.chroot("apk", "verify", "/iso-kernel/Packages.adb", "/iso-kernel/" + expected)
        self.verify_musl_media(extracted, repo)
        from sbom import generate
        generate(extracted, self.out / "metadata" / self.args.arch / f"uflinux-{self.args.version}-{self.args.arch}.spdx.json", self.args.version, self.args.arch, self.epoch)

    def verify_iso_grub(self):
        arch = self.args.arch
        iso = f"/image-out/uflinux-{self.args.version}-{arch}.iso"
        destination = self.root / "iso-grub"
        if destination.exists():
            remove_tree(destination)
        self.chroot("xorriso", "-osirrox", "on", "-indev", iso,
                    "-extract", "/boot/grub", "/iso-grub")
        self.chroot("grub-script-check", "/iso-grub/grub.cfg")
        menu = (destination / "grub.cfg").read_text()
        for mode in ("standard", "debug", "tty"):
            if f"--id uflinux-{mode} " not in menu:
                raise RuntimeError(f"{arch}: installer ISO is missing GRUB mode {mode}")
        if (destination / "themes/uflinux/splash.png").read_bytes() != (
                PROJECT / "aports/uflinux/splash.png").read_bytes():
            raise RuntimeError(f"{arch}: GRUB splash differs from the packaged theme")
        if arch in {"x86_64", "aarch64"} and (destination / "memtest86plus").read_bytes() != (
                self.root / "opt/uflinux/memtest86plus").read_bytes():
            raise RuntimeError(f"{arch}: installer ISO has the wrong Memtest86+ payload")

    def verify_image_compression(self):
        compression_dir = self.root / "compression-check"
        compression_dir.mkdir(exist_ok=True)
        flavor = live_flavor(self.args.arch)
        initramfs, modloop = f"initramfs-{flavor}", f"modloop-{flavor}"
        for name in (initramfs, modloop):
            (compression_dir / name).unlink(missing_ok=True)
            self.chroot("xorriso", "-osirrox", "on", "-indev",
                        "/image-out/uflinux-" + self.args.version + "-" + self.args.arch + ".iso",
                        "-extract", "/boot/" + name, "/compression-check/" + name)
        compression = image_compression(compression_dir / initramfs, compression_dir / modloop)
        listing = self.chroot("unsquashfs", "-ll", "/compression-check/" + modloop,
                              stdout=subprocess.PIPE, text=True).stdout
        check_modloop_firmware_listing(listing)
        compression["modloop_device_firmware"] = False
        self.chroot("zstd", "--test", "/compression-check/" + initramfs)
        report = self.out / "checks" / self.args.arch / "compression.json"
        report.parent.mkdir(parents=True, exist_ok=True)
        report.write_text(json.dumps(compression, indent=2) + "\n")

    def device_bootstrap(self, device, kernel_repo):
        """Prepare an installed rootfs for *device*: repos, keys and /project."""
        arch = self.args.arch
        root = self.root
        fingerprint = hashlib.sha256(json.dumps([LOCK, DEVICE_LOCK, kernel_fingerprint(), device],
                                                sort_keys=True).encode()).hexdigest()
        marker = root / ".uflinux-bootstrap"
        if marker.exists() and marker.read_text() != fingerprint:
            raise RuntimeError("Device build inputs changed: use a new --workdir")
        root.mkdir(parents=True, exist_ok=True)
        if not marker.exists():
            if any(root.iterdir()):
                raise RuntimeError("Incomplete device rootfs: remove only this device's "
                                   "workdir and retry")
            archive = download(LOCK["rootfs"][arch], self.cache / "downloads")
            run(["tar", "--numeric-owner", "-xpf", archive, "-C", root])
        marker.write_text(fingerprint)
        resolv = root / "etc/resolv.conf"
        resolv.unlink(missing_ok=True)
        resolv.write_bytes(Path("/etc/resolv.conf").read_bytes())
        # Executing apk is also a check of the kernel's binfmt registration.
        self.chroot("apk", "--print-arch")
        nura = DEVICE_LOCK["nura"] if device["nura"] else None
        if nura and arch not in nura["arches"]:
            raise ValueError(f"the Nura repository has no {arch} suite")
        keys = root / "etc/apk/keys"
        keys.mkdir(parents=True, exist_ok=True)
        for source in Path("/usr/share/apk/keys").glob(arch + "/*"):
            shutil.copy2(source, keys / source.name)
        public = self.out / "repository" / (SIGNING_KEY + ".pub")
        if not public.is_file():
            raise RuntimeError("Missing signed repository; run the sign stage first")
        self.put(TRUSTED_KEY, public.read_text())
        if arch in LOCK["apps"]["arches"]:
            # uflinux-desktop-meta pulls uflinux-files from the apps repository,
            # so the device rootfs needs its index and signing key up front.
            apps = LOCK["apps"]
            key = self.cache / "downloads" / ("apps-" + APPS_KEY)
            download_pinned(apps["repository_key"], apps["repository_key_sha256"], "sha256", key)
            (keys / APPS_KEY).write_bytes(key.read_bytes())
            (keys / APPS_KEY).chmod(0o644)
            # The apps index is deliberately not pinned: the repository is
            # republished whenever its packages are rebuilt, so the URL itself is
            # the pin. apk fetch below downloads the current index itself, so
            # there is nothing to cache here.
        if nura:
            # The key file name is what apk records in the signature check.
            name = urllib.parse.urlparse(nura["repository_key"]).path.rsplit("/", 1)[-1]
            key = self.cache / "downloads" / ("nura-" + name)
            download_pinned(nura["repository_key"], nura["repository_key_sha256"], "sha256", key)
            (keys / name).write_bytes(key.read_bytes())
            (keys / name).chmod(0o644)
            # The Nura index is deliberately not pinned: postmarketOS rebuilds a
            # release branch's index regularly, so the branch itself is the pin.
            # apk fetch --repository below downloads the current index itself, so
            # there is nothing to cache here.
        # The published repository only carries this architecture once a release
        # has been published, so build against the signed repository we just made,
        # copied in exactly as the ISO assembly stage does.
        final = self.out / "repository" / arch
        repo = root / "repo" / arch
        if repo.exists():
            shutil.rmtree(repo)
        shutil.copytree(final, repo)
        for package in kernel_repo.iterdir():
            shutil.copy2(package, repo)
        self.put("etc/apk/keys/" + KERNEL_KEY,
                 (PROJECT / "aports/uflinux" / KERNEL_KEY).read_text())
        self.put("etc/apk/repositories", device_repositories("/repo", arch))
        for name in DEVICE_PROJECT_DIRS:
            destination = root / "project" / name
            shutil.rmtree(destination, ignore_errors=True)
            shutil.copytree(PROJECT / name, destination,
                            ignore=shutil.ignore_patterns("__pycache__", "*.pyc"))
        (root / "project/LICENSE").write_bytes((PROJECT / "LICENSE").read_bytes())
        self.put("project/repository-url", self.url + "\n")
        # No other stage refreshes the indexes: apk 3 needs them before the first add.
        self.chroot("apk", "update")

    def device_image(self):
        """Assemble one installed GPT disk image for the selected device."""
        name = self.args.device
        device = device_target(DEVICES, name, self.args.arch)
        arch = self.args.arch
        tools = ["dosfstools", "mtools", "zstd", "util-linux", "e2fsprogs",
                 "f2fs-tools", "xfsprogs"]
        self.tools(*tools)
        uuids = device_uuids(f"{self.args.version}:{name}:{arch}")
        layout = device_partitions(device["img_size_mib"], bios=device["boot"] == "pc")
        args = argparse.Namespace(**vars(self.args))
        args.workdir = str(Path(self.args.workdir).resolve() / "device" / name)
        # The device rootfs pulls kernels and firmware that no other stage uses;
        # keeping them out of the shared cache keeps the ISO inputs unchanged.
        args.cache = str(Path(self.args.cache).resolve() / "device" / name)
        root = Builder(args)
        self.signing_key(release=self.args.release)
        kernel_repo = self.root / "device-kernels" / arch
        if kernel_repo.exists():
            shutil.rmtree(kernel_repo)
        stage_kernels(self, arch, kernel_repo, {device["kernel"].format(arch=arch)})
        root.device_bootstrap(device, kernel_repo)
        root.stage_memtest()
        with root.mounted():
            if name == "qcs6490":
                root.build_q6a_grub()
            root.chroot("mkdir", "-p", DEVICE_ESP)
            root.chroot("sh", "/project/profiles/device-rootfs.sh", extra_env=dict(
                device_environment(name, device, arch, self.args.version, uuids["root"],
                                   DEVICE_ESP),
                features_dir="/project/aports/uflinux /etc/mkinitfs/features.d",
                complevel_zstd="-" + str(LOCK["image"]["initramfs_level"])))
            root.verify_musl_installed()
            root.device_info = {"device": name, "device_label": device["label"],
                                "device_status": device["status"]}
            root.manifest("device")
            # The build read packages from the copied signed repository, which is
            # not part of the image; the shipped system points at the published
            # release repository instead, as uflinux-release advertises.
            root.put("etc/apk/repositories", device_repositories(self.url, arch))
            snapshot = self.root / "device-image" / name
            remove_tree(snapshot)
            staging = snapshot / "rootfs"
            esp = snapshot / "esp"
            device_snapshot(root.root, staging, rootfs=True)
            device_snapshot(root.root / DEVICE_ESP.lstrip("/"), esp)
        disk = snapshot / "disk"
        run(["truncate", "-s", f"{device['img_size_mib']}M", disk])
        is_pc = device["boot"] == "pc"
        run(["sfdisk", "--quiet", "--force", str(disk)],
            input=device_partition_script(layout, uuids, bios=is_pc), text=True)
        run(["mkfs.vfat", "-n", "UFLINUX", "-i", uuids["serial"], "-F", "32", "-S", "512",
             "--offset", str(layout["esp_start"]), str(disk), str(layout["esp_size"] // 2)],
            stderr=subprocess.DEVNULL)
        mcopy = ["mcopy", "-s", "-n", "-i", f"{disk}@@{layout['esp_start']}s"]
        mcopy += sorted(str(path) for path in esp.iterdir())
        run(mcopy + ["::/"], env=dict(os.environ, MTOOLS_SKIP_CHECK="1"))
        if is_pc:
            modules = root.root / "usr/lib/grub/i386-pc"
            shutil.copytree(modules, esp / "boot/grub/i386-pc", dirs_exist_ok=True)
            run(["mcopy", "-s", "-i", f"{disk}@@{layout['esp_start']}s",
                 esp / "boot/grub/i386-pc", "::/boot/grub/"])
            early = snapshot / "bios.cfg"
            fat_id = uuids["serial"][:4] + "-" + uuids["serial"][4:]
            early.write_text(f"search.fs_uuid {fat_id.upper()} root\n"
                             'set prefix="($root)/boot/grub"\n')
            shutil.copy2(modules / "boot.img", snapshot)
            run(["grub-mkimage", "-d", modules, "-O", "i386-pc", "-p", "/boot/grub",
                 "-c", early, "-o", snapshot / "core.img", "biosdisk", "part_gpt", "fat", "search_fs_uuid", "normal"])
            loop = run(["losetup", "--find", "--show", "--partscan", disk],
                       stdout=subprocess.PIPE, text=True).stdout.strip()
            try:
                run(["grub-bios-setup", "--skip-fs-probe", "-d", snapshot, loop])
            finally:
                run(["losetup", "--detach", loop])
        rootfs = device.get("rootfs", "ext4")
        if rootfs == "ext4":
            run(["mke2fs", "-q", "-Fn", "-t", "ext4", "-d", staging, "-L", "UFLINUX_ROOT",
                 "-U", uuids["root"], "-E",
                 f"offset={layout['root_start'] * SECTOR},root_owner=0:0,lazy_itable_init=0",
                 str(disk), str(layout["root_size"] // 2)])
        else:
            device_write_rootfs(rootfs, disk, snapshot, staging, layout, uuids, self.epoch,
                                device.get("f2fs_compress"))
        report = device_verify_image(disk, device, arch, uuids, layout)
        target = self.out / "devices" / arch
        target.mkdir(parents=True, exist_ok=True)
        image = target / device_image_name(self.args.version, name, arch)
        partial = image.with_name(image.name + ".partial")
        with partial.open("wb") as handle:
            run(["zstd", "-q", "-f", f"-{DEVICE_IMAGE_ZSTD_LEVEL}", "-T0", "-c", str(disk)],
                stdout=handle)
        partial.replace(image)
        digest = sha(image)
        (target / (image.name + ".sha256")).write_text(f"{digest}  {image.name}\n")
        report.update({"device": name, "image": str(image.relative_to(self.out)),
                       "sha256": digest, "bytes": image.stat().st_size,
                       "uncompressed_bytes": layout["total"] * SECTOR})
        checks = self.out / "checks" / arch / f"device-{name}.json"
        checks.parent.mkdir(parents=True, exist_ok=True)
        checks.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n")
        remove_tree(snapshot)

    def verify(self):
        (self.out / "checks" / self.args.arch / "verified.json").unlink(missing_ok=True)
        # Test the package closure without libraries or defaults from earlier builds.
        with temporary_tree(prefix="verification-", dir=self.root.parent) as directory:
            args = argparse.Namespace(**vars(self.args))
            args.workdir = str(directory)
            runtime = Builder(args)
            runtime.bootstrap()
            with runtime.mounted():
                runtime.verify_installed()

    def verify_musl_media(self, extracted, repo):
        for name in ("musl", "musl-utils"):
            filename = name + "-" + LOCK["musl"]["version"] + "-r" + LOCK["musl"]["revision"] + ".apk"
            if sha(extracted / filename) != sha(repo / filename):
                raise RuntimeError("Live media does not contain the signed UFLinux " + filename)

    def verify_musl_installed(self):
        version = LOCK["musl"]["version"] + "-r" + LOCK["musl"]["revision"]
        self.chroot("apk", "info", "-e", "musl=" + version, "musl-utils=" + version)
        self.chroot("busybox", "true")
        self.chroot("apk", "--version")

    def verify_installed(self):
        (self.out / "checks" / self.args.arch / "verified.json").unlink(missing_ok=True)
        self.tools("pax-utils", "file", "dbus", "shadow", "wayland-utils", "wlrctl", "wlr-randr", "grim", "procps", "pulseaudio-utils", "libnotify", "py3-gobject3", "py3-atspi", "wtype", "gcc", "musl-dev", "wayland-dev", "libxkbcommon-dev", "desktop-file-utils")
        self.put(TRUSTED_KEY, (self.out / "repository" / (SIGNING_KEY + ".pub")).read_text())
        repo = self.root / "repo" / self.args.arch
        if repo.exists():
            shutil.rmtree(repo)
        shutil.copytree(self.out / "repository" / self.args.arch, repo)
        if self.args.arch in LOCK["apps"]["arches"]:
            # uflinux-desktop-meta pulls uflinux-files, which only the apps
            # repository carries, and the build chroot lists Alpine alone.
            self.put(APPS_TRUSTED_KEY, APPS_BUNDLED_KEY.read_text())
            repositories = self.root / "etc/apk/repositories"
            text = repositories.read_text()
            if LOCK["apps"]["url"] not in text:
                repositories.write_text(text + LOCK["apps"]["url"] + "\n")
            self.chroot("apk", "update")
        self.chroot("apk", "upgrade", "--repository", "/repo")
        self.verify_musl_installed()
        self.chroot("apk", "add", "--repository", "/repo", "uflinux", "uflinux-live", "uflinux-installer", "uflinux-installer-ui")
        # Damaging a signed archive must fail verification before installation.
        sample = sorted(repo.glob("uflinux-defaults-*.apk"))[0]
        damaged = bytearray(sample.read_bytes())
        damaged[-12] ^= 1
        (self.root / "tmp/tampered.apk").write_bytes(damaged)
        try:
            self.chroot("apk", "verify", "/tmp/tampered.apk")
        except subprocess.CalledProcessError:
            pass
        else:
            raise RuntimeError("apk accepted a damaged signed archive")
        defaults = {
            "etc/xdg/labwc/rc.xml": "\n<!-- UFLinux upgrade preservation check -->\n",
            "etc/xdg/xdg-desktop-portal/labwc-portals.conf": "\n# UFLinux upgrade preservation check\n",
        }
        original_defaults = {name: (self.root / name).read_text() for name in defaults}
        try:
            for name, marker in defaults.items():
                (self.root / name).write_text(original_defaults[name] + marker)
            self.chroot("apk", "fix", "--reinstall", "--repository", "/repo", "uflinux-defaults")
            for name, marker in defaults.items():
                if not (self.root / name).read_text().endswith(marker):
                    raise RuntimeError("apk replaced modified system defaults: " + name)
        finally:
            for name, original in original_defaults.items():
                (self.root / name).write_text(original)
        self.chroot("sh", "-ec", "id smoke >/dev/null 2>&1 || adduser -D -u 2000 smoke; addgroup smoke netdev")
        report = self.out / "checks" / self.args.arch
        report.mkdir(parents=True, exist_ok=True)
        try:
            with self.verification_services(report):
                smoke_env = "UFLINUX_TEST_PORTALS=1 " if self.args.arch == "x86_64" else ""
                self.chroot("su", "-s", "/bin/sh", "smoke", "-c",
                            smoke_env + "UFLINUX_TEST_CONNMAN_FIXTURE=1 sh -x /project/tests/desktop-smoke.sh")
        finally:
            results = self.root / "tmp/uflinux-smoke-results"
            if results.exists():
                shutil.copytree(results, report / "desktop", dirs_exist_ok=True)
            results = self.root / "tmp/uflinux-settings-smoke-results"
            if results.exists():
                shutil.copytree(results, report / "settings", dirs_exist_ok=True)
        self.chroot("python3", "-m", "unittest", "discover", "-s", "/project/tests", "-p", "test_installer*.py")
        self.chroot("python3", "/project/tests/power-acpid-smoke.py")
        self.chroot("scanelf", "--needed", "/usr/bin/uflinux-desktop", stdout=(report / "elf.txt").open("w"))
        self.chroot("ldd", "/usr/bin/uflinux-desktop", stdout=(report / "libraries.txt").open("w"))
        binary = self.root / "usr/bin/uflinux-desktop"
        header = binary.read_bytes()[:64]
        if header[:4] != b"\x7fELF":
            raise RuntimeError("Desktop is not an ELF executable")
        byte_order = "<" if header[5] == 1 else ">"
        elf_type, machine = struct.unpack_from(byte_order + "HH", header, 16)
        expected = {"x86_64": 62, "aarch64": 183,
                    "ppc64le": 21, "riscv64": 243}[self.args.arch]
        if machine != expected or elf_type != 3:
            raise RuntimeError("Desktop must be a position-independent executable for the target architecture")
        settings_binaries = {}
        for relative in ("usr/bin/uflinux-installer", "usr/bin/uflinux-settings", "usr/bin/uflinux-settingsctl",
                         "usr/libexec/uflinux-settingsd", "usr/libexec/uflinux-settings-system", "usr/sbin/connmand",
                         "usr/sbin/connman-vpnd"):
            path = self.root / relative
            data = path.read_bytes()[:64]
            order = "<" if data[5] == 1 else ">"
            if data[:4] != b"\x7fELF" or struct.unpack_from(order + "HH", data, 16) != (3, expected):
                raise RuntimeError("Wrong architecture or non-PIE settings executable: " + relative)
            libraries = self.chroot("ldd", "/" + relative, stdout=subprocess.PIPE).stdout.decode()
            if relative.startswith("usr/libexec/") and any(name in libraries for name in ("libgtk-4", "libadwaita")):
                raise RuntimeError("A background settings service links GTK")
            (report / (path.name + "-libraries.txt")).write_text(libraries)
            settings_binaries[path.name] = {"sha256": sha(path), "bytes": path.stat().st_size}
        (report / "verified.json").write_text(json.dumps({"arch": self.args.arch, "version": self.args.version,
            "desktop": "headless smoke passed", "desktop_bytes": binary.stat().st_size, "elf_machine": machine,
            "signature_rejection": True, "configuration_preserved_on_reinstall": True, "binary_sha256": sha(binary),
            "verified_packages": {p.name: sha(p) for p in sorted(repo.glob("*.apk"))},
            "settings_binaries": settings_binaries,
            "signing": "release" if self.args.release else "development"}, indent=2) + "\n")
        self.manifest("verify")

    @contextlib.contextmanager
    def verification_services(self, report):
        """Own exact child PIDs; never kill unrelated host D-Bus processes by name."""
        processes = []
        logs = []
        try:
            (self.root / "run/dbus").mkdir(parents=True, exist_ok=True)
            # The system settings service owns a bus name that a privileged
            # operation activates on demand. Under qemu-user that activation
            # cannot elevate through the target setuid helper, so run it as a
            # managed resident process here, exactly like the D-Bus fixtures,
            # instead of letting the bus daemon start it.
            for name, command in (("system-bus", ["dbus-daemon", "--system", "--nofork", "--nopidfile"]),
                                  ("connman-fixture", ["python3", "/project/tests/mock-connman.py"]),
                                  ("bluez-fixture", ["python3", "/project/tests/mock-bluez.py"]),
                                  ("login1-fixture", ["python3", "/project/tests/mock-login1.py"]),
                                  ("settings-system", ["/usr/libexec/uflinux-settings-system", "--resident"])):
                log = (report / (name + ".log")).open("w")
                logs.append(log)
                process = subprocess.Popen(["chroot", str(self.root), "/usr/bin/env", "-i",
                    *[f"{key}={value}" for key, value in self.env.items()], *command], stdout=log, stderr=log)
                processes.append(process)
                time.sleep(0.4)
                if process.poll() is not None:
                    raise RuntimeError("Verification service failed to start: " + name)
            yield
        finally:
            for process in reversed(processes):
                if process.poll() is None:
                    process.terminate()
                    try:
                        process.wait(timeout=5)
                    except subprocess.TimeoutExpired:
                        process.kill()
                        process.wait()
            for log in logs:
                log.close()

    def manifest(self, stage):
        directory = self.out / "metadata" / self.args.arch
        directory.mkdir(parents=True, exist_ok=True)
        # Several devices share an architecture, so their records must not overwrite
        # each other when the release job merges the per-device artifacts.
        name = stage
        if stage == "device" and getattr(self.args, "device", None):
            name = stage + "-" + self.args.device
        with (directory / (name + "-packages.txt")).open("w") as record:
            self.chroot("apk", "info", "-v", stdout=record)
        files = {}
        for base in (self.packages_dir, self.out / "repository" / self.args.arch, self.out / "apk-cache" / self.args.arch):
            if base.exists():
                for path in base.glob("*.apk"):
                    files[str(path.relative_to(self.out))] = {"sha256": sha(path), "bytes": path.stat().st_size}
        metadata = {"schema": 1, "arch": self.args.arch, "version": self.args.version,
                    "stage": stage, "package": getattr(self.args, "package", None), "source_date_epoch": int(self.epoch), "sources": LOCK,
                    "repository_url": self.url, "signing": "release" if self.args.release else "development",
                    "files": files, "host": platform.platform(), "source_commit": os.environ.get("GITHUB_SHA", "local")}
        if stage == "compile" and getattr(self.args, "settings_pgo", False):
            metadata["settings_pgo"] = True
        if stage == "compile" and getattr(self.args, "rds_pgo", False):
            metadata["rds_pgo"] = True
        if stage == "image":
            iso = self.out / "images" / self.args.arch / f"uflinux-{self.args.version}-{self.args.arch}.iso"
            metadata["iso_sha256"] = sha(iso)
            metadata["iso_bytes"] = iso.stat().st_size
        if stage == "device":
            metadata.update(getattr(self, "device_info", {}))
            metadata["image_format"] = "raw-gpt"
        (directory / (name + ".json")).write_text(json.dumps(metadata, indent=2, sort_keys=True) + "\n")


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--arch", choices=ARCHES, required=True)
    parser.add_argument("--version", default=LOCK["distribution"]["version"])
    parser.add_argument("--outdir", default=str(PROJECT / "out"))
    parser.add_argument("--package", choices=PACKAGE_NAMES, help="Compile one independent package origin (packages/bootstrap stages only)")
    parser.add_argument("--device", choices=sorted(DEVICES), help="Build a device disk image instead of an ISO (--stage device, or --stage all)")
    parser.add_argument("--workdir")
    parser.add_argument("--cache")
    parser.add_argument("--repository-url", default=os.environ.get("APK_REPOSITORY_URL"))
    parser.add_argument("--stage", choices=["bootstrap", "packages", "sign", "image", "device", "verify", "all"], default="all")
    parser.add_argument("--release", action="store_true")
    parser.add_argument("--settings-pgo", action="store_true", help="Build the native Settings shard with PGO and BOLT")
    parser.add_argument("--settings-pgo-profiles", type=Path, help="Replay archived Settings training profiles")
    parser.add_argument("--rds-pgo", action="store_true", help="Build the native Remote Desktop shard with PGO and BOLT")
    parser.add_argument("--rds-pgo-profiles", type=Path, help="Replay archived Remote Desktop training profiles")
    parser.add_argument("--clang-aport-profiles", type=Path, help="Replay archived ConnMan and GPU Screen Recorder profiles")
    musl_profiles = parser.add_mutually_exclusive_group()
    musl_profiles.add_argument("--musl-bolt-profiles", type=Path, help="Replay an archived musl BOLT profile")
    musl_profiles.add_argument("--refresh-musl-bolt-profile", action="store_true", help="Record a fresh musl BOLT profile")
    args = parser.parse_args(argv)
    if args.clang_aport_profiles and (
            args.stage not in {"packages", "all"} or args.package not in CLANG_APORTS | {None}):
        parser.error("--clang-aport-profiles requires a ConnMan or GPU Screen Recorder package build")
    if (args.musl_bolt_profiles or args.refresh_musl_bolt_profile) and (
            args.stage not in {"packages", "all"} or args.package not in {None, "musl"}
            or args.arch not in MUSL_BOLT_ARCHES):
        parser.error("musl profiling requires a musl package build on x86_64, aarch64, or riscv64")
    if args.settings_pgo and (args.stage != "packages" or args.package != "uflinux-settings"
                              or args.arch not in {"x86_64", "aarch64"} or platform.machine() != args.arch):
        parser.error("--settings-pgo requires the Settings package stage on native x86_64 or aarch64")
    if args.settings_pgo_profiles and not args.settings_pgo:
        parser.error("--settings-pgo-profiles requires --settings-pgo")
    if args.rds_pgo and (args.stage != "packages" or args.package != "uflinux-rds"
                         or args.arch not in {"x86_64", "aarch64"} or platform.machine() != args.arch):
        parser.error("--rds-pgo requires the Remote Desktop package stage on native x86_64 or aarch64")
    if args.rds_pgo_profiles and not args.rds_pgo:
        parser.error("--rds-pgo-profiles requires --rds-pgo")
    if args.package and (args.stage not in {"bootstrap", "packages"} or args.release):
        parser.error("--package requires --stage packages or bootstrap, without --release")
    if args.stage == "device" and args.device is None:
        parser.error("--stage device requires --device")
    if args.device is not None and args.stage not in {"device", "all"}:
        parser.error("--device is used by --stage device and by --stage all only")
    if args.device:
        try:
            device_target(DEVICES, args.device, args.arch)
        except ValueError as exc:
            parser.error(str(exc))
    args.workdir = args.workdir or str(PROJECT / ".state" / args.package if args.package else PROJECT / ".state")
    args.cache = args.cache or str(PROJECT / ".cache/build" / args.package if args.package else PROJECT / ".cache/build")
    if not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+(?:_(?:alpha|beta|rc)[0-9]+)?", args.version):
        parser.error("Use an Alpine-compatible version, e.g. 0.1.0 or 0.1.0_rc1")
    if os.geteuid() != 0:
        parser.error("Run on a disposable Linux build host as root (sudo scripts/build.sh ...)")
    builder = Builder(args)
    try:
        builder.bootstrap()
        with builder.mounted():
            stages = ["packages", "sign", "image", "verify"]
            if args.device is not None:
                # A device build replaces the ISO: verify_installed() exercises the
                # live media and the installer, neither of which is on a board image.
                stages = ["packages", "sign", "device"]
            for stage in (stages if args.stage == "all" else [args.stage]):
                if stage != "bootstrap":
                    try:
                        getattr(builder, STAGE_METHODS.get(stage, stage))()
                    finally:
                        if args.release and stage in {"sign", "image", "device"}:
                            (builder.root / "var/lib/uflinux/keys" / SIGNING_KEY).unlink(missing_ok=True)
    except (OSError, RuntimeError, ValueError, subprocess.CalledProcessError) as exc:
        print(f"uflinux-build: {exc}", file=sys.stderr)
        return 1
    finally:
        if args.release:
            # Verification executes the packaged binary after the release key is gone.
            (builder.root / "var/lib/uflinux/keys" / SIGNING_KEY).unlink(missing_ok=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
