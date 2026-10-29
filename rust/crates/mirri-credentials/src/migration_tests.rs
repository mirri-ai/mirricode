//! Migration tests — config.toml `api_key` fields into the credential
//! store (docs/superpowers/plans/2026-10-28-provider-credential-store.md
//! Task 2, scenarios 1–7).

use std::path::PathBuf;
use std::sync::Mutex;

use crate::store::{CredentialStore, CredentialValue};
use crate::migration::{dry_run_config_api_keys, migrate_config_api_keys, SKIPPED_GUIDANCE};

fn write_config(home: &std::path::Path, text: &str) -> PathBuf {
    let cfg = home.join("config.toml");
    std::fs::write(&cfg, text).unwrap();
    cfg
}

/// Serializes tests that mutate process environment variables (edition
/// 2024 makes `set_var` unsafe; same discipline as mirri-core's flags
/// tests).
static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Scenario 1: no config file at all — an empty report, no error, and
/// the store stays empty (nothing to migrate).
#[test]
fn should_return_empty_report_when_config_file_is_missing() {
    let home = tempfile::tempdir().unwrap();
    let store = CredentialStore::open(home.path()).unwrap();
    let cfg = home.path().join("config.toml");

    let report = migrate_config_api_keys(&store, &cfg).unwrap();

    assert!(report.migrated.is_empty());
    assert!(report.cleaned.is_empty());
    assert!(report.skipped.is_empty());
    assert!(store.read_all().is_empty());
}

/// Scenario 2: `[providers.openai] api_key = "sk-old"` migrates into the
/// store, the `api_key` key disappears from the config file, and the
/// other keys of that provider table survive untouched.
#[test]
fn should_migrate_api_key_and_strip_field_from_config() {
    let home = tempfile::tempdir().unwrap();
    let cfg = write_config(
        home.path(),
        "[providers.openai]\ntype = \"openai\"\napi_key = \"sk-old\"\nbase_url = \"https://x\"\n",
    );
    let store = CredentialStore::open(home.path()).unwrap();

    let report = migrate_config_api_keys(&store, &cfg).unwrap();

    assert_eq!(report.migrated, vec!["openai"]);
    assert_eq!(report.cleaned, vec!["openai"]);
    assert_eq!(store.reveal("openai").unwrap(), "sk-old");
    let after = std::fs::read_to_string(&cfg).unwrap();
    assert!(!after.contains("sk-old"));
    assert!(after.contains("type = \"openai\""));
    assert!(after.contains("base_url"));
}

/// Scenario 3 (idempotence / interrupted-resume residue): the store
/// already holds the openai entry and the config still carries the
/// `api_key` field — the entry must not be duplicated (migrated empty)
/// but the config field is still compensated-cleaned (E.2).
#[test]
fn should_clean_residual_config_key_without_duplicating_store_entry() {
    let home = tempfile::tempdir().unwrap();
    let cfg = write_config(
        home.path(),
        "[providers.openai]\ntype = \"openai\"\napi_key = \"sk-old\"\n",
    );
    let store = CredentialStore::open(home.path()).unwrap();
    store
        .set("openai", CredentialValue::Key { key: "sk-existing".into() })
        .unwrap();

    let report = migrate_config_api_keys(&store, &cfg).unwrap();

    assert!(report.migrated.is_empty());
    assert_eq!(report.cleaned, vec!["openai"]);
    // The pre-existing entry is kept as-is — no overwrite from the residual
    // config field.
    assert_eq!(store.reveal("openai").unwrap(), "sk-existing");
    assert!(!std::fs::read_to_string(&cfg).unwrap().contains("sk-old"));
}

/// Scenario 4 (E.3): a provider whose section key is unusable (empty name)
/// is skipped without failing the whole migration.
#[test]
fn should_skip_malformed_provider_without_failing_migration() {
    let home = tempfile::tempdir().unwrap();
    let cfg = write_config(
        home.path(),
        "[providers.\"\"]\ntype = \"openai\"\napi_key = \"sk-bad\"\n[providers.openai]\ntype = \"openai\"\napi_key = \"sk-good\"\n",
    );
    let store = CredentialStore::open(home.path()).unwrap();

    let report = migrate_config_api_keys(&store, &cfg).unwrap();

    assert_eq!(report.migrated, vec!["openai"]);
    assert!(report.skipped.contains(&String::new()));
    assert_eq!(store.reveal("openai").unwrap(), "sk-good");
    // The skipped provider's key is still stripped from the config — it is
    // dead weight in the new world and must not survive the write-back.
    assert!(!std::fs::read_to_string(&cfg).unwrap().contains("sk-bad"));
}

/// Scenario 5 (E.4): a key living only in the process environment is not
/// a migration target — the config file has no `api_key` field for the
/// provider, so nothing is migrated or cleaned.
#[test]
fn should_not_migrate_env_derived_key_when_config_has_no_field() {
    let home = tempfile::tempdir().unwrap();
    let cfg = write_config(home.path(), "[providers.ci]\ntype = \"openai\"\n");
    let store = CredentialStore::open(home.path()).unwrap();
    let _env_guard = ENV_LOCK.lock().unwrap();
    // SAFETY: guarded by ENV_LOCK; the test process is single-threaded at
    // this point and no other test reads this variable concurrently.
    unsafe { std::env::set_var("OPENAI_API_KEY", "sk-from-env") };

    let report = migrate_config_api_keys(&store, &cfg).unwrap();

    // SAFETY: same discipline as the set above.
    unsafe { std::env::remove_var("OPENAI_API_KEY") };
    drop(_env_guard);

    assert!(report.migrated.is_empty());
    assert!(report.cleaned.is_empty());
    assert!(store.read_all().is_empty());
    assert!(std::fs::read_to_string(&cfg).unwrap().contains("[providers.ci]"));
}

