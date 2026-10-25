//! Wire projections of the config: the camelCase MirriConfig JSON, patch
//! merge, the klient `configService` records, redaction, and the
//! JSON/TOML key-spelling conversions.

use super::*;

impl ServerConfig {
    /// Build the camelCase MirriConfig wire JSON for `config.get`.
    ///
    /// Absent fields are **omitted** (not `null`) so the result is accepted
    /// by the TypeScript `MirriConfigSchema` (zod `.optional()` rejects
    /// `null`). `version`/`serverId`/`engine`/`startedAt` are deliberately
    /// **not** included — those live on `core.info`. The `raw` field carries
    /// the original parsed TOML document (when the config was loaded from a
    /// file) for round-trip fidelity and TS `config.raw` parity.
    ///
    /// Legacy `[provider]`/`[model]` sections are mapped into the `providers`
    /// and `models` maps when the multi-provider fields are absent. The
    /// mapping uses `"default"` as the provider key and `model.id` as the
    /// model alias key. The model alias's `provider` field always points to
    /// `"default"` (the providers map key), not to the provider type name.
    pub fn to_mirri_config_wire(&self) -> serde_json::Value {
        // Build providers map: prefer explicit `providers`, fall back to
        // legacy `[provider]`.
        let providers = if let Some(ref providers) = self.providers {
            providers_to_wire(providers)
        } else if let Some(ref provider) = self.provider {
            let mut map = serde_json::Map::new();
            let provider_type = self
                .model
                .as_ref()
                .and_then(|model| model.provider.as_deref())
                .unwrap_or("openai");
            let mut entry = serde_json::Map::new();
            entry.insert("type".to_string(), Value::String(provider_type.to_string()));
            if let Some(ref key) = provider.api_key {
                entry.insert("apiKey".to_string(), Value::String(key.clone()));
            }
            if let Some(ref url) = provider.base_url {
                entry.insert("baseUrl".to_string(), Value::String(url.clone()));
            }
            map.insert("default".to_string(), Value::Object(entry));
            Value::Object(map)
        } else {
            Value::Object(serde_json::Map::new())
        };

        // Build models map: prefer explicit `models`, fall back to legacy
        // `[model]`. The model alias's `provider` is always `"default"` (the
        // key used in the providers map above), not the provider type name.
        let models = if let Some(ref models) = self.models {
            models_to_wire(models)
        } else if let Some(ref model) = self.model {
            let mut map = serde_json::Map::new();
            let mut entry = serde_json::Map::new();
            entry.insert("provider".to_string(), Value::String("default".to_string()));
            entry.insert("model".to_string(), Value::String(model.id.clone()));
            entry.insert(
                "maxContextSize".to_string(),
                Value::Number(serde_json::Number::from(200000u64)),
            );
            map.insert(model.id.clone(), Value::Object(entry));
            Value::Object(map)
        } else {
            Value::Object(serde_json::Map::new())
        };

        // Build the wire object, omitting absent fields (zod .optional()
        // rejects null, so we skip unset fields entirely).
        let mut wire = serde_json::Map::new();
        wire.insert("providers".to_string(), providers);
        if let Some(v) = &self.default_provider {
            wire.insert("defaultProvider".into(), Value::String(v.clone()));
        } else if self.provider.is_some() {
            wire.insert("defaultProvider".into(), Value::String("default".into()));
        }
        if let Some(v) = &self.default_model {
            wire.insert("defaultModel".into(), Value::String(v.clone()));
        } else if let Some(model) = &self.model {
            wire.insert("defaultModel".into(), Value::String(model.id.clone()));
        }
        if !models.as_object().map(|m| m.is_empty()).unwrap_or(true) {
            wire.insert("models".into(), models);
        }
        if let Some(v) = self.thinking.as_ref().map(thinking_to_wire) {
            wire.insert("thinking".into(), v);
        }
        if let Some(v) = self.permission.as_ref().map(permission_to_wire) {
            wire.insert("permission".into(), v);
        }
        if let Some(hooks) = &self.hooks {
            let arr: Vec<Value> = hooks.iter().map(hook_to_wire).collect();
            wire.insert("hooks".into(), Value::Array(arr));
        }
        if let Some(v) = self.loop_control.as_ref().map(loop_control_to_wire) {
            wire.insert("loopControl".into(), v);
        }
        if let Some(flags) = &self.experimental {
            wire.insert(
                "experimental".into(),
                serde_json::to_value(flags).unwrap_or(Value::Null),
            );
        }
        if let Some(v) = self.plan_mode {
            wire.insert("planMode".into(), Value::Bool(v));
        }
        if let Some(v) = self.yolo {
            wire.insert("yolo".into(), Value::Bool(v));
        }
        if let Some(v) = &self.default_permission_mode {
            wire.insert("defaultPermissionMode".into(), Value::String(v.clone()));
        }
        if let Some(v) = self.default_plan_mode {
            wire.insert("defaultPlanMode".into(), Value::Bool(v));
        }
        if let Some(v) = self.services.as_ref().map(toml_to_json) {
            wire.insert("services".into(), v);
        }
        if let Some(v) = self.merge_all_available_skills {
            wire.insert("mergeAllAvailableSkills".into(), Value::Bool(v));
        }
        if let Some(v) = &self.extra_skill_dirs {
            wire.insert(
                "extraSkillDirs".into(),
                serde_json::to_value(v).unwrap_or(Value::Null),
            );
        }
        if let Some(v) = &self.extra_agent_dirs {
            wire.insert(
                "extraAgentDirs".into(),
                serde_json::to_value(v).unwrap_or(Value::Null),
            );
        }
        if let Some(v) = &self.disabled_skills {
            wire.insert(
                "disabledSkills".into(),
                serde_json::to_value(v).unwrap_or(Value::Null),
            );
        }
        if let Some(v) = &self.disabled_agents {
            wire.insert(
                "disabledAgents".into(),
                serde_json::to_value(v).unwrap_or(Value::Null),
            );
        }
        // The legacy `[background]` passthrough is emitted with camelCase
        // keys — the TS `BackgroundConfigSchema` spelling (the pre-B1 wire
        // leaked the file's snake_case spelling verbatim). Only the
        // TOP-LEVEL keys are converted: nested maps like `env` carry
        // environment variable names (case-sensitive — `FOO_BAR` must
        // stay `FOO_BAR`, not become `FOOBAR`), so the recursive
        // `convert_keys_to_camel_case` must not descend. Keys the file
        // already wrote camelCase stay untouched.
        if let Some(mut v) = self.background.as_ref().map(toml_to_json) {
            if let Value::Object(map) = &mut v {
                let renamed: serde_json::Map<String, Value> = std::mem::take(map)
                    .into_iter()
                    .map(|(key, entry)| (snake_to_camel(&key), entry))
                    .collect();
                *map = renamed;
            }
            wire.insert("background".into(), v);
        }
        if let Some(v) = self.task.as_ref().map(task_to_wire) {
            wire.insert("task".into(), v);
        }
        if let Some(v) = self.subagent.as_ref().map(subagent_to_wire) {
            wire.insert("subagent".into(), v);
        }
        if let Some(v) = self.interaction.as_ref().map(interaction_to_wire) {
            wire.insert("interaction".into(), v);
        }
        if let Some(v) = self.image.as_ref().map(toml_to_json) {
            wire.insert("image".into(), v);
        }
        if let Some(catalog) = &self.model_catalog {
            wire.insert(
                "modelCatalog".into(),
                serde_json::json!({ "persistCache": catalog.persist_cache }),
            );
        }
        if let Some(retention) = &self.retention {
            wire.insert("retention".into(), retention_to_wire(retention));
        }
        if let Some(v) = self.telemetry {
            wire.insert("telemetry".into(), Value::Bool(v));
        }
        if let Some(raw) = &self.raw {
            wire.insert("raw".into(), toml_to_json(raw));
        }
        Value::Object(wire)
    }

