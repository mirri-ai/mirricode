//! The wire-op reducer: folds a [`WireOp`] into an [`AgentState`],
//! producing a new state and a list of domain events.
//!
//! Each domain event is a typed [`DomainEvent`] variant — a flat,
//! internally-tagged enum that mirrors the `WireOp` design. This makes
//! the reducer output serialisable, deserialisable, and forward-compatible
//! (unknown event types fall through to [`DomainEvent::Unknown`]).
//!
//! ## Design principle: store, don't transform
//!
//! The TypeScript kernel applies a sophisticated folding algorithm that
//! reconstructs richly typed `ContextMessage` objects from wire records.
//! For PoC-1 we deliberately skip that: messages and loop events are
//! stored as raw JSON, and the reducer only performs simple, verifiable
//! state transitions (append, clear, set field). This is sufficient to
//! prove `wire.jsonl` read → store → verify compatibility.

use crate::events::DomainEvent;
use crate::state::{
    AgentState, ConversationState, FirstCallUsage, GoalState, PlanModeState, TimelineEntry,
};
use crate::wire::WireOp;

/// Extract the session-approval pattern from a
/// `permission.record_approval_result` record, if the record represents an
/// approval that must hold for the rest of the session.
///
/// TS v2 parity (`permissionRulesOps.ts`): a pattern is derived only when
/// `result.decision === "approved"` and `result.scope === "session"` and the
/// record carries a `sessionApprovalRule`; a pattern already present is the
/// caller's dedup concern.
///
/// Two record shapes are accepted:
///
/// 1. The v2 wire shape — `sessionApprovalRule` is the pattern string
///    (e.g. `"Bash"`), gated on the record's `result`.
/// 2. The legacy shape produced by the Rust kernel's own v1.1→v1.2
///    migration (`mirri-persistence`), where `sessionApprovalRule` is
///    `{ "rule": "allow" | "deny" | "allowAll" | "denyAll", "toolName" }`
///    and the v1 record carries no `result` field. The `allow` / `allowAll`
///    rules mean "approved for the session"; the deny rules do not.
fn derive_session_approval_pattern(record: &serde_json::Value) -> Option<String> {
    match record.get("sessionApprovalRule") {
        // v2 shape: the pattern string, gated on the record's result.
        Some(serde_json::Value::String(pattern)) => {
            let result = record.get("result")?;
            let approved = result.get("decision").and_then(|v| v.as_str()) == Some("approved");
            let session_scoped = result.get("scope").and_then(|v| v.as_str()) == Some("session");
            (approved && session_scoped).then(|| pattern.clone())
        }
        // Legacy migrated shape: { rule, toolName } without a `result`.
        Some(rule @ serde_json::Value::Object(_)) => {
            if record.get("result").is_some() {
                // A record that carries a result is a v2-shape record; an
                // object sessionApprovalRule there is not a pattern source.
                return None;
            }
            let allows = matches!(
                rule.get("rule").and_then(|v| v.as_str()),
                Some("allow") | Some("allowAll")
            );
            let tool_name = rule.get("toolName").and_then(|v| v.as_str())?;
            allows.then(|| tool_name.to_string())
        }
        _ => None,
    }
}

/// Fold a single [`WireOp`] into the current [`AgentState`], returning the
/// new state and any domain events that resulted.
///
/// The reducer is **pure**: it clones the input state, mutates the clone,
/// and never touches the original. This makes replay deterministic and
/// side-effect-free.
///
/// # Arguments
/// * `state` — The state before the op is applied (not mutated).
/// * `op` — The wire operation to fold in.
///
/// # Returns
/// A tuple of `(state, events)` where `events` is a list of typed
/// [`DomainEvent`] values describing what happened (e.g.
/// [`DomainEvent::MessageAdded`], [`DomainEvent::GoalStarted`]).
pub fn reduce(state: &AgentState, op: &WireOp) -> (AgentState, Vec<DomainEvent>) {
    reduce_with_limits(state, op, None)
}

/// [`reduce`] with resume-time capacity limits for the fold simulation.
///
/// When `replay_limits` is `Some`, the compaction-apply arm's slot
/// simulation models the ceilings the session's live turns ran with (the
/// `set_limits` values the turn flow applies from its strategy every
/// turn), instead of the default constants; `None` keeps the historical
/// default-constants behavior (see [`ReplayLimits`]). All other arms are
/// limits-independent.
///
/// This is a thin compatibility wrapper over [`apply_with_limits`]: it
/// clones the input state, folds the op in place on the clone, and returns
/// the resulting state alongside the produced events. Replay hot paths that
/// mutate an [`AgentState`] they own — and discard the events — should call
/// [`apply_with_limits`] directly to avoid the per-op deep clone.
pub fn reduce_with_limits(
    state: &AgentState,
    op: &WireOp,
    replay_limits: Option<ReplayLimits>,
) -> (AgentState, Vec<DomainEvent>) {
    let mut state = state.clone();
    let events = apply_with_limits(&mut state, op, replay_limits);
    (state, events)
}

