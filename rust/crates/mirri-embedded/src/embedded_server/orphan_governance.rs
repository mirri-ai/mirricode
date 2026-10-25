//! Startup orphan census + resident lifetime sweeper (ADR-012 f, US14).
//!
//! Two pieces of assembly, both owned by [`EmbeddedServer::call_method`]'s
//! first-call seam (construction may run before the Tokio runtime exists —
//! `for_cwd` / `from_parts` — while the first RPC call never does):
//!
//! - **The one-shot orphan sweep** runs on the first dispatched RPC,
//!   inline-awaited through `spawn_blocking`: the census must settle
//!   before any `session.create` / `session.resume` can register a task of
//!   this instance's own (Task 3's ordering constraint — the sweep's
//!   candidates are *all* Running+Process archives across every workspace,
//!   with no epoch filter, so a live task of the current instance would be
//!   misreported as an orphan if the sweep ran late). The sweep itself is
//!   a synchronous pure-function walk (see
//!   [`mirri_core::orphan_sweep`]); `spawn_blocking` keeps the bounded
//!   local-disk I/O off the async workers, and the report is always
//!   archived under `<sessions_root>/orphan-sweep/<ts>.json` (US14.6).
//!
//! - **The resident lifetime sweeper** ticks every
//!   [`ORPHAN_LIFETIME_SWEEP_SECS`] (60s): it re-walks the sessions root
//!   (the same directory-walk shape the census uses) and terminates every
//!   unsettled Running process-kind archive whose `created_at` is past the
//!   resolved lifetime ceiling (`resolve_max_task_lifetime`, `0` = the
//!   unbounded sentinel — the loop itself is still spawned, it just never
//!   terminates anything). This disk-based walk is the ADR-012 f point:
//!   it covers the keepAlive-surviving task whose session closed with no
//!   driver left to sweep it — the per-session task drivers only see live
//!   sessions. Termination goes through the census's narrow shared entry
//!   (`mirri_core::orphan_sweep::terminate_expired_task`): fingerprint
//!   identity re-verification before the group signal, then the archive
//!   settles `Stopped` with the lifetime-ceiling annotation. The blocking
//!   `terminate_group` (up-to-5s poll) rides `spawn_blocking` per task.
//!
//!   The `lifetime_exempt` flag (SDR-013 d) exempts a candidate from this
//!   sweeper: either the model's explicit one-shot `exempt_lifetime` at
//!   dispatch, or the keepAliveOnExit close path stamping every survivor
//!   (the user who configured exit-survival already said "keep it alive").
//!   The time-based predicate (`task_is_lifetime_expired`) stays pure; the
//!   exemption is this pass's composition.

use std::sync::Arc;

use mirri_core::orphan_sweep as core_sweep;

use super::EmbeddedServer;

/// The lifetime sweeper's production tick (ADR-012 f): one pass per
/// minute over the unsettled process-task archives.
pub(super) const ORPHAN_LIFETIME_SWEEP_SECS: u64 = 60;

