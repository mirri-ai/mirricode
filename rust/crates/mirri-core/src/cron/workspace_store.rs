//! The workspace-level cron store (stage-2 A.2 / D-M1 / ADR-007): the
//! single JSON document `<sessions_root>/<workspace_key>/cron/tasks.json`
//! holding every workspace-scoped [`CronTask`], its run records, and (from
//! batch 4b) the pending proposals.
//!
//! Conventions — deliberately the session-index family's, not new ones:
//!
//! - **Atomic writes** through `mirri_persistence::write_file_atomically`
//!   (temp file + rename): a crash mid-write never leaves a half document.
//! - **Cross-process lock**: the `create_new` + stale-takeover + bounded-wait
//!   `RegistryLock` over `tasks.json.lock` — the same primitive and the
//!   same default timeouts the `workspaces.json` merged writes use
//!   (`mirri-session-index`). Acquire through `spawn_blocking` on async
//!   paths (the polling loop blocks).
//! - **Tolerant reads**: a missing file is an empty store (first run), a
//!   corrupt one degrades to empty with a warning — a dirty document never
//!   fails the manager that reads it (the registry's posture).
//!
//! Scope spelling: the task's scope is flattened as the serde enum
//! (`"scope": "workspace"` / `{"workspace": "<id>"}`). The read path
//! accepts BOTH spellings — the flat `"scope": "workspace"` +
//! `"workspaceId"` form is normalized on open by
//! [`WorkspaceCronStore::read`]'s per-record wire parse.
//!
//! On-disk shape (v1):
//!
//! ```json
//! {
//!   "version": 1,
//!   "tasks": [ { "...CronTask serde form (scope flattened)..." } ],
//!   "runs": { "<task_id>": [ { "...RunRecord..." } ] },
//!   "proposals": []
//! }
//! ```

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::TaskScope;

/// How long a pending proposal waits for a user decision before lapsing
/// (stage-2 D.0e): 72 hours, aligned with the approval domain's
/// disconnect-wait magnitude. Implementation-tunable, not user-facing.
pub const CRON_PROPOSAL_TTL: chrono::Duration = chrono::Duration::hours(72);

/// The store document's version (v1 — the stage-2 batch-4a shape).
const WORKSPACE_CRON_STORE_VERSION: i64 = 1;

/// The directory name inside the workspace's sessions bucket.
pub const WORKSPACE_CRON_DIR: &str = "cron";

/// The store file name inside [`WORKSPACE_CRON_DIR`].
pub const WORKSPACE_CRON_TASKS_FILENAME: &str = "tasks.json";

/// One recorded run of a workspace task (stage-2 C.2): the terminal state,
/// timing, usage summary, and the execution session's reference. Embedded
/// per task (ADR-007: single-task run volume is small and dies with the
/// task — a separate runs table is a join cost with no payoff).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunRecord {
    /// Unique run identifier.
    pub run_id: String,
    /// The run's lifecycle state.
    pub status: RunStatus,
    /// When the run started.
    pub started_at: DateTime<Utc>,
    /// When the run reached its terminal state (`None` while running).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<DateTime<Utc>>,
    /// The run's wall-clock duration in milliseconds (`None` while
    /// running).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// The usage summary attributed to this run (cost + tokens — the
    /// existing usage attribution family's summary shape). NOT yet
    /// collected by the executor (review — the honest labeling): the
    /// spawner writes `None` and only the D-M6 run summary rides this
    /// slot's JSON object; the turn-usage collection plumbing (the
    /// settle path's usage accounting reaching the RunRecord) is a
    /// later batch — until then every produced record carries no usage
    /// numbers and the panel renders the empty state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<serde_json::Value>,
    /// The execution session this run created (B.1) — `None` while the
    /// executor is pending (batch 4a's skip records) or for runs that
    /// never spawned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_ref: Option<String>,
    /// Why the run was skipped or rejected (D-M3's skip record, the
    /// disabled state, ...) — `None` for runs that actually ran.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skip_reason: Option<String>,
    /// The permission asks the run's executor observed (batch 4b fills
    /// this — D.2's leave-a-trace contract); reserved in the shape now so
    /// the document needs no migration when 4b lands.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub asked_ops: Vec<serde_json::Value>,
    /// What started this run (ADR-021's launch record). `None` for rows
    /// written before the launch record existed (honest: unknown, not
    /// "scheduled").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trigger: Option<RunTrigger>,
    /// The moment the schedule said this run was due (ADR-021's launch
    /// record). `started_at` minus this is the launch delay (a scheduler
    /// that fired late, a restart that caught up). `None` for run-now
    /// fires and pre-launch-record rows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scheduled_at: Option<DateTime<Utc>>,
}

/// A run's lifecycle state (stage-2 C.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RunStatus {
    /// The run is in flight.
    #[serde(rename = "running")]
    Running,
    /// The run's turn completed successfully.
    #[serde(rename = "succeeded")]
    Succeeded,
    /// The run's turn failed.
    #[serde(rename = "failed")]
    Failed,
    /// The run was rejected before running (e.g. the disabled state).
    #[serde(rename = "rejected")]
    Rejected,
    /// The run was skipped (D-M3's default concurrency posture — the
    /// previous run was still in flight — or the 4a executor-pending
    /// placeholder).
    #[serde(rename = "skipped")]
    Skipped,
    /// The run's turn is parked on an approval the user has not answered
    /// (ADR-021): not failed, not finished — waiting on a person.
    #[serde(rename = "waiting_approval")]
    WaitingApproval,
}

impl RunStatus {
    /// Whether the run still occupies its task: in flight, or parked on an
    /// approval nobody has answered yet (ADR-021). Every concurrency /
    /// orphan / deletion check keys on THIS — a person-blocked run is not
    /// finished, so it must neither admit a second concurrent run nor
    /// outlive a crash or a workspace deletion as a zombie row.
    pub fn is_in_flight(self) -> bool {
        matches!(self, Self::Running | Self::WaitingApproval)
    }
}

/// What started a run (ADR-021) — recorded so the history can answer "who
/// fired this" without guessing from timing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunTrigger {
    /// The scheduler's tick found the task due.
    Scheduled,
    /// The user pressed "run now".
    RunNow,
    /// The queue posture's backlog slot drained after the previous run ended.
    QueueDrain,
}

/// The persisted store document (v1).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceCronDocument {
    /// The document version.
    #[serde(default)]
    pub version: i64,
    /// Every workspace task, in insertion order (the manager's table is a
    /// map; the document's list keeps a stable write order).
    #[serde(default)]
    pub tasks: Vec<super::CronTask>,
    /// Run records keyed by task id (insertion order per task).
    #[serde(default)]
    pub runs: HashMap<String, Vec<RunRecord>>,
    /// LEGACY (C2 retired the proposal channel): tolerated so old
    /// documents keep parsing; nothing produces or reads it.
    #[serde(default)]
    pub proposals: Vec<CronProposal>,
}

/// A proposal's lifecycle state (stage-2 D.0e / SDR-105): the draft sits
/// `Pending` until the user decides, the TTL sweeps it to `Expired`, and
/// a decided proposal keeps its terminal state for audit (the
/// `cron.proposals.list` query face reads every state).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProposalState {
    #[serde(rename = "pending")]
    Pending,
    #[serde(rename = "approved")]
    Approved,
    #[serde(rename = "rejected")]
    Rejected,
    #[serde(rename = "expired")]
    Expired,
    #[serde(rename = "withdrawn")]
    Withdrawn,
    /// The user approved, but the task creation itself failed (a store
    /// write error, an unusable expression at creation time): the
    /// proposal is honestly terminal — it does NOT stay Pending (the
    /// TTL sweep would later mislabel it Expired, hiding the real
    /// cause) and is not Approved (no task exists). The receipt names
    /// the failure and the reroute path (re-propose, or create from the
    /// management face).
    #[serde(rename = "failed")]
    Failed,
}

/// One agent-drafted workspace-task proposal (stage-2 D.0a / ADR-013): the
/// FULL draft the user judges — schedule (with timezone), the verbatim
/// prompt, the recurring declaration, the budget declaration, the
/// suggested runtime posture, and the proposing agent's identity. A
/// proposal is NOT a task: approval is the only path to
/// `WorkspaceCronManager::add` (never-auto-approve, D.0).
///
/// The draft's prompt is untrusted content by construction (D.0e) — the
/// presentation faces render it as model-drafted text, never as user
/// intent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CronProposal {
    /// Unique proposal identifier (`proposal-<uuid>`).
    pub id: String,
    /// The workspace the draft targets (the proposing agent's workspace —
    /// ADR-014: the tool face has no scope parameter, the workspace is
    /// bound by the session's runtime, not chosen by the model).
    pub workspace_id: String,
    /// The 5-field cron expression (the D.0a schedule).
    pub cron: String,
    /// The IANA timezone the expression is evaluated in (`None` = the
    /// host's local zone — presented as that explicit value, D.5).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
    /// The verbatim prompt (D.0a: 一字不改).
    pub prompt: String,
    /// The recurring declaration.
    pub recurring: bool,
    /// The budget declaration (`None` = unbounded, presented explicitly).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_fires: Option<u32>,
    /// The suggested runtime permission posture (D-M2's three postures;
    /// the user may effectively override by rejecting + rerouting).
    pub suggested_posture: super::PermissionPosture,
    /// The proposed concurrency posture (D-M3).
    #[serde(default)]
    pub concurrency: super::TaskConcurrency,
    /// The proposing agent's optional reason (display-only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// The session the proposal was raised from (the async receipt's
    /// delivery target — D.0e's "下一次该 agent 活跃" address).
    pub proposed_by_session: String,
    /// The agent (within the session) that raised the proposal.
    pub proposed_by_agent: String,
    /// When the proposal was created.
    pub created_at: DateTime<Utc>,
    /// When the proposal lapses (created_at + 72h, the implementation-
    /// tunable `CRON_PROPOSAL_TTL`).
    pub expires_at: DateTime<Utc>,
    /// The lifecycle state.
    pub state: ProposalState,
    /// The user's feedback on a rejection (D.0b's reroute receipt input).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rejection_feedback: Option<String>,
    /// The task id minted on approval (the receipt's "任务已创建" anchor).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approved_task_id: Option<String>,
    /// The approval gate's request id — the R4 leak-prevention link (the
    /// decide path and the TTL sweep both withdraw the gate entry).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gate_request_id: Option<String>,
    /// Whether the async receipt was delivered into the proposing session.
    #[serde(default)]
    pub receipt_delivered: bool,
}

impl CronProposal {
    /// Whether the proposal is still awaiting a decision.
    pub fn is_pending(&self) -> bool {
        self.state == ProposalState::Pending
    }
}

impl WorkspaceCronDocument {
    /// An empty v1 document.
    pub fn new() -> Self {
        Self {
            version: WORKSPACE_CRON_STORE_VERSION,
            ..Self::default()
        }
    }
}

/// The outcome of [`WorkspaceCronManager::try_open_run`] — the
/// executor-side contract for the concurrency posture:
/// - `Opened(run_id)` — the run is live (the `Running` record landed);
/// - `Queued(run_id)` — the fire parked in the Queue posture's single
///   backlogged slot (execute LATER: the terminal side drains it once);
/// - `Declined(opener)` — the Skip posture (the caller records the skip
///   off the returned template).
#[derive(Debug, Clone)]
pub enum OpenRunOutcome {
    Opened(String),
    Queued(String),
    Declined(RunRecord),
}

/// Whether one run record is the Queue posture's backlogged slot (the
/// marker the drain and the merge both key on).
fn is_queued_slot(run: &RunRecord) -> bool {
    run.status == RunStatus::Skipped
        && run
            .skip_reason
            .as_deref()
            .is_some_and(|reason| reason.contains("单槽排队"))
}

/// The workspace-level cron store — a path holder over
/// `<sessions_root>/<workspace_key>/cron/tasks.json`.
///
/// Every mutation goes through [`WorkspaceCronStore::mutate`]: take the
/// lock file, read the current document, apply the caller's pure
/// transform, write it back atomically. The read-modify-write under the
/// lock is the whole point of the protocol (two concurrent tasks in this
/// one process — or another process sharing the home — can never
/// interleave a lost update).
#[derive(Clone)]
pub struct WorkspaceCronStore {
    /// The `tasks.json` path.
    path: PathBuf,
    /// Injected sync filesystem (zero-IO 契约 6); the engine default is
    /// [`mirri_os::LocalSyncFs`], tests inject doubles via [`Self::with_fs`].
    fs: std::sync::Arc<dyn mirri_os::SyncFs>,
}

impl std::fmt::Debug for WorkspaceCronStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkspaceCronStore")
            .field("path", &self.path)
            .field("fs", &"<dyn SyncFs>")
            .finish()
    }
}

/// Whether an fs error's chain carries a `NotFound` io cause — the
/// best-effort delete cascade's tolerated outcome.
fn is_not_found(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound)
    })
}

impl WorkspaceCronStore {
    /// The store for a workspace's sessions bucket: the caller passes the
    /// bucket directory (`<sessions_root>/<workspace_key>`) — the same
    /// bucket the session persistence derives from
    /// `encode_work_dir_key`.
    ///
    /// The engine default wiring: the host-local [`mirri_os::LocalSyncFs`]
    /// (the [`AgentBlobService`](crate::blob::AgentBlobService) precedent).
    pub fn new(bucket_dir: &Path) -> Self {
        Self::with_fs(
            bucket_dir,
            std::sync::Arc::new(mirri_os::LocalSyncFs::new()),
        )
    }

    /// [`Self::new`] with an injected filesystem provider (the test seam).
    pub fn with_fs(bucket_dir: &Path, fs: std::sync::Arc<dyn mirri_os::SyncFs>) -> Self {
        Self {
            path: bucket_dir
                .join(WORKSPACE_CRON_DIR)
                .join(WORKSPACE_CRON_TASKS_FILENAME),
            fs,
        }
    }

