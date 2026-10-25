//! Content search via ripgrep.
//!
//! This module provides a [`ContentSearch`] trait and a [`RipgrepSearch`]
//! implementation that wraps the `rg` (ripgrep) subprocess. It mirrors the
//! TypeScript search integration: pattern matching, glob/type filtering,
//! sensitive-file filtering, output-size limits, and head-limit truncation.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::Context;
use serde::Deserialize;
use tokio::io::AsyncReadExt;

use crate::path_access::PathAccess;

/// Maximum stdout size we will read from ripgrep before killing the process.
///
/// 10 MiB — matches the TypeScript implementation's cap.
const MAX_OUTPUT_BYTES: usize = 10 * 1024 * 1024;

/// The hard-cap notice the tool layer renders when the search provider hit
/// [`MAX_OUTPUT_BYTES`] — exposed so both layers quote the same figure.
pub const MAX_OUTPUT_BYTES_NOTICE: &str = "10485760";

/// Timeout for the ripgrep subprocess.
const SEARCH_TIMEOUT: Duration = Duration::from_secs(20);

/// VCS metadata directories always excluded from search results (TS
/// `VCS_DIRECTORIES_TO_EXCLUDE`), regardless of `include_ignored`.
const VCS_DIRECTORIES_TO_EXCLUDE: &[&str] = &[".git", ".svn", ".hg", ".bzr", ".jj", ".sl"];

/// Column cap applied to non-content modes (`--max-columns`), matching TS
/// `RG_MAX_COLUMNS`, so a single pathological line cannot blow up the output
/// budget. Content mode omits it — the actual match text must surface verbatim.
const RG_MAX_COLUMNS: u32 = 500;

/// Parameters that control a content search invocation.
///
/// Mirrors the TypeScript `SearchParams` interface.
#[derive(Debug, Clone)]
pub struct SearchParams {
    /// Regex pattern to search for.
    pub pattern: String,
    /// Root paths to search in. Multiple roots are searched in a single rg
    /// invocation; for `content` output the result groups are ordered by
    /// their position in this array (stable group ordering — rg's own
    /// multi-path output order is nondeterministic).
    ///
    /// The default is a single empty path (`vec![PathBuf::new()]`), matching
    /// the legacy `PathBuf::new()` semantics — rg itself reports the error.
    pub paths: Vec<PathBuf>,
    /// Optional regular expression excluding matching lines from results.
    ///
    /// Applied as a post-parse filter: matches whose line text matches this
    /// pattern are dropped (content mode also drops their orphan context
    /// lines, and files/count modes drop files whose matches are all
    /// excluded).
    pub exclude_pattern: Option<String>,
    /// Optional glob filter (e.g. `"*.ts"`). Only files matching this glob are
    /// searched. A pattern containing a path separator (e.g. `"src/**/*.ts"`)
    /// is matched against each file's path relative to the search root.
    pub glob: Option<String>,
    /// Optional file type filter (e.g. `"ts"`, `"rust"`). Maps to `rg --type`.
    pub file_type: Option<String>,
    /// Case-insensitive search.
    pub case_insensitive: bool,
    /// Multiline mode (allows patterns to span line boundaries).
    pub multiline: bool,
    /// Output mode: `"content"`, `"files"`, or `"count"`.
    pub output_mode: String,
    /// Number of context lines before each match.
    pub context_before: u32,
    /// Number of context lines after each match.
    pub context_after: u32,
    /// Combined context lines (sets both before and after). Takes precedence
    /// over `context_before` and `context_after` when non-zero.
    pub context: u32,
    /// Maximum number of results to return. `usize::MAX` means unlimited.
    pub head_limit: usize,
    /// Number of leading matches to skip before collecting results. Used
    /// together with `head_limit` to page through large result sets.
    pub offset: usize,
    /// Whether to include line numbers in content-mode output. Defaults to
    /// `true`. Meta-information only: the parser always keeps the real line
    /// number in parsed matches; whether it is *displayed* is the tool
    /// layer's decision.
    pub line_numbers: bool,
    /// Include files excluded by `.gitignore`.
    pub include_ignored: bool,
}

impl Default for SearchParams {
    fn default() -> Self {
        Self {
            pattern: String::new(),
            paths: vec![PathBuf::new()],
            exclude_pattern: None,
            glob: None,
            file_type: None,
            case_insensitive: false,
            multiline: false,
            output_mode: "content".to_string(),
            context_before: 0,
            context_after: 0,
            context: 0,
            head_limit: usize::MAX,
            offset: 0,
            line_numbers: true,
            include_ignored: false,
        }
    }
}

/// A single search match.
///
/// In `content` mode all fields are populated. In `files` mode
/// only `path` is meaningful (`line_number` is 0, `line` is empty). In
/// `count` mode `line_number` carries the per-file match count.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SearchMatch {
    /// Path of the file that matched.
    pub path: String,
    /// 1-based line number of the match. In `count` mode this holds the
    /// total match count for the file.
    #[serde(rename = "lineNumber")]
    pub line_number: u64,
    /// The text of the matching line.
    pub line: String,
    /// 0-based column offset of the match within the line.
    pub column: u64,
    /// `true` for actual pattern matches, `false` for context lines emitted
    /// by `-A`/`-B`/`-C`. In `files`/`count` modes this is always `true`.
    ///
    /// Pagination (`offset`/`head_limit`) and `total_matches` count only
    /// entries with `is_match == true`; whether the line number is *shown*
    /// stays a tool-layer concern.
    #[serde(default)]
    pub is_match: bool,
}

/// The result of a content search.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SearchResult {
    /// Collected matches, capped at `head_limit`.
    pub matches: Vec<SearchMatch>,
    /// Total number of matches found (may exceed `matches.len()` if truncated).
    #[serde(rename = "totalMatches")]
    pub total_matches: usize,
    /// `true` if the result set was truncated due to `head_limit` or the output
    /// size cap.
    pub truncated: bool,
    /// `true` if the underlying rg output was cut off at the hard stdout cap
    /// (10 MiB) rather than by `head_limit` pagination. Distinguishes the
    /// two so the tool layer can render the hard-cap notice.
    #[serde(default)]
    pub truncated_by_size: bool,
}

/// Async content search abstraction.
///
/// All methods are async via RPITIT (return-position impl trait in trait), so
/// no `async-trait` macro is required. Implementors must be `Send + Sync`.
#[allow(async_fn_in_trait)]
pub trait ContentSearch: Send + Sync {
    /// Search for `params.pattern` under `params.path` and return matches.
    async fn search(&self, params: &SearchParams) -> anyhow::Result<SearchResult>;
}

/// [`ContentSearch`] implementation backed by the `rg` (ripgrep) subprocess.
#[derive(Debug, Default)]
pub struct RipgrepSearch;

impl RipgrepSearch {
    /// Create a new `RipgrepSearch`.
    pub fn new() -> Self {
        Self
    }
}

impl ContentSearch for RipgrepSearch {
    async fn search(&self, params: &SearchParams) -> anyhow::Result<SearchResult> {
        search_with_ripgrep(params).await
    }
}

