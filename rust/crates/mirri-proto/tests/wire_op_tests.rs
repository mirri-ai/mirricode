use mirri_proto::WireOp;
use serde_json::json;

#[test]
fn should_serialize_with_type_tag_when_converting_metadata_op_to_json() {
    let op = WireOp::Metadata {
        protocol_version: Some("1.5".to_string()),
        created_at: Some(1234567890),
    };
    let json = serde_json::to_value(&op).unwrap();
    assert_eq!(json["type"], "metadata");
    assert_eq!(json["protocol_version"], "1.5");
}

#[test]
fn should_serialize_with_type_tag_when_converting_context_append_message_to_json() {
    let op = WireOp::ContextAppendMessage {
        message: json!({"role": "user", "content": [{"type": "text", "text": "hello"}]}),
    };
    let json = serde_json::to_value(&op).unwrap();
    assert_eq!(json["type"], "context.append_message");
    assert_eq!(json["message"]["role"], "user");
}

#[test]
fn should_preserve_fields_when_roundtripping_goal_create() {
    let op = WireOp::GoalCreate {
        goal_id: "g1".to_string(),
        objective: "test objective".to_string(),
        completion_criterion: None,
        status: Some("active".to_string()),
        actor: Some("user".to_string()),
        budget_limits: None,
    };
    let json_str = serde_json::to_string(&op).unwrap();
    let deserialized: WireOp = serde_json::from_str(&json_str).unwrap();

    match deserialized {
        WireOp::GoalCreate {
            goal_id,
            objective,
            status,
            ..
        } => {
            assert_eq!(goal_id, "g1");
            assert_eq!(objective, "test objective");
            assert_eq!(status, Some("active".to_string()));
        }
        _ => panic!("expected GoalCreate"),
    }
}

#[test]
fn should_preserve_variant_when_roundtripping_turn_prompt() {
    let op = WireOp::TurnPrompt {
        input: json!([{"type": "text", "text": "do something"}]),
        origin: json!({"kind": "user"}),
    };
    let json = serde_json::to_string(&op).unwrap();
    let deserialized: WireOp = serde_json::from_str(&json).unwrap();
    assert!(matches!(deserialized, WireOp::TurnPrompt { .. }));
}

#[test]
fn should_serialize_with_type_tag_when_converting_permission_set_mode_to_json() {
    let op = WireOp::PermissionSetMode {
        mode: "auto".to_string(),
    };
    let json = serde_json::to_value(&op).unwrap();
    assert_eq!(json["type"], "permission.set_mode");
    assert_eq!(json["mode"], "auto");
}

#[test]
fn should_serialize_as_typed_empty_object_when_converting_context_clear_to_json() {
    let op = WireOp::ContextClear {};
    let json = serde_json::to_value(&op).unwrap();
    assert_eq!(json["type"], "context.clear");
}

#[test]
fn should_deserialize_to_unknown_variant_when_type_tag_is_unrecognized() {
    let unknown_json = r#"{"type":"some_future_op_v2","data":"whatever","time":12345}"#;
    let op: WireOp = serde_json::from_str(unknown_json).unwrap();
    assert!(matches!(op, WireOp::Unknown));
}

#[test]
fn should_deserialize_to_unknown_variant_when_payload_is_complex() {
    let json = r#"{"type":"future.new_op","nested":{"a":1},"array":[1,2,3],"time":999}"#;
    let op: WireOp = serde_json::from_str(json).unwrap();
    assert!(matches!(op, WireOp::Unknown));
}

#[test]
fn should_ignore_time_field_when_deserializing_variant_without_time_field() {
    // Real TS records have a `time` field that Rust variants don't capture
    let json = r#"{"type":"context.clear","time":1234567890}"#;
    let op: WireOp = serde_json::from_str(json).unwrap();
    assert!(matches!(op, WireOp::ContextClear {}));
}

#[test]
fn should_deserialize_partial_fields_when_some_goal_update_fields_are_omitted() {
    let json = r#"{"type":"goal.update","status":"paused","reason":"user requested","time":123}"#;
    let op: WireOp = serde_json::from_str(json).unwrap();
    match op {
        WireOp::GoalUpdate { status, reason, .. } => {
            assert_eq!(status, Some("paused".to_string()));
            assert_eq!(reason, Some("user requested".to_string()));
        }
        _ => panic!("expected GoalUpdate"),
    }
}

#[test]
fn should_roundtrip_without_error_when_serializing_all_unit_variant_op_types() {
    // Test that every variant can serialize and deserialize
    let ops = vec![
        WireOp::Metadata {
            protocol_version: Some("1.5".to_string()),
            created_at: Some(0),
        },
        WireOp::ContextClear {},
        WireOp::GoalClear {},
        WireOp::Forked {},
        WireOp::ToolsResetActiveTools {},
        WireOp::FullCompactionCancel {},
        WireOp::FullCompactionComplete {},
        WireOp::SwarmModeExit {},
    ];

    for op in &ops {
        let json = serde_json::to_string(op).unwrap();
        let _deserialized: WireOp = serde_json::from_str(&json).unwrap();
        // Just verify it doesn't panic — we can't easily compare enums with Value fields
    }
}

#[test]
fn should_serialize_with_reason_when_converting_request_header_changed_to_json() {
    let op = WireOp::RequestHeaderChanged {
        reason: "tools_changed".to_string(),
    };
    let json = serde_json::to_value(&op).unwrap();
    assert_eq!(json["type"], "request.header.changed");
    assert_eq!(json["reason"], "tools_changed");
}

