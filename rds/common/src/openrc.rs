//! Locating the OpenRC client binaries.
//!
//! Every caller here reaches OpenRC from a process whose `PATH` may not
//! include the administrator directories: the settings daemon hands work to a
//! polkit-spawned helper, and that helper inherits a `PATH` without `/sbin`.
//! A bare `rc-update` then fails with `ENOENT`, which the UI reports as
//! "Cannot start rc-update" even though the user is entitled to run it.
//! Resolving the absolute path once removes the dependency on the caller's
//! environment, the same way `uflinux-settings` invokes `/sbin/hwclock`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// Directories that hold the OpenRC clients, in the order they are probed.
/// Alpine keeps `/sbin` as a symlink into `/usr/sbin` under merged-usr, but a
/// non-merged install really does have both, so neither entry is redundant.
const DIRECTORIES: [&str; 4] = ["/sbin", "/usr/sbin", "/bin", "/usr/bin"];

/// Absolute path of an OpenRC client such as `rc-update` or `rc-service`.
///
/// Falls back to the bare name when the binary is nowhere to be found, so the
/// message the user sees still names the tool that is missing rather than an
/// odd path.
pub fn binary(name: &str) -> PathBuf {
    static CACHE: OnceLock<Mutex<HashMap<String, PathBuf>>> = OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    if let Some(path) = cache.lock().ok().and_then(|paths| paths.get(name).cloned()) {
        return path;
    }
    let path = DIRECTORIES
        .iter()
        .map(|dir| Path::new(dir).join(name))
        .find(|candidate| candidate.is_file())
        .unwrap_or_else(|| PathBuf::from(name));
    if let Ok(mut paths) = cache.lock() {
        paths.insert(name.to_owned(), path.clone());
    }
    path
}

/// `rc-update`, already resolved.
pub fn rc_update() -> PathBuf {
    binary("rc-update")
}

/// `rc-service`, already resolved.
pub fn rc_service() -> PathBuf {
    binary("rc-service")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_binary_that_exists_resolves_to_an_absolute_path() {
        // `sh` is in `/bin` on every system that can run this crate, so the
        // probe has something real to find.
        let path = binary("sh");
        assert!(path.is_absolute(), "{path:?} is not absolute");
        assert!(path.is_file(), "{path:?} does not exist");
    }

    #[test]
    fn a_missing_binary_falls_back_to_the_bare_name() {
        let path = binary("uflinux-rds-not-a-real-binary");
        assert_eq!(path, PathBuf::from("uflinux-rds-not-a-real-binary"));
    }

    #[test]
    fn repeated_lookups_agree() {
        assert_eq!(binary("sh"), binary("sh"));
    }
}
