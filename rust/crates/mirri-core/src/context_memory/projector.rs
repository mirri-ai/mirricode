//! Projection pipeline — turns stored [`ContextMessage`] history into a
//! wire-valid `Vec<Message>` for the provider.
//!
//! The projection is a **pure, read-side transform**: it operates on a copy of
//! the history and never mutates the canonical store. Every repair it applies
//! to keep the outgoing wire valid (a displaced result moved back to its call,
//! a synthetic result invented for a lost one, an orphan/duplicate dropped,
//! leading non-user messages dropped, consecutive assistants merged, blank text
//! dropped) is reported through anomalies so observers can detect provider
//! rejections or history corruption.

use std::collections::HashSet;

use crate::context::{ContextMessage, ProjectOptions, ProjectionAnomaly};
use crate::error::{EngineError, EngineResult};
use crate::media_placeholder::{MediaKind, PlaceholderCause, build_media_placeholder};
use crate::message::{ContentPart, Message, Role};

// ---------------------------------------------------------------------------
// Tool result rendering constants
// ---------------------------------------------------------------------------

const TOOL_ERROR_STATUS: &str = "<system>ERROR: Tool execution failed.</system>";
const TOOL_EMPTY_STATUS: &str = "<system>Tool output is empty.</system>";
const TOOL_EMPTY_ERROR_STATUS: &str =
    "<system>ERROR: Tool execution failed. Tool output is empty.</system>";
const TOOL_OUTPUT_EMPTY_TEXT: &str = "Tool output is empty.";
const SYNTHETIC_TOOL_RESULT_TEXT: &str = "Tool result is not available in the current context. Do not assume the tool completed successfully.";

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Run the full projection pipeline.
///
/// Pipeline order (matching the TS `project` function):
/// 1. `merge_adjacent_user_messages` — also runs `prepare_message_for_projection`
/// 2. `dedupe_duplicate_tool_calls` (strict only)
/// 3. `repair_tool_exchange_adjacency` (always)
/// 4. `merge_consecutive_assistants` (strict only)
/// 5. `drop_orphan_tool_results` (every wire send)
/// 6. `drop_leading_non_user` (strict only)
/// 7. `strip_context_metadata` — produce plain `Message` array
///
/// Returns `Vec<Message>` (wire-valid).
pub fn project(history: &[ContextMessage], options: &ProjectOptions) -> EngineResult<Vec<Message>> {
    let anomalies: Vec<ProjectionAnomaly> = Vec::new();
    let (result, anomalies) = project_with_anomalies(history, options, anomalies)?;
    report_projection_repairs(&anomalies);
    Ok(result)
}

/// Run projection and return the anomalies alongside the result (for callers
/// that want to inspect repairs).
pub fn project_with_anomalies(
    history: &[ContextMessage],
    options: &ProjectOptions,
    mut anomalies: Vec<ProjectionAnomaly>,
) -> EngineResult<(Vec<Message>, Vec<ProjectionAnomaly>)> {
    // When strip_media is enabled with keep_recent > 0, pre-count total media
    // parts so we can spare the most recent ones (matching TS
    // `degradeOlderMediaParts`). When keep_recent == 0, set threshold to
    // usize::MAX so all media is stripped.
    let (_total_media, media_threshold) = if options.strip_media {
        if options.keep_recent_media > 0 {
            let total = count_media_parts(history);
            (total, total.saturating_sub(options.keep_recent_media))
        } else {
            (0, usize::MAX)
        }
    } else {
        (0, 0)
    };

    // Step 1: prepare + merge adjacent user messages + strip metadata to
    // wire Messages.
    let mut result = merge_adjacent_user_messages(
        history,
        &mut anomalies,
        options.strip_media,
        options.format_rejected,
        options.keep_recent_media,
        media_threshold,
    )?;

    // Step 2: dedupe duplicate tool calls (strict only).
    if options.dedupe_duplicate_tool_calls {
        result = dedupe_duplicate_tool_calls(result, &mut anomalies);
    }

    // Step 3: repair tool exchange adjacency (always).
    result = repair_tool_exchange_adjacency(result, options, &mut anomalies);

    // Step 4: merge consecutive assistants (strict only).
    if options.merge_consecutive_assistants {
        result = merge_consecutive_assistants(result, &mut anomalies);
    }

    // Step 5: drop orphan tool results (every wire send).
    if options.drop_orphan_results {
        result = drop_orphan_tool_results(result, &mut anomalies);
    }

    // Step 6: drop leading non-user (strict only).
    if options.drop_leading_non_user {
        result = drop_leading_non_user(result, &mut anomalies);
    }

    // Step 7: apply micro-compaction (replace old tool-result content with
    // placeholder when keep_recent is set).
    if let Some(keep_recent) = options.micro_compaction_keep_recent {
        result = apply_micro_compaction(
            result,
            keep_recent,
            options.micro_compaction_min_tokens,
            &options.micro_compaction_marker,
        );
    }

    // Step 8: strip_context_metadata is already done in merge_adjacent_user_messages.
    Ok((result, anomalies))
}

/// Log a warning for each unique anomaly type.
pub fn report_projection_repairs(anomalies: &[ProjectionAnomaly]) {
    let mut seen: HashSet<core::mem::Discriminant<ProjectionAnomaly>> = HashSet::new();
    for anomaly in anomalies {
        let key = core::mem::discriminant(anomaly);
        if seen.insert(key) {
            tracing::warn!("Projection repair: {:?}", anomaly);
        }
    }
}

// ---------------------------------------------------------------------------
// Media stripping (degraded projection)
// ---------------------------------------------------------------------------

/// Whether a content part is a media part (image, audio, video).
fn is_media_content_part(part: &ContentPart) -> bool {
    matches!(
        part,
        ContentPart::ImageUrl { .. } | ContentPart::AudioUrl { .. } | ContentPart::VideoUrl { .. }
    )
}

/// Count the total number of media parts across all non-partial messages.
fn count_media_parts(history: &[ContextMessage]) -> usize {
    history
        .iter()
        .filter(|m| !m.message.partial)
        .flat_map(|m| m.message.content.iter())
        .filter(|p| is_media_content_part(p))
        .count()
}

/// Replace a media content part with a text placeholder describing what was
/// removed. Non-media parts are returned unchanged.
///
/// `format_rejected` selects the placeholder cause: `true` announces that the
/// provider rejected the media's format and carries the conversion guidance
/// (US6.2 — the E6 format-rejected chain); `false` keeps the size-limit
/// copy (US6.1 — the E6 413 chain).
fn replace_media_with_text_placeholder(part: &ContentPart, format_rejected: bool) -> ContentPart {
    fn cause<'a>(
        format_rejected: bool,
        kind: MediaKind,
        path: Option<&'a str>,
    ) -> PlaceholderCause<'a> {
        if format_rejected {
            PlaceholderCause::FormatRejected { kind, path }
        } else {
            PlaceholderCause::SizeLimit { kind, path }
        }
    }
    match part {
        ContentPart::ImageUrl { image_url, .. } => ContentPart::Text {
            text: build_media_placeholder(cause(
                format_rejected,
                MediaKind::Image,
                image_url.id.as_deref(),
            )),
        },
        ContentPart::AudioUrl { .. } => ContentPart::Text {
            text: build_media_placeholder(cause(format_rejected, MediaKind::Audio, None)),
        },
        ContentPart::VideoUrl { .. } => ContentPart::Text {
            text: build_media_placeholder(cause(format_rejected, MediaKind::Video, None)),
        },
        _ => part.clone(),
    }
}

