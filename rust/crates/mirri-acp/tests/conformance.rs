//! End-to-end conformance tests for the `mirri-acp` binary.
//!
//! Each test spawns the real binary as a child process and drives it over
//! stdin/stdout with newline-delimited JSON-RPC 2.0, asserting the wire
//! behavior described by the Agent Client Protocol (ACP).

use mirri_transport::adapter::EngineAdapter as _;
use serde_json::{Value, json};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::time::timeout;

/// Path to the `mirri-acp` binary under test (set by cargo for integration tests).
const ACP_BIN: &str = env!("CARGO_BIN_EXE_mirri-acp");

/// Deadline for a single request/response round trip. Generous on purpose:
/// every test spawns a full engine binary, and under `cargo test --workspace`
/// load (many test binaries back to back on a busy machine) a cold child can
/// legitimately take longer than a few seconds to answer its first calls —
/// a tight deadline here flakes the workspace gate while the same tests pass
/// in isolation. The happy path is unaffected; this only bounds the worst case.
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(30);

/// A live `mirri-acp` child process with piped stdio.
struct AcpProcess {
    /// Held for its lifetime so the process stays alive; dropped → killed.
    _child: Child,
    /// The child's OS pid — used to inspect its descendants when proving
    /// the conformance child never spawns the real home's MCP servers.
    child_id: u32,
    /// Held for its lifetime so the child's workspace directory exists.
    _workspace_dir: tempfile::TempDir,
    /// Held for its lifetime so the child's isolated Mirri home exists.
    _mirri_home: tempfile::TempDir,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl AcpProcess {
    async fn spawn() -> Self {
        Self::spawn_env(&[]).await
    }

    /// The shared spawn: the isolated workspace / home setup plus the
    /// extra environment pairs for the child (the provider-env injection
    /// point of the stdio smoke tests — the fully-wired engine reads
    /// `ANTHROPIC_API_KEY`/`ANTHROPIC_BASE_URL` from its env).
    async fn spawn_env(extra_env: &[(&str, &str)]) -> Self {
        Self::spawn_dirs(
            tempfile::tempdir().expect("tempdir must be creatable"),
            tempfile::tempdir().expect("tempdir must be creatable"),
            extra_env,
        )
        .await
    }

    /// [`Self::spawn_env`] with caller-owned, pre-seeded directories — the
    /// shared-home pinning test shapes the Mirri home's `workspaces.json` and
    /// `session-index.db` before the child boots (the engine reads both at
    /// construction and on its first RPC, so they must land before spawn).
    async fn spawn_dirs(
        workspace_dir: tempfile::TempDir,
        mirri_home: tempfile::TempDir,
        extra_env: &[(&str, &str)],
    ) -> Self {
        // The conformance home's config seed: the embedded session-create
        // chain requires a resolvable model (spec 11 R4 — no built-in
        // fallback), so every conformant `session/new` (which carries no
        // model parameter) needs a `default_model` configured. The
        // `conformance-default-model` alias entry makes the default a real
        // `[models]` alias — the M1 resolution face (spec 11 ADR-004: no
        // bare-name environment fallback), with the provider credentials
        // still injected through the env (ANTHROPIC_API_KEY /
        // ANTHROPIC_BASE_URL — the provider resolution layer the config
        // alias reads, orthogonal to model resolution). The `gpt-4o` alias
        // entry is what the set-model conformance tests switch to
        // (`agent.setModel` accepts configured aliases only). An explicit
        // test fixture seed, exactly what a real user's config.toml would
        // carry.
        std::fs::write(
            mirri_home.path().join("config.toml"),
            "default_model = \"conformance-default-model\"\n\n\
             [providers.anthropic]\n\
             type = \"anthropic\"\n\n\
             [models.\"conformance-default-model\"]\n\
             provider = \"anthropic\"\n\
             model = \"conformance-default-model\"\n\
             max_context_size = 128000\n\n\
             [models.\"gpt-4o\"]\n\
             provider = \"anthropic\"\n\
             model = \"gpt-4o\"\n\
             max_context_size = 128000\n",
        )
        .expect("the seeded config.toml must be writable");
        // `HOME` must be isolated too: the MCP global layer is discovered
        // from `$HOME/.mirri-code/mcp.json` (the `mirri_mcp` contract,
        // which `MIRRICODE_HOME` does not relocate), so a leaked real
        // HOME makes every engine-touching child spawn the developer's
        // real MCP servers in the background on its first engine call —
        // a machine-wide process storm that blows the 30s response
        // deadlines and leaves orphaned servers behind.
        let mut command = Command::new(ACP_BIN);
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .env("MIRRI_ACP_WORKSPACE_DIR", workspace_dir.path().as_os_str())
            // The fully-wired engine auto-discovers config.toml, plugins, and
            // agent profiles from `MIRRICODE_HOME`, while MCP servers come
            // from `$HOME/.mirri-code/mcp.json` — both env vars point at
            // throwaway directories so conformance runs never depend on (or
            // mutate) the developer's real ~/.mirri-code.
            .env("MIRRICODE_HOME", mirri_home.path().as_os_str())
            .env("HOME", mirri_home.path().as_os_str());
        for (key, value) in extra_env {
            command.env(key, value);
        }
        let mut child = command
            .kill_on_drop(true)
            .spawn()
            .expect("failed to spawn mirri-acp binary");
        let stdin = child.stdin.take().expect("child stdin is piped");
        let stdout = child.stdout.take().expect("child stdout is piped");
        let child_id = child.id().expect("spawned child must have a pid");
        Self {
            _child: child,
            child_id,
            _workspace_dir: workspace_dir,
            _mirri_home: mirri_home,
            stdin,
            stdout: BufReader::new(stdout),
        }
    }

    /// The child's workspace root (where the engine persists `.mirri/`
    /// session state) — assertions about on-disk removal read from here.
    fn workspace_root(&self) -> &std::path::Path {
        self._workspace_dir.path()
    }

    /// Write one newline-delimited line to the child's stdin.
    async fn send(&mut self, line: &str) {
        let mut payload = line.to_owned();
        payload.push('\n');
        self.stdin
            .write_all(payload.as_bytes())
            .await
            .expect("write to child stdin failed");
        self.stdin.flush().await.expect("flush child stdin failed");
    }

    /// Read the next response line from the child and parse it as JSON.
    async fn recv(&mut self) -> Value {
        let mut line = String::new();
        let bytes_read = timeout(RESPONSE_TIMEOUT, self.stdout.read_line(&mut line))
            .await
            .expect("timed out waiting for a response from mirri-acp")
            .expect("read from child stdout failed");
        assert!(
            bytes_read > 0,
            "child stdout closed before a response arrived"
        );
        serde_json::from_str(&line)
            .unwrap_or_else(|error| panic!("response is not valid JSON ({error}): {line:?}"))
    }

    /// Send one line and read exactly one response.
    async fn request(&mut self, line: &str) -> Value {
        self.send(line).await;
        self.recv().await
    }

    /// Send one request and read the next **response**, skipping any
    /// interleaved `session/update` notifications the serve loop relays
    /// while the method runs (e.g. the `current_mode_update` /
    /// `config_option_update` notifications the mode/config methods emit).
    async fn request_skipping_notifications(&mut self, line: &str) -> Value {
        self.send(line).await;
        loop {
            let frame = self.recv().await;
            if frame.get("method").is_some() && frame.get("id").is_none() {
                // A notification (no id) — not the response we are waiting
                // for; keep reading.
                continue;
            }
            return frame;
        }
    }

    /// Try to read a response, returning `None` if none arrives within
    /// `wait` — used to prove that no response is produced.
    async fn try_recv(&mut self, wait: Duration) -> Option<Value> {
        let mut line = String::new();
        match timeout(wait, self.stdout.read_line(&mut line)).await {
            Ok(Ok(0)) => panic!("child stdout closed unexpectedly"),
            Ok(Ok(_)) => {
                Some(serde_json::from_str(&line).unwrap_or_else(|error| {
                    panic!("response is not valid JSON ({error}): {line:?}")
                }))
            }
            Ok(Err(error)) => panic!("read from child stdout failed: {error}"),
            Err(_elapsed) => None,
        }
    }
}

// ===== A. Framing & transport =====

#[tokio::test]
async fn should_return_parse_error_when_given_invalid_json() {
    let mut acp = AcpProcess::spawn().await;
    let response = acp.request("not json").await;
    assert_eq!(
        response,
        json!({
            "jsonrpc": "2.0",
            "id": null,
            "error": {"code": -32700, "message": "parse error"}
        })
    );
}

#[tokio::test]
async fn should_return_method_not_found_when_method_is_unknown() {
    let mut acp = AcpProcess::spawn().await;
    let response = acp
        .request(r#"{"jsonrpc":"2.0","id":1,"method":"unknown/method","params":{}}"#)
        .await;
    assert_eq!(response["jsonrpc"], "2.0");
    assert_eq!(response["id"], 1);
    assert_eq!(response["error"]["code"], -32601);
    assert!(response["result"].is_null());
}

#[tokio::test]
async fn should_include_jsonrpc_2_0_field_in_every_response() {
    let mut acp = AcpProcess::spawn().await;
    let requests = [
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1}}"#,
        r#"{"jsonrpc":"2.0","id":2,"method":"unknown/method","params":{}}"#,
        r#"{"jsonrpc":"2.0","id":3,"method":"session/new","params":{}}"#,
    ];
    for request in requests {
        let response = acp.request(request).await;
        assert!(
            response.is_object(),
            "response must be an object: {response}"
        );
        assert_eq!(
            response["jsonrpc"], "2.0",
            "every frame must declare jsonrpc 2.0: {response}"
        );
    }
    // Parse-error responses must carry the version field too.
    let response = acp.request("not json").await;
    assert_eq!(response["jsonrpc"], "2.0");
}

#[tokio::test]
async fn should_skip_blank_lines_without_responding() {
    let mut acp = AcpProcess::spawn().await;
    let initialize =
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1}}"#;
    // Blank lines and the request arrive in a single write; only the
    // request may produce a response.
    acp.send(&format!("\n\n{initialize}")).await;
    let response = acp.recv().await;
    assert_eq!(response["id"], 1);
    assert!(
        response["error"].is_null(),
        "initialize must succeed: {response}"
    );
    assert!(
        acp.try_recv(Duration::from_millis(250)).await.is_none(),
        "blank lines must not produce responses"
    );
}

// ===== B. Initialize handshake =====

#[tokio::test]
async fn should_return_protocol_version_1_and_capabilities_when_initialized() {
    let mut acp = AcpProcess::spawn().await;
    let response = acp
        .request(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1}}"#)
        .await;
    assert!(
        response["error"].is_null(),
        "initialize must succeed: {response}"
    );

    let result = &response["result"];
    assert_eq!(result["protocolVersion"], 1);

    let capabilities = &result["agentCapabilities"];
    assert_eq!(capabilities["loadSession"], true);
    assert_eq!(
        capabilities["promptCapabilities"],
        json!({"image": true, "audio": false, "embeddedContext": true})
    );
    assert_eq!(
        capabilities["mcpCapabilities"],
        json!({"http": true, "sse": true})
    );
    assert_eq!(
        capabilities["sessionCapabilities"],
        json!({"list": {}, "resume": {}, "close": {}, "delete": {}}),
        "sessionCapabilities values must be empty objects, not booleans"
    );
    // The UNSTABLE fork capability is advertised off the stable surface,
    // under the reserved `_meta` extensibility channel (G19).
    assert_eq!(
        result["_meta"]["unstableSessionCapabilities"],
        json!({"fork": {}})
    );
}

#[tokio::test]
async fn should_clamp_to_protocol_version_1_when_client_sends_higher_version() {
    let mut acp = AcpProcess::spawn().await;
    let response = acp
        .request(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":99}}"#,
        )
        .await;
    assert!(
        response["error"].is_null(),
        "initialize must succeed: {response}"
    );
    assert_eq!(response["result"]["protocolVersion"], 1);
}

// ===== C. Lifecycle state machine =====

#[tokio::test]
async fn should_return_error_when_session_new_called_before_initialize() {
    let mut acp = AcpProcess::spawn().await;
    let response = acp
        .request(r#"{"jsonrpc":"2.0","id":1,"method":"session/new","params":{}}"#)
        .await;
    assert!(
        response["error"].is_object(),
        "expected an error response, got {response}"
    );
    assert_eq!(response["error"]["code"], -32000);
    assert!(response["result"].is_null());
}

#[tokio::test]
async fn should_return_error_when_session_prompt_has_unknown_session_id() {
    let mut acp = AcpProcess::spawn().await;
    let response = acp
        .request(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1}}"#)
        .await;
    assert!(
        response["error"].is_null(),
        "initialize must succeed: {response}"
    );

    let response = acp
        .request(
            r#"{"jsonrpc":"2.0","id":2,"method":"session/prompt","params":{"sessionId":"does-not-exist","prompt":"hi"}}"#,
        )
        .await;
    assert!(
        response["error"].is_object(),
        "expected an error response, got {response}"
    );
    assert_eq!(response["error"]["code"], -32002);
}

// ===== D. Error conformance =====

