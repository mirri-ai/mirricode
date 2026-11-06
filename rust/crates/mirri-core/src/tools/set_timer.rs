//! `SetTimerTool` — the agent's single in-session delayed-callback tool
//! (cron-improvement C1, replacing the retired CronCreate/CronList/
//! CronDelete trio).
//!
//! The tool expresses exactly two things: HOW LONG from now (whole
//! minutes) and WHAT to say when the timer fires. It is ALWAYS one-shot:
//! the target instant is converted to a 5-field cron expression pinned to
//! that exact minute (all fields fixed — naturally unique), registered on
//! the session-scoped [`CronManager`] as `recurring: false`, and removed
//! automatically after the single fire (the existing one-shot semantics —
//! the model neither needs nor has any cleanup tool). The fire rides the
//! existing `inject_cron_prompt` chain (idle injection, missed-fire
//! coalescing, the 30s tick and the whole-minute one-shot early-jitter
//! window).
//!
//! The lineage defense rides the existing one-shot chain cap (A.6b, cap
//! 12): a cron-fired turn MAY set another timer (the "re-check in 10
//! minutes" pattern) but the cumulative count is capped, so the
//! set-next-timer-at-fire pattern cannot loop forever. There is no
//! recurring arm to defend — the parameter does not exist.

use std::sync::Arc;

use chrono::{Datelike, Timelike};

use crate::cron::{CronManager, CronTimezone, TaskOrigin, format_local_iso_with_offset};
use crate::error::EngineResult;
use crate::tool::{ExecutionPlan, Tool, ToolErrorResult, ToolExecution, ToolInputDisplay, ToolResult};

/// `SetTimer` — a one-shot delayed reminder that injects a message into
/// the current session.
pub struct SetTimerTool {
    /// The session-scoped cron manager this tool mutates.
    cron: Arc<CronManager>,
    /// The session this tool's timers belong to.
    session_id: String,
    /// Whether `MIRRICODE_DISABLE_CRON` is set (ADR-006 / E.4): a disabled
    /// server refuses new timers at this face.
    scheduling_disabled: bool,
}

impl SetTimerTool {
    /// Create the tool bound to the session-scoped cron manager.
    ///
    /// The `session_id` is captured at construction time so the model never
    /// has to supply it; every timer created through this tool belongs to
    /// the same session.
    pub fn new(cron: Arc<CronManager>, session_id: String) -> Self {
        Self {
            cron,
            session_id,
            scheduling_disabled: false,
        }
    }

    /// Mark the tool as running under `MIRRICODE_DISABLE_CRON=1` (ADR-006 /
    /// E.4): creation is refused with the documented reason. The switch is
    /// read once at server construction (restart-time semantics — E.4),
    /// never per call.
    pub fn with_scheduling_disabled(mut self, disabled: bool) -> Self {
        self.scheduling_disabled = disabled;
        self
    }
}

/// Convert a delay (in whole minutes) into the 5-field cron expression
/// pinned to the target minute on the given calendar: `M H D Mon *`.
///
/// A delay under one minute rounds UP to the next whole minute (the
/// engine's 30s detection tick cannot honor a sub-minute deadline anyway).
/// The day-of-week field stays `*`: the day-of-month pin alone identifies
/// the date, whereas pinning both triggers cron's OR semantics and would
/// widen the day window.
fn timer_cron_expression(
    delay_minutes: i64,
    calendar: chrono_tz::Tz,
) -> (String, chrono::DateTime<chrono::Utc>) {
    let now = chrono::Utc::now();
    let target = (now + chrono::Duration::minutes(delay_minutes.max(1))).with_timezone(&calendar);
    let mut target = target
        .with_second(0)
        .and_then(|t| t.with_nanosecond(0))
        .unwrap_or(target);
    // A sub-minute delay (rounded to the same minute as now) must still
    // land in the FUTURE: bump to the next minute when the truncation
    // collapsed the delay away.
    if target.with_timezone(&chrono::Utc) <= now {
        target += chrono::Duration::minutes(1);
    }
    // Day-of-week stays `*` (the day-of-month pin alone identifies the
    // date; pinning both fields triggers cron's OR semantics and could
    // double the day window).
    let expression = format!(
        "{} {} {} {} *",
        target.minute(),
        target.hour(),
        target.day(),
        target.month()
    );
    (expression, target.with_timezone(&chrono::Utc))
}

impl Tool for SetTimerTool {
    fn name(&self) -> &str {
        "SetTimer"
    }

