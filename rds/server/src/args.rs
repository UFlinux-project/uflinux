//! Command line for `uflinux-rdp-session`.
//!
//! The broker builds a fixed argv (`--socket-fd 3 --status-fd 4 --config
//! <path> --session-command <cmd>`) and mirrors the same values into the
//! environment, so every flag also has an `UFLINUX_RDS_*` fallback. Parsing is
//! hand-rolled: this binary is forked once per client, and pulling in a derive
//! macro crate for eight flags would add to the per-connection start-up cost
//! for no benefit.
//!
//! `--socket-fd` is accepted as an alias of `--client-fd` because that is the
//! spelling the broker uses; both name the same descriptor.

use std::path::PathBuf;

/// How the client reached this process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    /// A plain RDP connection from the broker's TCP listener.
    Rdp,
    /// A browser connection relayed by `uflinux-rds-gateway`. The gateway has
    /// already terminated TLS, which changes how the server is configured.
    Web,
}

impl Transport {
    /// The label used in status frames and the Settings UI.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Rdp => "rdp",
            Self::Web => "web",
        }
    }

    /// Parses `rdp`/`web`, case-insensitively.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "rdp" => Some(Self::Rdp),
            "web" => Some(Self::Web),
            _ => None,
        }
    }
}

/// Why the command line was rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArgError {
    /// A flag was given without its value.
    MissingValue(String),
    /// A value that had to be a number was not, or did not fit.
    InvalidNumber {
        /// The offending flag.
        flag: String,
        /// The text that failed to parse.
        value: String,
    },
    /// A value that had to name a transport was not `rdp` or `web`.
    InvalidTransport(String),
    /// A flag this binary does not implement.
    Unknown(String),
}

impl core::fmt::Display for ArgError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::MissingValue(flag) => write!(f, "{flag} requires a value"),
            Self::InvalidNumber { flag, value } => {
                write!(f, "{flag} expects a number, got {value:?}")
            }
            Self::InvalidTransport(value) => {
                write!(f, "--transport expects \"rdp\" or \"web\", got {value:?}")
            }
            Self::Unknown(flag) => write!(f, "unknown option {flag}"),
        }
    }
}

impl std::error::Error for ArgError {}

/// Everything the command line and environment say about this session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Args {
    /// Configuration file to load.
    pub config: PathBuf,
    /// Descriptor of the already-connected client socket. `None` means the
    /// broker did not hand one over, which is fatal.
    pub client_fd: Option<i32>,
    /// Descriptor of the `SOCK_DGRAM` status channel back to the broker.
    /// `None` only costs the broker its live roster detail.
    pub status_fd: Option<i32>,
    /// User to authenticate as when PAM is not in use.
    pub user: Option<String>,
    /// Which front end forwarded the connection.
    pub transport: Transport,
    /// Overrides `Config::session_command`.
    pub session_command: Option<String>,
    /// Overrides `Config::idle_timeout_secs`.
    pub idle_timeout: Option<u64>,
    /// `-h`/`--help` was given.
    pub help: bool,
}

impl Default for Args {
    fn default() -> Self {
        Self {
            config: PathBuf::from(uflinux_rds_common::DEFAULT_CONFIG_PATH),
            client_fd: None,
            status_fd: None,
            user: None,
            transport: Transport::Rdp,
            session_command: None,
            idle_timeout: None,
            help: false,
        }
    }
}

/// Environment variables read when a flag is absent.
const ENV_CLIENT_FD: &str = "UFLINUX_RDS_CLIENT_FD";
const ENV_STATUS_FD: &str = "UFLINUX_RDS_STATUS_FD";
const ENV_CONFIG: &str = "UFLINUX_RDS_CONFIG";
const ENV_SESSION_COMMAND: &str = "UFLINUX_RDS_SESSION_COMMAND";
const ENV_SESSION_USER: &str = "UFLINUX_RDS_SESSION_USER";
const ENV_TRANSPORT: &str = "UFLINUX_RDS_TRANSPORT";

/// The first non-empty value for `key`, preferring the command line.
fn env_value(env: &[(String, String)], key: &str) -> Option<String> {
    env.iter()
        .find(|(name, _)| name == key)
        .map(|(_, value)| value.clone())
        .filter(|value| !value.trim().is_empty())
}

