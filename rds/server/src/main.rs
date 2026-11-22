//! `uflinux-rdp-session` — one RDS session host, one connection, one desktop.
//!
//! The broker forks this binary per session and hands it the two descriptors
//! the session needs before it can do anything else. Nothing is listened on:
//! the socket is already connected when `main` starts, which is what lets the
//! broker accept the TCP connection, apply admission control, and only then
//! pay for a compositor.
//!
//! # Descriptor contract
//!
//! * **fd 3 — the client socket.** An already-connected `SOCK_STREAM` socket
//!   (the broker accepted it, optionally wrapped it, and cleared `O_NONBLOCK`
//!   in its `pre_exec`). `--transport rdp` (the default) expects
//!   `AF_INET`/`AF_INET6`; `--transport web` expects `AF_UNIX`, because a
//!   browser connection reaches the session through the gateway's private
//!   backend socket rather than through the public TCP listener. Override the
//!   number with `--client-fd N` or `--socket-fd N` (aliases), or with
//!   `UFLINUX_RDS_CLIENT_FD`. A missing or unusable fd 3 is a hard error: the
//!   process prints one line and exits non-zero without panicking.
//! * **fd 4 — the status socket.** One end of an `AF_UNIX` `SOCK_DGRAM`
//!   `socketpair()` toward the broker. Every state change is one JSON line
//!   (see [`uflinux_rds_server::status`]); the broker's reader turns them into
//!   `SessionUpdate`s. Override with `--status-fd N` or
//!   `UFLINUX_RDS_STATUS_FD`. Omitting it is legal — the reporter then
//!   swallows every frame — because a session started by hand should still
//!   work.
//!
//! Both ends are inherited across `fork`/`exec`, so the numbers are the
//! broker's choice, not ours; anything else is argv plus the configuration
//! file (see [`uflinux_rds_server::args`]).
//!
//! # What one session does
//!
//! 1. Parses argv, loads and validates the configuration.
//! 2. Takes over the client descriptor and builds the display state, but starts
//!    no desktop yet.
//! 3. Chooses a codec tier from what this build, this machine and the client
//!    can do ([`uflinux_rds_server::caps`]).
//! 4. Runs one IronRDP connection over the inherited socket. Once the client's
//!    credentials have been checked, the credential validator starts a nested
//!    compositor as the authenticated account ([`DesktopLauncher`],
//!    [`uflinux_rds_wayland::DesktopBackend`]) and the frame pump that feeds
//!    the client from it (a dedicated OS thread — the Wayland frame source
//!    blocks).
//! 5. Reports state to the broker and honours `--idle-timeout`. The desktop
//!    lives exactly as long as its connection: a reconnect gets a fresh host.
//!
//! The order of steps 2 and 4 is the point: a session host that started the
//! desktop first would have to pick a desktop user before it knew who was
//! logging in, and a packaged broker runs as root, so every unauthenticated
//! connection would get a root-owned desktop.

mod clipboard;
mod sound;

use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::net::UnixStream as StdUnixStream;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::net::TcpStream;
use tokio::net::UnixStream;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use ironrdp_pdu::codecs::rfx::Quant;
use ironrdp_pdu::rdp::capability_sets::{BitmapCodecs, server_codecs_capabilities};
#[cfg(feature = "egfx")]
use ironrdp_server::GfxServerFactory;
use ironrdp_server::{
    ConnectionPolicy, CredentialDecision, CredentialValidationError, CredentialValidator,
    Credentials, DesktopSize, KeyboardEvent, MouseButton, MouseEvent, RdpServer,
    RdpServerInputHandler, ServerEvent, TransportTls,
};

use uflinux_rds_codec::{
    CodecTier, H264Backend, HardwareCaps, VaapiConfig, detect_vaapi, quant_subbands,
};
use uflinux_rds_common::{Config, H264Policy, STATE_DIR};
use uflinux_rds_wayland::{
    DesktopBackend, DesktopSpec, KeyboardEvent as WlKeyboardEvent, PointerEvent as WlPointerEvent,
    button,
};

use uflinux_rds_server::args::{ArgError, Args, Transport};
use uflinux_rds_server::auth::{self, AuthError, Policy as AdmissionPolicy};
#[cfg(not(feature = "pam"))]
use uflinux_rds_server::auth::{Authenticator, StubAuthenticator};
use uflinux_rds_server::caps::{self, Decision, Negotiation};
use uflinux_rds_server::display::{
    EncoderFactory, FramePump, PumpCommand, PumpConfig, SessionDisplay,
};
use uflinux_rds_server::pacing::Pacer;
use uflinux_rds_server::scancode::keysym_for;
use uflinux_rds_server::status::{StatusReporter, StatusUpdate};
use uflinux_rds_server::tls;

/// Initial desktop geometry, before the client has said how big it wants
/// things. The desktop is started after the client's X.224 and credential
/// exchange, so the launcher uses whatever size the display handler adopted
/// from `request_initial_size`; these numbers are only what a client that
/// never sends `desktop_resize_flag` gets.
const DESKTOP_WIDTH: u32 = 1280;
const DESKTOP_HEIGHT: u32 = 720;

/// PAM service name, matching `etc/pam.d/uflinux-rds` in the package.
#[cfg(feature = "pam")]
const PAM_SERVICE: &str = "uflinux-rds";

/// A traversable runtime base, separate from the broker's private TLS state.
const DESKTOP_RUNTIME_ROOT: &str = "/run/uflinux-rds-desktops";

/// How often the idle watchdog runs.
const WATCHDOG_PERIOD: Duration = Duration::from_secs(5);

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let env: Vec<(String, String)> = std::env::vars().collect();

    let args = match Args::parse(argv.iter().cloned(), env.iter().cloned()) {
        Ok(args) => args,
        Err(error) => {
            eprintln!("uflinux-rdp-session: {error}");
            eprintln!("uflinux-rdp-session: see --help for the option list");
            std::process::exit(2);
        }
    };
    if args.help {
        println!("{}", Args::HELP);
        return;
    }

    match run(args, &env) {
        Ok(()) => {}
        Err(reason) => {
            eprintln!("uflinux-rdp-session: {reason}");
            std::process::exit(1);
        }
    }
}

/// Everything one session needs after start-up, shared between the connection
/// task, the watchdog and the input bridge.
struct Session {
    args: Args,
    status: StatusReporter,
    idle_timeout: Option<Duration>,
    last_input: Mutex<Instant>,
    /// Read side of the display state, so the watchdog can report a codec
    /// downgrade or a pump failure without owning the pump.
    display: SessionDisplay,
    /// Device reported for the hardware tier, `None` for every other tier.
    codec_device: Option<String>,
    admitted: Mutex<Option<String>>,
}

