//! Provider-domain dispatch handlers for the embedded
//! [`EmbeddedServer`](super::EmbeddedServer) — the P6
//! `providerDiscovery.refreshProviderModels` models.dev pull, the B4a
//! `providerDiscovery.listCatalog` browse, the B4b
//! `providerDiscovery.importCatalog` directory import, plus the B5-b
//! `provider.*` CRUD rows (`provider.get` / `provider.list` /
//! `provider.set` / `provider.delete`).
//!
//! The B5-b records live in the config.toml `[providers.<id>]` sections —
//! the same section the TS engine's kosong provider registry persists
//! through — reached via the raw TOML document so klient passthrough keys
//! survive a write/read cycle. Writes ride the standard
//! load-without-env → mutate → persist → reload pipeline; reads project
//! the camelCase klient `providerConfigSchema` shape (records are redacted
//! with the `apiKey` → `[REDACTED]` rule the Settled `config.get` wire
//! applies). Unknown-id reads answer JSON `null` (the klient `maybe`
//! semantics).

use serde::Deserialize;
use serde_json::{Value, json};

use mirri_transport::adapter::EngineError;

use super::config_handlers::{first_string_arg, redact_api_keys};
use super::model_handlers::record_args;
use super::responses::{
    CatalogModelEntry, CatalogProviderEntry, RefreshProviderModelsResult, to_result,
};
use super::{
    EmbeddedServer, MODELS_DEV_FETCH_TIMEOUT_SECS, engine_error, invalid_params, normalize_params,
    not_found_domain,
};

/// Runtime credential adoption (R2): when a provider record about to be
/// written carries a non-empty `apiKey` field, move the key into the
/// credential store and strip the field — no plaintext key ever lands in
/// config.toml through `provider.set` / `providerDiscovery.importCatalog`,
/// and no record is double-written (field + store). Adoption failure (no
/// store, unwritable) only warns and keeps the field in the record: the
/// user's key still persists, and the next startup migration adopts it —
/// which is why the strip happens only after a successful `store.set`.
fn adopt_runtime_api_key(
    provider_id: &str,
    mut record: Value,
    store: Option<&mirri_credentials::CredentialStore>,
) -> Value {
    let Some(api_key) = record
        .as_object_mut()
        .and_then(|map| map.remove("apiKey").or_else(|| map.remove("api_key")))
    else {
        return record;
    };
    let Some(key) = api_key.as_str().map(str::trim).filter(|k| !k.is_empty()) else {
        // An empty or non-string field is not a credential — drop it the
        // same way (a blank key never belongs in either tier).
        return record;
    };
    let Some(store) = store else {
        tracing::warn!(
            provider = %provider_id,
            "credential store unavailable; the apiKey field is written to config as-is \
             (the next startup migration adopts it)"
        );
        // Restore the field: nothing was adopted, the record keeps its key.
        record
            .as_object_mut()
            .expect("checked object above")
            .insert("apiKey".to_string(), api_key);
        return record;
    };
    if let Err(error) = store.set(
        provider_id,
        mirri_credentials::CredentialValue::Key { key: key.into() },
    ) {
        tracing::warn!(
            provider = %provider_id,
            %error,
            "runtime credential adoption failed; the apiKey field is written to config as-is \
             (the next startup migration adopts it)"
        );
        // Restore the field: nothing was adopted, the record keeps its key.
        record
            .as_object_mut()
            .expect("checked object above")
            .insert("apiKey".to_string(), api_key);
    }
    // Success: the field stays removed — the store holds the credential.
    record
}

