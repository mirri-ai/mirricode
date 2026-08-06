# Agent Profile 统一设计

## 背景与痛点

当前 agent profile 的定义、读取、保存逻辑分散在 `agent-core`（v1）和 `agent-core-v2`（v2）两套引擎中，各自为战。以下是通过代码审计确认的具体痛点。

### P1: 两套 parse/serialize 管道，同一个领域两种表述

| 维度 | v1 (`agent-core/src/profile/`) | v2 (`agent-core-v2/src/.../agentFile.ts`) |
|---|---|---|
| 核心类型 | `RawAgentProfile`（zod, `types.ts:12`） | `AgentFileDefinition`（interface, `internal/types.ts:22`） |
| 文件 parse | `scanner.ts:187` — `parseAgentProfileFile()` | `internal/agentFile.ts:60` — `parseAgentFileText()` |
| schema 校验 | `scanner.ts:221` — `RawAgentProfileSchema.safeParse()` | `internal/agentFile.ts:78-130` — 手动字段提取 |
| 序列化 | `profileService.ts:327` — `serializeAgentFile()` | `internal/agentFile.ts:280` — `serializeAgentFile()` |

两条独立的 parse → validate → serialize 管道，字段含义不完全相同，没有共享任何代码。

### P2: `removeStaleFormatFiles` 引发格式冲突

`profileService.ts:411-425`：保存时删除同名的其他扩展名文件。用户手写 `.md` 后用 web UI 编辑 → v1 写 `.yaml`，删除 `.md` → 用户下次看到文件没了。反之 `findOverrideFile` 优先找 `.md` 就锁死在 md 格式，同名的 `.yaml` 被删掉。

### P3: `findOverrideFile` 检测顺序与 `DEFAULT_AGENT_FILE_EXTENSION` 自相矛盾

- 新建：`DEFAULT_AGENT_FILE_EXTENSION = '.yaml'`（`profileService.ts:26`）
- 覆盖注释：`"default to .md for new overrides"`（`profileService.ts:116`），但代码用的仍是 `.yaml`（`:118`）
- 检测：`findOverrideFile`（`:382-395`）顺序是 `['.md', '.yaml', '.yml']`，`.md` 排第一

同一个类里两个函数对默认格式的理解就不一样。

### P4: 扫描器的扩展名优先级两套不一致

v1 `scanner.ts`：`EXTENSION_PRIORITY = { '.md': 0, '.yaml': 1, '.yml': 2 }`，`.md` 优先。

v2 `agentFileDiscovery.ts`：`AGENT_FILE_EXTENSIONS = ['.md', '.yml', '.yaml']`，字母排序 `.md` 在前。但 v2 的 `serializeAgentFile()` 优先写当前路径的扩展名——路径是 v1 创建的 `.yaml`，所以 v2 读进去是 `.md`，写出来却是 `.yaml`，来回变。

### P5: schema 字段不对称——同含义不同写法

| 含义 | v1 字段名 | v2 字段名 |
|---|---|---|
| 系统提示 body | `systemPromptTemplate` | `prompt` |
| 用户级能力标签 | `capabilities` | 不存在 |
| 必需能力标签 | `capabilitiesRequired` | `capabilitiesRequired` |
| 子 Agent 定义 | `Record<string, RawSubagentProfile>` | `string[]` |
| 禁用工具 | 不存在 | `disallowedTools` |
| 覆盖内置 | 不存在（隐式：同名即覆盖） | `override: true`（显式） |
| `systemPromptPath` | 支持（文件引用） | 不存在 |
| `model_preference` | 不存在 | `primary` \| `secondary` |
| `promptVars` | 支持 | 支持 |

### P6: v2 只读不写——读的又和 v1 写的不完全兼容

v2 引擎没有 profile 文件写入路径。写入走 v1 `IProfileService.createProfile/updateProfile`，读取走 v2（当使用 v2 backend 时）。两套 parse 逻辑如果对某条字段组合处理不同，v2 引擎就崩。

### P7: 运行时 prompt 渲染函数签名不兼容

v1 `SystemPromptContext`（`types.ts:39-49`）和 v2 `AgentProfileContext`（`agentProfileCatalog.ts:57-72`）字段不同，但这是运行时适配层的问题，不属于文件格式统一范围。

---

## 设计决策

### 格式决策

- **新建一律 `.md`**（YAML frontmatter + Markdown body）
- **读取兼容 `.md` / `.yaml` / `.yml`**
- 同名冲突时 `.md` 优先，`.yaml` 次之，`.yml` 最低
- 保存成功后自动删除同名的其他扩展名文件

### 字段裁剪

以下字段从 schema 中删除：