impl EmbeddedServer {
    /// Run the one-shot startup orphan census (ADR-012 c/d, US14.2/14.3)
    /// and spawn the resident lifetime sweeper (ADR-012 f, US14.4).
    ///
    /// Called on the first dispatched RPC — the same lazy seam
    /// `migrate_legacy_sessions_once` uses (construction may precede the
    /// Tokio runtime; the first `call_method` never does). The census
    /// itself is **awaited inline** (not spawned): the sweep must settle
    /// before this instance can register any session task of its own
    /// (Task 3's ordering constraint), and a concurrent first dispatch
    /// cannot overtake it. The work is bounded local-disk I/O and process
    /// probes, offloaded to the blocking pool. The sweeper loop is spawned
    /// afterwards — its first pass never terminates anything the census
    /// owns (the default 4h ceiling, versus tasks just swept at startup).
    pub(super) async fn run_startup_orphan_governance(&self) {
        // The OnceCell waits concurrent first-callers out: a racing second
        // dispatch enters `get_or_init` and AWAITS the same cell the first
        // caller is initializing, instead of the early-return
        // `AtomicBool::swap` shape, where the loser would skip the await
        // and proceed before the census settled (the Task 3 ordering
        // ledger broken — a session-create riding that loser dispatch
        // could register a task the still-running census would misreport
        // as an orphan). The cell's value is just the completion marker;
        // the work itself runs inside the initializer.
        self.orphan_governance_settled
            .get_or_init(|| async {
                // The ordering ledger (Task 3): the census must complete
                // before the first TaskManager of this instance registers
                // a task — the candidates are all Running+Process archives
                // with no epoch filter, so a late sweep would misreport
                // this instance's own live tasks as orphans. Holding this
                // future to completion (the inline await) is what enforces
                // it: every session-create path rides `call_method`, whose
                // first arrivals all await this same cell.
                let sessions_root = self.manager.persistence().sessions_dir().to_path_buf();
                let policy = self
                    .config
                    .read()
                    .unwrap_or_else(|e| e.into_inner())
                    .resolve_orphan_dispose_policy();
                let epoch = self.epoch.clone();
                match tokio::task::spawn_blocking(move || {
                    core_sweep::sweep_orphans(&sessions_root, policy, &epoch)
                })
                .await
                {
                    Ok(report) => {
                        if !report.entries.is_empty() {
                            tracing::info!(
                                count = report.entries.len(),
                                "startup orphan census: reported unsettled \
                                 process-task archives"
                            );
                        }
                    }
                    Err(join_error) => {
                        tracing::warn!(
                            %join_error,
                            "startup orphan census task failed to join"
                        );
                    }
                }
                self.spawn_orphan_lifetime_sweeper();
            })
            .await;
    }

    /// The resident lifetime sweeper's body: one Tokio task ticking every
    /// [`ORPHAN_LIFETIME_SWEEP_SECS`], walking the sessions root's
    /// unsettled Running process-kind archives and terminating the
    /// lifetime-expired ones (ADR-012 f).
    ///
    /// The loop holds no `&EmbeddedServer` (the same shape as the task
    /// driver): the sessions root, the config snapshot handle and the
    /// epoch ride along as captured values. It never exits on its own —
    /// the sweeper is server-lifetime-resident, exactly like the
    /// index-refresh driver.
    pub(super) fn spawn_orphan_lifetime_sweeper(&self) {
        let sessions_root = self.manager.persistence().sessions_dir().to_path_buf();
        let config = Arc::clone(&self.config);
        let tick = std::time::Duration::from_secs(ORPHAN_LIFETIME_SWEEP_SECS);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(tick);
            // The first tick completes immediately — a warm engine with
            // an already-expired archive is terminated without waiting a
            // full period.
            loop {
                interval.tick().await;
                if let Err(error) = sweep_expired_task_archives(&sessions_root, &config).await {
                    tracing::warn!(%error, "lifetime sweeper pass failed");
                }
            }
        });
    }
}

