"""Device disk images are laid out, described and checked deterministically."""
import argparse
import contextlib
import errno
import gzip
import io
import json
from pathlib import Path
import os
import struct
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
import builder
from builder import (DEVICES, device_environment, device_image_name, device_nura_repository,
                     device_partitions, device_partition_script, device_packages,
                     device_repositories, device_target, device_uuids, device_verify_image)


class DeviceLayoutTests(unittest.TestCase):
    def test_geometry_is_sector_aligned(self):
        layout = device_partitions(6144)
        self.assertEqual(layout["esp_start"], builder.ESP_START)
        self.assertEqual(layout["root_start"], builder.ESP_START + builder.ESP_SECTORS)
        self.assertEqual(layout["total"], 6144 * 2048)
        self.assertEqual(layout["esp_size"], builder.ESP_SECTORS)
        # A 1 MiB backup GPT is reserved at the end of the disk.
        self.assertEqual(layout["root_size"], layout["total"] - layout["root_start"] - 2048)
        self.assertLess(layout["root_start"] + layout["root_size"], layout["total"])
        with self.assertRaises(ValueError):
            device_partitions(256)

    def test_uuids_are_stable_and_well_formed(self):
        first = device_uuids("0.1.0:rpi:aarch64")
        self.assertEqual(first, device_uuids("0.1.0:rpi:aarch64"))
        self.assertNotEqual(first["disk"], device_uuids("0.1.0:vm:aarch64")["disk"])
        for role in ("disk", "esp", "root"):
            value = first[role]
            self.assertEqual(len(value), 36, role)
            self.assertEqual(value, value.upper(), role)
            self.assertEqual([len(part) for part in value.split("-")], [8, 4, 4, 4, 12], role)
        self.assertEqual(len(first["serial"]), 8)
        self.assertEqual(first["serial"], first["serial"].lower())

    def test_partition_script_pins_guids_and_types(self):
        uuids = device_uuids("0.1.0:rpi:aarch64")
        script = device_partition_script(device_partitions(6144), uuids)
        self.assertIn("label: gpt", script)
        self.assertIn("label-id: " + uuids["disk"], script)
        self.assertIn("type=" + builder.GPT_ESP_TYPE, script)
        self.assertIn("type=" + builder.GPT_LINUX_TYPE, script)
        self.assertIn("uuid=" + uuids["esp"], script)
        self.assertIn("uuid=" + uuids["root"], script)
        self.assertIn("name=ESP", script)
        pc = device_partition_script(device_partitions(6144, bios=True), uuids, bios=True)
        self.assertIn("type=" + builder.GPT_BIOS_TYPE, pc)
        self.assertIn("name=ROOT", pc)

    def test_image_names_match_the_release_assets(self):
        self.assertEqual(device_image_name("0.1.0", "rpi", "aarch64"),
                         "uflinux-0.1.0-rpi-aarch64.img.zst")

    def test_architecture_without_a_target_is_rejected(self):
        self.assertIs(device_target(DEVICES, "rpi", "aarch64"), DEVICES["rpi"])
        with self.assertRaises(ValueError):
            device_target(DEVICES, "rpi", "x86_64")
        with self.assertRaises(ValueError):
            device_target(DEVICES, "spacemit-k1", "aarch64")


