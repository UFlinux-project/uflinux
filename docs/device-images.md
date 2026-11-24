# Device images

Besides the generic live ISOs, UFLinux builds **installed** GPT disk images for
specific machines and boards. A device image writes straight to the disk, SD
card or eMMC, boots to the login screen, and uses an installed root filesystem.
Every image expands to fill the storage medium on first boot: XFS images online, F2FS
images offline in the initramfs. There is no squashfs modloop, no `live` user
and no live session.

Device images are **supplementary**: they are built on tags and attached to the
same release as the ISOs, but a failing or missing image never blocks publication
of the ISOs and the signed APK repository.

## Targets

| Id | Boards | Alpine arch | Kernel | Boot | Root fs | Status |
| --- | --- | --- | --- | --- | --- | --- |
| `pc-f2fs` | PCs booted from a flash drive or SD card | x86_64 | `linux-uflinux-x86_64` | BIOS and UEFI GRUB | f2fs (lz4) | supported |
| `pc-xfs` | PCs with a hard drive or SSD | x86_64 | `linux-uflinux-x86_64` | BIOS and UEFI GRUB | XFS | supported |
| `qcs6490` | Arrow Dragon Q6A and similar QCS6490 boards | aarch64 | `linux-uflinux-dragon-q6a` | board UEFI + GRUB | f2fs | experimental |
| `rpi` | Raspberry Pi 3, 4 and 5 | aarch64 | `linux-uflinux-rpi` | VideoCore firmware | f2fs | supported |
| `spacemit-k1` | SpacemiT K1 boards (Orange Pi RV2, Orange Pi R2S, BananaPi-F3) | riscv64 | `linux-uflinux-orangepi-rv2` | U-Boot distro boot (`extlinux`) | f2fs | experimental |
| `vm` | QEMU, KVM and other hypervisors | x86_64, aarch64 | `linux-uflinux-virt-<arch>` | UEFI + GRUB | XFS | supported |

The table, the per-board package list, the root filesystem, the serial console and
the kernel command line come from `config/devices.json`; the optional Nura
repository is pinned in `config/devices.lock.json`. Both files are validated by
`python3 scripts/check.py`, and every package name is checked against the live
Alpine index by `python3 scripts/check-packages.py`.

The two `pc-*` targets are not boards but machines: they write to an internal
disk or to a USB stick and boot via BIOS or x86_64 UEFI, so they are the only images
that carry a BIOS and a UEFI boot chain on the same disk. `pc-xfs` is meant for
a hard drive or SSD and uses XFS; `pc-f2fs` is meant for the flash drive or SD
card that has to survive thousands of small writes, and asks the kernel for
F2FS's lz4 cluster compression. Both images target x86_64.
Both ship `amd-ucode` and `intel-ucode` and pass both microcode images to both
bootloaders, so the CPU microcode is loaded before the kernel starts on either
firmware.

The SpacemiT K1 image uses the Orange Pi RV2 kernel. Its extlinux menu defaults
to the RV2 device tree and includes a separate Orange Pi R2S entry. The Banana
Pi F3 entry relies on a firmware-provided device tree. Boot on the F3 is
unverified and the kernel APK does not include an F3 device tree.

Dragon Q6A boots the EFI stub and Q6A device tree from its kernel APK.
The image carries Nura's `dtbloader` EFI driver at `EFI/uflinux/dtbloader.efi`
and starts it from GRUB before loading the board DTB. GRUB applies any advertised
EFI DT fixups. This GRUB path still needs physical board validation.
Raspberry Pi firmware loads the packaged `kernel8.img`, Broadcom device trees,
and overlays from the boot partition.

Device images pin the kernel version in APK world. Updating a prebuilt image
in place requires updating the kernel, its matching initramfs, and any device
trees together on the separate boot partition before rebooting. New images
already contain the corresponding boot files.

## Building one locally

```sh
sudo env APK_REPOSITORY_URL=https://repo.uflinux.org/apk/v3.24/uflinux \
  bash scripts/build.sh --arch aarch64 --version 0.1.0 --device rpi --stage all
```

