//! WorkDir-key derivation — the TS kernel's per-workspace session bucket
//! naming, reproduced exactly (see `packages/agent-core/src/session/store/
//! workdir-key.ts` and `packages/agent-core/src/utils/workdir-slug.ts`).
//!
//! The TS engine stores sessions under
//! `<sessionsDir>/<workspaceId>/<sessionId>/` where `workspaceId` is
//! `wd_<slug>_<hash12>`:
//!
//! - `slug` — the normalized path's basename, lowercased, every run of
//!   characters outside `[a-z0-9._-]` collapsed to a single `-`, leading /
//!   trailing `-` trimmed, truncated to 40 chars (and trimmed again — a
//!   truncation can expose a new trailing `-`). Empty / `.` / `..` results
//!   fall back to `workspace`.
//! - `hash12` — the first 12 hex chars of `sha256(normalized_path)`.
//!
//! CRITICAL parity detail: the hashed path is the **lexically resolved**
//! absolute path (TS `resolve()`), NOT the symlink-resolved one. Rust's
//! [`std::fs::canonicalize`] resolves symlinks and would hash a different
//! string on symlinked homes/workspaces, so [`lexically_resolve`] performs
//! the normalization manually: make absolute against the cwd, then clean
//! `.` / `..` / repeated separators without touching the filesystem.

use std::path::{Component, Path, PathBuf};

use sha2::{Digest, Sha256};

/// The prefix every workspace key carries (TS `WORKDIR_KEY_PREFIX`).
const WORKDIR_KEY_PREFIX: &str = "wd_";
/// How many hex characters of the sha256 digest the key carries (TS
/// `HASH_LENGTH`).
const HASH_LENGTH: usize = 12;
/// Maximum slug length before truncation (TS `MAX_WORKDIR_SLUG_LENGTH`).
const MAX_WORKDIR_SLUG_LENGTH: usize = 40;

/// Derive the workspace key for a working directory:
/// `wd_<slug>_<hash12>` — byte-for-byte the TS kernel's `encodeWorkDirKey`.
///
/// The Rust session manager uses this as the per-workspace bucket directory
/// name under the shared sessions root, so a rust-written session lands in
/// exactly the bucket the TS kernel would compute for the same workspace.
pub fn encode_work_dir_key(work_dir: &Path) -> String {
    let normalized = lexically_resolve(work_dir);
    // The hash input is the normalized path's string form — lossy for
    // non-UTF-8 paths, matching the TS side where the input is always a
    // JS string.
    let normalized_str = normalized.to_string_lossy();
    let slug = slugify_work_dir_name(file_name_str(&normalized));
    let digest = Sha256::digest(normalized_str.as_bytes());
    let hash = hex_encode(&digest)[..HASH_LENGTH].to_string();
    format!("{WORKDIR_KEY_PREFIX}{slug}_{hash}")
}

/// Lexically resolve a path to its absolute, cleaned form — the TS
/// `resolve()` semantics: an absolute path is kept, a relative one is joined
/// onto the current directory, then `.` segments are dropped, `..` segments
/// pop their parent (a `..` at the root is dropped — one cannot go above
/// `/`), and repeated separators collapse. Symlinks are **never** followed
/// (unlike [`std::fs::canonicalize`]) because the TS side hashes the
/// lexical form.
pub fn lexically_resolve(path: &Path) -> PathBuf {
    // Make absolute against the cwd first, exactly as TS `resolve()` does
    // for relative inputs. A missing cwd (extreme embedded environments)
    // keeps the path as given — best effort, never a failure.
    let joined: PathBuf = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(path)
    };

    // Clean component by component. `Path::components()` already collapses
    // repeated separators and drops interior `.` entries, but KEEPS `..` —
    // so `..` is handled here by popping the last normal component.
    let mut resolved = if joined.has_root() {
        PathBuf::from("/")
    } else {
        PathBuf::new()
    };
    for component in joined.components() {
        match component {
            Component::RootDir | Component::Prefix(_) => { /* the leading `/` is already laid */ }
            Component::CurDir => { /* `.` is a no-op (leading `.` survives components()) */ }
            Component::ParentDir => {
                // Pop the parent; at the root the pop is a no-op (`/..` → `/`).
                resolved.pop();
            }
            Component::Normal(segment) => resolved.push(segment),
        }
    }
    resolved
}

/// The normalized path's basename as a `&str` (lossy for non-UTF-8 — the
/// slug replacement would drop the raw bytes anyway).
fn file_name_str(path: &Path) -> &str {
    // `file_name()` returns None for `/`, `..`, and `.` — all of which
    // slugify to the `workspace` fallback, so the empty string is the
    // correct stand-in.
    path.file_name().and_then(|n| n.to_str()).unwrap_or("")
}

/// Slugify a workspace basename — the TS `slugifyWorkDirName` algorithm
/// (see the module doc for the step-by-step derivation).
fn slugify_work_dir_name(name: &str) -> String {
    // Lowercase first (TS `.toLowerCase()`), then collapse every run of
    // characters outside [a-z0-9._-] into a single `-`. The regex runs on
    // the lowercased string, so only the ASCII-safe set survives.
    let lowered = name.to_lowercase();
    let mut slug = String::with_capacity(lowered.len());
    let mut in_bad_run = false;
    for ch in lowered.chars() {
        if ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '.' || ch == '_' || ch == '-' {
            slug.push(ch);
            in_bad_run = false;
        } else if !in_bad_run {
            // A consecutive run of bad characters becomes one dash (the
            // `+` quantifier in the TS regex).
            slug.push('-');
            in_bad_run = true;
        }
    }

    // Trim leading/trailing dashes, truncate to the max length, and trim
    // again — a truncation can expose a new trailing dash.
    let trimmed = trim_dashes(&slug);
    let mut truncated: String = trimmed.chars().take(MAX_WORKDIR_SLUG_LENGTH).collect();
    truncated = trim_dashes(&truncated);

    if truncated.is_empty() || truncated == "." || truncated == ".." {
        "workspace".to_string()
    } else {
        truncated
    }
}

