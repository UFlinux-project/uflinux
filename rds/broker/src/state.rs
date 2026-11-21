//! Shared broker state: the session roster and the [`BrokerState`] snapshot.
//!
//! One [`State`] value lives for the lifetime of the process and is shared by
//! every task (accept loop, control handlers, session reapers, web supervisor,
//! idle timer). It is deliberately a plain mutex around small data: the
//! critical sections are a handful of `Vec` operations, so a lock costs less
//! than a message round-trip to a dedicated task, and it keeps the roster
//! consistent with the config it is interpreted against.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use tokio::sync::watch;

use uflinux_rds_common::{BrokerState, Config, SessionInfo};

use crate::session::SessionUpdate;

/// One row of the roster: a child the broker started and has not reaped yet.
#[derive(Debug, Clone)]
pub(crate) struct Entry {
    /// Roster id, also the id the child is told about and the id clients use
    /// with `disconnect`.
    pub(crate) id: u32,
    /// Process id of the child, used for `disconnect`.
    pub(crate) pid: u32,
    /// Latest known facts, merged from the child's status frames.
    pub(crate) username: String,
    /// `"rdp"` or `"web"`.
    pub(crate) transport: String,
    /// Codec label, e.g. `"h264-vaapi"`.
    pub(crate) codec: String,
    /// Codec device detail shown by the Settings UI.
    pub(crate) codec_device: Option<String>,
    /// Desktop size.
    pub(crate) width: u16,
    /// Desktop size.
    pub(crate) height: u16,
    /// When the socket was accepted. The broker owns this clock so a child
    /// that never reports anything still ages correctly.
    pub(crate) started: Instant,
    /// When the child last claimed activity, if it ever did.
    pub(crate) last_activity: Option<Instant>,
}

impl Entry {
    /// A freshly accepted session before any child has reported anything.
    fn new(id: u32, pid: u32, started: Instant) -> Self {
        Self {
            id,
            pid,
            username: String::new(),
            transport: "rdp".to_owned(),
            codec: "unknown".to_owned(),
            codec_device: None,
            width: 0,
            height: 0,
            started,
            last_activity: None,
        }
    }

    /// Fold one status frame from the child into the row.
    ///
    /// Only fields the child actually sent are touched, so a partial frame is
    /// fine and later frames refine earlier ones.
    fn merge(&mut self, update: &SessionUpdate) {
        if let Some(username) = update.username.as_deref() {
            self.username = username.to_owned();
        }
        if let Some(transport) = update.transport.as_deref() {
            self.transport = transport.to_owned();
        }
        if let Some(codec) = update.codec.as_deref() {
            self.codec = codec.to_owned();
        }
        // Double option: the outer one says "the child mentioned the device",
        // the inner one says "it has none".
        if let Some(device) = &update.codec_device {
            self.codec_device = device.clone();
        }
        if let Some(width) = update.width {
            self.width = width;
        }
        if let Some(height) = update.height {
            self.height = height;
        }
        if update.idle == Some(false) || update.activity == Some(true) {
            self.last_activity = Some(Instant::now());
        }
    }

    /// The row as the wire format wants it, with both ages stamped by the
    /// broker rather than trusted from the child.
    fn to_info(&self, now: Instant) -> SessionInfo {
        SessionInfo {
            id: self.id,
            username: self.username.clone(),
            transport: self.transport.clone(),
            codec: self.codec.clone(),
            codec_device: self.codec_device.clone(),
            width: self.width,
            height: self.height,
            connected_seconds: elapsed_secs(now.saturating_duration_since(self.started)),
            idle_seconds: match self.last_activity {
                Some(at) => elapsed_secs(now.saturating_duration_since(at)),
                None => elapsed_secs(now.saturating_duration_since(self.started)),
            },
        }
    }
}

/// Whole-process state, shared behind an `Arc`.
#[derive(Debug)]
pub(crate) struct State {
    inner: Mutex<Shared>,
    /// Set by the signal handler; read by the accept loop and the idle timer.
    shutting_down: AtomicBool,
    /// Control clients currently attached. Two clients talking at once must
    /// hold off an idle quit.
    control_clients: AtomicUsize,
    /// Bumped whenever the running configuration changes or the broker starts
    /// shutting down, so the supervisor tasks re-read their goals at once
    /// instead of waiting for their poll tick.
    wake: watch::Sender<u64>,
}

