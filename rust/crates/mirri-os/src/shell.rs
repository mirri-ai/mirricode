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
}
