# 旁路 LLM 会话标题生成（side-llm-session-title）实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 移除 `SetSessionTitle` 内置工具与轮内注入机制，改为在首个用户轮次成功 settle 后，以一次不影响主会话循环的旁路 LLM 调用生成标题——该调用复用主循环的请求前缀（同模型、同 system prompt、同 tools、同消息投影），最大化 provider 侧输入缓存命中。

**Architecture:** 触发点为 wait/fire/steer 三个用户轮次成功 settle 站点；旁路任务 `tokio::spawn` 后台执行，用与主循环相同的 `ConfigResolvingLlm` 解析器重建请求（复用 `ContextMemory` 的 normal 投影 + media pipeline + `loop_tools`），追加一条标题指令 user 消息后做一次性 chat；成功则经既有 `set_auto_title` 守卫写入并 `note_turn_settled` 落盘，失败/超时则退回既有规则式兜底。工具、`TitleInjector`、`title_handle`、`title_materialized` 标志全部移除。

**Tech Stack:** Rust（mirri-core / mirri-embedded），tokio spawn + timeout，既有 `Llm`/`LlmResolver` 抽象与 `EventBus`。

## Global Constraints

- 质量门禁（rust/AGENTS.md）：`cargo test --workspace` 全绿 + `cargo clippy --workspace --all-targets -- -D warnings` 零告警；命令在 `rust/` 目录下执行。
- zero-IO 守卫：mirri-core 生产代码禁直接 IO（`node rust/scripts/check-zero-io.mjs` exit 0）。本计划不新增任何 IO。
- 缓存可观测（rust/AGENTS.md 契约 5）：涉及请求前缀/缓存命中的改动，验收必须给出 usage 三桶（uncachedInput/cacheRead/cacheWrite）数值或请求前缀对比证据，不接受"应该会更好"。本计划以 Task 5 的前缀对比测试 + 旁路调用 usage 日志满足。
- wire log append-only（契约 2）：旁路标题调用**不**向 wire log 记录任何操作（避免新增会话状态域的写侧+reducer 义务），标题持久化仅走 `state.json` + 索引写通。
- 测试命名 Given-When-Then（`should ... when ...`），不提及实现细节。
- 提交信息 Conventional Commit 风格，无 co-author 署名，不暴露 agent 身份。
- 注释/文档语言：代码注释跟随所在文件既有风格（本仓库 rust 代码为英文注释）；spec/plan 文档为中文。
- 本任务全程不做 `git push`/PR（除非用户另行指示）；changeset 走 gen-changesets skill，默认 `minor`，发现疑似 breaking 须停下与用户确认。

---

## 设计（终态）

### D1. 决策结论（结论先行）

**最终决策：首个用户轮次成功 settle 后，spawn 一个旁路 LLM 调用生成标题；请求前缀与主循环最后一个请求逐字节对齐（同模型、同 system prompt、同 tools 序列、同消息投影），仅在末尾追加一条标题指令 user 消息。**

被否决的备选方向（及理由）：

| 备选 | 问题 | 结论 |
|---|---|---|
| 首条用户消息派发时点（turn 开始前）调用 | ① 意图只有用户消息，无助手响应佐证，清晰度差；② 主循环请求尚未发出，provider 缓存未写入，并发竞态下缓存冷；③ A9 时序契约要求轮前保持占位 | 否决 |
| 用独立小模型调用 | 前缀与主会话不匹配，**零**缓存命中，输入全额计费且慢；且需新增"小模型"配置面 | 否决（未来可作可选配置，见 D7） |
| 主循环内注入指令、模型以文本输出标题（不用工具） | 仍污染主会话上下文与缓存前缀（指令进入历史、tool-result 语义消失），与现状痛点同构 | 否决 |
| 保留工具作兜底、旁路作主路径 | 两套机制并存，工具 schema 常驻每个请求的隐性成本与守卫双写 | 否决 |
| 轮次错误（failed）settle 时也触发旁路调用 | 轮次失败通常意味着 provider 异常，旁路调用大概率同样失败；且错误时消息尾部可能残缺 | 否决——错误 settle 直接走规则式兜底（保持 R-D） |

### D2. 触发时机：为什么是"首个成功用户轮次 settle"

- **用户意图清晰**：此刻会话已有"用户消息 + 助手完整响应"，意图比仅有用户消息时更明确。
- **缓存最热**：主循环的最后一批请求刚在 provider 侧完成缓存写入（Anthropic ephemeral 缓存 TTL 5 分钟，settle 距最后请求通常仅数秒）。
- **不碰主循环**：settle 之后主轮次已终结，旁路任务与下一个用户轮次天然错开；即使与下一轮并发，也只读投影、不写 memory、不记 wire op，对主循环的前缀稳定性零影响。

触发条件（全部满足才 spawn）：`agent_id == DEFAULT_MAIN_AGENT_ID`（主 agent）；会话未命名（`!is_custom_title && is_untitled_title(title)`）；进程内一次性 claim 成功（D5）。

触发站点（成功 settle）：wait 模式 `message_handlers.rs:875`、fire 模式 `:1017`、steer `:1716`。**不**触发的站点：错误 settle（`:820`、`:1053`、`:1685`）、硬中止 settle（`:1290`）、no-provider 派发分支（`:498`）——这三类保持规则式兜底，保证"标题恒存在"（R-D）。

被取消（cancelled→Aborted）的轮次仍走成功 settle 路径（`coerce_aborted_if_cancelled` 后是 Ok 分支），会触发旁路调用：用户消息意图仍清晰，且若 provider 正常则标题照样生成；若 provider 卡死则 30s 超时后走规则式兜底。

### D3. 请求构造：缓存命中率策略

旁路调用必须**逐字节复用**主循环请求前缀：

1. **同一 LLM 实例配置**：用 `ConfigResolvingLlm::new(agent.config(), llm_factory)`（与主循环 `message_handlers.rs:529` 同构造）→ 同 provider、同模型、同 system prompt（system prompt 存于 adapter，`AgentScopeConfig.system_prompt` 冻结，settle 时可原样复用）。
2. **同 tools 序列**：`agent.tool_manager().read().loop_tools()`，并复刻 `turn_step.rs:233-238` 的规则——模型无 `image_in`/`video_in` 时剔除 `ReadMediaFile`。**注意**：工具移除（Task 4）后主循环不再发送 `SetSessionTitle`，两边自然一致。
3. **同消息投影**：`agent.context_memory().lock().messages()`（normal 投影，无 micro-compaction——这正是仓库为保前缀缓存刻意选择的 `ProjectOptions::normal()`），再过与 `turn_step.rs:165-176` 相同的 media pipeline（`VideoReferenceResolver::for_capability` + `MediaCapabilityAdapter`）。
4. **追加一条指令 user 消息**（成功 settle 时对话尾部必为 assistant 消息，交替合法）。

缓存命中分析（Anthropic，adapter 在 `providers/anthropic.rs:187/210/213` 设 3 个断点：system 尾块、最后一条工具、最后一条消息的最后 block）：

- system 断点：旁路请求 system 与主循环完全相同 → **命中**（Anthropic 最大的静态块，含 AGENTS.md + 目录树）。
- tools 断点：tools 列表逐字节相同 → **命中**。
- 消息断点：落在追加的指令消息上（新内容，cache write）；system+tools 之间的消息段为未缓存输入——**首轮后该段仅 1 条用户消息 + 1 条助手消息（约几百 token），成本可忽略**。
- OpenAI / Google / Kimi：隐式服务端前缀缓存，旁路请求前缀是主请求前缀的严格超集 → 消息段同样命中。
- 旁路调用自身**不写**任何进入后续请求前缀的内容（不追加 memory、不记 wire op）→ 主循环缓存不受影响。

结论：旁路调用输入 ≈ 全量缓存读（约 0.1x 单价）+ 数百 token 新输入 + 约 20 token 输出。相比现状（工具 schema 常驻 + 提醒注入 + tool-call 帧进入缓存上下文），总量更低且不占主循环。

### D4. 写入、守卫与降级链

