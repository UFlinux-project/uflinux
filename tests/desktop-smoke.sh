#!/bin/sh
# Runs inside the target Alpine userspace, including under qemu-user.
set -eu
if [ "${UFLINUX_TEST_BUS:-}" != 1 ]; then
	exec dbus-run-session -- env UFLINUX_TEST_BUS=1 sh -x "$0"
fi
export XDG_RUNTIME_DIR
XDG_RUNTIME_DIR=$(mktemp -d)
chmod 700 "$XDG_RUNTIME_DIR"
export XDG_CONFIG_HOME="$XDG_RUNTIME_DIR/preferences" XDG_DATA_HOME="$XDG_RUNTIME_DIR/data" XDG_CACHE_HOME="$XDG_RUNTIME_DIR/cache"
export WAYLAND_DISPLAY=wayland-0 WLR_BACKENDS=headless WLR_HEADLESS_OUTPUTS=1
export WLR_RENDERER=pixman WLR_LIBINPUT_NO_DEVICES=1 GSK_RENDERER=cairo
export XDG_CURRENT_DESKTOP=labwc XDG_SESSION_TYPE=wayland
results=/tmp/uflinux-smoke-results
mkdir -p "$results" "$XDG_RUNTIME_DIR/config"
python3 - "$XDG_RUNTIME_DIR/config/rc.xml" <<'RC'
import sys,xml.etree.ElementTree as E
root=E.parse('/etc/xdg/labwc/rc.xml')
keyboard=root.getroot().find('keyboard')
for key,action in [('C-A-F10','VirtualOutputAdd'),('C-A-F11','VirtualOutputRemove')]:
    binding=E.SubElement(keyboard,'keybind',key=key)
    E.SubElement(binding,'action',name=action,output_name='UF-TEST')
binding=E.SubElement(keyboard,'keybind',key='C-A-F9')
E.SubElement(binding,'action',name='MoveToOutput',output='HEADLESS-1')
E.SubElement(binding,'action',name='MoveTo',x='0',y='0')
root.write(sys.argv[1])
RC
: > "$XDG_RUNTIME_DIR/config/autostart"
pids=""
cleanup() {
	status=$?
	trap - EXIT HUP INT TERM
	if [ "$status" -ne 0 ]; then
		for log in "$results/desktop.log" "$results/labwc.log"; do
			if [ -f "$log" ]; then
				printf '\n--- %s ---\n' "$log" >&2
				tail -80 "$log" >&2 || :
			fi
		done
	fi
	for pid in $pids; do kill "$pid" 2>/dev/null || :; done
	# Under qemu-user the host process name is qemu-*, not the guest daemon
	# name. Restrict cleanup to this session's unique runtime directory.
	python3 - <<'PY'
import os, signal
from pathlib import Path
runtime = ('XDG_RUNTIME_DIR=' + os.environ['XDG_RUNTIME_DIR']).encode()
for process in Path('/proc').glob('[0-9]*'):
    try:
        pid = int(process.name)
        if pid in (os.getpid(), os.getppid()) or process.stat().st_uid != os.getuid():
            continue
        if runtime in (process / 'environ').read_bytes().split(b'\0'):
            os.kill(pid, signal.SIGTERM)
    except (OSError, ValueError):
        pass
PY
	wait 2>/dev/null || :
	rm -rf "$XDG_RUNTIME_DIR"
	exit "$status"
}
trap cleanup EXIT HUP INT TERM
# A resident process that dies is usually reported much later as a client
# timeout, because the client falls back to running its own foreground shell.
# Name the process and show its log at the moment it goes away instead.
alive() {
	if ! kill -0 "$1" 2>/dev/null; then
		echo "$2 exited unexpectedly" >&2
		if [ -n "${3:-}" ] && [ -f "$3" ]; then
			echo "--- $2 log ---" >&2
			tail -40 "$3" >&2
		fi
		exit 1
	fi
}
# Opening a popup for the first time builds its whole window tree, which under
# an emulated architecture can keep the shell's main loop from answering for
# several seconds. Ask for the popup, then wait for the shell to report it
# rather than trusting a single status read that may have raced that build.
popup_shown() {
	for _attempt in $(seq 1 30); do
		if uflinux-desktop --status | python3 -c 'import json,sys; assert json.load(sys.stdin)["popup"] == sys.argv[1]' "$1"; then
			return 0
		fi
		sleep 1
	done
	return 1
}
labwc --config-dir "$XDG_RUNTIME_DIR/config" > "$results/labwc.log" 2>&1 &
compositor=$!; pids="$pids $compositor"
for _attempt in $(seq 1 60); do
	[ -S "$XDG_RUNTIME_DIR/$WAYLAND_DISPLAY" ] && break
	alive "$compositor" 'Compositor' "$results/labwc.log"
	sleep 1
