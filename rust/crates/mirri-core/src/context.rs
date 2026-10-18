use serde::{Deserialize, Serialize};

use crate::message::{Message, WithSkillsEntry};
use crate::origin::PromptOrigin;

pub mod dir_tree;

/// Number of most-recent media parts to keep intact during degraded
/// projection (mirrors `MEDIA_DEGRADE_KEEP_RECENT` in the TypeScript
/// projector).
pub const MEDIA_DEGRADE_KEEP_RECENT: usize = 2;

/// Micro-compaction: number of most-recent messages always kept untouched
/// when pruning old tool results out of a request projection (P8a-2 修剪
/// 优先于摘要). Mirrors [`crate::compaction::micro`] defaults.
pub const MICRO_COMPACTION_KEEP_RECENT: usize = 20;
/// Minimum estimated token count for a tool result to be eligible for
/// micro-compaction truncation.
pub const MICRO_COMPACTION_MIN_TOKENS: usize = 100;
/// The placeholder text that replaces cleared tool-result content.
pub const MICRO_COMPACTION_MARKER: &str = "[Old tool result content cleared]";

/// The canonical, stored form of a message in context memory.
///
/// Extends the wire [`Message`] with provenance metadata that drives
/// compaction, projection, and undo. The inner `message` is flattened so
/// that `origin`/`is_error`/`note` serialize as siblings to
/// `role`/`content`/etc., matching the TS wire format persisted in
/// `context.jsonl`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextMessage {
    #[serde(flatten)]
    pub message: Message,
    /// Provenance tag — drives compaction keep/drop, undo, replay.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<PromptOrigin>,
    /// Structured error flag for tool results (rendered at projection).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_error: bool,
    /// Model-visible, UI-invisible side channel.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// Skills attached to a rewritten user message (ADR-006). Product-record
    /// only — never forwarded to the model as context. Wire key matches
    /// the TS `with_skills` array exactly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub with_skills: Option<Vec<WithSkillsEntry>>,
    /// Session-cumulative entry number (ADR-007): the monotonic counter
    /// value the owning `ContextMemory` assigned when this message
    /// materialized into model-visible history. Compaction and undo remove
    /// messages but never renumber the survivors or recycle numbers, so a
    /// number referenced by a summary stays stable across compactions.
    ///
    /// In-memory only — `skip` keeps it out of every serialized form
    /// (wire payloads and snapshots alike); numbering crosses the wire via
    /// the apply record's interval fields instead, and a replay re-derives
    /// it at the same materialization points.
    #[serde(skip)]
    pub entry_seq: u64,
}

impl ContextMessage {
    pub fn from_message(message: Message) -> Self {
        Self {
            message,
            origin: None,
            is_error: false,
            note: None,
            with_skills: None,
            entry_seq: 0,
        }
    }

    pub fn with_origin(mut self, origin: PromptOrigin) -> Self {
        self.origin = Some(origin);
        self
    }
}

/// Options controlling projection repair passes.
///
/// Projection is the process of turning the stored context history into a
/// wire-valid message array for the provider. Different call sites need
/// different repair aggressiveness.
#[derive(Debug, Clone, Default)]
pub struct ProjectOptions {
    /// Close trailing open tool calls with synthetic results (used by compaction).
    pub synthesize_missing: bool,
    /// Drop tool results with no matching tool call (every wire send).
    pub drop_orphan_results: bool,
    /// Strict-resend: first message must be `user`.
    pub drop_leading_non_user: bool,
    /// Strict-resend: merge consecutive assistant messages.
    pub merge_consecutive_assistants: bool,
    /// Strict-resend: dedupe duplicate tool calls.
    pub dedupe_duplicate_tool_calls: bool,
    /// Media-degraded: replace **older** media content parts (image, audio,
    /// video) with text placeholders, keeping the most recent media parts
    /// intact. Used as a fallback when the provider rejects the normal
    /// projection due to payload size (413).
    ///
    /// Mirrors the TypeScript `degradeOlderMediaParts` which keeps
    /// `MEDIA_DEGRADE_KEEP_RECENT = 2` most recent media parts and replaces
    /// older ones with a placeholder.
    pub strip_media: bool,
    /// When `strip_media` is set, keep the `keep_recent_media` most recent
    /// media parts across all messages and only replace older ones. When
    /// `0`, all media parts are replaced (full strip).
    pub keep_recent_media: usize,
    /// Micro-compaction: number of recent messages to keep untouched. When
    /// set, tool-result messages before `len - keep_recent` whose content
    /// exceeds the min token threshold have their content replaced with a
    /// placeholder. `None` disables micro-compaction.
    pub micro_compaction_keep_recent: Option<usize>,
    /// Minimum token count for a tool result to be eligible for
    /// micro-compaction truncation.
    pub micro_compaction_min_tokens: usize,
    /// The placeholder text for micro-compacted tool results.
    pub micro_compaction_marker: String,
    /// Whether the media placeholders built during media stripping should
    /// announce a **rejected format** (multimodal spec US6.2 — the E6
    /// format-rejected chain) instead of the request size limit (US6.1 —
    /// the E6 413 chain). Only meaningful with `strip_media` set: the
    /// provider rejected the image's format/encoding, so the placeholder
    /// carries the convert-to-PNG/JPEG/WebP guidance rather than the
    /// re-read hint.
    pub format_rejected: bool,
}

