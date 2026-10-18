//! Log redaction utilities — strip secrets from structured data before
//! they reach logs or CLI output.
//!
//! Mirrors the TypeScript `redactCtx` / `redactString` pair from
//! `packages/agent-core/src/logging/formatter.ts`.

use serde_json::{Map, Value};

/// Keys whose values are replaced with `[REDACTED]` when found in
/// structured JSON data. A key is redacted when its normalized form
/// (lowercased with `_`/`-`/`.` separators stripped) exactly matches one
/// of these patterns — mirroring TS `normalizeKey` + `REDACTED_KEYS`
/// membership, so e.g. `apiKey`, `api_key`, `accessToken` and
/// `client_secret` all match while `maxTokens` does not.
const REDACTED_KEY_PATTERNS: &[&str] = &[
    "authorization",
    "apikey",
    "token",
    "refreshtoken",
    "accesstoken",
    "idtoken",
    "password",
    "secret",
    "clientsecret",
    "apisecret",
    "cookie",
    "setcookie",
    "bearer",
];

/// Raw-string secret patterns for redacting secrets embedded in free
/// text (e.g. `"authorization: bearer sk-..."`).
const RAW_SECRET_PATTERNS: &[(&str, &str)] = &[
    // authorization: bearer <token>
    (
        r"(?i)(authorization\s*[:=]\s*)(bearer\s+)?[A-Za-z0-9_\-\.]+",
        r"$1[REDACTED]",
    ),
    // api_key=<value>  or  api-key: <value>
    (r"(?i)(api[_-]?key\s*[:=]\s*)[^\s,;}\]]+", r"$1[REDACTED]"),
    // cookie: <value>
    (r"(?i)(cookie\s*[:=]\s*)[^\s,;}\]]+", r"$1[REDACTED]"),
    // sk-<key> (Anthropic/OpenAI key prefix)
    (r"sk-[A-Za-z0-9_\-]{20,}", "[REDACTED]"),
];

/// Check whether a key name matches any redacted pattern.
///
/// Mirrors the TS pair `normalizeKey` + `REDACTED_KEYS.has(...)`: the key
/// is lowercased, `_`/`-`/`.` separators are stripped, and the result must
/// equal one of [`REDACTED_KEY_PATTERNS`] exactly.
fn is_redacted_key(key: &str) -> bool {
    let normalized: String = key
        .to_lowercase()
        .chars()
        .filter(|c| !matches!(c, '_' | '-' | '.'))
        .collect();
    REDACTED_KEY_PATTERNS
        .iter()
        .any(|pattern| normalized == *pattern)
}

/// Recursively redact sensitive values in a JSON value.
///
/// Walks objects and arrays; any key matching [`REDACTED_KEY_PATTERNS`]
/// has its value replaced with `"[REDACTED]"`. Includes depth and
/// cycle protection.
pub fn redact_json(value: &Value) -> Value {
    redact_json_inner(value, 0)
}

fn redact_json_inner(value: &Value, depth: u8) -> Value {
    if depth > 20 {
        return Value::String("[REDACTED: depth limit]".to_string());
    }
    match value {
        Value::Object(map) => {
            let redacted: Map<String, Value> = map
                .iter()
                .map(|(k, v)| {
                    if is_redacted_key(k) {
                        (k.clone(), Value::String("[REDACTED]".to_string()))
                    } else {
                        (k.clone(), redact_json_inner(v, depth + 1))
                    }
                })
                .collect();
            Value::Object(redacted)
        }
        Value::Array(arr) => Value::Array(
            arr.iter()
                .map(|v| redact_json_inner(v, depth + 1))
                .collect(),
        ),
        Value::String(s) => Value::String(redact_string(s)),
        other => other.clone(),
    }
}

