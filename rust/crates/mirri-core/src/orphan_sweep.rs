//! Startup orphan sweep (ADR-012 c/d, US14.2/14.3).
//!
//! The sweep runs once at service startup: it walks every workspace's
//! every session's task archives under the sessions root, picks the
//! process-kind [`Running`](crate::task::TaskStatus::Running) records
//! left behind by previous service instances, and verifies each one
//! against the live process table via its
//! [`ProcessFingerprint`](mirri_os::ProcessFingerprint). The verdict
//! tree has three branches:
//!
//! - the fingerprint's process is still alive and start-tick-verified →
//!   [`SweepVerdict::LiveOrphan`] (a genuine orphan left by a crash);
//! - the process is gone but its process group still has members →
//!   [`SweepVerdict::GroupResidue`] (the leader died, descendants linger);
//! - the process is gone and the group is empty → the archive is stale
//!   (either a clean death — [`SweepVerdict::Dead`] — or a pid that has
//!   since been recycled to an unrelated process —
//!   [`SweepVerdict::ReconciledDead`]); both are settled on disk so the
//!   next sweep never re-reports them.
//!
//! Archives that cannot be judged — no fingerprint (legacy `!`-path
//! bash tasks), or a live pid whose start ticks cannot be confirmed
//! (fingerprint `0`, an unreadable probe, or a probe failure) — are
//! [`SweepVerdict::Undeterminable`]: reported, never touched (neither
//! process nor archive), the existing load-time reconcile path settles
//! them when the session opens.
//!
//! Disposal is opt-in via
//! [`OrphanDisposePolicy::ReportAndTerminate`](crate::config::OrphanDisposePolicy::ReportAndTerminate);
//! the default policy is report-only. Under the terminate policy both
//! the live orphan and the group residue are disposed, each guarded by
//! its own re-verification (fingerprint identity for the live leader,
//! group membership inside `terminate_group`). Every sweep writes a
//! census report under
//! `<sessions_root>/orphan-sweep/<UTC timestamp>.json`
//! (US14.6 visibility).
//!
//! The sweep is a synchronous pure-function walk (directory traversal +
//! process probes); the async wrapper lives in the embedded assembly.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use mirri_os::SyncFs;
use mirri_os::probe_process;
use mirri_os::process::{ProcessFingerprint, ProcessProbe, list_group_members, terminate_group};
use serde::{Deserialize, Serialize};

use crate::config::OrphanDisposePolicy;
use crate::task::{AgentTask, TaskKind, TaskStatus};
use crate::task_persist::TaskPersistence;

/// Annotation written into a settled archive's output stream when the
/// sweep disposes of a verified live orphan.
pub const DISPOSAL_ANNOTATION: &str = "orphan-sweep disposal";

/// One candidate's census verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SweepVerdict {
    /// The fingerprint's process is alive and start-tick-verified: a
    /// genuine orphan left behind by a previous service instance.
    LiveOrphan,
    /// The leader is gone but its process group still has live members
    /// (descendants the leader never reaped or killed).
    GroupResidue,
    /// The process is gone and the group is empty — a clean death whose
    /// archive simply never settled.
    Dead,
    /// The pid now belongs to a different process (start ticks differ or
    /// are unreadable): pid recycling, never an orphan.
    ReconciledDead,
    /// No fingerprint on the archive (legacy `!`-path bash tasks): the
    /// sweep cannot judge, so it only reports.
    Undeterminable,
}

/// One archive's sweep outcome, as archived in the census report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SweepEntry {
    pub task_id: String,
    pub description: String,
    /// The command line for process-kind tasks (`None` for pre-kind
    /// legacy archives).
    pub command: Option<String>,
    pub verdict: SweepVerdict,
    /// How long the archive says the task has been alive at census time
    /// (`scanned_at - created_at`, US14.2 已存活时长).
    pub age_secs: i64,
    /// Human-readable explanation: how the verdict was reached, what
    /// disposal (if any) was attempted, and whether it succeeded.
    pub note: String,
    /// The fingerprint the verdict was verified against (`None` for
    /// undeterminable archives).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<ProcessFingerprint>,
}

/// The result of one sweep run, archived as JSON under
/// `<sessions_root>/orphan-sweep/`.
#[derive(Debug, Serialize, Deserialize)]
pub struct SweepReport {
    /// The service instance epoch the sweep ran under (metadata only in
    /// this batch — it does not filter candidates).
    pub epoch: String,
    /// When the sweep started scanning (UTC).
    pub scanned_at: chrono::DateTime<chrono::Utc>,
    /// The disposal policy in effect during the sweep.
    pub policy: OrphanDisposePolicyArchive,
    /// One entry per candidate archive.
    pub entries: Vec<SweepEntry>,
}

/// The serde-friendly record of the policy in effect (the live enum has
/// no serde derives; the archive is a plain string).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrphanDisposePolicyArchive {
    ReportOnly,
    ReportAndTerminate,
}

impl From<&OrphanDisposePolicy> for OrphanDisposePolicyArchive {
    fn from(policy: &OrphanDisposePolicy) -> Self {
        match policy {
            OrphanDisposePolicy::ReportOnly => Self::ReportOnly,
            OrphanDisposePolicy::ReportAndTerminate => Self::ReportAndTerminate,
        }
    }
}

/// One candidate archive located on disk, with the directory it lives
/// in (needed to settle the record in place).
pub struct Candidate {
    pub task: AgentTask,
    pub tasks_dir: PathBuf,
}

/// Run the orphan census over every workspace's task archives.
///
/// See the [module docs](self) for the verdict-tree semantics and the
/// per-branch actions. The report is always archived under
/// `<sessions_root>/orphan-sweep/<UTC timestamp>.json`; the archive
/// write happens even when the sweep finds nothing (an empty census is
/// itself an answer). `current_epoch` is metadata in this batch: it is
/// recorded in the report for traceability and does not filter
/// candidates.
pub fn sweep_orphans(
    sessions_root: &Path,
    policy: OrphanDisposePolicy,
    current_epoch: &str,
) -> SweepReport {
    sweep_orphans_with_fs(sessions_root, policy, current_epoch, &local_sync_fs())
}