impl ProjectOptions {
    /// Default projection options for every normal wire send.
    ///
    /// Micro-compaction (P8a-2 修剪优先) is **disabled** here: replacing
    /// old tool-result content mid-history mutates the request prefix,
    /// which invalidates the provider's prefix cache from the first mutated
    /// token onward — every subsequent request re-bills the whole tail as
    /// uncached input (observed: ~8.5k re-billed tokens per step on a
    /// 60k-context session). Trimming old tool results must stay an
    /// explicit, gated decision (cold cache + high context usage — see
    /// [`crate::compaction::MicroCompaction`]), never a default of the
    /// every-request projection. An explicit `micro_compaction_keep_recent`
    /// opt-in still routes through the projector.
    pub fn normal() -> Self {
        Self {
            drop_orphan_results: true,
            micro_compaction_keep_recent: None,
            ..Default::default()
        }
    }

    /// Strict projection for resend after provider rejects normal projection.
    pub fn strict() -> Self {
        Self {
            drop_leading_non_user: true,
            merge_consecutive_assistants: true,
            dedupe_duplicate_tool_calls: true,
            ..Self::normal()
        }
    }

    /// Compaction projection (synthesize missing results).
    pub fn compaction() -> Self {
        Self {
            synthesize_missing: true,
            ..Self::normal()
        }
    }

    /// Media-degraded projection: replace **older** media parts with text
    /// placeholders, keeping the 2 most recent media parts intact.
    ///
    /// Used as a fallback when the provider rejects the normal projection due
    /// to payload size (413). Mirrors the TypeScript
    /// `degradeOlderMediaParts` with `keepRecent = 2`.
    pub fn degraded() -> Self {
        Self {
            strip_media: true,
            keep_recent_media: MEDIA_DEGRADE_KEEP_RECENT,
            ..Self::normal()
        }
    }

    /// Media-stripped projection: replace **all** media parts with text
    /// placeholders, plus strict repair passes.
    ///
    /// Used as a second-level fallback when degraded projection still fails.
    /// Combines `strict()` repair passes with `strip_media = true` and
    /// `keep_recent_media = 0` (no media is spared). Mirrors the TypeScript
    /// `MEDIA_STRIPPED_PLACEHOLDERS` behavior with `keepRecent = 0`.
    pub fn stripped() -> Self {
        Self {
            drop_leading_non_user: true,
            merge_consecutive_assistants: true,
            dedupe_duplicate_tool_calls: true,
            strip_media: true,
            keep_recent_media: 0,
            ..Self::normal()
        }
    }

    /// Media-stripped projection with the **format-rejected** placeholder
    /// cause (US6.2): identical repair passes to [`Self::stripped()`], but
    /// the media placeholders announce that the provider rejected the
    /// media's format and carry the convert-to-PNG/JPEG/WebP guidance
    /// instead of the size-limit re-read hint.
    ///
    /// Used by the format-error leg of the media fallback chain (and the
    /// sticky stripped replays it leaves behind for the rest of the turn).
    pub fn stripped_format_rejected() -> Self {
        Self {
            format_rejected: true,
            ..Self::stripped()
        }
    }
}

/// Anomaly reported during projection repair.
///
/// These are surfaced (logged / telemetry) but do not fail the projection;
/// they describe the repair actions taken so observers can detect provider
/// rejections or history corruption.
#[derive(Debug, Clone)]
pub enum ProjectionAnomaly {
    ToolResultReordered,
    ToolResultSynthesized,
    OrphanToolResultDropped,
    DuplicateToolCallDropped,
    DuplicateToolResultDropped,
    LeadingNonUserDropped,
    ConsecutiveAssistantsMerged,
    WhitespaceTextDropped,
    /// An assistant tool call's persisted `function.arguments` text was not
    /// a JSON object (e.g. the literal `"null"` left behind by a corrupted
    /// provider stream) and was sanitized to `{}` before the wire send.
    InvalidToolArgumentsSanitized,
}

/// Snapshot of agent context data.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentContextData {
    pub history: Vec<ContextMessage>,
    pub token_count: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::WithSkillsEntry;

    #[test]
    fn should_serialize_with_skills_as_sibling_key_when_present() {
        // Given a context message carrying a with_skills attachment
        let msg = ContextMessage {
            message: Message::user("rewritten"),
            origin: None,
            is_error: false,
            note: None,
            with_skills: Some(vec![WithSkillsEntry {
                name: "read-file".into(),
                target: "file:///tmp/a.txt".into(),
                trigger: "mentioned".into(),
            }]),
            entry_seq: 0,
        };

        // When it serializes to the wire JSON shape
        let wire = serde_json::to_value(&msg).expect("serialize");

        // Then with_skills appears as a sibling of origin/note (flattened
        // message fields), with each entry's fields as single words
        let obj = wire.as_object().expect("object");
        assert!(obj.contains_key("with_skills"), "key present: {obj:?}");
        assert_eq!(
            obj["with_skills"],
            serde_json::json!([{ "name": "read-file", "target": "file:///tmp/a.txt", "trigger": "mentioned" }])
        );
    }

    #[test]
    fn should_omit_with_skills_key_when_absent() {
        // Given a context message without a with_skills attachment
        let msg = ContextMessage::from_message(Message::user("plain"));

        // When it serializes to the wire JSON shape
        let wire = serde_json::to_value(&msg).expect("serialize");

        // Then the with_skills key is absent (skip_serializing_if)
        let obj = wire.as_object().expect("object");
        assert!(!obj.contains_key("with_skills"), "key absent: {obj:?}");
    }

    #[test]
    fn should_parse_legacy_journal_message_when_with_skills_missing() {
        // Given an old journal record written before ADR-006 (no with_skills)
        let raw = serde_json::json!({
            "role": "user",
            "content": [{"type": "text", "text": "hello"}]
        });

        // When it deserializes into a ContextMessage
        let msg: ContextMessage = serde_json::from_value(raw).expect("parse");

        // Then with_skills is None and the message round-trips unchanged
        assert_eq!(msg.with_skills, None);
        assert_eq!(msg.message.text_content(), "hello");
    }
}
