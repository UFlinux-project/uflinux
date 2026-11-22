//! `/etc/uflinux/rds.json` schema, defaults and loading.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub const DEFAULT_RDP_PORT: u16 = 3389;
pub const DEFAULT_WEB_PORT: u16 = 3390;
pub const DEFAULT_CONFIG_PATH: &str = "/etc/uflinux/rds.json";
pub const DEFAULT_SESSION_COMMAND: &str = "uflinux-session";
pub const DEFAULT_MAX_SESSIONS: usize = 4;
pub const DEFAULT_IDLE_TIMEOUT_SECS: u64 = 900;
pub const DEFAULT_MAX_FPS: u32 = 30;

/// Everything the Remote Desktop tab can edit. Missing keys keep their default,
/// so a partially written file is always valid.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Master switch. `false` means the broker holds no listener at all.
    pub enabled: bool,
    pub rdp_port: u16,
    /// Bind address for the RDP listener. Loopback keeps RDS local-only.
    pub rdp_bind: String,
    /// Require CredSSP/NLA. Off by default: answering CredSSP needs a copy of
    /// the user's password, which a session host does not have (see
    /// `rds/server/src/main.rs::session_security`). Clients that merely prefer
    /// NLA connect anyway over server-authenticated TLS; clients that require
    /// it are refused until there is a secret store to read a real one from.
    pub nla_required: bool,
    /// Users allowed to open a session. Empty means every local user.
    pub allowed_users: BTreeSet<String>,
    /// Groups allowed to open a session, checked against the user's groups.
    pub allowed_groups: BTreeSet<String>,
    /// Concurrent sessions before new connections are refused.
    pub max_sessions: usize,
    /// Drop a session after this much client inactivity. Zero disables it.
    pub idle_timeout_secs: u64,
    /// Keep the desktop alive this long after a disconnect so a reconnect can
    /// reattach. Zero means the session dies with the connection.
    pub linger_secs: u64,
    /// Session program launched per connection, e.g. `uflinux-session`.
    /// Overridden by `UFLINUX_RDS_SESSION_COMMAND`.
    pub session_command: String,
    pub codec: CodecConfig,
    pub web: WebConfig,
}

/// Codec negotiation policy and encoder tuning.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CodecConfig {
    /// `auto` prefers VA-API, `force` refuses to fall back, `software` keeps
    /// H.264 but never touches the GPU, `off` disables H.264.
    pub h264: H264Policy,
    /// Allow RemoteFX for clients without RemoteApp/EGFX AVC support.
    pub allow_remotefx: bool,
    /// RemoteFX quantization, 0 (best quality) to 63 (smallest).
    pub rfx_quant: u8,
    /// Cap on frames per second per session; protects the host and the link.
    pub max_fps: u32,
    /// Preferred VA-API render node, e.g. `/dev/dri/renderD128`. Probed when unset.
    pub vaapi_device: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum H264Policy {
    /// Use hardware when a VA-API driver loads, otherwise fall back.
    #[default]
    Auto,
    /// Require H.264; refuse the connection when no encoder is usable.
    Force,
    /// H.264 is allowed but the GPU stays out of it: software encode only.
    Software,
    /// Never negotiate H.264.
    Off,
}

