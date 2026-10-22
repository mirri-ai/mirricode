//! E2E closed loop for the builtin skill bundle (design 13, batch M3 —
//! the Done-when ③ "real e2e" leg).
//!
//! The server is a real `mirri serve`-shaped boot: the full assembly path
//! (`mirri_embedded::bootstrap::assemble` — config load, plugin manager,
//! the complete skill-discovery sequence that seeds the builtin bundle)
//! behind the serve transport, listening on an ephemeral loopback port.
//! Every step drives the wire protocol (JSON-RPC 2.0 over the
//! `/api/v1/rpc` WebSocket) — no in-process engine calls.
//!
//! The boot mirrors `mirri-cli`'s `serve` composition: the transport's
//! `ServerState` is built with the ENGINE's session manager (the same
//! `parts.session_manager`), so sessions created through the engine's RPC
//! surface resolve on the transport's session routing — the shared-manager
//! wiring the real binary uses (minus auth, web SPA, and the multi-root
//! watchers, none of which this loop touches).
//!
//! The loop, in one test (design 13 §6):
//!
//! 1. `skill.list` → the eight builtin entries with the optional gate
//!    flag projection;
//! 2. `agent.activateSkill` of `write-goal` → the embedded body injected
//!    into the turn (the builtin content table supply);
//! 3. a model-side `Skill` call of the user-only `mcp-config` → hard
//!    gate, error text "can only be triggered by the user";
//! 4. `skill.setEnabled` disable → the activation refused → restore →
//!    the activation succeeds again.
//!
//! The engine's LLM factory is swapped for a scripted one (the assembled
//! `EngineParts.llm_factory` seam) so the model side is observable: the
//! mock records every chat call's messages, which is how the test proves
//! the embedded skill body actually reached the model.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use mirri_core::llm::{Llm, LlmChatParams, LlmEvent, LlmEventStream, LlmToolCall, ModelCapability};
use mirri_core::{AgentScopeConfig, LlmFactory, WorkspaceId};
use mirri_embedded::EmbeddedServer;
use mirri_server::ServerState;
use mirri_transport::adapter::EngineAdapter;
use serde_json::{Value, json};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

type TestSocket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

// ===========================================================================
// Scripted, message-recording LLM (the e2e-local counterpart of the
// embedded suite's ScriptedLlm — no recording mock exists in
// mirri-test-harness, and the body-injection assertion needs the model's
// view of the turn)
// ===========================================================================

/// A scripted response: either a plain text end-turn reply or tool calls
/// that keep the loop running.
#[derive(Debug)]
enum ScriptedResponse {
    /// Plain text reply that ends the turn.
    Text(String),
    /// Tool calls that keep the loop running.
    ToolUse(Vec<LlmToolCall>),
}

/// A scripted [`Llm`] that records every chat call's serialized messages —
/// the observation point for "the embedded skill body entered the turn".
#[derive(Debug, Default)]
struct ScriptedLlm {
    responses: Mutex<VecDeque<ScriptedResponse>>,
    recorded_messages: Mutex<Vec<serde_json::Value>>,
}

impl ScriptedLlm {
    fn new() -> Self {
        Self::default()
    }

    /// Queue a plain text reply.
    fn push_text(&self, text: &str) {
        self.responses
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push_back(ScriptedResponse::Text(text.to_string()));
    }

    /// Queue a tool-use reply.
    fn push_tool_use(&self, tool_calls: Vec<LlmToolCall>) {
        self.responses
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push_back(ScriptedResponse::ToolUse(tool_calls));
    }

    /// The serialized messages of the n-th chat call (0-based).
    fn messages_of_call(&self, call: usize) -> serde_json::Value {
        self.recorded_messages
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(call)
            .cloned()
            .unwrap_or(serde_json::Value::Null)
    }
}

impl Llm for ScriptedLlm {
    fn system_prompt(&self) -> &str {
        "You are a scripted e2e assistant."
    }

