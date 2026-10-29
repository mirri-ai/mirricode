//! User home directory resolution — the single OS-facing seam for "where
//! is the user's home" questions (design: platform-specific home lookup is
//! OS-layer plumbing, so every crate funnels through this module instead
//! of reading env vars or `dirs::home_dir()` on its own).
//!
//! Two questions exist and both have exactly one answer here:
//!
//! - [`os_home_dir`]: the OS user home (`$HOME`, falling back to
//!   `$USERPROFILE` on Windows where `HOME` does not exist). Used for
//!   user-level paths that follow the real user, not the product.
//! - [`mirri_home_dir`]: the product's data home (`$MIRRICODE_HOME` when
//!   set, else `<os_home_dir>/.mirri-code`).
//!
//! Empty values are treated as unset throughout: a set-but-empty
//! `MIRRICODE_HOME` falls through to the home-based default rather than
//! resolving to the process CWD (the empty-string trap).

use std::ffi::OsString;
use std::path::PathBuf;

/// The OS user home directory.
///
/// Resolution order: `HOME` (non-empty) → `USERPROFILE` (non-empty). On
/// Unix only `HOME` exists in practice; on a plain Windows environment only
/// `USERPROFILE` does (Git Bash exports `HOME` itself, and it must keep
/// winning when both are present). Returns `None` when neither is set.
pub fn os_home_dir() -> Option<PathBuf> {
    resolve_os_home_dir_from(|name| std::env::var_os(name))
}

/// The product data home directory.
///
/// `MIRRICODE_HOME` (non-empty) is used verbatim; otherwise the directory
/// is `<os home>/.mirri-code`. Returns `None` when neither can be resolved.
pub fn mirri_home_dir() -> Option<PathBuf> {
    resolve_mirri_home_dir_from(|name| std::env::var_os(name))
}

/// Pure resolver behind [`os_home_dir`], parameterised over the env lookup
/// so the precedence rules are testable without mutating the process
/// environment.
pub fn resolve_os_home_dir_from(
    get_env: impl Fn(&str) -> Option<OsString>,
) -> Option<PathBuf> {
    get_env("HOME")
        .filter(|value| !value.is_empty())
        .or_else(|| get_env("USERPROFILE").filter(|value| !value.is_empty()))
        .map(PathBuf::from)
}

/// Pure resolver behind [`mirri_home_dir`], parameterised over the env
/// lookup (see [`resolve_os_home_dir_from`]).
pub fn resolve_mirri_home_dir_from(
    get_env: impl Fn(&str) -> Option<OsString>,
) -> Option<PathBuf> {
    if let Some(mirri_home) = get_env("MIRRICODE_HOME").filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(mirri_home));
    }
    resolve_os_home_dir_from(get_env).map(|home| home.join(".mirri-code"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_env<'a>(entries: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<OsString> + 'a {
        move |name| {
            entries
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| OsString::from(*value))
        }
    }

    #[test]
    fn should_return_home_when_only_home_is_set() {
        let dir = resolve_os_home_dir_from(fake_env(&[("HOME", "/home/user")]))
            .expect("resolution should succeed");
        assert_eq!(dir, PathBuf::from("/home/user"));
    }

    #[test]
    fn should_fall_back_to_userprofile_when_home_is_not_set() {
        // The default Windows environment: no HOME, only USERPROFILE
        let dir = resolve_os_home_dir_from(fake_env(&[("USERPROFILE", r"C:\Users\user")]))
            .expect("resolution should succeed");
        assert_eq!(dir, PathBuf::from(r"C:\Users\user"));
    }

    #[test]
    fn should_prefer_home_over_userprofile_when_both_are_set() {
        // Git Bash on Windows exports HOME itself — it must keep winning
        let dir = resolve_os_home_dir_from(fake_env(&[
            ("HOME", "/home/user"),
            ("USERPROFILE", r"C:\Users\user"),
        ]))
        .expect("resolution should succeed");
        assert_eq!(dir, PathBuf::from("/home/user"));
    }

    #[test]
    fn should_return_none_when_home_and_userprofile_are_both_missing() {
        assert!(resolve_os_home_dir_from(fake_env(&[])).is_none());
    }

    #[test]
    fn should_treat_an_empty_home_as_unset_when_resolving_os_home() {
        assert!(
            resolve_os_home_dir_from(fake_env(&[("HOME", "")])).is_none(),
            "an empty HOME must not resolve to the process CWD"
        );
    }

    #[test]
    fn should_treat_an_empty_userprofile_as_unset_when_resolving_os_home() {
        assert!(resolve_os_home_dir_from(fake_env(&[("USERPROFILE", "")])).is_none());
    }

    #[test]
    fn should_use_mirricode_home_verbatim_when_set() {
        let dir = resolve_mirri_home_dir_from(fake_env(&[
            ("MIRRICODE_HOME", "/custom/mirri"),
            ("HOME", "/home/user"),
        ]))
        .expect("resolution should succeed");
        assert_eq!(dir, PathBuf::from("/custom/mirri"));
    }

    #[test]
    fn should_treat_an_empty_mirricode_home_as_unset() {
        let dir = resolve_mirri_home_dir_from(fake_env(&[
            ("MIRRICODE_HOME", ""),
            ("HOME", "/home/user"),
        ]))
        .expect("resolution should succeed");
        assert_eq!(dir, PathBuf::from("/home/user/.mirri-code"));
    }

    #[test]
    fn should_derive_the_brand_home_from_the_os_home_when_mirricode_home_is_absent() {
        let dir = resolve_mirri_home_dir_from(fake_env(&[("USERPROFILE", r"C:\Users\user")]))
            .expect("resolution should succeed");
        assert_eq!(
            dir,
            PathBuf::from(r"C:\Users\user").join(".mirri-code")
        );
    }

    #[test]
    fn should_return_none_for_the_brand_home_when_no_home_variable_exists() {
        assert!(resolve_mirri_home_dir_from(fake_env(&[])).is_none());
    }
}
