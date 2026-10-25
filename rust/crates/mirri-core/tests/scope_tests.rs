//! Integration tests for the scope hierarchy (Task 22).
//!
//! These tests verify the explicit nested-struct scope tree that replaces the
//! TS v2 DI container. Tests are organized by scope level and use the
//! Given-When-Then naming convention.

use std::sync::Arc;

use mirri_core::{
    AgentId, AgentScopeConfig, AppScope, AppScopeConfig, LifecycleScope, ProviderType, ScopeTree,
    SessionId, SessionMetadata, TokenUsage, WorkspaceId, WorkspaceScopeConfig, WorkspaceTrustLevel,
};

// ===========================================================================
// LifecycleScope tests
// ===========================================================================

#[test]
fn should_succeed_when_child_scope_is_strictly_higher() {
    assert!(
        LifecycleScope::Workspace
            .must_be_strictly_higher(LifecycleScope::App)
            .is_ok()
    );
    assert!(
        LifecycleScope::Session
            .must_be_strictly_higher(LifecycleScope::Workspace)
            .is_ok()
    );
    assert!(
        LifecycleScope::Agent
            .must_be_strictly_higher(LifecycleScope::Session)
            .is_ok()
    );
}

#[test]
fn should_return_error_when_validating_equal_scopes_as_strictly_higher() {
    let result = LifecycleScope::Session.must_be_strictly_higher(LifecycleScope::Session);
    assert!(result.is_err());
}

#[test]
fn should_return_error_when_child_scope_is_lower_than_parent() {
    let result = LifecycleScope::App.must_be_strictly_higher(LifecycleScope::Session);
    assert!(result.is_err());
}

#[test]
fn should_return_true_when_agent_is_shorter_lived_than_app() {
    assert!(LifecycleScope::Agent.is_shorter_lived_than(LifecycleScope::App));
    assert!(LifecycleScope::Session.is_shorter_lived_than(LifecycleScope::App));
    assert!(!LifecycleScope::App.is_shorter_lived_than(LifecycleScope::Agent));
}

// ===========================================================================
// AppScope tests
// ===========================================================================

#[tokio::test]
async fn should_have_no_workspaces_when_app_scope_is_newly_created() {
    let app = AppScope::new(AppScopeConfig::default());
    let workspaces = app.list_workspaces().await;
    assert!(workspaces.is_empty());
}

#[tokio::test]
async fn should_register_workspace_when_created() {
    let app = AppScope::new(AppScopeConfig::default());
    let id = WorkspaceId::new("ws-1");
    let cwd = std::path::PathBuf::from("/tmp/project");

    let ws = app.create_workspace(id.clone(), cwd.clone()).await.unwrap();
    assert_eq!(ws.id(), &id);
    assert_eq!(ws.cwd(), cwd.as_path());

    let listed = app.list_workspaces().await;
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0], id);
}

#[tokio::test]
async fn should_return_same_arc_when_workspace_is_fetched_by_id() {
    let app = AppScope::new(AppScopeConfig::default());
    let id = WorkspaceId::new("ws-get");
    let created = app
        .create_workspace(id.clone(), std::path::PathBuf::from("/tmp"))
        .await
        .unwrap();

    let fetched = app.get_workspace(&id).await.unwrap();
    assert!(Arc::ptr_eq(&created, &fetched));
}

#[tokio::test]
async fn should_remove_workspace_and_empty_list_when_workspace_is_removed() {
    let app = AppScope::new(AppScopeConfig::default());
    let id = WorkspaceId::new("ws-remove");
    let _ = app
        .create_workspace(id.clone(), std::path::PathBuf::from("/tmp"))
        .await
        .unwrap();

    let removed = app.remove_workspace(&id).await;
    assert!(removed.is_some());

    let listed = app.list_workspaces().await;
    assert!(listed.is_empty());

    let gone = app.get_workspace(&id).await;
    assert!(gone.is_none());
}

#[tokio::test]
async fn should_return_error_when_creating_duplicate_workspace() {
    let app = AppScope::new(AppScopeConfig::default());
    let id = WorkspaceId::new("ws-dup");
    let _ = app
        .create_workspace(id.clone(), std::path::PathBuf::from("/tmp"))
        .await
        .unwrap();

    let result = app
        .create_workspace(id, std::path::PathBuf::from("/tmp"))
        .await;
    assert!(result.is_err());
}