    /// Deep-merge a camelCase MirriConfig wire patch (the `config.set`
    /// payload) into `self`, in place.
    ///
    /// Mirrors the TypeScript `mergeConfigPatch` semantics:
    /// - `null` patch values are stripped first (JSON has no `undefined`; a
    ///   null field means "not provided", never "delete this key").
    /// - camelCase wire keys are converted to the snake_case TOML field
    ///   names before merging.
    /// - Plain-object values (e.g. `providers`, `models`, `thinking`) merge
    ///   recursively per key; scalars and arrays are replaced wholesale.
    /// - After merging, any model alias (re-)added by the patch whose model
    ///   id sits on its provider's `removed_model_ids` denylist clears that
    ///   provider's denylist (a later catalog refresh may then re-offer the
    ///   model), matching TypeScript `setMirriConfig`.
    ///
    /// A patch that would leave the config unserializable (e.g. a new model
    /// alias without `maxContextSize`) fails with `RequestInvalid`,
    /// mirroring TypeScript's zod validation of the merged result. The
    /// merged result must also satisfy [`LoopControlConfig::validate`] —
    /// e.g. a patch (or a patch merged onto keys already in the file)
    /// setting both `retentionTailRatio` and `retentionTailTokens` is
    /// rejected the same way, so a `config.set` can never persist a
    /// config.toml that the next `ServerConfig::load` would refuse.
    pub fn merge_wire_patch(&mut self, patch: &Value) -> EngineResult<()> {
        let mut patch = patch.clone();
        strip_nulls(&mut patch);
        convert_keys_to_snake_case(&mut patch);

        // Reject unknown top-level keys, matching TS MirriConfigPatchSchema.strict().
        if let Some(obj) = patch.as_object() {
            for key in obj.keys() {
                if !KNOWN_CONFIG_TOP_LEVEL_KEYS.contains(&key.as_str()) {
                    return Err(EngineError::RequestInvalid(format!(
                        "unknown config key '{key}'"
                    )));
                }
            }
        }

        // Merge the patch into a JSON mirror of self (so the camelCase⇄
        // snake_case conversion applies uniformly), then deserialize the
        // merged value back into the struct.
        let mut merged = serde_json::to_value(&*self).map_err(|err| {
            EngineError::RequestInvalid(format!("cannot serialize config for merge: {err}"))
        })?;
        deep_merge_json(&mut merged, &patch);
        let mut merged_config: ServerConfig = serde_json::from_value(merged).map_err(|err| {
            EngineError::RequestInvalid(format!("cannot apply config patch: {err}"))
        })?;
        clear_removed_model_ids_for_readded_aliases(&mut merged_config, &patch);
        // Hard validation (US6.2), same as `from_toml` enforces at load: the
        // merged config must not set mutually exclusive retention keys, or a
        // saved file would fail every subsequent boot. Validating the merged
        // result (not just the patch) also rejects a patch that adds one key
        // on top of the other already present in the merge base.
        if let Some(loop_control) = &merged_config.loop_control {
            loop_control
                .validate()
                .map_err(EngineError::RequestInvalid)?;
        }
        // Same for the typed `[task]` section (D-65): a `config.set` must
        // not persist bounds the next `ServerConfig::load` would refuse.
        if let Some(task) = &merged_config.task {
            task.validate().map_err(EngineError::RequestInvalid)?;
        }
        // And for the legacy `[background]` base's lifecycle fields (the
        // same reasoning as `from_toml`'s check: a saved file must not
        // fail every subsequent boot).
        if let Some(base) = merged_config.background_task_base() {
            base.validate().map_err(EngineError::RequestInvalid)?;
        }
        // Preserve the raw TOML document through the merge: `#[serde(skip)]`
        // means it is not part of the JSON round-trip, so we restore it
        // from the pre-merge config. After a merge the raw may be stale
        // (the merged values differ from the original file), but keeping it
        // ensures a subsequent `to_toml_string` still preserves unknown keys
        // from the original file.
        merged_config.raw = self.raw.take();
        *self = merged_config;
        Ok(())
    }
}
impl ServerConfig {
    /// The layered section record for the klient `configService` reads
    /// (`config.getAll` / `config.inspect`): every top-level section as a
    /// camelCase-keyed record.
    ///
    /// The base is the struct wire projection (env-applied, the closest rust
    /// analogue of the TS effective layer); keys present only in the raw
    /// document (sections the struct does not model, in their camelCase
    /// spelling) are overlaid so the record is the union of both views.
    /// The `raw` passthrough field itself is excluded.
    pub fn to_config_sections_wire(&self) -> Value {
        let mut wire = self.to_mirri_config_wire();
        let Some(sections) = wire.as_object_mut() else {
            return wire;
        };
        sections.remove("raw");
        if let Some(raw) = &self.raw {
            let raw_json = toml_to_json(raw);
            if let Some(raw_sections) = raw_json.as_object() {
                for (key, value) in raw_sections {
                    let camel = snake_to_camel(key);
                    if !sections.contains_key(&camel) {
                        sections.insert(camel, value.clone());
                    }
                }
            }
        }
        wire
    }

