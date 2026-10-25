//! Wire-log replay — reconstructing a [`ContextMemory`] from a folded
//! [`AgentState`].
//!
//! `resume_session` folds the session's `wire.jsonl` through
//! [`mirri_proto::reduce`] to rebuild an [`AgentState`]; this module converts
//! that state back into the engine's canonical history store
//! ([`ContextMemory`]).
//!
//! ## Message conversion
//!
//! [`ConversationState`](mirri_proto::state::ConversationState) stores
//! messages as raw JSON exactly as they appear
//! on the wire. The TS kernel writes camelCase shapes (`toolCalls`,
//! `toolCallId`, `isError`) while Rust-native records use snake_case
//! (`tool_calls`, `tool_call_id`, `is_error`); the converters here accept
//! both spellings. A plain-string `content` (a shape some TS records use) is
//! wrapped into a single text part.
//!
//! ## Reconstruction order
//!
//! When the folded state carries a `timeline` (the interleaved wire-op
//! order the reducer records), messages and loop events replay in that
//! order — each turn's user message precedes the step events of its
//! assistant reply, matching how the turn ran live. States without a
//! timeline (written before the field existed) fall back to the lossy
//! two-phase order: compaction summary → messages → loop events. Any tool
//! exchange still open at the end of the log is closed with a synthetic
//! interrupted result, mirroring the TS kernel's end-of-resume behaviour.

use mirri_proto::AgentState;
use mirri_proto::state::TimelineEntry;
use tracing::{debug, warn};

use crate::blob::AgentBlobService;
use crate::context::ContextMessage;
use crate::context_memory::{ContextMemory, LoopEvent};
use crate::message::{ContentPart, Message, Role, ToolCall, ToolCallFunction, WithSkillsEntry};
use crate::origin::PromptOrigin;
use crate::tool::ToolOutput;

/// Reconstruct a [`ContextMemory`] from a replayed [`AgentState`].
///
/// Order: compaction summary (if any) → conversation entries in the state's
/// recorded timeline (wire-op order; the lossy messages-then-loop-events
/// two-phase order only for pre-timeline states). A message or event whose
/// shape is not recognised is skipped with a warning — the same resilience
/// the wire log reader applies to malformed lines — so a single exotic
/// record never fails the whole resume.
///
/// A tool exchange still open at the end of the log is closed via
/// [`ContextMemory::close_pending_tool_results`], which synthesises
/// interrupted tool results as errors.
///
/// `blobref:` media URLs in message content parts are left as-is; use
/// [`reconstruct_context_memory_with_blobs`] to rehydrate them to `data:`
/// URIs during reconstruction.
pub fn reconstruct_context_memory(state: &AgentState) -> ContextMemory {
    reconstruct_context_memory_inner(state, None)
}

/// Like [`reconstruct_context_memory`], but rehydrates `blobref:` media
/// URLs in message content parts back to `data:` URIs via `blob_service`
/// (mirroring the TS kernel's post-restore rehydrate pass). Blobs that
/// cannot be loaded degrade to a text placeholder instead of failing.
pub fn reconstruct_context_memory_with_blobs(
    state: &AgentState,
    blob_service: &AgentBlobService,
) -> ContextMemory {
    reconstruct_context_memory_inner(state, Some(blob_service))
}

/// Shared reconstruction core; `blob_service` is `None` when blob
/// rehydration is not desired.
fn reconstruct_context_memory_inner(
    state: &AgentState,
    blob_service: Option<&AgentBlobService>,
) -> ContextMemory {
    let conversation = &state.conversation;
    let mut memory = ContextMemory::new();

    // 1. Compaction summary, if the log contains a compaction. Every apply
    //    branch of the reducer (Task 7's three-branch selection) embeds the
    //    summary as a user message tagged `compaction_summary` — the
    //    rust-native record between head essence and tail, the TS newer
    //    record LAST (after the kept user segments), the TS legacy record
    //    FIRST (ahead of the verbatim tail) — so the summary message is
    //    replayed at its embedded position and must NOT be prepended again.
    //    The `summary_embedded` check below is position-agnostic: it looks
    //    for the tag anywhere in the folded messages.
    let summary_embedded = conversation.messages.iter().any(|raw| {
        raw.get("origin")
            .and_then(|o| o.get("kind"))
            .and_then(serde_json::Value::as_str)
            == Some("compaction_summary")
    });
    if !summary_embedded && let Some(summary) = &conversation.summary {
        memory.append_message(ContextMessage {
            message: Message::user(summary.clone()),
            origin: Some(PromptOrigin::CompactionSummary),
            is_error: false,
            note: None,
            with_skills: None,
            entry_seq: 0,
        });
    }

    // 2+3. Conversation messages and loop events. When the state's timeline
    //     covers every message and loop event (every state folded since the
    //     timeline field exists), the entries replay in the original wire-op
    //     order — each turn's user message lands before the step events that
    //     streamed its assistant reply, exactly as the turn ran live. A state
    //     whose coverage is partial or absent (snapshots written before the
    //     field existed, plus ops folded after them) falls back to the lossy
    //     two-phase order: all messages, then all loop events.
    if timeline_covers_entries(conversation) {
        for entry in &conversation.timeline {
            match entry {
                TimelineEntry::Message { index } => {
                    let Some(raw) = conversation.messages.get(*index) else {
                        continue;
                    };
                    match parse_context_message(raw, blob_service) {
                        Some(message) => memory.append_message(message),
                        None => warn!(
                            raw_message = %raw,
                            "skipping wire message that could not be converted to ContextMessage"
                        ),
                    }
                }
                TimelineEntry::LoopEvent { index } => {
                    let Some(raw) = conversation.loop_events.get(*index) else {
                        continue;
                    };
                    match parse_loop_event(raw) {
                        Some(event) => memory.append_loop_event(event),
                        None => debug!(
                            raw_event = %raw,
                            "skipping wire loop event that could not be converted to LoopEvent"
                        ),
                    }
                }
            }
        }
    } else {
        for raw in &conversation.messages {
            match parse_context_message(raw, blob_service) {
                Some(message) => memory.append_message(message),
                None => warn!(
                    raw_message = %raw,
                    "skipping wire message that could not be converted to ContextMessage"
                ),
            }
        }
        for raw in &conversation.loop_events {
            match parse_loop_event(raw) {
                Some(event) => memory.append_loop_event(event),
                None => debug!(
                    raw_event = %raw,
                    "skipping wire loop event that could not be converted to LoopEvent"
                ),
            }
        }
    }

    // 4. Close any tool exchange that was still open when the log ended —
    //    the agent process died mid-step. Steps whose step.end never made
    //    it to the journal are settled first so their assistant message
    //    survives projection instead of orphaning the synthesized results;
    //    the results themselves are errors so the model does not assume
    //    the tool completed.
    memory.settle_open_steps();
    memory.close_pending_tool_results();

    memory
}