impl EmbeddedServer {
    /// `providerDiscovery.refreshProviderModels` — refresh the engine's
    /// remote provider-model knowledge by pulling the models.dev directory
    /// (`api.json`), caching it under the Mirri home, and feeding the
    /// `model.list` display-name fallback and the catalog browse/import
    /// surfaces.
    ///
    /// The klient options are honored: `scope: "oauth"` answers the honest
    /// empty success (models.dev is an API-key directory; rust has no
    /// remote provider-model refresh) and `providerId` scopes the merge to
    /// one configured provider (an unknown id answers `provider.not_found`,
    /// like `model.getProvider`).
    ///
    /// **Degradation never errors the call** (the TS upstream contract):
    /// a failed fetch keeps the cached directory — the in-memory catalog
    /// when one is present, else the `<home>/models-dev.json` cache file —
    /// and surfaces the failure as a `config.diagnostics` warning instead.
    pub(super) async fn dispatch_refresh_provider_models(
        &self,
        params: Value,
    ) -> Result<Value, EngineError> {
        #[derive(Debug, Default, Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct RefreshOptions {
            #[serde(default)]
            scope: Option<String>,
            #[serde(default, rename = "providerId", alias = "provider_id")]
            provider_id: Option<String>,
        }
        // Accept the positional `[options]`, bare-object, and omitted
        // spellings (the klient tuple contract sends the optional options
        // object as the single argument).
        let options_value = match &params {
            Value::Null => Value::Null,
            Value::Array(items) => {
                if items.len() > 1
                    || items
                        .first()
                        .is_some_and(|item| !item.is_null() && !item.is_object())
                {
                    return Err(invalid_params(
                        "providerDiscovery.refreshProviderModels takes at most one options object",
                    ));
                }
                items.first().cloned().unwrap_or(Value::Null)
            }
            Value::Object(_) => params.clone(),
            _ => {
                return Err(invalid_params(
                    "providerDiscovery.refreshProviderModels takes at most one options object",
                ));
            }
        };
        let options: RefreshOptions = serde_json::from_value(normalize_params(options_value))
            .map_err(|e| invalid_params(e.to_string()))?;
        if let Some(scope) = &options.scope
            && scope != "all"
            && scope != "oauth"
        {
            return Err(invalid_params(format!(
                "scope must be \"all\" or \"oauth\", got {scope:?}"
            )));
        }
        // The OAuth scope has nothing to pull from an API-key directory.
        if options.scope.as_deref() == Some("oauth") {
            return to_result(RefreshProviderModelsResult::empty());
        }
        // A scoped refresh addresses one configured provider.
        if let Some(provider_id) = &options.provider_id {
            let config = self.config.read().unwrap_or_else(|e| e.into_inner());
            if !config
                .providers
                .as_ref()
                .is_some_and(|providers| providers.contains_key(provider_id))
            {
                return Err(not_found_domain(
                    "provider.not_found",
                    format!("provider {provider_id} does not exist"),
                ));
            }
        }

        let upstream_url = match &self.models_dev_url {
            // The disabled upstream (the hermetic test seam): the honest
            // empty success, no fetch, no warning.
            None => return to_result(RefreshProviderModelsResult::empty()),
            Some(url) => url.clone(),
        };

