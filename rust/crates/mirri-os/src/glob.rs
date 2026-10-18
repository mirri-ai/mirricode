//! Glob-pattern file matching for the mirri execution environment.
//!
//! This module provides a [`GlobMatch`] trait that abstracts glob-based file
//! discovery behind an async interface, plus [`LocalGlob`] — a concrete
//! implementation backed by the [`ignore`] crate (the same library that powers
//! ripgrep) and [`globset`] for pattern compilation.
//!
//! The walker respects `.gitignore` rules by default; set
//! [`GlobParams::include_ignored`] to `true` to include ignored files.

use std::path::PathBuf;

use anyhow::{Context, anyhow};
use globset::{GlobBuilder, GlobSetBuilder};

/// Parameters for a glob search.
///
/// Controls the pattern, root directory, and whether files excluded by
/// `.gitignore` / `.ignore` / `.git/info/exclude` should be included in the
/// results.
#[derive(Debug, Clone)]
pub struct GlobParams {
    /// Glob pattern (e.g. `**/*.rs`, `*.ts`, `src/**/*.test.ts`).
    ///
    /// Multiple patterns may be supplied as a comma-separated list; a file is
    /// included if it matches *any* of them.
    pub pattern: String,
    /// Root directory to search in.
    pub path: PathBuf,
    /// Include files excluded by `.gitignore`.
    pub include_ignored: bool,
}

/// Result of a glob search.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct GlobResult {
    /// Matched file paths, sorted by modification time (newest first).
    ///
    /// Paths are relative to the search root ([`GlobParams::path`]).
    pub matches: Vec<String>,
    /// `true` when the number of matches exceeded the 100-file cap and was
    /// truncated.
    pub truncated: bool,
}

/// Maximum number of matches returned by a single glob call.
const MAX_MATCHES: usize = 100;

/// Async glob abstraction used by the agent engine.
///
/// All methods are async via RPITIT (return-position impl trait in trait), so
/// no `async-trait` macro is required. Implementors must be `Send + Sync`.
///
/// The `async_fn_in_trait` clippy lint is allowed because this trait is
/// intended for internal use within the runtime where the implementors
/// (`LocalGlob`, and future test doubles) produce `Send` futures via `tokio`.
#[allow(async_fn_in_trait)]
pub trait GlobMatch: Send + Sync {
    /// Search for files matching `params.pattern` under `params.path`.
    async fn glob(&self, params: &GlobParams) -> anyhow::Result<GlobResult>;
}

/// [`GlobMatch`] implementation backed by the host local filesystem via the
/// `ignore` crate.
#[derive(Debug, Default)]
pub struct LocalGlob;

impl LocalGlob {
    /// Create a new `LocalGlob`.
    pub fn new() -> Self {
        Self
    }
}

/// A collected match with its modification time, used for sorting before the
/// results are converted to strings.
struct CollectedMatch {
    /// Path relative to the search root.
    relative_path: String,
    /// Modification time; used to sort newest-first.
    mtime: std::time::SystemTime,
}

