use super::*;
use mirri_persistence::{AppendLogStore, JsonlAppendLog};
use mirri_proto::WireOp;
use serde_json::json;

/// Create a `SessionManager` backed by a temp directory and return both
/// the manager and the temp dir (so the dir lives as long as the test).
fn make_manager() -> (SessionManager, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    let workspace = Arc::new(WorkspaceScope::new(
        crate::scope::WorkspaceId::new("test-ws"),
        tmp.path().to_path_buf(),
        crate::scope::WorkspaceScopeConfig::default(),
    ));
    let sessions_dir = tmp.path().join(".mirri").join("sessions");
    let manager = SessionManager::with_persistence_dir(
        workspace,
        sessions_dir,
        Arc::new(mirri_os::LocalFs::new()),
    );
    (manager, tmp)
}

/// The model-record seed for the resume fixtures (spec 11 R4): a resumed
/// main agent needs a resolvable model — the wire log's `config.update`
/// model record, else the base config's model. These fixtures write wire
/// logs by hand (or create sessions that never conversed through the
/// embedded host), so they append the model record a real embedded create
/// would have written.
fn model_record_op() -> WireOp {
    WireOp::ConfigUpdate {
        model_alias: Some("test-resume-model".to_string()),
        profile_name: None,
        thinking_effort: None,
        system_prompt: None,
        disallowed_tools: None,
    }
}

#[tokio::test]
async fn should_write_sessions_under_v3_root_when_manager_creates() {
    // Given a SessionManager::new(workspace) over a throwaway workspace —
    // the default-root construction every composition root without an
    // injected home uses (ADR-006-R2: the rust engine's storage root is
    // `.mirri/v3_sessions`, physically separate from the TS engine's
    // `.mirri/sessions`)
    let tmp = tempfile::tempdir().unwrap();
    let workspace = Arc::new(WorkspaceScope::new(
        crate::scope::WorkspaceId::new("v3-root-test"),
        tmp.path().to_path_buf(),
        crate::scope::WorkspaceScopeConfig::default(),
    ));
    let manager = SessionManager::new(workspace);

    // When a session is created and persisted
    let session = manager
        .create_session(SessionMetadata::default(), SessionScopeConfig::default())
        .await
        .unwrap();

    // Then the session directory lands under .mirri/v3_sessions/<bucket>/<id>
    // (the bucketed TS-parity layout, now rooted at the v3 segment)
    let bucket = encode_work_dir_key(tmp.path());
    let session_dir = tmp
        .path()
        .join(".mirri")
        .join("v3_sessions")
        .join(&bucket)
        .join(session.id().as_str());
    assert!(
        session_dir.join("state.json").exists(),
        "the session must persist under {} (got bucket {bucket})",
        session_dir.display()
    );
    assert!(session_dir.join("wire.jsonl").exists());

    // And nothing lands under the legacy TS-era `.mirri/sessions` root —
    // cross-engine session sharing is physically impossible
    assert!(
        !tmp.path().join(".mirri").join("sessions").exists(),
        "the legacy sessions root must stay untouched by the rust engine"
    );
}

#[tokio::test]
async fn should_restore_permission_mode_when_resuming_session() {
    // Given a session with a wire log that sets permission mode to "auto"
    let (manager, _tmp) = make_manager();
    let session_id = SessionId::new("test-resume-perm");

    // The session dir is the manager's bucketed layout (the manager
    // derives the workspace key from the workspace cwd — tmp here).
    let session_dir = manager.persistence().session_dir(&session_id);
    tokio::fs::create_dir_all(&session_dir).await.unwrap();

    let wire_path = session_dir.join("wire.jsonl");
    let mut log = JsonlAppendLog::create(&wire_path).await.unwrap();
    log.append(&WireOp::Metadata {
        protocol_version: None,
        created_at: None,
    })
    .await
    .unwrap();
    log.append(&model_record_op()).await.unwrap();
    log.append(&WireOp::PermissionSetMode {
        mode: "auto".to_string(),
    })
    .await
    .unwrap();
    drop(log);

    manager
        .persistence()
        .save_state(&session_id, &SessionMetadata::default())
        .await
        .unwrap();

    // When the session is resumed
    let session = manager.resume_session(&session_id).await.unwrap();

    // Then the recreated agent has the restored permission mode
    let agent = session
        .get_agent(&AgentId::new(MAIN_AGENT_ID))
        .await
        .expect("main agent should exist");
    assert_eq!(agent.permission_manager().mode(), PermissionMode::Auto);
}

#[tokio::test]
async fn should_restore_usage_totals_when_resuming_session() {
    // Given a session with a wire log that records token usage
    let (manager, _tmp) = make_manager();
    let session_id = SessionId::new("test-resume-usage");

    // The session dir is the manager's bucketed layout (the manager
    // derives the workspace key from the workspace cwd — tmp here).
    let session_dir = manager.persistence().session_dir(&session_id);
    tokio::fs::create_dir_all(&session_dir).await.unwrap();

    let wire_path = session_dir.join("wire.jsonl");
    let mut log = JsonlAppendLog::create(&wire_path).await.unwrap();
    log.append(&WireOp::Metadata {
        protocol_version: None,
        created_at: None,
    })
    .await
    .unwrap();
    log.append(&model_record_op()).await.unwrap();
    log.append(&WireOp::UsageRecord {
            model: "claude-sonnet".to_string(),
            usage: json!({"inputTokens": 500, "outputTokens": 200, "cacheReadTokens": 50, "cacheCreationTokens": 10}),
            usage_scope: None,
        })
        .await
        .unwrap();
    log.append(&WireOp::UsageRecord {
            model: "claude-sonnet".to_string(),
            usage: json!({"inputTokens": 300, "outputTokens": 100, "cacheReadTokens": 70, "cacheCreationTokens": 20}),
            usage_scope: None,
        })
        .await
        .unwrap();
    drop(log);

    manager
        .persistence()
        .save_state(&session_id, &SessionMetadata::default())
        .await
        .unwrap();

    // When the session is resumed
    let session = manager.resume_session(&session_id).await.unwrap();

    // Then the recreated agent's usage service reflects the accumulated totals
    let agent = session
        .get_agent(&AgentId::new(MAIN_AGENT_ID))
        .await
        .expect("main agent should exist");
    let usage_service = agent.usage_service();
    let status = usage_service.lock().unwrap().status();
    assert_eq!(status.total.input_tokens, 800);
    assert_eq!(status.total.output_tokens, 300);
    // Then the cache buckets accumulated in the reducer survive the
    // resume backfill into the agent's usage service
    assert_eq!(status.total.cache_read_tokens, 120);
    assert_eq!(status.total.cache_creation_tokens, 30);
}

#[tokio::test]
async fn should_restore_first_call_when_resuming_session() {
    // Given a session with a wire log whose FIRST usage record is the
    // cold-start baseline
    let (manager, _tmp) = make_manager();
    let session_id = SessionId::new("test-resume-first-call");

    let session_dir = manager.persistence().session_dir(&session_id);
    tokio::fs::create_dir_all(&session_dir).await.unwrap();

    let wire_path = session_dir.join("wire.jsonl");
    let mut log = JsonlAppendLog::create(&wire_path).await.unwrap();
    log.append(&WireOp::Metadata {
        protocol_version: None,
        created_at: None,
    })
    .await
    .unwrap();
    log.append(&model_record_op()).await.unwrap();
    log.append(&WireOp::UsageRecord {
            model: "claude-sonnet".to_string(),
            usage: json!({"inputTokens": 500, "outputTokens": 200, "cacheReadTokens": 50, "cacheCreationTokens": 10}),
            usage_scope: None,
        })
        .await
        .unwrap();
    log.append(&WireOp::UsageRecord {
            model: "claude-sonnet".to_string(),
            usage: json!({"inputTokens": 300, "outputTokens": 100, "cacheReadTokens": 70, "cacheCreationTokens": 20}),
            usage_scope: None,
        })
        .await
        .unwrap();
    drop(log);

    manager
        .persistence()
        .save_state(&session_id, &SessionMetadata::default())
        .await
        .unwrap();

    // When the session is resumed
    let session = manager.resume_session(&session_id).await.unwrap();

    // Then the recreated agent carries the FIRST record as the cold-start
    // baseline, and the resume seed did not overwrite it
    let agent = session
        .get_agent(&AgentId::new(MAIN_AGENT_ID))
        .await
        .expect("main agent should exist");
    let status = agent.usage_service().lock().unwrap().status();
    let first = status
        .first_call
        .expect("first call must survive the resume");
    assert_eq!(first.input_tokens, 500);
    assert_eq!(first.output_tokens, 200);
    assert_eq!(first.cache_read_tokens, 50);
    assert_eq!(first.cache_creation_tokens, 10);
    // And the totals still include both records
    assert_eq!(status.total.input_tokens, 800);
}

#[tokio::test]
async fn should_restore_goal_when_resuming_session() {
    // Given a session with a wire log that creates a goal
    let (manager, _tmp) = make_manager();
    let session_id = SessionId::new("test-resume-goal");

    // The session dir is the manager's bucketed layout (the manager
    // derives the workspace key from the workspace cwd — tmp here).
    let session_dir = manager.persistence().session_dir(&session_id);
    tokio::fs::create_dir_all(&session_dir).await.unwrap();

    let wire_path = session_dir.join("wire.jsonl");
    let mut log = JsonlAppendLog::create(&wire_path).await.unwrap();
    log.append(&WireOp::Metadata {
        protocol_version: None,
        created_at: None,
    })
    .await
    .unwrap();
    log.append(&model_record_op()).await.unwrap();
    log.append(&WireOp::GoalCreate {
        goal_id: "g1".to_string(),
        objective: "Ship the feature".to_string(),
        completion_criterion: Some("All tests pass".to_string()),
        status: None,
        actor: None,
        budget_limits: Some(json!({"turns": 50, "tokens": 100000})),
    })
    .await
    .unwrap();
    drop(log);

    manager
        .persistence()
        .save_state(&session_id, &SessionMetadata::default())
        .await
        .unwrap();

    // When the session is resumed
    let session = manager.resume_session(&session_id).await.unwrap();

    // Then the session's goal manager has the restored goal
    let goal = session.goal().get();
    let goal = goal.expect("goal should be restored");
    assert_eq!(goal.objective, "Ship the feature");
    assert_eq!(goal.completion_criterion.as_deref(), Some("All tests pass"));
    assert_eq!(goal.status, GoalStatus::Active);
    let budget = goal.budget.expect("budget should be restored");
    assert_eq!(budget.turns, Some(50));
    assert_eq!(budget.tokens, Some(100_000));
}

#[tokio::test]
async fn should_restore_goal_usage_counters_when_resuming_session() {
    // Given a session with a wire log that creates a budgeted goal and
    // records usage-only goal.update ops (the turn-accounting write side)
    let (manager, _tmp) = make_manager();
    let session_id = SessionId::new("test-resume-goal-usage");

    // The session dir is the manager's bucketed layout (the manager
    // derives the workspace key from the workspace cwd — tmp here).
    let session_dir = manager.persistence().session_dir(&session_id);
    tokio::fs::create_dir_all(&session_dir).await.unwrap();

    let wire_path = session_dir.join("wire.jsonl");
    let mut log = JsonlAppendLog::create(&wire_path).await.unwrap();
    log.append(&WireOp::Metadata {
        protocol_version: None,
        created_at: None,
    })
    .await
    .unwrap();
    log.append(&model_record_op()).await.unwrap();
    log.append(&WireOp::GoalCreate {
        goal_id: "g1".to_string(),
        objective: "Land the parity batch".to_string(),
        completion_criterion: None,
        status: None,
        actor: None,
        budget_limits: Some(json!({"turns": 40, "tokens": 100000})),
    })
    .await
    .unwrap();
    log.append(&WireOp::GoalUpdate {
        goal_id: None,
        status: None,
        reason: None,
        turns_used: Some(30),
        tokens_used: Some(4_500),
        wall_clock_ms: None,
        budget_limits: None,
        actor: Some("runtime".to_string()),
    })
    .await
    .unwrap();
    drop(log);

    manager
        .persistence()
        .save_state(&session_id, &SessionMetadata::default())
        .await
        .unwrap();

    // When the session is resumed
    let session = manager.resume_session(&session_id).await.unwrap();

    // Then the goal manager's usage counters continue from the replayed
    // totals — the budget enforcement does not reopen from zero
    let goal = session.goal();
    assert_eq!(goal.used_turns(), 30);
    assert_eq!(goal.used_tokens(), 4_500);
    assert!(
        !goal.is_budget_exhausted(),
        "30 of 40 turns must leave budget remaining after the resume"
    );
    assert_eq!(goal.get().unwrap().objective, "Land the parity batch");
}

#[tokio::test]
async fn should_restore_plan_mode_when_resuming_session() {
    // Given a session with a wire log that enters plan mode
    let (manager, _tmp) = make_manager();
    let session_id = SessionId::new("test-resume-plan");

    // The session dir is the manager's bucketed layout (the manager
    // derives the workspace key from the workspace cwd — tmp here).
    let session_dir = manager.persistence().session_dir(&session_id);
    tokio::fs::create_dir_all(&session_dir).await.unwrap();

    let wire_path = session_dir.join("wire.jsonl");
    let mut log = JsonlAppendLog::create(&wire_path).await.unwrap();
    log.append(&WireOp::Metadata {
        protocol_version: None,
        created_at: None,
    })
    .await
    .unwrap();
    log.append(&model_record_op()).await.unwrap();
    log.append(&WireOp::PlanModeEnter {
        plan_text: None,
        id: "plan-123".to_string(),
    })
    .await
    .unwrap();
    drop(log);

    manager
        .persistence()
        .save_state(&session_id, &SessionMetadata::default())
        .await
        .unwrap();

    // When the session is resumed
    let session = manager.resume_session(&session_id).await.unwrap();

    // Then the session's plan manager has plan mode active
    let plan = session.plan();
    assert!(plan.is_active(), "plan mode should be restored as active");
    assert_eq!(plan.get_plan().as_deref(), Some("plan-123"));
}

#[tokio::test]
async fn should_restore_session_approvals_when_resuming_session() {
    // Given a session with a wire log that records a session-scoped
    // approval for the Bash pattern
    let (manager, _tmp) = make_manager();
    let session_id = SessionId::new("test-resume-approvals");

    // The session dir is the manager's bucketed layout (the manager
    // derives the workspace key from the workspace cwd — tmp here).
    let session_dir = manager.persistence().session_dir(&session_id);
    tokio::fs::create_dir_all(&session_dir).await.unwrap();

    let wire_path = session_dir.join("wire.jsonl");
    let mut log = JsonlAppendLog::create(&wire_path).await.unwrap();
    log.append(&WireOp::Metadata {
        protocol_version: None,
        created_at: None,
    })
    .await
    .unwrap();
    log.append(&model_record_op()).await.unwrap();
    log.append(&WireOp::PermissionRecordApprovalResult {
        record: json!({
            "turnId": 1,
            "toolCallId": "call-1",
            "toolName": "Bash",
            "action": "execute Bash",
            "sessionApprovalRule": "Bash",
            "result": { "decision": "approved", "scope": "session" },
        }),
    })
    .await
    .unwrap();
    drop(log);

    manager
        .persistence()
        .save_state(&session_id, &SessionMetadata::default())
        .await
        .unwrap();

    // When the session is resumed
    let session = manager.resume_session(&session_id).await.unwrap();

    // Then the recreated agent's permission manager carries the
    // session approval, and a Bash call is approved by the
    // session-approval history instead of asking again
    let agent = session
        .get_agent(&AgentId::new(MAIN_AGENT_ID))
        .await
        .expect("main agent should exist");
    let pm = agent.permission_manager();
    assert_eq!(pm.all_session_approvals(), vec!["Bash".to_string()]);

    let args = json!({"command": "ls"});
    let display = crate::tool::ToolInputDisplay::Command {
        command: "ls".to_string(),
    };
    let cancel = crate::cancel::CancelToken::new();
    let approvals: std::collections::HashSet<String> =
        pm.all_session_approvals().into_iter().collect();
    let ctx = crate::permission::PermissionPolicyContext {
        tool_name: "Bash",
        tool_call_id: "call-resumed",
        args: &args,
        approval_rule: "Bash",
        display: &display,
        accesses: &[],
        mode: PermissionMode::Manual,
        rules: &[],
        session_approvals: &approvals,
        cancel: &cancel,
        net_class: None,
    };
    match pm.evaluate(&ctx) {
        crate::permission::PermissionPolicyResult::Approve { .. } => {}
        other => panic!("expected session approval to approve Bash, got {other:?}"),
    }
}

#[tokio::test]
async fn should_expose_restored_approvals_to_agents_created_after_resume() {
    // Given a session with a wire log that records a session-scoped
    // approval for the Bash pattern
    let (manager, _tmp) = make_manager();
    let session_id = SessionId::new("test-resume-approvals-late");

    let session_dir = manager.persistence().session_dir(&session_id);
    tokio::fs::create_dir_all(&session_dir).await.unwrap();

    let wire_path = session_dir.join("wire.jsonl");
    let mut log = JsonlAppendLog::create(&wire_path).await.unwrap();
    log.append(&WireOp::Metadata {
        protocol_version: None,
        created_at: None,
    })
    .await
    .unwrap();
    log.append(&model_record_op()).await.unwrap();
    log.append(&WireOp::PermissionRecordApprovalResult {
        record: json!({
            "turnId": 1,
            "toolCallId": "call-1",
            "toolName": "Bash",
            "action": "execute Bash",
            "sessionApprovalRule": "Bash",
            "result": { "decision": "approved", "scope": "session" },
        }),
    })
    .await
    .unwrap();
    drop(log);

    manager
        .persistence()
        .save_state(&session_id, &SessionMetadata::default())
        .await
        .unwrap();

    // When the session is resumed and a second agent is created after
    let session = manager.resume_session(&session_id).await.unwrap();
    let late = session
        .create_agent(
            AgentId::new("agent-late"),
            crate::scope::config::AgentScopeConfig::default(),
        )
        .await
        .expect("late agent created");

    // Then the late agent inherits the approval too — the store is
    // session-scoped, not main-agent-scoped (D-07).
    assert_eq!(
        late.permission_manager().all_session_approvals(),
        vec!["Bash".to_string()]
    );
}