        match fetch_models_dev_catalog(&upstream_url).await {
            Ok(fetched) => {
                // Cache the full fetched directory under the home (the
                // restart/degradation source), best-effort: a read-only
                // home degrades to the in-memory catalog only. The disk
                // write is gated on `[model_catalog].persist_cache` (P5
                // R6): off ⇒ never write — and never delete an old file.
                if self
                    .config
                    .read()
                    .map_or(true, |c| c.persist_cache_enabled())
                    && let Err(error) =
                        std::fs::write(self.models_dev_cache_path(), fetched.to_string())
                {
                    tracing::warn!(
                        path = %self.models_dev_cache_path().display(),
                        %error,
                        "cannot cache the models.dev directory"
                    );
                }
                let merged = self.scope_models_dev_catalog(fetched, options.provider_id.as_deref());
                let result = self.merge_models_dev_catalog(&merged);
                *self
                    .models_dev_warning
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) = None;
                to_result(result)
            }
            Err(error) => {
                // Degradation: keep the current catalog — the in-memory
                // one when a previous refresh succeeded, else the cache
                // file — and surface the failure as a diagnostics warning
                // instead of an RPC error.
                let catalog_empty = self
                    .models_dev_catalog
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .is_none();
                if catalog_empty
                    && self
                        .config
                        .read()
                        .map_or(true, |c| c.persist_cache_enabled())
                    && let Ok(cached) = std::fs::read_to_string(self.models_dev_cache_path())
                        .map_err(|err| tracing::warn!(%err, "no models.dev cache to fall back on"))
                        .and_then(|text| {
                            serde_json::from_str::<Value>(&text).map_err(
                                |err| tracing::warn!(%err, "the models.dev cache is unparseable"),
                            )
                        })
                {
                    let scoped =
                        self.scope_models_dev_catalog(cached, options.provider_id.as_deref());
                    self.merge_models_dev_catalog(&scoped);
                }
                *self
                    .models_dev_warning
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) = Some(format!(
                    "models.dev refresh failed ({error}); serving the cached model directory"
                ));
                let unchanged = self
                    .models_dev_catalog
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .as_ref()
                    .map(|catalog| {
                        catalog
                            .as_object()
                            .map(|providers| {
                                providers
                                    .keys()
                                    .map(|provider| provider.to_string())
                                    .collect::<Vec<_>>()
                            })
                            .unwrap_or_default()
                    })
                    .unwrap_or_default();
                to_result(RefreshProviderModelsResult {
                    changed: Vec::new(),
                    unchanged,
                    failed: Vec::new(),
                })
            }
        }
    }

    /// `providerDiscovery.listCatalog` — browse the models.dev directory
    /// (C4). Read-only in-memory/disk cache with zero network: the empty
    /// array is the honest not-yet-refreshed signal (C1/US-5 degradation
    /// path). The returned rows are directory facts — never the configured
    /// set (US-5) — and never credentials (N4).
    pub(super) async fn dispatch_list_provider_catalog(
        &self,
        _params: Value,
    ) -> Result<Value, EngineError> {
        self.ensure_models_dev_catalog_loaded();
        let catalog = self
            .models_dev_catalog
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let entries = catalog
            .map(|c| catalog_provider_entries(&c))
            .unwrap_or_default();
        serde_json::to_value(entries).map_err(|e| EngineError::Internal(e.to_string()))
    }

    /// `providerDiscovery.importCatalog` — B4b (G-8 后半): import one
    /// models.dev directory entry as a configured provider plus its full
    /// model-alias set in ONE config write transaction (C4). The provider
    /// record lands in `[providers.<id>]` with the type from the
    /// directory's re-derived wire (never the pruned browse payload's
    /// `wire_type` — the invalid bedrock/cohere face is rejected here) and
    /// the optional credentials.
    /// The directory's model map is filtered through the shared
    /// usability predicate (import and browse emit one set, ADR-004):
    /// an unusable chat model (no positive limit.context,
    /// deprecated/alpha, non-text output, embedding-marked) is skipped,
    /// never imported with a zero context.
    /// An OAuth-managed provider id answers the
    /// provider.oauth_managed domain error; any other existing id
    /// MERGES (ADR-004): existing aliases stay untouched, tombstones
    /// hold, only new models append, and an unset default is seeded.
    /// An unloaded directory is a
    /// clear not-found error, never an implicit network fetch.
    pub(super) async fn dispatch_import_provider_catalog(
        &self,
        params: Value,
    ) -> Result<Value, EngineError> {
        #[derive(Debug, Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct ImportCatalogArgs {
            #[serde(alias = "catalog_id")]
            catalog_id: String,
            #[serde(default)]
            id: Option<String>,
            #[serde(default, alias = "api_key")]
            api_key: Option<String>,
            #[serde(default, alias = "base_url")]
            base_url: Option<String>,
            #[serde(default, alias = "exclude_model_ids")]
            exclude_model_ids: Option<Vec<String>>,
        }
        // Accept the positional `[args]`, bare-object, and omitted
        // spellings (the klient tuple contract sends the options object as
        // the single argument).
        let args_value = match &params {
            Value::Null => Value::Null,
            Value::Array(items) => {
                if items.len() > 1
                    || items
                        .first()
                        .is_some_and(|item| !item.is_null() && !item.is_object())
                {
                    return Err(invalid_params(
                        "providerDiscovery.importCatalog takes at most one options object",
                    ));
                }
                items.first().cloned().unwrap_or(Value::Null)
            }
            Value::Object(_) => params.clone(),
            _ => {
                return Err(invalid_params(
                    "providerDiscovery.importCatalog takes at most one options object",
                ));
            }
        };
        let args: ImportCatalogArgs = serde_json::from_value(normalize_params(args_value))
            .map_err(|e| invalid_params(e.to_string()))?;

        // The directory must already be loaded (refresh or the lazy disk
        // cache) — the import never fetches on its own.
        self.ensure_models_dev_catalog_loaded();
        let catalog = self
            .models_dev_catalog
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let entry = catalog.as_ref().and_then(|c| c.get(&args.catalog_id));
        let Some(entry) = entry.and_then(Value::as_object) else {
            let message = if catalog.is_none() {
                "the models.dev directory is not loaded; refresh it with \
                 providerDiscovery.refreshProviderModels before importing"
            } else {
                "the models.dev directory has no such entry (refresh it to \
                 pull the latest directory)"
            };
            return Err(not_found_domain(
                "provider.not_found",
                format!(
                    "catalog entry {} cannot be imported: {message}",
                    args.catalog_id
                ),
            ));
        };

        // Re-derive the wire from the directory source (the TS
        // `resolveModelsDevWire`): an unresolvable entry — an unknown
        // declared type or the bedrock/cohere npm — is the invalid face the
        // pruned browse shape cannot express, and the import rejects it.
        let Some(wire) = models_dev_wire_type(entry) else {
            return Err(invalid_params(format!(
                "models.dev catalog entry {} cannot be imported: the entry resolves to no \
                 known wire protocol",
                args.catalog_id
            )));
        };
        // The needs-base-url face (the TS `resolveModelsDevImport`): a
        // declared-but-placeholder api demands an explicit base_url.
        if args.base_url.is_none() && catalog_needs_base_url(entry, Some(wire)) {
            return Err(invalid_params(format!(
                "models.dev catalog entry {} requires a base_url",
                args.catalog_id
            )));
        }
        // The directory's model map is filtered through the shared
        // usability predicate (the TS `modelsDevProviderModels` set —
        // embedding/deprecated/alpha/no-context models are skipped, never
        // imported with a zero context); an entry with no models map
        // cannot be imported (TS parity).
        let models = entry.get("models").and_then(Value::as_object);
        let Some(models) = models else {
            return Err(invalid_params(format!(
                "models.dev catalog entry {} cannot be imported: it declares no models map",
                args.catalog_id
            )));
        };
        if models.is_empty() {
            return Err(invalid_params(format!(
                "models.dev catalog entry {} cannot be imported: it has no importable models",
                args.catalog_id
            )));
        }

        let target_id = args.id.clone().unwrap_or_else(|| args.catalog_id.clone());
        // The derived aliases: one per usable directory model, each
        // carrying the directory facts ADR-004 derives (capabilities /
        // max_output_size / reasoning_key / support_efforts); displayName
        // and description stay unwritten (D-M1's chain resolves names, the
        // curated gate is the user's explicit act).
        let aliases: Vec<(String, Value)> = models
            .iter()
            .filter_map(|(model_id, raw)| {
                super::models_dev_import::models_dev_alias_record(raw).map(|record| {
                    let mut alias = serde_json::Map::new();
                    alias.insert("provider".to_string(), json!(target_id));
                    alias.insert("model".to_string(), json!(model_id));
                    alias.insert("max_context_size".to_string(), json!(record.context));
                    if !record.capabilities.is_empty() {
                        alias.insert("capabilities".to_string(), json!(record.capabilities));
                    }
                    if let Some(max_output_size) = record.max_output_size {
                        alias.insert("max_output_size".to_string(), json!(max_output_size));
                    }
                    if let Some(reasoning_key) = record.reasoning_key {
                        alias.insert("reasoning_key".to_string(), json!(reasoning_key));
                    }
                    if let Some(support_efforts) = record.support_efforts {
                        alias.insert("support_efforts".to_string(), json!(support_efforts));
                    }
                    (format!("{target_id}/{model_id}"), Value::Object(alias))
                })
            })
            .collect();
        // An entry whose every model is unusable is the TS "no importable
        // models" face.
        if aliases.is_empty() {
            return Err(invalid_params(format!(
                "models.dev catalog entry {} cannot be imported: it has no importable models",
                args.catalog_id
            )));
        }
        // One config write transaction: the provider record and every
        // appended alias land together or not at all (R4). All merge
        // decisions read the in-closure reloaded config — no earlier
        // snapshot can race the write (ADR-004: an existing id merges;
        // existing aliases stay byte-for-byte untouched, tombstones hold,
        // only new models append).
        let mut imported = 0usize;
        self.mutate_config_raw(|config| {
            let existing = config
                .providers
                .as_ref()
                .and_then(|providers| providers.get(&target_id))
                .cloned();
            // OAuth-managed providers are never rewritten by an import.
            if existing.as_ref().is_some_and(|entry| entry.oauth.is_some()) {
                return Err(provider_oauth_managed(&target_id));
            }
            // Effective tombstones = the existing denylist ∪ this import's
            // exclusions. One shared set feeds BOTH the record written below
            // and the append-loop filter — the record is written before the
            // tombstones are rebuilt, so deriving them independently from
            // `existing` would resurrect an exclusion on re-import (ADR-005).
            let effective_tombstones: Vec<String> = {
                let mut ids: Vec<String> = existing
                    .as_ref()
                    .and_then(|entry| entry.removed_model_ids.clone())
                    .unwrap_or_default();
                if let Some(excluded) = &args.exclude_model_ids {
                    for model_id in excluded {
                        if !ids.contains(model_id) {
                            ids.push(model_id.clone());
                        }
                    }
                }
                ids
            };
            // The merged provider record: the re-derived wire type (the
            // directory is the authoritative source — always rewritten),
            // credentials kept from the existing record when the import
            // carries none (the TS `options.apiKey ?? existing.apiKey`),
            // tombstones preserved so a user-deleted model stays deleted
            // across imports. The raw entry is the base: every other
            // user-level setting — default_model, env, custom_headers,
            // media_file_refs, model_source/source — survives verbatim,
            // the import only ever overlays the four rebuilt fields. Raw
            // keys are normalized to the snake_case document spelling
            // (the TS `providerEntryToToml` transform: camelCase aliases
            // such as `defaultModel` / `removedModelIds` canonicalize,
            // nested value maps keep their keys, unknown keys pass
            // through untouched).
            let mut provider_record = {
                let raw_entry = config.raw_document().ok().and_then(|raw| {
                    raw.get("providers")
                        .and_then(toml::Value::as_table)
                        .and_then(|providers| providers.get(&target_id))
                        .and_then(toml::Value::as_table)
                        .cloned()
                });
                match raw_entry {
                    Some(raw_entry) => {
                        let mut base = serde_json::Map::new();
                        for (key, value) in raw_entry {
                            let normalized = mirri_core::camel_to_snake(&key);
                            // Fail loud instead of silently nulling a key
                            // that cannot round-trip: a dropped value would
                            // clear a user setting on re-import.
                            let json_value = serde_json::to_value(value).map_err(|_| {
                                invalid_params(format!(
                                    "provider {} key {normalized} cannot be converted for storage",
                                    target_id
                                ))
                            })?;
                            base.insert(normalized, json_value);
                        }
                        base
                    }
                    // No raw entry (e.g. the record exists only as the
                    // env-synthesized overlay): build from scratch as before.
                    None => serde_json::Map::new(),
                }
            };
            provider_record.insert("type".to_string(), json!(wire));
            let api_key = args
                .api_key
                .clone()
                .or_else(|| existing.as_ref().and_then(|entry| entry.api_key.clone()));
            if let Some(api_key) = api_key {
                provider_record.insert("api_key".to_string(), json!(api_key));
            }
            let base_url = args
                .base_url
                .clone()
                .or_else(|| existing.as_ref().and_then(|entry| entry.base_url.clone()));
            if let Some(base_url) = base_url {
                provider_record.insert("base_url".to_string(), json!(base_url));
            }
            if !effective_tombstones.is_empty() {
                provider_record.insert(
                    "removed_model_ids".to_string(),
                    json!(effective_tombstones),
                );
            }
            // Runtime credential adoption (R2): a record about to be
            // written with a non-empty `api_key` moves the key into the
            // credential store and the field is stripped — the same
            // contract `provider.set` applies. A store failure keeps the
            // field (warned): the startup migration adopts it later.
            let provider_record = adopt_runtime_api_key(
                &target_id,
                Value::Object(provider_record),
                self.credentials(),
            );
            config
                .set_raw_section_entry(
                    "providers",
                    &target_id,
                    Some(&provider_record),
                )
                .map_err(engine_error)?;

            // The configured model ids of this provider (any alias whose
            // provider field targets it — short-form keys included) and
            // its tombstones.
            let existing_model_ids: std::collections::HashSet<String> = config
                .models
                .as_ref()
                .map(|models| {
                    models
                        .iter()
                        .filter(|(_, alias)| alias.provider == target_id)
                        .map(|(_, alias)| alias.model.clone())
                        .collect()
                })
                .unwrap_or_default();
            let tombstones: std::collections::HashSet<String> =
                std::collections::HashSet::from_iter(effective_tombstones.iter().cloned());

            let mut appended: Vec<String> = Vec::new();
            for (alias_key, alias) in &aliases {
                let model_id = alias
                    .get("model")
                    .and_then(Value::as_str)
                    .expect("the derived alias always carries its model id");
                if existing_model_ids.contains(model_id) {
                    continue;
                }
                if tombstones.contains(model_id) {
                    continue;
                }
                config
                    .set_raw_section_entry("models", alias_key, Some(alias))
                    .map_err(engine_error)?;
                imported += 1;
                appended.push(alias_key.clone());
            }

            // A fresh environment (no default model anywhere) seeds the
            // first model the import made real — appended or already
            // configured, never a tombstoned one (the TS seed semantics;
            // an existing pointer is never moved).
            if config.default_model.is_none() {
                let seed = appended.first().cloned().or_else(|| {
                    aliases
                        .iter()
                        .find(|(_, alias)| {
                            let model_id = alias
                                .get("model")
                                .and_then(Value::as_str)
                                .unwrap_or_default();
                            !tombstones.contains(model_id) && existing_model_ids.contains(model_id)
                        })
                        .map(|(alias_key, _)| alias_key.clone())
                });
                if let Some(seed) = seed {
                    config
                        .replace_raw_sections(&[(
                            "default_model".to_string(),
                            Some(Value::String(seed)),
                        )])
                        .map_err(engine_error)?;
                }
            }
            Ok(())
        })?;

        // The klient result never echoes credentials (N4); the count is
        // the appended aliases (the TS `added`).
        Ok(json!({ "provider": target_id, "models_imported": imported }))
    }

    // --- shared models.dev helpers --------------------------------------

    /// `<mirri-home>/models-dev.json` — the on-disk cache of the last
    /// successful pull. `pub(super)`: the lazy-load/preheat path in `mod.rs`
    /// resolves the same path when restoring the catalog without a refresh.
    pub(super) fn models_dev_cache_path(&self) -> std::path::PathBuf {
        self.home_root().join("models-dev.json")
    }

    /// Keep only the addressed provider of the directory (or the whole
    /// directory when no `providerId` scoped the refresh).
    fn scope_models_dev_catalog(&self, catalog: Value, provider_id: Option<&str>) -> Value {
        match provider_id {
            None => catalog,
            Some(provider_id) => {
                let mut scoped = serde_json::Map::new();
                if let Some(entry) = catalog.get(provider_id) {
                    scoped.insert(provider_id.to_string(), entry.clone());
                }
                Value::Object(scoped)
            }
        }
    }

    /// Swap the in-memory catalog for `catalog` and compute the klient
    /// result rows: every provider whose model count moved is a `changed`
    /// row, the rest are `unchanged`.
    fn merge_models_dev_catalog(
        &self,
        catalog: &Value,
    ) -> super::responses::RefreshProviderModelsResult {
        let previous = self
            .models_dev_catalog
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let mut result = RefreshProviderModelsResult::empty();
        if let Some(providers) = catalog.as_object() {
            for (provider_id, entry) in providers {
                let new_count = entry
                    .get("models")
                    .and_then(Value::as_object)
                    .map(|models| models.len())
                    .unwrap_or(0);
                let old_count = previous
                    .as_ref()
                    .and_then(|catalog| catalog.get(provider_id))
                    .and_then(|entry| entry.get("models"))
                    .and_then(Value::as_object)
                    .map(|models| models.len())
                    .unwrap_or(0);
                let provider_name = entry
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or(provider_id)
                    .to_string();
                let (added, removed) = match new_count.cmp(&old_count) {
                    std::cmp::Ordering::Greater => (new_count - old_count, 0),
                    std::cmp::Ordering::Less => (0, old_count - new_count),
                    std::cmp::Ordering::Equal => (0, 0),
                };
                if added + removed > 0 {
                    result
                        .changed
                        .push(super::responses::RefreshProviderChangedEntry {
                            provider_id: provider_id.clone(),
                            provider_name,
                            added,
                            removed,
                        });
                } else {
                    result.unchanged.push(provider_id.clone());
                }
            }
        }
        *self
            .models_dev_catalog
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(std::sync::Arc::new(catalog.clone()));
        result
    }
}

