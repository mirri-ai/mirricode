use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use crate::cancel::CancelToken;
use crate::error::EngineResult;
use crate::message::ContentPart;

/// A boxed, pinned future for async tool execution.
pub type ToolFuture = Pin<Box<dyn Future<Output = ToolResult> + Send>>;

/// Resource access descriptor for concurrency conflict detection.
///
/// When multiple tool calls are batched in a single turn, the scheduler uses
/// [`ToolAccess::conflicts`] to decide which calls can run concurrently and
/// which must be serialized.
#[derive(Debug, Clone)]
pub struct ToolAccess {
    pub kind: AccessKind,
    pub path: Option<String>,
    pub operation: AccessOperation,
}

/// Coarse-grained resource category for conflict detection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccessKind {
    /// Conflicts with everything (global mutex).
    All,
    /// File-system access.
    File,
    /// Search/glob access.
    Search,
}

/// Read/write classification of a resource access.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccessOperation {
    Read,
    Write,
    ReadWrite,
}

impl ToolAccess {
    /// Check if two access lists conflict.
    ///
    /// Rules (matching TypeScript `tool-access.ts`):
    /// - `All` conflicts with everything.
    /// - `File` conflicts if at least one side writes AND paths overlap.
    /// - `Read + Read` and `Read + Search` never conflict.
    pub fn conflicts(left: &[ToolAccess], right: &[ToolAccess]) -> bool {
        for l in left {
            for r in right {
                if Self::conflicts_pair(l, r) {
                    return true;
                }
            }
        }
        false
    }

    fn conflicts_pair(left: &ToolAccess, right: &ToolAccess) -> bool {
        // All conflicts with everything.
        if left.kind == AccessKind::All || right.kind == AccessKind::All {
            return true;
        }

        // File-File: conflict only if any side writes and paths overlap.
        if left.kind == AccessKind::File && right.kind == AccessKind::File {
            let any_write = matches!(
                left.operation,
                AccessOperation::Write | AccessOperation::ReadWrite
            ) || matches!(
                right.operation,
                AccessOperation::Write | AccessOperation::ReadWrite
            );
            if !any_write {
                return false;
            }
            // Check path overlap.
            match (&left.path, &right.path) {
                (Some(lp), Some(rp)) => Self::paths_overlap(lp, rp),
                // No path on at least one side: treat as conflict (conservative).
                _ => true,
            }
        } else {
            // Different kinds (e.g. File + Search): no conflict.
            false
        }
    }

    /// Check if two paths overlap (recursive prefix match, case-insensitive).
    ///
    /// `/a/b` overlaps with `/a/b`, `/a/b/c`, and `/a`. Trailing slashes are
    /// ignored.
    fn paths_overlap(a: &str, b: &str) -> bool {
        let a_lower = a.to_lowercase();
        let b_lower = b.to_lowercase();
        let a = a_lower.trim_end_matches('/');
        let b = b_lower.trim_end_matches('/');
        a == b || a.starts_with(&format!("{b}/")) || b.starts_with(&format!("{a}/"))
    }
}

/// UI display hint for a tool call.
///
/// The host uses this to render a compact, human-readable summary of what a
/// tool call does (e.g. in the transcript or approval prompt) without having
/// to parse the raw JSON arguments. The wire dialect is the protocol
/// `ToolInputDisplay` contract (`docs/specs/rust-core-design/
/// 01-protocol-unification.md` $defs): internally tagged `kind`, snake_case
/// variant names, optional fields omitted (never null) when absent.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolInputDisplay {
    Command {
        command: String,
    },
    FileIo {
        path: String,
        operation: String,
    },
    Diff {
        path: String,
        before: String,
        after: String,
    },
    Search {
        query: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        scope: Option<String>,
    },
    UrlFetch {
        url: String,
    },
    AgentCall {
        agent_name: String,
        prompt: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        background: Option<bool>,
    },
    SkillCall {
        skill_name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        args: Option<String>,
    },
    TodoList,
    BackgroundTask,
    TaskStop {
        task_id: String,
    },
    PlanReview {
        plan: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        path: Option<String>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        options: Vec<PlanReviewOption>,
    },
    GoalStart {
        objective: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        completion_criterion: Option<String>,
    },
    Generic {
        summary: String,
    },
}

