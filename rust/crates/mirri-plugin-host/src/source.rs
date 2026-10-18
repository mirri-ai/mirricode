//! Installation source resolution and plugin fetching.
//!
//! A plugin can be installed from three kinds of source:
//!
//! | Prefix                         | Variant   | Action                           |
//! |--------------------------------|-----------|----------------------------------|
//! | `local:/path/to/dir`           | [`PluginSource::Local`]   | Copy directory tree  |
//! | `github:user/repo`             | [`PluginSource::Github`]  | `git clone`          |
//! | `https://example.com/x.zip`    | [`PluginSource::Zip`]     | Download + extract   |
//!
//! [`parse_source`] converts a source string into a [`PluginSource`], and
//! [`install_from_source`] performs the actual fetch into a target directory.

use std::path::{Path, PathBuf};

use tracing::warn;

use crate::error::{PluginError, PluginResult};
use crate::github::{GithubRef, GithubRefKind, GithubResolver, is_full_sha};

/// Describes where a plugin was installed from.
///
/// Tagged by `type` for JSON serialization so it round-trips through
/// `installed.json` without ambiguity.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(tag = "type")]
pub enum PluginSource {
    /// A local filesystem path.
    #[serde(rename = "local")]
    Local {
        /// Absolute or relative path to the source directory.
        path: String,
    },
    /// A GitHub repository (`user/repo`).
    #[serde(rename = "github")]
    Github {
        /// Repository specifier, e.g. `"mirricode/my-plugin"`.
        repo: String,
        /// Optional commit SHA or branch/tag. If `None`, the default branch
        /// is used.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        commit: Option<String>,
        /// The ref kind when the source string spelled it out (the TS
        /// `github.com/.../tree|releases/tag|commit` grammar, A1): `sha` /
        /// `tag` are unambiguous from the URL, `tree` is `sha` for a
        /// 7-40-hex value and `branch` otherwise. `None` on `github:`
        /// prefixed sources and legacy records — the kind is then derived
        /// at install time (the historical behavior).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        kind: Option<crate::github::GithubRefKind>,
    },
    /// A ZIP archive downloaded from a URL.
    #[serde(rename = "zip")]
    Zip {
        /// HTTP(S) URL to the `.zip` archive.
        url: String,
    },
}

