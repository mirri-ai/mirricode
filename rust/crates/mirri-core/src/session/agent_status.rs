//! Agent status phase model for the `event.agent.status.updated` payload.
//!
//! CR4 replaces the debug-placeholder status string with a structured
//! payload. This module defines [`AgentPhase`] — the phase field of that
//! payload — and [`phase_from_activity`], the pure derivation from the
//! agent's [`AgentActivityState`] fold. Later tasks wire the phase into the
//! `SessionEvent` variant, the publisher, and the TS contract chain.

use std::sync::{Arc, Mutex, RwLock};

use serde::{Deserialize, Serialize};

use crate::activity_view::{AgentActivityState, TurnPhase};
use crate::compaction::FullCompaction;
use crate::context_memory::ContextMemory;
use crate::plan::PlanManager;
use crate::scope::config::AgentScopeConfig;
use crate::swarm::SwarmModeService;
use crate::usage::AgentUsageService;

use super::events::{EventBus, SessionEvent};

/// The `phase` field of the combined `agent.status.updated` payload.
///
/// A v1-only wire concept with no producer on the v2 side, defined here at
/// the mirri-core edge so later tasks can publish it. The variant set maps
/// 1:1 to the TS `AgentPhase` union in
/// `packages/kap-server/src/services/legacyStatus/legacyStatus.ts:35-96`;
/// serde snake_case is the wire value (`tool_call`, `awaiting_approval`, …).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentPhase {
    /// No active turn and no recorded last-turn outcome.
    Idle,
    /// A turn is active but not streaming or in a tool call.
    Running,
    /// The model is actively streaming tokens.
    Streaming,
    /// One or more tool calls are in flight.
    ToolCall,
    /// The turn is waiting to retry after a failure.
    Retrying,
    /// An approval request is pending on the current turn.
    AwaitingApproval,
    /// The turn is ending prematurely (aborted / max_steps / error).
    Interrupted,
    /// The most recent turn has completed; no turn is active.
    Ended,
}

/// Derive the legacy v1 [`AgentPhase`] from the native [`AgentActivityState`]
/// fold snapshot.
///
/// Ported from the kap-server TS function `toLegacyPhase`
/// (`packages/kap-server/src/services/legacyStatus/legacyStatus.ts:155-238`),
/// which is the source of truth for the branch priority. Priority order:
///
/// 1. No active turn → `Ended` when a last-turn outcome exists, else `Idle`.
/// 2. Pending approvals → `AwaitingApproval` (checked *before* the ending
///    flag — a parallel approval resolve keeps `awaiting_approval` while any
///    approval is still pending; no premature `running`).
/// 3. Ending with a known reason → `Interrupted`.
/// 4. Otherwise the turn phase maps 1:1: `running` → [`AgentPhase::Running`],
///    `streaming` → [`AgentPhase::Streaming`], `tool_call` →
///    [`AgentPhase::ToolCall`], `retrying` → [`AgentPhase::Retrying`].
///
/// Two port notes, both faithful to the TS source:
///
/// - `interrupted` requires `ending_reason` to be set; an ending turn with an
///   unknown reason falls through to the turn-phase mapping (TS:
///   `turn.ending && turn.endingReason !== undefined`).
/// - The `retrying` turn phase maps directly to [`AgentPhase::Retrying`]
///   even when `retry` detail is absent (TS defaults the detail fields to
///   zero; there is no fallback to `running`).
///
/// The TS source returns `undefined` for the `disposing` / `disposed`
/// lifecycles (no v1 concept — emitting `idle` would mislead the UI). This
/// signature returns a total [`AgentPhase`]; callers (the event publisher,
/// later CR4 tasks) must check `lifecycle == Disposed` themselves and skip
/// publishing, mirroring the TS suppression.
pub fn phase_from_activity(state: &AgentActivityState) -> AgentPhase {
    let Some(turn) = &state.turn else {
        return if state.last_turn.is_some() {
            AgentPhase::Ended
        } else {
            AgentPhase::Idle
        };
    };
    if !turn.pending_approvals.is_empty() {
        return AgentPhase::AwaitingApproval;
    }
    if turn.ending && turn.ending_reason.is_some() {
        return AgentPhase::Interrupted;
    }
    match turn.phase {
        TurnPhase::Running => AgentPhase::Running,
        TurnPhase::Streaming => AgentPhase::Streaming,
        TurnPhase::ToolCall => AgentPhase::ToolCall,
        TurnPhase::Retrying => AgentPhase::Retrying,
    }
}

