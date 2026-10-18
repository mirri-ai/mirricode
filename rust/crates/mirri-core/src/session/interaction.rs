//! The interaction kernel — the composition view over one session's HITL
//! gates (B4-a).
//!
//! [`InteractionKernel`] binds a session's [`ApprovalGate`] and
//! [`QuestionGate`] into the single read/respond surface the
//! `approval.listPending` / `question.listPending` / `interaction.*` RPC
//! methods speak: the gates are the pending source of truth (the ask flows
//! register their payloads there), and the kernel only projects and routes.
//!
//! ```text
//! approval.ask ──register──► ApprovalGate ─┐
//! question.ask ──register──► QuestionGate ─┼──► InteractionKernel ──► interaction.* / *.listPending
//!                                          ┘        (read + respond routing)
//! ```
//!
//! The kernel owns no state of its own — it holds the gates' `Arc`s, so a
//! fresh instance can be built from
//! [`SessionArtifacts`](super::artifacts::SessionArtifacts) at any time.
//! `user_tool` interactions have no rust engine source yet: the interaction
//! listing never produces `user_tool` rows (TSV note records the constant).

use serde_json::{Value, json};
use uuid::Uuid;

use super::gates::{
    AdditionalDirGrantSink, ApprovalDecision, ApprovalGate, ApprovalSnapshot, QuestionGate,
    QuestionSnapshot,
};

/// Failure modes of [`InteractionKernel::respond`].
#[derive(Debug, thiserror::Error)]
pub enum InteractionRespondError {
    /// The addressed request id is not pending in either gate (unknown or
    /// already resolved — the first resolution channel won).
    #[error("interaction not found: {0}")]
    NotFound(String),
    /// The response payload could not be interpreted for the addressed
    /// request's kind (the klient contract types `response` as `unknown`;
    /// a payload without a usable decision/answer is a client error, not a
    /// panic).
    #[error("invalid interaction response: {0}")]
    InvalidParams(String),
}

/// The composition view over one session's approval and question gates —
/// the `interaction.*` surface (B4-a).
#[derive(Clone)]
pub struct InteractionKernel {
    approval_gate: std::sync::Arc<ApprovalGate>,
    question_gate: std::sync::Arc<QuestionGate>,
    /// The session's directory-grant sink (plan D3): `approval.decide` /
    /// `interaction.respond` apply a granted dir through it when the ask
    /// carried a `grant_dir` and the response scope asked for session or
    /// persist. `None` (the default) degrades such a decision to a one-off
    /// approval — the pre-D3 behavior. The same instance rides the turn
    /// flow's hooks (via [`InteractionKernel::grant_sink`]) so the
    /// reverse-RPC reply branch applies the same grant semantics.
    grant_sink: Option<std::sync::Arc<dyn AdditionalDirGrantSink>>,
}

impl std::fmt::Debug for InteractionKernel {
    /// Manual `Debug`: `dyn AdditionalDirGrantSink` has no `Debug` impl, so
    /// print only whether a sink is attached.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InteractionKernel")
            .field("approval_gate", &self.approval_gate)
            .field("question_gate", &self.question_gate)
            .field("grant_sink", &self.grant_sink.is_some())
            .finish()
    }
}

impl InteractionKernel {
    /// Build a kernel over a session's two gates (no directory-grant sink —
    /// wire one with [`InteractionKernel::with_grant_sink`] when the host
    /// can apply directory grants).
    pub fn new(
        approval_gate: std::sync::Arc<ApprovalGate>,
        question_gate: std::sync::Arc<QuestionGate>,
    ) -> Self {
        Self {
            approval_gate,
            question_gate,
            grant_sink: None,
        }
    }

    /// Attach (or clear) the session's directory-grant sink.
    ///
    /// The embedded host builds the same sink the session's other surfaces
    /// use (a closure over the session's shared `WorkspacePaths` plus the
    /// project root); `None` keeps the one-off-degradation behavior.
    pub fn with_grant_sink(
        mut self,
        grant_sink: Option<std::sync::Arc<dyn AdditionalDirGrantSink>>,
    ) -> Self {
        self.grant_sink = grant_sink;
        self
    }

    /// The bound directory-grant sink, when the host wired one.
    pub fn grant_sink(&self) -> Option<std::sync::Arc<dyn AdditionalDirGrantSink>> {
        self.grant_sink.clone()
    }

