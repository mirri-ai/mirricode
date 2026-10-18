use mirri_proto::wire_op_schema;

// These tests use MinimalWireOp (the small 4-variant enum in schema.rs)
// to verify schemars JSON Schema generation. The full 48-variant WireOp
// lives in wire.rs and is tested in wire_op_tests.rs.

#[test]
fn should_contain_variant_tags_as_const_values_when_generating_schema() {
    let schema = wire_op_schema();
    let schema_str = serde_json::to_string_pretty(&schema).unwrap();
    eprintln!("Generated schema:\n{schema_str}");

    // schemars with #[serde(tag = "type")] should generate a schema with sub-schemas.
    // The exact structure depends on the schemars version, but the renamed
    // variant tags must appear as const values somewhere in the schema.
    let schema_str = schema_str.as_str();
    assert!(
        schema_str.contains("user_message"),
        "schema should contain 'user_message' variant"
    );
    assert!(
        schema_str.contains("assistant_message"),
        "schema should contain 'assistant_message' variant"
    );
    assert!(
        schema_str.contains("tool_call"),
        "schema should contain 'tool_call' variant"
    );
    assert!(
        schema_str.contains("tool_result"),
        "schema should contain 'tool_result' variant"
    );
}

#[test]
fn should_use_type_tag_as_discriminator_when_serializing_minimal_wire_op() {
    use mirri_proto::MinimalWireOp;

    let op = MinimalWireOp::UserMessage {
        content: "hello".to_string(),
        attachments: None,
    };

    let json_str = serde_json::to_string(&op).unwrap();
    let json_val: serde_json::Value = serde_json::from_str(&json_str).unwrap();

    assert_eq!(json_val["type"], "user_message");
    assert_eq!(json_val["content"], "hello");
    assert!(json_val.get("attachments").is_none());
}

#[test]
fn should_deserialize_correctly_when_parsing_ts_format_json() {
    use mirri_proto::MinimalWireOp;

    let ts_json =
        r#"{"type":"tool_result","call_id":"call-1","result":{"ok":true},"truncated":false}"#;
    let op: MinimalWireOp = serde_json::from_str(ts_json).unwrap();

    match op {
        MinimalWireOp::ToolResult {
            call_id,
            result,
            truncated,
        } => {
            assert_eq!(call_id, "call-1");
            assert_eq!(result["ok"], true);
            assert!(!truncated);
        }
        _ => panic!("expected ToolResult"),
    }
}

#[test]
fn should_default_missing_optional_fields_when_deserializing() {
    use mirri_proto::MinimalWireOp;

    let ts_json = r#"{"type":"tool_result","call_id":"call-1","result":{"ok":true}}"#;
    let op: MinimalWireOp = serde_json::from_str(ts_json).unwrap();

    match op {
        MinimalWireOp::ToolResult { truncated, .. } => {
            assert!(!truncated, "missing truncated should default to false");
        }
        _ => panic!("expected ToolResult"),
    }
}

#[test]
fn should_preserve_all_fields_when_roundtripping_minimal_wire_op() {
    use mirri_proto::{MinimalWireOp, TokenUsage};

    let original = MinimalWireOp::AssistantMessage {
        content: "I'll help you.".to_string(),
        model: "claude-sonnet-4-20250514".to_string(),
        usage: Some(TokenUsage {
            input_tokens: 100,
            output_tokens: 50,
            cache_read_tokens: Some(200),
            cache_write_tokens: None,
        }),
    };

    let json = serde_json::to_string(&original).unwrap();
    let deserialized: MinimalWireOp = serde_json::from_str(&json).unwrap();

    match deserialized {
        MinimalWireOp::AssistantMessage {
            content,
            model,
            usage,
        } => {
            assert_eq!(content, "I'll help you.");
            assert_eq!(model, "claude-sonnet-4-20250514");
            let usage = usage.unwrap();
            assert_eq!(usage.input_tokens, 100);
            assert_eq!(usage.output_tokens, 50);
            assert_eq!(usage.cache_read_tokens, Some(200));
            assert!(usage.cache_write_tokens.is_none());
        }
        _ => panic!("expected AssistantMessage"),
    }
}
