//! The control socket: the only interface the Settings tab and `uflinux-rdsctl` use.
//!
//! Wire format is one JSON object per line in each direction, exactly as
//! defined by [`Request`]/[`Reply`]. Two clients share the socket with
//! different patience: `uflinux-rdsctl` waits ten seconds, the Settings daemon
//! gives up after 300 ms. Consequently a connection is served on its own task,
//! every wait is bounded, and a reply is always written — even for a request
//! this build cannot understand.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::time::timeout;
use tracing::{debug, warn};

use uflinux_rds_common::{Config, Reply, Request, SystemAction};

use crate::cli::Args;
use crate::error::{Error, Result};
use crate::session;
use crate::state::State;

/// OpenRC service name, matching `rds/ctl` and `rds/data/uflinux-rds.initd`.
pub(crate) const SERVICE: &str = "uflinux-rds";

/// Runlevel `enable` and `disable` operate on, matching `rds/ctl` and the
/// Settings helper.
const RUNLEVEL: &str = "default";

/// How long a client may take to send its single request line.
const READ_TIMEOUT: Duration = Duration::from_secs(5);
/// How long writing the reply may take before the client is forgotten.
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
/// A request larger than this is a mistake or an attack, not a client.
const MAX_FRAME: usize = 1024 * 1024;
/// Long enough for a slow `rc-service`, short enough not to hang the caller.
const OPENRC_TIMEOUT: Duration = Duration::from_secs(20);
/// How long the Settings daemon waits, used as the probe budget for takeover.
const PROBE_TIMEOUT: Duration = Duration::from_millis(400);
/// How long a disconnected session may take its leave before it is killed.
const DISCONNECT_GRACE: Duration = Duration::from_secs(5);

/// A bound control socket.
#[derive(Debug)]
pub(crate) struct Server {
    listener: UnixListener,
    path: PathBuf,
}

/// Create the control socket at `path`.
///
/// A stale socket left by a crashed broker is removed; a live one is not
/// touched. With `take_over` a live socket is replaced anyway, which is what
/// `--take-over` means: the operator knows better than the probe.
pub(crate) async fn bind(path: &Path, take_over: bool) -> Result<Server> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| Error::io("create socket directory", &e))?;
    }

    if path.symlink_metadata().is_ok() {
        if !take_over && probe(path).await.is_some() {
            return Err(Error::AlreadyRunning(path.to_path_buf()));
        }
        std::fs::remove_file(path).map_err(|e| Error::io("remove stale socket", &e))?;
    }

    let listener = UnixListener::bind(path).map_err(|e| Error::io("bind control socket", &e))?;
    // 0600: the socket is the control plane, and it lives in a shared /run.
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .map_err(|e| Error::io("protect control socket", &e))?;

    Ok(Server {
        listener,
        path: path.to_path_buf(),
    })
}

impl Server {
    /// The path the socket is bound to, for logging.
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Accept connections until the broker begins to shut down, then unlink.
    pub(crate) async fn serve(self, state: Arc<State>, args: Arc<Args>) {
        let Server { listener, path } = self;
        loop {
            let accepted = tokio::select! {
                accepted = listener.accept() => accepted,
                () = state.wait_shutdown() => break,
            };
            match accepted {
                Ok((stream, _)) => {
                    let state = Arc::clone(&state);
                    let args = Arc::clone(&args);
                    tokio::spawn(async move {
                        if let Err(error) = handle(stream, state, args).await {
                            debug!("control connection ended: {error}");
                        }
                    });
                }
                Err(error) => warn!("control socket accept failed: {error}"),
            }
        }
        remove_socket(&path);
    }
}

/// Remove the socket file, ignoring a file someone else already replaced.
pub(crate) fn remove_socket(path: &Path) {
    let _ = std::fs::remove_file(path);
}

/// Ask a socket whether a broker is answering behind it.
///
/// This is deliberately the same conversation the Settings daemon has, so the
/// two components agree on what "another broker is running" means. It is async
/// because `bind` runs on the runtime: a blocking read here would park the
/// worker thread and could deadlock against the very broker being probed.
pub(crate) async fn probe(path: &Path) -> Option<Reply> {
    let stream = timeout(PROBE_TIMEOUT, UnixStream::connect(path))
        .await
        .ok()?
        .ok()?;
    let line = serde_json::to_string(&Request::GetState).ok()? + "\n";
    let mut stream = stream;
    timeout(PROBE_TIMEOUT, stream.write_all(line.as_bytes()))
        .await
        .ok()?
        .ok()?;
    let mut reply = String::new();
    timeout(
        PROBE_TIMEOUT,
        BufReader::new(&mut stream).read_line(&mut reply),
    )
    .await
    .ok()?
    .ok()?;
    serde_json::from_str(reply.trim_end()).ok()
}

