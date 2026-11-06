//! Cron — session-level cron scheduling.
//!
//! [`CronManager`] owns the set of [`CronTask`]s belonging to one session.
//! The session's event loop calls [`CronManager::check_due`] periodically
//! (e.g. once per loop iteration); every task whose
//! [`next_fire_at`](CronTask::next_fire_at) has passed yields its prompt,
//! which the event loop injects into the live conversation.
//!
//! Expressions are standard 5-field cron expressions evaluated on a local
//! calendar: by default the host's local timezone (the TS kernel's
//! semantics), or the task's explicit IANA `timezone` when one is recorded
//! (B5 / DEC-3 — `"UTC"` remains available as an explicit escape hatch):
//!
//! ```text
//! ┌───────────── minute (0 - 59)
//! │ ┌─────────── hour (0 - 23)
//! │ │ ┌───────── day of month (1 - 31)
//! │ │ │ ┌─────── month (1 - 12)
//! │ │ │ │ ┌───── day of week (0 - 6; Sunday to Saturday, 7 is also Sunday)
//! │ │ │ │ │
//! * * * * *
//! ```
//!
//! Each field supports:
//! - `*` — any value (a bare wildcard);
//! - a specific value (`5`);
//! - an inclusive range (`1-5`);
//! - a list (`1,3,5`);
//! - step values (`*/5`, `1-10/3`, `5/15` — the latter runs from `5` to the
//!   field maximum in steps of 15).
//!
//! When **both** the day-of-month and day-of-week fields are restricted (not
//! a bare `*`), a day matches when *either* field matches, following standard
//! Vixie-cron OR semantics. When either is a bare `*`, both must match (the
//! unrestricted field is ignored).
//!
//! The parser is deliberately small and dependency-free: no seconds, no
//! year fields, no timezone conversion, no named months or weekdays.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

/// LLM-facing rendering (the `<cron-fire>` envelope).
pub mod format;
/// The workspace-level cron store + shared fire-planning types (stage-2
/// batch-4a, T2/T3).
pub mod workspace_store;

pub use workspace_store::{
    CRON_PROPOSAL_TTL, CronPersistence, CronProposal, OpenRunOutcome, ProposalState, RunRecord,
    RunStatus, RunTrigger, ScheduledTaskRecord, WORKSPACE_CRON_DIR, WORKSPACE_CRON_TASKS_FILENAME,
    WorkspaceCronDocument, WorkspaceCronManager, WorkspaceCronStore,
};

use chrono::{DateTime, Datelike, Duration, NaiveDateTime, Timelike, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

pub mod jitter;
pub mod tz;

pub use jitter::{
    DEFAULT_CRON_JITTER_CONFIG, JitterConfig, jittered_next_run, one_shot_jittered_next,
};
pub use tz::{CronTimezone, format_local_iso_with_offset};

use crate::scope::SessionWireLog;
use mirri_proto::WireOp;

/// How far into the future `compute_next_fire` searches for the next
/// occurrence. Ten years covers the longest legitimate gap between fires of a
/// valid 5-field expression (e.g. Feb 29 in years where the next leap day is
/// up to 8 years away, like 2096 → 2104).
const SEARCH_HORIZON_DAYS: i64 = 366 * 10;

/// How many overdue occurrences one fire may coalesce (the TS
/// `MAX_COALESCE_ITERATIONS`): a task that missed a very long downtime
/// delivers one merged fire, never a burst of catch-up prompts.
pub const MAX_COALESCE_ITERATIONS: usize = 10_000;

/// A recurring task whose last successful fire (or creation, when it has
/// never fired) is older than this fires once more (flagged stale) and is
/// then removed (the TS `STALE_THRESHOLD_MS`, re-based to the last fire —
/// B.1 / D-M5 fire-refresh: an active task never stales, a stalled one is
/// bounded). One-shots never stale — they are removed by their one-fire
/// rule anyway.
pub const STALE_THRESHOLD: Duration = Duration::days(7);

/// The model-created session cron task budget gate (SDR-006 / D-M8,
/// stage-1 batch-1 S8): a declared fire budget at or below this value (or
/// a one-shot, which counts as 1) passes without asking; above it (or
/// unbounded) the creation asks an answerable posture and denies
/// auto/unattended ones. A compile-time constant by design — a
/// configurable gate invites "raise it to make it go away".
pub const AGENT_CRON_FIRE_GATE: u32 = 3;

/// How late an injected fire must be before its envelope carries the
/// lateness mark (C.2a, stage-1 batch-2): `now - scheduled_at` above this
/// threshold stamps `lateByMs`. The value separates ordinary scheduler
/// jitter (a sub-second-to-seconds drift on every fire) from genuine
/// lateness (a busy window, a service restart, a closed session's
/// redelivery) — the model's judgment input ("is this task still
/// relevant?"). The threshold is implementation-tunable, not
/// user-facing.
pub const CRON_LATE_MARK_THRESHOLD: Duration = Duration::seconds(60);

/// The per-session cumulative cap on one-shot tasks created from
/// cron-lineage turns (SDR-106 A.6b, stage-1 batch-1 S9): enough for the
/// legitimate CI-revisit self-timing chain, far below an
/// around-the-clock unattended loop's order of magnitude. A
/// compile-time constant like the gate — the cap value is tunable at
/// implementation time, not user-facing.
pub const CRON_LINEAGE_ONE_SHOT_LIMIT: u32 = 12;

/// The task's ownership scope (stage-2 D-M1 / architecture §1): a task
/// belongs either to one session (the stage-1 shape — every historical
/// record) or to a workspace (stage-2 A.1: independent of any session's
/// lifetime, stored in the workspace-level cron store).
///
/// Serialized FLAT onto the owning record as sibling keys —
/// `{"scope":"session"}` / `{"scope":"workspace","workspaceId":"..."}` —
/// so `CronTask` mounts it behind `#[serde(flatten)]`. An absent `scope`
/// key reads back as [`TaskScope::Session`]: every record written before
/// stage 2 is a session task, and the workspace id rides the same flat
/// form the wire DTO carries (see [`cron_task_wire`]).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum TaskScope {
    /// The task belongs to one session (its `session_id`).
    #[default]
    Session,
    /// The task belongs to a workspace (the registry/bucket id).
    Workspace(String),
}

impl Serialize for TaskScope {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(None)?;
        match self {
            TaskScope::Session => {
                map.serialize_entry("scope", "session")?;
            }
            TaskScope::Workspace(workspace_id) => {
                map.serialize_entry("scope", "workspace")?;
                map.serialize_entry("workspaceId", workspace_id)?;
            }
        }
        map.end()
    }
}

impl<'de> Deserialize<'de> for TaskScope {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = TaskScope;
            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a task scope map")
            }
            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::MapAccess<'de>,
            {
                let mut tag: Option<String> = None;
                let mut workspace_id: Option<String> = None;
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "scope" => tag = Some(map.next_value()?),
                        "workspaceId" => workspace_id = Some(map.next_value()?),
                        // Unknown keys (the flattened siblings of the owning
                        // record) are ignored.
                        _ => {
                            map.next_value::<serde::de::IgnoredAny>()?;
                        }
                    }
                }
                match tag.as_deref() {
                    Some("workspace") => {
                        Ok(TaskScope::Workspace(workspace_id.ok_or_else(|| {
                            serde::de::Error::custom("a workspace scope requires a workspaceId")
                        })?))
                    }
                    // Absent tag (a pre-stage-2 record flattened onto an
                    // empty remainder) and an explicit "session" tag are
                    // the same thing.
                    _ => Ok(TaskScope::Session),
                }
            }
        }
        deserializer.deserialize_map(Visitor)
    }
}

/// The runtime permission posture of a workspace task (stage-2 D-M2 /
/// D.1-D.3): which stance the task's execution sessions run under.
/// `None` on the task carries the **ask-deny** semantics (the default
/// posture — approval asks are rejected with a reroute receipt), so old
/// records and the default creation flow need no key at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PermissionPosture {
    /// Ask-deny (the pre-batch-2 default): approval asks are rejected
    /// with a reroute receipt — the stage-1 unattended semantics. No
    /// creation face writes it any more; records (and the legacy read
    /// path) still carry it.
    #[serde(rename = "ask-deny")]
    AskDeny,
    /// Manual (A4's `manual`): asks are neither auto-approved nor
    /// rejected — they stay pending in the execution session's approval
    /// channel for the user (A13's pending list).
    #[serde(rename = "manual")]
    Manual,
    /// Yolo (A4's `yolo`): never ask, approve everything.
    #[serde(rename = "yolo")]
    Yolo,
    /// Auto: everything is allowed without asking (the existing auto
    /// permission mode's semantics).
    #[serde(rename = "auto")]
    Auto,
    /// Read-only observation: every write operation is rejected
    /// (read-only asks included).
    #[serde(rename = "read-only")]
    ReadOnly,
}

/// The task's declared permission mode (cron-improvement batch 2 / A4):
/// the SAME three modes a session carries, on the SAME wire spelling
/// (`permissionMode: "manual" | "auto" | "yolo"`). The task stores what
/// the user chose verbatim — what is selected is what is executed — and
/// [`Posture`](Self::posture) is the kernel's expansion into the runtime
/// [`PermissionPosture`] the spawner applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PermissionMode {
    /// Ask for anything not explicitly allowed: the runs are UNATTENDED
    /// (nobody answers at 09:00), so an ask is neither auto-approved nor
    /// rejected — it coalesces with the other pending approvals of the
    /// execution session (the A13 channel), waiting for the user.
    Manual,
    /// The ordinary session auto: everything that would ask is approved,
    /// and `AskUserQuestion` is denied (the existing mode semantics).
    Auto,
    /// Never ask, allow everything — including questions.
    Yolo,
}

impl PermissionMode {
    /// The wire name of the mode (the creation form's select value and
    /// the approval card's display — ADR-022's structured card).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::Auto => "auto",
            Self::Yolo => "yolo",
        }
    }

    /// The runtime posture this mode expands to (the spawner's input).
    ///
    /// - `manual` → [`PermissionPosture::Manual`] (asks stay pending in
    ///   the approval channel instead of being auto-answered or
    ///   rejected — A4's ruling, batch 2);
    /// - `auto` → [`PermissionPosture::Auto`] (the ordinary auto mode);
    /// - `yolo` → [`PermissionPosture::Yolo`] (never ask, approve all).
    pub fn posture(self) -> PermissionPosture {
        match self {
            PermissionMode::Manual => PermissionPosture::Manual,
            PermissionMode::Auto => PermissionPosture::Auto,
            PermissionMode::Yolo => PermissionPosture::Yolo,
        }
    }

    /// Map the engine's session mode onto the task mode (the same three
    /// names; the spawner is the translation point).
    pub fn from_permission_mode(mode: crate::permission::PermissionMode) -> Self {
        match mode {
            crate::permission::PermissionMode::Manual => PermissionMode::Manual,
            crate::permission::PermissionMode::Auto => PermissionMode::Auto,
            crate::permission::PermissionMode::Yolo => PermissionMode::Yolo,
        }
    }

    /// The mode a legacy posture record reads back as (a pre-batch-2
    /// record carries `permissionPosture` only): the best-effort inverse,
    /// used nowhere but the legacy read path and the spawner's fallback.
    pub fn from_posture_record(posture: PermissionPosture) -> Self {
        match posture {
            PermissionPosture::Auto => PermissionMode::Auto,
            PermissionPosture::Yolo => PermissionMode::Yolo,
            // `read-only` had no session name; the ask-deny default was
            // created from the `manual` option.
            PermissionPosture::Manual
            | PermissionPosture::AskDeny
            | PermissionPosture::ReadOnly => PermissionMode::Manual,
        }
    }
}

impl PermissionPosture {
    /// The mode this posture expands from, when it has one (batch 2 / A4):
    /// `None` for the two pre-A4 postures (`ask-deny`, `read-only`) that
    /// no mode name produces any more.
    pub fn mode(self) -> Option<PermissionMode> {
        match self {
            PermissionPosture::Manual => Some(PermissionMode::Manual),
            PermissionPosture::Auto => Some(PermissionMode::Auto),
            PermissionPosture::Yolo => Some(PermissionMode::Yolo),
            PermissionPosture::AskDeny | PermissionPosture::ReadOnly => None,
        }
    }
}

/// The concurrency posture of a workspace task (stage-2 D-M3 / B.4): what
/// happens when the next trigger fires while the previous run is still
/// in flight. Defaults to skip.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum TaskConcurrency {
    /// Skip (the default): the new trigger is skipped and recorded
    /// ("上次运行中"), never queued.
    #[default]
    #[serde(rename = "skip")]
    Skip,
    /// Queue: at most one backlogged run; further triggers merge into it.
    #[serde(rename = "queue")]
    Queue,
}

/// Who created a task (stage-1 batch-1, S1) — consumed by the approval
/// chain and the list face to tell user-created tasks apart from
/// model-created ones. The default is `Agent`: every historical record was
/// created by the model through a cron-creation tool, so a record without
/// the key reads back as `Agent` (`User` arrives with the batch-3 user
/// RPC face; the serde round-trip test guards the spelling against drift).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum TaskOrigin {
    #[serde(rename = "user")]
    User,
    #[default]
    #[serde(rename = "agent")]
    Agent,
    /// Agent-drafted, user-approved: the model requested creation through
    /// `CreateScheduledTask` and the task was created only after the user
    /// approved the gate — the honest origin labeling "模型（经审批）".
    #[serde(rename = "agent_approved")]
    AgentApproved,
}

/// A scheduled cron task.
///
/// One-shot tasks (`recurring == false`) are removed after their first fire;
/// recurring tasks are rescheduled in place with an updated
/// [`next_fire_at`](CronTask::next_fire_at).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CronTask {
    /// Unique task identifier (generated by [`CronManager::add`]).
    pub id: String,
    /// The 5-field cron expression controlling when the task fires.
    pub cron: String,
    /// The prompt injected into the session when the task fires.
    pub prompt: String,
    /// The session this task belongs to.
    pub session_id: String,
    /// Whether the task reschedules itself after firing (`true`) or is
    /// removed after its first fire (`false`).
    pub recurring: bool,
    /// Explicit timezone (IANA name, e.g. `Asia/Shanghai`) the expression
    /// is evaluated in. `None` = the host's local timezone at evaluation
    /// time (the TS semantics; records written before B5 carry no
    /// `timezone` key and are therefore read back as `None`). The name is
    /// stored verbatim — an explicit zone stays stable across restarts,
    /// while the local default follows the host.
    #[serde(default)]
    pub timezone: Option<String>,
    /// When the task was registered.
    pub created_at: DateTime<Utc>,
    /// When the task last fired (`None` until the first fire). Doubles as
    /// the coalesce cursor base (the TS `baseFromMs` — only a fire that
    /// advanced the cursor counts, hence the `> created_at` filter in
    /// consumers) and the TTL base (B.1 / D-M5, fire-refresh — see
    /// [`CronManager::is_stale`]); both semantics share this one field,
    /// advanced atomically under the task-table lock at delivery, so there
    /// is no second source of truth to drift.
    pub last_fired: Option<DateTime<Utc>>,
    /// The next scheduled fire time. Always `Some` while the task is
    /// registered: `add` rejects expressions that never fire again.
    pub next_fire_at: Option<DateTime<Utc>>,
    /// Who created the task (S1). Defaults to `Agent` — see
    /// [`TaskOrigin`]: old records deserialize as `Agent`.
    #[serde(default)]
    pub origin: TaskOrigin,
    /// The hard fire budget of a recurring task (A.5): `Some(n)` removes
    /// the task after its n-th delivered fire (maxed-out finale). `None`
    /// = unbounded. A one-shot task never carries a budget (its single
    /// fire is its own bound; the model-creation gate treats one-shots
    /// as budget 1).
    #[serde(default)]
    pub max_fires: Option<u32>,
    /// How many fires the task has delivered (D-M7): incremented at the
    /// cursor write, same lock — the max_fires verdict and the delivery
    /// are atomic. Coalesced deliveries count once; run-now never counts.
    #[serde(default)]
    pub fire_count: u32,
    /// Whether this task was created from inside a cron-fired turn (its
    /// turn origin is `Cron`) — the SDR-106 lineage marker riding the
    /// task itself (consumed by the lineage chain cap, S9). Defaults to
    /// `false`.
    #[serde(default)]
    pub cron_lineage: bool,
    /// The task's ownership scope (stage-2 D-M1, architecture §1):
    /// session (the stage-1 shape, the default) or workspace. Flattened
    /// onto the record as `scope` / `workspaceId` sibling keys; an absent
    /// key pair reads back as [`TaskScope::Session`] — every pre-stage-2
    /// record is a session task by construction.
    #[serde(default, flatten)]
    pub scope: TaskScope,
    /// The runtime permission posture of a workspace task (stage-2 D-M2,
    /// 4a data plane). Since batch 2 (A4) the product face writes
    /// [`permission_mode`](Self::permission_mode) and this field is its
    /// kernel expansion ([`PermissionMode::posture`]) — kept on the record
    /// so the spawner and old documents read one field. `None` = the
    /// pre-batch-2 ask-deny default. Stage-1 session tasks keep `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_posture: Option<PermissionPosture>,
    /// The task's declared permission mode (batch 2 / A4): the SAME three
    /// modes a session carries, stored verbatim. `None` = a pre-batch-2
    /// record (its behavior comes from
    /// [`permission_posture`](Self::permission_posture) alone).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<PermissionMode>,
    /// Whether the task is paused (batch 2 / A9): a paused task keeps its
    /// identity, its history, and its position in the list, but fires
    /// nothing and consumes no cursor. Absent keys read back as running
    /// (every pre-batch-2 record).
    #[serde(default)]
    pub paused: bool,
    /// The concurrency posture (stage-2 D-M3, 4a data plane): skip (the
    /// default) or queue. Data plane only in 4a — the executor that
    /// consults it lands in 4b.
    #[serde(default)]
    pub concurrency: TaskConcurrency,
    /// The task's display title (ADR-019 / form §3.1 区块一): `None` = no
    /// title was given — the list face answers `null` and the
    /// presentation layer's i18n owns the untitled label (the prompt
    /// stays the actual payload).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// The model alias the task's runs bind (ADR-019 / VU10: same slot as
    /// `session.create.model_id`). `None` = the server's default model —
    /// the spawner's existing resolution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The thinking effort the task's runs carry (ADR-019 / VU11): fills
    /// the `AgentScopeConfig.thinking_effort` slot the spawner already
    /// consults. `None` = the model catalog's default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<crate::profile::ThinkingEffort>,
    /// The working directory the task's execution session runs in
    /// (ADR-019 / VU10): a directory under the workspace; `None` = the
    /// workspace's default (the spawner's existing behavior).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workdir: Option<String>,
    /// The end date (ADR-019 / B1, SDR-110 — inclusive): the task's last
    /// fire may fall on this LOCAL day (evaluated in the task's timezone);
    /// the scheduler removes the task once the day passes. `None` = no
    /// end date.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ends_at: Option<chrono::NaiveDate>,
    /// The elapsed-interval schedule (ADR-016 revision / SDR-113): the
    /// alternative to the 5-field cron expression — "every N
    /// days/weeks/months at HH:MM from an anchor date". Mutually exclusive
    /// with a meaningful `cron` (the create face rejects both being set);
    /// a task with `interval: Some` evaluates through the schedule
    /// kernel's interval branch, never the expression parser.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interval: Option<IntervalSpec>,
}

/// An elapsed-interval schedule (ADR-016 revision / SDR-113): "every N
/// days/weeks/months at `time`, counting from `anchor_date`". The 5-field
/// cron expression cannot express elapsed intervals (dom/dow are field
/// enumerations — `*/N` in dom skips months without an N-th day), so the
/// schedule carries this structured form instead of a pseudo-expression.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntervalSpec {
    /// How many `unit`s pass between fires (>= 1).
    pub every: u32,
    /// The unit `every` counts.
    pub unit: IntervalUnit,
    /// The local wall-clock time of day each fire lands at (`HH:MM`).
    pub time: String,
    /// The local date the counting starts from (inclusive; the first fire
    /// is the first `time` strictly after the anchor moment).
    #[serde(rename = "anchorDate", alias = "anchor_date")]
    pub anchor_date: chrono::NaiveDate,
}

/// The unit an [`IntervalSpec`]'s `every` counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IntervalUnit {
    Day,
    Week,
    Month,
}

/// One schedule's evaluation shape (ADR-017): a task either carries a
/// 5-field cron expression or an elapsed interval — never both. The
/// preview/`check_due` kernel and the `cron.preview` RPC both consume
/// this single discriminated form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScheduleSpec<'a> {
    /// The 5-field cron expression, evaluated on the zone's local
    /// calendar.
    Cron(&'a str),
    /// The elapsed interval.
    Interval(&'a IntervalSpec),
}

impl IntervalSpec {
    /// Validate the spec's own shape (a positive `every`, a parseable
    /// `HH:MM` time): the create face rejects a malformed spec before
    /// anything registers.
    pub fn validate(&self) -> Result<(), CronError> {
        if self.every == 0 {
            return Err(CronError::InvalidExpression(
                "interval every must be a positive integer".to_string(),
            ));
        }
        if parse_hhmm(&self.time).is_none() {
            return Err(CronError::InvalidExpression(format!(
                "interval time must be HH:MM, got '{}'",
                self.time
            )));
        }
        Ok(())
    }
}

/// Parse an `HH:MM` wall-clock time (0 <= HH <= 23, 0 <= MM <= 59).
fn parse_hhmm(time: &str) -> Option<(u32, u32)> {
    let (hour, minute) = time.split_once(':')?;
    let hour: u32 = hour.parse().ok()?;
    let minute: u32 = minute.parse().ok()?;
    (hour <= 23 && minute <= 59).then_some((hour, minute))
}

/// A minimal, fully-defaulted [`CronTask`] — the literal base (form
/// batch 6 T1): every optional field at its documented default, callers
/// override only what they mean to pin. Serves the test literals across
/// crates (the struct grew past hand-copying every field per site).
pub fn cron_task_base(id: &str, cron: &str, prompt: &str) -> CronTask {
    CronTask {
        id: id.to_string(),
        cron: cron.to_string(),
        prompt: prompt.to_string(),
        session_id: String::new(),
        recurring: true,
        timezone: None,
        created_at: chrono::Utc::now(),
        last_fired: None,
        next_fire_at: None,
        origin: TaskOrigin::Agent,
        max_fires: None,
        fire_count: 0,
        cron_lineage: false,
        scope: TaskScope::Session,
        permission_posture: None,
        permission_mode: None,
        paused: false,
        concurrency: TaskConcurrency::Skip,
        title: None,
        model: None,
        effort: None,
        workdir: None,
        ends_at: None,
        interval: None,
    }
}

/// Serialize a [`CronTask`] into the cross-language wire DTO (F16).
///
/// The shape is the TS kernel's `CronTask` declaration
/// (`agent-core-v2/src/app/cron/cronTask.ts`, mirrored in
/// `wire-manifest.d.ts`): camelCase keys, epoch-millisecond numbers, the
/// session id folded into `tags.sessionId` (`CRON_SESSION_TAG`), and no
/// absolute `nextFireAt` — both engines recompute the next fire from the
/// expression on restore, so the persisted absolute would be stale anyway.
///
/// The domain struct's own serde form (snake_case, RFC 3339) never crosses
/// the wire; [`CronManager::add`] records this DTO in the `cron.add` op.
pub fn cron_task_wire(task: &CronTask) -> serde_json::Value {
    let mut wire = serde_json::Map::new();
    wire.insert("id".to_string(), serde_json::json!(task.id));
    wire.insert("cron".to_string(), serde_json::json!(task.cron));
    wire.insert("prompt".to_string(), serde_json::json!(task.prompt));
    wire.insert("recurring".to_string(), serde_json::json!(task.recurring));
    wire.insert(
        "createdAt".to_string(),
        serde_json::json!(task.created_at.timestamp_millis()),
    );
    if let Some(last_fired) = task.last_fired {
        wire.insert(
            "lastFiredAt".to_string(),
            serde_json::json!(last_fired.timestamp_millis()),
        );
    }
    wire.insert(
        "tags".to_string(),
        serde_json::json!({ "sessionId": task.session_id }),
    );
    if let Some(timezone) = &task.timezone {
        wire.insert("timezone".to_string(), serde_json::json!(timezone));
    }
    // Stage-1 batch-1 (S1): the new scheduling fields ride the same DTO —
    // every key stays optional so a record written before this batch (or
    // by a consumer that does not know the fields) round-trips with the
    // documented defaults. `maxFires` is omitted when unbounded (the
    // skip-when-none tolerance `timezone` already established).
    wire.insert("origin".to_string(), serde_json::json!(task.origin));
    if let Some(max_fires) = task.max_fires {
        wire.insert("maxFires".to_string(), serde_json::json!(max_fires));
    }
    wire.insert("fireCount".to_string(), serde_json::json!(task.fire_count));
    wire.insert(
        "cronLineage".to_string(),
        serde_json::json!(task.cron_lineage),
    );
    // Stage-2 D-M1 (4a, T1): the scope rides the same DTO as flat sibling
    // keys — omitted entirely for session tasks (the pre-stage-2 records
    // read back as session scope), `scope` + `workspaceId` for workspace
    // tasks.
    if let TaskScope::Workspace(workspace_id) = &task.scope {
        wire.insert("scope".to_string(), serde_json::json!("workspace"));
        wire.insert("workspaceId".to_string(), serde_json::json!(workspace_id));
    }
    if let Some(posture) = task.permission_posture {
        wire.insert("permissionPosture".to_string(), serde_json::json!(posture));
    }
    if let Some(mode) = task.permission_mode {
        wire.insert("permissionMode".to_string(), serde_json::json!(mode));
    }
    if task.paused {
        wire.insert("paused".to_string(), serde_json::json!(true));
    }
    if task.concurrency != TaskConcurrency::Skip {
        wire.insert(
            "concurrency".to_string(),
            serde_json::json!(task.concurrency),
        );
    }
    // ADR-019 透传链: the form's six optional fields — camelCase, omitted
    // when unset (the skip-when-none tolerance the DTO already runs).
    if let Some(title) = &task.title {
        wire.insert("title".to_string(), serde_json::json!(title));
    }
    if let Some(model) = &task.model {
        wire.insert("model".to_string(), serde_json::json!(model));
    }
    if let Some(effort) = &task.effort {
        wire.insert(
            "effort".to_string(),
            serde_json::to_value(effort).unwrap_or_default(),
        );
    }
    if let Some(workdir) = &task.workdir {
        wire.insert("workdir".to_string(), serde_json::json!(workdir));
    }
    if let Some(ends_at) = task.ends_at {
        wire.insert("endsAt".to_string(), serde_json::json!(ends_at.to_string()));
    }
    if let Some(interval) = &task.interval {
        wire.insert(
            "interval".to_string(),
            serde_json::to_value(interval).unwrap_or_default(),
        );
    }
    serde_json::Value::Object(wire)
}

/// Parse a `cron.add` task record back into a [`CronTask`] — the restore
/// side of [`cron_task_wire`] (F16).
///
/// Shape-tolerant on purpose: the record may be
///
/// 1. the wire DTO (camelCase, epoch millis, `tags.sessionId`) — logs
///    written by the rust kernel since T6 and by the TS kernel, and
///    pre-T6 rust logs after the 1.5→1.6 migration;
/// 2. the legacy rust shape (snake_case, RFC 3339) — a pre-T6 log the
///    migration never touched (the on-disk rewrite is best-effort).
///
/// Returns `None` when the record does not carry a parseable task (no id,
/// or unparseable timestamps) — the caller skips it rather than failing
/// the whole resume, mirroring the per-record tolerance of the restore.
pub fn cron_task_from_wire(value: &serde_json::Value) -> Option<CronTask> {
    /// Read a timestamp under either spelling: epoch millis (the wire DTO)
    /// or an RFC 3339 string (the legacy rust shape).
    fn timestamp(
        record: &serde_json::Value,
        dto_key: &str,
        legacy_key: &str,
    ) -> Option<DateTime<Utc>> {
        match record.get(dto_key).or_else(|| record.get(legacy_key)) {
            Some(serde_json::Value::Number(millis)) => {
                DateTime::from_timestamp_millis(millis.as_i64()?)
            }
            Some(serde_json::Value::String(rfc3339)) => DateTime::parse_from_rfc3339(rfc3339)
                .map(|at| at.with_timezone(&Utc))
                .ok(),
            _ => None,
        }
    }

    let str_field = |key: &str| {
        value
            .get(key)
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
    };
    Some(CronTask {
        id: str_field("id")?,
        cron: str_field("cron").unwrap_or_default(),
        prompt: str_field("prompt").unwrap_or_default(),
        // `session_id` is a plain field in the rust shapes and folded into
        // `tags.sessionId` in the DTO (CRON_SESSION_TAG).
        session_id: str_field("session_id")
            .or_else(|| {
                value
                    .get("tags")
                    .and_then(|tags| tags.get("sessionId"))
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string)
            })
            .unwrap_or_default(),
        // The TS declaration marks `recurring` optional; an absent flag is
        // a one-shot (fires once, never reschedules).
        recurring: value
            .get("recurring")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
        // The optional IANA zone the expression is evaluated in; records
        // without the key (pre-B5 rust logs, TS logs) read back as `None`
        // and are evaluated on the host's local calendar.
        timezone: str_field("timezone"),
        created_at: timestamp(value, "createdAt", "created_at")?,
        last_fired: timestamp(value, "lastFiredAt", "last_fired"),
        // The next fire is always recomputed from the expression — the
        // persisted absolute is stale whenever the process was down.
        next_fire_at: None,
        // Stage-1 batch-1 (S1): shape-tolerant reads of the new scheduling
        // fields — absent keys take the documented defaults (origin agent,
        // unbounded, zero fires, not lineage-born) so pre-batch records
        // keep their exact semantics. The retired `unattended` key is
        // ignored on read (batch 4 / A4).
        origin: value
            .get("origin")
            .and_then(serde_json::Value::as_str)
            // R6 (stage-2 4b): the origin wire field is three-valued —
            // "user" | "agent" | "agent_approved" all parse explicitly; a
            // missing or UNKNOWN spelling takes the documented default
            // (agent) with no silent degradation of the two known new
            // spellings (the shape-tolerant restore's contract).
            .and_then(|raw| serde_json::from_value(serde_json::json!(raw)).ok())
            .unwrap_or_default(),
        max_fires: value
            .get("maxFires")
            .or_else(|| value.get("max_fires"))
            .and_then(serde_json::Value::as_u64)
            .map(|budget| budget as u32),
        fire_count: value
            .get("fireCount")
            .or_else(|| value.get("fire_count"))
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0) as u32,
        cron_lineage: value
            .get("cronLineage")
            .or_else(|| value.get("cron_lineage"))
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
        // Stage-2 D-M1 (4a, T1): the flat scope keys — absent keys read
        // back as the session scope (every pre-stage-2 record), a
        // `scope: "workspace"` record requires its `workspaceId`.
        scope: match value.get("scope").and_then(serde_json::Value::as_str) {
            Some("workspace") => TaskScope::Workspace(
                str_field("workspaceId")
                    .or_else(|| str_field("workspace_id"))
                    .unwrap_or_default(),
            ),
            _ => TaskScope::Session,
        },
        permission_posture: value
            .get("permissionPosture")
            .or_else(|| value.get("permission_posture"))
            .cloned()
            .and_then(|raw| serde_json::from_value(raw).ok()),
        permission_mode: {
            let declared = value
                .get("permissionMode")
                .or_else(|| value.get("permission_mode"))
                .cloned()
                .and_then(|raw| serde_json::from_value(raw).ok());
            // A pre-batch-2 record carries the posture only — read the
            // mode back from it (the best-effort inverse), so the edit
            // face always has a value to show.
            declared.or_else(|| {
                value
                    .get("permissionPosture")
                    .or_else(|| value.get("permission_posture"))
                    .cloned()
                    .and_then(|raw| serde_json::from_value(raw).ok())
                    .map(PermissionMode::from_posture_record)
            })
        },
        paused: value
            .get("paused")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
        concurrency: value
            .get("concurrency")
            .cloned()
            .and_then(|raw| serde_json::from_value(raw).ok())
            .unwrap_or_default(),
        // ADR-019 透传链: shape-tolerant reads of the form's six optional
        // fields — absent keys read back as `None` (pre-form records keep
        // their exact semantics).
        title: str_field("title"),
        model: str_field("model"),
        effort: value
            .get("effort")
            .cloned()
            .and_then(|raw| serde_json::from_value(raw).ok()),
        workdir: str_field("workdir").or_else(|| str_field("work_dir")),
        ends_at: value
            .get("endsAt")
            .or_else(|| value.get("ends_at"))
            .and_then(serde_json::Value::as_str)
            .and_then(|raw| chrono::NaiveDate::parse_from_str(raw, "%Y-%m-%d").ok()),
        interval: value
            .get("interval")
            .cloned()
            .and_then(|raw| serde_json::from_value(raw).ok()),
    })
}