#[tokio::test]
async fn should_set_config_when_creating_workspace_with_explicit_config() {
    let app = AppScope::new(AppScopeConfig::default());
    let id = WorkspaceId::new("ws-config");
    let config = WorkspaceScopeConfig {
        trust_level: WorkspaceTrustLevel::Trusted,
        ..Default::default()
    };
    let ws = app
        .create_workspace_with_config(id.clone(), std::path::PathBuf::from("/tmp"), config)
        .await
        .unwrap();
    assert_eq!(ws.config().trust_level, WorkspaceTrustLevel::Trusted);
}

// ===========================================================================
// WorkspaceScope tests
// ===========================================================================

#[tokio::test]
async fn should_maintain_parent_child_relationship_when_session_is_created_in_workspace() {
    let app = AppScope::new(AppScopeConfig::default());
    let ws = app
        .create_workspace(WorkspaceId::new("ws-1"), std::path::PathBuf::from("/tmp"))
        .await
        .unwrap();

    let session_id = SessionId::new("sess-1");
    let metadata = SessionMetadata::default();
    let session = ws
        .create_session(session_id.clone(), metadata)
        .await
        .unwrap();

    assert_eq!(session.id(), &session_id);

    let sessions = ws.list_sessions().await;
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0], session_id);
}

#[tokio::test]
async fn should_return_same_arc_when_session_is_fetched_by_id() {
    let app = AppScope::new(AppScopeConfig::default());
    let ws = app
        .create_workspace(WorkspaceId::new("ws-2"), std::path::PathBuf::from("/tmp"))
        .await
        .unwrap();

    let sid = SessionId::new("sess-get");
    let created = ws
        .create_session(sid.clone(), SessionMetadata::default())
        .await
        .unwrap();

    let fetched = ws.get_session(&sid).await.unwrap();
    assert!(Arc::ptr_eq(&created, &fetched));
}

#[tokio::test]
async fn should_remove_session_and_empty_list_when_session_is_removed() {
    let app = AppScope::new(AppScopeConfig::default());
    let ws = app
        .create_workspace(WorkspaceId::new("ws-3"), std::path::PathBuf::from("/tmp"))
        .await
        .unwrap();

    let sid = SessionId::new("sess-remove");
    let _ = ws
        .create_session(sid.clone(), SessionMetadata::default())
        .await
        .unwrap();

    let removed = ws.remove_session(&sid).await;
    assert!(removed.is_some());

    assert!(ws.list_sessions().await.is_empty());
    assert!(ws.get_session(&sid).await.is_none());
}

#[tokio::test]
async fn should_return_error_when_creating_duplicate_session() {
    let app = AppScope::new(AppScopeConfig::default());
    let ws = app
        .create_workspace(WorkspaceId::new("ws-dup"), std::path::PathBuf::from("/tmp"))
        .await
        .unwrap();

    let sid = SessionId::new("sess-dup");
    let _ = ws
        .create_session(sid.clone(), SessionMetadata::default())
        .await
        .unwrap();

    let result = ws.create_session(sid, SessionMetadata::default()).await;
    assert!(result.is_err());
}

#[tokio::test]
async fn should_list_all_sessions_when_multiple_sessions_exist_in_same_workspace() {
    let app = AppScope::new(AppScopeConfig::default());
    let ws = app
        .create_workspace(
            WorkspaceId::new("ws-multi"),
            std::path::PathBuf::from("/tmp"),
        )
        .await
        .unwrap();

    for i in 0..5 {
        let sid = SessionId::new(format!("sess-{i}"));
        ws.create_session(sid, SessionMetadata::default())
            .await
            .unwrap();
    }

    let sessions = ws.list_sessions().await;
    assert_eq!(sessions.len(), 5);
}

// ===========================================================================
// SessionScope tests
// ===========================================================================

#[tokio::test]
async fn should_maintain_parent_child_relationship_when_agent_is_created_in_session() {
    let app = AppScope::new(AppScopeConfig::default());
    let ws = app
        .create_workspace(WorkspaceId::new("ws-a"), std::path::PathBuf::from("/tmp"))
        .await
        .unwrap();
    let session = ws
        .create_session(SessionId::new("sess-a"), SessionMetadata::default())
        .await
        .unwrap();

    let agent_id = AgentId::new("agent-1");
    let agent = session
        .create_agent(agent_id.clone(), AgentScopeConfig::default())
        .await
        .unwrap();

    assert_eq!(agent.id(), &agent_id);

    let agents = session.list_agents().await;
    assert_eq!(agents.len(), 1);
    assert_eq!(agents[0], agent_id);
}

