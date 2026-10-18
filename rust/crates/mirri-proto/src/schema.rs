use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Minimal wire op type for schema compatibility testing.
///
/// This is a small subset of the full `WireOp` (defined in `wire.rs`)
/// used to verify that `schemars` generates a correct JSON Schema
/// discriminated union for internally-tagged enums. The full
/// `WireOp` with every TS op type lives in `wire.rs`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type")]
pub enum MinimalWireOp {
    #[serde(rename = "user_message")]
    UserMessage {
        content: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attachments: Option<Vec<String>>,
    },
    #[serde(rename = "assistant_message")]
    AssistantMessage {
        content: String,
        model: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<TokenUsage>,
    },
    #[serde(rename = "tool_call")]
    ToolCall {
        tool: String,
        args: serde_json::Value,
    },
    #[serde(rename = "tool_result")]
    ToolResult {
        call_id: String,
        result: serde_json::Value,
        #[serde(default)]
        truncated: bool,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_tokens: Option<u64>,
}

/// Generate the JSON Schema for `MinimalWireOp`.
///
/// The returned schema should contain `oneOf` (or equivalent) sub-schemas
/// whose `type` property carries a `const` value matching each variant's
/// `#[serde(rename = ...)]`. This is the shape that `zod-to-json-schema`
/// must produce on the TypeScript side for the two to interoperate.
pub fn wire_op_schema() -> serde_json::Value {
    let schema: schemars::Schema = schemars::schema_for!(MinimalWireOp);
    serde_json::to_value(&schema).expect("schema is always serializable")
}
