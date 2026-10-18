//! Implementations of the non-`serve` `mirri` subcommands.
//!
//! Every handler here operates on the user's Mirri data directory
//! (`$HOME/.mirri-code/` by default, overridable via the `MIRRICODE_HOME`
//! environment variable), which holds:
//!
//! - `v3_sessions/` — the rust engine's session storage root (ADR-006-R2),
//! - `sessions/` — the TS-era session root (pre-migration sessions),
//! - `config.toml` — server configuration.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::Value;

/// Locate the Mirri data directory, honouring the `MIRRICODE_HOME`
/// environment variable.
///
/// When `MIRRICODE_HOME` is set to a non-empty value it is used verbatim;
/// otherwise the directory is `$HOME/.mirri-code`.
pub fn mirri_data_dir() -> Result<PathBuf> {
    resolve_mirri_data_dir_from(|name| std::env::var_os(name))
}

/// Pure resolver behind [`mirri_data_dir`], parameterised over the env lookup
/// so the precedence rules are testable without mutating the process
/// environment.
fn resolve_mirri_data_dir_from(
    get_env: impl Fn(&str) -> Option<std::ffi::OsString>,
) -> Result<PathBuf> {
    // MIRRICODE_HOME wins when set to a non-empty value; an empty value is
    // treated as unset so it falls through to the HOME-based default.
    if let Some(mirri_home) = get_env("MIRRICODE_HOME").filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(mirri_home));
    }
    let home = get_env("HOME")
        .context("HOME is not set; cannot locate the ~/.mirri-code data directory")?;
    Ok(PathBuf::from(home).join(".mirri-code"))
}

/// Relative path (under the Mirri data dir) of the rust engine's session
/// storage root (ADR-006-R2).
const V3_SESSIONS_DIR: &str = "v3_sessions";

/// Relative path (under the Mirri data dir) of the TS-era sessions root
/// (pre-migration sessions; the explicit migration path into the v3 root
/// is `mirri migrate`).
const SESSIONS_DIR: &str = "sessions";

/// Run `mirri session-list`: list the persisted sessions found under the
/// Mirri data dir's session roots — the v3 storage root first, then the
/// TS-era `sessions/` root — reading each session's `state.json` for its
/// creation timestamp. The session ID is the directory name (not a field
/// inside `state.json`).
///
/// The timestamp is read from the `created_at` field of the `Snapshot`
/// object. Sessions written by the legacy TypeScript engine, however,
/// serialise the same field as camelCase `createdAt`. Both spellings are
/// accepted so `session-list` works regardless of which engine produced the
/// session: `createdAt` is checked first (legacy), falling back to
/// `created_at` (current Rust `Snapshot`).
pub async fn run_session_list(_workspace: &str) -> Result<()> {
    let home = mirri_data_dir()?;
    let sessions = list_sessions_dual_root(&home.join(V3_SESSIONS_DIR), &home.join(SESSIONS_DIR))?;

    if sessions.is_empty() {
        println!("No sessions found.");
    } else {
        println!("{:<40} Created At", "Session ID");
        println!("{:-<40} {:-<30}", "-", "-");
        for (id, created) in sessions {
            println!("{:<40} {}", id, created);
        }
    }
    Ok(())
}

/// Collect `(session_id, created_at)` pairs from every `state.json` under
/// both session roots (v3 first, then the TS-era root), deduplicated by
/// session id — a session present in both roots is reported once, from the
/// v3 root (the post-migration location) — and sorted deterministically by
/// `(created_at, session_id)`.
///
/// A missing root simply contributes nothing; only when NEITHER root exists
/// is the absence reported.
fn list_sessions_dual_root(
    v3_sessions_dir: &Path,
    legacy_sessions_dir: &Path,
) -> Result<Vec<(String, String)>> {
    if !v3_sessions_dir.exists() && !legacy_sessions_dir.exists() {
        println!(
            "No sessions directory found at {} (or {})",
            v3_sessions_dir.display(),
            legacy_sessions_dir.display()
        );
        return Ok(vec![]);
    }

    let mut sessions = list_sessions(v3_sessions_dir)?;
    for (id, created) in list_sessions(legacy_sessions_dir)? {
        // A session id already seen from the v3 root wins — the legacy copy
        // is the pre-migration duplicate.
        if !sessions.iter().any(|(existing, _)| existing == &id) {
            sessions.push((id, created));
        }
    }

    // Deterministic output: order by creation timestamp, then by id.
    sessions
        .sort_by(|(id_a, created_a), (id_b, created_b)| (created_a, id_a).cmp(&(created_b, id_b)));

    Ok(sessions)
}

