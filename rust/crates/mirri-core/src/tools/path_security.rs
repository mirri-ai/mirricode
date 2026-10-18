//! Workspace path classification for file and search tools.
//!
//! The file tools (Read/Write/Edit), the search tools (Glob/Grep), and the
//! media reader (ReadMediaFile) accept a path from the model or from a direct
//! RPC caller. Untrusted paths must not silently leave the workspace root —
//! otherwise a crafted `../../etc/passwd`, an absolute `/etc/passwd`, or a
//! symlink pointing outside the workspace could read or overwrite arbitrary
//! files. [`WorkspacePaths::classify`] is the single classification point: it
//! resolves the requested path lexically (`.`/`..`), canonicalizes it through
//! the deepest existing ancestor (so symlink escapes are resolved, while a
//! not-yet-created Write target still validates), verifies containment in
//! the workspace root **or any registered additional dir**, and returns a
//! three-state verdict:
//!
//! - [`PathClassification::Inside`] — inside the sandbox, carry on (unchanged);
//! - [`PathClassification::Outside`] — a clean absolute path outside every
//!   base; the tool no longer refuses it, the permission chain authorizes it;
//! - [`PathClassification::Escape`] — a relative `..` escape, a symlink
//!   resolving outside, or an unreadable symlink: hard-refused at the tool
//!   layer, never authorizable.
//!
//! [`WorkspacePaths::validate`] is kept as the thin Inside→Ok / else→Err
//! wrapper for pre-existing call sites (mirri-server RPC, mirri-embedded),
//! and [`validate_workspace_path`] remains the root-only wrapper.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use mirri_os::{DynFileSystem, LocalFs};

/// Error returned when a requested path is not inside the workspace root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspacePathError {
    message: String,
}

impl WorkspacePathError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for WorkspacePathError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for WorkspacePathError {}

/// Three-state classification of a requested path against the workspace
/// sandbox, replacing the old refuse-everything-outside validation for the
/// file tools (plan 2026-10-10-sandbox-path-permission, D1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathClassification {
    /// Inside the sandbox (the root or any registered additional dir),
    /// carrying the canonical path to operate on — missing Write targets
    /// included. Directly runnable, unchanged from the pre-existing shape.
    Inside(PathBuf),
    /// A clean absolute path outside every base: lexical normalization
    /// landed outside and canonicalization found no symlink escape. Carries
    /// the canonical path. The tool must NOT refuse this shape: it enters
    /// the permission chain for authorization (`WorkspaceBoundaryPolicy`,
    /// plan Task 2).
    Outside(PathBuf),
    /// An escape shape that must never be authorized: a relative path that
    /// lexical `..` normalization pushes out of the sandbox, a path that
    /// resolves outside through a symlink, or a symlink that cannot be read.
    /// Carries the human-readable reason for the refusal message.
    Escape { reason: String },
}

/// The session-shared workspace sandbox: the workspace root plus the
/// additional workspace directories added at runtime.
///
/// Mirrors the TS kernel's `WorkspaceConfig { workspaceDir, additionalDirs }`
/// consumption (`path-access.ts` `isWithinWorkspace` — the primary
/// `workspaceDir` OR any `additionalDirs` entry): a requested path is
/// allowed when it sits inside the root or inside any registered
/// additional dir.
///
/// The handle is session-scoped: [`SessionScope`] constructs one per
/// session (root = the session's workspace root) and every agent created
/// in that session — including btw forks and subagents — receives the same
/// `Arc`, so `session.addAdditionalDir` extends the sandbox for the whole
/// session at once (the TS `setAdditionalDirs` walks every ready agent
/// with the same effect). The additional list is interior-mutable because
/// the tools are constructed once at agent creation.
///
/// [`SessionScope`]: crate::scope::SessionScope
pub struct WorkspacePaths {
    /// The workspace root — the primary allowed base. Canonicalized at
    /// validation time (per call, exactly like the root-only
    /// [`validate_workspace_path`] wrapper).
    root: PathBuf,
    /// The additional allowed bases, deduplicated on insert. `std::sync`
    /// because tool path validation is a synchronous path.
    additional: std::sync::RwLock<Vec<PathBuf>>,
}

