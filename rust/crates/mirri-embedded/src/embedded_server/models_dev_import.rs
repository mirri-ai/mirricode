//! The models.dev directory → alias derivation (B6, ADR-004): the field
//! mapping the TS baseline (`modelsDevModelToCapability` /
//! `modelsDevModelToRecord`) applies when a directory model becomes a
//! `[models]` alias. Pure functions over the raw directory JSON — no
//! engine state, no IO — so import and browse share one filter and one
//! derivation (the browse face emits exactly the facts an import would
//! write).

use std::sync::OnceLock;

use serde_json::Value;

/// The bundled bootstrap models.dev directory — a field-trimmed,
/// provider-curated subset of the live community directory (see
/// `data/models-dev-snapshot.json` headers for source/fetch date/license).
/// It replaces ONLY the empty face of the lazy load: a fresh home (no
/// disk cache) can browse and import a small realistic directory, while a
/// runtime refresh still swaps in the full live fetch and remains the only
/// writer of the disk cache (the snapshot is never written to disk).
///
/// The top-level bookkeeping keys (`_source` / `_fetched_at` / `_license` /
/// `_note`) are dropped before returning: the browse and merge faces
/// iterate top-level keys as provider rows, so the meta headers must never
/// enter the served catalog.
pub(super) fn bundled_models_dev_snapshot() -> Option<Value> {
    static SNAPSHOT: OnceLock<Option<Value>> = OnceLock::new();
    SNAPSHOT
        .get_or_init(|| {
            let parsed =
                serde_json::from_str::<Value>(include_str!("../../data/models-dev-snapshot.json"))
                    .ok()?;
            let mut catalog = parsed.as_object()?.clone();
            catalog.retain(|key, _| !key.starts_with('_') && key != "license");
            Some(Value::Object(catalog))
        })
        .clone()
}

/// The alias fields one importable directory model contributes — the
/// TS-derived subset of `[models]` fields (ADR-004: `displayName` and
/// `description` are deliberately absent; `maxInputSize` / `offEffort` /
/// `always_thinking` have no rust landing field, G-18).
pub(super) struct ModelsDevAliasRecord {
    pub context: u64,
    pub capabilities: Vec<String>,
    pub max_output_size: Option<u64>,
    pub reasoning_key: Option<String>,
    pub support_efforts: Option<Vec<String>>,
}

/// The usable-chat-model predicate (the TS `isUsableChatModel` +
/// `modelsDevModelToCapability` context pre-filter): a model without a
/// positive integer `limit.context`, a deprecated/alpha status, an
/// output-modality list without `text`, or an embedding-marked
/// family/id/name is not importable (import AND browse skip it — one
/// shared set, ADR-004).
fn is_usable_chat_model(raw: &Value) -> bool {
    if let Some(status) = raw.get("status").and_then(Value::as_str)
        && (status == "deprecated" || status == "alpha")
    {
        return false;
    }
    if let Some(outputs) = raw.pointer("/modalities/output").and_then(Value::as_array)
        && !outputs
            .iter()
            .any(|modality| modality.as_str() == Some("text"))
    {
        return false;
    }
    !has_embedding_marker(raw.get("family").and_then(Value::as_str))
        && !has_embedding_marker(raw.get("id").and_then(Value::as_str))
        && !has_embedding_marker(raw.get("name").and_then(Value::as_str))
}

/// The TS `hasEmbeddingMarker`: a lowercase `embedding` substring, or the
/// `(^|[-_/])embed($|[-_/])` word-shape hit.
fn has_embedding_marker(value: Option<&str>) -> bool {
    let Some(lower) = value.map(str::to_lowercase) else {
        return false;
    };
    if lower.contains("embedding") {
        return true;
    }
    let bytes = lower.as_bytes();
    let boundary = |byte: u8| matches!(byte, b'-' | b'_' | b'/');
    let mut from = 0;
    while let Some(relative) = lower[from..].find("embed") {
        let start = from + relative;
        let end = start + "embed".len();
        let before_ok = start == 0 || boundary(bytes[start - 1]);
        let after_ok = end == bytes.len() || boundary(bytes[end]);
        if before_ok && after_ok {
            return true;
        }
        from = start + 1;
    }
    false
}

struct ThinkingOptions {
    efforts: Option<Vec<String>>,
    has_toggle: bool,
}