/// One selectable approach offered by an `ExitPlanMode` plan review.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PlanReviewOption {
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// The execution plan returned by [`Tool::resolve_execution`].
#[derive(Debug, Clone)]
pub struct ExecutionPlan {
    /// Resource accesses this tool call will perform.
    pub accesses: Vec<ToolAccess>,
    /// UI display hint.
    pub display: ToolInputDisplay,
    /// DSL pattern for permission rules (e.g. `"Write(/path)"` or `"Bash"`).
    pub approval_rule: String,
    /// Whether to stop scheduling later tool calls in this batch after this
    /// one. Used by tools that mutate global state in a way that invalidates
    /// the rest of the batch (e.g. a goal change).
    pub stop_batch_after_this: bool,
}

/// Two-phase tool execution result.
///
/// Phase 1 ([`Tool::resolve_execution`]) returns this. Phase 2 (`execute`)
/// runs the closure only if `Runnable`.
pub enum ToolExecution {
    /// The tool is ready to execute. Call the `execute` closure to run it.
    Runnable {
        plan: ExecutionPlan,
        execute: Box<dyn FnOnce(ExecutableToolContext) -> ToolFuture + Send>,
    },
    /// The tool refused to run (e.g. disabled MCP tool, path security error).
    Error(ToolErrorResult),
}

/// Context passed to tool execution (phase 2).
pub struct ExecutableToolContext {
    pub turn_id: String,
    pub tool_call_id: String,
    pub cancel: CancelToken,
    /// Live-update channel for the running tool call. `send` is a no-op when
    /// no sink is wired (tests, detached execution), so tools may hold and
    /// use it unconditionally.
    pub progress: ToolProgressHandle,
    /// The turn's ADR-006 wait registry: a tool's blocking wait registers
    /// its WaitKind entry into it (WaitingTask / WaitingSubagent /
    /// WaitingGate) so the watchdog can pull the owner verdict at window
    /// expiry. `None` (standalone tests, detached execution) registers
    /// nothing — the tool's own behavior is unchanged.
    pub wait_registry: Option<std::sync::Arc<crate::turn_flow::WaitRegistry>>,
}

/// One incremental update emitted by a running tool call, surfaced live to
/// the host as a `tool.progress` event (volatile: streamed, never replayed).
///
/// Mirrors the protocol's `ToolUpdate` shape (`toolUpdateSchema` in
/// `packages/protocol/src/events.ts`): `kind` discriminates the update
/// class, and the optional fields are populated per kind — `text` for
/// stdout/stderr/status, `percent` for progress, and `customKind`/
/// `customData` for custom updates. Absent optional fields are omitted from
/// the serialized form (skip-when-none).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolUpdate {
    pub kind: ToolUpdateKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub percent: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub custom_kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub custom_data: Option<serde_json::Value>,
}

/// The update class of a [`ToolUpdate`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolUpdateKind {
    Stdout,
    Stderr,
    Progress,
    Status,
    Custom,
}

/// A clonable handle a running tool uses to report incremental updates
/// (`ctx.progress.send(update)`). The engine wires the sink to the turn's
/// live-event callback; a default (no-sink) handle is a silent no-op.
#[derive(Clone, Default)]
pub struct ToolProgressHandle {
    sink: Option<Arc<dyn Fn(ToolUpdate) + Send + Sync>>,
}

impl ToolProgressHandle {
    /// Wire a sink that receives every update sent through this handle.
    pub fn with_sink(sink: Arc<dyn Fn(ToolUpdate) + Send + Sync>) -> Self {
        Self { sink: Some(sink) }
    }

    /// Emit one update; a no-op when no sink is wired.
    pub fn send(&self, update: ToolUpdate) {
        if let Some(sink) = &self.sink {
            sink(update);
        }
    }
}

impl std::fmt::Debug for ToolProgressHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolProgressHandle")
            .field("wired", &self.sink.is_some())
            .finish()
    }
}