/// Collect `(session_id, created_at)` pairs from every `state.json` under
/// `sessions_dir`, sorted deterministically by `(created_at, session_id)`.
///
/// Supports both the Rust flat layout (`<sessions_dir>/<session_id>/`) and
/// the TS bucketed layout (`<sessions_dir>/<bucket>/<session_id>/`, where
/// `<bucket>` is a workDir-keyed directory loosely matching the `wd_*_*`
/// naming convention), mirroring the `migrate` CLI's structural detection:
/// a first-level entry containing `state.json` is a session; one that does
/// not is treated as a potential bucket and its subdirectories are scanned.
///
/// Accepts both `createdAt` (camelCase, legacy TS engine) and `created_at`
/// (snake_case, current Rust `Snapshot`) spellings of the timestamp field.
/// A missing directory yields an empty vector (the dual-root caller
/// reports the combined absence); a session whose `state.json` cannot be
/// parsed or lacks a timestamp falls back to `"unknown"`, and a session
/// whose `state.json` cannot be read is skipped with a warning.
fn list_sessions(sessions_dir: &Path) -> Result<Vec<(String, String)>> {
    if !sessions_dir.exists() {
        return Ok(vec![]);
    }

    let mut sessions: Vec<(String, String)> = vec![]; // (id, created_at)

    for entry in std::fs::read_dir(sessions_dir)? {
        let entry = entry?;
        let entry_path = entry.path();

        // --- Flat layout: <sessions_dir>/<session_id>/state.json ---
        let state_file = entry_path.join("state.json");
        if state_file.exists() {
            // The session ID is the directory name.
            let id = entry.file_name().to_str().unwrap_or("unknown").to_string();
            if let Some(created) = session_created_at(&state_file) {
                sessions.push((id, created));
            }
            continue;
        }

        // --- TS bucketed layout: <sessions_dir>/<bucket>/<session_id>/state.json ---
        //
        // The TS kernel organises sessions under a workDir-keyed bucket
        // directory (loosely matching the `wd_*_*` naming convention).
        // When a first-level entry has no state.json, treat it as a
        // potential bucket and scan its subdirectories for sessions.
        if !entry.file_type()?.is_dir() {
            continue;
        }

        for bucket_entry in std::fs::read_dir(&entry_path)? {
            let bucket_entry = bucket_entry?;
            let bucket_state_file = bucket_entry.path().join("state.json");
            if !bucket_state_file.exists() {
                continue;
            }
            // The session ID is the bucket-entry directory name.
            let id = bucket_entry
                .file_name()
                .to_str()
                .unwrap_or("unknown")
                .to_string();
            if let Some(created) = session_created_at(&bucket_state_file) {
                sessions.push((id, created));
            }
        }
    }

    // Deterministic output: order by creation timestamp, then by id.
    sessions
        .sort_by(|(id_a, created_a), (id_b, created_b)| (created_a, id_a).cmp(&(created_b, id_b)));

    Ok(sessions)
}

/// Read a session's creation timestamp from its `state.json`. Accepts both
/// the camelCase `createdAt` (legacy TS engine) and snake_case `created_at`
/// (current Rust `Snapshot`) spellings.
///
/// Returns `None` when the file cannot be read (callers skip the session,
/// matching the persistence layer's handling of unreadable state files); an
/// unparseable file or a missing timestamp yields `Some("unknown")`.
fn session_created_at(state_file: &Path) -> Option<String> {
    let content = match std::fs::read_to_string(state_file) {
        Ok(content) => content,
        Err(e) => {
            eprintln!("Failed to read {}: {}", state_file.display(), e);
            return None;
        }
    };
    let created = if let Ok(state) = serde_json::from_str::<Value>(&content) {
        state
            .get("createdAt")
            .or_else(|| state.get("created_at"))
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string()
    } else {
        "unknown".to_string()
    };
    Some(created)
}