#[tokio::test]
async fn should_leave_defaults_when_state_fields_are_absent() {
    // Given a session with an empty wire log (only metadata)
    let (manager, _tmp) = make_manager();
    let session_id = SessionId::new("test-resume-empty");

    // The session dir is the manager's bucketed layout (the manager
    // derives the workspace key from the workspace cwd — tmp here).
    let session_dir = manager.persistence().session_dir(&session_id);
    tokio::fs::create_dir_all(&session_dir).await.unwrap();

    let wire_path = session_dir.join("wire.jsonl");
    let mut log = JsonlAppendLog::create(&wire_path).await.unwrap();
    log.append(&WireOp::Metadata {
        protocol_version: None,
        created_at: None,
    })
    .await
    .unwrap();
    log.append(&model_record_op()).await.unwrap();
    drop(log);

    manager
        .persistence()
        .save_state(&session_id, &SessionMetadata::default())
        .await
        .unwrap();

    // When the session is resumed
    let session = manager.resume_session(&session_id).await.unwrap();

    // Then all state fields are at their defaults
    let agent = session
        .get_agent(&AgentId::new(MAIN_AGENT_ID))
        .await
        .expect("main agent should exist");
    assert_eq!(
        agent.permission_manager().mode(),
        PermissionMode::Manual,
        "permission mode should default to Manual"
    );
    assert!(session.goal().get().is_none(), "no goal should be active");
    assert!(!session.plan().is_active(), "plan mode should be inactive");
    let status = agent.usage_service().lock().unwrap().status();
    assert_eq!(
        status.total.input_tokens, 0,
        "usage should be zero when no usage ops were recorded"
    );
}

#[test]
fn should_parse_permission_mode_when_given_known_mode_strings() {
    // Given known mode strings
    // When parsing
    // Then the correct PermissionMode is returned
    assert_eq!(parse_permission_mode("manual"), PermissionMode::Manual);
    assert_eq!(parse_permission_mode("auto"), PermissionMode::Auto);
    assert_eq!(parse_permission_mode("yolo"), PermissionMode::Yolo);
}

#[test]
fn should_default_to_manual_when_permission_mode_unknown() {
    // Given an unrecognised mode string
    // When parsing
    // Then it falls back to Manual
    assert_eq!(parse_permission_mode("unknown"), PermissionMode::Manual);
    assert_eq!(parse_permission_mode(""), PermissionMode::Manual);
}

#[test]
fn should_parse_goal_status_when_given_known_status_strings() {
    // Given known status strings
    // When parsing
    // Then the correct GoalStatus is returned
    assert_eq!(parse_goal_status("active"), GoalStatus::Active);
    assert_eq!(parse_goal_status("paused"), GoalStatus::Paused);
    assert_eq!(parse_goal_status("completed"), GoalStatus::Complete);
    assert_eq!(parse_goal_status("blocked"), GoalStatus::Blocked);
}

#[test]
fn should_default_to_active_when_goal_status_unknown() {
    // Given an unrecognised status string
    // When parsing
    // Then it falls back to Active
    assert_eq!(parse_goal_status("unknown"), GoalStatus::Active);
}

// ── parse_goal_budget unit tests ───────────────────────────────────

/// Build a minimal `GoalState` for testing, with an optional
/// `budget_limits` JSON value.
fn test_goal_state(budget_limits: Option<serde_json::Value>) -> mirri_proto::GoalState {
    mirri_proto::GoalState {
        goal_id: "g1".to_string(),
        objective: "Test goal".to_string(),
        completion_criterion: None,
        status: "active".to_string(),
        turns_used: None,
        tokens_used: None,
        budget_limits,
    }
}

#[test]
fn should_prefer_session_level_budget_when_goal_level_budget_is_also_present() {
    // Given a session-level budget AND a goal-level JSON budget
    let goal_state = test_goal_state(Some(json!({"turns": 99, "tokens": 999})));
    let session_budget = mirri_proto::BudgetState {
        turns_limit: Some(10),
        tokens_limit: Some(5_000),
        ..Default::default()
    };

    // When parsing
    let budget = parse_goal_budget(&goal_state, &session_budget);

    // Then the session-level limits win
    let budget = budget.expect("budget should be derived from session-level limits");
    assert_eq!(budget.turns, Some(10));
    assert_eq!(budget.tokens, Some(5_000));
    assert_eq!(budget.wall_clock_ms, None);
}

#[test]
fn should_use_goal_level_json_when_session_budget_absent() {
    // Given no session-level budget but a goal-level JSON budget
    let goal_state = test_goal_state(Some(json!({"turns": 50, "tokens": 100000})));
    let session_budget = mirri_proto::BudgetState::default();

    // When parsing
    let budget = parse_goal_budget(&goal_state, &session_budget);

    // Then the goal-level JSON is used
    let budget = budget.expect("budget should be parsed from goal JSON");
    assert_eq!(budget.turns, Some(50));
    assert_eq!(budget.tokens, Some(100_000));
    assert_eq!(budget.wall_clock_ms, None);
}

#[test]
fn should_return_none_when_both_budgets_absent() {
    // Given no session-level budget and no goal-level JSON budget
    let goal_state = test_goal_state(None);
    let session_budget = mirri_proto::BudgetState::default();

    // When parsing
    let budget = parse_goal_budget(&goal_state, &session_budget);

    // Then no budget is returned
    assert!(budget.is_none());
}

#[test]
fn should_parse_turns_when_goal_json_contains_only_turns() {
    // Given a goal-level JSON with only "turns"
    let goal_state = test_goal_state(Some(json!({"turns": 42})));
    let session_budget = mirri_proto::BudgetState::default();

    // When parsing
    let budget = parse_goal_budget(&goal_state, &session_budget);

    // Then only turns is set
    let budget = budget.expect("budget should be parsed from goal JSON");
    assert_eq!(budget.turns, Some(42));
    assert_eq!(budget.tokens, None);
    assert_eq!(budget.wall_clock_ms, None);
}

#[test]
fn should_parse_tokens_when_goal_json_contains_only_tokens() {
    // Given a goal-level JSON with only "tokens"
    let goal_state = test_goal_state(Some(json!({"tokens": 8000})));
    let session_budget = mirri_proto::BudgetState::default();

    // When parsing
    let budget = parse_goal_budget(&goal_state, &session_budget);

    // Then only tokens is set
    let budget = budget.expect("budget should be parsed from goal JSON");
    assert_eq!(budget.turns, None);
    assert_eq!(budget.tokens, Some(8_000));
    assert_eq!(budget.wall_clock_ms, None);
}

#[test]
fn should_parse_wall_clock_ms_when_goal_json_uses_accepted_key_spelling() {
    // Given goal-level JSON with each accepted wall-clock key spelling
    let session_budget = mirri_proto::BudgetState::default();

    // "wallClockMs" (Rust short form)
    let goal_state = test_goal_state(Some(json!({"wallClockMs": 60000})));
    let budget = parse_goal_budget(&goal_state, &session_budget);
    let budget = budget.expect("budget should be parsed from wallClockMs key");
    assert_eq!(budget.wall_clock_ms, Some(60_000));
    assert_eq!(budget.turns, None);
    assert_eq!(budget.tokens, None);

    // "wall_clock_ms" (snake_case)
    let goal_state = test_goal_state(Some(json!({"wall_clock_ms": 120000})));
    let budget = parse_goal_budget(&goal_state, &session_budget);
    let budget = budget.expect("budget should be parsed from wall_clock_ms key");
    assert_eq!(budget.wall_clock_ms, Some(120_000));

    // "wallClockBudgetMs" (TS kernel form)
    let goal_state = test_goal_state(Some(json!({"wallClockBudgetMs": 300000})));
    let budget = parse_goal_budget(&goal_state, &session_budget);
    let budget = budget.expect("budget should be parsed from wallClockBudgetMs key");
    assert_eq!(budget.wall_clock_ms, Some(300_000));
}

#[test]
fn should_parse_budget_when_goal_json_uses_ts_style_keys() {
    // Given a goal-level JSON using the TS kernel's key names
    let goal_state = test_goal_state(Some(
        json!({"turnBudget": 25, "tokenBudget": 50000, "wallClockBudgetMs": 45000}),
    ));
    let session_budget = mirri_proto::BudgetState::default();

    // When parsing
    let budget = parse_goal_budget(&goal_state, &session_budget);

    // Then all TS-style keys are recognised
    let budget = budget.expect("budget should be parsed from TS-style keys");
    assert_eq!(budget.turns, Some(25));
    assert_eq!(budget.tokens, Some(50_000));
    assert_eq!(budget.wall_clock_ms, Some(45_000));
}

#[test]
fn should_return_none_when_budget_json_is_empty() {
    // Given a goal-level JSON that is an empty object
    let goal_state = test_goal_state(Some(json!({})));
    let session_budget = mirri_proto::BudgetState::default();

    // When parsing
    let budget = parse_goal_budget(&goal_state, &session_budget);

    // Then no budget is returned (no recognised keys present)
    assert!(budget.is_none());
}

// ── E2E resume tests ───────────────────────────────────────────────

#[tokio::test]
async fn should_restore_all_fields_together_when_resuming_session() {
    // Given a session with ALL fields set: goal (with budget), permission
    // mode (auto), usage (non-zero tokens), and plan mode
    let (manager, _tmp) = make_manager();
    let session_id = SessionId::new("test-resume-all-fields");

    // The session dir is the manager's bucketed layout (the manager
    // derives the workspace key from the workspace cwd — tmp here).
    let session_dir = manager.persistence().session_dir(&session_id);
    tokio::fs::create_dir_all(&session_dir).await.unwrap();

    let wire_path = session_dir.join("wire.jsonl");
    let mut log = JsonlAppendLog::create(&wire_path).await.unwrap();
    log.append(&WireOp::Metadata {
        protocol_version: None,
        created_at: None,
    })
    .await
    .unwrap();
    log.append(&model_record_op()).await.unwrap();
    log.append(&WireOp::PermissionSetMode {
        mode: "auto".to_string(),
    })
    .await
    .unwrap();
    log.append(&WireOp::GoalCreate {
        goal_id: "g-combined".to_string(),
        objective: "Ship the combined feature".to_string(),
        completion_criterion: Some("All integration tests pass".to_string()),
        status: None,
        actor: None,
        budget_limits: Some(json!({"turns": 75, "tokens": 200000, "wallClockMs": 3600000})),
    })
    .await
    .unwrap();
    log.append(&WireOp::UsageRecord {
        model: "claude-sonnet".to_string(),
        usage: json!({"inputTokens": 1200, "outputTokens": 800}),
        usage_scope: None,
    })
    .await
    .unwrap();
    log.append(&WireOp::UsageRecord {
        model: "claude-sonnet".to_string(),
        usage: json!({"inputTokens": 400, "outputTokens": 600}),
        usage_scope: None,
    })
    .await
    .unwrap();
    log.append(&WireOp::PlanModeEnter {
        plan_text: None,
        id: "plan-combined-789".to_string(),
    })
    .await
    .unwrap();
    drop(log);

    manager
        .persistence()
        .save_state(&session_id, &SessionMetadata::default())
        .await
        .unwrap();

    // When the session is resumed
    let session = manager.resume_session(&session_id).await.unwrap();

    // Then ALL fields are restored on the live agent:

    // Permission mode
    let agent = session
        .get_agent(&AgentId::new(MAIN_AGENT_ID))
        .await
        .expect("main agent should exist");
    assert_eq!(
        agent.permission_manager().mode(),
        PermissionMode::Auto,
        "permission mode should be restored to Auto"
    );

    // Usage totals
    let usage_service = agent.usage_service();
    let status = usage_service.lock().unwrap().status();
    assert_eq!(
        status.total.input_tokens, 1600,
        "input tokens should be summed across both usage records"
    );
    assert_eq!(
        status.total.output_tokens, 1400,
        "output tokens should be summed across both usage records"
    );

    // Goal with budget
    let goal = session.goal().get().expect("goal should be restored");
    assert_eq!(goal.objective, "Ship the combined feature");
    assert_eq!(
        goal.completion_criterion.as_deref(),
        Some("All integration tests pass")
    );
    assert_eq!(goal.status, GoalStatus::Active);
    let budget = goal.budget.expect("budget should be restored");
    assert_eq!(budget.turns, Some(75));
    assert_eq!(budget.tokens, Some(200_000));
    assert_eq!(budget.wall_clock_ms, Some(3_600_000));

    // Plan mode
    let plan = session.plan();
    assert!(plan.is_active(), "plan mode should be restored as active");
    assert_eq!(plan.get_plan().as_deref(), Some("plan-combined-789"));
}

#[tokio::test]
async fn should_restore_goal_without_budget_when_no_budget_present() {
    // Given a session with a goal that has NO budget_limits JSON and no
    // session-level budget
    let (manager, _tmp) = make_manager();
    let session_id = SessionId::new("test-resume-goal-no-budget");

    // The session dir is the manager's bucketed layout (the manager
    // derives the workspace key from the workspace cwd — tmp here).
    let session_dir = manager.persistence().session_dir(&session_id);
    tokio::fs::create_dir_all(&session_dir).await.unwrap();

    let wire_path = session_dir.join("wire.jsonl");
    let mut log = JsonlAppendLog::create(&wire_path).await.unwrap();
    log.append(&WireOp::Metadata {
        protocol_version: None,
        created_at: None,
    })
    .await
    .unwrap();
    log.append(&model_record_op()).await.unwrap();
    log.append(&WireOp::GoalCreate {
        goal_id: "g-no-budget".to_string(),
        objective: "Ship without a budget".to_string(),
        completion_criterion: None,
        status: None,
        actor: None,
        budget_limits: None,
    })
    .await
    .unwrap();
    drop(log);

    manager
        .persistence()
        .save_state(&session_id, &SessionMetadata::default())
        .await
        .unwrap();

    // When the session is resumed
    let session = manager.resume_session(&session_id).await.unwrap();

    // Then the goal is restored but has no budget set
    let goal = session
        .goal()
        .get()
        .expect("goal should be restored even without a budget");
    assert_eq!(goal.objective, "Ship without a budget");
    assert_eq!(goal.status, GoalStatus::Active);
    assert!(
        goal.budget.is_none(),
        "budget should be None when no budget was set"
    );
}

// ── Live wire-log recording (4.3) ──────────────────────────────────

#[tokio::test]
async fn should_open_wire_log_with_metadata_envelope_when_creating_session() {
    // Given a session manager
    let (manager, _tmp) = make_manager();

    // When a session is created
    let session = manager
        .create_session(SessionMetadata::default(), SessionScopeConfig::default())
        .await
        .expect("session creation succeeds");

    // Then wire.jsonl exists and its first record is the metadata envelope
    let wire_log_path = manager.persistence().wire_log_path(session.id());
    assert!(wire_log_path.exists(), "wire.jsonl must exist after create");
    let log = JsonlAppendLog::open(&wire_log_path).await.unwrap();
    let ops = log.read_all().await.unwrap();
    assert_eq!(ops.len(), 1, "only the metadata envelope at creation");
    assert!(
        matches!(&ops[0], WireOp::Metadata { .. }),
        "the first record must be the metadata envelope"
    );

    // And ops recorded live are appended after it
    session.record_op(WireOp::ContextClear {}).await;
    let ops = log.read_all().await.unwrap();
    assert_eq!(ops.len(), 2);
    assert!(matches!(&ops[1], WireOp::ContextClear {}));
}

#[tokio::test]
async fn should_replay_recorded_ops_without_double_logging_when_resuming_session() {
    // Given a created session with a recorded user message op
    let (manager, _tmp) = make_manager();
    let session = manager
        .create_session(SessionMetadata::default(), SessionScopeConfig::default())
        .await
        .expect("session creation succeeds");
    let session_id = session.id().clone();
    // The model record a real embedded create writes (spec 11 R4: the
    // resume fallback chain needs it — wire-log model record, else base
    // config model, else a loud error).
    session
        .record_op(WireOp::ConfigUpdate {
            model_alias: Some("test-resume-model".to_string()),
            profile_name: None,
            thinking_effort: None,
            system_prompt: None,
            disallowed_tools: None,
        })
        .await;
    session
        .record_op(WireOp::ContextAppendMessage {
            message: json!({
                "role": "user",
                "content": [{"type": "text", "text": "hello from the wire log"}],
                "origin": {"kind": "user"},
            }),
        })
        .await;

    // When the session is closed and resumed
    manager
        .close_session(&session_id, CloseReason::UserInitiated)
        .await
        .expect("close succeeds");
    let wire_log_path = manager.persistence().wire_log_path(&session_id);
    let pre_resume_count = JsonlAppendLog::open(&wire_log_path)
        .await
        .unwrap()
        .count()
        .await
        .unwrap();
    let resumed = manager
        .resume_session(&session_id)
        .await
        .expect("resume succeeds");

    // Then the replay rebuilt the agent's memory from the recorded op…
    let agent = resumed
        .get_agent(&AgentId::new(MAIN_AGENT_ID))
        .await
        .expect("main agent exists after resume");
    let memory = agent.context_memory();
    {
        let guard = memory.lock().unwrap();
        let history = guard.history();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].message.text_content(), "hello from the wire log");
    }

    // …and did NOT re-append the replayed ops (replay runs with no
    // writer installed, so the log is unchanged by the resume).
    let post_resume_count = JsonlAppendLog::open(&wire_log_path)
        .await
        .unwrap()
        .count()
        .await
        .unwrap();
    assert_eq!(
        post_resume_count, pre_resume_count,
        "replay must not double-log the ops it consumed"
    );

    // And ops recorded after resume append to the same log
    resumed
        .record_op(WireOp::UsageRecord {
            model: "test-model".to_string(),
            usage: json!({"inputTokens": 100, "outputTokens": 50}),
            usage_scope: None,
        })
        .await;
    let ops = JsonlAppendLog::open(&wire_log_path)
        .await
        .unwrap()
        .read_all()
        .await
        .unwrap();
    assert_eq!(ops.len(), pre_resume_count + 1);
    assert!(matches!(&ops[ops.len() - 1], WireOp::UsageRecord { .. }));
}

// -----------------------------------------------------------------------
// G32 — list_all_sessions merges persisted + active sessions
// -----------------------------------------------------------------------

#[tokio::test]
async fn should_return_empty_list_when_no_sessions_exist() {
    // Given a fresh session manager with no sessions
    let (manager, _tmp) = make_manager();

    // When listing all sessions
    let sessions = manager.list_all_sessions(false).await;

    // Then an empty list is returned
    assert!(
        sessions.is_empty(),
        "list_all_sessions should return empty when no sessions exist"
    );
}

