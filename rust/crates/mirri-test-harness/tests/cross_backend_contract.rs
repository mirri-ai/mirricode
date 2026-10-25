//! Cross-backend wire-shape contract test.
//!
//! After the M5-B3a thinning the server no longer owns engine-domain
//! handlers: the serve wire dispatches them to the embedded engine through
//! the [`EngineAdapter`](mirri_transport::EngineAdapter) seam. This file
//! re-scoped the M1.2-a 34 BOTH-row checks into TRANSPORT-INTEGRITY checks:
//! the same call is driven through the engine-backed server's shared dispatch
//! (`ServerBackend`, no HTTP bind — the same [`ServerState::dispatch`] the
//! WS/HTTP/IPC transports share) and directly through a second
//! [`EmbeddedServer`], and the RESULT wire shapes are compared. Any drift
//! between the serve path and the direct engine path turns the test red.
//! (The M5-3b `core.echo` drill row joins the BOTH surface — and the
//! comparisons below — as row 35.)
//!
//! The few methods the transport still owns (`btw.start`'s real server-side
//! fork handler) keep their explicit per-side locks; the historic per-side
//! model-catalog divergences (server-empty vs embedded rows) are GONE with
//! the server handlers — both sides are now the same engine, so
//! `model.list` / `model.listProviders` assert plain shape equality like
//! every other both-row.
//!
//! The fact-table surface itself is pinned here too:
//! [`should_serve_exactly_the_fact_table_when_the_surface_is_enumerated`]
//! drives *every* `serves_server` method through the dispatch and asserts
//! that embedded-only and unknown methods answer method-not-found — the
//! registry gate keeps the serve wire exactly the 58 `mirri_proto::rpc`
//! facts.
//!
//! Params drift stays out of scope: params are constructed per backend
//! (camelCase primary + snake_case aliases are accepted on both paths) —
//! only the RESULT shape is compared.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, RwLock};

use serde_json::{Value, json};
use tempfile::TempDir;

use mirri_core::{
    AgentId, AgentProfile, AgentScopeConfig, ProfileManager, SessionId, SessionManager, TaskKind,
    TokenUsage, WorkspaceId, WorkspaceScope, WorkspaceScopeConfig,
};
use mirri_embedded::EmbeddedServer;
use mirri_proto::rpc::{RpcBackend, all_methods, methods_for};
use mirri_server::ServerState;
use mirri_test_harness::{MockLlm, MockLlmFactory, MockResponse};
use mirri_transport::EngineAdapter;

// ===========================================================================
// Wire-shape normalization (pure, engine-independent — unit-tested below)
// ===========================================================================

