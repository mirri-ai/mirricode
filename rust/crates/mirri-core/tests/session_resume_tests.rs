//! Integration tests for session resume with wire-log replay (Task: session
//! resume).
//!
//! These tests verify that `SessionManager::resume_session` reconstructs a
//! closed session's agent state by replaying its `wire.jsonl` through the
//! wire-op reducer:
//!
//! - full replay from an empty base state when no snapshot exists;
//! - incremental replay from a `snapshot.json` checkpoint when one exists;
//! - empty wire logs resume to an empty (but usable) agent;
//! - closing a session writes a snapshot and resume recreates the agents.
//!
//! Tests use the Given-When-Then naming convention.

use std::collections::HashSet;

use mirri_core::shell_command::{ShellCommandRegistry, ShellRunTarget, run_shell_command};
use mirri_core::task::TaskStatus;
use mirri_core::todo::{TodoItem, TodoStatus};
use mirri_core::{
    AgentBlobService, AgentId, AgentScopeConfig, AppScope, AppScopeConfig, CloseReason, EventBus,
    PromptOrigin, Role, SessionError, SessionId, SessionManager, SessionMetadata,
    SessionScopeConfig, WorkspaceId, WorkspaceScopeConfig, WorkspaceTrustLevel,
};
use mirri_persistence::{AppendLogStore, JsonlAppendLog, SnapshotStore};
use mirri_proto::{AgentState, WireOp};
use serde_json::json;

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

/// A resume base config carrying an explicit model (spec 11 R4): the
/// hand-written wire logs in this file carry no model record, so the base
/// config seeds the model exactly the way the embedded host seeds it from
/// the config snapshot's `default_model` (the no-record fallback chain:
/// config default model → loud error).
fn resume_base_config() -> AgentScopeConfig {
    AgentScopeConfig {
        model_id: "test-resume-model".to_string(),
        ..AgentScopeConfig::default()
    }
}

/// Write a sequence of wire ops to the session's `wire.jsonl`
/// (truncating any existing file).
async fn write_wire_log(manager: &SessionManager, session_id: &SessionId, ops: &[WireOp]) {
    let wire_log_path = manager.persistence().wire_log_path(session_id);
    let mut log = JsonlAppendLog::create(&wire_log_path).await.unwrap();
    for op in ops {
        log.append(op).await.unwrap();
    }
}

/// Build a `context.append_message` op in the TS wire shape (camelCase
/// `toolCalls`, content parts tagged with `type`).
fn append_message_op(role: &str, text: &str) -> WireOp {
    WireOp::ContextAppendMessage {
        message: json!({
            "role": role,
            "content": [{"type": "text", "text": text}],
            "toolCalls": [],
        }),
    }
}

/// A full assistant step: step.begin → content.part → tool.call →
/// tool.result → step.end (shapes taken from the TS wire fixtures).
fn assistant_step_ops(step_id: &str, call_id: &str, text: &str) -> [WireOp; 5] {
    [
        WireOp::ContextAppendLoopEvent {
            event: json!({"type": "step.begin", "uuid": step_id, "turnId": "1", "step": 0}),
        },
        WireOp::ContextAppendLoopEvent {
            event: json!({"type": "content.part", "part": {"type": "text", "text": text}, "uuid": step_id}),
        },
        WireOp::ContextAppendLoopEvent {
            event: json!({
                "type": "tool.call",
                "callId": call_id,
                "name": "Read",
                "input": {"path": "/tmp/test.txt"},
                "uuid": step_id,
            }),
        },
        WireOp::ContextAppendLoopEvent {
            event: json!({
                "type": "tool.result",
                "callId": call_id,
                "result": {"output": [{"type": "text", "text": "file contents"}]},
                "uuid": step_id,
            }),
        },
        WireOp::ContextAppendLoopEvent {
            event: json!({
                "type": "step.end",
                "uuid": step_id,
                "turnId": "1",
                "step": 0,
                "finishReason": "end_turn",
                "usage": {"inputTokens": 10, "outputTokens": 5},
            }),
        },
    ]
}

/// The text content of each message in the agent's reconstructed history.
fn history_texts(agent: &mirri_core::AgentScope) -> Vec<String> {
    let memory = agent.context_memory();
    let guard = memory.lock().unwrap();
    guard
        .history()
        .iter()
        .map(|m| m.message.text_content())
        .collect()
}

// ===========================================================================
// SessionManager — resume replays the wire log into agent state
// ===========================================================================

#[tokio::test]
async fn should_reconstruct_state_when_session_with_wire_log_resumed() {
    // Given a closed session whose wire log records one user turn: two
    // plain messages plus a full assistant step (text + tool call + result)
    let env = TestEnv::new().await;
    let session = env
        .manager
        .create_session_with_id(
            SessionId::new("resume-wire-log"),
            make_metadata("Wire Log", &["resume"]),
            SessionScopeConfig::default(),
        )
        .await
        .unwrap();
    let session_id = session.id().clone();

    let mut ops = vec![
        WireOp::Metadata {
            protocol_version: Some("1.5".to_string()),
            created_at: Some(1700000000000),
        },
        append_message_op("user", "hello"),
        append_message_op("assistant", "hi there"),
    ];
    ops.extend(assistant_step_ops("step-001", "call-001", "Let me check."));
    write_wire_log(&env.manager, &session_id, &ops).await;

    env.manager
        .close_session(&session_id, CloseReason::Exit)
        .await
        .unwrap();
    assert!(env.manager.get_session(&session_id).await.is_none());

    // When resuming the session
    let resumed = env
        .manager
        .resume_session_with_limits(&session_id, resume_base_config(), None)
        .await
        .unwrap();

    // Then the main agent is recreated with the reconstructed context
    let agent = resumed
        .get_agent(&AgentId::new("main"))
        .await
        .expect("resumed session must recreate the main agent");
    let texts = history_texts(&agent);
    assert_eq!(
        texts,
        vec![
            "hello".to_string(),
            "hi there".to_string(),
            "Let me check.".to_string(),
            "file contents".to_string()
        ],
        "replayed state must yield the recorded messages in wire order"
    );

    let memory = agent.context_memory();
    {
        let guard = memory.lock().unwrap();
        let history = guard.history();
        assert_eq!(history.len(), 4);
        assert_eq!(history[0].message.role, Role::User);
        assert_eq!(history[1].message.role, Role::Assistant);

        // The step message carries the content part and the tool call
        assert_eq!(history[2].message.role, Role::Assistant);
        assert_eq!(history[2].message.tool_calls.len(), 1);
        assert_eq!(history[2].message.tool_calls[0].id, "call-001");
        assert_eq!(history[2].message.tool_calls[0].function.name, "Read");

        // The tool result message is attached via toolCallId (TS camelCase)
        assert_eq!(history[3].message.role, Role::Tool);
        assert_eq!(history[3].message.tool_call_id.as_deref(), Some("call-001"));
        assert!(!history[3].is_error);

        // Step usage is folded into the token count and the exchange is closed
        assert_eq!(guard.token_count(), 15);
        assert!(!guard.has_open_tool_exchange());
    } // guard dropped here, before the await below

    // And the session metadata is restored
    let metadata = resumed.metadata().await;
    assert_eq!(metadata.title, "Wire Log");
    assert!(metadata.tags.contains("resume"));
}

