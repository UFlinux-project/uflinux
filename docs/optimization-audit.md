# Optimization audit

Reviewed 17 November 2026 against the current working tree, including its existing
uncommitted changes. This is a source and saved-artifact audit. No fresh guest
boot, hardware benchmark, or performance comparison was run, and no runtime or
build configuration was changed.

A later round implemented priority 7 and documented priority 8 as blocked. The
same round confirmed that the shipped window control layout gives applications
all three buttons; see
[desktop prototype validation](desktop-prototype-validation.md).

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

`settings/daemon/src/session.rs:144` calls `clock::status()` on the GLib event
loop every 15 seconds. That function, in `settings/core/src/clock.rs:96`, makes
two synchronous ConnMan requests, each with a 2,000 ms timeout. A stalled
ConnMan can therefore consume roughly four seconds in those requests while
the Settings service cannot process other work on its event loop.

Reuse the service's system-bus connection, use asynchronous calls or one bounded
background refresh, and cache clock/service properties from ConnMan signals.
Keep a modest reconciliation timer for kernel synchronization state and missed
events. Prevent overlapping refreshes and preserve the last useful snapshot.
Test with an intentionally unresponsive ConnMan fixture while measuring other
Settings request latencies. [GIO documents that `call_sync` blocks its caller](https://docs.gtk.org/gio/method.DBusConnection.call_sync.html).

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

`.github/workflows/build.yml:72` sets `CARGO_BUILD_JOBS=1` for every compile job.
`aports/uflinux-settings/APKBUILD:22` overrides the workspace's ThinLTO with fat
LTO in packaged builds. Native builds may benefit from additional compiler
workers and ThinLTO; emulated jobs need their own memory/concurrency limits.

Compare one versus two workers on native runners, then compare fat versus thin
LTO using clean build time, peak RAM, binary/APK size, desktop latency, and the
existing reproducibility checks. Retain architecture portability and source
verification. This is a build-time opportunity; a desktop runtime speedup has
not been demonstrated.

## Recommended first implementation

Start with asynchronous clock refresh and per-device firmware lists: both have
specific causes and bounded scope. The shared firmware list already covers the
generic PC case, so the remaining firmware work is a small VM and board list.
Follow with wallpaper loading and an optional
small desktop profile. Treat the compressed live root as a separate prototype.

The unified desktop, on-demand screen capture, capped/on-demand preview capture,
OpenRC parallel startup, and Zstandard boot-image compression already exist in
the current code. Recommending those as new work would duplicate existing work.
Collect fresh baselines for the current binaries before attributing older
Ironbar-versus-desktop measurements to this working tree.