/// Redact known secret patterns in a raw string.
pub fn redact_string(input: &str) -> String {
    let mut result = input.to_string();
    for (pattern, replacement) in RAW_SECRET_PATTERNS {
        // Use regex crate if available, otherwise simple string replacement
        // For now, use simple contains-based approach for key patterns
        if let Ok(re) = regex::Regex::new(pattern) {
            result = re.replace_all(&result, *replacement).to_string();
        }
    }
    result
}

/// Redact the `api_key` / `apiKey` fields from a provider config JSON
/// before printing to stdout or logs.
pub fn redact_provider_config(value: &Value) -> Value {
    let mut v = value.clone();
    if let Some(obj) = v.as_object_mut() {
        for (_, provider) in obj.iter_mut() {
            if let Some(p) = provider.as_object_mut() {
                if p.contains_key("apiKey") {
                    p.insert(
                        "apiKey".to_string(),
                        Value::String("[REDACTED]".to_string()),
                    );
                }
                if p.contains_key("api_key") {
                    p.insert(
                        "api_key".to_string(),
                        Value::String("[REDACTED]".to_string()),
                    );
                }
            }
        }
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn should_redact_api_key_when_key_matches_pattern() {
        let input = json!({"apiKey": "sk-secret123", "name": "test"});
        let result = redact_json(&input);
        assert_eq!(result["apiKey"], "[REDACTED]");
        assert_eq!(result["name"], "test");
    }

    #[test]
    fn should_redact_nested_token_when_in_object() {
        let input = json!({
            "config": {"accessToken": "abc123", "port": 8080}
        });
        let result = redact_json(&input);
        assert_eq!(result["config"]["accessToken"], "[REDACTED]");
        assert_eq!(result["config"]["port"], 8080);
    }

    #[test]
    fn should_redact_sk_prefix_when_in_string() {
        let input = "error: sk-abcdefghijklmnopqrstuvwxyz123456 invalid";
        let result = redact_string(input);
        assert!(result.contains("[REDACTED]"));
        assert!(!result.contains("sk-abcdef"));
    }

    #[test]
    fn should_redact_authorization_header_when_in_string() {
        let input = "authorization: bearer eyJhbGciOi.test";
        let result = redact_string(input);
        assert!(result.contains("[REDACTED]"));
    }

    #[test]
    fn should_preserve_non_sensitive_fields_when_redacting() {
        let input = json!({
            "provider": "anthropic",
            "model": "claude-sonnet-4",
            "apiKey": "sk-test",
            "maxTokens": 8192
        });
        let result = redact_json(&input);
        assert_eq!(result["provider"], "anthropic");
        assert_eq!(result["model"], "claude-sonnet-4");
        assert_eq!(result["apiKey"], "[REDACTED]");
        assert_eq!(result["maxTokens"], 8192);
    }

    #[test]
    fn should_redact_provider_config_when_applied() {
        let input = json!({
            "default": {"type": "anthropic", "apiKey": "sk-test", "baseUrl": "https://api.anthropic.com"}
        });
        let result = redact_provider_config(&input);
        assert_eq!(result["default"]["apiKey"], "[REDACTED]");
        assert_eq!(result["default"]["type"], "anthropic");
        assert_eq!(result["default"]["baseUrl"], "https://api.anthropic.com");
    }

    #[test]
    fn should_handle_array_when_redacting() {
        let input = json!([
            {"name": "ok", "password": "secret"},
            {"token": "abc", "value": 42}
        ]);
        let result = redact_json(&input);
        assert_eq!(result[0]["name"], "ok");
        assert_eq!(result[0]["password"], "[REDACTED]");
        assert_eq!(result[1]["token"], "[REDACTED]");
        assert_eq!(result[1]["value"], 42);
    }

    #[test]
    fn should_respect_depth_limit_when_deeply_nested() {
        let mut input = json!({"key": "value"});
        for _ in 0..25 {
            input = json!({"nested": input});
        }
        let result = redact_json(&input);
        // Should not stack overflow; depth limit kicks in
        assert!(result.to_string().contains("REDACTED"));
    }
}
