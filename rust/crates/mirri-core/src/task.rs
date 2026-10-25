//! Background task tracking — the session-level task manager.
//!
//! [`TaskManager`] records long-running background work spawned by the agent
//! (shell commands, build watchers, subagents). Each task accumulates output
//! capped at [`MAX_TASK_OUTPUT_BYTES`]; once the cap is exceeded the head of
//! the accumulated output is dropped and reads report a truncated preview via
//! [`TaskManager::output`]. When a session closes, [`TaskManager::stop_all`]
//! marks every still-running task as [`TaskStatus::Stopped`]. The raw stream
//! is also mirrored to an append-only on-disk log when persistence is
//! attached — [`TaskManager::output_snapshot`] reads that authoritative,
//! never-truncated source (the `TaskOutput` tool's view; a resumed session
//! sees the full history this way).
//!
//! Two lifecycle knobs are configurable per manager:
//! - a **heartbeat interval** — [`TaskManager::heartbeat`] refreshes a task's
//!   liveness window and [`TaskManager::is_stale`] reports tasks whose last
//!   heartbeat is older than the interval (used to detect agents that died
//!   without reporting back);
//! - an **agent timeout** — [`TaskManager::terminate_expired`] marks
//!   still-running tasks whose `created_at` + timeout has elapsed as
//!   [`TaskStatus::Failed`], so a stuck agent cannot leak tasks forever.
//!
//! Output reads are also available in a session-scoped form,
//! [`TaskManager::output_for_session`], which refuses to return output to a
//! session other than the one that created the task.
//!
//! The manager optionally mirrors every mutation to the filesystem through a
//! [`TaskPersistence`] (per-task JSON on status changes, append-only output
//! logs) so tasks survive a session restart. Persistence is best-effort: an
//! I/O failure is logged and never fails the in-memory operation.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;
use uuid::Uuid;

use crate::cancel::CancelToken;

use crate::session::events::{EventBus, SessionEvent};
use crate::task_persist::TaskPersistence;

/// Maximum number of bytes of accumulated output retained per task.
/// Output beyond this cap is dropped from the *head* so the most recent
/// output always survives.
pub const MAX_TASK_OUTPUT_BYTES: usize = 1024 * 1024;

/// Number of characters included at each end of a truncated output preview.
pub const TASK_OUTPUT_PREVIEW_CHARS: usize = 2000;

/// Message recorded on a still-running task that outlived its agent timeout.
pub const TASK_TIMEOUT_MESSAGE: &str = "task exceeded its configured timeout";

/// Capacity rejection message (TS `assertCanRegister`, `taskService.ts:939`).
pub const TASK_CAPACITY_MESSAGE: &str = "Too many background tasks are already running.";

/// Status of a background task.
///
/// Serialized as an adjacently tagged enum for the wire protocol, e.g.
/// `{"status": "running"}`, `{"status": "completed", "detail": {"exit_code": 0}}`,
/// `{"status": "failed", "detail": "boom"}`. Struct variants serialize
/// their fields as a nested `detail` object.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", content = "detail", rename_all = "snake_case")]
pub enum TaskStatus {
    /// The task is still executing.
    Running,
    /// The task finished on its own with the given process exit code.
    Completed { exit_code: i32 },
    /// The task terminated with an error.
    Failed(String),
    /// The task was stopped by the manager (e.g. session close).
    Stopped,
}

impl TaskStatus {
    /// Returns `true` when the task is no longer running — i.e. it has
    /// reached any terminal status (completed, failed, or stopped).
    pub fn is_terminal(&self) -> bool {
        !matches!(self, TaskStatus::Running)
    }
}

/// The boxed detached-run future the spawn variants drive: the settle
/// payload is `(exit_code, stdout, stderr)`.
pub type DetachedRunFuture =
    Pin<Box<dyn Future<Output = anyhow::Result<(i32, String, String)>> + Send>>;

/// What kind of work a background task tracks — the discriminator of the
/// protocol's `BackgroundTaskInfo` union (design §7.8). The registering call
/// site always knows the kind: the Bash tool spawns processes, the Agent
/// tool tracks subagents, the question tool tracks reverse-RPC asks.
///
/// Serialized as an internally tagged enum (`"kind": "process"`, …) so the
/// same value serves both the persisted [`AgentTask`] record and — flattened
/// into [`BackgroundTaskInfo`] — the `event.background.task.*` wire payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TaskKind {
    /// A detached shell command started through the Bash tool.
    #[serde(rename_all = "camelCase")]
    Process {
        /// The command line being run; `None` only for tasks loaded from
        /// pre-kind persistence files, which are terminal after a restart
        /// and never emit live events.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        command: Option<String>,
        /// The agent that registered the task — the notification
        /// routing's dispatcher attribution: a bash background task a
        /// subagent started delivers its completion notification to that
        /// subagent, not the session's main agent. `None` on pre-field
        /// persistence files (main stays the addressee) and for
        /// user/main-originated tasks.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        dispatcher: Option<String>,
    },
    /// A subagent tracked by the Agent tool.
    #[serde(rename_all = "camelCase")]
    Agent {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        subagent_type: Option<String>,
        /// The spawning tool call this member belongs to — the parent linkage
        /// the supervision snapshot roster and the web's swarm grouping key on
        /// (supervision architecture §4.2). Absent on pre-field persistence
        /// files, which load as unparented.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent_tool_call_id: Option<String>,
        /// The member's index inside its swarm batch (swarm members only).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        swarm_index: Option<u32>,
        /// The resolved model alias the subagent runs on (explicit → profile
        /// default → caller), surfaced so task-list consumers (the web's
        /// per-agent model badge) survive polling rebuilds. Absent on
        /// pre-field persistence files and profile-less registrations.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
    },
    /// A user-question reverse-RPC round trip.
    #[serde(rename_all = "camelCase")]
    Question { question_count: u32 },
}

/// Legacy persisted tasks predate the `kind` field; they load as
/// process-kind with an unknown command (the TaskManager predates any other
/// task producer, so process is the honest retro-classification).
fn legacy_task_kind() -> TaskKind {
    TaskKind::Process {
        command: None,
        dispatcher: None,
    }
}

/// The wire snapshot of one background task — the TS protocol's
/// `BackgroundTaskInfo` (a discriminated union on `kind`), carried by the
/// `event.background.task.started` / `event.background.task.terminated`
/// payloads.
///
/// The kind-specific fields ride along through the flattened [`TaskKind`];
/// `pid`, `exitCode` (outside process completions), and the TS optional
/// fields the engine does not track (`detached`,
/// `terminalNotificationSuppressed`, `timeoutMs`, `toolCallId`) stay absent
/// rather than degraded to null placeholders — the contract suite's branch
/// schema declares those adaptations.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackgroundTaskInfo {
    /// What kind of work the task tracks (the union discriminator).
    #[serde(flatten)]
    pub kind: TaskKind,
    /// Unique task identifier.
    pub task_id: String,
    /// Short human-readable description of the work.
    pub description: String,
    /// Protocol status vocabulary: `running` / `completed` / `failed` /
    /// `killed` (manager-stopped).
    pub status: String,
    /// Registration instant, epoch milliseconds.
    pub started_at: i64,
    /// Terminal instant, epoch milliseconds; `null` while running — the
    /// protocol's own required-nullable spelling (never absent).
    pub ended_at: Option<i64>,
    /// Failure text on `failed`; absent otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    /// Agent identity for agent-kind tasks — the subagent member the task
    /// tracks, in the `"agent-<uuid>"` convention; absent for process and
    /// question tasks (the TS contract reserves the same optional
    /// `AgentBackgroundTaskInfo.agentId` slot).
    ///
    /// Engine-side accessor only: the wire `agentId` key is owned by the
    /// flattened [`TaskKind::Agent`] member (the same value rides out there),
    /// so this flat copy stays out of (de)serialization — emitting it as a
    /// sibling key would duplicate `agentId` and break the ws-relay's
    /// `from_value::<SessionEvent>` deserialization (serde rejects the
    /// duplicate field).
    #[serde(skip)]
    pub agent_id: Option<String>,
    /// Process exit code; present only on `completed` process tasks (the
    /// flat struct is shared across kinds, so the process-member field
    /// stays absent rather than leaking a null onto agent/question tasks).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i64>,
}

/// Outcome of [`TaskManager::wait_for_completion`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitResult {
    /// The task reached a terminal status before the deadline.
    Completed,
    /// The timeout elapsed while the task was still running.
    TimedOut,
    /// The task id is unknown.
    NotFound,
    /// The caller's cancel token fired while the task was still running —
    /// the wait ended immediately without observing a settlement. The task
    /// itself keeps running; only the wait was abandoned.
    Cancelled,
}

/// Why a foreground task's release wait ended (the TS
/// `ForegroundTaskReleaseReason`): `Detached` when a `task.detach`
/// conversion fired while the tool still ran, `Terminal` when the task
/// settled first (completed / failed / stopped — the awaiter then observes
/// the natural settlement, never a conversion).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForegroundReleaseReason {
    /// A `task.detach` conversion fired while the tool still ran.
    Detached,
    /// The task settled before any detach (the tool observes its own
    /// natural exit path).
    Terminal,
}

/// The shared state behind a [`ForegroundRegistration`]: the resolved
/// reason (first writer wins — a detach followed by a terminal settle
/// keeps `Detached`) plus the `Notify` permit store. Firing stores the
/// reason BEFORE the permit so an awaiter that wakes always reads a set
/// reason.
struct ForegroundReleaseState {
    reason: Mutex<Option<ForegroundReleaseReason>>,
    notify: Notify,
}

impl ForegroundReleaseState {
    /// Resolve the release: record the reason (first writer wins) and
    /// store one permit so both an armed and a later awaiter observe it.
    fn fire(&self, reason: ForegroundReleaseReason) {
        let mut slot = self
            .reason
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if slot.is_none() {
            *slot = Some(reason);
        }
        drop(slot);
        self.notify.notify_one();
    }

    /// The resolved reason, when the release already fired.
    fn resolved(&self) -> Option<ForegroundReleaseReason> {
        *self
            .reason
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// The release handle a foreground tool awaits (the TS
/// `waitForForegroundRelease` counterpart): the registration returned by
/// [`TaskManager::register_foreground`]. `released()` resolves
/// [`ForegroundReleaseReason::Detached`] when `task.detach` fires the
/// conversion, [`ForegroundReleaseReason::Terminal`] when the task settles
/// first — a stored permit before the first poll still wins
/// (`tokio::sync::Notify` keeps one unclaimed notify, the same trick
/// `shell_command.rs` uses).
pub struct ForegroundRegistration {
    /// The minted task id (registered `detached=false`).
    pub task_id: String,
    state: Arc<ForegroundReleaseState>,
}

impl ForegroundRegistration {
    /// Await the release. Resolves immediately when the release already
    /// fired (the stored reason wins over arming the notify).
    pub async fn released(&self) -> ForegroundReleaseReason {
        if let Some(reason) = self.state.resolved() {
            return reason;
        }
        self.state.notify.notified().await;
        self.state
            .resolved()
            .unwrap_or(ForegroundReleaseReason::Terminal)
    }
}

/// A background task tracked by the agent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentTask {
    /// Unique task identifier (generated by [`TaskManager::register`]).
    pub id: String,
    /// Short human-readable description of the work being done.
    pub description: String,
    /// What kind of work the task tracks (persisted; absent in pre-kind
    /// files, which load as process-kind via [`legacy_task_kind`]).
    #[serde(default = "legacy_task_kind")]
    pub kind: TaskKind,
    /// Current lifecycle status.
    pub status: TaskStatus,
    /// When the task was registered.
    pub created_at: DateTime<Utc>,
    /// When the task reached a terminal status; `None` while running
    /// (absent in pre-`ended_at` persistence files).
    #[serde(default)]
    pub ended_at: Option<DateTime<Utc>>,
    /// Accumulated output, capped at [`MAX_TASK_OUTPUT_BYTES`].
    pub output: String,
    /// The session this task belongs to.
    pub session_id: String,
    /// Explicit per-task timeout deadline (TS `registerTask({timeoutMs})`).
    /// In-memory only — never persisted and never carried on the wire. When
    /// set, it wins over the manager-wide `agent_timeout` fallback in
    /// [`TaskManager::terminate_expired`].
    #[serde(skip)]
    pub deadline: Option<Instant>,
    /// True for background (detached) registrations — survives the
    /// terminal transition (unlike `background_ids`, which is pruned) so
    /// the notification loop can filter foreground tasks out (TS
    /// `detached`). Legacy records without the key load as detached (TS
    /// parity: only an explicit `false` marks foreground).
    #[serde(default = "default_detached")]
    pub detached: bool,
    /// Terminal notification suppressed for this task (G25: TaskStop makes
    /// its tool result the only answer; session close / print drain suppress
    /// before stopping). Persisted so a resumed session never notifies.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub notification_suppressed: bool,
    /// When the terminal notification was durably delivered (own turn, the
    /// driver's opportunistic context sweep, or restore re-append). Persisted
    /// so a rebuilt context (resume, compaction, subagent journal replay)
    /// never re-arms delivery; `None` until first delivery. Undo clears it
    /// to re-arm (SDR-B2-01).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notification_delivered_at: Option<DateTime<Utc>>,
    /// The process identity fingerprint (ADR-012 a) for process-kind
    /// tasks: pid + pgid + start time, captured when the child was
    /// obtained. Persisted so the startup orphan sweep can answer "is the
    /// process this archive points at still alive, and is it really it"
    /// across service restarts (pid reuse included). `None` on legacy
    /// archives — the sweep degrades those to report-only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<mirri_os::process::ProcessFingerprint>,
    /// The task's lifetime ceiling is lifted (US14.4/SDR-013 d): the
    /// resident lifetime sweeper never terminates this task on the
    /// age-based ceiling. Either an explicit model request at dispatch
    /// (`exempt_lifetime: true` on the Bash tool — once per task, never
    /// appendable: a longer-lived need means re-dispatching) or the
    /// keepAliveOnExit session setting (D-64/G33: a user who configured
    /// exit-survival already said "keep it alive"). Persisted so the
    /// sweep judges it across restarts; `serde(default)` keeps legacy
    /// archives non-exempt.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub lifetime_exempt: bool,
}

/// The `detached` default for legacy persistence files (pre-B2 records
/// predate the field): detached, matching the TS loader's treatment — a
/// record that does not say `detached: false` still gets its notification.
fn default_detached() -> bool {
    true
}

impl AgentTask {
    /// The wire snapshot of this task — the `info` carried by the
    /// `event.background.task.*` events. Maps the engine's
    /// [`TaskStatus`] onto the protocol's status vocabulary: `stopped`
    /// becomes `killed` (manager-initiated stop, e.g. session close; the
    /// protocol's `lost` — reconciled after a restart — is never emitted
    /// live because reconciliation happens before any subscriber exists),
    /// and `timed_out` is carried as `failed` with the timeout message in
    /// `stopReason`.
    pub fn background_task_info(&self) -> BackgroundTaskInfo {
        let (status, stop_reason, exit_code) = match &self.status {
            TaskStatus::Running => ("running", None, None),
            TaskStatus::Completed { exit_code } => ("completed", None, Some(i64::from(*exit_code))),
            TaskStatus::Failed(reason) => ("failed", Some(reason.clone()), None),
            TaskStatus::Stopped => ("killed", None, None),
        };
        // Agent-kind tasks surface the member identity on the wire snapshot;
        // process/question tasks stay absent (the TS `agentId` is optional).
        let agent_id = match &self.kind {
            TaskKind::Agent { agent_id, .. } => agent_id.clone(),
            _ => None,
        };
        BackgroundTaskInfo {
            kind: self.kind.clone(),
            task_id: self.id.clone(),
            description: self.description.clone(),
            status: status.to_string(),
            started_at: self.created_at.timestamp_millis(),
            ended_at: self.ended_at.map(|ended| ended.timestamp_millis()),
            stop_reason,
            agent_id,
            exit_code,
        }
    }
}

/// A snapshot of a task's accumulated output.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskOutput {
    /// Human-friendly view: the full output when it fits, otherwise the
    /// first and last [`TASK_OUTPUT_PREVIEW_CHARS`] characters with an
    /// elision marker between them.
    pub preview: String,
    /// The complete retained output (bounded by [`MAX_TASK_OUTPUT_BYTES`]).
    pub full_output: String,
    /// Whether the retained output was truncated — i.e. bytes were dropped
    /// from the head because the cap was exceeded.
    pub truncated: bool,
}

/// The output snapshot the `TaskOutput` tool serves (the TS
/// `getOutputSnapshot` counterpart): the persisted log is the
/// authoritative, never-truncated source when one exists; the retained
/// in-memory buffer is the fallback.
#[derive(Debug, Clone, PartialEq)]
pub struct TaskOutputSnapshot {
    /// The task's append-only output log, when a persistence layer is
    /// attached and the log exists — the complete, never-truncated
    /// source the model can page through with the Read tool.
    pub output_path: Option<std::path::PathBuf>,
    /// Total output bytes: the full log size when persisted; the retained
    /// buffer's length in the fallback (the manager keeps no
    /// total-produced counter in memory).
    pub output_size_bytes: u64,
    /// Whether the preview omits the head of the output.
    pub truncated: bool,
    /// Whether the complete output can be read from `output_path`.
    pub full_output_available: bool,
    /// The preview text served inline.
    pub preview: String,
}

impl TaskOutputSnapshot {
    /// The fallback view over a [`TaskOutput`] retained-buffer read —
    /// detached managers and tasks without a persisted log.
    fn from_retained(retained: &TaskOutput) -> Self {
        Self {
            output_path: None,
            output_size_bytes: retained.full_output.len() as u64,
            truncated: retained.truncated,
            full_output_available: false,
            preview: retained.preview.clone(),
        }
    }
}

/// Manages background tasks for a session.
///
/// All operations are synchronous and internally synchronized; the manager is
/// The liveness listener wired onto a [`TaskManager`] (ADR-004 batch a):
/// invoked when a waited-on task's heartbeat advances.
pub type LivenessListener = Arc<dyn Fn(&str) + Send + Sync>;