class DeviceRootfsTests(unittest.TestCase):
    def test_root_snapshot_preserves_mountpoints_without_build_mount_contents(self):
        with tempfile.TemporaryDirectory() as temporary:
            source = Path(temporary) / "source"
            destination = Path(temporary) / "rootfs"
            for name in ("dev", "proc", "sys", "run", "project"):
                (source / name).mkdir(parents=True)
                (source / name / "build-only").write_text("must not ship")
            (source / "etc").mkdir()
            (source / "etc/hostname").write_text("uflinux-test\n")
            with patch.dict(os.environ, {"TAR_OPTIONS": ""}):
                builder.device_snapshot(source, destination, rootfs=True)
            for name in ("dev", "proc", "sys", "run"):
                self.assertTrue((destination / name).is_dir(), name)
                self.assertEqual(list((destination / name).iterdir()), [], name)
                self.assertEqual((destination / name).stat().st_mode & 0o777, 0o755)
            self.assertFalse((destination / "project").exists())
            self.assertEqual((destination / "etc/hostname").read_text(), "uflinux-test\n")

    def test_root_snapshot_preserves_file_capabilities(self):
        with tempfile.TemporaryDirectory() as temporary:
            source = Path(temporary) / "source"
            source.mkdir()
            helper = source / "helper"
            helper.touch()
            # Linux v2 file capability with CAP_NET_BIND_SERVICE permitted/effective.
            capability = struct.pack("<IIIII", 0x02000001, 1 << 10, 0, 0, 0)
            try:
                os.setxattr(helper, "security.capability", capability)
            except OSError as error:
                if error.errno in (errno.EPERM, errno.EACCES, errno.ENOTSUP):
                    self.skipTest("test requires a filesystem and user with file capability support")
                raise
            destination = Path(temporary) / "rootfs"
            builder.device_snapshot(source, destination, rootfs=True)
            self.assertEqual(os.getxattr(destination / helper.name, "security.capability"),
                             os.getxattr(helper, "security.capability"))

    def test_esp_snapshot_does_not_gain_root_mountpoints(self):
        with tempfile.TemporaryDirectory() as temporary:
            source = Path(temporary) / "esp-source"
            (source / "boot").mkdir(parents=True)
            (source / "boot/vmlinuz").write_bytes(b"kernel")
            destination = Path(temporary) / "esp"
            builder.device_snapshot(source, destination)
            self.assertEqual(sorted(path.name for path in destination.iterdir()), ["boot"])

    def test_build_uses_the_copied_signed_repository(self):
        lines = device_repositories("/repo", "aarch64").split()
        self.assertEqual(lines[0], "/repo")
        self.assertEqual(len(lines), 5, lines)
        self.assertTrue(all(line in ("/repo", "/repo/aarch64/Packages.adb") or line.startswith("https://") for line in lines),
                        lines)

    def test_shipped_image_points_at_the_published_repository(self):
        lines = device_repositories("https://example.invalid/uflinux/0.1.0", "aarch64").split()
        self.assertEqual(lines[0], "https://example.invalid/uflinux/0.1.0")
        self.assertTrue(all(line.startswith("https://") for line in lines), lines)
        self.assertEqual(len(lines), 5, lines)

    def test_nura_suite_stays_out_of_the_repositories_file(self):
        # A visible Nura line makes apk's solver unresolvable for any install
        # that pulls in a kernel (postmarketos-mkinitfs vs mkinitfs), so the
        # file only ever lists the UFLinux repo, Alpine and the apps repository.
        # The profile reaches Nura through apk fetch --repository instead.
        lines = device_repositories("/repo", "aarch64").split()
        self.assertEqual(len(lines), 5, lines)
        self.assertNotIn("nura", " ".join(lines))
        self.assertEqual(device_nura_repository(),
                         "https://mirror.nura.eco/postmarketos/v26.06")

    def test_apps_repository_is_listed_for_every_supported_architecture(self):
        for arch in builder.ARCHES:
            with self.subTest(arch=arch):
                lines = device_repositories("/repo", arch).split()
                self.assertIn(builder.LOCK["apps"]["url"], lines)

    def test_shared_desktop_and_boot_tools_are_installed(self):
        packages = device_packages(DEVICES["vm"], "x86_64")
        for required in ("uflinux-desktop-meta", "uflinux-device", "alpine-base", "zstd",
                         "e2fsprogs-extra", "cloud-utils-growpart", "linux-uflinux-virt-x86_64=7.3.6-r14",
                         "musl-locales", "labwc-lang", "firefox", "vulkan-loader"):
            self.assertIn(required, packages)
        self.assertNotIn("uflinux-live", packages)
        self.assertEqual(packages.count("efibootmgr"), 1)
        # dtbloader and the Nura kernel reach the image through
        # UFLINUX_NURA_PACKAGES (fetched and unpacked by the profile), never
        # through the installed package set.
        # Alpine has no riscv64 firefox, so that architecture gets the Extended
        # Support Release instead, without the ublock-origin xpi that only
        # installs into /usr/lib/firefox.
        riscv = device_packages(DEVICES["spacemit-k1"], "riscv64")
        self.assertIn("firefox-esr", riscv)
        self.assertNotIn("firefox", riscv)
        self.assertNotIn("ublock-origin", riscv)
        qcom_packages = device_packages(DEVICES["qcs6490"], "aarch64")
        self.assertIn("firefox", qcom_packages)
        self.assertIn("ublock-origin", qcom_packages)
        self.assertNotIn("dtbloader", qcom_packages)
        self.assertNotIn("linux-postmarketos-qcom-sc7280", qcom_packages)
        self.assertIn("linux-uflinux-dragon-q6a=7.0.11-r14", qcom_packages)
        self.assertNotIn("linux-lts", qcom_packages)
        self.assertNotIn("efibootmgr", device_packages(DEVICES["spacemit-k1"], "riscv64"))

    def test_only_f2fs_devices_pull_in_the_f2fs_tools(self):
        # F2FS boards carry tools for offline maintenance; the XFS vm image
        # already carries xfsprogs for the shared desktop set.
        for name in ("qcs6490", "rpi", "spacemit-k1", "pc-f2fs"):
            arch = DEVICES[name]["arches"][0]
            self.assertIn("f2fs-tools", device_packages(DEVICES[name], arch), name)
        for name, arch in (("vm", "x86_64"), ("pc-xfs", "x86_64")):
            packages = device_packages(DEVICES[name], arch)
            self.assertNotIn("f2fs-tools", packages, name)
            self.assertIn("xfsprogs", packages, name)

    def test_environment_covers_the_profile_contract(self):
        env = device_environment("rpi", DEVICES["rpi"], "aarch64", "0.1.0",
                                 "01234567-89AB-4CDE-8F01-23456789ABCD", "/device-esp")
        self.assertEqual(env["UFLINUX_DEVICE"], "rpi")
        self.assertEqual(env["UFLINUX_BOOT"], "rpi")
        self.assertEqual(env["UFLINUX_SERIAL"], "ttyAMA0")
        self.assertEqual(env["UFLINUX_ROOT_PARTUUID"],
                         "01234567-89AB-4CDE-8F01-23456789ABCD")
        self.assertEqual(env["UFLINUX_ESP"], "/device-esp")
        self.assertEqual(env["UFLINUX_ROOTFS"], "f2fs")
        self.assertEqual(env["UFLINUX_INITRAMFS_COMPRESSION"], "zstd")
        self.assertNotIn("UFLINUX_DTB", env)
        self.assertNotIn("UFLINUX_NURA_PACKAGES", env)
        self.assertNotIn("UFLINUX_INITRAMFS_FILES", env)
        self.assertNotIn("UFLINUX_INITRAMFS_MODULES", env)
        qcom = device_environment("qcs6490", DEVICES["qcs6490"], "aarch64", "0.1.0",
                                  "01234567-89AB-4CDE-8F01-23456789ABCD", "/device-esp")
        self.assertEqual(qcom["UFLINUX_NURA_PACKAGES"],
                         "dtbloader")
        self.assertEqual(qcom["UFLINUX_NURA_REPOSITORY"], device_nura_repository())
        self.assertEqual(qcom["UFLINUX_DTBLOADER"], "/usr/share/dtbloader/dtbloader.efi")
        # The actual Nura 7.0.8 kernel enables RD_GZIP but disables RD_ZSTD.
        self.assertEqual(qcom["UFLINUX_INITRAMFS_COMPRESSION"], "gzip")
        # systemd-boot is told about the board tree through its own variable, so
        # that the extlinux-only FDT path stays untouched.
        self.assertEqual(qcom["UFLINUX_DEVICETREE"], DEVICES["qcs6490"]["dtb"])
        self.assertNotIn("UFLINUX_DTB", qcom)
        # The QUP I2C driver is built into the Nura kernel, so its firmware has
        # no .ko for mkinitfs to find and must be named for custom_files.
        self.assertEqual(qcom["UFLINUX_INITRAMFS_FILES"],
                         " ".join(DEVICES["qcs6490"]["initramfs_files"]))
        # The HDMI bridge and connector are loadable modules that nothing else
        # loads before the root is mounted, so they have to be named too.
        self.assertEqual(qcom["UFLINUX_INITRAMFS_MODULES"],
                         " ".join(DEVICES["qcs6490"]["initramfs_modules"]))
        k1 = device_environment("spacemit-k1", DEVICES["spacemit-k1"], "riscv64", "0.1.0",
                                "01234567-89AB-4CDE-8F01-23456789ABCD", "/device-esp")
        self.assertEqual(k1["UFLINUX_DTB"], DEVICES["spacemit-k1"]["dtb"])
        self.assertEqual(k1["UFLINUX_SERIAL"], "ttyS0")
        vm = device_environment("vm", DEVICES["vm"], "x86_64", "0.1.0",
                                "01234567-89AB-4CDE-8F01-23456789ABCD", "/device-esp")
        self.assertEqual(vm["UFLINUX_ROOTFS"], "xfs")
        self.assertNotIn("UFLINUX_F2FS_COMPRESS", vm)
        # The compressed PC variant has to tell the profile which algorithm the
        # kernel is being told to mount with.
        flash = device_environment("pc-f2fs", DEVICES["pc-f2fs"], "x86_64", "0.1.0",
                                   "01234567-89AB-4CDE-8F01-23456789ABCD", "/device-esp")
        self.assertEqual(flash["UFLINUX_F2FS_COMPRESS"], DEVICES["pc-f2fs"]["f2fs_compress"])
        self.assertEqual(flash["UFLINUX_BOOT"], "pc")