impl Session {
    fn publish(&self, width: Option<u16>, height: Option<u16>) {
        let username = self
            .admitted
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let tier = self.display.tier();
        let update = StatusUpdate {
            username,
            transport: Some(self.args.transport.as_str().to_owned()),
            codec: Some(caps::codec_label(tier).to_owned()),
            codec_device: Some(if tier == CodecTier::Avc420Hardware {
                self.codec_device.clone()
            } else {
                None
            }),
            width,
            height,
            idle: None,
        };
        self.status.send(&update);
    }

    /// Re-sends the codec line when the pump has downgraded or given up.
    fn refresh_codec(&self, reported: &mut CodecTier) {
        let now = self.display.tier();
        if now == *reported {
            return;
        }
        *reported = now;
        match self.display.failure() {
            Some(reason) => warn!(codec = caps::codec_label(now), reason, "codec changed"),
            None => info!(codec = caps::codec_label(now), "codec downgraded"),
        }
        self.publish(None, None);
    }

    fn mark_input(&self) {
        *self
            .last_input
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Instant::now();
    }

    fn idle_for(&self) -> Duration {
        let since = *self
            .last_input
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Instant::now().saturating_duration_since(since)
    }

    fn admit(&self, username: String) {
        {
            let mut slot = self
                .admitted
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            *slot = Some(username);
        }
        let size = self.display.size();
        self.publish(Some(size.width), Some(size.height));
    }
}

fn run(args: Args, env: &[(String, String)]) -> Result<(), String> {
    tls::install_crypto_provider();
    init_tracing();

    let status = StatusReporter::new(args.status_fd);
    status.send(&StatusUpdate {
        transport: Some(args.transport.as_str().to_owned()),
        ..Default::default()
    });

    let mut config = Config::load(&args.config)
        .map_err(|e| format!("cannot read {}: {e}", args.config.display()))?;
    config.apply_env_overrides(env.iter().cloned());
    config
        .validate()
        .map_err(|e| format!("configuration rejected: {e}"))?;

    // The client descriptor is mandatory and must be a connected stream
    // socket. `from_raw_fd` plus `peer_addr` is the cheapest honest test: a
    // closed or wrong-kind descriptor fails the address call with a real
    // `errno` instead of panicking later inside the connection.
    let client_fd = args.client_fd.ok_or_else(|| {
        "no client socket: pass --client-fd (or set UFLINUX_RDS_CLIENT_FD); the broker passes fd 3".to_owned()
    })?;
    // Take the descriptor over before anything else is decided. It is the one
    // resource a session cannot start without, and a bad one should be the only
    // thing in the journal rather than a footnote after the codec plan. Handing
    // the socket to the runtime waits until the runtime is entered, below.
    let (std_stream, peer) = client_stream(client_fd, args.transport)?;

    let hardware = detect_vaapi(config.codec.vaapi_device.as_deref());
    let negotiation = planned_negotiation(&config, args.transport);
    let h264_available = encoder_present(&config, &hardware);
    let decision = caps::decide(
        &negotiation,
        &hardware,
        if args.transport == Transport::Web {
            H264Policy::Off
        } else {
            config.codec.h264
        },
        config.codec.allow_remotefx,
        h264_available,
    )
    .map_err(|e| match e {
        caps::DecideError::ClientWithoutH264 => {
            "h264 is forced but this client does not negotiate AVC420".to_owned()
        }
        caps::DecideError::NoEncoder => {
            "h264 is forced but no encoder is available on this machine".to_owned()
        }
    })?;
    if let Some(note) = &decision.note {
        warn!(
            codec = caps::codec_label(decision.tier),
            note, "codec plan adjusted"
        );
    }

    let runtime_dir = session_runtime_dir(env);
    // The desktop directory is decided again per user once somebody logs in,
    // because a root session host serves whichever account authenticates.
    let desktop_base = desktop_runtime_base(env);
    let pacer = Pacer::new(config.codec.max_fps, Instant::now());
    let idle_timeout = pick_idle_timeout(&args, &config);

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("rdp-session")
        .enable_all()
        .build()
        .map_err(|e| format!("cannot start the session runtime: {e}"))?;

    // Registering the socket with tokio needs a live reactor, so it happens
    // inside `block_on` rather than out here: creating a runtime does not enter
    // it, and `TcpStream::from_std` panics with "there is no reactor running"
    // when called outside one.
    runtime.block_on(async move {
        let stream: Box<dyn ClientStream> = match std_stream {
            StdClientStream::Tcp(socket) => Box::new(
                TcpStream::from_std(socket)
                    .map_err(|e| format!("cannot register client socket with the runtime: {e}"))?,
            ),
            StdClientStream::Unix(socket) => Box::new(
                UnixStream::from_std(socket)
                    .map_err(|e| format!("cannot register client socket with the runtime: {e}"))?,
            ),
        };
        serve(
            args,
            Plan {
                config: &config,
                hardware: &hardware,
                decision: &decision,
                runtime_dir: &runtime_dir,
                desktop_base,
                status,
                pacer,
                idle_timeout,
                stream,
                peer,
            },
        )
        .await
    })
}

