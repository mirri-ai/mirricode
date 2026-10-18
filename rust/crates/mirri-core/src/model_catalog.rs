//! Model catalog — the engine's read-only view of configured model aliases.
//!
//! The [`ModelCatalog`] trait gives the rest of the engine access to the set
//! of models the host has configured (from `ServerConfig.models`, provider
//! capability tables, or any other host-owned source):
//!
//! - `AgentTool` / `AgentSwarmTool` inject the available alias list into
//!   their tool descriptions so the LLM can pick a valid `model` override.
//! - [`SessionSubagentHost`](crate::subagent::SessionSubagentHost) validates
//!   model-alias overrides against the catalog before spawning, so an invalid
//!   alias fails fast with the list of valid options instead of silently
//!   routing to a wrong model.
//! - The capability-gap detector reads real per-model capabilities from the
//!   catalog instead of assuming every model is text-only.
//!
//! The trait deliberately returns owned values (cheap clones of small
//! structs) so implementations can read from an `RwLock`-protected config
//! snapshot without lifetime gymnastics.

use crate::config::ModelAlias;
use crate::profile::ThinkingEffort;

/// One configured model alias in the catalog.
///
/// `id` is the user-facing alias name (the `[models.<id>]` config key, e.g.
/// `"sonnet"` or `"kimi/alpha"`); `model` is the actual model identifier sent
/// to the provider (e.g. `"claude-sonnet-4-20250514"`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelCatalogEntry {
    /// Model alias name (the key the LLM passes as `model` in tool calls).
    pub id: String,
    /// Actual model name sent to the provider.
    pub model: String,
    /// Provider key (`"anthropic"`, `"openai"`, custom id, ...).
    pub provider: String,
    /// Human-readable description of the model alias, when configured.
    pub description: Option<String>,
    /// Maximum context size in tokens, when known.
    pub max_context_size: Option<usize>,
    /// Capability names, e.g. `["vision", "tool-use", "streaming"]`.
    pub capabilities: Vec<String>,
    /// Default reasoning effort for the model, when configured.
    pub default_effort: Option<String>,
}

impl ModelCatalogEntry {
    /// Build an entry from a config `[models.<id>]` alias.
    ///
    /// Capability and size fields that are unset in the config default to
    /// their "unknown" representations (`None` / empty) so callers can
    /// distinguish "not configured" from a real value.
    pub fn from_model_alias(id: &str, alias: &ModelAlias) -> Self {
        Self {
            id: id.to_string(),
            model: alias.model.clone(),
            provider: alias.provider.clone(),
            description: alias.description.clone(),
            max_context_size: (alias.max_context_size > 0)
                .then_some(alias.max_context_size as usize),
            capabilities: alias.capabilities.clone().unwrap_or_default(),
            default_effort: alias.default_effort.clone(),
        }
    }
}

/// Build catalog entries from a server config snapshot.
///
/// Prefers the explicit `models` map (each alias becomes one entry keyed by
/// its alias id); falls back to the legacy single-model config as one
/// entry (200k context, the TS legacy default) so alias validation and
/// enumeration work for both config shapes. Single source for every
/// config-derived catalog consumer (`mirri-embedded`'s
/// `ConfigModelCatalog`, the serve wire's `session.status` window read).
pub fn model_entries_from_config(config: &crate::config::ServerConfig) -> Vec<ModelCatalogEntry> {
    if let Some(models) = &config.models {
        return models
            .iter()
            .map(|(id, alias)| ModelCatalogEntry::from_model_alias(id, alias))
            .collect();
    }
    if let Some(model) = &config.model {
        return vec![ModelCatalogEntry {
            id: model.id.clone(),
            model: model.id.clone(),
            provider: model
                .provider
                .clone()
                .unwrap_or_else(|| "default".to_string()),
            description: None,
            max_context_size: Some(200_000),
            capabilities: Vec::new(),
            default_effort: None,
        }];
    }
    Vec::new()
}

