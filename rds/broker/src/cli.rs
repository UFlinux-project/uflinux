//! Command line for `uflinux-rdsd`.
//!
//! Hand-rolled rather than `clap`: the broker ships in a package whose whole
//! selling point is a small resident daemon, and `clap` is not a workspace
//! dependency. The shape mirrors `uflinux-rds-web` so both binaries behave the
//! same way under OpenRC (`command_args` in `/etc/conf.d/uflinux-rds`).
//!
//! Every flag can also arrive through the environment, because
//! `/etc/conf.d/uflinux-rds` is the file an integrator edits. Precedence is
//! always flag > environment > built-in default.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

/// Built-in location of the shared configuration file.
const DEFAULT_CONFIG: &str = "/etc/uflinux/rds.json";
/// Built-in path of the session binary the RDP listener forks.
const DEFAULT_SESSION_BINARY: &str = "/usr/libexec/uflinux-rdp-session";
/// Built-in path of the web gateway the broker supervises.
const DEFAULT_WEB_BINARY: &str = "/usr/bin/uflinux-rds-web";
/// Default idle-quit window. `0` makes the daemon resident.
const DEFAULT_IDLE_QUIT_SECS: u64 = 45;

/// Text printed for `-h`/`--help` and on a parse failure.
pub(crate) const HELP: &str = "\
uflinux-rdsd — UFLinux remote-desktop broker

USAGE
  uflinux-rdsd [OPTIONS]

OPTIONS
  --config <PATH>         configuration file (default /etc/uflinux/rds.json,
                          $UFLINUX_RDS_CONFIG)
  --socket <PATH>         control socket (default /run/uflinux-rds/control.sock,
                          $UFLINUX_RDS_CONTROL_SOCKET)
  --session-binary <PATH> program forked per RDP connection
                          (default /usr/libexec/uflinux-rdp-session,
                          $UFLINUX_RDS_SESSION_BINARY)
  --web-binary <PATH>     web gateway to supervise
                          (default /usr/bin/uflinux-rds-web,
                          $UFLINUX_RDS_WEB_BINARY)
  --idle-quit <SECS>      exit when idle for this long (default 45, 0 = resident)
  --resident              never quit on idle; same as --idle-quit 0
  --take-over             replace a stale control socket instead of refusing
  --rdp-bind <ADDR>       override the RDP bind address from the config
  --rdp-port <PORT>       override the RDP port from the config
  -h, --help              show this help

ENVIRONMENT
  UFLINUX_RDS_CONFIG, UFLINUX_RDS_CONTROL_SOCKET, UFLINUX_RDS_SESSION_BINARY,
  UFLINUX_RDS_WEB_BINARY, UFLINUX_RDS_H264, UFLINUX_RDS_SESSION_COMMAND,
  UFLINUX_RDS_RDP_PORT, UFLINUX_RDS_WEB_PORT, RUST_LOG
";

/// Environment snapshot used for defaults and config overrides.
///
/// A snapshot (rather than live `std::env::var` calls) keeps the resolution
/// order testable: a unit test can hand in an arbitrary map.
#[derive(Debug, Clone, Default)]
pub(crate) struct Env {
    vars: BTreeMap<String, String>,
}

impl Env {
    /// The real process environment.
    pub(crate) fn from_env() -> Self {
        Self {
            vars: std::env::vars().collect(),
        }
    }

    /// Build from an arbitrary map, for tests.
    #[cfg(test)]
    pub(crate) fn from_pairs(pairs: impl IntoIterator<Item = (String, String)>) -> Self {
        Self {
            vars: pairs.into_iter().collect(),
        }
    }

    /// Value of `key`, if set.
    pub(crate) fn get(&self, key: &str) -> Option<&str> {
        self.vars.get(key).map(String::as_str)
    }

    /// The `(key, value)` pairs `Config::apply_env_overrides` understands.
    pub(crate) fn overrides(&self) -> Vec<(String, String)> {
        self.vars
            .iter()
            .filter(|(key, _)| key.starts_with("UFLINUX_RDS_"))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect()
    }
}

/// Parsed, resolved startup arguments.
#[derive(Debug, Clone)]
pub(crate) struct Args {
    /// Shared config file; also where `apply_config` writes.
    pub(crate) config_path: PathBuf,
    /// Control socket the broker binds.
    pub(crate) socket_path: PathBuf,
    /// Program forked for each incoming RDP connection.
    pub(crate) session_binary: PathBuf,
    /// Web gateway child.
    pub(crate) web_binary: PathBuf,
    /// How long the daemon may sit with no clients, sessions or web users
    /// before exiting. Zero means resident.
    pub(crate) idle_quit: Duration,
    /// Replace a stale control socket rather than refusing to start.
    pub(crate) take_over: bool,
    /// CLI override for the RDP bind address.
    pub(crate) rdp_bind: Option<String>,
    /// CLI override for the RDP port.
    pub(crate) rdp_port: Option<u16>,
}