#[tokio::test]
async fn should_return_error_object_with_code_and_message_for_all_errors() {
    let mut acp = AcpProcess::spawn().await;
    let error_responses = [
        acp.request("not json").await,
        acp.request(r#"{"jsonrpc":"2.0","id":1,"method":"unknown/method","params":{}}"#)
            .await,
        acp.request(r#"{"jsonrpc":"2.0","id":2,"method":"session/new","params":{}}"#)
            .await,
    ];
    for response in error_responses {
        let error = response["error"]
            .as_object()
            .unwrap_or_else(|| panic!("expected an error object, got {response}"));
        assert!(
            error.get("code").and_then(Value::as_i64).is_some(),
            "error must carry a numeric code: {response}"
        );
        assert!(
            matches!(error.get("message").and_then(Value::as_str), Some(message) if !message.is_empty()),
            "error must carry a non-empty message: {response}"
        );
        assert_eq!(response["jsonrpc"], "2.0");
    }
}

#[tokio::test]
async fn should_echo_request_id_in_error_responses() {
    let mut acp = AcpProcess::spawn().await;
    let response = acp
        .request(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1}}"#)
        .await;
    assert!(
        response["error"].is_null(),
        "initialize must succeed: {response}"
    );

    // Distinct request ids; every error response must echo its id verbatim.
    let response_ten = acp
        .request(r#"{"jsonrpc":"2.0","id":10,"method":"unknown/method","params":{}}"#)
        .await;
    let response_twenty = acp
        .request(
            r#"{"jsonrpc":"2.0","id":20,"method":"session/prompt","params":{"sessionId":"nope","prompt":"hi"}}"#,
        )
        .await;
    for expected_id in [10i64, 20] {
        let response = if expected_id == 10 {
            &response_ten
        } else {
            &response_twenty
        };
        assert_eq!(
            response["id"], expected_id,
            "error response must echo the request id: {response}"
        );
        assert!(response["error"].is_object());
    }

    // An unparseable request carries no id, so the id must be null.
    let response = acp.request("not json").await;
    assert!(response["id"].is_null());
}

// ===== E. Extended session methods (Phase 4.5) =====

/// Initialize the child and create one session, returning its session id.
async fn initialize_and_create_session(acp: &mut AcpProcess) -> String {
    let response = acp
        .request(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1}}"#)
        .await;
    assert!(
        response["error"].is_null(),
        "initialize must succeed: {response}"
    );
    let response = acp
        .request(r#"{"jsonrpc":"2.0","id":2,"method":"session/new","params":{}}"#)
        .await;
    assert!(
        response["error"].is_null(),
        "session/new must succeed: {response}"
    );
    response["result"]["sessionId"]
        .as_str()
        .expect("session/new returns a sessionId")
        .to_string()
}

/// Converse once through `session/prompt` — SDR-004's materialization
/// trigger (W5): a never-conversed session is memory-only (no session
/// directory, no wire log), so the close/load/delete/fork flows below have
/// nothing to act on until the first user input lands. The turn itself
/// fails in these conformance homes (no provider credentials are seeded),
/// which still counts — "the conversation failed" is not empty — so either
/// response settles the trigger.
async fn converse_created_session(acp: &mut AcpProcess, session_id: &str) {
    let _ = acp
        .request_skipping_notifications(&format!(
            r#"{{"jsonrpc":"2.0","id":90,"method":"session/prompt","params":{{"sessionId":"{session_id}","prompt":"seed"}}}}"#
        ))
        .await;
}

#[tokio::test]
async fn should_close_created_session_when_session_close_called() {
    let mut acp = AcpProcess::spawn().await;
    let session_id = initialize_and_create_session(&mut acp).await;
    converse_created_session(&mut acp, &session_id).await;

    let response = acp
        .request(&format!(
            r#"{{"jsonrpc":"2.0","id":3,"method":"session/close","params":{{"sessionId":"{session_id}"}}}}"#
        ))
        .await;
    assert!(
        response["error"].is_null(),
        "session/close must succeed: {response}"
    );
    assert_eq!(response["result"], json!({}));

    // The closed session stays in the list (G9): the listing is the
    // engine's full view — persisted sessions included — never
    // self-filtered down to the running ones; the honest signal that it
    // is closed rides under `_meta.status`.
    let response = acp
        .request(r#"{"jsonrpc":"2.0","id":4,"method":"session/list","params":{}}"#)
        .await;
    assert!(response["error"].is_null(), "session/list must succeed");
    let sessions = response["result"]["sessions"]
        .as_array()
        .expect("session/list returns a sessions array");
    assert_eq!(sessions.len(), 1, "the closed session is still listed");
    assert_eq!(sessions[0]["sessionId"], json!(session_id));
    assert_eq!(sessions[0]["_meta"]["status"], json!("closed"));
}

#[tokio::test]
async fn should_delete_created_session_when_session_delete_called() {
    let mut acp = AcpProcess::spawn().await;
    let session_id = initialize_and_create_session(&mut acp).await;
    converse_created_session(&mut acp, &session_id).await;
    // The session persists under the per-workspace bucket (Phase 2: the TS
    // layout `<sessions>/<wd_…>/<id>/`, keyed off the workspace root) —
    // SDR-004 (W5): materialized by the first user input, not the create.
    let session_dir = acp
        .workspace_root()
        .join(".mirri")
        .join("v3_sessions")
        .join(mirri_core::encode_work_dir_key(acp.workspace_root()))
        .join(&session_id);
    assert!(
        session_dir.is_dir(),
        "the engine persists the session on creation: {}",
        session_dir.display()
    );

    // The deletion is real: the engine archives the session and the host
    // removes the persisted files from disk.
    let response = acp
        .request(&format!(
            r#"{{"jsonrpc":"2.0","id":3,"method":"session/delete","params":{{"sessionId":"{session_id}"}}}}"#
        ))
        .await;
    assert!(
        response["error"].is_null(),
        "session/delete must be acknowledged: {response}"
    );
    assert_eq!(response["result"], json!({}));
    assert!(
        !session_dir.exists(),
        "the persisted session files must be removed: {}",
        session_dir.display()
    );

    // The deleted session is gone from the list…
    let response = acp
        .request(r#"{"jsonrpc":"2.0","id":4,"method":"session/list","params":{}}"#)
        .await;
    let sessions = response["result"]["sessions"]
        .as_array()
        .expect("session/list returns a sessions array");
    assert!(
        sessions
            .iter()
            .all(|session| session["sessionId"] != json!(session_id)),
        "the deleted session must not be listed: {sessions:?}"
    );

    // …and loading it back fails: the session is really gone, never a
    // hollow acknowledgement.
    let response = acp
        .request(&format!(
            r#"{{"jsonrpc":"2.0","id":5,"method":"session/load","params":{{"sessionId":"{session_id}"}}}}"#
        ))
        .await;
    assert!(
        response["error"].is_object(),
        "loading a deleted session must fail: {response}"
    );
}

#[tokio::test]
async fn should_not_respond_when_session_cancel_sent_as_notification() {
    let mut acp = AcpProcess::spawn().await;
    let session_id = initialize_and_create_session(&mut acp).await;

    // `session/cancel` is a notification: it carries no id and must not
    // produce a response.
    acp.send(&format!(
        r#"{{"jsonrpc":"2.0","method":"session/cancel","params":{{"sessionId":"{session_id}"}}}}"#
    ))
    .await;
    assert!(
        acp.try_recv(Duration::from_millis(250)).await.is_none(),
        "session/cancel is a notification and must not produce a response"
    );

    // The connection stays healthy for subsequent requests.
    let response = acp
        .request(r#"{"jsonrpc":"2.0","id":4,"method":"session/list","params":{}}"#)
        .await;
    assert!(
        response["error"].is_null(),
        "the connection must remain usable after a notification: {response}"
    );
}

#[tokio::test]
async fn should_switch_modes_when_session_set_mode_called_with_mode_id() {
    let mut acp = AcpProcess::spawn().await;
    let session_id = initialize_and_create_session(&mut acp).await;

    let response = acp
        .request_skipping_notifications(&format!(
            r#"{{"jsonrpc":"2.0","id":3,"method":"session/set_mode","params":{{"sessionId":"{session_id}","modeId":"plan"}}}}"#
        ))
        .await;
    assert!(
        response["error"].is_null(),
        "entering plan mode must succeed: {response}"
    );
    assert_eq!(response["result"], json!({}));

    let response = acp
        .request_skipping_notifications(&format!(
            r#"{{"jsonrpc":"2.0","id":4,"method":"session/set_mode","params":{{"sessionId":"{session_id}","modeId":"default"}}}}"#
        ))
        .await;
    assert!(
        response["error"].is_null(),
        "returning to the default mode must succeed: {response}"
    );
    assert_eq!(response["result"], json!({}));

    // An unknown modeId is rejected as invalid params.
    let response = acp
        .request_skipping_notifications(&format!(
            r#"{{"jsonrpc":"2.0","id":5,"method":"session/set_mode","params":{{"sessionId":"{session_id}","modeId":"whisper"}}}}"#
        ))
        .await;
    assert_eq!(response["error"]["code"], -32602);

    // The pre-v1 `mode` parameter spelling is rejected, not shimmed.
    let response = acp
        .request_skipping_notifications(&format!(
            r#"{{"jsonrpc":"2.0","id":6,"method":"session/set_mode","params":{{"sessionId":"{session_id}","mode":"plan"}}}}"#
        ))
        .await;
    assert_eq!(response["error"]["code"], -32602);
}

#[tokio::test]
async fn should_set_model_option_when_session_set_config_option_called() {
    let mut acp = AcpProcess::spawn().await;
    let session_id = initialize_and_create_session(&mut acp).await;

    let response = acp
        .request_skipping_notifications(&format!(
            r#"{{"jsonrpc":"2.0","id":3,"method":"session/set_config_option","params":{{"sessionId":"{session_id}","configId":"model","value":"gpt-4o"}}}}"#
        ))
        .await;
    assert!(
        response["error"].is_null(),
        "setting the model option must succeed: {response}"
    );
    // The response carries the refreshed configOptions surface.
    assert!(
        response["result"]["configOptions"]
            .as_array()
            .is_some_and(
                |options| options.iter().any(|option| option["id"] == json!("model")
                    && option["currentValue"] == json!("gpt-4o"))
            ),
        "the refreshed configOptions must report the applied model: {response}"
    );

    // An unknown config option is rejected as invalid params.
    let response = acp
        .request_skipping_notifications(&format!(
            r#"{{"jsonrpc":"2.0","id":4,"method":"session/set_config_option","params":{{"sessionId":"{session_id}","configId":"temperature","value":0.7}}}}"#
        ))
        .await;
    assert_eq!(response["error"]["code"], -32602);

    // The pre-v1 `name` parameter spelling is rejected, not shimmed.
    let response = acp
        .request_skipping_notifications(&format!(
            r#"{{"jsonrpc":"2.0","id":5,"method":"session/set_config_option","params":{{"sessionId":"{session_id}","name":"model","value":"gpt-4o"}}}}"#
        ))
        .await;
    assert_eq!(response["error"]["code"], -32602);
}

#[tokio::test]
async fn should_set_model_when_session_set_model_called() {
    let mut acp = AcpProcess::spawn().await;
    let session_id = initialize_and_create_session(&mut acp).await;

    let response = acp
        .request(&format!(
            r#"{{"jsonrpc":"2.0","id":3,"method":"session/set_model","params":{{"sessionId":"{session_id}","model":"gpt-4o"}}}}"#
        ))
        .await;
    assert!(
        response["error"].is_null(),
        "session/set_model must succeed: {response}"
    );
    assert_eq!(response["result"], json!({}));
}

#[tokio::test]
async fn should_fork_created_session_when_session_fork_called() {
    let mut acp = AcpProcess::spawn().await;
    let session_id = initialize_and_create_session(&mut acp).await;

    let response = acp
        .request(&format!(
            r#"{{"jsonrpc":"2.0","id":3,"method":"session/fork","params":{{"sessionId":"{session_id}"}}}}"#
        ))
        .await;
    assert!(
        response["error"].is_null(),
        "session/fork must succeed: {response}"
    );
    let forked_id = response["result"]["sessionId"]
        .as_str()
        .expect("session/fork returns a sessionId");
    assert_ne!(
        forked_id, session_id,
        "the forked session must have its own id, got {forked_id}"
    );

    // Both sessions are live after the fork.
    let response = acp
        .request(r#"{"jsonrpc":"2.0","id":4,"method":"session/list","params":{}}"#)
        .await;
    let sessions = response["result"]["sessions"]
        .as_array()
        .expect("session/list returns a sessions array");
    assert_eq!(sessions.len(), 2, "fork must create a second live session");
}

#[tokio::test]
async fn should_create_sessions_without_a_credential_when_the_login_gate_is_removed() {
    // The managed-login gate no longer exists: sessions start on plain
    // per-provider credentials, so no seeded token is needed anywhere.
    let mut acp = AcpProcess::spawn().await;
    let response = acp
        .request(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1}}"#)
        .await;
    assert!(
        response["error"].is_null(),
        "initialize must succeed: {response}"
    );

    // session-creating methods proceed with no credential stored…
    let response = acp
        .request(r#"{"jsonrpc":"2.0","id":2,"method":"session/new","params":{}}"#)
        .await;
    assert!(
        response["error"].is_null(),
        "session/new must succeed without a credential: {response}"
    );

    // …and the removed ACP auth methods answer method-not-found.
    let response = acp
        .request(
            r#"{"jsonrpc":"2.0","id":3,"method":"authenticate","params":{"methodId":"login"}}"#,
        )
        .await;
    assert_eq!(response["error"]["code"], -32601);

    let response = acp
        .request(r#"{"jsonrpc":"2.0","id":4,"method":"logout","params":{}}"#)
        .await;
    assert_eq!(response["error"]["code"], -32601);
}

// ===== F. Hermeticity =====

/// The conformance child must be hermetic: the engine discovers MCP servers
/// from the real `$HOME` unless the test isolates it (the MCP global layer is
/// `$HOME/.mirri-code/mcp.json`, which `MIRRICODE_HOME` does not relocate).
/// If this guard fails, every engine-touching test silently spawns the
/// developer's real MCP servers in the background — a machine-wide process
/// storm that blows the 30s response deadlines and leaves orphaned server
/// processes behind after the children are killed.
///
/// This guards the test harness's own isolation, not the binary's behavior:
/// a real `mirri-acp` user *should* get MCP servers from their home.
#[cfg(unix)]
#[tokio::test]
async fn should_not_spawn_the_real_homes_mcp_servers_when_the_conformance_child_creates_a_session()
{
    let mut acp = AcpProcess::spawn().await;
    initialize_and_create_session(&mut acp).await;

    // `session/new` is the first engine call, which lazily spawns
    // `connect_all` for every discovered MCP server; the process spawns
    // land asynchronously after the response, so sample the child's
    // descendants over a window instead of checking once.
    for _ in 0..10 {
        let output = tokio::process::Command::new("pgrep")
            .arg("-P")
            .arg(acp.child_id.to_string())
            .output()
            .await
            .expect("pgrep must be installed to inspect the child's descendants");
        let descendants = String::from_utf8_lossy(&output.stdout);
        assert!(
            descendants.trim().is_empty(),
            "mirri-acp spawned descendant processes ({descendants}); the real \
             home's MCP servers leaked into the conformance child — check the \
             HOME isolation in AcpProcess::spawn"
        );
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
}

/// Given a shared Mirri home carrying a non-empty workspace registry and a
/// foreign workspace's session-index row, when the ACP child boots on its
/// negotiated root and lists sessions, then the negotiated-root semantics hold
/// (design 08 §2.2): the default listing never surfaces the foreign
/// workspace's session — the embedded host's pin, observed across a real
/// process boundary.
///
/// Mechanism note: the ACP host's engine is workspace-local by construction
/// (session persistence under the negotiated root's `.mirri/`, no home
/// injection — see `mirri-acp/src/main.rs`), so the home's registry rows are
/// never adopted at all; the NAPI host exercises the pin the stronger way
/// (home-injected, rows registered-but-lazy,
/// `should_pin_the_shared_home_to_the_negotiated_root_when_the_registry_is_non_empty`
/// in `mirri-node/src/engine_e2e_tests.rs`). This test is the regression
/// guard for the same acceptance: any future change that makes the ACP
/// engine adopt the shared home without the §2.2 pin turns it red (the
/// foreign session would appear in the default listing).
#[tokio::test]
async fn should_keep_the_negotiated_roots_listing_clean_when_the_shared_home_registry_is_non_empty()
{
    // Given a negotiated workspace directory and a shared home carrying a
    // foreign workspace's registry row and index row
    let workspace_dir = tempfile::tempdir().expect("workspace tempdir");
    let mirri_home = tempfile::tempdir().expect("home tempdir");
    let foreign_root = tempfile::tempdir().expect("foreign root tempdir");
    let foreign_key = mirri_core::encode_work_dir_key(foreign_root.path());
    std::fs::write(
        mirri_home.path().join("workspaces.json"),
        json!({
            "version": 1,
            "workspaces": {
                foreign_key.as_str(): {
                    "root": foreign_root.path().to_string_lossy(),
                    "name": "foreign",
                    "created_at": "2024-01-01T00:00:00.000Z",
                    "last_opened_at": "2024-06-01T00:00:00.000Z",
                }
            },
            "deleted_workspace_ids": [],
        })
        .to_string(),
    )
    .expect("the seeded registry is written");
    {
        let index =
            mirri_session_index::SessionIndex::open(&mirri_home.path().join("session-index.db"))
                .expect("the shared index opens");
        let now = chrono::Utc::now();
        index
            .upsert_session(&mirri_session_index::SessionRow {
                id: "sess-foreign-workspace".to_string(),
                workspace_id: foreign_key.clone(),
                cwd: Some(foreign_root.path().to_string_lossy().into_owned()),
                title: Some("the foreign workspace's session".to_string()),
                created_at: now,
                updated_at: now,
                archived: false,
                tags: Vec::new(),
                custom: None,
                last_prompt: Some("a real prompt".to_string()),
                state_mtime: None,
            })
            .expect("the foreign index row is seeded");
    }

    // When the child boots on the negotiated root and lists with default
    // params
    let mut acp = AcpProcess::spawn_dirs(workspace_dir, mirri_home, &[]).await;
    let response = acp
        .request(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1}}"#)
        .await;
    assert!(
        response["error"].is_null(),
        "initialize must succeed: {response}"
    );
    let response = acp
        .request(r#"{"jsonrpc":"2.0","id":2,"method":"session/list","params":{}}"#)
        .await;
    assert!(
        response["error"].is_null(),
        "session/list must succeed: {response}"
    );

    // Then the negotiated root's listing stays clean: the foreign
    // workspace's session never appears (the §2.2 pin — a shared home's
    // non-empty registry is not this host's to see by default).
    let sessions = response["result"]["sessions"]
        .as_array()
        .expect("session/list returns a sessions array");
    assert!(
        sessions
            .iter()
            .all(|session| session["sessionId"] != json!("sess-foreign-workspace")),
        "the foreign workspace's session leaked into the negotiated root's listing: {sessions:?}"
    );
    assert!(
        sessions.is_empty(),
        "a fresh negotiated root has no sessions of its own: {sessions:?}"
    );
}

// ===== G. Session update vocabulary (official ACP v1 schema) =====

/// Path to the vendored official ACP schema snapshot (see
/// `tests/fixtures/README.md` for provenance and the upgrade ritual).
const OFFICIAL_SCHEMA_PATH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/official-schema.json"
);

/// The official schema snapshot, parsed once and shared by the update
/// conformance tests (`$defs` only — the snapshot's own root is the
/// three-way Agent/Client/ProtocolLevel `anyOf`, which cannot validate a
/// bare `session/update` params object).
fn official_schema_defs() -> &'static serde_json::Map<String, Value> {
    static DEFS: std::sync::OnceLock<serde_json::Map<String, Value>> = std::sync::OnceLock::new();
    DEFS.get_or_init(|| {
        let raw = std::fs::read_to_string(OFFICIAL_SCHEMA_PATH)
            .expect("vendored official schema fixture must be readable");
        let schema: Value =
            serde_json::from_str(&raw).expect("vendored official schema must be valid JSON");
        schema["$defs"]
            .as_object()
            .expect("official schema must carry $defs")
            .clone()
    })
}

/// A compiled validator for a `session/update` notification's `params`,
/// built by re-rooting the snapshot's `$defs` under a
/// `{"$ref": "#/$defs/SessionNotification"}` wrapper: `SessionNotification`
/// is exactly the params shape (`{sessionId, update}`), and validating the
/// whole envelope transitively validates `update` against the official
/// `SessionUpdate` discriminated union.
fn session_notification_validator() -> &'static jsonschema::Validator {
    static VALIDATOR: std::sync::OnceLock<jsonschema::Validator> = std::sync::OnceLock::new();
    VALIDATOR.get_or_init(|| {
        let wrapper = json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "$ref": "#/$defs/SessionNotification",
            "$defs": official_schema_defs(),
        });
        jsonschema::Validator::new(&wrapper).expect("the official schema snapshot must compile")
    })
}

/// Assert that one `session/update` notification's `params` conforms to the
/// official `SessionNotification` schema (AJV-style single error report,
/// mirroring `mirri-core`'s `ToolArgsValidator`).
fn assert_conforms_to_official_schema(notification: &Value) {
    let params = &notification["params"];
    if let Err(error) = session_notification_validator().validate(params) {
        panic!(
            "session/update params do not conform to the official schema \
             ({error}): {params}"
        );
    }
}

/// Assert that the collected updates carry no trace of the pre-v1 draft
/// dialect (`agent_text_delta`, `turn_started`, `turn_ended`, the `state` /
/// `toolName` / `output` field names) — the reverse guard of the vocabulary
/// migration.
fn assert_no_legacy_dialect(updates: &[Value]) {
    for notification in updates {
        let update = &notification["params"]["update"];
        let literal = update["sessionUpdate"].as_str().unwrap_or_default();
        assert!(
            !matches!(literal, "agent_text_delta" | "turn_started" | "turn_ended"),
            "legacy dialect literal '{literal}' must not appear: {update}"
        );
        let legacy_fields = ["type", "state", "toolName", "output", "stopReason"];
        let present: Vec<&str> = legacy_fields
            .iter()
            .copied()
            .filter(|field| {
                update
                    .as_object()
                    .is_some_and(|object| object.contains_key(*field))
            })
            .collect();
        assert!(
            present.is_empty(),
            "legacy dialect fields {present:?} must not appear: {update}"
        );
    }
}

/// The scripted mock-LLM turn harness for update conformance.
///
/// The method-level tests above drive the real `mirri-acp` binary over
/// stdio; the update tests below instead drive an in-process
/// [`AcpHandler`](mirri_acp::AcpHandler) on an
/// [`EmbeddedServer`](mirri_embedded::EmbeddedServer) with a scripted
/// [`MockLlm`](mirri_test_harness::MockLlm) — the binary has no mock-LLM
/// hook (its engine resolves providers from real env credentials), so a
/// scripted turn is only possible in-process. The handler emits the exact
/// [`JsonRpcNotification`](mirri_transport::frame::JsonRpcNotification)
/// values the stdio transport relays verbatim, so schema-validating them
/// here is validating the wire content.
struct UpdateHarness {
    handler: mirri_acp::AcpHandler,
    engine: std::sync::Arc<dyn mirri_transport::adapter::EngineAdapter>,
    _workspace: tempfile::TempDir,
}

impl UpdateHarness {
    /// Build the harness around a fresh empty workspace and the given
    /// mock LLM.
    fn new(mock: std::sync::Arc<mirri_test_harness::MockLlm>) -> Self {
        let workspace = tempfile::tempdir().expect("tempdir must be creatable");
        let scope = std::sync::Arc::new(mirri_core::WorkspaceScope::new(
            mirri_core::WorkspaceId::new("acp-update-conformance"),
            workspace.path().to_path_buf(),
            mirri_core::WorkspaceScopeConfig::default(),
        ));
        let manager = std::sync::Arc::new(mirri_core::SessionManager::new(scope));
        let factory = std::sync::Arc::new(mirri_test_harness::MockLlmFactory::new(
            std::sync::Arc::clone(&mock),
        ));
        let engine: std::sync::Arc<dyn mirri_transport::adapter::EngineAdapter> =
            std::sync::Arc::new(seeded_engine(manager, Some(factory)));
        Self {
            handler: mirri_acp::AcpHandler::with_engine(engine.clone()),
            engine,
            _workspace: workspace,
        }
    }

    /// Initialize, create one session, and return its id.
    async fn create_session(&self) -> String {
        self.handler
            .handle("initialize", json!({"protocolVersion": 1}))
            .await
            .expect("initialize must succeed");
        let result = self
            .handler
            .handle("session/new", json!({"title": "Update conformance"}))
            .await
            .expect("session/new must succeed");
        result["sessionId"]
            .as_str()
            .expect("session/new returns a sessionId")
            .to_string()
    }

    /// Prompt the session (wait mode) and collect every `session/update`
    /// notification the turn produced, asserting the ACP envelope (method
    /// name and addressed session) on each.
    async fn prompt_and_collect_updates(&self, session_id: &str) -> Vec<Value> {
        let mut updates = self.handler.subscribe_updates();
        let result = self
            .handler
            .handle(
                "session/prompt",
                json!({"sessionId": session_id, "prompt": "run the turn"}),
            )
            .await
            .expect("session/prompt must succeed");
        assert!(
            result["stopReason"].as_str().is_some(),
            "the wait-mode turn must settle with a stopReason: {result}"
        );

        let mut collected = Vec::new();
        while let Ok(notification) = updates.try_recv() {
            assert_eq!(
                notification.method, "session/update",
                "engine events must be relayed as session/update notifications"
            );
            assert_eq!(
                notification.params["sessionId"].as_str(),
                Some(session_id),
                "every update must address the prompted session: {}",
                notification.params
            );
            collected.push(serde_json::to_value(&notification).expect("notification is JSON"));
        }
        assert!(
            !collected.is_empty(),
            "a completed turn must have streamed at least one session/update"
        );
        collected
    }
}

#[tokio::test]
async fn should_stream_official_v1_chunks_when_prompting_a_mock_text_turn() {
    // Given a scripted turn: think a little, then answer in text
    let mock = std::sync::Arc::new(mirri_test_harness::MockLlm::new());
    mock.push_response(mirri_test_harness::MockResponse::thinking_then_end_turn(
        "Let me think about the answer.",
        "Hello from the mock assistant",
    ));
    let harness = UpdateHarness::new(mock);
    let session_id = harness.create_session().await;

    // When the turn is prompted and its updates collected
    let updates = harness.prompt_and_collect_updates(&session_id).await;

    // Then every update conforms to the official schema and none carries the
    // legacy dialect
    for notification in &updates {
        assert_conforms_to_official_schema(notification);
    }
    assert_no_legacy_dialect(&updates);

    // And the turn streamed at least one agent_message_chunk whose content
    // is a text ContentBlock, and at least one agent_thought_chunk
    let mut message_text = String::new();
    let mut thought_text = String::new();
    for notification in &updates {
        let update = &notification["params"]["update"];
        match update["sessionUpdate"].as_str() {
            Some("agent_message_chunk") => {
                assert_eq!(
                    update["content"]["type"], "text",
                    "the chunk content must be a text ContentBlock: {update}"
                );
                message_text.push_str(update["content"]["text"].as_str().unwrap_or_default());
            }
            Some("agent_thought_chunk") => {
                assert_eq!(
                    update["content"]["type"], "text",
                    "the thought content must be a text ContentBlock: {update}"
                );
                thought_text.push_str(
                    update["content"]["text"]
                        .as_str()
                        .expect("agent_thought_chunk carries text"),
                );
            }
            _ => {}
        }
    }
    assert_eq!(
        message_text, "Hello from the mock assistant",
        "the message chunks must reassemble the scripted reply"
    );
    assert_eq!(
        thought_text, "Let me think about the answer.",
        "the thought chunks must reassemble the scripted thinking"
    );
}

#[tokio::test]
async fn should_stream_official_v1_tool_call_updates_when_prompting_a_mock_tool_turn() {
    // Given a scripted turn: request one Bash tool call, then answer in text
    let mock = std::sync::Arc::new(mirri_test_harness::MockLlm::new());
    mock.push_tool_use_response(vec![mirri_core::llm::LlmToolCall {
        id: "call_1".to_string(),
        name: "Bash".to_string(),
        args: json!({"command": "echo hello"}),
    }]);
    mock.push_text_response("Command ran fine");
    let harness = UpdateHarness::new(mock);
    let session_id = harness.create_session().await;

    // And the session's permission mode lifted to yolo so the Bash call
    // actually executes (the default manual mode would reject it — the
    // ACP approval bridge is a later batch, so the engine RPC is driven
    // directly here as test setup)
    harness
        .engine
        .call_method(
            "setPermission",
            json!({"sessionId": session_id, "mode": "yolo"}),
        )
        .await
        .expect("setPermission must succeed as test setup");

    // When the turn is prompted and its updates collected
    let updates = harness.prompt_and_collect_updates(&session_id).await;

    // Then every update conforms to the official schema and none carries the
    // legacy dialect
    for notification in &updates {
        assert_conforms_to_official_schema(notification);
    }
    assert_no_legacy_dialect(&updates);

    // And the turn emitted a tool_call CREATE (title = tool name, status
    // in_progress) followed by a tool_call_update marking it completed
    let mut create = None;
    let mut completed_ids = Vec::new();
    for notification in &updates {
        let update = &notification["params"]["update"];
        match update["sessionUpdate"].as_str() {
            Some("tool_call") => {
                let title = update["title"].as_str().expect("tool_call carries a title");
                assert!(!title.is_empty(), "tool_call title must be non-empty");
                assert_eq!(
                    title, "Bash",
                    "the CREATE's title is the tool name (the projection \
                     carries no description)"
                );
                assert_eq!(
                    update["status"], "in_progress",
                    "the CREATE announces execution: {update}"
                );
                assert_eq!(
                    update["toolCallId"], "call_1",
                    "the engine's tool call id flows through verbatim"
                );
                create = Some(update.clone());
            }
            Some("tool_call_update") if update["status"] == "completed" => {
                completed_ids.push(
                    update["toolCallId"]
                        .as_str()
                        .expect("tool_call_update carries the toolCallId")
                        .to_string(),
                );
            }
            _ => {}
        }
    }
    let create = create.expect("the turn must emit a tool_call CREATE");
    let create_id = create["toolCallId"].as_str().expect("CREATE carries an id");
    assert!(
        completed_ids.contains(&create_id.to_string()),
        "the CREATE for {create_id} must be completed by a tool_call_update"
    );
}

// ===== H. Permission bridge (approval.ask ⇄ session/request_permission) =====

/// A compiled validator for a `session/request_permission` request's
/// params, built by re-rooting the official snapshot's `$defs` under a
/// `{"$ref": "#/$defs/RequestPermissionRequest"}` wrapper — the same
/// re-rooting ritual the `session/update` validator uses.
fn request_permission_validator() -> &'static jsonschema::Validator {
    static VALIDATOR: std::sync::OnceLock<jsonschema::Validator> = std::sync::OnceLock::new();
    VALIDATOR.get_or_init(|| {
        let wrapper = json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "$ref": "#/$defs/RequestPermissionRequest",
            "$defs": official_schema_defs(),
        });
        jsonschema::Validator::new(&wrapper).expect("the official schema snapshot must compile")
    })
}

