//! Implementations of the non-`serve` `mirri` subcommands.
//!
//! Every handler here operates on the user's Mirri data directory
//! (`$HOME/.mirri-code/` by default, overridable via the `MIRRICODE_HOME`
//! environment variable), which holds:
//!
//! - `v3_sessions/` — the rust engine's session storage root (ADR-006-R2),
//! - `sessions/` — the TS-era session root (pre-migration sessions),
//! - `config.toml` — server configuration.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use mirri_credentials::migration::{dry_run_config_api_keys, migrate_config_api_keys};
use mirri_credentials::{CredentialStore, CredentialValue};
use serde_json::Value;

use crate::CredentialCommands;

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

// --- `mirri credential` subcommands ---

/// Run `mirri credential <subcommand>`: manage the provider credential
/// store under the Mirri home (the same home every other command resolves
/// through [`mirri_core::config::mirri_home_dir`] — the single source of
/// truth for the credential store's location, SDR-002).
pub async fn run_credential(command: CredentialCommands) -> Result<()> {
    use std::io::IsTerminal;

    let home = mirri_core::config::mirri_home_dir()?;
    let store = CredentialStore::open(&home)?;
    let is_tty = std::io::stdin().is_terminal();
    match command {
        CredentialCommands::List => {
            let mut out = std::io::stdout().lock();
            credential_list(&store, &mut out)?;
        }
        CredentialCommands::Set { provider_id } => {
            let secret = if is_tty {
                rpassword::prompt_password("API key: ")?
            } else {
                let mut line = String::new();
                std::io::stdin().read_line(&mut line)?;
                line.trim().to_string()
            };
            credential_set(&store, &provider_id, &secret)?;
            println!("credential stored for provider '{provider_id}'");
        }
        CredentialCommands::Remove { provider_id } => {
            let mut out = std::io::stdout().lock();
            credential_remove(&store, &provider_id, &mut out)?;
            println!("credential removed for provider '{provider_id}'");
        }
        CredentialCommands::Show { provider_id } => {
            let is_stdout_tty = std::io::stdout().is_terminal();
            let mut out = std::io::stdout().lock();
            let mut err = std::io::stderr().lock();
            credential_show(&store, &provider_id, is_stdout_tty, &mut out, &mut err)?;
        }
        CredentialCommands::Migrate { dry_run } => {
            let config_path = home.join("config.toml");
            if dry_run {
                let report = dry_run_config_api_keys(&config_path)?;
                let mut out = std::io::stdout().lock();
                print_migration_dry_run(&report, &mut out)?;
            } else {
                let report = migrate_config_api_keys(&store, &config_path)?;
                let mut out = std::io::stdout().lock();
                print_migration_report(&report, &mut out)?;
            }
        }
    }
    Ok(())
}

/// The `credential list` body: a `PROVIDER  TYPE  STATUS  CREATED` table
/// (plus `AZ PRESENT` for external rows) over the store, never including
/// key material (B.1). STATUS is always `unknown` here — the standalone
/// CLI has no engine runtime to consult, so the honest unknown is the
/// only defensible verdict. Output goes to `out` so tests can capture it
/// without a process.
fn credential_list(store: &CredentialStore, out: &mut impl Write) -> Result<()> {
    let mut records = store.read_all();
    records.sort_by(|a, b| a.provider_id.cmp(&b.provider_id));
    if records.is_empty() {
        writeln!(out, "No stored credentials.")?;
        return Ok(());
    }
    writeln!(
        out,
        "{:<24} {:<10} {:<8} CREATED",
        "PROVIDER", "TYPE", "STATUS"
    )?;
    for record in records {
        match record.value {
            CredentialValue::Key { .. } => writeln!(
                out,
                "{:<24} {:<10} {:<8} {}",
                record.provider_id,
                "key",
                "unknown",
                format_ms(record.time_created)
            )?,
            CredentialValue::External { method_id, .. } => {
                // The AZ-presence flag is a separate fact from STATUS — only
                // external rows carry it, answering a real PATH probe
                // (yes/no), never a guess.
                let az_present = if mirri_credentials::external_available(&method_id) {
                    "yes"
                } else {
                    "no"
                };
                writeln!(
                    out,
                    "{:<24} {:<10} {:<8} {}  AZ PRESENT: {}",
                    record.provider_id,
                    "external",
                    "unknown",
                    format_ms(record.time_created),
                    az_present
                )?;
            }
            CredentialValue::Oauth { .. } => writeln!(
                out,
                "{:<24} {:<10} {:<8} {}",
                record.provider_id,
                "oauth",
                "unknown",
                format_ms(record.time_created)
            )?,
        }
    }
    Ok(())
}

