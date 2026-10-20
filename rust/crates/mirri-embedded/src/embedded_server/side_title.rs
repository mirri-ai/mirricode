//! Out-of-band session-title generation (spec 09 / ADR-020 + SDR-016):
//! when the first LLM response of a main-agent user turn lands on an
//! untitled session — or, as the settle-site retry, when that turn
//! settles — spawn a side LLM call that rides the provider's prompt
//! cache: same resolver construction, same system prompt (held by the
//! adapter), same tool list, same projected message prefix as the main
//! loop's request, with one appended instruction message. The main turn
//! loop is untouched: nothing here records into context memory or the
//! wire log, and the response never enters the conversation.

use std::sync::Arc;
use std::time::Duration;

use mirri_core::scope::{AgentScope, SessionScope};
use mirri_core::session::{EventBus, SessionEvent};
use mirri_core::{
    LlmResolver as _, MediaCapabilityAdapter, MessagePipeline, VideoReferenceResolver,
};

use mirri_session_index::SessionIndex;

use super::session_index::note_turn_settled;
use crate::providers::ConfigResolvingLlm;
use crate::workspace_registry::WorkspaceRegistry;

/// The side call must never linger: a wedged provider falls back to the
/// rule-based title instead of blocking nothing forever.
const SIDE_TITLE_TIMEOUT: Duration = Duration::from_secs(30);

/// Which dispatch site spawned the side call — the fallback semantics
/// differ per phase (spec 09 §2.3, SDR-016).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SideTitlePhase {
    /// The in-turn site: the turn's first LLM response just landed. A
    /// failure here must NOT run the rule-based fallback — the settle
    /// site retries the side call first, so the title never degrades
    /// to a truncated prompt while the turn still has a chance to name
    /// it properly (A9).
    InTurn,
    /// The settle site: the turn finished (successfully, or cancelled —
    /// a cancelled turn still reaches the successful-settle path). A
    /// failure here is final: release the claim and fall back to the
    /// rule-based title (R-D).
    SettleRetry,
}

#[derive(Clone)]
pub(super) struct SideTitleContext {
    pub session: Arc<SessionScope>,
    pub agent: Arc<AgentScope>,
    pub llm_factory: Arc<dyn mirri_core::LlmFactory>,
    pub registry: Arc<WorkspaceRegistry>,
    pub index: Option<Arc<SessionIndex>>,
    pub event_bus: Arc<EventBus>,
    pub user_text: String,
    pub phase: SideTitlePhase,
}

impl SideTitleContext {
    /// The same dispatch, re-labelled for the other phase — the
    /// orchestrator runs both attempts in sequence, so it rebuilds the
    /// context instead of moving it.
    fn with_phase(&self, phase: SideTitlePhase) -> Self {
        SideTitleContext {
            phase,
            ..self.clone()
        }
    }
}

/// The rule-based fallback (spec 09 §2.3): derive a title from the user's
/// message text via the same guard and sanitize path as the side call
/// (`set_auto_title` is idempotent: a titled/custom session is a silent
/// no-op, and a prompt with no text derives nothing). Returns the stored
/// title when this call wrote one; the wire announcement mirrors the
/// side call's `SessionMetaUpdated` event.
pub(super) async fn apply_fallback_title(
    event_bus: &EventBus,
    meta: &mirri_core::scope::SessionMetaHandle,
    user_text: &str,
) -> Option<String> {
    let stored = meta.set_auto_title(user_text.to_string()).await?;
    event_bus.publish(SessionEvent::SessionMetaUpdated {
        title: Some(stored.clone()),
        patch: None,
    });
    Some(stored)
}

/// Handle a side-call failure according to the dispatch phase (SDR-016):
/// in-turn failures only release the claim and leave the session
/// untitled — the settle site retries; settle-retry failures release
/// the claim and run the rule-based fallback so the session always ends
/// up named (R-D).
async fn handle_side_title_failure(
    phase: SideTitlePhase,
    session: &SessionScope,
    meta: &mirri_core::scope::SessionMetaHandle,
    event_bus: &EventBus,
    user_text: &str,
) {
    meta.release_side_title_generation();
    if phase == SideTitlePhase::SettleRetry {
        let _ = apply_fallback_title(event_bus, meta, user_text).await;
    } else {
        tracing::debug!(
            session_id = %session.id(),
            "in-turn side title call failed; deferring to the settle-site retry"
        );
    }
}

