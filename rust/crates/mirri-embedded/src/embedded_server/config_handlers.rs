//! Config-domain dispatch handlers for the embedded
//! [`EmbeddedServer`](super::EmbeddedServer) — the B5-b klient
//! `configService` section methods: `config.getAll` / `config.inspect` /
//! `config.replace` / `config.replaceSections` / `config.reload`.
//!
//! These sit next to (and deliberately reuse) the Settled `config.get` /
//! `config.set` machinery: reads project the in-memory [`ServerConfig`]
//! snapshot (the env-applied struct wire union the raw document's unmodeled
//! sections), and writes go through the raw TOML document — load without
//! env, mutate, validate by re-parsing, persist atomically, reload with
//! env — the same write protocol `config.set` uses, so env-injected values
//! are never baked into config.toml.
//!
//! Layering DRIFT vs the TS `agent-core-v2` `ConfigService` (recorded in the
//! TSV rows): the rust engine has no section registry (no `defaultValue`
//! layer, no zod re-validation) and no per-run memory layer, so
//! `config.inspect` answers only the `value` / `userValue` layers and the
//! `memory` target is honestly rejected with invalid params — never
//! fabricated. Section values are redacted with the same `apiKey` →
//! `[REDACTED]` rule the Settled `config.get` wire applies (the domain
//! reference), so a secret never rides the section reads.

use mirri_core::ServerConfig;
use mirri_transport::adapter::EngineError;
use serde_json::Value;

use super::{EmbeddedServer, engine_error, invalid_params, normalize_params};

/// Parse the optional klient config `target` argument (`'user'` /
/// `'memory'`). The rust engine has no memory layer, so only the `user`
/// spelling is accepted — the memory layer is honestly rejected, never
/// fabricated.
fn parse_target(target: Option<&str>) -> Result<(), EngineError> {
    match target {
        None | Some("user") => Ok(()),
        Some("memory") => Err(invalid_params(
            "the memory config target is not supported by the rust engine (no per-run memory layer)",
        )),
        Some(other) => Err(invalid_params(format!(
            "target must be \"user\" or \"memory\", got {other:?}"
        ))),
    }
}

impl EmbeddedServer {
    /// `config.getAll` — every top-level config section as a camelCase
    /// record (the klient `configService.getAll` contract; the layered
    /// union of the env-applied wire projection and the raw document's
    /// unmodeled sections).
    pub(super) async fn dispatch_config_get_all(&self) -> Result<Value, EngineError> {
        let config = self.config.read().unwrap_or_else(|e| e.into_inner());
        Ok(redact_api_keys(config.to_config_sections_wire()))
    }

    /// `config.inspect(key)` — the layered snapshot for one section:
    /// `value` and `userValue` both carry the current effective value
    /// (env overrides already applied). The engine does not separate the
    /// file-backed layer from the effective one, so `userValue` is not
    /// the pre-env file value when overrides are active. `defaultValue`
    /// and `memoryValue` are honestly absent (no section registry / no
    /// memory layer — the TSV note records the DRIFT).
    pub(super) async fn dispatch_config_inspect(
        &self,
        params: Value,
    ) -> Result<Value, EngineError> {
        let key = first_string_arg(&params, &["key", "domain"])?;
        let config = self.config.read().unwrap_or_else(|e| e.into_inner());
        let sections = redact_api_keys(config.to_config_sections_wire());
        let mut snapshot = serde_json::Map::new();
        if let Some(value) = sections.get(&key) {
            snapshot.insert("value".to_string(), value.clone());
            snapshot.insert("userValue".to_string(), value.clone());
        }
        Ok(Value::Object(snapshot))
    }

    /// `config.replace(key, value, target?)` — replace (or clear, when the
    /// value is JSON `null`) one top-level section, then persist and reload
    /// (the same write protocol `config.set` uses). noResult on the wire.
    pub(super) async fn dispatch_config_replace(
        &self,
        params: Value,
    ) -> Result<Value, EngineError> {
        let (key, value, target) = triple_args(&params, &["key", "domain"])?;
        parse_target(target.as_deref())?;
        self.replace_sections_raw(&[(key, value_filter_null(&value))])
    }

    /// `config.replaceSections(record, target?)` — batch section replace /
    /// clear, then persist and reload. noResult on the wire.
    pub(super) async fn dispatch_config_replace_sections(
        &self,
        params: Value,
    ) -> Result<Value, EngineError> {
        let (record, target) = pair_args_record(&params)?;
        parse_target(target.as_deref())?;
        let Some(sections) = record.as_object() else {
            return Err(invalid_params(
                "config.replaceSections requires a record of sections",
            ));
        };
        if sections.is_empty() {
            // Matching the TS service: an empty record is a no-op.
            return Ok(Value::Null);
        }
        let replacements: Vec<(String, Option<Value>)> = sections
            .iter()
            .map(|(key, value)| (key.clone(), value_filter_null(value)))
            .collect();
        self.replace_sections_raw(&replacements)
    }

