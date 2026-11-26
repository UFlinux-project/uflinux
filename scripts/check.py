#!/usr/bin/env python3
"""Fast checks that run without root or an Alpine build environment."""
import ast
import hashlib
import json
from pathlib import Path
import re
import shlex
import subprocess
import sys
import tomllib
import xml.etree.ElementTree as ET

ROOT = Path(__file__).resolve().parents[1]
arches = json.loads((ROOT / "config/architectures.json").read_text())
lock = json.loads((ROOT / "config/sources.lock.json").read_text())
assert set(arches) == set(lock["rootfs"])
assert set(arches) == {"x86_64", "aarch64", "ppc64le", "riscv64"}
for architecture, config in arches.items():
    jobs = config["jobs"]
    assert isinstance(jobs, int) and not isinstance(jobs, bool) and jobs > 0, \
        f"{architecture} needs a positive integer jobs count"
kernels = json.loads((ROOT / "config/kernels.json").read_text())
# The kernel release itself is not pinned: the newest one is fetched at build
# time and checked against the digest GitHub publishes with each asset. What the
# configuration does pin is the repository, the signing key and the target list
# every release has to carry.
assert re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", kernels["repository"]), \
    "kernels.json must name the kernel repository as owner/repo"
assert re.fullmatch(r"[0-9a-f]{64}", kernels["key"]["sha256"]), \
    "The kernel signing key needs its SHA-256"
assert set(kernels["targets"]) <= set(arches), "Kernel targets list an unknown architecture"
for arch, targets in kernels["targets"].items():
    assert targets and len(set(targets)) == len(targets), f"{arch} lists kernel targets twice"
    for target in targets:
        assert re.fullmatch(r"[a-z0-9][a-z0-9_-]*", target), f"Kernel target must be a lowercase slug: {target}"
devices = json.loads((ROOT / "config/devices.json").read_text())
device_lock = json.loads((ROOT / "config/devices.lock.json").read_text())
assert device_lock["schema"] == 1
for device_id, device in devices.items():
    assert re.fullmatch(r"[a-z0-9][a-z0-9-]*", device_id), f"Device id must be a lowercase slug: {device_id}"
    assert device["label"], f"{device_id} needs a label"
    assert device["arches"] and set(device["arches"]) <= set(arches), f"{device_id} lists an unknown architecture"
    for arch in device["arches"]:
        assert arch in kernels["targets"], f"{device_id} builds {arch}, which has no kernel targets"
        assert device["kernel"].format(arch=arch) in kernels["targets"][arch], \
            f"{device_id} needs a kernel target for {arch}"
    assert device["boot"] in {"efi", "rpi", "extlinux", "pc"}, f"{device_id} must use a known boot mode"
    if device["boot"] == "extlinux":
        assert re.fullmatch(r"[a-z0-9][a-z0-9._/-]*\.dtb", device.get("dtb", "")), \
            f"{device_id} boots with extlinux and needs the dtb path it loads"
    if device["boot"] == "pc":
        assert set(device["arches"]) == {"x86_64"}, \
            f"{device_id} requires x86_64 for PC firmware"
        assert device["kernel"] == "x86_64", \
            f"{device_id} boots a PC kernel and needs the generic x86_64 flavour"
        assert not device.get("dtb"), f"{device_id} boots PC firmware and cannot use a device tree"
    if device.get("dtb"):
        assert re.fullmatch(r"[a-z0-9][a-z0-9._/-]*\.dtb", device["dtb"]), \
            f"{device_id} needs a device tree path relative to the boot partition's dtbs directory"
    assert device["status"] in {"supported", "experimental"}, f"{device_id} must declare a release status"
    assert device["rootfs"] in {"f2fs", "xfs"}, f"{device_id} must use a root filesystem the device stage can build"
    # The algorithm reaches the kernel as a mount option, and the kernel only
    # implements these two for on-disk cluster compression; zstd and lz4hc are
    # write-only algorithms there, so a target must not name them.
    assert device.get("f2fs_compress") in (None, "lz4", "lzo"), \
        f"{device_id} must compress its f2fs root with an algorithm the kernel can read back"
    if device.get("f2fs_compress"):
        assert device["rootfs"] == "f2fs", f"{device_id} asks for f2fs compression without an f2fs root"
    assert device.get("initramfs_compression", "zstd") in {"gzip", "zstd"}, \
        f"{device_id} must use a supported initramfs compression"
    assert device["packages"] and all(re.fullmatch(r"[a-z0-9][a-z0-9+.-]*", p) for p in device["packages"]), \
        f"{device_id} needs a list of Alpine package names"
    assert isinstance(device["img_size_mib"], int) and device["img_size_mib"] > 512, f"{device_id} needs a usable image size"
    assert re.fullmatch(r"tty[A-Z]*[0-9]+", device["serial"]), f"{device_id} needs a serial console device"
    assert isinstance(device["nura"], bool), f"{device_id} must state whether the Nura repository is used"
    # mkinitfs copies custom_files verbatim from the rootfs root, so the paths are
    # root-relative and must not be able to escape it.
    assert all(re.fullmatch(r"(usr/)?(lib|share)/[a-z0-9][a-z0-9._/-]*", path)
               for path in device.get("initramfs_files", [])), \
        f"{device_id} lists an initramfs file that is not a root-relative library path"
    assert device["nura_packages"] == [] or device["nura"], f"{device_id} lists Nura packages without enabling the repository"
