//! `ExitPlanModeTool` — exit plan mode and present the plan for approval.
//!
//! Mirrors the TypeScript `ExitPlanModeTool`: the model calls this after
//! writing a plan to signal readiness for execution. The tool reads the
//! current plan text from the [`PlanManager`], records a revision for
//! audit history, deactivates plan mode, and returns the plan formatted
//! for the model. Calling `ExitPlanMode` while plan mode is inactive, or
//! with an empty plan, is an error.
//!
//! The tool also accepts optional alternative-approach `options` (up to 3)
//! so the user can choose which approach to execute. Option labels must be
//! unique and must not reuse the reserved approval labels
//! (`Approve`, `Reject`, `Reject and Exit`, `Revise`).

use std::sync::Arc;

use mirri_proto::WireOp;
use sha2::{Digest, Sha256};

use crate::error::EngineResult;
use crate::plan::PlanManager;
use crate::scope::SessionWireLog;
use crate::tool::{
    ExecutionPlan, PlanReviewOption, Tool, ToolErrorResult, ToolExecution, ToolInputDisplay,
    ToolResult,
};

/// Reserved option labels that cannot be used for custom options.
///
/// These are the approval-flow button labels; reusing them as option labels
/// would create ambiguity in the UI.
const RESERVED_OPTION_LABELS: &[&str] = &["approve", "reject", "reject and exit", "revise"];

/// `ExitPlanMode` — exit plan mode and present a plan for approval.
///
/// The tool holds an `Arc<PlanManager>` — the session-scoped service tracking
/// plan-mode state, plan text, and revision history. Exiting plan mode
/// restores the full tool set (writes are permitted again), so
/// `stop_batch_after_this` is set to `true` to ensure the permission change
/// is visible to later tool calls in the same batch.
pub struct ExitPlanModeTool {
    /// The session-scoped plan manager backing this tool.
    plan: Arc<PlanManager>,
    /// The session's wire-log recorder. When attached, exiting plan mode
    /// is persisted as a `plan_mode.exit` op (write-before-mutate) so a
    /// resumed session replays plan mode back to inactive.
    wire_recorder: Option<SessionWireLog>,
}

impl ExitPlanModeTool {
    /// Create the tool bound to the session-scoped plan manager.
    pub fn new(plan: Arc<PlanManager>) -> Self {
        Self {
            plan,
            wire_recorder: None,
        }
    }

    /// Attach the session's wire-log recorder so plan-mode exit persists.
    pub fn with_wire_recorder(mut self, recorder: SessionWireLog) -> Self {
        self.wire_recorder = Some(recorder);
        self
    }
}

impl Tool for ExitPlanModeTool {
    fn name(&self) -> &str {
        "ExitPlanMode"
    }

    fn description(&self) -> &str {
        "Use this tool when you are in plan mode and have finished designing \
         the plan and are ready for user approval. Pass the complete plan text \
         via the `plan` parameter — it is shown to the user for approval and \
         persisted as the plan revision. If the plan offers multiple \
         alternative approaches, pass them via the `options` parameter. Only \
         use this tool for tasks that require planning implementation steps; \
         for research tasks, do NOT use this tool."
    }

    fn parameters(&self) -> &serde_json::Value {
        static SCHEMA: std::sync::OnceLock<serde_json::Value> = std::sync::OnceLock::new();
        SCHEMA.get_or_init(|| {
            serde_json::json!({
                "type": "object",
                "properties": {
                    "plan": {
                        "type": "string",
                        "description": "The complete plan text. Write the full plan here — it is presented to the user for approval and persisted as the plan revision."
                    },
                    "options": {
                        "type": "array",
                        "minItems": 1,
                        "maxItems": 3,
                        "items": {
                            "type": "object",
                            "properties": {
                                "label": {
                                    "type": "string",
                                    "minLength": 1,
                                    "maxLength": 80,
                                    "description": "Short name for this option (1-8 words). Append \"(Recommended)\" if you recommend this option."
                                },
                                "description": {
                                    "type": "string",
                                    "default": "",
                                    "description": "Brief summary of this approach and its trade-offs."
                                }
                            },
                            "required": ["label", "description"],
                            "additionalProperties": false
                        },
                        "description": "When the plan contains multiple alternative approaches, list them here so the user can choose which one to execute. Provide up to 3 options. Each option represents a distinct approach. Do not use \"Reject\", \"Revise\", \"Approve\", or \"Reject and Exit\" as labels."
                    }
                },
                "additionalProperties": false
            })
        })
    }

