//! Integration tests for the `context_memory` module.
//!
//! These verify the load-bearing invariants of `ContextMemory`, the projection
//! pipeline, token estimation, compaction, and undo.

use mirri_core::context::{ContextMessage, ProjectOptions};
use mirri_core::context_memory::select_compaction_user_messages;
use mirri_core::context_memory::token_estimator::{
    MEDIA_TOKEN_ESTIMATE, estimate_message_tokens, estimate_tokens, truncate_text_to_tokens,
    truncate_text_to_tokens_from_end,
};
use mirri_core::message::{ContentPart, ImageUrlData, Message, Role, ToolCall, ToolCallFunction};
use mirri_core::origin::PromptOrigin;
use mirri_core::tool::ToolOutput;
use mirri_core::{CompactionInput, ContextMemory, LoopEvent};

use serde_json::json;

// ---------------------------------------------------------------------------
// Helper builders
// ---------------------------------------------------------------------------

fn user_msg(text: &str) -> ContextMessage {
    ContextMessage {
        message: Message::user(text),
        origin: Some(PromptOrigin::User),
        is_error: false,
        note: None,
        with_skills: None,
        entry_seq: 0,
    }
}

fn assistant_text_msg(text: &str) -> ContextMessage {
    ContextMessage {
        message: Message::assistant(text),
        origin: None,
        is_error: false,
        note: None,
        with_skills: None,
        entry_seq: 0,
    }
}

fn assistant_tool_call_msg(tool_call_id: &str, tool_name: &str) -> ContextMessage {
    ContextMessage {
        message: Message {
            role: Role::Assistant,
            name: None,
            content: Vec::new(),
            tool_calls: vec![ToolCall {
                id: tool_call_id.to_string(),
                call_type: "function".to_string(),
                function: ToolCallFunction {
                    name: tool_name.to_string(),
                    arguments: "{}".to_string(),
                },
            }],
            tool_call_id: None,
            partial: false,
        },
        origin: None,
        is_error: false,
        note: None,
        with_skills: None,
        entry_seq: 0,
    }
}

fn tool_result_msg(tool_call_id: &str, output: &str) -> ContextMessage {
    ContextMessage {
        message: Message::tool_result(tool_call_id, output),
        origin: None,
        is_error: false,
        note: None,
        with_skills: None,
        entry_seq: 0,
    }
}

fn tool_result_error_msg(tool_call_id: &str, output: &str) -> ContextMessage {
    let mut msg = tool_result_msg(tool_call_id, output);
    msg.is_error = true;
    msg
}

// ---------------------------------------------------------------------------
// 1. Basic append and history
// ---------------------------------------------------------------------------

#[test]
fn should_store_user_message_with_correct_role_and_origin_when_appended() {
    let mut mem = ContextMemory::new();
    mem.append_user_message(
        vec![ContentPart::Text {
            text: "hello world".to_string(),
        }],
        PromptOrigin::User,
    );

    assert_eq!(mem.history().len(), 1);
    assert_eq!(mem.history()[0].message.role, Role::User);
    assert_eq!(
        mem.history()[0].message.content[0],
        ContentPart::Text {
            text: "hello world".to_string()
        }
    );
    assert_eq!(mem.history()[0].origin, Some(PromptOrigin::User));
}

// ---------------------------------------------------------------------------
// 2. Deferred messages during open exchange
// ---------------------------------------------------------------------------

#[test]
fn should_defer_user_messages_when_tool_exchange_is_open() {
    let mut mem = ContextMemory::new();

    // Start a step and make a tool call → opens an exchange.
    mem.append_loop_event(LoopEvent::StepBegin {
        step_id: "step1".to_string(),
        entry_seq: None,
    });
    mem.append_loop_event(LoopEvent::ToolCall {
        step_id: "step1".to_string(),
        tool_call_id: "call_1".to_string(),
        tool_name: "read_file".to_string(),
        args: json!({}),
    });

    assert!(mem.has_open_tool_exchange());

    // Append a user message → should be deferred, not in history.
    mem.append_user_message(
        vec![ContentPart::Text {
            text: "deferred user msg".to_string(),
        }],
        PromptOrigin::User,
    );
    // History should have: [assistant(partial, tool_call)] but NOT the user msg.
    assert_eq!(mem.history().len(), 1);
    assert_eq!(mem.history()[0].message.role, Role::Assistant);

    // Provide the tool result → closes the exchange, flushes deferred.
    mem.append_loop_event(LoopEvent::ToolResult {
        tool_call_id: "call_1".to_string(),
        output: ToolOutput::text("file contents"),
        is_error: false,
        note: None,
        entry_seq: None,
    });

    assert!(!mem.has_open_tool_exchange());
    // History should now be: [assistant, tool_result, user_msg]
    assert_eq!(mem.history().len(), 3);
    assert_eq!(mem.history()[0].message.role, Role::Assistant);
    assert_eq!(mem.history()[1].message.role, Role::Tool);
    assert_eq!(mem.history()[2].message.role, Role::User);
    assert_eq!(
        mem.history()[2].message.content[0],
        ContentPart::Text {
            text: "deferred user msg".to_string()
        }
    );
}