/// should reconstruct both inline skill activation wraps plus the body when a
/// session whose wire log records them is resumed (SDR-005 / US3.6 replay:
/// two activations in one turn replay as two independent activation facts and
/// no duplicate telemetry — the facts ride as `skill.activate` ops and the
/// wraps + body as ordered `context.append_message` records).
#[tokio::test]
async fn should_reconstruct_inline_skill_activations_when_session_resumed() {
    // Given a closed session whose wire log records one user turn with two
    // inline skill activation wraps threaded before the body message (the
    // ADR-004 "N wraps + 1 body" sequence) plus the assistant reply
    let env = TestEnv::new().await;
    let session = env
        .manager
        .create_session_with_id(
            SessionId::new("resume-inline-skills"),
            make_metadata("Inline", &[]),
            SessionScopeConfig::default(),
        )
        .await
        .unwrap();
    let session_id = session.id().clone();
    let alpha_origin = json!({
        "kind": "skill_activation",
        "activationId": "msg-abc#0",
        "skillName": "alpha",
        "trigger": "user-inline",
        "skillPath": "/tmp/skills/alpha/SKILL.md",
        "skillSource": "project",
    });
    let beta_origin = json!({
        "kind": "skill_activation",
        "activationId": "msg-abc#1",
        "skillName": "beta",
        "trigger": "user-inline",
        "skillPath": "/tmp/skills/beta/SKILL.md",
        "skillSource": "project",
    });
    let ops: Vec<WireOp> = vec![
        WireOp::Metadata {
            protocol_version: Some("1.5".to_string()),
            created_at: Some(1700000000000),
        },
        WireOp::TurnPrompt {
            input: json!([{"type": "text", "text": "body"}]),
            origin: json!({"kind": "user"}),
        },
        WireOp::SkillActivate {
            origin: alpha_origin.clone(),
        },
        WireOp::ContextAppendMessage {
            message: json!({
                "role": "user",
                "content": [{"type": "text", "text": "Alpha wrap"}],
                "origin": alpha_origin,
            }),
        },
        WireOp::SkillActivate {
            origin: beta_origin.clone(),
        },
        WireOp::ContextAppendMessage {
            message: json!({
                "role": "user",
                "content": [{"type": "text", "text": "Beta wrap"}],
                "origin": beta_origin,
            }),
        },
        WireOp::ContextAppendMessage {
            message: json!({
                "role": "user",
                "content": [{"type": "text", "text": "body"}],
                "origin": json!({"kind": "user"}),
            }),
        },
        append_message_op("assistant", "ok."),
    ];
    write_wire_log(&env.manager, &session_id, &ops).await;

    env.manager
        .close_session(&session_id, CloseReason::Exit)
        .await
        .unwrap();
    assert!(env.manager.get_session(&session_id).await.is_none());

    // When resuming the session
    let resumed = env
        .manager
        .resume_session_with_limits(&session_id, resume_base_config(), None)
        .await
        .unwrap();

    // Then the reconstructed context carries both wraps then the body then the
    // reply, and each activation message keeps its user-inline origin (the two
    // activation facts are recovered — no telemetry is re-run on replay)
    let agent = resumed
        .get_agent(&AgentId::new("main"))
        .await
        .expect("resumed session recreates the main agent");
    let texts = history_texts(&agent);
    // The two wraps plus the body plus the assistant reply. (The leading
    // TurnPrompt is a turn-op marker, not a message.)
    assert_eq!(
        texts,
        vec![
            "Alpha wrap".to_string(),
            "Beta wrap".to_string(),
            "body".to_string(),
            "ok.".to_string(),
        ],
        "replay yields wraps + body + reply in wire order"
    );
    let memory = agent.context_memory();
    let guard = memory.lock().unwrap();
    let history = guard.history();
    assert_eq!(history.len(), 4);
    assert_eq!(
        history[0].origin.as_ref(),
        Some(&PromptOrigin::SkillActivation {
            activation_id: Some("msg-abc#0".into()),
            skill_name: Some("alpha".into()),
            trigger: Some("user-inline".into()),
            skill_args: None,
            skill_path: Some("/tmp/skills/alpha/SKILL.md".into()),
            skill_source: Some("project".into()),
        })
    );
    assert_eq!(
        history[1].origin.as_ref(),
        Some(&PromptOrigin::SkillActivation {
            activation_id: Some("msg-abc#1".into()),
            skill_name: Some("beta".into()),
            trigger: Some("user-inline".into()),
            skill_args: None,
            skill_path: Some("/tmp/skills/beta/SKILL.md".into()),
            skill_source: Some("project".into()),
        })
    );
    // The body message is plain user input, the reply has no origin.
    assert_eq!(history[2].origin.as_ref(), Some(&PromptOrigin::User));
    assert_eq!(history[3].origin.as_ref(), None);
}

// ===========================================================================
// SessionManager — resume with a snapshot replays only from the checkpoint
// ===========================================================================

#[tokio::test]
async fn should_only_replay_from_checkpoint_when_session_with_snapshot_resumed() {
    // Given a persisted session whose wire log has three messages and a
    // snapshot checkpoint taken after the first op. The snapshot's state is
    // crafted to contain a message NOT in the log, so the test can observe
    // whether resume loads the snapshot as its base state (it must) and
    // whether it re-folds the ops before the checkpoint (it must not).
    let env = TestEnv::new().await;
    let session_id = SessionId::new("resume-snapshot");
    env.manager
        .persistence()
        .save_state(&session_id, &make_metadata("Checkpoint", &[]))
        .await
        .unwrap();
    write_wire_log(
        &env.manager,
        &session_id,
        &[
            append_message_op("user", "from-log-1"),
            append_message_op("user", "from-log-2"),
            append_message_op("user", "from-log-3"),
        ],
    )
    .await;

    let mut snapshot_state = AgentState::new();
    snapshot_state.conversation.messages.push(json!({
        "role": "user",
        "content": [{"type": "text", "text": "from-snapshot"}],
        "toolCalls": [],
    }));
    let snapshot_path = env.manager.persistence().snapshot_path(&session_id);
    SnapshotStore::write(&snapshot_path, &snapshot_state, 1)
        .await
        .unwrap();
    assert!(snapshot_path.exists());

    // When resuming
    let resumed = env
        .manager
        .resume_session_with_limits(&session_id, resume_base_config(), None)
        .await
        .unwrap();

    // Then the agent's context is the snapshot base plus exactly the ops
    // recorded after the checkpoint — op 0 was not replayed
    let agent = resumed
        .get_agent(&AgentId::new("main"))
        .await
        .expect("main agent recreated");
    let texts = history_texts(&agent);
    assert_eq!(
        texts,
        vec![
            "from-snapshot".to_string(),
            "from-log-2".to_string(),
            "from-log-3".to_string(),
        ],
        "resume must load the snapshot state and replay only ops after the checkpoint"
    );
    assert!(
        !texts.contains(&"from-log-1".to_string()),
        "ops before the snapshot op_index must not be replayed"
    );
}

