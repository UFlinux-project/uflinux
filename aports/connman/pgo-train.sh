#!/bin/sh
set -eu

export DBUS_SYSTEM_BUS_ADDRESS="$DBUS_SESSION_BUS_ADDRESS"

bins=$1
work=$2
cd "$work"
cat > main.conf <<'CONFIG'
[General]
PreferredTechnologies=ethernet,wifi
DefaultAutoConnectTechnologies=ethernet,wifi
AllowHostnameUpdates=false
AllowDomainnameUpdates=false
ResolvConf=/dev/null
CONFIG
printf '[General]\n' > vpn.conf

"$bins/connmand" --nodaemon --nodnsproxy --device=uflinux-pgo --config=main.conf > connmand.log 2>&1 &
manager=$!
"$bins/connman-vpnd" --nodaemon --config=vpn.conf > connman-vpnd.log 2>&1 &
vpn=$!
trap 'kill "$manager" "$vpn" 2>/dev/null || :; wait "$manager" "$vpn" 2>/dev/null || :' EXIT
trap 'exit 1' HUP INT TERM

wait_for_manager() {
	local service=$1 method=$2
	for attempt in $(seq 30); do
		if dbus-send --session --print-reply --reply-timeout=500 --dest="$service" / "$method" >/dev/null 2>&1; then
			return 0
		fi
		sleep 0.1
	done
	cat connmand.log connman-vpnd.log >&2
	return 1
}

wait_for_manager net.connman net.connman.Manager.GetProperties
wait_for_manager net.connman.vpn net.connman.vpn.Manager.GetConnections
for command in state technologies services vpnconnections; do
	"$bins/connmanctl" "$command"
done

status=0
"$bins/connmand-wait-online" --interface=uflinux-pgo --timeout=1 || status=$?
test "$status" -eq 110 # ETIMEDOUT on the deliberately offline connection.

kill "$manager" "$vpn"
wait "$manager"
wait "$vpn"
trap - EXIT