// ---------------------------------------------------------------------------
// 3. Multiple tool calls in one step
// ---------------------------------------------------------------------------

#[test]
fn should_keep_exchange_open_when_multiple_tool_calls_pending() {
    let mut mem = ContextMemory::new();

    mem.append_loop_event(LoopEvent::StepBegin {
        step_id: "step1".to_string(),
        entry_seq: None,
    });
    mem.append_loop_event(LoopEvent::ToolCall {
        step_id: "step1".to_string(),
        tool_call_id: "call_a".to_string(),
        tool_name: "read_file".to_string(),
        args: json!({}),
    });
    mem.append_loop_event(LoopEvent::ToolCall {
        step_id: "step1".to_string(),
        tool_call_id: "call_b".to_string(),
        tool_name: "write_file".to_string(),
        args: json!({}),
    });

    assert!(mem.has_open_tool_exchange());
    assert_eq!(mem.history().len(), 1);
    assert_eq!(mem.history()[0].message.tool_calls.len(), 2);

    mem.append_loop_event(LoopEvent::ToolResult {
        tool_call_id: "call_a".to_string(),
        output: ToolOutput::text("result_a"),
        is_error: false,
        note: None,
        entry_seq: None,
    });
    // Still open — call_b pending.
    assert!(mem.has_open_tool_exchange());

    mem.append_loop_event(LoopEvent::ToolResult {
        tool_call_id: "call_b".to_string(),
        output: ToolOutput::text("result_b"),
        is_error: false,
        note: None,
        entry_seq: None,
    });
    assert!(!mem.has_open_tool_exchange());

    // History: [assistant(2 tool_calls), tool_result_a, tool_result_b]
    assert_eq!(mem.history().len(), 3);
    assert_eq!(mem.history()[1].message.role, Role::Tool);
    assert_eq!(
        mem.history()[1].message.tool_call_id,
        Some("call_a".to_string())
    );
    assert_eq!(mem.history()[2].message.role, Role::Tool);
    assert_eq!(
        mem.history()[2].message.tool_call_id,
        Some("call_b".to_string())
    );
}

// ---------------------------------------------------------------------------
// 4. Close pending tool results (synthesize error)
// ---------------------------------------------------------------------------

#[test]
fn should_synthesize_error_results_when_closing_pending_tool_results() {
    let mut mem = ContextMemory::new();

    mem.append_loop_event(LoopEvent::StepBegin {
        step_id: "step1".to_string(),
        entry_seq: None,
    });
    mem.append_loop_event(LoopEvent::ToolCall {
        step_id: "step1".to_string(),
        tool_call_id: "call_x".to_string(),
        tool_name: "read_file".to_string(),
        args: json!({}),
    });

    assert!(mem.has_open_tool_exchange());

    mem.close_pending_tool_results();

    assert!(!mem.has_open_tool_exchange());
    // History: [assistant, synthetic_tool_result]
    assert_eq!(mem.history().len(), 2);
    assert_eq!(mem.history()[1].message.role, Role::Tool);
    assert_eq!(
        mem.history()[1].message.tool_call_id,
        Some("call_x".to_string())
    );
    assert!(mem.history()[1].is_error);
}

// ---------------------------------------------------------------------------
// 5. Projection: normal (strips metadata)
// ---------------------------------------------------------------------------