/// A tool's error result (synthetic, no execution).
#[derive(Debug, Clone)]
pub struct ToolErrorResult {
    pub message: String,
}

/// The result of executing a tool.
#[derive(Debug, Clone)]
pub struct ToolResult {
    /// What the model sees (string or content-part array for media).
    pub output: ToolOutput,
    /// Whether the tool execution resulted in an error.
    pub is_error: bool,
    /// Model-visible, UI-invisible side channel.
    pub note: Option<String>,
    /// Loop-control hint — stop the current turn. Stripped before persistence.
    pub stop_turn: bool,
    /// Signals the visible output is incomplete.
    pub truncated: bool,
    /// 可选的系统消息注入 — 当工具需要向上下文注入额外指令时设置。
    ///
    /// 内容会以 `<system-reminder>` 标签的形式追加到上下文记忆中,
    /// 在工具结果之后立即执行。例如 `SkillTool` 使用此字段注入 skill 指令。
    pub injection: Option<String>,
}

/// Tool output can be text or structured content parts.
///
/// Wire shape (the TS protocol's `tool.result` `output`): untagged — plain
/// text serializes to a bare JSON string, structured parts to the
/// content-part array.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(untagged)]
pub enum ToolOutput {
    Text(String),
    Parts(Vec<ContentPart>),
}

impl ToolOutput {
    pub fn text(s: impl Into<String>) -> Self {
        Self::Text(s.into())
    }
}

impl ToolResult {
    pub fn success(output: impl Into<String>) -> Self {
        Self {
            output: ToolOutput::text(output),
            is_error: false,
            note: None,
            stop_turn: false,
            truncated: false,
            injection: None,
        }
    }

    pub fn error(message: impl Into<String>) -> Self {
        Self {
            output: ToolOutput::text(message),
            is_error: true,
            note: None,
            stop_turn: false,
            truncated: false,
            injection: None,
        }
    }
}

/// The core Tool trait.
///
/// Two-phase design:
/// 1. `resolve_execution(input)` — synchronous planning: computes accesses,
///    display, approval rule, resolves safe paths. Can throw a path-security
///    error *before any I/O*.
/// 2. `execute(ctx)` — the actual work, called only if phase 1 returned
///    `Runnable`.
///
/// This separation lets the scheduler batch-plan all tool calls in a turn,
/// detect conflicts, run the permission gate, and only then execute — all
/// without committing side effects for tools that will be denied.
pub trait Tool: Send + Sync {
    /// Tool name (e.g. `"Read"`, `"Write"`, `"Bash"`).
    fn name(&self) -> &str;

    /// Human-readable description shown to the LLM.
    fn description(&self) -> &str;

    /// JSON Schema for the tool's parameters.
    fn parameters(&self) -> &serde_json::Value;

    /// Capability tags (e.g. `"code.explore"`, `"code.read"`).
    fn capabilities(&self) -> &[String] {
        &[]
    }

    /// Whether this tool is deferred (progressive disclosure marker).
    fn deferred(&self) -> bool {
        false
    }

    /// Phase 1: resolve the execution plan.
    ///
    /// Takes raw JSON input (already parsed + validated), returns either
    /// a `Runnable` execution (with plan + execute closure) or an `Error`
    /// result.
    fn resolve_execution(&self, input: serde_json::Value) -> EngineResult<ToolExecution>;
}

/// A simple tool definition (name + description + schema) without execution.
/// Used for the tool table sent to the provider.
#[derive(Debug, Clone)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
    pub deferred: bool,
}

impl From<&dyn Tool> for ToolDefinition {
    fn from(tool: &dyn Tool) -> Self {
        Self {
            name: tool.name().to_string(),
            description: tool.description().to_string(),
            parameters: tool.parameters().clone(),
            deferred: tool.deferred(),
        }
    }
}

