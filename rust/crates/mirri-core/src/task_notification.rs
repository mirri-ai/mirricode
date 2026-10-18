//! Background task terminal notifications (B2, D-51/D-58/D-66) — the model-
//! side closure of the notification loop.
//!
//! The TS engine's counterpart is `agent-core-v2/src/agent/task/taskService.ts`
//! plus `notificationXml.ts`: when a detached (background) task reaches a
//! terminal status, a synthetic user message carrying a `<notification>` XML
//! document is delivered to the main agent — folded into the running turn's
//! next step when busy, or run as its own turn when idle. This module owns the
//! pure rendering half: the notification body, the XML document, and the
//! identity key the delivery ledger dedups on. The delivery engine itself
//! lives in the embedded server's task driver; the ledger is scan-based
//! (SDR-B2-01): the context history is the single source of delivered truth.

use std::collections::HashSet;

use crate::context_memory::ContextMemory;
use crate::hook_engine::{HookEngine, HookEventType};
use crate::origin::PromptOrigin;
use crate::task::{
    AgentTask, TASK_TIMEOUT_MESSAGE, TaskKind, TaskManager, TaskOutputSnapshot, TaskStatus,
};
use crate::turn_flow::steering::SteerBuffer;

/// The fallback preview cap for notifications without a persisted output
/// log (TS `NOTIFICATION_FALLBACK_PREVIEW_BYTES`, taskService.ts:196).
pub const NOTIFICATION_FALLBACK_PREVIEW_BYTES: usize = 3_000;

/// The dismissal note — the single source of the user-closed wording shared
/// by the blocking tool result and the background cancellation notification
/// (SDR-005 (c) / ADR-007: the notification carries the SAME behavioral
/// instruction; one voice, no second wording).
pub const DISMISSED_NOTE: &str = "The user dismissed the question without providing an answer.";

/// The dismissal behavioral instruction — shared verbatim by the blocking
/// tool result and the background cancellation notification: stop dependent
/// decisions, do not assume an answer, do not re-ask the same question.
pub const DISMISSED_INSTRUCTION: &str = "\
Do not proceed on assumptions or guesses. The user closed this question \
without answering — end your current turn and wait for their next input. \
Do not re-ask the same question in this turn; if you still need an answer \
later, ask it again in a different, clearer way.";

/// The notification body — the TS `AgentTaskNotification` struct counterpart.
/// Rendered into the `<notification>` XML document the model reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskNotification {
    /// `task:{task_id}:{status}` — stable per task+terminal pair.
    pub id: String,
    /// Always `"task"`.
    pub category: &'static str,
    /// `task.completed` / `task.failed` / `task.killed` / `task.timed_out`.
    pub r#type: String,
    /// Always `"background_task"`.
    pub source_kind: &'static str,
    /// The task id.
    pub source_id: String,
    /// The subagent id for agent-kind tasks whose agent id differs from the
    /// task id (the resume path's key); absent otherwise.
    pub agent_id: Option<String>,
    /// `Background {kind} {status}`.
    pub title: String,
    /// `completed` → `"info"`, everything else → `"warning"`.
    pub severity: &'static str,
    /// The human-facing settlement line(s), including the agent recovery
    /// guidance block for failed agent tasks.
    pub body: String,
    /// The output blocks (`<output-file>` / `<output-preview>`), `None`
    /// when the task produced no output at all.
    pub children: Option<Vec<String>>,
}

/// The notification identity — the `PromptOrigin::BackgroundTask` triple
/// in a dedup-key shape (TS `TaskOrigin` / `notificationKey`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct NotificationKey {
    pub task_id: String,
    pub status: String,
    pub notification_id: String,
}

impl NotificationKey {
    /// Build the key for a task's terminal pair; the notification id is
    /// `task:{task_id}:{status}` (TS `buildAgentTaskNotificationContext`).
    pub fn new(task_id: &str, status: &str) -> Self {
        Self {
            task_id: task_id.to_string(),
            status: status.to_string(),
            notification_id: format!("task:{task_id}:{status}"),
        }
    }

    /// The wire origin JSON — the exact `{"kind":"background_task",…}` shape
    /// `PromptOrigin::BackgroundTask` serializes to (TS log-compatible).
    pub fn wire(&self) -> serde_json::Value {
        serde_json::to_value(self.origin())
            .unwrap_or_else(|_| serde_json::json!({ "kind": "background_task" }))
    }

    /// The prompt origin this key routes by.
    pub fn origin(&self) -> PromptOrigin {
        PromptOrigin::BackgroundTask {
            task_id: self.task_id.clone(),
            status: self.status.clone(),
            notification_id: self.notification_id.clone(),
        }
    }