/// cheaply shareable behind an `Arc` and is intended to live as a per-session
/// service.
pub struct TaskManager {
    tasks: Mutex<HashMap<String, AgentTask>>,
    /// Ids of tasks whose retained output has been truncated to the cap.
    truncated: Mutex<HashSet<String>>,
    /// Optional on-disk mirror of task metadata and output, rooted at
    /// `<session_dir>/tasks/`. Every mutation is also written through here,
    /// best-effort: an I/O failure is logged and swallowed so persistence
    /// never breaks the live in-memory operation.
    persistence: Option<Arc<TaskPersistence>>,
    /// Liveness window for stale detection, when enabled. Tasks whose last
    /// [`Self::heartbeat`] is older than this interval are stale per
    /// [`Self::is_stale`]. `None` disables stale detection entirely.
    heartbeat_interval: Option<Duration>,
    /// Last heartbeat instant per task — used by [`Self::is_stale`]. Seeded
    /// at registration; purely in-memory (never persisted).
    last_heartbeat: Mutex<HashMap<String, Instant>>,
    /// Liveness listener for blocked waits (ADR-004 batch a): invoked from
    /// [`Self::wait_for_completion_with`] whenever the waited-on task's
    /// heartbeat advances. The stall watchdog routes its activity signal
    /// through here; `None` (the default) makes liveness reporting a no-op.
    /// Interior-mutable so the turn wrapper can install/clear it through
    /// the shared `Arc<TaskManager>`.
    liveness_listener: Mutex<Option<LivenessListener>>,
    /// Wall-clock deadline for every registered task, when enabled. Tasks
    /// still running past `created_at + agent_timeout` are terminated by
    /// [`Self::terminate_expired`]. `None` disables the agent timeout.
    /// This is only the **fallback**: a task registered with an explicit
    /// per-task deadline ([`Self::register_with_timeout`]) ignores it.
    agent_timeout: Option<Duration>,
    /// Maximum number of concurrently running background tasks (D-49).
    /// `0` = unbounded (the default). Enforced only on the
    /// [`Self::register_background`] entry point — foreground
    /// registrations are never capacity-limited (TS `assertCanRegister`
    /// applies to detached work only).
    max_running_tasks: usize,
    /// Ids of tasks registered through the [`Self::register_background`]
    /// entry point — the only registrations the capacity limit counts
    /// (bash background commands and background questions register here
    /// too — TS `assertCanRegister` counts every detached entry). An id is
    /// pruned on the task's first terminal transition, alongside its
    /// signal entry, so the set tracks the live background tasks.
    background_ids: Mutex<HashSet<String>>,
    /// Optional bypass onto the session's event bus — publishes the
    /// `background.task.started` / `background.task.terminated`
    /// [`SessionEvent`]s the backends project as `event.background.task.*`.
    /// Wired by [`Self::set_event_bus`] after construction (the manager
    /// outlives the wiring point behind its `Arc`), like the turn flow's
    /// `set_event_bus` seam.
    event_bus: Mutex<Option<Arc<EventBus>>>,
    /// Cancel signals per task id, wired by the registering call site
    /// ([`Self::attach_signal`]) so an explicit stop can actually abort the
    /// underlying work (G16 — `TaskStop` fires the signal before the status
    /// transition; a task without a signal, e.g. a shell process, only gets
    /// its status changed, exactly like before). Removed when the task
    /// reaches a terminal status.
    signals: Mutex<HashMap<String, CancelToken>>,
    /// Foreground release permits per task id (B7 D-3): the
    /// [`ForegroundRegistration`] face a running foreground tool awaits.
    /// An entry exists exactly while the task is a live foreground
    /// registration — the first terminal transition (and the
    /// [`Self::stop_all`] bulk path) prunes it, firing `Terminal` so a
    /// still-armed awaiter never hangs.
    foreground_releases: Mutex<HashMap<String, Arc<ForegroundReleaseState>>>,
    /// The tool-call-id addressing face of the foreground registrations
    /// (B7 D-3): model-side tool call id → the foreground task it tracks,
    /// so `task.detach { toolCallId }` finds the task without knowing its
    /// minted id. Pruned together with the release entry.
    tool_call_ids: Mutex<HashMap<String, String>>,
}

impl Default for TaskManager {
    fn default() -> Self {
        Self::new()
    }
}

impl TaskManager {
    /// Create an empty task manager with no persistence, no heartbeat-based
    /// stale detection, and no agent timeout.
    pub fn new() -> Self {
        Self {
            tasks: Mutex::new(HashMap::new()),
            truncated: Mutex::new(HashSet::new()),
            persistence: None,
            heartbeat_interval: None,
            last_heartbeat: Mutex::new(HashMap::new()),
            liveness_listener: Mutex::new(None),
            agent_timeout: None,
            max_running_tasks: 0,
            background_ids: Mutex::new(HashSet::new()),
            event_bus: Mutex::new(None),
            signals: Mutex::new(HashMap::new()),
            foreground_releases: Mutex::new(HashMap::new()),
            tool_call_ids: Mutex::new(HashMap::new()),
        }
    }

    /// Wire the session's event bus onto the manager so every task
    /// lifecycle mutation publishes the `background.task.*`
    /// [`SessionEvent`]s. Idempotent; a manager without a bus stays fully
    /// functional (tests, detached hosts) and simply never emits.
    pub fn set_event_bus(&self, event_bus: Arc<EventBus>) {
        *self
            .event_bus
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(event_bus);
    }

    /// Publish one lifecycle event on the attached bus, if any. Publishing is
    /// non-blocking (broadcast send) and safe under the tasks lock; an event
    /// with no subscriber is dropped (a bus attached after the fact replays
    /// nothing — same semantics as every other SessionEvent publisher).
    fn publish_event(&self, event: SessionEvent) {
        let bus = self
            .event_bus
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(bus) = bus.as_ref() {
            bus.publish(event);
        }
    }

    /// Configure a heartbeat interval for stale detection.
    ///
    /// When set, [`Self::register`] seeds each task with an initial heartbeat
    /// and [`Self::is_stale`] reports tasks whose last [`Self::heartbeat`]
    /// is older than `interval`. Consume the returned manager.
    pub fn with_heartbeat_interval(mut self, interval: Duration) -> Self {
        self.heartbeat_interval = Some(interval);
        self
    }

    /// Configure an agent timeout applied to every registered task.
    ///
    /// When set, [`Self::terminate_expired`] marks still-running tasks whose
    /// `created_at + timeout` has elapsed as failed. Consume the returned
    /// manager.
    pub fn with_agent_timeout(mut self, timeout: Duration) -> Self {
        self.agent_timeout = Some(timeout);
        self
    }

    /// Configure the background-task capacity limit (D-49).
    ///
    /// When set to a non-zero `limit`, [`Self::register_background`]
    /// rejects registration once `limit` tasks are concurrently running
    /// with [`TASK_CAPACITY_MESSAGE`]. Foreground registrations
    /// ([`Self::register`]) are never limited. `0` (the default) keeps
    /// registration unbounded.
    pub fn with_max_running_tasks(mut self, limit: usize) -> Self {
        self.set_max_running_tasks(limit);
        self
    }

    /// In-place capacity setter (the [`Self::with_max_running_tasks`]
    /// builder consumes the manager, which the session assembly cannot —
    /// it already holds a concrete instance). The builder delegates here,
    /// so both idioms mutate the same field.
    pub fn set_max_running_tasks(&mut self, limit: usize) {
        self.max_running_tasks = limit;
    }

    /// In-place heartbeat setter (the [`Self::with_heartbeat_interval`]
    /// builder counterpart for an already-constructed manager — the
    /// builder delegates here).
    pub fn set_heartbeat_interval(&mut self, interval: Duration) {
        self.heartbeat_interval = Some(interval);
    }

    /// Wire the liveness listener invoked by
    /// [`Self::wait_for_completion_with`] when the waited-on task's
    /// heartbeat advances (ADR-004 batch a — the stall watchdog's activity
    /// signal for blocked waits). `None` un-wires; idempotent.
    pub fn set_liveness_listener(&self, listener: Option<LivenessListener>) {
        *self
            .liveness_listener
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = listener;
    }

    /// Create a task manager that mirrors every mutation to the given
    /// persistence root.
    ///
    /// Tasks already persisted on disk are loaded into memory. Tasks found
    /// in [`TaskStatus::Running`] WITHOUT a fingerprint are reconciled to
    /// [`TaskStatus::Stopped`]: after a restart no live process owns them
    /// (the TS kernel calls these "lost" tasks), and the reconciled status
    /// is written back to disk so a subsequent load sees it directly.
    /// Running tasks WITH a process fingerprint are kept Running — the
    /// startup orphan sweep (ADR-012 c) probes the live process and owns
    /// their verdict.
    pub fn with_persistence(persistence: Arc<TaskPersistence>) -> Self {
        let mut manager = Self::new();
        manager.persistence = Some(Arc::clone(&persistence));
        match persistence.load_tasks() {
            Ok(loaded) => {
                let mut tasks = manager
                    .tasks
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                for task in loaded {
                    // ADR-012 c "census first": a Running archive WITH a
                    // fingerprint is NOT reconciled here — the startup
                    // orphan sweep owns its verdict (probe the live process,
                    // then converge). Only fingerprint-less archives (legacy
                    // or in-process kinds) keep the pre-E14 reconcile.
                    let task = if matches!(task.status, TaskStatus::Running)
                        && task.fingerprint.is_none()
                    {
                        tracing::warn!(
                            task_id = %task.id,
                            "reconciling lost running task as stopped after restart"
                        );
                        let reconciled = AgentTask {
                            status: TaskStatus::Stopped,
                            // The restart instant is the best available
                            // terminal timestamp for a task whose owning
                            // process died with the previous run.
                            ended_at: Some(Utc::now()),
                            ..task
                        };
                        if let Err(error) = persistence.save_task(&reconciled) {
                            tracing::warn!(
                                task_id = %reconciled.id,
                                %error,
                                "failed to persist reconciled task status"
                            );
                        }
                        reconciled
                    } else {
                        task
                    };
                    tasks.insert(task.id.clone(), task);
                }
            }
            Err(error) => {
                tracing::warn!(
                    %error,
                    "failed to load persisted tasks; starting with an empty task set"
                );
            }
        }
        manager
    }

    /// Register a new background task.
    ///
    /// The task starts in [`TaskStatus::Running`] with empty output. Returns
    /// the generated task id. Publishing the `background.task.started` event
    /// happens after the insert so a subscriber's handler can already read
    /// the task back through [`Self::get`].
    pub fn register(&self, kind: TaskKind, description: String, session_id: String) -> String {
        self.register_inner(kind, description, session_id, None, false)
    }

    /// Register a new background task with an explicit per-task timeout
    /// (the TS `registerTask({ timeoutMs })` counterpart).
    ///
    /// The deadline is in-memory only (never persisted) and wins over the
    /// manager-wide `agent_timeout` fallback when [`Self::terminate_expired`]
    /// evaluates expiry. `None` behaves exactly like [`Self::register`].
    pub fn register_with_timeout(
        &self,
        kind: TaskKind,
        description: String,
        session_id: String,
        timeout: Option<Duration>,
    ) -> String {
        self.register_inner(kind, description, session_id, timeout, false)
    }

    /// Register a detached (background) task under the capacity limit
    /// (D-49).
    ///
    /// When [`Self::with_max_running_tasks`] configured a non-zero limit and
    /// that many tasks are already running, registration is rejected with
    /// [`TASK_CAPACITY_MESSAGE`] (TS `assertCanRegister` parity). Foreground
    /// registration through [`Self::register`] is never limited.
    pub fn register_background(
        &self,
        kind: TaskKind,
        description: String,
        session_id: String,
        timeout: Option<Duration>,
    ) -> Result<String, String> {
        // The `background_ids` lock is held across the capacity check AND
        // the insert: concurrent registrations serialize here, so two
        // racing callers cannot both observe a free slot and overshoot the
        // limit by one (the lock order is always background_ids → tasks —
        // `transition` only touches `background_ids` after releasing the
        // tasks lock, so no reverse acquisition exists).
        let mut background_ids = self
            .background_ids
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if self.max_running_tasks > 0
            && self.running_background_count(&background_ids) >= self.max_running_tasks
        {
            tracing::warn!(
                limit = self.max_running_tasks,
                "background task registration rejected: capacity reached"
            );
            return Err(TASK_CAPACITY_MESSAGE.to_string());
        }
        let id = self.register_inner(kind, description, session_id, timeout, true);
        background_ids.insert(id.clone());
        Ok(id)
    }

    /// Whether a background registration would currently fit under the
    /// capacity limit — the pre-spawn query the Agent tool consults BEFORE
    /// dispatching a subagent (a rejected dispatch must not mint a
    /// `subagent.spawned` event). Advisory only: the authoritative check
    /// still runs inside [`Self::register_background`], so a concurrent
    /// registration can close the window between the two.
    pub fn can_register_background(&self) -> bool {
        if self.max_running_tasks == 0 {
            return true;
        }
        let background_ids = self
            .background_ids
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.running_background_count(&background_ids) < self.max_running_tasks
    }

    /// Count the background-registered tasks that are still running.
    ///
    /// Only background-registered tasks consume capacity: the TS
    /// `assertCanRegister` counts the (detached) tracked tasks, and a
    /// foreground run that merely tracks its progress through the manager
    /// (the Rust Agent tool's foreground path) must not lock a background
    /// slot. The caller must hold the `background_ids` lock (read or
    /// write); the `tasks` lock is taken here (always in that order).
    fn running_background_count(
        &self,
        background_ids: &std::collections::HashSet<String>,
    ) -> usize {
        let tasks = self
            .tasks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        background_ids
            .iter()
            .filter(|id| {
                tasks
                    .get(*id)
                    .is_some_and(|task| task.status == TaskStatus::Running)
            })
            .count()
    }

    /// Register a foreground (detached=false) task a running tool tracks,
    /// keyed for `task.detach` by BOTH the minted task id and the optional
    /// model-side tool call id (B7 D-3, the TS `registerTask` /
    /// `waitForForegroundRelease` counterpart).
    ///
    /// No capacity check — a foreground run must not lock a
    /// `[task] maxRunningTasks` slot (TS `assertCanRegister` applies to
    /// detached work only), and no `BackgroundTaskStarted` event fires at
    /// registration: the started event belongs to the detach conversion
    /// ([`Self::detach_foreground`], the TS `detachEntry` parity — the
    /// `recordTaskStarted` call lives there).
    pub fn register_foreground(
        &self,
        kind: TaskKind,
        description: String,
        session_id: String,
        tool_call_id: Option<String>,
    ) -> ForegroundRegistration {
        let (task_id, task) = Self::fresh_task(kind, description, session_id, None, false);
        let state = Arc::new(ForegroundReleaseState {
            reason: Mutex::new(None),
            notify: Notify::new(),
        });
        // The release entry and the tool-call index row land BEFORE the
        // task becomes visible: an observer that races the insertion
        // (task.list / the overview) must never see a foreground task no
        // detach can address. A detach arriving in the tiny window before
        // the insert answers None, exactly like an unknown id.
        self.foreground_releases
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(task_id.clone(), Arc::clone(&state));
        if let Some(tool_call_id) = tool_call_id {
            self.tool_call_ids
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(tool_call_id, task_id.clone());
        }
        self.insert(task_id.clone(), task, false);
        ForegroundRegistration { task_id, state }
    }

    /// Convert a running foreground task to background (the TS
    /// `detachEntry` conversion): flip `detached`, claim a `background_ids`
    /// slot (the converted task is a running background task from here on —
    /// the capacity count stays honest), persist, publish
    /// `BackgroundTaskStarted`, and fire the release so the tool's
    /// [`ForegroundRegistration::released`] awaiter resolves `Detached`.
    ///
    /// Terminal or already-detached tasks (and unknown ids) answer `None`
    /// with no state change — the B7-adjudicated honest null (the TS
    /// `detachEntry` answers the task info for these; the Rust surface
    /// keeps null so a stale call is a visible no-op, the klient
    /// `noResult` spelling).
    pub fn detach_foreground(&self, task_id: &str) -> Option<AgentTask> {
        // The release entry is the live-foreground marker: absent for
        // background registrations, unknown ids, and pruned (terminal)
        // tasks. Lock order: release tables before `tasks` (the same
        // direction `register_background`'s background_ids → tasks takes).
        let release = self
            .foreground_releases
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(task_id)
            .cloned()?;
        let updated = {
            let mut tasks = self
                .tasks
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let task = tasks.get_mut(task_id)?;
            if task.status.is_terminal() || task.detached {
                return None;
            }
            task.detached = true;
            task.clone()
        };
        self.background_ids
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(task_id.to_string());
        self.persist_task(&updated);
        // Publish the conversion's Started BEFORE firing the release: the
        // fire is what wakes the runner's continuation, whose settle
        // publishes Terminated — the Started of the same conversion must
        // never land after it (the started-before-terminated ordering
        // every other registration path keeps).
        self.publish_event(SessionEvent::BackgroundTaskStarted {
            info: updated.background_task_info(),
        });
        release.fire(ForegroundReleaseReason::Detached);
        Some(updated)
    }

    /// The tool-call-id addressing face of the same conversion —
    /// `task.detach { toolCallId }` routes here (B7 D-3).
    pub fn detach_foreground_by_tool_call(&self, tool_call_id: &str) -> Option<AgentTask> {
        let task_id = self
            .tool_call_ids
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(tool_call_id)
            .cloned()?;
        self.detach_foreground(&task_id)
    }

    /// Shared registration path: insert the task, seed the heartbeat,
    /// persist best-effort, and publish the started event (the foreground
    /// registration is the one caller that suppresses the event — it
    /// builds through [`Self::fresh_task`] and calls [`Self::insert`]
    /// directly, so its started event belongs to the detach conversion).
    fn register_inner(
        &self,
        kind: TaskKind,
        description: String,
        session_id: String,
        timeout: Option<Duration>,
        detached: bool,
    ) -> String {
        let (id, task) = Self::fresh_task(kind, description, session_id, timeout, detached);
        self.insert(id.clone(), task, true);
        id
    }

    /// Build the fresh task record: id minted, Running, heartbeat-seeded,
    /// deadline applied — NOT yet inserted.
    fn fresh_task(
        kind: TaskKind,
        description: String,
        session_id: String,
        timeout: Option<Duration>,
        detached: bool,
    ) -> (String, AgentTask) {
        let id = format!("task-{}", Uuid::new_v4());
        let task = AgentTask {
            id: id.clone(),
            kind,
            description,
            status: TaskStatus::Running,
            created_at: Utc::now(),
            ended_at: None,
            output: String::new(),
            session_id,
            deadline: timeout.map(|timeout| Instant::now() + timeout),
            detached,
            notification_suppressed: false,
            notification_delivered_at: None,
            fingerprint: None,
            lifetime_exempt: false,
        };
        (id, task)
    }

    /// Register a background task under a caller-supplied id.
    ///
    /// The shell-command runner mints its foreground task id *before* the
    /// run starts (the `shell.started` event carries the id a detach will
    /// convert into a background task), so the conversion must register
    /// under that pre-assigned id instead of a freshly generated one.
    /// Idempotence is the caller's responsibility: a duplicate id silently
    /// replaces the earlier task, like [`Self::register`] would.
    pub fn register_with_id(
        &self,
        id: String,
        kind: TaskKind,
        description: String,
        session_id: String,
    ) -> AgentTask {
        let task = AgentTask {
            id: id.clone(),
            kind,
            description,
            status: TaskStatus::Running,
            created_at: Utc::now(),
            ended_at: None,
            output: String::new(),
            session_id,
            deadline: None,
            // The pre-assigned id path is the foreground→background
            // conversion (the shell detach): the task IS detached.
            detached: true,
            notification_suppressed: false,
            notification_delivered_at: None,
            fingerprint: None,
            lifetime_exempt: false,
        };
        self.insert(id, task.clone(), true);
        task
    }