#[tokio::test]
async fn should_list_persisted_sessions_when_no_active_sessions_exist() {
    // Given a manager with several persisted (but closed) sessions
    let (manager, _tmp) = make_manager();

    let session_ids: Vec<SessionId> = (0..3)
        .map(|i| SessionId::new(format!("persisted-session-{i}")))
        .collect();
    for sid in &session_ids {
        let metadata = SessionMetadata {
            title: format!("Persisted {}", sid.as_str()),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            tags: std::collections::HashSet::new(),
            archived: false,
            is_custom_title: false,
            work_dir: None,
            last_prompt: None,
            forked_from: None,
            cache_root_id: None,
            custom: None,
            last_used_at: None,
        };
        manager
            .persistence
            .save_state(sid, &metadata)
            .await
            .unwrap();
    }

    // When listing all sessions (no active sessions in the workspace)
    let sessions = manager.list_all_sessions(false).await;

    // Then all persisted sessions are returned
    assert_eq!(sessions.len(), 3, "should list all persisted sessions");
    for sid in &session_ids {
        assert!(
            sessions.iter().any(|s| s.id == sid.as_str()),
            "persisted session {} should be in the list",
            sid.as_str()
        );
    }
    // And all should be marked as Closed
    assert!(
        sessions.iter().all(|s| s.status == SessionStatus::Closed),
        "persisted-only sessions should have status Closed"
    );
}

#[tokio::test]
async fn should_mark_active_sessions_as_running_when_listed() {
    // Given a manager with one active session and two persisted-only sessions
    let (manager, _tmp) = make_manager();

    // Create an active session (it gets persisted too).
    let active = manager
        .create_session(
            SessionMetadata {
                title: "Active".to_string(),
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
                tags: std::collections::HashSet::new(),
                archived: false,
                is_custom_title: false,
                work_dir: None,
                last_prompt: None,
                forked_from: None,
                cache_root_id: None,
                custom: None,
                last_used_at: None,
            },
            SessionScopeConfig::default(),
        )
        .await
        .unwrap();

    // Create two persisted-only sessions.
    for i in 0..2 {
        let sid = SessionId::new(format!("persisted-only-{i}"));
        let metadata = SessionMetadata {
            title: format!("Persisted {i}"),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            tags: std::collections::HashSet::new(),
            archived: false,
            is_custom_title: false,
            work_dir: None,
            last_prompt: None,
            forked_from: None,
            cache_root_id: None,
            custom: None,
            last_used_at: None,
        };
        manager
            .persistence
            .save_state(&sid, &metadata)
            .await
            .unwrap();
    }

    // When listing all sessions
    let sessions = manager.list_all_sessions(false).await;

    // Then all three sessions are present
    assert_eq!(sessions.len(), 3, "should list active + persisted sessions");

    // And the active session is marked as Running
    let active_info = sessions
        .iter()
        .find(|s| s.id == active.id().as_str())
        .expect("active session should be in the list");
    assert_eq!(
        active_info.status,
        SessionStatus::Running,
        "active session should have status Running"
    );

    // And the persisted-only sessions are marked as Closed
    for s in &sessions {
        if s.id != active.id().as_str() {
            assert_eq!(
                s.status,
                SessionStatus::Closed,
                "persisted-only session {} should have status Closed",
                s.id
            );
        }
    }
}

#[tokio::test]
async fn should_exclude_archived_persisted_sessions_when_listing_by_default() {
    // Given a manager with one plain persisted session and one whose
    // state.json carries archived=true (set through set_archived)
    let (manager, _tmp) = make_manager();

    let plain = SessionId::new("plain-persisted");
    let archived = SessionId::new("archived-persisted");
    for sid in [&plain, &archived] {
        manager
            .persistence()
            .save_state(sid, &SessionMetadata::default())
            .await
            .unwrap();
    }
    manager
        .persistence()
        .set_archived(&archived, true)
        .await
        .unwrap();

    // When listing all sessions with the default filter
    let sessions = manager.list_all_sessions(false).await;

    // Then only the plain persisted session is returned
    assert_eq!(sessions.len(), 1, "{sessions:?}");
    assert_eq!(sessions[0].id, plain.as_str());
    assert!(!sessions[0].archived);

    // And when the listing opts in to archived sessions
    let sessions = manager.list_all_sessions(true).await;

    // Then both are returned and the archived row carries its flag
    assert_eq!(sessions.len(), 2, "{sessions:?}");
    let archived_info = sessions
        .iter()
        .find(|s| s.id == archived.as_str())
        .expect("archived session should be listed when opted in");
    assert!(archived_info.archived);
}

/// A no-op reverse-RPC callback for resume tests (user-tool replay and
/// approval asks route through it).
struct NoopCallback;

#[async_trait::async_trait]
impl crate::reverse_rpc::ReverseRpcCallback for NoopCallback {
    async fn call(
        &self,
        _method: &str,
        _params: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        Ok(serde_json::json!({}))
    }
}

#[tokio::test]
async fn should_restore_active_tools_and_user_tools_when_resuming_session() {
    // Given a session with a wire log that restricts the active tool set
    // and registers one user tool
    let (manager, _tmp) = make_manager();
    let session_id = SessionId::new("test-resume-tools");

    // The session dir is the manager's bucketed layout (the manager
    // derives the workspace key from the workspace cwd — tmp here).
    let session_dir = manager.persistence().session_dir(&session_id);
    tokio::fs::create_dir_all(&session_dir).await.unwrap();

    let wire_path = session_dir.join("wire.jsonl");
    let mut log = JsonlAppendLog::create(&wire_path).await.unwrap();
    log.append(&WireOp::Metadata {
        protocol_version: None,
        created_at: None,
    })
    .await
    .unwrap();
    log.append(&model_record_op()).await.unwrap();
    log.append(&WireOp::ToolsSetActiveTools {
        names: vec!["Read".to_string(), "Grep".to_string()],
    })
    .await
    .unwrap();
    log.append(&WireOp::ToolsRegisterUserTool {
        registration: json!({
            "name": "lookup_issue",
            "description": "Look up an issue",
            "parameters": { "type": "object", "properties": {} },
        }),
    })
    .await
    .unwrap();
    drop(log);

    manager
        .persistence()
        .save_state(&session_id, &SessionMetadata::default())
        .await
        .unwrap();

    // When the session is resumed with a base config carrying a
    // reverse-RPC callback (the host seam the user tools need)
    let base_config = AgentScopeConfig {
        reverse_rpc_slot: Some(Arc::new(std::sync::RwLock::new(Some(
            Arc::new(NoopCallback) as Arc<dyn crate::reverse_rpc::ReverseRpcCallback>,
        )))),
        ..AgentScopeConfig::default()
    };
    let session = manager
        .resume_session_with_config(&session_id, base_config)
        .await
        .unwrap();

    // Then the recreated agent's tool manager carries the restricted
    // active set, and the LLM-visible tool list matches it exactly
    let agent = session
        .get_agent(&AgentId::new(MAIN_AGENT_ID))
        .await
        .expect("main agent should exist");
    let tool_manager = agent.tool_manager();
    let tool_manager = tool_manager.read();
    let visible: Vec<String> = tool_manager
        .loop_tools()
        .into_iter()
        .map(|definition| definition.name)
        .collect();
    let mut expected = vec!["Grep".to_string(), "Read".to_string()];
    expected.sort();
    assert_eq!(visible, expected);

    // And the registered user tool is replayed back into the registry
    assert!(
        tool_manager.find_tool("lookup_issue").is_some(),
        "the user-tool registration must be replayed on resume"
    );
}

#[tokio::test]
async fn should_keep_unrestricted_tools_when_resuming_session_without_restriction() {
    // Given a session with a wire log that never restricted the active
    // tool set
    let (manager, _tmp) = make_manager();
    let session_id = SessionId::new("test-resume-tools-default");

    // The session dir is the manager's bucketed layout (the manager
    // derives the workspace key from the workspace cwd — tmp here).
    let session_dir = manager.persistence().session_dir(&session_id);
    tokio::fs::create_dir_all(&session_dir).await.unwrap();

    let wire_path = session_dir.join("wire.jsonl");
    let mut log = JsonlAppendLog::create(&wire_path).await.unwrap();
    log.append(&WireOp::Metadata {
        protocol_version: None,
        created_at: None,
    })
    .await
    .unwrap();
    log.append(&model_record_op()).await.unwrap();
    drop(log);

    manager
        .persistence()
        .save_state(&session_id, &SessionMetadata::default())
        .await
        .unwrap();

    // When the session is resumed
    let session = manager.resume_session(&session_id).await.unwrap();

    // Then the recreated agent's tool manager stays unrestricted (the
    // freshly created default, not an empty active set)
    let agent = session
        .get_agent(&AgentId::new(MAIN_AGENT_ID))
        .await
        .expect("main agent should exist");
    let tool_manager = agent.tool_manager();
    let tool_manager = tool_manager.read();
    assert!(
        !tool_manager.loop_tools().is_empty(),
        "the unrestricted default must keep every builtin tool visible"
    );
}

/// Given a wire log that registers a user tool and then disables it by
/// name, When the session is resumed, Then the replayed registration
/// rebuilds the tool but the replayed disable keeps it out of the
/// LLM-visible table — the pre-restart visibility survives the restart.
#[tokio::test]
async fn should_restore_disabled_user_tool_when_resuming_session() {
    // Given a wire log with one registered, then disabled, user tool
    let (manager, _tmp) = make_manager();
    let session_id = SessionId::new("test-resume-user-tool-disabled");

    // The session dir is the manager's bucketed layout (the manager
    // derives the workspace key from the workspace cwd — tmp here).
    let session_dir = manager.persistence().session_dir(&session_id);
    tokio::fs::create_dir_all(&session_dir).await.unwrap();

    let wire_path = session_dir.join("wire.jsonl");
    let mut log = JsonlAppendLog::create(&wire_path).await.unwrap();
    log.append(&WireOp::Metadata {
        protocol_version: None,
        created_at: None,
    })
    .await
    .unwrap();
    log.append(&model_record_op()).await.unwrap();
    log.append(&WireOp::ToolsRegisterUserTool {
        registration: json!({
            "name": "lookup_issue",
            "description": "Look up an issue",
            "parameters": { "type": "object", "properties": {} },
        }),
    })
    .await
    .unwrap();
    log.append(&WireOp::ToolsSetUserToolEnabled {
        name: "lookup_issue".to_string(),
        enabled: false,
    })
    .await
    .unwrap();
    drop(log);

    manager
        .persistence()
        .save_state(&session_id, &SessionMetadata::default())
        .await
        .unwrap();

    // When the session is resumed with a base config carrying a
    // reverse-RPC callback (the host seam the user tools need)
    let base_config = AgentScopeConfig {
        reverse_rpc_slot: Some(Arc::new(std::sync::RwLock::new(Some(
            Arc::new(NoopCallback) as Arc<dyn crate::reverse_rpc::ReverseRpcCallback>,
        )))),
        ..AgentScopeConfig::default()
    };
    let session = manager
        .resume_session_with_config(&session_id, base_config)
        .await
        .unwrap();

    // Then the recreated agent keeps the tool registered, but the
    // replayed disable keeps it out of the visible table
    let agent = session
        .get_agent(&AgentId::new(MAIN_AGENT_ID))
        .await
        .expect("main agent should exist");
    let tool_manager = agent.tool_manager();
    let tool_manager = tool_manager.read();
    assert!(
        tool_manager.find_tool("lookup_issue").is_some(),
        "the disabled user tool must stay registered after the resume"
    );
    let visible: Vec<String> = tool_manager
        .loop_tools()
        .into_iter()
        .map(|definition| definition.name)
        .collect();
    assert!(
        !visible.contains(&"lookup_issue".to_string()),
        "the replayed disable must keep the tool out of the visible table: {visible:?}"
    );
}

/// Given a wire log that registers a user tool without any active-set
/// restriction, When the session is resumed, Then the replayed
/// registration is LLM-visible — TS `addActiveTool` parity across the
/// restart (F22's replay arm).
#[tokio::test]
async fn should_restore_registered_user_tool_visibility_when_resuming_session_without_restriction()
{
    // Given a wire log with one registered user tool and no
    // active-set restriction
    let (manager, _tmp) = make_manager();
    let session_id = SessionId::new("test-resume-user-tool-visible");

    // The session dir is the manager's bucketed layout (the manager
    // derives the workspace key from the workspace cwd — tmp here).
    let session_dir = manager.persistence().session_dir(&session_id);
    tokio::fs::create_dir_all(&session_dir).await.unwrap();

    let wire_path = session_dir.join("wire.jsonl");
    let mut log = JsonlAppendLog::create(&wire_path).await.unwrap();
    log.append(&WireOp::Metadata {
        protocol_version: None,
        created_at: None,
    })
    .await
    .unwrap();
    log.append(&model_record_op()).await.unwrap();
    log.append(&WireOp::ToolsRegisterUserTool {
        registration: json!({
            "name": "lookup_issue",
            "description": "Look up an issue",
            "parameters": { "type": "object", "properties": {} },
        }),
    })
    .await
    .unwrap();
    drop(log);

    manager
        .persistence()
        .save_state(&session_id, &SessionMetadata::default())
        .await
        .unwrap();

    // When the session is resumed with a reverse-RPC-capable base config
    let base_config = AgentScopeConfig {
        reverse_rpc_slot: Some(Arc::new(std::sync::RwLock::new(Some(
            Arc::new(NoopCallback) as Arc<dyn crate::reverse_rpc::ReverseRpcCallback>,
        )))),
        ..AgentScopeConfig::default()
    };
    let session = manager
        .resume_session_with_config(&session_id, base_config)
        .await
        .unwrap();

    // Then the replayed registration is part of the LLM-visible table
    let agent = session
        .get_agent(&AgentId::new(MAIN_AGENT_ID))
        .await
        .expect("main agent should exist");
    let tool_manager = agent.tool_manager();
    let tool_manager = tool_manager.read();
    let visible: Vec<String> = tool_manager
        .loop_tools()
        .into_iter()
        .map(|definition| definition.name)
        .collect();
    assert!(
        visible.contains(&"lookup_issue".to_string()),
        "a replayed user-tool registration must be LLM-visible in the unrestricted default: {visible:?}"
    );
}

// -----------------------------------------------------------------------
// Phase 2 — per-workspace bucket layout + legacy flat migration
// -----------------------------------------------------------------------

#[tokio::test]
async fn should_persist_created_sessions_under_the_workspace_bucket_with_work_dir() {
    // Given a manager whose workspace cwd is the temp dir
    let (manager, tmp) = make_manager();

    // When a session is created
    let session = manager
        .create_session(SessionMetadata::default(), SessionScopeConfig::default())
        .await
        .unwrap();

    // Then the session directory nests under the workspace bucket
    // derived from the workspace cwd (the TS per-workspace layout)
    let bucket = encode_work_dir_key(tmp.path());
    let session_dir = tmp
        .path()
        .join(".mirri")
        .join("sessions")
        .join(&bucket)
        .join(session.id().as_str());
    assert!(
        session_dir.join("state.json").exists(),
        "the session must persist under {} (got bucket {bucket})",
        session_dir.display()
    );
    assert!(session_dir.join("wire.jsonl").exists());

    // And the state records the session's workDir (the workspace cwd)
    let loaded = manager
        .persistence()
        .load_state(session.id())
        .await
        .unwrap();
    assert_eq!(
        loaded.work_dir.as_deref(),
        Some(tmp.path().to_string_lossy().as_ref()),
        "the created session must record its workDir"
    );
}

#[tokio::test]
async fn should_migrate_a_legacy_flat_session_into_the_manager_bucket_when_migrating() {
    // Given a legacy flat session on disk (the pre-Phase-2 layout),
    // with a wire log so it is resumable
    let (manager, tmp) = make_manager();
    let session_id = SessionId::new("legacy-flat-session");
    let flat_dir = tmp
        .path()
        .join(".mirri")
        .join("sessions")
        .join(session_id.as_str());
    tokio::fs::create_dir_all(&flat_dir).await.unwrap();
    let mut log = JsonlAppendLog::create(&flat_dir.join("wire.jsonl"))
        .await
        .unwrap();
    log.append(&WireOp::Metadata {
        protocol_version: None,
        created_at: None,
    })
    .await
    .unwrap();
    // The model record a real embedded create writes (spec 11 R4): the
    // legacy session must stay resolvable after the migration moves it.
    log.append(&model_record_op()).await.unwrap();
    drop(log);
    tokio::fs::write(
            flat_dir.join("state.json"),
            r#"{"title":"Legacy","createdAt":"2024-01-01T00:00:00Z","updatedAt":"2024-01-01T00:00:00Z","tags":[]}"#,
        )
        .await
        .unwrap();

    // When the manager runs the one-time legacy migration
    let migrated = manager.migrate_legacy_flat_sessions().await.unwrap();

    // Then the session moved into the manager's own bucket (the flat
    // session had no workDir, so the current workspace cwd is the
    // backfilled attribution)
    assert_eq!(migrated, 1);
    assert!(!flat_dir.exists(), "the flat directory must be gone");
    let bucket = encode_work_dir_key(tmp.path());
    let moved = tmp
        .path()
        .join(".mirri")
        .join("sessions")
        .join(&bucket)
        .join(session_id.as_str());
    assert!(moved.join("state.json").exists());
    assert!(moved.join("wire.jsonl").exists());

    // And the migrated session resumes through the manager's bucketed
    // paths (the wire log replays from its new location)
    let session = manager.resume_session(&session_id).await.unwrap();
    assert_eq!(session.id().as_str(), "legacy-flat-session");

    // And a second run finds nothing left to migrate (idempotent)
    assert_eq!(manager.migrate_legacy_flat_sessions().await.unwrap(), 0);
}

// -----------------------------------------------------------------------
// SDR-004 — lazy physical creation
// -----------------------------------------------------------------------

/// Given a lazily created session, when nothing further happens, then no
/// session directory, no `state.json`, and no `wire.jsonl` exist on disk
/// — the session lives in memory only (a never-conversed session leaves
/// nothing behind when the process exits).
#[tokio::test]
async fn should_create_nothing_on_disk_when_a_session_is_created_lazily() {
    let (manager, tmp) = make_manager();
    let session = manager
        .create_session_lazy(SessionMetadata::default(), SessionScopeConfig::default())
        .await
        .unwrap();

    let bucket = encode_work_dir_key(tmp.path());
    let session_dir = tmp
        .path()
        .join(".mirri")
        .join("sessions")
        .join(&bucket)
        .join(session.id().as_str());
    assert!(!session_dir.exists(), "no session directory may exist");
    // But the session itself is live and promptable.
    assert!(manager.get_session(session.id()).await.is_some());
    assert!(!session.wire_log_installed().await);
}

