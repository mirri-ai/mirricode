# Rust 会话代理上下文组装（Agent Context Assembly）

> 状态：v1 基线 · 2026-09-14 · 本文件是 Rust 会话代理 system prompt 组装面的设计事实源（内部规格文档，不进文档站构建，中文单语）。
> 现状证据：会话审计 docs/specs/reports/rust-v3-session-30c0d8bb-audit.md；实现锚点 `rust/crates/mirri-core/src/scope/session_scope.rs`。

## 1. 需求

模型必须知道**自己在哪里、受哪些约定约束**才能正确行动：

- 语言跟随（用户语言的指令）——TS 基线默认 profile 的 `# Language` 段（审计 6b03e323 P2-5 修复引入的逐字镜像）。
- 项目约定（AGENTS.md 内容,经非特权指令通道声明）。
- 工作目录（会话 workDir 的绝对路径与目录树）——任何路径决策（写文件、派发子代理、构造命令）的第一事实。

## 2. 设计：组装顺序

会话代理（caller 未自带 system prompt 时）的 system prompt 按固定顺序拼装：

1. **基础段**（`DEFAULT_BASE_SYSTEM_PROMPT`）：语言跟随指令（TS `system.md` 的 `# Language` 段逐字镜像）。
2. **AGENTS.md 后缀**（`agents_md_suffix`）：非特权声明包裹的项目指导；无内容时省略。
3. **工作目录段**（`dir_tree_suffix`）：见 ADR-001。

组装点两处共用同一组 helper 保证字节一致：agent 创建（`create_agent`）与压缩后刷新（`refresh_agent_system_prompt` / `refresh_all_agent_system_prompts`）。caller 自带 prompt 时整体替换基础段（TS custom-profile 语义）。

## 3. 决策记录

### ADR-001 工作目录段恒定披露（含绝对路径）

- **日期**：2026-09-14
- **上下文与动机**：会话审计 session-30c0d8bb（P1-2）：工作目录树为空时（新项目空目录——高频起步路径）整个"## Working Directory"段被省略，**路径本身也不出现**——父代理臆测工作目录为 `~/.mirri-code` 并写进两个子代理任务 prompt，直接导致子代理首轮写入全部被路径守卫拒绝、多轮试错后才"考古"出真实目录。
- **决策**：工作目录段**恒定注入**，段内先给绝对路径再给目录树（树为空时显示 `(empty directory)`、不可读时 `[not readable]`——目录树契约不变）；仅在 caller 自带 system prompt 时整体替换（既有语义）。模型由此在任何空目录会话中都知道自己的工作目录。
- **被否决的备选**：a) 仅在树非空时注入（现状——空目录会话零路径信息）；b) 把路径并入工具描述（Read/Bash 等每个工具重复携带，上下文成本放大且易漂移）；c) 并入 AGENTS.md 段（项目指导与环境事实职责混淆）。
- **影响范围**：本文件 §2；`dir_tree_suffix` helper 及其两个调用点（创建/刷新，字节一致由共享 helper 保证）；TS `${cwd_listing}` 镜像注释的措辞对齐。

## 4. 验收标准

- 空目录会话的 agent system prompt 包含工作目录绝对路径。
- 同一会话内创建与刷新产出的 prompt 字节一致。
- 目录树契约（空目录/不可读标记）不回归。
