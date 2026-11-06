//! `CreateScheduledTaskTool` — the agent's face into workspace scheduled
//! tasks (cron-improvement C2, superseded by SDR-126 / ADR-022).
//!
//! The create semantics are ONE approval, the engine's ordinary
//! tool-call approval: the never-auto-approve policy chain asks BEFORE
//! the call executes (any permission mode, never auto-granted), and on
//! approval the tool's `execute` itself creates the task through the
//! host's [`ScheduledTaskCreator`] — approve means create, in the very
//! same call; deny means the call never runs and nothing happens.
//!
//! There is NO scope parameter (the model cannot lie about ownership —
//! ADR-014 kept): the host binds the session's runtime workspace at
//! construction.
//!
//! The lineage defense (D.0d / SDR-106, kept from the propose era) sits at
//! the VERY FRONT of the execution: a turn whose origin is `Cron` (any
//! variant, including subagent propagation) may not raise ANY workspace
//! task creation — one-shot drafts included (mechanical simplicity: a
//! cron turn produces zero workspace creations).

use std::sync::Arc;

use crate::cron::{CronManager, PermissionPosture, TaskConcurrency};
use crate::error::EngineResult;
use crate::permission::TurnOriginContext;
use crate::tool::{
    ExecutionPlan, ScheduledTaskCreateDisplay, Tool, ToolErrorResult, ToolExecution,
    ToolInputDisplay, ToolResult,
};

/// The lineage-defense denial receipt (D.0d kept): cron-lineage turns
/// raise ZERO workspace task creations — the same defense family as the
/// session timer's one-shot chain cap.
pub const CRON_LINEAGE_CREATE_DENY_MESSAGE: &str = "工作区定时任务不可在定时回合内创建（血统防线）\
——定时回合派生的执行链不可再创建工作区定时任务（含一次性）。\
如确需工作区定时任务，请在交互回合发起创建（会触发用户审批），或请用户从任务管理页直接创建。";

/// The full draft the tool hands to the host sink — one serializable
/// struct (the gate display, the `approval.ask` payload, and the decide
/// arm's creation input all read the SAME fields, so the approval card,
/// the ask, and the created task can never disagree).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ScheduledTaskDraft {
    /// The workspace the task will belong to (bound by the host — the
    /// model never chooses).
    pub workspace_id: String,
    /// The 5-field cron expression.
    pub cron: String,
    /// The prompt injected as the execution session's first message when
    /// the task fires (verbatim in the approval card — the user approves
    /// exactly this text).
    pub prompt: String,
    /// Whether the task reschedules after every fire. Workspace tasks
    /// are usually recurring.
    pub recurring: bool,
    /// The IANA timezone the expression is evaluated in and displayed
    /// with (`None` = the host's local calendar).
    pub timezone: Option<String>,
    /// The hard cap on how many times a RECURRING task may fire
    /// (`None` = unbounded).
    pub max_fires: Option<u32>,
    /// The suggested permission MODE of the runs (batch 2 / A4): the same
    /// three session modes, `None` = the server default (`manual` — the
    /// creation form's default).
    pub permission_mode: Option<crate::cron::PermissionMode>,
    /// The legacy posture shape, kept for readers that only know the
    /// pre-batch-2 draft: always `permission_mode`'s expansion when a mode
    /// rides the draft.
    pub suggested_posture: PermissionPosture,
    /// The concurrency posture (skip by default).
    pub concurrency: TaskConcurrency,
    /// Optional human-readable reason, shown in the approval card.
    pub reason: Option<String>,
    /// The execution environment passthrough (C3's full-parameter card):
    /// the task's display title, model alias, and thinking effort.
    pub title: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
}