    /// Parse a wire origin JSON back into a key — the TS `isTaskOrigin`
    /// caliber: `kind` is `background_task` (or the legacy `task`) and the
    /// three identity fields are strings. Anything else is `None`.
    ///
    /// Note on consumers: the production ledger scans
    /// ([`delivered_keys`]/[`parked_keys`]) read the typed
    /// [`PromptOrigin::BackgroundTask`] directly — a legacy `task`-kind
    /// origin never parses into that variant and the restore replay's
    /// warn-and-skip tolerates it. This constructor serves the wire-interop
    /// surface (round-tripping the shape external tooling or TS-written
    /// logs carry) and the test contracts; it is deliberately not on the
    /// hot delivery path.
    pub fn parse(origin: &serde_json::Value) -> Option<Self> {
        let kind = origin.get("kind")?.as_str()?;
        if kind != "background_task" && kind != "task" {
            return None;
        }
        Some(Self {
            task_id: origin.get("taskId")?.as_str()?.to_string(),
            status: origin.get("status")?.as_str()?.to_string(),
            notification_id: origin.get("notificationId")?.as_str()?.to_string(),
        })
    }
}

/// The task's kind word (`process` / `agent` / `question`) — the TS `info.kind`
/// discriminator the notification title carries.
fn kind_word(kind: &TaskKind) -> &'static str {
    match kind {
        TaskKind::Process { .. } => "process",
        TaskKind::Agent { .. } => "agent",
        TaskKind::Question { .. } => "question",
    }
}

/// The terminal status word in the wire vocabulary: `completed` / `failed` /
/// `killed` (manager-stopped). A timeout termination is `Failed` carrying
/// [`TASK_TIMEOUT_MESSAGE`], which maps to the TS `timed_out` word so the
/// body's timeout wording and the `task.timed_out` type stay reachable.
pub fn task_status_word(status: &TaskStatus) -> &'static str {
    match status {
        TaskStatus::Running => "running",
        TaskStatus::Completed { .. } => "completed",
        TaskStatus::Failed(reason) if reason == TASK_TIMEOUT_MESSAGE => "timed_out",
        TaskStatus::Failed(_) => "failed",
        TaskStatus::Stopped => "killed",
    }
}

