//! W3 session-routing e2e tests: dispatch-surface calls addressing a session
//! that lives in a **non-default workspace** resolve through the routing
//! seam (design `docs/specs/rust-core-design/08-multi-workspace.md` §3) —
//! never by silently falling back to the default workspace's manager.
//!
//! The stage mirrors the two-window reality: one server whose home carries a
//! `workspaces.json` with a second registered row, and a session created
//! directly on the second workspace's runtime (the host-API path — the
//! session never touches the default workspace's manager, so any dispatch
//! surface that reaches it must have routed).
//!
//! The W4 section below covers the create-side entry: `session.create`
//! with a `workspace_id` routes the whole construction chain (create,
//! additional-dirs seeding, main agent, index write-through) to the
//! requested workspace, and `session.list`'s live union surfaces the
//! result — the batch's user-perceivable acceptance.

use std::sync::Arc;

use serde_json::{Value, json};

use mirri_core::{
    AgentId, AgentScopeConfig, ApprovalDecision, SessionId, SessionMetadata, SessionScopeConfig,
    WorkspaceId,
};

use mirri_transport::adapter::{EngineAdapter, EngineError};

use super::tests::{ScriptedLlm, ScriptedLlmFactory};
use super::{DEFAULT_MAIN_AGENT_ID, EmbeddedServer};

/// A two-workspace server on a throwaway home: the default workspace at
/// `default_root`, a second row registered at `second_root` (the exact
/// `workspaces.json` shape the TS engine writes, keyed by the work-dir key),
/// and a scripted LLM injected after the assembly so turns can run. Returns
/// the server and the home (kept alive for the process lifetime).
fn two_workspace_server(
    default_root: &std::path::Path,
    second_root: &std::path::Path,
    llm: Arc<ScriptedLlm>,
) -> (EmbeddedServer, tempfile::TempDir) {
    two_workspace_server_with_visibility(
        default_root,
        second_root,
        llm,
        super::ListingVisibility::AllRegistered,
    )
}

/// [`two_workspace_server`] with an explicit listing visibility — the W5
/// pinned-visibility test needs the embedded host's own pin (`for_workspace_root`'s
/// default) instead of the serve-like unrestricted view.
fn two_workspace_server_with_visibility(
    default_root: &std::path::Path,
    second_root: &std::path::Path,
    llm: Arc<ScriptedLlm>,
    visibility: super::ListingVisibility,
) -> (EmbeddedServer, tempfile::TempDir) {
    let home = tempfile::tempdir().expect("home tempdir");
    // The create chain requires a resolvable model (spec 11 R4 — no
    // built-in fallback), so the fixture home carries a config.toml seed,
    // exactly like a real user's home would.
    std::fs::write(
        home.path().join("config.toml"),
        "default_model = \"test-default-model\"\n",
    )
    .expect("seed config.toml must be writable");
    let second_key = mirri_core::encode_work_dir_key(second_root);
    let registry_json = json!({
        "version": 1,
        "workspaces": {
            second_key.as_str(): {
                "root": second_root.to_string_lossy(),
                "name": second_root.file_name().unwrap().to_string_lossy(),
                "created_at": "2024-01-01T00:00:00.000Z",
                "last_opened_at": "2024-06-01T00:00:00.000Z",
            }
        },
        "deleted_workspace_ids": [],
    });
    std::fs::write(
        home.path().join("workspaces.json"),
        serde_json::to_string(&registry_json).expect("registry json serializes"),
    )
    .expect("workspaces.json written");

    let mut server = EmbeddedServer::for_workspace_root(
        WorkspaceId::new("routing-default"),
        default_root.to_path_buf(),
        Some(home.path().to_path_buf()),
        Vec::new(),
    );
    // The assembled factory is env-based (no credentials in tests) — swap in
    // the scripted one so wait-mode turns actually run.
    server.llm_factory = Some(Arc::new(ScriptedLlmFactory::new(llm)));
    // The serve-like listing view: these tests assert the multi-workspace
    // union (a routed create's session is visible in `session.list`), while
    // `for_workspace_root`'s default is the embedded-host pin (the NAPI/ACP
    // visibility — its dedicated test pins it separately).
    server.listing_visibility = visibility;
    (server, home)
}

/// Create a session (with a main agent) directly on the second workspace's
/// runtime — the host-API path that bypasses every dispatch surface: no
/// `note_active`, no index write-through, nothing but the persisted bucket.
async fn create_session_on_second_workspace(
    server: &EmbeddedServer,
    second_root: &std::path::Path,
) -> (String, Arc<mirri_core::SessionScope>) {
    let second_key = mirri_core::encode_work_dir_key(second_root);
    let runtime = server
        .registry
        .ensure_built(&second_key)
        .expect("the second workspace row is registered")
        .expect("the second workspace runtime builds");
    let now = chrono::Utc::now();
    let session = runtime
        .manager
        .create_session(
            SessionMetadata {
                title: "second-workspace session".to_string(),
                created_at: now,
                updated_at: now,
                tags: Default::default(),
                archived: false,
                is_custom_title: false,
                work_dir: None,
                last_prompt: None,
                forked_from: None,
                cache_root_id: None,
                custom: None,
            },
            SessionScopeConfig::default(),
        )
        .await
        .expect("the second workspace manager creates the session");
    session
        .create_agent(
            AgentId::new(DEFAULT_MAIN_AGENT_ID),
            AgentScopeConfig::default(),
        )
        .await
        .expect("the main agent is creatable");
    (session.id().as_str().to_string(), session)
}

/// Given a session that lives only in a non-default workspace, when a
/// wait-mode `message.send` addresses it, then the turn runs on the second
/// workspace's own agent — the scripted LLM is consulted, the reply lands
/// in that agent's context memory, and the default workspace's manager
/// never sees the session.
#[tokio::test]
async fn should_route_message_send_to_a_non_default_workspace_session() {
    // Given a two-workspace server and a session created directly on the
    // second workspace's runtime
    let default_root = tempfile::tempdir().expect("default root tempdir");
    let second_root = tempfile::tempdir().expect("second root tempdir");
    let llm = Arc::new(ScriptedLlm::new());
    llm.push_text("routed hello");
    let (server, _home) =
        two_workspace_server(default_root.path(), second_root.path(), Arc::clone(&llm));
    let (session_id, session) =
        create_session_on_second_workspace(&server, second_root.path()).await;

    // Sanity: the default workspace's manager does NOT hold the session —
    // any dispatch that reaches it routed.
    assert!(
        server
            .session_manager()
            .get_session(&SessionId::new(&session_id))
            .await
            .is_none(),
        "the session never touches the default workspace"
    );

    // When a wait-mode message is sent to the non-default session
    let result = server
        .call_method(
            "message.send",
            json!({ "session_id": &session_id, "text": "hi", "mode": "wait" }),
        )
        .await
        .expect("the routed message.send turn completes");

    // Then the turn ran to its end on the second workspace's agent
    assert_eq!(result["turn"]["stopReason"], "end_turn");
    assert_eq!(llm.chat_call_count(), 1, "the scripted LLM was consulted");

    // And the turn's user message landed in the second workspace's agent
    let agent = session
        .get_agent(&AgentId::new(DEFAULT_MAIN_AGENT_ID))
        .await
        .expect("the main agent stays live");
    let history = agent
        .context_memory()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .history()
        .to_vec();
    assert!(
        history.iter().any(
            |message| message.message.content.iter().any(|part| matches!(
                part,
                mirri_core::ContentPart::Text { text } if text == "hi"
            ))
        ),
        "the user message was recorded on the routed agent"
    );
}

/// Given a pending approval asked through a non-default session's gate, when
/// `approval.decide` addresses the session, then the decision routes to that
/// session's gate — the awaiting asker wakes with the approval.
#[tokio::test]
async fn should_route_approval_decisions_to_a_non_default_workspace_session() {
    // Given a two-workspace server and a live session on the second workspace
    let default_root = tempfile::tempdir().expect("default root tempdir");
    let second_root = tempfile::tempdir().expect("second root tempdir");
    let llm = Arc::new(ScriptedLlm::new());
    let (server, _home) =
        two_workspace_server(default_root.path(), second_root.path(), Arc::clone(&llm));
    let (session_id, _session) =
        create_session_on_second_workspace(&server, second_root.path()).await;

    // And a pending approval asked through that session's own gate
    let artifacts = server
        .resolve_session_artifacts(&session_id)
        .expect("the routed artifacts resolve for the non-default session");
    let mut rx = artifacts.event_bus.subscribe();
    let gate = Arc::clone(&artifacts.approval_gate);
    let asker = tokio::spawn(async move { gate.request("test tool", "summary").await });
    let request_id = match rx.recv().await.expect("the approval requested event") {
        mirri_core::SessionEvent::ToolApprovalRequested { request_id, .. } => request_id,
        other => panic!("expected ToolApprovalRequested, got {other:?}"),
    };

    // When the approval is decided through the dispatch surface
    let decided = server
        .call_method(
            "approval.decide",
            json!({ "session_id": &session_id, "requestId": request_id.to_string(), "approved": true }),
        )
        .await
        .expect("the routed approval.decide resolves");

    // Then the decision reached the second workspace's gate and the asker
    // wakes with the approval
    assert_eq!(decided, json!({ "ok": true }));
    assert!(matches!(
        asker.await.expect("ask task joins"),
        ApprovalDecision::Approved
    ));
}

