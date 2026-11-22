//! `uflinux-rdsd` — the resident UFLinux remote-desktop broker.
//!
//! The broker is the only always-on process in the RDS feature. It owns the
//! control socket that the Settings tab and `uflinux-rdsctl` talk to, the two
//! listeners that accept desktop connections — TCP for native RDP clients and a
//! private unix socket for the web gateway — the roster of live sessions, and
//! the `uflinux-rds-web` child process. It contains no RDP protocol code: each
//! connection is handed to a freshly forked session binary (see [`session`]),
//! which is told which listener the connection arrived on.
//!
//! Two properties matter more than anything else here. When the service is
//! *disabled* the broker binds nothing at all — one unix socket and an idle
//! epoll loop — so shipping it enabled-by-default costs a machine almost
//! nothing. And when it *is* serving, it must never block on a slow client or a
//! wedged child, because the Settings daemon gives up after 300 ms and treats a
//! missing answer as a broken service.

mod cli;
mod control;
mod error;
mod session;
mod state;
mod web;

use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;
use std::time::Duration;

use tokio::net::{TcpListener, UnixListener};
use tokio::signal::unix::{SignalKind, signal};
use tracing::{info, warn};

use uflinux_rds_common::Config;

use crate::cli::{Args, Env};
use crate::error::{Error, Result};
use crate::session::Transport;
use crate::state::State;

/// How long sessions and the web child may take their leave on shutdown.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);
/// How often the idle-quit and listener loops re-check their goals.
const TICK: Duration = Duration::from_secs(1);

fn main() {
    let argv = std::env::args().skip(1);
    let env = Env::from_env();
    let args = match Args::parse(argv, &env) {
        Ok(args) => args,
        Err(error) => {
            // `--help` arrives here too, and is not a failure.
            if error.contains("USAGE") {
                println!("{error}");
                return;
            }
            eprintln!("uflinux-rdsd: {error}\n\n{}", cli::HELP);
            std::process::exit(2);
        }
    };

    install_tracing();

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("uflinux-rdsd: cannot start the async runtime: {error}");
            std::process::exit(1);
        }
    };

    match runtime.block_on(run(args, env)) {
        Ok(()) => {}
        Err(error) => {
            eprintln!("uflinux-rdsd: {error}");
            std::process::exit(1);
        }
    }
}

/// Install logging exactly like the gateway does, so `RUST_LOG` behaves the
/// same way in both binaries.
fn install_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .try_init();
}

/// Load the configuration the broker will run with.
///
/// Order is file → `UFLINUX_RDS_*` → command line, which is the same precedence
/// the gateway uses, so `/etc/conf.d/uflinux-rds` cannot be overridden by
/// accident. Validation happens here rather than inside `Config::load` so a bad
/// file produces a startup message instead of a control-socket error.
fn resolve(args: &Args, env: &Env) -> Result<Config> {
    let mut config = Config::load(&args.config_path)?;
    config.apply_env_overrides(env.overrides());
    if let Some(bind) = &args.rdp_bind {
        config.rdp_bind = bind.clone();
    }
    if let Some(port) = args.rdp_port {
        config.rdp_port = port;
    }
    config.validate()?;
    Ok(config)
}

