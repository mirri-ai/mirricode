//! Retention-domain routes — the 6 `retention.*` arms of the
//! session-retention management page (session-retention-architecture §4,
//! the "管理页 API" component). Rust-first wires: no klient RPC
//! counterpart (the TS v2 surface stays off this vertical), both faces
//! ride the same engine — the embedded route and the serve wire's engine
//! fall-through.
//!
//! Five of the six answer an explicit `not_implemented` today (Task 7
//! registers the wire surface; Task 9 lands the handlers over the session
//! index, the retention engine, and the delete executor). Only
//! `retention.getPolicy` serves its final shape — it is a pure config
//! snapshot read.

use super::Route;

pub(super) const ROUTES: &[Route] = &[
    // The management page's archive / compressed view listing. Task 9:
    // serve `mirri_proto::retention::RetentionListResult` over the
    // session index + disk scans.
    Route {
        method: "retention.list",
        aliases: &[],
        handler: |server, params| Box::pin(server.dispatch_retention_list(params)),
    },
    // The header counters plus the four per-tier delete estimates
    // (30/90/180/365 — the product constant set). Task 9: serve
    // `mirri_proto::retention::RetentionStatsResult`.
    Route {
        method: "retention.stats",
        aliases: &[],
        handler: |server, params| Box::pin(server.dispatch_retention_stats(params)),
    },
    // Compress one session immediately. Task 9: the retention engine's
    // compress path (directory lock + revalidation + atomic swap).
    Route {
        method: "retention.compressNow",
        aliases: &[],
        handler: |server, params| Box::pin(server.dispatch_retention_compress_now(params)),
    },
    // Single-session delete. Task 9: the unified delete executor
    // (directory-then-index-row, blob-ledger attachment reclamation).
    Route {
        method: "retention.delete",
        aliases: &[],
        handler: |server, params| Box::pin(server.dispatch_retention_delete(params)),
    },
    // The tier-button batch delete (older_than_days ∈ 30/90/180/365,
    // validated against the constant set). Task 9: the same executor,
    // batched with per-row outcome reporting.
    Route {
        method: "retention.deleteTiered",
        aliases: &[],
        handler: |server, params| Box::pin(server.dispatch_retention_delete_tiered(params)),
    },
    // The resolved [retention] knobs — final shape today (a config
    // snapshot read; the same resolve_retention the periodic task will
    // consult every round).
    Route {
        method: "retention.getPolicy",
        aliases: &[],
        handler: |server, params| Box::pin(server.dispatch_retention_get_policy(params)),
    },
];