    fn model_name(&self) -> &str {
        "scripted-e2e-llm"
    }

    fn capability(&self) -> &ModelCapability {
        static CAPABILITY: ModelCapability = ModelCapability {
            image_in: false,
            video_in: false,
            audio_in: false,
            dynamically_loaded_tools: false,
        };
        &CAPABILITY
    }

    fn chat(&self, params: LlmChatParams) -> mirri_core::EngineResult<LlmEventStream> {
        self.recorded_messages
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(serde_json::to_value(&params.messages).unwrap_or_default());
        // Exhausted queue: a placeholder reply, never an empty response
        // (the empty-response guard fails empty-but-completed turns).
        let response = self
            .responses
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .pop_front()
            .unwrap_or_else(|| ScriptedResponse::Text("(no more scripted replies)".to_string()));
        let mut events = VecDeque::new();
        match response {
            ScriptedResponse::Text(text) => {
                for ch in text.chars() {
                    events.push_back(LlmEvent::TextDelta {
                        text: ch.to_string(),
                    });
                }
                if !text.is_empty() {
                    events.push_back(LlmEvent::TextPart { text });
                }
                events.push_back(LlmEvent::Usage {
                    input_tokens: 0,
                    output_tokens: 0,
                    cache_read_tokens: 0,
                    cache_creation_tokens: 0,
                });
                events.push_back(LlmEvent::Done {
                    stop_reason: "completed".to_string(),
                });
            }
            ScriptedResponse::ToolUse(tool_calls) => {
                for tool_call in &tool_calls {
                    events.push_back(LlmEvent::ToolCall {
                        tool: tool_call.name.clone(),
                        args: tool_call.args.clone(),
                        call_id: tool_call.id.clone(),
                    });
                }
                events.push_back(LlmEvent::Usage {
                    input_tokens: 0,
                    output_tokens: 0,
                    cache_read_tokens: 0,
                    cache_creation_tokens: 0,
                });
                events.push_back(LlmEvent::Done {
                    stop_reason: "tool_calls".to_string(),
                });
            }
        }
        Ok(Box::pin(futures::stream::iter(events)))
    }
}

/// An [`LlmFactory`] serving one shared [`ScriptedLlm`].
struct ScriptedLlmFactory {
    llm: std::sync::Arc<ScriptedLlm>,
}

impl LlmFactory for ScriptedLlmFactory {
    fn create_llm(&self, _config: &AgentScopeConfig) -> Result<std::sync::Arc<dyn Llm>, String> {
        Ok(self.llm.clone() as std::sync::Arc<dyn Llm>)
    }
}

// ===========================================================================
// Wire helpers (handshake, frames, RPC calls)
// ===========================================================================

/// Connect to the unified JSON-RPC WebSocket endpoint (`/api/v1/rpc`).
async fn connect_ws(ws_rpc_url: &str) -> TestSocket {
    connect_async(ws_rpc_url)
        .await
        .expect("websocket upgrade")
        .0
}

/// Read the next text frame, parsing it into one or more JSON values
/// (a batched array frame is flattened into its members).
async fn next_frames(socket: &mut TestSocket) -> Vec<Value> {
    let message = tokio::time::timeout(Duration::from_secs(15), socket.next())
        .await
        .expect("message arrives within the timeout")
        .expect("stream stays open")
        .expect("text message");
    let value: Value = serde_json::from_str(message.to_text().expect("UTF-8 text message"))
        .expect("message is valid JSON");
    match value {
        Value::Array(frames) => frames,
        other => vec![other],
    }
}

/// The client's wire demultiplexer: one socket carries BOTH the responses
/// and the subscribed event notifications, and an engine turn's events are
/// published BEFORE the fire-mode call's response arrives — so a response
/// hunt must PRESERVE the interleaved event frames instead of skipping
/// them (they land in a backlog the event waits drain first).
struct Wire {
    socket: TestSocket,
    /// Event notifications read while hunting a response (arrival order).
    events: VecDeque<Value>,
}