nura = device_lock["nura"]
assert re.fullmatch(r"v[0-9]{2}\.[0-9]{2}|main", nura["branch"]), "Nura must be pinned to a release branch"
assert nura["mirror"].startswith("https://") and not nura["mirror"].endswith("/")
assert re.fullmatch(r"[0-9a-f]{64}", nura["repository_key_sha256"])
assert set(nura["arches"]) <= set(arches)
# The Nura index is deliberately not pinned by hash: postmarketOS rebuilds a
# release branch's index regularly, so device packages are checked against the
# live index of the pinned branch instead (see scripts/check-packages.py).
assert "index_sha256" not in nura, "The Nura repository index must not be pinned by sha256"
# Every image uses the apps repository for the file manager.
apps = lock["apps"]
assert apps["url"].startswith("https://") and not apps["url"].endswith("/")
assert apps["repository_key"] == apps["url"] + "/uflinux3.ec384.pub"
assert re.fullmatch(r"[0-9a-f]{64}", apps["repository_key_sha256"])
assert set(apps["arches"]) == set(arches)
# Like the Nura suite, the apps index is deliberately not pinned by hash: the
# repository is republished whenever its packages are rebuilt, so the URL is the
# stable input and uflinux-files is checked against the live index instead (see
# scripts/check-packages.py).
assert "index_sha256" not in apps, "The apps repository index must not be pinned by sha256"
# uflinux-release ships the copy of the key that apk trusts on disk, so it has to
# be byte-identical to the key published next to the repository index.
assert hashlib.sha256((ROOT / "aports/uflinux/uflinux3.ec384.pub").read_bytes()).hexdigest() \
    == apps["repository_key_sha256"], "The bundled apps key differs from the published one"
for device_id, device in devices.items():
    if device["nura"]:
        assert set(device["arches"]) <= set(nura["arches"]), f"{device_id} uses Nura on an unpinned architecture"
for path in [*ROOT.glob("scripts/*.py"), *ROOT.glob("tests/*.py"), ROOT / "aports/uflinux/uflinux-install", ROOT / "aports/uflinux/uflinux-install-gui-backend", ROOT / "aports/uflinux/uflinux-boot-assets.py"]:
    ast.parse(path.read_text(), filename=str(path))
for path in [*ROOT.glob("scripts/*.sh"), *ROOT.glob("tests/*.sh"), *ROOT.glob("profiles/*.sh"), *ROOT.glob("rds/scripts/*.sh"), *ROOT.glob("aports/*/APKBUILD"),
             *[ROOT / "aports/uflinux" / n for n in ("uflinux-session", "uflinux-autostart", "uflinux-live.initd", "uflinux-image-user", "uflinux-compositor", "uflinux-network-init", "uflinux-bootsplash", "uflinux-splash.initd", "uflinux-splash-stop.initd", "uflinux-device.initd", "uflinux-zram.initd", "uflinux-prompt.sh")], *ROOT.glob("settings/data/*.initd"), ROOT / "settings/data/uflinux-power-button"]:
    subprocess.run(["sh", "-n", str(path)], check=True)
for path in (ROOT / "aports/uflinux").glob("*.xml"):
    ET.parse(path)