impl Args {
    /// Parse `argv` (without the program name) against `env`.
    ///
    /// Errors are single-line messages; `-h`/`--help` is reported as an error
    /// carrying [`HELP`] so `main` can print it and exit successfully.
    pub(crate) fn parse(argv: impl IntoIterator<Item = String>, env: &Env) -> Result<Self, String> {
        let mut config_path = None;
        let mut socket_path = None;
        let mut session_binary = None;
        let mut web_binary = None;
        let mut idle_quit: Option<u64> = None;
        let mut resident = false;
        let mut take_over = false;
        let mut rdp_bind = None;
        let mut rdp_port: Option<u16> = None;

        let mut it = argv.into_iter();
        while let Some(arg) = it.next() {
            match arg.as_str() {
                "-h" | "--help" => return Err(HELP.to_owned()),
                "--config" => config_path = Some(take_value(&mut it, "--config")?),
                "--socket" => socket_path = Some(take_value(&mut it, "--socket")?),
                "--session-binary" => {
                    session_binary = Some(take_value(&mut it, "--session-binary")?)
                }
                "--web-binary" => web_binary = Some(take_value(&mut it, "--web-binary")?),
                "--rdp-bind" => rdp_bind = Some(take_value(&mut it, "--rdp-bind")?),
                "--idle-quit" => {
                    let raw = take_value(&mut it, "--idle-quit")?;
                    idle_quit = Some(raw.parse::<u64>().map_err(|_| {
                        format!("--idle-quit expects a number of seconds, got {raw:?}")
                    })?);
                }
                "--rdp-port" => {
                    let raw = take_value(&mut it, "--rdp-port")?;
                    rdp_port = Some(
                        raw.parse::<u16>()
                            .map_err(|_| format!("--rdp-port expects a port, got {raw:?}"))?,
                    );
                }
                "--resident" => resident = true,
                "--take-over" => take_over = true,
                other => {
                    // Accept `--flag=value` as well; OpenRC's command_args
                    // quoting makes the two-word form awkward.
                    let Some((flag, value)) = other.split_once('=') else {
                        return Err(format!("unknown option {other}"));
                    };
                    match flag {
                        "--config" => config_path = Some(value.to_owned()),
                        "--socket" => socket_path = Some(value.to_owned()),
                        "--session-binary" => session_binary = Some(value.to_owned()),
                        "--web-binary" => web_binary = Some(value.to_owned()),
                        "--rdp-bind" => rdp_bind = Some(value.to_owned()),
                        "--idle-quit" => {
                            idle_quit = Some(value.parse::<u64>().map_err(|_| {
                                format!("--idle-quit expects a number of seconds, got {value:?}")
                            })?);
                        }
                        "--rdp-port" => {
                            rdp_port = Some(value.parse::<u16>().map_err(|_| {
                                format!("--rdp-port expects a port, got {value:?}")
                            })?);
                        }
                        _ => return Err(format!("unknown option {flag}")),
                    }
                }
            }
        }

        let config_path = config_path
            .or_else(|| env.get("UFLINUX_RDS_CONFIG").map(str::to_owned))
            .unwrap_or_else(|| DEFAULT_CONFIG.to_owned());
        let socket_path = socket_path
            .or_else(|| env.get("UFLINUX_RDS_CONTROL_SOCKET").map(str::to_owned))
            .unwrap_or_else(|| {
                uflinux_rds_common::control_socket_path()
                    .to_string_lossy()
                    .into_owned()
            });
        let session_binary = session_binary
            .or_else(|| env.get("UFLINUX_RDS_SESSION_BINARY").map(str::to_owned))
            .unwrap_or_else(|| DEFAULT_SESSION_BINARY.to_owned());
        let web_binary = web_binary
            .or_else(|| env.get("UFLINUX_RDS_WEB_BINARY").map(str::to_owned))
            .unwrap_or_else(|| DEFAULT_WEB_BINARY.to_owned());

        // `--resident` wins over any `--idle-quit`, and over the environment
        // default, so an integrator can pin residency in the unit file.
        let secs = if resident {
            0
        } else {
            idle_quit.unwrap_or(DEFAULT_IDLE_QUIT_SECS)
        };

        Ok(Self {
            config_path: PathBuf::from(config_path),
            socket_path: PathBuf::from(socket_path),
            session_binary: PathBuf::from(session_binary),
            web_binary: PathBuf::from(web_binary),
            idle_quit: Duration::from_secs(secs),
            take_over,
            rdp_bind,
            rdp_port,
        })
    }
}