/// Given a lazily created session whose ops were recorded after creation,
/// when it is materialized, then `state.json` + `wire.jsonl` appear and
/// the buffered ops flush in their original record order — the
/// materialized wire log is byte-equivalent to an eagerly created one's.
#[tokio::test]
async fn should_write_the_buffered_ops_in_order_when_materializing_a_lazy_session() {
    let (manager, tmp) = make_manager();
    let session = manager
        .create_session_lazy(
            SessionMetadata {
                title: "lazy".to_string(),
                ..SessionMetadata::default()
            },
            SessionScopeConfig::default(),
        )
        .await
        .unwrap();
    session
        .record_op(WireOp::ConfigUpdate {
            model_alias: Some("claude".to_string()),
            profile_name: None,
            thinking_effort: None,
            system_prompt: None,
            disallowed_tools: None,
        })
        .await;
    assert!(!session.wire_log_installed().await, "still unmaterialized");

    let materialized = manager.materialize_session(session.id()).await.unwrap();

    assert!(materialized, "the first touch materializes");
    let bucket = encode_work_dir_key(tmp.path());
    let session_dir = tmp
        .path()
        .join(".mirri")
        .join("sessions")
        .join(&bucket)
        .join(session.id().as_str());
    assert!(session_dir.join("state.json").is_file());
    assert!(session_dir.join("wire.jsonl").is_file());
    // The Metadata envelope (recorded at create) precedes the
    // ConfigUpdate recorded after it — the buffer flushed in order.
    let content = std::fs::read_to_string(session_dir.join("wire.jsonl")).unwrap();
    let ops: Vec<serde_json::Value> = content
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert!(ops.len() >= 2);
    assert_eq!(ops[0]["type"], json!("metadata"));
    assert_eq!(ops[1]["type"], json!("config.update"));

    // And a second materialization is a no-op (idempotent).
    assert!(!manager.materialize_session(session.id()).await.unwrap());
}

/// Given an eagerly created (or resumed) session, when materialize is
/// called, then it answers `false` and writes nothing new — the eager
/// path is the identity for materialization.
#[tokio::test]
async fn should_answer_false_when_materializing_an_eagerly_created_session() {
    let (manager, _tmp) = make_manager();
    let session = manager
        .create_session(SessionMetadata::default(), SessionScopeConfig::default())
        .await
        .unwrap();
    assert!(session.wire_log_installed().await);
    assert!(!manager.materialize_session(session.id()).await.unwrap());
    // And an unknown session id answers false (nothing to materialize).
    assert!(
        !manager
            .materialize_session(&SessionId::new("no-such-session"))
            .await
            .unwrap()
    );
}

/// Given a source session holding a cron task, when the session is forked,
/// then the fork's cron manager starts empty (DEC-4 / SDR: fork carries no
/// running assets — a clone would double-fire the schedule).
#[tokio::test]
async fn should_not_carry_cron_tasks_when_forking_a_session() {
    let (manager, _tmp) = make_manager();
    let source = manager
        .create_session(SessionMetadata::default(), SessionScopeConfig::default())
        .await
        .unwrap();
    // The main agent (agents are created lazily; fork needs one to copy).
    source
        .create_agent(
            crate::AgentId::new(MAIN_AGENT_ID),
            crate::AgentScopeConfig::default(),
        )
        .await
        .unwrap();
    source
        .cron()
        .add(
            "*/5 * * * *".into(),
            "tick".into(),
            source.id().as_str().to_string(),
            true,
            None,
        )
        .await
        .unwrap();
    assert_eq!(source.cron().list().len(), 1);

    let fork = manager
        .fork_session(
            &source,
            &AgentId::new(MAIN_AGENT_ID),
            SessionMetadata::default(),
        )
        .await
        .unwrap();

    assert!(
        fork.cron().list().is_empty(),
        "a fork carries no cron tasks (DEC-4: no double-firing)"
    );
    // And the source keeps its schedule untouched
    assert_eq!(source.cron().list().len(), 1);
}

/// Given a source session running under a user-level deny rule, when the
/// session is forked, then the fork's agents evaluate under the same
/// rules (D-34: a deny rule must not silently vanish across a fork).
#[tokio::test]
async fn should_carry_user_permission_rules_when_forking_a_session() {
    let (manager, _tmp) = make_manager();
    let deny = crate::permission::PermissionRule {
        decision: crate::permission::RuleDecision::Deny,
        scope: crate::permission::RuleScope::User,
        pattern: "Bash".to_string(),
        reason: None,
    };
    let source = manager
        .create_session(
            SessionMetadata::default(),
            SessionScopeConfig {
                user_permission_rules: vec![deny],
                ..SessionScopeConfig::default()
            },
        )
        .await
        .unwrap();
    source
        .create_agent(
            crate::AgentId::new(MAIN_AGENT_ID),
            crate::AgentScopeConfig::default(),
        )
        .await
        .unwrap();

    let fork = manager
        .fork_session(
            &source,
            &AgentId::new(MAIN_AGENT_ID),
            SessionMetadata::default(),
        )
        .await
        .unwrap();

    let fork_agent_id = fork
        .list_agents()
        .await
        .pop()
        .expect("the forked agent exists");
    let fork_agent = fork
        .get_agent(&fork_agent_id)
        .await
        .expect("the forked agent resolves");
    let rules = fork_agent.permission_manager().effective_rules();
    assert!(
        rules
            .iter()
            .any(|rule| rule.pattern == "Bash"
                && rule.decision == crate::permission::RuleDecision::Deny),
        "the fork must inherit the source session's user deny rule, got {rules:?}"
    );

    // And an agent created in the fork afterwards is stamped too (the
    // fork's session, not just the copied agent, carries the rules).
    let late = fork
        .create_agent(
            crate::AgentId::new("agent-fork-late"),
            crate::AgentScopeConfig::default(),
        )
        .await
        .unwrap();
    assert!(
        late.permission_manager()
            .effective_rules()
            .iter()
            .any(|rule| rule.pattern == "Bash"),
        "agents created in the fork must run under the inherited rules"
    );
}

/// Given a workspace carrying a live hook and a session created in it,
/// when the session is forked, then the fork registers the workspace hook
/// exactly ONCE — the source session's persisted `hooks` already contains
/// the workspace's pre-chained copy, and the fork's own create chains the
/// live hooks in again, so an inherited list would double-register every
/// workspace hook (design review I-1: `register_all` is a plain extend,
/// no dedup).
#[tokio::test]
async fn should_register_workspace_hooks_exactly_once_when_forking_a_session() {
    use crate::hook_engine::HookDef;
    let (manager, _tmp) = make_manager();
    manager.workspace().hook_engine().register(HookDef::new(
        crate::hook_engine::HookEventType::PreToolUse,
        "echo fork-hook-fired",
    ));

    let source = manager
        .create_session(SessionMetadata::default(), SessionScopeConfig::default())
        .await
        .unwrap();
    source
        .create_agent(
            crate::AgentId::new(MAIN_AGENT_ID),
            crate::AgentScopeConfig::default(),
        )
        .await
        .unwrap();

    let fork = manager
        .fork_session(
            &source,
            &AgentId::new(MAIN_AGENT_ID),
            SessionMetadata::default(),
        )
        .await
        .unwrap();

    // Then the fork's scope config carries the hook exactly once
    let hooks = &fork.config().hooks;
    let count = hooks
        .iter()
        .filter(|hook| hook.command.contains("fork-hook-fired"))
        .count();
    assert_eq!(
        count, 1,
        "the fork must register the workspace hook exactly once, got {hooks:?}"
    );
}

// -------------------------------------------------------------------------
// Fork pipeline (F2): turnId-anchored truncation through SessionManager.
//
// The fixtures below hand-write a source session's wire log (two completed
// turns plus a trailing open bracket), then drive fork_session through
// the new ForkOptions surface and verify the fork's persisted log and the
// resume-reconstructed history.
// -------------------------------------------------------------------------

/// Write a two-turn-plus-open-bracket source wire log for a fresh session
/// id: metadata header, model record, turn 1 (user marker "t1", reply
/// marker "r1"), turn 2 (marker "t2"/"r2"), then an ACTIVE turn 3
/// (prompt + user append, no turn.ended — the fork's default cut must
/// drop it).
async fn write_fork_pipeline_source(manager: &SessionManager, session_id: &SessionId) {
    let session_dir = manager.persistence().session_dir(session_id);
    tokio::fs::create_dir_all(&session_dir).await.unwrap();
    let wire_path = session_dir.join("wire.jsonl");
    let mut log = JsonlAppendLog::create(&wire_path).await.unwrap();
    log.append(&WireOp::Metadata {
        protocol_version: None,
        created_at: None,
    })
    .await
    .unwrap();
    log.append(&model_record_op()).await.unwrap();
    let turns: [(u64, &str, &str); 3] = [(1, "t1", "r1"), (2, "t2", "r2"), (3, "t3", "r3")];
    for (turn_id, user, reply) in turns {
        log.append(&WireOp::TurnPrompt {
            input: json!(user),
            origin: json!({ "kind": "user" }),
        })
        .await
        .unwrap();
        log.append(&WireOp::ContextAppendMessage {
            message: json!({
                "role": "user",
                "content": user,
                "origin": { "kind": "user" }
            }),
        })
        .await
        .unwrap();
        log.append(&WireOp::ContextAppendMessage {
            message: json!({
                "role": "assistant",
                "content": reply,
                "origin": { "kind": "user" }
            }),
        })
        .await
        .unwrap();
        // Only turns 1 and 2 close — turn 3 stays open (an active turn).
        if turn_id < 3 {
            log.append(&WireOp::TurnEnded {
                turn_id,
                reason: "completed".to_string(),
                error: None,
                duration_ms: None,
            })
            .await
            .unwrap();
        }
    }
    drop(log);
    manager
        .persistence()
        .save_state(session_id, &SessionMetadata::default())
        .await
        .unwrap();
}

/// Resume-reconstructed main-agent history markers for a session id.
///
/// A live session is closed first — the markers must come from the
/// persisted wire log (what a later process resume reconstructs), not the
/// in-memory deep copy the fork installed.
async fn resumed_history_markers(manager: &SessionManager, session_id: &SessionId) -> Vec<String> {
    if manager.workspace.get_session(session_id).await.is_some() {
        manager
            .close_session(session_id, CloseReason::UserInitiated)
            .await
            .unwrap();
    }
    let session = manager.resume_session(session_id).await.unwrap();
    let agent = session
        .get_agent(&AgentId::new(MAIN_AGENT_ID))
        .await
        .expect("main agent rebuilt on resume");
    let memory = agent.context_memory();
    let guard = memory.lock().expect("context memory mutex poisoned");
    let serialized: Vec<String> = guard
        .history()
        .iter()
        .map(|m| serde_json::to_string(m).expect("message serializes"))
        .collect();
    drop(guard);
    serialized
}

/// Given a source wire log with two completed turns and an active third
/// turn, When the session is forked anchored at turnId=1, Then the fork's
/// persisted log replays to exactly turn 1's history.
#[tokio::test]
async fn should_keep_only_the_first_turn_when_forking_at_turn_one() {
    let (manager, _tmp) = make_manager();
    let source_id = SessionId::new("session-fork-at-1");
    write_fork_pipeline_source(&manager, &source_id).await;

    // The fork reads the source's persisted log; a live source agent is
    // still needed for the in-memory deep copy, so resume the source first.
    manager.resume_session(&source_id).await.unwrap();
    let fork = manager
        .fork_session_with_options(
            &source_id,
            ForkOptions { turn_id: Some(1) },
            &AgentId::new(MAIN_AGENT_ID),
            SessionMetadata::default(),
        )
        .await
        .unwrap();

    let markers = resumed_history_markers(&manager, fork.id()).await;
    let joined = markers.join("\n");
    assert!(
        joined.contains("t1") && joined.contains("r1"),
        "turn 1 must survive the turnId=1 fork: {markers:?}"
    );
    assert!(
        !joined.contains("t2") && !joined.contains("r2"),
        "turn 2 must not survive the turnId=1 fork: {markers:?}"
    );
    assert!(
        !joined.contains("t3") && !joined.contains("r3"),
        "the active turn 3 must not survive the turnId=1 fork: {markers:?}"
    );
}

/// Given a source wire log whose turns end at 2, When the session is
/// forked anchored at a nonexistent turnId, Then the fork fails with
/// ForkTargetNotFound and the message carries the available range.
#[tokio::test]
async fn should_fail_with_fork_target_not_found_when_the_anchor_turn_is_missing() {
    let (manager, _tmp) = make_manager();
    let source_id = SessionId::new("session-fork-missing-turn");
    write_fork_pipeline_source(&manager, &source_id).await;

    manager.resume_session(&source_id).await.unwrap();
    let error = manager
        .fork_session_with_options(
            &source_id,
            ForkOptions { turn_id: Some(999) },
            &AgentId::new(MAIN_AGENT_ID),
            SessionMetadata::default(),
        )
        .await
        .unwrap_err();
    match &error {
        SessionError::ForkTargetNotFound(message) => {
            assert!(
                message.contains("999") && message.contains("1") && message.contains("2"),
                "the error must carry the target and available range: {message}"
            );
        }
        other => panic!("expected ForkTargetNotFound, got {other:?}"),
    }
}

/// Given a source wire log with two completed turns and an active third
/// turn, When the session is forked with NO turnId, Then the fork carries
/// every COMPLETED turn and drops the active turn's content.
#[tokio::test]
async fn should_keep_all_completed_turns_when_forking_without_a_turn_id() {
    let (manager, _tmp) = make_manager();
    let source_id = SessionId::new("session-fork-default");
    write_fork_pipeline_source(&manager, &source_id).await;

    manager.resume_session(&source_id).await.unwrap();
    let fork = manager
        .fork_session_with_options(
            &source_id,
            ForkOptions::default(),
            &AgentId::new(MAIN_AGENT_ID),
            SessionMetadata::default(),
        )
        .await
        .unwrap();

    let markers = resumed_history_markers(&manager, fork.id()).await;
    let joined = markers.join("\n");
    assert!(
        joined.contains("t1") && joined.contains("t2"),
        "both completed turns must survive the default fork: {markers:?}"
    );
    assert!(
        !joined.contains("t3") && !joined.contains("r3"),
        "the active turn 3 must be cut by the default fork: {markers:?}"
    );
}

/// Given a forked session whose wire log carries the batch-written forked
/// history, When the fork's live session records a new op, Then the log
/// stays fully parseable and the new op lands AFTER the forked history —
/// the installed writer must never overwrite the batch from its stale
/// create-time offset (regression: a forked session's first live op used
/// to clobber the forked history's head, leaving a mid-file fragment that
/// broke every replaying read face).
#[tokio::test]
async fn should_append_live_ops_after_the_forked_history_when_recording_after_a_fork() {
    let (manager, _tmp) = make_manager();
    let source_id = SessionId::new("session-fork-then-record");
    write_fork_pipeline_source(&manager, &source_id).await;

    manager.resume_session(&source_id).await.unwrap();
    let fork = manager
        .fork_session_with_options(
            &source_id,
            ForkOptions::default(),
            &AgentId::new(MAIN_AGENT_ID),
            SessionMetadata::default(),
        )
        .await
        .unwrap();

    // The forked session goes on living: a later op is recorded through
    // the session's installed writer.
    fork.record_op(WireOp::ContextAppendMessage {
        message: json!({
            "role": "user",
            "content": "post-fork-live-op",
            "origin": { "kind": "user" }
        }),
    })
    .await;

    // Then the fork's wire log must remain fully parseable — a mid-file
    // unparseable line is corruption, not a torn final write.
    let wire_path = manager.persistence().wire_log_path(fork.id());
    let log = JsonlAppendLog::open(&wire_path).await.unwrap();
    let ops = log.read_all().await.unwrap_or_else(|e| {
        panic!("the forked session's wire log must stay parseable after live ops: {e}")
    });

    // And the forked history must survive ahead of the live op.
    let serialized: Vec<String> = ops
        .iter()
        .map(|op| serde_json::to_string(op).expect("op serializes"))
        .collect();
    let joined = serialized.join("\n");
    assert!(
        joined.contains("t1") && joined.contains("t2"),
        "the forked history must survive live recording: {serialized:?}"
    );
    assert!(
        serialized
            .last()
            .is_some_and(|last| last.contains("post-fork-live-op")),
        "the live op must land after the forked history: {serialized:?}"
    );
}

/// Given a persisted session whose wire log carries a mid-file unparseable
/// fragment (the corruption a stale-offset overwrite leaves behind), When
/// the session is resumed, Then the resume fails AND the session is not
/// left live in the workspace: no hollow read face, no artifacts, and a
/// later resume attempt retries the replay instead of failing with
/// AlreadyExists against a half-installed zombie (regression: a failed
/// resume used to leave the session in the workspace map with no artifacts,
/// so `session.get` answered with an empty session while `session.snapshot`
/// reported not-found, and every later restore failed with AlreadyExists
/// until the process restarted).
#[tokio::test]
async fn should_not_leave_a_live_session_when_the_resume_replay_fails() {
    let (manager, _tmp) = make_manager();
    let session_id = SessionId::new("session-corrupt-mid-file");
    write_fork_pipeline_source(&manager, &session_id).await;

    // Corrupt the log mid-file: splice an unparseable fragment between two
    // valid records (a torn line is dropped only at EOF; mid-file it is
    // corruption the replay must fail on).
    let wire_path = manager.persistence().wire_log_path(&session_id);
    let content = tokio::fs::read_to_string(&wire_path).await.unwrap();
    let mut lines: Vec<&str> = content.lines().collect();
    assert!(lines.len() >= 3, "fixture must have at least three records");
    lines.insert(
        2,
        "\"},\"time\":123,\"type\":\"context.append_loop_event\"}",
    );
    tokio::fs::write(&wire_path, format!("{}\n", lines.join("\n")))
        .await
        .unwrap();

    // When the session is resumed, Then the resume fails on the corruption.
    let first = manager
        .resume_session(&session_id)
        .await
        .expect_err("a mid-file corrupt record must fail the resume");
    assert!(
        !matches!(first, SessionError::AlreadyExists(_)),
        "the failure must come from the replay, not a zombie registration: {first:?}"
    );

    // And the failed session is NOT left live: no workspace entry (a read
    // face must never see a hollow session), and no artifacts (a snapshot
    // must never report not-found for a session the manager resolved).
    assert!(
        manager.workspace.get_session(&session_id).await.is_none(),
        "a failed resume must not leave the session live in the workspace"
    );
    assert!(
        manager.session_artifacts(&session_id).is_none(),
        "a failed resume must not leave partial artifacts installed"
    );

    // And a second resume attempt RETRIES the replay (the same corruption
    // error), instead of failing with AlreadyExists against the zombie.
    let second = manager
        .resume_session(&session_id)
        .await
        .expect_err("the retry must run the replay again and fail on it");
    assert!(
        !matches!(second, SessionError::AlreadyExists(_)),
        "the retry must not hit a zombie AlreadyExists: {second:?}"
    );
}