/// Scenario 6 (E.5): dry run parses the same way but writes nothing — the
/// store stays empty and the config file is untouched, while `migrated`
/// lists the would-migrate providers.
#[test]
fn should_only_report_when_dry_run_without_writing() {
    let home = tempfile::tempdir().unwrap();
    let cfg = write_config(
        home.path(),
        "[providers.openai]\ntype = \"openai\"\napi_key = \"sk-old\"\n",
    );
    let store = CredentialStore::open(home.path()).unwrap();

    let report = dry_run_config_api_keys(&cfg).unwrap();

    assert_eq!(report.migrated, vec!["openai"]);
    assert!(store.read_all().is_empty());
    assert!(std::fs::read_to_string(&cfg).unwrap().contains("sk-old"));
}

/// Scenario 7 (E.6 unit pin): re-adding an `api_key` by hand after a
/// migration and running the migration again goes through the
/// compensating-clean path — migrated stays empty, cleaned contains the
/// provider, and the field is stripped again.
#[test]
fn should_reap_hand_re_added_key_on_subsequent_migration() {
    let home = tempfile::tempdir().unwrap();
    let cfg = write_config(
        home.path(),
        "[providers.openai]\ntype = \"openai\"\napi_key = \"sk-old\"\n",
    );
    let store = CredentialStore::open(home.path()).unwrap();
    migrate_config_api_keys(&store, &cfg).unwrap();
    assert_eq!(store.reveal("openai").unwrap(), "sk-old");

    // Simulate the user hand-editing the key back into config.toml.
    std::fs::write(
        &cfg,
        "[providers.openai]\ntype = \"openai\"\napi_key = \"sk-readded\"\n",
    )
    .unwrap();

    let report = migrate_config_api_keys(&store, &cfg).unwrap();

    assert!(report.migrated.is_empty());
    assert_eq!(report.cleaned, vec!["openai"]);
    assert_eq!(store.reveal("openai").unwrap(), "sk-old");
    assert!(!std::fs::read_to_string(&cfg).unwrap().contains("sk-readded"));
}

/// Extra: the camelCase `apiKey` spelling in the raw config is cleaned
/// alongside the snake_case one (both spellings exist in the wild — the
/// wire layer redacts both, so the migrator strips both). Detection
/// walks the typed `api_key` field, so a camelCase-only provider is not
/// a migration target itself, but its residual key is stripped whenever
/// a write-back happens.
#[test]
fn should_strip_camelcase_apikey_spelling_from_raw_config() {
    let home = tempfile::tempdir().unwrap();
    let cfg = write_config(
        home.path(),
        "[providers.openai]\ntype = \"openai\"\napi_key = \"sk-snake\"\n\
         [providers.camel]\ntype = \"openai\"\napiKey = \"sk-camel\"\nbase_url = \"https://y\"\n",
    );
    let store = CredentialStore::open(home.path()).unwrap();

    let report = migrate_config_api_keys(&store, &cfg).unwrap();

    assert_eq!(report.migrated, vec!["openai"]);
    let after = std::fs::read_to_string(&cfg).unwrap();
    assert!(!after.contains("sk-camel"));
    assert!(!after.contains("apiKey"));
    // The camelCase provider's other keys survive the write-back.
    assert!(after.contains("base_url = \"https://y\""));
}

/// Extra: a whitespace-only `api_key` is not a credential — nothing
/// migrates, nothing is cleaned, the file is not rewritten.
#[test]
fn should_ignore_blank_api_key_values() {
    let home = tempfile::tempdir().unwrap();
    let cfg = write_config(
        home.path(),
        "[providers.blank]\ntype = \"openai\"\napi_key = \"   \"\n",
    );
    let store = CredentialStore::open(home.path()).unwrap();

    let report = migrate_config_api_keys(&store, &cfg).unwrap();

    assert!(report.migrated.is_empty());
    assert!(report.cleaned.is_empty());
    assert!(store.read_all().is_empty());
    assert!(std::fs::read_to_string(&cfg).unwrap().contains("   "));
}

/// E.3 (per-entry alarm): every skipped provider is reported with its id
/// and the manual-handling guidance — a bare count would leave the user
/// with no way to know which entry needs attention.
#[test]
fn should_report_each_skipped_provider_with_manual_handling_guidance() {
    let home = tempfile::tempdir().unwrap();
    let cfg = write_config(
        home.path(),
        "[providers.\"\"]\ntype = \"openai\"\napi_key = \"sk-bad\"\n\
         [providers.\" \"]\ntype = \"openai\"\napi_key = \"sk-bad2\"\n\
         [providers.openai]\ntype = \"openai\"\napi_key = \"sk-good\"\n",
    );
    let store = CredentialStore::open(home.path()).unwrap();

    let report = migrate_config_api_keys(&store, &cfg).unwrap();

    assert_eq!(report.skipped.len(), 2, "both malformed ids are skipped");
    for id in &report.skipped {
        let guidance = format!(
            "provider credential '{id}' {SKIPPED_GUIDANCE}"
        );
        assert!(
            guidance.contains(id.as_str()),
            "the guidance names the skipped provider: {guidance}"
        );
        assert!(
            guidance.contains("manually"),
            "the guidance tells the user to handle it manually: {guidance}"
        );
    }
    // The well-formed provider still migrates — the skips never fail the
    // whole pass (E.3).
    assert_eq!(report.migrated, vec!["openai"]);
}