#[tokio::test]
async fn should_return_same_arc_when_agent_is_fetched_by_id() {
    let app = AppScope::new(AppScopeConfig::default());
    let ws = app
        .create_workspace(WorkspaceId::new("ws-b"), std::path::PathBuf::from("/tmp"))
        .await
        .unwrap();
    let session = ws
        .create_session(SessionId::new("sess-b"), SessionMetadata::default())
        .await
        .unwrap();

    let aid = AgentId::new("agent-get");
    let created = session
        .create_agent(aid.clone(), AgentScopeConfig::default())
        .await
        .unwrap();

    let fetched = session.get_agent(&aid).await.unwrap();
    assert!(Arc::ptr_eq(&created, &fetched));
}

#[tokio::test]
async fn should_remove_agent_and_empty_list_when_agent_is_removed() {
    let app = AppScope::new(AppScopeConfig::default());
    let ws = app
        .create_workspace(WorkspaceId::new("ws-c"), std::path::PathBuf::from("/tmp"))
        .await
        .unwrap();
    let session = ws
        .create_session(SessionId::new("sess-c"), SessionMetadata::default())
        .await
        .unwrap();

    let aid = AgentId::new("agent-remove");
    let _ = session
        .create_agent(aid.clone(), AgentScopeConfig::default())
        .await
        .unwrap();

    let removed = session.remove_agent(&aid).await;
    assert!(removed.is_some());

    assert!(session.list_agents().await.is_empty());
    assert!(session.get_agent(&aid).await.is_none());
}

#[tokio::test]
async fn should_return_error_when_creating_duplicate_agent() {
    let app = AppScope::new(AppScopeConfig::default());
    let ws = app
        .create_workspace(WorkspaceId::new("ws-d"), std::path::PathBuf::from("/tmp"))
        .await
        .unwrap();
    let session = ws
        .create_session(SessionId::new("sess-d"), SessionMetadata::default())
        .await
        .unwrap();

    let aid = AgentId::new("agent-dup");
    let _ = session
        .create_agent(aid.clone(), AgentScopeConfig::default())
        .await
        .unwrap();

    let result = session.create_agent(aid, AgentScopeConfig::default()).await;
    assert!(result.is_err());
}

#[tokio::test]
async fn should_list_all_agents_when_multiple_agents_exist_in_same_session() {
    let app = AppScope::new(AppScopeConfig::default());
    let ws = app
        .create_workspace(WorkspaceId::new("ws-e"), std::path::PathBuf::from("/tmp"))
        .await
        .unwrap();
    let session = ws
        .create_session(SessionId::new("sess-e"), SessionMetadata::default())
        .await
        .unwrap();

    for i in 0..5 {
        let aid = AgentId::new(format!("agent-{i}"));
        session
            .create_agent(aid, AgentScopeConfig::default())
            .await
            .unwrap();
    }

    let agents = session.list_agents().await;
    assert_eq!(agents.len(), 5);
}

#[tokio::test]
async fn should_return_clone_when_session_metadata_is_read() {
    let app = AppScope::new(AppScopeConfig::default());
    let ws = app
        .create_workspace(
            WorkspaceId::new("ws-meta"),
            std::path::PathBuf::from("/tmp"),
        )
        .await
        .unwrap();

    let metadata = SessionMetadata {
        title: "My Session".to_string(),
        ..Default::default()
    };
    let session = ws
        .create_session(SessionId::new("sess-meta"), metadata)
        .await
        .unwrap();

    let read = session.metadata().await;
    assert_eq!(read.title, "My Session");
}

#[tokio::test]
async fn should_update_title_and_refresh_updated_at_when_metadata_is_updated() {
    let app = AppScope::new(AppScopeConfig::default());
    let ws = app
        .create_workspace(
            WorkspaceId::new("ws-update"),
            std::path::PathBuf::from("/tmp"),
        )
        .await
        .unwrap();
    let session = ws
        .create_session(SessionId::new("sess-update"), SessionMetadata::default())
        .await
        .unwrap();

    let original = session.metadata().await;

    // Small delay so updated_at is measurably different.
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;

    session
        .update_metadata(|m| {
            m.title = "Updated Title".to_string();
        })
        .await;

    let updated = session.metadata().await;
    assert_eq!(updated.title, "Updated Title");
    assert!(updated.updated_at > original.updated_at);
    assert_eq!(updated.created_at, original.created_at);
}