class DevicePcBootTests(unittest.TestCase):
    """A PC disk image carries a BIOS chain and a UEFI chain on the same ESP."""

    # A bootstrap that differs from the zero-filled disk, so a missing or
    # unmoved MBR is detectable.
    bootstrap = bytes(range(256)) + bytes(184)

    def esp(self):
        return {"boot/vmlinuz": b"MZkern", "boot/initramfs": b"\x28\xb5\x2f\xfd",
                "boot/grub/grub.cfg": b"set default=0", "EFI/BOOT/BOOTX64.EFI": b"MZ"}

    def check(self, esp=None, rootfs="xfs", arch="x86_64", device=None, **kwargs):
        return DeviceImageCheckTests.report(
            self, "pc", self.esp() if esp is None else esp, rootfs=rootfs, arch=arch,
            device=device or DEVICES["pc-xfs"], **kwargs)

    def test_pc_disk_needs_grub_core_and_menu(self):
        self.assertEqual(self.check()["boot"], "pc")
        with self.assertRaisesRegex(ValueError, "GRUB MBR"):
            self.check(disk_mbr=bytes(440))
        with self.assertRaisesRegex(ValueError, "GRUB BIOS boot partition"):
            self.check(bios=False)
        esp = self.esp()
        del esp["boot/grub/grub.cfg"]
        with self.assertRaisesRegex(ValueError, "grub.cfg"):
            self.check(esp=esp)

    def test_f2fs_compression_needs_the_on_disk_features(self):
        # mkfs.f2fs only accepts -O compression together with extra_attr, and
        # the kernel refuses compress_algorithm= without both bits.
        both = builder.F2FS_FEATURE_COMPRESSION | builder.F2FS_FEATURE_EXTRA_ATTR
        report = self.check(rootfs="f2fs", device=DEVICES["pc-f2fs"], f2fs_features=both)
        self.assertEqual(report["device_image_format"], "gpt-f2fs")
        for missing in (0, builder.F2FS_FEATURE_COMPRESSION, builder.F2FS_FEATURE_EXTRA_ATTR):
            with self.subTest(features=missing):
                with self.assertRaisesRegex(ValueError, "compression features"):
                    self.check(rootfs="f2fs", device=DEVICES["pc-f2fs"], f2fs_features=missing)

    def test_an_uncompressed_f2fs_device_is_not_checked_for_the_features(self):
        report = DeviceImageCheckTests.report(
            self, "efi", {"boot/vmlinuz": b"MZkern",
                          "boot/initramfs": b"\x28\xb5\x2f\xfd" + b"x",
                          "EFI/BOOT/BOOTAA64.EFI": b"MZ", "boot/grub/grub.cfg": b"config"}, rootfs="f2fs")
        self.assertEqual(report["device_image_format"], "gpt-f2fs")