impl H264Policy {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "force" | "on" | "1" | "true" => Some(Self::Force),
            "auto" => Some(Self::Auto),
            "software" | "soft" => Some(Self::Software),
            "off" | "0" | "false" => Some(Self::Off),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Force => "force",
            Self::Software => "software",
            Self::Off => "off",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WebConfig {
    pub enabled: bool,
    pub port: u16,
    pub bind: String,
    /// `false` serves plain HTTP.
    pub https: bool,
    pub certificate: CertificateSource,
    /// Minutes a one-time web login token stays valid.
    pub token_ttl_minutes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(tag = "source", rename_all = "snake_case", deny_unknown_fields)]
pub enum CertificateSource {
    /// Generated on demand into `/var/lib/uflinux-rds/`.
    #[default]
    SelfSigned,
    /// Administrator supplied PEM material already on disk.
    Custom {
        certificate: PathBuf,
        private_key: PathBuf,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        chain: Option<PathBuf>,
    },
}

impl Default for Config {
    fn default() -> Self {
        Self {
            enabled: false,
            rdp_port: DEFAULT_RDP_PORT,
            rdp_bind: "0.0.0.0".to_owned(),
            nla_required: false,
            allowed_users: BTreeSet::new(),
            allowed_groups: BTreeSet::new(),
            max_sessions: DEFAULT_MAX_SESSIONS,
            idle_timeout_secs: DEFAULT_IDLE_TIMEOUT_SECS,
            linger_secs: 0,
            session_command: DEFAULT_SESSION_COMMAND.to_owned(),
            codec: CodecConfig::default(),
            web: WebConfig::default(),
        }
    }
}

impl Default for CodecConfig {
    fn default() -> Self {
        Self {
            h264: H264Policy::default(),
            allow_remotefx: true,
            rfx_quant: 20,
            max_fps: DEFAULT_MAX_FPS,
            vaapi_device: None,
        }
    }
}

impl Default for WebConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            port: DEFAULT_WEB_PORT,
            bind: "0.0.0.0".to_owned(),
            https: true,
            certificate: CertificateSource::default(),
            token_ttl_minutes: 5,
        }
    }
}

