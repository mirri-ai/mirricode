//! G-30: log redaction, tool exchange fallback recovery, and RPC method
//! isolation tests.
//!
//! - **Part 1** exercises the public `mirri_core::redact` API: structured
//!   JSON redaction and raw-string scrubbing for secrets that must never
//!   reach logs or CLI output.
//! - **Part 2** exercises `ContextMemory` tool-exchange semantics: an open
//!   tool exchange defers appends, a failed exchange must close and flush,
//!   and when normal invocation is unavailable the engine falls back to
//!   recording unexecuted calls directly with synthetic error results.
//! - **Part 3** covers `session.create` ("create-rpc") / `plugin.*`
//!   ("plugins-rpc") method isolation. Those handlers live in the host
//!   layer (`mirri-embedded` / `mirri-server`), not in `mirri-core`, so
//!   those tests are `#[ignore]`d placeholders with TODOs (see below).

use mirri_core::loop_engine::tool_call_batch::record_unexecuted_tool_calls;
use mirri_core::redact::{redact_json, redact_string};
use mirri_core::tool::ToolOutput;
use mirri_core::{ContentPart, ContextMemory, LlmToolCall, LoopEvent, PromptOrigin, Role};
use serde_json::json;

/// Build a single text content part, the common helper for user messages.
fn user_text(text: &str) -> Vec<ContentPart> {
    vec![ContentPart::Text {
        text: text.to_string(),
    }]
}

/// Extract the tool call id from a `ToolCall` / `ToolResult` event.
fn event_tool_call_id(event: &LoopEvent) -> &str {
    match event {
        LoopEvent::ToolCall { tool_call_id, .. } | LoopEvent::ToolResult { tool_call_id, .. } => {
            tool_call_id.as_str()
        }
        _ => panic!("expected a tool call/result event, got: {event:?}"),
    }
}

// ---------------------------------------------------------------------------
// Part 1: log redaction
// ---------------------------------------------------------------------------

#[test]
fn should_redact_sensitive_data_in_log_output() {
    let input = json!({
        "provider": "openai",
        "model": "gpt-4o",
        "apiKey": "sk-proj-live-secret",
        "accessToken": "at_1234",
        "refreshToken": "rt_5678",
        "password": "hunter2",
        "clientSecret": "cs_9012",
    });

    let output = redact_json(&input);

    // Values under sensitive keys are replaced in place with the marker.
    assert_eq!(output["apiKey"], "[REDACTED]");
    assert_eq!(output["accessToken"], "[REDACTED]");
    assert_eq!(output["refreshToken"], "[REDACTED]");
    assert_eq!(output["password"], "[REDACTED]");
    assert_eq!(output["clientSecret"], "[REDACTED]");

    // Non-sensitive keys are untouched.
    assert_eq!(output["provider"], "openai");
    assert_eq!(output["model"], "gpt-4o");

    // The serialized log line must not leak any raw secret anywhere.
    let serialized = output.to_string();
    for secret in [
        "sk-proj-live-secret",
        "at_1234",
        "rt_5678",
        "hunter2",
        "cs_9012",
    ] {
        assert!(
            !serialized.contains(secret),
            "secret leaked into log output: {secret}"
        );
    }
}

#[test]
fn should_preserve_non_sensitive_data_in_log_output() {
    let input = json!({
        "provider": "anthropic",
        "model": "claude-sonnet-4",
        "requestId": "req-123",
        "maxTokens": 8192,
        "temperature": 0.7,
        "message": "the access token will rotate soon",
        "url": "https://api.example.com/v1/messages",
        "headers": { "content-type": "application/json" },
        "flags": [true, false],
        "nested": { "enabled": true, "count": 3 },
    });

    let output = redact_json(&input);

    // Exact-match key normalization: "maxTokens" must NOT be redacted even
    // though it contains the substring "token".
    assert_eq!(output["provider"], "anthropic");
    assert_eq!(output["model"], "claude-sonnet-4");
    assert_eq!(output["requestId"], "req-123");
    assert_eq!(output["maxTokens"], 8192);
    assert_eq!(output["temperature"], 0.7);
    assert_eq!(output["message"], "the access token will rotate soon");
    assert_eq!(output["url"], "https://api.example.com/v1/messages");
    assert_eq!(
        output["headers"],
        json!({ "content-type": "application/json" })
    );
    assert_eq!(output["flags"], json!([true, false]));
    assert_eq!(output["nested"], json!({ "enabled": true, "count": 3 }));
}

