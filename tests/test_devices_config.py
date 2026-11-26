"""Device targets must stay consistent with the build matrix and their boot paths."""
import json
from pathlib import Path
import unittest

root = Path(__file__).resolve().parents[1]
devices = json.loads((root / "config/devices.json").read_text())
arches = json.loads((root / "config/architectures.json").read_text())
lock = json.loads((root / "config/devices.lock.json").read_text())


class DevicesConfigTests(unittest.TestCase):
    def test_expected_targets_are_present(self):
        self.assertEqual(sorted(devices),
                         ["pc-f2fs", "pc-xfs", "qcs6490", "rpi", "spacemit-k1", "vm"])

    def test_every_target_uses_a_supported_architecture(self):
        for device_id, device in devices.items():
            self.assertTrue(device["arches"], device_id)
            self.assertTrue(set(device["arches"]) <= set(arches), device_id)

    def test_kernel_flavour_matches_the_boot_path(self):
        for device_id, device in devices.items():
            if device["boot"] == "rpi":
                self.assertEqual(device["kernel"], "rpi", device_id)
            if device["boot"] == "extlinux":
                self.assertEqual(device["arches"], ["riscv64"], device_id)
            if device["boot"] == "pc":
                # The dual-firmware image targets PC hardware
                # classes, and gptmbr.bin needs a kernel the loader can chainload
                # from the ESP, so the PC targets stay on the generic flavour.
                self.assertEqual(device["kernel"], "x86_64", device_id)
                self.assertTrue(set(device["arches"]) == {"x86_64"}, device_id)

    def test_device_kernel_is_required_for_every_architecture(self):
        kernels = json.loads((root / "config/kernels.json").read_text())
        for device in devices.values():
            for arch in device["arches"]:
                self.assertIn(device["kernel"].format(arch=arch), kernels["targets"][arch])

    def test_the_kernel_release_is_not_pinned(self):
        # Like the Nura and apps indexes, the kernel moves on its own: the build
        # takes the newest release and checks each asset against the digest the
        # release server publishes with it, so no tag or hash belongs here.
        kernels = json.loads((root / "config/kernels.json").read_text())
        self.assertNotIn("tag", kernels)
        self.assertNotIn("packages", kernels)
        self.assertEqual(kernels["repository"], "UFlinux-project/kernel")

    def test_boards_needing_a_device_tree_declare_one(self):
        # extlinux loads the tree by path from extlinux.conf, and the qcs6490
        # GRUB entry names it explicitly, so both need the path in the config.
        # The Raspberry Pi firmware picks its own tree, so it needs none.
        for device_id, device in devices.items():
            if device["boot"] == "extlinux":
                self.assertTrue(device.get("dtb"), device_id)
        self.assertEqual(devices["qcs6490"]["dtb"], "qcom/qcs6490-radxa-dragon-q6a.dtb")

    def test_qcs6490_builtin_gpu_has_early_firmware(self):
        files = set(devices["qcs6490"]["initramfs_files"])
        self.assertTrue({"lib/firmware/qcom/a660_sqe.fw.zst",
                         "lib/firmware/qcom/a660_gmu.bin.zst",
                         "lib/firmware/qcom/qcs6490/a660_zap.mbn.zst"} <= files)

    def test_serial_console_matches_the_architecture(self):
        for device_id, device in devices.items():
            for arch in device["arches"]:
                if arch == "aarch64" and device["boot"] != "efi":
                    self.assertEqual(device["serial"], "ttyAMA0", device_id)
                if arch == "riscv64":
                    self.assertEqual(device["serial"], "ttyS0", device_id)

    def test_only_qcom_uses_the_nura_repository(self):
        self.assertEqual([name for name, device in devices.items() if device["nura"]], ["qcs6490"])
        self.assertEqual(lock["nura"]["arches"], ["aarch64"])
        # The Nura index is not pinned by hash: postmarketOS rebuilds a release
        # branch's index regularly, so only the branch is a stable input.
        self.assertNotIn("index_sha256", lock["nura"])
        # The apps index is not pinned either: the repository is republished
        # whenever its packages are rebuilt, so only the URL is a stable input.
        apps = json.loads((root / "config/sources.lock.json").read_text())["apps"]
        self.assertNotIn("index_sha256", apps)

    def test_supported_targets_ship_a_bootloader_package(self):
        for device_id, device in devices.items():
            if device["status"] != "supported":
                continue
            if device["boot"] == "rpi":
                self.assertIn("raspberrypi-bootloader", device["packages"], device_id)
            elif device["boot"] == "efi":
                self.assertIn("grub-efi", device["packages"], device_id)
            elif device["boot"] == "pc":
                # One disk boots from either firmware, so both loaders have to
                # be installed for a dual-firmware GRUB image.
                self.assertIn("grub-bios", device["packages"], device_id)
                self.assertIn("grub-efi", device["packages"], device_id)

    def test_images_leave_room_for_the_root_filesystem(self):
        for device_id, device in devices.items():
            self.assertGreaterEqual(device["img_size_mib"], 4096, device_id)

    def test_boards_use_f2fs_and_virtual_machines_use_xfs(self):
        # f2fs suits the wear-levelling of SD cards and eMMC; a virtual disk has
        # no such constraint and matches the XFS of the installed ISO target.
        # The PC targets cover both media classes, so each states its own.
        for device_id, device in devices.items():
            if device_id in ("vm", "pc-xfs"):
                self.assertEqual(device["rootfs"], "xfs", device_id)
            else:
                self.assertEqual(device["rootfs"], "f2fs", device_id)

    def test_only_f2fs_targets_ask_for_compression(self):
        # The kernel refuses compress_algorithm= on a volume formatted without
        # the feature, so the option may only appear with an F2FS root.
        for device_id, device in devices.items():
            compression = device.get("f2fs_compress")
            if compression is None:
                continue
            self.assertEqual(device["rootfs"], "f2fs", device_id)
            self.assertIn(compression, ("lz4", "lzo"), device_id)


if __name__ == "__main__":
    unittest.main()