/// A user-registered tool whose execution is forwarded to the client via
/// reverse RPC.
///
/// When the LLM calls this tool, the engine sends a `tool.call` reverse-RPC
/// to the client (through [`ReverseRpcCallback`]). The client executes the
/// tool and returns the result as a JSON value.
///
/// Created by `agent.registerTool` and removed by `agent.unregisterTool`.
///
/// ## Callback slot
///
/// The tool holds the session's [`ReverseRpcSlot`](crate::reverse_rpc::ReverseRpcSlot)
/// and resolves the current callback at call time — the same late-subscribe
/// semantics as the `AskUserQuestion` tool. A resume replays the
/// registration before the host seeds the engine-level callback into the
/// slot (and a serve transport may subscribe even later); the replayed tool
/// must still observe whichever callback lands in the slot afterwards. When
/// the slot is empty at call time the call fails fast with an error result.
pub struct RpcTool {
    name: String,
    description: String,
    parameters: serde_json::Value,
    callback_slot: crate::reverse_rpc::ReverseRpcSlot,
}

impl RpcTool {
    /// Create a new RPC-backed tool.
    ///
    /// The `callback_slot` is the session's reverse-RPC channel slot; the
    /// engine calls `tool.call` on the CURRENT callback at execution time and
    /// expects `{ output, isError }` back.
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: serde_json::Value,
        callback_slot: crate::reverse_rpc::ReverseRpcSlot,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            parameters,
            callback_slot,
        }
    }
}

