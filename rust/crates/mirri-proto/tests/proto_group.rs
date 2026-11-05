// layer: T1
// Domain group binary consolidated from per-file test binaries
// per 16-test-execution-strategy.md §4.4 (pure file moves).
mod events_tests {
use mirri_proto::DomainEvent;

#[test]
fn should_include_type_tag_when_serializing_domain_event() {
    let event = DomainEvent::TurnStarted { turn_id: 1 };
    let json = serde_json::to_value(&event).unwrap();
    assert_eq!(json["type"], "turn_started");
    assert_eq!(json["turn_id"], 1);
}

#[test]
fn should_deserialize_correct_variant_when_parsing_json() {
    let json = r#"{"type":"goal_started","objective":"test"}"#;
    let event: DomainEvent = serde_json::from_str(json).unwrap();
    assert!(matches!(event, DomainEvent::GoalStarted { objective } if objective == "test"));
}

#[test]
fn should_preserve_all_fields_when_roundtripping_domain_event() {
    let event = DomainEvent::MessageAdded {
        role: "user".to_string(),
        content_preview: "hello".to_string(),
    };
    let json = serde_json::to_string(&event).unwrap();
    let deserialized: DomainEvent = serde_json::from_str(&json).unwrap();
    assert!(
        matches!(deserialized, DomainEvent::MessageAdded { role, content_preview } if role == "user" && content_preview == "hello")
    );
}

#[test]
fn should_deserialize_to_unknown_when_type_tag_is_unrecognized() {
    let json = r#"{"type":"some_future_event","data":"x"}"#;
    let event: DomainEvent = serde_json::from_str(json).unwrap();
    assert!(matches!(event, DomainEvent::Unknown));
}

#[test]
fn should_serialize_and_deserialize_when_event_variant_has_no_fields() {
    let events = vec![
        DomainEvent::GoalCleared {},
        DomainEvent::ContextCleared {},
        DomainEvent::GoalCompleted {},
        DomainEvent::GoalResumed {},
        DomainEvent::PlanModeExited {},
    ];
    for event in &events {
        let json = serde_json::to_string(event).unwrap();
        let _: DomainEvent = serde_json::from_str(&json).unwrap();
    }
}

}

