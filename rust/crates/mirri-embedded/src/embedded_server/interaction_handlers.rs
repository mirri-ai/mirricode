//! Human-in-the-loop interaction dispatch handlers for the embedded
//! [`EmbeddedServer`](super::EmbeddedServer) — the B4-a blocking-HITL
//! surface (`approval.*` / `question.*` / `interaction.*`).
//!
//! The ask flows (`approval.ask` via the permission gate, `question.ask`
//! via the `AskUserQuestion` tool) register their pending requests with the
//! session's HITL gates (see the [`InteractionKernel`] wiring in
//! `session.create` / `message.send`). These handlers are the RPC face over
//! those gates:
//!
//! - `approval.decide` / `question.answer` / `question.dismiss` mirror the
//!   `mirri-server` handlers byte-for-byte on the wire (snake/camel
//!   tolerant params; bad uuid → invalid params, unknown id or missing
//!   session → not found).
//! - `approval.listPending` / `question.listPending` / `interaction.*`
//!   serve the klient shapes (camelCase rows, `createdAt` epoch
//!   milliseconds) straight from the gate snapshots.

use std::sync::Arc;

use serde::Deserialize;
use serde_json::{Value, json};

use mirri_transport::adapter::EngineError;

use mirri_core::{ApprovalDecision, InteractionRespondError};

use super::additional_dirs::session_grant_sink;
use super::responses::{DismissQuestionResult, ack, to_result};
use super::{EmbeddedServer, SessionIdParams, invalid_params, normalize_params, not_found};

impl EmbeddedServer {
    /// The addressed session's interaction kernel — the gates the ask flows
    /// registered with, plus the session's directory-grant sink (plan D3)
    /// so `interaction.respond` applies session/persist grants. `None`
    /// mirrors the (unreachable in practice) missing artifacts case as a
    /// not-found session.
    async fn interaction_kernel(
        &self,
        session_id: &str,
    ) -> Result<mirri_core::InteractionKernel, EngineError> {
        let artifacts = self
            .resolve_session_artifacts(session_id)
            .map_err(|_| not_found(format!("session not found: {session_id}")))?;
        let kernel = mirri_core::InteractionKernel::new(
            Arc::clone(&artifacts.approval_gate),
            Arc::clone(&artifacts.question_gate),
        );
        // Grant-capable sessions wire the sink (legacy sessions without a
        // workspace sandbox degrade to one-off approvals — the trait's
        // `None` semantics).
        let session = self.resolve_session(session_id).await.ok();
        Ok(match session {
            Some(session) => kernel.with_grant_sink(session_grant_sink(&session)),
            None => kernel,
        })
    }

    // --- approval / question resolve (SRV wire) --------------------------

    /// `approval.decide` — resolve a pending tool-approval request through
    /// the session's approval gate (the SRV wire shape: `approved` plus an
    /// optional `reason`, answered `{ok: true}`). An optional `scope:
    /// "session"` mirrors the klient `ApprovalResponse.scope` field: the
    /// decision is recorded as session-scoped and the approval-rule pattern
    /// is memorized for the rest of the session, so later same-pattern calls
    /// no longer ask (anything else or absent is a one-off decision). The
    /// reverse-RPC channel is the client's primary path — whichever channel
    /// resolves the request first wins; a late decide on the other channel
    /// finds the id gone.
    pub(super) async fn dispatch_approval_decide(
        &self,
        params: Value,
    ) -> Result<Value, EngineError> {
        #[derive(Debug, Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct DecideParams {
            #[serde(rename = "sessionId", alias = "session_id")]
            session_id: String,
            #[serde(rename = "requestId", alias = "request_id")]
            request_id: String,
            approved: bool,
            #[serde(default)]
            reason: Option<String>,
            /// `Some("session")` memorizes the approval as a session-scoped rule
            /// (the klient `ApprovalResponse.scope` field; anything else or absent
            /// is a one-off decision).
            #[serde(default)]
            scope: Option<String>,
        }
        let params: DecideParams = serde_json::from_value(normalize_params(params))
            .map_err(|e| invalid_params(e.to_string()))?;

        // The session must still be live for its approvals to be resolvable.
        let artifacts = self
            .resolve_session_artifacts(&params.session_id)
            .map_err(|_| not_found(params.session_id.clone()))?;