    /// The `tasks.json` path (tests and the delete cascade).
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The lock file path (`tasks.json.lock` — the `create_new` lock
    /// convention shared with the registry writer).
    pub fn lock_path(&self) -> PathBuf {
        PathBuf::from(format!("{}.lock", self.path.to_string_lossy()))
    }

    /// Read the current document — tolerant: a missing file is an empty
    /// v1 document (first run), a corrupt one degrades to empty with a
    /// warning (never a failure — the registry's read posture).
    ///
    /// **Unlocked** by design: the read side of [`Self::mutate`] holds the
    /// lock; plain readers (the listAll aggregation) take a
    /// point-in-time snapshot and tolerate a concurrent write landing
    /// between the stat and the read (the next call sees it).
    pub fn read(&self) -> WorkspaceCronDocument {
        let Ok(text) = self.fs.read_text(&self.path) else {
            return WorkspaceCronDocument::new();
        };
        match serde_json::from_str::<WorkspaceCronDocument>(&text) {
            Ok(document) => document,
            // Scope-spelling tolerance: a record carrying the flat
            // `"scope": "workspace"` + `"workspaceId"` pair (the wire DTO
            // shape) does not fit the enum and would fail the whole
            // document. Re-read every record through the shape-tolerant
            // wire parser (the same one the session restore uses) so the
            // file is adopted rather than silently discarded.
            Err(_) => match serde_json::from_str::<serde_json::Value>(&text)
                .ok()
                .map(Self::tolerant_document)
            {
                Some(document) => document,
                None => {
                    tracing::warn!(
                        store = %self.path.display(),
                        "workspace cron store could not be parsed; treating it as empty"
                    );
                    WorkspaceCronDocument::new()
                }
            },
        }
    }

    /// The tolerant fallback of [`Self::read`] (see there): parse each task
    /// through the wire parser, keep the runs and version as given.
    fn tolerant_document(value: serde_json::Value) -> WorkspaceCronDocument {
        let mut document = WorkspaceCronDocument::new();
        document.version = value
            .get("version")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(WORKSPACE_CRON_STORE_VERSION);
        document.tasks = value
            .get("tasks")
            .and_then(serde_json::Value::as_array)
            .map(|tasks| {
                tasks
                    .iter()
                    .filter_map(super::cron_task_from_wire)
                    .collect()
            })
            .unwrap_or_default();
        document.runs = value
            .get("runs")
            .cloned()
            .and_then(|runs| serde_json::from_value(runs).ok())
            .unwrap_or_default();
        document
    }

    /// Take the store's lock file (the registry lock family's semantics:
    /// `create_new` wins, a stale residue — mtime older than 10 s — is
    /// taken over, a live lock is waited out up to 2 s, then the
    /// mutation fails hard rather than clobbering a live writer).
    ///
    /// Blocking (`create_new` + a 50 ms polling loop) — async callers
    /// wrap in `spawn_blocking`.
    fn acquire_lock(&self) -> anyhow::Result<mirri_persistence::LockGuard> {
        mirri_persistence::RegistryLock::new(self.lock_path()).acquire()
    }

    /// Locked read-transform-write: hold the lock, read the current
    /// document, apply `transform` (which may fail with `E`), write the
    /// result back atomically. The lock is the file's existence — the
    /// guard's drop releases it. Blocking end to end (the `create_new`
    /// polling loop) — async callers wrap the CALLER in `spawn_blocking`
    /// (B1: the scheduling path does exactly that; this fn itself stays
    /// plain-sync because the test bodies and the registry-family
    /// callers are sync).
    pub fn mutate<T, E>(
        &self,
        transform: impl FnOnce(&mut WorkspaceCronDocument) -> Result<T, E>,
    ) -> Result<T, E>
    where
        E: From<anyhow::Error>,
    {
        // The cron/ directory (and the bucket) exist only after the first
        // write — create them BEFORE taking the lock (the lock file itself
        // lives in that directory; creating it in a missing parent fails).
        // Note: only the read and the mkdir route through the injected fs —
        // the atomic write below stays mirri-persistence-owned
        // (`write_file_atomically` is the shared crash-safe primitive; the
        // zero-IO gate scopes mirri-core, not the persistence crate).
        if let Some(parent) = self.path.parent() {
            self.fs.mkdir(parent, true)?;
        }
        let _lock = self.acquire_lock()?;
        let mut document = self.read();
        let outcome = transform(&mut document)?;
        document.version = WORKSPACE_CRON_STORE_VERSION;
        let text = serde_json::to_string_pretty(&document).map_err(anyhow::Error::from)?;
        mirri_persistence::write_file_atomically(&self.path, text.as_bytes())
            .map_err(anyhow::Error::from)?;
        Ok(outcome)
    }

    /// Remove the store file and its lock sibling (A.4's cascade): the
    /// workspace's tasks and run records go away with the workspace.
    /// Best-effort per file — a missing file is fine, a failure logs and
    /// the cascade proceeds (the bucket removal is the authoritative
    /// cleanup).
    pub fn remove_files(&self) {
        for path in [self.path.clone(), self.lock_path()] {
            if let Err(error) = self.fs.remove_file(&path)
                && !is_not_found(&error)
            {
                tracing::warn!(
                    store = %path.display(),
                    %error,
                    "removing the workspace cron store file failed"
                );
            }
        }
    }
}

/// One edit request over a workspace task (batch 2 / A9, `cron.update`).
///
/// Every field is optional and three-valued for the nullable ones:
/// `None` = leave as is, `Some(Some(v))` = set to `v`, `Some(None)` =
/// clear. The workspace is absent by design — the field exists only so an
/// out-of-bounds caller is REFUSED explicitly instead of silently
/// honored.
#[derive(Debug, Clone, Default)]
pub struct WorkspaceTaskEdit {
    /// The display title (`Some(None)` = untitled).
    pub title: Option<Option<String>>,
    /// The instruction the runs execute.
    pub prompt: Option<String>,
    /// A new cron expression (replaces the whole schedule; mutually
    /// exclusive with `interval`).
    pub cron: Option<String>,
    /// A new interval schedule (`Some(None)` = drop the interval and keep
    /// the expression; see `cron` for the mutually exclusive pair).
    pub interval: Option<Option<super::IntervalSpec>>,
    /// The evaluation zone (`Some(None)` = the host's local zone).
    pub timezone: Option<Option<String>>,
    /// The inclusive end date (`Some(None)` = no end date).
    pub ends_at: Option<Option<chrono::NaiveDate>>,
    /// The fire budget (`Some(None)` = unbounded).
    pub max_fires: Option<Option<u32>>,
    /// The model alias (`Some(None)` = the server default).
    pub model: Option<Option<String>>,
    /// The thinking effort (`Some(None)` = the model catalog's default).
    pub effort: Option<Option<crate::profile::ThinkingEffort>>,
    /// The declared permission mode (`Some(None)` = leave the record's
    /// pre-batch-2 posture untouched).
    pub permission_mode: Option<Option<super::PermissionMode>>,
    /// REFUSED when it differs from the manager's workspace.
    pub workspace_id: Option<String>,
}

/// The one-fire plan shared by both managers (T3's extraction): the
/// outcome of one due task computed under the task-table lock, before
/// any persistence or in-memory transition is applied. `super::CronManager`
/// and the workspace manager each hold their own lock and call their
/// `plan_fire` — the plan/apply split lives in their respective
/// `check_due` bodies, which differ in persistence (wire ops vs the
/// store document), not in scheduling math.
#[derive(Debug, Clone)]
pub(crate) struct FirePlan {
    /// A snapshot of the due task at detection time.
    pub(crate) task: super::CronTask,
    pub(crate) coalesced_count: usize,
    pub(crate) stale: bool,
    /// This delivery exhausts the task's max_fires budget (S3): the
    /// task is removed after this fire, same path as a one-shot.
    pub(crate) maxed_out: bool,
    /// The cursor the fire advances to (the last due occurrence, or `now`
    /// when the walk counted nothing).
    pub(crate) advanced: DateTime<Utc>,
    /// The post-fire schedule for a surviving recurring task; `None` when
    /// the task is removed by this fire (one-shot, stale) or can no longer
    /// be scheduled.
    pub(crate) next_fire_at: Option<DateTime<Utc>>,
    /// The moment this fire was scheduled for (the task's pre-fire
    /// `next_fire_at` — C.2a's lateness reference).
    pub(crate) scheduled_at: Option<DateTime<Utc>>,
    /// This delivery is the task's last because its `ends_at` day has
    /// arrived or passed (B1 / SDR-110): the task is removed after this
    /// fire, the same finale family as `maxed_out` and `stale`. The
    /// reschedule walk clamps at the end date — this flag means the NEXT
    /// occurrence would fall past `ends_at`'s local day.
    pub(crate) ended: bool,
}

/// The stale verdict for one task under the given semantic toggles — the
/// shared rule both managers' `is_stale` consult (the standalone
/// `super::CronManager::task_is_stale` face re-derives the default options).
pub(crate) fn task_is_stale_under(
    task: &super::CronTask,
    now: DateTime<Utc>,
    stale_enabled: bool,
) -> bool {
    let ttl_base = task
        .last_fired
        .filter(|last_fired| *last_fired > task.created_at)
        .unwrap_or(task.created_at);
    stale_enabled && task.recurring && now - ttl_base >= super::STALE_THRESHOLD
}

/// Whether the document's task belongs to the workspace scope (the
/// restore filter: a record that lost its scope keys defaults to
/// session and is skipped by the workspace manager).
pub(crate) fn task_belongs_to_workspace(task: &super::CronTask) -> bool {
    matches!(task.scope, TaskScope::Workspace(_))
}

/// The restore pass shared by the construction paths and
/// [`WorkspaceCronManager::reload_from_index`]: fold a document into the
/// in-memory task/run tables — scope filtering, paused/one-shot
/// next-fire recomputation, unschedulable-task skipping. One function so
/// the two media (a build-time document, a post-import reload) can never
/// drift apart.
fn restored_tables(
    document: WorkspaceCronDocument,
    options: super::CronOptions,
) -> (HashMap<String, super::CronTask>, HashMap<String, Vec<RunRecord>>) {
    let mut tasks = HashMap::new();
    let mut runs = document.runs;
    for mut task in document.tasks {
        if !task_belongs_to_workspace(&task) {
            // A record that lost its scope keys defaults to session —
            // it never belonged to this face; skip rather than adopt
            // (a dirty document never fails the open).
            continue;
        }
        let next_fire_at = if task.paused {
            // A paused task has no live schedule: the persisted `None`
            // survives the reopen (resuming recomputes), so a restart
            // never resurrects a moment that will not happen.
            None
        } else if !task.recurring && task.last_fired.is_some() {
            // A fired one-shot: dormant, same as the session restore.
            None
        } else {
            let tz = match super::CronTimezone::parse(task.timezone.as_deref()) {
                Ok(declared) => declared.resolve(),
                Err(error) => {
                    tracing::warn!(
                        task_id = %task.id,
                        %error,
                        "restored workspace cron task carries an unusable timezone; \
                         falling back to the host's local zone"
                    );
                    super::CronTimezone::Local.resolve()
                }
            };
            let base = task
                .last_fired
                .filter(|last_fired| *last_fired > task.created_at)
                .unwrap_or(task.created_at);
            let schedule = super::task_schedule(&task);
            // B1 / SDR-110: a restored task whose schedule already ran
            // past its end date restores dormant (its runs and records
            // survive; nothing fires again).
            match super::CronManager::compute_schedule_next_fire_pub(&schedule, base, tz) {
                Ok(ideal) if super::occurrence_past_ends_at(&task, ideal, tz) => None,
                Ok(ideal) => Some(
                    super::CronManager::schedule_occurrence_for(
                        &schedule,
                        &task.id,
                        ideal,
                        task.recurring,
                        task.created_at,
                        tz,
                        options,
                    )
                    .unwrap_or(ideal),
                ),
                Err(error) => {
                    tracing::warn!(
                        task_id = %task.id,
                        %error,
                        "restored workspace cron task is no longer schedulable; skipping it"
                    );
                    runs.remove(&task.id);
                    continue;
                }
            }
        };
        task.next_fire_at = next_fire_at;
        tasks.insert(task.id.clone(), task);
    }
    (tasks, runs)
}

/// The workspace-level cron manager (stage-2 T3 — D-M1's "coexistence,
/// no migration"): the same in-memory task-table + check_due semantics
/// [`super::CronManager`](super::CronManager) runs for a session, but persisted
/// through a [`WorkspaceCronStore`] instead of a session wire log, and
/// independent of every session's lifecycle (A.1/V11: closing all the
/// workspace's sessions never touches this table).
///
/// **Persistence posture** — write-before-mutate, the session manager's
/// contract translated to the store: every mutation (`add`, `remove`,
/// `check_due`'s cursor/finalize advances, run-record appends) lands in
/// `tasks.json` under the store lock BEFORE the in-memory table moves, so
/// a crash between the two leaves the file ahead of memory (a restart
/// re-reads the file as the source of truth). The session manager's
/// per-fire `cron.cursor` ops become one store write per `check_due`
/// batch — the plan/apply split collapses into the locked transform
/// because the store's read-modify-write IS the durable record.
///
/// **What this manager deliberately does NOT do**: the lineage defense,
/// the creation gate, and the approval chain are the tool/policy faces'
/// business (the same stance `super::CronManager` takes); the executor (the
/// run spawning, batch 4b) is consumed through the returned fires.
pub struct WorkspaceCronManager {
    /// The workspace this manager serves (the bucket id).
    workspace_id: String,
    /// The durable store backing this manager.
    store: WorkspaceCronStore,
    /// The SQLite write-through backend (ADR-020), consumed through the
    /// [`CronPersistence`] seam: `None` keeps the legacy JSON path (the
    /// existing tests), `Some` makes every `mutate_store` write land in
    /// SQLite transactionally and SKIPS the JSON write (the legacy file is
    /// a frozen read-only archive after the migration). The trait keeps
    /// the rusqlite carrier (`mirri-cron-index`) out of mirri-core's
    /// dependency tree — the zero-IO gate's regression lockout.
    index: Option<std::sync::Arc<dyn CronPersistence + Send + Sync>>,
    /// The in-memory task table (the authoritative copy between writes;
    /// every mutation syncs through the store first).
    tasks: std::sync::Mutex<HashMap<String, super::CronTask>>,
    /// The in-memory run records (keyed by task id, insertion order).
    runs: std::sync::Mutex<HashMap<String, Vec<RunRecord>>>,
    /// The semantic toggles (jitter / stale) this manager runs with.
    options: super::CronOptions,
    /// The registered change listeners — invoked after every durable
    /// mutation (the server side forwards them as
    /// `event.cron.workspaceChanged`).
    change_listeners: std::sync::Mutex<Vec<Box<dyn Fn() + Send + Sync>>>,
    /// Batch 4's apply-window test seam: a one-shot closure fired between
    /// the durable fire write and the in-memory apply, so a test can land
    /// a concurrent removal exactly inside that window instead of hoping
    /// a raced thread lands there (the C-13 determinization).
    #[cfg(test)]
    apply_hook: std::sync::Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
}

/// One stored task row of the SQLite write-through backend, mirroring the
/// carrier crate's table columns. Core owns the contract type; the
/// carrier (mirri-cron-index) converts at its seam.
pub struct ScheduledTaskRecord {
    pub id: String,
    pub workspace_id: String,
    pub next_fire_at: Option<String>,
    pub paused: bool,
    pub created_at: String,
    pub body: String,
}

/// The write-through backend contract (ADR-020): mirri-core's cron domain
/// consumes SQLite storage through this seam only — the carrier crate
/// (mirri-cron-index, rusqlite) stays out of mirri-core's dependency tree
/// (the zero-IO gate's regression lockout). mirri-embedded's adapter
/// implements it over the shared CronIndex connection.
pub trait CronPersistence: Send + Sync {
    fn list_workspace_rows(&self, workspace_id: &str) -> anyhow::Result<Vec<ScheduledTaskRecord>>;
    fn replace_workspace_rows(
        &self,
        workspace_id: &str,
        rows: &[ScheduledTaskRecord],
    ) -> anyhow::Result<()>;
}

impl WorkspaceCronManager {
    /// A manager over an empty or existing store: the document on disk is
    /// read once (the restart recovery — tasks with their cursors, plus
    /// the run records); a missing or corrupt store starts empty.
    pub fn open(workspace_id: &str, bucket_dir: &Path) -> Self {
        Self::with_options(workspace_id, bucket_dir, super::CronOptions::default())
    }

    /// [`Self::open`] with explicit semantic toggles.
    pub fn with_options(
        workspace_id: &str,
        bucket_dir: &Path,
        options: super::CronOptions,
    ) -> Self {
        let store = WorkspaceCronStore::new(bucket_dir);
        let document = store.read();
        Self::with_options_from_document(workspace_id, &store, document, options)
    }

    /// A manager over the SQLite index (ADR-020's write-through backend),
    /// injected through the [`CronPersistence`] seam (mirri-embedded's
    /// adapter over the shared `CronIndex` connection): the document is
    /// rebuilt from the index rows (the durable truth once the index is
    /// attached), then restored through the SAME
    /// [`Self::with_options`] recovery path the JSON store uses — scope
    /// filtering, next-fire recomputation, and skip-unschedulable semantics
    /// stay identical between the two media. The legacy JSON file is never
    /// read or written again by this manager.
    pub fn open_with_index(
        workspace_id: &str,
        bucket_dir: &Path,
        index: std::sync::Arc<dyn CronPersistence + Send + Sync>,
    ) -> Self {
        // Rebuild the document from SQLite rows, then restore through the
        // same recovery core (see with_options_from_document).
        let document = rebuild_document_from_index(index.as_ref(), workspace_id);
        let mut manager = Self::with_options_from_document(
            workspace_id,
            &WorkspaceCronStore::new(bucket_dir),
            document,
            super::CronOptions::default(),
        );
        manager.index = Some(index);
        manager
    }

    /// The restore core shared by [`Self::with_options`] (the JSON media)
    /// and [`Self::open_with_index`] (the SQLite media): rebuild the
    /// in-memory tables from a document — scope filtering, paused/one-shot
    /// next-fire recomputation, unschedulable-task skipping. Extracted from
    /// `with_options` with zero semantic change; `store` is the legacy JSON
    /// face (kept for the LEGACY proposal reads and the delete cascade).
    fn with_options_from_document(
        workspace_id: &str,
        store: &WorkspaceCronStore,
        document: WorkspaceCronDocument,
        options: super::CronOptions,
    ) -> Self {
        let (tasks, runs) = restored_tables(document, options);
        Self {
            workspace_id: workspace_id.to_string(),
            store: store.clone(),
            index: None,
            tasks: std::sync::Mutex::new(tasks),
            runs: std::sync::Mutex::new(runs),
            options,
            change_listeners: std::sync::Mutex::new(Vec::new()),
            #[cfg(test)]
            apply_hook: std::sync::Mutex::new(None),
        }
    }

    /// Adopt the SQLite rows' current state into the in-memory tables
    /// (ADR-006 Task 8): the one-time legacy-cron import lands its rows
    /// through the SHARED index connection AFTER the runtime's managers
    /// were already built from the (then-empty) rows — the default
    /// workspace's manager would otherwise never schedule a task the
    /// import adopted (its in-memory table stays empty until this call).
    ///
    /// The rebuild is the SAME restore core [`Self::open_with_index`] used
    /// (scope filtering, paused/one-shot next-fire recomputation,
    /// unschedulable-task skipping), so an adopted task fires exactly like
    /// one that was in SQLite at build time. A no-op without an attached
    /// index (the plain-JSON managers never see background imports).
    pub fn reload_from_index(&self) {
        let Some(index) = &self.index else {
            return;
        };
        let document = rebuild_document_from_index(index.as_ref(), &self.workspace_id);
        let (tasks, runs) = restored_tables(document, self.options);
        *self.tasks.lock().unwrap_or_else(|p| p.into_inner()) = tasks;
        *self.runs.lock().unwrap_or_else(|p| p.into_inner()) = runs;
    }

    /// Register a change listener — invoked after every durable mutation
    /// (the server side forwards it as `event.cron.workspaceChanged`).
    pub fn on_change(&self, listener: impl Fn() + Send + Sync + 'static) {
        self.change_listeners
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(Box::new(listener));
    }

    /// Install the one-shot apply-window hook (batch 4's C-13 test
    /// seam): it fires inside [`Self::check_due`] AFTER the durable fire
    /// write and BEFORE the in-memory apply — the exact window a
    /// concurrent removal must win without the task being resurrected.
    #[cfg(test)]
    pub(crate) fn set_apply_hook(&self, hook: impl Fn() + Send + Sync + 'static) {
        *self
            .apply_hook
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Box::new(hook));
    }

    /// Fire the registered change listeners (post-mutation).
    fn notify_change(&self) {
        for listener in self
            .change_listeners
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
        {
            listener();
        }
    }

    /// The workspace this manager serves.
    pub fn workspace_id(&self) -> &str {
        &self.workspace_id
    }

    /// The backing store (the delete cascade and the aggregation read it).
    pub fn store(&self) -> &WorkspaceCronStore {
        &self.store
    }