/// The `credential migrate` summary: counts first, then one guidance line
/// per skipped provider (E.3 per-entry alarm — the id plus the
/// manual-handling wording shared with the server-side migration log).
/// Output goes to `out` so tests can capture it without a process.
fn print_migration_report(
    report: &mirri_credentials::MigrationReport,
    out: &mut impl Write,
) -> Result<()> {
    writeln!(
        out,
        "migrated: {}, cleaned: {}, skipped: {}",
        report.migrated.len(),
        report.cleaned.len(),
        report.skipped.len()
    )?;
    for id in &report.skipped {
        writeln!(
            out,
            "skipped provider '{id}': {}",
            mirri_credentials::SKIPPED_GUIDANCE
        )?;
    }
    Ok(())
}

/// The `credential migrate --dry-run` summary (E.5): the would-migrate
/// provider ids only — never key material — with the same per-skipped
/// guidance as the real run.
fn print_migration_dry_run(
    report: &mirri_credentials::MigrationReport,
    out: &mut impl Write,
) -> Result<()> {
    if report.migrated.is_empty() && report.skipped.is_empty() {
        writeln!(out, "nothing to migrate")?;
    } else {
        writeln!(out, "would migrate: {}", report.migrated.join(", "))?;
    }
    for id in &report.skipped {
        writeln!(
            out,
            "would skip provider '{id}': {}",
            mirri_credentials::SKIPPED_GUIDANCE
        )?;
    }
    Ok(())
}

/// The `credential set` body: reject a blank secret (an empty trimmed
/// stdin line is an accident, not a credential) and store the trimmed value.
fn credential_set(store: &CredentialStore, provider_id: &str, secret: &str) -> Result<()> {
    let key = secret.trim();
    if key.is_empty() {
        bail!("refusing to store an empty credential for provider '{provider_id}'");
    }
    store.set(provider_id, CredentialValue::Key { key: key.into() })?;
    Ok(())
}

/// List the provider ids currently stored — the message carried by the
/// unknown-provider errors (B.7): naming what IS stored next to what was
/// asked for.
fn stored_provider_ids(store: &CredentialStore) -> Vec<String> {
    store
        .read_all()
        .into_iter()
        .map(|record| record.provider_id)
        .collect()
}

/// The `credential remove` body: remove a stored provider or name the
/// unknown id together with what IS stored (B.7).
fn credential_remove(
    store: &CredentialStore,
    provider_id: &str,
    out: &mut impl Write,
) -> Result<()> {
    if !store.remove(provider_id)? {
        let stored = stored_provider_ids(store);
        bail!(
            "unknown provider '{provider_id}'; stored providers: {}",
            if stored.is_empty() {
                "(none)".to_string()
            } else {
                stored.join(", ")
            }
        );
    }
    writeln!(out, "removed credential for provider '{provider_id}'")?;
    Ok(())
}

/// The `credential show` body: reveal a provider's credential in plaintext
/// (the CLI-only outlet, B.3). On a terminal the warning goes to stderr
/// first so the secret stays greppable on stdout; an external reference is
/// explained instead of shown (it holds no key material).
fn credential_show(
    store: &CredentialStore,
    provider_id: &str,
    is_tty: bool,
    out: &mut impl Write,
    err: &mut impl Write,
) -> Result<()> {
    let Some(record) = store.get(provider_id) else {
        let stored = stored_provider_ids(store);
        bail!(
            "unknown provider '{provider_id}'; stored providers: {}",
            if stored.is_empty() {
                "(none)".to_string()
            } else {
                stored.join(", ")
            }
        );
    };
    match record.value {
        CredentialValue::Key { key } => {
            if is_tty {
                writeln!(err, "Warning: the next line contains a secret in plaintext")?;
            }
            writeln!(out, "{key}")?;
        }
        CredentialValue::External { method_id, .. } => {
            writeln!(
                out,
                "external:{method_id} — this entry holds no key material; it is resolved from the \
                 source tool at call time"
            )?;
        }
        CredentialValue::Oauth { .. } => {
            bail!(
                "provider '{provider_id}' holds an oauth credential; oauth is not implemented yet"
            );
        }
    }
    Ok(())
}