#[test]
fn should_strip_metadata_and_produce_correct_message_sequence_when_projecting() {
    let mut mem = ContextMemory::new();
    mem.append_user_message(
        vec![ContentPart::Text {
            text: "hello".to_string(),
        }],
        PromptOrigin::User,
    );
    mem.append_loop_event(LoopEvent::StepBegin {
        step_id: "s1".to_string(),
        entry_seq: None,
    });
    mem.append_loop_event(LoopEvent::ContentPart {
        step_id: "s1".to_string(),
        text: "I will use a tool".to_string(),
    });
    mem.append_loop_event(LoopEvent::ToolCall {
        step_id: "s1".to_string(),
        tool_call_id: "tc1".to_string(),
        tool_name: "read".to_string(),
        args: json!({}),
    });
    mem.append_loop_event(LoopEvent::ToolResult {
        tool_call_id: "tc1".to_string(),
        output: ToolOutput::text("output data"),
        is_error: false,
        note: None,
        entry_seq: None,
    });
    mem.append_loop_event(LoopEvent::StepEnd {
        step_id: "s1".to_string(),
        input_tokens: 100,
        output_tokens: 50,
        cache_read_tokens: 0,
        cache_creation_tokens: 0,
    });

    let messages = mem.messages();
    // Should be: [user, assistant(text + tool_call), tool_result]
    assert_eq!(messages.len(), 3);
    assert_eq!(messages[0].role, Role::User);
    assert_eq!(messages[1].role, Role::Assistant);
    assert_eq!(messages[1].tool_calls.len(), 1);
    assert_eq!(messages[2].role, Role::Tool);

    // Verify metadata is stripped: messages are plain Message, no origin field.
    // (We can't check origin directly on Message, but we verify the content is
    // wire-valid.)
    assert!(!messages[0].content.is_empty());
}

// ---------------------------------------------------------------------------
// 6. Projection: orphan tool result dropped
// ---------------------------------------------------------------------------

#[test]
fn should_drop_orphan_tool_results_when_projecting() {
    // Build a history with an orphan tool result (no matching tool call).
    let mut mem = ContextMemory::new();
    mem.append_user_message(
        vec![ContentPart::Text {
            text: "hi".to_string(),
        }],
        PromptOrigin::User,
    );
    // Orphan tool result — no assistant with tool_call "orphan_id" exists.
    mem.append_message(tool_result_msg("orphan_id", "orphan output"));

    let options = ProjectOptions::normal();
    let result = mem.project(&options).unwrap();

    // The orphan should be dropped; only the user message survives.
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].role, Role::User);
}

// ---------------------------------------------------------------------------
// 7. Projection: adjacency repair (tool result moved adjacent to call)
// ---------------------------------------------------------------------------

#[test]
fn should_move_tool_result_adjacent_to_call_when_projecting() {
    // Build: [user, assistant(tool_call), user, tool_result]
    // The tool result is not adjacent to its call — it should be moved up.
    let mut mem = ContextMemory::new();
    mem.append_user_message(
        vec![ContentPart::Text {
            text: "prompt".to_string(),
        }],
        PromptOrigin::User,
    );
    mem.append_message(assistant_tool_call_msg("tc_adj", "read"));
    mem.append_user_message(
        vec![ContentPart::Text {
            text: "injected".to_string(),
        }],
        PromptOrigin::User,
    );
    mem.append_message(tool_result_msg("tc_adj", "result"));

    let options = ProjectOptions::normal();
    let result = mem.project(&options).unwrap();

    // After repair, the tool result should be adjacent to the assistant.
    // Expected order: [user, user(merged), assistant, tool_result, user]
    // or similar. The key check: tool_result is right after the assistant.
    let assistant_idx = result
        .iter()
        .position(|m| m.role == Role::Assistant && !m.tool_calls.is_empty())
        .expect("assistant with tool_call must exist");
    assert_eq!(result[assistant_idx + 1].role, Role::Tool);
    assert_eq!(
        result[assistant_idx + 1].tool_call_id,
        Some("tc_adj".to_string())
    );
}

// ---------------------------------------------------------------------------
// 8. Projection: synthesize missing (trailing)
// ---------------------------------------------------------------------------