/// Given a session whose main journal folds a still-parked steer
/// (`turn.steer` with no matching `turn.steer_flushed`), When the session is
/// resumed and its pending steers are read, Then the read returns the exact
/// parked pairs derived by the resume's SINGLE main-log fold (F2, ADR-017):
/// the second full-log replay is eliminated, and the embedded re-park sees
/// the same blob-rehydrated pairs the eager double-replay produced.
#[tokio::test]
async fn should_return_the_cached_pending_steers_from_the_single_fold_when_a_session_resumes() {
    let (manager, tmp) = make_manager();
    let session_id = SessionId::new("session-pending-steer-f2");
    // Write a main journal carrying one still-parked steer (a user steer never
    // flushed by a `turn.steer_flushed` marker).
    let session_dir = manager.persistence().session_dir(&session_id);
    tokio::fs::create_dir_all(&session_dir).await.unwrap();
    let wire_path = session_dir.join("wire.jsonl");
    let mut log = JsonlAppendLog::create(&wire_path).await.unwrap();
    log.append(&WireOp::Metadata {
        protocol_version: None,
        created_at: None,
    })
    .await
    .unwrap();
    log.append(&model_record_op()).await.unwrap();
    log.append(&WireOp::TurnSteer {
        input: json!([{ "type": "text", "text": "steer me" }]),
        origin: json!({ "kind": "user" }),
    })
    .await
    .unwrap();
    drop(log);
    manager
        .persistence()
        .save_state(&session_id, &SessionMetadata::default())
        .await
        .unwrap();

    // When the session is resumed, Then the resume's single fold caches the
    // parked steer on the manager.
    let resumed = manager
        .resume_session(&session_id)
        .await
        .expect("session resumes");
    assert_eq!(
        resumed.id().as_str(),
        session_id.as_str(),
        "the resumed session is live"
    );

    // And the read face returns THE SAME pairs from the cache — no second
    // replay — so the embedded re-park gets the identical blob-rehydrated
    // steer the eager double-replay produced.
    let read_pairs = manager.replayed_pending_steers(&session_id).await;
    assert_eq!(read_pairs.len(), 1, "exactly the one parked steer re-parks");
    assert_eq!(
        read_pairs[0].0,
        vec![crate::message::ContentPart::Text {
            text: "steer me".to_string()
        }]
    );
    assert_eq!(read_pairs[0].1, crate::origin::PromptOrigin::User);
    let _ = tmp;
}

/// Given a session whose main journal is resumed, When the pending-steer
/// cache is consumed once, Then a second read falls back to a fresh replay
/// (the cache is single-use — a non-resume caller or a re-read after the
/// embedded layer drained it still derives the correct pairs).
#[tokio::test]
async fn should_replay_pending_steers_afresh_after_the_cache_is_consumed() {
    let (manager, _tmp) = make_manager();
    let session_id = SessionId::new("session-pending-steer-f2-again");
    let session_dir = manager.persistence().session_dir(&session_id);
    tokio::fs::create_dir_all(&session_dir).await.unwrap();
    let wire_path = session_dir.join("wire.jsonl");
    let mut log = JsonlAppendLog::create(&wire_path).await.unwrap();
    log.append(&WireOp::Metadata {
        protocol_version: None,
        created_at: None,
    })
    .await
    .unwrap();
    log.append(&model_record_op()).await.unwrap();
    log.append(&WireOp::TurnSteer {
        input: json!([{ "type": "text", "text": "steer A" }]),
        origin: json!({ "kind": "user" }),
    })
    .await
    .unwrap();
    drop(log);
    manager
        .persistence()
        .save_state(&session_id, &SessionMetadata::default())
        .await
        .unwrap();

    // Resume: the single fold caches the parked steer.
    manager
        .resume_session(&session_id)
        .await
        .expect("session resumes");
    // Consume the cache (the embedded resume assembly does this once).
    assert!(
        manager.take_pending_steers(&session_id).is_some(),
        "the cache holds the pairs right after resume"
    );
    // A later read falls back to a fresh replay and still derives the steer.
    let read_pairs = manager.replayed_pending_steers(&session_id).await;
    assert_eq!(
        read_pairs.len(),
        1,
        "the replay fallback still derives the parked steer"
    );
    assert_eq!(
        read_pairs[0].0[0],
        crate::message::ContentPart::Text {
            text: "steer A".to_string()
        }
    );
}

/// Given a session whose journal holds no parked steers at all, When the
/// session is resumed, Then the pending-steer cache is still written (with
/// an empty pair list) — an empty-steer session is the common case, and
/// leaving the cache unset on it makes every later `replayed_pending_steers`
/// read fall back to a full second main-log replay (the F2 gap the cache
/// exists to close).
#[tokio::test]
async fn should_cache_empty_pending_steers_after_resume_when_journal_has_none() {
    let (manager, _tmp) = make_manager();
    let session_id = SessionId::new("session-no-steer-f2");
    let session_dir = manager.persistence().session_dir(&session_id);
    tokio::fs::create_dir_all(&session_dir).await.unwrap();
    let wire_path = session_dir.join("wire.jsonl");
    let mut log = JsonlAppendLog::create(&wire_path).await.unwrap();
    log.append(&WireOp::Metadata {
        protocol_version: None,
        created_at: None,
    })
    .await
    .unwrap();
    log.append(&model_record_op()).await.unwrap();
    drop(log);
    manager
        .persistence()
        .save_state(&session_id, &SessionMetadata::default())
        .await
        .unwrap();

    // Resume: the single fold derived the (empty) steer set.
    manager
        .resume_session(&session_id)
        .await
        .expect("session resumes");

    // The cache must hold the empty result — Some(()) so the embedded
    // assembly's take hits instead of re-running a full second replay.
    assert_eq!(
        manager
            .take_pending_steers(&session_id)
            .map(|steers| steers.len()),
        Some(0),
        "an empty-steer resume must still populate the single-fold cache"
    );
}

/// Given a wire log where a mid-turn user steer was durably flushed (CR13):
/// `turn.prompt(opener) → context.append_message(opener) →
/// turn.steer{origin:user_steer} → turn.steer_flushed{1} →
/// context.append_message{role:user, origin:user_steer} → …assistant
/// step…`, When the session is resumed and the log replays, Then the rebuilt
/// history carries the steer row with the SAME field-identical shape a live
/// session would have appended — role, content, origin, and `entry_seq` all
/// equal — and `steer_pending` is empty (the flush marker consumed the
/// steer, the durable append re-materialized it). Live ≡ replay (ADR-009).
#[tokio::test]
async fn should_replay_a_flushed_user_steer_row_field_identical_to_the_live_append() {
    let (manager, _tmp) = make_manager();
    let session_id = SessionId::new("session-steer-flush-replay");
    let session_dir = manager.persistence().session_dir(&session_id);
    tokio::fs::create_dir_all(&session_dir).await.unwrap();
    let wire_path = session_dir.join("wire.jsonl");

    // The live reference: what the running session's memory would hold
    // after the flush appended the steer (the exact ContextMemory state the
    // flush produces — role user, origin user_steer, the steer content).
    let steer_content = json!([{ "type": "text", "text": "and focus on the rust side" }]);
    let mut live = crate::context_memory::ContextMemory::new();
    live.append_user_message(
        vec![crate::message::ContentPart::Text {
            text: "original question".to_string(),
        }],
        crate::origin::PromptOrigin::User,
    );
    live.append_user_message(
        vec![crate::message::ContentPart::Text {
            text: "and focus on the rust side".to_string(),
        }],
        crate::origin::PromptOrigin::UserSteer,
    );

    // The wire log the flush writes (write-before-mutate order): marker
    // first, then the durable steer append, then the turn's own records.
    {
        let mut log = JsonlAppendLog::create(&wire_path).await.unwrap();
        log.append(&WireOp::Metadata {
            protocol_version: None,
            created_at: None,
        })
        .await
        .unwrap();
        log.append(&model_record_op()).await.unwrap();
        log.append(&WireOp::TurnPrompt {
            input: json!([{ "type": "text", "text": "original question" }]),
            origin: json!({ "kind": "user" }),
        })
        .await
        .unwrap();
        log.append(&WireOp::ContextAppendMessage {
            message: json!({
                "role": "user",
                "content": [{ "type": "text", "text": "original question" }],
                "origin": { "kind": "user" },
            }),
        })
        .await
        .unwrap();
        // The steer parks (busy-path admission tags it user_steer)…
        log.append(&WireOp::TurnSteer {
            input: steer_content.clone(),
            origin: json!({ "kind": "user_steer" }),
        })
        .await
        .unwrap();
        // …the flush consumes it (marker first)…
        log.append(&WireOp::TurnSteerFlushed { count: 1 })
            .await
            .unwrap();
        // …and the flush's durable append re-materializes the row on replay.
        log.append(&WireOp::ContextAppendMessage {
            message: json!({
                "role": "user",
                "content": steer_content,
                "origin": { "kind": "user_steer" },
            }),
        })
        .await
        .unwrap();
        // The turn's assistant reply streams as loop events after the fold.
        log.append(&WireOp::ContextAppendLoopEvent {
            event: json!({ "type": "step.begin", "uuid": "s1", "turnId": "1", "step": 0 }),
        })
        .await
        .unwrap();
        log.append(&WireOp::ContextAppendLoopEvent {
            event: json!({
                "type": "content.part",
                "part": { "type": "text", "text": "steered answer" },
                "uuid": "s1",
            }),
        })
        .await
        .unwrap();
        log.append(&WireOp::ContextAppendLoopEvent {
            event: json!({
                "type": "step.end",
                "uuid": "s1",
                "turnId": "1",
                "step": 0,
                "finishReason": "end_turn",
                "usage": { "inputTokens": 10, "outputTokens": 5 },
            }),
        })
        .await
        .unwrap();
        log.append(&WireOp::TurnEnded {
            turn_id: 1,
            reason: "completed".to_string(),
            error: None,
            duration_ms: None,
        })
        .await
        .unwrap();
    }
    manager
        .persistence()
        .save_state(&session_id, &SessionMetadata::default())
        .await
        .unwrap();

    // When the session resumes (the log folds and the memory rebuilds)
    let resumed = manager
        .resume_session(&session_id)
        .await
        .expect("session resumes");

    // Then the steer row is field-identical to the live append: the
    // opener, the steer, and the assistant reply all replay in wire order.
    let agent = resumed
        .get_agent(&crate::AgentId::new("main"))
        .await
        .expect("resumed session must recreate the main agent");
    let memory = agent.context_memory();
    let replayed = {
        let guard = memory.lock().unwrap_or_else(|e| e.into_inner());
        guard.history().to_vec()
    };
    let live_rows = live.history().to_vec();
    assert_eq!(
        replayed.len(),
        3,
        "opener + steer + settled assistant must all replay: {replayed:?}"
    );
    let live_steer = live_rows
        .iter()
        .find(|m| m.origin.as_ref() == Some(&crate::origin::PromptOrigin::UserSteer))
        .expect("the live reference must hold the steer row");
    let replayed_steer = replayed
        .iter()
        .find(|m| m.origin.as_ref() == Some(&crate::origin::PromptOrigin::UserSteer))
        .expect("the replayed history must carry the steer row (CR13 durable append)");
    assert_eq!(replayed_steer.message.role, live_steer.message.role);
    assert_eq!(replayed_steer.message.content, live_steer.message.content);
    assert_eq!(replayed_steer.origin, live_steer.origin);
    assert_eq!(
        replayed_steer.entry_seq, live_steer.entry_seq,
        "the replayed steer row's entry_seq must equal the live one (ADR-007/009)"
    );

    // And the steer is fully consumed on the replay side: the flush marker
    // drained `steer_pending`, so nothing re-parks (the durable append
    // already re-materialized the row).
    assert!(
        manager.replayed_pending_steers(&session_id).await.is_empty(),
        "a flushed steer must not re-park on resume"
    );
}

/// Given a wire log capturing a MIXED-buffer stop-boundary flush (CR13 fix):
/// `turn.steer{A} → turn.steer{B} → turn.steer{C}` (A/C notifications, B a
/// user steer), the flush marker `turn.steer_flushed{3}` (ALL takes — the
/// reducer's FIFO drain), the flushed steer B's durable append, then FRESH
/// `turn.steer` re-records for the re-parked A and C, When the session
/// resumes, Then the pending set equals the live parked set {A, C} — the
/// flushed user steer B is NOT re-parked (no duplicate row) and neither
/// notification is silently dropped.
#[tokio::test]
async fn should_re_park_exactly_the_live_parked_set_when_resuming_after_a_mixed_buffer_flush() {
    let (manager, _tmp) = make_manager();
    let session_id = SessionId::new("session-mixed-flush-replay");
    let session_dir = manager.persistence().session_dir(&session_id);
    tokio::fs::create_dir_all(&session_dir).await.unwrap();
    let wire_path = session_dir.join("wire.jsonl");

    let notification_origin = |task: &str| {
        json!({
            "kind": "background_task",
            "taskId": task,
            "status": "completed",
            "notificationId": format!("task:{task}:completed"),
        })
    };
    let steer_input = |text: &str| json!([{ "type": "text", "text": text }]);

    {
        let mut log = JsonlAppendLog::create(&wire_path).await.unwrap();
        log.append(&WireOp::Metadata {
            protocol_version: None,
            created_at: None,
        })
        .await
        .unwrap();
        log.append(&model_record_op()).await.unwrap();
        log.append(&WireOp::TurnPrompt {
            input: steer_input("original question"),
            origin: json!({ "kind": "user" }),
        })
        .await
        .unwrap();
        log.append(&WireOp::ContextAppendMessage {
            message: json!({
                "role": "user",
                "content": steer_input("original question"),
                "origin": { "kind": "user" },
            }),
        })
        .await
        .unwrap();
        // The mixed buffer parks A, B, C in FIFO order.
        log.append(&WireOp::TurnSteer {
            input: steer_input("notification A"),
            origin: notification_origin("t1"),
        })
        .await
        .unwrap();
        log.append(&WireOp::TurnSteer {
            input: steer_input("user steer B"),
            origin: json!({ "kind": "user_steer" }),
        })
        .await
        .unwrap();
        log.append(&WireOp::TurnSteer {
            input: steer_input("notification C"),
            origin: notification_origin("t2"),
        })
        .await
        .unwrap();
        // The stop-boundary flush: the marker covers ALL THREE takes (the
        // reducer's FIFO drain), then the flushed user steer's durable
        // append…
        log.append(&WireOp::TurnSteerFlushed { count: 3 })
            .await
            .unwrap();
        log.append(&WireOp::ContextAppendMessage {
            message: json!({
                "role": "user",
                "content": steer_input("user steer B"),
                "origin": { "kind": "user_steer" },
            }),
        })
        .await
        .unwrap();
        // …and each re-parked notification re-records a FRESH `turn.steer`
        // after the drain, so the replayed pending set equals the live
        // parked set.
        log.append(&WireOp::TurnSteer {
            input: steer_input("notification A"),
            origin: notification_origin("t1"),
        })
        .await
        .unwrap();
        log.append(&WireOp::TurnSteer {
            input: steer_input("notification C"),
            origin: notification_origin("t2"),
        })
        .await
        .unwrap();
    }
    manager
        .persistence()
        .save_state(&session_id, &SessionMetadata::default())
        .await
        .unwrap();

    // When the session resumes
    manager
        .resume_session(&session_id)
        .await
        .expect("session resumes");

    // Then the pending set is exactly the two notifications, in FIFO order
    // — the flushed user steer B is not re-parked (it already replayed as
    // history via its durable append) and neither notification dropped.
    let pairs = manager.replayed_pending_steers(&session_id).await;
    let texts: Vec<String> = pairs
        .iter()
        .map(|(content, _)| match &content[0] {
            crate::message::ContentPart::Text { text } => text.clone(),
            _ => String::new(),
        })
        .collect();
    assert_eq!(
        texts,
        vec![
            "notification A".to_string(),
            "notification C".to_string(),
        ],
        "the replayed pending set must equal the live parked set: {texts:?}"
    );
    assert!(
        pairs.iter().all(|(_, origin)| matches!(
            origin,
            crate::origin::PromptOrigin::BackgroundTask { .. }
        )),
        "both re-parked entries keep the background_task origin"
    );

    // And the history holds the steer row exactly once (the durable append
    // re-materialized it; the replay did not add a second copy).
    let session = manager
        .get_session(&session_id)
        .await
        .expect("the resumed session is live");
    let agent = session
        .get_agent(&crate::AgentId::new("main"))
        .await
        .expect("main agent");
    let memory = agent.context_memory();
    let steer_rows = {
        let guard = memory.lock().unwrap_or_else(|e| e.into_inner());
        guard
            .history()
            .iter()
            .filter(|m| m.origin.as_ref() == Some(&crate::origin::PromptOrigin::UserSteer))
            .count()
    };
    assert_eq!(
        steer_rows, 1,
        "the flushed user steer must appear in history exactly once (no duplicate)"
    );
}