/// The TS `modelsDevThinkingOptions` (effort tiers only — `offEffort` and
/// `alwaysThinking` have no rust landing field, G-18): an `effort` option's
/// string tiers minus the 'none' tier; a `toggle` option only marks
/// thinking present.
fn models_dev_thinking_options(raw: &Value) -> ThinkingOptions {
    let mut efforts = None;
    let mut has_toggle = false;
    let Some(options) = raw.get("reasoning_options").and_then(Value::as_array) else {
        return ThinkingOptions {
            efforts,
            has_toggle,
        };
    };
    for option in options {
        match option.get("type").and_then(Value::as_str) {
            Some("toggle") => has_toggle = true,
            Some("effort") => {
                let Some(values) = option.get("values").and_then(Value::as_array) else {
                    continue;
                };
                let selectable: Vec<String> = values
                    .iter()
                    .filter_map(Value::as_str)
                    .filter(|value| !value.is_empty() && value.to_lowercase() != "none")
                    .map(str::to_string)
                    .collect();
                if !selectable.is_empty() {
                    efforts = Some(selectable);
                }
            }
            _ => {}
        }
    }
    ThinkingOptions {
        efforts,
        has_toggle,
    }
}

/// Derive one directory model's alias fields — `None` marks an unusable
/// chat model the import skips (the TS `modelsDevModelToCapability`
/// returning `undefined`). The mapping is the TS baseline:
/// - `tool_use` = `tool_call != false` (an absent declaration defaults to
///   callable — the TS `model.tool_call ?? true`);
/// - `thinking` = `reasoning == true` OR effort tiers exist OR a toggle
///   exists;
/// - `image_in`/`video_in`/`audio_in` from `modalities.input`;
/// - `dynamically_loaded_tools` only when explicitly `true`;
/// - `maxOutputSize` = a positive `limit.output`;
/// - `reasoningKey` = `interleaved.field` (trimmed, non-empty);
/// - `supportEfforts` = the effort tiers minus the 'none' tier.
pub(super) fn models_dev_alias_record(raw: &Value) -> Option<ModelsDevAliasRecord> {
    let context = raw.pointer("/limit/context").and_then(Value::as_u64)?;
    if context == 0 {
        return None;
    }
    if !is_usable_chat_model(raw) {
        return None;
    }
    let inputs = raw.pointer("/modalities/input").and_then(Value::as_array);
    let input_has =
        |kind: &str| inputs.is_some_and(|list| list.iter().any(|m| m.as_str() == Some(kind)));
    let thinking_options = models_dev_thinking_options(raw);
    let mut capabilities = Vec::new();
    if input_has("image") {
        capabilities.push("image_in".to_string());
    }
    if input_has("video") {
        capabilities.push("video_in".to_string());
    }
    if input_has("audio") {
        capabilities.push("audio_in".to_string());
    }
    let reasoning = raw
        .get("reasoning")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if reasoning || thinking_options.efforts.is_some() || thinking_options.has_toggle {
        capabilities.push("thinking".to_string());
    }
    if raw.get("tool_call").and_then(Value::as_bool) != Some(false) {
        capabilities.push("tool_use".to_string());
    }
    if raw.get("dynamically_loaded_tools").and_then(Value::as_bool) == Some(true) {
        capabilities.push("dynamically_loaded_tools".to_string());
    }
    Some(ModelsDevAliasRecord {
        context,
        capabilities,
        max_output_size: raw
            .pointer("/limit/output")
            .and_then(Value::as_u64)
            .filter(|output| *output > 0),
        reasoning_key: raw
            .get("interleaved")
            .and_then(Value::as_object)
            .and_then(|interleaved| interleaved.get("field"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|field| !field.is_empty())
            .map(str::to_string),
        support_efforts: thinking_options.efforts,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn should_derive_capabilities_from_modalities_and_tool_call_when_both_declared() {
        let record = models_dev_alias_record(&json!({
            "id": "imp-large",
            "limit": { "context": 200000, "output": 8192 },
            "tool_call": true,
            "reasoning": true,
            "interleaved": { "field": "reasoning" },
            "modalities": { "input": ["text", "image"] },
            "dynamically_loaded_tools": true
        }))
        .expect("a fully declared model is usable");
        assert_eq!(record.context, 200000);
        assert_eq!(
            record.capabilities,
            vec![
                "image_in".to_string(),
                "thinking".to_string(),
                "tool_use".to_string(),
                "dynamically_loaded_tools".to_string()
            ]
        );
        assert_eq!(record.max_output_size, Some(8192));
        assert_eq!(record.reasoning_key.as_deref(), Some("reasoning"));
        assert_eq!(record.support_efforts, None);
    }

    #[test]
    fn should_default_tool_use_to_present_when_tool_call_is_absent() {
        // The TS `model.tool_call ?? true` — an absent declaration means
        // callable.
        let record = models_dev_alias_record(&json!({
            "id": "imp-small", "limit": { "context": 32000 }
        }))
        .expect("a context-only model is usable");
        assert_eq!(record.capabilities, vec!["tool_use".to_string()]);
        assert_eq!(record.max_output_size, None);
        assert_eq!(record.reasoning_key, None);
    }

    #[test]
    fn should_skip_models_without_a_positive_context() {
        assert!(models_dev_alias_record(&json!({ "id": "imp-raw" })).is_none());
        assert!(
            models_dev_alias_record(&json!({
                "id": "imp-zero", "limit": { "context": 0 }
            }))
            .is_none()
        );
    }

    #[test]
    fn should_skip_deprecated_and_alpha_models() {
        for status in ["deprecated", "alpha"] {
            assert!(
                models_dev_alias_record(&json!({
                    "id": "imp-old", "limit": { "context": 1000 }, "status": status
                }))
                .is_none(),
                "status {status} must disqualify"
            );
        }
    }

    #[test]
    fn should_skip_models_whose_output_lacks_text() {
        assert!(
            models_dev_alias_record(&json!({
                "id": "imp-image",
                "limit": { "context": 1000 },
                "modalities": { "input": ["text"], "output": ["image"] }
            }))
            .is_none()
        );
    }

    #[test]
    fn should_skip_embedding_marked_models() {
        for id in ["text-embedding-3", "my-embed-tool", "family-embed"] {
            assert!(
                models_dev_alias_record(&json!({
                    "id": id, "limit": { "context": 1000 }
                }))
                .is_none(),
                "id {id} must be filtered"
            );
        }
        // `myembedded` is NOT the word shape — it stays usable.
        assert!(
            models_dev_alias_record(&json!({
                "id": "myembedded", "limit": { "context": 1000 }
            }))
            .is_some()
        );
    }

    #[test]
    fn should_mark_thinking_when_effort_tiers_exist_even_without_reasoning_flag() {
        let record = models_dev_alias_record(&json!({
            "id": "imp-effort",
            "limit": { "context": 1000 },
            "reasoning_options": [ { "type": "effort", "values": ["low", "high"] } ]
        }))
        .expect("an effort-tier model is usable");
        assert!(record.capabilities.contains(&"thinking".to_string()));
        assert_eq!(
            record.support_efforts,
            Some(vec!["low".to_string(), "high".to_string()])
        );
    }

    #[test]
    fn should_exclude_the_none_tier_from_support_efforts() {
        let record = models_dev_alias_record(&json!({
            "id": "imp-none",
            "limit": { "context": 1000 },
            "reasoning_options": [ { "type": "effort", "values": ["none", "low", "high"] } ]
        }))
        .expect("an effort-tier model is usable");
        assert_eq!(
            record.support_efforts,
            Some(vec!["low".to_string(), "high".to_string()])
        );
    }

    #[test]
    fn should_derive_reasoning_key_only_from_a_non_empty_interleaved_field() {
        let record = models_dev_alias_record(&json!({
            "id": "imp-key", "limit": { "context": 1000 },
            "interleaved": { "field": "  reasoning_content " }
        }))
        .expect("an interleaved model is usable");
        assert_eq!(record.reasoning_key.as_deref(), Some("reasoning_content"));
        let record = models_dev_alias_record(&json!({
            "id": "imp-key2", "limit": { "context": 1000 },
            "interleaved": true
        }))
        .expect("a boolean interleaved is not a filter");
        assert_eq!(record.reasoning_key, None);
    }

    #[test]
    fn should_derive_max_output_size_only_when_positive() {
        let record = models_dev_alias_record(&json!({
            "id": "imp-out", "limit": { "context": 1000, "output": 0 }
        }))
        .expect("a zero output is not a filter");
        assert_eq!(record.max_output_size, None);
    }

    #[test]
    fn should_include_dynamically_loaded_tools_only_when_true() {
        let record = models_dev_alias_record(&json!({
            "id": "imp-dyn", "limit": { "context": 1000 },
            "dynamically_loaded_tools": true
        }))
        .expect("a dynamically-loaded model is usable");
        assert!(
            record
                .capabilities
                .contains(&"dynamically_loaded_tools".to_string())
        );
        let record = models_dev_alias_record(&json!({
            "id": "imp-dyn2", "limit": { "context": 1000 },
            "dynamically_loaded_tools": false
        }))
        .expect("an explicit false is not a filter");
        assert!(
            !record
                .capabilities
                .contains(&"dynamically_loaded_tools".to_string())
        );
    }
}