    /// `config.reload` — re-read config.toml from disk (env applied) into
    /// the in-memory snapshot. noResult on the wire.
    pub(super) async fn dispatch_config_reload(&self) -> Result<Value, EngineError> {
        let config_path = self.config_path.clone().ok_or_else(|| {
            invalid_params("config.reload requires a config file path; the server has none")
        })?;
        // B8: the pre-reload in-memory snapshot (env-applied) — the tail
        // hook compares it against the reloaded view so a hand-edited
        // config.toml that touches the skill-derived keys resyncs the
        // built catalogs too.
        let before = self
            .config
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let reloaded =
            ServerConfig::load(&config_path, &mirri_os::LocalSyncFs).map_err(engine_error)?;
        *self.config.write().unwrap_or_else(|e| e.into_inner()) = reloaded;
        self.resync_skill_catalogs_if_changed(&before);
        Ok(Value::Null)
    }

    /// `config.diagnostics` — the advisory config warnings (the klient
    /// `ConfigDiagnostics` contract: a bare `{warnings: string[]}`).
    ///
    /// The consistency checks over the multi-provider sections come from
    /// [`ServerConfig::diagnostics_warnings`] (dangling `default_provider`,
    /// a model alias over a missing provider, the legacy `[model]` section
    /// migration hint, the retired `disabled_agents` key's D15 deprecation
    /// notice); the models.dev refresh appends its degradation
    /// warning when the last refresh failed and the cached directory is
    /// being served (see `provider_handlers`).
    pub(super) fn dispatch_config_diagnostics(&self) -> Result<Value, EngineError> {
        let config = self.config.read().unwrap_or_else(|e| e.into_inner());
        let mut warnings = config.diagnostics_warnings();
        if let Some(warning) = self
            .models_dev_warning
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        {
            warnings.push(warning);
        }
        Ok(
            serde_json::to_value(super::responses::ConfigDiagnosticsResult { warnings })
                .expect("the diagnostics result is always serializable"),
        )
    }

    // --- shared write pipeline (also used by the model/provider record
    // handlers) ----------------------------------------------------------

    /// Run one raw-document mutation through the standard
    /// load-without-env → mutate → persist → reload cycle.
    ///
    /// The mutation itself validates (a change that would leave the config
    /// unparseable fails inside `mutate` before anything touches the
    /// disk); write failures surface as the engine error carrying the
    /// cause, never a panic.
    pub(super) fn mutate_config_raw(
        &self,
        mutate: impl FnOnce(&mut ServerConfig) -> Result<(), EngineError>,
    ) -> Result<(), EngineError> {
        let config_path = self.config_path.clone().ok_or_else(|| {
            invalid_params("config writes require a config file path; the server has none")
        })?;
        // B8: the pre-write IN-MEMORY snapshot (env-applied) — the tail hook
        // compares it against the reloaded view on the same env-expanded
        // plane, so `${VAR}` entries in the raw document cannot produce a
        // spurious every-write resync.
        let before = self
            .config
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let mut config = ServerConfig::load_without_env(&config_path, &mirri_os::LocalSyncFs)
            .map_err(engine_error)?;
        mutate(&mut config)?;
        config.save_raw_to(&config_path).map_err(engine_error)?;
        let reloaded =
            ServerConfig::load(&config_path, &mirri_os::LocalSyncFs).map_err(engine_error)?;
        *self.config.write().unwrap_or_else(|e| e.into_inner()) = reloaded;
        self.resync_skill_catalogs_if_changed(&before);
        Ok(())
    }

    /// Replace/clear top-level sections in the raw document and persist.
    pub(super) fn replace_sections_raw(
        &self,
        sections: &[(String, Option<Value>)],
    ) -> Result<Value, EngineError> {
        let replacements: Vec<(String, Option<Value>)> = sections
            .iter()
            .map(|(key, value)| {
                let key = mirri_core::camel_to_snake(key);
                let value = value.as_ref().map(|v| {
                    let mut v = v.clone();
                    mirri_core::convert_keys_to_snake_case(&mut v);
                    v
                });
                (key, value)
            })
            .collect();
        for (key, value) in &replacements {
            if key.is_empty() {
                return Err(invalid_params("config section key must be non-empty"));
            }
            if let Some(value) = value
                && json_carries_null(value)
            {
                return Err(invalid_params(format!(
                    "config section '{key}' carries a null member (TOML has no null)"
                )));
            }
        }
        self.mutate_config_raw(|config| {
            config
                .replace_raw_sections(&replacements)
                .map_err(engine_error)
        })?;
        Ok(Value::Null)
    }
}