mod proptest_reducer {
//! Property-based tests for the wire-op reducer.
//!
//! These tests use `proptest` to generate random sequences of wire ops and
//! verify that the reducer maintains fundamental invariants:
//!
//! 1. **Purity**: reducing the same op on the same state always produces the
//!    same result (no hidden state or side effects).
//! 2. **Idempotency of Metadata**: reducing `Metadata` never changes state.
//! 3. **Monotonic turn counter**: `TurnPrompt` always increments the counter.
//! 4. **Unknown safety**: reducing `Unknown` never panics and never changes
//!    state.
//! 5. **ContextClear empties conversation**: after `ContextClear`, the
//!    conversation is empty regardless of prior state.
//! 6. **Message append preserves order**: `ContextAppendMessage` appends to
//!    the end of the message list.
//! 7. **Serialisation round-trip**: any `WireOp` serialised to JSON and
//!    deserialised back is identical (except `Unknown`).
//! 8. **Undo truncation**: `ContextUndo` removes exactly `count` messages
//!    when `count <= len`, or empties when `count > len`.
//! 9. **Goal lifecycle**: `GoalCreate` sets goal, `GoalClear` removes it,
//!    `GoalUpdate` modifies fields.
//! 10. **Plan mode lifecycle**: `PlanModeEnter` sets plan_mode,
//!     `PlanModeExit` clears it.
//! 11. **Compaction**: `ContextApplyCompaction` (TS legacy record — no
//!     counts) rebuilds the conversation as the summary message alone when
//!     compactedCount covers the folded history, and sets the summary.
//! 12. **No-panic on mixed op sequences**: random sequences of all
//!     implemented ops never panic.

use mirri_proto::{AgentState, WireOp, reduce};
use proptest::prelude::*;

/// Strategy for generating simple JSON values suitable for message content.
fn arb_json_value() -> impl Strategy<Value = serde_json::Value> {
    prop_oneof![
        Just(serde_json::json!("hello")),
        Just(serde_json::json!(42)),
        Just(serde_json::json!({"role": "user", "content": "test"})),
        Just(serde_json::json!([1, 2, 3])),
        Just(serde_json::json!(null)),
    ]
}

/// Strategy for generating wire ops that the reducer has explicit match arms
/// for (context, turn, goal, plan, compaction, metadata, unknown).
fn arb_wire_op() -> impl Strategy<Value = WireOp> {
    prop_oneof![
        Just(WireOp::Metadata {
            protocol_version: Some("1.5".to_string()),
            created_at: Some(0),
        }),
        arb_json_value().prop_map(|message| WireOp::ContextAppendMessage { message }),
        arb_json_value().prop_map(|event| WireOp::ContextAppendLoopEvent { event }),
        Just(WireOp::ContextClear {}),
        // Undo with small count (0..5)
        (0u32..5).prop_map(|count| WireOp::ContextUndo { count }),
        // Compaction
        Just(WireOp::ContextApplyCompaction {
            summary: "compacted".to_string(),
            compacted_count: 3,
            tokens_before: Some(1000),
            tokens_after: Some(500),
            context_summary: None,
            dropped_count: None,
            retained_tail_count: None,
            kept_user_message_count: None,
            kept_head_user_message_count: None,
            elision_message: None,
            first_replaced_entry_seq: None,
            last_replaced_entry_seq: None,
            dropped_entry_seqs: None,
        }),
        Just(WireOp::TurnPrompt {
            input: serde_json::json!("test"),
            origin: serde_json::json!("user"),
        }),
        // Goal ops
        Just(WireOp::GoalCreate {
            goal_id: "g1".to_string(),
            objective: "test goal".to_string(),
            completion_criterion: Some("done when x".to_string()),
            status: Some("active".to_string()),
            actor: None,
            budget_limits: None,
        }),
        Just(WireOp::GoalUpdate {
            goal_id: Some("g1".to_string()),
            status: Some("paused".to_string()),
            reason: None,
            turns_used: Some(5),
            tokens_used: Some(1000),
            wall_clock_ms: None,
            budget_limits: None,
            actor: None,
        }),
        Just(WireOp::GoalClear {}),
        // Plan mode ops
        Just(WireOp::PlanModeEnter {
            id: "plan-1".to_string(),
            plan_text: None,
        }),
        Just(WireOp::PlanModeExit { id: None }),
        Just(WireOp::PlanModeCancel { id: None }),
        Just(WireOp::FullCompactionBegin {
            data: serde_json::json!({"summary": "full"}),
        }),
        Just(WireOp::FullCompactionCancel {}),
        Just(WireOp::FullCompactionComplete {}),
        Just(WireOp::Unknown),
    ]
}

/// Compare two AgentStates via JSON serialisation (AgentState does not derive
/// PartialEq, so we compare the canonical JSON form).
fn state_json(s: &AgentState) -> String {
    serde_json::to_string(s).expect("state serialises")
}

proptest! {
    /// Reducing the same op on the same state always produces the same output.
    #[test]
    fn should_produce_identical_results_when_reducing_same_op_twice(
        op in arb_wire_op()
    ) {
        let state = AgentState::new();
        let (s1, e1) = reduce(&state, &op);
        let (s2, e2) = reduce(&state, &op);
        prop_assert_eq!(state_json(&s1), state_json(&s2));
        prop_assert_eq!(e1.len(), e2.len());
    }

    /// Reducing Metadata never changes the agent state.
    #[test]
    fn should_not_change_state_when_reducing_metadata(
        _ in 0..10u8
    ) {
        let state = AgentState::new();
        let op = WireOp::Metadata {
            protocol_version: Some("1.5".to_string()),
            created_at: Some(12345),
        };
        let (new_state, _) = reduce(&state, &op);
        prop_assert_eq!(state_json(&state), state_json(&new_state));
    }

    /// Reducing Unknown never changes the agent state and never panics.
    #[test]
    fn should_not_change_state_or_panic_when_reducing_unknown(
        _ in 0..10u8
    ) {
        let state = AgentState::new();
        let (new_state, events) = reduce(&state, &WireOp::Unknown);
        prop_assert_eq!(state_json(&state), state_json(&new_state));
        prop_assert!(events.is_empty());
    }

    /// TurnPrompt always increments the turn counter by exactly 1.
    #[test]
    fn should_increment_turn_counter_when_reducing_turn_prompt(
        initial_turns in 0u64..100
    ) {
        let mut state = AgentState::new();
        state.turn_counter = initial_turns;
        let (new_state, _) = reduce(&state, &WireOp::TurnPrompt {
            input: serde_json::json!("go"),
            origin: serde_json::json!("user"),
        });
        prop_assert_eq!(new_state.turn_counter, initial_turns + 1);
    }

    /// ContextClear always results in an empty conversation.
    #[test]
    fn should_empty_conversation_when_reducing_context_clear(
        messages in prop::collection::vec(arb_json_value(), 0..10)
    ) {
        let mut state = AgentState::new();
        for msg in &messages {
            let (s, _) = reduce(&state, &WireOp::ContextAppendMessage { message: msg.clone() });
            state = s;
        }
        // Verify messages were added
        prop_assert!(!state.conversation.messages.is_empty() || messages.is_empty());

        let (cleared, _) = reduce(&state, &WireOp::ContextClear {});
        prop_assert!(cleared.conversation.messages.is_empty());
    }

    /// ContextAppendMessage always appends to the end of the message list,
    /// preserving insertion order.
    #[test]
    fn should_preserve_insertion_order_when_appending_messages(
        messages in prop::collection::vec(arb_json_value(), 1..20)
    ) {
        let mut state = AgentState::new();
        for msg in &messages {
            let (s, _) = reduce(&state, &WireOp::ContextAppendMessage { message: msg.clone() });
            state = s;
        }
        prop_assert_eq!(state.conversation.messages.len(), messages.len());
        // The last message should be the last one we appended
        let last = state.conversation.messages.last().unwrap();
        let expected = messages.last().unwrap();
        prop_assert_eq!(last, expected);
    }

    /// Reducing a sequence of ops is deterministic: replaying the same
    /// sequence from the same starting state always reaches the same end
    /// state.
    #[test]
    fn should_reach_same_state_when_replaying_same_op_sequence(
        ops in prop::collection::vec(arb_wire_op(), 0..30)
    ) {
        let state = AgentState::new();

        let mut s1 = state.clone();
        for op in &ops {
            let (s, _) = reduce(&s1, op);
            s1 = s;
        }

        let mut s2 = state.clone();
        for op in &ops {
            let (s, _) = reduce(&s2, op);
            s2 = s;
        }

        prop_assert_eq!(state_json(&s1), state_json(&s2));
    }

    /// Serialisation round-trip: any WireOp serialised to JSON and
    /// deserialised back should be equal to the original (except Unknown,
    /// which absorbs all data).
    #[test]
    fn should_survive_json_roundtrip_when_serializing_wire_op(
        op in arb_wire_op()
    ) {
        if matches!(op, WireOp::Unknown) {
            // Unknown is a catch-all; skip round-trip verification
            return Ok(());
        }
        let json = serde_json::to_string(&op).expect("serialize");
        let parsed: WireOp = serde_json::from_str(&json).expect("deserialize");
        let reparsed = serde_json::to_string(&parsed).expect("reserialize");
        prop_assert_eq!(json, reparsed);
    }

    /// ContextUndo truncates messages when `count <= len`.
    /// When `count > len`, the reducer leaves the messages unchanged
    /// (no-op guard: `if undo_count <= len`).
    #[test]
    fn should_truncate_messages_when_reducing_context_undo(
        msg_count in 1u32..20,
        undo_count in 0u32..25,
    ) {
        let mut state = AgentState::new();
        for i in 0..msg_count {
            let (s, _) = reduce(&state, &WireOp::ContextAppendMessage {
                message: serde_json::json!({"i": i}),
            });
            state = s;
        }
        let original_len = state.conversation.messages.len() as u32;
        let (new_state, _) = reduce(&state, &WireOp::ContextUndo { count: undo_count });

        if undo_count <= original_len {
            prop_assert_eq!(
                new_state.conversation.messages.len() as u32,
                original_len - undo_count
            );
        } else {
            // Reducer guards with `if undo_count <= len`, so messages
            // are left unchanged when count exceeds the list length.
            prop_assert_eq!(
                new_state.conversation.messages.len() as u32,
                original_len
            );
        }
    }

    /// GoalCreate sets the goal, GoalClear removes it.
    #[test]
    fn should_set_and_clear_goal_when_reducing_goal_ops(
        _ in 0..5u8
    ) {
        let state = AgentState::new();
        prop_assert!(state.goal.is_none());

        // Create
        let (created, _) = reduce(&state, &WireOp::GoalCreate {
            goal_id: "g1".to_string(),
            objective: "test".to_string(),
            completion_criterion: None,
            status: Some("active".to_string()),
            actor: None,
            budget_limits: None,
        });
        prop_assert!(created.goal.is_some());
        prop_assert_eq!(&created.goal.as_ref().unwrap().goal_id, "g1");
        prop_assert_eq!(&created.goal.as_ref().unwrap().status, "active");

        // Clear
        let (cleared, _) = reduce(&created, &WireOp::GoalClear {});
        prop_assert!(cleared.goal.is_none());
    }

    /// GoalUpdate modifies the goal status without removing it.
    #[test]
    fn should_update_goal_status_when_reducing_goal_update(
        new_status in "[a-z]{1,10}"
    ) {
        let state = AgentState::new();
        let (created, _) = reduce(&state, &WireOp::GoalCreate {
            goal_id: "g1".to_string(),
            objective: "test".to_string(),
            completion_criterion: None,
            status: Some("active".to_string()),
            actor: None,
            budget_limits: None,
        });

        let (updated, _) = reduce(&created, &WireOp::GoalUpdate {
            goal_id: Some("g1".to_string()),
            status: Some(new_status.clone()),
            reason: None,
            turns_used: Some(5),
            tokens_used: Some(1000),
            wall_clock_ms: None,
            budget_limits: None,
            actor: None,
        });

        prop_assert!(updated.goal.is_some());
        prop_assert_eq!(&updated.goal.as_ref().unwrap().status, &new_status);
        prop_assert_eq!(updated.goal.as_ref().unwrap().turns_used, Some(5));
    }

    /// PlanModeEnter sets plan_mode, PlanModeExit clears it.
    #[test]
    fn should_set_and_clear_plan_mode_when_reducing_plan_ops(
        plan_id in "[a-z0-9]{1,10}"
    ) {
        let state = AgentState::new();
        prop_assert!(state.plan_mode.is_none());

        // Enter
        let (entered, _) = reduce(&state, &WireOp::PlanModeEnter {
            id: plan_id.clone(),
            plan_text: None,
        });
        prop_assert!(entered.plan_mode.is_some());
        prop_assert_eq!(&entered.plan_mode.as_ref().unwrap().plan_id, &plan_id);

        // Exit
        let (exited, _) = reduce(&entered, &WireOp::PlanModeExit { id: None });
        prop_assert!(exited.plan_mode.is_none());
    }

    /// ContextApplyCompaction (TS legacy record — no counts) rebuilds the
    /// conversation as the summary message alone when compactedCount
    /// covers the whole folded history, and sets the summary text.
    #[test]
    fn should_rebuild_summary_alone_when_reducing_compaction_over_whole_history(
        messages in prop::collection::vec(arb_json_value(), 1..10)
    ) {
        let mut state = AgentState::new();
        for msg in &messages {
            let (s, _) = reduce(&state, &WireOp::ContextAppendMessage { message: msg.clone() });
            state = s;
        }
        prop_assert!(!state.conversation.messages.is_empty());

        let summary = "compacted summary".to_string();
        let (compacted, _) = reduce(&state, &WireOp::ContextApplyCompaction {
            summary: summary.clone(),
            compacted_count: messages.len() as u32,
            tokens_before: None,
            tokens_after: None,
            context_summary: None,
            dropped_count: None,
            retained_tail_count: None,
            kept_user_message_count: None,
            kept_head_user_message_count: None,
            elision_message: None,
            first_replaced_entry_seq: None,
            last_replaced_entry_seq: None,
            dropped_entry_seqs: None,
        });

        // The TS legacy rebuild keeps folded history from index
        // compactedCount on — compactedCount covering the whole history
        // leaves the summary message alone (transcript.ts:273's guard
        // complement).
        prop_assert_eq!(compacted.conversation.messages.len(), 1);
        prop_assert_eq!(
            compacted.conversation.messages[0]["origin"]["kind"].as_str(),
            Some("compaction_summary")
        );
        prop_assert_eq!(
            compacted.conversation.messages[0]["content"][0]["text"].as_str(),
            Some("compacted summary")
        );
        prop_assert!(compacted.conversation.loop_events.is_empty());
        prop_assert_eq!(compacted.conversation.summary, Some(summary));
    }

    /// No-panic: random sequences of all implemented ops (including undo,
    /// goal, plan, compaction) never cause a panic.
    #[test]
    fn should_not_panic_when_reducing_mixed_op_sequences(
        ops in prop::collection::vec(arb_wire_op(), 0..50)
    ) {
        let mut state = AgentState::new();
        for op in &ops {
            let (s, _) = reduce(&state, op);
            state = s;
        }
        // If we reach here without panicking, the test passes.
    }
}

}