/// Build the `rg` argument list from [`SearchParams`].
///
/// The first element of the returned tuple is the args for the `rg` invocation
/// (excluding the program name). The second element is the resolved output
/// mode string (lowercased). The pattern and search path are appended by the
/// caller ([`search_with_ripgrep`]) because they must come last.
fn build_args(params: &SearchParams) -> (Vec<String>, String) {
    let mode = params.output_mode.to_lowercase();
    let mut args: Vec<String> = Vec::new();

    match mode.as_str() {
        "files" => {
            args.push("--files-with-matches".to_string());
        }
        "count" => {
            args.push("--count-matches".to_string());
        }
        _ => {
            // content mode — emit JSON for structured parsing.
            args.push("--json".to_string());
        }
    }

    // Hidden files are always searched (TS parity: `--hidden` is turned on
    // unconditionally); `include_ignored` only adds `--no-ignore` so gitignored
    // files (node_modules, build outputs) are also matched.
    args.push("--hidden".to_string());
    if params.include_ignored {
        args.push("--no-ignore".to_string());
    }

    // VCS metadata directories (.git and similar) are always skipped, even
    // when include_ignored is set (spec .gitignore and similar).
    for dir in VCS_DIRECTORIES_TO_EXCLUDE {
        args.push("--glob".to_string());
        args.push(format!("!{dir}"));
    }

    // Non-content modes cap the column width so a single huge line (minified
    // files, giant JSON) cannot monopolize the output budget. Content mode
    // leaves it off — the match text must surface verbatim (rerun the rg
    // parser against unchanged data).
    if mode != "content" {
        args.push("--max-columns".to_string());
        args.push(RG_MAX_COLUMNS.to_string());
    }

    if params.case_insensitive {
        args.push("-i".to_string());
    }
    if params.multiline {
        args.push("-U".to_string());
        args.push("--multiline-dotall".to_string());
    }
    if let Some(ft) = &params.file_type {
        args.push("--type".to_string());
        args.push(ft.clone());
    }
    if let Some(glob) = &params.glob {
        args.push("--glob".to_string());
        args.push(glob.clone());
    }
    // `-C` takes precedence over `-A` and `-B` — when set, only pass `-C`.
    if params.context > 0 {
        args.push("-C".to_string());
        args.push(params.context.to_string());
    } else {
        if params.context_before > 0 {
            args.push("-B".to_string());
            args.push(params.context_before.to_string());
        }
        if params.context_after > 0 {
            args.push("-A".to_string());
            args.push(params.context_after.to_string());
        }
    }

    (args, mode)
}

/// Resolve the `rg` executable to invoke.
///
/// Resolution order (read-only — nothing is ever downloaded or spawned):
/// 1. Any `rg` executable on `PATH` → the plain name `"rg"` (the spawn then
///    behaves exactly as before, including the "ripgrep not found in PATH"
///    error when it is absent).
/// 2. Otherwise `<MIRRICODE_HOME || $HOME/.mirri-code>/bin/rg` — the binary
///    cached by the desktop/CLI installers — when it exists and is
///    executable; its absolute path is returned.
/// 3. Otherwise `"rg"`, preserving the historical spawn-failure error text.
fn resolve_rg_path() -> PathBuf {
    if which_exists_on_path("rg") {
        return PathBuf::from("rg");
    }
    if let Some(cached) = cached_rg_path()
        && is_executable_file(&cached)
    {
        return cached;
    }
    PathBuf::from("rg")
}

/// Look for an executable file named `name` on `PATH` without spawning it.
fn which_exists_on_path(name: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| {
            std::env::split_paths(&paths).any(|dir| is_executable_file(&dir.join(name)))
        })
        .unwrap_or(false)
}

/// The cached-rg candidate under the Mirri home directory.
fn cached_rg_path() -> Option<PathBuf> {
    let home = std::env::var_os("MIRRICODE_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".mirri-code")))?;
    Some(home.join("bin").join("rg"))
}

/// Whether `path` is an existing file we may execute.
fn is_executable_file(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        // On non-Unix platforms the execute bit does not exist; a present
        // regular file is the best available signal.
        true
    }
}

/// Run ripgrep and collect its output, enforcing the output-size cap.
///
/// Returns `(stdout_bytes, truncated_by_size)`. If the output exceeds
/// [`MAX_OUTPUT_BYTES`] the child is killed and `truncated_by_size` is `true`.
/// `rg_cwd` optionally pins the subprocess working directory (see
/// [`rg_invocation`]).
async fn run_ripgrep(
    args: &[String],
    rg_cwd: Option<&std::path::Path>,
) -> anyhow::Result<(Vec<u8>, bool)> {
    let mut cmd = tokio::process::Command::new(resolve_rg_path());
    cmd.args(args);
    if let Some(dir) = rg_cwd {
        cmd.current_dir(dir);
    }
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    let mut child = cmd
        .spawn()
        .context("failed to spawn rg — ripgrep not found in PATH")?;

    let mut stdout = child.stdout.take().context("rg stdout was not piped")?;
    let mut stderr = child.stderr.take().context("rg stderr was not piped")?;

    // Read stdout with a size cap. We read in chunks so we can detect the cap
    // before slurping the entire pipe buffer.
    let mut out: Vec<u8> = Vec::new();
    let mut buf = [0u8; 8192];
    let mut truncated_by_size = false;

    loop {
        let n = tokio::select! {
            biased;
            read_result = stdout.read(&mut buf) => {
                match read_result {
                    Ok(0) => break,
                    Ok(n) => n,
                    Err(e) => return Err(anyhow::Error::new(e).context("failed to read rg stdout")),
                }
            }
        };

        if out.len() + n > MAX_OUTPUT_BYTES {
            // Only keep up to the cap.
            let remaining = MAX_OUTPUT_BYTES.saturating_sub(out.len());
            out.extend_from_slice(&buf[..remaining]);
            truncated_by_size = true;
            break;
        }
        out.extend_from_slice(&buf[..n]);
    }

    if truncated_by_size {
        // Best-effort kill.
        let _ = child.start_kill();
        let _ = child.wait().await;
    } else {
        // Wait for the process to finish, with a timeout.
        //
        // We use `wait()` (which borrows `&mut self`) rather than
        // `wait_with_output()` (which takes `self` by value) because we have
        // already consumed stdout/stderr via the pipes, and we need `child` to
        // remain available for `start_kill()` in the timeout branch.
        match tokio::time::timeout(SEARCH_TIMEOUT, child.wait()).await {
            Ok(Ok(_status)) => {
                // stdout already consumed via the pipe; nothing else to do.
            }
            Ok(Err(e)) => {
                return Err(e).context("failed to wait for rg");
            }
            Err(_) => {
                let _ = child.start_kill();
                let _ = child.wait().await;
                anyhow::bail!(
                    "ripgrep search timed out after {} seconds",
                    SEARCH_TIMEOUT.as_secs()
                );
            }
        }
    }

    // Surface stderr if ripgrep exited non-zero and we have no stdout. We read
    // stderr opportunistically (non-blocking) — if it is empty we just proceed.
    let mut stderr_str = String::new();
    let _ = tokio::time::timeout(
        Duration::from_millis(100),
        stderr.read_to_string(&mut stderr_str),
    )
    .await;

    // ripgrep exits with code 1 when there are no matches — that is not an
    // error for us. Only surface real failures (code 2+ or non-UTF8 stderr
    // mentioning error). When truncated by size we don't treat the kill as an
    // error.
    if !truncated_by_size && out.is_empty() && stderr_str.contains("error") {
        anyhow::bail!("ripgrep failed: {stderr_str}");
    }

    Ok((out, truncated_by_size))
}