impl Wire {
    /// Connect to the endpoint and complete the handshake.
    async fn connect(ws_rpc_url: &str) -> Self {
        let mut socket = connect_ws(ws_rpc_url).await;
        let hello = {
            let mut frames = next_frames(&mut socket).await;
            assert_eq!(
                frames.len(),
                1,
                "expected a single greeting frame, got {frames:?}"
            );
            frames.remove(0)
        };
        assert_eq!(
            hello["method"], "server_hello",
            "first frame is the server greeting"
        );
        socket
            .send(Message::Text(
                r#"{"id":1,"method":"client_hello","params":{"clientId":"skill-e2e-client","capabilities":{"reverseRpc":["approval","question","fs","terminal"]}}}"#
                    .into(),
            ))
            .await
            .expect("send client_hello");
        let mut handshake_events = VecDeque::new();
        let ack = Self::response(&mut socket, 1, &mut handshake_events).await;
        assert_eq!(ack["result"]["accepted"], true, "handshake accepted: {ack}");
        Self {
            socket,
            events: VecDeque::new(),
        }
    }

    /// The raw response hunt: read frames until the response with the given
    /// id arrives, preserving interleaved event notifications in `events`.
    async fn response(socket: &mut TestSocket, id: i64, events: &mut VecDeque<Value>) -> Value {
        loop {
            for frame in next_frames(socket).await {
                if frame.get("id").and_then(Value::as_i64) == Some(id) {
                    return frame;
                }
                if frame
                    .get("method")
                    .and_then(Value::as_str)
                    .is_some_and(|method| method.starts_with("event."))
                {
                    events.push_back(frame);
                }
            }
        }
    }

    /// Send one JSON-RPC request and return the matching response.
    async fn call(&mut self, id: i64, method: &str, params: Value) -> Value {
        self.socket
            .send(Message::Text(
                json!({ "id": id, "method": method, "params": params })
                    .to_string()
                    .into(),
            ))
            .await
            .expect("send rpc request");
        Self::response(&mut self.socket, id, &mut self.events).await
    }

    /// Read until an `event.*` notification matching the predicate arrives —
    /// the backlog (events preserved by the response hunts) is drained first.
    async fn event_where(&mut self, predicate: impl Fn(&Value) -> bool) -> Value {
        if let Some(index) = self.events.iter().position(&predicate) {
            return self.events.remove(index).expect("the position found above");
        }
        loop {
            for frame in next_frames(&mut self.socket).await {
                let is_event = frame
                    .get("method")
                    .and_then(Value::as_str)
                    .is_some_and(|method| method.starts_with("event."));
                if is_event {
                    if predicate(&frame) {
                        return frame;
                    }
                    self.events.push_back(frame);
                }
            }
        }
    }
}

// ===========================================================================
// Server boot — the `mirri serve` composition on an ephemeral port
// ===========================================================================