    fn resolve_execution(&self, input: serde_json::Value) -> EngineResult<ToolExecution> {
        // Validate optional `options` array before building the execution plan.
        let options = match input.get("options") {
            None => Vec::new(),
            Some(serde_json::Value::Array(arr)) => {
                let mut parsed = Vec::with_capacity(arr.len());
                for raw in arr {
                    let Some(label) = raw.get("label").and_then(serde_json::Value::as_str) else {
                        return Ok(ToolExecution::Error(ToolErrorResult {
                            message: "ExitPlanMode option missing 'label'".to_string(),
                        }));
                    };
                    let description = raw
                        .get("description")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    parsed.push((label.to_string(), description));
                }

                // Enforce unique labels (case-insensitive).
                let mut seen = std::collections::HashSet::new();
                for (label, _) in &parsed {
                    let normalized = normalize_label(label);
                    if !seen.insert(normalized) {
                        return Ok(ToolExecution::Error(ToolErrorResult {
                            message: format!(
                                "ExitPlanMode option labels must be unique. Duplicate: '{label}'"
                            ),
                        }));
                    }
                }

                // Reject reserved labels (case-insensitive).
                for (label, _) in &parsed {
                    let normalized = normalize_label(label);
                    if RESERVED_OPTION_LABELS.contains(&normalized.as_str()) {
                        return Ok(ToolExecution::Error(ToolErrorResult {
                            message: format!(
                                "ExitPlanMode option label '{label}' is reserved. Do not use \
                                 Approve, Reject, Reject and Exit, or Revise as option labels."
                            ),
                        }));
                    }
                }

                parsed
            }
            Some(_) => {
                return Ok(ToolExecution::Error(ToolErrorResult {
                    message: "ExitPlanMode 'options' must be an array".to_string(),
                }));
            }
        };

        // The plan text submitted with this call — the rust dialect's
        // channel for plan content (no host-managed plan file exists; the
        // host's enter-time text, when present, is only a fallback). The
        // clone rides the execute closure; the display preview below uses
        // its own clone so neither moves the other.
        let input_plan = input
            .get("plan")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);

        // The display's option preview is built from a clone: the execute
        // closure below still consumes `options` for the model-visible
        // output, so the plan preview must not move it.
        let display_options = options.clone();
        let plan = Arc::clone(&self.plan);
        let wire_recorder = self.wire_recorder.clone();
        let execution_plan = ExecutionPlan {
            accesses: Vec::new(),
            display: ToolInputDisplay::PlanReview {
                plan: input_plan
                    .clone()
                    .filter(|text| !text.trim().is_empty())
                    .or_else(|| self.plan.get_plan())
                    .unwrap_or_default(),
                path: self
                    .plan
                    .get_plan_file_path()
                    .map(|p| p.to_string_lossy().into_owned()),
                options: display_options
                    .into_iter()
                    .map(|(label, description)| PlanReviewOption {
                        label,
                        description: if description.is_empty() {
                            None
                        } else {
                            Some(description)
                        },
                    })
                    .collect(),
            },
            approval_rule: "ExitPlanMode".to_string(),
            // Exiting plan mode restores write permissions; stop the batch so
            // the permission change is visible to subsequent tool calls.
            stop_batch_after_this: true,
        };

