#!/usr/bin/env python3
"""Prepare a draft before Pages deployment, then publish it after deployment succeeds."""
import argparse
import json
import os
from pathlib import Path
import subprocess

from builder import ARCHES, DEVICES, LOCK, device_image_name

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("operation", choices=["create", "prepare", "publish"])
parser.add_argument("--version", required=True)
args = parser.parse_args()
tag = "v" + args.version
repository = os.environ["GITHUB_REPOSITORY"]
if args.operation == "publish":
    subprocess.run(["gh", "release", "edit", tag, "--repo", repository, "--draft=false"], check=True)
else:
    Path("out").mkdir(exist_ok=True)
    notes = Path("out/release-notes.md")
    notes.write_text(f"UFLinux {args.version}, based on Alpine {LOCK['alpine']['version']}, with labwc and UFLinux Desktop.\n\n"
                     "Install into existing partitions with XFS root and a separate /boot: FAT32 for UEFI or x86 BIOS, or ext4 for PowerPC. See the repository installation guide before writing media.\n\n"
                     "Live login: live / live. The live account has local administrative access. Installed systems use a new password and have no live autologin.\n\n"
                     "Device images are raw GPT disk images compressed with zstd. Uncompress and write them to an SD card or eMMC with `zstd -d < image.img.zst | sudo dd of=/dev/sdX bs=8M oflag=direct status=progress`. The root partition grows to fill the media on first boot. Images boot straight to the login screen and ship no regular user: log in as root (empty password), create an account with `adduser -G wheel,audio,video,netdev,input user` and set a root password with `passwd` before exposing the board to a network.\n\n"
                     "The x86_64 ISO passed the configured QEMU boot checks in both BIOS and UEFI mode, including XFS installation and reboot. Packages and ISOs for the other release architectures are built but not boot-tested. This is virtual-machine coverage; physical hardware compatibility depends on its firmware and graphics support. Device images marked experimental are published as a technology preview.\n\n"
                     "Verify SHA256SUMS or the device image's checksum and GitHub artifact attestations before use.\n")
    result = subprocess.run(["gh", "release", "view", tag, "--repo", repository, "--json", "isDraft"], capture_output=True, text=True)
    if result.returncode:
        if "release not found" not in result.stderr.lower() and "HTTP 404" not in result.stderr:
            raise RuntimeError(result.stderr)
        subprocess.run(["gh", "release", "create", tag, "--repo", repository, "--verify-tag", "--draft",
                        "--title", "UFLinux " + args.version, "--notes-file", str(notes)], check=True)
    elif not json.loads(result.stdout)["isDraft"]:
        raise RuntimeError("Refusing to modify an already published release")
    if args.operation == "prepare":
        subprocess.run(["gh", "release", "edit", tag, "--repo", repository, "--notes-file", str(notes)], check=True)
        assets = [Path("out/images") / arch / f"uflinux-{args.version}-{arch}.iso"
                  for arch in sorted(ARCHES)]
        assets.append(Path("out/SHA256SUMS"))
        allowed = {asset.name for asset in assets}
        for device, config in DEVICES.items():
            for arch in config["arches"]:
                name = device_image_name(args.version, device, arch)
                allowed.update((name, name + ".sha256"))
        existing = subprocess.run(["gh", "release", "view", tag, "--repo", repository, "--json", "assets"],
                                  capture_output=True, text=True, check=True)
        for asset in json.loads(existing.stdout)["assets"]:
            name = asset["name"]
            if name not in allowed:
                subprocess.run(["gh", "release", "delete-asset", tag, name, "--repo", repository, "--yes"], check=True)
        subprocess.run(["gh", "release", "upload", tag, "--repo", repository, "--clobber", *map(str, assets)], check=True)