    /// The `models` registry as klient `modelService` records (camelCase
    /// keys, unknown per-entry keys preserved from the raw document).
    ///
    /// The base entries come from the struct wire projection (env-expanded
    /// known fields); per-entry keys present only in the raw document — the
    /// klient passthrough fields such as `providerId` / `baseUrl` — are
    /// overlaid so a `model.set` → `model.get` round trip keeps them.
    pub fn to_model_records_wire(&self) -> Value {
        self.records_with_raw_passthrough("models")
    }

    /// The `providers` registry as klient `providerService` records
    /// (camelCase keys, unknown per-entry keys preserved from the raw
    /// document). See [`to_model_records_wire`](Self::to_model_records_wire).
    pub fn to_provider_records_wire(&self) -> Value {
        self.records_with_raw_passthrough("providers")
    }

    /// Resolve a model id to its `models` alias key: an exact key match
    /// first, then the `${provider}/${model}` composite convention — see
    /// [`resolve_model_alias_in`].
    pub fn resolve_model_alias(&self, model_id: &str) -> Option<String> {
        resolve_model_alias_in(&self.to_model_records_wire(), model_id)
    }

    /// Shared record projection: the wire map for `section` with the raw
    /// document's per-entry passthrough keys overlaid.
    ///
    /// The raw entry keys are transformed like the TS readers: `models`
    /// entries convert recursively (`transformPlainObject`), `providers`
    /// entries convert their top-level keys plus the nested `oauth` object
    /// (`providerEntryFromToml` — `env` / `customHeaders` values keep their
    /// keys verbatim).
    fn records_with_raw_passthrough(&self, section: &str) -> Value {
        let wire = self.to_mirri_config_wire();
        let mut records = wire
            .get(section)
            .cloned()
            .unwrap_or_else(|| Value::Object(serde_json::Map::new()));
        let Some(raw_entries) = self
            .raw
            .as_ref()
            .and_then(|raw| raw.get(section))
            .map(toml_to_json)
            .and_then(|entries| entries.as_object().cloned())
        else {
            return records;
        };
        let Some(out) = records.as_object_mut() else {
            return records;
        };
        for (id, raw_entry) in raw_entries {
            let mut entry = raw_entry;
            if section == "models" {
                convert_keys_to_camel_case(&mut entry);
            } else if let Some(fields) = entry.as_object_mut() {
                let mut converted = serde_json::Map::new();
                for (key, mut value) in std::mem::take(fields) {
                    if key == "oauth" {
                        convert_keys_to_camel_case(&mut value);
                    }
                    converted.insert(snake_to_camel(&key), value);
                }
                entry = Value::Object(converted);
            }
            // Merge the raw passthrough keys over the wire entry: keys the
            // wire projection already carries (the typed, env-expanded
            // fields) win; unknown keys are preserved.
            if let (Some(dst), Some(src)) = (
                out.entry(id)
                    .or_insert_with(|| Value::Object(serde_json::Map::new()))
                    .as_object_mut(),
                entry.as_object(),
            ) {
                for (key, value) in src {
                    dst.entry(key.clone()).or_insert_with(|| value.clone());
                }
            }
        }
        records
    }
}

/// Resolve a model id against a `models` wire-records map: the exact alias
/// key first, then the `${provider}/${model}` composite convention.
///
/// The klient/web model pickers address models by the composite
/// `${provider}/${model}` id (the C6 id-composition ruling: `model.list`
/// rows carry no config key, so the UI composes one). The TS daemon's
/// registry serves those composites as real alias keys — the models.dev
/// import seeds `${providerId}/${model.id}` keys — so a pick that names
/// provider `rakuten` model `DeepSeek-V4-Flash` resolves even when the
/// config keeps that pair under another alias key (e.g. `fast`): the
/// composite matches the alias's own `provider` + `model` fields. Multiple
/// matches resolve deterministically to the lexicographically smallest
/// key. An id naming no configured alias — composite or not — resolves
/// to `None` (the TS catalog's `Model "X" is not configured` case).
pub fn resolve_model_alias_in(models: &Value, model_id: &str) -> Option<String> {
    let entries = models.as_object()?;
    if entries.contains_key(model_id) {
        return Some(model_id.to_string());
    }
    // Split at the FIRST '/' — the provider id, then the upstream model
    // (provider ids never carry '/'; an id with more segments names no
    // provider this registry knows).
    let (provider, model) = model_id.split_once('/')?;
    entries
        .iter()
        .filter(|(_, alias)| {
            alias.get("provider").and_then(Value::as_str) == Some(provider)
                && alias.get("model").and_then(Value::as_str) == Some(model)
        })
        .map(|(key, _)| key.as_str())
        .min()
        .map(str::to_string)
}

/// Redact provider api keys from a MirriConfig wire shape before returning
/// it to clients.
///
/// Walks the wire's `providers` map (the shape
/// [`ServerConfig::to_mirri_config_wire`] builds) and replaces every
/// provider entry's api key with `"[REDACTED]"`, leaving all other fields
/// untouched. The `raw` document (the original TOML, embedded verbatim for
/// round-trip fidelity) carries the same keys in their TOML spelling —
/// including the legacy singular `[provider]` table — so it is scrubbed
/// with the same replacement; otherwise the redaction would be trivially
/// bypassed through `raw`. Both RPC backends run `config.get`/`config.set`
/// responses through this before a config is exposed over the wire.
pub fn redact_config_wire(mut wire: Value) -> Value {
    if let Some(providers) = wire.get_mut("providers").and_then(|p| p.as_object_mut()) {
        for (_, provider) in providers.iter_mut() {
            redact_provider_api_key(provider);
        }
    }
    if let Some(raw) = wire.get_mut("raw") {
        if let Some(providers) = raw.get_mut("providers").and_then(|p| p.as_object_mut()) {
            for (_, provider) in providers.iter_mut() {
                redact_provider_api_key(provider);
            }
        }
        if let Some(provider) = raw.get_mut("provider") {
            redact_provider_api_key(provider);
        }
    }
    wire
}

