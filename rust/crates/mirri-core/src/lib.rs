//! `mirri-core` — the engine layer for the Rust kernel rewrite.
//!
//! This crate holds the core traits and types that all other Phase 2 modules
//! depend on:
//!
//! - [`error`] — `EngineError`, `ErrorCode`, `EngineResult`.
//! - [`cancel`] — `CancelToken`, the `AbortSignal` equivalent.
//! - [`message`] — wire-level `Message` and content-part types.
//! - [`origin`] — `PromptOrigin` provenance tag (16 variants).
//! - [`panic_capture`] — thread-local panic location capture so
//!   `catch_unwind` surfaces can append "(panic at file:line:col)".
//! - [`context`] — `ContextMessage`, projection options, context snapshot.
//! - [`context_memory`] — `ContextMemory`, the canonical history store with
//!   projection pipeline.
//! - [`tool`] — the two-phase `Tool` trait and execution types.
//! - [`tool_registry`] — `ToolManager` (registry), `ToolScheduler` (concurrent
//!   execution), argument validation, and the full execution pipeline.
//! - [`llm`] — the `Llm` trait and streaming event types.
//! - [`hooks`] — `LoopHooks`, the host injection surface.
//! - [`cron`] — per-session cron scheduling (`CronManager`, 5-field parser).
//! - [`task`] — per-session background task tracking (`TaskManager`).
//! - [`task_persist`] — filesystem persistence for background tasks
//!   (`TaskPersistence`).
//! - [`skill`] — workspace-scoped `SKILL.md` discovery (`SkillCatalog`).
//! - [`flags`] — env-driven feature flags (`FlagRegistry`).
//! - [`todo`] — session-scoped todo list service (`TodoList`).
//! - [`goal`] — session-scoped goal tracking (`GoalManager`).
//! - [`plan`] — session-scoped plan-mode management (`PlanManager`).
//! - [`profile`] — agent profiles (`AgentProfile`, `ProfileManager`).
//! - [`config`] — `ServerConfig`, TOML config with env overrides.
//! - [`tools`] — the built-in tool set (Bash, Read, Write, Edit, Glob, Grep,
//!   TodoList, TaskList, TaskStop, TaskOutput) and registration.
//! - [`tool_result_truncation`] — oversized tool result truncation with
//!   agent-scoped storage and recoverable preview.
//! - [`capability`] — builtin tool capability registration and
//!   `capabilitiesRequired` resolution.
//! - [`context_size`] — measured vs estimated token counting with range
//!   queries.
//! - [`interruption_reminder`] — user-interruption reminder injection after
//!   cancelled turns.
//! - [`activity_view`] — per-agent read-only activity projection (turn phase,
//!   tool calls, retries, approvals).
//! - [`session_activity`] — session-level aggregated activity view (busy,
//!   mainTurnActive, pendingInteraction).
//! - [`subagent`] — subagent task system (SubagentHandle, SubagentTask).
//! - [`session_export`] — session diagnostic export to zip with manifest.
//! - [`blob`] — content-addressed blob storage for media attachments
//!   with LRU cache and blobref protocol.
//! - [`prompt`] — prompt lifecycle queue with dedup, completion tracking,
//!   and snapshot.
//! - [`redact`] — log redaction utilities (`redact_json`, `redact_string`,
//!   `redact_provider_config`) for stripping secrets from logs and output.
//! - [`swarm`] — swarm mode service for multi-agent orchestration.
//! - [`usage`] — per-agent token usage accounting with budget tracking.
//! - [`capability_gap`] — model capability gap detection and subagent
//!   dispatch hints.
//! - [`undo_service`] — conversation undo availability with idle gating.
//! - [`tool_policy`] — agent-scope tool authorization policy.
//! - [`tool_select`] — progressive tool disclosure and announcements.
//! - [`message_pipeline`] — pluggable post-projection message processing
//!   pipeline.
//! - [`media_placeholder`] — unified media placeholder constructor
//!   (`MediaKind`, `PlaceholderCause`, `build_media_placeholder`,
//!   `is_archive_path`).
//! - [`media_ingest`] — multimodal entry pipeline: archive prompt media
//!   originals into the content-addressed blob store.