- **写入**：沿用 `SessionMetaHandle::set_auto_title`（守卫：`is_custom_title == false` 且标题仍为占位；脱敏 + 200 字符截断，与规则式同一实现）→ 发布既有 `SessionEvent::SessionMetaUpdated`（web/TUI 已消费，零契约变更）→ 再调 `note_turn_settled` 写通 `state.json` + 索引（settle 的写通发生在旁路调用之前，标题落地后必须再写一次）。
- **降级链（终态）**：

| 情形 | 行为 |
|---|---|
| 成功 settle，会话未命名 | spawn 旁路调用；成功 → LLM 标题 |
| 旁路调用失败 / 超时（30s）/ 响应无文本 | release claim → 规则式兜底（用户消息文本截取） |
| 轮次错误 settle（wait/fire/steer） | 直接规则式兜底（不触发旁路调用） |
| 硬中止（abort）settle | 规则式兜底（锁无关路径，保持现状） |
| 未配置 provider | 派发时点规则式命名（现状不变） |
| 兜底也无文本（纯图片消息） | 保持未命名，release claim，下一用户轮次 settle 重试旁路调用 |
| cron/task 引擎内部轮次 | 不参与命名（现状不变） |

- **崩溃窗口自愈**（A8 语义保持）：settle 写通的是占位标题、旁路标题落地前的进程死亡 → 恢复后会话仍未命名，下一用户轮次 settle 重试。

### D5. 并发与生命周期

- `SessionMetaHandle` 新增进程内原子标志 `side_title_claimed`（不持久化）：`claim_side_title_generation()`（CAS false→true）防止 fire/steer 并发 settle 重复触发；失败路径 `release_side_title_generation()` 复位（仅"未产生标题"时复位——成功路径标题已落定，claim 无需复位）。
- 用户在旁路调用在途时手动改名：`set_auto_title` 守卫原子拒绝，用户标题胜出（A2/A3 语义不变）。
- 旁路调用自带**全新** `CancelToken`（不可复用轮次的 token——wait 轮次 settle 时其 token 可能已取消）+ `tokio::time::timeout(30s)`。
- 旁路调用不进 in-flight 注册表、不可取消、不记 wire log；完成后 `tracing::info!` 输出 usage 三桶（契约 5）。

### D6. 移除清单（工具机制退役）

| 项 | 位置 |
|---|---|
| `SetSessionTitleTool` | `rust/crates/mirri-core/src/tools/set_session_title.rs`（整文件删除） |
| 工具注册/目录项 | `tools/mod.rs`：模块声明:39、re-export:76、`BuiltinToolServices.title`:227、注册块:359-363、`BUILTIN_TOOL_NAMES`:491、`BUILTIN_TOOL_CATALOG`:590-591 |
| `TitleInjector` + `TITLE_REMINDER` | `rust/crates/mirri-core/src/turn_flow/injection/title.rs`（整文件删除）+ `injection/mod.rs` |
| `TurnFlow.title_injector` + `set_title_injector` | `turn_flow/mod.rs:262, 526` |
| 挂载块 | `message_handlers.rs:595-607`（wait/fire）、`:1559-1572`（steer） |
| `unmount_title_reminder` + 全部调用 | `message_handlers.rs:293`（fn）及 826/882/1024/1059/1691/1722 |
| `clear_title_reminder` 参数 | `run_engine_turn` 的参数:158 与消费块:173-179；调用方 `cron_driver.rs:526,564`、`task_driver.rs:762,801`、wait:806、fire:981、steer 处 |
| `AgentScopeConfig.title_handle` | `scope/config.rs:358,435`；`agent_scope.rs:211` 接线；`session_handlers.rs:409`、`session/manager/restore.rs:183` 写入点 |
| `title_materialized` 标志及方法 | `session_scope.rs:237,260,276-284`（`set_auto_title` 内的置位行、`title_materialized()`、`mark_title_materialized()`）及其全部调用点（rename/updateMeta/restore 的置位点） |

旧会话兼容：wire log 中历史 `SetSessionTitle` tool_use/tool_result 作为普通消息内容回放，无需执行；历史注入的 reminder 消息同样按普通消息回放。Task 5 含恢复测试。

### D7. 非目标（本计划不做）

- 不新增"标题专用小模型"配置（未来可选优化：配置项切换旁路调用用的 model alias，届时再评缓存损失）。
- 不做"标题生成中"中间 UI 状态。
- 不改变 TS 各端（agent-core-v2 的规则式路径、web/TUI 事件消费）——零 TS 改动。
- 不为 ACP 端新增标题推送（既有边界）。

### D8. t9 测试处置表

| 现有测试 | 处置 |
|---|---|
| `should_title_via_tool_call_within_first_user_turn` | Task 3 重写为旁路调用命名 |
| `should_write_the_title_to_state_json_mid_turn_before_the_turn_settles` | Task 3 删除（无轮内命名路径）；改为断言"标题落地后 state.json 已写通"并入旁路测试 |
| `should_keep_placeholder_title_while_first_turn_is_in_flight` | 保留（语义不变） |
| `should_fall_back_to_prompt_text_when_model_skips_the_tool` | Task 3 重写：旁路调用失败→规则式兜底 |
| `should_fall_back_when_first_turn_is_cancelled` | Task 3 重写：取消轮次仍触发旁路调用；旁路失败→兜底 |
| `should_not_overwrite_tool_title_with_fallback` | Task 3 重写：旁路写入的标题不被兜底覆盖 |
| `should_keep_untitled_when_first_prompt_has_no_text` | Task 3 重写：旁路调用失败且无文本→保持未命名 |
| `should_not_fall_back_or_inject_for_cron_turns` | Task 4 更新：cron 轮次无注入（断言注入器面为空） |
| `should_persist_the_derived_title_when_no_llm_provider_is_configured` | 保留（不变） |
| `should_never_overwrite_a_user_renamed_title` | Task 3 重写：改名胜过旁路写入 |
| `should_extinguish_the_reminder_and_keep_the_title_tool_active_after_naming` | Task 4 删除（机制移除） |
| `should_redact_credentials_in_tool_title_argument` | Task 3 重写：LLM 返回的标题经同一脱敏写入 |
| `should_treat_idle_steer_as_first_user_turn` | Task 3 重写为 steer settle 触发旁路调用 |
| `should_fall_back_when_an_idle_steer_skips_the_tool` | Task 3 重写：steer 首轮旁路失败→兜底 |
| `should_not_offer_title_tool_on_subagents` | Task 4 删除（工具不存在；子 agent settle 站点本就无标题逻辑） |
| `should_close_the_abort_residue_before_the_next_cron_turn` | Task 4 更新：注入器已不存在，残余清理断言简化/删除 |
| `should_write_a_user_rename_to_state_json_immediately` | 保留（不变） |
| `should_extinguish_reminder_when_title_written_via_update_meta_mid_turn` | Task 4 删除（reminder 机制移除；`mark_title_materialized` 调用点随移除） |

---

## File Structure

| 文件 | 职责 |
|---|---|
| Create: `rust/crates/mirri-core/src/session/title_llm.rs` | 纯函数层：标题指令常量 + 一次性旁路 chat（消息追加、流消费、usage 返回） |
| Modify: `rust/crates/mirri-core/src/session/mod.rs`（或 `session.rs`） | 注册 `title_llm` 模块 |
| Modify: `rust/crates/mirri-core/src/scope/session_scope.rs` | `SessionMetaHandle` 增加 claim/release；（Task 4 移除 materialized 标志） |
| Create: `rust/crates/mirri-embedded/src/embedded_server/side_title.rs` | 编排层：spawn 旁路任务、重建请求前缀、写入/兜底/落盘 |
| Modify: `rust/crates/mirri-embedded/src/embedded_server/mod.rs` | 注册 `side_title` 模块 |
| Modify: `rust/crates/mirri-embedded/src/embedded_server/message_handlers.rs` | settle 站点接线；（Task 4 移除挂载/卸载/参数） |
| Modify: `rust/crates/mirri-embedded/src/embedded_server/tests/t9_session_title_tests.rs` | 按处置表重写/删除/保留 |
| Modify（Task 4）: `tools/mod.rs`、`turn_flow/*`、`scope/config.rs`、`agent_scope.rs`、`session_handlers.rs`、`session/manager/restore.rs`、`cron_driver.rs`、`task_driver.rs` | 机制退役 |
| Modify: `docs/specs/rust-core-design/09-session-title-generation.md` | ADR-020 + 终态改写 |