| 删除字段 | 原因 |
|---|---|
| `override` | 文件存在即表达覆盖意图，不需要显式标志 |
| `extends` | 创建时的 UI 概念（"基于 X 创建"），不落盘；创建后文件是自包含的完整定义 |
| `model_preference` | 只有 `primary`/`secondary` 两值，语义模糊；`defaultModel` 已够用 |
| `systemPromptPath` | body 直接就是 prompt，不需要外部文件引用 |
| `promptVars` | 实际零使用（唯一用户 `roleAdditional` 已砍掉）；用户要自定义 prompt 直接写 body |
| `roleAdditional` | 是内置模板的内部实现细节（子 agent 角色前缀），不是用户该关心的概念；直接写 body |
| `capabilities`（v1 独有） | 与 `capabilitiesRequired` 语义重叠 |
| `disallowedTools` | 白名单本身就是排除机制——不列出就是排除 |

### 统一 schema — `AgentFileDef`

```typescript
export interface AgentFileDef {
  /** 必填，kebab-case 标识符，如 "code-reviewer" */
  readonly name: string;
  /** 必填，一句话简介，用于 UI 展示和 LLM 选型 */
  readonly description: string;
  /** 可选，LLM 选型提示，告诉 main agent 什么时候用这个 profile */
  readonly whenToUse?: string;
  /** 可选，默认模型别名，如 "claude-sonnet" */
  readonly defaultModel?: string;
  /** 可选，工具白名单（省略 = 全部工具可用） */
  readonly tools?: readonly string[];
  /** 可选，可派发的子 agent profile 名白名单（省略 = 全部） */
  readonly subagents?: readonly string[];
  /** 可选，能力标签，如 ["code.explore", "code.read"] */
  readonly capabilitiesRequired?: readonly string[];
  /** system prompt 模板正文，.md body 或 .yaml 的 prompt 字段 */
  readonly prompt: string;
  /** 文件来源路径（仅从文件解析时填充） */
  readonly filePath?: string;
  /** 文件来源类型 */
  readonly source?: AgentFileSource;
}

export type AgentFileSource = 'plugin' | 'project' | 'user' | 'extra' | 'explicit';
```

注：`builtin` 不在 `AgentFileSource` 中——内置 profile 不经过文件发现，不通过 `agent-profile` 包解析。内置 profile 由各 core 自己的 embedded sources 机制加载（v1: `DEFAULT_PROFILE_SOURCES` + zod；v2: `registerAgentProfile()` 代码贡献）。

### 字段位置：frontmatter vs body

**原则：frontmatter 放结构化数据，body 放自由文本。**

| 字段 | 位置 | 理由 |
|---|---|---|
| `name` | frontmatter | 机器标识，kebab-case，必须可解析 |
| `description` | frontmatter | 短文本，UI 展示/LLM 选型用 |
| `whenToUse` | frontmatter | 短文本，LLM 选型元数据 |
| `defaultModel` | frontmatter | 枚举值 |
| `tools` | frontmatter | 列表，机器解析 |
| `subagents` | frontmatter | 列表，机器解析 |
| `capabilitiesRequired` | frontmatter | 列表，机器解析 |
| `prompt` | body | 自由文本模板，人可读 |

示例文件：

```markdown
---
name: code-reviewer
description: Code review specialist
whenToUse: Use when you need code review
defaultModel: claude-sonnet
tools:
  - Read
  - Grep
  - Glob
subagents:
  - coder
capabilitiesRequired:
  - code.explore
---

You are a code reviewer. Focus on security, performance, and style.
```

### 字段对 LLM 的作用

| 字段 | 进 system prompt? | 影响 LLM 选型? | 说明 |
|---|---|---|---|
| `name` | 否 | 是（工具名称） | 索引标识 |
| `description` | 否 | 是（工具描述） | UI/LLM 选型元数据 |
| `whenToUse` | 否 | 是（工具描述） | LLM 选型提示 |
| `prompt` | 是 | 否 | 完整 system prompt 模板 |
| `tools` | 否 | 否 | 决定 agent 可见的工具集 |
| `defaultModel` | 否 | 否 | 决定绑定的模型 |
| `subagents` | 否 | 否 | 决定可派发的子 agent |
| `capabilitiesRequired` | 否 | 否 | 能力标签，影响工具注入 |

---

## 架构：独立包 `packages/agent-profile`

### 包结构

```
packages/agent-profile/
  src/
    schema.ts          — AgentFileDef 类型 + 字段校验常量
    parse.ts           — parseAgentFileText() 支持 .md/.yaml/.yml
    serialize.ts       — serializeAgentFile() 新建一律 .md
    discovery.ts       — discoverAgentFiles() 递归扫描 + 同名优先级
    roots.ts           — userAgentRoots / projectAgentRoots / configuredAgentRoots
    frontmatter.ts     — parseFrontmatter / serializeFrontmatter
    errors.ts          — AgentFileParseError
    index.ts           — 导出以上
  test/
    parse.test.ts
    serialize.test.ts
    discovery.test.ts
    roots.test.ts
  package.json
  tsconfig.json
```