`--device` is accepted only together with `--stage device` (or `--stage all`) and
only for an architecture the board supports. The stage reuses the ordinary
`bootstrap`, `packages` and `sign` stages, so the packages must already be built
and collected for that architecture (`scripts/merge-packages.py`) exactly as for
an ISO. Each device gets its own work and cache directory below
`.state/device/BOARD` and `.cache/build/device/BOARD`, so kernels and firmware do
not leak into the ISO inputs.

Outputs:

| Path | Contents |
| --- | --- |
| `out/devices/ARCH/uflinux-VERSION-DEVICE-ARCH.img.zst` | Raw GPT disk image, Zstandard level 12 |
| `out/devices/ARCH/…img.zst.sha256` | Checksum sidecar |
| `out/metadata/ARCH/device-DEVICE.json` | Build record: geometry, GUIDs, boot file sizes, installed package list |
| `out/checks/ARCH/device-DEVICE.json` | Verification report for the assembled disk |

## Disk layout and boot

Every image has the same two-partition GPT layout, built without loop devices or
mounts so that it is identical on a runner and on a developer machine:

| Partition | Start (512 B sectors) | Size | Filesystem |
| --- | --- | --- | --- |
| ESP | 2048 | 1 048 576 (512 MiB) | FAT32, label `UFLINUX` |
| ROOT | 1 050 624 | rest of the disk minus one alignment gap | f2fs on boards and `pc-f2fs`, XFS on `vm` and `pc-xfs`, label `UFLINUX_ROOT` |

f2fs is the root filesystem on the SBC targets because it is built for the
wear-levelling and small-write behaviour of SD cards and eMMC, and `pc-f2fs`
uses it for the same reason on USB flash drives and SD cards in a PC. The `vm`
and `pc-xfs` images use XFS, the same filesystem as the installed ISO target,
since a hard drive, SSD or virtual disk has none of those constraints.
`mkfs.f2fs` and `mkfs.xfs` cannot write a tree at a partition offset inside a
disk image the way `mke2fs -d` does, so those two roots
are built as a standalone volume file (`mkfs.f2fs` plus `sload.f2fs`, or
`mkfs.xfs -p DIRECTORY`) and placed with a single MiB-aligned `dd`. Those formatters
run on the build host rather than in the build chroot, so a local device build needs
`f2fs-tools` and `xfsprogs` installed there in addition to the tools an ISO build
uses; the `device-images` CI job installs them.

The disk, partition and FAT volume identifiers are derived from the version, board
id and architecture, so a rebuild produces the same GUIDs. The kernel command line
uses `root=UUID=<ROOT filesystem UUID> rootwait rootfstype=<fs> rootflags=ro`.
The formatters set the filesystem UUID to the root partition GUID too. Alpine's
initramfs passes root to BusyBox `mount`, which resolves filesystem `UUID=` and
`LABEL=` but not `PARTUUID=`. The explicit type means the initramfs carries only
the one filesystem module it needs. A target that wants F2FS cluster compression
adds `compress_algorithm=<algo>,compress_mode=fs` to those flags, and
`mkfs.f2fs` is given `-O compression,extra_attr` to match; the kernel refuses the
mount option unless both the feature bits and `extra_attr` are present in the
superblock, which `scripts/builder.py` checks by reading the bitmap at offset
`0xc84` of the root partition. Alpine's `sload.f2fs -c -a lz4` cannot be used
instead: it prints "The selected compression algorithm is not supported" and
still exits 0, because `f2fs-tools` is not linked against liblz4. The ESP carries
the kernel as `boot/vmlinuz` and the initramfs as `boot/initramfs`, plus whatever
the boot scheme needs:

- `efi` — a GRUB fallback EFI executable, `boot/grub/grub.cfg`, and any
  board device tree under `dtbs/`.
- `rpi` — native VideoCore firmware, `config.txt`, and the Standard profile in
  `cmdline.txt`. To change modes, copy `cmdline-debug.txt` or `cmdline-tty.txt`
  into place on the boot partition. Back up the Standard file first.
- `extlinux` — U-Boot loads `extlinux/extlinux.conf` and the selected board tree.
- `pc` — BIOS GRUB embeds its core in a dedicated GPT BIOS boot partition.
  UEFI uses the removable-media fallback executable. Both use the same
  GRUB menu, kernel, microcode, and initramfs on the FAT boot partition.