/// Replace a provider-like object's api-key entry with `"[REDACTED]"`, in
/// both the wire (`apiKey`) and TOML (`api_key`) spellings; absent keys are
/// left absent so the wire shape never changes.
fn redact_provider_api_key(provider: &mut Value) {
    if let Some(p) = provider.as_object_mut() {
        for key in ["apiKey", "api_key"] {
            if p.contains_key(key) {
                p.insert(key.to_string(), Value::String("[REDACTED]".to_string()));
            }
        }
    }
}

/// Known top-level keys of [`ServerConfig`] in their TOML (snake_case) spelling.
///
/// Used by [`ServerConfig::to_toml_string`] to distinguish known struct fields
/// (whose absence from the serialized output means they were cleared) from
/// unknown keys (which should be preserved from `raw` for round-trip fidelity).
pub(crate) const KNOWN_CONFIG_TOP_LEVEL_KEYS: &[&str] = &[
    "model",
    "provider",
    "permission",
    "loop_control",
    "hooks",
    "thinking",
    "experimental",
    "providers",
    "models",
    "default_provider",
    "default_model",
    "plan_mode",
    "yolo",
    "default_permission_mode",
    "default_plan_mode",
    "services",
    "merge_all_available_skills",
    "extra_skill_dirs",
    "extra_agent_dirs",
    "disabled_skills",
    "disabled_agents",
    "background",
    "task",
    "subagent",
    "interaction",
    "image",
    "model_catalog",
    "retention",
    "telemetry",
];

/// Recursively remove `null` values from a JSON value.
///
/// JSON has no `undefined`; a `null` patch field means "not provided" (the
/// TypeScript client strips `undefined` before serializing), never "delete
/// this key". Stripping nulls keeps a patch such as `{ "thinking": null }`
/// from clobbering an existing `thinking` section.
fn strip_nulls(value: &mut Value) {
    match value {
        Value::Object(map) => {
            map.retain(|_, entry| !entry.is_null());
            for entry in map.values_mut() {
                strip_nulls(entry);
            }
        }
        Value::Array(items) => {
            for item in items {
                strip_nulls(item);
            }
        }
        _ => {}
    }
}

/// Convert a camelCase wire key to the snake_case TOML field name
/// (`"maxContextSize"` → `"max_context_size"`). Keys without uppercase
/// letters pass through unchanged.
pub fn camel_to_snake(key: &str) -> String {
    let mut snake = String::with_capacity(key.len() + 4);
    for (index, ch) in key.chars().enumerate() {
        if ch.is_uppercase() {
            if index > 0 {
                snake.push('_');
            }
            snake.push(ch.to_ascii_lowercase());
        } else {
            snake.push(ch);
        }
    }
    snake
}

/// Recursively convert every object key of a JSON value from camelCase to
/// snake_case. Map keys such as the user-chosen `providers`/`models` ids
/// carry no uppercase field spellings, so they pass through unchanged.
pub fn convert_keys_to_snake_case(value: &mut Value) {
    match value {
        Value::Object(map) => {
            let mut renamed = serde_json::Map::with_capacity(map.len());
            for (key, mut entry) in std::mem::take(map) {
                convert_keys_to_snake_case(&mut entry);
                renamed.insert(camel_to_snake(&key), entry);
            }
            *map = renamed;
        }
        Value::Array(items) => {
            for item in items {
                convert_keys_to_snake_case(item);
            }
        }
        _ => {}
    }
}

/// Deep-merge `patch` into `base`, mirroring the TypeScript `deepMerge`:
/// when both sides of a key are plain objects the merge recurses; for
/// scalars, arrays, and absent keys the patch value replaces/inserts.
fn deep_merge_json(base: &mut Value, patch: &Value) {
    match (&mut *base, patch) {
        (Value::Object(base_map), Value::Object(patch_map)) => {
            for (key, patch_value) in patch_map {
                match base_map.get_mut(key) {
                    Some(base_value) => deep_merge_json(base_value, patch_value),
                    None => {
                        base_map.insert(key.clone(), patch_value.clone());
                    }
                }
            }
        }
        _ => *base = patch.clone(),
    }
}

/// Clear a provider's `removed_model_ids` denylist when the patch (re-)adds
/// a model alias whose underlying model id is on it.
///
/// Mirrors TypeScript `setMirriConfig`: the alias's provider/model ids come
/// from the patch, falling back to the merged config for a partial alias.
/// The entire denylist is cleared (not just the offending id), matching TS.
fn clear_removed_model_ids_for_readded_aliases(config: &mut ServerConfig, patch: &Value) {
    let Some(models_patch) = patch.get("models").and_then(Value::as_object) else {
        return;
    };
    let Some(providers) = config.providers.as_mut() else {
        return;
    };
    for (alias_id, alias_patch) in models_patch {
        let Some(alias_patch) = alias_patch.as_object() else {
            continue;
        };
        let provider_id = alias_patch
            .get("provider")
            .and_then(Value::as_str)
            .or_else(|| {
                config
                    .models
                    .as_ref()
                    .and_then(|models| models.get(alias_id))
                    .map(|alias| alias.provider.as_str())
            });
        let Some(provider_id) = provider_id else {
            continue;
        };
        let Some(provider) = providers.get_mut(provider_id) else {
            continue;
        };
        let Some(removed_model_ids) = provider.removed_model_ids.as_ref() else {
            continue;
        };
        let model_id = alias_patch
            .get("model")
            .and_then(Value::as_str)
            .or_else(|| {
                config
                    .models
                    .as_ref()
                    .and_then(|models| models.get(alias_id))
                    .map(|alias| alias.model.as_str())
            });
        let Some(model_id) = model_id else {
            continue;
        };
        if removed_model_ids.iter().any(|removed| removed == model_id) {
            provider.removed_model_ids = None;
        }
    }
}