// ===========================================================================
// SessionManager — resume with an empty wire log yields an empty state
// ===========================================================================

#[tokio::test]
async fn should_yield_empty_state_when_session_with_no_wire_log_resumed() {
    // Given a persisted session whose wire log exists but is empty
    let env = TestEnv::new().await;
    let session_id = SessionId::new("resume-empty-log");
    env.manager
        .persistence()
        .save_state(&session_id, &make_metadata("Empty Log", &[]))
        .await
        .unwrap();
    let wire_log_path = env.manager.persistence().wire_log_path(&session_id);
    tokio::fs::create_dir_all(wire_log_path.parent().unwrap())
        .await
        .unwrap();
    tokio::fs::write(&wire_log_path, "").await.unwrap();

    // When resuming
    let resumed = env
        .manager
        .resume_session_with_limits(&session_id, resume_base_config(), None)
        .await
        .unwrap();

    // Then the session is live with the main agent holding an empty context
    let agent = resumed
        .get_agent(&AgentId::new("main"))
        .await
        .expect("main agent recreated");
    let memory = agent.context_memory();
    let guard = memory.lock().unwrap();
    assert!(
        guard.history().is_empty(),
        "empty wire log must yield an empty context"
    );
    assert_eq!(guard.token_count(), 0);
    assert!(!guard.has_open_tool_exchange());
}

// ===========================================================================
// SessionManager — close writes a snapshot; resume recreates the agents
// ===========================================================================

#[tokio::test]
async fn should_recreate_agents_when_closed_session_resumed() {
    // Given a session with a recorded conversation that is closed (close
    // must persist a snapshot checkpoint)
    let env = TestEnv::new().await;
    let session = env
        .manager
        .create_session_with_id(
            SessionId::new("resume-after-close"),
            make_metadata("Closed Session", &["restored"]),
            SessionScopeConfig::default(),
        )
        .await
        .unwrap();
    let session_id = session.id().clone();
    write_wire_log(
        &env.manager,
        &session_id,
        &[
            append_message_op("user", "question one"),
            append_message_op("assistant", "answer one"),
        ],
    )
    .await;

    let snapshot_path = env.manager.persistence().snapshot_path(&session_id);
    assert!(!snapshot_path.exists(), "no snapshot before close");

    env.manager
        .close_session(&session_id, CloseReason::Exit)
        .await
        .unwrap();
    assert!(env.manager.get_session(&session_id).await.is_none());

    // Then the close wrote a snapshot checkpoint
    assert!(
        snapshot_path.exists(),
        "closing a session with a wire log must write a snapshot"
    );

    // When resuming the closed session
    let resumed = env
        .manager
        .resume_session_with_limits(&session_id, resume_base_config(), None)
        .await
        .unwrap();

    // Then the agent is recreated with the reconstructed context
    let agents = resumed.list_agents().await;
    assert_eq!(
        agents.len(),
        1,
        "resumed session must contain its main agent"
    );
    assert_eq!(agents[0], AgentId::new("main"));

    let agent = resumed.get_agent(&AgentId::new("main")).await.unwrap();
    assert_eq!(
        history_texts(&agent),
        vec!["question one".to_string(), "answer one".to_string()]
    );

    // And the metadata survives the close/resume round trip
    let metadata = resumed.metadata().await;
    assert_eq!(metadata.title, "Closed Session");
    assert!(metadata.tags.contains("restored"));
}

// ===========================================================================
// SessionManager — a compacted wire log resumes with the summary restored
// ===========================================================================

#[tokio::test]
async fn should_restore_summary_message_when_compacted_wire_log_resumed() {
    // Given a session whose wire log contains a compaction: two messages
    // replaced by a summary, then one more message
    let env = TestEnv::new().await;
    let session_id = SessionId::new("resume-compacted");
    env.manager
        .persistence()
        .save_state(&session_id, &make_metadata("Compacted", &[]))
        .await
        .unwrap();
    write_wire_log(
        &env.manager,
        &session_id,
        &[
            append_message_op("user", "old message one"),
            append_message_op("user", "old message two"),
            WireOp::ContextApplyCompaction {
                summary: "context compacted here".to_string(),
                compacted_count: 2,
                tokens_before: Some(60_000),
                tokens_after: Some(800),
                context_summary: None,
                dropped_count: Some(2),
                retained_tail_count: None,
                kept_user_message_count: None,
                kept_head_user_message_count: None,
                elision_message: None,
                first_replaced_entry_seq: None,
                last_replaced_entry_seq: None,
                dropped_entry_seqs: None,
            },
            append_message_op("user", "message after compaction"),
        ],
    )
    .await;

    // When resuming
    let resumed = env
        .manager
        .resume_session_with_limits(&session_id, resume_base_config(), None)
        .await
        .unwrap();

    // Then the compacted messages are gone and the summary is restored as a
    // user message tagged with the compaction origin, ahead of the newer
    // message
    let agent = resumed.get_agent(&AgentId::new("main")).await.unwrap();
    let memory = agent.context_memory();
    let guard = memory.lock().unwrap();
    let history = guard.history();
    assert_eq!(history.len(), 2);
    assert_eq!(
        history[0].origin,
        Some(PromptOrigin::CompactionSummary),
        "compaction summary must be restored with the compaction origin"
    );
    assert_eq!(history[0].message.text_content(), "context compacted here");
    assert_eq!(
        history[1].message.text_content(),
        "message after compaction"
    );
}

// ===========================================================================
// SessionManager — resume edge cases
// ===========================================================================

#[tokio::test]
async fn should_return_error_when_resume_session_with_nonexistent_id() {
    // Given a session id that has never been created or persisted
    let env = TestEnv::new().await;
    let session_id = SessionId::new("never-created");

    // When attempting to resume it
    let result = env.manager.resume_session(&session_id).await;

    // Then resume fails because the wire log does not exist
    assert!(
        matches!(result, Err(SessionError::ResumeFailed(_))),
        "resuming a nonexistent session must fail, got: {result:?}"
    );
}

