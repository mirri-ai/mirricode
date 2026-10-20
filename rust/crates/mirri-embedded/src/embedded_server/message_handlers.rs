//! `message.send` / `message.abort` / `message.steer` handlers and the
//! engine turn execution (`run_engine_turn`) they share.

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};

use mirri_core::TurnWaitState;
use mirri_core::tool_registry::ValidatorCache;
use mirri_core::{
    AgentBlobService, AgentId, AgentScope, CancelToken, ContentPart, ContextMemory,
    EngineError as CoreEngineError, HookEngine, InteractionKernel, LiveEventCallback, LlmResolver,
    PermissionManager, PromptOrigin, ReverseRpcCallback, SessionEvent, SessionId, SessionManager,
    SessionScope, TurnEndResult, TurnFlow, WireOpRecorder,
};
use mirri_proto::WireOp;
use mirri_transport::adapter::EngineError;

use crate::providers::ConfigResolvingLlm;
use mirri_core::event_projection::turn_stop_reason_wire;
use mirri_core::session::journal_route::RoutedWireRecorder;
use mirri_core::{GoalStatus, goal_status_wire};

use super::additional_dirs::session_grant_sink;
use super::goal_driver::{
    TurnOutcome, maybe_launch_goal_chain, reset_goal_chain_guards, settle_goal_after_turn,
    turn_outcome,
};
use super::responses::{MessageSendFireResult, MessageSendWaitResult, ack, to_result};
use super::session_index::note_turn_settled;
use super::side_title::{
    SideTitleContext, SideTitlePhase, apply_fallback_title, spawn_in_turn_title_watch,
};
use super::{
    DEFAULT_MAIN_AGENT_ID, EmbeddedServer, InFlightTurn, WaitTurnGuard,
    coerce_aborted_if_cancelled, engine_error, goal_snapshot_json, invalid_params,
    live_chunk_callback, mirror_reverse_rpc_callback_into_agent_config, normalize_params,
    not_found, prompt_text, resolve_session_agent, session_error, summarize, turn_end_to_json,
};

/// Stall deadline applied to one engine turn driven by `message.send`
/// (issue 15E).
///
/// Same value as `mirri-server`'s constant, different semantics: this is an
/// *inactivity* bound, not a total-turn bound. Every live event the engine
/// emits — LLM deltas, tool-call starts/completions, retry notices — pushes
/// the deadline out again (see [`run_turn_with_stall_deadline`]), so a long
/// agentic turn with steady progress is never dropped; only a turn that goes
/// silent for the full window — a provider that never answers — is.
/// Without any deadline an unresponsive LLM would hold the
/// `std::sync::Mutex<TurnFlow>` guard on the blocking thread indefinitely.
///
/// Kept strictly ABOVE kosong's stream-read failure windows — the data-line
/// idle timeout `SSE_IDLE_TIMEOUT` (300 s) and the byte-level
/// `HTTP_READ_TIMEOUT` (240 s), both in `mirri-kosong/src/provider.rs` /
/// `streaming.rs`: a silently wedged provider stream must hit those
/// provider-side read errors FIRST and surface as a retryable stream
/// error — the watchdog is the engine-side backstop, never the first
/// responder.
pub(super) const ENGINE_TURN_STALL_TIMEOUT: Duration = Duration::from_secs(600);

/// The ADR-006 long backstop window — the third-tier deadline the stall
/// watchdog applies to Degraded waits (Unknown, probe-degraded,
/// all-settled-but-not-returning). Deliberately NOT the
/// `turnStallTimeoutMs` config key: the two windows have different
/// semantics (classic = per-kind / engine silence; backstop = the
/// framework-bug escape and the keep-alive-masked upstream hang) and are
/// scaled independently — tests shorten the backstop through the
/// `with_turn_backstop_timeout` seam.
///
/// 30 minutes per the ADR-006 裁决参数 (统一兜底窗口, 不按 WaitKind 分窗).
pub(super) const ENGINE_TURN_BACKSTOP_TIMEOUT: Duration = Duration::from_secs(1800);
// ===========================================================================
// Engine turn execution
// ===========================================================================

/// Admit the session's blob store (media-originals archive + wire
/// offloads) into the session-shared path sandbox.
///
/// The multimodal spec hands the archive paths to agents and subagents as
/// recovery handles (US5.1) that must go through the standard workspace
/// validation (US7.3) — but the store lives outside the workspace root
/// (injected home `<home>/v3_sessions`; the ADR-007 global
/// content-addressed store will move it again). The admission is
/// location-agnostic: whatever root the service uses is registered as one
/// additional dir, extending the sandbox for every agent of the session
/// (the `WorkspacePaths` handle is shared, subagents included) while the
/// traversal rules for everything else stay intact.
fn admit_blob_store_into_sandbox(session: &SessionScope, blobs: &AgentBlobService) {
    if let Some(paths) = session.workspace_paths() {
        paths.add_additional(blobs.blobs_dir());
    }
}

/// Why an engine turn failed to produce a [`TurnEndResult`] — the failure
/// modes shared by the wait and fire paths of `message.send`.
pub(super) enum EngineTurnError {
    /// The blocking task panicked instead of returning a result.
    Panicked(String),
    /// [`ENGINE_TURN_STALL_TIMEOUT`] elapsed with the engine emitting no
    /// live progress (no LLM deltas, no tool activity).
    TimedOut,
    /// The engine itself failed (provider error, hook rejection, …).
    Engine(CoreEngineError),
}

impl EngineTurnError {
    /// Human-readable failure description, used for the wait-mode error
    /// response and the fire-mode `turn_failed` event payload.
    pub(super) fn message(&self) -> String {
        match self {
            EngineTurnError::Panicked(join_error) => {
                format!("engine turn task panicked: {join_error}")
            }
            EngineTurnError::TimedOut => format!(
                "engine turn stalled for {} seconds with no LLM or tool progress",
                ENGINE_TURN_STALL_TIMEOUT.as_secs()
            ),
            EngineTurnError::Engine(engine_error) => engine_error.to_string(),
        }
    }

    /// The failure's domain code (`ErrorCode::as_str()` vocabulary) for the
    /// wire's structured error object. Engine failures report their real
    /// class; bridge-level failures (panic, timeout) have no engine error to
    /// ask and fall back to `internal`.
    pub(super) fn domain_code(&self) -> &'static str {
        match self {
            EngineTurnError::Panicked(_) | EngineTurnError::TimedOut => "internal",
            EngineTurnError::Engine(engine_error) => engine_error.code().as_str(),
        }
    }
}

/// Decide whether the session's MAIN journal is still fresh from this
/// process's perspective — the shared verdict of the write-side consistency
/// checks (session-consistency batch 2).
///
/// A single stat of the main journal (resolved through the session-manager
/// routing seam) is compared against the session's known-size anchor:
/// `Fresh` (disk == anchor) is the only pass; `Divergent` (a foreign writer
/// appended since our last absorb), `Truncated` (the journal was rebuilt
/// externally), and an unprobed journal (unresolvable path, missing/unreadable
/// file — 宁拦勿写, spec §5: an unreadable journal cannot prove freshness)
/// all fail.
///
/// Main journal ONLY, deliberately (spec §2.2): a subagent child-journal
/// divergence does not participate — child journals are absorbed by the
/// follow path (batch 1.5) and carry no turn-ordering contract for the main
/// conversation.
///
/// The `manager` is passed as an already-resolved `Option` (not resolved
/// inside) so both call sites share the caller's routing-seam failure
/// posture: a `None` manager falls through to the unprobed rejection.
fn journal_tail_verdict_fresh(
    manager: Option<&Arc<SessionManager>>,
    session_id: &str,
    known_size: u64,
) -> bool {
    // The verdict needs one stat of the main journal, routed through the
    // manager's injected sync filesystem (zero-IO); both fence sites are
    // sync (the pre-record write fence, the guard closure polled inside the
    // turn's blocking submission).
    let Some(manager) = manager else {
        return false;
    };
    let wire_path = manager
        .persistence()
        .wire_log_path(&SessionId::new(session_id));
    mirri_core::session::consistency::probe_journal_version(manager.sync_io().as_ref(), &wire_path)
        .is_some_and(|disk| {
            matches!(
                mirri_core::session::consistency::check_version(disk, known_size),
                mirri_core::session::consistency::VersionCheck::Fresh
            )
        })
}

/// Build the mid-turn abort guard (session-consistency invariant 2, spec
/// §2.2 ③) for a session the consistency flag covers: the predicate the
/// engine loop polls before each step and each tool batch, holding the
/// same journal-tail verdict [`journal_tail_verdict_fresh`] applies — a
/// foreign append (or truncation, or an unprobed journal) trips the guard
/// and the turn aborts through the existing cancellation path.
///
/// The guard re-reads the anchor at each poll (`known_size()` is a fresh
/// atomic load, not a captured value): the turn's OWN writes advance the
/// anchor at the write point, so the guard never mistakes its own partial
/// records for foreign ones. A journal probe failure mid-turn (the journal
/// file vanishing) trips the guard too — 宁拦勿写's mid-turn analog.
///
/// When the guard trips, THIS layer logs the domain cause (the core
/// loop's abort message is deliberately generic — "turn aborted by guard"
/// — because the loop only knows its predicate failed): the journal tail
/// diverged from the turn's anchor. The log fires once per turn (the flag
/// flips on the first trip), matching the loop's own once-per-poll-site
/// warn — the two poll sites (between steps, before the tool batch) never
/// stack duplicate cause lines.
fn turn_abort_guard_for(
    flag_registry: &mirri_core::FlagRegistry,
    manager: Option<Arc<SessionManager>>,
    session_id: &str,
    session: &Arc<SessionScope>,
) -> Option<mirri_core::loop_engine::TurnAbortGuard> {
    if !flag_registry.enabled(mirri_core::SESSION_CONSISTENCY_FOLLOW_FLAG) {
        return None;
    }
    let manager = manager.clone();
    let session = Arc::clone(session);
    let session_id = session_id.to_string();
    let cause_logged = Arc::new(std::sync::atomic::AtomicBool::new(false));
    Some(Arc::new(move || {
        let fresh = journal_tail_verdict_fresh(
            manager.as_ref(),
            &session_id,
            session.wire_log_known_size(),
        );
        if !fresh && !cause_logged.swap(true, std::sync::atomic::Ordering::SeqCst) {
            tracing::warn!(
                session_id = %session_id,
                "aborting the turn: the session journal diverged from this \
                 instance's anchor mid-turn (a foreign write arrived; the \
                 message will need to be resent after the session converges)"
            );
        }
        fresh
    }))
}

/// Run one engine turn on the blocking pool, under the
/// [`ENGINE_TURN_STALL_TIMEOUT`] inactivity deadline. Shared by the wait
/// path (which propagates failures as engine errors) and the fire path
/// (which publishes a `turn_failed` event instead).
///
/// Mirrors `mirri-server`'s `run_engine_turn`; the differences are the
/// `live_callback`, which the embedded engine passes through to
/// [`TurnFlow::run_one_turn`] so incremental `TextDelta`s stream onto the
/// session's event bus (the server passes `None` — its UI events stream
/// over the WebSocket layer instead), the `wire_turn_id` the dispatch
/// minted for its `turn.started` announcement (plumbed onto the flow so
/// the turn-internal `turn.step.*` events carry the same turn key), and
/// the `wire_recorder`, installed on the turn flow so the loop's events
/// and usage are persisted to the session's wire log.
///
/// `#[allow(clippy::too_many_arguments)]` matches the codebase convention
/// for the engine-loop plumbing (see `TurnFlow::run_one_turn`).
#[allow(clippy::too_many_arguments)]
pub(super) async fn run_engine_turn(
    flow_handle: Arc<std::sync::Mutex<TurnFlow>>,
    agent_hook_engine: Arc<HookEngine>,
    external_hook_env: HashMap<String, String>,
    flag_registry: Arc<mirri_core::FlagRegistry>,
    agent_tools: Arc<parking_lot::RwLock<mirri_core::ToolManager>>,
    memory: Arc<std::sync::Mutex<ContextMemory>>,
    llm_resolver: Arc<dyn LlmResolver>,
    prompt: Vec<ContentPart>,
    origin: mirri_core::PromptOrigin,
    with_skills: Option<Vec<mirri_core::WithSkillsEntry>>,
    live_callback: Option<LiveEventCallback>,
    wire_turn_id: String,
    wire_recorder: Option<Arc<dyn WireOpRecorder>>,
    permission_manager: Arc<PermissionManager>,
    reverse_rpc_callback: Option<Arc<dyn ReverseRpcCallback>>,
    task_manager: Arc<mirri_core::TaskManager>,
    interaction_kernel: Option<Arc<InteractionKernel>>,
    cancel: CancelToken,
    stall_timeout: Duration,
    backstop_timeout: Duration,
    turn_abort_guard: Option<mirri_core::loop_engine::TurnAbortGuard>,
    stall_log_ids: (String, String),
) -> Result<TurnEndResult, EngineTurnError> {
    // The ADR-006 pull-model registry: fetch the turn's wait registry from
    // ADR-006: the registry handle is NOT taken here — the turn mints a
    // fresh registry per turn (run_one_turn_inner), so an up-front handle
    // would observe the PREVIOUS turn's (emptied) registry and the probe
    // would be dead code. The closure resets the flow's registry under
    // its own flow lock FIRST, then hands the same Arc to both the turn
    // body and the deadline loop.
    // The settle-side cleanup (after the turn) runs outside the blocking
    // closure — this Arc survives the move below.
    let task_manager_for_cleanup = Arc::clone(&task_manager);
    // The stall-watchdog log key (ADR-004 batch c): session + agent join
    // the wire turn id in every warn line.
    let stall_key_ids = (stall_log_ids.0, stall_log_ids.1);
    let stall_wire_turn_id = wire_turn_id.clone();
    let outcome = tokio::task::spawn_blocking(move || {
        let mut turn_flow = flow_handle.lock().unwrap_or_else(|e| e.into_inner());
        // ADR-006 (review C1): reset the per-turn wait registry HERE —
        // under the flow lock, before the deadline loop below captures
        // its probe handle — so the turn body's registrations and the
        // watchdog's probes share the SAME Arc. The turn's
        // run_one_turn_inner sees a clean registry and reuses it.
        let wait_registry = turn_flow.reset_wait_registry();
        // The turn-scoped liveness listener (ADR-004 batch a) and the
        // post-turn cleanup both need the shared manager after the move
        // into `set_task_manager` below — split the Arc.
        let task_manager_for_settle = Arc::clone(&task_manager);
        // Clone BEFORE installing: the recorder handle consumed by
        // `set_wire_recorder` is also what the hard-drop settle below
        // records the closure's synthetic events and the failed turn
        // bracket with (the wire recorder is the session's durable
        // append log, shared with the dispatch arm's own `record_op`).
        let settle_recorder = wire_recorder.clone();
        turn_flow.set_wire_recorder(wire_recorder);
        // The mid-turn abort guard (session-consistency invariant 2): the
        // engine loop polls this before each step and each tool batch. See
        // [`turn_abort_guard_for`] for the verdict; `None` (flag off)
        // leaves the loop guard-free.
        turn_flow.set_turn_abort_guard(turn_abort_guard);
        turn_flow.set_external_hooks(agent_hook_engine, external_hook_env);
        // The engine's flag registry — carries the config `[experimental]`
        // layer, so the turn's hooks resolve flags through the same
        // env → config → defaults chain as the TS engine.
        turn_flow.set_flag_registry(flag_registry);
        turn_flow.set_permission_manager(permission_manager);
        turn_flow.set_reverse_rpc_callback(reverse_rpc_callback);
        turn_flow.set_task_manager(task_manager);
        turn_flow.set_interaction_kernel(interaction_kernel);
        // The wire turn id (`turn-<uuid>`) the dispatch minted and already
        // announced via `turn.started` — plumbed down so the turn-internal
        // `turn.step.*` events publish the same turn key.
        turn_flow.set_wire_turn_id(Some(wire_turn_id));
        // The turn runs on a snapshot of the tool table, not on a read
        // guard held for the turn's whole duration. The guard was the
        // deadlock's other half: a registration's `write()` behind it
        // pinned a worker until the turn ended, and enough of them froze
        // the whole runtime. The snapshot is semantically what the guard
        // already provided — writers could not run under a turn-held read
        // guard anyway, so the turn sees one stable view either way (the
        // per-step `loop_tools` re-read now re-reads the snapshot).
        let tools = agent_tools.read().clone();
        // Stall-watchdog feed: the turn's live events (LLM deltas, tool
        // starts/completions, retries) each reset the inactivity deadline,
        // so the bound measures engine silence — a provider that never
        // answers — not the turn's total duration. A turn with steady
        // progress is never dropped for running long.
        let (progress_tx, progress_rx) = tokio::sync::watch::channel(());
        // ADR-004 batch a — the waited-on object's activity signal: a
        // blocked task wait whose task heartbeat advances feeds the same
        // watchdog clock, so a turn healthily waiting on a progressing
        // task is not judged stalled (US5.5). A silent heartbeat (a dead
        // task) feeds nothing and the stall window still applies. The
        // listener is scoped to this turn: taken off on both settle paths
        // below so a stale watch sender never lingers on the shared
        // manager.
        // ADR-006 milestone 4 RETAINED (not retired): the pull probe
        // covers the turn's OWN waits, but a subagent's internal
        // TaskOutput wait (a two-hop wait chain) does not bubble to the
        // parent's registry — the listener is the cross-hop backstop
        // until wait-chain propagation lands (O10 open item). Known
        // limitation (review I4): it feeds on ANY session task's
        // heartbeat, not just the waited one — coarser than the probe,
        // but only ever in the keep-alive direction.
        task_manager_for_settle.set_liveness_listener(Some(Arc::new({
            let progress_tx = progress_tx.clone();
            move |_task_id: &str| {
                let _ = progress_tx.send(());
            }
        })));
        let live_callback = Some({
            let progress_tx = progress_tx;
            Arc::new(move |event: mirri_core::LiveEvent| {
                let _ = progress_tx.send(());
                if let Some(callback) = live_callback.as_ref() {
                    callback(event);
                }
            }) as LiveEventCallback
        });
        // block_in_place ensures the runtime can still make progress when
        // subagents (spawned by AgentSwarm) call spawn_blocking + block_on
        // inside this turn — without it, nested block_on deadlocks.
        tokio::task::block_in_place(|| {
            let result = tokio::runtime::Handle::current().block_on(run_turn_with_stall_deadline(
                turn_flow.run_one_turn(
                    prompt,
                    origin,
                    // The closure keeps its own Arc so the hard-drop
                    // settle below can still lock the memory and apply
                    // `close_interrupted` after the turn future is gone.
                    Arc::clone(&memory),
                    &tools,
                    llm_resolver,
                    &cancel,
                    &mut ValidatorCache::new(),
                    live_callback,
                    // The ADR-006 admission stamp rides the body message
                    // into live memory (None for every other launch).
                    with_skills,
                ),
                progress_rx,
                stall_timeout,
                backstop_timeout,
                wait_registry,
                cancel.clone(),
                StallTurnKey {
                    session_id: &stall_key_ids.0,
                    agent_id: &stall_key_ids.1,
                    wire_turn_id: &stall_wire_turn_id,
                },
            ));
            if result.is_none() {
                // The hard drop: the turn ignored its own cancellation.
                // The closure still holds the flow guard and the memory
                // Arc, so it settles the wire itself — a dangling
                // tool.call and a missing turn bracket would leave every
                // later message deferred behind the unresolved exchange
                // on replay (方式 B 取消闭合, the same closed-state
                // contract `close_interrupted_turn` applies on the
                // engine's own settle paths).
                tokio::runtime::Handle::current().block_on(async {
                    let pending = {
                        let mut mem = memory.lock().unwrap_or_else(|e| e.into_inner());
                        mem.close_interrupted()
                    };
                    let error_message = format!(
                        "engine turn stalled for {} seconds with no LLM or tool \
                         progress and did not settle after cancellation",
                        stall_timeout.as_secs()
                    );
                    if let Some(recorder) = settle_recorder.as_ref() {
                        for event in &pending {
                            recorder
                                .record_op(WireOp::ContextAppendLoopEvent {
                                    event: event.to_wire_value(),
                                })
                                .await;
                        }
                        if let Some(turn_id) = turn_flow.active_turn_id() {
                            recorder
                                .record_op(WireOp::TurnEnded {
                                    turn_id,
                                    reason: "failed".to_string(),
                                    error: Some(serde_json::json!(error_message)),
                                    duration_ms: None,
                                })
                                .await;
                        }
                    }
                });
            }
            result
        })
    })
    .await
    .map_err(|join_error| EngineTurnError::Panicked(join_error.to_string()))?;

    // The turn is over on every path (settled, cancelled, or hard-dropped):
    // take this turn's liveness listener off the shared task manager.
    task_manager_for_cleanup.set_liveness_listener(None);

    match outcome {
        None => Err(EngineTurnError::TimedOut),
        Some(engine_result) => engine_result.map_err(EngineTurnError::Engine),
    }
}

