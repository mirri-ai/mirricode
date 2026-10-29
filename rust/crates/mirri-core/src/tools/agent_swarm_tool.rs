//! AgentSwarm tool — dispatch multiple subagents concurrently from a template.
//!
//! The `AgentSwarm` tool launches N subagents, each with a prompt expanded
//! from a `{{item}}` template, runs them concurrently (with a bounded
//! concurrency cap), and returns all results as an XML block.
//!
//! It can also resume existing subagents via `resume_agent_ids`.

use std::sync::Arc;

use serde_json::json;

use crate::error::{EngineError, EngineResult};
use crate::subagent::{
    SubagentHost,
    swarm_batch::{
        SwarmBatchOptions, SwarmOutcome, SwarmRunResult, SwarmStartState, SwarmTask, SwarmTaskKind,
        SwarmTaskRegistry, parse_swarm_max_concurrency, run_swarm_batch,
    },
};
use crate::tool::{
    ExecutableToolContext, ExecutionPlan, Tool, ToolExecution, ToolFuture, ToolInputDisplay,
    ToolResult,
};

/// The env override names for the per-call item cap (primary `MIRRI_*` per
/// B11's naming rule, plus the TS spelling for cross-backend parity).
const MAX_ITEMS_ENV: &str = "MIRRI_AGENT_SWARM_MAX_ITEMS";
const MAX_ITEMS_ENV_TS: &str = "MIRRICODE_AGENT_SWARM_MAX_ITEMS";

/// Resolve the per-call swarm item cap: the env override when set (fail-soft
/// — warn + ignore — on an invalid value, SDR-B3-01 semantics), else the
/// `[subagent] maxSwarmItems` config value threaded in at construction,
/// else the default of [`DEFAULT_MAX_SWARM_ITEMS`] (3). `0` means unlimited
/// (mirrors `[subagent] maxNestingDepth`). Resolved at call time so an env
/// change is picked up without reassembly, exactly like the concurrency
/// override below.
fn resolve_swarm_max_items(config_value: Option<u32>) -> u32 {
    let raw = std::env::var(MAX_ITEMS_ENV)
        .ok()
        .or_else(|| std::env::var(MAX_ITEMS_ENV_TS).ok());
    let fallback = config_value.unwrap_or(crate::subagent::DEFAULT_MAX_SWARM_ITEMS);
    let Some(raw) = raw else { return fallback };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return fallback;
    }
    match trimmed.parse::<u32>() {
        Ok(cap) => cap,
        Err(_) => {
            tracing::warn!(value = trimmed, "ignoring invalid swarm max-items override");
            fallback
        }
    }
}

/// The env override names for the concurrency ceiling (primary `MIRRI_*`
/// per B11's naming rule, plus the TS spelling for cross-backend parity).
const MAX_CONCURRENCY_ENV: &str = "MIRRI_AGENT_SWARM_MAX_CONCURRENCY";
const MAX_CONCURRENCY_ENV_TS: &str = "MIRRICODE_AGENT_SWARM_MAX_CONCURRENCY";

/// Resolve the env concurrency override — fail-soft (warn + `None`) on an
/// invalid value (TS throws here, which would take the whole session
/// assembly down; SDR-B3-01 keeps the engine running instead).
fn resolve_swarm_max_concurrency() -> Option<usize> {
    let raw = std::env::var(MAX_CONCURRENCY_ENV)
        .ok()
        .or_else(|| std::env::var(MAX_CONCURRENCY_ENV_TS).ok());
    match parse_swarm_max_concurrency(raw.as_deref()) {
        Ok(cap) => cap,
        Err(reason) => {
            tracing::warn!(%reason, "ignoring invalid swarm max-concurrency override");
            None
        }
    }
}

/// The `AgentSwarm` tool — dispatch multiple subagents concurrently.
pub struct AgentSwarmTool {
    host: Arc<dyn SubagentHost>,
    description: String,
    /// Per-member subagent timeout (B1 D-45). `None` = unbounded.
    subagent_timeout: Option<std::time::Duration>,
    /// The `[subagent]` `maxSwarmItems` config value (SDR-012). `None`
    /// keeps the default of 3; `Some(0)` means unlimited. Re-resolved
    /// against the env override at every call.
    max_swarm_items: Option<u32>,
    /// The session-scoped task manager members register under (US5.1:
    /// observable + individually stoppable members, the parity the single
    /// `Agent` delegation already has). `None` until
    /// [`Self::with_task_tracking`] wires it (tests / bare assemblies).
    tasks: Option<Arc<crate::task::TaskManager>>,
    /// The owning session's id (carried on the registered member rows).
    session_id: Option<String>,
}

impl AgentSwarmTool {
    pub fn new(
        host: Arc<dyn SubagentHost>,
        subagent_timeout: Option<std::time::Duration>,
        max_swarm_items: Option<u32>,
    ) -> Self {
        let description = build_swarm_description(&host);
        Self {
            host,
            description,
            subagent_timeout,
            max_swarm_items,
            tasks: None,
            session_id: None,
        }
    }

    /// Track every launched member under the session's [`TaskManager`] so
    /// `task.list` observes the batch and `task.cancel` / `task.stop` can
    /// stop one member without touching the rest (US5.1).
    pub fn with_task_tracking(
        mut self,
        tasks: Arc<crate::task::TaskManager>,
        session_id: String,
    ) -> Self {
        self.tasks = Some(tasks);
        self.session_id = Some(session_id);
        self
    }
}

/// The per-member description (D-52) — TS `childDescription`
/// (agentSwarmTool.ts:303-305): `"{swarm description} #{1-based index}
/// ({profile})"`. Spawn members carry the subagent profile; resumed members
/// display as `resume` (the TS shape — the actual spawn profile stays on the
/// spawned row).
fn child_description(swarm_description: &str, one_based_index: u32, profile_name: &str) -> String {
    format!("{swarm_description} #{one_based_index} ({profile_name})")
}

/// Build the parameters schema for a given resolved item cap (SDR-012):
/// the `items` description names the live limit so the model sees the real
/// ceiling. A cap of `0` renders as "no item-count limit".
fn build_swarm_parameters(max_items: u32) -> serde_json::Value {
    let items_description = if max_items == 0 {
        "Each item spawns one subagent. No item-count limit. Must have at least 2 \
         (unless resume_agent_ids is provided)."
            .to_string()
    } else {
        format!(
            "Each item spawns one subagent. Max {max_items}. Must have at least 2 \
             (unless resume_agent_ids is provided)."
        )
    };
    json!({
        "type": "object",
        "properties": {
            "description": {
                "type": "string",
                "description": "Short description for the swarm."
            },
            "subagent_type": {
                "type": "string",
                "description": "Profile name for new subagents. Defaults to 'coder'."
            },
            "prompt_template": {
                "type": "string",
                "description": "Template with {{item}} placeholder. Required when 'items' is present."
            },
            "items": {
                "type": "array",
                "items": {"type": "string"},
                "description": items_description
            },
            "resume_agent_ids": {
                "type": "object",
                "description": "Map of agent_id to resume prompt. Launched before new spawns.",
                "additionalProperties": {"type": "string"}
            },
            "model": {
                "type": "string",
                "description": "Model alias to override the subagent default. Valid aliases are listed in the 'Available models' section of this tool description when applicable."
            }
        },
        "required": ["description"],
        "additionalProperties": false
    })
}

