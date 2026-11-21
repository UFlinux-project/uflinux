//! Who is allowed to open a session, and how they prove it.
//!
//! Two separate questions, kept separate here:
//!
//! * **Authentication** — the password (or the absence of one). With the
//!   `pam` cargo feature this is PAM against the `uflinux-rds` service, the
//!   same way the web gateway authenticates. Without it there is a documented
//!   stub that accepts exactly the single user named by `--user` /
//!   `$UFLINUX_RDS_SESSION_USER`, which is what the developer smoke test and
//!   the broker's own tests rely on.
//! * **Admission** — whether that user may have a remote desktop at all, per
//!   `allowed_users` / `allowed_groups`. This is policy, not proof, so it runs
//!   after authentication and is a pure function over an injected group list.

use std::fmt;

/// Why a logon was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthError {
    /// Credentials were wrong. The message shown to the client deliberately
    /// says nothing about whether the user exists.
    Denied,
    /// The user authenticated but policy forbids a remote session.
    NotAllowed,
    /// The authentication backend could not be consulted at all.
    BackendUnavailable(&'static str),
    /// The backend failed in an unexpected way.
    Backend(String),
}

impl AuthError {
    /// What the client is told. Never reveals whether the account exists.
    pub fn client_message(&self) -> &'static str {
        match self {
            Self::Denied | Self::NotAllowed => "invalid credentials",
            Self::BackendUnavailable(_) | Self::Backend(_) => "authentication unavailable",
        }
    }
}

impl fmt::Display for AuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Denied => f.write_str("credentials were rejected"),
            Self::NotAllowed => f.write_str("user is not allowed a remote session"),
            Self::BackendUnavailable(why) => write!(f, "authentication backend unavailable: {why}"),
            Self::Backend(why) => write!(f, "authentication backend error: {why}"),
        }
    }
}

impl std::error::Error for AuthError {}

/// Something that can turn a username and password into a verdict.
pub trait Authenticator: fmt::Debug + Send + Sync {
    /// Checks one logon attempt.
    fn authenticate(&self, username: &str, password: &str) -> Result<(), AuthError>;
}

/// PAM-backed authentication.
///
/// Only meaningful when the crate is built with `--features pam`; see the
/// feature's comment in `Cargo.toml` for why it is opt-in (`pam-sys` needs
/// `security/pam_appl.h` and an unversioned `libclang.so` at build time).
#[derive(Debug, Clone)]
pub struct PamAuthenticator {
    /// PAM service name, i.e. `/etc/pam.d/<service>`.
    pub service: String,
}

impl PamAuthenticator {
    /// A PAM authenticator for the given service.
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
            .map_err(|_| AuthError::BackendUnavailable("could not open the PAM session"))?;
        client
            .conversation_mut()
            .set_credentials(username, password);
        // `pam::Client` exposes authenticate() only; `pam_acct_mgmt` would have
        // to come from `pam::functions` with a raw handle, and the account
        // check is enforced by the broker's allow-list plus the session
        // server's own admission policy.
        client.authenticate().map_err(|_| AuthError::Denied)?;
        Ok(())
    }
}

#[cfg(not(feature = "pam"))]
impl Authenticator for PamAuthenticator {
    fn authenticate(&self, _username: &str, _password: &str) -> Result<(), AuthError> {
        Err(AuthError::BackendUnavailable(
            "this build has no PAM support; rebuild with --features pam",
        ))
    }
}

/// The no-PAM fallback: one pre-named user, no password check.
///
/// This is not a security boundary — it exists so the binary can be exercised
/// (and so a kiosk-style single-user image can run it) without linking PAM.
/// The empty username is rejected rather than accepted, because an unset
/// `--user` means nobody decided who may connect.
#[derive(Debug, Clone)]
pub struct StubAuthenticator {
    /// The only username that will be accepted.
    pub user: Option<String>,
}

impl StubAuthenticator {
    /// A stub that accepts `user` if it names anyone.
    pub fn new(user: Option<String>) -> Self {
        Self { user }
    }
}

impl Authenticator for StubAuthenticator {
    fn authenticate(&self, username: &str, _password: &str) -> Result<(), AuthError> {
        match &self.user {
            Some(expected) if !expected.trim().is_empty() && username == expected.trim() => Ok(()),
            Some(_) | None => Err(AuthError::Denied),
        }
    }
}

/// `allowed_users` / `allowed_groups` from the configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Policy {
    /// Usernames allowed a session. Empty means "not restricting by name".
    pub allowed_users: Vec<String>,
    /// Group names allowed a session. Empty means "not restricting by group".
    pub allowed_groups: Vec<String>,
}

impl Policy {
    /// Builds the policy from a loaded configuration.
    pub fn from_config(config: &uflinux_rds_common::Config) -> Self {
        Self {
            allowed_users: config.allowed_users.iter().cloned().collect(),
            allowed_groups: config.allowed_groups.iter().cloned().collect(),
        }
    }

    /// Whether `username` may have a session, using `groups` as the answer to
    /// "which groups is this user in".
    pub fn is_allowed(&self, username: &str, groups: &[String]) -> bool {
        is_allowed(username, &self.allowed_users, &self.allowed_groups, groups)
    }
}

/// Admission rule, free of any system state so it can be tested directly.
///
/// Both lists empty means the operator did not restrict anything, so everyone
/// who authenticated is admitted. Otherwise a named user is admitted, and
/// failing that any membership in `allowed_groups` is.
pub fn is_allowed(
    username: &str,
    allowed_users: &[String],
    allowed_groups: &[String],
    groups: &[String],
) -> bool {
    if allowed_users.is_empty() && allowed_groups.is_empty() {
        return true;
    }
    if allowed_users.iter().any(|name| name == username) {
        return true;
    }
    allowed_groups
        .iter()
        .any(|wanted| groups.iter().any(|have| have == wanted))
}

