//! Lifecycle sentinel for service-exit detection.
//!
//! Each server instance leaves a two-file sentinel under
//! `<mirri-home>/lifecycle/<instance-id>/`: `launch.json` written at startup
//! and `graceful.json` written on a graceful shutdown (carrying the sessions
//! that were still running). A later startup scans all instance directories
//! and judges how the previous instances exited — abnormal (crash) exits are
//! inferred from "launch without graceful" once the liveness gate rules the
//! instance dead, graceful exits are read straight from their record.
//!
//! Judged directories are moved into `<root>/consumed/` by [`consume`] — the
//! directory rename is the atomic placeholder that gives a single winner
//! when two instances race their startup checks. Judgement itself
//! ([`scan_and_judge`]) is read-only: it never deletes anything.
//!
//! Spec: `docs/specs/interrupted-task-recovery-architecture.md` §1
//! (ADR-001 sentinel shape, ADR-008 read-judge-write idempotence).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Filename of the startup marker inside an instance directory.
const LAUNCH_FILE: &str = "launch.json";
/// Filename of the graceful-exit record inside an instance directory.
const GRACEFUL_FILE: &str = "graceful.json";
/// Subdirectory that judged instance directories are renamed into.
const CONSUMED_DIR: &str = "consumed";

/// Startup marker written by a server instance when it becomes servable
/// (spec §1.1 `launch.json`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaunchRecord {
    /// The instance identifier (port + start-moment by convention).
    pub instance: String,
    /// Epoch-milliseconds timestamp of when the instance started.
    pub started_at: u64,
    /// The port the instance listens on — the liveness-gate probe target.
    pub port: u16,
    /// The OS process id — the tie-breaker when the port was reused.
    pub pid: u32,
}

/// One entry of the graceful record's still-running session list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunningSessionEntry {
    /// The session identifier.
    pub session_id: String,
    /// The workspace the session belongs to.
    pub workspace_key: String,
    /// The session's display title.
    pub title: String,
    /// Epoch-milliseconds timestamp of the session's last visible activity.
    pub interrupted_at: u64,
}

/// Graceful-exit record written on the shutdown path (spec §1.1
/// `graceful.json`), carrying the sessions that were still running.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GracefulRecord {
    /// The instance identifier.
    pub instance: String,
    /// Epoch-milliseconds timestamp of when the instance exited.
    pub exited_at: u64,
    /// The sessions that were still running at exit.
    pub running_sessions: Vec<RunningSessionEntry>,
}

/// An instance judged to have exited abnormally (launch without graceful,
/// ruled dead by the liveness gate).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AbnormalExit {
    /// The instance identifier.
    pub instance: String,
    /// The port the instance listened on.
    pub port: u16,
    /// The OS process id of the exited instance.
    pub pid: u32,
    /// Epoch-milliseconds timestamp of when the instance started.
    pub started_at: u64,
}

/// The outcome of a startup scan: which instances exited abnormally, and
/// which directories are ready to be consumed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LifecycleVerdict {
    /// Instances with a launch marker but no graceful record, ruled dead
    /// by the liveness gate.
    pub abnormal: Vec<AbnormalExit>,
    /// Instance directories that were judged and should be renamed into
    /// `<root>/consumed/` by [`consume`]. Alive instances are deliberately
    /// absent — they are neither judged nor consumed.
    pub consumed: Vec<PathBuf>,
}

/// The lifecycle sentinel root for a mirri home: `<home>/lifecycle`.
pub fn lifecycle_root(mirri_home: &Path) -> PathBuf {
    mirri_home.join("lifecycle")
}

/// Atomically write a startup marker for `rec` under `root`.
///
/// The record lands in `<root>/<rec.instance>/launch.json` via the
/// crate-wide 0600-temp + rename write path, so a crash mid-write never
/// leaves a torn marker.
pub async fn write_launch(root: &Path, rec: &LaunchRecord) -> std::io::Result<()> {
    write_record(&root.join(&rec.instance).join(LAUNCH_FILE), rec).await
}

/// Atomically write a graceful-exit record for `rec` under `root`.
///
/// The record lands in `<root>/<rec.instance>/graceful.json` via the
/// crate-wide 0600-temp + rename write path.
pub async fn write_graceful(root: &Path, rec: &GracefulRecord) -> std::io::Result<()> {
    write_record(&root.join(&rec.instance).join(GRACEFUL_FILE), rec).await
}