for path in (ROOT / "aports/uflinux").glob("*.svg"):
    ET.parse(path)
for path in (ROOT / "settings/data").iterdir():
    if path.suffix in {"svg", "policy", "conf"} and path.read_text().lstrip().startswith("<"):
        ET.parse(path)
artwork = json.loads((ROOT / "config/artwork.lock.json").read_text())
for name in artwork["logo"]["copies"]:
    assert hashlib.sha512((ROOT / name).read_bytes()).hexdigest() == artwork["logo"]["sha512"], "Imported logo differs from its pinned original"
for name in artwork["logotype"]["copies"]:
    assert hashlib.sha512((ROOT / name).read_bytes()).hexdigest() == artwork["logotype"]["sha512"], "Imported boot logotype differs from its pinned original"
assert (ROOT / "LICENSE").read_bytes() == (ROOT / "aports/uflinux/LICENSE").read_bytes()
for name in ("greetd-installed.toml", "greetd-live.toml", "regreet.toml"):
    tomllib.loads((ROOT / "aports/uflinux" / name).read_text())
subprocess.run([sys.executable, str(ROOT / "scripts/update-checksums.py"), "--check"], check=True)
subprocess.run([sys.executable, str(ROOT / "scripts/settings-source.py"), "--check"], check=True)
connman=(ROOT / "aports/connman/APKBUILD").read_text()
assert lock["sources"]["connman"]["sha512"] in connman
for name,digest in lock["connman"]["patches"].items():
    assert hashlib.sha512((ROOT / "aports/connman" / name).read_bytes()).hexdigest()==digest
regreet = (ROOT / "aports/uflinux-regreet/APKBUILD").read_text()
assert lock["sources"]["uflinux-regreet"]["sha512"] in regreet
for name, digest in lock["uflinux-regreet"]["patches"].items():
    assert hashlib.sha512((ROOT / "aports/uflinux-regreet" / name).read_bytes()).hexdigest() == digest
recorder = (ROOT / "aports/gpu-screen-recorder/APKBUILD").read_text()
assert lock["sources"]["gpu-screen-recorder"]["sha512"] in recorder
for name, digest in lock["gpu-screen-recorder"]["patches"].items():
    assert hashlib.sha512((ROOT / "aports/gpu-screen-recorder" / name).read_bytes()).hexdigest() == digest
# The Qualcomm Venus capture alignment has to stay in the patch that carries it,
# and that patch has to be applied: without it the Iris encoder on qcs6490 shows
# the padding artefact.
venus = (ROOT / "aports/gpu-screen-recorder/0010-Encoder-align-v4l2-m2m-capture-for-Qualcomm-venus.patch").read_text()
assert "VENUS_WIDTH_ALIGNMENT 128" in venus and "VENUS_HEIGHT_ALIGNMENT 32" in venus
assert "0010-Encoder-align-v4l2-m2m-capture-for-Qualcomm-venus.patch" in recorder
# Every app has to get all three window buttons. GTK reads gtk-decoration-layout
# from the XDG system config dirs and from the sysconfdir, so the settings file
# must be installed under both /etc/xdg and /etc for each toolkit; if either
# copy goes missing, affected apps fall back to the built-in "menu:close".
uflinux_apkbuild = (ROOT / "aports/uflinux/APKBUILD").read_text()
for path in ("/etc/xdg/gtk-3.0/settings.ini", "/etc/xdg/gtk-4.0/settings.ini",
             "/etc/gtk-3.0/settings.ini", "/etc/gtk-4.0/settings.ini"):
    assert f'install -Dm644 "$srcdir/gtk-settings.ini" "$pkgdir{path}"' in uflinux_apkbuild, \
        f"gtk-settings.ini must be installed to {path}"
assert 'install -Dm644 "$srcdir/90-uflinux.gschema.override"' in uflinux_apkbuild
# The prompt ships as a profile.d drop-in that /etc/profile sources last, and
# the desktop exports ENV so a non-login ash gets it too. If the drop-in is
# renamed in only one of the three places, terminals silently keep the
# upstream "host:dir # " prompt.
assert 'install -Dm644 "$srcdir/uflinux-prompt.sh" "$pkgdir/etc/profile.d/30-uflinux-prompt.sh"' in uflinux_apkbuild
assert "etc/profile.d/30-uflinux-prompt.sh" in uflinux_apkbuild, \
    "uflinux-defaults must claim the profile.d prompt drop-in"