/// Watch for the turn's first LLM response landing (spec 09 / SDR-016):
/// subscribe to the session bus and, on the first `TurnStepCompleted`
/// carrying this dispatch's own wire turn id, start the naming question's
/// in-turn attempt; when the turn then closes, await that attempt and run
/// the settle-site retry.
///
/// The two attempts are **sequenced, never raced**. The phase semantics
/// assume it: an in-turn failure defers to a settle retry that has not
/// happened yet, and a settle retry racing an in-flight in-turn call would
/// find the one-shot claim taken and skip the naming question — including
/// its fallback — leaving the session untitled when the in-turn call then
/// failed. The in-turn attempt still runs as its own task so this watch
/// keeps draining the bus while it is in flight (a blocked subscriber
/// would fall behind and could miss the very `TurnCompleted` that gates
/// the retry).
///
/// Filtering by the wire turn id the dispatch minted (not by agent or
/// session identity) is what keeps subagent / cron / task turns out:
/// they share the session bus but never carry this dispatch's turn id.
/// The id is passed in (not captured from the bus) because the
/// dispatch publishes `TurnStarted` before spawning this watch — a
/// broadcast subscription never replays it.
pub(super) fn spawn_in_turn_title_watch(ctx: SideTitleContext, turn_id: String) {
    let mut events = ctx.event_bus.subscribe();
    tokio::spawn(async move {
        // A session that already has (or manually set) a title never
        // reaches the side call — checked inside `run_side_title_call`
        // too; this early exit only avoids holding the watch open for the
        // whole turn.
        {
            let metadata = ctx.session.metadata().await;
            if metadata.is_custom_title
                || !mirri_core::session::title::is_untitled_title(&metadata.title)
            {
                return;
            }
        }
        let mut in_turn: Option<tokio::task::JoinHandle<()>> = None;
        loop {
            let event = match events.recv().await {
                Ok(event) => event,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
            };
            match event {
                SessionEvent::TurnStepCompleted {
                    turn_id: step_turn,
                    step,
                    ..
                } if step_turn == turn_id && step == 1 && in_turn.is_none() => {
                    let in_turn_ctx = ctx.with_phase(SideTitlePhase::InTurn);
                    in_turn = Some(tokio::spawn(run_side_title_call(in_turn_ctx)));
                }
                SessionEvent::TurnCompleted {
                    turn_id: ended_turn,
                    ..
                } if ended_turn == turn_id => {
                    // The turn succeeded: the settle site is this dispatch's
                    // last chance to name the session. Awaiting the in-turn
                    // attempt first keeps the retry behind it (a titled
                    // session makes the retry a no-op inside its own guard).
                    if let Some(handle) = in_turn.take() {
                        let _ = handle.await;
                    }
                    run_side_title_call(ctx.with_phase(SideTitlePhase::SettleRetry)).await;
                    return;
                }
                SessionEvent::TurnFailed {
                    turn_id: ended_turn,
                    ..
                } if ended_turn == turn_id => {
                    // A failed turn settles through the dispatch's error
                    // path, which owns the rule-based fallback.
                    return;
                }
                _ => {}
            }
        }
    });
}