// ===========================================================================
// AgentScope tests
// ===========================================================================

#[tokio::test]
async fn should_have_all_engine_components_initialized_when_agent_scope_is_newly_created() {
    let app = AppScope::new(AppScopeConfig::default());
    let ws = app
        .create_workspace(
            WorkspaceId::new("ws-agent"),
            std::path::PathBuf::from("/tmp"),
        )
        .await
        .unwrap();
    let session = ws
        .create_session(SessionId::new("sess-agent"), SessionMetadata::default())
        .await
        .unwrap();

    let agent = session
        .create_agent(AgentId::new("agent-init"), AgentScopeConfig::default())
        .await
        .unwrap();

    // ContextMemory should be empty initially.
    {
        let cm = agent.context_memory();
        let guard = cm.lock().unwrap();
        assert!(guard.history().is_empty());
        assert_eq!(guard.token_count(), 0);
    }

    // ToolManager should have all built-in tools registered and all
    // enabled by default (no explicit allowlist means enable-all).
    let tm = agent.tool_manager();
    {
        let guard = tm.read();
        assert_eq!(guard.all_tool_names().len(), 30);
        assert!(
            !guard.enabled_tools().is_empty(),
            "default config should enable all registered tools"
        );
        assert!(
            !guard.loop_tools().is_empty(),
            "tools should be visible to the LLM by default"
        );
    }

    // Session-scoped services are accessible and initialized.
    assert!(agent.todo_list().get().is_empty());
    assert!(agent.goal().get().is_none());
    assert!(!agent.plan().is_active());
    assert!(agent.tasks().list().is_empty());

    // PermissionManager should be in default mode.
    let pm = agent.permission_manager();
    assert_eq!(pm.mode(), mirri_core::PermissionMode::default());
}

#[tokio::test]
async fn should_point_to_same_instance_when_getting_arc_handles_from_agent_scope() {
    let app = AppScope::new(AppScopeConfig::default());
    let ws = app
        .create_workspace(WorkspaceId::new("ws-arc"), std::path::PathBuf::from("/tmp"))
        .await
        .unwrap();
    let session = ws
        .create_session(SessionId::new("sess-arc"), SessionMetadata::default())
        .await
        .unwrap();

    let agent = session
        .create_agent(AgentId::new("agent-arc"), AgentScopeConfig::default())
        .await
        .unwrap();

    let cm1 = agent.context_memory();
    let cm2 = agent.context_memory();
    assert!(Arc::ptr_eq(&cm1, &cm2));

    let tm1 = agent.tool_manager();
    let tm2 = agent.tool_manager();
    assert!(Arc::ptr_eq(&tm1, &tm2));

    let pm1 = agent.permission_manager();
    let pm2 = agent.permission_manager();
    assert!(Arc::ptr_eq(&pm1, &pm2));
}

#[tokio::test]
async fn should_be_accessible_when_agent_config_has_custom_values() {
    let app = AppScope::new(AppScopeConfig::default());
    let ws = app
        .create_workspace(WorkspaceId::new("ws-cfg"), std::path::PathBuf::from("/tmp"))
        .await
        .unwrap();
    let session = ws
        .create_session(SessionId::new("sess-cfg"), SessionMetadata::default())
        .await
        .unwrap();

    let config = AgentScopeConfig {
        model_id: "gpt-4".to_string(),
        provider_type: ProviderType::OpenAi,
        system_prompt: Some("You are a helpful assistant.".to_string()),
        max_turns: 50,
        permission_mode: mirri_core::PermissionMode::Yolo,
        ..Default::default()
    };
    let agent = session
        .create_agent(AgentId::new("agent-cfg"), config)
        .await
        .unwrap();

    let config = agent.config();
    let cfg = config.read().unwrap_or_else(|e| e.into_inner());
    assert_eq!(cfg.model_id, "gpt-4");
    assert_eq!(cfg.provider_type, ProviderType::OpenAi);
    // The system prompt may have AGENTS.md context appended by create_agent.
    let prompt = cfg.system_prompt.as_ref().unwrap();
    assert!(
        prompt.starts_with("You are a helpful assistant."),
        "prompt should start with the original text: {prompt}"
    );
    assert_eq!(cfg.max_turns, 50);
}