The initramfs is generated by `profiles/device-rootfs.sh` with an explicit feature
set (`base <rootfs> phy mmc usb scsi nvme ata virtio kms`, where `<rootfs>` is
`f2fs` or `xfs`) and an explicit kernel version, rather than through
`update-kernel`, because that helper always passes `--media` and would build the
live ISO's squashfs modloop. The image builder uses the installed kernel's
`/lib/modules/<version>` tree to generate the initramfs. Its compression is
verified against the board's `initramfs_compression` setting (gzip for
`qcs6490`, Zstandard by default). This is independent of the `.img.zst`
compression used to distribute the complete disk image.

mkinitfs collects firmware by scanning the `.ko` files it copies into the
initramfs, so firmware needed by a driver that is **built into** the kernel has
no `.ko` to declare it and would be left out. `qcs6490` hits exactly that: its
QUP I2C driver (`i2c-qcom-geni`) is built in, probes while the initramfs is
still the running system, and asks for `qcom/qcm6490/qupv3fw.elf` before the
root filesystem is mounted — without it the I2C bus feeding the HDMI bridge
never appears and HDMI stays black. A board therefore lists such files in
`initramfs_files` in `config/devices.json`; the profile passes them to mkinitfs
as `custom_files` and fails the build when a listed file is not installed.

The Adreno driver is built in too. The same list includes `a660_sqe.fw`,
`a660_gmu.bin` and the board's `qcom/qcs6490/a660_zap.mbn`, supplied as compressed
`.zst` files. Without early firmware, the kernel can report
`adreno_request_fw: failed to load a660_sqe.fw` before mounting root; rendering
startup may fail even though the text console works. Supplying firmware removes
that known failure, but a successful accelerated graphical session still needs
hardware verification.

An ARM64 initramfs assembled with Alpine mkinitfs 3.14.1, the Nura 7.0.8
module tree and Alpine's `linux-firmware-qcom-20260519-r0` was inspected on
2026-11-17. All four configured early firmware files were present under
`usr/lib/firmware`, with `/lib -> usr/lib`, matched the source package bytes,
and passed Zstandard integrity checks. The kernel's embedded configuration
enables `CONFIG_RD_GZIP` and `CONFIG_FW_LOADER_COMPRESS_ZSTD`. The device-tree
ADSP/CDSP firmware paths also resolved in the unpacked root filesystem.
This is archive-level verification of a reconstructed initramfs, not a
successful hardware-loading result or inspection of a flashed disk image.

`tests/test_qcs6490_kernel_inputs.py` can check a generated archive by setting
`UFLINUX_TEST_QCOM_INITRAMFS` to its path and
`UFLINUX_TEST_QCOM_FIRMWARE_APK` to the firmware APK. That check verifies the
ARM64 init binary, firmware contents and compression integrity, rather than
only the filenames in the board configuration. Runtime acceptance by the GPU
and secure firmware must still be checked on the board.

After unpacking the kernel, the profile always runs `depmod -a <version>`.
The inspected Nura `7.0.8-r0` package has a `modules.dep` WireGuard entry with
no prerequisites, although its module declares crypto and UDP-tunnel
dependencies. Regeneration restores those dependencies and the binary index,
so both mkinitfs and the installed modprobe can load the required modules first.

For the Dragon Q6A, networking support depends on the interface. The USB
RTL8153 Ethernet device (`0bda:8153`) is supported by `r8152`, which requires
the `mii` module; regenerating the dependency index is necessary here too.
The onboard AIC USB Wi-Fi device (`a69c:8d80`) has no matching driver in the
inspected Nura 7.0.8 module package. Installing firmware or enabling iwd alone
does not supply that missing driver. Onboard Wi-Fi is not currently supported
by this image's selected kernel package.