impl WorkspacePaths {
    /// Create a sandbox confined to `root` with no additional dirs.
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            additional: std::sync::RwLock::new(Vec::new()),
        }
    }

    /// The primary workspace root, as configured.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The registered additional dirs (canonicalized where they existed at
    /// registration time), in insertion order.
    pub fn additional(&self) -> Vec<PathBuf> {
        self.additional
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Register one additional allowed base.
    ///
    /// The dir is canonicalized when it exists (so dedup compares like
    /// forms, e.g. `/var` vs `/private/var` on macOS); a not-yet-existing
    /// dir keeps its lexical form — it simply never matches a canonical
    /// requested path until it exists. Duplicate adds are ignored.
    pub fn add_additional(&self, dir: PathBuf) {
        let dir = dir.canonicalize().unwrap_or(dir); // zero-io-allow: baseline Path IO, see F-T13-2
        let mut additional = self.additional.write().unwrap_or_else(|e| e.into_inner());
        if !additional.iter().any(|existing| existing == &dir) {
            additional.push(dir);
        }
    }

    /// Validate that `requested` lies inside the workspace root or any
    /// registered additional dir, and return the canonical path to operate
    /// on.
    ///
    /// Thin `Inside → Ok` wrapper over [`WorkspacePaths::classify`] kept for
    /// the pre-existing call sites (mirri-server RPC, mirri-embedded): every
    /// non-Inside verdict — including a clean `Outside` absolute path — is an
    /// error here, because these call sites have no permission chain to
    /// escalate to. The file tools use [`WorkspacePaths::classify_with_fs`]
    /// and route `Outside` into the authorization flow instead.
    ///
    /// The symlink probes go through the engine-default local filesystem;
    /// callers holding an injected provider use
    /// [`WorkspacePaths::validate_with_fs`] instead.
    pub fn validate(&self, requested: &Path) -> Result<PathBuf, WorkspacePathError> {
        let fs: Arc<dyn DynFileSystem> = Arc::new(LocalFs::new());
        self.validate_with_fs(&fs, requested)
    }

    /// [`WorkspacePaths::validate`] with an explicitly injected filesystem
    /// provider: the symlink detection (`stat`) and link-target read
    /// (`read_link`) during canonicalization go through `fs` instead of the
    /// engine default (zero-IO T13a).
    pub fn validate_with_fs(
        &self,
        fs: &Arc<dyn DynFileSystem>,
        requested: &Path,
    ) -> Result<PathBuf, WorkspacePathError> {
        match self.classify_with_fs(fs, requested) {
            PathClassification::Inside(canonical) => Ok(canonical),
            PathClassification::Outside(_) => Err(WorkspacePathError::new(format!(
                "path '{}' is outside the workspace sandbox '{}'",
                requested.display(),
                self.root.display()
            ))),
            PathClassification::Escape { reason } => Err(WorkspacePathError::new(format!(
                "path '{}' is outside the workspace sandbox '{}': {reason}",
                requested.display(),
                self.root.display()
            ))),
        }
    }

    /// Classify `requested` against the sandbox using the engine-default
    /// local filesystem (see [`WorkspacePaths::classify_with_fs`] for the
    /// decision order).
    pub fn classify(&self, requested: &Path) -> PathClassification {
        let fs: Arc<dyn DynFileSystem> = Arc::new(LocalFs::new());
        self.classify_with_fs(&fs, requested)
    }

    /// Classify `requested` against the sandbox with an explicitly injected
    /// filesystem provider, deciding one of three verdicts in this order:
    ///
    /// 1. Relative paths resolve against the workspace root, and `..` /
    ///    `.` segments are normalized lexically without I/O.
    /// 2. A *relative* path whose lexical normalization lands outside every
    ///    base is a [`PathClassification::Escape`]: a relative path can
    ///    never declare outside intent, so there is nothing to authorize.
    /// 3. [`canonicalize_allow_missing`] failure (unreadable symlink, no
    ///    existing ancestor) is a [`PathClassification::Escape`] carrying
    ///    the resolution error as its reason.
    /// 4. The canonical path inside any base (root or an additional dir) is
    ///    [`PathClassification::Inside`].
    /// 5. The canonical path outside every base: when the *lexical* path was
    ///    inside a base, a symlink carried the path out — that is an
    ///    [`PathClassification::Escape`]; otherwise the input was an explicit
    ///    absolute path already outside, i.e. [`PathClassification::Outside`].
    ///
    /// The symlink probes go through `fs` instead of the engine default
    /// (zero-IO T13a).
    pub fn classify_with_fs(
        &self,
        fs: &Arc<dyn DynFileSystem>,
        requested: &Path,
    ) -> PathClassification {
        let root = match self.root.canonicalize() {
            // zero-io-allow: baseline Path IO, see F-T13-2
            Ok(root) => root,
            Err(error) => {
                return PathClassification::Escape {
                    reason: format!(
                        "workspace root '{}' cannot be canonicalized: {error}",
                        self.root.display()
                    ),
                };
            }
        };

        // Resolve relative paths against the workspace root so `..` cannot
        // escape the containment check by targeting the process cwd.
        let requested_absolute = if requested.is_absolute() {
            requested.to_path_buf()
        } else {
            root.join(requested)
        };

        // Lexical normalization: drop `.`, resolve `..` without I/O.
        let normalized = lexically_normalize(&requested_absolute);

        // Canonical containment bases: the canonical root plus every
        // additional dir canonicalized the same way (a base that cannot be
        // resolved never matches — deny, never fail open).
        let canonical_bases = self.canonical_bases(fs, &root);

        // Lexical containment bases: the requested path is written inside
        // the sandbox when it sits under the configured root spelling or its
        // canonical spelling (aliases such as `/var` vs `/private/var` on
        // macOS are legitimate inside spellings). The canonical containment
        // below uses only canonical bases; this list only answers "was the
        // request written as inside?".
        let lexical_bases = self.lexical_bases(&root);
        let lexically_inside = lexical_bases
            .iter()
            .any(|base| normalized.starts_with(base));

        // A relative path that lexically escapes every base is an escape
        // shape: refused, not routed to the authorization flow.
        if !lexically_inside && !requested.is_absolute() {
            return PathClassification::Escape {
                reason: "a relative path cannot escape the workspace; if the target is \
                     intentionally outside, state it as an absolute path"
                    .to_string(),
            };
        }

        let canonical = match canonicalize_allow_missing(fs.as_ref(), &normalized) {
            Ok(canonical) => canonical,
            Err(error) => {
                return PathClassification::Escape {
                    reason: error.to_string(),
                };
            }
        };

        if canonical_bases
            .iter()
            .any(|base| canonical.starts_with(base))
        {
            return PathClassification::Inside(canonical);
        }

        // Canonical outside every base while the lexical path was inside one
        // means a symlink carried the path out of the sandbox — an escape.
        // Otherwise the input was an explicit absolute path already outside.
        if lexically_inside {
            PathClassification::Escape {
                reason: "the path resolves outside the workspace sandbox through a symlink"
                    .to_string(),
            }
        } else {
            PathClassification::Outside(canonical)
        }
    }

    /// The canonical containment bases: the canonical root plus every
    /// additional dir canonicalized with the same allow-missing resolution
    /// the containment check uses.
    fn canonical_bases(&self, fs: &Arc<dyn DynFileSystem>, root: &Path) -> Vec<PathBuf> {
        let mut bases = vec![root.to_path_buf()];
        for dir in self
            .additional
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
        {
            if let Ok(base) = canonicalize_allow_missing(fs.as_ref(), dir) {
                bases.push(base);
            }
        }
        bases
    }

    /// The lexical containment bases: every base in its configured spelling
    /// plus its canonical spelling, so an alias-written request (e.g. `/var`
    /// for the canonical `/private/var` on macOS) still counts as written
    /// inside the sandbox.
    fn lexical_bases(&self, canonical_root: &Path) -> Vec<PathBuf> {
        let mut bases = vec![
            lexically_normalize(&self.root),
            canonical_root.to_path_buf(),
        ];
        for dir in self
            .additional
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
        {
            bases.push(dir.clone());
        }
        bases
    }
}

