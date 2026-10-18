# Rust v3 内核会话审计报告：session-30c0d8bb-f60f-4d25-97f3-832b718b8daa

- **审计日期**：2026-09-14
- **审计对象**：Rust v3 内核下一次"子代理创建 HelloWorld（PHP + Rust）"的真实会话，重点覆盖子代理（Agent 工具）链路
- **数据来源**：
  - 会话目录：`~/.mirri-code/v3_sessions/wd_t1_44fdeaa12ba5/session-30c0d8bb-f60f-4d25-97f3-832b718b8daa/`（state.json、wire.jsonl、tasks/、agents/）
  - 两个子代理 journal：`agents/agent-de840f62-…/wire.jsonl`（PHP）、`agents/agent-9c865eb7-…/wire.jsonl`（Rust）
  - SQLite 索引：`~/.mirri-code/session-index.db`
  - Rust 源码：`rust/crates/mirri-core/src/{subagent/host.rs, turn_flow/{hooks.rs, injection/title.rs}, scope/session_scope.rs, permission/mod.rs, tools/path_security.rs}`、`rust/crates/mirri-embedded/src/embedded_server/message_handlers.rs`

## 一、会话概况

| 项 | 值 |
|---|---|
| workDir | `/Users/chengri.a.qi/workspace/opensource/t1`（会话创建时为空目录） |
| 用户提问 | "trigger subagent to create 2 helloword sample with PHP and rust, don't use swarm mode." |
| 模型 | alias `fast` → `DeepSeek-V4-Flash`（主代理与两个 coder 子代理同模型） |
| 执行过程 | 1 turn / 7 steps / 71s：主代理并行派出 2 个普通子代理（未用 swarm，符合要求）→ 两个子代理均无法写文件/Bash → 主代理自行创建 hello.php + hello_rust/ 并验证通过 |
| 结束状态 | `turn.ended reason=completed durationMs=71066` |
| 落盘产物 | `t1/hello.php`、`t1/hello_rust/{Cargo.toml, src/main.rs}`（php/cargo 双验证通过）+ 一个多余的空目录 `t1/hello_php_from_subagent` |

**上次审计（6b03e323）修复项回归验证：全部仍然生效**——Bash 在会话 workDir 执行（P0-1）、`updated_at` 随 turn 刷新且 SQLite 写通（P1-1）、`last_prompt` 落盘（P1-2）、wire 事件带 `time`（P2-1）、Bash 结果纯文本（P2-2）、state.json 带 `version:3`（P2-4）。

本轮聚焦子代理链路，共发现 **2 个 P0、3 个 P1、3 个 P2**。

---

## 二、问题清单

### P0-1 子代理没有任何审批通道，Manual 模式下全部写/Bash 操作被硬拒绝（严重：功能正确性）

**现象**：两个子代理的 journal 里，所有 `Write`（含工作目录内的合法写入）和 `Bash`（含 `pwd`、`which php` 等只读命令）调用均在 ~5ms 内返回同一个错误：`approval required but no approval mechanism available`。子代理从未产生任何 `approval.ask` 请求——客户端根本没有机会批准。而同一 turn 内主代理的 Bash/Write 都正常走 `permission.record_approval_result`（用户批准）后执行。

**根因（源码定位）**：
- 子代理的 `Ask` 审批路径依赖 `TurnFlow.reverse_rpc_callback`（`turn_flow/hooks.rs:810`：有 callback → `approval.ask`；无 callback 且非 headless → 直接 `Rejected`，即 hooks.rs:1067 的该错误串）。
- 子代理的 TurnFlow 由 `TurnFlow::new(self.turn_flow_config.clone())` 创建（`subagent/host.rs:520`），turn 开始时逐项注入服务（`host.rs:805-825`：permission_manager、external_hooks、task_manager、profile、goal、plan、todo、event_bus、wire_turn_id、wire_recorder）——**唯独没有 `set_reverse_rpc_callback` / `set_interaction_kernel`**。
- 全代码库中 `set_reverse_rpc_callback` 只有一个生产调用点：embedded 主代理 dispatch（`embedded_server/message_handlers.rs:136-138`）。子代理路径永远不接线。

**影响**：`PermissionMode::Manual`（默认模式）下，子代理只读（Glob 可用）而完全无法执行任何写文件/命令任务——`Agent` 工具的派遣能力形同虚设。本次用户需求最终靠主代理亲自兜底完成；模型自行"降级为主会话直接干"掩盖了故障，若不读子代理 journal 用户几乎无感知。

