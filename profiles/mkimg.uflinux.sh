# shellcheck shell=sh disable=SC2034,SC3043
# These variables are consumed by the upstream mkimage plugin loader.
profile_uflinux() {
	profile_standard
	# Retain upstream boot support, but keep competing network/clock stacks and
	# the firmware metapackage off media: profile_standard asks for the whole
	# collection, which pins the Snapdragon-only linux-firmware-qcom.
	local selected="" package
	for package in $apks; do
		case "$package" in chrony|openntpd|dhcpcd|wpa_supplicant|network-extras|tiny-cloud-alpine|linux-lts|linux-firmware|linux-firmware-none) ;; *) selected="$selected $package" ;; esac
	done
	apks="$selected"
	title="UFLinux"
	desc="Minimal Alpine live desktop with labwc and the UFLinux desktop shell"
	image_name="uflinux"
	profile_abbrev="uflinux"
	hostname="uflinux"
	output_filename="uflinux-${RELEASE}-${ARCH}.iso"
	apkovl="/project/profiles/genapkovl-uflinux.sh"
	# Keep profile_standard's architecture-specific console and boot configuration.
	kernel_flavors="${UFLINUX_KERNEL_FLAVOR:?}"
	kernel_addons=""
	initfs_features="$initfs_features kms xfs uflinux-plymouth"
	# Alpine's firmware metapackage pins every vendor, so the list that keeps the
	# Snapdragon-only blobs off the release ISO is shared with the profile.
	initfs_cmdline="$initfs_cmdline rootflags=mode=0755,size=75%"
	# Boot straight past the loader menu with minimal console chatter; warnings
	# and errors still reach the serial log. Hold a key before the loader
	# initializes to select another entry in the GRUB menu.
	kernel_cmdline="$kernel_cmdline quiet"
	modloop_addons="$modloop_addons uflinux-initramfs"
	apks="$apks musl musl-utils uflinux uflinux-live uflinux-installer uflinux-installer-ui musl-locales musl-locales-lang glib-lang gtk4.0-lang labwc-lang $(grep -v '^#' /project/config/firmware-packages.txt)
		grub mkinitfs kmod xfsprogs e2fsprogs dosfstools util-linux vulkan-loader mesa-vulkan-ati"
	# Alpine builds firefox for every target architecture except riscv64, which
	# only gets firefox-esr. ublock-origin is a noarch xpi that installs into
	# /usr/lib/firefox/browser/extensions, so it ships with firefox only.
	if [ "$kernel_flavors" = lts ]; then apks="$apks linux-lts"; fi
	case "$ARCH" in
		x86_64|aarch64|ppc64le) apks="$apks firefox ublock-origin" ;;
		riscv64) apks="$apks firefox-esr" ;;
	esac
	case "$ARCH" in
		x86_64|aarch64|riscv64) apks="$apks efibootmgr" ;;
	esac
	case "$ARCH" in
		x86_64) apks="$apks mesa-vulkan-intel mesa-vulkan-nouveau mesa-vulkan-virtio" ;;
		aarch64) apks="$apks mesa-vulkan-asahi mesa-vulkan-broadcom mesa-vulkan-freedreno mesa-vulkan-imagination mesa-vulkan-intel mesa-vulkan-nouveau mesa-vulkan-panfrost mesa-vulkan-virtio" ;;
	esac
	case "$ARCH" in
		x86_64)

			apks="$apks grub-bios grub-efi amd-ucode intel-ucode"
			boot_addons="amd-ucode intel-ucode"
			initrd_ucode="/boot/amd-ucode.img /boot/intel-ucode.img"
			kernel_cmdline="$kernel_cmdline console=tty0 console=ttyS0,115200"
			;;
		ppc64le) apks="$apks grub-ieee1275" ;;
		aarch64) apks="$apks grub-efi"; kernel_cmdline="$kernel_cmdline console=tty0 console=ttyAMA0,115200" ;;
		riscv64) apks="$apks grub-efi"; kernel_cmdline="$kernel_cmdline console=tty0 console=ttyS0,115200" ;;
	esac
}


section_syslinux() { return 0; }
section_syslinux_cfg() { return 0; }
section_grub_efi() { return 0; }
section_grubieee1275() { return 0; }

create_image_iso() {
	local flavor="$kernel_flavors" kernel="$DESTDIR/boot/vmlinuz-$kernel_flavors"
	if [ "$flavor" != lts ]; then
		mkdir -p "$DESTDIR/apks/kernels/$ARCH" || return 1
		cp /image-kernels/"$ARCH"/* "$DESTDIR/apks/kernels/$ARCH/" || return 1
	fi
	if [ "$ARCH" = aarch64 ] && [ "$(head -c 2 "$kernel")" != MZ ]; then
		gzip -dc "$kernel" > "$kernel.efi" || return 1
		[ "$(head -c 2 "$kernel.efi")" = MZ ] || return 1
		mv "$kernel.efi" "$kernel"
	fi
	rm -f "$DESTDIR"/boot/System.map-* "$DESTDIR"/boot/config-*
	mkdir -p "$DESTDIR/boot/grub/themes/uflinux"
	cp /project/aports/uflinux/Ubuntu.pf2 /project/aports/uflinux/theme.txt /project/aports/uflinux/splash.png "$DESTDIR/boot/grub/themes/uflinux/"
	[ ! -f /opt/uflinux/memtest86plus ] || cp /opt/uflinux/memtest86plus "$DESTDIR/boot/grub/"
	if [ "$ARCH" = ppc64le ]; then kernel_cmdline="$kernel_cmdline console=hvc0"; fi
	sh -c '. /project/aports/uflinux/uflinux-boot-menu; grub_menu "$@"' sh \
		"/boot/vmlinuz-$flavor" "${initrd_ucode:+$initrd_ucode }/boot/initramfs-$flavor" "" \
		"$initfs_cmdline $kernel_cmdline" /boot/grub > "$DESTDIR/boot/grub/grub.cfg" || return 1
	grub-mkrescue --output "${OUTDIR}/${output_filename}" "$DESTDIR" -follow-links -sysid LINUX -volid "$(gen_volid)"
}