/// Given a persisted session whose wire log carries no model record (no
/// `config.update` op) and a resume base config with no model, when the
/// session is resumed, then resume fails loudly with an actionable error
/// pointing at the configuration path — never a silent fallback to a
/// built-in model (spec 11 R9: the no-record fallback chain equals the
/// fresh-create chain: config default model → error).
#[tokio::test]
async fn should_fail_resume_with_actionable_error_when_no_model_record_and_no_default() {
    let env = TestEnv::new().await;
    let session_id = SessionId::new("resume-no-model");
    env.manager
        .create_session_with_id(
            session_id.clone(),
            make_metadata("No Model", &["resume"]),
            SessionScopeConfig::default(),
        )
        .await
        .unwrap();
    // A wire log with conversation ops only — no `config.update` model
    // record anywhere.
    write_wire_log(
        &env.manager,
        &session_id,
        &[
            WireOp::Metadata {
                protocol_version: Some("1.5".to_string()),
                created_at: Some(1700000000000),
            },
            append_message_op("user", "hello"),
        ],
    )
    .await;
    env.manager
        .close_session(&session_id, CloseReason::Exit)
        .await
        .unwrap();

    // When the session is resumed on a base config carrying no model
    let result = env.manager.resume_session(&session_id).await;

    // Then resume fails with an actionable message naming the remediation
    // (a model record or a configured default model), not a built-in one
    let error = result.expect_err("resume must fail when no model is resolvable");
    assert!(
        matches!(&error, SessionError::ResumeFailed(message)
            if message.contains("default_model")),
        "expected an actionable ResumeFailed error, got: {error:?}"
    );
    assert!(
        !error.to_string().contains("claude-"),
        "the error must not name any built-in model: {error}"
    );
}

/// Given a multi-agent session whose wire log records two session-level
/// model switches (the second to "switch-two") and a registered subagent
/// whose registration pins "pinned-at-spawn", when the session is
/// resumed, then the main agent's model is the LAST switch record (A17 —
/// the switch records carry no agent attribution, so no fold
/// misattribution is possible) and the child is rehydrated on its
/// registration's pinned value (A14/R5 — the spawn-time pin survives the
/// restart, spec 11 §2.4).
#[tokio::test]
async fn should_restore_multi_agent_session_to_last_switched_model() {
    // Given a persisted multi-agent session: two session-level switch
    // records plus a pinned subagent registration, and the child's own
    // (empty) journal on disk so the restore rehydrates it
    let env = TestEnv::new().await;
    let session_id = SessionId::new("resume-multi-agent");
    env.manager
        .persistence()
        .save_state(&session_id, &make_metadata("Multi Agent", &["resume"]))
        .await
        .unwrap();
    let config_update = |alias: &str| WireOp::ConfigUpdate {
        model_alias: Some(alias.to_string()),
        profile_name: None,
        thinking_effort: None,
        system_prompt: None,
        disallowed_tools: None,
    };
    let child_id = "agent-pinned";
    write_wire_log(
        &env.manager,
        &session_id,
        &[
            WireOp::Metadata {
                protocol_version: Some("1.5".to_string()),
                created_at: Some(1700000000000),
            },
            config_update("switch-one"),
            config_update("switch-two"),
            WireOp::SubagentRegistered {
                agent_id: child_id.to_string(),
                profile_name: "coder".to_string(),
                model: Some("pinned-at-spawn".to_string()),
                thinking_effort: None,
                parent_agent_id: Some("main".to_string()),
                swarm_item: None,
                parent_tool_call_id: None,
                swarm_index: None,
            },
        ],
    )
    .await;
    let child_journal = env
        .manager
        .persistence()
        .agent_wire_log_path(&session_id, child_id);
    std::fs::create_dir_all(child_journal.parent().unwrap()).unwrap();
    std::fs::write(&child_journal, "").unwrap();

    // When the session is resumed
    let resumed = env
        .manager
        .resume_session_with_limits(&session_id, resume_base_config(), None)
        .await
        .expect("resume succeeds");

    // Then the main agent carries the LAST switch record (A17) — neither
    // the base config's default nor the first switch
    let main = resumed
        .get_agent(&AgentId::new("main"))
        .await
        .expect("main agent recreated");
    assert_eq!(
        main.config()
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .model_id,
        "switch-two",
        "the session-level records fold onto the main agent in order — the last switch wins"
    );

    // And the subagent, once woken (lazy restore: it stays dormant until the
    // child is hydrated), is rehydrated on its registration's pinned model —
    // the dispatch-time pin survives the restart (A14/R5)
    assert!(
        resumed.hydrate_subagent(child_id).await,
        "the dormant child hydrates on wake"
    );
    let child = resumed
        .get_agent(&AgentId::new(child_id))
        .await
        .expect("the registered child is rehydrated");
    assert_eq!(
        child
            .config()
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .model_id,
        "pinned-at-spawn",
        "the child keeps its dispatch-time pinned model across the resume"
    );
}

#[tokio::test]
async fn should_resume_with_multiple_assistant_steps_when_wire_log_has_many() {
    // Given a session whose wire log records a user turn followed by three
    // full assistant steps (text + tool call + result each)
    let env = TestEnv::new().await;
    let session_id = SessionId::new("resume-many-steps");
    env.manager
        .persistence()
        .save_state(&session_id, &make_metadata("Many Steps", &["multi"]))
        .await
        .unwrap();
    let mut ops = vec![append_message_op("user", "start")];
    ops.extend(assistant_step_ops("step-001", "call-001", "first answer"));
    ops.extend(assistant_step_ops("step-002", "call-002", "second answer"));
    ops.extend(assistant_step_ops("step-003", "call-003", "third answer"));
    write_wire_log(&env.manager, &session_id, &ops).await;

    // When resuming
    let resumed = env
        .manager
        .resume_session_with_limits(&session_id, resume_base_config(), None)
        .await
        .unwrap();

    // Then all three steps are replayed in wire order with no open exchange
    let agent = resumed
        .get_agent(&AgentId::new("main"))
        .await
        .expect("main agent recreated");
    assert_eq!(
        history_texts(&agent),
        vec![
            "start".to_string(),
            "first answer".to_string(),
            "file contents".to_string(),
            "second answer".to_string(),
            "file contents".to_string(),
            "third answer".to_string(),
            "file contents".to_string(),
        ],
        "every replayed step must yield its assistant text and tool result"
    );
    let memory = agent.context_memory();
    let guard = memory.lock().unwrap();
    assert_eq!(guard.history().len(), 7);
    // Token count holds the latest step's reported usage (10 + 5), not a sum.
    assert_eq!(guard.token_count(), 15);
    assert!(!guard.has_open_tool_exchange());
}

#[tokio::test]
async fn should_preserve_metadata_tags_when_session_resumed() {
    // Given a closed session carrying multiple metadata tags
    let env = TestEnv::new().await;
    let session = env
        .manager
        .create_session_with_id(
            SessionId::new("resume-tags"),
            make_metadata("Tagged Session", &["alpha", "beta", "gamma"]),
            SessionScopeConfig::default(),
        )
        .await
        .unwrap();
    let session_id = session.id().clone();
    write_wire_log(
        &env.manager,
        &session_id,
        &[append_message_op("user", "tagged question")],
    )
    .await;
    env.manager
        .close_session(&session_id, CloseReason::Exit)
        .await
        .unwrap();

    // When resuming the closed session
    let resumed = env
        .manager
        .resume_session_with_limits(&session_id, resume_base_config(), None)
        .await
        .unwrap();

    // Then every tag survives the close/resume round trip
    let metadata = resumed.metadata().await;
    assert_eq!(
        metadata.tags.len(),
        3,
        "all tags must be preserved, got: {:?}",
        metadata.tags
    );
    for tag in ["alpha", "beta", "gamma"] {
        assert!(
            metadata.tags.contains(tag),
            "tag {tag:?} must be preserved, got: {:?}",
            metadata.tags
        );
    }
}

