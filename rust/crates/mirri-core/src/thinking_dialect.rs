//! Thinking-dialect resolution — which wire field carries "stop thinking".
//!
//! OpenAI-compatible endpoints disagree on how extended reasoning is turned
//! off: `reasoning_effort` (OpenAI), a `thinking` object with
//! `type: enabled|disabled` (MiMo, DeepSeek, Zhipu, Kimi K2, MiniMax), and a
//! bare `enable_thinking` boolean (Qwen/DashScope) are three spellings of the
//! same intent. Omitting the field means "use the provider default", which
//! for every one of those vendors is *on* — so a transport that only ever
//! omits cannot express "off" at all.

use serde::{Deserialize, Serialize};

/// Which wire field an endpoint reads to stop extended reasoning.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ThinkingDialect {
    /// `"thinking": {"type": "disabled"}` — MiMo, DeepSeek, Zhipu GLM,
    /// Kimi K2, MiniMax.
    #[serde(rename = "thinking_type", alias = "thinking.type")]
    ThinkingType,
    /// `"reasoning_effort": "none"` — OpenAI Chat Completions.
    #[serde(rename = "reasoning_effort")]
    ReasoningEffort,
    /// `"enable_thinking": false` — Qwen / DashScope.
    #[serde(rename = "enable_thinking")]
    EnableThinking,
}

impl ThinkingDialect {
    /// Every accepted config spelling, in documentation order.
    pub const ALL: [&'static str; 3] = ["thinking_type", "reasoning_effort", "enable_thinking"];

    /// Parse a config value. Accepts the canonical snake_case spelling and
    /// the tolerated dotted / camel spellings; an unknown value yields
    /// `None` so the caller can fall through instead of guessing.
    pub fn parse(raw: &str) -> Option<Self> {
        let normalized: String = raw
            .trim()
            .chars()
            .filter(|c| *c != '_' && *c != '.' && *c != '-')
            .flat_map(char::to_lowercase)
            .collect();
        match normalized.as_str() {
            "thinkingtype" => Some(Self::ThinkingType),
            "reasoningeffort" => Some(Self::ReasoningEffort),
            "enablethinking" => Some(Self::EnableThinking),
            _ => None,
        }
    }

    /// The canonical config spelling.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::ThinkingType => "thinking_type",
            Self::ReasoningEffort => "reasoning_effort",
            Self::EnableThinking => "enable_thinking",
        }
    }
}

/// The capability spelling marking a model whose reasoning cannot be turned
/// off through this engine.
pub const ALWAYS_THINKING: &str = "always_thinking";

/// The lowercased host of `url`, or an empty string when it cannot be
/// parsed. Accepts a bare host (`api.deepseek.com`) as well as a full URL —
/// the config field is user-supplied and both shapes appear in practice.
fn host_of(url: &str) -> String {
    let trimmed = url.trim().to_ascii_lowercase();
    if trimmed.is_empty() {
        return String::new();
    }
    let without_scheme = trimmed
        .strip_prefix("https://")
        .or_else(|| trimmed.strip_prefix("http://"))
        .unwrap_or(&trimmed);
    without_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or("")
        .split('@')
        .next_back()
        .unwrap_or("")
        .split(':')
        .next()
        .unwrap_or("")
        .to_string()
}

/// Whether `host` equals `suffix` or is a subdomain of it. Exact-or-suffix
/// matching keeps `z.ai` from matching `fizz.ai` and `kimi.com` from matching
/// `notkimi.com`.
fn host_matches(host: &str, suffix: &str) -> bool {
    host == suffix || host.ends_with(&format!(".{suffix}"))
}