/// Fetch the models.dev `api.json` directory — a 10s-bounded GET that
/// must answer 2xx JSON (the TS `UPSTREAM_FETCH_TIMEOUT_MS` upstream
/// contract). The error is a plain message (the degradation warning's
/// cause), never an RPC error.
async fn fetch_models_dev_catalog(url: &str) -> Result<Value, String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(
            MODELS_DEV_FETCH_TIMEOUT_SECS,
        ))
        .build()
        .map_err(|error| error.to_string())?;
    let response = client
        .get(url)
        .header("Accept", "application/json")
        .send()
        .await
        .map_err(|error| error.to_string())?;
    if !response.status().is_success() {
        return Err(format!("upstream answered HTTP {}", response.status()));
    }
    let catalog: Value = response.json().await.map_err(|error| error.to_string())?;
    if !catalog.is_object() {
        return Err("the upstream payload is not a JSON object".to_string());
    }
    Ok(catalog)
}

// --- P7 B4b: the importCatalog rejection helper ----------------------------

/// Build the `provider.oauth_managed` domain error (the TS
/// PROVIDER_OAUTH_MANAGED face over the import path: an OAuth-managed
/// provider is never rewritten by a directory import).
fn provider_oauth_managed(id: &str) -> EngineError {
    EngineError::Rpc {
        code: mirri_proto::rpc::json_rpc_code_for_domain_code("provider.oauth_managed"),
        message: format!("provider {id} is managed by OAuth login; log out before importing"),
        data: Some(mirri_proto::rpc::domain_error_data(
            "provider.oauth_managed",
        )),
    }
}