/// Given a non-default session, when it is closed and resumed through the
/// dispatch surface, then both lifecycle calls route to the second
/// workspace's manager — the close persists into the second bucket, and the
/// resume replays from there (never the default's, which has no such wire
/// log).
#[tokio::test]
async fn should_route_close_and_resume_to_a_non_default_workspace_session() {
    // Given a two-workspace server, a live session on the second workspace,
    // and a completed turn on it (so the wire log carries replayable ops)
    let default_root = tempfile::tempdir().expect("default root tempdir");
    let second_root = tempfile::tempdir().expect("second root tempdir");
    let llm = Arc::new(ScriptedLlm::new());
    llm.push_text("first reply");
    let (server, _home) =
        two_workspace_server(default_root.path(), second_root.path(), Arc::clone(&llm));
    let (session_id, _session) =
        create_session_on_second_workspace(&server, second_root.path()).await;
    server
        .call_method(
            "message.send",
            json!({ "session_id": &session_id, "text": "hi", "mode": "wait" }),
        )
        .await
        .expect("the routed turn completes");

    // When the session is closed through the dispatch surface
    server
        .call_method("session.close", json!({ "session_id": &session_id }))
        .await
        .expect("the routed close succeeds");

    // Then the second workspace's manager no longer holds it live
    let second_key = mirri_core::encode_work_dir_key(second_root.path());
    let runtime = server
        .registry
        .ensure_built(&second_key)
        .expect("the second runtime stays registered")
        .expect("the second runtime is built");
    assert!(
        runtime
            .manager
            .get_session(&SessionId::new(&session_id))
            .await
            .is_none(),
        "the close ran on the owning workspace's manager"
    );

    // And when the session is resumed through the dispatch surface, then the
    // replay comes back live on the second workspace's manager
    let resumed = server
        .call_method("session.resume", json!({ "session_id": &session_id }))
        .await
        .expect("the routed resume replays the second bucket's wire log");
    assert_eq!(resumed["id"], json!(session_id));
    assert!(
        runtime
            .manager
            .get_session(&SessionId::new(&session_id))
            .await
            .is_some(),
        "the resumed session is live on its owning workspace's manager"
    );
    // And the default workspace's manager still never sees it
    assert!(
        server
            .session_manager()
            .get_session(&SessionId::new(&session_id))
            .await
            .is_none(),
        "the default workspace never adopts the foreign session"
    );
}

/// Given sessions persisted in BOTH registered workspaces' buckets before
/// the first RPC, when the one-time index reconcile runs, then each bucket
/// converges — both rows enter the index under their owning workspace keys
/// (the routing seam's level-2 fallback reads exactly these rows).
#[tokio::test]
async fn should_reconcile_the_index_across_both_workspaces_buckets() {
    // Given a two-workspace server (no runtime of the second workspace built
    // yet — the converge must not need one)
    let default_root = tempfile::tempdir().expect("default root tempdir");
    let second_root = tempfile::tempdir().expect("second root tempdir");
    let llm = Arc::new(ScriptedLlm::new());
    let (server, _home) =
        two_workspace_server(default_root.path(), second_root.path(), Arc::clone(&llm));

    // And a session in each bucket, both created directly on their
    // managers (before any RPC, so neither row was written through)
    let now = chrono::Utc::now();
    let metadata = || SessionMetadata {
        title: "converge".to_string(),
        created_at: now,
        updated_at: now,
        tags: Default::default(),
        archived: false,
        is_custom_title: false,
        work_dir: None,
        last_prompt: None,
        forked_from: None,
        cache_root_id: None,
        custom: None,
    };
    let default_session = server
        .session_manager()
        .create_session(metadata(), SessionScopeConfig::default())
        .await
        .expect("the default bucket's session is created");
    let (second_session_id, _session) =
        create_session_on_second_workspace(&server, second_root.path()).await;

    // When the first RPC runs (the one-time reconcile seam)
    server
        .call_method("workspace.list", json!({}))
        .await
        .expect("the first RPC succeeds");

    // Then both buckets' rows entered the index under their owning keys
    let index = server.session_index().expect("the index is open");
    let second_key = mirri_core::encode_work_dir_key(second_root.path());
    let default_key = mirri_core::encode_work_dir_key(default_root.path());
    let second_row = index
        .get_session(&second_session_id)
        .expect("the second workspace's row converged");
    assert_eq!(second_row.workspace_id, second_key);
    let default_row = index
        .get_session(default_session.id().as_str())
        .expect("the default workspace's row converged");
    assert_eq!(default_row.workspace_id, default_key);
}

// --- W4: workspace-routed session.create ---------------------------------

/// Given a registered second workspace, when `session.create` carries its
/// id (the web dual-send shape: `workspace_id` + `metadata.cwd`), then the
/// whole construction chain runs on THAT workspace — the session directory
/// lands in its bucket, the default manager never sees the session, the
/// recorded workDir and the wire view's cwd are its root, a message
/// round-trips through the routing seam, `session.list`'s live union
/// surfaces it, and a close→resume replays ITS wire log. This is the W4
/// user-perceivable acceptance, pinned end-to-end.
#[tokio::test]
async fn should_route_session_create_to_the_requested_workspace() {
    // Given a two-workspace server with a scripted LLM
    let default_root = tempfile::tempdir().expect("default root tempdir");
    let second_root = tempfile::tempdir().expect("second root tempdir");
    let second_root_str = second_root.path().to_string_lossy().into_owned();
    let llm = Arc::new(ScriptedLlm::new());
    llm.push_text("routed create reply");
    let (server, home) =
        two_workspace_server(default_root.path(), second_root.path(), Arc::clone(&llm));
    let second_key = mirri_core::encode_work_dir_key(second_root.path());

    // When a session is created with the second workspace's id (the web
    // dual-send shape)
    let created = server
        .call_method(
            "session.create",
            json!({
                "title": "routed create",
                "workspace_id": second_key.as_str(),
                "metadata": { "cwd": second_root_str.as_str() },
            }),
        )
        .await
        .expect("the routed session.create succeeds");
    let session_id = created["id"]
        .as_str()
        .expect("the create returns the session id")
        .to_string();

    // Then the session lives on the SECOND workspace's runtime — the
    // default manager never sees it
    let second_runtime = server
        .registry
        .ensure_built(&second_key)
        .expect("the second workspace row is registered")
        .expect("the second workspace runtime is built");
    let session = second_runtime
        .manager
        .get_session(&SessionId::new(&session_id))
        .await
        .expect("the routed session is live on the second workspace");
    assert!(
        server
            .session_manager()
            .get_session(&SessionId::new(&session_id))
            .await
            .is_none(),
        "the default workspace never adopts the routed session"
    );

    // And its directory landed under the second workspace's bucket (disk
    // truth — the bucket key spelling `wd_<slug>…` the TS layout uses).
    // SDR-004: the LAZY creation wrote nothing yet — the first user message
    // below materializes the directory, and the assertion follows it.
    let bucket_session_dir = home
        .path()
        .join("v3_sessions")
        .join(second_key.as_str())
        .join(&session_id);

    // And the recorded workDir is the second workspace's own root (never
    // the process default root)
    let metadata = session.metadata().await;
    assert_eq!(
        metadata.work_dir.as_deref(),
        Some(second_root_str.as_str()),
        "work_dir is the session's own workspace root"
    );

    // And the create's own wire view reports the routed workspace's cwd
    assert_eq!(created["cwd"].as_str(), Some(second_root_str.as_str()));

    // When a wait-mode message is sent to the routed session, then the turn
    // runs on the second workspace's own agent — and the send MATERIALIZED
    // the session (SDR-004): the directory, the index row, and the
    // last_prompt all appear now, never at create.
    assert!(
        !bucket_session_dir.exists(),
        "the lazy create left no session directory behind"
    );
    let index = server.session_index().expect("the index is open");
    assert!(
        index.get_session(&session_id).is_none(),
        "the lazy create wrote no index row"
    );
    let result = server
        .call_method(
            "message.send",
            json!({ "session_id": &session_id, "text": "hi", "mode": "wait" }),
        )
        .await
        .expect("the routed turn completes");
    assert_eq!(result["turn"]["stopReason"], "end_turn");
    assert_eq!(llm.chat_call_count(), 1, "the scripted LLM was consulted");
    assert!(
        bucket_session_dir.join("state.json").is_file(),
        "the session dir landed in the routed workspace's bucket: {}",
        bucket_session_dir.display()
    );

    // And the materialization attributed the row to the routed workspace,
    // carrying the send's last_prompt
    let row = index
        .get_session(&session_id)
        .expect("the materialized session wrote its row through");
    assert_eq!(row.workspace_id, second_key);
    assert_eq!(row.last_prompt.as_deref(), Some("hi"));

    // And `session.list`'s live union surfaces the routed session with
    // cwd = the second workspace's root
    let listed = server
        .call_method("session.list", json!({}))
        .await
        .expect("session.list succeeds");
    let row = listed["items"]
        .as_array()
        .expect("session.list returns the page envelope's items")
        .iter()
        .find(|row| row["id"] == json!(&session_id))
        .expect("the live union lists the routed session")
        .clone();
    assert_eq!(row["cwd"].as_str(), Some(second_root_str.as_str()));
    assert_eq!(row["lastPrompt"].as_str(), Some("hi"));

    // And after a close, a resume replays the SECOND workspace's wire log
    server
        .call_method("session.close", json!({ "session_id": &session_id }))
        .await
        .expect("the routed close succeeds");
    assert!(
        second_runtime
            .manager
            .get_session(&SessionId::new(&session_id))
            .await
            .is_none(),
        "the close ran on the owning workspace's manager"
    );
    let resumed = server
        .call_method("session.resume", json!({ "session_id": &session_id }))
        .await
        .expect("the routed resume replays the second bucket's wire log");
    assert_eq!(resumed["id"], json!(session_id));
    assert!(
        second_runtime
            .manager
            .get_session(&SessionId::new(&session_id))
            .await
            .is_some(),
        "the resumed session is live on its owning workspace's manager"
    );
}