**修复方向**：`subagent/host.rs` 的 turn 接线处补 `set_reverse_rpc_callback`（从 `parent_config.reverse_rpc_callback` 继承，host.rs:484 已继承到 config，只是没接到 turn flow）与 `set_interaction_kernel`；审批请求带上 subagent 身份以便 UI 区分来源。

### P0-2 Glob/Grep 无 workspace 越界校验，子代理可枚举引擎自身存储（严重：安全/隔离）

**现象**：Rust 子代理用 `Glob(path="/Users/chengri.a.qi/.mirri-code", pattern="**")` 成功列出了引擎 home 全部内容——包括其他会话的 wire.jsonl、`workspaces.json`、`workspace-trust.json`、`session-index.db-wal`、`credentials/` 目录名等（截断于 100 条）。

**根因**：`tools/path_security.rs` 的 workspace 沙箱只接入了 `Read`/`Edit`/`Write`（三个文件各自注释声明）；`glob.rs`/`grep.rs` 对显式传入的 `path` 参数完全没有 workspace root 校验（grep 无任何 sandbox 相关代码）。写路径被 "path traversal detected" 拒绝、读路径却任意穿越，隔离策略不自洽。

**影响**：任意会话/子代理可枚举磁盘上任意目录的结构（文件名、路径元信息），包括引擎自身的会话库与凭据目录布局；结合无审批的子代理（P0-1 之下它们只剩读能力），泄露面被放大。

**修复方向**：为 Glob/Grep 的 `path` 入参加入与 Read 一致的 `WorkspacePaths::validate`（或按产品决策显式放开，但至少要记录为一致的设计决策，而非代码里"忘了"）。

---

### P1-1 标题 reminder 在标题已设置后仍每步重复注入

**现象**：主代理 step 1 已成功调用 `SetSessionTitle`（结果 "Session title set to: 创建 PHP 和 Rust 的 HelloWorld 示例"，state.json 也有 title），但此后每个 step 前仍注入一次 "The session does not have a title yet…"（wire 第 15/26/36/47/58/66 行，共 6 次，每次 ~120 token）。

**根因**：`turn_flow/injection/title.rs` 的 `TitleInjector` 无状态，门控只在 dispatch 时决定、整 turn 静态保持（模块注释自述 "holds statically for the turn"）。turn 中途 `SetSessionTitle` 成功后注入器不感知。

**影响**：上下文浪费并持续误导模型（提示与事实矛盾）；低遵从模型可能重复调用 `SetSessionTitle` 覆盖用户/自己起过的标题。

**修复方向**：注入时读取 title 状态（`ContextMemory` 或 title handle）做实时判断，或 `SetSessionTitle` 成功后卸载注入器。

### P1-2 空 workdir 时 system prompt 完全不告知工作目录，父代理猜错路径并污染两个子代理任务

**现象**：主代理在两个 Agent 调用的 prompt 里都写了 "the current working directory `/Users/chengri.a.qi/.mirri-code/`"——这既不是会话 workDir（`t1`），也从未被任何工具结果证实，纯属模型臆测。两个子代理因此第一轮 `Write` 全部被 path traversal 拒绝（越界写到 `.mirri-code/`），各浪费 1-2 个 step 才从报错里"考古"出真实工作目录。

**根因**：`scope/session_scope.rs:1279-1287` 的 `dir_tree_suffix` 在目录树为空时返回 `None`，把"## Working Directory"整段（**连同路径本身**）从 system prompt 里省略。会话建立在空目录上时，模型对工作目录零信息。

**影响**：所有"空目录起步"的会话（新项目脚手架场景的高频路径）都会出现；本次直接导致子代理首轮全部失败 + 主代理多轮试错。

**修复方向**：路径恒定注入（`## Working Directory\n\n{workdir}`），仅目录树部分可选。

### P1-3 `<tools_added>` 提示与 select_tools 实际状态自相矛盾

**现象**：turn 开始注入的 `<tools_added>` reminder 明确指示 "Use the select_tools tool with exact names to load full tool definitions"，且列表里包含 `select_tools`；模型照做后返回 `isError: true`："select_tools is not configured for this agent. Progressive tool disclosure is disabled (all tools are already available). There is nothing to load."

**根因**：主代理并未启用 progressive tool disclosure，但注入层仍然发出了"要求先 select_tools 再调用"的 announcement。注入内容与工具注册表状态不同源。

**影响**：每个 turn 的第一次工具交互固定浪费一次调用 + 一段错误上下文；对遵从性强的模型会造成持续困惑（"我到底能不能直接调工具"）。

**修复方向**：未启用 progressive disclosure 时不注入该 reminder（或注入"工具已全量可用，无需 select_tools"的变体）。

---