    fn description(&self) -> &str {
        "Set a one-shot timer in the CURRENT session: after `delay_minutes` \
         whole minutes, your `message` is injected into the session as if \
         the user sent it, and the timer is gone — it fires exactly once \
         and cleans itself up, no follow-up needed. Use it for delayed \
         follow-ups inside this conversation (e.g. 'check the CI result in \
         10 minutes'); when a repeated patrol is needed, simply set the \
         next timer again at each fire. This is NOT for recurring or \
         cross-session work — for scheduled routine work in the workspace \
         use CreateScheduledTask (which requires user approval). Timing \
         notes: detection ticks every 30 seconds, and whole-minute targets \
         may fire up to 90 seconds early, so expect the message around the \
         requested minute. Fires are only injected into an idle session: \
         while a turn runs they are held and injected when it ends (missed \
         occurrences coalesce into one fire). A delay under one minute is \
         rounded up to the next whole minute."
    }

    fn parameters(&self) -> &serde_json::Value {
        static SCHEMA: std::sync::OnceLock<serde_json::Value> = std::sync::OnceLock::new();
        SCHEMA.get_or_init(|| {
            serde_json::json!({
                "type": "object",
                "properties": {
                    "delay_minutes": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "How many whole minutes from now until the message is injected. Rounded up when under one minute."
                    },
                    "message": {
                        "type": "string",
                        "description": "What to inject into the session when the timer fires — write it as the instruction you want to receive at that moment."
                    }
                },
                "required": ["delay_minutes", "message"],
                "additionalProperties": false
            })
        })
    }

    fn resolve_execution(&self, input: serde_json::Value) -> EngineResult<ToolExecution> {
        // ADR-006 / E.4 layer 1: a disabled cron subsystem refuses creation
        // outright.
        if self.scheduling_disabled {
            return Ok(ToolExecution::Error(ToolErrorResult {
                message: "定时任务已禁用（MIRRICODE_DISABLE_CRON）".to_string(),
            }));
        }
        let Some(delay) = input.get("delay_minutes").and_then(serde_json::Value::as_i64) else {
            return Ok(ToolExecution::Error(ToolErrorResult {
                message: "SetTimer requires an integer 'delay_minutes' argument".to_string(),
            }));
        };
        if delay < 1 {
            return Ok(ToolExecution::Error(ToolErrorResult {
                message: "SetTimer requires a positive 'delay_minutes' (sub-minute delays are rounded up to 1)"
                    .to_string(),
            }));
        }
        let Some(message) = input.get("message").and_then(serde_json::Value::as_str) else {
            return Ok(ToolExecution::Error(ToolErrorResult {
                message: "SetTimer requires a string 'message' argument".to_string(),
            }));
        };

        let message = message.to_string();
        let summary = format!("timer in {delay}m: {message}");
        let cron_manager = Arc::clone(&self.cron);
        let session_id = self.session_id.clone();
        let plan = ExecutionPlan {
            accesses: Vec::new(),
            display: ToolInputDisplay::Generic { summary },
            approval_rule: "SetTimer".to_string(),
            stop_batch_after_this: false,
        };

        Ok(ToolExecution::Runnable {
            plan,
            execute: Box::new(move |ctx| {
                Box::pin(async move {
                    // The one-shot target minute on the host's local
                    // calendar (SetTimer has no timezone parameter — "in N
                    // minutes" is calendar-agnostic).
                    let calendar = CronTimezone::Local.resolve();
                    let (expression, _target) = timer_cron_expression(delay, calendar);
                    // The lineage arm (A.6b): a cron-fired turn setting
                    // another timer rides the capped one-shot chain; an
                    // interactive turn takes the plain add. Both are
                    // recurring: false by construction.
                    let add_result = if matches!(
                        ctx.turn_origin,
                        crate::permission::TurnOriginContext::Cron { .. }
                    ) {
                        cron_manager
                            .add_lineage_one_shot(
                                expression.clone(),
                                message.clone(),
                                session_id.clone(),
                                None,
                            )
                            .await
                    } else {
                        cron_manager
                            .add(
                                expression.clone(),
                                message.clone(),
                                session_id.clone(),
                                false,
                                crate::cron::CronCreateOptions {
                                    origin: TaskOrigin::Agent,
                                    cron_lineage: false,
                                    ..crate::cron::CronCreateOptions::default()
                                },
                            )
                            .await
                    };
                    let id = match add_result {
                        Ok(id) => id,
                        Err(error) => {
                            return ToolResult::error(format!("SetTimer failed: {error}"));
                        }
                    };
                    let task = match cron_manager.get(&id) {
                        Some(task) => task,
                        None => {
                            return ToolResult::error(
                                "The timer was removed before its details could be retrieved."
                                    .to_string(),
                            );
                        }
                    };
                    let tz = CronTimezone::parse(task.timezone.as_deref())
                        .map(|declared| declared.resolve())
                        .unwrap_or_else(|_| CronTimezone::Local.resolve());
                    let payload = serde_json::json!({
                        "id": task.id,
                        "delayMinutes": delay,
                        "message": task.prompt,
                        "firesOnceAt": task
                            .next_fire_at
                            .map(|at| format_local_iso_with_offset(at, tz)),
                    });
                    ToolResult::success(
                        serde_json::to_string_pretty(&payload)
                            .unwrap_or_else(|e| format!("{{\"error\": \"{e}\"}}")),
                    )
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

    async fn execute_tool(tool: &SetTimerTool, input: serde_json::Value) -> ToolResult {
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

    fn lineage_ctx() -> crate::tool::ExecutableToolContext {
        crate::tool::ExecutableToolContext {
            approval_metadata: None,
            wait_registry: None,
            turn_id: "test-turn".to_string(),
            tool_call_id: "test-call".to_string(),
            cancel: crate::cancel::CancelToken::default(),
            progress: crate::tool::ToolProgressHandle::default(),
            turn_origin: crate::permission::TurnOriginContext::Cron {
                reject_asks: true,
                workspace: false,
                readonly: false,
                task_id: "cron-parent".to_string(),
            },
        }
    }

    /// Given a valid delay and message, when SetTimer runs, then a
    /// one-shot task lands on the session's cron manager with the target
    /// minute's full-field-pinned expression, and the reply reports the
    /// delay, the verbatim message, and the fire instant.
    #[tokio::test]
    async fn should_register_a_one_shot_task_pinned_to_the_target_minute() {
        let manager = Arc::new(CronManager::new());
        let tool = SetTimerTool::new(Arc::clone(&manager), "test-session".to_string());
        let result = execute_tool(
            &tool,
            serde_json::json!({ "delay_minutes": 10, "message": "check the CI result" }),
        )
        .await;
        assert!(!result.is_error, "{}", output_text(&result));
        let payload: serde_json::Value = serde_json::from_str(&output_text(&result)).unwrap();
        let id = payload["id"].as_str().expect("id present").to_string();
        let task = manager.get(&id).expect("the timer registered");
        assert!(!task.recurring, "C1: a timer is one-shot by construction");
        assert_eq!(task.prompt, "check the CI result");
        assert_eq!(task.session_id, "test-session");
        // The expression pins the target minute: minute/hour/day/month
        // are fixed values, day-of-week stays `*` (pinning both day fields
        // would trigger cron's OR semantics), and no */-style frequency
        // token survives — the schedule fires exactly once by shape.
        let fields: Vec<&str> = task.cron.split_whitespace().collect();
        assert_eq!(fields.len(), 5, "the pinned-minute shape: {}", task.cron);
        for (index, field) in fields.iter().enumerate() {
            if index == 4 {
                assert_eq!(*field, "*", "day-of-week stays `*`: {}", task.cron);
            } else {
                assert!(
                    field.chars().all(|c| c.is_ascii_digit()),
                    "field {index} is a fixed value (no frequency tokens): {}",
                    task.cron
                );
            }
        }
        assert_eq!(payload["delayMinutes"], 10);
        assert_eq!(payload["message"], "check the CI result");
        assert!(
            payload["firesOnceAt"].is_string(),
            "the fire instant is reported: {}",
            output_text(&result)
        );
    }

    /// Given a timer whose target minute lands due, when the manager's
    /// one-shot fire consumes it, then the timer disappears from the table
    /// — it fires once and cleans itself up (C1's acceptance).
    #[tokio::test]
    async fn should_disappear_from_the_table_after_its_single_fire() {
        let manager = Arc::new(CronManager::new());
        let tool = SetTimerTool::new(Arc::clone(&manager), "test-session".to_string());
        let result = execute_tool(
            &tool,
            serde_json::json!({ "delay_minutes": 5, "message": "once" }),
        )
        .await;
        let payload: serde_json::Value = serde_json::from_str(&output_text(&result)).unwrap();
        let id = payload["id"].as_str().unwrap().to_string();
        // Force the fire due and run one check_due pass (the one-shot
        // removal arm — set_next_fire_at only moves the cursor).
        assert!(
            manager.set_next_fire_at(&id, Some(chrono::Utc::now() - chrono::Duration::seconds(5))),
            "the forced-due timer is set due"
        );
        let fires = manager.check_due().await;
        assert_eq!(fires.len(), 1, "the forced-due timer fires once");
        assert!(
            manager.get(&id).is_none(),
            "C1: the fired timer is removed — no cleanup tool needed"
        );
        assert!(manager.list().is_empty(), "the table is empty again");
    }

    /// Given a missing or non-positive delay, when SetTimer resolves, then
    /// a synthetic error answers and nothing registers.
    #[test]
    fn should_refuse_a_missing_or_non_positive_delay() {
        let manager = Arc::new(CronManager::new());
        let tool = SetTimerTool::new(Arc::clone(&manager), "test-session".to_string());
        for bad in [
            serde_json::json!({ "message": "m" }),
            serde_json::json!({ "delay_minutes": 0, "message": "m" }),
            serde_json::json!({ "delay_minutes": -5, "message": "m" }),
        ] {
            let execution = tool.resolve_execution(bad).expect("resolve");
            let ToolExecution::Error(error) = execution else {
                panic!("expected synthetic error execution");
            };
            assert!(
                error.message.contains("delay_minutes"),
                "the refusal names the parameter: {}",
                error.message
            );
        }
        assert!(manager.list().is_empty());
    }

    /// Given a missing message, when SetTimer resolves, then a synthetic
    /// error answers and nothing registers.
    #[test]
    fn should_refuse_a_missing_message() {
        let manager = Arc::new(CronManager::new());
        let tool = SetTimerTool::new(Arc::clone(&manager), "test-session".to_string());
        let execution = tool
            .resolve_execution(serde_json::json!({ "delay_minutes": 5 }))
            .expect("resolve");
        let ToolExecution::Error(error) = execution else {
            panic!("expected synthetic error execution");
        };
        assert!(error.message.contains("message"));
        assert!(manager.list().is_empty());
    }

    /// Given a disabled cron subsystem, when SetTimer runs, then the
    /// documented refusal answers before anything else.
    #[test]
    fn should_refuse_timers_when_scheduling_is_disabled() {
        let manager = Arc::new(CronManager::new());
        let tool = SetTimerTool::new(Arc::clone(&manager), "test-session".to_string())
            .with_scheduling_disabled(true);
        let execution = tool
            .resolve_execution(serde_json::json!({
                "delay_minutes": 10, "message": "m"
            }))
            .expect("resolve");
        let ToolExecution::Error(error) = execution else {
            panic!("expected synthetic error execution");
        };
        assert_eq!(error.message, "定时任务已禁用（MIRRICODE_DISABLE_CRON）");
        assert!(manager.list().is_empty());
    }

    /// Given a cron-lineage turn, when SetTimer runs, then the timer
    /// registers as a lineage one-shot (the capped set-next-timer-at-fire
    /// pattern) — A.6a/A.6b's surviving arm.
    #[tokio::test]
    async fn should_ride_the_lineage_one_shot_chain_when_set_from_a_cron_turn() {
        let manager = Arc::new(CronManager::new());
        assert_eq!(manager.lineage_one_shot_created(), 0);
        let tool = SetTimerTool::new(Arc::clone(&manager), "test-session".to_string());
        let execution = tool
            .resolve_execution(serde_json::json!({
                "delay_minutes": 5, "message": "again"
            }))
            .expect("resolve");
        let ToolExecution::Runnable { execute, .. } = execution else {
            panic!("expected runnable execution");
        };
        let result = execute(lineage_ctx()).await;
        assert!(!result.is_error, "{}", output_text(&result));
        let payload: serde_json::Value = serde_json::from_str(&output_text(&result)).unwrap();
        let task = manager
            .get(payload["id"].as_str().unwrap())
            .expect("the lineage timer registered");
        assert!(task.cron_lineage, "the lineage marker rides the timer");
        assert!(!task.recurring);
        assert_eq!(manager.lineage_one_shot_created(), 1);
    }

    /// Given an interactive turn, when SetTimer runs, then the plain add
    /// arm is used — no lineage marker, no count advance.
    #[tokio::test]
    async fn should_use_the_plain_add_arm_on_an_interactive_turn() {
        let manager = Arc::new(CronManager::new());
        let tool = SetTimerTool::new(Arc::clone(&manager), "test-session".to_string());
        let result = execute_tool(
            &tool,
            serde_json::json!({ "delay_minutes": 5, "message": "hi" }),
        )
        .await;
        assert!(!result.is_error);
        assert_eq!(manager.lineage_one_shot_created(), 0);
        let task = &manager.list()[0];
        assert!(!task.cron_lineage);
    }

    /// Given the timer expression builder, when a sub-minute delay is
    /// requested, then the target rounds UP to the next whole minute
    /// (still strictly in the future).
    #[test]
    fn should_round_a_sub_minute_delay_up_to_the_next_whole_minute() {
        let calendar = CronTimezone::Local.resolve();
        let before = chrono::Utc::now();
        let (expression, target) = timer_cron_expression(0, calendar);
        assert!(target > before, "the rounded target stays in the future");
        assert_eq!(target.timestamp_subsec_micros(), 0);
        let fields: Vec<&str> = expression.split_whitespace().collect();
        assert_eq!(fields.len(), 5);
        assert_eq!(fields[4], "*");
        let local = target.with_timezone(&calendar);
        assert_eq!(fields[0].parse::<u32>().unwrap(), local.minute());
        assert_eq!(fields[1].parse::<u32>().unwrap(), local.hour());
        assert_eq!(fields[2].parse::<u32>().unwrap(), local.day());
        assert_eq!(fields[3].parse::<u32>().unwrap(), local.month());
    }
}