/// Serialize `rec` to JSON and write it atomically to `path`.
async fn write_record<T: Serialize>(path: &Path, rec: &T) -> std::io::Result<()> {
    let json = serde_json::to_vec(rec)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    crate::fs_utils::write_file_atomically(path, &json)
}

/// Read-only scan of every instance directory under `root`.
///
/// The liveness gate (`probe(port, pid)` returning `true` = alive) protects
/// concurrently running instances from being misjudged; an instance whose
/// recorded port equals `self_port` is structurally dead (the port's unique
/// occupant is now the caller) and skips the probe. Corrupt records are
/// undeterminable: they never yield an abnormal verdict, but their
/// directories still land in `consumed` so they do not linger.
pub async fn scan_and_judge(
    root: &Path,
    self_port: u16,
    now_epoch_ms: u64,
    probe: &(dyn Fn(u16, u32) -> bool + Sync),
) -> LifecycleVerdict {
    let _ = now_epoch_ms; // reserved for retention-window judgement (ADR-006)
    let mut abnormal = Vec::new();
    let mut consumed = Vec::new();

    let Ok(entries) = crate::fs_utils::read_dir_entries(root) else {
        return LifecycleVerdict { abnormal, consumed };
    };
    for entry in entries {
        if !entry.is_dir || entry.name == CONSUMED_DIR {
            continue;
        }
        let instance_dir = entry.path;
        match read_launch_record(&instance_dir).await {
            // No launch marker: nothing to run the liveness gate on. If a
            // graceful record is present the instance was judged graceful;
            // otherwise the directory is undeterminable debris.
            None => {
                if read_graceful(root, &entry.name).await.is_some() {
                    consumed.push(instance_dir);
                } else {
                    tracing::warn!(
                        directory = %entry.name,
                        "lifecycle directory without launch.json is undeterminable"
                    );
                    consumed.push(instance_dir);
                }
            }
            // Corrupt launch marker: undeterminable (F.4) — no abnormal
            // verdict, no error, but the directory is still consumed.
            Some(Err(())) => {
                tracing::warn!(
                    directory = %entry.name,
                    "corrupt lifecycle launch record is undeterminable"
                );
                consumed.push(instance_dir);
            }
            Some(Ok(launch)) => {
                if is_alive(self_port, &launch, probe) {
                    // Running instance: neither judged nor consumed (A.5).
                    continue;
                }
                match read_graceful(root, &entry.name).await {
                    Some(_) => consumed.push(instance_dir),
                    None => {
                        abnormal.push(AbnormalExit {
                            instance: launch.instance,
                            port: launch.port,
                            pid: launch.pid,
                            started_at: launch.started_at,
                        });
                        consumed.push(instance_dir);
                    }
                }
            }
        }
    }
    LifecycleVerdict { abnormal, consumed }
}

/// The liveness gate (spec §1.2): same-port instances are structurally
/// dead (the port's unique occupant is the caller itself), everything else
/// defers to the probe.
fn is_alive(
    self_port: u16,
    launch: &LaunchRecord,
    probe: &(dyn Fn(u16, u32) -> bool + Sync),
) -> bool {
    launch.port != self_port && probe(launch.port, launch.pid)
}

/// Rename judged instance directories into `<root>/consumed/`.
///
/// The directory rename is the atomic placeholder that makes a racing
/// double startup check single-winner. Consumed directories are
/// deliberately never cleaned up afterwards — they are kept as judgement
/// traces for diagnostics (A.6 only requires they stop participating in
/// future judgements).
pub async fn consume(root: &Path, dirs: &[PathBuf]) -> std::io::Result<()> {
    let consumed_root = root.join(CONSUMED_DIR);
    tokio::fs::create_dir_all(&consumed_root).await?;
    for dir in dirs {
        let Some(name) = dir.file_name().map(|n| n.to_owned()) else {
            continue;
        };
        tokio::fs::rename(dir, consumed_root.join(name)).await?;
    }
    Ok(())
}

