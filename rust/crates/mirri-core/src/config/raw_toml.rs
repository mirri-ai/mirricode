//! Raw TOML document persistence: serialize-and-merge round trips that
//! preserve unknown keys from the original file.

use super::wire::{
    KNOWN_CONFIG_TOP_LEVEL_KEYS, KNOWN_MODEL_ENTRY_KEYS, KNOWN_PROVIDER_ENTRY_KEYS,
    merge_map_entry_passthrough,
};
use super::*;

impl ServerConfig {
    /// Serialize `self` to a pretty TOML string for persistence.
    ///
    /// When [`ServerConfig::raw`] is present (i.e. the config was loaded from
    /// a TOML file), the serialized struct is the source of truth for all
    /// known fields, and any unknown keys from the original `raw` document
    /// are merged back in. This preserves unknown keys that the struct does
    /// not model, ensuring round-trip fidelity (a save/load cycle never drops
    /// user-written keys the engine does not understand), while correctly
    /// reflecting deletions of known fields (e.g. a removed provider or a
    /// cleared `default_provider`).
    pub fn to_toml_string(&self) -> EngineResult<String> {
        // Serialize the struct (minus `raw`, which has `#[serde(skip)]`).
        // Compact style inlines arrays (matching the TypeScript smol-toml
        // writer) so a save from either engine does not churn array lines.
        let serialized_str = toml::to_string(self).map_err(|err| {
            EngineError::RequestInvalid(format!("cannot serialize config: {err}"))
        })?;

        if let Some(ref raw) = self.raw {
            // Re-parse the serialized struct as a `toml::Value` so we can
            // merge unknown keys from the raw document into it.
            let mut serialized: toml::Value = toml::from_str(&serialized_str).map_err(|err| {
                EngineError::RequestInvalid(format!("cannot re-parse serialized config: {err}"))
            })?;

            // Merge unknown keys from `raw` into the serialized struct. Only
            // keys that are NOT known struct fields are preserved — a known
            // field absent from the serialized struct means it was cleared
            // (e.g. `default_provider = None`), and re-merging it from `raw`
            // would undo the deletion.
            if let (Some(merged_table), Some(raw_table)) =
                (serialized.as_table_mut(), raw.as_table())
            {
                for (key, value) in raw_table {
                    if !KNOWN_CONFIG_TOP_LEVEL_KEYS.contains(&key.as_str())
                        && !merged_table.contains_key(key)
                    {
                        merged_table.insert(key.clone(), value.clone());
                    }
                }
                // The `providers` / `models` maps are known top-level keys,
                // so the loop above does not reach inside them — but the
                // B5-b modelService/providerService records persist extra
                // per-entry keys (klient passthrough fields such as
                // `providerId` / `baseUrl` / `deletedModels`) that the typed
                // entries do not model. Merge those per-entry unknown keys
                // back in so a struct-path save (e.g. `config.set`) does not
                // silently drop them.
                for section in ["providers", "models"] {
                    merge_map_entry_passthrough(
                        merged_table,
                        raw_table,
                        section,
                        if section == "providers" {
                            KNOWN_PROVIDER_ENTRY_KEYS
                        } else {
                            KNOWN_MODEL_ENTRY_KEYS
                        },
                    );
                }
            }
            toml::to_string(&serialized).map_err(|err| {
                EngineError::RequestInvalid(format!("cannot serialize config: {err}"))
            })
        } else {
            Ok(serialized_str)
        }
    }

    /// Write `self` to `path` as TOML, atomically and durably.
    ///
    /// Strips environment-synthesized entries (see
    /// [`strip_env_model_config`](Self::strip_env_model_config)) before
    /// serializing so the env overlay is never persisted to disk.
    ///
    /// See [`write_config_file_atomically`] for the write protocol (temp
    /// file + fsync + rename, mode 0o600).
    pub fn save_to(&self, path: &Path) -> EngineResult<()> {
        let mut clone = self.clone();
        clone.strip_env_model_config();
        let contents = clone.to_toml_string()?;
        write_config_file_atomically(path, &contents)
    }

    /// The raw TOML document behind `self`, materialized when the config was
    /// not parsed from a file (`raw` absent).
    ///
    /// The B5-b record-oriented services (`modelService` /
    /// `providerService` / the klient `configService` section methods)
    /// persist through this document so keys the typed struct does not
    /// model survive a save/load cycle.
    pub fn raw_document(&self) -> EngineResult<toml::Value> {
        match &self.raw {
            Some(raw) => Ok(raw.clone()),
            None => {
                let contents = self.to_toml_string()?;
                toml::from_str(&contents).map_err(|err| {
                    EngineError::RequestInvalid(format!("cannot re-serialize config: {err}"))
                })
            }
        }
    }