/// Deserialize helper for a single `--json` line from ripgrep.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum RgJsonLine {
    Match {
        data: MatchData,
    },
    /// Context line emitted when `-A`, `-B`, or `-C` is active.
    /// Has the same shape as `Match` but no `submatches`.
    Context {
        data: MatchData,
    },
    #[allow(dead_code)]
    Begin {
        #[allow(dead_code)]
        data: serde_json::Value,
    },
    #[allow(dead_code)]
    End {
        #[allow(dead_code)]
        data: serde_json::Value,
    },
    #[allow(dead_code)]
    Summary {
        #[allow(dead_code)]
        data: serde_json::Value,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
struct MatchData {
    path: RgPath,
    #[serde(default)]
    line_number: Option<u64>,
    lines: RgLines,
    #[serde(default)]
    submatches: Vec<Submatch>,
}

#[derive(Debug, Deserialize)]
struct RgPath {
    text: String,
}

#[derive(Debug, Deserialize)]
struct RgLines {
    text: String,
}

#[derive(Debug, Deserialize)]
struct Submatch {
    start: u64,
    #[allow(dead_code)]
    end: u64,
}

/// Compute the rg search paths, subprocess cwd, and resolve roots for a
/// search.
///
/// When a glob filter is active, ripgrep matches a `--glob` pattern
/// containing a `/` against paths relative to its own cwd — with absolute
/// search paths such a pattern is matched against absolute paths and
/// silently matches nothing. So whenever a `/`-bearing glob is combined with
/// one or more search roots, rg runs with its cwd pinned to the common
/// ancestor of the roots, receives the roots as cwd-relative paths, and the
/// result layer resolves the emitted `./relative` records back against each
/// root (mirrors the TS Grep/Glob tools' F11 fix). Without such a glob
/// nothing depends on rg's cwd, so the legacy invocation — absolute paths,
/// no cwd pinning — is kept.
///
/// Multi-path searches validate every root up front: a missing root is
/// named explicitly in a `search path does not exist: ...` error (rg's own
/// message for a missing path among several is generic; naming the element
/// is a US-G2 requirement).
///
/// Returns `(rg_search_paths, rg_cwd, resolve_roots)`. `rg_cwd` is `Some`
/// only for the common-ancestor case; `resolve_roots` is non-empty only
/// then, and carries the original (absolute) roots for record resolution.
fn rg_invocation(
    params: &SearchParams,
) -> anyhow::Result<(Vec<String>, Option<PathBuf>, Vec<PathBuf>)> {
    let glob_has_separator = params
        .glob
        .as_deref()
        .is_some_and(|g| g.contains(std::path::MAIN_SEPARATOR) || g.contains('/'));
    let pin_common_ancestor = glob_has_separator || params.paths.len() > 1;

    if !pin_common_ancestor {
        // Legacy shape: absolute paths on argv, no cwd pinning, no resolve
        // roots. A single-file root cannot serve as a cwd, so it stays
        // absolute even with a `/`-bearing glob (its records then carry
        // absolute paths and need no resolution).
        //
        // A missing single root is reported by rg itself; the spawn does
        // not fail on a nonexistent cwd.
        return Ok((
            params
                .paths
                .iter()
                .map(|p| p.to_string_lossy().into_owned())
                .collect(),
            None,
            Vec::new(),
        ));
    }

    // Validate every root before spawning — name the missing element.
    for path in &params.paths {
        if !path.try_exists().unwrap_or(false) {
            anyhow::bail!("search path does not exist: {}", path.display());
        }
    }

    let ancestor = common_ancestor(&params.paths);
    let mut search_paths = Vec::with_capacity(params.paths.len());
    for path in &params.paths {
        // The ancestor is computed as a path-component prefix, so
        // stripping always succeeds for well-formed inputs; a degenerate
        // empty ancestor degrades to the absolute form (rg still resolves
        // it, only glob matching loses root relativity).
        let arg = path
            .strip_prefix(&ancestor)
            .map(|rel| rel.to_string_lossy().into_owned())
            .unwrap_or_else(|_| path.to_string_lossy().into_owned());
        // A root that IS the common ancestor strips to the empty path — rg
        // rejects an empty-string argv element with exit 2 (`IO error for
        // operation on :`). `.` keeps the invocation cwd-relative (cwd is
        // pinned to the ancestor) and glob matching intact.
        let arg = if arg.is_empty() { ".".to_string() } else { arg };
        search_paths.push(arg);
    }

    Ok((search_paths, Some(ancestor), params.paths.clone()))
}

/// Compute the common ancestor directory of `paths`.
///
/// The ancestor is the longest shared leading-component prefix of the input
/// paths; it never resolves symlinks (a cwd must match the form the roots
/// were given in, so `./relative` records resolve back lexically). Degenerate
/// inputs (empty slice) map to the current directory. A single-file root
/// contributes its parent directory — rg needs a directory as its cwd.
fn common_ancestor(paths: &[PathBuf]) -> PathBuf {
    let mut iter = paths.iter();
    let Some(first) = iter.next() else {
        return PathBuf::from(".");
    };
    let mut ancestor: Vec<std::ffi::OsString> = if first.is_dir() {
        first
            .components()
            .map(|c| c.as_os_str().to_os_string())
            .collect()
    } else {
        first
            .parent()
            .map(|parent| {
                parent
                    .components()
                    .map(|c| c.as_os_str().to_os_string())
                    .collect()
            })
            .unwrap_or_default()
    };

    for path in iter {
        // The first mismatching component truncates the ancestor; equal
        // prefixes keep it. A root that is a file can only contribute its
        // parent — which is always at least as short.
        let components: Vec<std::ffi::OsString> = path
            .components()
            .map(|c| c.as_os_str().to_os_string())
            .collect();
        let components = if path.is_dir() {
            components
        } else {
            let mut trimmed = components;
            trimmed.pop();
            trimmed
        };
        let mut new_len = 0;
        for (a, b) in ancestor.iter().zip(components.iter()) {
            if a != b {
                break;
            }
            new_len += 1;
        }
        ancestor.truncate(new_len);
    }

    // A single-file root with no parent (e.g. `/a.rs`) degenerates to the
    // filesystem root — still a usable cwd; a truly empty ancestor falls
    // back to the current directory so the spawn never fails on `""`.
    let mut result = PathBuf::new();
    for component in &ancestor {
        result.push(component);
    }
    if result.as_os_str().is_empty() {
        result.push(".");
    }
    result
}

/// Parse `--files-with-matches` output (files × exclude: a file survives
/// only when at least one of its matches escapes the exclude pattern).
fn parse_files_with_matches_excluded(
    entries: Vec<SearchMatch>,
    head_limit: usize,
    offset: usize,
) -> (Vec<SearchMatch>, usize, bool) {
    // Context entries (possible when -A/-B/-C accompanied the forced --json
    // invocation) never keep a file alive on their own.
    let entries: Vec<SearchMatch> = entries.into_iter().filter(|m| m.is_match).collect();
    let mut files: Vec<SearchMatch> = Vec::new();
    for entry in &entries {
        if !files.iter().any(|m| m.path == entry.path) {
            files.push(entry.clone());
        }
    }
    let total_matches = files.len();
    let mut matches: Vec<SearchMatch> = Vec::new();
    let mut truncated = false;
    for (i, file) in files.into_iter().enumerate() {
        if i < offset {
            continue;
        }
        if matches.len() < head_limit {
            matches.push(SearchMatch {
                line_number: 0,
                line: String::new(),
                column: 0,
                is_match: true,
                path: file.path,
            });
        } else {
            truncated = true;
        }
    }
    (matches, total_matches, truncated)
}

/// Parse a `--json` stream into count-mode form (count × exclude: only
/// matches escaping the exclude pattern are counted, per file).
fn parse_count_matches_excluded(
    entries: Vec<SearchMatch>,
    head_limit: usize,
    offset: usize,
) -> (Vec<SearchMatch>, usize, bool) {
    // Only actual matches count (see parse_files_with_matches_excluded).
    let entries: Vec<SearchMatch> = entries.into_iter().filter(|m| m.is_match).collect();
    // Per-file kept match counts, in first-seen file order.
    let mut counts: Vec<(String, u64)> = Vec::new();
    for entry in entries {
        if let Some((_, count)) = counts.iter_mut().find(|(p, _)| *p == entry.path) {
            *count += 1;
        } else {
            counts.push((entry.path, 1));
        }
    }
    let total_matches = counts.len();
    let mut matches: Vec<SearchMatch> = Vec::new();
    let mut truncated = false;
    for (i, (path, count)) in counts.into_iter().enumerate() {
        if i < offset {
            continue;
        }
        if matches.len() < head_limit {
            matches.push(SearchMatch {
                path,
                line_number: count,
                line: String::new(),
                column: 0,
                is_match: true,
            });
        } else {
            truncated = true;
        }
    }
    (matches, total_matches, truncated)
}

/// Run the full search pipeline: spawn rg, parse output, filter sensitive
/// files, apply head_limit.
async fn search_with_ripgrep(params: &SearchParams) -> anyhow::Result<SearchResult> {
    let (rg_search_paths, rg_cwd, _resolve_roots) = rg_invocation(params)?;

    // files/count × exclude_pattern cannot be answered by rg's own
    // --files-with-matches/--count-matches (they know nothing of the exclude
    // filter) — force the --json path and merge per-file afterwards.
    let exclude_forced_json = params.output_mode.to_lowercase() != "content"
        && params.exclude_pattern.is_some();
    let (mut args, mode) = build_args(params);
    if exclude_forced_json {
        // build_args already pushed the mode flag first; strip it so the
        // invocation becomes a plain content-mode --json search.
        match mode.as_str() {
            "files" => {
                args.retain(|a| a != "--files-with-matches");
            }
            "count" => {
                args.retain(|a| a != "--count-matches");
            }
            _ => {}
        }
        args.insert(0, "--json".to_string());
    }

    // The pattern and search paths always come last.
    args.push(params.pattern.clone());
    for path in &rg_search_paths {
        args.push(path.clone());
    }

    let (output, truncated_by_size) = run_ripgrep(&args, rg_cwd.as_deref()).await?;

    // One PathAccess per search root: every record is judged by the root it
    // belongs to (longest-prefix match), so sensitive-file detection stays
    // correct across disjoint roots.
    let roots = RootContext {
        roots: params.paths.clone(),
        cwd: rg_cwd,
        accesses: params
            .paths
            .iter()
            .map(|root| PathAccess::new(root.clone()))
            .collect(),
    };

    let (mut matches, total_matches, truncated) = if exclude_forced_json {
        // Collect full JSON matches, apply the exclude filter, then merge
        // into the requested files/count shape.
        let exclude_pattern = params.exclude_pattern.as_deref();
        let (entries, _total, _trunc) = collect_json_entries(
            &output,
            &roots,
            exclude_pattern,
        )?;
        match mode.as_str() {
            "files" => parse_files_with_matches_excluded(entries, params.head_limit, params.offset),
            _ => parse_count_matches_excluded(entries, params.head_limit, params.offset),
        }
    } else {
        match mode.as_str() {
            "files" => parse_files_with_matches(
                &output,
                &roots,
                params.head_limit,
                params.offset,
                params.exclude_pattern.as_deref(),
            )?,
            "count" => parse_count_matches(&output, &roots, params.head_limit, params.offset)?,
            _ => parse_json_content(
                &output,
                &roots,
                params.head_limit,
                params.offset,
                params.exclude_pattern.as_deref(),
                ContextWindow::from_params(params),
            )?,
        }
    };

    if mode == "content" && params.paths.len() > 1 {
        // Stable group ordering: rg's multi-path output order is
        // nondeterministic, so content matches are re-ordered by their
        // owning root's position in `params.paths` (longest-prefix match).
        // `sort_by_key` is stable, so within a group rg's original order
        // (file order, line order) is preserved. This makes offset/head_limit
        // pagination reproducible across runs.
        matches.sort_by_key(|m| {
            group_index(std::path::Path::new(&m.path), &params.paths).unwrap_or(usize::MAX)
        });
    }

    Ok(SearchResult {
        matches,
        total_matches,
        truncated: truncated || truncated_by_size,
        truncated_by_size,
    })
}

/// Per-root context shared by the output parsers.
///
/// Records emitted by rg mirror the argv form: absolute when absolute paths
/// were passed, cwd-relative (possibly `./`-prefixed) when the cwd was
/// pinned to the common ancestor. `resolve` maps every record back to its
/// original absolute form; sensitivity is then judged by the record's
/// owning root (longest-prefix match), falling back to the first root for
/// records that escape all roots (e.g. paths pulled in by a glob).
struct RootContext {
    /// The original search roots, in `params.paths` order.
    roots: Vec<PathBuf>,
    /// The rg subprocess cwd, pinned when argv paths were made relative.
    cwd: Option<PathBuf>,
    /// One access per search root, parallel to `roots`.
    accesses: Vec<PathAccess>,
}

impl RootContext {
    /// Resolve a raw record path from rg output into its absolute form.
    ///
    /// The joined result is lexically normalized (`.` and redundant
    /// separators collapsed, no symlink resolution) so `./relative` records
    /// resolve to the same textual form as the original root — the tempdir
    /// symlink on macOS must not be canonicalized away.
    fn resolve(&self, raw: &str) -> String {
        let path = Path::new(raw);
        if path.is_absolute() {
            return raw.to_string();
        }
        match &self.cwd {
            Some(cwd) => lexical_clean(&cwd.join(path))
                .to_string_lossy()
                .into_owned(),
            // No pinned cwd means the argv paths were absolute and rg
            // echoes them back as-is; keep the raw text so nothing is lost.
            None => raw.to_string(),
        }
    }

    /// Judge a resolved absolute record path for sensitivity using the
    /// owning root's access instance (longest-prefix match).
    fn is_sensitive(&self, absolute: &Path) -> bool {
        let access = match group_index(absolute, &self.roots) {
            Some(index) => &self.accesses[index],
            None => &self.accesses[0],
        };
        access.is_sensitive(absolute)
    }
}

/// Lexically clean a path: drop `.` components and collapse redundant
/// separators without resolving symlinks (the cwd and roots are given in
/// the same textual form, so records resolve back to that form).
fn lexical_clean(path: &Path) -> PathBuf {
    let mut cleaned = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                // `..` cannot escape: cwd-joined records are root-relative.
                // Popping keeps the path honest without touching the fs.
                cleaned.pop();
            }
            _ => cleaned.push(component.as_os_str()),
        }
    }
    if cleaned.as_os_str().is_empty() {
        cleaned.push(".");
    }
    cleaned
}