/// Validates the inherited descriptor and turns it into a ready std socket,
/// plus what is on the other end for the log. The caller registers it with the
/// runtime — see [`run`], which is why this returns `std` sockets.
///
/// The expected socket family comes from the transport, not from what the
/// descriptor happens to be: a web session that arrived over TCP would be
/// unencrypted while still being told its TLS was already done, so guessing
/// silently in the wrong direction is a security bug rather than a detail.
fn client_stream(fd: RawFd, transport: Transport) -> Result<(StdClientStream, String), String> {
    if fd < 0 {
        return Err(format!(
            "client descriptor {fd} is not a valid file descriptor"
        ));
    }
    let what = match transport {
        Transport::Rdp => "TCP",
        Transport::Web => "backend",
    };
    // Duplicate first rather than adopting the inherited descriptor: taking
    // ownership of a dead fd is an IO-safety violation that aborts a debug
    // build when it is closed a second time, and a `dup` failure is an ordinary
    // `EBADF` we can report.
    // Safety: `F_DUPFD_CLOEXEC` has no preconditions beyond an integer argument
    // and reports an invalid descriptor as an error.
    let dup = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
    if dup == -1 {
        let error = std::io::Error::last_os_error();
        return Err(format!(
            "fd {fd} is not a connected client {what} socket: {error}"
        ));
    }
    // The duplicate now keeps the connection alive, so the inherited number is
    // released: leaving it open would leak one descriptor per session and keep
    // the socket readable from two places.
    // Safety: `dup` above proved `fd` is open, and nothing else uses it.
    unsafe { libc::close(fd) };
    // Ask the socket what it is before wrapping it in a typed `std` stream:
    // handing an `AF_UNIX` descriptor to `TcpStream::from_raw_fd` would make
    // the later `peer_addr` call fail with `ENOTCONN` and blame the client.
    // Safety: `sockaddr_storage` is a plain C struct of fixed size, and an all
    // -zero value is a valid placeholder for the out-parameter.
    let mut storage = unsafe { std::mem::zeroed::<libc::sockaddr_storage>() };
    let mut len = std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
    // Safety: `dup` is a live descriptor, and `storage`/`len` are a writable
    // buffer of the size `getsockname` was told about.
    let family = unsafe {
        if libc::getsockname(
            dup,
            (&mut storage as *mut libc::sockaddr_storage).cast::<libc::sockaddr>(),
            &mut len,
        ) != 0
        {
            let error = std::io::Error::last_os_error();
            libc::close(dup);
            return Err(format!(
                "fd {fd} is not a connected client {what} socket: {error}"
            ));
        }
        i32::from(storage.ss_family)
    };
    // Safety: `dup` returned a fresh descriptor that nothing else owns, and the
    // family check above proved which typed stream may take it over.
    let std_stream = match family {
        libc::AF_INET | libc::AF_INET6 if transport == Transport::Rdp => {
            StdClientStream::Tcp(unsafe { std::net::TcpStream::from_raw_fd(dup) })
        }
        libc::AF_UNIX if transport == Transport::Web => {
            StdClientStream::Unix(unsafe { StdUnixStream::from_raw_fd(dup) })
        }
        _ => {
            // Safety: `dup` is live and owned here; it is dropped rather than
            // wrapped because it is the wrong kind of socket for this session.
            unsafe { libc::close(dup) };
            return Err(format!(
                "fd {fd} is not a connected client {what} socket: \
                 expected {}, got address family {family}",
                match transport {
                    Transport::Rdp => "AF_INET/AF_INET6",
                    Transport::Web => "AF_UNIX",
                }
            ));
        }
    };
    // tokio refuses a blocking socket, and IronRDP wants the connection to
    // avoid Nagle.
    std_stream
        .set_nonblocking(true)
        .map_err(|e| format!("cannot set fd {fd} non-blocking: {e}"))?;
    let peer = std_stream.describe();
    if let StdClientStream::Tcp(socket) = &std_stream {
        socket
            .set_nodelay(true)
            .map_err(|e| format!("cannot set TCP_NODELAY on fd {fd}: {e}"))?;
    }
    Ok((std_stream, peer))
}

/// The client socket once it is registered with the runtime. A native RDP
/// connection is TCP; a web connection is the gateway's end of the broker's
/// private backend socket. IronRDP only needs the two IO traits, so the two
/// travel as one boxed trait object rather than turning `serve` into a generic.
///
/// The supertrait bounds are what make `Box<dyn ClientStream>` usable as the
/// stream argument of `run_connection_with`, which asks for exactly these.
trait ClientStream: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Sync + Unpin {}

impl ClientStream for tokio::net::TcpStream {}
impl ClientStream for tokio::net::UnixStream {}

/// The inherited client socket before the async runtime exists, kept in the
/// family the transport requires.
enum StdClientStream {
    Tcp(std::net::TcpStream),
    Unix(StdUnixStream),
}

impl StdClientStream {
    fn set_nonblocking(&self, blocking: bool) -> std::io::Result<()> {
        match self {
            Self::Tcp(socket) => socket.set_nonblocking(blocking),
            Self::Unix(socket) => socket.set_nonblocking(blocking),
        }
    }

    /// Both ends of the connection as they should appear in the log. A unix
    /// socket has no address of its own once connected, so the peer's uid is
    /// named instead, which is the useful fact about a gateway connection.
    fn describe(&self) -> String {
        match self {
            Self::Tcp(socket) => {
                let peer = socket.peer_addr().ok();
                let local = socket.local_addr().ok();
                match (local, peer) {
                    (Some(local), Some(peer)) => format!("{local} -> {peer}"),
                    (None, Some(peer)) => format!("unknown -> {peer}"),
                    _ => "unknown".to_owned(),
                }
            }
            Self::Unix(socket) => match peer_uid(socket) {
                Some(uid) => format!("unix: gateway uid {uid}"),
                None => "unix: gateway".to_owned(),
            },
        }
    }
}

/// The uid on the other end of a connected unix socket, or `None` when the
/// kernel declines to say. `UCred::peer_cred` is still unstable, so this asks
/// for `SO_PEERCRED` directly.
fn peer_uid(socket: &StdUnixStream) -> Option<u32> {
    let mut cred: libc::ucred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: `cred` and `len` are writable storage of the sizes the kernel
    // expects for SO_PEERCRED, and the socket is one we just accepted.
    let rc = unsafe {
        libc::getsockopt(
            socket.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            std::ptr::addr_of_mut!(cred).cast(),
            &mut len,
        )
    };
    if rc == 0 { Some(cred.uid) } else { None }
}

/// Everything one session needs that is decided before the async runtime
/// exists, carried into [`serve`] as one value.
struct Plan<'a> {
    config: &'a Config,
    hardware: &'a HardwareCaps,
    decision: &'a Decision,
    runtime_dir: &'a Path,
    /// Root under which the authenticated user's private desktop directory is
    /// created. Owned, because the desktop is started from the credential
    /// validator, which outlives the borrow of the environment snapshot.
    desktop_base: DesktopBase,
    status: StatusReporter,
    pacer: Pacer,
    idle_timeout: Option<Duration>,
    stream: Box<dyn ClientStream>,
    peer: String,
}

/// What a desktop needs to be started with, decided before the connection and
/// consumed by [`DesktopLauncher`] once somebody has logged in.
struct DesktopPlan {
    command: String,
    base: DesktopBase,
    pacer: Pacer,
    bitmap_tier: CodecTier,
    require_h264: bool,
    encoder: Option<Box<dyn H264Backend>>,
    encoder_factory: EncoderFactory,
}

/// Where per-user desktop directories are created, and whether they have to be
/// namespaced by uid.
struct DesktopBase {
    root: PathBuf,
    /// `true` when this process runs as root and therefore serves whichever
    /// account authenticates: each desktop then needs its own uid directory.
    /// An unprivileged host's base directory is already private to it.
    privileged: bool,
}

/// Starts the compositor and the frame pump the first time a connection
/// authenticates, and remembers the pump so the connection can take it down.
///
/// The desktop cannot be started when the session host starts: the host runs as
/// root (the broker runs as root so it can drop privileges at all), so the
/// desktop user is only known after credential validation. Everything needed to
/// spawn later is prepared here instead.
///
/// `Box<dyn H264Backend>` is `Send` but not `Sync`, which is why the payload
/// sits behind a `Mutex`: the validator that calls this is held as
/// `Arc<dyn CredentialValidator>`, and that trait requires `Send + Sync`.
struct DesktopLauncher {
    plan: Mutex<Option<DesktopPlan>>,
    pump: Mutex<Option<FramePump>>,
    desktop_endpoint: clipboard::EndpointSlot,
    #[cfg(feature = "pam")]
    login: Mutex<Option<std::sync::mpsc::Sender<()>>>,
    display: SessionDisplay,
    peer: String,
    tier: CodecTier,
}