/// The klient `null`-means-clear encoding: a JSON `null` value clears the
/// section; anything else is the replacement value.
fn value_filter_null(value: &Value) -> Option<Value> {
    if value.is_null() {
        None
    } else {
        Some(value.clone())
    }
}

/// Whether the value still carries `null` members — TOML has no null, and
/// silently stripping them would lie about the write (the top-level `null`
/// already carries the clear meaning).
fn json_carries_null(value: &Value) -> bool {
    match value {
        Value::Object(map) => map.values().any(|v| v.is_null() || json_carries_null(v)),
        Value::Array(items) => items.iter().any(json_carries_null),
        _ => false,
    }
}

/// Recursively replace every `apiKey` / `api_key` member with `[REDACTED]`,
/// the same rule the Settled `config.get` wire applies to provider secrets.
pub(super) fn redact_api_keys(mut value: Value) -> Value {
    redact_api_keys_in_place(&mut value);
    value
}

fn redact_api_keys_in_place(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for (key, entry) in map.iter_mut() {
                if (key == "apiKey" || key == "api_key") && entry.is_string() {
                    *entry = Value::String("[REDACTED]".to_string());
                } else {
                    redact_api_keys_in_place(entry);
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                redact_api_keys_in_place(item);
            }
        }
        _ => {}
    }
}

/// Extract the first string argument, accepting the object spelling (any of
/// `names`) and the positional tuple spelling (`["<value>"]`).
pub(super) fn first_string_arg(params: &Value, names: &[&str]) -> Result<String, EngineError> {
    match normalize_params(params.clone()) {
        Value::Array(items) => match items.first() {
            Some(Value::String(s)) => Ok(s.clone()),
            _ => Err(invalid_params("expected the first argument to be a string")),
        },
        Value::Object(map) => {
            for name in names {
                if let Some(Value::String(s)) = map.get(*name) {
                    return Ok(s.clone());
                }
            }
            Err(invalid_params(format!(
                "missing required string argument (one of {names:?})"
            )))
        }
        _ => Err(invalid_params("expected an object or positional tuple")),
    }
}

/// Extract a `(string, value, optional target)` triple, accepting the
/// object spelling (`{<name>: ..., value, target}`) and the positional
/// tuple spelling (`[key, value, target?]`).
pub(super) fn triple_args(
    params: &Value,
    names: &[&str],
) -> Result<(String, Value, Option<String>), EngineError> {
    match normalize_params(params.clone()) {
        Value::Array(items) => {
            if items.len() < 2 || items.len() > 3 {
                return Err(invalid_params("expected [key, value, target?]"));
            }
            let key = items[0]
                .as_str()
                .map(str::to_string)
                .ok_or_else(|| invalid_params("expected the first argument to be a string"))?;
            let target = items.get(2).and_then(Value::as_str).map(str::to_string);
            Ok((key, items[1].clone(), target))
        }
        Value::Object(map) => {
            let mut key = None;
            for name in names {
                if let Some(Value::String(s)) = map.get(*name) {
                    key = Some(s.clone());
                    break;
                }
            }
            let key = key.ok_or_else(|| {
                invalid_params(format!(
                    "missing required string argument (one of {names:?})"
                ))
            })?;
            let value = map
                .get("value")
                .cloned()
                .ok_or_else(|| invalid_params("missing required 'value' argument"))?;
            let target = map
                .get("target")
                .and_then(Value::as_str)
                .map(str::to_string);
            Ok((key, value, target))
        }
        _ => Err(invalid_params("expected an object or positional tuple")),
    }
}

/// Extract a `(record, optional target)` pair for `replaceSections`,
/// accepting `{sections, target}` and `[record, target?]`.
pub(super) fn pair_args_record(params: &Value) -> Result<(Value, Option<String>), EngineError> {
    match normalize_params(params.clone()) {
        Value::Array(items) => {
            if items.is_empty() || items.len() > 2 {
                return Err(invalid_params("expected [sections, target?]"));
            }
            let target = items.get(1).and_then(Value::as_str).map(str::to_string);
            Ok((items[0].clone(), target))
        }
        Value::Object(map) => {
            let record = map
                .get("sections")
                .or_else(|| map.get("record"))
                .cloned()
                .ok_or_else(|| invalid_params("missing required 'sections' record"))?;
            let target = map
                .get("target")
                .and_then(Value::as_str)
                .map(str::to_string);
            Ok((record, target))
        }
        _ => Err(invalid_params("expected an object or positional tuple")),
    }
}
