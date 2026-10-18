//! B3 T10: swarm scheduler integration — the 429 → same-identity retry loop.
//!
//! One shared `ScriptedLlm` serves the main agent and both swarm members
//! (FIFO): the main turn dispatches an AgentSwarm tool call, each member's
//! first attempt exhausts the step-retry ladder with 429s (three chat calls
//! per member — one initial + two step retries), the scheduler parks both
//! (`subagent.suspended`) and requeues them on the SAME identities via the
//! retry-shaped resume after the 3s backoff (real time — the assembly has no
//! test seam for `SwarmBatchOptions::retry_base_ms`, accepted per plan),
//! and the retry turns succeed.

use std::sync::Arc;

use serde_json::json;

use super::b1_task_lifecycle_tests::{await_chat_calls, b1_profiles_server_with_config};
use super::*;
use mirri_core::llm::LlmToolCall;
use mirri_transport::adapter::ReverseRpcHandler;
use mirri_transport::frame::JsonRpcNotification;

/// Given a main agent scripted to call AgentSwarm with 2 items, and a shared
/// subagent LLM whose member turns first fail with 429s then succeed, When
/// the main turn completes, Then both members recover on their ORIGINAL
/// agent identities (no extra subagent.spawned after the retry — D-43), the
/// session event stream carries subagent.suspended for both, and the tool
/// result reports completed: 2.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn should_retry_rate_limited_swarm_members_on_the_same_identity() {
    let mock = Arc::new(ScriptedLlm::new());
    // 1. Main agent → AgentSwarm with 2 items.
    mock.push_tool_use(vec![LlmToolCall {
        id: "call_0".to_string(),
        name: "AgentSwarm".to_string(),
        args: json!({
            "description": "rate limited swarm",
            "subagent_type": "coder",
            "prompt_template": "P: {{item}}",
            "items": ["a", "b"]
        }),
    }]);
    // 2. Both members' first attempts — three 429s each exhaust the
    //    step-retry ladder (chat_with_retry itself never retries: the mock
    //    keeps `is_retryable_error` at its default false).
    for _ in 0..6 {
        mock.push_error("HTTP 429: too many requests");
    }
    // 3. The retry turns succeed (after the 3s requeue backoff).
    mock.push_text("member result 1");
    mock.push_text("member result 2");
    // 4. Main agent closes on the swarm result.
    mock.push_text("all done");

    let (server, _tmp) = b1_profiles_server_with_config(
        Arc::new(ScriptedLlmFactory::new(Arc::clone(&mock))),
        "",
        std::time::Duration::from_millis(50),
    );
    let mut events = server.subscribe_events().await.unwrap();
    // A titled session — the test counts the mock's calls exactly, and an
    // untitled session's settle would spawn the side title call too.
    let created = server
        .call_method(
            "session.create",
            json!({ "permissionMode": "yolo", "title": "swarm retry" }),
        )
        .await
        .expect("create");
    let session_id = created["id"].as_str().unwrap().to_string();
    server
        .call_method(
            "message.send",
            json!({
                "session_id": session_id,
                "text": "run the swarm",
                "mode": "fire"
            }),
        )
        .await
        .expect("send");

    // 10 chat calls: 1 main dispatch + 6 member 429s + 2 member retries +
    // 1 main close. The retry backoff is ~3s real time; 30s is generous.
    await_chat_calls(&mock, 10).await;
    assert_eq!(mock.chat_call_count(), 10);

    // The final main-agent call's input carries the swarm tool result: both
    // members completed via their retries, results in the XML.
    let final_messages = mock.messages_of_call(9).to_string();
    assert!(
        final_messages.contains("completed: 2"),
        "the swarm result must report both members completed: {final_messages}"
    );
    assert!(
        final_messages.contains("member result 1"),
        "the retry result must reach the model: {final_messages}"
    );

    // The event stream: exactly 2 spawned (the retry reuses identities —
    // D-43's "no new spawned"), 2 suspended (the parking announcement,
    // machine-readable `rate_limit` reasons per ADR-011), 2 completed (the
    // retry turns' terminal), and no failed events — the rate-limit
    // failure events are suppressed on the requeue path.
    let mut spawned = 0;
    let mut suspended = 0;
    let mut completed = 0;
    let mut failed = 0;
    let mut subagent_events: Vec<(String, Value)> = Vec::new();
    while let Ok(notification) = events.try_recv() {
        match notification.method.as_str() {
            "event.subagent.spawned" => spawned += 1,
            "event.subagent.suspended" => {
                suspended += 1;
                subagent_events.push((notification.method, notification.params.clone()));
            }
            "event.subagent.completed" => {
                completed += 1;
                subagent_events.push((notification.method, notification.params.clone()));
            }
            "event.subagent.failed" => failed += 1,
            "event.subagent.started" => {
                subagent_events.push((notification.method, notification.params.clone()));
            }
            _ => {}
        }
    }
    assert_eq!(
        spawned, 2,
        "the retry must reuse the original identities (no new spawned)"
    );
    assert_eq!(
        suspended, 2,
        "each parked member must announce its suspension exactly once"
    );
    assert_eq!(completed, 2, "both retry turns complete");
    assert_eq!(failed, 0, "suppressed rate-limit failures publish nothing");
    assert!(
        subagent_events
            .iter()
            .filter(|(method, _)| method == "event.subagent.suspended")
            .all(|(_, params)| params["reason"] == json!({ "kind": "rate_limit" })),
        "parked members' reasons are machine-readable rate_limit: {subagent_events:?}"
    );
    assert_suspension_pairing(&subagent_events);
}

