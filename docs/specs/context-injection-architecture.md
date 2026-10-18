# 上下文注入域 · 架构文档（context-injection-architecture）

> 本文记录 TS 引擎（v2 为主，v1 以 `[v1]` 标注）实现上下文注入的机制、接口形状与数据流。代码事实（file:line）见 [context-injection-context.md](context-injection-context.md)。Rust 侧对应机制与差异以该文档的对比矩阵为准，本文不重复。

## 1. 分层结构

```
┌─ 事件驱动消息层（promptService.inject / MessageStepRequest）────────┐
│  cron / 后台任务通知 / stop hook / 钩子附加上下文 / shell 直跑 /       │
│  技能激活 / 插件命令 / 子代理启动 / 目标续航 —— 走回合调度域的队列       │
├─ 事件型系统提醒层（直接调用系统提醒服务）──────────────────────────┤
│  打断提醒 / btw / 项目说明生成完成 / 目标事件×3 / 工具增删公告 /        │
│  插件会话启动 / 蜂群进出 / 图片压缩说明                                │
├─ 条件注入框架层（contextInjector，注册式）─────────────────────────┤
│  plugin_session_start / goal / plan_mode / permission_mode /         │
│  todo_list_reminder / background_task_status / capability_gap        │
├─ 核心通道层（AgentSystemReminderService）──────────────────────────┤
│  appendSystemReminder(content, origin)                              │
├─ 上下文记忆层（contextMemory）──────────────────────────────────────┤
│  追加/暂存缓冲（deferred）/ wire 持久化 / splice 事件                  │
└─ 读侧投影层（contextProjector）──────────────────────────────────────┘
   合并连续同角色消息 / 剥离失效动态工具上下文 / 413 降级替换
```

## 2. 核心通道：系统提醒服务

**接口形状**（v2）：

```ts
interface IAgentSystemReminderService {
  appendSystemReminder(content: string, origin: PromptOrigin): ContextMessage;
}
```

- 构造 `role: 'user'`、单一 text part 的消息，文本为 `<system-reminder>\n{content.trim()}\n</system-reminder>`，直接追加到上下文记忆尾部。
- 去重/频率完全由调用方负责；服务本身无状态。
- `[v1]` 对应物是 `ContextMemory.appendSystemReminder`，行为一致。
- **投递信封教学**：系统提示模板中声明 `<system-reminder>` 为"必须遵守的权威系统指令"（与工具侧 `<system>` 补充标记形成双信封体系）。

**暂存缓冲（deferred）**：当对话尾部有未闭合的工具调用/结果配对时，追加的消息进入暂存缓冲，配对闭合后按序落地（v2 在事件折叠层实现，无上限；`[v1]` 与 Rust 均有对应机制，Rust 带 `max_deferred_messages` 上限）。

## 3. 条件注入框架（contextInjector）

**接口形状**：

```ts
interface ContextInjectionProvider {
  (ctx: {
    injectedPositions: readonly number[];
    lastInjectedAt: number | null;
    isNewTurn: boolean;
  }): Promise<string | ContentPart[] | undefined>;
}
```

- **注册**：按变体名 `register(name, provider)`；注册时扫描历史收集该变体已有注入位置作为初始记账。
- **触发**：挂在循环服务的 `onWillBeginStep` 钩子**末段**（钩子链完成后 LLM 请求才发出，保证注入进入当步请求）；压缩完成后由压缩服务显式调用 `injectAfterCompaction()`（重置 `isNewTurn`）。
- **回合边界**：`isNewTurn` 标志在 `turn.started` 事件置真、每次注入消费一次；注册进 agentState（随会话持久化，恢复后不重复消费）。
- **注入形态**：提供者返回字符串 → 走系统提醒信封；返回 ContentPart 数组 → 以裸 user 消息追加（无信封）；返回 `undefined` → 本步不注入。
- **位置记账与对账**：监听 `context.spliced`（撤销/压缩/裁剪）增量修正落点（区间删除 + 偏移平移 + 收养插入段内同变体注入）；wire 恢复（`onDidRestore`）后全量重扫对账。提供者凭 `injectedPositions`/`lastInjectedAt` 自行决定去重。
- `[v1]` 对应物是 `DynamicInjector` 基类 + `InjectionManager`（注入器在管理器内硬编码注册；钩子 `onContextClear/onContextCompacted/onContextMessageRemoved` 做索引对账）。Rust 侧 `InjectionManager` + `Injector` trait 为骨架等价物，但无位置记账/对账状态——以"每步重扫历史"替代。

## 4. 注入器与注入物清单（通道视角）

