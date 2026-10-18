//! Manual-compaction dispatch handlers
//! (`session.beginCompaction` / `session.cancelCompaction`).

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::Value;

use mirri_core::hook_engine::HookEventType;
use mirri_core::{CompactionSource, SessionEvent, hook_additional_context_messages};
use mirri_proto::WireOp;
use mirri_transport::adapter::EngineError;

use mirri_core::EngineError as CoreEngineError;

use super::{AgentRefParams, EmbeddedServer, engine_error, invalid_params, normalize_params};

impl EmbeddedServer {
    // --- compaction domain ------------------------------------------------

    /// `session.beginCompaction` — starts a manual compaction.
    ///
    /// The compaction runs as a fire-and-forget background task. RPC returns
    /// immediately. The caller will be notified of completion via events.
    ///
    /// Returns an error if a turn is active (manual compaction requires a
    /// clean boundary) or if a compaction is already in flight. When there
    /// is nothing to compact, resolves with the bare `false` — an
    /// info-level skip, never an error (US2.6/SDR-003).
    ///
    /// Wire shape (TSV row 85): the engine-service contract models
    /// `IAgentFullCompactionService.begin(input): boolean`, so success is
    /// the bare `true` — the `{ok, started}` envelope was dropped in M3-c
    /// (no TS consumer ever read it: node-sdk `compact()` is `Promise<void>`).
    pub(super) async fn dispatch_begin_compaction(
        &self,
        params: Value,
    ) -> Result<Value, EngineError> {
        // Both spellings parse (snake_case primary, camelCase alias — the
        // sibling-handler idiom); the only wire caller (node-sdk `compact`)
        // sends camelCase, which stays the canonical working spelling.
        #[derive(serde::Deserialize)]
        struct BeginCompactionParams {
            #[serde(alias = "sessionId")]
            session_id: String,
            #[serde(default, alias = "agentId")]
            agent_id: Option<String>,
            #[serde(default)]
            instruction: Option<String>,
        }
        let params: BeginCompactionParams = serde_json::from_value(normalize_params(params))
            .map_err(|e| invalid_params(e.to_string()))?;
        let agent = self
            .resolve_agent(&AgentRefParams {
                session_id: params.session_id.clone(),
                agent_id: params.agent_id,
            })
            .await?;

        // Busy pre-check (US1.4-C2, SDR-012): refuse immediately — before
        // any lock acquisition — when a turn of this agent is in flight.
        // The engine loop holds the turn-flow mutex for the whole turn
        // (`run_engine_turn`), so a blocking acquire below would park the
        // RPC until the turn settles instead of refusing; worse, the
        // pre-fix dispatch took the memory lock before the flow lock (the
        // inverse of the documented order), turning the park into an ABBA
        // that bricks the session. The registry is keyed
        // `"{session_id}:{agent_id}:{turn_id}"`; the LLM is not created
        // and the PreCompact hook does not fire on a refused begin.
        let turn_key_prefix = format!("{}:{}:", params.session_id, agent.id().as_str());
        let turn_in_flight = {
            let turns = self
                .in_flight_turns
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            turns.keys().any(|key| key.starts_with(&turn_key_prefix))
        };
        if turn_in_flight {
            return Err(engine_error(CoreEngineError::CompactionFailed(
                "cannot compact while a turn is active".to_string(),
            )));
        }

        // Resolve the session once up front (W3 routing): the apply task
        // below records its ops on the owning workspace's wire log. Resolved
        // here — before the compaction/turn-flow guards below — so the
        // future stays Send (the await happens while no guard is held).
        let session = self.resolve_session(&params.session_id).await.ok();

        // Create the LLM for the compaction summarizer.
        let config = agent
            .config()
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let llm_factory = self.llm_factory.as_ref().ok_or_else(|| {
            EngineError::Internal("no LLM factory available for compaction".to_string())
        })?;
        let llm = llm_factory.create_llm(&config).map_err(|reason| {
            EngineError::Internal(format!(
                "LLM provider creation failed for compaction: {reason}"
            ))
        })?;
        let memory = agent.context_memory();

        // Fire the PreCompact external hook before the compaction task
        // starts (TS parity: `externalHooksService.runPreCompact` — input
        // shape `{ trigger, tokenCount }`; a block decision is not consulted
        // for manual compaction, matching TS). The hooks'
        // `additionalContext` rides along on the compaction request so the
        // summarizer sees it. Fired after the busy pre-check so a refused
        // begin does not run hooks.
        let hook_engine = agent.hook_engine();
        let mut hook_env = HashMap::new();
        hook_env.insert("MIRRI_SESSION_ID".to_string(), params.session_id.clone());
        hook_env.insert(
            "MIRRI_AGENT_ID".to_string(),
            agent.id().as_str().to_string(),
        );
        let token_count = memory
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .token_count_with_pending();
        let pre_input = serde_json::json!({
            "trigger": "manual",
            "tokenCount": token_count,
        });
        let pre_result = hook_engine
            .trigger(HookEventType::PreCompact, pre_input, &hook_env)
            .await;
        let hook_additional_context =
            hook_additional_context_messages(&pre_result.additional_context);

        // Get the turn flow (creates one if not exists yet).
        let flow_arc =
            self.get_or_create_turn_flow(&params.session_id, agent.id().as_str(), &config.model_id);

        // Non-blocking flow acquire (SDR-012, the `message.steer` precedent):
        // contention on the turn-flow mutex means a turn of this agent is
        // running (the engine loop holds it for the whole turn) — the
        // registry check above covers the common case; this arm closes the
        // registration-race window and guarantees the RPC never parks on
        // the mutex. A brief non-turn holder (e.g. a concurrent
        // `cancelCompaction`) can be mislabeled as turn-active — a
        // conservative refusal the client resolves by retrying.
        let turn_flow = match flow_arc.try_lock() {
            Ok(guard) => guard,
            Err(_) => {
                return Err(engine_error(CoreEngineError::CompactionFailed(
                    "cannot compact while a turn is active".to_string(),
                )));
            }
        };
        // The compaction orchestrator handle is cloned while the flow is
        // held, so the apply task below needs no second flow acquisition
        // (the pre-fix re-lock could park behind a turn that started in
        // the drop window, while that turn's buffer phase waited on this
        // compaction's settlement).
        let compaction_arc = turn_flow.compaction_handle();

        let attempt = turn_flow.request_manual_compaction(
            &memory,
            llm,
            params.instruction.clone(),
            None,
            hook_additional_context,
        );
        // "Nothing to compact" (the whole history fits the retention budget,
        // or no legal cut exists) skips with `false` instead of erroring —
        // the boolean `begin(): boolean` contract carries the skip, and the
        // client renders an info-level notice from it (US2.6/SDR-003; the
        // auto path skips silently). Everything else (turn active, already
        // in flight) stays a genuine error.
        let attempt = match attempt {
            Ok(attempt) => attempt,
            Err(CoreEngineError::CompactionUnable(reason)) => {
                tracing::info!(
                    session_id = %params.session_id,
                    reason = %reason,
                    "manual compaction skipped: nothing to compact"
                );
                return Ok(Value::Bool(false));
            }
            Err(e) => return Err(engine_error(e)),
        };
        drop(turn_flow);

        // Announce the compaction start on the session bus (the relay is
        // installed for a live session, so subscribers observe
        // `event.compaction.started` with the trigger and the optional
        // instruction — TS parity: the emit happens at begin, before the
        // summarizer runs).
        if let Ok(artifacts) = self.resolve_session_artifacts(&params.session_id) {
            artifacts
                .event_bus
                .publish(SessionEvent::CompactionStarted {
                    agent_id: agent.id().as_str().to_string(),
                    trigger: CompactionSource::Manual,
                    instruction: params.instruction,
                });
        }

        // Spawn a task that awaits the compaction result and applies it
        // to memory. The turn flow's compaction orchestrator state is
        // updated through the shared Arc<Mutex<FullCompaction>> — cloned
        // while the flow guard was held above (SDR-012: no second flow
        // acquisition here; see the clone site's note).
        let memory_clone = Arc::clone(&memory);
        // The session bus outlives the request; the apply task publishes
        // `event.compaction.completed` through it when the summary lands.
        let event_bus = self
            .resolve_session_artifacts(&params.session_id)
            .ok()
            .map(|artifacts| artifacts.event_bus);
        // For the post-compaction system prompt refresh — the session
        // resolved through the routing seam (W3) up front (see the
        // resolve_agent block); a `None` here means a concurrent close beat
        // the compaction, and the apply task degrades gracefully.
        let agent_id_clone = agent.id().clone();
        tokio::spawn(async move {
            match attempt.task.await {
                Ok(Ok(result)) => {
                    // A cancelled attempt never lands in memory — the
                    // cancellation event was already published by the
                    // cancel dispatch, applying here would follow it with
                    // a spurious completion. The check runs under the
                    // orchestrator lock so a racing cancel either precedes
                    // it (token already cancelled) or waits behind the
                    // apply. Settling is scoped to this attempt's id: a
                    // stale attempt must never clear a newer one's
                    // registration.
                    // The cancel check, apply, and settle run under the
                    // orchestrator lock in one scope so no guard is held
                    // across the PostCompact hook await below.
                    let applied = {
                        let mut comp = compaction_arc.lock().unwrap_or_else(|e| e.into_inner());
                        if attempt.cancel.is_cancelled() {
                            comp.settle_manual(attempt.attempt_id);
                            tracing::info!("manual compaction result discarded (cancelled)");
                            return;
                        }
                        let mut mem = memory_clone.lock().unwrap_or_else(|e| e.into_inner());
                        let applied = comp.apply_compaction_result(&mut mem, result);
                        comp.settle_manual(attempt.attempt_id);
                        applied
                    };
                    if let Some(event_bus) = &event_bus {
                        event_bus.publish(SessionEvent::CompactionCompleted {
                            agent_id: agent_id_clone.as_str().to_string(),
                            result: applied.clone(),
                        });
                    }
                    // Fire the PostCompact external hook after the successful
                    // apply (TS parity: `notifyPostCompact` — input shape
                    // `{ trigger, estimatedTokenCount }`, fire-and-forget;
                    // cancelled or failed attempts never fire it).
                    let post_input = serde_json::json!({
                        "trigger": "manual",
                        "estimatedTokenCount": applied.tokens_after,
                    });
                    let _ = hook_engine
                        .trigger(HookEventType::PostCompact, post_input, &hook_env)
                        .await;
                    // TS parity (contextMemoryService.applyCompaction +
                    // compactionWorker → refreshSystemPrompt): persist the
                    // applied compaction so `session.resume` replays the
                    // summary instead of the compacted-away history, then
                    // rebuild the system prompt from current workspace state.
                    // Both best-effort. A summary that failed the shrink
                    // check (变小校验) was never applied — persisting an
                    // apply op would corrupt resume, so nothing is recorded.
                    if applied.applied
                        && let Some(session) = session.as_ref()
                    {
                        let length = memory_clone
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .history()
                            .len() as u64;
                        session
                            .record_op(WireOp::RequestHeaderChanged {
                                reason: "compaction_applied".to_string(),
                            })
                            .await;
                        // ADR-007 provenance, derived through the shared
                        // apply-record mapping helper (see its doc for
                        // the wire-shape rules).
                        let (first_replaced_entry_seq, last_replaced_entry_seq, dropped_entry_seqs) =
                            mirri_core::compaction::apply_compaction_op_fields(&applied);
                        session
                            .record_op(WireOp::ContextApplyCompaction {
                                summary: applied.summary.clone(),
                                compacted_count: applied.compacted_count as u32,
                                tokens_before: Some(applied.tokens_before),
                                tokens_after: Some(applied.tokens_after),
                                context_summary: Some(applied.context_summary.clone()),
                                dropped_count: Some(applied.dropped_count as u32),
                                retained_tail_count: Some(applied.retained_tail_count as u32),
                                // TS-dialect reader field — the rust
                                // kernel never writes it (ADR-008).
                                kept_user_message_count: None,
                                kept_head_user_message_count: Some(
                                    applied.kept_head_user_message_count as u32,
                                ),
                                elision_message: applied.elision_message.clone(),
                                // ADR-007 provenance: the replaced range
                                // and the ladder's dropped entry numbers,
                                // mapped once for every apply-record
                                // write point.
                                first_replaced_entry_seq,
                                last_replaced_entry_seq,
                                dropped_entry_seqs,
                            })
                            .await;
                        session
                            .record_op(WireOp::ContextSizeMeasured {
                                length,
                                tokens: applied.tokens_after,
                            })
                            .await;
                        session.refresh_agent_system_prompt(&agent_id_clone).await;
                    }
                    tracing::info!("manual compaction completed successfully");
                }
                Ok(Err(e)) => {
                    tracing::error!(error = %e, "manual compaction failed");
                    // Settle the attempt's registration (scoped: a newer
                    // begin's registration is left alone). No event is
                    // published — the embedded engine has no failure-event
                    // vocabulary; the TS kernel's failure-time cancel
                    // emit is a deliberate divergence.
                    let mut comp = compaction_arc.lock().unwrap_or_else(|e| e.into_inner());
                    comp.settle_manual(attempt.attempt_id);
                }
                Err(join_err) => {
                    tracing::error!(error = %join_err, "manual compaction task panicked");
                    let mut comp = compaction_arc.lock().unwrap_or_else(|e| e.into_inner());
                    comp.settle_manual(attempt.attempt_id);
                }
            }
        });

        Ok(Value::Bool(true))
    }