    /// The bound approval gate (the ask-flow registration seam).
    pub fn approval_gate(&self) -> &std::sync::Arc<ApprovalGate> {
        &self.approval_gate
    }

    /// The bound question gate (the ask-flow registration seam).
    pub fn question_gate(&self) -> &std::sync::Arc<QuestionGate> {
        &self.question_gate
    }

    /// The pending approvals in the klient `approvalRequest` wire shape
    /// (`{id, agentId?, turnId?, toolCallId?, toolName, action, display}` —
    /// `sessionId` is added by the session-addressed RPC handler).
    pub fn approval_list_pending(&self) -> Vec<Value> {
        self.approval_gate
            .list_pending()
            .into_iter()
            .map(|(id, snapshot)| approval_request_row(id, &snapshot))
            .collect()
    }

    /// The pending questions in the klient `questionRequest` wire shape
    /// (`{id, turnId?, toolCallId?, questions}` — the question items carry
    /// the klient `QuestionItem` spelling, `multiSelect`).
    pub fn question_list_pending(&self) -> Vec<Value> {
        self.question_gate
            .list_pending()
            .into_iter()
            .map(|(id, snapshot)| question_request_row(id, &snapshot))
            .collect()
    }

    /// The merged pending list in the klient `interaction` wire shape
    /// (`{id, kind, payload, origin: {agentId?, turnId?}, createdAt}`),
    /// filtered by kind when given. `user_tool` has no rust engine source
    /// yet, so that filter always answers an empty list.
    pub fn list_pending(&self, kind: Option<&str>) -> Vec<Value> {
        let mut rows = Vec::new();
        if kind.is_none_or(|k| k == "approval") {
            rows.extend(
                self.approval_gate
                    .list_pending()
                    .into_iter()
                    .map(|(id, snapshot)| {
                        interaction_row(
                            id,
                            "approval",
                            &approval_request_row(id, &snapshot),
                            &snapshot,
                        )
                    }),
            );
        }
        if kind.is_none_or(|k| k == "question") {
            rows.extend(
                self.question_gate
                    .list_pending()
                    .into_iter()
                    .map(|(id, snapshot)| {
                        interaction_row(
                            id,
                            "question",
                            &question_request_row(id, &snapshot),
                            &snapshot,
                        )
                    }),
            );
        }
        // `user_tool` interactions have no rust engine source yet — the
        // listing never produces them (TSV note records the constant).
        rows
    }

