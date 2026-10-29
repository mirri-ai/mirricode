//! Cross-platform default shell resolution for terminal spawning.
//!
//! [`resolve_shell`] is a pure decision function: every environment input and
//! the PATH-lookup predicate are injected, so the full priority chain
//! (including the Windows pwsh → powershell → cmd fallback) is testable on
//! any host. [`default_shell`] is the thin production wrapper that reads the
//! real environment and consults `PATH`.

/// Resolve the shell program for a new terminal.
///
/// Priority: `explicit` argument > `MIRRI_SHELL_PATH` (canonical) >
/// `MIRRICODE_SHELL_PATH` (legacy alias, same pattern as mirri-core's config
/// `env_layer`) > `$SHELL` > platform fallback (Windows: `pwsh.exe` →
/// `powershell.exe` → `cmd.exe`; otherwise `/bin/sh`).
///
/// Blank or whitespace-only values are skipped at every priority level
/// (mirroring `env_var_non_empty` in mirri-core's config env layer), so an
/// empty `MIRRI_SHELL_PATH` falls through to the alias instead of winning.
///
/// `can_find` decides whether a Windows fallback candidate is actually
/// resolvable (PATH directory scan / absolute-path existence); it is injected
/// so the fallback order is verifiable on non-Windows hosts. When no
/// candidate is findable the final `cmd.exe` is still returned as the last
/// resort — spawning then fails with a diagnosable error rather than a
/// guaranteed-wrong `/bin/sh`.
pub fn resolve_shell(
    explicit: Option<&str>,
    mirri_shell_path: Option<&str>,
    mirricode_shell_path: Option<&str>,
    shell_var: Option<&str>,
    windows: bool,
    can_find: &dyn Fn(&str) -> bool,
) -> String {
    for candidate in [explicit, mirri_shell_path, mirricode_shell_path, shell_var] {
        if let Some(value) = candidate.filter(|s| !s.trim().is_empty()) {
            return value.to_string();
        }
    }
    if windows {
        for prog in ["pwsh.exe", "powershell.exe"] {
            if can_find(prog) {
                return prog.to_string();
            }
        }
        // Last resort even when unfindable: spawning fails with a diagnosable
        // error instead of the guaranteed-wrong Unix `/bin/sh`.
        "cmd.exe".to_string()
    } else {
        "/bin/sh".to_string()
    }
}

/// Production shell resolution: reads the real environment, uses `cfg!(windows)`,
/// and looks Windows fallback candidates up on `PATH`.
///
/// See [`resolve_shell`] for the priority chain.
pub fn default_shell(explicit: Option<&str>) -> String {
    let mirri_shell_path = env_var_non_blank("MIRRI_SHELL_PATH");
    let mirricode_shell_path = env_var_non_blank("MIRRICODE_SHELL_PATH");
    let shell_var = env_var_non_blank("SHELL");
    resolve_shell(
        explicit,
        mirri_shell_path.as_deref(),
        mirricode_shell_path.as_deref(),
        shell_var.as_deref(),
        cfg!(windows),
        &findable_on_path,
    )
}

/// Read an environment variable, treating unset, empty, and whitespace-only
/// values as absent (same semantics as mirri-core's `env_var_non_empty`).
fn env_var_non_blank(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
}

/// Whether a program name resolves on the host: either an absolute path that
/// exists, or a file with that name in some `PATH` directory.
///
/// Only consulted for the Windows fallback candidates; the exact executable
/// lookup rules (PATHEXT &c.) are irrelevant because the candidates carry
/// their `.exe` extension already.
fn findable_on_path(prog: &str) -> bool {
    let path = std::path::Path::new(prog);
    if path.is_absolute() {
        return path.is_file();
    }
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|dir| dir.join(prog).is_file()))
        .unwrap_or(false)
}

/// Resolution outcome for the Bash tool's Windows Git Bash lookup (WO-3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitBashResolution {
    Found(String),
    /// env-var set but points to a non-bash family executable
    EnvVarNotBash { value: String },
    /// env-var set but the path does not exist
    EnvVarMissing { value: String },
    /// nothing resolvable
    Unresolved,
}

