//! `uflinux-rdsctl` — command-line control for the UFLinux RDS broker.
//!
//! Talks to `uflinux-rdsd` over the control socket using the shared
//! [`Request`]/[`Reply`] protocol. Service persistence (`enable`, `disable`)
//! shells out to `rc-update`, so those verbs need root; the Settings UI reaches
//! the same effect through the polkit helper instead.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use uflinux_rds_common::openrc;
use uflinux_rds_common::{BrokerState, Config, Reply, Request, SessionInfo, SystemAction};

const SERVICE: &str = "uflinux-rds";
/// Runlevel `enable` and `disable` operate on, matching the Settings helper.
const RUNLEVEL: &str = "default";
const USAGE: &str = "\
Usage: uflinux-rdsctl [OPTIONS] <COMMAND>

Commands:
  status               Configuration, listener and roster summary
  sessions             One line per live session
  disconnect <id>      Terminate one session
  apply <file>         Push a JSON configuration to the broker
  enable               Add the service to the default runlevel
  disable              Remove the service from the default runlevel
  start                Start the service now
  stop                 Stop the service now

Options:
  --socket <path>      Control socket (default $UFLINUX_RDS_CONTROL_SOCKET or
                       /run/uflinux-rds/control.sock)
  -h, --help           Show this help
";

fn main() {
    let code = match run() {
        Ok(()) => 0,
        Err(message) => {
            eprintln!("uflinux-rdsctl: {message}");
            1
        }
    };
    std::process::exit(code);
}

fn run() -> Result<(), String> {
    let mut socket: Option<PathBuf> = None;
    let mut command: Option<String> = None;
    let mut argument: Option<String> = None;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                print!("{USAGE}");
                return Ok(());
            }
            "--socket" => {
                socket = Some(PathBuf::from(args.next().ok_or("--socket needs a path")?));
            }
            other if other.starts_with('-') => return Err(format!("unknown option {other}")),
            other => {
                if command.is_none() {
                    command = Some(other.to_owned());
                } else if argument.is_none() {
                    argument = Some(other.to_owned());
                } else {
                    return Err(format!("unexpected argument {other}"));
                }
            }
        }
    }

    let socket = socket.unwrap_or_else(uflinux_rds_common::control_socket_path);
    match command.as_deref() {
        None => Err(format!("no command given\n{USAGE}")),
        Some("status") => status(&socket),
        Some("sessions") => sessions(&socket),
        Some("disconnect") => {
            let id = argument
                .ok_or("disconnect needs a session id")?
                .parse::<u32>()
                .map_err(|_| "session id must be a number".to_owned())?;
            let reply = exchange(&socket, Request::Disconnect { session: id })?;
            expect_ok(reply)?;
            println!("session {id} disconnected");
            Ok(())
        }
        Some("apply") => {
            let path = argument.ok_or("apply needs a configuration file")?;
            let config =
                Config::load(Path::new(&path)).map_err(|e| format!("cannot read {path}: {e}"))?;
            config.validate().map_err(|e| format!("{path}: {e}"))?;
            expect_ok(exchange(
                &socket,
                Request::Execute {
                    action: SystemAction::ApplyConfig {
                        config: Box::new(config),
                    },
                },
            )?)?;
            println!("configuration applied");
            Ok(())
        }
        Some("enable") => set_enabled(&socket, true),
        Some("disable") => set_enabled(&socket, false),
        Some("start") => service_toggle(&socket, true),
        Some("stop") => service_toggle(&socket, false),
        Some(other) => Err(format!("unknown command {other}\n{USAGE}")),
    }
}

fn status(socket: &Path) -> Result<(), String> {
    match exchange(socket, Request::GetState)? {
        Reply::Ok {
            state: Some(state), ..
        } => {
            print_state(&state);
            Ok(())
        }
        Reply::Ok { .. } => Err("broker returned no state".to_owned()),
        Reply::Error { message } => Err(message),
    }
}

fn sessions(socket: &Path) -> Result<(), String> {
    match exchange(socket, Request::ListSessions)? {
        Reply::Ok {
            sessions: Some(sessions),
            ..
        } => {
            for session in sessions {
                print_session(&session);
            }
            Ok(())
        }
        Reply::Ok { .. } => Err("broker returned no sessions".to_owned()),
        Reply::Error { message } => Err(message),
    }
}

fn print_state(state: &BrokerState) {
    let config = &state.effective;
    println!(
        "Service:   {} / {}",
        if state.enabled { "enabled" } else { "disabled" },
        if state.running { "running" } else { "stopped" }
    );
    println!(
        "RDP:       {} on {}:{} (NLA {})",
        if state.rdp_listening {
            "listening"
        } else {
            "closed"
        },
        config.rdp_bind,
        config.rdp_port,
        if config.nla_required {
            "required"
        } else {
            "off, TLS only"
        }
    );
    if config.web.enabled {
        println!(
            "Web:       {} on {}:{} ({})",
            if state.web_listening {
                "listening"
            } else {
                "closed"
            },
            config.web.bind,
            config.web.port,
            if config.web.https { "https" } else { "http" }
        );
    } else {
        println!("Web:       disabled");
    }
    println!(
        "Codecs:    h264={}, remotefx={}, max {} fps",
        format!("{:?}", config.codec.h264).to_ascii_lowercase(),
        if config.codec.allow_remotefx {
            "on"
        } else {
            "off"
        },
        config.codec.max_fps
    );
    println!(
        "Sessions:  {} of {}",
        state.sessions.len(),
        config.max_sessions
    );
    for session in &state.sessions {
        print_session(session);
    }
    if let Some(fault) = &state.fault {
        println!("Fault:     {fault}");
    }
}

