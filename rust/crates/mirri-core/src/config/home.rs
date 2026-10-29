//! Mirri home directory resolution.
//!
//! Thin engine-layer wrapper over the OS-layer seam
//! ([`mirri_os::home`](mirri_os::home)): the precedence rules themselves
//! live in `mirri-os` so every crate shares one implementation; this module
//! only maps an unresolvable home to [`EngineError`] (the error type cannot
//! cross the crate boundary into `mirri-os`).

use super::*;

/// Locate the Mirri data directory, honouring the `MIRRICODE_HOME`
/// environment variable.
///
/// When `MIRRICODE_HOME` is set to a non-empty value it is used verbatim;
/// otherwise the directory is `$HOME/.mirri-code` (or
/// `%USERPROFILE%\.mirri-code` when `HOME` is unset — the default Windows
/// environment has no `HOME`). Resolution itself is delegated to
/// [`mirri_os::mirri_home_dir`].
pub fn mirri_home_dir() -> EngineResult<PathBuf> {
    mirri_os::mirri_home_dir().ok_or_else(unresolved_home_error)
}

/// The error an unresolvable home maps to (serve exits on this; exposed
/// separately so tests can assert the mapping without mutating the
/// process environment).
pub(crate) fn unresolved_home_error() -> EngineError {
    EngineError::RequestInvalid(
        "HOME is not set; cannot locate the ~/.mirri-code data directory".to_string(),
    )
}