#[test]
fn should_redact_api_key_in_url_query_params() {
    // A URL with an `api_key` query parameter leaked into a raw log line.
    let url = "https://api.example.com/v1/models?api_key=sk-abcdefGHIJKLMNOP1234567&limit=5";
    let redacted = redact_string(url);
    assert!(
        redacted.contains("[REDACTED]"),
        "api_key value must be redacted"
    );
    assert!(
        !redacted.contains("sk-abcdefGHIJKLMNOP1234567"),
        "secret must not survive in the log line: {redacted}"
    );

    // Structured log output: secrets embedded in string values (including
    // URLs) are scrubbed too.
    let output = redact_json(&json!({
        "url": url,
        "level": "info",
    }));
    let url_out = output["url"].to_string();
    assert!(
        url_out.contains("[REDACTED]"),
        "url query param must be redacted: {url_out}"
    );
    assert!(!url_out.contains("sk-abcdefGHIJKLMNOP1234567"));
    assert_eq!(output["level"], "info");
}

#[test]
fn should_redact_authorization_header() {
    // Structured log output: an authorization header value is replaced in
    // place, while sibling headers survive.
    let output = redact_json(&json!({
        "headers": {
            "authorization": "Bearer eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.signature",
            "content-type": "application/json",
        },
        "requestId": "req-123",
    }));
    assert_eq!(output["headers"]["authorization"], "[REDACTED]");
    assert_eq!(output["headers"]["content-type"], "application/json");
    assert_eq!(output["requestId"], "req-123");

    // Free-form log lines: `authorization: bearer <token>` is scrubbed.
    let line = "POST /v1/messages authorization: bearer eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.sig status=200";
    let redacted = redact_string(line);
    assert!(
        redacted.contains("[REDACTED]"),
        "authorization header must be redacted: {redacted}"
    );
    assert!(
        !redacted.contains("eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.sig"),
        "bearer token must not survive in the log line: {redacted}"
    );
}

// ---------------------------------------------------------------------------
// Part 2: tool exchange fallback
// ---------------------------------------------------------------------------