/// The builtin endpoint table: vendors whose "off" spelling is known.
///
/// Matching is by host substring, then by model-name prefix where one vendor
/// splits across dialects. Anything absent resolves to `None` — the
/// transport keeps omitting the field, and the model is surfaced as
/// `always_thinking`.
///
/// Deliberately absent: OpenAI itself. Its `reasoning_effort: "none"` is
/// valid only for some models, so an endpoint-level guess would turn a
/// "cannot disable" model into a 400. Imported OpenAI models carry the fact
/// per model instead.
pub fn infer_dialect_from_endpoint(base_url: &str, model: &str) -> Option<ThinkingDialect> {
    // Match the HOST, not the whole URL: a gateway path such as
    // `https://gw.example.test/proxy/deepseek` must not be read as DeepSeek,
    // and `z.ai` must not match `fizz.ai`.
    let host = host_of(base_url);
    let model = model.trim().to_ascii_lowercase();
    if host.is_empty() {
        return None;
    }
    if host_matches(&host, "xiaomimimo.com") {
        return Some(ThinkingDialect::ThinkingType);
    }
    if host_matches(&host, "deepseek.com") {
        return Some(ThinkingDialect::ThinkingType);
    }
    if host_matches(&host, "bigmodel.cn") || host_matches(&host, "z.ai") {
        return Some(ThinkingDialect::ThinkingType);
    }
    if host_matches(&host, "minimax.io") {
        return Some(ThinkingDialect::ThinkingType);
    }
    if host_matches(&host, "dashscope.aliyuncs.com")
        || host_matches(&host, "aliyuncs.com")
    {
        return Some(ThinkingDialect::EnableThinking);
    }
    if host.contains("moonshot")
        || host_matches(&host, "kimi.com")
        || host_matches(&host, "mirricode.com")
    {
        // Kimi K3 rejects the `thinking` object outright and exposes no
        // "none" effort tier, so it has no expressible off.
        if model.starts_with("kimi-k3") || model.starts_with("k3") {
            return None;
        }
        return Some(ThinkingDialect::ThinkingType);
    }
    None
}

/// Whether a thinking-object endpoint ALSO documents a top-level
/// `reasoning_effort` field. DeepSeek and Zhipu do; MiMo, Kimi K2 and
/// MiniMax document only the thinking object.
pub fn documents_sibling_effort(base_url: &str) -> bool {
    let host = base_url.trim().to_ascii_lowercase();
    host.contains("deepseek") || host.contains("bigmodel.cn") || host.contains("z.ai")
}

/// Resolve the dialect through the config layers, then the builtin table.
///
/// An unrecognized explicit setting falls through to the next layer rather
/// than aborting: a typo must not silently disable reasoning control for a
/// model the builtin table already knows.
pub fn resolve_dialect(
    model_setting: Option<&str>,
    provider_setting: Option<&str>,
    base_url: &str,
    model: &str,
) -> Option<ThinkingDialect> {
    model_setting
        .and_then(ThinkingDialect::parse)
        .or_else(|| provider_setting.and_then(ThinkingDialect::parse))
        .or_else(|| infer_dialect_from_endpoint(base_url, model))
}

/// The resolved thinking-expression contract for one model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ThinkingIntent {
    /// How to spell "off". `None` — the endpoint cannot be addressed.
    pub dialect: Option<ThinkingDialect>,
    /// Whether an "off" request may be sent at all.
    pub off_allowed: bool,
    /// Whether the model declares reasoning-effort tiers. Only declared
    /// models receive an effort value alongside a thinking object — an
    /// endpoint that documents no tiers may reject the extra field.
    pub declared_efforts: bool,
    /// Whether the endpoint documents a TOP-LEVEL effort field next to its
    /// thinking object. MiMo / Kimi K2 / MiniMax document the object alone,
    /// so a tier sent to them would be an undeclared field; DeepSeek and
    /// Zhipu document both.
    pub emits_sibling_effort: bool,
}

