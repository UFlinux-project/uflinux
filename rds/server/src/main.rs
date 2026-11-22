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
use uflinux_rds_server::auth::{
    self, AuthError, Authenticator, Policy as AdmissionPolicy, StubAuthenticator,
};
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
const PAM_SERVICE: &str = "uflinux-rds";

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
        let update = StatusUpdate {
            username,
            transport: Some(self.args.transport.as_str().to_owned()),
            codec: Some(caps::codec_label(self.display.tier()).to_owned()),
            codec_device: Some(self.codec_device.clone()),
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
        self.publish(None, None);
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
    let negotiation = planned_negotiation(&config);
    let h264_available = encoder_present(&config, &hardware);
    let decision = caps::decide(
        &negotiation,
        &hardware,
        config.codec.h264,
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
    fn start(&self, username: Option<&str>) -> Result<StartedDesktop, String> {
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
        let dir = auth::desktop_runtime_dir(
            &plan.base.root,
            if plan.base.privileged {
                Some(uid)
            } else {
                None
            },
        );
        let dir = auth::prepare_desktop_dir(&dir, owner.as_ref())
            .map_err(|e| format!("cannot prepare {}: {e}", dir.display()))?;

        // The client's own geometry is adopted a moment later, through
        // `request_initial_size` and the pump's `Resize`; this is the size the
        // compositor is created with.
        let initial = self.display.size();
        let mut spec = DesktopSpec::new(
            plan.command,
            u32::from(initial.width),
            u32::from(initial.height),
            dir.join("wayland"),
        );
        if let Some(owner) = &owner {
            spec = spec.with_user(owner.name.clone());
        }
        let backend = DesktopBackend::spawn(spec)
            .map_err(|e| format!("cannot start the desktop compositor: {e}"))?;
        let (width, height) = backend.size();

        let (pump, sender) = FramePump::spawn(
            backend,
            PumpConfig {
                shared: self.display.shared_arc(),
                updates: self.display.update_sender(),
                events: self.display.event_slot(),
                pacer: plan.pacer,
                tier: self.tier,
                encoder: plan.encoder,
                encoder_factory: plan.encoder_factory,
                degrader: uflinux_rds_server::degrade::Degrader::new(self.tier),
            },
        )
        .map_err(|e| format!("cannot start the frame pump: {e}"))?;

        // Input events that arrived before this point were dropped; from here
        // on the bridge finds the pump through the display handler.
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
    let launcher = Arc::new(DesktopLauncher::new(
        DesktopPlan {
            command,
            base: desktop_base,
            pacer,
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
    let security_is_hybrid = security.is_hybrid();
    let parts = ServerParts {
        security,
        display,
        bridge: InputBridge::new(Arc::clone(&session)),
        codecs: advertised_codecs(config.codec.allow_remotefx, decision.tier),
        quant: remote_fx_quant(config.codec.rfx_quant),
        validator: validator(Arc::clone(&session), config, Arc::clone(&launcher)),
        #[cfg(feature = "egfx")]
        factory: Some(factory),
    };
    let mut server = build_server(parts);
    if security_is_hybrid {
        let credentials = hybrid_credentials(&session.args);
        warn!(
            username = %credentials.username,
            "NLA is required but this session has no secret to answer CredSSP with; \
             NLA-only clients will be refused with a logon failure"
        );
        server.set_credentials(Some(credentials));
    }
    let connection_events = server.event_sender().clone();

    let watchdog = tokio::spawn(idle_watchdog(Arc::clone(&session), connection_events));

    // A web session arrives already decrypted by the gateway's TLS layer, so
    // the server must not try to terminate TLS a second time.
    let tls_mode = if session.args.transport == Transport::Web {
        TransportTls::AlreadyDone
    } else {
        TransportTls::Managed
    };
    let outcome = server.run_connection_with(stream, tls_mode).await;
    watchdog.abort();
    if let Err(error) = &outcome {
        warn!(error = %error, "connection ended with an error");
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

    outcome.map_err(|e| format!("session ended: {e}"))
}

/// Which security layer the server terminates itself.
#[derive(Clone)]
enum Security {
    /// Server-authenticated TLS.
    Tls(tokio_rustls::TlsAcceptor),
    /// NLA: CredSSP on top of TLS, which needs the certificate's public key.
    Hybrid(tokio_rustls::TlsAcceptor, Vec<u8>),
    /// Nothing: the gateway already terminated TLS and handed the connection
    /// over a private unix socket. X.224 then confirms with an empty security
    /// protocol, which is the only reading that matches what the browser's
    /// wasm client does — it never starts a TLS handshake inside the WebSocket.
    Web,
}

impl Security {
    /// `true` when the session negotiated CredSSP, i.e. when the server needs
    /// an identity of its own to answer the client's NTLM challenge.
    fn is_hybrid(&self) -> bool {
        matches!(self, Self::Hybrid(..))
    }
}

/// The security layer for one connection.
///
/// The default is server-authenticated TLS, not NLA. IronRDP's CredSSP server
/// verifies the client's NTLM Type-3 against a password held by *this* process,
/// and a session host has none: it learns who the user is only from the
/// credentials in the connection, which is exactly what PAM is there to check.
/// Handing CredSSP a placeholder password therefore fails every NLA client with
/// `LOGON_FAILURE`, so NLA stays off until there is a secret store to read a
/// real one from. TLS still encrypts the session and still authenticates the
/// server; the user simply authenticates in the Client Info PDU, which is where
/// the web client already sends the login form.
///
/// A client that merely *prefers* NLA (the mstsc default) accepts the TLS
/// fallback. A client configured to *require* NLA is refused by the negotiation,
/// which the broker then reports as the session's failure reason.
///
/// A web connection is the exception and does not need an identity at all: the
/// gateway terminated TLS in the browser's own TLS session, and the socket the
/// session host was handed is a unix socket on a machine the client cannot
/// reach. Generating a certificate for that would be work for a handshake
/// nobody performs.
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
    if config.nla_required {
        Ok(Security::Hybrid(acceptor, identity.spki().to_owned()))
    } else {
        Ok(Security::Tls(acceptor))
    }
}

/// The identity CredSSP answers with when NLA is required.
///
/// There is no secret to put in it, so the password is empty and the username
/// is the session's fallback user. NTLM then rejects the client's Type-3 with a
/// clean `LOGON_FAILURE` rather than the connection dying on
/// "no credentials while doing credssp"; the warning below names the reason.
fn hybrid_credentials(args: &Args) -> Credentials {
    Credentials {
        username: args.session_user().unwrap_or_default().to_owned(),
        password: String::new(),
        domain: None,
    }
}

/// The pieces `build_server` needs, taken by value because the builder states
/// are unnameable and the chain has to be one expression.
struct ServerParts {
    security: Security,
    display: SessionDisplay,
    bridge: InputBridge,
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
        codecs,
        quant,
        validator,
        #[cfg(feature = "egfx")]
        factory,
    } = parts;

    let started = RdpServer::builder().with_addr(addr);
    let secured = match security {
        Security::Tls(acceptor) => started.with_tls(acceptor),
        Security::Hybrid(acceptor, public_key) => started.with_hybrid(acceptor, public_key),
        Security::Web => started.with_no_security(),
    };
    let displayed = secured
        .with_input_handler(bridge)
        .with_display_handler(display);
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
fn planned_negotiation(config: &Config) -> Negotiation {
    let egfx = if cfg!(feature = "egfx") && config.codec.h264 != H264Policy::Off {
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
    authenticator: Arc<dyn Authenticator>,
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
        let authenticator = Arc::clone(&self.authenticator);
        let policy = self.policy.clone();
        let username = credentials.username.clone();
        let password = credentials.password.clone();
        // PAM talks to the system over a blocking conversation, so it must not
        // run on a runtime worker thread.
        let outcome = tokio::task::spawn_blocking(move || {
            authenticator
                .authenticate(&username, &password)
                .map(|()| auth::admit(&policy, &username))
        })
        .await;
        match outcome {
            Ok(Ok(true)) => {
                // Forking, execing and waiting for the compositor's socket all
                // block, so this runs on the same blocking thread as PAM.
                let desktop = Arc::clone(&self.desktop);
                let username = credentials.username.clone();
                let started = tokio::task::spawn_blocking(move || {
                    desktop
                        .start(Some(&username))
                        .map(|started| (started, username))
                })
                .await
                .map_err(|join| CredentialValidationError::new(join))?;
                match started {
                    Ok((started, username)) => {
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
                    // There is no "the desktop could not start" reason in the
                    // protocol at this point, so a refused login is the honest
                    // answer the client can act on.
                    Err(reason) => {
                        warn!(reason, "desktop could not start; refusing the login");
                        Ok(CredentialDecision::Reject)
                    }
                }
            }
            Ok(Ok(false)) => Ok(CredentialDecision::Reject),
            Ok(Err(AuthError::Denied)) | Ok(Err(AuthError::NotAllowed)) => {
                Ok(CredentialDecision::Reject)
            }
            Ok(Err(other)) => Err(CredentialValidationError::new(other)),
            Err(join) => Err(CredentialValidationError::new(join)),
        }
    }
}

fn validator(
    session: Arc<Session>,
    config: &Config,
    desktop: Arc<DesktopLauncher>,
) -> Arc<dyn CredentialValidator> {
    let authenticator: Arc<dyn Authenticator> = if cfg!(feature = "pam") {
        Arc::new(auth::PamAuthenticator::new(PAM_SERVICE))
    } else {
        // Without PAM there is no password database to consult, so the only
        // honest check is "the username the broker asked for".
        Arc::new(StubAuthenticator::new(
            session.args.session_user().map(str::to_owned),
        ))
    };
    Arc::new(Admit {
        authenticator,
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
        if let Some(pump) = self.session.display.pump() {
            let _ = pump.send(PumpCommand::Pointer(event));
        }
    }

    fn key(&self, code: u8, extended: bool, pressed: bool) {
        self.session.mark_input();
        // An unmappable keysym is rejected by the backend and kills the
        // session, so drop it here instead.
        if let Some(keysym) = keysym_for(code, extended)
            && let Some(pump) = self.session.display.pump()
        {
            let _ = pump.send(PumpCommand::Keyboard(WlKeyboardEvent { keysym, pressed }));
        }
    }

    fn unicode(&self, value: u16, pressed: bool) {
        self.session.mark_input();
        // Latin-1 code points are their own X keysyms; anything else would need
        // a Compose table the session does not have.
        if value < 0x100
            && let Some(pump) = self.session.display.pump()
        {
            let _ = pump.send(PumpCommand::Keyboard(WlKeyboardEvent {
                keysym: u32::from(value),
                pressed,
            }));
        }
    }
}

impl RdpServerInputHandler for InputBridge {
    fn keyboard(&mut self, event: KeyboardEvent) {
        match event {
            KeyboardEvent::Pressed { code, extended } => self.key(code, extended, true),
            KeyboardEvent::Released { code, extended } => self.key(code, extended, false),
            KeyboardEvent::UnicodePressed(value) => self.unicode(value, true),
            KeyboardEvent::UnicodeReleased(value) => self.unicode(value, false),
            KeyboardEvent::Synchronize(_) => {}
        }
    }

    fn mouse(&mut self, event: MouseEvent) {
        match event {
            MouseEvent::Move { x, y } => self.send(WlPointerEvent::Move { x, y }),
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
                self.send(WlPointerEvent::Move {
                    x: clamp_u16(x),
                    y: clamp_u16(y),
                });
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
        // X1/X2 are the Wayland codes 8 and 9 (BTN_SIDE / BTN_EXTRA).
        MouseButton::X1 => Some(8),
        MouseButton::X2 => Some(9),
        _ => None,
    }
}

fn clamp_u16(value: i32) -> u16 {
    u16::try_from(value.max(0)).unwrap_or(u16::MAX)
}

/// Reports idleness and enforces `--idle-timeout`.
async fn idle_watchdog(session: Arc<Session>, events: mpsc::UnboundedSender<ServerEvent>) {
    let Some(limit) = session.idle_timeout else {
        return;
    };
    let notice = limit.min(Duration::from_secs(60));
    let mut ticker = tokio::time::interval(WATCHDOG_PERIOD);
    let mut reported_idle = false;
    let mut reported_tier = session.display.tier();
    loop {
        ticker.tick().await;
        let idle_for = session.idle_for();
        if idle_for >= limit {
            info!(?idle_for, "idle timeout; ending the session");
            let _ = events.send(ServerEvent::Quit("idle timeout".to_owned()));
            // No pump when the client never authenticated: there is no desktop
            // to stop, and the teardown below has nothing to join.
            if let Some(pump) = session.display.pump() {
                let _ = pump.send(PumpCommand::Stop);
            }
            return;
        }
        session.refresh_codec(&mut reported_tier);
        let idle = idle_for >= notice;
        if idle != reported_idle {
            reported_idle = idle;
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
/// therefore uses the shared state directory, where every desktop additionally
/// gets a `sessions/<uid>` level.
fn desktop_runtime_base(env: &[(String, String)]) -> DesktopBase {
    // Safety: `geteuid` has no preconditions.
    desktop_base_for(unsafe { libc::geteuid() } == 0, env)
}

/// [`desktop_runtime_base`] with the effective uid decided by the caller, so the
/// choice can be tested without running the test as root.
fn desktop_base_for(privileged: bool, env: &[(String, String)]) -> DesktopBase {
    let root = if privileged {
        PathBuf::from(STATE_DIR)
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
        assert_eq!(base.root, PathBuf::from(STATE_DIR));
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
            .start(None)
            .expect_err("a compositor that exits immediately must fail");
        assert!(error.contains("compositor"), "{error}");
        assert!(!launcher.pending());
        assert!(launcher.take_pump().is_none());
        assert!(
            launcher
                .start(None)
                .expect_err("a second start must be refused")
                .contains("already started")
        );
        let _ = std::fs::remove_dir_all(&base);
    }
}
