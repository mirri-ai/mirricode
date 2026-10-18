//! Flag-domain dispatch handlers for the embedded
//! [`EmbeddedServer`](super::EmbeddedServer) — the B5-a `flag.*` surface,
//! the klient `flagService` contract mirrored on the engine wire.
//!
//! The registry is [`EmbeddedServer::flag_registry`] — shared behind an
//! `Arc`, empty by default (the embedded composition root registers no
//! flags; hosts register through the handle). The five methods:
//!
//! - `enabled(id)` → `bool` — the klient semantics: an *unregistered* id is
//!   `false` (the env chain never promotes an id the registry does not
//!   know); a registered id resolves through the env override chain.
//! - `snapshot()` → the `record(id → bool)` of the real registered set.
//! - `enabledIds()` → the ids whose effective state is enabled.
//! - `explain(id)` → the rich state, or JSON `null` for an unknown id (the
//!   klient `maybe` semantics — the same convention `workspace.get` uses).
//! - `explainAll()` → the rich states of every registered flag, sorted by
//!   id.
//!
//! Wire shape (the klient `experimentalFeatureStateSchema`) is served by
//! [`super::responses::FlagStateResult`] — the rich-field mapping and its
//! honest degradations (`surface` defaults to `core`, `source` never
//! reports `config` because the rust registry has no config override chain,
//! `configValue` is omitted) are documented there and in the TSV row notes.

use serde::Deserialize;
use serde_json::Value;

use mirri_transport::adapter::EngineError;

use super::responses::{FlagStateResult, to_result};
use super::{EmbeddedServer, invalid_params, normalize_params};

impl EmbeddedServer {
    /// `flag.enabled` — whether the flag id is enabled. Unregistered ids
    /// are `false` (klient semantics: the env chain does not promote ids
    /// the registry does not know).
    pub(super) async fn dispatch_flag_enabled(&self, params: Value) -> Result<Value, EngineError> {
        let id = flag_id_params(params)?;
        let enabled = self
            .flag_registry
            .explain(&id)
            .is_some_and(|state| state.enabled);
        to_result(enabled)
    }

    /// `flag.snapshot` — the registered flags' effective states as the
    /// klient record (`id → bool`). The registry starts empty in the
    /// embedded composition root, so the honest default is `{}`.
    pub(super) async fn dispatch_flag_snapshot(&self) -> Result<Value, EngineError> {
        let snapshot = self.flag_registry.snapshot();
        let record: serde_json::Map<String, Value> = snapshot
            .into_iter()
            .map(|(id, enabled)| (id, Value::Bool(enabled)))
            .collect();
        to_result(record)
    }

    /// `flag.enabledIds` — the registered ids whose effective state is
    /// enabled (sorted by id, the snapshot's deterministic order).
    pub(super) async fn dispatch_flag_enabled_ids(&self) -> Result<Value, EngineError> {
        let ids: Vec<String> = self
            .flag_registry
            .snapshot()
            .into_iter()
            .filter(|(_, enabled)| *enabled)
            .map(|(id, _)| id)
            .collect();
        to_result(ids)
    }

    /// `flag.explain` — the flag's rich state, or JSON `null` when the id
    /// is not registered (the klient `maybe` semantics: an unknown id is a
    /// miss, not an error).
    pub(super) async fn dispatch_flag_explain(&self, params: Value) -> Result<Value, EngineError> {
        let id = flag_id_params(params)?;
        match self.flag_registry.explain(&id) {
            Some(state) => to_result(FlagStateResult::from(state)),
            None => to_result(Value::Null),
        }
    }

    /// `flag.explainAll` — every registered flag's rich state, sorted by
    /// id (the empty registry answers `[]` — the real registered set, never
    /// a fabricated one).
    pub(super) async fn dispatch_flag_explain_all(&self) -> Result<Value, EngineError> {
        let states: Vec<FlagStateResult> = self
            .flag_registry
            .explain_all()
            .into_iter()
            .map(FlagStateResult::from)
            .collect();
        to_result(states)
    }
}

/// Parse the single flag-id parameter the klient tuple contract sends:
/// either the positional `["<id>"]` form or the object `{"id": "<id>"}`
/// form (both spellings accepted, the domain's tolerant convention).
fn flag_id_params(params: Value) -> Result<String, EngineError> {
    #[derive(Debug, Deserialize)]
    struct FlagIdParams {
        id: String,
    }
    let params = normalize_params(params);
    if let Value::Array(items) = &params {
        if items.len() != 1 || !items[0].is_string() {
            return Err(invalid_params(
                "flag methods take a single flag id: [\"<id>\"] or {\"id\": \"<id>\"}",
            ));
        }
        return Ok(items[0].as_str().expect("checked above").to_string());
    }
    let params: FlagIdParams =
        serde_json::from_value(params).map_err(|e| invalid_params(e.to_string()))?;
    Ok(params.id)
}
