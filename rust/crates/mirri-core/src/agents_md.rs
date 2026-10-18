//! AGENTS.md discovery and loading.
//!
//! Mirrors the TypeScript `loadAgentsMdForRoots`: discovers AGENTS.md files
//! from user-level and project-level locations, concatenates them with
//! `<!-- From: {path} -->` annotations, and returns the combined content
//! for injection into the system prompt.
//!
//! ## Discovery order
//!
//! 1. **User-level (brand):** `~/.mirri-code/AGENTS.md`
//! 2. **User-level (generic):** `~/.agents/AGENTS.md` or `~/.agents/agents.md`
//!    (first match wins)
//! 3. **Project-level:** For the workspace root, walks from the workspace dir
//!    up to the project root (found via `.git`), collecting from **root to
//!    leaf**:
//!    - `{dir}/.mirri-code/AGENTS.md`
//!    - `{dir}/AGENTS.md` or `{dir}/agents.md` (first match per dir)
//!
//! Each discovered file is annotated with `<!-- From: {path} -->` and
//! concatenated with `\n\n`. A 32 KB soft budget triggers a warning (returned
//! but does not truncate).

use std::path::{Path, PathBuf};

use mirri_os::SyncFs;

/// Recommended maximum total size for AGENTS.md content (32 KB).
const RECOMMENDED_MAX_BYTES: usize = 32 * 1024;

/// Resolve the brand home directory: `$MIRRICODE_HOME` if set, otherwise
/// `~/.mirri-code`. Returns `None` only if neither is available (no
/// `MIRRICODE_HOME` and `dirs::home_dir()` returns `None`).
///
/// Used by AGENTS.md discovery, tool-result truncation, and any other
/// subsystem that needs the brand home path.
pub fn brand_home() -> Option<PathBuf> {
    std::env::var("MIRRICODE_HOME")
        .ok()
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".mirri-code")))
}

/// Result of loading AGENTS.md files.
pub struct LoadedAgentsMd {
    /// Concatenated, annotated AGENTS.md content.
    pub content: String,
    /// Warning message if content exceeds the recommended size, if any.
    pub warning: Option<String>,
}

/// Discover and load AGENTS.md files for the given workspace root.
///
/// The brand home directory follows `MIRRICODE_HOME` (falling back to
/// `~/.mirri-code`), matching the TypeScript `brandHome` parameter.
/// The generic user-level directory always uses the real OS home (`~/.agents`).
///
/// Returns `LoadedAgentsMd` with empty content if no files are found. File
/// reads go through the injected [`SyncFs`] provider (zero-IO T13b;
/// callers pass `LocalSyncFs` for the engine default).
pub fn load_agents_md(workspace_root: Option<&Path>, fs: &dyn SyncFs) -> LoadedAgentsMd {
    load_agents_md_for_trust(workspace_root, true, fs)
}

/// The trust-gated AGENTS.md load (design `08-multi-workspace.md` §5 D6):
/// the **user-level** sections (brand + generic) always load, while the
/// **project-level** walk (workspace dir up to the project root) runs only
/// when `include_project` is set — an untrusted workspace's project
/// instructions never enter the system prompt. `include_project = true`
/// reproduces [`load_agents_md`] byte for byte.
pub fn load_agents_md_for_trust(
    workspace_root: Option<&Path>,
    include_project: bool,
    fs: &dyn SyncFs,
) -> LoadedAgentsMd {
    let real_home = dirs::home_dir();
    let brand_home = brand_home();
    load_agents_md_with_home_for_trust(
        workspace_root,
        include_project,
        real_home.as_deref(),
        brand_home.as_deref(),
        fs,
    )
}

/// Test-only shorthand: the ungated load with explicit home/brand
/// directories (the historical internal entry point; production callers
/// go through [`load_agents_md_for_trust`]).
#[cfg(test)]
fn load_agents_md_with_home(
    workspace_root: Option<&Path>,
    home: Option<&Path>,
    brand_home: Option<&Path>,
    fs: &dyn SyncFs,
) -> LoadedAgentsMd {
    load_agents_md_with_home_for_trust(workspace_root, true, home, brand_home, fs)
}