impl Config {
    /// Read the on-disk configuration, or defaults when the file is absent.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| ConfigError::Parse {
                path: path.to_path_buf(),
                message: e.to_string(),
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(ConfigError::Io {
                path: path.to_path_buf(),
                message: e.to_string(),
            }),
        }
    }

    /// Atomically replace `path` with this configuration.
    pub fn save(&self, path: &Path) -> Result<(), ConfigError> {
        let bytes = serde_json::to_vec_pretty(self).map_err(|e| ConfigError::Parse {
            path: path.to_path_buf(),
            message: e.to_string(),
        })?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, bytes).map_err(|e| ConfigError::Io {
            path: tmp.clone(),
            message: e.to_string(),
        })?;
        std::fs::rename(&tmp, path).map_err(|e| ConfigError::Io {
            path: path.to_path_buf(),
            message: e.to_string(),
        })
    }

    /// Apply `UFLINUX_RDS_*` overrides from `/etc/conf.d/uflinux-rds`.
    pub fn apply_env_overrides(&mut self, env: impl IntoIterator<Item = (String, String)>) {
        for (key, value) in env {
            match key.as_str() {
                "UFLINUX_RDS_H264" => {
                    if let Some(policy) = H264Policy::parse(&value) {
                        self.codec.h264 = policy;
                    }
                }
                "UFLINUX_RDS_SESSION_COMMAND" => {
                    if !value.trim().is_empty() {
                        self.session_command = value;
                    }
                }
                "UFLINUX_RDS_RDP_PORT" => {
                    if let Ok(port) = value.trim().parse() {
                        self.rdp_port = port;
                    }
                }
                "UFLINUX_RDS_WEB_PORT" => {
                    if let Ok(port) = value.trim().parse() {
                        self.web.port = port;
                    }
                }
                _ => {}
            }
        }
    }

    /// Reject values that would make the service unusable at startup.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.rdp_port == 0 {
            return Err(ConfigError::Invalid("rdp_port must not be 0".to_owned()));
        }
        if self.web.enabled && self.web.port == 0 {
            return Err(ConfigError::Invalid("web.port must not be 0".to_owned()));
        }
        if self.enabled && self.web.enabled && self.web.https && self.rdp_port == self.web.port {
            return Err(ConfigError::Invalid(
                "rdp_port and web.port must differ".to_owned(),
            ));
        }
        if self.codec.rfx_quant > 63 {
            return Err(ConfigError::Invalid(
                "codec.rfx_quant must be 0..=63".to_owned(),
            ));
        }
        if self.codec.max_fps == 0 || self.codec.max_fps > 240 {
            return Err(ConfigError::Invalid(
                "codec.max_fps must be 1..=240".to_owned(),
            ));
        }
        if self.max_sessions == 0 {
            return Err(ConfigError::Invalid(
                "max_sessions must be at least 1".to_owned(),
            ));
        }
        let tls_active = self.enabled && self.web.enabled && self.web.https;
        if let CertificateSource::Custom {
            certificate,
            private_key,
            ..
        } = &self.web.certificate
            && tls_active
        {
            for path in [certificate.as_path(), private_key.as_path()] {
                if !path.is_absolute() {
                    return Err(ConfigError::Invalid(format!(
                        "{} must be an absolute path",
                        path.display()
                    )));
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read {path}: {message}")]
    Io { path: PathBuf, message: String },
    #[error("cannot parse {path}: {message}")]
    Parse { path: PathBuf, message: String },
    #[error("invalid configuration: {0}")]
    Invalid(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_disabled_with_standard_ports() {
        let config = Config::default();
        assert!(!config.enabled);
        assert_eq!(config.rdp_port, 3389);
        assert_eq!(config.web.port, 3390);
        assert!(config.web.https);
        assert_eq!(config.web.certificate, CertificateSource::SelfSigned);
        assert!(config.validate().is_ok());
    }

    /// The file the package installs must parse, and must say the same thing as
    /// `Config::default()`, otherwise a fresh install behaves differently from a
    /// host with no config file at all.
    #[test]
    fn shipped_default_file_matches_builtin_defaults() {
        let path = std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../data/rds.json"));
        let config = Config::load(path).expect("data/rds.json must parse");
        assert_eq!(config, Config::default());
        assert!(config.validate().is_ok());
    }

    #[test]
    fn missing_keys_fall_back_to_defaults() {
        let config: Config = serde_json::from_str(r#"{"rdp_port": 3489}"#).unwrap();
        assert_eq!(config.rdp_port, 3489);
        assert!(!config.enabled);
        assert_eq!(config.web.port, 3390);
    }

    #[test]
    fn unknown_keys_are_rejected() {
        let err = serde_json::from_str::<Config>(r#"{"nope": 1}"#).unwrap_err();
        assert!(err.to_string().contains("nope"));
    }

    #[test]
    fn custom_certificate_round_trips() {
        let config = Config {
            enabled: true,
            web: WebConfig {
                certificate: CertificateSource::Custom {
                    certificate: "/etc/uflinux-rds/tls/cert.pem".into(),
                    private_key: "/etc/uflinux-rds/tls/key.pem".into(),
                    chain: None,
                },
                ..WebConfig::default()
            },
            ..Config::default()
        };
        let json = serde_json::to_string(&config).unwrap();
        let back: Config = serde_json::from_str(&json).unwrap();
        assert_eq!(config, back);
        assert!(back.validate().is_ok());
    }

    #[test]
    fn relative_custom_certificate_paths_are_rejected() {
        let config = Config {
            enabled: true,
            web: WebConfig {
                certificate: CertificateSource::Custom {
                    certificate: "cert.pem".into(),
                    private_key: "/etc/uflinux-rds/tls/key.pem".into(),
                    chain: None,
                },
                ..WebConfig::default()
            },
            ..Config::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn same_port_for_rdp_and_web_is_rejected() {
        let config = Config {
            enabled: true,
            rdp_port: 3390,
            ..Config::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn environment_overrides_win() {
        let mut config = Config::default();
        config.apply_env_overrides(Vec::new());
        let mut overrides = vec![("UFLINUX_RDS_H264".to_owned(), "force".to_owned())];
        config.apply_env_overrides(overrides.drain(..));
        assert_eq!(config.codec.h264, H264Policy::Force);
    }

    #[test]
    fn h264_policy_parses_conf_d_values() {
        assert_eq!(H264Policy::parse("force"), Some(H264Policy::Force));
        assert_eq!(H264Policy::parse(" OFF "), Some(H264Policy::Off));
        assert_eq!(H264Policy::parse("auto"), Some(H264Policy::Auto));
        assert_eq!(H264Policy::parse("maybe"), None);
    }
}