---

### Task 1: core — 旁路标题调用纯函数（`title_llm.rs`）

**Files:**
- Create: `rust/crates/mirri-core/src/session/title_llm.rs`
- Modify: `rust/crates/mirri-core/src/session/mod.rs`（若 `session` 是文件模块则为 `rust/crates/mirri-core/src/session.rs`；用 `rg -n "pub mod title" rust/crates/mirri-core/src` 确认后在其旁注册 `pub mod title_llm;`）

**Interfaces:**
- Consumes: `crate::llm::{Llm, LlmChatParams, LlmEventStream, LlmEvent, LlmUsage}`（`llm.rs`）、`crate::message::Message`（`Message::user(text)`，`message.rs:118`）、`crate::tool::ToolDefinition`、`crate::loop_engine::shared::collect_stream_events`（`pub(crate)`，`shared.rs:179`）、`crate::cancel::CancelToken`、`crate::EngineError::ProviderError`。
- Produces（后续任务依赖）:
  - `pub const TITLE_GENERATION_INSTRUCTION: &str`
  - `pub async fn generate_session_title(llm: &dyn Llm, messages: Vec<Message>, tools: Vec<ToolDefinition>) -> EngineResult<(String, LlmUsage)>`

- [ ] **Step 1: 写失败测试**

在 `title_llm.rs` 中创建文件与内联测试模块（先只写测试与常量，函数体 `todo!()` 也可以——本步只求编译通过、测试失败）：

```rust
//! Out-of-band LLM session-title generation (spec 09 / ADR-020).
//!
//! The title is produced by a side LLM call that never touches the main
//! turn loop: the caller passes the conversation prefix exactly as the
//! main loop serialized it (same system prompt lives in the adapter,
//! same tool list, same projected messages), and this module appends one
//! instruction message, performs a single chat, and returns the raw
//! response text for the caller to sanitize through the shared
//! rule-based path. Nothing here records into context memory or the
//! wire log.

use crate::llm::{Llm, LlmChatParams, LlmEvent, LlmEventStream, LlmUsage, ModelCapability, ProviderErrorInfo};
use crate::message::Message;
use crate::tool::ToolDefinition;
use crate::EngineResult;

/// The instruction appended as the final user message of the side call.
pub const TITLE_GENERATION_INSTRUCTION: &str = "Based on the conversation above, write a short title (3-8 words) that captures the user's main intent in this session.\n\nRules:\n- Reply with the title text only — no quotes, no trailing punctuation, no explanation.\n- Use the same language as the user's first message.\n- Never include secrets, tokens, or credentials.";

/// Run the one-shot side title call. `messages` must already be the
/// media-adapted projection the main loop sent; the instruction message
/// is appended here. Returns the raw model text (the caller sanitizes
/// through `set_auto_title`) plus the usage buckets for cache
/// observability (rust AGENTS.md gate 5).
pub async fn generate_session_title(
    llm: &dyn Llm,
    messages: Vec<Message>,
    tools: Vec<ToolDefinition>,
) -> EngineResult<(String, LlmUsage)> {
    todo!("implemented in Step 3")
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::stream;
    use std::sync::{Arc, Mutex};

    /// Minimal scripted mock mirroring the `MockLlm` pattern in
    /// `tests/compaction_tests.rs:171` — captures the chat params and
    /// replies with one text part.
    struct TitleMockLlm {
        captured: Mutex<Vec<LlmChatParams>>,
        events: Mutex<Vec<LlmEvent>>,
    }

    impl TitleMockLlm {
        fn replying(text: &str) -> Arc<Self> {
            Arc::new(Self {
                captured: Mutex::new(Vec::new()),
                events: Mutex::new(vec![
                    LlmEvent::TextPart { text: text.to_string() },
                    LlmEvent::Done { stop_reason: "end_turn".to_string() },
                ]),
            })
        }
        fn erroring(message: &str) -> Arc<Self> {
            Arc::new(Self {
                captured: Mutex::new(Vec::new()),
                events: Mutex::new(vec![LlmEvent::Error { message: message.to_string() }]),
            })
        }
    }

    impl Llm for TitleMockLlm {
        fn system_prompt(&self) -> &str { "system" }
        fn model_name(&self) -> &str { "mock-model" }
        fn capability(&self) -> &ModelCapability {
            static CAP: ModelCapability = ModelCapability {
                image_in: false,
                video_in: false,
                audio_in: false,
                dynamically_loaded_tools: false,
            };
            &CAP
        }
        fn is_retryable_error(&self, _error: &ProviderErrorInfo) -> bool { false }
        fn chat(&self, params: LlmChatParams) -> EngineResult<LlmEventStream> {
            self.captured.lock().unwrap().push(params.clone());
            let events = self.events.lock().unwrap().clone();
            Ok(Box::pin(stream::iter(events)))
        }
    }

    #[tokio::test]
    async fn should_return_the_model_title_and_append_the_instruction_as_the_last_message() {
        let llm = TitleMockLlm::replying("  Fix login timeout  ");
        let messages = vec![Message::user("my login always times out"), Message::assistant("let me look into it")];
        let tools = vec![ToolDefinition { name: "Bash".to_string(), description: "run".to_string(), schema: serde_json::json!({"type": "object"}) }];

        let (title, _usage) = generate_session_title(llm.as_ref(), messages, tools).await.unwrap();

        assert_eq!(title, "Fix login timeout");
        let captured = llm.captured.lock().unwrap();
        let params = captured.last().expect("one chat call");
        // The instruction rides as the final user message, after the
        // conversation prefix.
        let last = params.messages.last().expect("messages non-empty");
        assert_eq!(last.text_content(), TITLE_GENERATION_INSTRUCTION);
        assert_eq!(params.messages.len(), 3);
        assert_eq!(params.messages[0].text_content(), "my login always times out");
        // The tools pass through untouched (prefix fidelity).
        assert_eq!(params.tools.len(), 1);
        assert_eq!(params.tools[0].name, "Bash");
    }

    #[tokio::test]
    async fn should_fail_when_the_response_carries_no_text() {
        let llm = Arc::new(TitleMockLlm {
            captured: Mutex::new(Vec::new()),
            events: Mutex::new(vec![LlmEvent::Done { stop_reason: "end_turn".to_string() }]),
        });
        let result = generate_session_title(llm.as_ref(), vec![Message::user("hi")], Vec::new()).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async should_surface_stream_errors() {
        let llm = TitleMockLlm::erroring("provider exploded");
        let result = generate_session_title(llm.as_ref(), vec![Message::user("hi")], Vec::new()).await;
        assert!(result.is_err());
    }
}
```

实现前先打开 `rust/crates/mirri-core/src/llm.rs:318-390` 与 `tests/compaction_tests.rs:171-216`，把 mock 的 trait 方法签名对齐到**当前**签名（以编译器为准，勿照抄本计划若已有漂移）；`ToolDefinition` 的字段名以 `rust/crates/mirri-core/src/tool.rs` 实际定义为准。

- [ ] **Step 2: 运行测试确认失败**

```bash
cd rust && cargo test -p mirri-core --lib session::title_llm
```

预期：编译通过，`should_return_the_model_title_and_append_the_instruction_as_the_last_message` FAIL（`todo!()` panic）。

- [ ] **Step 3: 最小实现**

```rust
pub async fn generate_session_title(
    llm: &dyn Llm,
    mut messages: Vec<Message>,
    tools: Vec<ToolDefinition>,
) -> EngineResult<(String, LlmUsage)> {
    messages.push(Message::user(TITLE_GENERATION_INSTRUCTION));
    let mut params = LlmChatParams {
        messages,
        tools,
        // A fresh token: the settled turn's token may already be
        // cancelled, and this call must never be tied to the turn's
        // lifecycle.
        cancel: crate::cancel::CancelToken::new(),
        request_log_fields: Some(serde_json::json!({ "purpose": "session-title" })),
    };
    if let Err(error) = llm.materialize_media(&mut params).await {
        tracing::warn!(
            %error,
            "title-call media materialization failed; sending inline media unchanged"
        );
    }
    let stream = llm.chat(params)?;
    let collected = crate::loop_engine::shared::collect_stream_events(stream, None).await;
    if let Some(message) = collected.stream_error {
        return Err(EngineError::ProviderError(message));
    }
    let usage = collected.usage.clone();
    let text = collected.text_parts.join("");
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err(EngineError::ProviderError(
            "title call returned no text".to_string(),
        ));
    }
    Ok((trimmed.to_string(), usage))
}
```