impl std::fmt::Debug for WorkspacePaths {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkspacePaths")
            .field("root", &self.root)
            .field("additional", &self.additional())
            .finish()
    }
}

/// Validate that `requested` lies inside `workspace_root` and return the
/// canonical path to operate on.
///
/// Root-only thin wrapper over [`WorkspacePaths::validate`] with an empty
/// additional list — the pre-existing call sites keep their root-only
/// semantics.
///
/// - Relative `requested` paths are resolved against `workspace_root` (never
///   against the process working directory), so a relative path can only ever
///   resolve inside the workspace.
/// - `..` segments are normalized lexically and symlinks are resolved via
///   canonicalization, so `../secret` and `/workspace/link -> /etc` escapes
///   are detected.
/// - The `requested` path may not exist yet (e.g. a Write target): the
///   deepest existing ancestor is canonicalized and the missing tail is
///   re-appended.
pub fn validate_workspace_path(
    workspace_root: &Path,
    requested: &Path,
) -> Result<PathBuf, WorkspacePathError> {
    WorkspacePaths::new(workspace_root.to_path_buf()).validate(requested)
}

/// Classify a tool-supplied path when a workspace sandbox is configured.
///
/// `None` (no workspace wired, e.g. unit tests or legacy hosts) keeps
/// the previous behavior: the path passes through as
/// [`PathClassification::Inside`] without classification. The symlink probes
/// during classification go through the injected `fs` provider (zero-IO
/// T13a).
pub(crate) fn classify_tool_path(
    workspace_paths: Option<&WorkspacePaths>,
    fs: &Arc<dyn DynFileSystem>,
    path: &str,
) -> PathClassification {
    match workspace_paths {
        Some(paths) => paths.classify_with_fs(fs, Path::new(path)),
        None => PathClassification::Inside(PathBuf::from(path)),
    }
}

/// The tool-level refusal message for an escape-classified path.
///
/// Never names an alternative tool or workaround: the only sanctioned route
/// for an intentionally-outside target is re-stating it as an absolute path,
/// which the authorization flow evaluates (zero-alternative-guidance
/// contract, plan §2).
pub(crate) fn escape_tool_error(tool: &str, path: &str, reason: &str) -> String {
    format!(
        "{tool} refused '{path}': the path escapes the workspace sandbox ({reason}). \
         If the target is intentionally outside the workspace, state it as an \
         absolute path so the authorization flow can evaluate it."
    )
}

