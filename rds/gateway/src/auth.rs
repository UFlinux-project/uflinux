//! Authentication and the user/group allow-list.
//!
//! The default build has **no** PAM backend: `pam-sys` needs `libclang` and the
//! PAM headers at build time, which are not present in every build
//! environment. Build with `--features pam` for the real
//! [`crate::auth::PamAuthenticator`]; without it, `PamAuthenticator` exists but
//! reports [`AuthError::BackendUnavailable`].

use std::ffi::CString;
use std::time::{Duration, Instant};

/// Why a login was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthError {
    /// Bad username/password, or the user is not allowed through.
    Denied,
    /// No usable authentication backend (e.g. built without `--features pam`).
    BackendUnavailable(&'static str),
    /// The backend itself failed.
    Backend(String),
}

impl std::fmt::Display for AuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Denied => f.write_str("login denied"),
            Self::BackendUnavailable(why) => write!(f, "{why}"),
            Self::Backend(why) => write!(f, "authentication backend error: {why}"),
        }
    }
}

impl std::error::Error for AuthError {}

impl AuthError {
    /// Message safe to return to an unauthenticated client.
    pub fn client_message(&self) -> &'static str {
        match self {
            Self::Denied => "invalid credentials",
            Self::BackendUnavailable(_) | Self::Backend(_) => "authentication unavailable",
        }
    }
}

/// Username/password backend.
pub trait Authenticator: std::fmt::Debug + Send + Sync {
    /// Verify `username`/`password`.
    fn authenticate(&self, username: &str, password: &str) -> Result<(), AuthError>;
}

/// Backend backed by the system PAM stack (`--features pam`).
#[derive(Debug, Clone)]
pub struct PamAuthenticator {
    /// PAM service name, i.e. `/etc/pam.d/<service>`.
    pub service: String,
}

impl PamAuthenticator {
    /// New backend for the given PAM service.
    pub fn new(service: impl Into<String>) -> Self {
        Self {
            service: service.into(),
        }
    }
}

#[cfg(feature = "pam")]
impl Authenticator for PamAuthenticator {
    fn authenticate(&self, username: &str, password: &str) -> Result<(), AuthError> {
        use pam::Client;

        let mut client = Client::with_password(&self.service)
            .map_err(|e| AuthError::Backend(format!("PAM service {:?}: {e}", self.service)))?;
        client
            .conversation_mut()
            .set_credentials(username, password);
        // `pam::Client` exposes only authenticate(); the account check that a
        // full stack would run is done by the session server, which is the
        // process that actually has to enforce access and expiry.
        client.authenticate().map_err(|_| AuthError::Denied)?;
        Ok(())
    }
}

#[cfg(not(feature = "pam"))]
impl Authenticator for PamAuthenticator {
    fn authenticate(&self, _username: &str, _password: &str) -> Result<(), AuthError> {
        Err(AuthError::BackendUnavailable(
            "PAM backend not compiled in (rebuild uflinux-rds-web with --features pam)",
        ))
    }
}

/// Exact-match backend used by the tests and by `UFLINUX_RDS_DEV_LOGIN`.
#[derive(Debug, Clone, Default)]
pub struct ConfiguredAuthenticator {
    /// `username -> password`.
    pub users: std::collections::BTreeMap<String, String>,
}

impl Authenticator for ConfiguredAuthenticator {
    fn authenticate(&self, username: &str, password: &str) -> Result<(), AuthError> {
        match self.users.get(username) {
            Some(expected)
                if crate::tokens::constant_time_eq(expected.as_bytes(), password.as_bytes()) =>
            {
                Ok(())
            }
            Some(_) | None => Err(AuthError::Denied),
        }
    }
}

/// Refusing backend, used when no authentication backend is available.
#[derive(Debug, Clone)]
pub struct DenyAll(pub AuthError);

impl Authenticator for DenyAll {
    fn authenticate(&self, _username: &str, _password: &str) -> Result<(), AuthError> {
        Err(self.0.clone())
    }
}