/// Fold a single [`WireOp`] into an [`AgentState`] in place, returning the
/// domain events that resulted. Unlike [`reduce`] / [`reduce_with_limits`]
/// this does **not** clone the input state: it mutates `state` directly, so
/// callers that replay a long op sequence (e.g. a session restore) avoid an
/// O(N²) deep-copy blow-up on every op. The events are returned so callers
/// can still project them; replay loops that discard events simply ignore
/// the returned vector.
///
/// The fold is equivalent to [`reduce_with_limits`] with the same
/// `replay_limits`: folding `ops` through repeated `apply_with_limits` on a
/// single mutable state yields the same final state and the same event
/// sequence as folding them through repeated [`reduce_with_limits`].
///
/// # Arguments
/// * `state` — The state to fold `op` into, modified in place.
/// * `op` — The wire operation to fold in.
/// * `replay_limits` — Resume-time capacity limits for the fold simulation
///   (see [`reduce_with_limits`]); `None` keeps the default-constants
///   behavior.
///
/// # Returns
/// A list of typed [`DomainEvent`] values describing what happened (e.g.
/// [`DomainEvent::MessageAdded`], [`DomainEvent::GoalStarted`]).
pub fn apply_with_limits(
    state: &mut AgentState,
    op: &WireOp,
    replay_limits: Option<ReplayLimits>,
) -> Vec<DomainEvent> {
    let mut events = vec![];

    match op {
        // ── Envelope ───────────────────────────────────────────────────
        // `metadata` is the first line of every wire.jsonl file. It carries
        // protocol-level info but does not affect agent state.
        WireOp::Metadata { .. } => {}

        // ── Context domain ─────────────────────────────────────────────
        WireOp::ContextAppendMessage { message } => {
            state.conversation.messages.push(message.clone());
            state.conversation.timeline.push(TimelineEntry::Message {
                index: state.conversation.messages.len() - 1,
            });
            // TS parity (`conversationTime.ts` `defineCheckpointedModel` /
            // `todoOps.ts`): a real-user-input undo anchor checkpoints the
            // todo list. The checkpoint value is the CURRENT `"todo"`
            // store entry (the TS model's `state.current`), or the empty
            // list when none exists yet.
            if is_undo_anchor_message(message) {
                let current = state
                    .tools
                    .store
                    .get("todo")
                    .cloned()
                    .unwrap_or_else(|| serde_json::Value::Array(vec![]));
                state.tools.todo_checkpoints.push(current);
            }
            let role = message
                .get("role")
                .and_then(|r| r.as_str())
                .unwrap_or("")
                .to_string();
            // PoC-1: no content preview extraction — the raw message is
            // already stored in state for consumers that need full content.
            events.push(DomainEvent::MessageAdded {
                role,
                content_preview: String::new(),
            });
        }

        WireOp::ContextAppendLoopEvent { event } => {
            // Store the raw event verbatim — no sub-type extraction for PoC-1.
            state.conversation.loop_events.push(event.clone());
            state.conversation.timeline.push(TimelineEntry::LoopEvent {
                index: state.conversation.loop_events.len() - 1,
            });
            let event_type = event
                .get("type")
                .and_then(|t| t.as_str())
                .unwrap_or("")
                .to_string();
            events.push(DomainEvent::LoopEventAdded { event_type });
        }

        WireOp::ContextClear {} => {
            state.conversation.messages.clear();
            state.conversation.loop_events.clear();
            state.conversation.timeline.clear();
            // TS parity (`conversationTime.ts` `context.clear`): a cleared
            // conversation drops the undo checkpoints — the pre-clear anchor
            // positions are no longer reachable by undo.
            state.tools.todo_checkpoints.clear();
            events.push(DomainEvent::ContextCleared {});
        }

        WireOp::ContextApplyCompaction {
            summary,
            compacted_count,
            tokens_before,
            context_summary,
            retained_tail_count,
            kept_user_message_count,
            kept_head_user_message_count,
            elision_message,
            first_replaced_entry_seq,
            last_replaced_entry_seq,
            ..
        } => {
            state.conversation.summary = Some(summary.clone());
            state.conversation.summary_tokens = tokens_before.unwrap_or(0) as u32;
            // TS parity (`contextSummary ?? summary`, compactionHandoff.ts:55):
            // the summary-as-user-message carries the context summary when
            // the record wrote one, the raw summary otherwise.
            let summary_text = context_summary.as_deref().unwrap_or(summary);
            // Three-branch selection (Task 7): a rust-native record always
            // carries `retained_tail_count` (ADR-005 — the rust kernel's
            // three write points always record both counts) and rebuilds
            // through the slot simulation; a TS-written record never does.
            // TS records are told apart by `keptUserMessageCount` — v2
            // live always writes it (the newer dialect), and its absence
            // marks the legacy dialect (transcript.ts:267; there is no
            // `legacyTail` field on the wire to read).
            if let (Some(tail_count), Some(head_count)) =
                (*retained_tail_count, *kept_head_user_message_count)
            {
                // Branch 1 — rust-native record (ADR-005): unchanged.
                reconstruct_compacted_conversation(
                    &mut state.conversation,
                    tail_count as usize,
                    head_count as usize,
                    elision_message.as_ref(),
                    summary_text,
                    replay_limits,
                    *first_replaced_entry_seq,
                    *last_replaced_entry_seq,
                );
            } else if retained_tail_count.is_some() {
                // A rust record missing its head count never occurs live;
                // the pre-Task-7 fallback holds for the torn shape.
                state.conversation.messages.clear();
                state.conversation.loop_events.clear();
                state.conversation.timeline.clear();
            } else if let Some(kept_count) = *kept_user_message_count {
                // Branch 2 — TS newer record (`keptUserMessageCount`
                // present).
                reconstruct_ts_newer_compaction(
                    &mut state.conversation,
                    kept_count as usize,
                    kept_head_user_message_count.map(|count| count as usize),
                    summary_text,
                    replay_limits,
                );
            } else {
                // Branch 3 — TS legacy record (no kept counts).
                reconstruct_ts_legacy_compaction(
                    &mut state.conversation,
                    *compacted_count as usize,
                    summary_text,
                    replay_limits,
                );
            }
            events.push(DomainEvent::CompactionApplied {
                summary_tokens: tokens_before.unwrap_or(0) as u32,
            });
            // TS parity (`conversationTime.ts` `context.apply_compaction`):
            // compaction clears the todo undo checkpoints — the
            // pre-compaction anchor positions are no longer reachable by
            // undo. This is the op the rust engine records live
            // (turn_flow/mod.rs:2108); `full_compaction.complete` never
            // touches checkpoints (TS's `FullCompaction` model has no
            // checkpoint rule).
            state.tools.todo_checkpoints.clear();
        }

        WireOp::ContextUndo { count } => {
            let undo_count = *count as usize;
            if undo_count <= state.conversation.messages.len() {
                let new_len = state.conversation.messages.len() - undo_count;
                state.conversation.messages.truncate(new_len);
                // The undone tail is contiguous at the end of the wire order:
                // drop the timeline from the first removed message entry
                // onward (loop events recorded inside that tail go with it;
                // earlier loop-event entries keep their indices — the
                // loop_events vector itself is never truncated by an undo).
                if let Some(cut) = state.conversation.timeline.iter().position(
                    |entry| matches!(entry, TimelineEntry::Message { index } if *index >= new_len),
                ) {
                    state.conversation.timeline.truncate(cut);
                }
            }
            // TS parity (`conversationTime.ts` `context.undo` reducer): undo
            // restores the todo list from the checkpoint at the popped
            // depth. An undo count exceeding the stack leaves the store
            // unchanged (the TS reducer returns the state untouched).
            if undo_count > 0 && undo_count <= state.tools.todo_checkpoints.len() {
                let idx = state.tools.todo_checkpoints.len() - undo_count;
                if let Some(restored) = state.tools.todo_checkpoints.get(idx).cloned() {
                    state.tools.store.insert("todo".to_string(), restored);
                }
                state.tools.todo_checkpoints.truncate(idx);
            }
            events.push(DomainEvent::ContextUndone { count: *count });
        }

        // ── Turn domain ────────────────────────────────────────────────
        WireOp::TurnPrompt { .. } => {
            state.turn_counter += 1;
            events.push(DomainEvent::TurnStarted {
                turn_id: state.turn_counter,
            });
        }

        // The durable steer buffer (T3): `turn.steer` parks a steer, the
        // consumption marker pops it. A resume restores what is left —
        // the steers that never reached context memory.
        WireOp::TurnSteer { input, origin } => {
            state.steer_pending.push(serde_json::json!({
                "input": input,
                "origin": origin,
            }));
        }

        WireOp::TurnSteerFlushed { count } => {
            // FIFO consumption, clamped so a torn or foreign-authored log
            // can never underflow the replay.
            let take = (*count as usize).min(state.steer_pending.len());
            state.steer_pending.drain(0..take);
        }

        WireOp::TurnCancel {
            turn_id: Some(turn_id),
            target,
            reason,
        } => {
            events.push(DomainEvent::TurnCancelled {
                turn_id: *turn_id,
                reason: reason.clone().unwrap_or_default(),
            });
            // TS parity (`interruptionReminderOps.ts`): a user-initiated
            // cancellation of the active turn parks that turn's reminder
            // until `interruptionReminder.recorded` drains it — deduped and
            // inserted in turn order.
            if target.as_deref() == Some("active")
                && reason.as_deref() == Some("user_cancelled")
                && !state.interruption_pending.contains(turn_id)
            {
                state.interruption_pending.push(*turn_id);
                state.interruption_pending.sort_unstable();
            }
        }

        WireOp::TurnCancel { .. } => {}

        WireOp::TurnEnded {
            turn_id,
            reason,
            error,
            ..
        } => {
            // The turn counter was already incremented by TurnPrompt.
            // We emit an event so callers know the turn completed.
            // Map the reason string to the appropriate variant.
            match reason.as_str() {
                "cancelled" => events.push(DomainEvent::TurnCancelled {
                    turn_id: *turn_id,
                    reason: reason.clone(),
                }),
                // ADR-004: a watchdog stall termination. It is NOT a user
                // cancel — project it as a failed turn with the stall
                // wording so replay consumers see the true outcome (the
                // domain-event vocabulary has no stalled variant; the
                // reason string carries it).
                "stalled" => events.push(DomainEvent::TurnFailed {
                    turn_id: *turn_id,
                    error: "turn stalled: no progress within the inactivity window".to_string(),
                }),
                "failed" => events.push(DomainEvent::TurnFailed {
                    turn_id: *turn_id,
                    error: error
                        .as_ref()
                        .and_then(|e| e.as_str())
                        .unwrap_or("")
                        .to_string(),
                }),
                _ => events.push(DomainEvent::TurnCompleted {
                    turn_id: *turn_id,
                    // PoC-1: per-turn token counts are not tracked here.
                    input_tokens: 0,
                    output_tokens: 0,
                }),
            }
        }

        // ── Goal domain ────────────────────────────────────────────────
        WireOp::GoalCreate {
            goal_id,
            objective,
            completion_criterion,
            status,
            budget_limits,
            ..
        } => {
            state.goal = Some(GoalState {
                goal_id: goal_id.clone(),
                objective: objective.clone(),
                completion_criterion: completion_criterion.clone(),
                status: status.clone().unwrap_or_else(|| "active".to_string()),
                turns_used: None,
                tokens_used: None,
                budget_limits: budget_limits.clone(),
            });
            events.push(DomainEvent::GoalStarted {
                objective: objective.clone(),
            });
        }

        WireOp::GoalUpdate {
            goal_id,
            status,
            reason,
            turns_used,
            tokens_used,
            budget_limits,
            ..
        } => {
            if let Some(goal) = &mut state.goal {
                if let Some(gid) = goal_id {
                    goal.goal_id = gid.clone();
                }
                if let Some(s) = status {
                    goal.status = s.clone();
                }
                if let Some(t) = turns_used {
                    goal.turns_used = Some(*t);
                }
                if let Some(t) = tokens_used {
                    goal.tokens_used = Some(*t);
                }
                if let Some(b) = budget_limits {
                    goal.budget_limits = Some(b.clone());
                }
            }
            if let Some(s) = status {
                // Emit a typed goal-status event. For well-known statuses
                // we emit the specific variant; otherwise we fall back to
                // the generic GoalUpdated.
                match s.as_str() {
                    "paused" => events.push(DomainEvent::GoalPaused {
                        reason: reason.clone().unwrap_or_default(),
                    }),
                    "resumed" => events.push(DomainEvent::GoalResumed {}),
                    "completed" => events.push(DomainEvent::GoalCompleted {}),
                    "blocked" => events.push(DomainEvent::GoalBlocked {
                        reason: reason.clone().unwrap_or_default(),
                    }),
                    _ => events.push(DomainEvent::GoalUpdated { status: s.clone() }),
                }
            }
        }

        WireOp::GoalClear {} => {
            state.goal = None;
            events.push(DomainEvent::GoalCleared {});
        }

        // ── Permission domain ──────────────────────────────────────────
        WireOp::PermissionSetMode { mode } => {
            let old_mode = state.permission.mode.clone();
            state.permission.mode = mode.clone();
            events.push(DomainEvent::PermissionModeChanged {
                old_mode,
                new_mode: mode.clone(),
            });
        }

        // `permission.record_approval_result` — derive the deduplicated
        // session-approval patterns (TS parity: the v2 kernel's
        // `sessionApprovalRulePatterns`). The full approval records are the
        // wire log itself; only the derived patterns become model state.
        WireOp::PermissionRecordApprovalResult { record } => {
            if let Some(pattern) = derive_session_approval_pattern(record)
                && !state
                    .permission
                    .session_approval_patterns
                    .contains(&pattern)
            {
                state.permission.session_approval_patterns.push(pattern);
            }
        }

        // ── Usage domain ───────────────────────────────────────────────
        WireOp::UsageRecord { model, usage, .. } => {
            // The TS wire format uses camelCase token field names:
            // `inputTokens`, `outputTokens`, `cacheReadTokens`, and
            // `cacheCreationTokens` (the cache buckets are additive — ops
            // written before the three-bucket carry-through omit them).
            let token_field = |key: &str| {
                usage
                    .as_object()
                    .and_then(|o| o.get(key))
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0)
            };
            let input = token_field("inputTokens");
            let output = token_field("outputTokens");
            let cache_read = token_field("cacheReadTokens");
            let cache_creation = token_field("cacheCreationTokens");

            // SDR-004: capture the first LLM call as the cold-start baseline
            // — only when no usage has accumulated yet, so a snapshot resumed
            // mid-session keeps its recorded first call, and old snapshots
            // (totals > 0, no first call) stay honestly absent instead of
            // mistaking the next post-resume call for the session's first.
            if state.usage.first_call.is_none()
                && state.usage.total_input_tokens == 0
                && state.usage.total_output_tokens == 0
                && state.usage.total_cache_read_tokens == 0
                && state.usage.total_cache_creation_tokens == 0
            {
                state.usage.first_call = Some(FirstCallUsage {
                    input_tokens: input,
                    output_tokens: output,
                    cache_read_tokens: cache_read,
                    cache_creation_tokens: cache_creation,
                });
            }

            state.usage.total_input_tokens += input;
            state.usage.total_output_tokens += output;
            state.usage.total_cache_read_tokens += cache_read;
            state.usage.total_cache_creation_tokens += cache_creation;

            events.push(DomainEvent::UsageRecorded {
                model: model.clone(),
                input_tokens: input,
                output_tokens: output,
                cache_read_tokens: cache_read,
                cache_creation_tokens: cache_creation,
            });
        }

        // ── Plan mode domain ───────────────────────────────────────────
        WireOp::PlanModeEnter { id, plan_text } => {
            state.plan_mode = Some(PlanModeState {
                plan_id: id.clone(),
            });
            // The versioned planText field (F12): present on logs written
            // by the rust kernel since T6, absent (None) on older logs.
            if let Some(text) = plan_text {
                state.plan_text = Some(text.clone());
            }
            events.push(DomainEvent::PlanModeEntered {
                plan_id: id.clone(),
            });
        }

        WireOp::PlanModeExit { .. } => {
            state.plan_mode = None;
            // The plan text survives the exit — the approved plan stays
            // readable after plan mode ends (restore feeds it back into
            // the plan manager's reference surface).
            events.push(DomainEvent::PlanModeExited {});
        }

        WireOp::PlanModeCancel { .. } => {
            state.plan_mode = None;
            // Cancelling discards the plan — text included.
            state.plan_text = None;
            events.push(DomainEvent::PlanModeCancelled {});
        }

        WireOp::PlanRevision {
            id,
            version,
            plan_text,
            ..
        } => {
            // The revision is the plan-content restore carrier (F12): the
            // latest revision's text wins, in enter order.
            if let Some(text) = plan_text {
                state.plan_text = Some(text.clone());
            }
            events.push(DomainEvent::PlanRevision {
                plan_id: id.clone(),
                version: *version,
            });
        }

        // ── Profile domain ─────────────────────────────────────────────
        // TS parity (`profileOps.ts`): `profile.bind` carries the bound
        // profile name and seeds the active tool set (`activeToolNames`);
        // the name falls back to the previously bound one when absent.
        WireOp::ProfileBind {
            model_alias,
            profile_name,
            active_tool_names,
            ..
        } => {
            if let Some(name) = profile_name {
                events.push(DomainEvent::ProfileBound {
                    profile_name: name.clone(),
                });
                state.profile_name = Some(name.clone());
            }
            if let Some(alias) = model_alias {
                state.model = Some(alias.clone());
            }
            state.tools.active = active_tool_names.clone();
        }

        WireOp::ConfigUpdate {
            model_alias,
            profile_name,
            ..
        } => {
            if let Some(name) = profile_name {
                state.profile_name = Some(name.clone());
            }
            if let Some(alias) = model_alias {
                state.model = Some(alias.clone());
            }
            events.push(DomainEvent::ConfigUpdated {
                model_alias: model_alias.clone(),
            });
        }

        // ── Tools domain ───────────────────────────────────────────────
        // TS parity (`profileOps.ts` / `userToolOps.ts`): the active tool
        // set is a whole-set replace (`undefined` = unrestricted) and the
        // user-tool registrations are upserted/removed by name. A whole-set
        // replace/reset supersedes the per-name enablement overrides, so
        // they clear the disabled list.
        WireOp::ToolsSetActiveTools { names } => {
            state.tools.active = Some(names.clone());
            state.tools.disabled_user_tools.clear();
        }

        WireOp::ToolsResetActiveTools {} => {
            state.tools.active = None;
            state.tools.disabled_user_tools.clear();
        }

        WireOp::ToolsRegisterUserTool { registration } => {
            let name = registration.get("name").and_then(|v| v.as_str());
            if let Some(name) = name {
                match state.tools.user_registrations.iter().position(|existing| {
                    existing.get("name").and_then(|v| v.as_str()) == Some(name)
                }) {
                    Some(index) => state.tools.user_registrations[index] = registration.clone(),
                    None => state.tools.user_registrations.push(registration.clone()),
                }
                // A fresh registration resets visibility to the default
                // (registered tools are visible in the unrestricted mode).
                state
                    .tools
                    .disabled_user_tools
                    .retain(|disabled| disabled != name);
            }
        }

        WireOp::ToolsUnregisterUserTool { name } => {
            state.tools.user_registrations.retain(|registration| {
                registration.get("name").and_then(|v| v.as_str()) != Some(name.as_str())
            });
        }

        // Per-name user-tool enablement (`tool.setUserToolEnabled`): enable
        // widens an explicit whitelist by the name (in the unrestricted
        // default the live side is already visible, so nothing to fold);
        // disable removes the name from the active set and marks it for
        // the restore-time override.
        WireOp::ToolsSetUserToolEnabled { name, enabled } => {
            if *enabled {
                state
                    .tools
                    .disabled_user_tools
                    .retain(|disabled| disabled != name);
                if let Some(active) = &mut state.tools.active
                    && !active.iter().any(|active_name| active_name == name)
                {
                    active.push(name.clone());
                }
            } else {
                if let Some(active) = &mut state.tools.active {
                    active.retain(|active_name| active_name != name);
                }
                if !state
                    .tools
                    .disabled_user_tools
                    .iter()
                    .any(|disabled| disabled == name)
                {
                    state.tools.disabled_user_tools.push(name.clone());
                }
            }
        }

        // Generic tool-store upsert (`tools.update_store`). TS parity
        // (todoOps.ts): the `"todo"` key validates strictly — a malformed
        // value (non-array, bad item shape) is skipped so one bad record
        // never takes the fold down; other keys are stored verbatim.
        WireOp::ToolsUpdateStore { key, value } => {
            if key == "todo" {
                if parse_todo_items(value).is_some() {
                    state.tools.store.insert(key.clone(), value.clone());
                }
            } else {
                state.tools.store.insert(key.clone(), value.clone());
            }
        }

        // ── MCP domain ─────────────────────────────────────────────────
        WireOp::SubagentRegistered {
            agent_id,
            profile_name,
            model,
            thinking_effort,
            parent_agent_id,
            swarm_item,
            parent_tool_call_id,
            swarm_index,
        } => {
            // Upsert by agent id — the latest registration of an identity
            // wins (a re-registration is a metadata change; the child's
            // conversation lives in its own journal, not here).
            let registration = crate::state::SubagentRegistration {
                agent_id: agent_id.clone(),
                profile_name: profile_name.clone(),
                model: model.clone(),
                thinking_effort: thinking_effort.clone(),
                parent_agent_id: parent_agent_id.clone(),
                swarm_item: swarm_item.clone(),
                parent_tool_call_id: parent_tool_call_id.clone(),
                swarm_index: *swarm_index,
            };
            match state
                .subagents
                .iter_mut()
                .find(|existing| existing.agent_id == *agent_id)
            {
                Some(existing) => *existing = registration,
                None => state.subagents.push(registration),
            }
        }
        WireOp::McpToolsDiscovered {
            server_name, tools, ..
        } => {
            let tool_count = tools.as_array().map(|a| a.len() as u32).unwrap_or(0);
            events.push(DomainEvent::McpToolsDiscovered {
                server_name: server_name.clone(),
                tool_count,
            });
        }

        // ── Compaction domain ──────────────────────────────────────────
        WireOp::FullCompactionBegin { data } => {
            let from_op = data.get("fromOp").and_then(|v| v.as_u64()).unwrap_or(0);
            let message_count = data
                .get("messageCount")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            events.push(DomainEvent::CompactionStarted {
                from_op,
                message_count,
            });
        }

        WireOp::FullCompactionComplete {} => {
            // The wire op carries no detailed data for PoC-1; the
            // summary token count and target op are not available here.
            // NOTE (TS parity): this op does NOT clear todo_checkpoints —
            // TS's `FullCompaction` model has no checkpoint rule; the
            // checkpoint clear rides `context.apply_compaction` /
            // `context.clear` (conversationTime.ts:69-71).
            events.push(DomainEvent::CompactionCompleted {
                summary_tokens: 0,
                to_op: 0,
            });
        }

        WireOp::FullCompactionCancel {} => {
            events.push(DomainEvent::CompactionFailed {
                reason: "cancelled".to_string(),
                retry_count: 0,
            });
        }

        // ── Task domain ────────────────────────────────────────────────
        WireOp::TaskStarted { info } => {
            let task_id = info
                .get("taskId")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let description = info
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            events.push(DomainEvent::BackgroundTaskStarted {
                task_id,
                description,
            });
        }

        WireOp::TaskTerminated { info, .. } => {
            let task_id = info
                .get("taskId")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let exit_code = info
                .get("exitCode")
                .and_then(|v| v.as_i64())
                .map(|i| i as i32);
            events.push(DomainEvent::BackgroundTaskCompleted { task_id, exit_code });
        }

        // ── Cron domain ────────────────────────────────────────────────
        WireOp::CronAdd { task } => {
            let task_id = task
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let schedule = task
                .get("cron")
                .or_else(|| task.get("schedule"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            // Upsert by id: re-adding an existing id replaces it in place,
            // a new id appends. A record whose id cannot be read is kept
            // verbatim so one bad record never fails the replay.
            match state.cron_tasks.iter().position(|existing| {
                existing.get("id").and_then(|v| v.as_str()) == Some(task_id.as_str())
            }) {
                Some(index) => state.cron_tasks[index] = task.clone(),
                None => state.cron_tasks.push(task.clone()),
            }
            events.push(DomainEvent::CronTaskCreated { task_id, schedule });
        }

        WireOp::CronDelete { ids } => {
            state.cron_tasks.retain(|task| {
                let id = task.get("id").and_then(|v| v.as_str());
                !id.is_some_and(|id| ids.iter().any(|target| target == id))
            });
            for id in ids {
                events.push(DomainEvent::CronTaskDeleted {
                    task_id: id.clone(),
                });
            }
        }

        WireOp::CronCursor { id, last_fired_at } => {
            // Stamp the cursor onto the matching task in place (F16): the
            // epoch-milliseconds value lands under the wire DTO key
            // `lastFiredAt` — the TS `cronCursor` apply stamps the same
            // shape — with no RFC 3339 conversion. A task stored in the
            // legacy snake_case shape (an old log the migration never
            // rewrote, or a snapshot folded before T6) keeps its old keys;
            // the restore-side parser reads both spellings. A cursor for an
            // unknown id (e.g. a task deleted earlier in the log) is a
            // no-op.
            if let Some(task) = state
                .cron_tasks
                .iter_mut()
                .find(|task| task.get("id").and_then(|v| v.as_str()) == Some(id.as_str()))
                && let Some(object) = task.as_object_mut()
            {
                object.insert(
                    "lastFiredAt".to_string(),
                    serde_json::Value::from(*last_fired_at),
                );
            }
        }

        // ── Collaboration domain ──────────────────────────────────────
        // Collaboration sessions are managed by `CollaborationManager`
        // (in `mirri-core`) at runtime. The wire record exists for
        // durability/replay; the reducer only records the event. The
        // `mode` payload stays opaque JSON — `mirri-proto` has no
        // dependency on the collaboration crate.
        WireOp::CollaborationStart {
            session_id,
            mode: _,
            task,
        } => {
            events.push(DomainEvent::CollaborationSessionStarted {
                session_id: session_id.clone(),
                task: task.clone(),
            });
        }

        WireOp::CollaborationEnd { session_id } => {
            events.push(DomainEvent::CollaborationSessionEnded {
                session_id: session_id.clone(),
            });
        }

        WireOp::CollaborationMessage {
            session_id,
            message,
        } => {
            let from = message
                .get("from")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string);
            events.push(DomainEvent::CollaborationMessageSent {
                session_id: session_id.clone(),
                from,
            });
        }

        // ── LLM domain ───────────────────────────────────────────────
        // The request-envelope drift marker (P8a-4): fold the change count
        // and the latest reason so a resume can audit when the request
        // prefix changed and why.
        WireOp::RequestHeaderChanged { reason } => {
            let header = state.request_header.get_or_insert_with(Default::default);
            header.change_count += 1;
            header.last_reason = reason.clone();
            events.push(DomainEvent::RequestHeaderChanged {
                reason: reason.clone(),
            });
        }

        // ── Interruption domain ─────────────────────────────────────────
        // Drains the pending reminder parked by `turn.cancel`: the turn's
        // interruption reminder reached the conversation (TS
        // `interruptionReminder.recorded`).
        WireOp::InterruptionReminderRecorded { turn_id } => {
            state.interruption_pending.retain(|id| id != turn_id);
        }

        // ── Swarm domain ───────────────────────────────────────────────
        // The active trigger is the swarm-mode state (TS `SwarmModel`);
        // exit retracts the enter reminder when it is still the newest
        // message — the live side pops it from memory, and replay must
        // match (TS `popSwarmModeReminder`).
        WireOp::SwarmModeEnter { trigger } => {
            state.swarm_trigger = Some(trigger.clone());
        }

        WireOp::SwarmModeExit {} => {
            state.swarm_trigger = None;
            // Retract the enter reminder when it is still the newest
            // message — the live side pops it from memory; replay must
            // match (TS `popSwarmModeReminder`).
            if let Some(popped_index) = state
                .conversation
                .messages
                .last()
                .filter(|msg| is_swarm_enter_reminder(msg))
                .map(|_| state.conversation.messages.len() - 1)
            {
                state.conversation.messages.pop();
                // The popped message's timeline entry must go with it —
                // leaving it dangling would alias the next appended
                // message's index and double-count it on replay (the
                // pop mirrors `context.undo`'s timeline maintenance).
                if let Some(entry_pos) = state.conversation.timeline.iter().rposition(
                    |entry| matches!(entry, TimelineEntry::Message { index } if *index == popped_index),
                ) {
                    state.conversation.timeline.remove(entry_pos);
                }
            }
        }

        // ── Forward compatibility ──────────────────────────────────────
        WireOp::Unknown => {
            // Unknown ops are silently skipped so that older kernels can
            // process wire.jsonl files written by newer agents.
        }

        // ── All other ops ──────────────────────────────────────────────
        // These ops (permission.rules.add, llm.request,
        // interaction.request, context_size.measured, etc.) do not modify
        // AgentState in PoC-1 and have no corresponding DomainEvent variant
        // with enough data to populate. They are accepted but produce no
        // state change or event.
        _ => {}
    }

    events
}

/// Whether a wire message is a real-user-input undo anchor (TS parity:
/// `conversationTime.ts` `isUndoAnchor`, mirrored from mirri-core's
/// `context_memory` / `undo_service` `is_undo_anchor` over the typed model).
///
/// The message is the serialized `ContextMessage` wire form: `role == "user"`
/// AND (`origin` absent/null — unattributed user messages anchor — OR the
/// origin's kind classifies as real user input). The origin's serialized
/// shape is the `PromptOrigin` wire form `{"kind": "<variant>"}`; the
/// real-user-input kinds are `"user"`, `"skill_activation"`, and
/// `"plugin_command"` (mirri-core `PromptOrigin::is_real_user_input`).
/// Injection and compaction-summary origins must NOT anchor.
///
/// Deliberate divergence from TS (adjudicated): TS's `isUndoAnchor` requires
/// `trigger === 'user-slash'` for skill_activation/plugin_command origins;
/// the rust fold matches the rust memory-side `is_undo_anchor`, which is
/// trigger-agnostic (rust live undo is the authority for rust logs). A
/// TS-written log with a `user-inline` trigger will fold one extra
/// checkpoint — accepted divergence.
fn is_undo_anchor_message(message: &serde_json::Value) -> bool {
    if message.get("role").and_then(|r| r.as_str()) != Some("user") {
        return false;
    }
    match message.get("origin") {
        None | Some(serde_json::Value::Null) => true,
        Some(origin) => {
            matches!(
                origin.get("kind").and_then(|k| k.as_str()),
                Some("user") | Some("skill_activation") | Some("plugin_command")
            )
        }
    }
}

