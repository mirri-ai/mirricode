//! `AskUserQuestion` tool — ask the user a structured question via reverse RPC.
//!
//! When the LLM calls this tool, the engine issues a `question.ask` reverse
//! RPC to the client, which presents the question to the user and returns
//! their answer. The answer is serialized as the tool result so the model can
//! act on it.
//!
//! ## Callback slot
//!
//! The tool holds the session's [`ReverseRpcSlot`](crate::reverse_rpc::ReverseRpcSlot)
//! and resolves the current callback at call time. A transport that
//! subscribes AFTER the owning agent was created still reaches the tool —
//! the callback is never baked into the agent's create-time snapshot.
//!
//! ## Background mode
//!
//! When `background` is `true` and a [`TaskManager`] is configured, the
//! tool registers a background task that issues the reverse RPC and returns
//! immediately with the `task_id`. The model can continue working and will be
//! notified when the user answers. When no `TaskManager` is configured (tests,
//! headless hosts), the tool falls back to blocking mode regardless of the
//! flag.

use std::future::Future;
use std::sync::Arc;

use serde_json::{Value, json};

use crate::error::EngineResult;
use crate::reverse_rpc::{ReverseRpcCallback, ReverseRpcSlot, is_channel_error};
use crate::session::gates::{QuestionSnapshot, now_ms, question_texts};
use crate::session::interaction::InteractionKernel;
use crate::task::{TaskKind, TaskManager};
use crate::task_notification::{DISMISSED_INSTRUCTION, DISMISSED_NOTE};
use crate::tool::{
    ExecutionPlan, Tool, ToolErrorResult, ToolExecution, ToolFuture, ToolInputDisplay, ToolOutput,
    ToolResult,
};

const ASK_USER_TOOL_NAME: &str = "AskUserQuestion";

const ASK_USER_DESCRIPTION: &str = "\
Ask the user a question with structured options. Use when you need user input \
to proceed — clarifying requirements, choosing between approaches, or getting \
confirmation. The question is presented to the user with the provided options; \
their answer is returned as the tool result. The user always has an 'Other' \
option for custom input. Set background=true to ask in the background \
and return immediately with a task_id; you are notified automatically when the \
user answers — do not poll with TaskOutput while the question is pending. \
If the user dismisses the question without answering, the tool returns a \
dismissal result with empty answers and the turn ends: do not proceed on \
assumptions or guesses, do not re-ask the same question, end your current \
output and wait for the user's next instruction.";

const ASK_USER_PARAMETERS: &str = r#"{
  "type": "object",
  "properties": {
    "questions": {
      "type": "array",
      "minItems": 1,
      "maxItems": 4,
      "items": {
        "type": "object",
        "properties": {
          "question": { "type": "string", "minLength": 1 },
          "header": { "type": "string" },
          "options": {
            "type": "array",
            "minItems": 2,
            "maxItems": 4,
            "items": {
              "type": "object",
              "properties": {
                "label": { "type": "string", "minLength": 1 },
                "description": { "type": "string" }
              },
              "required": ["label"]
            }
          },
          "multi_select": { "type": "boolean", "default": false }
        },
        "required": ["question"]
      }
    },
    "background": {
      "type": "boolean",
      "default": false,
      "description": "Set true to ask in the background and return immediately with a task_id; you are notified automatically when the user answers."
    }
  },
  "required": ["questions"]
}"#;

/// Liveness tick interval while a blocking ask waits for the user's answer
/// (see [`drive_ask_with_keepalive`]). A fraction of the engine's 600s stall
/// window — several ticks fit inside one window, so a single dropped tick
/// never lets the deadline lapse.
///
/// Scope: questions only. The `approval.ask` wait in `turn_flow/hooks.rs`
/// shares the same stall exposure but is deliberately NOT covered here —
/// it is a separately-scoped sibling path.
const ASK_KEEPALIVE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(75);

/// Default question-channel wait ceiling (ADR-008 / D-4): 5 minutes. See
/// [`AskUserQuestionTool::with_question_channel_wait`]. Derived from the
/// config-side constant ([`crate::config::DEFAULT_QUESTION_CHANNEL_WAIT_SECS`])
/// — the single source of truth.
const DEFAULT_QUESTION_CHANNEL_WAIT: std::time::Duration =
    std::time::Duration::from_secs(crate::config::DEFAULT_QUESTION_CHANNEL_WAIT_SECS);

/// How long the question delivery loop waits between retries after a
/// channel-class failure (ADR-008 / D-2). A fraction of the 300s ceiling so
/// a reconnecting client is picked up promptly without busy-looping the
/// slot read; the background path's TaskStop poll rides on the same period
/// (zero additional wake sources).
const QUESTION_DELIVERY_RETRY_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);

/// The error text returned when the question-channel wait ceiling expires
/// (ADR-008 / D-3): a distinguishable "channel unavailable" outcome. The
/// wording deliberately avoids the word "dismissed" so it can never trip
/// the ADR-007 re-ask guard (which keys on dismissal wording), and it
/// directs the model to the plain-text fallback.
fn question_channel_unavailable_error(wait_secs: u64) -> String {
    format!(
        "question channel unavailable: waited {wait_secs}s for the client to reconnect; \
         ask in plain text or retry later"
    )
}

/// Drive one blocking ask to completion while emitting liveness ticks
/// through the tool call's live-update channel.
///
/// While the ask waits for the user's answer the engine produces NO live
/// events of its own (no LLM deltas, no tool starts) — to the
/// embedded-server stall watchdog the turn looks exactly like a wedged
/// engine. Each periodic tick surfaces as a `LiveEvent::ToolProgress`
/// (volatile, never replayed), which resets the turn's stall-watchdog
/// deadline, so a turn parked on the user's answer is never mistaken for a
/// stalled engine and killed after the inactivity window. The question
/// wait is indefinite by contract (the user answers whenever they answer);
/// only a real answer, dismissal, cancellation, or channel failure ends the
/// drive.
async fn drive_ask_with_keepalive<T>(
    ask: impl Future<Output = T>,
    progress: crate::tool::ToolProgressHandle,
) -> T {
    let mut ask = std::pin::pin!(ask);
    let mut ticker = tokio::time::interval(ASK_KEEPALIVE_INTERVAL);
    // The first tick of a fresh interval fires immediately — harmless (the
    // wait has just begun); Delay only keeps any later burst from firing
    // back-to-back after a scheduler hiccup.
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            result = &mut ask => return result,
            _ = ticker.tick() => {
                progress.send(crate::tool::ToolUpdate {
                    kind: crate::tool::ToolUpdateKind::Custom,
                    text: None,
                    percent: None,
                    custom_kind: Some("ask_user.waiting".to_string()),
                    custom_data: Some(json!({ "waitingFor": "user_answer" })),
                });
            }
        }
    }
}

/// The tool-input-validation rejection message for a verbatim re-ask of a
/// recently dismissed question (US8 不得原样重问 / ADR-007 重问护栏).
///
/// Mirrors the `QUESTION_UNSUPPORTED_FAILURE_MESSAGE` pattern — the
/// rejection names the failure and carries an explicit "do not" behavioral
/// guide (the TS precedent says "Do NOT call this tool again"; here:
/// don't re-ask verbatim, rephrase substantially if still needed, or
/// proceed per the dismissal instruction).
fn recently_dismissed_rejection(question_text: &str) -> String {
    format!(
        "You asked this exact question before and the user dismissed it \
         without answering: \"{question_text}\". Do NOT re-ask the same \
         question verbatim. If you still need an answer, rephrase the \
         question substantially or proceed as the dismissal instruction \
         directs — end your current output and wait for the user's next \
         input."
    )
}

/// `AskUserQuestion` — ask the user a structured question via reverse RPC.
///
/// The tool holds the session's [`ReverseRpcSlot`] — the mutable, shared
/// holder the transport installs a callback into when a client subscribes.
/// The callback is resolved at CALL time, not baked at construction: a
/// transport that subscribes after the agent was created still reaches the
/// tool (the create-time snapshot defect — a main agent whose session
/// predated the first subscribe — was the original bug). When the slot is
/// empty at call time (no interactive client attached — headless hosts,
/// cron-driven turns before any subscribe), the call fails fast with a
/// message instructing the model to fall back to a plain-text question.
///
/// When a [`TaskManager`] is configured via [`with_task_manager`](Self::with_task_manager),
/// the tool supports `background: true` — the question is registered as
/// a background task and the tool returns the `task_id` immediately so the
/// model can continue working.
pub struct AskUserQuestionTool {
    /// The session's reverse-RPC slot: resolved at call time so a late
    /// subscribe (after agent creation) is still honored.
    callback_slot: ReverseRpcSlot,
    /// 可选的后台任务管理器, 启用 background mode。
    task_manager: Option<Arc<TaskManager>>,
    /// 会话 ID, 用于注册后台任务。
    session_id: Option<String>,
    /// 可选的会话交互内核 (B4-a): 阻塞模式的问题先向 QuestionGate 注册
    /// (gate 是 pending 唯一事实源), `question.answer` / `question.dismiss`
    /// / `interaction.respond` 与 reverse-RPC 响应双通道收敛, 先到先得。
    interaction_kernel: Option<Arc<InteractionKernel>>,
    /// 问询投递等待上限 (ADR-008 / D-4): 通道断开后, 投递循环等待客户端
    /// 重连 (槽位重装回调) 的最长时间, 到期撤销 gate 注册并以可区分的
    /// 「通道不可用」错误收尾。种子自 `[interaction]` 配置段 (经
    /// `BuiltinToolServices`), 直构工具保持 300s 缺省。
    question_channel_wait: std::time::Duration,
}

impl AskUserQuestionTool {
    /// Create a new `AskUserQuestion` tool bound to the given reverse-RPC
    /// callback slot.
    pub fn new(callback_slot: ReverseRpcSlot) -> Self {
        Self {
            callback_slot,
            task_manager: None,
            session_id: None,
            interaction_kernel: None,
            question_channel_wait: DEFAULT_QUESTION_CHANNEL_WAIT,
        }
    }

    /// 绑定会话交互内核 (B4-a): 阻塞模式的问题注册进 QuestionGate,
    /// 新 RPC 通道 (`question.answer` 等) 可直接回答。
    pub fn with_interaction_kernel(mut self, kernel: Option<Arc<InteractionKernel>>) -> Self {
        self.interaction_kernel = kernel;
        self
    }

    /// 设置问询投递等待上限 (ADR-008 / D-4): 通道断开后投递循环等待
    /// 客户端重连的最长时间。种子自 `[interaction]` 配置段
    /// (`questionChannelWaitSecs`, 缺省 300s), 经 `BuiltinToolServices`
    /// 在工具注册时注入。
    pub fn with_question_channel_wait(mut self, wait: std::time::Duration) -> Self {
        self.question_channel_wait = wait;
        self
    }

    /// 配置后台任务管理器, 启用 `background` 模式。
    ///
    /// 当 LLM 传入 `background: true` 时, 问题将作为后台任务注册,
    /// 工具立即返回 `task_id`, 用户回答后通过后台任务完成通知送达。
    pub fn with_task_manager(mut self, tasks: Arc<TaskManager>, session_id: String) -> Self {
        self.task_manager = Some(tasks);
        self.session_id = Some(session_id);
        self
    }

    /// Resolve the current callback from the slot, or explain the fallback
    /// when no interactive client is attached.
    fn resolve_callback(&self) -> Result<Arc<dyn ReverseRpcCallback>, String> {
        self.callback_slot
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .ok_or_else(|| {
                "no interactive client is attached to this session, so a structured \
                 question cannot be asked right now; ask the user in plain assistant \
                 text instead"
                    .to_string()
            })
    }
}