/// Given an unregistered workspace id, when `session.create` carries it,
/// then the RPC fails with `invalid params` (never a silent fallback to the
/// default workspace) and no session is created anywhere.
#[tokio::test]
async fn should_reject_session_create_for_an_unknown_workspace() {
    let default_root = tempfile::tempdir().expect("default root tempdir");
    let second_root = tempfile::tempdir().expect("second root tempdir");
    let llm = Arc::new(ScriptedLlm::new());
    let (server, _home) =
        two_workspace_server(default_root.path(), second_root.path(), Arc::clone(&llm));

    let error = server
        .call_method(
            "session.create",
            json!({ "workspace_id": "wd_unknown_deadbeef0000" }),
        )
        .await
        .expect_err("an unknown workspace id is rejected");

    let EngineError::Rpc { code, message, .. } = &error else {
        panic!("expected an rpc error, got: {error}");
    };
    assert_eq!(*code, mirri_proto::rpc::JSON_RPC_INVALID_PARAMS);
    assert!(
        message.contains("does not exist"),
        "the message mirrors the TS route's spelling: {message}"
    );

    // Never a silent fallback: nothing was created anywhere.
    let listed = server
        .call_method("session.list", json!({}))
        .await
        .expect("session.list succeeds");
    assert_eq!(
        listed["items"].as_array().map(Vec::len),
        Some(0),
        "no session was created on any workspace"
    );
}

/// Given the dual-send pair, when `metadata.cwd` disagrees with the routed
/// workspace's root, then the RPC fails with `invalid params` — a
/// mismatched pair must never mis-attribute the session (the TS v2
/// route's parity check).
#[tokio::test]
async fn should_reject_a_mismatched_cwd_workspace_pair() {
    let default_root = tempfile::tempdir().expect("default root tempdir");
    let second_root = tempfile::tempdir().expect("second root tempdir");
    let llm = Arc::new(ScriptedLlm::new());
    let (server, _home) =
        two_workspace_server(default_root.path(), second_root.path(), Arc::clone(&llm));
    let second_key = mirri_core::encode_work_dir_key(second_root.path());

    let error = server
        .call_method(
            "session.create",
            json!({
                "workspace_id": second_key.as_str(),
                "metadata": { "cwd": "/definitely/not/the/second/root" },
            }),
        )
        .await
        .expect_err("a mismatched pair is rejected");

    let EngineError::Rpc { code, message, .. } = &error else {
        panic!("expected an rpc error, got: {error}");
    };
    assert_eq!(*code, mirri_proto::rpc::JSON_RPC_INVALID_PARAMS);
    assert!(
        message.contains("must equal workspace root"),
        "the message mirrors the TS route's spelling: {message}"
    );
}

/// Given no workspace id at all, when `session.create` runs, then it lands
/// on the default workspace exactly as before (zero break for existing
/// callers) — the camelCase alias resolves the same workspace too.
#[tokio::test]
async fn should_keep_session_create_on_the_default_workspace_without_an_id() {
    let default_root = tempfile::tempdir().expect("default root tempdir");
    let second_root = tempfile::tempdir().expect("second root tempdir");
    let llm = Arc::new(ScriptedLlm::new());
    let (server, _home) =
        two_workspace_server(default_root.path(), second_root.path(), Arc::clone(&llm));

    let created = server
        .call_method("session.create", json!({ "title": "plain create" }))
        .await
        .expect("the id-less create succeeds");
    let session_id = created["id"].as_str().expect("the id").to_string();
    assert!(
        server
            .session_manager()
            .get_session(&SessionId::new(&session_id))
            .await
            .is_some(),
        "the default workspace keeps the id-less create"
    );
    assert_eq!(
        created["cwd"].as_str(),
        Some(default_root.path().to_string_lossy().as_ref()),
        "the default root is echoed as the session's own workspace"
    );

    // And the camelCase alias routes the second workspace the same way
    let second_key = mirri_core::encode_work_dir_key(second_root.path());
    let aliased = server
        .call_method(
            "session.create",
            json!({ "workspaceId": second_key.as_str() }),
        )
        .await
        .expect("the camelCase alias is accepted");
    assert!(
        server
            .session_manager()
            .get_session(&SessionId::new(aliased["id"].as_str().expect("the id")))
            .await
            .is_none(),
        "the aliased create routes away from the default"
    );
}

// --- W5: session.list/search 多工作区化 (the acceptance face) ------------

/// The W5 fixture's materialization step: create a session through the RPC
/// face and drive one scripted turn — the first user input lands the
/// session directory, the index row, and the `last_prompt` (SDR-004).
async fn create_and_converse(
    server: &EmbeddedServer,
    create_params: serde_json::Value,
    prompt: &str,
) -> String {
    let created = server
        .call_method("session.create", create_params)
        .await
        .expect("session.create succeeds");
    let session_id = created["id"].as_str().expect("the id").to_string();
    let result = server
        .call_method(
            "message.send",
            json!({ "session_id": &session_id, "text": prompt, "mode": "wait" }),
        )
        .await
        .expect("the scripted turn completes");
    assert_eq!(result["turn"]["stopReason"], "end_turn");
    session_id
}

/// The session ids of one `session.list` page, in wire order.
fn listed_ids(page: &serde_json::Value) -> Vec<String> {
    page["items"]
        .as_array()
        .expect("session.list returns the page envelope's items")
        .iter()
        .map(|row| {
            row["id"]
                .as_str()
                .expect("every row carries its id")
                .to_string()
        })
        .collect()
}