（`CollectedStream` 的字段名 `stream_error` / `text_parts` / `usage` 以 `loop_engine/shared.rs` 实际定义为准；`materialize_media` 有默认空实现，参照 `compaction/round.rs:449-458` 的 `call_llm_once`。）

- [ ] **Step 4: 运行测试确认通过**

```bash
cd rust && cargo test -p mirri-core --lib session::title_llm
```

预期：3 个测试 PASS。

- [ ] **Step 5: clippy**

```bash
cd rust && cargo clippy -p mirri-core --all-targets -- -D warnings
```

预期：零告警。

- [ ] **Step 6: Commit**

```bash
git add rust/crates/mirri-core/src/session/title_llm.rs rust/crates/mirri-core/src/session/mod.rs
git commit -m "feat(core): add out-of-band session title generation call"
```

---

### Task 2: core — `SessionMetaHandle` 一次性 claim

**Files:**
- Modify: `rust/crates/mirri-core/src/scope/session_scope.rs`（`SessionMetaHandle`，232-285 行区域；内联测试模块在 2122 行附近）

**Interfaces:**
- Consumes: 既有 `SessionMetaHandle`（`metadata: Arc<RwLock<SessionMetadata>>` + `title_materialized: Arc<AtomicBool>`）。
- Produces（Task 3 依赖）:
  - `pub fn claim_side_title_generation(&self) -> bool`
  - `pub fn release_side_title_generation(&self)`
  - 字段 `side_title_claimed: Arc<AtomicBool>`（构造处初始化为 false——所有构造 `SessionMetaHandle { .. }` 的位置都要加该字段；用 `rg -n "SessionMetaHandle \{" rust/crates` 找全）

- [ ] **Step 1: 写失败测试**

在 `session_scope.rs` 现有内联测试模块（`should_set_auto_title_when_session_is_untitled` 所在处）追加：

```rust
#[test]
fn should_allow_only_the_first_side_title_generation_claim_per_process() {
    let handle = test_meta_handle_with_untitled_title();
    assert!(handle.claim_side_title_generation());
    assert!(!handle.claim_side_title_generation());
}

#[test]
fn should_allow_a_new_claim_after_a_released_failure() {
    let handle = test_meta_handle_with_untitled_title();
    assert!(handle.claim_side_title_generation());
    handle.release_side_title_generation();
    assert!(handle.claim_side_title_generation());
}
```

`test_meta_handle_with_untitled_title()`：若同模块已有构造 `SessionMetaHandle` 的测试 helper 则复用；否则按现有测试的构造方式新建（`Arc<RwLock<SessionMetadata>>` 初始为占位标题 `"Untitled Session"`、`is_custom_title: false`）。

- [ ] **Step 2: 运行测试确认失败**

```bash
cd rust && cargo test -p mirri-core --lib scope::session_scope
```

预期：编译失败——`claim_side_title_generation` 未定义。

- [ ] **Step 3: 最小实现**

在 `SessionMetaHandle` 中新增字段与方法（保持既有 `title_materialized` 不动——它在 Task 4 才移除）：

```rust
pub struct SessionMetaHandle {
    metadata: Arc<RwLock<SessionMetadata>>,
    title_materialized: Arc<AtomicBool>,
    /// One-shot claim for the out-of-band title generation (ADR-020):
    /// only the first claimant per process lifetime spawns the side
    /// call. Not persisted — after a restart an untitled session simply
    /// retries at the next user-turn settle (A8 self-healing).
    side_title_claimed: Arc<AtomicBool>,
}

impl SessionMetaHandle {
    /// Claim the one-shot side-call slot. Returns `true` only for the
    /// first caller per process lifetime.
    pub fn claim_side_title_generation(&self) -> bool {
        !self.side_title_claimed.swap(true, Ordering::AcqRel)
    }

    /// Release the claim after a failed generation attempt that left
    /// the session untitled, so the next user-turn settle retries.
    pub fn release_side_title_generation(&self) {
        self.side_title_claimed.store(false, Ordering::Release);
    }
}
```

- [ ] **Step 4: 运行测试确认通过**

```bash
cd rust && cargo test -p mirri-core --lib scope::session_scope && cargo clippy -p mirri-core --all-targets -- -D warnings
```

预期：PASS + 零告警（若其它包编译因构造处缺字段报错，把 `side_title_claimed: Arc::new(AtomicBool::new(false))` 补进对应构造点）。

- [ ] **Step 5: Commit**

```bash
git add rust/crates/mirri-core/src/scope/session_scope.rs
git commit -m "feat(core): add one-shot side title generation claim on session meta handle"
```

---

### Task 3: embedded — 旁路任务编排 + settle 站点接线

**Files:**
- Create: `rust/crates/mirri-embedded/src/embedded_server/side_title.rs`
- Modify: `rust/crates/mirri-embedded/src/embedded_server/mod.rs`（注册 `mod side_title;`——与 `message_handlers`、`session_index` 同级，先 `rg -n "mod message_handlers" rust/crates/mirri-embedded/src/embedded_server/mod.rs` 确认注册位置）
- Modify: `rust/crates/mirri-embedded/src/embedded_server/message_handlers.rs`（三个成功 settle 站点 + `apply_fallback_title` 迁移）
- Test: `rust/crates/mirri-embedded/src/embedded_server/tests/t9_session_title_tests.rs`

**Interfaces:**
- Consumes: Task 1 的 `mirri_core::session::title_llm::generate_session_title`；Task 2 的 `claim/release_side_title_generation`；`ConfigResolvingLlm`（`providers/config_factory.rs`）；`MessagePipeline`/`VideoReferenceResolver`/`MediaCapabilityAdapter`（`mirri-core` 根 re-export，若未 re-export 则用 `mirri_core::message_pipeline::{...}`——以 `turn_step.rs:165-176` 的既有 use 为准）；`note_turn_settled`（`session_index.rs:652`，`pub(super)`）；`SessionEvent::SessionMetaUpdated`。
- Produces（Task 4/5 依赖）:
  - `pub(super) struct SideTitleContext { pub session: Arc<SessionScope>, pub agent: Arc<AgentScope>, pub llm_factory: Arc<dyn mirri_core::LlmFactory>, pub registry: Arc<WorkspaceRegistry>, pub index: Option<Arc<SessionIndex>>, pub event_bus: Arc<mirri_core::session::EventBus>, pub user_text: String }`
  - `pub(super) fn spawn_side_title_generation(ctx: SideTitleContext)`
  - `pub(super) async fn apply_fallback_title(event_bus: &mirri_core::session::EventBus, meta: &mirri_core::scope::SessionMetaHandle, user_text: &str) -> Option<String>`（从 message_handlers.rs:270 **迁移**过来，原处删除并改 import）

- [ ] **Step 1: 写失败测试（t9 重写第一批）**

按 D8 处置表，在 `t9_session_title_tests.rs` 中：

1. **重写** `should_title_via_tool_call_within_first_user_turn` 为旁路调用命名（保留原函数名与 Given-When-Then 语义，改为断言旁路标题）：