// --- P6 B4a: the listCatalog browse projection ---------------------------

/// The known models.dev wire types — the TS `KNOWN_WIRE_TYPES`
/// (`modelsDev.ts`); `wire_type` is emitted only from an explicit
/// declaration, never guessed (the pruned browse shape drops the TS
/// `guessed` flag).
const KNOWN_CATALOG_WIRE_TYPES: &[&str] = &[
    "anthropic",
    "openai",
    "kimi",
    "google-genai",
    "openai_responses",
    "vertexai",
];

/// Project the models.dev directory (`api.json`-shaped, top-level provider
/// id → entry) into the pruned `listCatalog` browse rows — the TS
/// `toModelsDevProviderItem` mapping (`modelsDevUpstream.ts`) minus
/// `guessed` / `rejected` / `reject_reason`. Field paths and derivations
/// mirror the TS baseline (Step 0 of the P6 brief); missing fields degrade
/// to omission (R2 lenient parsing, the refresh path's existing mode). A
/// non-object catalog answers the empty vector — never an error.
fn catalog_provider_entries(catalog: &Value) -> Vec<CatalogProviderEntry> {
    let Some(providers) = catalog.as_object() else {
        return Vec::new();
    };
    providers
        .iter()
        .map(|(id, entry)| catalog_provider_entry(id, entry.as_object()))
        .collect()
}

