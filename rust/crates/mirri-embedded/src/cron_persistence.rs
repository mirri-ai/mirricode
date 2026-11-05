//! The embedded-layer [`CronPersistence`] adapter (ADR-020): the production
//! twin of mirri-core's dev-only test impl. rusqlite is legal in this crate,
//! so the carrier (`mirri-cron-index`) is consumed here directly and adapted
//! into mirri-core's storage-contract seam — keeping the carrier out of
//! mirri-core's dependency tree (the zero-IO gate's regression lockout).
//!
//! Row conversion is plain functions, not `From` impls: both the record
//! (mirri-core) and the row (mirri-cron-index) are foreign types here, so an
//! orphan `From` impl would not compile — the orphan rule permits the trait
//! impl below only because `SqliteCronPersistence` is local.

use std::sync::Arc;

use mirri_core::cron::{CronPersistence, ScheduledTaskRecord};

/// The SQLite write-through backend over the registry's shared
/// [`mirri_cron_index::CronIndex`] connection: the same `Arc` the RPC faces
/// (`cron.listAll`, the workspace delete cascade) read, so the manager and
/// the RPC views never diverge.
pub struct SqliteCronPersistence(pub Arc<mirri_cron_index::CronIndex>);

fn to_row(record: &ScheduledTaskRecord) -> mirri_cron_index::ScheduledTaskRow {
    mirri_cron_index::ScheduledTaskRow {
        id: record.id.clone(),
        workspace_id: record.workspace_id.clone(),
        next_fire_at: record.next_fire_at.clone(),
        paused: record.paused,
        created_at: record.created_at.clone(),
        body: record.body.clone(),
    }
}

fn to_record(row: &mirri_cron_index::ScheduledTaskRow) -> ScheduledTaskRecord {
    ScheduledTaskRecord {
        id: row.id.clone(),
        workspace_id: row.workspace_id.clone(),
        next_fire_at: row.next_fire_at.clone(),
        paused: row.paused,
        created_at: row.created_at.clone(),
        body: row.body.clone(),
    }
}

impl CronPersistence for SqliteCronPersistence {
    fn list_workspace_rows(&self, workspace_id: &str) -> anyhow::Result<Vec<ScheduledTaskRecord>> {
        Ok(self.0.list_workspace(workspace_id)?.iter().map(to_record).collect())
    }

    fn replace_workspace_rows(
        &self,
        workspace_id: &str,
        rows: &[ScheduledTaskRecord],
    ) -> anyhow::Result<()> {
        let rows: Vec<mirri_cron_index::ScheduledTaskRow> = rows.iter().map(to_row).collect();
        self.0.replace_workspace(workspace_id, &rows)
    }
}