pub mod activity_view;
pub mod agents_md;
pub mod blob;
pub mod blob_ledger;
pub mod cancel;
pub mod capability;
pub mod capability_gap;
pub mod compaction;
pub mod config;
pub mod context;
pub mod context_memory;
pub mod context_size;
pub mod cron;
pub mod error;
pub mod flags;
pub mod goal;
pub mod hook_engine;
pub mod hooks;
pub mod image_pipeline;
pub mod interruption_reminder;
pub mod llm;
pub mod llm_recorder;
pub mod loop_engine;
pub mod media_ingest;
pub mod media_placeholder;
pub mod message;
pub mod message_pipeline;
pub mod model_catalog;
pub mod origin;
pub mod orphan_sweep;
pub mod panic_capture;
pub mod permission;
pub mod plan;
pub mod plugin_runtime;
pub mod profile;
pub mod prompt;
pub mod redact;
pub mod reverse_rpc;
pub mod scope;
pub mod session;
pub mod session_activity;
pub mod session_export;
pub mod shell_command;
pub mod skill;
pub mod skill_reference;
pub mod subagent;
pub mod swarm;
pub mod system_reminder_wire;
pub mod task;
pub mod task_notification;
pub mod task_persist;
pub mod telemetry;
pub mod terminal_pool;
pub mod todo;
pub mod tool;
pub mod tool_policy;
pub mod tool_registry;
pub mod tool_result_truncation;
pub mod tool_select;
pub mod tools;
pub mod turn_flow;
pub mod undo_service;
pub mod usage;

