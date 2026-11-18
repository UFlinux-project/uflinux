#!/bin/sh
set -eu
name=$1
release=$2
suffix=$3
shim=$4
binary=$release/$name$suffix
scratch=$(mktemp -d)
app=
companion=
compositor=
cleanup() {
    status=$?
    if [ "$status" -ne 0 ]; then
        echo "Training failed for $binary" >&2
        cat "$scratch/"*.log >&2 || :
    fi
    for pid in "$app" "$companion" "$compositor"; do
        if [ -n "$pid" ]; then
            kill -KILL "$pid" 2>/dev/null || :
            wait "$pid" 2>/dev/null || :
        fi
    done
    rm -rf "$scratch"
}
trap cleanup EXIT
trap 'exit 1' HUP INT TERM
export HOME="$scratch/home" XDG_RUNTIME_DIR="$scratch/runtime" XDG_CONFIG_HOME="$scratch/config"
export XDG_DATA_HOME="$scratch/data" XDG_CACHE_HOME="$scratch/cache"
export DBUS_SYSTEM_BUS_ADDRESS="$DBUS_SESSION_BUS_ADDRESS"
export WAYLAND_DISPLAY=wayland-0 WLR_BACKENDS=headless WLR_RENDERER=pixman WLR_HEADLESS_OUTPUTS=1
export WLR_LIBINPUT_NO_DEVICES=1 XDG_CURRENT_DESKTOP=labwc XDG_SESSION_TYPE=wayland
export GDK_BACKEND=wayland GSK_RENDERER=cairo GTK_A11Y=none
mkdir -m 700 "$HOME" "$XDG_RUNTIME_DIR" "$XDG_CONFIG_HOME" "$XDG_DATA_HOME" "$XDG_CACHE_HOME"
has_name() {
    gdbus call --session --timeout 2 --dest org.freedesktop.DBus --object-path /org/freedesktop/DBus \
        --method org.freedesktop.DBus.NameHasOwner "$1" | grep -q true
}
wait_for() {
    attempts=0
    until "$@"; do
        attempts=$((attempts + 1))
        test "$attempts" -lt 100
        sleep 0.1
    done
}
wait_app() {
    wait "$app"
    app=
}
run() {
    "$binary" "$@" >"$scratch/app.log" 2>&1 &
    app=$!
}
session_state() {
    gdbus call --session --timeout 5 --dest org.uflinux.Settings1 --object-path /org/uflinux/Settings \
        --method org.uflinux.Settings1.GetState
}
close_window() {
    attempts=0
    while kill -0 "$app" 2>/dev/null; do
        attempts=$((attempts + 1))
        test "$attempts" -lt 30
        wtype -M alt -k F4 -m alt
        sleep 0.2
    done
    wait_app
}
case "$name" in
    uflinux-settings|uflinux-desktop|uflinux-installer|uflinux-screencapture)
        cat >"$XDG_CONFIG_HOME/rc.xml" <<'CONFIG'
<labwc_config><keyboard><keybind key="A-F4"><action name="Close" /></keybind></keyboard></labwc_config>
CONFIG
        : >"$XDG_CONFIG_HOME/autostart"
        labwc -C "$XDG_CONFIG_HOME" >"$scratch/labwc.log" 2>&1 &
        compositor=$!
        wait_for test -S "$XDG_RUNTIME_DIR/$WAYLAND_DISPLAY"
        ;;
esac
case "$name" in
    uflinux-settings)
        run --page network
        wait_for has_name org.uflinux.Settings
        "$binary" --page sound
        "$binary" --page power
        close_window
        ;;
    uflinux-desktop)
        run --daemon
        wait_for has_name org.uflinux.Desktop
        "$binary" --panel sound
        "$binary" --panel power
        "$binary" --hide
        "$binary" --quit
        wait_app
        ;;
    uflinux-installer)
        run
        wait_for has_name org.uflinux.Installer
        # Exercise the first wizard pages without requiring a storage backend.
        sleep 1
        wtype -k Tab -k Tab -k space
        close_window
        ;;
    uflinux-screencapture)
        run
        # Let the layer-shell picker map and take keyboard focus.
        sleep 2
        kill -0 "$app"
        wtype -k Escape
        kill -TERM "$app"
        wait_app
        ;;
    uflinux-settingsd)
        run
        wait_for has_name org.uflinux.Settings1
        session_state >/dev/null
        kill -TERM "$app"
        wait_app
        ;;
    uflinux-settingsctl)
        # The companion can still generate its own BOLT data. Keep its LLVM
        # counters separate from the CLI's counters.
        LLVM_PROFILE_FILE="$scratch/companion.profraw" \
            "$release/uflinux-settingsd$suffix" >"$scratch/companion.log" 2>&1 &
        companion=$!
        wait_for has_name org.uflinux.Settings1
        "$binary" status | grep -q '^{'
        kill -TERM "$companion"
        wait "$companion"
        companion=
        ;;
    uflinux-settings-system)
        LD_PRELOAD="$shim" "$binary" >"$scratch/app.log" 2>&1 &
        app=$!
        wait_for has_name org.uflinux.Settings.System1
        gdbus call --session --timeout 5 --dest org.uflinux.Settings.System1 \
            --object-path /org/uflinux/Settings/System --method org.uflinux.Settings.System1.Execute \
            '{"operation":"status"}'
        # The service exits normally when idle, flushing profiling hooks.
        wait_app
        ;;
    *) exit 2 ;;
esac