/// What one successful launch produced, for the log line.
#[derive(Debug)]
struct StartedDesktop {
    width: u32,
    height: u32,
    user: Option<String>,
}

impl DesktopLauncher {
    fn new(plan: DesktopPlan, display: SessionDisplay, peer: String, tier: CodecTier) -> Self {
        Self {
            plan: Mutex::new(Some(plan)),
            pump: Mutex::new(None),
            desktop_endpoint: Default::default(),
            #[cfg(feature = "pam")]
            login: Mutex::new(None),
            display,
            peer,
            tier,
        }
    }

    /// `true` while a desktop has yet to be started. A connection that never
    /// authenticates never calls this, so it never spawns anything.
    fn pending(&self) -> bool {
        self.plan
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_some()
    }

    /// Takes the plan out (so a second call cannot start a second desktop) and
    /// spawns the compositor as `username`, then the pump that feeds it to the
    /// client. Blocking: it forks, execs and waits for the compositor's socket.
    fn start(
        &self,
        username: Option<&str>,
        session_id: Option<&str>,
    ) -> Result<StartedDesktop, String> {
        let plan = self
            .plan
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
            .ok_or_else(|| "the desktop was already started".to_owned())?;

        // The authenticated account decides the uid, the supplementary groups
        // and the ownership of the runtime directory. An account the password
        // database does not know (only reachable without PAM) keeps the host's
        // own privileges, which is what an unprivileged host would have used.
        let owner = username.and_then(auth::DesktopOwner::lookup);
        // An account the password database does not know (only reachable when
        // PAM is not built in) keeps the host's own uid, which is what the host
        // would have used before this existed.
        let uid = owner.as_ref().map_or_else(
            || {
                // Safety: `getuid` has no preconditions.
                unsafe { libc::getuid() }
            },
            |owner| owner.uid,
        );
        // A privileged host serves whichever account authenticates, so the
        // desktop directory is namespaced by that uid; an unprivileged host
        // already has a private base directory.
        let dir = auth::prepare_desktop_dir(
            &plan.base.root,
            plan.base.privileged.then_some(uid),
            owner.as_ref(),
        )
        .map_err(|e| {
            format!(
                "cannot prepare desktop under {}: {e}",
                plan.base.root.display()
            )
        })?;

        // The client's own geometry is adopted a moment later, through
        // `request_initial_size` and the pump's `Resize`; this is the size the
        // compositor is created with.
        let initial = self.display.size();
        let mut spec = DesktopSpec::new(
            plan.command,
            u32::from(initial.width),
            u32::from(initial.height),
            dir,
        );
        spec.session_id = session_id.map(str::to_owned);
        if let Some(owner) = &owner {
            spec = spec.with_user(owner.name.clone());
        }
        let backend = DesktopBackend::spawn(spec)
            .map_err(|e| format!("cannot start the desktop compositor: {e}"))?;
        let (width, height) = backend.size();
        let endpoint = owner.as_ref().and_then(|owner| {
            Some(clipboard::Endpoint::new(
                backend.socket_path()?,
                owner.uid,
                owner.gid,
            ))
        });

        let (pump, sender) = FramePump::spawn(
            backend,
            PumpConfig {
                shared: self.display.shared_arc(),
                updates: self.display.update_sender(),
                events: self.display.event_slot(),
                pacer: plan.pacer,
                tier: self.tier,
                bitmap_tier: plan.bitmap_tier,
                require_h264: plan.require_h264,
                encoder: plan.encoder,
                encoder_factory: plan.encoder_factory,
                degrader: uflinux_rds_server::degrade::Degrader::new(self.tier),
            },
        )
        .map_err(|e| format!("cannot start the frame pump: {e}"))?;

        // Input events that arrived before this point were dropped; from here
        // on the bridge finds the pump through the display handler.
        *self
            .desktop_endpoint
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = endpoint;
        self.display.attach_pump(sender);
        *self
            .pump
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(pump);

        Ok(StartedDesktop {
            width,
            height,
            user: owner.map(|owner| owner.name),
        })
    }

    /// Takes the frame pump down, returning the compositor for the caller to
    /// shut its process group down. `None` when no desktop was ever started.
    fn take_pump(&self) -> Option<FramePump> {
        self.pump
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
    }
}

async fn serve(args: Args, plan: Plan<'_>) -> Result<(), String> {
    let Plan {
        config,
        hardware,
        decision,
        runtime_dir,
        desktop_base,
        status,
        pacer,
        idle_timeout,
        stream,
        peer,
    } = plan;
    // --- display state (no desktop yet) ------------------------------------
    // The compositor is started by the credential validator, so the display
    // handler answers the client's size queries from this value alone until
    // then, and takes over the pump's shared state once it exists.
    let display = SessionDisplay::from_size(DESKTOP_WIDTH, DESKTOP_HEIGHT, decision.tier);
    #[cfg(feature = "egfx")]
    let factory = display.factory();

    let command = args
        .session_command
        .clone()
        .or_else(|| Some(config.session_command.clone()))
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| uflinux_rds_wayland::DesktopSpec::DEFAULT_COMMAND.to_owned());
    let device = decision.device.clone();
    let encoder = build_encoder(
        decision.tier,
        hardware,
        config.codec.vaapi_device.as_deref(),
    );
    let factory_tier = EncoderFactoryBox::new(hardware.clone(), config.codec.vaapi_device.clone());
    let session_transport = args.transport;
    let launcher = Arc::new(DesktopLauncher::new(
        DesktopPlan {
            command,
            base: desktop_base,
            pacer,
            bitmap_tier: if config.codec.allow_remotefx {
                CodecTier::RemoteFx
            } else {
                CodecTier::Qoi
            },
            require_h264: config.codec.h264 == H264Policy::Force
                && session_transport == Transport::Rdp,
            encoder,
            encoder_factory: factory_tier.into_box(),
        },
        display.clone(),
        peer.clone(),
        decision.tier,
    ));

    let session = Arc::new(Session {
        args,
        status,
        idle_timeout,
        last_input: Mutex::new(Instant::now()),
        display: display.clone(),
        codec_device: device,
        admitted: Mutex::new(None),
    });
    // No geometry yet: the desktop that has these pixels does not exist until
    // somebody logs in.
    session.publish(None, None);

    // --- connection --------------------------------------------------------
    let security = session_security(config, runtime_dir, session.args.transport)?;
    let parts = ServerParts {
        security,
        display,
        bridge: InputBridge::new(Arc::clone(&session)),
        desktop_endpoint: Arc::clone(&launcher.desktop_endpoint),
        codecs: advertised_codecs(config.codec.allow_remotefx, decision.tier),
        quant: remote_fx_quant(config.codec.rfx_quant),
        validator: validator(Arc::clone(&session), config, Arc::clone(&launcher)),
        #[cfg(feature = "egfx")]
        factory: Some(factory),
    };
    let mut server = build_server(parts);
    let mut terminate =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .map_err(|error| format!("cannot watch for session termination: {error}"))?;
    let connection_events = server.event_sender().clone();

    let watchdog = tokio::spawn(idle_watchdog(Arc::clone(&session), connection_events));

    // A web session arrives already decrypted by the gateway's TLS layer, so
    // the server must not try to terminate TLS a second time.
    let tls_mode = if session.args.transport == Transport::Web {
        TransportTls::AlreadyDone
    } else {
        TransportTls::Managed
    };
    let outcome = tokio::select! {
        result = server.run_connection_with(stream, tls_mode) => result.map(|_| ()),
        _ = terminate.recv() => Ok(()),
    };
    watchdog.abort();
    if let Err(error) = &outcome {
        warn!(error = %error, "connection ended with an error");
        debug!(?error, "RDP connection error detail");
    }

    // --- teardown ----------------------------------------------------------
    session.publish(None, None);
    match launcher.take_pump() {
        Some(pump) => {
            if let Some(backend) = pump.join()
                && let Err(error) = backend.shutdown()
            {
                warn!(error = %error, "compositor did not shut down cleanly");
            }
        }
        // Nobody authenticated, so there is no desktop to take down.
        None => debug!(
            peer,
            desktop = launcher.pending(),
            "no desktop was ever started"
        ),
    }

    #[cfg(feature = "pam")]
    launcher
        .login
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .take();

    outcome.map_err(|e| format!("session ended: {e}"))
}

