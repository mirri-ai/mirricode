//! Telemetry system — typed event catalog, sink trait, and service.
//!
//! This module provides the infrastructure for emitting structured telemetry
//! events from across the kernel. It mirrors the TypeScript v2 design:
//!
//! - [`TelemetryEvent`] — a typed enum covering all event categories (turn
//!   lifecycle, tool calls, permissions, goals, cron, session, etc.).
//! - [`TelemetrySink`] — the appender trait; implementations decide where
//!   events go (console, cloud, file, …).
//! - [`TelemetryService`] — the central service that fans out events to
//!   registered sinks, manages enable/disable state, and carries ambient
//!   context (device id, session id, agent id).
//!
//! # Design principles
//!
//! - Events are **typed** at the Rust level — no stringly-typed names.
//! - Sinks are **infallible** — a sink that errors logs internally but never
//!   panics the caller.
//! - The service is **cheaply cloneable** behind `Arc` for sharing across
//!   async tasks.

use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};

// ---------------------------------------------------------------------------
// TelemetryValue — the primitive types allowed in event properties
// ---------------------------------------------------------------------------

/// A value that can appear in a telemetry event property.
#[derive(Debug, Clone, PartialEq)]
pub enum TelemetryValue {
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    Null,
}

impl From<bool> for TelemetryValue {
    fn from(v: bool) -> Self {
        Self::Bool(v)
    }
}

impl From<i64> for TelemetryValue {
    fn from(v: i64) -> Self {
        Self::Int(v)
    }
}

impl From<i32> for TelemetryValue {
    fn from(v: i32) -> Self {
        Self::Int(v as i64)
    }
}

impl From<u64> for TelemetryValue {
    fn from(v: u64) -> Self {
        Self::Int(v as i64)
    }
}

impl From<usize> for TelemetryValue {
    fn from(v: usize) -> Self {
        Self::Int(v as i64)
    }
}

impl From<f64> for TelemetryValue {
    fn from(v: f64) -> Self {
        Self::Float(v)
    }
}

impl From<&str> for TelemetryValue {
    fn from(v: &str) -> Self {
        Self::Str(v.to_string())
    }
}

impl From<String> for TelemetryValue {
    fn from(v: String) -> Self {
        Self::Str(v)
    }
}

impl From<Option<String>> for TelemetryValue {
    fn from(v: Option<String>) -> Self {
        match v {
            Some(s) => Self::Str(s),
            None => Self::Null,
        }
    }
}

// Reference variants — needed because `to_properties` matches through `&self`.
impl From<&bool> for TelemetryValue {
    fn from(v: &bool) -> Self {
        Self::Bool(*v)
    }
}

impl From<&i64> for TelemetryValue {
    fn from(v: &i64) -> Self {
        Self::Int(*v)
    }
}

impl From<&i32> for TelemetryValue {
    fn from(v: &i32) -> Self {
        Self::Int(*v as i64)
    }
}

impl From<&u64> for TelemetryValue {
    fn from(v: &u64) -> Self {
        Self::Int(*v as i64)
    }
}

impl From<&u32> for TelemetryValue {
    fn from(v: &u32) -> Self {
        Self::Int(*v as i64)
    }
}

impl From<&usize> for TelemetryValue {
    fn from(v: &usize) -> Self {
        Self::Int(*v as i64)
    }
}

impl From<&f64> for TelemetryValue {
    fn from(v: &f64) -> Self {
        Self::Float(*v)
    }
}

impl fmt::Display for TelemetryValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Bool(b) => write!(f, "{b}"),
            Self::Int(i) => write!(f, "{i}"),
            Self::Float(n) => write!(f, "{n}"),
            Self::Str(s) => write!(f, "{s}"),
            Self::Null => write!(f, "null"),
        }
    }
}

/// A properties map for telemetry events.
pub type TelemetryProperties = HashMap<String, TelemetryValue>;

// ---------------------------------------------------------------------------
// TelemetryEvent — the typed event catalog
// ---------------------------------------------------------------------------

/// Ambient context injected into every event by the service.
#[derive(Debug, Clone, Default)]
pub struct TelemetryContext {
    pub device_id: Option<String>,
    pub session_id: Option<String>,
    pub agent_id: Option<String>,
}