| 层 | 变体/机制 | 触发 | 形态 | 去重/节流策略 |
|---|---|---|---|---|
| 框架层 | `plugin_session_start` | 每步 + 技能目录变更 | 信封（`<plugin_session_start>` 内嵌块） | 历史无同变体注入才注；目录变更强制刷新；无活跃插件时注中和声明 |
| 框架层 | `goal` | 新回合首步 | 信封 | goal 活动/blocked/paused 三档；预算 ≥75% 切换收敛措辞 |
| 框架层 | `plan_mode` | 每步 | 信封 | 进入 full / ≥2 助手轮 sparse / ≥5 轮 full 重申；转换沿必注；退出注解除提醒 |
| 框架层 | `permission_mode` | 每步 | 信封 | auto 模式进入/退出转换沿；持续中历史缺失则补注一次（压缩折叠自愈） |
| 框架层 | `todo_list_reminder` | 每步 | 信封 + 当前列表 | 距上次写 TodoList ≥10 助手轮 且 距上次提醒 ≥10 轮；TodoList 工具需 active |
| 框架层 | `background_task_status` | 每步（压缩后置 pending） | 信封 + 任务清单 | 仅压缩后有活跃任务时注入一次 |
| 框架层 | `capability_gap` | 新回合首步 | 信封 | 最后用户消息含不支持模态且目录有替代模型；**无历史查重**——缺口持续存在则每个新回合首步重新注入 |
| 事件层 | `interruption` | 回合以用户取消结束后 | 信封 | 末条持久消息已是本变体则跳过；待发集合持久化于 wire 专属模型，恢复补发 |
| 事件层 | `btw` | 侧问子代理 fork 时 | 信封 | 一次性 |
| 事件层 | `init` | `/init` 子代理完成后 | 信封（含最新项目说明全文） | 每次显式调用 |
| 事件层 | `goal_cancelled` / `goal_budget_stop` / `goal_fork_cleared` | 目标取消 / 预算触顶宽限步 / 分叉恢复 | 信封 | 一次性；fork 清除由 wire 待办标志保证恢复补发 |
| 事件层 | `loadable-tools`（tools diff） | 步开始（首步 + 压缩后边界） | 信封（`<tools_added>/<tools_removed>`） | 差集为空不注；历史已公告集合折叠自愈 |
| 事件层 | `swarm_mode` / `swarm_mode_exit` | 用户开关蜂群模式 | 信封 | tool 触发不注；退出时进入说明恰为末条消息则**弹出**（wire reducer 保证重放一致） |
| 事件层 | `image_compression` | 用户消息物化 / 被 hook 阻断时 | 信封（`ownerPromptId` 关联归属消息） | 随所属消息撤销连带删除 |
| 事件层 | `compaction_elision` | 压缩划定保留窗口、中间段被省略时 | 信封（插在保留头与摘要之间） | 每次压缩按需生成；内容语义归压缩域 |
| 事件层 | `dynamic_tool_schema` | 模型调用按需加载工具时 | **role=system + tools 字段**（协议专用通道，非对话正文） | 发送时若渐进披露关闭则整体剥离 |
| 嵌入层 | 工具去重提醒 | 工具结果返回 | **拼在结果 output 尾部**的信封 | streak ≥3/5/8 递进、≥12 强制停回合 |
| 嵌入层 | `<system>` 侧信道 | 工具结果渲染 | 结果内标记（状态前缀包信封；note 纯文本尾附） | 无 |
| 消息层 | cron / 后台任务通知 / stop hook / 钩子附加 / 钩子阻断结论（assistant 角色例外） / shell 直跑（`<bash-input>` 等信封） / shell 后台化（`shell_command_backgrounded`） / 技能激活 / 插件命令 / 子代理启动 / 目标续航 | 各事件 | 裸 user 消息（部分 XML 信封如 `<cron-fire>`、`<notification>`） | 通知去重键集合；恢复改补录不重放 |

## 5. 来源标识体系（PromptOrigin）

消息级封闭联合（`user | skill_activation | plugin_command | injection{variant, ownerPromptId?} | shell_command | compaction_summary | system_trigger | background_task | cron_job | cron_missed | hook_result | hook_additional_context | retry`，`[v1]` 另有 `model_switched`）。

- **注入物判定**：压缩保留策略以 origin 判定"真实用户输入"（user、user-slash 触发的技能激活/插件命令保留；injection/system_trigger/cron/hook 等全部退场，由框架重注与各领域自愈恢复）。
- **撤销锚点**：撤销遍历跳过 `injection` 类 origin；`ownerPromptId` 使 prompt 拥有的注入随 prompt 连带删除。
- **协议暴露**：消息投影把 origin 塞进 `metadata.origin` 给 REST/WS 客户端。