/// One lifetime-sweep pass over the sessions root (ADR-012 f, US14.4).
///
/// Every unsettled Running process-kind archive past the resolved
/// lifetime ceiling is terminated through the census's verified group
/// termination and settled `Stopped` with the lifetime-ceiling
/// annotation. The walk and the per-task probes/termination are blocking
/// work — the whole pass rides `spawn_blocking` so the async workers
/// stay free (the per-task `terminate_group` polls up to 5 s).
///
/// Failures are per-task and best-effort (logged, never propagated): a
/// task whose fingerprint cannot be re-verified stays `Running` for the
/// load-time reconcile path, and the next pass retries it.
async fn sweep_expired_task_archives(
    sessions_root: &std::path::Path,
    config: &Arc<std::sync::RwLock<mirri_core::ServerConfig>>,
) -> Result<(), String> {
    let sessions_root = sessions_root.to_path_buf();
    let config = Arc::clone(config);
    tokio::task::spawn_blocking(move || -> std::io::Result<()> {
        let max_lifetime = config
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .resolve_max_task_lifetime();
        if max_lifetime.is_zero() {
            // The unbounded sentinel: the ceiling is disabled — the pass
            // is a cheap no-op walk (not even the directory scan).
            return Ok(());
        }
        let mut expired = Vec::new();
        for candidate in core_sweep::load_running_process_tasks(&sessions_root) {
            // The SDR-013 d exemption: a task marked `lifetime_exempt`
            // never falls to this sweeper, however old — either the model
            // exempted it explicitly at dispatch (once per task, US14.4)
            // or the keepAliveOnExit close path stamped the surviving
            // task. The time-based predicate stays pure
            // (`task_is_lifetime_expired`); the exemption is composed
            // here so the census and this pass never drift.
            if !candidate.task.lifetime_exempt
                && core_sweep::task_is_lifetime_expired(&candidate.task, max_lifetime)
            {
                expired.push(candidate);
            }
        }
        for mut candidate in expired {
            // `terminate_expired_task` re-verifies the fingerprint
            // identity before the group signal and settles the archive
            // only on a verifiable termination — a failure leaves the
            // record Running for the next pass / the load-time
            // reconcile path.
            let task_id = candidate.task.id.clone();
            let result =
                core_sweep::terminate_expired_task(&mut candidate.task, &candidate.tasks_dir);
            match result {
                Ok(()) => tracing::info!(
                    task_id = %task_id,
                    "lifetime sweeper terminated an expired task"
                ),
                Err(error) => tracing::warn!(
                    task_id = %task_id,
                    %error,
                    "lifetime sweeper could not verifiably terminate an expired task"
                ),
            }
        }
        Ok(())
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| e.to_string())
}

// The whole module spawns real process-group leaders (Unix
// `process_group(0)`) and probes them through the Unix-only start-tick
// readers, so it cannot compile — let alone pass — off Unix.
#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use mirri_core::TaskKind;
    use mirri_core::TaskStatus;
    use mirri_core::orphan_sweep::{LIFETIME_CEILING_ANNOTATION, SweepReport};
    use mirri_core::task_persist::TaskPersistence;
    use mirri_os::LocalSyncFs;
    use mirri_transport::EngineAdapter;
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    /// The latest census report archived under
    /// `<sessions_root>/orphan-sweep/`, if any — the test seam asserting
    /// the startup census ran (and archived).
    fn last_sweep_report(sessions_root: &std::path::Path) -> Option<SweepReport> {
        let dir = sessions_root.join("orphan-sweep");
        let latest = std::fs::read_dir(&dir)
            .ok()?
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
            .max()?;
        let text = std::fs::read_to_string(latest).ok()?;
        serde_json::from_str(&text).ok()
    }

    /// A 60-second sleep gives every test ample headroom on slow CI.
    const SLEEP_SECONDS: &str = "60";

    /// A spawned long-running group leader. Dropping the guard kills the
    /// whole group, so a failing assertion can never leak a child.
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
            let _ = mirri_os::process::terminate_group(self.pgid);
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

    fn probe_fingerprint(pid: i32, pgid: i32) -> mirri_os::ProcessFingerprint {
        let start_ticks = mirri_os::probe_process(pid)
            .expect("probe the test child")
            .start_ticks
            .expect("start ticks are readable on unix");
        mirri_os::ProcessFingerprint {
            pid,
            pgid,
            start_ticks,
        }
    }

    /// A Running process-kind archive planted on disk under the given
    /// sessions root, in the bucketed layout the engine writes.
    fn plant_archive(
        sessions_root: &std::path::Path,
        id: &str,
        fingerprint: Option<mirri_os::ProcessFingerprint>,
        created_at: chrono::DateTime<chrono::Utc>,
    ) -> std::path::PathBuf {
        plant_archive_ex(sessions_root, id, fingerprint, created_at, false)
    }

    /// [`plant_archive`] with the `lifetime_exempt` flag controlled — the
    /// keepAlive-shaped archive the sweeper must skip (SDR-013 d).
    fn plant_archive_ex(
        sessions_root: &std::path::Path,
        id: &str,
        fingerprint: Option<mirri_os::ProcessFingerprint>,
        created_at: chrono::DateTime<chrono::Utc>,
        lifetime_exempt: bool,
    ) -> std::path::PathBuf {
        let tasks_dir = sessions_root
            .join("wd_test")
            .join("session-x")
            .join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        let task = mirri_core::AgentTask {
            id: id.to_string(),
            kind: TaskKind::Process {
                command: Some("sleep 60".to_string()),
                dispatcher: None,
            },
            description: "sweep test task".to_string(),
            status: TaskStatus::Running,
            created_at,
            ended_at: None,
            output: String::new(),
            session_id: "session-x".to_string(),
            deadline: None,
            detached: true,
            notification_suppressed: false,
            notification_delivered_at: None,
            fingerprint,
            lifetime_exempt,
        };
        TaskPersistence::new(tasks_dir.clone(), Arc::new(LocalSyncFs::new()))
            .save_task(&task)
            .unwrap();
        tasks_dir.join(format!("{id}.json"))
    }

    fn read_archive(path: &std::path::Path) -> mirri_core::AgentTask {
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    /// An assembled engine bound to a temp workspace + temp home (the
    /// serve-shaped funnel — `call_method` dispatch drives the census).
    fn assembled_orphan_test_server(home: &std::path::Path) -> EmbeddedServer {
        let workspace = tempfile::tempdir().expect("workspace tempdir");
        let parts = crate::bootstrap::assemble(
            mirri_core::WorkspaceId::new("orphan-governance-ws"),
            workspace.path().to_path_buf(),
            Some(home.to_path_buf()),
            None,
            crate::bootstrap::WorkspacePolicy::TrustedFromConfig,
            Vec::new(),
        );
        // Keep the workspace tempdir alive for the server's lifetime: the
        // engine routes through the registry built over it.
        std::mem::forget(workspace);
        EmbeddedServer::from_parts(parts)
    }

    /// Given an assembled engine over a home with a planted live-orphan
    /// archive, when the first RPC dispatch runs, then the startup census
    /// archives exactly one report under `<home>/v3_sessions/orphan-sweep/`
    /// and (report-only default) leaves both the process and the archive
    /// untouched.
    #[tokio::test]
    async fn should_run_the_orphan_sweep_once_when_the_server_starts() {
        let home = tempfile::tempdir().expect("home tempdir");
        let sessions_root = home.path().join("v3_sessions");
        let spawned = spawn_leader("sleep", &[SLEEP_SECONDS]);
        let fp = probe_fingerprint(spawned.pid, spawned.pgid);
        let task_json = plant_archive(&sessions_root, "task-live", Some(fp), chrono::Utc::now());

        let server = assembled_orphan_test_server(home.path());
        // The first dispatched RPC triggers the inline census.
        server
            .call_method("config.getAll", serde_json::json!({}))
            .await
            .expect("first dispatch must succeed");

        let report = last_sweep_report(&sessions_root).expect("a census report must be archived");
        assert_eq!(
            report.entries.len(),
            1,
            "the planted orphan must be censused"
        );
        assert_eq!(report.entries[0].task_id, "task-live");
        // The default policy is report-only: no signal, no settle.
        assert!(mirri_os::probe_process(spawned.pid).unwrap().alive);
        assert!(matches!(
            read_archive(&task_json).status,
            TaskStatus::Running
        ));

        // A second dispatch must NOT run the census again (the once-guard).
        std::fs::remove_dir_all(sessions_root.join("orphan-sweep")).unwrap();
        server
            .call_method("config.getAll", serde_json::json!({}))
            .await
            .expect("second dispatch must succeed");
        assert!(
            !sessions_root.join("orphan-sweep").exists(),
            "the census must run exactly once per engine"
        );
        drop(spawned);
    }

    /// Given a planted Running process-task archive whose created_at is
    /// past the lifetime ceiling, when the sweeper pass runs, then the
    /// task's process group is terminated and the archive settles
    /// Stopped with the lifetime-ceiling annotation.
    #[tokio::test]
    async fn should_terminate_a_lifetime_expired_task_when_the_sweeper_ticks() {
        let home = tempfile::tempdir().expect("home tempdir");
        let sessions_root = home.path().join("v3_sessions");
        let mut spawned = spawn_leader("sleep", &[SLEEP_SECONDS]);
        let fp = probe_fingerprint(spawned.pid, spawned.pgid);
        let task_json = plant_archive(
            &sessions_root,
            "task-expired",
            Some(fp),
            // 5 hours old — past the 4-hour default ceiling.
            chrono::Utc::now() - chrono::Duration::hours(5),
        );
        // A fresh (non-expired) archive as the control: must survive.
        let fresh_json = plant_archive(&sessions_root, "task-fresh", None, chrono::Utc::now());

        let config = Arc::new(std::sync::RwLock::new(mirri_core::ServerConfig::default()));
        sweep_expired_task_archives(&sessions_root, &config)
            .await
            .expect("the sweep pass must succeed");

        // The expired task's group is gone and the archive settled.
        assert!(
            eventually(|| {
                mirri_os::probe_process(spawned.pid)
                    .ok()
                    .filter(|p| !p.alive)
                    .map(|_| ())
            })
            .is_some(),
            "the expired task's process group must be terminated"
        );
        let settled = read_archive(&task_json);
        assert!(matches!(settled.status, TaskStatus::Stopped));
        assert!(settled.ended_at.is_some());
        assert!(
            settled.output.contains(LIFETIME_CEILING_ANNOTATION),
            "annotation: {}",
            settled.output
        );
        // The fresh task is untouched.
        assert!(matches!(
            read_archive(&fresh_json).status,
            TaskStatus::Running
        ));
        // Reaped here because the sweeper already killed the group, so this
        // returns immediately — unlike the exempt-task test below.
        spawned.wait();
    }

    /// Given a planted Running process-task archive past the lifetime
    /// ceiling that carries the lifetime exemption (a keepAlive survivor
    /// — the session closed under keepAliveOnExit and stamped the row), when
    /// the sweeper pass runs, then the task is skipped entirely: the
    /// process keeps running and the archive stays Running (SDR-013 d —
    /// the user already said "keep it alive").
    #[tokio::test]
    async fn should_keep_an_exempt_task_alive_when_the_sweeper_ticks() {
        let home = tempfile::tempdir().expect("home tempdir");
        let sessions_root = home.path().join("v3_sessions");
        let spawned = spawn_leader("sleep", &[SLEEP_SECONDS]);
        let fp = probe_fingerprint(spawned.pid, spawned.pgid);
        let exempt_json = plant_archive_ex(
            &sessions_root,
            "task-keepalive",
            Some(fp),
            // 5 hours old — past the 4-hour default ceiling, but exempt.
            chrono::Utc::now() - chrono::Duration::hours(5),
            true,
        );

        let config = Arc::new(std::sync::RwLock::new(mirri_core::ServerConfig::default()));
        sweep_expired_task_archives(&sessions_root, &config)
            .await
            .expect("the sweep pass must succeed");

        // The exempt task's process group is still alive...
        assert!(
            mirri_os::probe_process(spawned.pid)
                .expect("probe the exempt child")
                .alive,
            "the exempt task's process group must NOT be terminated"
        );
        // ...and the archive stays Running with no terminal annotation.
        let untouched = read_archive(&exempt_json);
        assert!(
            matches!(untouched.status, TaskStatus::Running),
            "the exempt archive must stay Running, was {:?}",
            untouched.status
        );
        assert_eq!(untouched.ended_at, None);
        assert!(
            !untouched.output.contains(LIFETIME_CEILING_ANNOTATION),
            "no lifetime-ceiling annotation may land on an exempt task"
        );
        // Deliberately no `spawned.wait()`: this test asserts the sweeper left
        // the exemption alone, so the `sleep` is still running here by design.
        // Waiting for it to exit on its own burned the full SLEEP_SECONDS in
        // every run; `Drop` terminates the group and reaps it.
    }
}