### P2-1 子代理 wire metadata 与主会话口径不一致

**现象**：子代理 journal 首行 `{"created_at":1789351922,"protocol_version":"2"}` vs 主会话 `{"created_at":1789351914939,"protocol_version":"2.0"}`。两个差异：`created_at` 秒级 vs 毫秒级；协议版本 `"2"` vs `"2.0"`。

**根因**：`scope/session_scope.rs:711-719`（`child_wire_log`）手工构造 envelope 时用 `d.as_secs()` 写秒、写 `"2"` 字符串，与主 journal 的写法（毫秒、"2.0"）不同源。

**影响**：按毫秒解析子代理 `created_at` 会得到 1970-01-21；重放/vis 工具要为子代理 journal 特判两种口径。

### P2-2 agent 任务的 status 只反映"子代理 turn 跑完"，不反映任务成败

**现象**：两个子代理实际都没完成交付（0 文件落盘、明确报告失败），但 `tasks/task-*.json` 均为 `status: completed, exit_code: 0`。任务列表/UI 无法从状态字段区分"子代理成功交付"与"子代理跑完但失败"。

**影响**：依赖任务状态做汇总/通知的逻辑会误报成功。本次父代理读了 result 文本才知道失败，纯靠 LLM 阅读理解。

**修复方向**：agent 任务 status 引入子代理 turn 的最终语义（或至少在 detail 中带上"报告失败"的标记）；短期可在文档中明确该字段语义。

### P2-3 主代理遗留无用目录 `hello_php_from_subagent`

**现象**：主代理的 `mkdir -p hello_rust/src && mkdir -p hello_php_from_subagent`（description 只写了"创建 Rust 项目目录结构"）在工作区留下一个从未使用的空目录，最终汇报的产物树里也没有它，未清理。

**性质**：模型行为质量问题（DeepSeek-V4-Flash），非引擎 bug；记录为观察项。目录描述与实际命令不符同样会误导审批界面的用户。

---

## 三、已排查并排除的项（避免误报）

| 疑点 | 结论 |
|---|---|
| step1 `inputTokens 16590 / cacheReadTokens 0`，step2 起 `cacheReadTokens ≈ 全量` | DeepSeek 隐式缓存语义，上次审计 ADR-012 已显式化，预期行为。 |
| `usage.record` 与 `step.end` 双份 usage | 两层各司其职（流式/终态），与主会话一致的设计。 |
| 子代理 prompt origin 为 `system_trigger/subagent` | 设计如此。 |
| 主代理 Bash/Write 审批耗时 ~2.5-8.7s | 用户手动批准延迟，非引擎问题。 |
| `subagent.registered` 的 model 记 alias（fast）而 usage.record 记实际模型 | ADR-012 既定口径。 |
| 两个子代理并行（同 step 双 tool.call）| Agent 工具语义本身并行，未用 AgentSwarm，符合用户"非 swarm"要求。 |
| 主代理最终放弃子代理改为亲自完成 | 合理的兜底决策（虽然根因是 P0-1 的引擎缺陷，但模型处置得当、汇报诚实）。 |

## 四、优先级汇总与修复建议

| 编号 | 严重度 | 问题 | 一句话修复方向 |
|---|---|---|---|
| P0-1 | **致命** | 子代理 TurnFlow 未接审批 callback，Manual 模式下写/Bash 全灭 | `subagent/host.rs` turn 接线处补 `set_reverse_rpc_callback`/`set_interaction_kernel`（config 已继承，差最后一跳） |
| P0-2 | **严重** | Glob/Grep 可枚举 workspace 外任意目录（含引擎自身存储） | 为两个工具的 `path` 参数接入 `WorkspacePaths` 校验，或显式记录放开决策 |
| P1-1 | 高 | 标题 reminder 已设标题后每步重复注入 | 注入时实时读 title 状态，或成功后卸载注入器 |
| P1-2 | 高 | 空 workdir 时 system prompt 不含工作目录，父代理猜错并污染子代理 | 路径恒定注入，目录树才可选 |
| P1-3 | 高 | `<tools_added>` 指示调用 select_tools 但该工具未配置 | 未启用 progressive disclosure 时改写/不注入该 reminder |
| P2-1 | 中 | 子代理 journal 元数据秒级时间戳 + 协议版本 "2" | envelope 构造与主 journal 统一（毫秒、"2.0"） |
| P2-2 | 中 | agent 任务 status 不区分子代理成败 | status/detail 引入子代理结果语义 |
| P2-3 | 低 | 遗留空目录 `hello_php_from_subagent` | 模型质量观察项，不修 |