/// Given a subagent that dispatches an AgentSwarm batch whose members first
/// exhaust the step-retry ladder with 429s, When the nested scheduler
/// parks and requeues them, Then both members recover on their ORIGINAL
/// identities through the nested host's retry-shaped resume (B4 T9,
/// A14: the B3 retry face under nesting).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn should_requeue_a_rate_limited_nested_member_on_the_same_identity() {
    let mock = Arc::new(ScriptedLlm::new());
    // 1. Main agent dispatches a coder subagent.
    mock.push_tool_use(vec![LlmToolCall {
        id: "call_0".to_string(),
        name: "Agent".to_string(),
        args: json!({
            "subagent_type": "coder",
            "prompt": "run the nested swarm",
            "description": "nested swarm dispatch"
        }),
    }]);
    // 2. The child dispatches an AgentSwarm batch with 2 items.
    mock.push_tool_use(vec![LlmToolCall {
        id: "call_1".to_string(),
        name: "AgentSwarm".to_string(),
        args: json!({
            "description": "nested rate limited swarm",
            "subagent_type": "coder",
            "prompt_template": "N: {{item}}",
            "items": ["a", "b"]
        }),
    }]);
    // 3. Both members' first attempts — three 429s each exhaust the
    //    step-retry ladder.
    for _ in 0..6 {
        mock.push_error("HTTP 429: too many requests");
    }
    // 4. The retry turns succeed (after the requeue backoff).
    mock.push_text("nested member result 1");
    mock.push_text("nested member result 2");
    // 5. The child closes on the swarm result; 6. main closes on the child.
    mock.push_text("nested child summary");
    mock.push_text("all done");

    let (server, _tmp) = b1_profiles_server_with_config(
        Arc::new(ScriptedLlmFactory::new(Arc::clone(&mock))),
        "",
        std::time::Duration::from_millis(50),
    );
    let mut events = server.subscribe_events().await.unwrap();
    // A titled session — the test counts the mock's calls exactly, and an
    // untitled session's settle would spawn the side title call too.
    let created = server
        .call_method(
            "session.create",
            json!({ "permissionMode": "yolo", "title": "nested swarm retry" }),
        )
        .await
        .expect("create");
    let session_id = created["id"].as_str().unwrap().to_string();
    server
        .call_method(
            "message.send",
            json!({
                "session_id": session_id,
                "text": "run the nested swarm",
                "mode": "fire"
            }),
        )
        .await
        .expect("send");

    // 12 chat calls: 1 main dispatch + 1 child dispatch + 6 member 429s +
    // 2 member retries + 1 child close + 1 main close.
    await_chat_calls(&mock, 12).await;
    assert_eq!(mock.chat_call_count(), 12);

    // The main agent's final call carries the child's summary; the child's
    // final call carries the swarm result with both members completed.
    let main_final = mock.messages_of_call(11).to_string();
    assert!(
        main_final.contains("nested child summary"),
        "the nested child's summary must reach the main agent: {main_final}"
    );
    let child_final = mock.messages_of_call(10).to_string();
    assert!(
        child_final.contains("completed: 2"),
        "the nested swarm result must report both members completed: {child_final}"
    );

    // The event stream: 3 spawned (the child + 2 members — the retry
    // reuses identities), 2 suspended (the members' parking, with
    // machine-readable rate_limit reasons per ADR-011), 3 completed (2
    // members + the child), and no failed events.
    let mut spawned = 0;
    let mut suspended = 0;
    let mut completed = 0;
    let mut failed = 0;
    let mut subagent_events: Vec<(String, Value)> = Vec::new();
    while let Ok(notification) = events.try_recv() {
        match notification.method.as_str() {
            "event.subagent.spawned" => spawned += 1,
            "event.subagent.suspended" => {
                suspended += 1;
                subagent_events.push((notification.method, notification.params.clone()));
            }
            "event.subagent.completed" => {
                completed += 1;
                subagent_events.push((notification.method, notification.params.clone()));
            }
            "event.subagent.failed" => failed += 1,
            "event.subagent.started" => {
                subagent_events.push((notification.method, notification.params.clone()));
            }
            _ => {}
        }
    }
    assert_eq!(
        spawned, 3,
        "the nested retry must reuse the original identities"
    );
    assert_eq!(suspended, 2, "each parked member announces once");
    assert_eq!(completed, 3, "both members and the child complete");
    assert_eq!(failed, 0, "suppressed rate-limit failures publish nothing");
    assert!(
        subagent_events
            .iter()
            .filter(|(method, _)| method == "event.subagent.suspended")
            .all(|(_, params)| params["reason"] == json!({ "kind": "rate_limit" })),
        "parked members' reasons are machine-readable rate_limit: {subagent_events:?}"
    );
    assert_suspension_pairing(&subagent_events);
}