/// Resolve the Git Bash executable for the Bash tool on Windows (WO3-R2).
///
/// Priority: `MIRRICODE_SHELL_PATH` (must exist and be bash-family) >
/// `known_locations` (caller-derived: per-git.exe install roots,
/// LOCALAPPDATA prefixes) > `bash.exe` on PATH entries, skipping entries
/// `is_wsl_entry` flags (the WSL System32 launcher, ADR-WO3-6). Pure and
/// parameterized so the chain is testable on any host.
pub fn resolve_git_bash_from(
    env_var: Option<&str>,
    known_locations: &[std::path::PathBuf],
    path_dirs: &[std::path::PathBuf],
    is_wsl_entry: &dyn Fn(&std::path::Path) -> bool,
    exists: &dyn Fn(&std::path::Path) -> bool,
) -> GitBashResolution {
    use std::path::Path;
    if let Some(value) = env_var {
        let path = Path::new(value);
        if !exists(path) {
            return GitBashResolution::EnvVarMissing {
                value: value.to_string(),
            };
        }
        let is_bash = path
            .file_name()
            .map(|n| n.to_string_lossy().to_lowercase().contains("bash"))
            .unwrap_or(false);
        if !is_bash {
            return GitBashResolution::EnvVarNotBash {
                value: value.to_string(),
            };
        }
        return GitBashResolution::Found(value.to_string());
    }
    for known in known_locations {
        if exists(known) {
            return GitBashResolution::Found(known.to_string_lossy().into_owned());
        }
    }
    for dir in path_dirs {
        let candidate = dir.join("bash.exe");
        if is_wsl_entry(&candidate) {
            continue;
        }
        if exists(&candidate) {
            return GitBashResolution::Found(candidate.to_string_lossy().into_owned());
        }
    }
    GitBashResolution::Unresolved
}

/// Derive the known Git Bash locations from PATH entries and LOCALAPPDATA
/// (WO3-R2 / ADR-WO3-4).
///
/// Order: the `LOCALAPPDATA\Programs\Git\bin\bash.exe` prefix first, then for
/// every PATH directory whose `git.exe` exists the install-root-derived
/// `bin\bash.exe` candidates (`git.exe` usually lives in `<root>\cmd\` or
/// `<root>\bin\`, so both the parent and the directory itself are tried as
/// roots). Pure and parameterized so the derivation is testable on any host.
pub fn git_bash_known_locations_from(
    path_dirs: &[std::path::PathBuf],
    local_appdata: Option<&str>,
    exists: &dyn Fn(&std::path::Path) -> bool,
) -> Vec<std::path::PathBuf> {
    let mut known: Vec<std::path::PathBuf> = Vec::new();
    if let Some(local) = local_appdata {
        known.push(
            std::path::PathBuf::from(local)
                .join("Programs")
                .join("Git")
                .join("bin")
                .join("bash.exe"),
        );
    }
    for dir in path_dirs {
        let git = dir.join("git.exe");
        if exists(&git) {
            // git.exe usually lives in <root>\cmd\ or <root>\bin\ — the bash
            // is <root>\bin\bash.exe
            for root in [dir.parent().map(|p| p.to_path_buf()), Some(dir.clone())]
                .into_iter()
                .flatten()
            {
                known.push(root.join("bin").join("bash.exe"));
            }
        }
    }
    known
}

/// Actionable error when Git Bash cannot be resolved for the Bash tool.
#[cfg(windows)]
#[derive(Debug, Clone)]
pub struct GitBashError(pub String);

#[cfg(windows)]
impl std::fmt::Display for GitBashError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Production Git Bash resolution for the Bash tool (Windows only).
///
/// Success results are cached per-process; failures re-probe each call
/// (ADR-WO3-2: install-then-run works without a restart).
#[cfg(windows)]
pub fn git_bash_for_bash_tool() -> Result<String, GitBashError> {
    use std::sync::Mutex;
    // Cache: key includes the current MIRRICODE_SHELL_PATH value so a
    // mid-session env change invalidates.
    static CACHE: Mutex<Option<(String, String)>> = Mutex::new(None); // (env_var_snapshot, path)
    let env_snapshot = env_var_non_blank("MIRRICODE_SHELL_PATH").unwrap_or_default();
    if let Ok(cache) = CACHE.lock() {
        if let Some((cached_env, cached_path)) = cache.as_ref() {
            if *cached_env == env_snapshot {
                return Ok(cached_path.clone());
            }
        }
    }
    match resolve_git_bash_production() {
        GitBashResolution::Found(path) => {
            if let Ok(mut cache) = CACHE.lock() {
                *cache = Some((env_snapshot, path.clone()));
            }
            Ok(path)
        }
        GitBashResolution::EnvVarMissing { value } => Err(GitBashError(format!(
            "MIRRICODE_SHELL_PATH points to '{value}' which does not exist. \
             Fix the variable or clear it to use auto-detected Git Bash."
        ))),
        GitBashResolution::EnvVarNotBash { value } => Err(GitBashError(format!(
            "MIRRICODE_SHELL_PATH points to '{value}' which is not a bash-family \
             shell. Point it at bash.exe for the Bash tool, or use the PowerShell tool."
        ))),
        GitBashResolution::Unresolved => Err(GitBashError(
            "Git Bash was not found. Install Git for Windows \
             (https://gitforwindows.org/), or set MIRRICODE_SHELL_PATH to \
             bash.exe, or use the PowerShell tool for native Windows commands."
                .to_string(),
        )),
    }
}