/// A structural description of a wire response: leaves are reduced to type
/// names, objects to sorted key sets with per-key shapes, arrays to their
/// element shape. Concrete values and array lengths never appear, so a
/// comparison locks the *shape* of the contract, not the data.
///
/// Ported from `packages/node-sdk/test/rust-engine-contract-e2e.test.ts`
/// (serde_json has no `undefined` leaf; a heterogeneous array is the sorted
/// set of its distinct element shapes, so element order and repetition never
/// matter).
#[derive(Debug, Clone, PartialEq)]
enum WireShape {
    /// A scalar leaf: "string" | "number" | "boolean" | "null".
    Leaf(&'static str),
    /// An empty array.
    EmptyArray,
    /// A non-empty array: one element when homogeneous, otherwise the sorted
    /// set of distinct element shapes.
    Array(Vec<WireShape>),
    /// An object: its fields, sorted by key.
    Object(Vec<(String, WireShape)>),
}

/// Reduce a wire response to its structural shape description.
fn normalize(value: &Value) -> WireShape {
    match value {
        Value::Null => WireShape::Leaf("null"),
        Value::Bool(_) => WireShape::Leaf("boolean"),
        Value::Number(_) => WireShape::Leaf("number"),
        Value::String(_) => WireShape::Leaf("string"),
        Value::Array(items) => {
            if items.is_empty() {
                return WireShape::EmptyArray;
            }
            let mut distinct: Vec<WireShape> = Vec::new();
            for item in items {
                let shape = normalize(item);
                if !distinct.contains(&shape) {
                    distinct.push(shape);
                }
            }
            // Heterogeneous arrays: lock the sorted set of distinct element
            // shapes so element order and repetition never matter.
            distinct.sort_by_key(|shape| shape.render());
            WireShape::Array(distinct)
        }
        Value::Object(map) => {
            let mut fields: Vec<(String, WireShape)> = map
                .iter()
                .map(|(key, value)| (key.clone(), normalize(value)))
                .collect();
            fields.sort_by(|a, b| a.0.cmp(&b.0));
            WireShape::Object(fields)
        }
    }
}

impl WireShape {
    /// Canonical rendering used for panic messages and lock comments.
    fn render(&self) -> String {
        match self {
            WireShape::Leaf(kind) => (*kind).to_string(),
            WireShape::EmptyArray => "[]".to_string(),
            WireShape::Array(elements) if elements.len() == 1 => {
                format!("[{}]", elements[0].render())
            }
            WireShape::Array(elements) => format!(
                "[{}]",
                elements
                    .iter()
                    .map(WireShape::render)
                    .collect::<Vec<_>>()
                    .join(" | ")
            ),
            WireShape::Object(fields) => format!(
                "{{{}}}",
                fields
                    .iter()
                    .map(|(key, shape)| format!("{key}: {}", shape.render()))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }
}

/// Build the expected shape of a recorded-divergence lock from a concrete
/// sample JSON value (the same normalization runs on it, so a lock states a
/// shape, never concrete data).
fn shape_of(sample: Value) -> WireShape {
    normalize(&sample)
}

#[test]
fn should_reduce_leaves_to_type_names_when_normalizing_scalar_values() {
    assert_eq!(normalize(&json!("session-42")), WireShape::Leaf("string"));
    assert_eq!(normalize(&json!(7)), WireShape::Leaf("number"));
    assert_eq!(normalize(&json!(7.5)), WireShape::Leaf("number"));
    assert_eq!(normalize(&json!(true)), WireShape::Leaf("boolean"));
    assert_eq!(normalize(&Value::Null), WireShape::Leaf("null"));
}

#[test]
fn should_sort_object_keys_and_flatten_nested_objects_when_normalizing() {
    // Key order never matters, nesting is preserved per key.
    assert_eq!(
        normalize(&json!({"b": 1, "a": {"x": "s", "y": null}})),
        normalize(&json!({"a": {"y": null, "x": "s"}, "b": 1}))
    );
    let shape = normalize(&json!({"b": 1, "a": "x"}));
    let WireShape::Object(fields) = shape else {
        panic!("an object must normalize to an object shape");
    };
    let keys: Vec<&str> = fields.iter().map(|(key, _)| key.as_str()).collect();
    assert_eq!(keys, vec!["a", "b"]);
}

#[test]
fn should_ignore_element_order_and_repetition_when_normalizing_arrays() {
    // Empty arrays collapse to the empty marker...
    assert_eq!(normalize(&json!([])), WireShape::EmptyArray);
    // ...homogeneous arrays to their single element shape...
    assert_eq!(
        normalize(&json!([{"id": "a"}, {"id": "b"}, {"id": "c"}])),
        normalize(&json!([{"id": "z"}]))
    );
    // ...and heterogeneous arrays to the sorted set of distinct shapes,
    // regardless of order and repetition.
    assert_eq!(
        normalize(&json!([1, "a", 2, "b", 3])),
        normalize(&json!(["b", 3, "a"]))
    );
    let shape = normalize(&json!([true, "s", 1]));
    let WireShape::Array(elements) = shape else {
        panic!("a heterogeneous array must normalize to an array shape");
    };
    assert_eq!(elements.len(), 3);
}

// ===========================================================================
// Backend boots
// ===========================================================================

/// The process-wide fake `$HOME`. `mirri-server`'s `session.export` resolves
/// its export root from `$HOME` at call time, so the server's session
/// persistence root is placed under this directory — export then writes only
/// inside throwaway test directories. Every test sets the same value before
/// any handler runs, so concurrent `set_var` calls are benign.
fn shared_home() -> PathBuf {
    static HOME: OnceLock<PathBuf> = OnceLock::new();
    HOME.get_or_init(|| {
        let dir = tempfile::tempdir().expect("create the shared test home");
        let path = dir.keep();
        // SAFETY: this is a test binary; every test in it assigns HOME the
        // same shared path before touching any HOME-reading handler, so all
        // writers store an identical value.
        unsafe { std::env::set_var("HOME", &path) };
        path
    })
    .clone()
}

/// Seed one workspace skill so `skill.list` and `agent.activateSkill` have a
/// real (project-sourced) skill to work with on both backends.
fn seed_skill(workspace_root: &Path, name: &str) {
    let skill_dir = workspace_root.join(".agents").join("skills").join(name);
    std::fs::create_dir_all(&skill_dir).expect("create the skill directory");
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\ndescription: Contract-test skill.\n---\n# Contract-test skill body\n",
    )
    .expect("write the skill's SKILL.md");
}

/// A booted engine-backed server backend: an [`EmbeddedServer`] behind a
/// [`ServerState`], with no HTTP bind — calls go through the same shared
/// [`ServerState::dispatch`] the WS/HTTP/IPC transports use, so the
/// transport registry, the engine fall-through and the registry gate are
/// all exercised.
struct ServerBackend {
    state: ServerState,
    manager: Arc<SessionManager>,
    mock: Arc<MockLlm>,
    config_path: PathBuf,
    /// The in-memory config snapshot the engine was booted with — the same
    /// value `config.get` serves, used to derive its expected lock.
    initial_config: mirri_core::ServerConfig,
    _workspace: TempDir,
}

/// Boot an engine-backed server backend at a fresh workspace tempdir; when
/// `skill` names a skill, it is seeded into the workspace root BEFORE the
/// workspace scope discovers skills (discovery runs at workspace
/// construction).
async fn boot_server(skill: Option<&str>) -> ServerBackend {
    let workspace = tempfile::tempdir().expect("create the server workspace tempdir");
    let root = workspace.path().to_path_buf();
    boot_server_at(&root, skill, workspace).await
}

/// Boot an engine-backed server backend at an explicit workspace root.
///
/// `keepalive` is the tempdir the returned backend stores: when the root is
/// this backend's own (the [`boot_server`] path) it IS the workspace root's
/// tempdir; when the root is shared with another backend (the cold-restore
/// half of the wire-replay equivalence invariant), the caller's tempdir
/// keeps the shared root alive and the field merely satisfies the struct.
///
/// The cold-restore backend MUST boot at the live backend's root: the
/// session persistence bucket key derives from the workspace root (see
/// [`SessionManager::with_persistence_dir`]), so the second backend can only
/// resolve the session directory the live backend wrote when both roots
/// agree.
async fn boot_server_at(
    workspace_root: &Path,
    skill: Option<&str>,
    keepalive: TempDir,
) -> ServerBackend {
    let home = shared_home();
    if let Some(skill) = skill {
        seed_skill(workspace_root, skill);
    }
    let workspace_scope = Arc::new(WorkspaceScope::new(
        WorkspaceId::new("server-contract"),
        workspace_root.to_path_buf(),
        WorkspaceScopeConfig::default(),
    ));
    let manager = Arc::new(SessionManager::with_persistence_dir(
        workspace_scope,
        home.join(".mirri-code").join("sessions"),
        std::sync::Arc::new(mirri_os::LocalFs::new()),
    ));
    let mock = Arc::new(MockLlm::new());
    let factory = Arc::new(MockLlmFactory::new(Arc::clone(&mock)));
    let profile_manager = Arc::new(ProfileManager::new());
    profile_manager
        .register(AgentProfile {
            name: "coder".to_string(),
            model: "mock-llm".to_string(),
            ..Default::default()
        })
        .unwrap();
    // The same-shaped config.toml as the embedded boot (anthropic provider
    // + contract-model alias), placed OUTSIDE the shared test home —
    // `config.set` mutates the file and `shared_home()` is shared across
    // the whole test binary, so it lives in this backend's own keepalive
    // tempdir instead (never in the possibly-shared workspace root). The
    // in-memory snapshot is the constructed config itself (not a disk
    // reload), mirroring the embedded boot so both sides' `config.get`
    // wires carry the same fields.
    let config_path = keepalive.path().join("config.toml");
    let mut config = mirri_core::ServerConfig::default();
    let mut providers = std::collections::HashMap::new();
    providers.insert(
        "anthropic".to_string(),
        mirri_core::config::ProviderEntry {
            r#type: Some("anthropic".to_string()),
            api_key: Some("contract-test-key".to_string()),
            ..Default::default()
        },
    );
    let mut models = std::collections::HashMap::new();
    models.insert(
        "contract-model".to_string(),
        mirri_core::config::ModelAlias {
            provider: "anthropic".to_string(),
            model: "contract-model-1".to_string(),
            // No max_context_size on purpose: the alias resolves (create /
            // model.list membership) while the context window stays unknown,
            // so the manual-compaction segment keeps the pre-T1 budget-1
            // semantics its two-exchange histories rely on (a known 128k
            // window would make beginCompaction answer false — "nothing to
            // compact" — on both backends).
            ..Default::default()
        },
    );
    config.providers = Some(providers);
    config.models = Some(models);
    // The config snapshot's default_model (spec 11 R4): the server-side
    // session.create must resolve the configured alias exactly like the
    // embedded boot — the same contract-suite-wide T1 seed as below.
    config.default_model = Some("contract-model".to_string());
    config
        .save_to(&config_path)
        .expect("write the server default config.toml");
    // The engine behind the serve state mirrors the embedded backend's
    // construction (mock factory + coder profile + config), so the two
    // sides' sessions are comparable.
    let engine: Arc<dyn EngineAdapter> = Arc::new(
        EmbeddedServer::with_providers(
            Arc::clone(&manager),
            Some(factory as Arc<dyn mirri_embedded::LlmFactory>),
            Some(profile_manager),
            None,
            None,
            Arc::new(RwLock::new(config.clone())),
            Some(config_path.clone()),
        )
        // The models.dev upstream stays disabled so the contract suite
        // never touches the network; the home keeps resolving through the
        // shared test `$HOME` (the ambient-home semantics session.export
        // and the profile writes already rely on).
        .with_models_dev_url(None),
    );
    let state = ServerState::with_engine(
        engine,
        Arc::clone(&manager),
        "0.1.0-contract".to_string(),
        None,
    )
    .with_config(
        Arc::new(RwLock::new(config.clone())),
        Some(config_path.clone()),
    );
    ServerBackend {
        state,
        manager,
        mock,
        config_path,
        initial_config: config,
        _workspace: keepalive,
    }
}

/// A booted mirri-embedded backend, mirroring the NAPI mock-engine
/// construction: a session manager rooted at a tempdir, a scripted mock LLM
/// factory, a "coder" agent profile, and an injected Mirri home (so exports
/// and config writes stay inside the tempdir).
struct EmbeddedBackend {
    engine: Arc<EmbeddedServer>,
    manager: Arc<SessionManager>,
    mock: Arc<MockLlm>,
    config_path: PathBuf,
    /// The in-memory config snapshot the engine was booted with — the same
    /// value `config.get` serves, used to derive its expected lock.
    initial_config: mirri_core::ServerConfig,
    _home: TempDir,
}

/// Boot an embedded backend; when `skill` names a skill, it is seeded into the
/// workspace root BEFORE the workspace scope discovers skills.
fn boot_embedded(skill: Option<&str>) -> EmbeddedBackend {
    // Ensure the ambient HOME is already the shared test home: user-level
    // skill discovery (`$HOME/.agents/skills`) must see the same (empty)
    // directory on both backends regardless of test scheduling order.
    let _ = shared_home();
    let home = tempfile::tempdir().expect("create the embedded home tempdir");
    if let Some(skill) = skill {
        seed_skill(home.path(), skill);
    }
    let workspace = Arc::new(WorkspaceScope::new(
        WorkspaceId::new("embedded-contract"),
        home.path().to_path_buf(),
        WorkspaceScopeConfig::default(),
    ));
    let manager = Arc::new(SessionManager::with_persistence_dir(
        workspace,
        home.path().join("sessions"),
        std::sync::Arc::new(mirri_os::LocalFs::new()),
    ));
    let mock = Arc::new(MockLlm::new());
    let factory = Arc::new(MockLlmFactory::new(Arc::clone(&mock)));
    let profile_manager = Arc::new(ProfileManager::new());
    profile_manager
        .register(AgentProfile {
            name: "coder".to_string(),
            model: "mock-llm".to_string(),
            ..Default::default()
        })
        .unwrap();
    // A real (default + one provider and model alias) config.toml inside the
    // tempdir, so `config.set` has a file to merge into, `model.list`'s
    // embedded wrap locks its element shape, and `model.listProviders`'s
    // embedded wrap locks its element shape too — not just empty catalogs.
    let config_path = home.path().join("config.toml");
    let mut config = mirri_core::ServerConfig::default();
    let mut providers = std::collections::HashMap::new();
    providers.insert(
        "anthropic".to_string(),
        mirri_core::config::ProviderEntry {
            r#type: Some("anthropic".to_string()),
            api_key: Some("contract-test-key".to_string()),
            ..Default::default()
        },
    );
    let mut models = std::collections::HashMap::new();
    models.insert(
        "contract-model".to_string(),
        mirri_core::config::ModelAlias {
            provider: "anthropic".to_string(),
            model: "contract-model-1".to_string(),
            // No max_context_size on purpose: the alias resolves (create /
            // model.list membership) while the context window stays unknown,
            // so the manual-compaction segment keeps the pre-T1 budget-1
            // semantics its two-exchange histories rely on (a known 128k
            // window would make beginCompaction answer false — "nothing to
            // compact" — on both backends).
            ..Default::default()
        },
    );
    config.providers = Some(providers);
    config.models = Some(models);
    // The config snapshot's default_model (spec 11 R4): a session created
    // without an explicit modelId must resolve the configured alias — the
    // T1 fail-loud kernel rejects a no-default config, so the contract
    // suite seeds the alias it registered above as the default too.
    config.default_model = Some("contract-model".to_string());
    config
        .save_to(&config_path)
        .expect("write the embedded default config.toml");
    let engine = Arc::new(
        EmbeddedServer::with_providers(
            Arc::clone(&manager),
            Some(factory as Arc<dyn mirri_embedded::LlmFactory>),
            Some(profile_manager),
            None,
            None,
            Arc::new(RwLock::new(config.clone())),
            Some(config_path.clone()),
        )
        .with_home_dir(Some(home.path().to_path_buf()))
        // The models.dev upstream stays disabled so the contract suite
        // never touches the network.
        .with_models_dev_url(None),
    );
    EmbeddedBackend {
        engine,
        manager,
        mock,
        config_path,
        initial_config: config,
        _home: home,
    }
}

/// Provision an extra agent with the default configuration on the given
/// session under a chosen id. Both backends' `session.create` now provisions
/// the "main" agent eagerly, so this helper is only needed to add a NAMED
/// agent besides it (e.g. the embedded side's "plain" agent, which has a
/// default config without the subagent-host wiring of its "main" agent).
async fn provision_default_agent(manager: &SessionManager, session_id: &str, agent_id: &str) {
    let session = manager
        .get_session(&SessionId::new(session_id))
        .await
        .expect("the session to provision exists");
    session
        .create_agent(AgentId::new(agent_id), AgentScopeConfig::default())
        .await
        .expect("create the default agent");
}

/// Create a session on the server backend and return its own id.
async fn create_session_on_server(backend: &ServerBackend, title: &str) -> String {
    let result = call_server(backend, "session.create", json!({ "title": title }))
        .await
        .expect("server session.create succeeds");
    result["id"].as_str().expect("a session id").to_string()
}

// ===========================================================================
// Call + assertion helpers
// ===========================================================================

/// Drive one method through the engine-backed server's shared dispatch —
/// the same [`ServerState::dispatch`] the WS/HTTP/IPC transports share, so
/// the transport registry, the engine fall-through and the registry gate
/// are all exercised.
async fn call_server(
    backend: &ServerBackend,
    method: &str,
    params: Value,
) -> Result<Value, String> {
    backend
        .state
        .dispatch(method, params)
        .await
        .map_err(|error| error.to_string())
}

/// Drive one method on the embedded engine.
async fn call_embedded(
    backend: &EmbeddedBackend,
    method: &str,
    params: Value,
) -> Result<Value, String> {
    EngineAdapter::call_method(&*backend.engine, method, params)
        .await
        .map_err(|error| error.to_string())
}

/// Assert that both backends' results have the SAME wire shape.
fn assert_same_shape(
    method: &str,
    server: &Result<Value, String>,
    embedded: &Result<Value, String>,
) {
    let server = server
        .as_ref()
        .unwrap_or_else(|error| panic!("{method}: the server errored: {error}"));
    let embedded = embedded
        .as_ref()
        .unwrap_or_else(|error| panic!("{method}: the embedded engine errored: {error}"));
    let server_shape = normalize(server);
    let embedded_shape = normalize(embedded);
    assert_eq!(
        server_shape,
        embedded_shape,
        "{method}: wire shapes drifted between the two rust backends —\n  server:   {}\n  embedded: {}",
        server_shape.render(),
        embedded_shape.render()
    );
}

/// Lock one side's concrete current shape for a recorded divergence (or a
/// REPORT divergence): any further drift on that side turns CI red.
fn assert_recorded_shape(
    method: &str,
    side: &str,
    result: &Result<Value, String>,
    expected: &WireShape,
) {
    let value = result
        .as_ref()
        .unwrap_or_else(|error| panic!("{method} ({side}) errored: {error}"));
    assert_eq!(
        &normalize(value),
        expected,
        "{method} ({side}): the recorded wire shape drifted —\n  actual:   {}\n  expected: {}",
        normalize(value).render(),
        expected.render()
    );
}

// ===========================================================================
// Session domain (15 methods)
// ===========================================================================

#[tokio::test]
async fn should_match_session_domain_wire_shapes_across_backends_when_sessions_are_driven_on_both()
{
    let server = boot_server(None).await;
    let embedded = boot_embedded(None);

    // --- session.create / session.list -----------------------------------
    // M3: session.create / session.list are on the recorded divergence list
    // for RICHER semantic rows; their result shapes still compare equal today,
    // so equality is asserted (the recorded rule: equal shape → Equal).
    let srv_create = call_server(
        &server,
        "session.create",
        json!({ "title": "contract-session" }),
    )
    .await;
    let emb_create = call_embedded(
        &embedded,
        "session.create",
        json!({ "title": "contract-session" }),
    )
    .await;
    assert_same_shape("session.create", &srv_create, &emb_create);
    let srv_id = srv_create.as_ref().expect("server session.create succeeds")["id"]
        .as_str()
        .expect("a session id")
        .to_string();
    let emb_id = emb_create
        .as_ref()
        .expect("embedded session.create succeeds")["id"]
        .as_str()
        .expect("a session id")
        .to_string();

    let srv_list = call_server(&server, "session.list", json!({})).await;
    let emb_list = call_embedded(&embedded, "session.list", json!({})).await;
    assert_same_shape("session.list", &srv_list, &emb_list);

    // Materialize the first pair too (one turn each): `session.export`
    // resolves the session DIRECTORY on disk, and a session that never
    // conversed has none (W5 lazy creation — the dir appears with the first
    // turn's wire log).
    server.mock.push_text_response("contract ack");
    call_server(
        &server,
        "message.send",
        json!({ "sessionId": srv_id, "agentId": "main", "text": "materialize" }),
    )
    .await
    .expect("the server turn drives");
    embedded.mock.push_text_response("contract ack");
    call_embedded(
        &embedded,
        "message.send",
        json!({ "session_id": emb_id, "agent_id": "main", "text": "materialize" }),
    )
    .await
    .expect("the embedded turn drives");

    // --- session.close + session.resume ----------------------------------
    let srv_resumable = create_session_on_server(&server, "resumable").await;
    let emb_resumable = create_session_on_embedded(&embedded, "resumable").await;
    // Materialize the resumable pair first (one turn each): the wire log is
    // the resume source of truth, and a session that never conversed has
    // none (W5 lazy creation) — resume would honestly fail on a session
    // that was created and closed without a single turn.
    server.mock.push_text_response("resumable ack");
    call_server(
        &server,
        "message.send",
        json!({ "sessionId": srv_resumable, "agentId": "main", "text": "materialize" }),
    )
    .await
    .expect("the server turn drives");
    embedded.mock.push_text_response("resumable ack");
    call_embedded(
        &embedded,
        "message.send",
        json!({ "session_id": emb_resumable, "agent_id": "main", "text": "materialize" }),
    )
    .await
    .expect("the embedded turn drives");
    let srv_close = call_server(
        &server,
        "session.close",
        json!({ "sessionId": srv_resumable }),
    )
    .await;
    let emb_close = call_embedded(
        &embedded,
        "session.close",
        json!({ "session_id": emb_resumable }),
    )
    .await;
    assert_same_shape("session.close", &srv_close, &emb_close);

    let srv_resume = call_server(
        &server,
        "session.resume",
        json!({ "sessionId": srv_resumable }),
    )
    .await;
    let emb_resume = call_embedded(
        &embedded,
        "session.resume",
        json!({ "session_id": emb_resumable }),
    )
    .await;
    // P7e: the embedded resume answers with the v1 `ResumedSessionSummary`
    // contract (summary scalars + per-agent replay snapshots — the TS SDK /
    // TUI resume shape); the serve wire inherits it through the engine
    // fall-through. One explicit pin against the v1 contract so the two rust
    // backends drifting together away from v1 still fails.
    assert_same_shape("session.resume", &srv_resume, &emb_resume);
    // P7e: both backends answer with the v1 `ResumedSessionSummary`
    // contract (summary scalars + per-agent replay snapshots — the TS SDK /
    // TUI resume shape); the serve wire inherits it through the engine
    // fall-through. The exact v1 shape is key-set-locked in the embedded
    // suite (`should_resume_closed_session_when_session_resume_called`)
    // and deep-asserted in the SDK's rust-engine e2e — the recursive pin
    // here would be data-dependent (`usage.byModel` keys are model names).

    // --- session.reload / session.rename / session.updateMeta -------------
    let srv_reload = call_server(&server, "session.reload", json!({ "sessionId": srv_id })).await;
    let emb_reload =
        call_embedded(&embedded, "session.reload", json!({ "session_id": emb_id })).await;
    // P7e (same contract as session.resume): the embedded reload serves the
    // v1 resumed summary on the live path too — the SDK's `reloadSession`
    // feeds the result straight back into the session's resume state.
    assert_same_shape("session.reload", &srv_reload, &emb_reload);

    let srv_rename = call_server(
        &server,
        "session.rename",
        json!({ "sessionId": srv_id, "title": "renamed" }),
    )
    .await;
    let emb_rename = call_embedded(
        &embedded,
        "session.rename",
        json!({ "session_id": emb_id, "title": "renamed" }),
    )
    .await;
    assert_same_shape("session.rename", &srv_rename, &emb_rename);

    let srv_update_meta = call_server(
        &server,
        "session.updateMeta",
        json!({ "sessionId": srv_id, "tags": ["contract"] }),
    )
    .await;
    let emb_update_meta = call_embedded(
        &embedded,
        "session.updateMeta",
        json!({ "session_id": emb_id, "tags": ["contract"] }),
    )
    .await;
    assert_same_shape("session.updateMeta", &srv_update_meta, &emb_update_meta);

    // --- session.export ----------------------------------------------------
    // Both backends persist each created session under their sessions root
    // in the bucketed TS layout (`<root>/<workspace_key>/<session_id>/`
    // with `state.json`), so the exporter resolves the real session dir and
    // its manifest carries the persisted title / activity timestamps. (An
    // earlier flat `<root>/<session_id>/` prefab would SHADOW the real
    // bucketed dir and export an empty folder — the v2 pin below guards
    // the manifest metadata against exactly that drift.)
    let srv_export = call_server(&server, "session.export", json!({ "sessionId": srv_id })).await;
    let emb_export =
        call_embedded(&embedded, "session.export", json!({ "session_id": emb_id })).await;
    // M3-c b5: both backends are aligned to the authoritative
    // v2:ExportSessionResult {zipPath, entries, sessionDir, manifest} —
    // `entryCount` was dropped and the manifest serialises camelCase with
    // `nodejsVersion` (never `runtimeVersion`). Both sides additionally
    // carry per-backend key-set locks in their own unit tests
    // (mirri-server rpc.rs / mirri-embedded tests.rs) guarding against
    // one-sided drift.
    assert_same_shape("session.export", &srv_export, &emb_export);
    // Keep one explicit pin against the v2 contract on the server side so
    // the two rust backends drifting together away from v2 still fails.
    assert_recorded_shape(
        "session.export",
        "server (v2 pin)",
        &srv_export,
        &shape_of(json!({
            "zipPath": "/tmp/contract.zip",
            "entries": ["state.json"],
            "sessionDir": "/tmp/contract-session",
            "manifest": {
                "sessionId": "sess-contract",
                "exportedAt": "2024-01-01T00:00:00Z",
                "kimiCodeVersion": "1.0.0",
                "wireProtocolVersion": "1.5",
                "os": "macos",
                "nodejsVersion": "rustc 1.0.0",
                "sessionFirstActivity": "2024-01-01T00:00:00Z",
                "sessionLastActivity": "2024-01-01T00:30:00Z",
                "title": "contract-session",
                "workspaceDir": "/tmp/contract-ws",
                "sessionLogPath": "/tmp/contract-ws/sessions/sess-contract",
                "globalLogPath": null,
                "desktopLogPath": null,
                "webLogPath": null,
                "desktopVersion": null,
                "installSource": null,
                "shellEnv": null,
            },
        })),
    );

    // --- session.fork / session.clearContext / session.undo ---------------
    // Both backends' session.create eagerly provisions the same "main" agent
    // now, so fork / clearContext / undo already address comparable state.
    let srv_fork = call_server(
        &server,
        "session.fork",
        json!({ "sessionId": srv_id, "agentId": "main", "title": "forked" }),
    )
    .await;
    let emb_fork = call_embedded(
        &embedded,
        "session.fork",
        json!({ "session_id": emb_id, "agentId": "main", "title": "forked" }),
    )
    .await;
    assert_same_shape("session.fork", &srv_fork, &emb_fork);

    let srv_clear = call_server(
        &server,
        "session.clearContext",
        json!({ "sessionId": srv_id, "agentId": "main" }),
    )
    .await;
    let emb_clear = call_embedded(
        &embedded,
        "session.clearContext",
        json!({ "session_id": emb_id, "agent_id": "main" }),
    )
    .await;
    assert_same_shape("session.clearContext", &srv_clear, &emb_clear);

    // A turn first, so session.undo has a user message to undo.
    server.mock.push_text_response("hello back");
    call_server(
        &server,
        "message.send",
        json!({ "sessionId": srv_id, "agentId": "main", "text": "hello" }),
    )
    .await
    .expect("the server engine turn succeeds");
    embedded.mock.push_text_response("hello back");
    call_embedded(
        &embedded,
        "message.send",
        json!({ "session_id": emb_id, "agent_id": "main", "text": "hello" }),
    )
    .await
    .expect("the embedded engine turn succeeds");

    let srv_undo = call_server(
        &server,
        "session.undo",
        json!({ "sessionId": srv_id, "agentId": "main", "count": 1 }),
    )
    .await;
    let emb_undo = call_embedded(
        &embedded,
        "session.undo",
        json!({ "session_id": emb_id, "agent_id": "main", "count": 1 }),
    )
    .await;
    assert_same_shape("session.undo", &srv_undo, &emb_undo);

    // --- session.warnings ---------------------------------------------------
    // Both backends return the bare `SessionWarning[]` array today and no
    // warning source exists yet, so the element shape is vacuous — the
    // assertion still pins the wrapper (the 855f57ef class of regression).
    let srv_warnings =
        call_server(&server, "session.warnings", json!({ "sessionId": srv_id })).await;
    let emb_warnings = call_embedded(
        &embedded,
        "session.warnings",
        json!({ "session_id": emb_id }),
    )
    .await;
    assert_same_shape("session.warnings", &srv_warnings, &emb_warnings);

    // --- session.archive ----------------------------------------------------
    // Both sides return the klient `noResult` representation (JSON `null` —
    // the klient contract models the method as void, and JSON has no
    // `undefined`), so shape equality is asserted. Aligned in M3-c; the old
    // divergence (server `{archived: true}` vs embedded `{ok: true}`) is
    // gone.
    let srv_archivable = create_session_on_server(&server, "archivable").await;
    let emb_archivable = create_session_on_embedded(&embedded, "archivable").await;
    // Materialize the archivable pair (one turn each): the archive path
    // snapshots the session's state.json, which a never-conversed session
    // has none of (W5 lazy creation).
    server.mock.push_text_response("archivable ack");
    call_server(
        &server,
        "message.send",
        json!({ "sessionId": srv_archivable, "agentId": "main", "text": "materialize" }),
    )
    .await
    .expect("the server turn drives");
    embedded.mock.push_text_response("archivable ack");
    call_embedded(
        &embedded,
        "message.send",
        json!({ "session_id": emb_archivable, "agent_id": "main", "text": "materialize" }),
    )
    .await
    .expect("the embedded turn drives");
    let srv_archive = call_server(
        &server,
        "session.archive",
        json!({ "sessionId": srv_archivable }),
    )
    .await;
    let emb_archive = call_embedded(
        &embedded,
        "session.archive",
        json!({ "session_id": emb_archivable }),
    )
    .await;
    assert_same_shape("session.archive", &srv_archive, &emb_archive);
    // The shared shape assertion above proves both sides are `null`, but it
    // is vacuous against a pair of non-null shapes — pin the concrete
    // klient noResult spelling on both sides explicitly.
    for (side, result) in [("server", &srv_archive), ("embedded", &emb_archive)] {
        assert_eq!(
            result,
            &Ok(Value::Null),
            "session.archive ({side}): the klient noResult wire spelling is JSON null"
        );
    }

    // --- session.restore ----------------------------------------------------
    // The B1 EMB backfill: both engines model archive as close-with-files-
    // kept, so restore is the resume path on both. Both sides return the
    // bare session_view (the SRV transport handler's shape — the klient
    // maybe(handleWire) drift is parked with the session.get shape
    // adjudication), asserted shape-equal across backends.
    let srv_restorable = create_session_on_server(&server, "restorable").await;
    let emb_restorable = create_session_on_embedded(&embedded, "restorable").await;
    // Materialize the restorable pair (one turn each): restore is the
    // resume path on archived sessions, and the wire log it replays exists
    // only after the first turn (W5 lazy creation).
    server.mock.push_text_response("restorable ack");
    call_server(
        &server,
        "message.send",
        json!({ "sessionId": srv_restorable, "agentId": "main", "text": "materialize" }),
    )
    .await
    .expect("the server turn drives");
    embedded.mock.push_text_response("restorable ack");
    call_embedded(
        &embedded,
        "message.send",
        json!({ "session_id": emb_restorable, "agent_id": "main", "text": "materialize" }),
    )
    .await
    .expect("the embedded turn drives");
    call_server(
        &server,
        "session.archive",
        json!({ "sessionId": srv_restorable }),
    )
    .await
    .expect("server archive must succeed");
    call_embedded(
        &embedded,
        "session.archive",
        json!({ "session_id": emb_restorable }),
    )
    .await
    .expect("embedded archive must succeed");
    let srv_restore = call_server(
        &server,
        "session.restore",
        json!({ "sessionId": srv_restorable }),
    )
    .await;
    let emb_restore = call_embedded(
        &embedded,
        "session.restore",
        json!({ "session_id": emb_restorable }),
    )
    .await;
    // P7e recorded divergence: the embedded restore answers with the v1
    // `ResumedSessionSummary` contract (the same body `session.resume`
    // serves — restore is the resume path for archived sessions), while
    // the serve transport keeps its local session-view handler. The serve
    // side is pinned to its concrete view shape; the embedded side's v1
    // shape is key-set-locked in the embedded suite
    // (`should_return_live_session_view_when_session_restore_called_after_archive`).
    assert_recorded_shape(
        "session.restore",
        "server (session view pin)",
        &srv_restore,
        &shape_of(json!({
            "id": "", "title": "", "createdAt": "", "updatedAt": "",
            "agentCount": 0, "tags": [], "status": "", "busy": false,
            "model": "",
            "usage": {
                "inputTokens": 0, "outputTokens": 0,
                "cacheReadTokens": 0, "cacheCreationTokens": 0,
                "firstCall": {
                    "inputTokens": 0, "outputTokens": 0,
                    "cacheReadTokens": 0, "cacheCreationTokens": 0,
                },
            },
            "messageCount": 0, "lastSeq": 0,
            "cwd": "",
            "lastUsedAt": "",
        })),
    );
    for (side, result, session_id) in [
        ("server", &srv_restore, srv_restorable.as_str()),
        ("embedded", &emb_restore, emb_restorable.as_str()),
    ] {
        let view = result
            .as_ref()
            .expect("session.restore must succeed after archive");
        assert_eq!(
            view["id"].as_str(),
            Some(session_id),
            "session.restore ({side}): the restored session id round-trips"
        );
        let main_agent_recreated = if side == "server" {
            view["agentCount"] == 1
        } else {
            view["agents"]["main"]["type"] == "main"
        };
        assert!(
            main_agent_recreated,
            "session.restore ({side}): the main agent is recreated"
        );
    }

    // --- session.beginCompaction / session.cancelCompaction ------------------
    // M3-c: both backends now run a real manual compaction (the server
    // mirrors the embedded engine's TurnFlow::request_manual_compaction +
    // fire-and-forget apply task), and the wire converged to the TSV
    // authority: begin answers the engine-service `begin(): boolean`
    // contract as a bare boolean, cancel is `v2:void` null.
    // `begin()` answers `false` — not an error — when the history has no
    // legal retention-tail cut (US2.6/SDR-003: an empty or single
    // [user, assistant] exchange is one indivisible unit), so both
    // compactable sessions are seeded with two exchanges first, mirroring
    // the embedded suite's begin-compaction happy path.
    let srv_compactable = create_session_on_server(&server, "compactable").await;
    let emb_compactable = create_session_on_embedded(&embedded, "compactable").await;
    server.mock.push_text_response("compactable one");
    server.mock.push_text_response("compactable two");
    call_server(
        &server,
        "message.send",
        json!({ "sessionId": srv_compactable, "agentId": "main", "text": "tell me a story" }),
    )
    .await
    .expect("the server compactable turn succeeds");
    call_server(
        &server,
        "message.send",
        json!({ "sessionId": srv_compactable, "agentId": "main", "text": "tell me another" }),
    )
    .await
    .expect("the server compactable turn succeeds");
    embedded.mock.push_text_response("compactable one");
    embedded.mock.push_text_response("compactable two");
    call_embedded(
        &embedded,
        "message.send",
        json!({ "session_id": emb_compactable, "agent_id": "main", "text": "tell me a story" }),
    )
    .await
    .expect("the embedded compactable turn succeeds");
    call_embedded(
        &embedded,
        "message.send",
        json!({ "session_id": emb_compactable, "agent_id": "main", "text": "tell me another" }),
    )
    .await
    .expect("the embedded compactable turn succeeds");
    server.mock.push_text_response("compaction summary");
    embedded.mock.push_text_response("compaction summary");
    let srv_begin = call_server(
        &server,
        "session.beginCompaction",
        json!({ "sessionId": srv_compactable, "agentId": "main" }),
    )
    .await;
    let emb_begin = call_embedded(
        &embedded,
        "session.beginCompaction",
        // Both spellings parse since the M3-c params alignment (snake primary,
        // camelCase alias — the sibling-handler idiom); camelCase stays the
        // canonical working spelling the node-sdk client sends.
        json!({ "session_id": emb_compactable, "agent_id": "main" }),
    )
    .await;
    assert_same_shape("session.beginCompaction", &srv_begin, &emb_begin);
    // Shape equality on a scalar leaf is weak — pin the concrete bare
    // boolean spelling on both sides explicitly.
    for (side, result) in [("server", &srv_begin), ("embedded", &emb_begin)] {
        assert_eq!(
            result,
            &Ok(Value::Bool(true)),
            "session.beginCompaction ({side}): bare boolean true per engine-service begin(): boolean"
        );
    }

    let srv_cancel_compaction = call_server(
        &server,
        "session.cancelCompaction",
        json!({ "sessionId": srv_compactable, "agentId": "main" }),
    )
    .await;
    let emb_cancel_compaction = call_embedded(
        &embedded,
        "session.cancelCompaction",
        json!({ "session_id": emb_compactable, "agent_id": "main" }),
    )
    .await;
    // M3-c: cancel is fire-and-forget on both sides — v2:void null, the
    // cancelled boolean is no longer carried.
    assert_same_shape(
        "session.cancelCompaction",
        &srv_cancel_compaction,
        &emb_cancel_compaction,
    );
    for (side, result) in [
        ("server", &srv_cancel_compaction),
        ("embedded", &emb_cancel_compaction),
    ] {
        assert_eq!(
            result,
            &Ok(Value::Null),
            "session.cancelCompaction ({side}): v2:void null — the cancelled boolean is no longer carried"
        );
    }
}

/// Create a session on the embedded backend and return its own id.
async fn create_session_on_embedded(backend: &EmbeddedBackend, title: &str) -> String {
    let result = call_embedded(backend, "session.create", json!({ "title": title }))
        .await
        .expect("embedded session.create succeeds");
    result["id"].as_str().expect("a session id").to_string()
}

// ===========================================================================
// Message domain (3 methods)
// ===========================================================================

#[tokio::test]
async fn should_match_message_domain_wire_shapes_across_backends_when_a_turn_is_driven_on_both() {
    let server = boot_server(None).await;
    let embedded = boot_embedded(None);

    let srv_id = create_session_on_server(&server, "message-contract").await;
    let emb_id = create_session_on_embedded(&embedded, "message-contract").await;

    // --- message.send (wait mode) -------------------------------------------
    // M3-c: the wait/fire-branch key-convention drift is closed — both
    // branches and both backends spell the envelope snake_case (the turn
    // view inside wait results stays camelCase), so equality is asserted.
    server.mock.push_text_response("hello back");
    let srv_send = call_server(
        &server,
        "message.send",
        json!({ "sessionId": srv_id, "agentId": "main", "text": "hello" }),
    )
    .await;
    embedded.mock.push_text_response("hello back");
    let emb_send = call_embedded(
        &embedded,
        "message.send",
        json!({ "session_id": emb_id, "agent_id": "main", "text": "hello" }),
    )
    .await;
    assert_same_shape("message.send", &srv_send, &emb_send);

    // --- message.send (fire mode) -------------------------------------------
    // M3-c: the fire envelope converged to the same snake_case spelling as
    // the wait envelope ({ok, session_id, agent_id, turn_id}), matching v2's
    // snake `turn_id`. The background turns consume their scripted mock
    // responses and end on their own; nobody needs to await their events.
    server.mock.push_text_response("hello async");
    let srv_send_fire = call_server(
        &server,
        "message.send",
        json!({
            "sessionId": srv_id,
            "agentId": "main",
            "text": "hello fire",
            "mode": "fire",
        }),
    )
    .await;
    embedded.mock.push_text_response("hello async");
    let emb_send_fire = call_embedded(
        &embedded,
        "message.send",
        json!({
            "session_id": emb_id,
            "agent_id": "main",
            "text": "hello fire",
            "mode": "fire",
        }),
    )
    .await;
    assert_same_shape("message.send (fire)", &srv_send_fire, &emb_send_fire);

    // --- message.abort (idempotent when nothing is in flight) ----------------
    let srv_abort = call_server(&server, "message.abort", json!({ "sessionId": srv_id })).await;
    let emb_abort =
        call_embedded(&embedded, "message.abort", json!({ "session_id": emb_id })).await;
    assert_same_shape("message.abort", &srv_abort, &emb_abort);

    // --- message.steer ---------------------------------------------------------
    let srv_steer = call_server(
        &server,
        "message.steer",
        json!({ "sessionId": srv_id, "agentId": "main", "text": "a steer" }),
    )
    .await;
    let emb_steer = call_embedded(
        &embedded,
        "message.steer",
        json!({ "session_id": emb_id, "agent_id": "main", "text": "a steer" }),
    )
    .await;
    assert_same_shape("message.steer", &srv_steer, &emb_steer);
}

// ===========================================================================
// Agent profile CRUD domain (5 methods, P6)
// ===========================================================================

/// The P6 profile CRUD vertical: every `agent.*Profile` method answers the
/// same wire shape on both backends across the full create → read →
/// update → delete cycle (the TS `/agents` REST contract's method faces).
#[tokio::test]
async fn should_match_profile_crud_wire_shapes_across_backends_when_profiles_are_driven() {
    let server = boot_server(None).await;
    let embedded = boot_embedded(None);

    // --- agent.listProfiles (the builtin + registered baseline) ------------
    let srv_list = call_server(&server, "agent.listProfiles", json!({})).await;
    let emb_list = call_embedded(&embedded, "agent.listProfiles", json!({})).await;
    assert_same_shape("agent.listProfiles", &srv_list, &emb_list);
    // R3-6: the response is the {items, skipped} envelope — the rows under
    // `items`, the discovery pass's skip-and-collect diagnostics under
    // `skipped` as {path, reason} entries.
    // R3-5: the built-in rows carry the builtin source with no file and no
    // suppressed candidates (both backends share the embedded projection —
    // the value assertions pin one side per row, the shape lock above pins
    // the symmetry)
    for (list, side) in [(&srv_list, "server"), (&emb_list, "embedded")] {
        let list = list.as_ref().unwrap();
        let items = list["items"]
            .as_array()
            .unwrap_or_else(|| panic!("({side}) the response is an {{items, skipped}} object"));
        assert!(
            list["skipped"].as_array().is_some(),
            "({side}) skipped rides alongside items"
        );
        let explore = items
            .iter()
            .find(|item| item["name"] == "explore")
            .unwrap_or_else(|| panic!("explore is listed ({side})"));
        assert_eq!(explore["source"], "builtin", "({side})");
        assert_eq!(explore["builtin"], true, "({side})");
        assert!(
            explore.get("file_path").is_none(),
            "({side}) built-ins carry no file"
        );
        assert!(
            explore.get("suppressed_candidates").is_none(),
            "({side}) unshadowed rows carry no suppressed candidates"
        );
    }

    // --- agent.createProfile ----------------------------------------------
    let create = json!({
        "name": "contract-profile",
        "description": "The contract test profile.",
        "default_model": "contract-model-1",
        "tools": ["Read", "Grep"],
        "system_prompt_template": "You uphold the contract.",
        "when_to_use": "When shapes must match.",
    });
    let srv_created = call_server(&server, "agent.createProfile", create.clone()).await;
    let emb_created = call_embedded(&embedded, "agent.createProfile", create.clone()).await;
    assert_same_shape("agent.createProfile", &srv_created, &emb_created);
    // R3-5: the created row is user-sourced with its persisted file, and
    // the fields the request does not carry stay absent (the optional
    // contract — absent, never null)
    for (created, side) in [(&srv_created, "server"), (&emb_created, "embedded")] {
        let created = created.as_ref().unwrap();
        assert_eq!(created["source"], "user", "({side})");
        assert_eq!(created["builtin"], false, "({side})");
        assert_eq!(created["has_override"], false, "({side})");
        assert!(
            created["file_path"].as_str().is_some(),
            "({side}) the persisted file is reported"
        );
        for absent in [
            "disallowed_tools",
            "capabilities_required",
            "subagents",
            "suppressed_candidates",
        ] {
            assert!(
                created.get(absent).is_none(),
                "({side}) an unset field must be absent, not null: {absent}"
            );
        }
    }

    // --- agent.getProfile ---------------------------------------------------
    let get = json!({ "name": "contract-profile" });
    let srv_get = call_server(&server, "agent.getProfile", get.clone()).await;
    let emb_get = call_embedded(&embedded, "agent.getProfile", get.clone()).await;
    assert_same_shape("agent.getProfile", &srv_get, &emb_get);

    // --- agent.updateProfile (partial merge) --------------------------------
    let update = json!({ "name": "contract-profile", "description": "Updated." });
    let srv_updated = call_server(&server, "agent.updateProfile", update.clone()).await;
    let emb_updated = call_embedded(&embedded, "agent.updateProfile", update.clone()).await;
    assert_same_shape("agent.updateProfile", &srv_updated, &emb_updated);

    // --- agent.listProfiles (the created row joined the catalog) ------------
    let srv_list = call_server(&server, "agent.listProfiles", json!({})).await;
    let emb_list = call_embedded(&embedded, "agent.listProfiles", json!({})).await;
    assert_same_shape("agent.listProfiles (after create)", &srv_list, &emb_list);

    // --- agent.deleteProfile ------------------------------------------------
    let delete = json!({ "name": "contract-profile" });
    let srv_deleted = call_server(&server, "agent.deleteProfile", delete.clone()).await;
    let emb_deleted = call_embedded(&embedded, "agent.deleteProfile", delete.clone()).await;
    assert_same_shape("agent.deleteProfile", &srv_deleted, &emb_deleted);

    // --- error shape (the essential-name refusal) ---------------------------
    let srv_error = call_server(&server, "agent.createProfile", json!({ "name": "agent" })).await;
    let emb_error =
        call_embedded(&embedded, "agent.createProfile", json!({ "name": "agent" })).await;
    let srv_error = srv_error.expect_err("the essential name must be refused server-side");
    let emb_error = emb_error.expect_err("the essential name must be refused embedded-side");
    // The TS-exact refusal text (`agents.ts:700`) — the web layer matches
    // messages by string, so both transport paths must carry it verbatim.
    let expected = "Cannot create a profile with the reserved name \"agent\"";
    assert!(
        srv_error.contains(expected) && emb_error.contains(expected),
        "the essential-name refusal matches on both backends: {srv_error} / {emb_error}"
    );
}
// ===========================================================================
// Agent domain (6 methods)
// ===========================================================================

#[tokio::test]
async fn should_match_agent_domain_wire_shapes_across_backends_when_agent_state_is_driven_on_both()
{
    let server = boot_server(Some("contract-skill")).await;
    let embedded = boot_embedded(Some("contract-skill"));

    let srv_id = create_session_on_server(&server, "agent-contract").await;
    let emb_id = create_session_on_embedded(&embedded, "agent-contract").await;

    // --- agent.getContext / agent.getUsage -----------------------------------
    let srv_context = call_server(
        &server,
        "agent.getContext",
        json!({ "sessionId": srv_id, "agentId": "main" }),
    )
    .await;
    let emb_context = call_embedded(
        &embedded,
        "agent.getContext",
        json!({ "session_id": emb_id, "agent_id": "main" }),
    )
    .await;
    assert_same_shape("agent.getContext", &srv_context, &emb_context);

    let srv_usage = call_server(
        &server,
        "agent.getUsage",
        json!({ "sessionId": srv_id, "agentId": "main" }),
    )
    .await;
    let emb_usage = call_embedded(
        &embedded,
        "agent.getUsage",
        json!({ "session_id": emb_id, "agent_id": "main" }),
    )
    .await;
    assert_same_shape("agent.getUsage", &srv_usage, &emb_usage);

    // --- agent.setModel / agent.setThinking / agent.setPermission -------------
    // M3-c batch 2: all three now really apply on BOTH backends and answer
    // on the klient result shapes — setModel echoes `{model, providerName?}`
    // (providerName = the serialized `provider_type`; both sides' default
    // agent configs resolve it to Anthropic, so the cross-backend shape is
    // deterministic), and the two `noResult` methods answer JSON null.
    // Both sides are called with the same params (canonical `thinking`
    // spelling; the `effort` sibling alias is probed below).
    let srv_set_model = call_server(
        &server,
        "agent.setModel",
        json!({ "sessionId": srv_id, "agentId": "main", "model": "contract-model" }),
    )
    .await;
    let emb_set_model = call_embedded(
        &embedded,
        "agent.setModel",
        json!({ "session_id": emb_id, "agent_id": "main", "model": "contract-model" }),
    )
    .await;
    assert_same_shape("agent.setModel", &srv_set_model, &emb_set_model);
    // Value pin: the klient `setModelResultSchema` — the applied model
    // echoed back plus the serialized provider type on both sides.
    let expected_set_model = json!({ "model": "contract-model", "providerName": "anthropic" });
    let srv_set_model_value = srv_set_model
        .as_ref()
        .expect("agent.setModel resolves on the server");
    let emb_set_model_value = emb_set_model
        .as_ref()
        .expect("agent.setModel resolves on the embedded engine");
    assert_eq!(srv_set_model_value, &expected_set_model);
    assert_eq!(emb_set_model_value, &expected_set_model);

    let srv_set_thinking = call_server(
        &server,
        "agent.setThinking",
        json!({ "sessionId": srv_id, "agentId": "main", "thinking": "high" }),
    )
    .await;
    let emb_set_thinking = call_embedded(
        &embedded,
        "agent.setThinking",
        json!({ "session_id": emb_id, "agent_id": "main", "thinking": "high" }),
    )
    .await;
    assert_same_shape("agent.setThinking", &srv_set_thinking, &emb_set_thinking);
    // Value pin: the klient `noResult` contract — JSON null on both sides.
    let srv_set_thinking_value = srv_set_thinking
        .as_ref()
        .expect("agent.setThinking resolves on the server");
    let emb_set_thinking_value = emb_set_thinking
        .as_ref()
        .expect("agent.setThinking resolves on the embedded engine");
    assert_eq!(srv_set_thinking_value, &Value::Null);
    assert_eq!(emb_set_thinking_value, &Value::Null);

    let srv_set_permission = call_server(
        &server,
        "agent.setPermission",
        json!({ "sessionId": srv_id, "agentId": "main", "mode": "yolo" }),
    )
    .await;
    let emb_set_permission = call_embedded(
        &embedded,
        "agent.setPermission",
        json!({ "session_id": emb_id, "agent_id": "main", "mode": "yolo" }),
    )
    .await;
    assert_same_shape(
        "agent.setPermission",
        &srv_set_permission,
        &emb_set_permission,
    );
    // Value pin: the klient `noResult` contract — JSON null on both sides.
    let srv_set_permission_value = srv_set_permission
        .as_ref()
        .expect("agent.setPermission resolves on the server");
    let emb_set_permission_value = emb_set_permission
        .as_ref()
        .expect("agent.setPermission resolves on the embedded engine");
    assert_eq!(srv_set_permission_value, &Value::Null);
    assert_eq!(emb_set_permission_value, &Value::Null);

    // Alias probe: the `effort` sibling-alias spelling (what the node-sdk
    // Session facade sends) behaves identically on both backends.
    let srv_set_thinking_alias = call_server(
        &server,
        "agent.setThinking",
        json!({ "sessionId": srv_id, "agentId": "main", "effort": "low" }),
    )
    .await;
    let emb_set_thinking_alias = call_embedded(
        &embedded,
        "agent.setThinking",
        json!({ "session_id": emb_id, "agent_id": "main", "effort": "low" }),
    )
    .await;
    assert_eq!(
        srv_set_thinking_alias
            .as_ref()
            .expect("agent.setThinking (effort alias) resolves on the server"),
        &Value::Null
    );
    assert_eq!(
        emb_set_thinking_alias
            .as_ref()
            .expect("agent.setThinking (effort alias) resolves on the embedded engine"),
        &Value::Null
    );

    // --- agent.activateSkill -----------------------------------------------------
    let srv_activate = call_server(
        &server,
        "agent.activateSkill",
        json!({ "sessionId": srv_id, "agentId": "main", "name": "contract-skill" }),
    )
    .await;
    let emb_activate = call_embedded(
        &embedded,
        "agent.activateSkill",
        json!({ "session_id": emb_id, "name": "contract-skill" }),
    )
    .await;
    assert_same_shape("agent.activateSkill", &srv_activate, &emb_activate);
}

// ===========================================================================
// Tool + skill domain (2 methods)
// ===========================================================================

#[tokio::test]
async fn should_match_tool_and_skill_domain_wire_shapes_across_backends_when_catalogs_are_listed() {
    let server = boot_server(Some("contract-skill")).await;
    let embedded = boot_embedded(Some("contract-skill"));

    let srv_id = create_session_on_server(&server, "catalog-contract").await;
    let emb_id = create_session_on_embedded(&embedded, "catalog-contract").await;
    // Both engines' session.create provisions a "main" agent wired with the
    // subagent host (extra Agent/AgentSwarm tools when a profile manager is
    // present); drive tool.list against a plain default-config agent on both
    // sides so the comparison addresses the same logical agent.
    provision_default_agent(&server.manager, &srv_id, "plain").await;
    provision_default_agent(&embedded.manager, &emb_id, "plain").await;

    // --- tool.list -------------------------------------------------------------
    let srv_tools = call_server(
        &server,
        "tool.list",
        json!({ "sessionId": srv_id, "agentId": "plain" }),
    )
    .await;
    let emb_tools = call_embedded(
        &embedded,
        "tool.list",
        json!({ "session_id": emb_id, "agent_id": "plain" }),
    )
    .await;
    assert_same_shape("tool.list", &srv_tools, &emb_tools);

    // --- skill.list --------------------------------------------------------------
    let srv_skills = call_server(&server, "skill.list", json!({})).await;
    let emb_skills = call_embedded(&embedded, "skill.list", json!({})).await;
    assert_same_shape("skill.list", &srv_skills, &emb_skills);

    // --- skill.getContent (B8 management read face) -------------------------------
    let srv_content = call_server(
        &server,
        "skill.getContent",
        json!({ "name": "contract-skill" }),
    )
    .await;
    let emb_content = call_embedded(
        &embedded,
        "skill.getContent",
        json!({ "name": "contract-skill" }),
    )
    .await;
    assert_same_shape("skill.getContent", &srv_content, &emb_content);

    // --- skill.reload (ADR-010 Task 6: the manual catalog-reload face) -----------
    // Both backends answer the same wire shape: the per-workspace outcome
    // rows with the diff counts and the watching status. The serve wire
    // reaches the engine's own orchestrator through the engine
    // fall-through, so both sides report the same (their own) built
    // workspace's row — shape-identical, never value-identical (the ids
    // differ).
    let srv_reload = call_server(&server, "skill.reload", json!({})).await;
    let emb_reload = call_embedded(&embedded, "skill.reload", json!({})).await;
    assert_same_shape("skill.reload", &srv_reload, &emb_reload);
    // The shape pin: one workspace row per side, the exact key set and
    // types (counts derive from real rescans; watching is a bool).
    let srv_reload = srv_reload.expect("the serve wire answers skill.reload");
    let emb_reload = emb_reload.expect("the embedded wire answers skill.reload");
    for (backend, result) in [("server", &srv_reload), ("embedded", &emb_reload)] {
        let rows = result["workspaces"]
            .as_array()
            .unwrap_or_else(|| panic!("{backend}: skill.reload answers a workspaces array"));
        assert_eq!(
            rows.len(),
            1,
            "{backend}: exactly one built workspace row"
        );
        let row = &rows[0];
        assert!(
            row.get("workspace_id").is_some_and(Value::is_string),
            "{backend}: the row carries a string workspace_id"
        );
        for key in ["added", "removed", "updated"] {
            assert!(
                row.get(key).is_some_and(Value::is_u64),
                "{backend}: the row carries the {key} count"
            );
        }
        assert!(
            row.get("watching").is_some_and(Value::is_boolean),
            "{backend}: the row carries the watching status"
        );
    }
}

// ===========================================================================
// Task domain (3 methods)
// ===========================================================================

#[tokio::test]
async fn should_match_task_domain_wire_shapes_across_backends_when_a_task_is_registered_on_both() {
    let server = boot_server(None).await;
    let embedded = boot_embedded(None);

    let srv_id = create_session_on_server(&server, "task-contract").await;
    let emb_id = create_session_on_embedded(&embedded, "task-contract").await;

    // Register the same background task on both sessions through the shared
    // core TaskManager, so task.list / task.cancel / task.output address real
    // (identically-created) tasks on both backends.
    let srv_task_id = server
        .manager
        .get_session(&SessionId::new(&srv_id))
        .await
        .expect("the server session exists")
        .tasks()
        .register(
            TaskKind::Process {
                command: Some("echo contract".to_string()),
                dispatcher: None,
            },
            "contract task".to_string(),
            srv_id.clone(),
        );
    let emb_task_id = embedded
        .manager
        .get_session(&SessionId::new(&emb_id))
        .await
        .expect("the embedded session exists")
        .tasks()
        .register(
            TaskKind::Process {
                command: Some("echo contract".to_string()),
                dispatcher: None,
            },
            "contract task".to_string(),
            emb_id.clone(),
        );

    // --- task.list ---------------------------------------------------------------
    // Both backends serve the identical element shape: the direct serde
    // serialization of `mirri_core::AgentTask` (TSV row 80, M3-c batch 8).
    let srv_task_list = call_server(&server, "task.list", json!({ "sessionId": srv_id })).await;
    let emb_task_list =
        call_embedded(&embedded, "task.list", json!({ "session_id": emb_id })).await;
    assert_same_shape("task.list", &srv_task_list, &emb_task_list);

    // --- task.listWorkspace --------------------------------------------------------
    // B7 (P-023 / D-1): both backends serve the identical element shape over
    // the live-session union (each seeded with one task on its session — the
    // same direct AgentTask serialization task.list serves, `session_id`
    // carrying the attribution; the server face rides the engine
    // fall-through).
    let srv_ws = call_server(&server, "task.listWorkspace", json!({})).await;
    let emb_ws = call_embedded(&embedded, "task.listWorkspace", json!({})).await;
    assert_same_shape("task.listWorkspace", &srv_ws, &emb_ws);

    // --- task.cancel -----------------------------------------------------------------
    let srv_task_cancel = call_server(
        &server,
        "task.cancel",
        json!({ "sessionId": srv_id, "taskId": srv_task_id }),
    )
    .await;
    let emb_task_cancel = call_embedded(
        &embedded,
        "task.cancel",
        json!({ "session_id": emb_id, "taskId": emb_task_id }),
    )
    .await;
    assert_same_shape("task.cancel", &srv_task_cancel, &emb_task_cancel);

    // --- task.output -------------------------------------------------------------------
    let srv_task_output = call_server(
        &server,
        "task.output",
        json!({ "sessionId": srv_id, "taskId": srv_task_id }),
    )
    .await;
    let emb_task_output = call_embedded(
        &embedded,
        "task.output",
        json!({ "session_id": emb_id, "taskId": emb_task_id }),
    )
    .await;
    assert_same_shape("task.output", &srv_task_output, &emb_task_output);
}

// ===========================================================================
// Cron domain (3 methods — B6 management faces)
// ===========================================================================

#[tokio::test]
async fn should_match_cron_domain_wire_shapes_across_backends_when_a_task_is_seeded_on_both() {
    let server = boot_server(None).await;
    let embedded = boot_embedded(None);

    let srv_id = create_session_on_server(&server, "cron-contract").await;
    let emb_id = create_session_on_embedded(&embedded, "cron-contract").await;

    // Seed the same logical task on both sessions through the shared
    // core CronManager (the task-domain seeding posture): the ids differ,
    // and the shape comparison never sees values — only structure.
    let srv_task_id = server
        .manager
        .get_session(&SessionId::new(&srv_id))
        .await
        .expect("the server session exists")
        .cron()
        .add(
            "*/5 * * * *".to_string(),
            "contract tick".to_string(),
            srv_id.clone(),
            true,
            None,
        )
        .await
        .expect("seed the server cron task");
    let emb_task_id = embedded
        .manager
        .get_session(&SessionId::new(&emb_id))
        .await
        .expect("the embedded session exists")
        .cron()
        .add(
            "*/5 * * * *".to_string(),
            "contract tick".to_string(),
            emb_id.clone(),
            true,
            None,
        )
        .await
        .expect("seed the embedded cron task");

    // --- cron.list (the seeded row shapes must match) --------------------------
    let srv_list = call_server(&server, "cron.list", json!({ "sessionId": srv_id })).await;
    let emb_list = call_embedded(&embedded, "cron.list", json!({ "session_id": emb_id })).await;
    assert_same_shape("cron.list", &srv_list, &emb_list);

    // --- cron.trigger (run-now ack; the injection is spawned, not compared) ----
    let srv_trigger = call_server(
        &server,
        "cron.trigger",
        json!({ "sessionId": srv_id, "taskId": srv_task_id }),
    )
    .await;
    let emb_trigger = call_embedded(
        &embedded,
        "cron.trigger",
        json!({ "session_id": emb_id, "taskId": emb_task_id }),
    )
    .await;
    assert_same_shape("cron.trigger", &srv_trigger, &emb_trigger);

    // --- cron.delete (the removal ack + the emptied list) ----------------------
    let srv_delete = call_server(
        &server,
        "cron.delete",
        json!({ "sessionId": srv_id, "taskId": srv_task_id }),
    )
    .await;
    let emb_delete = call_embedded(
        &embedded,
        "cron.delete",
        json!({ "session_id": emb_id, "taskId": emb_task_id }),
    )
    .await;
    assert_same_shape("cron.delete", &srv_delete, &emb_delete);
    let srv_after = call_server(&server, "cron.list", json!({ "sessionId": srv_id })).await;
    let emb_after = call_embedded(&embedded, "cron.list", json!({ "session_id": emb_id })).await;
    assert_same_shape("cron.list (after delete)", &srv_after, &emb_after);
}

// ===========================================================================
// Config + model domain (4 methods)
// ===========================================================================

#[tokio::test]
async fn should_match_config_and_model_domain_wire_shapes_across_backends_when_catalogs_are_read() {
    let server = boot_server(None).await;
    let embedded = boot_embedded(None);

    // --- config.get -------------------------------------------------------------------
    // Both backends serve the same resolved-config wire shape (M3-c batch
    // 9): the to_mirri_config_wire() serialization of the booted config with
    // provider api keys redacted — the server's identity triple is gone
    // (identity stays in REST /meta).
    let srv_config_get = call_server(&server, "config.get", json!({})).await;
    let emb_config_get = call_embedded(&embedded, "config.get", json!({})).await;
    assert_same_shape("config.get", &srv_config_get, &emb_config_get);
    assert_recorded_shape(
        "config.get",
        "server",
        &srv_config_get,
        // The concrete wire shape is locked through the same public core
        // serialization the handler serves from its shared config.
        &normalize(&redact_providers(
            server.initial_config.to_mirri_config_wire(),
        )),
    );
    assert_recorded_shape(
        "config.get",
        "embedded",
        &emb_config_get,
        // The concrete wire shape is locked through the same public core
        // serialization the handler serves from its in-memory config.
        &normalize(&redact_providers(
            embedded.initial_config.to_mirri_config_wire(),
        )),
    );

    // --- config.set --------------------------------------------------------------------
    // Both backends take the full-file merge patch (the ResolvedConfig wire
    // shape itself — an empty patch merges nothing) and answer with the
    // redacted wire shape of the reloaded config (M3-c batch 9). The
    // expected locks derive from the same reload on each side so
    // env-injected config values cannot make them flaky.
    let srv_config_set = call_server(&server, "config.set", json!({})).await;
    let emb_config_set = call_embedded(&embedded, "config.set", json!({})).await;
    assert_same_shape("config.set", &srv_config_set, &emb_config_set);
    let srv_reloaded = mirri_core::ServerConfig::load(&server.config_path, &mirri_os::LocalSyncFs)
        .expect("the server config.toml reloads");
    assert_recorded_shape(
        "config.set",
        "server",
        &srv_config_set,
        &normalize(&redact_providers(srv_reloaded.to_mirri_config_wire())),
    );
    // The embedded handler deep-merges the (empty) patch, saves, reloads
    // with env overrides, and returns that wire shape — the expected lock is
    // derived from the same reload so env-injected config values cannot make
    // the lock flaky.
    let emb_reloaded =
        mirri_core::ServerConfig::load(&embedded.config_path, &mirri_os::LocalSyncFs)
            .expect("the embedded config.toml reloads");
    assert_recorded_shape(
        "config.set",
        "embedded",
        &emb_config_set,
        &normalize(&redact_providers(emb_reloaded.to_mirri_config_wire())),
    );

    // --- model.list -----------------------------------------------------------------------
    // M5-B3a: the historic per-side divergences are retired with the server
    // handlers — `model.list` is engine-domain now, so the serve path and
    // the direct engine path must agree exactly (both answer the configured
    // `modelCatalogItemSchema` rows).
    let srv_model_list = call_server(&server, "model.list", json!({})).await;
    let emb_model_list = call_embedded(&embedded, "model.list", json!({})).await;
    assert_same_shape("model.list", &srv_model_list, &emb_model_list);
    assert_recorded_shape(
        "model.list",
        "embedded",
        &emb_model_list,
        &shape_of(json!([{
            // The config key — the canonical alias id (ADR-003).
            "id": "contract-model",
            "provider": "anthropic",
            "model": "contract-model-1",
            "max_context_size": 128000,
        }])),
    );

    // --- model.listProviders ----------------------------------------------------------------
    // Same M5-B3a state as model.list: both sides return the bare provider
    // array and must agree exactly.
    let srv_providers = call_server(&server, "model.listProviders", json!({})).await;
    let emb_providers = call_embedded(&embedded, "model.listProviders", json!({})).await;
    assert_same_shape("model.listProviders", &srv_providers, &emb_providers);
    assert_recorded_shape(
        "model.listProviders",
        "embedded",
        &emb_providers,
        &shape_of(json!([{
            "id": "anthropic",
            "type": "anthropic",
            "has_api_key": true,
            "status": "connected",
            "models": ["contract-model-1"],
        }])),
    );
}

/// Apply the same provider api-key redaction the embedded handler applies
/// before a wire shape is taken, so the expected lock can be built from the
/// public core serialization without depending on secrets.
fn redact_providers(mut wire: Value) -> Value {
    if let Some(providers) = wire.get_mut("providers").and_then(Value::as_object_mut) {
        for provider in providers.values_mut() {
            if let Some(provider) = provider.as_object_mut()
                && provider.contains_key("apiKey")
            {
                provider.insert(
                    "apiKey".to_string(),
                    Value::String("[REDACTED]".to_string()),
                );
            }
        }
    }
    wire
}

// ===========================================================================
// Btw domain (1 method)
// ===========================================================================

#[tokio::test]
async fn should_match_btw_domain_wire_shapes_across_backends_when_a_side_channel_is_started() {
    let server = boot_server(None).await;
    let embedded = boot_embedded(None);

    let srv_id = create_session_on_server(&server, "btw-contract").await;
    let emb_id = create_session_on_embedded(&embedded, "btw-contract").await;

    // Reconciled (P11a F-P8-1): both backends now fork a real BTW agent and
    // return its id under the `agent_id` key — the former recorded
    // divergence (the embedded engine's error-shaped stub) is gone, so the
    // two sides are pinned to the SAME shape like every other method.
    let srv_btw = call_server(&server, "btw.start", json!({ "sessionId": srv_id })).await;
    let emb_btw = call_embedded(&embedded, "btw.start", json!({ "session_id": emb_id })).await;
    assert_same_shape("btw.start", &srv_btw, &emb_btw);
    assert_recorded_shape(
        "btw.start",
        "embedded",
        &emb_btw,
        &shape_of(json!({ "agent_id": "agent-id" })),
    );
}

// ===========================================================================
// B9: user-level permission rule CRUD (approval domain)
// ===========================================================================

/// Given both backends with empty rule sets, when the approval rule CRUD
/// vertical is driven on both (list → add → conflict → remove), then the
/// two wires answer the same shapes and the same values — the serve wire
/// rides the engine fall-through to the very same handler.
#[tokio::test]
async fn should_match_permission_rules_crud_wire_shapes_across_backends_when_rules_are_driven_on_both()
 {
    let server = boot_server(None).await;
    let embedded = boot_embedded(None);

    // --- approval.listRules (the empty baseline) ---------------------------
    let srv_list = call_server(&server, "approval.listRules", json!({})).await;
    let emb_list = call_embedded(&embedded, "approval.listRules", json!({})).await;
    assert_same_shape("approval.listRules", &srv_list, &emb_list);
    for (list, side) in [(&srv_list, "server"), (&emb_list, "embedded")] {
        let list = list.as_ref().unwrap();
        assert_eq!(
            list["rules"].as_array().map(Vec::len),
            Some(0),
            "({side}) the seeded config carries no rules"
        );
    }

    // --- approval.addRule (a deny rule for Bash) ---------------------------
    let add = json!({ "rule": { "decision": "deny", "pattern": "Bash" } });
    let srv_added = call_server(&server, "approval.addRule", add.clone()).await;
    let emb_added = call_embedded(&embedded, "approval.addRule", add.clone()).await;
    assert_same_shape("approval.addRule", &srv_added, &emb_added);
    for (added, side) in [(&srv_added, "server"), (&emb_added, "embedded")] {
        let added = added.as_ref().unwrap();
        assert_eq!(added["rules"].as_array().map(Vec::len), Some(1), "({side})");
        assert_eq!(added["rules"][0]["pattern"], "Bash", "({side})");
        assert_eq!(added["rules"][0]["decision"], "deny", "({side})");
        assert_eq!(added["rules"][0]["scope"], "user", "({side})");
    }

    // --- approval.addRule error shapes (duplicate no-op, conflict, bad
    //     pattern) — the wires must agree on both transport paths -----------
    let dup = json!({ "rule": { "decision": "deny", "pattern": "Bash" } });
    let srv_dup = call_server(&server, "approval.addRule", dup.clone()).await;
    let emb_dup = call_embedded(&embedded, "approval.addRule", dup.clone()).await;
    assert_same_shape("approval.addRule (duplicate)", &srv_dup, &emb_dup);

    let conflict = json!({ "rule": { "decision": "allow", "pattern": "Bash" } });
    let srv_conflict = call_server(&server, "approval.addRule", conflict.clone()).await;
    let emb_conflict = call_embedded(&embedded, "approval.addRule", conflict.clone()).await;
    let srv_conflict = srv_conflict.expect_err("the conflicting rule must be refused server-side");
    let emb_conflict =
        emb_conflict.expect_err("the conflicting rule must be refused embedded-side");
    assert!(
        srv_conflict.contains("conflicting rule") && emb_conflict.contains("conflicting rule"),
        "the conflict refusal matches on both backends: {srv_conflict} / {emb_conflict}"
    );

    let bad = json!({ "rule": { "decision": "allow", "pattern": "Write(/unclosed" } });
    let srv_bad = call_server(&server, "approval.addRule", bad.clone()).await;
    let emb_bad = call_embedded(&embedded, "approval.addRule", bad.clone()).await;
    assert!(
        srv_bad.is_err() && emb_bad.is_err(),
        "unparseable pattern refused on both"
    );

    // --- approval.listRules (the rule landed) ------------------------------
    let srv_list = call_server(&server, "approval.listRules", json!({})).await;
    let emb_list = call_embedded(&embedded, "approval.listRules", json!({})).await;
    assert_same_shape("approval.listRules (after add)", &srv_list, &emb_list);

    // --- approval.removeRule (by pattern + decision) -----------------------
    let remove = json!({ "pattern": "Bash", "decision": "deny" });
    let srv_removed = call_server(&server, "approval.removeRule", remove.clone()).await;
    let emb_removed = call_embedded(&embedded, "approval.removeRule", remove.clone()).await;
    assert_same_shape("approval.removeRule", &srv_removed, &emb_removed);
    for (removed, side) in [(&srv_removed, "server"), (&emb_removed, "embedded")] {
        let removed = removed.as_ref().unwrap();
        assert_eq!(
            removed["rules"].as_array().map(Vec::len),
            Some(0),
            "({side}) the rule is gone"
        );
    }

    // --- approval.removeRule error shape (the unknown pair) ----------------
    let srv_missing = call_server(&server, "approval.removeRule", remove.clone()).await;
    let emb_missing = call_embedded(&embedded, "approval.removeRule", remove.clone()).await;
    assert!(
        srv_missing.is_err() && emb_missing.is_err(),
        "the unknown pair answers not found on both backends"
    );
}

// ===========================================================================
// Registry gate: the serve wire surface is exactly the fact table (M5-B3a)
// ===========================================================================

/// The M5-B3a registry gate proof: every `serves_server` fact row answers
/// through the shared dispatch (transport registry or engine fall-through),
/// and every embedded-only / unimplemented / unknown method answers
/// method-not-found. The serve wire surface stays EXACTLY the 190
/// [`mirri_proto::rpc`] facts — no engine-only method leaks onto the wire,
/// no transport registry grows beyond its 25 rows (the B2/B3 flips and the
/// B4-a/B4-b/B5-a/B5-b rows ride the engine fall-through, not the
/// transport registry).
#[tokio::test]
async fn should_serve_exactly_the_fact_table_when_the_surface_is_enumerated() {
    // Given an engine-backed server state
    let server = boot_server(None).await;

    // When every method the fact table marks `serves_server` is called with
    // empty params through the shared dispatch (the gate every wire client
    // crosses)
    let serve_surface: Vec<&'static str> = methods_for(RpcBackend::Server)
        .map(|entry| entry.name)
        .collect();
    assert_eq!(
        serve_surface.len(),
        190,
        "the pinned serve surface (16 server-only + 174 both rows — the rust-core-mcp-model batches among them: the M5 mcp.config.preview, the M6 mcp.sync, the M7 mcp.subscribe, the R4 Task 25 agent profile lifecycle rows: enable/disable/reset, the W8 workspace.setTrust row, the collaboration.enter/exit mode-gate pair, the B8 skill.getContent/skill.setEnabled management pair, the B6 cron.delete/cron.trigger management pair, the B7 task.listWorkspace overview row, the 6 session-retention management rows, and the ADR-010 skill.reload management row; the 8 auth rows left with the managed-login removal, plus the ADR-010 skill.subscribe server-only row)"
    );
    for method in &serve_surface {
        // A valid dispatch outcome is any error except method-not-found:
        // handlers and engine arms may reject empty params (invalid params),
        // unknown sessions (not found), etc. — the point is they ANSWER.
        let outcome = server.state.dispatch(method, json!({})).await;
        assert!(
            !matches!(outcome, Err(mirri_server::RpcError::MethodNotFound(_))),
            "{method}: a serves_server fact row must not answer method-not-found"
        );
    }

    // Then every row that does NOT serve the server is gated off: the
    // embedded-only and unimplemented rows never appear on the serve wire
    for entry in all_methods() {
        if entry.availability.serves_server() {
            continue;
        }
        let outcome = server.state.dispatch(entry.name, json!({})).await;
        assert!(
            matches!(outcome, Err(mirri_server::RpcError::MethodNotFound(_))),
            "{}: a non-serves_server fact row must be gated off the serve wire",
            entry.name
        );
    }

    // And a never-registered name is method-not-found too
    let outcome = server.state.dispatch("no.such.method", json!({})).await;
    assert!(
        matches!(outcome, Err(mirri_server::RpcError::MethodNotFound(_))),
        "unknown methods answer method-not-found"
    );
}

// ===========================================================================
// W6: the workspace update/delete wire faces (embedded-only pins)
// ===========================================================================

/// Given a REAL two-workspace assembled server (`for_workspace_root` over a
/// home whose `workspaces.json` pre-seeds a second row), when the second
/// workspace is renamed and then deleted in purge mode, then the result
/// objects match the W6 wire faces exactly, the registry file tombstones the
/// id, and deleting the pinned default workspace is refused.
#[tokio::test]
async fn should_pin_the_w6_workspace_update_and_delete_wire_faces_on_a_two_workspace_server() {
    // Given an injected home pre-seeding a second workspace row (the root
    // exists, so the boot registers it lazily beside the bound default)
    let home = tempfile::tempdir().expect("home tempdir");
    let second_root = tempfile::tempdir().expect("second workspace tempdir");
    let second_id = mirri_core::encode_work_dir_key(second_root.path());
    let registry_path = home
        .path()
        .join(mirri_session_index::WORKSPACE_REGISTRY_FILENAME);
    std::fs::write(
        &registry_path,
        serde_json::to_string(&serde_json::json!({
            "version": 1,
            "workspaces": {
                second_id.as_str(): {
                    "root": second_root.path().to_string_lossy(),
                    "name": "second",
                    "created_at": "2024-01-01T00:00:00.000Z",
                    "last_opened_at": "2024-01-01T00:00:00.000Z",
                }
            },
            "deleted_workspace_ids": [],
        }))
        .expect("registry JSON serializes"),
    )
    .expect("pre-seed workspaces.json");
    let bound_root = tempfile::tempdir().expect("bound workspace tempdir");
    let engine = Arc::new(EmbeddedServer::for_workspace_root(
        WorkspaceId::new("w6-contract"),
        bound_root.path().to_path_buf(),
        Some(home.path().to_path_buf()),
        Vec::new(),
    ));

    // When the second workspace is renamed
    let updated = EngineAdapter::call_method(
        &*engine,
        "workspace.update",
        json!({ "workspaceId": second_id, "name": "renamed" }),
    )
    .await
    .expect("workspace.update resolves");

    // Then the result is the renamed WorkspaceView (the exact W6 face:
    // camelCase keys, RFC3339 string timestamps, the session count)
    assert_eq!(updated["id"], json!(second_id));
    assert_eq!(updated["name"], json!("renamed"));
    assert_eq!(updated["root"], json!(second_root.path().to_string_lossy()));
    assert_eq!(updated["sessionCount"], json!(0));
    assert!(updated["createdAt"].as_str().is_some(), "createdAt string");
    assert!(
        updated["lastOpenedAt"].as_str().is_some(),
        "lastOpenedAt string"
    );
    assert_eq!(
        updated.as_object().expect("an object").keys().count(),
        7,
        "the update result field set is exactly the seven WorkspaceView keys (W8 added trusted)"
    );

    // And when the second workspace is deleted in purge mode
    let deleted = EngineAdapter::call_method(
        &*engine,
        "workspace.delete",
        json!({ "workspaceId": second_id, "mode": "purge" }),
    )
    .await
    .expect("workspace.delete resolves");

    // Then the result is the exact W6 deletion face (camelCase keys, the
    // executed mode echo, the index rows removed count)
    assert_eq!(
        deleted,
        json!({
            "ok": true,
            "mode": "purge",
            "workspaceId": second_id,
            "deletedSessions": 0,
        })
    );

    // And the cross-engine registry file tombstones the id (a cold read of
    // the pre-seeded row now answers empty)
    let registry: Value = serde_json::from_str(
        &std::fs::read_to_string(&registry_path).expect("workspaces.json written"),
    )
    .expect("workspaces.json parses");
    assert_eq!(
        registry["deleted_workspace_ids"],
        json!([second_id]),
        "the purge tombstones the deleted id"
    );
    assert!(
        mirri_session_index::read_workspace_registry(&registry_path)
            .iter()
            .all(|row| row.id != second_id),
        "a cold registry read never sees the deleted workspace"
    );

    // And deleting the pinned default workspace is refused (the bound root
    // is this engine's process default — every degrade path addresses its
    // runtime; a re-election is a W9 candidate)
    let default_id = mirri_core::encode_work_dir_key(bound_root.path());
    let refusal = EngineAdapter::call_method(
        &*engine,
        "workspace.delete",
        json!({ "workspaceId": default_id, "mode": "remove" }),
    )
    .await
    .expect_err("deleting the default must be refused");
    assert!(
        refusal.to_string().contains("-32602"),
        "the default-refusal is an invalid_params error: {refusal}"
    );
}

// W8: the workspace trust management wire face (embedded-only pin)
// ===========================================================================

/// Given a REAL two-workspace assembled server (`for_workspace_root` over a
/// home whose `workspaces.json` pre-seeds a second row), when the second
/// workspace is trusted and then untrusted through `workspace.setTrust`,
/// then the results carry the TS trust face `{trusted}`, the
/// `workspace.list` rows report the live store-read verdict both ways
/// (the default row stays trusted), the untrust converges the second
/// workspace's pool to empty, and untrusting the pinned default workspace
/// is refused.
#[tokio::test]
async fn should_pin_the_w8_workspace_trust_wire_faces_on_a_two_workspace_server() {
    // Given an injected home pre-seeding a second workspace row
    let home = tempfile::tempdir().expect("home tempdir");
    let second_root = tempfile::tempdir().expect("second workspace tempdir");
    let second_id = mirri_core::encode_work_dir_key(second_root.path());
    std::fs::write(
        home.path()
            .join(mirri_session_index::WORKSPACE_REGISTRY_FILENAME),
        serde_json::to_string(&serde_json::json!({
            "version": 1,
            "workspaces": {
                second_id.as_str(): {
                    "root": second_root.path().to_string_lossy(),
                    "name": "second",
                    "created_at": "2024-01-01T00:00:00.000Z",
                    "last_opened_at": "2024-01-01T00:00:00.000Z",
                }
            },
            "deleted_workspace_ids": [],
        }))
        .expect("registry JSON serializes"),
    )
    .expect("pre-seed workspaces.json");
    // The second workspace's project layer declares one (disabled-echo)
    // server so the connect/disconnect symmetry is observable.
    std::fs::write(
        second_root.path().join(".mcp.json"),
        serde_json::to_string(&serde_json::json!({
            "mcpServers": { "projsrv": { "command": "echo", "enabled": false } }
        }))
        .expect("mcp JSON serializes"),
    )
    .expect("write the second root's .mcp.json");
    let bound_root = tempfile::tempdir().expect("bound workspace tempdir");
    let engine = Arc::new(EmbeddedServer::for_workspace_root(
        WorkspaceId::new("w8-contract"),
        bound_root.path().to_path_buf(),
        Some(home.path().to_path_buf()),
        Vec::new(),
    ));
    let default_id = mirri_core::encode_work_dir_key(bound_root.path());
    let second_runtime = engine
        .workspace_registry()
        .ensure_built(&second_id)
        .expect("the second row is registered")
        .expect("the second runtime builds");
    let second_pool = second_runtime
        .mcp_manager
        .clone()
        .expect("the untrusted workspace still gets a pool");

    // Then the cold listing reads untrusted for the second row, trusted
    // for the pinned default (the live store read, never a snapshot)
    let trust_of = |rows: &Value, id: &str| {
        rows.as_array()
            .expect("the bare array")
            .iter()
            .find(|row| row["id"] == json!(id))
            .unwrap_or_else(|| panic!("the row {id} must be listed"))["trusted"]
            .as_bool()
            .expect("trusted is a boolean")
    };
    let cold = EngineAdapter::call_method(&*engine, "workspace.list", json!({}))
        .await
        .expect("workspace.list resolves");
    assert!(!trust_of(&cold, &second_id));
    assert!(trust_of(&cold, &default_id));

    // When the second workspace is trusted
    let granted = EngineAdapter::call_method(
        &*engine,
        "workspace.setTrust",
        json!({ "workspaceId": second_id, "trusted": true }),
    )
    .await
    .expect("the grant resolves");

    // Then the result is the TS trust face and the listing flips
    assert_eq!(granted, json!({ "trusted": true }));
    let trusted_listing = EngineAdapter::call_method(&*engine, "workspace.list", json!({}))
        .await
        .expect("workspace.list resolves");
    assert!(trust_of(&trusted_listing, &second_id));
    // And the trust store file carries the grant (a cold read agrees)
    let store =
        mirri_embedded::WorkspaceTrustStore::open(Some(home.path())).expect("the store opens");
    assert!(store.is_trusted(&second_id));

    // And the project layer connected on the workspace's own pool
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while !second_pool
        .list_connections()
        .await
        .iter()
        .any(|(name, _)| name == "projsrv")
    {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the trusted workspace's project server never connected"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }

    // And when trust is revoked
    let revoked = EngineAdapter::call_method(
        &*engine,
        "workspace.setTrust",
        json!({ "workspaceId": second_id, "trusted": false }),
    )
    .await
    .expect("the revoke resolves");

    // Then the result carries the new state, the pool converged to empty
    // (the immediate disconnect — no restart, no rebuild), and the listing
    // reports untrusted again
    assert_eq!(revoked, json!({ "trusted": false }));
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while !second_pool.list_connections().await.is_empty() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the untrusted workspace's project server never disconnected"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let untrusted_listing = EngineAdapter::call_method(&*engine, "workspace.list", json!({}))
        .await
        .expect("workspace.list resolves");
    assert!(!trust_of(&untrusted_listing, &second_id));
    assert!(!store.is_trusted(&second_id));

    // And untrusting the pinned default workspace is refused
    let refusal = EngineAdapter::call_method(
        &*engine,
        "workspace.setTrust",
        json!({ "workspaceId": default_id, "trusted": false }),
    )
    .await
    .expect_err("untrusting the default must be refused");
    assert!(
        refusal.to_string().contains("-32602"),
        "the default-refusal is an invalid_params error: {refusal}"
    );
}

// ===========================================================================
// Wire replay equivalence invariant (rendering contract, ADR-009)
// ===========================================================================

/// The wire replay equivalence invariant: a session's snapshot taken LIVE
/// must be field-identical to the snapshot taken after a COLD restore from
/// the session's persisted `wire.jsonl` — the guard that the live
/// (loop-event) materialization of context memory and the replay (wire-op
/// fold) materialization never drift apart (ADR-009 in
/// `docs/specs/conversation-rendering-architecture.md`). A diff here is a
/// replay/reducer defect, not a test artifact.
///
/// Both snapshots go through the same engine-backed server dispatch and the
/// same `message_list_row` row builder, so the comparison locks the whole
/// read face: row ids (the ADR-007 entry-seq anchors), roles, `agentId`,
/// the TS-protocol content parts (including the projected `tool_use` /
/// `tool_result` shapes), and the derived `createdAt` (base = the session's
/// creation time, restored from the persisted `state.json` so the base is
/// identical on both sides).
#[tokio::test]
async fn should_yield_an_identical_snapshot_when_a_session_is_restored_from_disk() {
    // Given an engine-backed server backend whose scripted mock LLM drives a
    // tool-calling turn: one Bash request (yolo permission mode so it runs
    // without an approval ask), then a plain reply that closes the turn.
    // The shared workspace tempdir is the cold-restore anchor: the session
    // persistence bucket key derives from the workspace root, so the second
    // backend boots at the SAME root — the tempdir is passed as its keepalive
    // and stays alive for the whole test through the live backend's struct.
    let shared_workspace = tempfile::tempdir().expect("create the shared workspace tempdir");
    let workspace_root = shared_workspace.path().to_path_buf();
    let live = boot_server_at(&workspace_root, None, shared_workspace).await;

    let created = call_server(
        &live,
        "session.create",
        json!({ "title": "replay equivalence", "permissionMode": "yolo" }),
    )
    .await
    .expect("session.create succeeds");
    let session_id = created["id"]
        .as_str()
        .expect("session.create returns a session id")
        .to_string();

    // The two scripted replies of the driven turn report non-zero token
    // usage (the mock defaults to zeros), so both `step.end` loop events
    // carry buckets and the per-turn fold emits a record for the turn —
    // otherwise the usage read face has nothing to guard.
    let scripted_usage = TokenUsage {
        input_tokens: 10,
        output_tokens: 5,
        cache_read_tokens: 1_000,
        cache_creation_tokens: 200,
    };
    live.mock
        .push_response(
            MockResponse::tool_use(vec![mirri_core::llm::LlmToolCall {
                id: "call_1".to_string(),
                name: "Bash".to_string(),
                args: json!({ "command": "echo hello" }),
            }])
            .with_usage(scripted_usage.clone()),
        );
    live.mock.push_response(
        MockResponse::end_turn("Command ran fine").with_usage(scripted_usage),
    );
    let turn = call_server(
        &live,
        "message.send",
        json!({ "sessionId": session_id, "agentId": "main", "text": "run a tool" }),
    )
    .await
    .expect("the live turn drives");
    assert_eq!(turn["ok"], json!(true), "the live turn completes: {turn}");
    assert_eq!(
        live.mock.chat_call_count(),
        2,
        "a tool turn must consume exactly two chat calls (tool request + closing reply)"
    );

    let snapshot_a = call_server(
        &live,
        "session.snapshot",
        json!({ "sessionId": session_id }),
    )
    .await
    .expect("the live snapshot is taken");

    // When the session is closed onto disk (state.json + snapshot.json land,
    // the wire log was already appended through), and a fresh backend
    // cold-restores it purely from the persisted wire log
    call_server(&live, "session.close", json!({ "sessionId": session_id }))
        .await
        .expect("the session closes to disk");

    let restored = boot_server_at(
        &workspace_root,
        None,
        tempfile::tempdir().expect("create the restored backend keepalive tempdir"),
    )
    .await;
    call_server(
        &restored,
        "session.restore",
        json!({ "sessionId": session_id }),
    )
    .await
    .expect("the cold restore replays the wire log");
    assert_eq!(
        restored.mock.chat_call_count(),
        0,
        "a replay must consult no LLM"
    );
    let snapshot_b = call_server(
        &restored,
        "session.snapshot",
        json!({ "sessionId": session_id }),
    )
    .await
    .expect("the restored snapshot is taken");

    // Then the two snapshots' message pages agree field-by-field: ids, roles,
    // agentId, content parts (the tool_use/tool_result projection included)
    // and the base+position-derived createdAt all match.
    let live_messages = &snapshot_a["messages"]["items"];
    let restored_messages = &snapshot_b["messages"]["items"];
    assert_eq!(
        live_messages, restored_messages,
        "live and wire-replayed snapshots drifted:\n  live:     {}\n  restored: {}",
        live_messages, restored_messages
    );

    // And the per-turn usage read face agrees between live and restored —
    // both fold the same wire log, so any drift is a regression.
    assert_eq!(
        snapshot_a["turn_usages"], snapshot_b["turn_usages"],
        "turn_usages drifted between live and wire-replayed snapshots:\n  live:     {}\n  restored: {}",
        snapshot_a["turn_usages"], snapshot_b["turn_usages"]
    );
    let live_turn_usages = snapshot_a["turn_usages"]
        .as_array()
        .expect("turn_usages is an array");
    assert!(
        !live_turn_usages.is_empty(),
        "the driven turn recorded step usage, so turn_usages must be non-empty"
    );
    assert!(
        live_turn_usages[0]["promptId"].is_string(),
        "each entry keys the turn: {}",
        live_turn_usages[0]
    );
    // And at least one assistant row carries the stamped prompt id.
    assert!(
        live_messages
            .as_array()
            .expect("messages array")
            .iter()
            .any(|row| row.get("promptId").is_some()),
        "an assistant row must carry the turn's prompt id: {}",
        live_messages
    );
    assert!(
        live_messages.as_array().is_some_and(|rows| rows.len() >= 4),
        "the tool turn must leave at least user/assistant/tool/assistant rows: {}",
        live_messages
    );

    // And the in-flight turn agrees — a completed wait-mode turn leaves no
    // shadow on either path.
    assert_eq!(
        snapshot_a["in_flight_turn"], snapshot_b["in_flight_turn"],
        "in_flight_turn drifted between live and wire-replayed snapshots"
    );
    assert_eq!(
        snapshot_a["in_flight_turn"],
        Value::Null,
        "a finished wait-mode turn must leave in_flight_turn null"
    );
}

/// The skill-activation read face across a cold restore: the user message a
/// `agent.activateSkill` turn appends carries the rendered user-slash skill
/// prompt with the full `skill_activation` origin payload (identity fields
/// included), and BOTH must survive the persistence round-trip — the wire
/// log's `context.append_message` op has to carry the origin so the replay
/// (wire-op fold) rebuilds the same row the live (loop-event)
/// materialization produced. The row is served through `message.list`'s
/// `message_list_row` builder, where origin rides under `metadata.origin` —
/// the exact face the web transcript reads to filter system-injected turns
/// back in as user input.
///
/// Modeled on
/// [`should_yield_an_identical_snapshot_when_a_session_is_restored_from_disk`]:
/// the shared workspace tempdir is the cold-restore anchor (the persistence
/// bucket key derives from the workspace root), so the fresh backend boots
/// at the SAME root.
#[tokio::test]
async fn should_keep_skill_activation_origin_payload_when_session_is_restored_from_disk() {
    // Given a live backend with the seeded contract skill, a session, and a
    // completed skill-activation turn (fire mode: the RPC resolves once the
    // turn launches, so the turn's model call is awaited by bounded polling
    // of the mock's chat call count, then the turn's closing assistant reply
    // is awaited on the live read face so the close never races the turn)
    let shared_workspace = tempfile::tempdir().expect("create the shared workspace tempdir");
    let workspace_root = shared_workspace.path().to_path_buf();
    let live = boot_server_at(&workspace_root, Some("contract-skill"), shared_workspace).await;

    let created = call_server(
        &live,
        "session.create",
        json!({ "title": "skill activation replay" }),
    )
    .await
    .expect("session.create succeeds");
    let session_id = created["id"]
        .as_str()
        .expect("session.create returns a session id")
        .to_string();

    live.mock.push_text_response("Skill loaded and followed.");
    let activated = call_server(
        &live,
        "agent.activateSkill",
        json!({ "sessionId": session_id, "name": "contract-skill", "args": "review the diff" }),
    )
    .await
    .expect("activateSkill succeeds");
    assert_eq!(activated["activated"], json!(true), "activation acked: {activated}");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while live.mock.chat_call_count() < 1 {
        assert!(
            std::time::Instant::now() < deadline,
            "the skill turn's model call never ran"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let turn_recorded = |rows: &Value| {
        rows["messages"]
            .as_array()
            .is_some_and(|messages| messages.len() >= 2)
    };
    loop {
        let rows = call_server(
            &live,
            "message.list",
            json!({ "sessionId": session_id }),
        )
        .await
        .expect("the live message.list succeeds");
        if turn_recorded(&rows) {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the skill turn never completed (no assistant reply landed)"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }

    // When the session is closed to disk and a fresh backend cold-restores it
    // purely from the persisted wire log
    call_server(&live, "session.close", json!({ "sessionId": session_id }))
        .await
        .expect("the session closes to disk");
    let restored = boot_server_at(
        &workspace_root,
        Some("contract-skill"),
        tempfile::tempdir().expect("create the restored backend keepalive tempdir"),
    )
    .await;
    call_server(
        &restored,
        "session.restore",
        json!({ "sessionId": session_id }),
    )
    .await
    .expect("the cold restore replays the wire log");
    assert_eq!(
        restored.mock.chat_call_count(),
        0,
        "a replay must consult no LLM"
    );

    // Then message.list serves the skill-activation user message with its
    // full identity payload intact — the exact face the web transcript reads
    let listed = call_server(
        &restored,
        "message.list",
        json!({ "sessionId": session_id }),
    )
    .await
    .expect("message.list succeeds after the restore");
    let messages = listed["messages"].as_array().expect("message rows");
    let activation = messages
        .iter()
        .find(|m| m["metadata"]["origin"]["kind"] == json!("skill_activation"))
        .expect("the skill-activation message survived the restore");
    assert_eq!(activation["role"], json!("user"));
    assert_eq!(
        activation["metadata"]["origin"]["skillName"],
        json!("contract-skill")
    );
    assert_eq!(
        activation["metadata"]["origin"]["trigger"],
        json!("user-slash")
    );
    assert_eq!(
        activation["metadata"]["origin"]["skillArgs"],
        json!("review the diff")
    );
    // Plain-text rows keep the historical string collapse (the
    // `message_content_to_protocol_json` contract), so the rendered prompt
    // body is the row's content string.
    let text = activation["content"].as_str().unwrap_or_default();
    assert!(
        text.contains("<kimi-skill-loaded name=\"contract-skill\""),
        "the rendered prompt body must survive the restore: {text}"
    );
}

/// The ADR-006 admission-stamp read face across a cold restore: a user
/// message whose prompt carried an inline skill reference is rewritten by
/// the engine at admission (wrap messages + body with the references
/// replaced by bare `[display]` markers), and the DEDUPED activated-skill
/// list is stamped on the body's `ContextMessage` as `with_skills`
/// (`[{name,target,trigger}]`, the TS wire contract). `message.list`'s
/// shared `message_list_row` builder projects that stamp as a TOP-LEVEL row
/// key — the exact face the web transcript reads to re-render skill chips
/// on reloaded sessions. The stamp must survive the persistence
/// round-trip: the wire log's `context.append_message` op has to carry it
/// so the replay (wire-op fold) rebuilds the same row the live
/// (in-memory) materialization produced — a journal-only stamp would make
/// live and restored diverge.
///
/// Modeled on
/// [`should_keep_skill_activation_origin_payload_when_session_is_restored_from_disk`]:
/// the same shared workspace tempdir is the cold-restore anchor (the
/// persistence bucket key derives from the workspace root), and the fresh
/// backend boots at the SAME root. The live backend runs the engine-backed
/// serve path (`ServerState::dispatch` → engine fall-through →
/// `EmbeddedServer`), which is the same `dispatch_send_message` →
/// `maybe_inject_inline_skill_activations` route the embedded test from
/// Task 2 drives — inline injection is exercised live, not simulated.
#[tokio::test]
async fn should_keep_with_skills_when_session_is_restored_from_disk() {
    // Given a live backend with the seeded contract skill, a session, and a
    // completed wait-mode turn whose prompt carried an inline reference to
    // the skill (the turn's model call is scripted; wait mode settles the
    // turn, so the body's memory append and the wire-log stamp have both
    // landed when message.send resolves)
    let shared_workspace = tempfile::tempdir().expect("create the shared workspace tempdir");
    let workspace_root = shared_workspace.path().to_path_buf();

    let live = boot_server_at(&workspace_root, Some("contract-skill"), shared_workspace).await;

    let created = call_server(
        &live,
        "session.create",
        json!({ "title": "with_skills inline replay" }),
    )
    .await
    .expect("session.create succeeds");
    let session_id = created["id"]
        .as_str()
        .expect("session.create returns a session id")
        .to_string();

    // The inline reference links to the skill's own SKILL.md — the same
    // `[$name](/abs/path/SKILL.md)` form the web composer inserts and
    // ADR-004's parser consumes (trigger prefix + display + file target).
    let skill_md = workspace_root
        .join(".agents")
        .join("skills")
        .join("contract-skill")
        .join("SKILL.md");
    let inline_text = format!("[$contract-skill]({}) please", skill_md.display());

    live.mock.push_text_response("Skill loaded and followed.");
    let turn = call_server(
        &live,
        "message.send",
        json!({ "sessionId": session_id, "agentId": "main", "text": inline_text }),
    )
    .await
    .expect("the live turn drives");
    assert_eq!(turn["ok"], json!(true), "the live turn completes: {turn}");
    assert_eq!(
        live.mock.chat_call_count(),
        1,
        "a plain inline-skill turn consumes exactly one chat call"
    );

    // Then message.list serves the rewritten user row with its top-level
    // `with_skills` list intact on the LIVE session — the session is read
    // before it closes, because `session.close` evicts it from the live
    // backend (the restored backend is the only one that serves it after)
    let expected_with_skills = json!([
        {
            "name": "contract-skill",
            "target": skill_md.display().to_string(),
            "trigger": "user-inline",
        }
    ]);
    let expected_content = "[contract-skill] please";

    let live_rows = call_server(
        &live,
        "message.list",
        json!({ "sessionId": session_id }),
    )
    .await
    .expect("the live message.list succeeds");
    let live_body = live_rows["messages"]
        .as_array()
        .expect("message rows")
        .iter()
        .find(|m| m["role"] == json!("user") && m["metadata"]["origin"]["kind"] == json!("user"))
        .expect("the rewritten body row is served on the live session");
    assert_eq!(
        live_rows["messages"]
            .as_array()
            .expect("message rows")
            .iter()
            .filter(|m| m["metadata"]["origin"]["kind"] == json!("skill_activation"))
            .count(),
        1,
        "the inline reference must inject exactly one wrap row on the live session"
    );
    assert_eq!(
        live_body["with_skills"], expected_with_skills,
        "the live body row carries the admission stamp: {live_body}"
    );
    assert_eq!(
        live_body["content"], json!(expected_content),
        "the live body row shows the rewritten prompt body: {live_body}"
    );

    // When the session is closed to disk and a fresh backend cold-restores
    // it purely from the persisted wire log
    call_server(&live, "session.close", json!({ "sessionId": session_id }))
        .await
        .expect("the session closes to disk");
    let restored = boot_server_at(
        &workspace_root,
        Some("contract-skill"),
        tempfile::tempdir().expect("create the restored backend keepalive tempdir"),
    )
    .await;
    call_server(
        &restored,
        "session.restore",
        json!({ "sessionId": session_id }),
    )
    .await
    .expect("the cold restore replays the wire log");
    assert_eq!(
        restored.mock.chat_call_count(),
        0,
        "a replay must consult no LLM"
    );

    // Then the restored backend serves the SAME rewritten user row with the
    // same top-level `with_skills` list intact — identical shape and
    // content (the row builder's projection is shared; the divergence this
    // test guards is live-memory vs wire-replay state)
    let restored_rows = call_server(
        &restored,
        "message.list",
        json!({ "sessionId": session_id }),
    )
    .await
    .expect("the restored message.list succeeds");
    let restored_body = restored_rows["messages"]
        .as_array()
        .expect("message rows")
        .iter()
        .find(|m| m["role"] == json!("user") && m["metadata"]["origin"]["kind"] == json!("user"))
        .expect("the rewritten body row survives the restore");
    assert_eq!(
        restored_rows["messages"]
            .as_array()
            .expect("message rows")
            .iter()
            .filter(|m| m["metadata"]["origin"]["kind"] == json!("skill_activation"))
            .count(),
        1,
        "the inline reference must inject exactly one wrap row on the restored session"
    );
    assert_eq!(
        restored_body["with_skills"], expected_with_skills,
        "the restored body row carries the admission stamp: {restored_body}"
    );
    assert_eq!(
        restored_body["content"], json!(expected_content),
        "the restored body row shows the rewritten prompt body: {restored_body}"
    );

    // And the two faces agree field-for-field — the live and restored rows
    // are byte-identical (ids, roles, agentId, content, with_skills).
    assert_eq!(
        live_body, restored_body,
        "live and wire-replayed with_skills rows drifted:\n  live:     {}\n  restored: {}",
        live_body, restored_body
    );
}
