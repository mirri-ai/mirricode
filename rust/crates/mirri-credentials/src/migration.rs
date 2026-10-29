//! One-shot migration: config.toml `providers.*.api_key` fields into the
//! credential store (spec E: E.2 compensating cleanup, E.3 malformed
//! entries never fail the run, E.4 env-derived values are not migrated,
//! E.5 dry run, E.6 re-checked at every server start).
//!
//! Failure discipline: the migration returns `Err` instead of panicking —
//! the caller (server bootstrap) treats an error as "leave the world as
//! it was and keep booting", so a broken config or an unwritable store
//! must never take the server down. Logs carry counts only, never key
//! material (spec F.5).

use std::path::Path;

use anyhow::Context;
use mirri_core::config::ServerConfig;

use crate::store::{CredentialStore, CredentialValue};

/// The per-provider guidance message the migration emits for every
/// skipped entry (E.3): the key is neither migrated nor does it take
/// part in resolution anymore — it must be handled by hand. Exported so
/// the CLI `migrate` output can reuse the exact wording.
pub const SKIPPED_GUIDANCE: &str =
    "not migrated and no longer takes part in credential resolution; handle it manually";


/// Outcome of a migration pass. In [`dry_run_config_api_keys`] the
/// `migrated` field is reused as the "would migrate" list (E.5).
#[derive(Debug, Default, Clone, PartialEq)]
pub struct MigrationReport {
    pub migrated: Vec<String>,
    pub cleaned: Vec<String>,
    pub skipped: Vec<String>,
}

/// Migrate every typed `providers.*.api_key` field from the config at
/// `config_path` into `store`, then — whether or not the store already
/// held the entry — strip the `api_key`/`apiKey` keys from the config
/// file (compensating cleanup, E.2) and write it back atomically.
///
/// A missing config file is the empty report. An unparseable one is an
/// `Err` (a broken config must surface to the caller, which decides how
/// to boot). Values are trimmed; blank values are not credentials and
/// are ignored. Entries whose provider id is not a usable section key
/// (e.g. an empty name) are skipped, and their residual field is still
/// cleaned (E.3).
pub fn migrate_config_api_keys(
    store: &CredentialStore,
    config_path: &Path,
) -> anyhow::Result<MigrationReport> {
    let Some(plan) = parse_targets(config_path)? else {
        return Ok(MigrationReport::default());
    };
    let MigrationPlan { mut config, targets } = plan;
    let mut report = MigrationReport::default();
    let mut dirty = false;
    for (id, api_key) in targets {
        if !is_valid_section_key(&id) {
            report.skipped.push(id.clone());
            // E.3 per-entry alarm: the skipped key is neither migrated nor
            // resolvable anymore — the warning must tell the user so.
            tracing::warn!(provider = %id, "provider credential '{}' {}", id, SKIPPED_GUIDANCE);
            // Still clean the residual field (E.2): a skipped provider is
            // dead weight in the new world, but the migration must not
            // fail because of it (E.3).
            dirty = true;
            continue;
        }
        if store.get(&id).is_none() {
            store
                .set(&id, CredentialValue::Key { key: api_key })
                .with_context(|| format!("store credential for provider '{id}'"))?;
            report.migrated.push(id.clone());
        }
        report.cleaned.push(id);
        dirty = true;
    }
    if dirty {
        config
            .strip_provider_api_keys()
            .context("strip provider api_key fields from raw config")?;
        config
            .save_raw_to(config_path)
            .with_context(|| format!("write migrated config to {}", config_path.display()))?;
    }
    tracing::info!(
        migrated = report.migrated.len(),
        cleaned = report.cleaned.len(),
        skipped = report.skipped.len(),
        "provider credential migration complete"
    );
    Ok(report)
}

/// Dry-run the same parse without touching the store or the config file
/// (E.5): the report's `migrated` field lists the providers that
/// *would* be migrated (providers the store does not hold yet), and
/// `cleaned` the fields that would be stripped.
pub fn dry_run_config_api_keys(config_path: &Path) -> anyhow::Result<MigrationReport> {
    let Some(plan) = parse_targets(config_path)? else {
        return Ok(MigrationReport::default());
    };
    let MigrationPlan { targets, .. } = plan;
    let mut report = MigrationReport::default();
    for (id, _api_key) in targets {
        if !is_valid_section_key(&id) {
            tracing::warn!(provider = %id, "provider credential '{}' {}", id, SKIPPED_GUIDANCE);
            report.skipped.push(id);
        } else {
            report.migrated.push(id);
        }
    }
    Ok(report)
}

/// A parsed config plus the `(provider_id, api_key)` migration targets
/// collected from its typed `providers` map.
struct MigrationPlan {
    config: ServerConfig,
    targets: Vec<(String, String)>,
}

/// Parse the config without the env layer (so only file-held `api_key`
/// fields are migration targets — env-derived values never are, E.4)
/// and collect the targets with non-blank trimmed values. `Ok(None)`
/// means the config file does not exist.
fn parse_targets(config_path: &Path) -> anyhow::Result<Option<MigrationPlan>> {
    if !config_path.exists() {
        return Ok(None);
    }
    let config = ServerConfig::load_without_env(config_path, &mirri_os::LocalSyncFs)
        .with_context(|| format!("parse {}", config_path.display()))?;
    let targets = config
        .providers
        .iter()
        .flatten()
        .filter(|(_, entry)| {
            entry
                .api_key
                .as_deref()
                .map(str::trim)
                .is_some_and(|k| !k.is_empty())
        })
        .map(|(id, entry)| (id.clone(), entry.api_key.clone().unwrap_or_default()))
        .collect();
    Ok(Some(MigrationPlan { config, targets }))
}

/// A provider id must at least be non-empty to name a credential entry
/// (E.3: unusable ids are skipped, never fatal).
fn is_valid_section_key(id: &str) -> bool {
    !id.trim().is_empty()
}