/// Parse a source string into a [`PluginSource`].
///
/// Accepted formats — the rust prefixes are a **superset** of the TS
/// `resolveInstallSource` grammar (A1; `agent-core-v2/app/plugin/source.ts`):
///
/// | Form | Variant | Note |
/// |---|---|---|
/// | `local:/path/to/plugin` | [`PluginSource::Local`] | rust prefix (superset) |
/// | `/path/to/plugin` (absolute) | [`PluginSource::Local`] | TS bare absolute path |
/// | `github:user/repo[@ref]` | [`PluginSource::Github`] | rust prefix (superset) |
/// | `https://github.com/user/repo` | [`PluginSource::Github`] | TS URL grammar |
/// | `https://github.com/user/repo/tree/<ref>` | [`PluginSource::Github`] | `<ref>` decoded; 7-40 hex → `sha`, else `branch` |
/// | `https://github.com/user/repo/releases/tag/<tag>` | [`PluginSource::Github`] | tag |
/// | `https://github.com/user/repo/commit/<sha>` | [`PluginSource::Github`] | sha |
/// | `https://example.com/plugin.zip` | [`PluginSource::Zip`] | same in both |
///
/// The `github.com` URL grammar mirrors the TS parser exactly: `.git`
/// suffix stripped from the repo, ref segments percent-decoded (with a raw
/// fallback on malformed escapes). A `github.com` URL matching no
/// recognized path falls through to the generic `http(s)` → zip-url arm,
/// like the TS `resolveInstallSource` does.
pub fn parse_source(source: &str) -> PluginResult<PluginSource> {
    let trimmed = source.trim();

    if trimmed.is_empty() {
        return Err(PluginError::InstallFailed(
            "source string is empty".to_string(),
        ));
    }

    // local: prefix (rust superset — also accepts relative paths, unlike
    // the TS bare-path form which requires absolution).
    if let Some(path) = trimmed.strip_prefix("local:") {
        let path = path.trim();
        if path.is_empty() {
            return Err(PluginError::InstallFailed(
                "local source path is empty".to_string(),
            ));
        }
        return Ok(PluginSource::Local {
            path: path.to_string(),
        });
    }

    // github: prefix (rust superset — optionally with @commit).
    if let Some(rest) = trimmed.strip_prefix("github:") {
        let rest = rest.trim();
        if rest.is_empty() {
            return Err(PluginError::InstallFailed(
                "github source repo is empty".to_string(),
            ));
        }
        // Split on the last '@' to separate repo from commit.
        if let Some(at_pos) = rest.rfind('@') {
            let repo = &rest[..at_pos];
            let commit = &rest[at_pos + 1..];
            if repo.is_empty() || commit.is_empty() {
                return Err(PluginError::InstallFailed(format!(
                    "invalid github source format: '{rest}'"
                )));
            }
            return Ok(PluginSource::Github {
                repo: repo.to_string(),
                commit: Some(commit.to_string()),
                kind: None,
            });
        }
        return Ok(PluginSource::Github {
            repo: rest.to_string(),
            commit: None,
            kind: None,
        });
    }

    // TS URL grammar first mirror: the `github.com/<owner>/<repo>` URL
    // (with optional `/tree|/releases/tag|/commit` ref path) — the TS
    // `parseGithubUrl` (source.ts). Must run BEFORE the generic http(s)
    // zip-url arm so github.com URLs resolve as GitHub sources, exactly
    // like the TS resolution order.
    if let Some(github) = parse_github_url(trimmed) {
        return Ok(github);
    }

    // http(s) URL → zip (shared grammar).
    if trimmed.starts_with("https://") || trimmed.starts_with("http://") {
        return Ok(PluginSource::Zip {
            url: trimmed.to_string(),
        });
    }

    // TS bare-path form: an absolute path is a local source. Relative
    // paths are rejected (the TS `path.isAbsolute` requirement — the
    // `local:` prefix remains the escape hatch for relative roots).
    if trimmed.starts_with('/') {
        return Ok(PluginSource::Local {
            path: trimmed.to_string(),
        });
    }

    Err(PluginError::InstallFailed(format!(
        "unrecognised source format: '{trimmed}'. \
         Expected 'local:<path>', an absolute path, 'github:<user/repo>', \
         a github.com URL, or an HTTP(S) URL."
    )))
}

/// The TS `parseGithubUrl` mirror (A1): parse a `github.com` (or
/// `www.github.com`) https URL into a [`PluginSource::Github`], or return
/// `None` when the URL is not a github repository URL.
///
/// Rules ported from `source.ts` verbatim (a minimal hand-rolled URL parse
/// — no `url` dependency; the inputs this grammar accepts are a fixed
/// subset of https URLs):
/// - protocol must be `https:` and the host `github.com` / `www.github.com`
///   (host matching is case-insensitive, like `new URL().hostname`);
/// - the first two path segments are `owner` / `repo` (required); query
///   strings and fragments are excluded from the path;
/// - a trailing `.git` is stripped from `repo`;
/// - `/tree/<ref>` → ref (segments joined with `/` after percent-decoding),
///   kind `sha` when it is 7-40 hex (the TS `SHA_RE`), else `branch`;
/// - `/releases/tag/<tag>` → kind `tag`;
/// - `/commit/<sha>` → kind `sha`;
/// - any other path → `None` — the caller falls back to the generic
///   `http(s)` → zip-url arm (the TS resolution order).
fn parse_github_url(raw: &str) -> Option<PluginSource> {
    let rest = raw.strip_prefix("https://")?;
    let (host, path) = match rest.split_once('/') {
        Some((host, path)) => (host, path),
        None => (rest, ""),
    };
    let host = host.to_ascii_lowercase();
    if host != "github.com" && host != "www.github.com" {
        return None;
    }
    // `window.location` parity: the path excludes the query and fragment.
    let path = path.split(['?', '#']).next().unwrap_or("");

    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let owner = segments.first()?;
    let repo_raw = segments.get(1)?;
    let repo = repo_raw.strip_suffix(".git").unwrap_or(repo_raw);
    let rest = &segments[2..];

    if rest.is_empty() {
        return Some(PluginSource::Github {
            repo: format!("{owner}/{repo}"),
            commit: None,
            kind: None,
        });
    }

    let head = rest[0];
    match head {
        "tree" if rest.len() >= 2 => {
            let value = decode_ref_segments(&rest[1..]);
            let kind = if is_ts_sha_ref(&value) {
                crate::github::GithubRefKind::Sha
            } else {
                crate::github::GithubRefKind::Branch
            };
            Some(PluginSource::Github {
                repo: format!("{owner}/{repo}"),
                commit: Some(value),
                kind: Some(kind),
            })
        }
        "releases" if rest.len() >= 3 && rest[1] == "tag" => {
            let value = decode_ref_segments(&rest[2..]);
            Some(PluginSource::Github {
                repo: format!("{owner}/{repo}"),
                commit: Some(value),
                kind: Some(crate::github::GithubRefKind::Tag),
            })
        }
        "commit" if rest.len() >= 2 => {
            let value = decode_ref_segments(&rest[1..]);
            Some(PluginSource::Github {
                repo: format!("{owner}/{repo}"),
                commit: Some(value),
                kind: Some(crate::github::GithubRefKind::Sha),
            })
        }
        _ => None,
    }
}