#[test]
fn should_synthesize_missing_trailing_tool_results_when_projecting() {
    // Build: [user, assistant(tool_call)] with no tool result — trailing open.
    let mut mem = ContextMemory::new();
    mem.append_user_message(
        vec![ContentPart::Text {
            text: "prompt".to_string(),
        }],
        PromptOrigin::User,
    );
    mem.append_message(assistant_tool_call_msg("tc_miss", "read"));

    // With synthesize_missing=true, a synthetic result should be added.
    let options = ProjectOptions {
        synthesize_missing: true,
        drop_orphan_results: true,
        ..Default::default()
    };
    let result = mem.project(&options).unwrap();

    // Expected: [user, assistant, synthetic_tool_result]
    assert_eq!(result.len(), 3);
    assert_eq!(result[2].role, Role::Tool);
    assert_eq!(result[2].tool_call_id, Some("tc_miss".to_string()));
}

// ---------------------------------------------------------------------------
// 9. Token estimation
// ---------------------------------------------------------------------------

#[test]
fn should_estimate_tokens_correctly_when_message_contains_ascii_cjk_or_media() {
    // ASCII: 8 chars / 4 = 2 tokens
    assert_eq!(estimate_tokens("abcdefgh"), 2);
    // CJK: 3 chars = 3 tokens
    assert_eq!(estimate_tokens("你好吗"), 3);

    // Message with image: flat 2000 tokens
    let img_msg = ContextMessage {
        message: Message {
            role: Role::User,
            name: None,
            content: vec![ContentPart::ImageUrl {
                image_url: ImageUrlData {
                    url: "http://example.com/img.png".to_string(),
                    detail: None,
                    id: None,
                },
                detail: None,
            }],
            tool_calls: Vec::new(),
            tool_call_id: None,
            partial: false,
        },
        origin: None,
        is_error: false,
        note: None,
        with_skills: None,
        entry_seq: 0,
    };
    assert_eq!(estimate_message_tokens(&img_msg), MEDIA_TOKEN_ESTIMATE);
}

// ---------------------------------------------------------------------------
// 10. Truncate to tokens (beginning and end)
// ---------------------------------------------------------------------------

#[test]
fn should_truncate_text_from_beginning_and_end_when_truncating_to_token_limit() {
    // 8 ASCII chars = 2 tokens. Truncate to 1 token → first 4 chars.
    let text = "abcdefgh";
    let from_begin = truncate_text_to_tokens(text, 1);
    assert_eq!(from_begin, "abcd");

    // From end: last 4 chars.
    let from_end = truncate_text_to_tokens_from_end(text, 1);
    assert_eq!(from_end, "efgh");

    // CJK: 3 chars = 3 tokens. Truncate to 1 → first 1 char.
    let cjk = "你好吗";
    let cjk_begin = truncate_text_to_tokens(cjk, 1);
    assert_eq!(cjk_begin, "你");
    let cjk_end = truncate_text_to_tokens_from_end(cjk, 1);
    assert_eq!(cjk_end, "吗");
}

// ---------------------------------------------------------------------------
// 11. Compaction user message selection
// ---------------------------------------------------------------------------

#[test]
fn should_keep_all_user_messages_when_compaction_budget_is_sufficient() {
    let mut history = Vec::new();

    // 5 user messages with different origins.
    for i in 0..5 {
        history.push(user_msg(&format!("user message {i}")));
    }
    // An injection (should be excluded).
    history.push(ContextMessage {
        message: Message::user("injection text"),
        origin: Some(PromptOrigin::Injection),
        is_error: false,
        note: None,
        with_skills: None,
        entry_seq: 0,
    });
    // A compaction summary (should be excluded).
    history.push(ContextMessage {
        message: Message::user("summary"),
        origin: Some(PromptOrigin::CompactionSummary),
        is_error: false,
        note: None,
        with_skills: None,
        entry_seq: 0,
    });

    let selection = select_compaction_user_messages(&history, 20_000);

    // All 5 real user messages should be kept, no elision (small enough).
    assert!(!selection.elided);
    assert_eq!(selection.kept.len(), 5);
    assert_eq!(selection.omitted_tokens, 0);
}

#[test]
fn should_elide_messages_when_compaction_budget_exceeded() {
    let mut history = Vec::new();

    // Create a large set of user messages that exceed the 20k token budget.
    // Each message ~100 ASCII chars = 25 tokens. Need >800 messages to exceed.
    // Instead, use fewer messages with CJK (1 token per char) to exceed faster.
    // 500 CJK chars per message × 50 messages = 25000 tokens.
    let big_text: String = "你".repeat(500);
    for _ in 0..50 {
        history.push(user_msg(&big_text));
    }

    let selection = select_compaction_user_messages(&history, 20_000);

    assert!(selection.elided);
    assert!(selection.omitted_tokens > 0);
    // Should have both head and tail messages.
    assert!(!selection.kept.is_empty());
}