impl Tool for RpcTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters(&self) -> &serde_json::Value {
        &self.parameters
    }

    fn resolve_execution(&self, input: serde_json::Value) -> EngineResult<ToolExecution> {
        let callback_slot = Arc::clone(&self.callback_slot);
        let tool_name = self.name.clone();
        let plan = ExecutionPlan {
            accesses: vec![],
            display: ToolInputDisplay::Generic {
                summary: format!("RPC tool: {}", tool_name),
            },
            approval_rule: tool_name.clone(),
            stop_batch_after_this: false,
        };
        Ok(ToolExecution::Runnable {
            plan,
            execute: Box::new(move |_ctx| {
                Box::pin(async move {
                    let Some(callback) = callback_slot
                        .read()
                        .unwrap_or_else(|e| e.into_inner())
                        .clone()
                    else {
                        return ToolResult {
                            output: ToolOutput::Text(format!(
                                "RPC tool {}: no client is attached to this session, \
                                 so the tool cannot be executed right now",
                                tool_name
                            )),
                            is_error: true,
                            note: None,
                            stop_turn: false,
                            truncated: false,
                            injection: None,
                        };
                    };
                    let rpc_params = serde_json::json!({
                        "name": tool_name,
                        "input": input,
                    });
                    match callback.call("tool.call", rpc_params).await {
                        Ok(result) => {
                            let is_error = result
                                .get("isError")
                                .and_then(|v| v.as_bool())
                                .unwrap_or(false);
                            let output_text = result
                                .get("output")
                                .and_then(|v| v.as_str())
                                .map(|s| s.to_string())
                                .unwrap_or_else(|| {
                                    serde_json::to_string(&result).unwrap_or_default()
                                });
                            ToolResult {
                                output: ToolOutput::Text(output_text),
                                is_error,
                                note: None,
                                stop_turn: false,
                                truncated: false,
                                injection: None,
                            }
                        }
                        Err(e) => ToolResult {
                            output: ToolOutput::Text(format!("RPC tool error: {e}")),
                            is_error: true,
                            note: None,
                            stop_turn: false,
                            truncated: false,
                            injection: None,
                        },
                    }
                })
            }),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reverse_rpc::ReverseRpcCallback;
    use async_trait::async_trait;
    use serde_json::Value;
    use serde_json::json;

    /// A mock reverse-RPC callback that returns a canned response.
    struct MockCallback {
        response: Result<Value, String>,
    }

    #[async_trait]
    impl ReverseRpcCallback for MockCallback {
        async fn call(&self, _method: &str, _params: Value) -> Result<Value, String> {
            self.response.clone()
        }
    }

    /// An empty reverse-RPC slot (no callback installed).
    fn empty_slot() -> crate::reverse_rpc::ReverseRpcSlot {
        std::sync::Arc::new(std::sync::RwLock::new(None))
    }

    /// A slot with the given callback pre-installed.
    fn slot_with(
        callback: std::sync::Arc<dyn ReverseRpcCallback>,
    ) -> crate::reverse_rpc::ReverseRpcSlot {
        std::sync::Arc::new(std::sync::RwLock::new(Some(callback)))
    }

    #[tokio::test]
    async fn should_forward_input_to_callback_when_rpc_tool_executed() {
        // Given an RpcTool with a mock callback that returns a text result
        let tool = RpcTool::new(
            "myTool",
            "A test tool",
            json!({"type": "object", "properties": {}}),
            slot_with(Arc::new(MockCallback {
                response: Ok(json!({
                    "output": "hello from client",
                    "isError": false,
                })),
            })),
        );

        // When the tool resolves and executes
        let execution = tool
            .resolve_execution(json!({"key": "value"}))
            .expect("resolve");
        match execution {
            ToolExecution::Runnable { plan, execute } => {
                assert_eq!(plan.approval_rule, "myTool");
                let ctx = ExecutableToolContext {
                    wait_registry: None,
                    turn_id: "t1".to_string(),
                    tool_call_id: "tc1".to_string(),
                    cancel: CancelToken::new(),
                    progress: ToolProgressHandle::default(),
                };
                let result = execute(ctx).await;
                assert!(!result.is_error);
                assert_eq!(
                    result.output,
                    ToolOutput::Text("hello from client".to_string())
                );
            }
            ToolExecution::Error(e) => panic!("expected runnable, got error: {:?}", e),
        }
    }

    #[tokio::test]
    async fn should_return_error_result_when_callback_fails() {
        // Given an RpcTool whose callback returns an error
        let tool = RpcTool::new(
            "failingTool",
            "A failing tool",
            json!({"type": "object"}),
            slot_with(Arc::new(MockCallback {
                response: Err("connection refused".to_string()),
            })),
        );

        // When the tool executes
        let execution = tool.resolve_execution(json!({})).expect("resolve");
        match execution {
            ToolExecution::Runnable { execute, .. } => {
                let ctx = ExecutableToolContext {
                    wait_registry: None,
                    turn_id: "t1".to_string(),
                    tool_call_id: "tc1".to_string(),
                    cancel: CancelToken::new(),
                    progress: ToolProgressHandle::default(),
                };
                let result = execute(ctx).await;
                assert!(result.is_error);
                let output_str = match &result.output {
                    ToolOutput::Text(t) => t.clone(),
                    ToolOutput::Parts(_) => "{parts}".to_string(),
                };
                assert!(output_str.contains("connection refused"));
            }
            ToolExecution::Error(e) => panic!("expected runnable, got error: {:?}", e),
        }
    }

    #[test]
    fn should_expose_name_description_and_parameters_when_rpc_tool_created() {
        // Given an RpcTool
        let schema = json!({"type": "object", "properties": {"x": {"type": "string"}}});
        let tool = RpcTool::new("testTool", "description here", schema.clone(), empty_slot());

        // Then it exposes its metadata
        assert_eq!(tool.name(), "testTool");
        assert_eq!(tool.description(), "description here");
        assert_eq!(tool.parameters(), &schema);
        assert!(!tool.deferred());
    }

    #[tokio::test]
    async fn should_fail_fast_with_error_result_when_slot_is_empty_at_call_time() {
        // Given an RpcTool whose callback slot is empty (no client attached)
        let tool = RpcTool::new(
            "orphanTool",
            "A tool with no client",
            json!({"type": "object"}),
            empty_slot(),
        );

        // When the tool executes
        let execution = tool.resolve_execution(json!({})).expect("resolve");
        match execution {
            ToolExecution::Runnable { execute, .. } => {
                let ctx = ExecutableToolContext {
                    wait_registry: None,
                    turn_id: "t1".to_string(),
                    tool_call_id: "tc1".to_string(),
                    cancel: CancelToken::new(),
                    progress: ToolProgressHandle::default(),
                };
                let result = execute(ctx).await;
                // Then the call fails fast with an error result naming the tool
                assert!(result.is_error);
                let output_str = match &result.output {
                    ToolOutput::Text(t) => t.clone(),
                    ToolOutput::Parts(_) => "{parts}".to_string(),
                };
                assert!(output_str.contains("orphanTool"));
                assert!(output_str.contains("no client is attached"));
            }
            ToolExecution::Error(e) => panic!("expected runnable, got error: {:?}", e),
        }
    }

    #[tokio::test]
    async fn should_reach_callback_installed_into_slot_after_tool_construction() {
        // Given an RpcTool built over an EMPTY slot (the resume-replay shape:
        // the registration replays before the host seeds the callback)
        let slot = empty_slot();
        let tool = RpcTool::new(
            "lateTool",
            "A late-bound tool",
            json!({"type": "object"}),
            Arc::clone(&slot),
        );

        // When the callback is installed into the slot after construction and
        // the LLM then invokes the tool
        *slot.write().unwrap() = Some(Arc::new(MockCallback {
            response: Ok(json!({
                "output": "late hello",
                "isError": false,
            })),
        }) as Arc<dyn ReverseRpcCallback>);
        let execution = tool.resolve_execution(json!({})).expect("resolve");
        let result = match execution {
            ToolExecution::Runnable { execute, .. } => {
                let ctx = ExecutableToolContext {
                    wait_registry: None,
                    turn_id: "t1".to_string(),
                    tool_call_id: "tc1".to_string(),
                    cancel: CancelToken::new(),
                    progress: ToolProgressHandle::default(),
                };
                execute(ctx).await
            }
            ToolExecution::Error(e) => panic!("expected runnable, got error: {:?}", e),
        };

        // Then the call reaches the late-installed callback
        assert!(!result.is_error);
        assert_eq!(result.output, ToolOutput::Text("late hello".to_string()));
    }

    #[test]
    fn should_roundtrip_tool_update_through_the_wire_shape() {
        // Given a fully-populated stdout update
        let update = ToolUpdate {
            kind: ToolUpdateKind::Custom,
            text: Some("building".to_string()),
            percent: Some(42.5),
            custom_kind: Some("compile".to_string()),
            custom_data: Some(json!({"file": "main.rs"})),
        };

        // When it serializes to the protocol's camelCase shape
        let wire = serde_json::to_value(&update).expect("serialize");

        // Then every field is present and spelled per the ToolUpdate schema
        assert_eq!(wire["kind"], "custom");
        assert_eq!(wire["text"], "building");
        assert_eq!(wire["percent"], 42.5);
        assert_eq!(wire["customKind"], "compile");
        assert_eq!(wire["customData"]["file"], "main.rs");

        // And the value round-trips back through deserialization
        let back: ToolUpdate = serde_json::from_value(wire).expect("deserialize");
        assert_eq!(back, update);
    }

    #[test]
    fn should_omit_absent_tool_update_fields_when_serialized() {
        // Given an update carrying only the discriminator and text
        let update = ToolUpdate {
            kind: ToolUpdateKind::Stdout,
            text: Some("hello".to_string()),
            percent: None,
            custom_kind: None,
            custom_data: None,
        };

        // When it serializes
        let wire = serde_json::to_value(&update).expect("serialize");

        // Then the absent optional fields are omitted, not null
        let map = wire.as_object().expect("object");
        assert_eq!(map.len(), 2);
        assert!(map.contains_key("kind"));
        assert!(map.contains_key("text"));
    }

    #[test]
    fn should_swallow_updates_when_no_progress_sink_is_wired() {
        // Given a default (no-sink) handle
        let handle = ToolProgressHandle::default();

        // When an update is sent, it is a silent no-op (no panic)
        handle.send(ToolUpdate {
            kind: ToolUpdateKind::Status,
            text: Some("working".to_string()),
            percent: None,
            custom_kind: None,
            custom_data: None,
        });
    }

    #[test]
    fn should_forward_updates_to_the_wired_sink() {
        // Given a handle wired to a recording sink
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorder = Arc::clone(&seen);
        let handle = ToolProgressHandle::with_sink(Arc::new(move |update| {
            recorder.lock().unwrap().push(update);
        }));

        // When two updates are sent
        handle.send(ToolUpdate {
            kind: ToolUpdateKind::Progress,
            text: None,
            percent: Some(10.0),
            custom_kind: None,
            custom_data: None,
        });
        handle.send(ToolUpdate {
            kind: ToolUpdateKind::Stderr,
            text: Some("warn".to_string()),
            percent: None,
            custom_kind: None,
            custom_data: None,
        });

        // Then the sink received both, in order
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0].kind, ToolUpdateKind::Progress);
        assert_eq!(seen[0].percent, Some(10.0));
        assert_eq!(seen[1].kind, ToolUpdateKind::Stderr);
        assert_eq!(seen[1].text.as_deref(), Some("warn"));
    }

    /// Given the protocol wire contract (01-protocol-unification.md
    /// `ToolInputDisplay`), when a display is serialized, then it is the
    /// internally-tagged `{ kind: ... }` dialect with snake_case variant
    /// names — the shape every approval UI reads. Optional fields are
    /// omitted (never null) when absent.
    #[test]
    fn should_serialize_display_with_protocol_kind_tagging_when_crossing_the_wire() {
        let cases: Vec<(ToolInputDisplay, serde_json::Value)> = vec![
            (
                ToolInputDisplay::Command {
                    command: "ls".into(),
                },
                serde_json::json!({"kind":"command","command":"ls"}),
            ),
            (
                ToolInputDisplay::FileIo {
                    path: "/a.md".into(),
                    operation: "write".into(),
                },
                serde_json::json!({"kind":"file_io","path":"/a.md","operation":"write"}),
            ),
            (
                ToolInputDisplay::Diff {
                    path: "/a.rs".into(),
                    before: "old".into(),
                    after: "new".into(),
                },
                serde_json::json!({"kind":"diff","path":"/a.rs","before":"old","after":"new"}),
            ),
            (
                ToolInputDisplay::Search {
                    query: "foo".into(),
                    scope: Some("src".into()),
                },
                serde_json::json!({"kind":"search","query":"foo","scope":"src"}),
            ),
            (
                ToolInputDisplay::Search {
                    query: "foo".into(),
                    scope: None,
                },
                serde_json::json!({"kind":"search","query":"foo"}),
            ),
            (
                ToolInputDisplay::UrlFetch {
                    url: "https://example.com".into(),
                },
                serde_json::json!({"kind":"url_fetch","url":"https://example.com"}),
            ),
            (
                ToolInputDisplay::AgentCall {
                    agent_name: "coder".into(),
                    prompt: "fix it".into(),
                    background: None,
                },
                serde_json::json!({"kind":"agent_call","agent_name":"coder","prompt":"fix it"}),
            ),
            (
                ToolInputDisplay::SkillCall {
                    skill_name: "commit".into(),
                    args: None,
                },
                serde_json::json!({"kind":"skill_call","skill_name":"commit"}),
            ),
            (
                ToolInputDisplay::TodoList,
                serde_json::json!({"kind":"todo_list"}),
            ),
            (
                ToolInputDisplay::BackgroundTask,
                serde_json::json!({"kind":"background_task"}),
            ),
            (
                ToolInputDisplay::TaskStop {
                    task_id: "t-1".into(),
                },
                serde_json::json!({"kind":"task_stop","task_id":"t-1"}),
            ),
            (
                ToolInputDisplay::PlanReview {
                    plan: "the plan".into(),
                    path: Some("/p.md".into()),
                    options: vec![PlanReviewOption {
                        label: "Approach A".into(),
                        description: None,
                    }],
                },
                serde_json::json!({"kind":"plan_review","plan":"the plan","path":"/p.md","options":[{"label":"Approach A"}]}),
            ),
            (
                ToolInputDisplay::GoalStart {
                    objective: "ship it".into(),
                    completion_criterion: Some("tests green".into()),
                },
                serde_json::json!({"kind":"goal_start","objective":"ship it","completion_criterion":"tests green"}),
            ),
            (
                ToolInputDisplay::Generic {
                    summary: "s".into(),
                },
                serde_json::json!({"kind":"generic","summary":"s"}),
            ),
        ];
        for (display, expected) in cases {
            let wire = serde_json::to_value(&display).expect("display must serialize");
            assert_eq!(wire, expected, "wire shape of {display:?}");
        }
    }
}