#[derive(Debug)]
struct Shared {
    config: Config,
    roster: Vec<Entry>,
    next_id: u32,
    rdp_listening: bool,
    /// Whether the listeners are meant to be up. Distinct from
    /// `config.enabled`: `enabled` is "installed in the default runlevel",
    /// `running` is "started", mirroring the two OpenRC states the Settings UI
    /// shows.
    running: bool,
    web_listening: bool,
    web_running: bool,
    fault: Option<String>,
    /// Last moment the broker had a reason to stay alive (control client,
    /// live session, web child serving).
    last_busy: Instant,
    /// Paths and binaries that `execute`/reconcile need.
    config_path: std::path::PathBuf,
    allowed_users: BTreeSet<String>,
    allowed_groups: BTreeSet<String>,
}

impl State {
    /// Fresh state for a loaded config.
    pub(crate) fn new(config: Config, config_path: &Path) -> Self {
        let allowed_users = config.allowed_users.clone();
        let allowed_groups = config.allowed_groups.clone();
        Self {
            inner: Mutex::new(Shared {
                roster: Vec::new(),
                next_id: 1,
                rdp_listening: false,
                running: config.enabled,
                web_listening: false,
                web_running: false,
                fault: None,
                last_busy: Instant::now(),
                config,
                config_path: config_path.to_path_buf(),
                allowed_users,
                allowed_groups,
            }),
            shutting_down: AtomicBool::new(false),
            control_clients: AtomicUsize::new(0),
            wake: watch::channel(0).0,
        }
    }

    /// A receiver for the "re-read your goals" signal.
    pub(crate) fn wake(&self) -> watch::Receiver<u64> {
        self.wake.subscribe()
    }

    /// Tell the supervisor tasks something changed.
    pub(crate) fn notify_change(&self) {
        self.wake.send_modify(|generation| *generation += 1);
    }

    /// Begin shutdown. Idempotent.
    pub(crate) fn begin_shutdown(&self) {
        self.shutting_down.store(true, Ordering::Relaxed);
        self.notify_change();
    }

    /// Resolves once [`Self::begin_shutdown`] has been called.
    pub(crate) async fn wait_shutdown(&self) {
        let mut generation = self.wake.subscribe();
        loop {
            if self.shutting_down() {
                return;
            }
            if generation.changed().await.is_err() {
                return;
            }
        }
    }

    /// Handle `set_enabled`. Enabling also asks for the listeners, disabling
    /// drops them, which is what a user means by the two switch positions.
    pub(crate) fn set_enabled(&self, enabled: bool) {
        let mut shared = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        shared.config.enabled = enabled;
        shared.running = enabled;
        if enabled {
            shared.last_busy = Instant::now();
        }
    }

    /// Control clients currently attached.
    pub(crate) fn control_clients(&self) -> usize {
        self.control_clients.load(Ordering::Relaxed)
    }

    /// Whether shutdown has been requested.
    pub(crate) fn shutting_down(&self) -> bool {
        self.shutting_down.load(Ordering::Relaxed)
    }

    /// Snapshot of the running configuration.
    pub(crate) fn config(&self) -> Config {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .config
            .clone()
    }

    /// Swap in a reloaded configuration and refresh the admission roster.
    pub(crate) fn replace_config(&self, config: Config) {
        let mut shared = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        shared.allowed_users = config.allowed_users.clone();
        shared.allowed_groups = config.allowed_groups.clone();
        shared.config = config;
    }

    /// The file `apply_config` writes to.
    pub(crate) fn config_path(&self) -> std::path::PathBuf {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .config_path
            .clone()
    }

    /// Record why the service is degraded, or clear the previous reason.
    pub(crate) fn set_fault(&self, fault: Option<String>) {
        let mut shared = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        shared.fault = fault;
    }