The same `custom_files` mechanism also carries the boot files of the splash
wrapper. The profile builds the initramfs with `-i aports/uflinux/uflinux-initramfs-init`,
and mkinitfs only *replaces* `/init` with it — it does not copy Alpine's
`/usr/share/mkinitfs/initramfs-init`, which is what the wrapper execs. On the ISO
and the installed system that file arrives through the `uflinux-plymouth` feature,
but a device image deliberately leaves that feature out, so without help the
generated `/init` execs a path that is not in the archive, init exits, and the
kernel panics before the display shows anything. The profile therefore always
appends `usr/share/mkinitfs/initramfs-init` and the two `/usr/libexec/uflinux-*`
helpers the wrapper calls to `custom_files`, and checks that all three are
installed before running mkinitfs.

Being copied into the initramfs is not the same as being loaded. The `kms`
feature pulls in everything under `kernel/drivers/gpu`, but `initramfs-init`
only probes what the kernel command line, its own built-in list, and
`/etc/modules` name, and this kernel has no `simpledrm` to fall back on. On
`qcs6490` the whole HDMI chain is loadable modules — the Dragon Q6A's bridge is
described in the device tree as `radxa,ra620`, which the generic `simple-bridge`
driver claims — so nothing brings up a display until something asks for them,
and `mdev` only handles devices registered after the initramfs starts. A board
lists such drivers in `initramfs_modules`; the profile writes them to
`/etc/modules` (which no package owns), adds that file to `custom_files`, and
checks it exists before running mkinitfs. The installed system loads the same
set later through the ordinary module cold-plug.

## Package sources

While an image is built, its rootfs reads packages from the signed repository that
the `sign` stage just produced, copied into the rootfs as `/repo`, together with
Alpine and the apps repository. The published Pages URL is not used at build time,
because it only lists an architecture after a release has been published. That copy
is a build artefact and is excluded from the image; the shipped
`/etc/apk/repositories` lists the published UFLinux repository, Alpine and the apps
repository, so `apk update` on the board works as soon as the release is public.

The apps repository is listed because `uflinux-desktop-meta` depends on
`uflinux-files`, which only it carries. It indexes few architectures, and apk 3
fails the whole transaction on a repository line whose index 404s, so the line is
written only for the architectures pinned in the `apps` section of
`config/sources.lock.json`. Its packages are signed by a separate key that apk
resolves by filename, so the device stage downloads the published
`uflinux3.ec384.pub`, verifies it against the lock, and installs it under that
exact name in `/etc/apk/keys`; `uflinux-release` ships the same file in the
installed system.

The Q6A image fetches and unpacks `dtbloader` from the pinned Nura suite.
Keeping Nura outside the configured repositories avoids its conflicting
`postmarketos-mkinitfs` provider. The kernel itself is installed from the signed
UFlinux kernel index and is recorded in the APK database.

## First boot

The rootfs snapshot excludes build-time `/dev`, `/proc`, `/sys` and `/run`
contents, but recreates those directories empty with mode 0755 before formatting
the image. They must already exist when the initramfs mounts root read-only:
Alpine moves its runtime mounts there before `switch_root`, and cannot create
missing directories on a read-only filesystem. Without them, OpenRC loses its
device nodes and kernel interfaces, and udev, gettys and the greeter fail.

The `uflinux-device` OpenRC service (from the `uflinux-device` package, which is
published in the release repository but not bundled in the live ISO) runs early in the
`boot` runlevel, after filesystem checks and before the root remount and
`localmount`. It must not depend on `localmount` while ordering itself before
`modules`: that creates a circular dependency and boot timeouts. It is inert
unless `/etc/uflinux/device` exists, so it does nothing
on an ordinary installed system; a device image writes that file with the board id.
For XFS/ext4 it runs `growpart` on the root partition once, then
`xfs_growfs /` or `resize2fs`, and records `/var/lib/uflinux/device-resized`, so
a later partition change is not undone and a reboot does not repeat the work.
Failures are warnings, not boot failures.