/// Production resolution: reads the real environment and the filesystem,
/// then delegates to the pure [`resolve_git_bash_from`] chain.
#[cfg(windows)]
fn resolve_git_bash_production() -> GitBashResolution {
    use std::path::{Path, PathBuf};
    let env_var = env_var_non_blank("MIRRICODE_SHELL_PATH");
    let path_dirs: Vec<PathBuf> = env_var_non_blank("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    let known = git_bash_known_locations_from(
        &path_dirs,
        env_var_non_blank("LOCALAPPDATA").as_deref(),
        &|p: &Path| p.is_file(),
    );
    // ADR-WO3-6: case-insensitive, separator-normalized comparison — a
    // bare PathBuf eq misses `C:\WINDOWS\System32` and slash variants.
    let system_root = std::env::var_os("SystemRoot")
        .map(|s| s.to_string_lossy().to_lowercase())
        .unwrap_or_else(|| "c:\\windows".to_string());
    let is_wsl_entry = move |p: &Path| -> bool {
        let normalize = |q: &Path| q.to_string_lossy().to_lowercase().replace('/', "\\");
        p.parent()
            .map(normalize)
            .is_some_and(|d| d == format!("{system_root}\\system32"))
    };
    let exists = |p: &Path| p.is_file();
    resolve_git_bash_from(
        env_var.as_deref(),
        &known,
        &path_dirs,
        &is_wsl_entry,
        &exists,
    )
}

/// How the PowerShell tool's `MIRRICODE_SHELL_PATH` consumption decides
/// (ADR-WO3-4's family dispatch, third family rule).
///
/// Returns:
/// - `Ok(Some(value))` — the value is PowerShell-family and exists: use it.
/// - `Ok(None)` — unset, or a bash-family value: bash.exe legitimately
///   configures the terminal pool and the Bash tool, not the PowerShell
///   tool — fall through to PATH probing.
/// - `Err(..)` — PowerShell-family but missing (a configured-but-missing
///   path is a hard error, the same signal-truthfulness rule the Bash side
///   applies), or neither bash- nor PowerShell-family: the value was not
///   adopted for this tool and the error says so.
///
/// Pure and parameterized (the existence predicate is injected) so the
/// dispatch is testable on any host.
pub fn powershell_env_dispatch(
    env_var: Option<&str>,
    exists: &dyn Fn(&std::path::Path) -> bool,
) -> Result<Option<String>, String> {
    let Some(value) = env_var else {
        return Ok(None);
    };
    let path = std::path::Path::new(value);
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    if name.contains("pwsh") || name.contains("powershell") {
        if exists(path) {
            return Ok(Some(value.to_string()));
        }
        return Err(format!(
            "MIRRICODE_SHELL_PATH points to '{value}' which does not exist. \
             Fix the variable or clear it to use auto-detected PowerShell."
        ));
    }
    if name.contains("bash") {
        return Ok(None);
    }
    Err(format!(
        "MIRRICODE_SHELL_PATH points to '{value}' which is not a \
         PowerShell-family shell. Point it at pwsh.exe/powershell.exe for the \
         PowerShell tool, or clear it to use auto-detected PowerShell."
    ))
}