/// Validate a `"todo"` store value: an array whose every item has a
/// non-empty string `title` and a `status` of pending/in_progress/done
/// (case-insensitive). Returns None on any malformation.
pub fn parse_todo_items(value: &serde_json::Value) -> Option<Vec<(String, String)>> {
    let arr = value.as_array()?;
    let mut out = Vec::with_capacity(arr.len());
    for item in arr {
        let title = item.get("title")?.as_str()?;
        if title.is_empty() {
            return None;
        }
        let status = item.get("status")?.as_str()?.to_ascii_lowercase();
        if !matches!(status.as_str(), "pending" | "in_progress" | "done") {
            return None;
        }
        out.push((title.to_string(), status));
    }
    Some(out)
}

/// The summary-as-user-message every apply branch embeds — the exact shape
/// the live rust apply builds and TS's `createCompactionSummaryMessage`
/// (compactionHandoff.ts:106-113) defines: a user-role message carrying the
/// summary text, tagged with the `compaction_summary` origin so the replay
/// does not prepend `conversation.summary` on top of it.
fn compaction_summary_message(summary_text: &str) -> serde_json::Value {
    serde_json::json!({
        "role": "user",
        "content": [{"type": "text", "text": summary_text}],
        "origin": {"kind": "compaction_summary"},
    })
}

/// Whether a folded conversation message is the swarm enter reminder
/// (injection origin + the reminder's leading heading —
/// `enter-reminder.md` opens with `## Swarm Mode`). The `origin.kind`
/// spelling matches the injection messages this reducer itself writes
/// (e.g. [`ts_elision_marker_message`]).
fn is_swarm_enter_reminder(msg: &serde_json::Value) -> bool {
    let is_injection = msg
        .get("origin")
        .and_then(|o| o.get("kind"))
        .and_then(|k| k.as_str())
        == Some("injection");
    let text = msg
        .get("content")
        .and_then(|c| c.as_array())
        .and_then(|parts| parts.first())
        .and_then(|p| p.get("text"))
        .and_then(|t| t.as_str());
    is_injection && text.is_some_and(is_swarm_enter_reminder_text)
}

/// Whether a context-message text is the swarm-mode ENTER reminder body —
/// the live-side gate of the exit retraction, shared with the embedded
/// `swarm.exit` handler so replay and live agree (the wire replay
/// equivalence invariant).
///
/// The TS baseline discriminates by origin sub-variant
/// (`swarm_mode` vs `swarm_mode_exit`); the Rust `PromptOrigin::Injection`
/// is a unit variant, so the header line carries the discrimination
/// instead: `## Swarm Mode\n` matches the enter reminder while the exit
/// reminder's `## Swarm Mode Ended` header does not. Live write-side
/// records wrap the body in the `<system-reminder>` envelope, so the
/// predicate sees through a leading wrapper (pre-wire fixtures and
/// hand-built states store the bare body — both must match).
pub fn is_swarm_enter_reminder_text(text: &str) -> bool {
    text.strip_prefix("<system-reminder>\n")
        .unwrap_or(text)
        .starts_with("## Swarm Mode\n")
}

/// Whether a raw record already carries its session-cumulative entry number
/// (ADR-007) — present only on messages/events retained through an earlier
/// compaction, whose rebuild stamped the number so a later apply's
/// simulation re-derives the same live numbering.
fn carries_entry_seq(raw: &serde_json::Value) -> bool {
    raw.get("entrySeq")
        .or_else(|| raw.get("entry_seq"))
        .and_then(serde_json::Value::as_u64)
        .is_some()
}

/// Stamp the session-cumulative entry number (ADR-007) onto a raw record.
/// Note the stamped-0 sentinel: `entrySeq: 0` is both "unassigned" and
/// "the session's very first entry"; the fold can only ever see it on the
/// first materialization, because a stamped-0 record replaying at a nonzero
/// counter would have taken that counter's value live instead.
fn stamp_entry_seq(raw: &mut serde_json::Value, seq: u64) {
    if let Some(object) = raw.as_object_mut() {
        object.insert("entrySeq".to_string(), serde_json::Value::from(seq));
    }
}

/// The folded conversation's coordinates in the live materialization order
/// — the shared prefix of every apply-reconstruction branch.
struct FoldedLayout {
    plan: MaterializationPlan,
    /// Every timeline entry whose slot survived, as `(live position, wire
    /// index)` pairs sorted so same-slot entries keep their wire order.
    placed: Vec<(usize, usize)>,
    /// The number of surviving slots — live `history.len()` at the apply
    /// point.
    total_surviving: usize,
}

impl FoldedLayout {
    /// The entry number of the slot a placed entry materialized, lifted by
    /// the bracket correction for fresh (unstamped) records (ADR-007) — a
    /// record that already carries a number keeps it and takes no
    /// correction. The TS branches pass offset 0: TS records carry no
    /// interval fields, so there is no undo gap to correct (see
    /// [`reconstruct_ts_newer_compaction`]).
    fn corrected_seq(&self, raw: &serde_json::Value, wire: usize, offset: u64) -> Option<u64> {
        self.plan.entry_slot[wire].map(|slot| {
            let sim = self.plan.slot_entry_seq[slot];
            if carries_entry_seq(raw) {
                sim
            } else {
                sim + offset
            }
        })
    }
}

/// Simulate the live materialization over the folded wire order (under the
/// resume-time limits when the replay supplies them) and lay out every
/// surviving entry in live order — step 1 shared by all three apply
/// branches.
fn folded_layout(conv: &ConversationState, replay_limits: Option<ReplayLimits>) -> FoldedLayout {
    let plan = plan_materialization(
        &conv.timeline,
        &conv.messages,
        &conv.loop_events,
        replay_limits,
    );
    // Final position (live order) of each surviving slot; a vacuous step's
    // partial assistant never occupies one.
    let mut final_position: Vec<Option<usize>> = Vec::with_capacity(plan.slot_survives.len());
    let mut total_surviving = 0usize;
    for &survives in &plan.slot_survives {
        if survives {
            final_position.push(Some(total_surviving));
            total_surviving += 1;
        } else {
            final_position.push(None);
        }
    }
    // All placed entries in live materialization order: (slot position,
    // wire index), sorted so same-slot entries keep their wire order.
    let mut placed: Vec<(usize, usize)> = Vec::with_capacity(plan.entry_slot.len());
    for (wire, slot) in plan.entry_slot.iter().enumerate() {
        if let Some(slot) = slot
            && let Some(pos) = final_position[*slot]
        {
            placed.push((pos, wire));
        }
    }
    placed.sort_unstable();
    FoldedLayout {
        plan,
        placed,
        total_surviving,
    }
}

/// TS `compactionUserMessageDisposition` (compactionHandoff.ts:142-173):
/// which user-role messages a TS compaction keeps verbatim. An absent
/// origin and `user` origins are kept; skill/plugin activations only when
/// user-slash triggered; every other origin (injections, shell commands,
/// compaction summaries, reminders, ...) is compactable-away metadata.
/// Unlike the rust branch's origin-kind-only `real_user` predicate this
/// mirrors the TS one exactly — role and trigger included.
fn ts_compactable_user(raw: &serde_json::Value) -> bool {
    if raw.get("role").and_then(serde_json::Value::as_str) != Some("user") {
        return false;
    }
    match raw.get("origin") {
        None | Some(serde_json::Value::Null) => true,
        Some(origin) => match origin.get("kind").and_then(serde_json::Value::as_str) {
            Some("user") => true,
            Some("skill_activation") | Some("plugin_command") => {
                origin.get("trigger").and_then(serde_json::Value::as_str) == Some("user-slash")
            }
            _ => false,
        },
    }
}

/// The TS elision marker (`createCompactionElisionMessage` /
/// `buildCompactionElisionText`, compactionHandoff.ts:115-130): an
/// injection-origin `<system-reminder>` inserted between the head and
/// tail kept-user segments when the selection split. TS records carry
/// neither the marker nor its omitted-token count (unlike rust records,
/// which embed the marker verbatim), so the rebuild synthesizes both —
/// the count as an estimate (see [`estimate_user_message_tokens`]).
fn ts_elision_marker_message(omitted_tokens: u64) -> serde_json::Value {
    let text = [
        "<system-reminder>",
        &format!(
            "Some of this conversation's user messages were omitted here during compaction: \
             the messages above this note are the oldest user input, the messages below are \
             the most recent, and roughly {omitted_tokens} tokens in between were dropped. \
             The omitted content is covered by the compaction summary at the end of the \
             conversation."
        ),
        "</system-reminder>",
    ]
    .join("\n");
    serde_json::json!({
        "role": "user",
        "content": [{"type": "text", "text": text}],
        "origin": {"kind": "injection", "variant": "compaction_elision"},
    })
}

/// Rough token estimate for the synthesized elision marker's omitted-token
/// count: the live TS text embeds the TS estimator's figure, which the
/// wire record does not carry. The estimate uses the same chars-per-4
/// heuristic the TS estimator applies to ASCII text, over the dropped
/// compactable user messages.
///
/// Two approximations, both documented: on split records the caller sums
/// this estimate over the rust-side contiguous prefix below the kept
/// window — NOT TS's actual middle-gap set (see the head-divergence TODO
/// on [`reconstruct_ts_newer_compaction`]); and chars/4 undercounts CJK
/// text versus TS's estimator, which counts non-ASCII characters per-char.
fn estimate_user_message_tokens(conv: &ConversationState, wire: usize) -> u64 {
    let text_len = |raw: &serde_json::Value| -> u64 {
        match raw.get("content") {
            Some(serde_json::Value::String(text)) => text.len() as u64,
            Some(serde_json::Value::Array(parts)) => parts
                .iter()
                .filter(|part| part.get("type").and_then(serde_json::Value::as_str) == Some("text"))
                .filter_map(|part| part.get("text").and_then(serde_json::Value::as_str))
                .map(str::len)
                .map(|len| len as u64)
                .sum(),
            _ => 0,
        }
    };
    match &conv.timeline[wire] {
        TimelineEntry::Message { index } => conv
            .messages
            .get(*index)
            .map(|raw| text_len(raw).div_ceil(4))
            .unwrap_or(0),
        TimelineEntry::LoopEvent { .. } => 0,
    }
}

/// Rebuild the conversation as `[head user essence][elision?][summary]
/// [retained tail]` (ADR-005). The counts come from the live apply, which
/// knows the exact memory-slot split; the timeline's slot coordinates are
/// recovered by simulating the same materialization rules
/// `ContextMemory::append_message` / `append_loop_event` apply.
///
/// ADR-007 provenance (Task 4): `first_replaced_entry_seq` /
/// `last_replaced_entry_seq` carry the LIVE entry numbers of the replaced
/// prefix's ends. Without them the fold's simulation cannot know the live
/// counter when an undo precedes the apply — an undo consumes numbers the
/// fold never sees (the removed records are gone from the fold state, and
/// numbering never rewinds), so the simulation's counter ends below the
/// live one and the naive stamps would collide with live numbers. The
/// record's bracket closes that gap: comparing it against the simulation's
/// own bracket over the same prefix slots yields exactly the invisible
/// number gap, which lifts the fresh (unstamped) tail materializations and
/// the apply's own marker/summary numbers to their live values. Records
/// that already carry a number (retained through an earlier compaction)
/// keep it on both sides and take no correction.
///
/// Residual (documented, full surface): the bracket yields exactly two
/// uniform offsets — `head_offset` for the fresh head-essence stamps and
/// `tail_offset` for the fresh tail stamps plus the marker/summary numbers
/// — so the correction is exact **iff the undo cut lies outside both the
/// head-essence region and the tail region** (before the prefix start, or
/// in the discarded middle the apply folds away). An undo cut inside the
/// tail region leaves entries past the cut understamped by the missing
/// gap; symmetrically, a cut between two kept head-essence entries (e.g.
/// live 0,1,2,3 → undo → 4,5,6 with head keeping the 0- and 4-numbered
/// questions) leaves the later head entries understamped (stamped with the
/// simulation's number instead of their live one) while the tail, marker,
/// and summary stay correct. A single undo recorded before the compaction
/// is the shape the round-trip gate pins. Multi-undo or arbitrary-cut
/// fidelity would need per-undo records (each carrying its own consumed
/// numbers) — the escape hatch ADR-007's record shape deliberately defers.
#[allow(clippy::too_many_arguments)]
fn reconstruct_compacted_conversation(
    conv: &mut ConversationState,
    retained_tail_count: usize,
    kept_head_user_message_count: usize,
    elision_message: Option<&serde_json::Value>,
    summary_text: &str,
    replay_limits: Option<ReplayLimits>,
    first_replaced_entry_seq: Option<u64>,
    last_replaced_entry_seq: Option<u64>,
) {
    let legacy_clear = |conv: &mut ConversationState| {
        conv.messages.clear();
        conv.loop_events.clear();
        conv.timeline.clear();
    };

    if retained_tail_count == 0 {
        legacy_clear(conv);
        return;
    }

    // 1. Lay out the folded wire order in live materialization order.
    let layout = folded_layout(conv, replay_limits);
    let Some(tail_start) = layout.total_surviving.checked_sub(retained_tail_count) else {
        // Counts exceed the folded history (corrupt log) — fall back.
        legacy_clear(conv);
        return;
    };
    let placed = &layout.placed;

    // ADR-007 undo-gap correction (Task 4): compare the record's replaced
    // range against the simulation's own bracket over the same prefix slots.
    // `first - sim_first` is the invisible gap below the prefix (numbers
    // consumed by an undo that cut before the prefix started, or by
    // anything else the fold never materialized); `last - sim_last` is the
    // gap through the prefix's end. Without an undo in the log both are 0
    // — the correction is a no-op and the retention-count derivation
    // stands exactly as before. An inconsistent bracket (empty prefix or a
    // corrupt/hand-written record) also yields (0, 0).
    let (head_offset, tail_offset) = match (first_replaced_entry_seq, last_replaced_entry_seq) {
        (Some(first), Some(last)) => {
            let mut sim_first = u64::MAX;
            let mut sim_last = 0u64;
            let mut prefix_slots = 0usize;
            for &(pos, wire) in placed {
                if pos >= tail_start {
                    continue;
                }
                if let Some(slot) = layout.plan.entry_slot[wire] {
                    let seq = layout.plan.slot_entry_seq[slot];
                    sim_first = sim_first.min(seq);
                    sim_last = sim_last.max(seq);
                    prefix_slots += 1;
                }
            }
            if prefix_slots == 0 || sim_first > first || sim_last > last {
                (0, 0)
            } else {
                (first - sim_first, last - sim_last)
            }
        }
        _ => (0, 0),
    };

    // 2. Rebuild: head essence, elision, summary, then the tail with
    //    remapped indices. Every rebuilt record carries its
    //    session-cumulative entry number (ADR-007): retained
    //    messages/events keep the number the simulation re-derived for
    //    their slot, while the elision marker and the summary — the
    //    apply's only new materializations — take the counter's next
    //    values in history order (marker first), exactly what the live
    //    `apply_compaction` assigned. A resume therefore re-derives
    //    identical numbering, and the live == replayed gate holds by
    //    construction.
    let elision_entry_seq = layout.plan.next_entry_seq + tail_offset;
    let summary_entry_seq = elision_entry_seq + u64::from(elision_message.is_some());

    let mut new_messages: Vec<serde_json::Value> = Vec::new();
    let mut new_loop_events: Vec<serde_json::Value> = Vec::new();
    let mut new_timeline: Vec<TimelineEntry> = Vec::new();

    // 2a. Head essence: the first K real-user messages of the PREFIX
    //     (slot position < tail_start), in live slot order — wire order
    //     can differ from slot order when a message was deferred behind
    //     an open tool exchange.
    let real_user = |raw: &serde_json::Value| {
        raw.get("origin")
            .and_then(|o| o.get("kind"))
            .and_then(serde_json::Value::as_str)
            .is_some_and(|kind| matches!(kind, "user" | "skill_activation" | "plugin_command"))
    };
    let mut kept = 0usize;
    for &(pos, wire) in placed {
        if kept >= kept_head_user_message_count || pos >= tail_start {
            break; // `placed` is sorted by slot position.
        }
        if let TimelineEntry::Message { index } = &conv.timeline[wire]
            && let Some(raw) = conv.messages.get(*index)
            && real_user(raw)
        {
            let mut raw = raw.clone();
            if let Some(seq) = layout.corrected_seq(&raw, wire, head_offset) {
                stamp_entry_seq(&mut raw, seq);
            }
            new_timeline.push(TimelineEntry::Message {
                index: new_messages.len(),
            });
            new_messages.push(raw);
            kept += 1;
        }
    }

    // 2b. Elision marker exactly as the live apply inserted it, carrying the
    //     number the live apply assigned it.
    if let Some(marker) = elision_message {
        let mut marker = marker.clone();
        stamp_entry_seq(&mut marker, elision_entry_seq);
        new_timeline.push(TimelineEntry::Message {
            index: new_messages.len(),
        });
        new_messages.push(marker);
    }

    // 2c. Summary message at its live position (mirrors the
    //     CompactionSummary-tagged user message apply_compaction built;
    //     replay.rs must not prepend conversation.summary again — it
    //     skips when a compaction_summary message is already embedded).
    new_timeline.push(TimelineEntry::Message {
        index: new_messages.len(),
    });
    let mut summary = compaction_summary_message(summary_text);
    stamp_entry_seq(&mut summary, summary_entry_seq);
    new_messages.push(summary);

    // 2d. Retained tail: every entry whose slot position is a tail slot,
    //     in live materialization order (indices remapped into the new
    //     vectors). A deferred message therefore replays after the tool
    //     result that closed its exchange, and a vacuous step's entries
    //     are dropped entirely — exactly the slots live kept. The
    //     slot-creating events (`step.begin` / a resolving `tool.result`)
    //     carry their slot's entry number so the step-materialized
    //     messages re-derive their original numbers on replay too.
    for &(pos, wire) in placed {
        if pos < tail_start {
            continue;
        }
        match &conv.timeline[wire] {
            TimelineEntry::Message { index } => {
                if let Some(raw) = conv.messages.get(*index) {
                    let mut raw = raw.clone();
                    if let Some(seq) = layout.corrected_seq(&raw, wire, tail_offset) {
                        stamp_entry_seq(&mut raw, seq);
                    }
                    new_timeline.push(TimelineEntry::Message {
                        index: new_messages.len(),
                    });
                    new_messages.push(raw);
                }
            }
            TimelineEntry::LoopEvent { index } => {
                if let Some(raw) = conv.loop_events.get(*index) {
                    let mut raw = raw.clone();
                    let creates_slot = raw
                        .get("type")
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(|kind| matches!(kind, "step.begin" | "tool.result"));
                    if creates_slot && let Some(seq) = layout.corrected_seq(&raw, wire, tail_offset)
                    {
                        stamp_entry_seq(&mut raw, seq);
                    }
                    new_timeline.push(TimelineEntry::LoopEvent {
                        index: new_loop_events.len(),
                    });
                    new_loop_events.push(raw);
                }
            }
        }
    }

    conv.messages = new_messages;
    conv.loop_events = new_loop_events;
    conv.timeline = new_timeline;
}