/// Assert that a bridged permission request's params conform to the
/// official `RequestPermissionRequest` schema.
fn assert_request_permission_conforms(pending: &mirri_acp::PendingClientRequest) {
    if let Err(error) = request_permission_validator().validate(&pending.params) {
        panic!(
            "session/request_permission params do not conform to the official \
             schema ({error}): {}",
            pending.params
        );
    }
}

/// An embedded engine for the in-process conformance harnesses, seeded
/// like the child-process runs (`AcpProcess::spawn_dirs`): the embedded
/// create chain requires a resolvable model (spec 11 R4 — no built-in
/// fallback), so every in-process engine needs a `default_model`
/// configured too.
fn seeded_engine(
    manager: std::sync::Arc<mirri_core::SessionManager>,
    factory: Option<std::sync::Arc<dyn mirri_embedded::LlmFactory>>,
) -> mirri_embedded::EmbeddedServer {
    seeded_engine_with_config(
        manager,
        factory,
        mirri_core::ServerConfig {
            default_model: Some("conformance-default-model".to_string()),
            ..mirri_core::ServerConfig::default()
        },
    )
}

/// The same in-process engine as [`seeded_engine`], but with a caller-owned
/// config — the declared-model tests seed a `models` map (e.g. a
/// `supportEfforts` declaration) without touching the shared seed.
fn seeded_engine_with_config(
    manager: std::sync::Arc<mirri_core::SessionManager>,
    factory: Option<std::sync::Arc<dyn mirri_embedded::LlmFactory>>,
    config: mirri_core::ServerConfig,
) -> mirri_embedded::EmbeddedServer {
    mirri_embedded::EmbeddedServer::with_providers(
        manager,
        factory,
        None,
        None,
        None,
        std::sync::Arc::new(std::sync::RwLock::new(config)),
        None,
    )
}

