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
    /// Legacy NLA switch. Validation rejects true because this PAM service
    /// accepts native RDP over server-authenticated TLS.
    pub nla_required: bool,
    /// Users allowed to open a session. Empty means every local user.
    pub allowed_users: BTreeSet<String>,
    /// Groups allowed to open a session, checked against the user's groups.
    pub allowed_groups: BTreeSet<String>,
    /// Concurrent sessions before new connections are refused.
    pub max_sessions: usize,
    /// Drop a session after this much client inactivity. Zero disables it.
    pub idle_timeout_secs: u64,
    /// Accepted and ignored. Sessions used to hold the desktop open for this
    /// long after a disconnect; the desktop now dies with its connection and a
    /// reconnect gets a fresh session host. The field keeps configs written by
    /// older installs loading unchanged — `Config` rejects unknown keys — and
    /// is never written back out.
    #[serde(default, skip_serializing)]
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

    /// Rewrites the `enabled` key of the on-disk configuration in place.
    ///
    /// This is what makes the master switch survive a restart: the OpenRC
    /// runlevel decides whether the broker starts, but the broker only ever
    /// reads its state from this file, so a runlevel entry without
    /// `"enabled": true` leaves it holding no listener. Everything else in the
    /// file is preserved verbatim — see [`patch_enabled`].
    pub fn set_enabled_on_disk(path: &Path, enabled: bool) -> Result<(), ConfigError> {
        patch_enabled(path, enabled)
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
        if self.nla_required {
            return Err(ConfigError::Invalid(
                "nla_required is unsupported for PAM accounts. Set it to false to use RDP over TLS.".to_owned(),
            ));
        }
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

/// Rewrites one key of a JSON configuration file, keeping every other byte.
///
/// The file is read as free-form JSON rather than as [`Config`] on purpose:
/// `Config` rejects unknown keys, and a configuration written by a newer build
/// must not lose those keys just because an older helper is patching the
/// switch. A missing file starts from an empty object, which is what
/// [`Config::load`] treats as "all defaults".
pub fn patch_enabled(path: &Path, enabled: bool) -> Result<(), ConfigError> {
    use std::os::unix::fs::PermissionsExt;

    let mut value = match std::fs::read(path) {
        Ok(bytes) if bytes.iter().any(|byte| !byte.is_ascii_whitespace()) => {
            serde_json::from_slice::<serde_json::Value>(&bytes).map_err(|e| ConfigError::Parse {
                path: path.to_path_buf(),
                message: e.to_string(),
            })?
        }
        Ok(_) => serde_json::Value::Object(serde_json::Map::new()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            serde_json::Value::Object(serde_json::Map::new())
        }
        Err(e) => {
            return Err(ConfigError::Io {
                path: path.to_path_buf(),
                message: e.to_string(),
            });
        }
    };
    let object = value
        .as_object_mut()
        .ok_or_else(|| ConfigError::Invalid(format!("{} is not a JSON object", path.display())))?;
    object.insert("enabled".to_owned(), serde_json::Value::Bool(enabled));
    let bytes = serde_json::to_vec_pretty(&value).map_err(|e| ConfigError::Parse {
        path: path.to_path_buf(),
        message: e.to_string(),
    })?;

    // Same tmp-then-rename discipline as `Config::save`, so a reader never sees
    // a half-written file, plus an explicit mode because the file is world
    // readable and a restrictive umask would otherwise make it root-only.
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, bytes).map_err(|e| ConfigError::Io {
        path: tmp.clone(),
        message: e.to_string(),
    })?;
    let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o644));
    std::fs::rename(&tmp, path).map_err(|e| ConfigError::Io {
        path: path.to_path_buf(),
        message: e.to_string(),
    })
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

    /// An upgraded host keeps the file it already had, and `rds.json` is
    /// installed without `backup=!`, so a key the current build no longer uses
    /// has to keep parsing rather than bricking the service.
    #[test]
    fn a_deprecated_linger_secs_still_loads_and_is_not_written_back() {
        let config: Config =
            serde_json::from_str(r#"{"linger_secs": 120, "max_sessions": 2}"#).unwrap();
        assert_eq!(config.max_sessions, 2);
        // The value is parsed and then ignored: nothing reads the field, so a
        // machine that still says 120 behaves exactly like one that says 0.
        assert_eq!(config.linger_secs, 120);
        let json = serde_json::to_string(&config).unwrap();
        assert!(!json.contains("linger_secs"), "{json}");
    }

    /// Scratch directory for the tests below, following the convention used by
    /// the rest of the workspace (no extra dev-dependency).
    fn scratch_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("uflinux-rds-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A host that never had the file (or has an empty one) still gets a
    /// readable configuration out of a switch flip.
    #[test]
    fn patching_enabled_creates_a_missing_file() {
        let dir = scratch_dir("patch-missing");
        let path = dir.join("rds.json");
        Config::set_enabled_on_disk(&path, true).unwrap();
        let config = Config::load(&path).unwrap();
        assert!(config.enabled);
        assert_eq!(config.rdp_port, DEFAULT_RDP_PORT);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The helper must not be a configuration writer in disguise: a port the
    /// user changed, or a key from a newer build, survives the switch flip even
    /// though `Config` itself would reject it.
    #[test]
    fn patching_enabled_keeps_other_keys() {
        let dir = scratch_dir("patch-keep");
        let path = dir.join("rds.json");
        std::fs::write(
            &path,
            r#"{"rdp_port": 3489, "web": {"port": 3490}, "from_the_future": 7}"#,
        )
        .unwrap();
        Config::set_enabled_on_disk(&path, true).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["enabled"], serde_json::Value::Bool(true));
        assert_eq!(value["rdp_port"], serde_json::json!(3489));
        assert_eq!(value["web"]["port"], serde_json::json!(3490));
        assert_eq!(value["from_the_future"], serde_json::json!(7));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn patching_enabled_round_trips_and_is_idempotent() {
        let dir = scratch_dir("patch-idle");
        let path = dir.join("rds.json");
        Config::set_enabled_on_disk(&path, true).unwrap();
        Config::set_enabled_on_disk(&path, true).unwrap();
        assert!(Config::load(&path).unwrap().enabled);
        Config::set_enabled_on_disk(&path, false).unwrap();
        let config = Config::load(&path).unwrap();
        assert!(!config.enabled);
        assert!(config.validate().is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn patching_enabled_refuses_a_non_object_file() {
        let dir = scratch_dir("patch-broken");
        let path = dir.join("rds.json");
        std::fs::write(&path, b"[1, 2, 3]").unwrap();
        let err = Config::set_enabled_on_disk(&path, true).unwrap_err();
        assert!(err.to_string().contains("not a JSON object"));
        // The broken file is left alone rather than silently replaced.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "[1, 2, 3]");
        let _ = std::fs::remove_dir_all(&dir);
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