/// Everything needed to decide whether a user may open a session.
#[derive(Debug, Clone)]
pub struct Policy {
    /// `Config::allowed_users`.
    pub allowed_users: Vec<String>,
    /// `Config::allowed_groups`.
    pub allowed_groups: Vec<String>,
}

impl Policy {
    /// Build from the shared config.
    pub fn from_config(config: &uflinux_rds_common::Config) -> Self {
        Self {
            allowed_users: config.allowed_users.iter().cloned().collect(),
            allowed_groups: config.allowed_groups.iter().cloned().collect(),
        }
    }

    /// Accept the user when the allow-lists are empty (open policy), the user is
    /// named, or the user is in one of the named groups.
    pub fn is_allowed(
        &self,
        username: &str,
        member_of: impl Fn(&str, &[String]) -> Vec<String>,
    ) -> bool {
        is_allowed(
            username,
            &self.allowed_users,
            &self.allowed_groups,
            &member_of,
        )
    }
}

/// Pure allow-list check: `member_of` returns the groups of `username`.
pub fn is_allowed(
    username: &str,
    allowed_users: &[String],
    allowed_groups: &[String],
    member_of: &dyn Fn(&str, &[String]) -> Vec<String>,
) -> bool {
    if allowed_users.is_empty() && allowed_groups.is_empty() {
        return true;
    }
    if allowed_users.iter().any(|u| u == username) {
        return true;
    }
    if allowed_groups.is_empty() {
        return false;
    }
    let groups = member_of(username, allowed_groups);
    allowed_groups
        .iter()
        .any(|group| groups.iter().any(|have| have == group))
}

/// Group membership of `username`, restricted to `candidates`.
///
/// Uses `getgrouplist(3)` via `nix`; an unknown user has no groups.
pub fn system_groups(username: &str, candidates: &[String]) -> Vec<String> {
    let Ok(cname) = CString::new(username) else {
        return Vec::new();
    };
    let Ok(Some(user)) = nix::unistd::User::from_name(username) else {
        return Vec::new();
    };
    let Ok(gids) = nix::unistd::getgrouplist(&cname, user.gid) else {
        return Vec::new();
    };
    let wanted: Vec<String> = candidates.to_vec();
    let mut out = Vec::new();
    for gid in gids {
        if let Ok(Some(group)) = nix::unistd::Group::from_gid(gid) {
            let name: &str = &group.name;
            if wanted.iter().any(|candidate| candidate == name) {
                out.push(name.to_string());
            }
        }
    }
    out
}

