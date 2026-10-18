//! NAPI (Node.js N-API) FFI surface — the exports JavaScript sees.
//!
//! Node.js `require()`s the compiled `.node` cdylib and calls the functions
//! and classes registered here by `napi-derive`:
//!
//! - [`create_engine`] — construct an embedded engine instance (JS:
//!   `createEngine`)
//! - [`create_remote_engine`] — construct an engine backed by a remote
//!   `mirri-server` WS endpoint (JS: `createRemoteEngine`)
//! - [`MirriEngine`] — class whose methods dispatch into the Rust engine:
//!   - [`MirriEngine::call_method`] — forward RPC
//!     (`method` + `params` → `Promise<result>`, JS: `callMethod`)
//!   - [`MirriEngine::on_event`] — subscribe to engine events
//!     (JS: `onEvent`)
//!   - [`MirriEngine::on_reverse_rpc`] — register the reverse-RPC handler
//!     (JS: `onReverseRpc`)
//!   - [`MirriEngine::respond_reverse_rpc`] /
//!     [`MirriEngine::respond_reverse_rpc_error`] — answer a pending reverse
//!     RPC request (JS: `respondReverseRpc` / `respondReverseRpcError`)
//!   - [`MirriEngine::dispose`] — release the engine
//!
//! ## Async surface (Phase 4.1)
//!
//! `callMethod`, `respondReverseRpc` / `respondReverseRpcError`, and
//! `dispose` are `async fn`s: napi-rs drives them on its global Tokio
//! runtime and returns a Promise to JavaScript, so the Node.js event loop is
//! never blocked. Each future hops onto the bridge's own runtime before
//! touching the engine (see [`EngineBridge::call_method`]).
//!
//! `onEvent` and `onReverseRpc` wrap the JS callback in a
//! `ThreadsafeFunction`; the bridge's forwarder tasks (running on the Tokio
//! runtime) call back into JavaScript on the Node.js main thread without
//! blocking it.
//!
//! ## Relationship to the Phase 3 design
//!
//! The design doc (`.tmp/rust-core-design/03-phase3-tui-acp-migration.md`,
//! §2.3–§2.5) specifies this surface: `callMethod` returning a Promise
//! (engine on the Tokio worker pool, Node event loop never blocked),
//! `onEvent` / `onReverseRpc` subscriptions, reverse RPC answers, and a
//! graceful multi-step `dispose()`.

use std::sync::Arc;

use napi::threadsafe_function::ThreadsafeFunctionCallMode;
use napi_derive::napi;
use serde_json::json;

use crate::engine_bridge::EngineBridge;
use mirri_proto::rpc::{JSON_RPC_INTERNAL, JSON_RPC_METHOD_NOT_FOUND};
use mirri_transport::adapter::EngineError;