impl Args {
    /// Parses `argv[1..]` with `env` as the process environment.
    ///
    /// Split out from `std::env` access so the tests can drive every flag
    /// without spawning a process.
    pub fn parse<I, E>(argv: I, env: E) -> Result<Self, ArgError>
    where
        I: IntoIterator<Item = String>,
        E: IntoIterator<Item = (String, String)>,
    {
        let env: Vec<(String, String)> = env.into_iter().collect();
        let mut args = Self {
            config: env_value(&env, ENV_CONFIG)
                .map(PathBuf::from)
                .unwrap_or_default(),
            client_fd: env_value(&env, ENV_CLIENT_FD).and_then(|v| v.trim().parse().ok()),
            status_fd: env_value(&env, ENV_STATUS_FD).and_then(|v| v.trim().parse().ok()),
            user: env_value(&env, ENV_SESSION_USER),
            session_command: env_value(&env, ENV_SESSION_COMMAND),
            // An unrecognised value in the environment is ignored rather than
            // fatal: the environment is the fallback, and a stray variable
            // should not stop a session that the command line describes.
            transport: env_value(&env, ENV_TRANSPORT)
                .and_then(|value| Transport::parse(&value))
                .unwrap_or(Transport::Rdp),
            ..Self::default()
        };
        if args.config.as_os_str().is_empty() {
            args.config = PathBuf::from(uflinux_rds_common::DEFAULT_CONFIG_PATH);
        }

        let mut items = argv.into_iter();
        while let Some(flag) = items.next() {
            match flag.as_str() {
                "-h" | "--help" => args.help = true,
                "--config" => args.config = PathBuf::from(text(&mut items, &flag)?),
                "--client-fd" | "--socket-fd" => {
                    args.client_fd = Some(descriptor(&mut items, &flag)?);
                }
                "--status-fd" => args.status_fd = Some(descriptor(&mut items, &flag)?),
                "--user" => args.user = Some(text(&mut items, &flag)?),
                "--transport" => {
                    let value = text(&mut items, &flag)?;
                    args.transport =
                        Transport::parse(&value).ok_or(ArgError::InvalidTransport(value))?;
                }
                "--session-command" => args.session_command = Some(text(&mut items, &flag)?),
                "--idle-timeout" => args.idle_timeout = Some(number(&mut items, &flag)?),
                other => return Err(ArgError::Unknown(other.to_owned())),
            }
        }
        Ok(args)
    }

    /// The user this session may run as, for the non-PAM fallback.
    ///
    /// An empty name is rejected rather than treated as unset: a session that
    /// cannot say who it belongs to must not start a desktop.
    pub fn session_user(&self) -> Option<&str> {
        self.user.as_deref().filter(|name| !name.trim().is_empty())
    }

    /// `-h`/`--help` text.
    pub const HELP: &'static str = "\
uflinux-rdp-session - host one user's desktop over one RDP connection

The broker hands over the already-accepted client socket as fd 3 and a
SOCK_DGRAM status channel as fd 4; both descriptors are inherited, not
created here. Everything else is policy and comes from the config file.

Options:
  --config <path>          configuration file (default /etc/uflinux/rds.json,
                           $UFLINUX_RDS_CONFIG)
  --client-fd <n>          connected client socket ($UFLINUX_RDS_CLIENT_FD);
                           --socket-fd is the same flag under the name the
                           broker uses
  --status-fd <n>          status datagram socket to the broker
                           ($UFLINUX_RDS_STATUS_FD)
  --user <name>            user to run as when PAM is not built in
                           ($UFLINUX_RDS_SESSION_USER)
  --transport rdp|web      front end that forwarded the connection
                           ($UFLINUX_RDS_TRANSPORT)
  --session-command <cmd>  desktop to start ($UFLINUX_RDS_SESSION_COMMAND)
  --idle-timeout <secs>    override idle_timeout_secs from the config
  -h, --help               print this help
";
}

fn text(items: &mut impl Iterator<Item = String>, flag: &str) -> Result<String, ArgError> {
    items
        .next()
        .ok_or_else(|| ArgError::MissingValue(flag.to_owned()))
}

fn number(items: &mut impl Iterator<Item = String>, flag: &str) -> Result<u64, ArgError> {
    let value = text(items, flag)?;
    value.trim().parse().map_err(|_| ArgError::InvalidNumber {
        flag: flag.to_owned(),
        value,
    })
}