/// Index of the search root that owns `path` — the root sharing the longest
/// leading-component prefix with it. `None` when no root is a prefix (the
/// match sorts last, after all root groups).
fn group_index(path: &Path, roots: &[PathBuf]) -> Option<usize> {
    let mut best: Option<(usize, usize)> = None; // (matched components, index)
    for (index, root) in roots.iter().enumerate() {
        let mut root_components = root.components().peekable();
        if root_components.peek().is_none() {
            continue;
        }
        let mut path_components = path.components();
        let mut matched = 0usize;
        for root_component in root_components.by_ref() {
            match path_components.next() {
                Some(path_component) if root_component == path_component => matched += 1,
                _ => break,
            }
        }
        // Only whole-component prefixes count (`/foo/bar` does not own
        // `/foo/baz`): every root component must have been consumed.
        if root_components.peek().is_none()
            && matched > 0
            && best.is_none_or(|(len, _)| matched > len)
        {
            best = Some((matched, index));
        }
    }
    best.map(|(_, index)| index)
}

/// Collect all match entries from a `--json` stream, resolving paths,
/// applying sensitive-file filtering, and dropping excluded matches.
///
/// Shared by the content pipeline and the files/count × exclude merge paths.
fn collect_json_entries(
    output: &[u8],
    roots: &RootContext,
    exclude_pattern: Option<&str>,
) -> anyhow::Result<(Vec<SearchMatch>, usize, bool)> {
    let exclude = exclude_pattern.map(regex::Regex::new).transpose().map_err(|e| {
        let pattern = exclude_pattern.unwrap_or_default();
        anyhow::anyhow!("Grep failed: invalid exclude_pattern '{pattern}': {e}")
    })?;
    let text = String::from_utf8_lossy(output);
    let mut entries: Vec<SearchMatch> = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let parsed: RgJsonLine = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let (data, is_match) = match parsed {
            RgJsonLine::Match { data } => (data, true),
            RgJsonLine::Context { data } => (data, false),
            _ => continue,
        };
        let absolute = roots.resolve(&data.path.text);
        if roots.is_sensitive(Path::new(&absolute)) {
            continue;
        }
        if let (Some(exclude), true) = (&exclude, is_match)
            && exclude.is_match(data.lines.text.trim_end_matches('\n'))
        {
            continue;
        }
        let column = if is_match {
            data.submatches.first().map(|s| s.start).unwrap_or(0)
        } else {
            0
        };
        entries.push(SearchMatch {
            path: absolute,
            line_number: data.line_number.unwrap_or(0),
            line: data.lines.text.trim_end_matches('\n').to_string(),
            column,
            is_match,
        });
    }
    Ok((entries, 0, false))
}