/// Lexically normalize a path: drop `.` segments and resolve `..` against
/// previous `Normal` components without any I/O.
///
/// Absolute paths are the primary input here ([`WorkspacePaths::classify_with_fs`]
/// joins relative inputs against the canonical root first), so a `..` at the
/// root is absorbed; a leading `..` of a bare relative path is preserved.
fn lexically_normalize(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                let popped = matches!(
                    normalized.components().next_back(),
                    Some(std::path::Component::Normal(_))
                ) && {
                    normalized.pop();
                    true
                };
                // A Leading `..` of a relative path cannot be collapsed
                // (there is no parent to pop into); keep it.
                if !popped && normalized.as_os_str().is_empty() {
                    normalized.push("..");
                }
            }
            component => normalized.push(component.as_os_str()),
        }
    }
    normalized
}

/// Canonicalize `path` even when the final component does not exist yet:
/// canonicalize the deepest existing ancestor, then re-append the missing
/// tail. `..` segments are normalized lexically before any filesystem I/O,
/// so no symlink resolution is skipped for the missing part.
///
/// **Symlink safety:** if the *final* component exists as a symlink (even a
/// dangling one whose target does not exist yet), the symlink itself is
/// canonicalized so the containment check sees the real target. This
/// prevents a workspace-local dangling symlink from bypassing validation
/// only to be followed by `O_CREAT` on write.
///
/// The symlink probes (`stat`, `read_link`) go through the injected `fs`
/// provider. Validation is synchronous (`Tool::resolve_execution`), but the
/// provider's methods are async — the probes are bridged with the same
/// escape-thread + shared blocking runtime pattern the hook engine uses
/// (issue 15C): a fresh scoped OS thread blocks on the shared multi-threaded
/// runtime, which is safe on both current-thread and multi-threaded runtimes
/// and from inside another runtime's async context.
fn canonicalize_allow_missing(
    fs: &dyn DynFileSystem,
    path: &Path,
) -> Result<PathBuf, WorkspacePathError> {
    // Lexical normalization: drop `.` and resolve `..` without I/O.
    let normalized: PathBuf = path.components().collect();
    let mut existing = normalized.as_path();
    let mut missing_tail: Vec<std::path::Component<'_>> = Vec::new();
    while !existing.exists() {
        // zero-io-allow: baseline Path IO, see F-T13-2
        // Before popping, check if the final component is a *dangling
        // symlink* — `exists()` returns false for dangling symlinks, but
        // the symlink itself exists and must be resolved for containment.
        if let Ok(stat) = block_on_fs(fs.stat(existing))
            && stat.is_symlink
        {
            // The symlink target may or may not exist. Canonicalize
            // the symlink path itself — on dangling symlinks this
            // fails, so we read the link target lexically and
            // re-check containment on the target.
            if let Ok(canonical) = existing.canonicalize() {
                // zero-io-allow: baseline Path IO, see F-T13-2
                return Ok(canonical);
            }
            // Dangling symlink: read the link target and validate
            // that instead.
            if let Ok(target) = block_on_fs(fs.read_link(existing)) {
                let resolved = if target.is_absolute() {
                    target
                } else {
                    existing.parent().unwrap_or(Path::new(".")).join(target)
                };
                // Recurse on the resolved target — if it exists, it
                // will canonicalize; if not, the loop continues
                // stripping ancestors.
                return canonicalize_allow_missing(fs, &resolved);
            }
            // Cannot read the symlink — treat as an error.
            return Err(WorkspacePathError::new(format!(
                "path '{}' contains an unreadable symlink",
                path.display()
            )));
        }
        let Some(component) = existing.components().next_back() else {
            return Err(WorkspacePathError::new(format!(
                "path '{}' has no existing ancestor to canonicalize against",
                path.display()
            )));
        };
        missing_tail.push(component);
        existing = existing.parent().ok_or_else(|| {
            WorkspacePathError::new(format!(
                "path '{}' has no existing ancestor to canonicalize against",
                path.display()
            ))
        })?;
    }
    let mut canonical = existing.canonicalize().map_err(|e| {
        // zero-io-allow: baseline Path IO, see F-T13-2
        WorkspacePathError::new(format!(
            "path '{}' cannot be canonicalized: {e}",
            path.display()
        ))
    })?;
    for component in missing_tail.iter().rev() {
        canonical.push(component.as_os_str());
    }
    Ok(canonical)
}

/// Run one async filesystem probe to completion from this synchronous
/// validation path (see [`canonicalize_allow_missing`] for why the probe
/// cannot simply be awaited).
fn block_on_fs<T: Send>(future: impl Future<Output = T> + Send) -> T {
    std::thread::scope(|scope| {
        scope
            .spawn(move || crate::hook_engine::shared_blocking_runtime().block_on(future))
            .join()
            .expect("filesystem probe thread panicked")
    })
}