mod reducer_tests {
use mirri_proto::{AgentState, DomainEvent, GoalState, WireOp, apply_with_limits, reduce};
use serde_json::json;

#[test]
fn should_add_message_to_conversation_when_reducing_context_append_message() {
    let initial = AgentState::new();
    let op = WireOp::ContextAppendMessage {
        message: json!({"role": "user", "content": [{"type": "text", "text": "hello"}]}),
    };

    let (state, events) = reduce(&initial, &op);

    assert_eq!(state.conversation.messages.len(), 1);
    assert_eq!(state.conversation.messages[0]["role"], "user");
    assert_eq!(
        state.conversation.messages[0]["content"][0]["text"],
        "hello"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, DomainEvent::MessageAdded { .. }))
    );
}

#[test]
fn should_store_loop_event_raw_when_reducing_context_append_loop_event() {
    let initial = AgentState::new();
    let op = WireOp::ContextAppendLoopEvent {
        event: json!({"type": "step.begin", "uuid": "step-1", "turnId": "turn-1"}),
    };

    let (state, _) = reduce(&initial, &op);

    assert_eq!(state.conversation.loop_events.len(), 1);
    assert_eq!(state.conversation.loop_events[0]["type"], "step.begin");
}

#[test]
fn should_empty_conversation_when_reducing_context_clear() {
    let mut state = AgentState::new();
    state
        .conversation
        .messages
        .push(json!({"role": "user", "content": []}));
    state
        .conversation
        .loop_events
        .push(json!({"type": "step.begin"}));

    let (state, _) = reduce(&state, &WireOp::ContextClear {});

    assert_eq!(state.conversation.messages.len(), 0);
    assert_eq!(state.conversation.loop_events.len(), 0);
}

#[test]
fn should_increment_turn_counter_when_reducing_turn_prompt() {
    let initial = AgentState::new();
    assert_eq!(initial.turn_counter, 0);

    let op = WireOp::TurnPrompt {
        input: json!([{"type": "text", "text": "do something"}]),
        origin: json!({"kind": "user"}),
    };
    let (state, _) = reduce(&initial, &op);

    assert_eq!(state.turn_counter, 1);
}

#[test]
fn should_set_goal_state_when_reducing_goal_create() {
    let initial = AgentState::new();
    assert!(initial.goal.is_none());

    let op = WireOp::GoalCreate {
        goal_id: "g1".to_string(),
        objective: "refactor auth".to_string(),
        completion_criterion: Some("all tests pass".to_string()),
        status: Some("active".to_string()),
        actor: Some("user".to_string()),
        budget_limits: None,
    };
    let (state, _) = reduce(&initial, &op);

    let goal = state.goal.expect("goal should be set");
    assert_eq!(goal.goal_id, "g1");
    assert_eq!(goal.objective, "refactor auth");
    assert_eq!(goal.status, "active");
}

#[test]
fn should_change_goal_status_when_reducing_goal_update() {
    let mut state = AgentState::new();
    state.goal = Some(GoalState {
        goal_id: "g1".to_string(),
        objective: "test".to_string(),
        completion_criterion: None,
        status: "active".to_string(),
        turns_used: None,
        tokens_used: None,
        budget_limits: None,
    });

    let op = WireOp::GoalUpdate {
        goal_id: None,
        status: Some("paused".to_string()),
        reason: Some("user requested".to_string()),
        turns_used: None,
        tokens_used: Some(5000),
        wall_clock_ms: None,
        budget_limits: None,
        actor: None,
    };
    let (state, _) = reduce(&state, &op);

    let goal = state.goal.expect("goal should exist");
    assert_eq!(goal.status, "paused");
    assert_eq!(goal.tokens_used, Some(5000));
}

#[test]
fn should_remove_goal_when_reducing_goal_clear() {
    let mut state = AgentState::new();
    state.goal = Some(GoalState {
        goal_id: "g1".to_string(),
        objective: "test".to_string(),
        completion_criterion: None,
        status: "active".to_string(),
        turns_used: None,
        tokens_used: None,
        budget_limits: None,
    });

    let (state, _) = reduce(&state, &WireOp::GoalClear {});

    assert!(state.goal.is_none());
}

#[test]
fn should_update_permission_mode_when_reducing_permission_set_mode() {
    let initial = AgentState::new();
    assert_eq!(initial.permission.mode, "manual");

    let op = WireOp::PermissionSetMode {
        mode: "auto".to_string(),
    };
    let (state, _) = reduce(&initial, &op);

    assert_eq!(state.permission.mode, "auto");
}

#[test]
fn should_derive_session_approval_pattern_when_reducing_session_scoped_approval() {
    // Given a fresh state
    let initial = AgentState::new();

    // When an approval result record arrives whose decision was approved
    // with session scope and carries a session approval rule pattern
    let op = WireOp::PermissionRecordApprovalResult {
        record: json!({
            "turnId": 1,
            "toolCallId": "call-1",
            "toolName": "Bash",
            "action": "execute Bash",
            "sessionApprovalRule": "Bash",
            "result": { "decision": "approved", "scope": "session" },
        }),
    };
    let (state, _) = reduce(&initial, &op);

    // Then the derived pattern list carries the approval rule
    assert_eq!(
        state.permission.session_approval_patterns,
        vec!["Bash".to_string()]
    );
}

#[test]
fn should_not_derive_pattern_when_approval_was_not_session_scoped() {
    // Given a fresh state
    let initial = AgentState::new();

    // When an approval result record arrives that was approved without
    // session scope (a one-shot approval)
    let op = WireOp::PermissionRecordApprovalResult {
        record: json!({
            "turnId": 1,
            "toolCallId": "call-1",
            "toolName": "Bash",
            "action": "execute Bash",
            "sessionApprovalRule": "Bash",
            "result": { "decision": "approved" },
        }),
    };
    let (state, _) = reduce(&initial, &op);

    // Then no pattern is derived
    assert!(state.permission.session_approval_patterns.is_empty());
}

#[test]
fn should_not_duplicate_pattern_when_same_approval_recorded_twice() {
    // Given a state that already derived the Bash session approval
    let initial = AgentState::new();
    let op = WireOp::PermissionRecordApprovalResult {
        record: json!({
            "turnId": 1,
            "toolCallId": "call-1",
            "toolName": "Bash",
            "action": "execute Bash",
            "sessionApprovalRule": "Bash",
            "result": { "decision": "approved", "scope": "session" },
        }),
    };
    let (state, _) = reduce(&initial, &op);

    // When the same approval is recorded again from a later turn
    let op = WireOp::PermissionRecordApprovalResult {
        record: json!({
            "turnId": 2,
            "toolCallId": "call-2",
            "toolName": "Bash",
            "action": "execute Bash",
            "sessionApprovalRule": "Bash",
            "result": { "decision": "approved", "scope": "session" },
        }),
    };
    let (state, _) = reduce(&state, &op);

    // Then the pattern list stays deduplicated
    assert_eq!(
        state.permission.session_approval_patterns,
        vec!["Bash".to_string()]
    );
}