// ===========================================================================
// ADR-011: structured suspension + approval-gate pairing
// ===========================================================================

/// A reverse-RPC handler that blocks forever without answering — the
/// approval ask's gate channel must carry the resolution instead (the
/// B4-a dual-channel convergence).
fn never_answering_handler() -> ReverseRpcHandler {
    Arc::new(|_method: String, _params: Value| {
        Box::pin(async {
            futures::future::pending::<()>().await;
            unreachable!("the pending future never resolves")
        })
    })
}

/// A reverse-RPC handler that FAILS every delivery — the transport shape of
/// a dropped connection / missing subscriber (the pending responder's
/// oneshot is dropped, so the engine's `approval.ask` callback resolves
/// `Err`). Per ADR-011 e the delivery failure must NOT resolve the ask:
/// the gate keeps the entry pending and the notification face failing is
/// only logged.
fn failing_reverse_rpc_handler() -> ReverseRpcHandler {
    Arc::new(|_method: String, _params: Value| {
        Box::pin(async { Err("reverse RPC connection closed".to_string()) })
    })
}

/// Drain every notification currently buffered on the subscriber channel
/// into `(method, params)` pairs.
fn drain_events(rx: &mut tokio::sync::mpsc::Receiver<JsonRpcNotification>) -> Vec<(String, Value)> {
    let mut out = Vec::new();
    while let Ok(notification) = rx.try_recv() {
        out.push((notification.method, notification.params));
    }
    out
}

/// The ADR-011 pairing invariant (§3.6 of the subagent architecture): every
/// `event.subagent.suspended` is closed by a LATER `event.subagent.started`
/// or terminal event (`completed` / `failed`) for the same member — no
/// suspension dangles at the end of the stream, per agent id.
fn assert_suspension_pairing(events: &[(String, Value)]) {
    let mut open: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for (method, params) in events {
        let member = params
            .get("subagentId")
            .and_then(Value::as_str)
            .unwrap_or("?")
            .to_string();
        match method.as_str() {
            "event.subagent.suspended" => {
                *open.entry(member).or_insert(0) += 1;
            }
            "event.subagent.started" | "event.subagent.completed" | "event.subagent.failed" => {
                if let Some(count) = open.get_mut(&member)
                    && *count > 0
                {
                    *count -= 1;
                }
                // A fully closed member leaves the map (only dangling
                // suspensions remain keys — the assertion below checks
                // emptiness).
                if open.get(&member) == Some(&0) {
                    open.remove(&member);
                }
            }
            _ => {}
        }
    }
    assert!(
        open.is_empty(),
        "unclosed suspensions remain: {open:?} (stream: {events:?})"
    );
}

/// Build the shared scripted flow for the approval-pairing tests: the main
/// agent dispatches a one-item swarm whose member asks to run Bash (Manual
/// mode → approval ask), then ends its turn, and the main agent closes on
/// the batch result. The member's approval is decided through the gate by
/// the test.
fn approval_swarm_script(mock: &ScriptedLlm) {
    // Two members (AgentSwarm requires ≥2 items). One of them asks to run
    // Bash — the Manual-mode approval ask; the other ends its turn
    // directly; the main agent closes on the batch result afterwards.
    mock.push_tool_use(vec![LlmToolCall {
        id: "call_0".to_string(),
        name: "AgentSwarm".to_string(),
        args: json!({
            "description": "approval swarm",
            "subagent_type": "coder",
            "prompt_template": "A: {{item}}",
            "items": ["a", "b"]
        }),
    }]);
    mock.push_tool_use(vec![LlmToolCall {
        id: "call_bash".to_string(),
        name: "Bash".to_string(),
        args: json!({ "command": "echo hello" }),
    }]);
    mock.push_text("member a result");
    mock.push_text("member b result");
    mock.push_text("all done");
}