/// Given sessions materialized in BOTH workspace buckets (plus one
/// never-conversed straggler), when `session.list` runs, then the merged
/// listing answers the full W5 face: the global `(updated_at DESC, id DESC)`
/// order, the `exclude_empty` hide (SDR-004), the alias-folded workspace
/// filter, the unknown-workspace rejection, the keyset cursors, the busy
/// post-page filter, and the archive triad (default exclude / include /
/// archived-only).
#[tokio::test]
async fn should_list_and_page_sessions_across_both_workspaces_buckets() {
    // Given a two-workspace server with a scripted LLM (three turns)
    let default_root = tempfile::tempdir().expect("default root tempdir");
    let second_root = tempfile::tempdir().expect("second root tempdir");
    let llm = Arc::new(ScriptedLlm::new());
    llm.push_text("first reply");
    llm.push_text("second reply");
    llm.push_text("fourth reply");
    let (server, _home) =
        two_workspace_server(default_root.path(), second_root.path(), Arc::clone(&llm));
    let second_key = mirri_core::encode_work_dir_key(second_root.path());
    let default_key = mirri_core::encode_work_dir_key(default_root.path());

    // And four sessions: a never-conversed straggler in the second bucket
    // (created first — the oldest), two conversed ones (one per bucket),
    // and the newest conversed one in the default bucket.
    let straggler = server
        .call_method(
            "session.create",
            json!({ "workspace_id": second_key.as_str(), "title": "straggler" }),
        )
        .await
        .expect("the straggler's create succeeds")["id"]
        .as_str()
        .expect("the id")
        .to_string();
    let first = create_and_converse(&server, json!({ "title": "first" }), "alpha prompt").await;
    let second = create_and_converse(
        &server,
        json!({ "workspace_id": second_key.as_str(), "title": "second" }),
        "beta prompt",
    )
    .await;
    let newest = create_and_converse(&server, json!({ "title": "newest" }), "gamma prompt").await;

    // Then the unfiltered listing is the merged global order (newest
    // first), the never-conversed straggler included, and no page size
    // answers everything with has_more false (the TS `pageSize ??
    // visible.length`).
    let listed = server
        .call_method("session.list", json!({}))
        .await
        .expect("session.list succeeds");
    assert_eq!(
        listed_ids(&listed),
        vec![
            newest.as_str(),
            second.as_str(),
            first.as_str(),
            straggler.as_str()
        ]
    );
    assert_eq!(listed["has_more"], json!(false));

    // And every conversed row carries its last prompt (the index's
    // `last_prompt` surfaced on the view); the straggler carries none.
    let row_of = |page: &serde_json::Value, id: &str| {
        page["items"]
            .as_array()
            .expect("items")
            .iter()
            .find(|row| row["id"] == json!(id))
            .unwrap_or_else(|| panic!("the row {id} must be listed"))
            .clone()
    };
    assert_eq!(row_of(&listed, &first)["lastPrompt"], json!("alpha prompt"));
    assert_eq!(row_of(&listed, &second)["lastPrompt"], json!("beta prompt"));
    assert_eq!(
        row_of(&listed, &straggler)["lastPrompt"],
        serde_json::Value::Null
    );

    // And `exclude_empty` hides the never-conversed session (SDR-004 —
    // a session with no last_prompt is memory-only, not "listed as empty").
    let listed = server
        .call_method("session.list", json!({ "exclude_empty": true }))
        .await
        .expect("session.list succeeds");
    assert_eq!(
        listed_ids(&listed),
        vec![newest.as_str(), second.as_str(), first.as_str()]
    );

    // And the workspace filter scopes to one bucket's alias set.
    let listed = server
        .call_method(
            "session.list",
            json!({ "workspace_id": second_key.as_str() }),
        )
        .await
        .expect("session.list succeeds");
    assert_eq!(
        listed_ids(&listed),
        vec![second.as_str(), straggler.as_str()]
    );
    let listed = server
        .call_method(
            "session.list",
            json!({ "workspaceId": default_key.as_str(), "exclude_empty": true }),
        )
        .await
        .expect("session.list succeeds");
    assert_eq!(listed_ids(&listed), vec![newest.as_str(), first.as_str()]);

    // And an unknown workspace id is rejected with the TS route's
    // spelling (never a silent empty list).
    let error = server
        .call_method(
            "session.list",
            json!({ "workspace_id": "wd_unknown_deadbeef0000" }),
        )
        .await
        .expect_err("an unknown workspace id is rejected");
    let EngineError::Rpc { code, message, .. } = &error else {
        panic!("expected an rpc error, got: {error}");
    };
    assert_eq!(*code, mirri_proto::rpc::JSON_RPC_INVALID_PARAMS);
    assert!(
        message.contains("does not exist"),
        "the message mirrors the TS route's spelling: {message}"
    );

    // And the keyset cursors page the global order: `page_size` first…
    let page_one = server
        .call_method("session.list", json!({ "page_size": 2 }))
        .await
        .expect("the first page succeeds");
    assert_eq!(
        listed_ids(&page_one),
        vec![newest.as_str(), second.as_str()]
    );
    assert_eq!(page_one["has_more"], json!(true));
    // …`before_id` continues strictly older…
    let page_two = server
        .call_method(
            "session.list",
            json!({ "page_size": 2, "before_id": second.as_str() }),
        )
        .await
        .expect("the second page succeeds");
    assert_eq!(
        listed_ids(&page_two),
        vec![first.as_str(), straggler.as_str()]
    );
    assert_eq!(page_two["has_more"], json!(false));
    // …`after_id` walks strictly newer…
    let back = server
        .call_method("session.list", json!({ "after_id": second.as_str() }))
        .await
        .expect("the backward page succeeds");
    assert_eq!(listed_ids(&back), vec![newest.as_str()]);
    // …and an unknown cursor answers the empty terminal page (a client
    // cannot spin on a cursor the server cannot advance).
    let terminal = server
        .call_method("session.list", json!({ "before_id": "wd_missing" }))
        .await
        .expect("the unknown cursor still answers a page");
    assert_eq!(listed_ids(&terminal), Vec::<String>::new());
    assert_eq!(terminal["has_more"], json!(false));

    // And the busy filter is post-page on ordinary lists (an idle
    // full page stays a full page, has_more intact).
    let idle = server
        .call_method("session.list", json!({ "page_size": 2, "busy": false }))
        .await
        .expect("the busy-filtered page succeeds");
    assert_eq!(listed_ids(&idle), vec![newest.as_str(), second.as_str()]);
    assert_eq!(idle["has_more"], json!(true));

    // And the archive triad: the default listing excludes archived rows…
    server
        .call_method("session.archive", json!({ "session_id": newest.as_str() }))
        .await
        .expect("the archive succeeds");
    let listed = server
        .call_method("session.list", json!({}))
        .await
        .expect("session.list succeeds");
    assert_eq!(
        listed_ids(&listed),
        vec![second.as_str(), first.as_str(), straggler.as_str()]
    );
    // …`include_archive` opts them back in…
    let listed = server
        .call_method("session.list", json!({ "include_archive": true }))
        .await
        .expect("session.list succeeds");
    assert_eq!(
        listed_ids(&listed),
        vec![
            newest.as_str(),
            second.as_str(),
            first.as_str(),
            straggler.as_str()
        ]
    );
    // …and `archived_only` drains to exactly the archived set.
    let listed = server
        .call_method("session.list", json!({ "archived_only": true }))
        .await
        .expect("session.list succeeds");
    assert_eq!(listed_ids(&listed), vec![newest.as_str()]);
}

/// Given conversed sessions in both workspace buckets, when
/// `session.search` runs, then the hits attribute each session to the
/// workspace whose bucket OWNS it (the true attribution — the default
/// bucket's sessions name the default workspace, the routed ones name the
/// second), never the bound root.
#[tokio::test]
async fn should_attribute_search_hits_to_the_workspace_owning_each_session() {
    // Given a two-workspace server with a scripted LLM (two turns)
    let default_root = tempfile::tempdir().expect("default root tempdir");
    let second_root = tempfile::tempdir().expect("second root tempdir");
    let llm = Arc::new(ScriptedLlm::new());
    llm.push_text("default bucket reply");
    llm.push_text("second bucket reply");
    let (server, _home) =
        two_workspace_server(default_root.path(), second_root.path(), Arc::clone(&llm));
    let second_key = mirri_core::encode_work_dir_key(second_root.path());
    let default_key = mirri_core::encode_work_dir_key(default_root.path());

    // And a conversed session in each bucket, each with a unique token
    let default_session = create_and_converse(
        &server,
        json!({ "title": "the default bucket session" }),
        "the xylophone draft lives here",
    )
    .await;
    let second_session = create_and_converse(
        &server,
        json!({ "workspace_id": second_key.as_str(), "title": "the second bucket session" }),
        "the zebra draft lives there",
    )
    .await;

    // When the search crosses the buckets, then each hit names its owning
    // workspace (never the bound root).
    let found = server
        .call_method("session.search", json!({ "query": "xylophone" }))
        .await
        .expect("the search succeeds");
    let hits = found["hits"].as_array().expect("the hits array");
    assert_eq!(hits.len(), 1, "one hit: {found}");
    assert_eq!(hits[0]["session_id"], json!(default_session));
    assert_eq!(hits[0]["workspace_id"], json!(default_key));

    let found = server
        .call_method("session.search", json!({ "query": "zebra" }))
        .await
        .expect("the search succeeds");
    let hits = found["hits"].as_array().expect("the hits array");
    assert_eq!(hits.len(), 1, "one hit: {found}");
    assert_eq!(hits[0]["session_id"], json!(second_session));
    assert_eq!(hits[0]["workspace_id"], json!(second_key));

    // And a query only the default bucket answers stays scoped honestly
    // (no cross-bucket bleed in either direction).
    let found = server
        .call_method(
            "session.search",
            json!({ "query": "xylophone", "container": { "sessionId": second_session } }),
        )
        .await
        .expect("the container-scoped search succeeds");
    assert_eq!(
        found["hits"].as_array().map(Vec::len),
        Some(0),
        "the container scope keeps the hit out: {found}"
    );
}