### 外部依赖

- `js-yaml` — YAML parse/serialize
- `pathe` — 路径操作
- `IHostFileSystem` — 文件系统抽象接口（v1/v2 都有定义），作为 `discovery.ts` 和 `roots.ts` 的参数类型。包内只引用接口类型，不引用任何具体实现。如果 v1/v2 的 `IHostFileSystem` 接口定义不同，包内定义一个最小子集接口（`readText`/`readdir`/`realpath`/`stat`），由调用方适配。

零 DI，零 agent-core 依赖。

### 导出边界

只导出纯数据类型和纯函数。不导出 DI service，不导出 `systemPrompt()` 渲染逻辑，不导出任何需要运行时上下文的东西。v1 和 v2 各自在 core 里定义适配层，把 `AgentFileDef` 映射到自己的运行时 `AgentProfile`（带上 `systemPrompt()` 函数）。

### 配置目录契约

与项目其他配置领域（skills、themes、mcp、config.toml）一致——bootstrap 解析 `homeDir`，`agent-profile` 包只管自己的 `agents/` 子目录：

| 来源 | 路径 |
|---|---|
| 用户 brand | `<homeDir>/agents` |
| 用户 generic | `<osHomeDir>/.agents/agents` |
| 项目 brand | `<projectRoot>/.mirri-code/agents` |
| 项目 generic | `<projectRoot>/.agents/agents` |
| 配置 extra | `extra_agent_dirs` 中的路径 |

`homeDir` 的解析（`MIRRICODE_HOME` → `~/.mirri-code`）统一在 bootstrap 层，`agent-profile` 包只接收传入的值。

---

## 模块详解

### parse — `parseAgentFileText()`

```typescript
function parseAgentFileText(options: {
  path: string;
  text: string;
  source?: AgentFileSource;
}): AgentFileDef
```

行为：
1. 根据扩展名分支：
   - `.md`：`parseFrontmatter()` 分离 YAML header 和 body，body 为 `prompt`
   - `.yaml`/`.yml`：整个内容作为 YAML 解析，取 `prompt` 或旧名 `systemPromptTemplate` 字段为 prompt，若无则默认 `"You are a helpful assistant."`
2. 提取 `name`，若缺失则从文件名推导（去掉扩展名）
3. 校验 `name` 符合 kebab-case：`/^[a-z0-9]+(?:-[a-z0-9]+)*$/`
4. 校验 `description` 非空
5. 应用旧字段兼容映射
6. 返回 `AgentFileDef`

校验失败抛 `AgentFileParseError`。

### serialize — `serializeAgentFile()`

```typescript
function serializeAgentFile(def: AgentFileDef): string
```

行为：
1. 构建 frontmatter data：`name` + 所有非空的可选字段
2. 用 `serializeFrontmatter(data, def.prompt)` 输出 `.md` 格式
3. 不再支持输出 `.yaml` 格式

### discovery — `discoverAgentFiles()`

```typescript
function discoverAgentFiles(options: {
  fs: IHostFileSystem;
  roots: readonly AgentFileRoot[];
  warn?: (message: string) => void;
}): Promise<AgentFileDiscoveryResult>
```

行为：
1. 对每个 root 递归 walk，最大深度 8
2. 跳过 `.` 开头和 `node_modules` 目录
3. 只处理 `.md`/`.yaml`/`.yml` 结尾的文件
4. 逐个 `parseAgentFileText()`，失败则加入 `skipped` 列表
5. 同名冲突：同目录下 `.md` 优先，发出警告，忽略 `.yaml`/`.yml`
6. 不同目录同名：先注册的赢（按 root 顺序，root 内按字母排序，`.md` 排在 `.yaml` 前面）
7. skip 警告上限 5 条，超出后汇总提示

返回 `AgentFileDiscoveryResult`（agents + skipped + scannedRoots）。

### roots — 目录解析

```typescript
function userAgentRoots(fs, homeDir, osHomeDir, warn?): Promise<AgentFileRoot[]>
function projectAgentRoots(fs, workDir, warn?): Promise<AgentFileRoot[]>
function configuredAgentRoots(fs, dirs, workDir, osHomeDir, source, warn?): Promise<AgentFileRoot[]>
```

项目根通过向上查找 `.git` 目录确定。

---

## 旧字段兼容映射

读取旧 `.yaml` 文件时，自动映射已删除的字段：