/// Convert an [`EngineError`] into the [`napi::Error`] JavaScript receives.
///
/// The N-API error channel is two strings — a status Node.js surfaces as the
/// JS `Error`'s `code`, and a reason that becomes its `message` — and napi-rs
/// 2.16's async promise machinery pins the status to the fixed `Status` enum
/// (`"GenericFailure"`), so no arbitrary code can ride it. The structured
/// engine error information (the JSON-RPC code and the domain code) therefore
/// travels as a compact JSON suffix on the message, and the TS SDK
/// (`#/rust-engine/error-channel`) parses it back off:
///
/// ```text
/// Not implemented: … [mirri-rpc-error {"code":-32603,"domainCode":"not_implemented","retryable":false}]
/// ```
///
/// The human-readable text stays the message's prefix, so a consumer that
/// never parses the suffix still reads a clear reason (and a better one than
/// the former `EngineError` Display, which wrapped everything in
/// `rpc error: code=… message=…`). The suffix is always a single trailing
/// JSON object prefixed with `[mirri-rpc-error `, which no hand-written
/// message ends with; `JSON.parse` validates it on the JS side.
fn engine_error_to_napi(error: EngineError) -> napi::Error {
    let (code, message, data) = match error {
        EngineError::Rpc {
            code,
            message,
            data,
        } => (code, message, data),
        // Reserved for genuinely unknown methods (the JSON-RPC standard
        // code); a retired/unimplemented method is an `Rpc` error with the
        // `not_implemented` domain code, never this.
        EngineError::MethodNotFound(method) => (
            JSON_RPC_METHOD_NOT_FOUND,
            format!("method not found: {method}"),
            None,
        ),
        // Bridge-level failures (panicked task, transport): internal errors.
        error => (JSON_RPC_INTERNAL, error.to_string(), None),
    };
    let domain_code = data
        .as_ref()
        .and_then(|data| data.get("domainCode"))
        .and_then(|value| value.as_str());
    // The retryability verdict rides the same `data` payload the wire
    // surfaces carry (`domain_error_data`), so SDK consumers branch on the
    // same field regardless of the engine host.
    let retryable = data
        .as_ref()
        .and_then(|data| data.get("retryable"))
        .and_then(|value| value.as_bool());
    // The quota-exhausted error's `Retry-After` hint (P11a F-P8-13) —
    // carried in milliseconds so the TS decoder can surface "retry after N"
    // without re-parsing the message.
    let retry_after_ms = data
        .as_ref()
        .and_then(|data| data.get("retryAfterMs"))
        .and_then(|value| value.as_u64());
    let payload = match (domain_code, retryable, retry_after_ms) {
        (Some(domain_code), Some(retryable), Some(retry_after_ms)) => json!({
            "code": code,
            "domainCode": domain_code,
            "retryable": retryable,
            "retryAfterMs": retry_after_ms
        }),
        (Some(domain_code), Some(retryable), None) => {
            json!({ "code": code, "domainCode": domain_code, "retryable": retryable })
        }
        (Some(domain_code), None, Some(retry_after_ms)) => json!({
            "code": code,
            "domainCode": domain_code,
            "retryAfterMs": retry_after_ms
        }),
        (Some(domain_code), None, None) => json!({ "code": code, "domainCode": domain_code }),
        (None, _, _) => json!({ "code": code }),
    };
    napi::Error::from_reason(format!("{message} [mirri-rpc-error {payload}]"))
}

/// Engine instance handle returned to JavaScript.
///
/// One `MirriEngine` owns one [`EngineBridge`] (one Tokio runtime + one
/// `EmbeddedServer`). JavaScript holds this object and calls its methods;
/// the Rust side is dropped when the JS object is garbage collected.
#[napi]
pub struct MirriEngine {
    bridge: Arc<EngineBridge>,
    /// Plugin-manager directory owned by this engine (mock-llm factories
    /// only). Removed on [`MirriEngine::dispose`] so temp accumulation stays
    /// bounded; `None` for the production [`create_engine`] path.
    #[cfg(feature = "mock-llm")]
    plugin_dir: Option<std::path::PathBuf>,
}

/// Construct a new engine instance.
///
/// `home_dir` optionally roots the engine's Mirri home directory — the
/// directory holding `config.toml`, `profiles/`, `plugins/`, MCP config, and
/// the session persistence root — instead of the env-based
/// `mirri_home_dir()` resolution (`MIRRICODE_HOME`, then `$HOME/.mirri-code`).
/// The TS harness passes its isolated `homeDir` here (I-1) so an engine under
/// test never reads or writes the developer's real home; `None`/omitted keeps
/// the ambient-environment behavior.
///
/// `skill_dirs` optionally carries the SDK's explicit skill directories
/// (v1/v2 `skillDirs`, the CLI's `--skills-dir`): when non-empty they
/// **replace** the engine's default user/project skill discovery and serve
/// as the user skill source — the same contract the v1
/// ([`MirriCore`](mirri_node) `skillDirs`) and v2 (`BootstrapInput.args.
/// skillDirs`) TS engines implement. `None`/omitted/empty keeps the ambient
/// discovery (`<cwd>/.agents/skills` + `~/.agents/skills`).
#[napi]
pub fn create_engine(home_dir: Option<String>, skill_dirs: Option<Vec<String>>) -> MirriEngine {
    MirriEngine {
        bridge: Arc::new(EngineBridge::with_home_dir(
            home_dir.map(std::path::PathBuf::from),
            skill_dirs
                .unwrap_or_default()
                .into_iter()
                .map(std::path::PathBuf::from)
                .collect(),
        )),
        #[cfg(feature = "mock-llm")]
        plugin_dir: None,
    }
}