/// One directory entry projected as a browse row. `name` falls back to
/// the provider id — the TS `entry.name || id` and the refresh path's
/// existing `unwrap_or(provider_id)` pattern.
fn catalog_provider_entry(
    id: &str,
    entry: Option<&serde_json::Map<String, Value>>,
) -> CatalogProviderEntry {
    let Some(entry) = entry else {
        return CatalogProviderEntry {
            id: id.to_string(),
            name: id.to_string(),
            wire_type: None,
            needs_base_url: false,
            env_key: None,
            models: Vec::new(),
        };
    };
    let resolved_wire = models_dev_wire_type(entry);
    CatalogProviderEntry {
        id: id.to_string(),
        name: entry
            .get("name")
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty())
            .unwrap_or(id)
            .to_string(),
        // Only an explicit known declaration is emitted (the TS item's
        // `wire_type` is null for the invalid face and guessed after
        // inference — the pruned shape conflates neither).
        wire_type: entry
            .get("type")
            .and_then(Value::as_str)
            .filter(|declared| KNOWN_CATALOG_WIRE_TYPES.contains(declared))
            .map(str::to_string),
        needs_base_url: catalog_needs_base_url(entry, resolved_wire),
        env_key: entry
            .get("env")
            .and_then(Value::as_array)
            .and_then(|env| env.first())
            .and_then(Value::as_str)
            .map(str::to_string),
        models: catalog_model_entries(entry.get("models")),
    }
}