/// The addressed agent's turn recorder — SDR-B4-02's journal split applied
/// to the dispatch paths.
///
/// The MAIN agent's turn-domain ops (`turn.prompt` / `context.append_message`
/// / `turn.ended` / loop events) ARE the session log's turn stream — the
/// single log a resume replays into the main agent's history and the fork
/// pipeline anchors on. Any OTHER agent (a `session.registerAgent` child, a
/// restored member) records its turn-domain ops into its OWN journal
/// (`agents/<id>/wire.jsonl`) through a [`RoutedWireRecorder`]: writing them
/// into the main log would fold the subagent's conversation into the MAIN
/// agent's history at replay, and the fork's latest-completed-turn anchor
/// would cut at the subagent's bracket instead of the main agent's.
/// Session-domain ops (goal, plan, permission, …) still reach the session's
/// log through the router.
async fn turn_recorder_for(session: &Arc<SessionScope>, agent_id: &str) -> Arc<dyn WireOpRecorder> {
    if agent_id == DEFAULT_MAIN_AGENT_ID {
        Arc::clone(session) as Arc<dyn WireOpRecorder>
    } else {
        let child_journal = session.child_wire_log(agent_id).await;
        Arc::new(RoutedWireRecorder::new(
            child_journal,
            session.wire_recorder(),
        ))
    }
}

/// The stall-watchdog log context: the identifiers a warn line names when
/// a turn is judged stalled (ADR-004 batch c).
struct StallTurnKey<'a> {
    session_id: &'a str,
    agent_id: &'a str,
    wire_turn_id: &'a str,
}

/// Run `turn` under the inactivity deadline (see
/// [`ENGINE_TURN_STALL_TIMEOUT`]): every progress tick on `progress` pushes
/// the deadline out again, so the bound measures silence, not total turn
/// duration.
///
/// `run_engine_turn` feeds the channel by wrapping the turn's live-event
/// callback — every [`mirri_core::LiveEvent`] the engine emits (LLM deltas,
/// tool-call starts/completions, retry notices) is a tick, so any engine
/// activity counts as progress. Returns the turn's output, or `None` when
/// the engine went silent for the full `stall_timeout` AND ignored its
/// cancel token — the turn future is dropped at that point, exactly as
/// `tokio::time::timeout` would drop it.
///
/// ADR-004 batch c: both termination stages leave a warn log so a stalled
/// turn is attributable from the server log alone (US5.4) — the cooperative
/// cancel names the turn (session/agent/wire turn id); the hard drop adds
/// that the turn ignored its own cancellation.
#[allow(clippy::too_many_arguments)]
async fn run_turn_with_stall_deadline<T>(
    turn: impl Future<Output = T>,
    mut progress: tokio::sync::watch::Receiver<()>,
    stall_timeout: Duration,
    backstop_timeout: Duration,
    wait_registry: Arc<mirri_core::WaitRegistry>,
    cancel: CancelToken,
    turn_key: StallTurnKey<'_>,
) -> Option<T> {
    let mut turn = std::pin::pin!(turn);
    let stall = tokio::time::sleep(stall_timeout);
    tokio::pin!(stall);
    // The long window only arms when the probe degrades (ADR-006: the
    // Degraded tier starts the backstop clock; it never cascades from the
    // classic window). Held as a deadline instant — the sleep future is
    // created fresh in each select round (Sleep is !Unpin, and pinning
    // it inside a loop-local Option fights the borrow checker; the
    // deadline form is equivalent and simpler).
    let mut backstop_deadline: Option<tokio::time::Instant> = None;
    loop {
        tokio::select! {
            result = &mut turn => return Some(result),
            _ = &mut stall => {
                // Probe the turn's wait set at expiry (ADR-006: pull at
                // expiry only — never in the hot path).
                match wait_registry.probe() {
                    TurnWaitState::Advancing => {
                        // The pull-model reset: the waited-on object is
                        // still advancing — same effect as a progress tick.
                        // A recovered wait set also DISARMS the backstop
                        // (review C2): an armed long window must never
                        // outlive the degradation that armed it, or a
                        // turn that recovered would be hard-dropped half
                        // an hour later — a brand-new false kill.
                        backstop_deadline = None;
                        stall
                            .as_mut()
                            .reset(tokio::time::Instant::now() + stall_timeout);
                    }
                    TurnWaitState::Idle => {
                        // No wait in flight — plain engine silence, today's
                        // behavior (the classic window IS the engine-silence
                        // window).
                        cooperative_stall_cancel(
                            &cancel,
                            "stall watchdog fired: turn went silent for the full window \
                             with no registered wait; cooperatively cancelling",
                            &turn_key,
                            stall_timeout,
                        );
                        return settle_or_drop(turn.as_mut(), stall_timeout, &turn_key).await;
                    }
                    TurnWaitState::Stalled { kind } => {
                        // The waited-on object exists but stopped
                        // advancing — the per-kind window (same scale as
                        // the classic one; 两窗不串联 applies the SAME
                        // two-stage close as engine silence).
                        cooperative_stall_cancel(
                            &cancel,
                            format!(
                                "stall watchdog fired: the turn's registered wait \
                                 ({}) reported its owner stalled; cooperatively cancelling",
                                kind.as_str()
                            ),
                            &turn_key,
                            stall_timeout,
                        );
                        return settle_or_drop(turn.as_mut(), stall_timeout, &turn_key).await;
                    }
                    TurnWaitState::Degraded => {
                        // Unknown / degraded / all-settled-but-not-returning
                        // — arm (or keep) the LONG window; the diagnostic
                        // dump fires when it elapses.
                        if backstop_deadline.is_none() {
                            tracing::warn!(
                                session_id = %turn_key.session_id,
                                agent_id = %turn_key.agent_id,
                                turn_id = %turn_key.wire_turn_id,
                                stall_secs = stall_timeout.as_secs(),
                                backstop_secs = backstop_timeout.as_secs(),
                                wait_kinds = ?wait_registry.wait_kind_labels(),
                                "stall watchdog: the turn's wait set is degraded \
                                 (unknown or settled-but-not-returning); arming the \
                                 long backstop window"
                            );
                            backstop_deadline =
                                Some(tokio::time::Instant::now() + backstop_timeout);
                        }
                        // The classic window re-arms too — a subsequent
                        // Advancing probe (the wait set recovers) must be
                        // able to reset it.
                        stall
                            .as_mut()
                            .reset(tokio::time::Instant::now() + stall_timeout);
                    }
                }
            }
            _ = async {
                match backstop_deadline {
                    Some(deadline) => tokio::time::sleep_until(deadline).await,
                    // No backstop armed — park this branch forever (the
                    // select's other arms drive the loop).
                    None => std::future::pending::<()>().await,
                }
            } => {
                // The long window elapsed — but a stale arming must not
                // kill a turn that recovered (review C2): re-probe first.
                // Only a still-degraded (or newly Idle — the wait set
                // vanished without the turn returning, the framework-bug
                // escape) set reaches the dump-and-cancel.
                match wait_registry.probe() {
                    TurnWaitState::Advancing => {
                        backstop_deadline = None;
                        stall
                            .as_mut()
                            .reset(tokio::time::Instant::now() + stall_timeout);
                        continue;
                    }
                    recovered @ (TurnWaitState::Idle
                    | TurnWaitState::Stalled { .. }) => {
                        // The set recovered to a classic-tier shape — the
                        // classic window governs again from now (the
                        // degenerate arm below never fires for it).
                        backstop_deadline = None;
                        stall
                            .as_mut()
                            .reset(tokio::time::Instant::now() + stall_timeout);
                        let _ = &recovered;
                        continue;
                    }
                    TurnWaitState::Degraded => {}
                }
                // The diagnostic dump first (ADR-006 触发可见 —
                // structured tracing fields only, no file I/O), then the
                // same two-stage close.
                tracing::warn!(
                    session_id = %turn_key.session_id,
                    agent_id = %turn_key.agent_id,
                    turn_id = %turn_key.wire_turn_id,
                    stall_secs = stall_timeout.as_secs(),
                    backstop_secs = backstop_timeout.as_secs(),
                    wait_kinds = ?wait_registry.wait_kind_labels(),
                    "stall watchdog long window fired: dumping diagnostics and \
                     hard-dropping the degraded turn"
                );
                cooperative_stall_cancel(
                    &cancel,
                    "stall watchdog long window fired: the turn's degraded wait \
                     set never recovered; cooperatively cancelling",
                    &turn_key,
                    stall_timeout,
                );
                return settle_or_drop(turn.as_mut(), stall_timeout, &turn_key).await;
            }
            changed = progress.changed() => {
                if changed.is_ok() {
                    stall
                        .as_mut()
                        .reset(tokio::time::Instant::now() + stall_timeout);
                }
                // A dropped sender can only mean the engine feed is gone;
                // the remaining deadline still bounds the turn.
            }
        }
    }
}

/// The stall-flavored cooperative cancel (ADR-004 batch d): mark the
/// source so the settle path reports `stalled`, never a user `cancelled`.
fn cooperative_stall_cancel(
    cancel: &CancelToken,
    reason: impl std::fmt::Display,
    turn_key: &StallTurnKey<'_>,
    stall_timeout: Duration,
) {
    tracing::warn!(
        session_id = %turn_key.session_id,
        agent_id = %turn_key.agent_id,
        turn_id = %turn_key.wire_turn_id,
        stall_secs = stall_timeout.as_secs(),
        %reason,
    );
    cancel.cancel_stalled();
}

/// The post-cancel grace (ADR-004 two-stage close): give the engine one
/// more stall window to settle itself; only a turn that ignores its own
/// cancellation falls through to the hard drop.
async fn settle_or_drop<T>(
    turn: std::pin::Pin<&mut impl Future<Output = T>>,
    stall_timeout: Duration,
    turn_key: &StallTurnKey<'_>,
) -> Option<T> {
    let mut turn = turn;
    match tokio::time::timeout(stall_timeout, &mut turn).await {
        Ok(result) => Some(result),
        Err(_) => {
            tracing::warn!(
                session_id = %turn_key.session_id,
                agent_id = %turn_key.agent_id,
                turn_id = %turn_key.wire_turn_id,
                stall_secs = stall_timeout.as_secs(),
                "stall watchdog hard drop: the turn ignored its own \
                 cancellation for the full grace window"
            );
            None
        }
    }
}

impl EmbeddedServer {
    /// Validate the agent's LLM provider and build the switch-aware turn
    /// resolver: errors when no provider is configured for this agent or
    /// when the provider config cannot build.
    ///
    /// Turn-launching dispatch paths run this BEFORE recording the fact the
    /// turn announces — the fact and its turn must stay an atomic pair on
    /// the wire log, so a missing or broken provider config must never
    /// leave a fact with no turn.
    ///
    /// Returns [`ConfigResolvingLlm`]: the agent's live config drives each
    /// resolution, so a mid-turn `agent.setModel` takes effect on the
    /// running turn's next step instead of the next turn.
    pub(super) fn resolve_turn_llm(
        &self,
        agent: &Arc<AgentScope>,
    ) -> Result<Arc<dyn LlmResolver>, EngineError> {
        let Some(llm_factory) = &self.llm_factory else {
            return Err(EngineError::Internal(
                "no LLM provider configured for this agent".to_string(),
            ));
        };
        let llm_resolver: Arc<dyn LlmResolver> =
            Arc::new(ConfigResolvingLlm::new(agent.config(), llm_factory.clone()));
        // Fail fast on a broken provider config before the turn starts (the
        // pre-switch dispatch-time create_llm check). The send path reuses the
        // returned resolver for its turn; validation-only callers (the
        // skill-activation fact guard) discard it, accepting a second build.
        llm_resolver.resolve().map_err(|reason| {
            EngineError::Internal(format!(
                "LLM provider creation failed for agent '{}': {reason}",
                agent.id()
            ))
        })?;
        Ok(llm_resolver)
    }