/// Given the embedded-host constructor (`for_workspace_root` — the
/// NAPI/ACP pin), when `session.list` / `session.search` run with no
/// explicit workspace filter, then the default scope is pinned to the
/// default workspace only (the second bucket's sessions are invisible),
/// while an explicit `workspace_id` filter still crosses the pin.
#[tokio::test]
async fn should_pin_the_embedded_hosts_listing_to_the_default_workspace() {
    // Given a two-workspace server left on the embedded host's own pin
    let default_root = tempfile::tempdir().expect("default root tempdir");
    let second_root = tempfile::tempdir().expect("second root tempdir");
    let llm = Arc::new(ScriptedLlm::new());
    llm.push_text("visible reply");
    llm.push_text("hidden reply");
    let (server, _home) = two_workspace_server_with_visibility(
        default_root.path(),
        second_root.path(),
        Arc::clone(&llm),
        super::ListingVisibility::PinnedToDefault,
    );
    let second_key = mirri_core::encode_work_dir_key(second_root.path());

    // And a conversed session in each bucket
    let visible = create_and_converse(
        &server,
        json!({ "title": "the visible session" }),
        "the visible prompt",
    )
    .await;
    let hidden = create_and_converse(
        &server,
        json!({ "workspace_id": second_key.as_str(), "title": "the hidden session" }),
        "the hidden zebra prompt",
    )
    .await;

    // Then the default listing scope is pinned: only the default
    // workspace's sessions answer (the second bucket stays invisible).
    let listed = server
        .call_method("session.list", json!({}))
        .await
        .expect("session.list succeeds");
    assert_eq!(listed_ids(&listed), vec![visible.as_str()]);

    // And the search scan is pinned the same way: the second bucket's
    // unique token is unreachable, the default's still answers.
    let found = server
        .call_method("session.search", json!({ "query": "zebra" }))
        .await
        .expect("the search succeeds");
    assert_eq!(
        found["hits"].as_array().map(Vec::len),
        Some(0),
        "the pinned scope never scans the second bucket: {found}"
    );
    let found = server
        .call_method("session.search", json!({ "query": "visible" }))
        .await
        .expect("the search succeeds");
    let hits = found["hits"].as_array().expect("the hits array");
    assert!(
        !hits.is_empty(),
        "the pinned scope still scans the default bucket: {found}"
    );
    assert!(
        hits.iter().all(|hit| hit["session_id"] == json!(visible)),
        "every hit is the default bucket's own session: {found}"
    );

    // But an explicit workspace filter crosses the pin (the client asked
    // for that workspace by name).
    let listed = server
        .call_method(
            "session.list",
            json!({ "workspace_id": second_key.as_str() }),
        )
        .await
        .expect("the explicit filtered list succeeds");
    assert_eq!(listed_ids(&listed), vec![hidden.as_str()]);
}

/// Given never-conversed sessions (SDR-004's lazy creation — no first user
/// input), when the server is dropped (the process-exit analogue), then
/// nothing landed: no session directory under any bucket and no index row
/// anywhere.
#[tokio::test]
async fn should_leave_nothing_behind_when_never_conversed_sessions_are_dropped() {
    // Given a two-workspace server and one never-conversed session per
    // bucket (lazy creation — memory-only)
    let default_root = tempfile::tempdir().expect("default root tempdir");
    let second_root = tempfile::tempdir().expect("second root tempdir");
    let llm = Arc::new(ScriptedLlm::new());
    let (server, home) =
        two_workspace_server(default_root.path(), second_root.path(), Arc::clone(&llm));
    let second_key = mirri_core::encode_work_dir_key(second_root.path());
    let default_key = mirri_core::encode_work_dir_key(default_root.path());

    let default_session = server
        .call_method("session.create", json!({ "title": "never conversed" }))
        .await
        .expect("the default bucket's create succeeds")["id"]
        .as_str()
        .expect("the id")
        .to_string();
    let second_session = server
        .call_method(
            "session.create",
            json!({ "workspace_id": second_key.as_str(), "title": "never conversed" }),
        )
        .await
        .expect("the second bucket's create succeeds")["id"]
        .as_str()
        .expect("the id")
        .to_string();

    // Then neither session materialized: no directory under any bucket…
    let sessions_root = home.path().join("v3_sessions");
    for (key, id) in [
        (&default_key, &default_session),
        (&second_key, &second_session),
    ] {
        assert!(
            !sessions_root.join(key.as_str()).join(id).exists(),
            "the never-conversed session left no directory behind"
        );
    }
    // …and no index row anywhere (the live-union view does surface them —
    // the memory-only rows — but the durable index never saw them).
    let index = server.session_index().expect("the index is open");
    assert!(index.get_session(&default_session).is_none());
    assert!(index.get_session(&second_session).is_none());

    // When the server is dropped (the process-exit analogue), then still
    // nothing landed: no session files under the sessions tree at all.
    drop(server);
    fn collect_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                collect_files(&path, out);
            } else {
                out.push(path);
            }
        }
    }
    let mut landed_files = Vec::new();
    if sessions_root.exists() {
        collect_files(&sessions_root, &mut landed_files);
    }
    // The startup orphan census's archived reports live under
    // `orphan-sweep/` — the governance plane's own bookkeeping (US14.6),
    // never session data; the leak assertion is about session files.
    landed_files.retain(|path| {
        !path
            .components()
            .any(|component| component.as_os_str() == "orphan-sweep")
    });
    assert!(
        landed_files.is_empty(),
        "the dropped never-conversed sessions left files behind: {landed_files:?}"
    );
}

/// Given a second registration whose root string is a different spelling
/// of the default workspace's root (a D3 alias — the same physical
/// directory under another work-dir key), when `session.list` filters by
/// either id and `workspace.list` counts, then the alias set folds: the
/// alias spelling's filter lists the default bucket's sessions, and every
/// row of the group reports the same folded session count.
#[tokio::test]
async fn should_fold_alias_workspace_spellings_into_one_listing_scope() {
    // Given a two-workspace server whose second row addresses the default
    // root's trailing-backslash spelling — a distinct work-dir key (a
    // distinct row) whose root folds with the default's under the
    // comparison-only key (the `workspaceRootKey` parity: `\` → `/`, then
    // the trailing separator strips).
    let default_root = tempfile::tempdir().expect("default root tempdir");
    let alias_root = std::path::PathBuf::from(format!("{}\\", default_root.path().display()));
    let llm = Arc::new(ScriptedLlm::new());
    llm.push_text("folded reply");
    let (server, _home) = two_workspace_server(default_root.path(), &alias_root, Arc::clone(&llm));
    let alias_key = mirri_core::encode_work_dir_key(&alias_root);
    let default_key = mirri_core::encode_work_dir_key(default_root.path());
    assert_ne!(
        alias_key, default_key,
        "the alias spelling registers as its own row"
    );

    // And one conversed session on the DEFAULT workspace
    let session = create_and_converse(
        &server,
        json!({ "title": "the folded session" }),
        "the folded prompt",
    )
    .await;

    // Then the alias spelling's filter lists the default bucket's session
    // (the D3 fold — one physical directory, one listing scope).
    let listed = server
        .call_method(
            "session.list",
            json!({ "workspace_id": alias_key.as_str() }),
        )
        .await
        .expect("the alias-filtered list succeeds");
    assert_eq!(
        listed_ids(&listed),
        vec![session.as_str()],
        "the alias id folds onto the default bucket's scope"
    );

    // And the default's own filter keeps answering the same scope.
    let listed = server
        .call_method(
            "session.list",
            json!({ "workspace_id": default_key.as_str() }),
        )
        .await
        .expect("the default-filtered list succeeds");
    assert_eq!(listed_ids(&listed), vec![session.as_str()]);

    // And `workspace.list` folds the count: BOTH rows of the alias set
    // report the same session count (a legacy split bucket counts with
    // its siblings — never double, never zero).
    let workspaces = server
        .call_method("workspace.list", json!({}))
        .await
        .expect("workspace.list succeeds");
    let rows = workspaces
        .as_array()
        .expect("workspace.list returns its rows");
    let count_of = |workspace_id: &str| {
        rows.iter()
            .find(|row| row["id"] == json!(workspace_id))
            .unwrap_or_else(|| panic!("the row {workspace_id} must be listed"))["sessionCount"]
            .as_u64()
            .expect("sessionCount is a number")
    };
    assert_eq!(count_of(default_key.as_str()), 1);
    assert_eq!(
        count_of(alias_key.as_str()),
        1,
        "the alias row's count folds over the shared bucket: {workspaces}"
    );
}

// --- W9: closing-batch e2es -----------------------------------------------
//
// The final batch's end-to-end pins: the legacy db migration path (an
// existing installation upgraded in place), the D8 cross-engine write
// coexistence (a simulated TS writer interleaved with live rust
// registrations/unregistrations), and the D3 alias-set convergence over
// forked bucket history.

/// A serve-like server on a throwaway home with an EXPLICIT registry content:
/// every `(id, root)` pair becomes one `workspaces.json` row (the exact shape
/// the TS engine writes), the default workspace is the passed root, and the
/// listing visibility is the serve view (every registered workspace).
fn w9_server(
    default_root: &std::path::Path,
    rows: &[(&str, &str)],
) -> (EmbeddedServer, tempfile::TempDir) {
    let home = tempfile::tempdir().expect("home tempdir");
    let mut workspaces = serde_json::Map::new();
    for (id, root) in rows {
        workspaces.insert(
            (*id).to_string(),
            json!({
                "root": root,
                "name": "w9",
                "created_at": "2024-01-01T00:00:00.000Z",
                "last_opened_at": "2024-06-01T00:00:00.000Z",
            }),
        );
    }
    std::fs::write(
        home.path().join("workspaces.json"),
        serde_json::to_string(&json!({
            "version": 1,
            "workspaces": Value::Object(workspaces),
            "deleted_workspace_ids": [],
        }))
        .expect("registry json serializes"),
    )
    .expect("workspaces.json written");
    let server = w9_boot(home.path(), default_root);
    (server, home)
}

