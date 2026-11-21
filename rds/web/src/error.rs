//! Mapping from `IronErrorKind` discriminants to short, human-readable strings
//! suitable for the console UI.
//!
//! The discriminants mirror the `#[wasm_bindgen]` enum in `iron-remote-desktop`
//! (`General = 0 .. NegotiationFailure = 6`). We keep the mapping as a plain
//! integer function so it stays unit-testable on the host target, where the
//! wasm-bindgen enum itself is not directly usable.

/// General / catch-all.
pub const KIND_GENERAL: i32 = 0;
/// Incorrect password used.
pub const KIND_WRONG_PASSWORD: i32 = 1;
/// Unable to log in to the machine.
pub const KIND_LOGON_FAILURE: i32 = 2;
/// Insufficient permission; the server denied access.
pub const KIND_ACCESS_DENIED: i32 = 3;
/// The RDCleanPath message failed.
pub const KIND_RDCLEANPATH: i32 = 4;
/// Could not connect to the proxy (gateway).
pub const KIND_PROXY_CONNECT: i32 = 5;
/// Protocol negotiation failed.
pub const KIND_NEGOTIATION_FAILURE: i32 = 6;

/// Short, user-facing message for an [`IronErrorKind`] discriminant.
///
/// [`IronErrorKind`]: iron_remote_desktop::IronErrorKind
#[must_use]
pub fn error_message(kind: i32) -> &'static str {
    match kind {
        KIND_WRONG_PASSWORD => "Wrong username or password",
        KIND_LOGON_FAILURE => "The server refused the logon",
        KIND_ACCESS_DENIED => "Access denied by the server",
        KIND_RDCLEANPATH => "Gateway handshake failed",
        KIND_PROXY_CONNECT => "Could not reach the gateway",
        KIND_NEGOTIATION_FAILURE => "Protocol negotiation failed",
        _ => "Connection error",
    }
}

/// Stable, lowercase identifier for an error kind (used as a CSS/telemetry tag).
#[must_use]
pub fn error_kind_name(kind: i32) -> &'static str {
    match kind {
        KIND_WRONG_PASSWORD => "wrong_password",
        KIND_LOGON_FAILURE => "logon_failure",
        KIND_ACCESS_DENIED => "access_denied",
        KIND_RDCLEANPATH => "rdcleanpath",
        KIND_PROXY_CONNECT => "proxy_connect",
        KIND_NEGOTIATION_FAILURE => "negotiation_failure",
        _ => "general",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrong_password_maps_to_expected_copy() {
        assert_eq!(
            error_message(KIND_WRONG_PASSWORD),
            "Wrong username or password"
        );
    }

    #[test]
    fn every_known_kind_has_a_message() {
        for kind in KIND_GENERAL..=KIND_NEGOTIATION_FAILURE {
            assert!(
                !error_message(kind).is_empty(),
                "kind {kind} has no message"
            );
            assert!(!error_kind_name(kind).is_empty(), "kind {kind} has no name");
        }
    }

    #[test]
    fn unknown_kind_falls_back_to_general() {
        assert_eq!(error_message(99), "Connection error");
        assert_eq!(error_kind_name(99), "general");
    }
}