    /// SDR-005 / US3.6 inline skill-reference injection (the "N activation
    /// wraps + 1 body message" turn sequence, ADR-004 §3): scan the user
    /// prompt text for activatable inline reference links, resolve each
    /// skill references against the session catalog, and — for every unique
    /// resolved skill, in appearance order — record a `skill.activate` fact,
    /// append a `<kimi-skill-loaded>` user message (with a `skill_activation`
    /// origin, `trigger = user-inline`) to memory and the wire log, and
    /// publish the `event.skill.activated` announcement. The body prompt has
    /// its reference links replaced by the idempotent `[display]` markers.
    ///
    /// Fail-soft contract (ADR-004): an unknown SKILL.md target, a disabled
    /// skill, a model-invocation-gated skill (sub-skill /
    /// `disable_model_invocation`), or a command reference does NOT block the
    /// message — the body keeps the original link text and no activation is
    /// attempted. The body prompt is rewritten in place (reference links for
    /// activated skills become bare `[display]` markers).
    ///
    /// Returns the ADR-006 admission stamp: the deduped activated-skill
    /// entries in reference order (`trigger = "user-inline"`), empty when
    /// nothing activated. The caller stamps the returned list onto BOTH
    /// persistence faces of the body message — the wire-log row and the
    /// live in-memory `ContextMessage` — so the two never diverge.
    async fn maybe_inject_inline_skill_activations(
        &self,
        session: &Arc<SessionScope>,
        session_id: &str,
        origin: &PromptOrigin,
        prompt: &mut Vec<ContentPart>,
        memory: &Arc<std::sync::Mutex<ContextMemory>>,
        turn_recorder: &Arc<dyn WireOpRecorder>,
    ) -> Vec<mirri_core::WithSkillsEntry> {
        // Only real user input carries inline references (the slash path does
        // its own single-skill injection).
        if !matches!(origin, PromptOrigin::User) {
            return Vec::new();
        }
        // A text-only prompt is required: inline links live in a text part. A
        // mixed-media prompt (images/audio/video) has no single text run to
        // rewrite safely — keep it conservative and skip injection.
        let [ContentPart::Text { text }] = prompt.as_slice() else {
            return Vec::new();
        };
        let parsed = mirri_core::parse_inline_references(text);
        if parsed.references.is_empty() {
            return Vec::new();
        }
        let catalog = session.skill_catalog();
        let event_bus = self
            .resolve_session_artifacts(session_id)
            .map(|a| a.event_bus.clone())
            .ok();
        let message_id = format!("msg-{}", uuid::Uuid::new_v4());

        // Pass 1 — decide which references activate, in appearance order,
        // deduped by resolved skill name (ADR-004 folds duplicates into one
        // activation keyed by the resolved skill instance). Unknown targets,
        // disabled skills, model-invocation-gated skills (sub-skills and
        // `disable_model_invocation`), and command refs are skipped (fail
        // soft — the body keeps them). The deduped rows ARE the ADR-006
        // admission stamp: name + the SKILL.md path each reference resolved
        // through + the user-inline trigger.
        let mut activations: Vec<(usize, String)> = Vec::new(); // (ordinal of first span, skill name)
        let mut with_skills: Vec<mirri_core::WithSkillsEntry> = Vec::new();
        {
            let mut seen: Vec<String> = Vec::new();
            for (ordinal, reference) in parsed.references.iter().enumerate() {
                if reference.kind != mirri_core::InlineReferenceKind::Skill {
                    // Command refs: never executed by the engine — command
                    // execution is the submitting client's job (ADR-004 §4).
                    // Body keeps the link. TODO(SDR-005): surface pending
                    // commands on a response channel when one exists.
                    continue;
                }
                let Some(skill) = catalog.resolve_by_skill_md_path(&reference.target) else {
                    continue; // unknown target — fail soft
                };
                if catalog.is_disabled(&skill.name) {
                    continue; // user-disabled — fail soft
                }
                if skill.disable_model_invocation || skill.is_sub_skill {
                    // Model-invocation gate (design 13 §3.4 / ADR-B2): these
                    // flags mean "the model should not pull this in
                    // unsolicited" — inline activation IS model-unsolicited,
                    // so gated skills get the same fail-soft treatment as
                    // disabled ones (no wrap, link kept verbatim). The user
                    // slash path (`agent.activateSkill`) stays unaffected:
                    // user-invoked activation is exactly what the flags allow.
                    continue;
                }
                if seen.iter().any(|n| n == &skill.name) {
                    continue; // duplicate — folded
                }
                seen.push(skill.name.clone());
                activations.push((ordinal, skill.name.clone()));
                with_skills.push(mirri_core::WithSkillsEntry {
                    name: skill.name,
                    target: reference.target.clone(),
                    trigger: "user-inline".to_string(),
                });
            }
        }
        if activations.is_empty() {
            return Vec::new(); // nothing activated — body unchanged
        }

        // Pass 2 — emit one `<kimi-skill-loaded>` wrap (fact + memory + wire +
        // event) per activated skill, at its first occurrence.
        for (ordinal, skill_name) in activations.iter() {
            let reference = &parsed.references[*ordinal];
            let skill = catalog.get(skill_name).expect("resolved above");
            let Some(render) = catalog.render_user_inline_skill_prompt(skill_name) else {
                continue;
            };
            let skill_source = match skill.source {
                mirri_core::SkillSource::Builtin => "builtin",
                mirri_core::SkillSource::User => "user",
                mirri_core::SkillSource::Project => "project",
                mirri_core::SkillSource::Plugin => "extra",
            };
            let activation_id = format!("{message_id}#{ordinal}");
            let skill_origin = PromptOrigin::SkillActivation {
                activation_id: Some(activation_id.clone()),
                skill_name: Some(skill.name.clone()),
                trigger: Some("user-inline".to_string()),
                skill_args: None,
                skill_path: Some(reference.target.clone()),
                skill_source: Some(skill_source.to_string()),
            };
            let origin_wire = serde_json::to_value(&skill_origin)
                .unwrap_or_else(|_| json!({ "kind": "skill_activation" }));
            // Wrap message → memory + wire; fact → wire; event → bus. Each
            // activation is its own user message threaded before the body.
            memory
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .append_user_message(
                    vec![ContentPart::Text {
                        text: render.clone(),
                    }],
                    skill_origin,
                );
            turn_recorder
                .record_op(WireOp::SkillActivate {
                    origin: origin_wire.clone(),
                })
                .await;
            turn_recorder
                .record_op(WireOp::ContextAppendMessage {
                    message: json!({
                        "role": "user",
                        "content": [json!({ "type": "text", "text": render })],
                        "origin": origin_wire,
                    }),
                })
                .await;
            if let Some(event_bus) = event_bus.as_ref() {
                event_bus.publish(SessionEvent::SkillActivated {
                    activation_id,
                    skill_name: skill.name.clone(),
                    args: None,
                    trigger: "user-inline".to_string(),
                    skill_path: Some(reference.target.clone()),
                    skill_source: Some(skill_source.to_string()),
                });
            }
        }

        // Pass 3 — rewrite the body: replace ONLY the activated skills'
        // reference spans with their `[display]` markers, keeping unknown /
        // disabled / command refs as the original link text (fail soft and
        // ADR-004 command non-execution). Runs left-to-right over the
        // original text using the parser's spans. Every span of an activated
        // skill is replaced (the duplicate references too), so re-parsing the
        // body yields no trigger-link (idempotent).
        let mut body = String::with_capacity(text.len());
        let mut cursor = 0;
        for reference in parsed.references.iter() {
            let activated = reference.kind == mirri_core::InlineReferenceKind::Skill
                && catalog
                    .resolve_by_skill_md_path(&reference.target)
                    .is_some_and(|s| activations.iter().any(|(_, n)| n == &s.name));
            if !activated {
                continue; // unknown / disabled / command — keep verbatim
            }
            body.push_str(&text[cursor..reference.start]);
            body.push_str(&format!("[{}]", reference.display));
            cursor = reference.end;
        }
        body.push_str(&text[cursor..]);
        if let Some(ContentPart::Text { text }) = prompt.first_mut() {
            *text = body;
        }
        with_skills
    }