/// Percent-decode each path segment and join with `/` — the TS
/// `decodeRefSegments` (source.ts): `decodeURIComponent` per segment with a
/// fallback to the raw segment on malformed escapes.
fn decode_ref_segments(segments: &[&str]) -> String {
    segments
        .iter()
        .map(|segment| {
            crate::github::percent_encoding_decode(segment)
                .unwrap_or_else(|| (*segment).to_string())
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// The TS `SHA_RE` — `/^[0-9a-f]{7,40}$/` (a short or full lowercase-hex
/// SHA; `parseGithubUrl` uses it to classify `/tree/<ref>` as sha).
fn is_ts_sha_ref(value: &str) -> bool {
    (7..=40).contains(&value.len())
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Install-time GitHub metadata captured alongside the fetch — the inputs
/// `plugin.checkUpdates` later consumes.
///
/// Both fields are best-effort: a failed resolution degrades to `None`
/// (the record then follows the legacy semantics at check time) rather than
/// failing the install itself.
#[derive(Debug, Default, Clone)]
pub struct GithubInstallCapture {
    /// The resolved GitHub ref (`{kind, value}`) the install used.
    pub github_ref: Option<GithubRef>,
    /// The commit SHA the repo was cloned/checked out at.
    pub installed_sha: Option<String>,
}

/// Install a plugin from the given source into `target_dir`, capturing
/// GitHub ref metadata along the way.
///
/// - **Local**: copies the source directory tree into `target_dir`.
/// - **Github**: runs `git clone` (with optional `--branch`/checkout) into
///   `target_dir`, then records `git rev-parse HEAD` and the resolved ref
///   kind (see [`install_github`]).
/// - **Zip**: downloads the archive into memory (the TS `downloadZip`) and
///   extracts it through the in-memory [`extract_zip`] (the TS `extractZip`,
///   item A3): every entry is guarded against path escape, and the returned
///   root may be `target_dir` itself or its single manifest-carrying child
///   directory (the TS `detectPluginRoot`).
///
/// Returns the path to the installed plugin root (with the TS zip root
/// detection: `target_dir`, or the promoted single child directory) plus the
/// captured GitHub metadata (empty for local/zip sources).
pub async fn install_from_source(
    source: &PluginSource,
    target_dir: &Path,
    resolver: &dyn GithubResolver,
) -> PluginResult<(PathBuf, GithubInstallCapture)> {
    match source {
        PluginSource::Local { path } => {
            install_local(Path::new(path), target_dir)?;
            Ok((target_dir.to_path_buf(), GithubInstallCapture::default()))
        }
        PluginSource::Github { repo, commit, kind } => {
            let capture =
                install_github(repo, commit.as_deref(), *kind, target_dir, resolver).await?;
            Ok((target_dir.to_path_buf(), capture))
        }
        PluginSource::Zip { url } => {
            let root = install_zip(url, target_dir).await?;
            Ok((root, GithubInstallCapture::default()))
        }
    }
}

// ---------------------------------------------------------------------------
// Local
// ---------------------------------------------------------------------------

/// Recursively copy `src` into `dst`.
fn install_local(src: &Path, dst: &Path) -> PluginResult<()> {
    if !src.is_dir() {
        return Err(PluginError::InstallFailed(format!(
            "local source is not a directory: {}",
            src.display()
        )));
    }
    std::fs::create_dir_all(dst)?;
    copy_dir_recursive(src, dst)?;
    Ok(())
}

/// Recursively copy a directory tree. `pub(crate)` for the manager's
/// managed-copy staging step (the TS `cp(…, {recursive: true})`).
pub(crate) fn copy_dir_recursive(src: &Path, dst: &Path) -> PluginResult<()> {
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if file_type.is_dir() {
            std::fs::create_dir_all(&dst_path)?;
            copy_dir_recursive(&src_path, &dst_path)?;
        } else if file_type.is_file() {
            std::fs::copy(&src_path, &dst_path)?;
        }
        // Symlinks and other special files are skipped.
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Github
// ---------------------------------------------------------------------------

/// Clone a GitHub repository into `target_dir`.
///
/// Uses the system `git` command. If `commit` is specified, the repo is
/// cloned and then checked out at that ref.
///
/// After the clone/checkout the install-time metadata is captured (recorded,
/// never install-affecting):
///
/// - `installed_sha` — `git rev-parse HEAD` of the working tree;
/// - `github_ref` — the pinned ref classified by kind: an explicit kind
///   from the TS URL grammar (`sha`/`tag`/`branch` — the
///   `github.com/.../commit|releases/tag|tree` paths, A1) is used as-is; a
///   `github:`-prefixed source with no kind classifies as: 40-hex explicit
///   commit → `sha`; any other explicit ref → `branch` when the Atom feed
///   resolves it (branches and tags both appear there; a plain name is
///   resolved through GitHub's ref machinery, so an unresolvable name falls
///   back to `tag`); an unpinned install resolves the "latest" ref
///   (release tag first, `HEAD` fallback).
///
/// A failed metadata capture degrades to `None` (the record then follows the
/// legacy semantics at `check_updates` time) instead of failing the install.
async fn install_github(
    repo: &str,
    commit: Option<&str>,
    kind: Option<GithubRefKind>,
    target_dir: &Path,
    resolver: &dyn GithubResolver,
) -> PluginResult<GithubInstallCapture> {
    if let Some(parent) = target_dir.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let clone_url = format!("https://github.com/{repo}.git");

    let mut clone = tokio::process::Command::new("git");
    clone.arg("clone").arg(&clone_url).arg(target_dir);

    let output = clone.output().await.map_err(|e| {
        PluginError::InstallFailed(format!(
            "failed to run 'git clone' (is git installed?): {e}"
        ))
    })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(PluginError::InstallFailed(format!(
            "git clone failed for {repo}: {stderr}"
        )));
    }

    // If a commit/branch/tag is specified, check it out.
    if let Some(ref_name) = commit {
        let mut checkout = tokio::process::Command::new("git");
        checkout
            .arg("checkout")
            .arg(ref_name)
            .current_dir(target_dir);
        let output = checkout.output().await.map_err(|e| {
            PluginError::InstallFailed(format!("failed to run 'git checkout': {e}"))
        })?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(PluginError::InstallFailed(format!(
                "git checkout {ref_name} failed for {repo}: {stderr}"
            )));
        }
    }

    // --- metadata capture (best-effort, never install-affecting) ----------

    let installed_sha = match git_rev_parse_head(target_dir).await {
        Ok(sha) => Some(sha),
        Err(e) => {
            warn!(repo, error = %e, "failed to capture installed SHA");
            None
        }
    };

    let github_ref = match (commit, kind) {
        // The TS URL grammar spelled the kind out — use it as-is (A1):
        // `/commit/<sha>` pins a sha, `/releases/tag/<t>` a tag,
        // `/tree/<ref>` a branch (or a sha for a 7-40-hex value).
        (Some(ref_name), Some(kind)) => Some(GithubRef {
            kind,
            value: ref_name.to_string(),
        }),
        // Explicit 40-hex commit → a pinned SHA.
        (Some(ref_name), None) if is_full_sha(ref_name) => {
            Some(GithubRef::sha(ref_name.to_lowercase()))
        }
        // Explicit other ref: branch when the Atom feed resolves it, else
        // tag (unresolvable names fall back to tag, matching the install
        // grammar where any non-SHA ref is a branch-or-tag name).
        (Some(ref_name), None) => match resolver.resolve_commit_sha(repo, ref_name).await {
            Ok(_) => Some(GithubRef::branch(ref_name.to_string())),
            Err(_) => Some(GithubRef::tag(ref_name.to_string())),
        },
        // Unpinned: resolve the latest release tag, else HEAD.
        (None, _) => match resolver.resolve_latest_ref(repo).await {
            Ok((latest, _display)) => Some(latest),
            Err(e) => {
                warn!(repo, error = %e, "failed to resolve latest ref at install");
                None
            }
        },
    };

    Ok(GithubInstallCapture {
        github_ref,
        installed_sha,
    })
}

/// Run `git rev-parse HEAD` inside a repository and return the trimmed,
/// lowercased SHA.
async fn git_rev_parse_head(repo_dir: &Path) -> PluginResult<String> {
    let output = tokio::process::Command::new("git")
        .arg("rev-parse")
        .arg("HEAD")
        .current_dir(repo_dir)
        .output()
        .await
        .map_err(|e| PluginError::InstallFailed(format!("failed to run 'git rev-parse': {e}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(PluginError::InstallFailed(format!(
            "git rev-parse HEAD failed: {stderr}"
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .trim()
        .to_lowercase())
}

// ---------------------------------------------------------------------------
// Zip
// ---------------------------------------------------------------------------

/// Download a ZIP archive and extract it into `target_dir` (item A3, the TS
/// `downloadZip` + `extractZip` of `archive.ts`).
///
/// The archive downloads into memory (a 5-minute request timeout, the TS
/// `downloadZip` deadline) and extracts through the in-memory [`extract_zip`]
/// — no system `curl`/`unzip`, matching the TS yauzl flow. Every entry is
/// validated against path escape before writing; after extraction the plugin
/// root is detected (the TS `detectPluginRoot` single-child directory
/// promotion).
///
/// Returns the detected plugin root (the extraction directory itself, or the
/// single child directory carrying the manifest).
async fn install_zip(url: &str, target_dir: &Path) -> PluginResult<PathBuf> {
    std::fs::create_dir_all(target_dir)?;
    let bytes = download_zip(url).await?;
    extract_zip(&bytes, target_dir)
}

/// The TS `downloadZip` (`archive.ts`): fetch the archive fully into memory
/// with a 5-minute deadline. A non-2xx response fails with the TS error
/// text (`Failed to download zip: HTTP <status> <statusText>`); a transport
/// failure fails the install with the URL echoed.
pub async fn download_zip(url: &str) -> PluginResult<Vec<u8>> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5 * 60))
        .build()
        .map_err(|e| PluginError::InstallFailed(format!("failed to build HTTP client: {e}")))?;
    let response = client.get(url).send().await.map_err(|e| {
        PluginError::InstallFailed(format!("Failed to download zip: {e}"))
    })?;
    if !response.status().is_success() {
        let status = response.status();
        return Err(PluginError::InstallFailed(format!(
            "Failed to download zip: HTTP {} {}",
            status.as_u16(),
            status.canonical_reason().unwrap_or(""),
        )));
    }
    let bytes = response
        .bytes()
        .await
        .map_err(|e| PluginError::InstallFailed(format!("Failed to download zip: {e}")))?;
    Ok(bytes.to_vec())
}

/// The TS `extractZip` (`archive.ts`): extract an in-memory zip archive into
/// `dest_dir` and return the detected plugin root (the TS
/// `detectPluginRoot`).
///
/// Rules mirrored field for field:
///
/// - every entry's destination is resolved lexically against the resolved
///   destination directory and must stay inside it — a `..`/absolute path
///   escaping the directory aborts the whole extraction with the TS error
///   text `Path traversal detected in zip entry: <name>`;
/// - entries whose name ends in `/` create directories recursively;
/// - files materialize under their parent directory and then restore the
///   entry's unix permission bits (`mode & 0o777` from `externalFileAttributes
///   >>> 16`, zero meaning "leave default");
/// - symlink entries (unix mode with the `S_IFLNK` type bit) are **written as
///   regular files** holding the link-target bytes — exactly what the TS
///   yauzl flow produces (yauzl has no symlink concept, so the archive never
///   creates one; no indirection ever leaves `dest_dir`).
///
/// After all entries the plugin root is detected: `dest_dir` itself when it
/// carries a manifest, else its single child directory when that child
/// carries one (the TS `detectPluginRoot`).
pub fn extract_zip(buffer: &[u8], dest_dir: &Path) -> PluginResult<PathBuf> {
    std::fs::create_dir_all(dest_dir)?;
    // The TS `path.resolve(destDir)` — the containment check compares
    // against the resolved destination, not the caller's spelling.
    let dest_dir_resolved = lexical_absolute(dest_dir);
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(buffer))
        .map_err(|e| PluginError::InstallFailed(format!("Failed to open zip: {e}")))?;
    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .map_err(|e| PluginError::InstallFailed(format!("Failed to open zip: {e}")))?;
        let file_name = entry.name().to_string();
        let mode = entry.unix_mode();
        let dest_path = resolve_zip_entry_path(&dest_dir_resolved, &file_name).ok_or_else(|| {
            PluginError::InstallFailed(format!(
                "Path traversal detected in zip entry: {file_name}"
            ))
        })?;
        if entry.is_dir() || file_name.ends_with('/') {
            std::fs::create_dir_all(&dest_path)?;
            continue;
        }
        let mut contents = Vec::new();
        std::io::Read::read_to_end(&mut entry, &mut contents).map_err(|e| {
            PluginError::InstallFailed(format!(
                "Failed to read {file_name} from archive: {e}"
            ))
        })?;
        if let Some(parent) = dest_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&dest_path, &contents)?;
        restore_file_permissions(&dest_path, mode)?;
    }
    Ok(detect_plugin_root(&dest_dir_resolved))
}

