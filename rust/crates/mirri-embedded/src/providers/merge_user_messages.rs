//! Collapse consecutive same-role "user" turns in a provider's already-converted
//! wire message list into one turn (a port of kosong's
//! `mergeConsecutiveUserMessages`, `packages/kosong/src/providers/merge-user-messages.ts`).
//!
//! Strict providers (Anthropic, Gemini/Vertex) reject consecutive user turns
//! with HTTP 400 ("roles must alternate"). Consecutive user turns arise
//! naturally:
//!   - after compaction, whose shape is `[kept user prompts, user-role
//!     summary, injected reminders]` — all user turns; and
//!   - when a user turn (steer/injection) follows a tool result.
//!
//! Both only become visible once tool messages have been converted to
//! user-role turns, which is why this runs at each provider's conversion
//! boundary rather than in the provider-agnostic projector: the projector
//! deliberately preserves message structure for lenient providers
//! (OpenAI/Kimi) that accept — and read more clearly — distinct turns, while
//! strict providers normalize for their own protocol here. Keeping the
//! algorithm in one place stops a provider from silently omitting it (the
//! original cause of the Gemini regression).
//!
//! The merge is asymmetric, keyed on whether the running turn is
//! tool-result-only:
//!   - a tool-result-only running turn absorbs whatever follows — another
//!     tool-result-only turn (the parallel-tool-use spec requires all tool
//!     results answering parallel calls to share one user turn) or a following
//!     text turn, yielding a valid `[tool_result, …, text]` turn;
//!   - a text running turn absorbs only a following text turn, never a leading
//!     tool-result turn (a tool-result must answer the immediately preceding
//!     assistant tool_use, which a text turn is not — though in well-formed
//!     transcripts this ordering never arises).
//!
//! Currently wired into the Anthropic adapter; the rust Google adapter shares
//! the same need and reuses this helper when it adopts the merge (TS
//! google-genai.ts calls the same helper).

/// Merge consecutive user turns in `messages` into single turns.
///
/// `is_user` / `is_tool_result_only` classify a wire message, and `merge`
/// combines `last` and `next` into a replacement turn (the TS reference passes
/// closures per provider; here the same three predicates travel as arguments).
/// Returns a new `Vec` with consecutive user turns merged.
pub(crate) fn merge_consecutive_user_messages<T>(
    messages: Vec<T>,
    is_user: impl Fn(&T) -> bool,
    is_tool_result_only: impl Fn(&T) -> bool,
    merge: impl Fn(T, T) -> T,
) -> Vec<T> {
    let mut out: Vec<T> = Vec::new();
    for message in messages {
        let merges_with_last = matches!(
            out.last(),
            Some(last)
                if is_user(last)
                    && is_user(&message)
                    && (is_tool_result_only(last) || !is_tool_result_only(&message))
        );
        if merges_with_last {
            let last = out.pop().expect("out is non-empty when a last was seen");
            out.push(merge(last, message));
        } else {
            out.push(message);
        }
    }
    out
}