// ---------------------------------------------------------------------------
// 12. Apply compaction
// ---------------------------------------------------------------------------

#[test]
fn should_rewrite_history_with_summary_and_kept_messages_when_compaction_applied() {
    let mut mem = ContextMemory::new();

    // Build a history with user + assistant + tool messages.
    mem.append_user_message(
        vec![ContentPart::Text {
            text: "do something".to_string(),
        }],
        PromptOrigin::User,
    );
    // Large assistant/tool bodies so the compaction rewrite genuinely
    // shrinks the history — a tiny memory cannot pass the shrink check
    // (P8a-3 变小校验) and is rejected by design.
    mem.append_message(assistant_text_msg(&format!(
        "I will help: {}",
        "x".repeat(8_000)
    )));
    mem.append_message(assistant_tool_call_msg("tc_c1", "read"));
    mem.append_message(tool_result_msg(
        "tc_c1",
        &format!("data: {}", "y".repeat(8_000)),
    ));
    mem.append_user_message(
        vec![ContentPart::Text {
            text: "thanks".to_string(),
        }],
        PromptOrigin::User,
    );

    let input = CompactionInput {
        summary: "Conversation summary".to_string(),
        context_summary: "Context summary text".to_string(),
        compacted_count: 4,
        tokens_before: 1000,
        dropped_count: 0,
        // Retention tail = the newest user message; the prefix ends right
        // after the closed tool exchange (a legal cut).
        retention_tail_start: Some(4),
    };

    let outcome = mem.apply_compaction(input);

    // New history = [head essence user][summary (ADR-002)][retention tail]
    assert!(outcome.applied);
    assert_eq!(mem.history().len(), 3);
    // First should be the kept head user message (verbatim).
    assert_eq!(mem.history()[0].message.role, Role::User);
    assert_eq!(mem.history()[0].origin, Some(PromptOrigin::User));
    // The summary (CompactionSummary origin) sits BEFORE the tail.
    assert_eq!(mem.history()[1].message.role, Role::User);
    assert_eq!(
        mem.history()[1].origin,
        Some(PromptOrigin::CompactionSummary)
    );
    // The retention tail is kept verbatim.
    assert_eq!(mem.history()[2].message.role, Role::User);
    assert_eq!(mem.history()[2].message.text_content(), "thanks");

    // Token count should be re-estimated.
    assert!(mem.token_count() > 0);
    // Open exchange state should be cleared.
    assert!(!mem.has_open_tool_exchange());
}

// ---------------------------------------------------------------------------
// 13. Undo
// ---------------------------------------------------------------------------

#[test]
fn should_remove_last_user_and_trailing_messages_when_undo_called() {
    let mut mem = ContextMemory::new();

    mem.append_user_message(
        vec![ContentPart::Text {
            text: "first prompt".to_string(),
        }],
        PromptOrigin::User,
    );
    mem.append_message(assistant_text_msg("first response"));
    mem.append_user_message(
        vec![ContentPart::Text {
            text: "second prompt".to_string(),
        }],
        PromptOrigin::User,
    );
    mem.append_message(assistant_text_msg("second response"));

    assert_eq!(mem.history().len(), 4);

    mem.undo().unwrap();

    // Undo should remove "second prompt" and "second response".
    assert_eq!(mem.history().len(), 2);
    assert_eq!(mem.history()[0].message.role, Role::User);
    assert_eq!(
        mem.history()[0].message.content[0],
        ContentPart::Text {
            text: "first prompt".to_string()
        }
    );
    assert_eq!(mem.history()[1].message.role, Role::Assistant);
}

// ---------------------------------------------------------------------------
// Extra: system reminder wrapping
// ---------------------------------------------------------------------------

#[test]
fn should_wrap_reminder_in_system_tag_when_appending_system_reminder() {
    let mut mem = ContextMemory::new();
    mem.append_system_reminder("reminder text".to_string());

    assert_eq!(mem.history().len(), 1);
    if let ContentPart::Text { text } = &mem.history()[0].message.content[0] {
        assert!(text.contains("<system-reminder>"));
        assert!(text.contains("reminder text"));
        assert!(text.contains("</system-reminder>"));
    } else {
        panic!("expected text content part");
    }
    assert_eq!(mem.history()[0].origin, Some(PromptOrigin::Injection));
}

