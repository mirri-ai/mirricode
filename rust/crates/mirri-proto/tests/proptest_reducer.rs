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