    /// `message.send` — append a user message to an agent's context memory
    /// and run one engine turn when an LLM provider is configured.
    ///
    /// The message is *not* recorded here: a `message_received` event is
    /// published for listeners in all cases, but the durable append happens
    /// only inside the engine loop ([`TurnFlow::run_one_turn`] re-appends
    /// its prompt). Without a configured provider the call fails with `no
    /// LLM provider configured for this agent` and leaves session state
    /// untouched — no memory mutation, no wire-log record, only the
    /// listener notification.
    ///
    /// Two execution modes (mirroring `mirri-server`, design §6.2):
    ///
    /// - [`MessageSendMode::Wait`] (the default): block until the turn ends
    ///   and return the full `TurnEnd` result.
    /// - [`MessageSendMode::Fire`]: spawn the turn on a background task,
    ///   return immediately with a `turn_id`, and stream progress through the
    ///   event channel. A failed turn publishes a `turn_failed` event.
    pub(super) async fn send_message_with_origin(
        &self,
        params: Value,
        origin: PromptOrigin,
        origin_wire: Value,
        display_text: Option<String>,
    ) -> Result<Value, EngineError> {
        #[derive(Debug, Deserialize)]
        struct SendMessageParams {
            #[serde(alias = "sessionId")]
            session_id: String,
            #[serde(default, alias = "agentId")]
            agent_id: Option<String>,
            /// The model the caller resolved for this prompt (the web sends
            /// the session's current model on every send — the common case
            /// is an idempotent no-op against the agent's scope config).
            /// Applied through the shared model-switch path exactly like
            /// `session.updateMeta`'s model field: validated, recorded to
            /// the wire log, and replayed on resume.
            #[serde(default, alias = "modelId")]
            model: Option<String>,
            // Dotted-protocol spelling: a plain text string. The TS client's
            // `prompt` alias sends `input` instead — an array of content
            // parts in either the canonical wire spelling or the v1 TS REST
            // spelling (see the `prompt_parts` module); exactly one of the
            // two must be present.
            #[serde(default)]
            text: Option<String>,
            #[serde(default)]
            input: Option<Vec<Value>>,
            #[serde(default)]
            mode: MessageSendMode,
        }
        #[derive(Debug, Deserialize, Clone, PartialEq)]
        #[serde(rename_all = "camelCase")]
        #[derive(Default)]
        enum MessageSendMode {
            Fire,
            #[default]
            Wait,
        }

        let params: SendMessageParams = serde_json::from_value(normalize_params(params))
            .map_err(|e| invalid_params(e.to_string()))?;

        let session = self
            .resolve_session(&params.session_id)
            .await
            .map_err(|_| not_found(format!("session not found: {}", params.session_id)))?;

        // Address the requested agent, or the session's first agent.
        let agent: Option<Arc<AgentScope>> = match &params.agent_id {
            Some(agent_id) => session.get_agent(&AgentId::new(agent_id)).await,
            None => match session.list_agents().await.into_iter().next() {
                Some(first) => session.get_agent(&first).await,
                None => None,
            },
        };
        let Some(agent) = agent else {
            return Err(not_found(format!(
                "session '{}' has no agents yet; create an agent before sending messages",
                params.session_id
            )));
        };

        // Apply the caller's model param before anything else touches the
        // agent config, so the LLM factory below resolves the requested
        // model — "this request is the new model" (spec 11 A9). An empty
        // string is treated as absent (the wire's "no preference" spelling).
        // One param-carrying send is one standard switch (spec 11 §2.2 /
        // ADR-002): the shared path `agent.setModel` walks — validate the
        // alias, set the live config, persist to the wire log so a resume
        // replays it, record the marker, publish the event. The switch is
        // session-level (A17): it always targets the session's MAIN agent,
        // even when the send itself addresses a named agent — and the one
        // illegal combination is a send that both addresses a subagent and
        // carries a model (spec 11 §2.4/R1, the `agent.setModel` subagent
        // target's same caliber): accepting it would announce a divider to
        // the new model while the addressed agent's turn still runs its
        // dispatch-time pinned model.
        if let Some(model) = params.model.filter(|model| !model.is_empty()) {
            let main = resolve_session_agent(&session, None).await.ok_or_else(|| {
                not_found(format!(
                    "session '{}' has no agents yet; create an agent before sending messages",
                    params.session_id
                ))
            })?;
            if let Some(agent_id) = params.agent_id.as_deref()
                && main.id().as_str() != agent_id
            {
                return Err(super::subagent_model_target_rejection(agent_id));
            }
            super::session_handlers::switch_session_model(self, &params.session_id, &main, &model)
                .await?;
        }

        // Phase 1 — prepare the user prompt. Recording is owned by the
        // engine loop: run_one_turn appends its prompt to memory and the
        // turn's wire-op records (`turn.prompt` / `context.append_message`)
        // durably store it. The no-provider path therefore never appends
        // here — it fails before any memory or wire-log mutation, so the
        // session state stays untouched and consistent with its (empty)
        // wire log — and the provider path lets the turn own the append;
        // the message must never land in memory twice.
        let mut prompt = match (params.text, params.input) {
            (Some(text), _) => vec![ContentPart::Text { text }],
            (None, Some(parts)) if !parts.is_empty() => self.normalize_input_parts(parts)?,
            (None, Some(_)) => {
                return Err(invalid_params("input must not be empty"));
            }
            (None, None) => {
                return Err(invalid_params(
                    "message.send requires either 'text' or 'input'",
                ));
            }
        };
        let user_text = display_text.unwrap_or_else(|| prompt_text(&prompt));
        let memory = agent.context_memory();

        // Phase 1.5 — SDR-004 lazy physical creation + lastPrompt: the
        // first (and every subsequent) user input materializes the session
        // (create wrote nothing to disk) and stamps the last prompt into
        // the scope-layer metadata — the most recent user input REGARDLESS
        // of the turn's outcome ("the conversation failed" is not empty: a
        // turn that fails below still leaves a materialized, non-empty
        // session). The scope-layer stamp is what the persistence
        // write-through below and the index row share (ADR-009 — a
        // load-mutate-save cycle no longer erases it). The recency bump
        // keeps the listing's `updated_at` order honest (the TS sidebar
        // sorts on it). Runs BEFORE the no-provider check on purpose — a
        // send whose turn fails must still count as conversed.
        session
            .update_metadata(|metadata| metadata.last_prompt = Some(user_text.clone()))
            .await;
        // TS `updatePromptMetadata` parity for the auto-title is GONE here
        // (spec 09 §2.1 timing contract): the dispatch-time rule-based
        // derivation no longer runs on the LLM path — the title must stay
        // placeholder until the turn settles (A9); the out-of-band side
        // call (ADR-020) or the turn-end fallback names it later. The ONLY
        // dispatch-time derivation left is the no-provider branch below
        // (spec 09 §2.3 — no turn exists there).
        let (owner_manager, _owning_workspace) = self
            .resolve_session_owner(&params.session_id)
            .map_err(|_| not_found(format!("session not found: {}", params.session_id)))?;
        let session_id = SessionId::new(&params.session_id);
        let materialized = owner_manager
            .materialize_session(&session_id)
            .await
            .map_err(session_error)?;
        if !materialized {
            // Already materialized (an earlier send, an eager create, a
            // resume): persist the recency bump — the fresh path's
            // materialize wrote the state above, this path would
            // otherwise leave the disk stale.
            let metadata = session.metadata().await;
            owner_manager
                .persistence()
                .save_state(&session_id, &metadata)
                .await
                .map_err(session_error)?;
        }
        let metadata = session.metadata().await;
        self.index_note_last_prompt(&session_id, &metadata, &user_text);

        // Phase 2 — resolve the session's event plumbing early (the turn
        // wiring below and the interaction kernel consume the gates). The
        // MessageReceived publish itself is DEFERRED past the write-side
        // commit fence (F1, session-consistency batch 2 review): a rejected
        // send must never have told the UI "message received" — the phantom
        // message a fenced client would render before the conflict error
        // arrives. The publish now sits immediately after the fence passes
        // (still before the turn's first record_op's user-visible effects
        // and before TurnStarted), so ordering semantics are preserved for
        // every allowed send.
        let artifacts = self
            .resolve_session_artifacts(&params.session_id)
            .map_err(|_| not_found(format!("session not found: {}", params.session_id)))?;

        // Phase 3 — run the engine loop, but only when an LLM provider is
        // configured for this agent. The check runs before any memory
        // mutation or wire-log record so a no-provider error leaves the
        // session state untouched and consistent with its (empty) wire log.
        // The no-provider branch stays inline rather than inside
        // `resolve_turn_llm` because it carries the dispatch-time title
        // derivation (spec 09 §2.3), which needs the user text; the binding
        // itself feeds the downstream side-title captures.
        let Some(llm_factory) = &self.llm_factory else {
            // No provider → no turn: the message still counts as conversed
            // (SDR-004) and the session still gets its fallback title at
            // dispatch time (spec 09 §2.3 — the only path where the
            // rule-based derivation stays at send time).
            if let Some(title) = session
                .meta_handle()
                .set_auto_title(user_text.clone())
                .await
            {
                self.publish_session_event(
                    &params.session_id,
                    SessionEvent::SessionMetaUpdated {
                        title: Some(title),
                        patch: None,
                    },
                );
                // No turn ever settles on this error path, so the branch
                // must persist the derived title itself (review I-1): the
                // materialize above wrote the PRE-title state and the
                // index row was upserted from it — re-write both so a
                // restart and the listing show the derived value, not the
                // placeholder.
                let metadata = session.metadata().await;
                owner_manager
                    .persistence()
                    .save_state(&session_id, &metadata)
                    .await
                    .map_err(session_error)?;
                self.index_upsert_live(&session_id, &metadata);
            }
            return Err(EngineError::Internal(
                "no LLM provider configured for this agent".to_string(),
            ));
        };
        let llm_resolver = self.resolve_turn_llm(&agent)?;
        let agent_id = agent.id().as_str().to_string();

        // Phase 2.5 — SDR-005 / US3.6 inline skill references: when the
        // message is real user input carrying activatable inline reference
        // links, resolve and inject a `<kimi-skill-loaded>` activation wrap
        // for each unique referenced skill (in appearance order) BEFORE the
        // body prompt, per ADR-004's "N activation wraps + 1 body message"
        // turn sequence. Each resolved wrap becomes its own user message with
        // a `skill_activation` origin (trigger `user-inline`) fed to memory,
        // its own durable `skill.activate` fact + `context.append_message`
        // wire record, and its own `event.skill.activated` announcement —
        // exactly mirroring the slash path's fact-then-message ordering. The
        // body prompt has its reference links replaced by bare `[display]`
        // markers (the same idempotent body the TS parser produces). Unknown
        // targets / disabled skills / command refs fail soft: the body keeps
        // the original link text and no activation happens.
        let turn_recorder = turn_recorder_for(&session, &agent_id).await;
        // The admission stamp (ADR-006): the deduped activated-skill list the
        // injection just resolved. It is attached to the body message at BOTH
        // persistence sites below — the durable wire row (Phase 3b) and the
        // live in-memory append (run_one_turn's user-message append, threaded
        // via the with_skills parameter).
        let admission_with_skills = self
            .maybe_inject_inline_skill_activations(
                &session,
                &params.session_id,
                &origin,
                &mut prompt,
                &memory,
                &turn_recorder,
            )
            .await;

        let agent_tools = agent.tool_manager();
        let model_id = agent
            .config()
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .model_id
            .clone();

        // Get-or-create this agent's turn flow (brief map lock only).
        let flow_handle = self.get_or_create_turn_flow(&params.session_id, &agent_id, &model_id);

        // Wire the structured agent-status publisher (CR4) before the
        // turn's hooks start firing — the phase transitions below
        // publish `event.agent.status.updated` from here on.
        self.attach_status_publisher(&params.session_id, &agent, &flow_handle);

        // Feed the agent's live usage accumulator from the turn's steps
        // (F5, P7c): `agent.getUsage` and the session view's usage report
        // live totals without a resume. The sink is the agent's own service
        // Arc — re-setting it on a later turn is an idempotent overwrite.
        // The by_model key is the agent's model alias (TS parity, N4).
        flow_handle
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .set_usage_sink(agent.usage_service());
        flow_handle
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .set_usage_model_key(agent.model_id());
        // Wire the session's goal manager into the turn flow for usage
        // accounting (F11): each step records the goal's running totals
        // (in memory and as a usage-only `goal.update` op) and enforces
        // budget exhaustion at the step level. Accounting-only — the
        // goal-continuation layer (`should_continue_after_stop`) stays
        // off on every engine turn path (the subagent host wires the
        // same accounting-only slot for its child flows).
        flow_handle
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .set_goal_usage_accounting(agent.goal());
        // Fork-chain cache routing (fork-cache-key): the session's persisted
        // `cacheRootId` (the fork chain root, minted at fork time) rides every
        // turn's provider request as the prompt-cache key. `None` (fresh or
        // legacy sessions) omits the provider's cache-routing field — the
        // flow defaults to absent and the re-set is idempotent.
        let session_metadata = session.metadata().await;
        flow_handle
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .set_cache_root_id(session_metadata.cache_root_id);

        if agent_id == DEFAULT_MAIN_AGENT_ID {
            // Item I — mount the plugin `sessionStart` injector on the MAIN
            // agent only (the TS `AgentPluginService` self-gates on
            // `agentId === 'main'`). The data crosses as plain values from
            // the plugin manager (zero-IO contract 6); the injector renders
            // one `<plugin_session_start>` reminder per session — the
            // first-turn injection, deduped against the recorded history.
            // The reload-driven "append a superseded reminder" path has no
            // rust equivalent yet (item C4 does not refresh live sessions);
            // the divergence is recorded in the parity plan.
            let session_starts = self
                .plugin_manager
                .as_ref()
                .map(|pm| pm.enabled_session_starts())
                .unwrap_or_default();
            flow_handle
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .set_plugin_session_start_injector((!session_starts.is_empty()).then(|| {
                    Arc::new(mirri_core::PluginSessionStartInjector::new(
                        session_starts,
                        session.skill_catalog(),
                    ))
                }));
        }

        // Wire the session's external-command hook engine into the turn flow,
        // so user-configured hooks fire at the loop's lifecycle points.
        let agent_hook_engine = agent.hook_engine();
        let external_hook_env = {
            let mut env = HashMap::new();
            env.insert("MIRRI_SESSION_ID".to_string(), params.session_id.clone());
            env.insert("MIRRI_AGENT_ID".to_string(), agent_id.clone());
            env
        };

        // Wire the agent's permission manager and the session's reverse-RPC
        // callback into the turn flow so `approval.ask` and `question.ask`
        // can reach the client. The session artifacts' callback (stamped by
        // a serve transport at subscribe — `ws_rpc.rs` sets it per connection)
        // takes precedence, so each serve connection routes its own asks;
        // the engine-level callback (registered through
        // `EngineAdapter::on_reverse_rpc` — the ACP / NAPI late-registration
        // path) is the fallback.
        let permission_manager = agent.permission_manager();
        let reverse_rpc_callback = artifacts
            .reverse_rpc_callback
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .or_else(|| {
                self.reverse_rpc_callback
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone()
            });

        // ADR-009 (audit P0-1): mirror the callback this turn dispatches on
        // into the agent's live config, so a subagent spawned mid-turn
        // inherits the serve transport's per-connection callback (installed
        // into the session artifacts at subscribe — later than the agent's
        // create-time snapshot).
        mirror_reverse_rpc_callback_into_agent_config(&agent, &reverse_rpc_callback);

        // Phase 3b — persist the turn's opening to the session's wire log
        // (the same `logRecord` pattern the TS kernel applies in
        // `TurnFlow.prompt`): the `turn.prompt` record brackets the turn and
        // the `context.append_message` record durably stores the user
        // message, so `session.resume` can rebuild this prompt later.
        //
        // Large inline `data:` URIs are offloaded to `blobref:` first (the
        // session's content-addressed blob store) — the wire log stays lean
        // while context memory keeps the full inline form the providers
        // consume. Resume rehydrates the blobrefs back to data URIs (the
        // existing `reconstruct_context_memory_with_blobs` pass). The
        // session dir resolves through the routing seam (W3) so the blob
        // store of a non-default workspace's session is used.
        let blob_service = AgentBlobService::new(
            self.resolve_session_manager(&params.session_id)
                .map_err(|_| not_found(format!("session not found: {}", params.session_id)))?
                .persistence()
                .session_dir(&SessionId::new(&params.session_id)),
        );
        // The archive paths this store mints are the recovery handles the
        // spec hands to agents and subagents — admit the store into the
        // session sandbox so they stay readable (see
        // `admit_blob_store_into_sandbox`).
        admit_blob_store_into_sandbox(&session, &blob_service);
        // Multimodal spec E2: archive the prompt's media originals
        // (content-addressed blob store) and stamp the archive path into
        // the part's hidden `id` channel — the recovery handle consumed
        // by capability-gap placeholders and subagent delegation. Runs
        // before the wire offload so the wire record carries the handle
        // too (resume rehydrates it from context.jsonl).
        mirri_core::media_ingest::ingest_prompt_media(&blob_service, &mut prompt);
        let prompt_wire =
            serde_json::to_value(blob_service.offload_parts(&prompt)).unwrap_or_else(|_| json!([]));

        // Phase 3c — the write-side commit fence (session-consistency
        // batch 2, ADR-002, behind the `session-consistency-follow` flag):
        // the LAST check before the user message is durably recorded. See
        // [`journal_tail_verdict_fresh`] for the full verdict semantics
        // (main journal only, 宁拦勿写).
        //
        // Rejection happens BEFORE the first `record_op` below, so the
        // rejected send leaves ZERO bytes on the disk (ADR-002: the
        // client re-fills the message; convergence is a session resume).
        // Note the materialize/last-prompt persistence above already
        // wrote `state.json` — that is session metadata, not journal
        // content, and its concurrent-rewrite concern is ADR-003's
        // (three-way merge), not this fence's.
        if self
            .flag_registry()
            .enabled(mirri_core::SESSION_CONSISTENCY_FOLLOW_FLAG)
            && !journal_tail_verdict_fresh(
                self.resolve_session_manager(&params.session_id)
                    .ok()
                    .as_ref(),
                &params.session_id,
                session.wire_log_known_size(),
            )
        {
            return Err(engine_error(CoreEngineError::SessionConflict {
                session_id: params.session_id.clone(),
            }));
        }

        // The fence passed: NOW notify listeners so the UI reflects the
        // message while the (potentially slow) engine turn runs (F1 — moved
        // from Phase 2's position before the fence: a fenced send must leave
        // no phantom "message received" on the client). Ordering is
        // preserved relative to the turn's own lifecycle events: this still
        // precedes TurnStarted and every chunk/turn event the flow below
        // publishes.
        artifacts.event_bus.publish(SessionEvent::MessageReceived {
            agent_id: agent.id().as_str().to_string(),
            role: "user".to_string(),
            content_summary: summarize(&user_text, 120),
        });

        // The turn's journal: the main agent's records land in the session
        // log, any other agent's in its own journal (see
        // [`turn_recorder_for`]) — loop events and step usage emitted by the
        // engine ride the same recorder.
        turn_recorder
            .record_op(WireOp::TurnPrompt {
                input: prompt_wire.clone(),
                origin: origin_wire.clone(),
            })
            .await;
        // The body row carries the ADR-006 admission stamp when inline
        // skills activated (deduped, trigger user-inline) — sibling of
        // origin, matching the TS wire shape. The wrap rows above never
        // carry it: only the rewritten body admits the list.
        let body_message = if admission_with_skills.is_empty() {
            json!({
                "role": "user",
                "content": prompt_wire.clone(),
                "origin": origin_wire.clone(),
            })
        } else {
            json!({
                "role": "user",
                "content": prompt_wire.clone(),
                "origin": origin_wire.clone(),
                "with_skills": admission_with_skills,
            })
        };
        turn_recorder
            .record_op(WireOp::ContextAppendMessage {
                message: body_message,
            })
            .await;
        let wire_recorder: Option<Arc<dyn WireOpRecorder>> = Some(turn_recorder.clone());

        // The mid-turn abort guard (invariant 2, ③): built once, shared by
        // the wait and fire arms — the same journal-tail verdict the fence
        // above applied at submit time, re-evaluated before each step and
        // each tool batch while the turn runs (收口 the check→record
        // residual window: a foreign write landing between the submit
        // fence and any later tool boundary still aborts the turn).
        let turn_abort_guard = turn_abort_guard_for(
            self.flag_registry(),
            self.resolve_session_manager(&params.session_id).ok(),
            &params.session_id,
            &session,
        );

        // Phase 4 — run the turn, streaming assistant text deltas onto the
        // session's event bus as `agent_message_chunk` events. The interaction
        // kernel (B4-a) binds the turn flow to the session's HITL gates so
        // the embedded `approval.decide` / `question.answer` /
        // `interaction.*` RPCs can see and resolve the ask flows' pending
        // requests (the reverse-RPC channel stays the client's primary path —
        // first resolution wins).
        let interaction_kernel = Some(Arc::new(
            InteractionKernel::new(
                Arc::clone(&artifacts.approval_gate),
                Arc::clone(&artifacts.question_gate),
            )
            // Plan D3: the turn flow's hooks read the grant sink off the kernel
            // (the reverse-RPC reply branch) — the session's shared sink.
            .with_grant_sink(session_grant_sink(&session)),
        ));
        match params.mode {
            MessageSendMode::Wait => {
                // Mint the client-facing turn id (`turn-<uuid>`, the
                // `TurnFailed` convention) up front: the engine's numeric
                // per-flow id is only known after the turn runs, and the
                // turn events need a stable id to bracket the engine loop.
                let turn_id = format!("turn-{}", uuid::Uuid::new_v4());
                // spec 09 / SDR-016: watch for the turn's first LLM
                // response landing and hand the naming question to the
                // side call right then — a long multi-step turn must not
                // keep the placeholder title until it settles. The watch
                // filters by this dispatch's turn id, so subagent / cron
                // / task turns never fire it.
                if agent_id == DEFAULT_MAIN_AGENT_ID {
                    spawn_in_turn_title_watch(
                        SideTitleContext {
                            session: Arc::clone(&session),
                            agent: Arc::clone(&agent),
                            llm_factory: llm_factory.clone(),
                            registry: Arc::clone(&self.registry),
                            index: self.session_index(),
                            event_bus: artifacts.event_bus.clone(),
                            user_text: user_text.clone(),
                            phase: SideTitlePhase::InTurn,
                        },
                        turn_id.clone(),
                    );
                }
                let chunk_callback = live_chunk_callback(
                    &artifacts.event_bus,
                    &agent_id,
                    &params.session_id,
                    Some(turn_id.clone()),
                );
                let cancel_token = CancelToken::new();
                // Register the turn for cancellation (the `message.abort`
                // path): wait-mode turns run inline on this future, so only
                // the cooperative cancel token is registered — aborting
                // cancels it and the turn settles below with the aborted
                // stop reason (a hard task abort would strand the turn's
                // terminal wire records, so the wait arm always closes its
                // own bracket).
                let in_flight_key = format!("{}:{}:{}", params.session_id, agent_id, turn_id);
                self.in_flight_turns
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(
                        in_flight_key.clone(),
                        InFlightTurn::Wait {
                            cancel: cancel_token.clone(),
                        },
                    );
                // Whatever the outcome — success, error, or a panic — the
                // registration must not outlive the turn, or a later
                // `message.abort` would cancel a dead token.
                let _wait_turn_registration = WaitTurnGuard {
                    turns: Arc::clone(&self.in_flight_turns),
                    key: in_flight_key,
                };
                artifacts.event_bus.publish(SessionEvent::TurnStarted {
                    agent_id: agent_id.clone(),
                    session_id: params.session_id.clone(),
                    turn_id: turn_id.clone(),
                });
                let turn_end = run_engine_turn(
                    flow_handle,
                    agent_hook_engine,
                    external_hook_env,
                    Arc::clone(self.flag_registry()),
                    agent_tools,
                    memory,
                    // The switch-aware resolver re-reads the agent's live
                    // config per loop step, so a mid-turn model switch takes
                    // effect on the next LLM call (the dispatch-time
                    // fail-fast check above already validates the config).
                    llm_resolver,
                    prompt,
                    origin.clone(),
                    // The admission stamp resolved above rides the body
                    // into live memory (ADR-006) — None for steers.
                    Some(admission_with_skills),
                    Some(chunk_callback),
                    turn_id.clone(),
                    wire_recorder,
                    permission_manager,
                    reverse_rpc_callback,
                    session.tasks(),
                    interaction_kernel,
                    cancel_token.clone(),
                    self.turn_stall_timeout,
                    self.turn_backstop_timeout,
                    turn_abort_guard,
                    (params.session_id.clone(), agent_id.clone()),
                )
                .await;
                let turn_end = match turn_end {
                    Ok(turn_end) => turn_end,
                    Err(error) => {
                        // The first user turn failed without a model-set
                        // title — the rule-based fallback still applies
                        // (spec 09 / ADR-017 §6: completed/failed/cancelled
                        // all settle the naming question).
                        if agent_id == DEFAULT_MAIN_AGENT_ID {
                            let _ = apply_fallback_title(
                                &artifacts.event_bus,
                                &session.meta_handle(),
                                &user_text,
                            )
                            .await;
                        }
                        // A failed turn is still conversation activity
                        // (ADR-009: completed/failed both settle) — refresh
                        // the session's recency before the error propagates.
                        note_turn_settled(self.session_index(), &self.registry, &session).await;
                        // Goal settlement (TS settleAbnormalTurn parity): a
                        // failed turn must not leave an Active goal dangling.
                        // The error text is bound first — the settlement
                        // borrows it. For engine failures the raw engine
                        // error's own text rides the reason (the class
                        // prefixes like "Provider error: " stay on the wire
                        // error, mirroring what `engine_error` reports);
                        // bridge-level failures (panic, timeout) use the
                        // human-readable `message()` rendering.
                        let error_text = match &error {
                            EngineTurnError::Engine(core_error) => core_error.to_string(),
                            other => other.message(),
                        };
                        let _ = settle_goal_after_turn(
                            &self.registry,
                            &session,
                            TurnOutcome::Failed(&error_text),
                        )
                        .await;
                        // Goal continuation gating (TS handleTurnEnded
                        // parity): a failed turn settles first, then the
                        // chain decision runs (the budget block can still
                        // fire here — a turn may consume the last budgeted
                        // turn and then fail).
                        maybe_launch_goal_chain(
                            &self.registry,
                            &self.goal_chain_states,
                            &self.turn_flows,
                            &session,
                            &agent_id,
                            &self.goal_chain_resources(),
                        )
                        .await;
                        return Err(match error {
                            EngineTurnError::Engine(core_error) => engine_error(core_error),
                            other => EngineError::Internal(other.message()),
                        });
                    }
                };

                // A cancelled wait turn reports `Aborted` even when the
                // underlying provider stream ended cleanly after observing
                // the token: a cancel-aware stream truncates without its
                // `Done` event and collects as a clean `EndTurn`, but the
                // awaiting client cancelled and must hear the turn ended
                // because of that — never a phantom clean turn.
                let turn_end = coerce_aborted_if_cancelled(turn_end, &cancel_token);

                tracing::info!(
                    session_id = %params.session_id,
                    agent_id = %agent_id,
                    turn_id = turn_end.turn_id,
                    stop_reason = ?turn_end.stop_reason,
                    steps = turn_end.steps,
                    "engine turn completed"
                );

                // Phase 5 — persist the turn's terminal record (TS kernel
                // parity: `turn.ended` closes the bracket `turn.prompt`
                // opened, so replay tooling sees the turn boundary) on the
                // turn's own journal.
                turn_recorder
                    .record_op(WireOp::TurnEnded {
                        turn_id: turn_end.turn_id,
                        reason: turn_stop_reason_wire(&turn_end.stop_reason).to_string(),
                        error: None,
                        duration_ms: Some(turn_end.duration_ms),
                    })
                    .await;

                // spec 09 / ADR-020 + SDR-016: the settle-site retry is not
                // dispatched here — the in-turn watch spawned at dispatch
                // time owns both attempts and runs them in sequence (the
                // retry fires when that watch sees this turn's
                // `TurnCompleted`). Spawning a second task here would race
                // the in-turn call for the one-shot claim, and whichever
                // won would decide whether the naming question got its
                // retry at all.

                // ADR-009 turn settle: the turn ended — refresh the session's
                // recency and persist it (state.json + index write-through).
                // Settled BEFORE the terminal event publish so no await
                // separates the publish from the RPC response below — a
                // settle in between lets the relay deliver `event.turn.ended`
                // ahead of the `message.send` ack, which clients scanning
                // for the response discard (the response frame must win
                // the race with the events it closes).
                note_turn_settled(self.session_index(), &self.registry, &session).await;

                // Success counterpart of `TurnFailed`: exactly one of the two
                // closes the turn bracket `TurnStarted` opened above.
                artifacts.event_bus.publish(SessionEvent::TurnCompleted {
                    agent_id: agent_id.clone(),
                    session_id: params.session_id.clone(),
                    turn_id,
                    stop_reason: turn_stop_reason_wire(&turn_end.stop_reason).to_string(),
                    wire_turn_id: turn_end.turn_id,
                    duration_ms: Some(turn_end.duration_ms),
                });

                // Goal settlement (TS settleAbnormalTurn parity). The
                // result is bound FIRST so the RPC ack is fully
                // constructed; the settlement's await then runs after the
                // response frame has won its race — the ADR-009 relay-race
                // rule forbids any await between the TurnCompleted publish
                // and the ack, so the goal.update must not precede the
                // already-built response.
                let result = to_result(MessageSendWaitResult {
                    ok: true,
                    session_id: params.session_id.clone(),
                    agent_id: agent.id().as_str().to_string(),
                    turn: turn_end_to_json(turn_end.clone()),
                });
                let _ = settle_goal_after_turn(
                    &self.registry,
                    &session,
                    turn_outcome(&turn_end.stop_reason),
                )
                .await;
                // Goal continuation gating (TS handleTurnEnded parity):
                // the goal survived the turn's own settlement — the chain
                // decision runs (a settled goal exits early inside). The
                // user-originated turn resets the burn guards first (the
                // human is steering again — the idle/continuation
                // counters restart from zero).
                reset_goal_chain_guards(&self.goal_chain_states, session.id().as_str());
                maybe_launch_goal_chain(
                    &self.registry,
                    &self.goal_chain_states,
                    &self.turn_flows,
                    &session,
                    &agent_id,
                    &self.goal_chain_resources(),
                )
                .await;
                result
            }
            MessageSendMode::Fire => {
                // The client-facing turn id is a UUID-backed string; the
                // engine assigns its own numeric per-flow id later.
                let turn_id = format!("turn-{}", uuid::Uuid::new_v4());
                // spec 09 / SDR-016: same in-turn watch as the wait arm —
                // see there for the rationale. Published TurnStarted
                // below already carries this id.
                if agent_id == DEFAULT_MAIN_AGENT_ID {
                    spawn_in_turn_title_watch(
                        SideTitleContext {
                            session: Arc::clone(&session),
                            agent: Arc::clone(&agent),
                            llm_factory: llm_factory.clone(),
                            registry: Arc::clone(&self.registry),
                            index: self.session_index(),
                            event_bus: artifacts.event_bus.clone(),
                            user_text: user_text.clone(),
                            phase: SideTitlePhase::InTurn,
                        },
                        turn_id.clone(),
                    );
                }
                // Announce the turn on the session's event bus before the
                // background task runs the engine loop, so subscribers see
                // `turn_started` as soon as `message.send` returns (the
                // `TurnCompleted` / `TurnFailed` that closes the bracket is
                // published from inside the task).
                artifacts.event_bus.publish(SessionEvent::TurnStarted {
                    agent_id: agent_id.clone(),
                    session_id: params.session_id.clone(),
                    turn_id: turn_id.clone(),
                });
                let turn_id_for_task = turn_id.clone();
                let session_id = params.session_id.clone();
                let task_agent_id = agent_id.clone();
                let task_user_text = user_text.clone();
                let event_bus = Arc::clone(&artifacts.event_bus);
                let task_session = Arc::clone(&session);
                // The turn's journal (the addressed agent's own for a
                // non-main agent — see [`turn_recorder_for`]), captured so
                // the spawned task closes the turn bracket on it.
                let task_recorder = turn_recorder.clone();
                // Track the task in the in-flight registry (keyed like
                // `mirri-server`'s `{session}:{agent}:{turn}`) so
                // `message.abort` can cancel it; the task removes its own
                // entry when it finishes.
                let in_flight_key = format!("{session_id}:{task_agent_id}:{turn_id}");
                let in_flight_key_for_task = in_flight_key.clone();
                let in_flight_turns_for_task = Arc::clone(&self.in_flight_turns);
                let in_flight_turns = Arc::clone(&self.in_flight_turns);
                // The turn-settle write-through needs the shared index and
                // workspace registry — captured so the spawned task (which
                // holds no `&EmbeddedServer`) can refresh the session's
                // recency when the turn settles (ADR-009).
                let settle_index = self.session_index();
                let settle_registry = Arc::clone(&self.registry);
                // The goal continuation decision runs after the fire turn's
                // settle — the spawned task holds no `&EmbeddedServer`, so
                // the chain-state and turn-flow maps travel with it as
                // cloned field Arcs (the busy probe needs no `self`).
                let goal_chain_states = Arc::clone(&self.goal_chain_states);
                let goal_chain_turn_flows = Arc::clone(&self.turn_flows);
                // The chain's injection resources, snapshotted before the
                // spawn (the task holds no `&EmbeddedServer` — the field
                // Arcs travel with it, the same stance as the settle
                // captures above).
                let goal_chain_resources = self.goal_chain_resources();
                let turn_flag_registry = Arc::clone(self.flag_registry());

                let cancel_token = CancelToken::new();
                let cancel_token_for_task = cancel_token.clone();
                // The settle-side handle: `run_engine_turn` consumes
                // `cancel_token_for_task` by value, but the fire arm's
                // coerce below needs the same source's stall marker at
                // settle time — a clone is another handle to the same
                // cancellation.
                let settle_cancel = cancel_token_for_task.clone();
                let stall_timeout = self.turn_stall_timeout;
                let backstop_timeout = self.turn_backstop_timeout;

                let handle = tokio::spawn(async move {
                    let outcome = run_engine_turn(
                        flow_handle,
                        agent_hook_engine,
                        external_hook_env,
                        turn_flag_registry,
                        agent_tools,
                        memory,
                        // Switch-aware resolver — see the wait path's comment.
                        llm_resolver,
                        prompt,
                        origin.clone(),
                        // Same stamp as the wait arm — the fire arm must
                        // stamp live memory identically (ADR-006).
                        Some(admission_with_skills),
                        Some(live_chunk_callback(
                            &event_bus,
                            &task_agent_id,
                            &session_id,
                            Some(turn_id_for_task.clone()),
                        )),
                        turn_id_for_task.clone(),
                        wire_recorder,
                        permission_manager,
                        reverse_rpc_callback,
                        task_session.tasks(),
                        interaction_kernel,
                        cancel_token_for_task,
                        stall_timeout,
                        backstop_timeout,
                        turn_abort_guard,
                        (session_id.clone(), task_agent_id.to_string()),
                    )
                    .await;

                    in_flight_turns_for_task
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .remove(&in_flight_key_for_task);

                    match &outcome {
                        Ok(turn_end) => {
                            // ADR-004 batch d: coerce before rendering — the
                            // fire arm is a settle path alongside the wait
                            // arm and the task/cron drivers; a watchdog kill
                            // must record `stalled`, never a user
                            // `cancelled`.
                            let turn_end =
                                &coerce_aborted_if_cancelled(turn_end.clone(), &settle_cancel);
                            // Persist the turn's terminal record, exactly as
                            // the wait path does after a successful turn —
                            // on the turn's own journal.
                            task_recorder
                                .record_op(WireOp::TurnEnded {
                                    turn_id: turn_end.turn_id,
                                    reason: turn_stop_reason_wire(&turn_end.stop_reason)
                                        .to_string(),
                                    error: None,
                                    duration_ms: Some(turn_end.duration_ms),
                                })
                                .await;
                            // Success counterpart of TurnFailed below:
                            // exactly one of the two closes the turn bracket
                            // TurnStarted opened above.
                            event_bus.publish(SessionEvent::TurnCompleted {
                                agent_id: task_agent_id.clone(),
                                session_id: session_id.clone(),
                                turn_id: turn_id_for_task.clone(),
                                stop_reason: turn_stop_reason_wire(&turn_end.stop_reason)
                                    .to_string(),
                                wire_turn_id: turn_end.turn_id,
                                duration_ms: Some(turn_end.duration_ms),
                            });
                            // spec 09 / ADR-020 + SDR-016: the settle-site
                            // retry is not dispatched here — the in-turn
                            // watch spawned at dispatch time owns both
                            // attempts and runs them in sequence (see the
                            // wait arm's site for the rationale).
                            // ADR-009 turn settle: the turn ended — refresh
                            // the session's recency and persist it.
                            note_turn_settled(settle_index, &settle_registry, &task_session).await;
                            // Goal settlement (TS settleAbnormalTurn parity):
                            // an abnormally-ended fire turn must not leave an
                            // Active goal dangling. NOTE: on this worktree's
                            // HEAD the fire arm does not yet coerce the stop
                            // reason (Fix F, landed elsewhere) — the settle
                            // wiring below is final placement; the coerced
                            // semantics arrive with Fix F.
                            let _ = settle_goal_after_turn(
                                &settle_registry,
                                &task_session,
                                turn_outcome(&turn_end.stop_reason),
                            )
                            .await;
                            // Goal continuation gating (TS handleTurnEnded
                            // parity): the fire turn settled — the chain
                            // decision runs from the captured field Arcs.
                            // The user-originated turn resets the burn
                            // guards first (the human is steering again —
                            // the idle/continuation counters restart).
                            reset_goal_chain_guards(&goal_chain_states, task_session.id().as_str());
                            maybe_launch_goal_chain(
                                &settle_registry,
                                &goal_chain_states,
                                &goal_chain_turn_flows,
                                &task_session,
                                &task_agent_id,
                                &goal_chain_resources,
                            )
                            .await;
                        }
                        Err(error) => {
                            let error_message = error.message();
                            tracing::error!(
                                session_id = %session_id,
                                agent_id = %task_agent_id,
                                turn_id = %turn_id_for_task,
                                error = %error_message,
                                "engine turn failed (fire mode)"
                            );
                            // The turn will never complete — tell the
                            // session's subscribers so their UI can stop
                            // waiting.
                            event_bus.publish(SessionEvent::TurnFailed {
                                agent_id: task_agent_id.clone(),
                                turn_id: turn_id_for_task,
                                error: error_message,
                                error_code: error.domain_code().to_string(),
                                // The fire task died before a TurnEndResult —
                                // no numeric id was allocated for the client.
                                wire_turn_id: 0,
                            });
                            // A failed turn is still a settled user turn for
                            // the naming question (ADR-017 §6): the fallback
                            // runs — see the wait arm's site.
                            if task_agent_id == DEFAULT_MAIN_AGENT_ID {
                                let _ = apply_fallback_title(
                                    &event_bus,
                                    &task_session.meta_handle(),
                                    &task_user_text,
                                )
                                .await;
                            }
                            // A failed turn is still conversation activity
                            // (ADR-009) — settle before the task ends.
                            note_turn_settled(settle_index, &settle_registry, &task_session).await;
                            // Goal settlement (TS settleAbnormalTurn parity):
                            // a failed fire turn parks an Active goal as
                            // paused with the failure reason.
                            let error_text = match error {
                                EngineTurnError::Engine(core_error) => core_error.to_string(),
                                other => other.message(),
                            };
                            let _ = settle_goal_after_turn(
                                &settle_registry,
                                &task_session,
                                TurnOutcome::Failed(&error_text),
                            )
                            .await;
                            // Goal continuation gating (TS handleTurnEnded
                            // parity): even a failed fire turn runs the
                            // chain decision after its settle (the budget
                            // block can still fire).
                            maybe_launch_goal_chain(
                                &settle_registry,
                                &goal_chain_states,
                                &goal_chain_turn_flows,
                                &task_session,
                                &task_agent_id,
                                &goal_chain_resources,
                            )
                            .await;
                        }
                    }
                });
                {
                    let mut turns = in_flight_turns.lock().unwrap_or_else(|e| e.into_inner());
                    // A finished handle means the turn completed between
                    // spawn and here — drop it instead of keeping a stale
                    // in-flight entry.
                    turns.retain(|_, pending| {
                        !matches!(pending, InFlightTurn::Fire { handle, .. } if handle.is_finished())
                    });
                    turns.insert(
                        in_flight_key,
                        InFlightTurn::Fire {
                            handle,
                            cancel: cancel_token,
                        },
                    );
                }

                to_result(MessageSendFireResult {
                    ok: true,
                    session_id: params.session_id.clone(),
                    agent_id: agent.id().as_str().to_string(),
                    turn_id,
                })
            }
        }
    }

