//! Shared application state.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use uflinux_rds_common::Config;

use crate::auth::{Authenticator, Policy};
use crate::tokens::TokenStore;

/// Everything the request handlers need.
#[derive(Clone)]
pub struct AppState {
    /// Inner state, cheap to clone behind an `Arc`.
    pub inner: Arc<Inner>,
}

/// The state itself.
pub struct Inner {
    /// Effective configuration.
    pub config: Config,
    /// Single-use WebSocket tokens.
    pub tokens: Arc<Mutex<TokenStore>>,
    /// Where the self-signed certificate lives.
    pub state_dir: PathBuf,
    /// Broker control socket.
    pub control_socket: PathBuf,
    /// Login backend.
    pub authenticator: Arc<dyn Authenticator>,
    /// Who may log in.
    pub policy: Policy,
    /// True when serving HTTPS (drives HSTS and the `ws_url` scheme).
    pub https: bool,
}

impl AppState {
    /// Build the state for a resolved configuration.
    pub fn new(
        config: Config,
        state_dir: PathBuf,
        control_socket: PathBuf,
        authenticator: Arc<dyn Authenticator>,
    ) -> Self {
        let policy = Policy::from_config(&config);
        let https = config.web.https;
        let tokens = Arc::new(Mutex::new(TokenStore::new(
            u32::try_from(config.web.token_ttl_minutes).unwrap_or(u32::MAX),
        )));
        Self {
            inner: Arc::new(Inner {
                config,
                tokens,
                state_dir,
                control_socket,
                authenticator,
                policy,
                https,
            }),
        }
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
        f.debug_struct("Inner")
            .field("config", &self.config)
            .field("state_dir", &self.state_dir)
            .field("control_socket", &self.control_socket)
            .field("policy", &self.policy)
            .field("https", &self.https)
            .finish_non_exhaustive()
    }
}