done
[ -S "$XDG_RUNTIME_DIR/$WAYLAND_DISPLAY" ]
# The settings service reloads the compositor with `labwc --reconfigure`, which
# signals the process named by LABWC_PID rather than the socket it just waited
# for. A real session exports the identifier from its autostart script.
export LABWC_PID=$compositor
# D-Bus activates a desktop from the environment the session bus saved, not this
# shell's. Without this a desktop that has to activate itself, because the
# resident one is gone or not registered yet, opens with no display and the
# client ends up running a whole second shell in the foreground.
dbus-update-activation-environment WAYLAND_DISPLAY XDG_RUNTIME_DIR XDG_CURRENT_DESKTOP XDG_SESSION_TYPE LABWC_PID GSK_RENDERER XDG_CONFIG_HOME XDG_DATA_HOME XDG_CACHE_HOME
wayland-info > "$results/wayland.txt"
grep -q zwlr_foreign_toplevel_manager_v1 "$results/wayland.txt"
python3 /project/tests/artwork-smoke.py > "$results/artwork.txt"
# Record Wayland setup and guest syscalls. The trace distinguishes a stalled
# compositor handshake from a D-Bus connection failure and identifies the
# qemu-user socket emulation defect recognised below.
WAYLAND_DEBUG=1 QEMU_STRACE=1 mako > "$results/notifications.log" 2>&1 &
notifications=$!; pids="$pids $notifications"
# A running process is not necessarily registered on D-Bus yet. Calling
# notify-send too early can trigger a competing D-Bus activation of mako.
notifications_ready=0
for _attempt in $(seq 1 120); do
	if gdbus call --session --dest org.freedesktop.DBus --object-path /org/freedesktop/DBus --method org.freedesktop.DBus.NameHasOwner org.freedesktop.Notifications | grep -q true; then notifications_ready=1; break; fi
	if ! kill -0 "$notifications" 2>/dev/null; then cat "$results/notifications.log" >&2; exit 1; fi
	sleep 1
done
if [ "$notifications_ready" != 1 ]; then
	for entry in status wchan syscall; do
		{ echo "--- mako /proc/$notifications/$entry ---"; cat "/proc/$notifications/$entry"; } >> "$results/notifications-state.txt" 2>&1 || :
	done
	# qemu-user has no getsockopt case for SO_PEERGROUPS (SOL_SOCKET option 59),
	# so it forwards the option through its four-byte integer path. The kernel
	# answers ERANGE because a peer with supplementary groups needs more room, and
	# QEMU returns from that error without writing the required length back. sd-bus
	# only enlarges its buffer on ERANGE, so it repeats the identical call forever
	# and never reaches D-Bus authentication. That is an emulator defect, not a
	# desktop defect: record it and validate the rest of the session without
	# notifications instead of failing the whole architecture.
	if kill -0 "$notifications" 2>/dev/null &&
		grep -qE 'getsockopt\([0-9]+, ?1, ?59,' "$results/notifications.log"; then
		printf '%s\n' 'getsockopt(SO_PEERGROUPS) returns ERANGE without the required length, so sd-bus never authenticates' > "$results/notifications-emulation.txt"
		kill "$notifications"
	else
		cat "$results/notifications-state.txt" >&2
		tail -100 "$results/notifications.log" >&2
		echo 'Notification service did not register on D-Bus' >&2
		exit 1
	fi
fi
/usr/libexec/pipewire-launcher > "$results/audio.log" 2>&1 &
for _attempt in $(seq 1 30); do
	pactl info > "$results/pulse.txt" 2>/dev/null && break
	sleep 1
done
pactl load-module module-null-sink sink_name=uflinux_test > "$results/audio-module.txt"
for _attempt in $(seq 1 30); do
	if pactl set-default-sink uflinux_test 2> "$results/audio-default-error.txt" &&
		pactl info > "$results/pulse.txt" && grep -Fxq 'Default Sink: uflinux_test' "$results/pulse.txt"; then break; fi
	sleep 1
done
if ! grep -Fxq 'Default Sink: uflinux_test' "$results/pulse.txt"; then
	cat "$results/audio-default-error.txt" "$results/audio.log" >&2
	exit 1
fi
for _attempt in $(seq 1 10); do
	pactl set-sink-volume uflinux_test 50%
	pactl get-sink-volume uflinux_test > "$results/volume.txt"
	grep -q '50%' "$results/volume.txt" && break
	sleep 1
done
grep -q '50%' "$results/volume.txt"
if [ "${UFLINUX_TEST_PORTALS:-}" = 1 ]; then
    gdbus call --session --dest org.freedesktop.portal.Desktop \
        --object-path /org/freedesktop/portal/desktop --timeout 45 \
        --method org.freedesktop.portal.Settings.Read org.freedesktop.appearance color-scheme \
        > "$results/portal-settings.txt"
    gdbus call --session --dest org.freedesktop.portal.Desktop \
        --object-path /org/freedesktop/portal/desktop --timeout 45 \
        --method org.freedesktop.DBus.Properties.Get org.freedesktop.portal.ScreenCast version \
        > "$results/portal-screencast.txt"
    for backend in gtk4 wlr; do
        gdbus call --session --dest org.freedesktop.DBus \
            --object-path /org/freedesktop/DBus --method org.freedesktop.DBus.NameHasOwner \
            "org.freedesktop.impl.portal.desktop.$backend" | grep -q true
    done