/// The outcome of one question-delivery loop pass (ADR-008 / D-2).
enum DeliverOutcome {
    /// The reverse-RPC delivery completed: `Ok` carries the client's
    /// response (record it into the gate), `Err` carries a BUSINESS-class
    /// delivery error (the ask was refused — already withdrawn from the
    /// gate by the loop).
    Delivered(Result<Value, String>),
    /// The question was answered / dismissed through the gate channel
    /// (question.answer / interaction.respond / question.dismiss) while
    /// the delivery loop was still waiting: `Ok` is the answer text,
    /// `Err` is a dismissal/withdrawal.
    GateResolved(Result<String, ()>),
    /// The wait ceiling expired: the channel never recovered in time.
    /// The gate registration has been withdrawn.
    TimedOut,
    /// The caller cancelled (the blocking path's `ctx.cancel`): the gate
    /// registration has been withdrawn; settle as a cancellation.
    Cancelled,
    /// The background task this ask rides on was stopped (TaskStop, D-6):
    /// the gate registration has been withdrawn; settle the task
    /// cancelled.
    TaskStopped,
}

/// How one delivery attempt classified (ADR-008 / D-1).
enum AttemptOutcome {
    /// The client responded (the response may itself be a JSON-RPC
    /// error-shaped value, but the DELIVERY succeeded).
    Response(Value),
    /// A channel-class failure: the connection is gone — retry after the
    /// interval (a re-subscribe re-installs a fresh callback).
    ChannelError,
    /// A business-class refusal (e.g. "not negotiated"): retrying is
    /// pointless — withdraw and fail.
    BusinessError(String),
}

/// The shared core of the question-delivery loop (ADR-008): deliver the
/// `question.ask` reverse RPC with reconnect resilience.
///
/// Every iteration resolves the CURRENT callback from the shared slot (a
/// transport re-subscribe re-installs a fresh one — the whole point of the
/// resilience), then races the delivery against the gate receiver and the
/// caller's cancel token (blocking path only).
///
/// Delivery error classification (D-1):
/// - CHANNEL-class ([`is_channel_error`], e.g. a revoked teardown or a
///   dead outbox): the connection broke — the wait-ceiling clock STARTS
///   (it is a ceiling on the broken-channel wait, not on the answer
///   wait). After one retry interval the loop re-resolves the slot. The
///   gate registration STAYS registered so `question.answer` can still
///   settle the ask through the gate channel while the loop waits
///   (first-one-wins convergence holds inside the wait loop).
/// - BUSINESS-class (e.g. "not negotiated"): the request itself was
///   refused — withdraw the gate registration (no orphan pending) and
///   return the error, mirroring the pre-resilience behavior.
///
/// The wait ceiling measures the TOTAL broken-channel wait (D-2): the
/// clock starts when the loop FIRST observes a channel-class failure, and
/// covers every retry sleep until the channel recovers (or the ceiling
/// lapses — then the gate registration is withdrawn and
/// [`DeliverOutcome::TimedOut`] is returned so the caller surfaces the
/// distinguishable "channel unavailable" error). A HEALTHY in-flight
/// delivery attempt (the client is attached and simply has not answered
/// yet) is NOT on the clock: once the ask is delivered, the wait for the
/// user's answer is the pre-existing indefinite gate wait (ADR-008:
/// "投递成功 → 转入既有 gate 双通道等待" — the ceiling never bounds the
/// answer wait).
///
/// `task_status` is the background path's stop-poller (D-6): `Some`
/// carries the owning task manager + task id and checks
/// [`TaskManager::get`] every retry interval — a detached question
/// future observes no cancel signal, so a TaskStop only flips the task's
/// status; the poller sees `Stopped` and ends the wait within one retry
/// interval (500ms — far inside the 5-minute ceiling). The blocking
/// path passes `None` (its cancellation rides `cancel`).
///
/// A mid-loop channel-error wait resolves `None` (the callback future
/// was cancelled): the next iteration re-resolves the slot. An empty
/// slot mid-loop is likewise treated as a channel-class unavailability
/// (keep waiting; the never-attached fast-fail already happened at the
/// tool entry, before this loop).
/// The caller-side interruption arm of the delivery loop's selects: await
/// the blocking path's cancel token, OR — on the background path — poll
/// the owning task's status every retry interval (a TaskStop only flips
/// the status; the detached future never sees a signal, D-6), OR never
/// resolve when neither is armed.
async fn delivery_interrupt(
    cancel: Option<&crate::cancel::CancelToken>,
    task_status: Option<&(Arc<TaskManager>, String)>,
) {
    match (cancel, task_status) {
        (Some(cancel), _) => cancel.cancelled().await,
        (None, Some((tasks, task_id))) => loop {
            if matches!(
                tasks.get(task_id).map(|task| task.status),
                Some(crate::task::TaskStatus::Stopped)
            ) {
                return;
            }
            tokio::time::sleep(QUESTION_DELIVERY_RETRY_INTERVAL).await;
        },
        _ => std::future::pending().await,
    }
}

#[allow(clippy::too_many_arguments)]
async fn deliver_question_with_reconnect(
    callback_slot: ReverseRpcSlot,
    mut gate_rx: tokio::sync::oneshot::Receiver<String>,
    kernel: Arc<InteractionKernel>,
    request_id: uuid::Uuid,
    payload: Value,
    cancel: Option<crate::cancel::CancelToken>,
    task_status: Option<(Arc<TaskManager>, String)>,
    wait_ceiling: std::time::Duration,
) -> DeliverOutcome {
    // The broken-channel clock: `None` until the first channel-class
    // failure; once set, every retry sleep advances towards the deadline.
    let mut channel_deadline: Option<tokio::time::Instant> = None;
    loop {
        // Resolve the CURRENT callback — never a create-time snapshot
        // (the resilience contract).
        let callback = callback_slot
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        // One delivery attempt, classified by the error contract: the
        // future is raced (not awaited directly) so the gate / cancel
        // (and the stop poller on the retry path) branches can win while
        // it is in flight; an empty mid-loop slot never issues an RPC at
        // all (it is a channel-class unavailability by construction).
        let attempt = async {
            match callback.as_ref() {
                Some(callback) => match callback.call("question.ask", payload.clone()).await {
                    Ok(response) => AttemptOutcome::Response(response),
                    Err(err) if is_channel_error(&err) => AttemptOutcome::ChannelError,
                    Err(err) => AttemptOutcome::BusinessError(err),
                },
                None => AttemptOutcome::ChannelError,
            }
        };
        let mut attempt = std::pin::pin!(attempt);
        tokio::select! {
            classified = &mut attempt => match classified {
                AttemptOutcome::Response(response) => {
                    return DeliverOutcome::Delivered(Ok(response));
                }
                AttemptOutcome::BusinessError(err) => {
                    // A business-class refusal: withdraw (no orphan
                    // pending) and return the error — the
                    // pre-resilience fast-fail behavior, preserved.
                    kernel.question_gate().withdraw(request_id);
                    return DeliverOutcome::Delivered(Err(err));
                }
                // Channel-class: the connection is gone. Start (or
                // keep) the broken-channel clock and fall through to
                // the retry wait below.
                AttemptOutcome::ChannelError => {
                    channel_deadline.get_or_insert_with(tokio::time::Instant::now);
                }
            },
            answer = &mut gate_rx => {
                return match answer {
                    Ok(answer) => DeliverOutcome::GateResolved(Ok(answer)),
                    // The gate sender dropped: dismissed or withdrawn.
                    Err(_) => DeliverOutcome::GateResolved(Err(())),
                };
            }
            _ = delivery_interrupt(cancel.as_ref(), task_status.as_ref()) => {
                kernel.question_gate().withdraw(request_id);
                return if cancel.is_some() {
                    DeliverOutcome::Cancelled
                } else {
                    DeliverOutcome::TaskStopped
                };
            }
        }
        // Channel-class failure: wait one retry interval before
        // re-resolving the slot. While waiting, the gate receiver (an
        // early answer through question.answer must win immediately),
        // the deadline (the ceiling only counts broken-channel time),
        // and — for the background path — the task's stopped status
        // stay armed.
        let deadline = channel_deadline.expect("set on every ChannelError branch") + wait_ceiling;
        tokio::select! {
            _ = tokio::time::sleep(QUESTION_DELIVERY_RETRY_INTERVAL) => {}
            answer = &mut gate_rx => {
                return match answer {
                    Ok(answer) => DeliverOutcome::GateResolved(Ok(answer)),
                    Err(_) => DeliverOutcome::GateResolved(Err(())),
                };
            }
            _ = tokio::time::sleep_until(deadline) => {
                kernel.question_gate().withdraw(request_id);
                return DeliverOutcome::TimedOut;
            }
            _ = delivery_interrupt(cancel.as_ref(), task_status.as_ref()) => {
                kernel.question_gate().withdraw(request_id);
                return if cancel.is_some() {
                    DeliverOutcome::Cancelled
                } else {
                    DeliverOutcome::TaskStopped
                };
            }
        }
    }
}

impl Tool for AskUserQuestionTool {
    fn name(&self) -> &str {
        ASK_USER_TOOL_NAME
    }

    fn description(&self) -> &str {
        ASK_USER_DESCRIPTION
    }

    fn parameters(&self) -> &Value {
        static PARAMETERS: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
        PARAMETERS.get_or_init(|| serde_json::from_str(ASK_USER_PARAMETERS).unwrap())
    }