/// The host-side creation seam: the core tool cannot reach the workspace
/// registry (`WorkspaceCronManager` lives in the embedded host's
/// runtime), so the host installs a creator at agent construction. It
/// runs AFTER the one approval (the engine's default tool approval) has
/// been granted — approve means create, in this very call (ADR-022).
pub trait ScheduledTaskCreator: Send + Sync {
    /// Create the task from the approved draft; returns the new task id,
    /// or a human-readable reason on failure.
    fn create(
        &self,
        draft: ScheduledTaskDraft,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, String>> + Send + '_>>;
}

/// `CreateScheduledTask` — create a workspace scheduled task, gated by
/// the never-auto-approve high-risk approval in every permission mode.
pub struct CreateScheduledTaskTool {
    /// The workspace the task targets (the proposing session's runtime
    /// workspace — bound by the host, never chosen by the model).
    workspace_id: String,
    /// The host-side creation seam (see [`ScheduledTaskCreator`]):
    /// `None` (tests, core-only agents) makes an approved execution fail
    /// honestly (no host registry to create in).
    creator: Option<Arc<dyn ScheduledTaskCreator>>,
    /// Whether `MIRRICODE_DISABLE_CRON` is set (ADR-006 / E.4): a disabled
    /// server refuses new creations at this face.
    scheduling_disabled: bool,
}

impl CreateScheduledTaskTool {
    /// Create the tool bound to the proposing session's workspace.
    pub fn new(workspace_id: String) -> Self {
        Self {
            workspace_id,
            creator: None,
            scheduling_disabled: false,
        }
    }

    /// Wire the host-side creator (the embedded server's
    /// workspace-registry-backed implementation).
    pub fn with_creator(mut self, creator: Arc<dyn ScheduledTaskCreator>) -> Self {
        self.creator = Some(creator);
        self
    }

    /// Mark the tool as running under `MIRRICODE_DISABLE_CRON=1`
    /// (ADR-006 / E.4): creation is refused with the documented reason.
    pub fn with_scheduling_disabled(mut self, disabled: bool) -> Self {
        self.scheduling_disabled = disabled;
        self
    }
}

impl Tool for CreateScheduledTaskTool {
    fn name(&self) -> &str {
        "CreateScheduledTask"
    }

    fn description(&self) -> &str {
        "Create a WORKSPACE scheduled task — recurring work the user delegated that \
         must run on its own schedule across sessions (e.g. an automated PR review, \
         a daily news digest), each run in a fresh execution session. Use this ONLY \
         for routine workspace-level work the user explicitly wants on a schedule; \
         for a one-shot delayed reminder inside the CURRENT session use SetTimer \
         instead. Every call asks the user to approve in every permission mode \
         (never auto-granted); once approved the task is created immediately and \
         this call returns its id. If the user declines, nothing is created and \
         the call is reported as rejected with their reason. You cannot edit or \
         delete existing scheduled tasks — only the user can, from the task \
         management page."
    }

    fn parameters(&self) -> &serde_json::Value {
        static SCHEMA: std::sync::OnceLock<serde_json::Value> = std::sync::OnceLock::new();
        SCHEMA.get_or_init(|| {
            serde_json::json!({
                "type": "object",
                "properties": {
                    "cron": {
                        "type": "string",
                        "description": "Standard 5-field cron expression (minute hour day-of-month month day-of-week) describing how often the task fires."
                    },
                    "prompt": {
                        "type": "string",
                        "description": "The instruction the task executes on every run — injected as the execution session's first message (verbatim in the approval card: the user approves exactly this text)."
                    },
                    "recurring": {
                        "type": "boolean",
                        "default": true,
                        "description": "Whether the task reschedules after every fire (true) or runs once (false). Workspace tasks are usually recurring."
                    },
                    "timezone": {
                        "type": "string",
                        "description": "Optional IANA timezone name (e.g. 'Asia/Shanghai') the cron expression is evaluated in and displayed with. Defaults to the host's local timezone."
                    },
                    "max_fires": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Optional hard cap on how many times a RECURRING task may fire. Only valid with recurring=true; omitted = unbounded (shown as such in the approval card)."
                    },
                    "permission_mode": {
                        "type": "string",
                        "enum": ["manual", "auto", "yolo"],
                        "description": "The suggested permission mode of the runs — the SAME three modes a session carries: 'manual' (default — approval asks wait for the user), 'auto' (everything allowed without asking), 'yolo' (never asks, allows everything). The user approves this as part of the draft."
                    },
                    "permission_posture": {
                        "type": "string",
                        "enum": ["ask-deny", "auto", "read-only"],
                        "description": "LEGACY spelling of permission_mode (kept for older callers): 'ask-deny' ≈ 'manual', 'auto' = 'auto', 'read-only' rejects every write. Prefer permission_mode."
                    },
                    "title": {
                        "type": "string",
                        "description": "Optional human-readable task title shown in the task list and the approval card."
                    },
                    "model": {
                        "type": "string",
                        "description": "Optional model alias the task's runs bind (shown in the approval card)."
                    },
                    "effort": {
                        "type": "string",
                        "enum": ["low", "medium", "high"],
                        "description": "Optional thinking effort the task's runs carry (shown in the approval card)."
                    },
                    "reason": {
                        "type": "string",
                        "description": "Optional human-readable reason for the creation, shown in the approval card. Display only."
                    }
                },
                "required": ["cron", "prompt"],
                "additionalProperties": false
            })
        })
    }