/// Start everything and run until a signal, the idle timer, or a fatal error.
async fn run(args: Args, env: Env) -> Result<()> {
    let config = resolve(&args, &env)?;
    let state = Arc::new(State::new(config, &args.config_path));
    let args = Arc::new(args);

    let server = control::bind(&args.socket_path, args.take_over).await?;
    info!(
        socket = %server.path().display(),
        config = %args.config_path.display(),
        enabled = state.config().enabled,
        "uflinux-rdsd started"
    );
    if !session::is_executable(&args.session_binary) {
        // Not fatal: the service may still be disabled, and an administrator
        // may install the session binary later.
        warn!(
            "session binary {} is missing or not executable",
            args.session_binary.display()
        );
    }

    let mut tasks = vec![
        tokio::spawn(server.serve(Arc::clone(&state), Arc::clone(&args))),
        tokio::spawn(web::supervise(Arc::clone(&state), Arc::clone(&args))),
        tokio::spawn(listeners(Arc::clone(&state), Arc::clone(&args))),
        tokio::spawn(reload(Arc::clone(&state), Arc::clone(&args))),
        tokio::spawn(shutdown_watcher(Arc::clone(&state))),
    ];
    if !args.idle_quit.is_zero() {
        tasks.push(tokio::spawn(idle_quit(
            Arc::clone(&state),
            Arc::clone(&args),
        )));
    }

    state.wait_shutdown().await;
    info!("shutting down");
    state.begin_shutdown();

    // Stop the children first: they get the whole grace period, and the tasks
    // that own them then notice the exit on their own.
    session::terminate_all(&state, SHUTDOWN_GRACE).await;
    for task in tasks {
        if let Err(error) = await_task(task).await {
            warn!("a broker task did not stop cleanly: {error}");
        }
    }
    control::remove_socket(&args.socket_path);
    Ok(())
}

/// Await a spawned task, turning a panic into a plain message.
async fn await_task(task: tokio::task::JoinHandle<()>) -> std::result::Result<(), String> {
    match task.await {
        Ok(()) => Ok(()),
        Err(error) => Err(if error.is_panic() {
            "task panicked".to_owned()
        } else {
            "task cancelled".to_owned()
        }),
    }
}

/// Bind and serve the RDP listener, following the configuration.
///
/// The listener lives in this one task so that "enabled and started" is the
/// single condition that decides whether a port is open. When the answer is no,
/// the socket is dropped here and the port is genuinely closed — that is what
/// makes the disabled state cost nothing.
///
/// The web backend socket is bound and closed by the same task for the same
/// reason: it is the same listener seen from a different front end, and a
/// disabled service must not leave either endpoint behind.
async fn listeners(state: Arc<State>, args: Arc<Args>) {
    let mut wake = state.wake();
    let mut bound: Option<(TcpListener, String, u16)> = None;
    let mut web: Option<WebListener> = None;
    let mut tick = tokio::time::interval(TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        let config = state.config();
        let want = config.enabled && state.desired_running();
        let address = format!("{}:{}", config.rdp_bind, config.rdp_port);
        let same = bound
            .as_ref()
            .is_some_and(|(_, host, port)| *host == config.rdp_bind && *port == config.rdp_port);

        if want && !same {
            if let Some((listener, _, _)) = bound.take() {
                drop(listener);
                state.set_rdp_listening(false);
            }
            match TcpListener::bind(&address).await {
                Ok(listener) => {
                    let local = listener
                        .local_addr()
                        .map(|addr| addr.to_string())
                        .unwrap_or_else(|_| address.clone());
                    info!("RDP listener bound on {local}");
                    state.set_rdp_listening(true);
                    state.set_fault(None);
                    bound = Some((listener, config.rdp_bind.clone(), config.rdp_port));
                }
                Err(error) => {
                    warn!("cannot bind {address}: {error}");
                    state.set_rdp_listening(false);
                    state.set_fault(Some(format!("cannot bind {address}: {error}")));
                }
            }
        } else if !want && bound.is_some() {
            info!("RDP listener closed");
            bound = None;
            state.set_rdp_listening(false);
        }

        if want && web.is_none() {
            match bind_web_listener() {
                Ok(listener) => {
                    info!(
                        socket = %listener.path.display(),
                        "web backend listener bound"
                    );
                    web = Some(listener);
                }
                Err(error) => {
                    // Not fatal: native RDP clients still work, and the next
                    // tick retries. The gateway reports a refused connection as
                    // its own error.
                    warn!("cannot bind the web backend socket: {error}");
                    state.set_fault(Some(error.reply_message()));
                }
            }
        } else if !want && web.is_some() {
            info!("web backend listener closed");
            web = None;
        }

        tokio::select! {
            () = accept_into(&state, &args, &mut bound) => {}
            () = accept_web_into(&state, &args, &mut web) => {}
            _ = wake.changed() => {}
            _ = tick.tick() => {}
            () = state.wait_shutdown() => break,
        }
    }

    state.set_rdp_listening(false);
}

