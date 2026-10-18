//! Pre-validation argument conversion.
//!
//! Two converters (matching TypeScript):
//! 1. [`parameter_alias_convert`] — remaps LLM-hallucinated parameter names
//!    to canonical schema names (e.g. `offset` → `line_offset`)
//! 2. [`numeric_string_convert`] — coerces numeric strings to numbers where
//!    the schema declares `type: "number"`
//!
//! These run after parsing but before schema validation, to handle common
//! LLM argument mistakes gracefully.

use std::collections::HashSet;

use serde_json::Value;

/// Convert tool arguments before validation.
///
/// Applies both the parameter alias remapper and the numeric string coercer
/// in sequence.
pub fn convert_tool_args(_tool_name: &str, schema: &Value, args: &mut Value) {
    parameter_alias_convert(schema, args);
    numeric_string_convert(schema, args);
}

/// Known parameter aliases (matching TypeScript `parameter-alias-config.ts`).
///
/// Each tuple is `(alias, canonical)`. When the args contain `alias` but NOT
/// `canonical`, and the schema has `canonical` in its properties, the alias
/// key is remapped to the canonical key.
const PARAMETER_ALIASES: &[(&str, &str)] = &[
    ("offset", "line_offset"),
    ("limit", "n_lines"),
    ("pattern", "regex"),
    ("path", "glob"),
    ("query", "pattern"),
];

/// Remap alias parameter names to canonical schema names.
///
/// If a key in `args` is NOT in `schema.properties`, but a known alias IS
/// listed in [`PARAMETER_ALIASES`], the alias key is remapped to the canonical
/// key. If both alias and canonical are present, the canonical value wins
/// (alias is dropped).
fn parameter_alias_convert(schema: &Value, args: &mut Value) {
    let Some(obj) = args.as_object_mut() else {
        return;
    };
    let Some(props) = schema.get("properties").and_then(|p| p.as_object()) else {
        return;
    };

    let schema_keys: HashSet<&str> = props.keys().map(|s| s.as_str()).collect();

    for &(alias, canonical) in PARAMETER_ALIASES {
        // Only remap if the schema has the canonical key but NOT the alias key.
        if schema_keys.contains(canonical)
            && !schema_keys.contains(alias)
            && let Some(val) = obj.remove(alias)
        {
            // Canonical wins if already present (don't overwrite)
            obj.entry(canonical.to_string()).or_insert(val);
        }
    }
}

/// Coerce numeric strings to numbers where schema declares `type: "number"`.
///
/// This handles the common LLM mistake of passing `"42"` instead of `42` for
/// a numeric field. Only coerces when:
/// - The property schema exists and has `"type": "number"`
/// - The argument value is a string
/// - The string parses as `f64`
fn numeric_string_convert(schema: &Value, args: &mut Value) {
    let Some(obj) = args.as_object_mut() else {
        return;
    };
    let Some(props) = schema.get("properties").and_then(|p| p.as_object()) else {
        return;
    };

    for (key, val) in obj.iter_mut() {
        if let Some(prop_schema) = props.get(key)
            && let Some(prop_type) = prop_schema.get("type").and_then(|t| t.as_str())
            && (prop_type == "number" || prop_type == "integer")
            && let Some(s) = val.as_str()
        {
            if prop_type == "number" {
                if let Ok(n) = s.parse::<f64>() {
                    *val = serde_json::json!(n);
                }
            } else if prop_type == "integer"
                && let Ok(n) = s.parse::<i64>()
            {
                *val = serde_json::json!(n);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_remap_alias_to_canonical_when_schema_has_canonical_key() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "line_offset": { "type": "number" }
            }
        });
        let mut args = serde_json::json!({"offset": 10});
        parameter_alias_convert(&schema, &mut args);
        assert_eq!(args["line_offset"], 10);
        assert!(args.get("offset").is_none());
    }

    #[test]
    fn should_keep_canonical_value_when_both_alias_and_canonical_present() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "line_offset": { "type": "number" }
            }
        });
        let mut args = serde_json::json!({"offset": 10, "line_offset": 20});
        parameter_alias_convert(&schema, &mut args);
        // Canonical wins
        assert_eq!(args["line_offset"], 20);
        assert!(args.get("offset").is_none());
    }

    #[test]
    fn should_leave_alias_untouched_when_schema_defines_alias_key() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "offset": { "type": "number" }
            }
        });
        let mut args = serde_json::json!({"offset": 10});
        parameter_alias_convert(&schema, &mut args);
        // Should stay as "offset" since schema has it
        assert_eq!(args["offset"], 10);
    }

    #[test]
    fn should_coerce_string_to_number_when_schema_type_is_number() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "count": { "type": "number" }
            }
        });
        let mut args = serde_json::json!({"count": "42"});
        numeric_string_convert(&schema, &mut args);
        assert_eq!(args["count"], 42.0);
    }

    #[test]
    fn should_keep_numeric_string_when_schema_type_is_string() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "name": { "type": "string" }
            }
        });
        let mut args = serde_json::json!({"name": "42"});
        numeric_string_convert(&schema, &mut args);
        assert_eq!(args["name"], "42");
    }

    #[test]
    fn should_coerce_string_to_integer_when_schema_type_is_integer() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "n_lines": { "type": "integer" }
            }
        });
        let mut args = serde_json::json!({"n_lines": "100"});
        numeric_string_convert(&schema, &mut args);
        assert_eq!(args["n_lines"], 100);
    }

    #[test]
    fn should_apply_aliases_and_numeric_coercion_when_converting_tool_args() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "line_offset": { "type": "number" }
            }
        });
        let mut args = serde_json::json!({"offset": "10"});
        convert_tool_args("Read", &schema, &mut args);
        // Alias remapped AND numeric string coerced
        assert_eq!(args["line_offset"], 10.0);
        assert!(args.get("offset").is_none());
    }
}