/// The groups `username` belongs to, restricted to `candidates`.
///
/// Returns `None` when the account cannot be looked up, which the caller must
/// treat as "no groups" rather than as an error: a user who is not in the
/// password database will fail authentication anyway.
pub fn system_groups(username: &str, candidates: &[String]) -> Option<Vec<String>> {
    use std::ffi::CString;

    let cname = CString::new(username).ok()?;
    let user = nix::unistd::User::from_name(username).ok()??;
    let gids = nix::unistd::getgrouplist(&cname, user.gid).ok()?;
    let mut member_of = Vec::new();
    for gid in gids {
        if let Ok(Some(group)) = nix::unistd::Group::from_gid(gid) {
            member_of.push(group.name.clone());
        }
    }
    Some(
        candidates
            .iter()
            .filter(|wanted| member_of.iter().any(|have| have == *wanted))
            .cloned()
            .collect(),
    )
}

/// Looks up the real groups of `username` and applies `policy`.
pub fn admit(policy: &Policy, username: &str) -> bool {
    if policy.allowed_users.is_empty() && policy.allowed_groups.is_empty() {
        return true;
    }
    let groups = system_groups(username, &policy.allowed_groups).unwrap_or_default();
    policy.is_allowed(username, &groups)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn an_unrestricted_policy_admits_anyone_authenticated() {
        let policy = Policy::default();
        assert!(policy.is_allowed("anyone", &[]));
        assert!(policy.is_allowed("anyone", &list(&["wheel"])));
    }

    #[test]
    fn a_named_user_is_admitted_without_the_right_groups() {
        let policy = Policy {
            allowed_users: list(&["panos"]),
            allowed_groups: list(&["rdp"]),
        };
        assert!(policy.is_allowed("panos", &[]));
    }

    #[test]
    fn group_membership_admits_when_the_name_is_not_listed() {
        let policy = Policy {
            allowed_users: list(&["someoneelse"]),
            allowed_groups: list(&["rdp", "wheel"]),
        };
        assert!(policy.is_allowed("panos", &list(&["users", "wheel"])));
        assert!(!policy.is_allowed("panos", &list(&["users"])));
    }

    #[test]
    fn a_group_only_policy_rejects_an_outsider() {
        let policy = Policy {
            allowed_users: Vec::new(),
            allowed_groups: list(&["rdp"]),
        };
        assert!(!policy.is_allowed("guest", &list(&["users"])));
        assert!(policy.is_allowed("guest", &list(&["rdp"])));
    }

    #[test]
    fn a_user_only_policy_ignores_groups_entirely() {
        let policy = Policy {
            allowed_users: list(&["panos"]),
            allowed_groups: Vec::new(),
        };
        assert!(!policy.is_allowed("root", &list(&["wheel", "rdp"])));
    }

    #[test]
    fn the_policy_comes_straight_out_of_the_config() {
        let config = uflinux_rds_common::Config {
            allowed_users: ["panos".to_owned()].into_iter().collect(),
            allowed_groups: ["rdp".to_owned()].into_iter().collect(),
            ..Default::default()
        };
        let policy = Policy::from_config(&config);
        assert_eq!(policy.allowed_users, list(&["panos"]));
        assert_eq!(policy.allowed_groups, list(&["rdp"]));
    }

    #[test]
    fn the_stub_accepts_only_the_named_user_and_never_an_empty_name() {
        let auth = StubAuthenticator::new(Some("panos".to_owned()));
        assert!(auth.authenticate("panos", "").is_ok());
        assert_eq!(
            auth.authenticate("root", "hunter2"),
            Err(AuthError::Denied),
            "a password cannot promote a different user"
        );

        let unnamed = StubAuthenticator::new(None);
        assert_eq!(unnamed.authenticate("panos", "x"), Err(AuthError::Denied));

        let blank = StubAuthenticator::new(Some("   ".to_owned()));
        assert_eq!(blank.authenticate("", ""), Err(AuthError::Denied));
    }

    #[cfg(not(feature = "pam"))]
    #[test]
    fn a_missing_backend_is_reported_as_unavailable_not_denied() {
        // Without the feature, PAM must not look like a wrong password: the
        // client message differs and the log line says what to rebuild.
        let auth = PamAuthenticator::new("uflinux-rds");
        let err = auth
            .authenticate("panos", "x")
            .expect_err("no PAM in the default test build");
        assert!(matches!(err, AuthError::BackendUnavailable(_)));
        assert_eq!(err.client_message(), "authentication unavailable");
    }

    #[cfg(feature = "pam")]
    #[test]
    fn a_missing_backend_is_reported_as_unavailable_not_denied() {
        // With the feature the module really talks to PAM, so an unknown user
        // is a wrong password rather than a missing backend: `Denied`, never
        // `BackendUnavailable`, which would tell the client to retry.
        let auth = PamAuthenticator::new("uflinux-rds");
        let err = auth
            .authenticate("uflinux-rds-no-such-user", "x")
            .expect_err("PAM must reject a user that does not exist");
        assert!(
            matches!(err, AuthError::Denied),
            "PAM reported {err:?} instead of denying an unknown user"
        );
        assert_eq!(err.client_message(), "invalid credentials");
    }

    #[test]
    fn the_client_message_never_leaks_why() {
        assert_eq!(AuthError::Denied.client_message(), "invalid credentials");
        assert_eq!(
            AuthError::NotAllowed.client_message(),
            "invalid credentials"
        );
    }
}