#[test]
fn should_derive_pattern_from_migrated_rule_when_approval_record_has_no_result() {
    // Given a fresh state
    let initial = AgentState::new();

    // When a legacy record arrives whose sessionApprovalRule was derived
    // by the v1.1→v1.2 migration (the { rule, toolName } object shape,
    // carrying no `result` field)
    let op = WireOp::PermissionRecordApprovalResult {
        record: json!({
            "turnId": 1,
            "toolCallId": "call-1",
            "toolName": "bash",
            "action": "allow",
            "sessionApprovalRule": { "rule": "allow", "toolName": "bash" },
        }),
    };
    let (state, _) = reduce(&initial, &op);

    // Then the pattern list carries the tool name of the allow rule
    assert_eq!(
        state.permission.session_approval_patterns,
        vec!["bash".to_string()]
    );
}

#[test]
fn should_not_derive_pattern_when_migrated_rule_denies_the_tool() {
    // Given a fresh state
    let initial = AgentState::new();

    // When a legacy record arrives whose derived sessionApprovalRule is a
    // deny rule (not a session approval to honor on resume)
    let op = WireOp::PermissionRecordApprovalResult {
        record: json!({
            "turnId": 1,
            "toolCallId": "call-1",
            "toolName": "bash",
            "action": "deny",
            "sessionApprovalRule": { "rule": "deny", "toolName": "bash" },
        }),
    };
    let (state, _) = reduce(&initial, &op);

    // Then no pattern is derived
    assert!(state.permission.session_approval_patterns.is_empty());
}

#[test]
fn should_accumulate_token_counts_when_reducing_multiple_usage_records() {
    let mut state = AgentState::new();

    let op1 = WireOp::UsageRecord {
        model: "claude-sonnet-4".to_string(),
        usage: json!({"inputTokens": 100, "outputTokens": 50, "cacheReadTokens": 30, "cacheCreationTokens": 5}),
        usage_scope: Some("turn".to_string()),
    };
    let op2 = WireOp::UsageRecord {
        model: "claude-sonnet-4".to_string(),
        usage: json!({"inputTokens": 200, "outputTokens": 30, "cacheReadTokens": 10, "cacheCreationTokens": 2}),
        usage_scope: None,
    };

    state = reduce(&state, &op1).0;
    state = reduce(&state, &op2).0;

    assert_eq!(state.usage.total_input_tokens, 300);
    assert_eq!(state.usage.total_output_tokens, 80);
    // Then the cache buckets accumulate alongside the input/output totals
    assert_eq!(state.usage.total_cache_read_tokens, 40);
    assert_eq!(state.usage.total_cache_creation_tokens, 7);
}

#[test]
fn should_replace_messages_with_summary_when_reducing_context_apply_compaction() {
    let mut state = AgentState::new();
    state
        .conversation
        .messages
        .push(json!({"role": "user", "content": [{"type": "text", "text": "msg 1"}]}));
    state
        .conversation
        .messages
        .push(json!({"role": "assistant", "content": [{"type": "text", "text": "reply 1"}]}));

    let op = WireOp::ContextApplyCompaction {
        summary: "summary of conversation".to_string(),
        compacted_count: 2,
        tokens_before: Some(1000),
        tokens_after: Some(100),
        context_summary: None,
        dropped_count: None,
        retained_tail_count: None,
        kept_user_message_count: None,
        kept_head_user_message_count: None,
        elision_message: None,
        first_replaced_entry_seq: None,
        last_replaced_entry_seq: None,
        dropped_entry_seqs: None,
    };
    let (state, _) = reduce(&state, &op);

    assert_eq!(
        state.conversation.summary,
        Some("summary of conversation".to_string())
    );
    // TS legacy rebuild (no kept counts): the summary lands as a user
    // message ahead of the folded tail — here compactedCount covers the
    // whole folded history, so the rebuild is the summary alone.
    assert_eq!(state.conversation.messages.len(), 1);
    assert_eq!(
        state.conversation.messages[0]["origin"]["kind"],
        "compaction_summary"
    );
    assert_eq!(
        state.conversation.messages[0]["content"][0]["text"],
        "summary of conversation"
    );
    assert_eq!(state.conversation.loop_events.len(), 0);
}

#[test]
fn should_be_noop_when_reducing_unknown_op() {
    let initial = AgentState::new();
    let op = WireOp::Unknown;

    let (state, events) = reduce(&initial, &op);

    assert_eq!(
        state.conversation.messages.len(),
        initial.conversation.messages.len()
    );
    assert!(events.is_empty());
}

#[test]
fn should_be_noop_when_reducing_metadata_op() {
    let initial = AgentState::new();
    let op = WireOp::Metadata {
        protocol_version: Some("1.5".to_string()),
        created_at: Some(1234567890),
    };

    let (state, events) = reduce(&initial, &op);

    assert_eq!(state.turn_counter, 0);
    assert!(events.is_empty());
}

#[test]
fn should_enter_and_exit_plan_mode_when_reducing_plan_mode_ops() {
    let initial = AgentState::new();
    assert!(initial.plan_mode.is_none());

    let enter = WireOp::PlanModeEnter {
        id: "plan-1".to_string(),
        plan_text: None,
    };
    let (state, _) = reduce(&initial, &enter);
    assert_eq!(state.plan_mode.as_ref().unwrap().plan_id, "plan-1");

    let exit = WireOp::PlanModeExit {
        id: Some("plan-1".to_string()),
    };
    let (state, _) = reduce(&state, &exit);
    assert!(state.plan_mode.is_none());
}

#[test]
fn should_fold_plan_text_from_enter_revision_and_cancel_when_reducing_plan_ops() {
    // Given a fresh state
    let initial = AgentState::new();

    // When a plan_mode.enter carries the plan text (the rust dialect's
    // versioned field), Then the text is folded into the state alongside
    // the plan id.
    let enter = WireOp::PlanModeEnter {
        id: "plan-1".to_string(),
        plan_text: Some("1. Survey the write sides\n2. Add tests".to_string()),
    };
    let (state, _) = reduce(&initial, &enter);
    assert_eq!(
        state.plan_text.as_deref(),
        Some("1. Survey the write sides\n2. Add tests")
    );

    // And a later plan.revision carrying a revised text overwrites it (the
    // latest revision wins — the TS restore source for plan content).
    let revision = WireOp::PlanRevision {
        id: "plan-1".to_string(),
        version: 2,
        path: String::new(),
        sha256: "abc".to_string(),
        bytes: 12,
        plan_text: Some("1. Revised plan".to_string()),
    };
    let (state, _) = reduce(&state, &revision);
    assert_eq!(state.plan_text.as_deref(), Some("1. Revised plan"));

    // And a plan_mode.exit keeps the text (the approved plan stays
    // readable after the exit), while plan_mode.cancel discards it.
    let exit = WireOp::PlanModeExit { id: None };
    let (state, _) = reduce(&state, &exit);
    assert!(state.plan_mode.is_none());
    assert_eq!(state.plan_text.as_deref(), Some("1. Revised plan"));

    let cancel = WireOp::PlanModeCancel { id: None };
    let (state, _) = reduce(&state, &cancel);
    assert!(state.plan_mode.is_none());
    assert_eq!(state.plan_text, None);
}

#[test]
fn should_keep_plan_text_empty_when_enter_carries_no_text() {
    let initial = AgentState::new();
    let enter = WireOp::PlanModeEnter {
        id: "plan-2".to_string(),
        plan_text: None,
    };
    let (state, _) = reduce(&initial, &enter);
    assert_eq!(state.plan_text, None);
    assert_eq!(state.plan_mode.as_ref().unwrap().plan_id, "plan-2");
}

#[test]
fn should_remove_last_n_messages_when_reducing_context_undo() {
    let mut state = AgentState::new();
    for i in 0..5 {
        state.conversation.messages.push(
            json!({"role": "user", "content": [{"type": "text", "text": format!("msg {i}")}]}),
        );
    }

    let (state, _) = reduce(&state, &WireOp::ContextUndo { count: 2 });

    assert_eq!(state.conversation.messages.len(), 3);
    assert_eq!(
        state.conversation.messages[2]["content"][0]["text"],
        "msg 2"
    );
}