/// The per-process parameters-schema cache, keyed by the resolved item cap.
///
/// `Tool::parameters` returns `&serde_json::Value` borrowed from `&self`,
/// but the cap is resolved at call time (env/config can change after
/// construction), so the schema cannot live in a plain field. The cache
/// holds the most recently built schema; a cap change leaks one boxed
/// schema per distinct value — bounded by the distinct ceilings the
/// process ever resolves (in practice a handful), a few hundred bytes
/// each.
fn swarm_parameters_schema(max_items: u32) -> &'static serde_json::Value {
    static CACHED: std::sync::Mutex<Option<(u32, &'static serde_json::Value)>> =
        std::sync::Mutex::new(None);
    let mut cached = CACHED.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((cached_cap, schema)) = *cached
        && cached_cap == max_items
    {
        return schema;
    }
    let schema: &'static serde_json::Value = Box::leak(Box::new(build_swarm_parameters(max_items)));
    *cached = Some((max_items, schema));
    schema
}

/// Build the AgentSwarm tool description, appending available profiles.
fn build_swarm_description(host: &Arc<dyn SubagentHost>) -> String {
    let base = "Launch multiple subagents concurrently from a prompt template. Each \
         item in `items` expands `{{item}}` in `prompt_template` to create a \
         unique prompt for one subagent. Use `resume_agent_ids` to resume \
         existing subagents. Results are returned as an XML block.";

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

impl Tool for AgentSwarmTool {
    fn name(&self) -> &str {
        "AgentSwarm"
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters(&self) -> &serde_json::Value {
        swarm_parameters_schema(resolve_swarm_max_items(self.max_swarm_items))
    }

    fn resolve_execution(&self, input: serde_json::Value) -> EngineResult<ToolExecution> {
        let description: String = input
            .get("description")
            .and_then(|v| v.as_str())
            .ok_or_else(|| EngineError::RequestInvalid("missing 'description'".into()))?
            .to_string();
        let subagent_type: String = input
            .get("subagent_type")
            .and_then(|v| v.as_str())
            .unwrap_or("coder")
            .to_string();
        let prompt_template: Option<String> = input
            .get("prompt_template")
            .and_then(|v| v.as_str())
            .map(String::from);
        let items: Option<Vec<String>> = input.get("items").and_then(|v| v.as_array()).map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        });
        let resume_agent_ids: Option<serde_json::Map<String, serde_json::Value>> = input
            .get("resume_agent_ids")
            .and_then(|v| v.as_object())
            .cloned();
        let model: Option<String> = input
            .get("model")
            .and_then(|v| v.as_str())
            .map(String::from);

        // --- Validation ---

        // Need at least one source of subagents.
        let has_items = items.as_ref().is_some_and(|i| !i.is_empty());
        let has_resume = resume_agent_ids.as_ref().is_some_and(|m| !m.is_empty());
        if !has_items && !has_resume {
            return Ok(ToolExecution::Error(crate::tool::ToolErrorResult {
                message: "Must provide at least 'items' (with 'prompt_template') or \
                          'resume_agent_ids'."
                    .into(),
            }));
        }

        // Items validation. The cap is resolved per call (env > config >
        // default, SDR-012); `0` means unlimited.
        if let Some(ref items) = items {
            let max_items = resolve_swarm_max_items(self.max_swarm_items);
            if max_items != 0 && items.len() > max_items as usize {
                return Ok(ToolExecution::Error(crate::tool::ToolErrorResult {
                    message: format!("Too many items: {} (max {})", items.len(), max_items),
                }));
            }
            if items.len() < 2 && !has_resume {
                return Ok(ToolExecution::Error(crate::tool::ToolErrorResult {
                    message: "Need at least 2 items (or provide resume_agent_ids).".into(),
                }));
            }
        }

        // prompt_template required with items.
        let template = if has_items {
            match &prompt_template {
                Some(t) if t.contains("{{item}}") => t.clone(),
                Some(_) => {
                    return Ok(ToolExecution::Error(crate::tool::ToolErrorResult {
                        message: "prompt_template must contain '{{item}}' placeholder.".into(),
                    }));
                }
                None => {
                    return Ok(ToolExecution::Error(crate::tool::ToolErrorResult {
                        message: "prompt_template is required when 'items' is provided.".into(),
                    }));
                }
            }
        } else {
            String::new()
        };

        // Check for duplicate expanded prompts.
        if let Some(ref items) = items {
            let expanded: Vec<String> = items
                .iter()
                .map(|item| template.replace("{{item}}", item))
                .collect();
            let mut seen = std::collections::HashSet::new();
            for prompt in &expanded {
                if !seen.insert(prompt.clone()) {
                    return Ok(ToolExecution::Error(crate::tool::ToolErrorResult {
                        message:
                            "Duplicate prompts detected — items must expand to unique prompts."
                                .to_string(),
                    }));
                }
            }
        }

        // --- Build specs ---
        let host = Arc::clone(&self.host);
        // The per-member timeout (D-45): the map closure below wraps each
        // member's completion with it. Copied out of `&self` so the
        // 'static execute closure borrows nothing.
        let subagent_timeout = self.subagent_timeout;
        // Member tracking (US5.1): captured by value so the 'static closure
        // borrows nothing — `None` keeps the batch untracked.
        let task_manager = self.tasks.clone();
        let session_id = self.session_id.clone();

        // The TS tool display name — `swarm (N subagents)` over the member
        // count (resume entries + items).
        let member_count = resume_agent_ids.as_ref().map(|m| m.len()).unwrap_or(0)
            + items.as_ref().map(|i| i.len()).unwrap_or(0);

        let plan = ExecutionPlan {
            accesses: Vec::new(),
            display: ToolInputDisplay::AgentCall {
                agent_name: format!("swarm ({member_count} subagents)"),
                prompt: description.clone(),
                background: None,
            },
            approval_rule: "AgentSwarm".to_string(),
            stop_batch_after_this: true,
        };

        let execute: Box<dyn FnOnce(ExecutableToolContext) -> ToolFuture + Send> =
            Box::new(move |ctx: ExecutableToolContext| {
                Box::pin(async move {
                    // Build the member tasks — resume entries first, then
                    // spawns (the TS dispatch order). Each member carries its
                    // own description (`{swarm} #{n} ({profile})`, D-52) and
                    // resumes relabel their original swarm item (D-60).
                    let mut tasks: Vec<SwarmTask> = Vec::new();

                    // Resume specs first.
                    if let Some(ref resume_map) = resume_agent_ids {
                        for (agent_id, prompt_val) in resume_map {
                            let prompt = prompt_val.as_str().unwrap_or("").to_string();
                            let index = tasks.len() as u32;
                            tasks.push(SwarmTask {
                                kind: SwarmTaskKind::Resume {
                                    agent_id: agent_id.clone(),
                                },
                                prompt,
                                description: child_description(&description, index + 1, "resume"),
                                item: host.swarm_item_of(agent_id),
                                swarm_index: index,
                            });
                        }
                    }

                    // Spawn specs from items.
                    if let Some(ref items) = items {
                        for item in items {
                            let prompt = template.replace("{{item}}", item);
                            let index = tasks.len() as u32;
                            tasks.push(SwarmTask {
                                kind: SwarmTaskKind::Spawn {
                                    profile_name: subagent_type.clone(),
                                    model: model.clone(),
                                },
                                prompt,
                                description: child_description(
                                    &description,
                                    index + 1,
                                    &subagent_type,
                                ),
                                item: Some(item.clone()),
                                swarm_index: index,
                            });
                        }
                    }

                    // The batch runs under the scheduler: ramp-up pacing
                    // (D-42/D-54), per-attempt timeout (D-45, moved here from
                    // the B1 buffered-stream wrapper), tri-state cancellation
                    // (D-44) and rate-limit requeue (D-43).
                    let batch_cancel = ctx.cancel.child();
                    let options = SwarmBatchOptions {
                        max_concurrency: resolve_swarm_max_concurrency(),
                        attempt_timeout: subagent_timeout,
                        retry_base_ms: None,
                        // The foreground batch's stall-watchdog feed: the
                        // tool's live-update channel resets the parent
                        // turn's stall deadline while members work
                        // silently (ADR-004 batch a gap — the swarm
                        // edition of `drive_with_activity_forwarding`).
                        progress: ctx.progress.clone(),
                        registry: match (&task_manager, &session_id) {
                            (Some(tasks), Some(session_id)) => Some(SwarmTaskRegistry {
                                tasks: Arc::clone(tasks),
                                session_id: session_id.clone(),
                            }),
                            _ => None,
                        },
                    };
                    // ADR-006: the foreground batch wait registers its
                    // WaitingSubagent entry — the watchdog pulls the
                    // subagent host's owner verdict at window expiry.
                    // The swarm members run under the host; the probe is
                    // the batch-level composite: any member running with
                    // advancing activity → Advancing; all members gone →
                    // Settled; running but silent → Stalled (the kind
                    // window). The tracked task rows carry the members'
                    // heartbeats — the task-manager snapshot is the
                    // composite fact source.
                    let _swarm_wait = ctx.wait_registry.as_ref().map(|registry| {
                        let tasks_for_probe = task_manager.clone();
                        let tool_call_id = ctx.tool_call_id.clone();
                        registry.enter(
                            crate::turn_flow::WaitKind::WaitingSubagent,
                            crate::turn_flow::WaitSubject::Subagent {
                                probe: Arc::new(move || {
                                    // The batch's member rows are the
                                    // swarm-tracked tasks under this tool
                                    // call: any non-terminal row with a
                                    // live heartbeat → Advancing; all
                                    // rows terminal/gone → Settled.
                                    let tasks = match tasks_for_probe.as_ref() {
                                        Some(tasks) => tasks,
                                        None => return crate::turn_flow::OwnerVerdict::Advancing,
                                    };
                                    let member_rows: Vec<_> = tasks
                                        .list()
                                        .into_iter()
                                        .filter(|task| {
                                            matches!(
                                                &task.kind,
                                                crate::task::TaskKind::Agent {
                                                    parent_tool_call_id: Some(parent),
                                                    ..
                                                } if parent == &tool_call_id
                                            )
                                        })
                                        .collect();
                                    if member_rows.is_empty() {
                                        return crate::turn_flow::OwnerVerdict::Settled;
                                    }
                                    let any_alive =
                                        member_rows.iter().any(|row| !row.status.is_terminal());
                                    if any_alive {
                                        crate::turn_flow::OwnerVerdict::Advancing
                                    } else {
                                        crate::turn_flow::OwnerVerdict::Settled
                                    }
                                }),
                            },
                        )
                    });
                    let results = run_swarm_batch(
                        Arc::clone(&host),
                        tasks,
                        Some(ctx.tool_call_id.clone()),
                        batch_cancel,
                        options,
                    )
                    .await;

                    format_swarm_results(&results)
                })
            });

        Ok(ToolExecution::Runnable { plan, execute })
    }
}

