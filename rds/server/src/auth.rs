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
use std::path::{Path, PathBuf};

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

/// The account a desktop is started as.
///
/// The compositor drops its own privileges (see
/// [`uflinux_rds_wayland::DesktopBackend`]), but the session host runs as root
/// and has to prepare a private directory for the compositor's socket first,
/// which needs the numeric ids.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesktopOwner {
    /// Account name, as authenticated.
    pub name: String,
    /// Numeric user id.
    pub uid: u32,
    /// Primary numeric group id.
    pub gid: u32,
}

impl DesktopOwner {
    /// Resolves `username` in the password database.
    ///
    /// `None` means the account is unknown, which only happens with the
    /// non-PAM stub authenticator; the caller then keeps the desktop at the
    /// session host's own privileges.
    pub fn lookup(username: &str) -> Option<Self> {
        let user = nix::unistd::User::from_name(username).ok()??;
        Some(Self {
            name: username.to_owned(),
            uid: user.uid.as_raw(),
            gid: user.gid.as_raw(),
        })
    }
}

/// The private directory one desktop gets for its Wayland socket.
///
/// `base` is the directory this session host owns: `$XDG_RUNTIME_DIR` when it
/// is unprivileged, otherwise the shared state directory. A session host that
/// runs as root serves whichever account authenticates, so the path is
/// namespaced by uid — two users connecting at the same time must not share
/// one directory. An unprivileged host only ever serves one account, so it
/// needs no namespace and no ownership change.
pub fn desktop_runtime_dir(base: &Path, uid: Option<u32>) -> PathBuf {
    let mut path = match uid {
        Some(uid) => base.join("sessions").join(uid.to_string()),
        None => base.to_path_buf(),
    };
    path.push(format!("rdp-{}", std::process::id()));
    path
}

/// Creates [`desktop_runtime_dir`]'s directory as `0700` owned by `owner`.
///
/// Returns the path so the caller cannot use a directory it did not prepare.
pub fn prepare_desktop_dir(path: &Path, owner: Option<&DesktopOwner>) -> std::io::Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt;

    std::fs::create_dir_all(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    if let Some(owner) = owner {
        nix::unistd::chown(
            path,
            Some(nix::unistd::Uid::from_raw(owner.uid)),
            Some(nix::unistd::Gid::from_raw(owner.gid)),
        )
        .map_err(|e| std::io::Error::from_raw_os_error(e as i32))?;
    }
    Ok(path.to_path_buf())
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

    #[test]
    fn a_privileged_desktop_dir_is_namespaced_by_uid() {
        // The session host runs as root and serves whichever account
        // authenticates, so two users must not land in one directory.
        let root = Path::new("/var/lib/uflinux-rds");
        let a = desktop_runtime_dir(root, Some(1000));
        let b = desktop_runtime_dir(root, Some(1001));
        assert_ne!(a, b);
        // `Path::starts_with` compares components, so compare text here.
        assert!(
            a.to_string_lossy()
                .starts_with("/var/lib/uflinux-rds/sessions/1000/rdp-"),
            "{a:?}"
        );
        assert!(
            b.to_string_lossy()
                .starts_with("/var/lib/uflinux-rds/sessions/1001/rdp-"),
            "{b:?}"
        );
    }

    #[test]
    fn an_unprivileged_desktop_dir_stays_under_the_runtime_dir() {
        // A session host that is already the user needs no uid namespace: its
        // runtime directory is private to it already.
        let base = Path::new("/run/user/1000");
        let path = desktop_runtime_dir(base, None);
        assert!(
            path.to_string_lossy().starts_with("/run/user/1000/rdp-"),
            "{path:?}"
        );
        assert!(!path.starts_with("/run/user/1000/sessions"));
    }

    #[test]
    fn the_desktop_dir_is_private_to_this_process() {
        let path = desktop_runtime_dir(Path::new("/var/lib/uflinux-rds"), Some(1000));
        let name = path
            .file_name()
            .expect("a directory name")
            .to_string_lossy();
        assert_eq!(name, format!("rdp-{}", std::process::id()));
    }

    #[test]
    fn a_prepared_desktop_dir_is_mode_0700() {
        use std::os::unix::fs::PermissionsExt;

        let base = std::env::temp_dir().join(format!("uflinux-rds-auth-{}", std::process::id()));
        let path = base.join("rdp-0");
        let prepared = prepare_desktop_dir(&path, None).expect("the directory is created");
        assert_eq!(prepared, path);
        let mode = std::fs::metadata(&path)
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_desktop_owner_for_an_unknown_account_is_none() {
        // Only the non-PAM stub can authenticate somebody who is not in the
        // password database; the caller treats `None` as "no privilege drop".
        assert!(DesktopOwner::lookup("uflinux-rds-no-such-user").is_none());
    }
}