// ===========================================================================
// ScopeTree tests
// ===========================================================================

#[tokio::test]
async fn should_access_all_scope_levels_when_navigating_full_hierarchy() {
    let tree = ScopeTree::new(AppScopeConfig::default());
    let app = tree.app();

    let ws = app
        .create_workspace(
            WorkspaceId::new("tree-ws"),
            std::path::PathBuf::from("/tmp"),
        )
        .await
        .unwrap();
    let session = ws
        .create_session(SessionId::new("tree-sess"), SessionMetadata::default())
        .await
        .unwrap();
    let agent = session
        .create_agent(AgentId::new("tree-agent"), AgentScopeConfig::default())
        .await
        .unwrap();

    // Verify the full chain is accessible.
    assert_eq!(agent.id().as_str(), "tree-agent");
    assert_eq!(session.id().as_str(), "tree-sess");
    assert_eq!(ws.id().as_str(), "tree-ws");
}

#[tokio::test]
async fn should_share_state_when_both_clone_refs_access_app() {
    let tree = ScopeTree::new(AppScopeConfig::default());
    let tree2 = tree.clone();

    tree.app()
        .create_workspace(
            WorkspaceId::new("shared-ws"),
            std::path::PathBuf::from("/tmp"),
        )
        .await
        .unwrap();

    let listed = tree2.app().list_workspaces().await;
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].as_str(), "shared-ws");
}

// ===========================================================================
// Concurrency tests
// ===========================================================================

#[tokio::test]
async fn should_register_all_workspaces_when_created_concurrently() {
    let app = Arc::new(AppScope::new(AppScopeConfig::default()));
    let mut handles = Vec::new();

    for i in 0..10 {
        let app_clone = Arc::clone(&app);
        handles.push(tokio::spawn(async move {
            let id = WorkspaceId::new(format!("concurrent-ws-{i}"));
            app_clone
                .create_workspace(id, std::path::PathBuf::from(format!("/tmp/ws-{i}")))
                .await
                .unwrap();
        }));
    }

    for handle in handles {
        handle.await.unwrap();
    }

    let workspaces = app.list_workspaces().await;
    assert_eq!(workspaces.len(), 10);
}

#[tokio::test]
async fn should_register_all_sessions_when_created_concurrently() {
    let app = AppScope::new(AppScopeConfig::default());
    let ws = app
        .create_workspace(
            WorkspaceId::new("ws-concurrent"),
            std::path::PathBuf::from("/tmp"),
        )
        .await
        .unwrap();
    let ws = Arc::new(ws);

    let mut handles = Vec::new();
    for i in 0..10 {
        let ws_clone = Arc::clone(&ws);
        handles.push(tokio::spawn(async move {
            let sid = SessionId::new(format!("concurrent-sess-{i}"));
            ws_clone
                .create_session(sid, SessionMetadata::default())
                .await
                .unwrap();
        }));
    }

    for handle in handles {
        handle.await.unwrap();
    }

    let sessions = ws.list_sessions().await;
    assert_eq!(sessions.len(), 10);
}

#[tokio::test]
async fn should_register_all_agents_when_created_concurrently() {
    let app = AppScope::new(AppScopeConfig::default());
    let ws = app
        .create_workspace(
            WorkspaceId::new("ws-concurrent-agent"),
            std::path::PathBuf::from("/tmp"),
        )
        .await
        .unwrap();
    let session = ws
        .create_session(
            SessionId::new("sess-concurrent-agent"),
            SessionMetadata::default(),
        )
        .await
        .unwrap();
    let session = Arc::new(session);

    let mut handles = Vec::new();
    for i in 0..10 {
        let session_clone = Arc::clone(&session);
        handles.push(tokio::spawn(async move {
            let aid = AgentId::new(format!("concurrent-agent-{i}"));
            session_clone
                .create_agent(aid, AgentScopeConfig::default())
                .await
                .unwrap();
        }));
    }

    for handle in handles {
        handle.await.unwrap();
    }

    let agents = session.list_agents().await;
    assert_eq!(agents.len(), 10);
}