// ---------------------------------------------------------------------------
// Step 1: prepare + merge adjacent user messages
// ---------------------------------------------------------------------------

/// Prepare each message for projection, then merge consecutive user-origin
/// messages into one. Produces wire `Message` array (metadata stripped).
///
/// When `strip_media` is set and `keep_recent_media > 0`, only media parts
/// beyond the most recent `keep_recent_media` are replaced with placeholders;
/// the rest are preserved. When `keep_recent_media == 0`, all media parts are
/// replaced.
fn merge_adjacent_user_messages(
    history: &[ContextMessage],
    anomalies: &mut Vec<ProjectionAnomaly>,
    strip_media: bool,
    format_rejected: bool,
    _keep_recent_media: usize,
    media_threshold: usize,
) -> EngineResult<Vec<Message>> {
    // Track the media index as we walk through messages so we know which
    // media parts are "recent" (within the keep window) and which are "older".
    let mut media_counter = 0usize;

    let mut out: Vec<PreparedMessage> = Vec::new();

    for source in history {
        // Model-switch markers are UI-only dividers: they persist in the context
        // history (read faces replay them) but must never reach the provider.
        if matches!(
            source.origin,
            Some(crate::origin::PromptOrigin::ModelSwitched { .. })
        ) {
            continue;
        }

        let prepared = prepare_message_for_projection(
            source,
            anomalies,
            strip_media,
            format_rejected,
            &mut media_counter,
            media_threshold,
        )?;
        let Some(msg) = prepared else {
            continue;
        };

        if can_merge_user_message(&msg)
            && let Some(last) = out.last_mut()
            && can_merge_user_message(last)
        {
            *last = merge_two_user_messages(last, &msg);
            continue;
        }
        out.push(msg);
    }

    // Strip context metadata → wire Message.
    Ok(out
        .into_iter()
        .map(|msg| strip_context_metadata(&msg))
        .collect())
}

/// Intermediate representation: a ContextMessage that has been prepared for
/// projection but not yet stripped of metadata.
type PreparedMessage = ContextMessage;

/// Prepare a single message for projection.
///
/// - Drop if `partial == true`.
/// - If `strip_media` is set, replace media content parts that are beyond the
///   keep-recent window with text placeholders; media parts within the window
///   are preserved. `format_rejected` selects the placeholder cause (US6.2
///   format-rejected vs US6.1 size-limit copy).
/// - Render tool results (error prefix, empty placeholder, trailing note).
/// - Drop empty/whitespace-only text blocks.
/// - Throw if a tool message is empty after cleanup.
/// - Preserve messages that carry only tool_calls (no content).
/// - Drop messages that end up with no content and no tool calls.
///
/// `media_counter` is incremented for each media part encountered (including
/// those that are kept). `media_threshold` is the index at which media parts
/// start being kept (i.e. parts with index < threshold are replaced, parts
/// with index >= threshold are preserved).
fn prepare_message_for_projection(
    message: &ContextMessage,
    anomalies: &mut Vec<ProjectionAnomaly>,
    strip_media: bool,
    format_rejected: bool,
    media_counter: &mut usize,
    media_threshold: usize,
) -> EngineResult<Option<PreparedMessage>> {
    if message.message.partial {
        return Ok(None);
    }

    // Strip media parts → replace with text placeholders for older media.
    let source: ContextMessage = if strip_media {
        let has_media = message.message.content.iter().any(is_media_content_part);
        if has_media {
            let mut s = message.clone();
            s.message.content = s
                .message
                .content
                .into_iter()
                .map(|part| {
                    if is_media_content_part(&part) {
                        let idx = *media_counter;
                        *media_counter += 1;
                        if idx < media_threshold {
                            replace_media_with_text_placeholder(&part, format_rejected)
                        } else {
                            part
                        }
                    } else {
                        part
                    }
                })
                .collect();
            s
        } else {
            message.clone()
        }
    } else {
        message.clone()
    };

    // Tool results: render the model-visible form.
    let source: ContextMessage = if source.message.role == Role::Tool {
        let rendered = render_tool_result_for_model(
            &source.message.content,
            source.note.as_deref(),
            source.is_error,
        );
        let mut s = source.clone();
        s.message.content = rendered;
        s
    } else {
        source
    };

    // Drop empty/whitespace-only text blocks.
    let mut filtered_content: Vec<ContentPart> = Vec::new();
    let mut has_blank = false;
    for part in &source.message.content {
        if let ContentPart::Text { text } = part
            && text.trim().is_empty()
        {
            has_blank = true;
            continue;
        }
        filtered_content.push(part.clone());
    }

    let mut next: ContextMessage = if has_blank {
        // Report whitespace-only (non-empty) blocks that were dropped.
        for part in &source.message.content {
            if let ContentPart::Text { text } = part
                && text.trim().is_empty()
                && !text.is_empty()
            {
                anomalies.push(ProjectionAnomaly::WhitespaceTextDropped);
                break;
            }
        }
        let mut s = source.clone();
        s.message.content = filtered_content;
        s
    } else {
        source
    };

    // Tool message must not be empty after cleanup.
    if next.message.role == Role::Tool && next.message.content.is_empty() {
        return Err(EngineError::RequestInvalid(
            "Tool result message content cannot be empty after removing empty text blocks."
                .to_string(),
        ));
    }

    // Sanitize assistant tool-call arguments (always, every wire send): a
    // persisted arguments string that is not a JSON object (e.g. the literal
    // "null" left behind by a corrupted provider stream) is rejected by
    // OpenAI-compatible servers on every replay — sanitize to "{}" so the
    // turn can proceed instead of hard-failing.
    if next.message.role == Role::Assistant && !next.message.tool_calls.is_empty() {
        let mut sanitized = false;
        for tool_call in &mut next.message.tool_calls {
            let valid = serde_json::from_str::<serde_json::Value>(&tool_call.function.arguments)
                .ok()
                .is_some_and(|v| v.is_object());
            if !valid {
                tool_call.function.arguments = "{}".to_string();
                sanitized = true;
            }
        }
        if sanitized {
            anomalies.push(ProjectionAnomaly::InvalidToolArgumentsSanitized);
        }
    }

    // Drop messages with no content and no tool calls.
    if next.message.content.is_empty() && next.message.tool_calls.is_empty() {
        return Ok(None);
    }

    Ok(Some(next))
}

/// Whether a message is a user-origin message that can be merged with adjacent
/// user messages.
fn can_merge_user_message(msg: &ContextMessage) -> bool {
    msg.message.role == Role::User
        && msg
            .origin
            .as_ref()
            .map(|o| *o == crate::origin::PromptOrigin::User)
            .unwrap_or(false)
}