/// The effective context window for a search, mirroring `build_args`:
/// `-C` takes precedence over `-A`/`-B` when non-zero.
#[derive(Debug, Clone, Copy)]
struct ContextWindow {
    before: u32,
    after: u32,
}

impl ContextWindow {
    /// Derive the effective window from raw search params.
    fn from_params(params: &SearchParams) -> Self {
        if params.context > 0 {
            Self {
                before: params.context,
                after: params.context,
            }
        } else {
            Self {
                before: params.context_before,
                after: params.context_after,
            }
        }
    }
}

/// Parse `--json` output (content mode).
///
/// Post-parse pipeline (Task 2 design): **collect everything first, then
/// filter, then paginate** —
///
/// 1. Collect all match and context entries in output order, each keeping
///    its real line number and an `is_match` flag.
/// 2. Apply the exclude pattern (if any) to actual matches only.
/// 3. Window-trim orphan context: for each *kept* match, recompute the
///    context window `[line - C, line + C]` (with `-B`/`-A` asymmetry when
///    those were used instead of `-C`); a context line survives only if it
///    falls inside some kept match's window. Context belonging to excluded
///    matches is dropped. Note that `-B` context is emitted *before* its
///    match, so ownership cannot be assigned during collection — hence the
///    post-hoc window recomputation.
/// 4. Paginate by match count: `offset` skips the first `offset` matches,
///    `head_limit` caps how many matches are collected; context lines ride
///    along with the matches that are actually collected. `total_matches`
///    counts kept matches across the whole set.
///
/// `line_numbers` is meta-information only — the parser never zeroes line
/// numbers; display is the tool layer's concern.
fn parse_json_content(
    output: &[u8],
    roots: &RootContext,
    head_limit: usize,
    offset: usize,
    exclude_pattern: Option<&str>,
    window: ContextWindow,
) -> anyhow::Result<(Vec<SearchMatch>, usize, bool)> {
    // Step 1 + 2: full collection with exclude filtering.
    let (mut entries, _, _) = collect_json_entries(output, roots, exclude_pattern)?;

    // Step 3: window-trim orphan context. A context line survives only when
    // it falls inside some kept match's window (windows are per-file: line
    // numbers alone are not unique across files).
    if entries.iter().any(|m| !m.is_match) {
        let windows: Vec<(String, u64, u64)> = entries
            .iter()
            .filter(|m| m.is_match)
            .map(|m| {
                let line = m.line_number;
                (
                    m.path.clone(),
                    line.saturating_sub(u64::from(window.before)),
                    line.saturating_add(u64::from(window.after)),
                )
            })
            .collect();
        entries.retain(|m| {
            m.is_match
                || windows.iter().any(|(path, lo, hi)| {
                    m.path == *path && m.line_number >= *lo && m.line_number <= *hi
                })
        });
    }

    // Step 4: paginate by match count.
    let total_matches = entries.iter().filter(|m| m.is_match).count();
    let mut matches: Vec<SearchMatch> = Vec::new();
    let mut seen_matches = 0usize;
    let mut truncated = false;

    for entry in entries {
        if entry.is_match {
            if seen_matches < offset {
                seen_matches += 1;
                continue;
            }
            if matches.iter().filter(|m| m.is_match).count() >= head_limit {
                truncated = true;
                break;
            }
            seen_matches += 1;
            matches.push(entry);
        } else {
            // Context lines ride along on the page; they were already
            // window-validated, so keeping them preserves continuity.
            matches.push(entry);
        }
    }

    Ok((matches, total_matches, truncated))
}