/// One delivered fire of a due cron task (B5): everything the injection
/// path and the `cron.fired` event need — the TS kernel's `deliverDue`
/// input face. Serialize/Deserialize rides the held-fire persistence
/// (C.4: the `cron.held` op carries this shape through the wire log).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CronFire {
    /// The firing task's id.
    pub task_id: String,
    /// The prompt to inject into the session.
    pub prompt: String,
    /// The firing task's cron expression (carried for origin enrichment).
    pub cron: String,
    /// Whether the firing task reschedules itself.
    pub recurring: bool,
    /// How many overdue periods this single fire merged (`>= 1`).
    pub coalesced_count: usize,
    /// The task crossed the 7-day stale threshold — this is its final
    /// delivery, after which the task is removed.
    pub stale: bool,
    /// This delivery exhausts the task's `max_fires` budget (A.5 / F.1) —
    /// the task's final delivery, after which it is removed (stage-1
    /// batch-1, S3).
    pub maxed_out: bool,
    /// The task's ReadOnly posture (stage-2 D-M2 / ADR-015): `true` means
    /// the fired turn's execution refuses every tool that does not
    /// declare the read-only capability category. Session-level fires
    /// carry the task's posture-derived value (a session task's posture
    /// is `None`, so `false` unless explicitly set).
    #[serde(default)]
    pub readonly: bool,
    /// The moment this fire was *scheduled* for (the task's pre-fire
    /// `next_fire_at` — the last due occurrence when coalesced, C.2a's
    /// lateness reference). `None` on run-now fires (`trigger()` — a
    /// manual run has no lateness semantics: it is not late, it is early
    /// by design).
    pub scheduled_at: Option<DateTime<Utc>>,
    /// How late the injection is against `scheduled_at` (C.2a): set by
    /// the injection path when `now - scheduled_at` exceeds
    /// [`CRON_LATE_MARK_THRESHOLD`] (a busy hold, a restart, a closed
    /// session's redelivery — the model's "is this still relevant?"
    /// judgment input). `None` when the fire is on time or run-now (the
    /// default — the mark is only ever stamped at the injection site,
    /// never at fire time, so a held fire's wait itself computes the
    /// lateness fresh when it finally lands).
    pub late_by_ms: Option<u64>,
    /// The firing task's ownership scope (stage-2 D-M1, 4a T1): carried in
    /// the same flat form the task record uses, so the executor side can
    /// route the fire (session injection vs workspace run) without
    /// re-reading the task.
    #[serde(flatten)]
    pub scope: TaskScope,
    /// The task's end date has arrived or passed — this is its final
    /// delivery (B1 / SDR-110, the `max_fires` finale's sibling): the
    ///「到期」closing mark the run faces carry. Defaults to `false`
    /// (pre-B6 held-fire records).
    #[serde(default)]
    pub ended: bool,
}

impl CronFire {
    /// Whether this fire belongs to a WORKSPACE task (the
    /// execution-session shape — nobody watches that session's chat).
    pub fn is_workspace(&self) -> bool {
        matches!(self.scope, TaskScope::Workspace(_))
    }
}

/// The computed outcome of one due task, gathered under the task-table
/// lock before any wire op is recorded (the plan/apply split inside
/// [`CronManager::check_due`]).
use workspace_store::FirePlan;

/// Errors produced by cron expression parsing and scheduling.
#[derive(Debug, Error)]
pub enum CronError {
    /// The expression is malformed: wrong field count, out-of-range values,
    /// non-numeric tokens, or a step of zero.
    #[error("invalid cron expression: {0}")]
    InvalidExpression(String),
    /// The expression is valid but has no fire time within the search
    /// horizon (e.g. `0 0 30 2 *` — Feb 30 never exists).
    #[error("cron expression '{0}' has no next fire time within {SEARCH_HORIZON_DAYS} days")]
    NoNextFire(String),
}

/// Runtime toggles of the cron manager, mirroring the TS kernel's env
/// switch family (the two *semantic* switches — driver cadence and the
/// disable switch belong to the B6 management surface, not here).
///
/// Both flags default to enabled; `MIRRI_CRON_NO_JITTER=1` and
/// `MIRRI_CRON_NO_STALE=1` turn the respective feature off process-wide
/// (bench reproducibility / operational escape hatches). Tests inject the
/// flags through [`CronManager::with_options`] instead of mutating the
/// process environment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CronOptions {
    /// Whether per-task deterministic jitter is applied to fire times
    /// (the anti-thundering-herd shift; TS `MIRRI_CRON_NO_JITTER`).
    pub jitter: bool,
    /// Whether recurring tasks stale out after
    /// [`STALE_THRESHOLD`](crate::cron::STALE_THRESHOLD) and are removed
    /// after their final delivery (TS `MIRRI_CRON_NO_STALE`).
    pub stale: bool,
}

/// Read one env flag: exactly `"1"` means the feature is switched off.
/// Kept as a pure function so the parsing rule is testable without
/// touching process-global state.
fn env_flag(raw: Option<String>) -> bool {
    matches!(raw.as_deref(), Some("1"))
}

/// Whether the cron subsystem is disabled via `MIRRICODE_DISABLE_CRON`
/// (ADR-006 / requirements E.4): exactly `"1"` disables — anything else
/// (unset, `"0"`, `"true"`, ...) leaves scheduling enabled. Read once at
/// construction time by the surfaces that honor it (the tools registration
/// and the cron driver): the switch takes effect at server start and
/// clears at restart, not hot-reloaded.
pub fn scheduling_disabled_from_env() -> bool {
    env_flag(std::env::var("MIRRICODE_DISABLE_CRON").ok())
}

/// Creation-time knobs of [`CronManager::add`] beyond the task's core
/// (expression / prompt / session / recurring) — stage-1 batch-1 (S1)
/// collapses the growing positional tail into one struct so each new knob
/// lands without touching every call site.
#[derive(Debug, Clone, Default)]
pub struct CronCreateOptions {
    /// Explicit timezone (IANA name) the expression is evaluated in;
    /// `None` = the host's local zone.
    pub timezone: Option<String>,
    /// The hard fire budget of a recurring task (A.5). `None` = unbounded.
    /// Rejected on one-shot tasks and when zero.
    pub max_fires: Option<u32>,
    /// Who is creating the task (S1) — the tool face passes `Agent`, the
    /// batch-3 user RPC will pass `User`. Defaults to `Agent`.
    pub origin: TaskOrigin,
    /// The SDR-106 lineage marker: set when the creating turn's origin is
    /// `Cron` (consumed by the lineage chain cap, S9).
    pub cron_lineage: bool,
    /// The task's ownership scope (stage-2 D-M1, 4a T4): the service layer
    /// injects it per entry — the model tool face never passes it
    /// (ADR-014: the tool schema has no scope parameter), the user RPC
    /// face passes the workspace variant for workspace creation.
    /// Defaults to session.
    pub scope: TaskScope,
    /// The runtime permission posture of a workspace task (stage-2 D-M2,
    /// 4a data plane): `None` = the ask-deny default.
    pub permission_posture: Option<PermissionPosture>,
    /// The task's declared permission mode (batch 2 / A4). When set, it
    /// also fixes `permission_posture` to its expansion unless the caller
    /// set one explicitly.
    pub permission_mode: Option<PermissionMode>,
    /// The concurrency posture (stage-2 D-M3, 4a data plane): skip by
    /// default.
    pub concurrency: TaskConcurrency,
    /// The task's display title (ADR-019 透传链).
    pub title: Option<String>,
    /// The model alias the task's runs bind (ADR-019 / VU10).
    pub model: Option<String>,
    /// The thinking effort the task's runs carry (ADR-019 / VU11).
    pub effort: Option<crate::profile::ThinkingEffort>,
    /// The working directory the task's execution session runs in
    /// (ADR-019 / VU10).
    pub workdir: Option<String>,
    /// The inclusive end date (ADR-019 / B1, SDR-110).
    pub ends_at: Option<chrono::NaiveDate>,
    /// The elapsed-interval schedule (ADR-016 revision / SDR-113) —
    /// mutually exclusive with a cron expression.
    pub interval: Option<IntervalSpec>,
}

impl Default for CronOptions {
    fn default() -> Self {
        Self {
            jitter: !env_flag(std::env::var("MIRRI_CRON_NO_JITTER").ok()),
            stale: !env_flag(std::env::var("MIRRI_CRON_NO_STALE").ok()),
        }
    }
}

/// Session-level cron manager.
///
/// Holds the task table and a shutdown flag: after
/// [`shutdown`](CronManager::shutdown), [`check_due`](CronManager::check_due)
/// stops firing tasks. The manager is cheaply shareable behind an `Arc` and
/// is intended to live as a per-session service.
///
/// ## Wire-log persistence
///
/// Every mutation is persisted to the session's `wire.jsonl`
/// (write-before-mutate: the op is recorded *before* the in-memory change,
/// so a crash between the two leaves the wire log ahead of memory — resume
/// treats the wire log as the source of truth):
///
/// - [`add`](CronManager::add) records a `cron.add` op carrying the full
///   task (the [`CronTask`] serde shape);
/// - [`remove`](CronManager::remove) records a `cron.delete` op for the ids
///   it actually removes;
/// - [`check_due`](CronManager::check_due) records a `cron.cursor` op per
///   fired task before applying the in-memory fire.
///
/// A manager whose recorder slot is empty (the default, and any session
/// still replaying) records nothing. [`restore_tasks`](CronManager::restore_tasks)
/// never records — replayed ops must not be re-appended to the log they
/// came from.
#[derive(Debug)]
pub struct CronManager {
    tasks: Mutex<HashMap<String, CronTask>>,
    shutdown: AtomicBool,
    /// The semantic toggles (jitter / stale) this manager runs with.
    options: CronOptions,
    /// The session's wire-log recorder; shares the session's writer slot
    /// (an empty slot records nothing).
    wire_recorder: SessionWireLog,
    /// The session's event bus — every fired task publishes a
    /// `cron.fired` event on it (`None` until the session manager wires
    /// one in; a manager without a bus fires silently, e.g. in unit
    /// tests).
    event_bus: std::sync::Mutex<Option<std::sync::Arc<crate::session::EventBus>>>,
    /// Ids whose `remove()` was requested, whether or not a task was in
    /// the table at the time. A one-shot is removed from the table by
    /// `check_due` itself when it fires, so a later `remove` answers
    /// `false` — the table alone cannot distinguish "already fired
    /// normally" from "deleted while its fire was held". The driver
    /// consults [`CronManager::is_delete_requested`] to purge held fires.
    /// The set grows with the number of distinct deletions — bounded by
    /// the session's lifetime, never pruned (a known, accepted boundary).
    delete_requested: Mutex<HashSet<String>>,
    /// Held fires (C.4, stage-1 batch-2): fired prompts waiting for an
    /// idle target agent — task-side state, so a session that closes with
    /// fires still held redelivers them on resume instead of dropping the
    /// injection. At-most one entry per task id (same-task holds merge —
    /// see [`CronManager::hold_fire`]).
    held: Mutex<Vec<CronFire>>,
    /// Serializes [`CronManager::hold_fire`] across its two async halves
    /// (snapshot+persist, then the in-memory upsert): the driver's tick
    /// and a run-now trigger can hold concurrently, and an interleave
    /// across the `record_op` await point would let one fire's op land
    /// AFTER a later fire's memory state (a crash there loses the
    /// earlier hold). Async, not std — held across an await.
    hold_fire_gate: tokio::sync::Mutex<()>,
    /// The session's cumulative count of one-shot tasks created from
    /// cron-lineage turns (S9 / SDR-106 A.6b) — monotonic, never reset by
    /// task deletion (the chain cap counts creations, not live tasks).
    /// Restored from the reducer state on resume; advanced atomically
    /// with the creation's wire ops (see `add_lineage_one_shot`).
    lineage_one_shot_created: Mutex<u32>,
}

impl Default for CronManager {
    fn default() -> Self {
        Self::new()
    }
}

impl CronManager {
    /// Create an empty cron manager with no wire-log recorder attached
    /// and the environment-default semantic toggles.
    pub fn new() -> Self {
        Self::with_options(CronOptions::default())
    }

    /// Create an empty cron manager with explicit semantic toggles (the
    /// injection point tests use in place of mutating the process env).
    pub fn with_options(options: CronOptions) -> Self {
        Self {
            tasks: Mutex::new(HashMap::new()),
            shutdown: AtomicBool::new(false),
            options,
            wire_recorder: SessionWireLog::default(),
            event_bus: std::sync::Mutex::new(None),
            delete_requested: Mutex::new(HashSet::new()),
            held: Mutex::new(Vec::new()),
            hold_fire_gate: tokio::sync::Mutex::new(()),
            lineage_one_shot_created: Mutex::new(0),
        }
    }

    /// Attach the session's wire-log recorder so mutations are persisted.
    pub fn with_wire_recorder(mut self, recorder: SessionWireLog) -> Self {
        self.wire_recorder = recorder;
        self
    }

    /// Attach the session's event bus so every fired task publishes a
    /// `cron.fired` event on it (the session manager wires this in when it
    /// installs the session's artifacts).
    pub fn set_event_bus(&self, bus: std::sync::Arc<crate::session::EventBus>) {
        *self
            .event_bus
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(bus);
    }

    /// Record a wire op to the session's wire log.
    ///
    /// No-op when no writer is installed (the default recorder slot).
    async fn record_op(&self, op: WireOp) {
        self.wire_recorder.record_op(op).await;
    }

    /// Add a cron task.
    ///
    /// The expression is parsed and its next fire time computed immediately
    /// on the given timezone's calendar (`None` = the host's local zone);
    /// syntactically invalid expressions, unrecognised timezone names, and
    /// expressions that will never fire again are rejected with
    /// [`CronError`] before anything is registered. The recorded fire time
    /// carries the per-task deterministic jitter when the manager's
    /// [`CronOptions::jitter`] is on. A `cron.add` op is recorded before
    /// the in-memory insert. Returns the generated task id.
    pub async fn add(
        &self,
        cron: String,
        prompt: String,
        session_id: String,
        recurring: bool,
        options: CronCreateOptions,
    ) -> Result<String, CronError> {
        // Budget validation (S1): a one-shot task is its own budget (one
        // fire — declaring max_fires on it is a caller bug), and a budget
        // of zero would remove the task before it ever fired.
        if !recurring && options.max_fires.is_some() {
            return Err(CronError::InvalidExpression(
                "max_fires cannot be set on a one-shot task (a one-shot fires exactly once)"
                    .to_string(),
            ));
        }
        if options.max_fires == Some(0) {
            return Err(CronError::InvalidExpression(
                "max_fires must be a positive integer".to_string(),
            ));
        }
        // The ends_at authoritative validation (B1 / M3, SDR-110): the
        // end date may not fall before the schedule's FIRST fire day —
        // a task whose whole schedule lies past its end date would
        // never fire. Equal-to-first-fire-day is legal (the form's
        // one-shot spelling: ends.on == t0's local day).
        if let Some(ends_at) = options.ends_at {
            let tz = CronTimezone::parse(options.timezone.as_deref())?.resolve();
            let schedule = match &options.interval {
                Some(interval) => ScheduleSpec::Interval(interval),
                None => ScheduleSpec::Cron(&cron),
            };
            let first = Self::compute_schedule_next_fire(&schedule, Utc::now(), tz)?;
            let first_day = first.with_timezone(&tz).date_naive();
            if ends_at < first_day {
                return Err(CronError::InvalidExpression(
                    "结束日期不能早于首次运行日".to_string(),
                ));
            }
        }
        let task = Self::prepare_task(cron, prompt, session_id, recurring, options, self.options)?;
        let id = task.id.clone();
        // Write-before-mutate: the cron.add op lands before the in-memory
        // insert, so a crash between the two leaves the wire log ahead of
        // memory (resume treats the wire log as the source of truth). The
        // task is serialized in the cross-language wire DTO (F16) —
        // camelCase keys, epoch-millis timestamps — the shape the TS
        // kernel's `CronTask` declares, never the domain struct's serde
        // form.
        self.record_op(WireOp::CronAdd {
            task: cron_task_wire(&task),
        })
        .await;
        self.insert_task(task);
        Ok(id)
    }

    /// Create a one-shot task from a cron-lineage turn (S9 / SDR-106
    /// A.6a+A.6b) — the lineage-exempt path with its own atomicity:
    ///
    /// 1. under the count lock, the current count is checked against
    ///    [`CRON_LINEAGE_ONE_SHOT_LIMIT`] and advanced;
    /// 2. the `cron.lineage_created` op carrying the NEW absolute count is
    ///    recorded (write-before-mutate) BEFORE the `cron.add` op — a
    ///    crash between the two leaves the log having counted a creation
    ///    that never landed, which OVER-counts (fail-safe: the cap can
    ///    fire early, never late);
    /// 3. the task is added with `cron_lineage: true` (the marker the
    ///    chain cap's semantics ride on).
    ///
    /// Errors (over the cap, budget on one-shot, bad expression) register
    /// nothing and leave the count untouched.
    pub async fn add_lineage_one_shot(
        &self,
        cron: String,
        prompt: String,
        session_id: String,
        timezone: Option<String>,
    ) -> Result<String, CronError> {
        // m1 (review): validate the expression and timezone BEFORE
        // reserving the slot — a lineage turn with a bad expression must
        // not burn quota (the count only ever advances for creations that
        // could actually land; `prepare_task` re-validates identically).
        let tz = CronTimezone::parse(timezone.as_deref())?.resolve();
        Self::compute_next_fire(&cron, Utc::now(), tz)?;
        // Reserve the slot under the count lock first — the check and the
        // advance are one critical section, so concurrent lineage
        // creations cannot race past the cap.
        let new_count = {
            let mut count = self
                .lineage_one_shot_created
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if *count >= CRON_LINEAGE_ONE_SHOT_LIMIT {
                return Err(CronError::InvalidExpression(format!(
                    "本会话内定时血统创建的一次性任务累计已达上限（{CRON_LINEAGE_ONE_SHOT_LIMIT}），                     已拒绝新的创建。改道路径：请用户接管（由用户在管理面直接创建该任务）。"
                )));
            }
            *count += 1;
            *count
        };
        // The count op lands BEFORE the add op (both before the in-memory
        // insert): the crash window can only over-count.
        self.record_op(WireOp::CronLineageCreated { count: new_count })
            .await;
        // The count op lands BEFORE the add op (both before the
        // in-memory insert): the crash window can only over-count.
        // A failed add after the count op keeps the op — the over-count
        // is the accepted fail-safe direction.
        self.add(
            cron,
            prompt,
            session_id,
            false,
            CronCreateOptions {
                timezone,
                origin: TaskOrigin::Agent,
                cron_lineage: true,
                ..CronCreateOptions::default()
            },
        )
        .await
    }