fn print_session(session: &SessionInfo) {
    let device = session
        .codec_device
        .as_deref()
        .map(|device| format!(" on {device}"))
        .unwrap_or_default();
    println!(
        "  {:>3}  {:<16} {:<4} {}x{}  {}{}  {}s up, {}s idle",
        session.id,
        session.username,
        session.transport,
        session.width,
        session.height,
        session.codec,
        device,
        session.connected_seconds,
        session.idle_seconds
    );
}

/// Send one request and wait for its single reply.
fn exchange(socket: &Path, request: Request) -> Result<Reply, String> {
    let stream = UnixStream::connect(socket)
        .map_err(|e| format!("cannot reach {SERVICE} at {}: {e}", socket.display()))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .map_err(|e| e.to_string())?;
    let mut encoded = serde_json::to_vec(&request).map_err(|e| e.to_string())?;
    encoded.push(b'\n');
    (&stream).write_all(&encoded).map_err(|e| e.to_string())?;
    stream.shutdown(std::net::Shutdown::Write).ok();

    let mut line = String::new();
    BufReader::new(&stream)
        .read_line(&mut line)
        .map_err(|e| e.to_string())?;
    if line.trim().is_empty() {
        return Err("broker closed the connection".to_owned());
    }
    serde_json::from_str(line.trim_end()).map_err(|e| e.to_string())
}

fn expect_ok(reply: Reply) -> Result<(), String> {
    match reply {
        Reply::Ok { .. } => Ok(()),
        Reply::Error { message } => Err(message),
    }
}

/// Run `rc-update add|del` for the verbs that change the default runlevel;
/// those need root, and the caller says which verb to name in the message.
fn rc_update(args: &[&str], verb: &str) -> Result<(), String> {
    if unsafe { libc::geteuid() } != 0 {
        return Err(format!(
            "{verb} needs root (try: sudo uflinux-rdsctl {verb})"
        ));
    }
    run_openrc(&openrc::rc_update(), args)
}

/// `enable` and `disable`: move both halves of the master switch together.
///
/// The runlevel decides whether the broker starts at boot, while the `enabled`
/// key of the configuration file is what a running broker — and therefore the
/// Settings UI — reports. Moving only the runlevel left the two disagreeing.
/// A live broker is asked to do the whole job itself, exactly like `start` and
/// `stop` do; the direct path is for a machine where the service is stopped.
fn set_enabled(socket: &Path, enabled: bool) -> Result<(), String> {
    let verb = if enabled { "enable" } else { "disable" };
    if unsafe { libc::geteuid() } != 0 {
        return Err(format!(
            "{verb} needs root (try: sudo uflinux-rdsctl {verb})"
        ));
    }
    let request = Request::Execute {
        action: SystemAction::SetEnabled { enabled },
    };
    if let Ok(reply) = exchange(socket, request) {
        return expect_ok(reply);
    }
    let path = uflinux_rds_common::config_file_path();
    Config::set_enabled_on_disk(&path, enabled)
        .map_err(|error| format!("cannot update {}: {error}", path.display()))?;
    let args = if enabled {
        ["add", SERVICE, RUNLEVEL]
    } else {
        ["del", SERVICE, RUNLEVEL]
    };
    rc_update(&args, verb)
}

/// `start` and `stop`: ask the broker to bring its own listeners down or up,
/// since it owns the sockets and can do it without root. Only when no broker
/// answers (the service really is stopped) does this fall back to the runscript,
/// which needs root.
fn service_toggle(socket: &Path, running: bool) -> Result<(), String> {
    let verb = if running { "start" } else { "stop" };
    let request = Request::Execute {
        action: SystemAction::SetRunning { running },
    };
    match exchange(socket, request) {
        Ok(reply) => expect_ok(reply),
        // The broker is not reachable, so there is nothing to toggle in-place.
        // The runscript is the only thing left that can change the state.
        Err(_) => run_openrc(&openrc::rc_service(), &[SERVICE, verb]),
    }
}

/// Run one OpenRC command, argv-only. `rc-service` takes the service name
/// *before* the verb, so the arguments arrive already in that order.
fn run_openrc(executable: &Path, args: &[&str]) -> Result<(), String> {
    let name = executable
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| executable.display().to_string());
    let status = Command::new(executable)
        .args(args)
        .env("LC_ALL", "C")
        .status()
        .map_err(|_| format!("cannot run {name}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{name} {} failed", args.join(" ")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_printing_accepts_a_default_broker_state() {
        let state = BrokerState {
            config: Config::default(),
            effective: Config::default(),
            enabled: false,
            running: false,
            rdp_listening: false,
            web_listening: false,
            sessions: Vec::new(),
            fault: None,
        };
        print_state(&state);
    }

    #[test]
    fn requests_round_trip_through_the_cli_encoding() {
        let json = serde_json::to_string(&Request::Disconnect { session: 7 }).unwrap();
        assert_eq!(json, r#"{"op":"disconnect","session":7}"#);
    }

    #[test]
    fn an_error_reply_becomes_an_error() {
        assert!(
            expect_ok(Reply::Error {
                message: "refused".to_owned()
            })
            .is_err()
        );
        assert!(
            expect_ok(Reply::Ok {
                state: None,
                sessions: None
            })
            .is_ok()
        );
    }
}