/// Branch 2 — a TS v2 record (`keptUserMessageCount` present; TS v2 live
/// always writes it): rebuild the conversation as the TS compaction shape
/// `buildContextCompactionShape` produces (compactionHandoff.ts:50-99) —
/// `[head kept users][elision marker?][tail kept users][summary]`, the
/// summary LAST. The kept set is the LAST `kept_user_message_count`
/// compactable user messages of the folded history, in live
/// materialization order; the head segment is the
/// `kept_head_user_message_count` OLDEST of them. `kept_head` absent → no
/// split: no elision marker, everything kept is tail — the same
/// kept + 1 / head-split + 2 count derivation transcript.ts:271-272
/// applies. Everything else — assistant steps, tool exchanges, injections
/// — is folded into the summary and dropped from the fold state.
///
/// TODO(head divergence on split records): TS live, on a split record
/// (`keptHeadUserMessageCount` present), kept as head the OLDEST
/// compactable user messages of the WHOLE history and dropped the middle
/// between head and tail. This rebuild instead keeps the contiguous LAST
/// `kept_user_message_count` compactable messages and splits that window
/// at `kept_head_user_message_count`, so its head segment can consist of
/// messages TS actually dropped, and the synthesized elision marker's
/// "the messages above this note are the oldest user input" claim can be
/// false in the rebuild. The record's counts cannot distinguish the two
/// cases — full fidelity would need the kept messages' identities on the
/// wire, which the TS dialect does not carry.
///
/// Numbering (ADR-007): TS records carry no interval fields, so the kept
/// messages keep the numbers the fold's simulation re-derived for their
/// slots, while the elision marker and the summary — the apply's only new
/// materializations here — take the counter's next values in history order
/// (marker first), as the rust branch's rebuild does. An undo recorded
/// before the apply consumed numbers the fold never sees; the bracket
/// correction that closes that gap on rust records needs the interval
/// fields the TS dialect never writes, so the fresh marker/summary numbers
/// can sit below the values an undo would have pushed them to — the
/// documented residual [`reconstruct_compacted_conversation`] pins for
/// rust records does not apply here and no correction is attempted.
fn reconstruct_ts_newer_compaction(
    conv: &mut ConversationState,
    kept_user_message_count: usize,
    kept_head_user_message_count: Option<usize>,
    summary_text: &str,
    replay_limits: Option<ReplayLimits>,
) {
    let layout = folded_layout(conv, replay_limits);
    // TS `collectCompactableUserMessages` (compactionHandoff.ts:132-136)
    // over the folded history, in live materialization order.
    let compactable: Vec<usize> = layout
        .placed
        .iter()
        .filter_map(|&(_pos, wire)| match &conv.timeline[wire] {
            TimelineEntry::Message { index } => conv
                .messages
                .get(*index)
                .is_some_and(ts_compactable_user)
                .then_some(wire),
            TimelineEntry::LoopEvent { .. } => None,
        })
        .collect();
    // Keep the LAST kept_user_message_count of them (TS v2 keeps the most
    // recent user input verbatim ahead of the summary; a count beyond the
    // available messages keeps them all — no corrupt-record fallback).
    let kept_start = compactable.len().saturating_sub(kept_user_message_count);
    // The head segment: the oldest of the kept (absent → no split at all).
    let head_count = kept_head_user_message_count
        .map(|count| count.min(compactable.len() - kept_start))
        .unwrap_or(0);

    // The omitted-token estimate for the synthesized elision marker: the
    // live TS text embeds the TS estimator's figure, which the wire record
    // does not carry.
    let omitted_tokens: u64 = compactable[..kept_start]
        .iter()
        .map(|&wire| estimate_user_message_tokens(conv, wire))
        .sum();

    let elided = kept_head_user_message_count.is_some();
    let elision_entry_seq = layout.plan.next_entry_seq;
    let summary_entry_seq = elision_entry_seq + u64::from(elided);

    // A kept user message, stamped with its slot's entry number (ADR-007,
    // no bracket correction — TS records carry no interval fields).
    let kept_raw = |wire: usize| -> Option<serde_json::Value> {
        let TimelineEntry::Message { index } = &conv.timeline[wire] else {
            return None;
        };
        conv.messages.get(*index).map(|raw| {
            let mut raw = raw.clone();
            if let Some(seq) = layout.corrected_seq(&raw, wire, 0) {
                stamp_entry_seq(&mut raw, seq);
            }
            raw
        })
    };

    // Every rebuilt entry is a message on this branch, so the new timeline
    // is trivially sequential — collect the rebuilt records in order.
    let mut rebuilt: Vec<serde_json::Value> = Vec::new();
    // Head segment: the oldest of the kept user messages.
    for &wire in &compactable[kept_start..kept_start + head_count] {
        if let Some(raw) = kept_raw(wire) {
            rebuilt.push(raw);
        }
    }
    // The elision marker TS inserted between the segments when the
    // selection split (present iff keptHeadUserMessageCount is).
    if elided {
        let mut marker = ts_elision_marker_message(omitted_tokens);
        stamp_entry_seq(&mut marker, elision_entry_seq);
        rebuilt.push(marker);
    }
    // Tail segment: the remaining kept user messages, then the summary —
    // LAST, the TS v2 shape.
    for &wire in &compactable[kept_start + head_count..] {
        if let Some(raw) = kept_raw(wire) {
            rebuilt.push(raw);
        }
    }
    let mut summary = compaction_summary_message(summary_text);
    stamp_entry_seq(&mut summary, summary_entry_seq);
    rebuilt.push(summary);

    conv.messages = rebuilt;
    conv.loop_events.clear();
    conv.timeline = (0..conv.messages.len())
        .map(|index| TimelineEntry::Message { index })
        .collect();
}

/// Branch 3 — a TS legacy record (`keptUserMessageCount` absent, the
/// pre-field dialect): rebuild as `[summary][...folded history from live
/// index compacted_count onward verbatim]` — the legacy
/// `[summary, ...tail]` restore shape (contextOps.ts:56-59, mirrored at
/// transcript.ts:273-281). The tail keeps the folded history's
/// materialized entries from live position `compacted_count` on —
/// assistant steps and tool results included — with their raw records
/// verbatim. When `compacted_count` covers the whole folded length the
/// slice is empty and the rebuild is `[summary]` alone (the complement of
/// the `compactedCount < length` guard, transcript.ts:273).
///
/// Numbering (ADR-007): as on the newer branch — no interval fields, so
/// the retained entries keep their simulation-derived numbers and the
/// summary takes the counter's next value (the apply's only new
/// materialization here). See [`reconstruct_ts_newer_compaction`] for the
/// undo-gap residual.
fn reconstruct_ts_legacy_compaction(
    conv: &mut ConversationState,
    compacted_count: usize,
    summary_text: &str,
    replay_limits: Option<ReplayLimits>,
) {
    let layout = folded_layout(conv, replay_limits);
    // The tail boundary: everything from live position compacted_count on
    // survives verbatim (a count at or past the folded length keeps
    // nothing — [summary] alone, by design rather than fallback).
    let tail_start = compacted_count;

    let mut new_messages: Vec<serde_json::Value> = Vec::new();
    let mut new_loop_events: Vec<serde_json::Value> = Vec::new();
    let mut new_timeline: Vec<TimelineEntry> = Vec::new();

    // The summary FIRST — the legacy restore shape puts it ahead of the
    // kept tail (contextOps.ts:56-59).
    new_timeline.push(TimelineEntry::Message {
        index: new_messages.len(),
    });
    let mut summary = compaction_summary_message(summary_text);
    stamp_entry_seq(&mut summary, layout.plan.next_entry_seq);
    new_messages.push(summary);

    // The verbatim tail: every entry whose live position is at or past the
    // boundary, in live materialization order (indices remapped into the
    // new vectors) — the same walk the rust branch's retained tail takes,
    // so a deferred message replays after the tool result that closed its
    // exchange and the slot-creating events (`step.begin` / a resolving
    // `tool.result`) carry their slot's entry number.
    for &(pos, wire) in &layout.placed {
        if pos < tail_start {
            continue;
        }
        match &conv.timeline[wire] {
            TimelineEntry::Message { index } => {
                if let Some(raw) = conv.messages.get(*index) {
                    let mut raw = raw.clone();
                    if let Some(seq) = layout.corrected_seq(&raw, wire, 0) {
                        stamp_entry_seq(&mut raw, seq);
                    }
                    new_timeline.push(TimelineEntry::Message {
                        index: new_messages.len(),
                    });
                    new_messages.push(raw);
                }
            }
            TimelineEntry::LoopEvent { index } => {
                if let Some(raw) = conv.loop_events.get(*index) {
                    let mut raw = raw.clone();
                    let creates_slot = raw
                        .get("type")
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(|kind| matches!(kind, "step.begin" | "tool.result"));
                    if creates_slot && let Some(seq) = layout.corrected_seq(&raw, wire, 0) {
                        stamp_entry_seq(&mut raw, seq);
                    }
                    new_timeline.push(TimelineEntry::LoopEvent {
                        index: new_loop_events.len(),
                    });
                    new_loop_events.push(raw);
                }
            }
        }
    }

    conv.messages = new_messages;
    conv.loop_events = new_loop_events;
    conv.timeline = new_timeline;
}

/// A timeline's coordinates in the live memory materialization, produced
/// by simulating `ContextMemory`'s append rules over the folded wire order.
struct MaterializationPlan {
    /// Per timeline entry: the logical slot it created (slot-creating
    /// entries: directly-appended messages, `step.begin`, a `tool.result`
    /// resolving a pending call, and a deferred message flushed when its
    /// exchange closed) or mutated (`tool.call` / `content.part` /
    /// `think.part` / `step.end` targeting an open step's slot). `None`
    /// for entries with no live effect (an orphan `tool.result`, an event
    /// for an unknown step).
    entry_slot: Vec<Option<usize>>,
    /// Per logical slot: whether it survives in the live history. A
    /// vacuous step (no content part, no tool call at its `step.end`) has
    /// its empty partial assistant removed, so its slot does not survive.
    slot_survives: Vec<bool>,
    /// Per logical slot: its session-cumulative entry number (ADR-007) —
    /// the number live `push_history` assigned when the slot materialized.
    /// Mirrors the live counter: a slot created by a message/event that
    /// already carries a number (retained through an earlier compaction,
    /// re-stamped by that apply's rebuild) keeps it; a fresh materialization
    /// takes the running counter, which starts past every stamped number
    /// the folded state carries. A vacuous step's slot still consumes its
    /// number exactly like the live push-then-remove did.
    slot_entry_seq: Vec<u64>,
    /// The counter after the simulation — the number the live counter held
    /// at the apply point, i.e. the value the apply's own new
    /// materializations (elision marker, then summary) take.
    next_entry_seq: u64,
}

/// Mirror of live `is_vacuous_content_part` for the raw wire part JSON:
/// only text-bearing parts can be vacuous (whitespace-only); media
/// parts (image/audio/video/file, and unknown types) never are. TS
/// think parts carry the text under `"think"`, rust's under `"text"`.
fn is_vacuous_part(part: &serde_json::Value) -> bool {
    match part.get("type").and_then(serde_json::Value::as_str) {
        Some("text") => part
            .get("text")
            .and_then(serde_json::Value::as_str)
            .is_none_or(|text| text.trim().is_empty()),
        Some("think") => part
            .get("think")
            .or_else(|| part.get("text"))
            .and_then(serde_json::Value::as_str)
            .is_none_or(|text| text.trim().is_empty()),
        _ => false,
    }
}

/// Resume-time capacity limits for the fold simulation. `None` keeps
/// the historical default-constants behavior (10_000/1_000 — the
/// production norm). Limits are constant per session run (set every
/// turn from the strategy, compaction/mod.rs:112-119), so resume-time
/// values reproduce the historical fold unless the config changed
/// between runs — that residual is accepted and documented.
#[derive(Debug, Clone, Copy)]
pub struct ReplayLimits {
    pub max_history_messages: usize,
    pub max_deferred_messages: usize,
}

/// Ceiling on simulated history slots — a mirror of the live default
/// `DEFAULT_MAX_HISTORY_MESSAGES` (`mirri-core`'s `context_memory/mod.rs`).
///
/// The wire op carries no per-session limits, so the fold-time simulation
/// assumes these ceilings when the replay does not supply resume-time
/// limits ([`ReplayLimits`], threaded from the resume path — the mirri-core
/// session manager's replay — that knows the turn-flow strategy the
/// session ran with). A session that ran `set_limits` with
/// tighter ceilings will have rejected more appends than a default-limits
/// simulation models — the residual the `ReplayLimits` threading closes.
const MAX_HISTORY_MESSAGES: usize = 10_000;

/// Ceiling on simulated deferred messages — a mirror of the live default
/// `DEFAULT_MAX_DEFERRED_MESSAGES`.
const MAX_DEFERRED_MESSAGES: usize = 1_000;

/// Simulate `ContextMemory::append_message` / `append_loop_event` over the
/// folded wire order and assign every timeline entry a slot coordinate,
/// under the given capacity ceilings — the resume-time limits when the
/// replay supplies them, the default constants otherwise (see
/// [`plan_materialization_with_limits`] for the full rule set).
///
/// The simulated rules mirror the live materialization
/// (`context_memory/mod.rs`, `append_loop_event`):
/// - a Message entry materializes one slot when no tool exchange is open;
///   while one is open the message is **deferred** and its slot is assigned
///   only when the exchange closes (the resolving `tool.result`), so its
///   slot comes after the tool result's, not at its wire position,
/// - `step.begin` opens a partial assistant slot; `tool.call` attaches to
///   it (+0 slots) and registers a pending call id; `content.part` /
///   `think.part` attach to it (+0),
/// - a `tool.result` materializes one slot only for an id a `tool.call`
///   registered — a legacy message's inline `toolCalls` register nothing,
///   so a result for such an id is dropped without a slot, exactly like
///   live `append_message` (which registers no pending ids),
/// - `step.end` removes the step's slot entirely when the step is vacuous
///   (no non-whitespace content and no tool call),
fn plan_materialization(
    timeline: &[TimelineEntry],
    messages: &[serde_json::Value],
    loop_events: &[serde_json::Value],
    replay_limits: Option<ReplayLimits>,
) -> MaterializationPlan {
    let (max_history_messages, max_deferred_messages) = match replay_limits {
        Some(limits) => (limits.max_history_messages, limits.max_deferred_messages),
        None => (MAX_HISTORY_MESSAGES, MAX_DEFERRED_MESSAGES),
    };
    plan_materialization_with_limits(
        timeline,
        messages,
        loop_events,
        max_history_messages,
        max_deferred_messages,
    )
}

