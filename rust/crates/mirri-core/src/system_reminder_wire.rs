//! Public helpers for recording system-reminder `context.append_message`
//! wire ops (the durable half of every injected system reminder).
//!
//! Extracted from `turn_flow/hooks.rs` so the embedded handler and restore
//! paths can reuse the same wire record shape without reaching into the
//! turn-flow internals.

use crate::loop_engine::WireOpRecorder;
use mirri_proto::WireOp;

/// Record one durable `context.append_message` per injected system
/// reminder (the per-step injections, tool-result injections, and
/// post-compaction reminders the live loop appends to context memory).
///
/// Every system reminder appended to live memory must be recorded, or the
/// wire replay — and therefore the restored session's snapshot — misses
/// the message entirely (the wire replay equivalence invariant, ADR-009).
/// The message JSON mirrors byte-for-byte what
/// [`ContextMemory::append_system_reminder`](crate::context_memory::ContextMemory::append_system_reminder) stores (the
/// `<system-reminder>` wrapping included) with the Injection origin the
/// append stamps. No-op when no recorder is installed; failures are
/// swallowed by the recorder (best-effort persistence never fails the
/// engine loop).
pub async fn record_system_reminders(
    wire_recorder: Option<&dyn WireOpRecorder>,
    reminders: &[String],
) {
    let Some(recorder) = wire_recorder else {
        return;
    };
    for text in reminders {
        recorder.record_op(system_reminder_op(text)).await;
    }
}

/// Build the `context.append_message` wire op for one system reminder,
/// mirroring byte-for-byte what
/// [`ContextMemory::append_system_reminder`](crate::context_memory::ContextMemory::append_system_reminder)
/// stores (the `<system-reminder>` wrapping included, see
/// [`system_reminder_text`]) with the `injection` origin the append
/// stamps.
pub fn system_reminder_op(text: &str) -> WireOp {
    WireOp::ContextAppendMessage {
        message: serde_json::json!({
            "role": "user",
            "content": [{"type": "text", "text": system_reminder_text(text)}],
            "origin": { "kind": "injection" },
        }),
    }
}

/// Wrap `text` in the `<system-reminder>` envelope (with surrounding
/// newlines, after trimming) exactly as
/// [`ContextMemory::append_system_reminder`](crate::context_memory::ContextMemory::append_system_reminder)
/// stores it.
pub fn system_reminder_text(text: &str) -> String {
    format!("<system-reminder>\n{}\n</system-reminder>", text.trim())
}