| 旧字段 | 映射到 | 处理 |
|---|---|---|
| `systemPromptTemplate` | `prompt` | 直接赋值 |
| `systemPromptPath` | `prompt` | 由 `discovery.ts` 在调用 `parseAgentFileText` 之前用 `fs` 读取外部文件内容并替换为 `prompt` 字段；`parseAgentFileText` 本身是纯函数，无文件系统访问 |
| `extends` | 忽略 | 静默跳过 |
| `override` | 忽略 | 静默跳过 |
| `model_preference` | 忽略 | 静默跳过 |
| `promptVars` | 忽略 | 静默跳过 |
| `capabilities` | 忽略 | 静默跳过（用 `capabilitiesRequired` 代替） |
| `subagents: Record<string, {description}>` | `subagents: string[]` | 取 key 列表，丢弃 description |
| `disallowedTools` | 忽略 | 静默跳过 |

保存时一律输出 `.md` 格式，旧字段不再出现。

---

## 迁移路径

### 迁移顺序

```
1. 创建 packages/agent-profile 包（含完整测试）
2. v2 迁移（改动小，验证包的正确性）
3. v1 迁移（改动大，用 v2 已验证的包）
4. server + web 迁移
5. 删除 v1/v2 中已废弃的旧代码
6. 更新 pnpm-workspace.yaml + flake.nix
```

### v2 (`agent-core-v2`)

1. `workspace/workspaceAgentProfileLoader/internal/agentFile.ts` — 删除 `parseAgentFileText`/`serializeAgentFile`/`parsePromptVars`，改为从包导入
2. `workspace/workspaceAgentProfileLoader/internal/agentFileDiscovery.ts` — 删除 `discoverAgentFiles`，改为从包导入
3. `workspace/workspaceAgentProfileLoader/internal/agentRoots.ts` — 删除 `userAgentRoots`/`projectAgentRoots`/`configuredAgentRoots`，改为从包导入
4. `workspace/workspaceAgentProfileLoader/internal/agentProfileFromFile.ts` — `AgentFileDefinition` → `AgentFileDef`，去掉已删除字段的透传
5. `workspace/workspaceAgentProfileLoader/internal/types.ts` — 删除 `AgentFileDefinition`，改为 re-export
6. `_base/text/frontmatter.ts` — 如果 frontmatter 逻辑只被 agent-file 用，搬到包里；否则保留原位

### v1 (`agent-core`)

1. `profile/scanner.ts` — 删除 `discoverAgentProfiles`/`parseAgentProfileFile`/`readAgentFiles`，改为从包导入 `discoverAgentFiles`
2. `profile/load.ts` — 删除 `finalizeRawAgentProfile`/`parseAgentProfileYaml`，内置 profile 仍用 embedded sources（`DEFAULT_PROFILE_SOURCES` + `finalizeRawAgentProfileSource`），不经过 `agent-profile` 包；文件发现走包
3. `profile/types.ts` — `RawAgentProfileSchema` 保留（内置 profile 仍用），文件发现的类型改为 `AgentFileDef`
4. `profile/registry.ts` — `reload()` 中文件发现改用包，然后把 `AgentFileDef` 映射为 `RawAgentProfile`（适配层：`systemPromptTemplate: def.prompt`，丢弃不存在的字段）。内置 profile 的 `systemPromptPath` 解析仍在 v1 自己的 `finalizeRawAgentProfileSource` 中处理，不经过包
5. `services/profile/profileService.ts` — `createProfile`/`updateProfile` 改用包的 `serializeAgentFile` + `parseAgentFileText`，删除内联的旧函数

### server (`packages/server`)

1. `routes/agents.ts` — wire 类型跟随调整
2. `package.json` — 新增 `@mirri-ai/agent-profile` 依赖

### web (`apps/mirri-web`)

1. `AgentProfilesPanel.vue` — 删除 `roleAdditional` 表单字段，body 直接编辑
2. `api/daemon/client.ts` / `api/types.ts` — wire 类型跟随 server 调整

---

## 测试策略

- `parse.test.ts` — 覆盖 `.md`/`.yaml`/`.yml` 三种格式、旧字段映射、name 推导、校验失败
- `serialize.test.ts` — 覆盖字段序列化、空字段省略、round-trip（parse → serialize → parse）
- `discovery.test.ts` — 覆盖递归扫描、同名优先级、skip 逻辑、深度限制
- `roots.test.ts` — 覆盖 user/project/configured 路径解析、`.git` 向上查找、缺失目录

测试命名遵循 Given-When-Then 风格。

---

## 不在本次范围内

- 运行时 `systemPrompt(context)` 渲染函数的统一（v1/v2 各自保留自己的适配层）
- 内置 profile 的 embedded sources 改造（仍用各 core 自己的机制）
- `IAgentProfileRegistry` / `ISessionAgentProfileCatalog` 等 DI 层（v2 保留自己的）
- `IProfileService` 的 CRUD 接口签名（v1 保留，内部实现改用包）
