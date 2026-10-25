//! Integration tests for session lifecycle management (Task 32).
//!
//! These tests verify the session management layer that sits above the scope
//! hierarchy: `SessionManager` (create / resume / fork / close / list
//! sessions), `AgentLifecycle` (create / fork / remove / list agents), and
//! `SessionPersistence` (state.json save/load, wire log path computation).
//!
//! Tests use the Given-When-Then naming convention.

use std::collections::HashSet;
use std::sync::Arc;

use mirri_core::task::TaskKind;
use mirri_core::{
    AgentId, AgentLifecycle, AgentScopeConfig, AgentTask, AppScope, AppScopeConfig, CloseReason,
    ProviderType, SessionId, SessionManager, SessionMetadata, SessionPersistence,
    SessionScopeConfig, SessionStatus, TaskStatus, WorkspaceId, WorkspaceScopeConfig,
    WorkspaceTrustLevel,
};

// ===========================================================================
// Helper: create a workspace + session manager backed by a temp dir
// ===========================================================================

struct TestEnv {
    manager: SessionManager,
    _tmp: tempfile::TempDir,
}

impl TestEnv {
    async fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let app = AppScope::new(AppScopeConfig::default());
        let workspace = app
            .create_workspace_with_config(
                WorkspaceId::new("test-ws"),
                tmp.path().to_path_buf(),
                WorkspaceScopeConfig {
                    trust_level: WorkspaceTrustLevel::Trusted,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let manager = SessionManager::with_persistence_dir(
            workspace,
            tmp.path().join(".mirri").join("sessions"),
            std::sync::Arc::new(mirri_os::LocalFs::new()),
        );
        Self { manager, _tmp: tmp }
    }
}

/// A resume base config carrying an explicit model (spec 11 R4): the
/// hand-written wire logs in these tests carry no model record, so the
/// base config seeds the model exactly the way the embedded host seeds it
/// from the config snapshot's `default_model` (the no-record fallback
/// chain: config default model → loud error).
fn resume_base_config() -> AgentScopeConfig {
    AgentScopeConfig {
        model_id: "test-resume-model".to_string(),
        ..AgentScopeConfig::default()
    }
}

fn make_metadata(title: &str, tags: &[&str]) -> SessionMetadata {
    let mut tag_set = HashSet::new();
    for tag in tags {
        tag_set.insert(tag.to_string());
    }
    SessionMetadata {
        title: title.to_string(),
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
        tags: tag_set,
        archived: false,
        is_custom_title: false,
        work_dir: None,
        last_prompt: None,
        forked_from: None,
        cache_root_id: None,
        custom: None,
        last_used_at: None,
    }
}

// ===========================================================================
// SessionManager — create + initial state
// ===========================================================================

#[tokio::test]
async fn should_have_initial_state_when_session_created() {
    // Given a fresh session manager
    let env = TestEnv::new().await;

    // When creating a session
    let metadata = make_metadata("My Session", &["test", "phase3"]);
    let session = env
        .manager
        .create_session(metadata, SessionScopeConfig::default())
        .await
        .unwrap();

    // Then the session has zero agents
    let agents = session.list_agents().await;
    assert_eq!(agents.len(), 0, "new session should have zero agents");

    // And the session is listed
    let sessions = env.manager.list_sessions().await;
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].agent_count, 0);
    assert_eq!(sessions[0].title, "My Session");
    assert!(sessions[0].tags.contains(&"test".to_string()));
}

// ===========================================================================
// AgentLifecycle — create multiple agents and verify isolation
// ===========================================================================

#[tokio::test]
async fn should_be_isolated_when_multiple_agents_created() {
    // Given a session
    let env = TestEnv::new().await;
    let session = env
        .manager
        .create_session(
            make_metadata("Multi-Agent", &[]),
            SessionScopeConfig::default(),
        )
        .await
        .unwrap();

    // When creating two agents with different configs
    let lifecycle = AgentLifecycle::new(Arc::clone(&session));
    let agent1 = lifecycle
        .create_agent(AgentScopeConfig {
            model_id: "claude-sonnet-4-20250514".to_string(),
            provider_type: ProviderType::Anthropic,
            ..Default::default()
        })
        .await
        .unwrap();
    let agent2 = lifecycle
        .create_agent(AgentScopeConfig {
            model_id: "gpt-4o".to_string(),
            provider_type: ProviderType::OpenAi,
            ..Default::default()
        })
        .await
        .unwrap();

    // Then both agents exist with distinct IDs
    assert_ne!(agent1.id(), agent2.id());

    // And both agents are listed
    let agents = lifecycle.list_agents().await;
    assert_eq!(agents.len(), 2);

    // And each agent's config is independent
    let cm1 = agent1.context_memory();
    let cm2 = agent2.context_memory();
    assert!(
        !Arc::ptr_eq(&cm1, &cm2),
        "agents must not share context memory"
    );
}

// ===========================================================================
// AgentLifecycle — fork agent and verify context copy
// ===========================================================================