/// A single telemetry event.
///
/// Each variant carries a properties struct specific to that event type,
/// making the payload compile-time checked at the call site.
#[derive(Debug, Clone)]
pub enum TelemetryEvent {
    // -- Turn lifecycle --
    TurnStarted {
        turn_index: u64,
    },
    TurnInterrupted {
        turn_index: u64,
        reason: String,
    },
    TurnEnded {
        turn_index: u64,
        duration_ms: u64,
    },

    // -- Tool calls --
    ToolCall {
        tool_name: String,
        success: bool,
        duration_ms: u64,
    },
    ToolCallDedupDetected {
        tool_name: String,
        step: u64,
    },
    ToolCallRepeat {
        tool_name: String,
        repeat_count: u32,
    },
    ToolExchangeAbandoned {
        tool_name: String,
        attempt_count: u32,
    },

    // -- API / errors --
    ApiError {
        provider: String,
        error_kind: String,
        status_code: Option<u16>,
    },

    // -- Skill / flow --
    SkillInvoked {
        skill_name: String,
    },
    FlowInvoked {
        flow_name: String,
    },

    // -- Input control --
    InputSteer {
        target_turn: u64,
    },
    Cancel {
        turn_index: u64,
    },
    ConversationUndo {
        steps_undone: u32,
    },

    // -- Toggles --
    YoloToggle {
        enabled: bool,
    },
    AfkToggle {
        enabled: bool,
    },
    ModelSwitch {
        from_model: String,
        to_model: String,
    },
    ThinkingToggle {
        enabled: bool,
    },

    // -- Permission --
    PermissionPolicyDecision {
        tool_name: String,
        decision: String,
        policy_name: String,
    },
    PermissionApprovalResult {
        tool_name: String,
        approved: bool,
    },

    // -- Plan mode --
    PlanSubmitted {
        plan_length: usize,
    },
    PlanResolved {
        approved: bool,
    },
    PlanEnterResolved {
        mode: String,
    },

    // -- Compaction --
    CompactionFinished {
        tokens_before: u64,
        tokens_after: u64,
    },
    CompactionFailed {
        reason: String,
    },

    // -- Context --
    ContextProjectionRepaired {
        anomaly_count: u32,
    },

    // -- Background tasks --
    BackgroundTaskCreated {
        task_id: String,
        description: String,
    },
    BackgroundTaskCompleted {
        task_id: String,
        exit_code: Option<i32>,
    },

    // -- Questions --
    QuestionDismissed {
        question_id: String,
    },
    QuestionAnswered {
        question_id: String,
    },

    // -- Goal --
    GoalCreated {
        objective_length: usize,
    },
    GoalBudgetSet {
        budget_type: String,
        value: u64,
        unit: String,
    },
    GoalContinued {
        turns_used: u64,
    },
    GoalCleared,
    GoalStatusChanged {
        from_status: String,
        to_status: String,
    },

    // -- File search fallbacks --
    GrepToolRgFallback {
        pattern: String,
    },
    GlobToolRgFallback {
        pattern: String,
    },
    FsGrepNodeFallback {
        pattern: String,
    },

    // -- MCP / Subagent --
    McpConnected {
        server_name: String,
    },
    McpFailed {
        server_name: String,
        error: String,
    },
    SubagentCreated {
        subagent_type: String,
    },

    // -- Session --
    SessionStarted {
        session_id: String,
    },
    SessionLoadFailed {
        error: String,
    },
    SessionCreateMs {
        duration_ms: u64,
    },

    // -- Cron --
    CronMissed {
        job_id: String,
        coalesced_count: u32,
    },
    CronScheduled {
        job_id: String,
        recurring: bool,
    },
    CronDeleted {
        job_id: String,
    },
    CronFired {
        job_id: String,
    },

    // -- Media --
    ImageCompress {
        original_bytes: u64,
        compressed_bytes: u64,
    },
    ImageCrop {
        width: u32,
        height: u32,
    },
    VideoUpload {
        size_bytes: u64,
    },

    // -- Startup / exit --
    FirstLaunch,
    Exit,

    // -- Custom / extension --
    Custom {
        event_name: String,
        properties: TelemetryProperties,
    },
}