/// Read the graceful-exit record of `instance` under `root`.
///
/// Returns `None` when the record is missing or corrupt — callers treat
/// both as "not a graceful exit" (F.4 tolerance, no error surfaced).
pub async fn read_graceful(root: &Path, instance: &str) -> Option<GracefulRecord> {
    let path = root.join(instance).join(GRACEFUL_FILE);
    let bytes = tokio::fs::read(&path).await.ok()?;
    match serde_json::from_slice(&bytes) {
        Ok(record) => Some(record),
        Err(error) => {
            tracing::warn!(
                instance = instance,
                error = %error,
                "corrupt lifecycle graceful record"
            );
            None
        }
    }
}

/// Read and parse a directory's `launch.json`.
///
/// `Some(Err(()))` means the marker exists but cannot be parsed; `None`
/// means there is no marker to judge from.
async fn read_launch_record(instance_dir: &Path) -> Option<std::result::Result<LaunchRecord, ()>> {
    let bytes = match tokio::fs::read(instance_dir.join(LAUNCH_FILE)).await {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
        Err(error) => {
            tracing::warn!(
                directory = %instance_dir.display(),
                error = %error,
                "failed to read lifecycle launch record"
            );
            return Some(Err(()));
        }
    };
    match serde_json::from_slice(&bytes) {
        Ok(record) => Some(Ok(record)),
        Err(error) => {
            tracing::warn!(
                directory = %instance_dir.display(),
                error = %error,
                "failed to parse lifecycle launch record"
            );
            Some(Err(()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn should_judge_abnormal_when_launch_exists_but_no_graceful() {
        let home = tempfile::tempdir().unwrap();
        let root = lifecycle_root(home.path());
        write_launch(
            &root,
            &LaunchRecord {
                instance: "55827-1770000000".to_string(),
                started_at: 1_770_000_000_000,
                port: 55827,
                pid: 4242,
            },
        )
        .await
        .unwrap();

        let verdict = scan_and_judge(&root, 55999, 0, &|_port, _pid| false).await;

        assert_eq!(verdict.abnormal.len(), 1);
        assert_eq!(verdict.abnormal[0].instance, "55827-1770000000");
        assert_eq!(verdict.abnormal[0].port, 55827);
        assert_eq!(verdict.consumed, vec![root.join("55827-1770000000")]);
    }

    #[tokio::test]
    async fn should_skip_judging_when_instance_still_alive() {
        let home = tempfile::tempdir().unwrap();
        let root = lifecycle_root(home.path());
        write_launch(
            &root,
            &LaunchRecord {
                instance: "55827-1770000000".to_string(),
                started_at: 1_770_000_000_000,
                port: 55827,
                pid: 4242,
            },
        )
        .await
        .unwrap();

        let verdict = scan_and_judge(&root, 55999, 0, &|_port, _pid| true).await;

        assert!(verdict.abnormal.is_empty());
        assert!(verdict.consumed.is_empty());
    }

    #[tokio::test]
    async fn should_judge_dead_without_probe_when_port_equals_self() {
        let home = tempfile::tempdir().unwrap();
        let root = lifecycle_root(home.path());
        write_launch(
            &root,
            &LaunchRecord {
                instance: "55827-1770000000".to_string(),
                started_at: 1_770_000_000_000,
                port: 55827,
                pid: 4242,
            },
        )
        .await
        .unwrap();

        // The probe claims the instance is alive, but the recorded port is
        // the caller's own — the previous holder is structurally dead.
        let verdict = scan_and_judge(&root, 55827, 0, &|_port, _pid| true).await;

        assert_eq!(verdict.abnormal.len(), 1);
        assert_eq!(verdict.abnormal[0].instance, "55827-1770000000");
    }

    #[tokio::test]
    async fn should_treat_corrupt_records_as_undeterminable() {
        let home = tempfile::tempdir().unwrap();
        let root = lifecycle_root(home.path());
        let instance_dir = root.join("55827-1770000000");
        std::fs::create_dir_all(&instance_dir).unwrap();
        std::fs::write(instance_dir.join(LAUNCH_FILE), b"{ not valid json").unwrap();

        let verdict = scan_and_judge(&root, 55999, 0, &|_port, _pid| true).await;

        assert!(verdict.abnormal.is_empty());
        assert!(verdict.consumed.contains(&instance_dir));
    }
}