#[tokio::test]
async fn should_copy_context_when_agent_forked() {
    // Given a session with one agent that has context
    let env = TestEnv::new().await;
    let session = env
        .manager
        .create_session(
            make_metadata("Fork Test", &[]),
            SessionScopeConfig::default(),
        )
        .await
        .unwrap();

    let lifecycle = AgentLifecycle::new(Arc::clone(&session));
    let source_agent = lifecycle
        .create_agent(AgentScopeConfig::default())
        .await
        .unwrap();

    // Add a message to the source agent's context memory
    {
        use mirri_core::message::ContentPart;
        use mirri_core::origin::PromptOrigin;

        let memory = source_agent.context_memory();
        let mut guard = memory.lock().unwrap();
        guard.append_user_message(
            vec![ContentPart::Text {
                text: "Hello from source agent".to_string(),
            }],
            PromptOrigin::User,
        );
    }

    // When forking the agent
    let forked_agent = lifecycle
        .fork_agent(&source_agent, AgentScopeConfig::default())
        .await
        .unwrap();

    // Then the forked agent has a different ID
    assert_ne!(source_agent.id(), forked_agent.id());

    // And the forked agent has the same context (history length)
    let source_history = {
        let memory = source_agent.context_memory();
        let guard = memory.lock().unwrap();
        guard.history().len()
    };
    let forked_history = {
        let memory = forked_agent.context_memory();
        let guard = memory.lock().unwrap();
        guard.history().len()
    };
    assert_eq!(
        source_history, forked_history,
        "forked agent should have the same number of messages as source"
    );

    // And both agents are in the session
    let agents = lifecycle.list_agents().await;
    assert_eq!(agents.len(), 2);
}

// ===========================================================================
// AgentLifecycle — remove agent and verify cleanup
// ===========================================================================

#[tokio::test]
async fn should_clean_up_agent_when_removed() {
    // Given a session with two agents
    let env = TestEnv::new().await;
    let session = env
        .manager
        .create_session(
            make_metadata("Remove Test", &[]),
            SessionScopeConfig::default(),
        )
        .await
        .unwrap();

    let lifecycle = AgentLifecycle::new(Arc::clone(&session));
    let agent1 = lifecycle
        .create_agent(AgentScopeConfig::default())
        .await
        .unwrap();
    let _agent2 = lifecycle
        .create_agent(AgentScopeConfig::default())
        .await
        .unwrap();

    assert_eq!(lifecycle.list_agents().await.len(), 2);

    // When removing agent1
    lifecycle.remove_agent(agent1.id()).await.unwrap();

    // Then only one agent remains
    let remaining = lifecycle.list_agents().await;
    assert_eq!(remaining.len(), 1);
    assert_ne!(remaining[0].id, agent1.id().as_str());

    // And the removed agent cannot be retrieved
    assert!(session.get_agent(agent1.id()).await.is_none());
}

// ===========================================================================
// AgentLifecycle — remove non-existent agent returns error
// ===========================================================================

#[tokio::test]
async fn should_return_error_when_removing_nonexistent_agent() {
    let env = TestEnv::new().await;
    let session = env
        .manager
        .create_session(make_metadata("Empty", &[]), SessionScopeConfig::default())
        .await
        .unwrap();

    let lifecycle = AgentLifecycle::new(session);
    let fake_id = AgentId::new("nonexistent-agent");

    let result = lifecycle.remove_agent(&fake_id).await;
    assert!(result.is_err());
}

// ===========================================================================
// SessionManager — close session and verify cleanup
// ===========================================================================

#[tokio::test]
async fn should_remove_session_from_workspace_when_closed() {
    // Given a session
    let env = TestEnv::new().await;
    let session = env
        .manager
        .create_session(
            make_metadata("To Close", &[]),
            SessionScopeConfig::default(),
        )
        .await
        .unwrap();
    let session_id = session.id().clone();

    // Verify it exists
    assert!(env.manager.get_session(&session_id).await.is_some());

    // When closing the session
    env.manager
        .close_session(&session_id, CloseReason::Exit)
        .await
        .unwrap();

    // Then the session is no longer in the workspace
    assert!(env.manager.get_session(&session_id).await.is_none());

    // And the session list is empty
    let sessions = env.manager.list_sessions().await;
    assert_eq!(sessions.len(), 0);
}

// ===========================================================================
// SessionManager — close non-existent session returns error
// ===========================================================================

#[tokio::test]
async fn should_return_error_when_closing_nonexistent_session() {
    let env = TestEnv::new().await;
    let fake_id = SessionId::new("nonexistent-session");

    let result = env
        .manager
        .close_session(&fake_id, CloseReason::UserInitiated)
        .await;
    assert!(result.is_err());
}

// ===========================================================================
// SessionManager — list sessions
// ===========================================================================