/// The effort a model falls back to when the caller sets none explicitly.
///
/// Mirrors the TS `defaultThinkingEffortForModel`
/// (`packages/agent-core-v2/src/kosong/model/thinking.ts:148-160` /
/// `apps/mirri-web/src/lib/modelThinking.ts:26-43`): the declared
/// `default_effort` when it is listed in `support_efforts` (an unconstrained
/// declaration also binds), else the middle of `support_efforts`
/// (`efforts[floor(len / 2)]`).
///
/// `None` — no declaration at all — keeps the caller's current effort. That
/// is the deliberate deviation (D2) from the TS `'on'` fallback: the rust
/// engine never force-enables thinking for models that declare nothing.
pub fn default_thinking_effort(
    default_effort: Option<&str>,
    support_efforts: &[String],
) -> Option<ThinkingEffort> {
    let parse = |effort: &str| {
        serde_json::from_value::<ThinkingEffort>(serde_json::Value::String(effort.to_string())).ok()
    };
    if let Some(declared) = default_effort
        && let Some(effort) = parse(declared)
        && (support_efforts.is_empty() || support_efforts.iter().any(|s| s == declared))
    {
        return Some(effort);
    }
    if support_efforts.is_empty() {
        return None;
    }
    parse(&support_efforts[support_efforts.len() / 2])
}

/// The context window (in tokens) of the model `model_id` names in the
/// config: an exact alias id, else the `${provider}/${model}` composite
/// resolved through the alias naming the same pair. `0` when the id is
/// unknown or the resolved alias declares no window — the "no ceiling
/// wired" spelling clients already treat as unknown.
pub fn max_context_tokens_for(config: &crate::config::ServerConfig, model_id: &str) -> u64 {
    if model_id.is_empty() {
        return 0;
    }
    let entries = model_entries_from_config(config);
    let entry = entries
        .iter()
        .find(|entry| entry.id == model_id)
        .cloned()
        .or_else(|| {
            config
                .resolve_model_alias(model_id)
                .and_then(|key| entries.into_iter().find(|entry| entry.id == key))
        });
    entry
        .and_then(|entry| entry.max_context_size)
        .filter(|size| *size > 0)
        .map(|size| size as u64)
        .unwrap_or(0)
}

/// Read-only access to the configured model catalog.
///
/// Implemented by the host layer (e.g. `mirri-embedded`), which owns the
/// config snapshot. The engine core consumes the trait so tools and the
/// subagent host never depend on a concrete config representation.
pub trait ModelCatalog: Send + Sync {
    /// List all configured model aliases.
    fn list_models(&self) -> Vec<ModelCatalogEntry>;

    /// Look up a model alias by its id (`ModelCatalogEntry::id`).
    fn get_by_id(&self, id: &str) -> Option<ModelCatalogEntry>;

    /// D-M10.1 curated gate: `description` is the *only* visibility gate on
    /// the LLM perception surface (tool descriptions, spawn-error hints).
    /// A model without a description stays visible on human-facing surfaces
    /// but is hidden from the LLM entirely.
    fn curated_models(&self) -> Vec<ModelCatalogEntry> {
        self.list_models()
            .into_iter()
            .filter(|e| {
                e.description
                    .as_deref()
                    .is_some_and(|d| !d.trim().is_empty())
            })
            .collect()
    }
}

/// The fixed model-adjudication sentence the dispatch tools (`Agent`,
/// `AgentSwarm`) render ahead of the curated model list (spec 11 R7/A8):
/// the description's only model guidance outside the catalog-sourced list
/// itself — a fixed rule instead of per-profile model ids.
pub const MODEL_DEFAULTS_SENTENCE: &str = "Model defaults to the dispatcher's current model unless overridden per agent; \
     available models are listed below.";

/// Resolve a model's media capabilities from the config declaration and
/// the name-inference catalog (multimodal spec US3.1 / architecture §4).
///
/// Priority semantics (ADR-003 revision): a declaration containing media
/// capability terms is authoritative — any media modality it does not list
/// is unsupported. An empty (or media-less) declaration falls back to name
/// inference; when neither has an opinion every media modality resolves
/// to `false` (undefined is NOT "unknown", media is never silently passed
/// through).
pub fn resolve_media_capability(
    declared: &[String],
    model_name: &str,
) -> crate::llm::ModelCapability {
    let mut caps = crate::llm::ModelCapability::default();
    let mut declares_media = false;
    for cap in declared {
        match cap.trim().to_ascii_lowercase().as_str() {
            "vision" | "image" | "image_in" => {
                caps.image_in = true;
                declares_media = true;
            }
            "video" | "video_in" => {
                caps.video_in = true;
                declares_media = true;
            }
            "audio" | "audio_in" => {
                caps.audio_in = true;
                declares_media = true;
            }
            "dynamically_loaded_tools" => caps.dynamically_loaded_tools = true,
            _ => {}
        }
    }
    if !declares_media {
        // 空声明（或仅非媒体词）→ 名字推断兜底；无信号则全 false。
        if crate::image_pipeline::name_vision_support(model_name) == Some(true) {
            caps.image_in = true;
        }
    }
    caps
}

