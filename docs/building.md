# Building

Use a disposable Linux VM with Python 3.11+, GNU tar, gzip, coreutils, util-linux,
network access, root privileges, and at least 10 GiB free storage. More space is
useful when retaining several builds. QEMU user emulation and binfmt_misc with the
`F` flag are required for nonnative architectures. CI builds x86_64 and aarch64
on native runners and emulates ppc64le and riscv64.

On the configured Ubuntu runners, install `qemu-user-binfmt binfmt-support` and run:

```sh
python3 scripts/preflight.py --arch ppc64le
```

`config/sources.lock.json` pins minirootfs hashes, commit archive hashes for the
vendored sources (aports, ConnMan, gnome-backgrounds, gpu-screen-recorder, the GTK4 portal backend, the
labwc themes and ReGreet), feature choices, and the default source epoch.
Downloaded files are checked again on every reuse. Alpine's official package
repositories remain on the named
stable branch so security revisions can be incorporated. Their resolved package
versions and hashes are recorded; a pinned minirootfs does not freeze those repos.

The `apps` section pins UFLinux's own apps repository, which publishes
`uflinux-files`: its URL, signing key, key hash and indexed architectures. The
index itself is not pinned by hash: the repository is republished whenever its
packages are rebuilt, so the URL is the pin (the same policy as the Nura suite).
`scripts/check.py` verifies the pins and that
`aports/uflinux/uflinux3.ec384.pub` matches the published key, and
`scripts/check-packages.py` re-checks the live indexes. Architectures the apps
repository does not index never list it, because apk fails on an unreachable
repository line. The apps index is only a hard gate on a release tag: the
repository is published independently of this one, so an ordinary PR, main or
manual build reports an unreachable or incomplete apps index as a warning
instead of failing. A release still fails, because its images cannot resolve
`uflinux-files` without it. `check-packages.py` reads `EVENT` and `REF` to make
that decision, and retries a transient 5xx from any index before reporting it.

```sh
export APK_REPOSITORY_URL=https://repo.uflinux.org/apk/v3.24/uflinux
sudo --preserve-env=APK_REPOSITORY_URL bash scripts/build.sh --arch x86_64
```

`APK_REPOSITORY_URL` is optional: builds from the distribution repository
default to `https://repo.uflinux.org/apk/v3.24/uflinux`, and other repositories
derive `https://OWNER.github.io/REPO/apk/v3.24/uflinux` from `GITHUB_REPOSITORY`.

`--stage` accepts `bootstrap`, `packages`, `sign`, `image`, `device`, `verify`, or `all`.
With `--device TARGET`, `device` (or `all`) assembles an installed device disk
image instead of an ISO; see [device images](device-images.md).
Stages share `--workdir` (default `.state`) and `--outdir` (default `out`). Download,
APK, Cargo, and compiler caches use `--cache` (default `.cache/build`). A lockfile
change requires a fresh work directory, but verified downloads can be reused.
After each compile the Cargo and compiler caches are made world-readable: abuild
leaves them owned by uid 1000 with mode 640, and the host user that archives or
restores a cache directory cannot read those files otherwise.

GitHub compiles seven independent origins per architecture: `connman`,
`gpu-screen-recorder`, `simp1e-cursors`, `uflinux-regreet`, `uflinux-settings`, `xdg-desktop-portal-gtk4` and `uflinux`. PR/main builds schedule
seven compile jobs (x86_64 only). A release tag or a full-matrix manual run schedules 42, one
per architecture and origin, with at most eight compile jobs running together.
Each origin has its own VM, development signing key, timeout and cache. Heavy Rust
builds and their native clean-rebuild comparisons run independently. Signing and
ISO assembly wait for a complete validated set of APKs.

For an individual package build, use the same entry point:

```sh
sudo --preserve-env=APK_REPOSITORY_URL bash scripts/build.sh --arch x86_64 \
  --stage packages --package uflinux-settings
sudo --preserve-env=APK_REPOSITORY_URL python3 scripts/reproducibility.py \
  --arch x86_64 --version 0.1.0 --package uflinux-settings
```

Repeat independently for the other five origins. `--package` automatically uses
`.state/ORIGIN` and `.cache/build/ORIGIN`, writing below
`out/package-shards/ARCH/ORIGIN`. Explicit work/cache directory overrides must
remain distinct for concurrent builds. Shards support only `packages`/`bootstrap`
and never receive release keys. The ordinary `--stage all` command retains its
existing sequential local-build behavior.