## 6. 持久化与重放数据流

```
注入发生 → contextMemory.append → wire op context.append_message 落盘
                                          │
        会话恢复 ← wire journal 重放 ←────┘
```

- 所有进入上下文的注入物都持久化，**不存在**"只在请求里临时存在"的注入；临时的只有读侧变换（投影层合并、动态工具上下文剥离、413 降级）。
- 两个**待发模型**（wire 专属状态 + reducer）：打断提醒待发集合（`interruptionReminder.recorded` + 送达清空）、分叉目标清除待办（`goal.forked` 置位 + 落地清空）——崩溃/恢复后补发。
- 蜂群退出"弹出"同样有 wire reducer，保证重放一致。

## 7. ADR 记录

### ADR-001 系统提醒以用户侧消息角色投递，而非 system 角色

- **背景与动机**：中途插入 system 角色消息会破坏部分供应商的交替对话约束与提示缓存；用户侧消息 + 文本内权威信封能在任何供应商上一致工作，且与相邻用户消息合并（Anthropic 合并器）不损语义。
- **决策**：信封靠文本标记表达权威性，消息角色固定 user；`<system-reminder>` 的教学写在系统提示里。唯一例外：动态工具 schema 注入用 `role: 'system'` + `tools` 字段（借协议原生通道）。
- **被否决的替代**：直接用 system 角色（供应商兼容性差、可能打断缓存）；只靠 system prompt 静态声明（无法中途动态注入）。

### ADR-002 注入物压缩退场 + 提供者自愈，而非随保留尾保留

- **背景与动机**：注入物本质是"某时刻的引擎状态快照"，过时快照堆积会稀释上下文且误导模型；状态是可重生的。
- **决策**：压缩保留窗口只保真实用户输入；框架在压缩后以 `isNewTurn=true` 重跑提供者，按当前状态重新生成而非搬运旧文。
- **被否决的替代**：把常驻提醒标记为压缩免疫（上下文膨胀、状态漂移风险）；压缩时把注入物并入摘要（摘要无法保证各变体时序与措辞契约）。

### ADR-003 注入位置记账 + splice 对账，而非每次全量重扫

- **背景与动机**：提供者的去重/节流决策依赖"历史上我注在哪"；撤销与压缩会移动历史。
- **决策**：框架维护各变体落点索引，监听 `context.spliced` 增量修正（删除段移除、后续偏移平移、插入段收养），恢复时全量重扫兜底。
- **被否决的替代**：提供者各自全量扫历史（重复扫描成本、逻辑漂移不一）；不记账（节流判断失效）。Rust 实现选择了"每步重扫历史"的替代路线——语义等价但把成本摊到每步，见 context 对比矩阵。

### ADR-004 双通道职责分离：用户侧消息 vs 系统提醒旁注

- **背景与动机**：引擎内容进入对话有两条路——需要**驱动回合**的内容（定时触发、任务通知、停止钩子）必须走回合队列参与准入/合并；纯**旁注**内容（状态提醒）只需要在场，不应打断调度语义。
- **决策**：前者经消息注入通道（promptService.inject / MessageStepRequest，origin 各异、不包信封），工具也可以经结果投递通道（`delivery: { kind: 'steer', message }`）发出用户侧消息（技能加载块先例）；后者经系统提醒通道（信封、injection origin、不驱动回合），工具结果文本尾部亦允许直接拼接信封提醒（工具去重递进提醒先例）。动态工具 schema 是消息角色的唯一例外（system 角色 + tools 字段，借协议原生通道）。
- **被否决的替代**：全部走回合队列（旁注被准入语义约束，丢失"在场即可"的轻量性）；全部走旁注（驱动类内容错过合并/挂起语义）。Rust 侧曾将技能激活说明走旁注，规格裁决为走用户侧消息——分歧登记见工具调用迁移计划（P1）；Rust 另设工具结果注入字段承载同类语义，字段形状差异见 context 对比矩阵。

## 8. 与相邻域的接缝

- **回合调度域**：消息层的准入类别、合并规则、挂起联动、撤销对账五要素由其统一定义（唤醒源契约）；本域只约束消息形态。
- **压缩域**：本域定义注入物退场/重注规则，压缩流程本体归其所有。
- **画像域**：静态系统提示组装（基础提示、项目说明快照、目录树）归其所有；动态注入全部归本域。
- **工具调用域**：工具去重提醒的阈值语义、动态工具加载协议归其所有；本域只定"嵌入结果尾部"的通道形态。
