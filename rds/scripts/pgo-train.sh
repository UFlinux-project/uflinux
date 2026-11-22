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
cleanup() {
    status=$?
    if [ "$status" -ne 0 ]; then
        echo "Training failed for $binary" >&2
        cat "$scratch/"*.log >&2 || :
    fi
    for pid in "$app" "$companion"; do
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
mkdir -m 700 "$HOME" "$XDG_RUNTIME_DIR" "$XDG_CONFIG_HOME" "$XDG_DATA_HOME" "$XDG_CACHE_HOME"
# Nothing here needs a display, a GPU or root.
#
# Ports come from the process id. Two rules matter: they stay below the kernel
# ephemeral range, which starts at 32768, because a port inside it may already
# belong to an unrelated outbound socket on the runner; and consecutive ids
# pick blocks seven apart, because the shards run several workloads at once
# with neighbouring process ids and adjacent pairs would overlap. The broker
# tolerates a listener it cannot bind with a warning, but the web gateway
# exits, and a workload that cannot reach it fails the build.
port_busy() {
    # The local address column is "ADDR:PORT" with a four digit uppercase port.
    # A missing procfs counts as free, which is what we would do anyway.
    port_hex=$(printf '%04X' "$1")
    for table in /proc/net/tcp /proc/net/tcp6; do
        cat "$table" 2>/dev/null || :
    done | awk -v needle=":$port_hex\$" '$2 ~ needle { found = 1 } END { exit !found }'
}
slot=$(($$ % 2000))
tries=0
while :; do
    rdp_port=$((10000 + slot * 7))
    web_port=$((rdp_port + 1))
    if ! port_busy "$rdp_port" && ! port_busy "$web_port"; then
        break
    fi
    tries=$((tries + 1))
    if [ "$tries" -ge 32 ]; then
        echo "pgo-train: no free port pair found near $rdp_port" >&2
        exit 1
    fi
    slot=$(( (slot + 911) % 2000 ))
done
socket=$scratch/control.sock
config=$scratch/rds.json
cat >"$config" <<CONFIG
{"enabled":true,"rdp_port":$rdp_port,"rdp_bind":"127.0.0.1","nla_required":false,"max_sessions":2,"idle_timeout_secs":0,"linger_secs":0,"session_command":"/bin/true","codec":{"h264":"off","allow_remotefx":false,"rfx_quant":20,"max_fps":30},"web":{"enabled":true,"port":$web_port,"bind":"127.0.0.1","https":false,"certificate":{"source":"self_signed"},"token_ttl_minutes":5}}
CONFIG
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
# The control protocol is a private binary framing, so the broker is driven
# with the shipped client rather than with wget.
ctl() {
    # The companion can still generate its own profile data. Keep its counters
    # out of the binary being trained.
    LLVM_PROFILE_FILE="$scratch/ctl-%m-%p.profraw" \
        "$release/uflinux-rdsctl$suffix" --socket "$socket" "$@" >"$scratch/ctl.log" 2>&1 || :
}
http() {
    wget -q -O "$scratch/response" "$@"
}
case "$name" in
    uflinux-rdsd)
        run --config "$config" --socket "$socket" --session-binary /bin/false \
            --web-binary /bin/true --rdp-bind 127.0.0.1 --rdp-port "$rdp_port" \
            --take-over --resident
        wait_for test -S "$socket"
        # A few control round trips: read-only queries first, then a config
        # push, which re-validates, rebinds and rewrites the scratch file.
        ctl status
        ctl sessions
        ctl apply "$config"
        ctl status
        ctl sessions
        ctl disconnect 1
        kill -TERM "$app"
        # The broker is resident and holds the RDP listener, so it has to be
        # asked to shut down instead of waiting for its idle exit.
        wait_app
        ;;
    uflinux-rds-web)
        run --config "$config" --port "$web_port" --bind 127.0.0.1 \
            --plain-http --state-dir "$scratch/state"
        wait_for http "http://127.0.0.1:$web_port/healthz"
        http "http://127.0.0.1:$web_port/" >"$scratch/index.log" 2>&1 || :
        http "http://127.0.0.1:$web_port/api/state" >"$scratch/state.log" 2>&1 || :
        # No PAM in this build, so a well formed login is refused with 401;
        # wget treats that as an error, which is the expected answer here.
        http --post-data '{"username":"nobody","password":"x"}' \
            --header 'Content-Type: application/json' \
            "http://127.0.0.1:$web_port/api/login" >"$scratch/login.log" 2>&1 || :
        http --post-data '{"username":"","password":""}' \
            --header 'Content-Type: application/json' \
            "http://127.0.0.1:$web_port/api/login" >"$scratch/empty.log" 2>&1 || :
        kill -TERM "$app"
        wait_app
        ;;
    uflinux-rds-system)
        LD_PRELOAD="$shim" "$binary" >"$scratch/app.log" 2>&1 &
        app=$!
        wait_for has_name org.uflinux.Rds.System1
        gdbus call --session --timeout 5 --dest org.uflinux.Rds.System1 \
            --object-path /org/uflinux/Rds/System1 --method org.uflinux.Rds.System1.Execute \
            '{"operation":"status"}'
        # Only the read-only operation: the others write under /etc and /var.
        # The service exits normally when idle, flushing profiling hooks.
        wait_app
        ;;
    uflinux-rdsctl)
        "$binary" --help
        # No broker owns this socket, so every verb must fail with a message.
        "$binary" --socket "$socket" status >"$scratch/dead.log" 2>&1 || :
        "$binary" --socket "$socket" sessions >"$scratch/dead.log" 2>&1 || :
        "$binary" --socket "$socket" apply "$config" >"$scratch/dead.log" 2>&1 || :
        "$binary" --socket "$socket" disconnect one >"$scratch/bad.log" 2>&1 || :
        "$binary" --socket "$socket" --nope status >"$scratch/bad.log" 2>&1 || :
        "$binary" >"$scratch/bad.log" 2>&1 || :
        ;;
    uflinux-rdp-session)
        # The session host takes its client socket on fd 3 and drives a
        # compositor, neither of which a build chroot can offer, so only the
        # argument handling is exercised until main() grows a real body.
        "$binary" --help >"$scratch/help.log" 2>&1 || :
        "$binary" >"$scratch/noargs.log" 2>&1 || :
        "$binary" --client-fd 3 --transport rdp >"$scratch/nofd.log" 2>&1 || :
        "$binary" --transport vnc >"$scratch/bad.log" 2>&1 || :
        "$binary" --nope >"$scratch/bad.log" 2>&1 || :
        ;;
    *) exit 2 ;;
esac