impl TelemetryEvent {
    /// Return the canonical snake_case event name for this event.
    pub fn event_name(&self) -> &'static str {
        match self {
            Self::TurnStarted { .. } => "turn_started",
            Self::TurnInterrupted { .. } => "turn_interrupted",
            Self::TurnEnded { .. } => "turn_ended",
            Self::ToolCall { .. } => "tool_call",
            Self::ToolCallDedupDetected { .. } => "tool_call_dedup_detected",
            Self::ToolCallRepeat { .. } => "tool_call_repeat",
            Self::ToolExchangeAbandoned { .. } => "tool_exchange_abandoned",
            Self::ApiError { .. } => "api_error",
            Self::SkillInvoked { .. } => "skill_invoked",
            Self::FlowInvoked { .. } => "flow_invoked",
            Self::InputSteer { .. } => "input_steer",
            Self::Cancel { .. } => "cancel",
            Self::ConversationUndo { .. } => "conversation_undo",
            Self::YoloToggle { .. } => "yolo_toggle",
            Self::AfkToggle { .. } => "afk_toggle",
            Self::ModelSwitch { .. } => "model_switch",
            Self::ThinkingToggle { .. } => "thinking_toggle",
            Self::PermissionPolicyDecision { .. } => "permission_policy_decision",
            Self::PermissionApprovalResult { .. } => "permission_approval_result",
            Self::PlanSubmitted { .. } => "plan_submitted",
            Self::PlanResolved { .. } => "plan_resolved",
            Self::PlanEnterResolved { .. } => "plan_enter_resolved",
            Self::CompactionFinished { .. } => "compaction_finished",
            Self::CompactionFailed { .. } => "compaction_failed",
            Self::ContextProjectionRepaired { .. } => "context_projection_repaired",
            Self::BackgroundTaskCreated { .. } => "background_task_created",
            Self::BackgroundTaskCompleted { .. } => "background_task_completed",
            Self::QuestionDismissed { .. } => "question_dismissed",
            Self::QuestionAnswered { .. } => "question_answered",
            Self::GoalCreated { .. } => "goal_created",
            Self::GoalBudgetSet { .. } => "goal_budget_set",
            Self::GoalContinued { .. } => "goal_continued",
            Self::GoalCleared => "goal_cleared",
            Self::GoalStatusChanged { .. } => "goal_status_changed",
            Self::GrepToolRgFallback { .. } => "grep_tool_rg_fallback",
            Self::GlobToolRgFallback { .. } => "glob_tool_rg_fallback",
            Self::FsGrepNodeFallback { .. } => "fs_grep_node_fallback",
            Self::McpConnected { .. } => "mcp_connected",
            Self::McpFailed { .. } => "mcp_failed",
            Self::SubagentCreated { .. } => "subagent_created",
            Self::SessionStarted { .. } => "session_started",
            Self::SessionLoadFailed { .. } => "session_load_failed",
            Self::SessionCreateMs { .. } => "session_create_ms",
            Self::CronMissed { .. } => "cron_missed",
            Self::CronScheduled { .. } => "cron_scheduled",
            Self::CronDeleted { .. } => "cron_deleted",
            Self::CronFired { .. } => "cron_fired",
            Self::ImageCompress { .. } => "image_compress",
            Self::ImageCrop { .. } => "image_crop",
            Self::VideoUpload { .. } => "video_upload",
            Self::FirstLaunch => "first_launch",
            Self::Exit => "exit",
            Self::Custom { .. } => "custom",
        }
    }

    /// Convert this event into a flat properties map.
    pub fn to_properties(&self) -> TelemetryProperties {
        let mut props = HashMap::new();
        match self {
            Self::TurnStarted { turn_index } => {
                props.insert("turn_index".into(), turn_index.into());
            }
            Self::TurnInterrupted { turn_index, reason } => {
                props.insert("turn_index".into(), turn_index.into());
                props.insert("reason".into(), reason.as_str().into());
            }
            Self::TurnEnded {
                turn_index,
                duration_ms,
            } => {
                props.insert("turn_index".into(), turn_index.into());
                props.insert("duration_ms".into(), duration_ms.into());
            }
            Self::ToolCall {
                tool_name,
                success,
                duration_ms,
            } => {
                props.insert("tool_name".into(), tool_name.as_str().into());
                props.insert("success".into(), (*success).into());
                props.insert("duration_ms".into(), duration_ms.into());
            }
            Self::ToolCallDedupDetected { tool_name, step } => {
                props.insert("tool_name".into(), tool_name.as_str().into());
                props.insert("step".into(), step.into());
            }
            Self::ToolCallRepeat {
                tool_name,
                repeat_count,
            } => {
                props.insert("tool_name".into(), tool_name.as_str().into());
                props.insert("repeat_count".into(), repeat_count.into());
            }
            Self::ToolExchangeAbandoned {
                tool_name,
                attempt_count,
            } => {
                props.insert("tool_name".into(), tool_name.as_str().into());
                props.insert("attempt_count".into(), attempt_count.into());
            }
            Self::ApiError {
                provider,
                error_kind,
                status_code,
            } => {
                props.insert("provider".into(), provider.as_str().into());
                props.insert("error_kind".into(), error_kind.as_str().into());
                if let Some(code) = status_code {
                    props.insert("status_code".into(), (*code as i64).into());
                }
            }
            Self::SkillInvoked { skill_name } => {
                props.insert("skill_name".into(), skill_name.as_str().into());
            }
            Self::FlowInvoked { flow_name } => {
                props.insert("flow_name".into(), flow_name.as_str().into());
            }
            Self::InputSteer { target_turn } => {
                props.insert("target_turn".into(), target_turn.into());
            }
            Self::Cancel { turn_index } => {
                props.insert("turn_index".into(), turn_index.into());
            }
            Self::ConversationUndo { steps_undone } => {
                props.insert("steps_undone".into(), steps_undone.into());
            }
            Self::YoloToggle { enabled } => {
                props.insert("enabled".into(), (*enabled).into());
            }
            Self::AfkToggle { enabled } => {
                props.insert("enabled".into(), (*enabled).into());
            }
            Self::ModelSwitch {
                from_model,
                to_model,
            } => {
                props.insert("from_model".into(), from_model.as_str().into());
                props.insert("to_model".into(), to_model.as_str().into());
            }
            Self::ThinkingToggle { enabled } => {
                props.insert("enabled".into(), (*enabled).into());
            }
            Self::PermissionPolicyDecision {
                tool_name,
                decision,
                policy_name,
            } => {
                props.insert("tool_name".into(), tool_name.as_str().into());
                props.insert("decision".into(), decision.as_str().into());
                props.insert("policy_name".into(), policy_name.as_str().into());
            }
            Self::PermissionApprovalResult {
                tool_name,
                approved,
            } => {
                props.insert("tool_name".into(), tool_name.as_str().into());
                props.insert("approved".into(), (*approved).into());
            }
            Self::PlanSubmitted { plan_length } => {
                props.insert("plan_length".into(), plan_length.into());
            }
            Self::PlanResolved { approved } => {
                props.insert("approved".into(), (*approved).into());
            }
            Self::PlanEnterResolved { mode } => {
                props.insert("mode".into(), mode.as_str().into());
            }
            Self::CompactionFinished {
                tokens_before,
                tokens_after,
            } => {
                props.insert("tokens_before".into(), tokens_before.into());
                props.insert("tokens_after".into(), tokens_after.into());
            }
            Self::CompactionFailed { reason } => {
                props.insert("reason".into(), reason.as_str().into());
            }
            Self::ContextProjectionRepaired { anomaly_count } => {
                props.insert("anomaly_count".into(), anomaly_count.into());
            }
            Self::BackgroundTaskCreated {
                task_id,
                description,
            } => {
                props.insert("task_id".into(), task_id.as_str().into());
                props.insert("description".into(), description.as_str().into());
            }
            Self::BackgroundTaskCompleted { task_id, exit_code } => {
                props.insert("task_id".into(), task_id.as_str().into());
                if let Some(code) = exit_code {
                    props.insert("exit_code".into(), (*code as i64).into());
                }
            }
            Self::QuestionDismissed { question_id } => {
                props.insert("question_id".into(), question_id.as_str().into());
            }
            Self::QuestionAnswered { question_id } => {
                props.insert("question_id".into(), question_id.as_str().into());
            }
            Self::GoalCreated { objective_length } => {
                props.insert("objective_length".into(), objective_length.into());
            }
            Self::GoalBudgetSet {
                budget_type,
                value,
                unit,
            } => {
                props.insert("budget_type".into(), budget_type.as_str().into());
                props.insert("value".into(), value.into());
                props.insert("unit".into(), unit.as_str().into());
            }
            Self::GoalContinued { turns_used } => {
                props.insert("turns_used".into(), turns_used.into());
            }
            Self::GoalCleared => {}
            Self::GoalStatusChanged {
                from_status,
                to_status,
            } => {
                props.insert("from_status".into(), from_status.as_str().into());
                props.insert("to_status".into(), to_status.as_str().into());
            }
            Self::GrepToolRgFallback { pattern } => {
                props.insert("pattern".into(), pattern.as_str().into());
            }
            Self::GlobToolRgFallback { pattern } => {
                props.insert("pattern".into(), pattern.as_str().into());
            }
            Self::FsGrepNodeFallback { pattern } => {
                props.insert("pattern".into(), pattern.as_str().into());
            }
            Self::McpConnected { server_name } => {
                props.insert("server_name".into(), server_name.as_str().into());
            }
            Self::McpFailed { server_name, error } => {
                props.insert("server_name".into(), server_name.as_str().into());
                props.insert("error".into(), error.as_str().into());
            }
            Self::SubagentCreated { subagent_type } => {
                props.insert("subagent_type".into(), subagent_type.as_str().into());
            }
            Self::SessionStarted { session_id } => {
                props.insert("session_id".into(), session_id.as_str().into());
            }
            Self::SessionLoadFailed { error } => {
                props.insert("error".into(), error.as_str().into());
            }
            Self::SessionCreateMs { duration_ms } => {
                props.insert("duration_ms".into(), duration_ms.into());
            }
            Self::CronMissed {
                job_id,
                coalesced_count,
            } => {
                props.insert("job_id".into(), job_id.as_str().into());
                props.insert("coalesced_count".into(), coalesced_count.into());
            }
            Self::CronScheduled { job_id, recurring } => {
                props.insert("job_id".into(), job_id.as_str().into());
                props.insert("recurring".into(), (*recurring).into());
            }
            Self::CronDeleted { job_id } => {
                props.insert("job_id".into(), job_id.as_str().into());
            }
            Self::CronFired { job_id } => {
                props.insert("job_id".into(), job_id.as_str().into());
            }
            Self::ImageCompress {
                original_bytes,
                compressed_bytes,
            } => {
                props.insert("original_bytes".into(), original_bytes.into());
                props.insert("compressed_bytes".into(), compressed_bytes.into());
            }
            Self::ImageCrop { width, height } => {
                props.insert("width".into(), width.into());
                props.insert("height".into(), height.into());
            }
            Self::VideoUpload { size_bytes } => {
                props.insert("size_bytes".into(), size_bytes.into());
            }
            Self::FirstLaunch | Self::Exit => {}
            Self::Custom {
                event_name,
                properties: custom_props,
            } => {
                props.insert("event_name".into(), event_name.as_str().into());
                props.extend(custom_props.clone());
            }
        }
        props
    }
}