    /// Respond to one pending request by id — the `interaction.respond`
    /// routing: the id's gate is located first, then the response is
    /// interpreted for that gate's kind (approval → decision, question →
    /// answer). First resolution wins; a later response on the other
    /// channel finds the id gone and errors.
    pub fn respond(&self, id: &str, response: Value) -> Result<(), InteractionRespondError> {
        // A malformed id is simply an id no gate knows: not-found (the
        // response payload — not the id — is what invalid-params guards).
        let request_id =
            Uuid::parse_str(id).map_err(|_| InteractionRespondError::NotFound(id.to_string()))?;

        if self.approval_gate.is_pending(request_id) {
            // Approval branch: the response carries the klient
            // approvalResponse shape `{decision, scope?, feedback?}` (parsed
            // leniently — the contract types the payload `unknown`).
            let decision = response
                .get("decision")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    InteractionRespondError::InvalidParams(
                        "approval response requires a 'decision' string".to_string(),
                    )
                })?;
            let scope = response.get("scope").and_then(Value::as_str);
            let mut approval = match decision {
                "approved" => match scope {
                    Some("session") => ApprovalDecision::ApprovedForSession,
                    _ => ApprovalDecision::Approved,
                },
                "rejected" => ApprovalDecision::Denied(
                    response
                        .get("feedback")
                        .and_then(Value::as_str)
                        .unwrap_or("denied by user")
                        .to_string(),
                ),
                "cancelled" => ApprovalDecision::Denied("cancelled by user".to_string()),
                other => {
                    return Err(InteractionRespondError::InvalidParams(format!(
                        "unknown approval decision: {other}"
                    )));
                }
            };
            // Plan D3: a grant-carrying ask applies its directory grant at
            // the decision source — BEFORE the gate resolves — when the
            // response approves with session or persist scope. The gate then
            // resolves plain `Approved` (never `ApprovedForSession`): the
            // directory grant itself is the memory, and the tool-name
            // pattern must not be memorized ("approve Read for session"
            // would over-approve every later read).
            if decision == "approved"
                && let Some(snapshot) = self.approval_gate.snapshot(request_id)
                && let Some(dir) = snapshot.grant_dir
            {
                match scope {
                    Some("session") | Some("persist") => {
                        if let Some(sink) = &self.grant_sink {
                            sink.apply_grant(&dir, scope == Some("persist"));
                        }
                        approval = ApprovalDecision::Approved;
                    }
                    _ => {}
                }
            }
            if !self.approval_gate.resolve(request_id, approval) {
                return Err(InteractionRespondError::NotFound(id.to_string()));
            }
            return Ok(());
        }

        if self.question_gate.is_pending(request_id) {
            // Question branch: the response is the answer payload — a
            // plain string answers directly, anything else is serialized
            // (the klient QuestionResult union: null | answers | response).
            let answer = match &response {
                Value::Null => String::new(),
                Value::String(text) => text.clone(),
                other => serde_json::to_string(other)
                    .map_err(|e| InteractionRespondError::InvalidParams(e.to_string()))?,
            };
            if !self.question_gate.answer(request_id, answer) {
                return Err(InteractionRespondError::NotFound(id.to_string()));
            }
            return Ok(());
        }

        Err(InteractionRespondError::NotFound(id.to_string()))
    }

    /// Whether the given request id was resolved recently — either gate's
    /// resolved ring still remembers it.
    pub fn is_recently_resolved(&self, id: &str) -> bool {
        match Uuid::parse_str(id) {
            Ok(request_id) => {
                self.approval_gate.is_recently_resolved(request_id)
                    || self.question_gate.is_recently_resolved(request_id)
            }
            Err(_) => false,
        }
    }
}

/// One klient `approvalRequest` row for a pending approval snapshot.
fn approval_request_row(id: Uuid, snapshot: &ApprovalSnapshot) -> Value {
    let mut row = json!({
        "id": id.to_string(),
        "toolName": snapshot.tool_name,
        "action": snapshot.action,
        "display": snapshot.display,
    });
    if !snapshot.agent_id.is_nil() {
        row["agentId"] = json!(snapshot.agent_id.to_string());
    }
    if let Some(turn_id) = snapshot.turn_id {
        row["turnId"] = json!(turn_id);
    }
    if let Some(tool_call_id) = &snapshot.tool_call_id {
        row["toolCallId"] = json!(tool_call_id);
    }
    // The engine approval-rule facet the call was gated on (B9): present
    // only when the tool reported one — the klient field is optional.
    if let Some(approval_rule) = &snapshot.approval_rule {
        row["approvalRule"] = json!(approval_rule);
    }
    // The grantable directory (plan D3): present only when the ask carries
    // one — the klient shows a once/session/persist grant choice for it.
    if let Some(grant_dir) = &snapshot.grant_dir {
        row["grant"] = json!({ "dir": grant_dir });
    }
    row
}

/// One klient `questionRequest` row for a pending question snapshot — the
/// question items keep the klient `QuestionItem` camelCase spelling
/// (`multi_select` → `multiSelect`).
fn question_request_row(id: Uuid, snapshot: &QuestionSnapshot) -> Value {
    let mut row = json!({
        "id": id.to_string(),
        "questions": klient_question_items(&snapshot.questions),
    });
    if let Some(turn_id) = snapshot.turn_id {
        row["turnId"] = json!(turn_id);
    }
    if let Some(tool_call_id) = &snapshot.tool_call_id {
        row["toolCallId"] = json!(tool_call_id);
    }
    row
}

/// One klient `interaction` row: the request's kind-specific payload under
/// the shared envelope (`origin`, `createdAt`).
fn interaction_row<T: InteractionOrigin>(
    id: Uuid,
    kind: &str,
    payload: &Value,
    snapshot: &T,
) -> Value {
    let mut origin = json!({});
    if !snapshot.interaction_agent_id().is_nil() {
        origin["agentId"] = json!(snapshot.interaction_agent_id().to_string());
    }
    if let Some(turn_id) = snapshot.interaction_turn_id() {
        origin["turnId"] = json!(turn_id);
    }
    json!({
        "id": id.to_string(),
        "kind": kind,
        "payload": payload,
        "origin": origin,
        "createdAt": snapshot.interaction_created_at(),
    })
}