        Ok(ToolExecution::Runnable {
            plan: execution_plan,
            execute: Box::new(move |_ctx| {
                Box::pin(async move {
                    // Guard: ExitPlanMode only makes sense while plan mode is
                    // active.
                    if !plan.is_active() {
                        return ToolResult::error(
                            "ExitPlanMode can only be called while plan mode is active. Use \
                             EnterPlanMode (or /plan) first.",
                        );
                    }

                    // The submitted text becomes the plan (write side for
                    // the revision op recorded below). An absent or blank
                    // parameter falls through to the host-set text, if any.
                    if let Some(text) = input_plan.as_deref().filter(|text| !text.trim().is_empty())
                        && let Err(error) = plan.set_plan_text(text.to_string())
                    {
                        return ToolResult::error(format!("Failed to update plan text: {error}"));
                    }

                    // Read the plan text. An empty plan means the model has
                    // not written anything yet.
                    let plan_text = plan.get_plan().unwrap_or_default();
                    if plan_text.trim().is_empty() {
                        return ToolResult::error(
                            "No plan text found. Pass the complete plan text via the `plan` \
                             parameter of ExitPlanMode, then retry.",
                        );
                    }

                    // Record this revision for audit history before exiting.
                    //
                    // Write-before-mutate: the `plan.revision` op is the TS
                    // restore-semantics carrier for plan content (F12) —
                    // reference fields plus the rust dialect's inline
                    // `planText` (the rust kernel has no plan-file/blob
                    // store, so the wire record itself is the durable
                    // content). It lands before the in-memory revision is
                    // recorded, so a crash between the two leaves the wire
                    // log ahead of memory (resume treats the wire log as
                    // the source of truth).
                    if let Some(recorder) = wire_recorder.as_ref() {
                        let id = plan.plan_id().unwrap_or_else(|| {
                            // Defensive: an enter write side (or a resume)
                            // stamps the id; mint one anyway rather than
                            // write an empty key.
                            format!("plan-{}", uuid::Uuid::new_v4())
                        });
                        let version = plan.revision_count() + 1;
                        let sha256 = {
                            let mut hasher = Sha256::new();
                            hasher.update(plan_text.as_bytes());
                            let digest = hasher.finalize();
                            let mut acc = String::with_capacity(digest.len() * 2);
                            for b in digest.iter() {
                                use std::fmt::Write;
                                let _ = write!(acc, "{b:02x}");
                            }
                            acc
                        };
                        recorder
                            .record_op(WireOp::PlanRevision {
                                id,
                                version,
                                path: plan
                                    .get_plan_file_path()
                                    .map(|path| path.to_string_lossy().into_owned())
                                    .unwrap_or_default(),
                                sha256,
                                bytes: plan_text.len() as u64,
                                plan_text: Some(plan_text.clone()),
                            })
                            .await;
                    }
                    plan.record_revision(plan_text.clone());

                    // Write-before-mutate: persist the plan_mode.exit op
                    // before deactivating plan mode in memory, so a crash
                    // between the two leaves the wire log ahead of memory
                    // (resume treats the wire log as the source of truth).
                    if let Some(recorder) = wire_recorder.as_ref() {
                        recorder.record_op(WireOp::PlanModeExit { id: None }).await;
                    }

                    // Deactivate plan mode — the plan text is retained for
                    // reference via `get_plan`.
                    plan.exit();

                    ToolResult::success(format_plan_for_output(&plan_text, &options))
                })
            }),
        })
    }
}

/// Normalize an option label for uniqueness and reserved-label comparison.
///
/// Trims whitespace and lowercases so that `Approve` and `approve` are
/// treated as the same label.
fn normalize_label(label: &str) -> String {
    label.trim().to_lowercase()
}