// ---------------------------------------------------------------------------
// TelemetrySink — the appender trait
// ---------------------------------------------------------------------------

/// A sink (appender) that receives telemetry events.
///
/// Implementations are responsible for their own buffering, transport, and
/// error handling. A sink must never panic — if it cannot process an event,
/// it should log internally and return.
pub trait TelemetrySink: Send + Sync + fmt::Debug {
    /// Called for every event when the service is enabled.
    fn track(&self, event: &TelemetryEvent, context: &TelemetryContext);

    /// Called on shutdown; sinks should flush any pending buffers.
    fn flush(&self) {}

    /// Human-readable name for diagnostics.
    fn name(&self) -> &str {
        "sink"
    }
}

/// A sink that discards all events — the default.
#[derive(Debug, Default, Clone)]
pub struct NoopSink;

impl TelemetrySink for NoopSink {
    fn track(&self, _event: &TelemetryEvent, _context: &TelemetryContext) {}

    fn name(&self) -> &str {
        "noop"
    }
}

/// A sink that logs events via `tracing::debug!`.
#[derive(Debug, Default, Clone)]
pub struct LoggingSink;

impl TelemetrySink for LoggingSink {
    fn track(&self, event: &TelemetryEvent, context: &TelemetryContext) {
        tracing::debug!(
            event = event.event_name(),
            ?context,
            props = ?event.to_properties(),
            "telemetry event"
        );
    }