/// Given a swarm member whose Manual-mode tool call reaches the approval
/// gate (its reverse-RPC ask blocks forever), When the batch runs, Then the
/// member announces `event.subagent.suspended` with the structured approval
/// reason (`kind == "approval"`, carrying the minted request_id and the
/// gated tool call id), and the decided approval closes the pairing with
/// `event.subagent.started` for the same subagent_id (ADR-011 gate hooks).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn should_publish_a_structured_approval_suspension_when_a_swarm_member_awaits_an_approval() {
    // Given a shared scripted LLM: main dispatch → member Bash ask → member
    // ends → main closes, and a server whose reverse-RPC channel never
    // answers (the ask resolves through the approval gate instead)
    let mock = Arc::new(ScriptedLlm::new());
    approval_swarm_script(&mock);
    let (server, _tmp) = b1_profiles_server_with_config(
        Arc::new(ScriptedLlmFactory::new(Arc::clone(&mock))),
        "",
        std::time::Duration::from_millis(50),
    );
    server
        .on_reverse_rpc(never_answering_handler())
        .await
        .expect("reverse rpc handler installs");
    let mut events = server.subscribe_events().await.unwrap();
    // A titled session — the test counts the mock's calls exactly, and an
    // untitled session's settle would spawn the side title call too.
    let created = server
        .call_method(
            "session.create",
            json!({ "permissionMode": "manual", "title": "approval suspension" }),
        )
        .await
        .expect("create");
    let session_id = created["id"].as_str().unwrap().to_string();

    // When the swarm runs and the member reaches the approval gate
    server
        .call_method(
            "message.send",
            json!({
                "session_id": session_id,
                "text": "run the swarm",
                "mode": "fire"
            }),
        )
        .await
        .expect("send");

    // Then the member announces its parking with the structured approval
    // reason before any decision arrives — the main agent's own Manual-mode
    // dispatch ask is decided the moment it appears so the batch can launch
    let mut seen: Vec<(String, Value)> = Vec::new();
    let mut member_request_id: Option<String> = None;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(15);
    let suspended: Value = loop {
        seen.extend(drain_events(&mut events));
        // The gate is the pending source of truth (ADR-011 e): while the
        // member waits, its Bash ask sits on the session gate.
        if let Ok(rows) = server
            .call_method("approval.listPending", json!({ "session_id": &session_id }))
            .await
            && let Some(rows) = rows.as_array()
            && let Some(row) = rows.first()
        {
            match row["toolName"].as_str() {
                // The main agent's own dispatch ask — decide it so the
                // batch launches.
                Some("AgentSwarm") => {
                    let _ = server
                        .call_method(
                            "approval.decide",
                            json!({
                                "session_id": &session_id,
                                "requestId": row["id"],
                                "approved": true
                            }),
                        )
                        .await;
                }
                // The member's gated tool call.
                Some("Bash") if member_request_id.is_none() => {
                    member_request_id = Some(
                        row["id"]
                            .as_str()
                            .expect("the member's ask carries a gate id")
                            .to_string(),
                    );
                }
                _ => {}
            }
        }
        if let Some((_, params)) = seen
            .iter()
            .find(|(method, _)| method == "event.subagent.suspended")
        {
            break params.clone();
        }
        if tokio::time::Instant::now() >= deadline {
            panic!(
                "the member never announced its approval suspension; chat calls so far: {}; \
                 member_request_id so far: {member_request_id:?} ({seen:?})",
                mock.chat_call_count(),
            );
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    };
    assert_eq!(
        suspended["reason"]["kind"],
        json!("approval"),
        "params: {suspended}"
    );
    let request_id = suspended["reason"]["request_id"]
        .as_str()
        .expect("the approval suspension carries the minted request id")
        .to_string();
    assert!(
        !request_id.is_empty(),
        "the request id is minted: {suspended}"
    );
    assert_eq!(
        member_request_id.as_deref(),
        Some(request_id.as_str()),
        "the gate's pending row and the suspension name the same request"
    );
    assert_eq!(
        suspended["reason"]["tool_call_id"],
        json!("call_bash"),
        "the approval suspension carries the gated tool call id: {suspended}"
    );
    let member_id = suspended["subagentId"]
        .as_str()
        .expect("the suspension names the member")
        .to_string();
    assert!(
        member_id.starts_with("agent-"),
        "a member identity: {member_id}"
    );

    // When the approval is decided through the gate channel
    let decided = server
        .call_method(
            "approval.decide",
            json!({
                "session_id": &session_id,
                "requestId": &request_id,
                "approved": true
            }),
        )
        .await
        .expect("the gate decision resolves");
    assert_eq!(decided, json!({ "ok": true }));

    // Then the pairing closes with started for the SAME member id and the
    // batch completes (5 chat calls: dispatch, the approval ask, the two
    // member endings, the main agent's close)
    await_chat_calls(&mock, 5).await;
    assert_eq!(mock.chat_call_count(), 5, "the full scripted flow ran");
    seen.extend(drain_events(&mut events));
    let started: Vec<String> = seen
        .iter()
        .filter(|(method, _)| method == "event.subagent.started")
        .map(|(_, params)| {
            params["subagentId"]
                .as_str()
                .unwrap_or_default()
                .to_string()
        })
        .collect();
    let completed: Vec<String> = seen
        .iter()
        .filter(|(method, _)| method == "event.subagent.completed")
        .map(|(_, params)| {
            params["subagentId"]
                .as_str()
                .unwrap_or_default()
                .to_string()
        })
        .collect();
    // F3: exactly ONE `started` fires AFTER the suspension — the decide-time
    // gate wake no longer publishes (the public `started` fires once, when
    // the member actually resumes running; the other started sits before the
    // suspension at launch).
    let suspended_pos = seen
        .iter()
        .position(|(method, params)| {
            method == "event.subagent.suspended"
                && params["subagentId"].as_str() == Some(member_id.as_str())
        })
        .expect("the member parked");
    assert_eq!(
        seen.iter()
            .skip(suspended_pos + 1)
            .filter(|(method, params)| {
                method == "event.subagent.started"
                    && params["subagentId"].as_str() == Some(member_id.as_str())
            })
            .count(),
        1,
        "the suspension episode closes with exactly one started: {seen:?}"
    );
    assert!(
        started.iter().any(|id| id == &member_id),
        "the approval resolution resumes the member: started={started:?}"
    );
    assert!(
        completed.iter().any(|id| id == &member_id),
        "the member completes: completed={completed:?}"
    );
    assert_suspension_pairing(&seen);
}

