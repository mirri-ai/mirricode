//! Agent tool — dispatch a subagent to handle a delegated task.
//!
//! The `Agent` tool spawns a child agent with a specified profile (e.g.
//! `"coder"`, `"explore"`) and a full task prompt. The child agent runs its
//! own turn loop to completion, and the result is returned to the parent.
//!
//! The actual subagent lifecycle (creating an `AgentScope`, building a
//! `TurnFlow`, running `run_one_turn`) is delegated to the host layer via
//! the [`SubagentHost`](crate::subagent::SubagentHost) trait.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use serde_json::json;

use crate::cancel::CancelToken;
use crate::error::{EngineError, EngineResult};
use crate::subagent::{ResumeRequest, SpawnRequest, SubagentHost, SubagentTask, TaskSink};
use crate::task::{TaskKind, TaskManager};
use crate::tool::{
    ExecutableToolContext, ExecutionPlan, Tool, ToolExecution, ToolFuture, ToolInputDisplay,
    ToolResult,
};
use crate::turn_flow::ActivityTracker;

/// Default profile name when `subagent_type` is not specified.
const DEFAULT_PROFILE: &str = "coder";

/// The `Agent` tool — dispatch a single subagent.
pub struct AgentTool {
    host: Arc<dyn SubagentHost>,
    tasks: Arc<TaskManager>,
    session_id: String,
    description: String,
    /// Per-subagent timeout (B1 D-40): `None` = unbounded (print mode).
    subagent_timeout: Option<Duration>,
    /// Whether background dispatch is available (B1 D-50): `true` only
    /// when TaskList, TaskOutput, and TaskStop are all active on the
    /// owning agent's tool manager.
    background_gate: Arc<dyn Fn() -> bool + Send + Sync>,
}

impl AgentTool {
    pub fn new(
        host: Arc<dyn SubagentHost>,
        tasks: Arc<TaskManager>,
        session_id: String,
        subagent_timeout: Option<Duration>,
        background_gate: Arc<dyn Fn() -> bool + Send + Sync>,
    ) -> Self {
        let mut description = build_agent_description(&host);
        // The background-execution requirement rides the description so
        // the model knows why a `run_in_background` call may be rejected
        // (the live gate itself is evaluated per call — tool activation
        // can change after construction).
        description
            .push_str("\n\nBackground execution requires TaskList, TaskOutput, and TaskStop.");
        Self {
            host,
            tasks,
            session_id,
            description,
            subagent_timeout,
            background_gate,
        }
    }
}

/// Build the Agent tool description, appending available profiles and models
/// from the host so the LLM knows what it can select.
fn build_agent_description(host: &Arc<dyn SubagentHost>) -> String {
    let base = "Launch a subagent to handle a task. The subagent runs its own agent \
         loop and returns a result. Use `subagent_type` to select a profile \
         (e.g. 'coder', 'explore', 'plan'). Use `resume` to continue a \
         previous subagent by its agent_id. Set `run_in_background` to return \
         immediately with a task_id.";

    let profiles = host.available_profiles();
    if profiles.is_empty() {
        return base.to_string();
    }

    // Profile lines render the shared dispatch entry (see
    // `SubagentProfileInfo::dispatch_description_line`): a
    // `- {name}: {description} {when_to_use}` header plus a `Tools:`
    // annotation mirroring the declared tool surface. This is duties-only
    // metadata from the user's profile config — a description is not a
    // model name, so R7/A8 are unaffected.
    let lines: Vec<String> = profiles
        .iter()
        .map(|p| p.dispatch_description_line())
        .collect();

    let mut description = format!(
        "{base}\n\nAvailable profiles (subagent_type):\n{}",
        lines.join("\n")
    );

    // Model adjudication (spec 11 R7/A8): the description never carries a
    // concrete model name from the profiles — builtin profiles declare no
    // model (R6), and even a user-configured profile's default model is not
    // rendered. The resolution rule is a fixed sentence, and the only
    // model names below it are catalog-sourced.
    //
    // The curated list comes from the catalog so the LLM can pick a valid
    // `model` override (aliases are validated before spawning). Only
    // aliases with a non-empty description are visible on this surface
    // (D-M10.1) — bare alias strings carry no purpose info and are gated
    // out; display names never appear (D-M10.2).
    let curated = host.curated_models();
    if !curated.is_empty() {
        description.push_str("\n\n");
        description.push_str(crate::model_catalog::MODEL_DEFAULTS_SENTENCE);
        description.push_str("\n\nAvailable models:\n");
        for m in curated {
            description.push_str(&crate::model_catalog::curated_line(&m));
            description.push('\n');
        }
    }

    description
}