/// True when `path` points at a sensitive file (secrets, credentials, keys)
/// that search/results tools must never surface to the model.
///
/// Detection rules (mirroring `mirri_os::path_access::PathAccess::is_sensitive`,
/// extended for the tool-result layer):
///
/// 1. `.env` files — filename is `.env`, or starts with `.env.` but is not
///    one of the known non-sensitive example variants (`.env.example`,
///    `.env.sample`, `.env.template`).
/// 2. Private-key material — `*.pem`, `*.key`, and SSH private-key basenames
///    (`id_rsa*`, `id_ecdsa*`, `id_ed25519*`, `id_dsa*`; public `*.pub`
///    variants are not sensitive).
/// 3. Credential stores — filenames starting with `credentials` (covers
///    `.aws/credentials`, `credentials.json`, `credentials.yml`, ...),
///    `.npmrc`, and `.git-credentials`.
/// 4. Whole sensitive directories — anything under a `.ssh` or `.git`
///    component.
///
/// The check is purely lexical: no filesystem access is performed.
pub fn is_sensitive_file(path: &Path) -> bool {
    let file_name = match path.file_name().and_then(|n| n.to_str()) {
        Some(name) => name,
        None => return false,
    };

    // Whole-directory rules.
    let contains_component = |name: &str| {
        path.components().any(|component| match component {
            std::path::Component::Normal(part) => part == name,
            _ => false,
        })
    };
    if contains_component(".ssh") || contains_component(".git") {
        return true;
    }

    // .env files — example/sample/template variants are safe to surface.
    if is_sensitive_env_file(file_name) {
        return true;
    }

    // Private keys.
    if file_name.ends_with(".pem") || file_name.ends_with(".key") {
        return true;
    }
    if is_ssh_private_key_basename(file_name) {
        return true;
    }

    // Credential stores.
    if file_name.starts_with("credentials") {
        return true;
    }
    if matches!(file_name, ".npmrc" | ".git-credentials") {
        return true;
    }

    false
}

/// True when `file_name` is a sensitive env file: `.env`, `.env.<anything>`
/// except the known example/sample/template variants.
fn is_sensitive_env_file(file_name: &str) -> bool {
    if file_name == ".env" {
        return true;
    }
    if let Some(rest) = file_name.strip_prefix(".env.") {
        let non_sensitive = ["example", "sample", "template"];
        return !non_sensitive.contains(&rest);
    }
    false
}

