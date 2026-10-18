//! `EnterPlanModeTool` — enter plan mode for read-only investigation.
//!
//! Mirrors the TypeScript `EnterPlanModeTool`: the model calls this to enter
//! plan mode, after which only read-only tools and plan-mode controls are
//! permitted (enforced by the permission system's `PlanModeGuardDenyPolicy`).
//! Entering when plan mode is already active is reported as an error so the
//! model knows to call `ExitPlanMode` instead.

use std::sync::Arc;

use mirri_proto::WireOp;

use crate::error::EngineResult;
use crate::plan::PlanManager;
use crate::scope::SessionWireLog;
use crate::tool::{ExecutionPlan, Tool, ToolExecution, ToolInputDisplay, ToolResult};

/// `EnterPlanMode` — enter plan mode for read-only investigation.
///
/// The tool holds an `Arc<PlanManager>` — the session-scoped service whose
/// `is_active` / `enter` methods drive the plan-mode state. Entering plan
/// mode changes global permission state (subsequent write tools are denied
/// by the plan-mode guard), so `stop_batch_after_this` is set to `true` to
/// ensure the effect is visible to later tool calls in the same batch.
pub struct EnterPlanModeTool {
    /// The session-scoped plan manager backing this tool.
    plan: Arc<PlanManager>,
    /// The session's wire-log recorder. When attached, entering plan mode
    /// is persisted as a `plan_mode.enter` op (write-before-mutate) so a
    /// resumed session replays plan mode back to active.
    wire_recorder: Option<SessionWireLog>,
}

impl EnterPlanModeTool {
    /// Create the tool bound to the session-scoped plan manager.
    pub fn new(plan: Arc<PlanManager>) -> Self {
        Self {
            plan,
            wire_recorder: None,
        }
    }

    /// Attach the session's wire-log recorder so plan-mode entry persists.
    pub fn with_wire_recorder(mut self, recorder: SessionWireLog) -> Self {
        self.wire_recorder = Some(recorder);
        self
    }
}

impl Tool for EnterPlanModeTool {
    fn name(&self) -> &str {
        "EnterPlanMode"
    }

    fn description(&self) -> &str {
        "Use this tool proactively when you're about to start a non-trivial \
         implementation task. Getting user sign-off on your approach via \
         ExitPlanMode before writing code prevents wasted effort. Use it when \
         ANY of these conditions apply: new feature implementation, multiple \
         valid approaches, code modifications, architectural decisions, \
         multi-file changes, unclear requirements, or when user preferences \
         matter. Do NOT use it for single-line fixes, very specific \
         instructions, or pure research tasks."
    }

    fn parameters(&self) -> &serde_json::Value {
        static SCHEMA: std::sync::OnceLock<serde_json::Value> = std::sync::OnceLock::new();
        SCHEMA.get_or_init(|| {
            serde_json::json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            })
        })
    }

    fn resolve_execution(&self, _input: serde_json::Value) -> EngineResult<ToolExecution> {
        let plan = Arc::clone(&self.plan);
        let wire_recorder = self.wire_recorder.clone();
        let execution_plan = ExecutionPlan {
            accesses: Vec::new(),
            display: ToolInputDisplay::Generic {
                summary: "Entering plan mode".to_string(),
            },
            approval_rule: "EnterPlanMode".to_string(),
            // Entering plan mode changes the permission landscape for every
            // subsequent tool in the batch (writes become denied). Stop the
            // batch so the new permission state is visible.
            stop_batch_after_this: true,
        };

        Ok(ToolExecution::Runnable {
            plan: execution_plan,
            execute: Box::new(move |_ctx| {
                Box::pin(async move {
                    if plan.is_active() {
                        return ToolResult::error(
                            "Plan mode is already active. Use ExitPlanMode when the plan is ready.",
                        );
                    }

                    // The plan id minted for this plan-mode session — the
                    // TS wire contract identifier the revision ops key on.
                    let plan_id = format!("plan-{}", uuid::Uuid::new_v4());

                    // Write-before-mutate: persist the plan_mode.enter op
                    // before entering in memory, so a crash between the two
                    // leaves the wire log ahead of memory (resume treats the
                    // wire log as the source of truth). The `id` follows the
                    // TS wire contract — a plan identifier the plan file is
                    // derived from, not the plan text. The rust dialect's
                    // optional `planText` stays absent here: the model-side
                    // entry always starts from an empty plan.
                    if let Some(recorder) = wire_recorder.as_ref() {
                        recorder
                            .record_op(WireOp::PlanModeEnter {
                                id: plan_id.clone(),
                                plan_text: None,
                            })
                            .await;
                    }

                    // Enter with an empty plan text — the model writes the
                    // actual plan during investigation, and ExitPlanMode reads
                    // it back. An empty string at enter time is the expected
                    // initial state.
                    match plan.enter(String::new()) {
                        Ok(()) => {
                            // Remember the minted id so the ExitPlanMode
                            // revision op keys on the same plan.
                            plan.set_plan_id(plan_id);
                            ToolResult::success(entered_plan_mode_message())
                        }
                        Err(error) => {
                            ToolResult::error(format!("Failed to enter plan mode: {error}"))
                        }
                    }
                })
            }),
        })
    }
}

