//! Supervision of the `uflinux-rds-web` gateway child.
//!
//! The gateway is a separate binary (`rds/gateway`) that reads the same config
//! file and serves HTTPS/WebSocket itself. The broker only keeps it alive: it
//! starts it when the configuration wants web access, restarts it when it dies
//! unexpectedly, and stops it when the configuration, the service state, or the
//! broker itself says otherwise.
//!
//! Two details shape the loop. The gateway *exits by itself* when it loads a
//! configuration that disables web access, which is a clean exit and must not
//! be treated as a crash. And "is the web reachable" is a question only the
//! port can answer, so liveness is a TCP connect probe rather than a waitpid.

use std::net::SocketAddr;
use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use tokio::net::TcpStream;
use tokio::process::Command;
use tokio::time::timeout;
use tracing::{debug, info, warn};

use uflinux_rds_common::{Config, WebConfig};

use crate::cli::Args;
use crate::state::State;

/// First restart delay after an unexpected exit; doubled on every failure.
const BACKOFF_START: Duration = Duration::from_secs(1);
/// Ceiling for the restart backoff, per the approved plan.
const BACKOFF_CAP: Duration = Duration::from_secs(60);
/// A child that stayed up this long is considered healthy again.
const HEALTHY_UPTIME: Duration = Duration::from_secs(60);
/// Grace given to the child between SIGTERM and SIGKILL.
const KILL_GRACE: Duration = Duration::from_secs(5);
/// Budget for one liveness probe.
const PROBE_TIMEOUT: Duration = Duration::from_millis(300);
/// How often liveness is re-checked while the child is alive.
const PROBE_EVERY: Duration = Duration::from_secs(2);

/// What the supervisor should be doing right now, derived from the config.
///
/// `PartialEq` is what lets a configuration reload be a no-op: applying the
/// same web settings again does not bounce a live browser session.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Goal {
    /// Web access is wanted: the service is enabled, started, and `web.enabled`.
    wanted: bool,
    /// The settings the child was started with, used to spot a real change.
    web: WebConfig,
    /// Certificate material lives outside `web`, but changing it matters.
    certificate: String,
}

/// Derive the supervisor's goal from a configuration snapshot.
fn goal(config: &Config, running: bool) -> Goal {
    Goal {
        wanted: config.enabled && running && config.web.enabled,
        web: config.web.clone(),
        certificate: format!(
            "{:?}{}",
            config.web.certificate,
            config
                .codec
                .vaapi_device
                .as_ref()
                .map(|device| device.to_string_lossy().into_owned())
                .unwrap_or_default()
        ),
    }
}

/// The restart delay for the `failures`-th consecutive crash.
///
/// Kept as a pure function so the growth and the cap are both testable.
fn restart_delay(failures: u32) -> Duration {
    let factor = failures.saturating_sub(1).min(16);
    let scaled = BACKOFF_START.saturating_mul(1u32 << factor);
    if scaled > BACKOFF_CAP {
        BACKOFF_CAP
    } else {
        scaled
    }
}

/// The address a liveness probe should reach.
///
/// A child bound to a wildcard address is probed on loopback, since
/// `connect(0.0.0.0)` is not a meaningful question.
fn probe_address(bind: &str, port: u16) -> Option<SocketAddr> {
    let host = match bind {
        "0.0.0.0" | "::" | "*" | "" => "127.0.0.1",
        other => other,
    };
    match host.parse::<std::net::IpAddr>() {
        Ok(addr) => Some(SocketAddr::new(addr, port)),
        Err(_) => format!("{host}:{port}")
            .parse()
            .ok()
            .or_else(|| Some(SocketAddr::from(([127, 0, 0, 1], port)))),
    }
}