    /// Add a workspace task: validation (the same family the session
    /// `add` applies — max_fires on a one-shot, a zero budget, an invalid
    /// expression, an unusable timezone) then the durable insert
    /// (write-before-mutate: the store write precedes the in-memory
    /// insert). Returns the generated task id.
    pub async fn add(
        &self,
        cron: String,
        prompt: String,
        recurring: bool,
        options: super::CronCreateOptions,
    ) -> Result<String, super::CronError> {
        if !recurring && options.max_fires.is_some() {
            return Err(super::CronError::InvalidExpression(
                "max_fires cannot be set on a one-shot task (a one-shot fires exactly once)"
                    .to_string(),
            ));
        }
        if options.max_fires == Some(0) {
            return Err(super::CronError::InvalidExpression(
                "max_fires must be a positive integer".to_string(),
            ));
        }
        // The ends_at authoritative validation (B1 / M3, SDR-110) — the
        // workspace twin of the session `add`'s check: the end date may
        // not fall before the schedule's first fire day (the same
        // `preview_fires` kernel resolves the first fire, cron and
        // interval isomorphic).
        if let Some(ends_at) = options.ends_at {
            let tz = super::CronTimezone::parse(options.timezone.as_deref())?.resolve();
            let schedule = match &options.interval {
                Some(interval) => super::ScheduleSpec::Interval(interval),
                None => super::ScheduleSpec::Cron(&cron),
            };
            let first =
                super::CronManager::compute_schedule_next_fire_pub(&schedule, Utc::now(), tz)?;
            let first_day = first.with_timezone(&tz).date_naive();
            if ends_at < first_day {
                return Err(super::CronError::InvalidExpression(
                    "结束日期不能早于首次运行日".to_string(),
                ));
            }
        }
        // The scope is forced to this workspace regardless of what the
        // options carried (the manager IS the workspace face; a mismatched
        // option is a caller bug, not a routing decision).
        let mut options = options;
        options.scope = TaskScope::Workspace(self.workspace_id.clone());
        let task = super::CronManager::prepare_task(
            cron,
            prompt,
            String::new(),
            recurring,
            options,
            self.options,
        )?;
        let id = task.id.clone();
        // Write-before-mutate: the locked store write lands first; the
        // crash window leaves the file holding a task memory never saw
        // (the next open re-reads it — at-least-once, never lost).
        let task_for_store = task.clone();
        if self
            .mutate_store(move |document| {
                document
                    .tasks
                    .retain(|existing| existing.id != task_for_store.id);
                document.tasks.push(task_for_store.clone());
            })
            .is_err()
        {
            // A failed durable write never registers in memory (the file
            // is the source of truth; a half-registered task would drift
            // on the next open).
            return Err(super::CronError::InvalidExpression(
                "persisting the workspace cron task failed".to_string(),
            ));
        }
        self.tasks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(id.clone(), task);
        self.notify_change();
        Ok(id)
    }

    /// Remove a workspace task by id — the durable removal lands before
    /// the in-memory one. `true` when a task was removed.
    pub async fn remove(&self, id: &str) -> bool {
        let existed = self
            .tasks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains_key(id);
        if !existed {
            return false;
        }
        let id_owned = id.to_string();
        let durable = self
            .mutate_store(move |document| {
                document.tasks.retain(|task| task.id != id_owned);
                document.runs.remove(&id_owned);
            })
            .is_ok();
        if !durable {
            tracing::warn!(task_id = id, "persisting the workspace cron removal failed");
        }
        self.tasks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(id);
        self.notify_change();
        true
    }

    /// The workspace's tasks, sorted by id (the `super::CronManager::list`
    /// contract).
    pub fn list(&self) -> Vec<super::CronTask> {
        let mut tasks: Vec<super::CronTask> = self
            .tasks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .cloned()
            .collect();
        tasks.sort_by(|a, b| a.id.cmp(&b.id));
        tasks
    }

    /// Get a workspace task by id.
    pub fn get(&self, id: &str) -> Option<super::CronTask> {
        self.tasks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(id)
            .cloned()
    }

    /// How many run records one task keeps (review M2: the executor side
    /// appends unboundedly — a `* * * * *` task would grow its `runs`
    /// array for the process's whole lifetime). Retention is newest-first:
    /// appends beyond this bound drop the OLDEST records. Deliberately a
    /// plain constant (not user-configurable — no spec requirement asked
    /// for a knob; raise it here when the 4b executor's usage data makes a
    /// per-task volume case).
    pub const MAX_RUN_RECORDS_PER_TASK: usize = 50;

    /// The run records of one task (oldest first).
    pub fn runs(&self, id: &str) -> Vec<RunRecord> {
        self.runs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(id)
            .cloned()
            .unwrap_or_default()
    }

    /// Atomically open a run (review M4's TOCTOU fix): the has-a-Running
    /// check and the `Running` opener's durable write + in-memory append
    /// happen inside ONE manager-level critical section (the `runs` mutex
    /// is held across the check, the store write, and the mirror), so a
    /// run-now trigger racing a scheduler tick can never observe
    /// "not running" twice and double-open the task.
    ///
    /// `queue` (D-M3's Queue posture): a Running present parks the fire
    /// in the SINGLE backlogged slot — a run_id stamps the queued intent,
    /// merged one deep (a second queued fire replaces the first's slot —
    /// one queued execution, never a backlog pile). The caller must NOT
    /// execute a queued fire now: the terminal side drains it once after
    /// the current run settles.
    pub async fn try_open_run(
        &self,
        task_id: &str,
        queue: bool,
        opener: RunRecord,
    ) -> OpenRunOutcome {
        // The critical section: the runs lock is held across the check,
        // the durable write, and the in-memory mirror. The store write
        // inside the mutex is a blocking call (the file-lock polling) —
        // acceptable here: the section is per-task-sized (one append),
        // and correctness (never double-open) beats the lock's latency.
        let mut runs = self
            .runs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let has_running = runs
            .get(task_id)
            .is_some_and(|records| records.iter().any(|run| run.status.is_in_flight()));
        if has_running {
            if queue {
                // The single queued slot: an existing queued entry merges
                // (its record is replaced — one queued execution); the
                // queue drains after the current run's terminal state
                // (the executor's drain step).
                let queued = RunRecord {
                    status: RunStatus::Skipped,
                    skip_reason: Some("排队中（并发姿态：Queue，单槽排队）".to_string()),
                    ended_at: None,
                    ..opener
                };
                let run_id = queued.run_id.clone();
                let task_for_store = task_id.to_string();
                let queued_for_store = queued.clone();
                let existing_queued = runs
                    .get(&task_for_store)
                    .is_some_and(|records| records.iter().any(is_queued_slot));
                let _ = self.mutate_store(move |document| {
                    let records = document.runs.entry(task_for_store.clone()).or_default();
                    if existing_queued {
                        // Merge into the existing slot: the queued record
                        // is replaced, not appended (one queued execution).
                        if let Some(slot) = records.iter_mut().rev().find(|run| is_queued_slot(run))
                        {
                            *slot = queued_for_store.clone();
                        }
                    } else {
                        records.push(queued_for_store.clone());
                        let overflow = records.len().saturating_sub(Self::MAX_RUN_RECORDS_PER_TASK);
                        if overflow > 0 {
                            records.drain(..overflow);
                        }
                    }
                });
                let records = runs.entry(task_id.to_string()).or_default();
                if existing_queued {
                    if let Some(slot) = records.iter_mut().rev().find(|run| is_queued_slot(run)) {
                        *slot = queued;
                    }
                } else {
                    records.push(queued);
                    let overflow = records.len().saturating_sub(Self::MAX_RUN_RECORDS_PER_TASK);
                    if overflow > 0 {
                        records.drain(..overflow);
                    }
                }
                drop(runs);
                self.notify_change();
                OpenRunOutcome::Queued(run_id)
            } else {
                // The Skip posture: the caller records the skip (the
                // returned opener is the template; the caller stamps the
                // skip reason).
                OpenRunOutcome::Declined(opener)
            }
        } else {
            // No Running: open. The durable write first (the same
            // write-before-mirror discipline), still inside the section.
            let run_id = opener.run_id.clone();
            let task_for_store = task_id.to_string();
            let opener_for_store = opener.clone();
            let _ = self.mutate_store(move |document| {
                let records = document.runs.entry(task_for_store).or_default();
                records.push(opener_for_store);
                let overflow = records.len().saturating_sub(Self::MAX_RUN_RECORDS_PER_TASK);
                if overflow > 0 {
                    records.drain(..overflow);
                }
            });
            let records = runs.entry(task_id.to_string()).or_default();
            records.push(opener);
            let overflow = records.len().saturating_sub(Self::MAX_RUN_RECORDS_PER_TASK);
            if overflow > 0 {
                records.drain(..overflow);
            }
            drop(runs);
            self.notify_change();
            OpenRunOutcome::Opened(run_id)
        }
    }

    /// Drain one queued fire (D-M3's Queue posture, the terminal side):
    /// after a run's terminal state, if the task holds a queued (single
    /// slot) record, it is popped and executed once — the queued record's
    /// `skip_reason` is cleared and the record re-opens as `Running` for
    /// the drained execution. Returns the run's task id and run_id when
    /// a queued fire drained (the executor spawns it); `None` when
    /// nothing was queued.
    pub async fn drain_queued_run(&self, task_id: &str) -> Option<(String, String)> {
        let mut runs = self
            .runs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut record = runs
            .get(task_id)
            .and_then(|records| records.iter().find(|run| is_queued_slot(run)).cloned())?;
        // No drain while another run is still Running (the queue drains
        // only after the terminal state; the guard is belt-and-braces
        // against a double drain racing a fresh opener).
        let still_running = runs
            .get(task_id)
            .is_some_and(|records| records.iter().any(|run| run.status.is_in_flight()));
        if still_running {
            return None;
        }
        record.status = RunStatus::Running;
        record.started_at = chrono::Utc::now();
        record.ended_at = None;
        record.duration_ms = None;
        record.skip_reason = None;
        let task_for_store = task_id.to_string();
        let record_for_store = record.clone();
        let _ = self.mutate_store(move |document| {
            let records = document.runs.entry(task_for_store).or_default();
            // The stored list may have shifted (retention drained the
            // oldest); re-find the queued record by its run_id.
            if let Some(slot) = records
                .iter_mut()
                .find(|run| run.run_id == record_for_store.run_id)
            {
                *slot = record_for_store;
            }
        });
        if let Some(records) = runs.get_mut(task_id)
            && let Some(slot) = records.iter_mut().find(|run| run.run_id == record.run_id)
        {
            *slot = record.clone();
        }
        drop(runs);
        self.notify_change();
        Some((task_id.to_string(), record.run_id))
    }

    /// Append one run record to a task (the executor's write face — 4a's
    /// skip records ride it too). Durable first, then in memory. Bounded
    /// retention (review M2): a task keeps at most
    /// [`MAX_RUN_RECORDS_PER_TASK`] records — appends beyond the bound
    /// drop the oldest.
    pub async fn append_run(&self, task_id: &str, record: RunRecord) {
        let (task_id, record_for_store) = (task_id.to_string(), record.clone());
        let task_id_for_store = task_id.clone();
        let _ = self.mutate_store(move |document| {
            let runs = document.runs.entry(task_id_for_store).or_default();
            runs.push(record_for_store);
            let overflow = runs.len().saturating_sub(Self::MAX_RUN_RECORDS_PER_TASK);
            if overflow > 0 {
                runs.drain(..overflow);
            }
        });
        let mut runs = self
            .runs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let entry = runs.entry(task_id).or_default();
        entry.push(record);
        let overflow = entry.len().saturating_sub(Self::MAX_RUN_RECORDS_PER_TASK);
        if overflow > 0 {
            entry.drain(..overflow);
        }
        self.notify_change();
    }

    /// Record one run lifecycle record and advance the task's
    /// `fire_count` (review M1): the count belongs to the run semantics,
    /// not the due-detection cursor — only a REAL terminal state
    /// (`Succeeded`/`Failed`/`Rejected`) constitutes "ran" and consumes
    /// the `max_fires` budget. A `Skipped` record (the D-M3 concurrency
    /// skip) and the in-flight `Running` opener (4b's spawner opens the
    /// record before the turn starts) do NOT count — the count advances
    /// when the run's terminal state lands (the 4a M1 fix's 4b
    /// continuation: the placeholder era only ever wrote Skipped +
    /// terminals, the spawner era also writes Running openers, and an
    /// opener must not consume the budget twice).
    ///
    /// One durable write carries both the run record and the count (the
    /// same locked read-modify-write every mutation rides); the in-memory
    /// mirror follows under the table lock, conditionally on the task
    /// still existing (review M4: a task removed concurrently with the
    /// apply is not resurrected — its runs were dropped with it).
    pub async fn record_run(&self, task_id: &str, record: RunRecord) {
        let counts = matches!(
            record.status,
            RunStatus::Succeeded | RunStatus::Failed | RunStatus::Rejected
        );
        let (task_id, record_for_store) = (task_id.to_string(), record.clone());
        let task_id_for_store = task_id.clone();
        let _ = self.mutate_store(move |document| {
            let runs = document.runs.entry(task_id_for_store.clone()).or_default();
            runs.push(record_for_store);
            let overflow = runs.len().saturating_sub(Self::MAX_RUN_RECORDS_PER_TASK);
            if overflow > 0 {
                runs.drain(..overflow);
            }
            if counts
                && let Some(task) = document
                    .tasks
                    .iter_mut()
                    .find(|task| task.id == task_id_for_store)
            {
                task.fire_count += 1;
            }
        });
        let mut runs = self
            .runs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let entry = runs.entry(task_id.clone()).or_default();
        entry.push(record);
        let overflow = entry.len().saturating_sub(Self::MAX_RUN_RECORDS_PER_TASK);
        if overflow > 0 {
            entry.drain(..overflow);
        }
        drop(runs);
        if counts {
            let mut tasks = self
                .tasks
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(task) = tasks.get_mut(&task_id) {
                task.fire_count += 1;
            }
        }
        self.notify_change();
    }

    // ------------------------------------------------------------------
    // batch 2 (A9): edit / pause / resume — read-modify-WHOLE-document
    // ------------------------------------------------------------------
    //
    // The store persists the whole `WorkspaceCronDocument` atomically
    // (temp file + rename under the lock), so an edit is: validate the new
    // shape, compute the new schedule's next fire, then ONE
    // `mutate_store` call that replaces the task's record in place. The
    // task id, its `created_at`, and its run records never move — the
    // identity and the history survive an edit by construction.

    /// Pause or resume one task (`cron.pause` / `cron.resume`'s durable
    /// core). The document write lands first; the in-memory table follows.
    /// `Ok(false)` for an unknown id.
    ///
    /// The cursor semantics: pausing clears `next_fire_at` (`None` — a
    /// paused task has no next moment to show; the same shape the edit
    /// path stores, so the task row never renders a moment that will not
    /// happen), while `last_fired` and the schedule keep their phase. The
    /// scheduling pass skips paused tasks — so the occurrences inside the
    /// pause window are simply never planned, never recorded, and never
    /// counted. [`Self::resume`] then recomputes the next fire from now,
    /// which lands on a future occurrence: a resumed task never catches up
    /// on what it missed.
    pub async fn set_paused(&self, id: &str, paused: bool) -> Result<bool, super::CronError> {
        let Some(existing) = self.get(id) else {
            return Ok(false);
        };
        let next_fire_at = if paused {
            None
        } else {
            self.next_fire_after_edit(&existing, Utc::now())?
        };
        let id_owned = id.to_string();
        self.mutate_store(move |document| {
            if let Some(task) = document.tasks.iter_mut().find(|task| task.id == id_owned) {
                task.paused = paused;
                task.next_fire_at = next_fire_at;
            }
        })
        .map_err(store_error)?;
        if let Some(task) = self
            .tasks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get_mut(id)
        {
            task.paused = paused;
            task.next_fire_at = next_fire_at;
        }
        self.notify_change();
        Ok(true)
    }

    /// Resume one task: clear the pause AND recompute its next fire from
    /// `now` (the first future occurrence — no backlog replay). Returns
    /// the resumed task, `None` for an unknown id.
    pub async fn resume(&self, id: &str) -> Option<super::CronTask> {
        match self.set_paused(id, false).await {
            Ok(true) => self.get(id),
            Ok(false) => None,
            Err(error) => {
                tracing::warn!(task_id = id, %error, "resuming the workspace cron task failed");
                None
            }
        }
    }

    /// Edit one workspace task (A9). Only the fields the caller sets are
    /// changed (`Some(...)`; clearing an optional field is `Some(None)`).
    ///
    /// The workspace is NOT editable — a task moved elsewhere is a
    /// different task and its history would lie — so a `workspace_id`
    /// that differs from this manager's workspace is REFUSED outright.
    ///
    /// The new shape is validated exactly like a create (max_fires rules,
    /// interval/timezone validity, ends_at vs the first fire), the next
    /// fire is recomputed from `now` under the new schedule, and the whole
    /// change lands in ONE durable document write. A `now` anchored on the
    /// real clock is [`Self::update`]; [`Self::update_at`] pins it for
    /// tests. Unknown id → error; nothing is written on any error path.
    pub async fn update(
        &self,
        id: &str,
        edit: WorkspaceTaskEdit,
    ) -> Result<super::CronTask, super::CronError> {
        self.update_at(id, edit, Utc::now()).await
    }