impl Tool for AgentTool {
    fn name(&self) -> &str {
        "Agent"
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters(&self) -> &serde_json::Value {
        static SCHEMA: std::sync::OnceLock<serde_json::Value> = std::sync::OnceLock::new();
        SCHEMA.get_or_init(|| {
            json!({
                "type": "object",
                "properties": {
                    "prompt": {
                        "type": "string",
                        "description": "Full task prompt for the subagent."
                    },
                    "description": {
                        "type": "string",
                        "description": "Short 3-5 word description for UI display."
                    },
                    "subagent_type": {
                        "type": "string",
                        "description": "Profile name. Defaults to 'coder'. Mutually exclusive with 'resume'."
                    },
                    "resume": {
                        "type": "string",
                        "description": "Agent ID to resume. Mutually exclusive with 'subagent_type'."
                    },
                    "run_in_background": {
                        "type": "boolean",
                        "description": "If true, return immediately with a task_id instead of waiting for completion."
                    },
                    "model": {
                        "type": "string",
                        "description": "Model alias to override the subagent default. Valid aliases are listed in the 'Available models' section of this tool description when applicable."
                    }
                },
                "required": ["prompt", "description"],
                "additionalProperties": false
            })
        })
    }

    fn resolve_execution(&self, input: serde_json::Value) -> EngineResult<ToolExecution> {
        let prompt: String = input
            .get("prompt")
            .and_then(|v| v.as_str())
            .ok_or_else(|| EngineError::RequestInvalid("missing 'prompt'".into()))?
            .to_string();
        let description: String = input
            .get("description")
            .and_then(|v| v.as_str())
            .ok_or_else(|| EngineError::RequestInvalid("missing 'description'".into()))?
            .to_string();
        let subagent_type: Option<String> = input
            .get("subagent_type")
            .and_then(|v| v.as_str())
            .map(String::from);
        let resume: Option<String> = input
            .get("resume")
            .and_then(|v| v.as_str())
            .map(String::from);
        let run_in_background: bool = input
            .get("run_in_background")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let model: Option<String> = input
            .get("model")
            .and_then(|v| v.as_str())
            .map(String::from);

        // Validate: resume and subagent_type are mutually exclusive.
        if resume.is_some() && subagent_type.is_some() {
            return Ok(ToolExecution::Error(crate::tool::ToolErrorResult {
                message: "Cannot specify both 'resume' and 'subagent_type'. Use 'resume' to \
                          continue an existing subagent, or 'subagent_type' to spawn a new one."
                    .into(),
            }));
        }

        // Background gating (D-50, TS agent.ts:85): reject BEFORE spawning
        // when the task-view tools are not all active — the model must not
        // get a task_id it cannot observe or stop.
        if run_in_background && !(self.background_gate)() {
            return Ok(ToolExecution::Error(crate::tool::ToolErrorResult {
                message: "Background agent execution is not available for this agent because \
                          TaskList, TaskOutput, and TaskStop are not enabled."
                    .into(),
            }));
        }

        // Capacity pre-check (D-49, TS `assertCanRegister` order): reject
        // BEFORE spawning — a post-spawn rejection would still have minted
        // a `subagent.spawned` wire event and a children entry for a
        // subagent that never runs. Advisory only; the authoritative check
        // runs at `register_background` (the residual race is cleaned up
        // there).
        if run_in_background && !self.tasks.can_register_background() {
            return Ok(ToolExecution::Error(crate::tool::ToolErrorResult {
                message: crate::task::TASK_CAPACITY_MESSAGE.to_string(),
            }));
        }

        let profile_name = subagent_type
            .clone()
            .unwrap_or_else(|| DEFAULT_PROFILE.to_string());
        let host = Arc::clone(&self.host);
        let tasks = Arc::clone(&self.tasks);
        let session_id = self.session_id.clone();
        let subagent_timeout = self.subagent_timeout;

        let plan = ExecutionPlan {
            accesses: Vec::new(),
            display: ToolInputDisplay::AgentCall {
                agent_name: profile_name.clone(),
                prompt: description.clone(),
                background: input
                    .get("run_in_background")
                    .and_then(serde_json::Value::as_bool),
            },
            approval_rule: "Agent".to_string(),
            stop_batch_after_this: true,
        };

        let execute: Box<dyn FnOnce(ExecutableToolContext) -> ToolFuture + Send> =
            Box::new(move |ctx: ExecutableToolContext| {
                Box::pin(async move {
                    // Create a child cancel token so parent cancellation propagates
                    // but independent TaskStop only affects this subagent.
                    let child_cancel = ctx.cancel.child();

                    let handle = if let Some(ref agent_id) = resume {
                        match host.resume(ResumeRequest {
                            agent_id: agent_id.clone(),
                            prompt: prompt.clone(),
                            cancel: child_cancel.clone(),
                            retry: false,
                            suppress_rate_limit_failure_event: false,
                        }) {
                            Ok(h) => h,
                            Err(e) => {
                                return ToolResult::error(format!(
                                    "Failed to resume subagent '{agent_id}': {e}"
                                ));
                            }
                        }
                    } else {
                        let request = SpawnRequest {
                            profile_name: profile_name.clone(),
                            prompt: prompt.clone(),
                            description: description.clone(),
                            model: model.clone(),
                            cancel: child_cancel.clone(),
                            run_in_background,
                            parent_tool_call_id: Some(ctx.tool_call_id.clone()),
                            swarm_index: None,
                            swarm_item: None,
                            suppress_rate_limit_failure_event: false,
                        };
                        match host.spawn(request) {
                            Ok(h) => h,
                            Err(e) => {
                                return ToolResult::error(format!("Failed to spawn subagent: {e}"));
                            }
                        }
                    };

                    let agent_id = handle.agent_id.clone();
                    let actual_type = handle.profile_name.clone();
                    let resolved_model = handle.model.clone();
                    let task_desc = description.clone();
                    // The child's engine activity source — the heartbeat
                    // forwarding below polls it while the run is in flight.
                    let activity = handle.activity.clone();
                    // ADR-006 wait-chain propagation: the child's own wait
                    // registry, cloned before the handle moves into the
                    // task (the WaitingSubagent probe falls back to it
                    // when the child's activity clock is frozen).
                    let child_wait_registry = handle.wait_registry.clone();

                    let mut task = SubagentTask::new(handle, task_desc, child_cancel.clone());

                    if run_in_background {
                        // Register with TaskManager under the capacity
                        // limit (D-49) and the per-subagent timeout
                        // (D-40) and return immediately.
                        let task_id = match tasks.register_background(
                            TaskKind::Agent {
                                agent_id: Some(agent_id.clone()),
                                subagent_type: Some(actual_type.clone()),
                                parent_tool_call_id: Some(ctx.tool_call_id.clone()),
                                swarm_index: None,
                                model: resolved_model.clone(),
                            },
                            format!("agent: {description}"),
                            session_id.clone(),
                            subagent_timeout,
                        ) {
                            Ok(task_id) => task_id,
                            Err(message) => {
                                // Residual capacity race (a concurrent
                                // registration closed the window between
                                // the pre-check and here): the spawn side
                                // effects already happened, so clean them
                                // up — cancel the child token and forget
                                // the children entry; the `subagent.spawned`
                                // event this minted cannot be retracted,
                                // which is why the pre-check above does
                                // the common rejection.
                                child_cancel.cancel();
                                host.forget_child(&agent_id);
                                return ToolResult::error(message);
                            }
                        };
                        // The cancel signal rides the task entry so a
                        // `TaskStop` actually aborts the child (G16).
                        tasks.attach_signal(&task_id, child_cancel.clone());
                        let tasks_clone = Arc::clone(&tasks);
                        let task_id_clone = task_id.clone();
                        let tasks_for_heartbeat = Arc::clone(&tasks);
                        let task_id_for_heartbeat = task_id.clone();
                        tokio::spawn(async move {
                            let mut sink =
                                TaskManagerSink::new(task_id_clone, tasks_clone, child_cancel);
                            drive_with_activity_forwarding(
                                task.start(&mut sink),
                                activity,
                                tasks_for_heartbeat,
                                task_id_for_heartbeat,
                                // Background: the parent turn is no longer
                                // awaiting this tool call, so there is no
                                // stall deadline to feed — task-heartbeat
                                // forwarding only.
                                crate::tool::ToolProgressHandle::default(),
                            )
                            .await;
                        });

                        return format_background_result(
                            &task_id,
                            &agent_id,
                            &actual_type,
                            &resolved_model,
                        );
                    }

                    // Foreground: run to completion, bounded by the
                    // per-subagent timeout (D-40). The tracked task
                    // carries the same explicit deadline so the session
                    // sweeper is a second line of defense, not the only one.
                    let task_id = tasks.register_with_timeout(
                        TaskKind::Agent {
                            agent_id: Some(agent_id.clone()),
                            subagent_type: Some(actual_type.clone()),
                            parent_tool_call_id: Some(ctx.tool_call_id.clone()),
                            swarm_index: None,
                            model: resolved_model.clone(),
                        },
                        format!("agent: {description}"),
                        session_id.clone(),
                        subagent_timeout,
                    );
                    // Same signal wiring as the background path — a
                    // `TaskStop` on the tracked task cancels the child.
                    tasks.attach_signal(&task_id, child_cancel.clone());
                    // The orphan guard: this tool-execution future drives
                    // the run INLINE (`task.start` is awaited below), so a
                    // parent turn killed by dropping its future (the
                    // `message.send` stall watchdog) drops this driver too
                    // — without the guard the tracked task lingers
                    // "running" until the sweeper's multi-hour timeout
                    // while the task driver warns it is stale. On drop the
                    // guard cancels the child token and stops the tracked
                    // entry; first-terminal-wins keeps the normal,
                    // timeout, and cancellation settles from being
                    // overwritten.
                    let _orphan_guard = ForegroundOrphanGuard {
                        tasks: Arc::clone(&tasks),
                        task_id: task_id.clone(),
                        cancel: child_cancel.clone(),
                    };
                    let mut sink =
                        TaskManagerSink::new(task_id.clone(), Arc::clone(&tasks), child_cancel);

                    // ADR-006: the foreground wait registers its
                    // WaitingSubagent entry — the watchdog pulls the
                    // subagent host's owner verdict (activity advanced →
                    // Advancing; child not running → Settled; running but
                    // silent → Stalled) at window expiry. The closure
                    // captures the child's activity tracker and the host
                    // handle, so turn_flow stays decoupled from the host
                    // (reviewer I6).
                    let _subagent_wait = ctx.wait_registry.as_ref().map(|registry| {
                        let activity = activity.clone();
                        let host = Arc::clone(&host);
                        let agent_id = agent_id.clone();
                        // ADR-006 wait-chain propagation: the child's own
                        // wait registry — when the child's activity clock
                        // is frozen (the child is itself waiting on a task
                        // or gate), the probe falls back to the child's
                        // registry: a child healthily waiting on something
                        // advancing is Advancing up the chain (the
                        // fork/converted incident shapes).
                        let child_registry = child_wait_registry.clone();
                        // The activity baseline: bumped on registration,
                        // compared per probe — only NEW activity (the
                        // tracker's monotonic last-activity clock moving)
                        // counts as advancing.
                        let baseline = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(
                            activity
                                .as_ref()
                                .map_or(0, |tracker| tracker.last_activity_ms()),
                        ));
                        registry.enter(
                            crate::turn_flow::WaitKind::WaitingSubagent,
                            crate::turn_flow::WaitSubject::Subagent {
                                probe: std::sync::Arc::new(move || {
                                    if !host.is_child_running(&agent_id) {
                                        // The child settled (its run
                                        // finished; the host forgot it).
                                        return crate::turn_flow::OwnerVerdict::Settled;
                                    }
                                    let now = activity
                                        .as_ref()
                                        .map_or(0, |tracker| tracker.last_activity_ms());
                                    let last =
                                        baseline.swap(now, std::sync::atomic::Ordering::Relaxed);
                                    if now > last {
                                        crate::turn_flow::OwnerVerdict::Advancing
                                    } else if let Some(child) = child_registry.as_ref()
                                        && child.probe()
                                            == crate::turn_flow::TurnWaitState::Advancing
                                    {
                                        // The child's own clock is frozen
                                        // but its internal wait is
                                        // advancing (a task heartbeat, a
                                        // pending gate) — the wait chain
                                        // is alive end to end.
                                        crate::turn_flow::OwnerVerdict::Advancing
                                    } else {
                                        // Running but silent — the kind
                                        // window applies (ADR-006: the
                                        // subagent class has no better
                                        // fact source).
                                        crate::turn_flow::OwnerVerdict::Stalled
                                    }
                                }),
                            },
                        )
                    });
                    let run = drive_with_activity_forwarding(
                        task.start(&mut sink),
                        activity,
                        Arc::clone(&tasks),
                        task_id.clone(),
                        // Foreground: the parent turn is blocked on this
                        // tool call, so every observed activity bump is
                        // also forwarded through the tool's live-update
                        // channel — a `LiveEvent::ToolProgress` that
                        // resets the turn's stall-watchdog deadline, so a
                        // live subagent is never mistaken for a stalled
                        // engine (the stall false positive).
                        ctx.progress.clone(),
                    );
                    match subagent_timeout {
                        Some(timeout) => {
                            if tokio::time::timeout(timeout, run).await.is_err() {
                                // A timed-out foreground run is a REAL
                                // cancellation: fire the child token (the
                                // dropped runner and any detached work
                                // observe it) and settle the tracked task
                                // with the timeout message — the
                                // first-terminal-wins rule keeps a late
                                // settle from overwriting.
                                sink.signal().cancel();
                                sink.settle(
                                    crate::task::TaskStatus::Failed(
                                        crate::task::TASK_TIMEOUT_MESSAGE.to_string(),
                                    ),
                                    Some("timeout".to_string()),
                                );
                                return format_foreground_timeout_result(
                                    &agent_id,
                                    &actual_type,
                                    &resolved_model,
                                    &sink.output,
                                    timeout.as_secs(),
                                );
                            }
                        }
                        None => run.await,
                    }

                    format_foreground_result(
                        &agent_id,
                        &actual_type,
                        &resolved_model,
                        &sink.output,
                        &ForegroundOutcome::from_sink(&sink),
                    )
                })
            });

        Ok(ToolExecution::Runnable { plan, execute })
    }
}