impl ThinkingIntent {
    /// Resolve the intent for one model.
    ///
    /// `declared_capabilities` is the alias's capability list (for the
    /// `always_thinking` opt-out); the two settings are the alias-level and
    /// provider-level `thinking_off` values.
    pub fn resolve(
        declared_capabilities: &[String],
        declared_efforts: bool,
        model_setting: Option<&str>,
        provider_setting: Option<&str>,
        base_url: &str,
        model: &str,
    ) -> Self {
        let always = declared_capabilities
            .iter()
            .any(|cap| cap.trim().eq_ignore_ascii_case(ALWAYS_THINKING));
        let dialect = resolve_dialect(model_setting, provider_setting, base_url, model);
        // Which thinking-object endpoints also document a top-level effort
        // field. Kept as a table so the encoding stays data-driven rather
        // than growing a per-vendor branch in the adapter.
        let emits_sibling_effort = matches!(
            infer_dialect_from_endpoint(base_url, model),
            Some(ThinkingDialect::ThinkingType)
        ) && documents_sibling_effort(base_url);
        // An always-thinking model keeps its dialect: the marker suppresses
        // the OFF payload only. Dropping the dialect here would push its
        // effort encoding back onto the legacy path, which emits
        // `reasoning_effort` unconditionally — a field some of these
        // endpoints do not document.
        Self {
            dialect,
            off_allowed: !always && dialect.is_some(),
            declared_efforts,
            emits_sibling_effort,
        }
    }
}