/// Boot a serve-like server on an EXISTING home — the registry file is left
/// exactly as it is (a reboot of a home whose catalog the test or a foreign
/// writer shaped).
fn w9_boot(home: &std::path::Path, default_root: &std::path::Path) -> EmbeddedServer {
    let mut server = EmbeddedServer::for_workspace_root(
        WorkspaceId::new("w9-default"),
        default_root.to_path_buf(),
        Some(home.to_path_buf()),
        Vec::new(),
    );
    server.listing_visibility = super::ListingVisibility::AllRegistered;
    server
}

/// The workspace ids the home's `workspaces.json` currently carries.
fn registry_ids(registry_path: &std::path::Path) -> Vec<String> {
    let mut ids: Vec<String> = mirri_session_index::read_workspace_registry(registry_path)
        .into_iter()
        .map(|row| row.id)
        .collect();
    ids.sort();
    ids
}

/// The raw registry document (tombstone-list assertions read it directly).
fn registry_document(registry_path: &std::path::Path) -> Value {
    serde_json::from_str(
        &std::fs::read_to_string(registry_path).expect("workspaces.json is readable"),
    )
    .expect("workspaces.json parses")
}

/// Simulate the TS writer — `FileWorkspacePersistence.save`
/// (`packages/agent-core-v2/src/app/workspace/fileWorkspacePersistence.ts:67-82`):
/// a WHOLE-DOCUMENT replacement with **no lock and no merge**, carrying the
/// caller's rows and tombstone list verbatim (an atomic replace only). This
/// is exactly the writer shape the design's §7.3 residual window documents.
fn ts_save_whole_document(
    registry_path: &std::path::Path,
    rows: &[mirri_session_index::WorkspaceRow],
    deleted_ids: &[String],
) {
    let mut workspaces = serde_json::Map::new();
    for row in rows {
        workspaces.insert(
            row.id.clone(),
            json!({
                "root": row.root,
                "name": row.name,
                "created_at": row.created_at.to_rfc3339(),
                "last_opened_at": row.last_opened_at.to_rfc3339(),
            }),
        );
    }
    let document = json!({
        "version": 1,
        "workspaces": Value::Object(workspaces),
        "deleted_workspace_ids": deleted_ids,
    });
    mirri_persistence::write_file_atomically(
        registry_path,
        serde_json::to_string_pretty(&document)
            .expect("the TS document serializes")
            .as_bytes(),
    )
    .expect("the TS writer's atomic replace lands");
}

/// One minimal, valid `state.json` — the on-disk shape a persisted session
/// directory of an existing installation carries.
fn seed_state_json(home: &std::path::Path, workspace_key: &str, session_id: &str, title: &str) {
    let dir = home
        .join("v3_sessions")
        .join(workspace_key)
        .join(session_id);
    std::fs::create_dir_all(&dir).expect("the session dir is created");
    let now = chrono::Utc::now().to_rfc3339();
    std::fs::write(
        dir.join("state.json"),
        serde_json::to_string(&json!({
            "title": title,
            "createdAt": now,
            "updatedAt": now,
        }))
        .expect("state.json serializes"),
    )
    .expect("state.json is written");
}

/// Given an existing installation's home — a pre-W5 (v0) `session-index.db`
/// (no `last_prompt` column) whose row points at a session directory that
/// still exists on disk, when the engine boots on that home and serves its
/// first RPC, then the migration steps the database to v1 in place, the old
/// row stays readable, and `session.list` surfaces it (the upgrade path W5's
/// unit test pins at the index level, exercised through the engine's open +
/// reconcile seams).
#[tokio::test]
async fn should_migrate_a_v0_session_index_database_when_the_engine_boots_on_an_existing_installation()
 {
    let home = tempfile::tempdir().expect("home tempdir");
    let default_root = tempfile::tempdir().expect("default root tempdir");
    let default_key = mirri_core::encode_work_dir_key(default_root.path());
    seed_state_json(home.path(), &default_key, "sess-old", "the legacy session");
    {
        // The v0 database: the pre-W5 schema (no `last_prompt` column), one
        // row attributed to the default workspace's bucket.
        let conn = rusqlite::Connection::open(home.path().join("session-index.db"))
            .expect("the v0 db opens");
        conn.execute_batch(
            "CREATE TABLE sessions (
                id          TEXT PRIMARY KEY NOT NULL,
                workspace_id TEXT NOT NULL,
                cwd         TEXT,
                title       TEXT,
                created_at  TEXT NOT NULL,
                updated_at  TEXT NOT NULL,
                archived    INTEGER NOT NULL DEFAULT 0,
                tags        TEXT,
                custom      TEXT,
                state_mtime INTEGER
            );",
        )
        .expect("the v0 schema is created");
        let now = chrono::Utc::now().to_rfc3339();
        conn.execute(
            "INSERT INTO sessions (id, workspace_id, cwd, title, created_at, updated_at, \
             archived, state_mtime)
             VALUES ('sess-old', ?1, ?2, 'the legacy session', ?3, ?3, 0, NULL)",
            rusqlite::params![default_key, default_root.path().to_string_lossy(), now],
        )
        .expect("the legacy row is inserted");
    }

    // When the engine boots on that home and serves its first RPC (the
    // SessionIndex::open migrate seam + the one-time reconcile)
    let mut server = EmbeddedServer::for_workspace_root(
        WorkspaceId::new("w9-migration"),
        default_root.path().to_path_buf(),
        Some(home.path().to_path_buf()),
        Vec::new(),
    );
    server.listing_visibility = super::ListingVisibility::AllRegistered;
    let listed = server
        .call_method("session.list", json!({}))
        .await
        .expect("the first RPC succeeds");

    // Then the legacy session is readable and listed
    assert_eq!(
        listed_ids(&listed),
        vec!["sess-old"],
        "the migrated row surfaces through session.list: {listed}"
    );
    let row = server
        .session_index()
        .expect("the index is open")
        .get_session("sess-old")
        .expect("the old row survived the migration");
    assert_eq!(row.workspace_id, default_key);
    assert_eq!(row.title.as_deref(), Some("the legacy session"));
    assert!(
        row.last_prompt.is_none(),
        "a v0 row carries no last_prompt (NULL, never a guess)"
    );

    // And the database itself was stepped to schema v1 (an external reader's
    // view — the same upgrade an existing installation takes on first boot)
    let conn =
        rusqlite::Connection::open(home.path().join("session-index.db")).expect("the db reopens");
    let version: i64 = conn
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .expect("user_version reads");
    assert_eq!(version, 1, "the v0 database was migrated to v1 in place");
}

/// Given a home whose `v3_sessions/<bucket>/<sid>/` holds a **TS-written**
/// `state.json` (the `cwd` key, epoch-millisecond timestamps, `lastPrompt`)
/// and NO `session-index.db`, when the engine boots on that home and serves
/// its first `session.list`, then the one-time reconcile's cold-miss backfill
/// (the disk read) surfaces the migrated session with its recorded cwd —
/// never a bound-root backfill — its title and its last prompt (ADR-015:
/// `mirri migrate` relies on reconcile + disk scan, not on a pre-seeded
/// index).
#[tokio::test]
async fn should_list_a_migrated_ts_session_after_reconciling_from_disk() {
    // Given a throwaway home with a TS-shaped session in the default
    // workspace's bucket and no session-index.db (the migrate shape:
    // sessions land in v3_sessions, the index is not pre-seeded)
    let home = tempfile::tempdir().expect("home tempdir");
    let default_root = tempfile::tempdir().expect("default root tempdir");
    let ts_workdir = tempfile::tempdir().expect("the TS session's recorded workdir");
    let ts_cwd = ts_workdir.path().to_string_lossy().into_owned();
    let default_key = mirri_core::encode_work_dir_key(default_root.path());
    let session_id = "ts-migrated-01";
    let session_dir = home
        .path()
        .join("v3_sessions")
        .join(default_key.as_str())
        .join(session_id);
    std::fs::create_dir_all(&session_dir).expect("the session dir is created");
    std::fs::write(
        session_dir.join("state.json"),
        serde_json::to_string(&json!({
            "id": session_id,
            "version": 2,
            "cwd": ts_cwd.as_str(),
            "createdAt": 1788494508725i64,
            "updatedAt": 1788494509999i64,
            "archived": false,
            "lastPrompt": "hello from the TS engine",
            "title": "the migrated TS session",
        }))
        .expect("the TS state.json serializes"),
    )
    .expect("the TS state.json is written");

    // When the engine boots on that home and serves its first RPC (the
    // one-time reconcile seam backfills the row from the disk read)
    let mut server = EmbeddedServer::for_workspace_root(
        WorkspaceId::new("w9-ts-migrated"),
        default_root.path().to_path_buf(),
        Some(home.path().to_path_buf()),
        Vec::new(),
    );
    server.listing_visibility = super::ListingVisibility::AllRegistered;
    let listed = server
        .call_method("session.list", json!({}))
        .await
        .expect("the first RPC succeeds");

    // Then the migrated session is listed with its recorded cwd (the TS
    // `cwd` key — never a backfill of the bound root), title and last prompt
    assert_eq!(
        listed_ids(&listed),
        vec![session_id],
        "the migrated TS session surfaces through session.list: {listed}"
    );
    let row = listed["items"]
        .as_array()
        .expect("items")
        .iter()
        .find(|row| row["id"] == json!(session_id))
        .expect("the migrated session's row is listed")
        .clone();
    assert_eq!(
        row["cwd"].as_str(),
        Some(ts_cwd.as_str()),
        "the recorded TS cwd key is read, not backfilled"
    );
    assert_eq!(row["title"].as_str(), Some("the migrated TS session"));
    assert_eq!(row["lastPrompt"].as_str(), Some("hello from the TS engine"));

    // And the reconcile wrote the row into the (initially empty) SQLite
    // index under the owning workspace
    let index_row = server
        .session_index()
        .expect("the index is open")
        .get_session(session_id)
        .expect("the cold-miss backfill upserted the row");
    assert_eq!(index_row.workspace_id, default_key);
    assert_eq!(index_row.cwd.as_deref(), Some(ts_cwd.as_str()));
}

