//! Shell-path translation for the Windows shells the engine is driven from
//! (Git Bash / MSYS2 / Cygwin).
//!
//! Windows binaries normally see native paths (`C:\Users\me\proj`), but the
//! agent is often driven from a POSIX-ish shell whose paths are spelled
//! differently. Two shapes reach the engine:
//!
//! - **MSYS drive form** — `/c/Users/me/proj` (Git Bash's default). The
//!   leading segment is the drive letter, lowercased.
//! - **MSYS home form** — `/Users/me/proj`, i.e. the drive part dropped
//!   entirely. Git Bash accepts this because it maps `/` onto the MSYS
//!   installation root in some configurations, and users paste it from
//!   POSIX documentation.
//!
//! [`std::path::Path`] does not know either shape on Windows: `is_absolute()`
//! is **false** for both (a rooted path with no drive prefix is not absolute
//! there), so treating such a path as "relative to the workspace" produces
//! `C:\ws\c\Users\me\proj` for the first form and `C:\ws\Users\me\proj` for
//! the second — neither is the directory the user meant. That misresolution
//! is what made a Git-Bash-spelled workspace path fail the sandbox check
//! (and therefore stay outside YOLO's trusted scope) even though it pointed
//! straight at the workspace.
//!
//! This module translates both shapes into the native form. It is pure
//! (no I/O, no environment reads) so it is testable on every platform: the
//! Windows-only spellings are recognized on POSIX too, and returning them
//! unchanged there is the correct no-op — `C:\...` is not a meaningful path
//! on a POSIX host in the first place.

use std::path::{Component, Path, PathBuf};

/// Translate a shell-spelled Windows path into the native form.
///
/// - `/c/Users/me/proj` → `C:\Users\me\proj` (MSYS drive form)
/// - `/Users/me/proj` → `<anchor>\Users\me\proj` (MSYS home form — the drive
///   comes from `anchor`, the workspace root, never from the process cwd)
/// - anything else → `None` (the caller keeps its own handling)
///
/// `anchor` supplies the drive for the home form; it is also what makes the
/// translation correct when the workspace lives on `D:` — a bare `/Users/...`
/// then maps to `D:\Users\...`, not to a hardcoded `C:`.
///
/// Returns `None` for a path that is not a shell-spelled Windows path, so a
/// caller can chain this in front of its existing logic without changing any
/// behaviour it did not mean to touch.
///
/// **Windows only, deliberately.** On POSIX, `/c/Users/me` is a perfectly
/// ordinary absolute path — a real directory under `/c`. Translating it there
/// would corrupt a legitimate path into a Windows one, so this function
/// declines everything off Windows (the notion of a "drive letter" does not
/// exist on the host to begin with). The MSYS ambiguity is a Windows-only
/// problem because only there is a drive-less rooted path *not* absolute.
pub fn translate_msys_path(path: &Path, anchor: &Path) -> Option<PathBuf> {
    #[cfg(not(windows))]
    {
        let _ = (path, anchor);
        None
    }

    #[cfg(windows)]
    {
        translate_msys_path_on_windows(path, anchor)
    }
}

/// The Windows implementation — see [`translate_msys_path`] for the contract.
#[cfg(windows)]
fn translate_msys_path_on_windows(path: &Path, anchor: &Path) -> Option<PathBuf> {
    let raw = path.to_str()?;

    // Only `/`-rooted inputs are candidates. A native `C:\...`, a UNC path,
    // or a relative path is left to the caller.
    if !raw.starts_with('/') || raw.starts_with("//") {
        return None;
    }

    let mut segments = raw.split('/').filter(|segment| !segment.is_empty());

    // MSYS drive form: the first segment is a single ASCII letter.
    if let Some(first) = segments.clone().next()
        && is_drive_letter(first)
    {
        let drive = first.to_ascii_uppercase();
        let rest: Vec<&str> = segments.by_ref().skip(1).collect();
        return Some(join_under_root(&format!("{drive}:"), &rest));
    }

    // MSYS home form: no drive segment, so the drive comes from the anchor.
    // Without a drive-qualified anchor there is nothing to anchor to, and
    // guessing a drive would be worse than leaving the path alone.
    let anchor_drive = anchor
        .components()
        .find_map(|component| match component {
            Component::Prefix(prefix) => match prefix.kind() {
                std::path::Prefix::Disk(letter) | std::path::Prefix::VerbatimDisk(letter) => {
                    Some((letter as char).to_ascii_uppercase())
                }
                _ => None,
            },
            _ => None,
        })?;
    let rest: Vec<&str> = segments.collect();
    if rest.is_empty() {
        return None;
    }
    Some(join_under_root(&format!("{anchor_drive}:"), &rest))
}