The ConnMan and GPU Screen Recorder aports use Clang 22 with `-O3`, full LTO,
and instrumentation PGO on every package architecture. On x86_64 and aarch64,
the builder also compiles the pinned LLVM BOLT tool and uses it to profile and
rewrite the installed executables before packaging. The aports run the
workloads again against the finished binaries. ConnMan's workload uses a private
D-Bus session for manager, VPN, and client operations. GPU Screen Recorder's
headless workload covers startup, argument parsing, and the CLI and KMS
helpers. GPU capture and encoding require a separate hardware workload.
The package stage archives the profiles in `clang-aport-profiles.tar.gz`.
Clean rebuilds replay them with `--clang-aport-profiles`.

After every origin succeeds, collect their outputs before running `sign`, `image`
and `verify` as usual:

```sh
python3 scripts/merge-packages.py --arch x86_64 --version 0.1.0
```

Collection rejects missing APKs, stale hashes, mixed source commits, different
build settings and incomplete native reproducibility proofs. It retains a public
key per APK; the signing stage verifies each original signature against that key
before replacing its signature stream. Development private keys are not uploaded.
Each native origin must have its clean-rebuild report before collection succeeds.
See [local parallel-build validation](parallel-build-validation.md) for completed
checks and their scope.

Thirty-four custom APKs are built: patched ConnMan (runtime/OpenRC/dev/docs/debug, seven),
ReGreet (one), the GTK4 portal backend (one), the simp1e cursor theme (two: the
Simp1e scheme and the Adwaita-dark variant), patched gpu-screen-recorder (runtime/dev/docs/debug, four), the Settings
origin (nine: GUI, desktop shell, screencapture tool, installer UI, daemon/CLI, system
helper, OpenRC, documentation and debug symbols) and the distribution packages (ten).
ConnMan's pinned
upstream source and Alpine compatibility patches are retained with build inputs.

- `packages`: root installs recipe dependencies with `SUDO_APK=apk abuild -F deps`;
  the unprivileged builder runs plain `abuild`, including dependency validation,
  locked Cargo fetch, frozen release build and tests, and development signing.
  Root removes temporary dependencies afterward, including after build failures.
  This avoids target setuid helpers, which cannot elevate under QEMU user
  emulation. Dependency records and input archives are retained.
- `sign`: verifies the incoming development signatures, replaces only the APK v2
  signature stream, signs indexes, and verifies the resulting packages.
- `image`: uses Alpine mkimage, a custom profile, an APK cache, and a reproducible
  configuration overlay. Initramfs and the signed modloop use Zstandard level 19;
  SquashFS uses 128 KiB blocks without XZ-specific BCJ filters. An image-only copy
  of Alpine’s `update-kernel` selects these formats and requires kernel Zstandard
  support. The installed upstream helper remains intact. Both image formats and
  the initramfs stream are checked after ISO extraction. Compression settings and
  the helper patch checksum are recorded in the source lock. The apk-tools 3 and
  compression patches are in `patches`; see [compression details](compression.md).
- `verify`: installs the custom packages through apk, tests a headless desktop and toplevel
  controls against the resident desktop shell (quick-settings panels, Start menu and
  wallpaper drawing), checks every shipped executable's architecture and library linkage, and
  exercises the installer safeguards. Firmware boot and disk installation are separate VM jobs.
- `device` (with `--device`): installs the target's package set into a separate
  chroot, configures an installed (non-live) system, regenerates the initramfs
  without a modloop, and writes a raw GPT disk image without loop devices or
  mounts. The assembled disk is re-read and checked for its geometry, identifiers,
  boot files and initramfs compression before it is compressed.

The chroot exposes proc and selected device nodes, but not host sysfs. It is a
build environment, not a security boundary for hostile source code. GitHub uses
fresh disposable VMs and never supplies release private keys to compilation.

Outputs:

| Directory | Contents |
| --- | --- |
| `out/images/ARCH` | ISO and SHA-256/SHA-512 sidecars |
| `out/devices/ARCH` | Device disk images (`img.zst`) and their SHA-256 sidecars |
| `out/package-shards/ARCH/ORIGIN` | Isolated compile outputs before collection |
| `out/packages/ARCH` | Build-signed APKs and their public key |
| `out/repository/ARCH` | Final signed APKs and APKINDEX |
| `out/metadata/ARCH` | Source/build records, hashes, package lists, SPDX inventory |
| `out/checks/ARCH` | Headless desktop logs, screenshot, memory, validation result |
| `out/vm/ARCH/MODE` | Actual ISO/install/reboot console logs and result |
| `out/inputs/ARCH` | Compressed package, source, and Cargo inputs for rebuilding |