/// Serialize an explicit `providers` map with camelCase wire keys.
///
/// Absent fields within each entry are omitted (not `null`) so the result
/// passes TypeScript zod `.optional()` validation.
fn providers_to_wire(providers: &HashMap<String, ProviderEntry>) -> Value {
    let mut map = serde_json::Map::new();
    for (provider_id, entry) in providers {
        let mut entry_map = serde_json::Map::new();
        if let Some(t) = &entry.r#type {
            entry_map.insert("type".into(), Value::String(t.clone()));
        }
        if let Some(v) = &entry.api_key {
            entry_map.insert("apiKey".into(), Value::String(v.clone()));
        }
        if let Some(v) = &entry.base_url {
            entry_map.insert("baseUrl".into(), Value::String(v.clone()));
        }
        if let Some(v) = &entry.default_model {
            entry_map.insert("defaultModel".into(), Value::String(v.clone()));
        }
        if let Some(v) = &entry.removed_model_ids {
            entry_map.insert(
                "removedModelIds".into(),
                serde_json::to_value(v).unwrap_or(Value::Null),
            );
        }
        if let Some(v) = &entry.model_source {
            entry_map.insert("modelSource".into(), Value::String(v.clone()));
        }
        if let Some(v) = &entry.oauth {
            entry_map.insert("oauth".into(), v.clone());
        }
        if let Some(v) = &entry.env {
            entry_map.insert("env".into(), serde_json::to_value(v).unwrap_or(Value::Null));
        }
        if let Some(v) = &entry.custom_headers {
            entry_map.insert(
                "customHeaders".into(),
                serde_json::to_value(v).unwrap_or(Value::Null),
            );
        }
        if let Some(v) = &entry.media_file_refs {
            entry_map.insert("mediaFileRefs".into(), Value::Bool(*v));
        }
        if let Some(v) = &entry.source {
            entry_map.insert("source".into(), v.clone());
        }
        map.insert(provider_id.clone(), Value::Object(entry_map));
    }
    Value::Object(map)
}

/// Serialize an explicit `models` map with camelCase wire keys.
///
/// Absent fields within each alias are omitted (not `null`).
fn models_to_wire(models: &HashMap<String, ModelAlias>) -> Value {
    let mut map = serde_json::Map::new();
    for (alias_id, alias) in models {
        let mut entry = serde_json::Map::new();
        entry.insert("provider".into(), Value::String(alias.provider.clone()));
        entry.insert("model".into(), Value::String(alias.model.clone()));
        entry.insert(
            "maxContextSize".into(),
            Value::Number(serde_json::Number::from(alias.max_context_size)),
        );
        if let Some(v) = alias.max_output_size {
            entry.insert(
                "maxOutputSize".into(),
                Value::Number(serde_json::Number::from(v)),
            );
        }
        if let Some(v) = &alias.capabilities {
            entry.insert(
                "capabilities".into(),
                serde_json::to_value(v).unwrap_or(Value::Null),
            );
        }
        if let Some(v) = &alias.display_name {
            entry.insert("displayName".into(), Value::String(v.clone()));
        }
        if let Some(v) = &alias.description {
            entry.insert("description".into(), Value::String(v.clone()));
        }
        if let Some(v) = &alias.reasoning_key {
            entry.insert("reasoningKey".into(), Value::String(v.clone()));
        }
        if let Some(v) = alias.adaptive_thinking {
            entry.insert("adaptiveThinking".into(), Value::Bool(v));
        }
        if let Some(v) = &alias.support_efforts {
            entry.insert(
                "supportEfforts".into(),
                serde_json::to_value(v).unwrap_or(Value::Null),
            );
        }
        if let Some(v) = &alias.default_effort {
            entry.insert("defaultEffort".into(), Value::String(v.clone()));
        }
        if let Some(v) = &alias.protocol {
            entry.insert("protocol".into(), Value::String(v.clone()));
        }
        if let Some(v) = alias.beta_api {
            entry.insert("betaApi".into(), Value::Bool(v));
        }
        map.insert(alias_id.clone(), Value::Object(entry));
    }
    Value::Object(map)
}

/// Serialize a [`ThinkingConfig`] with the wire's `{enabled, effort, keep}`
/// keys. Absent fields are omitted.
fn thinking_to_wire(thinking: &ThinkingConfig) -> Value {
    let mut map = serde_json::Map::new();
    if let Some(v) = thinking.enabled {
        map.insert("enabled".into(), Value::Bool(v));
    }
    if let Some(v) = &thinking.effort {
        map.insert("effort".into(), Value::String(v.clone()));
    }
    if let Some(v) = &thinking.keep {
        map.insert("keep".into(), Value::String(v.clone()));
    }
    Value::Object(map)
}

/// Serialize one [`PermissionRule`] into the klient wire row shape:
/// lowercase `decision`, kebab-case `scope`, `pattern`, and `reason`
/// omitted when `None` (zod `.optional()` rejects `null`).
pub fn permission_rule_to_wire(rule: &crate::permission::PermissionRule) -> Value {
    let mut r = serde_json::Map::new();
    r.insert(
        "decision".into(),
        serde_json::to_value(rule.decision).unwrap_or(Value::Null),
    );
    r.insert(
        "scope".into(),
        serde_json::to_value(rule.scope).unwrap_or(Value::Null),
    );
    r.insert("pattern".into(), Value::String(rule.pattern.clone()));
    if let Some(reason) = &rule.reason {
        r.insert("reason".into(), Value::String(reason.clone()));
    }
    Value::Object(r)
}

/// Serialize a [`PermissionConfig`] with the wire's `{rules: [...]}` shape.
///
/// Each rule's fields use camelCase wire names; `reason` is omitted when
/// `None` (zod `.optional()` rejects `null`).
fn permission_to_wire(permission: &PermissionConfig) -> Value {
    let rules: Vec<Value> = permission
        .rules
        .as_ref()
        .map(|rules| rules.iter().map(permission_rule_to_wire).collect())
        .unwrap_or_default();
    serde_json::json!({ "rules": rules })
}