/// The guidance message shown to the model after entering plan mode.
///
/// Walks the model through the plan-mode workflow: investigate with the
/// read-only tools, then submit the plan through `ExitPlanMode`'s `plan`
/// parameter for approval. Every instruction here matches what the
/// permission system's plan-mode guard actually enforces — the model
/// must never be told to wait for a host that provides nothing.
fn entered_plan_mode_message() -> String {
    [
        "Plan mode is now active. Your workflow:",
        "",
        "1. Investigate with the read-only tools (Read, Grep, Glob) and AskUserQuestion",
        "   (AskUserQuestion is denied in auto mode — go straight to ExitPlanMode there.)",
        "   Bash, Write, Edit, and all other tools are denied while plan mode is active.",
        "2. Design a concrete, step-by-step plan.",
        "3. Call ExitPlanMode with the complete plan text in its `plan` parameter (and",
        "   optional `options` for alternative approaches) to request user approval.",
        "",
        "Do NOT use Write or Edit while plan mode is active — the plan is submitted",
        "through ExitPlanMode's `plan` parameter, not written to a file.",
    ]
    .join("\n")
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

    async fn execute_tool(tool: &EnterPlanModeTool, input: serde_json::Value) -> ToolResult {
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
    async fn should_enter_plan_mode_when_not_already_active() {
        let plan = Arc::new(PlanManager::new());
        let tool = EnterPlanModeTool::new(Arc::clone(&plan));

        let result = execute_tool(&tool, serde_json::json!({})).await;

        assert!(!result.is_error, "result: {:?}", result);
        assert!(plan.is_active(), "plan mode should be active after enter");
        let message = output_text(&result);
        assert!(
            message.contains("Plan mode is now active"),
            "message should confirm entry: {message}"
        );
        assert!(
            message.contains("ExitPlanMode"),
            "message should mention ExitPlanMode: {message}"
        );
    }

    #[tokio::test]
    async fn should_return_error_when_plan_mode_already_active() {
        let plan = Arc::new(PlanManager::new());
        plan.enter("existing plan".to_string()).unwrap();
        let tool = EnterPlanModeTool::new(Arc::clone(&plan));

        let result = execute_tool(&tool, serde_json::json!({})).await;

        assert!(result.is_error);
        let message = output_text(&result);
        assert!(
            message.contains("already active"),
            "should explain plan mode is already active: {message}"
        );
        // The original plan text is preserved.
        assert_eq!(plan.get_plan().as_deref(), Some("existing plan"));
    }

    #[tokio::test]
    async fn should_stop_batch_when_entering_plan_mode() {
        let plan = Arc::new(PlanManager::new());
        let tool = EnterPlanModeTool::new(plan);

        let execution = tool.resolve_execution(serde_json::json!({})).unwrap();
        let ToolExecution::Runnable { plan, .. } = execution else {
            panic!("expected runnable execution");
        };

        assert!(
            plan.stop_batch_after_this,
            "entering plan mode must stop the batch so the permission change is visible"
        );
    }

    #[tokio::test]
    async fn should_use_generic_display_when_entering_plan_mode() {
        let plan = Arc::new(PlanManager::new());
        let tool = EnterPlanModeTool::new(plan);

        let execution = tool.resolve_execution(serde_json::json!({})).unwrap();
        let ToolExecution::Runnable { plan, .. } = execution else {
            panic!("expected runnable execution");
        };

        match &plan.display {
            ToolInputDisplay::Generic { summary } => {
                assert!(
                    summary.contains("plan mode"),
                    "display summary should mention plan mode: {summary}"
                );
            }
            other => panic!("expected Generic display, got {other:?}"),
        }
    }
}
