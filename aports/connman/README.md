# ConnMan packaging

The build enables ConnMan's VPN daemon with the in-kernel WireGuard plugin.
`connman-vpn` ships `/usr/sbin/connman-vpnd`, its D-Bus activation files and
the plugin; `connman-vpn-openrc` ships the OpenRC script that starts it before
connman (the manager bus name must have an owner early so the Settings VPN tab
appears). Persisted tunnels live in `/var/lib/connman-vpn`, which the daemon
creates itself, so an unprivileged console session can create, connect and
remove WireGuard tunnels through the `net.connman.vpn` API without a helper.

The packaged ConnMan sources and patches in this directory use ConnMan's
GPL-2.0-only license. Alpine patch notices are retained. The small UFLinux iwd
credential-delegation patch is also provided under GPL-2.0-only, matching the
source it modifies. See the pinned source archive's COPYING file.

The build recipe belongs to the UFLinux build system. Package license metadata
reports the license of the resulting ConnMan package.