/// Serialize a single [`HookDef`](crate::hook_engine::HookDef) with the
/// TypeScript wire field names.
///
/// Maps Rust field names to TS wire names:
/// - `pattern` → `matcher`
/// - `timeout_ms` (milliseconds) → `timeout` (seconds)
/// - `fail_closed` → `failClosed`
/// - `cwd` is carried verbatim (same name on both sides)
/// - `env` is omitted (not in the TS wire schema)
fn hook_to_wire(hook: &crate::hook_engine::HookDef) -> Value {
    let mut map = serde_json::Map::new();
    map.insert(
        "event".into(),
        serde_json::to_value(&hook.event).unwrap_or(Value::Null),
    );
    map.insert("command".into(), Value::String(hook.command.clone()));
    if let Some(pattern) = &hook.pattern {
        map.insert("matcher".into(), Value::String(pattern.clone()));
    }
    // Convert milliseconds → seconds for the TS wire shape.
    let timeout_secs = (hook.timeout_ms / 1000).min(600);
    map.insert(
        "timeout".into(),
        Value::Number(serde_json::Number::from(timeout_secs)),
    );
    if let Some(cwd) = &hook.cwd {
        map.insert("cwd".into(), Value::String(cwd.clone()));
    }
    if hook.fail_closed {
        map.insert("failClosed".into(), Value::Bool(true));
    }
    Value::Object(map)
}

/// Serialize a typed [`TaskConfig`] with the wire's camelCase keys (the
/// TS `AgentTaskConfigSchema` spellings). Absent fields are omitted.
fn task_to_wire(task: &TaskConfig) -> Value {
    let mut map = serde_json::Map::new();
    if let Some(v) = task.max_running_tasks {
        map.insert(
            "maxRunningTasks".into(),
            Value::Number(serde_json::Number::from(v)),
        );
    }
    if let Some(v) = task.keep_alive_on_exit {
        map.insert("keepAliveOnExit".into(), Value::Bool(v));
    }
    if let Some(v) = task.bash_auto_background_on_timeout {
        map.insert("bashAutoBackgroundOnTimeout".into(), Value::Bool(v));
    }
    if let Some(v) = task.bash_task_timeout_s {
        map.insert(
            "bashTaskTimeoutS".into(),
            Value::Number(serde_json::Number::from(v)),
        );
    }
    if let Some(v) = task.kill_grace_period_ms {
        map.insert(
            "killGracePeriodMs".into(),
            Value::Number(serde_json::Number::from(v)),
        );
    }
    if let Some(v) = task.print_wait_ceiling_s {
        map.insert(
            "printWaitCeilingS".into(),
            Value::Number(serde_json::Number::from(v)),
        );
    }
    if let Some(v) = &task.print_background_mode {
        map.insert(
            "printBackgroundMode".into(),
            Value::String(
                match v {
                    PrintBackgroundMode::Exit => "exit",
                    PrintBackgroundMode::Drain => "drain",
                    PrintBackgroundMode::Steer => "steer",
                }
                .to_string(),
            ),
        );
    }
    if let Some(v) = task.print_max_turns {
        map.insert(
            "printMaxTurns".into(),
            Value::Number(serde_json::Number::from(v)),
        );
    }
    Value::Object(map)
}

/// Serialize a [`RetentionConfig`] with the wire's camelCase keys (the
/// TS `snakeToCamel` transform's spelling; the wire emits the section
/// only when the config file carries one). Absent fields keep their serde
/// defaults, which the sanitizer reconciles per-item at resolve time.
fn retention_to_wire(retention: &RetentionConfig) -> Value {
    serde_json::json!({
        "enabled": retention.enabled,
        "archiveGraceDays": retention.archive_grace_days,
        "intervalHours": retention.interval_hours,
    })
}

/// Serialize a typed [`SubagentConfig`] with the wire's camelCase key.
fn subagent_to_wire(subagent: &SubagentConfig) -> Value {
    match subagent.timeout_ms {
        Some(ms) => serde_json::json!({ "timeoutMs": ms }),
        None => serde_json::json!({}),
    }
}

/// Serialize a typed [`InteractionConfig`] with the wire's camelCase key
/// (ADR-008 / D-4). Absent fields are omitted — the TS schema treats the
/// whole section as optional.
fn interaction_to_wire(interaction: &InteractionConfig) -> Value {
    match interaction.question_channel_wait_secs {
        Some(secs) => serde_json::json!({ "questionChannelWaitSecs": secs }),
        None => serde_json::json!({}),
    }
}

/// Serialize a [`LoopControlConfig`] with the wire's camelCase keys.
/// Absent fields are omitted.
fn loop_control_to_wire(loop_control: &LoopControlConfig) -> Value {
    let mut map = serde_json::Map::new();
    if let Some(v) = loop_control.max_steps_per_turn {
        map.insert(
            "maxStepsPerTurn".into(),
            Value::Number(serde_json::Number::from(v)),
        );
    }
    if let Some(v) = loop_control.max_retries_per_step {
        map.insert(
            "maxRetriesPerStep".into(),
            Value::Number(serde_json::Number::from(v)),
        );
    }
    if let Some(v) = loop_control.max_ralph_iterations {
        map.insert(
            "maxRalphIterations".into(),
            Value::Number(serde_json::Number::from(v)),
        );
    }
    if let Some(v) = loop_control.reserved_context_size {
        map.insert(
            "reservedContextSize".into(),
            Value::Number(serde_json::Number::from(v)),
        );
    }
    if let Some(v) = loop_control.compaction_trigger_ratio {
        map.insert(
            "compactionTriggerRatio".into(),
            serde_json::to_value(v).unwrap_or(Value::Null),
        );
    }
    if let Some(v) = loop_control.retention_tail_ratio {
        map.insert(
            "retentionTailRatio".into(),
            serde_json::to_value(v).unwrap_or(Value::Null),
        );
    }
    if let Some(v) = loop_control.retention_tail_tokens {
        map.insert(
            "retentionTailTokens".into(),
            Value::Number(serde_json::Number::from(v)),
        );
    }
    if let Some(v) = loop_control.max_overflow_compaction_attempts {
        map.insert(
            "maxOverflowCompactionAttempts".into(),
            Value::Number(serde_json::Number::from(v)),
        );
    }
    if let Some(v) = loop_control.max_history_messages {
        map.insert(
            "maxHistoryMessages".into(),
            Value::Number(serde_json::Number::from(v)),
        );
    }
    if let Some(v) = loop_control.max_deferred_messages {
        map.insert(
            "maxDeferredMessages".into(),
            Value::Number(serde_json::Number::from(v)),
        );
    }
    if let Some(v) = loop_control.turn_stall_timeout_ms {
        map.insert(
            "turnStallTimeoutMs".into(),
            Value::Number(serde_json::Number::from(v)),
        );
    }
    Value::Object(map)
}