/// Production PowerShell resolution for the PowerShell tool (Windows only,
/// WO3-R4/R5): `MIRRICODE_SHELL_PATH` family dispatch (ADR-WO3-4) >
/// `pwsh.exe` on PATH > `powershell.exe` on PATH.
///
/// A set `MIRRICODE_SHELL_PATH` whose filename is PowerShell-family must
/// exist (a configured-but-missing path is a hard error). A bash-family
/// value is not consumed here — it configures the terminal pool and the
/// Bash tool — and neither-bash-nor-PS values error, per ADR-WO3-4's third
/// family rule. Failures re-probe each call (no cache): PowerShell is
/// preinstalled on every supported Windows host, so there is no
/// install-then-run restart concern like Git Bash's (ADR-WO3-2's cache
/// rationale does not transfer).
#[cfg(windows)]
pub fn powershell_exe_for_tool() -> Result<String, String> {
    match powershell_env_dispatch(
        env_var_non_blank("MIRRICODE_SHELL_PATH").as_deref(),
        &|p: &std::path::Path| p.is_file(),
    ) {
        Ok(Some(shell)) => return Ok(shell),
        Ok(None) => {}
        Err(error) => return Err(error),
    }
    for candidate in ["pwsh.exe", "powershell.exe"] {
        let hit = std::env::var_os("PATH")
            .map(|p| std::env::split_paths(&p).any(|d| d.join(candidate).is_file()))
            .unwrap_or(false);
        if hit {
            return Ok(candidate.to_string());
        }
    }
    Err("PowerShell was not found on PATH. Install PowerShell 7 or verify \
         C:\\Windows\\System32\\WindowsPowerShell is on PATH."
        .to_string())
}

#[cfg(test)]
mod tests {
    use super::resolve_shell;

    #[test]
    fn should_prefer_explicit_shell_over_all_env_overrides() {
        let resolved = resolve_shell(
            Some("/usr/local/bin/fish"),
            Some("/opt/zsh"),
            Some("/opt/bash"),
            Some("/bin/sh"),
            false,
            &|_| true,
        );
        assert_eq!(resolved, "/usr/local/bin/fish");
    }

    #[test]
    fn should_prefer_mirri_shell_path_over_legacy_alias_and_shell_env() {
        let resolved = resolve_shell(
            None,
            Some("/opt/mirri-sh"),
            Some("/opt/legacy-sh"),
            Some("/bin/bash"),
            false,
            &|_| true,
        );
        assert_eq!(resolved, "/opt/mirri-sh");
    }

    #[test]
    fn should_prefer_legacy_alias_over_shell_env_when_canonical_unset() {
        let resolved = resolve_shell(
            None,
            None,
            Some("/opt/legacy-sh"),
            Some("/bin/bash"),
            false,
            &|_| true,
        );
        assert_eq!(resolved, "/opt/legacy-sh");
    }

    #[test]
    fn should_skip_blank_overrides_and_honor_shell_env_on_windows() {
        let resolved = resolve_shell(
            None,
            Some("   "),
            Some(""),
            Some("C:/tools/bash.exe"),
            true,
            &|_| true,
        );
        assert_eq!(resolved, "C:/tools/bash.exe");
    }

    #[test]
    fn should_fall_back_to_powershell_when_windows_and_pwsh_not_findable() {
        let resolved = resolve_shell(None, None, None, None, true, &|prog| prog != "pwsh.exe");
        assert_eq!(resolved, "powershell.exe");
    }

    #[test]
    fn should_fall_back_to_cmd_exe_when_no_windows_shell_is_findable() {
        let resolved = resolve_shell(None, None, None, None, true, &|_| false);
        assert_eq!(resolved, "cmd.exe");
    }

    #[test]
    fn should_fall_back_to_bin_sh_on_unix_without_any_env() {
        let resolved = resolve_shell(None, None, None, None, false, &|_| true);
        assert_eq!(resolved, "/bin/sh");
    }

    // ===== Git Bash resolution (WO-3) tests =====

    use super::{resolve_git_bash_from, GitBashResolution};
    use std::path::{Path, PathBuf};

    fn no_wsl(_p: &Path) -> bool {
        false
    }
    fn always_exists(_p: &Path) -> bool {
        true
    }
    fn never_exists(_p: &Path) -> bool {
        false
    }

    #[test]
    fn should_use_env_var_when_it_points_to_an_existing_bash() {
        let r = resolve_git_bash_from(
            Some(r"C:\tools\git\bin\bash.exe"),
            &[],
            &[],
            &no_wsl,
            &always_exists,
        );
        assert!(
            matches!(r, GitBashResolution::Found(p) if p == r"C:\tools\git\bin\bash.exe")
        );
    }

    #[test]
    fn should_report_env_var_missing_when_the_configured_path_does_not_exist() {
        let r = resolve_git_bash_from(
            Some(r"C:\nope\bash.exe"),
            &[],
            &[],
            &no_wsl,
            &never_exists,
        );
        assert!(matches!(r, GitBashResolution::EnvVarMissing { .. }));
    }

    #[test]
    fn should_report_env_var_not_bash_when_it_points_to_powershell() {
        let r = resolve_git_bash_from(
            Some(r"C:\tools\pwsh.exe"),
            &[],
            &[],
            &no_wsl,
            &always_exists,
        );
        assert!(matches!(r, GitBashResolution::EnvVarNotBash { .. }));
    }