/// Render epoch milliseconds as `YYYY-MM-DD HH:MM` in UTC — a small
/// hand-rolled calendar (civil-from-days) so the CLI pulls no new
/// time-formatting dependency for one table column.
fn format_ms(epoch_ms: u64) -> String {
    const MINUTE: u64 = 60;
    const HOUR: u64 = 60 * MINUTE;
    const DAY: u64 = 24 * HOUR;

    let days_since_epoch = epoch_ms / DAY / 1000;
    let seconds_of_day = epoch_ms / 1000 % DAY;
    let (year, month, day) = civil_from_days(days_since_epoch);
    let hour = seconds_of_day / HOUR;
    let minute = seconds_of_day % HOUR / MINUTE;
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}")
}

/// Convert days since 1970-01-01 into a `(year, month, day)` civil date
/// (Howard Hinnant's `civil_from_days`, valid for the whole u64 range).
fn civil_from_days(days: u64) -> (i64, u32, u32) {
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097); // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
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

    // --- credential subcommands ---

    /// A store under a temporary Mirri home holding an `openai` key
    /// credential (`sk-secret123`) and an `azure` external reference —
    /// the fixture every credential behavior test builds on.
    fn credential_store_with_entries() -> (tempfile::TempDir, mirri_credentials::CredentialStore) {
        let home = tempfile::tempdir().expect("home tempdir should succeed");
        let store =
            mirri_credentials::CredentialStore::open(home.path()).expect("store should open");
        store
            .set(
                "openai",
                mirri_credentials::CredentialValue::Key {
                    key: "sk-secret123".into(),
                },
            )
            .expect("set openai credential should succeed");
        store
            .set(
                "azure",
                mirri_credentials::CredentialValue::External {
                    method_id: "azure-cli".into(),
                    metadata: Default::default(),
                },
            )
            .expect("set azure credential should succeed");
        (home, store)
    }

    #[test]
    fn should_list_credentials_without_plaintext_when_listing_the_store() {
        // Given a store holding an openai key credential and an azure external
        // reference
        let (_home, store) = credential_store_with_entries();

        // When listing the credentials
        let mut out = Vec::new();
        credential_list(&store, &mut out).expect("list should succeed");
        let text = String::from_utf8(out).expect("output should be utf-8");

        // Then the table names the providers and their types but never the
        // key material
        assert!(text.contains("openai"), "list should name openai: {text}");
        assert!(text.contains("azure"), "list should name azure: {text}");
        assert!(
            text.contains("key"),
            "list should show the key type: {text}"
        );
        assert!(
            !text.contains("sk-secret123"),
            "list must not leak the plaintext key: {text}"
        );
    }

    #[test]
    fn should_print_each_skipped_provider_with_manual_handling_guidance_when_migrating() {
        // Given a migration report carrying two skipped providers alongside
        // a migrated one (the shape `migrate_config_api_keys` returns for
        // malformed entries, E.3)
        let report = mirri_credentials::MigrationReport {
            migrated: vec!["openai".to_string()],
            cleaned: vec!["openai".to_string()],
            skipped: vec![String::new(), " ".to_string()],
        };

        // When printing the migration summary
        let mut out = Vec::new();
        print_migration_report(&report, &mut out).expect("print should succeed");
        let text = String::from_utf8(out).expect("output should be utf-8");

        // Then the counts line survives and every skipped id gets its own
        // guidance line telling the user to handle it manually
        assert!(
            text.contains("migrated: 1, cleaned: 1, skipped: 2"),
            "the counts line survives: {text}"
        );
        assert!(
            text.contains("manually"),
            "the guidance tells the user to handle it manually: {text}"
        );
        assert!(
            text.matches("skipped provider '").count() == 2,
            "each skipped provider gets its own line: {text}"
        );

        // And the dry-run variant carries the same per-entry guidance
        let mut dry = Vec::new();
        print_migration_dry_run(&report, &mut dry).expect("print should succeed");
        let dry_text = String::from_utf8(dry).expect("output should be utf-8");
        assert!(
            dry_text.contains("would migrate: openai"),
            "the dry run lists the would-migrate ids: {dry_text}"
        );
        assert!(
            dry_text.matches("would skip provider '").count() == 2,
            "each skipped provider gets its own dry-run line: {dry_text}"
        );
        assert!(
            dry_text.contains("manually"),
            "the dry-run guidance carries the manual-handling wording: {dry_text}"
        );
    }

    #[test]
    fn should_render_status_unknown_and_an_external_availability_flag_when_listing() {
        // Given a store holding an openai key credential and an azure
        // external reference
        let (_home, store) = credential_store_with_entries();

        // When listing the credentials
        let mut out = Vec::new();
        credential_list(&store, &mut out).expect("list should succeed");
        let text = String::from_utf8(out).expect("output should be utf-8");

        // Then the header carries the STATUS column and every row answers
        // `unknown` — the standalone CLI has no engine runtime to ask, so an
        // honest unknown beats a fabricated verdict
        let header = text.lines().next().unwrap_or_default();
        assert!(
            header.contains("STATUS"),
            "the table header carries the STATUS column: {text}"
        );
        let rows: Vec<&str> = text.lines().skip(1).collect();
        assert_eq!(rows.len(), 2, "one row per stored credential: {text}");
        for row in &rows {
            assert!(
                row.contains("unknown"),
                "every row answers unknown (no runtime evidence here): {text}"
            );
        }

        // And the two facts stay separate: the AZ PRESENT column exists only
        // for external rows — a key row reports n/a, never a fabricated probe
        let key_row = rows.iter().find(|r| r.contains("openai")).unwrap();
        assert!(
            !key_row.contains("AZ PRESENT"),
            "a key credential carries no external availability flag: {key_row}"
        );
        let external_row = rows.iter().find(|r| r.contains("azure")).unwrap();
        assert!(
            external_row.contains("AZ PRESENT"),
            "an external credential row carries the AZ PRESENT column: {external_row}"
        );
        assert!(
            external_row.contains("yes") || external_row.contains("no"),
            "the AZ PRESENT cell answers yes or no (a real probe, not a blank): {external_row}"
        );
    }

    #[test]
    fn should_render_the_creation_time_when_listing_credentials() {
        // Given a store holding one key credential whose time_updated was
        // refreshed after its creation (an overwrite in place, A.3) — the
        // drift is injected by rewriting the store file, since every store
        // API stamps "now" itself
        let home = tempfile::tempdir().expect("home tempdir should succeed");
        let store =
            mirri_credentials::CredentialStore::open(home.path()).expect("store should open");
        store
            .set(
                "openai",
                mirri_credentials::CredentialValue::Key {
                    key: "sk-one".into(),
                },
            )
            .expect("first set should succeed");
        let created = store.get("openai").expect("entry exists").time_created;
        // A set far in the future of the creation stamp: enough for the
        // minute-granular rendering to differ when time_updated is (wrongly)
        // the rendered column.
        let drifted = created + 10 * 60 * 1000;
        let file_path = home.path().join("credentials").join("credentials.json");
        let mut file: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&file_path).expect("store file reads"))
                .expect("store file parses");
        file["entries"][0]["time_updated"] = serde_json::json!(drifted);
        std::fs::write(
            &file_path,
            serde_json::to_string(&file).expect("serializes"),
        )
        .expect("store file writes");

        // When listing the credentials
        let mut out = Vec::new();
        credential_list(&store, &mut out).expect("list should succeed");
        let text = String::from_utf8(out).expect("output should be utf-8");

        // Then the CREATED column renders the creation time (B.1 — the same
        // fact the RPC face reports), never the updated stamp
        let expected = format_ms(created);
        assert!(
            text.contains(&expected),
            "the CREATED column renders the creation time: {text}"
        );
        let wrong = format_ms(drifted);
        assert_ne!(
            expected, wrong,
            "fixture sanity: the drifted stamp must render differently"
        );
        assert!(
            !text.contains(&wrong),
            "the CREATED column must not render the updated time: {text}"
        );
    }

    #[test]
    fn should_show_plaintext_with_tty_warning_when_revealing_on_a_tty() {
        // Given a store holding an openai key credential
        let (_home, store) = credential_store_with_entries();

        // When revealing it with the terminal flag set
        let mut out = Vec::new();
        let mut err = Vec::new();
        credential_show(&store, "openai", true, &mut out, &mut err).expect("show should succeed");
        let stdout = String::from_utf8(out).expect("stdout should be utf-8");
        let stderr = String::from_utf8(err).expect("stderr should be utf-8");

        // Then the secret is printed and preceded by a plaintext warning on
        // stderr
        assert!(
            stdout.contains("sk-secret123"),
            "show should print the plaintext key: {stdout}"
        );
        assert!(
            stderr.to_lowercase().contains("warning"),
            "show on a tty should warn on stderr first: {stderr}"
        );
    }

    #[test]
    fn should_show_without_tty_warning_when_revealing_off_a_tty() {
        // Given a store holding an openai key credential
        let (_home, store) = credential_store_with_entries();

        // When revealing it without the terminal flag (piped output)
        let mut out = Vec::new();
        let mut err = Vec::new();
        credential_show(&store, "openai", false, &mut out, &mut err).expect("show should succeed");

        // Then the secret still prints, with no interactive warning
        assert!(
            String::from_utf8(out).unwrap().contains("sk-secret123"),
            "piped show should still print the key"
        );
        assert!(
            String::from_utf8(err).unwrap().is_empty(),
            "piped show should not print a tty warning"
        );
    }

    #[test]
    fn should_explain_external_references_when_showing_an_external_credential() {
        // Given a store whose azure entry is an external reference
        let (_home, store) = credential_store_with_entries();

        // When revealing it
        let mut out = Vec::new();
        let mut err = Vec::new();
        credential_show(&store, "azure", false, &mut out, &mut err).expect("show should succeed");
        let text = String::from_utf8(out).expect("output should be utf-8");

        // Then the output explains the reference instead of a secret
        assert!(
            text.contains("azure-cli"),
            "show should name the external method: {text}"
        );
        assert!(
            text.to_lowercase().contains("external"),
            "show should explain this is an external reference: {text}"
        );
    }

    #[test]
    fn should_remove_and_report_unknown_provider_when_removing_credentials() {
        // Given a store holding an openai key credential
        let (_home, store) = credential_store_with_entries();

        // When removing a stored provider
        let mut out = Vec::new();
        credential_remove(&store, "openai", &mut out)
            .expect("remove of a stored provider should succeed");

        // Then the provider no longer appears in the listing
        let mut listed = Vec::new();
        credential_list(&store, &mut listed).expect("list should succeed");
        let text = String::from_utf8(listed).expect("listing should be utf-8");
        assert!(
            !text.contains("openai"),
            "removed provider should be gone: {text}"
        );

        // And when removing a provider the store never held
        let mut out = Vec::new();
        let error = credential_remove(&store, "no-such-provider", &mut out)
            .expect_err("removing an unknown provider should fail");

        // Then the error names the unknown provider and lists what IS stored
        let message = format!("{error:#}");
        assert!(
            message.contains("no-such-provider"),
            "error should name the unknown provider: {message}"
        );
        assert!(
            message.contains("azure"),
            "error should list the stored providers: {message}"
        );
    }

    #[test]
    fn should_reject_an_empty_secret_when_setting_a_credential() {
        // Given a store under a temporary home
        let home = tempfile::tempdir().expect("home tempdir should succeed");
        let store =
            mirri_credentials::CredentialStore::open(home.path()).expect("store should open");

        // When setting a credential from an empty trimmed secret (blank stdin)
        let error =
            credential_set(&store, "openai", "  ").expect_err("an empty secret must be rejected");

        // Then the store was not touched and the error says why
        assert!(
            format!("{error:#}").to_lowercase().contains("empty"),
            "error should explain the empty secret: {error:#}"
        );
        assert!(
            store.get("openai").is_none(),
            "no entry should be created for an empty secret"
        );
    }

    #[test]
    fn should_store_a_trimmed_secret_when_setting_a_credential() {
        // Given a store under a temporary home
        let home = tempfile::tempdir().expect("home tempdir should succeed");
        let store =
            mirri_credentials::CredentialStore::open(home.path()).expect("store should open");

        // When setting a credential from a padded stdin line
        credential_set(&store, "openai", "  sk-secret123  \n").expect("set should succeed");

        // Then the stored secret is the trimmed value
        assert_eq!(store.reveal("openai").as_deref(), Some("sk-secret123"));
    }

    #[test]
    fn should_format_epoch_millis_as_utc_when_listing_credentials() {
        // Given known UTC instants (leap-year February included)
        // When formatting them
        // Then the rendering is `YYYY-MM-DD HH:MM` in UTC
        assert_eq!(format_ms(0), "1970-01-01 00:00");
        assert_eq!(format_ms(1_700_000_000_000), "2023-11-14 22:13");
        assert_eq!(format_ms(1_739_577_600_000), "2025-02-15 00:00");
    }
}