    /// Replace or clear top-level sections of the raw TOML document in place
    /// and re-parse the mutated document back into `self`.
    ///
    /// The B5-b `configService` section methods (`config.replace` /
    /// `config.replaceSections`) land here: `sections` carries the
    /// snake_case section key and either the replacement JSON value (its
    /// keys must already be snake_case) or `None` to clear the section (the
    /// klient `null`-means-clear encoding). The mutated document must parse
    /// back into a valid [`ServerConfig`] before it is committed — an
    /// invalid replacement fails with `RequestInvalid` and leaves `self`
    /// untouched. The raw document is committed onto the re-parsed config,
    /// so a subsequent save preserves the mutation.
    pub fn replace_raw_sections(
        &mut self,
        sections: &[(String, Option<Value>)],
    ) -> EngineResult<()> {
        self.mutate_raw_document(|raw| {
            let table = raw
                .as_table_mut()
                .ok_or_else(|| EngineError::RequestInvalid("raw config is not a table".into()))?;
            for (key, value) in sections {
                match value {
                    Some(value) => {
                        table.insert(key.clone(), json_to_toml_value(value)?);
                    }
                    None => {
                        table.remove(key);
                    }
                }
            }
            Ok(())
        })
    }

    /// Replace or clear one entry of a raw map section (`models` /
    /// `providers`) in place and re-parse the mutated document into `self`.
    ///
    /// The B5-b record services (`model.set` / `model.delete` /
    /// `provider.set` / `provider.delete`) land here: the value's keys must
    /// already be snake_case, and `None` removes the entry (a missing entry
    /// is a no-op, matching the klient service semantics).
    pub fn set_raw_section_entry(
        &mut self,
        section: &str,
        key: &str,
        value: Option<&Value>,
    ) -> EngineResult<()> {
        self.mutate_raw_document(|raw| {
            let table = raw
                .as_table_mut()
                .ok_or_else(|| EngineError::RequestInvalid("raw config is not a table".into()))?;
            match value {
                Some(value) => {
                    let entries = table
                        .entry(section)
                        .or_insert_with(|| toml::Value::Table(toml::map::Map::new()));
                    let entries = entries.as_table_mut().ok_or_else(|| {
                        EngineError::RequestInvalid(format!(
                            "raw config section '{section}' is not a table"
                        ))
                    })?;
                    entries.insert(key.to_string(), json_to_toml_value(value)?);
                }
                None => {
                    if let Some(entries) =
                        table.get_mut(section).and_then(toml::Value::as_table_mut)
                    {
                        entries.remove(key);
                    }
                }
            }
            Ok(())
        })
    }

    /// Write the raw TOML document verbatim to `path`, atomically and durably
    /// (see [`write_config_file_atomically`]).
    ///
    /// The record-oriented services persist through this (not
    /// [`save_to`], whose struct serialization would drop unmodeled keys).
    /// The document must be env-free — callers load with
    /// [`ServerConfig::load_without_env`] first, so nothing env-synthesized
    /// can leak to disk.
    pub fn save_raw_to(&self, path: &Path) -> EngineResult<()> {
        let raw = self
            .raw
            .as_ref()
            .ok_or_else(|| EngineError::RequestInvalid("config has no raw document".into()))?;
        let contents = toml::to_string(raw).map_err(|err| {
            EngineError::RequestInvalid(format!("cannot serialize raw config: {err}"))
        })?;
        write_config_file_atomically(path, &contents)
    }

    /// Remove the `api_key` / `apiKey` keys from every `[providers.*]`
    /// table of the raw TOML document, in place, and re-parse the mutated
    /// document into `self` (the same discipline as
    /// [`replace_raw_sections`](Self::replace_raw_sections): an invalid
    /// result leaves `self` untouched).
    ///
    /// The credential-store migration
    /// (`mirri-credentials` Task 2) calls this before
    /// [`save_raw_to`](Self::save_raw_to) so a migrated key is
    /// compensatingly cleaned from config.toml (spec E.2) while every
    /// other key of the raw document survives verbatim.
    pub fn strip_provider_api_keys(&mut self) -> EngineResult<()> {
        self.mutate_raw_document(|raw| {
            let table = raw
                .as_table_mut()
                .ok_or_else(|| EngineError::RequestInvalid("raw config is not a table".into()))?;
            if let Some(providers) = table.get_mut("providers").and_then(toml::Value::as_table_mut)
            {
                for (_id, entry) in providers.iter_mut() {
                    if let Some(entry) = entry.as_table_mut() {
                        // Both spellings exist in the wild (the typed entry
                        // accepts `apiKey` as an alias; the wire layer redacts
                        // both), so both are stripped.
                        entry.remove("api_key");
                        entry.remove("apiKey");
                    }
                }
            }
            Ok(())
        })
    }

    /// Shared mutation pipeline behind [`replace_raw_sections`] and
    /// [`set_raw_section_entry`]: run `mutate` on a copy of the raw document,
    /// require the result to re-parse into a valid [`ServerConfig`], then
    /// commit both the parsed struct and the mutated raw document.
    fn mutate_raw_document(
        &mut self,
        mutate: impl FnOnce(&mut toml::Value) -> EngineResult<()>,
    ) -> EngineResult<()> {
        let mut raw = self.raw_document()?;
        mutate(&mut raw)?;
        let contents = toml::to_string(&raw).map_err(|err| {
            EngineError::RequestInvalid(format!("cannot serialize mutated config: {err}"))
        })?;
        let mut parsed = ServerConfig::from_toml(&contents)?;
        parsed.raw = Some(raw);
        *self = parsed;
        Ok(())
    }
}
