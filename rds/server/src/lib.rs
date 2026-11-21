//! Per-connection UFLinux RDS session host.
//!
//! One process of `uflinux-rdp-session` serves exactly one RDP connection. The
//! broker forks it after `accept()`, handing the already-connected client socket
//! and a status socket down as inherited descriptors; see the `main` binary's
//! module documentation for the exact descriptor contract.
//!
//! The modules are independent of one another except through the types named in
//! [`main`](../../uflinux_rdp_session/index.html):
//!
//! - [`args`] — command line / environment parsing.
//! - [`tls`] — certificate identity and the rustls crypto provider.
//! - [`auth`] — credential checking and user/group admission.
//! - [`caps`] — client capability snapshot and codec-tier decision.
//! - [`display`] — the RDP display handler, EGFX pipeline and frame pump.
//! - [`scancode`] — RDP scancodes to Wayland keysyms.
//! - [`pacing`] — frames-per-second limiting.
//! - [`degrade`] — codec downgrade state machine.
//! - [`status`] — newline-delimited JSON status updates for the broker.
//! - [`linger`] — what to do with the desktop after the client goes away.

pub mod args;
pub mod auth;
pub mod caps;
pub mod degrade;
pub mod display;
pub mod linger;
pub mod pacing;
pub mod scancode;
pub mod status;
pub mod tls;
