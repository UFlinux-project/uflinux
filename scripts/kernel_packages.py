"""Import signed kernel release packages for use by stock Alpine apk-tools."""
import hashlib
import json
from pathlib import Path
import subprocess
import tempfile
import zlib

PROJECT = Path(__file__).resolve().parents[1]
KERNELS = json.loads((PROJECT / "config/kernels.lock.json").read_text())
KEY = KERNELS["key"]["name"]
KEY_FILE = PROJECT / "aports/uflinux" / KEY
BASE = "https://github.com/UFlinux-project/kernel/releases/download/" + KERNELS["tag"]
LIVE_TARGET = {"x86_64": "x86_64", "aarch64": "aarch64"}


def live_flavor(arch):
    return "uflinux-" + LIVE_TARGET[arch] if arch in LIVE_TARGET else "lts"


def kernel_index(source, arch):
    return f"{source}/{arch}/Packages.adb"


def deflate_package(source, destination):
    """Rewrap the signed ADB stream without modifying any of its blocks."""
    with source.open("rb", buffering=0) as original, destination.open("wb") as output:
        header = original.read(6)
        if header[:5] != b"ADBc\x02":
            raise ValueError(f"Unexpected kernel APK compression in {source.name}")
        compressor = zlib.compressobj(level=6, wbits=-15)
        output.write(b"ADBd")
        with subprocess.Popen(["zstd", "-dc"], stdin=original, stdout=subprocess.PIPE) as decoder:
            magic = decoder.stdout.read(8)
            if magic != b"ADB.pckg":
                raise ValueError(f"Unexpected kernel APK payload in {source.name}")
            output.write(compressor.compress(magic))
            while chunk := decoder.stdout.read(1024 * 1024):
                output.write(compressor.compress(chunk))
            if decoder.wait():
                raise RuntimeError(f"Could not decompress {source.name}")
        output.write(compressor.flush())


def stage_kernels(builder, arch, directory, targets=None):
    from builder import SIGNING_PRIVATE, download_pinned

    if arch not in KERNELS["packages"]:
        return
    key = KEY_FILE.read_bytes()
    if hashlib.sha256(key).hexdigest() != KERNELS["key"]["sha256"]:
        raise ValueError("Bundled kernel signing key differs from the release pin")
    builder.put("etc/apk/keys/" + KEY, key.decode())
    directory.mkdir(parents=True, exist_ok=True)
    relative = "/" + directory.relative_to(builder.root).as_posix()
    packages = []
    for target, item in KERNELS["packages"][arch].items():
        if targets is not None and target not in targets:
            continue
        name = item["name"]
        source = builder.cache / "downloads" / "kernels" / KERNELS["tag"] / name
        download_pinned(f"{BASE}/{name}", item["sha256"], "sha256", source)
        with tempfile.TemporaryDirectory(dir=directory) as temporary:
            converted = Path(temporary) / name
            deflate_package(source, converted)
            converted.replace(directory / name)
        packages.append(f"{relative}/{name}")
    builder.chroot("apk", "verify", *packages)
    builder.chroot("apk", "mkndx", "--sign-key", SIGNING_PRIVATE,
                   "--output", relative + "/Packages.adb", *packages)
    builder.chroot("apk", "verify", relative + "/Packages.adb")