#[tokio::test]
async fn should_resume_with_tool_error_result_when_wire_log_has_error() {
    // Given a session whose wire log records an assistant step whose
    // tool.result event is flagged with isError
    let env = TestEnv::new().await;
    let session_id = SessionId::new("resume-tool-error");
    env.manager
        .persistence()
        .save_state(&session_id, &make_metadata("Tool Error", &[]))
        .await
        .unwrap();
    write_wire_log(
        &env.manager,
        &session_id,
        &[
            WireOp::ContextAppendLoopEvent {
                event: json!({"type": "step.begin", "uuid": "step-err", "turnId": "1", "step": 0}),
            },
            WireOp::ContextAppendLoopEvent {
                event: json!({
                    "type": "tool.call",
                    "callId": "call-err",
                    "name": "Bash",
                    "input": {"command": "exit 1"},
                    "uuid": "step-err",
                }),
            },
            WireOp::ContextAppendLoopEvent {
                event: json!({
                    "type": "tool.result",
                    "callId": "call-err",
                    "result": {"output": [{"type": "text", "text": "command failed"}]},
                    "uuid": "step-err",
                    "isError": true,
                }),
            },
            WireOp::ContextAppendLoopEvent {
                event: json!({
                    "type": "step.end",
                    "uuid": "step-err",
                    "turnId": "1",
                    "step": 0,
                    "finishReason": "end_turn",
                    "usage": {"inputTokens": 5, "outputTokens": 2},
                }),
            },
        ],
    )
    .await;

    // When resuming
    let resumed = env
        .manager
        .resume_session_with_limits(&session_id, resume_base_config(), None)
        .await
        .unwrap();

    // Then the replayed tool result keeps its error flag
    let agent = resumed
        .get_agent(&AgentId::new("main"))
        .await
        .expect("main agent recreated");
    let memory = agent.context_memory();
    let guard = memory.lock().unwrap();
    let history = guard.history();
    assert_eq!(history.len(), 2);
    assert_eq!(history[0].message.role, Role::Assistant);
    assert_eq!(history[1].message.role, Role::Tool);
    assert_eq!(history[1].message.tool_call_id.as_deref(), Some("call-err"));
    assert_eq!(history[1].message.text_content(), "command failed");
    assert!(
        history[1].is_error,
        "the isError flag must survive the wire-log replay"
    );
}

// ===========================================================================
// SessionManager — old-format (v1.0) wire logs are migrated on resume (12A)
// ===========================================================================

/// Write a raw wire-log file (bypassing `WireOp` serialisation) so a test
/// can craft old-format records the current kernel no longer writes.
async fn write_raw_wire_log(manager: &SessionManager, session_id: &SessionId, content: &str) {
    let wire_log_path = manager.persistence().wire_log_path(session_id);
    tokio::fs::create_dir_all(wire_log_path.parent().unwrap())
        .await
        .unwrap();
    tokio::fs::write(&wire_log_path, content).await.unwrap();
}

/// The first line of the session's `wire.jsonl`, parsed as JSON.
async fn read_wire_log_header(
    manager: &SessionManager,
    session_id: &SessionId,
) -> serde_json::Value {
    let wire_log_path = manager.persistence().wire_log_path(session_id);
    let content = tokio::fs::read_to_string(&wire_log_path).await.unwrap();
    serde_json::from_str(content.lines().next().unwrap()).unwrap()
}

#[tokio::test]
async fn should_migrate_and_replay_old_wire_log_when_resuming_session_with_v1_0_log() {
    // Given a persisted session whose wire log is written in the v1.0
    // protocol: legacy `op`-tagged records carrying a v1.0 header. Without
    // migration these records do not deserialise as current WireOps and
    // would be lost (or fail the read).
    let env = TestEnv::new().await;
    let session_id = SessionId::new("resume-v1.0-log");
    env.manager
        .persistence()
        .save_state(&session_id, &make_metadata("Old Log", &["legacy"]))
        .await
        .unwrap();
    write_raw_wire_log(
        &env.manager,
        &session_id,
        concat!(
            r#"{"op":"metadata","protocolVersion":"1.0","createdAt":"2024-01-01T00:00:00Z"}"#,
            "\n",
            r#"{"op":"permission.set_mode","mode":"auto","time":"2024-01-01T00:00:01Z"}"#,
            "\n",
            r#"{"op":"context.append_message","message":{"role":"user","content":[{"type":"text","text":"from the old log"}],"toolCalls":[]},"time":"2024-01-01T00:00:02Z"}"#,
            "\n",
        ),
    )
    .await;

    // When resuming the session
    let resumed = env
        .manager
        .resume_session_with_limits(&session_id, resume_base_config(), None)
        .await
        .unwrap();

    // Then every record is migrated and replayed — the message survives
    let agent = resumed
        .get_agent(&AgentId::new("main"))
        .await
        .expect("main agent recreated");
    assert_eq!(
        history_texts(&agent),
        vec!["from the old log".to_string()],
        "migrated v1.0 records must replay into the agent context"
    );

    // And the wire log was rewritten atomically to the current protocol
    let header = read_wire_log_header(&env.manager, &session_id).await;
    assert_eq!(
        header["type"], "metadata",
        "the legacy op discriminator must be rewritten as the current type tag"
    );
    assert_eq!(
        header["protocol_version"], "2.0",
        "the rewritten header must carry the current protocol version"
    );
}

#[tokio::test]
async fn should_still_resume_when_old_log_rewrite_fails_due_to_torn_final_line() {
    // Given a v1.0 wire log whose final line is torn (truncated mid-append,
    // no trailing newline). The on-disk migration rewrite cannot handle the
    // torn line, but the in-memory migration used for replay can — resume
    // must not be blocked by the rewrite failure.
    let env = TestEnv::new().await;
    let session_id = SessionId::new("resume-torn-old-log");
    env.manager
        .persistence()
        .save_state(&session_id, &make_metadata("Torn Old Log", &[]))
        .await
        .unwrap();
    write_raw_wire_log(
        &env.manager,
        &session_id,
        concat!(
            r#"{"op":"metadata","protocolVersion":"1.0","createdAt":"2024-01-01T00:00:00Z"}"#,
            "\n",
            r#"{"op":"permission.set_mode","mode":"auto","time":"2024-01-01T00:00:01Z"}"#,
            "\n",
            r#"{"op":"context.append_message","message":{"role":"user","content":[{"type":"text","text":"before the crash"}],"toolCalls":[]},"time":"2024-01-01T00:00:02Z"}"#,
            "\n",
            r#"{"op":"context.append_message","message":{"role":"us"#,
        ),
    )
    .await;

    // When resuming the session
    let resumed = env
        .manager
        .resume_session_with_limits(&session_id, resume_base_config(), None)
        .await
        .unwrap();

    // Then the complete records replay and the torn line is silently dropped
    let agent = resumed
        .get_agent(&AgentId::new("main"))
        .await
        .expect("main agent recreated");
    assert_eq!(
        history_texts(&agent),
        vec!["before the crash".to_string()],
        "the migrated records before the torn line must replay"
    );
}