/// Construct an engine backed by a remote `mirri-server` instance speaking
/// the unified JSON-RPC 2.0 WebSocket protocol (`/api/v1/rpc`).
///
/// `url` is the WebSocket endpoint, e.g. `ws://127.0.0.1:58627/api/v1/rpc`;
/// `auth_token` is offered as the `mirri-code.bearer.<token>` subprotocol
/// when the server requires authentication. The engine negotiates the
/// `approval` / `question` reverse-RPC domains at the handshake and
/// auto-subscribes to every session, so the resulting [`MirriEngine`] is
/// drop-in equivalent to an embedded engine from the JS side (the NAPI
/// surface is identical; only the engine behind the bridge differs).
///
/// The function is async because connecting performs the protocol handshake;
/// napi-rs drives it on its Tokio runtime, and the returned promise rejects
/// with the transport error when the server is unreachable, the handshake
/// fails, or the bearer token is rejected.
#[napi]
pub async fn create_remote_engine(
    url: String,
    auth_token: Option<String>,
) -> napi::Result<MirriEngine> {
    let engine = mirri_remote::RemoteAdapter::connect(
        &url,
        auth_token.as_deref(),
        &["approval", "question"],
    )
    .await
    .map_err(engine_error_to_napi)?;
    Ok(MirriEngine {
        bridge: Arc::new(EngineBridge::with_engine(engine)),
        #[cfg(feature = "mock-llm")]
        plugin_dir: None,
    })
}

/// Construct an engine backed by a scripted mock LLM (feature `mock-llm`).
///
/// Each string in `responses` is queued as a plain-text end-turn reply; the
/// mock LLM replays them in FIFO order, one per `chat()` call. This lets
/// TS-side E2E tests exercise the full NAPI → EngineBridge →
/// EmbeddedServer → LLM loop without a real provider.
///
/// A `"coder"` agent profile is registered programmatically so the `Agent`
/// and `AgentSwarm` tools are available (the mock LLM factory ignores the
/// profile's model field).
///
/// `home_dir` optionally roots the engine's session persistence under
/// `<home>/v3_sessions` instead of the workspace-local
/// `<cwd>/.mirri/v3_sessions`,
/// and RPC-time home-relative resolution prefers the injected home — the
/// same rooting the production [`create_engine`] applies. The parameter is
/// trailing and optional so existing JS callers that pass only the
/// responses array keep working (`createEngineWithMockLlm([…]`)).
///
/// Only available when the `mirri-node` crate is built with
/// `--features mock-llm`; production npm builds never enable it.
#[cfg(feature = "mock-llm")]
#[napi]
pub fn create_engine_with_mock_llm(
    responses: Vec<String>,
    home_dir: Option<String>,
) -> MirriEngine {
    use mirri_embedded::EmbeddedServer;
    use mirri_test_harness::{MockLlm, MockLlmFactory};

    let manager = mock_session_manager(home_dir.as_deref());
    let mock = Arc::new(MockLlm::new());
    for text in &responses {
        mock.push_text_response(text);
    }
    let factory: Arc<dyn mirri_embedded::LlmFactory> =
        Arc::new(MockLlmFactory::new(Arc::clone(&mock)));
    let profile_manager = mock_profile_manager();

    // The mock engine's config seeds exactly one model alias: the contract
    // e2e's `agent.setModel` fixture switches the runtime model to
    // `contract-e2e-model`, and the embedded set_model path validates the
    // alias against the config's model map (a fresh default config accepts
    // none). The mock LLM factory ignores the resolved model, so the alias
    // needs no provider backing.
    let mut config = mirri_core::ServerConfig::default();
    config.models = Some(std::collections::HashMap::from([(
        "contract-e2e-model".to_string(),
        mirri_core::config::ModelAlias {
            provider: "mock".to_string(),
            model: "mock-llm".to_string(),
            max_context_size: 128_000,
            ..Default::default()
        },
    )]));

    let (plugin_manager, plugin_dir) = mock_plugin_manager();
    let engine = Arc::new(
        EmbeddedServer::with_providers(
            manager,
            Some(factory),
            Some(profile_manager),
            None,
            None,
            Arc::new(std::sync::RwLock::new(config)),
            None,
        )
        .with_plugin_manager(plugin_manager)
        .with_home_dir(home_dir.map(std::path::PathBuf::from)),
    );
    MirriEngine {
        bridge: Arc::new(EngineBridge::with_engine(engine)),
        plugin_dir: Some(plugin_dir),
    }
}