/// Run one naming attempt to completion. The claim makes a second attempt
/// a no-op once the session is named; failures fall back per phase.
async fn run_side_title_call(ctx: SideTitleContext) {
    let SideTitleContext {
        session,
        agent,
        llm_factory,
        registry,
        index,
        event_bus,
        user_text,
        phase,
    } = ctx;
    // Guard: main-agent sessions only, untitled, one claim per process.
    {
        let metadata = session.metadata().await;
        if metadata.is_custom_title
            || !mirri_core::session::title::is_untitled_title(&metadata.title)
        {
            return;
        }
    }
    let meta = session.meta_handle();
    if !meta.claim_side_title_generation() {
        return;
    }
    // Same resolver construction as the main loop (message_handlers
    // dispatch): the side call resolves the same provider/model and
    // the same frozen system prompt as the settled turn.
    let resolver = ConfigResolvingLlm::new(agent.config(), llm_factory);
    let llm = match resolver.resolve() {
        Ok(llm) => llm,
        Err(reason) => {
            tracing::warn!(%reason, "side title call: provider resolution failed; falling back");
            handle_side_title_failure(phase, &session, &meta, &event_bus, &user_text).await;
            if phase == SideTitlePhase::SettleRetry {
                note_turn_settled(index, &registry, &session).await;
            }
            return;
        }
    };
    // Rebuild the exact request prefix the main loop sent: the
    // normal projection (no micro-compaction — the prefix-cache
    // preservation choice) plus the same media adaptation pipeline.
    // The in-turn phase snapshots whatever has landed so far; the
    // prefix stays a strict extension of the main loop's request.
    let messages = {
        let memory = agent.context_memory();
        let mem = memory.lock().unwrap_or_else(|e| e.into_inner());
        mem.messages()
    };
    let mut tools = {
        let manager = agent.tool_manager();
        let guard = manager.read();
        guard.loop_tools()
    };
    let capability = llm.capability();
    if !capability.image_in && !capability.video_in {
        tools.retain(|tool| tool.name != "ReadMediaFile");
    }
    let mut pipeline = MessagePipeline::new();
    pipeline.register(Box::new(VideoReferenceResolver::for_capability(capability)));
    pipeline.register(Box::new(MediaCapabilityAdapter::new(capability.clone())));
    let messages = pipeline.run("normal", messages);

    let outcome = tokio::time::timeout(
        SIDE_TITLE_TIMEOUT,
        mirri_core::session::title_llm::generate_session_title(llm.as_ref(), messages, tools),
    )
    .await;

    match outcome {
        Ok(Ok((raw, usage))) => {
            // Cache observability (rust AGENTS.md gate 5): the three
            // buckets — uncachedInput / cacheRead / cacheWrite.
            tracing::info!(
                session_id = %session.id(),
                uncached_input = usage.input_tokens,
                cache_read = usage.cache_read_tokens,
                cache_write = usage.cache_creation_tokens,
                output = usage.output_tokens,
                "side title call usage"
            );
            match meta.set_auto_title(raw).await {
                Some(stored) => {
                    event_bus.publish(SessionEvent::SessionMetaUpdated {
                        title: Some(stored),
                        patch: None,
                    });
                    // The settle's write-through ran before the title
                    // landed — write it through again so state.json and
                    // the index carry the title.
                    note_turn_settled(index, &registry, &session).await;
                }
                None => {
                    // `None` is two cases (plan D4): either the naming
                    // guard refused because the session got titled
                    // concurrently (a user rename) — the claim stays
                    // held and nothing is left to do — or the raw title
                    // sanitized to nothing displayable, which leaves
                    // the session untitled and must release the claim
                    // and run the rule-based fallback exactly like the
                    // failure arms, so the next user turn can retry.
                    let metadata = session.metadata().await;
                    if !metadata.is_custom_title
                        && mirri_core::session::title::is_untitled_title(&metadata.title)
                    {
                        tracing::warn!(
                            "side title call produced no displayable title; \
                             falling back to the rule-based title"
                        );
                        meta.release_side_title_generation();
                        let _ = apply_fallback_title(&event_bus, &meta, &user_text).await;
                        note_turn_settled(index, &registry, &session).await;
                    }
                }
            }
        }
        Ok(Err(error)) => {
            tracing::warn!(%error, "side title call failed; falling back to the rule-based title");
            handle_side_title_failure(phase, &session, &meta, &event_bus, &user_text).await;
            if phase == SideTitlePhase::SettleRetry {
                note_turn_settled(index, &registry, &session).await;
            }
        }
        Err(_) => {
            tracing::warn!("side title call timed out; falling back to the rule-based title");
            handle_side_title_failure(phase, &session, &meta, &event_bus, &user_text).await;
            if phase == SideTitlePhase::SettleRetry {
                note_turn_settled(index, &registry, &session).await;
            }
        }
    }
}