/// Whether `segment` is a single ASCII letter — the MSYS drive spelling.
///
/// `#[cfg(windows)]`: only the Windows translation path uses it, and an
/// ungated helper would be dead code on POSIX under `-D warnings`.
#[cfg(windows)]
fn is_drive_letter(segment: &str) -> bool {
    let mut chars = segment.chars();
    matches!((chars.next(), chars.next()), (Some(c), None) if c.is_ascii_alphabetic())
}

/// Build `<root>\segment\segment...` from a `"C:"` root and POSIX segments.
///
/// `#[cfg(windows)]` for the same reason as [`is_drive_letter`].
#[cfg(windows)]
fn join_under_root(root: &str, segments: &[&str]) -> PathBuf {
    let mut out = PathBuf::from(format!("{root}{}", std::path::MAIN_SEPARATOR));
    for segment in segments {
        out.push(segment);
    }
    out
}

/// Normalize a path for comparison: drop the Windows verbatim prefix
/// (`\\?\C:\x` → `C:\x`) and fold the drive letter's case.
///
/// `std::fs::canonicalize` returns verbatim paths on Windows, while callers
/// and configuration carry the plain spelling. `Path::starts_with` compares
/// `Component::Prefix` values structurally, so `\\?\C:\x` never "starts with"
/// `C:\x` even though they name the same directory — the containment check
/// then reports an escape. Folding the drive case covers the other half:
/// `c:\x` and `C:\x` are the same directory on Windows but different
/// components byte-wise.
///
/// On a POSIX host this is a no-op beyond rebuilding the path: none of the
/// Windows prefix kinds can occur there.
pub fn normalize_for_comparison(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    let mut verbatim_unc_root: Option<String> = None;
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => match prefix.kind() {
                // `\\?\C:\x` — the drive lives INSIDE the verbatim prefix,
                // so it must be re-emitted as a plain drive component.
                std::path::Prefix::VerbatimDisk(letter) => {
                    out.push(format!("{}:", (letter as char).to_ascii_uppercase()));
                }
                // A plain drive keeps its letter, case-folded.
                std::path::Prefix::Disk(letter) => {
                    out.push(format!("{}:", (letter as char).to_ascii_uppercase()));
                }
                // `\\?\UNC\server\share` — rebuild the plain UNC root. The
                // share is emitted here so the RootDir component below has
                // nothing left to add.
                std::path::Prefix::VerbatimUNC(server, share) => {
                    verbatim_unc_root = Some(format!(
                        "{sep}{sep}{server}{sep}{share}",
                        sep = std::path::MAIN_SEPARATOR,
                        server = server.to_string_lossy(),
                        share = share.to_string_lossy(),
                    ));
                }
                // Any other verbatim form (a volume GUID, a device path) has
                // no plain equivalent to fold into — keep it as written
                // rather than silently rewriting the target.
                _ => out.push(component.as_os_str()),
            },
            Component::RootDir if verbatim_unc_root.is_some() => {
                // The rebuilt UNC root already names the share.
            }
            other => push_for_comparison(&mut out, other),
        }
    }
    if let Some(root) = verbatim_unc_root {
        // The rebuilt root must be the prefix of the result, and the loop
        // above assembled only the components after it.
        let tail = out.as_os_str().to_string_lossy().into_owned();
        return PathBuf::from(format!("{root}{tail}"));
    }
    out
}

/// Push one component, preserving separators while keeping the drive root
/// intact (`C:` must be followed by a separator before a normal component).
fn push_for_comparison(out: &mut PathBuf, component: Component<'_>) {
    match component {
        Component::RootDir => {
            // Ensure the previous push (a bare `C:`) gets its separator.
            if matches!(
                out.components().next_back(),
                Some(Component::Prefix(_)) | None
            ) {
                let text = out.as_os_str().to_string_lossy().into_owned();
                *out = PathBuf::from(format!("{text}{}", std::path::MAIN_SEPARATOR));
            } else {
                out.push(component.as_os_str());
            }
        }
        other => out.push(other.as_os_str()),
    }
}

#[cfg(test)]
mod tests {
    // layer: T0
    use super::*;

    /// The MSYS drive form (`/c/...`) translates to the drive-qualified
    /// native path.
    #[test]
    #[cfg(windows)]
    fn should_translate_the_msys_drive_form_to_a_drive_qualified_path() {
        // Given a Git-Bash-spelled absolute path
        let raw = Path::new("/c/Users/me/proj");

        // When it is translated against any anchor
        let translated = translate_msys_path(raw, Path::new(r"C:\ws"))
            .expect("the MSYS drive form is translatable on Windows");

        // Then the drive survives and no `c` segment is left behind
        let text = translated.to_string_lossy().replace('\\', "/");
        assert_eq!(text, "C:/Users/me/proj");
        assert!(
            !text.contains("/c/"),
            "the drive segment must not survive as a directory name"
        );
    }