/// Run the supervisor until the broker shuts down.
///
/// `args` supplies the binary path; the child always gets `--config` pointing
/// at the same file the broker was started with, plus the broker's own
/// environment, so `/etc/conf.d/uflinux-rds` works for both processes.
pub(crate) async fn supervise(state: std::sync::Arc<State>, args: std::sync::Arc<Args>) {
    let mut wake = state.wake();
    let mut child: Option<(tokio::process::Child, Instant)> = None;
    let mut started: Option<Goal> = None;
    let mut failures = 0u32;
    let mut probe = tokio::time::interval(PROBE_EVERY);
    probe.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        let config = state.config();
        let goal = goal(&config, state.desired_running());

        if started.as_ref() != Some(&goal) {
            if let Some((alive, _)) = child.take() {
                info!("restarting the web gateway for a changed configuration");
                stop(&state, alive).await;
            }
            started = Some(goal.clone());
            if goal.wanted {
                match spawn(&args, &state.config_path()).await {
                    Ok(alive) => {
                        child = Some((alive, Instant::now()));
                        state.set_web_running(true);
                        state.set_fault(None);
                    }
                    Err(message) => {
                        // Leave `started` behind so the retry path below runs.
                        started = None;
                        state.set_web_running(false);
                        state.set_web_listening(false);
                        state.set_fault(Some(message));
                        failures += 1;
                    }
                }
            } else {
                state.set_web_running(false);
                state.set_web_listening(false);
                failures = 0;
            }
        }

        if child.is_none() && goal.wanted {
            let delay = restart_delay(failures.max(1));
            debug!("web gateway offline; retrying in {delay:?}");
            if wait_or_change(&mut wake, delay, &state).await && state.shutting_down() {
                break;
            }
            continue;
        }

        let Some((alive, born)) = child.as_mut() else {
            // Nothing wanted and nothing running: park until the next change.
            if wait_for_change(&mut wake, &state).await {
                break;
            }
            continue;
        };

        tokio::select! {
            status = alive.wait() => match status {
                Ok(status) => {
                    let up = born.elapsed();
                    state.set_web_running(false);
                    state.set_web_listening(false);
                    child = None;
                    started = None;
                    if state.shutting_down() {
                        break;
                    }
                    if !goal.wanted || status.success() {
                        // The gateway exits on its own when web access is off.
                        info!("web gateway exited cleanly; not restarting it");
                        failures = 0;
                    } else {
                        if up > HEALTHY_UPTIME {
                            failures = 1;
                        } else {
                            failures = failures.saturating_add(1);
                        }
                        state.set_fault(Some(format!(
                            "web gateway exited with {}",
                            status
                                .code()
                                .map(|code| code.to_string())
                                .unwrap_or_else(|| "a signal".to_owned())
                        )));
                        warn!("web gateway failed; restart #{failures} scheduled");
                    }
                }
                Err(error) => {
                    warn!("cannot wait for the web gateway: {error}");
                    state.set_web_running(false);
                    state.set_web_listening(false);
                    child = None;
                    started = None;
                }
            },
            _ = probe.tick() => {
                // No point probing a port whose owner we already know is gone.
                let listening = state.web_running() && probe_reachable(&config.web).await;
                state.set_web_listening(listening);
                if listening {
                    state.set_fault(None);
                }
            },
            _ = wake.changed() => {}
            () = state.wait_shutdown() => {
                if let Some((alive, _)) = child.take() {
                    stop(&state, alive).await;
                }
                break;
            },
        }
    }

    state.set_web_running(false);
    state.set_web_listening(false);
}

/// Start one gateway child.
async fn spawn(args: &Args, config_path: &Path) -> Result<tokio::process::Child, String> {
    let mut command = Command::new(&args.web_binary);
    command
        .arg("--config")
        .arg(config_path)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    match command.spawn() {
        Ok(child) => {
            info!(
                "web gateway {} started for {}",
                child.id().unwrap_or(0),
                config_path.display()
            );
            Ok(child)
        }
        Err(error) => Err(format!(
            "cannot start the web gateway {}: {error}",
            args.web_binary.display()
        )),
    }
}

