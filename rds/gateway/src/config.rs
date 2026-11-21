//! Command line and environment configuration for `uflinux-rds-web`.

use std::path::{Path, PathBuf};

use uflinux_rds_common::Config;

use crate::Result;

/// Where the gateway reads its JSON configuration from.
pub const DEFAULT_CONFIG_PATH: &str = uflinux_rds_common::DEFAULT_CONFIG_PATH;

/// Environment variable understood by [`uflinux_rds_common::Config::apply_env_overrides`],
/// plus the gateway-specific [`ENV_STATE_DIR`].
pub const ENV_STATE_DIR: &str = "UFLINUX_RDS_STATE_DIR";

/// Parsed command line.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Args {
    /// Path of the JSON config file.
    pub config: PathBuf,
    /// `--port`: overrides `web.port`.
    pub port: Option<u16>,
    /// `--bind`: overrides `web.bind`.
    pub bind: Option<String>,
    /// `--plain-http`: serves plain HTTP even if `web.https` is set.
    pub plain_http: bool,
    /// `--state-dir`: where the self-signed certificate is written.
    pub state_dir: Option<PathBuf>,
}

impl Args {
    /// `--help` / `-h` text.
    pub const HELP: &str = "\
uflinux-rds-web — UFLinux RDS web gateway

USAGE:
    uflinux-rds-web [OPTIONS]

OPTIONS:
    --config <PATH>     JSON config file (default /etc/uflinux/rds.json,
                        env UFLINUX_RDS_CONFIG)
    --port <PORT>       override web.port
    --bind <ADDR>       override web.bind
    --plain-http        serve plain HTTP instead of HTTPS
    --state-dir <PATH>  directory for the self-signed certificate
                        (default /var/lib/uflinux-rds, env UFLINUX_RDS_STATE_DIR)
    -h, --help          print this help
";

    /// Parse `argv` (without the program name).
    pub fn parse<I: IntoIterator<Item = String>>(argv: I) -> std::result::Result<Self, String> {
        let mut args = Args {
            config: PathBuf::from(DEFAULT_CONFIG_PATH),
            ..Args::default()
        };
        let mut it = argv.into_iter();
        while let Some(arg) = it.next() {
            match arg.as_str() {
                "-h" | "--help" => return Err(Self::HELP.to_string()),
                "--config" => {
                    args.config = PathBuf::from(it.next().ok_or("--config needs a path")?)
                }
                "--port" => {
                    let raw = it.next().ok_or("--port needs a number")?;
                    args.port = Some(
                        raw.parse()
                            .map_err(|_| format!("--port: invalid port {raw:?}"))?,
                    );
                }
                "--bind" => args.bind = Some(it.next().ok_or("--bind needs an address")?),
                "--plain-http" => args.plain_http = true,
                "--state-dir" => {
                    args.state_dir =
                        Some(PathBuf::from(it.next().ok_or("--state-dir needs a path")?))
                }
                other => return Err(format!("unknown argument {other:?}")),
            }
        }
        Ok(args)
    }

    /// Config path from `UFLINUX_RDS_CONFIG` when no `--config` was given.
    pub fn config_path(env: &BTree, args: &Self) -> PathBuf {
        match env.get("UFLINUX_RDS_CONFIG") {
            Some(path)
                if !path.is_empty() && args.config.as_path() == Path::new(DEFAULT_CONFIG_PATH) =>
            {
                PathBuf::from(path)
            }
            _ => args.config.clone(),
        }
    }
}

/// Resolve the effective configuration: file + environment + command line.
///
/// `env` is the environment (injectable for tests); the file path comes from
/// `--config`/`UFLINUX_RDS_CONFIG`.
pub fn resolve(args: &Args, env: &BTree) -> Result<Resolved> {
    let mut config = Config::load(&args.config)?;
    config.apply_env_overrides(env.vars.iter().map(|(k, v)| (k.clone(), v.clone())));
    if let Some(port) = args.port {
        config.web.port = port;
    }
    if let Some(bind) = &args.bind {
        config.web.bind = bind.clone();
    }
    if args.plain_http {
        config.web.https = false;
    }
    config.validate()?;
    Ok(Resolved {
        state_dir: state_dir(args, env),
        config,
    })
}

/// A resolved, validated run configuration.
#[derive(Debug, Clone)]
pub struct Resolved {
    /// The shared `uflinux-rds` configuration, with overrides applied.
    pub config: Config,
    /// Directory used for the self-signed certificate.
    pub state_dir: PathBuf,
}

/// Injectable environment.
#[derive(Debug, Clone, Default)]
pub struct BTree {
    /// Variables in `KEY=VALUE` form, as from [`std::env::vars`].
    pub vars: Vec<(String, String)>,
}