/// Serve one client: read a single request, write a single reply.
async fn handle(stream: UnixStream, state: Arc<State>, args: Arc<Args>) -> Result<()> {
    // Counted for as long as the connection lives: a client that opens a
    // socket and thinks about it holds off the idle quit.
    let _client = state.control_client_attached();

    let mut stream = stream;
    let request = match read_request(&mut stream).await {
        Ok(request) => request,
        Err(message) => {
            return reply_to(&mut stream, Reply::Error { message }).await;
        }
    };

    let reply = dispatch(request, &state, &args).await;
    reply_to(&mut stream, reply).await
}

/// Read and decode one request line, with every failure turned into a message.
async fn read_request(stream: &mut UnixStream) -> std::result::Result<Request, String> {
    let mut line = String::new();
    let read = timeout(READ_TIMEOUT, BufReader::new(stream).read_line(&mut line))
        .await
        .map_err(|_| "timed out waiting for a request".to_owned())?
        .map_err(|error| format!("cannot read request: {error}"))?;
    if read == 0 {
        return Err("connection closed without a request".to_owned());
    }
    if line.len() > MAX_FRAME {
        return Err("request is too large".to_owned());
    }
    let trimmed = line.trim_end();
    if trimmed.is_empty() {
        return Err("empty request".to_owned());
    }
    serde_json::from_str(trimmed).map_err(|error| format!("unreadable request: {error}"))
}

/// Write one reply line and close the connection.
async fn reply_to(stream: &mut UnixStream, reply: Reply) -> Result<()> {
    let mut encoded =
        serde_json::to_vec(&reply).map_err(|error| Error::Usage(error.to_string()))?;
    encoded.push(b'\n');
    timeout(WRITE_TIMEOUT, stream.write_all(&encoded))
        .await
        .map_err(|_| Error::Timeout("writing a reply"))?
        .map_err(|error| Error::io("write reply", &error))?;
    let _ = timeout(WRITE_TIMEOUT, stream.shutdown()).await;
    Ok(())
}

/// Act on one request. Nothing here may panic: a bad request gets an error.
async fn dispatch(request: Request, state: &State, args: &Args) -> Reply {
    match request {
        Request::GetState => Reply::Ok {
            state: Some(Box::new(state.snapshot())),
            sessions: None,
        },
        Request::ListSessions => Reply::Ok {
            state: None,
            sessions: Some(state.sessions()),
        },
        Request::Disconnect { session } => disconnect(state, session).await,
        Request::Execute { action } => execute(action, state, args).await,
    }
}

/// Answer `disconnect`. The roster is authoritative, so an unknown id is an
/// error rather than a silent success.
async fn disconnect(state: &State, session: u32) -> Reply {
    if state.session_pid(session).is_none() {
        return Reply::Error {
            message: format!("no session {session}"),
        };
    }
    session::disconnect(state, session, DISCONNECT_GRACE);
    Reply::Ok {
        state: Some(Box::new(state.snapshot())),
        sessions: Some(state.sessions()),
    }
}

/// Answer `execute`.
async fn execute(action: SystemAction, state: &State, args: &Args) -> Reply {
    match action {
        SystemAction::ApplyConfig { config } => apply_config(state, &config),
        SystemAction::SetRunning { running } => set_running(state, args, running),
        SystemAction::SetEnabled { enabled } => set_enabled(state, enabled).await,
    }
}

/// Answer `apply_config`: validate, persist, adopt.
///
/// The file is written before the in-process copy is replaced, so a failed
/// write leaves the running configuration untouched. The caller sends its own
/// validated copy, but it is validated again here — the broker is the last
/// place that can stop a nonsensical configuration from taking effect.
///
/// Applying a configuration that changes `enabled` also starts or stops the
/// listeners (see [`State::replace_config`]), which is what the Settings switch
/// promises; any other field is adopted without disturbing a running service.
fn apply_config(state: &State, config: &Config) -> Reply {
    if let Err(error) = config.validate() {
        return Reply::Error {
            message: error.to_string(),
        };
    }
    let path = state.config_path().to_path_buf();
    if let Err(error) = config.save(&path) {
        warn!("cannot save {path:?}: {error}");
        return Reply::Error {
            message: error.to_string(),
        };
    }
    state.replace_config(config.clone());
    state.notify_change();
    Reply::Ok {
        state: Some(Box::new(state.snapshot())),
        sessions: None,
    }
}