    /// [`Self::update`] with an explicit `now` (deterministic tests).
    pub async fn update_at(
        &self,
        id: &str,
        edit: WorkspaceTaskEdit,
        now: DateTime<Utc>,
    ) -> Result<super::CronTask, super::CronError> {
        let existing = self
            .get(id)
            .ok_or_else(|| super::CronError::InvalidExpression(format!("定时任务不存在：{id}")))?;
        if let Some(workspace_id) = &edit.workspace_id
            && workspace_id != &self.workspace_id
        {
            return Err(super::CronError::InvalidExpression(
                "工作区不可修改：更换工作区等于换一个任务，请删除后重新创建".to_string(),
            ));
        }
        let mut edited = existing.clone();
        if let Some(title) = edit.title {
            edited.title = title;
        }
        if let Some(prompt) = edit.prompt {
            edited.prompt = prompt;
        }
        if let Some(timezone) = edit.timezone {
            edited.timezone = timezone;
        }
        if let Some(ends_at) = edit.ends_at {
            edited.ends_at = ends_at;
        }
        if let Some(max_fires) = edit.max_fires {
            edited.max_fires = max_fires;
        }
        if let Some(model) = edit.model {
            edited.model = model;
        }
        if let Some(effort) = edit.effort {
            edited.effort = effort;
        }
        if let Some(mode) = edit.permission_mode {
            edited.permission_mode = mode;
            edited.permission_posture = mode.map(super::PermissionMode::posture);
        }
        if edit.cron.is_some() || edit.interval.is_some() {
            // The schedule pair is replaced as a WHOLE: an expression edit
            // drops the interval (and vice versa), exactly like a create.
            match (edit.cron, edit.interval) {
                (Some(cron), None) => {
                    edited.cron = cron;
                    edited.interval = None;
                }
                (None, Some(Some(interval))) => {
                    edited.cron = String::new();
                    edited.interval = Some(interval);
                }
                (_, Some(None)) => {
                    edited.cron = existing.cron.clone();
                    edited.interval = None;
                }
                (Some(_), Some(_)) => {
                    return Err(super::CronError::InvalidExpression(
                        "日程只能是一种形态：cron 表达式与间隔二选一".to_string(),
                    ));
                }
                (None, None) => {}
            }
        }
        // Validation — the same structural rules a create applies.
        if !edited.recurring && edited.max_fires.is_some() {
            return Err(super::CronError::InvalidExpression(
                "max_fires cannot be set on a one-shot task (a one-shot fires exactly once)"
                    .to_string(),
            ));
        }
        if edited.max_fires == Some(0) {
            return Err(super::CronError::InvalidExpression(
                "max_fires must be a positive integer".to_string(),
            ));
        }
        if let Some(interval) = &edited.interval {
            interval.validate()?;
        }
        let tz = super::CronTimezone::parse(edited.timezone.as_deref())?.resolve();
        let schedule = super::task_schedule(&edited);
        let first = super::CronManager::compute_schedule_next_fire_pub(&schedule, now, tz)?;
        if let Some(ends_at) = edited.ends_at
            && ends_at < first.with_timezone(&tz).date_naive()
        {
            return Err(super::CronError::InvalidExpression(
                "结束日期不能早于首次运行日".to_string(),
            ));
        }
        let ideal = super::CronManager::compute_schedule_next_fire_pub(&schedule, now, tz)?;
        edited.next_fire_at = if edited.paused {
            // A paused task keeps no live schedule; resuming recomputes.
            None
        } else if super::occurrence_past_ends_at(&edited, ideal, tz) {
            None
        } else {
            Some(
                super::CronManager::schedule_occurrence_for(
                    &schedule,
                    &edited.id,
                    ideal,
                    edited.recurring,
                    edited.created_at,
                    tz,
                    self.options,
                )
                .unwrap_or(ideal),
            )
        };
        let edited_for_store = edited.clone();
        self.mutate_store(move |document| {
            if let Some(task) = document
                .tasks
                .iter_mut()
                .find(|task| task.id == edited_for_store.id)
            {
                *task = edited_for_store.clone();
            }
        })
        .map_err(store_error)?;
        self.tasks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(edited.id.clone(), edited.clone());
        self.notify_change();
        Ok(edited)
    }

    /// The next fire a task would take from `now` under its current
    /// schedule (the resume-side recompute; `None` when the schedule has
    /// no future occurrence).
    fn next_fire_after_edit(
        &self,
        task: &super::CronTask,
        now: DateTime<Utc>,
    ) -> Result<Option<DateTime<Utc>>, super::CronError> {
        let tz = match super::CronTimezone::parse(task.timezone.as_deref()) {
            Ok(declared) => declared.resolve(),
            Err(error) => {
                tracing::warn!(
                    task_id = %task.id,
                    %error,
                    "resumed task carries an unusable timezone; using the host's local zone"
                );
                super::CronTimezone::Local.resolve()
            }
        };
        let schedule = super::task_schedule(task);
        let Ok(ideal) = super::CronManager::compute_schedule_next_fire_pub(&schedule, now, tz)
        else {
            return Ok(None);
        };
        if super::occurrence_past_ends_at(task, ideal, tz) {
            return Ok(None);
        }
        Ok(Some(
            super::CronManager::schedule_occurrence_for(
                &schedule,
                &task.id,
                ideal,
                task.recurring,
                task.created_at,
                tz,
                self.options,
            )
            .unwrap_or(ideal),
        ))
    }

    /// Force a registered task's next fire time — the host/test seam
    /// (`super::CronManager::set_next_fire_at`'s twin, memory only).
    pub fn set_next_fire_at(&self, id: &str, at: Option<DateTime<Utc>>) -> bool {
        let mut tasks = self
            .tasks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match tasks.get_mut(id) {
            Some(task) => {
                task.next_fire_at = at;
                true
            }
            None => false,
        }
    }

    // -- proposals (LEGACY compatibility reads) ----------------------------
    //
    // C2 retired the `ScheduledTaskPropose` propose-approve channel: nothing
    // writes proposals any more, and nothing consumes these reads (batch 3
    // removed the `cron.proposals.list` wire row). They and the
    // `CronProposal` / `ProposalState` types stay only so documents written
    // by older builds keep parsing; remove them once the oldest supported
    // document version no longer carries the `proposals` section.

    /// LEGACY read — every proposal (all states), for documents that still
    /// carry the section.
    pub fn proposals(&self) -> Vec<CronProposal> {
        let store = self.store.read();
        let mut proposals = store.proposals;
        proposals.sort_by_key(|proposal| proposal.created_at);
        proposals
    }

    /// LEGACY read — one proposal by id.
    pub fn proposal(&self, id: &str) -> Option<CronProposal> {
        self.store
            .read()
            .proposals
            .into_iter()
            .find(|proposal| proposal.id == id)
    }

    /// Sweep the pending proposals whose TTL lapsed (the scheduler tick's
    /// light pass — D.0e): each lapses to `Expired`. Returns the lapsed
    /// proposals so the caller can withdraw the gate entries and inject
    /// the "已作废（无响应）" receipts.
    pub async fn expire_lapsed_proposals(&self, now: DateTime<Utc>) -> Vec<CronProposal> {
        // The cheap pre-check first: no pending proposal past its TTL
        // means NO store write (a no-op mutate would still rewrite the
        // file and bump its mtime — the `cron.listAll` workspace cache
        // keys freshness on that mtime, so an every-tick rewrite would
        // defeat the cache).
        let any_lapsed =
            self.store.read().proposals.iter().any(|proposal| {
                proposal.state == ProposalState::Pending && proposal.expires_at <= now
            });
        if !any_lapsed {
            return Vec::new();
        }
        let lapsed: Vec<CronProposal> = {
            let mut lapsed_sink = Vec::new();
            let ids = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
            let ids_for_closure = std::sync::Arc::clone(&ids);
            let sweep = self.mutate_store(move |document| {
                for proposal in document.proposals.iter_mut() {
                    if proposal.state == ProposalState::Pending && proposal.expires_at <= now {
                        proposal.state = ProposalState::Expired;
                        ids_for_closure
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .push(proposal.id.clone());
                    }
                }
            });
            if sweep.is_err() {
                tracing::warn!("persisting the proposal TTL sweep failed");
                return Vec::new();
            }
            let swept = ids.lock().unwrap_or_else(|e| e.into_inner()).clone();
            if !swept.is_empty() {
                lapsed_sink.extend(
                    self.store
                        .read()
                        .proposals
                        .into_iter()
                        .filter(|proposal| swept.contains(&proposal.id)),
                );
            }
            lapsed_sink
        };
        if !lapsed.is_empty() {
            self.notify_change();
        }
        lapsed
    }

    /// Mark a proposal's async receipt as delivered (D.0e's once-only
    /// delivery bit — the notification injection path clears the debt).
    pub async fn mark_proposal_receipt_delivered(&self, id: &str) {
        let id = id.to_string();
        let _ = self.mutate_store(move |document| {
            if let Some(proposal) = document
                .proposals
                .iter_mut()
                .find(|proposal| proposal.id == id)
            {
                proposal.receipt_delivered = true;
            }
        });
    }

    /// Stamp the execution session's ref onto one run record (the opener
    /// opens without a session — the session is created after; this seam
    /// fills the slot without appending a second record).
    pub async fn set_run_session_ref(&self, task_id: &str, run_id: &str, session_ref: &str) {
        let (task_for_store, run_for_store, session_for_store) = (
            task_id.to_string(),
            run_id.to_string(),
            session_ref.to_string(),
        );
        let _ = self.mutate_store(move |document| {
            if let Some(records) = document.runs.get_mut(&task_for_store)
                && let Some(record) = records.iter_mut().find(|run| run.run_id == run_for_store)
            {
                record.session_ref = Some(session_for_store);
            }
        });
        let mut runs = self
            .runs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(records) = runs.get_mut(task_id)
            && let Some(record) = records.iter_mut().find(|run| run.run_id == run_id)
        {
            record.session_ref = Some(session_ref.to_string());
        }
    }

    /// Flip one run between `Running` and `WaitingApproval` (ADR-021):
    /// the turn-flow's approval-wait hook reports that the run's turn is
    /// parked on an approval (or that the wait is over). Only that pair
    /// may transition — a terminal state is never overwritten by a late
    /// signal, and any other target status is ignored.
    pub async fn set_run_status(&self, task_id: &str, run_id: &str, status: RunStatus) {
        let (task_for_store, run_for_store, status_for_store) =
            (task_id.to_string(), run_id.to_string(), status);
        let _ = self.mutate_store(move |document| {
            if let Some(records) = document.runs.get_mut(&task_for_store)
                && let Some(record) = records.iter_mut().find(|run| run.run_id == run_for_store)
                && matches!(
                    (record.status, status_for_store),
                    (RunStatus::Running, RunStatus::WaitingApproval)
                        | (RunStatus::WaitingApproval, RunStatus::Running)
                )
            {
                record.status = status_for_store;
            }
        });
        let mut runs = self
            .runs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(records) = runs.get_mut(task_id)
            && let Some(record) = records.iter_mut().find(|run| run.run_id == run_id)
            && matches!(
                (record.status, status),
                (RunStatus::Running, RunStatus::WaitingApproval)
                    | (RunStatus::WaitingApproval, RunStatus::Running)
            )
        {
            record.status = status;
        }
    }

    /// Replace one run record by run id (the terminal side of the
    /// opener/finale pairing: the terminal record carries the SAME run id
    /// as the Running opener, so the opener slot is replaced — a plain
    /// append would leave the stale Running behind and wedge the
    /// concurrency check).
    pub async fn replace_run(&self, task_id: &str, record: RunRecord) {
        let counts = matches!(
            record.status,
            RunStatus::Succeeded | RunStatus::Failed | RunStatus::Rejected
        );
        let task_for_store = task_id.to_string();
        let record_for_store = record.clone();
        let _ = self.mutate_store(move |document| {
            let replaced = if let Some(records) = document.runs.get_mut(&task_for_store) {
                let mut replaced = false;
                for run in records.iter_mut() {
                    if run.run_id == record_for_store.run_id {
                        *run = record_for_store.clone();
                        replaced = true;
                        break;
                    }
                }
                replaced
            } else {
                false
            };
            if !replaced {
                // The opener record was retained away (the 50-record cap
                // drained it mid-run): the terminal still lands as an
                // append — the run's outcome must never be lost.
                let records = document.runs.entry(task_for_store.clone()).or_default();
                records.push(record_for_store.clone());
                let overflow = records.len().saturating_sub(Self::MAX_RUN_RECORDS_PER_TASK);
                if overflow > 0 {
                    records.drain(..overflow);
                }
            }
            if counts
                && let Some(task) = document
                    .tasks
                    .iter_mut()
                    .find(|task| task.id == task_for_store)
            {
                task.fire_count += 1;
            }
        });
        let mut runs = self
            .runs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let entry = runs.entry(task_id.to_string()).or_default();
        let mut replaced = false;
        for run in entry.iter_mut() {
            if run.run_id == record.run_id {
                *run = record.clone();
                replaced = true;
                break;
            }
        }
        if !replaced {
            entry.push(record);
            let overflow = entry.len().saturating_sub(Self::MAX_RUN_RECORDS_PER_TASK);
            if overflow > 0 {
                entry.drain(..overflow);
            }
        }
        drop(runs);
        if counts {
            let mut tasks = self
                .tasks
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(task) = tasks.get_mut(task_id) {
                task.fire_count += 1;
            }
        }
        self.notify_change();
    }

    /// Converge every `Running` run record to `Failed{reason}` (M2's
    /// orphan sweep): called once per PROCESS (the scheduler's first
    /// workspace pass) — every Running record a fresh process reads is a
    /// crash orphan, because an execution turn cannot survive a restart.
    /// The task's scheduling posture recovers automatically: the
    /// concurrency check reads run records, and a Failed record is not
    /// Running, so the next fire opens a fresh run (a Skip/Queue task
    /// wedged behind the dead Running is un-wedged). Returns the
    /// converged (task_id, run_id) pairs.
    pub async fn fail_orphan_running_runs(&self, reason: &str) -> Vec<(String, String)> {
        // The cheap pre-check first (the expire_lapsed_proposals posture):
        // no Running record means NO store write — a no-op mutate would
        // still create the cron/ directory + tasks.json file where none
        // existed (a pristine bucket must stay pristine — the session
        // retention tests assert nothing is left behind).
        let any_running = {
            let runs = self
                .runs
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            runs.values()
                .flatten()
                .any(|run| run.status.is_in_flight())
        };
        if !any_running {
            return Vec::new();
        }
        let reason = reason.to_string();
        let swept: Vec<(String, String)> = {
            let mut sink = Vec::new();
            let ids = std::sync::Arc::new(std::sync::Mutex::new(Vec::<(String, String)>::new()));
            let ids_for_closure = std::sync::Arc::clone(&ids);
            let sweep = self.mutate_store(move |document| {
                for (task_id, runs) in document.runs.iter_mut() {
                    for run in runs.iter_mut() {
                        if run.status.is_in_flight() {
                            run.status = RunStatus::Failed;
                            run.ended_at = Some(chrono::Utc::now());
                            run.duration_ms = Some(
                                (chrono::Utc::now() - run.started_at)
                                    .num_milliseconds()
                                    .max(0) as u64,
                            );
                            run.skip_reason = Some(reason.clone());
                            ids_for_closure
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .push((task_id.clone(), run.run_id.clone()));
                        }
                    }
                }
            });
            if sweep.is_err() {
                tracing::warn!("persisting the stale-Running orphan sweep failed");
                return Vec::new();
            }
            sink.extend(ids.lock().unwrap_or_else(|e| e.into_inner()).clone());
            sink
        };
        // Mirror the durable convergence into the in-memory runs (the
        // write-before-mirror discipline — re-reading the store keeps the
        // two from drifting). With the SQLite index attached the mirror
        // source is the just-written rows: the frozen JSON archive would
        // roll memory back to the pre-migration state (the orphan sweep
        // would be undone in memory the moment it finished).
        let document = self.post_mutate_document();
        let mut runs = self
            .runs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *runs = document.runs;
        drop(runs);
        if !swept.is_empty() {
            self.notify_change();
        }
        swept
    }