impl GlobMatch for LocalGlob {
    async fn glob(&self, params: &GlobParams) -> anyhow::Result<GlobResult> {
        // Compile the glob pattern(s). A comma-separated list is split into
        // individual patterns and added to a single GlobSet so that a file is
        // included if it matches *any* pattern.
        let mut glob_set_builder = GlobSetBuilder::new();
        for raw_pattern in params.pattern.split(',') {
            let trimmed = raw_pattern.trim();
            if trimmed.is_empty() {
                continue;
            }
            // `*`/`?` must not cross path separators: a single `*` matches
            // only top-level entries (globset's default crosses separators).
            let glob = GlobBuilder::new(trimmed)
                .literal_separator(true)
                .build()
                .with_context(|| format!("failed to compile glob pattern: {trimmed}"))?;
            glob_set_builder.add(glob);
        }
        let glob_set = glob_set_builder
            .build()
            .map_err(|e| anyhow!("failed to build glob set: {e}"))?;

        // Build the directory walker. The `ignore` crate (ripgrep's underlying
        // library) respects .gitignore by default. When `include_ignored` is
        // true, we disable all ignore sources so that every file is visited.
        let mut builder = ignore::WalkBuilder::new(&params.path);
        builder.hidden(false); // include hidden files
        if params.include_ignored {
            builder.ignore(false); // don't read .ignore
            builder.git_ignore(false); // don't read .gitignore
            builder.git_exclude(false); // don't read .git/info/exclude
            builder.git_global(false); // don't read global gitignore
        }

        let root = params.path.clone();
        let walker = builder.build();

        // Collect matching files with their modification times.
        let mut collected: Vec<CollectedMatch> = Vec::new();

        for entry in walker {
            let entry = entry.context("error while walking directory tree")?;

            // Only regular files are matched — directories are skipped.
            if !entry.file_type().is_some_and(|ft| ft.is_file()) {
                continue;
            }

            // Match the path relative to the search root against the glob set.
            let relative = entry.path().strip_prefix(&root).unwrap_or(entry.path());
            if !glob_set.is_match(relative) {
                continue;
            }

            // Retrieve the modification time from the DirEntry metadata.
            // `ignore::DirEntry::metadata` returns `Ok` for normal files (it
            // follows symlinks); if it fails we fall back to the epoch so the
            // file still appears, just sorted last.
            let mtime = entry
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .unwrap_or(std::time::UNIX_EPOCH);

            let relative_path = relative.to_string_lossy().into_owned();

            collected.push(CollectedMatch {
                relative_path,
                mtime,
            });
        }

        // Sort by modification time, newest first.
        collected.sort_by_key(|c| std::cmp::Reverse(c.mtime));

        // Apply the 100-match cap.
        let truncated = collected.len() > MAX_MATCHES;
        if truncated {
            collected.truncate(MAX_MATCHES);
        }

        let matches = collected.into_iter().map(|c| c.relative_path).collect();

        Ok(GlobResult { matches, truncated })
    }
}

// ---------------------------------------------------------------------------
// Object-safe (dyn-compatible) mirror (zero-IO T13a)
// ---------------------------------------------------------------------------

/// Object-safe (dyn-compatible) mirror of [`GlobMatch`].
///
/// The RPITIT trait cannot be used as `dyn GlobMatch`, so provider injection
/// goes through this erased twin: the one method returns a boxed `Send`
/// future instead. [`LocalGlob`] implements both; test doubles implement
/// only this trait.
pub trait DynGlobMatch: Send + Sync {
    /// Search for files matching `params.pattern` under `params.path`.
    fn glob<'a>(&'a self, params: &'a GlobParams) -> DynGlobFuture<'a>;
}

/// The boxed future returned by [`DynGlobMatch::glob`].
pub type DynGlobFuture<'a> =
    std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<GlobResult>> + Send + 'a>>;

/// [`DynGlobMatch`] implementation delegating to [`LocalGlob`]'s
/// [`GlobMatch`] method (the RPITIT future is `Send`, so it boxes directly).
impl DynGlobMatch for LocalGlob {
    fn glob<'a>(&'a self, params: &'a GlobParams) -> DynGlobFuture<'a> {
        Box::pin(<Self as GlobMatch>::glob(self, params))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn should_match_only_top_level_entries_when_the_pattern_is_a_single_star() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "top level\n").unwrap();
        std::fs::create_dir_all(tmp.path().join("sub")).unwrap();
        std::fs::write(tmp.path().join("sub/b.txt"), "nested\n").unwrap();

        let result = <LocalGlob as GlobMatch>::glob(
            &LocalGlob::new(),
            &GlobParams {
                pattern: "*".to_string(),
                path: tmp.path().to_path_buf(),
                include_ignored: false,
            },
        )
        .await
        .expect("glob should succeed");

        assert_eq!(result.matches, vec!["a.txt"], "{:?}", result.matches);
    }
}