/// Drive one subagent run to completion while forwarding the child's real
/// engine activity (LLM steps, tool calls — the TurnFlow's activity fold)
/// into two liveness consumers (B1 T4, activity-as-liveness):
///
/// 1. the tracked task's liveness window — a live, working subagent never
///    reads as stale, while a runner that died without settling (or whose
///    engine went silent) still does;
/// 2. the parent tool call's live-update channel (`progress`) — each
///    observed bump surfaces as a `LiveEvent::ToolProgress`, which resets
///    the parent turn's stall-watchdog deadline. Without this forwarding a
///    foreground subagent running longer than the engine-turn stall window
///    would be killed as "stalled" while actively working, because the
///    parent emits no live events of its own while awaiting the tool.
///
/// The child's [`ActivityTracker`] observation points stamp
/// `last_activity_ms` while the run is in flight; this driver polls that
/// clock at a quarter of the heartbeat window and forwards only when NEW
/// activity was observed since the last poll. Runs without an activity
/// source (synthetic / test handles) or without a configured heartbeat
/// window are driven unchanged.
async fn drive_with_activity_forwarding(
    run: impl Future<Output = ()>,
    activity: Option<Arc<ActivityTracker>>,
    tasks: Arc<TaskManager>,
    task_id: String,
    progress: crate::tool::ToolProgressHandle,
) {
    let Some(tracker) = activity else {
        run.await;
        return;
    };
    let Some(window) = tasks.heartbeat_interval() else {
        // Stale detection is disabled — forwarding is pointless.
        run.await;
        return;
    };
    let poll = (window / 4).max(Duration::from_millis(1));
    let mut ticker = tokio::time::interval(poll);
    let mut last_seen = tracker.last_activity_ms();
    let mut run = std::pin::pin!(run);
    loop {
        tokio::select! {
            _ = &mut run => return,
            _ = ticker.tick() => {
                let now = tracker.last_activity_ms();
                if now != last_seen {
                    last_seen = now;
                    tasks.heartbeat(&task_id);
                    // The stall-watchdog feed: a custom (volatile,
                    // never-replayed) update carrying no payload — the
                    // tick itself is the signal that the awaited child is
                    // alive and working.
                    progress.send(crate::tool::ToolUpdate {
                        kind: crate::tool::ToolUpdateKind::Custom,
                        text: None,
                        percent: None,
                        custom_kind: Some("subagent.activity".to_string()),
                        custom_data: Some(serde_json::json!({
                            "lastActivityMs": now,
                        })),
                    });
                }
            }
        }
    }
}

/// The settled outcome of a foreground subagent run — the honest status
/// the tool result reports (G15): a stopped run is an ERROR result carrying
/// `status: stopped`, never a silent `status: completed`.
enum ForegroundOutcome {
    Completed,
    Failed,
    Stopped,
}

impl ForegroundOutcome {
    fn from_sink(sink: &TaskManagerSink) -> Self {
        if sink.stopped {
            Self::Stopped
        } else if sink.is_error {
            Self::Failed
        } else {
            Self::Completed
        }
    }
}

/// Format the result for a foreground (synchronous) subagent completion.
///
/// The output is wrapped in a `<result>` block and a simpler hint tells the
/// model it can continue the agent by resuming its id. A stopped run
/// renders `status: stopped` and settles as an error result so the model
/// sees the cancellation instead of a false success.
fn format_foreground_result(
    agent_id: &str,
    subagent_type: &str,
    model: &Option<String>,
    output: &str,
    outcome: &ForegroundOutcome,
) -> ToolResult {
    let status = match outcome {
        ForegroundOutcome::Completed => "completed",
        ForegroundOutcome::Failed => "failed",
        ForegroundOutcome::Stopped => "stopped",
    };
    let model_line = model
        .as_deref()
        .map(|m| format!("resolved_model: {m}\n"))
        .unwrap_or_default();
    let output_block = if matches!(outcome, ForegroundOutcome::Stopped) {
        if output.trim().is_empty() {
            "(the subagent was stopped before it produced any output)".to_string()
        } else {
            format!("{output}\n(the subagent was stopped before it completed)")
        }
    } else {
        output.to_string()
    };
    let text = format!(
        "agent_id: {agent_id}\n\
         actual_subagent_type: {subagent_type}\n\
         {model_line}\
         status: {status}\n\
         <result>\n{output_block}\n</result>\n\
         <resume_hint>Use resume={agent_id} to continue this agent's work</resume_hint>"
    );
    match outcome {
        ForegroundOutcome::Completed => ToolResult::success(text),
        ForegroundOutcome::Failed | ForegroundOutcome::Stopped => ToolResult::error(text),
    }
}

/// Format the result for a foreground subagent run that exceeded the
/// per-subagent timeout (D-40).
///
/// The output is an ERROR result (the run produced no complete answer) that
/// carries `status: failed`, the partial output observed so far, the timeout
/// line, and the resume hint — the model can continue the agent's work by
/// resuming its id (mirroring the TS agentTool timeout rendering).
fn format_foreground_timeout_result(
    agent_id: &str,
    subagent_type: &str,
    model: &Option<String>,
    partial_output: &str,
    secs: u64,
) -> ToolResult {
    let model_line = model
        .as_deref()
        .map(|m| format!("resolved_model: {m}\n"))
        .unwrap_or_default();
    let output_block = if partial_output.trim().is_empty() {
        "(the subagent produced no output before the timeout)".to_string()
    } else {
        partial_output.to_string()
    };
    ToolResult::error(format!(
        "agent_id: {agent_id}\n\
         actual_subagent_type: {subagent_type}\n\
         {model_line}\
         status: failed\n\
         Agent timed out after {secs}s. Re-run the task as a smaller \
         step, or resume the agent to redo any unfinished tool call.\n\
         <result>\n{output_block}\n</result>\n\
         <resume_hint>Use resume={agent_id} to continue this agent's work</resume_hint>"
    ))
}

/// Format the result for a background subagent (returns immediately).
///
/// The task runs detached: the model gets its task_id plus a resume hint so
/// it can continue the agent's work once the output is available (via
/// `TaskOutput` or the completion notification).
fn format_background_result(
    task_id: &str,
    agent_id: &str,
    subagent_type: &str,
    model: &Option<String>,
) -> ToolResult {
    let model_line = model
        .as_deref()
        .map(|m| format!("resolved_model: {m}\n"))
        .unwrap_or_default();
    ToolResult::success(format!(
        "task_id: {task_id}\n\
         agent_id: {agent_id}\n\
         actual_subagent_type: {subagent_type}\n\
         {model_line}\
         status: running (background)\n\n\
         Use TaskOutput to check progress and retrieve the result.\n\n\
         <result>\n(The agent output will appear here once the background task \
         completes; retrieve it with TaskOutput.)\n</result>\n\
         <resume_hint>Use resume={agent_id} to continue this agent's work</resume_hint>\n\
         <next_step>Check the agent's output above and decide whether to continue \
         or integrate the results.</next_step>"
    ))
}

// ---------------------------------------------------------------------------
// TaskManagerSink — bridges SubagentTask into TaskManager
// ---------------------------------------------------------------------------

/// Drop-guard keeping a foreground subagent's tracked task from going
/// orphaned when the driving future is dropped without completing (the
/// parent turn was killed — the `message.send` stall watchdog drops the
/// whole turn future, taking the inline `SubagentTask::start` driver with
/// it). On drop it cancels the child token (any detached work observes
/// cancellation) and stops the tracked entry; the TaskManager's
/// first-terminal-wins rule keeps the normal, timeout, and cancellation
/// settles from being overwritten — the guard only lands when nothing
/// else settled.
struct ForegroundOrphanGuard {
    tasks: Arc<TaskManager>,
    task_id: String,
    cancel: CancelToken,
}

impl Drop for ForegroundOrphanGuard {
    fn drop(&mut self) {
        self.cancel.cancel();
        self.tasks.stop(&self.task_id);
    }
}

/// A [`TaskSink`] backed by a [`TaskManager`] entry.
struct TaskManagerSink {
    task_id: String,
    manager: Arc<TaskManager>,
    cancel: CancelToken,
    /// Accumulated output (also written to TaskManager).
    output: String,
    /// Whether the task settled as an error.
    is_error: bool,
    /// Whether the task settled as stopped (G15) — reported distinctly
    /// from both completed and failed.
    stopped: bool,
}

impl TaskManagerSink {
    fn new(task_id: String, manager: Arc<TaskManager>, cancel: CancelToken) -> Self {
        Self {
            task_id,
            manager,
            cancel,
            output: String::new(),
            is_error: false,
            stopped: false,
        }
    }
}

impl TaskSink for TaskManagerSink {
    fn append_output(&mut self, data: &str) {
        self.output.push_str(data);
        // Output is activity: refresh the heartbeat so a live subagent
        // never reads as stale (B1 T4 — activity-as-liveness; the
        // engine-activity face of the same rule is the forwarding in
        // [`drive_with_activity_heartbeats`], which covers the whole run,
        // not just terminal output).
        self.manager.heartbeat(&self.task_id);
        self.manager.append_output(&self.task_id, data);
    }

    fn settle(&mut self, status: crate::task::TaskStatus, _stop_reason: Option<String>) {
        match status {
            crate::task::TaskStatus::Completed { exit_code } => {
                self.manager.complete(&self.task_id, exit_code);
            }
            crate::task::TaskStatus::Failed(msg) => {
                self.is_error = true;
                self.manager.fail(&self.task_id, msg);
            }
            crate::task::TaskStatus::Stopped => {
                // G15: a stopped run is its own outcome — never rendered as
                // a silent success.
                self.stopped = true;
                self.manager.stop(&self.task_id);
            }
            crate::task::TaskStatus::Running => {
                // Should not happen in settle, but handle gracefully.
                tracing::warn!("settle() called with Running status — ignoring");
            }
        }
    }