/// True when `file_name` is an SSH private-key basename (`id_rsa`,
/// `id_ecdsa`, `id_ed25519`, `id_dsa`, or any `id_*` variant thereof), but not
/// a public key (`*.pub`).
fn is_ssh_private_key_basename(file_name: &str) -> bool {
    const PRIVATE_KEY_PREFIXES: &[&str] = &["id_rsa", "id_ecdsa", "id_ed25519", "id_dsa"];
    PRIVATE_KEY_PREFIXES
        .iter()
        .any(|prefix| file_name.starts_with(prefix) && !file_name.ends_with(".pub"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_accept_path_inside_workspace_when_validating() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("src").join("main.rs");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "fn main() {}").unwrap();

        let validated = validate_workspace_path(tmp.path(), &file).expect("path is inside");
        assert_eq!(validated, file.canonicalize().unwrap());
    }

    #[test]
    fn should_classify_relative_dotdot_outside_as_escape_when_classifying() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = WorkspacePaths::new(tmp.path().to_path_buf());

        // A relative path pushed out of the sandbox by `..` is an escape
        // shape: a relative path can never declare outside intent, so it is
        // refused instead of routed to the authorization flow.
        let classification = paths.classify(Path::new("../escape.txt"));
        let PathClassification::Escape { reason } = classification else {
            panic!("relative dotdot escape must classify as Escape, got {classification:?}");
        };
        assert!(
            reason.contains("a relative path cannot escape the workspace"),
            "reason: {reason}"
        );
        assert!(
            reason.contains("absolute path"),
            "reason must point at the absolute-path channel: {reason}"
        );

        // The validate wrapper stays a thin Inside→Ok / else→Err surface for
        // legacy callers.
        let error = paths
            .validate(Path::new("../escape.txt"))
            .expect_err("escapes");
        assert!(error.to_string().contains("outside the workspace sandbox"));
    }

    #[test]
    fn should_classify_absolute_outside_path_as_outside_when_classifying() {
        let tmp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let outside_file = outside.path().join("passwd");
        std::fs::write(&outside_file, "root:x:0:0").unwrap();

        let paths = WorkspacePaths::new(tmp.path().to_path_buf());
        let classification = paths.classify(&outside_file);
        let PathClassification::Outside(canonical) = classification else {
            panic!(
                "a clean absolute outside path must classify as Outside, got {classification:?}"
            );
        };
        assert_eq!(canonical, outside_file.canonicalize().unwrap());

        // The validate wrapper still refuses it for legacy callers, but with
        // the new wording.
        let error = paths.validate(&outside_file).expect_err("outside");
        assert!(error.to_string().contains("outside the workspace sandbox"));
    }

    #[test]
    fn should_classify_absolute_dotdot_path_outside_as_outside_when_classifying() {
        let base = tempfile::tempdir().unwrap();
        let root = base.path().join("root");
        let outside_target = base.path().join("outside").join("hosts");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(base.path().join("outside")).unwrap();
        std::fs::write(&outside_target, "127.0.0.1 localhost").unwrap();
        let paths = WorkspacePaths::new(root.clone());

        // An *absolute* path whose lexical `..` chain lands outside is an
        // explicit outside declaration — it classifies as Outside (only
        // relative-path `..` escapes are Escape shapes).
        let traversal = root.join("..").join("outside").join("hosts");
        let PathClassification::Outside(canonical) = paths.classify(&traversal) else {
            panic!("an absolute lexical-outside path must classify as Outside");
        };
        assert_eq!(canonical, outside_target.canonicalize().unwrap());
    }

    #[test]
    fn should_accept_missing_write_target_inside_workspace_when_validating() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("new").join("deep").join("file.txt");

        let validated = validate_workspace_path(tmp.path(), &target).expect("inside workspace");
        // The canonical path may differ lexically (e.g. `/var` -> `/private/var`
        // on macOS); it must equal the canonicalized root + the missing tail.
        let expected = tmp
            .path()
            .canonicalize()
            .unwrap()
            .join("new")
            .join("deep")
            .join("file.txt");
        assert_eq!(validated, expected);
    }

    #[test]
    fn should_resolve_relative_path_against_workspace_root_when_validating() {
        let tmp = tempfile::tempdir().unwrap();

        let validated =
            validate_workspace_path(tmp.path(), Path::new("relative.txt")).expect("inside");
        assert_eq!(
            validated,
            tmp.path().canonicalize().unwrap().join("relative.txt")
        );
    }

    #[cfg(unix)]
    #[test]
    fn should_classify_symlink_escape_as_escape_when_classifying() {
        let tmp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let secret = outside.path().join("secret.txt");
        std::fs::write(&secret, "s3cret").unwrap();
        std::os::unix::fs::symlink(&secret, tmp.path().join("leak.txt")).unwrap();

        let paths = WorkspacePaths::new(tmp.path().to_path_buf());
        let classification = paths.classify(&tmp.path().join("leak.txt"));
        let PathClassification::Escape { reason } = classification else {
            panic!("a symlink escape must classify as Escape, got {classification:?}");
        };
        assert!(
            reason.contains("through a symlink"),
            "reason explains the symlink: {reason}"
        );

        let error = paths
            .validate(&tmp.path().join("leak.txt"))
            .expect_err("symlink escapes");
        assert!(error.to_string().contains("outside the workspace sandbox"));
    }

    #[cfg(unix)]
    #[test]
    fn should_classify_dangling_symlink_escape_as_escape_when_classifying() {
        let tmp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        // Dangling symlink: target does not exist, but the symlink does.
        // Without symlink_metadata check, this would pass validation and
        // a Write tool would create the file outside the workspace.
        let dangling_target = outside.path().join("authorized_keys");
        std::os::unix::fs::symlink(&dangling_target, tmp.path().join("leak.txt")).unwrap();

        let paths = WorkspacePaths::new(tmp.path().to_path_buf());
        let classification = paths.classify(&tmp.path().join("leak.txt"));
        let PathClassification::Escape { reason } = classification else {
            panic!("a dangling symlink escape must classify as Escape, got {classification:?}");
        };
        assert!(
            reason.contains("through a symlink"),
            "reason explains the symlink: {reason}"
        );
    }

    #[test]
    fn should_classify_path_as_inside_passthrough_when_no_workspace_configured() {
        let path = "/arbitrary/absolute/path.txt";
        let default_fs: Arc<dyn DynFileSystem> = Arc::new(LocalFs::new());
        let classification = classify_tool_path(None, &default_fs, path);
        assert_eq!(
            classification,
            PathClassification::Inside(PathBuf::from(path))
        );
    }

    #[test]
    fn should_refuse_with_outside_sandbox_wording_when_validating_an_outside_path() {
        let tmp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let file = outside.path().join("x.txt");
        std::fs::write(&file, "x").unwrap();

        let error = validate_workspace_path(tmp.path(), &file).expect_err("outside");
        let message = error.to_string();
        assert!(
            message.contains("outside the workspace sandbox"),
            "message: {message}"
        );
        assert!(
            !message.contains("path traversal detected"),
            "the old 'path traversal detected' wording is retired: {message}"
        );
    }

    #[test]
    fn should_classify_path_as_escape_when_the_workspace_root_cannot_be_canonicalized() {
        let missing_root = tempfile::tempdir().unwrap().path().join("does-not-exist");
        let paths = WorkspacePaths::new(missing_root.clone());
        let classification = paths.classify(Path::new("/etc/hosts"));
        let PathClassification::Escape { reason } = classification else {
            panic!("an unresolvable root must classify as Escape, got {classification:?}");
        };
        assert!(
            reason.contains("cannot be canonicalized"),
            "reason: {reason}"
        );
    }

    /// Given a workspace sandbox with no additional dir registered, When a
    /// path under a foreign directory is classified, Then it is `Outside`,
    /// not a tool refusal — the authorization flow decides (the allowlist
    /// must never open the sandbox on its own, F-P11a-3a).
    #[test]
    fn should_classify_unregistered_dir_path_as_outside_when_classifying() {
        let root = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let file = other.path().join("secret.txt");
        std::fs::write(&file, "secret").unwrap();

        let paths = WorkspacePaths::new(root.path().to_path_buf());
        let PathClassification::Outside(_) = paths.classify(&file) else {
            panic!("an unregistered-dir absolute path must classify as Outside");
        };
    }

    /// Given an additional dir registered on the sandbox, When a path inside
    /// it (including a not-yet-existing tail) is validated, Then it is
    /// admitted with its canonical path (F-P11a-3a — the TS
    /// `isWithinWorkspace` workDir-OR-any-additionalDir mirror).
    #[test]
    fn should_accept_a_path_inside_an_additional_dir_when_validating() {
        let root = tempfile::tempdir().unwrap();
        let additional = tempfile::tempdir().unwrap();
        let existing = additional.path().join("note.txt");
        std::fs::write(&existing, "content").unwrap();

        let paths = WorkspacePaths::new(root.path().to_path_buf());
        paths.add_additional(additional.path().to_path_buf());

        let validated = paths
            .validate(&existing)
            .expect("inside the additional dir");
        assert_eq!(validated, existing.canonicalize().unwrap());
        let missing = additional.path().join("new").join("file.txt");
        let validated_missing = paths
            .validate(&missing)
            .expect("a missing Write target inside the additional dir validates");
        assert_eq!(
            validated_missing,
            additional
                .path()
                .canonicalize()
                .unwrap()
                .join("new")
                .join("file.txt")
        );
    }

    /// Given additional dirs registered on the sandbox, When a path outside
    /// both the root and every additional dir is classified, Then it is
    /// `Outside`, not a tool refusal — the authorization flow decides (the
    /// additional dirs widen the sandbox, they do not disable it, F-P11a-3a).
    #[test]
    fn should_classify_unrelated_path_as_outside_when_additional_dirs_are_registered() {
        let root = tempfile::tempdir().unwrap();
        let additional = tempfile::tempdir().unwrap();
        let unrelated = tempfile::tempdir().unwrap();
        let file = unrelated.path().join("elsewhere.txt");
        std::fs::write(&file, "out").unwrap();

        let paths = WorkspacePaths::new(root.path().to_path_buf());
        paths.add_additional(additional.path().to_path_buf());

        let PathClassification::Outside(_) = paths.classify(&file) else {
            panic!("an unrelated-dir absolute path must classify as Outside");
        };
    }

    /// Given an additional dir `/…/a`, When a path under its sibling-prefix
    /// spelling `/…/abc` is classified, Then it is `Outside` — containment is
    /// component-based, a longer sibling name never matches (the
    /// shared-prefix escape `isWithinDirectory` blocks in the TS kernel).
    #[test]
    fn should_classify_sibling_prefix_path_as_outside_when_classifying() {
        let base = tempfile::tempdir().unwrap();
        let additional = base.path().join("a");
        let sibling = base.path().join("abc");
        std::fs::create_dir_all(&additional).unwrap();
        std::fs::create_dir_all(&sibling).unwrap();
        let file = sibling.join("poison.txt");
        std::fs::write(&file, "no").unwrap();

        let root = tempfile::tempdir().unwrap();
        let paths = WorkspacePaths::new(root.path().to_path_buf());
        paths.add_additional(additional.clone());

        let PathClassification::Outside(_) = paths.classify(&file) else {
            panic!("a sibling prefix must not match, so the path is Outside");
        };
    }

    /// Given an additional dir already registered, When the same dir is
    /// added again (possibly through a non-canonical alias), Then the list
    /// stays deduplicated — the sandbox never grows duplicate bases.
    #[test]
    fn should_deduplicate_repeated_additional_dirs_when_registering() {
        let root = tempfile::tempdir().unwrap();
        let additional = tempfile::tempdir().unwrap();

        let paths = WorkspacePaths::new(root.path().to_path_buf());
        paths.add_additional(additional.path().to_path_buf());
        // A `.`-segment alias resolves to the same canonical dir.
        let alias = additional.path().join(".");
        paths.add_additional(alias);

        assert_eq!(paths.additional().len(), 1);
    }

    #[test]
    fn should_flag_env_files_as_sensitive_and_skip_example_variants_when_checking() {
        assert!(is_sensitive_file(Path::new("/repo/.env")));
        assert!(is_sensitive_file(Path::new("/repo/config/.env.production")));
        assert!(!is_sensitive_file(Path::new("/repo/.env.example")));
        assert!(!is_sensitive_file(Path::new("/repo/.env.sample")));
        assert!(!is_sensitive_file(Path::new("/repo/.env.template")));
        assert!(!is_sensitive_file(Path::new("/repo/env.txt")));
    }

    #[test]
    fn should_flag_key_material_and_credentials_as_sensitive_when_checking() {
        assert!(is_sensitive_file(Path::new("server.pem")));
        assert!(is_sensitive_file(Path::new("private.key")));
        assert!(is_sensitive_file(Path::new("id_rsa")));
        assert!(is_sensitive_file(Path::new("id_rsa.backup")));
        assert!(is_sensitive_file(Path::new("id_ed25519")));
        assert!(
            !is_sensitive_file(Path::new("id_rsa.pub")),
            "public keys are not sensitive"
        );
        assert!(is_sensitive_file(Path::new("/repo/.aws/credentials")));
        assert!(is_sensitive_file(Path::new("/repo/credentials.json")));
    }

    #[test]
    fn should_flag_sensitive_dotfiles_and_directories_when_checking() {
        assert!(is_sensitive_file(Path::new("/repo/.npmrc")));
        assert!(is_sensitive_file(Path::new("/repo/.git-credentials")));
        assert!(is_sensitive_file(Path::new("/home/alice/.ssh/config")));
        assert!(is_sensitive_file(Path::new("/repo/.git/objects/ab/1234")));
    }

    #[test]
    fn should_not_flag_ordinary_files_as_sensitive_when_checking() {
        assert!(!is_sensitive_file(Path::new("/repo/src/main.rs")));
        assert!(!is_sensitive_file(Path::new("/repo/README.md")));
        assert!(!is_sensitive_file(Path::new("/repo/credentialing.rs")));
        assert!(!is_sensitive_file(Path::new("/repo/.npmrc.example")));
    }
    // -------------------------------------------------------------------------
    // T-13a-2: injected filesystem provider for symlink detection
    // -------------------------------------------------------------------------

    /// A [`mirri_os::DynFileSystem`] double with a scripted symlink at
    /// `link_path`: `stat` reports a symlink exactly there, `read_link`
    /// returns the (escaping) target for it (T-13a-2). Every other probe
    /// reports a plain missing regular file. All other operations keep the
    /// trait's default errors.
    struct SymlinkEscapeFs {
        link_path: PathBuf,
        link_target: PathBuf,
    }

    impl mirri_os::DynFileSystem for SymlinkEscapeFs {
        fn stat<'a>(
            &'a self,
            path: &'a Path,
        ) -> mirri_os::DynFsFuture<'a, anyhow::Result<mirri_os::FileStat>> {
            let is_symlink = path == self.link_path;
            Box::pin(async move {
                Ok(mirri_os::FileStat {
                    size: 1,
                    is_dir: false,
                    is_file: !is_symlink,
                    is_symlink,
                    mtime: 0,
                    atime: 0,
                    mode: 0o777,
                })
            })
        }

        fn read_link<'a>(
            &'a self,
            path: &'a Path,
        ) -> mirri_os::DynFsFuture<'a, anyhow::Result<PathBuf>> {
            let target = self.link_target.clone();
            Box::pin(async move {
                if path == target {
                    // Not a symlink — reading the link target itself must
                    // fail so the ancestor-stripping loop proceeds.
                    anyhow::bail!("not a symlink: {}", path.display());
                }
                Ok(target)
            })
        }
    }

    /// Given a workspace sandbox whose injected filesystem reports every
    /// probed path as a symlink whose target lies outside the root, When a
    /// path under the root is classified with that provider, Then the
    /// classification is `Escape` — the resolution went through the fake's
    /// `stat` (is_symlink) and `read_link` (escape target) (T-13a-2).
    #[test]
    fn should_classify_a_symlink_escape_through_the_fake_filesystem_when_classifying() {
        let tmp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let link_path = tmp.path().join("leak.txt");
        let fs: Arc<dyn DynFileSystem> = Arc::new(SymlinkEscapeFs {
            link_path: link_path.clone(),
            link_target: outside.path().join("authorized_keys"),
        });
        let paths = WorkspacePaths::new(tmp.path().to_path_buf());
        // Nothing exists at this path on disk — only the injected fake
        // claims it is a symlink.
        let requested = link_path;

        let classification = paths.classify_with_fs(&fs, &requested);
        let PathClassification::Escape { reason } = classification else {
            panic!("the fake symlink escape must classify as Escape, got {classification:?}");
        };
        assert!(
            reason.contains("through a symlink"),
            "reason explains the symlink: {reason}"
        );

        // With the engine-default (real local) filesystem the same path is
        // a plain missing Write target inside the root and classifies as
        // Inside — proving the Escape above came from the injected provider.
        // (Compare against the canonicalized root — on macOS `/var`
        // resolves to `/private/var`.)
        let PathClassification::Inside(canonical) = paths.classify(&requested) else {
            panic!("a missing in-root target must classify as Inside on the real fs");
        };
        assert!(canonical.starts_with(tmp.path().canonicalize().unwrap()));
    }
}