    /// Publish the RDP listener's state after bind/unbind.
    pub(crate) fn set_rdp_listening(&self, listening: bool) {
        let mut shared = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        shared.rdp_listening = listening;
        if listening {
            shared.last_busy = Instant::now();
        }
    }

    /// Whether the listeners are meant to be up right now.
    pub(crate) fn desired_running(&self) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .running
    }

    /// Handle `set_running`. A disabled service cannot be started in-process,
    /// so the effective value is reported back to the caller.
    pub(crate) fn set_desired_running(&self, running: bool) -> bool {
        let mut shared = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        shared.running = running && shared.config.enabled;
        if shared.running {
            shared.last_busy = Instant::now();
        }
        shared.running
    }

    /// Publish whether the web child process is alive.
    pub(crate) fn set_web_running(&self, running: bool) {
        let mut shared = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        shared.web_running = running;
    }

    /// Publish whether the web child is answering on its port.
    pub(crate) fn set_web_listening(&self, listening: bool) {
        let mut shared = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        shared.web_listening = listening;
        if listening {
            shared.last_busy = Instant::now();
        }
    }

    /// Whether the web child is alive.
    pub(crate) fn web_running(&self) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .web_running
    }

    /// Count a control client as attached for the duration of its request.
    pub(crate) fn control_client_attached(&self) -> ClientGuard<'_> {
        self.control_clients.fetch_add(1, Ordering::Relaxed);
        ClientGuard { state: self }
    }

    /// Add a freshly spawned child and return its roster id.
    pub(crate) fn add_session(&self, pid: u32) -> u32 {
        let mut shared = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let id = shared.next_id;
        shared.next_id = shared.next_id.wrapping_add(1).max(1);
        shared.roster.push(Entry::new(id, pid, Instant::now()));
        shared.last_busy = Instant::now();
        id
    }

    /// Fold a status frame into the named row, ignoring unknown ids.
    pub(crate) fn apply_update(&self, id: u32, update: &SessionUpdate) {
        let mut shared = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(entry) = shared.roster.iter_mut().find(|entry| entry.id == id) {
            entry.merge(update);
        }
    }

    /// Drop a row once its child has been reaped.
    pub(crate) fn remove_session(&self, id: u32) {
        let mut shared = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        shared.roster.retain(|entry| entry.id != id);
    }

    /// The row for `id`, if it exists.
    pub(crate) fn session_pid(&self, id: u32) -> Option<u32> {
        let shared = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        shared
            .roster
            .iter()
            .find(|entry| entry.id == id)
            .map(|e| e.pid)
    }

    /// Number of live sessions, used for the `max_sessions` gate.
    pub(crate) fn session_count(&self) -> usize {
        let shared = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        shared.roster.len()
    }

    /// Snapshot of the roster in wire form.
    pub(crate) fn sessions(&self) -> Vec<SessionInfo> {
        let shared = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let now = Instant::now();
        shared
            .roster
            .iter()
            .map(|entry| entry.to_info(now))
            .collect()
    }

    /// Note that a control client talked to us, which restarts the idle clock.
    fn note_control_activity(&self) {
        let mut shared = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        shared.last_busy = Instant::now();
    }

    /// The full `get_state` payload.
    pub(crate) fn snapshot(&self) -> BrokerState {
        let shared = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let now = Instant::now();
        BrokerState {
            config: shared.config.clone(),
            effective: shared.config.clone(),
            enabled: shared.config.enabled,
            running: shared.running && shared.rdp_listening,
            rdp_listening: shared.rdp_listening,
            web_listening: shared.web_listening,
            sessions: shared
                .roster
                .iter()
                .map(|entry| entry.to_info(now))
                .collect(),
            fault: shared.fault.clone(),
        }
    }

    /// Seconds since the last sign of life, for the idle-quit decision.
    pub(crate) fn idle_for(&self) -> Duration {
        let shared = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Instant::now().saturating_duration_since(shared.last_busy)
    }
}

/// Held while a control client is attached.
#[must_use = "detaching a control client early lets an idle quit race it"]
pub(crate) struct ClientGuard<'a> {
    state: &'a State,
}