    /// The session's lineage one-shot creation count (S9) — monotonic;
    /// deleting tasks never lowers it.
    pub fn lineage_one_shot_created(&self) -> u32 {
        *self
            .lineage_one_shot_created
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Restore the lineage count from the reducer state (S9) — the
    /// absolute-value op replays through a max, so the restored count is
    /// at least what any recorded op saw.
    pub fn restore_lineage_count(&self, count: u32) {
        let mut current = self
            .lineage_one_shot_created
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *current = (*current).max(count);
    }

    /// Insert an already-built task into the task table.
    fn insert_task(&self, task: CronTask) {
        tracing::info!(
            task_id = %task.id,
            cron = %task.cron,
            next_fire_at = ?task.next_fire_at,
            recurring = task.recurring,
            "cron task registered"
        );
        self.tasks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(task.id.clone(), task);
    }

    /// Remove a cron task by id.
    ///
    /// A `cron.delete` op is recorded before the in-memory removal when (and
    /// only when) a task with the id exists. Returns `true` when a task was
    /// removed, `false` when no task with the id existed.
    pub async fn remove(&self, id: &str) -> bool {
        // Record the delete request unconditionally — including the
        // one-shot case where `check_due` already removed the task from
        // the table (so the existence check below answers false): the
        // driver purges held fires through `is_delete_requested`, which
        // must see this id.
        self.delete_requested
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(id.to_string());
        if !self
            .tasks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains_key(id)
        {
            return false;
        }
        // Write-before-mutate: the cron.delete op lands before the in-memory
        // removal. The existence check above is advisory (single event loop);
        // the remove below re-checks under its own lock.
        self.record_op(WireOp::CronDelete {
            ids: vec![id.to_string()],
        })
        .await;
        let removed = self
            .tasks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(id)
            .is_some();
        if removed {
            tracing::info!(task_id = id, "cron task removed");
        }
        removed
    }

    /// Whether a delete was requested for the id — non-consuming: the
    /// driver asks on every tick, so the flag must survive until the
    /// session ends. Covers ids that never existed or already left the
    /// table (e.g. a one-shot consumed by `check_due`): the caller
    /// cannot distinguish those from "deleted" via the task table alone.
    pub fn is_delete_requested(&self, id: &str) -> bool {
        self.delete_requested
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains(id)
    }

    /// Hold one fired prompt on the task side (C.4, write-before-inject):
    /// the cursor already advanced and `cron.fired` already published,
    /// but the target agent was busy — the fire waits here until an idle
    /// tick (or a resume after a close) injects it. A hold for a task
    /// that already has one merges (coalescedCount adds, the freshest
    /// stale/maxed_out state wins). Durable through the wire
    /// log BEFORE the in-memory insert, so a crash between the two leaves
    /// the log ahead of memory (the resume redelivers).
    ///
    /// At-least-once window: the clear that follows a successful
    /// injection is a separate later op — a crash after the injected turn
    /// but before the clear replays the held fire once more on resume.
    /// Accepted by design (C.4's redelivery contract beats at-most-once
    /// loss); the double injection is visible as an ordinary second cron
    /// turn, not silent corruption.
    pub async fn hold_fire(&self, fire: CronFire) {
        let task_id = fire.task_id.clone();
        // The gate spans the whole write-before-mutate sequence (snapshot →
        // persist → memory upsert) so concurrent holders (the driver's tick
        // and a run-now trigger) serialize — a later fire's memory state
        // can never sit in front of an earlier fire's op in the log.
        let _gate = self.hold_fire_gate.lock().await;
        // 1. Compute the merged snapshot as a PURE read — no memory is
        //    touched yet (the reducer's `merge_held_records` semantics:
        //    coalescedCount adds, the incoming stale/maxed_out
        //    state wins, and a run-now fire's missing scheduled_at — early
        //    by design, m1 — never erases a natural held fire's lateness
        //    anchor).
        let snapshot = {
            let held = self
                .held
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            match held.iter().find(|existing| existing.task_id == task_id) {
                Some(existing) => {
                    let mut merged = existing.clone();
                    merged.coalesced_count += fire.coalesced_count;
                    merged.stale = fire.stale;
                    merged.maxed_out = fire.maxed_out;
                    merged.scheduled_at = fire.scheduled_at.or(existing.scheduled_at);
                    merged
                }
                None => fire,
            }
        };
        // 2. Persist first: a crash between this op and the memory upsert
        //    below leaves the wire log ahead of memory (the resume
        //    redelivers — the C.4 contract). The reverse order would lose
        //    the fire while `check_due`'s cursor has already advanced.
        self.record_op(WireOp::CronHeldAdd {
            task: serde_json::to_value(&snapshot).unwrap_or_default(),
        })
        .await;
        // 3. Only now upsert the in-memory table to the persisted snapshot.
        {
            let mut held = self
                .held
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            match held.iter_mut().find(|existing| existing.task_id == task_id) {
                Some(existing) => *existing = snapshot,
                None => held.push(snapshot),
            }
        }
    }

    /// The held fires waiting for an idle target agent (C.4) — oldest
    /// first (insertion order), so the scheduler's drain delivers in the
    /// order the periods came due.
    pub fn held_fires(&self) -> Vec<CronFire> {
        self.held
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Clear the held fire for a task id (C.4): written after a successful
    /// injection (the settle of write-before-inject) or before a purge
    /// (task deleted). Durable first (`cron.held_clear` op), then the
    /// in-memory removal.
    pub async fn clear_held(&self, task_id: &str) {
        self.record_op(WireOp::CronHeldClear {
            id: task_id.to_string(),
        })
        .await;
        self.held
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .retain(|fire| fire.task_id != task_id);
    }

    /// Drop held fires whose task was deleted after firing (#177, the
    /// held-side translation of the driver's original purge): a delete
    /// during the busy hold cancels the pending injection — the prompt
    /// must not outlive the task that owned it.
    pub async fn purge_deleted_held(&self) {
        let held = self
            .held
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        for fire in held {
            if self.is_delete_requested(&fire.task_id) {
                self.clear_held(&fire.task_id).await;
            }
        }
    }

    /// Restore held fires into this (fresh) manager after a resume (C.4) —
    /// the close-with-held-fires redelivery base. Each record is parsed
    /// individually: one that does not deserialize is skipped with a
    /// warning (it stays in the wire log) so a bad record never fails
    /// the whole resume. Never records wire ops: the entries came from
    /// the log being replayed.
    pub fn restore_held(&self, fires: impl IntoIterator<Item = CronFire>) {
        for fire in fires {
            self.held
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(fire);
        }
    }

    /// Fire one task *now* regardless of its schedule (B6 management
    /// run-now face). Purely additive: no wire op, no cursor advance, no
    /// reschedule, no one-shot consumption — the natural fire still happens
    /// per the expression (D-2). Publishes the `cron.fired` event
    /// (coalesced_count 1, stale false) under the task-table lock — the
    /// same place [`CronManager::check_due`] announces — and returns the
    /// fire for the injection path. `None` when no task with the id exists
    /// or the manager is shut down.
    pub fn trigger(&self, id: &str) -> Option<CronFire> {
        if self.shutdown.load(Ordering::SeqCst) {
            return None;
        }
        let tasks = self
            .tasks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let task = tasks.get(id)?;
        if let Some(bus) = self
            .event_bus
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
        {
            bus.publish(crate::session::SessionEvent::CronFired {
                task_id: task.id.clone(),
                cron: task.cron.clone(),
                prompt: task.prompt.clone(),
                recurring: task.recurring,
                coalesced_count: 1,
                stale: false,
                maxed_out: false,
                readonly: task.permission_posture == Some(PermissionPosture::ReadOnly),
            });
        }
        // Run-now never counts and never moves the cursor (D-M7): the
        // fire below carries the task's CURRENT count and budget state —
        // a run-now delivery is not a scheduled fire and cannot complete
        // a budget. `scheduled_at` is `None`: a manual run has no
        // lateness semantics (it is early by design, not late).
        Some(CronFire {
            task_id: task.id.clone(),
            prompt: task.prompt.clone(),
            cron: task.cron.clone(),
            recurring: task.recurring,
            coalesced_count: 1,
            stale: false,
            maxed_out: false,
            readonly: task.permission_posture == Some(PermissionPosture::ReadOnly),
            scheduled_at: None,
            late_by_ms: None,
            scope: task.scope.clone(),
            ended: false,
        })
    }

    /// Restore persisted tasks into this (fresh) manager after a resume.
    ///
    /// Each task's schedule is rebuilt from its *cursor* —
    /// `max(last_fired, created_at)`, the TS kernel's `baseFromMs` — on
    /// the task's timezone calendar (records without a `timezone` key
    /// evaluate on the host's local zone; an unparseable zone name falls
    /// back to local with a warning, since a single dirty record must not
    /// fail the resume). The persisted absolute timestamp is never
    /// trusted. A schedule computed from a cursor in the past stays due:
    /// the next [`check_due`](CronManager::check_due) delivers the missed
    /// occurrences as one coalesced fire (the TS catch-up semantics)
    /// rather than silently dropping them. A one-shot task that already
    /// fired (its cursor was recorded) is restored dormant: it stays
    /// listed with its `last_fired` preserved but never fires again. A
    /// task whose expression can no longer be scheduled is skipped with a
    /// warning rather than failing the resume. A stale recurring task is
    /// *not* dropped here — stale removal only rides a fire (mirroring
    /// the TS `deliverDue` path).
    ///
    /// Never records wire ops: these tasks came from the wire log being
    /// replayed, and re-appending them would duplicate history.
    pub fn restore_tasks(&self, tasks: impl IntoIterator<Item = CronTask>) {
        let mut restored = 0usize;
        let mut table = self
            .tasks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for mut task in tasks {
            let next_fire_at = if !task.recurring && task.last_fired.is_some() {
                // A fired one-shot: keep it listed (its last_fired shows the
                // completed state) but schedule nothing.
                None
            } else {
                let tz = match CronTimezone::parse(task.timezone.as_deref()) {
                    Ok(declared) => declared.resolve(),
                    Err(error) => {
                        tracing::warn!(
                            task_id = %task.id,
                            %error,
                            "restored cron task carries an unusable timezone; \
                             falling back to the host's local zone"
                        );
                        CronTimezone::Local.resolve()
                    }
                };
                // The TS baseFromMs: the cursor the task last advanced to.
                let base = task
                    .last_fired
                    .filter(|last_fired| *last_fired > task.created_at)
                    .unwrap_or(task.created_at);
                let schedule = task_schedule(&task);
                // B1 / SDR-110: a restored task whose schedule already ran
                // past its end date never reschedules — it restores dormant
                // (its records stay listed until the next mutation).
                match Self::compute_schedule_next_fire(&schedule, base, tz) {
                    Ok(ideal) if occurrence_past_ends_at(&task, ideal, tz) => None,
                    Ok(ideal) => Some(
                        Self::schedule_occurrence_for(
                            &schedule,
                            &task.id,
                            ideal,
                            task.recurring,
                            task.created_at,
                            tz,
                            self.options,
                        )
                        .unwrap_or(ideal),
                    ),
                    Err(error) => {
                        tracing::warn!(
                            task_id = %task.id,
                            %error,
                            "restored cron task is no longer schedulable; skipping it"
                        );
                        continue;
                    }
                }
            };
            task.next_fire_at = next_fire_at;
            table.insert(task.id.clone(), task);
            restored += 1;
        }
        if restored > 0 {
            tracing::info!(restored, "cron tasks restored after resume");
        }
    }

    /// Force a registered task's next fire time — a host/test seam for
    /// driving the due pass deterministically (the scheduler's own writes
    /// always go through `add`/`check_due`, which compute the next fire
    /// from the expression). Returns whether the task exists; a missing
    /// id is a no-op.
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

    /// List all cron tasks, sorted by id for deterministic ordering.
    pub fn list(&self) -> Vec<CronTask> {
        let mut tasks: Vec<CronTask> = self
            .tasks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .cloned()
            .collect();
        tasks.sort_by(|a, b| a.id.cmp(&b.id));
        tasks
    }

    /// Get a cron task by id.
    pub fn get(&self, id: &str) -> Option<CronTask> {
        self.tasks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(id)
            .cloned()
    }

    /// The next fire time an expression will produce after `now` (E.5 /
    /// stage-1 batch-3): the read-only projection face for a task whose
    /// `next_fire_at` is not materialized (a persisted record carries no
    /// absolute next fire — it is recomputed from the expression). This is
    /// the IDEAL, pre-jitter occurrence (the scheduler's own scheduling
    /// math stays untouched; the aggregated view's next-fire is a display
    /// hint, not the delivery clock). `None` when the expression cannot
    /// fire again or the timezone is unusable.
    pub fn next_fire_for(cron: &str, timezone: Option<&str>) -> Option<DateTime<Utc>> {
        let tz = CronTimezone::parse(timezone).ok()?.resolve();
        Self::compute_next_fire(cron, Utc::now(), tz).ok()
    }

    /// The multi-occurrence preview kernel (ADR-017): the next `count`
    /// fire times strictly after `anchor`, under either schedule shape —
    /// the SAME [`Self::compute_schedule_next_fire`] walk `check_due`
    /// plans with (pre-jitter ideal instants, display-only, no task side
    /// effects).
    ///
    /// The occurrences step through ONE continuous walk (each step feeds
    /// the next), never re-anchored per occurrence — a per-step re-anchor
    /// computes the "next fire after now" afresh and can mis-step across
    /// a timezone day boundary. An unusable expression or timezone
    /// answers an empty `Vec` (the preview face renders the honest
    /// empty state, never an error).
    pub fn preview_fires(
        schedule: &ScheduleSpec<'_>,
        timezone: Option<&str>,
        count: usize,
        anchor: DateTime<Utc>,
    ) -> Vec<DateTime<Utc>> {
        let Ok(tz) = CronTimezone::parse(timezone) else {
            return Vec::new();
        };
        let tz = tz.resolve();
        let mut fires = Vec::with_capacity(count.min(64));
        let mut cursor = anchor;
        for _ in 0..count {
            match Self::compute_schedule_next_fire(schedule, cursor, tz) {
                Ok(next) => {
                    fires.push(next);
                    cursor = next;
                }
                Err(_) => break,
            }
        }
        fires
    }

    /// The theoretical PAST occurrence (batch 2 / A5, ADR-4): the last
    /// ideal instant at or before `now` the schedule produces — INCLUSIVE
    /// of `now` when now sits exactly on an occurrence (so
    /// `prev_fire_for(.., now)` and `preview_fires(.., now)` agree about
    /// the same instant: the past owns "now", the future strictly
    /// follows it).
    ///
    /// Two arms, no separate algebra:
    ///
    /// - **cron expression**: a bounded backward DAY-walk on the declared
    ///   zone's LOCAL calendar (at most [`SEARCH_HORIZON_DAYS`] days,
    ///   nearest-first; within a day the hours/minutes step down). Each
    ///   candidate is judged by the expression's own field match, then
    ///   mapped through the SAME [`to_instant`] the forward walk uses — so
    ///   a slot inside a spring-forward gap resolves to the same shifted
    ///   instant in both directions. A candidate past `now` is skipped.
    /// - **interval**: the forward `interval_next_fire` walk from the
    ///   anchor (anchoring + month clamp + `to_instant` — the SAME path
    ///   the next-fire face runs), collecting candidates until one passes
    ///   `now`; the last collected is the answer. No month arithmetic is
    ///   re-derived backwards.
    ///
    /// A schedule with no occurrence before `now` (a future-anchored
    /// interval, an expression whose first slot is still ahead) answers
    /// `None` — the preview face renders "—" instead of fabricating a
    /// past instant. An unusable expression / timezone also answers `None`
    /// (the same honest empty answer `preview_fires` gives).
    pub fn prev_fire_for(
        schedule: &ScheduleSpec<'_>,
        timezone: Option<&str>,
        now: DateTime<Utc>,
    ) -> Option<DateTime<Utc>> {
        let tz = CronTimezone::parse(timezone).ok()?.resolve();
        match schedule {
            ScheduleSpec::Cron(cron) => Self::prev_cron_fire(cron, now, tz),
            ScheduleSpec::Interval(interval) => Self::prev_interval_fire(interval, now, tz),
        }
    }

    /// The cron arm of [`Self::prev_fire_for`]: the bounded backward
    /// DAY-walk judged by the forward kernel (see the caller's docs).
    fn prev_cron_fire(cron: &str, now: DateTime<Utc>, tz: chrono_tz::Tz) -> Option<DateTime<Utc>> {
        let spec = CronSpec::parse(cron).ok()?;
        // Resolve candidates nearest-first: days step back from today's
        // LOCAL date (the expression's fields mean what they say on the
        // declared zone's calendar), within a day the hours and minutes
        // step down. Every candidate is mapped through the SAME
        // `to_instant` the forward walk uses — a slot inside a
        // spring-forward gap resolves to that walk's shifted instant, and
        // a candidate past `now` is not an occurrence yet.
        let now_local = now.with_timezone(&tz).naive_local();
        let mut hours = spec.hour.values.clone();
        hours.sort_unstable_by(|a, b| b.cmp(a));
        let mut minutes = spec.minute.values.clone();
        minutes.sort_unstable_by(|a, b| b.cmp(a));
        let mut day = now_local.date();
        for _ in 0..=SEARCH_HORIZON_DAYS {
            if spec.month.values.contains(&(day.month() as i32)) {
                let day_of_month_matches = spec.day_of_month.values.contains(&(day.day() as i32));
                let day_of_week_matches = spec
                    .day_of_week
                    .values
                    .contains(&(day.weekday().num_days_from_sunday() as i32));
                let day_matches = match (spec.day_of_month.restricted, spec.day_of_week.restricted)
                {
                    (true, true) => day_of_month_matches || day_of_week_matches,
                    _ => day_of_month_matches && day_of_week_matches,
                };
                if day_matches {
                    for &hour in &hours {
                        for &minute in &minutes {
                            let Some(naive) = day.and_hms_opt(hour as u32, minute as u32, 0) else {
                                continue;
                            };
                            if let Some(instant) = to_instant(tz, naive)
                                && instant <= now
                            {
                                return Some(instant);
                            }
                        }
                    }
                }
            }
            day = day.checked_sub_days(chrono::Days::new(1))?;
        }
        None
    }

    /// The interval arm of [`Self::prev_fire_for`]: the forward walk from
    /// the anchor, keeping the last candidate at or before `now`.
    fn prev_interval_fire(
        interval: &IntervalSpec,
        now: DateTime<Utc>,
        tz: chrono_tz::Tz,
    ) -> Option<DateTime<Utc>> {
        // The anchor's local midnight is the floor of the walk — an
        // occurrence before the anchor does not exist by definition.
        let start = interval
            .anchor_date
            .and_hms_opt(0, 0, 0)
            .and_then(|naive| to_instant(tz, naive))?
            - Duration::minutes(1);
        let mut cursor = start;
        let mut last: Option<DateTime<Utc>> = None;
        // The same horizon the forward walk uses, plus the month-clamp
        // slack (identical bound to `interval_next_fire`).
        let horizon = SEARCH_HORIZON_DAYS + 366;
        for _ in 0..horizon {
            let Ok(next) = interval_next_fire(interval, cursor, tz) else {
                break;
            };
            if next > now {
                break;
            }
            last = Some(next);
            if next == now {
                break;
            }
            cursor = next;
        }
        last
    }

    /// The live task's next `count` ideal occurrences (the
    /// [`Self::preview_fires`] walk over whatever schedule shape the task
    /// carries — an interval task branches on `interval`, everything
    /// else on its expression).
    pub fn task_preview_fires(
        task: &CronTask,
        count: usize,
        anchor: DateTime<Utc>,
    ) -> Vec<DateTime<Utc>> {
        let schedule = match &task.interval {
            Some(interval) => ScheduleSpec::Interval(interval),
            None => ScheduleSpec::Cron(&task.cron),
        };
        Self::preview_fires(&schedule, task.timezone.as_deref(), count, anchor)
    }

    /// The stale verdict for one task (B.1's fire-refresh TTL, the
    /// standalone read of [`CronManager::is_stale`]'s rule): a recurring
    /// task whose last fire (or creation, when it never fired) is older
    /// than [`STALE_THRESHOLD`]. `options.stale = false` (the
    /// `MIRRI_CRON_NO_STALE` escape hatch) never stales — that toggle is
    /// process-global, so a caller holding only the task (the
    /// aggregation's replayed rows) reads the same default options the
    /// manager construction would.
    pub fn task_is_stale(task: &CronTask, now: DateTime<Utc>) -> bool {
        let ttl_base = task
            .last_fired
            .filter(|last_fired| *last_fired > task.created_at)
            .unwrap_or(task.created_at);
        CronOptions::default().stale && task.recurring && now - ttl_base >= STALE_THRESHOLD
    }

    /// Check for due tasks and return their fires.
    ///
    /// Should be called periodically by the session's event loop. Every task
    /// with `next_fire_at <= now` fires exactly once per call:
    ///
    /// - overdue periods are **coalesced** into that single fire: the walk
    ///   advances the task's cursor (`max(last_fired, created_at)` — the TS
    ///   `baseFromMs`) occurrence by occurrence and counts every ideal whose
    ///   jittered time has already passed (up to
    ///   [`MAX_COALESCE_ITERATIONS`]); the count rides the fire as
    ///   [`CronFire::coalesced_count`], and the cursor is *advanced to the
    ///   last due occurrence* (not to `now`) so the cadence keeps its
    ///   phase;
    /// - recurring tasks are rescheduled from the advanced cursor;
    /// - one-shot tasks are removed after firing;
    /// - a stale recurring task (see [`CronManager::is_stale`]) fires once
    ///   more, flagged, and is then removed — with a `cron.delete` op
    ///   recorded before the in-memory drop, so the removal survives
    ///   resume;
    /// - a recurring task that can no longer be scheduled (its expression
    ///   stopped matching within the horizon) is removed with a warning.
    ///
    /// A `cron.cursor` op carrying the advanced cursor is recorded for
    /// every due task *before* the in-memory fire is applied, so a crash
    /// between the two leaves the wire log ahead of memory (resume treats
    /// the wire log as the source of truth: a one-shot whose cursor was
    /// recorded is never re-fired). The detection, plan, and apply passes
    /// take the task-table lock separately (the recording in between must
    /// not hold it); the apply pass re-checks each task under its lock, so
    /// the contract above still holds for the single event loop that owns
    /// this manager.
    ///
    /// Once [`shutdown`](CronManager::shutdown) has been called, this always
    /// returns an empty vector. Due tasks are returned sorted by id so
    /// injection order is deterministic across calls.
    pub async fn check_due(&self) -> Vec<CronFire> {
        if self.shutdown.load(Ordering::SeqCst) {
            return Vec::new();
        }
        let now = Utc::now();
        // Detection + plan pass: compute each due task's fire (coalesced
        // count, stale flag, advanced cursor, reschedule) without mutating
        // anything yet.
        let mut plans: Vec<FirePlan> = {
            let tasks = self
                .tasks
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            tasks
                .values()
                .filter(|task| task.next_fire_at.is_some_and(|next| next <= now))
                .map(|task| self.plan_fire(task, now))
                .collect()
        };
        if plans.is_empty() {
            return Vec::new();
        }
        plans.sort_by(|a, b| a.task.id.cmp(&b.task.id));
        // Write-before-mutate: a cron.cursor op per due task (carrying the
        // *advanced* cursor, so a resumed process picks up where the
        // cadence left off), plus the durable delete of a stale task's
        // final delivery — both before any in-memory fire is applied.
        for plan in &plans {
            // S3: the cursor op carries the post-fire count (same lock
            // as the delivery — the budget verdict survives a crash).
            self.record_op(WireOp::CronCursor {
                id: plan.task.id.clone(),
                last_fired_at: plan.advanced.timestamp_millis() as u64,
                fire_count: Some(plan.task.fire_count + 1),
            })
            .await;
            if plan.stale {
                tracing::info!(
                    task_id = %plan.task.id,
                    "stale cron task auto-deleted after its final delivery"
                );
                self.record_op(WireOp::CronDelete {
                    ids: vec![plan.task.id.clone()],
                })
                .await;
            }
            if plan.maxed_out {
                tracing::info!(
                    task_id = %plan.task.id,
                    max_fires = plan.task.max_fires.unwrap_or(0),
                    "cron task auto-deleted after reaching its fire budget"
                );
                self.record_op(WireOp::CronDelete {
                    ids: vec![plan.task.id.clone()],
                })
                .await;
            }
            if plan.ended {
                tracing::info!(
                    task_id = %plan.task.id,
                    ends_at = %plan.task.ends_at.map(|date| date.to_string()).unwrap_or_default(),
                    "cron task auto-deleted after passing its end date"
                );
                self.record_op(WireOp::CronDelete {
                    ids: vec![plan.task.id.clone()],
                })
                .await;
            }
        }
        // Apply pass: apply the in-memory transitions, re-checking each task
        // under the lock (a task removed between the two passes is skipped).
        let mut due = Vec::new();
        let mut to_remove = Vec::new();
        {
            let mut tasks = self
                .tasks
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            for plan in plans {
                let Some(task) = tasks.get_mut(&plan.task.id) else {
                    continue;
                };
                if task.next_fire_at.is_none_or(|next| next > now) {
                    continue;
                }
                // Announce the fire on the session's event bus (the
                // `cron.fired` wire event) — published under the task-table
                // lock, before the reschedule/remove transition mutates the
                // task, so the event carries the firing task's full
                // identity plus the real coalesced/stale values.
                if let Some(bus) = self
                    .event_bus
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .as_ref()
                {
                    bus.publish(crate::session::SessionEvent::CronFired {
                        task_id: task.id.clone(),
                        cron: task.cron.clone(),
                        prompt: task.prompt.clone(),
                        recurring: task.recurring,
                        coalesced_count: plan.coalesced_count,
                        stale: plan.stale,
                        maxed_out: plan.maxed_out,
                        readonly: task.permission_posture == Some(PermissionPosture::ReadOnly),
                    });
                }
                task.last_fired = Some(plan.advanced);
                // S3: the delivery count advances with the cursor (same
                // lock — the budget verdict and the delivery are atomic).
                task.fire_count += 1;
                if task.recurring && !plan.stale && !plan.maxed_out && !plan.ended {
                    match plan.next_fire_at {
                        Some(next) => task.next_fire_at = Some(next),
                        None => {
                            tracing::warn!(
                                task_id = %task.id,
                                "recurring cron task is no longer schedulable; removing it"
                            );
                            to_remove.push(task.id.clone());
                        }
                    }
                } else {
                    // One-shot (its single fire), stale (its final
                    // delivery), maxed-out (its budget-exhausting
                    // delivery), or ended (its end-date finale): the task
                    // goes away after this fire.
                    to_remove.push(task.id.clone());
                }
                due.push(CronFire {
                    task_id: task.id.clone(),
                    prompt: task.prompt.clone(),
                    cron: task.cron.clone(),
                    recurring: task.recurring,
                    coalesced_count: plan.coalesced_count,
                    stale: plan.stale,
                    maxed_out: plan.maxed_out,
                    readonly: task.permission_posture == Some(PermissionPosture::ReadOnly),
                    scheduled_at: plan.scheduled_at,
                    late_by_ms: None,
                    scope: task.scope.clone(),
                    ended: plan.ended,
                });
            }
            for id in to_remove {
                tasks.remove(&id);
            }
        }
        due.sort_by(|a, b| a.task_id.cmp(&b.task_id));
        due
    }

    /// Whether `task` has crossed the stale threshold at `now`: only
    /// recurring tasks stale (one-shots are removed by their one-fire
    /// rule), only when the manager's [`CronOptions::stale`] is on, and
    /// only at fire time — stale removal always rides a delivery.
    ///
    /// The TTL base is the task's most recent successful fire, falling
    /// back to the creation when it has never fired (B.1 / D-M5,
    /// fire-refresh): an active task that keeps firing never stales no
    /// matter its age; a stalled task — one whose last fire (or creation)
    /// is older than [`STALE_THRESHOLD`] — stales on its next check.
    pub fn is_stale(&self, task: &CronTask, now: DateTime<Utc>) -> bool {
        let ttl_base = task
            .last_fired
            .filter(|last_fired| *last_fired > task.created_at)
            .unwrap_or(task.created_at);
        self.options.stale && task.recurring && now - ttl_base >= STALE_THRESHOLD
    }

    /// Build a fully populated task (id minted, next fire computed) without
    /// registering it — the shared builder the session `add` path and the
    /// workspace manager's `add` both reach (T3: the scheduling math is
    /// scope-independent). Not persisted by this function; the callers own
    /// their write-before-mutate sequencing.
    pub(crate) fn prepare_task(
        cron: String,
        prompt: String,
        session_id: String,
        recurring: bool,
        options: CronCreateOptions,
        manager_options: CronOptions,
    ) -> Result<CronTask, CronError> {
        // ADR-016 revision / SDR-113: a cron expression and an interval
        // are mutually exclusive — one task carries exactly one schedule
        // shape. An interval task leaves the expression field empty (the
        // consumers branch on `interval: Some` before ever parsing it).
        if options.interval.is_some() && !cron.trim().is_empty() {
            return Err(CronError::InvalidExpression(
                "a task carries either a cron expression or an interval, not both".to_string(),
            ));
        }
        if let Some(interval) = &options.interval {
            interval.validate()?;
        }
        // An unusable timezone name is rejected up front — the caller
        // (model-facing tool) surfaces the message verbatim.
        let tz = CronTimezone::parse(options.timezone.as_deref())?.resolve();
        let id = format!("cron-{}", Uuid::new_v4());
        let created_at = Utc::now();
        // A4: the declared mode fixes the posture (its kernel expansion);
        // an explicit posture without a mode stays as given (legacy
        // callers).
        let mut permission_posture = options.permission_posture;
        if let Some(mode) = options.permission_mode {
            permission_posture = Some(mode.posture());
        }
        let schedule = if let Some(interval) = &options.interval {
            ScheduleSpec::Interval(interval)
        } else {
            ScheduleSpec::Cron(&cron)
        };
        let ideal = Self::compute_schedule_next_fire(&schedule, created_at, tz)?;
        let next_fire_at = Self::schedule_occurrence_for(
            &schedule,
            &id,
            ideal,
            recurring,
            created_at,
            tz,
            manager_options,
        )?;
        Ok(CronTask {
            id,
            created_at,
            last_fired: None,
            next_fire_at: Some(next_fire_at),
            cron,
            prompt,
            session_id,
            recurring,
            timezone: options.timezone,
            origin: options.origin,
            max_fires: options.max_fires,
            fire_count: 0,
            cron_lineage: options.cron_lineage,
            scope: options.scope,
            permission_posture,
            permission_mode: options.permission_mode,
            paused: false,
            concurrency: options.concurrency,
            title: options.title,
            model: options.model,
            effort: options.effort,
            workdir: options.workdir,
            ends_at: options.ends_at,
            interval: options.interval,
        })
    }

    /// Apply the per-task jitter to one ideal occurrence, producing the
    /// moment the task will actually fire at (schedule-agnostic — the
    /// form batch 6 generalization of the expression-only original).
    /// With jitter disabled (or a zero-offset fraction) this is the
    /// identity. The jitter math consults the NEXT ideal occurrence,
    /// whichever schedule shape computes it. Interval tasks jitter
    /// through their own next step (the interval walk is pure calendar
    /// arithmetic); a one-shot task takes the one-shot jitter path.
    pub(crate) fn schedule_occurrence_for(
        schedule: &ScheduleSpec<'_>,
        id: &str,
        ideal: DateTime<Utc>,
        recurring: bool,
        created_at: DateTime<Utc>,
        tz: chrono_tz::Tz,
        options: CronOptions,
    ) -> Result<DateTime<Utc>, CronError> {
        if !options.jitter {
            return Ok(ideal);
        }
        if !recurring {
            return Ok(one_shot_jittered_next(
                id,
                ideal,
                created_at,
                tz,
                &DEFAULT_CRON_JITTER_CONFIG,
            ));
        }
        let next_ideal = match schedule {
            ScheduleSpec::Cron(cron) => Self::compute_next_fire(cron, ideal, tz).ok(),
            ScheduleSpec::Interval(interval) => interval_next_fire(interval, ideal, tz).ok(),
        };
        Ok(jittered_next_run(
            id,
            ideal,
            next_ideal,
            &DEFAULT_CRON_JITTER_CONFIG,
        ))
    }

    /// The single schedule kernel (ADR-017): the next fire strictly after
    /// `from` under either schedule shape. The cron arm is the historical
    /// [`Self::compute_next_fire`]; the interval arm walks
    /// `anchor_date + k × every × unit` on `tz`'s local calendar at the
    /// spec's `time` (DST edges resolve through the same [`to_instant`]
    /// rules the expression walk uses).
    fn compute_schedule_next_fire(
        schedule: &ScheduleSpec<'_>,
        from: DateTime<Utc>,
        tz: chrono_tz::Tz,
    ) -> Result<DateTime<Utc>, CronError> {
        match schedule {
            ScheduleSpec::Cron(cron) => Self::compute_next_fire(cron, from, tz),
            ScheduleSpec::Interval(interval) => interval_next_fire(interval, from, tz),
        }
    }

    /// The schedule-agnostic next-fire walk, exposed for the workspace
    /// store's create-time validations (the ends_at first-fire check
    /// rides the same kernel — one evaluation face, ADR-017).
    pub(crate) fn compute_schedule_next_fire_pub(
        schedule: &ScheduleSpec<'_>,
        from: DateTime<Utc>,
        tz: chrono_tz::Tz,
    ) -> Result<DateTime<Utc>, CronError> {
        Self::compute_schedule_next_fire(schedule, from, tz)
    }

    fn plan_fire(&self, task: &CronTask, now: DateTime<Utc>) -> FirePlan {
        let tz = match CronTimezone::parse(task.timezone.as_deref()) {
            Ok(declared) => declared.resolve(),
            Err(error) => {
                tracing::warn!(
                    task_id = %task.id,
                    %error,
                    "cron task carries an unusable timezone; firing on the host's local zone"
                );
                CronTimezone::Local.resolve()
            }
        };
        let schedule = task_schedule(task);
        let stale = self.is_stale(task, now);
        // S3: the budget verdict — this delivery is the task's last when
        // it exhausts max_fires. Computed on the plan (pre-fire) count:
        // stale wins first (the existing plan order), a surviving budgeted
        // task then checks its budget.
        let maxed_out = !stale
            && task.recurring
            && task.max_fires.is_some_and(|max| task.fire_count + 1 >= max);
        let mut count = 0usize;
        let mut last_due = None;
        if task.recurring {
            // The TS baseFromMs: the cursor the task last advanced to.
            let base = task
                .last_fired
                .filter(|last_fired| *last_fired > task.created_at)
                .unwrap_or(task.created_at);
            let mut cursor = base;
            while count < MAX_COALESCE_ITERATIONS {
                let ideal = match Self::compute_schedule_next_fire(&schedule, cursor, tz) {
                    Ok(ideal) => ideal,
                    Err(_) => break,
                };
                // Review Major 2: the walk never crosses the end date — an
                // occurrence past `ends_at`'s local day stops the coalesce
                // walk without counting it (a recovered backlog must not
                // coalesce occurrences the schedule had already ended on).
                if occurrence_past_ends_at(task, ideal, tz) {
                    break;
                }
                let jittered = Self::schedule_occurrence_for(
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
        // B1 / SDR-110: the end-date finale. The end date is INCLUSIVE on
        // the occurrence's local day — a next occurrence past that day
        // never schedules, and the delivery that exhausted the schedule
        // carries the `ended` mark (the max_fires finale's sibling).
        let ended = task
            .ends_at
            .is_some_and(|_| ends_past_schedule(task, &schedule, advanced, tz));
        // The post-fire schedule: recurring survivors reschedule from the
        // advanced cursor (jittered), clamped at the end date; one-shots,
        // stale, ended, and maxed-out tasks need none.
        let next_fire_at = if task.recurring && !stale && !maxed_out && !ended {
            Self::compute_schedule_next_fire(&schedule, advanced, tz)
                .ok()
                .filter(|ideal| !occurrence_past_ends_at(task, *ideal, tz))
                .map(|ideal| {
                    Self::schedule_occurrence_for(
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

    /// Compute the next fire time for a cron expression.
    ///
    /// Returns the smallest time strictly after `from` (minute precision)
    /// that matches the expression, or [`CronError::NoNextFire`] when no
    /// match exists within the search horizon. The expression's fields are
    /// evaluated on `tz`'s local calendar — the returned value is still an
    /// absolute UTC instant (the internal currency never changes).
    fn compute_next_fire(
        cron: &str,
        from: DateTime<Utc>,
        tz: chrono_tz::Tz,
    ) -> Result<DateTime<Utc>, CronError> {
        let spec = CronSpec::parse(cron)?;
        // The candidate search runs on the zone's local wall clock: the
        // start is the next local minute boundary strictly after `from`
        // (a fire time must always be in the future).
        let start = (from.with_timezone(&tz).naive_local() + Duration::minutes(1))
            .with_second(0)
            .and_then(|moment| moment.with_nanosecond(0))
            .ok_or_else(|| {
                CronError::InvalidExpression(format!("timestamp out of range: {from}"))
            })?;
        let horizon = start + Duration::days(SEARCH_HORIZON_DAYS);

        // Search day by day: at most one midnight iteration per day, so the
        // full horizon costs only a few thousand iterations even for
        // expressions that fire once a year. Day matching happens on the
        // local calendar (dom/dow mean what they say in `tz`); each
        // candidate wall time is then mapped back to an instant under the
        // DST edge rules of [`to_instant`].
        let first_day = start.date().and_hms_opt(0, 0, 0).ok_or_else(|| {
            CronError::InvalidExpression(format!("timestamp out of range: {from}"))
        })?;
        let mut day = first_day;
        while day <= horizon {
            let month_matches = spec.month.values.contains(&(day.month() as i32));
            let day_of_month_matches = spec.day_of_month.values.contains(&(day.day() as i32));
            let day_of_week_matches = spec
                .day_of_week
                .values
                .contains(&(day.weekday().num_days_from_sunday() as i32));
            let day_matches = match (spec.day_of_month.restricted, spec.day_of_week.restricted) {
                // Standard cron: when both fields are restricted, either may
                // satisfy the day.
                (true, true) => day_of_month_matches || day_of_week_matches,
                _ => day_of_month_matches && day_of_week_matches,
            };
            if month_matches && day_matches {
                let is_first_day = day == first_day;
                let hour_floor = if is_first_day { start.hour() as i32 } else { 0 };
                for &hour in spec.hour.values.iter().filter(|hour| **hour >= hour_floor) {
                    let minute_floor = if is_first_day && hour == hour_floor {
                        start.minute() as i32
                    } else {
                        0
                    };
                    for &minute in spec
                        .minute
                        .values
                        .iter()
                        .filter(|minute| **minute >= minute_floor)
                    {
                        let naive = day
                            .date()
                            .and_hms_opt(hour as u32, minute as u32, 0)
                            // Values were range-checked during parsing, so both
                            // setters are total.
                            .expect("validated cron field values are always settable");
                        // A candidate that cannot be mapped to an instant, or whose
                        // earliest instant is still at or before `from` (the
                        // fold of a fall-back transition, seen from its second
                        // pass) — keep scanning the following candidates.
                        if let Some(fire) = to_instant(tz, naive).filter(|fire| *fire > from) {
                            return Ok(fire);
                        }
                    }
                }
            }
            day += Duration::days(1);
        }
        Err(CronError::NoNextFire(cron.to_string()))
    }

    /// Shutdown the cron manager.
    ///
    /// After this call [`check_due`](CronManager::check_due) stops firing
    /// tasks; registered tasks remain queryable but are never due again.
    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
        tracing::info!("cron manager shut down");
    }
}

/// The elapsed-interval branch of the schedule kernel (ADR-016 revision /
/// SDR-113): the first `time` occurrence strictly after `from`, counting
/// `anchor_date + k × every × unit` on `tz`'s local calendar.
///
/// Calendar arithmetic: months add to the same day-of-month when it
/// exists (Jan 31 + 1 month = Feb 28), else the month's last day — the
/// `chrono` `checked_add_months` semantics; weeks/days are plain day
/// arithmetic. Each candidate's `time` maps to an instant through
/// [`to_instant`]'s DST edge rules, so a `time` inside a spring-forward
/// gap shifts forward exactly like the cron walk's candidates.
///
/// The search is bounded by [`SEARCH_HORIZON_DAYS`] for parity with the
/// expression walk (a gap-day month-clamped step can theoretically skip
/// past `from` — the loop keeps advancing until a candidate clears it).
fn interval_next_fire(
    interval: &IntervalSpec,
    from: DateTime<Utc>,
    tz: chrono_tz::Tz,
) -> Result<DateTime<Utc>, CronError> {
    let (hour, minute) = parse_hhmm(&interval.time).ok_or_else(|| {
        CronError::InvalidExpression(format!(
            "interval time must be HH:MM, got '{}'",
            interval.time
        ))
    })?;
    if interval.every == 0 {
        return Err(CronError::InvalidExpression(
            "interval every must be a positive integer".to_string(),
        ));
    }
    let horizon_days = SEARCH_HORIZON_DAYS + 366; // month-clamp slack
    // The anchor's local midnight is step k = 0; walk k = 0, 1, 2, ...
    // and return the first mapped instant strictly after `from`.
    let mut k: i64 = 0;
    let mut last_date = interval.anchor_date;
    loop {
        let naive = last_date.and_hms_opt(hour, minute, 0).ok_or_else(|| {
            CronError::InvalidExpression("interval time out of range".to_string())
        })?;
        if let Some(fire) = to_instant(tz, naive).filter(|fire| *fire > from) {
            return Ok(fire);
        }
        k += 1;
        if k > horizon_days {
            return Err(CronError::NoNextFire(format!(
                "interval every={} {:?} from {}",
                interval.every, interval.unit, interval.anchor_date
            )));
        }
        last_date = match interval.unit {
            IntervalUnit::Day => last_date
                .checked_add_signed(chrono::Duration::days(interval.every as i64))
                .ok_or_else(|| CronError::NoNextFire(String::new()))?,
            IntervalUnit::Week => last_date
                .checked_add_signed(chrono::Duration::weeks(interval.every as i64))
                .ok_or_else(|| CronError::NoNextFire(String::new()))?,
            IntervalUnit::Month => {
                // Review Blocker 1: the month arm anchors every step on the
                // ORIGINAL anchor (anchor + k × every), never on the previous
                // step — deriving each step from `last_date` compounds the
                // months ((2k-1) × every, skipping occurrences). Computing
                // from the original anchor also keeps the walk O(k) total
                // (one month addition per step, not one per prior step).
                interval
                    .anchor_date
                    .with_day(1)
                    .and_then(|first| {
                        first.checked_add_months(chrono::Months::new(
                            (k * interval.every as i64) as u32,
                        ))
                    })
                    .and_then(|shifted| {
                        // Restore the anchor's day-of-month when it
                        // exists in the target month, else clamp to the
                        // month's last day (Jan 31 → Feb 28).
                        let day = interval
                            .anchor_date
                            .day()
                            .min(days_in_month(shifted.year(), shifted.month()));
                        shifted.with_day(day)
                    })
                    .ok_or_else(|| CronError::NoNextFire(String::new()))?
            }
        };
    }
}

/// How many days a month carries (the interval month-clamp's bound).
fn days_in_month(year: i32, month: u32) -> u32 {
    let (next_year, next_month) = if month == 12 {
        (year + 1, 1)
    } else {
        (year, month + 1)
    };
    let first_of_next =
        chrono::NaiveDate::from_ymd_opt(next_year, next_month, 1).expect("a valid first-of-month");
    let first_of_this =
        chrono::NaiveDate::from_ymd_opt(year, month, 1).expect("a valid first-of-month");
    (first_of_next - first_of_this).num_days() as u32
}

/// The task's schedule shape (ADR-017): an interval task evaluates
/// through the interval branch, everything else through its expression —
/// the single branch both managers' fire planning and the preview walk
/// key on.
pub(crate) fn task_schedule(task: &CronTask) -> ScheduleSpec<'_> {
    match &task.interval {
        Some(interval) => ScheduleSpec::Interval(interval),
        None => ScheduleSpec::Cron(&task.cron),
    }
}

/// Whether an occurrence's LOCAL day falls strictly after the task's
/// `ends_at` (B1 / SDR-110 — the end date is inclusive: the last legal
/// occurrence is one whose local day equals `ends_at`). `None` ends:
/// never past.
pub(crate) fn occurrence_past_ends_at(
    task: &CronTask,
    occurrence: DateTime<Utc>,
    tz: chrono_tz::Tz,
) -> bool {
    task.ends_at
        .is_some_and(|ends_at| occurrence.with_timezone(&tz).date_naive() > ends_at)
}

/// Whether the task's schedule has run past its `ends_at` (the finale
/// verdict): true when NO further occurrence would land on or before the
/// end date — the next occurrence after `advanced` already falls past
/// `ends_at`'s local day.
pub(crate) fn ends_past_schedule(
    task: &CronTask,
    schedule: &ScheduleSpec<'_>,
    advanced: DateTime<Utc>,
    tz: chrono_tz::Tz,
) -> bool {
    let Some(ends_at) = task.ends_at else {
        return false;
    };
    // The advanced cursor's own day already past the end: no question.
    if advanced.with_timezone(&tz).date_naive() > ends_at {
        return true;
    }
    // Otherwise the finale arrives when the NEXT occurrence would fall
    // past the end date (the current delivery is the last legal one).
    match CronManager::compute_schedule_next_fire_pub(schedule, advanced, tz) {
        Ok(next) => next.with_timezone(&tz).date_naive() > ends_at,
        Err(_) => true, // no next occurrence at all: the schedule is over
    }
}

/// Map a local wall-clock candidate onto an instant under `tz`.
///
/// DST edge rules (aligned with the JS `Date` evaluator the TS kernel
/// uses — the semantic baseline of B5):
///
/// - a single mapping wins outright;
/// - an ambiguous local time (the fall-back fold) resolves to the
///   *earliest* instant — the offset in force before the transition;
/// - a non-existent local time (the spring-forward gap) is interpreted
///   with the offset in force one minute before the gap: `02:30` on a
///   2→3 jump lands on the instant the wall clock shows `03:30`.
fn to_instant(tz: chrono_tz::Tz, naive: NaiveDateTime) -> Option<DateTime<Utc>> {
    use chrono::{Offset, TimeZone};
    match tz.from_local_datetime(&naive) {
        chrono::LocalResult::Single(moment) => Some(moment.with_timezone(&Utc)),
        chrono::LocalResult::Ambiguous(earliest, _) => Some(earliest.with_timezone(&Utc)),
        chrono::LocalResult::None => {
            // A non-existent local time (the spring-forward gap): interpret
            // it with the offset in force just *before* the gap began — the
            // JS `new Date(...)` normalization (`02:30` on a 2->3 jump lands
            // on the instant the wall clock shows `03:30`). The candidate may
            // sit deeper into the gap than its first minute, so walk back
            // minute by minute (gaps are at most a few hours) to the last
            // wall minute that exists.
            let mut pre_gap = naive - Duration::minutes(1);
            for _ in 0..(24 * 60) {
                match tz.offset_from_local_datetime(&pre_gap) {
                    chrono::LocalResult::Single(offset)
                    | chrono::LocalResult::Ambiguous(offset, _) => {
                        return Some(Utc.from_utc_datetime(&(naive - offset.fix())));
                    }
                    chrono::LocalResult::None => pre_gap -= Duration::minutes(1),
                }
            }
            None
        }
    }
}

/// The set of values a single cron field may take, plus whether the field was
/// a bare wildcard (`*`).
#[derive(Debug, Clone, PartialEq, Eq)]
struct FieldSpec {
    /// Allowed values, sorted ascending and deduplicated.
    values: Vec<i32>,
    /// `false` only for a bare `*` field; any concrete selection (including
    /// `*/5`) counts as restricted.
    restricted: bool,
}

/// A parsed 5-field cron expression.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CronSpec {
    minute: FieldSpec,
    hour: FieldSpec,
    day_of_month: FieldSpec,
    month: FieldSpec,
    day_of_week: FieldSpec,
}

impl CronSpec {
    /// Parse a 5-field cron expression.
    fn parse(cron: &str) -> Result<Self, CronError> {
        let fields: Vec<&str> = cron.split_whitespace().collect();
        if fields.len() != 5 {
            return Err(CronError::InvalidExpression(format!(
                "expected 5 fields (minute hour day-of-month month day-of-week), got {}: '{cron}'",
                fields.len()
            )));
        }
        Ok(Self {
            minute: parse_field(fields[0], 0, 59, "minute", identity)?,
            hour: parse_field(fields[1], 0, 23, "hour", identity)?,
            day_of_month: parse_field(fields[2], 1, 31, "day-of-month", identity)?,
            month: parse_field(fields[3], 1, 12, "month", identity)?,
            day_of_week: parse_field(fields[4], 0, 7, "day-of-week", normalize_sunday)?,
        })
    }
}

fn identity(value: i32) -> i32 {
    value
}

/// Map `7` to `0` so both spellings of Sunday match the same weekday.
fn normalize_sunday(value: i32) -> i32 {
    if value == 7 { 0 } else { value }
}

/// Parse one cron field.
///
/// `min` and `max` bound the raw values (before `normalize` is applied);
/// `normalize` maps accepted raw values onto the lookup domain (day-of-week
/// `7` → `0`). Returns an error mentioning `field_name` for diagnostics.
fn parse_field(
    input: &str,
    min: i32,
    max: i32,
    field_name: &str,
    normalize: impl Fn(i32) -> i32,
) -> Result<FieldSpec, CronError> {
    let mut values: Vec<i32> = Vec::new();
    // A bare `*` is the only wildcard form; `*/5` is restricted.
    let mut restricted = false;
    for atom in input.split(',') {
        if atom.is_empty() {
            return Err(CronError::InvalidExpression(format!(
                "empty list entry in {field_name} field: '{input}'"
            )));
        }
        let pieces: Vec<&str> = atom.split('/').collect();
        if pieces.len() > 2 {
            return Err(CronError::InvalidExpression(format!(
                "too many '/' separators in {field_name} field: '{atom}'"
            )));
        }
        let step = if let Some(step) = pieces.get(1) {
            let step: i32 = step.trim().parse().map_err(|_| {
                CronError::InvalidExpression(format!(
                    "step '{step}' is not an integer in {field_name} field: '{atom}'"
                ))
            })?;
            if step <= 0 {
                return Err(CronError::InvalidExpression(format!(
                    "step must be >= 1 in {field_name} field: '{atom}'"
                )));
            }
            step
        } else {
            1
        };
        let base = pieces[0];
        let (lo, hi) = if base == "*" {
            // `*/S` keeps the wildcard semantics: it steps across the whole
            // field domain.
            (min, max)
        } else {
            restricted = true;
            if let Some((lo_text, hi_text)) = base.split_once('-') {
                let lo: i32 = parse_value(lo_text, field_name, atom)?;
                let hi: i32 = parse_value(hi_text, field_name, atom)?;
                if lo > hi {
                    return Err(CronError::InvalidExpression(format!(
                        "range start {lo} exceeds range end {hi} in {field_name} field: '{atom}'"
                    )));
                }
                (lo, hi)
            } else {
                let value = parse_value(base, field_name, atom)?;
                // A single value with a step (`5/15`) runs from the value to
                // the field maximum; without a step it is a fixed value.
                let hi = if pieces.len() == 2 { max } else { value };
                (value, hi)
            }
        };
        if lo < min || hi > max {
            return Err(CronError::InvalidExpression(format!(
                "value out of range {min}..={max} in {field_name} field: '{atom}'"
            )));
        }
        for value in (lo..=hi).step_by(step as usize) {
            values.push(normalize(value));
        }
    }
    values.sort_unstable();
    values.dedup();
    Ok(FieldSpec { values, restricted })
}

/// Parse a single numeric token and reject non-numeric input.
fn parse_value(text: &str, field_name: &str, atom: &str) -> Result<i32, CronError> {
    text.trim().parse().map_err(|_| {
        CronError::InvalidExpression(format!(
            "'{text}' is not an integer in {field_name} field: '{atom}'"
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use mirri_persistence::{AppendLogStore, JsonlAppendLog};

    // -----------------------------------------------------------------------
    // scheduling_disabled_from_env (ADR-006 / E.4)
    // -----------------------------------------------------------------------

    #[test]
    fn should_treat_only_the_value_one_as_the_disable_value_when_parsing_the_env_switch() {
        // Exactly "1" disables — the same rule `env_flag` applies to the
        // other cron env switches.
        assert!(!env_flag(None));
        assert!(env_flag(Some("1".to_string())));
        assert!(!env_flag(Some("0".to_string())));
        assert!(!env_flag(Some("true".to_string())));
        assert!(!env_flag(Some("".to_string())));
    }

    fn utc(y: i32, m: u32, d: u32, h: u32, min: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, m, d, h, min, 0).unwrap()
    }

    /// A single-fire `CronFire` expectation (the pre-B5 shape: nothing
    /// coalesced, nothing stale).
    fn single_fire(task_id: &str, prompt: &str, cron: &str, recurring: bool) -> CronFire {
        CronFire {
            task_id: task_id.to_string(),
            prompt: prompt.to_string(),
            cron: cron.to_string(),
            recurring,
            coalesced_count: 1,
            stale: false,
            maxed_out: false,
            readonly: false,
            scheduled_at: None,
            late_by_ms: None,
            scope: TaskScope::Session,
            ended: false,
        }
    }

    // -----------------------------------------------------------------------
    // compute_next_fire
    // -----------------------------------------------------------------------

    // -----------------------------------------------------------------------
    // form batch 6: the schedule kernel (preview_fires + interval, ADR-016/017)
    // -----------------------------------------------------------------------

    fn interval_spec(every: u32, unit: IntervalUnit, time: &str, anchor: &str) -> IntervalSpec {
        IntervalSpec {
            every,
            unit,
            time: time.to_string(),
            anchor_date: chrono::NaiveDate::parse_from_str(anchor, "%Y-%m-%d").unwrap(),
        }
    }

    /// Given a `* * * * *` schedule, when previewed for consecutive
    /// moments, then each occurrence equals the single-step walk from its
    /// predecessor — the continuous walk never mis-steps a day boundary.
    #[test]
    fn should_keep_the_continuous_walk_equal_to_per_occurrence_stepping_when_previewing_every_minute()
     {
        let anchor = utc(2026, 11, 1, 9, 0);
        let fires =
            CronManager::preview_fires(&ScheduleSpec::Cron("* * * * *"), Some("UTC"), 5, anchor);
        assert_eq!(fires.len(), 5);
        let mut expected = anchor;
        for fire in &fires {
            expected =
                CronManager::compute_next_fire("* * * * *", expected, chrono_tz::UTC).unwrap();
            assert_eq!(*fire, expected);
        }
    }

    /// Given an every-2-days interval, when the walk crosses a month
    /// boundary, then the chain never breaks — the pseudo-expression
    /// `*/2` in dom (which skips months without a 2nd... rather, misses
    /// day counts) cannot say the same.
    #[test]
    fn should_not_break_the_chain_when_an_every_two_days_interval_crosses_a_month() {
        let spec = interval_spec(2, IntervalUnit::Day, "09:30", "2026-11-28");
        let fires = CronManager::preview_fires(
            &ScheduleSpec::Interval(&spec),
            Some("UTC"),
            4,
            utc(2026, 11, 28, 0, 0),
        );
        let days: Vec<chrono::NaiveDate> = fires
            .iter()
            .map(|fire| fire.with_timezone(&chrono_tz::UTC).date_naive())
            .collect();
        assert_eq!(
            days,
            vec![
                chrono::NaiveDate::from_ymd_opt(2026, 11, 28).unwrap(),
                chrono::NaiveDate::from_ymd_opt(2026, 11, 30).unwrap(),
                // The month boundary: Dec 2 follows Nov 30 — the elapsed
                // interval keeps counting.
                chrono::NaiveDate::from_ymd_opt(2026, 12, 2).unwrap(),
                chrono::NaiveDate::from_ymd_opt(2026, 12, 4).unwrap(),
            ]
        );
        // Every occurrence lands at the spec's time of day.
        assert!(
            fires
                .iter()
                .all(|fire| fire.with_timezone(&chrono_tz::UTC).time()
                    == chrono::NaiveTime::from_hms_opt(9, 30, 0).unwrap())
        );
    }

    /// Given a time inside a spring-forward gap, when the interval walks,
    /// then the occurrence maps forward exactly like the cron walk's
    /// candidates (the same to_instant DST edge rules).
    #[test]
    fn should_shift_the_occurrence_forward_when_the_interval_time_lands_in_a_dst_gap() {
        // New York 2026-03-08: 02:00 → 03:00 (a 2-hour time inside the gap).
        let spec = interval_spec(7, IntervalUnit::Day, "02:30", "2026-03-01");
        let fires = CronManager::preview_fires(
            &ScheduleSpec::Interval(&spec),
            Some("America/New_York"),
            1,
            utc(2026, 3, 7, 12, 0),
        );
        assert_eq!(fires.len(), 1);
        // The wall clock reads 03:30 after the shift (the JS Date
        // normalization both engines align with).
        let local = fires[0].with_timezone(&chrono_tz::America::New_York);
        assert_eq!(
            local.time(),
            chrono::NaiveTime::from_hms_opt(3, 30, 0).unwrap()
        );
    }

    // -----------------------------------------------------------------------
    // batch 2 (A5 / ADR-4): the theoretical past occurrence
    // -----------------------------------------------------------------------

    /// Given a daily 09:00 schedule and a mid-day now, when the theoretical
    /// past occurrence is evaluated, then it is today's 09:00 — the last
    /// ideal occurrence at or before now.
    #[test]
    fn should_answer_todays_occurrence_as_the_past_when_now_is_after_it() {
        let prev = CronManager::prev_fire_for(
            &ScheduleSpec::Cron("0 9 * * *"),
            Some("UTC"),
            utc(2026, 11, 1, 12, 0),
        );
        assert_eq!(prev, Some(utc(2026, 11, 1, 9, 0)));
    }

    /// Given now lands EXACTLY on an occurrence, when the past and the next
    /// are evaluated, then the past INCLUDES now and the next is the one
    /// after — the two faces never disagree about the same instant.
    #[test]
    fn should_include_now_in_the_past_and_exclude_it_from_the_next_when_now_is_on_a_fire() {
        let now = utc(2026, 11, 1, 9, 0);
        let schedule = ScheduleSpec::Cron("0 9 * * *");
        assert_eq!(
            CronManager::prev_fire_for(&schedule, Some("UTC"), now),
            Some(now),
            "the past occurrence includes the instant now sits on"
        );
        let next = CronManager::preview_fires(&schedule, Some("UTC"), 1, now);
        assert_eq!(next.len(), 1);
        assert_eq!(
            next[0],
            utc(2026, 11, 2, 9, 0),
            "the next occurrence is strictly after now"
        );
    }

    /// Given a month-end expression (`0 0 31 * *`, the 31st at midnight),
    /// when the past is evaluated on a mid-month day, then the bounded walk
    /// reaches the previous month's 31st — month-length skips are handled by
    /// the same day walk the forward kernel uses.
    #[test]
    fn should_walk_back_across_short_months_when_evaluating_a_month_end_expression() {
        let prev = CronManager::prev_fire_for(
            &ScheduleSpec::Cron("0 0 31 * *"),
            Some("UTC"),
            utc(2026, 11, 15, 0, 0),
        );
        assert_eq!(prev, Some(utc(2026, 10, 31, 0, 0)));
        // December 31 → the previous 31st is October 31 (November has none).
        let prev = CronManager::prev_fire_for(
            &ScheduleSpec::Cron("0 0 31 * *"),
            Some("UTC"),
            utc(2026, 12, 31, 0, 0),
        );
        assert_eq!(prev, Some(utc(2026, 12, 31, 0, 0)), "now inclusive");
        let prev = CronManager::prev_fire_for(
            &ScheduleSpec::Cron("0 0 31 * *"),
            Some("UTC"),
            utc(2026, 12, 31, 0, 1),
        );
        assert_eq!(prev, Some(utc(2026, 12, 31, 0, 0)));
    }

    /// Given an expression whose slot falls into a DST gap on one day, when
    /// the past is evaluated after that day, then the past is the instant
    /// the forward kernel would have fired (the same `to_instant` mapping) —
    /// the past and the future never disagree about the same edge.
    #[test]
    fn should_return_the_same_instant_the_forward_kernel_fires_when_the_past_lands_in_a_dst_gap() {
        // New York 2026-03-08 02:30 → the gap; the forward kernel lands on
        // 03:30 local.
        let schedule = ScheduleSpec::Cron("30 2 * * *");
        let forward = CronManager::preview_fires(
            &schedule,
            Some("America/New_York"),
            1,
            utc(2026, 3, 7, 12, 0),
        );
        assert_eq!(forward.len(), 1);
        let prev =
            CronManager::prev_fire_for(&schedule, Some("America/New_York"), utc(2026, 3, 8, 12, 0));
        assert_eq!(
            prev,
            Some(forward[0]),
            "the gap day maps identically both ways"
        );
    }

    /// Given an every-1-month interval anchored on Jan 31 (the clamp rule:
    /// Feb 28 / Mar 31), when the past is evaluated in April, then the
    /// answer is Mar 31 — and it is EXACTLY the instant the forward walk
    /// produced, proving prev/next share the anchor+clamp+to_instant path.
    #[test]
    fn should_reuse_the_forward_walk_when_answering_the_past_of_a_monthly_interval() {
        let spec = interval_spec(1, IntervalUnit::Month, "09:00", "2026-01-31");
        let schedule = ScheduleSpec::Interval(&spec);
        let forward =
            CronManager::preview_fires(&schedule, Some("Asia/Tokyo"), 4, utc(2026, 1, 30, 0, 0));
        assert_eq!(forward.len(), 4, "{forward:?}");
        let now = utc(2026, 4, 15, 0, 0);
        let prev = CronManager::prev_fire_for(&schedule, Some("Asia/Tokyo"), now);
        // The last forward occurrence at or before now is the third one.
        let expected = forward.iter().copied().rfind(|fire| *fire <= now);
        assert_eq!(prev, expected);
        assert_eq!(
            prev.map(|at| at.with_timezone(&chrono_tz::Asia::Tokyo).date_naive()),
            Some(chrono::NaiveDate::from_ymd_opt(2026, 3, 31).unwrap()),
        );
    }

    /// Given an every-2-days interval, when now lands exactly on an
    /// occurrence, then the past is that occurrence itself (inclusive).
    #[test]
    fn should_include_the_current_occurrence_when_an_interval_now_lands_on_a_fire() {
        let spec = interval_spec(2, IntervalUnit::Day, "09:30", "2026-11-01");
        let schedule = ScheduleSpec::Interval(&spec);
        let now = utc(2026, 11, 5, 9, 30);
        assert_eq!(
            CronManager::prev_fire_for(&schedule, Some("UTC"), now),
            Some(now)
        );
        assert_eq!(
            CronManager::prev_fire_for(&schedule, Some("UTC"), utc(2026, 11, 5, 9, 29)),
            Some(utc(2026, 11, 3, 9, 30)),
        );
    }

    /// Given a weekly occurrence on the far side of a UTC day boundary for
    /// a +09:00 zone, when the past is evaluated, then it is the local
    /// weekday/time in that zone — the walk is on the LOCAL calendar, not
    /// the host's, so the timezone label on screen matches the instant.
    #[test]
    fn should_evaluate_the_past_on_the_declared_zone_calendar_not_utc() {
        // Monday 09:00 Tokyo = Sunday 00:00 UTC.
        let prev = CronManager::prev_fire_for(
            &ScheduleSpec::Cron("0 9 * * 1"),
            Some("Asia/Tokyo"),
            utc(2026, 10, 26, 3, 0), // Monday 12:00 Tokyo
        );
        let prev = prev.expect("last Monday exists");
        let local = prev.with_timezone(&chrono_tz::Asia::Tokyo);
        assert_eq!(
            local.date_naive(),
            chrono::NaiveDate::from_ymd_opt(2026, 10, 26).unwrap()
        );
        assert_eq!(
            local.time(),
            chrono::NaiveTime::from_hms_opt(9, 0, 0).unwrap()
        );
    }

    /// Given an expression that only ever fires in the future relative to
    /// now (an anchor before the first occurrence — the one-shot shape),
    /// when the past is evaluated, then the answer is `None` (no fabricated
    /// past instants; the one-shot rule's "—").
    #[test]
    fn should_answer_none_when_the_schedule_never_fired_before_now() {
        let spec = interval_spec(1, IntervalUnit::Month, "09:00", "2027-01-01");
        assert_eq!(
            CronManager::prev_fire_for(
                &ScheduleSpec::Interval(&spec),
                Some("UTC"),
                utc(2026, 12, 1, 0, 0),
            ),
            None
        );
        // The cron arm's end-bounded shape: `0 0 30 2 *` (Feb 30) can
        // NEVER fire — no fabricated past either.
        assert_eq!(
            CronManager::prev_fire_for(
                &ScheduleSpec::Cron("0 0 30 2 *"),
                Some("UTC"),
                utc(2026, 11, 1, 0, 0),
            ),
            None,
            "an expression that never fires has no theoretical past"
        );
    }

    /// Given an unusable schedule, when the past is evaluated, then `None`
    /// (the same honest empty answer `preview_fires` gives).
    #[test]
    fn should_answer_none_when_the_schedule_is_unusable() {
        assert_eq!(
            CronManager::prev_fire_for(
                &ScheduleSpec::Cron("not a cron"),
                Some("UTC"),
                utc(2026, 11, 1, 0, 0),
            ),
            None
        );
        assert_eq!(
            CronManager::prev_fire_for(
                &ScheduleSpec::Cron("0 9 * * *"),
                Some("Not/A_Zone"),
                utc(2026, 11, 1, 0, 0),
            ),
            None
        );
    }

    /// Given an unusable expression or timezone, when previewed, then the
    /// answer is the honest empty Vec (the preview face renders the empty
    /// state, never an error).
    #[test]
    fn should_answer_an_empty_vec_when_previewing_an_unusable_schedule() {
        assert!(
            CronManager::preview_fires(
                &ScheduleSpec::Cron("not a cron"),
                Some("UTC"),
                2,
                Utc::now(),
            )
            .is_empty()
        );
        assert!(
            CronManager::preview_fires(
                &ScheduleSpec::Cron("* * * * *"),
                Some("Not/A_Zone"),
                2,
                Utc::now(),
            )
            .is_empty()
        );
    }

    /// Given an interval with a bad shape, when validated at create,
    /// then the spec is rejected (a positive every, an HH:MM time).
    #[test]
    fn should_reject_a_malformed_interval_spec_at_validation() {
        assert!(
            interval_spec(0, IntervalUnit::Day, "09:30", "2026-11-01")
                .validate()
                .is_err()
        );
        assert!(
            interval_spec(2, IntervalUnit::Day, "9:30 pm", "2026-11-01")
                .validate()
                .is_err()
        );
        assert!(
            interval_spec(2, IntervalUnit::Day, "24:00", "2026-11-01")
                .validate()
                .is_err()
        );
    }

    /// Review Blocker 1: given an every-1-months interval anchored on Jan 31,
    /// when three consecutive steps walk, then the chain lands Feb 28, Mar 31,
    /// Apr 30 — each step one month from the ORIGINAL anchor (the
    /// clamped-month rule), never a skipped month.
    #[test]
    fn should_step_one_month_at_a_time_from_the_original_anchor_when_walking_months() {
        let spec = interval_spec(1, IntervalUnit::Month, "09:00", "2027-01-31");
        // The walk starts just after the anchor day's 09:00, so the first
        // occurrence is the anchor's first step (Feb).
        let fires = CronManager::preview_fires(
            &ScheduleSpec::Interval(&spec),
            Some("UTC"),
            3,
            utc(2027, 1, 31, 10, 0),
        );
        let days: Vec<chrono::NaiveDate> = fires
            .iter()
            .map(|fire| fire.with_timezone(&chrono_tz::UTC).date_naive())
            .collect();
        assert_eq!(
            days,
            vec![
                // Jan 31 + 1 month = Feb 28 (2027 is not a leap year — the
                // clamp), then the ORIGINAL day (31) returns in March and
                // clamps again in April.
                chrono::NaiveDate::from_ymd_opt(2027, 2, 28).unwrap(),
                chrono::NaiveDate::from_ymd_opt(2027, 3, 31).unwrap(),
                chrono::NaiveDate::from_ymd_opt(2027, 4, 30).unwrap(),
            ]
        );
    }

    /// Review Blocker 1: given an every-2-months interval, when the chain
    /// walks three steps, then it lands anchor+2, anchor+4, anchor+6 months —
    /// deriving each step from the previous one compounding to Jan, Mar, Jul
    /// (skipping May).
    #[test]
    fn should_count_every_two_months_from_the_anchor_not_the_previous_step() {
        let spec = interval_spec(2, IntervalUnit::Month, "09:00", "2027-01-15");
        let fires = CronManager::preview_fires(
            &ScheduleSpec::Interval(&spec),
            Some("UTC"),
            3,
            utc(2027, 1, 15, 10, 0),
        );
        let days: Vec<chrono::NaiveDate> = fires
            .iter()
            .map(|fire| fire.with_timezone(&chrono_tz::UTC).date_naive())
            .collect();
        assert_eq!(
            days,
            vec![
                chrono::NaiveDate::from_ymd_opt(2027, 3, 15).unwrap(),
                chrono::NaiveDate::from_ymd_opt(2027, 5, 15).unwrap(),
                chrono::NaiveDate::from_ymd_opt(2027, 7, 15).unwrap(),
            ]
        );
    }

    /// Review Major 2: given a backlog cursor behind a multi-occurrence gap
    /// and an `ends_at` inside that gap, when the coalesce walk recovers,
    /// then it counts and advances only the occurrences on or before the end
    /// date — never past it.
    #[test]
    fn should_stop_the_coalesce_walk_at_the_end_date_when_recovering_a_backlog() {
        let manager = CronManager::with_options(CronOptions {
            jitter: false,
            stale: false,
        });
        let mut task = task_base("cron-clamp", "0 9 * * *", "p");
        task.timezone = Some("UTC".into());
        // Daily 09:00, last fired four days ago at 09:00: three occurrences
        // are due (yesterday, two days, three days ago)... then the end date
        // sits one day before `now`, so the walk must count exactly the
        // three legal ones.
        let now = utc(2027, 6, 10, 12, 0);
        task.created_at = now - chrono::Duration::days(5);
        task.last_fired = Some(utc(2027, 6, 6, 9, 0));
        task.ends_at = Some(chrono::NaiveDate::from_ymd_opt(2027, 6, 9).unwrap());
        let plan = manager.plan_fire(&task, now);
        assert_eq!(plan.coalesced_count, 3, "only the legal occurrences count");
        assert_eq!(
            plan.advanced,
            utc(2027, 6, 9, 9, 0),
            "the cursor stops at the last legal day"
        );
        assert!(plan.ended, "the delivery past the end date is the finale");
        assert!(
            plan.next_fire_at.is_none(),
            "nothing schedules past the end"
        );
    }

    /// Review Major 2: given a cursor whose next occurrence falls exactly ON
    /// the end date, when the walk recovers, then that occurrence still
    /// counts (the end date is inclusive) and the task ends cleanly on it.
    #[test]
    fn should_count_the_occurrence_landing_on_the_end_date_when_coalescing() {
        let manager = CronManager::with_options(CronOptions {
            jitter: false,
            stale: false,
        });
        let mut task = task_base("cron-clamp-on", "0 9 * * *", "p");
        task.timezone = Some("UTC".into());
        let now = utc(2027, 6, 10, 12, 0);
        task.created_at = now - chrono::Duration::days(5);
        task.last_fired = Some(utc(2027, 6, 8, 9, 0));
        // June 10's 09:00 occurrence is due AND lands on the end date —
        // the inclusive last legal day.
        task.ends_at = Some(chrono::NaiveDate::from_ymd_opt(2027, 6, 10).unwrap());
        let plan = manager.plan_fire(&task, now);
        assert_eq!(plan.coalesced_count, 2);
        assert_eq!(plan.advanced, utc(2027, 6, 10, 9, 0));
        assert!(plan.ended, "the inclusive end-day occurrence is the finale");
    }

    // -----------------------------------------------------------------------
    // form batch 6: cron.add authoritative validations (T4, B1/M3)
    // -----------------------------------------------------------------------

    /// Given an ends_at strictly before the first fire's local day, when a
    /// task is created, then the create is rejected.
    #[tokio::test]
    async fn should_reject_the_create_when_ends_at_precedes_the_first_fire_day() {
        let manager = CronManager::with_options(CronOptions {
            jitter: false,
            stale: false,
        });
        let error = manager
            .add(
                "0 9 * * *".into(),
                "p".into(),
                "s".into(),
                true,
                CronCreateOptions {
                    timezone: Some("UTC".into()),
                    // The first fire is always after "today" — an end
                    // date of yesterday can never include it.
                    ends_at: Some((Utc::now() - chrono::Duration::days(2)).date_naive()),
                    ..CronCreateOptions::default()
                },
            )
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("结束日期不能早于首次运行日"),
            "the refusal names the rule: {error}"
        );
    }

    /// Given an ends_at equal to the first fire's local day, when a task
    /// is created, then it is legal (the one-shot spelling).
    #[tokio::test]
    async fn should_accept_the_create_when_ends_at_equals_the_first_fire_day() {
        let manager = CronManager::with_options(CronOptions {
            jitter: false,
            stale: false,
        });
        let tomorrow = (Utc::now() + chrono::Duration::days(1)).date_naive();
        let id = manager
            .add(
                "0 9 * * *".into(),
                "p".into(),
                "s".into(),
                true,
                CronCreateOptions {
                    timezone: Some("UTC".into()),
                    ends_at: Some(tomorrow),
                    ..CronCreateOptions::default()
                },
            )
            .await
            .unwrap();
        assert!(manager.get(&id).is_some());
    }

    /// Given both a cron expression and an interval on one create, when
    /// the task is created, then the pair is rejected (SDR-113).
    #[tokio::test]
    async fn should_reject_the_create_when_a_cron_expression_and_an_interval_both_ride() {
        let manager = CronManager::with_options(CronOptions {
            jitter: false,
            stale: false,
        });
        let error = manager
            .add(
                "0 9 * * *".into(),
                "p".into(),
                "s".into(),
                true,
                CronCreateOptions {
                    interval: Some(interval_spec(2, IntervalUnit::Day, "09:30", "2026-11-01")),
                    ..CronCreateOptions::default()
                },
            )
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("not both"),
            "the refusal names the mutual exclusion: {error}"
        );
    }

    // -----------------------------------------------------------------------
    // form batch 6: the ends_at scheduling enforcement (T5, B1 / SDR-110)
    // -----------------------------------------------------------------------

    /// Given a task carrying BOTH a max_fires budget and an end date, when
    /// each finale's turn comes, then they close independently (whichever
    /// arrives first wins; the other never fires again).
    #[test]
    fn should_close_the_task_through_either_finale_independently() {
        let manager = CronManager::with_options(CronOptions {
            jitter: false,
            stale: false,
        });
        let today = Utc::now().date_naive();
        let yesterday = today - chrono::Duration::days(1);
        // The max_fires verdict is computed on the plan's pre-fire count:
        // fire_count 2 against a budget of 3 means THIS delivery maxes out.
        let mut task = task_base("cron-both", "* * * * *", "p");
        task.timezone = Some("UTC".into());
        task.ends_at = Some(yesterday);
        task.max_fires = Some(3);
        task.fire_count = 2;
        task.created_at = Utc::now() - chrono::Duration::days(4);
        let plan = manager.plan_fire(&task, Utc::now());
        assert!(plan.maxed_out, "the budget verdict arrives");
        assert!(plan.ended, "the end-date verdict arrives too");
        assert!(
            plan.next_fire_at.is_none(),
            "nothing survives either finale"
        );
        // A budget NOT yet exhausted under an already-passed end date: the
        // end date alone closes it (the budget stays unconsumed).
        let mut unspent = task.clone();
        unspent.fire_count = 0;
        let plan = manager.plan_fire(&unspent, Utc::now());
        assert!(!plan.maxed_out, "the budget is untouched");
        assert!(plan.ended, "the end date still closes it");
    }

    /// Given a recurring task whose next occurrence falls past its end
    /// date, when the due pass plans its final fire, then the plan
    /// carries the `ended` mark and schedules no next occurrence — the
    /// task is removed after the finale (the max_fires finale's sibling).
    #[tokio::test]
    async fn should_remove_the_task_after_its_end_date_finale() {
        let manager = CronManager::with_options(CronOptions {
            jitter: false,
            stale: false,
        });
        let today = Utc::now().date_naive();
        let yesterday = today - chrono::Duration::days(1);
        let mut task = task_base("cron-ended", "0 9 * * *", "p");
        task.timezone = Some("UTC".into());
        // The end date is yesterday: today's occurrence (the next one)
        // already falls past it, and yesterday's (a due occurrence the
        // coalesce walk can still count, the inclusive-last-day rule)
        // was the task's final legal day.
        task.ends_at = Some(yesterday);
        task.created_at = Utc::now() - chrono::Duration::days(4);
        task.last_fired = Some(utc(
            yesterday.year(),
            yesterday.month(),
            yesterday.day(),
            0,
            0,
        ));
        let plan = manager.plan_fire(&task, Utc::now());
        // The next occurrence is today's 09:00 — past the end date: the
        // plan's delivery is the finale (carrying yesterday's due
        // occurrence counts, so the last legal day was delivered).
        assert!(
            plan.ended,
            "the finale is marked: next={:?} ends={yesterday}",
            plan.next_fire_at
        );
        assert!(
            plan.next_fire_at.is_none(),
            "nothing is scheduled past the end"
        );
        // And an occurrence after the end date never passes the clamp.
        assert!(
            occurrence_past_ends_at(
                &task,
                utc(today.year(), today.month(), today.day(), 9, 0),
                chrono_tz::UTC
            ),
            "an occurrence after the end date never passes the clamp"
        );
    }

    /// Given a recurring task whose next occurrence falls past its end
    /// date, when the day passes without a due pass, then the restore
    /// path leaves it dormant (it never fires again but stays listed
    /// until its next mutation).
    #[test]
    fn should_restore_dormant_when_the_schedule_already_ran_past_the_end_date() {
        let manager = CronManager::with_options(CronOptions {
            jitter: false,
            stale: false,
        });
        let mut task = task_base("cron-ended", "0 9 * * *", "p");
        task.timezone = Some("UTC".into());
        task.ends_at = Some(chrono::NaiveDate::from_ymd_opt(2026, 1, 1).unwrap());
        task.created_at = utc(2026, 2, 1, 0, 0);
        manager.restore_tasks([task]);
        let restored = manager.get("cron-ended").expect("the task stays listed");
        assert!(
            restored.next_fire_at.is_none(),
            "an expired schedule never reschedules"
        );
    }

    /// batch 2: `cron_task_base` (the shared test/record fixture).
    /// Given a base record, when its wire form round-trips, then the new
    /// batch-2 keys (`permissionMode` / `paused`) survive and a pre-batch-2
    /// record without them reads back as running with the mode inferred
    /// from its legacy posture.
    #[test]
    fn should_round_trip_the_mode_and_pause_keys_when_converting_a_task_to_wire() {
        let mut task = task_base("cron-b2", "0 9 * * *", "p");
        let wire = cron_task_wire(&task);
        assert!(wire.get("permissionMode").is_none());
        assert!(wire.get("paused").is_none());
        task.permission_mode = Some(PermissionMode::Yolo);
        task.paused = true;
        let wire = cron_task_wire(&task);
        assert_eq!(wire["permissionMode"], "yolo");
        assert_eq!(wire["paused"], true);
        let parsed = cron_task_from_wire(&wire).expect("the wire parses back");
        assert_eq!(parsed.permission_mode, Some(PermissionMode::Yolo));
        assert!(parsed.paused);
        // A legacy record: posture only, no mode / pause keys.
        let mut legacy = wire.clone();
        let object = legacy.as_object_mut().unwrap();
        object.remove("permissionMode");
        object.remove("paused");
        object.insert("permissionPosture".into(), serde_json::json!("auto"));
        let parsed = cron_task_from_wire(&legacy).expect("the legacy wire parses");
        assert_eq!(parsed.permission_mode, Some(PermissionMode::Auto));
        assert!(!parsed.paused);
    }

    // -----------------------------------------------------------------------
    // batch 2 (A4): the three session modes expand to three postures
    // -----------------------------------------------------------------------

    /// Given the three Session mode names, when expanded, then each maps
    /// to its OWN posture — no folding (A4: 选了什么就是什么). The fourth
    /// legacy option has no mode name any more.
    #[test]
    fn should_expand_each_of_the_three_modes_to_its_own_posture() {
        assert_eq!(PermissionMode::Manual.posture(), PermissionPosture::Manual);
        assert_eq!(PermissionMode::Auto.posture(), PermissionPosture::Auto);
        assert_eq!(PermissionMode::Yolo.posture(), PermissionPosture::Yolo);
        // The wire spelling is the session's own.
        assert_eq!(
            serde_json::to_value(PermissionMode::Yolo).unwrap(),
            serde_json::json!("yolo")
        );
        assert_eq!(
            serde_json::from_value::<PermissionMode>(serde_json::json!("manual")).unwrap(),
            PermissionMode::Manual
        );
        assert!(
            serde_json::from_value::<PermissionMode>(serde_json::json!("readonly")).is_err(),
            "the retired fourth option is not a mode"
        );
    }

    #[test]
    fn should_return_next_minute_boundary_when_compute_next_fire_with_every_minute_cron() {
        let from = utc(2025, 1, 15, 10, 30).with_second(45).unwrap();

        let next = CronManager::compute_next_fire("* * * * *", from, chrono_tz::UTC).unwrap();

        assert_eq!(next, utc(2025, 1, 15, 10, 31));
        assert!(next > from);
    }

    #[test]
    fn should_not_fire_at_current_minute_when_compute_next_fire_from_exact_match_at_minute_boundary()
     {
        // `from` sits exactly on a matching minute: the next fire must be the
        // following occurrence, never the current one.
        let from = utc(2025, 1, 15, 10, 30);

        let next = CronManager::compute_next_fire("* * * * *", from, chrono_tz::UTC).unwrap();

        assert_eq!(next, utc(2025, 1, 15, 10, 31));
    }

    #[test]
    fn should_return_matching_minute_when_compute_next_fire_with_specific_minute_cron() {
        let from = utc(2025, 1, 15, 9, 45);

        let next = CronManager::compute_next_fire("30 10 * * *", from, chrono_tz::UTC).unwrap();

        assert_eq!(next, utc(2025, 1, 15, 10, 30));
    }

    #[test]
    fn should_return_tomorrow_when_compute_next_fire_after_specific_minute_already_passed_today() {
        let from = utc(2025, 1, 15, 10, 31);

        let next = CronManager::compute_next_fire("30 10 * * *", from, chrono_tz::UTC).unwrap();

        assert_eq!(next, utc(2025, 1, 16, 10, 30));
    }

    #[test]
    fn should_return_next_value_in_range_when_compute_next_fire_with_range_cron() {
        let from = utc(2025, 1, 15, 17, 0);

        let next = CronManager::compute_next_fire("0 9-17 * * *", from, chrono_tz::UTC).unwrap();

        assert_eq!(next, utc(2025, 1, 16, 9, 0));
    }

    #[test]
    fn should_return_next_list_value_when_compute_next_fire_with_list_cron() {
        let from = utc(2025, 1, 15, 10, 31);

        let next =
            CronManager::compute_next_fire("0,15,30,45 * * * *", from, chrono_tz::UTC).unwrap();

        assert_eq!(next, utc(2025, 1, 15, 10, 45));
    }

    #[test]
    fn should_return_next_step_value_when_compute_next_fire_with_wildcard_step_cron() {
        let from = utc(2025, 1, 15, 10, 32);

        let next = CronManager::compute_next_fire("*/5 * * * *", from, chrono_tz::UTC).unwrap();

        assert_eq!(next, utc(2025, 1, 15, 10, 35));
    }

    #[test]
    fn should_step_to_field_maximum_when_compute_next_fire_with_value_step_cron() {
        // `5/15` means minutes 5, 20, 35, 50.
        let from = utc(2025, 1, 15, 10, 0);

        let next = CronManager::compute_next_fire("5/15 * * * *", from, chrono_tz::UTC).unwrap();

        assert_eq!(next, utc(2025, 1, 15, 10, 5));
    }

    #[test]
    fn should_respect_step_when_compute_next_fire_with_range_step_cron() {
        let from = utc(2025, 1, 15, 10, 0);

        let next = CronManager::compute_next_fire("1-10/3 * * * *", from, chrono_tz::UTC).unwrap();

        assert_eq!(next, utc(2025, 1, 15, 10, 1));
    }

    #[test]
    fn should_prefer_later_hour_in_same_day_over_tomorrow_when_compute_next_fire() {
        // 10:45 has passed; the next fire must be 11:30 today, not tomorrow.
        let from = utc(2025, 1, 15, 10, 45);

        let next = CronManager::compute_next_fire("30 10,11 * * *", from, chrono_tz::UTC).unwrap();

        assert_eq!(next, utc(2025, 1, 15, 11, 30));
    }

    #[test]
    fn should_return_next_matching_weekday_when_compute_next_fire_with_day_of_week_cron() {
        // 2025-01-17 is a Friday; the next Monday is 2025-01-20.
        let from = utc(2025, 1, 17, 10, 0);

        let next = CronManager::compute_next_fire("0 9 * * 1", from, chrono_tz::UTC).unwrap();

        assert_eq!(next, utc(2025, 1, 20, 9, 0));
    }

    #[test]
    fn should_match_sunday_when_compute_next_fire_with_sunday_alias_seven() {
        // 2025-01-17 is a Friday; the next Sunday is 2025-01-19.
        let from = utc(2025, 1, 17, 10, 0);

        let next = CronManager::compute_next_fire("0 0 * * 7", from, chrono_tz::UTC).unwrap();

        assert_eq!(next, utc(2025, 1, 19, 0, 0));
    }

    #[test]
    fn should_match_sunday_when_compute_next_fire_with_sunday_alias_zero() {
        // 2025-01-20 is a Monday; the next Sunday is 2025-01-26.
        let from = utc(2025, 1, 20, 10, 0);

        let next = CronManager::compute_next_fire("0 0 * * 0", from, chrono_tz::UTC).unwrap();

        assert_eq!(next, utc(2025, 1, 26, 0, 0));
    }

    #[test]
    fn should_use_or_semantics_when_compute_next_fire_with_restricted_dom_and_dow() {
        // `0 0 13 * 5`: fires on the 13th OR any Friday. From Thursday
        // 2025-01-16, the first match is Friday 2025-01-17.
        let from = utc(2025, 1, 16, 12, 0);

        let next = CronManager::compute_next_fire("0 0 13 * 5", from, chrono_tz::UTC).unwrap();

        assert_eq!(next, utc(2025, 1, 17, 0, 0));
    }

    #[test]
    fn should_match_dom_first_when_compute_next_fire_with_restricted_dom_and_dow_and_earlier_dom() {
        // From Friday 2025-01-10, the 13th (Monday 2025-01-13) comes before
        // the next Friday.
        let from = utc(2025, 1, 10, 0, 1);

        let next = CronManager::compute_next_fire("0 0 13 * 5", from, chrono_tz::UTC).unwrap();

        assert_eq!(next, utc(2025, 1, 13, 0, 0));
    }

    #[test]
    fn should_skip_other_months_when_compute_next_fire_with_restricted_month_cron() {
        let from = utc(2025, 1, 15, 0, 0);

        let next = CronManager::compute_next_fire("0 0 1 3 *", from, chrono_tz::UTC).unwrap();

        assert_eq!(next, utc(2025, 3, 1, 0, 0));
    }

    #[test]
    fn should_skip_non_leap_years_when_compute_next_fire_with_leap_day_cron() {
        let from = utc(2024, 3, 1, 0, 0);

        let next = CronManager::compute_next_fire("0 0 29 2 *", from, chrono_tz::UTC).unwrap();

        assert_eq!(next, utc(2028, 2, 29, 0, 0));
    }

    #[test]
    fn should_advance_date_when_compute_next_fire_at_midnight_rollover() {
        let from = utc(2025, 1, 31, 23, 59);

        let next = CronManager::compute_next_fire("0 0 * * *", from, chrono_tz::UTC).unwrap();

        assert_eq!(next, utc(2025, 2, 1, 0, 0));
    }

    // -----------------------------------------------------------------------
    // compute_next_fire — timezone-aware evaluation (B5 / DEC-3)
    // -----------------------------------------------------------------------

    fn sh() -> chrono_tz::Tz {
        chrono_tz::Asia::Shanghai
    }
    fn ny() -> chrono_tz::Tz {
        chrono_tz::America::New_York
    }

    #[test]
    fn should_fire_at_nine_local_when_compute_next_fire_with_shanghai_timezone() {
        // Given 2025-01-15 01:00 UTC == 09:00 in Shanghai, and "0 9 * * *"
        // (9am Shanghai every day), when computed from just after 9am local
        let from = chrono::Utc.with_ymd_and_hms(2025, 1, 15, 1, 0, 30).unwrap();

        let next = CronManager::compute_next_fire("0 9 * * *", from, sh()).unwrap();

        // Then the next fire is 9am Shanghai the following day == 01:00 UTC
        assert_eq!(
            next,
            chrono::Utc.with_ymd_and_hms(2025, 1, 16, 1, 0, 0).unwrap()
        );
    }

    #[test]
    fn should_evaluate_dom_and_dow_in_local_calendar_when_compute_next_fire_with_timezone() {
        // Given 2025-03-14 21:00 UTC == 2025-03-15 05:00 Shanghai (Saturday),
        // and "0 9 * * 1" (9am local on Mondays)
        let from = chrono::Utc.with_ymd_and_hms(2025, 3, 14, 21, 0, 0).unwrap();

        let next = CronManager::compute_next_fire("0 9 * * 1", from, sh()).unwrap();

        // Then the next Monday 9am Shanghai is 2025-03-17 01:00 UTC
        assert_eq!(
            next,
            chrono::Utc.with_ymd_and_hms(2025, 3, 17, 1, 0, 0).unwrap()
        );
    }

    #[test]
    fn should_push_a_gap_local_time_forward_when_compute_next_fire_across_dst_spring_forward() {
        // America/New_York springs forward 2025-03-09: 02:00 EST does not exist
        // (jumps to 03:00 EDT). Given "0 2 * * *" (2am local every day) from
        // 2025-03-08 12:00 UTC, when computed
        let from = chrono::Utc.with_ymd_and_hms(2025, 3, 8, 12, 0, 0).unwrap();

        let next = CronManager::compute_next_fire("0 2 * * *", from, ny()).unwrap();

        // Then 2025-03-09 02:00 local is interpreted with the pre-gap offset
        // (EST, UTC-5) — the JS Date normalization — landing at 07:00 UTC
        // (== 03:00 EDT wall clock).
        assert_eq!(
            next,
            chrono::Utc.with_ymd_and_hms(2025, 3, 9, 7, 0, 0).unwrap()
        );
    }

    #[test]
    fn should_resolve_a_deep_gap_minute_with_the_pre_gap_offset_when_compute_next_fire() {
        // A candidate deeper than the gap's first minute (02:30 on the
        // 2025-03-09 2->3 New York jump) must resolve with the pre-gap
        // offset too — the whole gap normalizes the JS Date way; it is
        // never skipped to the following day.
        let from = chrono::Utc.with_ymd_and_hms(2025, 3, 8, 12, 0, 0).unwrap();

        let next = CronManager::compute_next_fire("30 2 * * *", from, ny()).unwrap();

        // 02:30 local does not exist; interpreted with EST (UTC-5) it lands
        // at 07:30 UTC == 03:30 EDT wall clock, still on 2025-03-09.
        assert_eq!(
            next,
            chrono::Utc.with_ymd_and_hms(2025, 3, 9, 7, 30, 0).unwrap()
        );
    }

    #[test]
    fn should_pick_the_earliest_instant_when_compute_next_fire_across_dst_fall_back() {
        // America/New_York falls back 2025-11-02: 01:30 local happens twice.
        // Given "30 1 * * *" from 2025-11-01 12:00 UTC, when computed
        let from = chrono::Utc.with_ymd_and_hms(2025, 11, 1, 12, 0, 0).unwrap();

        let next = CronManager::compute_next_fire("30 1 * * *", from, ny()).unwrap();

        // Then the ambiguous 01:30 resolves to the earliest instant (EDT,
        // UTC-4) == 05:30 UTC, matching JS Date.
        assert_eq!(
            next,
            chrono::Utc.with_ymd_and_hms(2025, 11, 2, 5, 30, 0).unwrap()
        );
    }

    #[test]
    fn should_keep_utc_results_when_compute_next_fire_with_utc_timezone() {
        // The UTC escape hatch preserves the pre-B5 semantics exactly.
        let from = chrono::Utc
            .with_ymd_and_hms(2025, 1, 15, 10, 31, 0)
            .unwrap();
        let next = CronManager::compute_next_fire("30 10 * * *", from, chrono_tz::UTC).unwrap();
        assert_eq!(
            next,
            chrono::Utc
                .with_ymd_and_hms(2025, 1, 16, 10, 30, 0)
                .unwrap()
        );
    }

    // -----------------------------------------------------------------------
    // parse errors
    // -----------------------------------------------------------------------

    #[test]
    fn should_return_invalid_expression_when_compute_next_fire_with_too_few_fields() {
        let error =
            CronManager::compute_next_fire("0 0 * *", utc(2025, 1, 15, 0, 0), chrono_tz::UTC)
                .unwrap_err();
        assert!(
            matches!(error, CronError::InvalidExpression(message) if message.contains("5 fields"))
        );
    }

    #[test]
    fn should_return_invalid_expression_when_compute_next_fire_with_out_of_range_field_values() {
        assert!(
            CronManager::compute_next_fire("60 * * * *", utc(2025, 1, 15, 0, 0), chrono_tz::UTC)
                .is_err()
        );
        assert!(
            CronManager::compute_next_fire("* 24 * * *", utc(2025, 1, 15, 0, 0), chrono_tz::UTC)
                .is_err()
        );
        assert!(
            CronManager::compute_next_fire("* * 0 * *", utc(2025, 1, 15, 0, 0), chrono_tz::UTC)
                .is_err()
        );
        assert!(
            CronManager::compute_next_fire("* * * 13 *", utc(2025, 1, 15, 0, 0), chrono_tz::UTC)
                .is_err()
        );
        assert!(
            CronManager::compute_next_fire("* * * * 8", utc(2025, 1, 15, 0, 0), chrono_tz::UTC)
                .is_err()
        );
    }

    #[test]
    fn should_return_invalid_expression_when_compute_next_fire_with_non_numeric_field() {
        let error =
            CronManager::compute_next_fire("mon * * * *", utc(2025, 1, 15, 0, 0), chrono_tz::UTC)
                .unwrap_err();
        assert!(
            matches!(error, CronError::InvalidExpression(message) if message.contains("minute"))
        );
    }

    #[test]
    fn should_return_invalid_expression_when_compute_next_fire_with_zero_step() {
        let error =
            CronManager::compute_next_fire("*/0 * * * *", utc(2025, 1, 15, 0, 0), chrono_tz::UTC)
                .unwrap_err();
        assert!(matches!(error, CronError::InvalidExpression(message) if message.contains("step")));
    }

    #[test]
    fn should_return_invalid_expression_when_compute_next_fire_with_inverted_range() {
        assert!(
            CronManager::compute_next_fire("10-1 * * * *", utc(2025, 1, 15, 0, 0), chrono_tz::UTC)
                .is_err()
        );
    }

    #[test]
    fn should_return_no_next_fire_when_compute_next_fire_with_never_firing_cron() {
        // February never has a 30th.
        let error =
            CronManager::compute_next_fire("0 0 30 2 *", utc(2025, 1, 15, 0, 0), chrono_tz::UTC)
                .unwrap_err();
        assert!(matches!(error, CronError::NoNextFire(_)));
    }

    // -----------------------------------------------------------------------
    // CronManager behavior
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn should_list_task_with_future_next_fire_when_added() {
        let manager = CronManager::new();

        let id = manager
            .add(
                "0 9 * * *".into(),
                "morning briefing".into(),
                "session-1".into(),
                true,
                CronCreateOptions::default(),
            )
            .await
            .unwrap();

        let task = manager.get(&id).expect("task should be registered");
        assert!(task.id.starts_with("cron-"));
        assert_eq!(task.session_id, "session-1");
        assert!(task.recurring);
        assert!(task.last_fired.is_none());
        let next = task
            .next_fire_at
            .expect("next fire is always set after add");
        assert!(next > Utc::now());
        assert_eq!(manager.list(), vec![task]);
    }

    #[tokio::test]
    async fn should_return_error_and_register_nothing_when_adding_invalid_expression() {
        let manager = CronManager::new();

        let result = manager
            .add(
                "not a cron".into(),
                "boom".into(),
                "session-1".into(),
                true,
                CronCreateOptions::default(),
            )
            .await;

        assert!(result.is_err());
        assert!(manager.list().is_empty());
    }

    #[tokio::test]
    async fn should_remove_task_and_report_success_when_removing_registered_task() {
        let manager = CronManager::new();
        let id = manager
            .add(
                "* * * * *".into(),
                "tick".into(),
                "session-1".into(),
                true,
                CronCreateOptions::default(),
            )
            .await
            .unwrap();

        assert!(manager.remove(&id).await);
        assert!(!manager.remove(&id).await);
        assert!(manager.get(&id).is_none());
    }

    // -----------------------------------------------------------------------
    // held fires (C.4, stage-1 batch-2)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn should_merge_holds_by_task_id_when_hold_fire_is_called_twice() {
        // Given a recording manager with one task
        let (manager, _tmp) = recording_manager().await;
        // When the same task's fire is held twice while busy
        manager
            .hold_fire(CronFire {
                task_id: "cron-1".into(),
                prompt: "tick".into(),
                cron: "* * * * *".into(),
                recurring: true,
                coalesced_count: 2,
                stale: false,
                maxed_out: false,
                readonly: false,
                scheduled_at: None,
                late_by_ms: None,
                scope: TaskScope::Session,
                ended: false,
            })
            .await;
        manager
            .hold_fire(CronFire {
                task_id: "cron-1".into(),
                prompt: "tick".into(),
                cron: "* * * * *".into(),
                recurring: true,
                coalesced_count: 3,
                stale: false,
                maxed_out: false,
                readonly: false,
                scheduled_at: None,
                late_by_ms: None,
                scope: TaskScope::Session,
                ended: false,
            })
            .await;

        // Then one merged entry with the counts added is held
        let held = manager.held_fires();
        assert_eq!(held.len(), 1);
        assert_eq!(held[0].coalesced_count, 5);
    }

    #[tokio::test]
    async fn should_persist_the_clear_when_clear_held_is_called() {
        // Given a manager holding one fire
        let (manager, tmp) = recording_manager().await;
        manager
            .hold_fire(CronFire {
                task_id: "cron-1".into(),
                prompt: "tick".into(),
                cron: "* * * * *".into(),
                recurring: true,
                coalesced_count: 1,
                stale: false,
                maxed_out: false,
                readonly: false,
                scheduled_at: None,
                late_by_ms: None,
                scope: TaskScope::Session,
                ended: false,
            })
            .await;

        // When the held fire is cleared after its injection
        manager.clear_held("cron-1").await;

        // Then no fire is held anymore and the clear is durable
        assert!(manager.held_fires().is_empty());
        let ops = recorded_ops(&tmp).await;
        assert!(
            ops.iter()
                .any(|op| matches!(op, WireOp::CronHeldClear { id } if id == "cron-1")),
            "held clear is persisted: {ops:?}"
        );
    }

    #[tokio::test]
    async fn should_purge_a_held_fire_when_its_task_is_delete_requested() {
        // #177's held-side translation: a delete during the busy hold
        // cancels the pending injection.
        let (manager, _tmp) = recording_manager().await;
        manager
            .hold_fire(CronFire {
                task_id: "cron-1".into(),
                prompt: "tick".into(),
                cron: "* * * * *".into(),
                recurring: true,
                coalesced_count: 1,
                stale: false,
                maxed_out: false,
                readonly: false,
                scheduled_at: None,
                late_by_ms: None,
                scope: TaskScope::Session,
                ended: false,
            })
            .await;
        manager
            .hold_fire(CronFire {
                task_id: "cron-2".into(),
                prompt: "tock".into(),
                cron: "* * * * *".into(),
                recurring: true,
                coalesced_count: 1,
                stale: false,
                maxed_out: false,
                readonly: false,
                scheduled_at: None,
                late_by_ms: None,
                scope: TaskScope::Session,
                ended: false,
            })
            .await;
        // The task does not exist in the table — remove still records the
        // delete request (the mark, not the row, drives the purge).
        assert!(!manager.remove("cron-1").await, "marks the delete request");

        // When the purge runs
        manager.purge_deleted_held().await;

        // Then only the undeleted task's fire stays held
        let held = manager.held_fires();
        assert_eq!(held.len(), 1);
        assert_eq!(held[0].task_id, "cron-2");
    }

    #[tokio::test]
    async fn should_not_purge_a_held_fire_when_no_delete_was_requested() {
        let (manager, _tmp) = recording_manager().await;
        manager
            .hold_fire(CronFire {
                task_id: "cron-1".into(),
                prompt: "tick".into(),
                cron: "* * * * *".into(),
                recurring: true,
                coalesced_count: 1,
                stale: false,
                maxed_out: false,
                readonly: false,
                scheduled_at: None,
                late_by_ms: None,
                scope: TaskScope::Session,
                ended: false,
            })
            .await;

        manager.purge_deleted_held().await;

        assert_eq!(manager.held_fires().len(), 1, "no delete, no purge");
    }

    #[test]
    fn should_restore_held_fires_into_a_fresh_manager_when_restore_held_is_called() {
        // C.4's redelivery base: a resume rebuilds the held table from the
        // folded wire state.
        let manager = CronManager::new();
        manager.restore_held([CronFire {
            task_id: "cron-1".into(),
            prompt: "tick".into(),
            cron: "* * * * *".into(),
            recurring: true,
            coalesced_count: 1,
            stale: false,
            maxed_out: false,
            readonly: false,
            scheduled_at: None,
            late_by_ms: None,
            scope: TaskScope::Session,
            ended: false,
        }]);

        let held = manager.held_fires();
        assert_eq!(held.len(), 1);
        assert_eq!(held[0].task_id, "cron-1");
    }

    #[tokio::test]
    async fn should_record_the_held_add_op_to_the_wire_log_when_hold_fire_is_called() {
        let (manager, tmp) = recording_manager().await;

        manager
            .hold_fire(CronFire {
                task_id: "cron-1".into(),
                prompt: "tick".into(),
                cron: "* * * * *".into(),
                recurring: true,
                coalesced_count: 1,
                stale: false,
                maxed_out: false,
                readonly: false,
                scheduled_at: None,
                late_by_ms: None,
                scope: TaskScope::Session,
                ended: false,
            })
            .await;

        let ops = recorded_ops(&tmp).await;
        assert!(
            ops.iter()
                .any(|op| matches!(op, WireOp::CronHeldAdd { .. })),
            "held add is persisted: {ops:?}"
        );
    }

    #[tokio::test]
    async fn should_record_the_merged_snapshot_in_the_held_add_op_when_two_fires_are_held() {
        // M1: the op that lands on the wire log must already carry the
        // MERGED result (the reducer's fold rebuilds the same entry), and
        // it must land with the memory upsert — the log is never behind
        // memory on a merged hold (a crash there must leave the log ahead,
        // never the fire lost).
        let (manager, tmp) = recording_manager().await;
        let scheduled = Utc::now() - Duration::minutes(2);

        manager
            .hold_fire(CronFire {
                task_id: "cron-merge".into(),
                prompt: "tick".into(),
                cron: "* * * * *".into(),
                recurring: true,
                coalesced_count: 2,
                stale: false,
                maxed_out: false,
                readonly: false,
                scheduled_at: Some(scheduled),
                late_by_ms: None,
                scope: TaskScope::Session,
                ended: false,
            })
            .await;
        manager
            .hold_fire(CronFire {
                task_id: "cron-merge".into(),
                prompt: "tick".into(),
                cron: "* * * * *".into(),
                recurring: true,
                coalesced_count: 3,
                stale: true,
                maxed_out: false,
                readonly: false,
                // A run-now fire: no scheduled_at — must NOT erase the
                // natural hold's anchor (m1).
                scheduled_at: None,
                late_by_ms: None,
                scope: TaskScope::Session,
                ended: false,
            })
            .await;

        // Then the LAST held-add op on the log carries the full merge:
        // counts added, the incoming stale state, and the
        // natural fire's scheduled_at preserved.
        let ops = recorded_ops(&tmp).await;
        let merged = ops
            .iter()
            .rev()
            .find_map(|op| match op {
                WireOp::CronHeldAdd { task } => Some(task.clone()),
                _ => None,
            })
            .expect("the second hold must record a held-add op");
        assert_eq!(merged["task_id"], "cron-merge");
        assert_eq!(merged["coalesced_count"], 5, "counts add: {merged}");
        assert_eq!(merged["stale"], true, "incoming stale wins: {merged}");
        let recorded_scheduled = merged["scheduled_at"].as_str().expect("anchor kept");
        let parsed: DateTime<Utc> = serde_json::from_value(merged["scheduled_at"].clone())
            .expect("the anchor stays a parseable timestamp");
        assert_eq!(
            parsed, scheduled,
            "a run-now hold never erases the natural fire's anchor (m1): {merged} ({recorded_scheduled})"
        );

        // And the in-memory table matches the persisted snapshot (the
        // post-persist upsert landed the same merged entry).
        let held = manager.held_fires();
        assert_eq!(held.len(), 1, "same-task holds merge in memory too");
        assert_eq!(held[0].coalesced_count, 5);
        assert_eq!(held[0].scheduled_at, Some(scheduled));
    }

    #[tokio::test]
    async fn should_fire_and_reschedule_when_check_due_with_recurring_due_task() {
        let manager = CronManager::new();
        let id = manager
            .add(
                "* * * * *".into(),
                "tick".into(),
                "session-1".into(),
                true,
                CronCreateOptions::default(),
            )
            .await
            .unwrap();
        // White-box: force the task due by backdating its next fire time.
        {
            let mut tasks = manager.tasks.lock().unwrap();
            let task = tasks.get_mut(&id).unwrap();
            let backdated = Utc::now() - Duration::seconds(5);
            task.next_fire_at = Some(backdated);
        }

        let due = manager.check_due().await;

        // The white-box backdated next_fire_at rides the fire as its
        // scheduled moment (C.2a).
        let mut expected = single_fire(&id, "tick", "* * * * *", true);
        expected.scheduled_at = due[0].scheduled_at;
        assert_eq!(
            due,
            vec![expected],
            "an on-time recurring fire coalesces nothing and is not stale"
        );
        let task = manager.get(&id).expect("recurring task survives its fire");
        assert!(task.last_fired.is_some());
        let next = task.next_fire_at.expect("recurring task is rescheduled");
        assert!(next > Utc::now());
    }

    #[tokio::test]
    async fn should_fire_and_remove_task_when_check_due_with_one_shot_due_task() {
        let manager = CronManager::new();
        let id = manager
            .add(
                "* * * * *".into(),
                "once".into(),
                "session-1".into(),
                false,
                CronCreateOptions::default(),
            )
            .await
            .unwrap();
        {
            let mut tasks = manager.tasks.lock().unwrap();
            let backdated = Utc::now() - Duration::seconds(5);
            tasks.get_mut(&id).unwrap().next_fire_at = Some(backdated);
        }

        let due = manager.check_due().await;

        // The white-box backdated next_fire_at rides the fire as its
        // scheduled moment (C.2a).
        let mut expected = single_fire(&id, "once", "* * * * *", false);
        expected.scheduled_at = due[0].scheduled_at;
        assert_eq!(due, vec![expected], "a one-shot fires once, never stale");
        assert!(
            manager.get(&id).is_none(),
            "one-shot task is removed after firing"
        );
    }

    #[tokio::test]
    async fn should_return_empty_when_check_due_with_no_due_tasks() {
        let manager = CronManager::new();
        manager
            .add(
                "* * * * *".into(),
                "tick".into(),
                "session-1".into(),
                true,
                CronCreateOptions::default(),
            )
            .await
            .unwrap();

        assert!(manager.check_due().await.is_empty());
    }

    #[tokio::test]
    async fn should_never_fire_when_check_due_after_shutdown() {
        let manager = CronManager::new();
        let id = manager
            .add(
                "* * * * *".into(),
                "tick".into(),
                "session-1".into(),
                true,
                CronCreateOptions::default(),
            )
            .await
            .unwrap();
        {
            let mut tasks = manager.tasks.lock().unwrap();
            let backdated = Utc::now() - Duration::seconds(5);
            tasks.get_mut(&id).unwrap().next_fire_at = Some(backdated);
        }

        manager.shutdown();

        assert!(manager.check_due().await.is_empty());
        assert!(
            manager.get(&id).is_some(),
            "shutdown leaves tasks queryable"
        );
    }

    #[tokio::test]
    async fn should_return_sorted_by_id_when_check_due_with_multiple_due_tasks() {
        let manager = CronManager::new();
        // Both tasks fire now (backdated) and must be reported in id order.
        let first_id = manager
            .add(
                "* * * * *".into(),
                "b".into(),
                "session-1".into(),
                false,
                CronCreateOptions::default(),
            )
            .await
            .unwrap();
        let second_id = manager
            .add(
                "* * * * *".into(),
                "a".into(),
                "session-1".into(),
                false,
                CronCreateOptions::default(),
            )
            .await
            .unwrap();
        let now = Utc::now();
        for id in [&first_id, &second_id] {
            let mut tasks = manager.tasks.lock().unwrap();
            tasks.get_mut(id).unwrap().next_fire_at = Some(now - Duration::seconds(5));
        }

        let due = manager.check_due().await;

        let mut expected = vec![
            single_fire(&first_id, "b", "* * * * *", false),
            single_fire(&second_id, "a", "* * * * *", false),
        ];
        // The white-box backdated next_fire_at rides each fire as its
        // scheduled moment (C.2a) — copied from the actual fires, which the
        // sort order below still fully determines.
        for (fire, expectation) in due.iter().zip(expected.iter_mut()) {
            expectation.scheduled_at = fire.scheduled_at;
        }
        expected.sort_by(|left, right| left.task_id.cmp(&right.task_id));
        assert_eq!(due, expected);
    }

    // -----------------------------------------------------------------------
    // add/restore: timezone + jitter scheduling (B5 T5)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn should_store_the_explicit_timezone_when_adding_a_task_with_one() {
        // Jitter off: this test pins the *calendar* (9am Shanghai); the
        // deterministic per-id shift is covered by the jitter tests below.
        let manager = CronManager::with_options(CronOptions {
            jitter: false,
            stale: true,
        });

        let id = manager
            .add(
                "0 9 * * *".into(),
                "briefing".into(),
                "s".into(),
                true,
                CronCreateOptions {
                    timezone: Some("Asia/Shanghai".to_string()),
                    ..CronCreateOptions::default()
                },
            )
            .await
            .unwrap();

        let task = manager.get(&id).unwrap();
        assert_eq!(task.timezone.as_deref(), Some("Asia/Shanghai"));
        // next fire is 9am Shanghai = 01:00 UTC on some future day
        let next = task.next_fire_at.unwrap();
        let sh = chrono_tz::Asia::Shanghai;
        let local = next.with_timezone(&sh);
        assert_eq!((local.hour(), local.minute()), (9, 0));
    }

    #[tokio::test]
    async fn should_reject_an_unknown_timezone_when_adding_a_task() {
        let manager = CronManager::new();

        let result = manager
            .add(
                "0 9 * * *".into(),
                "p".into(),
                "s".into(),
                true,
                CronCreateOptions {
                    timezone: Some("No/Such_Zone".to_string()),
                    ..CronCreateOptions::default()
                },
            )
            .await;

        assert!(result.is_err());
        assert!(manager.list().is_empty(), "nothing registered on failure");
    }

    #[tokio::test]
    async fn should_apply_jitter_when_adding_a_recurring_task() {
        // Given a manager with jitter enabled and a daily task
        let manager = CronManager::with_options(CronOptions {
            jitter: true,
            stale: true,
        });

        let id = manager
            .add(
                "0 9 * * *".into(),
                "p".into(),
                "s".into(),
                true,
                CronCreateOptions::default(),
            )
            .await
            .unwrap();

        // Then the scheduled fire is NOT the bare ideal: it is shifted forward
        // by the deterministic per-id offset (0 <= offset <= 15min for a daily
        // task), i.e. at or after the ideal occurrence.
        let task = manager.get(&id).unwrap();
        let tz = CronTimezone::parse(task.timezone.as_deref())
            .unwrap()
            .resolve();
        let ideal = CronManager::compute_next_fire("0 9 * * *", Utc::now(), tz).unwrap();
        let next = task.next_fire_at.unwrap();
        assert!(next >= ideal, "recurring jitter never moves a fire earlier");
        assert!(next - ideal <= Duration::minutes(15), "15-minute hard cap");
    }

    #[tokio::test]
    async fn should_not_apply_jitter_when_options_disable_it() {
        let manager = CronManager::with_options(CronOptions {
            jitter: false,
            stale: true,
        });

        let id = manager
            .add(
                "0 9 * * *".into(),
                "p".into(),
                "s".into(),
                true,
                CronCreateOptions::default(),
            )
            .await
            .unwrap();

        let task = manager.get(&id).unwrap();
        let tz = CronTimezone::parse(task.timezone.as_deref())
            .unwrap()
            .resolve();
        let ideal = CronManager::compute_next_fire("0 9 * * *", Utc::now(), tz).unwrap();
        assert_eq!(
            task.next_fire_at.unwrap(),
            ideal,
            "no jitter: schedule == ideal"
        );
    }

    #[test]
    fn should_treat_exactly_one_as_on_when_reading_env_flags() {
        assert!(env_flag(Some("1".to_string())));
        assert!(!env_flag(Some("0".to_string())));
        assert!(!env_flag(Some("true".to_string())));
        assert!(!env_flag(None));
    }

    #[tokio::test]
    async fn should_record_timezone_in_the_cron_add_op_when_persisting() {
        let (manager, tmp) = recording_manager().await;

        manager
            .add(
                "0 9 * * *".into(),
                "p".into(),
                "s".into(),
                true,
                CronCreateOptions {
                    timezone: Some("Asia/Shanghai".to_string()),
                    ..CronCreateOptions::default()
                },
            )
            .await
            .unwrap();

        let ops = recorded_ops(&tmp).await;
        match &ops[0] {
            WireOp::CronAdd { task } => {
                assert_eq!(task["timezone"], "Asia/Shanghai");
            }
            other => panic!("expected cron.add, got {other:?}"),
        }
    }

    #[test]
    fn should_restore_a_task_in_its_explicit_timezone() {
        // Jitter off: pin the calendar, not the per-id shift.
        let manager = CronManager::with_options(CronOptions {
            jitter: false,
            stale: true,
        });
        let mut task = persisted_task("cron-1", "0 9 * * *", true);
        task.timezone = Some("Asia/Shanghai".to_string());

        manager.restore_tasks([task]);

        let restored = manager.get("cron-1").unwrap();
        let local = restored
            .next_fire_at
            .unwrap()
            .with_timezone(&chrono_tz::Asia::Shanghai);
        assert_eq!((local.hour(), local.minute()), (9, 0));
    }

    #[test]
    fn should_fall_back_to_local_when_restoring_a_task_without_timezone() {
        // 旧记录（Rust UTC 时代 / TS 记录）无 timezone 键 → 按本地求值。
        // 断言口径：next_fire_at 从记录的游标（created_at）重算且绝不采信
        // 持久化的绝对时刻；本地日历的绝对值由 embedded 的 TZ env 集成
        // 测试覆盖（B5 语义：重算自 baseFromMs 游标，可能落在过去——
        // 欠账由下一次 check_due 合并为一次触发）。
        let manager = CronManager::new();
        let task = persisted_task("cron-1", "*/5 * * * *", true);
        manager.restore_tasks([task.clone()]);
        let restored = manager.get("cron-1").unwrap();
        let next = restored.next_fire_at.unwrap();
        assert!(
            next != task.next_fire_at.unwrap(),
            "the persisted absolute never survives restore"
        );
        assert!(next > task.created_at, "the walk starts at the cursor");
        assert!(
            next <= task.created_at + Duration::minutes(5) + Duration::seconds(31),
            "the first */5 occurrence after the cursor (+jitter): {next}"
        );
    }

    // -----------------------------------------------------------------------
    // check_due: coalesced fires + cursor advance (B5 T6)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn should_carry_a_single_fire_when_check_due_with_a_recently_scheduled_task() {
        let manager = CronManager::new();
        let id = manager
            .add(
                "* * * * *".into(),
                "tick".into(),
                "s".into(),
                true,
                CronCreateOptions::default(),
            )
            .await
            .unwrap();
        {
            let mut tasks = manager.tasks.lock().unwrap();
            let backdated = Utc::now() - Duration::seconds(5);
            tasks.get_mut(&id).unwrap().next_fire_at = Some(backdated);
        }

        let due = manager.check_due().await;

        assert_eq!(due.len(), 1);
        assert_eq!(due[0].task_id, id);
        assert_eq!(due[0].prompt, "tick");
        assert_eq!(
            due[0].coalesced_count, 1,
            "an on-time fire coalesces nothing"
        );
        assert!(!due[0].stale);
    }

    #[tokio::test]
    async fn should_coalesce_missed_periods_into_one_fire_when_check_due_with_an_overdue_task() {
        // Given a */5 recurring task that last fired ~15 minutes ago (backdate
        // created_at, last_fired and next_fire_at, so the cursor base is
        // genuinely old)
        let manager = CronManager::new();
        let id = manager
            .add(
                "*/5 * * * *".into(),
                "tick".into(),
                "s".into(),
                true,
                CronCreateOptions::default(),
            )
            .await
            .unwrap();
        let now = Utc::now();
        let backdate = now - Duration::minutes(16);
        {
            let mut tasks = manager.tasks.lock().unwrap();
            let task = tasks.get_mut(&id).unwrap();
            task.created_at = backdate;
            task.last_fired = Some(backdate);
            task.next_fire_at = Some(backdate + Duration::minutes(5));
        }

        // When the due pass runs
        let due = manager.check_due().await;

        // Then exactly one fire is delivered, carrying the missed occurrences
        // (approx 3: at +5, +10, +15 minutes) — never a burst of catch-up prompts
        assert_eq!(due.len(), 1);
        assert!(
            (3..=4).contains(&due[0].coalesced_count),
            "approximately 3 missed periods coalesce (jitter may pull the boundary): {}",
            due[0].coalesced_count
        );
    }

    #[tokio::test]
    async fn should_advance_the_cursor_to_the_last_due_occurrence_when_check_due_coalesces() {
        let (manager, tmp) = recording_manager().await;
        let id = manager
            .add(
                "*/5 * * * *".into(),
                "tick".into(),
                "s".into(),
                true,
                CronCreateOptions::default(),
            )
            .await
            .unwrap();
        let now = Utc::now();
        let backdate = now - Duration::minutes(16);
        {
            let mut tasks = manager.tasks.lock().unwrap();
            let task = tasks.get_mut(&id).unwrap();
            task.created_at = backdate;
            task.last_fired = Some(backdate);
            task.next_fire_at = Some(backdate + Duration::minutes(5));
        }

        manager.check_due().await;

        // Then the recorded cursor carries the last due occurrence, not "now"
        let ops = recorded_ops(&tmp).await;
        match &ops[1] {
            WireOp::CronCursor {
                id: cursor_id,
                last_fired_at,
                fire_count: _,
            } => {
                assert_eq!(cursor_id, &id);
                assert!(
                    *last_fired_at < now.timestamp_millis() as u64,
                    "the cursor advances to the last due occurrence ({last_fired_at}), not now"
                );
            }
            other => panic!("expected cron.cursor, got {other:?}"),
        }
        // And the reschedule keeps the cadence phase: the next fire is a future
        // */5 boundary (minutes congruent to 0 mod 5 in the task's calendar)
        let task = manager.get(&id).unwrap();
        assert!(task.next_fire_at.unwrap() > now);
    }

    #[tokio::test]
    async fn should_keep_phase_when_check_due_reschedules_an_overdue_recurring_task() {
        let manager = CronManager::new();
        let id = manager
            .add(
                "*/5 * * * *".into(),
                "tick".into(),
                "s".into(),
                true,
                CronCreateOptions::default(),
            )
            .await
            .unwrap();
        let now = Utc::now();
        let backdate = now - Duration::minutes(16);
        {
            let mut tasks = manager.tasks.lock().unwrap();
            let task = tasks.get_mut(&id).unwrap();
            task.created_at = backdate;
            task.last_fired = Some(backdate);
            task.next_fire_at = Some(now - Duration::minutes(11));
        }

        manager.check_due().await;

        // Then the next fire stays on the */5 lattice: the local wall minute is
        // divisible by 5 (jitter <= 10% of 5min = 30s tolerates the boundary)
        let task = manager.get(&id).unwrap();
        let next = task.next_fire_at.unwrap();
        let tz = CronTimezone::Local.resolve();
        let shifted = next + Duration::seconds(30); // jitter tolerance for lattice alignment
        assert_eq!(
            shifted.with_timezone(&tz).minute() % 5,
            0,
            "cadence keeps its phase"
        );
    }

    // -----------------------------------------------------------------------
    // check_due: 7-day stale auto-delete (B5 T7)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn should_fire_once_then_delete_when_check_due_with_a_stale_recurring_task() {
        // Given a recurring task created 8 days ago, backdated onto the wire log
        let (manager, tmp) = recording_manager().await;
        let id = manager
            .add(
                "* * * * *".into(),
                "tick".into(),
                "s".into(),
                true,
                CronCreateOptions::default(),
            )
            .await
            .unwrap();
        let now = Utc::now();
        {
            let mut tasks = manager.tasks.lock().unwrap();
            let task = tasks.get_mut(&id).unwrap();
            task.created_at = now - Duration::days(8);
            task.next_fire_at = Some(now - Duration::seconds(5));
        }

        let due = manager.check_due().await;

        // Then it fires exactly once, flagged stale, and is auto-deleted
        assert_eq!(due.len(), 1);
        assert!(due[0].stale, "the fire carries the stale flag");
        assert!(
            manager.get(&id).is_none(),
            "a stale recurring task is removed after its final delivery"
        );
        // And the removal is durable: a cron.delete op precedes the in-memory drop
        let ops = recorded_ops(&tmp).await;
        assert!(
            ops.iter()
                .any(|op| matches!(op, WireOp::CronDelete { ids } if ids.contains(&id))),
            "stale auto-delete is persisted: {ops:?}"
        );
    }

    #[tokio::test]
    async fn should_not_stale_a_recurring_task_when_its_last_fired_refreshes_the_ttl_base() {
        // B.1 / D-M5 (fire-refresh): a recurring task created 8 days ago
        // but fired 1 day ago is ACTIVE — the TTL base is the last fire,
        // not the creation.
        let (manager, _tmp) = recording_manager().await;
        let id = manager
            .add(
                "* * * * *".into(),
                "tick".into(),
                "s".into(),
                true,
                CronCreateOptions::default(),
            )
            .await
            .unwrap();
        let now = Utc::now();
        {
            let mut tasks = manager.tasks.lock().unwrap();
            let task = tasks.get_mut(&id).unwrap();
            task.created_at = now - Duration::days(8);
            task.last_fired = Some(now - Duration::days(1));
            task.next_fire_at = Some(now - Duration::seconds(5));
        }

        let due = manager.check_due().await;

        assert_eq!(due.len(), 1);
        assert!(!due[0].stale, "a recently fired task is not stale");
        assert!(
            manager.get(&id).is_some(),
            "an active task survives its fire despite its age"
        );
    }

    #[tokio::test]
    async fn should_fire_stale_and_delete_when_the_last_fired_itself_is_past_the_ttl() {
        // B.2: the stalled-task bound — the LAST fire is 8 days old (the
        // plan itself stopped), so the task stales on its next check and
        // the final stale delivery removes it durably.
        let (manager, tmp) = recording_manager().await;
        let id = manager
            .add(
                "* * * * *".into(),
                "tick".into(),
                "s".into(),
                true,
                CronCreateOptions::default(),
            )
            .await
            .unwrap();
        let now = Utc::now();
        {
            let mut tasks = manager.tasks.lock().unwrap();
            let task = tasks.get_mut(&id).unwrap();
            task.created_at = now - Duration::days(20);
            task.last_fired = Some(now - Duration::days(8));
            task.next_fire_at = Some(now - Duration::seconds(5));
        }

        let due = manager.check_due().await;

        assert_eq!(due.len(), 1);
        assert!(due[0].stale, "a task stalled past the TTL stales");
        assert!(
            manager.get(&id).is_none(),
            "the stalled task is removed after its final stale delivery"
        );
        let ops = recorded_ops(&tmp).await;
        assert!(
            ops.iter()
                .any(|op| matches!(op, WireOp::CronDelete { ids } if ids.contains(&id))),
            "stale auto-delete is persisted: {ops:?}"
        );
    }

    #[test]
    fn should_only_mark_fires_later_than_the_late_threshold() {
        // C.2a's threshold caliber: exactly the threshold is not late
        // (a `>=` boundary would flip a fire scheduled a minute ago);
        // anything beyond it is. The test pins the computation's shape
        // (elapsed against CRON_LATE_MARK_THRESHOLD) — the injection
        // path applies it, this pins the constant's semantics.
        let now = Utc::now();
        let on_the_threshold = now - CRON_LATE_MARK_THRESHOLD;
        let past_the_threshold = now - CRON_LATE_MARK_THRESHOLD - Duration::milliseconds(1);
        assert!(
            now - on_the_threshold <= CRON_LATE_MARK_THRESHOLD,
            "a fire exactly at the threshold is not marked late"
        );
        assert!(
            now - past_the_threshold > CRON_LATE_MARK_THRESHOLD,
            "a fire past the threshold is marked late"
        );
    }

    #[tokio::test]
    async fn should_carry_the_scheduled_moment_when_a_planned_fire_is_returned() {
        // C.2a: a planned fire carries the moment it was scheduled for —
        // the task's pre-fire next_fire_at (the lateness reference).
        let (manager, _tmp) = recording_manager().await;
        let id = manager
            .add(
                "* * * * *".into(),
                "tick".into(),
                "s".into(),
                true,
                CronCreateOptions::default(),
            )
            .await
            .unwrap();
        let scheduled = Utc::now() - Duration::seconds(5);
        {
            let mut tasks = manager.tasks.lock().unwrap();
            let task = tasks.get_mut(&id).unwrap();
            task.next_fire_at = Some(scheduled);
        }

        let due = manager.check_due().await;

        assert_eq!(due.len(), 1);
        assert_eq!(
            due[0].scheduled_at,
            Some(scheduled),
            "the planned fire carries the moment it was scheduled for"
        );
    }

    #[tokio::test]
    async fn should_leave_the_scheduled_moment_absent_when_a_run_now_fire_is_returned() {
        // Run-now has no lateness semantics: a manual run is early by
        // design, never late — `scheduled_at` stays `None`.
        let manager = CronManager::with_options(CronOptions {
            jitter: false,
            stale: true,
        });
        let id = manager
            .add(
                "* * * * *".into(),
                "tick".into(),
                "s".into(),
                true,
                CronCreateOptions::default(),
            )
            .await
            .unwrap();

        let fire = manager.trigger(&id).unwrap();

        assert_eq!(fire.scheduled_at, None, "run-now fires carry no schedule");
    }

    #[tokio::test]
    async fn should_keep_a_fresh_recurring_task_when_check_due_fires_it() {
        // Hermetic options: the stale assertions must not depend on the
        // process env (MIRRI_CRON_NO_STALE=1 would otherwise flip them).
        let manager = CronManager::with_options(CronOptions {
            jitter: false,
            stale: true,
        });
        let id = manager
            .add(
                "* * * * *".into(),
                "tick".into(),
                "s".into(),
                true,
                CronCreateOptions::default(),
            )
            .await
            .unwrap();
        {
            let mut tasks = manager.tasks.lock().unwrap();
            let backdated = Utc::now() - Duration::seconds(5);
            tasks.get_mut(&id).unwrap().next_fire_at = Some(backdated);
        }

        let due = manager.check_due().await;

        assert!(!due[0].stale);
        assert!(
            manager.get(&id).is_some(),
            "a 7-day-young task survives its fire"
        );
    }

    #[tokio::test]
    async fn should_not_mark_a_one_shot_task_stale_when_check_due_fires_it() {
        // One-shots are never stale (TS: recurring === false short-circuits) —
        // they are removed by the one-shot rule, flagged stale=false.
        // Hermetic options: no env dependence for the stale flag.
        let manager = CronManager::with_options(CronOptions {
            jitter: false,
            stale: true,
        });
        let id = manager
            .add(
                "* * * * *".into(),
                "once".into(),
                "s".into(),
                false,
                CronCreateOptions::default(),
            )
            .await
            .unwrap();
        {
            let mut tasks = manager.tasks.lock().unwrap();
            let task = tasks.get_mut(&id).unwrap();
            task.created_at = Utc::now() - Duration::days(8);
            task.next_fire_at = Some(Utc::now() - Duration::seconds(5));
        }

        let due = manager.check_due().await;

        assert!(!due[0].stale);
        assert!(
            manager.get(&id).is_none(),
            "removed by the one-shot rule, not the stale rule"
        );
    }

    #[tokio::test]
    async fn should_not_delete_a_stale_task_when_stale_detection_is_disabled() {
        let manager = CronManager::with_options(CronOptions {
            jitter: true,
            stale: false,
        });
        let id = manager
            .add(
                "* * * * *".into(),
                "tick".into(),
                "s".into(),
                true,
                CronCreateOptions::default(),
            )
            .await
            .unwrap();
        {
            let mut tasks = manager.tasks.lock().unwrap();
            let task = tasks.get_mut(&id).unwrap();
            task.created_at = Utc::now() - Duration::days(8);
            task.next_fire_at = Some(Utc::now() - Duration::seconds(5));
        }

        let due = manager.check_due().await;

        assert!(!due[0].stale);
        assert!(
            manager.get(&id).is_some(),
            "MIRRI_CRON_NO_STALE keeps an old task alive"
        );
    }

    #[tokio::test]
    async fn should_not_fire_a_stale_task_before_its_due_time() {
        // Stale is evaluated at fire time only (TS deliverDue) — a 10-day-old
        // task with a future next_fire_at is NOT eagerly deleted by check_due.
        // Hermetic options: no env dependence for the stale flag.
        let manager = CronManager::with_options(CronOptions {
            jitter: false,
            stale: true,
        });
        let id = manager
            .add(
                "0 9 * * *".into(),
                "tick".into(),
                "s".into(),
                true,
                CronCreateOptions::default(),
            )
            .await
            .unwrap();
        {
            let mut tasks = manager.tasks.lock().unwrap();
            tasks.get_mut(&id).unwrap().created_at = Utc::now() - Duration::days(10);
        }

        assert!(manager.check_due().await.is_empty());
        assert!(
            manager.get(&id).is_some(),
            "stale deletion only rides a fire"
        );
    }

    #[test]
    fn should_keep_a_stale_task_listed_when_restoring_it() {
        // Restore never drops a stale task (TS: stale removal only rides a
        // fire) — it stays listed and fires once (flagged) on the next pass.
        // Hermetic options: no env dependence for the stale flag.
        let manager = CronManager::with_options(CronOptions {
            jitter: false,
            stale: true,
        });
        let mut task = persisted_task("cron-stale", "*/5 * * * *", true);
        task.created_at = Utc::now() - Duration::days(8);

        manager.restore_tasks([task]);

        assert!(
            manager.get("cron-stale").is_some(),
            "a stale task survives restore; only its next fire removes it"
        );
    }

    #[test]
    fn should_preserve_fields_when_serde_roundtripping_cron_task() {
        let task = CronTask {
            session_id: "session-9".into(),
            last_fired: Some(Utc::now()),
            next_fire_at: Some(Utc::now()),
            ..task_base("cron-1", "0 9 * * 1", "weekly")
        };

        let json = serde_json::to_string(&task).unwrap();
        let decoded: CronTask = serde_json::from_str(&json).unwrap();

        assert_eq!(task.id, decoded.id);
        assert_eq!(task.cron, decoded.cron);
        assert_eq!(task.prompt, decoded.prompt);
        assert_eq!(task.session_id, decoded.session_id);
        assert_eq!(task.recurring, decoded.recurring);
        assert_eq!(task.created_at, decoded.created_at);
        assert_eq!(task.last_fired, decoded.last_fired);
        assert_eq!(task.next_fire_at, decoded.next_fire_at);
    }

    #[test]
    fn should_serialize_timestamps_with_utc_z_suffix_when_serde_roundtripping_cron_task() {
        // Guard against chrono serde drift: timestamps must round-trip as ISO
        // 8601 (chrono emits the `Z` suffix for UTC).
        let task = CronTask {
            recurring: false,
            ..task_base("cron-rt", "*/5 * * * *", "rt")
        };
        let json = serde_json::to_string(&task).unwrap();
        assert!(
            json.contains('Z'),
            "UTC timestamps serialize with the Z suffix: {json}"
        );
        let decoded: CronTask = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.created_at, task.created_at);
    }

    // -----------------------------------------------------------------------
    // wire-log persistence
    // -----------------------------------------------------------------------

    /// A manager whose recorder shares a writer installed on a temp
    /// `wire.jsonl`, so tests can assert exactly what gets recorded.
    async fn recording_manager() -> (CronManager, tempfile::TempDir) {
        // Hermetic options (no env dependence, deterministic schedules).
        recording_manager_with(CronOptions {
            jitter: false,
            stale: true,
        })
        .await
    }

    /// [`recording_manager`] with explicit options.
    async fn recording_manager_with(options: CronOptions) -> (CronManager, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let writer = JsonlAppendLog::create(&tmp.path().join("wire.jsonl"))
            .await
            .unwrap();
        let recorder = SessionWireLog::default();
        assert!(
            recorder.install(writer).await,
            "fresh slot accepts a writer"
        );
        (
            CronManager::with_options(options).with_wire_recorder(recorder),
            tmp,
        )
    }

    /// The ops recorded to a recording manager's wire log (metadata excluded).
    async fn recorded_ops(tmp: &tempfile::TempDir) -> Vec<WireOp> {
        let log = JsonlAppendLog::open(&tmp.path().join("wire.jsonl"))
            .await
            .unwrap();
        log.read_all()
            .await
            .unwrap()
            .into_iter()
            .filter(|op| !matches!(op, WireOp::Metadata { .. }))
            .collect()
    }

    #[tokio::test]
    async fn should_record_cron_add_to_wire_log_when_task_is_added() {
        let (manager, tmp) = recording_manager().await;

        let id = manager
            .add(
                "0 9 * * *".into(),
                "morning briefing".into(),
                "session-9".into(),
                true,
                CronCreateOptions::default(),
            )
            .await
            .unwrap();

        let ops = recorded_ops(&tmp).await;
        assert_eq!(ops.len(), 1, "exactly one op: {ops:?}");
        match &ops[0] {
            WireOp::CronAdd { task } => {
                // The F16 wire DTO: camelCase keys, epoch millis, the
                // session id folded into tags.sessionId, no absolute next
                // fire (recomputed on restore).
                assert_eq!(task["id"], id);
                assert_eq!(task["cron"], "0 9 * * *");
                assert_eq!(task["prompt"], "morning briefing");
                assert_eq!(task["tags"]["sessionId"], "session-9");
                assert_eq!(task["recurring"], true);
                assert!(
                    task["createdAt"].is_number(),
                    "createdAt is epoch millis: {task}"
                );
                assert!(
                    task.get("lastFiredAt").is_none(),
                    "an unfired task omits lastFiredAt: {task}"
                );
                assert!(
                    task.get("nextFireAt").is_none(),
                    "the absolute next fire is not persisted: {task}"
                );
            }
            other => panic!("expected cron.add, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn should_record_nothing_when_mutating_a_manager_without_a_wire_log() {
        // The default recorder slot is empty: every mutation path must be a
        // no-op on the persistence side (test scopes, pre-install sessions).
        let manager = CronManager::new();

        let id = manager
            .add(
                "* * * * *".into(),
                "tick".into(),
                "s".into(),
                true,
                CronCreateOptions::default(),
            )
            .await
            .unwrap();
        assert!(manager.remove(&id).await);
        assert_eq!(manager.list().len(), 0);
    }

    #[tokio::test]
    async fn should_record_cron_delete_to_wire_log_when_existing_task_is_removed() {
        let (manager, tmp) = recording_manager().await;
        let id = manager
            .add(
                "* * * * *".into(),
                "tick".into(),
                "s".into(),
                true,
                CronCreateOptions::default(),
            )
            .await
            .unwrap();

        assert!(manager.remove(&id).await);

        let ops = recorded_ops(&tmp).await;
        assert_eq!(ops.len(), 2, "add + delete: {ops:?}");
        match &ops[1] {
            WireOp::CronDelete { ids } => assert_eq!(ids, &vec![id.clone()]),
            other => panic!("expected cron.delete, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn should_record_no_delete_when_removing_an_unknown_task() {
        let (manager, tmp) = recording_manager().await;

        assert!(!manager.remove("cron-nope").await);

        let ops = recorded_ops(&tmp).await;
        assert!(ops.is_empty(), "no op for a removal that removed nothing");
    }

    #[tokio::test]
    async fn should_record_cron_cursor_to_wire_log_when_check_due_fires_a_task() {
        let (manager, tmp) = recording_manager().await;
        let id = manager
            .add(
                "* * * * *".into(),
                "tick".into(),
                "s".into(),
                true,
                CronCreateOptions::default(),
            )
            .await
            .unwrap();
        {
            let mut tasks = manager.tasks.lock().unwrap();
            let backdated = Utc::now() - Duration::seconds(5);
            tasks.get_mut(&id).unwrap().next_fire_at = Some(backdated);
        }

        let due = manager.check_due().await;
        let after = Utc::now().timestamp_millis() as u64;

        // The pre-fire next_fire_at rides the fire as its scheduled moment
        // (C.2a) — backdated above, so the expectation is rebuilt with it.
        let mut expected = single_fire(&id, "tick", "* * * * *", true);
        expected.scheduled_at = due[0].scheduled_at;
        assert_eq!(due, vec![expected]);
        let ops = recorded_ops(&tmp).await;
        assert_eq!(ops.len(), 2, "add + cursor: {ops:?}");
        match &ops[1] {
            WireOp::CronCursor {
                id: cursor_id,
                last_fired_at,
                fire_count: _,
            } => {
                assert_eq!(cursor_id, &id);
                // The cursor is the fire time in epoch milliseconds. It is
                // either "now" (no occurrence coalesced) or, when the minute
                // boundary fell between `add` and the check, that boundary —
                // so the assertion is a one-minute window rather than
                // `>= add-time`: the tighter form flaked whenever a loaded
                // parallel run straddled the boundary. A seconds-scale value
                // (off by ~1.7e12) still fails this.
                assert!(
                    *last_fired_at <= after && after - *last_fired_at < 60_000,
                    "the cursor carries the fire time as epoch milliseconds: \
                     cursor {last_fired_at}, now {after}"
                );
            }
            other => panic!("expected cron.cursor, got {other:?}"),
        }
        // The cursor was recorded even though the recurring task stayed
        // registered (rescheduled in place).
        assert!(manager.get(&id).is_some());
    }

    // -----------------------------------------------------------------------
    // management run-now (B6)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn should_fire_now_without_touching_the_schedule_when_trigger_targets_a_registered_task()
    {
        // Given a manager with a registered recurring task whose schedule is known
        let (manager, _tmp) = recording_manager().await;
        let id = manager
            .add(
                "* * * * *".into(),
                "tick".into(),
                "s".into(),
                true,
                CronCreateOptions::default(),
            )
            .await
            .unwrap();
        let before = manager.get(&id).unwrap();

        // When the management face triggers it manually
        let fire = manager.trigger(&id).expect("the registered task fires");

        // Then the fire carries the task's prompt as a plain single fire
        assert_eq!(fire.task_id, id);
        assert_eq!(fire.prompt, "tick");
        assert_eq!(fire.cron, "* * * * *");
        assert!(fire.recurring);
        assert_eq!(fire.coalesced_count, 1, "a manual fire coalesces nothing");
        assert!(!fire.stale);
        // And the schedule is untouched: membership, next fire, and cursor
        // all keep their pre-trigger values (D-2 — run-now never moves the
        // cadence, so the natural fire still happens per the expression).
        let after = manager.get(&id).expect("the task survives the manual fire");
        assert_eq!(after.next_fire_at, before.next_fire_at);
        assert_eq!(after.last_fired, before.last_fired);
    }

    #[test]
    fn should_return_none_when_trigger_targets_an_unknown_task() {
        let manager = CronManager::new();
        assert!(manager.trigger("cron-nope").is_none());
    }

    #[test]
    fn should_publish_the_cron_fired_event_when_trigger_fires_a_task() {
        // Given a manager wired to a session bus with one restored task
        let manager = CronManager::new();
        let bus = std::sync::Arc::new(crate::session::EventBus::new(16));
        manager.set_event_bus(std::sync::Arc::clone(&bus));
        manager.restore_tasks([persisted_task("cron-manual", "* * * * *", true)]);
        let mut events = bus.subscribe();

        // When the management face triggers the task
        let fire = manager.trigger("cron-manual").expect("fires");

        // Then the wire event carries the full firing identity
        match events.try_recv() {
            Ok(crate::session::SessionEvent::CronFired {
                task_id,
                cron,
                prompt,
                coalesced_count,
                stale,
                ..
            }) => {
                assert_eq!(task_id, fire.task_id);
                assert_eq!(cron, "* * * * *");
                assert_eq!(prompt, fire.prompt);
                assert_eq!(coalesced_count, 1);
                assert!(!stale);
            }
            other => panic!("expected a CronFired event, got {other:?}"),
        }
    }

    #[test]
    fn should_return_none_when_trigger_targets_a_task_after_shutdown() {
        // Given a manager that was shut down with one task still registered
        let manager = CronManager::new();
        manager.restore_tasks([persisted_task("cron-late", "* * * * *", true)]);
        manager.shutdown();

        // When the management face tries to fire the task anyway
        // Then the trigger refuses, the same caliber as check_due
        assert!(manager.trigger("cron-late").is_none());
    }

    #[tokio::test]
    async fn should_record_no_wire_op_when_trigger_fires_a_registered_task() {
        // Given a recording manager with one registered task
        let (manager, tmp) = recording_manager().await;
        let id = manager
            .add(
                "* * * * *".into(),
                "tick".into(),
                "s".into(),
                true,
                CronCreateOptions::default(),
            )
            .await
            .unwrap();

        // When the management face fires it out of band
        assert!(manager.trigger(&id).is_some());

        // Then the wire log still holds exactly the one cron.add op —
        // the run-now face is persistence-invisible by design (D-2: no
        // cursor advance, no reschedule, no one-shot consumption).
        let ops = recorded_ops(&tmp).await;
        assert_eq!(ops.len(), 1, "only the add: {ops:?}");
        assert!(matches!(ops[0], WireOp::CronAdd { .. }));
    }

    #[tokio::test]
    async fn should_keep_a_one_shot_registered_when_trigger_fires_it_manually() {
        // Given a manager with a registered one-shot task (D-2's headline
        // promise: a one-shot manually run once still fires naturally —
        // it must not vanish after the manual run)
        let (manager, _tmp) = recording_manager().await;
        let id = manager
            .add(
                "0 9 1 1 *".into(),
                "once".into(),
                "s".into(),
                false,
                CronCreateOptions::default(),
            )
            .await
            .unwrap();
        let before = manager.get(&id).unwrap();

        // When the management face triggers it manually
        let fire = manager.trigger(&id).expect("the one-shot fires manually");

        // Then the fire carries the one-shot's prompt…
        assert_eq!(fire.prompt, "once");
        assert!(!fire.recurring);
        // …and the task survives with its schedule untouched — the manual
        // run consumed nothing; the natural fire still happens.
        let after = manager
            .get(&id)
            .expect("the one-shot survives the manual fire");
        assert_eq!(after.next_fire_at, before.next_fire_at);
        assert_eq!(after.last_fired, before.last_fired);
        assert!(!after.recurring, "still a one-shot, not converted");
    }

    // -----------------------------------------------------------------------
    // restore
    // -----------------------------------------------------------------------

    /// The literal base (form batch 6 T1): every field at its documented
    /// default, so a test literal overrides only what it means to pin —
    /// the six new optional fields never need hand-copying per site.
    fn task_base(id: &str, cron: &str, prompt: &str) -> CronTask {
        super::cron_task_base(id, cron, prompt)
    }

    fn persisted_task(id: &str, cron: &str, recurring: bool) -> CronTask {
        CronTask {
            prompt: "restored".into(),
            recurring,
            // A stale absolute timestamp: an hour in the past.
            created_at: Utc::now() - Duration::hours(2),
            next_fire_at: Some(Utc::now() - Duration::hours(1)),
            ..task_base(id, cron, "restored")
        }
    }

    // -----------------------------------------------------------------------
    // stage-1 batch-1: task origin / max_fires (S1); batch 4 retires
    // `unattended` from the record (A4: the permission mode law)
    // -----------------------------------------------------------------------

    #[test]
    fn should_default_the_new_scheduling_fields_when_a_legacy_record_has_no_keys() {
        // Given a wire record written before this batch (no origin /
        // maxFires / fireCount / cronLineage keys)
        let legacy = serde_json::json!({
            "id": "cron-legacy",
            "cron": "*/5 * * * *",
            "prompt": "tick",
            "recurring": true,
            "createdAt": utc(2026, 9, 7, 10, 0).timestamp_millis(),
            "tags": { "sessionId": "s-legacy" }
        });

        // When it is parsed back into a task
        let task = cron_task_from_wire(&legacy).expect("legacy record parses");

        // Then every new field takes its documented default: origin agent
        // (historical records are all model-created), no budget, zero
        // fires, not lineage-born.
        assert_eq!(task.origin, TaskOrigin::Agent);
        assert_eq!(task.max_fires, None);
        assert_eq!(task.fire_count, 0);
        assert!(!task.cron_lineage);
    }

    /// Batch 4 (A4 cleanup): the retired `unattended` key is neither
    /// written nor honoured — a legacy record that still carries it parses
    /// (the record shape stays readable), and a re-serialized task drops
    /// the key, so no store grows it back.
    #[test]
    fn should_ignore_the_retired_unattended_key_on_read_and_drop_it_on_write() {
        // Given a record persisted before the field was retired
        let legacy = serde_json::json!({
            "id": "cron-retired",
            "cron": "*/5 * * * *",
            "prompt": "tick",
            "recurring": true,
            "createdAt": utc(2026, 9, 7, 10, 0).timestamp_millis(),
            "tags": { "sessionId": "s-retired" },
            "unattended": true
        });

        // When it is parsed back and written out again
        let task = cron_task_from_wire(&legacy).expect("legacy record parses");
        let rewritten = cron_task_wire(&task);

        // Then the key is gone from the written shape — the permission
        // mode is the only carrier of a task's ask law.
        assert!(
            rewritten.get("unattended").is_none(),
            "the retired key must not be written back: {rewritten}"
        );
    }

    /// Review m5: given a wire record written before the form batch (none
    /// of the six form-field keys), when parsed back, then every form field
    /// reads `None` — a pre-form record keeps its exact semantics, no
    /// invented values.
    #[test]
    fn should_read_every_form_field_as_none_when_a_legacy_record_has_no_form_keys() {
        let legacy = serde_json::json!({
            "id": "cron-form-legacy",
            "cron": "*/5 * * * *",
            "prompt": "tick",
            "recurring": true,
            "createdAt": utc(2026, 10, 31, 10, 0).timestamp_millis(),
            "tags": { "sessionId": "s-form-legacy" }
        });

        let task = cron_task_from_wire(&legacy).expect("legacy record parses");

        assert_eq!(task.title, None);
        assert_eq!(task.model, None);
        assert_eq!(task.effort, None);
        assert_eq!(task.workdir, None);
        assert_eq!(task.ends_at, None);
        assert_eq!(task.interval, None);
    }

    #[test]
    fn should_round_trip_the_new_scheduling_fields_through_the_wire_dto() {
        // Given a task carrying every new field
        let mut task = persisted_task("cron-wire", "*/5 * * * *", true);
        task.origin = TaskOrigin::User;
        task.max_fires = Some(5);
        task.fire_count = 2;
        task.cron_lineage = true;

        // When it crosses the wire and comes back
        let parsed = cron_task_from_wire(&cron_task_wire(&task)).expect("round trip parses");

        // Then every new field survives verbatim.
        assert_eq!(parsed.origin, TaskOrigin::User);
        assert_eq!(parsed.max_fires, Some(5));
        assert_eq!(parsed.fire_count, 2);
        assert!(parsed.cron_lineage);
    }

    #[test]
    fn should_round_trip_task_origin_through_serde_without_enum_drift() {
        // Both variants must keep their wire spellings ("user" / "agent")
        // — a drift would mislabel every historical record on restore.
        for (origin, spelling) in [
            (TaskOrigin::User, "user"),
            (TaskOrigin::Agent, "agent"),
            // Stage-2 4b (R6): the approved-proposal origin keeps its own
            // explicit wire spelling — three-valued, never silently
            // degraded to plain "agent" (D.0c's honest labeling).
            (TaskOrigin::AgentApproved, "agent_approved"),
        ] {
            let json = serde_json::to_string(&origin).unwrap();
            assert_eq!(json, format!("\"{spelling}\""));
            let decoded: TaskOrigin = serde_json::from_str(&json).unwrap();
            assert_eq!(decoded, origin);
        }
        assert_eq!(
            serde_json::from_str::<TaskOrigin>("\"agent\"").unwrap(),
            TaskOrigin::default(),
            "agent is the default (historical records are all model-created)"
        );
    }

    #[tokio::test]
    async fn should_record_the_new_fields_in_the_cron_add_op_when_persisting() {
        // Given a recording manager and a task created with the new knobs
        let (manager, tmp) = recording_manager().await;

        manager
            .add(
                "0 9 * * *".into(),
                "p".into(),
                "s".into(),
                true,
                CronCreateOptions {
                    timezone: None,
                    max_fires: Some(4),
                    origin: TaskOrigin::User,
                    cron_lineage: false,
                    ..CronCreateOptions::default()
                },
            )
            .await
            .unwrap();

        // Then the cron.add op carries the new camelCase keys
        let ops = recorded_ops(&tmp).await;
        match &ops[0] {
            WireOp::CronAdd { task } => {
                assert_eq!(task["origin"], "user");
                assert_eq!(task["maxFires"], 4);
                assert_eq!(task["fireCount"], 0);
                assert_eq!(task["cronLineage"], false);
            }
            other => panic!("expected cron.add, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn should_omit_max_fires_from_the_wire_dto_when_no_budget_is_set() {
        let (manager, tmp) = recording_manager().await;

        manager
            .add(
                "0 9 * * *".into(),
                "p".into(),
                "s".into(),
                true,
                CronCreateOptions::default(),
            )
            .await
            .unwrap();

        let ops = recorded_ops(&tmp).await;
        match &ops[0] {
            WireOp::CronAdd { task } => {
                assert!(
                    task.get("maxFires").is_none(),
                    "an unbudgeted task omits maxFires: {task}"
                );
            }
            other => panic!("expected cron.add, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn should_reject_a_max_fires_budget_on_a_one_shot_task_when_adding() {
        // A one-shot is its own budget (one fire) — declaring maxFires on
        // it is a caller bug and must be rejected before anything is
        // registered.
        let manager = CronManager::new();

        let result = manager
            .add(
                "0 9 * * *".into(),
                "p".into(),
                "s".into(),
                false,
                CronCreateOptions {
                    max_fires: Some(3),
                    ..CronCreateOptions::default()
                },
            )
            .await;

        assert!(result.is_err());
        assert!(manager.list().is_empty());
    }

    #[tokio::test]
    async fn should_reject_a_zero_max_fires_budget_when_adding() {
        let manager = CronManager::new();

        let result = manager
            .add(
                "0 9 * * *".into(),
                "p".into(),
                "s".into(),
                true,
                CronCreateOptions {
                    max_fires: Some(0),
                    ..CronCreateOptions::default()
                },
            )
            .await;

        assert!(result.is_err());
        assert!(manager.list().is_empty());
    }

    // -----------------------------------------------------------------------
    // stage-1 batch-1: fire counting + maxed-out finale (S3)
    // -----------------------------------------------------------------------

    /// Force a task due and drive one check_due pass — the test driver for
    /// budget tests.
    async fn drive_one_fire(manager: &CronManager, id: &str) -> Vec<CronFire> {
        {
            let mut tasks = manager.tasks.lock().unwrap();
            tasks.get_mut(id).unwrap().next_fire_at = Some(Utc::now() - Duration::seconds(5));
        }
        manager.check_due().await
    }

    #[tokio::test]
    async fn should_remove_the_task_and_carry_the_maxed_out_mark_when_the_budget_is_exhausted() {
        // Given a recurring task with a budget of 3 (jitter off)
        let manager = CronManager::with_options(CronOptions {
            jitter: false,
            stale: true,
        });
        let id = manager
            .add(
                "* * * * *".into(),
                "tick".into(),
                "s".into(),
                true,
                CronCreateOptions {
                    max_fires: Some(3),
                    ..CronCreateOptions::default()
                },
            )
            .await
            .unwrap();

        // When it is driven three times
        let mut saw_maxed = false;
        for _ in 0..3 {
            let due = drive_one_fire(&manager, &id).await;
            assert_eq!(due.len(), 1);
            saw_maxed = saw_maxed || due[0].maxed_out;
        }

        // Then the third fire carried the maxed-out mark, the task is gone,
        // and its recorded count reached the budget.
        assert!(saw_maxed, "the budget-exhausting fire is marked");
        assert!(
            manager.get(&id).is_none(),
            "the task is removed after the budget exhausts"
        );
    }

    #[tokio::test]
    async fn should_record_the_delete_op_before_the_maxed_out_removal_when_persisting() {
        let (manager, tmp) = recording_manager().await;
        let id = manager
            .add(
                "* * * * *".into(),
                "tick".into(),
                "s".into(),
                true,
                CronCreateOptions {
                    max_fires: Some(1),
                    ..CronCreateOptions::default()
                },
            )
            .await
            .unwrap();

        let due = drive_one_fire(&manager, &id).await;

        // Then the fire is marked, and the wire log carries the delete op
        // for the removal (write-before-mutate — the removal survives a
        // crash between the op and the in-memory drop).
        assert_eq!(due.len(), 1);
        assert!(due[0].maxed_out);
        let ops = recorded_ops(&tmp).await;
        assert!(
            ops.iter()
                .any(|op| matches!(op, WireOp::CronDelete { ids } if ids.contains(&id))),
            "a cron.delete op is recorded for the maxed-out removal: {ops:?}"
        );
    }

    #[tokio::test]
    async fn should_carry_the_post_fire_count_on_the_cursor_op_when_persisting() {
        let (manager, tmp) = recording_manager().await;
        let id = manager
            .add(
                "* * * * *".into(),
                "tick".into(),
                "s".into(),
                true,
                CronCreateOptions::default(),
            )
            .await
            .unwrap();

        drive_one_fire(&manager, &id).await;

        // Then the cursor op carries the post-fire count (1), same lock as
        // the delivery — the count survives a crash between op and memory.
        let ops = recorded_ops(&tmp).await;
        let cursor = ops
            .iter()
            .find(|op| matches!(op, WireOp::CronCursor { .. }))
            .expect("cursor op recorded");
        match cursor {
            WireOp::CronCursor { fire_count, .. } => {
                assert_eq!(*fire_count, Some(1));
            }
            _ => unreachable!(),
        }
        // And the surviving task's in-memory count matches
        assert_eq!(manager.get(&id).unwrap().fire_count, 1);
    }

    #[tokio::test]
    async fn should_not_fire_again_when_a_maxed_out_task_is_driven_a_fourth_time() {
        let manager = CronManager::with_options(CronOptions {
            jitter: false,
            stale: true,
        });
        let id = manager
            .add(
                "* * * * *".into(),
                "tick".into(),
                "s".into(),
                true,
                CronCreateOptions {
                    max_fires: Some(2),
                    ..CronCreateOptions::default()
                },
            )
            .await
            .unwrap();

        for _ in 0..2 {
            drive_one_fire(&manager, &id).await;
        }

        // When the (removed) task would be driven again, nothing fires
        let due = manager.check_due().await;

        assert!(due.is_empty(), "no fourth fire after the budget exhausted");
    }

    #[tokio::test]
    async fn should_leave_the_count_untouched_when_triggering_run_now() {
        // Run-now never counts and never moves the cursor (D-M7) — the
        // natural fire still happens per the expression.
        let manager = CronManager::with_options(CronOptions {
            jitter: false,
            stale: true,
        });
        let id = manager
            .add(
                "0 9 * * *".into(),
                "p".into(),
                "s".into(),
                true,
                CronCreateOptions {
                    max_fires: Some(1),
                    ..CronCreateOptions::default()
                },
            )
            .await
            .unwrap();

        let fire = manager.trigger(&id).expect("run-now fires");

        // Then the fire carries no budget marks and the task is untouched
        assert!(!fire.maxed_out);
        assert_eq!(fire.coalesced_count, 1);
        let after = manager.get(&id).expect("the task survives run-now");
        assert_eq!(after.fire_count, 0, "run-now never counts");
        assert!(
            after.next_fire_at.is_some(),
            "run-now never moves the cursor — the natural schedule stands"
        );
        // And run-now does not complete a budget: a subsequent scheduled
        // fire still fires.
        let due = drive_one_fire(&manager, &id).await;
        assert_eq!(due.len(), 1);
        assert!(
            due[0].maxed_out,
            "the budget completes on the SCHEDULED fire"
        );
    }

    #[tokio::test]
    async fn should_fire_the_stale_finale_before_the_budget_verdict_when_both_apply() {
        // Stale + budget: the existing plan order computes stale first;
        // the fire carries stale (the older terminal signal) and the task
        // is removed either way.
        let (manager, _tmp) = recording_manager().await;
        let id = manager
            .add(
                "* * * * *".into(),
                "tick".into(),
                "s".into(),
                true,
                CronCreateOptions {
                    max_fires: Some(1),
                    ..CronCreateOptions::default()
                },
            )
            .await
            .unwrap();
        {
            let mut tasks = manager.tasks.lock().unwrap();
            let task = tasks.get_mut(&id).unwrap();
            task.created_at = Utc::now() - Duration::days(8);
        }

        let due = drive_one_fire(&manager, &id).await;

        assert_eq!(due.len(), 1);
        assert!(due[0].stale, "stale wins the plan order");
        assert!(manager.get(&id).is_none());
    }

    #[tokio::test]
    async fn should_not_burn_a_lineage_slot_when_the_expression_is_invalid() {
        // m1: a lineage one-shot with a syntactically invalid expression
        // registers NOTHING and leaves the creation count untouched —
        // the validation happens BEFORE the slot is reserved, so a bad
        // expression never consumes chain-cap quota.
        let (manager, _tmp) = recording_manager().await;

        let result = manager
            .add_lineage_one_shot(
                "not-a-cron-expression".into(),
                "tick".into(),
                "s".into(),
                None,
            )
            .await;

        assert!(result.is_err(), "the invalid expression is rejected");
        assert!(
            manager.list().is_empty(),
            "no task registered for the failed creation"
        );
        assert_eq!(
            manager.lineage_one_shot_created(),
            0,
            "the lineage count is untouched"
        );
    }

    #[tokio::test]
    async fn should_not_burn_a_lineage_slot_when_the_timezone_is_unknown() {
        // m1 companion: an unusable timezone name also fails before the
        // slot reservation.
        let (manager, _tmp) = recording_manager().await;

        let result = manager
            .add_lineage_one_shot(
                "*/5 * * * *".into(),
                "tick".into(),
                "s".into(),
                Some("Mars/Olympus_Mons".to_string()),
            )
            .await;

        assert!(result.is_err(), "the unknown timezone is rejected");
        assert_eq!(
            manager.lineage_one_shot_created(),
            0,
            "the lineage count is untouched"
        );
    }

    #[tokio::test]
    async fn should_record_the_lineage_count_op_before_the_cron_add_op_when_persisting() {
        // The crash-gap order (S9): the `cron.lineage_created` op must
        // precede the matching `cron.add` — a crash between them leaves
        // the log having counted a creation that never landed (over-count,
        // the fail-safe direction; an under-count would over-admit).
        let (manager, tmp) = recording_manager().await;

        let id = manager
            .add_lineage_one_shot("*/5 * * * *".into(), "tick".into(), "s".into(), None)
            .await
            .unwrap();

        let ops = recorded_ops(&tmp).await;
        let lineage_pos = ops
            .iter()
            .position(|op| matches!(op, WireOp::CronLineageCreated { count: 1 }))
            .expect("the lineage count op is recorded");
        let add_pos = ops
            .iter()
            .position(|op| matches!(op, WireOp::CronAdd { task } if task["id"] == id.as_str()))
            .expect("the cron.add op is recorded");
        assert!(
            lineage_pos < add_pos,
            "the count op precedes the add op: lineage@{lineage_pos} add@{add_pos}"
        );
    }

    #[tokio::test]
    async fn should_restore_the_lineage_count_from_the_reducer_state() {
        // The restore pours the folded absolute — the max semantics of
        // the reducer make any recorded value a floor.
        let manager = CronManager::new();
        assert_eq!(manager.lineage_one_shot_created(), 0);

        manager.restore_lineage_count(7);
        assert_eq!(manager.lineage_one_shot_created(), 7);

        // A lower restore value never lowers the count.
        manager.restore_lineage_count(3);
        assert_eq!(manager.lineage_one_shot_created(), 7);
    }

    #[tokio::test]
    async fn should_count_a_coalesced_delivery_as_one_fire_when_overdue_periods_merge() {
        // D-M7: coalesced deliveries count once.
        let manager = CronManager::with_options(CronOptions {
            jitter: false,
            stale: true,
        });
        let id = manager
            .add(
                "*/5 * * * *".into(),
                "tick".into(),
                "s".into(),
                true,
                CronCreateOptions {
                    max_fires: Some(2),
                    ..CronCreateOptions::default()
                },
            )
            .await
            .unwrap();
        // Backdate so the first pass coalesces ~3 periods
        let now = Utc::now();
        let backdate = now - Duration::minutes(16);
        {
            let mut tasks = manager.tasks.lock().unwrap();
            let task = tasks.get_mut(&id).unwrap();
            task.created_at = backdate;
            task.last_fired = Some(backdate);
            task.next_fire_at = Some(backdate + Duration::minutes(5));
        }

        let due = manager.check_due().await;

        // Then one merged fire advanced the count by exactly one
        assert_eq!(due.len(), 1);
        assert!(
            due[0].coalesced_count >= 3,
            "periods merged: {}",
            due[0].coalesced_count
        );
        assert_eq!(manager.get(&id).unwrap().fire_count, 1);
    }

    #[tokio::test]
    async fn should_recompute_next_fire_when_restoring_tasks() {
        let manager = CronManager::new();
        let task = persisted_task("cron-1", "*/5 * * * *", true);

        manager.restore_tasks([task.clone()]);

        // Then the next fire is rebuilt from the task's cursor
        // (created_at — the TS baseFromMs), never from the persisted
        // absolute: it is the first */5 occurrence after the cursor,
        // plus the deterministic per-id jitter (<= 30s for a */5 task).
        let restored = manager.get("cron-1").expect("task is restored");
        let next = restored
            .next_fire_at
            .expect("next fire is recomputed on restore");
        assert!(
            next != task.next_fire_at.unwrap(),
            "the stale persisted next_fire_at must not survive restore"
        );
        assert!(next > task.created_at, "the walk starts at the cursor");
        assert!(
            next <= task.created_at + Duration::minutes(5) + Duration::seconds(31),
            "the first */5 occurrence after the cursor (+ jitter): {next}"
        );
    }

    #[tokio::test]
    async fn should_keep_a_fired_one_shot_dormant_when_restoring_tasks() {
        let manager = CronManager::new();
        let mut fired = persisted_task("cron-1", "*/5 * * * *", false);
        fired.last_fired = Some(Utc::now() - Duration::hours(1));

        manager.restore_tasks([fired]);

        let task = manager.get("cron-1").expect("fired one-shot stays listed");
        assert!(task.last_fired.is_some(), "last_fired is preserved");
        assert!(
            task.next_fire_at.is_none(),
            "a fired one-shot is never scheduled again"
        );
        assert!(
            manager.check_due().await.is_empty(),
            "a fired one-shot does not re-fire after resume"
        );
    }

    #[tokio::test]
    async fn should_reschedule_an_unfired_one_shot_when_restoring_tasks() {
        let manager = CronManager::new();
        let task = persisted_task("cron-1", "*/5 * * * *", false);

        manager.restore_tasks([task.clone()]);

        let restored = manager.get("cron-1").expect("unfired one-shot is restored");
        assert!(restored.last_fired.is_none());
        // The schedule is rebuilt from the cursor (created_at): the first
        // */5 occurrence after it, with the one-shot jitter (which can
        // only pull a round minute earlier, bounded by created_at).
        let next = restored
            .next_fire_at
            .expect("an unfired one-shot is rescheduled");
        assert!(
            next != task.next_fire_at.unwrap(),
            "the stale persisted next_fire_at must not survive restore"
        );
        assert!(next > task.created_at, "the walk starts at the cursor");
        assert!(
            next <= task.created_at + Duration::minutes(5) + Duration::seconds(31),
            "the first */5 occurrence after the cursor: {next}"
        );
    }

    #[tokio::test]
    async fn should_skip_unschedulable_tasks_when_restoring_tasks() {
        let manager = CronManager::new();

        // February never has a 30th: never schedulable again.
        manager.restore_tasks([
            persisted_task("cron-1", "0 0 30 2 *", true),
            persisted_task("cron-2", "*/5 * * * *", true),
        ]);

        assert!(manager.get("cron-1").is_none(), "the dead task is skipped");
        assert!(manager.get("cron-2").is_some(), "the live task survives");
    }

    #[tokio::test]
    async fn should_record_no_wire_ops_when_restoring_tasks() {
        let (manager, tmp) = recording_manager().await;

        manager.restore_tasks([persisted_task("cron-1", "*/5 * * * *", true)]);

        let ops = recorded_ops(&tmp).await;
        assert!(
            ops.is_empty(),
            "restore must never re-append replayed ops: {ops:?}"
        );
    }

    // -----------------------------------------------------------------------
    // Wire DTO (F16): camelCase + epoch-millis cross-language shape
    // -----------------------------------------------------------------------

    #[test]
    fn should_serialize_a_task_to_the_camelcase_epoch_millis_wire_dto() {
        // Given a task in the domain shape (snake_case fields, chrono
        // timestamps)
        let task = CronTask {
            session_id: "s-1".to_string(),
            created_at: utc(2026, 9, 7, 10, 0),
            last_fired: Some(utc(2026, 9, 7, 10, 5)),
            next_fire_at: Some(utc(2026, 9, 7, 10, 10)),
            ..task_base("cron-1", "*/5 * * * *", "check the builds")
        };

        // When it is serialized for the wire
        let wire = cron_task_wire(&task);

        // Then the shape is the TS `CronTask` declaration (cronTask.ts /
        // wire-manifest.d.ts): camelCase keys, epoch-millis numbers, the
        // session id folded into `tags.sessionId` (CRON_SESSION_TAG), and
        // no absolute `nextFireAt` (both engines recompute it on restore).
        assert_eq!(wire["id"], "cron-1");
        assert_eq!(wire["cron"], "*/5 * * * *");
        assert_eq!(wire["prompt"], "check the builds");
        assert_eq!(wire["recurring"], true);
        assert_eq!(wire["createdAt"], utc(2026, 9, 7, 10, 0).timestamp_millis());
        assert_eq!(
            wire["lastFiredAt"],
            utc(2026, 9, 7, 10, 5).timestamp_millis()
        );
        assert_eq!(wire["tags"]["sessionId"], "s-1");
        assert!(
            wire.get("created_at").is_none()
                && wire.get("last_fired").is_none()
                && wire.get("next_fire_at").is_none()
                && wire.get("session_id").is_none()
                && wire.get("nextFireAt").is_none(),
            "the legacy snake_case keys must not leak: {wire}"
        );
    }

    #[test]
    fn should_omit_last_fired_at_when_the_task_has_never_fired() {
        let task = persisted_task("cron-2", "*/5 * * * *", true);
        let wire = cron_task_wire(&task);
        assert!(
            wire.get("lastFiredAt").is_none(),
            "an unfired task has no lastFiredAt key: {wire}"
        );
    }

    #[test]
    fn should_round_trip_a_task_through_the_wire_dto() {
        let task = CronTask {
            session_id: "s-2".to_string(),
            recurring: false,
            created_at: utc(2026, 9, 7, 9, 0),
            ..task_base("cron-3", "0 9 * * 1-5", "weekday standup")
        };

        let parsed = cron_task_from_wire(&cron_task_wire(&task)).expect("round trip parses");

        assert_eq!(parsed.id, "cron-3");
        assert_eq!(parsed.cron, "0 9 * * 1-5");
        assert_eq!(parsed.prompt, "weekday standup");
        assert_eq!(parsed.session_id, "s-2");
        assert!(!parsed.recurring);
        assert_eq!(parsed.created_at, utc(2026, 9, 7, 9, 0));
        assert_eq!(parsed.last_fired, None);
    }

    #[test]
    fn should_parse_the_legacy_snake_case_rfc3339_shape_when_reading_old_logs() {
        // Given a task record in the pre-T6 rust shape (the CronTask serde
        // form: snake_case, RFC 3339 strings) — a log written before the
        // wire DTO that was never migrated
        let legacy = serde_json::json!({
            "id": "cron-legacy",
            "cron": "*/5 * * * *",
            "prompt": "old prompt",
            "session_id": "s-old",
            "recurring": true,
            "created_at": "2026-09-07T10:00:00Z",
            "last_fired": "2026-09-07T10:05:00Z",
            "next_fire_at": "2026-09-07T10:10:00Z",
        });

        // When it is parsed
        let parsed = cron_task_from_wire(&legacy).expect("the legacy shape must parse");

        // Then the domain task is reconstructed with the timestamps
        // converted back
        assert_eq!(parsed.id, "cron-legacy");
        assert_eq!(parsed.session_id, "s-old");
        assert_eq!(parsed.created_at, utc(2026, 9, 7, 10, 0));
        assert_eq!(parsed.last_fired, Some(utc(2026, 9, 7, 10, 5)));
    }

    #[test]
    fn should_parse_the_ts_authored_camel_case_shape_when_reading_ts_logs() {
        // Given a task record authored by the TS kernel (cronTask.ts —
        // camelCase, epoch millis, tags, no session_id)
        let ts_record = serde_json::json!({
            "id": "cron-ts",
            "cron": "0 * * * *",
            "prompt": "ts prompt",
            "createdAt": 1_773_000_000_000u64,
            "recurring": false,
            "lastFiredAt": 1_773_000_600_000u64,
            "tags": { "sessionId": "s-ts" },
        });

        let parsed = cron_task_from_wire(&ts_record).expect("the TS shape must parse");

        assert_eq!(parsed.id, "cron-ts");
        assert!(!parsed.recurring);
        assert_eq!(parsed.session_id, "s-ts");
        assert_eq!(parsed.created_at.timestamp_millis(), 1_773_000_000_000i64);
        assert_eq!(
            parsed.last_fired.map(|at| at.timestamp_millis()),
            Some(1_773_000_600_000)
        );
    }

    #[test]
    fn should_return_none_when_the_record_is_not_a_task() {
        assert!(cron_task_from_wire(&serde_json::json!({"no": "id"})).is_none());
        assert!(cron_task_from_wire(&serde_json::Value::Null).is_none());
    }

    // -----------------------------------------------------------------------
    // wire DTO: the timezone key (B5 T4)
    // -----------------------------------------------------------------------

    #[test]
    fn should_serialize_timezone_to_the_wire_dto_when_present() {
        let mut task = persisted_task("cron-tz", "0 9 * * *", true);
        task.timezone = Some("Asia/Shanghai".to_string());

        let wire = cron_task_wire(&task);

        assert_eq!(wire["timezone"], "Asia/Shanghai");
        // 不持久化绝对 nextFireAt 的既有契约保持
        assert!(wire.get("nextFireAt").is_none());
    }

    #[test]
    fn should_omit_timezone_key_when_the_task_has_no_explicit_timezone() {
        let task = persisted_task("cron-tz", "0 9 * * *", true);
        let wire = cron_task_wire(&task);
        assert!(wire.get("timezone").is_none(), "absent means local: {wire}");
    }

    #[test]
    fn should_round_trip_timezone_through_the_wire_dto() {
        let mut task = persisted_task("cron-tz", "0 9 * * *", true);
        task.timezone = Some("America/New_York".to_string());

        let parsed = cron_task_from_wire(&cron_task_wire(&task)).expect("round trip parses");

        assert_eq!(parsed.timezone.as_deref(), Some("America/New_York"));
    }

    #[test]
    fn should_parse_timezone_from_a_legacy_record_when_reading_old_logs() {
        // legacy snake_case rust 形状（timezone 一词两形状同拼写）
        let legacy = serde_json::json!({
            "id": "cron-legacy-tz",
            "cron": "*/5 * * * *",
            "prompt": "old",
            "session_id": "s-old",
            "recurring": true,
            "created_at": "2026-09-07T10:00:00Z",
            "timezone": "Europe/Berlin",
        });
        let parsed = cron_task_from_wire(&legacy).expect("the legacy shape must parse");
        assert_eq!(parsed.timezone.as_deref(), Some("Europe/Berlin"));
    }

    #[test]
    fn should_default_timezone_to_none_when_reading_a_record_without_it() {
        // 旧 Rust（UTC 时代）与 TS（本地时代）记录都没有该键 → None = 本地。
        let ts_record = serde_json::json!({
            "id": "cron-ts",
            "cron": "0 * * * *",
            "prompt": "ts",
            "createdAt": 1_773_000_000_000u64,
            "recurring": false,
            "tags": { "sessionId": "s-ts" },
        });
        let parsed = cron_task_from_wire(&ts_record).expect("the TS shape must parse");
        assert_eq!(parsed.timezone, None);
    }
}
