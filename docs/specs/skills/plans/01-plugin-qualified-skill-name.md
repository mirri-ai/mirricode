# Skills 域计划 — 01：插件限定名解析实施（plugin-qualified skill name）

> **文档类型：计划类**（按 `docs/specs/AGENTS.md` 治理约定：只做任务拆分与执行状态，需求与设计见 [../skills-requirements.md](../skills-requirements.md) SDR-002 与 [../skills-architecture.md](../skills-architecture.md) ADR-001）。
> 分支：`fix/skill-qualified-lookup`（base `feat/rust-t8`）。

## 目标

使 `插件id:技能名` 形态的技能引用（Skill 工具参数、程序接口激活技能名）在两代引擎（TS v2 `agent-core-v2`、Rust `mirri-core`/`mirri-embedded`）中可解析，闭环「插件文档交叉引用 → 调用 → 激活」链路。

## 全局约束（评审用）

- 解析语义（次序、切分、id/技能名段大小写、遮蔽、下游命名）一律以 [ADR-001](../skills-architecture.md#adr-001) 为准，此处不复述。
- 既有错误文案逐字不变（`Skill "<原始输入名>" not found in the current skill listing.`）。
- 不改模型面工具描述文案（裸名导向不变，限定名为容错形态——ADR-001 裁决）。
- 测试：Given-When-Then 命名；优先加进对应模块既有测试文件，不新建多余测试文件；TDD（先红后绿）。

## 任务

### Task 1 — TS v2 引擎：目录解析器 + 两个调用点接线

范围：`packages/agent-core-v2`。

1. `InMemorySkillCatalog`（`src/app/skillCatalog/registry.ts`）与 `SkillCatalog` 接口（`types.ts`）：新增引用解析方法（裸名 `getSkill` 优先；未命中且含 `:` 时按首个 `:` 切分，插件 id 段小写归一后查 `byPluginAndName` 索引；空段不命中）。
2. `executeModelSkill`（`src/agent/tools/skill/skillTool.ts`）与用户激活校验 `assertUserActivatable`（`src/agent/skill/skillService.ts`）改用解析方法；下游全部用解析结果的正式裸名。
3. 测试（先红）：`test/app/skillCatalog/registry.test.ts`（解析器单测：命中/未命中/前缀非插件/大小写/空段）、`test/agent/skill/skill.test.ts`（限定名经 Skill 工具加载成功且回显正式名；门控组合：禁模型调用×限定名 → 门控错误；用户激活路径限定名）。如用户激活校验另有测试文件则加进对应文件。
4. 验证：`pnpm --filter @mirri-ai/agent-core-v2 run typecheck` + 相关 vitest 文件通过。

### Task 2 — Rust 引擎：SkillCatalog 解析 + SkillTool 与会话激活接线

范围：`rust/crates/mirri-core`、`rust/crates/mirri-embedded`。

1. `SkillCatalog`（`mirri-core/src/skill.rs`）：新增 `resolve(&self, reference: &str) -> Option<SkillDef>`（`get` 优先；未命中且含 `:` 时 `split_once(':')`，插件 id 段小写归一后走 `get_plugin_skill`；空段不命中）。
2. `SkillTool`（`mirri-core/src/tools/skill.rs`）：解析先于门控；禁用检查、渲染、事件、输出用解析后的正式名（`skill_def.name`）；错误文案保持原始输入名。
3. 会话激活处理器（`mirri-embedded/src/embedded_server/session_handlers.rs` 的 activate-skill 处理）：`resolve` 替代 `get`；停用检查与 `render_user_slash_skill_prompt` 用解析后的正式名。
4. 测试（先红）：`mirri-core/src/skill.rs` 内嵌 tests（resolve 单测）与 `mirri-core/src/tools/skill.rs` tests（限定名加载、门控组合、原始名错误回显）；embedded 激活处理器如有既有测试文件则补一条限定名用例。
5. 验证：`cargo test -p mirri-core skill`（及受影响目标）通过；embedded 如有测试则跑对应模块。

### Task 3 — 变更集

按 `.agents/skills/gen-changesets/SKILL.md` 为受影响发布包生成 changeset（`@mirri-ai/agent-core-v2`；Rust 侧按仓库惯例确认是否需要——引擎未发布 npm 包则不生成）。

## 执行状态

- Task 1: complete（f7db289a..0f125b1b）
- Task 2: complete（32781bb6..3b5244b0）
- Task 3: complete（a5356e79）