/// Compute the capability list a consumer should see: the declared list,
/// plus the always-thinking marker when off cannot be delivered.
///
/// The declared list stays the model fact (user-editable, written on
/// import); this projection adds the engine's "can this actually be
/// honoured today" verdict without rewriting configuration.
pub fn effective_capabilities(declared: &[String], intent: &ThinkingIntent) -> Vec<String> {
    let mut caps = declared.to_vec();
    // Only a model that actually declares thinking can be "always thinking".
    // Appending the marker to a plain chat model would flip it from
    // "unsupported" to "always on" in the composer.
    let declares_thinking = caps
        .iter()
        .any(|cap| cap.trim().eq_ignore_ascii_case("thinking"));
    if declares_thinking
        && !intent.off_allowed
        && !caps
            .iter()
            .any(|cap| cap.trim().eq_ignore_ascii_case(ALWAYS_THINKING))
    {
        caps.push(ALWAYS_THINKING.to_string());
    }
    caps
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_parse_every_spelling_when_reading_a_config_value() {
        // Given the canonical and tolerated spellings
        // When parsed
        // Then each maps to its dialect and an unknown value is rejected
        assert_eq!(ThinkingDialect::parse("thinking_type"), Some(ThinkingDialect::ThinkingType));
        assert_eq!(ThinkingDialect::parse("thinking.type"), Some(ThinkingDialect::ThinkingType));
        assert_eq!(ThinkingDialect::parse(" ReasoningEffort "), Some(ThinkingDialect::ReasoningEffort));
        assert_eq!(ThinkingDialect::parse("enable-thinking"), Some(ThinkingDialect::EnableThinking));
        assert_eq!(ThinkingDialect::parse("native"), None);
    }

    #[test]
    fn should_round_trip_every_dialect_through_its_config_spelling() {
        // Given every dialect
        // When serialized to its spelling and parsed back
        // Then the value is unchanged
        for spelling in ThinkingDialect::ALL {
            let parsed = ThinkingDialect::parse(spelling).expect("canonical spelling parses");
            assert_eq!(parsed.as_str(), spelling);
        }
    }

    #[test]
    fn should_infer_the_dialect_from_the_endpoint_host_when_no_setting_is_present() {
        // Given endpoints of the vendors in the builtin table
        // When resolved without any config setting
        // Then each answers its documented dialect
        assert_eq!(
            infer_dialect_from_endpoint("https://token-plan-cn.xiaomimimo.com/v1", "mimo-v2.6-flash"),
            Some(ThinkingDialect::ThinkingType)
        );
        assert_eq!(
            infer_dialect_from_endpoint("https://api.deepseek.com", "deepseek-flash"),
            Some(ThinkingDialect::ThinkingType)
        );
        assert_eq!(
            infer_dialect_from_endpoint("https://open.bigmodel.cn/api/paas/v4", "glm-5.2"),
            Some(ThinkingDialect::ThinkingType)
        );
        assert_eq!(
            infer_dialect_from_endpoint("https://api.moonshot.cn/v1", "kimi-k2.6"),
            Some(ThinkingDialect::ThinkingType)
        );
        assert_eq!(
            infer_dialect_from_endpoint("https://dashscope.aliyuncs.com/compatible-mode/v1", "qwen3.5-plus"),
            Some(ThinkingDialect::EnableThinking)
        );
    }

    #[test]
    fn should_not_infer_a_dialect_from_a_path_segment_when_the_host_is_a_gateway() {
        // Given a gateway whose PATH mentions a vendor
        // When resolved
        // Then the vendor table does not claim it (host-only matching)
        assert_eq!(
            infer_dialect_from_endpoint("https://gw.example.test/proxy/deepseek", "m"),
            None
        );
        // And a look-alike domain is not mistaken for the vendor
        assert_eq!(infer_dialect_from_endpoint("https://fizz.ai/v1", "m"), None);
    }

    #[test]
    fn should_report_no_dialect_when_the_endpoint_is_unknown() {
        // Given a gateway host the builtin table does not know
        // When resolved
        // Then no dialect is claimed
        assert_eq!(infer_dialect_from_endpoint("https://gateway.example.test/v1", "mimo-x"), None);
        assert_eq!(infer_dialect_from_endpoint("", "anything"), None);
    }

    #[test]
    fn should_leave_kimi_k3_without_a_dialect_when_it_rejects_the_thinking_object() {
        // Given Kimi K3, which accepts neither the thinking object nor a
        // "none" effort tier
        // When resolved from the Kimi host
        // Then no off spelling is claimed for it
        assert_eq!(infer_dialect_from_endpoint("https://api.moonshot.cn/v1", "kimi-k3"), None);
        // And a K2 model on the same host still resolves
        assert_eq!(
            infer_dialect_from_endpoint("https://api.moonshot.cn/v1", "kimi-k2.6"),
            Some(ThinkingDialect::ThinkingType)
        );
    }

    #[test]
    fn should_prefer_the_model_setting_over_the_provider_setting() {
        // Given both layers set to different dialects
        // When resolved
        // Then the model-level value wins
        assert_eq!(
            resolve_dialect(
                Some("reasoning_effort"),
                Some("thinking_type"),
                "https://token-plan-cn.xiaomimimo.com/v1",
                "mimo-v2.6-flash"
            ),
            Some(ThinkingDialect::ReasoningEffort)
        );
    }

    #[test]
    fn should_fall_through_to_the_builtin_table_when_the_setting_is_unrecognized() {
        // Given a typo'd setting above an endpoint the table knows
        // When resolved
        // Then the typo does not disable reasoning control
        assert_eq!(
            resolve_dialect(
                Some("thinkingtype-typo"),
                None,
                "https://api.deepseek.com",
                "deepseek-flash"
            ),
            Some(ThinkingDialect::ThinkingType)
        );
    }
    #[test]
    fn should_forbid_off_when_the_model_declares_always_thinking() {
        // Given a model declaring always_thinking on an endpoint whose
        // dialect is otherwise known
        // When the intent resolves
        // Then the dialect is withheld and off is not allowed
        let intent = ThinkingIntent::resolve(
            &["thinking".to_string(), "always_thinking".to_string()],
            false,
            None,
            None,
            "https://api.deepseek.com",
            "deepseek-flash",
        );
        // The dialect is retained (it still governs how a declared effort is
        // encoded); only the OFF payload is suppressed.
        assert_eq!(intent.dialect, Some(ThinkingDialect::ThinkingType));
        assert!(!intent.off_allowed);
    }

    #[test]
    fn should_allow_off_when_the_dialect_resolves_and_nothing_forbids_it() {
        // Given MiMo's endpoint, a plain thinking capability, and no setting
        // When the intent resolves
        // Then off is deliverable through the thinking object
        let intent = ThinkingIntent::resolve(
            &["thinking".to_string()],
            false,
            None,
            None,
            "https://token-plan-cn.xiaomimimo.com/v1",
            "mimo-v2.6-flash",
        );
        assert_eq!(intent.dialect, Some(ThinkingDialect::ThinkingType));
        assert!(intent.off_allowed);
    }

    #[test]
    fn should_withhold_off_when_no_dialect_resolves() {
        // Given an unknown gateway with no setting
        // When the intent resolves
        // Then off is not allowed
        let intent = ThinkingIntent::resolve(
            &["thinking".to_string()],
            false,
            None,
            None,
            "https://gateway.example.test/v1",
            "some-model",
        );
        assert_eq!(intent.dialect, None);
        assert!(!intent.off_allowed);
    }

    #[test]
    fn should_append_always_thinking_when_off_cannot_be_delivered() {
        // Given a declared capability list without the marker
        // And an intent that cannot deliver off
        // When the effective capability list is computed
        // Then the marker is appended exactly once
        let intent = ThinkingIntent::resolve(
            &["thinking".to_string()],
            false,
            None,
            None,
            "https://gateway.example.test/v1",
            "some-model",
        );
        let effective = effective_capabilities(&["thinking".to_string()], &intent);
        assert_eq!(effective, vec!["thinking".to_string(), ALWAYS_THINKING.to_string()]);
    }

    #[test]
    fn should_leave_capabilities_untouched_when_off_is_deliverable() {
        // Given a deliverable dialect
        // When the effective capability list is computed
        // Then the declared list is returned unchanged
        let intent = ThinkingIntent::resolve(
            &["thinking".to_string()],
            false,
            None,
            None,
            "https://api.deepseek.com",
            "deepseek-flash",
        );
        let effective = effective_capabilities(&["thinking".to_string()], &intent);
        assert_eq!(effective, vec!["thinking".to_string()]);
    }

    #[test]
    fn should_leave_a_model_without_thinking_capability_untouched() {
        // Given a plain chat model with no reasoning capability at all
        // And an endpoint whose dialect is unknown
        let intent = ThinkingIntent::resolve(
            &[],
            false,
            None,
            None,
            "https://gateway.example.test/v1",
            "chat-model",
        );

        // When the effective capability list is computed
        let effective = effective_capabilities(&[], &intent);

        // Then nothing is appended — "not a thinking model" must not be
        // reported as "always thinking"
        assert!(effective.is_empty());
    }

    #[test]
    fn should_keep_the_dialect_when_the_model_declares_always_thinking() {
        // Given an always-thinking model on a thinking-object endpoint
        // When the intent resolves
        let intent = ThinkingIntent::resolve(
            &["thinking".to_string(), ALWAYS_THINKING.to_string()],
            false,
            None,
            None,
            "https://api.deepseek.com",
            "deepseek-flash",
        );

        // Then the dialect survives (so effort encoding stays dialect-aware)
        // while the off payload is suppressed
        assert_eq!(intent.dialect, Some(ThinkingDialect::ThinkingType));
        assert!(!intent.off_allowed);
    }

    #[test]
    fn should_not_duplicate_always_thinking_when_already_declared() {
        // Given a list that already carries the marker
        // When the effective list is computed for an undeliverable intent
        // Then the marker appears once
        let intent = ThinkingIntent::resolve(
            &[ALWAYS_THINKING.to_string()],
            false,
            None,
            None,
            "https://gateway.example.test/v1",
            "m",
        );
        let effective = effective_capabilities(&[ALWAYS_THINKING.to_string()], &intent);
        assert_eq!(effective, vec![ALWAYS_THINKING.to_string()]);
    }
}