    fn name(&self) -> &str {
        "logging"
    }
}

/// A sink that collects events into a vector — useful for testing.
#[derive(Debug, Default)]
pub struct CollectingSink {
    events: RwLock<Vec<(TelemetryEvent, TelemetryContext)>>,
}

impl CollectingSink {
    /// Create a new empty collecting sink.
    pub fn new() -> Self {
        Self::default()
    }

    /// Take all collected events, leaving the sink empty.
    pub fn drain(&self) -> Vec<(TelemetryEvent, TelemetryContext)> {
        let mut guard = self.events.write().unwrap_or_else(|p| p.into_inner());
        std::mem::take(&mut *guard)
    }

    /// Return the number of collected events.
    pub fn len(&self) -> usize {
        self.events.read().unwrap_or_else(|p| p.into_inner()).len()
    }

    /// Return true if no events have been collected.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Return a snapshot of collected events.
    pub fn snapshot(&self) -> Vec<(TelemetryEvent, TelemetryContext)> {
        self.events
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }
}

impl TelemetrySink for CollectingSink {
    fn track(&self, event: &TelemetryEvent, context: &TelemetryContext) {
        let mut guard = self.events.write().unwrap_or_else(|p| p.into_inner());
        guard.push((event.clone(), context.clone()));
    }