/// The `event.agent.status.updated` emitter (CR4) — one per agent,
/// attached to the agent's [`ActivityTracker`] by the host that owns the
/// scopes (the embedded message dispatch for the main agent, the
/// subagent host at spawn/resume for members).
///
/// The publisher holds only the read-side handles it needs (the agent's
/// context memory, usage service, and live config; the flow's compaction
/// orchestrator; the session-scoped plan and swarm-mode services) plus
/// the session's event bus. [`publish`](Self::publish) assembles the
/// [`SessionEvent::AgentStatusUpdated`] payload from live snapshots of
/// those handles — a mid-turn model switch or a plan-mode toggle is
/// reflected on the very next frame, with no re-wiring.
///
/// ## Lock discipline
///
/// Every handle is read in its own short critical section and no two
/// guards ever coexist, so the publisher can never participate in a lock
/// cycle. This matters because the call sites fire from inside the turn
/// loop (the tracker's recording methods run while the turn holds the
/// `TurnFlow` mutex): the guards the publisher takes individually match
/// the documented flow-internal order (`TurnFlow → compaction → … →
/// memory`), and the bus publish itself is non-blocking (broadcast send).
///
/// ## Suppression
///
/// The publisher has no lifecycle of its own; the suppression contract
/// lives one level up in
/// [`ActivityTracker::publish_status`](crate::turn_flow::activity::ActivityTracker::publish_status),
/// which skips publication entirely for a disposed view (a disposed view
/// keeps its turn — publishing its phase would report a stale agent).
pub struct AgentStatusPublisher {
    /// The session's broadcast bus (the same channel every other
    /// session-scoped event rides).
    bus: Arc<EventBus>,
    /// The agent's string identity (`"main"`, `"agent-<uuid>"`) — the
    /// same wire vocabulary every other agent-scoped session event uses.
    agent_id: String,
    /// The agent's context memory — `token_count_with_pending` is the
    /// context-token watermark.
    context_memory: Arc<Mutex<ContextMemory>>,
    /// The agent's compaction orchestrator —
    /// `effective_max_context_tokens` is the context-window budget.
    compaction: Arc<Mutex<FullCompaction>>,
    /// The agent's usage service — the live usage snapshot.
    usage: Arc<Mutex<AgentUsageService>>,
    /// The agent's live config — the effective model alias and
    /// reasoning-effort level.
    config: Arc<RwLock<AgentScopeConfig>>,
    /// The session-scoped plan manager — the plan-mode flag.
    plan: Arc<PlanManager>,
    /// The session-scoped swarm-mode service — the swarm-mode flag.
    swarm_mode: Arc<Mutex<SwarmModeService>>,
}

impl AgentStatusPublisher {
    /// Create a publisher reading the given handles and publishing on
    /// `bus` under `agent_id`.
    // The constructor's parameter list IS the handle set the brief
    // prescribes (bus + agent id + the read-side Arcs) — each wiring site
    // (the embedded dispatch, the subagent host's two faces) spells them
    // out explicitly, so a bundling struct would only rename the list.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        bus: Arc<EventBus>,
        agent_id: String,
        context_memory: Arc<Mutex<ContextMemory>>,
        compaction: Arc<Mutex<FullCompaction>>,
        usage: Arc<Mutex<AgentUsageService>>,
        config: Arc<RwLock<AgentScopeConfig>>,
        plan: Arc<PlanManager>,
        swarm_mode: Arc<Mutex<SwarmModeService>>,
    ) -> Self {
        Self {
            bus,
            agent_id,
            context_memory,
            compaction,
            usage,
            config,
            plan,
            swarm_mode,
        }
    }

    /// Publish one `event.agent.status.updated` frame for `phase`.
    ///
    /// Reads each handle's live snapshot and broadcasts the structured
    /// payload. Chosen semantics for the optional fields:
    ///
    /// - `model`: the config's model alias, absent when the alias is
    ///   empty (the legal "unset" config state — an empty string would
    ///   be a lie on the wire).
    /// - `contextTokens` / `maxContextTokens` / `contextUsage`: emitted
    ///   as a **pair, only when the effective max is > 0**. When the
    ///   window is unknown (`0` — a model missing from the catalog), the
    ///   ratio would be `inf` and serde would emit a `null`
    ///   `contextUsage`; omitting the whole pair keeps the frame honest
    ///   (absent, never null) — the ledger's inf-hazard guard.
    /// - `planMode` / `swarmMode` / `usage`: always present — the
    ///   session-scoped flags and the usage snapshot are readable on
    ///   every frame.
    pub fn publish(&self, phase: AgentPhase) {
        // Each block takes exactly one guard and releases it before the
        // next — no nested critical sections (see the lock-discipline
        // note on the struct).
        let (model, thinking_effort) = {
            let config = self.config.read().unwrap_or_else(|e| e.into_inner());
            (
                (!config.model_id.is_empty()).then(|| config.model_id.clone()),
                config.thinking_effort.clone(),
            )
        };
        let effective_max = self
            .compaction
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .effective_max_context_tokens();
        let (context_tokens, max_context_tokens) = if effective_max > 0 {
            let context = self
                .context_memory
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .token_count_with_pending();
            (Some(context), Some(effective_max))
        } else {
            (None, None)
        };
        let usage = self
            .usage
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .status();
        let plan_mode = self.plan.is_active();
        let swarm_mode = self
            .swarm_mode
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_active();

        self.bus.publish(SessionEvent::AgentStatusUpdated {
            agent_id: self.agent_id.clone(),
            phase,
            model,
            thinking_effort,
            context_tokens,
            max_context_tokens,
            plan_mode: Some(plan_mode),
            swarm_mode: Some(swarm_mode),
            usage: Some(usage),
        });
    }
}