/// Parses a descriptor number. Negative values are rejected here rather than
/// later: a negative fd is always a bug in the caller, and `FromRawFd` would
/// turn it into a confusing `EBADF` somewhere far away.
fn descriptor(items: &mut impl Iterator<Item = String>, flag: &str) -> Result<i32, ArgError> {
    let value = text(items, flag)?;
    value
        .trim()
        .parse::<i32>()
        .ok()
        .filter(|fd| *fd >= 0)
        .ok_or_else(|| ArgError::InvalidNumber {
            flag: flag.to_owned(),
            value,
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(argv: &[&str]) -> Result<Args, ArgError> {
        Args::parse(
            argv.iter().map(|s| (*s).to_owned()),
            Vec::<(String, String)>::new(),
        )
    }

    #[test]
    fn defaults_match_the_broker_contract() {
        let args = parse(&[]).expect("empty command line");
        assert_eq!(args.config, PathBuf::from("/etc/uflinux/rds.json"));
        assert_eq!(args.client_fd, None);
        assert_eq!(args.status_fd, None);
        assert_eq!(args.transport, Transport::Rdp);
        assert!(!args.help);
    }

    #[test]
    fn every_flag_is_honoured() {
        let args = parse(&[
            "--config",
            "/tmp/rds.json",
            "--client-fd",
            "7",
            "--status-fd",
            "8",
            "--user",
            "panos",
            "--transport",
            "web",
            "--session-command",
            "labwc --session foo",
            "--idle-timeout",
            "0",
        ])
        .expect("full command line");
        assert_eq!(args.config, PathBuf::from("/tmp/rds.json"));
        assert_eq!(args.client_fd, Some(7));
        assert_eq!(args.status_fd, Some(8));
        assert_eq!(args.session_user(), Some("panos"));
        assert_eq!(args.transport, Transport::Web);
        assert_eq!(args.session_command.as_deref(), Some("labwc --session foo"));
        assert_eq!(args.idle_timeout, Some(0));
    }

    #[test]
    fn socket_fd_is_an_alias_of_client_fd() {
        // This is the exact argv the broker builds.
        let args = parse(&[
            "--socket-fd",
            "3",
            "--status-fd",
            "4",
            "--config",
            "/etc/uflinux/rds.json",
            "--session-command",
            "labwc",
        ])
        .expect("broker command line");
        assert_eq!(args.client_fd, Some(3));
        assert_eq!(args.status_fd, Some(4));
        assert_eq!(args.session_command.as_deref(), Some("labwc"));
    }

    #[test]
    fn help_flag_is_detected() {
        assert!(parse(&["-h"]).expect("-h").help);
        assert!(parse(&["--help"]).expect("--help").help);
    }

    #[test]
    fn bad_values_are_rejected_with_the_flag_name() {
        assert_eq!(
            parse(&["--client-fd", "x"]),
            Err(ArgError::InvalidNumber {
                flag: "--client-fd".to_owned(),
                value: "x".to_owned()
            })
        );
        assert_eq!(
            parse(&["--transport", "vnc"]),
            Err(ArgError::InvalidTransport("vnc".to_owned()))
        );
        assert_eq!(
            parse(&["--idle-timeout"]),
            Err(ArgError::MissingValue("--idle-timeout".to_owned()))
        );
        assert_eq!(
            parse(&["--nope"]),
            Err(ArgError::Unknown("--nope".to_owned()))
        );
    }

    #[test]
    fn environment_fills_in_missing_flags_and_argv_wins() {
        let env = vec![
            ("UFLINUX_RDS_CLIENT_FD".to_owned(), "3".to_owned()),
            ("UFLINUX_RDS_STATUS_FD".to_owned(), "4".to_owned()),
            ("UFLINUX_RDS_CONFIG".to_owned(), "/env/rds.json".to_owned()),
            ("UFLINUX_RDS_SESSION_COMMAND".to_owned(), "cage".to_owned()),
            ("UFLINUX_RDS_SESSION_USER".to_owned(), "fromenv".to_owned()),
        ];
        let args = Args::parse(Vec::new(), env.clone()).expect("env only");
        assert_eq!(args.client_fd, Some(3));
        assert_eq!(args.status_fd, Some(4));
        assert_eq!(args.config, PathBuf::from("/env/rds.json"));
        assert_eq!(args.session_command.as_deref(), Some("cage"));
        assert_eq!(args.session_user(), Some("fromenv"));

        let overridden = Args::parse(vec!["--client-fd".to_owned(), "9".to_owned()], env)
            .expect("argv overrides env");
        assert_eq!(overridden.client_fd, Some(9));
    }

    #[test]
    fn an_empty_user_is_not_a_user() {
        let args = parse(&["--user", "  "]).expect("blank user parses");
        assert_eq!(args.session_user(), None);
    }

    #[test]
    fn transport_labels_are_stable() {
        assert_eq!(Transport::Rdp.as_str(), "rdp");
        assert_eq!(Transport::Web.as_str(), "web");
        assert_eq!(Transport::parse(" WEB "), Some(Transport::Web));
        assert_eq!(Transport::parse("telnet"), None);
    }
}