/// The directory's models map → the browse rows: one shared filter and
/// derivation with the import face (ADR-004) — the browse set is exactly
/// what an import would write. `context_length` is the directory's
/// `limit.context`; `capabilities`/`reasoning` are the derived facts.
fn catalog_model_entries(models: Option<&Value>) -> Vec<CatalogModelEntry> {
    let Some(models) = models.and_then(Value::as_object) else {
        return Vec::new();
    };
    models
        .iter()
        .filter_map(|(model_id, raw)| {
            let record = super::models_dev_import::models_dev_alias_record(raw)?;
            let reasoning = record.capabilities.iter().any(|cap| cap == "thinking");
            Some(CatalogModelEntry {
                id: model_id.clone(),
                name: raw
                    .get("name")
                    .and_then(Value::as_str)
                    .filter(|name| !name.is_empty())
                    .map(str::to_string),
                context_length: Some(record.context),
                capabilities: (!record.capabilities.is_empty())
                    .then(|| record.capabilities.clone()),
                reasoning: Some(reasoning),
            })
        })
        .collect()
}

/// The models.dev wire-type resolution — the TS `resolveModelsDevWire`
/// (`modelsDev.ts`): a known declared `type` wins; an unknown declared
/// type has no wire (the TS 'invalid' face); otherwise the id/npm
/// declaration hints, then the SDK default `openai` unless the npm marks
/// bedrock/cohere (also invalid).
fn models_dev_wire_type(entry: &serde_json::Map<String, Value>) -> Option<&'static str> {
    if let Some(declared) = entry
        .get("type")
        .and_then(Value::as_str)
        .filter(|t| !t.is_empty())
    {
        return KNOWN_CATALOG_WIRE_TYPES
            .iter()
            .find(|known| **known == declared)
            .copied();
    }
    if let Some(inferred) = infer_declared_catalog_wire_type(entry) {
        return Some(inferred);
    }
    let npm = entry
        .get("npm")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_lowercase();
    if npm.contains("amazon-bedrock") || npm.contains("cohere") {
        return None;
    }
    Some("openai")
}

/// The TS `inferDeclaredWireType` — id/npm declaration sniffing (the
/// claude/anthropic, vertex, google/gemini, openai markers).
fn infer_declared_catalog_wire_type(
    entry: &serde_json::Map<String, Value>,
) -> Option<&'static str> {
    let npm = entry
        .get("npm")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_lowercase();
    let id = entry
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_lowercase();
    if npm.contains("anthropic") || id.contains("anthropic") || id.contains("claude") {
        return Some("anthropic");
    }
    if id.contains("vertex") {
        return Some("vertexai");
    }
    if npm.contains("google") || id.contains("google") || id.contains("gemini") {
        return Some("google-genai");
    }
    if npm.contains("openai") || id.contains("openai") {
        return Some("openai");
    }
    None
}

