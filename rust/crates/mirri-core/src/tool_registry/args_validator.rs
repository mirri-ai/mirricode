//! JSON Schema argument validation.
//!
//! Uses the `jsonschema` crate to compile each tool's parameter schema into a
//! reusable [`Validator`] and validate LLM-provided arguments against it.
//! Validators are cached per tool name via [`ValidatorCache`] to avoid
//! recompiling schemas on every step.

use std::collections::HashMap;

use jsonschema::Validator;
use serde_json::Value;

/// A compiled JSON Schema validator for a single tool's parameters.
pub struct ToolArgsValidator {
    validator: Validator,
    /// Sorted keys of `schema.properties`, used to suggest known arguments when
    /// an unknown/extra property is rejected.
    known_properties: Vec<String>,
}

impl ToolArgsValidator {
    /// Compile a JSON Schema into a validator.
    ///
    /// Returns an error string if the schema is invalid (cannot be compiled).
    pub fn new(schema: &Value) -> Result<Self, String> {
        let validator = Validator::new(schema).map_err(|e| e.to_string())?;
        let known_properties = schema
            .get("properties")
            .and_then(Value::as_object)
            .map(|m| {
                let mut v: Vec<String> = m.keys().cloned().collect();
                v.sort();
                v
            })
            .unwrap_or_default();
        Ok(Self {
            validator,
            known_properties,
        })
    }

    /// Validate arguments against the schema.
    ///
    /// Returns `Ok(())` if valid, or an error message string describing the
    /// first validation failure (matching AJV single-error style).
    pub fn validate(&self, args: &Value) -> Result<(), String> {
        match self.validator.validate(args) {
            Ok(()) => Ok(()),
            Err(err) => Err(format_validation_error_with_known(
                &err,
                &self.known_properties,
            )),
        }
    }
}

/// Format a validation error in a human-readable string.
///
/// Attempts to produce AJV-style messages for common cases (required property,
/// additional property) and falls back to the error's `Display` implementation.
fn format_validation_error(err: &jsonschema::ValidationError) -> String {
    let schema_path = err.schema_path.as_str();

    // "must have required property 'X'"
    if schema_path.ends_with("/required") {
        // jsonschema points `instance_path` at the missing property for
        // required-keyword errors; fall back to `instance` (a string) for
        // validators that report it differently.
        let missing = err.instance_path.as_str().trim_start_matches('/');
        let missing = if missing.is_empty() {
            err.instance.as_str().unwrap_or("value")
        } else {
            missing
        };
        return format!("must have required property '{missing}'");
    }

    // "must NOT have additional property 'X'"
    if schema_path.ends_with("/additionalProperties") {
        // jsonschema carries the offending keys in the error `kind`
        // (`instance` is the whole object, not a single key).
        if let jsonschema::error::ValidationErrorKind::AdditionalProperties { unexpected } =
            &err.kind
        {
            let extra = unexpected.first().map(String::as_str).unwrap_or("value");
            return format!("must NOT have additional property '{extra}'");
        }
    }

    // Fallback: use the error's Display implementation
    err.to_string()
}

/// Like [`format_validation_error`], but additionally suggests schema-declared
/// argument names when an extra property is rejected.
fn format_validation_error_with_known(
    err: &jsonschema::ValidationError,
    known: &[String],
) -> String {
    // "must NOT have additional property 'X'. Known arguments: a, b, c."
    if err.schema_path.as_str().ends_with("/additionalProperties")
        && let jsonschema::error::ValidationErrorKind::AdditionalProperties { unexpected } =
            &err.kind
    {
        let extra = unexpected.first().map(String::as_str).unwrap_or("value");
        return if known.is_empty() {
            format!("must NOT have additional property '{extra}'")
        } else {
            format!(
                "must NOT have additional property '{extra}'. Known arguments: {}.",
                known.join(", ")
            )
        };
    }

    format_validation_error(err)
}

/// A cache of validators keyed by tool name.
///
/// Schemas are compiled lazily on first access and reused for subsequent
/// validations of the same tool.
pub struct ValidatorCache {
    cache: HashMap<String, ToolArgsValidator>,
}

impl ValidatorCache {
    /// Create an empty validator cache.
    pub fn new() -> Self {
        Self {
            cache: HashMap::new(),
        }
    }

    /// Get or create a validator for a tool.
    ///
    /// If a validator for `tool_name` is already cached, returns it. Otherwise,
    /// compiles `schema` into a new validator, caches it, and returns a
    /// reference.
    ///
    /// Returns an error string if the schema cannot be compiled.
    pub fn get_or_create(
        &mut self,
        tool_name: &str,
        schema: &Value,
    ) -> Result<&ToolArgsValidator, String> {
        if !self.cache.contains_key(tool_name) {
            let validator = ToolArgsValidator::new(schema)?;
            self.cache.insert(tool_name.to_string(), validator);
        }
        Ok(self.cache.get(tool_name).expect("just inserted"))
    }

    /// Check if a validator for the given tool name is cached.
    pub fn contains(&self, tool_name: &str) -> bool {
        self.cache.contains_key(tool_name)
    }

    /// Clear all cached validators.
    pub fn clear(&mut self) {
        self.cache.clear();
    }
}

impl Default for ValidatorCache {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for ValidatorCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ValidatorCache")
            .field("cached_tools", &self.cache.keys().collect::<Vec<_>>())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_accept_args_when_they_match_the_schema() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "name": { "type": "string" }
            },
            "required": ["name"]
        });
        let validator = ToolArgsValidator::new(&schema).expect("valid schema");
        let args = serde_json::json!({"name": "hello"});
        assert!(validator.validate(&args).is_ok());
    }

    #[test]
    fn should_reject_with_required_error_when_required_property_is_missing() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "name": { "type": "string" }
            },
            "required": ["name"]
        });
        let validator = ToolArgsValidator::new(&schema).expect("valid schema");
        let args = serde_json::json!({});
        let err = validator.validate(&args).unwrap_err();
        assert!(
            err.contains("required"),
            "error should mention 'required': {}",
            err
        );
    }

    #[test]
    fn should_reject_when_arg_type_does_not_match_schema() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "count": { "type": "number" }
            }
        });
        let validator = ToolArgsValidator::new(&schema).expect("valid schema");
        let args = serde_json::json!({"count": "not a number"});
        assert!(validator.validate(&args).is_err());
    }

    #[test]
    fn should_reuse_cached_validator_when_getting_same_tool_again() {
        let mut cache = ValidatorCache::new();
        let schema = serde_json::json!({"type": "object"});
        cache.get_or_create("Read", &schema).expect("compile");
        assert!(cache.contains("Read"));
        assert!(!cache.contains("Write"));

        // Second call should return the cached validator
        cache.get_or_create("Read", &schema).expect("cached");
    }

    #[test]
    fn should_list_known_arguments_when_an_additional_property_is_rejected() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": { "pattern": { "type": "string" }, "path": { "type": "string" } },
            "additionalProperties": false
        });
        let validator = ToolArgsValidator::new(&schema).expect("valid schema");
        let err = validator
            .validate(&serde_json::json!({ "pattern": "x", "count_matches": true }))
            .unwrap_err();
        assert!(
            err.contains("count_matches"),
            "should name the offending key: {err}"
        );
        assert!(
            err.contains("pattern") && err.contains("path"),
            "should list known keys: {err}"
        );
    }
}