    fn name(&self) -> &str {
        "collecting"
    }
}

// ---------------------------------------------------------------------------
// TelemetryService — the central fan-out service
// ---------------------------------------------------------------------------

/// The central telemetry service that fans events out to registered sinks.
///
/// Cheaply cloneable behind `Arc`; all clones share the same sink list and
/// enable state.
#[derive(Debug, Clone)]
pub struct TelemetryService {
    inner: Arc<TelemetryServiceInner>,
}

#[derive(Debug)]
struct TelemetryServiceInner {
    sinks: RwLock<Vec<Arc<dyn TelemetrySink>>>,
    context: RwLock<TelemetryContext>,
    enabled: AtomicBool,
}

impl TelemetryService {
    /// Create a new disabled service with no sinks.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(TelemetryServiceInner {
                sinks: RwLock::new(Vec::new()),
                context: RwLock::new(TelemetryContext::default()),
                enabled: AtomicBool::new(false),
            }),
        }
    }

    /// Create a new enabled service with no sinks.
    pub fn new_enabled() -> Self {
        Self {
            inner: Arc::new(TelemetryServiceInner {
                sinks: RwLock::new(Vec::new()),
                context: RwLock::new(TelemetryContext::default()),
                enabled: AtomicBool::new(true),
            }),
        }
    }

    /// Add a sink. Returns the service for chaining.
    pub fn add_sink(&self, sink: Arc<dyn TelemetrySink>) {
        let mut guard = self.inner.sinks.write().unwrap_or_else(|p| p.into_inner());
        guard.push(sink);
    }

    /// Remove all sinks.
    pub fn clear_sinks(&self) {
        let mut guard = self.inner.sinks.write().unwrap_or_else(|p| p.into_inner());
        guard.clear();
    }

    /// Set the ambient context.
    pub fn set_context(&self, context: TelemetryContext) {
        let mut guard = self
            .inner
            .context
            .write()
            .unwrap_or_else(|p| p.into_inner());
        *guard = context;
    }

    /// Patch specific fields of the ambient context.
    pub fn patch_context(
        &self,
        device_id: Option<String>,
        session_id: Option<String>,
        agent_id: Option<String>,
    ) {
        let mut guard = self
            .inner
            .context
            .write()
            .unwrap_or_else(|p| p.into_inner());
        if device_id.is_some() {
            guard.device_id = device_id;
        }
        if session_id.is_some() {
            guard.session_id = session_id;
        }
        if agent_id.is_some() {
            guard.agent_id = agent_id;
        }
    }

    /// Enable or disable telemetry.
    pub fn set_enabled(&self, enabled: bool) {
        self.inner.enabled.store(enabled, Ordering::Relaxed);
    }

    /// Return whether telemetry is enabled.
    pub fn is_enabled(&self) -> bool {
        self.inner.enabled.load(Ordering::Relaxed)
    }

    /// Emit an event to all registered sinks.
    ///
    /// If the service is disabled, this is a no-op.
    pub fn track(&self, event: TelemetryEvent) {
        if !self.is_enabled() {
            return;
        }
        let context = self
            .inner
            .context
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        // Clone the sink list out of the lock before iterating so a sink that
        // re-enters a service write method (e.g. adding a sink from its
        // callback) cannot deadlock against the read guard we would otherwise
        // hold for the duration of the fan-out.
        let sinks: Vec<_> = self
            .inner
            .sinks
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .cloned()
            .collect();
        for sink in &sinks {
            sink.track(&event, &context);
        }
    }

    /// Flush all sinks.
    pub fn flush(&self) {
        // Same lock-release-before-fan-out discipline as `track`: a sink's
        // `flush` may re-enter the service, so we snapshot the sinks under the
        // guard and then drop it.
        let sinks: Vec<_> = self
            .inner
            .sinks
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .cloned()
            .collect();
        for sink in &sinks {
            sink.flush();
        }
    }
}