/// Escape a string for embedding in XML text (body lines) — the document
/// must stay balanced when a description carries markup.
fn escape_text(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Escape a string for an XML attribute value (TS `escapeXmlAttr`).
fn escape_attr(value: &str) -> String {
    escape_text(value)
}

/// Render the `<notification>` XML document (TS `renderNotificationXml`,
/// notificationXml.ts, ported verbatim in structure): the opening tag
/// carries the identity attributes, `Title:` / `Severity:` lines, the body,
/// any children, and the closing tag. Body and children are text-escaped —
/// the document must parse even when a description carries markup.
pub fn render_notification_xml(notification: &TaskNotification) -> String {
    let agent_id_attr = notification
        .agent_id
        .as_deref()
        .filter(|id| !id.is_empty())
        .map(|id| format!(" agent_id=\"{}\"", escape_attr(id)))
        .unwrap_or_default();
    let mut lines = vec![format!(
        "<notification id=\"{}\" category=\"{}\" type=\"{}\" source_kind=\"{}\" source_id=\"{}\"{}>",
        escape_attr(&notification.id),
        escape_attr(notification.category),
        escape_attr(&notification.r#type),
        escape_attr(notification.source_kind),
        escape_attr(&notification.source_id),
        agent_id_attr,
    )];
    if !notification.title.is_empty() {
        lines.push(format!("Title: {}", notification.title));
    }
    if !notification.severity.is_empty() {
        lines.push(format!("Severity: {}", notification.severity));
    }
    if !notification.body.is_empty() {
        lines.push(escape_text(&notification.body));
    }
    if let Some(children) = &notification.children {
        lines.extend(children.iter().cloned());
    }
    lines.push("</notification>".to_string());
    lines.join("\n")
}

/// Build the notification body (TS `buildAgentTaskNotificationBody`): the
/// settlement line — `timed out` for timeout failures, `was stopped` for
/// manager stops, `failed. Reason: …` for failures with a reason, plain
/// `completed.` otherwise — plus, for failed agent-kind tasks with a
/// distinguishable agent id, the recovery block teaching the resume path.
///
/// A dismissed background question (a `Question` task stopped by the user
/// closing the panel — SDR-005 (c)) gets the dismissal voice: the note and
/// the behavioral instruction ride the body itself so the model reads them
/// even when the children render only an `<output-file>` pointer.
pub fn build_notification_body(task: &AgentTask) -> String {
    let base_line = match &task.status {
        TaskStatus::Failed(reason) if reason == TASK_TIMEOUT_MESSAGE => {
            format!("{} timed out.", task.description)
        }
        TaskStatus::Stopped if matches!(task.kind, TaskKind::Question { .. }) => format!(
            "{} was cancelled. {DISMISSED_NOTE}\n{DISMISSED_INSTRUCTION}",
            task.description
        ),
        TaskStatus::Stopped => format!("{} was stopped.", task.description),
        TaskStatus::Failed(reason) => {
            format!("{} failed. Reason: {}", task.description, reason)
        }
        TaskStatus::Completed { .. } => format!("{} completed.", task.description),
        TaskStatus::Running => format!("{} running.", task.description),
    };
    // The recovery block applies only to agent-kind failures with a
    // distinguishable agent id (TS: kind === 'agent', status !==
    // 'completed', agentId !== taskId).
    let TaskKind::Agent {
        agent_id: Some(agent_id),
        ..
    } = &task.kind
    else {
        return base_line;
    };
    if matches!(task.status, TaskStatus::Completed { .. }) || agent_id == &task.id {
        return base_line;
    }
    let recovery = [
        String::new(),
        format!(
            "To recover or continue this subagent, call Agent(resume=\"{agent_id}\", \
             prompt=\"Pick up where you left off; redo the last tool call if its \
             result was never observed.\")."
        ),
        format!(
            "Use agent_id (\"{agent_id}\"), NOT source_id / task_id \
             (\"{}\") — the two look alike but only agent_id is accepted by \
             the resume parameter.",
            task.id
        ),
        "Add run_in_background=true to keep it backgrounded, or omit it to \
         take the result inline in the current turn."
            .to_string(),
        "The subagent retains its full prior context across the restart, but \
         any in-flight tool call lost its result and may need to be redone."
            .to_string(),
    ];
    format!("{base_line}\n{}", recovery.join("\n"))
}

/// Build the notification children blocks (TS
/// `agentTaskNotificationChildren`): the persisted-log `<output-file>` block
/// when the snapshot carries a readable log; otherwise a tail-clamped
/// `<output-preview>` block (the fallback cap); `None` when there is no
/// output at all.
pub fn build_notification_children(
    _task_id: &str,
    snapshot: Option<&TaskOutputSnapshot>,
) -> Option<Vec<String>> {
    let snapshot = snapshot?;
    // The persisted log is the authoritative, never-truncated source.
    if snapshot.full_output_available
        && let Some(path) = snapshot.output_path.as_ref()
    {
        let path_display = path.display();
        return Some(vec![format!(
            "<output-file path=\"{}\" bytes=\"{}\">\n\
             Read the output file to retrieve the result: {}\n\
             </output-file>",
            escape_attr(&path_display.to_string()),
            snapshot.output_size_bytes,
            escape_text(&path_display.to_string())
        )]);
    }
    // No persisted log: a preview exists only when the task produced
    // output at all.
    if snapshot.preview.is_empty() {
        return None;
    }
    // Clamp the preview to the fallback cap (tail-first: the settlement
    // tail is what the model needs; TS fetches the snapshot with exactly
    // this cap when no log exists).
    let preview = &snapshot.preview;
    let (clamped, bytes) = if preview.len() > NOTIFICATION_FALLBACK_PREVIEW_BYTES {
        let start = preview.len() - NOTIFICATION_FALLBACK_PREVIEW_BYTES;
        (&preview[start..], NOTIFICATION_FALLBACK_PREVIEW_BYTES)
    } else {
        (preview.as_str(), preview.len())
    };
    let hint = if snapshot.truncated {
        format!("Showing the last {bytes} bytes. No persisted full output is available.")
    } else {
        "No persisted full output is available; this preview is the \
         currently buffered task output."
            .to_string()
    };
    Some(vec![format!(
        "<output-preview bytes=\"{}\" total_bytes=\"{}\" truncated=\"{}\">\n{}\n{}\n</output-preview>",
        bytes,
        snapshot.output_size_bytes,
        snapshot.truncated,
        hint,
        escape_text(clamped)
    )])
}

/// Assemble the full notification for one terminal task (TS
/// `buildAgentTaskNotificationContext`): identity, title, severity, body,
/// and children from the task's output snapshot.
pub fn build_task_notification(
    task: &AgentTask,
    snapshot: Option<&TaskOutputSnapshot>,
) -> TaskNotification {
    let status_word = task_status_word(&task.status);
    let agent_id = match &task.kind {
        TaskKind::Agent { agent_id, .. } => agent_id.clone().filter(|id| id != &task.id),
        _ => None,
    };
    TaskNotification {
        id: format!("task:{}:{}", task.id, status_word),
        category: "task",
        r#type: format!("task.{status_word}"),
        source_kind: "background_task",
        source_id: task.id.clone(),
        agent_id,
        title: format!("Background {} {}", kind_word(&task.kind), status_word),
        severity: if status_word == "completed" {
            "info"
        } else {
            "warning"
        },
        body: build_notification_body(task),
        children: build_notification_children(&task.id, snapshot),
    }
}

/// One adjudicated delivery: the task snapshot, its dedup key, and the
/// rendered XML the delivery engine injects (either as a parked steer's
/// content or as a fresh turn's prompt).
#[derive(Debug, Clone)]
pub struct NotificationDue {
    pub task: AgentTask,
    pub key: NotificationKey,
    pub xml: String,
    /// The structured notification — the external `Notification` hook's input
    /// (fired once per delivery, TS parity: the `task.notified` event).
    pub notification: TaskNotification,
}

/// The scan-based delivered-keys ledger (SDR-B2-01): every background-task
/// origin in the context history counts as delivered. The context is the
/// single source of truth — an undo that removes the message removes the
/// key, and the driver re-delivers (TS `reconcileAfterUndo` for free).
pub fn delivered_keys(memory: &ContextMemory) -> HashSet<NotificationKey> {
    memory
        .history()
        .iter()
        .filter_map(|message| message.origin.as_ref())
        .filter_map(|origin| match origin {
            PromptOrigin::BackgroundTask {
                task_id,
                status,
                notification_id,
            } => Some(NotificationKey {
                task_id: task_id.clone(),
                status: status.clone(),
                notification_id: notification_id.clone(),
            }),
            _ => None,
        })
        .collect()
}

/// The parked-keys half of the ledger: a notification parked in the steer
/// buffer (busy-path fold) counts as scheduled, not undelivered.
pub fn parked_keys(buffer: &SteerBuffer) -> HashSet<NotificationKey> {
    buffer
        .iter()
        .filter_map(|steer| match &steer.origin {
            PromptOrigin::BackgroundTask {
                task_id,
                status,
                notification_id,
            } => Some(NotificationKey {
                task_id: task_id.clone(),
                status: status.clone(),
                notification_id: notification_id.clone(),
            }),
            _ => None,
        })
        .collect()
}

/// The eligibility gate shared by the driver tick and restore-time
/// reconciliation: terminal, detached (background-registered), unsuppressed.
pub fn is_notification_candidate(task: &AgentTask) -> bool {
    task.status.is_terminal() && task.detached && !task.notification_suppressed
}

/// The delivery adjudication core: every task that is terminal, detached
/// (background-registered), unsuppressed, not already durably delivered
/// (the persisted SDR-B2-01 marker), and whose key is in neither the
/// delivered set (context scan) nor the parked set (steer-buffer scan) —
/// restricted to the tasks `keep` admits — each with its output snapshot
/// folded into the rendered XML (TS `buildAgentTaskNotificationContext`'s
/// snapshot dance: probe with a zero window first, re-fetch with the
/// fallback cap when no log exists).
fn due_notifications_filtered(
    tasks: &TaskManager,
    delivered: &HashSet<NotificationKey>,
    parked: &HashSet<NotificationKey>,
    keep: impl Fn(&AgentTask) -> bool,
) -> Vec<NotificationDue> {
    let mut due = Vec::new();
    for task in tasks.list() {
        if !keep(&task)
            || !is_notification_candidate(&task)
            || task.notification_delivered_at.is_some()
        {
            continue;
        }
        let status_word = task_status_word(&task.status);
        let key = NotificationKey::new(&task.id, status_word);
        if delivered.contains(&key) || parked.contains(&key) {
            continue;
        }
        let snapshot = match tasks.output_snapshot(&task.id, 0) {
            // The zero-window probe with a persisted log: the file block
            // needs no inline preview — keep it as-is.
            Some(snapshot) if snapshot.full_output_available => Some(snapshot),
            // No log (or no persistence): re-fetch with the fallback cap
            // so the preview block carries the settlement tail.
            _ => tasks.output_snapshot(&task.id, NOTIFICATION_FALLBACK_PREVIEW_BYTES),
        };
        let notification = build_task_notification(&task, snapshot.as_ref());
        due.push(NotificationDue {
            task,
            key,
            xml: render_notification_xml(&notification),
            notification,
        });
    }
    due
}

/// The full-session delivery adjudication (driver tick): every due task of
/// the session, no addressee restriction — the un-filtered form of
/// [`due_notifications_for`].
pub fn due_notifications(
    tasks: &TaskManager,
    delivered: &HashSet<NotificationKey>,
    parked: &HashSet<NotificationKey>,
) -> Vec<NotificationDue> {
    due_notifications_filtered(tasks, delivered, parked, |_| true)
}

/// The delivery adjudication restricted to one addressee's task ids
/// (restore-time reconciliation, B5): the resume per-addressee pass needs
/// the same eligibility/dedup rules as the driver tick, but only over the
/// tasks grouped under the addressee being reconciled. The task-id set is
/// the index because the adjudication needs the `TaskManager` for the
/// output-snapshot fold.
pub fn due_notifications_for(
    tasks: &TaskManager,
    task_ids: &HashSet<String>,
    delivered: &HashSet<NotificationKey>,
    parked: &HashSet<NotificationKey>,
) -> Vec<NotificationDue> {
    due_notifications_filtered(tasks, delivered, parked, |task| task_ids.contains(&task.id))
}

/// Opportunistic marker sweep (review M1): a task whose notification is
/// present in an addressee's live context (the busy-path flush lands the
/// message in memory without a marker — park no longer marks, so a
/// flush-window crash self-heals via the restore re-append) gets its
/// durable delivery marker set here, within one driver tick, so a later
/// compaction or context rebuild can never re-arm the debt. Idempotent:
/// already-marked tasks are skipped, so the persist happens once.
pub fn sweep_mark_delivered(tasks: &TaskManager, delivered: &HashSet<NotificationKey>) -> usize {
    let mut marked = 0usize;
    for task in tasks.list() {
        if !is_notification_candidate(&task) || task.notification_delivered_at.is_some() {
            continue;
        }
        let key = NotificationKey::new(&task.id, task_status_word(&task.status));
        if delivered.contains(&key) {
            tasks.mark_notification_delivered(&task.id);
            marked += 1;
        }
    }
    marked
}

/// Fire the `Notification` external hook once for a due notification (TS
/// parity: v2's `task.notified` event → `notifyTaskNotification`). The input
/// carries the TS `AgentTaskNotificationContext` shape plus `sink:
/// "context"`; the merged result is discarded — the hook observes the
/// delivery and can never alter it. Returns early when no `Notification`
/// hooks are registered.
pub async fn fire_notification_hook(
    engine: &HookEngine,
    env: &std::collections::HashMap<String, String>,
    notification: &TaskNotification,
) {
    if engine.list_by_event(HookEventType::Notification).is_empty() {
        return;
    }
    let input = serde_json::json!({
        "sink": "context",
        "notification_type": notification.r#type,
        "title": notification.title,
        "body": notification.body,
        "severity": notification.severity,
        "source_kind": notification.source_kind,
        "source_id": notification.source_id,
    });
    let _ = engine
        .trigger(HookEventType::Notification, input, env)
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::ContentPart;
    use chrono::TimeZone;

    fn task(kind: TaskKind, status: TaskStatus) -> AgentTask {
        AgentTask {
            id: "process-abc12345".into(),
            description: "npm run watch".into(),
            kind,
            status,
            created_at: chrono::Utc.with_ymd_and_hms(2025, 1, 15, 10, 0, 0).unwrap(),
            ended_at: Some(chrono::Utc.with_ymd_and_hms(2025, 1, 15, 10, 0, 5).unwrap()),
            output: String::new(),
            session_id: "sess".into(),
            deadline: None,
            detached: true,
            notification_suppressed: false,
            notification_delivered_at: None,
            fingerprint: None,
            lifetime_exempt: false,
        }
    }

    #[test]
    fn should_render_the_notification_xml_with_all_fields_when_completed() {
        let t = task(
            TaskKind::Process {
                command: Some("npm run watch".into()),
                dispatcher: None,
            },
            TaskStatus::Completed { exit_code: 0 },
        );
        let n = build_task_notification(&t, None);
        let xml = render_notification_xml(&n);
        // Then the opening tag carries the identity, exactly the TS shape
        assert!(
            xml.starts_with(
                "<notification id=\"task:process-abc12345:completed\" \
             category=\"task\" type=\"task.completed\" \
             source_kind=\"background_task\" source_id=\"process-abc12345\">"
            ),
            "{xml}"
        );
        assert!(xml.contains("Title: Background process completed"));
        assert!(xml.contains("Severity: info"));
        assert!(xml.contains("npm run watch completed."));
        assert!(xml.ends_with("</notification>"));
    }

    #[test]
    fn should_escape_attributes_and_body_when_rendering_the_notification_xml() {
        let mut t = task(
            TaskKind::Process {
                command: None,
                dispatcher: None,
            },
            TaskStatus::Completed { exit_code: 0 },
        );
        t.description = "build <all> & \"ship\"".into();
        let xml = render_notification_xml(&build_task_notification(&t, None));
        // Then the body text is XML-escaped and the document stays balanced
        assert!(
            xml.contains("&lt;all&gt; &amp; &quot;ship&quot;"),
            "body must be escaped: {xml}"
        );
        assert!(xml.ends_with("</notification>"));
    }

    #[test]
    fn should_warn_when_the_task_failed_or_was_killed() {
        let t = task(
            TaskKind::Process {
                command: None,
                dispatcher: None,
            },
            TaskStatus::Failed("exit 2".into()),
        );
        let n = build_task_notification(&t, None);
        assert_eq!(n.severity, "warning");
        assert!(render_notification_xml(&n).contains("Severity: warning"));
        let t = task(
            TaskKind::Process {
                command: None,
                dispatcher: None,
            },
            TaskStatus::Stopped,
        );
        let n = build_task_notification(&t, None);
        assert_eq!(n.r#type, "task.killed");
        assert!(n.body.contains("was stopped"));
    }

    #[test]
    fn should_speak_the_timeout_wording_when_the_failure_is_the_timeout_message() {
        // A timeout termination is Failed(TASK_TIMEOUT_MESSAGE) — the body
        // and the type must both carry the TS timed_out vocabulary.
        let t = task(
            TaskKind::Process {
                command: None,
                dispatcher: None,
            },
            TaskStatus::Failed(TASK_TIMEOUT_MESSAGE.into()),
        );
        let n = build_task_notification(&t, None);
        assert_eq!(n.r#type, "task.timed_out");
        assert!(n.body.contains("timed out."), "{}", n.body);
        assert_eq!(
            task_status_word(&TaskStatus::Failed(TASK_TIMEOUT_MESSAGE.into())),
            "timed_out"
        );
    }

    #[test]
    fn should_append_the_resume_guidance_when_an_agent_task_failed() {
        let mut t = task(
            TaskKind::Agent {
                agent_id: Some("agent-777".into()),
                subagent_type: Some("coder".into()),
                parent_tool_call_id: None,
                swarm_index: None,
                model: None,
            },
            TaskStatus::Failed("tool error".into()),
        );
        t.id = "task-88888888".into();
        let n = build_task_notification(&t, None);
        // Then the body teaches the resume path, distinguishing agent_id from task_id
        assert!(n.body.contains("Agent(resume=\"agent-777\""), "{}", n.body);
        assert!(
            n.body
                .contains("agent_id (\"agent-777\"), NOT source_id / task_id (\"task-88888888\")"),
            "{}",
            n.body
        );
        assert_eq!(n.agent_id.as_deref(), Some("agent-777"));
        // …while a completed agent task carries no recovery block.
        let ok = build_task_notification(
            &task(
                TaskKind::Agent {
                    agent_id: Some("agent-777".into()),
                    subagent_type: None,
                    parent_tool_call_id: None,
                    swarm_index: None,
                    model: None,
                },
                TaskStatus::Completed { exit_code: 0 },
            ),
            None,
        );
        assert!(!ok.body.contains("Agent(resume="));
        // The agent_id attribute still rides along (TS: kind === 'agent'
        // ? agentId) — only the recovery block is completion-gated.
        assert_eq!(ok.agent_id.as_deref(), Some("agent-777"));
    }

    #[test]
    fn should_render_the_output_file_block_when_a_persisted_log_exists() {
        let t = task(
            TaskKind::Process {
                command: None,
                dispatcher: None,
            },
            TaskStatus::Completed { exit_code: 0 },
        );
        let snapshot = TaskOutputSnapshot {
            output_path: Some("/tmp/sess/tasks/process-abc12345/output.log".into()),
            output_size_bytes: 42_000,
            truncated: false,
            full_output_available: true,
            preview: String::new(),
        };
        let children = build_notification_children(&t.id, Some(&snapshot)).unwrap();
        assert_eq!(children.len(), 1);
        assert!(
            children[0].starts_with(
                "<output-file path=\"/tmp/sess/tasks/process-abc12345/output.log\" bytes=\"42000\">"
            ),
            "{}",
            children[0]
        );
        assert!(children[0].contains("Read the output file to retrieve the result"));
    }

    #[test]
    fn should_render_a_truncated_preview_block_when_no_persisted_log_exists() {
        let t = task(
            TaskKind::Process {
                command: None,
                dispatcher: None,
            },
            TaskStatus::Completed { exit_code: 0 },
        );
        let long = "x".repeat(NOTIFICATION_FALLBACK_PREVIEW_BYTES + 500);
        let total_bytes = long.len() as u64;
        let snapshot = TaskOutputSnapshot {
            output_path: None,
            output_size_bytes: total_bytes,
            truncated: true,
            full_output_available: false,
            preview: long,
        };
        let children = build_notification_children(&t.id, Some(&snapshot)).unwrap();
        assert!(
            children[0].starts_with(&format!(
                "<output-preview bytes=\"{}\" total_bytes=\"{}\" truncated=\"true\">",
                NOTIFICATION_FALLBACK_PREVIEW_BYTES, total_bytes
            )),
            "{}",
            children[0]
        );
        // …the preview itself is clamped to the fallback cap.
        assert!(children[0].matches('x').count() <= NOTIFICATION_FALLBACK_PREVIEW_BYTES);
        assert!(children[0].contains("Showing the last 3000 bytes."));
    }

    #[test]
    fn should_omit_children_when_there_is_no_output_at_all() {
        let t = task(
            TaskKind::Question { question_count: 1 },
            TaskStatus::Completed { exit_code: 0 },
        );
        assert!(build_notification_children(&t.id, None).is_none());
        assert!(
            build_task_notification(&t, None)
                .body
                .contains("npm run watch completed.")
        );
    }

    #[test]
    fn should_map_status_words_for_every_terminal_variant() {
        assert_eq!(
            task_status_word(&TaskStatus::Completed { exit_code: 0 }),
            "completed"
        );
        assert_eq!(task_status_word(&TaskStatus::Failed("x".into())), "failed");
        assert_eq!(task_status_word(&TaskStatus::Stopped), "killed");
    }

    /// Given a background question task stopped by a user dismissal, when
    /// the notification body is built, then it carries the dismissal note
    /// and the SAME behavioral instruction the blocking tool result feeds
    /// the model — one voice (SDR-005 (c) / ADR-007 background variant).
    #[test]
    fn should_carry_the_dismissal_voice_when_a_background_question_is_cancelled() {
        let t = task(
            TaskKind::Question { question_count: 1 },
            TaskStatus::Stopped,
        );
        let n = build_task_notification(&t, None);
        assert_eq!(n.r#type, "task.killed");
        let body = n.body.to_lowercase();
        assert!(body.contains("was cancelled"), "{}", n.body);
        assert!(body.contains("dismissed"), "{}", n.body);
        assert!(body.contains("do not proceed on assumptions"), "{}", n.body);
        assert!(body.contains("wait for their next input"), "{}", n.body);
        // The body reuses the same constants the tool result renders — the
        // wording cannot drift into a second voice.
        assert!(n.body.contains(DISMISSED_NOTE), "{}", n.body);
        assert!(n.body.contains(DISMISSED_INSTRUCTION), "{}", n.body);
    }

    #[test]
    fn should_round_trip_a_key_through_the_wire_origin() {
        let key = NotificationKey::new("process-abc12345", "completed");
        assert_eq!(key.notification_id, "task:process-abc12345:completed");
        let parsed = NotificationKey::parse(&key.wire()).unwrap();
        assert_eq!(parsed.notification_id, key.notification_id);
        assert_eq!(parsed, key);
        // Non-notification origins do not parse.
        assert!(NotificationKey::parse(&serde_json::json!({ "kind": "user" })).is_none());
        // The legacy TS `task` kind parses too (isTaskOrigin caliber).
        assert!(
            NotificationKey::parse(&serde_json::json!({
                "kind": "task", "taskId": "t", "status": "completed",
                "notificationId": "task:t:completed",
            }))
            .is_some()
        );
    }

    #[test]
    fn should_title_the_question_kind_notification_for_the_answer_delivery() {
        // D-58: a background question's notification names the question kind
        // so the model reads the answer block in the right frame.
        let t = task(
            TaskKind::Question { question_count: 2 },
            TaskStatus::Completed { exit_code: 0 },
        );
        let n = build_task_notification(&t, None);
        assert_eq!(n.title, "Background question completed");
        assert_eq!(n.r#type, "task.completed");
    }

    // -----------------------------------------------------------------------
    // Delivery adjudication (the scan-based ledger's pure core, SDR-B2-01)
    // -----------------------------------------------------------------------

    #[test]
    fn should_return_only_terminal_detached_unsuppressed_tasks_not_yet_delivered() {
        let tasks = TaskManager::new();
        let due = tasks
            .register_background(
                TaskKind::Process {
                    command: None,
                    dispatcher: None,
                },
                "due".into(),
                "s".into(),
                None,
            )
            .unwrap();
        tasks.complete(&due, 0);
        let delivered_one = tasks
            .register_background(
                TaskKind::Process {
                    command: None,
                    dispatcher: None,
                },
                "delivered".into(),
                "s".into(),
                None,
            )
            .unwrap();
        tasks.complete(&delivered_one, 0);
        let fg = tasks.register(
            TaskKind::Process {
                command: None,
                dispatcher: None,
            },
            "fg".into(),
            "s".into(),
        );
        tasks.complete(&fg, 0);
        let suppressed = tasks
            .register_background(
                TaskKind::Process {
                    command: None,
                    dispatcher: None,
                },
                "sup".into(),
                "s".into(),
                None,
            )
            .unwrap();
        tasks.complete(&suppressed, 0);
        tasks.suppress_terminal_notification(&suppressed);
        let parked_one = tasks
            .register_background(
                TaskKind::Process {
                    command: None,
                    dispatcher: None,
                },
                "parked".into(),
                "s".into(),
                None,
            )
            .unwrap();
        tasks.complete(&parked_one, 0);
        let still_running = tasks
            .register_background(
                TaskKind::Process {
                    command: None,
                    dispatcher: None,
                },
                "running".into(),
                "s".into(),
                None,
            )
            .unwrap();

        let mut delivered = HashSet::new();
        delivered.insert(NotificationKey::new(&delivered_one, "completed"));
        let mut parked = HashSet::new();
        parked.insert(NotificationKey::new(&parked_one, "completed"));

        let due_list = due_notifications(&tasks, &delivered, &parked);
        assert_eq!(
            due_list.len(),
            1,
            "only the undelivered, unparked, detached, unsuppressed terminal task"
        );
        assert_eq!(due_list[0].task.id, due);
        assert!(due_list[0].xml.starts_with("<notification "));
        assert_eq!(due_list[0].key.task_id, due);
        // The still-running task never appeared.
        assert!(due_list.iter().all(|d| d.task.id != still_running));
    }

    #[test]
    fn should_adjudicate_only_the_task_ids_of_one_addressee() {
        let tasks = TaskManager::new();
        let owned = tasks
            .register_background(
                TaskKind::Process {
                    command: None,
                    dispatcher: None,
                },
                "owned".into(),
                "s".into(),
                None,
            )
            .unwrap();
        tasks.complete(&owned, 0);
        let foreign = tasks
            .register_background(
                TaskKind::Process {
                    command: None,
                    dispatcher: None,
                },
                "foreign".into(),
                "s".into(),
                None,
            )
            .unwrap();
        tasks.complete(&foreign, 0);

        let delivered = HashSet::new();
        let parked = HashSet::new();
        // The addressee-restricted form yields only the owned task…
        let owned_due =
            due_notifications_for(&tasks, &HashSet::from([owned.clone()]), &delivered, &parked);
        assert_eq!(owned_due.len(), 1, "only the addressee's own task");
        assert_eq!(owned_due[0].task.id, owned);
        assert!(owned_due[0].xml.starts_with("<notification "));
        // …and an empty restriction set yields nothing, even though the
        // un-filtered form still sees the foreign task.
        assert!(due_notifications_for(&tasks, &HashSet::new(), &delivered, &parked).is_empty());
        assert_eq!(due_notifications(&tasks, &delivered, &parked).len(), 2);
        // Marking the owned task delivered removes it from its addressee's
        // due set (the shared adjudication core applies every gate).
        tasks.mark_notification_delivered(&owned);
        assert!(
            due_notifications_for(&tasks, &HashSet::from([owned.clone()]), &delivered, &parked)
                .is_empty(),
            "the persisted delivery marker suppresses the restricted adjudication too"
        );
    }

    #[test]
    fn should_scan_the_context_for_delivered_keys() {
        let mut memory = ContextMemory::new();
        memory.append_user_message(
            vec![ContentPart::Text {
                text: "<notification id=\"task:t1:completed\"></notification>".into(),
            }],
            PromptOrigin::BackgroundTask {
                task_id: "t1".into(),
                status: "completed".into(),
                notification_id: "task:t1:completed".into(),
            },
        );
        memory.append_user_message(
            vec![ContentPart::Text {
                text: "a user message".into(),
            }],
            PromptOrigin::User,
        );
        let keys = delivered_keys(&memory);
        assert!(keys.contains(&NotificationKey::new("t1", "completed")));
        // A different status of the same task is NOT delivered (the key is
        // the task+status pair)…
        assert!(!keys.contains(&NotificationKey::new("t1", "failed")));
        // …and a plain user message contributes nothing.
        assert_eq!(keys.len(), 1);
    }

    #[test]
    fn should_scan_the_steer_buffer_for_parked_keys() {
        let mut buf = SteerBuffer::new();
        buf.push(
            vec![ContentPart::Text {
                text: "user steer".into(),
            }],
            PromptOrigin::User,
        );
        buf.push(
            vec![ContentPart::Text {
                text: "<notification/>".into(),
            }],
            PromptOrigin::BackgroundTask {
                task_id: "t9".into(),
                status: "failed".into(),
                notification_id: "task:t9:failed".into(),
            },
        );
        let keys = parked_keys(&buf);
        assert_eq!(keys, HashSet::from([NotificationKey::new("t9", "failed")]));
    }

    #[test]
    fn should_not_deliver_a_task_whose_notification_marker_is_persisted_when_the_context_ledger_misses_it()
     {
        let tasks = TaskManager::new();
        let marked = tasks
            .register_background(
                TaskKind::Process {
                    command: None,
                    dispatcher: None,
                },
                "marked".into(),
                "s".into(),
                None,
            )
            .unwrap();
        tasks.complete(&marked, 0);
        tasks.mark_notification_delivered(&marked);
        let delivered = HashSet::new();
        let parked = HashSet::new();
        let due_list = due_notifications(&tasks, &delivered, &parked);
        assert!(
            due_list.is_empty(),
            "the persisted delivery marker must suppress adjudication even \
             when the context ledger misses the key"
        );
    }

    #[test]
    fn should_deliver_a_task_whose_notification_marker_was_cleared_when_undo_removed_the_message() {
        let tasks = TaskManager::new();
        let cleared = tasks
            .register_background(
                TaskKind::Process {
                    command: None,
                    dispatcher: None,
                },
                "cleared".into(),
                "s".into(),
                None,
            )
            .unwrap();
        tasks.complete(&cleared, 0);
        tasks.mark_notification_delivered(&cleared);
        tasks.clear_notification_delivered(&cleared);
        let delivered = HashSet::new();
        let parked = HashSet::new();
        let due_list = due_notifications(&tasks, &delivered, &parked);
        assert_eq!(
            due_list.len(),
            1,
            "clearing the marker (undo) must re-arm delivery"
        );
        assert_eq!(due_list[0].task.id, cleared);
    }

    #[test]
    fn should_sweep_mark_tasks_whose_notification_is_in_the_live_context() {
        let tasks = TaskManager::new();
        let in_context = tasks
            .register_background(
                TaskKind::Process {
                    command: None,
                    dispatcher: None,
                },
                "in-context".into(),
                "s".into(),
                None,
            )
            .unwrap();
        tasks.complete(&in_context, 0);
        let missing = tasks
            .register_background(
                TaskKind::Process {
                    command: None,
                    dispatcher: None,
                },
                "missing".into(),
                "s".into(),
                None,
            )
            .unwrap();
        tasks.complete(&missing, 0);
        let already_marked = tasks
            .register_background(
                TaskKind::Process {
                    command: None,
                    dispatcher: None,
                },
                "already".into(),
                "s".into(),
                None,
            )
            .unwrap();
        tasks.complete(&already_marked, 0);
        tasks.mark_notification_delivered(&already_marked);

        let mut delivered = HashSet::new();
        delivered.insert(NotificationKey::new(&in_context, "completed"));

        // When the sweep runs against the live-context delivered set
        let marked = sweep_mark_delivered(&tasks, &delivered);
        // Then only the in-context task gets its durable marker…
        assert_eq!(marked, 1, "exactly the in-context task is newly marked");
        assert!(
            tasks
                .get(&in_context)
                .unwrap()
                .notification_delivered_at
                .is_some(),
            "the in-context task now carries the durable marker"
        );
        // …a task whose key is absent from the context stays unmarked…
        assert!(
            tasks
                .get(&missing)
                .unwrap()
                .notification_delivered_at
                .is_none(),
            "a task whose key is absent from the context stays unmarked"
        );
        // …and an already-marked task is untouched (and never re-counted).
        assert!(
            tasks
                .get(&already_marked)
                .unwrap()
                .notification_delivered_at
                .is_some(),
            "an already-marked task keeps its marker"
        );
        assert_eq!(
            sweep_mark_delivered(&tasks, &delivered),
            0,
            "the sweep is idempotent: a second pass marks nothing new"
        );
    }
}