/// Whether the conversation's timeline references every message and every
/// loop event exactly once — the precondition for replaying it as the
/// authoritative wire-op order. Partial coverage (a pre-timeline snapshot
/// base plus newer ops, or a corrupt entry set) must not silently drop the
/// uncovered records, so the caller falls back to the lossy two-phase order.
fn timeline_covers_entries(conversation: &mirri_proto::state::ConversationState) -> bool {
    if conversation.timeline.is_empty() {
        return false;
    }
    let mut messages_covered = vec![false; conversation.messages.len()];
    let mut events_covered = vec![false; conversation.loop_events.len()];
    for entry in &conversation.timeline {
        match entry {
            TimelineEntry::Message { index } => {
                match messages_covered.get_mut(*index) {
                    Some(slot) if !*slot => *slot = true,
                    // Out-of-range or duplicated entry: the timeline is not
                    // a faithful order of the vectors.
                    _ => return false,
                }
            }
            TimelineEntry::LoopEvent { index } => match events_covered.get_mut(*index) {
                Some(slot) if !*slot => *slot = true,
                _ => return false,
            },
        }
    }
    messages_covered.into_iter().all(|covered| covered)
        && events_covered.into_iter().all(|covered| covered)
}

/// Parse a raw wire message JSON value into a [`ContextMessage`].
///
/// Accepts the TS camelCase spellings (`toolCalls`, `toolCallId`, `isError`)
/// and the Rust-native snake_case spellings (`tool_calls`, `tool_call_id`,
/// `is_error`). `content` may be an array of content parts (wire shapes
/// `{"type":"text",...}`, `{"type":"think",...}`, media URLs) or a plain
/// string. Returns `None` when the record has no recognisable `role`.
///
/// `blob_service` rehydrates `blobref:` media URLs in the message's content
/// parts when present.
pub(crate) fn parse_context_message(
    raw: &serde_json::Value,
    blob_service: Option<&AgentBlobService>,
) -> Option<ContextMessage> {
    let object = raw.as_object()?;
    let role: Role = serde_json::from_value(object.get("role")?.clone()).ok()?;
    let name = object
        .get("name")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);

    let content = parse_content_parts(object.get("content"), blob_service);
    let tool_calls = parse_tool_calls(object.get("toolCalls").or_else(|| object.get("tool_calls")));
    let tool_call_id = object
        .get("toolCallId")
        .or_else(|| object.get("tool_call_id"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    let partial = object
        .get("partial")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);

    let origin = object
        .get("origin")
        .and_then(|value| serde_json::from_value::<PromptOrigin>(value.clone()).ok());
    let with_skills = object
        .get("with_skills")
        .and_then(|value| serde_json::from_value::<Vec<WithSkillsEntry>>(value.clone()).ok());
    let is_error = object
        .get("isError")
        .or_else(|| object.get("is_error"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let note = object
        .get("note")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    // Session-cumulative entry number (ADR-007): the reducer's fold stamps
    // `entrySeq` onto messages retained through compaction so the replay
    // preserves their original numbers (wire payloads never carry it — the
    // in-memory fold state does).
    let entry_seq = object
        .get("entrySeq")
        .or_else(|| object.get("entry_seq"))
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);

    Some(ContextMessage {
        message: Message {
            role,
            name,
            content,
            tool_calls,
            tool_call_id,
            partial,
        },
        origin,
        is_error,
        note,
        with_skills,
        entry_seq,
    })
}

/// Parse a message `content` field into content parts.
///
/// A plain string is wrapped into a single text part. An array is parsed
/// part-by-part; unrecognised part shapes are dropped so one exotic part
/// does not lose the whole message. `blob_service` rehydrates `blobref:`
/// media URLs in the parsed parts when present.
fn parse_content_parts(
    value: Option<&serde_json::Value>,
    blob_service: Option<&AgentBlobService>,
) -> Vec<ContentPart> {
    let Some(value) = value else {
        return Vec::new();
    };
    match value {
        serde_json::Value::String(text) => vec![ContentPart::Text { text: text.clone() }],
        serde_json::Value::Array(parts) => {
            let parsed: Vec<ContentPart> = parts
                .iter()
                .filter_map(|part| match serde_json::from_value::<ContentPart>(part.clone()) {
                    Ok(part) => Some(part),
                    Err(error) => {
                        debug!(raw_part = %part, error = %error, "dropping unrecognised content part");
                        None
                    }
                })
                .collect();
            match blob_service {
                Some(service) => service.load_parts(&parsed),
                None => parsed,
            }
        }
        _ => Vec::new(),
    }
}

/// Parse a message `toolCalls` / `tool_calls` field into tool calls.
fn parse_tool_calls(value: Option<&serde_json::Value>) -> Vec<ToolCall> {
    let Some(serde_json::Value::Array(calls)) = value else {
        return Vec::new();
    };
    calls
        .iter()
        .filter_map(|call| {
            let object = call.as_object()?;
            let id = object.get("id")?.as_str()?.to_string();
            let function = object.get("function")?.as_object()?;
            let name = function.get("name")?.as_str()?.to_string();
            let arguments = match function.get("arguments") {
                Some(serde_json::Value::String(arguments)) => arguments.clone(),
                Some(other) => other.to_string(),
                None => String::new(),
            };
            Some(ToolCall {
                id,
                call_type: "function".to_string(),
                function: ToolCallFunction { name, arguments },
            })
        })
        .collect()
}

/// Parse a raw wire loop-event JSON value into a [`LoopEvent`].
///
/// Recognises the TS shapes: `step.begin`, `content.part`, `think.part`,
/// `tool.call`, `tool.result`, and `step.end`. The step identifier is read
/// from `uuid` (TS spelling) or `stepId`. Unknown event types are skipped.
pub(crate) fn parse_loop_event(raw: &serde_json::Value) -> Option<LoopEvent> {
    let object = raw.as_object()?;
    let event_type = object.get("type")?.as_str()?;
    let step_id_value = || {
        object
            .get("uuid")
            .or_else(|| object.get("stepId"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
    };
    let text_part = |part: &serde_json::Value| match part {
        serde_json::Value::String(text) => Some(text.clone()),
        _ => object_text(part),
    };
    // ADR-007: the reducer stamps `entrySeq` on step.begin / tool.result
    // events retained through compaction, so the replay materializes their
    // messages with the original numbers.
    let entry_seq = || {
        object
            .get("entrySeq")
            .or_else(|| object.get("entry_seq"))
            .and_then(serde_json::Value::as_u64)
    };

    match event_type {
        "step.begin" => Some(LoopEvent::StepBegin {
            step_id: step_id_value()?,
            entry_seq: entry_seq(),
        }),
        "content.part" => {
            let part = object.get("part")?;
            let is_think = part
                .as_object()
                .and_then(|p| p.get("type"))
                .and_then(serde_json::Value::as_str)
                == Some("think");
            let text = text_part(part)?;
            if is_think {
                Some(LoopEvent::ThinkPart {
                    step_id: step_id_value()?,
                    text,
                })
            } else {
                Some(LoopEvent::ContentPart {
                    step_id: step_id_value()?,
                    text,
                })
            }
        }
        "think.part" => Some(LoopEvent::ThinkPart {
            step_id: step_id_value()?,
            text: text_part(object.get("part")?)?,
        }),
        "tool.call" => {
            let tool_call_id = object
                .get("callId")
                .or_else(|| object.get("toolCallId"))?
                .as_str()?
                .to_string();
            let tool_name = object.get("name")?.as_str()?.to_string();
            let args = object
                .get("input")
                .or_else(|| object.get("arguments"))
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            Some(LoopEvent::ToolCall {
                step_id: step_id_value()?,
                tool_call_id,
                tool_name,
                args,
            })
        }
        "tool.result" => {
            let tool_call_id = object
                .get("callId")
                .or_else(|| object.get("toolCallId"))?
                .as_str()?
                .to_string();
            let output_val = object.get("output").or_else(|| object.get("result"))?;
            let output = parse_tool_output(output_val)
                .unwrap_or_else(|| ToolOutput::Text(object_text(output_val).unwrap_or_default()));
            let is_error = object
                .get("isError")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            let note = object
                .get("note")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string);
            Some(LoopEvent::ToolResult {
                tool_call_id,
                output,
                is_error,
                note,
                entry_seq: entry_seq(),
            })
        }
        "step.end" => {
            let usage = object.get("usage").and_then(serde_json::Value::as_object);
            let tokens = |key: &str| {
                usage
                    .and_then(|usage| usage.get(key))
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0)
            };
            Some(LoopEvent::StepEnd {
                step_id: step_id_value()?,
                input_tokens: tokens("inputTokens"),
                output_tokens: tokens("outputTokens"),
                // Cache buckets are additive on the wire shape: logs written
                // before the three-bucket carry-through omit them (0 here).
                cache_read_tokens: tokens("cacheReadTokens"),
                cache_creation_tokens: tokens("cacheCreationTokens"),
            })
        }
        _ => None,
    }
}

/// Extract human-readable text from a wire value.
///
/// Accepts a plain string, `{"text": "..."}`, an array of content parts
/// (text parts joined with newlines), or `{"output": [...]}` (the TS
/// `tool.result` shape, where the result is a list of parts).
/// Parse the `output`/`result` field of a `tool.result` wire event into a
/// [`ToolOutput`].
///
/// The wire shape can be:
/// - A plain string → `ToolOutput::Text`
/// - An object with `output` key → recurse into it
/// - An array of content parts → deserialize as `Vec<ContentPart>`; if all
///   parts are text-only, flatten to `ToolOutput::Text`, otherwise
///   `ToolOutput::Parts`
/// - An object with a `text` key → `ToolOutput::Text`
fn parse_tool_output(value: &serde_json::Value) -> Option<ToolOutput> {
    match value {
        serde_json::Value::String(s) => Some(ToolOutput::Text(s.clone())),
        serde_json::Value::Object(object) => {
            if let Some(text) = object.get("text").and_then(serde_json::Value::as_str) {
                return Some(ToolOutput::Text(text.to_string()));
            }
            if let Some(output) = object.get("output") {
                return parse_tool_output(output);
            }
            // A single content-part object (not wrapped in an array).
            if let Ok(part) = serde_json::from_value::<ContentPart>(value.clone()) {
                return Some(ToolOutput::Parts(vec![part]));
            }
            None
        }
        serde_json::Value::Array(parts) => {
            if parts.is_empty() {
                return Some(ToolOutput::Text(String::new()));
            }
            // Try to deserialize as content parts.
            if let Ok(content_parts) =
                serde_json::from_value::<Vec<ContentPart>>(serde_json::Value::Array(parts.clone()))
            {
                // If all parts are text-only, flatten to a single Text output
                // to preserve backward compatibility with the old wire shape.
                if content_parts
                    .iter()
                    .all(|p| matches!(p, ContentPart::Text { .. }))
                {
                    let text = content_parts
                        .iter()
                        .filter_map(|p| match p {
                            ContentPart::Text { text } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("");
                    return Some(ToolOutput::Text(text));
                }
                return Some(ToolOutput::Parts(content_parts));
            }
            // Fallback: join all text from the array.
            object_text(value).map(ToolOutput::Text)
        }
        _ => None,
    }
}

fn object_text(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(text) => Some(text.clone()),
        serde_json::Value::Object(object) => {
            if let Some(text) = object.get("text").and_then(serde_json::Value::as_str) {
                return Some(text.to_string());
            }
            if let Some(output) = object.get("output") {
                return object_text(output);
            }
            None
        }
        serde_json::Value::Array(parts) => {
            let texts: Vec<String> = parts
                .iter()
                .filter_map(|part| match part {
                    serde_json::Value::String(text) => Some(text.clone()),
                    _ => part
                        .as_object()
                        .and_then(|p| p.get("text"))
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_string),
                })
                .collect();
            if texts.is_empty() {
                None
            } else {
                Some(texts.join("\n"))
            }
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::ImageUrlData;
    use serde_json::json;

    #[test]
    fn should_parse_ts_wire_message_when_message_uses_camel_case_tool_calls() {
        let raw = json!({
            "role": "assistant",
            "content": [{"type": "text", "text": "let me check"}],
            "toolCalls": [{
                "id": "call-1",
                "type": "function",
                "function": {"name": "Read", "arguments": "{\"path\":\"/tmp/f\"}"}
            }],
            "origin": {"kind": "user"},
            "isError": false
        });
        let message = parse_context_message(&raw, None).expect("parse");
        assert_eq!(message.message.role, Role::Assistant);
        assert_eq!(message.message.text_content(), "let me check");
        assert_eq!(message.message.tool_calls.len(), 1);
        assert_eq!(message.message.tool_calls[0].id, "call-1");
        assert_eq!(message.message.tool_calls[0].function.name, "Read");
        assert_eq!(message.origin, Some(PromptOrigin::User));
    }

    #[test]
    fn should_parse_native_message_when_message_uses_snake_case_fields() {
        let raw = json!({
            "role": "tool",
            "content": [{"type": "text", "text": "out"}],
            "tool_call_id": "call-2",
            "is_error": true,
            "note": "interrupted"
        });
        let message = parse_context_message(&raw, None).expect("parse");
        assert_eq!(message.message.role, Role::Tool);
        assert_eq!(message.message.tool_call_id.as_deref(), Some("call-2"));
        assert!(message.is_error);
        assert_eq!(message.note.as_deref(), Some("interrupted"));
    }

    #[test]
    fn should_wrap_plain_string_content_when_message_content_is_a_string() {
        let raw = json!({"role": "user", "content": "hello"});
        let message = parse_context_message(&raw, None).expect("parse");
        assert_eq!(message.message.role, Role::User);
        assert_eq!(message.message.text_content(), "hello");
    }

    #[test]
    fn should_reject_message_when_role_is_missing() {
        let raw = json!({"content": [{"type": "text", "text": "no role"}]});
        assert!(parse_context_message(&raw, None).is_none());
    }

    #[test]
    fn should_extract_with_skills_when_journal_message_carries_the_array() {
        // Given a journal record with a with_skills array (ADR-006 wire key)
        let raw = json!({
            "role": "user",
            "content": [{"type": "text", "text": "rewritten"}],
            "with_skills": [
                {"name": "read-file", "target": "file:///tmp/a.txt", "trigger": "mentioned"}
            ]
        });

        // When parse_context_message extracts it
        let message = parse_context_message(&raw, None).expect("parse");

        // Then the entries land on the ContextMessage with all three fields
        let skills = message.with_skills.expect("with_skills");
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "read-file");
        assert_eq!(skills[0].target, "file:///tmp/a.txt");
        assert_eq!(skills[0].trigger, "mentioned");
    }

    #[test]
    fn should_degrade_to_none_when_with_skills_is_malformed() {
        // Given a journal record whose with_skills array has a bad entry
        let raw = json!({
            "role": "user",
            "content": [{"type": "text", "text": "rewritten"}],
            "with_skills": [{"name": "read-file"}]
        });

        // When parse_context_message extracts it
        let message = parse_context_message(&raw, None).expect("parse");

        // Then the malformed array degrades fail-soft to None
        assert_eq!(message.with_skills, None);
    }

    #[test]
    fn should_leave_with_skills_none_when_journal_message_has_no_field() {
        // Given a legacy journal record written before ADR-006
        let raw = json!({
            "role": "user",
            "content": [{"type": "text", "text": "plain"}]
        });

        // When parse_context_message extracts it
        let message = parse_context_message(&raw, None).expect("parse");

        // Then with_skills stays None
        assert_eq!(message.with_skills, None);
    }

    #[test]
    fn should_parse_step_lifecycle_events_when_given_ts_wire_shapes() {
        let step_begin =
            parse_loop_event(&json!({"type": "step.begin", "uuid": "s1"})).expect("step.begin");
        assert!(matches!(step_begin, LoopEvent::StepBegin { step_id, .. } if step_id == "s1"));

        let content_part = parse_loop_event(&json!({
            "type": "content.part",
            "part": {"type": "text", "text": "txt"},
            "uuid": "s1"
        }))
        .expect("content.part");
        assert!(
            matches!(content_part, LoopEvent::ContentPart { step_id, text } if step_id == "s1" && text == "txt")
        );

        let think_part = parse_loop_event(&json!({
            "type": "think.part",
            "part": {"type": "think", "text": "hmm"},
            "uuid": "s1"
        }))
        .expect("think.part");
        assert!(matches!(think_part, LoopEvent::ThinkPart { text, .. } if text == "hmm"));

        let tool_call = parse_loop_event(&json!({
            "type": "tool.call",
            "callId": "call-1",
            "name": "Read",
            "input": {"path": "/tmp/f"},
            "uuid": "s1"
        }))
        .expect("tool.call");
        assert!(matches!(
            tool_call,
            LoopEvent::ToolCall { step_id, tool_call_id, tool_name, .. }
                if step_id == "s1" && tool_call_id == "call-1" && tool_name == "Read"
        ));

        let tool_result = parse_loop_event(&json!({
            "type": "tool.result",
            "callId": "call-1",
            "result": {"output": [{"type": "text", "text": "file contents"}]},
            "uuid": "s1",
            "isError": true
        }))
        .expect("tool.result");
        assert!(matches!(
            tool_result,
            LoopEvent::ToolResult { tool_call_id, output, is_error, .. }
                if tool_call_id == "call-1"
                    && matches!(output, ToolOutput::Text(ref s) if s == "file contents")
                    && is_error
        ));

        let step_end = parse_loop_event(&json!({
            "type": "step.end",
            "uuid": "s1",
            "usage": {"inputTokens": 10, "outputTokens": 5}
        }))
        .expect("step.end");
        assert!(matches!(
            step_end,
            LoopEvent::StepEnd { step_id, input_tokens, output_tokens, .. }
                if step_id == "s1" && input_tokens == 10 && output_tokens == 5
        ));
    }

    #[test]
    fn should_parse_cache_buckets_when_step_end_usage_carries_them() {
        // Given a step.end wire event whose usage includes the cache buckets
        // (the shape a live turn records since the three-bucket
        // carry-through)
        let step_end = parse_loop_event(&json!({
            "type": "step.end",
            "uuid": "s1",
            "usage": {
                "inputTokens": 10,
                "outputTokens": 5,
                "cacheReadTokens": 200,
                "cacheCreationTokens": 40
            }
        }))
        .expect("step.end");

        // Then the parsed event carries all four buckets
        assert!(matches!(
            step_end,
            LoopEvent::StepEnd {
                step_id,
                input_tokens,
                output_tokens,
                cache_read_tokens,
                cache_creation_tokens,
            } if step_id == "s1"
                && input_tokens == 10
                && output_tokens == 5
                && cache_read_tokens == 200
                && cache_creation_tokens == 40
        ));
    }

    #[test]
    fn should_reject_loop_event_when_type_is_unknown() {
        let raw = json!({"type": "step.steer", "uuid": "s1"});
        assert!(parse_loop_event(&raw).is_none());
    }

    #[test]
    fn should_roundtrip_loop_events_when_converting_to_wire_value_and_parsing() {
        // Every `LoopEvent` must survive `to_wire_value` → `parse_loop_event`
        // unchanged: the wire shapes a live turn records must replay into the
        // exact event the engine folded into memory.
        let step_begin = LoopEvent::StepBegin {
            step_id: "s1".to_string(),
            entry_seq: None,
        };
        assert!(matches!(
            parse_loop_event(&step_begin.to_wire_value()).expect("step.begin round-trips"),
            LoopEvent::StepBegin { step_id, .. } if step_id == "s1"
        ));

        let content_part = LoopEvent::ContentPart {
            step_id: "s1".to_string(),
            text: "txt".to_string(),
        };
        assert!(matches!(
            parse_loop_event(&content_part.to_wire_value()).expect("content.part round-trips"),
            LoopEvent::ContentPart { step_id, text } if step_id == "s1" && text == "txt"
        ));

        let think_part = LoopEvent::ThinkPart {
            step_id: "s1".to_string(),
            text: "hmm".to_string(),
        };
        assert!(matches!(
            parse_loop_event(&think_part.to_wire_value()).expect("think.part round-trips"),
            LoopEvent::ThinkPart { step_id, text } if step_id == "s1" && text == "hmm"
        ));

        let tool_call = LoopEvent::ToolCall {
            step_id: "s1".to_string(),
            tool_call_id: "call-1".to_string(),
            tool_name: "Read".to_string(),
            args: json!({"path": "/tmp/f"}),
        };
        assert!(matches!(
            parse_loop_event(&tool_call.to_wire_value()).expect("tool.call round-trips"),
            LoopEvent::ToolCall { step_id, tool_call_id, tool_name, args }
                if step_id == "s1"
                    && tool_call_id == "call-1"
                    && tool_name == "Read"
                    && args == json!({"path": "/tmp/f"})
        ));

        let tool_result = LoopEvent::ToolResult {
            tool_call_id: "call-1".to_string(),
            output: ToolOutput::Text("file contents".to_string()),
            is_error: true,
            note: Some("interrupted".to_string()),
            entry_seq: None,
        };
        assert!(matches!(
            parse_loop_event(&tool_result.to_wire_value()).expect("tool.result round-trips"),
            LoopEvent::ToolResult { tool_call_id, output, is_error, note, .. }
                if tool_call_id == "call-1"
                    && matches!(output, ToolOutput::Text(ref s) if s == "file contents")
                    && is_error
                    && note.as_deref() == Some("interrupted")
        ));

        // ToolResult with Parts (image content) must also survive round-trip
        // — this is the critical path for ReadMediaFile tool output.
        let tool_result_parts = LoopEvent::ToolResult {
            tool_call_id: "call-2".to_string(),
            output: ToolOutput::Parts(vec![
                ContentPart::Text {
                    text: "<image data>".into(),
                },
                ContentPart::ImageUrl {
                    image_url: ImageUrlData {
                        url: "data:image/png;base64,iVBORw0KGgo=".into(),
                        detail: None,
                        id: None,
                    },
                    detail: None,
                },
            ]),
            is_error: false,
            note: None,
            entry_seq: None,
        };
        let parsed_parts = parse_loop_event(&tool_result_parts.to_wire_value())
            .expect("tool.result with Parts round-trips");
        match parsed_parts {
            LoopEvent::ToolResult {
                tool_call_id,
                output,
                is_error,
                note,
                ..
            } => {
                assert_eq!(tool_call_id, "call-2");
                assert!(!is_error);
                assert!(note.is_none());
                match output {
                    ToolOutput::Parts(parts) => {
                        assert_eq!(parts.len(), 2);
                        assert!(
                            matches!(&parts[0], ContentPart::Text { text } if text == "<image data>")
                        );
                        assert!(matches!(&parts[1], ContentPart::ImageUrl { image_url, .. }
                            if image_url.url == "data:image/png;base64,iVBORw0KGgo="));
                    }
                    other => panic!("expected ToolOutput::Parts, got {other:?}"),
                }
            }
            other => panic!("expected ToolResult, got {other:?}"),
        }

        let step_end = LoopEvent::StepEnd {
            step_id: "s1".to_string(),
            input_tokens: 10,
            output_tokens: 5,
            cache_read_tokens: 200,
            cache_creation_tokens: 40,
        };
        assert!(matches!(
            parse_loop_event(&step_end.to_wire_value()).expect("step.end round-trips"),
            LoopEvent::StepEnd {
                step_id,
                input_tokens,
                output_tokens,
                cache_read_tokens,
                cache_creation_tokens,
            } if step_id == "s1"
                && input_tokens == 10
                && output_tokens == 5
                && cache_read_tokens == 200
                && cache_creation_tokens == 40
        ));
    }

    #[test]
    fn should_replay_cached_step_end_with_same_formula_when_restoring_from_wire() {
        // Given a wire-recorded turn whose step.end usage carries non-zero
        // cache buckets (the shape every cache-enabled provider reports)
        let step_events = [
            json!({"type": "step.begin", "uuid": "s1"}),
            json!({"type": "content.part", "uuid": "s1", "part": {"type": "text", "text": "answer"}}),
            json!({
                "type": "step.end",
                "uuid": "s1",
                "usage": {
                    "inputTokens": 15,
                    "outputTokens": 5,
                    "cacheReadTokens": 200,
                    "cacheCreationTokens": 40
                }
            }),
        ];
        let mut state = AgentState::new();
        state.conversation.messages.push(json!({
            "role": "user",
            "content": [{"type": "text", "text": "hello"}],
            "toolCalls": []
        }));
        for event in &step_events {
            state.conversation.loop_events.push(event.clone());
        }

        // And the same events folded live (what the running session's memory
        // would hold — the wire values above are exactly `to_wire_value`
        // output)
        let mut live = ContextMemory::new();
        live.append_message(parse_context_message(&state.conversation.messages[0], None).unwrap());
        for raw in &step_events {
            live.append_loop_event(parse_loop_event(raw).expect("wire event parses"));
        }

        // When the memory is restored from the wire state
        let replayed = reconstruct_context_memory(&state);

        // Then the replayed token_count uses the same four-bucket formula
        // as the live fold (D-6): restored sessions agree with live ones by
        // construction, and old logs recorded with cache buckets gain the
        // corrected (larger, truer) pressure on restore.
        assert_eq!(live.token_count(), 260);
        assert_eq!(replayed.token_count(), live.token_count());
        assert_eq!(replayed.token_count(), 260);
    }

    #[test]
    fn should_append_messages_and_loop_events_in_order_when_reconstructing_memory() {
        let mut state = AgentState::new();
        state.conversation.messages.push(json!({
            "role": "user",
            "content": [{"type": "text", "text": "hello"}],
            "toolCalls": []
        }));
        state.conversation.loop_events.push(json!({
            "type": "step.begin",
            "uuid": "s1"
        }));
        state.conversation.loop_events.push(json!({
            "type": "content.part",
            "part": {"type": "text", "text": "answer"},
            "uuid": "s1"
        }));

        let memory = reconstruct_context_memory(&state);
        let history = memory.history();
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].message.role, Role::User);
        // Replaying step.begin opens an assistant message that the
        // log-ending settle closes (no step.end was recorded — a
        // hard-quit mid-step): it survives projection settled, not
        // flagged partial.
        assert_eq!(history[1].message.role, Role::Assistant);
        assert!(!history[1].message.partial);
        assert_eq!(history[1].message.text_content(), "answer");
    }

    #[test]
    fn should_drop_vacuous_step_when_log_ends_before_anything_was_delivered() {
        // Given a wire log whose only step event is a bare step.begin —
        // nothing sendable was recorded before the process died
        let mut state = AgentState::new();
        state.conversation.messages.push(json!({
            "role": "user",
            "content": [{"type": "text", "text": "hello"}],
            "toolCalls": []
        }));
        state.conversation.loop_events.push(json!({
            "type": "step.begin",
            "uuid": "s1"
        }));

        // When the memory is restored from the wire state
        let memory = reconstruct_context_memory(&state);

        // Then the vacuous assistant placeholder is dropped — the same
        // semantics a recorded step.end applies to an empty step
        let history = memory.history();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].message.role, Role::User);
    }

    #[test]
    fn should_interleave_messages_and_loop_events_when_state_carries_a_timeline() {
        // Given a state whose timeline records the wire-op order of two
        // turns: user message, assistant step, second user message, second
        // assistant step (the interleaving the two separate vectors lose)
        let mut state = AgentState::new();
        state.conversation.messages.push(json!({
            "role": "user",
            "content": [{"type": "text", "text": "first question"}],
            "toolCalls": []
        }));
        state.conversation.loop_events.push(json!({
            "type": "step.begin", "uuid": "s1"
        }));
        state.conversation.loop_events.push(json!({
            "type": "content.part", "part": {"type": "text", "text": "first answer"}, "uuid": "s1"
        }));
        state.conversation.loop_events.push(json!({
            "type": "step.end", "uuid": "s1"
        }));
        state.conversation.messages.push(json!({
            "role": "user",
            "content": [{"type": "text", "text": "second question"}],
            "toolCalls": []
        }));
        state.conversation.loop_events.push(json!({
            "type": "step.begin", "uuid": "s2"
        }));
        state.conversation.loop_events.push(json!({
            "type": "content.part", "part": {"type": "text", "text": "second answer"}, "uuid": "s2"
        }));
        state.conversation.loop_events.push(json!({
            "type": "step.end", "uuid": "s2"
        }));
        state.conversation.timeline = vec![
            TimelineEntry::Message { index: 0 },
            TimelineEntry::LoopEvent { index: 0 },
            TimelineEntry::LoopEvent { index: 1 },
            TimelineEntry::LoopEvent { index: 2 },
            TimelineEntry::Message { index: 1 },
            TimelineEntry::LoopEvent { index: 3 },
            TimelineEntry::LoopEvent { index: 4 },
            TimelineEntry::LoopEvent { index: 5 },
        ];

        // When reconstructing the memory
        let memory = reconstruct_context_memory(&state);
        let history = memory.history();

        // Then each user message precedes its own assistant reply — the
        // interleaved wire order, not users-then-assistants
        assert_eq!(history.len(), 4, "user + assistant per turn");
        assert_eq!(history[0].message.role, Role::User);
        assert_eq!(history[0].message.text_content(), "first question");
        assert_eq!(history[1].message.role, Role::Assistant);
        assert_eq!(history[1].message.text_content(), "first answer");
        assert_eq!(history[2].message.role, Role::User);
        assert_eq!(history[2].message.text_content(), "second question");
        assert_eq!(history[3].message.role, Role::Assistant);
        assert_eq!(history[3].message.text_content(), "second answer");
    }

    #[test]
    fn should_fall_back_to_two_phase_order_when_timeline_coverage_is_partial() {
        // Given a state whose timeline references an index beyond the
        // vectors (a corrupt or hand-written snapshot), reconstruction must
        // not follow the broken timeline — and must not drop the records it
        // fails to cover either: it falls back to the two-phase order
        let mut state = AgentState::new();
        state.conversation.messages.push(json!({
            "role": "user",
            "content": [{"type": "text", "text": "hello"}],
            "toolCalls": []
        }));
        state.conversation.timeline = vec![
            TimelineEntry::Message { index: 0 },
            TimelineEntry::LoopEvent { index: 9 },
            TimelineEntry::Message { index: 7 },
        ];

        let memory = reconstruct_context_memory(&state);
        let history = memory.history();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].message.role, Role::User);
    }

    #[test]
    fn should_close_open_tool_exchange_when_reconstructing_memory_with_unfinished_tool_call() {
        let mut state = AgentState::new();
        state.conversation.loop_events.push(json!({
            "type": "step.begin",
            "uuid": "s1"
        }));
        state.conversation.loop_events.push(json!({
            "type": "tool.call",
            "callId": "call-1",
            "name": "Read",
            "input": {"path": "/tmp/f"},
            "uuid": "s1"
        }));
        // No tool.result and no step.end: the exchange is still open when the
        // log ends. The reconstruction must synthesize an interrupted result.

        let memory = reconstruct_context_memory(&state);
        let history = memory.history();
        assert_eq!(history.len(), 2, "assistant step + synthesized tool result");
        assert_eq!(history[1].message.role, Role::Tool);
        assert!(history[1].is_error);
        assert!(!memory.has_open_tool_exchange());
    }

    #[test]
    fn should_settle_partial_assistant_when_log_ends_mid_tool_exchange() {
        // Given a wire log that ends after the assistant's tool call was
        // recorded but before its result and step.end (a hard-quit mid-step)
        let mut state = AgentState::new();
        state.conversation.messages.push(json!({
            "role": "user",
            "content": [{"type": "text", "text": "read the file"}],
            "toolCalls": []
        }));
        state.conversation.loop_events.push(json!({
            "type": "step.begin", "uuid": "s1"
        }));
        state.conversation.loop_events.push(json!({
            "type": "tool.call",
            "callId": "call-1",
            "name": "Read",
            "input": {"path": "/tmp/f"},
            "uuid": "s1"
        }));

        // When the memory is restored from the wire state
        let memory = reconstruct_context_memory(&state);

        // Then the interrupted step's assistant is settled — it survives
        // projection carrying its tool call, instead of being dropped as a
        // partial message that orphans the synthesized result.
        let history = memory.history();
        let assistant = history
            .iter()
            .find(|m| m.message.role == Role::Assistant)
            .expect("settled assistant survives in history");
        assert!(!assistant.message.partial);
        assert!(
            assistant
                .message
                .tool_calls
                .iter()
                .any(|tc| tc.id == "call-1")
        );

        // And projecting the restored history pairs the synthesized
        // interrupted result with that call — no orphan tool result.
        let (_projected, anomalies) = crate::context_memory::projector::project_with_anomalies(
            history,
            &crate::context::ProjectOptions::normal(),
            Vec::new(),
        )
        .unwrap();
        assert!(
            !anomalies.iter().any(|a| matches!(
                a,
                crate::context::ProjectionAnomaly::OrphanToolResultDropped
            )),
            "no orphan tool result after settle, anomalies: {anomalies:?}"
        );
    }

    #[test]
    fn should_restore_summary_before_messages_when_compacted_state_is_reconstructed() {
        let mut state = AgentState::new();
        state.conversation.summary = Some("context so far".to_string());
        state.conversation.messages.push(json!({
            "role": "user",
            "content": [{"type": "text", "text": "after compaction"}],
            "toolCalls": []
        }));

        let memory = reconstruct_context_memory(&state);
        let history = memory.history();
        assert_eq!(history.len(), 2);
        assert_eq!(
            history[0].origin,
            Some(PromptOrigin::CompactionSummary),
            "summary must come first with the compaction origin"
        );
        assert_eq!(history[0].message.text_content(), "context so far");
        assert_eq!(history[1].message.text_content(), "after compaction");
    }

    #[test]
    fn should_not_prepend_summary_when_reconstructed_state_embeds_it() {
        // Given a new-style reconstructed state: the summary message sits in
        // messages at its live position (after head essence, before tail)
        let mut state = AgentState::new();
        state.conversation.summary = Some("raw".to_string());
        state.conversation.messages.push(json!({
            "role": "user", "content": [{"type": "text", "text": "head q"}],
            "origin": {"kind": "user"}
        }));
        state.conversation.messages.push(json!({
            "role": "user", "content": [{"type": "text", "text": "SUMMARY"}],
            "origin": {"kind": "compaction_summary"}
        }));
        state.conversation.messages.push(json!({
            "role": "user", "content": [{"type": "text", "text": "tail q"}],
            "origin": {"kind": "user"}
        }));
        state.conversation.timeline = vec![
            TimelineEntry::Message { index: 0 },
            TimelineEntry::Message { index: 1 },
            TimelineEntry::Message { index: 2 },
        ];

        // When reconstructed
        let memory = reconstruct_context_memory(&state);
        let history = memory.history();

        // Then the summary is NOT prepended again — it stays at its live
        // position between head and tail
        assert_eq!(history.len(), 3);
        assert_eq!(history[0].message.text_content(), "head q");
        assert_eq!(history[1].origin, Some(PromptOrigin::CompactionSummary));
        assert_eq!(history[2].message.text_content(), "tail q");
    }

    // -----------------------------------------------------------------------
    // Blob rehydration during reconstruction (12F)
    // -----------------------------------------------------------------------

    #[test]
    fn should_rehydrate_blobref_to_data_uri_when_reconstructing_with_blob_service() {
        use crate::blob::{AgentBlobService, format_data_uri};

        let dir = tempfile::tempdir().unwrap();
        let blob_service = AgentBlobService::new(dir.path().to_path_buf());
        let data = vec![0x42; 8000];
        let blob_ref_url = blob_service.offload_string(&format_data_uri("image/png", &data));

        let mut state = AgentState::new();
        state.conversation.messages.push(json!({
            "role": "user",
            "content": [{"type": "image_url", "image_url": {"url": blob_ref_url}}],
            "toolCalls": []
        }));

        let memory = reconstruct_context_memory_with_blobs(&state, &blob_service);
        let history = memory.history();
        let parts = &history[0].message.content;
        assert_eq!(parts.len(), 1, "the image part must survive reconstruction");
        match &parts[0] {
            ContentPart::ImageUrl { image_url, .. } => {
                assert!(
                    image_url.url.starts_with("data:image/png;base64,"),
                    "blobref must be rehydrated to a data URI, got: {}",
                    image_url.url
                );
            }
            other => panic!("expected ImageUrl, got {other:?}"),
        }
    }

    #[test]
    fn should_keep_blobref_untouched_when_reconstructing_without_blob_service() {
        let mut state = AgentState::new();
        state.conversation.messages.push(json!({
            "role": "user",
            "content": [{"type": "image_url", "image_url": {"url": "blobref:image/png;abc123"}}],
            "toolCalls": []
        }));

        let memory = reconstruct_context_memory(&state);
        let history = memory.history();
        match &history[0].message.content[0] {
            ContentPart::ImageUrl { image_url, .. } => {
                assert_eq!(image_url.url, "blobref:image/png;abc123");
            }
            other => panic!("expected ImageUrl, got {other:?}"),
        }
    }

    #[test]
    fn should_replace_missing_blob_with_placeholder_when_reconstructing_with_blob_service() {
        use crate::blob::{AgentBlobService, MISSING_MEDIA_PLACEHOLDER};

        let dir = tempfile::tempdir().unwrap();
        let blob_service = AgentBlobService::new(dir.path().to_path_buf());

        let mut state = AgentState::new();
        state.conversation.messages.push(json!({
            "role": "user",
            "content": [{"type": "image_url", "image_url": {"url": "blobref:image/png;nonexistenthash"}}],
            "toolCalls": []
        }));

        let memory = reconstruct_context_memory_with_blobs(&state, &blob_service);
        let history = memory.history();
        match &history[0].message.content[0] {
            ContentPart::Text { text } => {
                assert_eq!(text, MISSING_MEDIA_PLACEHOLDER);
            }
            other => panic!("expected placeholder Text, got {other:?}"),
        }
    }
}