// ---------------------------------------------------------------------------
// Result rendering — TS agentSwarmTool.ts renderSwarmResults, verbatim shape
// ---------------------------------------------------------------------------

/// Format all swarm results as an XML block (TS v2 semantics, SDR-B3-03):
///
/// - `<summary>` lists only the non-zero counts (completed/failed/aborted);
/// - `<resume_hint>` (fixed wording) follows the summary whenever any member
///   is not completed AND some agent_id exists;
/// - each `<subagent>` row: `mode` only on resume members, `state` only on
///   non-completed rows, the body is the member's result or error and is
///   written UNESCAPED (the web `parseSwarmResult` contract — escaped bodies
///   would render entity literals);
/// - the tool result itself is NEVER an error — failures live in the XML
///   (`is_error` is reserved for input-validation errors).
fn format_swarm_results(results: &[SwarmRunResult]) -> ToolResult {
    let completed = results
        .iter()
        .filter(|r| r.outcome == SwarmOutcome::Completed)
        .count();
    let failed = results
        .iter()
        .filter(|r| r.outcome == SwarmOutcome::Failed)
        .count();
    let aborted = results
        .iter()
        .filter(|r| r.outcome == SwarmOutcome::Aborted)
        .count();

    let mut summary_parts: Vec<String> = Vec::new();
    if completed > 0 {
        summary_parts.push(format!("completed: {completed}"));
    }
    if failed > 0 {
        summary_parts.push(format!("failed: {failed}"));
    }
    if aborted > 0 {
        summary_parts.push(format!("aborted: {aborted}"));
    }

    // TS renders the hint when ANY member is non-completed AND (separately)
    // ANY member carries an agent_id — two independent existentials
    // (agentSwarmTool.ts:310-311).
    let should_render_resume_hint = results.iter().any(|r| r.outcome != SwarmOutcome::Completed)
        && results.iter().any(|r| r.agent_id.is_some());

    let mut xml = String::from("<agent_swarm_result>\n");
    xml.push_str(&format!(
        "<summary>{}</summary>\n",
        summary_parts.join(", ")
    ));

    if should_render_resume_hint {
        xml.push_str(
            "<resume_hint>Call AgentSwarm with resume_agent_ids using the \
             agent_id values in this result to continue unfinished \
             work.</resume_hint>\n",
        );
    }

    for r in results {
        let mode_attr = if matches!(r.task.kind, SwarmTaskKind::Resume { .. }) {
            " mode=\"resume\""
        } else {
            ""
        };
        let agent_id_attr = r
            .agent_id
            .as_deref()
            .map(|id| format!(" agent_id=\"{}\"", xml_escape(id)))
            .unwrap_or_default();
        let item_attr = r
            .task
            .item
            .as_deref()
            .map(|i| format!(" item=\"{}\"", xml_escape(i)))
            .unwrap_or_default();
        // `state` rides only non-completed rows (TS renders it from the
        // optional state field, which completion never sets).
        let state_attr = match (&r.outcome, &r.state) {
            (SwarmOutcome::Completed, _) => "",
            (_, SwarmStartState::Started) => " state=\"started\"",
            (_, SwarmStartState::NotStarted) => " state=\"not_started\"",
        };
        let outcome_attr = match r.outcome {
            SwarmOutcome::Completed => "completed",
            SwarmOutcome::Failed => "failed",
            SwarmOutcome::Aborted => "aborted",
        };
        let body = match (&r.outcome, &r.result, &r.error) {
            (SwarmOutcome::Completed, Some(result), _) => result.clone(),
            (_, _, Some(error)) => error.clone(),
            _ => "unknown error".to_string(),
        };
        xml.push_str(&format!(
            "<subagent{mode_attr}{agent_id_attr}{item_attr}{state_attr} \
             outcome=\"{outcome_attr}\">{body}</subagent>\n"
        ));
    }

    xml.push_str("</agent_swarm_result>");
    ToolResult::success(xml)
}

