//! Privileged D-Bus helper for the UFLinux Remote Desktop service.
//!
//! The unprivileged per-user Settings daemon talks to this root helper over
//! the system bus to toggle the OpenRC-managed broker and to manage its TLS
//! material. It mirrors `uflinux-settings-system` one-for-one: a single
//! `Execute(s)` method carrying a JSON operation, polkit authorization
//! against the sender, and a 45 second idle exit unless `--resident`.

mod bus;
mod certificate;
mod operations;

pub type Result<T> = std::result::Result<T, String>;

pub const SYSTEM_NAME: &str = "org.uflinux.Rds.System1";
pub const SYSTEM_PATH: &str = "/org/uflinux/Rds/System1";
pub const MAX_REQUEST: usize = 64 * 1024;

/// Operations accepted on `org.uflinux.Rds.System1.Execute`.
///
/// The serde tags are part of the wire contract with the Settings daemon;
/// renaming a variant is an ABI break.
#[derive(Debug, serde::Deserialize)]
#[serde(tag = "operation", rename_all = "kebab-case", deny_unknown_fields)]
pub enum RdpSystemOperation {
    /// `rc-update add` plus an immediate start.
    Enable,
    /// Best-effort stop, then `rc-update del`.
    Disable,
    Start,
    Stop,
    /// Install a custom PEM pair, or regenerate the self-signed pair when
    /// `mode` is `"self-signed"`.
    ApplyCertificate {
        mode: String,
        #[serde(default)]
        certificate_path: Option<String>,
        #[serde(default)]
        key_path: Option<String>,
    },
    /// Read-only status, no polkit check.
    Status,
}

fn main() {
    if let Err(error) = operations::run() {
        eprintln!("uflinux-rds-system: {error}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::RdpSystemOperation;

    fn parse(raw: &str) -> RdpSystemOperation {
        serde_json::from_str(raw).expect("operation should parse")
    }

    #[test]
    fn parses_every_variant() {
        assert!(matches!(
            parse(r#"{"operation":"enable"}"#),
            RdpSystemOperation::Enable
        ));
        assert!(matches!(
            parse(r#"{"operation":"disable"}"#),
            RdpSystemOperation::Disable
        ));
        assert!(matches!(
            parse(r#"{"operation":"start"}"#),
            RdpSystemOperation::Start
        ));
        assert!(matches!(
            parse(r#"{"operation":"stop"}"#),
            RdpSystemOperation::Stop
        ));
        assert!(matches!(
            parse(r#"{"operation":"status"}"#),
            RdpSystemOperation::Status
        ));
        let op = parse(r#"{"operation":"apply-certificate","mode":"self-signed"}"#);
        match op {
            RdpSystemOperation::ApplyCertificate {
                mode,
                certificate_path,
                key_path,
            } => {
                assert_eq!(mode, "self-signed");
                assert!(certificate_path.is_none());
                assert!(key_path.is_none());
            }
            other => panic!("unexpected variant {other:?}"),
        }
        let op = parse(
            r#"{"operation":"apply-certificate","mode":"custom","certificate_path":"/tmp/c.pem","key_path":"/tmp/k.pem"}"#,
        );
        match op {
            RdpSystemOperation::ApplyCertificate {
                mode,
                certificate_path,
                key_path,
            } => {
                assert_eq!(mode, "custom");
                assert_eq!(certificate_path.as_deref(), Some("/tmp/c.pem"));
                assert_eq!(key_path.as_deref(), Some("/tmp/k.pem"));
            }
            other => panic!("unexpected variant {other:?}"),
        }
    }

    #[test]
    fn rejects_unknown_operations_and_extra_fields() {
        assert!(serde_json::from_str::<RdpSystemOperation>(r#"{"operation":"reboot"}"#).is_err());
        assert!(
            serde_json::from_str::<RdpSystemOperation>(r#"{"operation":"apply-certificate"}"#)
                .is_err()
        );
        assert!(
            serde_json::from_str::<RdpSystemOperation>(
                r#"{"operation":"apply-certificate","mode":"custom","certificate_path":"/x"}"#
            )
            .is_ok()
        );
    }
}
