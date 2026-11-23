#!/usr/bin/env python3
"""Train and replay a BOLT profile for the musl ELF built by abuild."""
import argparse
import base64
import gzip
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import tempfile


def run(*args, **kwargs):
    return subprocess.run(args, check=True, timeout=300, **kwargs)


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def riscv_profile(root, loader, trainer, data):
    # Host perf cannot sample guest instruction addresses in QEMU user mode.
    # Run perf inside a small QEMU system guest and return its recording over
    # the serial console. Copy only the guest perf executable and its libraries.
    with tempfile.TemporaryDirectory(prefix="musl-riscv-") as directory:
        work = Path(directory)
        guest = work / "guest"
        guest.mkdir()
        run("chroot", root, "apk", "add", "--no-progress", "perf")
        kernel_archive = work / "kernel.apk"
        with kernel_archive.open("wb") as stream:
            run("chroot", root, "apk", "fetch", "--stdout", "linux-lts", stdout=stream)
        kernel = work / "kernel"
        with kernel.open("wb") as stream:
            run("tar", "--warning=no-unknown-keyword", "--ignore-zeros", "-xOf", kernel_archive, "boot/vmlinuz-lts", stdout=stream)
        if kernel.read_bytes()[:2] == b"\x1f\x8b":
            kernel.write_bytes(gzip.decompress(kernel.read_bytes()))

        dependencies = run("chroot", root, "ldd", "/usr/bin/perf", capture_output=True, text=True).stdout
        paths = {"/bin/busybox", "/usr/bin/perf", "/lib/ld-musl-riscv64.so.1"}
        paths.update(re.findall(r"=> (/[\w./+-]+)", dependencies))
        for name in paths:
            source = root / name.lstrip("/")
            target = guest / name.lstrip("/")
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(source, target)
        shutil.copy2(loader, guest / loader.name)
        shutil.copy2(trainer, guest / "allocator-test")
        for name in ("proc", "sys", "dev"):
            (guest / name).mkdir()
        init = guest / "init"
        init.write_text(f"""#!/bin/busybox sh
set -e
/bin/busybox mount -t proc proc /proc
/bin/busybox mount -t sysfs sysfs /sys
/bin/busybox mount -t devtmpfs devtmpfs /dev
/usr/bin/perf record -e cpu-clock:u -F 999 -o /perf.data -- /{loader.name} /allocator-test train
echo UFLINUX_PERF_BEGIN
/bin/busybox base64 /perf.data
echo UFLINUX_PERF_END
/bin/busybox poweroff -f
""")
        init.chmod(0o755)
        cpio = run("cpio", "-o", "-H", "newc", "--quiet", cwd=guest,
                   input=("\n".join([".", *(p.relative_to(guest).as_posix() for p in sorted(guest.rglob("*")))]) + "\n").encode(),
                   capture_output=True).stdout
        initramfs = work / "initramfs.cpio.gz"
        initramfs.write_bytes(gzip.compress(cpio, mtime=0))
        console = run("qemu-system-riscv64", "-machine", "virt", "-m", "1024", "-smp", "2",
                      "-nographic", "-no-reboot", "-kernel", kernel, "-initrd", initramfs,
                      "-append", "console=ttyS0 panic=1", stdin=subprocess.DEVNULL,
                      stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True).stdout
        match = re.search(r"UFLINUX_PERF_BEGIN\n(.*?)\nUFLINUX_PERF_END", console, re.S)
        if not match:
            raise RuntimeError("RISC-V guest did not return a profile\n" + console[-6000:])
        data.write_bytes(base64.b64decode("".join(match[1].split()), validate=True))


def abi(path):
    output = run("readelf", "--wide", "--dyn-syms", "--dynamic", "--program-headers",
                 path, capture_output=True, text=True).stdout
    symbols = set()
    for line in output.splitlines():
        fields = line.split()
        if len(fields) >= 8 and re.fullmatch(r"\d+:", fields[0]):
            _, _, size, kind, binding, visibility, index, name, *_ = fields
            symbols.add((name, kind, binding, visibility, index == "UND", size if kind in {"OBJECT", "TLS"} else ""))
    tags = re.findall(r"\((?:NEEDED|SONAME)\).*?\[(.*?)\]", output)
    headers = []
    for line in output.splitlines():
        fields = line.split()
        if fields and fields[0] in {"TLS", "GNU_STACK", "GNU_RELRO"}:
            headers.append((fields[0], *fields[4:]))
    return symbols, tags, headers


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("arch", choices=("x86_64", "aarch64", "riscv64"))
    parser.add_argument("root", type=Path)
    parser.add_argument("build", type=Path)
    parser.add_argument("profile", type=Path)
    parser.add_argument("--replay", action="store_true")
    args = parser.parse_args()

    loader = args.build / "lib/libc.so"
    trainer = args.build / "allocator-test"
    optimized = args.build / "lib/libc.bolt.so"
    tool = "llvm-bolt-22"
    version = run(tool, "--version", capture_output=True, text=True).stdout.strip()
    identity = {"arch": args.arch, "input_sha256": digest(loader), "bolt_version": version}
    fdata = args.profile / "libc.fdata"
    manifest = args.profile / "profile.json"
    if not args.replay:
        args.profile.mkdir(parents=True, exist_ok=True)
        with tempfile.TemporaryDirectory(prefix="musl-perf-") as directory:
            perf_data = Path(directory) / "perf.data"
            perf_tools = sorted(Path("/usr/lib/linux-tools").glob("*/perf"))
            perf_env = os.environ.copy()
            if perf_tools:
                perf_env["PATH"] = str(perf_tools[-1].parent) + ":" + perf_env["PATH"]
            if args.arch == "riscv64":
                riscv_profile(args.root, loader, trainer, perf_data)
            else:
                run("perf", "record", "-e", "cpu-clock:u", "-F", "999", "-o", perf_data,
                    "--", loader, trainer, "train", env=perf_env)
            run("perf2bolt-22", "--basic-events", "-p", perf_data, "-o", fdata, loader, env=perf_env)
        manifest.write_text(json.dumps(dict(identity, profile_sha256=digest(fdata)), indent=2) + "\n")
    if json.loads(manifest.read_text()) != dict(identity, profile_sha256=digest(fdata)):
        raise RuntimeError("musl BOLT profile does not match the linked ELF or tool")
    records = [line for line in fdata.read_text().splitlines() if re.search(r" [1-9][0-9]*$", line)]
    if not records or not any(re.search(r"\b(?:mi_|_mi_|__libc_malloc_impl)", line) for line in records):
        raise RuntimeError("musl BOLT profile has no usable allocator samples")
    # BOLT embeds its command line in an ELF note. Keep paths relative so
    # builds in different chroots produce identical unsigned APK payloads.
    shutil.copyfile(fdata, args.build / "libc.fdata")
    run(tool, "lib/libc.so", "-o", "lib/libc.bolt.so", "-data=libc.fdata",
        "--reorder-functions=exec-count", "--update-debug-sections", cwd=args.build)
    sections = run("readelf", "-SW", optimized, capture_output=True, text=True).stdout
    if ".bolt" not in sections or abi(loader) != abi(optimized):
        raise RuntimeError("musl BOLT output is missing its rewrite or changes the ELF ABI")


if __name__ == "__main__":
    main()