/// The TS `path.resolve(destDir, fileName)` containment check: resolve the
/// entry name lexically against the destination directory (absolute names are
/// rejected outright — `path.resolve` would return them unchanged and the
/// containment check then fails — and `..` components collapse against the
/// destination, escaping it when they run out of components). Returns `None`
/// for an entry that escapes the destination. An entry resolving exactly to
/// the destination itself (`./`-style directory entries, which `zip -r .`
/// archives emit) is allowed — the TS check permits the equality too, and
/// the caller handles it (a directory entry no-ops the mkdir, a file entry
/// fails the write, exactly like the TS stream pipeline).
fn resolve_zip_entry_path(root: &Path, name: &str) -> Option<PathBuf> {
    let name_path = Path::new(name);
    if name_path.is_absolute() {
        return None;
    }
    let mut resolved = root.to_path_buf();
    for component in name_path.components() {
        match component {
            std::path::Component::Normal(part) => resolved.push(part),
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if !resolved.pop() {
                    return None;
                }
            }
            std::path::Component::RootDir | std::path::Component::Prefix(_) => return None,
        }
    }
    if resolved.starts_with(root) {
        Some(resolved)
    } else {
        None
    }
}

/// The TS `detectPluginRoot` (`archive.ts`): return `dir` itself when it
/// contains a manifest; otherwise its **single** child directory when that
/// child contains a manifest. Anything else (multiple children, no child
/// manifest) falls back to `dir`.
fn detect_plugin_root(dir: &Path) -> PathBuf {
    if has_manifest(dir) {
        return dir.to_path_buf();
    }
    let mut child_dirs: Vec<PathBuf> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            if entry.path().is_dir() {
                child_dirs.push(entry.path());
            }
        }
    }
    if child_dirs.len() == 1 && has_manifest(&child_dirs[0]) {
        return child_dirs[0].clone();
    }
    dir.to_path_buf()
}