#[tokio::test]
async fn should_return_all_sessions_when_listed() {
    let env = TestEnv::new().await;

    // Create three sessions
    for i in 0..3 {
        env.manager
            .create_session(
                make_metadata(&format!("Session {}", i), &[]),
                SessionScopeConfig::default(),
            )
            .await
            .unwrap();
    }

    let sessions = env.manager.list_sessions().await;
    assert_eq!(sessions.len(), 3);

    // Each should have a unique ID
    let ids: Vec<&str> = sessions.iter().map(|s| s.id.as_str()).collect();
    let unique: std::collections::HashSet<&str> = ids.iter().copied().collect();
    assert_eq!(unique.len(), 3);
}

// ===========================================================================
// SessionManager — list agents in a session
// ===========================================================================

#[tokio::test]
async fn should_return_all_agents_with_info_when_listed() {
    let env = TestEnv::new().await;
    let session = env
        .manager
        .create_session(
            make_metadata("Agent List", &[]),
            SessionScopeConfig::default(),
        )
        .await
        .unwrap();

    let lifecycle = AgentLifecycle::new(Arc::clone(&session));

    lifecycle
        .create_agent(AgentScopeConfig {
            model_id: "claude-sonnet-4-20250514".to_string(),
            provider_type: ProviderType::Anthropic,
            ..Default::default()
        })
        .await
        .unwrap();
    lifecycle
        .create_agent(AgentScopeConfig {
            model_id: "gpt-4o".to_string(),
            provider_type: ProviderType::OpenAi,
            ..Default::default()
        })
        .await
        .unwrap();

    let agents = lifecycle.list_agents().await;
    assert_eq!(agents.len(), 2);

    // Verify provider strings
    let providers: Vec<&str> = agents.iter().map(|a| a.provider.as_str()).collect();
    assert!(providers.contains(&"anthropic"));
    assert!(providers.contains(&"openai"));
}

// ===========================================================================
// SessionPersistence — save and load state.json
// ===========================================================================

#[tokio::test]
async fn should_roundtrip_metadata_when_state_saved_and_loaded() {
    let tmp = tempfile::tempdir().unwrap();
    let persistence = SessionPersistence::new(
        tmp.path().to_path_buf(),
        std::sync::Arc::new(mirri_os::LocalFs::new()),
    );

    let session_id = SessionId::new("persist-test-1");
    let metadata = make_metadata("Persisted Session", &["tag-a", "tag-b"]);

    // When saving state
    persistence
        .save_state(&session_id, &metadata)
        .await
        .unwrap();

    // Then loading state returns the same metadata
    let loaded = persistence.load_state(&session_id).await.unwrap();
    assert_eq!(loaded.title, "Persisted Session");
    assert_eq!(loaded.tags.len(), 2);
    assert!(loaded.tags.contains("tag-a"));
    assert!(loaded.tags.contains("tag-b"));
}

// ===========================================================================
// SessionPersistence — wire log path generation
// ===========================================================================

#[tokio::test]
async fn should_return_correct_paths_when_wire_log_path_computed() {
    let tmp = tempfile::tempdir().unwrap();
    let persistence = SessionPersistence::new(
        tmp.path().to_path_buf(),
        std::sync::Arc::new(mirri_os::LocalFs::new()),
    );

    let session_id = SessionId::new("path-test-session");

    let wire_path = persistence.wire_log_path(&session_id);
    let state_path = persistence.state_path(&session_id);

    assert_eq!(
        wire_path,
        tmp.path().join("path-test-session").join("wire.jsonl")
    );
    assert_eq!(
        state_path,
        tmp.path().join("path-test-session").join("state.json")
    );
}

// ===========================================================================
// SessionPersistence — delete session files
// ===========================================================================

#[tokio::test]
async fn should_remove_files_when_session_deleted() {
    let tmp = tempfile::tempdir().unwrap();
    let persistence = SessionPersistence::new(
        tmp.path().to_path_buf(),
        std::sync::Arc::new(mirri_os::LocalFs::new()),
    );

    let session_id = SessionId::new("delete-me");
    persistence
        .save_state(&session_id, &SessionMetadata::default())
        .await
        .unwrap();

    assert!(persistence.state_path(&session_id).exists());

    persistence.delete_session_files(&session_id).await.unwrap();

    assert!(!persistence.state_path(&session_id).exists());
}

// ===========================================================================
// SessionPersistence — list persisted sessions
// ===========================================================================

#[tokio::test]
async fn should_return_all_sessions_sorted_when_persisted_sessions_listed() {
    let tmp = tempfile::tempdir().unwrap();
    let persistence = SessionPersistence::new(
        tmp.path().to_path_buf(),
        std::sync::Arc::new(mirri_os::LocalFs::new()),
    );

    for i in 0..3 {
        let sid = SessionId::new(format!("persisted-{}", i));
        persistence
            .save_state(&sid, &make_metadata(&format!("S{}", i), &[]))
            .await
            .unwrap();
    }

    let sessions = persistence.list_persisted_sessions(false).await;
    assert_eq!(sessions.len(), 3);
    // Should be sorted by session_id
    assert_eq!(sessions[0].session_id, "persisted-0");
    assert_eq!(sessions[1].session_id, "persisted-1");
    assert_eq!(sessions[2].session_id, "persisted-2");
}