    fn resolve_execution(&self, input: Value) -> EngineResult<ToolExecution> {
        // 重问护栏 (US8 / ADR-007): 在同一会话内, 对刚被用户关闭的问题
        // 原样重问在工具入参校验层直接拒绝 — 不注册新 pending, 不打扰用户,
        // 回附行为指引 (rephrase or proceed per the dismissal instruction),
        // 与 QUESTION_UNSUPPORTED_FAILURE_MESSAGE 的 "Do NOT call this tool
        // again" 模式同构。只有 dismissed 的问题会命中 (answered 的不会),
        // 且必须是逐字相同 (原样重问) — 实质性改写后重问仍然放行。
        if let Some(kernel) = &self.interaction_kernel {
            for text in question_texts(input.get("questions").unwrap_or(&Value::Null)) {
                if kernel.question_gate().was_recently_dismissed(&text) {
                    return Ok(ToolExecution::Error(ToolErrorResult {
                        message: recently_dismissed_rejection(&text),
                    }));
                }
            }
        }

        // Resolve the callback at CALL time, not construction time: the
        // slot may have been filled by a transport subscribe that happened
        // after this tool (and its owning agent) was created. ADR-008: the
        // snapshot here is used ONLY for the never-attached fast-fail gate
        // (an empty slot means no interactive client ever subscribed —
        // fail fast, no 5-minute wait); the actual delivery re-resolves
        // the slot on every attempt inside the delivery loop.
        if self.resolve_callback().is_err() {
            // No interactive client attached — fail fast so the model
            // falls back to a plain-text question instead of blocking
            // (SDR-006 scope boundary: the resilience wait presupposes
            // the user was once present; never-attached stays fast).
            return Ok(ToolExecution::Runnable {
                plan: ExecutionPlan {
                    accesses: Vec::new(),
                    display: ToolInputDisplay::Generic {
                        summary: "Ask user a question (no client attached)".to_string(),
                    },
                    approval_rule: "AskUserQuestion".to_string(),
                    stop_batch_after_this: false,
                },
                execute: Box::new(move |_ctx| {
                    Box::pin(async move {
                        ToolResult::error(
                            "no interactive client is attached to this session, so a structured \
                             question cannot be asked right now; ask the user in plain assistant \
                             text instead"
                                .to_string(),
                        )
                    }) as ToolFuture
                }),
            });
        }

        // 提取 background 标志 (默认 false)。
        let background = input
            .get("background")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        // 当请求 background mode 且配置了 TaskManager 时, 走后台路径。
        if background {
            if let (Some(tasks), Some(session_id)) =
                (self.task_manager.as_ref(), self.session_id.as_ref())
            {
                let tasks = Arc::clone(tasks);
                let session_id = session_id.clone();
                let description = question_description(&input);
                let question_count = question_count(&input);
                let interaction_kernel = self.interaction_kernel.clone();
                let callback_slot = Arc::clone(&self.callback_slot);
                let question_channel_wait = self.question_channel_wait;
                // 后台路径同样注入 allow_other: true — 与阻塞路径共用同一
                // 源头 (enrich_question_input), 保证标志恒在。
                let ask_payload = enrich_question_input(&input);

                return Ok(ToolExecution::Runnable {
                    plan: ExecutionPlan {
                        accesses: Vec::new(),
                        display: ToolInputDisplay::Generic {
                            summary: format!("Background question: {description}"),
                        },
                        approval_rule: "AskUserQuestion".to_string(),
                        stop_batch_after_this: false,
                    },
                    execute: Box::new(move |ctx| {
                        let callback_slot = Arc::clone(&callback_slot);
                        let question_channel_wait = question_channel_wait;
                        let session_id = session_id.clone();
                        let description = description.clone();
                        let question_count = question_count;
                        let interaction_kernel = interaction_kernel.clone();
                        Box::pin(async move {
                            // 后台路径同样把问题登记进 QuestionGate (B4-a): gate
                            // 是 pending 唯一事实源, `question.dismiss` (web 关闭
                            // 面板) 与 reverse-RPC 响应双通道收敛到同一 oneshot
                            // (先到先得, SRV question.dismiss 只认 gate 里注册的
                            // id)。阻塞路径已这样注册; 后台任务此前只挂 callback,
                            // 面板关闭时 dismiss 只能落在 gate 上 — 不注册则永远
                            // 以为「仍在等待」, 用户关闭也投递不到取消语义。
                            let gate = interaction_kernel.as_ref().map(|kernel| {
                                let snapshot = QuestionSnapshot {
                                    questions: input
                                        .get("questions")
                                        .cloned()
                                        .unwrap_or(serde_json::Value::Array(Vec::new())),
                                    agent_id: uuid::Uuid::nil(),
                                    turn_id: ctx.turn_id.parse::<u64>().ok(),
                                    tool_call_id: Some(ctx.tool_call_id.clone()),
                                    created_at: now_ms(),
                                };
                                let (request_id, receiver) =
                                    kernel.question_gate().register(snapshot);
                                (kernel.clone(), request_id, receiver)
                            });
                            // 问题任务经 register_background 计入容量（TS
                            // assertCanRegister 统计所有 detached 条目），但不
                            // 挂信号：问题的 future 无法观察取消（客户端没有
                            // 挂起问题的取消通道），挂了信号只会让 stop 路径
                            // 白烧整个 kill-grace 窗口——无信号注册让
                            // stop_with_grace 走即时升级捷径（纯状态翻转）。
                            let tasks_for_closure = Arc::clone(&tasks);
                            let task_id = match tasks.spawn_background_detached(
                                TaskKind::Question { question_count },
                                description,
                                session_id,
                                move |task_id| {
                                    let tasks = tasks_for_closure;
                                    let task_id = task_id.clone();
                                    Box::pin(async move {
                                        // 投递循环 + 等待环 (ADR-008 / D-2, D-3,
                                        // D-6): gate 注册提前到循环外, 同一
                                        // request_id 贯穿全程; 每次迭代从槽位
                                        // 重新解析回调, 通道断开则 500ms 后重试,
                                        // 等待 TaskStop 经状态轮询发现 (detached
                                        // future 无信号可观察)。投递成功后转入
                                        // 既有「双通道收敛」等待 — 结构与原路径
                                        // 一致, 但投递期间不再撤销 gate。
                                        let response = match gate {
                                            Some((kernel, request_id, receiver)) => {
                                                let outcome = deliver_question_with_reconnect(
                                                    Arc::clone(&callback_slot),
                                                    receiver,
                                                    kernel.clone(),
                                                    request_id,
                                                    question_ask_payload(&input, request_id),
                                                    // 后台路径无 ctx.cancel (D-2): 退出
                                                    // 条件为投递成功 / gate 通道 / 上限 /
                                                    // 任务状态轮询发现 Stopped。
                                                    None,
                                                    Some((tasks.clone(), task_id.clone())),
                                                    question_channel_wait,
                                                )
                                                .await;
                                                match outcome {
                                                    DeliverOutcome::Delivered(Ok(resp)) => {
                                                        // reverse-RPC 响应先到: 记入
                                                        // gate (迟到的 dismiss 得到
                                                        // not-found), 再走原有映射。
                                                        let text =
                                                            serde_json::to_string_pretty(&resp)
                                                                .unwrap_or_else(|_| {
                                                                    "question answered".to_string()
                                                                });
                                                        let _ = kernel
                                                            .question_gate()
                                                            .answer(request_id, text);
                                                        Ok(resp)
                                                    }
                                                    DeliverOutcome::Delivered(Err(err)) => Err(err),
                                                    DeliverOutcome::GateResolved(answer) => {
                                                        // 新 RPC 通道先到: 答案直接
                                                        // 用作任务输出; Err 即
                                                        // dismiss — 任务以取消定局,
                                                        // 取消通知携带与阻塞路径相同
                                                        // 的行为指令 (SDR-005 (c) /
                                                        // ADR-007 后台变体)。
                                                        return match answer {
                                                            Ok(answer) => {
                                                                Ok((0, answer, String::new()))
                                                            }
                                                            Err(()) => {
                                                                let text =
                                                                    dismissed_notification_text();
                                                                tasks
                                                                    .settle_background_cancelled(
                                                                        &task_id, &text,
                                                                    )
                                                                    .await;
                                                                Ok((
                                                                    1,
                                                                    String::new(),
                                                                    String::new(),
                                                                ))
                                                            }
                                                        };
                                                    }
                                                    DeliverOutcome::TimedOut => {
                                                        // 上限到期: gate 已被循环撤销,
                                                        // 任务以通道不可用错误定局
                                                        // (可区分文案, 不触发关闭护栏)。
                                                        Err(question_channel_unavailable_error(
                                                            question_channel_wait.as_secs(),
                                                        ))
                                                    }
                                                    DeliverOutcome::TaskStopped => {
                                                        // TaskStop (D-6): gate 已撤销,
                                                        // 任务以取消定局 (settle 让
                                                        // stop 翻转的 Stopped 状态
                                                        // 保持 — first-terminal-wins)。
                                                        let text =
                                                            "question cancelled by task stop";
                                                        tasks
                                                            .settle_background_cancelled(
                                                                &task_id, text,
                                                            )
                                                            .await;
                                                        return Ok((
                                                            1,
                                                            String::new(),
                                                            String::new(),
                                                        ));
                                                    }
                                                    // 阻塞路径专属 (cancel 传入 None,
                                                    // 此分支在后台路径不可达)。
                                                    DeliverOutcome::Cancelled => unreachable!(
                                                        "background delivery has no cancel token"
                                                    ),
                                                }
                                            }
                                            // 裸分支保持现状 (ADR-008 范围边界):
                                            // 无 gate 内核的路径 (测试直构) 维持
                                            // 单发投递语义, 不进等待环。槽位中途
                                            // 变空不作 panic 不变量 — 以通道类
                                            // 错误串收尾 (经 CHANNEL_ERROR_PREFIX
                                            // 构造, 符合 D-1 错误契约)。
                                            None => {
                                                let callback = callback_slot
                                                    .read()
                                                    .unwrap_or_else(|e| e.into_inner())
                                                    .clone();
                                                match callback {
                                                    Some(callback) => {
                                                        callback
                                                            .call("question.ask", ask_payload)
                                                            .await
                                                    }
                                                    None => Err(format!(
                                                        "{}connection closed",
                                                        crate::reverse_rpc::CHANNEL_ERROR_PREFIX
                                                    )),
                                                }
                                            }
                                        };
                                        match response {
                                            Ok(resp) => {
                                                let text = serde_json::to_string_pretty(&resp)
                                                    .unwrap_or_else(|_| {
                                                        "question answered".to_string()
                                                    });
                                                Ok((0, text, String::new()))
                                            }
                                            Err(err) => {
                                                Err(anyhow::anyhow!("question.ask failed: {err}"))
                                            }
                                        }
                                    })
                                },
                            ) {
                                Ok(task_id) => task_id,
                                Err(message) => return ToolResult::error(message),
                            };

                            // 立即返回 task_id, 不阻塞等待用户回答。
                            ToolResult::success(format!(
                                "task_id: {task_id}\n\
                                 status: running\n\
                                 automatic_notification: true\n\
                                 next_step: Continue your current work; \
                                 the answer will arrive automatically \
                                 when the user responds.\n\
                                 next_step: Use TaskOutput with this \
                                 task_id for a non-blocking status/answer \
                                 snapshot.\n\
                                 next_step: Use TaskStop only if the \
                                 question should be cancelled."
                            ))
                        }) as ToolFuture
                    }),
                });
            }

            // 没有配置 TaskManager — 降级为阻塞模式并附加提示。
            tracing::warn!(
                "background requested but no TaskManager configured; \
                 falling back to blocking mode"
            );
        }

        // 阻塞模式: 等待用户回答后返回。
        let interaction_kernel = self.interaction_kernel.clone();
        let callback_slot = Arc::clone(&self.callback_slot);
        let question_channel_wait = self.question_channel_wait;
        Ok(ToolExecution::Runnable {
            plan: ExecutionPlan {
                accesses: Vec::new(),
                display: ToolInputDisplay::Generic {
                    summary: "Ask user a question".to_string(),
                },
                approval_rule: "AskUserQuestion".to_string(),
                stop_batch_after_this: false,
            },
            execute: Box::new(move |ctx| {
                let callback_slot = Arc::clone(&callback_slot);
                let interaction_kernel = interaction_kernel.clone();
                // ADR-006: the gate wait registers its WaitingGate entry
                // when the interaction kernel is wired — the watchdog
                // pulls the kernel's pending list at window expiry (a
                // pending user decision is alive by design; an empty
                // list means the ask settled and the waiter should
                // return). Without a kernel the keepalive below remains
                // the only feed (the reverse-RPC path never registered a
                // gate — the pull model has no fact source there).
                let _gate_wait = ctx.wait_registry.as_ref().and_then(|registry| {
                    interaction_kernel.clone().map(|kernel| {
                        registry.enter(
                            crate::turn_flow::WaitKind::WaitingGate,
                            crate::turn_flow::WaitSubject::Gate { kernel },
                        )
                    })
                });
                // Stall-watchdog feed (同 agent_tool 的前台子代理防误杀):
                // 等待用户回答期间引擎自身不产生任何 live 事件, 周期性
                // liveness tick 经由 progress 通道重置回合的 stall deadline。
                let keepalive = ctx.progress.clone();
                Box::pin(drive_ask_with_keepalive(
                    async move {
                        // B4-a: 先向 QuestionGate 注册 (gate 是 pending 唯一
                        // 事实源), ask payload 携带 gate 铸造的 id (增量字段,
                        // 既有 reverse-RPC 消费方可忽略)。两条响应通道
                        // (reverse-RPC 响应、question.answer / interaction.respond
                        // 新 RPC) 收敛到同一 oneshot, 先到先得; 无内核时保持
                        // 逐字节兼容的纯 reverse-RPC 路径。
                        let gate = interaction_kernel.as_ref().map(|kernel| {
                            let snapshot = QuestionSnapshot {
                                questions: input
                                    .get("questions")
                                    .cloned()
                                    .unwrap_or(serde_json::Value::Array(Vec::new())),
                                agent_id: uuid::Uuid::nil(),
                                turn_id: ctx.turn_id.parse::<u64>().ok(),
                                tool_call_id: Some(ctx.tool_call_id.clone()),
                                created_at: now_ms(),
                            };
                            let (request_id, receiver) = kernel.question_gate().register(snapshot);
                            (kernel.clone(), request_id, receiver)
                        });
                        let response = match gate {
                            Some((kernel, request_id, receiver)) => {
                                // ADR-008 / D-2, D-3: gate 注册已在上方完成
                                // (循环外, 一次性铸造 request_id), payload 全程
                                // 复用同一 id。投递循环内联重试: 通道断开等
                                // 重连, gate receiver 与 ctx.cancel 并联 —
                                // 等待环期间用户经 question.answer 先行作答
                                // 直接收尾, 取消沿既有语义收尾。
                                let outcome = deliver_question_with_reconnect(
                                    Arc::clone(&callback_slot),
                                    receiver,
                                    kernel.clone(),
                                    request_id,
                                    question_ask_payload(&input, request_id),
                                    Some(ctx.cancel.clone()),
                                    None,
                                    question_channel_wait,
                                )
                                .await;
                                match outcome {
                                    DeliverOutcome::Delivered(r) => {
                                        // reverse-RPC 响应先到: 记入 gate (迟到的新
                                        // RPC 回答得到 not-found), 再走原有映射。
                                        if let Ok(resp) = &r {
                                            let text = serde_json::to_string_pretty(resp)
                                                .unwrap_or_else(|_| {
                                                    "question answered".to_string()
                                                });
                                            let _ = kernel.question_gate().answer(request_id, text);
                                        }
                                        r
                                    }
                                    DeliverOutcome::GateResolved(answer) => {
                                        // 新 RPC 通道先到 (question.answer /
                                        // interaction.respond) 或被 dismiss/撤销。
                                        return match answer {
                                            Ok(answer) => ToolResult::success(
                                                serde_json::to_string_pretty(&answer)
                                                    .unwrap_or(answer),
                                            ),
                                            // 用户直接关闭问询面板: 不是错误 — 引擎侧
                                            // 强制终止回合 (SDR-005 / ADR-007), 输入权
                                            // 归还用户, 不依赖模型自觉继续或猜测意图。
                                            Err(()) => dismissed_result(),
                                        };
                                    }
                                    DeliverOutcome::TimedOut => {
                                        // 上限到期: gate 已被循环撤销, 以可区分的
                                        // 「通道不可用」错误收尾并附降级指引
                                        // (D-3; 文案不写入 dismissed ring — 护栏
                                        // 只由用户 dismiss 写入)。
                                        return ToolResult::error(
                                            question_channel_unavailable_error(
                                                question_channel_wait.as_secs(),
                                            ),
                                        );
                                    }
                                    DeliverOutcome::Cancelled => {
                                        // ctx.cancel: 沿既有取消语义收尾
                                        // (withdraw 已由循环完成), 非通道不可用。
                                        return ToolResult::error("cancelled by user");
                                    }
                                    // 后台路径专属 (task_status 传入 None, 阻塞
                                    // 路径不可达)。
                                    DeliverOutcome::TaskStopped => {
                                        unreachable!("blocking delivery has no task to poll")
                                    }
                                }
                            }
                            None => tokio::select! {
                                // 无 gate 内核的裸路径 (测试直构): 无 pending
                                // 事实源, 保持单发语义 — 槽位在解析期非空
                                // (resolve_execution 已验), 此处中途变空视作
                                // 通道失败 (never-attached 语义的诚实回退)。
                                r = async {
                                    let callback = callback_slot
                                        .read()
                                        .unwrap_or_else(|e| e.into_inner())
                                        .clone();
                                    match callback {
                                        Some(callback) => callback
                                            .call("question.ask", enrich_question_input(&input))
                                            .await,
                                        None => Err(format!(
                                            "{}connection closed",
                                            crate::reverse_rpc::CHANNEL_ERROR_PREFIX
                                        )),
                                    }
                                } => r,
                                _ = ctx.cancel.cancelled() => return ToolResult::error("cancelled by user"),
                            },
                        };
                        match response {
                            Ok(response) => {
                                let text = serde_json::to_string_pretty(&response)
                                    .unwrap_or_else(|_| "question answered".to_string());
                                ToolResult::success(text)
                            }
                            Err(err) => ToolResult::error(format!("question.ask failed: {err}")),
                        }
                    },
                    keepalive,
                )) as ToolFuture
            }),
        })
    }
}