/// The permission-bridge test harness: an in-process
/// [`AcpHandler`](mirri_acp::AcpHandler) (as `Arc`, so the prompt turn can
/// run on its own task while the test plays the client) on an
/// [`EmbeddedServer`](mirri_embedded::EmbeddedServer) with a scripted
/// [`MockLlm`](mirri_test_harness::MockLlm).
struct BridgeHarness {
    handler: std::sync::Arc<mirri_acp::AcpHandler>,
    engine: std::sync::Arc<dyn mirri_transport::adapter::EngineAdapter>,
    _workspace: tempfile::TempDir,
}

impl BridgeHarness {
    fn new(mock: std::sync::Arc<mirri_test_harness::MockLlm>) -> Self {
        let workspace = tempfile::tempdir().expect("tempdir must be creatable");
        let scope = std::sync::Arc::new(mirri_core::WorkspaceScope::new(
            mirri_core::WorkspaceId::new("acp-permission-bridge"),
            workspace.path().to_path_buf(),
            mirri_core::WorkspaceScopeConfig::default(),
        ));
        let manager = std::sync::Arc::new(mirri_core::SessionManager::new(scope));
        let factory = std::sync::Arc::new(mirri_test_harness::MockLlmFactory::new(
            std::sync::Arc::clone(&mock),
        ));
        // The same config seed the child-process conformance runs carry
        // (`spawn_dirs`): the embedded create chain requires a resolvable
        // model (spec 11 R4 — no built-in fallback), so the in-process
        // bridge needs a `default_model` configured too.
        let engine: std::sync::Arc<dyn mirri_transport::adapter::EngineAdapter> =
            std::sync::Arc::new(seeded_engine(manager, Some(factory)));
        Self {
            handler: std::sync::Arc::new(mirri_acp::AcpHandler::with_engine(engine.clone())),
            engine,
            _workspace: workspace,
        }
    }

    /// Initialize, create one session (the default manual permission mode
    /// makes tool calls ask for approval), and return the session id.
    async fn create_session(&self) -> String {
        self.handler
            .handle("initialize", json!({"protocolVersion": 1}))
            .await
            .expect("initialize must succeed");
        let result = self
            .handler
            .handle("session/new", json!({"title": "Permission bridge"}))
            .await
            .expect("session/new must succeed");
        result["sessionId"]
            .as_str()
            .expect("session/new returns a sessionId")
            .to_string()
    }

    /// Run one `session/prompt` turn on its own task (so the test stays
    /// free to play the ACP client meanwhile), returning the task's join
    /// handle.
    fn spawn_prompt(
        &self,
        session_id: &str,
    ) -> tokio::task::JoinHandle<Result<Value, (i64, String)>> {
        let handler = std::sync::Arc::clone(&self.handler);
        let params = json!({"sessionId": session_id, "prompt": "run the tool"});
        tokio::spawn(async move { handler.handle("session/prompt", params).await })
    }

    /// [`Self::spawn_prompt`] under a client-facing JSON-RPC request id —
    /// the `$/cancel_request` target: the turn registers in the handler's
    /// in-flight prompt table under that id, so a `$/cancel_request`
    /// naming it cancels this exact turn.
    fn spawn_prompt_with_request_id(
        &self,
        session_id: &str,
        request_id: i64,
    ) -> tokio::task::JoinHandle<Result<Value, (i64, String)>> {
        let handler = std::sync::Arc::clone(&self.handler);
        let params = json!({"sessionId": session_id, "prompt": "run the tool"});
        tokio::spawn(async move {
            handler
                .handle_request(
                    Some(mirri_transport::RequestId::Number(request_id)),
                    "session/prompt",
                    params,
                )
                .await
        })
    }
}

/// Script the mock LLM for one turn that calls the Bash tool and then
/// answers in text (two LLM rounds: the tool-call round and the follow-up).
fn script_bash_turn(mock: &mirri_test_harness::MockLlm, tool_call_id: &str) {
    mock.push_tool_use_response(vec![mirri_core::llm::LlmToolCall {
        id: tool_call_id.to_string(),
        name: "Bash".to_string(),
        args: json!({"command": "echo bridged"}),
    }]);
    mock.push_text_response("the tool ran");
}

/// Whether the collected updates carry a terminal `tool_call_update` with
/// the given status for the tool call.
fn has_tool_status(updates: &[Value], tool_call_id: &str, status: &str) -> bool {
    updates.iter().any(|notification| {
        let update = &notification["params"]["update"];
        update["sessionUpdate"] == json!("tool_call_update")
            && update["toolCallId"] == json!(tool_call_id)
            && update["status"] == json!(status)
    })
}

/// Drain every buffered `session/update` notification from the receiver.
fn drain_updates(
    updates: &mut tokio::sync::broadcast::Receiver<mirri_transport::frame::JsonRpcNotification>,
) -> Vec<Value> {
    let mut collected = Vec::new();
    while let Ok(notification) = updates.try_recv() {
        collected.push(serde_json::to_value(&notification).expect("notification is JSON"));
    }
    collected
}

/// Given a manual-mode turn that calls a tool, when the client answers the
/// bridged `session/request_permission` request with approve-once, then the
/// request conforms to the official schema, addresses the prompted session
/// with the engine's tool call id, and the turn continues — the tool
/// executes and completes.
#[tokio::test]
async fn should_request_permission_and_continue_the_turn_when_the_client_allows() {
    let mock = std::sync::Arc::new(mirri_test_harness::MockLlm::new());
    script_bash_turn(&mock, "call_allow");
    let harness = BridgeHarness::new(mock);
    let session_id = harness.create_session().await;
    let mut updates = harness.handler.subscribe_updates();
    let mut client_requests = harness.handler.subscribe_client_requests();

    let prompt = harness.spawn_prompt(&session_id);
    let pending = timeout(Duration::from_secs(10), client_requests.recv())
        .await
        .expect("the client must receive a session/request_permission request")
        .expect("the client request channel stays open");

    // The request is a `session/request_permission` carrying the official
    // RequestPermissionRequest shape, addressed to the prompted session and
    // scoped to the engine's tool call id (the same id the tool_call
    // updates carry, so the client correlates prompt and card).
    assert_eq!(pending.method, "session/request_permission");
    assert_request_permission_conforms(&pending);
    assert_eq!(pending.params["sessionId"], json!(session_id));
    assert_eq!(
        pending.params["toolCall"]["toolCallId"],
        json!("call_allow")
    );
    assert_eq!(pending.params["toolCall"]["title"], json!("Bash"));
    assert!(
        pending.params["options"]
            .as_array()
            .is_some_and(|options| options.len() == 3),
        "the canonical 3 options must be offered: {}",
        pending.params
    );

    pending.respond(Ok(json!({
        "outcome": {"outcome": "selected", "optionId": "approve_once"},
    })));

    let result = prompt
        .await
        .expect("the prompt task must finish")
        .expect("the turn must settle successfully");
    assert_eq!(result["stopReason"], json!("end_turn"));
    let collected = drain_updates(&mut updates);
    assert!(
        has_tool_status(&collected, "call_allow", "completed"),
        "the allowed tool must have executed to completion: {collected:?}"
    );
    assert!(
        !has_tool_status(&collected, "call_allow", "failed"),
        "the allowed tool must not have failed: {collected:?}"
    );
}

/// Given a client that answers with approve-always, when the same tool
/// pattern runs again, then no second permission request is issued — the
/// approval's session scope installed an allow rule for the pattern.
#[tokio::test]
async fn should_not_ask_again_when_the_client_approved_for_the_session() {
    let mock = std::sync::Arc::new(mirri_test_harness::MockLlm::new());
    script_bash_turn(&mock, "call_always");
    script_bash_turn(&mock, "call_always_2");
    let harness = BridgeHarness::new(mock);
    let session_id = harness.create_session().await;
    let mut client_requests = harness.handler.subscribe_client_requests();

    // First turn: approve for this session.
    let prompt = harness.spawn_prompt(&session_id);
    let pending = timeout(Duration::from_secs(10), client_requests.recv())
        .await
        .expect("the first turn must ask for permission")
        .expect("the client request channel stays open");
    assert_eq!(pending.method, "session/request_permission");
    pending.respond(Ok(json!({
        "outcome": {"outcome": "selected", "optionId": "approve_always"},
    })));
    let result = prompt
        .await
        .expect("the prompt task must finish")
        .expect("the first turn must settle");
    assert_eq!(result["stopReason"], json!("end_turn"));

    // Second turn, same tool pattern: the session allow rule admits it
    // without another client round trip.
    let prompt = harness.spawn_prompt(&session_id);
    let second_ask = timeout(Duration::from_secs(2), client_requests.recv()).await;
    assert!(
        second_ask.is_err(),
        "no second session/request_permission may be issued for the \
         session-approved pattern (got {second_ask:?})"
    );
    let result = prompt
        .await
        .expect("the prompt task must finish")
        .expect("the second turn must settle");
    assert_eq!(result["stopReason"], json!("end_turn"));
}

/// Given a client that answers with reject, when the bridged request is
/// answered, then the tool is denied — a failed tool call update — and the
/// turn still settles (the model sees the denial and ends the turn).
#[tokio::test]
async fn should_deny_the_tool_when_the_client_selects_reject() {
    let mock = std::sync::Arc::new(mirri_test_harness::MockLlm::new());
    script_bash_turn(&mock, "call_reject");
    let harness = BridgeHarness::new(mock);
    let session_id = harness.create_session().await;
    let mut updates = harness.handler.subscribe_updates();
    let mut client_requests = harness.handler.subscribe_client_requests();

    let prompt = harness.spawn_prompt(&session_id);
    let pending = timeout(Duration::from_secs(10), client_requests.recv())
        .await
        .expect("the client must receive a session/request_permission request")
        .expect("the client request channel stays open");
    pending.respond(Ok(json!({
        "outcome": {"outcome": "selected", "optionId": "reject"},
    })));

    let result = prompt
        .await
        .expect("the prompt task must finish")
        .expect("the rejected turn must still settle");
    assert_eq!(result["stopReason"], json!("end_turn"));
    let collected = drain_updates(&mut updates);
    assert!(
        has_tool_status(&collected, "call_reject", "failed"),
        "the rejected tool must surface as a failed tool call: {collected:?}"
    );
    assert!(
        !has_tool_status(&collected, "call_reject", "completed"),
        "the rejected tool must never execute: {collected:?}"
    );
}

/// Given a client that never answers (the connection goes away mid-ask),
/// when the bridged request's reply channel drops, then the approval
/// resolves to its rejected fallback — the turn settles in seconds, never
/// hanging until the engine turn stall timeout.
#[tokio::test]
async fn should_fall_back_to_rejected_when_the_client_never_responds() {
    let mock = std::sync::Arc::new(mirri_test_harness::MockLlm::new());
    script_bash_turn(&mock, "call_silent");
    let harness = BridgeHarness::new(mock);
    let session_id = harness.create_session().await;
    let mut updates = harness.handler.subscribe_updates();
    let mut client_requests = harness.handler.subscribe_client_requests();

    let prompt = harness.spawn_prompt(&session_id);
    let pending = timeout(Duration::from_secs(10), client_requests.recv())
        .await
        .expect("the client must receive a session/request_permission request")
        .expect("the client request channel stays open");
    // The client disconnects without answering: dropping the pending
    // request drops the reply channel the bridge awaits.
    drop(pending);

    let result = timeout(Duration::from_secs(10), prompt)
        .await
        .expect("the fallback must settle the turn in seconds, not hang to the engine timeout")
        .expect("the prompt task must finish")
        .expect("the fallback turn must still settle");
    assert_eq!(result["stopReason"], json!("end_turn"));
    let collected = drain_updates(&mut updates);
    assert!(
        has_tool_status(&collected, "call_silent", "failed"),
        "the unanswered approval must deny the tool: {collected:?}"
    );
    assert!(
        !has_tool_status(&collected, "call_silent", "completed"),
        "the unanswered approval must never let the tool run: {collected:?}"
    );
}

/// Given a permission ask already parked on the client channel, when the
/// engine-side approval gate resolves the same ask first (the
/// `approval.decide` RPC — the dual-channel convergence), then the turn
/// proceeds off the gate's decision and the client's (late) answer is
/// harmlessly discarded.
#[tokio::test]
async fn should_let_the_gate_decide_first_when_approval_decide_wins_the_race() {
    let mock = std::sync::Arc::new(mirri_test_harness::MockLlm::new());
    script_bash_turn(&mock, "call_gate");
    let harness = BridgeHarness::new(mock);
    let session_id = harness.create_session().await;
    let mut updates = harness.handler.subscribe_updates();
    let mut client_requests = harness.handler.subscribe_client_requests();

    let prompt = harness.spawn_prompt(&session_id);
    let pending = timeout(Duration::from_secs(10), client_requests.recv())
        .await
        .expect("the client must receive a session/request_permission request")
        .expect("the client request channel stays open");

    // The pending approval is visible through the engine's approval gate —
    // poll until the ask registers (it parks there before the reverse RPC).
    let request_id = loop {
        let rows = harness
            .engine
            .call_method("approval.listPending", json!({"session_id": &session_id}))
            .await
            .expect("approval.listPending must succeed");
        if let Some(row) = rows.as_array().and_then(|rows| rows.first()) {
            break row["id"]
                .as_str()
                .expect("a pending row carries its id")
                .to_string();
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };

    // The gate wins the race: the decide resolves the ask while the
    // reverse-RPC request is still parked on the client channel.
    harness
        .engine
        .call_method(
            "approval.decide",
            json!({"session_id": &session_id, "request_id": request_id, "approved": true}),
        )
        .await
        .expect("approval.decide must succeed");

    let result = timeout(Duration::from_secs(10), prompt)
        .await
        .expect("the gate-resolved turn must settle")
        .expect("the prompt task must finish")
        .expect("the gate-resolved turn must settle successfully");
    assert_eq!(result["stopReason"], json!("end_turn"));
    let collected = drain_updates(&mut updates);
    assert!(
        has_tool_status(&collected, "call_gate", "completed"),
        "the gate-approved tool must have executed: {collected:?}"
    );

    // The client's late answer finds the abandoned ask gone: responding is
    // a harmless no-op, and no second ask is issued for this turn.
    pending.respond(Ok(json!({
        "outcome": {"outcome": "selected", "optionId": "reject"},
    })));
    let re_ask = timeout(Duration::from_secs(1), client_requests.recv()).await;
    assert!(
        re_ask.is_err(),
        "the resolved ask must not re-issue a permission request (got {re_ask:?})"
    );
}

// ===== K. $/cancel_request + cancellation during pending asks (G6 + F-P5b-4) =====

/// Assert that a `session/prompt` result conforms to the official
/// `PromptResponse` schema — the `stopReason` must be one of the five v1
/// literals, so a cancellation settling `cancelled` is provably
/// on-vocabulary (not the engine's private `aborted` spelling).
fn assert_prompt_response_conforms(result: &Value) {
    static VALIDATOR: std::sync::OnceLock<jsonschema::Validator> = std::sync::OnceLock::new();
    let validator = VALIDATOR.get_or_init(|| {
        let wrapper = json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "$ref": "#/$defs/PromptResponse",
            "$defs": official_schema_defs(),
        });
        jsonschema::Validator::new(&wrapper).expect("the official schema snapshot must compile")
    });
    if let Err(error) = validator.validate(result) {
        panic!(
            "session/prompt result does not conform to the official schema \
             ({error}): {result}"
        );
    }
}