/// Convert a TOML value (from a passthrough section) into JSON.
///
/// Keys keep the spelling used in the config file; the B5-b
/// modelService/providerService/configService record readers rename them to
/// the klient camelCase spellings via [`convert_keys_to_camel_case`].
pub fn toml_to_json(value: &toml::Value) -> Value {
    serde_json::to_value(value).unwrap_or(Value::Null)
}

/// Convert a snake_case TOML field name to the camelCase wire spelling
/// (`"max_context_size"` → `"maxContextSize"`) — the inverse of
/// [`camel_to_snake`]. Keys without underscores pass through unchanged.
pub fn snake_to_camel(key: &str) -> String {
    let mut camel = String::with_capacity(key.len());
    let mut upper_next = false;
    for ch in key.chars() {
        if ch == '_' {
            upper_next = true;
        } else if upper_next {
            camel.extend(ch.to_uppercase());
            upper_next = false;
        } else {
            camel.push(ch);
        }
    }
    camel
}

/// Recursively convert every object key of a JSON value from snake_case to
/// camelCase (the inverse of [`convert_keys_to_snake_case`]). Map keys such
/// as the user-chosen `providers`/`models` ids carry no underscores, so they
/// pass through unchanged.
pub fn convert_keys_to_camel_case(value: &mut Value) {
    match value {
        Value::Object(map) => {
            let mut renamed = serde_json::Map::with_capacity(map.len());
            for (key, mut entry) in std::mem::take(map) {
                convert_keys_to_camel_case(&mut entry);
                renamed.insert(snake_to_camel(&key), entry);
            }
            *map = renamed;
        }
        Value::Array(items) => {
            for item in items {
                convert_keys_to_camel_case(item);
            }
        }
        _ => {}
    }
}

/// Convert a JSON (camelCase or snake_case keys) value into a TOML value.
///
/// `null` values are stripped first (TOML has no null; a null field means
/// "not provided", never "delete this key"). Non-representable shapes
/// (e.g. mixed-key maps) surface as `RequestInvalid`.
pub fn json_to_toml_value(value: &Value) -> EngineResult<toml::Value> {
    let mut stripped = value.clone();
    strip_nulls(&mut stripped);
    serde_json::from_value(stripped)
        .map_err(|err| EngineError::RequestInvalid(format!("cannot convert value to TOML: {err}")))
}

/// Per-entry keys of [`ProviderEntry`] in their TOML (snake_case) spelling —
/// the keys a struct-path serialization owns inside `[providers.<id>]`.
pub(crate) const KNOWN_PROVIDER_ENTRY_KEYS: &[&str] = &[
    "type",
    "api_key",
    "base_url",
    "default_model",
    "removed_model_ids",
    "model_source",
    "oauth",
    "env",
    "custom_headers",
    "media_file_refs",
    "source",
];

/// camelCase serde aliases of [`ProviderEntry`] fields (see
/// `config/mod.rs`) — the TypeScript wire spellings the struct also accepts
/// on deserialization.
///
/// Kept separate from [`KNOWN_PROVIDER_ENTRY_KEYS`] on purpose: entries in
/// the base list are struct-owned during round-trip writes (see
/// [`merge_map_entry_passthrough`]), while alias-spelled keys must stay
/// passthrough-preserved from `raw`. Only the config doctor's unknown-key
/// scan consumes this supplement, never the write-back merge.
pub(crate) const KNOWN_PROVIDER_ENTRY_ALIAS_KEYS: &[&str] = &[
    "baseUrl",
    "defaultModel",
    "removedModelIds",
    "modelSource",
    "customHeaders",
    "mediaFileRefs",
];

/// Per-entry keys of [`ModelAlias`] in their TOML (snake_case) spelling —
/// the keys a struct-path serialization owns inside `[models.<id>]`.
pub(crate) const KNOWN_MODEL_ENTRY_KEYS: &[&str] = &[
    "provider",
    "model",
    "max_context_size",
    "max_output_size",
    "capabilities",
    "display_name",
    "description",
    "reasoning_key",
    "adaptive_thinking",
    "support_efforts",
    "default_effort",
    "protocol",
    "beta_api",
];

/// camelCase serde aliases of [`ModelAlias`] fields (see `config/mod.rs`) —
/// the TypeScript wire spellings the struct also accepts on deserialization.
///
/// Kept separate from [`KNOWN_MODEL_ENTRY_KEYS`] on purpose: entries in the
/// base list are struct-owned during round-trip writes (see
/// [`merge_map_entry_passthrough`]), while alias-spelled keys must stay
/// passthrough-preserved from `raw`. Only the config doctor's unknown-key
/// scan consumes this supplement, never the write-back merge.
pub(crate) const KNOWN_MODEL_ENTRY_ALIAS_KEYS: &[&str] = &[
    "maxOutputSize",
    "displayName",
    "reasoningKey",
    "adaptiveThinking",
    "supportEfforts",
    "defaultEffort",
    "betaApi",
];

/// Per-entry keys of a `[[hooks]]` entry in their TOML spellings — both the
/// snake_case native names and the TypeScript-compatible aliases accepted by
/// `HookDefInput` (see `hook_engine/types.rs`). Consumed by the doctor's
/// unknown-key detection.
pub(crate) const KNOWN_HOOK_ENTRY_KEYS: &[&str] = &[
    "event",
    "command",
    "pattern",
    "matcher",
    "timeout_ms",
    "timeout",
    "fail_closed",
    "failClosed",
    "env",
    "cwd",
];

/// Per-entry keys of a `[[permission.rules]]` entry in their TOML spellings
/// (the `permission.rs` `PermissionRule` serde shape). Consumed by the
/// doctor's unknown-key detection.
pub(crate) const KNOWN_PERMISSION_RULE_KEYS: &[&str] = &["decision", "scope", "pattern", "reason"];

