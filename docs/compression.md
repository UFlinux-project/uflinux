# Boot-image compression

UFLinux ISO initramfs and signed modloop images use Zstandard level 19.
Device disk images use Zstandard level 12 for the whole raw GPT image; see
[device images](device-images.md).
SquashFS retains 128 KiB blocks, keeping per-block decompression memory bounded.
XZ-specific BCJ filters are removed. The image builder installs the Zstandard
compressor in its build root, without adding it as a desktop dependency.

The builder copies Alpine's `update-kernel` into an image-only tools directory
and applies [the locked compression patch](../patches/update-kernel-zstd.patch).
It checks the patch checksum and requires `CONFIG_RD_ZSTD=y` and
`CONFIG_SQUASHFS_ZSTD=y` in the selected kernel configuration before proceeding.
The image-only helper removes device firmware from the modloop before signing;
the wireless regulatory database remains there. Alpine's mkinitfs still selects
firmware for initramfs drivers, and the live root includes the `linux-firmware`
subpackages listed in `config/firmware-packages.txt`, which is Alpine's
collection minus the Snapdragon-only `linux-firmware-qcom`. Devices that need
firmware before the live root is populated depend on the initramfs selection.
Alpine's installed helper
and installed-system mkinitfs settings are unchanged.

After ISO extraction, the builder checks the Zstandard frame magic, SquashFS
compressor identifier and block size, and tests the complete initramfs stream.
Results are written to `checks/ARCH/compression.json`. Source/build inventories
include the compression policy and patch checksum from `config/sources.lock.json`.

## Kernel module and kernel image compression (zstd)

The kernel build (`UFlinux-project/kernel`) compresses installed modules with
Zstandard instead of gzip. Measured on the shipped x86_64 module set (top 120
modules, zstd level 19 against the previous gzip): 44 MiB → 34 MiB, −22.8 %,
which applies both to the kernel APK and to the RAM held by the mounted
modloop, and zstd decompresses several times faster at `modprobe` time.
Alpine's `kmod` already supports zstd. x86 kernels additionally embed a zstd
self-decompressor (`KERNEL_ZSTD`), so the bzImage is smaller and starts faster
regardless of bootloader codec support. The build fails if either flip does
not take effect, and the package boot test requires a `.ko.zst` module.
`System.map` is no longer packaged (8.1 MiB per kernel APK); nothing consumes
it, and the `update-kernel` patch tolerates its absence.

The initramfs module sets are slimmed by `aports/uflinux/kms.modules` and
`scsi.modules`, which override Alpine's whole-tree KMS and SCSI feature files
with boot-only driver lists (about 13.5 MiB and 4.9 MiB less uncompressed
initramfs RAM respectively against the shipped tree; simpledrm/efifb are built
into the kernel and full GPU drivers load from the real rootfs). Both files
ship in `/etc/mkinitfs/features.d` so rebuilds on installed systems keep the
slim selection. See the optimization audit entries 13–15 for details.

## Local comparison, 8 November 2026

Both x86_64 development images contain the same desktop packages and artwork.
The compression change required no application recompilation; existing custom
APKs were verified and signed with a new development key before assembly.

| Component | Previous gzip/XZ image | Zstandard image |
|---|---:|---:|
| Initramfs | 173.29 MiB | 159.71 MiB |
| Modloop | 289.78 MiB | 300.30 MiB |
| ISO | 945.00 MiB | 942.00 MiB |

The net ISO reduction is 3.00 MiB.
These measurements do not establish a boot-time improvement or repeat-build
reproducibility. Other architectures may have different size trade-offs.

## Validation

Recorded for the development image of 8 November 2026, whose taskbar was Ironbar.
The x86_64 image passed both BIOS and UEFI live boot, signed-modloop mounting,
Ironbar validation, XFS installation/reboot and preservation of an unrelated
partition. Installed Settings/flyout, GTK authentication, TLP and ACPI tests passed
at 512 MiB with one display, no swap and no OOM. All 44 Python regression checks,
actionlint and whitespace checks passed. Network hwsim fixtures were not repeated.

Development ISO:
`.cache/zstd-out/images/x86_64/uflinux-0.1.0-x86_64.iso`

SHA-256: `4b66a788c06a308620996b0c44c5b8dde5483abf4985643270779b3002ce1f65`.

Compression evidence is in `.cache/zstd-out/checks/x86_64/compression.json`;
VM reports and consoles are in `.cache/zstd-vm/x86_64/{native,uefi}/`.
Other architecture images have not been rebuilt or boot-tested locally.

All four supported architectures have both required Zstandard capabilities in
Alpine's `linux-lts-6.18.62-r0` kernel APKs. The architecture set was narrowed after
these x86_64 image tests; their recorded ISO hashes and inventories remain
historical evidence, and the next build records the revised source lock.