/// [`sweep_orphans`] with an injected filesystem provider (zero-IO T13b).
///
/// The engine default is the host-local [`LocalSyncFs`](mirri_os::LocalSyncFs);
/// a virtual host or a test double injects its own.
pub fn sweep_orphans_with_fs(
    sessions_root: &Path,
    policy: OrphanDisposePolicy,
    current_epoch: &str,
    fs: &Arc<dyn SyncFs>,
) -> SweepReport {
    let scanned_at = chrono::Utc::now();
    let mut entries = Vec::new();

    for Candidate { task, tasks_dir } in load_running_process_tasks_with_fs(sessions_root, fs) {
        let mut task = task;
        match task.fingerprint.clone() {
            // Legacy archive (e.g. `!`-path bash tasks from before
            // fingerprint capture): undeterminable, report-only always.
            // The record stays Running — the load-time reconcile path
            // settles it when its session opens.
            None => {
                entries.push(SweepEntry {
                    task_id: task.id.clone(),
                    description: task.description.clone(),
                    command: process_command(&task.kind).map(str::to_string),
                    verdict: SweepVerdict::Undeterminable,
                    age_secs: age_secs_at(&scanned_at, &task),
                    note: "no fingerprint on the archive; left for the \
                           load-time reconcile path"
                        .to_string(),
                    fingerprint: None,
                });
            }
            Some(fingerprint) => {
                let original = task.clone();
                let verdict = judge(&fingerprint);
                let note = act(&mut task, &verdict, &fingerprint, &policy);
                // Persist only when the sweep actually changed the
                // record (a live orphan under report-only stays
                // untouched on disk as well as in memory).
                if task != original
                    && let Err(error) = persist_task(&tasks_dir, &task)
                {
                    tracing::warn!(
                        task_id = %task.id,
                        %error,
                        "orphan sweep failed to settle a task archive"
                    );
                }
                entries.push(SweepEntry {
                    task_id: task.id.clone(),
                    description: task.description.clone(),
                    command: process_command(&task.kind).map(str::to_string),
                    verdict,
                    age_secs: age_secs_at(&scanned_at, &task),
                    note,
                    fingerprint: Some(fingerprint),
                });
            }
        }
    }

    let report = SweepReport {
        epoch: current_epoch.to_string(),
        scanned_at,
        policy: (&policy).into(),
        entries,
    };
    archive_report(sessions_root, &report, fs.as_ref());
    report
}

/// The sweep verdict tree for one fingerprint (ADR-012 c).
///
/// [`SweepVerdict::Undeterminable`] covers every shape the sweep cannot
/// judge — no fingerprint, an unreadable start tick, or a failed probe:
/// such archives are reported only and left for the load-time reconcile
/// path, never settled or signaled.
fn judge(fingerprint: &ProcessFingerprint) -> SweepVerdict {
    match probe_process(fingerprint.pid) {
        // Alive: only a start-tick match proves it is *the* process the
        // archive points at; a mismatch means the pid was recycled.
        Ok(probe) if probe.alive => match start_ticks_match(&probe, fingerprint) {
            StartTickMatch::Verified => SweepVerdict::LiveOrphan,
            StartTickMatch::Unverifiable => SweepVerdict::Undeterminable,
            StartTickMatch::Mismatch => SweepVerdict::ReconciledDead,
        },
        // Leader gone: check the group before declaring a silent death —
        // descendants left in the group are GroupResidue, never Dead.
        Ok(_) => {
            if list_group_members(fingerprint.pgid)
                .map(|members| !members.is_empty())
                .unwrap_or(false)
            {
                SweepVerdict::GroupResidue
            } else {
                SweepVerdict::Dead
            }
        }
        Err(_) => SweepVerdict::Undeterminable,
    }
}

/// Outcome of comparing a live probe's start ticks against the
/// fingerprint's.
enum StartTickMatch {
    /// The probe's start ticks equal the fingerprint's.
    Verified,
    /// The probe's start ticks are readable and differ: pid recycling.
    Mismatch,
    /// Start ticks are unreadable (fingerprint zero or probe `None`):
    /// identity cannot be confirmed.
    Unverifiable,
}

fn start_ticks_match(probe: &ProcessProbe, fingerprint: &ProcessFingerprint) -> StartTickMatch {
    let Some(live_ticks) = probe.start_ticks else {
        return StartTickMatch::Unverifiable;
    };
    if fingerprint.start_ticks == 0 {
        // A zero fingerprint can never be verified against a live probe.
        return StartTickMatch::Unverifiable;
    }
    if live_ticks == fingerprint.start_ticks {
        StartTickMatch::Verified
    } else {
        StartTickMatch::Mismatch
    }
}