/// Which security layer the server terminates itself.
#[derive(Clone)]
enum Security {
    /// Server-authenticated TLS.
    Tls(tokio_rustls::TlsAcceptor),
    /// Nothing: the gateway already terminated TLS and handed the connection
    /// over a private unix socket. X.224 then confirms with an empty security
    /// protocol, which is the only reading that matches what the browser's
    /// wasm client does — it never starts a TLS handshake inside the WebSocket.
    Web,
}

/// The native listener uses TLS and PAM, and the private browser socket
/// carries RDP after the gateway terminates TLS.
fn session_security(
    config: &Config,
    runtime_dir: &Path,
    transport: Transport,
) -> Result<Security, String> {
    if transport == Transport::Web {
        return Ok(Security::Web);
    }
    // Reuse the key so a client that accepted it once is not warned again; fall
    // back to the session runtime directory when the state directory is not
    // writable for this process.
    let state_dir = PathBuf::from(STATE_DIR);
    let identity = tls::self_signed_identity(&state_dir, &config.rdp_bind)
        .or_else(|_| tls::self_signed_identity(runtime_dir, &config.rdp_bind))
        .map_err(|e| format!("cannot prepare the session TLS identity: {e}"))?;
    let acceptor = identity
        .acceptor()
        .map_err(|e| format!("cannot build the session TLS acceptor: {e}"))?;
    Ok(Security::Tls(acceptor))
}

/// The pieces `build_server` needs, taken by value because the builder states
/// are unnameable and the chain has to be one expression.
struct ServerParts {
    security: Security,
    display: SessionDisplay,
    bridge: InputBridge,
    desktop_endpoint: clipboard::EndpointSlot,
    codecs: BitmapCodecs,
    quant: Quant,
    validator: Arc<dyn CredentialValidator>,
    #[cfg(feature = "egfx")]
    factory: Option<SessionGfxFactorySlot>,
}

/// `Box<dyn GfxServerFactory>` is not `Clone`, so the factory is carried here
/// as a type alias to keep the struct declaration readable.
#[cfg(feature = "egfx")]
type SessionGfxFactorySlot = uflinux_rds_server::display::SessionGfxFactory;

/// Assembles the server.
///
/// `RdpServerBuilder`'s state types are private to `ironrdp-server`, so the
/// whole thing has to be a single chain that ends in `build()`; the only way to
/// vary it per feature is a `#[cfg]`-gated rebinding of the chain.
fn build_server(parts: ServerParts) -> RdpServer {
    let addr = bind_addr();
    let ServerParts {
        security,
        display,
        bridge,
        desktop_endpoint,
        codecs,
        quant,
        validator,
        #[cfg(feature = "egfx")]
        factory,
    } = parts;

    let started = RdpServer::builder().with_addr(addr);
    let secured = match security {
        Security::Tls(acceptor) => started.with_tls(acceptor),
        Security::Web => started.with_no_security(),
    };
    let sound: Option<Box<dyn ironrdp_server::SoundServerFactory>> =
        if bridge.session.args.transport == Transport::Rdp {
            Some(Box::new(sound::Factory::new(Arc::clone(&desktop_endpoint))))
        } else {
            None
        };
    let displayed = secured
        .with_input_handler(bridge)
        .with_display_handler(display)
        .with_cliprdr_factory(Some(Box::new(clipboard::Factory::new(Arc::clone(
            &desktop_endpoint,
        )))))
        .with_sound_factory(sound);
    #[cfg(feature = "egfx")]
    let displayed = displayed.with_gfx_factory(factory.map(|factory| {
        let boxed: Box<dyn GfxServerFactory> = Box::new(factory);
        boxed
    }));
    displayed
        .with_bitmap_codecs(codecs)
        .with_remotefx_quant(quant)
        .with_remotefx_entropy_coder(None)
        .with_connection_policy(ConnectionPolicy::Reject)
        .with_credential_validator(Some(validator))
        .build()
}

/// The address the session reports to the client. Nothing is bound: the
/// connection already exists, and this only appears in the capability sets and
/// in `GetLocalAddr`.
fn bind_addr() -> std::net::SocketAddr {
    std::env::var("UFLINUX_RDS_RDP_PORT")
        .ok()
        .and_then(|value| value.trim().parse::<u16>().ok())
        .and_then(|port| {
            format!("127.0.0.1:{port}")
                .parse::<std::net::SocketAddr>()
                .ok()
        })
        .unwrap_or(([127, 0, 0, 1], 3389).into())
}

/// What we tell the client we can do, before the real negotiation.
///
/// The EGFX capability sets are only knowable once the DVC channel opens,
/// which happens inside `run_connection`; the tier is therefore chosen
/// optimistically from configuration plus hardware and corrected afterwards by
/// the pump's degrader.
fn planned_negotiation(config: &Config, transport: Transport) -> Negotiation {
    let egfx = if transport == Transport::Rdp
        && cfg!(feature = "egfx")
        && config.codec.h264 != H264Policy::Off
    {
        Some(caps::EgxFacts {
            avc420: true,
            avc444: true,
        })
    } else {
        None
    };
    Negotiation {
        egfx,
        remotefx: config.codec.allow_remotefx,
        qoi: true,
    }
}