/// Build the session manager mock engines run on, rooted at the optional
/// injected Mirri home directory — the same rooting the production
/// [`create_engine`] applies through
/// [`EmbeddedServer::for_workspace_root`](mirri_embedded::EmbeddedServer::for_workspace_root):
/// with a home, session persistence roots under `<home>/v3_sessions` (the
/// rust engine's own layout, ADR-006-R2) instead of the workspace-local
/// `<cwd>/.mirri/v3_sessions`, so a mock engine under test never writes into
/// the process working directory.
#[cfg(feature = "mock-llm")]
fn mock_session_manager(home_dir: Option<&str>) -> Arc<mirri_core::SessionManager> {
    use mirri_core::{SessionManager, WorkspaceId, WorkspaceScope, WorkspaceScopeConfig};

    let workspace = Arc::new(WorkspaceScope::new(
        WorkspaceId::new("embedded"),
        std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
        WorkspaceScopeConfig::default(),
    ));
    Arc::new(match home_dir {
        Some(home) => SessionManager::with_persistence_dir(
            workspace,
            std::path::Path::new(home).join("v3_sessions"),
            std::sync::Arc::new(mirri_os::LocalFs::new()),
        ),
        None => SessionManager::new(workspace),
    })
}

/// Register a `"coder"` agent profile so the `Agent` and `AgentSwarm` tools
/// can resolve their default subagent type. The mock LLM factory ignores the
/// profile's model field.
#[cfg(feature = "mock-llm")]
fn mock_profile_manager() -> Arc<mirri_core::ProfileManager> {
    use mirri_core::{AgentProfile, ProfileManager};

    let profile_manager = Arc::new(ProfileManager::new());
    profile_manager
        .register(AgentProfile {
            name: "coder".to_string(),
            model: "mock-llm".to_string(),
            ..Default::default()
        })
        .unwrap();
    profile_manager
}

/// Build an isolated [`mirri_plugin_host::PluginManager`] for a mock-llm
/// engine, backed by a unique per-engine subdirectory under one per-process
/// base directory in `std::env::temp_dir()`.
///
/// The per-engine subdirectory keeps concurrent engines (same or different
/// processes) isolated from each other, and the per-process base keeps the
/// leftover footprint bounded to one directory per process even if an engine
/// is never disposed. The manager never touches the user's home /
/// `mirri_home_dir()`, keeping the engine independent of the host
/// environment. `load_all()` succeeds on a missing or empty directory —
/// `PluginStore::load` returns an empty `Vec` when `installed.json` does not
/// exist — so `plugin.reload` resolves to the real `{added, removed, errors}`
/// ReloadSummary shape (three empty arrays).
///
/// Returns the manager together with its directory; the caller stores the
/// directory on the engine so [`MirriEngine::dispose`] can remove it.
#[cfg(feature = "mock-llm")]
fn mock_plugin_manager() -> (Arc<mirri_plugin_host::PluginManager>, std::path::PathBuf) {
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir()
        .join(format!("mirri-node-mock-plugins-{}", std::process::id(),))
        .join(format!("engine-{seq}"));
    // `PluginStore::save` creates the directory lazily via `create_dir_all`
    // on the first write; create it eagerly so the manager always points at
    // an existing empty directory.
    let _ = std::fs::create_dir_all(&dir);
    (
        Arc::new(mirri_plugin_host::PluginManager::new(dir.clone())),
        dir,
    )
}

/// A single scripted mock LLM response. Either `text` (plain end-turn reply)
/// or `toolCalls` (a tool-use response that triggers tool execution).
#[cfg(feature = "mock-llm")]
#[napi(object)]
pub struct MockLlmResponseSpec {
    /// Plain text reply. When set (and `tool_calls` is empty), this is an
    /// end-turn response with stop_reason "completed".
    pub text: Option<String>,
    /// Tool calls to emit. When non-empty, stop_reason is "tool_calls" and
    /// the agent loop will execute the requested tools.
    pub tool_calls: Vec<MockLlmToolCallSpec>,
}