/// The limits-taking core of [`plan_materialization`], so tests can exercise
/// the capacity-rejection rules with small ceilings instead of the
/// 10 000 / 1 000 production defaults.
///
/// Capacity rejection (mirrors `push_history` / `append_message` /
/// `flush_deferred_if_exchange_closed`): an append beyond
/// `max_history_messages` is hard-rejected and never occupies a slot (for
/// directly-appended messages, `tool.result`s and `step.begin`s alike — a
/// rejected `step.begin` does not even register the step as open); a
/// message arriving while an exchange is open is rejected once the
/// deferred buffer sits at `max_deferred_messages`; and the deferred flush
/// is truncated at the remaining history capacity (oldest messages win).
fn plan_materialization_with_limits(
    timeline: &[TimelineEntry],
    messages: &[serde_json::Value],
    loop_events: &[serde_json::Value],
    max_history_messages: usize,
    max_deferred_messages: usize,
) -> MaterializationPlan {
    use std::collections::{HashMap, HashSet};

    struct OpenStep {
        slot: usize,
        has_content: bool,
        has_tool_call: bool,
    }

    // The session-cumulative entry number a folded raw record already
    // carries (ADR-007): present only on messages/events retained through
    // an earlier compaction, whose rebuild stamped the number so a later
    // apply's simulation re-derives the same live numbering.
    let stamped = |raw: &serde_json::Value| {
        raw.get("entrySeq")
            .or_else(|| raw.get("entry_seq"))
            .and_then(serde_json::Value::as_u64)
    };

    // Slot bookkeeping for the walk: the next slot id to hand out, the
    // per-slot survival flags (a vacuous step's slot stops surviving), and
    // the count of currently-surviving slots — live `history.len()`, the
    // value the capacity checks compare against.
    struct SlotLedger {
        next_slot: usize,
        surviving: usize,
        slot_survives: Vec<bool>,
        slot_entry_seq: Vec<u64>,
        next_entry_seq: u64,
    }

    impl SlotLedger {
        // One slot materialized — live `push_history` accepting a message.
        // The slot's entry number mirrors the live counter (ADR-007): a
        // stamped (retained-through-compaction) message keeps its number and
        // consumes no fresh one; an unstamped materialization takes the
        // running counter.
        fn create(&mut self, stamped: Option<u64>) -> usize {
            let slot = self.next_slot;
            let entry_seq = match stamped {
                Some(seq) => seq,
                None => {
                    let seq = self.next_entry_seq;
                    self.next_entry_seq += 1;
                    seq
                }
            };
            self.slot_entry_seq.push(entry_seq);
            self.slot_survives.push(true);
            self.next_slot += 1;
            self.surviving += 1;
            slot
        }
    }

    // Pending ids opened by `tool.call` loop events — gates message
    // deferral, exactly like `ContextMemory::pending_tool_result_ids`.
    // A message's own inline `toolCalls` register nothing: live
    // `append_message` adds no pending ids, so a `tool.result` for such
    // an id is dropped without a slot.
    let mut pending: HashSet<String> = HashSet::new();
    // Timeline positions (with their stamped entry numbers) of messages
    // deferred behind an open exchange, awaiting slot assignment at the
    // exchange close.
    let mut deferred: Vec<(usize, Option<u64>)> = Vec::new();
    let mut open_steps: HashMap<String, OpenStep> = HashMap::new();
    // `tool.call` events whose step was not open: they mutate no slot but
    // still open the exchange — back-patched to the slot of the
    // `tool.result` that resolves them so they ride with it.
    let mut orphan_calls: HashMap<String, usize> = HashMap::new();

    let call_id = |raw: &serde_json::Value| {
        raw.get("callId")
            .or_else(|| raw.get("toolCallId"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
    };
    let step_id = |raw: &serde_json::Value| {
        raw.get("uuid")
            .or_else(|| raw.get("stepId"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
    };

    let mut entry_slot: Vec<Option<usize>> = Vec::with_capacity(timeline.len());
    let mut ledger = SlotLedger {
        next_slot: 0,
        surviving: 0,
        slot_survives: Vec::new(),
        // The counter starts past every number the folded state carries
        // (the rebuild of the previous apply stamped them): a session's
        // numbering never resets across compactions (ADR-007), so the
        // first fresh materialization after the retained entries takes the
        // number the live counter held.
        next_entry_seq: messages
            .iter()
            .filter_map(&stamped)
            .map(|seq| seq.saturating_add(1))
            .max()
            .unwrap_or(0),
        slot_entry_seq: Vec::new(),
    };

    // Flush the deferred queue: each message materializes a slot, in
    // arrival (FIFO) order, right after the exchange closed — truncated at
    // the remaining history capacity, exactly like
    // `flush_deferred_if_exchange_closed` (oldest deferred messages win,
    // the overflow is dropped with a warn and never materializes).
    fn flush_deferred(
        deferred: &mut Vec<(usize, Option<u64>)>,
        entry_slot: &mut [Option<usize>],
        ledger: &mut SlotLedger,
        max_history_messages: usize,
    ) {
        let capacity = max_history_messages.saturating_sub(ledger.surviving);
        for (i, (pos, stamped)) in deferred.drain(..).enumerate() {
            if i == capacity {
                break;
            }
            entry_slot[pos] = Some(ledger.create(stamped));
        }
    }

    for entry in timeline {
        let mut slot: Option<usize> = None;
        match entry {
            TimelineEntry::Message { index } => {
                if let Some(raw) = messages.get(*index) {
                    if pending.is_empty() {
                        if ledger.surviving < max_history_messages {
                            slot = Some(ledger.create(stamped(raw)));
                        }
                        // else: live `push_history` rejected the message
                        // (history at the cap) — it never occupied a slot.
                    } else if deferred.len() < max_deferred_messages {
                        // Open tool exchange: live defers the message.
                        deferred.push((entry_slot.len(), stamped(raw)));
                    }
                    // else: live rejected the message (deferred buffer at
                    // the cap) — never stored, never materialized.
                }
            }
            TimelineEntry::LoopEvent { index } => {
                if let Some(raw) = loop_events.get(*index) {
                    let event_type = raw
                        .get("type")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("");
                    match event_type {
                        // The surviving cap gates the whole arm — live
                        // `push_history` rejected the partial assistant
                        // when it is full, so the step is not registered
                        // open and its later events mutate nothing live.
                        "step.begin" if ledger.surviving < max_history_messages => {
                            slot = Some(ledger.create(stamped(raw)));
                            if let Some(id) = step_id(raw) {
                                open_steps.insert(
                                    id,
                                    OpenStep {
                                        slot: slot.unwrap(),
                                        has_content: false,
                                        has_tool_call: false,
                                    },
                                );
                            }
                        }
                        "tool.call" => {
                            if let Some(id) = step_id(raw)
                                && let Some(step) = open_steps.get_mut(&id)
                            {
                                step.has_tool_call = true;
                                slot = Some(step.slot);
                            }
                            if let Some(id) = call_id(raw) {
                                pending.insert(id.clone());
                                if slot.is_none() {
                                    // The call attaches to no open step, but
                                    // it still opens the exchange.
                                    orphan_calls.insert(id, entry_slot.len());
                                }
                            }
                        }
                        "content.part" | "think.part" => {
                            if let Some(id) = step_id(raw)
                                && let Some(step) = open_steps.get_mut(&id)
                            {
                                // Whitespace-only text parts stay vacuous; media parts and TS think
                                // parts (keyed "think") always carry content — mirror of live
                                // `is_vacuous_content_part` (context_memory/mod.rs:962-971).
                                let vacuous = raw.get("part").is_none_or(is_vacuous_part);
                                if !vacuous {
                                    step.has_content = true;
                                }
                                slot = Some(step.slot);
                            }
                        }
                        "tool.result" => {
                            if let Some(id) = call_id(raw) {
                                // Only event-registered calls (tool.call)
                                // materialize a result slot — live
                                // ContextMemory drops results for ids it
                                // never registered and RETURNS EARLY on
                                // that discard (context_memory/
                                // mod.rs:534-537): an orphan result never
                                // reaches live's deferred flush.
                                // Legacy "message inline toolCalls +
                                // tool.result" exchanges must not inflate
                                // total_surviving and offset the
                                // retention-tail boundary. The simulation
                                // runs its flush after the discard anyway,
                                // which is equivalent for live-reachable
                                // logs — pending empty implies the
                                // deferred buffer already drained (the
                                // exchange-closing events flushed it), so
                                // that flush is a no-op — and only a
                                // hand-written log (deferred messages
                                // parked behind an emptied pending set)
                                // can make the simulation flush where
                                // live would not. A resolved id is still
                                // consumed even when the history cap then
                                // rejects the result message: live still
                                // drops the pending id and runs the
                                // deferred flush below.
                                if pending.remove(&id) && ledger.surviving < max_history_messages {
                                    slot = Some(ledger.create(stamped(raw)));
                                    if let Some(pos) = orphan_calls.remove(&id) {
                                        // Ride with the result that
                                        // resolves the orphan call.
                                        entry_slot[pos] = slot;
                                    }
                                }
                            }
                            if pending.is_empty() {
                                flush_deferred(
                                    &mut deferred,
                                    &mut entry_slot,
                                    &mut ledger,
                                    max_history_messages,
                                );
                            }
                        }
                        "step.end" => {
                            if let Some(id) = step_id(raw)
                                && let Some(step) = open_steps.remove(&id)
                            {
                                slot = Some(step.slot);
                                if !step.has_content && !step.has_tool_call {
                                    // Vacuous step: live removed the
                                    // empty partial assistant.
                                    ledger.slot_survives[step.slot] = false;
                                    ledger.surviving -= 1;
                                }
                            }
                            if pending.is_empty() {
                                flush_deferred(
                                    &mut deferred,
                                    &mut entry_slot,
                                    &mut ledger,
                                    max_history_messages,
                                );
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        entry_slot.push(slot);
    }

    // Deferred messages still buffered at the end of the log never
    // materialized (live drops them in `apply_compaction`) — they keep
    // `None` and are excluded from the rebuild.
    MaterializationPlan {
        entry_slot,
        slot_survives: ledger.slot_survives,
        slot_entry_seq: ledger.slot_entry_seq,
        next_entry_seq: ledger.next_entry_seq,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::WireOp;
    use serde_json::json;

    #[test]
    fn should_rebuild_head_elision_summary_and_tail_when_counts_present() {
        // Given a folded conversation: two user turns (each with an
        // assistant step), then an apply op with retention counts
        let mut state = AgentState::new();
        let ops = [
            WireOp::ContextAppendMessage {
                message: serde_json::json!({
                    "role": "user", "content": [{"type": "text", "text": "q1"}],
                    "origin": {"kind": "user"}
                }),
            },
            WireOp::ContextAppendLoopEvent {
                event: serde_json::json!({"type": "step.begin", "uuid": "s1"}),
            },
            WireOp::ContextAppendLoopEvent {
                event: serde_json::json!({
                    "type": "content.part", "uuid": "s1",
                    "part": {"type": "text", "text": "a1"}
                }),
            },
            WireOp::ContextAppendLoopEvent {
                event: serde_json::json!({"type": "step.end", "uuid": "s1"}),
            },
            WireOp::ContextAppendMessage {
                message: serde_json::json!({
                    "role": "user", "content": [{"type": "text", "text": "q2"}],
                    "origin": {"kind": "user"}
                }),
            },
            WireOp::ContextAppendLoopEvent {
                event: serde_json::json!({"type": "step.begin", "uuid": "s2"}),
            },
            WireOp::ContextAppendLoopEvent {
                event: serde_json::json!({
                    "type": "content.part", "uuid": "s2",
                    "part": {"type": "text", "text": "a2"}
                }),
            },
            WireOp::ContextAppendLoopEvent {
                event: serde_json::json!({"type": "step.end", "uuid": "s2"}),
            },
        ];

        // Materialized slots: q1(1) a1(2) q2(3) a2(4) → total 4, tail keeps
        // the last 2 slots (q2 + a2 = timeline entries 4..8), head keeps 1
        // user message (q1).
        let apply = WireOp::ContextApplyCompaction {
            summary: "raw summary".to_string(),
            compacted_count: 2,
            tokens_before: Some(100),
            tokens_after: Some(40),
            context_summary: Some("SUMMARY".to_string()),
            dropped_count: Some(0),
            retained_tail_count: Some(2),
            kept_user_message_count: None,
            kept_head_user_message_count: Some(1),
            elision_message: Some(serde_json::json!({
                "role": "user",
                "content": [{"type": "text", "text": "<system-reminder>elided</system-reminder>"}],
                "origin": {"kind": "injection"}
            })),
            first_replaced_entry_seq: None,
            last_replaced_entry_seq: None,
            dropped_entry_seqs: None,
        };

        for op in ops.iter().chain(std::iter::once(&apply)) {
            let (next, _events) = reduce(&state, op);
            state = next;
        }

        // Then the conversation is rebuilt as [q1][elision][SUMMARY][q2][a2]:
        // `messages` stores only ContextAppendMessage records — q1, the
        // elision marker, the summary, and the tail's appended message q2.
        // a2 is materialized from the tail's loop events, so it lives in
        // `loop_events`, not `messages`.
        let conv = &state.conversation;
        assert_eq!(conv.summary.as_deref(), Some("raw summary"));
        assert_eq!(conv.messages.len(), 4);
        assert_eq!(conv.messages[0]["content"][0]["text"], "q1");
        assert_eq!(conv.messages[1]["origin"]["kind"], "injection");
        assert_eq!(conv.messages[2]["origin"]["kind"], "compaction_summary");
        assert_eq!(conv.messages[2]["content"][0]["text"], "SUMMARY");
        assert_eq!(conv.messages[3]["content"][0]["text"], "q2");
        // loop_events: only the tail's step survived.
        assert_eq!(conv.loop_events.len(), 3);
        assert_eq!(conv.loop_events[0]["uuid"], "s2");
        // timeline: 4 message entries + 3 loop-event entries, full coverage.
        assert_eq!(conv.timeline.len(), 7);
        // And the rebuilt timeline covers every entry exactly once, so a
        // replay reconstructs memory in the rebuilt wire order.
        let mut messages_covered = vec![false; conv.messages.len()];
        let mut events_covered = vec![false; conv.loop_events.len()];
        for entry in &conv.timeline {
            match entry {
                TimelineEntry::Message { index } => {
                    assert!(!messages_covered[*index], "duplicate message index");
                    messages_covered[*index] = true;
                }
                TimelineEntry::LoopEvent { index } => {
                    assert!(!events_covered[*index], "duplicate loop-event index");
                    events_covered[*index] = true;
                }
            }
        }
        assert!(messages_covered.into_iter().all(|covered| covered));
        assert!(events_covered.into_iter().all(|covered| covered));
    }

    #[test]
    fn should_rebuild_legacy_tail_shape_when_ts_apply_record_has_no_kept_counts() {
        // Given a folded conversation of two user/assistant turns and a
        // TS-written apply record {summary, compactedCount: 2} (no
        // keptUserMessageCount, no retainedTailCount — the TS legacy
        // dialect, detected by keptUserMessageCount absence,
        // transcript.ts:267)
        let mut state = AgentState::new();
        let ops = [
            WireOp::ContextAppendMessage {
                message: serde_json::json!({
                    "role": "user", "content": [{"type": "text", "text": "q1"}],
                    "origin": {"kind": "user"}
                }),
            },
            WireOp::ContextAppendLoopEvent {
                event: serde_json::json!({"type": "step.begin", "uuid": "s1"}),
            },
            WireOp::ContextAppendLoopEvent {
                event: serde_json::json!({
                    "type": "content.part", "uuid": "s1",
                    "part": {"type": "text", "text": "a1"}
                }),
            },
            WireOp::ContextAppendLoopEvent {
                event: serde_json::json!({"type": "step.end", "uuid": "s1"}),
            },
            WireOp::ContextAppendMessage {
                message: serde_json::json!({
                    "role": "user", "content": [{"type": "text", "text": "q2"}],
                    "origin": {"kind": "user"}
                }),
            },
            WireOp::ContextAppendLoopEvent {
                event: serde_json::json!({"type": "step.begin", "uuid": "s2"}),
            },
            WireOp::ContextAppendLoopEvent {
                event: serde_json::json!({
                    "type": "content.part", "uuid": "s2",
                    "part": {"type": "text", "text": "a2"}
                }),
            },
            WireOp::ContextAppendLoopEvent {
                event: serde_json::json!({"type": "step.end", "uuid": "s2"}),
            },
        ];

        // Materialized slots: q1(0) a1(1) q2(2) a2(3) → compactedCount 2
        // folds away q1 and a1, keeping the history from index 2 on.
        let apply = WireOp::ContextApplyCompaction {
            summary: "SUMMARY".to_string(),
            compacted_count: 2,
            tokens_before: Some(1000),
            tokens_after: Some(50),
            context_summary: None,
            dropped_count: None,
            retained_tail_count: None,
            kept_user_message_count: None,
            kept_head_user_message_count: None,
            elision_message: None,
            first_replaced_entry_seq: None,
            last_replaced_entry_seq: None,
            dropped_entry_seqs: None,
        };

        for op in ops.iter().chain(std::iter::once(&apply)) {
            let (next, _events) = reduce(&state, op);
            state = next;
        }

        // Then the rebuilt messages are [summary-user-message(origin
        // compaction_summary), ...folded messages from index 2 onward]
        // (v1/v2 legacy semantics — summary FIRST, verbatim tail incl.
        // assistant/tool messages): the tail keeps q2's message and the
        // whole a2 step (its loop events survive verbatim).
        let conv = &state.conversation;
        assert_eq!(conv.summary.as_deref(), Some("SUMMARY"));
        assert_eq!(conv.messages.len(), 2);
        assert_eq!(conv.messages[0]["origin"]["kind"], "compaction_summary");
        assert_eq!(conv.messages[0]["content"][0]["text"], "SUMMARY");
        assert_eq!(conv.messages[1]["content"][0]["text"], "q2");
        // loop_events: only the a2 step's events survived the tail cut.
        assert_eq!(conv.loop_events.len(), 3);
        assert_eq!(conv.loop_events[0]["uuid"], "s2");
        assert_eq!(conv.loop_events[1]["part"]["text"], "a2");
        // timeline: 2 message entries + 3 loop-event entries, full coverage.
        assert_eq!(conv.timeline.len(), 5);
        let mut messages_covered = vec![false; conv.messages.len()];
        let mut events_covered = vec![false; conv.loop_events.len()];
        for entry in &conv.timeline {
            match entry {
                TimelineEntry::Message { index } => {
                    assert!(!messages_covered[*index], "duplicate message index");
                    messages_covered[*index] = true;
                }
                TimelineEntry::LoopEvent { index } => {
                    assert!(!events_covered[*index], "duplicate loop-event index");
                    events_covered[*index] = true;
                }
            }
        }
        assert!(messages_covered.into_iter().all(|covered| covered));
        assert!(events_covered.into_iter().all(|covered| covered));
    }

    #[test]
    fn should_rebuild_summary_alone_when_ts_legacy_compacted_count_covers_history() {
        // Given a TS legacy apply record whose compactedCount covers the
        // whole folded history (the complement of transcript.ts:273's
        // `compactedCount < length` guard — the tail slice is empty)
        let op = WireOp::ContextApplyCompaction {
            summary: "s".to_string(),
            compacted_count: 3,
            tokens_before: Some(1000),
            tokens_after: Some(500),
            context_summary: None,
            dropped_count: None,
            retained_tail_count: None,
            kept_user_message_count: None,
            kept_head_user_message_count: None,
            elision_message: None,
            first_replaced_entry_seq: None,
            last_replaced_entry_seq: None,
            dropped_entry_seqs: None,
        };

        // And a state with prior content
        let mut state = AgentState::new();
        state.conversation.messages.push(serde_json::json!({
            "role": "user", "content": "hello"
        }));
        state
            .conversation
            .timeline
            .push(TimelineEntry::Message { index: 0 });

        // When folded
        let (state, _events) = reduce(&state, &op);

        // Then the rebuild is the summary alone: [summary], nothing else
        assert_eq!(state.conversation.messages.len(), 1);
        assert_eq!(
            state.conversation.messages[0]["origin"]["kind"],
            "compaction_summary"
        );
        assert_eq!(state.conversation.messages[0]["content"][0]["text"], "s");
        assert!(state.conversation.loop_events.is_empty());
        assert_eq!(state.conversation.timeline.len(), 1);
    }

    #[test]
    fn should_rebuild_ts_newer_shape_when_kept_user_message_count_present() {
        // Given three user/assistant turns and an apply record carrying
        // keptUserMessageCount 2 + keptHeadUserMessageCount 1 (the TS v2
        // live record — v2 always writes keptUserMessageCount)
        let mut state = AgentState::new();
        let mut ops = vec![];
        for turn in 1..=3 {
            ops.push(WireOp::ContextAppendMessage {
                message: serde_json::json!({
                    "role": "user",
                    "content": [{"type": "text", "text": format!("q{turn}")}],
                    "origin": {"kind": "user"}
                }),
            });
            ops.push(WireOp::ContextAppendLoopEvent {
                event: serde_json::json!({"type": "step.begin", "uuid": format!("s{turn}")}),
            });
            ops.push(WireOp::ContextAppendLoopEvent {
                event: serde_json::json!({
                    "type": "content.part", "uuid": format!("s{turn}"),
                    "part": {"type": "text", "text": format!("a{turn}")}
                }),
            });
            ops.push(WireOp::ContextAppendLoopEvent {
                event: serde_json::json!({"type": "step.end", "uuid": format!("s{turn}")}),
            });
        }

        let apply = WireOp::ContextApplyCompaction {
            summary: "SUMMARY".to_string(),
            compacted_count: 4,
            tokens_before: Some(10_000),
            tokens_after: Some(100),
            context_summary: None,
            dropped_count: None,
            retained_tail_count: None,
            kept_user_message_count: Some(2),
            kept_head_user_message_count: Some(1),
            elision_message: None,
            first_replaced_entry_seq: None,
            last_replaced_entry_seq: None,
            dropped_entry_seqs: None,
        };

        for op in ops.iter().chain(std::iter::once(&apply)) {
            let (next, _events) = reduce(&state, op);
            state = next;
        }

        // Then the rebuilt messages keep the LAST keptUserMessageCount
        // compactable user messages ({q2, q3}; head = keptHeadUserMessageCount
        // oldest of them = q2), with an elision marker between the segments
        // and the summary user-message LAST — matching TS v2
        // buildContextCompactionShape (compactionHandoff.ts:72-98). The
        // assistant steps are folded into the summary and dropped.
        let conv = &state.conversation;
        assert_eq!(conv.messages.len(), 4);
        assert_eq!(conv.messages[0]["content"][0]["text"], "q2");
        assert_eq!(conv.messages[1]["origin"]["kind"], "injection");
        assert_eq!(conv.messages[1]["origin"]["variant"], "compaction_elision");
        assert!(
            conv.messages[1]["content"][0]["text"]
                .as_str()
                .unwrap()
                .starts_with("<system-reminder>")
        );
        assert_eq!(conv.messages[2]["content"][0]["text"], "q3");
        assert_eq!(conv.messages[3]["origin"]["kind"], "compaction_summary");
        assert_eq!(conv.messages[3]["content"][0]["text"], "SUMMARY");
        // Every assistant step was compacted away — no loop events survive.
        assert!(conv.loop_events.is_empty());
        assert_eq!(conv.timeline.len(), 4);
        for entry in &conv.timeline {
            assert!(matches!(entry, TimelineEntry::Message { .. }));
        }
    }

    #[test]
    fn should_rebuild_ts_newer_shape_without_elision_when_kept_head_count_absent() {
        // Given an apply record carrying keptUserMessageCount but no
        // keptHeadUserMessageCount: the selection did not split, so no
        // elision marker rides between the segments (the kept + 1 count
        // derivation, transcript.ts:271-272)
        let mut state = AgentState::new();
        let mut ops = vec![];
        for turn in 1..=3 {
            ops.push(WireOp::ContextAppendMessage {
                message: serde_json::json!({
                    "role": "user",
                    "content": [{"type": "text", "text": format!("q{turn}")}],
                    "origin": {"kind": "user"}
                }),
            });
            ops.push(WireOp::ContextAppendLoopEvent {
                event: serde_json::json!({"type": "step.begin", "uuid": format!("s{turn}")}),
            });
            ops.push(WireOp::ContextAppendLoopEvent {
                event: serde_json::json!({
                    "type": "content.part", "uuid": format!("s{turn}"),
                    "part": {"type": "text", "text": format!("a{turn}")}
                }),
            });
            ops.push(WireOp::ContextAppendLoopEvent {
                event: serde_json::json!({"type": "step.end", "uuid": format!("s{turn}")}),
            });
        }

        let apply = WireOp::ContextApplyCompaction {
            summary: "SUMMARY".to_string(),
            compacted_count: 4,
            tokens_before: None,
            tokens_after: None,
            context_summary: None,
            dropped_count: None,
            retained_tail_count: None,
            kept_user_message_count: Some(2),
            kept_head_user_message_count: None,
            elision_message: None,
            first_replaced_entry_seq: None,
            last_replaced_entry_seq: None,
            dropped_entry_seqs: None,
        };

        for op in ops.iter().chain(std::iter::once(&apply)) {
            let (next, _events) = reduce(&state, op);
            state = next;
        }

        // Then the rebuilt messages are all tail: [q2][q3][summary], no
        // elision marker, summary LAST
        let conv = &state.conversation;
        assert_eq!(conv.messages.len(), 3);
        assert_eq!(conv.messages[0]["content"][0]["text"], "q2");
        assert_eq!(conv.messages[1]["content"][0]["text"], "q3");
        assert_eq!(conv.messages[2]["origin"]["kind"], "compaction_summary");
        assert!(
            conv.messages
                .iter()
                .all(|m| m["origin"]["kind"] != "injection")
        );
        assert!(conv.loop_events.is_empty());
    }

    #[test]
    fn should_clear_everything_when_retained_tail_count_is_zero() {
        // Given a new-style apply op whose tail retention is zero — nothing
        // of the old context survives, not even the head essence
        let mut state = AgentState::new();
        state.conversation.messages.push(serde_json::json!({
            "role": "user", "content": "q1", "origin": {"kind": "user"}
        }));
        state
            .conversation
            .timeline
            .push(TimelineEntry::Message { index: 0 });
        let op = WireOp::ContextApplyCompaction {
            summary: "s".to_string(),
            compacted_count: 1,
            tokens_before: None,
            tokens_after: None,
            context_summary: None,
            dropped_count: None,
            retained_tail_count: Some(0),
            kept_user_message_count: None,
            kept_head_user_message_count: Some(1),
            elision_message: None,
            first_replaced_entry_seq: None,
            last_replaced_entry_seq: None,
            dropped_entry_seqs: None,
        };

        // When folded
        let (state, _events) = reduce(&state, &op);

        // Then the conversation is cleared (zero tail = full compaction)
        assert!(state.conversation.messages.is_empty());
        assert!(state.conversation.loop_events.is_empty());
        assert!(state.conversation.timeline.is_empty());
    }

    #[test]
    fn should_clear_everything_when_tail_count_exceeds_folded_slots() {
        // Given a corrupt new-style op whose retained tail exceeds the
        // folded history's slot count — reconstruction is impossible, so
        // the reducer falls back to the legacy clear rather than keeping a
        // half-rebuilt context
        let mut state = AgentState::new();
        state.conversation.messages.push(serde_json::json!({
            "role": "user", "content": "q1", "origin": {"kind": "user"}
        }));
        state
            .conversation
            .timeline
            .push(TimelineEntry::Message { index: 0 });
        let op = WireOp::ContextApplyCompaction {
            summary: "s".to_string(),
            compacted_count: 1,
            tokens_before: None,
            tokens_after: None,
            context_summary: None,
            dropped_count: None,
            retained_tail_count: Some(5),
            kept_user_message_count: None,
            kept_head_user_message_count: Some(1),
            elision_message: None,
            first_replaced_entry_seq: None,
            last_replaced_entry_seq: None,
            dropped_entry_seqs: None,
        };

        // When folded
        let (state, _events) = reduce(&state, &op);

        // Then the legacy fallback holds: cleared
        assert!(state.conversation.messages.is_empty());
        assert!(state.conversation.timeline.is_empty());
    }

    #[test]
    fn should_drop_tool_result_slots_when_calls_come_from_legacy_message_tool_calls() {
        // Given a legacy TS exchange: an assistant message carrying inline
        // toolCalls, followed by a tool.result event (no tool.call event)
        // — live ContextMemory registers pending ids only from
        // LoopEvent::ToolCall and drops results for unknown ids
        // (context_memory/mod.rs:506-509), so a message's own toolCalls
        // register nothing
        let messages = vec![serde_json::json!({
            "role": "assistant",
            "content": [],
            "toolCalls": [{"id": "call-1", "function": {"name": "Read"}}]
        })];
        let loop_events = vec![
            serde_json::json!({"type": "step.begin", "uuid": "s1"}),
            serde_json::json!({
                "type": "tool.result", "uuid": "s1", "callId": "call-1",
                "result": {"output": []}
            }),
            serde_json::json!({"type": "step.end", "uuid": "s1"}),
        ];
        let timeline = vec![
            TimelineEntry::Message { index: 0 },
            TimelineEntry::LoopEvent { index: 0 },
            TimelineEntry::LoopEvent { index: 1 },
            TimelineEntry::LoopEvent { index: 2 },
        ];

        // When the slot boundaries are simulated
        let plan = plan_materialization(&timeline, &messages, &loop_events, None);

        // Then the tool.result does NOT materialize a slot and does NOT
        // inflate total_surviving (no tail boundary offset): the message
        // (slot 0) and step.begin (slot 1) are the only materializations,
        // and the step is vacuous at step.end (no content part, no event
        // tool call — the inline toolCalls belong to the message, which
        // live never attaches to the step), so only the message survives.
        assert_eq!(plan.entry_slot, vec![Some(0), Some(1), None, Some(1)]);
        assert_eq!(plan.slot_survives, vec![true, false]);
    }

    #[test]
    fn should_not_count_tool_result_when_no_pending_call_id_matches() {
        // Given a tool.result whose call id was never registered (a
        // replay-only orphan — ContextMemory drops it without a slot)
        let loop_events = vec![
            serde_json::json!({"type": "step.begin", "uuid": "s1"}),
            serde_json::json!({
                "type": "tool.result", "uuid": "s1", "callId": "orphan",
                "result": {"output": []}
            }),
        ];
        let timeline = vec![
            TimelineEntry::LoopEvent { index: 0 },
            TimelineEntry::LoopEvent { index: 1 },
        ];

        // When the slot boundaries are simulated
        let plan = plan_materialization(&timeline, &[], &loop_events, None);

        // Then the orphan result materializes no slot: only step.begin's
        // slot exists.
        assert_eq!(plan.entry_slot, vec![Some(0), None]);
        assert_eq!(plan.slot_survives, vec![true]);
    }

    #[test]
    fn should_remove_vacuous_step_slot_when_step_end_sees_no_content_or_calls() {
        // Given a vacuous step: step.begin, a whitespace-only content part,
        // step.end — live removed the empty partial assistant, so the slot
        // it tentatively opened does not survive
        let loop_events = vec![
            serde_json::json!({"type": "step.begin", "uuid": "s1"}),
            serde_json::json!({
                "type": "content.part", "uuid": "s1",
                "part": {"type": "text", "text": "   "}
            }),
            serde_json::json!({"type": "step.end", "uuid": "s1"}),
        ];
        let timeline = vec![
            TimelineEntry::LoopEvent { index: 0 },
            TimelineEntry::LoopEvent { index: 1 },
            TimelineEntry::LoopEvent { index: 2 },
        ];

        // When the slot boundaries are simulated
        let plan = plan_materialization(&timeline, &[], &loop_events, None);

        // Then the step's slot is marked removed: a later compaction
        // counting live slots must not count it, and the rebuild must
        // exclude both of its entries.
        assert_eq!(plan.entry_slot, vec![Some(0), Some(0), Some(0)]);
        assert_eq!(plan.slot_survives, vec![false]);
    }

    #[test]
    fn should_keep_step_alive_when_ts_media_part_arrives_without_text_field() {
        // Given a step whose only content part is TS-shaped media (no "text"
        // key — media parts always carry content, live `is_vacuous_content_part`)
        let loop_events = vec![
            serde_json::json!({"type": "step.begin", "uuid": "s1"}),
            serde_json::json!({
                "type": "content.part", "uuid": "s1",
                "part": {"type": "image", "imageUrl": "blobref:image/png;abc"}
            }),
            serde_json::json!({"type": "step.end", "uuid": "s1"}),
        ];
        let timeline = vec![
            TimelineEntry::LoopEvent { index: 0 },
            TimelineEntry::LoopEvent { index: 1 },
            TimelineEntry::LoopEvent { index: 2 },
        ];

        // When the slot boundaries are simulated
        let plan = plan_materialization(&timeline, &[], &loop_events, None);

        // Then the step survives: its slot counts as live content
        assert_eq!(plan.entry_slot, vec![Some(0), Some(0), Some(0)]);
        assert_eq!(plan.slot_survives, vec![true]);
    }

    #[test]
    fn should_keep_step_alive_when_ts_think_part_uses_think_field() {
        // Given a TS think part keyed by "think" instead of "text"
        let loop_events = vec![
            serde_json::json!({"type": "step.begin", "uuid": "s1"}),
            serde_json::json!({
                "type": "think.part", "uuid": "s1",
                "part": {"type": "think", "think": "reasoning"}
            }),
            serde_json::json!({"type": "step.end", "uuid": "s1"}),
        ];
        let timeline = vec![
            TimelineEntry::LoopEvent { index: 0 },
            TimelineEntry::LoopEvent { index: 1 },
            TimelineEntry::LoopEvent { index: 2 },
        ];

        // When the fold runs
        let plan = plan_materialization(&timeline, &[], &loop_events, None);

        // Then the step survives — only whitespace-only text can be vacuous
        assert_eq!(plan.entry_slot, vec![Some(0), Some(0), Some(0)]);
        assert_eq!(plan.slot_survives, vec![true]);
    }

    #[test]
    fn should_keep_step_alive_when_think_part_falls_back_to_the_text_key() {
        // Given a think block whose content is plain text under the "text"
        // key — the rust dialect's own wire spelling for a think part
        // (LoopEvent::ThinkPart serialises `{"type": "think", "text": ...}`,
        // context_memory/mod.rs:133) — where the reducer's think handling
        // reads `think` first and falls back to `text`
        let loop_events = vec![
            serde_json::json!({"type": "step.begin", "uuid": "s1"}),
            serde_json::json!({
                "type": "think.part", "uuid": "s1",
                "part": {"type": "think", "text": "reasoning"}
            }),
            serde_json::json!({"type": "step.end", "uuid": "s1"}),
        ];
        let timeline = vec![
            TimelineEntry::LoopEvent { index: 0 },
            TimelineEntry::LoopEvent { index: 1 },
            TimelineEntry::LoopEvent { index: 2 },
        ];

        // When the fold runs
        let plan = plan_materialization(&timeline, &[], &loop_events, None);

        // Then the step survives — the text-key fallback carries real
        // content, so the step is not vacuous
        assert_eq!(plan.entry_slot, vec![Some(0), Some(0), Some(0)]);
        assert_eq!(plan.slot_survives, vec![true]);
    }

    #[test]
    fn should_remove_vacuous_step_when_think_part_falls_back_to_whitespace_text() {
        // Given a think block whose text-key fallback content is
        // whitespace-only — the fallback trims like the "think" key does
        let loop_events = vec![
            serde_json::json!({"type": "step.begin", "uuid": "s1"}),
            serde_json::json!({
                "type": "think.part", "uuid": "s1",
                "part": {"type": "think", "text": "   "}
            }),
            serde_json::json!({"type": "step.end", "uuid": "s1"}),
        ];
        let timeline = vec![
            TimelineEntry::LoopEvent { index: 0 },
            TimelineEntry::LoopEvent { index: 1 },
            TimelineEntry::LoopEvent { index: 2 },
        ];

        // When the fold runs
        let plan = plan_materialization(&timeline, &[], &loop_events, None);

        // Then the step is vacuous: its slot is removed, exactly like the
        // "think"-keyed whitespace case live `is_vacuous_content_part`
        // drops (context_memory/mod.rs:1036-1045)
        assert_eq!(plan.entry_slot, vec![Some(0), Some(0), Some(0)]);
        assert_eq!(plan.slot_survives, vec![false]);
    }

    #[test]
    fn should_assign_deferred_message_slot_at_exchange_close_not_at_arrival() {
        // Given a message appended while a tool exchange is open: the
        // wire position is between tool.call and tool.result, but live
        // defers it behind the resolving tool result
        let messages = vec![
            serde_json::json!({
                "role": "user", "content": [{"type": "text", "text": "q"}],
                "origin": {"kind": "user"}
            }),
            serde_json::json!({
                "role": "user", "content": [{"type": "text", "text": "late"}],
                "origin": {"kind": "user"}
            }),
        ];
        let loop_events = vec![
            serde_json::json!({"type": "step.begin", "uuid": "s1"}),
            serde_json::json!({
                "type": "tool.call", "uuid": "s1", "callId": "call-1",
                "name": "Read", "input": {}
            }),
            serde_json::json!({
                "type": "tool.result", "uuid": "s1", "callId": "call-1",
                "result": {"output": []}
            }),
        ];
        let timeline = vec![
            TimelineEntry::Message { index: 0 },
            TimelineEntry::LoopEvent { index: 0 },
            TimelineEntry::LoopEvent { index: 1 },
            TimelineEntry::Message { index: 1 },
            TimelineEntry::LoopEvent { index: 2 },
        ];

        // When the slot boundaries are simulated
        let plan = plan_materialization(&timeline, &messages, &loop_events, None);

        // Then the deferred message's slot (3) comes after the tool
        // result's (2), not at its wire-arrival position — matching the
        // live history order [q, assistant, tool result, late].
        assert_eq!(
            plan.entry_slot,
            vec![Some(0), Some(1), Some(1), Some(3), Some(2)]
        );
        assert_eq!(plan.slot_survives, vec![true, true, true, true]);
    }

    #[test]
    fn should_backpatch_orphan_tool_call_to_its_resolving_result_slot() {
        // Given a tool.call for a step that is not open: it mutates no
        // slot but still opens the exchange, so its tool.result
        // materializes one
        let loop_events = vec![
            serde_json::json!({
                "type": "tool.call", "uuid": "unknown-step", "callId": "call-1",
                "name": "Read", "input": {}
            }),
            serde_json::json!({
                "type": "tool.result", "uuid": "unknown-step", "callId": "call-1",
                "result": {"output": []}
            }),
        ];
        let timeline = vec![
            TimelineEntry::LoopEvent { index: 0 },
            TimelineEntry::LoopEvent { index: 1 },
        ];

        // When the slot boundaries are simulated
        let plan = plan_materialization(&timeline, &[], &loop_events, None);

        // Then the orphan call rides with the slot of the tool result
        // that resolves it (so a replay registers the pending id before
        // the result arrives), and exactly one slot exists.
        assert_eq!(plan.entry_slot, vec![Some(0), Some(0)]);
        assert_eq!(plan.slot_survives, vec![true]);
    }

    #[test]
    fn should_reject_message_slot_when_history_cap_is_reached() {
        // Given three directly-appended messages and a history cap of two
        let messages: Vec<_> = (0..3)
            .map(|i| {
                serde_json::json!({
                    "role": "user",
                    "content": [{"type": "text", "text": format!("q{i}")}],
                    "origin": {"kind": "user"}
                })
            })
            .collect();
        let timeline = vec![
            TimelineEntry::Message { index: 0 },
            TimelineEntry::Message { index: 1 },
            TimelineEntry::Message { index: 2 },
        ];

        // When the slot boundaries are simulated with the cap
        let plan = plan_materialization_with_limits(&timeline, &messages, &[], 2, 1_000);

        // Then only the first two messages materialized slots — the third
        // was hard-rejected by the live history cap (`push_history`), so it
        // must not inflate the simulated slot count.
        assert_eq!(plan.entry_slot, vec![Some(0), Some(1), None]);
        assert_eq!(plan.slot_survives, vec![true, true]);
    }

    #[test]
    fn should_reject_deferred_message_when_deferred_cap_is_reached() {
        // Given a tool exchange with two messages arriving while it is
        // open, and a deferred cap of one: live buffered the first and
        // hard-rejected the second (never stored)
        let messages = vec![
            serde_json::json!({
                "role": "user",
                "content": [{"type": "text", "text": "q"}],
                "origin": {"kind": "user"}
            }),
            serde_json::json!({
                "role": "user",
                "content": [{"type": "text", "text": "late1"}],
                "origin": {"kind": "user"}
            }),
            serde_json::json!({
                "role": "user",
                "content": [{"type": "text", "text": "late2"}],
                "origin": {"kind": "user"}
            }),
        ];
        let loop_events = vec![
            serde_json::json!({"type": "step.begin", "uuid": "s1"}),
            serde_json::json!({
                "type": "tool.call", "uuid": "s1", "callId": "call-1",
                "name": "Read", "input": {}
            }),
            serde_json::json!({
                "type": "tool.result", "uuid": "s1", "callId": "call-1",
                "result": {"output": []}
            }),
        ];
        let timeline = vec![
            TimelineEntry::Message { index: 0 },
            TimelineEntry::LoopEvent { index: 0 },
            TimelineEntry::LoopEvent { index: 1 },
            TimelineEntry::Message { index: 1 },
            TimelineEntry::Message { index: 2 },
            TimelineEntry::LoopEvent { index: 2 },
        ];

        // When the slot boundaries are simulated with the deferred cap
        let plan = plan_materialization_with_limits(&timeline, &messages, &loop_events, 10_000, 1);

        // Then the first late message materialized a slot at the exchange
        // close (after the tool result), while the second — rejected at the
        // deferred cap — never did: [q(0), assistant(1), tool result(2),
        // late1(3)] and no phantom slot for late2.
        assert_eq!(
            plan.entry_slot,
            vec![Some(0), Some(1), Some(1), Some(3), None, Some(2)]
        );
        assert_eq!(plan.slot_survives, vec![true, true, true, true]);
    }

    #[test]
    fn should_truncate_deferred_flush_when_history_cap_is_reached() {
        // Given an exchange closing over a full history: two deferred
        // messages queued behind it, but the history cap leaves no room at
        // flush time — live drops the overflow (`flush_deferred_if_
        // exchange_closed`, oldest win)
        let messages = vec![
            serde_json::json!({
                "role": "user",
                "content": [{"type": "text", "text": "q"}],
                "origin": {"kind": "user"}
            }),
            serde_json::json!({
                "role": "user",
                "content": [{"type": "text", "text": "late1"}],
                "origin": {"kind": "user"}
            }),
            serde_json::json!({
                "role": "user",
                "content": [{"type": "text", "text": "late2"}],
                "origin": {"kind": "user"}
            }),
        ];
        let loop_events = vec![
            serde_json::json!({"type": "step.begin", "uuid": "s1"}),
            serde_json::json!({
                "type": "tool.call", "uuid": "s1", "callId": "call-1",
                "name": "Read", "input": {}
            }),
            serde_json::json!({
                "type": "tool.result", "uuid": "s1", "callId": "call-1",
                "result": {"output": []}
            }),
            serde_json::json!({"type": "step.end", "uuid": "s1"}),
        ];
        let timeline = vec![
            TimelineEntry::Message { index: 0 },
            TimelineEntry::LoopEvent { index: 0 },
            TimelineEntry::LoopEvent { index: 1 },
            TimelineEntry::Message { index: 1 },
            TimelineEntry::Message { index: 2 },
            TimelineEntry::LoopEvent { index: 2 },
            TimelineEntry::LoopEvent { index: 3 },
        ];

        // When the slot boundaries are simulated with a history cap of
        // three — exactly what the message, the step and the tool result
        // already occupy
        let plan = plan_materialization_with_limits(&timeline, &messages, &loop_events, 3, 1_000);

        // Then both deferred messages are dropped at the flush (capacity
        // zero): neither materializes a slot, so the simulated history
        // stays at the live three.
        assert_eq!(
            plan.entry_slot,
            vec![Some(0), Some(1), Some(1), None, None, Some(2), Some(1)]
        );
        assert_eq!(plan.slot_survives, vec![true, true, true]);
    }

    #[test]
    fn should_consume_pending_id_without_slot_when_tool_result_hits_history_cap() {
        // Given a tool.result arriving against a full history: live still
        // consumes the pending id and flushes deferred messages, but the
        // result message itself is rejected (`push_history`) — no slot
        let messages = vec![
            serde_json::json!({
                "role": "user",
                "content": [{"type": "text", "text": "q"}],
                "origin": {"kind": "user"}
            }),
            serde_json::json!({
                "role": "user",
                "content": [{"type": "text", "text": "late"}],
                "origin": {"kind": "user"}
            }),
        ];
        let loop_events = vec![
            serde_json::json!({"type": "step.begin", "uuid": "s1"}),
            serde_json::json!({
                "type": "tool.call", "uuid": "s1", "callId": "call-1",
                "name": "Read", "input": {}
            }),
            serde_json::json!({
                "type": "tool.result", "uuid": "s1", "callId": "call-1",
                "result": {"output": []}
            }),
        ];
        let timeline = vec![
            TimelineEntry::Message { index: 0 },
            TimelineEntry::LoopEvent { index: 0 },
            TimelineEntry::LoopEvent { index: 1 },
            TimelineEntry::Message { index: 1 },
            TimelineEntry::LoopEvent { index: 2 },
        ];

        // When the slot boundaries are simulated with a history cap of
        // two — exactly what the message and the step already occupy
        let plan = plan_materialization_with_limits(&timeline, &messages, &loop_events, 2, 1_000);

        // Then the tool result gets no slot, but the pending id it consumed
        // still released the deferred message — which then hits the same
        // full history and materializes nothing either.
        assert_eq!(plan.entry_slot, vec![Some(0), Some(1), Some(1), None, None]);
        assert_eq!(plan.slot_survives, vec![true, true]);
    }

    #[test]
    fn should_not_open_step_slot_when_history_cap_is_reached() {
        // Given a step.begin arriving against a full history: live rejected
        // the partial assistant and did not register the step open, so its
        // content part mutates nothing
        let messages = vec![serde_json::json!({
            "role": "user",
            "content": [{"type": "text", "text": "q"}],
            "origin": {"kind": "user"}
        })];
        let loop_events = vec![
            serde_json::json!({"type": "step.begin", "uuid": "s1"}),
            serde_json::json!({
                "type": "content.part", "uuid": "s1",
                "part": {"type": "text", "text": "answer"}
            }),
            serde_json::json!({"type": "step.end", "uuid": "s1"}),
        ];
        let timeline = vec![
            TimelineEntry::Message { index: 0 },
            TimelineEntry::LoopEvent { index: 0 },
            TimelineEntry::LoopEvent { index: 1 },
            TimelineEntry::LoopEvent { index: 2 },
        ];

        // When the slot boundaries are simulated with a history cap of one
        let plan = plan_materialization_with_limits(&timeline, &messages, &loop_events, 1, 1_000);

        // Then the step's three entries all carry no slot — no phantom
        // partial assistant inflates the simulated history.
        assert_eq!(plan.entry_slot, vec![Some(0), None, None, None]);
        assert_eq!(plan.slot_survives, vec![true]);
    }
    /// should fold subagent.registered ops into the state subagents
    /// registry (B4 T8, D-55)
    #[test]
    fn should_fold_subagent_registered_ops_into_the_state_subagents_registry() {
        // Given a fresh state and two registrations (one re-spawned)
        let mut state = AgentState::new();
        let first = WireOp::SubagentRegistered {
            agent_id: "agent-one".to_string(),
            profile_name: "coder".to_string(),
            model: Some("sonnet".to_string()),
            thinking_effort: Some("high".to_string()),
            parent_agent_id: Some("main".to_string()),
            swarm_item: None,
            parent_tool_call_id: Some("call-1".to_string()),
            swarm_index: None,
        };
        state = reduce(&state, &first).0;
        state = reduce(
            &state,
            &WireOp::SubagentRegistered {
                agent_id: "agent-two".to_string(),
                profile_name: "reviewer".to_string(),
                model: None,
                thinking_effort: None,
                parent_agent_id: Some("agent-one".to_string()),
                swarm_item: Some("batch-a".to_string()),
                parent_tool_call_id: None,
                swarm_index: Some(1),
            },
        )
        .0;
        // A re-registration of agent-one upserts in place (latest wins)
        state = reduce(
            &state,
            &WireOp::SubagentRegistered {
                agent_id: "agent-one".to_string(),
                profile_name: "coder-v2".to_string(),
                model: Some("opus".to_string()),
                thinking_effort: None,
                parent_agent_id: Some("main".to_string()),
                swarm_item: None,
                parent_tool_call_id: None,
                swarm_index: None,
            },
        )
        .0;

        // Then the registry holds both agents, keyed and upserted
        assert_eq!(state.subagents.len(), 2);
        let one = state
            .subagents
            .iter()
            .find(|r| r.agent_id == "agent-one")
            .expect("agent-one registered");
        assert_eq!(one.profile_name, "coder-v2");
        assert_eq!(one.model.as_deref(), Some("opus"));
        let two = state
            .subagents
            .iter()
            .find(|r| r.agent_id == "agent-two")
            .expect("agent-two registered");
        assert_eq!(two.profile_name, "reviewer");
        assert_eq!(two.parent_agent_id.as_deref(), Some("agent-one"));
        assert_eq!(two.swarm_item.as_deref(), Some("batch-a"));

        // And the op survives the serde round-trip (camelCase wire fields)
        let value = serde_json::to_value(&first).unwrap();
        assert_eq!(value["type"], "subagent.registered");
        assert_eq!(value["agentId"], "agent-one");
        assert_eq!(value["profileName"], "coder");
        assert_eq!(value["parentAgentId"], "main");
        let back: WireOp = serde_json::from_value(value).unwrap();
        assert!(matches!(back, WireOp::SubagentRegistered { .. }));
    }

    // ── Interruption / swarm state folds (context-injection wiring, T3) ──

    /// Deserialize a wire-tagged op record into a [`WireOp`] — the same
    /// path a replayed `wire.jsonl` line takes through the reducer, so
    /// the tests also pin the wire tag and camelCase field spellings.
    fn op_json(kind: &str, payload: serde_json::Value) -> WireOp {
        let mut record = payload;
        record
            .as_object_mut()
            .expect("op payload must be a JSON object")
            .insert(
                "type".to_string(),
                serde_json::Value::String(kind.to_string()),
            );
        serde_json::from_value(record).expect("valid wire op record")
    }

    /// A plain conversation message in the `context.append_message`
    /// record shape the reducer stores verbatim.
    fn message_json(role: &str, text: &str) -> serde_json::Value {
        serde_json::json!({
            "role": role,
            "content": [{"type": "text", "text": text}],
            "origin": {"kind": role},
        })
    }

    /// The swarm enter reminder as the live side writes it: an
    /// injection-origin user message whose text opens with the reminder's
    /// leading heading (`enter-reminder.md` starts `## Swarm Mode`).
    fn injection_message_json(text: &str) -> serde_json::Value {
        serde_json::json!({
            "role": "user",
            "content": [{"type": "text", "text": text}],
            "origin": {"kind": "injection", "variant": "swarm_mode"},
        })
    }

    #[test]
    fn should_track_pending_interruption_when_turn_cancelled_by_user() {
        let mut state = AgentState::new();
        let (next, events) = reduce(
            &state,
            &op_json(
                "turn.cancel",
                serde_json::json!({
                    "turnId": 7, "target": "active", "reason": "user_cancelled"
                }),
            ),
        );
        state = next;
        // The pending fold is additive: the existing TurnCancelled event
        // keeps flowing (reducer contract for turn.cancel with a turn id).
        assert!(
            events
                .iter()
                .any(|event| matches!(event, DomainEvent::TurnCancelled { turn_id: 7, .. }))
        );
        assert_eq!(state.interruption_pending, vec![7]);
    }

    #[test]
    fn should_not_track_pending_when_cancel_target_or_reason_mismatch() {
        // target != active or reason != user_cancelled or no turnId → no
        // enqueue (the TS InterruptionReminderModel gate).
        let mut state = AgentState::new();
        for payload in [
            serde_json::json!({"turnId": 1, "target": "other", "reason": "user_cancelled"}),
            serde_json::json!({"turnId": 2, "target": "active", "reason": "system"}),
            serde_json::json!({"target": "active", "reason": "user_cancelled"}), // no turnId
        ] {
            state = reduce(&state, &op_json("turn.cancel", payload)).0;
        }
        assert!(state.interruption_pending.is_empty());
    }

    #[test]
    fn should_remove_pending_interruption_when_reminder_recorded() {
        let mut state = AgentState::new();
        state.interruption_pending = vec![3, 7];
        state = reduce(
            &state,
            &op_json(
                "interruptionReminder.recorded",
                serde_json::json!({"turnId": 7}),
            ),
        )
        .0;
        assert_eq!(state.interruption_pending, vec![3]);
    }

    #[test]
    fn should_fold_swarm_enter_and_exit_trigger_state() {
        let mut state = AgentState::new();
        state = reduce(
            &state,
            &op_json("swarm_mode.enter", serde_json::json!({"trigger": "manual"})),
        )
        .0;
        assert_eq!(state.swarm_trigger, Some(serde_json::json!("manual")));
        state = reduce(&state, &op_json("swarm_mode.exit", serde_json::json!({}))).0;
        assert_eq!(state.swarm_trigger, None);
    }

    #[test]
    fn should_retract_enter_reminder_from_conversation_when_exit_and_it_is_last() {
        let mut state = AgentState::new();
        state.conversation.messages = vec![
            message_json("user", "hello"),
            injection_message_json("## Swarm Mode\nYou are now in ..."),
        ];
        state = reduce(&state, &op_json("swarm_mode.exit", serde_json::json!({}))).0;
        assert_eq!(state.conversation.messages.len(), 1); // enter reminder popped
    }

    #[test]
    fn should_retract_the_wrapped_enter_reminder_when_exit_and_it_is_last() {
        // Given the enter reminder exactly as the live write side records
        // it: the `<system-reminder>`-wrapped body of the durable
        // context.append_message (mirroring what append_system_reminder
        // stores), with the plain injection origin
        let mut state = AgentState::new();
        state = reduce(
            &state,
            &op_json("swarm_mode.enter", serde_json::json!({"trigger": "manual"})),
        )
        .0;
        state = reduce(
            &state,
            &WireOp::ContextAppendMessage {
                message: serde_json::json!({
                    "role": "user",
                    "content": [{
                        "type": "text",
                        "text": "<system-reminder>\n## Swarm Mode\n\nYou are now in \"agent swarm\" mode.\n</system-reminder>",
                    }],
                    "origin": { "kind": "injection" },
                }),
            },
        )
        .0;
        assert_eq!(state.conversation.messages.len(), 1);

        // When the session exits
        state = reduce(&state, &op_json("swarm_mode.exit", serde_json::json!({}))).0;

        // Then the wrapped reminder is retracted — the replay matches the
        // live pop (TS popSwarmModeReminder)
        assert!(
            state.conversation.messages.is_empty(),
            "the wrapped enter reminder must be retracted on replay"
        );
    }

    #[test]
    fn should_keep_the_exit_reminder_when_exit_follows_exit() {
        // Given the exit reminder (header "## Swarm Mode Ended") as the
        // newest message — what a second exit would look at
        let mut state = AgentState::new();
        state.conversation.messages = vec![
            injection_message_json("## Swarm Mode\nYou are now in ..."),
            injection_message_json("## Swarm Mode Ended\nSwarm Mode has ended."),
        ];

        // When the session exits again
        state = reduce(&state, &op_json("swarm_mode.exit", serde_json::json!({}))).0;

        // Then the exit reminder is NOT mistaken for the enter reminder
        // (the TS origin variants swarm_mode / swarm_mode_exit map to the
        // header-line discrimination here)
        assert_eq!(
            state.conversation.messages.len(),
            2,
            "the exit reminder must not be retracted by a later exit"
        );
    }

    #[test]
    fn should_keep_conversation_when_exit_and_last_message_is_not_enter_reminder() {
        let mut state = AgentState::new();
        state.conversation.messages = vec![
            injection_message_json("## Swarm Mode\nYou are now in ..."),
            message_json("assistant", "done"),
        ];
        state = reduce(&state, &op_json("swarm_mode.exit", serde_json::json!({}))).0;
        assert_eq!(state.conversation.messages.len(), 2);
    }

    #[test]
    fn should_keep_the_timeline_consistent_when_exit_retracts_the_enter_reminder() {
        // Given an enter reminder appended through the real append arm
        // (so the timeline references it like any folded message)
        let mut state = AgentState::new();
        state = reduce(
            &state,
            &WireOp::ContextAppendMessage {
                message: message_json("user", "hello"),
            },
        )
        .0;
        state = reduce(
            &state,
            &WireOp::ContextAppendMessage {
                message: injection_message_json("## Swarm Mode\nYou are now in ..."),
            },
        )
        .0;

        // When the exit retracts the reminder and a new message arrives
        state = reduce(&state, &op_json("swarm_mode.exit", serde_json::json!({}))).0;
        state = reduce(
            &state,
            &WireOp::ContextAppendMessage {
                message: message_json("user", "next"),
            },
        )
        .0;

        // Then the timeline references each surviving message exactly
        // once — a dangling entry for the popped reminder would alias the
        // next appended message and double-count it on replay.
        let conv = &state.conversation;
        assert_eq!(conv.messages.len(), 2);
        let mut covered = vec![0usize; conv.messages.len()];
        for entry in &conv.timeline {
            if let TimelineEntry::Message { index } = entry {
                covered[*index] += 1;
            }
        }
        assert!(covered.into_iter().all(|count| count == 1));
    }

    #[test]
    fn should_store_the_todo_value_when_folding_an_update_store_op_with_todo_key() {
        // Given a fresh state and an update_store op carrying the session
        // todo list under the "todo" key (the exact shape the live TodoList
        // write side records — Task 1)
        let mut state = AgentState::default();
        let op = WireOp::ToolsUpdateStore {
            key: "todo".to_string(),
            value: serde_json::json!([{ "title": "a", "status": "pending" }]),
        };

        // When folded
        state = reduce(&state, &op).0;

        // Then the store keeps the value verbatim under the "todo" key —
        // the wire contract later tasks restore from (array of
        // {title, status} items)
        assert_eq!(
            state.tools.store.get("todo"),
            Some(&serde_json::json!([{ "title": "a", "status": "pending" }]))
        );
    }

    #[test]
    fn should_keep_the_previous_value_when_folding_a_malformed_todo_store_op() {
        // Given a folded valid todo value
        let mut state = AgentState::default();
        state = reduce(
            &state,
            &WireOp::ToolsUpdateStore {
                key: "todo".to_string(),
                value: serde_json::json!([{ "title": "a", "status": "pending" }]),
            },
        )
        .0;

        // When a non-array todo value is folded -> skipped, old kept
        state = reduce(
            &state,
            &WireOp::ToolsUpdateStore {
                key: "todo".to_string(),
                value: serde_json::json!("garbage"),
            },
        )
        .0;
        assert!(state.tools.store.get("todo").unwrap().is_array());

        // When an item-shaped todo value (bad item shape) is folded ->
        // skipped, old kept
        state = reduce(
            &state,
            &WireOp::ToolsUpdateStore {
                key: "todo".to_string(),
                value: serde_json::json!([{ "title": 42 }]),
            },
        )
        .0;
        assert!(state.tools.store.get("todo").unwrap().is_array());
    }

    #[test]
    fn should_store_unrelated_keys_untouched_when_folding_non_todo_store_ops() {
        // Given a fresh state and an update_store op for a non-todo key
        let mut state = AgentState::default();
        state = reduce(
            &state,
            &WireOp::ToolsUpdateStore {
                key: "other".to_string(),
                value: serde_json::json!({"x": 1}),
            },
        )
        .0;

        // Then the non-todo key is stored verbatim and no "todo" entry is
        // invented
        assert!(!state.tools.store.contains_key("todo"));
        assert!(state.tools.store.contains_key("other"));
    }

    #[test]
    fn should_checkpoint_the_todo_list_when_a_real_user_message_is_appended() {
        // Given state with store["todo"] = [item A]
        let mut state = AgentState::default();
        state = reduce(
            &state,
            &WireOp::ToolsUpdateStore {
                key: "todo".to_string(),
                value: json!([{ "title": "a", "status": "pending" }]),
            },
        )
        .0;

        // When ContextAppendMessage with role=user (no origin)
        state = reduce(
            &state,
            &WireOp::ContextAppendMessage {
                message: json!({
                    "role": "user",
                    "content": [{ "type": "text", "text": "hi" }],
                }),
            },
        )
        .0;

        // Then todo_checkpoints == [[item A]] and store unchanged
        assert_eq!(
            state.tools.todo_checkpoints,
            vec![json!([{ "title": "a", "status": "pending" }])]
        );
        assert_eq!(
            state.tools.store.get("todo"),
            Some(&json!([{ "title": "a", "status": "pending" }]))
        );
    }

    #[test]
    fn should_checkpoint_the_empty_list_when_no_todo_store_exists_on_user_append() {
        // Given a fresh state with no "todo" store entry
        let mut state = AgentState::default();

        // When a real user message is appended
        state = reduce(
            &state,
            &WireOp::ContextAppendMessage {
                message: json!({
                    "role": "user",
                    "content": [{ "type": "text", "text": "hi" }],
                }),
            },
        )
        .0;

        // Then the checkpoint is the empty list (the TS model's `initial()`)
        assert_eq!(state.tools.todo_checkpoints, vec![json!([])]);
    }

    #[test]
    fn should_checkpoint_when_a_user_message_carries_a_real_user_input_origin() {
        // Given a state carrying a todo list
        let mut state = AgentState::default();
        state = reduce(
            &state,
            &WireOp::ToolsUpdateStore {
                key: "todo".to_string(),
                value: json!([{ "title": "a", "status": "pending" }]),
            },
        )
        .0;

        // When the user message carries an explicit `{"kind": "user"}`
        // origin (the wire form of PromptOrigin::User)
        state = reduce(
            &state,
            &WireOp::ContextAppendMessage {
                message: json!({
                    "role": "user",
                    "content": [{ "type": "text", "text": "hi" }],
                    "origin": { "kind": "user" },
                }),
            },
        )
        .0;

        // Then a checkpoint was pushed
        assert_eq!(state.tools.todo_checkpoints.len(), 1);
    }

    #[test]
    fn should_not_checkpoint_when_an_assistant_message_is_appended() {
        // Given a state carrying a todo list
        let mut state = AgentState::default();
        state = reduce(
            &state,
            &WireOp::ToolsUpdateStore {
                key: "todo".to_string(),
                value: json!([{ "title": "a", "status": "pending" }]),
            },
        )
        .0;

        // When an assistant message is appended
        state = reduce(
            &state,
            &WireOp::ContextAppendMessage {
                message: json!({
                    "role": "assistant",
                    "content": [{ "type": "text", "text": "hello" }],
                }),
            },
        )
        .0;

        // Then no checkpoint was pushed
        assert!(state.tools.todo_checkpoints.is_empty());
    }

    #[test]
    fn should_not_checkpoint_when_a_user_message_carries_an_injection_origin() {
        // Given a state carrying a todo list
        let mut state = AgentState::default();
        state = reduce(
            &state,
            &WireOp::ToolsUpdateStore {
                key: "todo".to_string(),
                value: json!([{ "title": "a", "status": "pending" }]),
            },
        )
        .0;

        // When the appended user message carries an injection origin (the
        // engine-synthesized system-reminder form)
        state = reduce(
            &state,
            &WireOp::ContextAppendMessage {
                message: json!({
                    "role": "user",
                    "content": [{ "type": "text", "text": "reminder" }],
                    "origin": { "kind": "injection" },
                }),
            },
        )
        .0;

        // Then no checkpoint was pushed — injections must not anchor
        assert!(state.tools.todo_checkpoints.is_empty());
    }

    #[test]
    fn should_restore_the_older_todo_list_when_undoing_past_a_write() {
        // Given store["todo"] = [A], checkpoint [A] pushed, then store set to [B]
        let mut state = AgentState::default();
        state = reduce(
            &state,
            &WireOp::ToolsUpdateStore {
                key: "todo".to_string(),
                value: json!([{ "title": "a", "status": "pending" }]),
            },
        )
        .0;
        state = reduce(
            &state,
            &WireOp::ContextAppendMessage {
                message: json!({
                    "role": "user",
                    "content": [{ "type": "text", "text": "hi" }],
                }),
            },
        )
        .0;
        state = reduce(
            &state,
            &WireOp::ToolsUpdateStore {
                key: "todo".to_string(),
                value: json!([{ "title": "b", "status": "in_progress" }]),
            },
        )
        .0;

        // When ContextUndo { count: 1 }
        state = reduce(&state, &WireOp::ContextUndo { count: 1 }).0;

        // Then store["todo"] == [A] and checkpoints drained
        assert_eq!(
            state.tools.store.get("todo"),
            Some(&json!([{ "title": "a", "status": "pending" }]))
        );
        assert!(state.tools.todo_checkpoints.is_empty());
    }

    #[test]
    fn should_keep_the_todo_list_when_undo_count_exceeds_checkpoints() {
        // Given 1 checkpoint
        let mut state = AgentState::default();
        state = reduce(
            &state,
            &WireOp::ToolsUpdateStore {
                key: "todo".to_string(),
                value: json!([{ "title": "a", "status": "pending" }]),
            },
        )
        .0;
        state = reduce(
            &state,
            &WireOp::ContextAppendMessage {
                message: json!({
                    "role": "user",
                    "content": [{ "type": "text", "text": "hi" }],
                }),
            },
        )
        .0;
        state = reduce(
            &state,
            &WireOp::ToolsUpdateStore {
                key: "todo".to_string(),
                value: json!([{ "title": "b", "status": "in_progress" }]),
            },
        )
        .0;

        // When ContextUndo { count: 3 }
        state = reduce(&state, &WireOp::ContextUndo { count: 3 }).0;

        // Then store unchanged (still [B]) and checkpoints untouched
        assert_eq!(
            state.tools.store.get("todo"),
            Some(&json!([{ "title": "b", "status": "in_progress" }]))
        );
        assert_eq!(state.tools.todo_checkpoints.len(), 1);
    }

    #[test]
    fn should_clear_todo_checkpoints_when_a_compaction_is_applied() {
        // Given checkpoints non-empty
        let mut state = AgentState::default();
        state = reduce(
            &state,
            &WireOp::ToolsUpdateStore {
                key: "todo".to_string(),
                value: json!([{ "title": "a", "status": "pending" }]),
            },
        )
        .0;
        state = reduce(
            &state,
            &WireOp::ContextAppendMessage {
                message: json!({
                    "role": "user",
                    "content": [{ "type": "text", "text": "hi" }],
                }),
            },
        )
        .0;
        assert!(!state.tools.todo_checkpoints.is_empty());

        // When a compaction is applied (the op the rust engine records
        // live — NOT full_compaction.complete, which never clears)
        state = reduce(
            &state,
            &WireOp::ContextApplyCompaction {
                summary: "SUMMARY".to_string(),
                compacted_count: 1,
                tokens_before: Some(100),
                tokens_after: Some(10),
                context_summary: None,
                dropped_count: None,
                retained_tail_count: None,
                kept_user_message_count: Some(1),
                kept_head_user_message_count: Some(0),
                elision_message: None,
                first_replaced_entry_seq: None,
                last_replaced_entry_seq: None,
                dropped_entry_seqs: None,
            },
        )
        .0;

        // Then the checkpoint stack is empty and the store value survives
        assert!(state.tools.todo_checkpoints.is_empty());
        assert!(state.tools.store.contains_key("todo"));
    }

    #[test]
    fn should_not_clear_todo_checkpoints_when_full_compaction_completes() {
        // Given checkpoints non-empty (TS parity: full_compaction.complete
        // never touches checkpoints — the clear rides context.apply_compaction)
        let mut state = AgentState::default();
        state = reduce(
            &state,
            &WireOp::ToolsUpdateStore {
                key: "todo".to_string(),
                value: json!([{ "title": "a", "status": "pending" }]),
            },
        )
        .0;
        state = reduce(
            &state,
            &WireOp::ContextAppendMessage {
                message: json!({
                    "role": "user",
                    "content": [{ "type": "text", "text": "hi" }],
                }),
            },
        )
        .0;
        assert_eq!(state.tools.todo_checkpoints.len(), 1);

        // When FullCompactionComplete
        state = reduce(&state, &WireOp::FullCompactionComplete {}).0;

        // Then the checkpoint stack is UNCHANGED
        assert_eq!(state.tools.todo_checkpoints.len(), 1);
    }

    #[test]
    fn should_keep_the_store_when_undoing_after_a_compaction_cleared_the_stack() {
        // Given [A] → anchor → [B] → apply_compaction (stack cleared)
        let mut state = AgentState::default();
        state = reduce(
            &state,
            &WireOp::ToolsUpdateStore {
                key: "todo".to_string(),
                value: json!([{ "title": "a", "status": "pending" }]),
            },
        )
        .0;
        state = reduce(
            &state,
            &WireOp::ContextAppendMessage {
                message: json!({
                    "role": "user",
                    "content": [{ "type": "text", "text": "hi" }],
                }),
            },
        )
        .0;
        state = reduce(
            &state,
            &WireOp::ToolsUpdateStore {
                key: "todo".to_string(),
                value: json!([{ "title": "b", "status": "in_progress" }]),
            },
        )
        .0;
        state = reduce(
            &state,
            &WireOp::ContextApplyCompaction {
                summary: "SUMMARY".to_string(),
                compacted_count: 1,
                tokens_before: Some(100),
                tokens_after: Some(10),
                context_summary: None,
                dropped_count: None,
                retained_tail_count: None,
                kept_user_message_count: Some(1),
                kept_head_user_message_count: Some(0),
                elision_message: None,
                first_replaced_entry_seq: None,
                last_replaced_entry_seq: None,
                dropped_entry_seqs: None,
            },
        )
        .0;
        assert!(state.tools.todo_checkpoints.is_empty());

        // When an undo arrives (any count — the stack is empty)
        state = reduce(&state, &WireOp::ContextUndo { count: 1 }).0;

        // Then the store is unchanged ([B] survives — no checkpoint to
        // restore from) and the stack stays empty
        assert_eq!(
            state.tools.store.get("todo"),
            Some(&json!([{ "title": "b", "status": "in_progress" }]))
        );
        assert!(state.tools.todo_checkpoints.is_empty());
    }

    #[test]
    fn should_clear_todo_checkpoints_when_the_context_is_cleared() {
        // Given checkpoints non-empty
        let mut state = AgentState::default();
        state = reduce(
            &state,
            &WireOp::ToolsUpdateStore {
                key: "todo".to_string(),
                value: json!([{ "title": "a", "status": "pending" }]),
            },
        )
        .0;
        state = reduce(
            &state,
            &WireOp::ContextAppendMessage {
                message: json!({
                    "role": "user",
                    "content": [{ "type": "text", "text": "hi" }],
                }),
            },
        )
        .0;
        assert_eq!(state.tools.todo_checkpoints.len(), 1);

        // When the context is cleared
        state = reduce(&state, &WireOp::ContextClear {}).0;

        // Then the checkpoint stack is empty (the store value survives —
        // context.clear is conversation-scoped, not tool-store-scoped)
        assert!(state.tools.todo_checkpoints.is_empty());
        assert!(state.tools.store.contains_key("todo"));
    }

    #[test]
    fn should_anchor_a_skill_activation_with_a_user_inline_trigger() {
        // Given a state carrying a todo list
        let mut state = AgentState::default();
        state = reduce(
            &state,
            &WireOp::ToolsUpdateStore {
                key: "todo".to_string(),
                value: json!([{ "title": "a", "status": "pending" }]),
            },
        )
        .0;

        // When a skill-activation user message carries the user-INLINE
        // trigger (TS's isUndoAnchor would NOT anchor this —
        // it requires user-slash; the rust fold deliberately does, matching
        // the trigger-agnostic memory-side is_undo_anchor)
        state = reduce(
            &state,
            &WireOp::ContextAppendMessage {
                message: json!({
                    "role": "user",
                    "content": [{ "type": "text", "text": "wrapped skill" }],
                    "origin": { "kind": "skill_activation", "trigger": "user-inline" },
                }),
            },
        )
        .0;

        // Then a checkpoint WAS pushed — pinning the deliberate divergence
        assert_eq!(state.tools.todo_checkpoints.len(), 1);
    }
}