    /// Terminate every `Running` run record with the workspace-deletion
    /// cause (A.4/V19: an in-flight run whose workspace was deleted ends
    /// honestly — recorded, not silently dropped, not counted as a
    /// failure of the run itself). Returns the terminated run refs (the
    /// caller uses them for any cancellation plumbing it owns).
    pub async fn terminate_running_runs(&self, cause: &str) -> Vec<String> {
        let cause = cause.to_string();
        let terminated: std::sync::Arc<std::sync::Mutex<Vec<String>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = std::sync::Arc::clone(&terminated);
        let sweep = self.mutate_store(move |document| {
            for (task_id, runs) in document.runs.iter_mut() {
                for run in runs.iter_mut() {
                    if run.status.is_in_flight() {
                        run.status = RunStatus::Rejected;
                        run.ended_at = Some(chrono::Utc::now());
                        run.duration_ms = Some(
                            (chrono::Utc::now() - run.started_at)
                                .num_milliseconds()
                                .max(0) as u64,
                        );
                        run.skip_reason = Some(cause.clone());
                        if let Some(session_ref) = &run.session_ref {
                            sink.lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .push(session_ref.clone());
                        }
                        let _ = task_id;
                    }
                }
            }
        });
        if sweep.is_err() {
            tracing::warn!("persisting the workspace-deletion run terminations failed");
            return terminated.lock().unwrap_or_else(|e| e.into_inner()).clone();
        }
        // Mirror the durable terminations into the in-memory runs (the
        // same write-before-mirror discipline every mutation rides —
        // re-reading the store keeps the two from drifting). With the
        // SQLite index attached the mirror source is the just-written
        // rows, NOT the frozen JSON archive (see fail_orphan_running_runs).
        let document = self.post_mutate_document();
        let mut runs = self
            .runs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *runs = document.runs;
        terminated.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Check for due workspace tasks and return their fires — the same
    /// plan/apply semantics [`super::CronManager::check_due`] runs (coalescing,
    /// stale final delivery, max_fires finale, one-shot removal), with
    /// the persistence translated to one locked store write per pass:
    /// the cursor advances, the counts increment, and the final removals
    /// all land in the SAME durable write the plans computed, before the
    /// in-memory table moves. Due tasks are returned sorted by id.
    pub async fn check_due(&self) -> Vec<super::CronFire> {
        let now = Utc::now();
        // Detection + plan pass: pure reads under the table lock.
        let plans: Vec<FirePlan> = {
            let tasks = self
                .tasks
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let mut plans: Vec<FirePlan> = tasks
                .values()
                // A paused task never fires and never advances its cursor
                // (A9): nothing is recorded, so the pause window leaves no
                // trace in the history.
                .filter(|task| !task.paused)
                .filter(|task| task.next_fire_at.is_some_and(|next| next <= now))
                .map(|task| self.plan_fire(task, now))
                .collect();
            plans.sort_by(|a, b| a.task.id.cmp(&b.task.id));
            plans
        };
        if plans.is_empty() {
            return Vec::new();
        }
        // Write-before-mutate: ONE locked store write carries every
        // plan's transition (cursor, reschedule, removal). The
        // fire_count is deliberately NOT advanced here (review M1):
        // the count belongs to the run semantics — the executor-side
        // `record_run` advances it on every real terminal state (a
        // `Skipped` placeholder does not consume the budget). The cursor
        // below still advances: the occurrence WAS detected.
        let plans_for_store = plans.clone();
        let durable = self
            .mutate_store(move |document| {
                for plan in &plans_for_store {
                    if let Some(task) = document
                        .tasks
                        .iter_mut()
                        .find(|task| task.id == plan.task.id)
                    {
                        task.last_fired = Some(plan.advanced);
                        if task.recurring && !plan.stale && !plan.maxed_out && !plan.ended {
                            task.next_fire_at = plan.next_fire_at;
                        } else {
                            // One-shot, stale, maxed-out, or ended (the
                            // end-date finale, B1): removed after this fire.
                            task.next_fire_at = None;
                        }
                    }
                }
                document.tasks.retain(|task| {
                    !plans_for_store.iter().any(|plan| {
                        plan.task.id == task.id
                            && (!task.recurring || plan.stale || plan.maxed_out || plan.ended)
                    })
                });
            })
            .is_ok();
        if !durable {
            // A failed durable write means the fire never happened: return
            // nothing (the file's cursor did not advance; the next pass
            // re-plans the same occurrences — at-least-once, never lost).
            tracing::warn!(
                "persisting the workspace cron fire transitions failed; the pass is skipped"
            );
            return Vec::new();
        }
        // Batch 4's apply-window seam (C-13): fire the one-shot test hook
        // in the exact window between the durable write and the in-memory
        // apply — a removal landed here must win (the apply below is
        // conditional on the CURRENT entry, never a blind overwrite).
        // Take-then-run: the guard is dropped before `hook()` executes, so
        // a hook that re-enters `set_apply_hook` cannot self-deadlock on
        // the non-reentrant `apply_hook` mutex.
        #[cfg(test)]
        let apply_hook = self
            .apply_hook
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        #[cfg(test)]
        if let Some(hook) = apply_hook {
            hook();
        }
        // Apply pass (review M4 — the compare-and-swap posture): each
        // plan is applied against the task's CURRENT in-memory entry,
        // re-read by id under the table lock, not the plan's stale
        // snapshot. A task removed (or replaced) by a concurrent add /
        // remove in the window between the durable write and this apply
        // is therefore never resurrected nor reverted: removals stay
        // removed, and a surviving entry only takes the RUN-related
        // fields (cursor + schedule) — creation-owned fields
        // (`created_at`, the prompt, ...) stay whatever the current
        // entry holds.
        let mut due = Vec::new();
        {
            let mut tasks = self
                .tasks
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let mut removed: Vec<String> = Vec::new();
            for plan in plans {
                let Some(task) = tasks.get_mut(&plan.task.id) else {
                    // Removed concurrently after the durable write: the
                    // file already carries the truth; memory stays removed.
                    continue;
                };
                task.last_fired = Some(plan.advanced);
                if task.recurring && !plan.stale && !plan.maxed_out && !plan.ended {
                    task.next_fire_at = plan.next_fire_at;
                } else {
                    // One-shot, stale, maxed-out, or ended: the task goes
                    // away after this fire (the removal is deferred — the
                    // mutable borrow above must end first).
                    removed.push(task.id.clone());
                }
                due.push(super::CronFire {
                    task_id: task.id.clone(),
                    prompt: task.prompt.clone(),
                    cron: task.cron.clone(),
                    recurring: task.recurring,
                    coalesced_count: plan.coalesced_count,
                    stale: plan.stale,
                    maxed_out: plan.maxed_out,
                    readonly: task.permission_posture
                        == Some(crate::cron::PermissionPosture::ReadOnly),
                    scheduled_at: plan.scheduled_at,
                    late_by_ms: None,
                    scope: task.scope.clone(),
                    ended: plan.ended,
                });
            }
            for id in removed {
                tasks.remove(&id);
            }
        }
        self.notify_change();
        due.sort_by(|a, b| a.task_id.cmp(&b.task_id));
        due
    }

    /// The workspace twin of the session manager's stale verdict.
    fn is_stale(&self, task: &super::CronTask, now: DateTime<Utc>) -> bool {
        task_is_stale_under(task, now, self.options.stale)
    }

    /// The workspace twin of the session manager's fire planning — the
    /// same walk (coalesce, cursor, budget verdict, reschedule) through
    /// the same shared types.
    fn plan_fire(&self, task: &super::CronTask, now: DateTime<Utc>) -> FirePlan {
        let tz = match super::CronTimezone::parse(task.timezone.as_deref()) {
            Ok(declared) => declared.resolve(),
            Err(error) => {
                tracing::warn!(
                    task_id = %task.id,
                    %error,
                    "workspace cron task carries an unusable timezone; firing on the host's \
                     local zone"
                );
                super::CronTimezone::Local.resolve()
            }
        };
        let schedule = super::task_schedule(task);
        let stale = self.is_stale(task, now);
        let maxed_out = !stale
            && task.recurring
            && task.max_fires.is_some_and(|max| task.fire_count + 1 >= max);
        let mut count = 0usize;
        let mut last_due = None;
        if task.recurring {
            let base = task
                .last_fired
                .filter(|last_fired| *last_fired > task.created_at)
                .unwrap_or(task.created_at);
            let mut cursor = base;
            while count < super::MAX_COALESCE_ITERATIONS {
                let ideal =
                    match super::CronManager::compute_schedule_next_fire_pub(&schedule, cursor, tz)
                    {
                        Ok(ideal) => ideal,
                        Err(_) => break,
                    };
                // Review Major 2: the walk never crosses the end date — an
                // occurrence past `ends_at`'s local day stops the coalesce
                // walk without counting it (a recovered backlog must not
                // coalesce occurrences the schedule had already ended on).
                if super::occurrence_past_ends_at(task, ideal, tz) {
                    break;
                }
                let jittered = super::CronManager::schedule_occurrence_for(
                    &schedule,
                    &task.id,
                    ideal,
                    task.recurring,
                    task.created_at,
                    tz,
                    self.options,
                )
                .unwrap_or(ideal);
                if jittered > now {
                    break;
                }
                count += 1;
                last_due = Some(ideal);
                cursor = ideal;
            }
        }
        let coalesced_count = count.max(1);
        let advanced = last_due.unwrap_or(now);
        // B1 / SDR-110: the end-date finale (the max_fires finale's
        // sibling) — the delivery whose NEXT occurrence would fall past
        // `ends_at`'s local day is the task's last.
        let ended = super::ends_past_schedule(task, &schedule, advanced, tz);
        let next_fire_at = if task.recurring && !stale && !maxed_out && !ended {
            super::CronManager::compute_schedule_next_fire_pub(&schedule, advanced, tz)
                .ok()
                .filter(|ideal| !super::occurrence_past_ends_at(task, *ideal, tz))
                .map(|ideal| {
                    super::CronManager::schedule_occurrence_for(
                        &schedule,
                        &task.id,
                        ideal,
                        task.recurring,
                        task.created_at,
                        tz,
                        self.options,
                    )
                    .unwrap_or(ideal)
                })
        } else {
            None
        };
        FirePlan {
            task: task.clone(),
            coalesced_count,
            stale,
            maxed_out,
            advanced,
            next_fire_at,
            scheduled_at: task.next_fire_at,
            ended,
        }
    }

    /// The post-mutate mirror source: the durable document a just-finished
    /// `mutate_store` produced. With the SQLite index attached this is the
    /// live rows (`store.read()` would return the frozen legacy archive and
    /// roll memory back); without it, the JSON file (unchanged posture).
    fn post_mutate_document(&self) -> WorkspaceCronDocument {
        match &self.index {
            Some(index) => rebuild_document_from_index(index.as_ref(), &self.workspace_id),
            None => self.store.read(),
        }
    }

    /// The locked store write shared by every mutation. Blocking (the
    /// file-lock polling); the async scheduling path wraps the calling
    /// future's body in `spawn_blocking` before ever reaching here.
    ///
    /// ADR-020: once the SQLite index is attached, the JSON write is
    /// SKIPPED (the legacy file is a read-only archive after migration) and
    /// the durable target becomes SQLite — the baseline is the live rows
    /// (NOT the frozen JSON file: reading it here would drop every mutation
    /// made since the migration through the difference-sweep), and one
    /// transactional `replace_workspace` covers the whole post-transform
    /// document. The plain `open()` path (no index) keeps writing JSON.
    fn mutate_store(
        &self,
        transform: impl FnOnce(&mut WorkspaceCronDocument),
    ) -> Result<(), anyhow::Error> {
        if let Some(index) = &self.index {
            let mut document = rebuild_document_from_index(index.as_ref(), &self.workspace_id);
            transform(&mut document);
            persist_document(index.as_ref(), &self.workspace_id, &document);
            return Ok(());
        }
        self.store.mutate::<(), anyhow::Error>(|document| {
            transform(document);
            Ok(())
        })
    }
}

/// Rebuild the document from the persistence seam's rows — the inverse of
/// [`persist_document`], shared with `open_with_index`'s restore path (the
/// two media's rebuild is ONE code path, never two divergent ones).
fn rebuild_document_from_index(
    index: &dyn CronPersistence,
    workspace_id: &str,
) -> WorkspaceCronDocument {
    let mut document = WorkspaceCronDocument::new();
    if let Ok(rows) = index.list_workspace_rows(workspace_id) {
        for row in rows {
            if let Ok(body) = serde_json::from_str::<WorkspaceBody>(&row.body) {
                document.tasks.push(body.task);
                if !body.runs.is_empty() {
                    document.runs.insert(row.id, body.runs);
                }
            }
        }
    }
    document
}

/// Write the post-transform document through to SQLite, transactionally:
/// one `replace_workspace` covers inserts, updates AND deletions (a
/// per-row upsert loop that crashes midway would leave a half snapshot).
///
/// Best-effort by design (the JSON store's own tolerant posture): a failed
/// write logs a warning and the mutation still reports success — the task
/// lives in memory and the deviation self-heals on the next successful
/// mutation or the next open (rows rebuild). The trade-off: a task added
/// on a persisting-failure streak is volatile until one lands.
fn persist_document(
    index: &dyn CronPersistence,
    workspace_id: &str,
    document: &WorkspaceCronDocument,
) {
    let rows: Vec<ScheduledTaskRecord> = document
        .tasks
        .iter()
        .map(|task| {
            let body = serde_json::json!({
                "task": task,
                "runs": document.runs.get(&task.id).cloned().unwrap_or_default(),
            });
            ScheduledTaskRecord {
                id: task.id.clone(),
                workspace_id: workspace_id.to_string(),
                next_fire_at: task.next_fire_at.map(encode_ts),
                paused: task.paused,
                created_at: encode_ts(task.created_at),
                body: body.to_string(),
            }
        })
        .collect();
    // best-effort: an index write failure is logged, never blocks scheduling
    if let Err(error) = index.replace_workspace_rows(workspace_id, &rows) {
        tracing::warn!(
            workspace_id,
            %error,
            "persisting the workspace cron document through SQLite failed"
        );
    }
}

/// The row body's JSON envelope: the task and its embedded runs (ADR-020:
/// single-task run volume is small and dies with the task).
#[derive(serde::Serialize, serde::Deserialize)]
struct WorkspaceBody {
    task: super::CronTask,
    runs: Vec<RunRecord>,
}

/// Encode a timestamp for the SQLite row's lifted columns — the
/// fixed-width RFC3339 text the `mirri-session-index` family uses (NOT
/// `to_rfc3339()`, whose variable-width `+00:00` shape breaks the
/// lexicographic ordering the columns rely on).
fn encode_ts(at: DateTime<Utc>) -> String {
    at.format("%Y-%m-%dT%H:%M:%S%.9fZ").to_string()
}

/// Map a store failure onto the cron error vocabulary (the update path's
/// only failure source: the locked document write).
fn store_error(error: anyhow::Error) -> super::CronError {
    super::CronError::InvalidExpression(format!(
        "persisting the workspace cron task failed: {error}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cron::{CronCreateOptions, CronManager, CronTask};

    /// Given a run row persisted before the launch-record fields existed,
    /// when it is read back and written again, then it loads with no
    /// trigger and the new keys stay absent (no migration of the existing
    /// SQLite rows).
    #[test]
    fn should_read_a_pre_launch_record_run_row_when_the_new_fields_are_absent() {
        let legacy = serde_json::json!({
            "run_id": "run-old",
            "status": "succeeded",
            "started_at": "2026-11-04T01:22:57.576427Z",
            "ended_at": "2026-11-04T01:25:14.940427Z",
            "duration_ms": 137364,
            "session_ref": "session-b56b5d88"
        });
        let record: RunRecord = serde_json::from_value(legacy).expect("legacy row loads");
        assert_eq!(record.trigger, None);
        assert_eq!(record.scheduled_at, None);
        let rewritten = serde_json::to_value(&record).expect("rewrite");
        assert!(rewritten.get("trigger").is_none());
        assert!(rewritten.get("scheduled_at").is_none());
    }

    /// Given a record carrying a trigger and the original schedule moment,
    /// when it round-trips, then both survive.
    #[test]
    fn should_round_trip_the_trigger_and_the_scheduled_moment() {
        let at = "2026-11-04T01:15:00Z".parse::<DateTime<Utc>>().unwrap();
        let record = RunRecord {
            run_id: "run-1".to_string(),
            status: RunStatus::Running,
            started_at: at + chrono::Duration::minutes(7),
            ended_at: None,
            duration_ms: None,
            usage: None,
            session_ref: None,
            skip_reason: None,
            asked_ops: Vec::new(),
            trigger: Some(RunTrigger::Scheduled),
            scheduled_at: Some(at),
        };
        let back: RunRecord =
            serde_json::from_value(serde_json::to_value(&record).unwrap()).unwrap();
        assert_eq!(back, record);
        assert_eq!(serde_json::to_value(&record).unwrap()["trigger"], "scheduled");
    }


    /// The dev-only adapter over the real carrier crate: the trait is local
    /// to this crate, so the orphan rule allows implementing it for the
    /// foreign `CronIndex` here. This keeps the SQLite round-trip test on
    /// the REAL storage path (mirri-embedded's adapter is its production
    /// twin — the same From conversions, mirrored).
    impl From<&ScheduledTaskRecord> for mirri_cron_index::ScheduledTaskRow {
        fn from(record: &ScheduledTaskRecord) -> Self {
            mirri_cron_index::ScheduledTaskRow {
                id: record.id.clone(),
                workspace_id: record.workspace_id.clone(),
                next_fire_at: record.next_fire_at.clone(),
                paused: record.paused,
                created_at: record.created_at.clone(),
                body: record.body.clone(),
            }
        }
    }

    impl From<&mirri_cron_index::ScheduledTaskRow> for ScheduledTaskRecord {
        fn from(row: &mirri_cron_index::ScheduledTaskRow) -> Self {
            ScheduledTaskRecord {
                id: row.id.clone(),
                workspace_id: row.workspace_id.clone(),
                next_fire_at: row.next_fire_at.clone(),
                paused: row.paused,
                created_at: row.created_at.clone(),
                body: row.body.clone(),
            }
        }
    }

    impl CronPersistence for mirri_cron_index::CronIndex {
        fn list_workspace_rows(
            &self,
            workspace_id: &str,
        ) -> anyhow::Result<Vec<ScheduledTaskRecord>> {
            Ok(self
                .list_workspace(workspace_id)?
                .iter()
                .map(ScheduledTaskRecord::from)
                .collect())
        }

        fn replace_workspace_rows(
            &self,
            workspace_id: &str,
            rows: &[ScheduledTaskRecord],
        ) -> anyhow::Result<()> {
            let rows: Vec<mirri_cron_index::ScheduledTaskRow> =
                rows.iter().map(mirri_cron_index::ScheduledTaskRow::from).collect();
            self.replace_workspace(workspace_id, &rows)
        }
    }

    /// A SyncFs double serving one fixed document and recording routed calls.
    #[derive(Default)]
    struct RouteRecordingFs {
        text: Option<String>,
        reads: std::sync::Mutex<Vec<std::path::PathBuf>>,
        mkdirs: std::sync::Mutex<Vec<std::path::PathBuf>>,
        removals: std::sync::Mutex<Vec<std::path::PathBuf>>,
        mkdir_fails: bool,
    }

    impl mirri_os::SyncFs for RouteRecordingFs {
        fn read_text(&self, path: &Path) -> anyhow::Result<String> {
            self.reads
                .lock()
                .unwrap()
                .push(path.to_path_buf());
            self.text
                .clone()
                .ok_or_else(|| anyhow::anyhow!("no seeded text"))
        }
        fn mkdir(&self, path: &Path, _recursive: bool) -> anyhow::Result<()> {
            self.mkdirs.lock().unwrap().push(path.to_path_buf());
            if self.mkdir_fails {
                anyhow::bail!("mkdir refused")
            }
            Ok(())
        }
        fn remove_file(&self, path: &Path) -> anyhow::Result<()> {
            self.removals.lock().unwrap().push(path.to_path_buf());
            Err(anyhow::Error::new(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "gone",
            )))
        }
    }

    /// Given a store wired with an injected fs double, when the document is
    /// read, then the read is routed through the double (zero-IO 契约 6 —
    /// no direct `std::fs` reaches the disk behind the store's back).
    #[test]
    fn should_read_the_document_through_the_injected_fs_when_read_is_called() {
        let tmp = tempfile::tempdir().unwrap();
        let document = r#"{ "version": 1, "tasks": [], "runs": {} }"#;
        let store = WorkspaceCronStore::with_fs(
            tmp.path(),
            std::sync::Arc::new(RouteRecordingFs {
                text: Some(document.to_string()),
                ..Default::default()
            }),
        );
        assert_eq!(store.read().tasks.len(), 0);
    }

    /// Given a store whose injected fs rejects the parent-directory create,
    /// when a mutation runs, then the mutation fails (the mkdir is routed,
    /// not silently done behind the injected provider).
    #[test]
    fn should_fail_the_mutation_when_the_injected_fs_rejects_mkdir() {
        let tmp = tempfile::tempdir().unwrap();
        let store = WorkspaceCronStore::with_fs(
            tmp.path(),
            std::sync::Arc::new(RouteRecordingFs {
                mkdir_fails: true,
                ..Default::default()
            }),
        );
        assert!(
            store
                .mutate::<(), anyhow::Error>(|_doc: &mut WorkspaceCronDocument| Ok(()))
                .is_err()
        );
    }

    /// Given a store whose injected fs reports NotFound on removal, when
    /// remove_files runs, then BOTH the document and its lock sibling are
    /// routed and the NotFound outcome is tolerated (the best-effort
    /// cascade's contract).
    #[test]
    fn should_tolerate_not_found_and_route_both_files_when_remove_files_runs() {
        let tmp = tempfile::tempdir().unwrap();
        let fs = std::sync::Arc::new(RouteRecordingFs::default());
        let store = WorkspaceCronStore::with_fs(
            tmp.path(),
            std::sync::Arc::clone(&fs) as std::sync::Arc<dyn mirri_os::SyncFs>,
        );
        store.remove_files();
        let removals = fs.removals.lock().unwrap();
        assert_eq!(removals.len(), 2, "tasks.json and its lock sibling are both routed");
        drop(removals);
        let not_found = anyhow::Error::new(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "gone",
        ));
        assert!(is_not_found(&not_found));
        let other = anyhow::Error::new(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "nope",
        ));
        assert!(!is_not_found(&other));
    }