    fn resolve_execution(&self, input: serde_json::Value) -> EngineResult<ToolExecution> {
        // ADR-006 / E.4 layer 1: a disabled cron subsystem refuses creation
        // outright (same face as SetTimer).
        if self.scheduling_disabled {
            return Ok(ToolExecution::Error(ToolErrorResult {
                message: "定时任务已禁用（MIRRICODE_DISABLE_CRON）".to_string(),
            }));
        }
        let Some(cron_expr) = input.get("cron").and_then(serde_json::Value::as_str) else {
            return Ok(ToolExecution::Error(ToolErrorResult {
                message: "CreateScheduledTask requires a string 'cron' argument".to_string(),
            }));
        };
        let Some(prompt) = input.get("prompt").and_then(serde_json::Value::as_str) else {
            return Ok(ToolExecution::Error(ToolErrorResult {
                message: "CreateScheduledTask requires a string 'prompt' argument".to_string(),
            }));
        };
        // The draft defaults to recurring=true (workspace delegation is the
        // recurring-work product face — the mirror image of SetTimer's
        // one-shot semantics).
        let recurring = input
            .get("recurring")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(true);
        let timezone = input
            .get("timezone")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        let max_fires = input
            .get("max_fires")
            .or_else(|| input.get("maxFires"))
            .and_then(serde_json::Value::as_u64)
            .map(|budget| budget as u32);
        // Batch 2 (A4): the draft carries the same three session modes; a
        // legacy `permission_posture` input still parses (best effort).
        let permission_mode = match input
            .get("permission_mode")
            .or_else(|| input.get("permissionMode"))
            .and_then(serde_json::Value::as_str)
        {
            Some("auto") => Some(crate::cron::PermissionMode::Auto),
            Some("yolo") => Some(crate::cron::PermissionMode::Yolo),
            Some("manual") => Some(crate::cron::PermissionMode::Manual),
            Some("read-only") => None,
            _ => match input
                .get("permission_posture")
                .or_else(|| input.get("permissionPosture"))
                .and_then(serde_json::Value::as_str)
            {
                Some("auto") => Some(crate::cron::PermissionMode::Auto),
                Some("read-only") => None,
                Some(_) => Some(crate::cron::PermissionMode::Manual),
                // No declaration at all: the server default is `manual`
                // (the same default a new session starts with), so the
                // card always shows a concrete mode.
                None => Some(crate::cron::PermissionMode::Manual),
            },
        };
        let suggested_posture = match permission_mode {
            Some(mode) => mode.posture(),
            None => match input
                .get("permission_posture")
                .or_else(|| input.get("permissionPosture"))
                .and_then(serde_json::Value::as_str)
            {
                Some("auto") => PermissionPosture::Auto,
                Some("read-only") => PermissionPosture::ReadOnly,
                _ => PermissionPosture::Manual,
            },
        };
        let title = input
            .get("title")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        let model = input
            .get("model")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        let effort = input
            .get("effort")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        let reason = input
            .get("reason")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);

        let cron_expr = cron_expr.to_string();
        let prompt = prompt.to_string();

        // The validate-then-create inputs travel into the execute closure.
        let workspace_id = self.workspace_id.clone();
        let creator = self.creator.clone();
        let display = ToolInputDisplay::ScheduledTaskCreate(Box::new(ScheduledTaskCreateDisplay {
            cron: cron_expr.to_string(),
            timezone: timezone.clone(),
            workspace_id: self.workspace_id.clone(),
            permission_mode: permission_mode.map(|mode| mode.as_str().to_string()),
            model: model.clone(),
            effort: effort.clone(),
            title: title.clone(),
            prompt: prompt.to_string(),
            recurring,
            max_fires,
            reason: reason.clone(),
        }));
        let plan = ExecutionPlan {
            accesses: Vec::new(),
            display,
            approval_rule: "CreateScheduledTask".to_string(),
            stop_batch_after_this: false,
        };

