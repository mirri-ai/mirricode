//! The silence-warning render half of B-ADR-16 ① (BH8).
//!
//! The terminal-notification machinery owns the *terminal* half of the
//! background-task conversation loop; this module owns the *mid-flight*
//! half: a process-kind background task that has gone silent for longer
//! than the session's `silenceWarnSecs` threshold gets a synthetic user
//! message injected into the conversation — task id, description, silence
//! duration, a tail summary of the output, action guidance, and the
//! re-arm horizon when a second deadline exists.
//!
//! The delivery engine (candidate selection, episode dedup, busy-parking,
//! idle-turn injection) lives in the embedded server's task driver, on the
//! same seam the terminal notifications ride. This module owns only the
//! pure judgment + rendering: given a task, a silence anchor, and a
//! threshold, is the warning due, and what does it say.

use std::time::Duration;

use crate::task::{tail_bytes, AgentTask, TaskKind, TaskManager};

/// The tail clamp for the warning's output summary (1–2 KB family, the
/// brief's per-task tail: small enough to not flood the turn, large
/// enough to show where the process stopped talking).
pub const SILENCE_WARN_TAIL_BYTES: usize = 2_048;

/// Whether one task qualifies for a silence warning (B-ADR-16 ①):
/// process-kind, non-terminal, past the threshold since its last output.
///
/// Terminal tasks never warn (`status.is_terminal()` first); agent- and
/// question-kind tasks are out of scope (their liveness belongs to the
/// heartbeat/staleness machinery, not the output-silence machinery).
/// `last_output_at = None` means "no output observed yet" — the task is
/// silent since start, so the anchor is the first-scan instant, which
/// the caller supplies (`silent_since`); the driver seeds its snapshot
/// map on the first tick that observes the task.
pub fn is_silence_warn_due(
    task: &AgentTask,
    silent_since: Option<std::time::Instant>,
    threshold: Duration,
    now: std::time::Instant,
) -> bool {
    if task.status.is_terminal() {
        return false;
    }
    if !matches!(task.kind, TaskKind::Process { .. }) {
        return false;
    }
    let Some(anchor) = silent_since else {
        return false;
    };
    now.duration_since(anchor) >= threshold
}

/// One due silence warning: the task snapshot, its dedup key (the task
/// id — episode-scoped, see the driver), and the rendered message.
#[derive(Debug, Clone)]
pub struct SilenceWarning {
    pub task_id: String,
    /// The rendered synthetic user message (plain text, XML-structured).
    pub message: String,
}

/// Render the silence-warning message (B-ADR-16 ①): task id +
/// description + silence duration + tail output summary + action
/// guidance + the re-arm remaining duration when the task carries a
/// second deadline. The message is a synthetic USER message (the model is
/// the audience — it must decide whether to check, stop, or surface to
/// the user), so it reads as guidance, not as a terminal settlement.
pub fn render_silence_warning(
    tasks: &TaskManager,
    task: &AgentTask,
    silent_for: Duration,
) -> SilenceWarning {
    let mut lines = Vec::new();
    let command = match &task.kind {
        TaskKind::Process { command, .. } => command.clone(),
        _ => None,
    };
    let command_line = match &command {
        Some(command) => format!(" (command: {command})"),
        None => String::new(),
    };
    lines.push(format!(
        "<background-task-silence-warning task_id=\"{}\"{}>",
        task.id, command_line
    ));
    lines.push(format!(
        "The background task \"{}\" has produced no output for {}.",
        task.description,
        format_duration(silent_for)
    ));
    // The tail summary: the LAST output the task produced, clamped to
    // 1–2 KB (the `output_snapshot` tail — the persisted log is the
    // authoritative source, the retained buffer the fallback).
    let tail = silence_warn_tail(tasks, &task.id);
    if let Some(tail) = tail {
        lines.push(String::new());
        lines.push("Last output (tail):".to_string());
        lines.push(tail);
    } else {
        lines.push(String::new());
        lines.push("The task has not produced any output since it started.".to_string());
    }
    lines.push(String::new());
    lines.push(
        "This may be normal (a long-running build or watch), or the process may be stuck. \
         You can check the task with TaskOutput, consider stopping it with TaskStop, \
         or tell the user if it looks healthy."
            .to_string(),
    );
    // The re-arm horizon when a second deadline exists: the timeout
    // sweeper owns the kill, the warning only needs to say when the next
    // hard event lands.
    if let Some(deadline) = task.deadline {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        lines.push(format!(
            "Note: the task's timeout will fire in {} (after which it terminates).",
            format_duration(remaining)
        ));
    }
    lines.push("</background-task-silence-warning>".to_string());
    SilenceWarning {
        task_id: task.id.clone(),
        message: lines.join("\n"),
    }
}

/// The 1–2 KB tail of the task's output — the `output_snapshot` tail
/// (persisted log authoritative, retained buffer fallback), `None` when
/// the task produced nothing at all.
fn silence_warn_tail(tasks: &TaskManager, task_id: &str) -> Option<String> {
    let snapshot = tasks.output_snapshot(task_id, SILENCE_WARN_TAIL_BYTES)?;
    let tail = snapshot.preview;
    if tail.is_empty() {
        return None;
    }
    // Tail-clamp to the warning budget (the snapshot preview may exceed
    // it when no persisted log exists and the retained buffer is large).
    Some(tail_bytes(&tail, SILENCE_WARN_TAIL_BYTES).to_string())
}