/// Apply the per-verdict actions to the task record and return the
/// human-readable note.
///
/// - `LiveOrphan`: under `ReportAndTerminate`, re-verify the
///   fingerprint identity (pid alive + start ticks still match —
///   ADR-012 e pre-termination re-verification) and terminate the
///   group; on success the archive settles `Stopped` with the disposal
///   annotation (failed disposal keeps `Running` and the note records
///   `dispose_failed`). Under `ReportOnly` the process and the archive
///   are both left untouched.
/// - `GroupResidue`: the leader is gone, so the archive settles
///   `Stopped` under both policies (settling a lost record is archive
///   reconciliation, not disposal — SDR-013 b branch ②); the live
///   residue is terminated only under `ReportAndTerminate` (ADR-012 e:
///   a dead leader is no exemption — `terminate_group` itself
///   re-verifies the group still has members before signaling).
/// - `Dead` / `ReconciledDead`: the archive settles `Stopped` under
///   both policies (SDR-013 b branch ③); no live process to signal.
/// - `Undeterminable`: report-only no-op — neither the archive nor any
///   process is touched; the load-time reconcile path owns such
///   records.
fn act(
    task: &mut AgentTask,
    verdict: &SweepVerdict,
    fingerprint: &ProcessFingerprint,
    policy: &OrphanDisposePolicy,
) -> String {
    match verdict {
        SweepVerdict::LiveOrphan => match policy {
            OrphanDisposePolicy::ReportOnly => {
                "live orphan verified by fingerprint; report-only policy \
                     leaves the process running"
                    .to_string()
            }
            OrphanDisposePolicy::ReportAndTerminate => match dispose_group(fingerprint) {
                Ok(()) => {
                    settle_stopped(task, DISPOSAL_ANNOTATION);
                    format!(
                        "live orphan verified by fingerprint; group \
                                 terminated ({DISPOSAL_ANNOTATION})"
                    )
                }
                Err(error) => format!(
                    "live orphan verified by fingerprint; \
                             dispose_failed: {error}"
                ),
            },
        },
        SweepVerdict::GroupResidue => {
            let members = list_group_members(fingerprint.pgid).unwrap_or_default();
            match policy {
                OrphanDisposePolicy::ReportOnly => {
                    settle_stopped(task, "orphan-sweep reconcile: leader gone, group residue");
                    format!(
                        "leader gone but process group still has live members \
                         ({members:?}); archive reconciled, residue left running \
                         (report-only policy)"
                    )
                }
                OrphanDisposePolicy::ReportAndTerminate => {
                    match terminate_group(fingerprint.pgid) {
                        Ok(()) => {
                            settle_stopped(
                                task,
                                "orphan-sweep disposal: leader gone, residue group terminated",
                            );
                            format!(
                                "leader gone but process group still had live \
                                 members ({members:?}); residue group terminated \
                                 ({DISPOSAL_ANNOTATION})"
                            )
                        }
                        Err(error) => {
                            settle_stopped(
                                task,
                                "orphan-sweep reconcile: leader gone, group residue",
                            );
                            format!(
                                "leader gone but process group still has live \
                                 members ({members:?}); archive reconciled, \
                                 residue dispose_failed: {error}"
                            )
                        }
                    }
                }
            }
        }
        SweepVerdict::Dead => {
            settle_stopped(task, "orphan-sweep reconcile: process gone, group empty");
            "process gone and group empty; archive reconciled".to_string()
        }
        SweepVerdict::ReconciledDead => {
            settle_stopped(task, "orphan-sweep reconcile: pid identity mismatch");
            "pid recycled to an unrelated live process; \
             archive reconciled, live pid untouched"
                .to_string()
        }
        SweepVerdict::Undeterminable => {
            "cannot judge liveness (no readable start ticks or probe failed); \
             left for the load-time reconcile path"
                .to_string()
        }
    }
}

/// Dispose of a verified live orphan's process group, with the
/// ADR-012 e pre-termination fingerprint re-verification on the sweep's
/// side of the API boundary: the pid must still be alive *and* still
/// start-tick-match the fingerprint before the group signal goes out
/// (between the verdict probe and here, the leader could have died and
/// the pid been recycled — `terminate_group`'s own re-verification is
/// group membership, not identity; identity is the sweep's job).
fn dispose_group(fingerprint: &ProcessFingerprint) -> std::io::Result<()> {
    let probe = probe_process(fingerprint.pid)?;
    if !probe.alive
        || !matches!(
            start_ticks_match(&probe, fingerprint),
            StartTickMatch::Verified
        )
    {
        return Err(std::io::Error::other(
            "pre-termination fingerprint re-verification failed \
             (pid gone or identity no longer matches)",
        ));
    }
    terminate_group(fingerprint.pgid)
}

/// The annotation the lifetime sweeper (ADR-012 f) appends to a task's
/// output stream when it settles an expired archive.
pub const LIFETIME_CEILING_ANNOTATION: &str = "lifetime ceiling reached; task terminated";

/// Whether `task` is past the lifetime ceiling `max_lifetime`.
///
/// The zero duration is the unbounded sentinel
/// ([`ServerConfig::resolve_max_task_lifetime`](crate::ServerConfig::resolve_max_task_lifetime)):
/// nothing is ever expired under it. Expiry is judged from the archive's
/// `created_at` wall clock; a future `created_at` (clock skew) never expires.
///
/// This predicate is purely time-based by design: the exemption check
/// (`AgentTask::lifetime_exempt` — the explicit per-task exemption and the
/// keepAliveOnExit default exemption, SDR-013 d) is the *consumer's*
/// composition, so the startup census and the resident sweeper compose the
/// same two-part check and never drift.
pub fn task_is_lifetime_expired(task: &AgentTask, max_lifetime: std::time::Duration) -> bool {
    if max_lifetime.is_zero() {
        return false;
    }
    chrono::Utc::now()
        .signed_duration_since(task.created_at)
        .to_std()
        .is_ok_and(|elapsed| elapsed >= max_lifetime)
}

/// Terminate one lifetime-expired task's process group and settle its
/// archive — the narrow shared entry the embedded lifetime sweeper
/// (ADR-012 f) consumes, so the sweep's verified termination semantics
/// (fingerprint re-verification before the group signal) and its
/// settle annotation are written exactly once.
///
/// The pre-termination re-verification rides [`dispose_group`]: a task
/// whose leader already died (or whose pid was recycled) fails the
/// identity check and the record stays `Running` for the load-time
/// reconcile path — the sweeper never settles a task it could not
/// verifiably signal. On success the record settles `Stopped` with the
/// [`LIFETIME_CEILING_ANNOTATION`] note and is re-persisted through the
/// same atomic writer the manager uses; `tasks_dir` is the archive's
/// directory (see [`load_running_process_tasks`]'s layout).
pub fn terminate_expired_task(
    task: &mut AgentTask,
    tasks_dir: &Path,
) -> Result<(), std::io::Error> {
    let Some(ref fingerprint) = task.fingerprint else {
        return Err(std::io::Error::other(
            "no fingerprint on the archive; left for the load-time reconcile path",
        ));
    };
    dispose_group(fingerprint)?;
    settle_stopped(task, LIFETIME_CEILING_ANNOTATION);
    persist_task(tasks_dir, task)
}