/// Given a source wire log with no completed turn at all (only an open
/// bracket), When the session is forked with no turnId, Then the fork is
/// an empty-history session (a legal empty fork).
#[tokio::test]
async fn should_fork_to_an_empty_history_when_the_source_has_no_completed_turn() {
    let (manager, _tmp) = make_manager();
    let source_id = SessionId::new("session-fork-no-completed");
    let session_dir = manager.persistence().session_dir(&source_id);
    tokio::fs::create_dir_all(&session_dir).await.unwrap();
    let wire_path = session_dir.join("wire.jsonl");
    let mut log = JsonlAppendLog::create(&wire_path).await.unwrap();
    log.append(&WireOp::Metadata {
        protocol_version: None,
        created_at: None,
    })
    .await
    .unwrap();
    log.append(&model_record_op()).await.unwrap();
    log.append(&WireOp::TurnPrompt {
        input: json!("open-prompt"),
        origin: json!({ "kind": "user" }),
    })
    .await
    .unwrap();
    log.append(&WireOp::ContextAppendMessage {
        message: json!({
            "role": "user",
            "content": "open-prompt",
            "origin": { "kind": "user" }
        }),
    })
    .await
    .unwrap();
    drop(log);
    manager
        .persistence()
        .save_state(&source_id, &SessionMetadata::default())
        .await
        .unwrap();

    manager.resume_session(&source_id).await.unwrap();
    let fork = manager
        .fork_session_with_options(
            &source_id,
            ForkOptions::default(),
            &AgentId::new(MAIN_AGENT_ID),
            SessionMetadata::default(),
        )
        .await
        .unwrap();

    let markers = resumed_history_markers(&manager, fork.id()).await;
    assert!(
        !markers.iter().any(|m| m.contains("open-prompt")),
        "the open turn must not survive: {markers:?}"
    );
    // And the fork's wire log exists with its metadata header (a legal
    // empty-history fork, not a missing log).
    let fork_wire = manager.persistence().wire_log_path(fork.id());
    let content = tokio::fs::read_to_string(&fork_wire).await.unwrap();
    assert!(
        content.contains("\"metadata\"") || content.contains("metadata"),
        "the fork's log must carry its metadata header: {content}"
    );
}

// -------------------------------------------------------------------------
// F3 — state.json lineage minting: forked_from, custom metadata merge
// -------------------------------------------------------------------------

// -------------------------------------------------------------------------
// F4 — anchor-validation guard (ADR-F4 rev2) + atomic fork landing.
//
// The V4 acceptance triad: a source whose third turn is ACTIVE still forks
// at any COMPLETED anchor (no session-level busy gate — the guard is
// anchor-shaped, not session-shaped), and the default fork cuts the open
// bracket. Plus the zero-residue guarantee: a failed fork leaves neither
// a workspace registration nor a directory behind.
// -------------------------------------------------------------------------

/// Given a source whose third turn is active, When the session is forked
/// at the completed anchor turnId=2, Then the fork succeeds and its
/// history carries exactly the first two completed turns (ADR-F4 rev2:
/// a completed anchor passes even while another turn of the same session
/// is in flight — no session_is_busy gate).
#[tokio::test]
async fn should_fork_at_a_completed_anchor_while_the_source_has_an_active_turn() {
    let (manager, _tmp) = make_manager();
    let source_id = SessionId::new("session-f4-v4-active");
    write_fork_pipeline_source(&manager, &source_id).await;
    manager.resume_session(&source_id).await.unwrap();

    let fork = manager
        .fork_session_with_options(
            &source_id,
            ForkOptions { turn_id: Some(2) },
            &AgentId::new(MAIN_AGENT_ID),
            SessionMetadata::default(),
        )
        .await
        .expect("a completed anchor must pass while another turn is active");

    let markers = resumed_history_markers(&manager, fork.id()).await;
    let joined = markers.join("\n");
    assert!(
        joined.contains("t1") && joined.contains("t2"),
        "both completed turns must survive the turnId=2 fork: {markers:?}"
    );
    assert!(
        !joined.contains("t3") && !joined.contains("r3"),
        "the active turn 3 must not survive: {markers:?}"
    );
}

/// Given a source whose third turn is active, When the session is forked
/// with the default (absent) turnId, Then the fork succeeds and its
/// history carries exactly the completed prefix (the active bracket is
/// cut by the most-recent-completed-turn default anchor).
#[tokio::test]
async fn should_fork_at_the_latest_completed_turn_by_default_while_the_source_has_an_active_turn() {
    let (manager, _tmp) = make_manager();
    let source_id = SessionId::new("session-f4-v4-default");
    write_fork_pipeline_source(&manager, &source_id).await;
    manager.resume_session(&source_id).await.unwrap();

    let fork = manager
        .fork_session_with_options(
            &source_id,
            ForkOptions::default(),
            &AgentId::new(MAIN_AGENT_ID),
            SessionMetadata::default(),
        )
        .await
        .expect("the default anchor must pass while another turn is active");

    let markers = resumed_history_markers(&manager, fork.id()).await;
    let joined = markers.join("\n");
    assert!(
        joined.contains("t1") && joined.contains("t2"),
        "the completed prefix must survive the default fork: {markers:?}"
    );
    assert!(
        !joined.contains("t3") && !joined.contains("r3"),
        "the active turn's content must never carry: {markers:?}"
    );
}

/// Given a source whose third turn is active, When the session is forked
/// with a turnId that matches no completed turn, Then the fork fails with
/// ForkTargetNotFound whose message distinguishes the completed turns
/// from the in-progress one (the wire-log turn ids are the only
/// addressable namespace — an active turn has no turn.ended, hence no
/// id a caller can name; the guard surfaces the tail shape instead of a
/// session-level busy error).
#[tokio::test]
async fn should_explain_the_active_tail_when_the_anchor_matches_no_completed_turn() {
    let (manager, _tmp) = make_manager();
    let source_id = SessionId::new("session-f4-v4-active-anchor");
    write_fork_pipeline_source(&manager, &source_id).await;
    manager.resume_session(&source_id).await.unwrap();

    // Turn 3 is in progress: no turn.ended, so turnId=3 addresses nothing.
    let error = manager
        .fork_session_with_options(
            &source_id,
            ForkOptions { turn_id: Some(3) },
            &AgentId::new(MAIN_AGENT_ID),
            SessionMetadata::default(),
        )
        .await
        .unwrap_err();
    let SessionError::ForkTargetNotFound(message) = &error else {
        panic!("expected ForkTargetNotFound, got {error:?}");
    };
    assert!(
        message.contains("3") && message.contains("1") && message.contains("2"),
        "the message must name the target and the completed range: {message}"
    );
    assert!(
        message.contains("in progress"),
        "the message must distinguish the in-progress tail: {message}"
    );
}

/// Given a fork whose pipeline fails after the target session was created
/// (agent creation fails), When the fork errors, Then neither the
/// workspace map nor the session directory keeps a residue of the failed
/// target (atomic landing, design §3.5).
#[tokio::test]
async fn should_leave_no_workspace_or_directory_residue_when_the_fork_pipeline_fails() {
    let (manager, _tmp) = make_manager();
    let source_id = SessionId::new("session-f4-residue");
    write_fork_pipeline_source(&manager, &source_id).await;
    manager.resume_session(&source_id).await.unwrap();

    // Inject the failure after create_session_inner: the source agent
    // does not resolve (an AgentNotFound agentId), which exercises the
    // post-creation error path the rollback must clean up.
    let sessions_before = manager.workspace.list_sessions().await.len();
    let error = manager
        .fork_session_with_options(
            &source_id,
            ForkOptions { turn_id: Some(2) },
            &AgentId::new("agent-does-not-exist"),
            SessionMetadata::default(),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(error, SessionError::AgentNotFound(_)),
        "the injection must fail with AgentNotFound, got {error:?}"
    );

    let sessions_after = manager.workspace.list_sessions().await.len();
    assert_eq!(
        sessions_after, sessions_before,
        "a failed fork must not leave a live session registered in the workspace"
    );
    // And the target directory must be gone: the sessions root holds only
    // the source's directory. (Fork ids are minted UUIDs — the scan finds
    // no leftover the source does not account for.)
    let sessions_root = manager
        .persistence()
        .session_dir(&source_id)
        .parent()
        .expect("sessions root")
        .to_path_buf();
    let mut dir_names: Vec<String> = Vec::new();
    // zero-io-allow: test-side assertion over the tmp layout
    let mut stack = vec![sessions_root.clone()];
    while let Some(dir) = stack.pop() {
        let mut entries = tokio::fs::read_dir(&dir).await.unwrap();
        while let Some(entry) = entries.next_entry().await.unwrap() {
            let path = entry.path();
            if path.is_dir() {
                let name = path.file_name().unwrap().to_string_lossy().into_owned();
                if !name.starts_with('.') {
                    dir_names.push(name.clone());
                }
                stack.push(path);
            }
        }
    }
    // The root should hold exactly one session directory: the source's.
    assert_eq!(
        dir_names.len(),
        1,
        "a failed fork must delete the minted target directory; found {dir_names:?}"
    );
    assert!(
        manager
            .workspace
            .get_session(&SessionId::new(&dir_names[0]))
            .await
            .is_some(),
        "the surviving directory must be the (live) source: {dir_names:?}"
    );
}

/// Given a fork whose pipeline fails, When the fork errors, Then the
/// failed target's artifacts (event bus / gates) are removed too — a
/// later fork minting a fresh id must not observe stale plumbing.
#[tokio::test]
async fn should_drop_the_failed_targets_artifacts_when_the_fork_pipeline_fails() {
    let (manager, _tmp) = make_manager();
    let source_id = SessionId::new("session-f4-residue-artifacts");
    write_fork_pipeline_source(&manager, &source_id).await;
    manager.resume_session(&source_id).await.unwrap();

    let error = manager
        .fork_session_with_options(
            &source_id,
            ForkOptions { turn_id: Some(1) },
            &AgentId::new("agent-does-not-exist"),
            SessionMetadata::default(),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, SessionError::AgentNotFound(_)));

    // The manager's artifacts map must hold no entry beyond the source's.
    // (session_artifacts returns None for unknown ids — probe the source,
    // which must still resolve, and verify the workspace holds no other
    // live session whose artifacts could linger.)
    assert!(
        manager.session_artifacts(&source_id).is_some(),
        "the source's artifacts must survive the failed fork"
    );
    let live: Vec<_> = manager.workspace.list_sessions().await;
    assert_eq!(
        live.len(),
        1,
        "only the source remains live after the failed fork"
    );
    assert_eq!(live[0], source_id);
}

/// Given a fork at turn 1, When the source session's wire log is re-read,
/// Then it is untouched — the fork only ever writes the NEW session's log.
#[tokio::test]
async fn should_not_touch_the_source_wire_log_when_forking() {
    let (manager, _tmp) = make_manager();
    let source_id = SessionId::new("session-fork-source-untouched");
    write_fork_pipeline_source(&manager, &source_id).await;
    let source_wire = manager.persistence().wire_log_path(&source_id);
    let before = tokio::fs::read_to_string(&source_wire).await.unwrap();

    manager.resume_session(&source_id).await.unwrap();
    let fork = manager
        .fork_session_with_options(
            &source_id,
            ForkOptions { turn_id: Some(1) },
            &AgentId::new(MAIN_AGENT_ID),
            SessionMetadata::default(),
        )
        .await
        .unwrap();

    let after = tokio::fs::read_to_string(&source_wire).await.unwrap();
    assert_eq!(
        before, after,
        "the source wire log must be byte-identical after the fork"
    );
    assert_ne!(fork.id().as_str(), source_id.as_str());
}

// -------------------------------------------------------------------------
// F3 — state.json lineage minting: forked_from, custom metadata merge
// -------------------------------------------------------------------------

/// Given a fork, When the fork's `state.json` is read back from disk,
/// Then it carries `forkedFrom` pointing at the source session id (the
/// lineage the v2 state.json records and the TS client surfaces).
#[tokio::test]
async fn should_mint_forked_from_lineage_in_state_json_when_forking() {
    let (manager, _tmp) = make_manager();
    let source = manager
        .create_session(SessionMetadata::default(), SessionScopeConfig::default())
        .await
        .unwrap();
    source
        .create_agent(
            crate::AgentId::new(MAIN_AGENT_ID),
            crate::AgentScopeConfig::default(),
        )
        .await
        .unwrap();

    let fork = manager
        .fork_session(
            &source,
            &AgentId::new(MAIN_AGENT_ID),
            SessionMetadata::default(),
        )
        .await
        .unwrap();

    let state_path = manager.persistence().state_path(fork.id());
    let raw = tokio::fs::read_to_string(&state_path)
        .await
        .expect("the fork's state.json must exist on disk");
    let state: serde_json::Value = serde_json::from_str(&raw).expect("state.json is valid JSON");
    let forked_from = state
        .get("forked_from")
        .or_else(|| state.get("forkedFrom"))
        .and_then(|v| v.as_str());
    assert_eq!(
        forked_from,
        Some(source.id().as_str()),
        "the fork's state.json must record the source lineage, got: {raw}"
    );
}

/// Given a source session with no cache root of its own, When it is
/// forked, Then the target's persisted state records the source as the
/// cache root (a root session is its own chain root).
#[tokio::test]
async fn should_mint_source_id_as_cache_root_when_forking_a_root_session() {
    let (manager, _tmp) = make_manager();
    let source = manager
        .create_session(SessionMetadata::default(), SessionScopeConfig::default())
        .await
        .unwrap();
    source
        .create_agent(
            crate::AgentId::new(MAIN_AGENT_ID),
            crate::AgentScopeConfig::default(),
        )
        .await
        .unwrap();

    let forked = manager
        .fork_session(
            &source,
            &AgentId::new(MAIN_AGENT_ID),
            SessionMetadata::default(),
        )
        .await
        .unwrap();

    let state = manager.persistence.load_state(forked.id()).await.unwrap();
    assert_eq!(state.cache_root_id.as_deref(), Some(source.id().as_str()));
}

/// Given a source session whose cache root points at the chain root (the
/// source is itself a fork), When it is forked again, Then the new fork
/// inherits the chain root — not the immediate source — so the whole
/// chain keeps hitting the same provider prompt-cache shard.
#[tokio::test]
async fn should_inherit_chain_root_as_cache_root_when_forking_a_forked_session() {
    let (manager, _tmp) = make_manager();
    let root = manager
        .create_session(SessionMetadata::default(), SessionScopeConfig::default())
        .await
        .unwrap();
    root.create_agent(
        crate::AgentId::new(MAIN_AGENT_ID),
        crate::AgentScopeConfig::default(),
    )
    .await
    .unwrap();

    let mid = manager
        .fork_session(
            &root,
            &AgentId::new(MAIN_AGENT_ID),
            SessionMetadata::default(),
        )
        .await
        .unwrap();

    let leaf = manager
        .fork_session(
            &mid,
            &AgentId::new(MAIN_AGENT_ID),
            SessionMetadata::default(),
        )
        .await
        .unwrap();

    let state = manager.persistence.load_state(leaf.id()).await.unwrap();
    assert_eq!(state.cache_root_id.as_deref(), Some(root.id().as_str()));
}

/// Given a fork whose source `state.json` cannot be loaded, When it is
/// forked, Then the target's `cache_root_id` stays `None` — no panic, no
/// fallback mint on an unreadable source.
#[tokio::test]
async fn should_leave_cache_root_absent_when_source_state_is_unreadable() {
    let (manager, _tmp) = make_manager();
    let source = manager
        .create_session(SessionMetadata::default(), SessionScopeConfig::default())
        .await
        .unwrap();
    source
        .create_agent(
            crate::AgentId::new(MAIN_AGENT_ID),
            crate::AgentScopeConfig::default(),
        )
        .await
        .unwrap();
    let state_path = manager.persistence().state_path(source.id());
    tokio::fs::remove_file(&state_path)
        .await
        .expect("the source's state.json must exist to be deleted");

    let target = manager
        .fork_session(
            &source,
            &AgentId::new(MAIN_AGENT_ID),
            SessionMetadata::default(),
        )
        .await
        .unwrap();

    let state = manager.persistence.load_state(target.id()).await.unwrap();
    assert!(state.cache_root_id.is_none());
}

/// Given a `state.json` that predates the fork feature (no `forkedFrom`
/// key), When it goes through a load-mutate-save cycle, Then the existing
/// TS extra fields (`agents`, `custom`) survive untouched — the write path
/// must not regress the TS compatibility guarantees.
#[tokio::test]
async fn should_preserve_ts_extra_fields_across_a_load_mutate_save_cycle() {
    let (manager, _tmp) = make_manager();
    let session_id = SessionId::new("session-f3-old-state");
    let state_path = manager.persistence().state_path(&session_id);
    tokio::fs::create_dir_all(state_path.parent().unwrap())
        .await
        .unwrap();
    tokio::fs::write(
        &state_path,
        r#"{
  "title": "Old",
  "createdAt": "2024-01-01T00:00:00Z",
  "updatedAt": "2024-01-01T00:00:00Z",
  "archived": false,
  "forkedFrom": "session-ancestor",
  "agents": { "main": { "type": "main" } },
  "custom": { "theme": "dark" }
}"#,
    )
    .await
    .unwrap();

    // load → mutate → save (the `set_archived` cycle)
    manager
        .persistence()
        .set_archived(&session_id, true)
        .await
        .unwrap();

    let raw = tokio::fs::read_to_string(&state_path).await.unwrap();
    let state: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(
        state.get("forked_from").and_then(|v| v.as_str()),
        Some("session-ancestor"),
        "forkedFrom must survive the load-mutate-save cycle, got: {raw}"
    );
    assert_eq!(
        state.get("agents"),
        Some(&serde_json::json!({ "main": { "type": "main" } })),
        "the TS agents list must survive the load-mutate-save cycle, got: {raw}"
    );
    assert_eq!(
        state.get("custom"),
        Some(&serde_json::json!({ "theme": "dark" })),
        "the TS custom payload must survive the load-mutate-save cycle, got: {raw}"
    );
    assert_eq!(state.get("archived").and_then(|v| v.as_bool()), Some(true));
}

/// Given a source session whose `state.json` carries custom metadata and
/// a fork carrying incoming metadata, When the fork is minted, Then the
/// fork's custom dict is the merge of source custom (base) + fork metadata
/// (override) with the `goal` key dropped on both sides (the v2
/// `forkCustomMetadata` semantics).
#[tokio::test]
async fn should_merge_source_custom_with_fork_metadata_when_forking() {
    let (manager, _tmp) = make_manager();
    let source_id = SessionId::new("session-f3-merge-source");
    // A real wire log + a source custom dict in state.json (the fork reads
    // the source's persisted state for the custom base).
    write_fork_pipeline_source(&manager, &source_id).await;
    let state_path = manager.persistence().state_path(&source_id);
    let raw = tokio::fs::read_to_string(&state_path).await.unwrap();
    let mut state: serde_json::Value = serde_json::from_str(&raw).unwrap();
    state["custom"] = serde_json::json!({
        "theme": "dark",
        "goal": "source goal",
        "shared": "from-source"
    });
    tokio::fs::write(&state_path, serde_json::to_string(&state).unwrap())
        .await
        .unwrap();
    manager.resume_session(&source_id).await.unwrap();

    let fork = manager
        .fork_session_with_options(
            &source_id,
            ForkOptions::default(),
            &AgentId::new(MAIN_AGENT_ID),
            SessionMetadata {
                title: "Fork".to_string(),
                custom: serde_json::json!({
                    "origin": "fork",
                    "shared": "from-fork",
                    "goal": "fork goal"
                })
                .as_object()
                .cloned(),
                ..SessionMetadata::default()
            },
        )
        .await
        .unwrap();

    let fork_state_path = manager.persistence().state_path(fork.id());
    let raw = tokio::fs::read_to_string(&fork_state_path).await.unwrap();
    let state: serde_json::Value = serde_json::from_str(&raw).unwrap();
    let custom = state.get("custom").expect("the fork must carry custom");
    assert_eq!(
        custom,
        &serde_json::json!({
            "theme": "dark",
            "shared": "from-fork",
            "origin": "fork"
        }),
        "custom must be source-base + fork-override with goal dropped on both sides, got: {raw}"
    );
}

