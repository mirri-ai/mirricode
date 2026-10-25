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
    /// Soft walker-stop signal — **not** a result-set truncation.
    ///
    /// The search layer never truncates the collected result set. When
    /// non-zero, the walker stops as soon as `limit` matches have been
    /// collected (bounding traversal of large trees) and the stop point is
    /// reported as [`GlobResult::total_matched`] — the "enough collected"
    /// semantics: the full count is only known when `limit` is large enough
    /// or `0`. When `0`, collection is unbounded but guarded by the double
    /// hard cap ([`MAX_UNLIMITED_MATCHES`] matches /
    /// [`MAX_UNLIMITED_PATH_BYTES`] of accumulated relative-path bytes).
    pub limit: usize,
}

/// Result of a glob search.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct GlobResult {
    /// Matched file paths, sorted newest-first (mtime descending) with the
    /// relative path (lexicographic ascending) as a tie-break.
    ///
    /// Paths are relative to the search root ([`GlobParams::path`]). The
    /// search layer never truncates this list.
    pub matches: Vec<String>,
    /// `true` when a hard cap ([`MAX_UNLIMITED_MATCHES`] or
    /// [`MAX_UNLIMITED_PATH_BYTES`], effective for `limit == 0`) stopped the
    /// walker — more matches existed than were collected.
    pub truncated: bool,
    /// Number of matches collected, after sensitive-file filtering.
    ///
    /// Sensitive files (secrets, keys, credentials) are filtered at collection
    /// time, so this total never discloses their existence. With a non-zero
    /// [`GlobParams::limit`] this is the walker stop point, not necessarily
    /// the full match count of the tree.
    pub total_matched: usize,
}

/// Hard cap on collected matches when [`GlobParams::limit`] is `0`
/// (unlimited): once this many matches are collected the walker stops and
/// [`GlobResult::truncated`] is set, preventing unbounded collection on huge
/// trees.
pub const MAX_UNLIMITED_MATCHES: usize = 10_000;

/// Hard cap on the accumulated `relative_path` byte length (a conservative
/// approximation of the rendered output size) when collecting with
/// [`GlobParams::limit`] == `0`.
pub const MAX_UNLIMITED_PATH_BYTES: usize = 1024 * 1024;

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

        // Sensitive files (secrets, keys, credentials) are filtered here at
        // collection time, so neither the returned matches nor the reported
        // total can disclose their existence. The tool layer keeps its own
        // filter as a second defense line.
        let sensitive_filter = crate::path_access::PathAccess::new(root.clone());

        // Effective walker-stop bound. A non-zero limit is a soft stop (the
        // result set itself is never truncated); a zero limit means unbounded,
        // guarded by the double hard cap.
        let effective_limit = if params.limit == 0 {
            MAX_UNLIMITED_MATCHES
        } else {
            params.limit
        };

        // Collect matching files with their modification times, stopping at
        // the effective walker bound. Because the `ignore` walker's parallel
        // directory traversal order is not deterministic, the collected subset
        // at the stop point is a prefix of the tree in walk order, not in
        // relevance order — the count, not the selection, is what the stop
        // bound guarantees.
        let mut collected: Vec<CollectedMatch> = Vec::new();
        let mut path_bytes: usize = 0;
        let mut truncated = false;

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

            // Sensitive files never surface in the results or the totals.
            if sensitive_filter.is_sensitive(relative) {
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

            path_bytes = path_bytes.saturating_add(relative_path.len());
            collected.push(CollectedMatch {
                relative_path,
                mtime,
            });

            // Walker stop conditions. A non-zero limit stops collection early
            // (bounding tree traversal) — that is an optimization, not a
            // truncation of the result set. The zero-limit (unbounded) case
            // stops only at the double hard cap, where `truncated` marks that
            // more matches existed.
            if collected.len() >= effective_limit {
                if params.limit == 0 {
                    truncated = true;
                }
                break;
            }
            if params.limit == 0 && path_bytes >= MAX_UNLIMITED_PATH_BYTES {
                truncated = true;
                break;
            }
        }

        // Sort newest-first, breaking mtime ties by relative path
        // (lexicographic ascending) so the order is deterministic.
        collected.sort_by(|a, b| {
            b.mtime
                .cmp(&a.mtime)
                .then_with(|| a.relative_path.cmp(&b.relative_path))
        });

        let total_matched = collected.len();
        let matches = collected.into_iter().map(|c| c.relative_path).collect();

        Ok(GlobResult {
            matches,
            truncated,
            total_matched,
        })
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
                limit: 0,
            },
        )
        .await
        .expect("glob should succeed");

        assert_eq!(result.matches, vec!["a.txt"], "{:?}", result.matches);
    }
}