#[test]
fn should_not_mutate_input_state_when_reducing_op() {
    let mut state = AgentState::new();
    state
        .conversation
        .messages
        .push(json!({"role": "user", "content": []}));

    let original_message_count = state.conversation.messages.len();

    let op = WireOp::ContextAppendMessage {
        message: json!({"role": "assistant", "content": [{"type": "text", "text": "hi"}]}),
    };
    let (new_state, _) = reduce(&state, &op);

    // Input state must be unchanged.
    assert_eq!(state.conversation.messages.len(), original_message_count);
    // New state must have the additional message.
    assert_eq!(
        new_state.conversation.messages.len(),
        original_message_count + 1
    );
}

#[test]
fn should_accumulate_state_correctly_when_reducing_multiple_ops() {
    let mut state = AgentState::new();

    // Turn 1
    state = reduce(
        &state,
        &WireOp::TurnPrompt {
            input: json!([{"type": "text", "text": "hello"}]),
            origin: json!({"kind": "user"}),
        },
    )
    .0;
    state = reduce(
        &state,
        &WireOp::ContextAppendMessage {
            message: json!({"role": "user", "content": [{"type": "text", "text": "hello"}]}),
        },
    )
    .0;
    state = reduce(
        &state,
        &WireOp::ContextAppendMessage {
            message: json!({"role": "assistant", "content": [{"type": "text", "text": "hi there"}]}),
        },
    )
    .0;

    // Turn 2
    state = reduce(
        &state,
        &WireOp::TurnPrompt {
            input: json!([{"type": "text", "text": "do something"}]),
            origin: json!({"kind": "user"}),
        },
    )
    .0;
    state = reduce(
        &state,
        &WireOp::ContextAppendMessage {
            message: json!({"role": "user", "content": [{"type": "text", "text": "do something"}]}),
        },
    )
    .0;

    assert_eq!(state.turn_counter, 2);
    assert_eq!(state.conversation.messages.len(), 3);
}

#[test]
fn should_remain_safe_when_reducing_goal_update_without_existing_goal() {
    let initial = AgentState::new();
    assert!(initial.goal.is_none());

    let op = WireOp::GoalUpdate {
        goal_id: None,
        status: Some("completed".to_string()),
        reason: None,
        turns_used: Some(10),
        tokens_used: Some(1000),
        wall_clock_ms: None,
        budget_limits: None,
        actor: None,
    };

    let (state, events) = reduce(&initial, &op);

    // No panic, goal stays None.
    assert!(state.goal.is_none());
    // Event is still emitted based on status field.
    assert!(
        events
            .iter()
            .any(|e| matches!(e, DomainEvent::GoalCompleted {}))
    );
}

#[test]
fn should_clear_plan_mode_state_when_reducing_plan_mode_cancel() {
    let mut state = AgentState::new();
    state.plan_mode = Some(mirri_proto::PlanModeState {
        plan_id: "plan-2".to_string(),
    });

    let (state, events) = reduce(
        &state,
        &WireOp::PlanModeCancel {
            id: Some("plan-2".to_string()),
        },
    );

    assert!(state.plan_mode.is_none());
    assert!(
        events
            .iter()
            .any(|e| matches!(e, DomainEvent::PlanModeCancelled {}))
    );
}

// ── Cron domain ────────────────────────────────────────────────────────

/// A cron task in the wire shape written by `cron.add` (the `CronTask`
/// serde shape: snake_case timestamps as RFC 3339 strings).
fn cron_task_json(id: &str, cron: &str, recurring: bool) -> serde_json::Value {
    json!({
        "id": id,
        "cron": cron,
        "prompt": "tick",
        "session_id": "session-1",
        "recurring": recurring,
        "created_at": "2025-01-15T10:30:00Z",
        "last_fired": null,
        "next_fire_at": "2025-01-15T10:31:00Z",
    })
}

#[test]
fn should_store_task_in_state_when_reducing_cron_add() {
    let state = AgentState::new();

    let (state, events) = reduce(
        &state,
        &WireOp::CronAdd {
            task: cron_task_json("cron-1", "*/5 * * * *", true),
        },
    );

    assert_eq!(state.cron_tasks.len(), 1);
    assert_eq!(state.cron_tasks[0]["id"], "cron-1");
    assert_eq!(state.cron_tasks[0]["cron"], "*/5 * * * *");
    assert!(events.iter().any(
        |e| matches!(e, DomainEvent::CronTaskCreated { task_id, schedule }
                if task_id == "cron-1" && schedule == "*/5 * * * *")
    ));
}

#[test]
fn should_replace_task_in_place_when_reducing_cron_add_with_existing_id() {
    let mut state = AgentState::new();
    state.cron_tasks = vec![
        cron_task_json("cron-1", "*/5 * * * *", true),
        cron_task_json("cron-2", "0 9 * * *", true),
    ];
    let replacement = cron_task_json("cron-1", "0 12 * * *", false);

    let (state, _events) = reduce(&state, &WireOp::CronAdd { task: replacement });

    assert_eq!(
        state.cron_tasks.len(),
        2,
        "an upsert must not grow the list"
    );
    assert_eq!(state.cron_tasks[0]["cron"], "0 12 * * *");
    assert_eq!(state.cron_tasks[0]["recurring"], false);
    assert_eq!(
        state.cron_tasks[1]["id"], "cron-2",
        "other tasks keep their slots"
    );
}

#[test]
fn should_keep_task_without_id_when_reducing_cron_add() {
    // A record whose id cannot be read is kept verbatim rather than dropped,
    // so a bad record never fails the replay for the tasks around it.
    let state = AgentState::new();

    let (state, _events) = reduce(
        &state,
        &WireOp::CronAdd {
            task: json!({ "cron": "*/5 * * * *" }),
        },
    );

    assert_eq!(state.cron_tasks.len(), 1);
    assert_eq!(state.cron_tasks[0]["cron"], "*/5 * * * *");
}

#[test]
fn should_remove_only_matching_tasks_when_reducing_cron_delete() {
    let mut state = AgentState::new();
    state.cron_tasks = vec![
        cron_task_json("cron-1", "*/5 * * * *", true),
        cron_task_json("cron-2", "0 9 * * *", true),
        cron_task_json("cron-3", "0 12 * * *", false),
    ];

    let (state, events) = reduce(
        &state,
        &WireOp::CronDelete {
            ids: vec!["cron-1".to_string(), "cron-3".to_string()],
        },
    );

    let remaining_ids: Vec<&str> = state
        .cron_tasks
        .iter()
        .filter_map(|task| task["id"].as_str())
        .collect();
    assert_eq!(remaining_ids, vec!["cron-2"]);
    assert_eq!(events.len(), 2, "one deletion event per removed id");
}

#[test]
fn should_stamp_last_fired_when_reducing_cron_cursor() {
    let mut state = AgentState::new();
    state.cron_tasks = vec![
        cron_task_json("cron-1", "*/5 * * * *", true),
        cron_task_json("cron-2", "0 9 * * *", true),
    ];

    let (state, _events) = reduce(
        &state,
        &WireOp::CronCursor {
            id: "cron-1".to_string(),
            last_fired_at: 1_700_000_000_000,
            fire_count: None,
        },
    );

    // The cursor stamps the wire DTO key (`lastFiredAt`, epoch millis —
    // the TS cronCursor apply shape) regardless of the task record's own
    // shape; the restore-side parser reads both spellings (F16).
    assert_eq!(
        state.cron_tasks[0]["lastFiredAt"], 1_700_000_000_000u64,
        "the epoch-millis cursor is stamped under the DTO key"
    );
    assert!(
        state.cron_tasks[1]["lastFiredAt"].is_null(),
        "other tasks are untouched"
    );
}

#[test]
fn should_be_noop_when_reducing_cron_cursor_for_unknown_task() {
    let mut state = AgentState::new();
    state.cron_tasks = vec![cron_task_json("cron-1", "*/5 * * * *", true)];

    let (state, events) = reduce(
        &state,
        &WireOp::CronCursor {
            id: "cron-nope".to_string(),
            last_fired_at: 1_700_000_000_000,
            fire_count: None,
        },
    );

    assert_eq!(state.cron_tasks.len(), 1);
    assert!(state.cron_tasks[0]["last_fired"].is_null());
    assert!(events.is_empty());
}

// ── P2d: profile bind + tool store state ─────────────────────────────