/// Stop a child politely: SIGTERM, then SIGKILL after [`KILL_GRACE`].
async fn stop(state: &State, mut child: tokio::process::Child) {
    let pid = child.id();
    if let Some(pid) = pid {
        crate::session::signal(pid, nix::sys::signal::Signal::SIGTERM);
    } else {
        let _ = child.start_kill();
    }
    match timeout(KILL_GRACE, child.wait()).await {
        Ok(Ok(_)) => {}
        Ok(Err(error)) => debug!("web gateway wait failed: {error}"),
        Err(_) => {
            warn!("web gateway ignored SIGTERM; killing it");
            let _ = child.start_kill();
            let _ = child.wait().await;
        }
    }
    state.set_web_running(false);
    state.set_web_listening(false);
}

/// Ask the web port whether the gateway is serving.
async fn probe_reachable(web: &WebConfig) -> bool {
    let Some(addr) = probe_address(&web.bind, web.port) else {
        return false;
    };
    matches!(
        timeout(PROBE_TIMEOUT, TcpStream::connect(addr)).await,
        Ok(Ok(_))
    )
}

/// Wait for a wake signal, treating shutdown as "stop waiting".
async fn wait_for_change(wake: &mut tokio::sync::watch::Receiver<u64>, state: &State) -> bool {
    if state.shutting_down() {
        return true;
    }
    match wake.changed().await {
        Ok(()) => state.shutting_down(),
        Err(_) => true,
    }
}

/// Wait for a wake signal or a deadline. Returns true when the caller should
/// re-check shutdown.
async fn wait_or_change(
    wake: &mut tokio::sync::watch::Receiver<u64>,
    delay: Duration,
    state: &State,
) -> bool {
    tokio::select! {
        () = tokio::time::sleep(delay) => false,
        _ = wake.changed() => state.shutting_down(),
        () = state.wait_shutdown() => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_and_caps() {
        assert_eq!(restart_delay(1), Duration::from_secs(1));
        assert_eq!(restart_delay(2), Duration::from_secs(2));
        assert_eq!(restart_delay(4), Duration::from_secs(8));
        assert_eq!(restart_delay(10), BACKOFF_CAP);
        assert_eq!(restart_delay(u32::MAX), BACKOFF_CAP);
    }

    #[test]
    fn a_wildcard_bind_is_probed_on_loopback() {
        assert_eq!(
            probe_address("0.0.0.0", 3390),
            Some("127.0.0.1:3390".parse().unwrap())
        );
        assert_eq!(
            probe_address("::", 3390),
            Some("127.0.0.1:3390".parse().unwrap())
        );
        assert_eq!(
            probe_address("192.168.1.5", 3390),
            Some("192.168.1.5:3390".parse().unwrap())
        );
    }

    #[test]
    fn web_is_only_wanted_when_the_service_is_up() {
        let mut config = Config::default();
        config.web.enabled = true;
        assert!(!goal(&config, true).wanted, "disabled service");
        config.enabled = true;
        assert!(!goal(&config, false).wanted, "stopped service");
        assert!(goal(&config, true).wanted);
        config.web.enabled = false;
        assert!(!goal(&config, true).wanted, "web switched off");
    }

    #[test]
    fn an_unchanged_web_config_is_not_a_change() {
        // `WebConfig::default()` already has `enabled`, so this config wants web.
        let config = Config {
            enabled: true,
            ..Config::default()
        };
        let before = goal(&config, true);
        assert_eq!(before, goal(&config, true));
        let mut moved = config.clone();
        moved.web.port = 3491;
        assert_ne!(before, goal(&moved, true));
    }

    #[tokio::test]
    async fn a_dead_child_leaves_the_web_marked_down() {
        let state = State::new(Config::default(), Path::new("/nonexistent/rds.json"));
        state.set_web_running(true);
        state.set_web_listening(true);
        stop(
            &state,
            tokio::process::Command::new("/bin/true").spawn().unwrap(),
        )
        .await;
        assert!(!state.web_running());
        assert!(!state.snapshot().web_listening);
    }

    #[tokio::test]
    async fn a_live_listener_is_detected_by_the_probe() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let web = WebConfig {
            enabled: true,
            bind: "127.0.0.1".to_owned(),
            port,
            ..WebConfig::default()
        };
        assert!(probe_reachable(&web).await);

        let mut free = web.clone();
        free.port = 1;
        assert!(!probe_reachable(&free).await);
    }
}