/// The TS `hasManifest` (`archive.ts`): whether any of the four candidate
/// manifest paths exists as a file inside `dir`.
fn has_manifest(dir: &Path) -> bool {
    const CANDIDATES: &[&str] = &[
        "mirri.plugin.json",
        ".mirri-plugin/plugin.json",
        "kimi.plugin.json",
        ".kimi-plugin/plugin.json",
    ];
    CANDIDATES.iter().any(|name| dir.join(name).is_file())
}

/// The TS `restoreFilePermissions` (`archive.ts`): apply the entry's unix
/// permission bits (`externalFileAttributes >>> 16`, masked to `0o777`); a
/// zero mode (or no unix attributes) leaves the file's default permissions.
#[cfg(unix)]
fn restore_file_permissions(path: &Path, mode: Option<u32>) -> PluginResult<()> {
    let Some(mode) = mode else {
        return Ok(());
    };
    let permissions = mode & 0o777;
    if permissions == 0 {
        return Ok(());
    }
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(permissions))?;
    Ok(())
}

/// Non-unix hosts have no mode bits to restore (the TS `chmod` is a no-op
/// there too).
#[cfg(not(unix))]
fn restore_file_permissions(_path: &Path, _mode: Option<u32>) -> PluginResult<()> {
    Ok(())
}