/// Given a swarm member whose approval ask is DENIED through the gate, When
/// the batch settles, Then every suspension is closed by a later started or
/// terminal event for the same agent id — the pairing invariant holds even
/// when the ask resolves into a denial (ADR-011 pairing closure).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn should_close_every_suspension_with_a_resume_or_terminal_event() {
    // Given the same approval-gated swarm flow (the gate — not the blocked
    // reverse-RPC — carries the decision)
    let mock = Arc::new(ScriptedLlm::new());
    approval_swarm_script(&mock);
    let (server, _tmp) = b1_profiles_server_with_config(
        Arc::new(ScriptedLlmFactory::new(Arc::clone(&mock))),
        "",
        std::time::Duration::from_millis(50),
    );
    server
        .on_reverse_rpc(never_answering_handler())
        .await
        .expect("reverse rpc handler installs");
    let mut events = server.subscribe_events().await.unwrap();
    // A titled session — the test counts the mock's calls exactly, and an
    // untitled session's settle would spawn the side title call too.
    let created = server
        .call_method(
            "session.create",
            json!({ "permissionMode": "manual", "title": "suspension pairing" }),
        )
        .await
        .expect("create");
    let session_id = created["id"].as_str().unwrap().to_string();
    server
        .call_method(
            "message.send",
            json!({
                "session_id": session_id,
                "text": "run the swarm",
                "mode": "fire"
            }),
        )
        .await
        .expect("send");

    // When the member parks — the main agent's dispatch ask decided through
    // the gate so the batch launches — and the member's approval is then
    // denied through the gate
    let mut seen: Vec<(String, Value)> = Vec::new();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(15);
    let request_id: String = loop {
        seen.extend(drain_events(&mut events));
        if let Ok(rows) = server
            .call_method("approval.listPending", json!({ "session_id": &session_id }))
            .await
            && let Some(rows) = rows.as_array()
            && let Some(row) = rows.first()
            && row["toolName"].as_str() == Some("AgentSwarm")
        {
            let _ = server
                .call_method(
                    "approval.decide",
                    json!({
                        "session_id": &session_id,
                        "requestId": row["id"],
                        "approved": true
                    }),
                )
                .await;
        }
        if let Some((_, params)) = seen
            .iter()
            .find(|(method, _)| method == "event.subagent.suspended")
        {
            break params["reason"]["request_id"]
                .as_str()
                .expect("the suspension carries the minted request id")
                .to_string();
        }
        if tokio::time::Instant::now() >= deadline {
            panic!(
                "the member never announced its approval suspension; chat calls so far: {} ({seen:?})",
                mock.chat_call_count()
            );
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    };
    let decided = server
        .call_method(
            "approval.decide",
            json!({
                "session_id": &session_id,
                "requestId": &request_id,
                "approved": false
            }),
        )
        .await
        .expect("the gate decision resolves");
    assert_eq!(decided, json!({ "ok": true }));

    // Then the batch settles and the pairing invariant holds across the
    // whole stream — every suspension has a later started or terminal event
    await_chat_calls(&mock, 5).await;
    assert_eq!(mock.chat_call_count(), 5, "the full scripted flow ran");
    seen.extend(drain_events(&mut events));
    // F4: the DENIED member still RESUMES its turn — the denial releases
    // the gate and the member's flow continues (the tool error path), so a
    // `started` AFTER the suspension closes the pairing via the resume
    // edge, not just via the terminal.
    let member_id = seen
        .iter()
        .find(|(method, _params)| method == "event.subagent.suspended")
        .and_then(|(_, params)| params["subagentId"].as_str().map(str::to_string))
        .expect("the suspension names the member");
    let suspended_pos = seen
        .iter()
        .position(|(method, params)| {
            method == "event.subagent.suspended"
                && params["subagentId"].as_str() == Some(member_id.as_str())
        })
        .expect("the member parked");
    assert!(
        seen.iter().skip(suspended_pos + 1).any(|(method, params)| {
            method == "event.subagent.started"
                && params["subagentId"].as_str() == Some(member_id.as_str())
        }),
        "the denied member still resumes its turn after the denial: {seen:?}"
    );
    assert_suspension_pairing(&seen);
}