/// Parse `--files-with-matches` output.
///
/// Each line is a file path. `offset` skips the first N entries before
/// collecting. `exclude_pattern` is accepted only so an invalid pattern is
/// reported with the same error shape as the other modes — exclude-bearing
/// files searches are routed through the `--json` path by the caller and
/// never reach this parser with a filter to apply.
fn parse_files_with_matches(
    output: &[u8],
    roots: &RootContext,
    head_limit: usize,
    offset: usize,
    exclude_pattern: Option<&str>,
) -> anyhow::Result<(Vec<SearchMatch>, usize, bool)> {
    if let Some(pattern) = exclude_pattern.map(regex::Regex::new).transpose().map_err(|e| {
        anyhow::anyhow!("Grep failed: invalid exclude_pattern '{pattern}': {e}", pattern = exclude_pattern.unwrap_or_default())
    })? {
        // Unreachable in practice (see doc comment); treated as "no filter"
        // so the plain files listing still answers.
        drop(pattern);
    }
    let text = String::from_utf8_lossy(output);
    let mut matches: Vec<SearchMatch> = Vec::new();
    let mut total_matches: usize = 0;
    let mut truncated = false;

    for line in text.lines() {
        let path = line.trim();
        if path.is_empty() {
            continue;
        }

        let absolute = roots.resolve(path);
        if roots.is_sensitive(Path::new(&absolute)) {
            continue;
        }

        total_matches += 1;

        // Skip entries within the offset range.
        if total_matches <= offset {
            continue;
        }

        if matches.len() < head_limit {
            matches.push(SearchMatch {
                path: absolute,
                line_number: 0,
                line: String::new(),
                column: 0,
                is_match: true,
            });
        } else {
            truncated = true;
        }
    }

    Ok((matches, total_matches, truncated))
}

/// Parse `--count-matches` output.
///
/// Each line is `path:count`. `offset` skips the first N entries before
/// collecting.
fn parse_count_matches(
    output: &[u8],
    roots: &RootContext,
    head_limit: usize,
    offset: usize,
) -> anyhow::Result<(Vec<SearchMatch>, usize, bool)> {
    let text = String::from_utf8_lossy(output);
    let mut matches: Vec<SearchMatch> = Vec::new();
    let mut total_matches: usize = 0;
    let mut truncated = false;

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        // Split on the last colon — paths may contain colons on Windows, but
        // ripgrep uses `:` as the separator. The count is always the final
        // segment.
        let (path, count_str) = match line.rsplit_once(':') {
            Some((p, c)) => (p, c),
            None => continue,
        };

        let count: u64 = match count_str.trim().parse() {
            Ok(v) => v,
            Err(_) => continue,
        };

        let absolute = roots.resolve(path);
        if roots.is_sensitive(Path::new(&absolute)) {
            continue;
        }

        total_matches += 1;

        // Skip entries within the offset range.
        if total_matches <= offset {
            continue;
        }

        if matches.len() < head_limit {
            matches.push(SearchMatch {
                path: absolute,
                line_number: count,
                line: String::new(),
                column: 0,
                is_match: true,
            });
        } else {
            truncated = true;
        }
    }

    Ok((matches, total_matches, truncated))
}

/// Object-safe (dyn-compatible) mirror of [`ContentSearch`].
///
/// The RPITIT trait cannot be used as `dyn ContentSearch`, so provider
/// injection goes through this erased twin: the one method returns a boxed
/// `Send` future instead. [`RipgrepSearch`] implements both; test doubles
/// implement only this trait.
pub trait DynContentSearch: Send + Sync {
    /// Search for `params.pattern` under `params.path` and return matches.
    fn search<'a>(&'a self, params: &'a SearchParams) -> DynSearchFuture<'a>;
}

/// The boxed future returned by [`DynContentSearch::search`].
pub type DynSearchFuture<'a> =
    std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<SearchResult>> + Send + 'a>>;

/// [`DynContentSearch`] implementation delegating to [`RipgrepSearch`]'s
/// [`ContentSearch`] method (the RPITIT future is `Send`, so it boxes
/// directly).
impl DynContentSearch for RipgrepSearch {
    fn search<'a>(&'a self, params: &'a SearchParams) -> DynSearchFuture<'a> {
        Box::pin(<Self as ContentSearch>::search(self, params))
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    use super::*;

    /// Serializes tests that mutate the process-global `PATH` variable (used
    /// to control which `rg` executable subprocess spawning can find).
    /// Async-aware because the guard must span the `.await` on the rg spawn —
    /// releasing early would let another test rewrite `PATH` mid-spawn.
    static ENV_LOCK: std::sync::LazyLock<tokio::sync::Mutex<()>> =
        std::sync::LazyLock::new(|| tokio::sync::Mutex::new(()));

    /// Replaces `PATH` for the duration of a test and restores it on drop.
    struct PathGuard {
        original: Option<std::ffi::OsString>,
    }

    impl PathGuard {
        fn set(value: impl AsRef<std::ffi::OsStr>) -> Self {
            let original = std::env::var_os("PATH");
            unsafe { std::env::set_var("PATH", value) };
            Self { original }
        }
    }

    impl Drop for PathGuard {
        fn drop(&mut self) {
            match &self.original {
                Some(path) => unsafe { std::env::set_var("PATH", path) },
                None => unsafe { std::env::remove_var("PATH") },
            }
        }
    }

    /// Guard `MIRRICODE_HOME` the same way [`PathGuard`] guards `PATH`: set
    /// it for the duration of a test and restore (or remove) it on drop. The
    /// rg fallback probe reads this variable, so tests that exercise the
    /// fallback (or must avoid it) pin it to a tempdir.
    struct MirriHomeGuard {
        original: Option<std::ffi::OsString>,
    }

    impl MirriHomeGuard {
        fn set(value: impl AsRef<std::ffi::OsStr>) -> Self {
            let original = std::env::var_os("MIRRICODE_HOME");
            unsafe { std::env::set_var("MIRRICODE_HOME", value) };
            Self { original }
        }
    }

    impl Drop for MirriHomeGuard {
        fn drop(&mut self) {
            match &self.original {
                Some(home) => unsafe { std::env::set_var("MIRRICODE_HOME", home) },
                None => unsafe { std::env::remove_var("MIRRICODE_HOME") },
            }
        }
    }

    /// Write a fixture file under `dir`.
    fn write_file(dir: &Path, name: &str, content: &str) {
        std::fs::write(dir.join(name), content).unwrap();
    }

    /// A portable fake `rg` that emits the JSON stream one real match would:
    /// a `begin` line, a `match` line for `<search-dir>/hello.txt`, and an
    /// `end` line. The search directory is the last CLI argument.
    const FAKE_RG_SCRIPT: &str = r#"#!/bin/sh
# Fake rg: emit one JSON match for <search-dir>/hello.txt.
for last; do :; done
printf '{"type":"begin","data":{"path":{"text":"%s/hello.txt"}}}\n' "$last"
printf '{"type":"match","data":{"path":{"text":"%s/hello.txt"},"lines":{"text":"needle line"},"line_number":1,"submatches":[{"start":0,"end":6}]}}\n' "$last"
printf '{"type":"end","data":{"path":{"text":"%s/hello.txt"}}}\n' "$last"
exit 0
"#;

    /// A fake `rg` for glob-filtered searches: reports the subprocess cwd
    /// (via `pwd -P`, because `$PWD` is inherited from the parent
    /// environment rather than reflecting the child's cwd) as the matched
    /// line text, and emits a `./relative` record path the way a real rg
    /// does when invoked with `.` as the search path.
    const FAKE_RG_CWD_SCRIPT: &str = r#"#!/bin/sh
# Fake rg: report the child cwd and emit one ./relative JSON match.
CWD=$(pwd -P)
printf '{"type":"begin","data":{"path":{"text":"./hello.txt"}}}\n'
printf '{"type":"match","data":{"path":{"text":"./hello.txt"},"lines":{"text":"%s"},"line_number":1,"submatches":[{"start":0,"end":6}]}}\n' "$CWD"
printf '{"type":"end","data":{"path":{"text":"./hello.txt"}}}\n'
exit 0
"#;

    /// Install a fake `rg` with the given script into `<dir>/bin` and return
    /// the bin directory.
    fn install_custom_rg(dir: &Path, script: &str) -> std::path::PathBuf {
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let rg = bin.join("rg");
        std::fs::write(&rg, script).unwrap();
        let mut perms = std::fs::metadata(&rg).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&rg, perms).unwrap();
        bin
    }