    fn signal(&self) -> &CancelToken {
        &self.cancel
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cancel::CancelToken;
    use crate::error::EngineResult;
    use crate::subagent::SubagentHandle;
    use crate::tool::ToolOutput;

    /// A mock SubagentHost for testing.
    struct MockHost {
        result: String,
        should_fail: bool,
    }
    impl SubagentHost for MockHost {
        fn spawn(&self, request: SpawnRequest) -> EngineResult<SubagentHandle> {
            let agent_id = format!("agent-mock-{}", request.profile_name);
            if self.should_fail {
                Ok(SubagentHandle::from_error(
                    agent_id,
                    request.profile_name,
                    request.model,
                    "mock failure".into(),
                ))
            } else {
                Ok(SubagentHandle::from_result(
                    agent_id,
                    request.profile_name,
                    request.model,
                    self.result.clone(),
                ))
            }
        }

        fn resume(&self, request: ResumeRequest) -> EngineResult<SubagentHandle> {
            Err(EngineError::RequestInvalid(format!(
                "resume not implemented in mock for {}",
                request.agent_id
            )))
        }
    }

    /// The permissive background gate every plain test uses — the gating
    /// surface itself has dedicated tests.
    fn open_gate() -> Arc<dyn Fn() -> bool + Send + Sync> {
        Arc::new(|| true)
    }

    fn make_tool(result: &str) -> AgentTool {
        AgentTool::new(
            Arc::new(MockHost {
                result: result.to_string(),
                should_fail: false,
            }),
            Arc::new(TaskManager::new()),
            "test-session".to_string(),
            None,
            open_gate(),
        )
    }

    fn make_failing_tool() -> AgentTool {
        AgentTool::new(
            Arc::new(MockHost {
                result: String::new(),
                should_fail: true,
            }),
            Arc::new(TaskManager::new()),
            "test-session".to_string(),
            None,
            open_gate(),
        )
    }

    /// Given a manager already at its background capacity, When the Agent
    /// tool is called with run_in_background, Then the call is rejected
    /// BEFORE the host spawns — no `subagent.spawned` side effect, no
    /// children entry — with the TS parity capacity message.
    #[tokio::test]
    async fn should_reject_a_background_dispatch_at_capacity_before_spawning() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct CountingHost {
            spawns: AtomicUsize,
        }
        impl SubagentHost for CountingHost {
            fn spawn(&self, request: SpawnRequest) -> EngineResult<SubagentHandle> {
                self.spawns.fetch_add(1, Ordering::SeqCst);
                Ok(SubagentHandle::from_result(
                    "agent-counted".into(),
                    request.profile_name,
                    request.model,
                    "done".into(),
                ))
            }
            fn resume(&self, request: ResumeRequest) -> EngineResult<SubagentHandle> {
                Err(EngineError::RequestInvalid(format!(
                    "resume not implemented in mock for {}",
                    request.agent_id
                )))
            }
        }

        let tasks = Arc::new(TaskManager::new().with_max_running_tasks(1));
        tasks
            .register_background(
                TaskKind::Process {
                    command: None,
                    dispatcher: None,
                },
                "occupant".into(),
                "test-session".into(),
                None,
            )
            .expect("the first background task fits");
        let host = Arc::new(CountingHost {
            spawns: AtomicUsize::new(0),
        });
        let tool = AgentTool::new(
            host.clone(),
            tasks,
            "test-session".to_string(),
            None,
            open_gate(),
        );

        let result = run_tool(
            &tool,
            json!({
                "prompt": "p",
                "description": "d",
                "run_in_background": true
            }),
        )
        .await;

        assert!(result.is_error, "the over-capacity dispatch must error");
        let ToolOutput::Text(text) = &result.output else {
            panic!("text");
        };
        assert!(
            text.contains(crate::task::TASK_CAPACITY_MESSAGE),
            "the capacity message must reach the model: {text}"
        );
        assert_eq!(
            host.spawns.load(Ordering::SeqCst),
            0,
            "the rejection must happen BEFORE the host spawns — no \
             subagent.spawned side effect, no children entry"
        );
    }

    /// Given a subagent run that is cancelled before completing, When the
    /// foreground result is formatted, Then it is an ERROR result carrying
    /// `status: stopped` — the pre-fix code rendered a silent
    /// `status: completed` with empty output (G15).
    #[tokio::test]
    async fn should_report_an_error_with_stopped_status_when_the_subagent_is_cancelled() {
        struct CancellingHost;
        impl SubagentHost for CancellingHost {
            fn spawn(&self, request: SpawnRequest) -> EngineResult<SubagentHandle> {
                Ok(SubagentHandle::new(
                    format!("agent-cancel-{}", request.profile_name),
                    request.profile_name,
                    request.model,
                    Box::pin(async move { Err(crate::subagent::SubagentError::Cancelled) }),
                ))
            }
            fn resume(&self, _request: ResumeRequest) -> EngineResult<SubagentHandle> {
                Err(EngineError::RequestInvalid("not implemented".into()))
            }
        }
        let tool = AgentTool::new(
            Arc::new(CancellingHost),
            Arc::new(TaskManager::new()),
            "test-session".to_string(),
            None,
            open_gate(),
        );
        let result = run_tool(
            &tool,
            json!({
                "prompt": "do something",
                "description": "cancel test"
            }),
        )
        .await;
        assert!(result.is_error, "a cancelled run must be an error result");
        let ToolOutput::Text(text) = &result.output else {
            panic!("expected text output");
        };
        assert!(text.contains("status: stopped"), "text: {text}");
        assert!(text.contains("was stopped"), "text: {text}");
        assert!(
            !text.contains("status: completed"),
            "a cancelled run must never render completed: {text}"
        );
    }

    async fn run_tool(tool: &AgentTool, input: serde_json::Value) -> ToolResult {
        let execution = tool.resolve_execution(input).expect("resolve");
        match execution {
            ToolExecution::Runnable { execute, .. } => {
                let ctx = ExecutableToolContext {
                    wait_registry: None,
                    turn_id: "test-turn".into(),
                    tool_call_id: "test-call".into(),
                    cancel: CancelToken::new(),
                    progress: crate::tool::ToolProgressHandle::default(),
                };
                execute(ctx).await
            }
            ToolExecution::Error(err) => ToolResult::error(err.message),
        }
    }

    /// Given an Agent tool dispatch with an explicit model, When the task is
    /// registered (background and foreground alike), Then the tracked task
    /// row carries the RESOLVED model the spawn handle reports — the same
    /// value the `subagent.spawned` event delivers — so task-list consumers
    /// (the web's per-agent model badge) survive polling rebuilds.
    #[tokio::test]
    async fn should_register_the_task_with_the_resolved_model_when_spawned_with_one() {
        // A host that resolves every spawn onto a fixed model (the way the
        // production host resolves explicit → profile default → caller).
        struct ResolvingHost;
        impl SubagentHost for ResolvingHost {
            fn spawn(&self, request: SpawnRequest) -> EngineResult<SubagentHandle> {
                Ok(SubagentHandle::from_result(
                    format!("agent-{}", request.profile_name),
                    request.profile_name,
                    Some("resolved-model".to_string()),
                    "done".into(),
                ))
            }
            fn resume(&self, _request: ResumeRequest) -> EngineResult<SubagentHandle> {
                Err(EngineError::RequestInvalid("not implemented".into()))
            }
        }
        for run_in_background in [true, false] {
            let tasks = Arc::new(TaskManager::new());
            let tool = AgentTool::new(
                Arc::new(ResolvingHost),
                Arc::clone(&tasks),
                "test-session".to_string(),
                None,
                open_gate(),
            );

            let result = run_tool(
                &tool,
                json!({
                    "prompt": "p",
                    "description": "d",
                    "run_in_background": run_in_background
                }),
            )
            .await;
            assert!(
                !result.is_error,
                "background={run_in_background}: {result:?}"
            );

            let task = tasks
                .list()
                .into_iter()
                .find(|t| {
                    matches!(
                        &t.kind,
                        TaskKind::Agent { model: Some(m), .. } if m == "resolved-model"
                    )
                })
                .unwrap_or_else(|| {
                    panic!(
                        "background={run_in_background}: the tracked task must carry the \
                         resolved model, got {:?}",
                        tasks.list()
                    )
                });
            assert_eq!(task.session_id, "test-session");
        }
    }

    #[tokio::test]
    async fn should_return_result_when_subagent_completes_successfully() {
        let tool = make_tool("task completed successfully");
        let result = run_tool(
            &tool,
            json!({
                "prompt": "do something",
                "description": "test task"
            }),
        )
        .await;
        assert!(!result.is_error, "should not be an error");
        let ToolOutput::Text(text) = &result.output else {
            panic!("expected text output");
        };
        assert!(text.contains("status: completed"), "text: {text}");
        assert!(text.contains("task completed successfully"), "text: {text}");
        assert!(text.contains("agent_id: agent-mock-coder"), "text: {text}");
    }

    #[tokio::test]
    async fn should_return_error_when_subagent_fails() {
        let tool = make_failing_tool();
        let result = run_tool(
            &tool,
            json!({
                "prompt": "do something",
                "description": "failing task"
            }),
        )
        .await;
        assert!(result.is_error, "should be an error");
        let ToolOutput::Text(text) = &result.output else {
            panic!("expected text output");
        };
        assert!(text.contains("status: failed"), "text: {text}");
        assert!(text.contains("mock failure"), "text: {text}");
    }