/// Render the catalog's model listing for an error message — the valid
/// alias list, or the curated (alias — description) lines when any
/// descriptions exist (D-M10.4 self-correction surface). Capped so a huge
/// catalog cannot blow up the error message; an empty catalog answers with
/// the config remedy instead of an empty list.
///
/// Shared by every "model not configured" face so the same available-model
/// listing reaches the user (or LLM) wherever the rejection fired — the
/// subagent host's unknown-alias / three-nothing errors and the embedded
/// switch-path / LLM-factory rejections (spec 11 R10/A11).
pub fn render_model_listing(catalog: &dyn ModelCatalog) -> String {
    const MAX_INLINE_CURATED: usize = 20;
    let valid_models = catalog.list_models();
    if valid_models.is_empty() {
        return "no model aliases are configured (add a [models] entry to config.toml)".to_string();
    }
    let curated = catalog.curated_models();
    let mut listing = String::new();
    if curated.is_empty() {
        let valid_ids: Vec<&str> = valid_models.iter().map(|e| e.id.as_str()).collect();
        listing.push_str(&format!("Valid model aliases: {}", valid_ids.join(", ")));
    } else {
        listing.push_str("Valid model aliases (alias — description):\n");
        for entry in curated.iter().take(MAX_INLINE_CURATED) {
            listing.push_str(&curated_line(entry));
            listing.push('\n');
        }
        if curated.len() > MAX_INLINE_CURATED {
            listing.push_str(&format!(
                "... and {} more (curated list truncated to {MAX_INLINE_CURATED})",
                curated.len() - MAX_INLINE_CURATED
            ));
        }
    }
    listing
}