#[cfg(test)]
mod publisher_tests {
    use super::*;

    /// A publisher over fresh default handles, sharing the given
    /// compaction orchestrator, plus its bus subscription.
    fn publisher_with_compaction(
        compaction: Arc<Mutex<FullCompaction>>,
    ) -> (
        AgentStatusPublisher,
        tokio::sync::broadcast::Receiver<SessionEvent>,
    ) {
        use super::super::events::DEFAULT_EVENT_BUS_CAPACITY;
        let bus = Arc::new(EventBus::new(DEFAULT_EVENT_BUS_CAPACITY));
        let rx = bus.subscribe();
        let publisher = AgentStatusPublisher::new(
            Arc::clone(&bus),
            "main".to_string(),
            Arc::new(Mutex::new(ContextMemory::new())),
            compaction,
            Arc::new(Mutex::new(AgentUsageService::new())),
            Arc::new(RwLock::new(AgentScopeConfig::default())),
            Arc::new(PlanManager::new()),
            Arc::new(Mutex::new(SwarmModeService::new())),
        );
        (publisher, rx)
    }

    /// Given a compaction orchestrator with a known window, when a frame
    /// is published, then the token pair and the derived usage ratio ride
    /// the payload alongside the phase and agent id.
    #[tokio::test]
    async fn should_carry_the_token_pair_when_the_window_is_known() {
        let mut compaction = FullCompaction::new(crate::compaction::CompactionStrategy::default());
        compaction.set_max_context_tokens(1000);
        let (publisher, mut rx) = publisher_with_compaction(Arc::new(Mutex::new(compaction)));

        publisher.publish(AgentPhase::Streaming);

        let event = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .expect("the frame is broadcast")
            .expect("the bus stays open");
        let SessionEvent::AgentStatusUpdated {
            agent_id,
            phase,
            context_tokens,
            max_context_tokens,
            ..
        } = event
        else {
            panic!("expected AgentStatusUpdated, got {event:?}");
        };
        assert_eq!(agent_id, "main");
        assert_eq!(phase, AgentPhase::Streaming);
        assert_eq!(context_tokens, Some(0), "a fresh memory has no tokens yet");
        assert_eq!(max_context_tokens, Some(1000));
    }

    /// Given a compaction orchestrator with an unknown window (0 — a
    /// model missing from the catalog), when a frame is published, then
    /// the whole token pair stays ABSENT — emitting `max = Some(0)`
    /// would make the projected `contextUsage` infinite and serde would
    /// turn it into a null key (the ledger's inf-hazard guard).
    #[tokio::test]
    async fn should_omit_the_token_pair_when_the_window_is_unknown() {
        let (publisher, mut rx) = publisher_with_compaction(Arc::new(Mutex::new(
            FullCompaction::new(crate::compaction::CompactionStrategy::default()),
        )));

        publisher.publish(AgentPhase::Running);

        let event = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .expect("the frame is broadcast")
            .expect("the bus stays open");
        let SessionEvent::AgentStatusUpdated {
            context_tokens,
            max_context_tokens,
            ..
        } = event
        else {
            panic!("expected AgentStatusUpdated, got {event:?}");
        };
        assert_eq!(context_tokens, None);
        assert_eq!(max_context_tokens, None);
    }
}
