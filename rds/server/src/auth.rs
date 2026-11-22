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
fn desktop_runtime_dir(base: &Path, uid: Option<u32>) -> PathBuf {
    let pid = std::process::id();
    match uid {
        Some(uid) => base.join(format!("rdp-{uid}-{pid}")),
        None => base.join(format!("rdp-{pid}")),
    }
}

/// Create a private runtime directory for one desktop. The shared root-owned
/// base allows traversal, and only the authenticated account owns the leaf.
pub fn prepare_desktop_dir(
    base: &Path,
    uid: Option<u32>,
    owner: Option<&DesktopOwner>,
) -> std::io::Result<PathBuf> {
    use std::fs::{self, DirBuilder, Permissions};
    use std::io::{Error, ErrorKind};
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};

    if uid.is_some() {
        match DirBuilder::new().mode(0o711).create(base) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
        let metadata = fs::symlink_metadata(base)?;
        if !metadata.is_dir() || metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
            return Err(Error::new(
                ErrorKind::PermissionDenied,
                "unsafe RDS runtime directory",
            ));
        }
        fs::set_permissions(base, Permissions::from_mode(0o711))?;
    } else {
        fs::create_dir_all(base)?;
    }

    let path = desktop_runtime_dir(base, uid);
    DirBuilder::new().mode(0o700).create(&path)?;
    let prepared = fs::set_permissions(&path, Permissions::from_mode(0o700)).and_then(|()| {
        if let Some(owner) = owner {
            nix::unistd::chown(
                &path,
                Some(nix::unistd::Uid::from_raw(owner.uid)),
                Some(nix::unistd::Gid::from_raw(owner.gid)),
            )
            .map_err(Error::from)?;
        }
        Ok(())
    });
    if let Err(error) = prepared {
        let _ = fs::remove_dir(&path);
        return Err(error);
    }
    Ok(path)
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
        let root = Path::new("/run/uflinux-rds-desktops");
        let a = desktop_runtime_dir(root, Some(1000));
        let b = desktop_runtime_dir(root, Some(1001));
        assert_ne!(a, b);
        // `Path::starts_with` compares components, so compare text here.
        assert!(
            a.to_string_lossy()
                .starts_with("/run/uflinux-rds-desktops/rdp-1000-"),
            "{a:?}"
        );
        assert!(
            b.to_string_lossy()
                .starts_with("/run/uflinux-rds-desktops/rdp-1001-"),
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
        let path = desktop_runtime_dir(Path::new("/run/uflinux-rds-desktops"), Some(1000));
        let name = path
            .file_name()
            .expect("a directory name")
            .to_string_lossy();
        assert_eq!(name, format!("rdp-1000-{}", std::process::id()));
    }

    #[test]
    fn a_prepared_desktop_dir_is_mode_0700() {
        use std::os::unix::fs::PermissionsExt;

        let base = std::env::temp_dir().join(format!("uflinux-rds-auth-{}", std::process::id()));
        let path = desktop_runtime_dir(&base, None);
        let prepared = prepare_desktop_dir(&base, None, None).expect("the directory is created");
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

#[cfg(feature = "pam")]
pub mod login {
    use super::{AuthError, DesktopOwner, Policy, admit};
    use pam::{PamFlag, PamHandle, PamItemType, PamReturnCode};
    use std::ffi::{CStr, CString, c_int, c_void};

    struct Credentials {
        username: CString,
        password: CString,
    }

    unsafe extern "C" fn converse(
        count: c_int,
        messages: *mut *const pam::PamMessage,
        output: *mut *mut pam::PamResponse,
        data: *mut c_void,
    ) -> c_int {
        if count <= 0 || messages.is_null() || output.is_null() || data.is_null() {
            return PamReturnCode::Conv_Err as c_int;
        }
        // Linux PAM supplies an array of message pointers. It takes ownership
        // of the response array and strings only when the callback succeeds.
        unsafe {
            let credentials = &*data.cast::<Credentials>();
            let responses = libc::calloc(count as usize, size_of::<pam::PamResponse>())
                .cast::<pam::PamResponse>();
            if responses.is_null() {
                return PamReturnCode::Buf_Err as c_int;
            }
            for index in 0..count as usize {
                let message = *messages.add(index);
                let source = message
                    .as_ref()
                    .and_then(|message| match message.msg_style {
                        1 => Some(Some(&credentials.password)), // PAM_PROMPT_ECHO_OFF
                        2 => Some(Some(&credentials.username)), // PAM_PROMPT_ECHO_ON
                        3 | 4 => Some(None),                    // PAM_ERROR_MSG / PAM_TEXT_INFO
                        _ => None,
                    });
                match source {
                    Some(None) => continue,
                    Some(Some(source)) => {
                        let response = libc::strdup(source.as_ptr());
                        if !response.is_null() {
                            (*responses.add(index)).resp = response;
                            continue;
                        }
                    }
                    None => {}
                }
                for previous in 0..index {
                    libc::free((*responses.add(previous)).resp.cast());
                }
                libc::free(responses.cast());
                return PamReturnCode::Conv_Err as c_int;
            }
            *output = responses;
        }
        PamReturnCode::Success as c_int
    }

    /// Owns a PAM transaction on the worker that created it. The desktop
    /// lease keeps that worker alive until the compositor has stopped.
    pub struct Login {
        handle: &'static mut PamHandle,
        _credentials: Box<Credentials>,
        tracked: bool,
        established: bool,
        status: PamReturnCode,
        pub username: String,
        pub session_id: Option<String>,
    }

    impl Login {
        pub fn open(
            service: &str,
            username: String,
            password: String,
            peer: &str,
            policy: &Policy,
        ) -> Result<Self, AuthError> {
            let mut credentials = Box::new(Credentials {
                username: CString::new(username.as_str()).map_err(|_| AuthError::Denied)?,
                password: CString::new(password).map_err(|_| AuthError::Denied)?,
            });
            let conversation = pam::ffi::pam_conv {
                conv: Some(converse),
                appdata_ptr: (&mut *credentials as *mut Credentials).cast(),
            };
            let service =
                CString::new(service).map_err(|error| AuthError::Backend(error.to_string()))?;
            let mut handle = std::ptr::null_mut();
            // Keep the service name, username and conversation alive through
            // pam_start. PAM copies the names and stores the callback data.
            let status = unsafe {
                pam::ffi::pam_start(
                    service.as_ptr(),
                    credentials.username.as_ptr(),
                    &conversation,
                    &mut handle,
                )
            };
            let handle = match PamReturnCode::from(status) {
                PamReturnCode::Success if !handle.is_null() => {
                    // SAFETY: this transaction owns the handle until pam_end.
                    unsafe { &mut *handle }
                }
                status => return Err(AuthError::Backend(format!("pam_start: {status:?}"))),
            };
            let mut login = Self {
                handle,
                _credentials: credentials,
                tracked: false,
                established: false,
                status: PamReturnCode::Success,
                username,
                session_id: None,
            };
            let peer = peer.rsplit_once(" -> ").map_or(peer, |(_, peer)| peer);
            let peer = peer
                .parse::<std::net::SocketAddr>()
                .map_or_else(|_| "rdp".to_owned(), |address| address.ip().to_string());
            let peer = CString::new(peer).map_err(|_| AuthError::Denied)?;
            // PAM copies PAM_RHOST during this call.
            let status = unsafe {
                pam::ffi::pam_set_item(
                    login.handle,
                    PamItemType::RHost as c_int,
                    peer.as_ptr().cast(),
                )
            };
            login.check(status.into())?;
            let status = pam::authenticate(login.handle, PamFlag::None);
            login.check(status).map_err(|_| AuthError::Denied)?;
            let status = pam::acct_mgmt(login.handle, PamFlag::None);
            login.check(status).map_err(|_| AuthError::Denied)?;

            let mut item = std::ptr::null();
            // PAM owns the username string. Copy it before the next PAM call.
            let status = unsafe {
                pam::ffi::pam_get_item(login.handle, PamItemType::User as c_int, &mut item)
            };
            login.check(status.into())?;
            if item.is_null() {
                return Err(AuthError::Denied);
            }
            login.username = unsafe { CStr::from_ptr(item.cast()) }
                .to_str()
                .map_err(|_| AuthError::Denied)?
                .to_owned();
            if DesktopOwner::lookup(&login.username).is_none() || !admit(policy, &login.username) {
                return Err(AuthError::NotAllowed);
            }

            for entry in [
                "XDG_SESSION_TYPE=wayland",
                "XDG_SESSION_CLASS=user",
                "XDG_SESSION_DESKTOP=labwc",
                "XDG_SESSION_REMOTE=1",
            ] {
                pam::putenv(login.handle, entry)
                    .map_err(|error| AuthError::Backend(error.to_string()))?;
            }
            let status = pam::setcred(login.handle, PamFlag::Establish_Cred);
            login.check(status)?;
            login.established = true;
            let status = pam::open_session(login.handle, false);
            login.check(status)?;
            login.tracked = true;
            login.session_id = pam::getenv(login.handle, "XDG_SESSION_ID")
                .map_err(|error| AuthError::Backend(error.to_string()))?
                .map(str::to_owned);
            Ok(login)
        }

        fn check(&mut self, status: PamReturnCode) -> Result<(), AuthError> {
            self.status = status;
            if status == PamReturnCode::Success {
                Ok(())
            } else {
                Err(AuthError::Backend(
                    pam::strerror(self.handle, status).to_owned(),
                ))
            }
        }
    }

    impl Drop for Login {
        fn drop(&mut self) {
            if self.tracked {
                let _ = pam::close_session(self.handle, false);
            }
            if self.established {
                let _ = pam::setcred(self.handle, PamFlag::Delete_Cred);
            }
            let _ = pam::end(self.handle, self.status);
        }
    }
}