// ===========================================================================
// SessionPersistence — load state for non-existent session returns error
// ===========================================================================

#[tokio::test]
async fn should_return_error_when_loading_nonexistent_state_file() {
    let tmp = tempfile::tempdir().unwrap();
    let persistence = SessionPersistence::new(
        tmp.path().to_path_buf(),
        std::sync::Arc::new(mirri_os::LocalFs::new()),
    );

    let result = persistence.load_state(&SessionId::new("never-saved")).await;
    assert!(result.is_err());
}

// ===========================================================================
// SessionManager — resume (simplified: verifies flow without full replay)
// ===========================================================================

#[tokio::test]
async fn should_recreate_session_in_workspace_when_resumed() {
    let env = TestEnv::new().await;

    // Create and close a session (state.json is persisted on create)
    let session = env
        .manager
        .create_session(
            make_metadata("To Resume", &["resume"]),
            SessionScopeConfig::default(),
        )
        .await
        .unwrap();
    let session_id = session.id().clone();
    let wire_log_path = env.manager.persistence().wire_log_path(&session_id);

    // Create the wire log file so resume validation passes
    tokio::fs::create_dir_all(wire_log_path.parent().unwrap())
        .await
        .unwrap();
    tokio::fs::write(&wire_log_path, "").await.unwrap();

    // Close the session
    env.manager
        .close_session(&session_id, CloseReason::Exit)
        .await
        .unwrap();
    assert!(env.manager.get_session(&session_id).await.is_none());

    // When resuming (the base config carries the model the way the
    // embedded host seeds it from the config snapshot's `default_model` —
    // spec 11 R4: no built-in fallback)
    let resumed = env
        .manager
        .resume_session_with_limits(&session_id, resume_base_config(), None)
        .await
        .unwrap();

    // Then the session is back in the workspace with the original metadata
    assert_eq!(resumed.id(), &session_id);
    let metadata = resumed.metadata().await;
    assert_eq!(metadata.title, "To Resume");
    assert!(metadata.tags.contains("resume"));
}

// ===========================================================================
// SessionManager — resume with missing wire log returns error
// ===========================================================================

#[tokio::test]
async fn should_return_error_when_resuming_with_missing_wire_log() {
    let env = TestEnv::new().await;

    // Persist state for a session
    let session_id = SessionId::new("resume-fail-test");
    env.manager
        .persistence()
        .save_state(&session_id, &make_metadata("Fail", &[]))
        .await
        .unwrap();

    // But don't create the wire log file

    let result = env.manager.resume_session(&session_id).await;
    assert!(result.is_err());
}

// ===========================================================================
// SessionManager — fork session (verify new session is created)
// ===========================================================================

#[tokio::test]
async fn should_create_new_session_with_agent_when_session_forked() {
    let env = TestEnv::new().await;

    // Create source session with an agent
    let source_session = env
        .manager
        .create_session(make_metadata("Source", &[]), SessionScopeConfig::default())
        .await
        .unwrap();

    let lifecycle = AgentLifecycle::new(Arc::clone(&source_session));
    let source_agent = lifecycle
        .create_agent(AgentScopeConfig {
            model_id: "claude-sonnet-4-20250514".to_string(),
            ..Default::default()
        })
        .await
        .unwrap();
    let source_agent_id = source_agent.id().clone();

    // When forking the session
    let forked_session = env
        .manager
        .fork_session(
            &source_session,
            &source_agent_id,
            make_metadata("Forked", &[]),
        )
        .await
        .unwrap();

    // Then the forked session has a different ID
    assert_ne!(source_session.id(), forked_session.id());

    // And the forked session has one agent (the forked copy)
    let forked_agents = forked_session.list_agents().await;
    assert_eq!(forked_agents.len(), 1);

    // And the source session still has its agent
    let source_agents = source_session.list_agents().await;
    assert_eq!(source_agents.len(), 1);

    // And both sessions are listed
    let sessions = env.manager.list_sessions().await;
    assert_eq!(sessions.len(), 2);
}

// ===========================================================================
// SessionManager — fork with non-existent agent returns error
// ===========================================================================

#[tokio::test]
async fn should_return_error_when_forking_with_nonexistent_agent() {
    let env = TestEnv::new().await;

    let source_session = env
        .manager
        .create_session(make_metadata("Source", &[]), SessionScopeConfig::default())
        .await
        .unwrap();

    let fake_agent_id = AgentId::new("nonexistent-agent");
    let result = env
        .manager
        .fork_session(
            &source_session,
            &fake_agent_id,
            make_metadata("Forked", &[]),
        )
        .await;

    assert!(result.is_err());
}

// ===========================================================================
// SessionManager — create session persists state.json
// ===========================================================================