// ===========================================================================
// SessionManager — blobref media is rehydrated on resume (12F)
// ===========================================================================

#[tokio::test]
async fn should_rehydrate_blobref_media_when_resuming_session_with_blob_urls_in_log() {
    // Given a persisted session whose wire log references a `blobref:` media
    // URL backed by a real blob in the session's blob store
    let env = TestEnv::new().await;
    let session_id = SessionId::new("resume-blobs");
    env.manager
        .persistence()
        .save_state(&session_id, &make_metadata("Blobs", &["media"]))
        .await
        .unwrap();

    let blob_service = AgentBlobService::new(env.manager.persistence().session_dir(&session_id));
    let blob_ref_url = blob_service
        .write_blob("image/png", "aGVsbG8=")
        .expect("blob write should succeed");

    write_wire_log(
        &env.manager,
        &session_id,
        &[WireOp::ContextAppendMessage {
            message: json!({
                "role": "user",
                "content": [{"type": "image_url", "image_url": {"url": blob_ref_url}}],
                "toolCalls": [],
            }),
        }],
    )
    .await;

    // When resuming the session
    let resumed = env
        .manager
        .resume_session_with_limits(&session_id, resume_base_config(), None)
        .await
        .unwrap();

    // Then the blobref is rehydrated back to its data: URI
    let agent = resumed
        .get_agent(&AgentId::new("main"))
        .await
        .expect("main agent recreated");
    let memory = agent.context_memory();
    let guard = memory.lock().unwrap();
    let history = guard.history();
    assert_eq!(history.len(), 1);
    match &history[0].message.content[0] {
        mirri_core::ContentPart::ImageUrl { image_url, .. } => {
            assert_eq!(
                image_url.url, "data:image/png;base64,aGVsbG8=",
                "the blobref URL must be rehydrated to its data URI"
            );
        }
        other => panic!("expected ImageUrl, got {other:?}"),
    }
}

#[tokio::test]
async fn should_rehydrate_multi_part_file_and_video_blobs_when_resuming_session_with_blob_refs() {
    // Given a persisted session whose wire log records a multi-part user
    // message — text + a video blobref + a file part whose inline data is a
    // blobref — all backed by real blobs in the session's blob store
    let env = TestEnv::new().await;
    let session_id = SessionId::new("resume-blobs-multipart");
    env.manager
        .persistence()
        .save_state(&session_id, &make_metadata("Blobs", &["media"]))
        .await
        .unwrap();

    let blob_service = AgentBlobService::new(env.manager.persistence().session_dir(&session_id));
    let video_blob = blob_service
        .write_blob("video/mp4", "dmlkZW8=")
        .expect("video blob write should succeed");
    let file_blob = blob_service
        .write_blob("application/pdf", "ZmlsZQ==")
        .expect("file blob write should succeed");

    write_wire_log(
        &env.manager,
        &session_id,
        &[WireOp::ContextAppendMessage {
            message: json!({
                "role": "user",
                "content": [
                    { "type": "text", "text": "see attachments" },
                    { "type": "video_url", "video_url": video_blob },
                    { "type": "file", "data": file_blob, "name": "doc.pdf", "media_type": "application/pdf", "size": 5 },
                ],
                "toolCalls": [],
            }),
        }],
    )
    .await;

    // When resuming the session
    let resumed = env
        .manager
        .resume_session_with_limits(&session_id, resume_base_config(), None)
        .await
        .unwrap();

    // Then every media part is rehydrated back to its data: URI and the
    // text part survives untouched
    let agent = resumed
        .get_agent(&AgentId::new("main"))
        .await
        .expect("main agent recreated");
    let memory = agent.context_memory();
    let guard = memory.lock().unwrap();
    let history = guard.history();
    assert_eq!(history.len(), 1);
    let parts = &history[0].message.content;
    assert_eq!(parts.len(), 3, "all three parts survive: {parts:?}");
    match &parts[0] {
        mirri_core::ContentPart::Text { text } => {
            assert_eq!(text, "see attachments");
        }
        other => panic!("expected Text, got {other:?}"),
    }
    match &parts[1] {
        mirri_core::ContentPart::VideoUrl { video_url } => {
            assert_eq!(
                video_url, "data:video/mp4;base64,dmlkZW8=",
                "the video blobref must be rehydrated to its data URI"
            );
        }
        other => panic!("expected VideoUrl, got {other:?}"),
    }
    match &parts[2] {
        mirri_core::ContentPart::File {
            file_id,
            data,
            name,
            media_type,
            size,
        } => {
            assert_eq!(file_id, &None, "no bare files-store reference in memory");
            assert_eq!(
                data.as_deref(),
                Some("data:application/pdf;base64,ZmlsZQ=="),
                "the file blobref must be rehydrated to its data URI"
            );
            assert_eq!(name.as_deref(), Some("doc.pdf"));
            assert_eq!(media_type.as_deref(), Some("application/pdf"));
            assert_eq!(*size, Some(5));
        }
        other => panic!("expected File, got {other:?}"),
    }
}

// ===========================================================================
// SessionManager — cron tasks are restored on resume (P2b)
// ===========================================================================

/// A `cron.add` op carrying a task in the kernel's `CronTask` serde shape.
/// `next_fire_at` is deliberately stale (long past) so tests can observe that
/// restore recomputes it instead of trusting the persisted timestamp.
fn cron_add_op(id: &str, cron: &str, recurring: bool) -> WireOp {
    WireOp::CronAdd {
        task: json!({
            "id": id,
            "cron": cron,
            "prompt": "tick",
            "session_id": "resume-cron",
            "recurring": recurring,
            "created_at": "2020-01-01T00:00:00Z",
            "last_fired": null,
            "next_fire_at": "2020-01-01T00:01:00Z",
        }),
    }
}

