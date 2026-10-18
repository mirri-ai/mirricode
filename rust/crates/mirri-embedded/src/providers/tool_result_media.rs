//! Shared wording and text view for tool-result media that cannot live inside
//! a provider's tool-result message shape and must be reattached (or
//! acknowledged with a placeholder) elsewhere in the request. Ports kosong's
//! `openai-common.ts` / `openai-legacy.ts` wording verbatim so engine output
//! stays byte-identical with the TS baseline.

use mirri_core::message::{ContentPart, Message};

/// Lead-in text of the follow-up user message that reattaches tool-result
/// images (OpenAI Chat Completions / Responses dialects).
pub(crate) const TOOL_RESULT_MEDIA_PROMPT: &str = "Attached media from tool result:";

/// Placeholder left inside the tool message when it carries an image and no
/// text (the image itself moves to the reattached user message).
pub(crate) const TOOL_RESULT_MEDIA_PLACEHOLDER: &str = "(see attached media)";

/// Omission placeholder for audio parts a dialect cannot carry.
pub(crate) const OMITTED_AUDIO_PLACEHOLDER: &str =
    "(audio omitted: not supported by this provider)";

/// Omission placeholder for video parts a dialect cannot carry.
pub(crate) const OMITTED_VIDEO_PLACEHOLDER: &str =
    "(video omitted: not supported by this provider)";

/// The text a tool-result message contributes to a dialect whose tool
/// message shape cannot carry content arrays: joined text parts, explicit
/// omission placeholders for audio/video, and `(see attached media)` when
/// the message carries only images (they are reattached separately by the
/// caller).
pub(crate) fn tool_result_text_content(message: &Message) -> String {
    let mut lines: Vec<String> = Vec::new();
    let text = message.text_content();
    if !text.is_empty() {
        lines.push(text);
    }
    if message
        .content
        .iter()
        .any(|p| matches!(p, ContentPart::AudioUrl { .. }))
    {
        lines.push(OMITTED_AUDIO_PLACEHOLDER.to_string());
    }
    if message
        .content
        .iter()
        .any(|p| matches!(p, ContentPart::VideoUrl { .. }))
    {
        lines.push(OMITTED_VIDEO_PLACEHOLDER.to_string());
    }
    if lines.is_empty()
        && message
            .content
            .iter()
            .any(|p| matches!(p, ContentPart::ImageUrl { .. }))
    {
        return TOOL_RESULT_MEDIA_PLACEHOLDER.to_string();
    }
    lines.join("\n")
}