/// Settle an archive as `Stopped` at the given timestamp, appending the
/// annotation to its output stream.
fn settle_stopped(task: &mut AgentTask, annotation: &str) {
    task.status = TaskStatus::Stopped;
    task.ended_at = Some(chrono::Utc::now());
    if !task.output.is_empty() {
        task.output.push('\n');
    }
    task.output.push_str(annotation);
}

/// Re-persist a settled task through the same atomic writer the
/// manager uses, so the next load (or sweep) sees the settled state.
fn persist_task(tasks_dir: &Path, task: &AgentTask) -> std::io::Result<()> {
    let json = serde_json::to_string_pretty(task)
        .map_err(|e| std::io::Error::other(format!("cannot serialize task: {e}")))?;
    let path = tasks_dir.join(format!("{}.json", task.id));
    mirri_persistence::write_file_atomically(&path, json.as_bytes())
}

/// Walk `<sessions_root>/*/*/tasks/` and return every running
/// process-kind task archive found, each stamped with the `tasks`
/// directory it was read from (the sweep settles records in place
/// through it). Shared by the startup census and the embedded lifetime
/// sweeper — one walk, one candidate definition.
pub fn load_running_process_tasks(sessions_root: &Path) -> Vec<Candidate> {
    load_running_process_tasks_with_fs(sessions_root, &local_sync_fs())
}

/// [`load_running_process_tasks`] with an injected filesystem provider
/// (zero-IO T13b). Directory traversal and task loading both go through
/// `fs`, so a virtual host walks the same archive layout the engine
/// writes. Content writes stay on the shared atomic persistence writer —
/// the `TaskPersistence::save_task` split; see [`archive_report`].
pub fn load_running_process_tasks_with_fs(
    sessions_root: &Path,
    fs: &Arc<dyn SyncFs>,
) -> Vec<Candidate> {
    let mut candidates = Vec::new();
    let workspaces = match fs.iterdir(sessions_root) {
        Ok(entries) => entries,
        Err(error) => {
            // An unreadable sessions root must never masquerade as a
            // clean empty census.
            tracing::warn!(
                %error,
                sessions_root = %sessions_root.display(),
                "orphan sweep cannot read the sessions root"
            );
            return candidates;
        }
    };
    for workspace in workspaces.into_iter().filter(|entry| entry.is_dir) {
        let workspace_path = PathBuf::from(&workspace.path);
        let sessions = match fs.iterdir(&workspace_path) {
            Ok(entries) => entries,
            Err(error) => {
                tracing::warn!(
                    %error,
                    workspace = %workspace_path.display(),
                    "orphan sweep cannot read a workspace directory"
                );
                continue;
            }
        };
        for session in sessions.into_iter().filter(|entry| entry.is_dir) {
            let tasks_dir = PathBuf::from(&session.path).join("tasks");
            // `exists` follows symlinks like the `Path::is_dir` it replaces,
            // so a symlinked `tasks` directory is still walked. A non-directory
            // entry falls through to `load_tasks` and contributes nothing.
            if !fs.exists(&tasks_dir) {
                continue;
            }
            let persistence = TaskPersistence::new(tasks_dir.clone(), Arc::clone(fs));
            for task in persistence.load_tasks().unwrap_or_default() {
                if matches!(task.status, TaskStatus::Running)
                    && matches!(task.kind, TaskKind::Process { .. })
                {
                    candidates.push(Candidate {
                        task,
                        tasks_dir: tasks_dir.clone(),
                    });
                }
            }
        }
    }
    candidates
}

/// The engine-default filesystem provider for the sweep's entry points.
fn local_sync_fs() -> Arc<dyn SyncFs> {
    Arc::new(mirri_os::LocalSyncFs::new())
}

/// The command line of a process-kind task, if any.
fn process_command(kind: &TaskKind) -> Option<&str> {
    match kind {
        TaskKind::Process { command, .. } => command.as_deref(),
        _ => None,
    }
}

/// How long the task has been alive at census time: whole seconds from
/// the archive's `created_at` to the sweep's `scanned_at` (US14.2
/// 已存活时长). A `created_at` in the future (clock skew) clamps to 0 —
/// a census never reports a negative age.
fn age_secs_at(scanned_at: &chrono::DateTime<chrono::Utc>, task: &AgentTask) -> i64 {
    scanned_at
        .signed_duration_since(task.created_at)
        .num_seconds()
        .max(0)
}

/// Write the census report to
/// `<sessions_root>/orphan-sweep/<UTC timestamp>.json`.
///
/// The directory is created through the injected provider; the report
/// body rides the shared atomic persistence writer — the same
/// mkdir-through-provider / write-through-atomic-writer split
/// [`TaskPersistence::save_task`] uses for task archives. A failure to
/// archive is logged and swallowed — the in-memory report is still
/// returned to the caller.
fn archive_report(sessions_root: &Path, report: &SweepReport, fs: &dyn SyncFs) {
    let dir = sessions_root.join("orphan-sweep");
    if let Err(error) = fs.mkdir(&dir, true) {
        tracing::warn!(%error, dir = %dir.display(), "orphan sweep cannot create report dir");
        return;
    }
    let path = dir.join(format!(
        "{}.json",
        report.scanned_at.format("%Y%m%dT%H%M%S%3fZ")
    ));
    match serde_json::to_string_pretty(report) {
        Ok(json) => {
            if let Err(error) = mirri_persistence::write_file_atomically(&path, json.as_bytes()) {
                tracing::warn!(%error, path = %path.display(), "orphan sweep cannot archive report");
            }
        }
        Err(error) => {
            tracing::warn!(%error, "orphan sweep cannot serialize report");
        }
    }
}