/// The TS `needs_base_url` derivation (`resolveModelsDevImport`'s
/// 'needs-base-url' kind = `modelsDevBaseUrl` undefined ∧
/// `modelsDevEndpointRequired`): a concrete `api` (no `${` placeholder)
/// answers false; a `${`-placeholder or missing api flips to true when the
/// resolved wire needs an endpoint (openai/anthropic wires without the
/// stock SDK npm; any wire carrying a declared api). An unresolvable wire
/// (unknown declared type / bedrock-cohere npm) is the TS 'invalid' face —
/// never needs a base URL.
fn catalog_needs_base_url(
    entry: &serde_json::Map<String, Value>,
    wire: Option<&'static str>,
) -> bool {
    let api = entry.get("api").and_then(Value::as_str).unwrap_or("");
    if !api.is_empty() && !api.contains("${") {
        // modelsDevBaseUrl resolves — the catalog knows the endpoint.
        return false;
    }
    let Some(wire) = wire else {
        // TS 'invalid' face (rejected) — not a browse concern.
        return false;
    };
    if !api.is_empty() {
        // modelsDevEndpointRequired: a declared api means an endpoint must
        // be collected (the `${` placeholder case).
        return true;
    }
    let npm = entry
        .get("npm")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_lowercase();
    match wire {
        "openai" | "openai_responses" => npm != "@ai-sdk/openai",
        "anthropic" => npm != "@ai-sdk/anthropic",
        _ => false,
    }
}

impl EmbeddedServer {
    /// `provider.get(name)` — the providerService record for one provider,
    /// or JSON `null` for an unknown id (the klient `maybe` semantics).
    pub(super) async fn dispatch_provider_get(&self, params: Value) -> Result<Value, EngineError> {
        let name = first_string_arg(&params, &["name", "id", "providerId"])?;
        let config = self.config.read().unwrap_or_else(|e| e.into_inner());
        let records = config.to_provider_records_wire();
        Ok(records
            .get(&name)
            .map(|record| redact_api_keys(record.clone()))
            .unwrap_or(Value::Null))
    }

    /// `provider.list()` — every providerService record, keyed by provider
    /// id (the klient record contract).
    pub(super) async fn dispatch_provider_list(&self) -> Result<Value, EngineError> {
        let config = self.config.read().unwrap_or_else(|e| e.into_inner());
        Ok(redact_api_keys(config.to_provider_records_wire()))
    }

    /// `provider.set(name, config)` — replace the providerService record for
    /// one provider (a full-record replace, matching the TS service), then
    /// persist and reload. A record carrying a non-empty `apiKey` field is
    /// runtime-adopted first (R2): the key moves into the credential store
    /// and the field is stripped from the written record — no key lands in
    /// config.toml, no double-write. A store failure only warns: the key
    /// stays in the written record and the next startup migration adopts
    /// it (the strip happens only after a successful `store.set`).
    /// noResult on the wire.
    pub(super) async fn dispatch_provider_set(&self, params: Value) -> Result<Value, EngineError> {
        let (name, record) = record_args(
            &params,
            &["name", "id", "providerId"],
            &["config", "provider"],
        )?;
        let record = adopt_runtime_api_key(&name, record, self.credentials());
        self.set_record_section("providers", &name, record)?;
        Ok(Value::Null)
    }

    /// `provider.delete(name)` — remove the providerService record (and
    /// clear the `defaultProvider` pointer when it addressed the deleted
    /// provider, matching the TS service; no model cascade — that is the
    /// separate `config.removeProvider` behavior). noResult on the wire.
    pub(super) async fn dispatch_provider_delete(
        &self,
        params: Value,
    ) -> Result<Value, EngineError> {
        let name = first_string_arg(&params, &["name", "id", "providerId"])?;
        self.mutate_config_raw(|config| {
            config
                .set_raw_section_entry("providers", &name, None)
                .map_err(engine_error)?;
            if config.default_provider.as_deref() == Some(name.as_str()) {
                config
                    .replace_raw_sections(&[("default_provider".to_string(), None)])
                    .map_err(engine_error)?;
            }
            Ok(())
        })?;
        // Provider deleted from the config — cascade its stored credential
        // (A.5b/V11): best-effort, failure only warns (the credential
        // residue is caught by the orphan diagnostic on the next
        // credential.list) and never rolls back the config deletion.
        // The in-memory auth-failure flag goes with it: a re-created
        // provider with the same id must not inherit the stale
        // needs_reconfigure signal.
        self.clear_provider_auth_failure(&name);
        if let Some(store) = self.credentials()
            && let Err(error) = store.remove(&name)
        {
            tracing::warn!(
                provider = %name,
                %error,
                "credential cascade after provider.delete failed"
            );
        }
        Ok(Value::Null)
    }
}
