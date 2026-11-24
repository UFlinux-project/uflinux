# Memory compression

UFLinux compresses swapped-out memory with the kernel's own mechanisms: **zram**
(a compressed block device in RAM used as swap) and **zswap** (a compressed
cache in front of a real swap device). Both come from the `uflinux-zram`
OpenRC service, which ships in the `uflinux` package and is enabled in the
`boot` runlevel of installed systems and device images. The live session
installs the package but does not enable it, so a live system keeps the
512 MiB/no-swap reference case.

Nothing is read from a board table. The service looks at the machine it is
running on: how much memory is installed, and what CPU is doing the
compressing.

## What is chosen, and from what

| Input | Source | Effect |
| --- | --- | --- |
| Installed RAM | `MemTotal` in `/proc/meminfo` | Decides the layout and every size |
| CPU class | `uname -m` plus the online processor count | Decides the compressor |
| Filesystem under `/` | `/proc/mounts` | Decides how the swapfile is created |

**Layout.** At or below `RAM_SPLIT_MIB` (4 GiB by default) the service sets up
zram only, at priority 100, and leaves zswap off — a compressed cache in front
of a compressed device would compress the same page twice. Above the split it
sets up zram *and* a `/swapfile` at priority 10, and turns on zswap so the
pages heading for that file are compressed first and written out only when the
cache is full.

**Compressor.** LZ4 on low-end CPUs: `riscv64` or any CPU with two cores or fewer. Zstandard everywhere
else. LZ4 wins where cores are scarce because it needs far less work per page;
Zstandard gives a better ratio where there are cores to spare. Both backends
are built into the kernels this project ships, so the choice is always
available, and `ZRAM_COMP`/`ZSWAP_COMP` override it outright.

**Sizes.** All of them scale with installed RAM and are clamped, so a 1 GiB
board and a 64 GiB board both get something sensible:

| Value | Formula | Clamp |
| --- | --- | --- |
| zram `disksize` | RAM / 2 | 256 MiB – 2 GiB |
| zram `mem_limit` | RAM / 4 | 64 MiB – 1 GiB |
| `/swapfile` | RAM / 2 | 2 GiB – 8 GiB |
| zswap `max_pool_percent` | fixed | 20 |
| zswap `accept_threshold_percent` | fixed | 70 |

`mem_limit` is what bounds the RAM zram actually consumes: once it is reached,
further writes fail instead of pushing the system deeper into pressure.
`max_pool_percent` plays the same role for zswap.

## The swapfile

A machine with more than 4 GiB gets a `/swapfile` because no UFLinux image
creates a swap partition. The service creates it once, marks it `0600`, runs
`mkswap -L uflinux-swap`, and enables it at the lower priority so zram is
always filled first.

`fallocate` is used on ext4, XFS and btrfs. On **F2FS** — the default
filesystem for the Qualcomm device images — and on any filesystem that cannot
be identified from `/proc/mounts`, the service writes the file with `dd`
instead. A swap file must not contain holes, and F2FS reports a hole as a hard
`-EINVAL` when the kernel tries to use it, so the slower preallocation is the
correct choice there.

## Overriding

Every knob lives in `/etc/conf.d/uflinux-zram`, commented out and at its
default: `RAM_SPLIT_MIB`, `ZRAM_COMP`, `ZRAM_DISKSIZE`, `ZRAM_MEM_LIMIT`,
`SWAPFILE`, `SWAPFILE_MIB`, `ZSWAP_COMP`, `ZSWAP_MAX_POOL_PERCENT`,
`ZSWAP_ACCEPT_THRESHOLD_PERCENT`. Uncommenting a value replaces the
autodetected one, including the compressor. `rc-service uflinux-zram restart`
applies a change.

## Failure behaviour

Only a machine that cannot create a zram device at all counts as a failed
start. A missing zswap module, a rejected compressor, a swapfile that could not
be created or enabled each cost one layer and are reported as warnings, so a
board whose kernel lacks zswap still boots with compressed swap and a
`warn …` line in the boot log rather than a failed service.

`stop` and `shutdown` swap off the devices the service created and reset the
zram device, so a restart does not leak a device.

## Validation

`tests/test_zram_policy.py` runs the real `start()` against a fake `/proc` and
a fake sysfs and asserts the outcome per scenario: a small board with a weak
CPU gets LZ4 zram only, a large board gets Zstandard zram plus a swapfile plus
zswap, an F2FS root gets a `dd`-written swapfile, conf.d values win, a missing
zswap costs only the second layer, and a machine without zram fails.
`test_service_fits_alpines_boot_order` checks the service's `depend()` edges
against Alpine's own sysinit and boot dependency graph.
