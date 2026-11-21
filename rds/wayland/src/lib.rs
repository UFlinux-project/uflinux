//! Headless desktop backend for UFLinux RDS.
//!
//! [`DesktopBackend`] spawns a nested wlroots compositor (`labwc` with
//! `WLR_BACKENDS=headless`, `cage` as a fallback) inside a private
//! `XDG_RUNTIME_DIR`, connects to it as a Wayland client, captures its
//! output with `zwlr_screencopy_manager_v1` (full frames in shared
//! memory, diffed in software into dirty rectangles) and injects input
//! with `zwp_virtual_keyboard_v1` / `zwlr_virtual_pointer_v1`. No
//! `/dev/uinput` and no root privileges are required.
//!
//! ## Threading model
//!
//! All Wayland dispatching happens on a single dedicated background
//! thread created by [`DesktopBackend::spawn`]; that thread exclusively
//! owns the [`wayland_client::Connection`] and its `EventQueue`. The
//! caller never touches Wayland:
//!
//! * Outbound commands (input injection, resize, shutdown) travel to
//!   the event-loop thread over a `std::sync::mpsc` channel and are
//!   executed by it between dispatches.
//! * Completed frames travel back over a `std::sync::mpsc` rendezvous
//!   channel; [`DesktopBackend::next_frame`] blocks on it (with a
//!   caller-supplied timeout) and never blocks on Wayland directly.
//!
//! Dropping the [`DesktopBackend`] shuts the thread down and terminates
//! the child compositor (SIGTERM, then SIGKILL after a grace period).

mod compositor;
mod error;
mod frame;
mod input;
mod session;
mod spec;

pub use error::BackendError;
pub use frame::{DirtyRect, Frame, dirty_regions};
pub use input::{
    KeyState, KeyboardEvent, KeyboardState, ModifiersChanged, PointerEvent, button, keycode,
    keysym, keysym_to_keycode, modifier,
};
pub use session::DesktopBackend;
pub use spec::DesktopSpec;
