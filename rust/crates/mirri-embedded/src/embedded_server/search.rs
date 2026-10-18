//! The pure literal-search core behind the embedded `session.search` RPC.
//!
//! v1 is a full-scan, no-index literal matcher (the brief's architect
//! decision: single-machine session counts are small, the scan reads only
//! the in-memory / replayed context, and no new write-side artifact is
//! introduced). The pieces here are the pure halves of the handler:
//!
//! - [`fold_literal`] — the NFKC + lowercase folding every compared side
//!   goes through (the same `String.normalize('NFKC').toLowerCase()` the
//!   TS minidb `normalizeLiteral` applies);
//! - [`fingerprint`] / [`encode_page_token`] / [`decode_page_token`] — the
//!   opaque cursor pagination: the token carries the scan offset plus a
//!   fingerprint of the query conditions, and a mid-pagination condition
//!   change is an `invalid_params` error (the TS fingerprint semantics);
//! - [`ScanDoc`] / [`scan_docs`] — match, filter, snippet, and sort the
//!   collected documents into the time-desc hit order.
//!
//! The `terms` request mode is accepted by the handler but degrades to
//! this literal core (the response's `mode` field echoes the effective
//! mode; the full word-level index is a recorded followup, F-P10-1).

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde::{Deserialize, Serialize};
use unicode_normalization::UnicodeNormalization;

/// The page-size cap (`page_size > 50` clamps to 50, the TS default of 20
/// when absent).
pub(super) const PAGE_SIZE_MAX: u32 = 50;
/// The default page size when the request omits `page_size`.
pub(super) const PAGE_SIZE_DEFAULT: u32 = 20;
/// Half the snippet window: the snippet spans `radius` chars on each side
/// of the hit (the TS `makeSnippet` radius).
const SNIPPET_RADIUS: usize = 80;
/// The minimum folded-query length (in chars) a literal query must reach.
const MIN_QUERY_CHARS: usize = 2;

/// The literal fold every compared side goes through: NFKC normalization
/// plus lowercase — mirroring the TS `normalizeLiteral`
/// (`String.normalize('NFKC').toLowerCase()`), so full-width forms,
/// compat ligatures, and precomposed-vs-decomposed accents all compare
/// equal.
pub(super) fn fold_literal(text: &str) -> String {
    text.nfkc().collect::<String>().to_lowercase()
}

/// Whether the folded query is long enough to search with (the TS literal
/// mode's "at least 2 normalized characters" rule — a single folded char
/// is an n-gram degenerate, so it is rejected with `invalid_params`).
pub(super) fn query_is_searchable(folded_query: &str) -> bool {
    folded_query.chars().count() >= MIN_QUERY_CHARS
}

/// The fingerprint of the query conditions a page token is bound to:
/// every input that changes the matched set (folded query, container
/// session, role, clamped page size) — so a mid-pagination condition change
/// is detectable. The scan offset is deliberately excluded (it is the
/// thing the token carries).
pub(super) fn fingerprint(
    folded_query: &str,
    container_session_id: Option<&str>,
    role: Option<&str>,
    page_size: u32,
) -> String {
    format!(
        "{folded_query}|{}|{}|{page_size}",
        container_session_id.unwrap_or(""),
        role.unwrap_or(""),
    )
}

/// The cursor payload: the scan offset plus the fingerprint it was minted
/// under. Wire form is base64 of the JSON encoding (opaque to the client).
#[derive(Debug, Serialize, Deserialize)]
struct PageToken {
    offset: usize,
    fp: String,
}

/// Encode a page token for the next page starting at `offset`.
pub(super) fn encode_page_token(offset: usize, fp: &str) -> String {
    BASE64.encode(
        serde_json::to_vec(&PageToken {
            offset,
            fp: fp.to_string(),
        })
        .expect("page token serialization is infallible"),
    )
}

/// Decode a page token back into `(offset, fingerprint)`; `None` when the
/// token is not valid base64/JSON (the handler answers `invalid_params`).
pub(super) fn decode_page_token(token: &str) -> Option<(usize, String)> {
    let bytes = BASE64.decode(token).ok()?;
    let page: PageToken = serde_json::from_slice(&bytes).ok()?;
    Some((page.offset, page.fp))
}

/// The snippet window around the first hit: `radius` chars on each side of
/// the match, whitespace-collapsed, with an ellipsis marking each truncated
/// end — the TS `makeSnippet` shape. `anchor`/`hit_len` are char offsets in
/// the *folded* text; like the TS implementation, applying them to the
/// original text degrades to a slightly shifted window under NFKC, never
/// an error.
pub(super) fn snippet_window(text: &str, anchor: usize, hit_len: usize, radius: usize) -> String {
    let total = text.chars().count();
    let start = anchor.saturating_sub(radius);
    let end = (anchor + hit_len + radius).min(total);
    let window: String = text
        .chars()
        .skip(start)
        .take(end.saturating_sub(start))
        .collect();
    let collapsed: String = window.split_whitespace().collect::<Vec<_>>().join(" ");
    format!(
        "{}{collapsed}{}",
        if start > 0 { "…" } else { "" },
        if end < total { "…" } else { "" },
    )
}