/// Merge two user messages: text parts joined with `\n\n`, non-text parts
/// appended.
fn merge_two_user_messages(a: &ContextMessage, b: &ContextMessage) -> ContextMessage {
    let a_text = extract_text_only(a);
    let b_text = extract_text_only(b);
    let mut non_text_parts: Vec<ContentPart> = Vec::new();
    for part in &a.message.content {
        if !matches!(part, ContentPart::Text { .. }) {
            non_text_parts.push(part.clone());
        }
    }
    for part in &b.message.content {
        if !matches!(part, ContentPart::Text { .. }) {
            non_text_parts.push(part.clone());
        }
    }

    let merged_text = format!("{a_text}\n\n{b_text}");
    let mut content = vec![ContentPart::Text { text: merged_text }];
    content.extend(non_text_parts);

    let mut merged = a.clone();
    merged.message.content = content;
    merged.message.tool_calls.clear();
    merged
}

fn extract_text_only(msg: &ContextMessage) -> String {
    msg.message
        .content
        .iter()
        .filter_map(|p| {
            if let ContentPart::Text { text } = p {
                Some(text.as_str())
            } else {
                None
            }
        })
        .collect::<Vec<_>>()
        .join("")
}

// ---------------------------------------------------------------------------
// Tool result rendering
// ---------------------------------------------------------------------------

/// Render a stored tool result into model-visible content parts.
///
/// - Error → prepend `<system>ERROR: Tool execution failed.</system>`.
/// - Empty output → `<system>Tool output is empty.</system>`.
/// - Append `note` if present (joined into the single text part for text-only
///   results, or as a trailing text part for media-bearing results).
fn render_tool_result_for_model(
    content: &[ContentPart],
    note: Option<&str>,
    is_error: bool,
) -> Vec<ContentPart> {
    let rendered = render_tool_status(content, is_error);

    let note = note.filter(|n| !n.is_empty());
    let Some(note) = note else {
        return rendered;
    };

    // If there's exactly one text part, join the note into it.
    if rendered.len() == 1
        && let ContentPart::Text { text } = &rendered[0]
    {
        return vec![ContentPart::Text {
            text: format!("{text}\n{note}"),
        }];
    }

    let mut result = rendered;
    result.push(ContentPart::Text {
        text: note.to_string(),
    });
    result
}

fn render_tool_status(content: &[ContentPart], is_error: bool) -> Vec<ContentPart> {
    // If the content is a single text part, treat it as a string output.
    if content.len() == 1
        && let ContentPart::Text { text } = &content[0]
    {
        let single = text.as_str();
        if is_error {
            if single.is_empty() {
                return vec![text_part(TOOL_EMPTY_ERROR_STATUS)];
            }
            return vec![text_part(&format!("{TOOL_ERROR_STATUS}\n{single}"))];
        }
        if is_empty_output_text(single) {
            return vec![text_part(TOOL_EMPTY_STATUS)];
        }
        return vec![text_part(single)];
    }

    // Multi-part or non-text content.
    if is_empty_equivalent_content_array(content) {
        let status = if is_error {
            TOOL_EMPTY_ERROR_STATUS
        } else {
            TOOL_EMPTY_STATUS
        };
        return vec![text_part(status)];
    }

    if is_error {
        let mut result = vec![text_part(TOOL_ERROR_STATUS)];
        result.extend(content.iter().cloned());
        return result;
    }

    content.to_vec()
}

fn is_empty_output_text(output: &str) -> bool {
    output.trim().is_empty() || output.trim() == TOOL_OUTPUT_EMPTY_TEXT
}

fn is_empty_equivalent_content_array(parts: &[ContentPart]) -> bool {
    parts.iter().all(|p| match p {
        ContentPart::Text { text } => text.trim().is_empty(),
        _ => false,
    })
}