/// Given a TS engine sharing the home that wrote its own workspace row (the
/// lock-less whole-document save), when the rust engine registers another
/// workspace, then the TS-written row survives the rust write (the D8
/// read-merge-write: local rows ∪ disk rows — a row the rust process never
/// registered is never dropped), and a fresh engine boot on the same home
/// serves it.
#[tokio::test]
async fn should_keep_a_ts_written_row_when_rust_writes_another_registration() {
    let default_root = tempfile::tempdir().expect("default root tempdir");
    let (server, home) = w9_server(default_root.path(), &[]);
    let registry_path = home.path().join("workspaces.json");

    // And a rust registration (the RPC face a serve client drives)
    let ts_root = tempfile::tempdir().expect("ts root tempdir");
    server
        .call_method(
            "workspace.createOrTouch",
            json!({ "root": ts_root.path().to_string_lossy() }),
        )
        .await
        .expect("the rust registration succeeds");

    // And a TS writer saving its own catalog: the document it loaded plus
    // its own new row (no lock, no merge — the documented TS shape)
    let ts_catalog: Vec<mirri_session_index::WorkspaceRow> =
        mirri_session_index::read_workspace_registry(&registry_path)
            .into_iter()
            .chain(std::iter::once(mirri_session_index::WorkspaceRow {
                id: "wd_ts_engine_00000001".to_string(),
                root: Some("/ts/engine/row".to_string()),
                name: Some("ts-engine".to_string()),
                created_at: chrono::Utc::now(),
                last_opened_at: chrono::Utc::now(),
                sort_order: 0,
            }))
            .collect();
    ts_save_whole_document(&registry_path, &ts_catalog, &[]);

    // When the rust engine registers yet another workspace (a merged write)
    let third_root = tempfile::tempdir().expect("third root tempdir");
    let third_key = mirri_core::encode_work_dir_key(third_root.path());
    server
        .call_method(
            "workspace.createOrTouch",
            json!({ "root": third_root.path().to_string_lossy() }),
        )
        .await
        .expect("the later rust registration succeeds");

    // Then the TS-written row survived the rust write (never dropped)
    let ids = registry_ids(&registry_path);
    assert!(
        ids.iter().any(|id| id == "wd_ts_engine_00000001"),
        "the TS-written row survives the rust merged write: {ids:?}"
    );
    assert!(
        ids.contains(&third_key),
        "the later rust registration is on disk too: {ids:?}"
    );

    // And a fresh engine boot on the same home serves the TS row (the
    // cross-engine catalog converges at restart)
    let rebooted = w9_boot(home.path(), default_root.path());
    let workspaces = rebooted
        .call_method("workspace.list", json!({}))
        .await
        .expect("the rebooted engine lists");
    let listed_ids: Vec<String> = workspaces
        .as_array()
        .expect("workspace.list returns rows")
        .iter()
        .map(|row| row["id"].as_str().expect("the id").to_string())
        .collect();
    assert!(
        listed_ids.iter().any(|id| id == "wd_ts_engine_00000001"),
        "the rebooted engine serves the TS-written row: {listed_ids:?}"
    );
}

/// Given the documented TS-side residual window (design §7.3: the TS writer
/// holds no lock and replaces the whole document, so its save may drop rust
/// rows written after its read), when a rust registration lands inside that
/// window, then the rust row IS dropped from the file — the KNOWN behavior,
/// asserted here to document it, never to forbid it — and re-registering the
/// workspace recovers it (the design's documented remedy).
#[tokio::test]
async fn should_expose_the_documented_ts_residual_window_and_recover_by_re_registering() {
    let default_root = tempfile::tempdir().expect("default root tempdir");
    let (server, home) = w9_server(default_root.path(), &[]);
    let registry_path = home.path().join("workspaces.json");

    // And a rust registration the TS writer's catalog predates
    let in_window_root = tempfile::tempdir().expect("in-window root tempdir");
    server
        .call_method(
            "workspace.createOrTouch",
            json!({ "root": in_window_root.path().to_string_lossy() }),
        )
        .await
        .expect("the rust registration succeeds");
    // The TS writer loads its catalog BEFORE the rust row existed…
    let ts_catalog = mirri_session_index::read_workspace_registry(&registry_path);

    // …the rust engine registers another workspace after that read…
    let other_root = tempfile::tempdir().expect("other root tempdir");
    let other_key = mirri_core::encode_work_dir_key(other_root.path());
    server
        .call_method(
            "workspace.createOrTouch",
            json!({ "root": other_root.path().to_string_lossy() }),
        )
        .await
        .expect("the interleaved rust registration succeeds");

    // …and the TS writer saves its stale snapshot (the whole-document
    // replace — no lock, no merge, exactly `FileWorkspacePersistence.save`)
    ts_save_whole_document(&registry_path, &ts_catalog, &[]);

    // Then the rust row written inside the window IS dropped from the file
    // (the KNOWN residual window — the test documents it, it does not forbid
    // it; the fix belongs to the TS writer's retirement line)
    let ids = registry_ids(&registry_path);
    assert!(
        !ids.contains(&other_key),
        "the documented residual window: the TS save dropped the rust row \
         written after its read: {ids:?}"
    );

    // And the documented remedy recovers it: re-registering the workspace
    server
        .call_method(
            "workspace.createOrTouch",
            json!({ "root": other_root.path().to_string_lossy() }),
        )
        .await
        .expect("the re-registration succeeds");
    let ids = registry_ids(&registry_path);
    assert!(
        ids.contains(&other_key),
        "re-registering the workspace recovers the dropped row: {ids:?}"
    );
}

/// Given a workspace tombstoned through the rust unregister face (design
/// §2.3 rule 3), when a lock-less TS writer later leaves the tombstoned
/// workspace's stale row behind while the tombstone list survives, then
/// every subsequent rust write keeps the row out (the D8 tombstone-wins
/// merge) and the read side never serves it — the unregister semantics hold
/// through the full engine path, beyond the W2/W6 writer unit tests.
#[tokio::test]
async fn should_not_resurrect_a_tombstoned_workspace_when_a_stale_ts_row_is_left_behind() {
    let default_root = tempfile::tempdir().expect("default root tempdir");
    let (server, home) = w9_server(default_root.path(), &[]);
    let registry_path = home.path().join("workspaces.json");

    // And a registered workspace the rust engine then unregisters (remove
    // mode — data preserved, identity tombstoned)
    let removed_root = tempfile::tempdir().expect("removed root tempdir");
    let removed_key = mirri_core::encode_work_dir_key(removed_root.path());
    server
        .call_method(
            "workspace.createOrTouch",
            json!({ "root": removed_root.path().to_string_lossy() }),
        )
        .await
        .expect("the registration succeeds");
    let removed = server
        .call_method(
            "workspace.delete",
            json!({ "workspace_id": removed_key.as_str(), "mode": "remove" }),
        )
        .await
        .expect("the removal succeeds");
    assert_eq!(removed["ok"], json!(true));
    assert!(
        !registry_ids(&registry_path).contains(&removed_key),
        "the removed workspace's row is gone from the file"
    );

    // When a lock-less TS writer leaves the tombstoned workspace's stale row
    // behind while round-tripping the tombstone list (the file shape its
    // whole-document save can leave: a cached workspaces map, a fresh
    // deletedIds set)
    let stale_row = mirri_session_index::WorkspaceRow {
        id: removed_key.clone(),
        root: Some(removed_root.path().to_string_lossy().into_owned()),
        name: Some("removed".to_string()),
        created_at: chrono::Utc::now(),
        last_opened_at: chrono::Utc::now(),
        sort_order: 0,
    };
    ts_save_whole_document(
        &registry_path,
        &[stale_row],
        std::slice::from_ref(&removed_key),
    );

    // Then the read side never serves the stale row (the tombstone is
    // delete semantics, not just a write-side keepsake)
    assert!(
        !registry_ids(&registry_path).contains(&removed_key),
        "the read-side tombstone filter keeps the stale row unserved"
    );

    // And a subsequent rust registration (a merged write against the file
    // that still carries the stale row) does not resurrect it either — the
    // D8 tombstone-wins rule holds through the full engine path
    let later_root = tempfile::tempdir().expect("later root tempdir");
    server
        .call_method(
            "workspace.createOrTouch",
            json!({ "root": later_root.path().to_string_lossy() }),
        )
        .await
        .expect("the later registration succeeds");
    assert!(
        !registry_ids(&registry_path).contains(&removed_key),
        "the rust merged write never resurrects the tombstoned row"
    );
    assert_eq!(
        registry_document(&registry_path)["deleted_workspace_ids"],
        json!([removed_key]),
        "the tombstone itself survives every merged write"
    );
}