**修复顺序建议**：P0-1 先行（一条接线级修复即可恢复 Agent 工具在 Manual 模式的可用性，且是用户可感知度最高的缺陷）；P0-2 需要先做一次产品决策（读目录是否纳入沙箱）再动代码；P1-1/P1-2 同属注入层问题可一并修；P1-3 与 P2-1 是低风险清理项。

## 五、复现与验证方式

- **P0-1**：Manual 模式下任一会话调用 Agent 工具让子代理写文件；若 journal 中 Write/Bash 立即返回 "approval required but no approval mechanism available" 且无 `approval.ask` 到达客户端，即命中。修复后应看到审批请求弹出并路由到会话客户端。
- **P0-2**：让会话执行 `Glob(path="~/.mirri-code", pattern="**")`；当前会返回引擎 home 的完整文件列表。
- **P1-1**：跑一个新会话，模型调用 SetSessionTitle 成功后继续多 step；检查每个 step 前是否仍注入 "The session does not have a title yet"。
- **P1-2**：在空目录下建会话，dump 主代理发给 LLM 的首请求，确认 system prompt 是否包含工作目录路径。

---

## 六、处置结果（2026-09-14 修复批次）

**结论：7 项确认问题全部修复并通过三层验收（单元/全 workspace 测试、真实进程 fire+wait 双模式验收、web UI 实际体验）；验收期另发现并修复一个 serve 既有缺陷（wait 模式审批死锁）。P2-3 维持观察不修。**

### 6.1 修复清单

| 编号 | 处置 | 修复 commit | 设计依据 |
|---|---|---|---|
| P0-1 | 已修复 | `20a15edd`（内核接线）+ `a8264c99`（serve 部署面镜像继承）+ `8506aba4`（复审整改） | ADR-009 |
| P0-2 | 已修复（校验拒绝，决策为纳入沙箱） | `1730df45` | tool-calling ADR-2 |
| P1-1 | 已修复 | `049331ab` + `8506aba4`（复审整改） | ADR-018 修订 |
| P1-2 | 已修复 | `c8dde171` | context-assembly ADR-001 |
| P1-3 | 已修复 | `0fdc9d60` | tool-calling ADR-3 |
| P2-1 | 已修复 | `07aca9d5` | ADR-016 |
| P2-2 | 已修复（引入 stop_reason → 终态映射：Filtered/Paused/BlockedByHook/Unknown → failed） | `eb1a01d8` | ADR-010 |
| P2-3 | 不修（模型行为质量观察项） | — | — |

### 6.2 验收期新发现并修复：serve wait 模式审批死锁（既有缺陷，非本批引入）

- **现象**：`message.send(mode="wait")` 的连接循环内联阻塞整个连接，子代理 `approval.ask` 饿死，turn 被引擎看门狗在 300s 后以 "the LLM did not respond" 误杀。fire 模式不受影响，故长期未暴露。
- **修复**：`bcfb3ed4`——反向 RPC 状态移入共享 `Arc<Mutex>`，回调直接投递连接 outbox（独立 writer 任务刷写），引擎域 dispatch spawn 化（连接循环保持读 socket）。含回归测试 `should_deliver_reverse_rpc_while_a_wait_mode_request_is_in_flight`。
- **并发语义变化**（reviewer N1）：wait 模式下同一连接的并发请求从"串行"变为"并行"——此为解锁审批的必要前提，已记录于 ADR-009 修订记录。

### 6.3 验收证据

1. **单元/集成**：`cargo test --workspace` 全绿（116 个 target）；`cargo clippy` 干净。
2. **真实进程（fire 模式）**：fake provider + 隔离 MIRRICODE_HOME + `mirri serve`，11/11 断言通过（含 P0-1 子代理 Write→approval.ask→批准→落盘）。
3. **真实进程（wait 模式）**：同一脚本 11/11 断言通过（死锁修复后复验）。
4. **web UI 实际体验**：浏览器驱动 `mirri serve --web-dir`，Manual 模式下发消息→Agent 派发子代理→审批卡出现（"APPROVAL REQUIRED — execute Write"）→点击 Approve→文件落盘（web-workspace/web_accept.txt 内容 web-acceptance-ok）→任务卡 status ok、Write 步骤 status ok→回合正常结束。

### 6.4 后续建议（不在本批范围）

- reviewer N2：连接无并发上限，恶意/异常客户端可打满任务数，建议加限。
- reviewer Q1–Q3：outbox 容量策略、反向 RPC 超时统一、断线重连后 pending 审批的恢复语义，建议单独评审。
- ADR-009 c) 项：审批请求携带 agent 身份（当前以会话为粒度渲染）。