#[tokio::test]
async fn should_not_deadlock_when_concurrent_reads_and_writes_on_workspace_map() {
    let app = Arc::new(AppScope::new(AppScopeConfig::default()));

    // Pre-populate.
    for i in 0..5 {
        app.create_workspace(
            WorkspaceId::new(format!("rw-ws-{i}")),
            std::path::PathBuf::from("/tmp"),
        )
        .await
        .unwrap();
    }

    let mut handles = Vec::new();

    // Readers.
    for _ in 0..5 {
        let app_clone = Arc::clone(&app);
        handles.push(tokio::spawn(async move {
            for _ in 0..10 {
                let _ = app_clone.list_workspaces().await;
            }
        }));
    }

    // Writers.
    for i in 0..5 {
        let app_clone = Arc::clone(&app);
        handles.push(tokio::spawn(async move {
            let id = WorkspaceId::new(format!("rw-ws-new-{i}"));
            let _ = app_clone
                .create_workspace(id, std::path::PathBuf::from("/tmp"))
                .await;
        }));
    }

    for handle in handles {
        handle.await.unwrap();
    }

    // Should have at least the 5 original + 5 new = 10 workspaces.
    let workspaces = app.list_workspaces().await;
    assert!(workspaces.len() >= 10);
}

#[tokio::test]
async fn should_serialize_correctly_when_concurrent_access_to_shared_agent_context_memory() {
    use mirri_core::{ContentPart, PromptOrigin};

    let app = AppScope::new(AppScopeConfig::default());
    let ws = app
        .create_workspace(WorkspaceId::new("ws-ctx"), std::path::PathBuf::from("/tmp"))
        .await
        .unwrap();
    let session = ws
        .create_session(SessionId::new("sess-ctx"), SessionMetadata::default())
        .await
        .unwrap();
    let agent = session
        .create_agent(AgentId::new("agent-ctx"), AgentScopeConfig::default())
        .await
        .unwrap();

    let cm = agent.context_memory();

    // Concurrent writes to ContextMemory via std::sync::Mutex.
    let cm_clone = Arc::clone(&cm);
    let writer = tokio::task::spawn_blocking(move || {
        for i in 0..10 {
            let mut guard = cm_clone.lock().unwrap();
            guard.append_user_message(
                vec![ContentPart::Text {
                    text: format!("Message {i}"),
                }],
                PromptOrigin::User,
            );
        }
    });
    writer.await.unwrap();

    let guard = cm.lock().unwrap();
    assert_eq!(guard.history().len(), 10);
}

// ===========================================================================
// SessionScope::runtime_extras tests
// ===========================================================================

/// Given a session holding a main agent and a subagent, each with recorded
/// usage in its own usage service, when the runtime extras are read, then
/// the usage totals sum **every** agent (SDR-002 full aggregation) instead
/// of the first agent only.
#[tokio::test]
async fn should_sum_usage_across_all_agents_when_runtime_extras_called() {
    let app = AppScope::new(AppScopeConfig::default());
    let ws = app
        .create_workspace(
            WorkspaceId::new("ws-usage-agg"),
            std::path::PathBuf::from("/tmp"),
        )
        .await
        .unwrap();
    let session = ws
        .create_session(SessionId::new("sess-usage-agg"), SessionMetadata::default())
        .await
        .unwrap();

    let main = session
        .create_agent(AgentId::new("main"), AgentScopeConfig::default())
        .await
        .unwrap();
    let sub = session
        .create_agent(AgentId::new("sub-agent"), AgentScopeConfig::default())
        .await
        .unwrap();

    main.usage_service().lock().unwrap().record(
        "model-main",
        TokenUsage {
            input_tokens: 100,
            output_tokens: 40,
            cache_read_tokens: 10,
            cache_creation_tokens: 5,
        },
        None,
    );
    sub.usage_service().lock().unwrap().record(
        "model-sub",
        TokenUsage {
            input_tokens: 30,
            output_tokens: 20,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
        },
        None,
    );

    // When the runtime extras are read
    let extras = session.runtime_extras().await;

    // Then usage sums both agents — not the main agent's 100/40/10/5 alone
    assert_eq!(extras.usage.input_tokens, 130);
    assert_eq!(extras.usage.output_tokens, 60);
    assert_eq!(extras.usage.cache_read_tokens, 10);
    assert_eq!(extras.usage.cache_creation_tokens, 5);
}