// The whole module spawns real process-group leaders (Unix
// `process_group(0)`) and probes them through the Unix-only start-tick
// readers, so it cannot compile — let alone pass — off Unix.
#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    use std::sync::Arc;

    /// A 60-second sleep gives every test ample headroom on slow CI.
    const SLEEP_SECONDS: &str = "60";

    /// A spawned long-running group leader. Dropping the guard kills
    /// the whole group, so a failing assertion can never leak a child.
    struct SpawnedGroup {
        child: std::process::Child,
        pid: i32,
        pgid: i32,
    }

    impl SpawnedGroup {
        fn wait(&mut self) {
            let _ = self.child.wait();
        }
    }

    impl Drop for SpawnedGroup {
        fn drop(&mut self) {
            let _ = terminate_group(self.pgid);
            let _ = self.child.wait();
        }
    }

    /// Spawn `program args...` as its own process-group leader.
    fn spawn_leader(program: &str, args: &[&str]) -> SpawnedGroup {
        let child = Command::new(program)
            .args(args)
            .process_group(0)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn test child");
        let pid = child.id() as i32;
        SpawnedGroup {
            child,
            pid,
            pgid: pid,
        }
    }

    /// Spawn `sh -c script` as a group leader and wait for it to exit,
    /// leaving descendants in the group behind.
    fn spawn_exiting_leader(script: &str) -> i32 {
        let mut leader = Command::new("sh")
            .arg("-c")
            .arg(script)
            .process_group(0)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn test leader");
        let pgid = leader.id() as i32;
        let status = leader.wait().expect("wait for the test leader");
        assert!(status.success(), "test leader script must exit cleanly");
        pgid
    }

    /// Poll until `predicate` holds or ~5 s elapse.
    fn eventually<T>(mut predicate: impl FnMut() -> Option<T>) -> Option<T> {
        for _ in 0..50 {
            if let Some(value) = predicate() {
                return Some(value);
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        predicate()
    }

    /// Wait until the process group `pgid` has at least one live member.
    fn wait_for_group_member(pgid: i32) {
        assert!(
            eventually(|| {
                list_group_members(pgid)
                    .ok()
                    .and_then(|m| m.first().map(|_| ()))
            })
            .is_some(),
            "test setup: group must gain a live member"
        );
    }

    fn probe_fingerprint(pid: i32, pgid: i32) -> ProcessFingerprint {
        let start_ticks = probe_process(pid)
            .expect("probe the test child")
            .start_ticks
            .expect("start ticks are readable on unix");
        ProcessFingerprint {
            pid,
            pgid,
            start_ticks,
        }
    }

    fn archive_task(id: &str, fingerprint: Option<ProcessFingerprint>) -> AgentTask {
        AgentTask {
            id: id.to_string(),
            kind: TaskKind::Process {
                command: Some("sleep 60".to_string()),
                dispatcher: None,
            },
            description: "sweep test task".to_string(),
            status: TaskStatus::Running,
            created_at: chrono::Utc::now(),
            ended_at: None,
            output: String::new(),
            session_id: "session-x".to_string(),
            deadline: None,
            detached: true,
            notification_suppressed: false,
            notification_delivered_at: None,
            fingerprint,
            lifetime_exempt: false,
        }
    }

    /// A sessions-root fixture with one archive on disk under
    /// `wd_test/session-x/tasks/`, plus the task's JSON path for
    /// re-reading after the sweep.
    struct Fixture {
        _tmp: tempfile::TempDir,
        sessions_root: std::path::PathBuf,
        task_json: std::path::PathBuf,
    }

    fn fixture_with_task(task: &AgentTask) -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let tasks_dir = tmp
            .path()
            .join("wd_test")
            .join(&task.session_id)
            .join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        let persistence =
            TaskPersistence::new(tasks_dir.clone(), Arc::new(mirri_os::LocalSyncFs::new()));
        persistence.save_task(task).unwrap();
        let task_json = tasks_dir.join(format!("{}.json", task.id));
        Fixture {
            sessions_root: tmp.path().to_path_buf(),
            _tmp: tmp,
            task_json,
        }
    }

    fn read_archive(path: &Path) -> AgentTask {
        serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
    }

    fn find_report_file(sessions_root: &Path) -> std::path::PathBuf {
        let dir = sessions_root.join("orphan-sweep");
        let mut files: Vec<_> = fs::read_dir(&dir)
            .unwrap_or_else(|e| panic!("report dir must exist: {e}"))
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "json"))
            .collect();
        files.sort();
        assert_eq!(files.len(), 1, "exactly one census report must be archived");
        files.pop().unwrap()
    }

    fn entry_of<'a>(report: &'a SweepReport, id: &str) -> &'a SweepEntry {
        report
            .entries
            .iter()
            .find(|e| e.task_id == id)
            .unwrap_or_else(|| panic!("sweep must produce an entry for {id}"))
    }

    #[test]
    fn should_report_a_live_orphan_when_fingerprint_matches_a_live_process() {
        // Given a live process-group leader and a Running archive whose
        // fingerprint truly points at it
        let spawned = spawn_leader("sleep", &[SLEEP_SECONDS]);
        let fp = probe_fingerprint(spawned.pid, spawned.pgid);
        let task = archive_task("task-live", Some(fp));
        let fixture = fixture_with_task(&task);

        // When the sweep runs under the default report-only policy
        let report = sweep_orphans(
            &fixture.sessions_root,
            OrphanDisposePolicy::ReportOnly,
            "epoch-2",
        );

        // Then the census reports a live orphan...
        let entry = entry_of(&report, "task-live");
        assert_eq!(entry.verdict, SweepVerdict::LiveOrphan);
        // ...with the archive's alive duration recorded (US14.2).
        assert!(
            entry.age_secs >= 0,
            "age_secs must be a non-negative whole-second age, got {}",
            entry.age_secs
        );
        assert!(entry.note.contains("report-only"), "note: {}", entry.note);
        // ...and did NOT touch the process or the archive.
        let probe = probe_process(spawned.pid).unwrap();
        assert!(probe.alive, "ReportOnly must leave the orphan running");
        assert!(
            matches!(read_archive(&fixture.task_json).status, TaskStatus::Running),
            "a live orphan under ReportOnly keeps its Running archive"
        );
        // And the census report is archived with the epoch metadata.
        let report_path = find_report_file(&fixture.sessions_root);
        let archived: SweepReport =
            serde_json::from_str(&fs::read_to_string(report_path).unwrap()).unwrap();
        assert_eq!(archived.epoch, "epoch-2");
        assert_eq!(archived.policy, OrphanDisposePolicyArchive::ReportOnly);
        assert_eq!(archived.entries.len(), 1);
        drop(spawned);
    }

    #[test]
    fn should_not_signal_anything_when_policy_is_report_only() {
        // Given a live orphan under the report-only policy
        let spawned = spawn_leader("sleep", &[SLEEP_SECONDS]);
        let fp = probe_fingerprint(spawned.pid, spawned.pgid);
        let task = archive_task("task-report", Some(fp));
        let fixture = fixture_with_task(&task);

        // When the sweep runs
        let report = sweep_orphans(
            &fixture.sessions_root,
            OrphanDisposePolicy::ReportOnly,
            "epoch-2",
        );

        // Then the child is still alive afterwards (no signal was sent)
        let entry = entry_of(&report, "task-report");
        assert_eq!(entry.verdict, SweepVerdict::LiveOrphan);
        assert!(probe_process(spawned.pid).unwrap().alive);
        // And no terminal state leaked into the archive.
        let on_disk = read_archive(&fixture.task_json);
        assert!(matches!(on_disk.status, TaskStatus::Running));
        assert_eq!(on_disk.ended_at, None);
        drop(spawned);
    }

    #[test]
    fn should_terminate_a_verified_orphan_when_policy_is_report_and_terminate() {
        // Given a live orphan under the terminate policy
        let mut spawned = spawn_leader("sleep", &[SLEEP_SECONDS]);
        let fp = probe_fingerprint(spawned.pid, spawned.pgid);
        let task = archive_task("task-term", Some(fp));
        let fixture = fixture_with_task(&task);

        // When the sweep runs with ReportAndTerminate
        let report = sweep_orphans(
            &fixture.sessions_root,
            OrphanDisposePolicy::ReportAndTerminate,
            "epoch-2",
        );

        // Then the group is gone, the report records the disposal, and
        // the archive settled Stopped with the disposal annotation.
        let entry = entry_of(&report, "task-term");
        assert_eq!(entry.verdict, SweepVerdict::LiveOrphan);
        assert!(
            entry.note.contains(DISPOSAL_ANNOTATION),
            "note: {}",
            entry.note
        );
        assert!(
            eventually(|| probe_process(spawned.pid)
                .ok()
                .filter(|p| !p.alive)
                .map(|_| ()))
            .is_some(),
            "the orphan's process group must be terminated"
        );
        let on_disk = read_archive(&fixture.task_json);
        assert!(matches!(on_disk.status, TaskStatus::Stopped));
        assert!(on_disk.ended_at.is_some());
        assert!(on_disk.output.contains(DISPOSAL_ANNOTATION));
        spawned.wait();
    }

    #[test]
    fn should_report_group_residue_when_the_leader_died_but_descendants_live() {
        // Given a leader that already exited, leaving a live descendant
        // in its process group, and an archive pointing at the dead
        // leader's fingerprint
        let pgid = spawn_exiting_leader(&format!("sleep {SLEEP_SECONDS} & exec sleep 1"));
        wait_for_group_member(pgid);
        let leader_pid = pgid; // the leader was also the group leader
        let fp = ProcessFingerprint {
            pid: leader_pid,
            pgid,
            start_ticks: 1,
        };
        let task = archive_task("task-residue", Some(fp));
        let fixture = fixture_with_task(&task);

        // When the sweep runs
        let report = sweep_orphans(
            &fixture.sessions_root,
            OrphanDisposePolicy::ReportOnly,
            "epoch-2",
        );

        // Then the census reports group residue with the live member
        // listed — never a silent Dead — and the archive settles Stopped.
        let entry = entry_of(&report, "task-residue");
        assert_eq!(entry.verdict, SweepVerdict::GroupResidue);
        assert!(
            !entry.note.contains("()"),
            "note must list live members: {}",
            entry.note
        );
        let on_disk = read_archive(&fixture.task_json);
        assert!(matches!(on_disk.status, TaskStatus::Stopped));
        assert!(on_disk.ended_at.is_some());
        // And the residue itself is left running (report-only).
        assert!(!list_group_members(pgid).unwrap().is_empty());
        // Cleanup: kill the leftover descendant.
        let _ = terminate_group(pgid);
    }

    #[test]
    fn should_reconcile_a_dead_process_archive_when_fingerprint_matches_no_live_process() {
        // Given an archive whose fingerprint points at a long-gone
        // process and whose process group is empty
        let pgid = spawn_exiting_leader(&format!("sleep {SLEEP_SECONDS} & exec sleep 1"));
        let _ = terminate_group(pgid); // kill the residue so the group is empty
        assert!(list_group_members(pgid).unwrap().is_empty());
        let fp = ProcessFingerprint {
            pid: pgid,
            pgid,
            start_ticks: 1,
        };
        let task = archive_task("task-dead", Some(fp));
        let fixture = fixture_with_task(&task);

        // When the sweep runs
        let report = sweep_orphans(
            &fixture.sessions_root,
            OrphanDisposePolicy::ReportOnly,
            "epoch-2",
        );

        // Then the archive is reconciled as Stopped with the sweep
        // annotation, and a second sweep reports nothing.
        let entry = entry_of(&report, "task-dead");
        assert_eq!(entry.verdict, SweepVerdict::Dead);
        let on_disk = read_archive(&fixture.task_json);
        assert!(matches!(on_disk.status, TaskStatus::Stopped));
        assert!(on_disk.output.contains("orphan-sweep reconcile"));
        let second = sweep_orphans(
            &fixture.sessions_root,
            OrphanDisposePolicy::ReportOnly,
            "epoch-2",
        );
        assert!(
            second.entries.is_empty(),
            "a settled archive must not be re-reported"
        );
        assert!(
            find_report_files(&fixture.sessions_root).len() >= 2,
            "every sweep archives its census, even a quiet one"
        );
    }

    fn find_report_files(sessions_root: &Path) -> Vec<std::path::PathBuf> {
        fs::read_dir(sessions_root.join("orphan-sweep"))
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "json"))
            .collect()
    }

    #[test]
    fn should_refuse_to_treat_a_recycled_pid_as_an_orphan() {
        // Given a live process and an archive whose fingerprint carries
        // that pid but start ticks from a different epoch
        let spawned = spawn_leader("sleep", &[SLEEP_SECONDS]);
        let live = probe_fingerprint(spawned.pid, spawned.pgid);
        let mismatched = ProcessFingerprint {
            pid: live.pid,
            pgid: live.pgid,
            start_ticks: live.start_ticks.saturating_add(1),
        };
        let task = archive_task("task-recycled", Some(mismatched));
        let fixture = fixture_with_task(&task);

        // When the sweep runs under the terminate policy
        let report = sweep_orphans(
            &fixture.sessions_root,
            OrphanDisposePolicy::ReportAndTerminate,
            "epoch-2",
        );

        // Then the verdict is ReconciledDead — never LiveOrphan — and
        // the live (unrelated) process is untouched despite the
        // terminate policy.
        let entry = entry_of(&report, "task-recycled");
        assert_eq!(entry.verdict, SweepVerdict::ReconciledDead);
        assert!(
            probe_process(spawned.pid).unwrap().alive,
            "a recycled pid must never be signaled"
        );
        assert!(
            !list_group_members(spawned.pgid).unwrap().is_empty(),
            "the unrelated live group must still have its member"
        );
        // And the archive settled Stopped (reconciliation, not disposal).
        let on_disk = read_archive(&fixture.task_json);
        assert!(matches!(on_disk.status, TaskStatus::Stopped));
        drop(spawned);
    }

    #[test]
    fn should_report_an_undeterminable_verdict_and_not_touch_the_archive_when_there_is_no_fingerprint()
     {
        // Given a legacy Running archive without a fingerprint (the
        // `!`-path bash-task shape from before fingerprint capture)
        let task = archive_task("task-legacy", None);
        let fixture = fixture_with_task(&task);

        // When the sweep runs
        let report = sweep_orphans(
            &fixture.sessions_root,
            OrphanDisposePolicy::ReportAndTerminate,
            "epoch-2",
        );

        // Then the census reports Undeterminable and leaves the archive
        // untouched even under the terminate policy — the load-time
        // reconcile path owns such records.
        let entry = entry_of(&report, "task-legacy");
        assert_eq!(entry.verdict, SweepVerdict::Undeterminable);
        assert_eq!(entry.fingerprint, None);
        let on_disk = read_archive(&fixture.task_json);
        assert!(matches!(on_disk.status, TaskStatus::Running));
        assert_eq!(on_disk.ended_at, None);
    }

    #[test]
    fn should_report_an_undeterminable_verdict_and_touch_neither_process_nor_archive_when_start_ticks_are_unreadable()
     {
        // Given a live process and an archive whose fingerprint carries
        // its pid but a zero (never-recorded) start tick — identity
        // cannot be confirmed
        let spawned = spawn_leader("sleep", &[SLEEP_SECONDS]);
        let unverifiable = ProcessFingerprint {
            pid: spawned.pid,
            pgid: spawned.pgid,
            start_ticks: 0,
        };
        let task = archive_task("task-unreadable", Some(unverifiable));
        let fixture = fixture_with_task(&task);

        // When the sweep runs under the terminate policy
        let report = sweep_orphans(
            &fixture.sessions_root,
            OrphanDisposePolicy::ReportAndTerminate,
            "epoch-2",
        );

        // Then the verdict is Undeterminable — never ReconciledDead, so
        // the archive is not settled — and the live process is left
        // running: "cannot determine" is report-only, always.
        let entry = entry_of(&report, "task-unreadable");
        assert_eq!(entry.verdict, SweepVerdict::Undeterminable);
        assert!(entry.note.contains("cannot judge"), "note: {}", entry.note);
        assert!(probe_process(spawned.pid).unwrap().alive);
        let on_disk = read_archive(&fixture.task_json);
        assert!(matches!(on_disk.status, TaskStatus::Running));
        assert_eq!(on_disk.ended_at, None);
        drop(spawned);
    }

    #[test]
    fn should_terminate_group_residue_when_policy_is_report_and_terminate() {
        // Given a leader that already exited, leaving a live descendant
        // in its process group, and an archive pointing at the dead
        // leader's fingerprint
        let pgid = spawn_exiting_leader(&format!("sleep {SLEEP_SECONDS} & exec sleep 1"));
        wait_for_group_member(pgid);
        let fp = ProcessFingerprint {
            pid: pgid,
            pgid,
            start_ticks: 1,
        };
        let task = archive_task("task-residue-term", Some(fp));
        let fixture = fixture_with_task(&task);

        // When the sweep runs under the terminate policy
        let report = sweep_orphans(
            &fixture.sessions_root,
            OrphanDisposePolicy::ReportAndTerminate,
            "epoch-2",
        );

        // Then the residue group is terminated, the report records the
        // disposal, and the archive settles Stopped.
        let entry = entry_of(&report, "task-residue-term");
        assert_eq!(entry.verdict, SweepVerdict::GroupResidue);
        assert!(
            entry.note.contains(DISPOSAL_ANNOTATION),
            "note: {}",
            entry.note
        );
        assert!(
            eventually(|| list_group_members(pgid)
                .ok()
                .filter(|m| m.is_empty())
                .map(|_| ()))
            .is_some(),
            "the residue group must be terminated under ReportAndTerminate"
        );
        let on_disk = read_archive(&fixture.task_json);
        assert!(matches!(on_disk.status, TaskStatus::Stopped));
        assert!(on_disk.ended_at.is_some());
        assert!(on_disk.output.contains(DISPOSAL_ANNOTATION));
    }

    #[test]
    fn should_archive_an_empty_census_report_when_there_are_no_candidates() {
        // Given a sessions root with no task archives at all
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("wd_test/session-x")).unwrap();

        // When the sweep runs
        let report = sweep_orphans(tmp.path(), OrphanDisposePolicy::ReportOnly, "epoch-7");

        // Then the census is empty but still archived (an empty census
        // is itself an answer).
        assert!(report.entries.is_empty());
        assert_eq!(report.epoch, "epoch-7");
        let files = find_report_files(tmp.path());
        assert_eq!(files.len(), 1);
        let archived: SweepReport =
            serde_json::from_str(&fs::read_to_string(&files[0]).unwrap()).unwrap();
        assert!(archived.entries.is_empty());
        assert_eq!(archived.epoch, "epoch-7");
    }

    #[test]
    fn should_not_report_terminal_or_agent_kind_archives_when_loading_candidates() {
        // Given a sessions root mixing a completed process task, a
        // running agent task, and a running process task
        let tmp = tempfile::tempdir().unwrap();
        let tasks_dir = tmp.path().join("wd_test/session-x/tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        let persistence =
            TaskPersistence::new(tasks_dir.clone(), Arc::new(mirri_os::LocalSyncFs::new()));
        let mut completed = archive_task("task-done", None);
        completed.status = TaskStatus::Completed { exit_code: 0 };
        let mut agent = archive_task("task-agent", None);
        agent.kind = TaskKind::Agent {
            agent_id: Some("agent-1".to_string()),
            subagent_type: None,
            parent_tool_call_id: None,
            model: None,
            swarm_index: None,
        };
        let running = archive_task("task-run", None);
        persistence.save_task(&completed).unwrap();
        persistence.save_task(&agent).unwrap();
        persistence.save_task(&running).unwrap();

        // When the sweep runs
        let report = sweep_orphans(tmp.path(), OrphanDisposePolicy::ReportOnly, "epoch-2");

        // Then only the running process-kind archive is censused.
        let ids: Vec<&str> = report.entries.iter().map(|e| e.task_id.as_str()).collect();
        assert_eq!(ids, vec!["task-run"]);
    }

    /// A [`SyncFs`] that records every path it is asked to touch and
    /// delegates the real work to the host filesystem, so a test can prove
    /// the census reads through the injected provider rather than the
    /// process filesystem.
    #[derive(Default)]
    struct RecordingFs {
        inner: mirri_os::LocalSyncFs,
        seen: std::sync::Mutex<Vec<(&'static str, std::path::PathBuf)>>,
    }

    impl RecordingFs {
        fn record(&self, op: &'static str, path: &Path) {
            self.seen.lock().unwrap().push((op, path.to_path_buf()));
        }

        /// Every path the census touched through `op`.
        fn seen_via(&self, op: &'static str) -> Vec<std::path::PathBuf> {
            self.seen
                .lock()
                .unwrap()
                .iter()
                .filter(|(seen_op, _)| *seen_op == op)
                .map(|(_, path)| path.clone())
                .collect()
        }
    }

    impl SyncFs for RecordingFs {
        fn iterdir(&self, path: &Path) -> anyhow::Result<Vec<mirri_os::DirEntry>> {
            self.record("iterdir", path);
            self.inner.iterdir(path)
        }

        fn exists(&self, path: &Path) -> bool {
            self.record("exists", path);
            self.inner.exists(path)
        }

        fn read_text(&self, path: &Path) -> anyhow::Result<String> {
            self.record("read_text", path);
            self.inner.read_text(path)
        }
    }

    #[test]
    fn should_walk_and_load_the_archive_through_the_injected_filesystem_provider() {
        // Given one running process-task archive and a recording provider
        let fixture = fixture_with_task(&archive_task("task-injected", None));
        let recorder = Arc::new(RecordingFs::default());
        let fs: Arc<dyn SyncFs> = recorder.clone();

        // When the census walks through the injected provider
        let candidates = load_running_process_tasks_with_fs(&fixture.sessions_root, &fs);

        // Then it finds the archive and every read — traversal, the tasks
        // probe, and the archive load — went through the provider, not
        // `std::fs`.
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].task.id, "task-injected");
        let iterdir = recorder.seen_via("iterdir");
        assert!(
            iterdir.contains(&fixture.sessions_root),
            "sessions root must be listed through the provider"
        );
        assert!(
            iterdir.iter().any(|path| path.ends_with("wd_test")),
            "workspace directory must be listed through the provider"
        );
        assert!(
            recorder
                .seen_via("exists")
                .iter()
                .any(|path| path.ends_with("tasks")),
            "the tasks-directory probe must go through the provider"
        );
        assert!(
            iterdir.iter().any(|path| path.ends_with("tasks")),
            "the tasks directory must be listed through the provider"
        );
        assert!(
            recorder
                .seen_via("read_text")
                .iter()
                .any(|path| path.ends_with("task-injected.json")),
            "the task archive must be loaded through the provider"
        );
    }

    #[test]
    fn should_discover_an_archive_when_the_tasks_directory_is_a_symlink() {
        // Given a running process-task archive behind a symlinked tasks directory
        let fixture = fixture_with_task(&archive_task("task-symlinked", None));
        let tasks_dir = fixture.task_json.parent().unwrap().to_path_buf();
        let session_dir = tasks_dir.parent().unwrap().to_path_buf();
        std::fs::rename(&tasks_dir, session_dir.join("tasks-real")).unwrap();
        std::os::unix::fs::symlink("tasks-real", &tasks_dir).unwrap();

        // When the census walks the sessions root
        let candidates = load_running_process_tasks(&fixture.sessions_root);

        // Then the archive behind the symlink is discovered
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].task.id, "task-symlinked");
    }
}
