"""Import signed kernel release packages for use by stock Alpine apk-tools.

The kernel is not pinned: the newest published release of the kernel repository
is used, and each asset is checked against the SHA-256 digest GitHub publishes
with it. Set UFLINUX_KERNEL_TAG to use one release explicitly, which is how a
job keeps every one of its steps on the same kernel.
"""
import hashlib
import json
import os
import time
import urllib.error
import urllib.request
from pathlib import Path
import subprocess
import tempfile
import zlib

PROJECT = Path(__file__).resolve().parents[1]
CONFIG = json.loads((PROJECT / "config/kernels.json").read_text())
REPOSITORY = CONFIG["repository"]
KEY = CONFIG["key"]["name"]
KEY_FILE = PROJECT / "aports/uflinux" / KEY
API = "https://api.github.com/repos/" + REPOSITORY + "/releases"
LIVE_TARGET = {"x86_64": "x86_64", "aarch64": "aarch64"}

_resolved = None


def _release(tag=None):
    request = urllib.request.Request(
        API + ("/latest" if tag is None else "/tags/" + tag),
        headers={"Accept": "application/vnd.github+json", "User-Agent": "uflinux-build"})
    token = os.environ.get("GH_TOKEN") or os.environ.get("GITHUB_TOKEN")
    if token:
        request.add_header("Authorization", "Bearer " + token)
    for attempt in range(3):
        try:
            with urllib.request.urlopen(request, timeout=60) as response:
                return json.loads(response.read())
        except urllib.error.HTTPError as error:
            if error.code < 500 or attempt == 2:
                raise RuntimeError(f"Cannot read the kernel release {tag or 'latest'}: HTTP {error.code}")
            time.sleep(10 * (attempt + 1))
        except OSError as error:
            if attempt == 2:
                raise RuntimeError(f"Cannot read the kernel release {tag or 'latest'}: {error}")
            time.sleep(10 * (attempt + 1))


def describe(release):
    """The kernel index for a release document read from the release server."""
    tag = release["tag_name"]
    assets = {asset["name"]: asset for asset in release.get("assets", [])}
    packages = {}
    for arch, targets in CONFIG["targets"].items():
        entries = {}
        for target in targets:
            prefix = f"linux-uflinux-{target}-"
            name = next((name for name in assets
                         if name.startswith(prefix) and name.endswith(".apk")), None)
            if name is None:
                raise RuntimeError(f"Kernel release {tag} has no {arch}/{target} package")
            digest = assets[name].get("digest", "")
            if not digest.startswith("sha256:"):
                raise RuntimeError(f"Kernel release {tag} publishes no checksum for {name}")
            entries[target] = {"name": name, "sha256": digest.removeprefix("sha256:")}
        packages[arch] = entries
    return {"tag": tag,
            "base": f"https://github.com/{REPOSITORY}/releases/download/{tag}",
            "key": CONFIG["key"],
            "packages": packages}


def resolve(tag=None):
    """Describe the kernel packages published in *tag*, or in the newest release."""
    return describe(_release(tag))


def kernels():
    """The kernel index, read from the release server the first time it is needed."""
    global _resolved
    if _resolved is None:
        _resolved = resolve(os.environ.get("UFLINUX_KERNEL_TAG") or None)
    return _resolved


def kernel_fingerprint():
    """The kernel build input: what we require, not which release supplied it.

    The release moves on its own, so hashing the resolved index would make an
    existing workdir look stale the moment the kernel repository publishes.
    """
    return {"repository": REPOSITORY, "key": CONFIG["key"], "targets": CONFIG["targets"]}


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

    index = kernels()
    if arch not in index["packages"]:
        return
    key = KEY_FILE.read_bytes()
    if hashlib.sha256(key).hexdigest() != index["key"]["sha256"]:
        raise ValueError("Bundled kernel signing key differs from the release pin")
    builder.put("etc/apk/keys/" + KEY, key.decode())
    directory.mkdir(parents=True, exist_ok=True)
    relative = "/" + directory.relative_to(builder.root).as_posix()
    packages = []
    for target, item in index["packages"][arch].items():
        if targets is not None and target not in targets:
            continue
        name = item["name"]
        source = builder.cache / "downloads" / "kernels" / index["tag"] / name
        download_pinned(f"{index['base']}/{name}", item["sha256"], "sha256", source)
        with tempfile.TemporaryDirectory(dir=directory) as temporary:
            converted = Path(temporary) / name
            deflate_package(source, converted)
            converted.replace(directory / name)
        packages.append(f"{relative}/{name}")
    builder.chroot("apk", "verify", *packages)
    builder.chroot("apk", "mkndx", "--sign-key", SIGNING_PRIVATE,
                   "--output", relative + "/Packages.adb", *packages)
    builder.chroot("apk", "verify", relative + "/Packages.adb")