impl Drop for ClientGuard<'_> {
    fn drop(&mut self) {
        self.state.control_clients.fetch_sub(1, Ordering::Relaxed);
        // The idle clock restarts from the moment the last client leaves.
        self.state.note_control_activity();
    }
}

/// Whole seconds in a duration, saturating instead of overflowing.
fn elapsed_secs(d: Duration) -> u64 {
    d.as_secs()
}

/// Pure idle-quit decision, kept apart from the timer so it can be tested.
///
/// `serving` means the broker has something a client could still connect to
/// (see [`is_serving`]). The daemon only quits when it is serving nothing, has
/// no live session, and the idle window has fully elapsed; `resident`
/// (`--resident`, `--idle-quit 0`) turns the check off entirely.
#[must_use]
pub(crate) fn should_idle_quit(
    resident: bool,
    serving: bool,
    sessions: usize,
    clients: usize,
    idle_for: Duration,
    window: Duration,
) -> bool {
    if resident || serving || sessions > 0 || clients > 0 {
        return false;
    }
    idle_for >= window
}

/// Whether the broker currently has a listener a client could reach.
///
/// Under socket activation the listening socket belongs to the socket unit, so
/// quitting here is safe: the next connection starts a fresh broker. Under
/// plain OpenRC there is nothing to restart us, so a live listener counts as
/// work in progress and keeps the daemon resident.
#[must_use]
pub(crate) fn is_serving(
    enabled: bool,
    rdp_listening: bool,
    web_listening: bool,
    socket_activated: bool,
) -> bool {
    if web_listening {
        return true;
    }
    enabled && rdp_listening && !socket_activated
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;
    use std::path::Path;

    fn state_with(enabled: bool) -> State {
        let config = Config {
            enabled,
            ..Config::default()
        };
        State::new(config, Path::new("/nonexistent/rds.json"))
    }

    #[test]
    fn a_fresh_row_starts_blank_and_ages_from_accept() {
        let state = state_with(true);
        let id = state.add_session(4_321);
        assert_eq!(state.session_pid(id), Some(4_321));
        assert_eq!(state.session_count(), 1);
        let rows = state.sessions();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, id);
        assert_eq!(rows[0].transport, "rdp");
        assert_eq!(rows[0].codec, "unknown");
        // A child that never reports anything still ages from the accept.
        assert_eq!(rows[0].connected_seconds, 0);
        assert_eq!(rows[0].idle_seconds, 0);
    }

    #[test]
    fn partial_frames_merge_and_later_frames_refine_earlier_ones() {
        let state = state_with(true);
        let id = state.add_session(1);
        state.apply_update(
            id,
            &SessionUpdate {
                username: Some("panmourovaty".to_owned()),
                codec: Some("h264-vaapi".to_owned()),
                codec_device: Some(Some("/dev/dri/renderD128".to_owned())),
                ..SessionUpdate::default()
            },
        );
        state.apply_update(
            id,
            &SessionUpdate {
                width: Some(1920),
                height: Some(1080),
                ..SessionUpdate::default()
            },
        );
        let rows = state.sessions();
        assert_eq!(rows[0].username, "panmourovaty");
        assert_eq!(rows[0].codec, "h264-vaapi");
        assert_eq!(rows[0].codec_device.as_deref(), Some("/dev/dri/renderD128"));
        assert_eq!((rows[0].width, rows[0].height), (1920, 1080));
    }

    #[test]
    fn a_frame_can_clear_the_codec_device() {
        let state = state_with(true);
        let id = state.add_session(1);
        state.apply_update(
            id,
            &SessionUpdate {
                codec_device: Some(Some("/dev/dri/renderD128".to_owned())),
                ..SessionUpdate::default()
            },
        );
        state.apply_update(
            id,
            &SessionUpdate {
                codec_device: Some(None),
                ..SessionUpdate::default()
            },
        );
        assert_eq!(state.sessions()[0].codec_device, None);
    }

    #[test]
    fn updates_for_unknown_rows_are_ignored() {
        let state = state_with(true);
        state.apply_update(
            999,
            &SessionUpdate {
                username: Some("ghost".to_owned()),
                ..SessionUpdate::default()
            },
        );
        assert_eq!(state.session_count(), 0);
    }

    #[test]
    fn removing_a_row_drops_it_from_the_roster() {
        let state = state_with(true);
        let first = state.add_session(1);
        let second = state.add_session(2);
        assert_eq!(state.session_count(), 2);
        state.remove_session(first);
        assert_eq!(state.session_pid(first), None);
        assert_eq!(state.session_pid(second), Some(2));
        state.remove_session(first); // idempotent
        assert_eq!(state.session_count(), 1);
    }

    #[test]
    fn an_activity_frame_resets_the_idle_age() {
        let started = Instant::now() - Duration::from_secs(30);
        let mut entry = Entry::new(7, 7, started);
        let later = started + Duration::from_secs(25);
        // No activity reported yet: idle age tracks the accept time.
        assert_eq!(entry.to_info(later).idle_seconds, 25);
        entry.last_activity = Some(later);
        let info = entry.to_info(later);
        assert_eq!(info.connected_seconds, 25);
        assert_eq!(info.idle_seconds, 0);
    }

    #[test]
    fn the_snapshot_matches_what_the_settings_daemon_parses() {
        let state = state_with(true);
        state.set_rdp_listening(true);
        let id = state.add_session(1);
        state.apply_update(
            id,
            &SessionUpdate {
                username: Some("panmourovaty".to_owned()),
                codec: Some("h264-vaapi".to_owned()),
                codec_device: Some(Some("Intel iHD".to_owned())),
                width: Some(1920),
                height: Some(1080),
                ..SessionUpdate::default()
            },
        );
        let json = serde_json::to_value(state.snapshot()).unwrap();
        // The Settings daemon prefers `effective` over `config`.
        assert!(json.get("effective").is_some(), "effective must be present");
        assert_eq!(json["enabled"], serde_json::json!(true));
        assert_eq!(json["running"], serde_json::json!(true));
        assert_eq!(json["rdp_listening"], serde_json::json!(true));
        assert_eq!(json["web_listening"], serde_json::json!(false));
        // No fault recorded: the key must be absent, not null, so the
        // daemon's tolerant subset keeps its own default.
        assert!(
            json.get("fault").is_none(),
            "fault must be skipped when none"
        );
        let row = &json["sessions"][0];
        assert_eq!(row["id"], serde_json::json!(id));
        assert_eq!(row["username"], serde_json::json!("panmourovaty"));
        assert_eq!(row["transport"], serde_json::json!("rdp"));
        // "h264-vaapi" is the exact string the daemon maps to the hardware tier.
        assert_eq!(row["codec"], serde_json::json!("h264-vaapi"));
        assert_eq!(row["codec_device"], serde_json::json!("Intel iHD"));
        assert_eq!(row["width"], serde_json::json!(1920));
        assert_eq!(row["height"], serde_json::json!(1080));
        assert!(row.get("connected_seconds").is_some());
        assert!(row.get("idle_seconds").is_some());
    }

    #[test]
    fn a_fault_appears_in_the_snapshot_and_can_be_cleared() {
        let state = state_with(true);
        state.set_fault(Some("address in use".to_owned()));
        let json = serde_json::to_value(state.snapshot()).unwrap();
        assert_eq!(json["fault"], serde_json::json!("address in use"));
        state.set_fault(None);
        let json = serde_json::to_value(state.snapshot()).unwrap();
        assert!(json.get("fault").is_none());
    }

    #[test]
    fn running_is_reported_only_while_the_listener_is_bound() {
        let state = state_with(true);
        assert!(state.set_desired_running(true));
        assert!(!state.snapshot().running, "desired but not bound");
        state.set_rdp_listening(true);
        assert!(state.snapshot().running);
        state.set_rdp_listening(false);
        assert!(!state.snapshot().running);
    }

    #[test]
    fn a_disabled_service_cannot_be_started_in_process() {
        let state = state_with(false);
        assert!(!state.set_desired_running(true), "disabled blocks start");
        assert!(!state.desired_running());
    }

    #[test]
    fn toggling_enabled_also_moves_the_running_flag() {
        let state = state_with(false);
        state.set_enabled(true);
        assert!(state.desired_running());
        assert!(state.snapshot().enabled);
        state.set_enabled(false);
        assert!(!state.desired_running());
    }

    #[test]
    fn a_control_client_holds_off_the_idle_clock() {
        let state = state_with(false);
        assert_eq!(state.control_clients(), 0);
        {
            let guard = state.control_client_attached();
            assert_eq!(state.control_clients(), 1);
            drop(guard);
        }
        assert_eq!(state.control_clients(), 0);
        // Dropping the guard counts as activity, so the broker is not idle yet.
        assert!(state.idle_for() < Duration::from_secs(5));
    }

    #[test]
    fn idle_quit_needs_quiet_service_and_a_full_window() {
        let window = Duration::from_secs(45);
        let quiet =
            |idle: u64| should_idle_quit(false, false, 0, 0, Duration::from_secs(idle), window);
        assert!(!quiet(44), "window not elapsed");
        assert!(quiet(45), "window elapsed");
        assert!(
            !should_idle_quit(true, false, 0, 0, window, window),
            "--resident never quits"
        );
        assert!(
            !should_idle_quit(false, true, 0, 0, window, window),
            "a reachable listener holds the daemon"
        );
        assert!(
            !should_idle_quit(false, false, 1, 0, window, window),
            "a live session holds the daemon"
        );
        assert!(
            !should_idle_quit(false, false, 0, 1, window, window),
            "an attached client holds the daemon"
        );
    }

    #[test]
    fn socket_activation_makes_a_bare_listener_quittable() {
        assert!(
            is_serving(true, true, false, false),
            "openrc listener is busy"
        );
        assert!(
            !is_serving(true, true, false, true),
            "socket unit can restart us"
        );
        assert!(is_serving(false, false, true, true), "web child is busy");
        assert!(!is_serving(false, false, false, false), "nothing to serve");
    }

    #[test]
    fn replacing_the_config_refreshes_the_admission_roster() {
        let state = state_with(true);
        let mut config = Config {
            enabled: true,
            ..Config::default()
        };
        config.allowed_users.insert("panmourovaty".to_owned());
        state.replace_config(config);
        assert!(state.config().allowed_users.contains("panmourovaty"));
    }

    // ------------------------------------------------------------------
    // The real consumer of the control socket is `settings/daemon/src/rdp.rs`.
    // It deliberately does not link this workspace, so it mirrors the wire
    // shapes with its own permissive deserialisers. Those mirrors are copied
    // below verbatim (field names, defaults, `#[serde(default)]`) so a rename
    // in the broker that the daemon would silently fall back over is caught
    // here instead of showing up as a blank Settings page.
    // ------------------------------------------------------------------

    #[derive(Debug, Default, Deserialize)]
    #[serde(default)]
    struct DaemonState {
        config: Option<DaemonConfig>,
        effective: Option<DaemonConfig>,
        enabled: bool,
        running: bool,
        sessions: Vec<DaemonSession>,
        fault: Option<String>,
    }

    #[derive(Debug, Deserialize)]
    #[serde(default)]
    struct DaemonConfig {
        enabled: bool,
        rdp_port: u16,
        max_sessions: usize,
        codec: DaemonCodec,
        web: DaemonWeb,
    }

    impl Default for DaemonConfig {
        fn default() -> Self {
            Self {
                enabled: false,
                rdp_port: 3389,
                max_sessions: 4,
                codec: DaemonCodec::default(),
                web: DaemonWeb::default(),
            }
        }
    }

    #[derive(Debug, Deserialize)]
    #[serde(default)]
    struct DaemonCodec {
        h264: String,
        allow_remotefx: bool,
        vaapi_device: Option<String>,
    }

    impl Default for DaemonCodec {
        fn default() -> Self {
            Self {
                h264: "auto".into(),
                allow_remotefx: true,
                vaapi_device: None,
            }
        }
    }

    #[derive(Debug, Deserialize)]
    #[serde(default)]
    struct DaemonWeb {
        enabled: bool,
        port: u16,
        https: bool,
        certificate: DaemonCertificate,
    }

    impl Default for DaemonWeb {
        fn default() -> Self {
            Self {
                enabled: true,
                port: 3390,
                https: true,
                certificate: DaemonCertificate::default(),
            }
        }
    }

    /// Internally tagged in the broker (`{"source":"custom", ...}`); read flat.
    #[derive(Debug, Deserialize)]
    #[serde(default)]
    struct DaemonCertificate {
        source: String,
        certificate: Option<String>,
        private_key: Option<String>,
    }

    impl Default for DaemonCertificate {
        fn default() -> Self {
            Self {
                source: "self_signed".into(),
                certificate: None,
                private_key: None,
            }
        }
    }

    #[derive(Debug, Default, Deserialize)]
    #[serde(default)]
    struct DaemonSession {
        id: u32,
        username: String,
        transport: String,
        codec: String,
        codec_device: Option<String>,
        connected_seconds: u64,
    }

    /// Mirrors `codec_tier` in the Settings daemon.
    fn daemon_codec_tier(tag: &str) -> &'static str {
        match tag
            .trim()
            .to_ascii_lowercase()
            .replace('_', "-")
            .replace("remotefx", "remote-fx")
            .as_str()
        {
            "h264-vaapi" | "vaapi" => "h264-vaapi",
            "h264" | "h264-software" | "openh264" => "h264-software",
            "rfx" | "remote-fx" => "remote-fx",
            "qoi-z" | "qoiz" => "qoi-z",
            _ => "qoi",
        }
    }

    /// A snapshot with one hardware-accelerated RDP session and one web session.
    fn snapshot_for_the_daemon() -> serde_json::Value {
        use uflinux_rds_common::{CertificateSource, CodecConfig, H264Policy, WebConfig};

        let config = Config {
            enabled: true,
            rdp_port: 3391,
            max_sessions: 8,
            codec: CodecConfig {
                h264: H264Policy::Force,
                allow_remotefx: false,
                vaapi_device: Some("/dev/dri/renderD128".into()),
                ..CodecConfig::default()
            },
            web: WebConfig {
                enabled: true,
                port: 3392,
                https: false,
                certificate: CertificateSource::Custom {
                    certificate: "/etc/tls/c.pem".into(),
                    private_key: "/etc/tls/k.pem".into(),
                    chain: None,
                },
                ..WebConfig::default()
            },
            ..Config::default()
        };
        let state = State::new(config, Path::new("/nonexistent/rds.json"));
        state.set_rdp_listening(true);

        let rdp = state.add_session(1);
        state.apply_update(
            rdp,
            &SessionUpdate {
                username: Some("panmourovaty".to_owned()),
                codec: Some("h264-vaapi".to_owned()),
                codec_device: Some(Some("Intel iHD".to_owned())),
                width: Some(1920),
                height: Some(1080),
                ..SessionUpdate::default()
            },
        );
        let web = state.add_session(2);
        state.apply_update(
            web,
            &SessionUpdate {
                username: Some("root".to_owned()),
                transport: Some("web".to_owned()),
                codec: Some("qoi-z".to_owned()),
                width: Some(800),
                height: Some(600),
                ..SessionUpdate::default()
            },
        );

        serde_json::to_value(state.snapshot()).expect("a snapshot always serialises")
    }

    #[test]
    fn the_snapshot_deserialises_into_the_settings_daemon_mirrors() {
        let json = snapshot_for_the_daemon();

        // Shape, asserted against the literal JSON the daemon reads.
        assert_eq!(json["enabled"], serde_json::json!(true));
        assert_eq!(json["running"], serde_json::json!(true));
        assert_eq!(json["rdp_listening"], serde_json::json!(true));
        assert_eq!(json["web_listening"], serde_json::json!(false));
        assert!(json.get("fault").is_none(), "no fault means no key");
        // The daemon prefers `effective`, so it must be there and complete.
        assert!(json.get("effective").is_some(), "effective must be present");
        assert_eq!(json["effective"], json["config"]);
        assert_eq!(
            json["effective"]["codec"]["h264"],
            serde_json::json!("force")
        );
        assert_eq!(
            json["effective"]["codec"]["allow_remotefx"],
            serde_json::json!(false)
        );
        assert_eq!(
            json["effective"]["codec"]["vaapi_device"],
            serde_json::json!("/dev/dri/renderD128")
        );
        assert_eq!(json["effective"]["web"]["port"], serde_json::json!(3392));
        assert_eq!(
            json["effective"]["web"]["certificate"]["source"],
            serde_json::json!("custom")
        );
        assert_eq!(
            json["effective"]["web"]["certificate"]["certificate"],
            serde_json::json!("/etc/tls/c.pem")
        );

        let rows = json["sessions"].as_array().expect("sessions is an array");
        assert_eq!(rows.len(), 2);
        // Hardware session: the exact tier string and the device detail.
        assert_eq!(rows[0]["transport"], serde_json::json!("rdp"));
        assert_eq!(rows[0]["codec"], serde_json::json!("h264-vaapi"));
        assert_eq!(rows[0]["codec_device"], serde_json::json!("Intel iHD"));
        assert_eq!(rows[0]["width"], serde_json::json!(1920));
        // Software session: `"web"` transport, and no device key at all.
        assert_eq!(rows[1]["transport"], serde_json::json!("web"));
        assert_eq!(rows[1]["codec"], serde_json::json!("qoi-z"));
        assert!(
            rows[1].get("codec_device").is_none(),
            "codec_device must be skipped, not null, when there is none"
        );

        // Now through the daemon's own deserialisers.
        let state: DaemonState =
            serde_json::from_value(json.clone()).expect("the daemon mirrors must accept it");
        assert!(state.enabled);
        assert!(state.running);
        assert!(state.fault.is_none());
        // `effective.or(config)`: the effective config is what the page shows.
        let effective = state.effective.expect("effective deserialises");
        assert!(effective.enabled);
        assert_eq!(effective.rdp_port, 3391);
        assert_eq!(effective.max_sessions, 8);
        assert_eq!(effective.codec.h264, "force");
        assert!(!effective.codec.allow_remotefx);
        assert_eq!(
            effective.codec.vaapi_device.as_deref(),
            Some("/dev/dri/renderD128")
        );
        assert!(effective.web.enabled);
        assert_eq!(effective.web.port, 3392);
        assert!(!effective.web.https);
        assert_eq!(effective.web.certificate.source, "custom");
        assert_eq!(
            effective.web.certificate.certificate.as_deref(),
            Some("/etc/tls/c.pem")
        );
        assert_eq!(
            effective.web.certificate.private_key.as_deref(),
            Some("/etc/tls/k.pem")
        );

        assert_eq!(state.sessions.len(), 2);
        assert_eq!(state.sessions[0].transport, "rdp");
        assert_eq!(
            daemon_codec_tier(&state.sessions[0].codec),
            "h264-vaapi",
            "the hardware tier must be recognised, not fall back to QOI"
        );
        assert_eq!(state.sessions[0].codec_device.as_deref(), Some("Intel iHD"));
        assert_eq!(state.sessions[1].transport, "web");
        assert_eq!(daemon_codec_tier(&state.sessions[1].codec), "qoi-z");
        assert_eq!(state.sessions[1].codec_device, None);
    }

    #[test]
    fn the_daemon_prefers_effective_over_the_stored_config() {
        // Same conversation the daemon has, with the two configs made
        // distinguishable: whatever the daemon reports must come from
        // `effective`, not from the stored `config`.
        let mut json = snapshot_for_the_daemon();
        json["config"]["rdp_port"] = serde_json::json!(3389);
        json["effective"]["rdp_port"] = serde_json::json!(3391);

        let state: DaemonState = serde_json::from_value(json).expect("parses");
        let preferred = state.effective.or(state.config).expect("a config");
        assert_eq!(preferred.rdp_port, 3391, "effective wins");

        // And a state without `effective` still reads the stored config.
        let state: DaemonState =
            serde_json::from_str(r#"{"config": {"rdp_port": 3399}}"#).expect("parses");
        let preferred = state.effective.or(state.config).expect("a config");
        assert_eq!(preferred.rdp_port, 3399);
        assert_eq!(preferred.web.port, 3390, "missing keys keep defaults");
    }
}