// -------------------------------------------------------------------------
// F5 — entity disposition: member journals, blobs, agents manifest, and
// the directory's entity inventory (no tasks/, no snapshot.json).
//
// The fixtures hand-write a source session whose MAIN log registers two
// subagents and whose per-member journals hold 2 completed turns plus an
// open third bracket each (the journal_route.rs semantics: a member's
// turn ops — TurnPrompt/TurnEnded included — land in the member's OWN
// journal, so each member truncates on its own bracket structure).
// -------------------------------------------------------------------------

/// Write a member journal for `agent_id` under the session: a metadata
/// header, then `turns` completed turns (marker `m{agent}{n}u`/`m{agent}{n}a`
/// per turn), plus an open bracket (marker `...open`) when `open_tail` is
/// set. `turn_base` offsets the member's turn ids (members number turns
/// independently — the brief's reconnaissance conclusion).
async fn write_member_journal(
    manager: &SessionManager,
    session_id: &SessionId,
    agent_id: &str,
    turns: u64,
    open_tail: bool,
    turn_base: u64,
) {
    let journal_path = manager
        .persistence()
        .agent_wire_log_path(session_id, agent_id);
    tokio::fs::create_dir_all(journal_path.parent().unwrap())
        .await
        .unwrap();
    let mut log = JsonlAppendLog::create(&journal_path).await.unwrap();
    log.append(&WireOp::Metadata {
        protocol_version: None,
        created_at: None,
    })
    .await
    .unwrap();
    for n in 1..=turns {
        let (user, reply) = (format!("m{agent_id}{n}u"), format!("m{agent_id}{n}a"));
        log.append(&WireOp::TurnPrompt {
            input: json!(user),
            origin: json!({ "kind": "user" }),
        })
        .await
        .unwrap();
        log.append(&WireOp::ContextAppendMessage {
            message: json!({
                "role": "user",
                "content": user,
                "origin": { "kind": "user" }
            }),
        })
        .await
        .unwrap();
        log.append(&WireOp::ContextAppendMessage {
            message: json!({
                "role": "assistant",
                "content": reply,
                "origin": { "kind": "user" }
            }),
        })
        .await
        .unwrap();
        log.append(&WireOp::TurnEnded {
            turn_id: turn_base + n,
            reason: "completed".to_string(),
            error: None,
            duration_ms: None,
        })
        .await
        .unwrap();
    }
    if open_tail {
        log.append(&WireOp::TurnPrompt {
            input: json!(format!("m{agent_id}open")),
            origin: json!({ "kind": "user" }),
        })
        .await
        .unwrap();
        log.append(&WireOp::ContextAppendMessage {
            message: json!({
                "role": "user",
                "content": format!("m{agent_id}open"),
                "origin": { "kind": "user" }
            }),
        })
        .await
        .unwrap();
    }
    drop(log);
}

/// The two-member source fixture: the standard two-turn main log (turn 2
/// completed, turn 3 open — `write_fork_pipeline_source`'s shape) with
/// TWO `subagent.registered` entries spliced in AFTER turn 2's
/// `turn.ended`... no — BEFORE it (the registration precedes the turn
/// close in a real session's timeline, and the fork's default anchor
/// cuts at the last `turn.ended`; a registration in the post-anchor tail
/// would not carry). Members: `agent-f5-alpha` and `agent-f5-beta`,
/// each journal holding 2 completed turns and an open third bracket,
/// with member turn ids numbered 10../20.. (members number turns
/// independently of the main log).
async fn write_f5_member_source(manager: &SessionManager, session_id: &SessionId) {
    // The main log, hand-written so the registrations land inside the
    // turn-2 bracket (before its turn.ended, the anchor the default
    // fork cuts at).
    let session_dir = manager.persistence().session_dir(session_id);
    tokio::fs::create_dir_all(&session_dir).await.unwrap();
    let wire_path = session_dir.join("wire.jsonl");
    let mut log = JsonlAppendLog::create(&wire_path).await.unwrap();
    log.append(&WireOp::Metadata {
        protocol_version: None,
        created_at: None,
    })
    .await
    .unwrap();
    log.append(&model_record_op()).await.unwrap();
    // Turn 1 (completed).
    log.append(&WireOp::TurnPrompt {
        input: json!("t1"),
        origin: json!({ "kind": "user" }),
    })
    .await
    .unwrap();
    log.append(&WireOp::ContextAppendMessage {
        message: json!({
            "role": "user",
            "content": "t1",
            "origin": { "kind": "user" }
        }),
    })
    .await
    .unwrap();
    log.append(&WireOp::TurnEnded {
        turn_id: 1,
        reason: "completed".to_string(),
        error: None,
        duration_ms: None,
    })
    .await
    .unwrap();
    // Turn 2 (completed): the registrations ride inside this turn's
    // bracket — before its turn.ended, so the default-anchor fork keeps
    // them (the resume face rebuilds children from these entries).
    log.append(&WireOp::TurnPrompt {
        input: json!("t2"),
        origin: json!({ "kind": "user" }),
    })
    .await
    .unwrap();
    for agent_id in ["agent-f5-alpha", "agent-f5-beta"] {
        log.append(&WireOp::SubagentRegistered {
            agent_id: agent_id.to_string(),
            profile_name: "explorer".to_string(),
            model: Some("test-model".to_string()),
            thinking_effort: None,
            parent_agent_id: None,
            swarm_item: None,
            parent_tool_call_id: None,
            swarm_index: None,
        })
        .await
        .unwrap();
    }
    log.append(&WireOp::TurnEnded {
        turn_id: 2,
        reason: "completed".to_string(),
        error: None,
        duration_ms: None,
    })
    .await
    .unwrap();
    // Turn 3: the open bracket the default fork must cut.
    log.append(&WireOp::TurnPrompt {
        input: json!("t3"),
        origin: json!({ "kind": "user" }),
    })
    .await
    .unwrap();
    log.append(&WireOp::ContextAppendMessage {
        message: json!({
            "role": "user",
            "content": "t3",
            "origin": { "kind": "user" }
        }),
    })
    .await
    .unwrap();
    drop(log);
    manager
        .persistence()
        .save_state(session_id, &SessionMetadata::default())
        .await
        .unwrap();

    // The member journals (turn ids 10../20.. — independent numbering).
    write_member_journal(manager, session_id, "agent-f5-alpha", 2, true, 10).await;
    write_member_journal(manager, session_id, "agent-f5-beta", 2, true, 20).await;
}

/// Given a session with one registered subagent whose journal holds a
/// parked steer, When the session is resumed, Then the per-child steer
/// cache was derived by the resume's turn-id scan (single journal read)
/// and the per-child read face serves the parked steer from that cache —
/// no second journal parse/fold (D2).
#[tokio::test]
async fn should_serve_subagent_pending_steers_from_the_single_scan_cache_after_resume() {
    let (manager, _tmp) = make_manager();
    let session_id = SessionId::new("session-agent-steer-d2");
    let session_dir = manager.persistence().session_dir(&session_id);
    tokio::fs::create_dir_all(&session_dir).await.unwrap();
    let wire_path = session_dir.join("wire.jsonl");
    let mut log = JsonlAppendLog::create(&wire_path).await.unwrap();
    log.append(&WireOp::Metadata {
        protocol_version: None,
        created_at: None,
    })
    .await
    .unwrap();
    log.append(&model_record_op()).await.unwrap();
    log.append(&WireOp::SubagentRegistered {
        agent_id: "agent-d2-child".to_string(),
        profile_name: "explorer".to_string(),
        model: Some("test-model".to_string()),
        thinking_effort: None,
        parent_agent_id: None,
        swarm_item: None,
        parent_tool_call_id: None,
        swarm_index: None,
    })
    .await
    .unwrap();
    drop(log);
    // The child journal: metadata + one parked steer (never flushed).
    let journal_path = manager
        .persistence()
        .agent_wire_log_path(&session_id, "agent-d2-child");
    tokio::fs::create_dir_all(journal_path.parent().unwrap())
        .await
        .unwrap();
    let mut child_log = JsonlAppendLog::create(&journal_path).await.unwrap();
    child_log
        .append(&WireOp::Metadata {
            protocol_version: None,
            created_at: None,
        })
        .await
        .unwrap();
    child_log
        .append(&WireOp::TurnSteer {
            input: json!([{ "type": "text", "text": "child steer A" }]),
            origin: json!({ "kind": "user" }),
        })
        .await
        .unwrap();
    drop(child_log);
    manager
        .persistence()
        .save_state(&session_id, &SessionMetadata::default())
        .await
        .unwrap();

    // Resume: the turn-id scan derives the parked steer in its single pass.
    manager
        .resume_session(&session_id)
        .await
        .expect("session resumes");

    // The per-child read face serves the cached entry (no journal re-read).
    let pairs = manager
        .replayed_pending_agent_steers(&session_id, "agent-d2-child")
        .await;
    assert_eq!(pairs.len(), 1, "the child's parked steer is served");
    assert_eq!(
        pairs[0].0[0],
        crate::message::ContentPart::Text {
            text: "child steer A".to_string()
        }
    );

    // And the cache is take-once: a second read falls back to the fresh
    // replay, which still derives the same steer.
    let again = manager
        .replayed_pending_agent_steers(&session_id, "agent-d2-child")
        .await;
    assert_eq!(
        again.len(),
        1,
        "the fallback replay still derives the parked steer"
    );
}

/// Given a resumed session with no subagent journal at all, When the
/// per-child steer read face runs, Then it answers empty without failing
/// (the dormant child simply has no steer buffer).
#[tokio::test]
async fn should_answer_empty_subagent_steers_when_the_child_has_no_journal() {
    let (manager, _tmp) = make_manager();
    let session_id = SessionId::new("session-agent-nojournal-d2");
    let pairs = manager
        .replayed_pending_agent_steers(&session_id, "agent-never-existed")
        .await;
    assert!(
        pairs.is_empty(),
        "a missing child journal answers an empty steer buffer"
    );
}
/// The member journal's text content (empty string when the journal does
/// not exist).
async fn member_journal_text(
    manager: &SessionManager,
    session_id: &SessionId,
    agent_id: &str,
) -> String {
    let path = manager
        .persistence()
        .agent_wire_log_path(session_id, agent_id);
    tokio::fs::read_to_string(&path).await.unwrap_or_default()
}

/// Given a source with two registered members whose journals each hold
/// two completed turns and an open third bracket, When the session is
/// forked, Then both member journals exist in the target and each carries
/// exactly its own two completed turns (the open bracket is cut; the
/// member turn ids are independent of the main log's).
#[tokio::test]
async fn should_truncate_each_member_journal_to_its_latest_completed_turn_when_forking() {
    let (manager, _tmp) = make_manager();
    let source_id = SessionId::new("session-f5-members");
    write_f5_member_source(&manager, &source_id).await;
    manager.resume_session(&source_id).await.unwrap();

    let fork = manager
        .fork_session_with_options(
            &source_id,
            ForkOptions::default(),
            &AgentId::new(MAIN_AGENT_ID),
            SessionMetadata::default(),
        )
        .await
        .unwrap();

    for agent_id in ["agent-f5-alpha", "agent-f5-beta"] {
        let text = member_journal_text(&manager, fork.id(), agent_id).await;
        assert!(
            !text.is_empty(),
            "member {agent_id}'s journal must exist in the fork"
        );
        assert!(
            text.contains(&format!("m{agent_id}1a")) && text.contains(&format!("m{agent_id}2a")),
            "member {agent_id}'s completed turns must carry into the fork, got: {text}"
        );
        assert!(
            !text.contains(&format!("m{agent_id}open")),
            "member {agent_id}'s open bracket must not carry into the fork, got: {text}"
        );
    }
}

/// Given a registered member whose journal holds NO completed turn (only
/// an open bracket), When the session is forked, Then the member journal
/// carries only the turn-less prefix (metadata header) — the open turn's
/// content never reaches the fork (the member-level default of the main
/// log's empty-fork rule).
#[tokio::test]
async fn should_cut_a_member_journal_with_no_completed_turn_to_its_turn_less_prefix_when_forking() {
    let (manager, _tmp) = make_manager();
    let source_id = SessionId::new("session-f5-member-open-only");
    write_f5_member_source(&manager, &source_id).await;
    // Overwrite alpha's journal with an open-only one (1 open turn, 0
    // completed) to probe the member-level empty-cut rule.
    write_member_journal(&manager, &source_id, "agent-f5-alpha", 0, true, 10).await;
    manager.resume_session(&source_id).await.unwrap();

    let fork = manager
        .fork_session_with_options(
            &source_id,
            ForkOptions::default(),
            &AgentId::new(MAIN_AGENT_ID),
            SessionMetadata::default(),
        )
        .await
        .unwrap();

    let text = member_journal_text(&manager, fork.id(), "agent-f5-alpha").await;
    assert!(
        !text.contains("magent-f5-alphaopen"),
        "the open-only member turn must not carry, got: {text}"
    );
    assert!(
        text.contains("metadata") || text.is_empty(),
        "the turn-less prefix (metadata header) must survive, got: {text}"
    );
}

/// Given a source whose `state.json` carries a TS `agents` manifest, When
/// the session is forked, Then the fork's `state.json` carries the same
/// manifest verbatim (member ids unchanged) — the v2 `forkCustomMetadata`
/// companion for agent identity.
#[tokio::test]
async fn should_carry_the_agents_manifest_into_the_forks_state_json_when_forking() {
    let (manager, _tmp) = make_manager();
    let source_id = SessionId::new("session-f5-agents-manifest");
    write_f5_member_source(&manager, &source_id).await;
    let state_path = manager.persistence().state_path(&source_id);
    let raw = tokio::fs::read_to_string(&state_path).await.unwrap();
    let mut state: serde_json::Value = serde_json::from_str(&raw).unwrap();
    state["agents"] = serde_json::json!({
        "agent-f5-alpha": { "type": "subagent", "profile": "explorer" },
        "agent-f5-beta": { "type": "subagent", "profile": "explorer" }
    });
    tokio::fs::write(&state_path, serde_json::to_string(&state).unwrap())
        .await
        .unwrap();
    manager.resume_session(&source_id).await.unwrap();

    let fork = manager
        .fork_session_with_options(
            &source_id,
            ForkOptions::default(),
            &AgentId::new(MAIN_AGENT_ID),
            SessionMetadata::default(),
        )
        .await
        .unwrap();

    let fork_state_path = manager.persistence().state_path(fork.id());
    let raw = tokio::fs::read_to_string(&fork_state_path).await.unwrap();
    let state: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(
        state.get("agents"),
        Some(&serde_json::json!({
            "agent-f5-alpha": { "type": "subagent", "profile": "explorer" },
            "agent-f5-beta": { "type": "subagent", "profile": "explorer" }
        })),
        "the fork's state.json must carry the source's agents manifest verbatim, got: {raw}"
    );
}

/// Given a source whose `state.json` has NO `agents` key (an old session
/// or a main-only session), When the session is forked, Then the fork's
/// `state.json` does not mint one (main-agent-only sessions carry no
/// manifest — the TS shape keeps `main` out of the dict).
#[tokio::test]
async fn should_not_mint_an_agents_manifest_when_the_source_has_none() {
    let (manager, _tmp) = make_manager();
    let source = manager
        .create_session(SessionMetadata::default(), SessionScopeConfig::default())
        .await
        .unwrap();
    source
        .create_agent(
            crate::AgentId::new(MAIN_AGENT_ID),
            crate::AgentScopeConfig::default(),
        )
        .await
        .unwrap();

    let fork = manager
        .fork_session(
            &source,
            &AgentId::new(MAIN_AGENT_ID),
            SessionMetadata::default(),
        )
        .await
        .unwrap();

    let state_path = manager.persistence().state_path(fork.id());
    let raw = tokio::fs::read_to_string(&state_path).await.unwrap();
    let state: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert!(
        state.get("agents").is_none(),
        "a source without an agents manifest must not mint one in the fork, got: {raw}"
    );
}

