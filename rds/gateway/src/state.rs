//! Shared application state.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use uflinux_rds_common::Config;

use crate::auth::{Authenticator, Policy};
use crate::tokens::TokenStore;

/// Ceiling on simultaneous blocking authentication jobs.
///
/// PAM conversations and `getgrouplist(3)` block, and a slow backend (an
/// LDAP-backed PAM module, say) can hold a job for seconds. Sizing the pool to
/// the worker count would let a burst of logins occupy every Tokio worker;
/// four is plenty for a console and makes the rest queue instead.
const MAX_CONCURRENT_AUTH: usize = 4;

/// Everything the request handlers need.
#[derive(Clone)]
pub struct AppState {
    /// Inner state, cheap to clone behind an `Arc`.
    pub inner: Arc<Inner>,
}

/// The state itself.
pub struct Inner {
    /// Refreshable configuration view: read with [`AppState::view`], swapped
    /// by [`AppState::reload`] on SIGHUP.
    view: RwLock<Arc<Snapshot>>,
    /// Single-use WebSocket tokens.
    pub tokens: Arc<Mutex<TokenStore>>,
    /// Where the self-signed certificate lives.
    pub state_dir: PathBuf,
    /// Broker control socket.
    pub control_socket: PathBuf,
    /// Login backend.
    pub authenticator: Arc<dyn Authenticator>,
    /// Permits for blocking authentication jobs (see [`MAX_CONCURRENT_AUTH`]).
    pub auth_permits: Arc<tokio::sync::Semaphore>,
    /// True when serving HTTPS (drives HSTS and the `ws_url` scheme).
    ///
    /// Frozen at startup: the listener was built with this value, so a reload
    /// must not rewrite the scheme of an already-bound server.
    pub https: bool,
}

/// The part of the configuration a reload can change.
///
/// `web.https` is deliberately absent: the listener was built with it, so the
/// running server keeps whatever scheme it started with. Handlers take the
/// scheme from [`Inner::https`] instead.
#[derive(Debug, Clone)]
pub struct Snapshot {
    /// Effective configuration.
    pub config: Config,
    /// Who may log in.
    pub policy: Policy,
}

impl Snapshot {
    fn from_config(config: Config) -> Self {
        let policy = Policy::from_config(&config);
        Self { config, policy }
    }
}

impl AppState {
    /// Build the state for a resolved configuration.
    pub fn new(
        config: Config,
        state_dir: PathBuf,
        control_socket: PathBuf,
        authenticator: Arc<dyn Authenticator>,
    ) -> Self {
        let https = config.web.https;
        let tokens = Arc::new(Mutex::new(TokenStore::new(
            u32::try_from(config.web.token_ttl_minutes).unwrap_or(u32::MAX),
        )));
        Self {
            inner: Arc::new(Inner {
                view: RwLock::new(Arc::new(Snapshot::from_config(config))),
                tokens,
                state_dir,
                control_socket,
                authenticator,
                auth_permits: Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_AUTH)),
                https,
            }),
        }
    }

    /// Current configuration view. A handler should take one snapshot and use
    /// it throughout, so a reload cannot mix a pre-reload port with a
    /// post-reload allow-list.
    pub fn view(&self) -> Arc<Snapshot> {
        self.inner
            .view
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Swap in a reloaded configuration (SIGHUP). Established sessions are
    /// untouched: tokens live in the unchanged store and only new logins see
    /// the new RDP target or allow-list.
    pub fn reload(&self, config: Config) {
        let ttl_minutes = config.web.token_ttl_minutes;
        tracing::info!(
            rdp_target = %crate::config::rdp_target(&config),
            token_ttl_minutes = ttl_minutes,
            "configuration reloaded"
        );
        if let Ok(mut view) = self.inner.view.write() {
            *view = Arc::new(Snapshot::from_config(config));
        }
        // Tokens already handed to a browser keep the expiry they were issued
        // with; the new TTL applies to tokens minted from now on. Clamped like
        // `TokenStore::new`: a zero TTL would expire every token on arrival.
        let minutes = u64::from(u32::try_from(ttl_minutes).unwrap_or(u32::MAX)).max(1);
        self.tokens().set_ttl(Duration::from_secs(minutes * 60));
    }

    /// Lock the token store, recovering from a poisoned lock: the store is a
    /// plain map and a panic elsewhere must not take the gateway down.
    pub fn tokens(&self) -> std::sync::MutexGuard<'_, TokenStore> {
        self.inner
            .tokens
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl std::fmt::Debug for Inner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let view = self
            .view
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        f.debug_struct("Inner")
            .field("config", &view.config)
            .field("state_dir", &self.state_dir)
            .field("control_socket", &self.control_socket)
            .field("policy", &view.policy)
            .field("https", &self.https)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{AuthError, DenyAll};

    fn state_with(config: Config) -> AppState {
        AppState::new(
            config,
            PathBuf::from("/nonexistent"),
            PathBuf::from("/nonexistent/control.sock"),
            Arc::new(DenyAll(AuthError::Denied)),
        )
    }

    #[test]
    fn a_swapped_view_is_seen_but_an_issued_token_still_resolves() {
        let mut config = Config::default();
        config.rdp_port = 3391;
        config.allowed_users = ["alice".to_string()].into_iter().collect();
        let state = state_with(config);

        let before = state.view();
        assert_eq!(before.config.rdp_port, 3391);
        assert_eq!(before.policy.allowed_users, vec!["alice".to_owned()]);

        let issued = state
            .tokens()
            .issue("alice", "127.0.0.1:3391", std::time::Instant::now());

        let mut reloaded = Config::default();
        reloaded.rdp_port = 3392;
        reloaded.allowed_users = ["bob".to_string()].into_iter().collect();
        reloaded.web.token_ttl_minutes = 1;
        state.reload(reloaded);

        let after = state.view();
        assert_eq!(after.config.rdp_port, 3392);
        assert_eq!(after.policy.allowed_users, vec!["bob".to_owned()]);
        // A snapshot a handler took before the swap is unaffected by it.
        assert_eq!(before.config.rdp_port, 3391);

        // A token issued before the reload still resolves: the session it
        // belongs to is not disturbed by the swap.
        let session = state
            .tokens()
            .consume_detailed(&issued.secret, std::time::Instant::now())
            .expect("a token survives a config reload");
        assert_eq!(session.target, "127.0.0.1:3391");
        assert_eq!(session.user, "alice");
    }

    #[test]
    fn a_reload_updates_the_token_ttl() {
        let state = state_with(Config::default());
        assert_eq!(
            state.tokens().ttl(),
            Duration::from_secs(5 * 60),
            "default TTL is five minutes"
        );
        let mut reloaded = Config::default();
        reloaded.web.token_ttl_minutes = 30;
        state.reload(reloaded);
        assert_eq!(state.tokens().ttl(), Duration::from_secs(30 * 60));
    }
}