/// Whether this process could actually build an H.264 encoder right now.
fn encoder_present(config: &Config, hardware: &HardwareCaps) -> bool {
    if hardware.vaapi {
        return true;
    }
    #[cfg(feature = "h264-software")]
    {
        let _ = config;
        uflinux_rds_codec::software::SoftwareH264Encoder::from_system_library().is_ok()
    }
    #[cfg(not(feature = "h264-software"))]
    {
        let _ = config;
        false
    }
}

/// The bitmap codec capability set. An empty configuration means "everything
/// this build has", so only the things we actually want to suppress are named.
fn advertised_codecs(allow_remotefx: bool, tier: CodecTier) -> BitmapCodecs {
    let mut items: Vec<&str> = Vec::new();
    if !allow_remotefx || tier == CodecTier::Qoi {
        items.push("remotefx:off");
    }
    match server_codecs_capabilities(&items) {
        Ok(codecs) => codecs,
        Err(reason) => {
            warn!(reason, "bitmap codec list rejected; advertising nothing");
            BitmapCodecs::default()
        }
    }
}

/// RemoteFX quantisation, from the operator's 0..=63 byte.
fn remote_fx_quant(byte: u8) -> Quant {
    let bands = quant_subbands(byte);
    // `quant_subbands` clamps into the legal 6..=15 window, so this cannot
    // fail; the default is the same Windows pattern the byte is centred on.
    Quant::try_new(
        bands[0], bands[1], bands[2], bands[3], bands[4], bands[5], bands[6], bands[7], bands[8],
        bands[9],
    )
    .unwrap_or_default()
}

/// Builds the encoder a tier needs, or `None` when this machine cannot provide
/// one — which the pump answers with a downgrade rather than an error.
fn build_encoder(
    tier: CodecTier,
    hardware: &HardwareCaps,
    device: Option<&Path>,
) -> Option<Box<dyn H264Backend>> {
    match tier {
        CodecTier::Avc420Hardware => {
            let config = VaapiConfig::from_hardware(hardware).unwrap_or_default();
            let _ = device;
            match uflinux_rds_codec::VaapiH264Encoder::new(config) {
                Ok(encoder) => Some(Box::new(encoder)),
                Err(error) => {
                    warn!(error = %error, "no VA-API encoder");
                    None
                }
            }
        }
        CodecTier::Avc420Software | CodecTier::Avc444 => {
            #[cfg(feature = "h264-software")]
            {
                match uflinux_rds_codec::software::SoftwareH264Encoder::from_system_library() {
                    Ok(encoder) => Some(Box::new(encoder)),
                    Err(error) => {
                        warn!(error = %error, "no OpenH264 encoder");
                        None
                    }
                }
            }
            #[cfg(not(feature = "h264-software"))]
            {
                None
            }
        }
        CodecTier::RemoteFx | CodecTier::Qoi => None,
    }
}

/// Captures what an encoder needs so a mid-session downgrade can build one.
struct EncoderFactoryBox {
    hardware: HardwareCaps,
    device: Option<PathBuf>,
}

impl EncoderFactoryBox {
    fn new(hardware: HardwareCaps, device: Option<PathBuf>) -> Self {
        Self { hardware, device }
    }

    fn into_box(self) -> Box<dyn Fn(CodecTier) -> Option<Box<dyn H264Backend>> + Send> {
        let device = self.device;
        let hardware = self.hardware;
        Box::new(move |tier| build_encoder(tier, &hardware, device.as_deref()))
    }
}

/// The authenticator plus the `allowed_users`/`allowed_groups` admission check.
///
/// Successful authentication is also what starts the desktop: until somebody
/// has been checked there is no user to run a compositor as, and IronRDP does
/// not consult the display handler until after this has returned `Accept`.
struct Admit {
    #[cfg(not(feature = "pam"))]
    authenticator: StubAuthenticator,
    policy: AdmissionPolicy,
    session: Arc<Session>,
    desktop: Arc<DesktopLauncher>,
}

#[async_trait::async_trait]
impl CredentialValidator for Admit {
    async fn validate(
        &self,
        credentials: &Credentials,
    ) -> Result<CredentialDecision, CredentialValidationError> {
        let policy = self.policy.clone();
        let username = credentials.username.clone();
        let password = credentials.password.clone();
        let desktop = Arc::clone(&self.desktop);
        #[cfg(not(feature = "pam"))]
        let authenticator = self.authenticator.clone();
        #[cfg(feature = "pam")]
        let (release, closed) = std::sync::mpsc::channel();
        let (ready, outcome) = tokio::sync::oneshot::channel();

        tokio::task::spawn_blocking(move || {
            #[cfg(feature = "pam")]
            let login = match auth::login::Login::open(
                PAM_SERVICE,
                username,
                password,
                &desktop.peer,
                &policy,
            ) {
                Ok(login) => login,
                Err(error) => {
                    let _ = ready.send(Err(error));
                    return;
                }
            };
            #[cfg(feature = "pam")]
            let (username, session_id) = (login.username.clone(), login.session_id.as_deref());
            #[cfg(not(feature = "pam"))]
            let (username, session_id) = {
                let admitted = authenticator
                    .authenticate(&username, &password)
                    .and_then(|()| {
                        auth::admit(&policy, &username)
                            .then_some(())
                            .ok_or(AuthError::NotAllowed)
                    });
                if let Err(error) = admitted {
                    let _ = ready.send(Err(error));
                    return;
                }
                (username, None)
            };

            let started = desktop.start(Some(&username), session_id);
            let wait_for_close = started.is_ok();
            drop(desktop);
            let delivered = ready.send(Ok((started, username))).is_ok();
            #[cfg(feature = "pam")]
            if wait_for_close && delivered {
                let _ = closed.recv();
            }
            #[cfg(not(feature = "pam"))]
            let _ = (wait_for_close, delivered);
        });

        match outcome.await.map_err(CredentialValidationError::new)? {
            Ok((Ok(started), username)) => {
                #[cfg(feature = "pam")]
                {
                    *self
                        .desktop
                        .login
                        .lock()
                        .unwrap_or_else(|error| error.into_inner()) = Some(release);
                }
                info!(
                    peer = %self.desktop.peer,
                    width = started.width,
                    height = started.height,
                    user = started.user.as_deref().unwrap_or("<this process>"),
                    tier = caps::codec_label(self.desktop.tier),
                    "session started"
                );
                self.session.admit(username);
                Ok(CredentialDecision::Accept)
            }
            Ok((Err(reason), _)) => {
                warn!(reason, "desktop could not start; refusing the login");
                Ok(CredentialDecision::Reject)
            }
            Err(AuthError::Denied | AuthError::NotAllowed) => Ok(CredentialDecision::Reject),
            Err(error) => Err(CredentialValidationError::new(error)),
        }
    }
}

