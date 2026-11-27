#!/usr/bin/env python3
"""Reuse published APKs instead of recompiling unchanged origins.

An origin is reusable when its published repository already carries every
package the local recipe would produce, at the exact name-version-rN the build
would generate. The recipe revision is the change signal: bumping pkgrel (or a
version bump in config/sources.lock.json) changes the expected filenames and
forces a real compile.

A reused origin is written as a package shard with the same layout a compile
job uploads, so scripts/merge-packages.py collects it next to compiled shards.
The APKs keep their published bytes and their published signing key, and the
record marks them as reused so the merge can account for the missing rebuild
proof.

Anything this script cannot reuse it leaves unsharded, and it never fails on a
repository it cannot reach: the caller compiles the origins left behind.
"""
import argparse
import json
import os
from pathlib import Path
import platform
import re
import shutil
import tempfile
import time
import urllib.error
import urllib.request

from builder import (APPS_BUNDLED_KEY, ARCHES, BUILD_KEY, CARGO_ORIGINS, CLANG_APORTS,
                     CLANG_APORT_PROFILES, LOCK, PACKAGE_NAMES, archive_tree, package_output,
                     package_revision, package_version, repository_url, sha, unsigned_digest)
import sbom

# Origins whose published packages can stand in for a compile. Origins that
# publish optimisation or profile inputs (musl, ConnMan, gpu-screen-recorder,
# uflinux-settings, uflinux-rds) are excluded: their shards must also carry
# freshly generated profile archives.
REUSABLE_ORIGINS = ("uflinux-regreet", "xdg-desktop-portal-gtk4", "simp1e-cursors", "uflinux")
APK_NAME = re.compile(r"[A-Za-z0-9][A-Za-z0-9._+~-]*\.apk")
RELEASES_LIMIT = 1024 * 1024


class Unavailable(Exception):
    """Raised when an origin cannot be reused."""


def open_repository(url, timeout):
    for retry_delay in (5, 10, 20, None):
        try:
            return urllib.request.urlopen(url, timeout=timeout)
        except urllib.error.HTTPError as error:
            if retry_delay is None or error.code not in (429, 500, 502, 503, 504):
                error.add_note(f"While fetching {url}")
                raise
            error.close()
            print(f"HTTP {error.code} fetching {url}. Retrying in {retry_delay}s.", flush=True)
            time.sleep(retry_delay)


def read_releases(repository):
    try:
        with open_repository(repository + "/releases.json", timeout=30) as response:
            body = response.read(RELEASES_LIMIT + 1)
    except urllib.error.HTTPError as error:
        if error.code != 404:
            raise
        return None
    if len(body) > RELEASES_LIMIT:
        raise ValueError("Published repository history exceeds its size limit")
    history = json.loads(body)
    if not isinstance(history, list):
        raise ValueError("Published repository history is invalid")
    return history


def published_files(history, arch):
    names = set()
    for release in history:
        for name in release.get("packages", {}).get(arch, []):
            if not isinstance(name, str) or not APK_NAME.fullmatch(name):
                raise ValueError("Invalid published APK filename")
            names.add(name)
    return names


def expected_files(origin, version):
    suffix = package_version(origin, version) + "-r" + package_revision(origin)
    return {pkgname + "-" + suffix + ".apk" for pkgname in PACKAGE_NAMES[origin]}


def plan(repository, arch, version, origins=REUSABLE_ORIGINS):
    """Return reusable origins mapped to the published filenames they need."""
    history = read_releases(repository)
    if history is None:
        return {}
    available = published_files(history, arch)
    selected = {}
    for origin in origins:
        if origin not in PACKAGE_NAMES:
            raise ValueError("Unknown origin " + origin)
        expected = expected_files(origin, version)
        missing = expected - available
        if missing:
            continue
        selected[origin] = sorted(expected)
    return selected


def download(repository, arch, name, destination):
    destination.parent.mkdir(parents=True, exist_ok=True)
    partial = destination.with_name(destination.name + ".partial")
    with open_repository(repository + "/" + arch + "/" + name, timeout=120) as response:
        with partial.open("wb") as output:
            while chunk := response.read(1024 * 1024):
                output.write(chunk)
    os.replace(partial, destination)
    return destination


def inspect(path, arch, origin, expected):
    """Verify a downloaded APK is the package this build would have produced."""
    info = sbom.apk_metadata(path)
    if info.get("origin") != origin:
        raise Unavailable(f"{path.name} comes from origin {info.get('origin')}, expected {origin}")
    if info.get("pkgver") != expected:
        raise Unavailable(f"{path.name} provides {info.get('pkgver')}, expected {expected}")
    if info.get("arch") not in (arch, "noarch"):
        raise Unavailable(f"{path.name} is built for {info.get('arch')}, not {arch}")
    if path.name != info["pkgname"] + "-" + info["pkgver"] + ".apk":
        raise Unavailable(f"{path.name} is not named after its package version")
    try:
        unsigned_digest(path)
    except ValueError as error:
        raise Unavailable(f"{path.name} is not a signed APKv2 package: {error}") from error


def retained_archive(destination, epoch):
    """Write a deterministic empty archive for an input a reused shard cannot supply."""
    with tempfile.TemporaryDirectory() as empty:
        archive_tree(Path(empty), destination, epoch)