    #[test]
    fn should_use_known_location_before_path_when_env_var_is_absent() {
        let known = [PathBuf::from(r"C:\Program Files\Git\bin\bash.exe")];
        let path_dirs = [PathBuf::from(r"C:\Windows\System32")];
        let r = resolve_git_bash_from(None, &known, &path_dirs, &no_wsl, &always_exists);
        assert!(
            matches!(r, GitBashResolution::Found(p)
                if p == r"C:\Program Files\Git\bin\bash.exe")
        );
    }

    #[test]
    fn should_skip_the_wsl_entry_and_take_a_later_path_hit_when_scanning_path() {
        // Forward-slash Windows-style paths keep `Path` component semantics
        // identical on every host (backslashes are not separators on Unix).
        let system32 = PathBuf::from("C:/Windows/System32");
        let scoop = PathBuf::from("C:/Users/u/scoop/shims");
        let is_wsl = |p: &Path| p.starts_with("C:/Windows/System32");
        // exists() only true for the scoop one
        let exists = |p: &Path| p.starts_with("C:/Users/u/scoop");
        let r = resolve_git_bash_from(None, &[], &[system32, scoop], &is_wsl, &exists);
        assert!(
            matches!(r, GitBashResolution::Found(p)
                if p == "C:/Users/u/scoop/shims/bash.exe")
        );
    }

    #[test]
    fn should_return_unresolved_when_nothing_matches() {
        let r = resolve_git_bash_from(None, &[], &[], &no_wsl, &never_exists);
        assert!(matches!(r, GitBashResolution::Unresolved));
    }

    #[test]
    fn should_derive_known_locations_from_git_exe_roots_and_localappdata() {
        use super::git_bash_known_locations_from;
        // Forward-slash Windows-style paths keep `Path` component semantics
        // identical on every host (backslashes are not separators on Unix).
        let dirs = vec![
            PathBuf::from("C:/Program Files/Git/cmd"), // git.exe here → root C:/Program Files/Git
            PathBuf::from("C:/other"),
        ];
        let exists = |p: &Path| p.to_string_lossy().contains("Git");
        let known = git_bash_known_locations_from(&dirs, Some("C:/Users/u/AppData/Local"), &exists);
        assert_eq!(
            known[0],
            PathBuf::from("C:/Users/u/AppData/Local/Programs/Git/bin/bash.exe")
        );
        assert!(known.contains(&PathBuf::from("C:/Program Files/Git/bin/bash.exe")));
        assert!(!known
            .iter()
            .any(|p| p.to_string_lossy().starts_with("C:/other")));
    }

    // ===== PowerShell tool `MIRRICODE_SHELL_PATH` family dispatch (WO-3,
    // ADR-WO3-4) tests =====

    use super::powershell_env_dispatch;

    #[test]
    fn should_adopt_the_env_var_when_it_points_to_an_existing_powershell() {
        let r = powershell_env_dispatch(Some(r"C:\tools\pwsh.exe"), &always_exists);
        assert_eq!(r, Ok(Some(r"C:\tools\pwsh.exe".to_string())));
    }

    #[test]
    fn should_report_the_env_var_missing_when_a_powershell_path_does_not_exist() {
        let r = powershell_env_dispatch(Some(r"C:\nope\pwsh.exe"), &never_exists);
        assert!(r.unwrap_err().contains("does not exist"));
    }

    #[test]
    fn should_fall_through_to_path_probing_when_the_env_var_points_to_bash() {
        // A bash-family value configures the terminal pool and the Bash
        // tool — the PowerShell tool must not consume it, and must not
        // error either (ADR-WO3-4: only the third family errors here).
        let r = powershell_env_dispatch(Some(r"C:\Program Files\Git\bin\bash.exe"), &always_exists);
        assert_eq!(r, Ok(None));
    }

    #[test]
    fn should_error_when_the_env_var_is_neither_bash_nor_powershell_family() {
        let r = powershell_env_dispatch(Some(r"C:\tools\zsh.exe"), &always_exists);
        let error = r.unwrap_err();
        assert!(error.contains("not a PowerShell-family shell"));
        assert!(error.contains("pwsh.exe/powershell.exe"));
        assert!(error.contains("zsh.exe"));
    }

    #[test]
    fn should_fall_through_to_path_probing_when_the_env_var_is_unset() {
        let r = powershell_env_dispatch(None, &never_exists);
        assert_eq!(r, Ok(None));
    }
}