/// Render a duration for the model: whole seconds at second granularity,
/// minutes when it exceeds a minute (rounded down), hours when it
/// exceeds an hour.
fn format_duration(duration: Duration) -> String {
    let secs = duration.as_secs();
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m{}s", secs / 60, secs % 60)
    } else {
        format!("{}h{}m", secs / 3600, (secs % 3600) / 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::{TaskKind, TaskManager};

    fn process_task() -> AgentTask {
        let tasks = TaskManager::new();
        let id = tasks.register(
            TaskKind::Process {
                command: Some("npm run watch".into()),
                dispatcher: None,
            },
            "watch build".into(),
            "session".into(),
        );
        tasks.get(&id).unwrap()
    }

    #[test]
    fn should_flag_a_silent_process_task_when_past_the_threshold() {
        // Given a running process task silent for 301s and a 300s threshold
        let task = process_task();
        let now = std::time::Instant::now();
        let anchor = now - Duration::from_secs(301);

        // When the due check runs
        let due = is_silence_warn_due(&task, Some(anchor), Duration::from_secs(300), now);

        // Then the warning is due
        assert!(due);
    }

    #[test]
    fn should_not_flag_when_no_anchor_is_known() {
        // Given a running process task with no silence anchor
        let task = process_task();

        // When the due check runs with None
        let due = is_silence_warn_due(
            &task,
            None,
            Duration::from_secs(300),
            std::time::Instant::now(),
        );

        // Then no warning is due (None means the driver could not resolve
        // an anchor — nothing to measure against)
        assert!(!due);
    }

    #[test]
    fn should_not_flag_a_terminal_task_even_when_silent() {
        // Given a completed process task (terminal first)
        let tasks = TaskManager::new();
        let id = tasks.register(
            TaskKind::Process {
                command: None,
                dispatcher: None,
            },
            "t".into(),
            "s".into(),
        );
        tasks.complete(&id, 0);
        let task = tasks.get(&id).unwrap();
        let now = std::time::Instant::now();

        // When the due check runs with a long silence
        let due = is_silence_warn_due(
            &task,
            Some(now - Duration::from_secs(1000)),
            Duration::from_secs(300),
            now,
        );

        // Then no warning (terminal tasks never warn)
        assert!(!due);
    }

    #[test]
    fn should_not_flag_an_agent_kind_task_even_when_silent() {
        // Given a running agent-kind task
        let tasks = TaskManager::new();
        let id = tasks.register(
            TaskKind::Agent {
                agent_id: None,
                subagent_type: None,
                parent_tool_call_id: None,
                swarm_index: None,
                model: None,
            },
            "research".into(),
            "s".into(),
        );
        let task = tasks.get(&id).unwrap();
        let now = std::time::Instant::now();

        // When the due check runs with a long silence
        let due = is_silence_warn_due(
            &task,
            Some(now - Duration::from_secs(1000)),
            Duration::from_secs(300),
            now,
        );

        // Then no warning (only process-kind tasks warn)
        assert!(!due);
    }

    #[test]
    fn should_render_task_id_duration_and_guidance_in_the_warning() {
        // Given a silent process task with some output
        let tasks = TaskManager::new();
        let id = tasks.register(
            TaskKind::Process {
                command: Some("npm run watch".into()),
                dispatcher: None,
            },
            "watch build".into(),
            "s".into(),
        );
        tasks.append_output(&id, "line one\nline two\n");
        let task = tasks.get(&id).unwrap();

        // When the warning renders
        let warning = render_silence_warning(&tasks, &task, Duration::from_secs(301));

        // Then the message carries the identity, the duration, the tail,
        // and the action guidance
        assert!(warning.message.contains(&id), "{}", warning.message);
        assert!(
            warning.message.contains("watch build"),
            "{}",
            warning.message
        );
        assert!(warning.message.contains("5m1s"), "{}", warning.message);
        assert!(warning.message.contains("line two"), "{}", warning.message);
        assert!(
            warning.message.contains("TaskOutput"),
            "{}",
            warning.message
        );
        assert!(warning.message.contains("TaskStop"), "{}", warning.message);
        assert_eq!(warning.task_id, id);
    }

    #[test]
    fn should_render_the_rearm_horizon_when_the_task_has_a_deadline() {
        // Given a silent process task registered with a timeout
        let tasks = TaskManager::new();
        let id = tasks.register_with_timeout(
            TaskKind::Process {
                command: None,
                dispatcher: None,
            },
            "build".into(),
            "s".into(),
            Some(Duration::from_secs(600)),
        );
        let task = tasks.get(&id).unwrap();

        // When the warning renders
        let warning = render_silence_warning(&tasks, &task, Duration::from_secs(301));

        // Then the message names the remaining time to the deadline
        assert!(
            warning.message.contains("timeout will fire in"),
            "{}",
            warning.message
        );
    }

    #[test]
    fn should_render_the_empty_output_arm_when_nothing_was_produced() {
        // Given a silent process task with no output at all
        let tasks = TaskManager::new();
        let id = tasks.register(
            TaskKind::Process {
                command: None,
                dispatcher: None,
            },
            "quiet".into(),
            "s".into(),
        );
        let task = tasks.get(&id).unwrap();

        // When the warning renders
        let warning = render_silence_warning(&tasks, &task, Duration::from_secs(301));

        // Then the message says so instead of an empty tail
        assert!(
            warning
                .message
                .contains("has not produced any output since it started"),
            "{}",
            warning.message
        );
    }
}