for env_file in ("uflinux-session", "labwc-environment"):
    assert "ENV=/etc/profile.d/30-uflinux-prompt.sh" in (ROOT / "aports/uflinux" / env_file).read_text(), \
        f"{env_file} must export ENV so interactive ash loads the prompt"
# The tuning is only live if the split package that every image installs owns
# the drop-ins: the OpenRC sysctl and modules services apply /usr/lib/sysctl.d
# and /usr/lib/modules-load.d, and udev applies /usr/lib/udev/rules.d. Dropping
# a path from the amove list ships the file in the umbrella package instead,
# where no profile installs it, and the tuning silently stops applying.
for path in ("/usr/lib/sysctl.d/60-uflinux.conf", "/usr/lib/modules-load.d/uflinux-fq-codel.conf",
             "/usr/lib/udev/rules.d/60-uflinux-bfq.rules"):
    assert f'"$pkgdir{path}"' in uflinux_apkbuild, \
        f"the tuning drop-in must be installed to {path}"
for claimed in ("usr/lib/sysctl.d", "usr/lib/modules-load.d", "usr/lib/udev/rules.d"):
    assert claimed in uflinux_apkbuild.split("defaults() {", 1)[1].split("\n}", 1)[0], \
        f"uflinux-defaults must claim {claimed}"
# fq_codel is a module in the UFLinux kernel, so the qdisc default is inert
# without the load list; and the congestion control must stay unset so the
# kernel's BBRv3 default survives.
sysctl = (ROOT / "aports/uflinux/uflinux-sysctl.conf").read_text()
assert "net.core.default_qdisc = fq_codel" in sysctl
assert "net.ipv4.tcp_congestion_control =" not in sysctl, \
    "Setting the congestion control here would override the kernel's BBRv3 default"
assert (ROOT / "aports/uflinux/uflinux-fq-codel.conf").read_text().split() == ["sch_fq_codel"]
# The drop-ins are inert unless the services that read them start, so both the
# live overlay and the installed/device rootfs have to enable them at boot.
assert "'modules boot'" in (ROOT / "profiles/genapkovl-uflinux.sh").read_text()
assert "'sysctl boot'" in (ROOT / "profiles/genapkovl-uflinux.sh").read_text()
device_rootfs = (ROOT / "profiles/device-rootfs.sh").read_text()
assert "modules sysctl" in device_rootfs, \
    "device rootfs must enable modules and sysctl at boot for the tuning to apply"
# Settings owns the search field in its header bar, so it cannot rely on the
# environment for its buttons either: a theme or window manager that ships the
# built-in "menu:close" layout would hide minimize and maximize. The window pins
# the same layout the desktop installs above.
settings_app = (ROOT / "settings/ui/src/app.rs").read_text()
assert 'header.set_decoration_layout(Some("icon:minimize,maximize,close"))' in settings_app, \
    "The Settings header bar must pin all three window buttons"
assert "set_show_title_buttons(true)" in settings_app, \
    "The Settings header bar must show its window buttons"
portal = (ROOT / "aports/xdg-desktop-portal-gtk4/APKBUILD").read_text()
portal_source = lock["sources"]["xdg-desktop-portal-gtk4"]
assert portal_source["sha512"] in portal
assert f'_commit={portal_source["url"].rsplit("/", 1)[1]}' in portal
assert f'pkgver={lock["xdg-desktop-portal-gtk4"]["version"]}\n' in portal
for name, digest in lock["xdg-desktop-portal-gtk4"]["patches"].items():
    assert digest + "  " + name in portal
    assert hashlib.sha512((ROOT / "aports/xdg-desktop-portal-gtk4" / name).read_bytes()).hexdigest() == digest
cursors = (ROOT / "aports/simp1e-cursors/APKBUILD").read_text()
for source in ("simp1e-cursors", "cursor-generator"):
    assert lock["sources"][source]["sha512"] in cursors, f"simp1e-cursors recipe must pin {source}"