#[test]
fn should_preserve_reason_when_roundtripping_request_header_changed() {
    let op = WireOp::RequestHeaderChanged {
        reason: "compaction_applied".to_string(),
    };
    let json_str = serde_json::to_string(&op).unwrap();
    let deserialized: WireOp = serde_json::from_str(&json_str).unwrap();
    match deserialized {
        WireOp::RequestHeaderChanged { reason } => assert_eq!(reason, "compaction_applied"),
        other => panic!("expected RequestHeaderChanged, got {other:?}"),
    }
}

// ── ContextApplyCompaction negative deserialization contract ────────────────
//
// These tests pin the record's rejection/tolerance contract explicitly:
// serde already behaves this way by construction (unsigned integer fields
// reject negatives, required fields reject absence, internally-tagged
// enums ignore unknown keys), and these cases lock it in so future drift
// in either direction fails loudly.

/// Builds a minimal valid apply-compaction record as JSON, then overrides
/// `field` with `value` (so each negative case mutates exactly one field).
/// `Value`'s `IndexMut` inserts the key when absent.
fn apply_compaction_json(field: &str, value: serde_json::Value) -> serde_json::Value {
    let mut record = json!({
        "type": "context.apply_compaction",
        "summary": "compacted context",
        "compactedCount": 10,
        "retainedTailCount": 4,
        "keptUserMessageCount": 3,
        "firstReplacedEntrySeq": 3,
        "lastReplacedEntrySeq": 17,
        "droppedEntrySeqs": [5, 7]
    });
    record[field] = value;
    record
}

#[test]
fn should_reject_apply_record_when_counts_are_negative() {
    // Given apply records whose count fields are negative — each count is an
    // unsigned integer on the Rust side, so a negative must surface as a
    // serde error (not a panic, not a silent clamp to zero)
    let negative_counts: &[(&str, serde_json::Value)] = &[
        ("compactedCount", json!(-1)),
        ("retainedTailCount", json!(-3)),
        ("keptUserMessageCount", json!(-2)),
        ("keptHeadUserMessageCount", json!(-1)),
        ("droppedCount", json!(-5)),
    ];

    for (field, value) in negative_counts {
        // When deserialized
        let result: Result<WireOp, _> =
            serde_json::from_value(apply_compaction_json(field, value.clone()));
        // Then it is rejected with an error (a silent clamp would have
        // produced an `Ok` with the count zeroed — the assert message names
        // the field so a failure points at the culprit immediately)
        assert!(
            result.is_err(),
            "negative {field} must be rejected, was silently accepted"
        );
    }
}

#[test]
fn should_reject_apply_record_when_entry_seq_fields_have_wrong_type() {
    // Given apply records whose ADR-007 entry-seq provenance fields carry a
    // string or a float instead of the unsigned integer the wire contract
    // specifies
    let wrong_types: &[(&str, serde_json::Value)] = &[
        ("firstReplacedEntrySeq", json!("12")),
        ("firstReplacedEntrySeq", json!(12.5)),
        ("lastReplacedEntrySeq", json!("17")),
        ("lastReplacedEntrySeq", json!(17.5)),
        // a string inside the dropped-entry list is equally rejected
        ("droppedEntrySeqs", json!([5, "7"])),
    ];

    for (field, value) in wrong_types {
        // When deserialized
        let result: Result<WireOp, _> =
            serde_json::from_value(apply_compaction_json(field, value.clone()));
        // Then it is rejected — entry seqs are u64 on the wire, so neither
        // the JSON string nor the float form is coerced
        assert!(
            result.is_err(),
            "wrong-typed {field} must be rejected, was silently coerced"
        );
    }
}

#[test]
fn should_reject_apply_record_when_summary_is_missing() {
    // Given an apply record whose required `summary` field is absent
    let mut record = apply_compaction_json("compactedCount", json!(10));
    record.as_object_mut().unwrap().remove("summary");

    // When deserialized
    let result: Result<WireOp, _> = serde_json::from_value(record);

    // Then it is rejected: `summary` has no serde default, so a record
    // without it cannot become an apply op (an op with an empty-string
    // summary silently substituted would corrupt replay semantics)
    let err = result.expect_err("missing summary must be rejected");
    assert!(
        err.to_string().contains("missing field `summary`"),
        "expected a missing-field error, got: {err}"
    );
}

#[test]
fn should_tolerate_unknown_optional_fields_when_records_come_from_newer_writers() {
    // Given an apply record written by a newer writer that carries fields
    // this kernel does not know about
    let record = json!({
        "type": "context.apply_compaction",
        "summary": "compacted context",
        "compactedCount": 10,
        "firstReplacedEntrySeq": 3,
        "lastReplacedEntrySeq": 17,
        "droppedEntrySeqs": [5, 7],
        "someFutureDialectField": {"nested": ["payload"]},
        "anotherFutureField": 42,
        "time": 1234567890
    });

    // When deserialized
    let op: WireOp = serde_json::from_value(record)
        .expect("unknown fields from a newer writer must be tolerated");

    // Then the known fields are intact and the unknown ones dropped —
    // the forward-compatibility half of the contract, pinned so a future
    // `deny_unknown_fields` regression fails here instead of in production
    match op {
        WireOp::ContextApplyCompaction {
            summary,
            compacted_count,
            first_replaced_entry_seq,
            last_replaced_entry_seq,
            dropped_entry_seqs,
            ..
        } => {
            assert_eq!(summary, "compacted context");
            assert_eq!(compacted_count, 10);
            assert_eq!(first_replaced_entry_seq, Some(3));
            assert_eq!(last_replaced_entry_seq, Some(17));
            assert_eq!(dropped_entry_seqs, Some(vec![5, 7]));
        }
        other => panic!("expected ContextApplyCompaction, got {other:?}"),
    }
}