#[tokio::test]
async fn should_persist_state_json_when_session_created() {
    let env = TestEnv::new().await;

    let session = env
        .manager
        .create_session(
            make_metadata("Persisted on Create", &["auto"]),
            SessionScopeConfig::default(),
        )
        .await
        .unwrap();

    let state_path = env.manager.persistence().state_path(session.id());
    assert!(
        state_path.exists(),
        "state.json should be created on session creation"
    );

    // Verify the persisted state matches
    let loaded = env
        .manager
        .persistence()
        .load_state(session.id())
        .await
        .unwrap();
    assert_eq!(loaded.title, "Persisted on Create");
    assert!(loaded.tags.contains("auto"));
}

// ===========================================================================
// SessionManager — concurrent session creation
// ===========================================================================

#[tokio::test]
async fn should_succeed_with_unique_ids_when_sessions_created_concurrently() {
    // Given a fresh session manager
    let tmp = tempfile::tempdir().unwrap();
    let app = AppScope::new(AppScopeConfig::default());
    let workspace = app
        .create_workspace_with_config(
            WorkspaceId::new("concurrent-ws"),
            tmp.path().to_path_buf(),
            WorkspaceScopeConfig::default(),
        )
        .await
        .unwrap();
    let manager = Arc::new(SessionManager::with_persistence_dir(
        workspace,
        tmp.path().join(".mirri").join("sessions"),
        std::sync::Arc::new(mirri_os::LocalFs::new()),
    ));

    // Spawn 10 concurrent session creation tasks
    let mut handles = Vec::new();

    for i in 0..10 {
        let m = Arc::clone(&manager);
        handles.push(tokio::spawn(async move {
            m.create_session(
                make_metadata(&format!("Concurrent {}", i), &[]),
                SessionScopeConfig::default(),
            )
            .await
        }));
    }

    // Wait for all to complete
    let mut session_ids = Vec::new();
    for handle in handles {
        let session = handle.await.unwrap().unwrap();
        session_ids.push(session.id().clone());
    }

    // All sessions should be in the workspace
    let sessions = manager.list_sessions().await;
    assert_eq!(
        sessions.len(),
        10,
        "all 10 concurrent sessions should exist"
    );

    // All IDs should be unique
    let unique_ids: std::collections::HashSet<&str> =
        session_ids.iter().map(|id| id.as_str()).collect();
    assert_eq!(unique_ids.len(), 10, "all session IDs should be unique");
}

// ===========================================================================
// SessionManager — get session returns the correct session
// ===========================================================================

#[tokio::test]
async fn should_return_correct_session_when_session_retrieved_by_id() {
    let env = TestEnv::new().await;

    let session = env
        .manager
        .create_session(make_metadata("Get Me", &[]), SessionScopeConfig::default())
        .await
        .unwrap();
    let session_id = session.id().clone();

    let retrieved = env.manager.get_session(&session_id).await;
    assert!(retrieved.is_some());
    assert_eq!(retrieved.unwrap().id(), &session_id);

    let not_found = env
        .manager
        .get_session(&SessionId::new("nonexistent"))
        .await;
    assert!(not_found.is_none());
}

// ===========================================================================
// SessionManager — SessionInfo fields are correct
// ===========================================================================

#[tokio::test]
async fn should_have_correct_agent_count_when_sessions_listed() {
    let env = TestEnv::new().await;

    let session = env
        .manager
        .create_session(
            make_metadata("Info Test", &["alpha", "beta"]),
            SessionScopeConfig::default(),
        )
        .await
        .unwrap();

    let lifecycle = AgentLifecycle::new(Arc::clone(&session));
    lifecycle
        .create_agent(AgentScopeConfig::default())
        .await
        .unwrap();
    lifecycle
        .create_agent(AgentScopeConfig::default())
        .await
        .unwrap();
    lifecycle
        .create_agent(AgentScopeConfig::default())
        .await
        .unwrap();

    let sessions = env.manager.list_sessions().await;
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].agent_count, 3);
    assert_eq!(sessions[0].title, "Info Test");
    assert!(sessions[0].tags.contains(&"alpha".to_string()));
    assert!(sessions[0].tags.contains(&"beta".to_string()));
}

// ===========================================================================
// Session lifecycle external command hooks (SessionStart / SessionEnd)
// ===========================================================================