#[test]
fn should_recover_when_tool_exchange_fails() {
    let mut mem = ContextMemory::new();

    // The assistant opens a tool exchange by emitting a tool call.
    mem.append_loop_event(LoopEvent::StepBegin {
        step_id: "step-1".to_string(),
        entry_seq: None,
    });
    mem.append_loop_event(LoopEvent::ToolCall {
        step_id: "step-1".to_string(),
        tool_call_id: "call-1".to_string(),
        tool_name: "Bash".to_string(),
        args: json!({ "command": "ls" }),
    });
    assert!(mem.has_open_tool_exchange());

    // While the exchange is open, user appends are deferred, not stored.
    mem.append_user_message(user_text("please continue"), PromptOrigin::User);
    assert_eq!(
        mem.history().len(),
        1,
        "only the assistant step is stored while the exchange is open"
    );

    // The tool fails: its result arrives flagged as an error.
    mem.append_loop_event(LoopEvent::ToolResult {
        tool_call_id: "call-1".to_string(),
        output: ToolOutput::text("command failed: exit code 1"),
        is_error: true,
        note: None,
        entry_seq: None,
    });

    // Recovery: the exchange is closed, the error result is recorded, and
    // the deferred user message is flushed into history.
    assert!(
        !mem.has_open_tool_exchange(),
        "a failed exchange must not stay open"
    );
    let history = mem.history();
    assert_eq!(
        history.len(),
        3,
        "assistant step + failing tool result + flushed user message"
    );
    assert_eq!(history[1].message.role, Role::Tool);
    assert!(
        history[1].is_error,
        "the tool failure must be recorded as an error"
    );
    assert_eq!(history[1].message.tool_call_id.as_deref(), Some("call-1"));
    assert_eq!(history[2].message.role, Role::User);
    assert_eq!(history[2].message.content, user_text("please continue"));

    // The memory is fully usable again: new appends go straight to history.
    mem.append_user_message(user_text("after recovery"), PromptOrigin::User);
    assert_eq!(mem.history().len(), 4);
    assert!(!mem.has_open_tool_exchange());

    // Fallback variant: the result never arrives (abandoned exchange). The
    // engine falls back to synthesizing an error result for the abandoned
    // call so the memory does not stay stuck buffering.
    mem.append_loop_event(LoopEvent::StepBegin {
        step_id: "step-2".to_string(),
        entry_seq: None,
    });
    mem.append_loop_event(LoopEvent::ToolCall {
        step_id: "step-2".to_string(),
        tool_call_id: "call-2".to_string(),
        tool_name: "Read".to_string(),
        args: json!({ "path": "src/main.rs" }),
    });
    mem.append_user_message(
        user_text("deferred while exchange abandoned"),
        PromptOrigin::User,
    );
    assert!(mem.has_open_tool_exchange());

    mem.close_pending_tool_results();

    assert!(
        !mem.has_open_tool_exchange(),
        "an interrupted exchange must not stay open"
    );
    let history = mem.history();
    assert_eq!(
        history.len(),
        7,
        "scenario 1 (4) + step-2 assistant + synthetic error + flushed user"
    );
    assert_eq!(history[5].message.role, Role::Tool);
    assert!(
        history[5].is_error,
        "the synthesized result must be marked as an error"
    );
    assert_eq!(history[5].message.tool_call_id.as_deref(), Some("call-2"));
    assert_eq!(history[6].message.role, Role::User);
    assert_eq!(
        history[6].message.content,
        user_text("deferred while exchange abandoned")
    );
}

#[test]
fn should_fall_back_to_direct_invocation_when_exchange_unavailable() {
    let mut mem = ContextMemory::new();
    mem.append_user_message(user_text("run the task"), PromptOrigin::User);
    mem.append_loop_event(LoopEvent::StepBegin {
        step_id: "step-1".to_string(),
        entry_seq: None,
    });

    // The model stopped before tool execution could start, so normal
    // invocation is unavailable. The engine falls back to recording each
    // unexecuted call directly, paired with a synthetic error result, in
    // provider order.
    let calls = vec![
        LlmToolCall {
            id: "call-1".to_string(),
            name: "Bash".to_string(),
            args: json!({ "command": "ls" }),
        },
        LlmToolCall {
            id: "call-2".to_string(),
            name: "Read".to_string(),
            args: json!({ "path": "x" }),
        },
    ];
    let recorded = record_unexecuted_tool_calls(&calls, "step-1", &mut mem);

    // Events are persisted in call order: call, error result, call, error
    // result — each call is opened and immediately closed with an error.
    assert_eq!(recorded.len(), 4, "two calls + two synthetic error results");
    assert!(matches!(&recorded[0], LoopEvent::ToolCall { .. }));
    assert!(matches!(
        &recorded[1],
        LoopEvent::ToolResult { is_error: true, .. }
    ));
    assert!(matches!(&recorded[2], LoopEvent::ToolCall { .. }));
    assert!(matches!(
        &recorded[3],
        LoopEvent::ToolResult { is_error: true, .. }
    ));
    assert_eq!(event_tool_call_id(&recorded[0]), "call-1");
    assert_eq!(event_tool_call_id(&recorded[1]), "call-1");
    assert_eq!(event_tool_call_id(&recorded[2]), "call-2");
    assert_eq!(event_tool_call_id(&recorded[3]), "call-2");

    // Each call was closed with its synthetic error result, so no exchange
    // is left open — the memory never gets stuck buffering.
    assert!(!mem.has_open_tool_exchange());

    // Both synthetic failures are recorded in history (with the synthetic
    // tool calls attached to the assistant step).
    let history = mem.history();
    assert_eq!(
        history.len(),
        4,
        "user + assistant (with 2 tool calls) + 2 error results"
    );
    assert_eq!(history[1].message.role, Role::Assistant);
    assert_eq!(history[1].message.tool_calls.len(), 2);
    assert_eq!(history[2].message.role, Role::Tool);
    assert!(history[2].is_error);
    assert_eq!(history[2].message.tool_call_id.as_deref(), Some("call-1"));
    assert_eq!(history[3].message.role, Role::Tool);
    assert!(history[3].is_error);
    assert_eq!(history[3].message.tool_call_id.as_deref(), Some("call-2"));

    // Because no exchange is open, subsequent appends are not deferred.
    mem.append_user_message(user_text("after fallback"), PromptOrigin::User);
    assert_eq!(mem.history().len(), 5);
    assert!(!mem.has_open_tool_exchange());
}