class DeviceImageCheckTests(unittest.TestCase):
    def report(self, boot, esp_files, initramfs=b"\x28\xb5\x2f\xfd" + b"initramfs",
               rootfs="f2fs", arch="aarch64", device=None, bios=True,
               disk_mbr=b"bootstrap", f2fs_features=None):
        """Run the checker against a synthetic disk whose ESP is faked."""
        uuids = device_uuids("0.1.0:check:" + boot)
        layout = device_partitions(1024, bios=boot == "pc")
        with tempfile.TemporaryDirectory() as temporary:
            image = Path(temporary) / "disk"
            offset, magic, _ = builder.ROOTFS_SIGNATURES[rootfs]
            body = b"\0" * offset + magic
            if f2fs_features is not None:
                # The feature bitmap sits past the magic, so pad up to it.
                pad = builder.F2FS_FEATURES_OFFSET - len(body)
                body += b"\0" * pad + int(f2fs_features).to_bytes(4, "little")
            image.write_bytes(b"\0" * (layout["root_start"] * builder.SECTOR) + body)
            if disk_mbr is not None:
                with image.open("r+b") as handle:
                    handle.write(disk_mbr)
            if boot == "pc":
                with image.open("r+b") as handle:
                    handle.seek(layout["bios_start"] * builder.SECTOR)
                    handle.write(b"core")
            stage = image.parent / (image.name + "-esp")

            def extract(source, destination, esp_start=builder.ESP_START):
                destination.mkdir(parents=True, exist_ok=True)
                for name, content in esp_files.items():
                    target = destination / name
                    target.parent.mkdir(parents=True, exist_ok=True)
                    target.write_bytes(content)

            def geometry(source):
                esp = {"start": layout["esp_start"], "size": layout["esp_size"],
                       "uuid": uuids["esp"]}
                return {"disk": uuids["disk"], "esp": esp,
                        "bios": {"start": layout["bios_start"], "type": builder.GPT_BIOS_TYPE} if bios else None,
                        "root": {"start": layout["root_start"], "size": layout["root_size"],
                                 "uuid": uuids["root"]}}

            saved_layout, saved_extract = builder.device_layout, builder.device_extract
            builder.device_layout, builder.device_extract = geometry, extract
            try:
                report = device_verify_image(image, device or {"boot": boot, "rootfs": rootfs},
                                             arch, uuids, layout)
            finally:
                builder.device_layout, builder.device_extract = saved_layout, saved_extract
            self.assertFalse(stage.exists(), "the ESP extraction must be cleaned up")
            return report

    def test_report_records_geometry_and_boot_payload(self):
        report = self.report("efi", {"boot/vmlinuz": b"MZkern", "boot/initramfs": b"\x28\xb5\x2f\xfd" + b"x",
                                     "EFI/BOOT/BOOTAA64.EFI": b"MZ", "boot/grub/grub.cfg": b"config"})
        self.assertEqual(report["device_image_format"], "gpt-f2fs")
        self.assertEqual(report["initramfs_compression"], "zstd")
        self.assertEqual(report["boot"], "efi")
        self.assertEqual(report["partitions"]["root"]["uuid"],
                         device_uuids("0.1.0:check:efi")["root"])
        self.assertEqual(report["vmlinuz_bytes"], len(b"kernel"))

    def test_extlinux_riscv64_verifies_without_an_efi_binary(self):
        # riscv64 has no entry in EFI_SUFFIX; the extlinux boot path must not
        # look it up (the spacemit-k1 job crashed on exactly that).
        report = self.report("extlinux", {"boot/vmlinuz": b"MZkern",
                                          "boot/initramfs": b"\x28\xb5\x2f\xfd" + b"x",
                                          "extlinux/extlinux.conf": b"LABEL linux\n",
                                          "dtbs/ky/x1_orangepi-rv2.dtb": b"dtb",
                                          "dtbs/ky/x1_orangepi-r2s.dtb": b"dtb"},
                             arch="riscv64", device=DEVICES["spacemit-k1"])
        self.assertEqual(report["boot"], "extlinux")
        self.assertEqual(report["arch"], "riscv64")

    def test_a_gzip_initramfs_is_rejected(self):
        with self.assertRaisesRegex(ValueError, "Zstandard"):
            self.report("efi", {"boot/vmlinuz": b"MZkern",
                                "boot/initramfs": b"\x1f\x8b\x08\x00gzip"})

    def test_qcs6490_accepts_gzip_and_reports_its_actual_compression(self):
        report = self.report("efi", {"boot/vmlinuz": b"MZkern",
                                     "boot/initramfs": gzip.compress(b"070701"),
                                     "EFI/BOOT/BOOTAA64.EFI": b"MZ", "boot/grub/grub.cfg": b"config",
                                     "dtbs/qcom/qcs6490-radxa-dragon-q6a.dtb": b"dtb"},
                             device=DEVICES["qcs6490"])
        self.assertEqual(report["initramfs_compression"], "gzip")

    def test_qcs6490_reports_an_initramfs_compression_mismatch(self):
        with self.assertRaisesRegex(ValueError, "gzip stream"):
            self.report("efi", {"boot/vmlinuz": b"MZkern",
                                "boot/initramfs": b"\x28\xb5\x2f\xfd" + b"x",
                                "EFI/BOOT/BOOTAA64.EFI": b"MZ", "boot/grub/grub.cfg": b"config"},
                        device=DEVICES["qcs6490"])

    def test_unknown_initramfs_compression_is_rejected(self):
        with self.assertRaisesRegex(ValueError, "unsupported initramfs compression"):
            self.report("efi", {}, device={"boot": "efi", "initramfs_compression": "xz"})

    def test_report_accepts_the_root_filesystem_of_the_device(self):
        esp = {"boot/vmlinuz": b"MZkern", "boot/initramfs": b"\x28\xb5\x2f\xfd" + b"x",
               "EFI/BOOT/BOOTAA64.EFI": b"MZ", "boot/grub/grub.cfg": b"config"}
        for rootfs in ("ext4", "f2fs", "xfs"):
            with self.subTest(rootfs=rootfs):
                report = self.report("efi", esp, rootfs=rootfs)
                self.assertEqual(report["device_image_format"], "gpt-" + rootfs)

    def test_a_root_partition_of_the_wrong_type_is_rejected(self):
        layout = device_partitions(1024)
        with tempfile.TemporaryDirectory() as temporary:
            image = Path(temporary) / "disk"
            image.write_bytes(b"\0" * (layout["root_start"] * builder.SECTOR + 4096))
            with image.open("r+b") as handle:
                handle.seek(layout["root_start"] * builder.SECTOR)
                handle.write(b"XFSB")
            # The image holds an XFS superblock, so an f2fs device must not pass.
            with self.assertRaisesRegex(ValueError, "not f2fs"):
                builder.device_verify_rootfs(image, layout["root_start"], "f2fs")
            builder.device_verify_rootfs(image, layout["root_start"], "xfs")

    def test_missing_boot_files_are_reported(self):
        with self.assertRaisesRegex(ValueError, "initramfs"):
            self.report("efi", {"boot/vmlinuz": b"MZkern"})
        with self.assertRaisesRegex(ValueError, "BOOTAA64.EFI"):
            self.report("efi", {"boot/vmlinuz": b"MZkern",
                                "boot/initramfs": b"\x28\xb5\x2f\xfd"})
        with self.assertRaisesRegex(ValueError, "cmdline.txt"):
            self.report("rpi", {"boot/vmlinuz": b"MZkern",
                                "boot/initramfs": b"\x28\xb5\x2f\xfd"})

    def test_geometry_drift_is_rejected(self):
        uuids = device_uuids("0.1.0:check:drift")
        layout = device_partitions(1024)
        with tempfile.TemporaryDirectory() as temporary:
            image = Path(temporary) / "disk"
            image.write_bytes(b"")
            saved = builder.device_layout
            builder.device_layout = lambda source: {
                "disk": uuids["disk"],
                "esp": {"start": layout["esp_start"] + 1, "uuid": uuids["esp"]},
                "root": {"start": layout["root_start"], "uuid": uuids["root"]}}
            try:
                with self.assertRaisesRegex(ValueError, "ESP starts at sector"):
                    device_verify_image(image, {"boot": "efi"}, "aarch64", uuids, layout)
            finally:
                builder.device_layout = saved

    def test_lock_pins_every_device_index(self):
        for name, device in DEVICES.items():
            if not device["nura"]:
                continue
            self.assertTrue(set(device["arches"]) <= set(builder.DEVICE_LOCK["nura"]["arches"]),
                            name)