pub use activity_view::{
    ActivityLastTurnState, ActivityRetryState, ActivityTurnState, ActivityViewLifecycle,
    AgentActivityState, AgentActivityView, ApprovalRef, BackgroundRef, EndingReason, StreamKind,
    ToolCallRef, TurnEndReason, TurnPhase,
};
pub use blob::{
    AgentBlobService, BLOBREF_PROTOCOL, BlobRef, ByteLruCache, MISSING_MEDIA_PLACEHOLDER,
};
pub use cancel::CancelToken;
pub use capability::{
    CapabilityProvider, CapabilityRegistry, CapabilityResolver, IntegrationSpec,
    IntegrationsConfig, Source as CapabilitySource,
};
pub use capability_gap::{
    CapabilityGapDetector, ModalityGap, ModelCapabilities as GapModelCapabilities, ProfileInfo,
};
pub use compaction::{
    CompactionBeginParams, CompactionResult, CompactionRoundInput, CompactionSource,
    CompactionStrategy, FullCompaction, ManualCompactionHandle, compaction_round,
    hook_additional_context_messages,
};
pub use config::{
    LoopControlConfig, ModelAlias, ModelConfig, PermissionConfig, ProviderConfig, ProviderEntry,
    RetentionConfig, ServerConfig, ThinkingConfig, camel_to_snake, convert_keys_to_camel_case,
    convert_keys_to_snake_case, json_to_toml_value, load_retention, migrate_config, mirri_home_dir,
    resolve_model_alias_in, snake_to_camel, toml_to_json,
};
pub use context::{AgentContextData, ContextMessage, ProjectOptions, ProjectionAnomaly};
pub use context_memory::{
    CompactionInput, ContextMemory, LoopEvent, project, select_compaction_user_messages,
};
pub use context_size::{ContextSize, ContextSizeTracker, normalize_slice_index};
pub use cron::{CronError, CronManager, CronTask};
pub use error::{EngineError, EngineResult, ErrorCode};
pub use flags::{
    FLAG_GLOBAL_ENV, FlagDef, FlagRegistry, FlagSource, FlagState, FlagSurface,
    HOOK_COMMAND_REWRITE_FLAG, SESSION_CONSISTENCY_FOLLOW_FLAG, env_flag_enabled,
};
pub use goal::{
    Goal, GoalBudget, GoalManager, GoalStatus, goal_snapshot_wire, goal_status_snapshot_wire,
    goal_status_wire,
};
pub use hook_engine::{
    ExternalHooksBridge, HookBlockSource, HookDef, HookEngine, HookEventType, HookExecutionResult,
    HookOutcome, HookSpecificOutput, HookStructuredOutput, HookTriggerResult, OrderedHookSlot,
    SessionHookCallback, SessionHookSlot, SessionLifecycleHooks, matches, run_hook,
};
pub use hooks::*;
pub use interruption_reminder::InterruptionReminder;
pub use llm::*;
pub use llm_recorder::{LLM_RECORD_ENV, LLM_RECORD_FILE_NAME, LlmRecord, LlmRecorder};
pub use loop_engine::{
    ChatResult, LiveEvent, LiveEventCallback, MessageBuilders, RecordStepUsageFn, RunTurnConfig,
    StepResult, StepRetryConfig, StepRetryDecision, StepRetryService, StepRetrySnapshot,
    StepRetryState, StepStopReason, TurnResult, TurnStepDeps, TurnStopReason, WireOpRecorder,
    chat_with_retry, execute_loop_step, is_retryable_error, run_turn,
};
pub use media_ingest::ingest_prompt_media;
pub use media_placeholder::{
    MediaKind, PlaceholderCause, build_media_placeholder, is_archive_path,
};
pub use message::*;
pub use message_pipeline::{
    MediaCapabilityAdapter, MessagePipeline, ProjectionStep, VideoReferenceResolver,
};
pub use model_catalog::{
    ModelCatalog, ModelCatalogEntry, max_context_tokens_for, model_entries_from_config,
    render_model_listing,
};
pub use origin::PromptOrigin;
pub use orphan_sweep::{SweepEntry, SweepReport, SweepVerdict, sweep_orphans};
pub use permission::{
    OUTSIDE_SANDBOX_DENIAL_CONTRACT, ParsedPattern, PermissionManager, PermissionMode,
    PermissionPolicy, PermissionPolicyContext, PermissionPolicyResult, PermissionRule,
    PermissionServices, RuleDecision, RuleScope, match_permission_rule, parse_pattern,
};
pub use plan::{PlanManager, PlanState};
pub use profile::{
    AgentProfile, ESSENTIAL_OVERRIDE_SKIP_REASON, ESSENTIAL_PROFILE_NAME, ProfileManager,
    ThinkingEffort, subagent_allowlist_for, subagent_type_not_allowed_message,
};
pub use prompt::{
    PromptCompletion, PromptCompletionState, PromptInput, PromptQueue, PromptQueueSnapshot,
    PromptSnapshot, PromptState,
};
pub use redact::{redact_json, redact_provider_config, redact_string};
pub use reverse_rpc::ReverseRpcCallback;
pub use scope::{
    AgentId, AgentScope, AgentScopeConfig, AppScope, AppScopeConfig, LifecycleScope, ProviderType,
    ScopeTree, SessionId, SessionMetadata, SessionScope, SessionScopeConfig, SessionWireLog,
    SkillRootChannel, UsageInitialState, WorkspaceId, WorkspaceScope, WorkspaceScopeConfig,
    WorkspaceTrustLevel, dedupe_skill_roots, default_skill_root_channels,
};
pub use session::event_projection;
pub use session::{
    AdditionalDirGrantSink, AgentInfo, AgentLifecycle, ApprovalDecision, ApprovalGate,
    ApprovalSnapshot, CloseReason, EventBus, ForkOptions, InteractionKernel,
    InteractionRespondError, PersistedSessionInfo, QuestionGate, QuestionSnapshot, ResolvedRing,
    ServerEvent, SessionArtifacts, SessionError, SessionEvent, SessionInfo, SessionManager,
    SessionPersistence, SessionResult, SessionShutdown, SessionSource, SessionStatus,
    WorkspaceSnapshot, effective_compressed, encode_work_dir_key, lexically_resolve,
    project_server_event, session_scope_config_from, uuid_from_id,
};
pub use session_activity::{
    SessionActivityCause, SessionActivityChangedEvent, SessionActivityState, SessionActivityView,
    SessionPendingInteraction, SessionTurnOutcome,
};
pub use session_export::{
    ExportSessionManifest, ExportSessionOptions, ExportSessionPayload, ExportSessionResult,
    SessionExportError, SessionExporter, ShellEnvironment,
};
pub use skill::{
    SKILL_MD_FILE, SkillCatalog, SkillCatalogDiff, SkillDef, SkillError, SkillSource,
    diff_skill_catalogs,
};
pub use skill_reference::{
    InlineRef, InlineReferenceKind, ParsedInlineReferences, collapse_inline_whitespace,
    format_skill_reference_link, parse_inline_references,
};
pub use subagent::{
    NotificationRunGuard, ResumeRequest, SessionSubagentHost, SpawnRequest, SubagentCompletion,
    SubagentHandle, SubagentHost, SubagentLlmFactory, SubagentTask, SubagentTaskInfo, SuspendCause,
    SuspensionNotice, SuspensionNotify, TaskSink,
};
pub use swarm::{SwarmModeService, SwarmModeTrigger};
pub use task::{
    AgentTask, BackgroundTaskInfo, MAX_TASK_OUTPUT_BYTES, TASK_OUTPUT_PREVIEW_CHARS, TaskKind,
    TaskManager, TaskOutput, TaskOutputSnapshot, TaskStatus,
};
pub use task_persist::TaskPersistence;
pub use telemetry::{
    CollectingSink, LoggingSink, NoopSink, TelemetryContext, TelemetryEvent, TelemetryService,
    TelemetrySink, TelemetryValue,
};
pub use terminal_pool::{
    DEFAULT_BUFFER_SIZE, DEFAULT_MAX_TERMINALS, SinkHandle, TerminalFrame, TerminalInfo,
    TerminalPool, TerminalStatus,
};
pub use todo::{TodoItem, TodoList, TodoStatus, render_todo_list};
pub use tool::*;
pub use tool_policy::{AgentToolPolicyService, ToolActivationPolicy, ToolSource};
pub use tool_registry::{
    BlockedToolCall, DispatchedToolCall, McpToolEntry, PendingToolTask, PreflightResult,
    PrepareResult, PreparedToolCall, RejectedToolCall, SyntheticToolCall, ToolManager,
    ValidatedToolCall, execute_tool_batch, preflight_tool_call, prepare_tool_call, run_tool_batch,
    schedule_and_execute,
};
pub use tool_result_truncation::{
    MAX_CHARS as TRUNCATION_MAX_CHARS, PREVIEW_CHARS as TRUNCATION_PREVIEW_CHARS,
    ToolResultTruncator,
};
pub use tool_select::{DisclosureState, ToolSelectConfig, ToolSelectService};
pub use tools::{
    BashTool, BuiltinToolServices, EditTool, GlobTool, GrepTool, ReadTool, TaskListTool,
    TaskOutputTool, TaskStopTool, TodoListTool, WorkspacePathError, WriteTool,
    register_builtin_tools, validate_workspace_path,
};
pub use turn_flow::{
    BufferedSteer, CapabilityGapInjector, CompositeLoopHooks, InjectionManager, Injector,
    OwnerVerdict, PluginSessionStartInjector, SteerBuffer, ToolCallDeduplicator, TurnEndResult,
    TurnFlow, TurnFlowConfig, TurnFlowHooks, TurnFlowHooksConfig, TurnWaitState, WaitGuard,
    WaitKind, WaitRegistry, WaitSubject,
};
pub use undo_service::{ConversationUndoService, UndoAvailability, UndoResult};
pub use usage::{AgentUsageService, TokenUsage, UsageRecordedContext, UsageStatus};
