# Optimization audit

Reviewed 17 November 2026 against the current working tree, including its existing
uncommitted changes. This is a source and saved-artifact audit. No fresh guest
boot, hardware benchmark, or performance comparison was run, and no runtime or
build configuration was changed.

A later round implemented priority 7 and documented priority 8 as blocked. The
same round confirmed that the shipped window control layout gives applications
all three buttons; see
[desktop prototype validation](desktop-prototype-validation.md).

A third round measured the dynamic-loading question directly and implemented
priorities 10 and 11. Priority 12 was measured and rejected. Those numbers come
from the Alpine SDK container rather than a guest boot, so they compare two
builds of this tree against each other and are not a hardware benchmark.

## Priorities

| Priority | Opportunity | Main benefit | Effort |
| --- | --- | --- | --- |
| 1 | Select firmware per device | Smaller device images and installed footprint | Medium (generic PC media done) |
| 2 | Avoid unpacking the live desktop into RAM | Lower live RAM requirement and less boot extraction | High |
| 3 | Offer smaller icon/font/application profiles | Smaller live root and installed footprint | Medium |
| 4 | Remove blocking clock queries from the Settings event loop | Settings responsiveness during ConnMan delays | Low–medium |
| 5 | Decode wallpapers asynchronously | Desktop responsiveness on login and wallpaper changes | Medium |
| 6 | Shorten the graphical boot dependency chain | Potentially earlier greeter/desktop | Low–medium |
| 7 | Offer bounded zram swap | Better behavior under application memory pressure | Done (measurement left open) |
| 8 | Force OpenSSL kTLS | Kernel-side TLS record encryption | Blocked (needs an openssl fork) |
| 9 | Tune native compilation separately from emulated builds | Shorter build turnaround | Low–medium |
| 10 | Load the renderer probe's GL and Vulkan libraries on demand | Faster `uflinux-settingsctl`, far smaller CLI footprint | Done |
| 11 | Validate wallpapers without GdkPixbuf in the daemon | Smaller always-running daemon footprint | Done |
| 12 | Bind Settings symbols lazily | Shorter dynamic-linker startup | Rejected (measured no-op) |
| 13 | Zstd-compress kernel modules and the x86 kernel image | Smaller kernel APK, modloop and faster module load | Done |
| 14 | Move chipset glue and niche drivers out of the kernel image | Smaller resident kernel text | Done |
| 15 | Slim the initramfs KMS/SCSI module sets | Lower RAM held by the initramfs | Done |

### 1. Select firmware per device

Implemented for generic PC media: `config/firmware-packages.txt` lists every
Alpine `linux-firmware` subpackage except `linux-firmware-qcom` and
`linux-firmware-none`, and the ISO profile, the live APK world, the device
package set and `scripts/check-packages.py` all read that one list. Alpine's
`linux-firmware` metapackage pins all 111 vendors, so it is no longer requested
anywhere; the installer copies the firmware lines from the live system's
`/etc/apk/world` into the installed world. Dropping the Snapdragon-only
`linux-firmware-qcom` removes 163 MiB of compressed APKs from every ISO.

Per-device firmware lists remain open: `config/devices.json` still selects
board-specific packages on top of the shared list, and a generic VM image
carries the whole PC collection. Qualcomm M.2 Wi-Fi and Bluetooth are unaffected
by the exclusion, since their firmware is in the `ath10k`, `ath11k`, `ath12k`,
`qca` and `ar3k` subpackages, all of which are kept.

The cached x86_64 firmware APK set contained about **756 MiB of compressed
payload** before the list was introduced; the kept set is about 594 MiB.
That is the full set, not a promised saving: the required subset varies by
device. It also does not measure process PSS. Validate cold boot, network,
audio, graphics, suspend/resume, and hotplug before narrowing a profile.

### 2. Change how the live root is supplied

`profiles/genapkovl-uflinux.sh` requests the desktop and the firmware list in the live
APK world. `profiles/mkimg.uflinux.sh:22` currently sets the tmpfs root ceiling
to **75% of RAM**. Increasing this ceiling does not reduce the actual memory
consumed by unpacked files. The older half-RAM description in `docs/building.md`
does not describe this current setting.

Prototype a preassembled SquashFS desktop root with a writable tmpfs OverlayFS
upper layer. The existing modloop is a kernel-module image, not a compressed
desktop root. A compressed desktop root would let clean file pages be loaded
on demand instead of installing every APK into tmpfs on each boot. This is a
larger change involving initramfs, APK database state, live writes, signed media,
and the installer; keep it behind a separate image profile initially.