/// Given forked bucket history — a physical workspace root whose sessions
/// were persisted under TWO id spellings (the D3 alias set: the canonical
/// spelling and a variant spelling that folds under the comparison-only
/// `workspaceRootKey`), when the engine boots and serves, then the listing
/// filter, the session counts, and the index converge per the TS
/// `IWorkspaceAliases` semantics: one scope per physical root, every
/// bucket's sessions visible through either spelling, never double-counted,
/// never zero-counted.
#[tokio::test]
async fn should_converge_forked_alias_buckets_when_history_was_split_across_spellings() {
    // Given a default root and its trailing-backslash spelling — a distinct
    // work-dir key (a distinct bucket) whose root folds with the default's
    // under the comparison-only key (`\` → `/`, trailing separator strips)
    let default_root = tempfile::tempdir().expect("default root tempdir");
    let alias_root = std::path::PathBuf::from(format!("{}\\", default_root.path().display()));
    std::fs::create_dir_all(&alias_root).expect("the alias spelling's directory exists");
    let default_key = mirri_core::encode_work_dir_key(default_root.path());
    let alias_key = mirri_core::encode_work_dir_key(&alias_root);
    assert_ne!(default_key, alias_key, "the spellings mint distinct ids");
    let (server, _home) = w9_server(
        default_root.path(),
        &[(alias_key.as_str(), &alias_root.to_string_lossy())],
    );

    // And forked history: one persisted session under EACH spelling's bucket
    let now = chrono::Utc::now();
    let metadata = || SessionMetadata {
        title: "forked history".to_string(),
        created_at: now,
        updated_at: now,
        tags: Default::default(),
        archived: false,
        is_custom_title: false,
        work_dir: None,
        last_prompt: None,
        forked_from: None,
        cache_root_id: None,
        custom: None,
    };
    let default_session = server
        .session_manager()
        .create_session(metadata(), SessionScopeConfig::default())
        .await
        .expect("the default bucket's session is created");
    let alias_runtime = server
        .registry
        .ensure_built(&alias_key)
        .expect("the alias row is registered")
        .expect("the alias runtime builds");
    let alias_session = alias_runtime
        .manager
        .create_session(metadata(), SessionScopeConfig::default())
        .await
        .expect("the alias bucket's session is created");
    let default_id = default_session.id().as_str().to_string();
    let alias_id = alias_session.id().as_str().to_string();

    // When the engine serves (the one-time reconcile converges both buckets
    // into the index, each under its own spelling's key)
    let workspaces = server
        .call_method("workspace.list", json!({}))
        .await
        .expect("the first RPC succeeds");
    let index = server.session_index().expect("the index is open");
    assert_eq!(
        index
            .get_session(&default_id)
            .expect("the default bucket's row converged")
            .workspace_id,
        default_key,
        "true attribution: the default bucket's row keeps its own key"
    );
    assert_eq!(
        index
            .get_session(&alias_id)
            .expect("the alias bucket's row converged")
            .workspace_id,
        alias_key,
        "true attribution: the alias bucket's row keeps its own key"
    );

    // Then either spelling's filter lists BOTH buckets' sessions (one scope
    // per physical root)
    for filter in [default_key.as_str(), alias_key.as_str()] {
        let listed = server
            .call_method("session.list", json!({ "workspace_id": filter }))
            .await
            .expect("the alias-folded list succeeds");
        let mut listed = listed_ids(&listed);
        listed.sort();
        let mut expected = vec![default_id.clone(), alias_id.clone()];
        expected.sort();
        assert_eq!(
            listed, expected,
            "the {filter} filter folds both spellings' buckets into one scope"
        );
    }

    // And both registry rows report the folded count (never double, never
    // zero — the TS dedupeByRoot semantics)
    let count_of = |workspace_id: &str| {
        workspaces
            .as_array()
            .expect("workspace.list returns rows")
            .iter()
            .find(|row| row["id"] == json!(workspace_id))
            .unwrap_or_else(|| panic!("the row {workspace_id} must be listed"))["sessionCount"]
            .as_u64()
            .expect("sessionCount is a number")
    };
    assert_eq!(count_of(default_key.as_str()), 2);
    assert_eq!(
        count_of(alias_key.as_str()),
        2,
        "the alias row's count folds over the shared scope: {workspaces}"
    );
}

/// Given spellings that do NOT fold on POSIX — a case-variant spelling and a
/// `~`-prefixed spelling of a registered root (the TS `workspaceRootKey` is
/// case-sensitive outside win-shaped roots, and `~` is never expanded inside
/// the fold — the input boundary owns expansion), when the engine serves,
/// then each spelling keeps its own scope: the folded filter never crosses
/// them, and their counts stay their own (never inherited from the sibling).
#[tokio::test]
async fn should_keep_posix_case_and_tilde_spellings_in_separate_scopes_when_they_do_not_fold() {
    // Given a real project root plus two non-folding spellings of it
    let parent = tempfile::tempdir().expect("parent tempdir");
    let proj_root = parent.path().join("proj");
    std::fs::create_dir_all(&proj_root).expect("the project root exists");
    let case_root = parent.path().join("PROJ");
    std::fs::create_dir_all(&case_root).expect("the case spelling's dir exists");
    let tilde_root = std::path::PathBuf::from("~/proj");
    let proj_key = mirri_core::encode_work_dir_key(&proj_root);
    let case_key = mirri_core::encode_work_dir_key(&case_root);
    let tilde_key = mirri_core::encode_work_dir_key(&tilde_root);
    let default_root = tempfile::tempdir().expect("default root tempdir");
    let (server, _home) = w9_server(
        default_root.path(),
        &[
            (proj_key.as_str(), &proj_root.to_string_lossy()),
            (case_key.as_str(), &case_root.to_string_lossy()),
            (tilde_key.as_str(), &tilde_root.to_string_lossy()),
        ],
    );
    assert_ne!(proj_key, case_key, "a case variant mints its own id");
    assert_ne!(proj_key, tilde_key, "a ~ spelling mints its own id");

    // And one persisted session under the project root's own bucket
    let now = chrono::Utc::now();
    let proj_runtime = server
        .registry
        .ensure_built(&proj_key)
        .expect("the project row is registered")
        .expect("the project runtime builds");
    let proj_session = proj_runtime
        .manager
        .create_session(
            SessionMetadata {
                title: "the project session".to_string(),
                created_at: now,
                updated_at: now,
                tags: Default::default(),
                archived: false,
                is_custom_title: false,
                work_dir: None,
                last_prompt: None,
                forked_from: None,
                cache_root_id: None,
                custom: None,
            },
            SessionScopeConfig::default(),
        )
        .await
        .expect("the project bucket's session is created");
    let proj_session_id = proj_session.id().as_str().to_string();

    // When the engine serves
    let workspaces = server
        .call_method("workspace.list", json!({}))
        .await
        .expect("the first RPC succeeds");

    // Then the non-folding spellings' filters stay their own scopes: the
    // project session is unreachable through them
    for filter in [case_key.as_str(), tilde_key.as_str()] {
        let listed = server
            .call_method("session.list", json!({ "workspace_id": filter }))
            .await
            .expect("the filtered list succeeds");
        assert_eq!(
            listed_ids(&listed),
            Vec::<String>::new(),
            "the {filter} scope never inherits the project bucket's session"
        );
    }
    let listed = server
        .call_method("session.list", json!({ "workspace_id": proj_key.as_str() }))
        .await
        .expect("the project filter succeeds");
    assert_eq!(
        listed_ids(&listed),
        vec![proj_session_id.clone()],
        "the project's own filter still answers its session"
    );

    // And the counts stay their own (the TS POSIX stance: no case folding,
    // no ~ expansion — each row counts only its own scope)
    let count_of = |workspace_id: &str| {
        workspaces
            .as_array()
            .expect("workspace.list returns rows")
            .iter()
            .find(|row| row["id"] == json!(workspace_id))
            .unwrap_or_else(|| panic!("the row {workspace_id} must be listed"))["sessionCount"]
            .as_u64()
            .expect("sessionCount is a number")
    };
    assert_eq!(count_of(proj_key.as_str()), 1);
    assert_eq!(
        count_of(case_key.as_str()),
        0,
        "the case spelling never inherits the project's count"
    );
    assert_eq!(
        count_of(tilde_key.as_str()),
        0,
        "the ~ spelling never inherits the project's count"
    );
}