Release input archives retain build APK caches, Cargo registry sources/indexes,
verified upstream source archives, and image APK caches. Compiler targets are
cached in Actions rather than distributed. Parallel builds retain build APKs,
Cargo inputs and sources separately under `out/inputs/ARCH/ORIGIN`; release asset
names include that origin (for example `uflinux-0.1.0-x86_64-uflinux-settings-sources.tar.gz`).
Restore each archive into that origin's cache namespace; image APK inputs remain
shared at architecture level. This keeps exact input sets from independent jobs
without overwriting indexes or Cargo metadata. Sequential builds retain the
original four-archive format. To reconstruct an older build, use
its source commit and recorded epoch, unpack the input caches, and resolve the
recorded APK revisions from those saved packages. If mirrors have removed those
revisions, create a local signed repository from the retained APKs and explicitly
use it in the build environment. The default command still resolves current
packages from the stable branch; it is not an automatic offline replay mode.

The SPDX file inventories APKs physically bundled in the ISO, including packages
available for installation but not automatically installed at live boot. It is
not a claim that every cached package is running. Build/verify environment package
lists also contain build or test tools and are labeled by stage.

Publication requires every custom package that is not a `-dbg`, `-doc` or `-dev`
subpackage to appear in that inventory with the same checksum as the published
APK. The exception is the device-only set in `ISO_EXCLUDED_PACKAGES`
(`scripts/builder.py`): those APKs are built, signed and published so that device
disk images can install them from the release repository, but the live system
never pulls them in, so requiring them in the ISO would block every release.
A subpackage split out under a custom suffix gets no automatic dependency on its
main package, so such a recipe has to declare one; otherwise the main package is
published but never installed, and the release is refused.

Original APK license annotations are retained in SPDX `licenseComments`.
`licenseDeclared` and `licenseConcluded` use `NOASSERTION`: Alpine annotations can
contain non-SPDX strings or omit boolean operators, and this inventory does not
guess how to reinterpret them.

The native CI jobs also rebuild each origin with a fresh compiler target directory and compare
every custom APK's control and data streams, excluding signature streams. Publication
requires that report to match the exact native package set being published.

The build sets `SOURCE_DATE_EPOCH`, UTC, stable paths, normalized tar metadata,
and gzip without timestamps. GitHub uses the source commit time. These controls
help reproducibility; byte-for-byte reproducibility is not claimed until two
clean builds with identical resolved packages have been compared.

The ordinary Rust package builds use `opt-level=s`, eight codegen units, abort-on-panic, and
dynamically linked Alpine libraries. The Settings origin additionally forces fat LTO in
its APKBUILD, overriding the workspace's `lto = "thin"`; ReGreet only sets the
optimization level. Release builds use
Cargo's default Rust debug-info level; Alpine still splits any available debug
symbols into the debug APKs. There is no `target-cpu=native` or host instruction-set tuning.

For pushed release tags on native x86_64 and aarch64, the Settings origin builds
all seven executables with `opt-level=3`, fat LTO, PGO and BOLT. It trains the
instrumented binaries on private D-Bus and headless labwc and uses the same
workloads to check the final executables. To run that package shard locally on
the matching architecture, add `--settings-pgo` to the package-stage command.
The compile artifact includes `settings-pgo-profiles.tar.gz`. The native x86_64
clean rebuild replays that archive with a fresh compiler target directory.

The Remote Desktop origin is optimized the same way, with `--rds-pgo`: its five
executables are trained by starting the broker, logging in through the web
gateway and opening a headless session. Its profiles travel as
`rds-pgo-profiles.tar.gz` and replay identically. The two optimized origins
never share a shard, so a job replays at most one profile set.

Settings, the desktop shell and the screencapture tool share `settings/Cargo.lock`. Their UIs link GTK4;
the session service and privileged helper do not. `cargo auditable` records dependency data.
The nine Settings APKs split GUI, desktop shell, screencapture tool, installer UI,
daemon/CLI, system helper, OpenRC, documentation and debug symbols.
The Start menu is not a separate crate in the workspace: its sources
live in `settings/start-menu/src` and are compiled into `uflinux-desktop` with `#[path]`
module declarations, so the taskbar, its panels and the Start menu are one process.

After editing Settings source or data, run:

```sh
cargo fmt --manifest-path settings/Cargo.toml --all
python3 scripts/check.py
```