        Ok(ToolExecution::Runnable {
            plan,
            execute: Box::new(move |ctx| {
                Box::pin(async move {
                    // D.0d / SDR-106 (kept): the lineage defense sits at
                    // the VERY FRONT — a Cron-origin turn (any variant,
                    // subagent propagation included) raises ZERO workspace
                    // task creations (one-shot drafts included).
                    if matches!(ctx.turn_origin, TurnOriginContext::Cron { .. }) {
                        return ToolResult::error(CRON_LINEAGE_CREATE_DENY_MESSAGE.to_string());
                    }
                    // Structural validation (the shared family —
                    // `next_fire_for` validates the expression AND the
                    // timezone in one call): a draft that cannot schedule
                    // is never raised for approval.
                    if !recurring && max_fires.is_some() {
                        return ToolResult::error(
                            "一次性任务无 max_fires 参数（one-shot 恰好触发一次）".to_string(),
                        );
                    }
                    if max_fires == Some(0) {
                        return ToolResult::error("max_fires 必须为正整数".to_string());
                    }
                    if CronManager::next_fire_for(&cron_expr, timezone.as_deref()).is_none() {
                        return ToolResult::error(format!(
                            "CreateScheduledTask failed: 表达式或时区不可用（cron: {cron_expr}, timezone: {}）",
                            timezone.as_deref().unwrap_or("<local>")
                        ));
                    }
                    let draft = ScheduledTaskDraft {
                        workspace_id: workspace_id.clone(),
                        cron: cron_expr.clone(),
                        timezone: timezone.clone(),
                        prompt: prompt.clone(),
                        recurring,
                        max_fires,
                        permission_mode,
                        suggested_posture,
                        concurrency: TaskConcurrency::Skip,
                        reason: reason.clone(),
                        title: title.clone(),
                        model: model.clone(),
                        effort: effort.clone(),
                    };
                    // ADR-022: the creation happens IN the approved
                    // execution — approve means create, in this very
                    // call. Without a host creator (core-only agents,
                    // tests) the call fails honestly instead of
                    // pretending a creation was raised.
                    let Some(creator) = creator else {
                        return ToolResult::error(
                            "CreateScheduledTask 未接入宿主创建器（无法创建工作区定时任务）".to_string(),
                        );
                    };
                    match creator.create(draft).await {
                        Ok(task_id) => {
                            let schedule = match &timezone {
                                Some(zone) => format!("{cron_expr} [{zone}]"),
                                None => cron_expr.clone(),
                            };
                            ToolResult::success(
                                serde_json::to_string_pretty(&serde_json::json!({
                                    "created": true,
                                    "taskId": task_id,
                                    "schedule": schedule,
                                    "recurring": recurring,
                                }))
                                .unwrap_or_else(|e| format!("{{\"error\": \"{e}\"}}")),
                            )
                        }
                        Err(message) => {
                            ToolResult::error(format!("CreateScheduledTask failed: {message}"))
                        }
                    }
                })
            }),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn output_text(result: &ToolResult) -> String {
        match &result.output {
            crate::tool::ToolOutput::Text(text) => text.clone(),
            crate::tool::ToolOutput::Parts(_) => panic!("expected text output"),
        }
    }

    async fn execute_tool(tool: &CreateScheduledTaskTool, input: serde_json::Value) -> ToolResult {
        let execution = tool.resolve_execution(input).expect("resolve");
        let ToolExecution::Runnable { execute, .. } = execution else {
            panic!("expected runnable execution");
        };
        let ctx = crate::tool::ExecutableToolContext {
            approval_metadata: None,
            wait_registry: None,
            turn_id: "test-turn".to_string(),
            tool_call_id: "test-call".to_string(),
            cancel: crate::cancel::CancelToken::default(),
            progress: crate::tool::ToolProgressHandle::default(),
            turn_origin: Default::default(),
        };
        execute(ctx).await
    }

    fn tool() -> CreateScheduledTaskTool {
        CreateScheduledTaskTool::new("ws-1".to_string())
    }

    fn cron_ctx(turn_origin: TurnOriginContext) -> crate::tool::ExecutableToolContext {
        crate::tool::ExecutableToolContext {
            approval_metadata: None,
            wait_registry: None,
            turn_id: "test-turn".to_string(),
            tool_call_id: "test-call".to_string(),
            cancel: crate::cancel::CancelToken::default(),
            progress: crate::tool::ToolProgressHandle::default(),
            turn_origin,
        }
    }

    /// Given a creator wired and an approved execution, when the tool runs
    /// with the full parameter set, then the created draft carries every
    /// parameter the user judged on the card — no proposal state, no TTL,
    /// no intermediate concept (ADR-022: approve means create).
    #[tokio::test]
    async fn should_carry_the_full_draft_through_the_creation_without_any_proposal_concept() {
        let creator = Arc::new(RecordingCreator(std::sync::Mutex::new(Vec::new())));
        let tool = CreateScheduledTaskTool::new("ws-1".to_string())
            .with_creator(Arc::clone(&creator) as Arc<dyn ScheduledTaskCreator>);
        let result = execute_tool(
            &tool,
            serde_json::json!({
                "cron": "0 9 * * *",
                "prompt": "morning patrol",
                "recurring": true,
                "timezone": "Asia/Shanghai",
                "max_fires": 10,
                "permission_posture": "read-only",
                "title": "每日巡检",
                "reason": "daily check"
            }),
        )
        .await;
        assert!(!result.is_error, "{}", output_text(&result));
        let drafts = creator.0.lock().unwrap();
        let draft = &drafts[0];
        assert_eq!(draft.cron, "0 9 * * *");
        assert_eq!(draft.prompt, "morning patrol");
        assert!(draft.recurring);
        assert_eq!(draft.timezone.as_deref(), Some("Asia/Shanghai"));
        assert_eq!(draft.max_fires, Some(10));
        assert_eq!(draft.suggested_posture, PermissionPosture::ReadOnly);
        assert_eq!(draft.title.as_deref(), Some("每日巡检"));
        // C2/SDR-126: no proposal concepts anywhere on the face.
        let text = output_text(&result);
        for banned in ["proposal", "expires", "72", "TTL", "pending"] {
            assert!(
                !text.to_lowercase().contains(banned.to_lowercase().as_str()),
                "the face must not carry the retired '{banned}' concept: {text}"
            );
        }
    }

    struct RecordingCreator(std::sync::Mutex<Vec<ScheduledTaskDraft>>);
    impl ScheduledTaskCreator for RecordingCreator {
        fn create(
            &self,
            draft: ScheduledTaskDraft,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<String, String>> + Send + '_>,
        > {
            Box::pin(async move {
                self.0.lock().unwrap().push(draft);
                Ok("cron-created-1".to_string())
            })
        }
    }

    /// Given a creator is wired, when the approved tool executes, then the
    /// task is created in the same call and the result says so with the id.
    #[tokio::test]
    async fn should_create_the_task_in_the_same_call_when_the_execution_runs() {
        let creator = Arc::new(RecordingCreator(std::sync::Mutex::new(Vec::new())));
        let tool = CreateScheduledTaskTool::new("ws".to_string())
            .with_creator(Arc::clone(&creator) as Arc<dyn ScheduledTaskCreator>);
        let result = execute_tool(
            &tool,
            serde_json::json!({
                "cron": "15 10 * * 1-5", "prompt": "p", "recurring": true,
                "timezone": "Asia/Tokyo"
            }),
        )
        .await;
        assert!(!result.is_error, "{}", output_text(&result));
        let text = output_text(&result);
        let payload: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(payload["created"], true);
        assert_eq!(payload["taskId"], "cron-created-1");
        assert_eq!(payload["schedule"], "15 10 * * 1-5 [Asia/Tokyo]");
        let drafts = creator.0.lock().unwrap();
        assert_eq!(drafts.len(), 1, "exactly one creation");
        assert_eq!(drafts[0].cron, "15 10 * * 1-5");
        assert_eq!(drafts[0].workspace_id, "ws");
    }

    /// Given the tool is resolved, when the approval card's display is
    /// read, then it is the structured creation — every parameter the
    /// user judges, never a one-line generic summary.
    #[test]
    fn should_expose_the_full_creation_as_a_structured_display() {
        let tool = CreateScheduledTaskTool::new("ws".to_string());
        let execution = tool
            .resolve_execution(serde_json::json!({
                "cron": "0 9 * * *", "prompt": "digest",
                "title": "Daily digest", "permission_mode": "auto",
                "max_fires": 3, "reason": "user asked"
            }))
            .expect("resolve");
        let ToolExecution::Runnable { plan, .. } = execution else {
            panic!("expected runnable execution");
        };
        match plan.display {
            ToolInputDisplay::ScheduledTaskCreate(ref display) => {
                assert_eq!(display.cron, "0 9 * * *");
                assert_eq!(display.title.as_deref(), Some("Daily digest"));
                assert_eq!(display.permission_mode.as_deref(), Some("auto"));
                assert_eq!(display.max_fires, Some(3));
                assert!(display.recurring, "recurring defaults to true");
                assert_eq!(display.prompt, "digest");
                assert_eq!(display.reason.as_deref(), Some("user asked"));
            }
            other => panic!("expected the structured creation display, got {other:?}"),
        }
    }

    /// Given no creator is wired, when the approved tool executes, then it
    /// fails honestly instead of pretending a creation was raised.
    #[tokio::test]
    async fn should_fail_when_no_creator_is_wired() {
        let tool = CreateScheduledTaskTool::new("ws".to_string());
        let result = execute_tool(
            &tool,
            serde_json::json!({ "cron": "0 9 * * *", "prompt": "p", "recurring": true }),
        )
        .await;
        assert!(result.is_error);
        let text = output_text(&result);
        assert!(text.contains("未接入宿主创建器"), "{text}");
    }

    /// Given a cron-lineage turn (workspace or session-timer origin), when
    /// CreateScheduledTask runs, then NO creation is raised — the D.0d
    /// lineage front (zero workspace creations from cron turns).
    #[tokio::test]
    async fn should_raise_no_creation_when_called_from_a_cron_lineage_turn() {
        for parent_is_workspace in [false, true] {
            let execution = tool()
                .resolve_execution(serde_json::json!({
                    "cron": "0 9 * * *", "prompt": "p", "recurring": true
                }))
                .expect("resolve");
            let ToolExecution::Runnable { execute, .. } = execution else {
                panic!("expected runnable execution");
            };
            let result = execute(cron_ctx(TurnOriginContext::Cron {
                workspace: parent_is_workspace,
                reject_asks: true,
                readonly: false,
                task_id: "cron-parent".to_string(),
            }))
            .await;
            assert!(result.is_error, "workspace={parent_is_workspace}");
            let text = output_text(&result);
            assert!(text.contains("血统防线"), "names the defense: {text}");
            assert!(
                text.contains("工作区定时任务不可在定时回合内创建"),
                "the D.0d wording: {text}"
            );
            assert!(text.contains("一次性"), "covers one-shot drafts: {text}");
        }
    }

    /// Given a one-shot draft with max_fires, when the draft is validated,
    /// then it is refused before any registration (the shared validation
    /// family).
    #[tokio::test]
    async fn should_refuse_a_one_shot_draft_that_declares_max_fires() {
        let result = execute_tool(
            &tool(),
            serde_json::json!({
                "cron": "*/5 * * * *", "prompt": "p", "recurring": false, "max_fires": 3
            }),
        )
        .await;
        assert!(result.is_error);
        assert!(output_text(&result).contains("一次性"));
    }

    /// Given an invalid expression or timezone, when the draft is
    /// validated, then nothing is raised for approval.
    #[tokio::test]
    async fn should_refuse_a_draft_whose_expression_or_timezone_is_invalid() {
        for bad in [
            serde_json::json!({"cron": "not a cron", "prompt": "p"}),
            serde_json::json!({"cron": "0 9 * * *", "prompt": "p", "timezone": "Not/A_Zone"}),
        ] {
            let result = execute_tool(&tool(), bad).await;
            assert!(result.is_error, "{}", output_text(&result));
            assert!(output_text(&result).contains("CreateScheduledTask failed"));
        }
    }

    /// Given a disabled cron subsystem, when CreateScheduledTask runs, then
    /// the documented refusal answers before anything else.
    #[tokio::test]
    async fn should_refuse_creations_when_scheduling_is_disabled() {
        let tool = CreateScheduledTaskTool::new("ws".to_string())
            .with_scheduling_disabled(true);
        let execution = tool
            .resolve_execution(serde_json::json!({"cron": "0 9 * * *", "prompt": "p"}))
            .expect("resolve");
        let ToolExecution::Error(error) = execution else {
            panic!("expected synthetic error execution");
        };
        assert_eq!(error.message, "定时任务已禁用（MIRRICODE_DISABLE_CRON）");
    }
}