/// Poll the mock until it has streamed at least `count` deltas — the
/// evidence the turn is demonstrably in flight — failing on the deadline.
async fn wait_for_streamed_deltas(mock: &mirri_test_harness::MockLlm, count: u64) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while mock.delta_yield_count() < count {
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for {count} streamed deltas \
             (got {}): the turn never started streaming",
            mock.delta_yield_count()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Collect `session/update` notifications (streaming from the broadcast)
/// until one carries the given terminal tool status for the tool call, or
/// fail on the deadline — the forwarder relays engine events
/// asynchronously, so a drain-after-settle may miss the last updates.
async fn await_tool_status(
    updates: &mut tokio::sync::broadcast::Receiver<mirri_transport::frame::JsonRpcNotification>,
    tool_call_id: &str,
    status: &str,
) {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        let next = timeout(Duration::from_millis(200), updates.recv()).await;
        match next {
            Ok(Ok(notification)) => {
                let update = &serde_json::to_value(&notification).expect("notification is JSON")["params"]
                    ["update"];
                if update["sessionUpdate"] == json!("tool_call_update")
                    && update["toolCallId"] == json!(tool_call_id)
                    && update["status"] == json!(status)
                {
                    return;
                }
            }
            Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => continue,
            Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) => break,
            Err(_elapsed) => continue,
        }
    }
    panic!(
        "timed out waiting for a {status:?} tool_call_update for \
         {tool_call_id:?}"
    );
}

/// Given an in-flight `session/prompt` turn whose assistant reply streams
/// slowly, when the client sends `$/cancel_request` naming that request's
/// JSON-RPC id, then the turn is aborted through the same cancel path as
/// the `session/cancel` notification: the prompt settles with the v1
/// `cancelled` stop reason (a schema-valid terminal state for the
/// client) and the provider stream observed the cancellation.
#[tokio::test]
async fn should_cancel_an_in_flight_prompt_when_dollar_cancel_request_names_its_request_id() {
    // Given a scripted turn: a reply streamed one delta at a time
    // (64 chars × 50 ms ≈ 3.2 s), so the prompt is still in flight when
    // the cancel lands.
    let mock = std::sync::Arc::new(mirri_test_harness::MockLlm::new());
    mock.push_response(mirri_test_harness::MockResponse::streaming(
        "c".repeat(64),
        Duration::from_millis(50),
    ));
    let harness = BridgeHarness::new(std::sync::Arc::clone(&mock));
    let session_id = harness.create_session().await;

    let prompt = harness.spawn_prompt_with_request_id(&session_id, 42);
    wait_for_streamed_deltas(&mock, 2).await;

    // When the client cancels the in-flight prompt by its request id —
    // `$/cancel_request` is the v1 standard request-cancellation channel.
    let cancel = harness
        .handler
        .handle("$/cancel_request", json!({"requestId": 42}))
        .await
        .expect("$/cancel_request must succeed (it is idempotent)");

    // Then the prompt settles promptly with the v1 cancelled stop reason —
    // the client's terminal state for the turn, on the official
    // vocabulary — and the provider stream saw the cancellation.
    let result = timeout(Duration::from_secs(5), prompt)
        .await
        .expect("the cancelled prompt must settle promptly, not run out the stream")
        .expect("the prompt task must finish")
        .expect("a cancelled turn settles as a result, never an error");
    assert_eq!(result["stopReason"], json!("cancelled"));
    assert_prompt_response_conforms(&result);
    assert_eq!(cancel, json!({}));
    assert!(
        mock.cancel_seen(),
        "the provider stream must observe the cancellation token"
    );
}

/// Given an in-flight prompt blocked on a `session/request_permission`
/// ask (manual mode: a tool call awaiting the client's decision), when
/// the client cancels the prompt with `$/cancel_request` instead of
/// answering, then the pending ask is settled engine-side with the v1
/// `Cancelled` outcome semantics — the turn aborts without waiting for
/// the client's (never-arriving) reply or the bridge's 60 s timeout, the
/// tool never executes, and a late client answer is discarded harmlessly
/// (F-P5b-4).
#[tokio::test]
async fn should_settle_a_pending_permission_ask_as_cancelled_when_the_prompt_is_cancelled() {
    // Given a scripted turn: one Bash tool call (blocked on the approval
    // ask), then a follow-up reply that the cancellation must preempt.
    let mock = std::sync::Arc::new(mirri_test_harness::MockLlm::new());
    script_bash_turn(&mock, "call_cancel");
    let harness = BridgeHarness::new(mock);
    let session_id = harness.create_session().await;
    let mut updates = harness.handler.subscribe_updates();
    let mut client_requests = harness.handler.subscribe_client_requests();

    let prompt = harness.spawn_prompt_with_request_id(&session_id, 7);
    let pending = timeout(Duration::from_secs(10), client_requests.recv())
        .await
        .expect("the client must receive a session/request_permission request")
        .expect("the client request channel stays open");
    assert_eq!(pending.method, "session/request_permission");
    assert_request_permission_conforms(&pending);

    // When the client cancels the prompt instead of answering the ask.
    harness
        .handler
        .handle("$/cancel_request", json!({"requestId": 7}))
        .await
        .expect("$/cancel_request must succeed");

    // Then the turn aborts without the client's answer: the prompt
    // settles with the v1 cancelled stop reason and the tool never
    // executes (it surfaces as a failed tool call).
    let result = timeout(Duration::from_secs(5), prompt)
        .await
        .expect("the cancelled prompt must settle without the client's answer")
        .expect("the prompt task must finish")
        .expect("a cancelled turn settles as a result, never an error");
    assert_eq!(result["stopReason"], json!("cancelled"));
    assert_prompt_response_conforms(&result);
    await_tool_status(&mut updates, "call_cancel", "failed").await;
    let collected = drain_updates(&mut updates);
    assert!(
        !has_tool_status(&collected, "call_cancel", "completed"),
        "the cancelled tool must never execute: {collected:?}"
    );

    // And the client's late answer to the already-settled ask is a
    // harmless no-op (the reply channel resolved as cancelled).
    pending.respond(Ok(json!({
        "outcome": {"outcome": "selected", "optionId": "approve_once"},
    })));
    let re_ask = timeout(Duration::from_secs(1), client_requests.recv()).await;
    assert!(
        re_ask.is_err(),
        "the settled ask must not re-issue a permission request (got {re_ask:?})"
    );
}

