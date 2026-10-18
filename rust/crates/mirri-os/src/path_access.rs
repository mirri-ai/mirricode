//! Path access control for the workspace.
//!
//! This module mirrors the TypeScript `path-access.ts` and `sensitive.ts`
//! logic: lexical path normalization (no symlink resolution), workspace
//! containment checks, and sensitive-file detection. All checks are purely
//! lexical — no filesystem access is performed, which avoids TOCTOU
//! vulnerabilities.

use std::path::{Component, Path, PathBuf};

/// Path access control for the workspace.
///
/// Provides lexical path normalization (no symlink resolution), workspace
/// containment checks, and sensitive-file detection. All checks are lexical —
/// no filesystem access is performed, which avoids TOCTOU vulnerabilities.
pub struct PathAccess {
    workspace_root: PathBuf,
}

impl PathAccess {
    /// Create a new `PathAccess` rooted at `workspace_root`.
    pub fn new(workspace_root: PathBuf) -> Self {
        Self { workspace_root }
    }

    /// Lexically normalize a path (no realpath/symlink resolution).
    ///
    /// Resolves `.` and `..` components and collapses multiple slashes. If the
    /// path is relative, it is resolved against the workspace root.
    ///
    /// `..` components that would escape above the lexical root of the
    /// resolved path are simply dropped (they cannot pop the root component),
    /// mirroring the TypeScript implementation's behavior of clamping at the
    /// root rather than escaping it.
    pub fn normalize(&self, path: &Path) -> PathBuf {
        // Relative paths are resolved against the workspace root.
        let resolved: PathBuf = if path.is_relative() {
            self.workspace_root.join(path)
        } else {
            path.to_path_buf()
        };

        let mut normalized = PathBuf::new();
        for component in resolved.components() {
            match component {
                Component::CurDir => {
                    // `.` — skip.
                }
                Component::ParentDir => {
                    // `..` — pop the last component if there is a normal
                    // component to pop. We never pop a root prefix, so `..`
                    // at the lexical root is dropped.
                    let popped_normal = matches!(
                        normalized.components().next_back(),
                        Some(Component::Normal(_))
                    );
                    if popped_normal {
                        normalized.pop();
                    }
                }
                Component::RootDir => {
                    normalized.push(component.as_os_str());
                }
                Component::Prefix(prefix) => {
                    normalized.push(prefix.as_os_str());
                }
                Component::Normal(part) => {
                    normalized.push(part);
                }
            }
        }

        // An empty normalized result (e.g. root with only `..`) collapses to
        // the root itself.
        if normalized.as_os_str().is_empty() {
            normalized.push(Component::RootDir);
        }

        normalized
    }

    /// Check if a normalized path is within the workspace root.
    ///
    /// The check is performed by comparing component sequences rather than
    /// string prefixes, so `/foo/bar` and `/foo/ba` are correctly
    /// distinguished. A path equal to the workspace root is considered
    /// within the workspace.
    pub fn is_within_workspace(&self, path: &Path) -> bool {
        let normalized = self.normalize(path);
        components_start_with(&normalized, &self.workspace_root)
    }

    /// Check if a path points to a sensitive file (secrets, credentials).
    ///
    /// Detection rules (in priority order):
    /// 1. `.env` files — the filename is `.env`, or starts with `.env.` but is
    ///    not one of the known non-sensitive example variants
    ///    (`.env.example`, `.env.sample`, `.env.template`).
    /// 2. SSH private keys — the filename is one of `id_rsa`, `id_ecdsa`,
    ///    `id_ed25519`, `id_dsa` and the path contains a `.ssh` component.
    /// 3. Credential files — the filename is exactly `credentials`, or ends
    ///    with `.pem` / `.key`.
    /// 4. VCS metadata — the path contains a `.git` component.
    pub fn is_sensitive(&self, path: &Path) -> bool {
        let normalized = self.normalize(path);

        let file_name = normalized.file_name().and_then(|n| n.to_str());
        let file_name = match file_name {
            Some(name) => name,
            None => return false,
        };

        // Rule 1: .env files.
        if is_sensitive_env_file(file_name) {
            return true;
        }

        // Rule 2: SSH private keys inside a `.ssh` directory.
        if is_ssh_private_key(file_name) && path_contains_component(&normalized, ".ssh") {
            return true;
        }

        // Rule 3: credential files.
        if file_name == "credentials" {
            return true;
        }
        if file_name.ends_with(".pem") || file_name.ends_with(".key") {
            return true;
        }

        // Rule 4: VCS metadata.
        if path_contains_component(&normalized, ".git") {
            return true;
        }

        false
    }

