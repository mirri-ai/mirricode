//! Content search via ripgrep.
//!
//! This module provides a [`ContentSearch`] trait and a [`RipgrepSearch`]
//! implementation that wraps the `rg` (ripgrep) subprocess. It mirrors the
//! TypeScript search integration: pattern matching, glob/type filtering,
//! sensitive-file filtering, output-size limits, and head-limit truncation.

use std::path::PathBuf;
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
    /// Root directory to search in.
    pub path: PathBuf,
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
    /// Output mode: `"content"`, `"files_with_matches"`, or `"count_matches"`.
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
    /// `true`. When `false`, `line_number` is set to 0 in parsed matches.
    pub line_numbers: bool,
    /// Include files excluded by `.gitignore`.
    pub include_ignored: bool,
}

impl Default for SearchParams {
    fn default() -> Self {
        Self {
            pattern: String::new(),
            path: PathBuf::new(),
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
/// In `content` mode all fields are populated. In `files_with_matches` mode
/// only `path` is meaningful (`line_number` is 0, `line` is empty). In
/// `count_matches` mode `line_number` carries the per-file match count.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SearchMatch {
    /// Path of the file that matched.
    pub path: String,
    /// 1-based line number of the match. In `count_matches` mode this holds the
    /// total match count for the file.
    #[serde(rename = "lineNumber")]
    pub line_number: u64,
    /// The text of the matching line.
    pub line: String,
    /// 0-based column offset of the match within the line.
    pub column: u64,
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
        "files_with_matches" => {
            args.push("--files-with-matches".to_string());
        }
        "count_matches" => {
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
    let mut cmd = tokio::process::Command::new("rg");
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

/// Decide the rg search path and subprocess cwd for a search.
///
/// When a glob filter is active, ripgrep matches a `--glob` pattern
/// containing a `/` against paths relative to its own cwd — with an
/// absolute search path, such a pattern is matched against absolute paths
/// and silently matches nothing. So rg runs with its cwd pinned to the
/// search root and `.` as the search path, making glob matching relative to
/// that root (mirrors the TS Grep/Glob tools' F11 fix). Output records then
/// carry `./relative` paths, which [`search_with_ripgrep`] resolves back to
/// the original absolute form. When the root is a single file the absolute
/// path is still passed explicitly (cwd pinned to its parent); without a
/// glob filter nothing depends on rg's cwd, so the legacy invocation is
/// kept.
///
/// Returns `(rg_search_path, rg_cwd, resolve_root)` where `resolve_root` is
/// `Some` only for the `.`-rooted directory case.
fn rg_invocation(params: &SearchParams) -> (String, Option<PathBuf>, Option<PathBuf>) {
    let absolute = params.path.to_string_lossy().into_owned();
    if params.glob.is_none() {
        return (absolute, None, None);
    }
    match std::fs::metadata(&params.path) {
        Ok(md) if md.is_dir() => (
            ".".to_string(),
            Some(params.path.clone()),
            Some(params.path.clone()),
        ),
        // A single-file search root cannot serve as a cwd; pin to its parent.
        Ok(_) => (
            absolute,
            params.path.parent().map(std::path::Path::to_path_buf),
            None,
        ),
        // A missing root is reported by rg itself; keep the legacy shape so
        // the spawn does not fail on a nonexistent cwd.
        Err(_) => (absolute, None, None),
    }
}

/// Run the full search pipeline: spawn rg, parse output, filter sensitive
/// files, apply head_limit.
async fn search_with_ripgrep(params: &SearchParams) -> anyhow::Result<SearchResult> {
    let (rg_search_path, rg_cwd, resolve_root) = rg_invocation(params);
    let (mut args, mode) = build_args(params);

    // The pattern and path always come last.
    args.push(params.pattern.clone());
    args.push(rg_search_path);

    let (output, truncated_by_size) = run_ripgrep(&args, rg_cwd.as_deref()).await?;

    let path_access = PathAccess::new(params.path.clone());

    let (mut matches, total_matches, truncated) = match mode.as_str() {
        "files_with_matches" => {
            parse_files_with_matches(&output, &path_access, params.head_limit, params.offset)?
        }
        "count_matches" => {
            parse_count_matches(&output, &path_access, params.head_limit, params.offset)?
        }
        _ => parse_json_content(
            &output,
            &path_access,
            params.head_limit,
            params.offset,
            params.line_numbers,
        )?,
    };

    // rg ran with `.` as the search path, so records carry `./relative`
    // paths; resolve them back to the original absolute form before the
    // results leave the search layer.
    if let Some(root) = resolve_root {
        for m in matches.iter_mut() {
            if let Some(rel) = m.path.strip_prefix("./") {
                m.path = root.join(rel).to_string_lossy().into_owned();
            }
        }
    }

    Ok(SearchResult {
        matches,
        total_matches,
        truncated: truncated || truncated_by_size,
    })
}

/// Parse `--json` output (content mode).
///
/// Returns `(matches, total_matches, truncated)`.
///
/// `offset` skips the first N entries (match or context) before collecting.
/// `head_limit` caps the number of collected entries. `total_matches` counts
/// only actual regex matches (not context lines). When `line_numbers` is
/// `false`, `line_number` is zeroed in the output.
fn parse_json_content(
    output: &[u8],
    path_access: &PathAccess,
    head_limit: usize,
    offset: usize,
    line_numbers: bool,
) -> anyhow::Result<(Vec<SearchMatch>, usize, bool)> {
    let text = String::from_utf8_lossy(output);
    let mut matches: Vec<SearchMatch> = Vec::new();
    let mut total_matches: usize = 0;
    let mut total_entries: usize = 0;
    let mut truncated = false;

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

        // Sensitive-file filtering at the result layer.
        if path_access.is_sensitive(std::path::Path::new(&data.path.text)) {
            continue;
        }

        if is_match {
            total_matches += 1;
        }
        total_entries += 1;

        // Skip entries within the offset range.
        if total_entries <= offset {
            continue;
        }

        if matches.len() < head_limit {
            let column = if is_match {
                data.submatches.first().map(|s| s.start).unwrap_or(0)
            } else {
                0
            };
            matches.push(SearchMatch {
                path: data.path.text,
                line_number: if line_numbers {
                    data.line_number.unwrap_or(0)
                } else {
                    0
                },
                line: data.lines.text.trim_end_matches('\n').to_string(),
                column,
            });
        } else {
            truncated = true;
        }
    }

    Ok((matches, total_matches, truncated))
}

/// Parse `--files-with-matches` output.
///
/// Each line is a file path. `offset` skips the first N entries before
/// collecting.
fn parse_files_with_matches(
    output: &[u8],
    path_access: &PathAccess,
    head_limit: usize,
    offset: usize,
) -> anyhow::Result<(Vec<SearchMatch>, usize, bool)> {
    let text = String::from_utf8_lossy(output);
    let mut matches: Vec<SearchMatch> = Vec::new();
    let mut total_matches: usize = 0;
    let mut truncated = false;

    for line in text.lines() {
        let path = line.trim();
        if path.is_empty() {
            continue;
        }

        if path_access.is_sensitive(std::path::Path::new(path)) {
            continue;
        }

        total_matches += 1;

        // Skip entries within the offset range.
        if total_matches <= offset {
            continue;
        }

        if matches.len() < head_limit {
            matches.push(SearchMatch {
                path: path.to_string(),
                line_number: 0,
                line: String::new(),
                column: 0,
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
    path_access: &PathAccess,
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

        if path_access.is_sensitive(std::path::Path::new(path)) {
            continue;
        }

        total_matches += 1;

        // Skip entries within the offset range.
        if total_matches <= offset {
            continue;
        }

        if matches.len() < head_limit {
            matches.push(SearchMatch {
                path: path.to_string(),
                line_number: count,
                line: String::new(),
                column: 0,
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

        // And a searchable directory with a matching line
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "hello.txt", "needle line\n");
        let params = SearchParams {
            pattern: "needle".to_string(),
            path: dir.path().to_path_buf(),
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
            path: dir.path().to_path_buf(),
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
            path: dir.path().to_path_buf(),
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
            path: dir.path().to_path_buf(),
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

    #[test]
    fn should_exclude_vcs_directories_and_pin_hidden_when_building_args() {
        // Given default params, When args are built,
        // Then hidden is always on, VCS dirs are excluded regardless of
        // include_ignored, and non-content modes get a column cap.
        let params = SearchParams {
            output_mode: "files_with_matches".to_string(),
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
            args.windows(2).any(|w| w[0] == "--max-columns" && w[1] == "500"),
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
