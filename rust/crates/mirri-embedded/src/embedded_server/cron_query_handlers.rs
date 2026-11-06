//! The cron query + run-record RPC faces: `cron.preview` (the theoretical
//! moments), `cron.runs.list` (the run records a panel consumes), and
//! `cron.run.open` (the explicit-open path that materializes an unopened
//! execution session — ADR-008's lazy-index admission).

use std::sync::Arc;

use serde::Deserialize;
use serde_json::{Value, json};

use mirri_transport::adapter::EngineError;

use super::{EmbeddedServer, invalid_params, normalize_params, not_found};

impl EmbeddedServer {
    /// `cron.preview` — the read-only theoretical-moment preview face
    /// (ADR-017, form batch 6 T3; batch 2 / A5 adds the past): the next
    /// `count` occurrences of a schedule (expression or interval) plus the
    /// theoretical PAST occurrence (`previousT0`), all evaluated by the
    /// SAME [`mirri_core::cron::CronManager`] kernels the scheduler plans
    /// with (pre-jitter ideal instants). The past is inclusive of the
    /// anchor and `null` when the rule never produced an occurrence at or
    /// before it (a one-off-shaped rule → the form's "—"). No task side
    /// effects — and deliberately NOT gated on
    /// `MIRRICODE_DISABLE_CRON` (the preview is a display projection,
    /// not a scheduling face: a disabled scheduler may still render
    /// honest moments).
    pub(super) async fn dispatch_preview_cron(&self, params: Value) -> Result<Value, EngineError> {
        #[derive(Debug, Deserialize)]
        struct PreviewParams {
            /// The 5-field cron expression — exactly one of `cron` /
            /// `interval` must be present.
            #[serde(default)]
            cron: Option<String>,
            /// The elapsed-interval schedule (the alternative shape).
            #[serde(default)]
            interval: Option<mirri_core::cron::IntervalSpec>,
            /// The zone the schedule is evaluated in (`None` = the host's
            /// local zone — the form passes its visible default).
            #[serde(default)]
            timezone: Option<String>,
            /// How many occurrences to walk (the form asks for 2; capped
            /// at a sane bound).
            #[serde(default)]
            count: Option<usize>,
            /// The walk's anchor instant (`None` = now; RFC 3339 or epoch
            /// millis).
            #[serde(default)]
            anchor: Option<Value>,
        }
        let params: PreviewParams = serde_json::from_value(normalize_params(params))
            .map_err(|e| invalid_params(e.to_string()))?;
        let schedule = match (params.cron.as_deref(), &params.interval) {
            (Some(cron), None) if !cron.trim().is_empty() => {
                mirri_core::cron::ScheduleSpec::Cron(cron)
            }
            (None, Some(interval)) => mirri_core::cron::ScheduleSpec::Interval(interval),
            _ => {
                return Err(invalid_params(
                    "cron.preview requires exactly one of cron or interval",
                ));
            }
        };
        let count = params.count.unwrap_or(2).min(16);
        let anchor = match params.anchor.as_ref() {
            None => chrono::Utc::now(),
            Some(Value::Number(millis)) => {
                let millis = millis.as_i64().ok_or_else(|| {
                    invalid_params("anchor must be epoch millis or an RFC 3339 string")
                })?;
                chrono::DateTime::from_timestamp_millis(millis).ok_or_else(|| {
                    invalid_params("anchor must be epoch millis or an RFC 3339 string")
                })?
            }
            Some(Value::String(rfc3339)) => chrono::DateTime::parse_from_rfc3339(rfc3339)
                .map(|at| at.with_timezone(&chrono::Utc))
                .map_err(|e| invalid_params(format!("anchor is not RFC 3339: {e}")))?,
            Some(_) => {
                return Err(invalid_params(
                    "anchor must be epoch millis or an RFC 3339 string",
                ));
            }
        };
        let occurrences = mirri_core::cron::CronManager::preview_fires(
            &schedule,
            params.timezone.as_deref(),
            count,
            anchor,
        );
        let occurrences: Vec<i64> = occurrences.iter().map(|at| at.timestamp_millis()).collect();
        // The theoretical past (A5): the last ideal occurrence at or
        // before the anchor, `null` when there is none.
        let previous = mirri_core::cron::CronManager::prev_fire_for(
            &schedule,
            params.timezone.as_deref(),
            anchor,
        );
        Ok(json!({
            "occurrences": occurrences,
            "previousT0": previous.map(|at| at.timestamp_millis()),
        }))
    }