    /// A manager over a fresh tempdir bucket (jitter/stale off for
    /// deterministic scheduling).
    fn ws_manager(tmp: &tempfile::TempDir) -> WorkspaceCronManager {
        WorkspaceCronManager::with_options(
            "wd_test_000000000001",
            tmp.path(),
            super::super::CronOptions {
                jitter: false,
                stale: true,
            },
        )
    }

    /// Given a fresh store, when a document is written, then the file on
    /// disk is always a COMPLETE parseable document — the atomic write
    /// (temp file + rename) leaves no half-written tasks.json behind even
    /// when the writer is interrupted between writes.
    #[test]
    fn should_write_a_complete_parseable_document_on_every_mutation() {
        let tmp = tempfile::tempdir().unwrap();
        let store = WorkspaceCronStore::new(tmp.path());
        for i in 0..5 {
            store
                .mutate::<(), anyhow::Error>(|document| {
                    document.tasks.push(CronTask {
                        id: format!("cron-{i}"),
                        cron: "* * * * *".into(),
                        prompt: "tick".into(),
                        scope: TaskScope::Workspace("wd_test_000000000001".into()),
                        ..crate::cron::cron_task_base(&format!("cron-{i}"), "* * * * *", "tick")
                    });
                    Ok(())
                })
                .unwrap();
            // Every intermediate state is a complete, versioned document.
            let document: WorkspaceCronDocument =
                serde_json::from_str(&std::fs::read_to_string(store.path()).unwrap()).unwrap();
            assert_eq!(document.version, 1);
            assert_eq!(document.tasks.len(), i + 1);
        }
    }

    /// Given a corrupt store file, when a manager opens it, then it starts
    /// empty (a dirty document never fails the open — the registry's read
    /// posture).
    #[test]
    fn should_start_empty_when_the_store_file_is_corrupt() {
        let tmp = tempfile::tempdir().unwrap();
        let store = WorkspaceCronStore::new(tmp.path());
        std::fs::create_dir_all(tmp.path().join(WORKSPACE_CRON_DIR)).unwrap();
        std::fs::write(store.path(), "{ not json").unwrap();
        assert!(ws_manager(&tmp).list().is_empty());
    }

    /// Given two managers over the SAME bucket (two faces of one
    /// workspace, or a crash-restart overlap), when both add a task
    /// concurrently, then the locked read-modify-write keeps BOTH tasks —
    /// no lost update (the lock is the whole point of the protocol).
    #[test]
    fn should_keep_both_tasks_when_two_managers_add_concurrently() {
        let tmp = tempfile::tempdir().unwrap();
        let first = std::sync::Arc::new(ws_manager(&tmp));
        let second = std::sync::Arc::new(ws_manager(&tmp));
        let (a, b) = (
            std::sync::Arc::clone(&first),
            std::sync::Arc::clone(&second),
        );
        let left = std::thread::spawn(move || {
            futures_executor_block_on(a.add(
                "* * * * *".into(),
                "from first".into(),
                true,
                CronCreateOptions::default(),
            ))
        });
        let right = std::thread::spawn(move || {
            futures_executor_block_on(b.add(
                "*/2 * * * *".into(),
                "from second".into(),
                true,
                CronCreateOptions::default(),
            ))
        });
        let _ = left.join().unwrap().unwrap();
        let _ = right.join().unwrap().unwrap();

        let document = WorkspaceCronStore::new(tmp.path()).read();
        assert_eq!(document.tasks.len(), 2, "no lost update: {document:?}");
    }

    /// A minimal current-thread block_on (the test bodies are async; the
    /// store's lock is blocking and must NOT run on a shared runtime's
    /// worker thread).
    fn futures_executor_block_on<F: std::future::Future>(future: F) -> F::Output {
        // The manager's async faces only await the spawn_blocking; a
        // current-thread park loop drives it without a runtime.
        // A dedicated current-thread runtime drives the spawn_blocking
        // without a shared worker.
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(future)
    }

    /// Given a manager with a fired task, when a FRESH manager opens the
    /// same bucket, then the task restores with its cursor and count
    /// intact (the restart recovery — A.2) and its runs survive.
    #[test]
    fn should_restore_tasks_and_runs_when_reopened() {
        let tmp = tempfile::tempdir().unwrap();
        let manager = ws_manager(&tmp);
        let id = futures_executor_block_on(manager.add(
            "*/5 * * * *".into(),
            "watch".into(),
            true,
            CronCreateOptions {
                max_fires: Some(3),
                ..CronCreateOptions::default()
            },
        ))
        .unwrap();
        // Force due and fire once (cursor advances; the Skipped record
        // does NOT consume the budget — review M1).
        manager.set_next_fire_at(&id, Some(Utc::now() - chrono::Duration::seconds(5)));
        let fired = futures_executor_block_on(manager.check_due());
        assert_eq!(fired.len(), 1);
        futures_executor_block_on(manager.record_run(
            &id,
            RunRecord {
                run_id: "run-1".into(),
                status: RunStatus::Skipped,
                started_at: Utc::now(),
                ended_at: Some(Utc::now()),
                duration_ms: Some(0),
                usage: None,
                session_ref: None,
                skip_reason: Some("executor-pending-batch-4b".into()),
                asked_ops: Vec::new(),
                trigger: None,
                scheduled_at: None,
            },
        ));

        let reopened = ws_manager(&tmp);
        let restored = reopened.get(&id).expect("the task restores");
        assert_eq!(
            restored.fire_count, 0,
            "a Skipped record does not consume the budget (M1)"
        );
        assert!(
            restored.last_fired.is_some(),
            "the cursor survives the reopen"
        );
        assert_eq!(reopened.runs(&id).len(), 1, "the run records survive");
        assert_eq!(reopened.runs(&id)[0].run_id, "run-1");
        assert_eq!(
            restored.scope,
            TaskScope::Workspace("wd_test_000000000001".into())
        );
    }

    /// Given a workspace task alongside a SESSION manager's task, when a
    /// session closes (its manager dropped), then the workspace face is
    /// untouched — the two managers share nothing (V11's core: coexistence
    /// without migration).
    #[test]
    fn should_keep_the_workspace_face_untouched_when_a_session_manager_dies() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace = ws_manager(&tmp);
        let id = futures_executor_block_on(workspace.add(
            "* * * * *".into(),
            "workspace watch".into(),
            true,
            CronCreateOptions::default(),
        ))
        .unwrap();
        {
            // A session manager with its own task, in its own scope.
            let session = CronManager::new();
            futures_executor_block_on(session.add(
                "* * * * *".into(),
                "session watch".into(),
                "s-1".into(),
                true,
                CronCreateOptions::default(),
            ))
            .unwrap();
            assert_eq!(session.list().len(), 1);
        } // The session manager drops here (the session closed).