/// Given a swarm member whose `approval.ask` reverse-RPC DELIVERY fails
/// (the dropped-connection transport shape — the callback resolves Err),
/// When the batch runs, Then the ask stays PENDING on the gate (the row
/// keeps serving from `approval.listPending`; the turn is NOT concluded as
/// "cancelled by user" — ADR-011 e: a notify-face failure never resolves
/// the ask), and deciding it through the gate afterwards releases the
/// member and closes the suspension pairing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn should_keep_the_approval_pending_when_its_reverse_rpc_delivery_fails() {
    // Given the approval-gated swarm flow on a channel whose delivery fails
    let mock = Arc::new(ScriptedLlm::new());
    approval_swarm_script(&mock);
    let (server, _tmp) = b1_profiles_server_with_config(
        Arc::new(ScriptedLlmFactory::new(Arc::clone(&mock))),
        "",
        std::time::Duration::from_millis(50),
    );
    server
        .on_reverse_rpc(failing_reverse_rpc_handler())
        .await
        .expect("reverse rpc handler installs");
    let mut events = server.subscribe_events().await.unwrap();
    // A titled session — the test counts the mock's calls exactly, and an
    // untitled session's settle would spawn the side title call too.
    let created = server
        .call_method(
            "session.create",
            json!({ "permissionMode": "manual", "title": "pending approval" }),
        )
        .await
        .expect("create");
    let session_id = created["id"].as_str().unwrap().to_string();
    server
        .call_method(
            "message.send",
            json!({
                "session_id": session_id,
                "text": "run the swarm",
                "mode": "fire"
            }),
        )
        .await
        .expect("send");

    // When the member parks — decide the main agent's own dispatch ask so
    // the batch launches, and watch for the member's suspension
    let mut seen: Vec<(String, Value)> = Vec::new();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(15);
    let request_id: String = loop {
        seen.extend(drain_events(&mut events));
        if let Ok(rows) = server
            .call_method("approval.listPending", json!({ "session_id": &session_id }))
            .await
            && let Some(rows) = rows.as_array()
            && let Some(row) = rows.iter().find(|row| row["toolName"] == "AgentSwarm")
        {
            let _ = server
                .call_method(
                    "approval.decide",
                    json!({
                        "session_id": &session_id,
                        "requestId": row["id"],
                        "approved": true
                    }),
                )
                .await;
        }
        if let Some((_, params)) = seen
            .iter()
            .find(|(method, _)| method == "event.subagent.suspended")
        {
            let id = params["reason"]["request_id"]
                .as_str()
                .expect("the suspension carries the minted request id")
                .to_string();
            // Then the gate STILL serves the parked ask — the delivery
            // failure must not have withdrawn it. Pre-T5 this withdraw +
            // "cancelled by user" happened as soon as the Err landed, so
            // this condition can never hold and the deadline panics (RED).
            if let Ok(rows) = server
                .call_method("approval.listPending", json!({ "session_id": &session_id }))
                .await
                && let Some(rows) = rows.as_array()
                && rows
                    .iter()
                    .any(|row| row["id"].as_str() == Some(id.as_str()))
            {
                break id;
            }
        }
        if tokio::time::Instant::now() >= deadline {
            panic!(
                "the member's approval must stay pending after the delivery failure; \
                 chat calls so far: {} ({seen:?})",
                mock.chat_call_count()
            );
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    };

    // When the approval is decided through the gate channel
    let decided = server
        .call_method(
            "approval.decide",
            json!({
                "session_id": &session_id,
                "requestId": &request_id,
                "approved": true
            }),
        )
        .await
        .expect("the gate decision resolves");
    assert_eq!(decided, json!({ "ok": true }));

    // Then the member resumes (started), the batch completes, and every
    // suspension pairing is closed — the tool call was released, not
    // vetoed by the channel failure
    await_chat_calls(&mock, 5).await;
    assert_eq!(mock.chat_call_count(), 5, "the full scripted flow ran");
    seen.extend(drain_events(&mut events));
    let member_id = seen
        .iter()
        .find(|(method, _)| method == "event.subagent.suspended")
        .and_then(|(_, params)| params["subagentId"].as_str().map(str::to_string))
        .expect("the suspension names the member");
    assert!(
        seen.iter()
            .any(|(method, params)| method == "event.subagent.started"
                && params["subagentId"].as_str() == Some(member_id.as_str())),
        "the decided approval resumes the member: {seen:?}"
    );
    assert!(
        seen.iter()
            .any(|(method, params)| method == "event.subagent.completed"
                && params["subagentId"].as_str() == Some(member_id.as_str())),
        "the member completes: {seen:?}"
    );
    assert_suspension_pairing(&seen);
}

