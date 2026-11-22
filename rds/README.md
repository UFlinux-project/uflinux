# UFLinux Remote Desktop Services

An IronRDP-based remote desktop server for UFLinux with per-user sessions, a
browser client, and a system helper that lets an ordinary user administer it.
It is modelled on Windows Remote Desktop Services: one broker, one private
Wayland session per logged-in user, and no shared desktop.

Everything is disabled by default. Until the service is enabled nothing listens
on any port, and while enabled and idle the broker is a single small process
that opens its listeners and does nothing else. Encoder initialisation, the
per-user compositor and the web gateway's session state all appear on the first
connection and are torn down again when the last client leaves.

## Packages

| Package | Contents |
| --- | --- |
| `uflinux-rds` | Broker `/usr/bin/uflinux-rdsd`, `uflinux-rdsctl`, `/etc/uflinux/rds.json`, OpenRC service, PAM and D-Bus/polkit data |
| `uflinux-rds-server` | Per-connection session host `/usr/libexec/uflinux-rdp-session`, which runs the `uflinux-session` launcher (`uflinux-defaults`) |
| `uflinux-rds-web` | Web gateway `/usr/bin/uflinux-rds-web` with the compiled wasm client embedded |
| `uflinux-rds-system` | Authorized helper `/usr/libexec/uflinux-rds-system`, its D-Bus service and polkit rules |
| `uflinux-rds-vaapi` | Runtime packages (`ffmpeg`, `openh264`, `libva`) for hardware H.264 |

## Configuration

`/etc/uflinux/rds.json` is the only configuration file; `/etc/conf.d/uflinux-rds`
overrides a few common values for people who prefer shell syntax. The keys are
the ones the Remote Desktop page in `uflinux-settings` edits:

* `enabled`, `rdp_port`, `rdp_bind`, `nla_required` (off by default; see the
  note about CredSSP in `rds/server/src/main.rs`)
* `allowed_users`, `allowed_groups`, `max_sessions`, `idle_timeout_secs`,
  `session_command` (`linger_secs` is still accepted and ignored)
* `codec.h264` (`auto`/`force`/`off`), `codec.allow_remotefx`, `codec.rfx_quant`,
  `codec.max_fps`, `codec.vaapi_device`
* `web.enabled`, `web.port`, `web.bind`, `web.https`, `web.certificate.source`
  (`self_signed`, or `custom` with `web.certificate.chain_path` and
  `web.certificate.private_key_path`), `web.token_ttl_minutes`

Apply a change without restarting, which keeps existing sessions alive:

```sh
uflinux-rdsctl apply /etc/uflinux/rds.json
```

`uflinux-rdsctl status` and `uflinux-rdsctl sessions` report the listeners and
roster, and `uflinux-rdsctl disconnect SESSION_ID` ends one session. `start` and
`stop` ask the running broker to open or close its listeners, so they work
without root; if no broker answers they fall back to `rc-service uflinux-rds`,
which does need root. `enable` and `disable` move both halves of the master
switch — the default runlevel and the `enabled` key of `/etc/uflinux/rds.json` —
and always need root; a live broker is asked to do it itself, otherwise the file
is rewritten and `rc-update add`/`del` is run directly. The Settings page reaches
the same effect through the polkit helper.

Applying a configuration that changes `enabled` also starts or stops the
listeners — that is what the Settings switch promises — while any other field is
adopted without disturbing a running service. Starting a disabled service is
refused: enable it first.

## Service

`rc-service uflinux-rds start` runs the broker under OpenRC. The service creates
its own socket and pidfile directory (`/run/uflinux-rds`, or the parent of
`UFLINUX_RDS_CONTROL_SOCKET` if that is set in `/etc/conf.d/uflinux-rds`), and
starts the broker with `--resident` so it never quits on idle: under OpenRC a
self-exit would look like a crash and nothing would restart it. A resident
broker that is disabled and idle holds no listeners and has no children, so it
costs a few megabytes of RSS and nothing else.

The web gateway writes its self-signed certificate into `/var/lib/uflinux-rds`,
mode 0750, so the private key (0640) is not readable by ordinary users.

## How a connection is served

1. `uflinux-rdsd` accepts the connection — on the TCP port or on the web
   socket — and starts `uflinux-rdp-session` with the client's socket on fd 3.
   Nothing else has happened yet: no desktop, no encoder.
2. The session host runs X.224 and TLS (or the web offload below), then runs
   PAM (`/etc/pam.d/uflinux-rds`) and checks `allowed_users`/`allowed_groups`,
   `max_sessions` and NLA. Only a validated credential reaches the next step;
   until then the client is offered nothing but an access-denied.