/// Boot a real server on an ephemeral loopback port: the engine takes the
/// FULL assembly path (config load + the complete skill-discovery sequence
/// that seeds the builtin bundle) with the test's scripted LLM factory
/// swapped into the assembled parts, and the transport's `ServerState` is
/// built with the ENGINE's session manager (the shared-manager wiring
/// `mirri serve` uses, minus auth / web SPA / watchers). Returns the
/// `/api/v1/rpc` WebSocket URL; the temp roots outlive the test.
async fn builtin_skill_server(
    llm: std::sync::Arc<ScriptedLlm>,
) -> (String, tempfile::TempDir, tempfile::TempDir) {
    let workspace = tempfile::tempdir().expect("workspace tempdir");
    let home = tempfile::tempdir().expect("home tempdir");
    // The create chain requires a resolvable model (spec 11 R4 — no
    // built-in fallback), so the injected home carries a config.toml seed,
    // exactly like a real user's home would.
    std::fs::write(
        home.path().join("config.toml"),
        "default_model = \"test-default-model\"\n",
    )
    .expect("seed config.toml");
    // Hermetic discovery: a non-empty explicit skillDirs list replaces the
    // user/project default roots (which would otherwise read the ambient
    // HOME's `~/.agents/skills` of the machine running the test) — the
    // builtin bundle rides the explicit branch unchanged (design 13 §3.3),
    // so the wire list is exactly the eight builtin entries.
    let explicit_skill_dir = workspace.path().join("explicit-skills");
    std::fs::create_dir_all(&explicit_skill_dir).expect("explicit skill dir");
    let mut parts = mirri_embedded::bootstrap::assemble(
        WorkspaceId::new("ws-skill-e2e"),
        workspace.path().to_path_buf(),
        Some(home.path().to_path_buf()),
        None,
        mirri_embedded::bootstrap::WorkspacePolicy::UntrustedDefault,
        vec![explicit_skill_dir],
    );
    parts.llm_factory = std::sync::Arc::new(ScriptedLlmFactory { llm });
    // The shared session manager: cloned BEFORE `from_parts` consumes the
    // parts, then handed to the transport so the engine-created sessions
    // resolve on the transport's session routing.
    let session_manager = std::sync::Arc::clone(&parts.session_manager);
    let engine: std::sync::Arc<dyn EngineAdapter> =
        std::sync::Arc::new(EmbeddedServer::from_parts(parts));
    let state = ServerState::with_engine(engine, session_manager, "0.1.0-test".to_string(), None);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    tokio::spawn(async move {
        if let Err(error) = mirri_server::http::serve(listener, state, None).await {
            std::eprintln!("test server exited with an error: {error}");
        }
    });
    (format!("ws://{addr}/api/v1/rpc"), workspace, home)
}

// ===========================================================================
// The closed loop
// ===========================================================================

/// The `(name, pseudo path, gate)` wire expectations for the eight builtin
/// entries — the TS literals verbatim (design 13 §3.1) and the registration
/// layer's gate flags (six user-slash-only entries carry
/// `disable_model_invocation`; write-goal / update-config carry neither).
const BUILTIN_WIRE_EXPECTATIONS: [(&str, &str, bool); 8] = [
    ("mcp-config", "builtin://mcp-config", true),
    (
        "import-from-cc-codex",
        "builtin://import-from-cc-codex",
        true,
    ),
    ("update-config", "builtin://update-config", false),
    ("custom-theme", "builtin://custom-theme", true),
    ("write-goal", "builtin://write-goal", false),
    ("sub-skill", "builtin://sub-skill", true),
    ("sub-skill.review", "builtin://sub-skill/review", true),
    (
        "sub-skill.consolidate",
        "builtin://sub-skill/consolidate",
        true,
    ),
];