/// Background task that drops expired tokens every `interval`.
pub async fn reaper(
    store: std::sync::Arc<std::sync::Mutex<crate::tokens::TokenStore>>,
    interval: Duration,
) {
    let mut tick = tokio::time::interval(interval);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        let removed = store
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .reap(Instant::now());
        if removed > 0 {
            tracing::debug!(removed, "reaped expired gateway tokens");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn groups_from(table: &[(&str, &[&str])]) -> impl Fn(&str, &[String]) -> Vec<String> {
        let table: Vec<(String, Vec<String>)> = table
            .iter()
            .map(|(u, g)| {
                (
                    (*u).to_string(),
                    g.iter().map(|g| (*g).to_string()).collect(),
                )
            })
            .collect();
        move |user, candidates| {
            table
                .iter()
                .find(|(u, _)| u == user)
                .map(|(_, groups)| {
                    groups
                        .iter()
                        .filter(|g| candidates.iter().any(|c| c == *g))
                        .cloned()
                        .collect()
                })
                .unwrap_or_default()
        }
    }

    fn s(items: &[&str]) -> Vec<String> {
        items.iter().map(|i| (*i).to_string()).collect()
    }

    #[test]
    fn empty_allow_lists_are_open() {
        let never = |_u: &str, _c: &[String]| Vec::new();
        assert!(is_allowed("anyone", &[], &[], &never));
    }

    #[test]
    fn named_user_is_accepted_and_others_rejected() {
        let never = |_u: &str, _c: &[String]| Vec::new();
        assert!(is_allowed("alice", &s(&["alice", "bob"]), &[], &never));
        assert!(!is_allowed("carol", &s(&["alice", "bob"]), &[], &never));
    }

    #[test]
    fn group_member_is_accepted() {
        let spec: [(&str, &[&str]); 1] = [("carol", &["uflinux-rds", "wheel"][..])];
        let table = groups_from(&spec);
        assert!(is_allowed(
            "carol",
            &s(&["alice"]),
            &s(&["uflinux-rds"]),
            &table
        ));
        assert!(!is_allowed(
            "dave",
            &s(&["alice"]),
            &s(&["uflinux-rds"]),
            &table
        ));
    }

    #[test]
    fn group_check_only_consults_candidates() {
        let seen = std::cell::RefCell::new(None);
        let recorder = |user: &str, candidates: &[String]| {
            *seen.borrow_mut() = Some((user.to_string(), candidates.to_vec()));
            vec!["uflinux-rds".to_string()]
        };
        // No groups configured: the group lookup must not happen at all.
        assert!(!is_allowed("carol", &s(&["alice"]), &[], &recorder));
        assert!(seen.borrow().is_none());
        assert!(is_allowed("carol", &[], &s(&["uflinux-rds"]), &recorder));
        assert_eq!(
            *seen.borrow(),
            Some(("carol".to_string(), s(&["uflinux-rds"])))
        );
    }

    #[test]
    fn configured_authenticator_matches_password_exactly() {
        let auth = ConfiguredAuthenticator {
            users: std::collections::BTreeMap::from([
                ("alice".to_string(), "s3cret".to_string()),
                ("empty".to_string(), String::new()),
            ]),
        };
        assert!(auth.authenticate("alice", "s3cret").is_ok());
        assert_eq!(
            auth.authenticate("alice", "wrong"),
            Err(AuthError::Denied),
            "no user enumeration"
        );
        assert_eq!(
            auth.authenticate("mallory", "s3cret"),
            Err(AuthError::Denied)
        );
        assert!(auth.authenticate("empty", "").is_ok());
    }

    #[test]
    fn stub_pam_backend_reports_itself_missing_without_the_feature() {
        let auth = PamAuthenticator::new("uflinux-rds");
        let err = auth.authenticate("alice", "x").unwrap_err();
        if cfg!(feature = "pam") {
            assert!(!matches!(err, AuthError::BackendUnavailable(_)));
        } else {
            assert!(matches!(err, AuthError::BackendUnavailable(_)));
            assert_eq!(err.client_message(), "authentication unavailable");
        }
    }

    #[test]
    fn deny_all_refuses_everything() {
        let auth = DenyAll(AuthError::Denied);
        assert_eq!(auth.authenticate("a", "b"), Err(AuthError::Denied));
    }

    #[test]
    fn policy_from_config_copies_both_lists() {
        let config = uflinux_rds_common::Config {
            allowed_users: ["alice".to_string()].into_iter().collect(),
            allowed_groups: ["uflinux-rds".to_string()].into_iter().collect(),
            ..uflinux_rds_common::Config::default()
        };
        let policy = Policy::from_config(&config);
        assert_eq!(policy.allowed_users, s(&["alice"]));
        assert_eq!(policy.allowed_groups, s(&["uflinux-rds"]));
        assert!(policy.is_allowed("alice", |_, _| Vec::new()));
    }

    #[test]
    fn system_groups_for_the_current_user() {
        let me = std::env::var("USER").unwrap_or_default();
        if me.is_empty() {
            return;
        }
        let groups = system_groups(&me, &["wheel".to_string(), "users".to_string()]);
        assert!(groups.len() <= 2);
        assert!(system_groups("no-such-user-xyz-42", &["wheel".to_string()]).is_empty());
    }
}
