//! Mirri home directory resolution.

use super::*;

/// Locate the Mirri data directory, honouring the `MIRRICODE_HOME`
/// environment variable.
///
/// When `MIRRICODE_HOME` is set to a non-empty value it is used verbatim;
/// otherwise the directory is `$HOME/.mirri-code`.
pub fn mirri_home_dir() -> EngineResult<PathBuf> {
    resolve_mirri_home_dir_from(|name| std::env::var_os(name))
}

/// Pure resolver behind [`mirri_home_dir`], parameterised over the env
/// lookup so the precedence rules are testable without mutating the process
/// environment.
pub fn resolve_mirri_home_dir_from(
    get_env: impl Fn(&str) -> Option<std::ffi::OsString>,
) -> EngineResult<PathBuf> {
    // MIRRICODE_HOME wins when set to a non-empty value; an empty value is
    // treated as unset so it falls through to the HOME-based default.
    if let Some(mirri_home) = get_env("MIRRICODE_HOME").filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(mirri_home));
    }
    let home = get_env("HOME").ok_or_else(|| {
        EngineError::RequestInvalid(
            "HOME is not set; cannot locate the ~/.mirri-code data directory".to_string(),
        )
    })?;
    Ok(PathBuf::from(home).join(".mirri-code"))
}