/// E2E: serve the builtin skill bundle over a real port and close the loop
/// — list (with the gate projection) → activate (embedded body in the turn)
/// → model-side call gated → setEnabled disable/refuse/restore.
#[tokio::test]
async fn should_close_the_builtin_skill_loop_when_a_real_server_serves_the_wire() {
    // Given a real server on an ephemeral port: the full assembly path
    // (the builtin bundle seeded through the real discovery sequence)
    // behind the serve transport, with a scripted recording LLM
    let mock = std::sync::Arc::new(ScriptedLlm::new());
    let (ws_rpc_url, _workspace, _home) = builtin_skill_server(std::sync::Arc::clone(&mock)).await;
    let mut wire = Wire::connect(&ws_rpc_url).await;

    // When the workspace skill list is read over the wire
    let listed = wire.call(2, "skill.list", json!({})).await;

    // Then the eight builtin entries are listed with their TS-literal
    // pseudo paths, and the optional gate flag is emitted only when set
    let entries = listed["result"].as_array().expect("the bare array");
    assert_eq!(entries.len(), 8, "entries: {entries:?}");
    for (name, path, gated) in BUILTIN_WIRE_EXPECTATIONS {
        let entry = entries
            .iter()
            .find(|entry| entry["name"] == name)
            .unwrap_or_else(|| panic!("the builtin entry {name} must be listed"));
        assert_eq!(entry["source"], "builtin", "{name} source");
        assert_eq!(entry["path"], path, "{name} pseudo path");
        if gated {
            assert_eq!(
                entry["disable_model_invocation"], true,
                "{name} must carry the gate flag"
            );
        } else {
            assert!(
                entry.get("disable_model_invocation").is_none(),
                "{name} must NOT carry the gate flag: {entry}"
            );
        }
    }

    // And a titled session subscribed to events (the explicit title keeps
    // the side-title LLM call out of the scripted queue) with yolo
    // permissions so the model's Skill call runs unattended
    let created = wire
        .call(3, "session.create", json!({ "title": "builtin e2e" }))
        .await;
    let session_id = created["result"]["id"]
        .as_str()
        .expect("session.create returns the id")
        .to_string();
    let subscribe = wire
        .call(4, "session.subscribe", json!({ "sessionId": session_id }))
        .await;
    assert!(
        subscribe["result"]["seq"].as_u64().is_some(),
        "subscribed: {subscribe}"
    );
    wire.call(
        5,
        "agent.setPermission",
        json!({ "sessionId": session_id, "mode": "yolo" }),
    )
    .await;

    // And the user-slash activation of the builtin write-goal succeeds —
    // the embedded body enters the turn through the builtin content table
    mock.push_text("Goal drafted.");
    let activated = wire
        .call(
            6,
            "agent.activateSkill",
            json!({
                "sessionId": session_id,
                "name": "write-goal",
                "args": "ship the beta"
            }),
        )
        .await;
    assert_eq!(activated["result"]["ok"], true, "activation: {activated}");
    assert_eq!(activated["result"]["skill"], "write-goal");

    // Then the activation is announced with the builtin provenance and the
    // turn settles
    let event = wire
        .event_where(|frame| {
            frame["method"] == "event.skill.activated"
                && frame["params"]["skillName"] == "write-goal"
        })
        .await;
    assert_eq!(event["params"]["trigger"], "user-slash");
    assert_eq!(event["params"]["skillSource"], "builtin");
    assert_eq!(event["params"]["skillPath"], "builtin://write-goal");
    assert_eq!(event["params"]["args"], "ship the beta");
    let _ = wire
        .event_where(|frame| frame["method"] == "event.turn.ended")
        .await;

    // And the model's first call saw the embedded body injected into the
    // turn (the content-table supply behind the rendered skill prompt)
    let first_call = serde_json::to_string(&mock.messages_of_call(0)).unwrap_or_default();
    assert!(
        first_call.contains(r#"<kimi-skill-loaded name=\"write-goal\" trigger=\"user-slash\""#),
        "the model must see the rendered builtin skill prompt: {first_call}"
    );
    assert!(
        first_call.contains("completion contract"),
        "the embedded write-goal body must be inside the turn: {first_call}"
    );

    // And the model-side Skill call of the user-only mcp-config is hard
    // gated — the tool result carries the user-trigger-only error
    mock.push_tool_use(vec![LlmToolCall {
        id: "call_mcp".to_string(),
        name: "Skill".to_string(),
        args: json!({ "skill": "mcp-config" }),
    }]);
    mock.push_text("That skill is user-only; I told the user.");
    wire.call(
        7,
        "message.send",
        json!({ "sessionId": session_id, "text": "configure mcp", "mode": "fire" }),
    )
    .await;
    let tool_result = wire
        .event_where(|frame| {
            frame["method"] == "event.tool.result" && frame["params"]["name"] == "Skill"
        })
        .await;
    assert_eq!(
        tool_result["params"]["isError"], true,
        "frame: {tool_result}"
    );
    assert!(
        tool_result["params"]["output"]
            .as_str()
            .is_some_and(|output| output.contains("can only be triggered by the user")),
        "the gate error must reach the model: {tool_result}"
    );
    let _ = wire
        .event_where(|frame| frame["method"] == "event.turn.ended")
        .await;

    // And the management face closes the loop: disable write-goal → the
    // activation is refused → restore → the activation succeeds again
    let disabled = wire
        .call(
            8,
            "skill.setEnabled",
            json!({ "name": "write-goal", "enabled": false }),
        )
        .await;
    assert_eq!(disabled["result"]["registered"], true, "{disabled}");
    assert_eq!(disabled["result"]["disabled_skills"], json!(["write-goal"]));

    let refused = wire
        .call(
            9,
            "agent.activateSkill",
            json!({ "sessionId": session_id, "name": "write-goal" }),
        )
        .await;
    assert!(
        refused.get("error").is_some(),
        "the disabled builtin must not activate: {refused}"
    );
    assert!(
        refused["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("disabled")),
        "the refusal must carry the disabled error: {refused}"
    );

    let restored_flag = wire
        .call(
            10,
            "skill.setEnabled",
            json!({ "name": "write-goal", "enabled": true }),
        )
        .await;
    assert_eq!(
        restored_flag["result"]["registered"], true,
        "{restored_flag}"
    );

    mock.push_text("Goal drafted after restore.");
    let restored = wire
        .call(
            11,
            "agent.activateSkill",
            json!({ "sessionId": session_id, "name": "write-goal" }),
        )
        .await;
    assert_eq!(restored["result"]["ok"], true, "restored: {restored}");
    let _ = wire
        .event_where(|frame| {
            frame["method"] == "event.skill.activated"
                && frame["params"]["skillName"] == "write-goal"
        })
        .await;
    let _ = wire
        .event_where(|frame| frame["method"] == "event.turn.ended")
        .await;
}

// ===========================================================================
// The session-routed skill.list face (the composer `/` menu path)
// ===========================================================================

/// A two-workspace skill assembly: the bound default root (no skills) plus
/// a second workspace whose root carries `.agents/skills/proj-skill`.
/// Discovery rides the DEFAULT roots (no explicit skillDirs — unlike
/// [`builtin_skill_server`], whose hermetic replacement kills project
/// discovery), mirroring the desktop `mirri web` path. The second workspace
/// is registered over the wire (`workspace.createOrTouch`), not by
/// pre-seeding `workspaces.json`.
///
/// Returns `(ws_rpc_url, second_root, second_workspace_id, keep_alive)`
/// where `keep_alive` keeps every harness tempdir alive for the whole
/// test — the spawned server holds all three roots, and dropping them
/// early would delete directories the registration still references.
async fn two_workspace_skill_server(
    llm: std::sync::Arc<ScriptedLlm>,
) -> (String, String, String, TwoWorkspaceKeepAlive) {
    let bound_root = tempfile::tempdir().expect("bound workspace tempdir");
    let second_root = tempfile::tempdir().expect("second workspace tempdir");
    let home = tempfile::tempdir().expect("home tempdir");
    // The create chain requires a resolvable model (spec 11 R4).
    std::fs::write(
        home.path().join("config.toml"),
        "default_model = \"test-default-model\"\n",
    )
    .expect("seed config.toml");
    // The second workspace's project-level skill.
    let skill_dir = second_root.path().join(".agents/skills/proj-skill");
    std::fs::create_dir_all(&skill_dir).expect("create project skill dir");
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\ndescription: A second-workspace project skill.\n---\n# proj-skill\n",
    )
    .expect("write SKILL.md");
    let mut parts = mirri_embedded::bootstrap::assemble(
        WorkspaceId::new("ws-skill-session-e2e"),
        bound_root.path().to_path_buf(),
        Some(home.path().to_path_buf()),
        None,
        mirri_embedded::bootstrap::WorkspacePolicy::UntrustedDefault,
        Vec::new(),
    );
    parts.llm_factory = std::sync::Arc::new(ScriptedLlmFactory { llm });
    let session_manager = std::sync::Arc::clone(&parts.session_manager);
    let engine: std::sync::Arc<dyn EngineAdapter> =
        std::sync::Arc::new(EmbeddedServer::from_parts(parts));
    let state = ServerState::with_engine(engine, session_manager, "0.1.0-test".to_string(), None);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    tokio::spawn(async move {
        if let Err(error) = mirri_server::http::serve(listener, state, None).await {
            std::eprintln!("test server exited with an error: {error}");
        }
    });
    (
        format!("ws://{addr}/api/v1/rpc"),
        second_root.path().to_string_lossy().into_owned(),
        mirri_core::encode_work_dir_key(second_root.path()),
        TwoWorkspaceKeepAlive {
            _bound_root: bound_root,
            _second_root: second_root,
            _home: home,
        },
    )
}

/// The harness tempdirs the spawned server still references — returned so
/// the caller's binding keeps them alive for the whole test.
struct TwoWorkspaceKeepAlive {
    _bound_root: tempfile::TempDir,
    _second_root: tempfile::TempDir,
    _home: tempfile::TempDir,
}

/// Extract the `result` array of a `skill.list` response into owned names.
fn skill_names_of(listed: &Value) -> Vec<String> {
    listed["result"]
        .as_array()
        .expect("the bare array")
        .iter()
        .filter_map(|entry| entry["name"].as_str().map(str::to_string))
        .collect()
}

/// E2E: the session-routed `skill.list { sessionId }` must answer with the
/// session's OWN workspace catalog — the composer `/` menu path. A session
/// of a second (non-default) workspace sees that workspace's project
/// skills; a session of the default workspace does not.
#[tokio::test]
async fn should_list_project_skills_when_skill_list_is_routed_by_session_id() {
    // Given a two-workspace server whose second root carries a project skill
    let mock = std::sync::Arc::new(ScriptedLlm::new());
    let (ws_rpc_url, second_root, second_id, _keep_alive) =
        two_workspace_skill_server(std::sync::Arc::clone(&mock)).await;
    let mut wire = Wire::connect(&ws_rpc_url).await;

    // And the second workspace registered over the wire (the desktop path)
    let touched = wire
        .call(2, "workspace.createOrTouch", json!({ "root": second_root }))
        .await;
    assert!(
        touched["result"]["id"].as_str().is_some(),
        "createOrTouch returns the id: {touched}"
    );

    // And a session created IN the second workspace
    let created = wire
        .call(
            3,
            "session.create",
            json!({ "title": "proj skills", "workspaceId": second_id }),
        )
        .await;
    let session_id = created["result"]["id"]
        .as_str()
        .expect("session.create returns the id")
        .to_string();

    // When skill.list is routed by sessionId (the composer `/` menu path)
    let listed = wire
        .call(4, "skill.list", json!({ "sessionId": session_id }))
        .await;

    // Then the second workspace's project skill is listed
    let names = skill_names_of(&listed);
    assert!(
        names.iter().any(|name| name == "proj-skill"),
        "the session's workspace project skill must be listed, got: {names:?}"
    );

    // And the workspace-routed face agrees (the existing correct path)
    let ws_listed = wire
        .call(5, "skill.list", json!({ "workspaceId": second_id }))
        .await;
    let ws_names = skill_names_of(&ws_listed);
    assert!(
        ws_names.iter().any(|name| name == "proj-skill"),
        "the workspace-routed list must carry the project skill, got: {ws_names:?}"
    );

    // And a session of the DEFAULT workspace does not see it
    let default_created = wire
        .call(6, "session.create", json!({ "title": "default ws" }))
        .await;
    let default_sid = default_created["result"]["id"]
        .as_str()
        .expect("session.create returns the id")
        .to_string();
    let default_listed = wire
        .call(7, "skill.list", json!({ "sessionId": default_sid }))
        .await;
    let default_names = skill_names_of(&default_listed);
    assert!(
        !default_names.iter().any(|name| name == "proj-skill"),
        "the default workspace's session must NOT see the second workspace's \
         project skill, got: {default_names:?}"
    );
}