/// A tool call in a scripted mock LLM response.
#[cfg(feature = "mock-llm")]
#[napi(object)]
pub struct MockLlmToolCallSpec {
    /// Tool name, e.g. "Agent", "Read", "Bash".
    pub name: String,
    /// Tool call ID. Auto-generated when omitted.
    pub id: Option<String>,
    /// Tool arguments as a JSON object.
    pub args: serde_json::Value,
}

/// Construct an engine with a scripted mock LLM that supports tool calls.
///
/// Like [`create_engine_with_mock_llm`] but each response can include tool
/// calls, enabling E2E tests for subagent spawning, tool execution, and
/// multi-turn agent loops. Responses are replayed in FIFO order across all
/// agents (main + subagents share one queue).
///
/// `home_dir` threads through exactly like [`create_engine_with_mock_llm`]:
/// session persistence roots under `<home>/v3_sessions` when given (trailing
/// and optional, so existing JS callers that pass only the responses array
/// keep working).
#[cfg(feature = "mock-llm")]
#[napi]
pub fn create_engine_with_scripted_mock_llm(
    responses: Vec<MockLlmResponseSpec>,
    home_dir: Option<String>,
) -> MirriEngine {
    use mirri_embedded::EmbeddedServer;
    use mirri_test_harness::{MockLlm, MockLlmFactory, MockResponse};

    let manager = mock_session_manager(home_dir.as_deref());
    let mock = Arc::new(MockLlm::new());
    for spec in &responses {
        let response = if !spec.tool_calls.is_empty() {
            let tool_calls: Vec<mirri_core::LlmToolCall> = spec
                .tool_calls
                .iter()
                .enumerate()
                .map(|(i, tc)| mirri_core::LlmToolCall {
                    id: tc.id.clone().unwrap_or_else(|| format!("call_{}", i)),
                    name: tc.name.clone(),
                    args: tc.args.clone(),
                })
                .collect();
            MockResponse::tool_use(tool_calls)
        } else {
            MockResponse::end_turn(spec.text.as_deref().unwrap_or(""))
        };
        mock.push_response(response);
    }
    let factory: Arc<dyn mirri_embedded::LlmFactory> =
        Arc::new(MockLlmFactory::new(Arc::clone(&mock)));
    let profile_manager = mock_profile_manager();

    let (plugin_manager, plugin_dir) = mock_plugin_manager();
    let engine = Arc::new(
        EmbeddedServer::with_profile_manager(manager, Some(factory), Some(profile_manager))
            .with_plugin_manager(plugin_manager)
            .with_home_dir(home_dir.map(std::path::PathBuf::from)),
    );
    MirriEngine {
        bridge: Arc::new(EngineBridge::with_engine(engine)),
        plugin_dir: Some(plugin_dir),
    }
}

/// The embedded engine's dispatchable method names — the Embedded-available
/// entries of the `mirri-proto` RPC registry (the contract fact source,
/// M1.1), via [`current_names_for`], sorted alphabetically. The view yields
/// the canonical registry names (after the M3-c rename folds, `plan.status`
/// etc. — the old twin names are gone from the wire).
///
/// Used by the TS-side contract e2e suite
/// (`packages/node-sdk/test/rust-engine-contract-e2e.test.ts`) to reconcile
/// the 180-method fixture against the registry across the NAPI boundary, so a
/// dispatch arm added or renamed in Rust trips a TS test, not production.
///
/// Only available when the crate is built with `--features mock-llm`;
/// production npm builds never enable it.
#[cfg(feature = "mock-llm")]
#[napi]
pub fn embedded_method_names() -> Vec<String> {
    let mut names: Vec<&'static str> =
        mirri_proto::rpc::current_names_for(mirri_proto::rpc::RpcBackend::Embedded).collect();
    names.sort_unstable();
    names.into_iter().map(String::from).collect()
}