    /// `message.send` / `prompt` — the plain user-prompt face of the
    /// shared turn-driving body. See [`Self::send_message_with_origin`].
    pub(super) async fn dispatch_send_message(&self, params: Value) -> Result<Value, EngineError> {
        self.send_message_with_origin(params, PromptOrigin::User, json!({ "kind": "user" }), None)
            .await
    }

    // --- agent/turn domain (Phase 4.4) ------------------------------------

    /// `message.abort` — cancel in-flight engine turns of a session
    /// (mirrors `mirri-server`'s `message.abort`): a specific `turnId`
    /// cancels only that turn (unknown id → not found); omitting it
    /// cancels every in-flight turn of the session. Fire-mode turns are
    /// additionally hard-aborted (their task dies, and the `turn_failed`
    /// event below tells subscribers to stop waiting); wait-mode turns
    /// settle their own `message.send` response with the aborted stop
    /// reason. The blocking engine turn itself cannot be unwound
    /// mid-LLM-call; it stays bounded by [`ENGINE_TURN_STALL_TIMEOUT`]
    pub(super) async fn dispatch_cancel(&self, params: Value) -> Result<Value, EngineError> {
        #[derive(Debug, Deserialize)]
        struct CancelParams {
            #[serde(alias = "sessionId")]
            session_id: String,
            #[serde(default, alias = "turnId")]
            turn_id: Option<String>,
        }
        let params: CancelParams = serde_json::from_value(normalize_params(params))
            .map_err(|e| invalid_params(e.to_string()))?;

        self.resolve_session(&params.session_id)
            .await
            .map_err(|_| not_found(format!("session not found: {}", params.session_id)))?;

        self.cancel_in_flight_turns(
            &params.session_id,
            params.turn_id.as_deref(),
            "the user via message.abort",
        )
        .await?;
        ack()
    }