/// Consume the value that follows `flag`.
fn take_value(it: &mut impl Iterator<Item = String>, flag: &str) -> Result<String, String> {
    it.next().ok_or_else(|| format!("{flag} expects a value"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(argv: &[&str]) -> Result<Args, String> {
        Args::parse(argv.iter().map(|arg| (*arg).to_owned()), &Env::default())
    }

    #[test]
    fn defaults_match_the_openrc_unit() {
        let args = parse(&[]).expect("no arguments is valid");
        assert_eq!(args.config_path, PathBuf::from("/etc/uflinux/rds.json"));
        assert_eq!(
            args.socket_path,
            uflinux_rds_common::control_socket_path().to_path_buf()
        );
        assert_eq!(
            args.session_binary,
            PathBuf::from("/usr/libexec/uflinux-rdp-session")
        );
        assert_eq!(args.web_binary, PathBuf::from("/usr/bin/uflinux-rds-web"));
        assert_eq!(args.idle_quit, Duration::from_secs(45));
        assert!(!args.take_over);
        assert!(args.rdp_bind.is_none());
        assert!(args.rdp_port.is_none());
    }

    #[test]
    fn session_binary_flag_is_honoured() {
        let args = parse(&["--session-binary", "/bin/false"]).expect("parses");
        assert_eq!(args.session_binary, PathBuf::from("/bin/false"));
    }

    #[test]
    fn idle_quit_and_resident_are_configurable() {
        assert_eq!(
            parse(&["--idle-quit", "5"]).expect("parses").idle_quit,
            Duration::from_secs(5)
        );
        assert_eq!(
            parse(&["--idle-quit", "0"]).expect("parses").idle_quit,
            Duration::from_secs(0)
        );
        // `--resident` overrides an explicit window so a unit file can pin it.
        let args = parse(&["--idle-quit", "30", "--resident"]).expect("parses");
        assert_eq!(args.idle_quit, Duration::from_secs(0));
    }

    #[test]
    fn take_over_flag_is_honoured() {
        assert!(parse(&["--take-over"]).expect("parses").take_over);
    }

    #[test]
    fn equals_form_works() {
        let args = parse(&["--idle-quit=10", "--rdp-port=3400"]).expect("parses");
        assert_eq!(args.idle_quit, Duration::from_secs(10));
        assert_eq!(args.rdp_port, Some(3400));
    }

    #[test]
    fn bad_input_is_rejected() {
        assert!(parse(&["--idle-quit", "soon"]).is_err());
        assert!(parse(&["--rdp-port", "70000"]).is_err());
        assert!(parse(&["--config"]).is_err());
        assert!(parse(&["--nope"]).is_err());
        assert!(parse(&["positional"]).is_err());
    }

    #[test]
    fn help_is_reported_as_an_error_carrying_the_usage() {
        let err = parse(&["--help"]).expect_err("help is an error");
        assert!(err.contains("USAGE"), "{err}");
    }

    #[test]
    fn environment_supplies_defaults_but_flags_win() {
        let env = Env::from_pairs([
            ("UFLINUX_RDS_CONFIG".to_owned(), "/tmp/a.json".to_owned()),
            (
                "UFLINUX_RDS_SESSION_BINARY".to_owned(),
                "/bin/true".to_owned(),
            ),
            ("UFLINUX_RDS_WEB_BINARY".to_owned(), "/bin/echo".to_owned()),
            (
                "UFLINUX_RDS_CONTROL_SOCKET".to_owned(),
                "/tmp/x.sock".to_owned(),
            ),
        ]);
        let args = Args::parse([], &env).expect("parses");
        assert_eq!(args.config_path, PathBuf::from("/tmp/a.json"));
        assert_eq!(args.session_binary, PathBuf::from("/bin/true"));
        assert_eq!(args.web_binary, PathBuf::from("/bin/echo"));
        assert_eq!(args.socket_path, PathBuf::from("/tmp/x.sock"));

        let args =
            Args::parse(["--config".to_owned(), "/tmp/b.json".to_owned()], &env).expect("parses");
        assert_eq!(args.config_path, PathBuf::from("/tmp/b.json"));
    }

    #[test]
    fn env_snapshot_only_forwards_prefixed_keys() {
        let env = Env::from_pairs([
            ("UFLINUX_RDS_RDP_PORT".to_owned(), "3500".to_owned()),
            ("PATH".to_owned(), "/bin".to_owned()),
        ]);
        let overrides = env.overrides();
        assert_eq!(overrides.len(), 1);
        assert_eq!(overrides[0].0, "UFLINUX_RDS_RDP_PORT");
    }
}