/// The embedded engine's projected event method names — the closed
/// vocabulary of `event.*` notification methods the embedded relay can emit:
/// the session-scoped
/// [`PROJECTED_EVENT_METHODS`](mirri_core::event_projection::PROJECTED_EVENT_METHODS)
/// union the server-scoped
/// [`SERVER_PROJECTED_EVENT_METHODS`](mirri_core::event_projection::SERVER_PROJECTED_EVENT_METHODS)
/// (`event.workspace.created/updated/deleted`, the W2 server lifecycle
/// surface), sorted.
///
/// Used by the TS-side contract suite to reconcile the event vocabulary
/// across the NAPI boundary: a projection method added or renamed in Rust
/// trips the TS test, not production, instead of silently drifting from the
/// contract.
///
/// Only available when the crate is built with `--features mock-llm`;
/// production npm builds never enable it.
#[cfg(feature = "mock-llm")]
#[napi]
pub fn embedded_event_methods() -> Vec<String> {
    let mut methods: Vec<&str> = mirri_core::event_projection::PROJECTED_EVENT_METHODS
        .iter()
        .chain(mirri_core::event_projection::SERVER_PROJECTED_EVENT_METHODS.iter())
        .copied()
        .collect();
    methods.sort_unstable();
    methods.into_iter().map(String::from).collect()
}

#[napi]
impl MirriEngine {
    /// Call an RPC method on the engine.
    ///
    /// `params` is the JSON-RPC `params` payload (a JS object); the returned
    /// promise resolves with the JSON-RPC `result`. Engine errors reject the
    /// promise with a JavaScript `Error` whose `message` ends in the
    /// structured `[mirri-rpc-error {"code":…,"domainCode":…}]` suffix —
    /// see [`engine_error_to_napi`].
    #[napi]
    pub async fn call_method(
        &self,
        method: String,
        params: serde_json::Value,
    ) -> napi::Result<serde_json::Value> {
        self.bridge
            .call_method(&method, params)
            .await
            .map_err(engine_error_to_napi)
    }

    /// Subscribe to engine events.
    ///
    /// `callback` is invoked on the Node.js main thread for each engine
    /// event with a `{ method, params }` payload. Each engine supports one
    /// event subscription; calling `onEvent` again replaces the previous
    /// forwarder.
    #[napi]
    pub fn on_event(
        &self,
        // napi-rs ThreadsafeFunction defaults to CalleeHandled error strategy:
        // the JS callback receives (err, value) — err is null on success, value
        // is the event object. This matches Node.js error-first convention.
        #[napi(ts_arg_type = "(err: null, event: { method: string; params: unknown }) => void")]
        callback: napi::threadsafe_function::ThreadsafeFunction<serde_json::Value>,
    ) -> napi::Result<()> {
        self.bridge.start_event_forwarder(move |payload| {
            // The bridge already delivers `{ method, params }` objects; pass
            // through directly to avoid redundant clone-and-rebuild.
            let _ = callback.call(Ok(payload), ThreadsafeFunctionCallMode::NonBlocking);
        });
        Ok(())
    }