**F2FS roots are expanded offline in the initramfs.** `resize.f2fs` rewrites
on-disk metadata directly; running it on a mounted root, even read-only, then
remounting writable can leave the kernel using stale metadata. See the
[upstream resize implementation](https://kernel.googlesource.com/pub/scm/linux/kernel/git/jaegeuk/f2fs-tools/+/refs/heads/master/fsck/resize.c).
The initramfs is the only point in the boot where the root is guaranteed to be
unmounted, so the initramfs runs `/usr/libexec/uflinux-initramfs-resize` after
the storage devices have been located and before the root is mounted: it calls
`growpart` on the root partition and then `resize.f2fs -F`. The work is done by
the `uflinux-f2fs-resize` mkinitfs feature, which the device profile enables for
F2FS roots and which carries `growpart`, `sfdisk`, `partx` and `resize.f2fs`
into the initramfs. The hook is best-effort and never aborts the boot; a board
written to a card of the image's own size, or a second boot after a successful
growth, is a no-op. The first-boot service then only records
`/var/lib/uflinux/device-resized` for an F2FS root so the check is not repeated.

A reported reboot failure (`SIT is corrupted`, `Failed to initialize F2FS
segment manager`, `Data consistency error`) indicates existing filesystem
damage. The offline resize prevents the unsafe online operation; it does not
repair an already damaged card. Preserve data before running offline `fsck.f2fs`
or reflashing.

Device images also enable `uflinux-zram`, which sizes compressed swap from the
memory the board actually has and picks the compressor from its CPU. On these
boards that means an LZ4 zram device, plus a `dd`-written `/swapfile` and zswap
once the board has more than 4 GiB. See
[memory compression](memory-compression.md).

Every device image ships a regular account: **user / user**. Select the UFLinux
session at the graphical greeter, or use this account on the text/serial console.
The account belongs to `wheel`, `audio`, `video`, `input`, `netdev` and, when
available, `render`. Both `sudo` and `doas` allow administrative commands with
the account's password. To change it after login:

```sh
passwd
```

`greetd` with the UFLinux greeter is enabled by default, so the account can log
in graphically. Serial gettys are configured per board from the board description
(`ttyMSM0`, `ttyAMA0` or `ttyS0` at 115200 8n1) in addition to `tty1`/`tty2`.

The shared desktop package installs `util-linux-login`, whose PAM-enabled
`/bin/login` replaces the BusyBox login applet. Alpine's PAM `base-session`
opens an elogind session and provides `XDG_RUNTIME_DIR` for a fresh local TTY
login. With D-Bus and elogind running, log in as `user`, stop greetd if needed,
and run `uflinux-session` without sudo. Installing the login package while
already logged in requires logging out and back in; it does not retrofit the
current shell's session. Do not replace this with a shared runtime directory
or a manually exported `/tmp` path.

## Writing an image

```sh
zstd -d < uflinux-0.1.0-rpi-aarch64.img.zst | sudo dd of=/dev/sdX bs=8M oflag=direct status=progress
sync
```

The uncompressed image is 6144 MiB (5120 MiB for `vm`, 4096 MiB for the `pc-*`
images), so any larger card, eMMC or disk works. Every root filesystem expands on
first boot: XFS online through the
first-boot service, F2FS offline in the initramfs. Verify the checksum sidecar and the
GitHub attestation first:

```sh
sha256sum -c uflinux-0.1.0-rpi-aarch64.img.zst.sha256
gh attestation verify uflinux-0.1.0-rpi-aarch64.img.zst --repo OWNER/REPO
```

## Continuous integration

`device-images` is a matrix job that runs only on version tags (and on manual runs
where the **Build device images** option is checked). It reuses the compile
artifacts, signs the packages, and assembles one image per board/architecture pair,
with at most four at a time. Experimental entries use `continue-on-error`, so their
logs and artifacts are published without failing the release.

`device-assets` runs after `publish` and uploads the images, their checksums and
their build attestations to the already-published release. Because it runs last and
is marked `continue-on-error`, a device problem can never leave a release half
published.

## Status of the evidence

The device stage has unit tests for the geometry, identifiers, package set, boot
configuration contract and the post-build disk verification, and every package name
is checked against the live Alpine and Nura indexes. The `qcs6490` image has
been booted on an Arrow Dragon Q6A from a USB stick: the board's firmware reaches
`systemd-boot` and the entry loads, but with Alpine's `linux-lts` the HDMI output
stayed black, which is what prompted the switch to the Nura mainline kernel. The
other images have not been booted on physical hardware yet, and none of them are
covered by the QEMU ISO boot tests. That is why the QCS6490 and SpacemiT targets are
labelled experimental and why the release notes describe device images as a preview.
