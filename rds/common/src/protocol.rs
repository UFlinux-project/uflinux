//! UFLinux RDS broker control protocol.
//!
//! The broker (`uflinux-rdsd`) listens on `$XDG_RUNTIME_DIR/uflinux-rds.sock`
//! and speaks one JSON object per line in each direction. The shape mirrors the
//! Settings daemon's D-Bus verbs so the UI can treat both the same way.

use serde::{Deserialize, Serialize};

/// Request sent by a client, then answered with exactly one [`Reply`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    /// Configuration, listener state and the session roster.
    GetState,
    /// Applies configuration and reconciles the running services.
    Execute { action: SystemAction },
    /// Live roster only; cheaper than [`Request::GetState`].
    ListSessions,
    /// Terminate one session by its roster id.
    Disconnect { session: u32 },
}

/// Privileged operations the broker performs on behalf of the caller.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SystemAction {
    /// Enable or disable the service (adds or removes the OpenRC default entry).
    SetEnabled { enabled: bool },
    /// Start or stop the broker without changing persistence.
    SetRunning { running: bool },
    /// Rewrite `/etc/uflinux/rds.json` from the supplied configuration.
    /// Boxed so the common actions stay small.
    ApplyConfig { config: Box<crate::Config> },
}

/// One answer to one [`Request`]. Exactly one payload field is set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Reply {
    Ok {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        state: Option<Box<BrokerState>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sessions: Option<Vec<SessionInfo>>,
    },
    Error {
        message: String,
    },
}

/// Everything the Remote Desktop tab renders in one round trip.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrokerState {
    pub config: crate::Config,
    /// Effective configuration after defaults and environment overrides.
    pub effective: crate::Config,
    pub enabled: bool,
    pub running: bool,
    /// True when the RDP listener is bound.
    pub rdp_listening: bool,
    pub web_listening: bool,
    pub sessions: Vec<SessionInfo>,
    /// Human readable reason the listener is not bound, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fault: Option<String>,
}

/// A live or lingering session, as shown in the roster.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: u32,
    pub username: String,
    /// `"rdp"` for a direct client, `"web"` for a gateway connection.
    pub transport: String,
    /// Negotiated codec tier, e.g. `"h264-vaapi"`, `"rfx"`, `"qoiz"`.
    pub codec: String,
    /// Codec device when hardware accelerated, e.g. `"Intel iHD"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codec_device: Option<String>,
    pub width: u16,
    pub height: u16,
    /// Seconds since the session was established.
    pub connected_seconds: u64,
    /// Seconds since the last client input event.
    pub idle_seconds: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_use_the_documented_tags() {
        assert_eq!(
            serde_json::to_string(&Request::GetState).unwrap(),
            r#"{"op":"get_state"}"#
        );
        assert_eq!(
            serde_json::to_string(&Request::Disconnect { session: 7 }).unwrap(),
            r#"{"op":"disconnect","session":7}"#
        );
        assert_eq!(
            serde_json::to_string(&Request::Execute {
                action: SystemAction::SetRunning { running: true }
            })
            .unwrap(),
            r#"{"op":"execute","action":{"kind":"set_running","running":true}}"#
        );
    }

    #[test]
    fn an_error_reply_parses() {
        let reply: Reply = serde_json::from_str(r#"{"status":"error","message":"nope"}"#).unwrap();
        assert_eq!(
            reply,
            Reply::Error {
                message: "nope".to_owned()
            }
        );
    }

    #[test]
    fn an_ok_reply_without_payload_parses() {
        let reply: Reply = serde_json::from_str(r#"{"status":"ok"}"#).unwrap();
        match reply {
            Reply::Ok { state, sessions } => {
                assert!(state.is_none());
                assert!(sessions.is_none());
            }
            Reply::Error { message } => panic!("unexpected error: {message}"),
        }
    }
}