    /// Install the default fake `rg` (single absolute-path match) into
    /// `<dir>/bin` and return the bin directory.
    fn install_fake_rg(dir: &Path) -> std::path::PathBuf {
        install_custom_rg(dir, FAKE_RG_SCRIPT)
    }

    /// Prepend `bin` to `PATH` for the duration of the returned guard.
    fn prepend_to_path(bin: &Path) -> PathGuard {
        let mut dirs = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
            .collect::<Vec<_>>();
        dirs.insert(0, bin.to_path_buf());
        PathGuard::set(std::env::join_paths(dirs).expect("join PATH"))
    }

    #[tokio::test]
    async fn should_return_error_naming_ripgrep_when_rg_is_missing_from_path() {
        // Given a PATH that cannot contain any `rg` binary
        let _lock = ENV_LOCK.lock().await;
        let empty_bin = tempfile::tempdir().unwrap();
        let _path_guard = PathGuard::set(empty_bin.path());
        // And no cached rg under the fallback home (a machine-level
        // ~/.mirri-code/bin/rg must not rescue this test's missing-binary
        // expectation)
        let empty_home = tempfile::tempdir().unwrap();
        let _home_guard = MirriHomeGuard::set(empty_home.path());

        // And a searchable directory with a matching line
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "hello.txt", "needle line\n");
        let params = SearchParams {
            pattern: "needle".to_string(),
            paths: vec![dir.path().to_path_buf()],
            ..Default::default()
        };

        // When a content search runs
        let err = match ContentSearch::search(&RipgrepSearch::new(), &params).await {
            Ok(_) => panic!("search should fail when rg is unavailable"),
            Err(err) => err,
        };