/// Run `mirri config-show`: print the user's `config.toml` verbatim, or
/// report that the default configuration is in effect when no file exists.
pub async fn run_config_show(_workspace: &str) -> Result<()> {
    let config_path = mirri_data_dir()?.join("config.toml");

    if !config_path.exists() {
        println!("No config file found at {}", config_path.display());
        println!("Using default configuration.");
        return Ok(());
    }

    let content = std::fs::read_to_string(&config_path)?;
    println!("{}", content);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_os_env<'a>(
        entries: &'a [(&'a str, &'a str)],
    ) -> impl Fn(&str) -> Option<std::ffi::OsString> + 'a {
        move |name| {
            entries
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| std::ffi::OsString::from(*value))
        }
    }

    #[test]
    fn should_use_mirricode_home_when_mirricode_home_env_is_set() {
        // Given MIRRICODE_HOME is set to a custom directory and HOME is also
        // set
        let env = fake_os_env(&[("MIRRICODE_HOME", "/custom/mirri"), ("HOME", "/home/user")]);

        // When resolving the mirri data dir
        let dir = resolve_mirri_data_dir_from(env).expect("resolution should succeed");

        // Then MIRRICODE_HOME wins over the HOME-based default
        assert_eq!(dir, PathBuf::from("/custom/mirri"));
    }

    #[test]
    fn should_fall_back_to_home_when_mirricode_home_env_is_not_set() {
        // Given MIRRICODE_HOME is absent and HOME is set
        let env = fake_os_env(&[("HOME", "/home/user")]);

        // When resolving the mirri data dir
        let dir = resolve_mirri_data_dir_from(env).expect("resolution should succeed");

        // Then the data dir is $HOME/.mirri-code
        assert_eq!(dir, PathBuf::from("/home/user/.mirri-code"));
    }

    #[test]
    fn should_fall_back_to_home_when_mirricode_home_env_is_empty() {
        // Given MIRRICODE_HOME is set but empty (treated as unset)
        let env = fake_os_env(&[("MIRRICODE_HOME", ""), ("HOME", "/home/user")]);

        // When resolving the mirri data dir
        let dir = resolve_mirri_data_dir_from(env).expect("resolution should succeed");

        // Then the data dir is $HOME/.mirri-code
        assert_eq!(dir, PathBuf::from("/home/user/.mirri-code"));
    }

    #[test]
    fn should_read_created_at_when_state_json_uses_snake_case() {
        // Given a sessions directory with a state.json using the snake_case
        // `created_at` field (the current Rust Snapshot format)
        let temp_dir = tempfile::tempdir().expect("tempdir should succeed");
        let sessions_dir = temp_dir.path().join("sessions");
        let session_dir = sessions_dir.join("session-rust");
        std::fs::create_dir_all(&session_dir).expect("create session dir should succeed");
        std::fs::write(
            session_dir.join("state.json"),
            r#"{"version":1,"op_index":5,"state":{},"created_at":"2025-01-15T10:00:00Z"}"#,
        )
        .expect("write state.json should succeed");

        // When listing sessions
        let sessions = list_sessions(&sessions_dir).expect("list_sessions should succeed");

        // Then the snake_case timestamp is read correctly
        assert_eq!(
            sessions,
            vec![(
                "session-rust".to_string(),
                "2025-01-15T10:00:00Z".to_string()
            )]
        );
    }

    #[test]
    fn should_read_created_at_when_state_json_uses_camel_case() {
        // Given a sessions directory with a state.json using the camelCase
        // `createdAt` field (the legacy TypeScript engine format)
        let temp_dir = tempfile::tempdir().expect("tempdir should succeed");
        let sessions_dir = temp_dir.path().join("sessions");
        let session_dir = sessions_dir.join("session-legacy");
        std::fs::create_dir_all(&session_dir).expect("create session dir should succeed");
        std::fs::write(
            session_dir.join("state.json"),
            r#"{"version":1,"opIndex":5,"state":{},"createdAt":"2024-12-20T08:30:00Z"}"#,
        )
        .expect("write state.json should succeed");

        // When listing sessions
        let sessions = list_sessions(&sessions_dir).expect("list_sessions should succeed");

        // Then the camelCase timestamp is read correctly (not "unknown")
        assert_eq!(
            sessions,
            vec![(
                "session-legacy".to_string(),
                "2024-12-20T08:30:00Z".to_string()
            )]
        );
    }

    #[test]
    fn should_sort_sessions_by_timestamp_then_id_when_listing() {
        // Given sessions with different timestamps and a mix of camelCase and
        // snake_case field spellings
        let temp_dir = tempfile::tempdir().expect("tempdir should succeed");
        let sessions_dir = temp_dir.path().join("sessions");

        let make_session = |id: &str, json: &str| {
            let dir = sessions_dir.join(id);
            std::fs::create_dir_all(&dir).expect("create session dir should succeed");
            std::fs::write(dir.join("state.json"), json).expect("write state.json should succeed");
        };
        make_session("session-b", r#"{"created_at":"2025-01-02T00:00:00Z"}"#);
        make_session("session-a", r#"{"createdAt":"2025-01-01T00:00:00Z"}"#);
        make_session("session-c", r#"{"created_at":"2025-01-02T00:00:00Z"}"#);

        // When listing sessions
        let sessions = list_sessions(&sessions_dir).expect("list_sessions should succeed");

        // Then they are sorted by (timestamp, id): session-a first, then the
        // two with the same timestamp are ordered alphabetically by id
        assert_eq!(
            sessions,
            vec![
                ("session-a".to_string(), "2025-01-01T00:00:00Z".to_string()),
                ("session-b".to_string(), "2025-01-02T00:00:00Z".to_string()),
                ("session-c".to_string(), "2025-01-02T00:00:00Z".to_string()),
            ]
        );
    }

    #[test]
    fn should_return_unknown_when_timestamp_field_is_missing() {
        // Given a sessions directory with a state.json that has neither
        // `created_at` nor `createdAt`
        let temp_dir = tempfile::tempdir().expect("tempdir should succeed");
        let sessions_dir = temp_dir.path().join("sessions");
        let session_dir = sessions_dir.join("session-no-ts");
        std::fs::create_dir_all(&session_dir).expect("create session dir should succeed");
        std::fs::write(
            session_dir.join("state.json"),
            r#"{"version":1,"op_index":0,"state":{}}"#,
        )
        .expect("write state.json should succeed");

        // When listing sessions
        let sessions = list_sessions(&sessions_dir).expect("list_sessions should succeed");

        // Then the timestamp falls back to "unknown"
        assert_eq!(
            sessions,
            vec![("session-no-ts".to_string(), "unknown".to_string())]
        );
    }

    #[test]
    fn should_return_empty_when_sessions_directory_does_not_exist() {
        // Given a sessions directory path that does not exist on disk
        let temp_dir = tempfile::tempdir().expect("tempdir should succeed");
        let sessions_dir = temp_dir.path().join("nonexistent-sessions");

        // When listing sessions
        let sessions = list_sessions(&sessions_dir).expect("list_sessions should succeed");

        // Then the result is an empty vector (not an error)
        assert!(sessions.is_empty());
    }

    #[test]
    fn should_list_sessions_in_ts_bucketed_layout() {
        // Given a sessions directory with the TS bucketed layout holding a
        // state.json, plus a flat-layout session alongside it
        let temp_dir = tempfile::tempdir().expect("tempdir should succeed");
        let sessions_dir = temp_dir.path().join("sessions");

        // TS bucketed session — bucket dir loosely matches wd_*_*.
        let bucket_dir = sessions_dir.join("wd_abc123_def456");
        let ts_session_dir = bucket_dir.join("ts-session-1");
        std::fs::create_dir_all(&ts_session_dir).expect("create ts session dir should succeed");
        std::fs::write(
            ts_session_dir.join("state.json"),
            r#"{"version":1,"opIndex":5,"state":{},"createdAt":"2024-12-20T08:30:00Z"}"#,
        )
        .expect("write ts state.json should succeed");

        // Flat-layout session alongside the bucket.
        let flat_session_dir = sessions_dir.join("flat-session");
        std::fs::create_dir_all(&flat_session_dir).expect("create flat session dir should succeed");
        std::fs::write(
            flat_session_dir.join("state.json"),
            r#"{"version":1,"op_index":0,"state":{},"created_at":"2025-01-15T10:00:00Z"}"#,
        )
        .expect("write flat state.json should succeed");

        // When listing sessions
        let sessions = list_sessions(&sessions_dir).expect("list_sessions should succeed");

        // Then both sessions are found (bucketed and flat), sorted by
        // (timestamp, id)
        assert_eq!(
            sessions,
            vec![
                (
                    "ts-session-1".to_string(),
                    "2024-12-20T08:30:00Z".to_string()
                ),
                (
                    "flat-session".to_string(),
                    "2025-01-15T10:00:00Z".to_string()
                ),
            ]
        );
    }

    #[test]
    fn should_list_sessions_from_both_storage_roots() {
        // Given a Mirri home with a rust-written session under
        // v3_sessions/<bucket>/<id> and a TS-era session under
        // sessions/<bucket>/<id>
        let temp_dir = tempfile::tempdir().expect("tempdir should succeed");
        let v3_dir = temp_dir.path().join("v3_sessions");
        let v3_session_dir = v3_dir.join("wd_v3_bucket").join("session-rust");
        std::fs::create_dir_all(&v3_session_dir).expect("create v3 session dir should succeed");
        std::fs::write(
            v3_session_dir.join("state.json"),
            r#"{"created_at":"2025-06-01T10:00:00Z"}"#,
        )
        .expect("write v3 state.json should succeed");

        let legacy_dir = temp_dir.path().join("sessions");
        let ts_session_dir = legacy_dir.join("wd_ts_bucket").join("session-ts");
        std::fs::create_dir_all(&ts_session_dir).expect("create ts session dir should succeed");
        std::fs::write(
            ts_session_dir.join("state.json"),
            r#"{"createdAt":"2024-12-20T08:30:00Z"}"#,
        )
        .expect("write ts state.json should succeed");

        // When listing sessions across both roots
        let sessions = list_sessions_dual_root(&v3_dir, &legacy_dir)
            .expect("list_sessions_dual_root should succeed");

        // Then both eras' sessions are found, sorted by (timestamp, id)
        assert_eq!(
            sessions,
            vec![
                ("session-ts".to_string(), "2024-12-20T08:30:00Z".to_string()),
                (
                    "session-rust".to_string(),
                    "2025-06-01T10:00:00Z".to_string()
                ),
            ]
        );
    }

    #[test]
    fn should_prefer_the_v3_entry_when_a_session_exists_in_both_roots() {
        // Given a session id present in both the v3 root and the legacy root
        // (the pre-migration duplicate left behind by --copy)
        let temp_dir = tempfile::tempdir().expect("tempdir should succeed");
        let v3_dir = temp_dir.path().join("v3_sessions");
        let v3_session_dir = v3_dir.join("wd_v3_bucket").join("session-dup");
        std::fs::create_dir_all(&v3_session_dir).expect("create v3 session dir should succeed");
        std::fs::write(
            v3_session_dir.join("state.json"),
            r#"{"created_at":"2025-06-01T10:00:00Z"}"#,
        )
        .expect("write v3 state.json should succeed");

        let legacy_dir = temp_dir.path().join("sessions");
        let legacy_session_dir = legacy_dir.join("wd_ts_bucket").join("session-dup");
        std::fs::create_dir_all(&legacy_session_dir)
            .expect("create legacy session dir should succeed");
        std::fs::write(
            legacy_session_dir.join("state.json"),
            r#"{"createdAt":"2024-12-20T08:30:00Z"}"#,
        )
        .expect("write legacy state.json should succeed");

        // When listing sessions across both roots
        let sessions = list_sessions_dual_root(&v3_dir, &legacy_dir)
            .expect("list_sessions_dual_root should succeed");

        // Then the id appears once, with the v3 root's entry winning
        assert_eq!(
            sessions,
            vec![(
                "session-dup".to_string(),
                "2025-06-01T10:00:00Z".to_string()
            )]
        );
    }

    #[test]
    fn should_report_nothing_when_neither_storage_root_exists() {
        // Given a Mirri home with neither a v3_sessions nor a sessions root
        let temp_dir = tempfile::tempdir().expect("tempdir should succeed");
        let v3_dir = temp_dir.path().join("v3_sessions");
        let legacy_dir = temp_dir.path().join("sessions");

        // When listing sessions across both roots
        let sessions = list_sessions_dual_root(&v3_dir, &legacy_dir)
            .expect("list_sessions_dual_root should succeed");

        // Then the result is an empty vector (not an error)
        assert!(sessions.is_empty());
    }
}