/// Known keys inside the typed top-level sections, in their TOML spellings —
/// both snake_case field names and camelCase serde aliases. Sections keyed by
/// map (`providers`, `models`) and arrays (`hooks`, `permission.rules`) are
/// handled by their per-entry lists above; this table covers the scalar /
/// struct-shaped sections. Consumed by the doctor's unknown-key detection.
pub(crate) const KNOWN_SECTION_KEYS: &[(&str, &[&str])] = &[
    ("thinking", &["enabled", "effort", "keep"]),
    (
        "loop_control",
        &[
            "max_steps_per_turn",
            "max_retries_per_step",
            "max_ralph_iterations",
            "maxRalphIterations",
            "reserved_context_size",
            "reservedContextSize",
            "compaction_trigger_ratio",
            "compactionTriggerRatio",
            "max_overflow_compaction_attempts",
            "maxOverflowCompactionAttempts",
            "max_history_messages",
            "maxHistoryMessages",
            "max_deferred_messages",
            "maxDeferredMessages",
            "retention_tail_ratio",
            "retentionTailRatio",
            "retention_tail_tokens",
            "retentionTailTokens",
            "turn_stall_timeout_ms",
            "turnStallTimeoutMs",
        ],
    ),
    (
        "task",
        &[
            "max_running_tasks",
            "maxRunningTasks",
            "keep_alive_on_exit",
            "keepAliveOnExit",
            "bash_auto_background_on_timeout",
            "bashAutoBackgroundOnTimeout",
            "bash_task_timeout_s",
            "bashTaskTimeoutS",
            "kill_grace_period_ms",
            "killGracePeriodMs",
            "print_wait_ceiling_s",
            "printWaitCeilingS",
            "print_background_mode",
            "printBackgroundMode",
            "print_max_turns",
            "printMaxTurns",
            "heartbeat_interval_ms",
            "heartbeatIntervalMs",
            "orphan_dispose_policy",
            "orphanDisposePolicy",
            "max_task_lifetime_s",
            "maxTaskLifetimeS",
        ],
    ),
    (
        "subagent",
        &[
            "timeout_ms",
            "timeoutMs",
            "max_nesting_depth",
            "maxNestingDepth",
            "max_swarm_items",
            "maxSwarmItems",
        ],
    ),
    (
        "interaction",
        &["question_channel_wait_secs", "questionChannelWaitSecs"],
    ),
    ("model_catalog", &["persist_cache", "persistCache"]),
    (
        "retention",
        &[
            "enabled",
            "archive_grace_days",
            "archiveGraceDays",
            "interval_hours",
            "intervalHours",
        ],
    ),
];

/// Merge per-entry unknown keys from the raw document's `section` map into
/// the serialized struct's `section` map (see [`ServerConfig::to_toml_string`]).
///
/// Known entry keys are owned by the serialized struct (its value wins, and
/// its absence means the typed entry no longer has the field); keys the
/// typed entries do not model are preserved verbatim so a save/load cycle
/// never drops klient passthrough fields. Entries absent from the serialized
/// map were deleted, so nothing is merged back for them.
pub(crate) fn merge_map_entry_passthrough(
    serialized: &mut toml::value::Table,
    raw: &toml::value::Table,
    section: &str,
    known_entry_keys: &[&str],
) {
    let Some(raw_entries) = raw.get(section).and_then(toml::Value::as_table) else {
        return;
    };
    let Some(out_entries) = serialized
        .get_mut(section)
        .and_then(toml::Value::as_table_mut)
    else {
        return;
    };
    for (entry_id, raw_entry) in raw_entries {
        let Some(raw_fields) = raw_entry.as_table() else {
            continue;
        };
        let Some(out_fields) = out_entries
            .get_mut(entry_id)
            .and_then(toml::Value::as_table_mut)
        else {
            // Deleted (or never struct-modeled) entry — nothing to merge into.
            continue;
        };
        for (field, value) in raw_fields {
            if !known_entry_keys.contains(&field.as_str()) && !out_fields.contains_key(field) {
                out_fields.insert(field.clone(), value.clone());
            }
        }
    }
}

#[cfg(test)]
mod resolve_model_alias_tests {
    use serde_json::json;

    use super::resolve_model_alias_in;

    /// The registry of the reported bug: a `fast` alias naming rakuten's
    /// DeepSeek-V4-Flash next to the copilot composites.
    fn registry() -> serde_json::Value {
        json!({
            "fast": { "provider": "rakuten", "model": "DeepSeek-V4-Flash" },
            "github-copilot/claude-opus-5": {
                "provider": "github-copilot",
                "model": "claude-opus-5"
            }
        })
    }

    #[test]
    fn should_resolve_an_exact_alias_key_verbatim() {
        // Given a registry carrying the copilot composite as a real key
        // When an exact-key id is resolved
        // Then the key itself is the answer
        assert_eq!(
            resolve_model_alias_in(&registry(), "github-copilot/claude-opus-5"),
            Some("github-copilot/claude-opus-5".to_string())
        );
        assert_eq!(
            resolve_model_alias_in(&registry(), "fast"),
            Some("fast".to_string())
        );
    }

    #[test]
    fn should_resolve_a_composite_to_the_alias_naming_the_same_pair() {
        // Given a registry whose `fast` alias names rakuten's
        // DeepSeek-V4-Flash (the web picker composes exactly that pair)
        // When the composite id is resolved
        // Then it resolves to the `fast` alias key
        assert_eq!(
            resolve_model_alias_in(&registry(), "rakuten/DeepSeek-V4-Flash"),
            Some("fast".to_string())
        );
    }

    #[test]
    fn should_resolve_multiple_matches_to_the_smallest_key_deterministically() {
        // Given a registry where two aliases name the same provider + model
        let models = json!({
            "beta": { "provider": "rakuten", "model": "GLM" },
            "alpha": { "provider": "rakuten", "model": "GLM" }
        });

        // When the composite id is resolved
        // Then the lexicographically smallest matching key answers
        assert_eq!(
            resolve_model_alias_in(&models, "rakuten/GLM"),
            Some("alpha".to_string())
        );
    }

    #[test]
    fn should_resolve_none_for_an_unmatched_composite_or_a_foreign_model_name() {
        // Given a registry whose pairs do not include rakuten's Ghost
        // When an unmatched composite (or a model name of some other
        // provider, or a non-map registry) is resolved
        // Then no alias answers
        assert_eq!(resolve_model_alias_in(&registry(), "rakuten/Ghost-9"), None);
        assert_eq!(resolve_model_alias_in(&registry(), "glm-5.3-flash"), None);
        assert_eq!(resolve_model_alias_in(&json!(null), "fast"), None);
    }
}