        // Then the error explains that ripgrep could not be spawned rather
        // than panicking
        assert!(
            err.to_string().contains("ripgrep not found"),
            "error was: {err}"
        );
    }

    #[tokio::test]
    async fn should_use_the_cached_rg_binary_when_path_misses_and_cache_exists() {
        // Given a fake rg installed under the MIRRICODE_HOME bin cache
        let _lock = ENV_LOCK.lock().await;
        let home = tempfile::tempdir().unwrap();
        install_fake_rg(home.path());
        let _home_guard = MirriHomeGuard::set(home.path());
        // And a PATH that cannot contain any `rg` binary
        let empty_bin = tempfile::tempdir().unwrap();
        let _path_guard = PathGuard::set(empty_bin.path());

        // And a file with a matching line under the search root
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "hello.txt", "needle line\n");
        let params = SearchParams {
            pattern: "needle".to_string(),
            paths: vec![dir.path().to_path_buf()],
            ..Default::default()
        };

        // When a content search runs
        let result = ContentSearch::search(&RipgrepSearch::new(), &params)
            .await
            .expect("search succeeds via the cached rg");

        // Then the cached rg's match is parsed into a SearchMatch
        assert_eq!(result.matches.len(), 1);
        assert_eq!(result.matches[0].line, "needle line");
        assert_eq!(result.matches[0].line_number, 1);
    }

    #[tokio::test]
    async fn should_return_parsed_matches_when_rg_is_available_on_path() {
        // Given a fake rg that emits one JSON match, placed first on PATH
        let _lock = ENV_LOCK.lock().await;
        let tmp = tempfile::tempdir().unwrap();
        let _path_guard = prepend_to_path(&install_fake_rg(tmp.path()));

        // And a file with a matching line under the search root
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "hello.txt", "needle line\n");
        let params = SearchParams {
            pattern: "needle".to_string(),
            paths: vec![dir.path().to_path_buf()],
            ..Default::default()
        };

        // When a content search runs
        let result = ContentSearch::search(&RipgrepSearch::new(), &params)
            .await
            .expect("search succeeds");

        // Then the fake rg's match is parsed into a SearchMatch
        assert_eq!(result.matches.len(), 1);
        assert_eq!(result.matches[0].line, "needle line");
        assert_eq!(result.matches[0].line_number, 1);
        assert_eq!(result.matches[0].column, 0);
        assert!(
            result.matches[0].path.ends_with("hello.txt"),
            "path was: {}",
            result.matches[0].path
        );
        assert_eq!(result.total_matches, 1);
        assert!(!result.truncated);
    }

    #[tokio::test]
    async fn should_run_rg_from_the_search_root_when_a_glob_filter_is_active() {
        // Given a fake rg that reports its own cwd and emits a ./relative
        // record path, placed first on PATH
        let _lock = ENV_LOCK.lock().await;
        let tmp = tempfile::tempdir().unwrap();
        let _path_guard = prepend_to_path(&install_custom_rg(tmp.path(), FAKE_RG_CWD_SCRIPT));

        // And a search whose root differs from the process cwd, with a glob
        // filter active
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "hello.txt", "needle line\n");
        let params = SearchParams {
            pattern: "needle".to_string(),
            paths: vec![dir.path().to_path_buf()],
            glob: Some("src/**/*.rs".to_string()),
            ..Default::default()
        };

        // When the search runs
        let result = ContentSearch::search(&RipgrepSearch::new(), &params)
            .await
            .expect("search succeeds");

        // Then rg ran with its cwd pinned to the search root (the fake rg
        // echoes `pwd -P` as the matched line, and `current_dir` resolves
        // through the same symlinked tempdir path)
        let expected_cwd = dir.path().canonicalize().expect("canonicalize search root");
        assert_eq!(result.matches.len(), 1, "one match expected");
        assert_eq!(result.matches[0].line, expected_cwd.to_string_lossy());

        // And the ./relative record path was resolved back to the original
        // (non-canonical) absolute search-root form
        assert_eq!(
            result.matches[0].path,
            dir.path().join("hello.txt").to_string_lossy(),
            "record path should be resolved against the search root"
        );
    }

    #[tokio::test]
    async fn should_keep_the_legacy_invocation_when_no_glob_filter_is_active() {
        // Given a fake rg that emits an absolute-path match keyed on the
        // search directory passed as its last CLI argument
        let _lock = ENV_LOCK.lock().await;
        let tmp = tempfile::tempdir().unwrap();
        let _path_guard = prepend_to_path(&install_fake_rg(tmp.path()));

        // And a search without a glob filter
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "hello.txt", "needle line\n");
        let params = SearchParams {
            pattern: "needle".to_string(),
            paths: vec![dir.path().to_path_buf()],
            ..Default::default()
        };

        // When the search runs
        let result = ContentSearch::search(&RipgrepSearch::new(), &params)
            .await
            .expect("search succeeds");

        // Then rg received the absolute search root (the fake rg echoes it
        // back as the record path) — no cwd pinning, no `.` search path
        assert!(
            result.matches[0].path.ends_with("hello.txt"),
            "path was: {}",
            result.matches[0].path
        );
    }

    #[tokio::test]
    async fn should_drop_excluded_matches_and_orphan_context_lines() {
        let _lock = ENV_LOCK.lock().await;
        // Given a file with matches on lines 2(skip)/3(keep)/8(keep) searched
        // with -C 2, producing windows over lines 1-5 and 6-10
        let tmp = tempfile::tempdir().unwrap();
        let body = "noise\nnoise\nskip\nkeep\nnoise\nnoise\nnoise\nkeep\nnoise\nnoise\n";
        std::fs::write(tmp.path().join("a.txt"), body).unwrap();
        let params = SearchParams {
            pattern: "skip|keep".to_string(),
            paths: vec![tmp.path().to_path_buf()],
            output_mode: "content".to_string(),
            context: 2,
            exclude_pattern: Some("skip".to_string()),
            ..Default::default()
        };

        // When the search runs with an exclude pattern dropping the skip line
        let r = ContentSearch::search(&RipgrepSearch::new(), &params)
            .await
            .expect("search succeeds");

        // Then excluded matches do not count toward the total
        assert_eq!(r.total_matches, 2, "skip line excluded from total");
        assert!(
            r.matches.iter().all(|m| !m.line.contains("skip")),
            "{:?}",
            r.matches
        );
        // And keep@8's before-context (lines 6-7) survives while skip@2's
        // window line 1 is an orphan and is dropped
        let lines: Vec<u64> = r.matches.iter().map(|m| m.line_number).collect();
        assert!(lines.contains(&6) && lines.contains(&7), "-B context must survive: {lines:?}");
        assert!(!lines.contains(&1), "skip's orphan context must be dropped: {lines:?}");
    }

    #[tokio::test]
    async fn should_count_limit_and_total_by_matches_not_context_lines() {
        let _lock = ENV_LOCK.lock().await;
        // Given a file with 20 alternating hit/noise line pairs — 20 matches
        // each with 1 after-context line
        let tmp = tempfile::tempdir().unwrap();
        let body: String = (0..20).flat_map(|_| ["hit\n", "noise\n"]).collect();
        std::fs::write(tmp.path().join("a.txt"), &body).unwrap();
        let params = SearchParams {
            pattern: "hit".to_string(),
            paths: vec![tmp.path().to_path_buf()],
            output_mode: "content".to_string(),
            head_limit: 3,
            context_after: 1,
            ..Default::default()
        };

        // When the search runs with head_limit 3
        let r = ContentSearch::search(&RipgrepSearch::new(), &params)
            .await
            .expect("search succeeds");

        // Then the budget counts only actual matches, not context lines, and
        // total_matches counts matches across the whole result set
        assert_eq!(r.matches.iter().filter(|m| m.is_match).count(), 3, "budget by match count");
        assert_eq!(r.total_matches, 20, "total counts matches only");
    }

    #[tokio::test]
    async fn should_keep_real_line_numbers_when_line_numbers_is_false() {
        let _lock = ENV_LOCK.lock().await;
        // Given a file whose only match is on line 2, searched with
        // line_numbers=false
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "noise\nneedle\n").unwrap();
        let params = SearchParams {
            pattern: "needle".to_string(),
            paths: vec![tmp.path().to_path_buf()],
            line_numbers: false,
            ..Default::default()
        };

        // When the search runs
        let r = ContentSearch::search(&RipgrepSearch::new(), &params)
            .await
            .expect("search succeeds");

        // Then the parsed match keeps the real line number — whether to
        // display it is the tool layer's decision, not the parser's
        assert_eq!(r.matches[0].line_number, 2);
    }

    #[tokio::test]
    async fn should_apply_exclude_pattern_in_files_mode() {
        let _lock = ENV_LOCK.lock().await;
        // Given two files where a.txt's only match is excluded and b.txt has
        // a kept match
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "skip\n").unwrap();
        std::fs::write(tmp.path().join("b.txt"), "keep\n").unwrap();
        let params = SearchParams {
            pattern: "skip|keep".to_string(),
            paths: vec![tmp.path().to_path_buf()],
            output_mode: "files".to_string(),
            exclude_pattern: Some("skip".to_string()),
            ..Default::default()
        };

        // When the files-mode search runs with an exclude pattern
        let r = ContentSearch::search(&RipgrepSearch::new(), &params)
            .await
            .expect("search succeeds");

        // Then a file whose matches are all excluded disappears entirely
        let paths: Vec<&str> = r.matches.iter().map(|m| m.path.as_str()).collect();
        assert_eq!(
            paths,
            vec![tmp.path().join("b.txt").to_str().unwrap()],
            "a.txt with all matches excluded must vanish: {paths:?}"
        );
    }

    #[test]
    fn should_exclude_vcs_directories_and_pin_hidden_when_building_args() {
        // Given default params, When args are built,
        // Then hidden is always on, VCS dirs are excluded regardless of
        // include_ignored, and non-content modes get a column cap.
        let params = SearchParams {
            output_mode: "files".to_string(),
            pattern: "x".to_string(),
            ..Default::default()
        };
        let (args, _) = build_args(&params);

        // --hidden is always on (ts parity): hidden files are searched by
        // default, not only when include_ignored.
        assert!(args.windows(1).any(|w| w == ["--hidden"]), "args: {args:?}");

        // VCS directories are excluded unconditionally (spec .gitignore and
        // similar is always skipped), even with include_ignored=false.
        for dir in VCS_DIRECTORIES_TO_EXCLUDE {
            assert!(
                args.iter().any(|a| a == &format!("!{dir}")),
                "missing VCS exclusion for {dir}: {args:?}"
            );
        }

        // Non-content modes pin the 500-column cap so a huge single line cannot
        // blow up the per-path output (the cap is pointless in content mode,
        // where the actual match text must surface verbatim).
        assert!(
            args.windows(2)
                .any(|w| w[0] == "--max-columns" && w[1] == "500"),
            "missing --max-columns 500: {args:?}"
        );
    }

    #[test]
    fn should_omit_max_columns_cap_in_content_mode_when_building_args() {
        // Given content mode, When args are built,
        // Then no --max-columns cap is applied — the match line must surface
        // verbatim so the model sees the real text.
        let params = SearchParams {
            output_mode: "content".to_string(),
            pattern: "x".to_string(),
            ..Default::default()
        };
        let (args, _) = build_args(&params);
        assert!(
            !args.iter().any(|a| a == "--max-columns"),
            "content mode must not cap columns: {args:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Object-safe (dyn-compatible) mirror (zero-IO T13a)
// ---------------------------------------------------------------------------
