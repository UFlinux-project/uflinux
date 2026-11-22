//! `uflinux-rds-web` — the UFLinux RDS web gateway.

use std::sync::Arc;
use std::time::Duration;

use axum::serve::ListenerExt as _;

use uflinux_rds_common::{CertificateSource, Config};
use uflinux_rds_gateway::auth::{self, Authenticator, PamAuthenticator};
use uflinux_rds_gateway::config::{self, Args, BTree};
use uflinux_rds_gateway::http;
use uflinux_rds_gateway::state::AppState;
use uflinux_rds_gateway::tls;
use uflinux_rds_gateway::{Error, Result};

/// PAM service name when the environment does not say otherwise.
const DEFAULT_PAM_SERVICE: &str = "uflinux-rds";
/// How often expired tokens are dropped in the background.
const REAP_INTERVAL: Duration = Duration::from_secs(60);

fn main() {
    let argv = std::env::args().skip(1);
    let mut args = match Args::parse(argv) {
        Ok(args) => args,
        Err(error) => {
            // `--help` is reported the same way, but is not an error.
            if error.contains("USAGE") {
                println!("{error}");
                return;
            }
            eprintln!("uflinux-rds-web: {error}\n\n{}", Args::HELP);
            std::process::exit(2);
        }
    };

    let env = BTree::from_env();
    args.config = Args::config_path(&env, &args);
    match run(&args, &env) {
        Ok(()) => {}
        Err(error) => {
            eprintln!("uflinux-rds-web: {error}");
            std::process::exit(1);
        }
    }
}

/// Parse, load, bind and serve. Returns without serving when the gateway (or
/// RDP as a whole) is disabled in the configuration.
fn run(args: &Args, env: &BTree) -> Result<()> {
    let resolved = config::resolve(args, env)?;
    let config = resolved.config;
    let state_dir = resolved.state_dir;

    install_tracing();

    if !config.enabled {
        eprintln!("uflinux-rds-web: RDP is disabled, nothing to do.");
        return Ok(());
    }
    if !config.web.enabled {
        eprintln!("uflinux-rds-web: web gateway is disabled (web.enabled = false).");
        return Ok(());
    }

    // Must happen before any rustls configuration is built. `ring` rather than
    // `aws-lc-rs` so the binary also works on musl.
    tls::install_crypto_provider();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| Error::other(format!("cannot start tokio: {e}")))?;
    runtime.block_on(serve(args, env, config, state_dir))
}

fn install_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .try_init();
}

async fn serve(
    args: &Args,
    env: &BTree,
    config: Config,
    state_dir: std::path::PathBuf,
) -> Result<()> {
    let authenticator: Arc<dyn Authenticator> = Arc::new(PamAuthenticator::new(pam_service()));
    let control_socket = uflinux_rds_common::control_socket_path();
    let state = AppState::new(
        config.clone(),
        state_dir.clone(),
        control_socket.clone(),
        authenticator,
    );

    // Expired tokens are refused on use as well; the reaper only bounds the
    // size of the store.
    tokio::spawn(auth::reaper(state.inner.tokens.clone(), REAP_INTERVAL));

    let server = tls::Server::bind(&config, &state_dir)?;
    let addr = server
        .local_addr()
        .map_err(|e| Error::other(format!("local_addr: {e}")))?;
    let scheme = if config.web.https { "https" } else { "http" };
    let certificate = match &config.web.certificate {
        CertificateSource::SelfSigned if config.web.https => {
            let (cert, _) = config::self_signed_paths(&state_dir);
            format!(" (certificate {})", cert.display())
        }
        CertificateSource::Custom { certificate, .. } => {
            format!(" (certificate {})", certificate.display())
        }
        _ => String::new(),
    };
    tracing::info!(
        "listening on {scheme}://{addr}, RDP target {}, control socket {}{certificate}",
        config::rdp_target(&config),
        control_socket.display()
    );

    // `tap_io` gives axum the connect-info plumbing for a custom listener.
    let listener = server.into_listener().tap_io(|_| ());
    let app =
        http::router(state.clone()).into_make_service_with_connect_info::<std::net::SocketAddr>();
    // The shutdown future must be `'static`, so the signal handler gets its own
    // copy of what a reload needs rather than a borrow of `run`'s arguments.
    let reload_hint = ReloadHint {
        args: args.clone(),
        env: env.clone(),
    };
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal(reload_hint, state))
        .await
        .map_err(|e| Error::other(format!("server error: {e}")))
}

/// PAM service name: `UFLINUX_RDS_PAM_SERVICE`, else the built-in default.
fn pam_service() -> String {
    std::env::var("UFLINUX_RDS_PAM_SERVICE").unwrap_or_else(|_| DEFAULT_PAM_SERVICE.to_string())
}

/// Resolve on SIGINT or SIGTERM; reload the configuration on SIGHUP.
///
/// A reload re-reads the same file the gateway started from (same path
/// resolution, same environment overrides, same argument overrides) and swaps
/// the state's configuration view. The environment is the snapshot taken at
/// startup: a changed `Environment=` needs a restart, not a signal. The state
/// directory is re-derived but ignored — the self-signed certificate already
/// written there belongs to the running listener. Disabling the gateway in the
/// file is reported and takes effect on restart; a bad file is logged and the
/// previous configuration stays in force.
async fn shutdown_signal(hint: ReloadHint, state: AppState) {
    use tokio::signal::unix::{SignalKind, signal};

    let Ok(mut interrupt) = signal(SignalKind::interrupt()) else {
        tracing::warn!("cannot listen for SIGINT");
        return;
    };
    let Ok(mut terminate) = signal(SignalKind::terminate()) else {
        tracing::warn!("cannot listen for SIGTERM");
        return;
    };
    let Ok(mut hangup) = signal(SignalKind::hangup()) else {
        tracing::warn!("cannot listen for SIGHUP, configuration reload is unavailable");
        tokio::select! {
            _ = interrupt.recv() => tracing::info!("SIGINT received, shutting down"),
            _ = terminate.recv() => tracing::info!("SIGTERM received, shutting down"),
        }
        return;
    };

    loop {
        tokio::select! {
            _ = interrupt.recv() => {
                tracing::info!("SIGINT received, shutting down");
                return;
            }
            _ = terminate.recv() => {
                tracing::info!("SIGTERM received, shutting down");
                return;
            }
            _ = hangup.recv() => reload(&hint, &state),
        }
    }
}

/// What a SIGHUP reload needs: the parsed command line and the environment
/// snapshot taken at startup.
struct ReloadHint {
    /// The gateway's own arguments (`--config`, `--port`, `--bind`).
    args: Args,
    /// The environment the gateway started with.
    env: BTree,
}

/// One SIGHUP: re-read the configuration and swap the view new logins use.
fn reload(hint: &ReloadHint, state: &AppState) {
    let path = Args::config_path(&hint.env, &hint.args);
    match config::resolve(&hint.args, &hint.env) {
        Ok(resolved) => {
            if !resolved.config.enabled || !resolved.config.web.enabled {
                tracing::warn!(
                    "{} disables the web gateway; the running server keeps serving \
                     until it is restarted",
                    path.display()
                );
            }
            state.reload(resolved.config);
        }
        Err(error) => {
            tracing::error!("SIGHUP: keeping the running configuration: {error}");
        }
    }
}