```rust
/// Given an untitled session and a scripted LLM whose FIRST response is the
/// main turn's text and SECOND response is a concise title, when a wait-mode
/// `message.send` completes, then the session title is the side call's title
/// (not the prompt-text excerpt), `is_custom_title` stays false, the wire
/// carries `event.session.meta.updated`, and `state.json` on disk carries the
/// title (the side call's own write-through).
#[tokio::test]
async fn should_title_via_side_llm_call_after_the_first_user_turn_settles() {
    // Given a server whose scripted LLM serves the main turn, then the
    // side title call
    let mock = Arc::new(ScriptedLlm::new());
    mock.push_text("Done.");
    mock.push_text("Refactor the auth module");
    let (server, _tmp) = test_server(Some(Arc::new(ScriptedLlmFactory::new(Arc::clone(&mock)))));
    let created = server.call_method("session.create", json!({})).await.unwrap();
    let session_id = created["id"].as_str().unwrap().to_string();
    let mut events = server.subscribe_events().await.unwrap();

    // When the first prompt is sent (wait mode)
    let result = server
        .call_method("message.send", json!({
            "session_id": &session_id,
            "text": "please help me restructure the authentication code",
            "mode": "wait",
        }))
        .await
        .unwrap();

    // Then the turn completed and the side call titled the session
    assert_eq!(result["ok"], true);
    let event = next_event_where(&mut events, |n| n.method == "event.session.meta.updated", |_| {}).await;
    assert_eq!(event.params["title"], "Refactor the auth module");

    let session = server
        .session_manager()
        .get_session(&SessionId::new(&session_id))
        .await
        .expect("session stays live");
    let metadata = session.metadata().await;
    assert_eq!(metadata.title, "Refactor the auth module");
    assert!(!metadata.is_custom_title);

    // And the side call's write-through persisted the title to disk
    let state = std::fs::read_to_string(
        session_dir_under(&_tmp, &session_id).join("state.json"),
    )
    .unwrap();
    assert!(state.contains("Refactor the auth module"));
}
```

（`session_dir_under` 等 helper 若 t9/共享 harness 无现成实现，参照 t9 中 `should_write_the_title_to_state_json_mid_turn_before_the_turn_settles`（80-160 行）读取 state.json 的既有方式复用其路径构造；`ScriptedLlm` 的 `push_text` 两连发语义：第一个响应给主轮次、第二个给旁路调用——同一 resolver、同一 mock。）

2. **重写** `should_fall_back_to_prompt_text_when_model_skips_the_tool` → `should_fall_back_to_prompt_text_when_the_side_title_call_fails`：`mock.push_text("Done.")` 后 `mock.push_error("side call exploded")`（ScriptedLlm 的错误脚本 API 以 harness 实际为准，如 `push_chat_error`），断言标题为用户消息文本截取、且 `event.session.meta.updated` 的 title 是截取值。
3. **重写** `should_not_overwrite_tool_title_with_fallback` → `should_not_overwrite_a_side_call_title_with_fallback`：旁路调用成功（第二个响应给标题）后，再发第二条消息且 mock 第三个响应报错，断言标题不变。
4. **重写** `should_never_overwrite_a_user_renamed_title`：在旁路调用前用户先 `session.rename`，断言旁路写入被守卫拒绝、标题保持用户值（原测试的时序骨架保留，把"模型工具调用"换成"第二个 mock 响应返回的旁路标题"）。
5. **重写** `should_redact_credentials_in_tool_title_argument` → `should_redact_credentials_in_the_side_call_title`：第二个响应返回含 `sk-...` 密钥的标题文本，断言写入值经既有脱敏。
6. **重写** `should_treat_idle_steer_as_first_user_turn` 与 `should_fall_back_when_an_idle_steer_skips_the_tool`：idle steer 路径改断言旁路调用语义（成功→LLM 标题；失败→兜底），时序骨架保留。
7. **重写** `should_fall_back_when_first_turn_is_cancelled`：取消轮次后旁路调用仍触发（mock 第二个响应返回标题）→ 标题为旁路标题；再加一个变体：mock 第二个响应错误 → 兜底。（保留原名。）
8. **重写** `should_keep_untitled_when_first_prompt_has_no_text`：纯图片首条消息 + mock 旁路响应报错 → 保持未命名，且（可选）再发一条带文本消息 + mock 成功响应 → 旁路重试成功（验证 release 语义）。
9. **删除** `should_write_the_title_to_state_json_mid_turn_before_the_turn_settles`（其 state.json 断言已并入第 1 项新测试）。

每条重写先跑确认 FAIL（当前实现走规则式兜底，断言 LLM 标题必失败）：

```bash
cd rust && cargo test -p mirri-embedded --lib embedded_server::tests::t9_session_title_tests
```

- [ ] **Step 2: 实现 `side_title.rs`**

```rust
//! Out-of-band session-title generation (spec 09 / ADR-020): after a
//! successful main-agent user turn settles on an untitled session, spawn
//! a side LLM call that rides the provider's prompt cache — same
//! resolver construction, same system prompt (held by the adapter), same
//! tool list, same projected message prefix as the main loop's request —
//! with one appended instruction message. The main turn loop is
//! untouched: nothing here records into context memory or the wire
//! log, and the response never enters the conversation.

use std::sync::Arc;
use std::time::Duration;

use mirri_core::session::EventBus;
use mirri_core::session::SessionEvent;
use mirri_core::scope::AgentScope;
use mirri_core::scope::SessionScope;
use mirri_core::{AgentScopeHandle as _, MediaCapabilityAdapter, MessagePipeline, VideoReferenceResolver};

use crate::embedded_server::session_index::note_turn_settled;
use crate::embedded_server::SessionIndex;
use crate::embedded_server::WorkspaceRegistry;
use crate::providers::ConfigResolvingLlm;

/// The side call must never linger: a wedged provider falls back to the
/// rule-based title instead of blocking nothing forever.
const SIDE_TITLE_TIMEOUT: Duration = Duration::from_secs(30);

pub(super) struct SideTitleContext {
    pub session: Arc<SessionScope>,
    pub agent: Arc<AgentScope>,
    pub llm_factory: Arc<dyn mirri_core::LlmFactory>,
    pub registry: Arc<WorkspaceRegistry>,
    pub index: Option<Arc<SessionIndex>>,
    pub event_bus: Arc<EventBus>,
    pub user_text: String,
}

/// The rule-based fallback (spec 09 §2.3): derive a title from the user's
/// message text via the same guard and sanitize path as the side call
/// (`set_auto_title` is idempotent: a titled/custom session is a silent
/// no-op, and a prompt with no text derives nothing). Returns the stored
/// title when this call wrote one; the wire announcement mirrors the
/// side call's `SessionMetaUpdated` event.
pub(super) async fn apply_fallback_title(
    event_bus: &EventBus,
    meta: &mirri_core::scope::SessionMetaHandle,
    user_text: &str,
) -> Option<String> {
    let stored = meta.set_auto_title(user_text.to_string()).await?;
    event_bus.publish(SessionEvent::SessionMetaUpdated {
        title: Some(stored.clone()),
        patch: None,
    });
    Some(stored)
}

pub(super) fn spawn_side_title_generation(ctx: SideTitleContext) {
    let SideTitleContext { session, agent, llm_factory, registry, index, event_bus, user_text } = ctx;
    let meta = session.meta_handle();
    tokio::spawn(async move {
        // Guard: main-agent sessions only, untitled, one claim per process.
        {
            let metadata = session.metadata().await;
            if metadata.is_custom_title
                || !mirri_core::session::title::is_untitled_title(&metadata.title)
            {
                return;
            }
        }
        if !meta.claim_side_title_generation() {
            return;
        }
        // Same resolver construction as the main loop (message_handlers
        // dispatch): the side call resolves the same provider/model and
        // the same frozen system prompt as the settled turn.
        let resolver = ConfigResolvingLlm::new(agent.config(), llm_factory);
        let llm = match resolver.resolve() {
            Ok(llm) => llm,
            Err(reason) => {
                tracing::warn!(%reason, "side title call: provider resolution failed; falling back");
                meta.release_side_title_generation();
                let _ = apply_fallback_title(&event_bus, &meta, &user_text).await;
                note_turn_settled(index, &registry, &session).await;
                return;
            }
        };
        // Rebuild the exact request prefix the main loop sent: the
        // normal projection (no micro-compaction — the prefix-cache
        // preservation choice) plus the same media adaptation pipeline.
        let messages = {
            let memory = agent.context_memory();
            let mem = memory.lock().unwrap_or_else(|e| e.into_inner());
            mem.messages()
        };
        let mut tools = {
            let manager = agent.tool_manager();
            let guard = manager.read();
            guard.loop_tools()
        };
        let capability = llm.capability();
        if !capability.image_in && !capability.video_in {
            tools.retain(|tool| tool.name != "ReadMediaFile");
        }
        let mut pipeline = MessagePipeline::new();
        pipeline.register(Box::new(VideoReferenceResolver::for_capability(capability)));
        pipeline.register(Box::new(MediaCapabilityAdapter::new(capability.clone())));
        let messages = pipeline.run("normal", messages);

        let outcome = tokio::time::timeout(
            SIDE_TITLE_TIMEOUT,
            mirri_core::session::title_llm::generate_session_title(llm.as_ref(), messages, tools),
        )
        .await;

        match outcome {
            Ok(Ok((raw, usage))) => {
                // Cache observability (rust AGENTS.md gate 5): the three
                // buckets — uncachedInput / cacheRead / cacheWrite.
                tracing::info!(
                    session_id = %session.id(),
                    uncached_input = usage.input_tokens,
                    cache_read = usage.cache_read_tokens,
                    cache_write = usage.cache_creation_tokens,
                    output = usage.output_tokens,
                    "side title call usage"
                );
                if let Some(stored) = meta.set_auto_title(raw).await {
                    event_bus.publish(SessionEvent::SessionMetaUpdated {
                        title: Some(stored),
                        patch: None,
                    });
                    // The settle's write-through ran before the title
                    // landed — write it through again so state.json and
                    // the index carry the title.
                    note_turn_settled(index, &registry, &session).await;
                }
            }
            Ok(Err(error)) => {
                tracing::warn!(%error, "side title call failed; falling back to the rule-based title");
                meta.release_side_title_generation();
                let _ = apply_fallback_title(&event_bus, &meta, &user_text).await;
                note_turn_settled(index, &registry, &session).await;
            }
            Err(_) => {
                tracing::warn!("side title call timed out; falling back to the rule-based title");
                meta.release_side_title_generation();
                let _ = apply_fallback_title(&event_bus, &meta, &user_text).await;
                note_turn_settled(index, &registry, &session).await;
            }
        }
    });
}
```