/// Given a 1.5s per-attempt budget and a member whose approval ask parks
/// it at the gate for 3s (twice the whole budget), When the approval
/// eventually resolves, Then the member completes — the wait froze its
/// attempt budget (ADR-011 c: 等待用户停表; a running clock would fail the
/// member timed_out at 1.5s and its scripted continuation would never
/// run).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn should_freeze_the_attempt_budget_while_a_swarm_member_awaits_user_approval() {
    let mock = Arc::new(ScriptedLlm::new());
    approval_swarm_script(&mock);
    let (server, _tmp) = b1_profiles_server_with_config(
        Arc::new(ScriptedLlmFactory::new(Arc::clone(&mock))),
        "[subagent]\ntimeoutMs = 1500\n",
        std::time::Duration::from_millis(50),
    );
    server
        .on_reverse_rpc(never_answering_handler())
        .await
        .expect("reverse rpc handler installs");
    let mut events = server.subscribe_events().await.unwrap();
    // A titled session — the test counts the mock's calls exactly, and an
    // untitled session's settle would spawn the side title call too.
    let created = server
        .call_method(
            "session.create",
            json!({ "permissionMode": "manual", "title": "frozen budget" }),
        )
        .await
        .expect("create");
    let session_id = created["id"].as_str().unwrap().to_string();
    server
        .call_method(
            "message.send",
            json!({
                "session_id": &session_id,
                "text": "run the swarm",
                "mode": "fire"
            }),
        )
        .await
        .expect("send");

    // The member parks (the main agent's dispatch ask decided so the batch
    // launches; the member's own Bash ask is held).
    let mut seen: Vec<(String, Value)> = Vec::new();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(15);
    let (member_id, request_id): (String, String) = loop {
        seen.extend(drain_events(&mut events));
        if let Ok(rows) = server
            .call_method("approval.listPending", json!({ "session_id": &session_id }))
            .await
            && let Some(rows) = rows.as_array()
            && let Some(row) = rows.first()
            && row["toolName"].as_str() == Some("AgentSwarm")
        {
            let _ = server
                .call_method(
                    "approval.decide",
                    json!({
                        "session_id": &session_id,
                        "requestId": row["id"],
                        "approved": true
                    }),
                )
                .await;
        }
        if let Some((_, params)) = seen
            .iter()
            .find(|(method, _)| method == "event.subagent.suspended")
        {
            break (
                params["subagentId"]
                    .as_str()
                    .expect("the parking names the member")
                    .to_string(),
                params["reason"]["request_id"]
                    .as_str()
                    .expect("the parking carries the minted request id")
                    .to_string(),
            );
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the member never parked; seen: {seen:?}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    };

    // When the ask is held for 3s — twice the 1.5s per-attempt budget —
    // the member must not be charged for the wait: no failure may settle
    // it during the hold, and its scripted continuation must not have run.
    let hold_until = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
    while tokio::time::Instant::now() < hold_until {
        seen.extend(drain_events(&mut events));
        if let Some((_, params)) = seen
            .iter()
            .find(|(method, _)| method == "event.subagent.failed")
        {
            assert!(
                params["subagentId"].as_str() != Some(member_id.as_str()),
                "the parked member must not fail while it awaits the user: {params:?}"
            );
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(
        mock.chat_call_count() <= 3,
        "the member's continuation must not run before the decision (count {}): {:?}",
        mock.chat_call_count(),
        seen
    );

    // When the approval resolves through the gate
    let decided = server
        .call_method(
            "approval.decide",
            json!({
                "session_id": &session_id,
                "requestId": &request_id,
                "approved": true
            }),
        )
        .await
        .expect("the gate decision resolves");
    assert_eq!(decided, json!({ "ok": true }));

    // Then the member continues with its FROZEN remainder and completes —
    // the hold outlasted the whole budget, so only a stopped clock allows
    // it.
    await_chat_calls(&mock, 5).await;
    assert_eq!(mock.chat_call_count(), 5, "the full scripted flow ran");
    seen.extend(drain_events(&mut events));
    assert!(
        seen.iter().any(|(method, params)| {
            method == "event.subagent.completed"
                && params["subagentId"].as_str() == Some(member_id.as_str())
        }),
        "the member completes after the frozen wait: {seen:?}"
    );
    assert!(
        !seen.iter().any(|(method, params)| {
            method == "event.subagent.failed"
                && params["subagentId"].as_str() == Some(member_id.as_str())
        }),
        "the member never failed: {seen:?}"
    );
    assert_suspension_pairing(&seen);
}

/// Given a swarm member parked at the approval gate, When `message.abort`
/// cancels the batch while the ask is still pending, Then the member
/// settles with a terminal event — its suspension closes (resume-or-
/// terminal pairing) and it never completes (ADR-011 f: cancel settles
/// aborted). This exercises the cancel arm of the parked wait that Task 3
/// reasoned but never exercised.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn should_settle_a_member_aborted_when_the_batch_is_cancelled_while_an_approval_is_pending() {
    let mock = Arc::new(ScriptedLlm::new());
    approval_swarm_script(&mock);
    let (server, _tmp) = b1_profiles_server_with_config(
        Arc::new(ScriptedLlmFactory::new(Arc::clone(&mock))),
        "",
        std::time::Duration::from_millis(50),
    );
    server
        .on_reverse_rpc(never_answering_handler())
        .await
        .expect("reverse rpc handler installs");
    let mut events = server.subscribe_events().await.unwrap();
    let created = server
        .call_method("session.create", json!({ "permissionMode": "manual" }))
        .await
        .expect("create");
    let session_id = created["id"].as_str().unwrap().to_string();
    server
        .call_method(
            "message.send",
            json!({
                "session_id": &session_id,
                "text": "run the swarm",
                "mode": "fire"
            }),
        )
        .await
        .expect("send");

    // The member parks (the main agent's dispatch ask decided so the batch
    // launches; the member's Bash ask is held through the cancellation).
    let mut seen: Vec<(String, Value)> = Vec::new();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(15);
    let member_id: String = loop {
        seen.extend(drain_events(&mut events));
        if let Ok(rows) = server
            .call_method("approval.listPending", json!({ "session_id": &session_id }))
            .await
            && let Some(rows) = rows.as_array()
            && let Some(row) = rows.first()
            && row["toolName"].as_str() == Some("AgentSwarm")
        {
            let _ = server
                .call_method(
                    "approval.decide",
                    json!({
                        "session_id": &session_id,
                        "requestId": row["id"],
                        "approved": true
                    }),
                )
                .await;
        }
        if let Some((_, params)) = seen
            .iter()
            .find(|(method, _)| method == "event.subagent.suspended")
        {
            break params["subagentId"]
                .as_str()
                .expect("the parking names the member")
                .to_string();
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the member never parked; seen: {seen:?}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    };

    // When the batch is cancelled while the ask is still pending
    let aborted = server
        .call_method("message.abort", json!({ "session_id": &session_id }))
        .await
        .expect("abort");
    assert_eq!(aborted["ok"], true);

    // Then the member settles with a terminal event — the pairing closes
    // and the member never completes.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(15);
    let member_terminal: Value = loop {
        seen.extend(drain_events(&mut events));
        if let Some((_, params)) = seen.iter().rev().find(|(method, params)| {
            (method == "event.subagent.failed" || method == "event.subagent.completed")
                && params["subagentId"].as_str() == Some(member_id.as_str())
        }) {
            break params.clone();
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the member never settled; seen: {seen:?}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    };
    assert_eq!(
        member_terminal["subagentId"].as_str(),
        Some(member_id.as_str())
    );
    // The abort owns the settle: the member must NOT have been resumed
    // (no `started` AFTER its suspension — the launch-time `started` at
    // spawn is before it) and must not have completed.
    let suspended_pos = seen
        .iter()
        .position(|(method, params)| {
            method == "event.subagent.suspended"
                && params["subagentId"].as_str() == Some(member_id.as_str())
        })
        .expect("the member parked");
    assert!(
        !seen.iter().skip(suspended_pos + 1).any(|(method, params)| {
            method == "event.subagent.started"
                && params["subagentId"].as_str() == Some(member_id.as_str())
        }),
        "the cancelled member never resumes after its suspension: {seen:?}"
    );
    assert!(
        !seen.iter().any(|(method, params)| {
            method == "event.subagent.completed"
                && params["subagentId"].as_str() == Some(member_id.as_str())
        }),
        "the cancelled member never completes: {seen:?}"
    );
    assert_suspension_pairing(&seen);

    // F1: the aborted ask must NOT linger on the gate — a zombie entry
    // would keep serving the request to every `approval.listPending` /
    // reconnect (the web would rebuild an approval card for a member
    // already Aborted). The ask's turn is dropped without resolution, so
    // the drop-side backstop must withdraw it.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let pending = server
            .call_method("approval.listPending", json!({ "session_id": &session_id }))
            .await
            .expect("list pending");
        let rows = pending.as_array().expect("listPending returns an array");
        if rows.is_empty() {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the aborted member's approval must not linger on the gate: {pending:?}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}