/// Answer `set_running`: start or stop the listeners in this process.
///
/// The broker owns its own sockets, so this needs no privilege. When it *is*
/// root the OpenRC service is nudged as well, after the reply has been written,
/// so that `is_started` in the runscript agrees with reality; a failure there
/// is logged and does not fail the request, because the listeners — which are
/// what the user actually asked for — already changed.
fn set_running(state: &State, args: &Args, running: bool) -> Reply {
    if running && !state.config().enabled {
        return Reply::Error {
            message: "RDS is disabled; enable it first".to_owned(),
        };
    }
    state.set_desired_running(running);
    state.notify_change();

    if running_as_root() {
        let verb = if running { "start" } else { "stop" };
        tokio::spawn(async move {
            if let Err(message) = openrc("rc-service", &[SERVICE, verb]).await {
                warn!("rc-service {SERVICE} {verb}: {message}");
            }
        });
    }
    let _ = args;
    Reply::Ok {
        state: Some(Box::new(state.snapshot())),
        sessions: None,
    }
}

/// Answer `set_enabled`, mirroring `uflinux-rdsctl enable|disable`: only root
/// may change the default runlevel, and the broker says so instead of pretending.
async fn set_enabled(state: &State, enabled: bool) -> Reply {
    // Persistence lives in the runlevel, so this is `rc-update`, not
    // `rc-service`; the latter has no enable/disable verb.
    let verb = if enabled { "enable" } else { "disable" };
    if !running_as_root() {
        return Reply::Error {
            message: format!("{verb} needs root (try: sudo uflinux-rdsctl {verb})"),
        };
    }
    let args = if enabled {
        ["add", SERVICE, RUNLEVEL]
    } else {
        ["del", SERVICE, RUNLEVEL]
    };
    match openrc("rc-update", &args).await {
        Ok(()) => {
            state.set_enabled(enabled);
            state.notify_change();
            Reply::Ok {
                state: Some(Box::new(state.snapshot())),
                sessions: None,
            }
        }
        Err(message) => Reply::Error { message },
    }
}

/// True when the broker is root and may therefore talk to OpenRC.
fn running_as_root() -> bool {
    // SAFETY: geteuid has no preconditions.
    unsafe { libc::geteuid() == 0 }
}