impl Default for TelemetryService {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // --- TelemetryValue ---

    #[test]
    fn should_produce_correct_values_when_converting_from_primitives() {
        assert_eq!(TelemetryValue::from(true), TelemetryValue::Bool(true));
        assert_eq!(TelemetryValue::from(42i64), TelemetryValue::Int(42));
        assert_eq!(TelemetryValue::from(3.15), TelemetryValue::Float(3.15));
        assert_eq!(
            TelemetryValue::from("hello"),
            TelemetryValue::Str("hello".into())
        );
        assert_eq!(TelemetryValue::from(None::<String>), TelemetryValue::Null);
    }

    // --- TelemetryEvent ---

    #[test]
    fn should_return_snake_case_when_calling_event_name() {
        assert_eq!(TelemetryEvent::FirstLaunch.event_name(), "first_launch");
        assert_eq!(TelemetryEvent::GoalCleared.event_name(), "goal_cleared");
        assert_eq!(
            TelemetryEvent::ToolCallDedupDetected {
                tool_name: "x".into(),
                step: 1
            }
            .event_name(),
            "tool_call_dedup_detected"
        );
    }

    #[test]
    fn should_include_all_fields_when_converting_to_properties() {
        let event = TelemetryEvent::ToolCall {
            tool_name: "Bash".into(),
            success: true,
            duration_ms: 150,
        };
        let props = event.to_properties();
        assert_eq!(
            props.get("tool_name"),
            Some(&TelemetryValue::Str("Bash".into()))
        );
        assert_eq!(props.get("success"), Some(&TelemetryValue::Bool(true)));
        assert_eq!(props.get("duration_ms"), Some(&TelemetryValue::Int(150)));
    }

    #[test]
    fn should_return_empty_map_when_converting_goal_cleared_to_properties() {
        let event = TelemetryEvent::GoalCleared;
        let props = event.to_properties();
        assert!(props.is_empty());
    }

    #[test]
    fn should_preserve_custom_props_when_converting_custom_event_to_properties() {
        let mut custom = HashMap::new();
        custom.insert("key".into(), TelemetryValue::Str("val".into()));
        let event = TelemetryEvent::Custom {
            event_name: "my_event".into(),
            properties: custom,
        };
        let props = event.to_properties();
        assert_eq!(props.get("key"), Some(&TelemetryValue::Str("val".into())));
    }

    // --- NoopSink ---

    #[test]
    fn should_not_panic_when_tracking_event_with_noop_sink() {
        let sink = NoopSink;
        let ctx = TelemetryContext::default();
        sink.track(&TelemetryEvent::FirstLaunch, &ctx);
        assert_eq!(sink.name(), "noop");
    }

    // --- LoggingSink ---

    #[test]
    fn should_not_panic_when_tracking_event_with_logging_sink() {
        let sink = LoggingSink;
        let ctx = TelemetryContext::default();
        sink.track(&TelemetryEvent::Exit, &ctx);
        assert_eq!(sink.name(), "logging");
    }

    // --- CollectingSink ---

    #[test]
    fn should_return_all_tracked_events_when_draining_sink() {
        let sink = CollectingSink::new();
        let ctx = TelemetryContext::default();
        sink.track(&TelemetryEvent::FirstLaunch, &ctx);
        sink.track(&TelemetryEvent::Exit, &ctx);

        let drained = sink.drain();
        assert_eq!(drained.len(), 2);
        assert!(sink.is_empty());
    }