#[tokio::test]
async fn should_restore_cron_tasks_with_recomputed_next_fire_when_session_with_cron_ops_resumed() {
    use chrono::TimeZone;
    // Given a persisted session whose wire log registered two cron tasks
    // (both with stale, long-past next fire times) and deleted one
    let env = TestEnv::new().await;
    let session_id = SessionId::new("resume-cron-active");
    env.manager
        .persistence()
        .save_state(&session_id, &make_metadata("Cron", &["cron"]))
        .await
        .unwrap();
    write_wire_log(
        &env.manager,
        &session_id,
        &[
            WireOp::Metadata {
                protocol_version: Some("1.5".to_string()),
                created_at: Some(1_700_000_000_000),
            },
            cron_add_op("cron-1", "*/5 * * * *", true),
            cron_add_op("cron-2", "0 9 * * *", true),
            WireOp::CronDelete {
                ids: vec!["cron-2".to_string()],
            },
        ],
    )
    .await;
    // When resuming the session
    let resumed = env
        .manager
        .resume_session_with_limits(&session_id, resume_base_config(), None)
        .await
        .unwrap();

    // Then the surviving task is restored with a next fire time rebuilt
    // from the record's cursor (created_at — the TS baseFromMs), never
    // from the persisted absolute timestamp: it is the first */5
    // occurrence after 2020-01-01T00:00:00Z plus the deterministic
    // per-id jitter (<= 30s). A schedule in the past stays due — the
    // next check_due delivers the missed periods as one coalesced fire
    // (the TS catch-up semantics; B5).
    let cron = resumed.cron();
    let tasks = cron.list();
    assert_eq!(tasks.len(), 1, "deleted task stays deleted: {tasks:?}");
    assert_eq!(tasks[0].id, "cron-1");
    let next = tasks[0]
        .next_fire_at
        .expect("next fire is recomputed on restore");
    let base = chrono::Utc.with_ymd_and_hms(2020, 1, 1, 0, 0, 0).unwrap();
    let persisted = chrono::Utc.with_ymd_and_hms(2020, 1, 1, 0, 1, 0).unwrap();
    assert!(
        next != persisted,
        "stale persisted next_fire_at must be recomputed: {next}"
    );
    assert!(next > base, "the walk starts at the cursor: {next}");
    assert!(
        next <= base + chrono::Duration::minutes(5) + chrono::Duration::seconds(31),
        "the first */5 occurrence after the cursor (+jitter): {next}"
    );
}

#[tokio::test]
async fn should_not_refire_a_fired_one_shot_when_session_with_cron_cursor_resumed() {
    // Given a persisted session whose wire log registered a one-shot task
    // and recorded its fire cursor before the restart
    let env = TestEnv::new().await;
    let session_id = SessionId::new("resume-cron-fired");
    env.manager
        .persistence()
        .save_state(&session_id, &make_metadata("Cron", &["cron"]))
        .await
        .unwrap();
    write_wire_log(
        &env.manager,
        &session_id,
        &[
            cron_add_op("cron-1", "*/5 * * * *", false),
            WireOp::CronCursor {
                id: "cron-1".to_string(),
                last_fired_at: chrono::Utc::now().timestamp_millis() as u64,
            },
        ],
    )
    .await;

    // When resuming the session
    let resumed = env
        .manager
        .resume_session_with_limits(&session_id, resume_base_config(), None)
        .await
        .unwrap();

    // Then the fired one-shot stays listed with its last_fired preserved but
    // never fires again
    let cron = resumed.cron();
    let task = cron.get("cron-1").expect("fired one-shot stays listed");
    assert!(
        task.last_fired.is_some(),
        "the cursor's last_fired is preserved"
    );
    assert!(
        task.next_fire_at.is_none(),
        "a fired one-shot is never rescheduled"
    );
    assert!(
        cron.check_due().await.is_empty(),
        "a fired one-shot must not re-fire after resume"
    );
}

#[tokio::test]
async fn should_keep_resuming_when_cron_task_record_is_unparseable() {
    // Given a persisted session whose wire log holds one garbage cron task
    // record next to a good one
    let env = TestEnv::new().await;
    let session_id = SessionId::new("resume-cron-bad-record");
    env.manager
        .persistence()
        .save_state(&session_id, &make_metadata("Cron", &["cron"]))
        .await
        .unwrap();
    write_wire_log(
        &env.manager,
        &session_id,
        &[
            WireOp::CronAdd {
                task: json!({ "id": 123, "garbage": true }),
            },
            cron_add_op("cron-good", "*/5 * * * *", true),
        ],
    )
    .await;

    // When resuming the session, the bad record is skipped without failing
    // the resume and the good task is restored
    let resumed = env
        .manager
        .resume_session_with_limits(&session_id, resume_base_config(), None)
        .await
        .unwrap();

    let tasks = resumed.cron().list();
    assert_eq!(tasks.len(), 1, "only the parseable task is restored");
    assert_eq!(tasks[0].id, "cron-good");
}

// ===========================================================================
// Detached background task across close→resume (F-P3-3, engine-managed
// stance: tasks do not resume running; the record and its output log
// survive on disk and stay readable)
// ===========================================================================