/// Strip leading and trailing `-` characters (TS `/^-+|-+$/g`).
fn trim_dashes(s: &str) -> String {
    s.trim_matches('-').to_string()
}

/// Lowercase hex encoding of a digest slice (no `hex` crate dependency —
/// the digest is 32 bytes, the format is trivial).
fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The expected `hash12` for a path, computed through sha2 directly —
    /// the same primitive the TS side uses, so the tests assert the
    /// composition rather than a hardcoded digest.
    fn expected_hash(path: &str) -> String {
        use sha2::Digest;
        hex_encode(&Sha256::digest(path.as_bytes()))[..HASH_LENGTH].to_string()
    }

    #[test]
    fn should_derive_the_full_key_when_given_a_plain_workspace_path() {
        // Given a plain absolute path
        // When the key is derived
        let key = encode_work_dir_key(Path::new("/Users/dev/project"));
        // Then it is wd_<slug>_<hash12> with the slug from the basename
        // and the hash from sha256 of the normalized path
        assert_eq!(
            key,
            format!("wd_project_{}", expected_hash("/Users/dev/project"))
        );
    }

    #[test]
    fn should_slug_spaces_into_a_single_dash_when_basename_has_runs() {
        // Given a basename with a run of spaces and mixed bad characters
        let key = encode_work_dir_key(Path::new("/Users/dev/my   project!!"));
        // Then the whole run collapses into ONE dash (the `+` quantifier)
        assert_eq!(
            key,
            format!(
                "wd_my-project_{}",
                expected_hash("/Users/dev/my   project!!")
            )
        );
    }

    #[test]
    fn should_slug_unicode_to_a_dash_when_basename_has_non_ascii() {
        // Given a basename made entirely of unicode characters
        let key = encode_work_dir_path_str("/home/dev/プロジェクト");
        // Then the whole non-ASCII run collapses to ONE dash, which the
        // trim then removes — the slug falls back to `workspace` while the
        // hash still covers the original path
        assert_eq!(
            key,
            format!("wd_workspace_{}", expected_hash("/home/dev/プロジェクト"))
        );
    }

    #[test]
    fn should_fall_back_to_workspace_when_the_slug_is_empty() {
        // Given a path whose basename slugifies to the empty string ("/")
        let key = encode_work_dir_key(Path::new("/"));
        // Then the fallback slug is used
        assert_eq!(key, format!("wd_workspace_{}", expected_hash("/")));
    }

    #[test]
    fn should_truncate_the_slug_when_the_basename_exceeds_forty_chars() {
        // Given a 60-character basename of valid slug characters
        let long_name = "a".repeat(60);
        let path = format!("/work/{long_name}");
        let key = encode_work_dir_path_str(&path);
        // Then the slug is the first 40 characters
        assert_eq!(
            key,
            format!("wd_{}_{}", "a".repeat(40), expected_hash(&path))
        );
    }

    #[test]
    fn should_trim_a_trailing_dash_exposed_by_truncation() {
        // Given a basename whose 40th slug character is the dash the bad
        // run produced — truncation at 40 exposes it as a trailing dash
        let name = format!("{}{}", "b".repeat(39), " zzz");
        let path = format!("/work/{name}");
        let key = encode_work_dir_path_str(&path);
        // Then the exposed trailing dash is trimmed (39 b's, not 40)
        assert_eq!(
            key,
            format!("wd_{}_{}", "b".repeat(39), expected_hash(&path))
        );
    }

    #[test]
    fn should_lexically_resolve_dots_without_following_symlinks_when_normalizing() {
        // Given two spellings of the same lexical path (a `..` detour and
        // a doubled separator)
        let plain = encode_work_dir_path_str("/work/proj");
        let detour = encode_work_dir_path_str("/work/other/../proj");
        let doubled = encode_work_dir_path_str("/work//proj");
        // Then all three derive the SAME key — the hash input is the
        // lexically cleaned path — while a real symlink resolution would
        // differ (the reason canonicalize must NOT be used)
        assert_eq!(plain, detour);
        assert_eq!(plain, doubled);
    }

    #[test]
    fn should_keep_the_root_when_parent_dirs_overflow_the_root() {
        // Given a path that climbs above the root
        let key = encode_work_dir_path_str("/../../etc");
        // Then the normalization clamps at the root (TS resolve parity:
        // `/../..` → `/`), so the hash input is `/etc`
        assert_eq!(key, format!("wd_etc_{}", expected_hash("/etc")));
    }

    #[test]
    fn should_normalize_a_trailing_dot_component_when_deriving_the_key() {
        // Given a path with a trailing `.` component (and one climbing with
        // `..` to the root)
        let dotted = encode_work_dir_path_str("/work/.");
        let plain = encode_work_dir_path_str("/work");
        let climbed = encode_work_dir_path_str("/work/sub/..");
        // Then the dot component is cleaned away (TS resolve parity: the
        // basename of `/work/.` is `work`, not `.`), and the `..` detour
        // resolves back to `/work` — only the truly root path (`/`)
        // reaches the `workspace` slug fallback
        assert_eq!(dotted, plain);
        assert_eq!(climbed, plain);
        assert!(encode_work_dir_path_str("/work/..").starts_with("wd_workspace_"));
    }

    /// Test seam: [`encode_work_dir_key`] over a `&str` path (the tests
    /// spell paths as strings for readability).
    fn encode_work_dir_path_str(path: &str) -> String {
        encode_work_dir_key(Path::new(path))
    }
}