        let request_id = uuid::Uuid::parse_str(&params.request_id)
            .map_err(|e| invalid_params(format!("invalid request_id: {e}")))?;
        // Plan D3: a grant-carrying ask (the pending snapshot's `grant_dir`)
        // approved with `session`/`persist` scope applies the directory
        // grant through the session's sink and resolves plain `Approved` —
        // never `ApprovedForSession` (a directory grant is its own memory;
        // memorizing the tool-name pattern would over-approve every later
        // call of the same tool). Non-grant asks keep the pre-D3 mapping.
        let session = self.resolve_session(&params.session_id).await.ok();
        let decision = if params.approved {
            let pending = artifacts.approval_gate.snapshot(request_id);
            if let Some(dir) = pending.as_ref().and_then(|s| s.grant_dir.clone())
                && let Some(scope) = params.scope.as_deref()
                && (scope == "session" || scope == "persist")
            {
                if let Some(sink) = session.as_ref().and_then(|s| session_grant_sink(s)) {
                    sink.apply_grant(&dir, scope == "persist");
                }
                ApprovalDecision::Approved
            } else if params.scope.as_deref() == Some("session") {
                ApprovalDecision::ApprovedForSession
            } else {
                ApprovalDecision::Approved
            }
        } else {
            ApprovalDecision::Denied(
                params
                    .reason
                    .unwrap_or_else(|| "denied by user".to_string()),
            )
        };
        // Resolve through the session's own gate — approvals of other
        // sessions are never reachable (or affected) here.
        if !artifacts.approval_gate.resolve(request_id, decision) {
            return Err(not_found(format!(
                "approval request not found: {}",
                params.request_id
            )));
        }
        ack()
    }

    /// `question.answer` — answer a pending question through the session's
    /// question gate (`{ok: true}` on the SRV wire). The `answer` field is
    /// tolerant of the klient `QuestionResult` union: a plain string answers
    /// directly, anything else is serialized (the awaiting asker receives
    /// the JSON text).
    pub(super) async fn dispatch_answer_question(
        &self,
        params: Value,
    ) -> Result<Value, EngineError> {
        #[derive(Debug, Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct AnswerParams {
            #[serde(rename = "sessionId", alias = "session_id")]
            session_id: String,
            #[serde(rename = "requestId", alias = "request_id")]
            request_id: String,
            answer: Value,
        }
        let params: AnswerParams = serde_json::from_value(normalize_params(params))
            .map_err(|e| invalid_params(e.to_string()))?;

        let artifacts = self
            .resolve_session_artifacts(&params.session_id)
            .map_err(|_| not_found(params.session_id.clone()))?;

        let request_id = uuid::Uuid::parse_str(&params.request_id)
            .map_err(|e| invalid_params(format!("invalid request_id: {e}")))?;
        // The klient QuestionResult union: a string answers directly, any
        // other JSON value crosses as its serialization.
        let answer = match &params.answer {
            Value::String(text) => text.clone(),
            other => serde_json::to_string(other).map_err(|e| {
                invalid_params(format!("answer payload could not be serialized: {e}"))
            })?,
        };
        // Answer through the session's own gate — questions of other
        // sessions are never reachable (or affected) here.
        if !artifacts.question_gate.answer(request_id, answer) {
            return Err(not_found(format!(
                "question not found: {}",
                params.request_id
            )));
        }
        ack()
    }

    /// `question.dismiss` — dismiss a pending question without answering
    /// (`{ok: true, dismissed: true}` on the SRV wire). The awaiting asker
    /// resolves to its dismissed error path; a `QuestionDismissed` event is
    /// published on the session's bus.
    pub(super) async fn dispatch_dismiss_question(
        &self,
        params: Value,
    ) -> Result<Value, EngineError> {
        #[derive(Debug, Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct DismissParams {
            #[serde(rename = "sessionId", alias = "session_id")]
            session_id: String,
            #[serde(rename = "requestId", alias = "request_id")]
            request_id: String,
        }
        let params: DismissParams = serde_json::from_value(normalize_params(params))
            .map_err(|e| invalid_params(e.to_string()))?;

        let artifacts = self
            .resolve_session_artifacts(&params.session_id)
            .map_err(|_| not_found(params.session_id.clone()))?;

        let request_id = uuid::Uuid::parse_str(&params.request_id)
            .map_err(|e| invalid_params(format!("invalid request_id: {e}")))?;
        // Dismiss through the session's own gate — questions of other
        // sessions are never reachable (or affected) here.
        if !artifacts.question_gate.dismiss(request_id) {
            return Err(not_found(format!(
                "question not found: {}",
                params.request_id
            )));
        }
        to_result(DismissQuestionResult {
            ok: true,
            dismissed: true,
        })
    }

    // --- pending listings (klient shapes) ----------------------------------

    /// `approval.listPending` — the session's pending tool approvals in the
    /// klient `approvalRequest[]` wire shape (bare array; each row carries
    /// its `sessionId` — the klient row is session-addressed).
    pub(super) async fn dispatch_approval_list_pending(
        &self,
        params: Value,
    ) -> Result<Value, EngineError> {
        let params: SessionIdParams = serde_json::from_value(normalize_params(params))
            .map_err(|e| invalid_params(e.to_string()))?;
        let kernel = self.interaction_kernel(&params.session_id).await?;
        let mut rows = kernel.approval_list_pending();
        for row in &mut rows {
            row["sessionId"] = json!(params.session_id);
        }
        Ok(Value::Array(rows))
    }

    /// `question.listPending` — the session's pending questions in the klient
    /// `questionRequest[]` wire shape (bare array; the question items carry
    /// the klient `QuestionItem` spelling, `multiSelect`).
    pub(super) async fn dispatch_question_list_pending(
        &self,
        params: Value,
    ) -> Result<Value, EngineError> {
        let params: SessionIdParams = serde_json::from_value(normalize_params(params))
            .map_err(|e| invalid_params(e.to_string()))?;
        let kernel = self.interaction_kernel(&params.session_id).await?;
        Ok(Value::Array(kernel.question_list_pending()))
    }

    // --- interaction domain (klient shapes) -------------------------------

    /// `interaction.listPending` — the session's merged pending interactions
    /// in the klient envelope (`{id, kind, payload, origin, createdAt}`),
    /// filtered by `kind` (`approval` / `question`; `user_tool` has no rust
    /// engine source yet, so that filter always answers an empty list).
    pub(super) async fn dispatch_interaction_list_pending(
        &self,
        params: Value,
    ) -> Result<Value, EngineError> {
        #[derive(Debug, Deserialize)]
        struct ListPendingParams {
            #[serde(alias = "sessionId")]
            session_id: String,
            #[serde(default)]
            kind: Option<String>,
        }
        let params: ListPendingParams = serde_json::from_value(normalize_params(params))
            .map_err(|e| invalid_params(e.to_string()))?;
        let kernel = self.interaction_kernel(&params.session_id).await?;
        Ok(Value::Array(kernel.list_pending(params.kind.as_deref())))
    }

    /// `interaction.respond` — respond to one pending interaction by id.
    /// The response is interpreted for the id's kind (approval → the klient
    /// `{decision, feedback?}` shape; question → the answer payload) and
    /// routed to the owning gate. The klient contract models the result as
    /// `noResult` — JSON `null`.
    pub(super) async fn dispatch_interaction_respond(
        &self,
        params: Value,
    ) -> Result<Value, EngineError> {
        #[derive(Debug, Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct RespondParams {
            #[serde(rename = "sessionId", alias = "session_id")]
            session_id: String,
            #[serde(
                rename = "interactionId",
                alias = "interaction_id",
                alias = "requestId",
                alias = "request_id"
            )]
            interaction_id: String,
            response: Value,
        }
        let params: RespondParams = serde_json::from_value(normalize_params(params))
            .map_err(|e| invalid_params(e.to_string()))?;
        let kernel = self.interaction_kernel(&params.session_id).await?;
        kernel
            .respond(&params.interaction_id, params.response)
            .map_err(|e| match e {
                InteractionRespondError::NotFound(id) => {
                    not_found(format!("interaction not found: {id}"))
                }
                InteractionRespondError::InvalidParams(message) => invalid_params(message),
            })?;
        Ok(Value::Null)
    }

    /// `interaction.isRecentlyResolved` — whether the given request id was
    /// resolved recently (either gate's resolved ring still remembers it);
    /// the klient contract is a plain boolean.
    pub(super) async fn dispatch_interaction_is_recently_resolved(
        &self,
        params: Value,
    ) -> Result<Value, EngineError> {
        #[derive(Debug, Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct RecentlyResolvedParams {
            #[serde(rename = "sessionId", alias = "session_id")]
            session_id: String,
            #[serde(
                rename = "interactionId",
                alias = "interaction_id",
                alias = "requestId",
                alias = "request_id"
            )]
            interaction_id: String,
        }
        let params: RecentlyResolvedParams = serde_json::from_value(normalize_params(params))
            .map_err(|e| invalid_params(e.to_string()))?;
        let kernel = self.interaction_kernel(&params.session_id).await?;
        Ok(Value::Bool(
            kernel.is_recently_resolved(&params.interaction_id),
        ))
    }
}