/// Given the real binary over stdio, when `$/cancel_request` arrives as a
/// notification (no id — its only well-formed form), then it is processed
/// without ever producing a response, and the host keeps serving requests
/// afterwards (an unknown requestId is an idempotent no-op).
#[tokio::test]
async fn should_not_respond_when_dollar_cancel_request_arrives_as_notification() {
    let mut child = AcpProcess::spawn().await;
    child
        .send(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1}}"#)
        .await;
    let _ = child.recv().await;

    // A $/cancel_request for a request this host never saw: a no-op, and
    // never a response (it is a notification).
    child
        .send(r#"{"jsonrpc":"2.0","method":"$/cancel_request","params":{"requestId":12345}}"#)
        .await;
    assert!(
        child.try_recv(Duration::from_millis(500)).await.is_none(),
        "$/cancel_request is a notification and must not produce a response"
    );

    // The host stays alive and functional after the no-op cancel.
    child
        .send(r#"{"jsonrpc":"2.0","id":2,"method":"session/new","params":{}}"#)
        .await;
    let response = child.recv().await;
    assert_eq!(response["id"], json!(2));
    assert!(
        response["result"]["sessionId"].as_str().is_some(),
        "the host must keep serving after $/cancel_request: {response}"
    );
}

// ===== I. session/new cwd routing + mcpServers overlay (G4+G3) =====

/// Path to the test-only MCP stdio echo server binary (see
/// `tests/bin/echo_mcp.rs`) — the `command` of overlay servers the tests
/// register through `session/new`'s `mcpServers`.
const ECHO_MCP_BIN: &str = env!("CARGO_BIN_EXE_mirri-acp-echo-mcp");

/// The overlay + cwd test harness: an in-process
/// [`AcpHandler`](mirri_acp::AcpHandler) with a **per-cwd engine pool** —
/// the factory records every engine it mints so tests can inspect the
/// engine a `session/new`'s `cwd` routed to (which session store holds the
/// session, which overlay connections are live). The default engine and
/// the pool engines all run the same scripted
/// [`MockLlm`](mirri_test_harness::MockLlm).
struct OverlayHarness {
    handler: std::sync::Arc<mirri_acp::AcpHandler>,
    /// The concrete default engine (cwd-less requests), for inspection.
    default_engine: std::sync::Arc<mirri_embedded::EmbeddedServer>,
    /// Every engine the per-cwd pool minted, oldest first.
    created_engines:
        std::sync::Arc<std::sync::Mutex<Vec<std::sync::Arc<mirri_embedded::EmbeddedServer>>>>,
    _workspace: tempfile::TempDir,
}

impl OverlayHarness {
    /// Build the harness without a workspace-pool MCP manager.
    async fn new(mock: std::sync::Arc<mirri_test_harness::MockLlm>) -> Self {
        Self::with_workspace_mcp(mock, None).await
    }

    /// Build the harness, optionally attaching a pre-connected workspace
    /// MCP manager to every pool engine (the shadowing tests give the
    /// workspace pool a same-named server to be shadowed by an overlay).
    async fn with_workspace_mcp(
        mock: std::sync::Arc<mirri_test_harness::MockLlm>,
        workspace_mcp: Option<std::sync::Arc<mirri_mcp::McpConnectionManager>>,
    ) -> Self {
        let workspace = tempfile::tempdir().expect("tempdir must be creatable");
        let default_engine = std::sync::Arc::new(Self::build_engine(
            workspace.path(),
            std::sync::Arc::clone(&mock),
            workspace_mcp.clone(),
        ));
        let created_engines: std::sync::Arc<
            std::sync::Mutex<Vec<std::sync::Arc<mirri_embedded::EmbeddedServer>>>,
        > = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));

        let factory: mirri_acp::EngineFactory = {
            let mock = std::sync::Arc::clone(&mock);
            let created_engines = std::sync::Arc::clone(&created_engines);
            std::sync::Arc::new(move |cwd: &std::path::Path| {
                let engine = std::sync::Arc::new(Self::build_engine(
                    cwd,
                    std::sync::Arc::clone(&mock),
                    workspace_mcp.clone(),
                ));
                created_engines
                    .lock()
                    .unwrap()
                    .push(std::sync::Arc::clone(&engine));
                engine
            })
        };
        let handler = std::sync::Arc::new(mirri_acp::AcpHandler::with_engine_pool(
            std::sync::Arc::clone(&default_engine)
                as std::sync::Arc<dyn mirri_transport::adapter::EngineAdapter>,
            factory,
        ));
        Self {
            handler,
            default_engine,
            created_engines,
            _workspace: workspace,
        }
    }

    /// One engine of the pool: a mock-LLM embedded server rooted at the
    /// given cwd, optionally carrying the workspace MCP pool.
    fn build_engine(
        cwd: &std::path::Path,
        mock: std::sync::Arc<mirri_test_harness::MockLlm>,
        workspace_mcp: Option<std::sync::Arc<mirri_mcp::McpConnectionManager>>,
    ) -> mirri_embedded::EmbeddedServer {
        let scope = std::sync::Arc::new(mirri_core::WorkspaceScope::new(
            mirri_core::WorkspaceId::new("acp-overlay-conformance"),
            cwd.to_path_buf(),
            mirri_core::WorkspaceScopeConfig::default(),
        ));
        let manager = std::sync::Arc::new(mirri_core::SessionManager::new(scope));
        let factory = std::sync::Arc::new(mirri_test_harness::MockLlmFactory::new(mock));
        let engine = seeded_engine(manager, Some(factory));
        match workspace_mcp {
            Some(manager) => engine.with_mcp_manager(manager),
            None => engine,
        }
    }

    /// The most recently minted pool engine.
    fn last_engine(&self) -> std::sync::Arc<mirri_embedded::EmbeddedServer> {
        self.created_engines
            .lock()
            .unwrap()
            .last()
            .expect("session/new with a cwd must have minted a pool engine")
            .clone()
    }

    /// Initialize the handler.
    async fn initialize(&self) {
        self.handler
            .handle("initialize", json!({"protocolVersion": 1}))
            .await
            .expect("initialize must succeed");
    }

    /// One v1 stdio `mcpServers` entry for the echo server with the given
    /// tool name (v1 stdio has no `type` field — it is identified by
    /// absence).
    fn echo_server(tool_name: &str) -> Value {
        json!({
            "name": "echo",
            "command": ECHO_MCP_BIN,
            "args": [tool_name],
            "env": [],
        })
    }

    /// Poll the engine's overlay for the given session until the named
    /// server reports `Connected` (the overlay connects asynchronously —
    /// `session/new` does not wait for it), failing the test on timeout.
    async fn wait_for_overlay(
        engine: &std::sync::Arc<mirri_embedded::EmbeddedServer>,
        session_id: &str,
        server_name: &str,
    ) {
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        loop {
            let connections = engine.session_overlay_connections(session_id).await;
            if connections.iter().any(|(name, state)| {
                name == server_name
                    && matches!(state, mirri_mcp::McpConnectionState::Connected { .. })
            }) {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "overlay server '{server_name}' never connected for session \
                 {session_id} (connections: {connections:?})"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

/// Given a `session/new` whose `cwd` does not exist, when it is sent, then
/// the request fails with invalid params and no session is created.
#[tokio::test]
async fn should_reject_session_new_when_cwd_does_not_exist() {
    let harness =
        OverlayHarness::new(std::sync::Arc::new(mirri_test_harness::MockLlm::new())).await;
    harness.initialize().await;

    let (code, message) = harness
        .handler
        .handle(
            "session/new",
            json!({"cwd": "/definitely/not/a/real/directory", "mcpServers": []}),
        )
        .await
        .expect_err("a nonexistent cwd must be rejected");
    assert_eq!(code, -32602, "message: {message}");
    assert!(
        message.contains("cwd"),
        "the error must point at the cwd: {message}"
    );

    // A relative cwd is rejected too (v1: the path must be absolute).
    let (code, _) = harness
        .handler
        .handle(
            "session/new",
            json!({"cwd": "relative/path", "mcpServers": []}),
        )
        .await
        .expect_err("a relative cwd must be rejected");
    assert_eq!(code, -32602);
}

/// Given a `session/new` with a cwd and a stdio `mcpServers` entry, when
/// the session is created, then the overlay server connects (asynchronously
/// — the response does not wait for it) and its tool is callable inside
/// that session.
#[tokio::test]
async fn should_connect_overlay_mcp_tools_into_the_session() {
    let mock = std::sync::Arc::new(mirri_test_harness::MockLlm::new());
    let harness = OverlayHarness::new(std::sync::Arc::clone(&mock)).await;
    harness.initialize().await;
    let cwd = tempfile::tempdir().expect("tempdir must be creatable");

    let result = harness
        .handler
        .handle(
            "session/new",
            json!({
                "cwd": cwd.path().to_str().expect("tempdir path is utf-8"),
                "mcpServers": [OverlayHarness::echo_server("echo")],
            }),
        )
        .await
        .expect("session/new must succeed");
    let session_id = result["sessionId"]
        .as_str()
        .expect("a sessionId")
        .to_string();
    let engine = harness.last_engine();

    // The overlay connects in the background.
    OverlayHarness::wait_for_overlay(&engine, &session_id, "echo").await;

    // The session's prompt turn can call the overlay tool.
    mock.push_tool_use_response(vec![mirri_core::llm::LlmToolCall {
        id: "call_overlay".to_string(),
        name: "mcp__echo__echo".to_string(),
        args: json!({"text": "hello overlay"}),
    }]);
    // Test setup: the in-turn side title call fires once the turn's first
    // LLM response lands (SDR-016) and consumes one scripted reply — feed
    // it after the turn's first response (b1_task_lifecycle_tests.rs
    // parity), before the turn's post-tool continuation.
    mock.push_text_response("title");
    mock.push_text_response("the echo tool ran");
    // Test setup: lift the session to yolo so the tool call needs no
    // approval round trip (the permission bridge is §H's subject).
    engine
        .call_method(
            "setPermission",
            json!({"sessionId": session_id, "mode": "yolo"}),
        )
        .await
        .expect("setPermission must succeed as test setup");

    let mut updates = harness.handler.subscribe_updates();
    let result = harness
        .handler
        .handle(
            "session/prompt",
            json!({"sessionId": session_id, "prompt": "call the echo tool"}),
        )
        .await
        .expect("session/prompt must succeed");
    assert_eq!(result["stopReason"], json!("end_turn"));
    let collected = drain_updates(&mut updates);
    assert!(
        has_tool_status(&collected, "call_overlay", "completed"),
        "the overlay tool must have executed to completion: {collected:?}"
    );
    assert!(
        !has_tool_status(&collected, "call_overlay", "failed"),
        "the overlay tool must not have failed: {collected:?}"
    );
}

/// Given a session with a connected overlay, when the session is closed,
/// then the overlay is unregistered — its connection rows leave the
/// session's overlay view (the connection manager is torn down with it).
#[tokio::test]
async fn should_unregister_the_overlay_when_the_session_closes() {
    let mock = std::sync::Arc::new(mirri_test_harness::MockLlm::new());
    // SDR-004 (W5): the seed turn below needs a scripted reply.
    mock.push_text_response("seed");
    let harness = OverlayHarness::new(mock).await;
    harness.initialize().await;
    let cwd = tempfile::tempdir().expect("tempdir must be creatable");

    let result = harness
        .handler
        .handle(
            "session/new",
            json!({
                "cwd": cwd.path().to_str().expect("tempdir path is utf-8"),
                "mcpServers": [OverlayHarness::echo_server("echo")],
            }),
        )
        .await
        .expect("session/new must succeed");
    let session_id = result["sessionId"]
        .as_str()
        .expect("a sessionId")
        .to_string();
    let engine = harness.last_engine();
    OverlayHarness::wait_for_overlay(&engine, &session_id, "echo").await;
    // SDR-004 (W5): converse once — the first user input materializes the
    // session (a never-conversed session is memory-only, nothing to load).
    harness
        .handler
        .handle(
            "session/prompt",
            json!({"sessionId": session_id, "prompt": "seed"}),
        )
        .await
        .expect("session/prompt must succeed");

    harness
        .handler
        .handle("session/close", json!({"sessionId": session_id}))
        .await
        .expect("session/close must succeed");

    let connections = engine.session_overlay_connections(&session_id).await;
    assert!(
        connections.is_empty(),
        "closing the session must unregister its overlay (got {connections:?})"
    );
}

/// Given a session with a connected overlay, when the session is deleted
/// (archived), then the overlay is unregistered like on close.
#[tokio::test]
async fn should_unregister_the_overlay_when_the_session_is_deleted() {
    let mock = std::sync::Arc::new(mirri_test_harness::MockLlm::new());
    // SDR-004 (W5): the seed turn below needs a scripted reply.
    mock.push_text_response("seed");
    let harness = OverlayHarness::new(mock).await;
    harness.initialize().await;
    let cwd = tempfile::tempdir().expect("tempdir must be creatable");

    let result = harness
        .handler
        .handle(
            "session/new",
            json!({
                "cwd": cwd.path().to_str().expect("tempdir path is utf-8"),
                "mcpServers": [OverlayHarness::echo_server("echo")],
            }),
        )
        .await
        .expect("session/new must succeed");
    let session_id = result["sessionId"]
        .as_str()
        .expect("a sessionId")
        .to_string();
    let engine = harness.last_engine();
    OverlayHarness::wait_for_overlay(&engine, &session_id, "echo").await;
    // SDR-004 (W5): converse once — the first user input materializes the
    // session (a never-conversed session is memory-only and has nothing
    // to delete on disk).
    harness
        .handler
        .handle(
            "session/prompt",
            json!({"sessionId": session_id, "prompt": "seed"}),
        )
        .await
        .expect("session/prompt must succeed");

    harness
        .handler
        .handle("session/delete", json!({"sessionId": session_id}))
        .await
        .expect("session/delete must be acknowledged");

    let connections = engine.session_overlay_connections(&session_id).await;
    assert!(
        connections.is_empty(),
        "deleting the session must unregister its overlay (got {connections:?})"
    );
}

/// Given a workspace-pool server named `shared`, when a session is created
/// with an overlay server of the same name, then the overlay's tools win:
/// the session sees the overlay's tool and not the workspace pool's.
#[tokio::test]
async fn shadow_the_workspace_pool_with_a_same_named_overlay() {
    // The workspace pool: an echo server named `shared` exposing
    // `workspace_tool`.
    let workspace_mcp = std::sync::Arc::new(mirri_mcp::McpConnectionManager::new());
    workspace_mcp
        .connect_one(
            "shared",
            mirri_mcp::McpServerConfig {
                transport: mirri_mcp::McpTransportConfig::Stdio {
                    command: ECHO_MCP_BIN.to_string(),
                    args: vec!["workspace_tool".to_string()],
                    env: None,
                    cwd: None,
                },
                enabled: None,
                startup_timeout_ms: None,
                tool_timeout_ms: None,
                enabled_tools: None,
                disabled_tools: None,
                auth: None,
            },
        )
        .await
        .expect("the workspace echo server must connect");

    let harness = OverlayHarness::with_workspace_mcp(
        std::sync::Arc::new(mirri_test_harness::MockLlm::new()),
        Some(workspace_mcp),
    )
    .await;
    harness.initialize().await;
    let cwd = tempfile::tempdir().expect("tempdir must be creatable");

    let result = harness
        .handler
        .handle(
            "session/new",
            json!({
                "cwd": cwd.path().to_str().expect("tempdir path is utf-8"),
                "mcpServers": [{
                    "name": "shared",
                    "command": ECHO_MCP_BIN,
                    "args": ["overlay_tool"],
                    "env": [],
                }],
            }),
        )
        .await
        .expect("session/new must succeed");
    let session_id = result["sessionId"]
        .as_str()
        .expect("a sessionId")
        .to_string();
    let engine = harness.last_engine();
    OverlayHarness::wait_for_overlay(&engine, &session_id, "shared").await;

    let tools = engine
        .call_method("tool.list", json!({"sessionId": session_id}))
        .await
        .expect("tool.list must succeed");
    let names: Vec<&str> = tools
        .as_array()
        .expect("tool.list returns an array")
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect();
    assert!(
        names.contains(&"mcp__shared__overlay_tool"),
        "the overlay's tool must be registered: {names:?}"
    );
    assert!(
        !names.contains(&"mcp__shared__workspace_tool"),
        "the same-named overlay must shadow the workspace pool's tools: {names:?}"
    );
}

/// Given two `session/new` requests with different cwds, when both
/// succeed, then each session lives on its own per-cwd engine — and a
/// repeated cwd reuses the same engine (the pool is keyed by cwd).
#[tokio::test]
async fn should_route_sessions_of_different_cwds_to_separate_engines() {
    let harness =
        OverlayHarness::new(std::sync::Arc::new(mirri_test_harness::MockLlm::new())).await;
    harness.initialize().await;
    let cwd_a = tempfile::tempdir().expect("tempdir must be creatable");
    let cwd_b = tempfile::tempdir().expect("tempdir must be creatable");

    let new_session = |cwd: &std::path::Path| {
        let handler = std::sync::Arc::clone(&harness.handler);
        let cwd = cwd.to_str().expect("tempdir path is utf-8").to_string();
        async move {
            let result = handler
                .handle("session/new", json!({"cwd": cwd, "mcpServers": []}))
                .await
                .expect("session/new must succeed");
            result["sessionId"]
                .as_str()
                .expect("a sessionId")
                .to_string()
        }
    };
    let session_a1 = new_session(cwd_a.path()).await;
    let session_b = new_session(cwd_b.path()).await;
    let session_a2 = new_session(cwd_a.path()).await;

    // A cwd-less request (pre-v1 callers) stays on the default engine.
    let session_default = harness
        .handler
        .handle("session/new", json!({"mcpServers": []}))
        .await
        .expect("cwd-less session/new must succeed on the default engine");
    let session_default = session_default["sessionId"]
        .as_str()
        .expect("a sessionId")
        .to_string();
    assert!(
        harness
            .default_engine
            .session_manager()
            .workspace()
            .list_sessions()
            .await
            .iter()
            .any(|id| id.as_str() == session_default),
        "a cwd-less session must live on the default engine"
    );

    let engines = harness.created_engines.lock().unwrap().clone();
    assert_eq!(
        engines.len(),
        2,
        "two distinct cwds must mint exactly two pool engines"
    );
    assert!(
        !std::sync::Arc::ptr_eq(&engines[0], &engines[1]),
        "different cwds must not share an engine"
    );

    // Each engine's session store holds exactly its own sessions.
    let engine_a = std::sync::Arc::clone(&engines[0]);
    let engine_b = std::sync::Arc::clone(&engines[1]);
    async fn live_ids(engine: &std::sync::Arc<mirri_embedded::EmbeddedServer>) -> Vec<String> {
        engine
            .session_manager()
            .workspace()
            .list_sessions()
            .await
            .into_iter()
            .map(|id| id.as_str().to_string())
            .collect()
    }
    let ids_a = live_ids(&engine_a).await;
    let ids_b = live_ids(&engine_b).await;
    assert!(
        ids_a.contains(&session_a1) && ids_a.contains(&session_a2) && !ids_a.contains(&session_b),
        "engine A must hold exactly cwd A's sessions (got {ids_a:?})"
    );
    assert!(
        ids_b.contains(&session_b) && !ids_b.contains(&session_a1),
        "engine B must hold exactly cwd B's sessions (got {ids_b:?})"
    );

    // Session-addressed ACP methods route to the owning engine: closing
    // session A1 through ACP removes it from engine A's live set only.
    harness
        .handler
        .handle("session/close", json!({"sessionId": session_a1}))
        .await
        .expect("session/close must succeed");
    let ids_a_after = live_ids(&engine_a).await;
    assert!(
        !ids_a_after.contains(&session_a1) && ids_a_after.contains(&session_a2),
        "closing session A1 must affect only engine A's set (got {ids_a_after:?})"
    );
    assert!(
        live_ids(&engine_b).await.contains(&session_b),
        "closing session A1 must not touch engine B's sessions"
    );
}

/// Given a `mcpServers` list mixing a valid stdio server with `acp`- and
/// unknown-transport entries, when the session is created, then the
/// unsupported entries are warn-dropped — the session is still created and
/// the supported server still connects.
#[tokio::test]
async fn should_warn_drop_unsupported_transports_and_still_create_the_session() {
    let harness =
        OverlayHarness::new(std::sync::Arc::new(mirri_test_harness::MockLlm::new())).await;
    harness.initialize().await;
    let cwd = tempfile::tempdir().expect("tempdir must be creatable");

    let result = harness
        .handler
        .handle(
            "session/new",
            json!({
                "cwd": cwd.path().to_str().expect("tempdir path is utf-8"),
                "mcpServers": [
                    {"type": "acp", "name": "acp-transport", "serverId": "component-1"},
                    {"type": "websocket", "name": "future-transport", "url": "ws://nowhere"},
                    OverlayHarness::echo_server("echo"),
                ],
            }),
        )
        .await
        .expect("session/new must succeed despite unsupported transports");
    let session_id = result["sessionId"]
        .as_str()
        .expect("a sessionId")
        .to_string();
    let engine = harness.last_engine();

    OverlayHarness::wait_for_overlay(&engine, &session_id, "echo").await;
    let connections = engine.session_overlay_connections(&session_id).await;
    let names: Vec<&str> = connections.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(
        names,
        vec!["echo"],
        "only the supported stdio server may be registered (got {names:?})"
    );
}

/// Given a closed session that had an overlay, when it is loaded again
/// with a fresh `mcpServers` list, then the overlay is re-registered on
/// the engine that owns the session — the client re-sends the servers on
/// every load (the v1 non-persistence contract).
#[tokio::test]
async fn should_re_register_the_overlay_when_a_closed_session_is_loaded() {
    let mock = std::sync::Arc::new(mirri_test_harness::MockLlm::new());
    // SDR-004 (W5): the seed turn below needs a scripted reply.
    mock.push_text_response("seed");
    let harness = OverlayHarness::new(mock).await;
    harness.initialize().await;
    let cwd = tempfile::tempdir().expect("tempdir must be creatable");
    let cwd_str = cwd
        .path()
        .to_str()
        .expect("tempdir path is utf-8")
        .to_string();

    let result = harness
        .handler
        .handle(
            "session/new",
            json!({
                "cwd": cwd_str,
                "mcpServers": [OverlayHarness::echo_server("echo")],
            }),
        )
        .await
        .expect("session/new must succeed");
    let session_id = result["sessionId"]
        .as_str()
        .expect("a sessionId")
        .to_string();
    let engine = harness.last_engine();
    OverlayHarness::wait_for_overlay(&engine, &session_id, "echo").await;
    // SDR-004 (W5): converse once — the first user input materializes the
    // session (a never-conversed session is memory-only, nothing to load).
    harness
        .handler
        .handle(
            "session/prompt",
            json!({"sessionId": session_id, "prompt": "seed"}),
        )
        .await
        .expect("session/prompt must succeed");

    harness
        .handler
        .handle("session/close", json!({"sessionId": session_id}))
        .await
        .expect("session/close must succeed");
    assert!(
        engine
            .session_overlay_connections(&session_id)
            .await
            .is_empty(),
        "closing must have torn the overlay down"
    );

    let result = harness
        .handler
        .handle(
            "session/load",
            json!({
                "sessionId": session_id,
                "cwd": cwd_str,
                "mcpServers": [OverlayHarness::echo_server("echo")],
            }),
        )
        .await
        .expect("session/load must succeed");
    assert_eq!(
        result["sessionId"],
        json!(session_id),
        "loading keeps the session id"
    );
    assert_eq!(
        harness.created_engines.lock().unwrap().len(),
        1,
        "the loaded session must route back to its own engine, not mint a new one"
    );
    OverlayHarness::wait_for_overlay(&engine, &session_id, "echo").await;
}

/// A recording [`EngineAdapter`] wrapper: every `call_method`'s method
/// name and params are appended to a shared log and the call delegates to
/// the wrapped engine — the F-P5d-1 wire-level assertion surface (the
/// empty-overlay semantics live in what the ACP layer sends, which this
/// engine's closed-session flow cannot distinguish behaviorally: close
/// tears the overlay down either way).
struct RecordingEngine {
    inner: std::sync::Arc<dyn mirri_transport::adapter::EngineAdapter>,
    calls: std::sync::Arc<std::sync::Mutex<Vec<(String, Value)>>>,
}

#[async_trait::async_trait]
impl mirri_transport::adapter::EngineAdapter for RecordingEngine {
    async fn call_method(
        &self,
        method: &str,
        params: Value,
    ) -> Result<Value, mirri_transport::adapter::EngineError> {
        self.calls
            .lock()
            .unwrap()
            .push((method.to_string(), params.clone()));
        self.inner.call_method(method, params).await
    }

    async fn subscribe_events(
        &self,
    ) -> Result<
        tokio::sync::mpsc::Receiver<mirri_transport::frame::JsonRpcNotification>,
        mirri_transport::adapter::EngineError,
    > {
        self.inner.subscribe_events().await
    }

    async fn on_reverse_rpc(
        &self,
        handler: mirri_transport::adapter::ReverseRpcHandler,
    ) -> Result<(), mirri_transport::adapter::EngineError> {
        self.inner.on_reverse_rpc(handler).await
    }

    async fn dispose(&self) -> Result<(), mirri_transport::adapter::EngineError> {
        self.inner.dispose().await
    }
}

/// Given a closed session, when it is loaded with an **empty**
/// `mcpServers` list, then the ACP layer forwards the empty list to the
/// engine (F-P5d-1's field-present semantics: an explicit list always
/// replaces — even an empty one installs an empty overlay; only an absent
/// field keeps whatever the session has, which the following load without
/// the field proves by sending no `mcpServers` key at all).
#[tokio::test]
async fn should_forward_an_explicit_empty_mcp_servers_list_when_a_session_is_loaded() {
    let workspace = tempfile::tempdir().expect("tempdir must be creatable");
    let scope = std::sync::Arc::new(mirri_core::WorkspaceScope::new(
        mirri_core::WorkspaceId::new("acp-empty-overlay-conformance"),
        workspace.path().to_path_buf(),
        mirri_core::WorkspaceScopeConfig::default(),
    ));
    let manager = std::sync::Arc::new(mirri_core::SessionManager::new(scope));
    let mock = std::sync::Arc::new(mirri_test_harness::MockLlm::new());
    // SDR-004 (W5): the seed turn below needs a scripted reply.
    mock.push_text_response("seed");
    let factory = std::sync::Arc::new(mirri_test_harness::MockLlmFactory::new(mock));
    let engine: std::sync::Arc<dyn mirri_transport::adapter::EngineAdapter> =
        std::sync::Arc::new(seeded_engine(manager, Some(factory)));
    let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let handler = mirri_acp::AcpHandler::with_engine(std::sync::Arc::new(RecordingEngine {
        inner: engine,
        calls: std::sync::Arc::clone(&calls),
    }));

    handler
        .handle("initialize", json!({"protocolVersion": 1}))
        .await
        .expect("initialize must succeed");
    let result = handler
        .handle("session/new", json!({"title": "Empty overlay"}))
        .await
        .expect("session/new must succeed");
    let session_id = result["sessionId"]
        .as_str()
        .expect("a sessionId")
        .to_string();
    // SDR-004 (W5): converse once — the first user input materializes the
    // session (a never-conversed session is memory-only, nothing to load).
    handler
        .handle(
            "session/prompt",
            json!({"sessionId": session_id, "prompt": "seed"}),
        )
        .await
        .expect("session/prompt must succeed");
    handler
        .handle("session/close", json!({"sessionId": session_id}))
        .await
        .expect("session/close must succeed");

    // The load with an explicit empty list…
    handler
        .handle(
            "session/load",
            json!({"sessionId": session_id, "mcpServers": []}),
        )
        .await
        .expect("session/load with an empty list must succeed");

    // …forwards the empty list to the engine: the resume params carry a
    // present, empty `mcpServers` — never dropped for being empty (an
    // empty overlay installs, clearing whatever the session had).
    let recorded = calls.lock().unwrap().clone();
    let resume_params: Vec<&Value> = recorded
        .iter()
        .filter(|(method, _)| method == "session.resume")
        .map(|(_, params)| params)
        .collect();
    assert_eq!(
        resume_params.len(),
        1,
        "exactly one session/load ran: {resume_params:?}"
    );
    assert_eq!(
        resume_params[0]["mcpServers"],
        json!([]),
        "an explicit empty mcpServers list must reach the engine: {}",
        resume_params[0]
    );

    // …and the load *without* the field sends no `mcpServers` key at all
    // (the absent field keeps the session's existing overlay).
    handler
        .handle("session/close", json!({"sessionId": session_id}))
        .await
        .expect("session/close must succeed");
    handler
        .handle("session/load", json!({"sessionId": session_id}))
        .await
        .expect("session/load without the field must succeed");
    let recorded = calls.lock().unwrap().clone();
    let resume_params: Vec<&Value> = recorded
        .iter()
        .filter(|(method, _)| method == "session.resume")
        .map(|(_, params)| params)
        .collect();
    assert_eq!(
        resume_params.len(),
        2,
        "two session/loads ran: {resume_params:?}"
    );
    assert!(
        resume_params[1].get("mcpServers").is_none(),
        "an absent mcpServers field must not be sent to the engine: {}",
        resume_params[1]
    );
}

// ===== J. session/load history replay + modes/configOptions (G5+G10) =====

/// The load-replay + modes test harness: an in-process
/// [`AcpHandler`](mirri_acp::AcpHandler) (as `Arc`, so a prompt turn can
/// run on its own task while the test plays the client) on an
/// [`EmbeddedServer`](mirri_embedded::EmbeddedServer) with a scripted
/// [`MockLlm`](mirri_test_harness::MockLlm) — the same shape as §H's
/// `BridgeHarness`, kept separate so each section reads standalone.
struct ReplayHarness {
    handler: std::sync::Arc<mirri_acp::AcpHandler>,
    engine: std::sync::Arc<dyn mirri_transport::adapter::EngineAdapter>,
    _workspace: tempfile::TempDir,
}

impl ReplayHarness {
    fn new(mock: std::sync::Arc<mirri_test_harness::MockLlm>) -> Self {
        Self::new_with_config(mock, mirri_core::ServerConfig {
            default_model: Some("conformance-default-model".to_string()),
            ..mirri_core::ServerConfig::default()
        })
    }

    /// The same harness with a caller-owned config — the declared-model
    /// tests seed the engine's `models` map (e.g. a `supportEfforts`
    /// declaration) without touching the shared seed.
    fn new_with_config(
        mock: std::sync::Arc<mirri_test_harness::MockLlm>,
        config: mirri_core::ServerConfig,
    ) -> Self {
        let workspace = tempfile::tempdir().expect("tempdir must be creatable");
        let scope = std::sync::Arc::new(mirri_core::WorkspaceScope::new(
            mirri_core::WorkspaceId::new("acp-load-replay-conformance"),
            workspace.path().to_path_buf(),
            mirri_core::WorkspaceScopeConfig::default(),
        ));
        let manager = std::sync::Arc::new(mirri_core::SessionManager::new(scope));
        let factory = std::sync::Arc::new(mirri_test_harness::MockLlmFactory::new(
            std::sync::Arc::clone(&mock),
        ));
        let engine: std::sync::Arc<dyn mirri_transport::adapter::EngineAdapter> =
            std::sync::Arc::new(seeded_engine_with_config(manager, Some(factory), config));
        Self {
            handler: std::sync::Arc::new(mirri_acp::AcpHandler::with_engine(engine.clone())),
            engine,
            _workspace: workspace,
        }
    }

    /// Initialize, create one session, and return its id.
    async fn create_session(&self) -> String {
        self.handler
            .handle("initialize", json!({"protocolVersion": 1}))
            .await
            .expect("initialize must succeed");
        let result = self
            .handler
            .handle("session/new", json!({"title": "Load replay"}))
            .await
            .expect("session/new must succeed");
        result["sessionId"]
            .as_str()
            .expect("session/new returns a sessionId")
            .to_string()
    }

    /// Run one `session/prompt` turn on its own task (so the test stays
    /// free to play the ACP client meanwhile).
    fn spawn_prompt(
        &self,
        session_id: &str,
        prompt: &str,
    ) -> tokio::task::JoinHandle<Result<Value, (i64, String)>> {
        let handler = std::sync::Arc::clone(&self.handler);
        let params = json!({"sessionId": session_id, "prompt": prompt});
        tokio::spawn(async move { handler.handle("session/prompt", params).await })
    }
}

/// Given a session with one completed tool turn, when it is closed and
/// loaded again, then the client receives the session's full history as
/// an ordered `session/update` batch (user message, agent message, tool
/// call create + terminal update — every update conforming to the
/// official schema) and the load response carries the `modes` /
/// `configOptions` surface.
#[tokio::test]
async fn should_replay_full_history_chunks_when_a_closed_session_is_loaded() {
    // Given a scripted turn: one Bash tool call, then a text answer
    let mock = std::sync::Arc::new(mirri_test_harness::MockLlm::new());
    mock.push_tool_use_response(vec![mirri_core::llm::LlmToolCall {
        id: "call_replay".to_string(),
        name: "Bash".to_string(),
        args: json!({"command": "echo hello"}),
    }]);
    mock.push_text_response("the tool ran");
    let harness = ReplayHarness::new(mock);
    let session_id = harness.create_session().await;
    // Test setup: yolo lifts the permission gate so the Bash call needs no
    // approval round trip (the bridge-across-resume test below is §J's
    // approval subject).
    harness
        .engine
        .call_method(
            "setPermission",
            json!({"sessionId": session_id, "mode": "yolo"}),
        )
        .await
        .expect("setPermission must succeed as test setup");
    let result = harness
        .handler
        .handle(
            "session/prompt",
            json!({"sessionId": session_id, "prompt": "call the tool"}),
        )
        .await
        .expect("session/prompt must succeed");
    assert_eq!(result["stopReason"], json!("end_turn"));

    // The session is closed and loaded again.
    harness
        .handler
        .handle("session/close", json!({"sessionId": session_id}))
        .await
        .expect("session/close must succeed");
    let mut updates = harness.handler.subscribe_updates();
    let response = harness
        .handler
        .handle("session/load", json!({"sessionId": session_id}))
        .await
        .expect("session/load must succeed");

    // Then the client received the full history as session/update
    // notifications — every one conforming to the official schema.
    let collected = drain_updates(&mut updates);
    assert!(
        !collected.is_empty(),
        "loading must replay at least one session/update"
    );
    for notification in &collected {
        assert_eq!(
            notification["params"]["sessionId"],
            json!(session_id),
            "every replayed update must address the loaded session"
        );
        assert_conforms_to_official_schema(notification);
    }
    assert_no_legacy_dialect(&collected);

    // And the replayed sequence carries the persisted wire records: the
    // user prompt, the agent's tool call and final message, and the tool
    // call's create + terminal update pair.
    let mut user_text = String::new();
    let mut agent_text = String::new();
    let mut saw_tool_call_create = false;
    let mut saw_tool_call_completed = false;
    for notification in &collected {
        let update = &notification["params"]["update"];
        match update["sessionUpdate"].as_str() {
            Some("user_message_chunk") => {
                user_text.push_str(update["content"]["text"].as_str().unwrap_or_default());
            }
            Some("agent_message_chunk") => {
                agent_text.push_str(update["content"]["text"].as_str().unwrap_or_default());
            }
            Some("tool_call") => {
                assert_eq!(update["toolCallId"], json!("call_replay"));
                assert_eq!(update["status"], json!("in_progress"));
                saw_tool_call_create = true;
            }
            Some("tool_call_update") => {
                assert_eq!(update["toolCallId"], json!("call_replay"));
                if update["status"] == json!("completed") {
                    saw_tool_call_completed = true;
                }
            }
            _ => {}
        }
    }
    assert_eq!(
        user_text, "call the tool",
        "the replay must re-render the persisted user prompt"
    );
    assert_eq!(
        agent_text, "the tool ran",
        "the replay must re-render the persisted assistant message"
    );
    assert!(
        saw_tool_call_create && saw_tool_call_completed,
        "the replay must carry the tool call create + terminal update pair: {collected:?}"
    );

    // And the load response carries the modes and configOptions surface.
    // The current mode derives from the engine's own state: the test
    // setup lifted the session to yolo, so that is the current mode.
    assert_eq!(response["sessionId"], json!(session_id));
    assert_eq!(response["modes"]["currentModeId"], json!("yolo"));
    let mode_ids: Vec<&str> = response["modes"]["availableModes"]
        .as_array()
        .expect("availableModes is an array")
        .iter()
        .filter_map(|mode| mode["id"].as_str())
        .collect();
    assert_eq!(mode_ids, vec!["default", "plan", "auto", "yolo"]);
    let config_ids: Vec<&str> = response["configOptions"]
        .as_array()
        .expect("configOptions is an array")
        .iter()
        .filter_map(|option| option["id"].as_str())
        .collect();
    assert_eq!(
        config_ids,
        vec!["model", "thinking", "mode"],
        "the configOptions surface: model / thinking / mode"
    );
    assert_eq!(
        response["modes"]["currentModeId"], response["configOptions"][2]["currentValue"],
        "the mode arm and the first-class modes must agree"
    );

    // And `session/resume` deliberately skips the replay (v1's one
    // differentiator vs `session/load`).
    harness
        .handler
        .handle("session/close", json!({"sessionId": session_id}))
        .await
        .expect("session/close must succeed");
    let mut updates = harness.handler.subscribe_updates();
    let response = harness
        .handler
        .handle("session/resume", json!({"sessionId": session_id}))
        .await
        .expect("session/resume must succeed");
    assert_eq!(response["sessionId"], json!(session_id));
    assert_eq!(response["modes"]["currentModeId"], json!("yolo"));
    assert!(
        drain_updates(&mut updates).is_empty(),
        "session/resume must not replay the history"
    );
}

/// Given a session resumed through `session/load`, when its turn hits the
/// manual-mode permission gate, then the reverse-RPC bridge is still
/// alive: the client receives a `session/request_permission` request and
/// the turn continues on its answer (the P5b bridge survives the resume
/// round trip).
#[tokio::test]
async fn should_bridge_permission_requests_on_a_loaded_session() {
    let mock = std::sync::Arc::new(mirri_test_harness::MockLlm::new());
    // SDR-004 (W5): the seed turn below needs a scripted reply; queue it
    // before the bridged turn's script so that script stays intact for
    // the loaded session's prompt.
    mock.push_text_response("seed");
    script_bash_turn(&mock, "call_loaded");
    let harness = ReplayHarness::new(mock);
    let session_id = harness.create_session().await;
    // SDR-004 (W5): converse once — the first user input materializes the
    // session (a never-conversed session is memory-only, nothing to load).
    harness
        .handler
        .handle(
            "session/prompt",
            json!({"sessionId": session_id, "prompt": "seed"}),
        )
        .await
        .expect("session/prompt must succeed");

    // Close and load: the session is a fresh resume from the wire log.
    harness
        .handler
        .handle("session/close", json!({"sessionId": session_id}))
        .await
        .expect("session/close must succeed");
    harness
        .handler
        .handle("session/load", json!({"sessionId": session_id}))
        .await
        .expect("session/load must succeed");

    // When the loaded session's turn calls a tool in manual mode, the
    // bridged permission request still reaches the client.
    let mut updates = harness.handler.subscribe_updates();
    let mut client_requests = harness.handler.subscribe_client_requests();
    let prompt = harness.spawn_prompt(&session_id, "run the tool");
    let pending = timeout(Duration::from_secs(10), client_requests.recv())
        .await
        .expect("the client must receive a session/request_permission request")
        .expect("the client request channel stays open");
    assert_eq!(pending.method, "session/request_permission");
    assert_request_permission_conforms(&pending);
    assert_eq!(pending.params["sessionId"], json!(session_id));
    assert_eq!(
        pending.params["toolCall"]["toolCallId"],
        json!("call_loaded")
    );

    pending.respond(Ok(json!({
        "outcome": {"outcome": "selected", "optionId": "approve_once"},
    })));

    let result = prompt
        .await
        .expect("the prompt task must finish")
        .expect("the turn must settle successfully");
    assert_eq!(result["stopReason"], json!("end_turn"));
    let collected = drain_updates(&mut updates);
    assert!(
        has_tool_status(&collected, "call_loaded", "completed"),
        "the allowed tool must have executed to completion: {collected:?}"
    );
}

/// Given `session/set_mode` with `modeId: "plan"`, when the mode is
/// applied, then the client receives a `current_mode_update` notification
/// (schema-conformant) and a subsequent prompt's tool call is gated by
/// plan mode — the engine's plan-mode guard denies the write tool, so the
/// tool call surfaces as failed without ever executing.
#[tokio::test]
async fn should_gate_tool_calls_with_plan_mode_when_set_mode_selects_plan() {
    let mock = std::sync::Arc::new(mirri_test_harness::MockLlm::new());
    mock.push_tool_use_response(vec![mirri_core::llm::LlmToolCall {
        id: "call_plan_gate".to_string(),
        name: "Bash".to_string(),
        args: json!({"command": "echo forbidden"}),
    }]);
    mock.push_text_response("the plan gate held");
    let harness = ReplayHarness::new(mock);
    let session_id = harness.create_session().await;

    // When the mode is switched to plan
    let mut updates = harness.handler.subscribe_updates();
    let result = harness
        .handler
        .handle(
            "session/set_mode",
            json!({"sessionId": session_id, "modeId": "plan"}),
        )
        .await
        .expect("set_mode(plan) must succeed");
    assert_eq!(result, json!({}));

    // Then the client receives a current_mode_update notification carrying
    // the new mode id.
    let collected = drain_updates(&mut updates);
    let mode_updates: Vec<&Value> = collected
        .iter()
        .filter(|notification| {
            notification["params"]["update"]["sessionUpdate"] == json!("current_mode_update")
        })
        .collect();
    assert_eq!(mode_updates.len(), 1, "exactly one current_mode_update");
    assert_conforms_to_official_schema(mode_updates[0]);
    assert_eq!(
        mode_updates[0]["params"]["update"]["currentModeId"],
        json!("plan")
    );

    // And a subsequent prompt walks the plan gate: the engine's
    // plan-mode guard denies the non-read-only tool call (the turn still
    // settles — the model sees the denial and answers).
    let result = harness
        .handler
        .handle(
            "session/prompt",
            json!({"sessionId": session_id, "prompt": "run the tool"}),
        )
        .await
        .expect("the plan-gated turn must settle");
    assert_eq!(result["stopReason"], json!("end_turn"));
    let collected = drain_updates(&mut updates);
    assert!(
        has_tool_status(&collected, "call_plan_gate", "failed"),
        "the plan-mode guard must deny the tool call: {collected:?}"
    );
    assert!(
        !has_tool_status(&collected, "call_plan_gate", "completed"),
        "the plan-gated tool must never execute: {collected:?}"
    );

    // And the loaded/mode surface reports plan as the current mode (the
    // engine state is the source of the currentModeId).
    harness
        .handler
        .handle("session/close", json!({"sessionId": session_id}))
        .await
        .expect("session/close must succeed");
    let response = harness
        .handler
        .handle("session/load", json!({"sessionId": session_id}))
        .await
        .expect("session/load must succeed");
    assert_eq!(response["modes"]["currentModeId"], json!("plan"));
}

/// Given `session/set_config_option` with `configId: "thinking"` and
/// `value: "medium"`, when the option is set, then it really takes
/// effect: the response's refreshed configOptions report it and the
/// engine's `agent.getConfig` reads the same level back. A model-declared
/// custom spelling (`"max"`) rides the same path, while a blank value is
/// rejected as invalid params.
#[tokio::test]
async fn should_apply_thinking_level_when_set_config_option_sets_it() {
    let harness = ReplayHarness::new(std::sync::Arc::new(mirri_test_harness::MockLlm::new()));
    // The session is created through the ACP surface so the response's
    // mode / configOption surface is asserted on the way in.
    harness
        .handler
        .handle("initialize", json!({"protocolVersion": 1}))
        .await
        .expect("initialize must succeed");
    let created = harness
        .handler
        .handle("session/new", json!({"title": "Config options"}))
        .await
        .expect("session/new must succeed");
    assert_eq!(created["modes"]["currentModeId"], json!("default"));
    let new_config_ids: Vec<&str> = created["configOptions"]
        .as_array()
        .expect("session/new returns configOptions")
        .iter()
        .filter_map(|option| option["id"].as_str())
        .collect();
    assert_eq!(new_config_ids, vec!["model", "thinking", "mode"]);
    // The seeded model declares no `supportEfforts`, so the thinking
    // select falls back to exactly the four canonical options (the
    // undeclared-model surface is unchanged by the open vocabulary).
    let undeclared = created["configOptions"]
        .as_array()
        .expect("session/new returns configOptions")
        .iter()
        .find(|option| option["id"] == json!("thinking"))
        .expect("the thinking option is present");
    let undeclared_values: Vec<&str> = undeclared["options"]
        .as_array()
        .expect("the thinking options are an array")
        .iter()
        .filter_map(|option| option["value"].as_str())
        .collect();
    assert_eq!(undeclared_values, vec!["none", "low", "medium", "high"]);
    let session_id = created["sessionId"]
        .as_str()
        .expect("a sessionId")
        .to_string();

    let response = harness
        .handler
        .handle(
            "session/set_config_option",
            json!({"sessionId": session_id, "configId": "thinking", "value": "medium"}),
        )
        .await
        .expect("setting the thinking option must succeed");
    let thinking = response["configOptions"]
        .as_array()
        .expect("configOptions is an array")
        .iter()
        .find(|option| option["id"] == json!("thinking"))
        .expect("the thinking option is present");
    assert_eq!(thinking["currentValue"], json!("medium"));

    // The engine reads the same level back.
    let config = harness
        .engine
        .call_method("agent.getConfig", json!({"sessionId": session_id}))
        .await
        .expect("agent.getConfig must succeed");
    assert_eq!(config["thinkingEffort"], json!("medium"));

    // A model-declared custom spelling is accepted: the engine's effort
    // vocabulary is open, and the seeded model declares no
    // `support_efforts` allowlist to constrain it (a rejection here would
    // regress to the closed `none`/`low`/`medium`/`high` contract).
    let response = harness
        .handler
        .handle(
            "session/set_config_option",
            json!({"sessionId": session_id, "configId": "thinking", "value": "max"}),
        )
        .await
        .expect("a model-declared custom effort must be accepted");
    let thinking = response["configOptions"]
        .as_array()
        .expect("configOptions is an array")
        .iter()
        .find(|option| option["id"] == json!("thinking"))
        .expect("the thinking option is present");
    assert_eq!(thinking["currentValue"], json!("max"));

    // The engine reads the custom spelling back, verbatim.
    let config = harness
        .engine
        .call_method("agent.getConfig", json!({"sessionId": session_id}))
        .await
        .expect("agent.getConfig must succeed");
    assert_eq!(config["thinkingEffort"], json!("max"));

    // A blank value is still rejected as invalid params: the open
    // vocabulary requires a non-empty string, and the engine's serde
    // error surfaces as -32602 through the ACP translation.
    let (code, _) = harness
        .handler
        .handle(
            "session/set_config_option",
            json!({"sessionId": session_id, "configId": "thinking", "value": "   "}),
        )
        .await
        .expect_err("a blank thinking level must be rejected");
    assert_eq!(code, -32602);
}

/// Given an engine whose config declares the session model with
/// `support_efforts = ["low", "high", "max"]`, when the ACP surface is
/// built (`session/new`), then the `thinking` configOption advertises
/// exactly `none`/`low`/`high`/`max` — the model's declared vocabulary
/// (a host renders one pickable choice per declaration), not the closed
/// canonical four.
#[tokio::test]
async fn should_advertise_the_models_declared_efforts_when_the_thinking_option_is_built() {
    // The engine's catalog declares the default model's efforts,
    // including a custom `max` spelling outside the canonical four.
    let config = mirri_core::ServerConfig {
        default_model: Some("conformance-declared-model".to_string()),
        models: Some(std::collections::HashMap::from([(
            "conformance-declared-model".to_string(),
            mirri_core::ModelAlias {
                provider: "openai".to_string(),
                model: "declared-underlying-model".to_string(),
                max_context_size: 128_000,
                support_efforts: Some(vec![
                    "low".to_string(),
                    "high".to_string(),
                    "max".to_string(),
                ]),
                ..mirri_core::ModelAlias::default()
            },
        )])),
        ..mirri_core::ServerConfig::default()
    };
    let harness = ReplayHarness::new_with_config(
        std::sync::Arc::new(mirri_test_harness::MockLlm::new()),
        config,
    );
    harness
        .handler
        .handle("initialize", json!({"protocolVersion": 1}))
        .await
        .expect("initialize must succeed");
    let created = harness
        .handler
        .handle("session/new", json!({"title": "Declared efforts"}))
        .await
        .expect("session/new must succeed");

    // Then the thinking select offers "none" plus exactly the declared
    // efforts, in declaration order — the custom `max` is pickable.
    let thinking = created["configOptions"]
        .as_array()
        .expect("session/new returns configOptions")
        .iter()
        .find(|option| option["id"] == json!("thinking"))
        .expect("the thinking option is present");
    let values: Vec<&str> = thinking["options"]
        .as_array()
        .expect("the thinking options are an array")
        .iter()
        .filter_map(|option| option["value"].as_str())
        .collect();
    assert_eq!(values, vec!["none", "low", "high", "max"]);
    // And each declared effort is named with the "Thinking <Effort>"
    // spelling (capitalized first letter), like the canonical options.
    let names: Vec<&str> = thinking["options"]
        .as_array()
        .expect("the thinking options are an array")
        .iter()
        .filter_map(|option| option["name"].as_str())
        .collect();
    assert_eq!(
        names,
        vec!["Thinking None", "Thinking Low", "Thinking High", "Thinking Max"]
    );
    // And currentValue still echoes whatever agent.getConfig reports
    // (here the fresh session's "none"), independent of the options list.
    assert_eq!(thinking["currentValue"], json!("none"));
}

// ===== L. stdio smoke against the real provider stack =====================

/// Given a child whose provider env points at a local Anthropic-style SSE
/// endpoint, when a session is prompted, then the assistant's streamed text
/// reaches the client as official `agent_message_chunk` session/update
/// notifications on stdout and the prompt settles with
/// `stopReason: end_turn` (F-P5a-1: the whole stdio chain is exercised —
/// request → engine turn → provider HTTP → streamed events → relay →
/// stdout — through the fully-wired engine's real provider factory,
/// never a mock seam).
#[tokio::test]
async fn should_stream_a_real_provider_turn_over_stdio_when_the_anthropic_env_is_set() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    // The scripted Anthropic SSE turn: message start, two text deltas,
    // the end_turn message_delta, and the stop.
    let sse_body = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",",
        "\"model\":\"claude-sonnet-4-20250514\",\"usage\":{\"input_tokens\":10}}}\n\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,",
        "\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,",
        "\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,",
        "\"delta\":{\"type\":\"text_delta\",\"text\":\" world\"}}\n\n",
        "event: content_block_stop\n",
        "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},",
        "\"usage\":{\"output_tokens\":10}}\n\n",
        "event: message_stop\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    );
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse_body),
        )
        .mount(&server)
        .await;

    let base_url = format!("{}/v1", server.uri());
    let mut acp = AcpProcess::spawn_env(&[
        ("ANTHROPIC_API_KEY", "test-key"),
        ("ANTHROPIC_BASE_URL", base_url.as_str()),
    ])
    .await;

    let response = acp
        .request(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1}}"#)
        .await;
    assert!(
        response["error"].is_null(),
        "initialize must succeed: {response}"
    );

    let response = acp
        .request(r#"{"jsonrpc":"2.0","id":2,"method":"session/new","params":{}}"#)
        .await;
    assert!(
        response["error"].is_null(),
        "session/new must succeed (the provider credentials come from the env): {response}"
    );
    let session_id = response["result"]["sessionId"]
        .as_str()
        .expect("session/new returns a sessionId")
        .to_string();

    // The prompt runs a real engine turn: the engine POSTs /v1/messages,
    // the scripted SSE streams two text deltas, and the assistant's text
    // must arrive as session/update notifications *before* the response.
    acp.send(&format!(
        r#"{{"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{{"sessionId":"{session_id}","prompt":"hi"}}}}"#
    ))
    .await;
    let mut chunk_text = String::new();
    let response = loop {
        let frame = acp.recv().await;
        if frame.get("method").is_some() && frame.get("id").is_none() {
            assert_eq!(
                frame["method"], "session/update",
                "the turn's notifications are session/update relays: {frame}"
            );
            assert_eq!(
                frame["params"]["sessionId"],
                json!(session_id),
                "every update must address the prompted session: {frame}"
            );
            let update = &frame["params"]["update"];
            if update["sessionUpdate"] == "agent_message_chunk" {
                chunk_text.push_str(update["content"]["text"].as_str().unwrap_or_default());
            }
            continue;
        }
        break frame;
    };
    assert!(
        response["error"].is_null(),
        "session/prompt must succeed: {response}"
    );
    assert_eq!(
        response["result"]["stopReason"], "end_turn",
        "the scripted stop_reason flows through to the ACP surface: {response}"
    );
    assert_eq!(
        chunk_text, "Hello world",
        "the streamed deltas must reassemble on the client side"
    );
}