Measure BIOS/UEFI time to usable desktop, peak guest memory, root bytes/inodes,
and installation/reboot at 2 and 4 GiB. Historical 2 GiB live-root exhaustion is
documented in `docs/settings-validation.md`; it is not a measurement of today's
working tree. [Kernel OverlayFS documentation](https://docs.kernel.org/filesystems/overlayfs.html)
describes the read-only lower and writable upper arrangement.

### 3. Split large desktop extras into optional profiles

`aports/uflinux/APKBUILD:175` makes the icon themes, fonts, media applications,
archive manager, disk utility, screen recorder, and graphics drivers part of the
desktop dependency set. Separate a core desktop from a full desktop, with the
full profile retaining the current experience.

Cached package control metadata provides these size candidates:

| Package | Installed payload | Compressed APK |
| --- | ---: | ---: |
| `papirus-icon-theme` | 114.9 MiB | 37.1 MiB |
| `font-noto-cjk` | 88.8 MiB | 71.6 MiB |
| `font-ubuntu-nerd` | 42.3 MiB | 23.4 MiB |
| `font-ubuntu-mono-nerd` | 27.2 MiB | 15.3 MiB |
| `adwaita-icon-theme` | 12.6 MiB | 1.8 MiB |

These numbers come from `.cache/build/x86_64/image-apk`, using
`scripts/sbom.py:apk_metadata`; they describe cached package versions, not a
newly assembled release. For live tmpfs installations, unpacked file payload
also consumes RAM. For installed systems, these figures primarily concern disk
space and do not imply equivalent idle process memory savings.

Consider a reduced Papirus package with verified fallback icons, optional CJK
fonts, and ordinary Ubuntu fonts plus only required symbol glyphs. Font changes
must update GTK, fontconfig, and panel CSS, which explicitly select Nerd Font
family names. Check terminal symbols, all shipped applications, tray icons,
and multilingual text. An application profile saves footprint; merely removing
an application that was never running does not save its imagined idle RSS.

### 4. Make clock refresh asynchronous

`settings/daemon/src/session.rs` used to call `clock::status()` on the GLib event
loop every 15 seconds. That function makes two synchronous ConnMan requests, each
with a 2,000 ms timeout, so a stalled ConnMan could consume roughly four seconds
while the Settings service could not process other work on its event loop.

The daemon now reuses the system-bus connection it already holds and calls
`clock::status_async()`, which issues both ConnMan requests asynchronously and
reads the kernel and configuration state on the blocking pool. The 15 second
timer is kept for kernel synchronization state and missed events, and a pending
flag prevents overlapping refreshes; the previous snapshot stays visible until a
better one arrives. Caching clock and service properties from ConnMan signals,
instead of polling, remains open. [GIO documents that `call_sync` blocks its
caller](https://docs.gtk.org/gio/method.DBusConnection.call_sync.html).

### 5. Move wallpaper loading out of painting

`settings/desktop/src/background.rs:291` checks image metadata and decodes a file
synchronously from the wallpaper draw path on a cache miss. Because wallpaper
and panel share a process and event loop, slow storage or a large image can
delay panel input too. Failed decodes are not cached, allowing later redraws
to repeat the work.

Load asynchronously when the file or output geometry changes, paint the cached
image or background color immediately, and reject stale completed requests.
Share decoded images across outputs with identical image/fit/size keys, using a
bounded cache. Cache failures until the file changes or an explicit retry.
Measure first usable panel frame and input latency during wallpaper changes,
including corrupted images and multiple displays. [GdkPixbuf supplies an
asynchronous scaled loader](https://docs.gtk.org/gdk-pixbuf/type_func.Pixbuf.new_from_stream_at_scale_async.html).

### 6. Review graphical boot ordering

OpenRC parallel startup is already enabled. However,
`aports/uflinux/uflinux-splash-stop.initd:7` orders splash shutdown after ConnMan,
iwd, and TLP, and before greetd. Those ordering edges put their startup on the
greeter's dependency path even if graphics could otherwise start earlier.

Trace the actual service timing, then remove only unnecessary edges while
retaining display handoff and account/session prerequisites. Inspect
`udev-settle`, enabled in live, installed, and device startup, for avoidable
global waits; establish the required device dependencies before changing it.

The existing `scripts/vm-test.py` boot timer starts before ISO linking/copying
and hashing, and stops after scripted login. Add separate VM-launch-to-greeter
and login-to-painted-interactive-panel measurements so host preparation and
login automation do not distort boot comparisons.

### 7. Offer bounded zram swap for small machines

Implemented as the `uflinux-zram` OpenRC service in the `uflinux` package. It
runs in the `boot` runlevel of installed systems and device images, and is
installed but not enabled in the live session, so the existing 512 MiB/no-swap
live acceptance case still holds for the ISO. See
[memory compression](memory-compression.md) for the policy and the knobs.

The remaining measurement work is unchanged: benchmark LZ4 and Zstandard on
each target CPU instead of trusting the current heuristic, and compare OOM
behavior, input latency, memory pressure, and compression CPU cost under a
repeatable workload. Compressed swap provides headroom when Firefox and
applications exceed available RAM, at a CPU cost; it does not reduce baseline
process PSS. The [kernel zram guide](https://docs.kernel.org/admin-guide/blockdev/zram.html)
documents swap use, algorithm selection, and memory limits.

### 8. Force OpenSSL kTLS

Not implemented. Kernel TLS is off unless the loaded OpenSSL configuration asks
for it, and there is no way to ask for it from a separate file.

`Options = KTLS` is a real directive (`SSL_FLAG_TBL("KTLS", SSL_OP_ENABLE_KTLS)`
in OpenSSL's `ssl/ssl_conf.c`), but it is only read inside a
`[system_default_sect]` reached through `[openssl_init] ssl_conf` →
`[ssl_sect] system_default`. Alpine's `/etc/ssl/openssl.cnf` is built
`enable-ktls` and its `[openssl_init]` section lists only `providers`, plus a
commented `.include` line, so a standalone drop-in can never be reached:
OpenSSL has no configuration drop-in directory.

That file is owned by `libcrypto3`, whose `_libcrypto()` subpackage function does
`amove etc`. abuild and apk-tools have no `fileconflicts` mechanism, so the only
way to replace it is `replaces="libcrypto3"` (apk otherwise errors with
“trying to overwrite … owned by”), and a later upgrade can re-extract the
original. `CONFIG_TLS=m` in every shipped kernel means a `ktls`
`modules-load.d` drop-in is required as well; `aports/uflinux-settings/APKBUILD`
already uses that pattern for other modules.

Conclusion: forcing kTLS needs an `aports/openssl` fork that owns and patches
`openssl.cnf`, the same way `aports/connman` pins `connman=2.0-r4`. Until then
each application opts in itself, and nothing in the image does.

### 9. Tune build throughput with measured comparisons

`config/architectures.json` now carries a per-architecture `jobs` value, and
`.github/workflows/build.yml` passes it as `CARGO_BUILD_JOBS`, so native
compiles use four compiler workers while the emulated architectures stay at two.
`aports/uflinux-settings/APKBUILD:22` overrides the workspace's ThinLTO with fat
LTO in packaged builds, and the PGO training workloads in
`settings/scripts/pgo-build.sh` run three at a time between the phase barriers.

Compare fat versus thin LTO using clean build time, peak RAM, binary/APK size,
desktop latency, and the existing reproducibility checks. Retain architecture
portability and source verification. This is a build-time opportunity; a desktop
runtime speedup has not been demonstrated.

### 10. Load the renderer probe's GL and Vulkan libraries on demand

Implemented. `settings/daemon/src/renderer.c` used to call `eglGetDisplay`,
`eglInitialize`, `eglGetProcAddress`, `vkCreateInstance` and the Vulkan
enumeration entry points directly, and `settings/daemon/build.rs` emitted a
`cargo:rustc-link-lib` line for every one of `vulkan`, `EGL` and `GLESv2` that
`pkg-config` reported. Those three libraries therefore appeared in the
`DT_NEEDED` list of both `uflinux-settingsctl` and `uflinux-settingsd`, so the
dynamic linker mapped and relocated the whole driver stack — Mesa, LLVM, the
DRI drivers — before `main`, even for the `status` and `volume` subcommands that
never touch a GPU.

The probe now `dlopen`s `libvulkan.so.1` and `libEGL.so.1` with
`RTLD_NOW | RTLD_LOCAL` and resolves each entry point with `dlsym`;
`glGetString` and the `EXT` entry points come from `eglGetProcAddress`. The
link-emission loop is gone from `build.rs`, so the libraries are no longer
`NEEDED`. `vulkan-loader-dev` and `mesa-dev` stay in `makedepends` because the
headers are still compiled against, and the compositor integration is unchanged:
`aports/uflinux/uflinux-compositor` still runs `uflinux-settingsctl renderer`.

Measured in the SDK container against a baseline built from the same tree with
the same release flags (`opt-level=s`, fat LTO, 8 codegen units, `panic=abort`),
40 executions each:

| Case | Baseline | Now |
| --- | ---: | ---: |
| `uflinux-settingsctl status`, fastest exec | 11.450 ms | **0.818 ms** |
| `uflinux-settingsctl status`, median exec | 11.839 ms | **0.872 ms** |
| `uflinux-settingsctl status`, peak RSS | 49,088 KiB | **4,940 KiB** |
| `uflinux-settingsctl renderer`, fastest exec | 11.200 ms | 10.985 ms |
| `uflinux-settingsctl renderer`, peak RSS | 48,464 KiB | 48,756 KiB |
| `uflinux-settingsctl` `NEEDED` closure | 46 objects, 251,528,296 B | **14 objects, 7,261,824 B** |
| `/bin/true` floor | 0.089 ms, 868 KiB | 0.089 ms, 868 KiB |

`renderer` is deliberately unchanged: that subcommand is exactly the one that
pays for `dlopen`, and it still loads the driver stack. The gain is on every
other invocation, which is what the panel, the compositor's non-GL path and any
user shell command actually run. Both binaries print the same `pixman` verdict
with the same exit status in a GPU-less container, so the probe still falls back
correctly when `dlopen` fails.

### 11. Validate wallpapers without GdkPixbuf in the daemon

Implemented. The daemon only needed image *dimensions* to enforce the 16
megapixel ceiling before handing a path to the desktop, but linking
`gdk-pixbuf` pulled `libgdk_pixbuf-2.0`, its loader directory, and the shared
library machinery around it into a process that runs for the whole session.

`settings/core/src/image.rs` now reads the header of PNG, JPEG, WebP, GIF, BMP
and SVG files — a 64 KiB window, with a whole-file reread for a JPEG whose
start-of-frame marker sits past the window — and reports either pixel
dimensions or `Scalable`. `set_appearance` in
`settings/daemon/src/session.rs` calls `probe_file` and applies the ceiling only
to the `Pixels` case. `gdk-pixbuf` is dropped from the daemon's manifest and
lockfile entry; the desktop and the UI keep it, since they decode pixels.

| Case | Baseline | Now |
| --- | ---: | ---: |
| `uflinux-settingsd` `NEEDED` closure | 44 objects, 17,155,048 B | **33 objects, 12,397,936 B** |
| Idle daemon, `Rss` | 10,424 KiB | **8,716 KiB** |
| Idle daemon, `Pss` | 5,484 KiB | **5,149 KiB** |

The idle sample is one 6 second run per binary over a session and system bus;
the mapped-closure delta is the more stable figure, since an idle daemon has not
touched most of its pages. Behaviour was checked over real D-Bus by sending
`Execute` with a `set-appearance` action: the prober accepts PNG, GIF, JPEG with
a start-of-frame marker, SVG, BMP and WebP, rejects a 70 KiB random blob and a
missing file, and an 8192×8192 PNG is refused with the existing 16 megapixel
message. One intentional behaviour change: a JPEG that carries only a JFIF
APP0 segment and no start-of-frame marker is now rejected where GdkPixbuf
accepted it, because the header alone cannot bound its memory cost. The desktop
re-decodes every wallpaper anyway and rejects the same file.

### 12. Bind Settings symbols lazily

Not implemented; measured as a no-op. Lazy binding needs a `.rela.plt` section
and no `BIND_NOW`, and neither condition holds here. Across the 1,585 shared
objects in the container's `/usr/lib`, only 94 have a `.rela.plt` at all: the
toolchain emits `-fno-plt`, so calls go through `R_X86_64_GLOB_DAT` GOT entries
in `.rela.dyn`, which are always processed eagerly. The totals are 7,359
lazy-bindable PLT relocations against 832,849 eager `.rela.dyn` relocations, so
the addressable set is about 0.9% of the relocation work. `libgtk-4.so.1.2200.4`
has no `.plt` or `.rela.plt` section whatsoever and carries 3,008 `GLOB_DAT`
entries. `uflinux-settingsctl` itself has 19 `JUMP_SLOT` entries in the baseline
build, and 7 after priority 10, against 856 `RELATIVE` and 107 `GLOB_DAT` in
both, and the `DT_FLAGS BIND_NOW` and
`DT_FLAGS_1 NOW` entries come from Alpine's `abuild` default `LDFLAGS`, not from
this project's `RUSTFLAGS` — so `-z lazy` would mean overriding the distribution
policy for a rounding error. The `dlopen` work in priority 10 already removes
the relocations that mattered by removing the libraries that owned them.

### 13. Zstd-compress kernel modules and the kernel image

Implemented in the kernel build (`UFlinux-project/kernel`, `scripts/build.py`).
Modules were already compressed, but with gzip; the kernel's own
`MODULE_COMPRESS_ZSTD` flip replaces it. Alpine's `kmod` is built with zstd
support, so `modprobe` reads `.ko.zst` with no new dependency, and mkinitfs
`*.ko*` globs match the new suffix unchanged. Measured on the shipped x86_64
module set (top 120 modules by size, gzip vs zstd level 19): **44 MiB → 34 MiB,
−22.8 %**, which projects to roughly 20 MiB off both the kernel APK's module
tree and the signed modloop, and the same delta off the RAM the modloop holds
while mounted. Decompression at `request_module` time is several times faster
than gzip, so demand-loading gets quicker too.

x86 targets additionally flip `KERNEL_ZSTD`: the bzImage embeds its own zstd
decompressor, so this is independent of the bootloader (GRUB 2.14 also supports
zstd if the image is ever loaded directly). The build now fails if either flip
does not survive `olddefconfig`, and the package boot test requires a
`.ko.zst` module, so a silent regression to gzip fails packaging.

Alongside, `System.map` is no longer staged or packaged (8.1 MiB per kernel
APK): nothing in the images or the installed system consumes it, and the
`update-kernel` zstd patch already tolerates its absence.

### 14. Move chipset glue and niche drivers out of the kernel image

Implemented in the same `configure()` step, for the generic (Alpine-base-config)
targets only. `CRYPTO_DEV_CCP`, the Intel designware-I2C/pinctrl/LPSS family,
Belkin/ARM USB serial, the Vivaldi keymap, NFC SHDLC and the Fusion MPT HBA move
`=y → =m`; udev demand-loads each on probe, so machines without the hardware
stop paying resident-text cost for it. `RD_BZIP2`/`RD_LZMA`/`RD_LZO` (the
initramfs is zstd), the fair-share thermal governor, and OSS emulation
(`SOUND_OSS_CORE`, `SND_OSSEMUL`) are dropped outright. The shipped config was
already module-heavy (3522 `=m` against 2471 `=y`), so the remaining built-in
set is mostly boot-essential; this trims the clear remainder. Board kernels
(defconfig targets) are untouched.

### 15. Slim the initramfs KMS and SCSI module sets

Implemented in this repository: `aports/uflinux/kms.modules` and
`scsi.modules` override Alpine's feature files, which otherwise copy the whole
`kernel/drivers/gpu` and `kernel/drivers/scsi` trees into every initramfs and
force-load them. The overrides list only boot-capable hardware: nine VM/basic
display drivers (simpledrm and efifb are built in and cover real GPUs at early
boot; full drivers load from the real rootfs) and the SCSI core plus virtio-scsi
and Hyper-V storvsc (NVMe, AHCI, USB storage and MMC come from the other
features). Measured against the shipped module tree: about **13.5 MiB** less
uncompressed RAM for the KMS override and about **4.9 MiB** for SCSI, out of a
159.71 MiB zstd initramfs that is fully RAM-resident. The files ship in
`/etc/mkinitfs/features.d` (uflinux-initramfs subpackage) so `mkinitfs -u` on
an installed system rebuilds the same slim initramfs; the builder already
searches `aports/uflinux` first, so ISO and device builds pick the override up
with no builder change.

## Recommended first implementation

Start with per-device firmware lists: it has a specific cause and bounded scope.
The shared firmware list already covers the generic PC case, so the remaining
firmware work is a small VM and board list. Follow with wallpaper loading and an optional
small desktop profile. Treat the compressed live root as a separate prototype.

The unified desktop, on-demand screen capture, capped/on-demand preview capture,
OpenRC parallel startup, and Zstandard boot-image compression already exist in
the current code. Recommending those as new work would duplicate existing work.
Collect fresh baselines for the current binaries before attributing older
Ironbar-versus-desktop measurements to this working tree.