// ---------------------------------------------------------------------------
// Part 3: RPC method isolation
//
// The `session.create` ("create-rpc") and `plugin.*`/`plugins.*`
// ("plugins-rpc") methods are dispatched by the HOST layer, not by
// `mirri-core`:
//
// - `mirri-embedded`: `embedded_server/mod.rs` — `dispatch_create_session`
//   (`session.create` / `createSession`) and the `plugin.list` /
//   `plugin.install` / `plugin.reload` handlers.
// - `mirri-server`: `ipc.rs` / `ws_rpc.rs` — `session.create` JSON-RPC.
//
// `mirri-core` only knows the reverse direction
// (`reverse_rpc::ReverseRpcCallback`, engine → client), so there is no
// in-core dispatch table to test here. The isolation property these tests
// will assert once a host-level harness exists:
//
//   1. A `session.create` call resolves independently: its params/return
//      value never bleed into a plugins-rpc call's params/return, and vice
//      versa.
//   2. A failing `session.create` (invalid params, engine error) does not
//      poison subsequent `plugin.*` calls, and a failing `plugin.*` call
//      does not poison subsequent `session.create` calls — each method is
//      dispatched through its own handler with per-call state.
//
// TODO(G-30): move these tests into the crate that owns the dispatch
// (mirri-embedded / mirri-server integration tests) when the harness lands.
// ---------------------------------------------------------------------------

#[test]
#[ignore = "create-rpc dispatch lives in mirri-embedded/mirri-server, not mirri-core (G-30 TODO)"]
fn should_handle_create_rpc_independently() {
    // TODO(G-30, Part 3): exercise `session.create` through the host RPC
    // entry point found in `mirri-embedded/src/embedded_server/mod.rs`
    // (`dispatch_create_session`) and `mirri-server/src/ipc.rs`. Assert:
    //   1. `session.create` with valid params provisions a session and
    //      returns it without mutating any plugin state.
    //   2. An invalid `session.create` (missing/empty `title`) returns an
    //      error frame, and a follow-up valid create still succeeds.
    //   3. Interleaved `session.create` and `plugin.*` calls resolve with
    //      their own params/return values (no cross-talk).
    panic!("G-30 placeholder: create-rpc isolation test belongs in mirri-embedded/mirri-server");
}

#[test]
#[ignore = "plugins-rpc dispatch lives in mirri-embedded/mirri-server, not mirri-core (G-30 TODO)"]
fn should_handle_plugins_rpc_independently() {
    // TODO(G-30, Part 3): exercise `plugin.list` / `plugin.install` /
    // `plugin.reload` through the host RPC entry point found in
    // `mirri-embedded/src/embedded_server/mod.rs` (and any server-side
    // plugin dispatch in `mirri-server`). Assert:
    //   1. A failing plugins-rpc call (e.g. `plugin.install` with a missing
    //      `source`) returns an error frame but leaves `session.create`
    //      fully functional.
    //   2. Listing/installing plugins does not affect session creation
    //      state and vice versa (each handler owns its state).
    //   3. Concurrent or interleaved create + plugin calls never observe
    //      each other's params or return values.
    panic!("G-30 placeholder: plugins-rpc isolation test belongs in mirri-embedded/mirri-server");
}