    /// Check if a path is safe to access: within workspace AND not sensitive.
    ///
    /// Returns `Err` with a descriptive message if the check fails.
    pub fn check_access(&self, path: &Path) -> anyhow::Result<()> {
        let normalized = self.normalize(path);
        if !self.is_within_workspace(&normalized) {
            anyhow::bail!("path '{}' is outside the workspace", path.display());
        }
        if self.is_sensitive(&normalized) {
            anyhow::bail!("path '{}' is a sensitive file", path.display());
        }
        Ok(())
    }
}

/// Return true if `file_name` is a sensitive env file.
///
/// `.env` itself is sensitive; `.env.production` is sensitive; but
/// `.env.example`, `.env.sample`, and `.env.template` are not.
fn is_sensitive_env_file(file_name: &str) -> bool {
    if file_name == ".env" {
        return true;
    }
    if let Some(rest) = file_name.strip_prefix(".env.") {
        // `rest` is the portion after `.env.` — e.g. `production`, `example`.
        // The known non-sensitive example variants are excluded.
        let non_sensitive = ["example", "sample", "template"];
        if non_sensitive.contains(&rest) {
            return false;
        }
        return true;
    }
    false
}

/// Return true if `file_name` is a known SSH private-key basename.
fn is_ssh_private_key(file_name: &str) -> bool {
    matches!(file_name, "id_rsa" | "id_ecdsa" | "id_ed25519" | "id_dsa")
}

/// Return true if `path` contains a normal component equal to `name`.
fn path_contains_component(path: &Path, name: &str) -> bool {
    path.components().any(|component| match component {
        Component::Normal(part) => part == name,
        _ => false,
    })
}

/// Return true if the component sequence of `path` starts with all of the
/// components of `prefix`.
///
/// This is a structural prefix match on path components, not a string prefix
/// match, so `/foo/bar` does not match a prefix of `/foo/ba`.
fn components_start_with(path: &Path, prefix: &Path) -> bool {
    let path_components: Vec<Component<'_>> = path.components().collect();
    let prefix_components: Vec<Component<'_>> = prefix.components().collect();

    if prefix_components.len() > path_components.len() {
        return false;
    }

    prefix_components.iter().zip(path_components.iter()).all(
        |(prefix_component, path_component)| {
            prefix_component.as_os_str() == path_component.as_os_str()
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_flag_real_env_files_as_sensitive_and_skip_example_variants_when_checking_env_filenames()
     {
        assert!(is_sensitive_env_file(".env"));
        assert!(is_sensitive_env_file(".env.production"));
        assert!(!is_sensitive_env_file(".env.example"));
        assert!(!is_sensitive_env_file(".env.sample"));
        assert!(!is_sensitive_env_file(".env.template"));
        assert!(!is_sensitive_env_file("env.txt"));
    }

    #[test]
    fn should_match_known_ssh_private_key_basenames_when_checking_ssh_key_filenames() {
        assert!(is_ssh_private_key("id_rsa"));
        assert!(is_ssh_private_key("id_ecdsa"));
        assert!(is_ssh_private_key("id_ed25519"));
        assert!(is_ssh_private_key("id_dsa"));
        assert!(!is_ssh_private_key("id_rsa.pub"));
        assert!(!is_ssh_private_key("config"));
    }
}