/// Format the plan text and optional alternatives into the model-visible
/// output message.
fn format_plan_for_output(plan_text: &str, options: &[(String, String)]) -> String {
    let mut lines = vec![
        "Plan mode deactivated. All tools are now available.".to_string(),
        String::new(),
        "## Approved Plan:".to_string(),
        plan_text.to_string(),
    ];

    if !options.is_empty() {
        lines.push(String::new());
        lines.push("## Alternative Approaches:".to_string());
        for (i, (label, description)) in options.iter().enumerate() {
            lines.push(format!("{}. **{label}**: {description}", i + 1));
        }
    }

    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::ToolOutput;

    fn output_text(result: &ToolResult) -> String {
        match &result.output {
            ToolOutput::Text(text) => text.clone(),
            ToolOutput::Parts(_) => panic!("expected text output"),
        }
    }

    async fn execute_tool(tool: &ExitPlanModeTool, input: serde_json::Value) -> ToolResult {
        let execution = tool.resolve_execution(input).expect("resolve");
        let ToolExecution::Runnable { execute, .. } = execution else {
            panic!("expected runnable execution");
        };
        let ctx = crate::tool::ExecutableToolContext {
            wait_registry: None,
            turn_id: "test-turn".to_string(),
            tool_call_id: "test-call".to_string(),
            cancel: crate::cancel::CancelToken::default(),
            progress: crate::tool::ToolProgressHandle::default(),
        };
        execute(ctx).await
    }

    #[tokio::test]
    async fn should_exit_plan_mode_and_return_plan_when_plan_text_exists() {
        let plan = Arc::new(PlanManager::new());
        plan.enter("Step 1: do the thing\nStep 2: verify".to_string())
            .unwrap();
        let tool = ExitPlanModeTool::new(Arc::clone(&plan));

        let result = execute_tool(&tool, serde_json::json!({})).await;

        assert!(!result.is_error, "result: {:?}", result);
        assert!(!plan.is_active(), "plan mode should be inactive after exit");
        let message = output_text(&result);
        assert!(message.contains("Plan mode deactivated"));
        assert!(message.contains("## Approved Plan:"));
        assert!(message.contains("Step 1: do the thing"));
    }

    #[tokio::test]
    async fn should_exit_with_plan_text_passed_via_plan_parameter_when_manager_plan_is_empty() {
        let plan = Arc::new(PlanManager::new());
        // EnterPlanMode always enters with an empty plan — the model submits
        // the content through ExitPlanMode's `plan` parameter.
        plan.enter(String::new()).unwrap();
        let tool = ExitPlanModeTool::new(Arc::clone(&plan));

        let result = execute_tool(
            &tool,
            serde_json::json!({ "plan": "Step 1: do the thing\nStep 2: verify" }),
        )
        .await;

        assert!(!result.is_error, "result: {:?}", result);
        assert!(!plan.is_active(), "plan mode should be inactive after exit");
        assert_eq!(
            plan.get_plan().as_deref(),
            Some("Step 1: do the thing\nStep 2: verify")
        );
        let message = output_text(&result);
        assert!(message.contains("Step 1: do the thing"));
    }

    #[tokio::test]
    async fn should_fall_back_to_host_plan_text_when_plan_parameter_is_blank() {
        let plan = Arc::new(PlanManager::new());
        plan.enter("Step 1: host-entered plan".to_string()).unwrap();
        let tool = ExitPlanModeTool::new(Arc::clone(&plan));

        let result = execute_tool(&tool, serde_json::json!({ "plan": "   " })).await;

        assert!(!result.is_error, "result: {:?}", result);
        assert!(!plan.is_active());
        assert_eq!(
            plan.get_plan().as_deref(),
            Some("Step 1: host-entered plan")
        );
        assert!(output_text(&result).contains("Step 1: host-entered plan"));
    }

    #[tokio::test]
    async fn should_show_submitted_plan_in_review_display_before_execution() {
        let plan = Arc::new(PlanManager::new());
        plan.enter(String::new()).unwrap();
        let tool = ExitPlanModeTool::new(plan);

        let execution = tool
            .resolve_execution(serde_json::json!({ "plan": "Step 1: do the thing" }))
            .unwrap();
        let ToolExecution::Runnable { plan, .. } = execution else {
            panic!("expected runnable execution");
        };

        match &plan.display {
            ToolInputDisplay::PlanReview { plan: text, .. } => {
                assert_eq!(text, "Step 1: do the thing");
            }
            other => panic!("expected PlanReview display, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn should_return_error_when_plan_mode_not_active() {
        let plan = Arc::new(PlanManager::new());
        let tool = ExitPlanModeTool::new(plan);

        let result = execute_tool(&tool, serde_json::json!({})).await;

        assert!(result.is_error);
        let message = output_text(&result);
        assert!(
            message.contains("can only be called while plan mode is active"),
            "should explain plan mode is not active: {message}"
        );
    }

    #[tokio::test]
    async fn should_return_error_when_plan_text_is_empty() {
        let plan = Arc::new(PlanManager::new());
        plan.enter(String::new()).unwrap();
        let tool = ExitPlanModeTool::new(plan);

        let result = execute_tool(&tool, serde_json::json!({})).await;

        assert!(result.is_error);
        let message = output_text(&result);
        assert!(
            message.contains("No plan text found"),
            "should explain no plan text: {message}"
        );
    }

    #[tokio::test]
    async fn should_record_revision_when_exiting_plan_mode() {
        let plan = Arc::new(PlanManager::new());
        plan.enter("Plan v1".to_string()).unwrap();
        let tool = ExitPlanModeTool::new(Arc::clone(&plan));

        let _ = execute_tool(&tool, serde_json::json!({})).await;

        // The plan text is retained after exit for reference.
        assert_eq!(plan.get_plan().as_deref(), Some("Plan v1"));
    }

    #[tokio::test]
    async fn should_include_alternative_approaches_when_options_provided() {
        let plan = Arc::new(PlanManager::new());
        plan.enter("Refactor the module".to_string()).unwrap();
        let tool = ExitPlanModeTool::new(plan);

        let result = execute_tool(
            &tool,
            serde_json::json!({
                "options": [
                    {"label": "Option A (Recommended)", "description": "Use caching"},
                    {"label": "Option B", "description": "Rewrite from scratch"}
                ]
            }),
        )
        .await;

        assert!(!result.is_error);
        let message = output_text(&result);
        assert!(message.contains("Alternative Approaches"));
        assert!(message.contains("Option A (Recommended)"));
        assert!(message.contains("Use caching"));
        assert!(message.contains("Option B"));
    }

    #[tokio::test]
    async fn should_reject_duplicate_option_labels_when_options_share_a_label() {
        let plan = Arc::new(PlanManager::new());
        plan.enter("Plan".to_string()).unwrap();
        let tool = ExitPlanModeTool::new(plan);

        let execution = tool
            .resolve_execution(serde_json::json!({
                "options": [
                    {"label": "Option A", "description": "first"},
                    {"label": "option a", "description": "duplicate"}
                ]
            }))
            .unwrap();

        let ToolExecution::Error(error) = execution else {
            panic!("expected error execution for duplicate labels");
        };
        assert!(error.message.contains("unique"));
    }

    #[tokio::test]
    async fn should_reject_reserved_option_labels_when_option_label_is_reserved() {
        let plan = Arc::new(PlanManager::new());
        plan.enter("Plan".to_string()).unwrap();
        let tool = ExitPlanModeTool::new(plan);

        let execution = tool
            .resolve_execution(serde_json::json!({
                "options": [
                    {"label": "Approve", "description": "reserved label"}
                ]
            }))
            .unwrap();

        let ToolExecution::Error(error) = execution else {
            panic!("expected error execution for reserved label");
        };
        assert!(error.message.contains("reserved"));
    }

    #[tokio::test]
    async fn should_use_plan_review_display_when_exiting_plan_mode() {
        let plan = Arc::new(PlanManager::new());
        plan.enter("Plan".to_string()).unwrap();
        let tool = ExitPlanModeTool::new(plan);

        let execution = tool.resolve_execution(serde_json::json!({})).unwrap();
        let ToolExecution::Runnable { plan, .. } = execution else {
            panic!("expected runnable execution");
        };

        assert!(
            matches!(plan.display, ToolInputDisplay::PlanReview { .. }),
            "expected PlanReview display"
        );
    }

    #[tokio::test]
    async fn should_stop_batch_when_exiting_plan_mode() {
        let plan = Arc::new(PlanManager::new());
        plan.enter("Plan".to_string()).unwrap();
        let tool = ExitPlanModeTool::new(plan);

        let execution = tool.resolve_execution(serde_json::json!({})).unwrap();
        let ToolExecution::Runnable { plan, .. } = execution else {
            panic!("expected runnable execution");
        };

        assert!(
            plan.stop_batch_after_this,
            "exiting plan mode must stop the batch so the permission change is visible"
        );
    }
}
