#!/bin/sh
set -eu

bins=$1
work=$2
unset DISPLAY WAYLAND_DISPLAY
export HOME="$work" XDG_CONFIG_HOME="$work" XDG_RUNTIME_DIR="$work"

expect_status() {
	local expected=$1 status=0
	shift
	"$@" || status=$?
	if [ "$status" -ne "$expected" ]; then
		echo "Expected exit status $expected from $*, got $status" >&2
		return 1
	fi
}

recorder() {
	if [ -n "${GSR_PROFILE_PRELOAD:-}" ]; then
		LD_PRELOAD="$GSR_PROFILE_PRELOAD" "$bins/gpu-screen-recorder" "$@"
	else
		"$bins/gpu-screen-recorder" "$@"
	fi
}

recorder --version
recorder --list-v4l2-devices
expect_status 1 recorder --help
expect_status 1 recorder -w screen -f 60 -k h264 -encoder cpu -o "$work/training.mkv"
"$bins/gsr-cli" --help
expect_status 1 "$bins/gsr-cli" -ipc "$work/missing.sock" status
expect_status 1 "$bins/gsr-kms-server"
expect_status 2 "$bins/gsr-kms-server" "$work/missing.sock" "$work/missing-card"