The source bundle is a sorted, uncompressed USTAR archive with normalized headers;
the build calculates its SHA-512 and inserts it into the staged APKBUILD. Run
`python3 scripts/settings-source.py --update` when changing the Settings version.
Build preparation removes only project-crate compiler artifacts to avoid stale
Cargo fingerprints from normalized source timestamps; dependency caches survive.
CI cache keys track upstream inputs, build settings and Cargo locks independently
of the changing project-source hash. Static checks need Python 3.11; compiling
Settings requires Alpine's GTK4/layer-shell, PulseAudio client, polkit-agent,
elogind, Vulkan/EGL and GLib development packages listed in its APKBUILD.

The Power panel uses Alpine's TLP 1.10 series, with the three-profile interface
introduced in 1.9. Power packages are checked for all four architectures. Tests
run the actual BusyBox ACPI parser in a private chroot with synthetic input events;
the installed x86_64 VM verifies real TLP transitions, the desktop's shared power status and
physical-key Do nothing/Shut down behavior. TLP is never executed on the build
host. Live VM tests allocate 4096 MiB. The diskless root unpacks the desktop and
full Papirus icon theme into tmpfs, whose default capacity is 75 % of RAM. This
is separate from the installed reference target; a 2048 MiB live VM exhausted
its root filesystem during boot after the artwork/theme addition. The runner
rejects boot-time APK database errors and logs root byte/inode capacity.
The installed x86_64 test still uses 512 MiB, one display and no swap.
The 512 MiB VM uses an interpreter installed from signed media solely for
measurement; Python is not an installed-desktop dependency.

To preserve an older development ISO, choose a separate `--outdir out-settings`
and fresh `--workdir .state/settings`. Do not reuse results after changing inputs.

OpenRC starts independent services in parallel for both live and installed
systems. `uflinux-defaults` supplies `rc_parallel="YES"` in
`/etc/rc.conf.d/uflinux.conf`; dependencies still determine service ordering.
Edit this file to set `rc_parallel="NO"` when diagnosing boot issues. APK preserves
local edits during upgrades. This setting takes effect on the next boot.

### UFLinux musl

`aports/musl` contains the APKBUILD and UFLinux-specific sources. The builder
supplies the unchanged Alpine files from the pinned aports archive. The signed
repository publishes the package set with a `99.`
version prefix so APK selects it during normal installs and upgrades. The loader
reports the original musl version. The shared and static libraries use mimalloc
3.7.0 with `MI_SECURE=0` on all four targets. The builder fetches the pinned
mimalloc Git commit in `config/sources.lock.json`, verifies the generated archive's
checksum, and includes it in the offline rebuild inputs. The shared library also uses BOLT on
x86_64, aarch64, and riscv64.

The recipe compiles musl with `-O3 -fno-plt` on all four targets. On x86_64 and
aarch64, BellSoft's glibc-string 2.43.1 supplies CPU-dispatched memory and string
routines in both libc libraries. The builder verifies the pinned source archive
and saves it with the package shard's offline build inputs.

On an Ubuntu 26.04 build host, install `bolt-22` and `linux-perf`. The
RISC-V job additionally needs `qemu-system-riscv`, `opensbi`, and `cpio`. Build
the shard and collect a new profile with:

```sh
sudo bash scripts/build.sh --arch x86_64 --stage packages --package musl --refresh-musl-bolt-profile
```

Use the native runner for aarch64 and the x86_64 runner for riscv64. The profiling
command needs permission to use Linux perf events. The profile is saved in
`out/package-shards/ARCH/musl/inputs/ARCH/musl-bolt-profiles.tar.gz`. Pass that
archive as `--musl-bolt-profiles PATH` to reproduce the rewrite on an identical
input ELF and BOLT version. To make a profile the default for subsequent
builds, add its archive at `profiles/musl-bolt/ARCH.tar.gz`. When that archive is
absent, the package job records a fresh profile.

When Alpine updates musl, refresh the pinned recipe and patches, update the
upstream fields in `config/sources.lock.json`, and regenerate the BOLT profiles.
Start a new UFLinux package revision whenever the input or profile changes.
The repository rejects changes to a package with an already published version.

To return to Alpine musl, disable the UFLinux repository temporarily and use
`apk add` with explicit Alpine versions for `musl` and every installed musl split
package in one transaction. Remove those version constraints before resuming
normal Alpine updates. Re-enabling the UFLinux repository makes its `99.`
packages eligible again.