/// The origin/createdAt fields the interaction envelope reads, shared by
/// both snapshot types.
trait InteractionOrigin {
    fn interaction_agent_id(&self) -> Uuid;
    fn interaction_turn_id(&self) -> Option<u64>;
    fn interaction_created_at(&self) -> u64;
}

impl InteractionOrigin for ApprovalSnapshot {
    fn interaction_agent_id(&self) -> Uuid {
        self.agent_id
    }
    fn interaction_turn_id(&self) -> Option<u64> {
        self.turn_id
    }
    fn interaction_created_at(&self) -> u64 {
        self.created_at
    }
}

impl InteractionOrigin for QuestionSnapshot {
    fn interaction_agent_id(&self) -> Uuid {
        self.agent_id
    }
    fn interaction_turn_id(&self) -> Option<u64> {
        self.turn_id
    }
    fn interaction_created_at(&self) -> u64 {
        self.created_at
    }
}

/// Normalize the raw question items into the klient `QuestionItem[]`
/// spelling: `multi_select` (the `AskUserQuestion` tool's parameter
/// spelling) becomes `multiSelect`; every item also carries `allowOther:
/// true` (the engine always advertises the free-text Other option — the
/// tool's model-facing description promises it); every other field passes
/// through untouched.
fn klient_question_items(questions: &Value) -> Value {
    let Some(items) = questions.as_array() else {
        return questions.clone();
    };
    Value::Array(
        items
            .iter()
            .map(|item| {
                let mut item = item.clone();
                if let Some(obj) = item.as_object_mut() {
                    if let Some(multi) = obj.remove("multi_select") {
                        obj.insert("multiSelect".to_string(), multi);
                    }
                    obj.insert("allowOther".to_string(), serde_json::Value::Bool(true));
                }
                item
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::gates::now_ms;
    use std::sync::Arc;

    fn kernel_with_pending() -> (InteractionKernel, Uuid, Uuid) {
        let approval_gate = Arc::new(ApprovalGate::new(false));
        let question_gate = Arc::new(QuestionGate::new());
        let (approval_id, _rx) = approval_gate.register(ApprovalSnapshot {
            tool_name: "Bash".to_string(),
            action: "execute Bash".to_string(),
            display: json!({ "summary": "echo hi" }),
            agent_id: Uuid::nil(),
            turn_id: Some(3),
            tool_call_id: Some("call_a".to_string()),
            created_at: 1_000,
            approval_rule: None,
            grant_dir: None,
        });
        let (question_id, _rx) = question_gate.register(QuestionSnapshot {
            questions: json!([{
                "question": "Which?",
                "multi_select": true,
                "options": [{ "label": "A" }, { "label": "B" }],
            }]),
            agent_id: Uuid::nil(),
            turn_id: Some(3),
            tool_call_id: Some("call_q".to_string()),
            created_at: 1_001,
        });
        (
            InteractionKernel::new(approval_gate, question_gate),
            approval_id,
            question_id,
        )
    }

    /// Given a pending approval registered with an approval rule facet, when
    /// the klient rows are projected, then both the approvalRequest row and
    /// the interaction payload carry the facet under `approvalRule`.
    #[test]
    fn should_expose_approval_rule_on_pending_rows_when_snapshot_carries_one() {
        let gate = Arc::new(ApprovalGate::new(false));
        let (id, _rx) = gate.register(ApprovalSnapshot {
            tool_name: "Bash".to_string(),
            action: "execute Bash".to_string(),
            display: json!({}),
            agent_id: Uuid::nil(),
            turn_id: Some(3),
            tool_call_id: Some("call_a".to_string()),
            created_at: 1_000,
            approval_rule: Some("Bash".to_string()),
            grant_dir: None,
        });
        let kernel = InteractionKernel::new(gate, Arc::new(QuestionGate::new()));

        let rows = kernel.approval_list_pending();
        assert_eq!(rows[0]["approvalRule"], json!("Bash"));
        let interactions = kernel.list_pending(Some("approval"));
        assert_eq!(interactions[0]["payload"]["approvalRule"], json!("Bash"));

        // And a snapshot without the facet omits the field entirely (the
        // klient type is optional; absent ≠ empty).
        let bare_gate = Arc::new(ApprovalGate::new(false));
        let (bare_id, _rx) = bare_gate.register(ApprovalSnapshot {
            tool_name: "Bash".to_string(),
            action: "execute Bash".to_string(),
            display: json!({}),
            agent_id: Uuid::nil(),
            turn_id: Some(3),
            tool_call_id: Some("call_b".to_string()),
            created_at: 1_000,
            approval_rule: None,
            grant_dir: None,
        });
        let bare_kernel = InteractionKernel::new(bare_gate, Arc::new(QuestionGate::new()));
        let bare_rows = bare_kernel.approval_list_pending();
        assert!(
            bare_rows[0].get("approvalRule").is_none(),
            "no facet on the snapshot — no approvalRule on the row"
        );
        let _ = (id, bare_id);
    }

    /// Given approvals and questions pending, when the interaction list is
    /// read unfiltered, then both kinds appear in the klient envelope with
    /// camelCase question items and real timestamps.
    #[test]
    fn should_list_both_kinds_when_interaction_list_pending_unfiltered() {
        let (kernel, approval_id, question_id) = kernel_with_pending();

        let rows = kernel.list_pending(None);

        assert_eq!(rows.len(), 2);
        let approval_row = rows
            .iter()
            .find(|row| row["kind"] == json!("approval"))
            .expect("approval row");
        assert_eq!(approval_row["id"], json!(approval_id.to_string()));
        assert_eq!(approval_row["origin"]["turnId"], json!(3));
        assert_eq!(approval_row["createdAt"], json!(1_000));
        assert_eq!(approval_row["payload"]["toolName"], json!("Bash"));
        let question_row = rows
            .iter()
            .find(|row| row["kind"] == json!("question"))
            .expect("question row");
        assert_eq!(question_row["id"], json!(question_id.to_string()));
        assert_eq!(
            question_row["payload"]["questions"][0]["multiSelect"],
            json!(true),
            "multi_select must be normalized to the klient multiSelect"
        );
        assert_eq!(
            question_row["payload"]["questions"][0]["allowOther"],
            json!(true),
            "every listed question must advertise the klient allowOther"
        );
        // And the kind filter selects exactly one row.
        assert_eq!(kernel.list_pending(Some("approval")).len(), 1);
        assert_eq!(kernel.list_pending(Some("question")).len(), 1);
        // And user_tool has no rust source — the filter answers nothing.
        assert!(kernel.list_pending(Some("user_tool")).is_empty());
    }

    /// Given a pending approval, when respond carries an approval decision,
    /// then the approval resolves and the id becomes recently resolved.
    #[test]
    fn should_resolve_approval_when_interaction_respond_carries_decision() {
        let (kernel, approval_id, _question_id) = kernel_with_pending();

        kernel
            .respond(&approval_id.to_string(), json!({ "decision": "approved" }))
            .expect("approval responds");

        assert!(kernel.is_recently_resolved(&approval_id.to_string()));
        assert!(
            kernel.approval_gate.pending_count() == 0,
            "the approval left the pending map"
        );
    }

    /// Given a pending question, when respond carries the answer payload,
    /// then the question resolves.
    #[test]
    fn should_answer_question_when_interaction_respond_carries_answer() {
        let (kernel, _approval_id, question_id) = kernel_with_pending();

        kernel
            .respond(&question_id.to_string(), json!({ "answers": { "q": "A" } }))
            .expect("question responds");

        assert!(kernel.is_recently_resolved(&question_id.to_string()));
    }

    /// Given no pending request with the id, when respond is called, then it
    /// answers not-found — and an approval response without a decision
    /// answers invalid-params, never a panic.
    #[test]
    fn should_error_when_interaction_respond_targets_unknown_or_unparsable() {
        let (kernel, approval_id, _question_id) = kernel_with_pending();

        let unknown = kernel.respond("no-such-id", json!({ "decision": "approved" }));
        assert!(matches!(unknown, Err(InteractionRespondError::NotFound(_))));

        let unparsable = kernel.respond(
            &approval_id.to_string(),
            json!({ "feedback": "no decision" }),
        );
        assert!(matches!(
            unparsable,
            Err(InteractionRespondError::InvalidParams(_))
        ));

        let malformed_uuid = kernel.respond("not-a-uuid", json!({}));
        assert!(matches!(
            malformed_uuid,
            Err(InteractionRespondError::NotFound(_))
        ));
    }

    /// Given an unknown id, when isRecentlyResolved is queried, then the
    /// answer is false (and a malformed id never panics).
    #[test]
    fn should_answer_false_when_is_recently_resolved_queries_unknown_id() {
        let (kernel, _a, _q) = kernel_with_pending();

        assert!(!kernel.is_recently_resolved(&Uuid::new_v4().to_string()));
        assert!(!kernel.is_recently_resolved("not-a-uuid"));
    }

    /// Given a pending approval registered on the kernel's gate, when
    /// `interaction.respond` carries `{decision: "approved", scope: "session"}`,
    /// then the awaiting receiver resolves with the session-scoped variant.
    #[tokio::test]
    async fn should_memorize_session_scope_when_interaction_respond_carries_it() {
        // Given an interaction kernel whose approval gate holds one pending ask
        let approval_gate = Arc::new(ApprovalGate::new(false));
        let question_gate = Arc::new(QuestionGate::new());
        let kernel = InteractionKernel::new(Arc::clone(&approval_gate), Arc::clone(&question_gate));
        let (request_id, receiver) = approval_gate.register(ApprovalSnapshot {
            tool_name: "Bash".to_string(),
            action: "execute Bash".to_string(),
            display: json!({}),
            agent_id: Uuid::nil(),
            turn_id: None,
            tool_call_id: Some("c1".to_string()),
            created_at: now_ms(),
            approval_rule: Some("Bash".to_string()),
            grant_dir: None,
        });

        // When the response carries the session scope
        kernel
            .respond(
                &request_id.to_string(),
                json!({ "decision": "approved", "scope": "session" }),
            )
            .expect("the approval resolves");

        // Then the awaiting side receives the session-scoped approval
        let decision = receiver.await.expect("the receiver resolves");
        assert!(matches!(decision, ApprovalDecision::ApprovedForSession));

        // And a scope-less approval still resolves to the plain variant
        let (request_id2, receiver2) = approval_gate.register(ApprovalSnapshot {
            tool_name: "Bash".to_string(),
            action: "execute Bash".to_string(),
            display: json!({}),
            agent_id: Uuid::nil(),
            turn_id: None,
            tool_call_id: Some("c2".to_string()),
            created_at: now_ms(),
            approval_rule: None,
            grant_dir: None,
        });
        kernel
            .respond(&request_id2.to_string(), json!({ "decision": "approved" }))
            .expect("the approval resolves");
        let decision2 = receiver2.await.expect("the receiver resolves");
        assert!(matches!(decision2, ApprovalDecision::Approved));
    }

    /// A test grant sink that records every `(dir, persist)` application.
    #[derive(Default)]
    struct RecordingGrantSink {
        calls: Arc<std::sync::Mutex<Vec<(std::path::PathBuf, bool)>>>,
    }

    impl crate::session::gates::AdditionalDirGrantSink for RecordingGrantSink {
        fn apply_grant(&self, dir: &std::path::Path, persist: bool) {
            self.calls
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push((dir.to_path_buf(), persist));
        }
    }

    /// One pending grant-carrying approval on a kernel wired with a
    /// recording grant sink; returns `(kernel, request_id, receiver, sink,
    /// granted_dir)`.
    #[allow(clippy::type_complexity)]
    fn kernel_with_pending_grant_ask() -> (
        InteractionKernel,
        Uuid,
        tokio::sync::oneshot::Receiver<ApprovalDecision>,
        Arc<RecordingGrantSink>,
        std::path::PathBuf,
    ) {
        let approval_gate = Arc::new(ApprovalGate::new(false));
        let question_gate = Arc::new(QuestionGate::new());
        let sink = Arc::new(RecordingGrantSink::default());
        let kernel = InteractionKernel::new(Arc::clone(&approval_gate), Arc::clone(&question_gate))
            .with_grant_sink(Some(Arc::clone(&sink) as Arc<dyn AdditionalDirGrantSink>));
        let granted_dir = std::path::PathBuf::from("/tmp/outside-granted-dir");
        let (request_id, receiver) = approval_gate.register(ApprovalSnapshot {
            tool_name: "Read".to_string(),
            action: "execute Read".to_string(),
            display: json!({}),
            agent_id: Uuid::nil(),
            turn_id: None,
            tool_call_id: Some("call_read".to_string()),
            created_at: now_ms(),
            approval_rule: Some("Read".to_string()),
            grant_dir: Some(granted_dir.clone()),
        });
        (kernel, request_id, receiver, sink, granted_dir)
    }

    /// Given a grant-carrying pending approval, when `interaction.respond`
    /// approves it with session scope, then the directory grant is applied
    /// to the sink (not persisted) and the gate resolves plain `Approved` —
    /// never the session-scoped variant (a directory grant is its own
    /// memory; the tool-name pattern must not be memorized).
    #[tokio::test]
    async fn should_apply_session_dir_grant_when_respond_approves_a_grant_ask_with_session_scope() {
        let (kernel, request_id, receiver, sink, granted_dir) = kernel_with_pending_grant_ask();

        kernel
            .respond(
                &request_id.to_string(),
                json!({ "decision": "approved", "scope": "session" }),
            )
            .expect("the approval resolves");

        let calls = sink.calls.lock().unwrap_or_else(|e| e.into_inner()).clone();
        assert_eq!(
            calls,
            vec![(granted_dir, false)],
            "session scope must apply the dir grant without persisting"
        );
        let decision = receiver.await.expect("the receiver resolves");
        assert!(
            matches!(decision, ApprovalDecision::Approved),
            "a grant ask must resolve plain Approved, not ApprovedForSession, got {decision:?}"
        );
    }

    /// Given a grant-carrying pending approval, when `interaction.respond`
    /// approves it with persist scope, then the directory grant is applied
    /// with the persist flag and the gate resolves plain `Approved`.
    #[tokio::test]
    async fn should_apply_persisted_dir_grant_when_respond_approves_a_grant_ask_with_persist_scope()
    {
        let (kernel, request_id, receiver, sink, granted_dir) = kernel_with_pending_grant_ask();

        kernel
            .respond(
                &request_id.to_string(),
                json!({ "decision": "approved", "scope": "persist" }),
            )
            .expect("the approval resolves");

        let calls = sink.calls.lock().unwrap_or_else(|e| e.into_inner()).clone();
        assert_eq!(
            calls,
            vec![(granted_dir, true)],
            "persist scope must apply the dir grant with persist=true"
        );
        let decision = receiver.await.expect("the receiver resolves");
        assert!(matches!(decision, ApprovalDecision::Approved));
    }

    /// Given a grant-carrying pending approval, when `interaction.respond`
    /// approves it without a session/persist scope (once is the default),
    /// then no grant is applied — the approval is a one-off decision.
    #[tokio::test]
    async fn should_not_apply_dir_grant_when_respond_approves_a_grant_ask_with_once_scope() {
        let (kernel, request_id, receiver, sink, _granted_dir) = kernel_with_pending_grant_ask();

        kernel
            .respond(
                &request_id.to_string(),
                json!({ "decision": "approved", "scope": "once" }),
            )
            .expect("the approval resolves");

        let calls = sink.calls.lock().unwrap_or_else(|e| e.into_inner()).clone();
        assert!(
            calls.is_empty(),
            "a once-scope approval must not apply any grant"
        );
        let decision = receiver.await.expect("the receiver resolves");
        assert!(matches!(decision, ApprovalDecision::Approved));
    }

    /// Given a grant-carrying pending approval, when `interaction.respond`
    /// rejects (or cancels) it, then no grant is applied and the gate
    /// resolves denied.
    #[tokio::test]
    async fn should_not_apply_dir_grant_when_respond_rejects_a_grant_ask() {
        let (kernel, request_id, receiver, sink, _granted_dir) = kernel_with_pending_grant_ask();

        kernel
            .respond(
                &request_id.to_string(),
                json!({ "decision": "rejected", "feedback": "no" }),
            )
            .expect("the approval resolves");

        let calls = sink.calls.lock().unwrap_or_else(|e| e.into_inner()).clone();
        assert!(calls.is_empty(), "a rejected ask must not apply any grant");
        let decision = receiver.await.expect("the receiver resolves");
        assert!(matches!(decision, ApprovalDecision::Denied(feedback) if feedback == "no"));
    }
}