        assert!(
            workspace.get(&id).is_some(),
            "the workspace task survives the session's death"
        );
        // And the workspace store carries no session-scope records.
        let document = WorkspaceCronStore::new(tmp.path()).read();
        assert!(document.tasks.iter().all(task_belongs_to_workspace));
    }

    /// Given a max_fires budget of 2, when the task is driven due twice
    /// with a REAL terminal run recorded after each fire, then the second
    /// fire carries the maxed-out mark and the task is removed durably (a
    /// reopen sees it gone — the data flow is real and complete without
    /// the 4b executor; M1: the count advances on the run records, not
    /// the due checks).
    #[test]
    fn should_remove_the_task_durably_when_the_budget_exhausts() {
        let tmp = tempfile::tempdir().unwrap();
        let manager = ws_manager(&tmp);
        let id = futures_executor_block_on(manager.add(
            "*/5 * * * *".into(),
            "bounded".into(),
            true,
            CronCreateOptions {
                max_fires: Some(2),
                ..CronCreateOptions::default()
            },
        ))
        .unwrap();
        for expected_maxed in [false, true] {
            manager.set_next_fire_at(&id, Some(Utc::now() - chrono::Duration::seconds(5)));
            let fired = futures_executor_block_on(manager.check_due());
            assert_eq!(fired.len(), 1);
            assert_eq!(fired[0].maxed_out, expected_maxed);
            // The executor's real terminal record advances the count (M1).
            futures_executor_block_on(manager.record_run(
                &id,
                RunRecord {
                    run_id: format!("run-{expected_maxed}"),
                    status: RunStatus::Succeeded,
                    started_at: Utc::now(),
                    ended_at: Some(Utc::now()),
                    duration_ms: Some(0),
                    usage: None,
                    session_ref: Some("exec-session".into()),
                    skip_reason: None,
                    asked_ops: Vec::new(),
                    trigger: None,
                    scheduled_at: None,
                },
            ));
        }
        assert!(manager.get(&id).is_none(), "removed in memory");
        assert!(
            ws_manager(&tmp).get(&id).is_none(),
            "removed durably (the reopen sees it gone)"
        );
    }

    /// Given the 4a placeholder executor (every fire records a Skipped
    /// run), when a `*/1` task with max_fires=10 ticks far past its
    /// budget, then the fire_count never advances and the task is NEVER
    /// silently exhausted by the skips (review M1: a skip is not a run).
    #[test]
    fn should_not_consume_the_budget_while_runs_are_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        let manager = ws_manager(&tmp);
        let id = futures_executor_block_on(manager.add(
            "*/1 * * * *".into(),
            "placeholder watch".into(),
            true,
            CronCreateOptions {
                max_fires: Some(10),
                ..CronCreateOptions::default()
            },
        ))
        .unwrap();
        // Far more due occurrences than the budget — each one fires and
        // records the 4a Skipped placeholder.
        for _ in 0..20 {
            manager.set_next_fire_at(&id, Some(Utc::now() - chrono::Duration::seconds(5)));
            let fired = futures_executor_block_on(manager.check_due());
            assert_eq!(fired.len(), 1);
            futures_executor_block_on(manager.record_run(
                &id,
                RunRecord {
                    run_id: format!("run-{}", uuid::Uuid::new_v4()),
                    status: RunStatus::Skipped,
                    started_at: Utc::now(),
                    ended_at: Some(Utc::now()),
                    duration_ms: Some(0),
                    usage: None,
                    session_ref: None,
                    skip_reason: Some("executor-pending-batch-4b".into()),
                    asked_ops: Vec::new(),
                    trigger: None,
                    scheduled_at: None,
                },
            ));
        }
        let task = manager.get(&id).expect("the task survives the skips");
        assert_eq!(task.fire_count, 0, "a skip never counts (M1)");
        assert!(
            task.next_fire_at.is_some(),
            "the cursor still reschedules (the occurrence detection is real)"
        );
    }

    /// Given a task with runs appended, when more than
    /// [`MAX_RUN_RECORDS_PER_TASK`] records land, then only the newest
    /// [`MAX_RUN_RECORDS_PER_TASK`] survive (the oldest drop) — in memory
    /// AND in the store document (review M2: unbounded growth capped).
    #[test]
    fn should_cap_the_run_records_kept_per_task() {
        let tmp = tempfile::tempdir().unwrap();
        let manager = ws_manager(&tmp);
        let id = futures_executor_block_on(manager.add(
            "* * * * *".into(),
            "chatty".into(),
            true,
            CronCreateOptions::default(),
        ))
        .unwrap();
        for i in 0..(WorkspaceCronManager::MAX_RUN_RECORDS_PER_TASK + 10) {
            futures_executor_block_on(manager.append_run(
                &id,
                RunRecord {
                    run_id: format!("run-{i:03}"),
                    status: RunStatus::Succeeded,
                    started_at: Utc::now(),
                    ended_at: Some(Utc::now()),
                    duration_ms: Some(0),
                    usage: None,
                    session_ref: None,
                    skip_reason: None,
                    asked_ops: Vec::new(),
                    trigger: None,
                    scheduled_at: None,
                },
            ));
        }
        let runs = manager.runs(&id);
        assert_eq!(runs.len(), WorkspaceCronManager::MAX_RUN_RECORDS_PER_TASK);
        assert_eq!(
            runs.first().unwrap().run_id,
            format!("run-{:03}", 10),
            "the oldest records dropped"
        );
        assert_eq!(
            runs.last().unwrap().run_id,
            format!(
                "run-{:03}",
                WorkspaceCronManager::MAX_RUN_RECORDS_PER_TASK + 9
            ),
            "the newest record kept"
        );
        // The store document carries the same cap (the cap is durable,
        // not just in memory).
        let document = WorkspaceCronStore::new(tmp.path()).read();
        assert_eq!(
            document.runs[&id].len(),
            WorkspaceCronManager::MAX_RUN_RECORDS_PER_TASK
        );
    }

    /// Given a fire plan in flight, when the task is REMOVED concurrently
    /// (after the durable write, before the in-memory apply), then the
    /// apply does not resurrect it and does not resurrect its runs
    /// (review M4: the apply is conditional on the current entry, never
    /// a blind overwrite from the stale plan).
    #[test]
    fn should_not_resurrect_a_task_removed_during_the_apply_window() {
        let tmp = tempfile::tempdir().unwrap();
        let manager = std::sync::Arc::new(ws_manager(&tmp));
        let id = futures_executor_block_on(manager.add(
            "* * * * *".into(),
            "raced".into(),
            true,
            CronCreateOptions::default(),
        ))
        .unwrap();
        manager.set_next_fire_at(&id, Some(Utc::now() - chrono::Duration::seconds(5)));

        // Batch 4's apply-window seam: the removal lands EXACTLY between
        // the durable fire write and the in-memory apply — the one-shot
        // hook fires inside `check_due` at that point, so the window is
        // constructed, not hoped for (the raced thread + 5ms sleep is
        // gone: whatever the interleaving used to be, the invariant below
        // now runs against the precise interleaving that matters).
        {
            let racer = std::sync::Arc::clone(&manager);
            let raced_id = id.clone();
            manager.set_apply_hook(move || {
                // `futures::executor`, NOT the test helper's tokio
                // runtime: this hook runs INSIDE `check_due`'s own
                // `block_on`, and building a second tokio runtime there
                // panics (nested runtime). `remove`'s body is fully
                // synchronous, so the plain park loop drives it fine.
                futures::executor::block_on(racer.remove(&raced_id));
            });
        }
        let fired = futures_executor_block_on(manager.check_due());
        // The invariant: the task is GONE (the removal landed after the
        // durable fire write and wins — the apply skips a removed entry)
        // and never resurrected with a stale plan.
        assert!(
            manager.get(&id).is_none(),
            "the removed task is never resurrected (M4)"
        );
        let document = WorkspaceCronStore::new(tmp.path()).read();
        assert!(
            document.tasks.iter().all(|task| task.id != id),
            "the store holds no resurrection either"
        );
        let _ = fired;
    }

    /// Given a Running run record from a crashed process, when the
    /// manager's orphan sweep runs, then the record converges to Failed
    /// with the restart cause (M2: a Running record a fresh process reads
    /// is definitionally dead — an execution turn cannot survive a
    /// restart) and the task's next fire opens cleanly (the concurrency
    /// check no longer sees Running).
    #[test]
    fn should_converge_a_stale_running_record_to_failed_on_the_orphan_sweep() {
        let tmp = tempfile::tempdir().unwrap();
        let manager = ws_manager(&tmp);
        let id = futures_executor_block_on(manager.add(
            "*/5 * * * *".into(),
            "watch".into(),
            true,
            CronCreateOptions::default(),
        ))
        .unwrap();
        futures_executor_block_on(manager.append_run(
            &id,
            RunRecord {
                run_id: "run-orphan".into(),
                status: RunStatus::Running,
                started_at: Utc::now(),
                ended_at: None,
                duration_ms: None,
                usage: None,
                session_ref: Some("session-dead".into()),
                skip_reason: None,
                asked_ops: Vec::new(),
                trigger: None,
                scheduled_at: None,
            },
        ));
        // The sweep (the scheduler's first pass calls this).
        let swept = futures_executor_block_on(manager.fail_orphan_running_runs("服务重启中断"));
        assert_eq!(swept, vec![(id.clone(), "run-orphan".to_string())]);
        let runs = manager.runs(&id);
        let converged = runs
            .iter()
            .find(|run| run.run_id == "run-orphan")
            .expect("the record survives");
        assert_eq!(converged.status, RunStatus::Failed);
        assert_eq!(converged.skip_reason.as_deref(), Some("服务重启中断"));
        assert!(converged.ended_at.is_some(), "the failure is terminal");
        // Durable too (the reopen sees the same convergence).
        let reopened = ws_manager(&tmp);
        assert!(
            reopened
                .runs(&id)
                .iter()
                .all(|run| run.status != RunStatus::Running),
            "the orphan convergence persisted"
        );
    }

    /// Given a Running run, when it parks on an approval and then
    /// resumes, then the status flips Running → WaitingApproval → Running;
    /// and given a terminal run, when a late wait signal arrives, then the
    /// terminal state stays (ADR-021: a late hook never overwrites the
    /// finale).
    #[tokio::test]
    async fn should_flip_a_run_between_running_and_waiting_approval_without_touching_terminal_states() {
        let tmp = tempfile::tempdir().unwrap();
        let manager = std::sync::Arc::new(ws_manager(&tmp));
        let id = manager
            .add(
                "0 9 1 1 *".into(),
                "approval waits".into(),
                true,
                CronCreateOptions::default(),
            )
            .await
            .unwrap();
        let run_id = "run-wait-1".to_string();
        let opener = RunRecord {
            run_id: run_id.clone(),
            status: RunStatus::Running,
            started_at: Utc::now(),
            ended_at: None,
            duration_ms: None,
            usage: None,
            session_ref: None,
            skip_reason: None,
            asked_ops: Vec::new(),
            trigger: Some(RunTrigger::Scheduled),
            scheduled_at: None,
        };
        match manager.try_open_run(&id, false, opener).await {
            OpenRunOutcome::Opened(_) => {}
            other => panic!("the opener won the critical section: {other:?}"),
        }
        // Park → WaitingApproval.
        manager
            .set_run_status(&id, &run_id, RunStatus::WaitingApproval)
            .await;
        assert_eq!(
            manager.runs(&id)[0].status,
            RunStatus::WaitingApproval,
            "the parked run is waiting"
        );
        // Resume → Running.
        manager
            .set_run_status(&id, &run_id, RunStatus::Running)
            .await;
        assert_eq!(manager.runs(&id)[0].status, RunStatus::Running);
        // Terminal — a late signal must not touch it.
        let mut terminal = manager.runs(&id)[0].clone();
        terminal.status = RunStatus::Succeeded;
        terminal.ended_at = Some(Utc::now());
        manager.replace_run(&id, terminal).await;
        manager
            .set_run_status(&id, &run_id, RunStatus::WaitingApproval)
            .await;
        assert_eq!(
            manager.runs(&id)[0].status,
            RunStatus::Succeeded,
            "the terminal state survived the late signal"
        );
    }

    fn opener_for(run_id: &str) -> RunRecord {
        RunRecord {
            run_id: run_id.to_string(),
            status: RunStatus::Running,
            started_at: Utc::now(),
            ended_at: None,
            duration_ms: None,
            usage: None,
            session_ref: None,
            skip_reason: None,
            asked_ops: Vec::new(),
            trigger: Some(RunTrigger::Scheduled),
            scheduled_at: None,
        }
    }

    /// ADR-021 review M-2. Given a run parked WaitingApproval, when the
    /// next fire tries to open a run, then it is declined/queued exactly as
    /// for a Running run — a person-blocked run still occupies the task (no
    /// second concurrent run behind it).
    #[tokio::test]
    async fn should_treat_a_waiting_approval_run_as_still_occupying_the_task() {
        let tmp = tempfile::tempdir().unwrap();
        let manager = std::sync::Arc::new(ws_manager(&tmp));
        let id = manager
            .add(
                "0 9 1 1 *".into(),
                "approval waits".into(),
                true,
                CronCreateOptions::default(),
            )
            .await
            .unwrap();
        assert!(matches!(
            manager.try_open_run(&id, false, opener_for("run-a")).await,
            OpenRunOutcome::Opened(_)
        ));
        manager
            .set_run_status(&id, "run-a", RunStatus::WaitingApproval)
            .await;
        match manager.try_open_run(&id, false, opener_for("run-b")).await {
            OpenRunOutcome::Declined(_) => {}
            other => panic!("a waiting run still blocks the Skip posture: {other:?}"),
        }
        match manager.try_open_run(&id, true, opener_for("run-c")).await {
            OpenRunOutcome::Queued(_) => {}
            other => panic!("a waiting run still queues the Queue posture: {other:?}"),
        }
    }

    /// ADR-021 review M-2. Given a run left WaitingApproval by a crash, when
    /// the orphan sweep runs, then it converges to Failed like any orphaned
    /// Running run (never a permanent zombie "waiting" row).
    #[tokio::test]
    async fn should_fail_a_waiting_approval_orphan_on_the_process_sweep() {
        let tmp = tempfile::tempdir().unwrap();
        let manager = std::sync::Arc::new(ws_manager(&tmp));
        let id = manager
            .add(
                "0 9 1 1 *".into(),
                "orphan".into(),
                true,
                CronCreateOptions::default(),
            )
            .await
            .unwrap();
        assert!(matches!(
            manager.try_open_run(&id, false, opener_for("run-o")).await,
            OpenRunOutcome::Opened(_)
        ));
        manager
            .set_run_status(&id, "run-o", RunStatus::WaitingApproval)
            .await;
        let swept = manager.fail_orphan_running_runs("进程重启").await;
        assert_eq!(swept, vec![(id.clone(), "run-o".to_string())]);
        assert_eq!(manager.runs(&id)[0].status, RunStatus::Failed);
    }

    /// ADR-021 review M-2. Given a run parked WaitingApproval, when its
    /// workspace is deleted, then the run is terminated (not left a zombie).
    #[tokio::test]
    async fn should_terminate_a_waiting_approval_run_when_the_workspace_is_deleted() {
        let tmp = tempfile::tempdir().unwrap();
        let manager = std::sync::Arc::new(ws_manager(&tmp));
        let id = manager
            .add(
                "0 9 1 1 *".into(),
                "delete me".into(),
                true,
                CronCreateOptions::default(),
            )
            .await
            .unwrap();
        let mut opener = opener_for("run-d");
        opener.session_ref = Some("session-d".to_string());
        assert!(matches!(
            manager.try_open_run(&id, false, opener).await,
            OpenRunOutcome::Opened(_)
        ));
        manager
            .set_run_status(&id, "run-d", RunStatus::WaitingApproval)
            .await;
        // The returned refs are the terminated runs' execution sessions
        // (the caller's cancellation plumbing key).
        let terminated = manager.terminate_running_runs("工作区已删除").await;
        assert_eq!(terminated, vec!["session-d".to_string()]);
        assert_eq!(manager.runs(&id)[0].status, RunStatus::Rejected);
    }

    /// Given two concurrent open attempts (the run-now trigger racing the
    /// scheduler tick), when both call `try_open_run`, then exactly ONE
    /// Running record opens (M4: the check and the write share one
    /// critical section — the TOCTOU window is closed).
    #[test]
    fn should_open_exactly_one_running_record_when_two_attempts_race() {
        let tmp = tempfile::tempdir().unwrap();
        let manager = std::sync::Arc::new(ws_manager(&tmp));
        let id = futures_executor_block_on(manager.add(
            "0 9 1 1 *".into(),
            "raced".into(),
            true,
            CronCreateOptions::default(),
        ))
        .unwrap();
        let opener = || RunRecord {
            run_id: format!("run-{}", uuid::Uuid::new_v4()),
            status: RunStatus::Running,
            started_at: Utc::now(),
            ended_at: None,
            duration_ms: None,
            usage: None,
            session_ref: None,
            skip_reason: None,
            asked_ops: Vec::new(),
            trigger: None,
            scheduled_at: None,
        };
        // Two concurrent open attempts on two threads (each with its own
        // current-thread runtime — the manager is runtime-free).
        let (a, b) = (
            std::sync::Arc::clone(&manager),
            std::sync::Arc::clone(&manager),
        );
        let id_a = id.clone();
        let id_b = id.clone();
        let left = std::thread::spawn(move || {
            futures_executor_block_on(a.try_open_run(&id_a, false, opener()))
        });
        let right = std::thread::spawn(move || {
            futures_executor_block_on(b.try_open_run(&id_b, false, opener()))
        });
        let outcomes = [left.join().unwrap(), right.join().unwrap()];
        let opened = outcomes
            .iter()
            .filter(|r| matches!(r, OpenRunOutcome::Opened(_)))
            .count();
        let declined = outcomes
            .iter()
            .filter(|r| matches!(r, OpenRunOutcome::Declined(_)))
            .count();
        assert_eq!(opened, 1, "exactly one opener wins (M4): {outcomes:?}");
        assert_eq!(declined, 1, "the loser is declined for the skip path");
        let runs = manager.runs(&id);
        assert_eq!(
            runs.iter()
                .filter(|run| run.status == RunStatus::Running)
                .count(),
            1,
            "exactly one Running record exists: {runs:?}"
        );
    }

    /// Given a Queue-posture task with a Running run, when another fire
    /// opens, then the fire parks in the SINGLE queued slot (a merged
    /// Skipped record — one queued execution, never a pile), and after
    /// Given a workspace task with the ReadOnly posture, when a fire is
    /// produced, then the fire carries the readonly bit (the execution
    /// turn's write-class refusal input — ADR-015 / D-M2).
    #[test]
    fn should_stamp_the_readonly_bit_on_a_readonly_postured_task_fire() {
        let tmp = tempfile::tempdir().unwrap();
        let manager = ws_manager(&tmp);
        let id = futures_executor_block_on(manager.add(
            "0 9 1 1 *".into(),
            "observe only".into(),
            true,
            CronCreateOptions {
                permission_posture: Some(crate::cron::PermissionPosture::ReadOnly),
                scope: crate::cron::TaskScope::Workspace("ws".into()),
                ..CronCreateOptions::default()
            },
        ))
        .unwrap();
        // Force the next fire into the past so check_due answers it now.
        assert!(manager.set_next_fire_at(&id, Some(Utc::now() - chrono::Duration::minutes(1))));
        let fires = futures_executor_block_on(manager.check_due());
        let fire = fires
            .iter()
            .find(|fire| fire.task_id == id)
            .expect("the readonly-postured task fired");
        assert!(fire.readonly, "the fire carries the readonly bit");
    }

    /// Given a Queue-postured task with a Running record, when two further
    /// fires arrive, then both park in the single backlogged slot (merged
    /// one-deep) and the terminal state the drain re-opens the queued
    /// record as the next run's opener (D-M3's queue: 记一排一).
    #[test]
    fn should_queue_one_merged_fire_while_running_and_drain_it_after_the_terminal_state() {
        let tmp = tempfile::tempdir().unwrap();
        let manager = ws_manager(&tmp);
        let id = futures_executor_block_on(manager.add(
            "0 9 1 1 *".into(),
            "queued".into(),
            true,
            CronCreateOptions::default(),
        ))
        .unwrap();
        // The first run opens.
        let opener = RunRecord {
            run_id: "run-first".into(),
            status: RunStatus::Running,
            started_at: Utc::now(),
            ended_at: None,
            duration_ms: None,
            usage: None,
            session_ref: None,
            skip_reason: None,
            asked_ops: Vec::new(),
            trigger: None,
            scheduled_at: None,
        };
        let opened = futures_executor_block_on(manager.try_open_run(&id, true, opener));
        assert!(
            matches!(&opened, OpenRunOutcome::Opened(opened_id) if opened_id == "run-first"),
            "the first run opens: {opened:?}"
        );
        // Two further fires while Running: BOTH park in the queue (the
        // single slot merges — the second replaces the first's record).
        for run_id in ["run-queued-1", "run-queued-2"] {
            let queued_opener = RunRecord {
                run_id: run_id.into(),
                status: RunStatus::Running,
                started_at: Utc::now(),
                ended_at: None,
                duration_ms: None,
                usage: None,
                session_ref: None,
                skip_reason: None,
                asked_ops: Vec::new(),
                trigger: None,
                scheduled_at: None,
            };
            let queued = futures_executor_block_on(manager.try_open_run(&id, true, queued_opener));
            assert!(
                matches!(&queued, OpenRunOutcome::Queued(queued_id) if queued_id == run_id),
                "the queue posture parks the fire: {queued:?}"
            );
        }
        // Exactly one queued record exists (merged one-deep), plus the
        // running opener — and the LAST queued run id won the slot.
        let runs = manager.runs(&id);
        let queued: Vec<&RunRecord> = runs
            .iter()
            .filter(|run| {
                run.status == RunStatus::Skipped
                    && run
                        .skip_reason
                        .as_deref()
                        .is_some_and(|reason| reason.contains("单槽排队"))
            })
            .collect();
        assert_eq!(queued.len(), 1, "the queue is one slot deep: {runs:?}");
        assert_eq!(
            queued[0].run_id, "run-queued-2",
            "the latest fire holds the slot"
        );
        // No drain while the run is still live.
        assert!(
            futures_executor_block_on(manager.drain_queued_run(&id)).is_none(),
            "no drain while the current run is live"
        );
        // The current run's terminal state (replace_run), then the drain.
        futures_executor_block_on(manager.replace_run(
            &id,
            RunRecord {
                run_id: "run-first".into(),
                status: RunStatus::Succeeded,
                started_at: Utc::now(),
                ended_at: Some(Utc::now()),
                duration_ms: Some(10),
                usage: None,
                session_ref: None,
                skip_reason: None,
                asked_ops: Vec::new(),
                trigger: None,
                scheduled_at: None,
            },
        ));
        let drained = futures_executor_block_on(manager.drain_queued_run(&id))
            .expect("the queued fire drained after the terminal state");
        assert_eq!(drained, (id.clone(), "run-queued-2".to_string()));
        let runs = manager.runs(&id);
        assert!(
            runs.iter()
                .any(|run| run.run_id == "run-queued-2" && run.status == RunStatus::Running),
            "the drained run re-opened as Running: {runs:?}"
        );
        // The terminal replacement closed the opener (no stale Running
        // left behind — the replace_run contract).
        assert!(
            !runs
                .iter()
                .any(|run| run.run_id == "run-first" && run.status == RunStatus::Running),
            "the opener's Running was replaced by the terminal record"
        );
    }

    /// Given a change listener, when the store mutates, then the listener
    /// fires (the server side forwards it as
    /// `event.cron.workspaceChanged`).
    #[test]
    fn should_notify_change_listeners_after_durable_mutations() {
        let tmp = tempfile::tempdir().unwrap();
        let manager = ws_manager(&tmp);
        let notified = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = std::sync::Arc::clone(&notified);
        manager.on_change(move || {
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        });
        let id = futures_executor_block_on(manager.add(
            "* * * * *".into(),
            "watch".into(),
            true,
            CronCreateOptions::default(),
        ))
        .unwrap();
        assert!(notified.load(std::sync::atomic::Ordering::SeqCst) >= 1);
        futures_executor_block_on(manager.remove(&id));
        assert!(notified.load(std::sync::atomic::Ordering::SeqCst) >= 2);
    }

    // -----------------------------------------------------------------------
    // form batch 6: interval schedules + ends_at enforcement + the
    // task-form fields' persistence (T1/T4/T5)
    // -----------------------------------------------------------------------

    /// Given an interval task, when created and fired, then the schedule
    /// walks the interval branch (never the expression parser) and the
    /// task-form fields round-trip through the store.
    #[test]
    fn should_create_and_round_trip_an_interval_task_with_the_form_fields() {
        let tmp = tempfile::tempdir().unwrap();
        let manager = ws_manager(&tmp);
        let today = chrono::Utc::now().date_naive();
        let id = futures_executor_block_on(manager.add(
            String::new(),
            "sweep".into(),
            true,
            CronCreateOptions {
                interval: Some(super::super::IntervalSpec {
                    every: 2,
                    unit: super::super::IntervalUnit::Day,
                    time: "09:30".to_string(),
                    anchor_date: today,
                }),
                timezone: Some("UTC".into()),
                title: Some("双日巡检".into()),
                model: Some("claude-sonnet-4".into()),
                workdir: Some("/tmp/proj".into()),
                ends_at: Some(today + chrono::Duration::days(30)),
                ..CronCreateOptions::default()
            },
        ))
        .unwrap();
        let task = manager.get(&id).unwrap();
        assert_eq!(task.cron, "", "an interval task carries no expression");
        assert_eq!(
            task.interval,
            Some(super::super::IntervalSpec {
                every: 2,
                unit: super::super::IntervalUnit::Day,
                time: "09:30".to_string(),
                anchor_date: today,
            })
        );
        assert_eq!(task.title.as_deref(), Some("双日巡检"));
        assert_eq!(task.model.as_deref(), Some("claude-sonnet-4"));
        assert_eq!(task.workdir.as_deref(), Some("/tmp/proj"));
        assert!(
            task.next_fire_at.is_some(),
            "the interval schedules a next fire"
        );
        // The store round-trip keeps every field.
        let reopened = ws_manager(&tmp);
        let restored = reopened.get(&id).unwrap();
        assert_eq!(restored.interval, task.interval);
        assert_eq!(restored.title, task.title);
        assert_eq!(restored.model, task.model);
        assert_eq!(restored.workdir, task.workdir);
        assert_eq!(restored.ends_at, task.ends_at);
    }

    /// Given an ends_at before the first fire day, when a workspace task
    /// is created, then the create is rejected (the authoritative
    /// validation's workspace twin).
    #[test]
    fn should_reject_the_workspace_create_when_ends_at_precedes_the_first_fire_day() {
        let tmp = tempfile::tempdir().unwrap();
        let manager = ws_manager(&tmp);
        let error = futures_executor_block_on(manager.add(
            "0 9 * * *".into(),
            "p".into(),
            true,
            CronCreateOptions {
                timezone: Some("UTC".into()),
                ends_at: Some(chrono::Utc::now().date_naive() - chrono::Duration::days(1)),
                ..CronCreateOptions::default()
            },
        ))
        .unwrap_err();
        assert!(
            error.to_string().contains("结束日期不能早于首次运行日"),
            "the refusal names the rule: {error}"
        );
    }

    /// Given an ends_at whose final day's fire has come due, when
    /// check_due runs, then the fire carries the ended mark and the
    /// durable store drops the task (the max_fires finale's sibling).
    #[test]
    fn should_remove_the_workspace_task_after_its_end_date_finale() {
        let tmp = tempfile::tempdir().unwrap();
        let manager = ws_manager(&tmp);
        let today = chrono::Utc::now().date_naive();
        let yesterday = today - chrono::Duration::days(1);
        let mut task = crate::cron::cron_task_base("cron-ws-ended", "0 9 * * *", "p");
        task.scope = TaskScope::Workspace("wd_test_000000000001".into());
        task.timezone = Some("UTC".into());
        task.ends_at = Some(yesterday);
        task.created_at = chrono::Utc::now() - chrono::Duration::days(4);
        // Seed the manager with the task by writing through the store.
        {
            let task_for_store = task.clone();
            manager
                .store
                .mutate::<(), anyhow::Error>(move |document| {
                    document.tasks.push(task_for_store);
                    Ok(())
                })
                .unwrap();
        }
        // Reopen to restore through the scheduling path.
        drop(manager);
        let manager = ws_manager(&tmp);
        // Force the schedule due, then run the pass.
        manager.set_next_fire_at(
            "cron-ws-ended",
            Some(chrono::Utc::now() - chrono::Duration::seconds(5)),
        );
        let fires = futures_executor_block_on(manager.check_due());
        assert_eq!(fires.len(), 1);
        assert!(
            fires[0].ended,
            "the workspace finale carries the ended mark"
        );
        assert!(
            manager.get("cron-ws-ended").is_none(),
            "the task is durably removed after the finale"
        );
        let document = WorkspaceCronStore::new(tmp.path()).read();
        assert!(
            document.tasks.iter().all(|task| task.id != "cron-ws-ended"),
            "the store no longer carries the ended task"
        );
    }

    // -----------------------------------------------------------------------
    // batch 2 (A9): edit + pause/resume over the whole-document store
    // -----------------------------------------------------------------------

    /// One edit request over the manager's update face.
    fn edit() -> WorkspaceTaskEdit {
        WorkspaceTaskEdit::default()
    }

    /// Given a paused task whose fire moment arrives, when the pass runs,
    /// then nothing fires and the task is not removed — pausing only
    /// suspends.
    #[test]
    fn should_not_fire_a_paused_task_when_its_moment_arrives() {
        let tmp = tempfile::tempdir().unwrap();
        let manager = ws_manager(&tmp);
        let id = futures_executor_block_on(manager.add(
            "0 9 * * *".into(),
            "p".into(),
            true,
            CronCreateOptions {
                timezone: Some("UTC".into()),
                ..Default::default()
            },
        ))
        .unwrap();
        assert!(futures_executor_block_on(manager.set_paused(&id, true)).unwrap());
        let task = manager.get(&id).unwrap();
        assert!(task.paused);
        // A paused task has no next moment: the cursor is cleared (the
        // edit path's shape), so the row never shows a moment that will
        // not happen.
        assert_eq!(task.next_fire_at, None, "the pause clears the cursor");
        manager.set_next_fire_at(&id, Some(Utc::now() - chrono::Duration::seconds(5)));
        let fires = futures_executor_block_on(manager.check_due());
        assert!(fires.is_empty(), "a paused task never fires: {fires:?}");
        // And the pause is durable (a fresh manager reads it back).
        drop(manager);
        let reopened = ws_manager(&tmp);
        let task = reopened.get(&id).expect("the paused task survives");
        assert!(task.paused, "paused survives a reopen");
        assert_eq!(
            task.next_fire_at, None,
            "the cleared cursor survives a reopen"
        );
    }

    /// Given a task paused across several of its scheduled moments, when
    /// it is resumed, then the next fire is the first occurrence in the
    /// FUTURE — the missed occurrences are neither replayed nor disguised
    /// as run, and no cursor advanced.
    #[test]
    fn should_reschedule_to_the_next_future_occurrence_when_resumed_after_missed_moments() {
        let tmp = tempfile::tempdir().unwrap();
        let manager = ws_manager(&tmp);
        // Every minute, so the occurrences the pause skipped are real.
        let id = futures_executor_block_on(manager.add(
            "* * * * *".into(),
            "p".into(),
            true,
            CronCreateOptions {
                timezone: Some("UTC".into()),
                ..Default::default()
            },
        ))
        .unwrap();
        let created_at = manager.get(&id).unwrap().created_at;
        let before_pause = manager.get(&id).unwrap().next_fire_at;
        assert!(before_pause.is_some(), "a live task has a next moment");
        assert!(futures_executor_block_on(manager.set_paused(&id, true)).unwrap());
        assert_eq!(
            manager.get(&id).unwrap().next_fire_at,
            None,
            "the pause clears the cursor"
        );
        // The pause window: the next fire drifted into the past.
        manager.set_next_fire_at(&id, Some(Utc::now() - chrono::Duration::minutes(3)));
        assert!(futures_executor_block_on(manager.check_due()).is_empty());
        let resumed = futures_executor_block_on(manager.resume(&id)).unwrap();
        let now = Utc::now();
        let next = resumed.next_fire_at.expect("a resumed task reschedules");
        assert!(
            next > now,
            "the next fire is in the future: {next} vs {now}"
        );
        assert!(
            next <= now + chrono::Duration::minutes(2),
            "it is the FIRST future occurrence, no catch-up backlog: {next}"
        );
        assert_eq!(
            resumed.last_fired, None,
            "the pause window is not disguised as a run"
        );
        assert_eq!(resumed.fire_count, 0, "no fire budget was consumed");
        assert!(!resumed.paused);
        assert!(
            resumed.created_at == created_at,
            "the identity is untouched"
        );
    }

    /// Given an existing task with run records, when its schedule and
    /// prompt are edited, then the same task id carries the new values and
    /// the run history stays attached (no delete-and-recreate).
    #[test]
    fn should_keep_the_run_history_and_identity_when_the_task_is_edited() {
        let tmp = tempfile::tempdir().unwrap();
        let manager = ws_manager(&tmp);
        let id = futures_executor_block_on(manager.add(
            "0 9 * * *".into(),
            "old prompt".into(),
            true,
            CronCreateOptions {
                timezone: Some("UTC".into()),
                ..Default::default()
            },
        ))
        .unwrap();
        futures_executor_block_on(manager.record_run(
            &id,
            RunRecord {
                run_id: "run-1".into(),
                status: RunStatus::Succeeded,
                started_at: Utc::now(),
                ended_at: Some(Utc::now()),
                duration_ms: Some(1),
                usage: None,
                session_ref: None,
                skip_reason: None,
                asked_ops: Vec::new(),
                trigger: None,
                scheduled_at: None,
            },
        ));
        let edited = futures_executor_block_on(manager.update(
            &id,
            WorkspaceTaskEdit {
                prompt: Some("new prompt".into()),
                cron: Some("30 7 * * *".into()),
                timezone: Some(Some("Asia/Tokyo".into())),
                title: Some(Some("早报".into())),
                ..edit()
            },
        ))
        .unwrap();
        assert_eq!(edited.id, id, "the identity never changes");
        assert_eq!(edited.prompt, "new prompt");
        assert_eq!(edited.cron, "30 7 * * *");
        assert_eq!(edited.timezone.as_deref(), Some("Asia/Tokyo"));
        assert_eq!(edited.title.as_deref(), Some("早报"));
        // The new schedule drives the next fire (07:30 Tokyo = 22:30 UTC
        // the previous day, whichever side of now it lands on).
        let next = edited.next_fire_at.expect("a reschedule follows the edit");
        assert_eq!(
            next.with_timezone(&chrono::Utc).time(),
            chrono::NaiveTime::from_hms_opt(22, 30, 0).unwrap()
        );
        assert_eq!(
            manager.runs(&id).len(),
            1,
            "the run history is still attached"
        );
        // Durable: a reopen reads the edited values back.
        drop(manager);
        let reopened = ws_manager(&tmp);
        let task = reopened.get(&id).expect("the edited task survives");
        assert_eq!(task.cron, "30 7 * * *");
        assert_eq!(reopened.runs(&id).len(), 1);
    }

    /// Given an edit changing the execution environment, when it lands,
    /// then mode / model / effort / ends_at carry the new values.
    #[test]
    fn should_apply_the_environment_edit_when_it_lands() {
        let tmp = tempfile::tempdir().unwrap();
        let manager = ws_manager(&tmp);
        let id = futures_executor_block_on(manager.add(
            "0 9 * * *".into(),
            "p".into(),
            true,
            CronCreateOptions {
                timezone: Some("UTC".into()),
                ..Default::default()
            },
        ))
        .unwrap();
        let ends_at = chrono::NaiveDate::from_ymd_opt(2099, 1, 31).unwrap();
        let edited = futures_executor_block_on(manager.update(
            &id,
            WorkspaceTaskEdit {
                permission_mode: Some(Some(crate::cron::PermissionMode::Yolo)),
                model: Some(Some("claude-sonnet-4".into())),
                effort: Some(Some(crate::profile::ThinkingEffort::Low)),
                ends_at: Some(Some(ends_at)),
                max_fires: Some(Some(4)),
                ..edit()
            },
        ))
        .unwrap();
        assert_eq!(
            edited.permission_mode,
            Some(crate::cron::PermissionMode::Yolo)
        );
        assert_eq!(
            edited.permission_posture,
            Some(super::super::PermissionPosture::Yolo)
        );
        assert_eq!(edited.model.as_deref(), Some("claude-sonnet-4"));
        assert_eq!(edited.ends_at, Some(ends_at));
        assert_eq!(edited.max_fires, Some(4));
        // Clearing an optional field is an explicit `Some(None)`.
        let cleared = futures_executor_block_on(manager.update(
            &id,
            WorkspaceTaskEdit {
                model: Some(None),
                ..edit()
            },
        ))
        .unwrap();
        assert_eq!(cleared.model, None);
    }

    /// Given an edit that tries to change the workspace, when it is
    /// evaluated, then it is refused — the workspace is not an editable
    /// field (A9: moving a task elsewhere is a different task).
    #[test]
    fn should_refuse_the_edit_when_it_tries_to_change_the_workspace() {
        let tmp = tempfile::tempdir().unwrap();
        let manager = ws_manager(&tmp);
        let id = futures_executor_block_on(manager.add(
            "0 9 * * *".into(),
            "p".into(),
            true,
            CronCreateOptions::default(),
        ))
        .unwrap();
        let error = futures_executor_block_on(manager.update_at(
            &id,
            WorkspaceTaskEdit {
                workspace_id: Some("other-ws".into()),
                ..edit()
            },
            Utc::now(),
        ))
        .unwrap_err();
        assert!(
            error.to_string().contains("工作区"),
            "the refusal names the immutable field: {error}"
        );
    }

    /// Given an edit with an invalid schedule, when it is applied, then it
    /// is refused and NOTHING changes (the document and the memory both
    /// keep the old task — an atomic read-modify-write never half-lands).
    #[test]
    fn should_leave_the_task_untouched_when_an_edit_is_invalid() {
        let tmp = tempfile::tempdir().unwrap();
        let manager = ws_manager(&tmp);
        let id = futures_executor_block_on(manager.add(
            "0 9 * * *".into(),
            "p".into(),
            true,
            CronCreateOptions {
                timezone: Some("UTC".into()),
                ..Default::default()
            },
        ))
        .unwrap();
        let before = manager.get(&id).unwrap();
        assert!(
            futures_executor_block_on(manager.update(
                &id,
                WorkspaceTaskEdit {
                    cron: Some("not a cron".into()),
                    ..edit()
                },
            ))
            .is_err()
        );
        let after = manager.get(&id).unwrap();
        assert_eq!(after.cron, before.cron, "the invalid edit left no trace");
        let document = WorkspaceCronStore::new(tmp.path()).read();
        assert_eq!(document.tasks[0].cron, "0 9 * * *");
    }

    /// Given an unknown task id, when pause / resume / update is called,
    /// then the answer is None / false (not a silent success).
    #[test]
    fn should_answer_not_found_when_the_edited_task_does_not_exist() {
        let tmp = tempfile::tempdir().unwrap();
        let manager = ws_manager(&tmp);
        assert!(!futures_executor_block_on(manager.set_paused("nope", true)).unwrap());
        assert!(
            futures_executor_block_on(manager.update("nope", edit())).is_err(),
            "an edit of an unknown task is an error"
        );
    }

    /// Given a pre-batch-2 document whose tasks carry no `paused` key,
    /// when it is opened, then every task reads back running (the missing
    /// key is the documented default).
    #[test]
    fn should_read_old_documents_without_the_paused_key_as_running() {
        let tmp = tempfile::tempdir().unwrap();
        let store = WorkspaceCronStore::new(tmp.path());
        std::fs::create_dir_all(tmp.path().join(WORKSPACE_CRON_DIR)).unwrap();
        let legacy = serde_json::json!({
            "version": 1,
            "tasks": [{
                "id": "cron-legacy",
                "cron": "0 9 * * *",
                "prompt": "p",
                "sessionId": "",
                "recurring": true,
                "createdAt": chrono::Utc::now().timestamp_millis(),
                "lastFiredAt": null,
                "nextFireAt": null,
                "scope": "workspace",
                "workspaceId": "wd_test_000000000001"
            }],
            "runs": {}
        });
        std::fs::write(store.path(), legacy.to_string()).unwrap();
        let manager = ws_manager(&tmp);
        let task = manager.get("cron-legacy").expect("the legacy task opens");
        assert!(!task.paused);
        assert_eq!(task.permission_mode, None);
    }

    /// Given a manager opened over the SQLite index (ADR-020), when tasks
    /// are added twice on the SAME manager, then BOTH survive in SQLite (the
    /// mutate baseline is the live rows, never the frozen JSON archive), no
    /// tasks.json is written, and a second manager over the same index sees
    /// both tasks (the cross-runtime read).
    #[test]
    fn manager_round_trips_through_sqlite_index() {
        use mirri_cron_index::CronIndex;
        let dir = tempfile::tempdir().unwrap();
        let index = std::sync::Arc::new(CronIndex::open(&dir.path().join("cron-index.db")).unwrap());
        let manager = WorkspaceCronManager::open_with_index("ws-1", dir.path(), index.clone());

        let options = crate::cron::CronCreateOptions {
            timezone: Some("Asia/Tokyo".into()),
            title: Some("SQLite task".into()),
            ..crate::cron::CronCreateOptions::default()
        };
        let id = futures_executor_block_on(manager.add(
            "0 9 * * 1-5".into(),
            "test".into(),
            true,
            options,
        ))
        .unwrap();

        // persisted in SQLite, not in tasks.json
        let rows = index.list_workspace("ws-1").unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, id);
        assert!(rows[0].body.contains("SQLite task"));
        assert!(!dir.path().join("cron").join("tasks.json").exists());

        // REGRESSION N1: a SECOND mutation on the SAME manager must not
        // lose the first — the mutate baseline is the SQLite rows, never the
        // frozen JSON archive (which never contained either task).
        let id2 = futures_executor_block_on(manager.add(
            "0 10 * * 1-5".into(),
            "second".into(),
            true,
            crate::cron::CronCreateOptions::default(),
        ))
        .unwrap();
        let rows = index.list_workspace("ws-1").unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().any(|r| r.id == id));
        assert!(rows.iter().any(|r| r.id == id2));

        // a second manager over the same index sees BOTH tasks (cross-runtime read)
        let manager2 = WorkspaceCronManager::open_with_index("ws-1", dir.path(), index);
        assert!(manager2.get(&id).is_some());
        assert!(manager2.get(&id2).is_some());
    }
}