实现说明（执行者注意）：
- 顶部 use 列表按编译器修正：`AgentScopeHandle` 这类不存在/不需要的 import 删掉；`agent.context_memory()`、`agent.tool_manager()`、`agent.config()` 均为 `AgentScope` 既有 pub 方法（`agent_scope.rs:303/396`）；`agent.tool_manager()` 返回的锁类型（parking_lot 或 std）以 `message_handlers.rs:543` 的既有用法为准调整 `.read()` 形态。
- `pipeline.run("normal", messages)` 的签名以 `turn_step.rs:262` 的调用为准（返回 `Vec<Message>`）。
- `WorkspaceRegistry` / `SessionIndex` / `EventBus` 的实际类型与 import 路径以 `message_handlers.rs` / `session_index.rs` 顶部既有 use 为准（`event_bus` 若在 artifacts 中不是 `Arc<EventBus>` 而是 `EventBus` 本体可 Clone，则以 fire 模式 capture 处的类型为准）。
- 在 `embedded_server/mod.rs` 注册 `mod side_title;`。

- [ ] **Step 3: 接线三个成功 settle 站点**

`message_handlers.rs`：

1. 删除原 `apply_fallback_title`（270-281 行），顶部加 `use super::side_title::{apply_fallback_title, spawn_side_title_generation, SideTitleContext};`（错误 settle / abort / no-provider 站点继续用它）。
2. **wait 成功站点**（875-883 行）——把 `if agent_id == DEFAULT_MAIN_AGENT_ID { apply_fallback_title(...); unmount_title_reminder(...); }` 替换为：

```rust
if agent_id == DEFAULT_MAIN_AGENT_ID {
    // ADR-020: the successful settle hands the naming question to the
    // out-of-band side call (cache-riding, one-shot); the reminder
    // unmount is obsolete once Task 4 removes the injector.
    spawn_side_title_generation(SideTitleContext {
        session: Arc::clone(&session),
        agent: Arc::clone(&agent),
        llm_factory: llm_factory.clone(),
        registry: Arc::clone(&self.registry),
        index: self.session_index(),
        event_bus: artifacts.event_bus.clone(),
        user_text: user_text.clone(),
    });
    unmount_title_reminder(&settle_flow);
}
```

（`agent` 的实际绑定类型若不是 `Arc<AgentScope>` 而是其它智能指针，按 dispatch 顶部 364-371 行的绑定调整 clone 方式。）
3. **fire 成功站点**（1017-1025 行）：fire 任务 capture 列表（930-948 行区域）补 `let task_agent = Arc::clone(&agent);` 与 `let task_llm_factory = llm_factory.clone();`、`let task_registry = Arc::clone(&self.registry);`（fire 任务内拿不到 `&self`），settle 处替换为同样的 `spawn_side_title_generation(SideTitleContext { session: Arc::clone(&task_session), agent: task_agent, llm_factory: task_llm_factory, registry: task_registry, index: settle_index, event_bus: event_bus.clone(), user_text: task_user_text.clone() })`。
4. **steer 成功站点**（1716-1722 行）：读 steer 臂的上下文（1559-1729 行），按 wait 站点同样模式替换；steer 臂若未 capture agent/factory/registry，按第 3 项方式补 capture。
5. 错误 settle（820/1053/1685）、abort（1290）、no-provider（498）站点**保持** `apply_fallback_title` 调用不变（import 路径变了而已）。

- [ ] **Step 4: 运行测试确认通过**

```bash
cd rust && cargo test -p mirri-embedded --lib embedded_server::tests::t9_session_title_tests
```

预期：重写后的测试全部 PASS；`should_persist_the_derived_title_when_no_llm_provider_is_configured`、`should_write_a_user_rename_to_state_json_immediately`、`should_keep_placeholder_title_while_first_turn_is_in_flight` 保持绿。此时工具与注入器仍在（过渡态：模型若轮内调用工具，标题轮内落定，旁路守卫拒绝——无害）。

- [ ] **Step 5: 门禁**

```bash
cd rust && cargo test -p mirri-embedded --lib && cargo clippy -p mirri-embedded --all-targets -- -D warnings
```

预期：全绿 + 零告警（t9 之外的既有测试若因 settle 语义变化失败，逐个核对：凡断言"首轮 settle 后标题=截取"的测试按新语义更新为旁路/兜底二选一的断言）。

- [ ] **Step 6: Commit**

```bash
git add rust/crates/mirri-embedded/src/embedded_server/
git commit -m "feat(embedded): generate session titles via cache-riding side LLM call at turn settle"
```

---

### Task 4: 机制退役 — 移除工具、注入器与全部管线

**Files:**
- Delete: `rust/crates/mirri-core/src/tools/set_session_title.rs`、`rust/crates/mirri-core/src/turn_flow/injection/title.rs`
- Modify: `rust/crates/mirri-core/src/tools/mod.rs`、`rust/crates/mirri-core/src/turn_flow/injection/mod.rs`、`rust/crates/mirri-core/src/turn_flow/mod.rs`、`rust/crates/mirri-core/src/scope/config.rs`、`rust/crates/mirri-core/src/scope/agent_scope.rs`、`rust/crates/mirri-core/src/scope/session_scope.rs`、`rust/crates/mirri-embedded/src/embedded_server/message_handlers.rs`、`rust/crates/mirri-embedded/src/embedded_server/session_handlers.rs`、`rust/crates/mirri-core/src/session/manager/restore.rs`、`rust/crates/mirri-embedded/src/embedded_server/cron_driver.rs`、`rust/crates/mirri-embedded/src/embedded_server/task_driver.rs`
- Test: t9 处置表的 Task 4 行 + 全仓引用清理

**Interfaces:**
- Consumes: Task 3 已就位的 `spawn_side_title_generation`（错误/abort 站点不再需要 `unmount_title_reminder`）。
- Produces: 无新增对外接口；`SetSessionTitle` 从 `BUILTIN_TOOL_NAMES`/`BUILTIN_TOOL_CATALOG` 消失；`TurnFlow` 不再有 `title_injector`。

- [ ] **Step 1: 全量引用盘点**

```bash
rg -n "SetSessionTitle|TitleInjector|title_injector|title_handle|title_materialized|mark_title_materialized|clear_title_reminder|unmount_title_reminder|needs_title" rust/ packages/ apps/ docs/ --glob '!docs/superpowers/**'
```

把每一处命中归入：删除 / 改注释 / t9 处置。此清单即本任务的完成定义。

