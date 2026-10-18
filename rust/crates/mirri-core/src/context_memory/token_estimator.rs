//! Heuristic token-count estimates matching the TypeScript implementation.
//!
//! Rules:
//! - ASCII characters: ~4 chars per token.
//! - Non-ASCII (CJK, emoji, etc.): ~1 char per token.
//! - Media (image / audio / video): flat 2000 tokens each.
//!
//! These are heuristic — they size context windows and compaction budgets,
//! never billing.

use crate::context::ContextMessage;
use crate::message::ContentPart;

/// Flat token cost for any single media part (image, audio, or video URL).
pub const MEDIA_TOKEN_ESTIMATE: u64 = 2000;

/// Estimate tokens for a raw text string.
///
/// ASCII characters are counted at ~4 chars/token; every non-ASCII code point
/// is counted as one full token (CJK, emoji, etc.).
pub fn estimate_tokens(text: &str) -> u64 {
    let mut ascii_chars = 0usize;
    let mut non_ascii_chars = 0usize;
    for ch in text.chars() {
        if ch.is_ascii() {
            ascii_chars += 1;
        } else {
            non_ascii_chars += 1;
        }
    }
    let ascii_tokens = (ascii_chars as f64 / 4.0).ceil() as u64;
    let non_ascii_tokens = non_ascii_chars as u64;
    ascii_tokens + non_ascii_tokens
}

/// Estimate tokens for a single [`ContextMessage`].
///
/// Sums content parts (text/think by character estimate, media at the flat
/// rate) and tool-call argument JSON strings.
pub fn estimate_message_tokens(msg: &ContextMessage) -> u64 {
    let mut total = 0u64;
    for part in &msg.message.content {
        match part {
            ContentPart::Text { text } | ContentPart::Think { text } => {
                total += estimate_tokens(text);
            }
            ContentPart::ImageUrl { .. }
            | ContentPart::AudioUrl { .. }
            | ContentPart::VideoUrl { .. }
            | ContentPart::File { .. } => {
                total += MEDIA_TOKEN_ESTIMATE;
            }
        }
    }
    for tc in &msg.message.tool_calls {
        total += estimate_tokens(&tc.function.arguments);
    }
    total
}

/// Estimate tokens for a slice of [`ContextMessage`]s.
pub fn estimate_messages_tokens(messages: &[ContextMessage]) -> u64 {
    messages.iter().map(estimate_message_tokens).sum()
}

/// Truncate text to approximately `max_tokens` tokens, keeping the beginning.
///
/// Walks code points left-to-right, mirroring [`estimate_tokens`] (ASCII ~4
/// chars/token, non-ASCII ~1 char/token), and stops at the first code point
/// that would push the running total over the budget.
pub fn truncate_text_to_tokens(text: &str, max_tokens: u64) -> String {
    if max_tokens == 0 {
        return String::new();
    }
    let mut ascii_count = 0u64;
    let mut non_ascii_count = 0u64;
    let mut result = String::new();
    for ch in text.chars() {
        if ch.is_ascii() {
            ascii_count += 1;
        } else {
            non_ascii_count += 1;
        }
        if ceil_div_4(ascii_count) + non_ascii_count > max_tokens {
            break;
        }
        result.push(ch);
    }
    result
}

/// Truncate text to approximately `max_tokens` tokens, keeping the end.
///
/// Walks code points right-to-left (consuming surrogate pairs whole), stopping
/// at the first code point that would push the running total over the budget.
/// The kept suffix represents the most-recent portion of the text.
pub fn truncate_text_to_tokens_from_end(text: &str, max_tokens: u64) -> String {
    if max_tokens == 0 {
        return String::new();
    }
    let chars: Vec<char> = text.chars().collect();
    let mut ascii_count = 0u64;
    let mut non_ascii_count = 0u64;
    let mut start = chars.len();
    for i in (0..chars.len()).rev() {
        if chars[i].is_ascii() {
            ascii_count += 1;
        } else {
            non_ascii_count += 1;
        }
        if ceil_div_4(ascii_count) + non_ascii_count > max_tokens {
            break;
        }
        start = i;
    }
    chars[start..].iter().collect()
}

#[inline]
fn ceil_div_4(n: u64) -> u64 {
    n.div_ceil(4)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_estimate_4_ascii_chars_per_token_when_estimating_ascii_text() {
        // 8 ASCII chars = 2 tokens
        assert_eq!(estimate_tokens("abcdefgh"), 2);
        // 7 ASCII chars = ceil(7/4) = 2 tokens
        assert_eq!(estimate_tokens("abcdefg"), 2);
        // 4 ASCII chars = 1 token
        assert_eq!(estimate_tokens("abcd"), 1);
    }

    #[test]
    fn should_estimate_1_token_per_cjk_char_when_estimating_cjk_text() {
        // 3 CJK chars = 3 tokens
        assert_eq!(estimate_tokens("你好吗"), 3);
        // Mixed: 4 ASCII + 1 CJK = 1 + 1 = 2
        assert_eq!(estimate_tokens("abcd你"), 2);
    }

    #[test]
    fn should_estimate_zero_tokens_when_estimating_empty_string() {
        assert_eq!(estimate_tokens(""), 0);
    }
}