    /// Register the reverse-RPC handler.
    ///
    /// When the engine needs client-side action (approval, question, fs
    /// proxy, terminal) it invokes `callback` on the Node.js main thread
    /// with a `{ id, method, params }` request; the client answers with
    /// [`MirriEngine::respond_reverse_rpc`] or
    /// [`MirriEngine::respond_reverse_rpc_error`].
    #[napi]
    pub fn on_reverse_rpc(
        &self,
        // Same CalleeHandled convention as on_event: (err, request) in JS.
        #[napi(
            ts_arg_type = "(err: null, request: { id: number; method: string; params: unknown }) => void"
        )]
        callback: napi::threadsafe_function::ThreadsafeFunction<serde_json::Value>,
    ) -> napi::Result<()> {
        self.bridge
            .start_reverse_rpc_forwarder(move |id, method, params| {
                let payload = serde_json::json!({
                    "id": id,
                    "method": method,
                    "params": params,
                });
                let _ = callback.call(Ok(payload), ThreadsafeFunctionCallMode::NonBlocking);
            });
        Ok(())
    }

    /// Answer a pending reverse RPC request with a successful result.
    #[napi]
    pub async fn respond_reverse_rpc(
        &self,
        id: i64,
        #[napi(ts_arg_type = "any")] result: serde_json::Value,
    ) -> napi::Result<()> {
        self.bridge.respond_reverse_rpc(id, Ok(result)).await;
        Ok(())
    }

    /// Answer a pending reverse RPC request with an error.
    #[napi]
    pub async fn respond_reverse_rpc_error(&self, id: i64, error: String) -> napi::Result<()> {
        self.bridge.respond_reverse_rpc(id, Err(error)).await;
        Ok(())
    }

    /// Dispose the engine, cancelling all in-flight work.
    ///
    /// Idempotent from the engine's perspective: the underlying
    /// `EmbeddedServer::dispose` always succeeds, and repeated calls are a
    /// no-op from the engine's side (design doc §2.5.2).
    ///
    /// Mock-llm engines also drop their per-engine plugin directory here so
    /// temp accumulation stays bounded (see [`mock_plugin_manager`]).
    /// `remove_dir` only succeeds on empty directories: if plugin state was
    /// actually written, keep it rather than recursing into unknown content.
    /// The per-process base directory is removed too when this was the last
    /// engine still holding a subdirectory.
    #[napi]
    pub async fn dispose(&self) -> napi::Result<()> {
        self.bridge.dispose().await.map_err(engine_error_to_napi)?;
        #[cfg(feature = "mock-llm")]
        if let Some(dir) = &self.plugin_dir {
            let _ = std::fs::remove_dir(dir);
            if let Some(base) = dir.parent() {
                let _ = std::fs::remove_dir(base);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[cfg(feature = "mock-llm")]
    mod mock_llm_home_dir {
        use super::*;

        #[tokio::test]
        async fn should_persist_sessions_under_the_injected_home_when_mock_engine_created_with_one()
        {
            // Given an isolated home directory and a mock engine rooted at it
            let home = tempfile::tempdir().expect("tempdir must be creatable");
            let engine = create_engine_with_mock_llm(
                vec!["dummy".to_string()],
                Some(home.path().to_string_lossy().into_owned()),
            );

            // When a session is created through the engine's RPC surface
            let result = engine
                .bridge
                .call_method("session.create", json!({ "title": "home-dir test" }))
                .await
                .expect("session.create resolves");
            assert!(result.get("id").is_some(), "session view carries an id");

            // Then the session persisted under the injected home's
            // v3_sessions root — not the workspace-local
            // `<cwd>/.mirri/v3_sessions` the mock factories used before
            // homeDir threading
            let sessions_dir = home.path().join("v3_sessions");
            let entries: Vec<_> = std::fs::read_dir(&sessions_dir)
                .expect("<home>/v3_sessions exists after session.create")
                .collect();
            assert!(
                !entries.is_empty(),
                "session state landed under the injected home"
            );

            // And the engine's home resolution carries the injected home
            // (RPC-time consumers like session.export prefer it)
            engine.bridge.dispose().await.expect("dispose succeeds");
        }

        #[tokio::test]
        async fn should_keep_ambient_persistence_when_mock_engine_created_without_home_dir() {
            // Given a mock engine built without an injected home
            let engine = create_engine_with_mock_llm(vec!["dummy".to_string()], None);

            // When a session is created through the engine's RPC surface
            let result = engine
                .bridge
                .call_method("session.create", json!({}))
                .await
                .expect("session.create resolves");
            assert!(result.get("id").is_some(), "session view carries an id");

            // Then the engine stays functional (ambient persistence) and the
            // optional trailing parameter did not change the default path
            engine.bridge.dispose().await.expect("dispose succeeds");
        }
    }

    #[test]
    fn should_carry_the_code_and_domain_code_in_the_message_suffix_when_converting_an_rpc_error() {
        // Given an engine RPC error carrying a domain code (the M1.2-b wire
        // shape: code + message + data.domainCode)
        let error = EngineError::Rpc {
            code: -32603,
            message: "Not implemented: task.detach is not supported".to_string(),
            data: Some(json!({ "domainCode": "not_implemented" })),
        };

        // When it crosses the NAPI boundary
        let napi_error = engine_error_to_napi(error);

        // Then the message keeps the human-readable text as its prefix
        assert!(
            napi_error
                .reason
                .starts_with("Not implemented: task.detach is not supported [mirri-rpc-error ")
        );
        // And the structured suffix carries both the code and the domain code
        // for the TS-side decoder
        assert_eq!(
            napi_error.reason,
            "Not implemented: task.detach is not supported \
             [mirri-rpc-error {\"code\":-32603,\"domainCode\":\"not_implemented\"}]"
        );
    }

    #[test]
    fn should_carry_the_retryable_verdict_in_the_message_suffix_when_the_data_has_one() {
        // Given an engine RPC error whose data payload carries the
        // retryability verdict (the `domain_error_data` shape — e.g. the
        // retryable context.overflow)
        let error = EngineError::Rpc {
            code: -32603,
            message: "context overflow".to_string(),
            data: Some(json!({
                "domainCode": "context.overflow",
                "retryable": true,
            })),
        };

        // When it crosses the NAPI boundary
        let napi_error = engine_error_to_napi(error);

        // Then the structured suffix carries the verdict alongside the code
        // and domain code, so the TS decoder branches on the same field the
        // wire surfaces carry
        assert_eq!(
            napi_error.reason,
            "context overflow \
             [mirri-rpc-error {\"code\":-32603,\"domainCode\":\"context.overflow\",\"retryable\":true}]"
        );
    }

    #[test]
    fn should_carry_the_retry_after_hint_in_the_message_suffix_when_a_quota_error_crosses() {
        // Given a quota-exhausted engine error whose data payload carries
        // the provider's Retry-After hint (the P11a F-P8-13 shape)
        let error = EngineError::Rpc {
            code: -32603,
            message: "Provider quota exhausted: HTTP 429: top up".to_string(),
            data: Some(json!({
                "domainCode": "provider.quota_exhausted",
                "retryable": false,
                "retryAfterMs": 3000,
            })),
        };

        // When it crosses the NAPI boundary
        let napi_error = engine_error_to_napi(error);

        // Then the structured suffix carries the hint in milliseconds, so
        // the TS decoder surfaces "retry after N" without re-parsing
        assert_eq!(
            napi_error.reason,
            "Provider quota exhausted: HTTP 429: top up [mirri-rpc-error \
             {\"code\":-32603,\"domainCode\":\"provider.quota_exhausted\",\
             \"retryAfterMs\":3000,\"retryable\":false}]"
        );
    }

    #[test]
    fn should_leave_the_verdict_out_of_the_suffix_when_the_data_has_none() {
        // Given an engine RPC error whose hand-built data payload predates
        // the verdict field (older shapes degrade gracefully)
        let error = EngineError::Rpc {
            code: -32001,
            message: "session not found".to_string(),
            data: Some(json!({ "domainCode": "session.not_found" })),
        };

        // When it crosses the NAPI boundary
        let napi_error = engine_error_to_napi(error);

        // Then the suffix omits the verdict entirely — never guessed
        assert_eq!(
            napi_error.reason,
            "session not found [mirri-rpc-error {\"code\":-32001,\"domainCode\":\"session.not_found\"}]"
        );
    }

    #[test]
    fn should_carry_the_code_without_a_domain_code_when_the_rpc_error_has_no_data() {
        // Given an engine RPC error without a domain code (invalid params)
        let error = EngineError::Rpc {
            code: -32602,
            message: "name is required".to_string(),
            data: None,
        };

        // When it crosses the NAPI boundary
        let napi_error = engine_error_to_napi(error);

        // Then the suffix carries the code and nothing else
        assert_eq!(
            napi_error.reason,
            "name is required [mirri-rpc-error {\"code\":-32602}]"
        );
    }

    #[test]
    fn should_keep_the_reserved_code_for_unknown_methods_when_converting_method_not_found() {
        // Given a method-not-found error (genuinely unknown method)
        let error = EngineError::MethodNotFound("no.such.method".to_string());

        // When it crosses the NAPI boundary
        let napi_error = engine_error_to_napi(error);

        // Then the message keeps the reason and the suffix carries -32601 —
        // distinct from a retired/unimplemented method's -32603 +
        // not_implemented
        assert_eq!(
            napi_error.reason,
            "method not found: no.such.method [mirri-rpc-error {\"code\":-32601}]"
        );
    }

    #[test]
    fn should_map_bridge_level_failures_to_internal_errors_when_converting_other_variants() {
        // Given a bridge-level failure (a panicked dispatch task)
        let error = EngineError::Internal("call_method task panicked: boom".to_string());

        // When it crosses the NAPI boundary
        let napi_error = engine_error_to_napi(error);

        // Then the message keeps the reason and the suffix carries -32603
        assert_eq!(
            napi_error.reason,
            "internal error: call_method task panicked: boom [mirri-rpc-error {\"code\":-32603}]"
        );
    }
}