fi
mkdir -p /tmp/uflinux-settings-smoke-results
/usr/libexec/uflinux-settingsd > /tmp/uflinux-settings-smoke-results/service.log 2>&1 &
settings_daemon=$!; pids="$pids $settings_daemon"
for _attempt in $(seq 1 100); do
    if gdbus call --session --dest org.freedesktop.DBus --object-path /org/freedesktop/DBus --method org.freedesktop.DBus.NameHasOwner org.uflinux.Settings1 | grep -q true; then break; fi
    alive "$settings_daemon" 'Settings service' /tmp/uflinux-settings-smoke-results/service.log
    sleep .1
done
gdbus call --session --dest org.freedesktop.DBus --object-path /org/freedesktop/DBus --method org.freedesktop.DBus.NameHasOwner org.uflinux.Settings1 | grep -q true
# GDK routes libwayland's error details through its debug log. Without these,
# a protocol failure can report only EPROTO, with no interface, object or cause.
# Keep the wire trace in the diagnostics artifact, including output hotplug and
# fractional-scale changes, which can race differently under qemu-user.
G_MESSAGES_DEBUG=Gdk WAYLAND_DEBUG=client G_DEBUG=fatal-criticals UFLINUX_SETTINGS_BENCH=1 /usr/bin/uflinux-desktop --daemon > "$results/desktop.log" 2>&1 &
panel=$!; pids="$pids $panel"
ptyxis --standalone --title=UFLinux-Smoke -x 'sleep 600' > "$results/ptyxis.log" 2>&1 &
terminal=$!; pids="$pids $terminal"
for _attempt in $(seq 1 60); do
	wlrctl toplevel list > "$results/toplevels.txt"
	grep -q org.gnome.Ptyxis "$results/toplevels.txt" && break
	sleep 1
done
grep -q org.gnome.Ptyxis "$results/toplevels.txt"
wlrctl toplevel minimize app_id:org.gnome.Ptyxis
wlrctl toplevel focus app_id:org.gnome.Ptyxis
sleep 3
alive "$panel" 'Resident desktop' "$results/desktop.log"
kill -0 "$terminal"
# The shell draws the wallpaper itself, so the desktop being alive is only half
# of it: the appearance the service published has to be on the output.
background_ready=0
for _attempt in $(seq 1 60); do
	alive "$panel" 'Resident desktop' "$results/desktop.log"
	if uflinux-desktop --status | python3 -c 'import json,sys; b=json.load(sys.stdin).get("background") or {}; entry=next((e for e in b.get("outputs",[]) if e["output"]=="HEADLESS-1"),{}); sys.exit(0 if entry.get("file")=="/usr/share/backgrounds/uflinux/Dark_Ivy.jpg" and entry.get("fit")=="fill" and not b.get("failed") else 1)'; then
		background_ready=1
		break
	fi
	sleep 1
done
if [ "$background_ready" != 1 ]; then
	uflinux-desktop --status > "$results/background.json"
	cat "$results/background.json" "$results/desktop.log" >&2
	exit 1
fi
if [ "$notifications_ready" = 1 ]; then
	kill -0 "$notifications"
	notify-send --expire-time=3000 UFLinux 'Desktop validation passed'
fi
for menu_name in network sound power; do
    alive "$panel" 'Resident desktop' "$results/desktop.log"
    uflinux-desktop --panel "$menu_name" --output HEADLESS-1
    popup_shown "$menu_name"
    uflinux-desktop --hide
done
alive "$panel" 'Resident desktop' "$results/desktop.log"
uflinux-desktop --start-menu --output HEADLESS-1
popup_shown start
uflinux-desktop --hide
UFLINUX_DESKTOP_PID=$panel UFLINUX_DESKTOP_LOG=$results/desktop.log \
    UFLINUX_SETTINGS_DAEMON_PID=$settings_daemon sh /project/tests/settings-smoke.sh
if grep -Eiq 'panicked at|thread.*panicked|CRITICAL|assertion.*failed' "$results/desktop.log"; then
    cat "$results/desktop.log" >&2
    exit 1
fi
grim "$results/desktop.png"
ps -o pid,rss,args -p "$compositor","$panel","$terminal" > "$results/memory.txt"
notification_report='validated through the notification service'
if [ "$notifications_ready" != 1 ]; then
	notification_report='notification service skipped under qemu-user emulation'
fi
printf '%s\n' "headless compositor, configuration, panel, terminal, toplevel controls, missing battery and audio volume changes passed; $notification_report" > "$results/result.txt"