def retained_inputs(origin, destination, epoch):
    names = ["build-apks", "sources"]
    if origin in CARGO_ORIGINS:
        names.append("cargo-sources")
    if origin in CLANG_APORTS:
        names.append(CLANG_APORT_PROFILES[:-len(".tar.gz")])
    for name in names:
        retained_archive(destination / (name + ".tar.gz"), epoch)


def reuse_record(builder, origin, repository):
    """Build a compile record for a shard whose APKs came from the published repository."""
    shard = builder.out
    files = {}
    for path in sorted(shard.rglob("*")):
        if path.is_file():
            files[str(path.relative_to(shard))] = {"sha256": sha(path), "bytes": path.stat().st_size}
    return {
        "schema": 1,
        "arch": builder.args.arch,
        "version": builder.args.version,
        "stage": "compile",
        "package": origin,
        "source_date_epoch": int(builder.epoch),
        "sources": LOCK,
        "repository_url": repository,
        "signing": "release",
        "reused": True,
        "reused_from": repository,
        "host": platform.platform(),
        "source_commit": os.environ.get("GITHUB_SHA", "local"),
        "files": files,
    }


def reuse(builder, origin, names, repository):
    """Fetch one origin's published packages as a compile-compatible shard."""
    shard = builder.out
    packages = shard / "packages" / builder.args.arch
    metadata = shard / "metadata" / builder.args.arch
    inputs = shard / "inputs" / builder.args.arch
    if packages.exists():
        shutil.rmtree(packages)
    if inputs.exists():
        shutil.rmtree(inputs)
    packages.mkdir(parents=True)
    expected = package_version(origin, builder.args.version) + "-r" + package_revision(origin)
    for name in names:
        download(repository, builder.args.arch, name, packages / name)
        inspect(packages / name, builder.args.arch, origin, expected)
    shutil.copy2(APPS_BUNDLED_KEY, packages / BUILD_KEY)
    retained_inputs(origin, inputs, builder.epoch)
    record = reuse_record(builder, origin, repository)
    metadata.mkdir(parents=True, exist_ok=True)
    (metadata / "compile.json").write_text(json.dumps(record, indent=2, sort_keys=True) + "\n")
    return record


def report(requested, reused):
    """Publish the outcome for the workflow step that gates the compile step.

    ``reused=true`` means every requested origin already has a shard, so the
    caller may skip its compile step.
    """
    record = os.environ.get("GITHUB_OUTPUT")
    if record:
        complete = bool(requested) and all(origin in reused for origin in requested)
        with open(record, "a") as output:
            output.write(f"reused={'true' if complete else 'false'}\n")
            output.write("origins=" + ",".join(reused) + "\n")


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--arch", required=True, choices=ARCHES)
    parser.add_argument("--version", required=True)
    parser.add_argument("--outdir", default="out")
    parser.add_argument("--repository-url")
    parser.add_argument("--package", action="append", choices=sorted(PACKAGE_NAMES),
                        help="restrict reuse to this origin (repeatable)")
    parser.add_argument("--list", action="store_true",
                        help="print reusable origins and exit without downloading")
    args = parser.parse_args()

    repository = repository_url(args.repository_url or os.environ.get("APK_REPOSITORY_URL"))
    requested = tuple(args.package or REUSABLE_ORIGINS)
    # An origin outside the reuse list is always compiled, so a caller can
    # unconditionally ask about the origin of its own compile job.
    origins = tuple(origin for origin in requested if origin in REUSABLE_ORIGINS)
    for origin in requested:
        if origin not in origins:
            print(f"Reusing nothing for {origin} on {args.arch}: "
                  "origin always compiles", flush=True)

    from builder import Builder
    args.package = None
    args.device = None
    args.release = False
    args.repository_url = repository
    args.workdir = Path(os.environ.get("UFLINUX_REUSE_WORKDIR", ".state/reuse")) / args.arch
    args.cache = Path(os.environ.get("UFLINUX_REUSE_CACHE", ".cache/build/reuse"))
    builder = Builder(args)
    try:
        available = plan(repository, args.arch, args.version, origins)
    except (urllib.error.URLError, OSError, ValueError) as error:
        # An unreachable or unreadable repository is not a build failure: the
        # caller compiles whatever this script did not reuse.
        print(f"Reusing nothing for {args.arch}: {error}", flush=True)
        available = {}
    if args.list:
        for origin in origins:
            if origin in available:
                print(origin)
        return

    reused = []
    for origin in origins:
        names = available.get(origin)
        if names is None:
            print(f"Reusing nothing for {origin} on {args.arch}: "
                  "no complete published package set", flush=True)
            continue
        builder.out = package_output(Path(args.outdir), args.arch, origin)
        builder.packages_dir = builder.out / "packages" / args.arch
        try:
            record = reuse(builder, origin, names, repository)
        except (Unavailable, urllib.error.URLError, OSError, ValueError) as error:
            shutil.rmtree(builder.out, ignore_errors=True)
            print(f"Reusing nothing for {origin} on {args.arch}: {error}", flush=True)
            continue
        reused.append(origin)
        count = sum(1 for name in record["files"] if name.endswith(".apk"))
        print(f"Reused {origin} for {args.arch} from {repository}: "
              f"{count} packages, {len(record['files'])} retained files", flush=True)
    report(requested, reused)


if __name__ == "__main__":
    main()