- [ ] **Step 2: 写失败测试（行为先行：工具与注入面为空）**

t9 按处置表执行 Task 4 行：
1. 删除 `should_extinguish_the_reminder_and_keep_the_title_tool_active_after_naming`、`should_not_offer_title_tool_on_subagents`、`should_extinguish_reminder_when_title_written_via_update_meta_mid_turn`。
2. `should_not_fall_back_or_inject_for_cron_turns`：断言改为——cron 轮次后 `session.meta` 标题仍为占位、且（若 harness 可观测）主循环请求的 tools 列表不含 `SetSessionTitle`。
3. `should_close_the_abort_residue_before_the_next_cron_turn`：删除注入器残余相关断言，保留其余 abort 语义断言；若整个测试只剩空壳则删除。
4. 新增一条防回归测试：

```rust
/// Given a fresh untitled session, when the first turn runs, then the
/// serialized tool list offered to the model contains no
/// `SetSessionTitle` entry (the mechanism is retired; naming is
/// out-of-band).
#[tokio::test]
async fn should_not_offer_a_session_title_tool_to_the_model() {
    let mock = Arc::new(ScriptedLlm::new());
    mock.push_text("Done.");
    let (server, _tmp) = test_server(Some(Arc::new(ScriptedLlmFactory::new(Arc::clone(&mock)))));
    let created = server.call_method("session.create", json!({})).await.unwrap();
    let session_id = created["id"].as_str().unwrap().to_string();
    server.call_method("message.send", json!({
        "session_id": &session_id, "text": "hello", "mode": "wait",
    })).await.unwrap();

    let params = mock.captured_params();
    let first = params.first().expect("main loop call captured");
    assert!(first.tools.iter().all(|t| t.name != "SetSessionTitle"));
}
```

（`captured_params()`：若 `ScriptedLlm` 尚无 params 捕获，给 `ScriptedLlm` 增加 `captured: Mutex<Vec<LlmChatParams>>` 并在 `chat` 里 push——与 Task 5 复用同一捕获。）

运行确认：

```bash
cd rust && cargo test -p mirri-embedded --lib embedded_server::tests::t9_session_title_tests
```

预期：新测试 FAIL（工具仍注册）。

- [ ] **Step 3: 移除实现（按 D6 清单逐项）**

1. 删除两个文件；`tools/mod.rs` 移除模块声明、re-export、`BuiltinToolServices.title` 字段与注册块、`BUILTIN_TOOL_NAMES`/`BUILTIN_TOOL_CATALOG` 条目；`injection/mod.rs` 移除 `title` 模块；`turn_flow/mod.rs` 移除 `title_injector` 字段与 `set_title_injector`。
2. `message_handlers.rs`：删除挂载块（595-607、1559-1572——注意保留紧随其后的 plugin sessionStart 注入块 608+ 不动）、`unmount_title_reminder` fn 及其 6 处调用、`run_engine_turn` 的 `clear_title_reminder` 参数与消费块（114-179 注释一并清理）、wait/fire/steer 调用处删去该实参；同时清理 Step 3 接线时留下的过渡 `unmount_title_reminder(&settle_flow)` 行。
3. `cron_driver.rs:526,564`、`task_driver.rs:762,801`：删除 `clear_title_reminder` 计算与传参。
4. `scope/config.rs`：删 `title_handle` 字段（358）与默认值（435）；`agent_scope.rs:211` 删接线；`session_handlers.rs:409`、`restore.rs:183` 删写入点；`BuiltinToolServices` 的全部构造点（含 subagent host 的 `title: None`）同步删字段。
5. `session_scope.rs`：删 `title_materialized` 字段、`set_auto_title` 内的置位行（260）、`title_materialized()`（276）、`mark_title_materialized()`（282）及全部调用点（`session.rename`/`updateMeta`/restore 的置位点——Step 1 清单里都有）；doc 注释中对 `SetSessionTitleTool`/`TitleInjector` 的引用改写为旁路调用语义。
6. 重新跑 Step 1 的 `rg`：`rust/` 内零命中（docs 命中留给 Task 6）。

- [ ] **Step 4: 运行测试与门禁**

```bash
cd rust && cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings && node scripts/check-zero-io.mjs
```

预期：全绿、零告警、zero-IO 0 命中。若 `tool.catalog` 相关测试断言内置工具清单，同步更新。

- [ ] **Step 5: Commit**

```bash
git add -A rust/crates
git commit -m "refactor(core)!: retire the SetSessionTitle tool and title reminder injection"
```

（commit message 的 `!` 仅表达工具面移除；changeset 级别在 Task 7 与用户确认，不自行定 major。）

---

### Task 5: 缓存前缀保真测试 + 旧会话恢复兼容

**Files:**
- Test: `rust/crates/mirri-embedded/src/embedded_server/tests/t9_session_title_tests.rs`（追加）

**Interfaces:**
- Consumes: Task 3 的 `spawn_side_title_generation`（经 settle 自动触发）；Task 4 的 `ScriptedLlm::captured_params()`；t9 harness 的 `test_server`/wire 回放工具（参照 t9 或其它测试中 session resume 的既有用法，`rg -n "resume|restore" rust/crates/mirri-embedded/src/embedded_server/tests/ | head`）。

- [ ] **Step 1: 写失败/验证测试**

```rust
/// Given a completed first turn, when the side title call fires, then its
/// request prefix matches the main loop's request byte-for-byte up to the
/// appended instruction: same tool list in the same order, and a message
/// list that extends (never rewrites) the main call's messages — the
/// provider prefix-cache hit precondition (rust AGENTS.md gate 5).
#[tokio::test]
async fn should_ride_the_main_loop_request_prefix_for_cache_hits() {
    let mock = Arc::new(ScriptedLlm::new());
    mock.push_text("Done.");
    mock.push_text("Fix login timeout");
    let (server, _tmp) = test_server(Some(Arc::new(ScriptedLlmFactory::new(Arc::clone(&mock)))));
    let created = server.call_method("session.create", json!({})).await.unwrap();
    let session_id = created["id"].as_str().unwrap().to_string();
    let mut events = server.subscribe_events().await.unwrap();
    server.call_method("message.send", json!({
        "session_id": &session_id, "text": "my login always times out", "mode": "wait",
    })).await.unwrap();
    next_event_where(&mut events, |n| n.method == "event.session.meta.updated", |_| {}).await;

    let params = mock.captured_params();
    assert_eq!(params.len(), 2, "main call + side call");
    let (main_call, side_call) = (&params[0], &params[1]);
    // Tools identical, same order.
    assert_eq!(main_call.tools.len(), side_call.tools.len());
    for (a, b) in main_call.tools.iter().zip(side_call.tools.iter()) {
        assert_eq!(a.name, b.name);
        assert_eq!(a.schema, b.schema);
    }
    // The side call extends the main call's message prefix.
    assert!(side_call.messages.len() > main_call.messages.len());
    for (a, b) in main_call.messages.iter().zip(side_call.messages.iter()) {
        assert_eq!(a.role, b.role);
        assert_eq!(a.text_content(), b.text_content());
    }
    // The only tail additions are the settled turn's assistant reply
    // and the instruction message; nothing before it was rewritten.
    let tail = &side_call.messages[main_call.messages.len()..];
    assert!(tail.len() <= 2);
    assert_eq!(
        tail.last().unwrap().text_content(),
        mirri_core::session::title_llm::TITLE_GENERATION_INSTRUCTION
    );
}
```

（`Message` 的 `role` 字段比较方式以 `message.rs:103` 实际定义为准；若 `role` 无 `PartialEq` 或字段名不同，比较 `text_content()` + 消息数即可，勿为测试改生产类型。）

```rust
/// Given a legacy wire log whose recorded turn contains a `SetSessionTitle`
/// tool_use/tool_result pair, when the session is restored and a new turn
/// runs, then replay tolerates the retired tool (no execution, no error)
/// and the new turn settles normally.
#[tokio::test]
async fn should_restore_a_legacy_session_that_recorded_title_tool_calls() {
    // 参照 t9/harness 中既有的 wire-replay resume 测试构造：手工写一条
    // 含 SetSessionTitle tool_use 的 ContextAppendMessage wire op（或
    // 复制现有 fixtures 的写法），resume 后发一条新消息，断言 turn 正常
    // 完成、无 unknown tool 错误。
}
```