    #[tokio::test]
    async fn should_return_error_when_resume_and_subagent_type_both_specified() {
        let tool = make_tool("ok");
        let result = run_tool(
            &tool,
            json!({
                "prompt": "do something",
                "description": "test",
                "subagent_type": "coder",
                "resume": "agent-123"
            }),
        )
        .await;
        assert!(result.is_error, "should reject both resume + subagent_type");
        let ToolOutput::Text(text) = &result.output else {
            panic!("expected text output");
        };
        assert!(text.contains("Cannot specify both"), "text: {text}");
    }

    #[tokio::test]
    async fn should_return_task_id_when_running_in_background() {
        let tool = make_tool("bg result");
        let result = run_tool(
            &tool,
            json!({
                "prompt": "background task",
                "description": "bg test",
                "run_in_background": true
            }),
        )
        .await;
        assert!(!result.is_error, "should not be an error");
        let ToolOutput::Text(text) = &result.output else {
            panic!("expected text output");
        };
        assert!(text.contains("task_id:"), "text: {text}");
        assert!(
            text.contains("status: running (background)"),
            "text: {text}"
        );
    }

    #[tokio::test]
    async fn should_use_default_coder_profile_when_subagent_type_not_specified() {
        let tool = make_tool("ok");
        let result = run_tool(
            &tool,
            json!({
                "prompt": "test",
                "description": "default profile"
            }),
        )
        .await;
        let ToolOutput::Text(text) = &result.output else {
            panic!("expected text output");
        };
        assert!(
            text.contains("actual_subagent_type: coder"),
            "should default to coder: {text}"
        );
    }

    #[tokio::test]
    async fn should_use_custom_profile_when_subagent_type_specified() {
        let tool = make_tool("ok");
        let result = run_tool(
            &tool,
            json!({
                "prompt": "test",
                "description": "custom profile",
                "subagent_type": "explore"
            }),
        )
        .await;
        let ToolOutput::Text(text) = &result.output else {
            panic!("expected text output");
        };
        assert!(
            text.contains("actual_subagent_type: explore"),
            "should use explore: {text}"
        );
    }

    /// Given a background agent invocation, when the result is formatted, then
    /// it includes a `<resume_hint>` naming the agent_id so the model can
    /// continue the agent's work.
    #[tokio::test]
    async fn should_include_resume_hint_in_background_agent_result() {
        let tool = make_tool("bg result");
        let result = run_tool(
            &tool,
            json!({
                "prompt": "background task",
                "description": "bg test",
                "run_in_background": true
            }),
        )
        .await;
        assert!(!result.is_error, "should not be an error");
        let ToolOutput::Text(text) = &result.output else {
            panic!("expected text output");
        };
        assert!(
            text.contains("<resume_hint>Use resume=agent-mock-coder to continue this agent's work</resume_hint>"),
            "background result should carry a resume hint: {text}"
        );
        assert!(text.contains("agent_id: agent-mock-coder"), "text: {text}");
    }

    /// Given a foreground agent invocation, when the result is formatted, then
    /// it includes a resume hint pointing the model back to the agent.
    #[tokio::test]
    async fn should_include_resume_hint_in_foreground_agent_result() {
        let tool = make_tool("done");
        let result = run_tool(
            &tool,
            json!({
                "prompt": "foreground task",
                "description": "fg test"
            }),
        )
        .await;
        assert!(!result.is_error, "should not be an error");
        let ToolOutput::Text(text) = &result.output else {
            panic!("expected text output");
        };
        assert!(
            text.contains("<resume_hint>Use resume=agent-mock-coder to continue this agent's work</resume_hint>"),
            "foreground result should carry a resume hint: {text}"
        );
        assert!(
            text.contains("<result>"),
            "foreground output should be wrapped: {text}"
        );
    }

    /// Given a background agent invocation, when the result is formatted, then
    /// it includes a `<next_step>` suggestion telling the model to check the
    /// agent's output and decide whether to continue or integrate.
    #[tokio::test]
    async fn should_include_next_step_suggestion_in_agent_result() {
        let tool = make_tool("bg result");
        let result = run_tool(
            &tool,
            json!({
                "prompt": "background task",
                "description": "bg test",
                "run_in_background": true
            }),
        )
        .await;
        assert!(!result.is_error, "should not be an error");
        let ToolOutput::Text(text) = &result.output else {
            panic!("expected text output");
        };
        assert!(
            text.contains("<next_step>"),
            "background result should carry a next_step suggestion: {text}"
        );
        assert!(
            text.contains("decide whether to continue or integrate the results"),
            "next_step should tell the model what to do next: {text}"
        );
    }

    // ===== B1 T5: background gating, foreground timeout, capacity =====

    /// Given an agent whose enabled tool set omits TaskOutput, When the
    /// Agent tool is called with run_in_background, Then the call is
    /// rejected BEFORE spawning with the TS parity gating message
    /// (agent.ts:85).
    #[tokio::test]
    async fn should_reject_background_dispatch_when_task_tools_are_not_all_active() {
        let mut tool_manager = crate::tool_registry::ToolManager::new();
        tool_manager.set_active_tools(vec!["Agent".into(), "TaskList".into(), "TaskStop".into()]);
        // TaskOutput is absent.
        let tm = Arc::new(std::sync::RwLock::new(tool_manager));
        let gate: Arc<dyn Fn() -> bool + Send + Sync> = Arc::new(move || {
            ["TaskList", "TaskOutput", "TaskStop"]
                .iter()
                .all(|n| tm.read().map(|m| m.is_tool_active(n)).unwrap_or(false))
        });
        let tool = AgentTool::new(
            Arc::new(MockHost {
                result: "x".into(),
                should_fail: false,
            }),
            Arc::new(TaskManager::new()),
            "s1".into(),
            None,
            gate,
        );

        let result = run_tool(
            &tool,
            json!({
                "prompt": "p",
                "description": "d",
                "run_in_background": true
            }),
        )
        .await;

        assert!(result.is_error);
        let ToolOutput::Text(text) = &result.output else {
            panic!("text");
        };
        assert!(
            text.contains("Background agent execution is not available"),
            "{text}"
        );
        assert!(
            text.contains("TaskList, TaskOutput, and TaskStop"),
            "{text}"
        );
    }