    /// `cron.runs.list` — one workspace task's run records (C.2: status,
    /// timing, usage summary, the execution session's reference) — the
    /// batch-5 panel's consumption face.
    pub(super) async fn dispatch_list_cron_runs(
        &self,
        params: Value,
    ) -> Result<Value, EngineError> {
        #[derive(Debug, Deserialize)]
        struct ListRunsParams {
            #[serde(default, alias = "workspaceId")]
            workspace_id: Option<String>,
            #[serde(alias = "taskId")]
            task_id: String,
        }
        let params: ListRunsParams = serde_json::from_value(normalize_params(params))
            .map_err(|e| invalid_params(e.to_string()))?;
        let workspace_id = self.resolve_cron_workspace(params.workspace_id.as_deref())?;
        let Some(runtime) = self.registry.built_runtime(&workspace_id) else {
            return Err(not_found(format!("workspace not found: {workspace_id}")));
        };
        let Some(cron) = runtime.cron() else {
            return Err(not_found(format!("workspace not found: {workspace_id}")));
        };
        if cron.get(&params.task_id).is_none() {
            return Err(not_found(format!(
                "cron task not found: {}",
                params.task_id
            )));
        }
        let runs: Vec<Value> = cron
            .runs(&params.task_id)
            .into_iter()
            .map(|run| {
                json!({
                    "runId": run.run_id,
                    "status": serde_json::to_value(run.status).unwrap_or(Value::Null),
                    "startedAt": run.started_at.to_rfc3339(),
                    "endedAt": run.ended_at.map(|at| at.to_rfc3339()),
                    "durationMs": run.duration_ms,
                    "usage": run.usage,
                    "sessionRef": run.session_ref,
                    "skipReason": run.skip_reason,
                    // ADR-021's launch record: what started the run, the
                    // moment the schedule said it was due, and how late it
                    // actually started (None for run-now / pre-record rows).
                    "trigger": run.trigger,
                    "scheduledAt": run.scheduled_at.map(|at| at.timestamp_millis()),
                    "launchDelayMs": run
                        .scheduled_at
                        .map(|at| (run.started_at - at).num_milliseconds().max(0)),
                    // V15 (stage-2 D.2): the asked-ops trace — which
                    // operation, what result (the panel's 留痕 rendering).
                    "askedOps": run.asked_ops,
                })
            })
            .collect();
        Ok(json!({ "runs": runs }))
    }

    /// `cron.run.open` — the explicit-open path (B.2/C.4, ADR-008): the
    /// user opening a run record from the task face materializes the
    /// execution session (write `state.json`, open `wire.jsonl`, flush
    /// the deferred buffer) — the ONLY admission into the session index.
    /// Unknown/already-materialized sessions answer with the current
    /// state (idempotent).
    pub(super) async fn dispatch_open_cron_run(&self, params: Value) -> Result<Value, EngineError> {
        #[derive(Debug, Deserialize)]
        struct OpenRunParams {
            #[serde(default, alias = "workspaceId")]
            workspace_id: Option<String>,
            #[serde(alias = "sessionId", alias = "session_id")]
            session_id: String,
        }
        let params: OpenRunParams = serde_json::from_value(normalize_params(params))
            .map_err(|e| invalid_params(e.to_string()))?;
        let workspace_id = self.resolve_cron_workspace(params.workspace_id.as_deref())?;
        let Some(runtime) = self.registry.built_runtime(&workspace_id) else {
            return Err(not_found(format!("workspace not found: {workspace_id}")));
        };
        // Materialize (idempotent — an already-materialized session is a
        // no-op `false`): the explicit user action IS the lazy-index
        // admission ADR-008 names.
        let manager = Arc::clone(&runtime.manager);
        let materialized = manager
            .materialize_session(&mirri_core::SessionId::new(&params.session_id))
            .await
            .map_err(|e| {
                mirri_transport::adapter::EngineError::Internal(format!(
                    "materializing the run session failed: {e}"
                ))
            })?;
        Ok(json!({
            "sessionId": params.session_id,
            "materialized": materialized,
        }))
    }

    /// The workspace a cron query face addresses: an explicit id, else the
    /// default workspace (the panel's implicit context).
    pub(super) fn resolve_cron_workspace(
        &self,
        workspace_id: Option<&str>,
    ) -> Result<String, EngineError> {
        match workspace_id {
            Some(id) => Ok(id.to_string()),
            None => self
                .registry
                .default_workspace_id()
                .ok_or_else(|| invalid_params("no default workspace is set")),
        }
    }
}