    /// Cancel in-flight turns of the session (all, or one `turn_id`) —
    /// the shared kernel of `message.abort` and the goal lifecycle
    /// handlers (`goal.pause` / `goal.cancel` freeze the machine by
    /// cancelling whatever is running). Wait entries only observe the
    /// cooperative token and settle themselves; fire entries are
    /// hard-aborted and settled here (the dead task cannot publish its
    /// own bracket close).
    pub(super) async fn cancel_in_flight_turns(
        &self,
        session_id: &str,
        turn_id: Option<&str>,
        cause: &str,
    ) -> Result<(), EngineError> {
        let session_prefix = format!("{session_id}:");
        let mut targeted: Vec<(String, InFlightTurn)> = Vec::new();
        {
            let mut turns = self
                .in_flight_turns
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let keys: Vec<String> = turns
                .keys()
                .filter(|key| {
                    key.starts_with(&session_prefix)
                        && turn_id.is_none_or(|turn_id| key.ends_with(&format!(":{turn_id}")))
                })
                .cloned()
                .collect();
            for key in keys {
                if let Some(inflight) = turns.remove(&key) {
                    targeted.push((key, inflight));
                }
            }
        }

        if targeted.is_empty() {
            if let Some(turn_id) = turn_id {
                return Err(not_found(format!(
                    "no in-flight turn '{turn_id}' in session '{session_id}'"
                )));
            }
            // Nothing in flight and no specific turn requested: idempotent.
            return Ok(());
        }

        // A fire-mode abort settles its turn HERE (the aborted task never
        // runs its own settle) — resolve the session once for the loop so
        // the turn-failed publish can be followed by the ADR-009 write
        // through.
        let aborted_session = self.resolve_session(session_id).await.ok();

        // TS parity (`pauseOnInterrupt`, agent/turn/index.ts:446): any
        // turn-level cancellation parks an active goal as `paused`
        // (resumable) — an interrupted turn must stop the goal loop, or
        // the next stranded notification/continuation restarts it within
        // one driver tick (observed live: 63ms). No-op when the goal is
        // missing or already non-active, so the `goal.pause` (already
        // paused) and `goal.cancel` (cleared) callers never
        // re-transition. Ordered BEFORE the cancel tokens fire so the
        // wire reports the pause ahead of the freeze's turn endings (the
        // same ordering `goal.pause` established).
        if let Some(session) = &aborted_session {
            let goal = session.goal();
            if goal
                .get()
                .is_some_and(|goal| goal.status == GoalStatus::Active)
            {
                // Write-before-mutate: the goal.update op lands before the
                // in-memory status change.
                session
                    .record_op(WireOp::GoalUpdate {
                        goal_id: None,
                        status: Some(goal_status_wire(GoalStatus::Paused).to_string()),
                        reason: Some("Paused after interruption".to_string()),
                        turns_used: None,
                        tokens_used: None,
                        wall_clock_ms: None,
                        budget_limits: None,
                        actor: Some("user".to_string()),
                    })
                    .await;
                goal.update_with_reason(
                    GoalStatus::Paused,
                    Some("Paused after interruption".to_string()),
                );
                if let Ok(artifacts) = self.resolve_session_artifacts(session_id) {
                    let snapshot = goal
                        .get()
                        .map(|current| goal_snapshot_json(&current, &goal))
                        .unwrap_or(Value::Null);
                    artifacts.event_bus.publish(SessionEvent::GoalUpdated {
                        snapshot,
                        change: Some(json!({
                            "kind": "lifecycle",
                            "status": "paused",
                            "reason": "Paused after interruption",
                            "actor": "user",
                        })),
                    });
                }
            }
        }

        for (key, inflight) in targeted {
            // Both modes take the cooperative cancel (checked at loop
            // tops, selected on in the HITL gates, observed by cancel-aware
            // provider streams). Only fire mode additionally hard-aborts
            // its task — and only fire mode needs the aborting side to
            // publish the `TurnFailed` that closes the turn bracket: a
            // wait-mode turn runs inline on its `message.send` future and
            // closes its own bracket with `TurnCompleted`, settling the
            // response with the aborted stop reason.
            let fire_handle = match &inflight {
                InFlightTurn::Fire { handle, .. } => Some(handle),
                InFlightTurn::Wait { .. } => None,
            };
            inflight.cancel_token().cancel();
            if let Some(handle) = fire_handle {
                handle.abort();
            }
            let mut parts = key.splitn(3, ':');
            let _session_id = parts.next();
            let agent_id = parts.next().unwrap_or_default();
            let turn_id = parts.next().unwrap_or_default();
            tracing::info!(
                session_id = %session_id,
                agent_id = %agent_id,
                turn_id = %turn_id,
                cause = %cause,
                "cancelled in-flight turn"
            );
            if !agent_id.is_empty()
                && !turn_id.is_empty()
                && matches!(inflight, InFlightTurn::Fire { .. })
                && let Ok(artifacts) = self.resolve_session_artifacts(session_id)
            {
                // ADR-009: the aborted turn is still conversation activity —
                // refresh and persist the session's recency (the wait-mode
                // abort settles inline on its own `message.send` future; the
                // fire-mode abort closes the bracket here). Settled BEFORE
                // the terminal event publish: no await may separate the
                // publish from the `message.abort` ack below, or the relay
                // delivers `event.turn.ended` ahead of the response and
                // response-scanning clients discard the event.
                if let Some(session) = &aborted_session {
                    // spec 09 / ADR-017 §6 (review F-1b): the hard-aborted
                    // fire turn never reaches its settle sites, so THIS side
                    // settles the naming question — the A10 fallback runs
                    // lock-free against the session metadata (the tokio
                    // RwLock the atomic naming guard lives behind).
                    // `last_prompt` was stamped at dispatch time (SDR-004),
                    // before the turn ever started. Wait-mode turns settle
                    // their own fallback inline and need nothing here.
                    if agent_id == DEFAULT_MAIN_AGENT_ID
                        && let Some(user_text) = session.metadata().await.last_prompt.clone()
                    {
                        let _ = apply_fallback_title(
                            &artifacts.event_bus,
                            &session.meta_handle(),
                            &user_text,
                        )
                        .await;
                    }
                    note_turn_settled(self.session_index(), &self.registry, session).await;
                }
                artifacts.event_bus.publish(SessionEvent::TurnFailed {
                    agent_id: agent_id.to_string(),
                    turn_id: turn_id.to_string(),
                    error: format!("turn aborted by {cause}"),
                    error_code: "abort.user_cancelled".to_string(),
                    wire_turn_id: 0,
                });
            }
        }

        Ok(())
    }