/// Run one OpenRC command, argv-only, exactly like `rds/ctl` does. `rc-service`
/// wants the service name before the verb; `rc-update` wants verb, name, runlevel.
async fn openrc(executable: &str, args: &[&str]) -> std::result::Result<(), String> {
    let mut command = tokio::process::Command::new(executable);
    command
        .args(args)
        .env("LC_ALL", "C")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let status = timeout(OPENRC_TIMEOUT, command.status())
        .await
        .map_err(|_| format!("{executable} {} timed out", args.join(" ")))?
        .map_err(|_| format!("cannot run {executable}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{executable} {} failed", args.join(" ")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixStream as StdUnixStream;

    use tokio::io::AsyncWriteExt;

    use uflinux_rds_common::Config;

    fn temp_socket(name: &str) -> PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!("uflinux-rds-test-{name}.sock"));
        let _ = std::fs::remove_file(&path);
        path
    }

    fn state() -> Arc<State> {
        Arc::new(State::new(
            Config::default(),
            Path::new("/nonexistent/rds.json"),
        ))
    }

    /// Exactly what `uflinux-rdsctl` does: connect, one line, half-close, read
    /// one line back.
    async fn one_request(socket: &Path, request: &Request) -> Reply {
        let mut stream = UnixStream::connect(socket)
            .await
            .unwrap_or_else(|error| panic!("cannot connect to {}: {error}", socket.display()));
        let mut encoded = serde_json::to_vec(request).unwrap();
        encoded.push(b'\n');
        stream.write_all(&encoded).await.unwrap();
        stream.shutdown().await.unwrap();

        let mut line = String::new();
        BufReader::new(&mut stream)
            .read_line(&mut line)
            .await
            .unwrap();
        serde_json::from_str(line.trim_end()).unwrap()
    }

    #[tokio::test]
    async fn a_request_round_trips_over_a_real_socket() {
        let path = temp_socket("roundtrip");
        let state = state();
        let args = Arc::new(Args::parse(std::iter::empty(), &crate::cli::Env::default()).unwrap());
        let server = bind(&path, false).await.unwrap();
        let listener_task = tokio::spawn({
            let state = Arc::clone(&state);
            let args = Arc::clone(&args);
            async move { server.serve(state, args).await }
        });

        let reply = one_request(&path, &Request::GetState).await;
        match reply {
            Reply::Ok {
                state: Some(state), ..
            } => {
                // The Settings daemon prefers `effective`, so it must be there.
                let json = serde_json::to_value(&*state).unwrap();
                assert_eq!(json["enabled"], serde_json::json!(false));
                assert!(json.get("effective").is_some());
                assert!(json.get("fault").is_none());
            }
            other => panic!("unexpected reply {other:?}"),
        }

        state.begin_shutdown();
        let _ = listener_task.await;
        assert!(
            StdUnixStream::connect(&path).is_err(),
            "the socket file must be removed on shutdown"
        );
    }

    #[tokio::test]
    async fn a_garbage_line_gets_an_error_not_a_panic() {
        let path = temp_socket("garbage");
        let state = state();
        let args = Arc::new(Args::parse(std::iter::empty(), &crate::cli::Env::default()).unwrap());
        let server = bind(&path, false).await.unwrap();
        let task = tokio::spawn({
            let state = Arc::clone(&state);
            let args = Arc::clone(&args);
            async move { server.serve(state, args).await }
        });

        let stream = StdUnixStream::connect(&path).unwrap();
        drop(stream);

        state.begin_shutdown();
        let _ = task.await;
    }

    #[tokio::test]
    async fn an_unknown_session_is_refused() {
        let state = state();
        let args = Arc::new(Args::parse(std::iter::empty(), &crate::cli::Env::default()).unwrap());
        let reply = dispatch(Request::Disconnect { session: 42 }, &state, &args).await;
        match reply {
            Reply::Error { message } => assert!(message.contains("42"), "{message}"),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn running_a_disabled_broker_is_refused() {
        let state = state();
        let args = Arc::new(Args::parse(std::iter::empty(), &crate::cli::Env::default()).unwrap());
        let reply = dispatch(
            Request::Execute {
                action: SystemAction::SetRunning { running: true },
            },
            &state,
            &args,
        )
        .await;
        assert!(matches!(reply, Reply::Error { .. }), "{reply:?}");
    }

    #[tokio::test]
    async fn apply_config_writes_the_file_and_adopts_it() {
        let dir = std::env::temp_dir().join(format!("uflinux-rds-apply-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rds.json");
        let state = Arc::new(State::new(Config::default(), &path));
        let args = Arc::new(Args::parse(std::iter::empty(), &crate::cli::Env::default()).unwrap());

        let config = Config {
            enabled: true,
            rdp_port: 3499,
            max_sessions: 9,
            ..Config::default()
        };

        let reply = dispatch(
            Request::Execute {
                action: SystemAction::ApplyConfig {
                    config: Box::new(config),
                },
            },
            &state,
            &args,
        )
        .await;
        assert!(matches!(reply, Reply::Ok { .. }), "{reply:?}");

        let reread = Config::load(&path).unwrap();
        assert_eq!(reread.rdp_port, 3499);
        assert_eq!(reread.max_sessions, 9);
        assert!(reread.enabled);
        assert_eq!(state.config().rdp_port, 3499);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_broken_config_is_never_written() {
        let dir = std::env::temp_dir().join(format!("uflinux-rds-invalid-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rds.json");
        let state = Arc::new(State::new(Config::default(), &path));
        let args = Arc::new(Args::parse(std::iter::empty(), &crate::cli::Env::default()).unwrap());

        let config = Config {
            rdp_port: 0,
            ..Config::default()
        };
        let reply = dispatch(
            Request::Execute {
                action: SystemAction::ApplyConfig {
                    config: Box::new(config),
                },
            },
            &state,
            &args,
        )
        .await;
        assert!(matches!(reply, Reply::Error { .. }), "{reply:?}");
        assert!(!path.exists(), "an invalid config must not reach the disk");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_live_socket_refuses_a_second_broker() {
        let path = temp_socket("stale");
        let state = state();
        let args = Arc::new(Args::parse(std::iter::empty(), &crate::cli::Env::default()).unwrap());
        let server = bind(&path, false).await.unwrap();
        let task = tokio::spawn({
            let state = Arc::clone(&state);
            let args = Arc::clone(&args);
            async move { server.serve(state, args).await }
        });

        let second = bind(&path, false).await;
        match second {
            Err(Error::AlreadyRunning(found)) => assert_eq!(found, path),
            other => panic!("expected AlreadyRunning, got {other:?}"),
        }
        // Takeover is allowed, and the probe is what told us the difference.
        assert!(probe(&path).await.is_some());
        state.begin_shutdown();
        let _ = task.await;
    }
}