/// Given a session with a detached background shell task that has produced
/// output, when the session is closed and resumed, then the task record is
/// restored from `<session_dir>/tasks/` (stopped — never resumed running)
/// and its accumulated output is readable through the resumed scope.
#[tokio::test]
async fn should_restore_detached_task_record_and_output_when_session_is_resumed() {
    // Given a session whose detached run keeps producing output
    let env = TestEnv::new().await;
    let session = env
        .manager
        .create_session_with_id(
            SessionId::new("resume-detached-task"),
            make_metadata("Detached", &["task"]),
            SessionScopeConfig::default(),
        )
        .await
        .unwrap();
    let session_id = session.id().clone();

    // A tiny foreground window forces the run to detach immediately; the
    // marker prints after the detach point so it lands on the background
    // task (output within the foreground window stays in the result's
    // stdout), then the command outlives the test's close.
    let result = run_shell_command(
        ShellRunTarget {
            session_id: session_id.as_str().to_string(),
            registry: std::sync::Arc::new(ShellCommandRegistry::new()),
            tasks: session.tasks(),
            event_bus: std::sync::Arc::new(EventBus::new(64)),
            process: std::sync::Arc::new(mirri_os::LocalProcess::new()),
        },
        "sleep 1 && echo detached-marker && sleep 30",
        None,
        std::time::Duration::from_millis(200),
        |_| {},
    )
    .await;
    let task_id = result
        .backgrounded
        .expect("the run must detach to a background task");

    // Wait for the marker to land on the task (and thus on disk).
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let output = session
            .tasks()
            .output(&task_id)
            .expect("task output while running");
        if output.full_output.contains("detached-marker") {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "detached output never landed, was: {:?}",
            output.full_output
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    // When the session is closed and resumed
    env.manager
        .close_session(&session_id, CloseReason::Exit)
        .await
        .unwrap();
    let resumed = env
        .manager
        .resume_session_with_limits(&session_id, resume_base_config(), None)
        .await
        .unwrap();

    // Then the task record survived on disk: present, and stopped — the
    // engine-managed stance (a running task never resumes as running).
    let task = resumed
        .tasks()
        .get(&task_id)
        .expect("the detached task record must be restored on resume");
    assert_eq!(
        task.status,
        TaskStatus::Stopped,
        "close marks running tasks stopped; resume must not resurrect them"
    );

    // And its accumulated output is readable through the resumed scope
    // (served from the persisted output.log).
    let output = resumed
        .tasks()
        .output(&task_id)
        .expect("restored task output must be readable");
    assert!(
        output.full_output.contains("detached-marker"),
        "restored output must carry the pre-close marker, was: {}",
        output.full_output
    );
}

// ===========================================================================
// SessionManager — resume restores the todo list from the folded tool store
// ===========================================================================

/// A `tools.update_store` (key "todo") op carrying `items`.
fn todo_store_op(items: serde_json::Value) -> WireOp {
    WireOp::ToolsUpdateStore {
        key: "todo".to_string(),
        value: items,
    }
}

/// Given a closed session whose wire log records one `tools.update_store`
/// (todo) op with two items, when the session is resumed, then the resumed
/// session's todo list holds both items with their statuses preserved.
#[tokio::test]
async fn should_restore_the_todo_list_when_resuming_a_session() {
    let env = TestEnv::new().await;
    let session = env
        .manager
        .create_session_with_id(
            SessionId::new("resume-todo"),
            make_metadata("Todo Session", &["todo"]),
            SessionScopeConfig::default(),
        )
        .await
        .unwrap();
    let session_id = session.id().clone();
    write_wire_log(
        &env.manager,
        &session_id,
        &[
            append_message_op("user", "plan the work"),
            todo_store_op(json!([
                {"title": "Write tests", "status": "pending"},
                {"title": "Ship it", "status": "in_progress"},
            ])),
        ],
    )
    .await;
    env.manager
        .close_session(&session_id, CloseReason::Exit)
        .await
        .unwrap();

    // When resuming
    let resumed = env
        .manager
        .resume_session_with_limits(&session_id, resume_base_config(), None)
        .await
        .unwrap();

    // Then the session-scoped todo list carries both recorded items
    let todos = resumed.todo_list().get();
    assert_eq!(todos.len(), 2, "both recorded items must be restored");
    assert_eq!(todos[0].title, "Write tests");
    assert_eq!(todos[0].status, TodoStatus::Pending);
    assert_eq!(todos[1].title, "Ship it");
    assert_eq!(todos[1].status, TodoStatus::InProgress);
}

/// Given a closed session whose wire log records a `tools.update_store`
/// (todo) op with capitalized statuses (`Pending` / `IN_PROGRESS`), when the
/// session is resumed, then the items round-trip to the canonical statuses.
#[tokio::test]
async fn should_restore_capitalized_statuses_when_resuming_a_session() {
    let env = TestEnv::new().await;
    let session = env
        .manager
        .create_session_with_id(
            SessionId::new("resume-todo-caps"),
            make_metadata("Todo Caps", &[]),
            SessionScopeConfig::default(),
        )
        .await
        .unwrap();
    let session_id = session.id().clone();
    write_wire_log(
        &env.manager,
        &session_id,
        &[todo_store_op(json!([
            {"title": "Cap pending", "status": "Pending"},
            {"title": "Cap in progress", "status": "IN_PROGRESS"},
            {"title": "Cap done", "status": "Done"},
        ]))],
    )
    .await;
    env.manager
        .close_session(&session_id, CloseReason::Exit)
        .await
        .unwrap();

    let resumed = env
        .manager
        .resume_session_with_limits(&session_id, resume_base_config(), None)
        .await
        .unwrap();

    let todos = resumed.todo_list().get();
    assert_eq!(todos.len(), 3);
    assert_eq!(todos[0].status, TodoStatus::Pending);
    assert_eq!(todos[1].status, TodoStatus::InProgress);
    assert_eq!(todos[2].status, TodoStatus::Done);
}

/// Given a closed session whose wire log records a valid todo store op
/// followed by a malformed one, when the session is resumed, then the
/// malformed op was skipped by the reducer and the first op's list survives.
#[tokio::test]
async fn should_keep_the_first_todo_list_when_a_later_store_op_is_malformed() {
    let env = TestEnv::new().await;
    let session = env
        .manager
        .create_session_with_id(
            SessionId::new("resume-todo-malformed"),
            make_metadata("Todo Malformed", &[]),
            SessionScopeConfig::default(),
        )
        .await
        .unwrap();
    let session_id = session.id().clone();
    write_wire_log(
        &env.manager,
        &session_id,
        &[
            todo_store_op(json!([
                {"title": "Good item", "status": "pending"},
            ])),
            // A bad status never folds: the reducer's strict "todo" gate
            // skips the whole op rather than corrupting the stored list.
            todo_store_op(json!([
                {"title": "Bad item", "status": "blocked"},
            ])),
        ],
    )
    .await;
    env.manager
        .close_session(&session_id, CloseReason::Exit)
        .await
        .unwrap();

    let resumed = env
        .manager
        .resume_session_with_limits(&session_id, resume_base_config(), None)
        .await
        .unwrap();

    let todos = resumed.todo_list().get();
    assert_eq!(
        todos,
        vec![TodoItem {
            title: "Good item".to_string(),
            status: TodoStatus::Pending,
        }],
        "the malformed op must be skipped; the first op's list survives"
    );
}

/// Given a closed session whose wire log records [A] → a real-user anchor →
/// [B], when the session is resumed and one turn is undone through the live
/// `session.undo` count semantics, then the live todo list rolls back to [A]
/// — the resume rebuilt the undo checkpoint stack from the folded state.
#[tokio::test]
async fn should_restore_the_todo_checkpoints_when_resuming_a_session() {
    let env = TestEnv::new().await;
    let session = env
        .manager
        .create_session_with_id(
            SessionId::new("resume-todo-checkpoints"),
            make_metadata("Todo Checkpoints", &[]),
            SessionScopeConfig::default(),
        )
        .await
        .unwrap();
    let session_id = session.id().clone();
    write_wire_log(
        &env.manager,
        &session_id,
        &[
            todo_store_op(json!([
                {"title": "Before anchor", "status": "pending"},
            ])),
            // A real-user-input undo anchor: the fold snapshots the current
            // store value ([Before anchor]) onto todo_checkpoints.
            append_message_op("user", "go"),
            todo_store_op(json!([
                {"title": "After anchor", "status": "in_progress"},
            ])),
        ],
    )
    .await;
    env.manager
        .close_session(&session_id, CloseReason::Exit)
        .await
        .unwrap();

    let resumed = env
        .manager
        .resume_session_with_limits(&session_id, resume_base_config(), None)
        .await
        .unwrap();

    // The live list starts at the folded store value…
    let todos = resumed.todo_list().get();
    assert_eq!(todos.len(), 1);
    assert_eq!(todos[0].title, "After anchor");

    // …and the checkpoint stack was rebuilt: popping one depth (the
    // memory-side mirror of `context.undo` count 1) restores [Before
    // anchor].
    let applied = resumed.todo_list().pop_checkpoint(1);
    assert_eq!(applied, 1, "one checkpoint must have been restored");
    let rolled_back = resumed.todo_list().get();
    assert_eq!(
        rolled_back,
        vec![TodoItem {
            title: "Before anchor".to_string(),
            status: TodoStatus::Pending,
        }],
        "the popped checkpoint's list must be live again"
    );
}