/// A bound web backend socket, unlinked when dropped.
struct WebListener {
    listener: UnixListener,
    path: std::path::PathBuf,
}

impl Drop for WebListener {
    fn drop(&mut self) {
        session::remove_socket(&self.path);
    }
}

/// Bind the unix socket the web gateway relays browser connections into.
///
/// A stale file from a killed broker is replaced — nothing else can be holding
/// the socket, because the broker is the only process that binds it and it is
/// single-instance behind the control socket. Mode 0600: relaying into a user's
/// desktop is not something an unrelated local process should get to do.
fn bind_web_listener() -> Result<WebListener> {
    let path = uflinux_rds_common::web_socket_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| Error::io("create socket directory", &error))?;
    }
    if path.symlink_metadata().is_ok() {
        std::fs::remove_file(&path).map_err(|error| Error::io("remove stale socket", &error))?;
    }
    let listener =
        UnixListener::bind(&path).map_err(|error| Error::io("bind web backend socket", &error))?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).map_err(|error| {
        // The socket is already bound; leaving it world-writable is worse than
        // not having it, so unlink before reporting.
        let _ = std::fs::remove_file(&path);
        Error::io("protect web backend socket", &error)
    })?;
    Ok(WebListener { listener, path })
}

/// Accept one RDP connection when a listener is bound, otherwise wait forever.
async fn accept_into(
    state: &Arc<State>,
    args: &Arc<Args>,
    bound: &mut Option<(TcpListener, String, u16)>,
) {
    let Some((listener, _, _)) = bound else {
        std::future::pending::<()>().await;
        return;
    };
    match listener.accept().await {
        Ok((stream, peer)) => accept(state, args, stream, peer.to_string(), Transport::Rdp),
        Err(error) => warn!("RDP accept failed: {error}"),
    }
}

/// Accept one relayed browser connection, same rule as [`accept_into`].
async fn accept_web_into(state: &Arc<State>, args: &Arc<Args>, web: &mut Option<WebListener>) {
    let Some(web) = web else {
        std::future::pending::<()>().await;
        return;
    };
    match web.listener.accept().await {
        Ok((stream, _)) => {
            // A unix socket has no address worth showing; the gateway does not
            // forward the browser's origin, so the row says where it came from.
            accept(
                state,
                args,
                stream,
                "web gateway".to_owned(),
                Transport::Web,
            );
        }
        Err(error) => warn!("web backend accept failed: {error}"),
    }
}

/// One accepted connection: gate it, then hand it to a session binary.
///
/// `stream` is either kind of listener's connection; only the descriptor
/// matters, and the child learns the family from the socket itself.
fn accept<S>(state: &Arc<State>, args: &Arc<Args>, stream: S, peer: String, transport: Transport)
where
    S: session::ClientSocket,
{
    let config = state.config();
    if state.session_count() >= config.max_sessions {
        // Dropping the socket without reading is honest: the client sees the
        // connection close, which is all "no more sessions" can look like over
        // TCP before any protocol has been spoken.
        warn!(
            "refusing {peer}: the {} session limit is reached",
            config.max_sessions
        );
        return;
    }
    if !session::is_executable(&args.session_binary) {
        warn!(
            "refusing {peer}: session binary {} is missing or not executable",
            args.session_binary.display()
        );
        state.set_fault(Some(format!(
            "session binary {} is missing",
            args.session_binary.display()
        )));
        return;
    }

    let config_path = state.config_path();
    match session::spawn(
        &args.session_binary,
        &config_path,
        &config.session_command,
        stream,
        transport,
    ) {
        Ok(spawned) => {
            let pid = session::pid_of(&spawned.child);
            let id = state.add_session(pid);
            info!("session {id} for {peer} started as pid {pid}");
            tokio::spawn(session::watch(spawned, Arc::clone(state), id));
        }
        Err(error) => {
            warn!("refusing {peer}: {error}");
            state.set_fault(Some(error.reply_message()));
        }
    }
}