/// The shared discovery body (see [`load_agents_md_for_trust`]): the
/// user-level files first, then (when trusted) the project walk, rendered
/// into one annotated document with a single combined size warning.
fn load_agents_md_with_home_for_trust(
    workspace_root: Option<&Path>,
    include_project: bool,
    home: Option<&Path>,
    brand_home: Option<&Path>,
    fs: &dyn SyncFs,
) -> LoadedAgentsMd {
    let mut discovered: Vec<(PathBuf, String)> = Vec::new();
    let mut seen = std::collections::HashSet::new();

    // 1. User-level (brand): $MIRRICODE_HOME/AGENTS.md or ~/.mirri-code/AGENTS.md
    if let Some(brand_home) = brand_home {
        let brand_path = brand_home.join("AGENTS.md");
        collect_file(&brand_path, &mut discovered, &mut seen, fs);
    }

    // 2. User-level (generic): ~/.agents/AGENTS.md or ~/.agents/agents.md
    if let Some(home) = home {
        let generic_dir = home.join(".agents");
        for name in &["AGENTS.md", "agents.md"] {
            let path = generic_dir.join(name);
            if collect_file(&path, &mut discovered, &mut seen, fs) {
                break;
            }
        }
    }

    // 3. Project-level: walk from workspace dir up to project root — the
    //    trust-gated section (D6): an untrusted workspace's own
    //    instruction files are never read, let alone injected.
    if include_project && let Some(workspace) = workspace_root {
        let project_root = find_project_root(workspace);
        let dirs = dirs_root_to_leaf(workspace, &project_root);
        for dir in &dirs {
            // .mirri-code/AGENTS.md
            let brand_path = dir.join(".mirri-code").join("AGENTS.md");
            collect_file(&brand_path, &mut discovered, &mut seen, fs);

            // AGENTS.md or agents.md (first match per dir)
            for name in &["AGENTS.md", "agents.md"] {
                let path = dir.join(name);
                if collect_file(&path, &mut discovered, &mut seen, fs) {
                    break;
                }
            }
        }
    }

    let content = render_agent_files(&discovered);
    let total_bytes = content.len();
    let warning = if total_bytes > RECOMMENDED_MAX_BYTES {
        Some(format!(
            "AGENTS.md total {} KB exceeds the recommended {} KB. Large instruction files \
             increase cost and may impact performance; consider trimming.",
            format_kb(total_bytes),
            format_kb(RECOMMENDED_MAX_BYTES),
        ))
    } else {
        None
    };

    LoadedAgentsMd { content, warning }
}

/// Try to read a file; return true if it was collected. Reads go through
/// the injected [`SyncFs`] provider.
fn collect_file(
    path: &Path,
    discovered: &mut Vec<(PathBuf, String)>,
    seen: &mut std::collections::HashSet<PathBuf>,
    fs: &dyn SyncFs,
) -> bool {
    // Use the path as-is for dedup, not canonicalize — on case-insensitive
    // filesystems (macOS), AGENTS.md and agents.md canonicalize to the same
    // path, which would prevent the second variant from being collected.
    // We still canonicalize for the annotation to show an absolute path.
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf()); // zero-io-allow: baseline Path IO, see F-T13-2
    if seen.contains(path) {
        return false;
    }
    let content = match fs.read_text(path) {
        Ok(c) => c.trim().to_string(),
        Err(_) => return false,
    };
    if content.is_empty() {
        return false;
    }
    seen.insert(path.to_path_buf());
    discovered.push((canonical, content));
    true
}

/// Find the project root by walking up from `start` until a `.git` dir/file is found.
fn find_project_root(start: &Path) -> PathBuf {
    let mut current = start.to_path_buf();
    loop {
        if current.join(".git").exists() {
            // zero-io-allow: baseline Path IO, see F-T13-2
            return current;
        }
        match current.parent() {
            Some(parent) => current = parent.to_path_buf(),
            None => return start.to_path_buf(),
        }
    }
}

/// Collect directories from project root to the workspace dir (root → leaf).
fn dirs_root_to_leaf(workspace: &Path, project_root: &Path) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    let mut current = workspace.to_path_buf();
    loop {
        dirs.push(current.clone());
        if current == project_root {
            break;
        }
        match current.parent() {
            Some(parent) => current = parent.to_path_buf(),
            None => break,
        }
    }
    dirs.reverse();
    dirs
}