    /// Given a foreground subagent whose completion hangs past a 100ms
    /// timeout, When the Agent tool awaits it, Then the child cancel token
    /// fires, the result is an error carrying the timeout message and a
    /// resume hint.
    #[tokio::test]
    async fn should_time_out_a_foreground_subagent_when_the_run_exceeds_the_timeout() {
        struct HangingHost;
        impl SubagentHost for HangingHost {
            fn spawn(&self, request: SpawnRequest) -> EngineResult<SubagentHandle> {
                let cancel = request.cancel.clone();
                Ok(SubagentHandle::new(
                    format!("agent-{}", request.profile_name),
                    request.profile_name,
                    request.model,
                    Box::pin(async move {
                        cancel.cancelled().await; // hang until cancelled
                        Err(crate::subagent::SubagentError::Cancelled)
                    }),
                ))
            }
            fn resume(&self, _r: ResumeRequest) -> EngineResult<SubagentHandle> {
                Err(EngineError::RequestInvalid("n/a".into()))
            }
        }
        let gate: Arc<dyn Fn() -> bool + Send + Sync> = Arc::new(|| true);
        let tool = AgentTool::new(
            Arc::new(HangingHost),
            Arc::new(TaskManager::new()),
            "s1".into(),
            Some(Duration::from_millis(100)),
            gate,
        );

        let started = std::time::Instant::now();
        let result = run_tool(&tool, json!({"prompt": "p", "description": "d"})).await;

        assert!(result.is_error, "a timed-out run must be an error result");
        let ToolOutput::Text(text) = &result.output else {
            panic!("text");
        };
        assert!(text.contains("timed out"), "{text}");
        assert!(text.contains("<resume_hint>"), "{text}");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "must return promptly"
        );
    }

    /// Given a manager capped at one background task, When the Agent tool
    /// dispatches two background subagents, Then the second registration's
    /// capacity rejection surfaces as the tool result error (TS
    /// assertCanRegister parity).
    #[tokio::test]
    async fn should_return_the_capacity_error_when_the_background_limit_is_reached() {
        let gate: Arc<dyn Fn() -> bool + Send + Sync> = Arc::new(|| true);
        let tasks = Arc::new(TaskManager::new().with_max_running_tasks(1));
        let tool = AgentTool::new(
            Arc::new(MockHost {
                result: "x".into(),
                should_fail: false,
            }),
            Arc::clone(&tasks),
            "s1".into(),
            None,
            gate,
        );

        let first = run_tool(
            &tool,
            json!({"prompt": "p", "description": "one", "run_in_background": true}),
        )
        .await;
        assert!(
            !first.is_error,
            "first background dispatch fits the capacity"
        );

        let second = run_tool(
            &tool,
            json!({"prompt": "p", "description": "two", "run_in_background": true}),
        )
        .await;
        assert!(
            second.is_error,
            "second background dispatch must be rejected"
        );
        let ToolOutput::Text(text) = &second.output else {
            panic!("text");
        };
        assert!(
            text.contains(crate::task::TASK_CAPACITY_MESSAGE),
            "the TS parity capacity message must surface: {text}"
        );
    }

    #[tokio::test]
    async fn should_return_error_when_prompt_is_missing() {
        let tool = make_tool("ok");
        let result = tool.resolve_execution(json!({"description": "no prompt"}));
        assert!(result.is_err(), "should fail without prompt");
    }

    #[tokio::test]
    async fn should_return_error_when_description_is_missing() {
        let tool = make_tool("ok");
        let result = tool.resolve_execution(json!({"prompt": "no desc"}));
        assert!(result.is_err(), "should fail without description");
    }

    #[tokio::test]
    async fn should_include_resolved_model_when_model_override_specified() {
        let tool = make_tool("ok");
        let result = run_tool(
            &tool,
            json!({
                "prompt": "test",
                "description": "model override",
                "model": "gpt-4o"
            }),
        )
        .await;
        let ToolOutput::Text(text) = &result.output else {
            panic!("expected text output");
        };
        assert!(
            text.contains("resolved_model: gpt-4o"),
            "should include model: {text}"
        );
    }

    /// A mock host that returns profile info for `available_profiles()`.
    struct ProfiledMockHost {
        result: String,
        profiles: Vec<crate::subagent::SubagentProfileInfo>,
        curated: Vec<crate::model_catalog::ModelCatalogEntry>,
    }

    impl SubagentHost for ProfiledMockHost {
        fn spawn(&self, request: SpawnRequest) -> EngineResult<SubagentHandle> {
            Ok(SubagentHandle::from_result(
                format!("agent-mock-{}", request.profile_name),
                request.profile_name,
                request.model,
                self.result.clone(),
            ))
        }

        fn resume(&self, _request: ResumeRequest) -> EngineResult<SubagentHandle> {
            Err(EngineError::RequestInvalid("not implemented".into()))
        }

        fn available_profiles(&self) -> Vec<crate::subagent::SubagentProfileInfo> {
            self.profiles.clone()
        }

        fn curated_models(&self) -> Vec<crate::model_catalog::ModelCatalogEntry> {
            self.curated.clone()
        }
    }

    fn catalog_entry(
        id: &str,
        description: &str,
        max_context_size: Option<usize>,
        capabilities: Vec<&str>,
    ) -> crate::model_catalog::ModelCatalogEntry {
        crate::model_catalog::ModelCatalogEntry {
            id: id.to_string(),
            model: format!("model-{id}"),
            provider: "test".to_string(),
            description: Some(description.to_string()),
            max_context_size,
            capabilities: capabilities.into_iter().map(String::from).collect(),
            default_effort: None,
        }
    }

    #[tokio::test]
    async fn should_include_available_profiles_in_description_when_host_provides_them() {
        let host = Arc::new(ProfiledMockHost {
            result: "ok".to_string(),
            profiles: vec![
                crate::subagent::SubagentProfileInfo {
                    name: "coder".to_string(),
                    model: Some("test-model-a".to_string()),
                    description: None,
                    when_to_use: None,
                    tools: None,
                    disallowed_tools: None,
                    model_capabilities: vec!["tool-use".to_string()],
                    available_models: vec!["sonnet".to_string()],
                },
                crate::subagent::SubagentProfileInfo {
                    name: "explore".to_string(),
                    model: Some("test-model-b".to_string()),
                    description: None,
                    when_to_use: None,
                    tools: None,
                    disallowed_tools: None,
                    model_capabilities: Vec::new(),
                    available_models: vec!["haiku".to_string()],
                },
            ],
            curated: vec![
                catalog_entry(
                    "sonnet",
                    "The balanced model",
                    Some(200_000),
                    vec!["tool-use"],
                ),
                catalog_entry("haiku", "Fast responses", Some(100_000), vec![]),
            ],
        });
        let tool = AgentTool::new(
            host,
            Arc::new(TaskManager::new()),
            "s1".to_string(),
            None,
            open_gate(),
        );

        let desc = tool.description();
        assert!(desc.contains("coder"), "should list coder: {desc}");
        assert!(
            !desc.contains("test-model-a") && !desc.contains("test-model-b"),
            "profile model ids must not be rendered into the description: {desc}"
        );
        assert!(desc.contains("explore"), "should list explore: {desc}");
        assert!(
            desc.contains("Available profiles"),
            "should have profiles section: {desc}"
        );
        assert!(
            desc.contains("Available models"),
            "should have models section: {desc}"
        );
        assert!(desc.contains("- sonnet"), "should list sonnet: {desc}");
        assert!(desc.contains("- haiku"), "should list haiku: {desc}");
        assert!(
            !desc.contains("claude-sonnet") && !desc.contains("claude-haiku"),
            "A8 guard: no hardcoded vendor model names in the description: {desc}"
        );
    }

    /// Given a model-less profile (the post-T1 builtin shape) alongside a
    /// user-configured profile that declares a model, When the description
    /// is built, Then the profile lines render bare names — no `(model)`
    /// suffix on either, and the configured profile's model id never
    /// reaches the description (spec 11 R7/A8: the dispatch description
    /// carries no concrete model name that is not catalog-sourced). A
    /// profile's own `description` (duties text, not a model name) DOES
    /// render inline in the dispatch entry header (brief Interfaces).
    #[test]
    fn should_not_render_model_suffix_when_profile_has_no_model() {
        let host = Arc::new(ProfiledMockHost {
            result: "ok".to_string(),
            profiles: vec![
                crate::subagent::SubagentProfileInfo {
                    name: "coder".to_string(),
                    model: None,
                    description: Some("Writes and reviews code".to_string()),
                    when_to_use: None,
                    tools: None,
                    disallowed_tools: None,
                    model_capabilities: Vec::new(),
                    available_models: Vec::new(),
                },
                crate::subagent::SubagentProfileInfo {
                    name: "custom".to_string(),
                    model: Some("user-configured-model".to_string()),
                    description: None,
                    when_to_use: None,
                    tools: None,
                    disallowed_tools: None,
                    model_capabilities: Vec::new(),
                    available_models: Vec::new(),
                },
            ],
            curated: vec![catalog_entry(
                "alpha",
                "The alpha model",
                Some(100_000),
                vec![],
            )],
        });
        let tool = AgentTool::new(
            host,
            Arc::new(TaskManager::new()),
            "s1".to_string(),
            None,
            open_gate(),
        );

        let desc = tool.description();
        assert!(
            desc.contains("\n- coder: Writes and reviews code\n"),
            "a model-less profile renders its name and description inline: {desc}"
        );
        assert!(
            desc.contains("\n- custom\n"),
            "a profile without a description renders its bare name — no \
             dangling dash: {desc}"
        );
        assert!(
            !desc.contains("user-configured-model"),
            "a profile's model id must not be rendered into the description: {desc}"
        );
        assert!(
            !desc.contains("()"),
            "no empty parentheses from a dropped model suffix: {desc}"
        );
    }

    /// Given profiles that declare a tool whitelist, an empty whitelist, a
    /// denylist, and neither list, When the description is built, Then each
    /// profile line is followed by a `Tools:` annotation mirroring the
    /// declared surface: explicit whitelist joined by commas, `none` for an
    /// empty whitelist, `all except …` for a denylist, and `all` when
    /// nothing is declared (v2 buildProfileDescriptions semantics, minus
    /// the externally-restricted intersection which scope controls).
    #[test]
    fn should_annotate_profiles_with_their_declared_tools_when_building_the_description() {
        let host = Arc::new(ProfiledMockHost {
            result: "ok".to_string(),
            profiles: vec![
                crate::subagent::SubagentProfileInfo {
                    name: "coder".to_string(),
                    model: None,
                    description: Some("Writes and reviews code".to_string()),
                    when_to_use: Some("when implementation work is needed".to_string()),
                    tools: Some(vec!["Read".to_string(), "Edit".to_string()]),
                    disallowed_tools: None,
                    model_capabilities: Vec::new(),
                    available_models: Vec::new(),
                },
                crate::subagent::SubagentProfileInfo {
                    name: "bare".to_string(),
                    model: None,
                    description: None,
                    when_to_use: None,
                    tools: Some(vec![]),
                    disallowed_tools: None,
                    model_capabilities: Vec::new(),
                    available_models: Vec::new(),
                },
                crate::subagent::SubagentProfileInfo {
                    name: "wide".to_string(),
                    model: None,
                    description: None,
                    when_to_use: None,
                    tools: None,
                    disallowed_tools: None,
                    model_capabilities: Vec::new(),
                    available_models: Vec::new(),
                },
                crate::subagent::SubagentProfileInfo {
                    name: "restricted".to_string(),
                    model: None,
                    description: None,
                    when_to_use: None,
                    tools: None,
                    disallowed_tools: Some(vec!["Write".to_string()]),
                    model_capabilities: Vec::new(),
                    available_models: Vec::new(),
                },
            ],
            curated: vec![],
        });
        let tool = AgentTool::new(
            host,
            Arc::new(TaskManager::new()),
            "s1".to_string(),
            None,
            open_gate(),
        );

        let desc = tool.description();
        assert!(
            desc.contains("- coder: Writes and reviews code when implementation work is needed\n"),
            "description and when_to_use join after the name: {desc}"
        );
        assert!(
            desc.contains("\n  Tools: Read, Edit\n"),
            "an explicit whitelist renders as a Tools line: {desc}"
        );
        assert!(
            desc.contains("- bare\n  Tools: none\n"),
            "an empty whitelist renders as Tools: none: {desc}"
        );
        assert!(
            desc.contains("- wide\n  Tools: all\n"),
            "no declared lists renders as Tools: all: {desc}"
        );
        assert!(
            desc.contains("- restricted\n  Tools: all except Write\n"),
            "a denylist renders as Tools: all except …: {desc}"
        );
    }

    /// Given a host whose curated catalog lists exactly `alpha` and `beta`,
    /// When the description is built, Then the only model names in the
    /// description are the catalog-sourced aliases — introduced by the
    /// fixed resolution sentence — while the profile's own raw model id
    /// and any non-catalog alias never appear (spec 11 R7/A8).
    #[test]
    fn should_render_only_catalog_sourced_models_in_description() {
        let host = Arc::new(ProfiledMockHost {
            result: "ok".to_string(),
            profiles: vec![crate::subagent::SubagentProfileInfo {
                name: "coder".to_string(),
                model: Some("raw-profile-model-id".to_string()),
                description: None,
                when_to_use: None,
                tools: None,
                disallowed_tools: None,
                model_capabilities: Vec::new(),
                available_models: vec!["alpha".to_string(), "beta".to_string()],
            }],
            curated: vec![
                catalog_entry("alpha", "The alpha model", Some(100_000), vec![]),
                catalog_entry("beta", "The beta model", Some(200_000), vec![]),
            ],
        });
        let tool = AgentTool::new(
            host,
            Arc::new(TaskManager::new()),
            "s1".to_string(),
            None,
            open_gate(),
        );

        let desc = tool.description();

        // The fixed adjudication sentence introduces the curated list.
        assert!(
            desc.contains(
                "Model defaults to the dispatcher's current model unless overridden per \
                 agent; available models are listed below."
            ),
            "the fixed resolution sentence precedes the curated list: {desc}"
        );
        assert!(
            desc.contains("- alpha — The alpha model, 98k context"),
            "catalog-sourced alias alpha renders: {desc}"
        );
        assert!(
            desc.contains("- beta — The beta model, 195k context"),
            "catalog-sourced alias beta renders: {desc}"
        );
        assert!(
            !desc.contains("gamma"),
            "an alias outside the catalog never appears: {desc}"
        );
        assert!(
            !desc.contains("raw-profile-model-id"),
            "the profile's raw model id never appears: {desc}"
        );
        assert!(
            !desc.contains("model-alpha"),
            "the catalog entry's raw model id is not rendered either: {desc}"
        );
    }

    /// should include available models in agent tool description when
    /// catalog is provided
    #[tokio::test]
    async fn should_include_available_models_in_agent_tool_description_when_catalog_is_provided() {
        // Given a host whose profiles carry catalog-derived available_models
        let host = Arc::new(ProfiledMockHost {
            result: "ok".to_string(),
            profiles: vec![crate::subagent::SubagentProfileInfo {
                name: "coder".to_string(),
                model: Some("test-model-a".to_string()),
                description: None,
                when_to_use: None,
                tools: None,
                disallowed_tools: None,
                model_capabilities: Vec::new(),
                available_models: vec!["sonnet".to_string(), "opus".to_string()],
            }],
            curated: vec![
                catalog_entry("sonnet", "The balanced model", Some(200_000), vec![]),
                catalog_entry(
                    "opus",
                    "Heavy lifting for hard tasks",
                    Some(1_000_000),
                    vec!["tool-use", "vision"],
                ),
            ],
        });
        let tool = AgentTool::new(
            host,
            Arc::new(TaskManager::new()),
            "s1".to_string(),
            None,
            open_gate(),
        );

        // When the description is built
        let desc = tool.description();

        // Then the valid model aliases are listed for the LLM to pick from
        assert!(
            desc.contains("Available models"),
            "should have models section: {desc}"
        );
        assert!(desc.contains("- sonnet"), "should list sonnet: {desc}");
        assert!(desc.contains("- opus"), "should list opus: {desc}");
        let params = tool.parameters().to_string();
        assert!(
            params.contains("'Available models' section"),
            "model param description should reference the section: {params}"
        );
    }

    /// should render curated entries with description and context when
    /// building the agent description
    #[tokio::test]
    async fn should_render_curated_entries_with_description_and_context_when_building_agent_description()
     {
        // Given a host providing 2 curated entries (description + context size)
        let host = Arc::new(ProfiledMockHost {
            result: "ok".to_string(),
            profiles: vec![crate::subagent::SubagentProfileInfo {
                name: "coder".to_string(),
                model: Some("test-model-a".to_string()),
                description: None,
                when_to_use: None,
                tools: None,
                disallowed_tools: None,
                model_capabilities: Vec::new(),
                available_models: vec!["sonnet".to_string(), "haiku".to_string()],
            }],
            curated: vec![
                catalog_entry(
                    "sonnet",
                    "The balanced model",
                    Some(200_000),
                    vec!["vision", "tool-use"],
                ),
                catalog_entry("haiku", "Fast responses", Some(100_000), vec![]),
            ],
        });

        // When the agent tool description is built
        let tool = AgentTool::new(
            host,
            Arc::new(TaskManager::new()),
            "s1".to_string(),
            None,
            open_gate(),
        );
        let desc = tool.description();

        // Then the models section renders `- alias — description, Nk context`
        // (plus a capability hint when present), without non-purpose fields
        assert!(
            desc.contains("Available models"),
            "should have models section: {desc}"
        );
        assert!(
            desc.contains("- sonnet — The balanced model, 195k context (vision, tool-use)"),
            "should render alias, description, context and capabilities: {desc}"
        );
        assert!(
            desc.contains("- haiku — Fast responses, 98k context"),
            "should render without capability hint when none set: {desc}"
        );
        assert!(
            !desc.contains("display"),
            "should not leak display-name or other non-purpose fields: {desc}"
        );

        // Given a host with profiles but no curated entries
        let empty_curated = Arc::new(ProfiledMockHost {
            result: "ok".to_string(),
            profiles: vec![crate::subagent::SubagentProfileInfo {
                name: "coder".to_string(),
                model: Some("test-model-a".to_string()),
                description: None,
                when_to_use: None,
                tools: None,
                disallowed_tools: None,
                model_capabilities: Vec::new(),
                available_models: vec!["sonnet".to_string()],
            }],
            curated: Vec::new(),
        });

        // When the description is built
        let tool = AgentTool::new(
            empty_curated,
            Arc::new(TaskManager::new()),
            "s1".to_string(),
            None,
            open_gate(),
        );
        let desc = tool.description();

        // Then the models section is omitted entirely while the profiles
        // section still renders
        assert!(
            !desc.contains("Available models"),
            "should omit models section when nothing is curated: {desc}"
        );
        assert!(
            desc.contains("Available profiles"),
            "profiles section stays: {desc}"
        );
    }

    #[tokio::test]
    async fn should_not_include_profiles_section_when_host_returns_empty_list() {
        // Default MockHost returns empty vec from available_profiles()
        let tool = make_tool("ok");
        let desc = tool.description();
        assert!(
            !desc.contains("Available profiles"),
            "should not have profiles section: {desc}"
        );
    }

    // ===== B1 T4 (activity-as-liveness): heartbeat forwarding =====

    /// Build a real [`ActivityTracker`] (the same construction a TurnFlow
    /// performs) for hosts whose children report engine activity.
    fn activity_tracker() -> std::sync::Arc<crate::turn_flow::ActivityTracker> {
        std::sync::Arc::new(crate::turn_flow::ActivityTracker::new(std::sync::Arc::new(
            std::sync::Mutex::new(crate::compaction::FullCompaction::new(
                crate::compaction::CompactionStrategy::default(),
            )),
        )))
    }

    /// A host whose spawned children optionally keep producing engine
    /// activity (steps stamped on a real activity tracker) while a
    /// configurable completion delay runs — the liveness face the tool's
    /// heartbeat forwarding consumes.
    struct ActiveHost {
        tracker: std::sync::Arc<crate::turn_flow::ActivityTracker>,
        run_for: Duration,
        touch_every: Duration,
        /// Whether the child stamps activity observations during the run.
        touch: bool,
    }

    impl SubagentHost for ActiveHost {
        fn spawn(&self, request: SpawnRequest) -> EngineResult<SubagentHandle> {
            let tracker = std::sync::Arc::clone(&self.tracker);
            let run_for = self.run_for;
            let touch_every = self.touch_every;
            let touch = self.touch;
            let completion = Box::pin(async move {
                let started = std::time::Instant::now();
                let mut step = 1u32;
                while started.elapsed() < run_for {
                    tokio::time::sleep(touch_every).await;
                    if touch {
                        tracker.on_step(step);
                        step += 1;
                    }
                }
                Ok(crate::subagent::SubagentCompletion::from_text("done"))
            });
            Ok(SubagentHandle::new(
                format!("agent-active-{}", request.profile_name),
                request.profile_name,
                request.model,
                completion,
            )
            .with_activity(std::sync::Arc::clone(&self.tracker)))
        }

        fn resume(&self, _request: ResumeRequest) -> EngineResult<SubagentHandle> {
            Err(EngineError::RequestInvalid("not implemented".into()))
        }
    }

    /// A host whose spawned child's ONLY activity is the in-flight-LLM
    /// heartbeat — the child "waits on its provider's first token" for
    /// the whole run, stamping `observe_live(&LlmPending)` instead of
    /// steps. This isolates the incident's exact shape: no deltas, no
    /// tool calls, just the pending heartbeat.
    struct PendingOnlyHost {
        tracker: std::sync::Arc<crate::turn_flow::ActivityTracker>,
        run_for: Duration,
        beat_every: Duration,
    }

    impl SubagentHost for PendingOnlyHost {
        fn spawn(&self, request: SpawnRequest) -> EngineResult<SubagentHandle> {
            let tracker = std::sync::Arc::clone(&self.tracker);
            let run_for = self.run_for;
            let beat_every = self.beat_every;
            let completion = Box::pin(async move {
                let started = std::time::Instant::now();
                while started.elapsed() < run_for {
                    tokio::time::sleep(beat_every).await;
                    tracker.observe_live(&crate::LiveEvent::LlmPending { elapsed_ms: 1 });
                }
                Ok(crate::subagent::SubagentCompletion::from_text("done"))
            });
            Ok(SubagentHandle::new(
                format!("agent-pending-{}", request.profile_name),
                request.profile_name,
                request.model,
                completion,
            )
            .with_activity(std::sync::Arc::clone(&self.tracker)))
        }

        fn resume(&self, _request: ResumeRequest) -> EngineResult<SubagentHandle> {
            Err(EngineError::RequestInvalid("not implemented".into()))
        }
    }
    fn background_task_id(result: &ToolResult) -> String {
        let ToolOutput::Text(text) = &result.output else {
            panic!("expected text output");
        };
        text.lines()
            .find_map(|line| line.strip_prefix("task_id: "))
            .expect("background result carries the task id")
            .to_string()
    }

    /// Given a background subagent that keeps producing engine activity
    /// (steps, tool calls) while its completion future is in flight, When
    /// its runtime far exceeds the heartbeat window, Then the tracked task
    /// never reads as stale (B1 T4 — activity-as-liveness: a live,
    /// working subagent must not trip the stale detector that exists to
    /// catch a dead runner).
    #[tokio::test]
    async fn should_keep_a_background_agent_task_fresh_while_the_child_stays_active() {
        let tracker = activity_tracker();
        let tasks =
            Arc::new(TaskManager::new().with_heartbeat_interval(Duration::from_millis(200)));
        let tool = AgentTool::new(
            Arc::new(ActiveHost {
                tracker: Arc::clone(&tracker),
                run_for: Duration::from_millis(900),
                touch_every: Duration::from_millis(25),
                touch: true,
            }),
            Arc::clone(&tasks),
            "s1".into(),
            None,
            open_gate(),
        );

        let result = run_tool(
            &tool,
            json!({
                "prompt": "p",
                "description": "long active run",
                "run_in_background": true
            }),
        )
        .await;
        assert!(!result.is_error);
        let task_id = background_task_id(&result);

        // Then: at every sample point well past the 200ms liveness window,
        // the still-running task is not stale.
        for _ in 0..6 {
            tokio::time::sleep(Duration::from_millis(150)).await;
            assert!(
                !tasks.is_stale(&task_id),
                "an active subagent must never read as stale (window 200ms)"
            );
        }

        // The run settles normally.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if matches!(
                tasks.get(&task_id).map(|task| task.status),
                Some(crate::task::TaskStatus::Completed { .. })
            ) {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the background task must settle after its run completes"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Given a background subagent whose engine goes silent (no
    /// observation points fire during its run), When the heartbeat window
    /// elapses, Then the tracked task reads as stale — the detector must
    /// still surface a runner that stopped reporting.
    #[tokio::test]
    async fn should_read_as_stale_when_a_background_agent_stops_reporting_activity() {
        let tasks =
            Arc::new(TaskManager::new().with_heartbeat_interval(Duration::from_millis(100)));
        let tool = AgentTool::new(
            Arc::new(ActiveHost {
                tracker: activity_tracker(),
                run_for: Duration::from_millis(600),
                touch_every: Duration::from_millis(50),
                touch: false,
            }),
            Arc::clone(&tasks),
            "s1".into(),
            None,
            open_gate(),
        );

        let result = run_tool(
            &tool,
            json!({
                "prompt": "p",
                "description": "silent run",
                "run_in_background": true
            }),
        )
        .await;
        assert!(!result.is_error);
        let task_id = background_task_id(&result);

        tokio::time::sleep(Duration::from_millis(250)).await;
        assert!(
            tasks.is_stale(&task_id),
            "a subagent that stopped reporting activity must read as stale"
        );
    }

    /// Given a foreground subagent that keeps producing engine activity
    /// while its turn is awaited, When its runtime far exceeds the
    /// heartbeat window, Then the tracked task never reads as stale —
    /// the forwarding covers foreground dispatches too (the sweeper warns
    /// about every non-terminal task, not just detached ones).
    #[tokio::test]
    async fn should_keep_a_foreground_agent_task_fresh_while_the_child_stays_active() {
        let tracker = activity_tracker();
        let tasks =
            Arc::new(TaskManager::new().with_heartbeat_interval(Duration::from_millis(200)));
        let tool = AgentTool::new(
            Arc::new(ActiveHost {
                tracker: Arc::clone(&tracker),
                run_for: Duration::from_millis(900),
                touch_every: Duration::from_millis(25),
                touch: true,
            }),
            Arc::clone(&tasks),
            "s1".into(),
            None,
            open_gate(),
        );

        // A concurrent sampler: while the foreground run is in flight, the
        // tracked running task must never read as stale.
        let sampler_tasks = Arc::clone(&tasks);
        let saw_stale = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let sampler_flag = Arc::clone(&saw_stale);
        let sampler = tokio::spawn(async move {
            loop {
                for task in sampler_tasks.list() {
                    if matches!(task.status, crate::task::TaskStatus::Running)
                        && sampler_tasks.is_stale(&task.id)
                    {
                        sampler_flag.store(true, std::sync::atomic::Ordering::SeqCst);
                    }
                }
                tokio::time::sleep(Duration::from_millis(75)).await;
            }
        });

        let result = run_tool(
            &tool,
            json!({"prompt": "p", "description": "long active fg run"}),
        )
        .await;
        sampler.abort();
        assert!(!result.is_error);
        assert!(
            !saw_stale.load(std::sync::atomic::Ordering::SeqCst),
            "an active foreground subagent must never read as stale"
        );
    }

    /// Given a foreground subagent whose child keeps producing engine
    /// activity while the parent turn awaits the tool call, When the run
    /// is in flight, Then the tool forwards each observed activity bump
    /// through `ctx.progress` as a `custom` ToolUpdate — the
    /// `LiveEvent::ToolProgress` feed that resets the parent turn's stall
    /// watchdog deadline, so a live, working subagent is never mistaken
    /// for a stalled turn (the engine-turn stall false positive).
    #[tokio::test]
    async fn should_forward_child_activity_to_the_parent_tool_progress_channel_while_a_foreground_subagent_runs()
     {
        let tracker = activity_tracker();
        let tasks =
            Arc::new(TaskManager::new().with_heartbeat_interval(Duration::from_millis(200)));
        let tool = AgentTool::new(
            Arc::new(ActiveHost {
                tracker: Arc::clone(&tracker),
                run_for: Duration::from_millis(600),
                touch_every: Duration::from_millis(25),
                touch: true,
            }),
            Arc::clone(&tasks),
            "s1".into(),
            None,
            open_gate(),
        );

        let updates = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink_updates = Arc::clone(&updates);
        let progress = crate::tool::ToolProgressHandle::with_sink(Arc::new(move |update| {
            sink_updates.lock().unwrap().push(update);
        }));

        let execution = tool
            .resolve_execution(json!({"prompt": "p", "description": "live child"}))
            .expect("resolve");
        let ToolExecution::Runnable { execute, .. } = execution else {
            panic!("expected a runnable execution");
        };
        let ctx = ExecutableToolContext {
            wait_registry: None,
            turn_id: "test-turn".into(),
            tool_call_id: "test-call".into(),
            cancel: CancelToken::new(),
            progress,
        };
        let result = execute(ctx).await;
        assert!(!result.is_error, "the run itself must succeed");

        let seen = updates.lock().unwrap();
        assert!(
            !seen.is_empty(),
            "a live foreground child must emit progress ticks so the \
             parent turn's stall deadline keeps resetting"
        );
        assert!(
            seen.iter()
                .all(|u| u.kind == crate::tool::ToolUpdateKind::Custom),
            "the liveness ticks ride the custom update class: {seen:?}"
        );
    }

    /// Given a foreground subagent whose only activity is the
    /// in-flight-LLM heartbeat (the child's turn is parked on
    /// `stream.next()` waiting for the provider's first token — the
    /// session-fdf98268 incident shape), when the parent tool call runs
    /// under activity forwarding, then the child's `LlmPending`
    /// observations keep the parent tool's progress feed advancing —
    /// the activity clock is the forwarding bridge between a waiting
    /// child and the parent turn's stall watchdog.
    #[tokio::test]
    async fn should_forward_llm_pending_heartbeats_to_the_parent_tool_progress_channel_while_the_child_waits_on_its_first_token()
     {
        let tracker = activity_tracker();
        let tasks =
            Arc::new(TaskManager::new().with_heartbeat_interval(Duration::from_millis(200)));
        let tool = AgentTool::new(
            Arc::new(PendingOnlyHost {
                tracker: Arc::clone(&tracker),
                run_for: Duration::from_millis(600),
                beat_every: Duration::from_millis(25),
            }),
            Arc::clone(&tasks),
            "s1".into(),
            None,
            open_gate(),
        );

        let updates = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink_updates = Arc::clone(&updates);
        let progress = crate::tool::ToolProgressHandle::with_sink(Arc::new(move |update| {
            sink_updates.lock().unwrap().push(update);
        }));

        let execution = tool
            .resolve_execution(json!({"prompt": "p", "description": "waiting child"}))
            .expect("resolve");
        let ToolExecution::Runnable { execute, .. } = execution else {
            panic!("expected a runnable execution");
        };
        let ctx = ExecutableToolContext {
            wait_registry: None,
            turn_id: "test-turn".into(),
            tool_call_id: "test-call".into(),
            cancel: CancelToken::new(),
            progress,
        };
        let result = execute(ctx).await;
        assert!(!result.is_error, "the run itself must succeed");

        // Then the parent's progress channel saw liveness ticks — the
        // `LlmPending` → activity-clock → forwarding chain is the
        // incident's missing bridge.
        let seen = updates.lock().unwrap();
        assert!(
            !seen.is_empty(),
            "the child's LlmPending heartbeats must reach the parent's \
             stall-watchdog feed: {seen:?}"
        );
    }
}