/// Reload the configuration on `SIGHUP`, the signal the OpenRC `reload` verb
/// sends. Termination signals belong to [`shutdown_watcher`].
async fn reload(state: Arc<State>, args: Arc<Args>) {
    let Ok(mut sighup) = signal(SignalKind::hangup()) else {
        warn!("cannot listen for SIGHUP; configuration reloads are disabled");
        return;
    };
    loop {
        tokio::select! {
            received = sighup.recv() => {
                if received.is_none() {
                    return;
                }
            }
            () = state.wait_shutdown() => return,
        }
        match Config::load(&args.config_path) {
            Ok(mut config) => {
                config.apply_env_overrides(Env::from_env().overrides());
                if let Some(bind) = &args.rdp_bind {
                    config.rdp_bind = bind.clone();
                }
                if let Some(port) = args.rdp_port {
                    config.rdp_port = port;
                }
                if let Err(error) = config.validate() {
                    warn!("ignoring {}: {error}", args.config_path.display());
                    state.set_fault(Some(format!("reloaded configuration is invalid: {error}")));
                    continue;
                }
                let enabled = config.enabled;
                state.replace_config(config);
                state.notify_change();
                info!("configuration reloaded from {}", args.config_path.display());
                if !enabled {
                    info!("RDS is disabled; no listener is bound");
                }
            }
            Err(error) => warn!("cannot reload {}: {error}", args.config_path.display()),
        }
    }
}

/// Turn `SIGTERM`/`SIGINT` into a shutdown request.
async fn shutdown_watcher(state: Arc<State>) {
    let Ok(mut terminate) = signal(SignalKind::terminate()) else {
        warn!("cannot listen for SIGTERM");
        return;
    };
    let Ok(mut interrupt) = signal(SignalKind::interrupt()) else {
        warn!("cannot listen for SIGINT");
        return;
    };
    tokio::select! {
        _ = terminate.recv() => info!("SIGTERM received"),
        _ = interrupt.recv() => info!("SIGINT received"),
        () = state.wait_shutdown() => return,
    }
    state.begin_shutdown();
}

/// Quit when the broker has had nothing to do for `--idle-quit` seconds.
///
/// This is what lets the service be started on demand: the unit is launched by
/// the first connection or settings query and goes away again once the last
/// user leaves. `--resident` (or `--idle-quit 0`) turns the task off entirely,
/// which is what a plain OpenRC start wants unless socket activation is in use.
async fn idle_quit(state: Arc<State>, args: Arc<Args>) {
    let mut tick = tokio::time::interval(TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = tick.tick() => {}
            () = state.wait_shutdown() => return,
        }
        let snapshot = state.snapshot();
        let serving = state::is_serving(
            snapshot.enabled,
            snapshot.rdp_listening,
            snapshot.web_listening,
            socket_activated(),
        );
        if state::should_idle_quit(
            args.idle_quit.is_zero(),
            serving,
            state.session_count(),
            state.control_clients(),
            state.idle_for(),
            args.idle_quit,
        ) {
            info!(
                "idle for {:?}; exiting so the service can be started on demand",
                state.idle_for()
            );
            state.begin_shutdown();
            return;
        }
    }
}

/// Whether systemd handed us a listening socket (`LISTEN_FDS`/`LISTEN_PID`).
///
/// Only used to decide whether quitting while a port is open is safe: under
/// socket activation the socket belongs to the unit and the next connection
/// starts a new broker; under OpenRC nothing would bring us back.
fn socket_activated() -> bool {
    let fds: u64 = std::env::var("LISTEN_FDS")
        .ok()
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(0);
    let pid: u32 = std::env::var("LISTEN_PID")
        .ok()
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(0);
    fds > 0 && pid == std::process::id()
}