#[test]
fn should_set_active_tool_set_when_reducing_tools_set_active_tools() {
    // Given a fresh state (no tool restriction applied)
    let initial = AgentState::new();
    assert!(initial.tools.active.is_none());

    // When the active tool set is restricted by name
    let op = WireOp::ToolsSetActiveTools {
        names: vec!["Read".to_string(), "Grep".to_string()],
    };
    let (state, _) = reduce(&initial, &op);

    // Then the restricted set is the derived active set
    assert_eq!(
        state.tools.active,
        Some(vec!["Read".to_string(), "Grep".to_string()])
    );
}

#[test]
fn should_clear_active_tool_set_when_reducing_tools_reset_active_tools() {
    // Given a state with a restricted active tool set
    let mut state = AgentState::new();
    state.tools.active = Some(vec!["Read".to_string()]);

    // When the active tool set is reset to the default
    let (state, _) = reduce(&state, &WireOp::ToolsResetActiveTools {});

    // Then the restriction is lifted (no active-set override)
    assert!(state.tools.active.is_none());
}

#[test]
fn should_seed_active_tool_set_when_reducing_profile_bind() {
    // Given a fresh state
    let initial = AgentState::new();

    // When a profile binds with an active tool set and a profile name
    let op = WireOp::ProfileBind {
        model_alias: Some("sonnet".to_string()),
        profile_name: Some("coder".to_string()),
        thinking_effort: json!("high"),
        system_prompt: "You are a coder".to_string(),
        active_tool_names: Some(vec!["Bash".to_string(), "Read".to_string()]),
        disallowed_tools: vec![],
        subagents: None,
    };
    let (state, _) = reduce(&initial, &op);

    // Then the bound profile name and the seeded active set are derived
    assert_eq!(state.profile_name.as_deref(), Some("coder"));
    assert_eq!(
        state.tools.active,
        Some(vec!["Bash".to_string(), "Read".to_string()])
    );
}

#[test]
fn should_keep_profile_name_when_profile_bind_has_none() {
    // Given a state with a previously bound profile name
    let mut state = AgentState::new();
    state.profile_name = Some("coder".to_string());

    // When a profile binds without a profile name
    let op = WireOp::ProfileBind {
        model_alias: None,
        profile_name: None,
        thinking_effort: json!("off"),
        system_prompt: String::new(),
        active_tool_names: None,
        disallowed_tools: vec![],
        subagents: None,
    };
    let (state, _) = reduce(&state, &op);

    // Then the previously bound name is kept (TS parity: profileName
    // defaults to the existing value)
    assert_eq!(state.profile_name.as_deref(), Some("coder"));
}

#[test]
fn should_update_profile_name_when_reducing_config_update() {
    // Given a fresh state
    let initial = AgentState::new();

    // When a config update carries a profile name
    let op = WireOp::ConfigUpdate {
        model_alias: None,
        profile_name: Some("reviewer".to_string()),
        thinking_effort: None,
        system_prompt: None,
        disallowed_tools: None,
    };
    let (state, _) = reduce(&initial, &op);

    // Then the profile name is updated
    assert_eq!(state.profile_name.as_deref(), Some("reviewer"));
}

#[test]
fn should_upsert_user_tool_registration_when_reducing_tools_register_user_tool() {
    // Given a fresh state
    let initial = AgentState::new();

    // When a user tool is registered, then registered again with a new
    // description under the same name
    let op = WireOp::ToolsRegisterUserTool {
        registration: json!({
            "name": "lookup_issue",
            "description": "Look up an issue",
            "parameters": { "type": "object", "properties": {} },
        }),
    };
    let (state, _) = reduce(&initial, &op);
    let op = WireOp::ToolsRegisterUserTool {
        registration: json!({
            "name": "lookup_issue",
            "description": "Look up an issue by id",
            "parameters": { "type": "object", "properties": {} },
        }),
    };
    let (state, _) = reduce(&state, &op);

    // Then exactly one registration survives, carrying the latest payload
    assert_eq!(state.tools.user_registrations.len(), 1);
    assert_eq!(state.tools.user_registrations[0]["name"], "lookup_issue");
    assert_eq!(
        state.tools.user_registrations[0]["description"],
        "Look up an issue by id"
    );
}

#[test]
fn should_remove_user_tool_registration_when_reducing_tools_unregister_user_tool() {
    // Given a state with one user-tool registration
    let mut state = AgentState::new();
    state.tools.user_registrations = vec![json!({
        "name": "lookup_issue",
        "description": "Look up an issue",
        "parameters": { "type": "object", "properties": {} },
    })];

    // When the tool is unregistered by name
    let op = WireOp::ToolsUnregisterUserTool {
        name: "lookup_issue".to_string(),
    };
    let (state, _) = reduce(&state, &op);

    // Then the registration is gone
    assert!(state.tools.user_registrations.is_empty());
}

/// Given a state with one registered user tool, When the tool is disabled
/// by name (`tools.set_user_tool_enabled`), Then the name lands on the
/// disabled list (the restore-time override) — and a later enable removes
/// it again.
#[test]
fn should_track_disabled_user_tool_when_reducing_tools_set_user_tool_enabled() {
    // Given a state with one user-tool registration
    let mut state = AgentState::new();
    state.tools.user_registrations = vec![json!({
        "name": "lookup_issue",
        "description": "Look up an issue",
        "parameters": { "type": "object", "properties": {} },
    })];

    // When the tool is disabled by name
    let op = WireOp::ToolsSetUserToolEnabled {
        name: "lookup_issue".to_string(),
        enabled: false,
    };
    let (state, _) = reduce(&state, &op);

    // Then the name is on the disabled list
    assert_eq!(state.tools.disabled_user_tools, vec!["lookup_issue"]);

    // And when it is re-enabled, the mark is gone
    let op = WireOp::ToolsSetUserToolEnabled {
        name: "lookup_issue".to_string(),
        enabled: true,
    };
    let (state, _) = reduce(&state, &op);
    assert!(state.tools.disabled_user_tools.is_empty());
}

/// Given an explicit active-tool whitelist holding a user tool, When the
/// tool is disabled, Then the name leaves the whitelist and lands on the
/// disabled list.
#[test]
fn should_remove_user_tool_from_active_set_when_reducing_disable() {
    // Given an explicit whitelist that includes the user tool
    let mut state = AgentState::new();
    state.tools.active = Some(vec![
        "Read".to_string(),
        "Grep".to_string(),
        "lookup_issue".to_string(),
    ]);
    state.tools.user_registrations = vec![json!({
        "name": "lookup_issue",
        "description": "Look up an issue",
        "parameters": { "type": "object", "properties": {} },
    })];

    // When the tool is disabled by name
    let op = WireOp::ToolsSetUserToolEnabled {
        name: "lookup_issue".to_string(),
        enabled: false,
    };
    let (state, _) = reduce(&state, &op);

    // Then the whitelist no longer names it, and the disabled list does
    assert_eq!(
        state.tools.active,
        Some(vec!["Read".to_string(), "Grep".to_string()])
    );
    assert_eq!(state.tools.disabled_user_tools, vec!["lookup_issue"]);
}

/// Given an explicit active-tool whitelist and a registered user tool it
/// does not name, When the tool is enabled, Then the name joins the
/// whitelist (the explicit widening survives the replay in order).
#[test]
fn should_add_user_tool_to_active_set_when_reducing_enable_under_whitelist() {
    // Given an explicit whitelist without the user tool
    let mut state = AgentState::new();
    state.tools.active = Some(vec!["Read".to_string(), "Grep".to_string()]);
    state.tools.user_registrations = vec![json!({
        "name": "lookup_issue",
        "description": "Look up an issue",
        "parameters": { "type": "object", "properties": {} },
    })];

    // When the tool is enabled by name
    let op = WireOp::ToolsSetUserToolEnabled {
        name: "lookup_issue".to_string(),
        enabled: true,
    };
    let (state, _) = reduce(&state, &op);

    // Then the whitelist names it (appended), and nothing is disabled
    assert_eq!(
        state.tools.active,
        Some(vec![
            "Read".to_string(),
            "Grep".to_string(),
            "lookup_issue".to_string(),
        ])
    );
    assert!(state.tools.disabled_user_tools.is_empty());
}