    /// Insert a fully-formed task; the started event publishes only when
    /// `publish_started` (a foreground registration stays event-quiet until
    /// its detach conversion fires it).
    fn insert(&self, id: String, task: AgentTask, publish_started: bool) {
        tracing::info!(task_id = %id, "background task registered");
        self.tasks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(id.clone(), task.clone());
        // Registration counts as the initial heartbeat: a freshly registered
        // task is alive until the configured interval elapses.
        self.last_heartbeat
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(id, Instant::now());
        self.persist_task(&task);
        if publish_started {
            self.publish_event(SessionEvent::BackgroundTaskStarted {
                info: task.background_task_info(),
            });
        }
    }

    /// Drive a Process-kind run's future to completion while keeping the
    /// task's heartbeat advancing.
    ///
    /// Process-liveness heartbeat (ADR-004 batch a gap): a PENDING process
    /// task IS alive — a long silent command (a full cargo build emitting no
    /// output for minutes) must keep its heartbeat advancing, so neither
    /// [`Self::is_stale`] nor a waiting turn's watchdog feed sees a dead
    /// task. The ticker beats at a quarter of the heartbeat window (four
    /// beats per window — a single dropped beat cannot read as stale), with
    /// [`MissedTickBehavior::Delay`] so a busy runtime coalesces missed
    /// beats instead of firing a burst.
    ///
    /// Only the signal-carrying Process spawns use this — agent-kind tasks
    /// keep their activity-based semantics (a genuinely hung subagent must
    /// still read as stale).
    async fn drive_with_process_heartbeat(
        mut run: DetachedRunFuture,
        manager: Arc<TaskManager>,
        task_id: String,
        interval: Duration,
    ) -> anyhow::Result<(i32, String, String)> {
        let mut ticker = tokio::time::interval(interval / 4);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                result = &mut run => return result,
                _ = ticker.tick() => manager.heartbeat(&task_id),
            }
        }
    }

    /// Spawn a detached background task from a cancel-aware future factory
    /// and track it (D-63).
    ///
    /// Registers the task (starting in [`TaskStatus::Running`]), attaches a
    /// fresh [`CancelToken`] to it, and passes a clone into
    /// `future_factory` — so a [`TaskManager::stop_with_grace`] /
    /// [`TaskManager::stop_all_with_grace`] on this task actually cancels
    /// the underlying work (the pre-B1 spawn never wired a signal, leaving
    /// stopped background processes running as orphans). When the future
    /// resolves with `Ok((exit_code, stdout, stderr))`, stdout and stderr are
    /// appended to the task's accumulated output and the task is marked
    /// [`TaskStatus::Completed`] — unless the cancel signal fired while it
    /// ran, in which case the run settles as [`TaskStatus::Stopped`] (the
    /// honest wire `killed`, never a fake completion under cancellation).
    /// When it errors, the task is marked [`TaskStatus::Failed`] — unless
    /// the cancel signal fired, which settles [`TaskStatus::Stopped`]
    /// instead (the same honest-terminal rule).
    ///
    /// Process-kind tasks additionally run under
    /// [`Self::drive_with_process_heartbeat`]: their heartbeat keeps
    /// advancing while the future is pending (see that method for why).
    ///
    /// Settle-rule nondeterminism (documented tradeoff): the signal is
    /// sampled at settle time only, so a stop racing a natural completion
    /// may label an already-finished run `Stopped` — its result is then
    /// folded into the output but the exit code is reported as killed.
    ///
    /// Returns the generated task id immediately; the caller must not await
    /// the factory's future itself. Unlike the per-turn cancellation that
    /// stops foreground tool calls, a spawned task outlives the turn that
    /// started it and is only stopped by
    /// [`TaskManager::stop_all_with_grace`](Self::stop_all_with_grace)
    /// (session close) or an explicit
    /// [`TaskManager::stop_with_grace`](Self::stop_with_grace).
    ///
    /// The registration is NOT capacity-limited; the background producers
    /// that must count toward `[task] maxRunningTasks` use
    /// [`Self::spawn_background_with_signal`] or
    /// [`Self::spawn_background_detached`] instead.
    pub fn spawn_with_signal(
        self: &Arc<Self>,
        kind: TaskKind,
        description: String,
        session_id: String,
        future_factory: impl FnOnce(
            CancelToken,
        ) -> Pin<
            Box<dyn Future<Output = anyhow::Result<(i32, String, String)>> + Send>,
        >,
    ) -> String {
        let signal = CancelToken::new();
        let heartbeat_interval =
            (matches!(kind, TaskKind::Process { .. })).then(|| self.heartbeat_interval());
        let task_id = self.register(kind, description, session_id);
        self.attach_signal(&task_id, signal.clone());
        let future = future_factory(signal.clone());
        let manager = Arc::clone(self);
        let task_id_clone = task_id.clone();
        tokio::spawn(async move {
            let result = if let Some(interval) = heartbeat_interval.flatten() {
                Self::drive_with_process_heartbeat(
                    future,
                    Arc::clone(&manager),
                    task_id_clone.clone(),
                    interval,
                )
                .await
            } else {
                future.await
            };
            manager
                .settle_background_run(&task_id_clone, Some(signal), result)
                .await;
        });
        task_id
    }

    /// The capacity-limited, cancellable background spawn (D-49 + D-63):
    /// identical to [`Self::spawn_with_signal`] except the registration
    /// goes through [`Self::register_background`], so the spawned task
    /// consumes a `[task] maxRunningTasks` slot.
    ///
    /// The future factory receives the minted `task_id` alongside the cancel
    /// signal (the same allowance as [`Self::spawn_background_detached`]):
    /// the id is only known after registration, which happens inside this
    /// call — the bash tool needs it to attach the child's process identity
    /// fingerprint (ADR-012 a) to the row it just registered.
    ///
    /// Returns [`TASK_CAPACITY_MESSAGE`] when the limit is reached — the
    /// caller must surface that error to the model INSTEAD of running the
    /// work (TS `assertCanRegister` parity: a rejected producer never
    /// starts).
    pub fn spawn_background_with_signal(
        self: &Arc<Self>,
        kind: TaskKind,
        description: String,
        session_id: String,
        future_factory: impl FnOnce(
            CancelToken,
            String,
        ) -> Pin<
            Box<dyn Future<Output = anyhow::Result<(i32, String, String)>> + Send>,
        >,
    ) -> Result<String, String> {
        let signal = CancelToken::new();
        let heartbeat_interval =
            (matches!(kind, TaskKind::Process { .. })).then(|| self.heartbeat_interval());
        let task_id = self.register_background(kind, description, session_id, None)?;
        self.attach_signal(&task_id, signal.clone());
        let future = future_factory(signal.clone(), task_id.clone());
        let manager = Arc::clone(self);
        let task_id_clone = task_id.clone();
        tokio::spawn(async move {
            let result = if let Some(interval) = heartbeat_interval.flatten() {
                Self::drive_with_process_heartbeat(
                    future,
                    Arc::clone(&manager),
                    task_id_clone.clone(),
                    interval,
                )
                .await
            } else {
                future.await
            };
            manager
                .settle_background_run(&task_id_clone, Some(signal), result)
                .await;
        });
        Ok(task_id)
    }

    /// The capacity-limited, signal-less background spawn: for detached
    /// work whose future CANNOT observe a cancel token (a pending client
    /// question — the reverse RPC has no cancellation channel).
    ///
    /// The future factory receives the minted `task_id` so the closure can
    /// settle its own task through the manager (e.g. a dismissed question
    /// lands itself cancelled via
    /// [`Self::settle_background_cancelled`] — the id is only known after
    /// registration, which happens inside this call).
    ///
    /// Registering without a signal is deliberate: a stop then escalates
    /// immediately through the [`Self::stop_with_grace`] no-signal
    /// shortcut instead of burning the whole kill-grace window polling a
    /// settlement that cannot happen.
    pub fn spawn_background_detached(
        self: &Arc<Self>,
        kind: TaskKind,
        description: String,
        session_id: String,
        future_factory: impl FnOnce(String) -> DetachedRunFuture + Send + 'static,
    ) -> Result<String, String> {
        let task_id = self.register_background(kind, description, session_id, None)?;
        let manager = Arc::clone(self);
        let task_id_clone = task_id.clone();
        tokio::spawn(async move {
            let result = future_factory(task_id_clone.clone()).await;
            manager
                .settle_background_run(&task_id_clone, None, result)
                .await;
        });
        Ok(task_id)
    }

    /// Settle a background run as CANCELLED — the user dismissed a pending
    /// question (SDR-005 (c)): append the dismissal output, then land
    /// [`TaskStatus::Stopped`] (the wire `killed`, the UI vocabulary
    /// "cancelled").
    ///
    /// The dismissing question closure calls this itself from inside its
    /// spawned future (the gate channel resolved to dismissal — no
    /// answer); [`Self::settle_background_run`] then runs after and its
    /// `complete`/`fail` is a first-terminal-wins no-op.
    pub async fn settle_background_cancelled(&self, task_id: &str, output: &str) {
        self.append_output(task_id, output);
        self.stop(task_id);
    }

    /// Settle a background run (the post-detach continuation and every
    /// spawned detached run share it): append its output, then land the
    /// honest terminal status — `Stopped` when the signal fired (on either
    /// the Ok or the Err arm), the natural status otherwise.
    pub async fn settle_background_run(
        self: &Arc<Self>,
        task_id: &str,
        signal: Option<CancelToken>,
        result: anyhow::Result<(i32, String, String)>,
    ) {
        let cancelled = signal.as_ref().is_some_and(|s| s.is_cancelled());
        match result {
            Ok((exit_code, stdout, stderr)) => {
                self.append_output(task_id, &stdout);
                self.append_output(task_id, &stderr);
                if cancelled {
                    // The run ended under cancellation — the honest
                    // terminal is Stopped (wire "killed"), not a
                    // completion carrying whatever exit code the
                    // cancelled work last reported.
                    self.stop(task_id);
                } else {
                    self.complete(task_id, exit_code);
                }
            }
            Err(error) => {
                if cancelled {
                    self.stop(task_id);
                } else {
                    self.fail(task_id, error.to_string());
                }
            }
        }
    }

    /// Append output to a task's accumulated output.
    ///
    /// No-op for unknown task ids (with a warning). When appending would
    /// exceed [`MAX_TASK_OUTPUT_BYTES`], the retained output keeps only the
    /// most recent bytes up to the cap; a single chunk larger than the cap is
    /// itself tail-truncated first.
    pub fn append_output(&self, task_id: &str, output: &str) {
        if output.is_empty() {
            return;
        }
        let mut tasks = self
            .tasks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(task) = tasks.get_mut(task_id) else {
            tracing::warn!(task_id, "append_output called for unknown task");
            return;
        };
        if task.output.len() + output.len() <= MAX_TASK_OUTPUT_BYTES {
            task.output.push_str(output);
        } else {
            // The incoming chunk alone may exceed the cap.
            let chunk = tail_bytes(output, MAX_TASK_OUTPUT_BYTES);
            let keep = MAX_TASK_OUTPUT_BYTES.saturating_sub(chunk.len());
            if task.output.len() > keep {
                task.output.drain(..task.output.len() - keep);
            }
            task.output.push_str(chunk);
            tracing::warn!(
                task_id,
                "task output truncated at {MAX_TASK_OUTPUT_BYTES} bytes; head dropped"
            );
            self.truncated
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(task_id.to_string());
        }
        drop(tasks);
        // Mirror the *raw* chunk to the append-only on-disk log — the disk
        // stream is not subject to the in-memory cap, so a resumed session
        // can still read the full output history.
        if let Some(persistence) = &self.persistence
            && let Err(error) = persistence.append_output(task_id, output)
        {
            tracing::warn!(
                task_id,
                %error,
                "failed to append task output to disk"
            );
        }
    }

    /// Mark a task as completed with the given process exit code.
    ///
    /// Unknown task ids are ignored with a warning.
    pub fn complete(&self, task_id: &str, exit_code: i32) {
        self.transition(task_id, TaskStatus::Completed { exit_code });
        tracing::info!(task_id, exit_code, "background task completed");
    }

    /// Mark a task as failed with the given error message.
    pub fn fail(&self, task_id: &str, error: String) {
        self.transition(task_id, TaskStatus::Failed(error));
        tracing::warn!(task_id, "background task failed");
    }

    /// Wire a cancel signal to a registered task (G16).
    ///
    /// The registering call site (the Agent tool's subagent runner) supplies
    /// the token that actually aborts the underlying work; an explicit
    /// [`Self::stop`] (or [`Self::stop_all`] on session close) fires it so
    /// the stop is REAL, not just a status flip the still-running work later
    /// overwrites. Tasks without an attached signal (shell processes,
    /// reverse-RPC asks) keep the pre-existing status-only behavior.
    pub fn attach_signal(&self, task_id: &str, signal: CancelToken) {
        self.signals
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(task_id.to_string(), signal);
    }

    /// Fire the task's attached cancel signal, when one exists.
    fn fire_signal(&self, task_id: &str) {
        if let Some(signal) = self
            .signals
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(task_id)
        {
            signal.cancel();
        }
    }

    /// Mark a task as stopped.
    ///
    /// The attached cancel signal (if any) fires FIRST so the underlying
    /// work observes the cancellation before — or while — the status flips;
    /// with the first-terminal-wins rule below, a late natural settle from
    /// the runner can no longer overwrite this terminal status.
    pub fn stop(&self, task_id: &str) {
        self.fire_signal(task_id);
        self.transition(task_id, TaskStatus::Stopped);
        tracing::info!(task_id, "background task stopped");
    }

    /// G25: mark a task's terminal notification suppressed (persisted).
    ///
    /// TaskStop's tool result is the model's only answer for that task —
    /// a killed-notification on top of it would double-report the stop.
    /// Idempotent; unknown ids are ignored with a warning.
    pub fn suppress_terminal_notification(&self, task_id: &str) {
        let updated = {
            let mut tasks = self
                .tasks
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            match tasks.get_mut(task_id) {
                Some(task) => {
                    if task.notification_suppressed {
                        return;
                    }
                    task.notification_suppressed = true;
                    Some(task.clone())
                }
                None => {
                    tracing::warn!(
                        task_id,
                        "notification suppression attempted for unknown task"
                    );
                    None
                }
            }
        };
        if let Some(task) = updated {
            self.persist_task(&task);
        }
    }

    /// Mark a task's terminal notification as durably delivered (persisted).
    ///
    /// The delivery marker is the durable half of the SDR-B2-01 ledger: once
    /// a terminal notification is injected (own turn, the driver's
    /// opportunistic context sweep, or restore re-append), the task record
    /// carries the fact, so a rebuilt context (resume, compaction, subagent
    /// journal replay) never re-arms delivery.
    /// Idempotent; unknown ids are ignored with a warning.
    pub fn mark_notification_delivered(&self, task_id: &str) {
        let updated = {
            let mut tasks = self
                .tasks
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            match tasks.get_mut(task_id) {
                Some(task) => {
                    if task.notification_delivered_at.is_some() {
                        return;
                    }
                    task.notification_delivered_at = Some(Utc::now());
                    Some(task.clone())
                }
                None => {
                    tracing::warn!(task_id, "delivery marker set for unknown task");
                    None
                }
            }
        };
        if let Some(task) = updated {
            self.persist_task(&task);
        }
    }

    /// Clear the delivery marker so the driver re-arms (undo path).
    ///
    /// Undo removed the delivered message from the context, so the
    /// notification must be deliverable again; the in-memory flag and the
    /// persisted record are both cleared. Idempotent; unknown ids are
    /// ignored with a warning.
    pub fn clear_notification_delivered(&self, task_id: &str) {
        let updated = {
            let mut tasks = self
                .tasks
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            match tasks.get_mut(task_id) {
                Some(task) => {
                    if task.notification_delivered_at.is_none() {
                        return;
                    }
                    task.notification_delivered_at = None;
                    Some(task.clone())
                }
                None => {
                    tracing::warn!(task_id, "delivery marker clear for unknown task");
                    None
                }
            }
        };
        if let Some(task) = updated {
            self.persist_task(&task);
        }
    }

    /// Attach the process identity fingerprint to a registered task
    /// (ADR-012 a; the bash tool calls this once it has the child handle —
    /// both the direct-background and foreground-turned-background paths).
    /// The fingerprint is persisted so a later startup orphan sweep can
    /// verify the process this archive points at is still alive and really
    /// the same process (pid reuse included). Unknown ids are ignored.
    pub fn set_fingerprint(
        &self,
        task_id: &str,
        fingerprint: mirri_os::process::ProcessFingerprint,
    ) {
        let updated = {
            let mut tasks = self
                .tasks
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            match tasks.get_mut(task_id) {
                Some(task) => {
                    task.fingerprint = Some(fingerprint);
                    Some(task.clone())
                }
                None => {
                    tracing::warn!(task_id, "fingerprint set for unknown task");
                    None
                }
            }
        };
        if let Some(task) = updated {
            self.persist_task(&task);
        }
    }

    /// Mark a registered task exempt from the lifetime ceiling (persisted).
    ///
    /// The single exemption entry point (US14.4: once per task, never
    /// appendable — a longer-lived need means re-dispatching the work).
    /// The exemption only lifts the age-based lifetime ceiling the
    /// resident sweeper enforces (SDR-013 d); every other lifecycle knob
    /// (an explicit per-task deadline, session close, an explicit stop)
    /// is unaffected. Idempotent; unknown ids are ignored with a warning.
    pub fn set_lifetime_exempt(&self, task_id: &str) {
        let updated = {
            let mut tasks = self
                .tasks
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            match tasks.get_mut(task_id) {
                Some(task) => {
                    if task.lifetime_exempt {
                        return;
                    }
                    task.lifetime_exempt = true;
                    Some(task.clone())
                }
                None => {
                    tracing::warn!(task_id, "lifetime exemption set for unknown task");
                    None
                }
            }
        };
        if let Some(task) = updated {
            self.persist_task(&task);
        }
    }

    /// Suppress every still-running task's terminal notification (TS
    /// `stopAllOnExit` / the print drain): the close-path stop must not
    /// leave a killed-notification debt. Terminal tasks keep their own
    /// notification fate (their debt belongs to the pre-close loop).
    pub fn suppress_all_active(&self) {
        let suppressed: Vec<AgentTask> = {
            let mut tasks = self
                .tasks
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            tasks
                .values_mut()
                .filter(|task| task.status == TaskStatus::Running)
                .filter_map(|task| {
                    if task.notification_suppressed {
                        return None;
                    }
                    task.notification_suppressed = true;
                    Some(task.clone())
                })
                .collect()
        };
        for task in suppressed {
            self.persist_task(&task);
        }
    }

    /// Mark every still-running task exempt from the lifetime ceiling
    /// (SDR-013 d, D-64): the keepAliveOnExit close path calls this right
    /// before the session drops — the surviving tasks have no driver left,
    /// and the age-based ceiling the resident sweeper enforces must not
    /// claim tasks the user explicitly configured to survive the close.
    ///
    /// This only lifts the lifetime ceiling — every other lifecycle knob
    /// (an explicit stop, an explicit per-task deadline) stays armed.
    /// Persisted per task so the sweeper judges the exemption across
    /// restarts.
    pub fn mark_all_running_lifetime_exempt(&self) {
        let exempted: Vec<AgentTask> = {
            let mut tasks = self
                .tasks
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            tasks
                .values_mut()
                .filter(|task| task.status == TaskStatus::Running)
                .filter_map(|task| {
                    if task.lifetime_exempt {
                        return None;
                    }
                    task.lifetime_exempt = true;
                    Some(task.clone())
                })
                .collect()
        };
        for task in exempted {
            self.persist_task(&task);
        }
    }

    /// Overwrite a task's status, ignoring unknown ids. The FIRST terminal
    /// status wins (G16): once a task is completed, failed, or stopped, a
    /// later settle from the underlying runner is a no-op — a task stopped
    /// through [`Self::stop`] cannot flip back to completed when the
    /// cancelled work's late result finally lands. Only the first
    /// running→terminal transition publishes a `background.task.terminated`
    /// event — subscribers observe exactly one termination per started task.
    fn transition(&self, task_id: &str, status: TaskStatus) {
        let updated = {
            let mut tasks = self
                .tasks
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            match tasks.get_mut(task_id) {
                Some(task) => {
                    // First terminal status wins — later transitions on an
                    // already-terminal task change nothing (G16).
                    if task.status.is_terminal() {
                        tracing::debug!(
                            task_id,
                            "ignoring late status transition on terminal task"
                        );
                        return;
                    }
                    let was_running = task.status == TaskStatus::Running;
                    task.status = status;
                    task.ended_at = Some(Utc::now());
                    Some((task.clone(), was_running))
                }
                None => {
                    tracing::warn!(task_id, "status transition attempted for unknown task");
                    None
                }
            }
        };
        // The signal registry entry outlived its task: drop it on the first
        // terminal transition so the map does not grow with the session.
        // The background-id registry is pruned the same way — a terminal
        // entry no longer counts toward capacity, so keeping the id would
        // only grow the set (this runs after the tasks lock is released:
        // the lock order is always background_ids → tasks, never the
        // reverse). The B7 foreground release entries die with their task
        // too — the `Terminal` fire resolves a still-armed awaiter (a
        // detached task's entry survives its conversion; the fire is then
        // a first-wins no-op) and the tool-call index loses its row.
        self.signals
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(task_id);
        self.background_ids
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(task_id);
        if let Some(release) = self
            .foreground_releases
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(task_id)
        {
            release.fire(ForegroundReleaseReason::Terminal);
        }
        self.tool_call_ids
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .retain(|_, owner| owner != task_id);
        if let Some((task, was_running)) = updated {
            self.persist_task(&task);
            if was_running {
                self.publish_event(SessionEvent::BackgroundTaskTerminated {
                    info: task.background_task_info(),
                });
            }
        }
    }

    /// List all tasks, sorted by id for deterministic ordering.
    pub fn list(&self) -> Vec<AgentTask> {
        let mut tasks: Vec<AgentTask> = self
            .tasks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .cloned()
            .collect();
        tasks.sort_by(|a, b| a.id.cmp(&b.id));
        tasks
    }

    /// Get a task by id.
    pub fn get(&self, task_id: &str) -> Option<AgentTask> {
        self.tasks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(task_id)
            .cloned()
    }

    /// Resolve a task id the way the model-facing tools address tasks.
    ///
    /// Exact ids pass through unchanged. A bare uuid — the `task-` prefix
    /// stripped, the shape models send when they misremember the id format
    /// [`Self::register_inner`] generates — is repaired by normalizing the
    /// uuid and retrying with the prefix; normalizing covers the uppercase
    /// and hyphenless spellings `Uuid::parse_str` accepts, which a plain
    /// string concatenation would miss. Only a uuid-shaped id qualifies for
    /// the fallback: any other miss (a different prefix, a plain word)
    /// stays a miss, so the fallback never invents a match.
    ///
    /// The wire endpoints (`task.stop` / `task.cancel` / `task.output`)
    /// deliberately do NOT use this face: their callers echo back the exact
    /// ids the server itself emitted, so the model-facing prefix repair is
    /// out of scope there.
    pub fn resolve_id(&self, task_id: &str) -> Option<String> {
        if self.get(task_id).is_some() {
            return Some(task_id.to_string());
        }
        let normalized = Uuid::parse_str(task_id).ok()?;
        let prefixed = format!("task-{normalized}");
        self.get(&prefixed).is_some().then_some(prefixed)
    }

    /// Get task output, with a preview when the retained output is truncated.
    ///
    /// Returns `None` for unknown task ids. The preview shows the first and
    /// last [`TASK_OUTPUT_PREVIEW_CHARS`] characters of the retained output
    /// with an elision marker; untruncated output is its own preview.
    pub fn output(&self, task_id: &str) -> Option<TaskOutput> {
        let tasks = self
            .tasks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let task = tasks.get(task_id)?.clone();
        let truncated = self
            .truncated
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains(task_id);
        Some(TaskOutput {
            preview: build_preview(&task.output, truncated),
            full_output: task.output,
            truncated,
        })
    }

    /// Read a task's output, but only when `session_id` matches the session
    /// that created the task.
    ///
    /// Returns `None` for unknown task ids *and* for tasks owned by a
    /// different session — the caller cannot distinguish the two, so a
    /// foreign session learns neither the task's existence nor its output.
    pub fn output_for_session(&self, task_id: &str, session_id: &str) -> Option<TaskOutput> {
        let tasks = self
            .tasks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let task = tasks.get(task_id)?;
        if task.session_id != session_id {
            return None;
        }
        let truncated = self
            .truncated
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains(task_id);
        Some(TaskOutput {
            preview: build_preview(&task.output, truncated),
            full_output: task.output.clone(),
            truncated,
        })
    }

    /// The output snapshot the `TaskOutput` tool serves — the TS
    /// `getOutputSnapshot` counterpart. The persisted log is the
    /// authoritative, never-truncated source when the manager has a
    /// persistence layer and the task's log exists (the disk stream is
    /// not subject to the in-memory cap, so a resumed session reads the
    /// full history this way); detached managers and tasks without a log
    /// fall back to the retained in-memory buffer. Returns `None` for
    /// unknown task ids.
    pub fn output_snapshot(
        &self,
        task_id: &str,
        max_preview_bytes: usize,
    ) -> Option<TaskOutputSnapshot> {
        // The registry read doubles as the existence check — resumed tasks
        // are loaded into it by `with_persistence`.
        let retained = self.output(task_id)?;
        let Some(persistence) = &self.persistence else {
            return Some(TaskOutputSnapshot::from_retained(&retained));
        };
        // Probe the log and size together; an I/O failure falls back to the
        // retained buffer (serving something beats failing the read).
        let has_log = persistence.has_output_log(task_id);
        let log_size = persistence.output_size(task_id);
        // Unreachable in practice (manager-generated ids are validated on
        // register), but a probe failure must not break the read.
        let probe_failed = has_log.is_err() || log_size.is_err();
        let log_size = log_size.ok().filter(|_| matches!(has_log, Ok(true)));
        if let Some(size) = log_size {
            let offset = size.saturating_sub(max_preview_bytes as u64);
            match persistence.read_output(task_id, offset, max_preview_bytes) {
                Ok(preview) => {
                    return Some(TaskOutputSnapshot {
                        output_path: persistence.output_log_path(task_id).ok(),
                        output_size_bytes: size,
                        truncated: offset > 0,
                        full_output_available: true,
                        preview,
                    });
                }
                Err(error) => tracing::warn!(
                    task_id,
                    %error,
                    "failed to read the persisted task output; serving the retained buffer"
                ),
            }
        } else if probe_failed {
            tracing::warn!(
                task_id,
                "task output log probe failed; serving the retained buffer"
            );
        }
        Some(TaskOutputSnapshot::from_retained(&retained))
    }

    /// Refresh a task's heartbeat, extending its liveness window.
    ///
    /// Registration seeds an initial heartbeat, so a task that never calls
    /// this stays alive for exactly one heartbeat interval. Unknown task ids
    /// are ignored with a warning.
    pub fn heartbeat(&self, task_id: &str) {
        let known = self
            .tasks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains_key(task_id);
        if !known {
            tracing::warn!(task_id, "heartbeat received for unknown task");
            return;
        }
        self.last_heartbeat
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(task_id.to_string(), Instant::now());
    }

    /// The configured liveness window (when set) — the interval
    /// [`Self::is_stale`] measures against. `None` when stale detection is
    /// disabled.
    pub fn heartbeat_interval(&self) -> Option<std::time::Duration> {
        self.heartbeat_interval
    }

    /// Whether a task is stale — its last heartbeat is older than the
    /// configured heartbeat interval.
    ///
    /// Never `true` when no heartbeat interval is configured. Tasks loaded
    /// from disk at startup have no heartbeat recorded, so with an interval
    /// configured their liveness is unknowable and they are reported stale
    /// — mirroring the "no live process after restart" reconciliation of
    /// [`TaskManager::with_persistence`](Self::with_persistence).
    pub fn is_stale(&self, task_id: &str) -> bool {
        let Some(interval) = self.heartbeat_interval else {
            return false;
        };
        let last_heartbeat = self
            .last_heartbeat
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match last_heartbeat.get(task_id) {
            Some(last) => last.elapsed() > interval,
            None => true,
        }
    }

    /// Terminate tasks whose timeout has elapsed.
    ///
    /// Marks every still-running task that is past its deadline as
    /// [`TaskStatus::Failed`] with [`TASK_TIMEOUT_MESSAGE`] and returns the
    /// ids of the tasks it terminated. Expiry is per-task (D-40): a task
    /// registered through [`Self::register_with_timeout`] /
    /// [`Self::register_background`] with an explicit deadline expires by
    /// that deadline; a task without one falls back to the manager-wide
    /// `agent_timeout` ([`Self::with_agent_timeout`]), and is never
    /// terminated when no fallback is configured.
    ///
    /// A timeout termination is a REAL cancellation (recon: the pre-B1
    /// implementation only flipped the status): the task's attached cancel
    /// signal ([`Self::attach_signal`]) fires FIRST, then the status
    /// transition — the same order [`Self::stop`] uses, so the underlying
    /// work actually aborts instead of being orphaned under a failed
    /// status.
    ///
    /// On a multi-threaded runtime the woken runner can be polled by
    /// another worker between the two synchronous calls; when that
    /// happens the runner's own terminal (typically `Stopped`) lands
    /// first and the timeout `Failed` is a no-op under the
    /// first-terminal-wins rule — the task still terminates either way,
    /// just with the runner's status instead of the timeout message.
    pub fn terminate_expired(&self) -> Vec<String> {
        let now_instant = Instant::now();
        let now_wall = Utc::now();
        let expired: Vec<String> = {
            let tasks = self
                .tasks
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            tasks
                .values()
                .filter(|task| {
                    task.status == TaskStatus::Running
                        && match task.deadline {
                            // Explicit per-task deadline wins.
                            Some(deadline) => now_instant >= deadline,
                            // Fallback: the manager-wide agent timeout.
                            None => self.agent_timeout.is_some_and(|timeout| {
                                now_wall
                                    .signed_duration_since(task.created_at)
                                    .to_std()
                                    // Clock skew (created_at in the future)
                                    // must not terminate a task early.
                                    .is_ok_and(|elapsed| elapsed >= timeout)
                            }),
                        }
                })
                .map(|task| task.id.clone())
                .collect()
        };
        let mut terminated = Vec::with_capacity(expired.len());
        for id in expired {
            // Cancel the underlying work before flipping the status — a
            // timeout that leaves the runner alive is just an orphan with
            // a failed label.
            self.fire_signal(&id);
            self.transition(&id, TaskStatus::Failed(TASK_TIMEOUT_MESSAGE.to_string()));
            terminated.push(id);
        }
        terminated
    }

    /// Wait for a task to reach a terminal status, or until `timeout_secs`
    /// seconds elapse — whichever comes first.
    ///
    /// Polls the task status at a short interval. Returns immediately when
    /// the task is already terminal or the id is unknown. This is used by
    /// [`TaskOutputTool`](crate::tools::TaskOutputTool) when `block` is
    /// `true`.
    pub async fn wait_for_completion(&self, task_id: &str, timeout_secs: u64) -> WaitResult {
        self.wait_for_completion_with(task_id, timeout_secs, None)
            .await
    }

    /// The cancel-aware, liveness-reporting variant of
    /// [`Self::wait_for_completion`] (ADR-004 batches a+b).
    ///
    /// Three differences from the plain wait:
    /// - `cancel` is selected on: a fired token ends the wait immediately
    ///   with [`WaitResult::Cancelled`]. The task keeps running — only the
    ///   wait is abandoned. This is what lets a turn's cooperative
    ///   cancellation reach a blocked `TaskOutput` call (no zombie window
    ///   between the cancel and the wait's natural end).
    /// - `on_liveness` (when provided) is invoked on every poll iteration
    ///   where the task is still demonstrably alive — a heartbeat that has
    ///   advanced since the wait started. The stall watchdog's activity
    ///   signal rides this callback: a healthy task keeps the turn's
    ///   inactivity timer refreshed, while a task whose heartbeat has gone
    ///   silent stops invoking it and the turn is judged stalled within one
    ///   window ("the waiter is still polling" is not liveness —
    ///   `TaskStatus::Running` alone proves nothing, per [`Self::is_stale`]).
    ///   When no heartbeat interval is configured (liveness detection
    ///   disabled), every poll of a present task reports liveness — the
    ///   wait is bounded by `timeout_secs` in that mode anyway.
    pub async fn wait_for_completion_with(
        &self,
        task_id: &str,
        timeout_secs: u64,
        cancel: Option<&crate::cancel::CancelToken>,
    ) -> WaitResult {
        let poll_interval = std::time::Duration::from_millis(50);
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
        // The liveness probe tracks whether the task's heartbeat advanced
        // since the wait began: a task whose heartbeat is older than the
        // wait itself has gone silent (the producing process died without
        // settling) and must not be reported as alive.
        #[allow(unused_variables)]
        let mut last_reported_heartbeat = self.current_heartbeat(task_id);

        loop {
            match self.get(task_id) {
                Some(task) if task.status.is_terminal() => return WaitResult::Completed,
                None => return WaitResult::NotFound,
                _ => {}
            }

            if let Some(cancel) = cancel
                && cancel.is_cancelled()
            {
                // Settlement wins over a cancel that arrived in the same
                // instant (the same race the select branch re-checks for
                // the cancel-first direction): a terminal state observed
                // here means the waiter saw the real outcome.
                if let Some(task) = self.get(task_id)
                    && task.status.is_terminal()
                {
                    return WaitResult::Completed;
                }
                return WaitResult::Cancelled;
            }

            // Liveness: the heartbeat must have advanced since the last
            // report (or since the wait started) for this poll to count as
            // the waited-on object making progress. Without a heartbeat
            // interval (liveness detection disabled) the wait is bounded by
            // its own timeout, so every poll of a present task reports
            // liveness.
            if self.heartbeat_interval.is_none() {
                self.liveness_signal(task_id);
            } else if let Some(heartbeat) = self.current_heartbeat(task_id)
                && Some(heartbeat) > last_reported_heartbeat
            {
                last_reported_heartbeat = Some(heartbeat);
                // ADR-004 batch a: the activity signal for the stall
                // watchdog. The callback is invoked inline (never awaited)
                // — a slow sink must not stall the poll loop.
                self.liveness_signal(task_id);
            }

            if tokio::time::Instant::now() >= deadline {
                return WaitResult::TimedOut;
            }

            tokio::select! {
                _ = tokio::time::sleep(poll_interval) => {}
                _ = async {
                    match cancel {
                        Some(cancel) => cancel.cancelled().await,
                        None => std::future::pending::<()>().await,
                    }
                } => {
                    // Re-check the terminal state first — the task may have
                    // settled in the same instant the cancel fired, and a
                    // settlement that raced the cancel wins (the waiter saw
                    // the real outcome, not the interruption).
                    if let Some(task) = self.get(task_id)
                        && task.status.is_terminal()
                    {
                        return WaitResult::Completed;
                    }
                    return WaitResult::Cancelled;
                }
            }
        }
    }

    /// The task's current heartbeat instant, when heartbeat tracking is
    /// enabled and the task is known. Public for the ADR-006 wait
    /// registry's owner probe (the watchdog's pull at window expiry
    /// compares it against the probe's baseline).
    pub fn current_heartbeat(&self, task_id: &str) -> Option<std::time::Instant> {
        self.heartbeat_interval?;
        self.last_heartbeat
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(task_id)
            .copied()
    }
    /// Notify the liveness listener that the waited-on task is still
    /// making progress (ADR-004 batch a: the stall watchdog's activity
    /// signal for blocked waits).
    fn liveness_signal(&self, task_id: &str) {
        let listener = self
            .liveness_listener
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        if let Some(listener) = listener {
            listener(task_id);
        }
    }

    /// Stop all running tasks (session close).
    ///
    /// Tasks that already finished or failed keep their terminal status.
    /// Every task stopped here publishes one `killed` termination event —
    /// after the map lock is released, like every other publish site.
    pub fn stop_all(&self) {
        // Fire every attached cancel signal first so the underlying work
        // actually aborts (G16 — same rule as the single-task stop).
        let signals: Vec<CancelToken> = self
            .signals
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .cloned()
            .collect();
        for signal in signals {
            signal.cancel();
        }
        let stopped_tasks: Vec<AgentTask> = {
            let mut tasks = self
                .tasks
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let mut stopped = 0;
            let mut changed = Vec::new();
            let now = Utc::now();
            for task in tasks.values_mut() {
                if task.status == TaskStatus::Running {
                    task.status = TaskStatus::Stopped;
                    task.ended_at = Some(now);
                    stopped += 1;
                    changed.push(task.clone());
                }
            }
            tracing::info!(stopped, "session background tasks stopped");
            changed
        };
        // Every task is terminal now — their signal entries die with them,
        // and the B7 foreground release entries too: this bulk path bypasses
        // `transition`, so the `Terminal` fire happens here (a detached
        // task's entry already resolved `Detached` — first-wins keeps it).
        self.signals
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
        let releases: Vec<Arc<ForegroundReleaseState>> = {
            let mut foreground_releases = self
                .foreground_releases
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            foreground_releases
                .drain()
                .map(|(_, state)| state)
                .collect()
        };
        self.tool_call_ids
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
        for release in releases {
            release.fire(ForegroundReleaseReason::Terminal);
        }
        for task in stopped_tasks {
            self.persist_task(&task);
            self.publish_event(SessionEvent::BackgroundTaskTerminated {
                info: task.background_task_info(),
            });
        }
    }

    /// Gracefully stop one task (D-63): fire the cancel signal, then give
    /// the work `grace` to settle naturally.
    ///
    /// TS `stop`/`stopByUser` semantics (`taskService.ts:737-806`): a task
    /// that reaches a terminal status (completed/failed) inside the window
    /// keeps that honest status; only a task still running when the window
    /// expires escalates to [`TaskStatus::Stopped`] (wire `killed`) — the
    /// first-terminal-wins rule of [`Self::transition`] makes a late settle
    /// from the cancelled work a no-op.
    ///
    /// A task with **no attached signal** cannot settle in-window (nothing
    /// observes the cancellation), so the grace wait is skipped and the stop
    /// escalates immediately — the two-layer grace SDR keeps the
    /// SIGTERM→SIGKILL escalation for processes inside `mirri-os`.
    /// Unknown ids and already-terminal tasks are no-ops.
    pub async fn stop_with_grace(&self, task_id: &str, grace: Duration) {
        // Fire the attached cancel signal first (G16) so the underlying
        // work observes the cancellation before — or while — any status
        // flips.
        self.fire_signal(task_id);
        if self
            .get(task_id)
            .is_none_or(|task| task.status.is_terminal())
        {
            return;
        }
        // A task with no attached signal cannot settle in-window (nothing
        // observes the cancellation), so the grace wait is skipped and the
        // stop escalates immediately.
        if self.has_signal(task_id) && self.await_settlement(&[task_id.to_string()], grace).await {
            return; // The task settled naturally inside the window.
        }
        self.stop(task_id);
    }

    /// Gracefully stop every running task (session close, D-63/D-64): fire
    /// every attached signal, wait — concurrently, bounded by `grace` — for
    /// natural settlements, then escalate the still-running remainder to
    /// [`TaskStatus::Stopped`].
    ///
    /// Tasks that already finished or failed keep their terminal status.
    /// Signal-less tasks cannot settle in-window, so they escalate
    /// IMMEDIATELY (the same shortcut as the single-task
    /// [`Self::stop_with_grace`]) instead of burning the grace window.
    /// Callers honor `keep_alive_on_exit` (D-64) by skipping this call
    /// entirely.
    pub async fn stop_all_with_grace(&self, grace: Duration) {
        let running: Vec<String> = {
            let tasks = self
                .tasks
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            tasks
                .values()
                .filter(|task| task.status == TaskStatus::Running)
                .map(|task| task.id.clone())
                .collect()
        };
        if running.is_empty() {
            return;
        }
        // Fire every attached cancel signal first so the underlying work
        // actually aborts (G16 — same rule as the single-task stop).
        let signals: Vec<CancelToken> = self
            .signals
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .cloned()
            .collect();
        for signal in signals {
            signal.cancel();
        }
        // Signal-less tasks cannot settle in-window — escalate them right
        // away instead of burning the whole grace window on them.
        let (signaled, unsignaled): (Vec<String>, Vec<String>) =
            running.into_iter().partition(|id| self.has_signal(id));
        for task_id in &unsignaled {
            self.stop(task_id);
        }
        if self.await_settlement(&signaled, grace).await {
            return; // Everything settleable settled naturally in-window.
        }
        for task_id in signaled {
            self.stop(&task_id);
        }
    }

    /// Whether a task currently has an attached cancel signal.
    fn has_signal(&self, task_id: &str) -> bool {
        self.signals
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains_key(task_id)
    }

    /// Wait until every task in `ids` is terminal or `grace` elapses.
    /// Returns `true` when all settled in-window.
    async fn await_settlement(&self, ids: &[String], grace: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + grace;
        loop {
            let all_settled = ids
                .iter()
                .all(|id| self.get(id).is_none_or(|task| task.status.is_terminal()));
            if all_settled {
                return true;
            }
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Best-effort snapshot of a task to the persistence layer.
    ///
    /// A failure is logged and swallowed — persistence must never break the
    /// in-memory operation it mirrors.
    fn persist_task(&self, task: &AgentTask) {
        if let Some(persistence) = &self.persistence
            && let Err(error) = persistence.save_task(task)
        {
            tracing::warn!(
                task_id = %task.id,
                %error,
                "failed to persist task to disk"
            );
        }
    }
}

/// Return the trailing `max` bytes of `s`, always on a UTF-8 character
/// boundary. Returns `s` unchanged when it already fits.
fn tail_bytes(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut cut = s.len() - max;
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    &s[cut..]
}

/// Build the preview string for a task's retained output.
fn build_preview(full_output: &str, truncated: bool) -> String {
    if !truncated {
        return full_output.to_string();
    }
    let head: String = full_output
        .chars()
        .take(TASK_OUTPUT_PREVIEW_CHARS)
        .collect();
    let tail: String = full_output
        .chars()
        .rev()
        .take(TASK_OUTPUT_PREVIEW_CHARS)
        .collect::<Vec<char>>()
        .into_iter()
        .rev()
        .collect();
    format!("{head}\n… [output truncated] …\n{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    const SESSION: &str = "session-1";

    #[test]
    fn given_a_pre_field_process_kind_json_when_decoded_then_the_dispatcher_is_none() {
        let json = serde_json::json!({ "kind": "process", "command": "ls" });
        let kind: TaskKind = serde_json::from_value(json).expect("pre-field json decodes");
        assert!(matches!(
            kind,
            TaskKind::Process {
                dispatcher: None,
                ..
            }
        ));
    }

    /// Given a registered task with a `task-<uuid>` id, when the id is
    /// resolved through the fallback face, then the exact id passes through
    /// unchanged.
    #[test]
    fn should_resolve_the_exact_id_when_it_matches_a_registered_task() {
        let manager = TaskManager::new();
        let task_id = manager.register(
            TaskKind::Process {
                command: None,
                dispatcher: None,
            },
            "build".into(),
            SESSION.into(),
        );

        assert_eq!(manager.resolve_id(&task_id), Some(task_id));
    }

    /// Given a registered task whose id is `task-<uuid>`, when a bare uuid
    /// (the `task-` prefix stripped, the shape models send when they
    /// misremember the id format) is resolved, then the prefixed id is
    /// recovered.
    #[test]
    fn should_recover_the_prefixed_id_when_resolving_a_bare_uuid() {
        let manager = TaskManager::new();
        let task_id = manager.register(
            TaskKind::Process {
                command: None,
                dispatcher: None,
            },
            "build".into(),
            SESSION.into(),
        );
        let bare = task_id
            .strip_prefix("task-")
            .expect("registered ids carry the task- prefix");

        assert_eq!(manager.resolve_id(bare), Some(task_id));
    }

    /// Given a registered task whose id is `task-<uuid>`, when a bare uuid
    /// in a non-canonical spelling (uppercase, hyphenless) is resolved, then
    /// normalization recovers the prefixed id — `Uuid::parse_str` accepts
    /// those spellings and a plain concatenation would miss the key.
    #[test]
    fn should_recover_the_prefixed_id_when_resolving_a_non_canonical_bare_uuid() {
        let manager = TaskManager::new();
        let task_id = manager.register(
            TaskKind::Process {
                command: None,
                dispatcher: None,
            },
            "build".into(),
            SESSION.into(),
        );
        let bare = task_id
            .strip_prefix("task-")
            .expect("registered ids carry the task- prefix");
        let uppercase = bare.to_uppercase();
        let hyphenless = bare.replace('-', "");

        assert_eq!(manager.resolve_id(&uppercase), Some(task_id.clone()));
        assert_eq!(manager.resolve_id(&hyphenless), Some(task_id));
    }

    /// Given a registered task, when a bare uuid-shaped id that matches
    /// nothing is resolved, then no id is recovered — the fallback only
    /// repairs the prefix, it never invents a match.
    #[test]
    fn should_return_none_when_resolving_an_unknown_bare_uuid() {
        let manager = TaskManager::new();
        manager.register(
            TaskKind::Process {
                command: None,
                dispatcher: None,
            },
            "build".into(),
            SESSION.into(),
        );
        let stranger = Uuid::new_v4().to_string();

        assert_eq!(manager.resolve_id(&stranger), None);
    }

    /// Given a registered task, when a non-uuid id without the prefix
    /// (e.g. `nope`, an empty string, or an id with some other prefix like
    /// `cron-1`) is resolved, then no fallback is attempted — only the
    /// bare-uuid shape qualifies for repair.
    #[test]
    fn should_return_none_when_resolving_a_non_uuid_id_without_the_prefix() {
        let manager = TaskManager::new();
        manager.register(
            TaskKind::Process {
                command: None,
                dispatcher: None,
            },
            "build".into(),
            SESSION.into(),
        );

        assert_eq!(manager.resolve_id("nope"), None);
        assert_eq!(manager.resolve_id(""), None);
        assert_eq!(manager.resolve_id("cron-1"), None);
        // Also not a uuid: a 36-char string with no hyphens.
        assert_eq!(
            manager.resolve_id("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            None
        );
    }

    #[test]
    fn given_a_dispatcher_annotated_process_kind_when_roundtripped_then_the_dispatcher_survives() {
        let kind = TaskKind::Process {
            command: Some("cargo test".to_string()),
            dispatcher: Some("agent-worker-1".to_string()),
        };
        let json = serde_json::to_value(&kind).expect("encodes");
        let decoded: TaskKind = serde_json::from_value(json).expect("decodes");
        assert!(matches!(
            decoded,
            TaskKind::Process {
                dispatcher: Some(ref dispatcher),
                ..
            } if dispatcher == "agent-worker-1"
        ));
    }

    /// Given a manager with persistence whose task's on-disk log fits the
    /// preview window, when a snapshot is requested, then the persisted
    /// log is authoritative: the true log size, a tail-window preview,
    /// honest truncation, and the exposed log path.
    #[test]
    fn should_read_the_persisted_log_when_a_snapshot_is_requested_for_a_task_with_persistence() {
        let tmp = tempfile::tempdir().unwrap();
        let persistence = Arc::new(TaskPersistence::new(
            tmp.path().join("tasks"),
            Arc::new(mirri_os::LocalSyncFs::new()),
        ));
        let manager = TaskManager::with_persistence(Arc::clone(&persistence));

        let id = manager.register(
            TaskKind::Process {
                command: None,
                dispatcher: None,
            },
            "big".into(),
            SESSION.into(),
        );
        manager.append_output(&id, &"x".repeat(600_000));
        manager.append_output(&id, "TAIL_MARKER\n");

        let snapshot = manager
            .output_snapshot(&id, MAX_TASK_OUTPUT_BYTES)
            .expect("snapshot for a persisted task");
        assert_eq!(
            snapshot.output_size_bytes,
            (600_000 + "TAIL_MARKER\n".len()) as u64
        );
        assert!(!snapshot.truncated, "the log fits the 1 MiB window");
        assert!(snapshot.full_output_available);
        assert!(snapshot.preview.contains("TAIL_MARKER\n"));
        assert_eq!(
            snapshot.output_path.as_deref(),
            Some(
                tmp.path()
                    .join("tasks")
                    .join(&id)
                    .join("output.log")
                    .as_path()
            )
        );
        drop(tmp);
    }

    /// Given the same setup with a window smaller than the log, when a
    /// snapshot is requested, then the preview is the log's tail and the
    /// snapshot reports truncation.
    #[test]
    fn should_report_truncation_when_the_persisted_log_exceeds_the_snapshot_window() {
        let tmp = tempfile::tempdir().unwrap();
        let persistence = Arc::new(TaskPersistence::new(
            tmp.path().join("tasks"),
            Arc::new(mirri_os::LocalSyncFs::new()),
        ));
        let manager = TaskManager::with_persistence(Arc::clone(&persistence));

        let id = manager.register(
            TaskKind::Process {
                command: None,
                dispatcher: None,
            },
            "big".into(),
            SESSION.into(),
        );
        let head = "y".repeat(100);
        manager.append_output(&id, &head);
        manager.append_output(&id, "TAIL\n");

        let snapshot = manager.output_snapshot(&id, 8).expect("snapshot");
        assert!(
            snapshot.truncated,
            "the 105-byte log exceeds the 8-byte window"
        );
        assert_eq!(snapshot.preview, "yyyTAIL\n");
        assert_eq!(snapshot.output_size_bytes, 105);
        drop(tmp);
    }

    /// Given a manager without persistence, when a snapshot is requested,
    /// then the retained in-memory buffer is served (the fallback path) —
    /// no path, not full-output-available, and the same preview the
    /// plain `output()` read reports.
    #[test]
    fn should_fall_back_to_the_retained_buffer_when_no_persistence_is_attached() {
        let manager = TaskManager::new();
        let id = manager.register(
            TaskKind::Process {
                command: None,
                dispatcher: None,
            },
            "detached".into(),
            SESSION.into(),
        );
        manager.append_output(&id, "partial\n");

        let snapshot = manager
            .output_snapshot(&id, MAX_TASK_OUTPUT_BYTES)
            .expect("snapshot for a detached manager");
        let retained = manager.output(&id).expect("retained read");
        assert_eq!(snapshot.preview, retained.preview);
        assert_eq!(snapshot.truncated, retained.truncated);
        assert!(!snapshot.full_output_available);
        assert!(snapshot.output_path.is_none());
        assert_eq!(snapshot.output_size_bytes, "partial\n".len() as u64);
    }

    #[test]
    fn should_contain_registered_task_with_running_status_when_listing() {
        let manager = TaskManager::new();

        let id = manager.register(
            TaskKind::Process {
                command: None,
                dispatcher: None,
            },
            "build watcher".into(),
            SESSION.into(),
        );

        let task = manager.get(&id).expect("registered task is retrievable");
        assert!(task.id.starts_with("task-"));
        assert_eq!(task.description, "build watcher");
        assert_eq!(task.session_id, SESSION);
        assert_eq!(task.status, TaskStatus::Running);
        assert!(task.output.is_empty());
        assert!(task.created_at <= Utc::now());
        assert_eq!(manager.list(), vec![task]);
    }

    #[test]
    fn should_sort_tasks_by_id_when_listing_multiple_registered_tasks() {
        let manager = TaskManager::new();
        let first = manager.register(
            TaskKind::Process {
                command: None,
                dispatcher: None,
            },
            "first".into(),
            SESSION.into(),
        );
        let second = manager.register(
            TaskKind::Process {
                command: None,
                dispatcher: None,
            },
            "second".into(),
            SESSION.into(),
        );

        let listed = manager.list();

        let listed_ids: Vec<&str> = listed.iter().map(|task| task.id.as_str()).collect();
        let mut expected_ids = vec![first.as_str(), second.as_str()];
        expected_ids.sort_unstable();
        assert_eq!(listed_ids, expected_ids);
    }

    #[tokio::test]
    async fn should_complete_spawned_task_with_output_and_exit_code_when_future_finishes() {
        let manager = Arc::new(TaskManager::new());

        let id = manager.spawn_with_signal(
            TaskKind::Process {
                command: None,
                dispatcher: None,
            },
            "background build".into(),
            SESSION.into(),
            |_signal| Box::pin(async { Ok((0, "compiled\n".to_string(), String::new())) }),
        );

        assert_eq!(
            manager.wait_for_completion(&id, 5).await,
            WaitResult::Completed
        );
        let task = manager.get(&id).expect("spawned task is tracked");
        assert_eq!(task.status, TaskStatus::Completed { exit_code: 0 });
        assert_eq!(task.output, "compiled\n");
    }

    #[tokio::test]
    async fn should_fail_spawned_task_with_error_message_when_future_errors() {
        let manager = Arc::new(TaskManager::new());

        let id = manager.spawn_with_signal(
            TaskKind::Process {
                command: None,
                dispatcher: None,
            },
            "flaky job".into(),
            SESSION.into(),
            |_signal| Box::pin(async { Err(anyhow::anyhow!("boom")) }),
        );

        assert_eq!(
            manager.wait_for_completion(&id, 5).await,
            WaitResult::Completed
        );
        assert_eq!(
            manager.get(&id).unwrap().status,
            TaskStatus::Failed("boom".into())
        );
    }

    #[test]
    fn should_accumulate_output_in_order_when_appending() {
        let manager = TaskManager::new();
        let id = manager.register(
            TaskKind::Process {
                command: None,
                dispatcher: None,
            },
            "logger".into(),
            SESSION.into(),
        );

        manager.append_output(&id, "line one\n");
        manager.append_output(&id, "line two\n");

        let output = manager.output(&id).expect("task exists");
        assert_eq!(output.full_output, "line one\nline two\n");
        assert_eq!(output.preview, "line one\nline two\n");
        assert!(!output.truncated);
    }

    #[test]
    fn should_retain_recent_tail_and_report_truncated_when_output_exceeds_cap() {
        let manager = TaskManager::new();
        let id = manager.register(
            TaskKind::Process {
                command: None,
                dispatcher: None,
            },
            "chatty".into(),
            SESSION.into(),
        );
        let head = "H".repeat(3 * MAX_TASK_OUTPUT_BYTES / 4);
        let tail = "T".repeat(3 * MAX_TASK_OUTPUT_BYTES / 4);

        manager.append_output(&id, &head);
        manager.append_output(&id, &tail);

        let output = manager.output(&id).expect("task exists");
        assert!(output.truncated);
        assert!(output.full_output.len() <= MAX_TASK_OUTPUT_BYTES);
        assert!(
            output.full_output.ends_with("TTT"),
            "most recent output survives truncation"
        );
        assert!(output.preview.contains("[output truncated]"));
        assert!(
            output.preview.starts_with('H'),
            "retained head still starts with earlier chunk"
        );
        assert!(output.preview.ends_with('T'));
    }

    #[test]
    fn should_tail_truncate_chunk_when_single_chunk_exceeds_cap() {
        let manager = TaskManager::new();
        let id = manager.register(
            TaskKind::Process {
                command: None,
                dispatcher: None,
            },
            "gusher".into(),
            SESSION.into(),
        );
        let giant = "G".repeat(2 * MAX_TASK_OUTPUT_BYTES);

        manager.append_output(&id, &giant);

        let output = manager.output(&id).expect("task exists");
        assert!(output.truncated);
        assert!(output.full_output.len() <= MAX_TASK_OUTPUT_BYTES);
        assert!(output.full_output.chars().all(|c| c == 'G'));
    }

    #[test]
    fn should_preserve_valid_utf8_without_panicking_when_truncating_multibyte_output() {
        let manager = TaskManager::new();
        let id = manager.register(
            TaskKind::Process {
                command: None,
                dispatcher: None,
            },
            "emoji".into(),
            SESSION.into(),
        );
        // Box-drawing / emoji chars are 3-4 bytes; repeatedly crossing the
        // cap must never split a character (would panic) or corrupt UTF-8.
        let chunk = "█📦".repeat(MAX_TASK_OUTPUT_BYTES / 8);

        manager.append_output(&id, &chunk);
        manager.append_output(&id, &chunk);

        let output = manager.output(&id).expect("task exists");
        assert!(output.truncated);
        assert!(std::str::from_utf8(output.full_output.as_bytes()).is_ok());
    }

    #[test]
    fn should_report_completed_status_with_exit_code_when_getting_task() {
        let manager = TaskManager::new();
        let id = manager.register(
            TaskKind::Process {
                command: None,
                dispatcher: None,
            },
            "build".into(),
            SESSION.into(),
        );

        manager.complete(&id, 0);

        assert_eq!(
            manager.get(&id).unwrap().status,
            TaskStatus::Completed { exit_code: 0 }
        );
    }

    #[test]
    fn should_report_failed_status_with_error_message_when_getting_task() {
        let manager = TaskManager::new();
        let id = manager.register(
            TaskKind::Process {
                command: None,
                dispatcher: None,
            },
            "test".into(),
            SESSION.into(),
        );

        manager.fail(&id, "tests panicked".into());

        assert_eq!(
            manager.get(&id).unwrap().status,
            TaskStatus::Failed("tests panicked".into())
        );
    }

    #[test]
    fn should_report_stopped_status_when_getting_task() {
        let manager = TaskManager::new();
        let id = manager.register(
            TaskKind::Process {
                command: None,
                dispatcher: None,
            },
            "watcher".into(),
            SESSION.into(),
        );

        manager.stop(&id);

        assert_eq!(manager.get(&id).unwrap().status, TaskStatus::Stopped);
    }

    #[test]
    fn should_stop_running_tasks_and_leave_terminal_tasks_untouched_when_stopping_all() {
        let manager = TaskManager::new();
        let running = manager.register(
            TaskKind::Process {
                command: None,
                dispatcher: None,
            },
            "running".into(),
            SESSION.into(),
        );
        let completed = manager.register(
            TaskKind::Process {
                command: None,
                dispatcher: None,
            },
            "completed".into(),
            SESSION.into(),
        );
        let failed = manager.register(
            TaskKind::Process {
                command: None,
                dispatcher: None,
            },
            "failed".into(),
            SESSION.into(),
        );
        manager.complete(&completed, 0);
        manager.fail(&failed, "kaboom".into());

        manager.stop_all();

        assert_eq!(manager.get(&running).unwrap().status, TaskStatus::Stopped);
        assert_eq!(
            manager.get(&completed).unwrap().status,
            TaskStatus::Completed { exit_code: 0 }
        );
        assert_eq!(
            manager.get(&failed).unwrap().status,
            TaskStatus::Failed("kaboom".into())
        );
    }

    #[test]
    fn should_not_panic_and_return_none_when_accessing_unknown_task() {
        let manager = TaskManager::new();

        assert!(manager.get("task-nope").is_none());
        assert!(manager.output("task-nope").is_none());
        manager.append_output("task-nope", "data"); // must not panic
        manager.complete("task-nope", 0); // must not panic
        manager.fail("task-nope", "err".into()); // must not panic
        manager.stop("task-nope"); // must not panic
    }

    #[test]
    fn should_preserve_variants_when_serde_roundtripping_task_status() {
        let statuses = vec![
            TaskStatus::Running,
            TaskStatus::Completed { exit_code: 3 },
            TaskStatus::Failed("boom".into()),
            TaskStatus::Stopped,
        ];

        for status in statuses {
            let json = serde_json::to_string(&status).unwrap();
            let decoded: TaskStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(status, decoded);
        }
    }

    #[test]
    fn should_preserve_fields_when_serde_roundtripping_agent_task() {
        let task = AgentTask {
            id: "task-1".into(),
            kind: TaskKind::Question { question_count: 2 },
            description: "desc".into(),
            status: TaskStatus::Failed("nope".into()),
            created_at: Utc::now(),
            ended_at: Some(Utc::now()),
            output: "logs".into(),
            session_id: SESSION.into(),
            deadline: None,
            detached: true,
            notification_suppressed: false,
            notification_delivered_at: None,
            fingerprint: None,
            lifetime_exempt: false,
        };

        let json = serde_json::to_string(&task).unwrap();
        let decoded: AgentTask = serde_json::from_str(&json).unwrap();

        assert_eq!(decoded, task);
        // The detached flag round-trips explicitly; the suppression flag
        // stays out of the serialized form when false (additive JSON field).
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["detached"], serde_json::json!(true));
        assert!(value.get("notification_suppressed").is_none());
        // Legacy archives deserialize with the exemption absent → false
        // (serde default), and an exempt archive round-trips the flag.
        let legacy: AgentTask = serde_json::from_value(serde_json::json!({
            "id": "task-legacy",
            "description": "pre-exemption",
            "kind": { "kind": "process" },
            "status": { "status": "running" },
            "created_at": "2026-01-01T00:00:00Z",
            "output": "",
            "session_id": "session-1"
        }))
        .expect("pre-exemption archive deserializes");
        assert!(!legacy.lifetime_exempt);
        let exempt = serde_json::to_value(&legacy).unwrap();
        assert!(
            exempt.get("lifetime_exempt").is_none(),
            "a false exemption stays out of the archive (additive field)"
        );
    }

    #[test]
    fn should_use_tagged_wire_shape_when_serializing_task_status() {
        let json = serde_json::to_string(&TaskStatus::Failed("boom".to_string())).unwrap();
        assert!(json.contains("\"status\":\"failed\""));
        assert!(json.contains("\"detail\":\"boom\""));

        let json = serde_json::to_string(&TaskStatus::Running).unwrap();
        assert_eq!(json, "{\"status\":\"running\"}");

        // Struct variants serialize their fields as a nested `detail` object,
        // not a bare value — pinned to keep the klient zod mirror honest.
        let json = serde_json::to_string(&TaskStatus::Completed { exit_code: 0 }).unwrap();
        assert_eq!(
            json,
            "{\"status\":\"completed\",\"detail\":{\"exit_code\":0}}"
        );
    }

    // --- background.task live event bypass (design §7.8) -------------------

    /// Given a manager wired to a session event bus, when a task is
    /// registered, then subscribers observe the background-task started
    /// event with a running process-shaped info snapshot.
    #[test]
    fn should_publish_background_task_started_when_a_task_is_registered() {
        let bus = Arc::new(EventBus::new(8));
        let manager = TaskManager::new();
        manager.set_event_bus(Arc::clone(&bus));
        let mut rx = bus.subscribe();

        let id = manager.register(
            TaskKind::Process {
                command: Some("echo hi".into()),
                dispatcher: None,
            },
            "build watcher".into(),
            SESSION.into(),
        );

        match rx.try_recv().expect("started event published") {
            SessionEvent::BackgroundTaskStarted { info } => {
                assert_eq!(info.task_id, id);
                assert_eq!(info.description, "build watcher");
                assert_eq!(info.status, "running");
                assert_eq!(
                    info.kind,
                    TaskKind::Process {
                        command: Some("echo hi".into()),
                        dispatcher: None,
                    }
                );
                assert!(info.started_at > 0, "epoch-ms registration instant");
                assert_eq!(
                    serde_json::to_value(&info).unwrap()["endedAt"],
                    serde_json::Value::Null,
                    "the protocol's required-nullable endedAt rides the started spelling as null"
                );
                assert_eq!(info.stop_reason, None);
            }
            other => panic!("expected BackgroundTaskStarted, got {other:?}"),
        }
    }

    /// Given a running task, when it completes, then subscribers observe the
    /// terminated event carrying the exit code and a stamped end instant.
    #[test]
    fn should_publish_background_task_terminated_when_a_running_task_completes() {
        let bus = Arc::new(EventBus::new(8));
        let manager = TaskManager::new();
        manager.set_event_bus(Arc::clone(&bus));
        let mut rx = bus.subscribe();

        let id = manager.register(
            TaskKind::Process {
                command: Some("echo hi".into()),
                dispatcher: None,
            },
            "job".into(),
            SESSION.into(),
        );
        assert!(matches!(
            rx.try_recv().expect("started"),
            SessionEvent::BackgroundTaskStarted { .. }
        ));

        manager.complete(&id, 0);

        match rx.try_recv().expect("terminated event published") {
            SessionEvent::BackgroundTaskTerminated { info } => {
                assert_eq!(info.task_id, id);
                assert_eq!(info.status, "completed");
                assert_eq!(
                    info.kind,
                    TaskKind::Process {
                        command: Some("echo hi".into()),
                        dispatcher: None,
                    }
                );
                let ended = info.ended_at.expect("terminal tasks carry endedAt");
                assert!(ended >= info.started_at, "endedAt is not before startedAt");
                assert_eq!(
                    serde_json::to_value(&info).unwrap()["kind"],
                    serde_json::json!("process")
                );
                assert_eq!(
                    serde_json::to_value(&info).unwrap()["exitCode"],
                    serde_json::json!(0)
                );
            }
            other => panic!("expected BackgroundTaskTerminated, got {other:?}"),
        }
    }

    /// Given a running task, when it fails, then the terminated event maps to
    /// the `failed` status and carries the error text as the stop reason.
    #[test]
    fn should_map_failed_task_to_failed_status_with_stop_reason_when_it_fails() {
        let bus = Arc::new(EventBus::new(8));
        let manager = TaskManager::new();
        manager.set_event_bus(Arc::clone(&bus));
        let mut rx = bus.subscribe();

        let id = manager.register(
            TaskKind::Agent {
                agent_id: Some("agent-1".into()),
                subagent_type: Some("coder".into()),
                parent_tool_call_id: None,
                swarm_index: None,
                model: None,
            },
            "subagent".into(),
            SESSION.into(),
        );
        assert!(matches!(
            rx.try_recv().expect("started"),
            SessionEvent::BackgroundTaskStarted { .. }
        ));

        manager.fail(&id, "boom".into());

        match rx.try_recv().expect("terminated") {
            SessionEvent::BackgroundTaskTerminated { info } => {
                assert_eq!(info.status, "failed");
                assert_eq!(info.stop_reason.as_deref(), Some("boom"));
                assert_eq!(
                    info.kind,
                    TaskKind::Agent {
                        agent_id: Some("agent-1".into()),
                        subagent_type: Some("coder".into()),
                        parent_tool_call_id: None,
                        swarm_index: None,
                        model: None,
                    }
                );
            }
            other => panic!("expected BackgroundTaskTerminated, got {other:?}"),
        }
    }

    /// Given an agent-kind task, when the manager registers it, then the
    /// wire snapshot carries the identity of the subagent member the task
    /// tracks — `agentId` rides both the struct field and the serialized
    /// JSON (the TS contract's optional `AgentBackgroundTaskInfo.agentId`).
    #[test]
    fn should_carry_agent_identity_in_background_task_info_when_task_is_agent_kind() {
        let bus = Arc::new(EventBus::new(8));
        let manager = TaskManager::new();
        manager.set_event_bus(Arc::clone(&bus));
        let mut rx = bus.subscribe();

        let id = manager.register(
            TaskKind::Agent {
                agent_id: Some("agent-1".into()),
                subagent_type: Some("coder".into()),
                parent_tool_call_id: None,
                swarm_index: None,
                model: None,
            },
            "subagent".into(),
            SESSION.into(),
        );

        match rx.try_recv().expect("started event published") {
            SessionEvent::BackgroundTaskStarted { info } => {
                assert_eq!(info.task_id, id);
                assert_eq!(
                    info.agent_id.as_deref(),
                    Some("agent-1"),
                    "the wire snapshot carries the agent identity"
                );
                assert_eq!(
                    serde_json::to_value(&info).unwrap()["agentId"],
                    serde_json::json!("agent-1"),
                    "the wire key is camelCase agentId"
                );
            }
            other => panic!("expected BackgroundTaskStarted, got {other:?}"),
        }
    }

    /// Given an agent-kind task registered with a resolved model, when the
    /// wire snapshot serializes, then the model rides along as the camelCase
    /// `model` key — the task-list consumers (the web's per-agent model
    /// badge) key on it to survive polling rebuilds.
    #[test]
    fn should_carry_the_resolved_model_in_the_wire_snapshot_when_the_agent_task_has_one() {
        let bus = Arc::new(EventBus::new(8));
        let manager = TaskManager::new();
        manager.set_event_bus(Arc::clone(&bus));
        let mut rx = bus.subscribe();

        manager.register(
            TaskKind::Agent {
                agent_id: Some("agent-1".into()),
                subagent_type: Some("coder".into()),
                parent_tool_call_id: None,
                swarm_index: None,
                model: Some("kimi-k2".into()),
            },
            "subagent".into(),
            SESSION.into(),
        );

        match rx.try_recv().expect("started event published") {
            SessionEvent::BackgroundTaskStarted { info } => {
                assert_eq!(
                    serde_json::to_value(&info).unwrap()["model"],
                    serde_json::json!("kimi-k2"),
                    "the wire key is camelCase model"
                );
            }
            other => panic!("expected BackgroundTaskStarted, got {other:?}"),
        }
    }

    /// Given an agent-kind task without a resolved model (a pre-field
    /// persistence file or a profile-less registration), when the wire
    /// snapshot serializes, then the `model` key stays absent — the optional
    /// field is never fabricated as null.
    #[test]
    fn should_omit_the_model_key_in_the_wire_snapshot_when_the_agent_task_has_none() {
        let bus = Arc::new(EventBus::new(8));
        let manager = TaskManager::new();
        manager.set_event_bus(Arc::clone(&bus));
        let mut rx = bus.subscribe();

        manager.register(
            TaskKind::Agent {
                agent_id: Some("agent-1".into()),
                subagent_type: Some("coder".into()),
                parent_tool_call_id: None,
                swarm_index: None,
                model: None,
            },
            "subagent".into(),
            SESSION.into(),
        );

        match rx.try_recv().expect("started event published") {
            SessionEvent::BackgroundTaskStarted { info } => {
                let value = serde_json::to_value(&info).unwrap();
                assert!(
                    value.get("model").is_none(),
                    "the absent model must not serialize a null placeholder: {value}"
                );
            }
            other => panic!("expected BackgroundTaskStarted, got {other:?}"),
        }
    }

    /// Given a process-kind task, when the manager registers it, then the
    /// wire snapshot omits the agent identity — the optional field stays
    /// absent both on the struct and in the serialized JSON (the TS
    /// contract's `AgentBackgroundTaskInfo.agentId` is optional too).
    #[test]
    fn should_omit_agent_identity_in_background_task_info_when_task_is_process_kind() {
        let bus = Arc::new(EventBus::new(8));
        let manager = TaskManager::new();
        manager.set_event_bus(Arc::clone(&bus));
        let mut rx = bus.subscribe();

        manager.register(
            TaskKind::Process {
                command: Some("echo hi".into()),
                dispatcher: None,
            },
            "build watcher".into(),
            SESSION.into(),
        );

        match rx.try_recv().expect("started event published") {
            SessionEvent::BackgroundTaskStarted { info } => {
                assert_eq!(info.agent_id, None, "process tasks carry no agent identity");
                let value = serde_json::to_value(&info).unwrap();
                assert!(
                    !value.as_object().unwrap().contains_key("agentId"),
                    "the wire JSON omits agentId, got {value}"
                );
            }
            other => panic!("expected BackgroundTaskStarted, got {other:?}"),
        }
    }

    /// Given a running task, when the manager stops it, then the terminated
    /// event maps to the protocol's `killed` status.
    #[test]
    fn should_map_stopped_task_to_killed_status_when_the_manager_stops_it() {
        let bus = Arc::new(EventBus::new(8));
        let manager = TaskManager::new();
        manager.set_event_bus(Arc::clone(&bus));
        let mut rx = bus.subscribe();

        let id = manager.register(
            TaskKind::Question { question_count: 2 },
            "question".into(),
            SESSION.into(),
        );
        assert!(matches!(
            rx.try_recv().expect("started"),
            SessionEvent::BackgroundTaskStarted { .. }
        ));

        manager.stop(&id);

        match rx.try_recv().expect("terminated") {
            SessionEvent::BackgroundTaskTerminated { info } => {
                assert_eq!(info.status, "killed");
                assert_eq!(info.kind, TaskKind::Question { question_count: 2 });
            }
            other => panic!("expected BackgroundTaskTerminated, got {other:?}"),
        }
    }

    /// Given a task that already reached a terminal status, when its status is
    /// overwritten again, then no second terminated event is published —
    /// subscribers observe exactly one termination per started task.
    #[test]
    fn should_not_publish_terminated_twice_when_a_terminal_task_transitions_again() {
        let bus = Arc::new(EventBus::new(8));
        let manager = TaskManager::new();
        manager.set_event_bus(Arc::clone(&bus));
        let mut rx = bus.subscribe();

        let id = manager.register(
            TaskKind::Process {
                command: Some("true".into()),
                dispatcher: None,
            },
            "once".into(),
            SESSION.into(),
        );
        assert!(matches!(
            rx.try_recv().expect("started"),
            SessionEvent::BackgroundTaskStarted { .. }
        ));
        manager.complete(&id, 0);
        assert!(matches!(
            rx.try_recv().expect("first terminated"),
            SessionEvent::BackgroundTaskTerminated { .. }
        ));

        manager.fail(&id, "late failure".into());

        assert!(
            rx.try_recv().is_err(),
            "no second terminated event after a terminal overwrite"
        );
    }

    /// Given several running tasks, when the session closes and stops them
    /// all, then every still-running task publishes one killed termination.
    #[test]
    fn should_publish_terminated_for_each_running_task_when_stopping_all() {
        let bus = Arc::new(EventBus::new(8));
        let manager = TaskManager::new();
        manager.set_event_bus(Arc::clone(&bus));
        let mut rx = bus.subscribe();

        let first = manager.register(
            TaskKind::Process {
                command: Some("watch".into()),
                dispatcher: None,
            },
            "first".into(),
            SESSION.into(),
        );
        let second = manager.register(
            TaskKind::Process {
                command: Some("build".into()),
                dispatcher: None,
            },
            "second".into(),
            SESSION.into(),
        );
        let mut seen_ids = Vec::new();
        for _ in 0..2 {
            assert!(matches!(
                rx.try_recv().expect("started"),
                SessionEvent::BackgroundTaskStarted { .. }
            ));
        }

        manager.stop_all();

        for _ in 0..2 {
            match rx.try_recv().expect("terminated") {
                SessionEvent::BackgroundTaskTerminated { info } => {
                    assert_eq!(info.status, "killed");
                    seen_ids.push(info.task_id);
                }
                other => panic!("expected BackgroundTaskTerminated, got {other:?}"),
            }
        }
        seen_ids.sort();
        let mut expected = vec![first, second];
        expected.sort();
        assert_eq!(seen_ids, expected);
    }

    /// Given a manager with no event bus attached (detached construction,
    /// the default), when the full task lifecycle runs, then nothing panics —
    /// the bypass is strictly optional.
    #[test]
    fn should_not_panic_when_no_event_bus_is_attached() {
        let manager = TaskManager::new();

        let id = manager.register(
            TaskKind::Process {
                command: Some("true".into()),
                dispatcher: None,
            },
            "detached".into(),
            SESSION.into(),
        );
        manager.append_output(&id, "out");
        manager.complete(&id, 0);
        manager.stop_all();
    }

    /// Given a persisted task file written before the kind field existed,
    /// when the manager loads it, then the task survives as a process-kind
    /// task with an unknown command (legacy tasks are terminal after a
    /// restart, so they never emit live events).
    #[test]
    fn should_load_legacy_persisted_tasks_without_kind_as_process_kind() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("tasks");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("task-legacy.json"),
            serde_json::json!({
                "id": "task-legacy",
                "description": "old world",
                "status": {"status": "completed", "detail": {"exit_code": 0}},
                "created_at": "2026-01-01T00:00:00Z",
                "output": "",
                "session_id": SESSION,
            })
            .to_string(),
        )
        .unwrap();

        let manager = TaskManager::with_persistence(Arc::new(TaskPersistence::new(
            dir,
            std::sync::Arc::new(mirri_os::LocalSyncFs::new()),
        )));

        let task = manager.get("task-legacy").expect("legacy task loaded");
        assert_eq!(
            task.kind,
            TaskKind::Process {
                command: None,
                dispatcher: None
            },
            "legacy tasks load as process-kind with an unknown command (they are terminal after a restart, so they never emit live events)"
        );
        assert_eq!(task.ended_at, None);
    }

    // ===== G16: real stop (signal linkage + first-terminal-wins) =====

    /// Given a task with an attached cancel signal, When the task is
    /// stopped, Then the signal fires — `TaskStop` actually aborts the
    /// underlying work instead of only flipping the status.
    #[test]
    fn should_fire_the_attached_signal_when_a_task_is_stopped() {
        let tasks = TaskManager::new();
        let task_id = tasks.register(
            TaskKind::Process {
                command: None,
                dispatcher: None,
            },
            "agent work".to_string(),
            "session-1".to_string(),
        );
        let signal = CancelToken::new();
        tasks.attach_signal(&task_id, signal.clone());
        assert!(!signal.is_cancelled());

        tasks.stop(&task_id);
        assert!(
            signal.is_cancelled(),
            "the stop must fire the attached cancel signal"
        );
        assert_eq!(tasks.get(&task_id).unwrap().status, TaskStatus::Stopped);
    }

    /// Given a task already stopped, When a late natural settle arrives
    /// (the cancelled work's runner reports completion), Then the first
    /// terminal status wins — the "stopped" state is never overwritten
    /// back to completed.
    #[test]
    fn should_keep_the_first_terminal_status_when_a_late_settle_arrives() {
        let tasks = TaskManager::new();
        let task_id = tasks.register(
            TaskKind::Agent {
                agent_id: Some("agent-x".to_string()),
                subagent_type: Some("coder".to_string()),
                parent_tool_call_id: None,
                swarm_index: None,
                model: None,
            },
            "late settle".to_string(),
            "session-1".to_string(),
        );
        tasks.stop(&task_id);
        tasks.complete(&task_id, 0);
        tasks.fail(&task_id, "late failure".to_string());

        assert_eq!(
            tasks.get(&task_id).unwrap().status,
            TaskStatus::Stopped,
            "the stopped terminal status must survive late settles"
        );
    }

    /// Given running tasks with attached signals, When all tasks are
    /// stopped (session close), Then every signal fires.
    #[test]
    fn should_fire_all_attached_signals_when_all_tasks_are_stopped() {
        let tasks = TaskManager::new();
        let first = tasks.register(
            TaskKind::Process {
                command: None,
                dispatcher: None,
            },
            "one".to_string(),
            "session-1".to_string(),
        );
        let second = tasks.register(
            TaskKind::Process {
                command: None,
                dispatcher: None,
            },
            "two".to_string(),
            "session-1".to_string(),
        );
        let signal_a = CancelToken::new();
        let signal_b = CancelToken::new();
        tasks.attach_signal(&first, signal_a.clone());
        tasks.attach_signal(&second, signal_b.clone());

        tasks.stop_all();

        assert!(
            signal_a.is_cancelled(),
            "session close must cancel task one"
        );
        assert!(
            signal_b.is_cancelled(),
            "session close must cancel task two"
        );
    }

    // ===== B1 T2: capacity, per-task timeout, real cancel on expiry =====

    /// Given a manager with max_running_tasks=1 and one running detached
    /// task, When a second background task registers, Then registration is
    /// rejected with the TS parity capacity message — while foreground
    /// registration is unaffected (TS assertCanRegister applies to
    /// detached only).
    #[test]
    fn should_reject_the_second_background_task_when_capacity_is_reached() {
        let tasks = TaskManager::new().with_max_running_tasks(1);
        let first = tasks
            .register_background(
                TaskKind::Process {
                    command: Some("sleep 30".into()),
                    dispatcher: None,
                },
                "one".into(),
                "session-1".into(),
                None,
            )
            .expect("first background task fits the capacity");

        let second = tasks.register_background(
            TaskKind::Process {
                command: None,
                dispatcher: None,
            },
            "two".into(),
            "session-1".into(),
            None,
        );

        assert_eq!(second, Err(TASK_CAPACITY_MESSAGE.to_string()));
        // Foreground registration is unaffected by the capacity limit.
        let fg = tasks.register(
            TaskKind::Process {
                command: None,
                dispatcher: None,
            },
            "foreground".into(),
            "session-1".into(),
        );
        assert!(tasks.get(&fg).is_some());
        // A terminal task releases its slot.
        tasks.complete(&first, 0);
        assert!(
            tasks
                .register_background(
                    TaskKind::Process {
                        command: None,
                        dispatcher: None
                    },
                    "two".into(),
                    "session-1".into(),
                    None
                )
                .is_ok()
        );
    }

    /// Given a manager at capacity, When the pre-spawn capacity query runs,
    /// Then it reports false — the Agent tool consults it BEFORE spawning
    /// so a rejected dispatch never mints a `subagent.spawned` event.
    #[test]
    fn should_report_capacity_through_the_pre_spawn_query() {
        let tasks = TaskManager::new().with_max_running_tasks(1);
        assert!(
            tasks.can_register_background(),
            "an empty manager has capacity"
        );
        tasks
            .register_background(
                TaskKind::Process {
                    command: None,
                    dispatcher: None,
                },
                "occupant".into(),
                "session-1".into(),
                None,
            )
            .expect("the first background task fits");
        assert!(
            !tasks.can_register_background(),
            "the query must mirror the registration check"
        );
        // The unbounded default never reports false.
        assert!(TaskManager::new().can_register_background());
    }

    /// Given a signal-less background spawn (a pending client question),
    /// When it is stopped with grace, Then the stop escalates IMMEDIATELY —
    /// no signal means no in-window settlement is possible, so the grace
    /// wait must not burn — and the released slot is again registerable.
    #[tokio::test]
    async fn should_stop_a_signalless_background_spawn_immediately_and_count_capacity() {
        let tasks: Arc<TaskManager> = Arc::new(TaskManager::new().with_max_running_tasks(1));
        let task_id = tasks
            .clone()
            .spawn_background_detached(
                TaskKind::Question { question_count: 1 },
                "pending question".into(),
                "session-1".into(),
                |_task_id| {
                    Box::pin(async {
                        // A pending question that never answers within the
                        // test's horizon.
                        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                        Ok((0, "answered".to_string(), String::new()))
                    })
                },
            )
            .expect("the signal-less background task fits the capacity");
        assert!(
            !tasks.can_register_background(),
            "the signal-less spawn must consume a capacity slot"
        );

        let started = Instant::now();
        tasks
            .stop_with_grace(&task_id, std::time::Duration::from_secs(5))
            .await;
        assert!(
            started.elapsed() < std::time::Duration::from_secs(1),
            "a signal-less stop must escalate immediately, not burn the \
             grace window (took {:?})",
            started.elapsed()
        );
        assert_eq!(tasks.get(&task_id).unwrap().status, TaskStatus::Stopped);
        assert!(
            tasks.can_register_background(),
            "the terminal task must release its slot"
        );

        // At capacity the next signal-less spawn is rejected outright —
        // the producer must surface the message instead of starting.
        tasks
            .register_background(
                TaskKind::Process {
                    command: None,
                    dispatcher: None,
                },
                "occupant".into(),
                "session-1".into(),
                None,
            )
            .expect("re-occupy the released slot");
        let rejected = tasks.clone().spawn_background_detached(
            TaskKind::Question { question_count: 1 },
            "overflow".into(),
            "session-1".into(),
            |_task_id| Box::pin(async { Ok((0, String::new(), String::new())) }),
        );
        assert_eq!(
            rejected,
            Err(TASK_CAPACITY_MESSAGE.to_string()),
            "the capacity rejection must reach the producer"
        );
    }

    /// Given a task registered with an explicit 50ms timeout and an attached
    /// cancel signal, When terminate_expired runs after the deadline, Then
    /// the signal FIRES and the task lands in Failed(TASK_TIMEOUT_MESSAGE) —
    /// the pre-fix code only flipped the status and never cancelled the
    /// work.
    #[test]
    fn should_fire_the_attached_signal_when_terminate_expired_kills_a_timed_out_task() {
        let tasks = TaskManager::new();
        let id = tasks.register_with_timeout(
            TaskKind::Agent {
                agent_id: Some("agent-x".into()),
                subagent_type: Some("coder".into()),
                parent_tool_call_id: None,
                swarm_index: None,
                model: None,
            },
            "stuck".into(),
            "session-1".into(),
            Some(Duration::from_millis(50)),
        );
        let signal = CancelToken::new();
        tasks.attach_signal(&id, signal.clone());

        std::thread::sleep(Duration::from_millis(80));
        let terminated = tasks.terminate_expired();

        assert_eq!(terminated, vec![id.clone()]);
        assert!(
            signal.is_cancelled(),
            "timeout must cancel the underlying work"
        );
        assert_eq!(
            tasks.get(&id).unwrap().status,
            TaskStatus::Failed(TASK_TIMEOUT_MESSAGE.to_string())
        );
    }

    /// Given a task registered with an explicit timeout, When
    /// terminate_expired runs before the deadline, Then nothing is
    /// terminated (lazy deadline — queued-in-buffered tasks don't burn
    /// budget).
    #[test]
    fn should_not_terminate_a_task_when_its_explicit_deadline_has_not_elapsed() {
        let tasks = TaskManager::new();
        tasks.register_with_timeout(
            TaskKind::Process {
                command: None,
                dispatcher: None,
            },
            "fresh".into(),
            "session-1".into(),
            Some(Duration::from_secs(60)),
        );
        assert!(tasks.terminate_expired().is_empty());
    }

    /// Given an explicit per-task deadline and a conflicting global agent
    /// timeout, When terminate_expired evaluates expiry, Then the explicit
    /// deadline wins (the global timeout is only the fallback for tasks
    /// registered without one).
    #[test]
    fn should_prefer_the_explicit_deadline_over_the_global_agent_timeout() {
        // Global timeout long expired (the task's created_at is in the
        // past), but the explicit deadline is still ahead — not expired.
        let tasks = TaskManager::new().with_agent_timeout(Duration::from_millis(1));
        tasks.register_with_timeout(
            TaskKind::Process {
                command: None,
                dispatcher: None,
            },
            "explicit wins".into(),
            "session-1".into(),
            Some(Duration::from_secs(60)),
        );
        std::thread::sleep(Duration::from_millis(20));
        assert!(
            tasks.terminate_expired().is_empty(),
            "the explicit (unexpired) deadline must suppress the global fallback"
        );
    }

    /// Given a task registered and stamped through the lifetime-exemption
    /// setter, when the row is read back, then it carries `lifetime_exempt`
    /// (US14.4: the exemption must be visible to the resident sweeper
    /// through the archive) — and the stamp is idempotent, never an error
    /// and never a re-registration.
    #[test]
    fn should_carry_the_lifetime_exemption_when_stamped_after_registration() {
        let tasks = TaskManager::new();
        let id = tasks.register(
            TaskKind::Process {
                command: None,
                dispatcher: None,
            },
            "exempt long job".into(),
            "session-1".into(),
        );
        tasks.set_lifetime_exempt(&id);
        assert!(
            tasks
                .get(&id)
                .expect("the task is registered")
                .lifetime_exempt
        );

        // Idempotent: a second stamp is a no-op.
        tasks.set_lifetime_exempt(&id);
        assert!(tasks.get(&id).unwrap().lifetime_exempt);
    }

    /// Given a keepAlive session close, when the running tasks are stamped,
    /// then every still-running row becomes `lifetime_exempt` while terminal
    /// rows keep their own fate (SDR-013 d — the surviving tasks must not
    /// fall to the resident lifetime sweeper).
    #[test]
    fn should_exempt_every_running_task_when_the_keepalive_close_stamps_them() {
        let tasks = TaskManager::new();
        let running = tasks.register(
            TaskKind::Process {
                command: None,
                dispatcher: None,
            },
            "survivor".into(),
            "session-1".into(),
        );
        let done = tasks.register(
            TaskKind::Process {
                command: None,
                dispatcher: None,
            },
            "finished".into(),
            "session-1".into(),
        );
        tasks.complete(&done, 0);

        tasks.mark_all_running_lifetime_exempt();

        assert!(tasks.get(&running).unwrap().lifetime_exempt);
        // The terminal row was never stamped — its archive keeps the
        // default, and re-loading it must not resurrect an exemption.
        assert!(!tasks.get(&done).unwrap().lifetime_exempt);
    }

    /// Given tasks persisted before the deadline field existed, when one is
    /// reloaded from disk, then it deserializes with no deadline (the field
    /// is in-memory only and never persisted).
    #[test]
    fn should_load_persisted_tasks_without_a_deadline_field() {
        let task: AgentTask = serde_json::from_value(serde_json::json!({
            "id": "task-old",
            "description": "pre-deadline",
            "kind": { "kind": "process" },
            "status": { "status": "completed", "detail": { "exit_code": 0 } },
            "created_at": "2026-01-01T00:00:00Z",
            "output": "",
            "session_id": "session-1"
        }))
        .expect("pre-deadline persistence file deserializes");
        assert_eq!(task.deadline, None);
    }

    /// Given a persisted running task WITH a process fingerprint, when the
    /// archive is loaded after a restart, then the task stays Running — the
    /// startup orphan sweep (not the loader) owns its verdict (ADR-012 c
    /// "census first").
    #[test]
    fn should_keep_a_running_fingerprinted_task_when_loaded_after_restart() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("tasks");
        let persistence = Arc::new(TaskPersistence::new(
            dir.clone(),
            Arc::new(mirri_os::LocalSyncFs::new()),
        ));
        let mut task = running_process_task("task-fp1");
        task.fingerprint = Some(mirri_os::process::ProcessFingerprint {
            pid: 1,
            pgid: 1,
            start_ticks: 1,
        });
        persistence.save_task(&task).unwrap();

        let manager = TaskManager::with_persistence(persistence);

        assert_eq!(
            manager.get("task-fp1").unwrap().status,
            TaskStatus::Running,
            "fingerprinted running tasks are NOT reconciled at load — the sweep owns the verdict"
        );
        // And the archive on disk is untouched too (no write-back).
        let on_disk: AgentTask =
            serde_json::from_str(&std::fs::read_to_string(dir.join("task-fp1.json")).unwrap())
                .unwrap();
        assert_eq!(on_disk.status, TaskStatus::Running);
    }

    /// Given a persisted running task WITHOUT a fingerprint (a legacy or
    /// in-process-kind archive), when the archive is loaded after a
    /// restart, then the task is reconciled to Stopped — the pre-E14
    /// behavior is preserved for archives the sweep cannot verify.
    #[test]
    fn should_reconcile_a_running_legacy_task_when_loaded_after_restart() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("tasks");
        let persistence = Arc::new(TaskPersistence::new(
            dir.clone(),
            Arc::new(mirri_os::LocalSyncFs::new()),
        ));
        persistence
            .save_task(&running_process_task("task-legacy"))
            .unwrap();

        let manager = TaskManager::with_persistence(persistence);

        assert_eq!(
            manager.get("task-legacy").unwrap().status,
            TaskStatus::Stopped
        );
    }

    /// A running process-kind task with no fingerprint, ready to persist.
    fn running_process_task(id: &str) -> AgentTask {
        AgentTask {
            id: id.to_string(),
            kind: TaskKind::Process {
                command: Some("sleep 300".to_string()),
                dispatcher: None,
            },
            description: "long job".to_string(),
            status: TaskStatus::Running,
            created_at: Utc::now(),
            ended_at: None,
            output: String::new(),
            session_id: "session-1".to_string(),
            deadline: None,
            detached: true,
            notification_suppressed: false,
            notification_delivered_at: None,
            fingerprint: None,
            lifetime_exempt: false,
        }
    }

    /// A manager whose tasks persist under a tempdir — the B2 notification
    /// persistence tests' shared bed.
    fn persisted_manager() -> (Arc<TaskManager>, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let persistence = Arc::new(TaskPersistence::new(
            tmp.path().join("tasks"),
            Arc::new(mirri_os::LocalSyncFs::new()),
        ));
        (Arc::new(TaskManager::with_persistence(persistence)), tmp)
    }

    /// Given foreground and background registrations, when reading them
    /// back, then only the background ones are `detached` — the flag the
    /// notification loop filters foreground tasks out with (TS `detached`).
    #[test]
    fn should_mark_a_background_registration_as_detached() {
        let tasks = TaskManager::new();
        let bg = tasks
            .register_background(
                TaskKind::Process {
                    command: None,
                    dispatcher: None,
                },
                "bg".into(),
                "s".into(),
                None,
            )
            .unwrap();
        let fg = tasks.register(
            TaskKind::Process {
                command: None,
                dispatcher: None,
            },
            "fg".into(),
            "s".into(),
        );
        assert!(
            tasks.get(&bg).unwrap().detached,
            "register_background entries are detached"
        );
        assert!(
            !tasks.get(&fg).unwrap().detached,
            "plain register stays foreground"
        );
    }

    /// Given a detached registration, when the terminal notification is
    /// suppressed, then the persisted record carries the flag — a restart
    /// keeps the suppression (G25's durability).
    #[tokio::test]
    async fn should_persist_the_suppression_flag_when_it_is_set() {
        let (tasks, tmp) = persisted_manager();
        let id = tasks
            .register_background(
                TaskKind::Process {
                    command: None,
                    dispatcher: None,
                },
                "bg".into(),
                "s".into(),
                None,
            )
            .unwrap();
        tasks.suppress_terminal_notification(&id);
        // Then the persisted record carries the flag (restart keeps it)
        assert!(tasks.get(&id).unwrap().notification_suppressed);
        let reloaded = TaskManager::with_persistence(Arc::new(TaskPersistence::new(
            tmp.path().join("tasks"),
            Arc::new(mirri_os::LocalSyncFs::new()),
        )));
        assert!(reloaded.get(&id).unwrap().notification_suppressed);
    }

    /// Given a completed detached task, when its terminal notification is
    /// marked durably delivered, then the persisted record carries the
    /// marker — a rebuilt context (resume, compaction, subagent journal
    /// replay) never re-arms delivery (SDR-B2-01).
    #[tokio::test]
    async fn should_persist_the_delivery_marker_when_a_terminal_notification_is_marked_delivered() {
        let (tasks, tmp) = persisted_manager();
        let id = tasks
            .register_background(
                TaskKind::Process {
                    command: None,
                    dispatcher: None,
                },
                "bg".into(),
                "s".into(),
                None,
            )
            .unwrap();
        tasks.complete(&id, 0);
        tasks.mark_notification_delivered(&id);
        // Then the in-memory record carries the marker…
        assert!(tasks.get(&id).unwrap().notification_delivered_at.is_some());
        // …and the reloaded record keeps it (restart never re-delivers).
        let reloaded = TaskManager::with_persistence(Arc::new(TaskPersistence::new(
            tmp.path().join("tasks"),
            Arc::new(mirri_os::LocalSyncFs::new()),
        )));
        assert!(
            reloaded
                .get(&id)
                .unwrap()
                .notification_delivered_at
                .is_some()
        );
    }

    /// Given a durably-delivered task, when the delivery marker is cleared
    /// (the undo path), then the persisted record loses the marker — the
    /// notification re-arms for delivery.
    #[tokio::test]
    async fn should_clear_a_persisted_delivery_marker_when_undo_re_arms_the_notification() {
        let (tasks, tmp) = persisted_manager();
        let id = tasks
            .register_background(
                TaskKind::Process {
                    command: None,
                    dispatcher: None,
                },
                "bg".into(),
                "s".into(),
                None,
            )
            .unwrap();
        tasks.complete(&id, 0);
        tasks.mark_notification_delivered(&id);
        tasks.clear_notification_delivered(&id);
        // Then the in-memory record loses the marker…
        assert!(tasks.get(&id).unwrap().notification_delivered_at.is_none());
        // …and the reloaded record confirms the clear (undo re-arms).
        let reloaded = TaskManager::with_persistence(Arc::new(TaskPersistence::new(
            tmp.path().join("tasks"),
            Arc::new(mirri_os::LocalSyncFs::new()),
        )));
        assert!(
            reloaded
                .get(&id)
                .unwrap()
                .notification_delivered_at
                .is_none()
        );
    }

    /// Given one running and one completed background task, when all active
    /// tasks are suppressed (the session-close / print-drain path), then
    /// only the still-running one is flagged — a completed task's
    /// notification debt is its own.
    #[test]
    fn should_suppress_only_running_tasks_when_suppressing_all_active() {
        let tasks = TaskManager::new();
        let running = tasks
            .register_background(
                TaskKind::Process {
                    command: None,
                    dispatcher: None,
                },
                "r".into(),
                "s".into(),
                None,
            )
            .unwrap();
        let done = tasks
            .register_background(
                TaskKind::Process {
                    command: None,
                    dispatcher: None,
                },
                "d".into(),
                "s".into(),
                None,
            )
            .unwrap();
        tasks.complete(&done, 0);
        tasks.suppress_all_active();
        assert!(tasks.get(&running).unwrap().notification_suppressed);
        assert!(!tasks.get(&done).unwrap().notification_suppressed);
    }

    /// Given a pre-B2 persistence file (no `detached` key), when it is
    /// loaded, then the task reads detached — legacy records keep getting
    /// notifications (TS parity: only an explicit `detached: false` marks
    /// foreground).
    #[test]
    fn should_load_a_pre_b2_task_as_detached() {
        let task: AgentTask = serde_json::from_value(serde_json::json!({
            "id": "task-old",
            "description": "pre-b2",
            "kind": { "kind": "process" },
            "status": { "status": "completed", "detail": { "exit_code": 0 } },
            "created_at": "2026-01-01T00:00:00Z",
            "output": "",
            "session_id": "session-1"
        }))
        .expect("pre-B2 persistence file deserializes");
        assert!(task.detached, "legacy records load as detached");
        assert!(!task.notification_suppressed);
    }

    // ===== B7: foreground registration + release permit (D-3) =====

    fn foreground_bus() -> (
        TaskManager,
        Arc<EventBus>,
        tokio::sync::broadcast::Receiver<SessionEvent>,
    ) {
        let bus = Arc::new(EventBus::new(8));
        let manager = TaskManager::new();
        manager.set_event_bus(Arc::clone(&bus));
        let rx = bus.subscribe();
        (manager, bus, rx)
    }

    /// Given a foreground registration, when the task is listed, then it
    /// appears with detached=false and WITHOUT publishing
    /// BackgroundTaskStarted (the started event belongs to the detached
    /// lifecycle — the conversion publishes it, TS detachEntry parity).
    #[tokio::test]
    async fn should_register_a_foreground_task_without_the_started_event() {
        let (tasks, _bus, mut rx) = foreground_bus();

        let reg = tasks.register_foreground(
            TaskKind::Process {
                command: Some("sleep 5".into()),
                dispatcher: None,
            },
            "sleep 5".into(),
            "s-1".into(),
            Some("toolu-1".into()),
        );

        let task = tasks.get(&reg.task_id).expect("registered");
        assert!(
            !task.detached,
            "a foreground registration is detached=false"
        );
        assert_eq!(task.status, TaskStatus::Running);
        assert_eq!(task.session_id, "s-1");
        assert!(
            rx.try_recv().is_err(),
            "no BackgroundTaskStarted may fire at registration"
        );
    }

    /// Given a running foreground task, when detach_foreground fires, then
    /// the task flips detached, a BackgroundTaskStarted is published, the
    /// release resolves Detached, and the id now occupies a background slot.
    #[tokio::test]
    async fn should_flip_detached_and_publish_started_when_a_foreground_task_is_detached() {
        let (tasks, _bus, mut rx) = foreground_bus();
        let tasks = tasks.with_max_running_tasks(1);
        let reg = tasks.register_foreground(
            TaskKind::Process {
                command: Some("sleep 5".into()),
                dispatcher: None,
            },
            "sleep 5".into(),
            "s-1".into(),
            Some("toolu-1".into()),
        );
        let task_id = reg.task_id.clone();
        // The awaiter arms BEFORE the detach fires (the stored-permit race
        // is pinned by the terminal-side test instead).
        let release = tokio::spawn(async move { reg.released().await });

        let detached = tasks
            .detach_foreground(&task_id)
            .expect("conversion happens");

        assert!(detached.detached, "the converted task is detached");
        assert!(tasks.get(&task_id).unwrap().detached);
        match rx.try_recv().expect("started event published") {
            SessionEvent::BackgroundTaskStarted { info } => {
                assert_eq!(info.task_id, task_id);
                assert_eq!(info.status, "running");
            }
            other => panic!("expected BackgroundTaskStarted, got {other:?}"),
        }
        assert!(
            rx.try_recv().is_err(),
            "exactly one started event for the conversion"
        );
        assert_eq!(
            release.await.unwrap(),
            ForegroundReleaseReason::Detached,
            "the release resolves Detached"
        );
        // The converted id now occupies a background capacity slot.
        assert!(
            !tasks.can_register_background(),
            "the detached task must count toward the capacity limit"
        );
    }

    /// Given a detached-again call, when detach_foreground runs twice, then
    /// the second call answers None and no duplicate event fires.
    #[tokio::test]
    async fn should_answer_none_when_detaching_an_already_detached_task() {
        let (tasks, _bus, mut rx) = foreground_bus();
        let reg = tasks.register_foreground(
            TaskKind::Process {
                command: Some("sleep 5".into()),
                dispatcher: None,
            },
            "sleep 5".into(),
            "s-1".into(),
            None,
        );
        assert!(tasks.detach_foreground(&reg.task_id).is_some());
        assert!(
            rx.try_recv().is_ok(),
            "the first conversion publishes the started event"
        );

        assert!(
            tasks.detach_foreground(&reg.task_id).is_none(),
            "a second detach answers None"
        );
        assert!(
            rx.try_recv().is_err(),
            "no duplicate started event for the second detach"
        );
        // An unknown id answers None the same way.
        assert!(tasks.detach_foreground("task-nope").is_none());
    }

    /// Given the tool-call addressing face, when
    /// detach_foreground_by_tool_call runs, then the same conversion
    /// happens for the registered tool call id — and an unknown or settled
    /// tool call id answers None.
    #[tokio::test]
    async fn should_detach_by_tool_call_id() {
        let (tasks, _bus, _rx) = foreground_bus();
        let reg = tasks.register_foreground(
            TaskKind::Process {
                command: Some("sleep 5".into()),
                dispatcher: None,
            },
            "sleep 5".into(),
            "s-1".into(),
            Some("toolu-1".into()),
        );

        let detached = tasks
            .detach_foreground_by_tool_call("toolu-1")
            .expect("the tool call id addresses the foreground task");
        assert_eq!(detached.id, reg.task_id);
        assert!(tasks.get(&reg.task_id).unwrap().detached);

        // Unknown and settled tool call ids answer None.
        assert!(
            tasks
                .detach_foreground_by_tool_call("toolu-unknown")
                .is_none()
        );
        let settled = tasks.register_foreground(
            TaskKind::Process {
                command: Some("true".into()),
                dispatcher: None,
            },
            "true".into(),
            "s-1".into(),
            Some("toolu-2".into()),
        );
        tasks.complete(&settled.task_id, 0);
        assert!(
            tasks.detach_foreground_by_tool_call("toolu-2").is_none(),
            "a settled task no longer converts"
        );
    }

    /// Given a task that settles first, when the release is awaited, then
    /// it resolves Terminal and the release table is pruned — a stored
    /// permit still wins for a late awaiter, and the conversion no longer
    /// applies.
    #[tokio::test]
    async fn should_resolve_terminal_and_prune_the_release_when_the_task_settles_first() {
        let (tasks, _bus, _rx) = foreground_bus();
        let reg = tasks.register_foreground(
            TaskKind::Process {
                command: Some("true".into()),
                dispatcher: None,
            },
            "true".into(),
            "s-1".into(),
            Some("toolu-1".into()),
        );
        let task_id = reg.task_id.clone();

        // The task settles BEFORE anyone awaits the release.
        tasks.complete(&task_id, 0);
        // A late awaiter still resolves: the stored permit wins.
        assert_eq!(
            reg.released().await,
            ForegroundReleaseReason::Terminal,
            "the release resolves Terminal"
        );
        assert!(
            tasks.detach_foreground(&task_id).is_none(),
            "the pruned release entry means no conversion"
        );
        assert!(
            tasks.detach_foreground_by_tool_call("toolu-1").is_none(),
            "the tool-call index entry is pruned with it"
        );
    }

    #[tokio::test]
    async fn should_return_cancelled_immediately_when_the_wait_is_cancelled() {
        // Given a still-running task and a cancel token that fires while a
        // blocking wait is in flight
        let manager = TaskManager::new();
        let id = manager.register(
            TaskKind::Process {
                command: None,
                dispatcher: None,
            },
            "build".into(),
            SESSION.into(),
        );
        let cancel = crate::cancel::CancelToken::new();
        let cancel_clone = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(150)).await;
            cancel_clone.cancel();
        });

        // When the wait runs with a 10-second timeout (far longer than the
        // cancel)
        let start = std::time::Instant::now();
        let result = manager
            .wait_for_completion_with(&id, 10, Some(&cancel))
            .await;

        // Then the wait ends promptly with Cancelled — not after the task
        // settles nor after the timeout (US5.3: no zombie window)
        assert_eq!(result, WaitResult::Cancelled);
        assert!(
            start.elapsed() < std::time::Duration::from_secs(5),
            "cancel must end the wait promptly, took {:?}",
            start.elapsed()
        );
        // And the task itself is untouched — still running
        assert_eq!(manager.get(&id).unwrap().status, TaskStatus::Running);
    }

    #[tokio::test]
    async fn should_report_liveness_only_while_the_heartbeat_advances() {
        // Given a manager with a heartbeat interval and a liveness listener
        // that counts invocations
        let manager = TaskManager::new().with_heartbeat_interval(Duration::from_millis(200));
        let id = manager.register(
            TaskKind::Process {
                command: None,
                dispatcher: None,
            },
            "healthy".into(),
            SESSION.into(),
        );
        let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count_clone = Arc::clone(&count);
        manager.set_liveness_listener(Some(Arc::new(move |_task_id| {
            count_clone.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        })));

        // When a short wait runs while the heartbeat keeps advancing
        let manager = Arc::new(manager);
        let id_for_beats = id.clone();
        let manager_for_beats = Arc::clone(&manager);
        tokio::spawn(async move {
            for _ in 0..5 {
                tokio::time::sleep(Duration::from_millis(100)).await;
                manager_for_beats.heartbeat(&id_for_beats);
            }
        });
        manager.wait_for_completion(&id, 1).await;

        // Then the liveness listener fired for the advancing heartbeats
        // (US5.5 acceptance 1: a progressing task is not judged stalled)
        assert!(
            count.load(std::sync::atomic::Ordering::SeqCst) >= 2,
            "advancing heartbeats must report liveness, got {}",
            count.load(std::sync::atomic::Ordering::SeqCst)
        );
    }

    #[tokio::test]
    async fn should_stop_reporting_liveness_when_the_heartbeat_goes_silent() {
        // Given a task whose heartbeat never advances after registration
        // (the producing process died without settling)
        let manager = TaskManager::new().with_heartbeat_interval(Duration::from_millis(100));
        let id = manager.register(
            TaskKind::Process {
                command: None,
                dispatcher: None,
            },
            "silent".into(),
            SESSION.into(),
        );
        let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count_clone = Arc::clone(&count);
        manager.set_liveness_listener(Some(Arc::new(move |_task_id| {
            count_clone.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        })));

        // When a blocking wait runs past the heartbeat window with no
        // heartbeats at all (the registration seed is the last one — but
        // it predates the wait, so it never counts as "advanced during
        // the wait")
        manager.wait_for_completion(&id, 1).await;

        // Then the liveness listener never fired — a silent task is not
        // alive, and the turn's stall watchdog is left to expire
        // (US5.5 acceptance 2: the true-stall backstop stays effective)
        assert_eq!(
            count.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "a silent heartbeat must not report liveness"
        );
    }

    #[tokio::test]
    async fn should_prefer_the_settlement_when_the_cancel_lands_after_a_terminal_state() {
        // Given a task that has ALREADY settled when the cancel fires, with
        // both states visible before the wait's next poll (the tight race:
        // the loop-top check must not return Cancelled for a settled task)
        let manager = Arc::new(TaskManager::new());
        let id = manager.register(
            TaskKind::Process {
                command: None,
                dispatcher: None,
            },
            "settled-then-cancelled".into(),
            SESSION.into(),
        );
        manager.complete(&id, 0);
        let cancel = crate::cancel::CancelToken::new();
        cancel.cancel();

        // When the wait runs with the cancel already fired
        let result = manager
            .wait_for_completion_with(&id, 10, Some(&cancel))
            .await;

        // Then the settlement wins — the waiter saw the real outcome
        assert_eq!(result, WaitResult::Completed);
    }

    #[tokio::test]
    async fn should_prefer_the_settlement_when_it_races_the_cancel() {
        // Given a task that settles at the same moment the cancel fires
        let manager = TaskManager::new();
        let id = manager.register(
            TaskKind::Process {
                command: None,
                dispatcher: None,
            },
            "racy".into(),
            SESSION.into(),
        );
        let cancel = crate::cancel::CancelToken::new();
        let manager = Arc::new(manager);
        let manager_ref = Arc::clone(&manager);
        let id_clone = id.clone();
        let cancel_clone = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            manager_ref.complete(&id_clone, 0);
            cancel_clone.cancel();
        });

        // When the wait runs
        let result = manager
            .wait_for_completion_with(&id, 10, Some(&cancel))
            .await;

        // Then the observed settlement wins over the cancel — the waiter
        // saw the real outcome, not the interruption
        assert_eq!(result, WaitResult::Completed);
    }

    #[tokio::test]
    async fn should_report_liveness_every_poll_when_heartbeat_detection_is_disabled() {
        // Given a manager with no heartbeat interval (liveness detection
        // disabled — the plain-manager mode) and a wait on a running task
        let manager = TaskManager::new();
        let id = manager.register(
            TaskKind::Process {
                command: None,
                dispatcher: None,
            },
            "plain".into(),
            SESSION.into(),
        );
        let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count_clone = Arc::clone(&count);
        manager.set_liveness_listener(Some(Arc::new(move |_task_id| {
            count_clone.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        })));

        // When the wait times out quickly
        manager.wait_for_completion(&id, 1).await;

        // Then liveness was reported during the wait — without heartbeat
        // tracking the wait is bounded by its own timeout, so every poll
        // of a present task counts as alive
        assert!(
            count.load(std::sync::atomic::Ordering::SeqCst) > 0,
            "no-heartbeat mode must still report liveness while the task exists"
        );
    }

    /// Given a manager with a heartbeat interval and a spawned Process task
    /// whose future stays pending (a long silent command, e.g. a full cargo
    /// build emitting no output), when the heartbeat window elapses far past
    /// the interval, then the task's heartbeat has kept advancing — the
    /// liveness listener of a waiting poll loop keeps firing. Pre-fix,
    /// nothing refreshed the heartbeat while the process ran, so a silent
    /// command was reported stale and starved the stall-watchdog feed of
    /// any turn waiting on it.
    #[tokio::test(start_paused = true)]
    async fn should_keep_a_spawned_process_task_heartbeat_advancing_while_its_future_is_pending() {
        use std::time::Duration;

        let manager =
            Arc::new(TaskManager::new().with_heartbeat_interval(Duration::from_millis(200)));
        let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count_clone = Arc::clone(&count);
        manager.set_liveness_listener(Some(Arc::new(move |_task_id| {
            count_clone.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        })));
        let task_id = manager.spawn_with_signal(
            TaskKind::Process {
                command: Some("sleep-long".into()),
                dispatcher: None,
            },
            "silent build".to_string(),
            "session-1".to_string(),
            |cancel| {
                // A pending future that only resolves on cancellation — a
                // process that runs far longer than the heartbeat window.
                Box::pin(async move {
                    cancel.cancelled().await;
                    Ok((0, String::new(), String::new()))
                })
            },
        );

        // When simulated time advances well past several heartbeat windows
        // while a wait polls the still-pending task (the ticker beats at
        // window/4 = 50ms, the poll loop at 50ms). Paused time needs an
        // explicit driver, or the poll loop would never reach its deadline.
        let driver_manager = Arc::clone(&manager);
        let driver_id = task_id.clone();
        let driver = tokio::spawn(async move {
            // 200 steps x 25ms = 5s of simulated time > the 3s wait timeout.
            for _ in 0..200 {
                tokio::time::advance(Duration::from_millis(25)).await;
                tokio::task::yield_now().await;
                if matches!(
                    driver_manager
                        .get(&driver_id)
                        .map(|task| task.status.is_terminal()),
                    Some(true)
                ) {
                    return;
                }
            }
        });
        let result = manager.wait_for_completion_with(&task_id, 3, None).await;
        assert_eq!(
            result,
            WaitResult::TimedOut,
            "the pending future must outlive the wait"
        );

        // Then the wait kept reporting liveness — the spawned task's
        // heartbeat kept advancing while its future was pending
        assert!(
            count.load(std::sync::atomic::Ordering::SeqCst) >= 2,
            "a live process task must keep its heartbeat advancing while its future is pending, \
             liveness reports: {}",
            count.load(std::sync::atomic::Ordering::SeqCst)
        );

        // Cleanup: cancel and settle so the spawned wrapper exits cleanly
        manager.stop(&task_id);
        tokio::time::sleep(Duration::from_millis(50)).await;
        let _ = driver.await;
    }
}