class DeviceStageCliTests(unittest.TestCase):
    """The device stage is reachable only as --stage device --device <target>."""

    def rejected(self, *argv):
        with contextlib.redirect_stderr(io.StringIO()) as errors:
            with self.assertRaises(SystemExit) as caught:
                builder.main(list(argv))
        self.assertEqual(caught.exception.code, 2)
        return errors.getvalue()

    def test_device_requires_the_device_stage(self):
        self.assertIn("--device", self.rejected("--arch", "aarch64", "--stage", "image",
                                                "--device", "rpi"))

    def test_device_is_rejected_by_the_sign_stage(self):
        # The device job runs sign separately, without a board, so that the shared
        # repository is identical to the one the ISO job publishes.
        self.assertIn("--device", self.rejected("--arch", "aarch64", "--stage", "sign",
                                                "--device", "rpi"))

    def test_device_stage_requires_a_device(self):
        self.assertIn("--device", self.rejected("--arch", "aarch64", "--stage", "device"))

    def test_every_stage_reaches_a_builder_method(self):
        # The dispatcher looks stages up in STAGE_METHODS, so a stage without a
        # target only fails once CI runs it.
        for stage in ("bootstrap", "packages", "sign", "image", "device", "verify"):
            method = getattr(builder.Builder, builder.STAGE_METHODS.get(stage, stage), None)
            self.assertTrue(callable(method), stage)

    def test_device_must_support_the_architecture(self):
        self.assertIn("does not support x86_64",
                      self.rejected("--arch", "x86_64", "--stage", "device", "--device", "rpi"))

    def test_unknown_device_is_rejected(self):
        self.rejected("--arch", "aarch64", "--stage", "device", "--device", "bananapi")

    @unittest.skipIf(os.geteuid() == 0, "a root host would start a real build")
    def test_device_is_a_known_stage(self):
        # Past argument validation the builder insists on a root build host.
        stderr = self.rejected("--arch", "aarch64", "--stage", "device", "--device", "rpi")
        self.assertIn("build host", stderr)

    @unittest.skipIf(os.geteuid() == 0, "a root host would start a real build")
    def test_device_is_accepted_by_the_all_stage(self):
        stderr = self.rejected("--arch", "aarch64", "--stage", "all", "--device", "rpi")
        self.assertIn("build host", stderr)


if __name__ == "__main__":
    unittest.main()