    /// On POSIX, `/c/Users/me` is a REAL absolute path — a directory under
    /// `/c` — and must never be rewritten into a Windows spelling. The
    /// translation declines everything off Windows for exactly this reason.
    #[test]
    #[cfg(unix)]
    fn should_decline_the_msys_spelling_on_posix() {
        for raw in ["/c/Users/me/proj", "/Users/me/proj", "/d/work/repo"] {
            assert!(
                translate_msys_path(Path::new(raw), Path::new("/tmp/ws")).is_none(),
                "{raw} is an ordinary POSIX path and must pass through untouched"
            );
        }
    }

    /// A lowercase drive letter is folded up, matching the canonical
    /// spelling Windows APIs report.
    #[test]
    #[cfg(windows)]
    fn should_fold_a_lowercase_drive_letter_when_translating() {
        let translated = translate_msys_path(Path::new("/d/work/repo"), Path::new(r"C:\ws"))
            .expect("translatable");
        assert_eq!(
            translated.to_string_lossy().replace('\\', "/"),
            "D:/work/repo"
        );
    }

    /// The MSYS home form (`/Users/...`, no drive) takes its drive from the
    /// anchor — the workspace root — so a workspace on `D:` still resolves
    /// correctly instead of being pinned to `C:`.
    #[test]
    #[cfg(windows)]
    fn should_take_the_drive_from_the_anchor_when_the_form_has_none() {
        // Given a home-form path and a D:-drive anchor
        let translated = translate_msys_path(Path::new("/Users/me/proj"), Path::new(r"D:\work\ws"));

        // Then the drive comes from the anchor, never a hardcoded C:
        let translated = translated.expect("a Windows anchor always has a drive");
        assert_eq!(
            translated.to_string_lossy().replace('\\', "/"),
            "D:/Users/me/proj"
        );
    }

    /// A native path, a relative path, and a UNC path are all left alone —
    /// the translation must not claim inputs it does not own.
    #[test]
    fn should_decline_paths_that_are_not_shell_spelled() {
        let anchor = Path::new("C:\\work\\ws");
        for raw in ["C:\\Users\\me", "relative/dir", "\\\\server\\share", ""] {
            assert!(
                translate_msys_path(Path::new(raw), anchor).is_none(),
                "{raw:?} is not an MSYS spelling and must pass through"
            );
        }
    }

    /// A lone `/` is not a translatable path (there is no content to name).
    #[test]
    fn should_decline_a_bare_root() {
        assert!(translate_msys_path(Path::new("/"), Path::new("C:\\ws")).is_none());
        assert!(translate_msys_path(Path::new("//"), Path::new("C:\\ws")).is_none());
    }

    /// Comparison normalization folds the drive case so `c:\x` and `C:\x`
    /// compare equal, and leaves the rest of the path untouched.
    ///
    /// `#[cfg(windows)]`: `c:\Users\Me\Proj` is only a *drive-qualified
    /// path* on Windows. On POSIX it is a single relative component, so the
    /// two spellings differ in their content and folding them is not the
    /// right behavior there — the assertion would be testing Windows
    /// semantics on a host that has none.
    #[test]
    #[cfg(windows)]
    fn should_fold_the_drive_case_when_normalizing_for_comparison() {
        let lowered = normalize_for_comparison(Path::new("c:\\Users\\Me\\Proj"));
        let raised = normalize_for_comparison(Path::new("C:\\Users\\Me\\Proj"));
        assert_eq!(lowered, raised);
        assert!(
            lowered.to_string_lossy().contains("Users\\Me"),
            "only the drive folds — the rest keeps its spelling: {}",
            lowered.display()
        );
    }

    /// On Windows the verbatim prefix canonicalize produces (`\\?\C:\x`)
    /// must compare equal to the plain spelling — otherwise the containment
    /// check reports an escape for every path the sandbox itself resolved.
    ///
    /// `#[cfg(windows)]`: `\\?\` is a Windows verbatim prefix; on POSIX the
    /// same text is an ordinary (and different) path.
    #[test]
    #[cfg(windows)]
    fn should_strip_the_verbatim_prefix_when_normalizing_for_comparison() {
        let plain = normalize_for_comparison(Path::new("C:\\Users\\me"));
        let verbatim = normalize_for_comparison(Path::new("\\\\?\\C:\\Users\\me"));
        assert_eq!(plain, verbatim);
    }

    /// On POSIX, comparison normalization is a structural no-op: a path
    /// survives unchanged, which is what keeps the existing unix verdicts
    /// byte-identical.
    #[test]
    #[cfg(unix)]
    fn should_leave_a_posix_path_unchanged_when_normalizing_for_comparison() {
        for raw in ["/Users/me/proj", "relative/dir", "/"] {
            assert_eq!(
                normalize_for_comparison(Path::new(raw)),
                PathBuf::from(raw),
                "{raw} must survive comparison normalization unchanged"
            );
        }
    }
}
