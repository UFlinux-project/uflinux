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
    runtime.block_on(serve(config, state_dir))
}

fn install_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .try_init();
}

async fn serve(config: Config, state_dir: std::path::PathBuf) -> Result<()> {
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
    let app = http::router(state).into_make_service_with_connect_info::<std::net::SocketAddr>();
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .map_err(|e| Error::other(format!("server error: {e}")))
}

/// PAM service name: `UFLINUX_RDS_PAM_SERVICE`, else the built-in default.
fn pam_service() -> String {
    std::env::var("UFLINUX_RDS_PAM_SERVICE").unwrap_or_else(|_| DEFAULT_PAM_SERVICE.to_string())
}

/// Resolve on SIGINT or SIGTERM.
async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};

    let Ok(mut interrupt) = signal(SignalKind::interrupt()) else {
        tracing::warn!("cannot listen for SIGINT");
        return;
    };
    let Ok(mut terminate) = signal(SignalKind::terminate()) else {
        tracing::warn!("cannot listen for SIGTERM");
        return;
    };
    tokio::select! {
        _ = interrupt.recv() => tracing::info!("SIGINT received, shutting down"),
        _ = terminate.recv() => tracing::info!("SIGTERM received, shutting down"),
    }
}