impl BTree {
    /// The real process environment.
    pub fn from_env() -> Self {
        Self {
            vars: std::env::vars().collect(),
        }
    }

    /// Look up a variable.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.vars
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }
}

fn state_dir(args: &Args, env: &BTree) -> PathBuf {
    if let Some(dir) = &args.state_dir {
        return dir.clone();
    }
    match env.get(ENV_STATE_DIR) {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => std::path::PathBuf::from(uflinux_rds_common::STATE_DIR),
    }
}

/// Where the self-signed certificate lives inside `state_dir`.
pub fn self_signed_paths(state_dir: &std::path::Path) -> (PathBuf, PathBuf) {
    (
        state_dir.join("web-cert.pem"),
        state_dir.join("web-key.pem"),
    )
}

/// The RDP host/port a token points at: always the local RDP listener.
pub fn rdp_target(config: &Config) -> String {
    format!("127.0.0.1:{}", config.rdp_port)
}

/// `ws://`/`wss://` URL of the WebSocket endpoint for `host`.
pub fn ws_url(https: bool, host: &str, port: u16) -> String {
    let scheme = if https { "wss" } else { "ws" };
    let host = if host == "0.0.0.0" || host == "::" {
        "localhost"
    } else {
        host
    };
    format!("{scheme}://{host}:{port}/ws")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn args_parse_defaults_and_overrides() {
        let args = Args::parse(argv(&[])).unwrap();
        assert_eq!(args.config, PathBuf::from("/etc/uflinux/rds.json"));
        assert_eq!(args.port, None);
        assert!(!args.plain_http);

        let args = Args::parse(argv(&[
            "--config",
            "/x/rds.json",
            "--port",
            "3490",
            "--bind",
            "10.0.0.1",
            "--plain-http",
            "--state-dir",
            "/var/tmp/rds",
        ]))
        .unwrap();
        assert_eq!(args.config, PathBuf::from("/x/rds.json"));
        assert_eq!(args.port, Some(3490));
        assert_eq!(args.bind.as_deref(), Some("10.0.0.1"));
        assert!(args.plain_http);
        assert_eq!(args.state_dir, Some(PathBuf::from("/var/tmp/rds")));

        assert!(Args::parse(argv(&["--port", "nope"])).is_err());
        assert!(Args::parse(argv(&["--nope"])).is_err());
        assert!(Args::parse(argv(&["--config"])).is_err());
        assert!(
            Args::parse(argv(&["--help"]))
                .unwrap_err()
                .contains("USAGE")
        );
    }

    #[test]
    fn resolve_applies_cli_over_a_missing_file() {
        let dir = std::env::temp_dir();
        let args = Args {
            config: dir.join("uflinux-rds-gateway-does-not-exist.json"),
            port: Some(3599),
            bind: Some("127.0.0.1".into()),
            plain_http: true,
            state_dir: Some(dir.clone()),
        };
        let env = BTree { vars: Vec::new() };
        let resolved = resolve(&args, &env).unwrap();
        assert_eq!(resolved.config.web.port, 3599);
        assert_eq!(resolved.config.web.bind, "127.0.0.1");
        assert!(!resolved.config.web.https);
        assert_eq!(resolved.state_dir, dir);
    }

    #[test]
    fn state_dir_prefers_cli_then_env_then_default() {
        let env = |vars: Vec<(&str, &str)>| BTree {
            vars: vars
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        };
        let args = Args::default();
        assert_eq!(
            state_dir(&args, &env(vec![])),
            PathBuf::from(uflinux_rds_common::STATE_DIR)
        );
        assert_eq!(
            state_dir(&args, &env(vec![(ENV_STATE_DIR, "/tmp/alt")])),
            PathBuf::from("/tmp/alt")
        );
        let args = Args {
            state_dir: Some(PathBuf::from("/tmp/cli")),
            ..Args::default()
        };
        assert_eq!(
            state_dir(&args, &env(vec![(ENV_STATE_DIR, "/tmp/alt")])),
            PathBuf::from("/tmp/cli")
        );
    }

    #[test]
    fn ws_url_uses_scheme_and_wildcard_bind() {
        assert_eq!(ws_url(false, "0.0.0.0", 3390), "ws://localhost:3390/ws");
        assert_eq!(
            ws_url(true, "rds.example", 3390),
            "wss://rds.example:3390/ws"
        );
    }

    #[test]
    fn rdp_target_is_the_local_listener() {
        let config = Config {
            rdp_port: 3389,
            ..Config::default()
        };
        assert_eq!(rdp_target(&config), "127.0.0.1:3389");
        let config = Config {
            rdp_port: 3399,
            ..Config::default()
        };
        assert_eq!(rdp_target(&config), "127.0.0.1:3399");
    }
}