fn text_part(text: &str) -> ContentPart {
    ContentPart::Text {
        text: text.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Step 2: dedupe duplicate tool calls (strict only)
// ---------------------------------------------------------------------------

fn dedupe_duplicate_tool_calls(
    messages: Vec<Message>,
    anomalies: &mut Vec<ProjectionAnomaly>,
) -> Vec<Message> {
    let mut seen_tool_call_ids: HashSet<String> = HashSet::new();
    let mut seen_tool_result_ids: HashSet<String> = HashSet::new();
    let mut out: Vec<Message> = Vec::new();

    for message in messages {
        if message.role == Role::Assistant && !message.tool_calls.is_empty() {
            let kept: Vec<crate::message::ToolCall> = message
                .tool_calls
                .iter()
                .filter(|tc| {
                    if seen_tool_call_ids.contains(&tc.id) {
                        anomalies.push(ProjectionAnomaly::DuplicateToolCallDropped);
                        false
                    } else {
                        seen_tool_call_ids.insert(tc.id.clone());
                        true
                    }
                })
                .cloned()
                .collect();

            if kept.len() == message.tool_calls.len() {
                out.push(message);
            } else if !kept.is_empty() || !message.content.is_empty() {
                let mut m = message;
                m.tool_calls = kept;
                out.push(m);
            }
            continue;
        }

        if message.role == Role::Tool
            && let Some(ref tool_call_id) = message.tool_call_id
        {
            if seen_tool_result_ids.contains(tool_call_id) {
                anomalies.push(ProjectionAnomaly::DuplicateToolResultDropped);
                continue;
            }
            seen_tool_result_ids.insert(tool_call_id.clone());
        }
        out.push(message);
    }
    out
}

// ---------------------------------------------------------------------------
// Step 3: repair tool exchange adjacency (always)
// ---------------------------------------------------------------------------

fn repair_tool_exchange_adjacency(
    messages: Vec<Message>,
    options: &ProjectOptions,
    anomalies: &mut Vec<ProjectionAnomaly>,
) -> Vec<Message> {
    // Find the last non-tool message index. The trailing exchange is the only
    // one whose missing result may still be in-flight: any assistant tool_use
    // that precedes a later user/assistant message has been overtaken by a new
    // turn and cannot be pending.
    let mut last_non_tool_index: i64 = -1;
    for i in (0..messages.len()).rev() {
        if messages[i].role != Role::Tool {
            last_non_tool_index = i as i64;
            break;
        }
    }

    let mut out: Vec<Message> = Vec::new();
    let mut consumed: HashSet<usize> = HashSet::new();

    for i in 0..messages.len() {
        if consumed.contains(&i) {
            continue;
        }
        let message = &messages[i];
        if message.role != Role::Assistant || message.tool_calls.is_empty() {
            out.push(message.clone());
            continue;
        }

        out.push(message.clone());
        let mut pending: HashSet<String> =
            message.tool_calls.iter().map(|tc| tc.id.clone()).collect();
        let mut foreign_between = false;

        #[allow(clippy::needless_range_loop)]
        for j in (i + 1)..messages.len() {
            if pending.is_empty() {
                break;
            }
            if consumed.contains(&j) {
                continue;
            }
            let next = &messages[j];
            if next.role == Role::Tool
                && let Some(ref tool_call_id) = next.tool_call_id
                && pending.contains(tool_call_id)
            {
                out.push(next.clone());
                consumed.insert(j);
                pending.remove(tool_call_id);
                if foreign_between {
                    anomalies.push(ProjectionAnomaly::ToolResultReordered);
                }
                continue;
            }
            foreign_between = true;
        }

        // Close any tool call whose result is absent. A mid-history orphan (a
        // later user/assistant message follows) is always closed — it cannot
        // be in-flight. The trailing exchange is closed only when
        // `synthesize_missing` is set.
        let is_mid_history = (i as i64) < last_non_tool_index;
        if options.synthesize_missing || is_mid_history {
            // Sort for deterministic output.
            let mut missing: Vec<String> = pending.into_iter().collect();
            missing.sort();
            for missing_id in missing {
                out.push(make_synthetic_tool_result(&missing_id));
                anomalies.push(ProjectionAnomaly::ToolResultSynthesized);
            }
        }
    }
    out
}

fn make_synthetic_tool_result(tool_call_id: &str) -> Message {
    Message {
        role: Role::Tool,
        name: None,
        content: vec![ContentPart::Text {
            text: SYNTHETIC_TOOL_RESULT_TEXT.to_string(),
        }],
        tool_calls: Vec::new(),
        tool_call_id: Some(tool_call_id.to_string()),
        partial: false,
    }
}

// ---------------------------------------------------------------------------
// Step 4: merge consecutive assistants (strict only)
// ---------------------------------------------------------------------------

fn merge_consecutive_assistants(
    messages: Vec<Message>,
    anomalies: &mut Vec<ProjectionAnomaly>,
) -> Vec<Message> {
    let mut out: Vec<Message> = Vec::new();
    for message in messages {
        if let Some(last) = out.last_mut()
            && last.role == Role::Assistant
            && message.role == Role::Assistant
        {
            last.content.extend(message.content);
            last.tool_calls.extend(message.tool_calls);
            anomalies.push(ProjectionAnomaly::ConsecutiveAssistantsMerged);
            continue;
        }
        out.push(message);
    }
    out
}

// ---------------------------------------------------------------------------
// Step 5: drop orphan tool results (every wire send)
// ---------------------------------------------------------------------------

fn drop_orphan_tool_results(
    messages: Vec<Message>,
    anomalies: &mut Vec<ProjectionAnomaly>,
) -> Vec<Message> {
    let mut tool_use_ids: HashSet<String> = HashSet::new();
    for message in &messages {
        if message.role == Role::Assistant {
            for tc in &message.tool_calls {
                tool_use_ids.insert(tc.id.clone());
            }
        }
    }

    messages
        .into_iter()
        .filter(|message| {
            if message.role != Role::Tool {
                return true;
            }
            if let Some(ref tool_call_id) = message.tool_call_id {
                if tool_use_ids.contains(tool_call_id) {
                    return true;
                }
                anomalies.push(ProjectionAnomaly::OrphanToolResultDropped);
                return false;
            }
            true
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Step 6: drop leading non-user (strict only)
// ---------------------------------------------------------------------------

fn drop_leading_non_user(
    messages: Vec<Message>,
    anomalies: &mut Vec<ProjectionAnomaly>,
) -> Vec<Message> {
    let mut start = 0;
    while start < messages.len() && messages[start].role != Role::User {
        anomalies.push(ProjectionAnomaly::LeadingNonUserDropped);
        start += 1;
    }
    if start == 0 {
        messages
    } else {
        messages[start..].to_vec()
    }
}

// ---------------------------------------------------------------------------
// Step 7: strip context metadata
// ---------------------------------------------------------------------------

fn strip_context_metadata(msg: &ContextMessage) -> Message {
    msg.message.clone()
}

// ---------------------------------------------------------------------------
// Micro-compaction
// ---------------------------------------------------------------------------

/// Apply micro-compaction: replace content of old tool-result messages
/// (before the cutoff index) that exceed the minimum token threshold
/// with a placeholder marker.
///
/// This is a lossy, in-process operation — no LLM call is made. It only
/// affects `Tool` messages before the cutoff; user, assistant, and recent
/// messages are untouched.
///
/// The cutoff is computed as `messages.len().saturating_sub(keep_recent)`,
/// so it is always relative to the actual (post-repair) message array.
fn apply_micro_compaction(
    messages: Vec<Message>,
    keep_recent: usize,
    min_tokens: usize,
    marker: &str,
) -> Vec<Message> {
    if marker.is_empty() {
        return messages;
    }

    let cutoff = messages.len().saturating_sub(keep_recent);
    if cutoff == 0 {
        return messages;
    }

    messages
        .into_iter()
        .enumerate()
        .map(|(i, mut msg)| {
            if i < cutoff && msg.role == Role::Tool && msg.tool_call_id.is_some() {
                // Rough token estimate: 4 chars ≈ 1 token.
                let content_len: usize = msg
                    .content
                    .iter()
                    .filter_map(|p| match p {
                        ContentPart::Text { text } => Some(text.len()),
                        _ => None,
                    })
                    .sum();
                if content_len >= min_tokens * 4 {
                    msg.content = vec![ContentPart::Text {
                        text: marker.to_string(),
                    }];
                }
            }
            msg
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{ImageUrlData, ToolCall, ToolCallFunction};
    use crate::origin::PromptOrigin;

    // -- helpers ----------------------------------------------------------

    /// A user-role message tagged with the `PromptOrigin::User` origin (the
    /// combination that makes adjacent user messages mergeable).
    fn user_context(text: &str) -> ContextMessage {
        let mut message = ContextMessage::from_message(Message::user(text));
        message.origin = Some(PromptOrigin::User);
        message
    }

    /// A tool-result message for the given tool call id.
    fn tool_context(call_id: &str, output: &str) -> ContextMessage {
        ContextMessage::from_message(Message::tool_result(call_id, output))
    }

    /// An assistant message carrying one tool call.
    fn assistant_with_tool_call(text: &str, call_id: &str, tool_name: &str) -> Message {
        let mut message = Message::assistant(text);
        message.tool_calls = vec![ToolCall {
            id: call_id.to_string(),
            call_type: "function".to_string(),
            function: ToolCallFunction {
                name: tool_name.to_string(),
                arguments: "{}".to_string(),
            },
        }];
        message
    }

    /// An assistant message carrying one tool call with raw (possibly
    /// malformed) arguments text, plus its tool result — the minimal
    /// context shape a persisted bad call lives in.
    fn history_with_raw_tool_arguments(raw_args: &str) -> Vec<ContextMessage> {
        let mut message = Message::assistant("editing");
        message.tool_calls = vec![ToolCall {
            id: "call_bad".to_string(),
            call_type: "function".to_string(),
            function: ToolCallFunction {
                name: "Edit".to_string(),
                arguments: raw_args.to_string(),
            },
        }];
        vec![
            user_context("edit a file"),
            ContextMessage::from_message(message),
            tool_context(
                "call_bad",
                "Invalid args for tool Edit: null is not of type object",
            ),
        ]
    }

    /// Whether the anomaly list contains an anomaly of the given variant
    /// (discriminant comparison; `ProjectionAnomaly` has no `PartialEq`).
    fn has_anomaly(anomalies: &[ProjectionAnomaly], target: ProjectionAnomaly) -> bool {
        anomalies
            .iter()
            .any(|a| std::mem::discriminant(a) == std::mem::discriminant(&target))
    }

    // -- tests ------------------------------------------------------------

    #[test]
    fn should_return_empty_vec_when_projecting_empty_history() {
        // Given an empty context history and normal projection options
        let history: Vec<ContextMessage> = Vec::new();

        // When projecting the history
        let result = project(&history, &ProjectOptions::normal()).unwrap();

        // Then the projection yields an empty message list
        assert!(result.is_empty());
    }

    /// Given a history containing an assistant tool call whose persisted
    /// arguments are not a JSON object (e.g. the literal string "null" left
    /// behind by a corrupted stream), When the history is projected with
    /// normal options, Then the arguments must be sanitized to `{}` and an
    /// anomaly must be reported — otherwise OpenAI-compatible servers
    /// reject every replayed turn with
    /// "function.arguments must be a JSON object".
    #[test]
    fn should_sanitize_non_object_tool_arguments_when_projecting_history() {
        for raw_args in ["null", "\"plain string\"", "{truncated", "[1,2]"] {
            // Given a history whose assistant tool call carries non-object
            // arguments text
            let history = history_with_raw_tool_arguments(raw_args);

            // When projecting with normal options
            let (result, anomalies) =
                project_with_anomalies(&history, &ProjectOptions::normal(), Vec::new()).unwrap();

            // Then the assistant tool call survives with `{}` arguments
            let assistant = result
                .iter()
                .find(|m| m.role == Role::Assistant && !m.tool_calls.is_empty())
                .unwrap_or_else(|| panic!("assistant tool call survived for raw args {raw_args}"));
            assert_eq!(
                assistant.tool_calls[0].function.arguments, "{}",
                "non-object arguments must be sanitized to {{}} for raw args {raw_args}"
            );
            assert_eq!(assistant.tool_calls[0].function.name, "Edit");
            assert_eq!(assistant.tool_calls[0].id, "call_bad");
            assert!(
                has_anomaly(&anomalies, ProjectionAnomaly::InvalidToolArgumentsSanitized),
                "an anomaly must be reported for raw args {raw_args}"
            );
        }
    }

    /// Given a history whose assistant tool calls all carry well-formed JSON
    /// object arguments, When the history is projected, Then the arguments
    /// pass through unchanged and no invalid-arguments anomaly is reported.
    #[test]
    fn should_preserve_valid_tool_arguments_when_projecting_history() {
        // Given a history with valid object arguments
        let history = history_with_raw_tool_arguments(r#"{"path":"/tmp/a"}"#);

        // When projecting with normal options
        let (result, anomalies) =
            project_with_anomalies(&history, &ProjectOptions::normal(), Vec::new()).unwrap();

        // Then the arguments are preserved verbatim
        let assistant = result
            .iter()
            .find(|m| m.role == Role::Assistant && !m.tool_calls.is_empty())
            .expect("assistant tool call survived");
        assert_eq!(
            assistant.tool_calls[0].function.arguments,
            r#"{"path":"/tmp/a"}"#
        );
        assert!(
            !has_anomaly(&anomalies, ProjectionAnomaly::InvalidToolArgumentsSanitized),
            "no anomaly for valid arguments"
        );
    }

    #[test]
    fn should_preserve_messages_when_projecting_clean_history() {
        // Given a clean user → assistant → user history with no repairs needed
        let history = vec![
            user_context("hello"),
            ContextMessage::from_message(Message::assistant("hi")),
            user_context("bye"),
        ];

        // When projecting with normal options
        let result = project(&history, &ProjectOptions::normal()).unwrap();

        // Then all three messages survive in order with their roles and text
        assert_eq!(result.len(), 3);
        assert_eq!(result[0].role, Role::User);
        assert_eq!(result[0].text_content(), "hello");
        assert_eq!(result[1].role, Role::Assistant);
        assert_eq!(result[1].text_content(), "hi");
        assert_eq!(result[2].role, Role::User);
        assert_eq!(result[2].text_content(), "bye");
    }

    #[test]
    fn should_merge_adjacent_user_messages_when_projecting_with_normal_options() {
        // Given two consecutive user-origin messages
        let history = vec![user_context("first"), user_context("second")];

        // When projecting with normal options
        let result = project(&history, &ProjectOptions::normal()).unwrap();

        // Then they collapse into a single user message joined with "\n\n"
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].role, Role::User);
        assert_eq!(result[0].text_content(), "first\n\nsecond");
    }

    #[test]
    fn should_not_merge_user_messages_when_origin_is_not_user() {
        // Given two consecutive user-role messages whose origin is None
        // (not tagged with PromptOrigin::User)
        let history = vec![
            ContextMessage::from_message(Message::user("first")),
            ContextMessage::from_message(Message::user("second")),
        ];

        // When projecting with normal options
        let result = project(&history, &ProjectOptions::normal()).unwrap();

        // Then they stay as two separate messages
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].text_content(), "first");
        assert_eq!(result[1].text_content(), "second");
    }

    #[test]
    fn should_drop_model_switch_markers_when_projecting_history() {
        // Given a history containing a user message, a model-switch marker,
        // and an assistant message
        let mut marker = ContextMessage::from_message(Message::user("Model switched from a to b"));
        marker.origin = Some(PromptOrigin::ModelSwitched {
            from: "a".into(),
            to: "b".into(),
        });
        let history = vec![
            user_context("hello"),
            marker,
            ContextMessage::from_message(Message::assistant("hi there")),
        ];
        // When projected
        let out = project(&history, &ProjectOptions::default()).unwrap();
        // Then the marker never reaches the provider
        assert_eq!(out.len(), 2);
        assert!(!out.iter().any(|m| m.content.iter().any(|p| matches!(
            p,
            ContentPart::Text { text } if text.contains("Model switched")
        ))));
    }

    #[test]
    fn should_drop_partial_messages_when_projecting() {
        // Given a message flagged as partial (interrupted mid-stream)
        let mut message = Message::user("partial content");
        message.partial = true;
        let history = vec![ContextMessage::from_message(message)];

        // When projecting with normal options
        let result = project(&history, &ProjectOptions::normal()).unwrap();

        // Then the partial message is dropped entirely
        assert!(result.is_empty());
    }

    #[test]
    fn should_drop_whitespace_only_text_parts_when_projecting() {
        // Given a user message mixing a real text part with a whitespace-only one
        let mut message = Message::user("keep me");
        message.content.push(ContentPart::Text {
            text: "   ".to_string(),
        });
        let history = vec![ContextMessage::from_message(message)];

        // When projecting with normal options
        let (result, anomalies) =
            project_with_anomalies(&history, &ProjectOptions::normal(), Vec::new()).unwrap();

        // Then the whitespace-only part is dropped and the anomaly is reported
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].text_content(), "keep me");
        assert!(has_anomaly(
            &anomalies,
            ProjectionAnomaly::WhitespaceTextDropped
        ));
    }

    #[test]
    fn should_synthesize_missing_tool_result_when_compaction_option_set() {
        // Given an assistant tool call whose result never arrived, projected
        // with compaction options (synthesize_missing = true)
        let history = vec![ContextMessage::from_message(assistant_with_tool_call(
            "checking", "call-1", "Read",
        ))];

        // When projecting with compaction options
        let (result, anomalies) =
            project_with_anomalies(&history, &ProjectOptions::compaction(), Vec::new()).unwrap();

        // Then a synthetic tool result is inserted for the missing call
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].role, Role::Assistant);
        assert_eq!(result[1].role, Role::Tool);
        assert_eq!(result[1].tool_call_id.as_deref(), Some("call-1"));
        assert_eq!(result[1].text_content(), SYNTHETIC_TOOL_RESULT_TEXT);
        assert!(has_anomaly(
            &anomalies,
            ProjectionAnomaly::ToolResultSynthesized
        ));
    }

    #[test]
    fn should_not_synthesize_missing_tool_result_when_option_not_set() {
        // Given the same open assistant tool call, projected with normal
        // options (synthesize_missing = false)
        let history = vec![ContextMessage::from_message(assistant_with_tool_call(
            "checking", "call-1", "Read",
        ))];

        // When projecting with normal options
        let result = project(&history, &ProjectOptions::normal()).unwrap();

        // Then no synthetic result is invented
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].role, Role::Assistant);
        assert!(result.iter().all(|m| m.role != Role::Tool));
    }

    #[test]
    fn should_drop_orphan_tool_results_when_projecting() {
        // Given a tool result with no matching tool call anywhere in history
        let history = vec![tool_context("call-orphan", "stray output")];

        // When projecting with normal options
        let (result, anomalies) =
            project_with_anomalies(&history, &ProjectOptions::normal(), Vec::new()).unwrap();

        // Then the orphan result is dropped and the anomaly is reported
        assert!(result.is_empty());
        assert!(has_anomaly(
            &anomalies,
            ProjectionAnomaly::OrphanToolResultDropped
        ));
    }

    #[test]
    fn should_drop_duplicate_tool_calls_when_strict_option_set() {
        // Given two assistant messages carrying the same tool call id
        // (separated by the first call's result so they are not consecutive)
        let history = vec![
            user_context("hi"),
            ContextMessage::from_message(assistant_with_tool_call("first", "call-1", "Read")),
            tool_context("call-1", "result one"),
            ContextMessage::from_message(assistant_with_tool_call("again", "call-1", "Read")),
        ];

        // When projecting with strict options
        let (result, anomalies) =
            project_with_anomalies(&history, &ProjectOptions::strict(), Vec::new()).unwrap();

        // Then the duplicate call is dropped from the second assistant while
        // the message itself survives
        assert_eq!(result.len(), 4);
        assert_eq!(result[0].role, Role::User);
        assert_eq!(result[1].tool_calls.len(), 1);
        assert_eq!(result[2].role, Role::Tool);
        assert_eq!(result[2].text_content(), "result one");
        assert_eq!(result[3].role, Role::Assistant);
        assert_eq!(result[3].text_content(), "again");
        assert!(
            result[3].tool_calls.is_empty(),
            "the duplicate tool call must be dropped"
        );
        assert!(has_anomaly(
            &anomalies,
            ProjectionAnomaly::DuplicateToolCallDropped
        ));
    }

    #[test]
    fn should_drop_leading_non_user_messages_when_strict_option_set() {
        // Given a history whose first user message is preceded by a system
        // and an assistant message
        let history = vec![
            ContextMessage::from_message(Message::system("boot")),
            ContextMessage::from_message(Message::assistant("prelude")),
            user_context("hello"),
        ];

        // When projecting with strict options
        let (result, anomalies) =
            project_with_anomalies(&history, &ProjectOptions::strict(), Vec::new()).unwrap();

        // Then everything before the first user message is dropped
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].role, Role::User);
        assert_eq!(result[0].text_content(), "hello");
        assert!(has_anomaly(
            &anomalies,
            ProjectionAnomaly::LeadingNonUserDropped
        ));
    }

    #[test]
    fn should_merge_consecutive_assistants_when_strict_option_set() {
        // Given two consecutive assistant messages after a user message
        let history = vec![
            user_context("question"),
            ContextMessage::from_message(Message::assistant("answer one")),
            ContextMessage::from_message(Message::assistant("answer two")),
        ];

        // When projecting with strict options
        let (result, anomalies) =
            project_with_anomalies(&history, &ProjectOptions::strict(), Vec::new()).unwrap();

        // Then the two assistant messages merge into one with content preserved
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].text_content(), "question");
        assert_eq!(result[1].role, Role::Assistant);
        assert_eq!(result[1].content.len(), 2);
        assert_eq!(result[1].text_content(), "answer one\nanswer two");
        assert!(has_anomaly(
            &anomalies,
            ProjectionAnomaly::ConsecutiveAssistantsMerged
        ));
    }

    #[test]
    fn should_strip_older_media_when_degraded_option_set() {
        // Given three user messages each carrying an image, projected with
        // degraded options (keep_recent = 2)
        let make_msg = |text: &str| {
            let mut message = Message::user(text);
            message.content.push(ContentPart::ImageUrl {
                image_url: ImageUrlData {
                    url: format!("https://example.com/{text}.png"),
                    detail: None,
                    id: None,
                },
                detail: None,
            });
            ContextMessage::from_message(message)
        };
        let history = vec![make_msg("first"), make_msg("second"), make_msg("third")];

        // When projecting with degraded options (keep_recent_media = 2)
        let result = project(&history, &ProjectOptions::degraded()).unwrap();

        // Then the oldest image is replaced with a placeholder but the two
        // most recent images are preserved
        assert_eq!(result.len(), 3);
        assert!(
            result[0]
                .content
                .iter()
                .any(|p| matches!(p, ContentPart::Text { text } if text.contains("omitted"))),
            "oldest media should be replaced with placeholder"
        );
        assert!(
            result[1]
                .content
                .iter()
                .any(|p| matches!(p, ContentPart::ImageUrl { .. })),
            "second image should be preserved"
        );
        assert!(
            result[2]
                .content
                .iter()
                .any(|p| matches!(p, ContentPart::ImageUrl { .. })),
            "third image should be preserved"
        );
    }

    #[test]
    fn should_strip_all_media_when_stripped_option_set() {
        // Given two user messages each carrying an image
        let make_msg = |text: &str| {
            let mut message = Message::user(text);
            message.content.push(ContentPart::ImageUrl {
                image_url: ImageUrlData {
                    url: format!("https://example.com/{text}.png"),
                    detail: None,
                    id: None,
                },
                detail: None,
            });
            ContextMessage::from_message(message)
        };
        let history = vec![make_msg("first"), make_msg("second")];

        // When projecting with stripped options (keep_recent_media = 0)
        let result = project(&history, &ProjectOptions::stripped()).unwrap();

        // Then all media parts are replaced with placeholders
        assert_eq!(result.len(), 2);
        assert!(
            result[0]
                .content
                .iter()
                .all(|p| !matches!(p, ContentPart::ImageUrl { .. })),
            "no media may remain after stripped projection"
        );
        assert!(
            result[1]
                .content
                .iter()
                .all(|p| !matches!(p, ContentPart::ImageUrl { .. })),
            "no media may remain after stripped projection"
        );
    }

    #[test]
    fn should_report_anomalies_when_repairs_applied() {
        // Given a history that triggers multiple repairs: an early assistant
        // with a lost call (synthesized, then dropped as leading non-user),
        // an orphan tool result, and a proper later exchange
        let history = vec![
            ContextMessage::from_message(assistant_with_tool_call("early", "call-99", "Read")),
            tool_context("call-orphan", "stray"),
            user_context("hello"),
            ContextMessage::from_message(assistant_with_tool_call("next", "call-1", "Bash")),
            tool_context("call-1", "result"),
        ];

        // When projecting with strict options and collecting anomalies
        let (result, anomalies) =
            project_with_anomalies(&history, &ProjectOptions::strict(), Vec::new()).unwrap();

        // Then every applied repair is reported in the anomaly list
        assert_eq!(result.len(), 3);
        assert_eq!(result[0].text_content(), "hello");
        assert_eq!(result[1].text_content(), "next");
        assert_eq!(result[2].text_content(), "result");
        assert!(has_anomaly(
            &anomalies,
            ProjectionAnomaly::ToolResultSynthesized
        ));
        assert!(has_anomaly(
            &anomalies,
            ProjectionAnomaly::OrphanToolResultDropped
        ));
        assert!(has_anomaly(
            &anomalies,
            ProjectionAnomaly::LeadingNonUserDropped
        ));
    }

    #[test]
    fn should_prepend_error_prefix_when_tool_result_is_error() {
        // Given a tool result flagged as an error, preceded by its tool call
        let mut error_result = tool_context("call-1", "command failed");
        error_result.is_error = true;
        let history = vec![
            ContextMessage::from_message(assistant_with_tool_call("running", "call-1", "Bash")),
            error_result,
        ];

        // When projecting with normal options
        let result = project(&history, &ProjectOptions::normal()).unwrap();

        // Then the rendered output starts with the error status prefix
        assert_eq!(result.len(), 2);
        assert!(
            result[1].text_content().starts_with(TOOL_ERROR_STATUS),
            "error tool result must carry the error prefix, got: {}",
            result[1].text_content()
        );
    }

    #[test]
    fn should_show_empty_placeholder_when_tool_result_is_empty() {
        // Given a tool result whose stored output is empty
        let history = vec![
            ContextMessage::from_message(assistant_with_tool_call("running", "call-1", "Bash")),
            tool_context("call-1", ""),
        ];

        // When projecting with normal options
        let result = project(&history, &ProjectOptions::normal()).unwrap();

        // Then the rendered output is the empty-output placeholder
        assert_eq!(result.len(), 2);
        assert_eq!(result[1].text_content(), TOOL_EMPTY_STATUS);
    }

    #[test]
    fn should_append_note_when_tool_result_has_note() {
        // Given a tool result carrying a model-visible note
        let mut noted_result = tool_context("call-1", "results");
        noted_result.note = Some("extra info".to_string());
        let history = vec![
            ContextMessage::from_message(assistant_with_tool_call("running", "call-1", "Bash")),
            noted_result,
        ];

        // When projecting with normal options
        let result = project(&history, &ProjectOptions::normal()).unwrap();

        // Then the note is appended to the rendered output
        assert_eq!(result.len(), 2);
        assert_eq!(result[1].text_content(), "results\nextra info");
    }

    #[test]
    fn should_reorder_tool_result_when_displaced_from_call() {
        // Given a tool result that appears after a later user message instead
        // of immediately after its tool call
        let history = vec![
            ContextMessage::from_message(assistant_with_tool_call("a-text", "call-1", "Read")),
            user_context("interruption"),
            tool_context("call-1", "late result"),
        ];

        // When projecting with normal options
        let (result, anomalies) =
            project_with_anomalies(&history, &ProjectOptions::normal(), Vec::new()).unwrap();

        // Then the result is moved back to follow its tool call
        assert_eq!(result.len(), 3);
        assert_eq!(result[0].role, Role::Assistant);
        assert_eq!(result[0].tool_calls.len(), 1);
        assert_eq!(result[1].role, Role::Tool);
        assert_eq!(result[1].tool_call_id.as_deref(), Some("call-1"));
        assert_eq!(result[1].text_content(), "late result");
        assert_eq!(result[2].role, Role::User);
        assert_eq!(result[2].text_content(), "interruption");
        assert!(has_anomaly(
            &anomalies,
            ProjectionAnomaly::ToolResultReordered
        ));
    }

    // -- micro-compaction integration tests -------------------------------

    /// Helper: build a tool result message with long text content.
    fn long_tool_result(call_id: &str, text: &str) -> Message {
        Message::tool_result(call_id, text)
    }

    /// Helper: build ProjectOptions with micro-compaction enabled.
    fn options_with_micro_compaction(keep_recent: usize, min_tokens: usize) -> ProjectOptions {
        ProjectOptions {
            micro_compaction_keep_recent: Some(keep_recent),
            micro_compaction_min_tokens: min_tokens,
            micro_compaction_marker: "[cleared]".to_string(),
            ..Default::default()
        }
    }

    /// should replace old tool results above threshold with marker
    #[test]
    fn should_replace_old_tool_results_when_micro_compaction_enabled() {
        // Given 6 messages: user, assistant, tool(long), user, assistant, tool(long)
        let long = "x".repeat(500);
        let history = vec![
            ContextMessage::from_message(Message::user("q1")),
            ContextMessage::from_message(Message::assistant("a1")),
            ContextMessage::from_message(long_tool_result("c1", &long)),
            ContextMessage::from_message(Message::user("q2")),
            ContextMessage::from_message(Message::assistant("a2")),
            ContextMessage::from_message(long_tool_result("c2", &long)),
        ];

        // When keep_recent=3 (protect last 3), min_tokens=50 (200 chars)
        let opts = options_with_micro_compaction(3, 50);
        let result = project(&history, &opts).unwrap();

        // Then the first tool result (index 2) is replaced with marker
        assert_eq!(result.len(), 6);
        assert_eq!(result[2].role, Role::Tool);
        assert_eq!(result[2].text_content(), "[cleared]");
        // The recent tool result (index 5) is untouched
        assert_eq!(result[5].role, Role::Tool);
        assert_eq!(result[5].text_content(), long);
    }

    /// should leave short tool results untouched
    #[test]
    fn should_leave_short_tool_results_untouched_when_below_threshold() {
        // Given a tool result with short content (below threshold)
        let history = vec![
            ContextMessage::from_message(Message::user("q1")),
            ContextMessage::from_message(Message::assistant("a1")),
            ContextMessage::from_message(long_tool_result("c1", "short")),
            ContextMessage::from_message(Message::user("q2")),
        ];

        // When min_tokens=100 (400 chars), the short content is below threshold
        let opts = options_with_micro_compaction(1, 100);
        let result = project(&history, &opts).unwrap();

        // Then the tool result is not replaced
        assert_eq!(result[2].text_content(), "short");
    }

    /// should not touch user or assistant messages
    #[test]
    fn should_not_touch_user_or_assistant_messages_when_micro_compacting() {
        let long = "x".repeat(500);
        let history = vec![
            ContextMessage::from_message(Message::user(&long)),
            ContextMessage::from_message(Message::assistant(&long)),
            ContextMessage::from_message(long_tool_result("c1", &long)),
            ContextMessage::from_message(Message::user("recent")),
        ];

        let opts = options_with_micro_compaction(1, 50);
        let result = project(&history, &opts).unwrap();

        // User and assistant messages are untouched even if they're old
        assert_eq!(result[0].text_content(), long);
        assert_eq!(result[1].text_content(), long);
        // Tool result is replaced
        assert_eq!(result[2].text_content(), "[cleared]");
        // Recent user is untouched
        assert_eq!(result[3].text_content(), "recent");
    }

    /// should be no-op when marker is empty
    #[test]
    fn should_be_noop_when_marker_is_empty() {
        let long = "x".repeat(500);
        let history = vec![
            ContextMessage::from_message(Message::user("q1")),
            ContextMessage::from_message(Message::assistant("a1")),
            ContextMessage::from_message(long_tool_result("c1", &long)),
            ContextMessage::from_message(Message::user("q2")),
        ];

        let opts = ProjectOptions {
            micro_compaction_keep_recent: Some(1),
            micro_compaction_min_tokens: 50,
            micro_compaction_marker: String::new(),
            ..Default::default()
        };
        let result = project(&history, &opts).unwrap();

        // Tool result is not replaced because marker is empty
        assert_eq!(result[2].text_content(), long);
    }

    /// Given a history whose old tool result sits well before the
    /// micro-compaction keep-recent window, when it is projected with the
    /// normal options every live request uses, then the tool result content
    /// stays intact. Micro-compaction is a cache-hostile prefix mutation:
    /// replacing mid-history content invalidates the provider's prefix
    /// cache from the first mutated token onward, so the normal projection
    /// must not opt in — only an explicit `micro_compaction_keep_recent`
    /// does.
    #[test]
    fn should_keep_old_tool_results_intact_when_projecting_with_normal_options() {
        // Given a settled tool exchange followed by 21 later messages — the
        // tool result is 20+ messages old, past the keep-recent window
        let long = "x".repeat(500);
        let mut history = vec![
            ContextMessage::from_message(assistant_with_tool_call("a1", "c1", "Read")),
            tool_context("c1", &long),
        ];
        for i in 0..21 {
            history.push(ContextMessage::from_message(Message::assistant(format!(
                "recent {i}"
            ))));
        }

        // When projected with the normal options (what every outgoing
        // request uses)
        let result = project(&history, &ProjectOptions::normal()).unwrap();

        // Then the old tool result is intact, not the placeholder marker
        assert_eq!(result.len(), history.len());
        assert_eq!(result[1].role, Role::Tool);
        assert_eq!(
            result[1].text_content(),
            long,
            "normal projection must not micro-compact old tool results"
        );
    }

    /// should be no-op when keep_recent covers all messages
    #[test]
    fn should_be_noop_when_keep_recent_covers_all_messages() {
        let long = "x".repeat(500);
        let history = vec![
            ContextMessage::from_message(Message::user("q1")),
            ContextMessage::from_message(long_tool_result("c1", &long)),
        ];

        // keep_recent=10 but only 2 messages → cutoff = 0
        let opts = options_with_micro_compaction(10, 50);
        let result = project(&history, &opts).unwrap();

        assert_eq!(result[1].text_content(), long);
    }

    /// should not truncate tool messages without tool_call_id
    #[test]
    fn should_not_truncate_orphan_tool_messages_without_tool_call_id() {
        let long = "x".repeat(500);
        let mut orphan = long_tool_result("c1", &long);
        orphan.tool_call_id = None; // make it an orphan

        let history = vec![
            ContextMessage::from_message(Message::user("q1")),
            ContextMessage::from_message(orphan),
            ContextMessage::from_message(Message::user("q2")),
        ];

        // Disable orphan dropping to ensure the orphan survives to Step 7
        let opts = ProjectOptions {
            micro_compaction_keep_recent: Some(1),
            micro_compaction_min_tokens: 50,
            micro_compaction_marker: "[cleared]".to_string(),
            drop_orphan_results: false,
            ..Default::default()
        };
        let result = project(&history, &opts).unwrap();

        // Orphan tool (no tool_call_id) should not be truncated
        for msg in &result {
            if msg.role == Role::Tool {
                assert_ne!(msg.text_content(), "[cleared]");
            }
        }
    }

    // -- degraded media placeholder semantics --------------------------------

    #[test]
    fn should_carry_archive_path_in_degraded_media_placeholder() {
        // Given 带留档路径的图片部件进入降级投影（413）
        // When 媒体被占位替换
        // Then 占位含路径与重读提示（US6.1；ROADMAP §3 P2 语义差距收口）
        let part = ContentPart::ImageUrl {
            image_url: ImageUrlData {
                url: "data:image/png;base64,AAAA".into(),
                detail: None,
                id: Some("/blobs/ab12".into()),
            },
            detail: None,
        };
        let out = replace_media_with_text_placeholder(&part, false);
        let ContentPart::Text { text } = out else {
            panic!()
        };
        assert!(text.contains("size limit"));
        assert!(text.contains("/blobs/ab12"));
        assert!(text.contains("Re-read the file"));
    }

    #[test]
    fn should_use_short_form_when_no_archive_path() {
        // Given 无留档路径的音频部件
        // When 占位替换
        // Then 短文案、不编造路径
        let out = replace_media_with_text_placeholder(
            &ContentPart::AudioUrl {
                audio_url: "data:audio/wav;base64,AA".into(),
            },
            false,
        );
        let ContentPart::Text { text } = out else {
            panic!()
        };
        assert!(text.contains("audio"));
        assert!(text.contains("size limit"));
        assert!(!text.contains("saved to"));
    }

    #[test]
    fn should_build_size_limit_placeholder_when_format_not_rejected() {
        // Given 413 降级链（format_rejected=false）的带路径图片部件
        // When 占位替换
        // Then 占位为体积超限文案（US6.1），不含格式转换指引
        let part = ContentPart::ImageUrl {
            image_url: ImageUrlData {
                url: "data:image/png;base64,AAAA".into(),
                detail: None,
                id: Some("/blobs/ab12".into()),
            },
            detail: None,
        };
        let out = replace_media_with_text_placeholder(&part, false);
        let ContentPart::Text { text } = out else {
            panic!()
        };
        assert!(text.contains("size limit"));
        assert!(!text.contains("rejected its format"));
        assert!(!text.contains("Convert"));
    }

    #[test]
    fn should_build_format_rejected_placeholder_when_format_rejected() {
        // Given 格式被拒链（format_rejected=true）的带路径图片部件
        // When 占位替换
        // Then 占位为格式被拒文案并含转换指引（US6.2）
        let part = ContentPart::ImageUrl {
            image_url: ImageUrlData {
                url: "data:image/png;base64,AAAA".into(),
                detail: None,
                id: Some("/blobs/ab12".into()),
            },
            detail: None,
        };
        let out = replace_media_with_text_placeholder(&part, true);
        let ContentPart::Text { text } = out else {
            panic!()
        };
        assert!(text.contains("provider rejected its format"));
        assert!(text.contains("/blobs/ab12"));
        assert!(text.contains("Convert"));
        assert!(!text.contains("size limit"));
    }

    #[test]
    fn should_project_with_format_rejected_placeholders_when_option_set() {
        // Given 含图片消息的历史与 stripped_format_rejected 投影选项
        // When 投影
        // Then 图片被替换为格式被拒占位（含转换指引），而非体积文案
        let mut message = Message::user("what is this?");
        message.content.push(ContentPart::ImageUrl {
            image_url: ImageUrlData {
                url: "data:image/png;base64,AAAA".into(),
                detail: None,
                id: Some("/blobs/ab12".into()),
            },
            detail: None,
        });
        let history = vec![ContextMessage::from_message(message)];

        let result = project(&history, &ProjectOptions::stripped_format_rejected()).unwrap();

        assert_eq!(result.len(), 1);
        let ContentPart::Text { text } = &result[0].content[1] else {
            panic!("图片应替换为文本占位")
        };
        assert!(text.contains("provider rejected its format"));
        assert!(text.contains("Convert"));
    }
}