// ---------------------------------------------------------------------------
// Extra: tool result error rendering in projection
// ---------------------------------------------------------------------------

#[test]
fn should_render_error_prefix_when_projecting_tool_result_error() {
    let mut mem = ContextMemory::new();
    mem.append_user_message(
        vec![ContentPart::Text {
            text: "prompt".to_string(),
        }],
        PromptOrigin::User,
    );
    mem.append_message(assistant_tool_call_msg("tc_err", "read"));
    mem.append_message(tool_result_error_msg("tc_err", "tool failed output"));

    let options = ProjectOptions::normal();
    let result = mem.project(&options).unwrap();

    // Find the tool result.
    let tool_msg = result
        .iter()
        .find(|m| m.role == Role::Tool)
        .expect("tool result must exist");

    if let ContentPart::Text { text } = &tool_msg.content[0] {
        assert!(text.contains("<system>ERROR: Tool execution failed.</system>"));
        assert!(text.contains("tool failed output"));
    } else {
        panic!("expected text content part");
    }
}

// ---------------------------------------------------------------------------
// Extra: projection drops partial messages
// ---------------------------------------------------------------------------

#[test]
fn should_drop_partial_messages_when_projecting() {
    let mut mem = ContextMemory::new();
    mem.append_user_message(
        vec![ContentPart::Text {
            text: "prompt".to_string(),
        }],
        PromptOrigin::User,
    );
    // Push a partial assistant message (via StepBegin without StepEnd).
    mem.append_loop_event(LoopEvent::StepBegin {
        step_id: "partial_step".to_string(),
        entry_seq: None,
    });
    mem.append_loop_event(LoopEvent::ContentPart {
        step_id: "partial_step".to_string(),
        text: "partial content".to_string(),
    });

    let options = ProjectOptions::normal();
    let result = mem.project(&options).unwrap();

    // The partial assistant should be dropped; only user remains.
    // (Partial messages are dropped in prepare_message_for_projection.)
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].role, Role::User);
}

// ---------------------------------------------------------------------------
// Extra: undo with count > 1
// ---------------------------------------------------------------------------

#[test]
fn should_remove_multiple_pairs_when_undo_count_called() {
    let mut mem = ContextMemory::new();

    mem.append_user_message(
        vec![ContentPart::Text {
            text: "p1".to_string(),
        }],
        PromptOrigin::User,
    );
    mem.append_message(assistant_text_msg("a1"));
    mem.append_user_message(
        vec![ContentPart::Text {
            text: "p2".to_string(),
        }],
        PromptOrigin::User,
    );
    mem.append_message(assistant_text_msg("a2"));
    mem.append_user_message(
        vec![ContentPart::Text {
            text: "p3".to_string(),
        }],
        PromptOrigin::User,
    );
    mem.append_message(assistant_text_msg("a3"));

    assert_eq!(mem.history().len(), 6);

    mem.undo_count(2).unwrap();

    // Should remove p3+a3+p2+a2, leaving p1+a1.
    assert_eq!(mem.history().len(), 2);
    assert_eq!(
        mem.history()[0].message.content[0],
        ContentPart::Text {
            text: "p1".to_string()
        }
    );
}

// ---------------------------------------------------------------------------
// Extra: undo at compaction boundary fails gracefully
// ---------------------------------------------------------------------------

#[test]
fn should_fail_undo_when_crossing_compaction_boundary() {
    let mut mem = ContextMemory::new();
    mem.append_user_message(
        vec![ContentPart::Text {
            text: "p1".to_string(),
        }],
        PromptOrigin::User,
    );
    // Insert a compaction summary.
    mem.append_message(ContextMessage {
        message: Message::user("summary"),
        origin: Some(PromptOrigin::CompactionSummary),
        is_error: false,
        note: None,
        with_skills: None,
        entry_seq: 0,
    });
    mem.append_user_message(
        vec![ContentPart::Text {
            text: "p2".to_string(),
        }],
        PromptOrigin::User,
    );

    // Undo should only remove p2 (can't cross compaction boundary).
    mem.undo().unwrap();
    assert_eq!(mem.history().len(), 2);

    // Second undo should fail (would cross compaction boundary).
    let result = mem.undo();
    assert!(result.is_err());
}