    /// `message.steer` — steer the addressed agent with new content.
    ///
    /// TS `TurnFlow.steer` parity (T3): a steer arriving while a turn is
    /// running parks in the flow's shared steer buffer (flushed by the
    /// turn's next step — the deadlock-safe handle path from P7b, since the
    /// flow's mutex is held for the whole turn by the run loop); a steer
    /// arriving on an idle agent **launches a new turn** with the steer
    /// content as its prompt, so the user gets a response instead of a
    /// silently parked message (F-P7-13). Parking is durable: the
    /// `turn.steer` record lands in the session's wire log before the
    /// buffer push, and a resume restores the steers that never reached
    /// context (F-P7-11/12).
    pub(super) async fn dispatch_steer(&self, params: Value) -> Result<Value, EngineError> {
        #[derive(Debug, Deserialize)]
        struct SteerParams {
            #[serde(alias = "sessionId")]
            session_id: String,
            #[serde(default, alias = "agentId")]
            agent_id: Option<String>,
            #[serde(default)]
            input: Option<Vec<Value>>,
            #[serde(default)]
            text: Option<String>,
        }
        let params: SteerParams = serde_json::from_value(normalize_params(params))
            .map_err(|e| invalid_params(e.to_string()))?;

        let session = self
            .resolve_session(&params.session_id)
            .await
            .map_err(|_| not_found(format!("session not found: {}", params.session_id)))?;
        let agent = resolve_session_agent(&session, params.agent_id.as_deref())
            .await
            .ok_or_else(|| {
                not_found(format!("session '{}' has no agents yet", params.session_id))
            })?;

        let content = match (params.input, params.text) {
            (Some(parts), _) if !parts.is_empty() => self.normalize_input_parts(parts)?,
            (_, Some(text)) => vec![ContentPart::Text { text }],
            (_, None) => return Err(invalid_params("steer requires 'input' or 'text'")),
        };

        let session_id = params.session_id.clone();
        let agent_id = agent.id().as_str().to_string();
        let model_id = agent
            .config()
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .model_id
            .clone();

        // Busy detection: the turn flow's mutex is held for the whole turn
        // by the engine loop (message.send wait/fire, the cron driver, and
        // background turns all go through `run_engine_turn`), so lock
        // contention is exactly "a turn of this agent is running". The
        // check never blocks, and the launch below re-acquires the lock
        // through the turn executor itself — the serialization contract
        // is preserved. A steer that races a just-starting turn simply
        // runs as its own serialized turn instead of parking.
        let flow_handle = self.get_or_create_turn_flow(&session_id, &agent_id, &model_id);
        let turn_running = flow_handle.try_lock().is_err();

        // The steer's wire record is session-scoped and the resume
        // restores only the main agent's buffer — record (and restore)
        // only steers addressed to the canonical main agent id. That id
        // is pinned on both sides this gate must agree with: the
        // creation side (`session.create` provisions
        // `DEFAULT_MAIN_AGENT_ID`) and the resume side (the buffer is
        // restored via `steer_buffer_for(…, DEFAULT_MAIN_AGENT_ID)`).
        // Keying on the id — never on `list_agents().first()`, whose
        // `HashMap` iteration order is seed-nondeterministic once
        // `session.registerAgent` makes the session multi-agent — keeps
        // the durable contract deterministic and stops subagent steers
        // from being misrecorded (and restored into the main buffer).
        // Subagent steers keep the pre-T3 behavior (parked, unrecorded).
        let is_main_agent = agent_id == DEFAULT_MAIN_AGENT_ID;

        if turn_running {
            // Park path — write-before-mutate: the `turn.steer` record
            // (blob-offloaded, the same pass `message.send` applies to
            // prompts) lands before the buffer push, so a crash between
            // the two never strands a parked-but-unrecorded steer.
            if is_main_agent {
                // The session dir resolves through the routing seam (W3) —
                // the steer record's blobs land in the owning workspace's
                // blob store.
                let session_dir = self
                    .resolve_session_manager(&session_id)
                    .ok()
                    .map(|manager| {
                        manager
                            .persistence()
                            .session_dir(&SessionId::new(&session_id))
                    });
                if let Some(session_dir) = session_dir {
                    let blob_service = AgentBlobService::new(session_dir);
                    // Same admission as the `message.send` path — a steer's
                    // blob-offloaded records mint the same recovery handles.
                    admit_blob_store_into_sandbox(&session, &blob_service);
                    let steer_wire = serde_json::to_value(blob_service.offload_parts(&content))
                        .unwrap_or_else(|_| json!([]));
                    session
                        .record_op(WireOp::TurnSteer {
                            input: steer_wire,
                            origin: json!({ "kind": "user" }),
                        })
                        .await;
                }
            }
            // Park straight into the flow's shared steer buffer — NOT
            // through `TurnFlow::steer`: the flow's mutex is held for the
            // whole turn by the run loop, so a mid-turn steer through the
            // flow would block until the turn ends (parking the content
            // one turn late) or deadlock a single-threaded runtime. The
            // buffer is the same handle the turn's `before_step` /
            // `should_continue_after_stop` hooks flush, so the steer lands
            // in the running turn's next LLM request; on a turn that ends
            // without consuming it (e.g. an abort), it stays resident for
            // the next turn's first step.
            let steer_buffer = self.steer_buffer_for(&session_id, &agent_id, &model_id);
            steer_buffer
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(content, PromptOrigin::User);
            return ack();
        }

        // Idle path (F-P7-13, TS `steer()` → `launch()`): the steer
        // becomes a fire-mode turn of its own, with the steer content as
        // its prompt. The turn's wire shape matches every other dispatched
        // turn (`turn.prompt` + `context.append_message` + `turn.ended`),
        // so a resume replays the steered conversation exactly.
        let Some(llm_factory) = &self.llm_factory else {
            return Err(EngineError::Internal(
                "no LLM provider configured for this agent".to_string(),
            ));
        };
        // The switch-aware resolver (same as the `message.send` path): the
        // agent's live config drives each resolution, so a mid-turn
        // `agent.setModel` takes effect on the steer-launched turn's next
        // step instead of the next turn.
        let llm_resolver: Arc<dyn mirri_core::LlmResolver> =
            Arc::new(ConfigResolvingLlm::new(agent.config(), llm_factory.clone()));
        // Fail fast on a broken provider config before the turn starts (the
        // pre-switch dispatch-time create_llm check); the built provider is
        // cached and reused by the loop's per-step resolves.
        llm_resolver.resolve().map_err(|reason| {
            EngineError::Internal(format!(
                "LLM provider creation failed for agent '{}': {reason}",
                agent_id
            ))
        })?;

        let artifacts = self
            .resolve_session_artifacts(&session_id)
            .map_err(|_| not_found(format!("session not found: {}", session_id)))?;

        // Notify listeners so the UI reflects the steer while the turn
        // runs (the same immediate echo `message.send` applies).
        artifacts.event_bus.publish(SessionEvent::MessageReceived {
            agent_id: agent_id.clone(),
            role: "user".to_string(),
            content_summary: summarize(&prompt_text(&content), 120),
        });

        // Persist the turn's opening (write-before-mutate, blob-offloaded)
        // — the same records a `message.send` turn writes. The session dir
        // resolves through the routing seam (W3): the steer-launched turn's
        // blobs land in the owning workspace's blob store.
        let session_dir = self
            .resolve_session_manager(&session_id)
            .ok()
            .map(|manager| {
                manager
                    .persistence()
                    .session_dir(&SessionId::new(&session_id))
            });
        // The turn's journal: the main agent's records land in the session
        // log, any other agent's in its own journal (see
        // [`turn_recorder_for`]) — loop events and step usage emitted by the
        // engine ride the same recorder.
        let turn_recorder = turn_recorder_for(&session, &agent_id).await;
        if let Some(session_dir) = session_dir {
            let blob_service = AgentBlobService::new(session_dir);
            // Same admission as the `message.send` path — a steer-turned
            // turn's media archives the same recovery handles.
            admit_blob_store_into_sandbox(&session, &blob_service);
            let prompt_wire = serde_json::to_value(blob_service.offload_parts(&content))
                .unwrap_or_else(|_| json!([]));
            turn_recorder
                .record_op(WireOp::TurnPrompt {
                    input: prompt_wire.clone(),
                    origin: json!({ "kind": "user" }),
                })
                .await;
            turn_recorder
                .record_op(WireOp::ContextAppendMessage {
                    message: json!({
                        "role": "user",
                        "content": prompt_wire,
                        "origin": { "kind": "user" },
                    }),
                })
                .await;
        }
        let wire_recorder: Option<Arc<dyn WireOpRecorder>> = Some(turn_recorder.clone());

        // Feed the agent's live usage accumulator (F5 parity with
        // `message.send`'s turn wiring); the by_model key is the agent's
        // model alias (TS parity, N4).
        //
        // The structured agent-status publisher (CR4) rides along — the
        // steer-launched turn publishes its phase transitions the same
        // way `message.send`'s does.
        self.attach_status_publisher(&session_id, &agent, &flow_handle);
        flow_handle
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .set_usage_sink(agent.usage_service());
        flow_handle
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .set_usage_model_key(agent.model_id());
        // Wire the session's goal manager into the turn flow for usage
        // accounting (F11): each step records the goal's running totals
        // (in memory and as a usage-only `goal.update` op) and enforces
        // budget exhaustion at the step level. Accounting-only — the
        // goal-continuation layer (`should_continue_after_stop`) stays
        // off on every engine turn path (the subagent host wires the
        // same accounting-only slot for its child flows).
        flow_handle
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .set_goal_usage_accounting(agent.goal());
        // Fork-chain cache routing (fork-cache-key): the same session
        // `cacheRootId` injection the message.send path applies — a
        // steer-launched turn must route its provider prompt-cache key at
        // the same fork-chain root.
        let session_metadata = session.metadata().await;
        flow_handle
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .set_cache_root_id(session_metadata.cache_root_id);

        let agent_hook_engine = agent.hook_engine();
        let mut external_hook_env = HashMap::new();
        external_hook_env.insert("MIRRI_SESSION_ID".to_string(), session_id.clone());
        external_hook_env.insert("MIRRI_AGENT_ID".to_string(), agent_id.clone());
        let agent_tools = agent.tool_manager();
        let permission_manager = agent.permission_manager();
        let reverse_rpc_callback = artifacts
            .reverse_rpc_callback
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .or_else(|| {
                self.reverse_rpc_callback
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone()
            });
        // ADR-009 (audit P0-1): mirror the callback this steer-launched turn
        // dispatches on into the agent's live config, so subagents spawned
        // mid-turn inherit the serve transport's per-connection callback.
        mirror_reverse_rpc_callback_into_agent_config(&agent, &reverse_rpc_callback);
        let interaction_kernel = Some(Arc::new(
            InteractionKernel::new(
                Arc::clone(&artifacts.approval_gate),
                Arc::clone(&artifacts.question_gate),
            )
            // Plan D3: grant sink wiring parity with the message-send path.
            .with_grant_sink(session_grant_sink(&session)),
        ));
        let memory = agent.context_memory();

        // The client-facing turn id and bracket events (the same
        // `turn-<uuid>` convention the message-send dispatch uses).
        let turn_id = format!("turn-{}", uuid::Uuid::new_v4());
        // spec 09 / SDR-016 (A11): an idle steer is the first user turn —
        // the in-turn watch fires the side call at its first response,
        // same as the message-send arms.
        if agent_id == DEFAULT_MAIN_AGENT_ID {
            spawn_in_turn_title_watch(
                SideTitleContext {
                    session: Arc::clone(&session),
                    agent: Arc::clone(&agent),
                    llm_factory: llm_factory.clone(),
                    registry: Arc::clone(&self.registry),
                    index: self.session_index(),
                    event_bus: artifacts.event_bus.clone(),
                    user_text: prompt_text(&content),
                    phase: SideTitlePhase::InTurn,
                },
                turn_id.clone(),
            );
        }
        artifacts.event_bus.publish(SessionEvent::TurnStarted {
            agent_id: agent_id.clone(),
            session_id: session_id.clone(),
            turn_id: turn_id.clone(),
        });

        let task_session = Arc::clone(&session);
        let task_agent_id = agent_id.clone();
        // The steer-launched turn is a user turn for the naming question
        // (A11): its failure fallback derives from the steer text.
        let task_user_text = prompt_text(&content);
        // The turn's journal (the addressed agent's own for a non-main
        // agent — see [`turn_recorder_for`]), captured so the spawned task
        // closes the turn bracket on it.
        let task_recorder = turn_recorder.clone();
        let turn_id_for_task = turn_id.clone();
        let event_bus = Arc::clone(&artifacts.event_bus);
        let in_flight_key = format!("{session_id}:{agent_id}:{turn_id}");
        let in_flight_key_for_task = in_flight_key.clone();
        let in_flight_turns_for_task = Arc::clone(&self.in_flight_turns);
        let in_flight_turns = Arc::clone(&self.in_flight_turns);
        // The turn-settle write-through needs the shared index and workspace
        // registry — captured so the spawned task (which holds no
        // `&EmbeddedServer`) can refresh the session's recency (ADR-009).
        let settle_index = self.session_index();
        let settle_registry = Arc::clone(&self.registry);
        // The goal continuation decision runs after the steer-launched
        // turn's settle — the spawned task holds no `&EmbeddedServer`, so
        // the chain-state and turn-flow maps travel with it as cloned
        // field Arcs (the busy probe needs no `self`).
        let goal_chain_states = Arc::clone(&self.goal_chain_states);
        let goal_chain_turn_flows = Arc::clone(&self.turn_flows);
        // The chain's injection resources, snapshotted before the spawn
        // (the task holds no `&EmbeddedServer` — the field Arcs travel
        // with it, the same stance as the settle captures above).
        let goal_chain_resources = self.goal_chain_resources();
        let turn_flag_registry = Arc::clone(self.flag_registry());
        let cancel_token = CancelToken::new();
        let cancel_token_for_task = cancel_token.clone();
        // The settle-side handle: `run_engine_turn` consumes
        // `cancel_token_for_task` by value, but the fire arm's coerce
        // below needs the same source's stall marker at settle time — a
        // clone is another handle to the same cancellation.
        let settle_cancel = cancel_token_for_task.clone();
        let stall_timeout = self.turn_stall_timeout;
        let backstop_timeout = self.turn_backstop_timeout;
        // The mid-turn abort guard (invariant 2, ③) — the steer-launched
        // turn is a user turn on the main journal, so it carries the same
        // guard `message.send` installs. Main agent only: a subagent
        // steer's turn writes its own child journal, outside invariant
        // 2's main-journal scope.
        let turn_abort_guard = if is_main_agent {
            turn_abort_guard_for(
                self.flag_registry(),
                self.resolve_session_manager(&session_id).ok(),
                &session_id,
                &session,
            )
        } else {
            None
        };

        let handle = tokio::spawn(async move {
            let outcome = run_engine_turn(
                flow_handle,
                agent_hook_engine,
                external_hook_env,
                turn_flag_registry,
                agent_tools,
                memory,
                // Switch-aware: rebuilt on a model change, so a mid-turn
                // `agent.setModel` applies from the next step's LLM call.
                llm_resolver,
                content,
                PromptOrigin::User,
                // Steers never carry an admission stamp (ADR-006): the
                // inline-skill injection runs on `message.send` only.
                None,
                Some(live_chunk_callback(
                    &event_bus,
                    &task_agent_id,
                    &session_id,
                    Some(turn_id_for_task.clone()),
                )),
                turn_id_for_task.clone(),
                wire_recorder,
                permission_manager,
                reverse_rpc_callback,
                task_session.tasks(),
                interaction_kernel,
                cancel_token_for_task,
                stall_timeout,
                backstop_timeout,
                turn_abort_guard,
                (session_id.clone(), agent_id.clone()),
            )
            .await;

            in_flight_turns_for_task
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&in_flight_key_for_task);

            match &outcome {
                Ok(turn_end) => {
                    // ADR-004 batch d: coerce before rendering — the steer
                    // fire arm is the fourth settle path (the wait arm, the
                    // send fire arm, and the task/cron drivers all coerce);
                    // a watchdog kill must record `stalled`, never a user
                    // `cancelled` (the session-fdf98268 turn 47 incident).
                    let turn_end = &coerce_aborted_if_cancelled(turn_end.clone(), &settle_cancel);
                    task_recorder
                        .record_op(WireOp::TurnEnded {
                            turn_id: turn_end.turn_id,
                            reason: turn_stop_reason_wire(&turn_end.stop_reason).to_string(),
                            error: None,
                            duration_ms: Some(turn_end.duration_ms),
                        })
                        .await;
                    event_bus.publish(SessionEvent::TurnCompleted {
                        agent_id: task_agent_id.clone(),
                        session_id: session_id.clone(),
                        turn_id: turn_id_for_task.clone(),
                        stop_reason: turn_stop_reason_wire(&turn_end.stop_reason).to_string(),
                        wire_turn_id: turn_end.turn_id,
                        duration_ms: Some(turn_end.duration_ms),
                    });
                    // spec 09 / ADR-020 + SDR-016 (A11: a steer launched on
                    // an idle agent is a user turn): the settle-site retry
                    // is not dispatched here — the in-turn watch spawned at
                    // dispatch time owns both attempts and runs them in
                    // sequence (see the wait arm's site).
                    // ADR-009 turn settle: the steer-launched turn ended —
                    // refresh the session's recency and persist it.
                    note_turn_settled(settle_index, &settle_registry, &task_session).await;
                    // Goal settlement (TS settleAbnormalTurn parity): an
                    // abnormally-ended steer turn must not leave an Active
                    // goal dangling. NOTE: on this worktree's HEAD the steer
                    // idle path does not yet coerce the stop reason (Fix F,
                    // landed elsewhere) — the settle wiring below is final
                    // placement; the coerced semantics arrive with Fix F.
                    let _ = settle_goal_after_turn(
                        &settle_registry,
                        &task_session,
                        turn_outcome(&turn_end.stop_reason),
                    )
                    .await;
                    // Goal continuation gating (TS handleTurnEnded parity):
                    // the steer-launched turn settled — the chain decision
                    // runs from the captured field Arcs.
                    maybe_launch_goal_chain(
                        &settle_registry,
                        &goal_chain_states,
                        &goal_chain_turn_flows,
                        &task_session,
                        &task_agent_id,
                        &goal_chain_resources,
                    )
                    .await;
                }
                Err(error) => {
                    let error_message = error.message();
                    tracing::error!(
                        session_id = %session_id,
                        agent_id = %task_agent_id,
                        turn_id = %turn_id_for_task,
                        error = %error_message,
                        "steer-launched turn failed"
                    );
                    event_bus.publish(SessionEvent::TurnFailed {
                        agent_id: task_agent_id.clone(),
                        turn_id: turn_id_for_task.clone(),
                        error: error_message,
                        error_code: error.domain_code().to_string(),
                        wire_turn_id: 0,
                    });
                    // A failed turn is still a settled user turn for the
                    // naming question (ADR-017 §6): the fallback runs —
                    // see the wait arm's site.
                    if task_agent_id == DEFAULT_MAIN_AGENT_ID {
                        let _ = apply_fallback_title(
                            &event_bus,
                            &task_session.meta_handle(),
                            &task_user_text,
                        )
                        .await;
                    }
                    // A failed turn is still conversation activity
                    // (ADR-009) — settle before the task ends.
                    note_turn_settled(settle_index, &settle_registry, &task_session).await;
                    // Goal settlement (TS settleAbnormalTurn parity): a
                    // failed steer turn parks an Active goal as paused with
                    // the failure reason.
                    let error_text = match error {
                        EngineTurnError::Engine(core_error) => core_error.to_string(),
                        other => other.message(),
                    };
                    let _ = settle_goal_after_turn(
                        &settle_registry,
                        &task_session,
                        TurnOutcome::Failed(&error_text),
                    )
                    .await;
                    // Goal continuation gating (TS handleTurnEnded parity):
                    // even a failed steer turn runs the chain decision
                    // after its settle (the budget block can still fire).
                    maybe_launch_goal_chain(
                        &settle_registry,
                        &goal_chain_states,
                        &goal_chain_turn_flows,
                        &task_session,
                        &task_agent_id,
                        &goal_chain_resources,
                    )
                    .await;
                }
            }
        });
        {
            let mut turns = in_flight_turns.lock().unwrap_or_else(|e| e.into_inner());
            // A finished handle means the turn completed between spawn and
            // here — drop it instead of keeping a stale in-flight entry.
            turns.retain(|_, pending| {
                !matches!(pending, InFlightTurn::Fire { handle, .. } if handle.is_finished())
            });
            turns.insert(
                in_flight_key,
                InFlightTurn::Fire {
                    handle,
                    cancel: cancel_token,
                },
            );
        }

        ack()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_report_the_engine_error_code_when_the_engine_turn_failed() {
        // Given engine-turn failures across the vocabulary
        let cases = [
            (
                EngineTurnError::Engine(CoreEngineError::ProviderError(
                    "provider rejected the request".to_string(),
                )),
                "provider.api_error",
            ),
            (
                EngineTurnError::Engine(CoreEngineError::ContextOverflow { attempts: 3 }),
                "context.overflow",
            ),
            (
                EngineTurnError::Engine(CoreEngineError::UserCancelled),
                "abort.user_cancelled",
            ),
        ];

        for (error, expected_code) in cases {
            // When asking for the failure's domain code
            // Then it reports the engine error's real class
            assert_eq!(error.domain_code(), expected_code);
        }
    }

    #[test]
    fn should_fall_back_to_internal_when_the_turn_failed_at_the_bridge_level() {
        // Given bridge-level failures (panic, timeout) with no engine error
        let panicking = EngineTurnError::Panicked("task panicked".to_string());
        let timed_out = EngineTurnError::TimedOut;

        // When asking for the domain code
        // Then both fall back to internal — there is no engine error to ask
        assert_eq!(panicking.domain_code(), "internal");
        assert_eq!(timed_out.domain_code(), "internal");
    }

    // --- engine turn stall watchdog -----------------------------------------

    use std::future::pending;
    use tokio::sync::watch;

    #[tokio::test(start_paused = true)]
    async fn should_return_the_result_when_the_turn_completes_before_the_deadline() {
        // Given a turn that finishes immediately, with the progress feed live
        let (_progress_tx, progress_rx) = watch::channel(());

        // When running it under the stall deadline
        let outcome = run_turn_with_stall_deadline(
            async { "turn done" },
            progress_rx,
            Duration::from_secs(300),
            Duration::from_secs(1800),
            Arc::new(mirri_core::WaitRegistry::new()),
            CancelToken::new(),
            StallTurnKey {
                session_id: "session-test",
                agent_id: "main",
                wire_turn_id: "turn-test",
            },
        )
        .await;

        // Then the turn's output passes through untouched
        assert_eq!(outcome, Some("turn done"));
    }

    #[tokio::test(start_paused = true)]
    async fn should_drop_the_turn_when_no_progress_arrives_for_the_full_deadline() {
        // Given a turn that never completes and never emits progress
        let (_progress_tx, progress_rx) = watch::channel(());

        // When running it under the stall deadline
        let started = tokio::time::Instant::now();
        let outcome = run_turn_with_stall_deadline(
            pending::<()>(),
            progress_rx,
            Duration::from_secs(300),
            Duration::from_secs(1800),
            Arc::new(mirri_core::WaitRegistry::new()),
            CancelToken::new(),
            StallTurnKey {
                session_id: "session-test",
                agent_id: "main",
                wire_turn_id: "turn-test",
            },
        )
        .await;

        // Then the turn is dropped only after the full deadline elapsed
        assert_eq!(outcome, None);
        assert!(tokio::time::Instant::now().duration_since(started) >= Duration::from_secs(300));
    }

    #[tokio::test(start_paused = true)]
    async fn should_keep_a_turn_alive_when_progress_keeps_arriving_past_the_deadline() {
        // Given a turn that runs 1000s total — well past the 300s deadline —
        // but emits progress every 100s, so it is never silent for the full
        // window
        let (progress_tx, progress_rx) = watch::channel(());
        let turn = {
            let progress_tx = progress_tx.clone();
            async move {
                for _ in 0..10 {
                    tokio::time::sleep(Duration::from_secs(100)).await;
                    let _ = progress_tx.send(());
                }
                "turn done"
            }
        };

        // When running it under the stall deadline
        let started = tokio::time::Instant::now();
        let outcome = run_turn_with_stall_deadline(
            turn,
            progress_rx,
            Duration::from_secs(300),
            Duration::from_secs(1800),
            Arc::new(mirri_core::WaitRegistry::new()),
            CancelToken::new(),
            StallTurnKey {
                session_id: "session-test",
                agent_id: "main",
                wire_turn_id: "turn-test",
            },
        )
        .await;

        // Then the turn survives the full duration and completes
        assert_eq!(outcome, Some("turn done"));
        assert!(tokio::time::Instant::now().duration_since(started) >= Duration::from_secs(1000));
    }