/// Given a session whose main agent and subagent each recorded usage, when
/// the runtime extras are read, then the cold-start baseline is the MAIN
/// agent's first call (SDR-004) — the subagent's first call never moves it.
#[tokio::test]
async fn should_take_first_call_from_main_agent_when_runtime_extras_called() {
    let app = AppScope::new(AppScopeConfig::default());
    let ws = app
        .create_workspace(
            WorkspaceId::new("ws-usage-firstcall"),
            std::path::PathBuf::from("/tmp"),
        )
        .await
        .unwrap();
    let session = ws
        .create_session(
            SessionId::new("sess-usage-firstcall"),
            SessionMetadata::default(),
        )
        .await
        .unwrap();

    let main = session
        .create_agent(AgentId::new("main"), AgentScopeConfig::default())
        .await
        .unwrap();
    let sub = session
        .create_agent(AgentId::new("sub-agent"), AgentScopeConfig::default())
        .await
        .unwrap();

    main.usage_service().lock().unwrap().record(
        "model-main",
        TokenUsage {
            input_tokens: 100,
            output_tokens: 40,
            cache_read_tokens: 10,
            cache_creation_tokens: 5,
        },
        None,
    );
    sub.usage_service().lock().unwrap().record(
        "model-sub",
        TokenUsage {
            input_tokens: 30,
            output_tokens: 20,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
        },
        None,
    );

    // When the runtime extras are read
    let extras = session.runtime_extras().await;

    // Then the baseline is the main agent's first call, not the subagent's
    let first = extras.first_call.expect("main agent's first call");
    assert_eq!(first.input_tokens, 100);
    assert_eq!(first.output_tokens, 40);
    assert_eq!(first.cache_read_tokens, 10);
    // And the totals still sum every agent (SDR-002)
    assert_eq!(extras.usage.input_tokens, 130);
}

/// Given a session whose subagent recorded usage while the main agent
/// recorded none, when the runtime extras are read, then `model` and
/// `message_count` still describe the **first** agent — only usage is
/// session-aggregated (SDR-002: the "current turn" view stays main-agent).
#[tokio::test]
async fn should_keep_model_from_first_agent_when_usage_is_summed_across_agents() {
    let app = AppScope::new(AppScopeConfig::default());
    let ws = app
        .create_workspace(
            WorkspaceId::new("ws-usage-first"),
            std::path::PathBuf::from("/tmp"),
        )
        .await
        .unwrap();
    let session = ws
        .create_session(
            SessionId::new("sess-usage-first"),
            SessionMetadata::default(),
        )
        .await
        .unwrap();

    let _main = session
        .create_agent(
            AgentId::new("main"),
            AgentScopeConfig {
                model_id: "model-main".to_string(),
                ..AgentScopeConfig::default()
            },
        )
        .await
        .unwrap();
    let sub = session
        .create_agent(
            AgentId::new("sub-agent"),
            AgentScopeConfig {
                model_id: "model-sub".to_string(),
                ..AgentScopeConfig::default()
            },
        )
        .await
        .unwrap();

    sub.usage_service().lock().unwrap().record(
        "model-sub",
        TokenUsage {
            input_tokens: 999,
            output_tokens: 999,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
        },
        None,
    );

    // When the runtime extras are read
    let extras = session.runtime_extras().await;

    // Then model stays the first agent's, and usage still counts the subagent
    assert_eq!(extras.model, "model-main");
    assert_eq!(extras.message_count, 0);
    assert_eq!(extras.usage.input_tokens, 999);
}

/// Given a session with no agents, when the runtime extras are read, then
/// every field answers its neutral value (empty model, zeroed usage).
#[tokio::test]
async fn should_return_neutral_usage_when_session_has_no_agents() {
    let app = AppScope::new(AppScopeConfig::default());
    let ws = app
        .create_workspace(
            WorkspaceId::new("ws-usage-empty"),
            std::path::PathBuf::from("/tmp"),
        )
        .await
        .unwrap();
    let session = ws
        .create_session(
            SessionId::new("sess-usage-empty"),
            SessionMetadata::default(),
        )
        .await
        .unwrap();

    // When the runtime extras are read
    let extras = session.runtime_extras().await;

    // Then every field is neutral
    assert_eq!(extras.model, "");
    assert_eq!(extras.usage, TokenUsage::default());
    assert_eq!(extras.message_count, 0);
}
