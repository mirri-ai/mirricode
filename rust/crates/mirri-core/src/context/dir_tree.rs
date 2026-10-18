//! Compact 2-level directory tree for LLM context.
//!
//! Mirrors the TypeScript `listDirectory()` utility
//! (`packages/agent-core/src/tools/support/list-directory.ts`): renders a
//! box-drawing tree of the working directory with directories first, then
//! files, each group sorted case-insensitively by name. Width caps keep the
//! system-prompt token budget bounded:
//!   - Depth 0 (root): up to [`ROOT_WIDTH`] entries
//!   - Depth 1 (children of root dirs): up to [`CHILD_WIDTH`] entries
//!   - Truncated levels show "... and N more" so the LLM knows more exists.
//!
//! Hidden directories (names starting with `.`) are listed as entries at the
//! root level but their contents are intentionally not expanded.

use std::path::Path;

use mirri_os::SyncFs;

/// Maximum number of entries shown at the root level.
const ROOT_WIDTH: usize = 30;
/// Maximum number of entries shown per expanded directory.
const CHILD_WIDTH: usize = 10;

/// A single directory entry: its name and whether it is a directory.
struct Entry {
    name: String,
    is_dir: bool,
}

/// Collect up to `max_width` entries from `dir_path`, sorted with
/// directories first and then files, each group ordered case-insensitively
/// by name (with the raw name as a deterministic tie-break).
///
/// Returns `None` when the directory itself cannot be read (or does not
/// exist). Unreadable *individual* entries are still listed as files.
fn collect_entries(
    fs: &dyn SyncFs,
    dir_path: &Path,
    max_width: usize,
) -> Option<(Vec<Entry>, usize)> {
    let mut entries: Vec<Entry> = fs
        .iterdir(dir_path)
        .ok()?
        .into_iter()
        .map(|entry| Entry {
            name: entry.name,
            is_dir: entry.is_dir,
        })
        .collect();
    let total = entries.len();
    entries.sort_by(|a, b| match (a.is_dir, b.is_dir) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => a
            .name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then_with(|| a.name.cmp(&b.name)),
    });
    entries.truncate(max_width);
    Some((entries, total))
}

/// Generate a 2-level directory tree of the working directory.
///
/// Format: box-drawing chars (`├──`, `└──`, `│`), directories get a trailing
/// `/`, directories are listed before files (each case-insensitively sorted
/// by name), hidden directories are listed but not expanded, capped at
/// [`ROOT_WIDTH`] root entries and [`CHILD_WIDTH`] children per directory.
///
/// Returns `"[not readable]"` when the directory cannot be read, or
/// `"(empty directory)"` when it contains no entries.
pub fn list_directory(workdir: &Path) -> String {
    list_directory_with_fs(workdir, &mirri_os::LocalSyncFs::new())
}