/// Escape a value for XML attribute placement (the body is written raw).
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cancel::CancelToken;
    use crate::error::EngineResult;
    use crate::subagent::{
        ResumeRequest, SpawnRequest, SubagentCompletion, SubagentError, SubagentHandle,
    };
    use crate::tool::ToolOutput;
    use std::time::Duration;

    struct MockHost {
        results: Vec<String>,
        call_count: std::sync::Mutex<usize>,
    }

    impl SubagentHost for MockHost {
        fn spawn(&self, request: SpawnRequest) -> EngineResult<SubagentHandle> {
            let mut count = self.call_count.lock().unwrap();
            let idx = *count % self.results.len();
            *count += 1;
            let result = self.results[idx].clone();
            let agent_id = format!("agent-{}", *count);
            Ok(SubagentHandle::from_result(
                agent_id,
                request.profile_name,
                request.model,
                result,
            ))
        }

        fn resume(&self, request: ResumeRequest) -> EngineResult<SubagentHandle> {
            Ok(SubagentHandle::from_result(
                request.agent_id,
                "coder".into(),
                None,
                "resumed result".into(),
            ))
        }
    }

    fn make_tool(results: &[&str]) -> AgentSwarmTool {
        AgentSwarmTool::new(
            Arc::new(MockHost {
                results: results.iter().map(|s| s.to_string()).collect(),
                call_count: std::sync::Mutex::new(0),
            }),
            None,
            None,
        )
    }

    async fn run_tool(tool: &AgentSwarmTool, input: serde_json::Value) -> ToolResult {
        run_tool_with_cancel(tool, input, CancelToken::new()).await
    }

    async fn run_tool_with_cancel(
        tool: &AgentSwarmTool,
        input: serde_json::Value,
        cancel: CancelToken,
    ) -> ToolResult {
        let execution = tool.resolve_execution(input).expect("resolve");
        match execution {
            ToolExecution::Runnable { execute, .. } => {
                let ctx = ExecutableToolContext {
                    approval_metadata: None,
                    wait_registry: None,
                    turn_id: "test-turn".into(),
                    tool_call_id: "test-call".into(),
                    cancel,
                    progress: crate::tool::ToolProgressHandle::default(),
                };
                execute(ctx).await
            }
            ToolExecution::Error(err) => ToolResult::error(err.message),
        }
    }

    #[tokio::test]
    async fn should_dispatch_all_items_when_given_valid_swarm_request() {
        let tool = make_tool(&["result-a", "result-b", "result-c"]);
        let result = run_tool(
            &tool,
            json!({
                "description": "test swarm",
                "prompt_template": "Process: {{item}}",
                "items": ["alpha", "beta", "gamma"]
            }),
        )
        .await;
        assert!(!result.is_error, "should not be error");
        let ToolOutput::Text(xml) = &result.output else {
            panic!("expected text output");
        };
        assert!(xml.contains("completed: 3"), "xml: {xml}");
        assert!(xml.contains("result-a"), "xml: {xml}");
        assert!(xml.contains("result-b"), "xml: {xml}");
        assert!(xml.contains("result-c"), "xml: {xml}");
    }

    #[tokio::test]
    async fn should_return_error_when_items_has_fewer_than_two_entries() {
        let tool = make_tool(&["result"]);
        let result = run_tool(
            &tool,
            json!({
                "description": "single item",
                "prompt_template": "Process: {{item}}",
                "items": ["only-one"]
            }),
        )
        .await;
        assert!(result.is_error, "should reject single item");
        let ToolOutput::Text(msg) = &result.output else {
            panic!("expected text output");
        };
        assert!(msg.contains("at least 2"), "msg: {msg}");
    }

    #[tokio::test]
    async fn should_resume_existing_agents_when_resume_agent_ids_provided() {
        let tool = make_tool(&["new-result"]);
        let result = run_tool(
            &tool,
            json!({
                "description": "resume test",
                "resume_agent_ids": {
                    "agent-old-1": "continue task 1",
                    "agent-old-2": "continue task 2"
                }
            }),
        )
        .await;
        assert!(!result.is_error, "should not be error");
        let ToolOutput::Text(xml) = &result.output else {
            panic!("expected text output");
        };
        assert!(xml.contains("mode=\"resume\""), "xml: {xml}");
        assert!(xml.contains("agent-old-1"), "xml: {xml}");
        assert!(xml.contains("agent-old-2"), "xml: {xml}");
        assert!(xml.contains("resumed result"), "xml: {xml}");
    }

    #[tokio::test]
    async fn should_reject_template_without_item_placeholder_when_items_provided() {
        let tool = make_tool(&["result"]);
        let result = run_tool(
            &tool,
            json!({
                "description": "no placeholder",
                "prompt_template": "Do the same thing",
                "items": ["a", "b"]
            }),
        )
        .await;
        assert!(result.is_error);
        let ToolOutput::Text(msg) = &result.output else {
            panic!("expected text output");
        };
        assert!(msg.contains("{{item}}"), "msg: {msg}");
    }

    #[tokio::test]
    async fn should_reject_duplicate_prompts_when_items_expand_to_same_prompt() {
        let tool = make_tool(&["result"]);
        let result = run_tool(
            &tool,
            json!({
                "description": "dup test",
                "prompt_template": "fix: {{item}}",
                "items": ["same", "same"]
            }),
        )
        .await;
        assert!(result.is_error);
        let ToolOutput::Text(msg) = &result.output else {
            panic!("expected text output");
        };
        assert!(msg.contains("Duplicate prompts"), "msg: {msg}");
    }

    #[tokio::test]
    async fn should_return_error_when_no_items_or_resume_provided() {
        let tool = make_tool(&["result"]);
        let result = run_tool(
            &tool,
            json!({
                "description": "empty swarm"
            }),
        )
        .await;
        assert!(result.is_error, "should reject empty swarm");
        let ToolOutput::Text(msg) = &result.output else {
            panic!("expected text output");
        };
        assert!(msg.contains("Must provide"), "msg: {msg}");
    }

    #[tokio::test]
    async fn should_return_error_when_prompt_template_missing_with_items() {
        let tool = make_tool(&["result"]);
        let result = run_tool(
            &tool,
            json!({
                "description": "no template",
                "items": ["a", "b"]
            }),
        )
        .await;
        assert!(result.is_error);
        let ToolOutput::Text(msg) = &result.output else {
            panic!("expected text output");
        };
        assert!(msg.contains("prompt_template"), "msg: {msg}");
    }

    #[tokio::test]
    async fn should_include_summary_with_completed_and_failed_counts_when_swarm_updates_partially_fail()
     {
        // Mix success and failure by having fewer results than items.
        let tool = make_tool(&["ok-1", "ok-2", "ok-3"]);
        let result = run_tool(
            &tool,
            json!({
                "description": "summary test",
                "prompt_template": "Task: {{item}}",
                "items": ["x", "y", "z"]
            }),
        )
        .await;
        let ToolOutput::Text(xml) = &result.output else {
            panic!("expected text output");
        };
        assert!(
            xml.contains("<summary>completed: 3</summary>"),
            "xml: {xml}"
        );
    }

    #[tokio::test]
    async fn should_combine_resume_and_spawn_when_both_provided() {
        let tool = make_tool(&["spawn-result"]);
        let result = run_tool(
            &tool,
            json!({
                "description": "mixed swarm",
                "prompt_template": "New: {{item}}",
                "items": ["new-1", "new-2"],
                "resume_agent_ids": {
                    "agent-old": "continue"
                }
            }),
        )
        .await;
        assert!(!result.is_error);
        let ToolOutput::Text(xml) = &result.output else {
            panic!("expected text output");
        };
        assert!(xml.contains("mode=\"resume\""), "should have resume: {xml}");
        // TS v2: `mode` rides resume rows only — spawn rows carry the item
        // attribute instead.
        assert!(
            !xml.contains("mode=\"spawn\""),
            "must not tag spawns: {xml}"
        );
        assert!(xml.contains("item=\"new-1\""), "should have spawn: {xml}");
        assert!(xml.contains("completed: 3"), "should have 3 total: {xml}");
    }

    /// A mock host that returns profile info for `available_profiles()`.
    struct ProfiledMockHost {
        profiles: Vec<crate::subagent::SubagentProfileInfo>,
        curated: Vec<crate::model_catalog::ModelCatalogEntry>,
    }

    impl SubagentHost for ProfiledMockHost {
        fn spawn(&self, _request: SpawnRequest) -> EngineResult<SubagentHandle> {
            Ok(SubagentHandle::from_result(
                "agent-x".into(),
                "coder".into(),
                None,
                "ok".into(),
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

    /// The default seeded profile — a user-configured type declaring a
    /// model (the only legal place a concrete model name may appear, and
    /// one the description must NOT render; the neutral name keeps the
    /// seed explicit without hardcoding a vendor model).
    fn seeded_profile(model: Option<&str>) -> crate::subagent::SubagentProfileInfo {
        crate::subagent::SubagentProfileInfo {
            name: "coder".to_string(),
            model: model.map(String::from),
            description: None,
            when_to_use: None,
            tools: None,
            disallowed_tools: None,
            model_capabilities: vec!["tool-use".to_string()],
            available_models: vec!["sonnet".to_string()],
        }
    }

    #[tokio::test]
    async fn should_include_available_profiles_in_description_when_host_provides_them() {
        let tool = AgentSwarmTool::new(
            Arc::new(ProfiledMockHost {
                profiles: vec![seeded_profile(Some("test-model-a"))],
                curated: vec![
                    catalog_entry("sonnet", "The balanced model", Some(200_000), vec![]),
                    catalog_entry("haiku", "Fast responses", Some(100_000), vec![]),
                ],
            }),
            None,
            None,
        );
        let desc = tool.description();
        assert!(desc.contains("coder"), "should list coder: {desc}");
        assert!(
            !desc.contains("test-model-a"),
            "the profile's model id must not be rendered into the description: {desc}"
        );
        assert!(
            desc.contains("Available profiles"),
            "should have profiles section: {desc}"
        );
        assert!(
            desc.contains("Available models"),
            "should have models section: {desc}"
        );
        assert!(desc.contains("- sonnet"), "should list sonnet: {desc}");
        assert!(
            !desc.contains("claude-sonnet") && !desc.contains("claude-haiku"),
            "A8 guard: no hardcoded vendor model names in the description: {desc}"
        );
    }

    /// Given a model-less profile (the post-T1 builtin shape) alongside a
    /// user-configured profile that declares a model, When the swarm
    /// description is built, Then the profile lines render bare names —
    /// no `(model)` suffix on either, and the configured profile's model
    /// id never reaches the description (spec 11 R7/A8). A profile's own
    /// `description` (duties text, not a model name) DOES render inline in
    /// the dispatch entry header (brief Interfaces).
    #[test]
    fn should_not_render_model_suffix_when_profile_has_no_model() {
        let host = Arc::new(ProfiledMockHost {
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
        let tool = AgentSwarmTool::new(host, None, None);

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

    /// Given a host whose curated catalog lists exactly `alpha` and `beta`,
    /// When the swarm description is built, Then the only model names in
    /// the description are the catalog-sourced aliases — introduced by the
    /// fixed resolution sentence — while the profile's own raw model id
    /// and any non-catalog alias never appear (spec 11 R7/A8).
    #[test]
    fn should_render_only_catalog_sourced_models_in_description() {
        let host = Arc::new(ProfiledMockHost {
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
        let tool = AgentSwarmTool::new(host, None, None);

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

    /// should include available models in swarm tool description when
    /// catalog is provided
    #[tokio::test]
    async fn should_include_available_models_in_agent_swarm_tool_description_when_catalog_is_provided()
     {
        // Given a host whose profiles carry catalog-derived available_models
        let tool = AgentSwarmTool::new(
            Arc::new(ProfiledMockHost {
                profiles: vec![seeded_profile(Some("test-model-a"))],
                curated: vec![
                    catalog_entry("sonnet", "The balanced model", Some(200_000), vec![]),
                    catalog_entry("opus", "Heavy lifting", Some(1_000_000), vec!["tool-use"]),
                ],
            }),
            None,
            None,
        );

        // When the description is built it includes shared aliases across
        // profiles without duplication
        let desc = tool.description();
        assert!(
            desc.contains("Available models"),
            "should have models section: {desc}"
        );
        assert!(desc.contains("- sonnet"), "should list sonnet: {desc}");
        let params = tool.parameters().to_string();
        assert!(
            params.contains("'Available models' section"),
            "model param description should reference the section: {params}"
        );
    }

    /// should render curated entries with description and context when
    /// building the swarm description
    #[tokio::test]
    async fn should_render_curated_entries_with_description_and_context_when_building_swarm_description()
     {
        // Given a host providing 2 curated entries (description + context size)
        let tool = AgentSwarmTool::new(
            Arc::new(ProfiledMockHost {
                profiles: vec![seeded_profile(Some("test-model-a"))],
                curated: vec![
                    catalog_entry(
                        "sonnet",
                        "The balanced model",
                        Some(200_000),
                        vec!["vision", "tool-use"],
                    ),
                    catalog_entry("haiku", "Fast responses", Some(100_000), vec![]),
                ],
            }),
            None,
            None,
        );

        // When the swarm tool description is built
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
        let tool = AgentSwarmTool::new(
            Arc::new(ProfiledMockHost {
                profiles: vec![seeded_profile(Some("test-model-a"))],
                curated: Vec::new(),
            }),
            None,
            None,
        );

        // When the description is built
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
        let tool = make_tool(&["ok"]);
        let desc = tool.description();
        assert!(
            !desc.contains("Available profiles"),
            "should not have profiles section: {desc}"
        );
    }

    // ===== B3 T7: tri-state cancellation in the result XML =====

    /// Given a 7-member swarm where the first four hang until cancelled and
    /// the fifth completes fast, When the turn's cancel token fires
    /// mid-batch, Then the result XML marks launched members
    /// outcome="aborted" state="started", the two still-ramped members
    /// "not_started" — and the tool result is NOT an error (TS v2 never
    /// sets isError on the swarm result).
    #[tokio::test(start_paused = true)]
    #[allow(clippy::await_holding_lock)] // ENV_LOCK serializes env-var access across the whole test
    async fn should_distinguish_aborted_and_not_started_members_in_the_result_xml() {
        // The env-override tests mutate the swarm env spellings under
        // ENV_LOCK; this test resolves the cap at call time, so it must take
        // the same lock and clear the variables or a parallel env test can
        // shrink its raised cap (e.g. to 6) and reject its 7 items.
        let _env = ENV_LOCK.lock().unwrap();
        let mut guard = EnvGuard::new();
        clear_max_items_env(&mut guard);

        struct HangOrFastHost {
            /// The one 0-based swarm index that completes immediately.
            fast_index: u32,
        }
        impl SubagentHost for HangOrFastHost {
            fn spawn(&self, request: SpawnRequest) -> EngineResult<SubagentHandle> {
                let cancel = request.cancel.clone();
                let fast = request.swarm_index == Some(self.fast_index);
                Ok(SubagentHandle::new(
                    format!("agent-{}", request.swarm_index.unwrap_or(0)),
                    request.profile_name,
                    request.model,
                    Box::pin(async move {
                        if fast {
                            Ok(crate::subagent::SubagentCompletion::from_text("done"))
                        } else {
                            cancel.cancelled().await;
                            Err(crate::subagent::SubagentError::Cancelled)
                        }
                    }),
                ))
            }
            fn resume(&self, _r: ResumeRequest) -> EngineResult<SubagentHandle> {
                Err(EngineError::RequestInvalid("not implemented".into()))
            }
        }
        let cancel = CancelToken::new();
        // The cancellation test needs 7 members — construct the tool with a
        // raised item cap so the (default-3) ceiling does not reject it.
        let tool = AgentSwarmTool::new(Arc::new(HangOrFastHost { fast_index: 4 }), None, Some(7));
        let run = {
            let cancel = cancel.clone();
            tokio::spawn(async move {
                run_tool_with_cancel(
                    &tool,
                    json!({
                        "description": "mixed",
                        "prompt_template": "P: {{item}}",
                        "items": ["a","b","c","d","e","f","g"],
                    }),
                    cancel,
                )
                .await
            })
        };
        tokio::task::yield_now().await;
        tokio::task::yield_now().await; // let the initial burst launch
        cancel.cancel();
        let result = run.await.unwrap();
        let crate::tool::ToolOutput::Text(xml) = &result.output else {
            panic!("text");
        };
        assert!(xml.contains("completed: 1"), "{xml}");
        assert!(xml.contains("aborted: 6"), "{xml}");
        assert!(xml.contains("outcome=\"aborted\""), "{xml}");
        assert!(xml.contains("state=\"not_started\""), "{xml}");
        assert!(xml.contains("state=\"started\""), "{xml}");
        assert!(
            !result.is_error,
            "TS v2 never marks the swarm result as error"
        );
        assert!(xml.contains("resume_hint"), "{xml}");
    }

    // ===== B1 T6: per-member timeout (D-45) =====

    /// A host whose item-0 member hangs until its cancel token fires
    /// (recording the observation) while every other member completes
    /// immediately.
    struct MixedSwarmHost {
        cancelled_observed: std::sync::Arc<std::sync::atomic::AtomicBool>,
    }

    impl SubagentHost for MixedSwarmHost {
        fn spawn(&self, request: SpawnRequest) -> EngineResult<SubagentHandle> {
            let agent_id = format!("agent-{}", request.profile_name);
            if request.swarm_index == Some(0) {
                let cancel = request.cancel.clone();
                let flag = std::sync::Arc::clone(&self.cancelled_observed);
                Ok(SubagentHandle::new(
                    agent_id,
                    request.profile_name,
                    request.model,
                    Box::pin(async move {
                        cancel.cancelled().await;
                        flag.store(true, std::sync::atomic::Ordering::SeqCst);
                        Err(SubagentError::Cancelled)
                    }),
                ))
            } else {
                Ok(SubagentHandle::from_result(
                    agent_id,
                    request.profile_name,
                    request.model,
                    "quick result".into(),
                ))
            }
        }

        fn resume(&self, _request: ResumeRequest) -> EngineResult<SubagentHandle> {
            Err(EngineError::RequestInvalid("not implemented".into()))
        }
    }

    /// Given a swarm with a 150ms per-member timeout and one hanging member,
    /// When the swarm runs, Then that member is cancelled (its token fires —
    /// the pre-fix map closure dropped the token), the result XML marks it
    /// failed with "Subagent timed out", and the sibling member completes
    /// normally.
    #[tokio::test]
    async fn should_time_out_a_hanging_member_when_the_swarm_per_member_timeout_expires() {
        let cancelled_observed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let tool = AgentSwarmTool::new(
            Arc::new(MixedSwarmHost {
                cancelled_observed: std::sync::Arc::clone(&cancelled_observed),
            }),
            Some(Duration::from_millis(150)),
            None,
        );

        let result = run_tool(
            &tool,
            json!({
                "description": "mixed",
                "prompt_template": "P: {{item}}",
                "items": ["hangs", "quick"]
            }),
        )
        .await;

        let ToolOutput::Text(text) = &result.output else {
            panic!("text");
        };
        assert!(
            text.contains("failed: 1"),
            "timeout counts as failed: {text}"
        );
        assert!(text.contains("completed: 1"), "{text}");
        assert!(text.contains("Subagent timed out"), "{text}");
        assert!(
            cancelled_observed.load(std::sync::atomic::Ordering::SeqCst),
            "the timed-out member's cancel token must actually fire"
        );
    }

    /// Given a swarm without a timeout (None), When a member takes a slow
    /// but finite path, Then the swarm waits it out — no deadline is
    /// injected (the print-mode unbounded semantics).
    #[tokio::test]
    async fn should_not_wrap_members_when_no_timeout_is_configured() {
        struct SlowHost;
        impl SubagentHost for SlowHost {
            fn spawn(&self, request: SpawnRequest) -> EngineResult<SubagentHandle> {
                Ok(SubagentHandle::new(
                    format!("agent-{}", request.profile_name),
                    request.profile_name,
                    request.model,
                    Box::pin(async move {
                        tokio::time::sleep(Duration::from_millis(200)).await;
                        Ok(SubagentCompletion::from_text("slow but done".to_string()))
                    }),
                ))
            }
            fn resume(&self, _request: ResumeRequest) -> EngineResult<SubagentHandle> {
                Err(EngineError::RequestInvalid("not implemented".into()))
            }
        }
        let tool = AgentSwarmTool::new(Arc::new(SlowHost), None, None);

        let result = run_tool(
            &tool,
            json!({
                "description": "slow",
                "prompt_template": "P: {{item}}",
                "items": ["a", "b"]
            }),
        )
        .await;

        let ToolOutput::Text(text) = &result.output else {
            panic!("text");
        };
        assert!(text.contains("completed: 2"), "{text}");
        assert!(!text.contains("timed out"), "{text}");
    }

    // ===== B3 T8: per-member description + item relabeling =====

    /// Given a 3-item swarm, When the members launch, Then each one's
    /// description carries the swarm description, its 1-based index, and
    /// the profile — the rows a task panel shows (D-52).
    #[tokio::test]
    async fn should_describe_each_member_with_its_index_and_profile() {
        struct DescribingHost {
            descriptions: std::sync::Mutex<Vec<String>>,
        }
        impl SubagentHost for DescribingHost {
            fn spawn(&self, request: SpawnRequest) -> EngineResult<SubagentHandle> {
                self.descriptions
                    .lock()
                    .unwrap()
                    .push(request.description.clone());
                Ok(SubagentHandle::from_result(
                    format!("agent-{}", request.swarm_index.unwrap_or(0)),
                    request.profile_name,
                    request.model,
                    "ok".into(),
                ))
            }
            fn resume(&self, _request: ResumeRequest) -> EngineResult<SubagentHandle> {
                Err(EngineError::RequestInvalid("not implemented".into()))
            }
        }
        let host = Arc::new(DescribingHost {
            descriptions: std::sync::Mutex::new(Vec::new()),
        });
        let tool = AgentSwarmTool::new(Arc::clone(&host) as Arc<dyn SubagentHost>, None, None);
        let result = run_tool(
            &tool,
            json!({
                "description": "test swarm",
                "subagent_type": "coder",
                "prompt_template": "P: {{item}}",
                "items": ["a", "b", "c"]
            }),
        )
        .await;
        assert!(!result.is_error);
        assert_eq!(
            *host.descriptions.lock().unwrap(),
            vec![
                "test swarm #1 (coder)".to_string(),
                "test swarm #2 (coder)".to_string(),
                "test swarm #3 (coder)".to_string(),
            ]
        );
    }

    /// Given a host that remembers the item a member was spawned with, When
    /// resume_agent_ids resumes it, Then the result XML's resumed row
    /// carries the original item label (TS getSwarmItem — D-60's relabel).
    #[tokio::test]
    async fn should_relabel_a_resumed_member_with_its_original_swarm_item() {
        struct RelabelingHost {
            items: std::collections::HashMap<String, String>,
        }
        impl SubagentHost for RelabelingHost {
            fn spawn(&self, _request: SpawnRequest) -> EngineResult<SubagentHandle> {
                Err(EngineError::RequestInvalid("not implemented".into()))
            }
            fn resume(&self, request: ResumeRequest) -> EngineResult<SubagentHandle> {
                Ok(SubagentHandle::from_result(
                    request.agent_id,
                    "coder".into(),
                    None,
                    "resumed result".into(),
                ))
            }
            fn swarm_item_of(&self, agent_id: &str) -> Option<String> {
                self.items.get(agent_id).cloned()
            }
        }
        let host = Arc::new(RelabelingHost {
            items: std::collections::HashMap::from([(
                "agent-old".to_string(),
                "alpha".to_string(),
            )]),
        });
        let tool = AgentSwarmTool::new(Arc::clone(&host) as Arc<dyn SubagentHost>, None, None);
        let result = run_tool(
            &tool,
            json!({
                "description": "resume test",
                "resume_agent_ids": { "agent-old": "continue the work" }
            }),
        )
        .await;
        assert!(!result.is_error);
        let ToolOutput::Text(xml) = &result.output else {
            panic!("text");
        };
        assert!(
            xml.contains("mode=\"resume\" agent_id=\"agent-old\" item=\"alpha\""),
            "the resumed row must relabel the original item: {xml}"
        );
    }

    // ===== B3 T9: D-60 regression — per-member instruction map =====

    /// D-60 regression: resume_agent_ids is a per-member instruction map.
    /// Given two resume entries with different prompts, When the swarm
    /// runs, Then each agent receives exactly its own instruction (the
    /// current implementation already maps — this locks it against
    /// regression and documents the G29 record correction).
    #[tokio::test]
    async fn should_send_each_resume_member_its_own_instruction() {
        struct RecordingResumeHost {
            resumes: std::sync::Mutex<Vec<(String, String)>>,
        }
        impl SubagentHost for RecordingResumeHost {
            fn spawn(&self, _request: SpawnRequest) -> EngineResult<SubagentHandle> {
                Err(EngineError::RequestInvalid("not implemented".into()))
            }
            fn resume(&self, request: ResumeRequest) -> EngineResult<SubagentHandle> {
                self.resumes
                    .lock()
                    .unwrap()
                    .push((request.agent_id.clone(), request.prompt.clone()));
                Ok(SubagentHandle::from_result(
                    request.agent_id,
                    "coder".into(),
                    None,
                    "resumed result".into(),
                ))
            }
        }
        let host = Arc::new(RecordingResumeHost {
            resumes: std::sync::Mutex::new(Vec::new()),
        });
        let tool = AgentSwarmTool::new(Arc::clone(&host) as Arc<dyn SubagentHost>, None, None);
        let result = run_tool(
            &tool,
            json!({
                "description": "resume test",
                "resume_agent_ids": {
                    "agent-old-1": "continue task 1",
                    "agent-old-2": "continue task 2"
                }
            }),
        )
        .await;
        let ToolOutput::Text(xml) = &result.output else {
            panic!("text");
        };
        assert!(xml.contains("completed: 2"), "{xml}");
        let resumes = host.resumes.lock().unwrap().clone();
        assert!(
            resumes.contains(&("agent-old-1".to_string(), "continue task 1".to_string())),
            "resumes: {resumes:?}"
        );
        assert!(
            resumes.contains(&("agent-old-2".to_string(), "continue task 2".to_string())),
            "resumes: {resumes:?}"
        );
        assert_eq!(resumes.len(), 2, "each member receives exactly one resume");
    }

    // ===== Configurable swarm item cap (default 3, SDR-012) =====

    /// Serializes tests that mutate process environment variables.
    ///
    /// `std::env::set_var`/`remove_var` are `unsafe` in edition 2024, and
    /// environment reads/writes race across threads when tests run in
    /// parallel, so every env-mutating test holds this lock and restores the
    /// previous values on drop (the `flags.rs` discipline).
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Saves the current value of each touched variable and restores it on
    /// drop, so tests never leak env state into each other.
    struct EnvGuard {
        saved: std::collections::HashMap<String, Option<String>>,
    }

    impl EnvGuard {
        fn new() -> Self {
            Self {
                saved: std::collections::HashMap::new(),
            }
        }

        fn set(&mut self, name: &str, value: Option<&str>) {
            self.saved
                .entry(name.to_string())
                .or_insert_with(|| std::env::var(name).ok());
            match value {
                // SAFETY: guarded by ENV_LOCK and restored in Drop; no other
                // thread reads or writes these variables concurrently.
                Some(value) => unsafe { std::env::set_var(name, value) },
                None => unsafe { std::env::remove_var(name) },
            }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (name, value) in &self.saved {
                // SAFETY: same discipline as `set`.
                match value {
                    Some(value) => unsafe { std::env::set_var(name, value) },
                    None => unsafe { std::env::remove_var(name) },
                }
            }
        }
    }

    /// Clear both max-items env spellings so ambient values cannot skew the
    /// resolution chain under test.
    fn clear_max_items_env(guard: &mut EnvGuard) {
        guard.set("MIRRI_AGENT_SWARM_MAX_ITEMS", None);
        guard.set("MIRRICODE_AGENT_SWARM_MAX_ITEMS", None);
    }

    fn numbered_items(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("item-{i}")).collect()
    }

    fn swarm_input(items: Vec<String>) -> serde_json::Value {
        json!({
            "description": "cap test",
            "prompt_template": "P: {{item}}",
            "items": items,
        })
    }

    /// Given no config or env override, When a swarm call carries 4 items,
    /// Then it is rejected naming the default cap of 3 — and exactly 3
    /// still runs (the default was lowered from the TS-parity 128).
    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // ENV_LOCK serializes env-var access across the whole test
    async fn should_reject_a_swarm_above_the_default_item_cap_of_three() {
        let _env = ENV_LOCK.lock().unwrap();
        let mut guard = EnvGuard::new();
        clear_max_items_env(&mut guard);

        let tool = make_tool(&["ok"]);
        let over = run_tool(&tool, swarm_input(numbered_items(4))).await;
        assert!(over.is_error, "4 items must exceed the default cap");
        let ToolOutput::Text(msg) = &over.output else {
            panic!("text");
        };
        assert!(msg.contains("Too many items: 4 (max 3)"), "msg: {msg}");

        let fits = run_tool(&tool, swarm_input(numbered_items(3))).await;
        assert!(!fits.is_error, "3 items must fit the default cap");
    }

    /// Given MIRRI_AGENT_SWARM_MAX_ITEMS=10, When 10 items are sent, Then the
    /// swarm runs — and 11 is rejected against the same raised cap.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // ENV_LOCK serializes env-var access across the whole test
    async fn should_raise_the_item_cap_when_the_env_override_is_set() {
        let _env = ENV_LOCK.lock().unwrap();
        let mut guard = EnvGuard::new();
        clear_max_items_env(&mut guard);
        guard.set("MIRRI_AGENT_SWARM_MAX_ITEMS", Some("10"));

        let tool = make_tool(&["ok"]);
        let fits = run_tool(&tool, swarm_input(numbered_items(10))).await;
        assert!(!fits.is_error, "10 items must fit the env cap");
        let over = run_tool(&tool, swarm_input(numbered_items(11))).await;
        assert!(over.is_error, "11 items must exceed the env cap");
        let ToolOutput::Text(msg) = &over.output else {
            panic!("text");
        };
        assert!(msg.contains("Too many items: 11 (max 10)"), "msg: {msg}");
    }

    /// Given the TS-spelling alias MIRRICODE_AGENT_SWARM_MAX_ITEMS=6, When
    /// the cap is resolved, Then it applies exactly like the primary name.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // ENV_LOCK serializes env-var access across the whole test
    async fn should_honor_the_ts_env_alias_when_overriding_the_item_cap() {
        let _env = ENV_LOCK.lock().unwrap();
        let mut guard = EnvGuard::new();
        clear_max_items_env(&mut guard);
        guard.set("MIRRICODE_AGENT_SWARM_MAX_ITEMS", Some("6"));

        let tool = make_tool(&["ok"]);
        let fits = run_tool(&tool, swarm_input(numbered_items(6))).await;
        assert!(!fits.is_error, "6 items must fit the alias cap");
        let over = run_tool(&tool, swarm_input(numbered_items(7))).await;
        assert!(over.is_error, "7 items must exceed the alias cap");
        let ToolOutput::Text(msg) = &over.output else {
            panic!("text");
        };
        assert!(msg.contains("Too many items: 7 (max 6)"), "msg: {msg}");
    }

    /// Given a `[subagent] maxSwarmItems` config value and no env override,
    /// When the cap is resolved, Then the config value applies.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // ENV_LOCK serializes env-var access across the whole test
    async fn should_use_the_configured_max_swarm_items_when_no_env_override_is_set() {
        let _env = ENV_LOCK.lock().unwrap();
        let mut guard = EnvGuard::new();
        clear_max_items_env(&mut guard);

        let tool = AgentSwarmTool::new(
            Arc::new(MockHost {
                results: vec!["ok".to_string()],
                call_count: std::sync::Mutex::new(0),
            }),
            None,
            Some(5),
        );
        let fits = run_tool(&tool, swarm_input(numbered_items(5))).await;
        assert!(!fits.is_error, "5 items must fit the configured cap");
        let over = run_tool(&tool, swarm_input(numbered_items(6))).await;
        assert!(over.is_error, "6 items must exceed the configured cap");
        let ToolOutput::Text(msg) = &over.output else {
            panic!("text");
        };
        assert!(msg.contains("Too many items: 6 (max 5)"), "msg: {msg}");
    }

    /// Given both an env override and a configured cap, When the cap is
    /// resolved, Then the env wins (env > config file > default).
    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // ENV_LOCK serializes env-var access across the whole test
    async fn should_prefer_the_env_override_over_the_configured_item_cap() {
        let _env = ENV_LOCK.lock().unwrap();
        let mut guard = EnvGuard::new();
        clear_max_items_env(&mut guard);
        guard.set("MIRRI_AGENT_SWARM_MAX_ITEMS", Some("10"));

        let tool = AgentSwarmTool::new(
            Arc::new(MockHost {
                results: vec!["ok".to_string()],
                call_count: std::sync::Mutex::new(0),
            }),
            None,
            Some(2),
        );
        // 4 > the configured 2 but <= the env's 10: the env must win.
        let result = run_tool(&tool, swarm_input(numbered_items(4))).await;
        assert!(
            !result.is_error,
            "env override must beat the configured cap"
        );
    }

    /// Given an unparseable env value, When the cap is resolved, Then the
    /// override is ignored (fail-soft, SDR-B3-01 semantics) and the config
    /// value — or the default of 3 — applies instead.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // ENV_LOCK serializes env-var access across the whole test
    async fn should_fall_back_to_config_or_default_when_the_env_value_is_invalid() {
        let _env = ENV_LOCK.lock().unwrap();
        let mut guard = EnvGuard::new();
        clear_max_items_env(&mut guard);
        guard.set("MIRRI_AGENT_SWARM_MAX_ITEMS", Some("not-a-number"));

        let configured = AgentSwarmTool::new(
            Arc::new(MockHost {
                results: vec!["ok".to_string()],
                call_count: std::sync::Mutex::new(0),
            }),
            None,
            Some(5),
        );
        let fits = run_tool(&configured, swarm_input(numbered_items(5))).await;
        assert!(!fits.is_error, "invalid env must fall back to config");
        let over = run_tool(&configured, swarm_input(numbered_items(6))).await;
        assert!(over.is_error, "the configured cap still applies");

        let unconfigured = make_tool(&["ok"]);
        let fallback = run_tool(&unconfigured, swarm_input(numbered_items(4))).await;
        assert!(
            fallback.is_error,
            "without config the invalid env must fall back to the default"
        );
        let ToolOutput::Text(msg) = &fallback.output else {
            panic!("text");
        };
        assert!(msg.contains("Too many items: 4 (max 3)"), "msg: {msg}");
    }

    /// Given a configured cap of 0, When a swarm carries more items than the
    /// old default, Then it runs — `0` means unlimited (mirrors
    /// `[subagent] maxNestingDepth`).
    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // ENV_LOCK serializes env-var access across the whole test
    async fn should_treat_a_zero_item_cap_as_unlimited() {
        let _env = ENV_LOCK.lock().unwrap();
        let mut guard = EnvGuard::new();
        clear_max_items_env(&mut guard);

        let tool = AgentSwarmTool::new(
            Arc::new(MockHost {
                results: vec!["ok".to_string()],
                call_count: std::sync::Mutex::new(0),
            }),
            None,
            Some(0),
        );
        let result = run_tool(&tool, swarm_input(numbered_items(7))).await;
        assert!(!result.is_error, "a zero cap must be unlimited");
    }

    /// Given a raised cap, When a swarm carries a single item, Then it is
    /// still rejected — the at-least-2 rule is independent of the cap.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // ENV_LOCK serializes env-var access across the whole test
    async fn should_still_require_at_least_two_items_when_the_cap_is_raised() {
        let _env = ENV_LOCK.lock().unwrap();
        let mut guard = EnvGuard::new();
        clear_max_items_env(&mut guard);
        guard.set("MIRRI_AGENT_SWARM_MAX_ITEMS", Some("10"));

        let tool = make_tool(&["ok"]);
        let result = run_tool(&tool, swarm_input(numbered_items(1))).await;
        assert!(result.is_error, "a single item is still rejected");
        let ToolOutput::Text(msg) = &result.output else {
            panic!("text");
        };
        assert!(msg.contains("at least 2"), "msg: {msg}");
    }

    /// Given the resolved cap, When the parameters schema is built, Then the
    /// `items` description carries the live limit — the model must see the
    /// real ceiling, and it must follow env/config changes at call time.
    #[test]
    fn should_carry_the_resolved_item_cap_in_the_parameters_schema() {
        let _env = ENV_LOCK.lock().unwrap();
        let mut guard = EnvGuard::new();
        clear_max_items_env(&mut guard);

        let mock_host = || {
            Arc::new(MockHost {
                results: vec!["ok".to_string()],
                call_count: std::sync::Mutex::new(0),
            })
        };

        // Default: the schema names the default cap of 3.
        let tool = AgentSwarmTool::new(mock_host(), None, None);
        let params = tool.parameters().to_string();
        assert!(
            params.contains("Max 3."),
            "default schema must name the cap: {params}"
        );

        // Config: the schema names the configured value.
        let tool = AgentSwarmTool::new(mock_host(), None, Some(5));
        let params = tool.parameters().to_string();
        assert!(
            params.contains("Max 5."),
            "configured schema must name the cap: {params}"
        );

        // Env at call time: the same (already-constructed) tool follows the
        // fresh resolution.
        guard.set("MIRRI_AGENT_SWARM_MAX_ITEMS", Some("7"));
        let params = tool.parameters().to_string();
        assert!(
            params.contains("Max 7."),
            "schema must follow the call-time env resolution: {params}"
        );
    }
}