/// Given a source with a `blobs/` directory holding blob files, When the
/// session is forked, Then every blob file exists in the fork's `blobs/`
/// with identical bytes (a forked session can rehydrate every `blobref:`
/// its carried history references).
#[tokio::test]
async fn should_copy_every_blob_into_the_fork_when_forking() {
    let (manager, _tmp) = make_manager();
    let source_id = SessionId::new("session-f5-blobs");
    write_fork_pipeline_source(&manager, &source_id).await;
    let blobs_dir = manager.persistence().session_dir(&source_id).join("blobs");
    tokio::fs::create_dir_all(&blobs_dir).await.unwrap();
    let blob_a = blobs_dir.join("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    let blob_b = blobs_dir.join("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
    tokio::fs::write(&blob_a, b"blob-a-payload").await.unwrap();
    tokio::fs::write(&blob_b, b"blob-b-payload").await.unwrap();
    manager.resume_session(&source_id).await.unwrap();

    let fork = manager
        .fork_session_with_options(
            &source_id,
            ForkOptions::default(),
            &AgentId::new(MAIN_AGENT_ID),
            SessionMetadata::default(),
        )
        .await
        .unwrap();

    let fork_blobs = manager.persistence().session_dir(fork.id()).join("blobs");
    let copied_a = tokio::fs::read(fork_blobs.join(blob_a.file_name().unwrap()))
        .await
        .expect("blob a must exist in the fork");
    let copied_b = tokio::fs::read(fork_blobs.join(blob_b.file_name().unwrap()))
        .await
        .expect("blob b must exist in the fork");
    assert_eq!(copied_a, b"blob-a-payload".to_vec());
    assert_eq!(copied_b, b"blob-b-payload".to_vec());
}

/// Given a forked session (source with members, blobs, and a state.json),
/// When the target directory's entities are enumerated, Then the entity
/// inventory is exactly the expected set: `wire.jsonl`, `state.json`,
/// `blobs/`, and the member journals — NO `tasks/` directory and NO
/// `snapshot.json` (design §3.3's file-entity table: fork carries
/// conversation + identity + blobs only).
#[tokio::test]
async fn should_land_exactly_the_expected_entities_in_the_forks_directory() {
    let (manager, _tmp) = make_manager();
    let source_id = SessionId::new("session-f5-inventory");
    write_f5_member_source(&manager, &source_id).await;
    let blobs_dir = manager.persistence().session_dir(&source_id).join("blobs");
    tokio::fs::create_dir_all(&blobs_dir).await.unwrap();
    tokio::fs::write(
        blobs_dir.join("cccccccccccccccccccccccccccccccccccccccc"),
        b"blob-c",
    )
    .await
    .unwrap();
    manager.resume_session(&source_id).await.unwrap();

    let fork = manager
        .fork_session_with_options(
            &source_id,
            ForkOptions::default(),
            &AgentId::new(MAIN_AGENT_ID),
            SessionMetadata::default(),
        )
        .await
        .unwrap();

    let fork_dir = manager.persistence().session_dir(fork.id());
    // zero-io-allow: test-side assertion over the fork's on-disk layout
    let mut entities: Vec<String> = Vec::new();
    let mut stack = vec![fork_dir.clone()];
    while let Some(dir) = stack.pop() {
        let mut entries = tokio::fs::read_dir(&dir).await.unwrap();
        while let Some(entry) = entries.next_entry().await.unwrap() {
            let path = entry.path();
            let rel = path
                .strip_prefix(&fork_dir)
                .unwrap()
                .to_string_lossy()
                .into_owned();
            if path.is_dir() {
                stack.push(path);
            } else {
                entities.push(rel);
            }
        }
    }
    entities.sort();
    let expected = vec![
        "agents/agent-f5-alpha/wire.jsonl",
        "agents/agent-f5-beta/wire.jsonl",
        "blobs/cccccccccccccccccccccccccccccccccccccccc",
        "state.json",
        "wire.jsonl",
    ];
    assert_eq!(
        entities, expected,
        "the fork's entity inventory must be exactly the expected set"
    );
}

// -------------------------------------------------------------------------
// Session-scoped turn-id allocation (the close→resume turn-id reset fix):
// the wire log is the sole source of truth — a resumed session's allocator
// is seeded past every turn-id-bearing record the replayed log holds, so
// no flow of the session can re-mint an id the log already recorded.
// -------------------------------------------------------------------------

/// Write a persisted session's wire log whose turn ids repeat across four
/// allocator restarts (the production defect shape: every batch re-numbers
/// its turns 1..=per_batch), then close the live session it belongs to so
/// a resume replays exactly this log.
async fn write_duplicate_ridden_log(manager: &SessionManager, session_id: &SessionId) {
    let session_dir = manager.persistence().session_dir(session_id);
    tokio::fs::create_dir_all(&session_dir).await.unwrap();
    let wire_path = session_dir.join("wire.jsonl");
    let mut log = JsonlAppendLog::create(&wire_path).await.unwrap();
    log.append(&WireOp::Metadata {
        protocol_version: None,
        created_at: None,
    })
    .await
    .unwrap();
    log.append(&model_record_op()).await.unwrap();
    for batch in 0..4u64 {
        for n in 1..=8u64 {
            let user = format!("b{batch}q{n}");
            log.append(&WireOp::TurnPrompt {
                input: json!(user),
                origin: json!({ "kind": "user" }),
            })
            .await
            .unwrap();
            log.append(&WireOp::ContextAppendMessage {
                message: json!({
                    "role": "user",
                    "content": user,
                    "origin": { "kind": "user" }
                }),
            })
            .await
            .unwrap();
            log.append(&WireOp::TurnEnded {
                turn_id: n,
                reason: "completed".to_string(),
                error: None,
                duration_ms: Some(n * 100),
            })
            .await
            .unwrap();
        }
    }
    drop(log);
    manager
        .persistence()
        .save_state(session_id, &SessionMetadata::default())
        .await
        .unwrap();
}

/// Given a wire log whose four allocator restarts each re-numbered their
/// turns 1..=8, When the session is resumed, Then the session's turn-id
/// allocator continues from the GLOBAL wire maximum (32 brackets), not the
/// ended-id maximum (8) — every flow the resumed session creates draws from
/// it, so no id the log already holds is minted again.
#[tokio::test]
async fn should_seed_the_session_turn_id_counter_past_the_wire_log_maximum_when_resuming() {
    let (manager, _tmp) = make_manager();
    let session_id = SessionId::new("session-turn-id-seed");
    write_duplicate_ridden_log(&manager, &session_id).await;

    let session = manager.resume_session(&session_id).await.unwrap();
    assert!(
        manager.workspace.get_session(&session_id).await.is_some(),
        "the session is live after the resume"
    );
    let artifacts = manager
        .session_artifacts(&session_id)
        .expect("the live session has artifacts");
    assert_eq!(
        artifacts.next_turn_id(),
        33,
        "the counter must continue past the 32 turn.prompt brackets the log holds"
    );
    // The counter survives flow dispose/rebuild — dropping the session
    // handle does not reset the numbering while the session stays live.
    drop(session);
    assert_eq!(
        manager
            .session_artifacts(&session_id)
            .unwrap()
            .next_turn_id(),
        33
    );
}

/// Given a wire log whose dangling brackets outnumber its completed turns
/// (8 ended turns, 29 turn.prompt brackets — the aborted-namespace shape),
/// When the session is resumed, Then the allocator is seeded past the
/// DANGLING ordinals too, or an aborted id the log already occupies would
/// be re-minted as a completed turn's id.
#[tokio::test]
async fn should_seed_past_dangling_brackets_when_aborted_ordinals_exceed_the_ended_maximum() {
    let (manager, _tmp) = make_manager();
    let session_id = SessionId::new("session-aborted-ordinal-seed");
    let session_dir = manager.persistence().session_dir(&session_id);
    tokio::fs::create_dir_all(&session_dir).await.unwrap();
    let wire_path = session_dir.join("wire.jsonl");
    let mut log = JsonlAppendLog::create(&wire_path).await.unwrap();
    log.append(&WireOp::Metadata {
        protocol_version: None,
        created_at: None,
    })
    .await
    .unwrap();
    log.append(&model_record_op()).await.unwrap();
    for n in 1..=29u64 {
        log.append(&WireOp::TurnPrompt {
            input: json!(format!("q{n}")),
            origin: json!({ "kind": "user" }),
        })
        .await
        .unwrap();
        if n <= 8 {
            log.append(&WireOp::TurnEnded {
                turn_id: n,
                reason: "completed".to_string(),
                error: None,
                duration_ms: None,
            })
            .await
            .unwrap();
        }
    }
    drop(log);
    manager
        .persistence()
        .save_state(&session_id, &SessionMetadata::default())
        .await
        .unwrap();

    manager.resume_session(&session_id).await.unwrap();
    assert_eq!(
        manager
            .session_artifacts(&session_id)
            .expect("live session artifacts")
            .next_turn_id(),
        30,
        "the counter must clear the 29 turn.prompt ordinals, not the 8 ended ids"
    );
}

/// Given a freshly created session, When its allocator is read, Then it
/// starts at 1 — a session with no history has no floor.
#[tokio::test]
async fn should_start_a_fresh_sessions_turn_id_counter_at_one() {
    let (manager, _tmp) = make_manager();
    let session = manager
        .create_session(SessionMetadata::default(), SessionScopeConfig::default())
        .await
        .unwrap();
    assert_eq!(
        manager
            .session_artifacts(session.id())
            .expect("live session artifacts")
            .next_turn_id(),
        1
    );
}

/// Given a forked session whose persisted log carries the source's turn
/// ids, When the fork lands, Then the fork's allocator is seeded past the
/// truncated history's maximum — a forked session must not re-mint ids its
/// own wire log already holds.
#[tokio::test]
async fn should_seed_the_forked_sessions_turn_id_counter_from_its_truncated_log() {
    let (manager, _tmp) = make_manager();
    let source_id = SessionId::new("session-fork-turn-id-source");
    write_fork_pipeline_source(&manager, &source_id).await;
    // Drive a live source session so the fork has an agent to fork from.
    manager.resume_session(&source_id).await.unwrap();
    let forked = manager
        .fork_session_with_options(
            &source_id,
            ForkOptions::default(),
            &AgentId::new(MAIN_AGENT_ID),
            SessionMetadata::default(),
        )
        .await
        .expect("the fork lands");
    assert_eq!(
        manager
            .session_artifacts(forked.id())
            .expect("the forked session is live")
            .next_turn_id(),
        3,
        "the fork's counter must continue past the 2 brackets the truncated \
         log carries (turns 1 and 2 completed, the open third cut)"
    );
}

/// Given a persisted session with a large wire log, When it is resumed
/// while other async work is in flight, Then that work keeps making
/// progress during the replay — the replay fold must not starve the
/// async executor. Runs on a single-threaded runtime so the starvation
/// the production incident observed (parallel tasks unresponsive while a
/// 2876-op session restored) is reproducible: with the fold on the
/// worker thread, the spawned probe task cannot run at all.
#[tokio::test(flavor = "current_thread")]
async fn should_keep_async_work_schedulable_while_replaying_a_large_wire_log() {
    let (manager, _tmp) = make_manager();
    let session_id = SessionId::new("test-resume-schedulable");

    // A large log: a metadata envelope plus 8000 conversation ops — the
    // fold is CPU-bound JSON parsing + state reduction.
    let session_dir = manager.persistence().session_dir(&session_id);
    tokio::fs::create_dir_all(&session_dir).await.unwrap();
    let wire_path = session_dir.join("wire.jsonl");
    {
        let mut log = JsonlAppendLog::create(&wire_path).await.unwrap();
        log.append(&WireOp::Metadata {
            protocol_version: None,
            created_at: None,
        })
        .await
        .unwrap();
        let ops: Vec<WireOp> = (0..8000)
            .map(|i| WireOp::ContextAppendMessage {
                message: json!({ "role": "user", "content": format!("probe-{i}") }),
            })
            .collect();
        log.append_batch(&ops).await.unwrap();
    }
    manager
        .persistence()
        .save_state(&session_id, &SessionMetadata::default())
        .await
        .unwrap();

    // The liveness probe: a spawned task that yields; on a current-thread
    // runtime it can only advance at the resume's await points. If the
    // replay fold monopolizes the worker thread, the probe's completion
    // (observed via a shared flag after a yield) never happens until the
    // resume is done — the unresponsiveness signature.
    let resumed = Arc::new(tokio::sync::Mutex::new(0usize));
    let observed = Arc::clone(&resumed);
    let probe = tokio::spawn(async move {
        let mut rounds = 0usize;
        for _ in 0..5 {
            tokio::task::yield_now().await;
            rounds += 1;
            let mut guard = observed.lock().await;
            *guard = rounds;
        }
    });

    let resume = tokio::spawn(async move { manager.resume_session(&session_id).await });

    // While the resume is in flight, the probe must complete some rounds.
    // Poll the shared flag with real sleeps: each sleep is an await point
    // the (single) worker thread must return to the scheduler for, and the
    // probe task runs on those scheduling opportunities.
    let mut progressed = false;
    for _ in 0..100 {
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        if *resumed.lock().await > 0 {
            progressed = true;
            break;
        }
        if probe.is_finished() {
            break;
        }
    }
    assert!(
        progressed || probe.is_finished(),
        "async work must stay schedulable while a large wire log replays; \
         the executor was starved for the whole replay window"
    );
    let _ = resume.await.unwrap();
    probe.await.unwrap();
}

#[tokio::test]
async fn should_report_empty_and_reset_anchor_when_journal_truncated_externally() {
    // Given: a live session whose wire log holds the create-time metadata
    // record (the anchor sits at the file's end), and the journal then
    // being externally rebuilt SMALLER than the anchor.
    let (manager, _tmp) = make_manager();
    let session = manager
        .create_session(SessionMetadata::default(), SessionScopeConfig::default())
        .await
        .unwrap();
    session
        .create_agent(AgentId::new(MAIN_AGENT_ID), AgentScopeConfig::default())
        .await
        .unwrap();
    let known = session.wire_log_known_size();
    assert!(known > 0);

    let wire_log_path = manager.persistence().wire_log_path(session.id());
    std::fs::write(&wire_log_path, "{}\n").unwrap();

    // When: the manager absorbs the session's foreign updates
    let report = manager
        .absorb_session_foreign_updates(&session)
        .await
        .unwrap();

    // Then: the truncated journal is not incrementally recoverable — the
    // absorb returns an empty report, the anchor resets to zero, and no
    // op is folded (the upper layer falls back to a full resume).
    assert_eq!(report.ops_absorbed, 0);
    assert_eq!(report.bytes_consumed, 0);
    assert_eq!(session.wire_log_known_size(), 0);
}

// -----------------------------------------------------------------------
// Retention fork rejection (SDR-003): a compressed source session must
// not fork — the user action is "restore first" (US2.3).
// -----------------------------------------------------------------------

/// Given a source session whose wire log is gzip-compressed on disk, When
/// the session is forked, Then the fork fails with the restore-first
/// business error and the compressed source is left untouched.
#[tokio::test]
async fn should_fail_with_the_restore_first_error_when_forking_a_compressed_source() {
    let (manager, _tmp) = make_manager();
    let source_id = SessionId::new("session-fork-compressed");
    write_fork_pipeline_source(&manager, &source_id).await;
    manager.resume_session(&source_id).await.unwrap();
    // Retention: the archived source's wire log was compressed (the
    // on-disk shape is the truth — the compressed source under test).
    let source_dir = manager.persistence().session_dir(&source_id);
    mirri_persistence::compress::compress_wire_log(&source_dir).unwrap();

    let error = manager
        .fork_session_with_options(
            &source_id,
            ForkOptions::default(),
            &AgentId::new(MAIN_AGENT_ID),
            SessionMetadata::default(),
        )
        .await
        .unwrap_err();

    match &error {
        SessionError::ForkTargetNotFound(message) => {
            assert!(
                message.contains("compressed session must be restored before fork"),
                "the error must name the user action: {message}"
            );
        }
        other => panic!("expected the compressed-source rejection, got {other:?}"),
    }
    // And the compressed source is untouched: still the .gz shape.
    assert!(!source_dir.join("wire.jsonl").exists());
    assert!(source_dir.join("wire.jsonl.gz").exists());
}

/// Given a plain (un-compressed) source session, When the session is
/// forked, Then the fork succeeds — the retention gate is invisible to
/// normal sources.
#[tokio::test]
async fn should_fork_normally_when_the_source_session_is_not_compressed() {
    let (manager, _tmp) = make_manager();
    let source_id = SessionId::new("session-fork-plain");
    write_fork_pipeline_source(&manager, &source_id).await;

    manager.resume_session(&source_id).await.unwrap();
    let fork = manager
        .fork_session_with_options(
            &source_id,
            ForkOptions::default(),
            &AgentId::new(MAIN_AGENT_ID),
            SessionMetadata::default(),
        )
        .await
        .unwrap();

    let markers = resumed_history_markers(&manager, fork.id()).await;
    assert!(
        markers
            .iter()
            .any(|marker| marker.contains("t1") || marker.contains("t2")),
        "the fork carries the source's completed turns: {markers:?}"
    );
}

#[tokio::test]
async fn should_answer_a_business_error_when_resuming_a_compressed_session() {
    // Given a persisted session whose wire log was compressed (the
    // retention domain's .gz shape — the B3 regression: the old path
    // surfaced an opaque "wire log not found" / a hard read error instead
    // of naming the user action)
    let (manager, _tmp) = make_manager();
    let session_id = SessionId::new("compressed-resume");
    let session_dir = manager.persistence().session_dir(&session_id);
    tokio::fs::create_dir_all(&session_dir).await.unwrap();
    let wire_path = session_dir.join("wire.jsonl");
    let mut log = JsonlAppendLog::create(&wire_path).await.unwrap();
    log.append(&WireOp::Metadata {
        protocol_version: None,
        created_at: None,
    })
    .await
    .unwrap();
    log.append(&model_record_op()).await.unwrap();
    drop(log);
    manager
        .persistence()
        .save_state(&session_id, &SessionMetadata::default())
        .await
        .unwrap();
    mirri_persistence::compress::compress_wire_log(&session_dir).unwrap();
    assert!(!wire_path.exists(), "the plain log must be gone");

    // When the session is resumed
    let outcome = manager.resume_session(&session_id).await;

    // Then the failure is the BUSINESS error naming the restore path —
    // never an opaque NotFound and never a silent success
    let error = outcome.expect_err("a compressed session must not resume");
    assert!(
        error.to_string().contains("compressed"),
        "the error must name the compressed state: {error}"
    );
    assert!(
        error.to_string().contains("restore"),
        "the error must point at session.restore: {error}"
    );
}

#[tokio::test]
async fn should_resume_successfully_after_the_restore_path_decompresses() {
    // Given the same compressed session restored first (the user-action
    // the error above prescribes), when resumed, then it succeeds — the
    // guard and the restore path compose
    let (manager, _tmp) = make_manager();
    let session_id = SessionId::new("compressed-then-restore");
    let session_dir = manager.persistence().session_dir(&session_id);
    tokio::fs::create_dir_all(&session_dir).await.unwrap();
    let wire_path = session_dir.join("wire.jsonl");
    let mut log = JsonlAppendLog::create(&wire_path).await.unwrap();
    log.append(&WireOp::Metadata {
        protocol_version: None,
        created_at: None,
    })
    .await
    .unwrap();
    log.append(&model_record_op()).await.unwrap();
    drop(log);
    manager
        .persistence()
        .save_state(&session_id, &SessionMetadata::default())
        .await
        .unwrap();
    mirri_persistence::compress::compress_wire_log(&session_dir).unwrap();

    // When the restore-decompression runs (the embedded session.restore
    // dispatch's core seam) and the session is then resumed
    crate::session::retention::restore_decompress(manager.persistence(), &session_id, &session_dir)
        .await
        .unwrap();
    let session = manager.resume_session(&session_id).await.unwrap();

    // Then the resume succeeded (the main agent is back)
    assert!(
        session
            .get_agent(&AgentId::new(MAIN_AGENT_ID))
            .await
            .is_some()
    );
}