/// Build a manager whose workspace carries the given external command hooks.
async fn manager_with_hooks(
    hooks: Vec<mirri_core::hook_engine::HookDef>,
) -> (SessionManager, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    let app = AppScope::new(AppScopeConfig::default());
    let workspace = app
        .create_workspace_with_config(
            WorkspaceId::new("hooks-ws"),
            tmp.path().to_path_buf(),
            WorkspaceScopeConfig {
                trust_level: WorkspaceTrustLevel::Trusted,
                hooks,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let manager = SessionManager::with_persistence_dir(
        workspace,
        tmp.path().join(".mirri").join("sessions"),
        std::sync::Arc::new(mirri_os::LocalFs::new()),
    );
    (manager, tmp)
}

#[tokio::test]
async fn should_fire_session_start_hook_when_session_created() {
    use mirri_core::hook_engine::{HookDef, HookEventType};

    // Given a workspace with a SessionStart hook that dumps its JSON input
    // to a marker file
    let marker_path =
        std::env::temp_dir().join(format!("session_start_hook_{}.json", std::process::id()));
    let _ = std::fs::remove_file(&marker_path);
    let (manager, _tmp) = manager_with_hooks(vec![HookDef::new(
        HookEventType::SessionStart,
        format!("cat > {:?}", marker_path),
    )])
    .await;

    // When a session is created
    manager
        .create_session(make_metadata("Hooked", &[]), SessionScopeConfig::default())
        .await
        .expect("session should be created");

    // Then the hook fired with the TS input shape { source: "startup" }
    let content = std::fs::read_to_string(&marker_path).expect("hook input file should exist");
    let input: serde_json::Value =
        serde_json::from_str(&content).expect("hook input should be JSON");
    assert_eq!(input["source"], serde_json::json!("startup"));
    let _ = std::fs::remove_file(&marker_path);
}

#[tokio::test]
async fn should_fire_session_start_hook_when_resuming_session() {
    use mirri_core::hook_engine::{HookDef, HookEventType};
    use mirri_persistence::JsonlAppendLog;

    // Given a workspace with a SessionStart hook that dumps its JSON input
    // to a marker file, and a persisted session that has been closed
    let marker_path =
        std::env::temp_dir().join(format!("session_resume_hook_{}.json", std::process::id()));
    let _ = std::fs::remove_file(&marker_path);
    let (manager, _tmp) = manager_with_hooks(vec![HookDef::new(
        HookEventType::SessionStart,
        format!("cat > {:?}", marker_path),
    )])
    .await;

    let session = manager
        .create_session(
            make_metadata("Resume Me", &[]),
            SessionScopeConfig::default(),
        )
        .await
        .unwrap();
    let session_id = session.id().clone();
    manager
        .close_session(&session_id, CloseReason::Exit)
        .await
        .expect("session should close");

    // An empty wire log is enough for resume to reconstruct an empty agent.
    let wire_log_path = manager.persistence().wire_log_path(&session_id);
    JsonlAppendLog::create(&wire_log_path)
        .await
        .expect("create wire log");

    // When the session is resumed (the base config carries the model the
    // way the embedded host seeds it from the config snapshot's
    // `default_model` — spec 11 R4: no built-in fallback)
    let resumed = manager
        .resume_session_with_limits(&session_id, resume_base_config(), None)
        .await;
    assert!(
        resumed.is_ok(),
        "resume should succeed: {:?}",
        resumed.err()
    );

    // Then a SessionStart fired with { source: "resume" }
    let content = std::fs::read_to_string(&marker_path).expect("hook input file should exist");
    let input: serde_json::Value =
        serde_json::from_str(&content).expect("hook input should be JSON");
    assert_eq!(input["source"], serde_json::json!("resume"));
    let _ = std::fs::remove_file(&marker_path);
}

#[tokio::test]
async fn should_fire_session_end_hook_when_session_closed() {
    use mirri_core::hook_engine::{HookDef, HookEventType};

    // Given a workspace with a SessionEnd hook that dumps its JSON input to
    // a marker file
    let marker_path =
        std::env::temp_dir().join(format!("session_end_hook_{}.json", std::process::id()));
    let _ = std::fs::remove_file(&marker_path);
    let (manager, _tmp) = manager_with_hooks(vec![HookDef::new(
        HookEventType::SessionEnd,
        format!("cat > {:?}", marker_path),
    )])
    .await;

    let session = manager
        .create_session(make_metadata("Closing", &[]), SessionScopeConfig::default())
        .await
        .unwrap();

    // When the session is closed by the user
    manager
        .close_session(session.id(), CloseReason::UserInitiated)
        .await
        .expect("session should close");

    // Then the hook fired with the TS input shape { reason: "user" }
    let content = std::fs::read_to_string(&marker_path).expect("hook input file should exist");
    let input: serde_json::Value =
        serde_json::from_str(&content).expect("hook input should be JSON");
    assert_eq!(input["reason"], serde_json::json!("user"));
    let _ = std::fs::remove_file(&marker_path);
}

// ===========================================================================
// Task persistence — background tasks survive close/resume
// ===========================================================================

#[tokio::test]
async fn should_persist_and_reload_tasks_when_session_closed_and_resumed() {
    // Given a session with a registered background task and appended output
    let env = TestEnv::new().await;
    let session = env
        .manager
        .create_session(make_metadata("Tasks", &[]), SessionScopeConfig::default())
        .await
        .unwrap();
    let task_id = session.tasks().register(
        TaskKind::Process {
            command: None,
            dispatcher: None,
        },
        "build watcher".into(),
        session.id().to_string(),
    );
    session.tasks().append_output(&task_id, "line one\n");

    // Then the task is persisted under `<session_dir>/tasks/`
    let session_dir = env.manager.persistence().session_dir(session.id());
    let task_path = session_dir.join("tasks").join(format!("{task_id}.json"));
    assert!(
        task_path.exists(),
        "register must persist the task metadata JSON"
    );
    let log_path = session_dir.join("tasks").join(&task_id).join("output.log");
    assert_eq!(
        std::fs::read_to_string(&log_path).unwrap(),
        "line one\n",
        "appended output must land in the task's append-only log"
    );

    // When the session closes and is resumed (the base config carries the
    // model the way the embedded host seeds it from the config snapshot's
    // `default_model` — spec 11 R4: no built-in fallback)
    let session_id = session.id().clone();
    env.manager
        .close_session(&session_id, CloseReason::UserInitiated)
        .await
        .expect("session should close");
    let resumed = env
        .manager
        .resume_session_with_limits(&session_id, resume_base_config(), None)
        .await
        .unwrap();

    // Then the task is reloaded, and the still-running task is reconciled
    // to stopped because no live process owns it after the restart
    let task = resumed.tasks().get(&task_id).expect("task reloaded");
    assert_eq!(task.status, TaskStatus::Stopped);
    let on_disk: AgentTask =
        serde_json::from_str(&std::fs::read_to_string(&task_path).unwrap()).unwrap();
    assert_eq!(on_disk.status, TaskStatus::Stopped);
    assert_eq!(on_disk.output, "line one\n");
}

// ===========================================================================
// G32 — list_all_sessions (merged persisted + active)
// ===========================================================================

#[tokio::test]
async fn should_list_persisted_sessions_when_no_active_sessions_exist() {
    // Given several persisted sessions (created then closed, so they exist
    // on disk but not in the workspace)
    let env = TestEnv::new().await;

    let session1 = env
        .manager
        .create_session(
            make_metadata("Persisted One", &[]),
            SessionScopeConfig::default(),
        )
        .await
        .unwrap();
    let session2 = env
        .manager
        .create_session(
            make_metadata("Persisted Two", &[]),
            SessionScopeConfig::default(),
        )
        .await
        .unwrap();

    // Close both sessions so they are persisted but not active
    env.manager
        .close_session(session1.id(), CloseReason::Exit)
        .await
        .unwrap();
    env.manager
        .close_session(session2.id(), CloseReason::Exit)
        .await
        .unwrap();

    // When listing all sessions
    let sessions = env.manager.list_all_sessions(false).await;

    // Then both persisted sessions are returned
    assert_eq!(sessions.len(), 2, "should list both persisted sessions");

    // And both have the Closed status
    for info in &sessions {
        assert_eq!(
            info.status,
            SessionStatus::Closed,
            "persisted-only sessions should be Closed"
        );
    }

    // And the titles are correct
    let titles: Vec<&str> = sessions.iter().map(|s| s.title.as_str()).collect();
    assert!(titles.contains(&"Persisted One"));
    assert!(titles.contains(&"Persisted Two"));
}

#[tokio::test]
async fn should_mark_active_sessions_as_running_when_listed() {
    // Given one active session and two persisted-only sessions
    let env = TestEnv::new().await;

    // Create and close two sessions (they become persisted-only)
    let closed1 = env
        .manager
        .create_session(
            make_metadata("Closed One", &[]),
            SessionScopeConfig::default(),
        )
        .await
        .unwrap();
    let closed2 = env
        .manager
        .create_session(
            make_metadata("Closed Two", &[]),
            SessionScopeConfig::default(),
        )
        .await
        .unwrap();
    env.manager
        .close_session(closed1.id(), CloseReason::Exit)
        .await
        .unwrap();
    env.manager
        .close_session(closed2.id(), CloseReason::Exit)
        .await
        .unwrap();

    // Create a third session that stays active
    let active = env
        .manager
        .create_session(
            make_metadata("Active One", &[]),
            SessionScopeConfig::default(),
        )
        .await
        .unwrap();

    // When listing all sessions
    let sessions = env.manager.list_all_sessions(false).await;

    // Then three sessions are returned
    assert_eq!(sessions.len(), 3, "should list active + persisted sessions");

    // And the active session is marked as Running
    let active_info = sessions
        .iter()
        .find(|s| s.id == active.id().as_str())
        .expect("active session should be in the list");
    assert_eq!(
        active_info.status,
        SessionStatus::Running,
        "active session should be Running"
    );
    assert_eq!(active_info.title, "Active One");

    // And the persisted-only sessions are marked as Closed
    for info in sessions.iter().filter(|s| s.id != active.id().as_str()) {
        assert_eq!(
            info.status,
            SessionStatus::Closed,
            "persisted-only sessions should be Closed"
        );
    }
}

#[tokio::test]
async fn should_return_empty_list_when_no_sessions_exist() {
    // Given a fresh session manager with no sessions (active or persisted)
    let env = TestEnv::new().await;

    // When listing all sessions
    let sessions = env.manager.list_all_sessions(false).await;

    // Then an empty list is returned
    assert!(
        sessions.is_empty(),
        "should return empty list when no sessions exist"
    );
}

#[tokio::test]
async fn should_sort_sessions_by_created_at_descending_when_listed() {
    // Given three persisted sessions with different creation times
    let env = TestEnv::new().await;

    let s1 = env
        .manager
        .create_session(make_metadata("First", &[]), SessionScopeConfig::default())
        .await
        .unwrap();
    // Small delay to ensure distinct timestamps
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    let s2 = env
        .manager
        .create_session(make_metadata("Second", &[]), SessionScopeConfig::default())
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    let s3 = env
        .manager
        .create_session(make_metadata("Third", &[]), SessionScopeConfig::default())
        .await
        .unwrap();

    // Close all so they are persisted-only
    env.manager
        .close_session(s1.id(), CloseReason::Exit)
        .await
        .unwrap();
    env.manager
        .close_session(s2.id(), CloseReason::Exit)
        .await
        .unwrap();
    env.manager
        .close_session(s3.id(), CloseReason::Exit)
        .await
        .unwrap();

    // When listing all sessions
    let sessions = env.manager.list_all_sessions(false).await;

    // Then they are sorted by created_at descending (newest first)
    assert_eq!(sessions.len(), 3);
    assert_eq!(sessions[0].title, "Third", "newest session should be first");
    assert_eq!(
        sessions[1].title, "Second",
        "middle session should be second"
    );
    assert_eq!(sessions[2].title, "First", "oldest session should be last");
}

// ===========================================================================
// SessionManager — read-only per-turn usage fold
// ===========================================================================

#[tokio::test]
async fn should_fold_per_turn_usage_when_reading_a_closed_session_wire_log() {
    // Given a persisted session whose wire log carries one completed turn
    // with a single step (four usage buckets) plus an out-of-bracket
    // user injection — `manager_with_hooks(vec![])` is the file's plain
    // "manager + persisted sessions + temp dir kept alive" helper
    // (create installs the wire log eagerly, so `record_op` is durable).
    let (manager, tmp) = manager_with_hooks(vec![]).await;
    let session = manager
        .create_session(
            make_metadata("Per-Turn Usage Fold", &[]),
            SessionScopeConfig::default(),
        )
        .await
        .unwrap();
    let session_id = session.id().clone();
    session
        .record_op(mirri_proto::WireOp::ContextAppendMessage {
            message: serde_json::json!({
                "role": "user",
                "content": "injected",
                "origin": { "kind": "injection" },
            }),
        })
        .await;
    session
        .record_op(mirri_proto::WireOp::TurnPrompt {
            input: serde_json::json!("hi"),
            origin: serde_json::json!({ "kind": "user" }),
        })
        .await;
    session
        .record_op(mirri_proto::WireOp::ContextAppendMessage {
            message: serde_json::json!({
                "role": "user",
                "content": "hi",
                "origin": { "kind": "user" },
            }),
        })
        .await;
    session
        .record_op(mirri_proto::WireOp::ContextAppendLoopEvent {
            event: serde_json::json!({
                "type": "step.end",
                "uuid": "step-1",
                "usage": {
                    "inputTokens": 10,
                    "outputTokens": 5,
                    "cacheReadTokens": 1000,
                    "cacheCreationTokens": 200,
                },
            }),
        })
        .await;
    session
        .record_op(mirri_proto::WireOp::UsageRecord {
            model: "kimi-k2".to_string(),
            usage: serde_json::json!({
                "inputTokens": 10,
                "outputTokens": 5,
                "cacheReadTokens": 1000,
                "cacheCreationTokens": 200,
            }),
            usage_scope: None,
        })
        .await;
    session
        .record_op(mirri_proto::WireOp::TurnEnded {
            turn_id: 1,
            reason: "completed".to_string(),
            error: None,
            duration_ms: Some(1200),
        })
        .await;
    manager
        .close_session(&session_id, CloseReason::Exit)
        .await
        .expect("session closes to disk");
    drop(session);
    drop(manager);

    // When a fresh manager reads the turn-usage fold of the same session
    // (same cwd + persistence dir as the original, so the wire log path
    // resolves identically)
    let app = AppScope::new(AppScopeConfig::default());
    let workspace = app
        .create_workspace_with_config(
            WorkspaceId::new("fold-ws"),
            tmp.path().to_path_buf(),
            WorkspaceScopeConfig::default(),
        )
        .await
        .unwrap();
    let manager = SessionManager::with_persistence_dir(
        workspace,
        tmp.path().join(".mirri").join("sessions"),
        std::sync::Arc::new(mirri_os::LocalFs::new()),
    );
    let fold = manager
        .read_only_turn_usages(&session_id)
        .await
        .expect("the wire log folds");

    // Then the completed turn's buckets, model, and opener anchor are back
    assert_eq!(fold.turns.len(), 1);
    assert_eq!(fold.turns[0].turn_key, "turn-1");
    assert_eq!(fold.turns[0].input_tokens, 10);
    assert_eq!(fold.turns[0].output_tokens, 5);
    assert_eq!(fold.turns[0].cache_read_tokens, 1000);
    assert_eq!(fold.turns[0].cache_creation_tokens, 200);
    assert_eq!(fold.turns[0].models, vec!["kimi-k2"]);
    assert_eq!(fold.openers.len(), 1);
    assert_eq!(fold.openers[0].content_sig, serde_json::json!("hi"));
}