/// Render one curated entry as an LLM-facing line:
/// `- {alias} — {description}{, {N}k context}{ ({capabilities})}`.
///
/// The alias is the `[models.<id>]` config key — the only identity the LLM
/// references (D-M10.2: display name never appears on this surface). The
/// context size is round-half-up to whole KiB (`(n + 512) / 1024`), matching
/// TS `Math.round(maxContextSize / 1024)` in `buildSubagentModelDescriptions`
/// (`configSection.ts`) exactly for integer token counts.
pub fn curated_line(entry: &ModelCatalogEntry) -> String {
    let ctx = entry
        .max_context_size
        .map(|n| format!(", {}k context", (n + 512) / 1024))
        .unwrap_or_default();
    let caps = if entry.capabilities.is_empty() {
        String::new()
    } else {
        format!(" ({})", entry.capabilities.join(", "))
    };
    format!(
        "- {} — {}{}{}",
        entry.id,
        entry.description.clone().unwrap_or_default(),
        ctx,
        caps
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::ThinkingEffort;

    fn alias() -> ModelAlias {
        ModelAlias {
            provider: "anthropic".into(),
            model: "claude-sonnet-4-20250514".into(),
            max_context_size: 200_000,
            capabilities: Some(vec!["vision".into(), "tool-use".into()]),
            description: Some("The balanced model".into()),
            default_effort: Some("medium".into()),
            ..ModelAlias::default()
        }
    }

    #[test]
    fn should_resolve_declared_capabilities_as_known() {
        // Given 用户声明了能力词表
        // When 解析
        // Then 声明生效（US3.1 第一条）
        let caps = resolve_media_capability(&["image_in".into(), "video".into()], "x");
        assert!(caps.image_in && caps.video_in && !caps.audio_in);
    }

    #[test]
    fn should_treat_unlisted_media_as_unsupported_when_declaration_is_present() {
        // Given 用户声明了能力词表但未列 image，且模型名带 vision 标记（旧并集语义会误放行）
        // When 解析
        // Then 声明为王：image_in=false（ADR-003 修订：未定义=不支持）
        let caps = resolve_media_capability(&["audio".into()], "gpt-4o-vision");
        assert!(!caps.image_in);
        assert!(!caps.video_in);
        assert!(caps.audio_in);
    }

    #[test]
    fn should_fall_back_to_name_inference_when_declaration_is_empty() {
        // Given 无声明但模型名带 vision 标记
        // When 解析
        // Then 名字推断兜底生效
        let caps = resolve_media_capability(&[], "gpt-4o");
        assert!(caps.image_in);
    }

    #[test]
    fn should_default_all_modalities_to_unsupported_when_no_source_has_an_opinion() {
        // Given 无声明且名字无信号（旧语义为 unknown，放行由服务商裁决）
        // When 解析
        // Then 全模态 false（ADR-003 修订：废除未知态）
        let caps = resolve_media_capability(&[], "some-private-model");
        assert!(!caps.image_in && !caps.video_in && !caps.audio_in);
    }

    #[test]
    fn should_honor_declared_support_even_when_name_inference_disagrees() {
        // Given 声明 image 但名字推断为 text-only（deepseek）
        // When 解析
        // Then 声明为王：image_in=true
        let caps = resolve_media_capability(&["image".into()], "deepseek-chat");
        assert!(caps.image_in);
    }

    #[test]
    fn should_treat_non_media_declaration_as_media_absent() {
        // Given 只声明了非媒体能力（dynamically_loaded_tools）
        // When 解析
        // Then 媒体模态一律 false（声明为王，未列出即不支持）
        let caps = resolve_media_capability(&["dynamically_loaded_tools".into()], "some-model");
        assert!(!caps.image_in && !caps.video_in && !caps.audio_in);
        assert!(caps.dynamically_loaded_tools);
    }

    #[test]
    fn should_map_all_alias_fields_when_building_entry_from_config_alias() {
        // Given a configured model alias with all fields set
        let alias = alias();

        // When building a catalog entry from it
        let entry = ModelCatalogEntry::from_model_alias("sonnet", &alias);

        // Then every field carries the alias's value
        assert_eq!(entry.id, "sonnet");
        assert_eq!(entry.model, "claude-sonnet-4-20250514");
        assert_eq!(entry.provider, "anthropic");
        assert_eq!(entry.description.as_deref(), Some("The balanced model"));
        assert_eq!(entry.max_context_size, Some(200_000));
        assert_eq!(entry.capabilities, vec!["vision", "tool-use"]);
        assert_eq!(entry.default_effort.as_deref(), Some("medium"));
    }

    #[test]
    fn should_default_optional_fields_when_alias_has_zeros_and_missing_fields() {
        // Given an alias with no size, capabilities, description, or effort
        let alias = ModelAlias {
            provider: "openai".into(),
            model: "gpt-4o".into(),
            ..ModelAlias::default()
        };

        // When building a catalog entry from it
        let entry = ModelCatalogEntry::from_model_alias("gpt4o", &alias);

        // Then unknown fields are None / empty rather than zeroed values
        assert_eq!(entry.max_context_size, None);
        assert!(entry.capabilities.is_empty());
        assert!(entry.description.is_none());
        assert!(entry.default_effort.is_none());
    }

    /// A fixed catalog for testing the curated (LLM-facing) gate.
    struct TestCatalog {
        entries: Vec<ModelCatalogEntry>,
    }

    impl TestCatalog {
        fn new(entries: Vec<ModelCatalogEntry>) -> Self {
            Self { entries }
        }
    }

    impl ModelCatalog for TestCatalog {
        fn list_models(&self) -> Vec<ModelCatalogEntry> {
            self.entries.clone()
        }

        fn get_by_id(&self, id: &str) -> Option<ModelCatalogEntry> {
            self.entries.iter().find(|e| e.id == id).cloned()
        }
    }

    fn entry(id: &str, description: Option<&str>) -> ModelCatalogEntry {
        ModelCatalogEntry {
            id: id.to_string(),
            model: format!("model-{id}"),
            provider: "test".to_string(),
            description: description.map(String::from),
            max_context_size: None,
            capabilities: Vec::new(),
            default_effort: None,
        }
    }

    /// should list only models with a non-empty description when
    /// curated_models is called
    #[test]
    fn should_list_only_models_with_description_when_curated_models_is_called() {
        // Given 3 aliases of which 2 carry a real description (one is
        // whitespace-only — an empty purpose string is not a real one)
        let catalog = TestCatalog::new(vec![
            entry("sonnet", Some("The balanced model")),
            entry("opus", Some("   ")),
            entry("haiku", Some("Fast responses")),
        ]);

        // When the curated list is built for the LLM perception surface
        let curated = catalog.curated_models();

        // Then exactly 2 entries survive; empty / whitespace descriptions
        // are gated out
        assert_eq!(curated.len(), 2, "should gate on description: {curated:?}");
        assert_eq!(curated[0].id, "sonnet");
        assert_eq!(curated[1].id, "haiku");
        assert!(
            !curated.iter().any(|e| e.id == "opus"),
            "whitespace-only description must stay hidden: {curated:?}"
        );
    }

    /// should round the context size to the nearest KiB (Math.round parity
    /// with TS) when rendering a curated line
    #[test]
    fn should_round_context_to_nearest_kib_when_rendering_curated_line() {
        // Given a curated entry with a 1_000_000-token context window
        let mut opus = entry("opus", Some("Heavy lifting"));
        opus.max_context_size = Some(1_000_000);

        // When the curated line is rendered for the LLM surface
        let line = curated_line(&opus);

        // Then the size is round-half-up to whole KiB — 976.56 stays 976k
        // under floor but must render as 977k to match TS `Math.round`
        assert_eq!(
            line, "- opus — Heavy lifting, 977k context",
            "1_000_000 / 1024 = 976.56 → Math.round = 977: {line}"
        );
    }

    /// A server config whose `models` map carries one alias with a known
    /// context window under the provider + model pair `rakuten/GLM`.
    fn config_with_model_alias() -> crate::config::ServerConfig {
        use crate::config::ModelAlias;
        use std::collections::HashMap;

        crate::config::ServerConfig {
            models: Some(HashMap::from([(
                "expert".to_string(),
                ModelAlias {
                    provider: "rakuten".to_string(),
                    model: "GLM-5.3-Coding".to_string(),
                    max_context_size: 500_000,
                    ..ModelAlias::default()
                },
            )])),
            ..crate::config::ServerConfig::default()
        }
    }

    /// should resolve the context window from the alias id, the composite
    /// id, and stay 0 for unknown models
    #[test]
    fn should_resolve_the_context_window_from_alias_and_composite_model_ids() {
        let config = config_with_model_alias();

        // The alias id resolves directly
        assert_eq!(max_context_tokens_for(&config, "expert"), 500_000);
        // The `${provider}/${model}` composite the web pickers send
        // resolves through the alias naming the same pair
        assert_eq!(
            max_context_tokens_for(&config, "rakuten/GLM-5.3-Coding"),
            500_000
        );
        // An unknown model has no window — 0, the "unknown" spelling the
        // client treats as "no ceiling wired"
        assert_eq!(max_context_tokens_for(&config, "ghost"), 0);
    }

    /// should stay 0 when the resolved alias declares no window
    #[test]
    fn should_stay_zero_when_the_resolved_alias_declares_no_window() {
        use crate::config::ModelAlias;
        use std::collections::HashMap;

        let config = crate::config::ServerConfig {
            models: Some(HashMap::from([(
                "bare".to_string(),
                ModelAlias {
                    provider: "p".to_string(),
                    model: "m".to_string(),
                    ..ModelAlias::default()
                },
            )])),
            ..crate::config::ServerConfig::default()
        };

        assert_eq!(max_context_tokens_for(&config, "bare"), 0);
    }

    #[test]
    fn should_prefer_the_declared_default_effort_when_it_is_listed() {
        // Given a model declaring three efforts with max as the default
        let efforts = vec!["low".to_string(), "high".to_string(), "max".to_string()];
        // Then the declared default binds (custom spelling included)
        assert_eq!(
            default_thinking_effort(Some("max"), &efforts),
            Some(ThinkingEffort::Custom("max".to_string()))
        );
        // And an unlisted declared default falls back to the middle effort
        assert_eq!(
            default_thinking_effort(Some("ultra"), &efforts),
            Some(ThinkingEffort::High)
        );
    }

    #[test]
    fn should_take_the_middle_declared_effort_when_no_default_is_declared() {
        // Given a model declaring efforts without a default
        let efforts = vec!["low".to_string(), "high".to_string()];
        // Then the middle of the list applies (the TS middleOf:
        // efforts[floor(len / 2)])
        assert_eq!(
            default_thinking_effort(None, &efforts),
            Some(ThinkingEffort::High)
        );
        // And a model declaring nothing imposes no default (the caller keeps
        // its current effort — deviation D2 from the TS 'on' fallback)
        assert_eq!(default_thinking_effort(None, &[]), None);
    }
}