/// Given a disabled user tool, When the whole active set is replaced
/// (`tools.set_active_tools`) or reset (`tools.reset_active_tools`), Then
/// the disabled mark is cleared — a whole-set operation supersedes the
/// per-name overrides.
#[test]
fn should_clear_disabled_user_tools_when_the_active_set_is_replaced_or_reset() {
    // Given a state with a disabled user tool
    let mut state = AgentState::new();
    state.tools.user_registrations = vec![json!({
        "name": "lookup_issue",
        "description": "Look up an issue",
        "parameters": { "type": "object", "properties": {} },
    })];
    state.tools.disabled_user_tools = vec!["lookup_issue".to_string()];

    // When the whole active set is replaced
    let op = WireOp::ToolsSetActiveTools {
        names: vec!["Read".to_string()],
    };
    let (state, _) = reduce(&state, &op);
    assert!(
        state.tools.disabled_user_tools.is_empty(),
        "the whole-set replace clears the per-name disabled marks"
    );

    // And when a disabled mark is set again and the set is reset
    let op = WireOp::ToolsSetUserToolEnabled {
        name: "lookup_issue".to_string(),
        enabled: false,
    };
    let (state, _) = reduce(&state, &op);
    let op = WireOp::ToolsResetActiveTools {};
    let (state, _) = reduce(&state, &op);
    assert!(
        state.tools.disabled_user_tools.is_empty(),
        "the reset-to-default clears the per-name disabled marks"
    );
}

/// Given a disabled user tool, When the same name is re-registered, Then
/// the disabled mark is cleared — a fresh registration resets visibility
/// to the default (registered tools are visible in the unrestricted
/// mode).
#[test]
fn should_clear_disabled_mark_when_a_user_tool_is_re_registered() {
    // Given a state with a disabled user tool
    let mut state = AgentState::new();
    state.tools.disabled_user_tools = vec!["lookup_issue".to_string()];

    // When the tool is registered again under the same name
    let op = WireOp::ToolsRegisterUserTool {
        registration: json!({
            "name": "lookup_issue",
            "description": "Look up an issue by id",
            "parameters": { "type": "object", "properties": {} },
        }),
    };
    let (state, _) = reduce(&state, &op);

    // Then the disabled mark is gone
    assert!(state.tools.disabled_user_tools.is_empty());
}

#[test]
fn should_deserialize_state_without_tools_or_profile_fields_when_replaying_old_snapshots() {
    // Given a serialized state written before the tool/profile fields
    // existed (their keys are absent from the JSON)
    let raw = json!({
        "conversation": { "messages": [], "loop_events": [] },
        "permission": { "mode": "manual" },
        "budget": { "turns_used": 0, "tokens_used": 0 },
        "usage": { "total_input_tokens": 0, "total_output_tokens": 0 },
        "turn_counter": 0,
    });

    // When the state is deserialized
    let state: AgentState = serde_json::from_value(raw).expect("old snapshot must replay");

    // Then the tool/profile state falls back to its defaults
    assert!(state.tools.active.is_none());
    assert!(state.tools.user_registrations.is_empty());
    assert!(state.profile_name.is_none());
}

#[test]
fn should_fold_request_header_changed_into_request_header_state_and_event() {
    use mirri_proto::{AgentState, DomainEvent, WireOp, reducer};

    // Given a fresh state
    let mut state = AgentState::new();

    // When two header changes fold in (tools then compaction)
    let (s1, events) = reducer::reduce(
        &state,
        &WireOp::RequestHeaderChanged {
            reason: "tools_changed".to_string(),
        },
    );
    assert!(events.iter().any(
        |e| matches!(e, DomainEvent::RequestHeaderChanged { reason } if reason == "tools_changed")
    ));
    let (s2, _) = reducer::reduce(
        &s1,
        &WireOp::RequestHeaderChanged {
            reason: "compaction_applied".to_string(),
        },
    );
    state = s2;

    // Then the audit summary counts both and keeps the latest reason —
    // a resume can reconcile when the request prefix drifted and why.
    let header = state
        .request_header
        .expect("request header state must be folded");
    assert_eq!(header.change_count, 2);
    assert_eq!(header.last_reason, "compaction_applied");
}

#[test]
fn should_leave_request_header_state_absent_when_no_header_change_recorded() {
    use mirri_proto::{AgentState, WireOp, reducer};

    // Given a fresh state, when an unrelated op folds
    let (state, _) = reducer::reduce(&AgentState::new(), &WireOp::ContextClear {});

    // Then the request header state stays absent (no drift recorded).
    assert!(state.request_header.is_none());
}

// ---------------------------------------------------------------------------
// Steer-buffer durability (T3, F-P7-10..13)
// ---------------------------------------------------------------------------

#[test]
fn should_buffer_steer_in_pending_state_when_turn_steer_reduced() {
    use mirri_proto::{AgentState, WireOp, reducer};

    // Given a fresh state
    // When a mid-turn steer is recorded (the wire shape `message.steer`
    // parks with a turn in flight)
    let op = WireOp::TurnSteer {
        input: json!([{ "type": "text", "text": "focus on the rust side" }]),
        origin: json!({ "kind": "user" }),
    };
    let (state, _) = reducer::reduce(&AgentState::new(), &op);

    // Then the steer is pending — it has not been flushed into context
    assert_eq!(state.steer_pending.len(), 1);
    assert_eq!(
        state.steer_pending[0]["input"][0]["text"],
        "focus on the rust side"
    );
    assert_eq!(state.steer_pending[0]["origin"]["kind"], "user");
}

#[test]
fn should_pop_pending_steers_in_fifo_order_when_steer_flush_reduced() {
    use mirri_proto::{AgentState, WireOp, reducer};

    // Given two buffered steers (first, then second)
    let mut state = AgentState::new();
    for text in ["first steer", "second steer"] {
        let (s, _) = reducer::reduce(
            &state,
            &WireOp::TurnSteer {
                input: json!([{ "type": "text", "text": text }]),
                origin: json!({ "kind": "user" }),
            },
        );
        state = s;
    }

    // When the turn's next step flushes one steer
    let (state, _) = reducer::reduce(&state, &WireOp::TurnSteerFlushed { count: 1 });

    // Then only the second steer is still pending (FIFO consumption)
    assert_eq!(state.steer_pending.len(), 1);
    assert_eq!(state.steer_pending[0]["input"][0]["text"], "second steer");
}

#[test]
fn should_clear_all_pending_steers_when_flush_count_exceeds_buffered() {
    use mirri_proto::{AgentState, WireOp, reducer};

    // Given one buffered steer
    let (state, _) = reducer::reduce(
        &AgentState::new(),
        &WireOp::TurnSteer {
            input: json!([{ "type": "text", "text": "steer" }]),
            origin: json!({ "kind": "user" }),
        },
    );

    // When a flush marker claims more steers than were buffered (a torn
    // log replaying against a lost earlier marker must not underflow)
    let (state, _) = reducer::reduce(&state, &WireOp::TurnSteerFlushed { count: 5 });

    // Then the pending buffer is simply empty
    assert!(state.steer_pending.is_empty());
}

#[test]
fn should_track_pending_steers_across_interleaved_record_and_flush() {
    use mirri_proto::{AgentState, WireOp, reducer};

    // Given a steer buffered, then flushed, then another buffered
    let (s1, _) = reducer::reduce(
        &AgentState::new(),
        &WireOp::TurnSteer {
            input: json!([{ "type": "text", "text": "consumed" }]),
            origin: json!({ "kind": "user" }),
        },
    );
    let (s2, _) = reducer::reduce(&s1, &WireOp::TurnSteerFlushed { count: 1 });
    let (s3, _) = reducer::reduce(
        &s2,
        &WireOp::TurnSteer {
            input: json!([{ "type": "text", "text": "resident" }]),
            origin: json!({ "kind": "user" }),
        },
    );

    // When the whole sequence folds, only the unconsumed steer survives —
    // the durable buffer a resume restores holds exactly the parked steer.
    assert_eq!(s3.steer_pending.len(), 1);
    assert_eq!(s3.steer_pending[0]["input"][0]["text"], "resident");
}