    #[test]
    fn should_capture_context_when_tracking_with_collecting_sink() {
        let sink = CollectingSink::new();
        let ctx = TelemetryContext {
            session_id: Some("s1".into()),
            agent_id: Some("a1".into()),
            ..Default::default()
        };
        sink.track(
            &TelemetryEvent::SessionStarted {
                session_id: "s1".into(),
            },
            &ctx,
        );

        let snap = sink.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].1.session_id.as_deref(), Some("s1"));
        assert_eq!(snap[0].1.agent_id.as_deref(), Some("a1"));
    }

    // --- TelemetryService ---

    #[test]
    fn should_receive_no_events_when_tracking_with_disabled_service() {
        let svc = TelemetryService::new(); // disabled
        let sink = Arc::new(CollectingSink::new());
        svc.add_sink(sink.clone());

        svc.track(TelemetryEvent::FirstLaunch);
        assert!(sink.is_empty());
    }

    #[test]
    fn should_receive_event_when_tracking_with_enabled_service() {
        let svc = TelemetryService::new_enabled();
        let sink = Arc::new(CollectingSink::new());
        svc.add_sink(sink.clone());

        svc.track(TelemetryEvent::FirstLaunch);
        assert_eq!(sink.len(), 1);
    }

    #[test]
    fn should_propagate_ambient_context_when_tracking_with_service_context_set() {
        let svc = TelemetryService::new_enabled();
        let sink = Arc::new(CollectingSink::new());
        svc.add_sink(sink.clone());
        svc.set_context(TelemetryContext {
            device_id: Some("dev1".into()),
            session_id: Some("sess1".into()),
            ..Default::default()
        });

        svc.track(TelemetryEvent::Exit);
        let snap = sink.snapshot();
        assert_eq!(snap[0].1.device_id.as_deref(), Some("dev1"));
        assert_eq!(snap[0].1.session_id.as_deref(), Some("sess1"));
    }

    #[test]
    fn should_update_only_specified_fields_when_patching_context() {
        let svc = TelemetryService::new_enabled();
        svc.set_context(TelemetryContext {
            device_id: Some("dev1".into()),
            session_id: Some("sess1".into()),
            agent_id: None,
        });
        svc.patch_context(None, Some("sess2".into()), Some("a1".into()));

        let ctx = svc.inner.context.read().unwrap_or_else(|p| p.into_inner());
        assert_eq!(ctx.device_id.as_deref(), Some("dev1")); // unchanged
        assert_eq!(ctx.session_id.as_deref(), Some("sess2")); // updated
        assert_eq!(ctx.agent_id.as_deref(), Some("a1")); // updated
    }

    #[test]
    fn should_deliver_events_to_all_sinks_when_tracking_with_multiple_sinks() {
        let svc = TelemetryService::new_enabled();
        let sink1 = Arc::new(CollectingSink::new());
        let sink2 = Arc::new(CollectingSink::new());
        svc.add_sink(sink1.clone());
        svc.add_sink(sink2.clone());

        svc.track(TelemetryEvent::FirstLaunch);
        svc.track(TelemetryEvent::Exit);

        assert_eq!(sink1.len(), 2);
        assert_eq!(sink2.len(), 2);
    }

    #[test]
    fn should_return_false_from_is_enabled_when_service_disabled() {
        let svc = TelemetryService::new_enabled();
        assert!(svc.is_enabled());
        svc.set_enabled(false);
        assert!(!svc.is_enabled());
    }

    #[test]
    fn should_respect_enabled_state_when_toggling_service_enabled() {
        let svc = TelemetryService::new_enabled();
        let sink = Arc::new(CollectingSink::new());
        svc.add_sink(sink.clone());

        svc.track(TelemetryEvent::FirstLaunch); // enabled → captured
        svc.set_enabled(false);
        svc.track(TelemetryEvent::Exit); // disabled → dropped
        svc.set_enabled(true);
        svc.track(TelemetryEvent::GoalCleared); // re-enabled → captured

        assert_eq!(sink.len(), 2);
    }

    #[test]
    fn should_share_sinks_between_clones_when_tracking_through_clone() {
        let svc = TelemetryService::new_enabled();
        let sink = Arc::new(CollectingSink::new());
        svc.add_sink(sink.clone());
        let svc2 = svc.clone();

        svc2.track(TelemetryEvent::FirstLaunch);

        assert_eq!(sink.len(), 1, "clone should share the same sink list");
    }

    #[test]
    fn should_include_optional_status_code_when_converting_api_error_to_properties() {
        let event = TelemetryEvent::ApiError {
            provider: "anthropic".into(),
            error_kind: "rate_limit".into(),
            status_code: Some(429),
        };
        let props = event.to_properties();
        assert_eq!(
            props.get("provider"),
            Some(&TelemetryValue::Str("anthropic".into()))
        );
        assert_eq!(props.get("status_code"), Some(&TelemetryValue::Int(429)));
    }

    #[test]
    fn should_omit_status_code_when_converting_api_error_without_status_code_to_properties() {
        let event = TelemetryEvent::ApiError {
            provider: "openai".into(),
            error_kind: "timeout".into(),
            status_code: None,
        };
        let props = event.to_properties();
        assert!(!props.contains_key("status_code"));
    }
}