fn validator(
    session: Arc<Session>,
    config: &Config,
    desktop: Arc<DesktopLauncher>,
) -> Arc<dyn CredentialValidator> {
    Arc::new(Admit {
        #[cfg(not(feature = "pam"))]
        authenticator: StubAuthenticator::new(session.args.session_user().map(str::to_owned)),
        policy: AdmissionPolicy::from_config(config),
        session,
        desktop,
    })
}

/// Forwards IronRDP input events to the compositor through the pump.
///
/// The pump is looked up per event rather than captured: the desktop, and with
/// it the pump, only exists once somebody has authenticated. Events before that
/// are dropped.
#[derive(Clone)]
struct InputBridge {
    session: Arc<Session>,
}

impl InputBridge {
    fn new(session: Arc<Session>) -> Self {
        Self { session }
    }

    fn send(&self, event: WlPointerEvent) {
        self.session.mark_input();
        if let Some(pump) = self.session.display.pump() {
            let _ = pump.send(PumpCommand::Pointer(event));
        }
    }

    fn send_key(&self, keysym: Option<u32>, pressed: bool) {
        self.session.mark_input();
        // The virtual keyboard accepts only keysyms present in its keymap.
        if let Some(keysym) = keysym {
            let event = WlKeyboardEvent { keysym, pressed };
            if event.keycode().is_some()
                && let Some(pump) = self.session.display.pump()
            {
                let _ = pump.send(PumpCommand::Keyboard(event));
            }
        }
    }
}

impl RdpServerInputHandler for InputBridge {
    fn keyboard(&mut self, event: KeyboardEvent) {
        match event {
            KeyboardEvent::Pressed { code, extended } => {
                self.send_key(keysym_for(code, extended), true)
            }
            KeyboardEvent::Released { code, extended } => {
                self.send_key(keysym_for(code, extended), false)
            }
            KeyboardEvent::UnicodePressed(value) => {
                self.send_key((value < 0x100).then_some(u32::from(value)), true)
            }
            KeyboardEvent::UnicodeReleased(value) => {
                self.send_key((value < 0x100).then_some(u32::from(value)), false)
            }
            KeyboardEvent::Synchronize(_) => {}
        }
    }

    fn mouse(&mut self, event: MouseEvent) {
        match event {
            MouseEvent::Move { x, y } => self.send(WlPointerEvent::Move { x, y }),
            MouseEvent::RelMove { x, y } => self.send(WlPointerEvent::MoveRelative { x, y }),
            MouseEvent::Button {
                x,
                y,
                button,
                pressed,
            } => {
                self.send(WlPointerEvent::Move { x, y });
                if let Some(code) = wl_button(button) {
                    self.send(WlPointerEvent::Button {
                        button: code,
                        pressed,
                    });
                }
            }
            MouseEvent::ButtonRel {
                x,
                y,
                button,
                pressed,
            } => {
                self.send(WlPointerEvent::MoveRelative { x, y });
                if let Some(code) = wl_button(button) {
                    self.send(WlPointerEvent::Button {
                        button: code,
                        pressed,
                    });
                }
            }
            MouseEvent::VerticalScroll { value } => self.send(WlPointerEvent::Wheel {
                axis_x: 0,
                axis_y: i32::from(value),
            }),
            MouseEvent::HorizontalScroll { value } => self.send(WlPointerEvent::Wheel {
                axis_x: i32::from(value),
                axis_y: 0,
            }),
            MouseEvent::Scroll { x, y } => self.send(WlPointerEvent::Wheel {
                axis_x: x,
                axis_y: y,
            }),
            _ => {}
        }
    }
}

fn wl_button(button: MouseButton) -> Option<u16> {
    match button {
        MouseButton::Left => Some(button::LEFT),
        MouseButton::Right => Some(button::RIGHT),
        MouseButton::Middle => Some(button::MIDDLE),
        MouseButton::X1 => Some(button::SIDE),
        MouseButton::X2 => Some(button::EXTRA),
        _ => None,
    }
}

/// Reports idleness and enforces `--idle-timeout`.
async fn idle_watchdog(session: Arc<Session>, events: mpsc::UnboundedSender<ServerEvent>) {
    let limit = session.idle_timeout;
    let notice = limit
        .unwrap_or(Duration::from_secs(60))
        .min(Duration::from_secs(60));
    let mut ticker = tokio::time::interval(WATCHDOG_PERIOD);
    let mut reported_idle = false;
    let mut reported_tier = session.display.tier();
    let mut reported_size = session.display.size();
    let mut reported_input = None;
    loop {
        ticker.tick().await;
        let idle_for = session.idle_for();
        if limit.is_some_and(|limit| idle_for >= limit) {
            info!(?idle_for, "idle timeout; ending the session");
            let _ = events.send(ServerEvent::Quit("idle timeout".to_owned()));
            // No pump when the client never authenticated: there is no desktop
            // to stop, and the teardown below has nothing to join.
            if let Some(pump) = session.display.pump() {
                let _ = pump.send(PumpCommand::Stop);
            }
            return;
        }
        if let Some(reason) = session.display.failure() {
            let _ = events.send(ServerEvent::Quit(reason));
            return;
        }
        session.refresh_codec(&mut reported_tier);
        let size = session.display.size();
        if size != reported_size {
            reported_size = size;
            session.publish(Some(size.width), Some(size.height));
        }
        let last_input = *session
            .last_input
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let idle = idle_for >= notice;
        if idle != reported_idle || (!idle && reported_input != Some(last_input)) {
            reported_idle = idle;
            reported_input = Some(last_input);
            session.status.send_idle(idle);
        }
    }
}

/// `--idle-timeout` wins over the configured default; zero means "never".
fn pick_idle_timeout(args: &Args, config: &Config) -> Option<Duration> {
    let secs = args.idle_timeout.unwrap_or(config.idle_timeout_secs);
    if secs == 0 {
        None
    } else {
        Some(Duration::from_secs(secs))
    }
}

/// Where one session keeps its state: the TLS fallback and the nested
/// compositor's socket.
///
/// The last component is per-process, so two concurrent sessions of the same
/// user (every packaged session runs as the broker's uid) get separate
/// directories. They have to: the session launcher writes its ready file and a
/// private D-Bus socket into `XDG_RUNTIME_DIR`, and a compositor prunes stale
/// `wayland-*` sockets from the directory it starts in, so a shared directory
/// would make the second session tear the first one's display down.
fn session_runtime_dir(env: &[(String, String)]) -> PathBuf {
    let base = match env_value(env.to_vec(), "XDG_RUNTIME_DIR") {
        Some(dir) => {
            let path = PathBuf::from(dir);
            if path.is_absolute() {
                path
            } else {
                state_runtime_base()
            }
        }
        None => state_runtime_base(),
    };
    base.join(format!("rdp-{}", std::process::id()))
}