#[test]
fn should_capture_first_call_when_first_usage_record_arrives_before_any_accumulation() {
    // Given a fresh session state (no accumulated usage, no first call)
    let mut state = AgentState::new();

    // When the first usage record arrives, then a second one
    let op1 = WireOp::UsageRecord {
        model: "claude-sonnet-4".to_string(),
        usage: json!({"inputTokens": 100, "outputTokens": 50, "cacheReadTokens": 30, "cacheCreationTokens": 5}),
        usage_scope: None,
    };
    let op2 = WireOp::UsageRecord {
        model: "claude-sonnet-4".to_string(),
        usage: json!({"inputTokens": 200, "outputTokens": 30, "cacheReadTokens": 10, "cacheCreationTokens": 2}),
        usage_scope: None,
    };
    state = reduce(&state, &op1).0;
    state = reduce(&state, &op2).0;

    // Then the first-call snapshot holds the FIRST record's buckets
    let first = state.usage.first_call.expect("first call must be captured");
    assert_eq!(first.input_tokens, 100);
    assert_eq!(first.output_tokens, 50);
    assert_eq!(first.cache_read_tokens, 30);
    assert_eq!(first.cache_creation_tokens, 5);
    // And the totals still accumulate both records
    assert_eq!(state.usage.total_input_tokens, 300);
}

#[test]
fn should_default_first_call_to_none_when_replaying_old_snapshots() {
    // Given a serialized state written before the first_call field existed
    let raw = json!({
        "conversation": { "messages": [], "loop_events": [] },
        "permission": { "mode": "manual" },
        "budget": { "turns_used": 0, "tokens_used": 0 },
        "usage": { "total_input_tokens": 120, "total_output_tokens": 30 },
        "turn_counter": 0,
    });

    // When the state is deserialized
    let state: AgentState = serde_json::from_value(raw).expect("old snapshot must replay");

    // Then the first call stays honestly absent (no baseline to subtract)
    assert_eq!(state.usage.first_call, None);
}

#[test]
fn should_not_capture_first_call_when_replaying_onto_a_snapshot_without_one() {
    // Given a resumed mid-session state (totals > 0, first call absent)
    let mut state: AgentState = serde_json::from_value(json!({
        "conversation": { "messages": [], "loop_events": [] },
        "permission": { "mode": "manual" },
        "budget": { "turns_used": 0, "tokens_used": 0 },
        "usage": { "total_input_tokens": 120, "total_output_tokens": 30 },
        "turn_counter": 0,
    }))
    .expect("old snapshot must replay");

    // When the next post-resume usage record arrives
    let op = WireOp::UsageRecord {
        model: "claude-sonnet-4".to_string(),
        usage: json!({"inputTokens": 10, "outputTokens": 1, "cacheReadTokens": 4, "cacheCreationTokens": 0}),
        usage_scope: None,
    };
    state = reduce(&state, &op).0;

    // Then it must not masquerade as the session's first call
    assert_eq!(state.usage.first_call, None);
    assert_eq!(state.usage.total_input_tokens, 130);
}

/// Build a 20+ step mixed-op sequence covering messages, loop events,
/// compaction, goal updates, turn ops, plan mode, cron, tools, usage and
/// profile ops — the full surface the fold exercises in a real resume.
fn mixed_replay_ops() -> Vec<WireOp> {
    let mut ops = vec![
        WireOp::Metadata {
            protocol_version: Some("1.8".to_string()),
            created_at: Some(1234567890),
        },
        WireOp::ContextAppendMessage {
            message: json!({"role": "user", "content": [{"type": "text", "text": "hello"}]}),
        },
        WireOp::ContextAppendLoopEvent {
            event: json!({"type": "step.begin", "uuid": "step-1", "turnId": "turn-1"}),
        },
        WireOp::ContextAppendMessage {
            message: json!({"role": "assistant", "content": [{"type": "text", "text": "hi"}]}),
        },
        WireOp::TurnPrompt {
            input: json!({"text": "hello"}),
            origin: json!({"kind": "user"}),
        },
    ];
    for i in 0..8 {
        ops.push(WireOp::ContextAppendMessage {
            message: json!({"role": "user", "content": [{"type": "text", "text": format!("msg {i}")}]}),
        });
    }
    ops.extend([
        WireOp::ContextApplyCompaction {
            summary: "the conversation so far".to_string(),
            compacted_count: 10,
            tokens_before: Some(2000),
            tokens_after: Some(120),
            context_summary: None,
            dropped_count: None,
            retained_tail_count: None,
            kept_user_message_count: None,
            kept_head_user_message_count: None,
            elision_message: None,
            first_replaced_entry_seq: None,
            last_replaced_entry_seq: None,
            dropped_entry_seqs: None,
        },
        WireOp::ContextAppendMessage {
            message: json!({"role": "user", "content": [{"type": "text", "text": "after compaction"}]}),
        },
        WireOp::GoalCreate {
            goal_id: "goal-1".to_string(),
            objective: "finish".to_string(),
            completion_criterion: Some("done".to_string()),
            status: Some("active".to_string()),
            budget_limits: None,
            actor: None,
        },
        WireOp::GoalUpdate {
            goal_id: Some("goal-1".to_string()),
            status: Some("paused".to_string()),
            reason: Some("waiting".to_string()),
            turns_used: Some(1),
            tokens_used: Some(500),
            wall_clock_ms: None,
            budget_limits: None,
            actor: None,
        },
        WireOp::PlanModeEnter {
            id: "plan-1".to_string(),
            plan_text: Some("the plan".to_string()),
        },
        WireOp::PlanRevision {
            id: "plan-1".to_string(),
            version: 2,
            path: "plan.md".to_string(),
            sha256: "abc".to_string(),
            bytes: 100,
            plan_text: Some("revised plan".to_string()),
        },
        WireOp::PlanModeExit {
            id: Some("plan-1".to_string()),
        },
        WireOp::PermissionSetMode {
            mode: "auto".to_string(),
        },
        WireOp::CronAdd {
            task: json!({"id": "cron-1", "cron": "0 * * * *"}),
        },
        WireOp::ToolsSetActiveTools {
            names: vec!["bash".to_string(), "read".to_string()],
        },
        WireOp::ToolsRegisterUserTool {
            registration: json!({"name": "my-tool", "description": "a tool"}),
        },
        WireOp::UsageRecord {
            model: "claude-sonnet-4".to_string(),
            usage: json!({"inputTokens": 100, "outputTokens": 20, "cacheReadTokens": 5, "cacheCreationTokens": 0}),
            usage_scope: None,
        },
        WireOp::TurnEnded {
            turn_id: 1,
            reason: "completed".to_string(),
            error: None,
            duration_ms: None,
            ended_at: None,
        },
        WireOp::SwarmModeEnter {
            trigger: json!("auto"),
        },
        WireOp::SwarmModeExit {},
        WireOp::Unknown,
    ]);
    ops
}

#[test]
fn should_fold_same_state_in_place_when_apply_matches_reduce_over_a_mixed_log() {
    // Given the same mixed op log folded through repeated `reduce`
    let ops = mixed_replay_ops();
    let mut reduce_state = AgentState::new();
    for op in &ops {
        reduce_state = reduce(&reduce_state, op).0;
    }

    // And folded through repeated in-place `apply` on a single state
    let mut apply_state = AgentState::new();
    for op in &ops {
        apply_with_limits(&mut apply_state, op, None);
    }

    // Then both reach the same final state (compare via JSON — AgentState
    // carries no PartialEq)
    assert_eq!(
        serde_json::to_value(&apply_state).unwrap(),
        serde_json::to_value(&reduce_state).unwrap()
    );
}

#[test]
fn should_emit_same_events_when_apply_matches_reduce_over_a_mixed_log() {
    // Given the same mixed op log
    let ops = mixed_replay_ops();

    // When folded through `reduce`, collecting the per-op events
    let mut reduce_events: Vec<DomainEvent> = Vec::new();
    let mut reduce_state = AgentState::new();
    for op in &ops {
        let (state, events) = reduce(&reduce_state, op);
        reduce_events.extend(events);
        reduce_state = state;
    }

    // And through in-place `apply`, collecting the per-op events
    let mut apply_events: Vec<DomainEvent> = Vec::new();
    let mut apply_state = AgentState::new();
    for op in &ops {
        apply_events.extend(apply_with_limits(&mut apply_state, op, None));
    }

    // Then both emit the identical event sequence
    assert_eq!(
        serde_json::to_value(&apply_events).unwrap(),
        serde_json::to_value(&reduce_events).unwrap()
    );
}

}

mod schema_tests {
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

}

mod wire_op_tests {
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

}