    #[tokio::test(start_paused = true)]
    async fn should_drop_the_turn_when_progress_stops_for_the_full_deadline() {
        // Given a turn that emits progress at 100s and 200s, then goes
        // silent — the deadline counted from the LAST tick must fire
        let (progress_tx, progress_rx) = watch::channel(());
        let turn = async {
            for _ in 0..2 {
                tokio::time::sleep(Duration::from_secs(100)).await;
                let _ = progress_tx.send(());
            }
            pending::<()>().await
        };

        // When running it under the stall deadline
        let started = tokio::time::Instant::now();
        let outcome = run_turn_with_stall_deadline(
            turn,
            progress_rx,
            Duration::from_secs(300),
            Duration::from_secs(1800),
            Arc::new(mirri_core::WaitRegistry::new()),
            CancelToken::new(),
            StallTurnKey {
                session_id: "session-test",
                agent_id: "main",
                wire_turn_id: "turn-test",
            },
        )
        .await;

        // Then the turn is dropped at the last tick + deadline (500s), not
        // at the original 300s total — the deadline is inactivity-based
        assert_eq!(outcome, None);
        assert!(tokio::time::Instant::now().duration_since(started) >= Duration::from_secs(500));
    }

    #[test]
    fn should_describe_a_timeout_as_a_stall_without_progress() {
        // Given a bridge-level stall timeout
        let timed_out = EngineTurnError::TimedOut;

        // When asking for the human-readable failure
        // Then it names the inactivity window, not a total-turn limit
        assert_eq!(
            timed_out.message(),
            format!(
                "engine turn stalled for {} seconds with no LLM or tool progress",
                ENGINE_TURN_STALL_TIMEOUT.as_secs()
            )
        );
    }

    #[tokio::test(start_paused = true)]
    async fn should_keep_a_blocked_task_wait_alive_when_the_task_heartbeat_advances() {
        // Given a task whose heartbeat keeps advancing (a healthy task)
        // under a manager wired with a heartbeat interval, and a liveness
        // listener that feeds the watchdog's progress channel — the same
        // wiring run_engine_turn installs (ADR-004 batch a)
        let tasks = Arc::new(
            mirri_core::TaskManager::new().with_heartbeat_interval(Duration::from_secs(100)),
        );
        let task_id = tasks.register(
            mirri_core::task::TaskKind::Process {
                command: None,
                dispatcher: None,
            },
            "healthy build".to_string(),
            "session-1".to_string(),
        );
        let (progress_tx, progress_rx) = watch::channel(());
        tasks.set_liveness_listener(Some(Arc::new({
            let progress_tx = progress_tx.clone();
            move |_task_id: &str| {
                let _ = progress_tx.send(());
            }
        })));

        // When a blocked wait runs inside the stall deadline while the
        // task keeps heart-beating every 100s — the wait outlives the 300s
        // stall window (500s total) but never goes silent for a full window
        let tasks_for_beats = Arc::clone(&tasks);
        let id_for_beats = task_id.clone();
        let turn = async move {
            let beat_task = tokio::spawn(async move {
                for _ in 0..5 {
                    tokio::time::sleep(Duration::from_secs(100)).await;
                    tasks_for_beats.heartbeat(&id_for_beats);
                }
            });
            let outcome = tasks.wait_for_completion(&task_id, 600).await;
            beat_task.await.expect("beat task");
            format!("{outcome:?}")
        };

        let started = tokio::time::Instant::now();
        let outcome = run_turn_with_stall_deadline(
            turn,
            progress_rx,
            Duration::from_secs(300),
            Duration::from_secs(1800),
            Arc::new(mirri_core::WaitRegistry::new()),
            CancelToken::new(),
            StallTurnKey {
                session_id: "session-test",
                agent_id: "main",
                wire_turn_id: "turn-test",
            },
        )
        .await;

        // Then the wait completes (timed out at its own 600s bound after
        // the heartbeats stopped at 500s) and the turn is never dropped —
        // the advancing heartbeats kept the stall deadline refreshed
        // (US5.5 acceptance 1)
        assert!(
            outcome.is_some(),
            "a healthy blocked wait must not be dropped"
        );
        assert!(
            outcome
                .as_deref()
                .is_some_and(|text| text.contains("TimedOut"))
        );
        assert!(
            tokio::time::Instant::now().duration_since(started) >= Duration::from_secs(600),
            "the wait must run to its own timeout, not the stall window"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn should_drop_a_blocked_task_wait_when_the_task_heartbeat_goes_silent() {
        // Given a task whose heartbeat never advances (a dead task — the
        // registration seed predates the wait), wired the same way
        let tasks = Arc::new(
            mirri_core::TaskManager::new().with_heartbeat_interval(Duration::from_secs(100)),
        );
        let task_id = tasks.register(
            mirri_core::task::TaskKind::Process {
                command: None,
                dispatcher: None,
            },
            "dead build".to_string(),
            "session-1".to_string(),
        );
        let (progress_tx, progress_rx) = watch::channel(());
        tasks.set_liveness_listener(Some(Arc::new({
            let progress_tx = progress_tx.clone();
            move |_task_id: &str| {
                let _ = progress_tx.send(());
            }
        })));

        // When a blocked wait runs for far longer than the stall window
        // with no heartbeat at all
        let turn = async move { tasks.wait_for_completion(&task_id, 1000).await };

        let started = tokio::time::Instant::now();
        let outcome = run_turn_with_stall_deadline(
            turn,
            progress_rx,
            Duration::from_secs(300),
            Duration::from_secs(1800),
            Arc::new(mirri_core::WaitRegistry::new()),
            CancelToken::new(),
            StallTurnKey {
                session_id: "session-test",
                agent_id: "main",
                wire_turn_id: "turn-test",
            },
        )
        .await;

        // Then the turn is dropped by the watchdog after one silence
        // window (300s) plus the post-cancel grace window (300s) — a
        // silent heartbeat must not keep the turn alive (US5.5
        // acceptance 2: the true-stall backstop stays effective)
        assert!(outcome.is_none(), "a silent task must be judged stalled");
        assert!(
            tokio::time::Instant::now().duration_since(started) >= Duration::from_secs(600),
            "the watchdog fires after cancel + grace window"
        );
    }

    /// Given a turn whose registry holds an Advancing wait and whose
    /// progress feed never fires a single tick, when the classic window
    /// elapses, then the probe's Advancing verdict resets the window and
    /// the turn survives — the pull-model equivalent of the heartbeat
    /// feed (ADR-006 milestone 2 acceptance 1).
    #[tokio::test(start_paused = true)]
    async fn should_keep_the_turn_alive_when_the_registered_wait_advances() {
        // Given a registry with one Advancing entry (an unbounded tool
        // wait — existence liveness) and a turn that completes after
        // 1000s without ever emitting progress (the push feed is silent;
        // only the probe can save it)
        let registry = Arc::new(mirri_core::WaitRegistry::new());
        let _guard = registry.enter(
            mirri_core::WaitKind::WaitingTool,
            mirri_core::WaitSubject::Tool { deadline: None },
        );
        let (_progress_tx, progress_rx) = watch::channel(());

        // When running far past the classic window (1000s > 300s) under
        // the stall deadline
        let turn = async {
            tokio::time::sleep(Duration::from_secs(1000)).await;
            "turn done"
        };
        let started = tokio::time::Instant::now();
        let outcome = run_turn_with_stall_deadline(
            turn,
            progress_rx,
            Duration::from_secs(300),
            Duration::from_secs(1800),
            registry,
            CancelToken::new(),
            StallTurnKey {
                session_id: "session-test",
                agent_id: "main",
                wire_turn_id: "turn-test",
            },
        )
        .await;

        // Then the turn completes — the Advancing probe kept resetting
        // the classic window for the full 1000s (three resets past the
        // first deadline), and it was never dropped (US5.5 acceptance 1,
        // the pull-model form)
        assert_eq!(outcome, Some("turn done"));
        assert!(
            tokio::time::Instant::now().duration_since(started) >= Duration::from_secs(1000),
            "the turn must run past the classic window on probe resets alone"
        );
    }

    /// Given a turn whose registry holds a Stalled entry (a tool wait past
    /// its own deadline), when the classic window elapses, then the
    /// watchdog cooperatively cancels at the classic window — the same
    /// scale as engine silence (ADR-006 milestone 2 acceptance 2: the
    /// per-kind window equals the classic one, 两窗不串联).
    #[tokio::test(start_paused = true)]
    async fn should_cooperatively_cancel_when_the_wait_reports_stalled() {
        // Given a registry with one Stalled entry (a bounded tool wait
        // whose deadline already passed) and a turn that settles when
        // cancelled
        let registry = Arc::new(mirri_core::WaitRegistry::new());
        let _guard = registry.enter(
            mirri_core::WaitKind::WaitingTool,
            mirri_core::WaitSubject::Tool {
                deadline: Some(std::time::Instant::now() - Duration::from_secs(1)),
            },
        );
        let (_progress_tx, progress_rx) = watch::channel(());
        let cancel = CancelToken::new();
        let turn = {
            let cancel = cancel.clone();
            async move {
                cancel.cancelled().await;
                "turn done"
            }
        };

        // When running under the stall deadline
        let started = tokio::time::Instant::now();
        let outcome = run_turn_with_stall_deadline(
            turn,
            progress_rx,
            Duration::from_secs(300),
            Duration::from_secs(1800),
            registry,
            cancel.clone(),
            StallTurnKey {
                session_id: "session-test",
                agent_id: "main",
                wire_turn_id: "turn-test",
            },
        )
        .await;

        // Then the cooperative cancel fires at the CLASSIC window (the
        // Stalled tier never waits for the long one) and the turn
        // settles with the stall marker
        assert_eq!(outcome, Some("turn done"));
        assert!(
            cancel.is_stalled(),
            "the cancel must carry the stall marker"
        );
        assert!(
            tokio::time::Instant::now().duration_since(started) < Duration::from_secs(600),
            "a stalled wait must cancel at the classic window, not the long one"
        );
    }

    /// Given a turn whose registry holds a Degraded entry (all-settled —
    /// the waiter should have returned), when the classic window elapses,
    /// then the long window arms; the turn survives the classic window,
    /// dies at the long one, and the warn log names the wait kinds
    /// (ADR-006 milestone 2 acceptance 3).
    #[tokio::test(start_paused = true)]
    async fn should_dump_diagnostics_and_hard_drop_a_degraded_turn_only_after_the_long_window() {
        // Given a registry with one Degraded entry (a settled task — the
        // waiter should have returned but never does, the framework-bug
        // escape shape) and a turn that ignores its own cancellation
        let tasks = Arc::new(
            mirri_core::TaskManager::new().with_heartbeat_interval(Duration::from_secs(100)),
        );
        let task_id = tasks.register(
            mirri_core::task::TaskKind::Process {
                command: None,
                dispatcher: None,
            },
            "settled build".to_string(),
            "session-1".to_string(),
        );
        tasks.complete(&task_id, 0);
        let registry = Arc::new(mirri_core::WaitRegistry::new());
        let _guard = registry.enter(
            mirri_core::WaitKind::WaitingTask,
            mirri_core::WaitSubject::task(task_id, Arc::clone(&tasks)),
        );
        let (_progress_tx, progress_rx) = watch::channel(());

        // When running under the stall deadline with a SHORTENED
        // backstop (600s — the with_turn_backstop_timeout seam shrinks
        // the same parameter in the full server tests)
        let started = tokio::time::Instant::now();
        let outcome = run_turn_with_stall_deadline(
            std::future::pending::<()>(),
            progress_rx,
            Duration::from_secs(300),
            Duration::from_secs(600),
            registry,
            CancelToken::new(),
            StallTurnKey {
                session_id: "session-test",
                agent_id: "main",
                wire_turn_id: "turn-test",
            },
        )
        .await;

        // Then the turn survives the classic window (300s) and dies
        // only at the long one (300s classic + 600s backstop), dropped
        // after the post-cancel grace — never at the classic window alone
        assert!(outcome.is_none(), "the degraded turn must be hard-dropped");
        let elapsed = tokio::time::Instant::now().duration_since(started);
        assert!(
            elapsed >= Duration::from_secs(900),
            "the long window must fire after the classic one (got {elapsed:?})"
        );
        assert!(
            elapsed < Duration::from_secs(1800),
            "the long window must not be the default 1800s when shortened"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn should_record_the_stalled_reason_when_a_watchdog_cancelled_turn_settles() {
        // Given a turn that settles cleanly AFTER the watchdog's
        // stall-flavored cancel fires (the cooperative settle path)
        let (_progress_tx, progress_rx) = watch::channel(());
        let cancel = CancelToken::new();
        let turn = {
            let cancel = cancel.clone();
            async move {
                // The turn idles past the stall window, then settles
                // cleanly once cancelled (the cooperative engine's own
                // settle path returning Ok).
                cancel.cancelled().await;
                "turn done"
            }
        };

        // When running it under the stall deadline — the watchdog fires
        // the stall-flavored cancel and the turn settles
        let outcome = run_turn_with_stall_deadline(
            turn,
            progress_rx,
            Duration::from_secs(300),
            Duration::from_secs(1800),
            Arc::new(mirri_core::WaitRegistry::new()),
            cancel.clone(),
            StallTurnKey {
                session_id: "session-test",
                agent_id: "main",
                wire_turn_id: "turn-test",
            },
        )
        .await;

        // Then the turn's output passes through (cooperative settle) and
        // the token carries the stall marker the settle path maps to the
        // `stalled` wire reason
        assert_eq!(outcome, Some("turn done"));
        assert!(
            cancel.is_stalled(),
            "the watchdog cancel must carry the stall marker"
        );
        assert!(!cancel.is_user_cancelled());
    }

    /// Given a turn whose cancel token was fired by the stall watchdog
    /// (`cancel_stalled`) and a clean settle result flowing through the
    /// wait arm's coercion, when the coerced stop reason is rendered for
    /// the wire, then `turn.ended` carries `stalled` — never `cancelled`.
    /// The 2026-10-17 fork-session incident's wire log recorded
    /// `cancelled` for a watchdog kill because the running `mirri-acp`
    /// binary predated the `stalled` outcome (2def121cd) — this pin
    /// (shared with the incident follow-up) guards the current chain so
    /// a regression surfaces as a red test, not a mis-attributed wire
    /// log.
    #[test]
    fn should_report_stalled_when_a_stall_cancelled_turn_is_coerced() {
        use mirri_core::TurnEndResult;
        // Given a clean turn end observed after a stall-flavored cancel
        // (the racing-settle shape the coerce path must re-attribute)
        let cancel = CancelToken::new();
        cancel.cancel_stalled();
        let turn_end = TurnEndResult {
            turn_id: 7,
            stop_reason: mirri_core::TurnStopReason::EndTurn,
            steps: 2,
            usage: Default::default(),
            duration_ms: 1000,
            blocked_by_prompt_hook: false,
        };

        // When coercing
        let coerced = super::coerce_aborted_if_cancelled(turn_end, &cancel);

        // Then the outcome is Stalled (never the user's Cancelled word) —
        // and its wire mapping is "stalled" (ADR-004 batch d)
        assert_eq!(coerced.stop_reason, mirri_core::TurnStopReason::Stalled);
        assert_eq!(
            mirri_core::event_projection::turn_stop_reason_wire(&coerced.stop_reason),
            "stalled"
        );
    }

    #[test]
    fn should_report_cancelled_when_a_user_cancelled_turn_is_coerced() {
        use mirri_core::TurnEndResult;
        // Given a clean turn end observed after a user cancel
        let cancel = CancelToken::new();
        cancel.cancel_user();
        let turn_end = TurnEndResult {
            turn_id: 7,
            stop_reason: mirri_core::TurnStopReason::EndTurn,
            steps: 2,
            usage: Default::default(),
            duration_ms: 1000,
            blocked_by_prompt_hook: false,
        };

        // When coercing
        let coerced = super::coerce_aborted_if_cancelled(turn_end, &cancel);

        // Then the outcome stays the user's Cancelled word
        assert_eq!(coerced.stop_reason, mirri_core::TurnStopReason::Aborted);
        assert_eq!(
            mirri_core::event_projection::turn_stop_reason_wire(&coerced.stop_reason),
            "cancelled"
        );
    }
}