assert f'pkgver={lock["simp1e-cursors"]["version"]}\n' in cursors
# The vendored recipe only builds the desktop default and its base theme.
assert "--include=Simp1e.txt,Simp1e-Adw-Dark.txt,cursors.svg" in cursors
assert 'subpackages="$pkgname-adw-dark:adw_dark"' in cursors
# abuild only adds an automatic dependency on the main package for the suffixes
# it knows about, so the variant has to ask for the base scheme itself. Without
# it the base package is published but never reaches an image, and the publish
# gate refuses the release.
assert 'depends="$pkgname=$pkgver-r$pkgrel"' in cursors.split("adw_dark() {", 1)[1]
# The generator sanitizes scheme names, so the built directory is hyphenated.
assert 'mv "$pkgdir"/usr/share/icons/Simp1e-Adw-Dark ' in cursors
assert "libadwaita" not in (ROOT / "settings/Cargo.lock").read_text()
musl = (ROOT / "aports/musl/APKBUILD").read_text()
for variable, key in (("pkgver", "version"), ("pkgrel", "revision"), ("_upstreamver", "upstream_version"),
                      ("_mimallocver", "mimalloc_version"),
                      ("_glibc_stringver", "glibc_string_version")):
    assert f'{variable}={lock["musl"][key]}\n' in musl, f"musl {variable} differs from the source lock"
mimalloc = lock["sources"]["mimalloc"]
assert re.fullmatch(r"[0-9a-f]{40}", mimalloc["commit"]), "mimalloc must pin a full Git commit"
assert mimalloc["sha512"] in musl, "musl must verify the locked mimalloc archive"
glibc_string = lock["sources"]["glibc-string"]
assert glibc_string["sha512"] in musl, "musl must verify the locked glibc-string archive"
assert glibc_string["url"].replace(lock["musl"]["glibc_string_version"], "$_glibc_stringver") in musl, (
    "musl glibc-string source URL differs from the source lock"
)
for origin in ("musl", "connman", "gpu-screen-recorder", "uflinux-regreet", "uflinux-settings", "xdg-desktop-portal-gtk4"):
    recipe_arches = re.search(r'^arch="([^"]+)"$', (ROOT / "aports" / origin / "APKBUILD").read_text(), re.M)
    assert recipe_arches and set(recipe_arches.group(1).split()) == set(arches), f"{origin} architecture list differs from build matrix"
for name, digest in lock["image"]["patches"].items():
    assert hashlib.sha512((ROOT / "patches" / name).read_bytes()).hexdigest() == digest
profile=tomllib.loads((ROOT / "settings/Cargo.toml").read_text())["profile"]["release"]
recipe=(ROOT / "aports/uflinux-settings/APKBUILD").read_text()
for setting in ("codegen-units", "panic"):
    variable="CARGO_PROFILE_RELEASE_"+setting.upper().replace("-","_")
    assert f"export {variable}={profile[setting]}" in recipe, f"abuild must enforce {variable}"
assert "export CARGO_PROFILE_RELEASE_OPT_LEVEL=s" in recipe, "Settings must build with -Os"
assert re.search(r"^export CARGO_PROFILE_RELEASE_LTO=\S+", recipe, re.M), "abuild must set LTO"
for origin in ("uflinux-regreet",):
    assert "export CARGO_PROFILE_RELEASE_OPT_LEVEL=s" in (ROOT / "aports" / origin / "APKBUILD").read_text(), \
        f"{origin} must build with -Os"
for origin in ("connman", "gpu-screen-recorder"):
    recipe = (ROOT / "aports" / origin / "APKBUILD").read_text()
    for flag in ("clang-22", "-O3", "-flto=full", "-fprofile-generate", "-fprofile-use=", "llvm-bolt"):
        assert flag in recipe, f"{origin} recipe is missing {flag}"
assert "debug" not in profile, "Settings release profile must use Cargo's default debug level"
for origin in ("uflinux-settings",):
    recipe = (ROOT / "aports" / origin / "APKBUILD").read_text()
    assert "unset CARGO_PROFILE_RELEASE_DEBUG" in recipe, f"{origin} must clear abuild's debug override"
for path in (ROOT / ".github/workflows").glob("*.yml"):
    for action in re.findall(r"uses:\s*([^\s#]+)", path.read_text()):
        if not action.startswith("./"):
            assert re.fullmatch(r"[\w/-]+@[0-9a-f]{40}", action), f"Unpinned action: {action}"
subprocess.run([sys.executable, "-m", "unittest", "discover", "-s", "tests", "-p", "test_*.py"], cwd=ROOT, check=True)
print("Static checks passed")