/// One scannable document: a session title or one user/assistant message
/// (tool-call content is deliberately not indexed — F-P10-2). The text is
/// owned (message text is extracted into a fresh `String`); the identity
/// fields borrow the session info the scan iterates.
pub(super) struct ScanDoc<'a> {
    pub session_id: &'a str,
    pub session_title: &'a str,
    /// Owned: the per-agent histories it comes from are consumed inside
    /// the scan loop while the docs outlive each iteration.
    pub agent_id: String,
    /// `user` | `assistant` | `title` (already validated by the caller).
    pub role: &'static str,
    pub text: String,
    /// Epoch ms — the session's `updatedAt` (context memory carries no
    /// per-message timestamps; the honest v1 deviation).
    pub time: i64,
    /// The turn ordinal (None for title docs).
    pub turn: Option<u64>,
    /// The document's position in its scan group (history index; the
    /// tie-break that keeps equal-time hits deterministic).
    pub ord: u64,
}

/// One matched hit, in the final wire order.
pub(super) struct ScanHit {
    pub session_id: String,
    pub session_title: String,
    pub agent_id: String,
    pub role: &'static str,
    pub snippet: String,
    pub time: i64,
    pub turn: Option<u64>,
}

/// Match every doc against the folded query, apply the role filter, build
/// the snippet windows, and order the hits time-desc (ties: session id,
/// agent id, then the most recent doc first) — deterministic across equal
/// timestamps, so pages are stable.
pub(super) fn scan_docs(
    docs: &[ScanDoc<'_>],
    folded_query: &str,
    role: Option<&str>,
) -> Vec<ScanHit> {
    let mut hits: Vec<(ScanHit, u64)> = Vec::new();
    for doc in docs {
        if let Some(role) = role
            && doc.role != role
        {
            continue;
        }
        let folded_text = fold_literal(&doc.text);
        let Some(at) = folded_text.find(folded_query) else {
            continue;
        };
        // Char offsets for the window math (byte offset → char index of
        // the folded text; the anchor then degrades onto the original text
        // exactly like the TS snippet does under NFKC).
        let anchor = folded_text[..at].chars().count();
        let hit_chars = folded_query.chars().count();
        let snippet = if doc.role == "title" {
            // Title hits carry the title itself as the snippet (TS shape).
            doc.text.to_string()
        } else {
            snippet_window(&doc.text, anchor, hit_chars, SNIPPET_RADIUS)
        };
        hits.push((
            ScanHit {
                session_id: doc.session_id.to_string(),
                session_title: doc.session_title.to_string(),
                agent_id: doc.agent_id.to_string(),
                role: doc.role,
                snippet,
                time: doc.time,
                turn: doc.turn,
            },
            doc.ord,
        ));
    }
    hits.sort_by(|a, b| {
        b.0.time
            .cmp(&a.0.time)
            .then_with(|| a.0.session_id.cmp(&b.0.session_id))
            .then_with(|| a.0.agent_id.cmp(&b.0.agent_id))
            .then_with(|| b.1.cmp(&a.1))
    });
    hits.into_iter().map(|(hit, _)| hit).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_fold_fullwidth_ligature_and_case_when_normalizing_literal_text() {
        // Given mixed full-width, ligature, and uppercase spellings
        // When each is folded
        // Then they compare equal to their plain lowercase forms
        assert_eq!(fold_literal("ＣＡＦＥ"), "cafe");
        assert_eq!(fold_literal("ﬁle"), "file");
        assert_eq!(fold_literal("GREAT"), "great");
        assert_eq!(fold_literal("cafe\u{301}"), fold_literal("café"));
    }

    #[test]
    fn should_reject_queries_when_the_folded_form_has_fewer_than_two_chars() {
        // Given single-char and empty queries
        // When checked for searchability
        // Then only >=2-char folded queries pass
        assert!(!query_is_searchable(""));
        assert!(!query_is_searchable("x"));
        assert!(!query_is_searchable("Ａ"));
        assert!(query_is_searchable("ab"));
        assert!(query_is_searchable("café"));
    }

    #[test]
    fn should_round_trip_the_offset_when_a_page_token_is_encoded_and_decoded() {
        // Given a fingerprint and an offset
        let fp = fingerprint("needle", None, None, PAGE_SIZE_DEFAULT);
        // When a token is encoded and decoded back
        let token = encode_page_token(40, &fp);
        let (offset, decoded_fp) = decode_page_token(&token).expect("round trip");

        // Then both fields survive and the token is opaque (base64, not
        // the raw JSON)
        assert_eq!(offset, 40);
        assert_eq!(decoded_fp, fp);
        assert!(!token.contains("needle"));
    }

    #[test]
    fn should_return_none_when_the_page_token_is_not_a_valid_cursor() {
        // Given garbage tokens
        // When decoded
        // Then they are rejected without panicking
        assert!(decode_page_token("not-a-token").is_none());
        assert!(decode_page_token("").is_none());
        assert!(decode_page_token(&BASE64.encode("[]")).is_none());
    }

    #[test]
    fn should_distinguish_fingerprints_when_the_query_conditions_change() {
        // Given one query's fingerprint
        let base = fingerprint("needle", None, None, 20);
        // When the query, container, role, or page size changes
        // Then every changed condition yields a different fingerprint
        assert_ne!(fingerprint("other", None, None, 20), base);
        assert_ne!(fingerprint("needle", Some("s1"), None, 20), base);
        assert_ne!(fingerprint("needle", None, Some("user"), 20), base);
        assert_ne!(fingerprint("needle", None, None, 50), base);
        // And the same conditions reproduce it exactly
        assert_eq!(fingerprint("needle", None, None, 20), base);
    }

    #[test]
    fn should_bound_the_snippet_window_when_the_hit_is_midway_through_a_long_text() {
        // Given a long body with the hit in the middle
        let text = format!("{}needle{}", "filler ".repeat(60), " trailer".repeat(60));
        // When the window is taken around the hit (char anchor, ASCII text)
        let byte_at = text.find("needle").expect("hit present");
        let anchor = text[..byte_at].chars().count();
        let snippet = snippet_window(&text, anchor, "needle".chars().count(), SNIPPET_RADIUS);

        // Then the snippet is bounded, centered on the hit, and marks both
        // truncated ends
        assert!(snippet.contains("needle"));
        assert!(snippet.starts_with('…'));
        assert!(snippet.ends_with('…'));
        assert!(
            snippet.chars().count() <= SNIPPET_RADIUS * 2 + "needle".len() + 2,
            "bounded window: {snippet}"
        );
        assert!(snippet.len() < text.len());
    }

    #[test]
    fn should_keep_a_short_text_intact_when_the_window_covers_it_all() {
        // Given a short text fully inside the radius
        // When the window is taken
        let snippet = snippet_window("tiny needle text", 5, 6, SNIPPET_RADIUS);
        // Then no ellipsis is added and whitespace collapses
        assert_eq!(snippet, "tiny needle text");
    }

    #[test]
    fn should_order_hits_by_time_desc_and_break_ties_deterministically_when_scanning() {
        // Given docs from two sessions: an older pair from session A and a
        // newer pair from session B (same session times, ascending turns)
        let docs = [
            ScanDoc {
                session_id: "a",
                session_title: "A",
                agent_id: "main".to_string(),
                role: "user",
                text: "needle one".to_string(),
                time: 100,
                turn: Some(0),
                ord: 0,
            },
            ScanDoc {
                session_id: "a",
                session_title: "A",
                agent_id: "main".to_string(),
                role: "user",
                text: "needle two".to_string(),
                time: 100,
                turn: Some(1),
                ord: 1,
            },
            ScanDoc {
                session_id: "b",
                session_title: "B",
                agent_id: "main".to_string(),
                role: "assistant",
                text: "needle three".to_string(),
                time: 200,
                turn: Some(0),
                ord: 0,
            },
        ];
        // When scanned
        let hits = scan_docs(&docs, "needle", None);

        // Then the newer session leads and the equal-time pair keeps a
        // deterministic order (the later doc first)
        assert_eq!(hits.len(), 3);
        assert_eq!(hits[0].session_id, "b");
        assert_eq!(hits[0].role, "assistant");
        assert_eq!(hits[1].session_id, "a");
        assert_eq!(hits[1].turn, Some(1), "later doc first within a session");
        assert_eq!(hits[2].turn, Some(0));
    }

    #[test]
    fn should_apply_the_role_filter_when_scanning_docs() {
        // Given one doc per role
        let docs = [
            ScanDoc {
                session_id: "s",
                session_title: "S",
                agent_id: "main".to_string(),
                role: "user",
                text: "needle".to_string(),
                time: 100,
                turn: Some(0),
                ord: 0,
            },
            ScanDoc {
                session_id: "s",
                session_title: "S",
                agent_id: "main".to_string(),
                role: "assistant",
                text: "needle".to_string(),
                time: 100,
                turn: Some(0),
                ord: 1,
            },
            ScanDoc {
                session_id: "s",
                session_title: "S needle",
                agent_id: "".to_string(),
                role: "title",
                text: "S needle".to_string(),
                time: 100,
                turn: None,
                ord: 0,
            },
        ];
        // When scanned with the assistant filter
        let hits = scan_docs(&docs, "needle", Some("assistant"));
        // Then only the assistant doc hits (the title doc does not leak
        // through a role filter it does not satisfy)
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].role, "assistant");
    }
}