    /// `session.cancelCompaction` — cancels an in-flight manual compaction.
    ///
    /// Wire shape (TSV row 86): the v2 kernel service is fire-and-forget
    /// (`v2:void`), so the request resolves to `null` — the `cancelled`
    /// boolean is no longer carried on the wire (no TS consumer ever read
    /// it: node-sdk `cancelCompaction()` is `Promise<void>`).
    pub(super) async fn dispatch_cancel_compaction(
        &self,
        params: Value,
    ) -> Result<Value, EngineError> {
        let params: AgentRefParams = serde_json::from_value(normalize_params(params))
            .map_err(|e| invalid_params(e.to_string()))?;
        let agent = self.resolve_agent(&params).await?;
        let model_id = agent
            .config()
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .model_id
            .clone();
        let flow_arc =
            self.get_or_create_turn_flow(&params.session_id, agent.id().as_str(), &model_id);
        let turn_flow = flow_arc.lock().unwrap_or_else(|e| e.into_inner());
        // The boolean stays local: cancelling is best-effort, and the v2
        // contract models the method as void. But when a manual
        // compaction was actually in flight, subscribers learn through
        // `event.compaction.cancelled` (TS parity: cancelling an idle
        // compaction state is silent — no event).
        let cancelled = turn_flow.cancel_compaction();
        drop(turn_flow);
        if cancelled && let Ok(artifacts) = self.resolve_session_artifacts(&params.session_id) {
            artifacts
                .event_bus
                .publish(SessionEvent::CompactionCancelled {
                    agent_id: agent.id().as_str().to_string(),
                });
        }
        Ok(Value::Null)
    }
}