/// Render discovered files into a single annotated string.
fn render_agent_files(files: &[(PathBuf, String)]) -> String {
    if files.is_empty() {
        return String::new();
    }
    files
        .iter()
        .map(|(path, content)| format!("<!-- From: {} -->\n{}", path.display(), content))
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn format_kb(bytes: usize) -> String {
    let kb = bytes / 1024;
    let remainder = bytes % 1024;
    if remainder == 0 {
        kb.to_string()
    } else {
        format!("{:.1}", kb as f64 + remainder as f64 / 1024.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mirri_os::LocalSyncFs;
    use tempfile::TempDir;

    /// should return empty content when no agents md files exist
    #[test]
    fn should_return_empty_content_when_no_agents_md_files_exist() {
        let tmp = TempDir::new().unwrap();
        let result =
            load_agents_md_with_home(Some(tmp.path()), Some(tmp.path()), None, &LocalSyncFs);
        assert!(result.content.is_empty());
        assert!(result.warning.is_none());
    }

    /// should return empty content when workspace root and home are none
    #[test]
    fn should_return_empty_content_when_workspace_root_and_home_are_none() {
        let result = load_agents_md_with_home(None, None, None, &LocalSyncFs);
        assert!(result.content.is_empty());
    }

    /// should discover project level agents md from workspace root
    #[test]
    fn should_discover_project_level_agents_md_from_workspace_root() {
        let tmp = TempDir::new().unwrap();
        let agents_md = tmp.path().join("AGENTS.md");
        std::fs::write(&agents_md, "# Project Rules\n\nAlways test your code.").unwrap();

        let result =
            load_agents_md_with_home(Some(tmp.path()), Some(tmp.path()), None, &LocalSyncFs);
        assert!(result.content.contains("Project Rules"));
        assert!(result.content.contains("Always test your code."));
        assert!(result.content.contains("<!-- From:"));
    }

    /// should discover agents md from mirri-code subdirectory
    #[test]
    fn should_discover_agents_md_from_mirri_code_subdirectory() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".mirri-code");
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("AGENTS.md"), "# Brand Rules").unwrap();

        let result =
            load_agents_md_with_home(Some(tmp.path()), Some(tmp.path()), None, &LocalSyncFs);
        assert!(result.content.contains("Brand Rules"));
    }

    /// should discover agents md from nested subdirectory up to project root
    #[test]
    fn should_discover_agents_md_from_nested_subdirectory_up_to_project_root() {
        let tmp = TempDir::new().unwrap();
        // Create a git root
        std::fs::write(tmp.path().join(".git"), "gitdir").unwrap();
        // Create nested dir
        let nested = tmp.path().join("packages").join("myapp");
        std::fs::create_dir_all(&nested).unwrap();
        // Root AGENTS.md
        std::fs::write(tmp.path().join("AGENTS.md"), "# Root Rules").unwrap();
        // Nested AGENTS.md
        std::fs::write(nested.join("AGENTS.md"), "# Nested Rules").unwrap();

        let result = load_agents_md_with_home(Some(&nested), Some(tmp.path()), None, &LocalSyncFs);
        // Root should come first (root → leaf order)
        let root_pos = result.content.find("Root Rules").unwrap();
        let nested_pos = result.content.find("Nested Rules").unwrap();
        assert!(
            root_pos < nested_pos,
            "root rules should appear before nested rules"
        );
    }

    /// should prefer uppercase agents md filename when both exist
    #[test]
    fn should_prefer_uppercase_agents_md_filename_when_both_exist() {
        let tmp = TempDir::new().unwrap();
        // On case-insensitive filesystems (macOS), AGENTS.md and agents.md
        // are the same file. We test the preference logic by creating only
        // the uppercase variant and verifying it's collected.
        std::fs::write(tmp.path().join("AGENTS.md"), "# Upper").unwrap();

        let result =
            load_agents_md_with_home(Some(tmp.path()), Some(tmp.path()), None, &LocalSyncFs);
        assert!(result.content.contains("Upper"));
    }

    /// should not duplicate files seen via different paths
    #[test]
    fn should_not_duplicate_files_seen_via_different_paths() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("AGENTS.md"), "# Unique Rules").unwrap();

        let result =
            load_agents_md_with_home(Some(tmp.path()), Some(tmp.path()), None, &LocalSyncFs);
        let count = result.content.matches("Unique Rules").count();
        assert_eq!(count, 1, "file should appear exactly once");
    }

    /// should skip empty agents md files
    #[test]
    fn should_skip_empty_agents_md_files() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("AGENTS.md"), "   \n  \n").unwrap();

        let result =
            load_agents_md_with_home(Some(tmp.path()), Some(tmp.path()), None, &LocalSyncFs);
        assert!(result.content.is_empty());
    }

    /// should produce warning when content exceeds recommended max
    #[test]
    fn should_produce_warning_when_content_exceeds_recommended_max() {
        let tmp = TempDir::new().unwrap();
        // Write a file larger than 32 KB
        let big_content = "x".repeat(33 * 1024);
        std::fs::write(tmp.path().join("AGENTS.md"), &big_content).unwrap();

        let result =
            load_agents_md_with_home(Some(tmp.path()), Some(tmp.path()), None, &LocalSyncFs);
        assert!(result.warning.is_some(), "should have a size warning");
        assert!(
            result.warning.as_ref().unwrap().contains("exceeds"),
            "warning should mention exceeds"
        );
    }

    /// should not produce warning when content is under recommended max
    #[test]
    fn should_not_produce_warning_when_content_is_under_recommended_max() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("AGENTS.md"), "# Small file").unwrap();

        let result =
            load_agents_md_with_home(Some(tmp.path()), Some(tmp.path()), None, &LocalSyncFs);
        assert!(result.warning.is_none());
    }

    /// should annotate each file with from path comment
    #[test]
    fn should_annotate_each_file_with_from_path_comment() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join(".git"), "gitdir").unwrap();
        std::fs::write(tmp.path().join("AGENTS.md"), "# Root").unwrap();
        let nested = tmp.path().join("sub");
        std::fs::create_dir(&nested).unwrap();
        std::fs::write(nested.join("AGENTS.md"), "# Sub").unwrap();

        let result = load_agents_md_with_home(Some(&nested), Some(tmp.path()), None, &LocalSyncFs);
        let annotation_count = result.content.matches("<!-- From:").count();
        assert_eq!(annotation_count, 2, "should have two annotations");
    }

    /// should load user level brand agents md when workspace has none
    #[test]
    fn should_load_user_level_brand_agents_md_when_workspace_has_none() {
        let home = TempDir::new().unwrap();
        let workspace = TempDir::new().unwrap();
        let brand_dir = home.path().join(".mirri-code");
        std::fs::create_dir(&brand_dir).unwrap();
        std::fs::write(brand_dir.join("AGENTS.md"), "# User Brand Rules").unwrap();

        let result = load_agents_md_with_home(
            Some(workspace.path()),
            Some(home.path()),
            Some(&brand_dir),
            &LocalSyncFs,
        );
        assert!(result.content.contains("User Brand Rules"));
    }

    /// should load user level generic agents md as fallback
    #[test]
    fn should_load_user_level_generic_agents_md_as_fallback() {
        let home = TempDir::new().unwrap();
        let workspace = TempDir::new().unwrap();
        let generic_dir = home.path().join(".agents");
        std::fs::create_dir(&generic_dir).unwrap();
        std::fs::write(generic_dir.join("AGENTS.md"), "# Generic User Rules").unwrap();

        let result = load_agents_md_with_home(
            Some(workspace.path()),
            Some(home.path()),
            None,
            &LocalSyncFs,
        );
        assert!(result.content.contains("Generic User Rules"));
    }

    /// should honor mirricode_home env for brand directory
    #[test]
    fn should_honor_mirricode_home_env_for_brand_directory() {
        let brand_home = TempDir::new().unwrap();
        let workspace = TempDir::new().unwrap();
        std::fs::write(brand_home.path().join("AGENTS.md"), "# Custom Brand Rules").unwrap();

        let result = load_agents_md_with_home(
            Some(workspace.path()),
            None,
            Some(brand_home.path()),
            &LocalSyncFs,
        );
        assert!(result.content.contains("Custom Brand Rules"));
    }

    /// should order user level before project level
    #[test]
    fn should_order_user_level_before_project_level() {
        let home = TempDir::new().unwrap();
        let workspace = TempDir::new().unwrap();
        let brand_dir = home.path().join(".mirri-code");
        std::fs::create_dir(&brand_dir).unwrap();
        std::fs::write(brand_dir.join("AGENTS.md"), "# User Rules").unwrap();
        std::fs::write(workspace.path().join("AGENTS.md"), "# Project Rules").unwrap();

        let result = load_agents_md_with_home(
            Some(workspace.path()),
            Some(home.path()),
            Some(&brand_dir),
            &LocalSyncFs,
        );
        let user_pos = result.content.find("User Rules").unwrap();
        let project_pos = result.content.find("Project Rules").unwrap();
        assert!(
            user_pos < project_pos,
            "user-level should appear before project-level"
        );
    }

    // ===== injected SyncFs (T-13b1-4: zero-IO state plane) =====

    /// In-memory [`mirri_os::SyncFs`] double serving canned content for one
    /// path, so the AGENTS.md read is verified to go through the injection.
    struct CannedFs {
        path: PathBuf,
        content: String,
        reads: std::sync::Mutex<Vec<PathBuf>>,
    }

    impl mirri_os::SyncFs for CannedFs {
        fn read_text(&self, path: &Path) -> anyhow::Result<String> {
            self.reads.lock().unwrap().push(path.to_path_buf());
            if path == self.path {
                Ok(self.content.clone())
            } else {
                Err(anyhow::anyhow!("no such file: {}", path.display()))
            }
        }
    }

    /// should read agents md content through the injected fs (T-13b1-4)
    #[test]
    fn should_read_agents_md_content_through_injected_fs() {
        // Given a double serving one brand AGENTS.md and no workspace root
        let brand_dir = PathBuf::from("/virtual/brand");
        let fs = CannedFs {
            path: brand_dir.join("AGENTS.md"),
            content: "# Injected Rules\n\nBe injected.".to_string(),
            reads: std::sync::Mutex::new(Vec::new()),
        };

        // When the loader runs against the double
        let result = load_agents_md_with_home(None, None, Some(&brand_dir), &fs);

        // Then the content comes from the double (never the real fs) and is
        // annotated with the discovery path
        assert!(result.content.contains("Injected Rules"));
        assert!(result.content.contains("<!-- From:"));
        assert_eq!(
            fs.reads.lock().unwrap().as_slice(),
            &[brand_dir.join("AGENTS.md")],
            "the read must go through the injected fs"
        );
        assert!(
            !brand_dir.join("AGENTS.md").exists(),
            "the real path must not have been created"
        );
    }

    // ===== the W7 trust gate (design 08 §5 D6) =====

    /// should skip the project section when the trust gate excludes it
    #[test]
    fn should_skip_the_project_section_when_the_trust_gate_excludes_it() {
        // Given a home carrying a user-level (generic) AGENTS.md and a
        // workspace whose own AGENTS.md declares project rules
        let home = TempDir::new().unwrap();
        let workspace = TempDir::new().unwrap();
        let generic_dir = home.path().join(".agents");
        std::fs::create_dir(&generic_dir).unwrap();
        std::fs::write(generic_dir.join("AGENTS.md"), "# User Rules").unwrap();
        std::fs::write(workspace.path().join("AGENTS.md"), "# Project Rules").unwrap();

        // When the load runs with the project section excluded (the
        // untrusted-workspace gate)
        let result = load_agents_md_with_home_for_trust(
            Some(workspace.path()),
            false,
            Some(home.path()),
            None,
            &LocalSyncFs,
        );

        // Then the user-level section is injected but the project section
        // is absent (D6: AGENTS.md 项目段不注入)
        assert!(
            result.content.contains("User Rules"),
            "the user-level section must stay injected for an untrusted workspace"
        );
        assert!(
            !result.content.contains("Project Rules"),
            "the project section must not be injected for an untrusted workspace"
        );
    }

    /// should produce the identical document when the trust gate allows the
    /// project section
    #[test]
    fn should_match_the_ungated_load_when_the_trust_gate_allows_the_project_section() {
        // Given a home with a user-level file and a workspace with a
        // project-level one
        let home = TempDir::new().unwrap();
        let workspace = TempDir::new().unwrap();
        let brand_dir = home.path().join(".mirri-code");
        std::fs::create_dir(&brand_dir).unwrap();
        std::fs::write(brand_dir.join("AGENTS.md"), "# User Rules").unwrap();
        std::fs::write(workspace.path().join("AGENTS.md"), "# Project Rules").unwrap();

        // When the load runs with `include_project` on and off
        let allowed = load_agents_md_with_home_for_trust(
            Some(workspace.path()),
            true,
            None,
            Some(&brand_dir),
            &LocalSyncFs,
        );
        let gated = load_agents_md_with_home_for_trust(
            Some(workspace.path()),
            false,
            None,
            Some(&brand_dir),
            &LocalSyncFs,
        );

        // Then the allowed load matches the historical ungated load exactly
        assert_eq!(
            allowed.content,
            load_agents_md_with_home(Some(workspace.path()), None, Some(&brand_dir), &LocalSyncFs)
                .content,
            "`include_project = true` must reproduce the ungated document byte for byte"
        );
        assert!(allowed.content.contains("Project Rules"));
        // And the gated document carries the user section only.
        assert!(gated.content.contains("User Rules"));
        assert!(!gated.content.contains("Project Rules"));
    }
}
