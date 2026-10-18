//! Hook pattern matching logic.
//!
//! A hook's optional `pattern` is a regex that restricts which inputs the
//! hook fires for. The matcher checks the `tool_name` field of the input
//! JSON for tool-related events, falling back to matching against the
//! entire serialized JSON string.

use regex::Regex;

use crate::hook_engine::types::HookEventType;

/// Check whether `pattern` matches the given `input` for the given `event`.
///
/// # Matching rules
///
/// - If `pattern` is `None` or an empty string, the hook matches **all**
///   inputs (this is the "match all" sentinel).
/// - Otherwise the pattern is compiled as a regex. If compilation fails the
///   hook is treated as **not matching** (a safe default that avoids
///   running a hook with an ambiguous intent).
/// - For tool-related events ([`HookEventType::PreToolUse`],
///   [`HookEventType::PostToolUse`], etc.) the regex is tested against the
///   `tool_name` string field of the JSON input if present.
/// - For all other events the regex is tested against the entire serialized
///   JSON string of the input.
pub fn matches(pattern: &Option<String>, input: &serde_json::Value, event: &HookEventType) -> bool {
    // Empty or absent pattern = match all.
    let Some(pat) = pattern.as_deref() else {
        return true;
    };
    if pat.is_empty() {
        return true;
    }

    let Ok(re) = Regex::new(pat) else {
        // A malformed regex never matches — safer than matching everything.
        return false;
    };

    if is_tool_related_event(event) {
        // Try the `tool_name` field first (camelCase, then snake_case).
        if let Some(tool_name) = input
            .get("tool_name")
            .or_else(|| input.get("toolName"))
            .and_then(|v| v.as_str())
        {
            return re.is_match(tool_name);
        }
    }

    // Fall back to matching against the entire JSON string.
    let json_str = serde_json::to_string(input).unwrap_or_default();
    re.is_match(&json_str)
}

/// Whether the event type is associated with a tool invocation and therefore
/// eligible for `tool_name`-based pattern matching.
fn is_tool_related_event(event: &HookEventType) -> bool {
    matches!(
        event,
        HookEventType::PreToolUse
            | HookEventType::PostToolUse
            | HookEventType::PostToolUseFailure
            | HookEventType::PermissionRequest
            | HookEventType::PermissionResult
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_match_all_inputs_when_pattern_is_none() {
        let input = serde_json::json!({"tool_name": "Read"});
        assert!(matches(&None, &input, &HookEventType::PreToolUse));
    }

    #[test]
    fn should_match_all_inputs_when_pattern_is_empty_string() {
        let input = serde_json::json!({"tool_name": "Read"});
        assert!(matches(
            &Some(String::new()),
            &input,
            &HookEventType::PreToolUse
        ));
    }

    #[test]
    fn should_match_input_when_regex_matches_tool_name() {
        let input = serde_json::json!({"tool_name": "Read"});
        assert!(matches(
            &Some("Re.*".to_string()),
            &input,
            &HookEventType::PreToolUse
        ));
    }

    #[test]
    fn should_not_match_when_regex_does_not_match_tool_name() {
        let input = serde_json::json!({"tool_name": "Write"});
        assert!(!matches(
            &Some("^Read$".to_string()),
            &input,
            &HookEventType::PreToolUse
        ));
    }

    #[test]
    fn should_never_match_when_regex_is_invalid() {
        let input = serde_json::json!({"tool_name": "Read"});
        assert!(!matches(
            &Some("[invalid".to_string()),
            &input,
            &HookEventType::PreToolUse
        ));
    }

    #[test]
    fn should_match_serialized_json_when_event_is_not_tool_related() {
        let input = serde_json::json!({"session_id": "abc123"});
        assert!(matches(
            &Some("abc123".to_string()),
            &input,
            &HookEventType::SessionStart
        ));
    }
}