（第二条测试的 wire op 构造方式以 harness 既有 resume 测试为准——`rg -n "ContextAppendMessage" rust/crates/mirri-embedded/src/embedded_server/tests/` 找现成写法照抄结构，把 tool_use 内容换成 `SetSessionTitle`。实现时若发现旧 tool_use 在回放路径本就不执行（仅作为消息内容恢复），则该测试退化为"回归护栏"——照写即可。）

- [ ] **Step 2: 运行测试确认通过（若 FAIL 则修 prefix 构造）**

```bash
cd rust && cargo test -p mirri-embedded --lib embedded_server::tests::t9_session_title_tests
```

预期：PASS。若前缀对比失败，说明 Task 3 的重建路径与主循环有偏差（典型：tools 顺序、media pipeline、投影选项）——修 `side_title.rs` 而不是改测试，直到前缀对齐。**这是 AGENTS.md 契约 5 的硬验收，不允许跳过。**

- [ ] **Step 3: 真实 provider 手工验证（PR 描述必附证据）**

在本地跑一次真实会话（`pnpm dev:v2` 或桌面端，任一配置了真实 provider 的入口）：

1. 新建会话，发一条消息，等助手回复 + 标题出现；
2. 从日志中找 `side title call usage` 行，记录三桶数值（uncached_input / cache_read / cache_write）；
3. 把数值与"主循环同轮请求的 usage"一起贴进 PR 描述（对照证明旁路调用的输入几乎全为 cache read）。

若日志无该行（日志级别问题），用 `RUST_LOG=mirri_embedded=info` 重跑。无法连真实 provider 时，在 PR 中明确说明"仅完成前缀对比自动化证据，三桶数值待补"——不得伪造数值。

- [ ] **Step 4: Commit**

```bash
git add rust/crates/mirri-embedded/src/embedded_server/tests/t9_session_title_tests.rs
git commit -m "test(embedded): assert side title call rides the main-loop request prefix"
```

---

### Task 6: spec 09 终态改写 + 文档清扫

**Files:**
- Modify: `docs/specs/rust-core-design/09-session-title-generation.md`
- Modify: `docs/specs/rust-core-design/ROADMAP.md`（按该域治理约定记录条目；`context.md` §7 如有现状描述按需更新）

**Interfaces:** 无代码接口；产出为文档。

- [ ] **Step 1: 在 spec 09 附录 A 追加 ADR-020**（保持"只写终态、决策记录入附录"的治理约定；SDR 层面补一条 SDR 记录 R-H 的需求变更——"零额外调用"改为"一次缓存骑乘的旁路调用"）：

ADR-020 要点（正文按此展开）：
- **上下文与动机**：工具机制三重痛点——① 工具 schema 常驻每个请求（ADR-019 的交换代价）；② 提醒注入与 tool-call 帧进入缓存上下文（ADR-018 的轮内注入成本）；③ 占用内置工具面。且工具路径的"零额外请求"收益可被旁路调用的 provider 前缀缓存命中替代：同模型、同前缀的旁路调用输入几乎全为缓存读（~0.1x 单价）。
- **决策**：D1-D5 全部条目（触发点、请求构造、降级链、claim 守卫、不记 wire log）。
- **被否决的备选**：D1 表格五项。
- **supersede 关系**：ADR-016 的"独立调用成本不可接受"裁决被缓存骑乘论证推翻（推翻理由：彼时未考虑前缀复用）；ADR-017 的注入+工具机制、ADR-018 的注入器熄灭、ADR-019 的工具常驻全部随机制移除而失效（守卫与降级链语义被继承）。

- [ ] **Step 2: 改写 spec 09 正文终态**（不是追加变更日志——spec 只持终态）：
- §1.2：R-A 改为旁路调用表述；R-H 改为"命名经由一次缓存骑乘的旁路调用完成，不进入主会话上下文与 wire log"。
- §1.4 验收表：A1/A7/A8 按新语义改写（A1: settle 后旁路调用命名；A7: 主循环请求中不再出现任何命名指令与标题工具——增量严格为零；A8: 崩溃窗口自愈语义保留）；新增 A12：旁路调用请求前缀与主循环请求逐字节前缀对齐（Task 5 测试为其自动化证明）。
- §2.1 流程图、§2.3 降级链表（用 D4 表替换）、§2.4 数据契约（移除工具与 wire log 记录条目）、§2.5 成本语义（用 D3 分析替换）。
- §3 备选表、§4 风险表同步更新。

- [ ] **Step 3: 文档清扫**

```bash
rg -n "SetSessionTitle" docs/ apps/ packages/ --glob '!docs/superpowers/**' --glob '!**/node_modules/**'
```

每处命中：用户文档（docs/zh、docs/en）中描述"模型调用工具命名会话"的表述改为旁路调用表述（中英双语同步，遵循 translate-docs 惯例）；代码注释残留已在 Task 4 清理。ROADMAP 按其格式记一条。

- [ ] **Step 4: Commit**

```bash
git add docs/specs/rust-core-design/ docs/zh/ docs/en/
git commit -m "docs(spec): rewrite spec 09 terminal state for out-of-band title generation (ADR-020)"
```

---

### Task 7: 全量门禁 + changeset

**Files:**
- Create: `.changeset/*.md`（经 gen-changesets skill）

**Interfaces:** 无。

- [ ] **Step 1: 全量门禁**

```bash
cd rust && cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings && node scripts/check-zero-io.mjs
```

预期：全绿。超时建议 ≥ 30 分钟（workspace 测试较重）。

- [ ] **Step 2: TS 侧确认零改动**

```bash
git diff --stat main...HEAD -- packages/ apps/ || git diff --stat -- packages/ apps/
```

预期：无 TS 变更（本计划全部落 rust + docs）。若有意外改动，核实来源。

- [ ] **Step 3: 生成 changeset**

调用 `.agents/skills/gen-changesets/SKILL.md`（gen-changesets skill）生成 changeset。级别判断：内置工具 `SetSessionTitle` 从工具面移除——若 gen-changesets 规则判定其为用户可见 breaking（例如用户 `tool.catalog` 配置引用该名将报错），**停下向用户说明并确认是否 major**；未经确认一律写 `minor`。

- [ ] **Step 4: 最终自查 + Commit**

```bash
git status && git diff --staged --stat
```

确认无草稿/交接文件混入后：

```bash
git add .changeset/
git commit -m "chore: add changeset for out-of-band session title generation"
```

---

## Parallelization

- Task 1 与 Task 2 **可并行**（不同文件、零依赖）。
- Task 3 依赖 Task 1 + 2，必须串行。
- Task 4 依赖 Task 3；Task 5 依赖 Task 3 + 4（`captured_params` 与工具移除后的 tools 断言）。
- Task 6 可与 Task 4/5 并行（纯文档，引用的终态由本计划 D1-D6 锁定）。
- Task 7 最后。

## Self-Review 记录

- **Spec 覆盖**：用户需求四要素——①不用工具（D6 全量移除）②不影响主会话/turn（旁路 spawn、不写 memory/wire log）③provider 输入缓存命中（D3 前缀对齐 + Task 5 硬验收 + 契约 5 三桶证据）④合理时机（D2 settle 触发）——均有对应任务。spec 09 的 R-A/R-B/R-C/R-D/R-E/R-F/R-G 在新机制下全部保持（守卫/兜底/事件/默认开启不变），R-H 按 SDR 变更。
- **占位符扫描**：Task 5 第二条测试（旧会话恢复）与 Task 3 的 steer 接线给出的是"参照既有 harness 写法"的指引而非完整代码——这两处的完整代码依赖执行时读到的 harness 现状，指引中已指明确切参照物与构造要求；其余步骤均含完整代码。
- **类型一致性**：`generate_session_title` 签名在 Task 1/3/5 一致；`SideTitleContext` 字段与 Task 3 三站点传参一致；`claim/release_side_title_generation` 在 Task 2/3 一致；`TITLE_GENERATION_INSTRUCTION` 在 Task 1/5 一致。mock 的 trait 签名已标注"以 `llm.rs:318-390` 与编译器为准"，防止计划与代码漂移。