/// Fallback root for session state when there is no `XDG_RUNTIME_DIR`.
fn state_runtime_base() -> PathBuf {
    // Safety: `getuid` has no preconditions.
    let uid = unsafe { libc::getuid() };
    PathBuf::from(format!("{STATE_DIR}/sessions/{uid}"))
}

/// Where the authenticated user's desktop directory is created, and whether it
/// has to be namespaced by uid.
///
/// A host started by an unprivileged user keeps its desktops under its own
/// `XDG_RUNTIME_DIR`, which nothing else can write to. The packaged broker runs
/// as root (it has to: it drops the desktop to the authenticated account), and
/// then `XDG_RUNTIME_DIR` is root's — mode `0700`, so a compositor that has
/// dropped to another account could not even traverse it. A privileged host
/// therefore uses a shared base under `/run`, with private per-user session directories.
fn desktop_runtime_base(env: &[(String, String)]) -> DesktopBase {
    // Safety: `geteuid` has no preconditions.
    desktop_base_for(unsafe { libc::geteuid() } == 0, env)
}

/// [`desktop_runtime_base`] with the effective uid decided by the caller, so the
/// choice can be tested without running the test as root.
fn desktop_base_for(privileged: bool, env: &[(String, String)]) -> DesktopBase {
    let root = if privileged {
        PathBuf::from(DESKTOP_RUNTIME_ROOT)
    } else {
        match env_value(env.to_vec(), "XDG_RUNTIME_DIR") {
            Some(dir) if Path::new(&dir).is_absolute() => PathBuf::from(dir),
            _ => state_runtime_base(),
        }
    };
    DesktopBase { root, privileged }
}

/// First non-empty value of `key` in the environment snapshot.
fn env_value(env: impl IntoIterator<Item = (String, String)>, key: &str) -> Option<String> {
    env.into_iter()
        .find(|(name, _)| name == key)
        .map(|(_, value)| value)
        .filter(|value| !value.is_empty())
}

fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}

/// Keeps `ArgError` referenced for the doc link and future use.
const _: fn(&ArgError) -> String = |error| error.to_string();

/// Keeps `DesktopSize` in the public surface of the binary's imports.
const _: fn(&DesktopSize) -> u16 = |size| size.width;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_dir_honours_xdg_runtime_dir() {
        let dir = session_runtime_dir(&[("XDG_RUNTIME_DIR".to_owned(), "/run/user/1000".into())]);
        assert!(dir.starts_with("/run/user/1000"));
        assert!(dir.is_absolute());
        let name = dir.file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.starts_with("rdp-"), "{name}");
    }

    #[test]
    fn runtime_dir_falls_back_to_the_state_directory() {
        let dir = session_runtime_dir(&[("XDG_RUNTIME_DIR".to_owned(), "relative".into())]);
        assert!(dir.starts_with(&format!("{STATE_DIR}/sessions/")));
        let empty = session_runtime_dir(&[]);
        assert_eq!(dir, empty);
    }

    #[test]
    fn runtime_dir_is_per_process() {
        // Two session hosts for the same uid must not share a compositor
        // directory; within one process the value is stable.
        let env = [("XDG_RUNTIME_DIR".to_owned(), "/run/user/0".to_owned())];
        assert_eq!(session_runtime_dir(&env), session_runtime_dir(&env));
        assert!(session_runtime_dir(&env).ends_with(&format!("rdp-{}", std::process::id())));
    }

    #[test]
    fn a_privileged_desktop_base_ignores_roots_own_runtime_dir() {
        // Root's XDG_RUNTIME_DIR is mode 0700: a compositor that has dropped to
        // the authenticated account could not traverse it.
        let env = [("XDG_RUNTIME_DIR".to_owned(), "/run/user/0".to_owned())];
        let base = desktop_base_for(true, &env);
        assert_eq!(base.root, PathBuf::from(DESKTOP_RUNTIME_ROOT));
        assert!(base.privileged);
    }

    #[test]
    fn an_unprivileged_desktop_base_uses_the_owners_runtime_dir() {
        let env = [("XDG_RUNTIME_DIR".to_owned(), "/run/user/1000".to_owned())];
        let base = desktop_base_for(false, &env);
        assert_eq!(base.root, PathBuf::from("/run/user/1000"));
        assert!(!base.privileged);

        // A relative or missing value cannot hold a Wayland socket.
        let relative = desktop_base_for(false, &[("XDG_RUNTIME_DIR".to_owned(), ".".to_owned())]);
        assert_eq!(relative.root, desktop_base_for(false, &[]).root);
        assert!(relative.root.starts_with(&format!("{STATE_DIR}/sessions/")));
    }

    fn launcher_for_test(base: &Path) -> DesktopLauncher {
        let plan = DesktopPlan {
            command: "/bin/true".to_owned(),
            base: DesktopBase {
                root: base.to_path_buf(),
                privileged: false,
            },
            pacer: Pacer::new(30, Instant::now()),
            bitmap_tier: CodecTier::Qoi,
            require_h264: false,
            encoder: None,
            encoder_factory: Box::new(|_| None),
        };
        DesktopLauncher::new(
            plan,
            SessionDisplay::from_size(1024, 768, CodecTier::Qoi),
            "10.0.0.1:1".to_owned(),
            CodecTier::Qoi,
        )
    }

    #[test]
    fn a_launcher_that_was_never_started_has_no_desktop_to_take_down() {
        // What a connection that never authenticates leaves behind: no plan was
        // consumed, no compositor was spawned, and teardown has nothing to do.
        let base = std::env::temp_dir().join(format!("uflinux-rds-launch-{}", std::process::id()));
        let launcher = launcher_for_test(&base);
        assert!(launcher.pending());
        assert!(launcher.take_pump().is_none());
        assert!(!base.exists(), "nothing may be created before a login");
    }

    #[test]
    fn a_failed_desktop_start_consumes_the_plan_so_it_cannot_be_retried() {
        // `/bin/true` is not a compositor: it exits before its socket appears.
        // The plan is still consumed, so a second login cannot spawn a second
        // desktop for the same connection.
        let base = std::env::temp_dir().join(format!("uflinux-rds-spawn-{}", std::process::id()));
        let launcher = launcher_for_test(&base);
        let error = launcher
            .start(None, None)
            .expect_err("a compositor that exits immediately must fail");
        assert!(error.contains("compositor"), "{error}");
        assert!(!launcher.pending());
        assert!(launcher.take_pump().is_none());
        assert!(
            launcher
                .start(None, None)
                .expect_err("a second start must be refused")
                .contains("already started")
        );
        let _ = std::fs::remove_dir_all(&base);
    }
}