/// [`list_directory`] with an explicit [`SyncFs`] provider — the injection
/// seam (zero-IO T13c; the engine default is `LocalSyncFs`).
pub fn list_directory_with_fs(workdir: &Path, fs: &dyn SyncFs) -> String {
    let Some((entries, total)) = collect_entries(fs, workdir, ROOT_WIDTH) else {
        return "[not readable]".to_string();
    };
    let remaining = total - entries.len();
    let mut lines: Vec<String> = Vec::new();

    for (index, entry) in entries.iter().enumerate() {
        let is_last = index == entries.len() - 1 && remaining == 0;
        let connector = if is_last { "└── " } else { "├── " };

        if entry.is_dir {
            lines.push(format!("{connector}{}/", entry.name));
            // Hidden directories are listed but their contents are not expanded.
            if entry.name.starts_with('.') {
                continue;
            }
            let child_prefix = if is_last { "    " } else { "│   " };
            match collect_entries(fs, &workdir.join(&entry.name), CHILD_WIDTH) {
                None => lines.push(format!("{child_prefix}└── [not readable]")),
                Some((children, child_total)) => {
                    let child_remaining = child_total - children.len();
                    for (child_index, child) in children.iter().enumerate() {
                        let child_is_last =
                            child_index == children.len() - 1 && child_remaining == 0;
                        let child_connector = if child_is_last {
                            "└── "
                        } else {
                            "├── "
                        };
                        let suffix = if child.is_dir { "/" } else { "" };
                        lines.push(format!(
                            "{child_prefix}{child_connector}{}{suffix}",
                            child.name
                        ));
                    }
                    if child_remaining > 0 {
                        lines.push(format!("{child_prefix}└── ... and {child_remaining} more"));
                    }
                }
            }
        } else {
            lines.push(format!("{connector}{}", entry.name));
        }
    }

    if remaining > 0 {
        lines.push(format!("└── ... and {remaining} more entries"));
    }

    if lines.is_empty() {
        "(empty directory)".to_string()
    } else {
        lines.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn write_file(dir: &Path, name: &str) {
        std::fs::write(dir.join(name), b"x").unwrap();
    }

    fn make_dir(dir: &Path, name: &str) {
        std::fs::create_dir(dir.join(name)).unwrap();
    }

    fn write_file_in(dir: &Path, sub: &str, name: &str) {
        std::fs::write(dir.join(sub).join(name), b"x").unwrap();
    }

    #[test]
    fn should_list_root_entries_when_directory_has_files_and_subdirs() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        make_dir(root, "src");
        write_file_in(root, "src", "main.rs");
        write_file(root, "Cargo.toml");
        write_file(root, "README.md");

        let out = list_directory(root);

        assert_eq!(
            out,
            "├── src/\n\
             │   └── main.rs\n\
             ├── Cargo.toml\n\
             └── README.md"
        );
    }

    #[test]
    fn should_cap_root_entries_at_30_when_directory_has_many_items() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        for i in 0..40 {
            write_file(root, &format!("file{i:02}"));
        }

        let out = list_directory(root);

        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 31, "30 capped entries + 1 truncation line");
        assert!(lines[0].starts_with("├── file00"));
        assert!(lines[29].starts_with("├── file29"));
        assert_eq!(lines[30], "└── ... and 10 more entries");
        assert!(!out.contains("file30"), "beyond-cap entries are omitted");
    }

    #[test]
    fn should_cap_child_entries_at_10_when_subdirectory_has_many_items() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        make_dir(root, "src");
        for i in 0..20 {
            write_file_in(root, "src", &format!("child{i:02}"));
        }

        let out = list_directory(root);

        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(
            lines.len(),
            12,
            "1 dir header + 10 capped children + 1 truncation line"
        );
        assert_eq!(lines[0], "└── src/");
        assert!(lines[1].starts_with("    ├── child00"));
        assert!(lines[10].starts_with("    ├── child09"));
        assert_eq!(lines[11], "    └── ... and 10 more");
        assert!(!out.contains("child10"), "beyond-cap children are omitted");
    }

    #[test]
    fn should_not_expand_hidden_directories_when_listing_children() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        make_dir(root, ".git");
        write_file_in(root, ".git", "HEAD");
        make_dir(root, "src");
        write_file_in(root, "src", "main.rs");

        let out = list_directory(root);

        assert_eq!(
            out,
            "├── .git/\n\
             └── src/\n\
             \x20   └── main.rs"
        );
        assert!(
            !out.contains("HEAD"),
            "hidden dir contents are not expanded"
        );
    }

    #[test]
    fn should_show_empty_directory_marker_when_directory_is_empty() {
        let tmp = tempfile::tempdir().unwrap();

        let out = list_directory(tmp.path());

        assert_eq!(out, "(empty directory)");
    }

    #[test]
    fn should_return_not_readable_when_directory_does_not_exist() {
        let tmp = tempfile::tempdir().unwrap();

        let out = list_directory(&tmp.path().join("does-not-exist"));

        assert_eq!(out, "[not readable]");
    }

    #[test]
    fn should_sort_directories_first_then_files_alphabetically() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        make_dir(root, "zeta");
        make_dir(root, "Alpha");
        write_file(root, "Zebra.txt");
        write_file(root, "apple.txt");

        let out = list_directory(root);

        // Directories first; within each group the order is case-insensitive.
        assert_eq!(
            out,
            "├── Alpha/\n\
             ├── zeta/\n\
             ├── apple.txt\n\
             └── Zebra.txt"
        );
    }

    #[test]
    fn should_use_box_drawing_characters_in_output() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        make_dir(root, "a_dir");
        write_file_in(root, "a_dir", "inner.txt");
        make_dir(root, "z_dir");
        write_file_in(root, "z_dir", "inner.txt");

        let out = list_directory(root);

        // Both non-last (├── / │) and last (└── / spaces) connectors appear.
        assert_eq!(
            out,
            "├── a_dir/\n\
             │   └── inner.txt\n\
             └── z_dir/\n\
             \x20   └── inner.txt"
        );
    }

    // -----------------------------------------------------------------------
    // T-13c1-2 — fake SyncFs iterdir double (zero-IO T13c)
    // -----------------------------------------------------------------------

    /// Fake [`SyncFs`] double that serves a scripted directory tree from a
    /// map: `iterdir` answers from the map and every other operation fails,
    /// proving the tree is built purely through the injected provider.
    struct FakeTreeFs {
        dirs: std::collections::HashMap<PathBuf, Vec<(String, bool)>>,
    }

    impl SyncFs for FakeTreeFs {
        fn iterdir(&self, path: &Path) -> anyhow::Result<Vec<mirri_os::fs::DirEntry>> {
            let entries = self
                .dirs
                .get(path)
                .ok_or_else(|| anyhow::anyhow!("no such dir: {}", path.display()))?;
            Ok(entries
                .iter()
                .map(|(name, is_dir)| mirri_os::fs::DirEntry {
                    name: name.clone(),
                    path: path.join(name).to_string_lossy().into_owned(),
                    is_dir: *is_dir,
                    is_file: !*is_dir,
                })
                .collect())
        }
    }

    #[test]
    fn should_render_two_level_tree_when_listing_through_fake_iterdir() {
        // Given a fake filesystem with a src directory plus two root files
        let root = PathBuf::from("/virtual/work");
        let fs = FakeTreeFs {
            dirs: [
                (
                    root.clone(),
                    vec![
                        ("Cargo.toml".to_string(), false),
                        ("README.md".to_string(), false),
                        ("src".to_string(), true),
                    ],
                ),
                (root.join("src"), vec![("main.rs".to_string(), false)]),
            ]
            .into_iter()
            .collect(),
        };

        // When the tree is rendered through the injected provider
        let out = list_directory_with_fs(&root, &fs);

        // Then the depth, directory-first ordering, and connectors are
        // unchanged from the host-filesystem behavior
        assert_eq!(
            out,
            "├── src/\n\
             │   └── main.rs\n\
             ├── Cargo.toml\n\
             └── README.md"
        );
    }

    #[test]
    fn should_keep_depth_caps_and_ignore_rules_when_listing_through_fake_iterdir() {
        // Given a fake filesystem with 42 root entries (40 files plus a
        // hidden directory and `src`), a 21-entry `src` child set, and a
        // hidden directory holding an entry
        let root = PathBuf::from("/virtual/work");
        let mut root_entries: Vec<(String, bool)> =
            (0..40).map(|i| (format!("file{i:02}"), false)).collect();
        root_entries.push((".git".to_string(), true));
        root_entries.push(("src".to_string(), true));
        let mut child_entries: Vec<(String, bool)> =
            (0..20).map(|i| (format!("child{i:02}"), false)).collect();
        child_entries.push(("deep".to_string(), true));
        let fs = FakeTreeFs {
            dirs: [
                (root.clone(), root_entries),
                (root.join("src"), child_entries),
                (root.join(".git"), vec![("HEAD".to_string(), false)]),
                (root.join("src").join("deep"), Vec::new()),
            ]
            .into_iter()
            .collect(),
        };

        // When the tree is rendered through the injected provider
        let out = list_directory_with_fs(&root, &fs);

        // Then directories sort first at the root, and the root is capped at
        // 30 of 42 entries (both `... and 12 more entries` accounting)
        let lines: Vec<&str> = out.lines().collect();
        assert!(
            lines[0].starts_with("├── .git/"),
            "dirs sort first: {lines:?}"
        );
        assert!(lines[1].starts_with("├── src/"));
        assert!(
            out.contains("file27"),
            "the first 28 files fit inside the cap"
        );
        assert!(
            !out.contains("file28"),
            "beyond-cap root entries are omitted"
        );
        assert!(out.contains("└── ... and 12 more entries"));

        // And the child set is capped at 10 of 21 entries (dirs first:
        // `deep` plus child00..child08; `... and 11 more` accounting)
        assert!(out.contains("child08"));
        assert!(!out.contains("child09"), "beyond-cap children are omitted");
        assert!(out.contains("... and 11 more"));

        // And the hidden directory's contents stay sealed, while the depth
        // never exceeds two levels — the third-level `deep` directory
        // appears only as an unexpanded child line
        assert!(
            !out.contains("HEAD"),
            "hidden dir contents are not expanded"
        );
        assert!(out.contains("deep/"));
        assert!(
            !out.contains("(empty directory)"),
            "third-level directories are not expanded"
        );
        assert!(
            !out.lines().any(|line| line.starts_with("        ")),
            "no third-level indentation may appear: {out}"
        );
    }

    #[test]
    fn should_return_not_readable_when_fake_iterdir_has_no_directory() {
        // Given a fake filesystem with no entry for the requested root
        let fs = FakeTreeFs {
            dirs: std::collections::HashMap::new(),
        };

        // When listing a directory the double does not know
        let out = list_directory_with_fs(Path::new("/virtual/missing"), &fs);

        // Then the output degrades to the not-readable marker
        assert_eq!(out, "[not readable]");
    }
}