3. It then drops root to the *authenticated* account — setuid, setgid, that
   user's supplementary groups and a private `XDG_RUNTIME_DIR` owned by it —
   and starts the headless compositor for `session_command` (by default the
   `uflinux-session` launcher, so the session gets its own D-Bus session bus,
   `uflinux-compositor`, labwc and everything in `/etc/xdg/labwc/autostart`),
   attaching to it over Wayland using the virtual-pointer, virtual-keyboard and
   screencopy protocols. Input is injected rather than shared, so two users on
   one machine cannot see or drive each other's desktop. This is why the service
   runs as root: the desktop must belong to whoever logged in, not to whoever
   owns the daemon.
4. Frames are encoded by the tier chosen in `codec`: H.264 when the client
   negotiated EGFX and an encoder is available, otherwise RemoteFX, otherwise
   raw bitmap updates. An idle session stops encoding entirely.
5. A client that goes silent for `idle_timeout_secs` is dropped, and when the
   client disconnects the session host exits. The desktop does not outlive its
   connection: a reconnect gets a fresh session host and a fresh desktop.

## Codecs

`codec.h264 = "auto"` prefers a hardware encoder and falls back to software
OpenH264, then to RemoteFX. Hardware encode runs `ffmpeg -c:v h264_vaapi` out of
process, so a crash in the driver takes down one encoder rather than a session;
install `uflinux-rds-vaapi` for it. `force` refuses to fall back, which is the
way to check that the hardware path really is in use, and `off` disables H.264
altogether.

Browser clients always receive QOI or RemoteFX: no browser decodes the H.264 RDP
channel. H.264 is for native clients such as mstsc, FreeRDP and Remmina.

## Web client

`uflinux-rds-web` serves the login page and the wasm client on port 3390. It is
a separate binary so that disabling the web gateway also removes its listener.
With `web.https = true` it generates a self-signed certificate on first use, or
uses the `chain_path`/`private_key_path` pair when `certificate.source` is
`custom`; setting `https = false` serves plain HTTP instead.

The login form posts to the gateway, which authenticates through the same PAM
service, mints a short-lived one-time token (`web.token_ttl_minutes`), and hands
the browser `session.html`. That page opens a WebSocket and drives the IronRDP
wasm client, so the console in the browser speaks the same RDP protocol as a
native client. Nothing about the session is stored server-side beyond the token.

### How the browser reaches a session

The gateway does not dial the public RDP port. It relays each WebSocket into a
second, private listener of the broker: the unix socket `/run/uflinux-rds/web.sock`
(`UFLINUX_RDS_WEB_SOCKET`), mode 0600, bound and unbound by the same task that
owns the TCP port. Which front end a connection arrived on decides the
transport, so nothing on the wire claims it:

* TCP `rdp_port` → the session host is started with `--transport rdp` and
  terminates TLS itself, exactly as before. mstsc, FreeRDP and Remmina see
  `SSL` (or `HYBRID`) in the X.224 confirm and negotiate TLS inside the
  connection.
* the web socket → the host is started with `--transport web`, answers X.224
  with an empty `SecurityProtocol` and skips the inner TLS handshake, because
  the hop is already encrypted: TLS ended at the gateway's listener, and the
  rest of the path is a root-owned unix socket plus a WebSocket. The wasm
  client asks for this by enabling `enable_standard_rdp_security`.

Relaying into a session host's own TCP port instead would mean a second TLS
handshake inside a connection the browser already believes is secure, which is
why the offload is explicit rather than inferred. The socket path is also why
`web.sock` is not simply a port number: an unprivileged gateway could otherwise
be pointed at any local RDP server.

## Building

```sh
sh scripts/build-wasm.sh        # client into gateway/assets
cargo build --workspace --bins --features "$_rds_features"
cargo test --workspace --features "$_rds_features"
```

where the packaged feature set is

```sh
_rds_features="uflinux-rds-gateway/pam,uflinux-rds-server/pam,uflinux-rds-server/h264-vaapi,uflinux-rds-server/h264-software"
```

`pam` is opt-in because `pam-sys` runs bindgen, which needs libclang and the
libpam headers. `scripts/pgo-build.sh` does the profile-guided and BOLT-optimized
build used by `aports/uflinux-rds`, the same way `settings/` does for Settings:
train, rebuild with `-fprofile-generate`, train again, `llvm-bolt`, train a third
time. `scripts/pgo-train.sh` drives one real loop — start the broker, log in over
the web gateway, watch a session appear, disconnect — and needs a session bus
and `gdbus`; the session program itself is stubbed out with `/bin/true`.