/// 构造携带 gate 铸造的 request id 的 `question.ask` payload (B4-a: id 是
/// 增量字段, 既有消费方可忽略)。
fn question_ask_payload(input: &Value, request_id: uuid::Uuid) -> Value {
    let mut payload = enrich_question_input(input);
    payload["request_id"] = serde_json::json!(request_id.to_string());
    payload
}

/// 为 `question.ask` payload 的每个问题项注入 `allow_other: true` (增量
/// 字段): 工具描述承诺 "The user always has an 'Other' option", TS 引擎也总在
/// 线上 advertise 自由文本 Other — 引擎在源头统一置位, 所有客户端一致受益。
/// 非对象项原样保留 (防御性)。
fn enrich_question_input(input: &Value) -> Value {
    let mut payload = input.clone();
    if let Some(questions) = payload.get_mut("questions").and_then(Value::as_array_mut) {
        for question in questions.iter_mut() {
            if let Some(obj) = question.as_object_mut() {
                obj.insert("allow_other".to_string(), serde_json::Value::Bool(true));
            }
        }
    }
    payload
}

/// 构造问询被用户关闭 (dismiss) 时回传模型的结构化工具结果 (SDR-005 /
/// ADR-007)。
///
/// 关闭是意图不明的动作 — 它可能意味着「稍后再说」「问得不对」「想自己
/// 打字」或「误触」, 唯独不是「别问了你自己定」(后者只有 auto 模式的显式
/// 拒绝通道表达)。因此回传三要素: JSON 输出携带空的 `answers` 映射、说明
/// 用户未作答的 `note`、以及行为指令 `instruction` — 不要基于猜测继续,
/// 结束本轮输出, 等待用户主动输入; 且不得原样重问刚被关闭的问题。
///
/// 与普通成功/错误结果不同, 该结果置 `stop_turn: true`: 引擎在工具调用
/// 所在的回合边界强制终止 (loop_engine 消费该标志, 与「审批拒绝并终止」
/// 同构), 不依赖模型自觉。`is_error` 保持 `false` — 关闭不是工具失败,
/// 是用户的主动结束信号。
fn dismissed_result() -> ToolResult {
    ToolResult {
        output: ToolOutput::text(dismissed_notification_text()),
        is_error: false,
        note: Some(DISMISSED_NOTE.to_string()),
        stop_turn: true,
        truncated: false,
        injection: None,
    }
}

/// 用户关闭问询时的结构化输出文本 (空 `answers` + note + instruction)。
///
/// 前台路径把它作为工具结果回传模型; 后台路径把它作为任务输出写入, 取消
/// 通知的 body 携带同样的行为指令 (常量在 [`crate::task_notification`] 单一
/// 定义) — 发起代理从通知读到与阻塞路径一字不差的行为指令 (SDR-005 (c) /
/// ADR-007 后台变体)。
fn dismissed_notification_text() -> String {
    serde_json::to_string_pretty(&json!({
        "answers": {},
        "note": DISMISSED_NOTE,
        "instruction": DISMISSED_INSTRUCTION,
    }))
    .expect("dismissal payload is serializable")
}

/// 从工具输入中提取问题数量, 用于后台任务的 question kind 快照。
fn question_count(input: &Value) -> u32 {
    input
        .get("questions")
        .and_then(Value::as_array)
        .map_or(0, |questions| questions.len() as u32)
}