/// Resolve a possibly-relative path against the current directory — the
/// `path.resolve` base the TS containment check compares against.
fn lexical_absolute(path: &Path) -> PathBuf {
    if path.is_absolute() {
        let mut resolved = PathBuf::new();
        for component in path.components() {
            match component {
                std::path::Component::ParentDir => {
                    if !resolved.pop() {
                        resolved.push("..");
                    }
                }
                std::path::Component::CurDir => {}
                other => resolved.push(other.as_os_str()),
            }
        }
        resolved
    } else {
        match std::env::current_dir() {
            Ok(cwd) => cwd.join(path),
            Err(_) => path.to_path_buf(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_parse_local_source_when_source_has_local_prefix() {
        let source = parse_source("local:/home/user/my-plugin").unwrap();
        assert_eq!(
            source,
            PluginSource::Local {
                path: "/home/user/my-plugin".to_string()
            }
        );
    }

    #[test]
    fn should_parse_github_source_when_source_has_github_prefix() {
        let source = parse_source("github:user/repo").unwrap();
        assert_eq!(
            source,
            PluginSource::Github {
                repo: "user/repo".to_string(),
                commit: None,
                kind: None
            }
        );
    }

    #[test]
    fn should_parse_github_source_with_commit_when_source_includes_at_commit() {
        let source = parse_source("github:user/repo@abc123").unwrap();
        assert_eq!(
            source,
            PluginSource::Github {
                repo: "user/repo".to_string(),
                commit: Some("abc123".to_string()),
                kind: None
            }
        );
    }

    #[test]
    fn should_parse_zip_source_when_source_is_https_url() {
        let source = parse_source("https://example.com/plugin.zip").unwrap();
        assert_eq!(
            source,
            PluginSource::Zip {
                url: "https://example.com/plugin.zip".to_string()
            }
        );
    }

    #[test]
    fn should_parse_zip_source_when_source_is_http_url() {
        let source = parse_source("http://example.com/plugin.zip").unwrap();
        assert_eq!(
            source,
            PluginSource::Zip {
                url: "http://example.com/plugin.zip".to_string()
            }
        );
    }

    #[test]
    fn should_reject_source_when_source_string_is_empty() {
        assert!(parse_source("").is_err());
        assert!(parse_source("   ").is_err());
    }

    #[test]
    fn should_reject_source_when_format_is_unrecognized() {
        assert!(parse_source("ftp://example.com/plugin").is_err());
        assert!(parse_source("just-a-name").is_err());
    }

    #[test]
    fn should_reject_source_when_local_path_is_empty() {
        assert!(parse_source("local:").is_err());
    }

    #[test]
    fn should_reject_source_when_github_repo_is_empty() {
        assert!(parse_source("github:").is_err());
    }

    #[test]
    fn should_reject_source_when_github_commit_is_empty() {
        assert!(parse_source("github:user/repo@").is_err());
        assert!(parse_source("github:@abc").is_err());
    }

    #[tokio::test]
    async fn should_copy_directory_tree_when_installing_from_local_source() {
        let src_dir = tempfile::tempdir().unwrap();
        let dst_dir = tempfile::tempdir().unwrap();

        // Create a source plugin with a manifest.
        let manifest_content = r#"{"name":"test-plugin"}"#;
        std::fs::write(src_dir.path().join("mirri.plugin.json"), manifest_content).unwrap();
        std::fs::write(src_dir.path().join("README.md"), "hello").unwrap();

        let source = PluginSource::Local {
            path: src_dir.path().to_string_lossy().to_string(),
        };
        let (installed_path, capture) = install_from_source(
            &source,
            dst_dir.path(),
            &crate::github::ReqwestGithubResolver::new(),
        )
        .await
        .expect("install should succeed");

        assert!(installed_path.join("mirri.plugin.json").is_file());
        assert!(installed_path.join("README.md").is_file());
        // Local installs capture no GitHub metadata.
        assert!(capture.github_ref.is_none());
        assert!(capture.installed_sha.is_none());
    }

    #[tokio::test]
    async fn should_return_error_when_local_source_directory_does_not_exist() {
        let dst_dir = tempfile::tempdir().unwrap();
        let source = PluginSource::Local {
            path: "/nonexistent/path/xyz".to_string(),
        };
        let result = install_from_source(
            &source,
            dst_dir.path(),
            &crate::github::ReqwestGithubResolver::new(),
        )
        .await;
        assert!(result.is_err());
    }
}