/// 从工具输入中提取问题的简短描述, 用于后台任务的 description 字段。
fn question_description(input: &Value) -> String {
    if let Some(questions) = input.get("questions").and_then(Value::as_array)
        && let Some(first) = questions.first()
        && let Some(q) = first.get("question").and_then(Value::as_str)
    {
        let trimmed = q.trim();
        if !trimmed.is_empty() {
            if questions.len() > 1 {
                return format!("{trimmed} (+{} more)", questions.len() - 1);
            }
            return trimmed.to_string();
        }
    }
    "Ask user question".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cancel::CancelToken;
    use crate::session::gates::{ApprovalGate, QuestionGate, QuestionSnapshot};
    use crate::session::interaction::InteractionKernel;
    use crate::tool::{ExecutableToolContext, ToolOutput};
    use serde_json::json;
    use std::sync::Mutex;
    use uuid::Uuid;

    /// How a [`GateResolvingCallback`] resolves the pending question.
    #[derive(Clone)]
    enum GateResolveMode {
        Answer(String),
        Dismiss,
    }

    /// A callback that, on `question.ask`, resolves the pending question
    /// through the shared question gate (answer or dismiss the minted
    /// `request_id`) and then never completes the reverse-RPC reply — the
    /// gate channel must win the race.
    struct GateResolvingCallback {
        gate: Arc<QuestionGate>,
        mode: GateResolveMode,
    }

    #[async_trait::async_trait]
    impl ReverseRpcCallback for GateResolvingCallback {
        async fn call(&self, _method: &str, params: Value) -> Result<Value, String> {
            let request_id = params["request_id"]
                .as_str()
                .and_then(|raw| Uuid::parse_str(raw).ok())
                .expect("question.ask payload must carry the gate-minted request_id");
            match &self.mode {
                GateResolveMode::Answer(answer) => {
                    assert!(
                        self.gate.answer(request_id, answer.clone()),
                        "pending question must be answered"
                    );
                }
                GateResolveMode::Dismiss => {
                    assert!(
                        self.gate.dismiss(request_id),
                        "pending question must be dismissed"
                    );
                }
            }
            // Never complete the reverse-RPC reply: the gate channel must
            // resolve the awaiting ask first.
            std::future::pending().await
        }
    }

    /// A mock [`ReverseRpcCallback`] that records calls and returns a canned
    /// response.
    struct MockCallback {
        response: serde_json::Value,
        last_method: Mutex<Option<String>>,
        last_params: Mutex<Option<serde_json::Value>>,
    }

    #[async_trait::async_trait]
    impl ReverseRpcCallback for MockCallback {
        async fn call(&self, method: &str, params: Value) -> Result<Value, String> {
            *self.last_method.lock().unwrap() = Some(method.to_string());
            *self.last_params.lock().unwrap() = Some(params);
            Ok(self.response.clone())
        }
    }

    /// An empty [`ReverseRpcSlot`] — no callback installed.
    fn empty_slot() -> ReverseRpcSlot {
        Arc::new(std::sync::RwLock::new(None))
    }

    /// A slot with the given callback pre-installed.
    fn slot_with(callback: Arc<dyn ReverseRpcCallback>) -> ReverseRpcSlot {
        Arc::new(std::sync::RwLock::new(Some(callback)))
    }

    /// A callback whose calls fail with a CHANNEL-class error (the
    /// ADR-008 / D-1 signature — what a torn-down WS connection
    /// returns). Optionally counts the attempts so tests can assert the
    /// retry cadence.
    struct BrokenChannelCallback {
        attempts: std::sync::atomic::AtomicUsize,
    }

    impl BrokenChannelCallback {
        fn new() -> Self {
            Self {
                attempts: std::sync::atomic::AtomicUsize::new(0),
            }
        }

        fn attempts(&self) -> usize {
            self.attempts.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl ReverseRpcCallback for BrokenChannelCallback {
        async fn call(&self, _method: &str, _params: Value) -> Result<Value, String> {
            self.attempts
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Err(format!(
                "{}connection closed",
                crate::reverse_rpc::CHANNEL_ERROR_PREFIX
            ))
        }
    }

    /// A callback that fails with a BUSINESS-class error (no channel
    /// prefix) — e.g. the client did not negotiate the question domain.
    struct BusinessErrorCallback;

    #[async_trait::async_trait]
    impl ReverseRpcCallback for BusinessErrorCallback {
        async fn call(&self, _method: &str, _params: Value) -> Result<Value, String> {
            Err(
                "reverse RPC 'question.ask' was not negotiated by the client (allowed: [])"
                    .to_string(),
            )
        }
    }

    /// 构建测试用的 ExecutableToolContext。
    fn make_ctx() -> ExecutableToolContext {
        ExecutableToolContext {
            approval_metadata: None,
            wait_registry: None,
            turn_id: "t1".to_string(),
            tool_call_id: "c1".to_string(),
            cancel: CancelToken::new(),
            progress: crate::tool::ToolProgressHandle::default(),
        }
    }

    /// Given an `AskUserQuestion` tool with a mock callback, when the LLM
    /// invokes it, then the tool issues a `question.ask` reverse RPC and
    /// returns the response as a successful tool result.
    #[tokio::test]
    async fn should_issue_question_ask_reverse_rpc_and_return_response_as_tool_result() {
        let callback = Arc::new(MockCallback {
            response: json!({ "answers": [{ "question": "Which?", "answer": "A" }] }),
            last_method: Mutex::new(None),
            last_params: Mutex::new(None),
        });
        let tool = AskUserQuestionTool::new(slot_with(callback.clone()));
        let input = json!({
            "questions": [{
                "question": "Which approach?",
                "options": [
                    { "label": "A", "description": "Option A" },
                    { "label": "B", "description": "Option B" },
                ],
            }],
        });

        let execution = tool.resolve_execution(input.clone()).unwrap();
        match execution {
            ToolExecution::Runnable { plan, execute } => {
                assert_eq!(plan.approval_rule, "AskUserQuestion");
                let ctx = make_ctx();
                let result = execute(ctx).await;
                assert!(!result.is_error);
                let text = match result.output {
                    ToolOutput::Text(t) => t,
                    _ => panic!("expected text output"),
                };
                assert!(text.contains("Which?"));
                // The callback received the right method and params: the
                // original questions plus `allow_other: true` on each item
                // (the engine always advertises the free-text Other option).
                let method = callback.last_method.lock().unwrap().clone();
                assert_eq!(method.as_deref(), Some("question.ask"));
                let params = callback.last_params.lock().unwrap().clone();
                let params = params.expect("callback params");
                assert_eq!(
                    params["questions"][0]["question"],
                    input["questions"][0]["question"]
                );
                assert_eq!(
                    params["questions"][0]["options"],
                    input["questions"][0]["options"]
                );
                assert_eq!(params["questions"][0]["allow_other"], json!(true));
            }
            _ => panic!("expected Runnable execution"),
        }
    }

    /// Given an `AskUserQuestion` tool invocation carrying multiple
    /// questions, when the `question.ask` reverse RPC is issued, then every
    /// question item in the params carries `allow_other: true` — the engine
    /// always advertises the free-text Other option, matching the tool's
    /// model-facing description and the TS engine's wire behavior.
    #[tokio::test]
    async fn should_carry_allow_other_on_every_question_when_question_ask_rpc_is_issued() {
        let callback = Arc::new(MockCallback {
            response: json!({ "answers": [
                { "question": "Which approach?", "answer": "A" },
                { "question": "Timeline?", "answer": "soon" },
            ] }),
            last_method: Mutex::new(None),
            last_params: Mutex::new(None),
        });
        let tool = AskUserQuestionTool::new(slot_with(callback.clone()));
        let input = json!({
            "questions": [
                {
                    "question": "Which approach?",
                    "options": [
                        { "label": "A", "description": "Option A" },
                        { "label": "B", "description": "Option B" },
                    ],
                },
                {
                    "question": "Timeline?",
                    "multi_select": true,
                    "options": [
                        { "label": "now" },
                        { "label": "later" },
                    ],
                },
            ],
        });

        let execution = tool.resolve_execution(input).unwrap();
        match execution {
            ToolExecution::Runnable { execute, .. } => {
                let ctx = make_ctx();
                let result = execute(ctx).await;
                assert!(!result.is_error);
                let params = callback
                    .last_params
                    .lock()
                    .unwrap()
                    .clone()
                    .expect("callback params");
                let questions = params["questions"].as_array().expect("questions array");
                assert_eq!(questions.len(), 2);
                for (i, question) in questions.iter().enumerate() {
                    assert_eq!(
                        question["allow_other"],
                        json!(true),
                        "question {i} must advertise allow_other"
                    );
                }
                // The injected flag is additive: the original fields survive.
                assert_eq!(params["questions"][1]["multi_select"], json!(true));
                assert_eq!(params["questions"][1]["question"], json!("Timeline?"));
            }
            _ => panic!("expected Runnable execution"),
        }
    }

    /// Given an `AskUserQuestion` tool whose callback returns an error, when
    /// the LLM invokes it, then the tool result is an error.
    #[tokio::test]
    async fn should_return_error_tool_result_when_callback_fails() {
        struct ErrCallback;
        #[async_trait::async_trait]
        impl ReverseRpcCallback for ErrCallback {
            async fn call(&self, _method: &str, _params: Value) -> Result<Value, String> {
                Err("network timeout".to_string())
            }
        }

        let tool = AskUserQuestionTool::new(slot_with(Arc::new(ErrCallback)));
        let execution = tool
            .resolve_execution(json!({"questions": [{"question": "q"}]}))
            .unwrap();
        match execution {
            ToolExecution::Runnable { execute, .. } => {
                let ctx = make_ctx();
                let result = execute(ctx).await;
                assert!(result.is_error);
                let text = match result.output {
                    ToolOutput::Text(t) => t,
                    _ => panic!("expected text output"),
                };
                assert!(text.contains("network timeout"));
            }
            _ => panic!("expected Runnable execution"),
        }
    }

    /// 当 `background` 为 true 且配置了 TaskManager 时, 工具应立即返回
    /// task_id 而不阻塞等待用户回答。
    #[tokio::test]
    async fn should_return_task_id_when_background_is_true() {
        let callback = Arc::new(MockCallback {
            response: json!({ "answers": [{ "question": "Which?", "answer": "A" }] }),
            last_method: Mutex::new(None),
            last_params: Mutex::new(None),
        });
        let tasks = Arc::new(TaskManager::new());
        let tool = AskUserQuestionTool::new(slot_with(callback))
            .with_task_manager(tasks, "session-1".into());

        let input = json!({
            "questions": [{
                "question": "Which approach?",
                "options": [
                    { "label": "A", "description": "Option A" },
                    { "label": "B", "description": "Option B" },
                ],
            }],
            "background": true,
        });

        let execution = tool.resolve_execution(input).unwrap();
        match execution {
            ToolExecution::Runnable { execute, .. } => {
                let ctx = make_ctx();
                let result = execute(ctx).await;
                // 不应出错 — 应立即返回 task_id。
                assert!(!result.is_error, "background mode should not error");
                let text = match result.output {
                    ToolOutput::Text(t) => t,
                    _ => panic!("expected text output"),
                };
                assert!(text.contains("task_id:"), "应返回 task_id: {text}");
                assert!(
                    text.contains("status: running"),
                    "应报告 status: running: {text}"
                );
                assert!(
                    text.contains("automatic_notification: true"),
                    "应包含自动通知提示: {text}"
                );
            }
            _ => panic!("expected Runnable execution"),
        }
    }

    /// 当 `background` 为 false (默认) 时, 工具应阻塞等待用户回答,
    /// 而不是立即返回 task_id。
    #[tokio::test]
    async fn should_block_for_response_when_background_is_false() {
        let callback = Arc::new(MockCallback {
            response: json!({ "answers": [{ "question": "Which?", "answer": "B" }] }),
            last_method: Mutex::new(None),
            last_params: Mutex::new(None),
        });
        let tasks = Arc::new(TaskManager::new());
        let tool = AskUserQuestionTool::new(slot_with(callback.clone()))
            .with_task_manager(tasks, "session-1".into());

        let input = json!({
            "questions": [{
                "question": "Which approach?",
                "options": [
                    { "label": "A", "description": "Option A" },
                    { "label": "B", "description": "Option B" },
                ],
            }],
            "background": false,
        });

        let execution = tool.resolve_execution(input).unwrap();
        match execution {
            ToolExecution::Runnable { execute, .. } => {
                let ctx = make_ctx();
                let result = execute(ctx).await;
                assert!(!result.is_error);
                let text = match result.output {
                    ToolOutput::Text(t) => t,
                    _ => panic!("expected text output"),
                };
                // 阻塞模式应返回实际回答, 而非 task_id。
                assert!(
                    !text.contains("task_id:"),
                    "阻塞模式不应返回 task_id: {text}"
                );
                assert!(text.contains("Which?"), "应包含用户回答内容: {text}");
                // callback 应被调用 (阻塞模式确实发出了 RPC)。
                let method = callback.last_method.lock().unwrap().clone();
                assert_eq!(method.as_deref(), Some("question.ask"));
            }
            _ => panic!("expected Runnable execution"),
        }
    }

    /// Given an `AskUserQuestion` tool whose slot is EMPTY at call time (no
    /// interactive client attached), when the LLM invokes it, then the tool
    /// fails fast with a message directing the model to ask in plain text
    /// instead of blocking or silently delegating.
    #[tokio::test]
    async fn should_fail_fast_directing_to_plain_text_when_slot_is_empty() {
        let tool = AskUserQuestionTool::new(empty_slot());
        let execution = tool
            .resolve_execution(json!({"questions": [{"question": "q"}]}))
            .unwrap();
        match execution {
            ToolExecution::Runnable { execute, .. } => {
                let ctx = make_ctx();
                let result = execute(ctx).await;
                assert!(result.is_error);
                let text = match result.output {
                    ToolOutput::Text(t) => t,
                    _ => panic!("expected text output"),
                };
                assert!(
                    text.contains("plain assistant text"),
                    "错误信息应指引回退为纯文本提问: {text}"
                );
            }
            _ => panic!("expected Runnable execution"),
        }
    }

    /// Given an `AskUserQuestion` tool created BEFORE a transport subscribed
    /// (empty slot at construction), when the callback is installed into the
    /// slot afterwards and the LLM then invokes the tool, then the question
    /// reaches the late-installed callback — the create-time snapshot defect
    /// (main agent missing the tool until re-creation) cannot recur.
    #[tokio::test]
    async fn should_reach_callback_installed_into_slot_after_tool_construction() {
        let callback = Arc::new(MockCallback {
            response: json!({ "answers": [{ "question": "Which?", "answer": "A" }] }),
            last_method: Mutex::new(None),
            last_params: Mutex::new(None),
        });
        let slot = empty_slot();
        let tool = AskUserQuestionTool::new(Arc::clone(&slot));
        // The transport subscribes AFTER the tool (and its agent) existed.
        *slot.write().unwrap() = Some(callback as Arc<dyn ReverseRpcCallback>);

        let execution = tool
            .resolve_execution(json!({"questions": [{"question": "q"}]}))
            .unwrap();
        match execution {
            ToolExecution::Runnable { execute, .. } => {
                let ctx = make_ctx();
                let result = execute(ctx).await;
                assert!(!result.is_error, "晚到 callback 应被解析并调用");
            }
            _ => panic!("expected Runnable execution"),
        }
    }

    /// Build a blocking `AskUserQuestion` tool wired to a session-style
    /// interaction kernel (the B4-a gate registration path), whose callback
    /// resolves the pending question through the gate.
    fn kernel_tool_with(mode: GateResolveMode) -> (Arc<AskUserQuestionTool>, Arc<QuestionGate>) {
        let question_gate = Arc::new(QuestionGate::new());
        let approval_gate = Arc::new(ApprovalGate::new(false));
        let kernel = Arc::new(InteractionKernel::new(approval_gate, question_gate.clone()));
        let callback = Arc::new(GateResolvingCallback {
            gate: question_gate.clone(),
            mode,
        });
        let tool = Arc::new(
            AskUserQuestionTool::new(slot_with(callback)).with_interaction_kernel(Some(kernel)),
        );
        (tool, question_gate)
    }

    /// Given a blocking `AskUserQuestion` whose pending question is dismissed
    /// through the gate, when the ask resolves, then the tool result is a
    /// non-error acknowledgment that stops the turn (`stop_turn`), carries an
    /// empty answers map, and instructs the model to not proceed on
    /// assumptions (SDR-005 / ADR-007).
    #[tokio::test]
    async fn should_stop_turn_with_dismissal_instruction_when_question_is_dismissed() {
        let (tool, _question_gate) = kernel_tool_with(GateResolveMode::Dismiss);
        let execution = tool
            .resolve_execution(json!({
                "questions": [{
                    "question": "Which approach?",
                    "options": [
                        { "label": "A", "description": "Option A" },
                        { "label": "B", "description": "Option B" },
                    ],
                }],
            }))
            .unwrap();

        match execution {
            ToolExecution::Runnable { execute, .. } => {
                let ctx = make_ctx();
                let result = execute(ctx).await;
                assert!(
                    !result.is_error,
                    "dismissal must not surface as an error: {result:?}"
                );
                assert!(
                    result.stop_turn,
                    "dismissal must end the turn (stop_turn = true)"
                );
                let text = match result.output {
                    ToolOutput::Text(t) => t,
                    _ => panic!("expected text output"),
                };
                let payload: serde_json::Value =
                    serde_json::from_str(&text).expect("dismissal output must be JSON");
                assert_eq!(
                    payload["answers"],
                    json!({}),
                    "dismissal must carry empty answers"
                );
                let note = payload["note"].as_str().expect("note must be a string");
                assert!(
                    note.to_lowercase().contains("dismissed"),
                    "note must state the question was dismissed: {note}"
                );
                let instruction = payload["instruction"]
                    .as_str()
                    .expect("instruction must be a string");
                assert!(
                    instruction
                        .to_lowercase()
                        .contains("do not proceed on assumptions"),
                    "instruction must forbid assumption-driven progress: {instruction}"
                );
            }
            _ => panic!("expected Runnable execution"),
        }
    }

    /// Given a blocking `AskUserQuestion` whose pending question is answered
    /// through the gate, when the ask resolves, then the tool result is a
    /// plain success with the answer payload and `stop_turn` stays `false` —
    /// answering never ends the turn (SDR-005 restores the input right only
    /// on dismissal).
    #[tokio::test]
    async fn should_not_stop_turn_when_question_is_answered() {
        let (tool, _question_gate) = kernel_tool_with(GateResolveMode::Answer("A".to_string()));
        let execution = tool
            .resolve_execution(json!({
                "questions": [{
                    "question": "Which approach?",
                    "options": [
                        { "label": "A", "description": "Option A" },
                        { "label": "B", "description": "Option B" },
                    ],
                }],
            }))
            .unwrap();

        match execution {
            ToolExecution::Runnable { execute, .. } => {
                let ctx = make_ctx();
                let result = execute(ctx).await;
                assert!(!result.is_error);
                assert!(
                    !result.stop_turn,
                    "an answered question must not end the turn"
                );
                let text = match result.output {
                    ToolOutput::Text(t) => t,
                    _ => panic!("expected text output"),
                };
                assert!(
                    text.contains("A"),
                    "answer must be returned to the model: {text}"
                );
            }
            _ => panic!("expected Runnable execution"),
        }
    }

    /// Build a background `AskUserQuestion` tool whose callback resolves the
    /// pending question through the shared question gate (answer or dismiss
    /// the minted `request_id`) and then never completes the reverse-RPC
    /// reply — the gate channel must win the race, exactly like the blocking
    /// path's dismissal test.
    fn kernel_background_tool_with(
        mode: GateResolveMode,
    ) -> (
        Arc<AskUserQuestionTool>,
        Arc<TaskManager>,
        Arc<QuestionGate>,
    ) {
        let question_gate = Arc::new(QuestionGate::new());
        let approval_gate = Arc::new(ApprovalGate::new(false));
        let kernel = Arc::new(InteractionKernel::new(approval_gate, question_gate.clone()));
        let callback = Arc::new(GateResolvingCallback {
            gate: question_gate.clone(),
            mode,
        });
        let tasks = Arc::new(TaskManager::new());
        let tool = Arc::new(
            AskUserQuestionTool::new(slot_with(callback))
                .with_task_manager(tasks.clone(), "session-1".into())
                .with_interaction_kernel(Some(kernel)),
        );
        (tool, tasks, question_gate)
    }

    /// Given a background `AskUserQuestion` (`background: true`)
    /// whose pending question is dismissed through the gate, when the task
    /// settles, then the background task ends cancelled (not completed), its
    /// output carries the empty answers plus the dismissal note, and the
    /// terminal notification body carries the same behavioral instruction
    /// the blocking path feeds the model (SDR-005 (c) / ADR-007: the
    /// dismissing agent stops dependent decisions but keeps unrelated work).
    #[tokio::test]
    async fn should_end_background_question_task_cancelled_with_instruction_when_dismissed() {
        let (tool, tasks, _question_gate) = kernel_background_tool_with(GateResolveMode::Dismiss);
        let execution = tool
            .resolve_execution(json!({
                "questions": [{
                    "question": "Which approach?",
                    "options": [
                        { "label": "A", "description": "Option A" },
                        { "label": "B", "description": "Option B" },
                    ],
                }],
                "background": true,
            }))
            .unwrap();

        match execution {
            ToolExecution::Runnable { execute, .. } => {
                let ctx = make_ctx();
                let result = execute(ctx).await;
                assert!(!result.is_error, "background mode should not error");
                let text = match result.output {
                    ToolOutput::Text(t) => t,
                    _ => panic!("expected text output"),
                };
                let task_id = text
                    .lines()
                    .find_map(|l| l.strip_prefix("task_id: "))
                    .expect("the background ask returns a task id")
                    .to_string();

                // The task settles once the gate dismissal resolves the ask.
                assert_eq!(
                    tasks.wait_for_completion(&task_id, 5).await,
                    crate::task::WaitResult::Completed,
                    "the dismissed question task must settle"
                );
                let task = tasks.get(&task_id).expect("the task is tracked");
                assert_eq!(
                    task.status,
                    crate::task::TaskStatus::Stopped,
                    "a dismissed background question must end cancelled, not \
                     completed — the dismiss is not an answer (SDR-005 (c))"
                );
                let output = task.output.clone();
                let payload: serde_json::Value = serde_json::from_str(&output)
                    .expect("the notification output must be the dismissal JSON");
                assert_eq!(
                    payload["answers"],
                    json!({}),
                    "the dismissed task output carries empty answers"
                );
                assert!(
                    payload["note"]
                        .as_str()
                        .is_some_and(|note| note.to_lowercase().contains("dismissed")),
                    "the dismissed task output carries the dismissal note"
                );
                let instruction = payload["instruction"]
                    .as_str()
                    .expect("the dismissed task output carries the instruction");
                assert!(
                    instruction
                        .to_lowercase()
                        .contains("do not proceed on assumptions")
                        && instruction
                            .to_lowercase()
                            .contains("wait for their next input"),
                    "the instruction must match the blocking dismissal voice: {instruction}"
                );

                // The terminal notification body (what reaches the model)
                // carries the same behavioral instruction — SDR-005 (c)
                // reuses the blocking dismissal voice verbatim (no second
                // wording).
                let notification = crate::task_notification::build_task_notification(&task, None);
                assert_eq!(notification.r#type, "task.killed");
                assert!(
                    notification.body.to_lowercase().contains("dismissed"),
                    "the notification must name the dismissal: {}",
                    notification.body
                );
                assert!(
                    notification
                        .body
                        .to_lowercase()
                        .contains("do not proceed on assumptions"),
                    "the notification must carry the behavioral instruction: {}",
                    notification.body
                );
                assert!(
                    notification
                        .body
                        .to_lowercase()
                        .contains("wait for their next input"),
                    "the notification must reuse the blocking dismissal voice: {}",
                    notification.body
                );
            }
            _ => panic!("expected Runnable execution"),
        }
    }

    /// Given a background `AskUserQuestion` whose pending question is
    /// answered through the gate, when the task settles, then the task
    /// completes normally and its output carries the answer unchanged —
    /// answering never cancels the background task (SDR-005 (c) applies to
    /// dismissal only).
    #[tokio::test]
    async fn should_complete_background_question_task_with_answer_when_answered() {
        let (tool, tasks, _question_gate) =
            kernel_background_tool_with(GateResolveMode::Answer("A".to_string()));
        let execution = tool
            .resolve_execution(json!({
                "questions": [{
                    "question": "Which approach?",
                    "options": [
                        { "label": "A", "description": "Option A" },
                        { "label": "B", "description": "Option B" },
                    ],
                }],
                "background": true,
            }))
            .unwrap();

        match execution {
            ToolExecution::Runnable { execute, .. } => {
                let ctx = make_ctx();
                let result = execute(ctx).await;
                assert!(!result.is_error, "background mode should not error");
                let text = match result.output {
                    ToolOutput::Text(t) => t,
                    _ => panic!("expected text output"),
                };
                let task_id = text
                    .lines()
                    .find_map(|l| l.strip_prefix("task_id: "))
                    .expect("the background ask returns a task id")
                    .to_string();

                assert_eq!(
                    tasks.wait_for_completion(&task_id, 5).await,
                    crate::task::WaitResult::Completed,
                    "the answered question task must settle"
                );
                let task = tasks.get(&task_id).expect("the task is tracked");
                assert_eq!(
                    task.status,
                    crate::task::TaskStatus::Completed { exit_code: 0 },
                    "an answered background question must complete normally"
                );
                // When answered THROUGH THE GATE (question.answer channel),
                // the answer string lands as the task output verbatim —
                // exactly what the blocking path's gate branch returns.
                assert_eq!(
                    task.output, "A",
                    "the gate-answered task output must be the answer, unchanged: {}",
                    task.output
                );
                assert!(
                    !task.output.to_lowercase().contains("dismiss"),
                    "an answered question must never carry dismissal wording"
                );
            }
            _ => panic!("expected Runnable execution"),
        }
    }

    /// Given a blocking `AskUserQuestion` that stays unanswered (the user
    /// has not replied yet), When the tool is awaiting the response, Then it
    /// keeps emitting liveness ticks through its progress channel — each
    /// tick surfaces as a `LiveEvent::ToolProgress` that resets the turn's
    /// stall-watchdog deadline, so a turn parked on the user's answer is
    /// never mistaken for a stalled engine and killed after the inactivity
    /// window (the question wait is indefinite by contract).
    #[tokio::test(start_paused = true)]
    async fn should_emit_liveness_ticks_while_waiting_for_the_user_to_answer() {
        // A callback that never resolves — the user simply has not answered.
        struct NeverAnsweringCallback;
        #[async_trait::async_trait]
        impl ReverseRpcCallback for NeverAnsweringCallback {
            async fn call(&self, _method: &str, _params: Value) -> Result<Value, String> {
                std::future::pending().await
            }
        }

        let question_gate = Arc::new(QuestionGate::new());
        let approval_gate = Arc::new(ApprovalGate::new(false));
        let kernel = Arc::new(InteractionKernel::new(approval_gate, question_gate.clone()));
        let tool = AskUserQuestionTool::new(slot_with(Arc::new(NeverAnsweringCallback)))
            .with_interaction_kernel(Some(kernel));
        let execution = tool
            .resolve_execution(json!({
                "questions": [{
                    "question": "Which approach?",
                    "options": [
                        { "label": "A", "description": "Option A" },
                        { "label": "B", "description": "Option B" },
                    ],
                }],
            }))
            .unwrap();

        match execution {
            ToolExecution::Runnable { execute, .. } => {
                // Track ticks with a shared counter so we can assert the
                // cadence across virtual time.
                let ticks = Arc::new(std::sync::atomic::AtomicUsize::new(0));
                let progress = {
                    let ticks = Arc::clone(&ticks);
                    crate::tool::ToolProgressHandle::with_sink(Arc::new(move |_update| {
                        ticks.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    }))
                };
                let ctx = ExecutableToolContext {
                    approval_metadata: None,
                    wait_registry: None,
                    turn_id: "t1".to_string(),
                    tool_call_id: "c1".to_string(),
                    cancel: CancelToken::new(),
                    progress,
                };

                // Advance far past the engine's stall window in virtual
                // time; a tool without keepalive ticks would have produced
                // zero progress events by then. Sleeping (not `advance`) so
                // the paused runtime interleaves the spawned ask and its
                // keepalive ticker at every virtual wakeup.
                let ask = tokio::task::spawn(async move { execute(ctx).await });
                tokio::time::sleep(std::time::Duration::from_secs(600)).await;
                assert!(
                    !ask.is_finished(),
                    "the ask must still be waiting (no answer arrived)"
                );

                let tick_count = ticks.load(std::sync::atomic::Ordering::SeqCst);
                assert!(
                    tick_count >= 2,
                    "the waiting ask must keep emitting liveness ticks \
                     (got {tick_count} over 600 virtual seconds)"
                );
                ask.abort();
            }
            _ => panic!("expected Runnable execution"),
        }
    }

    /// Build a kernel-wired blocking tool over the given slot with a
    /// shortened question-channel wait ceiling (tests run in virtual or
    /// near-real time; the 300s default would stall them).
    fn resilient_tool_with_wait(
        slot: ReverseRpcSlot,
        wait: std::time::Duration,
    ) -> (Arc<AskUserQuestionTool>, Arc<QuestionGate>) {
        let question_gate = Arc::new(QuestionGate::new());
        let approval_gate = Arc::new(ApprovalGate::new(false));
        let kernel = Arc::new(InteractionKernel::new(approval_gate, question_gate.clone()));
        let tool = Arc::new(
            AskUserQuestionTool::new(slot)
                .with_interaction_kernel(Some(kernel))
                .with_question_channel_wait(wait),
        );
        (tool, question_gate)
    }

    /// Given a blocking `AskUserQuestion` whose delivery channel broke
    /// (channel-class error), when a fresh callback is installed into the
    /// slot afterwards (the reconnect — a new transport subscribe), then
    /// the delivery loop re-resolves the slot and completes the ask with
    /// the recovered channel's response (ADR-008: the question reaches
    /// the user after the reconnect; nothing was lost).
    #[tokio::test(start_paused = true)]
    async fn should_wait_for_callback_reinstall_when_channel_breaks() {
        let broken = Arc::new(BrokenChannelCallback::new());
        let slot = slot_with(broken.clone() as Arc<dyn ReverseRpcCallback>);
        let (tool, question_gate) =
            resilient_tool_with_wait(Arc::clone(&slot), std::time::Duration::from_secs(30));

        let execution = tool
            .resolve_execution(json!({
                "questions": [{
                    "question": "Which approach?",
                    "options": [{ "label": "A" }, { "label": "B" }],
                }],
            }))
            .unwrap();
        match execution {
            ToolExecution::Runnable { execute, .. } => {
                let ctx = make_ctx();
                let ask = tokio::task::spawn(async move { execute(ctx).await });

                // While the loop is retrying the broken channel, the
                // transport "re-subscribes": install a working callback
                // into the same slot.
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                *slot.write().unwrap() = Some(Arc::new(MockCallback {
                    response: json!({ "answers": [{ "question": "Which?", "answer": "B" }] }),
                    last_method: Mutex::new(None),
                    last_params: Mutex::new(None),
                }) as Arc<dyn ReverseRpcCallback>);

                // Then the ask completes with the recovered channel's
                // answer, and the broken callback was retried (more than
                // one attempt = the loop was really waiting).
                let result = tokio::time::timeout(std::time::Duration::from_secs(30), ask)
                    .await
                    .expect("the ask settles within the wait ceiling")
                    .expect("the spawned ask joins");
                assert!(
                    !result.is_error,
                    "the recovered delivery must succeed: {result:?}"
                );
                let text = match result.output {
                    ToolOutput::Text(t) => t,
                    _ => panic!("expected text output"),
                };
                assert!(text.contains("B"), "the answer must surface: {text}");
                assert!(
                    broken.attempts() >= 2,
                    "the broken channel must have been retried (attempts: {})",
                    broken.attempts()
                );
                // The gate registration settled (no orphan pending).
                assert_eq!(question_gate.pending_count(), 0);
            }
            _ => panic!("expected Runnable execution"),
        }
    }

    /// Given a blocking `AskUserQuestion` whose delivery channel stays
    /// broken, when the wait ceiling expires, then the ask fails with the
    /// distinguishable "channel unavailable" error (with the plain-text
    /// fallback guidance), and the gate registration is withdrawn — no
    /// orphan pending entry, and the wording never trips the dismissal
    /// re-ask guard (the error must not be recorded as a dismissal).
    #[tokio::test(start_paused = true)]
    async fn should_return_channel_unavailable_error_when_wait_ceiling_expires() {
        let (tool, question_gate) = resilient_tool_with_wait(
            slot_with(Arc::new(BrokenChannelCallback::new())),
            std::time::Duration::from_secs(3),
        );

        let execution = tool
            .resolve_execution(json!({
                "questions": [{
                    "question": "Which approach?",
                    "options": [{ "label": "A" }, { "label": "B" }],
                }],
            }))
            .unwrap();
        match execution {
            ToolExecution::Runnable { execute, .. } => {
                let ctx = make_ctx();
                let result = execute(ctx).await;
                assert!(result.is_error, "the expired wait must fail");
                let text = match result.output {
                    ToolOutput::Text(t) => t,
                    _ => panic!("expected text output"),
                };
                assert!(
                    text.contains("question channel unavailable"),
                    "the error must be the distinguishable channel outcome: {text}"
                );
                assert!(
                    text.contains("plain text"),
                    "the error must direct the plain-text fallback: {text}"
                );
                // The gate entry is withdrawn — no orphan pending.
                assert_eq!(
                    question_gate.pending_count(),
                    0,
                    "the expired ask must withdraw its gate registration"
                );
                // The wording is not a dismissal: the re-ask guard must
                // not fire for the same question text afterwards.
                assert!(
                    !question_gate.was_recently_dismissed("Which approach?"),
                    "a channel timeout must not enter the dismissal ring"
                );
            }
            _ => panic!("expected Runnable execution"),
        }
    }

    /// Given a blocking `AskUserQuestion` whose callback returns a
    /// BUSINESS-class error (the domain was not negotiated), when the ask
    /// is delivered, then it fails immediately (no channel wait) and the
    /// gate registration is withdrawn — the pre-resilience fast-fail
    /// behavior, preserved (ADR-008: only channel-class errors wait).
    #[tokio::test(start_paused = true)]
    async fn should_fail_fast_on_business_error_without_waiting() {
        let (tool, question_gate) = resilient_tool_with_wait(
            slot_with(Arc::new(BusinessErrorCallback)),
            // A long ceiling proves the business error does NOT wait.
            std::time::Duration::from_secs(300),
        );

        let execution = tool
            .resolve_execution(json!({
                "questions": [{ "question": "Which?" }],
            }))
            .unwrap();
        match execution {
            ToolExecution::Runnable { execute, .. } => {
                let ctx = make_ctx();
                let result = tokio::time::timeout(std::time::Duration::from_secs(30), execute(ctx))
                    .await
                    .expect("a business error settles without waiting for the ceiling");
                assert!(result.is_error);
                let text = match result.output {
                    ToolOutput::Text(t) => t,
                    _ => panic!("expected text output"),
                };
                assert!(
                    text.contains("was not negotiated"),
                    "the business error surfaces verbatim: {text}"
                );
                assert_eq!(question_gate.pending_count(), 0);
            }
            _ => panic!("expected Runnable execution"),
        }
    }

    /// Given a blocking `AskUserQuestion` waiting in the delivery loop
    /// (channel broken), when the user answers through the GATE channel
    /// (`question.answer` — the new RPC path), then the waiting loop
    /// yields to the gate answer and the tool returns it — the user's
    /// answer must never sit in the receiver waiting for the (still
    /// broken) reverse-RPC channel (ADR-008: gate convergence holds
    /// INSIDE the wait loop).
    #[tokio::test(start_paused = true)]
    async fn should_answer_via_gate_while_delivery_loop_waits() {
        let (tool, question_gate) = resilient_tool_with_wait(
            slot_with(Arc::new(BrokenChannelCallback::new())),
            std::time::Duration::from_secs(30),
        );

        let execution = tool
            .resolve_execution(json!({
                "questions": [{
                    "question": "Which approach?",
                    "options": [{ "label": "A" }, { "label": "B" }],
                }],
            }))
            .unwrap();
        match execution {
            ToolExecution::Runnable { execute, .. } => {
                let ctx = make_ctx();
                let ask = tokio::task::spawn(async move { execute(ctx).await });

                // While the loop waits for the channel, the user answers
                // through the gate's request_id (question.answer).
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                let pending = question_gate.list_pending();
                assert_eq!(pending.len(), 1, "the waiting ask stays pending");
                let (request_id, _) = pending[0].clone();
                assert!(
                    question_gate.answer(request_id, "A".to_string()),
                    "the gate answer must land while the channel is down"
                );

                let result = tokio::time::timeout(std::time::Duration::from_secs(30), ask)
                    .await
                    .expect("the gate answer settles the ask")
                    .expect("the spawned ask joins");
                assert!(!result.is_error, "the gate answer resolves cleanly");
                let text = match result.output {
                    ToolOutput::Text(t) => t,
                    _ => panic!("expected text output"),
                };
                assert!(text.contains("A"), "the gate answer surfaces: {text}");
            }
            _ => panic!("expected Runnable execution"),
        }
    }

    /// Given a BACKGROUND `AskUserQuestion` whose delivery channel broke,
    /// when the channel recovers (a fresh callback lands in the slot),
    /// then the background task eventually completes with the user's
    /// answer — the resilience covers the background path identically
    /// (ADR-008: both paths embed the delivery loop).
    #[tokio::test(start_paused = true)]
    async fn should_settle_background_task_when_channel_recovers() {
        let broken = Arc::new(BrokenChannelCallback::new());
        let slot = slot_with(broken.clone() as Arc<dyn ReverseRpcCallback>);
        let question_gate = Arc::new(QuestionGate::new());
        let approval_gate = Arc::new(ApprovalGate::new(false));
        let kernel = Arc::new(InteractionKernel::new(approval_gate, question_gate.clone()));
        let tasks = Arc::new(TaskManager::new());
        let tool = Arc::new(
            AskUserQuestionTool::new(Arc::clone(&slot))
                .with_task_manager(tasks.clone(), "session-1".into())
                .with_interaction_kernel(Some(kernel))
                .with_question_channel_wait(std::time::Duration::from_secs(30)),
        );

        let execution = tool
            .resolve_execution(json!({
                "questions": [{
                    "question": "Which approach?",
                    "options": [{ "label": "A" }, { "label": "B" }],
                }],
                "background": true,
            }))
            .unwrap();
        match execution {
            ToolExecution::Runnable { execute, .. } => {
                let ctx = make_ctx();
                let result = execute(ctx).await;
                assert!(!result.is_error);
                let text = match result.output {
                    ToolOutput::Text(t) => t,
                    _ => panic!("expected text output"),
                };
                let task_id = text
                    .lines()
                    .find_map(|l| l.strip_prefix("task_id: "))
                    .expect("the background ask returns a task id")
                    .to_string();

                // While the detached future retries the broken channel,
                // the transport "re-subscribes" with a working callback.
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                *slot.write().unwrap() = Some(Arc::new(MockCallback {
                    response: json!({ "answers": [{ "question": "Which?", "answer": "B" }] }),
                    last_method: Mutex::new(None),
                    last_params: Mutex::new(None),
                }) as Arc<dyn ReverseRpcCallback>);

                // Then the task completes with the recovered answer.
                assert_eq!(
                    tasks.wait_for_completion(&task_id, 30).await,
                    crate::task::WaitResult::Completed,
                    "the recovered background ask must settle"
                );
                let task = tasks.get(&task_id).expect("the task is tracked");
                assert_eq!(
                    task.status,
                    crate::task::TaskStatus::Completed { exit_code: 0 },
                    "a recovered delivery completes the question task"
                );
                assert!(
                    task.output.contains("B"),
                    "the answer lands as the task output: {}",
                    task.output
                );
                assert!(
                    broken.attempts() >= 2,
                    "the broken channel must have been retried (attempts: {})",
                    broken.attempts()
                );
                assert_eq!(question_gate.pending_count(), 0);
            }
            _ => panic!("expected Runnable execution"),
        }
    }

    /// Given a BACKGROUND `AskUserQuestion` waiting in the delivery loop
    /// (channel broken), when the model stops the task (TaskStop — a
    /// detached question future observes no cancel signal, so only the
    /// status flips), then the loop's status poll notices the Stopped
    /// state, withdraws the gate registration, and settles the task
    /// cancelled (D-6: the stop does not wait out the ceiling).
    #[tokio::test(start_paused = true)]
    async fn should_withdraw_gate_when_background_question_stopped_while_waiting() {
        let question_gate = Arc::new(QuestionGate::new());
        let approval_gate = Arc::new(ApprovalGate::new(false));
        let kernel = Arc::new(InteractionKernel::new(approval_gate, question_gate.clone()));
        let tasks = Arc::new(TaskManager::new());
        let tool = Arc::new(
            AskUserQuestionTool::new(slot_with(Arc::new(BrokenChannelCallback::new())))
                .with_task_manager(tasks.clone(), "session-1".into())
                .with_interaction_kernel(Some(kernel))
                .with_question_channel_wait(std::time::Duration::from_secs(300)),
        );

        let execution = tool
            .resolve_execution(json!({
                "questions": [{
                    "question": "Which approach?",
                    "options": [{ "label": "A" }, { "label": "B" }],
                }],
                "background": true,
            }))
            .unwrap();
        match execution {
            ToolExecution::Runnable { execute, .. } => {
                let ctx = make_ctx();
                let result = execute(ctx).await;
                assert!(!result.is_error);
                let text = match result.output {
                    ToolOutput::Text(t) => t,
                    _ => panic!("expected text output"),
                };
                let task_id = text
                    .lines()
                    .find_map(|l| l.strip_prefix("task_id: "))
                    .expect("the background ask returns a task id")
                    .to_string();
                assert_eq!(question_gate.pending_count(), 1);

                // When the model stops the task (TaskStop's face):
                // the status flips; the poller must see it within one
                // retry interval.
                tasks.stop(&task_id);
                assert_eq!(
                    tasks.wait_for_completion(&task_id, 30).await,
                    crate::task::WaitResult::Completed,
                    "the stopped background ask must settle promptly"
                );
                // `wait_for_completion` observes the STATUS flip (the
                // stop wrote it directly); the detached future's
                // withdraw + settle still run asynchronously — give the
                // loop one retry interval to observe the stop before
                // asserting the gate state.
                tokio::time::sleep(QUESTION_DELIVERY_RETRY_INTERVAL * 4).await;
                let task = tasks.get(&task_id).expect("the task is tracked");
                assert!(
                    matches!(task.status, crate::task::TaskStatus::Stopped),
                    "a stopped question task lands cancelled: {:?}",
                    task.status
                );
                // The gate entry was withdrawn — no orphan pending.
                assert_eq!(
                    question_gate.pending_count(),
                    0,
                    "the stop must withdraw the gate registration"
                );
            }
            _ => panic!("expected Runnable execution"),
        }
    }

    /// Resolve a pending question through the gate, then return a fresh tool
    /// bound to the same kernel — the state (dismissed/answered) the re-ask
    /// guard reads.
    fn kernel_tool_after_resolution(
        mode: GateResolveMode,
    ) -> (Arc<AskUserQuestionTool>, Arc<QuestionGate>) {
        let (tool, question_gate) = kernel_tool_with(mode.clone());
        // Register and resolve one question so the gate records the outcome.
        let (request_id, _receiver) = question_gate.register(QuestionSnapshot {
            questions: json!([{ "question": "Which approach?" }]),
            agent_id: uuid::Uuid::nil(),
            turn_id: None,
            tool_call_id: None,
            created_at: now_ms(),
        });
        match mode {
            GateResolveMode::Dismiss => {
                assert!(question_gate.dismiss(request_id));
            }
            GateResolveMode::Answer(answer) => {
                assert!(question_gate.answer(request_id, answer));
            }
        }
        (tool, question_gate)
    }

    /// Given a question that was dismissed, when the LLM re-asks it
    /// VERBATIM, then the re-ask is rejected before any question is
    /// registered — the guard feeds back a guidance message naming the
    /// dismissal and directing a rephrase (US8 不得原样重问 / ADR-007 工具
    /// 入参校验层拒绝).
    #[tokio::test]
    async fn should_reject_verbatim_reask_with_guidance_when_question_was_dismissed() {
        let (tool, _gate) = kernel_tool_after_resolution(GateResolveMode::Dismiss);

        // When the same text is re-asked
        let execution = tool
            .resolve_execution(json!({
                "questions": [{
                    "question": "Which approach?",
                    "options": [
                        { "label": "A", "description": "Option A" },
                        { "label": "B", "description": "Option B" },
                    ],
                }],
            }))
            .unwrap();

        // Then the call is rejected synthetically with an error result whose
        // message names the dismissal and directs to rephrase instead
        match execution {
            ToolExecution::Error(err) => {
                assert!(err.message.to_lowercase().contains("dismissed"));
                assert!(
                    err.message.to_lowercase().contains("rephrase"),
                    "the rejection must direct a rephrase: {}",
                    err.message
                );
                assert!(
                    err.message.to_lowercase().contains("same question"),
                    "the rejection must state the text was already asked: {}",
                    err.message
                );
            }
            _ => panic!("expected a synthetic Error rejection"),
        }
    }

    /// Given a question that was dismissed, when the LLM re-asks it
    /// REPHRASED, then the call is allowed through to the blocking ask —
    /// dismissal is '暂不回答' not '永久拒绝' (SDR-005 (d): a clearer
    /// rephrase stays allowed).
    #[tokio::test]
    async fn should_allow_rephrased_reask_when_question_was_dismissed() {
        let (tool, _gate) = kernel_tool_after_resolution(GateResolveMode::Dismiss);

        // When a rephrased question is asked
        let execution = tool
            .resolve_execution(json!({
                "questions": [{
                    "question": "Which approach would you prefer, A or B?",
                    "options": [
                        { "label": "A", "description": "Option A" },
                        { "label": "B", "description": "Option B" },
                    ],
                }],
            }))
            .unwrap();

        // Then the call proceeds to a Runnable execution (reaches the
        // reverse-RPC ask's gate registration)
        match execution {
            ToolExecution::Runnable { plan, .. } => {
                assert_eq!(plan.approval_rule, "AskUserQuestion");
            }
            ToolExecution::Error(err) => {
                panic!("a rephrased question must not be rejected: {}", err.message);
            }
        }
    }

    /// Given a question that was ANSWERED, when the LLM re-asks the same
    /// text, then the call is allowed — the guard fires only for dismissed
    /// questions, answered ones may be re-asked (the answer may have been
    /// insufficient or the model needs it again).
    #[tokio::test]
    async fn should_allow_reask_when_question_was_answered() {
        let (tool, _gate) = kernel_tool_after_resolution(GateResolveMode::Answer("A".to_string()));

        // When the same text is re-asked after an answer
        let execution = tool
            .resolve_execution(json!({
                "questions": [{
                    "question": "Which approach?",
                    "options": [
                        { "label": "A", "description": "Option A" },
                        { "label": "B", "description": "Option B" },
                    ],
                }],
            }))
            .unwrap();

        // Then the call proceeds to a Runnable execution
        match execution {
            ToolExecution::Runnable { plan, .. } => {
                assert_eq!(plan.approval_rule, "AskUserQuestion");
            }
            ToolExecution::Error(err) => {
                panic!("an answered question must not be rejected: {}", err.message);
            }
        }
    }

    /// Given a dismissed question in one tool instance, when a DIFFERENT
    /// tool instance bound to the same session kernel re-asks the verbatim
    /// text, then it is still rejected — the guard lives in the session's
    /// gate (the shared source of truth), not in a tool's instance state.
    #[tokio::test]
    async fn should_reject_verbatim_reask_from_another_tool_instance_when_question_was_dismissed() {
        let (first, question_gate) = kernel_tool_after_resolution(GateResolveMode::Dismiss);
        drop(first);
        // A fresh tool over the same gate (the session-scoped kernel).
        let approval_gate = Arc::new(ApprovalGate::new(false));
        let kernel = Arc::new(InteractionKernel::new(
            approval_gate,
            Arc::clone(&question_gate),
        ));
        let second =
            Arc::new(AskUserQuestionTool::new(empty_slot()).with_interaction_kernel(Some(kernel)));

        // When the same text is re-asked through the other instance
        let execution = second
            .resolve_execution(json!({
                "questions": [{
                    "question": "Which approach?",
                    "options": [
                        { "label": "A", "description": "Option A" },
                        { "label": "B", "description": "Option B" },
                    ],
                }],
            }))
            .unwrap();

        // Then the shared gate still rejects it
        match execution {
            ToolExecution::Error(err) => {
                assert!(err.message.to_lowercase().contains("dismissed"));
            }
            _ => panic!("expected a synthetic Error rejection"),
        }
    }

    /// Given a dismissed question followed by a REPHRASED re-ask that the
    /// user ANSWERS, when the ORIGINAL text is later re-asked verbatim,
    /// then it is STILL rejected — the dismissal text stays in the ring for
    /// the session; answering the rephrased question never clears it (the
    /// ring records per-text, dismissal is sticky within the session
    /// window; SDR-005 (d) prevents the original re-ask regardless of a
    /// later answer to a different phrasing).
    #[tokio::test]
    async fn should_still_reject_original_verbatim_reask_when_rephrased_was_answered() {
        let (tool, question_gate) = kernel_tool_after_resolution(GateResolveMode::Dismiss);
        // The rephrased question is asked and ANSWERED through the gate.
        let (rephrased_id, _receiver) = question_gate.register(QuestionSnapshot {
            questions: json!([{ "question": "Which approach would you prefer, A or B?" }]),
            agent_id: uuid::Uuid::nil(),
            turn_id: None,
            tool_call_id: None,
            created_at: now_ms(),
        });
        assert!(question_gate.answer(rephrased_id, "A".to_string()));
        assert!(
            !question_gate.was_recently_dismissed("Which approach would you prefer, A or B?"),
            "the answered rephrased text must not be dismissed"
        );

        // When the ORIGINAL dismissed text is re-asked verbatim
        let execution = tool
            .resolve_execution(json!({
                "questions": [{
                    "question": "Which approach?",
                    "options": [
                        { "label": "A", "description": "Option A" },
                        { "label": "B", "description": "Option B" },
                    ],
                }],
            }))
            .unwrap();

        // Then the original text is still rejected as recently dismissed
        match execution {
            ToolExecution::Error(err) => {
                assert!(err.message.to_lowercase().contains("dismissed"));
            }
            _ => panic!("expected a synthetic Error rejection"),
        }
    }
}
