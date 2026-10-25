# Skills 技能系统现状盘点（Context）

> 快照日期：**2026-09-12**（`date` 实测）；**§10 为 2026-10-10 内置技能增量**；**§6.3 输入区信息架构为 2026-10-12 增量**。本文为现状清单：代码事实（file:line）、两侧（TS 文档站 vs 代码 / v1 vs v2）差异矩阵、缺口速览。不含目标与裁决。
> 源码基线：v2 引擎 `packages/agent-core-v2`（主）、v1 引擎 `packages/agent-core`（保留表面）、消费端 `apps/mirri-code`（CLI/TUI）、`apps/mirri-web`、服务桥 `packages/kap-server`。
> 用户文档参照：`docs/en/customization/skills.md`、`docs/en/customization/plugins.md`（文档站口径）。

## 0. 闭环地图（现状实现覆盖确认）

| 闭环环节 | v2 实现位置 | 状态 |
|---|---|---|
| 定义/创建 | 手工放文件；无脚手架命令 | 无创建命令（CLI 无 `skill create`） |
| 发现（多源） | `app/skillCatalog/skillRoots.ts` + `workspace/workspaceSkillCatalog/*` | ✅ |
| 解析与校验 | `app/skillCatalog/parser.ts` + `fileSkillDiscovery.ts` | ✅ |
| 触发（用户） | TUI 斜杠 / web 斜杠菜单 / REST activate | ✅ |
| 触发（模型） | `agent/tools/skill/skillTool.ts`（Skill 工具） | ✅ |
| 注入（清单） | `agent/profile/profileService.ts` → 模板变量 `skills` | ✅ |
| 注入（全文） | `agent/skill/prompt.ts` 包裹 + steer / 新回合 | ✅ |
| 插件 sessionStart 注入 | `agent/plugin/agentPluginService.ts` | ✅ |
| 管理面 | 插件：TUI `/plugins` 全套；技能：**rust 引擎侧已落地**（B8：`skill.list` enabled 字段 + `skill.getContent`/`skill.setEnabled` RPC + `disabled_skills` config 键 + `extra_skill_dirs` 消费 + 写后 resync，见 §9）；web 管理页由 B8 web 轨随后落地；TS v2 引擎无对应面（RustFirst） | 🟡 引擎侧 ✅ / web 待落地 |
| 错误路径 | 分级跳过 / 错误码族 | ✅ |

## 1. 技能定义与格式（代码事实）

- 类型契约 `SkillMetadata`（开放 record；已知字段 name/description/type/whenToUse/disableModelInvocation/isSubSkill/safe/arguments）— `packages/agent-core-v2/src/app/skillCatalog/types.ts:3-13`；技能定义（含 path/dir/content/source/plugin 上下文/mermaid/d2）— `types.ts:15-26`；来源四值 `project|user|extra|builtin` — `types.ts:1`；类型判定（inline 族 = 无 type/prompt/inline；用户可激活 = inline 族 + flow；受支持 = + reference）— `types.ts:74-84`；名称小写归一 — `types.ts:70-72`。
- 解析器：目录技能缺 fence 抛错 — `parser.ts:57-59`；frontmatter 非 mapping 抛错 — `parser.ts:76-80`；类型不支持抛 `UnsupportedSkillTypeError` — `parser.ts:26-36, 83-85`；目录技能 name/description 必填 — `parser.ts:87-94`；平铺缺省名/描述（首非空行，240 截断）— `parser.ts:99-100, 147-154`；字段别名归一（when-to-use / disable-model-invocation 等）— `parser.ts:49-54, 128-145`；mermaid/d2 提取 — `parser.ts:111-117`。
- 占位符展开（v2）：`$ARGUMENTS` / `$N` / `$ARGUMENTS[N]` / 命名参数 / **`${MIRRI_SKILL_DIR}`** / **`${MIRRI_SESSION_ID}`**，值经 `escapeXmlTags`；无占位符有参数时追加 `ARGUMENTS:` 行 — `packages/agent-core-v2/src/app/skillCatalog/registry.ts:145-183`；参数分词器（引号分组）— `registry.ts:248-283`。
- **[v1] 差异**：v1 展开的是 **`${MIRRICODE_SKILL_DIR}` / `${MIRRICODE_SESSION_ID}`** — `packages/agent-core/src/skill/parser.ts:223-224`；v1 `escapeXmlTags` 同语义 — `parser.ts:192-230`。
- **文档站 vs v2 代码漂移**：`docs/en/customization/skills.md:60` 与 `docs/zh/customization/skills.md:60` 均只写 `${MIRRICODE_SKILL_DIR}`（无会话标识占位符条目）——v1 名称；v2 代码只认 `MIRRI_*` 短名。**用户按文档站写占位符在 v2 引擎下不展开**（见 §7 缺口 G1）。

## 2. 发现与来源层级（代码事实）

- 根目录常量：用户品牌 `skills`（品牌 home 下）/ 用户通用 `~/.agents/skills`；项目 `.mirri-code/skills` / `.agents/skills` — `packages/agent-core-v2/src/app/skillCatalog/skillRoots.ts:16-19`；项目根 = 向上找 `.git`（到顶回退起点）— `skillRoots.ts:80-89`；配置根路径解析（`~` / 绝对 / 项目相对）— `skillRoots.ts:129-134`；realpath 归一 + 去重 — `skillRoots.ts:118-127, 144-146`。
- 合并开关 `mergeAllAvailableSkills`（缺省 true）：品牌组全收 vs 每组取第一个存在 — `skillRoots.ts:25-35, 102-116`；配置段注册（TOML 键 `merge_all_available_skills`）— `packages/agent-core-v2/src/app/skillCatalog/configSection.ts:21-27`。
- 额外目录配置段（TOML `extra_skill_dirs`，内存 `extraSkillDirs`）— `configSection.ts:13-19`。
- 发现后端：遍历排序、深度上限 8、跳过 `node_modules`/点开头 — `packages/agent-core-v2/src/app/skillCatalog/fileSkillDiscovery.ts:19, 47-65`；目录技能包判定 — `fileSkillDiscovery.ts:58-65`；插件根顶层 `SKILL.md` 特例 — `fileSkillDiscovery.ts:83-96`；平铺 `.md` 仅顶层且同名目录优先 — `fileSkillDiscovery.ts:98-113`；子技能开关三写法 + `父名.子名` 限定 — `fileSkillDiscovery.ts:67-81, 116-127, 201-218`；错误分诊（UnsupportedSkillTypeError → skipped 清单；SkillParseError → warn；其余 → warn）— `fileSkillDiscovery.ts:176-189`。
- 源优先级常量（builtin 0 / plugin 5 / extra 10 / user 20 / workspace 30）与 `SkillContribution` 契约 — `packages/agent-core-v2/src/app/skillCatalog/skillSource.ts:19-41`。
- 六个具体源：内置（App，常量数组）— `app/skillCatalog/builtinSkillSource.ts:22-31`；用户（App，bootstrap home）— `app/skillCatalog/userFileSkillSource.ts:31-63`；显式（Workspace，`bootstrap.args.skillDirs` 非空时取代默认 user+project 发现，标 `user` 源）— `workspace/workspaceSkillCatalog/explicitFileSkillSource.ts:31-52`（默认源在 skillDirs 非空时返回空贡献 — `userFileSkillSource.ts:53-55`、`rootFileSkillSource.ts:76-78`）；额外（Workspace）— `extraFileSkillSource.ts:39-67`；工作区根（Workspace，项目技能）— `rootFileSkillSource.ts:48-99`；插件（Workspace）— `pluginSkillSource.ts:33-53`。
- 工作区合并（每 handler 一次；来源变更单源重扫 + 重合并；源加载按源串行化）— `workspace/workspaceSkillCatalog/workspaceSkillCatalogService.ts:43-160`；`sessionData()` 种子 — `workspaceSkillCatalogService.ts:106-116`。
- 会话折叠（种子 + 会话本地贡献 sink）— `packages/agent-core-v2/src/session/sessionSkillCatalog/skillCatalogService.ts:37-119`。
- 技能根目录对文件工具开放：read/write/edit/grep/glob/media 均注入 `catalog.getSkillRoots()` 作为额外可访问根 — `packages/agent-core-v2/src/agent/tools/os/read/readTool.ts:219`、`write/writeTool.ts:63`、`edit/editTool.ts:62`、`grep/grepTool.ts:118`、`glob/globTool.ts:143`、`media/mediaToolsRegistrar.ts:115`。
- **[v1] 对照**：v1 scanner 一次性解析根集（显式目录完全替代默认根、extra/plugin root 挂接），first-wins — `packages/agent-core/src/skill/scanner.ts:61-130, 132-258, 396-399`；v1 把技能根加入 workspace `additionalDirs`（经系统提示词 Additional Directories 段开放）— `scanner.ts:260-272`；v2 改为逐文件工具注入根清单（机制不同、语义等价）。
- 内置技能 8 个（mcp-config / import-from-cc-codex / update-config / custom-theme / write-goal / sub-skill 家族 3 个）— `packages/agent-core-v2/src/app/skillCatalog/builtin/builtin.ts:23-32`；各定义见同目录 `mcp-config.ts` 等。disableModelInvocation 标记：mcp-config/import-from-cc-codex/custom-theme/sub-skill 家族为 true；update-config/write-goal 为 false（模型可调）— v1 对应 `packages/agent-core/src/skill/builtin/index.ts:13-22` 各定义行。

## 3. 系统提示词注入（清单层，代码事实）

- 注入门控 `skillActiveFor(tools) = tools.includes('Skill')` — `packages/agent-core-v2/src/app/agentProfileCatalog/profile-shared.ts:43-45`；门控关闭时 `skills` 变量置空、`skills_section` 空串 — `profile-shared.ts:93-94, 113-115`。
- 清单渲染：首行 `DISREGARD any earlier skill listings. Current available skills:`；按 Project/User/Extra/Built-in 分组；仅 invocable（排除 disableModelInvocation 与非 inline 族）且非子技能；描述按图位素截断 250；附 whenToUse 与 Path — `packages/agent-core-v2/src/app/skillCatalog/registry.ts:21, 115-125, 225-232`；分组循环 — `registry.ts:198-219`；图位素安全截断（Intl.Segmenter）— `registry.ts:234-246`。
- 技能段说明文字（渐进披露语义 + scope 优先级声明 "Project overrides User overrides Extra overrides Built-in"）— `profile-shared.ts:78-82`。
- 上下文装配（`buildSystemPromptContext` 注入 `skills` = `getModelSkillListing()`、`skillActive`）— `packages/agent-core-v2/src/agent/profile/profileService.ts:861-890, 917-924`。
- **[v1] 对照**：v1 经 profile 模板变量 `MIRRICODE_SKILLS` 且仅当工具面含 `Skill` 才填充 — `packages/agent-core/src/profile/resolve.ts:165-168, 182`；说明段落 — `packages/agent-core/src/profile/default/system.md:131-142`（优先级声明 :140）。
- 子代理面：默认 agent 与 coder 子代理工具面含 `Skill`，explore 不含 — `packages/agent-core-v2/src/session/agentLifecycle/profile/profiles.ts:36, 65, 76-84`；各 profile 的 skillActive 判定 — `profiles.ts:109, 121, 134`、plan profile — `agent/plan/profile/plan.ts:49`；用户自定义角色判定 — `app/agentProfileCatalog/agentProfileManagementService.ts:247`。

## 4. 激活管线（代码事实）

### 4.1 用户斜杠（`IAgentSkillService.activate / enqueueDeferred`）

- 校验（`SKILL_NOT_FOUND` "Skill \"x\" was not found" / `SKILL_TYPE_UNSUPPORTED` "cannot be activated by the user"）— `packages/agent-core-v2/src/agent/skill/skillService.ts:112-125`；错误码常量 — `packages/agent-core-v2/src/errors.ts`。
- 入队：用户意图消息正文 `/<名> <参数>`、origin（kind `skill_activation`、activationId UUID、trigger `user-slash`、类型/路径/来源/参数）— `skillService.ts:59-79`；**物化发生在出队时刻**（记录激活 + 渲染）— `skillService.ts:85-105`；回合忙时 `TURN_AGENT_BUSY` "Cannot activate skill while another turn is active" — `skillService.ts:46-53`。
- 激活事实 `skill.activate` wire op + 派生 `skill.activated` 事件 — `agent/skill/skillOps.ts:41-54`、`skillService.ts:127-130`；重放无副作用（replay 为 no-op，事件/遥测不重放）— `skillService.ts:5-11`（头注释）、`skillOps.ts`。
- 遥测：`skill_invoked`（skill_name + trigger 三态）+ flow 型追加 `flow_invoked` — `skillService.ts:138-148`；事件注册 — `packages/agent-core-v2/src/app/telemetry/events.ts:125-132, 526-538`。
- 包裹渲染 `<kimi-skill-loaded name trigger source dir args>` + 引导语 — `packages/agent-core-v2/src/agent/skill/prompt.ts:18-59`。
- 插件技能前置 `<kimi-plugin-instructions plugin=…>` 附加指令块 — `app/skillCatalog/registry.ts:80-89`。

### 4.2 Skill 工具（模型侧）

- 契约与常量：`MAX_SKILL_QUERY_DEPTH = 3`、`NestedSkillTooDeepError` — `packages/agent-core-v2/src/agent/tools/skill/skill.ts:18-33`；参数 schema（`skill` 精确名 + `args` 命令行串，schema 描述含展开语义说明）— `skill.ts:40-52`。
- 工具描述模板（BLOCKING REQUIREMENT + 同 args 去重 + 不同 args 重调）— `agent/tools/skill/skill.md:1`。
- 执行体：深度门 → 未找到 → 禁模型调用 → 非 inline 三道门（错误结果文案）— `agent/tools/skill/skillTool.ts:95-115`；渲染 + origin（trigger 按深度分 model-tool/nested-skill）+ `delivery: { kind: 'steer', message }` + 输出 "loaded inline" — `skillTool.ts:117-152`；steer 由提示服务投递到当前回合 — `packages/agent-core-v2/src/agent/prompt/promptService.ts:263-267`；display `{kind:'skill_call'}` 与审批规则（规则名 = 工具名、按技能名 glob）— `skillTool.ts:60-67`。
- 注册（`registerAgentToolService`，name `Skill`）— `skillTool.ts:87`。
- **遗留文案**：`skillTool.ts:113` 的非 inline 错误文案仍写 "cannot be invoked by the model **in v1**"——v2 代码内的 v1 遗留措辞（缺口 G3）。
- **限定名解析缺口（2026-10-09 快照，SDR-002/ADR-001 修复中）**：上述调用点（v2 `skillTool.ts` 查找、v2 `skillService.ts` 用户激活校验、Rust `mirri-core/src/tools/skill.rs` 与 `mirri-embedded` 会话激活处理器）此前均为裸名精确匹配——`插件id:技能名` 形态引用全部落到「清单中不存在」；插件索引原语（`getPluginSkill` / `get_plugin_skill`）已存在但仅接入会话启动注入。真实会话证据：限定名调用失败后模型直接从磁盘读取 `SKILL.md` 补救（v3 会话 wire 记录）。

### 4.3 插件会话启动注入

- 仅主 agent（非 main 自钝化）；一次性变体注入（`injectedPositions` 去重）— `packages/agent-core-v2/src/agent/plugin/agentPluginService.ts:30-65`；插件源变更 → 追加刷新 + 中性化（无活动 session start 时声明取代）— `agentPluginService.ts:59-93, 126-136`；`<plugin_session_start plugin skill>` 包裹 — `agentPluginService.ts:138-147`；找不到技能 warn 跳过 — `agentPluginService.ts:110-118`。

## 5. 插件系统（代码事实）

### 5.1 manifest 与安装

- manifest 候选优先级链与遮蔽诊断 — `packages/agent-core-v2/src/app/plugin/manifest.ts`（v1 同构 `packages/agent-core/src/plugin/manifest.ts:19-24, 83-109`）；名称 regex `^[a-z0-9][a-z0-9_-]{0,63}$` — `app/plugin/types.ts:192`；skills 路径约束与根 `SKILL.md` 缺省 — v1 `manifest.ts:152-158, 198-248`；不支持字段 info 诊断 — v1 `manifest.ts:29-36, 185-196`。
- **v2 新增字段（v1 无、文档站字段表未列）**：`agents`（显式声明或根 `agents/` 目录自动采纳）— `packages/agent-core-v2/src/app/plugin/manifest.ts:130-134`、消费 `workspace/workspaceAgentProfileLoader/pluginAgentProfileLoaderService.ts:63`；`systemPrompt` / `systemPromptPath`（内联体积上限诊断）— `manifest.ts:290-313`、消费 `agent/profile/profileService.ts:927`。
- 安装流程（来源解析 → 下载/解压 → manifest 校验 → staging+rename 托管复制 → 回滚 → 记录保留旧偏好 → 原子持久化）— `packages/agent-core-v2/src/app/plugin/manager.ts:83-184`；GitHub 解析（绕开 api.github.com、裸仓库 latest 回退 HEAD、SHA 记录）— `app/plugin/github-resolver.ts`（v1 同构）+ `manager.ts:100-121`；zip 下载超时 / 路径穿越防护 / 权限恢复 / 单层包装下钻 — `app/plugin/archive.ts`（v1 同构）。
- 移除只删记录 — `manager.ts:261-269`；启停 / MCP 服务器级启停 / MCP 工具过滤（present-replace-absent-clear 补丁语义）— `manager.ts:186-259`；更新检查（GitHub ref/SHA 比较）— `manager.ts:271-287`；重载（+N -M errors）— `manager.ts:289-307`。
- 持久化 `<home>/plugins/installed.json`（version 1，原子写）— `app/plugin/store.ts:10, 54-64`（v1 同构）。
- 贡献面：技能根（`source` 标 `extra`，`manager.ts:351`）— `manager.ts:344-357`；会话启动 — `manager.ts:370-379`；系统提示词段 — `manager.ts:381-390`；MCP 运行名 `plugin-<id>:<name>` — `manager.ts:392-424` + types.ts:63-74；hooks（cwd=插件根 + env `MIRRICODE_HOME` / **`MIRRI_PLUGIN_ROOT`**）— `manager.ts:309-325`；命令 — `manager.ts:327-342` + `app/plugin/commands.ts`；agents — `manager.ts:359-368`。
- **[v1] env 差异**：v1 hooks env 为 `MIRRICODE_PLUGIN_ROOT`（v1 `packages/agent-core/src/plugin/manager.ts:245-253`，`enabledHooks`；其 MCP 服务器运行时 env 同名 — `:488-512`）；v2 为 `MIRRI_PLUGIN_ROOT`（`manager.ts:319`）；文档站写 `MIRRICODE_PLUGIN_ROOT`（`docs/en/customization/plugins.md:315`）——**三方漂移**（缺口 G2）。
- **node 回退（新旧引擎均有）**：v2 `manager.ts:728` 定义 `__plugin_run_node` 回退子命令，`:777-785` 在原生二进制构建下把 MCP `command:"node"` 替换为自带解释器入口，`:764` 注入 `MIRRI_PLUGIN_ROOT` env；另有 Electron 分支（`:767-775`，`ELECTRON_RUN_AS_NODE`）。v1 为 `MIRRICODE_PLUGIN_ROOT`（v1 `manager.ts:28-31, 488-512`）。**确认断链**：TUI 侧隐藏子命令要求 `MIRRICODE_PLUGIN_ROOT`（`apps/mirri-code/src/cli/sub/plugin-run-node.ts:6-8`），而 v2 注入的是 `MIRRI_PLUGIN_ROOT`——原生二进制 + 插件声明 `command:"node"` 时该回退必然因 env 不匹配失败（缺陷 D1，并入 G2 漂移族）。
- v2 `PluginManager` 构造注入 `discoverSkills`（默认 `discoverFileSkills`）供 `skillCount` 统计 — `manager.ts:44-59`（对照 v1 直接 import v1 scanner）。

### 5.2 marketplace（TUI 客户端）

- 目录加载（默认 URL / env `MIRRICODE_PLUGIN_MARKETPLACE_URL` / 参数覆盖；http/https、file://、本地路径；默认失败回退源码仓本地目录）— `apps/mirri-code/src/utils/plugin-marketplace.ts:76-96, 137-171`；条目解析与错误文案 — `plugin-marketplace.ts:112-135, 174-201`；分级 official/curated — `plugin-marketplace.ts:13-15, 216-233`；版本从 GitHub URL 推导 / 裸仓库查 latest release（绕 API 配额）— `plugin-marketplace.ts:253-350`；更新判定（双方合法 semver 且 latest>local 才报 update；未知不误报）— `plugin-marketplace.ts:34-62`。
- 常量（默认 URL）— `apps/mirri-code/src/constant/app.ts:87-88`；随产品分发目录 `plugins/marketplace.json`（mirri-datasource official、superpowers/vercel-plugin curated）。
- `/plugins` 命令族 UX（面板四标签、信任确认默认取消、重载报告、MCP 启停、移除确认、详情含技能目录列表）— `apps/mirri-code/src/tui/commands/plugins.ts:47-152, 253-294, 336-359, 448-546`；面板 — `apps/mirri-code/src/tui/components/messages/plugins-status-panel.ts:50, 105-114`。

## 6. 消费端（代码事实）

### 6.1 TUI / CLI

- `--skills-dir` 旗标（可重复，"Load skills from this directory instead of auto-discovered user and project directories"）— `apps/mirri-code/src/cli/commands.ts:66-73, 131`；类型 — `cli/options.ts:13`；传递 — `cli/run-prompt.ts:118`、`cli/run-shell.ts:62`、`packages/node-sdk/src/sdk-rpc-client-rust.ts:457-461, 621-624`。
- **无** `skill`/`plugin` 顶层 CLI 子命令（只有 export/provider/acp/server/login/doctor/vis/upgrade + 隐藏 `__plugin_run_node`）— `cli/commands.ts:86-108`。
- **无** `/skills` 内置斜杠命令（注册表 137-415 无此项）— `apps/mirri-code/src/tui/commands/registry.ts`。
- 技能斜杠命令构建（builtin/子技能裸名、其余 `skill:` 前缀；用户可激活类型过滤；builtin 优先排序）— `apps/mirri-code/src/tui/commands/skills.ts:12-48`；刷新时机（切换/重载/新建；失败静默）— `tui/mirri-tui.ts:456-477, 667, 1685, 1723, 1765`；补全合并 builtin+skill+plugin — `mirri-tui.ts:412-450`；`/help` 中 `skill:` 前缀单独分组 — `tui/components/dialogs/help-panel.ts:147-156`。
- 命令解析优先级 builtin > skill > plugin-command > 消息；省略前缀解析；忙碌拦截文案 — `tui/commands/resolve.ts:55-151`；分发（`input_command` 埋点 + `sendSkillActivation`，媒体占位重写）— `tui/commands/dispatch.ts:205-217`、`tui/mirri-tui.ts:1264-1284`。
- 激活卡片（`▶ Activated skill:` + 参数预览截断 200；事件按 activationId 去重）— `tui/components/messages/skill-activation.ts:22-63`、`tui/controllers/session-event-handler.ts:941-955`；回放 — `tui/controllers/session-replay.ts:322-324, 424`；`/undo` 锚点 — `tui/commands/undo.ts:238-420`；触发类型三态 — `tui/types.ts:153`。
- 技能调用审批展示（`skill_call` → "invoke skill <名>"）— `tui/reverse-rpc/approval/adapter.ts:197-198, 308-313`。
- headless `-p` 模式把 `skill.activated` 列为已知不投影 — `cli/run-prompt.ts:605`。
- ACP legacy 模式复用技能命令构建（失败降级 builtin-only）— `cli/sub/acp-legacy.ts:60-82`。

### 6.2 REST / RPC / SDK

- REST 三路由 + 会话激活门（40401 区分 not activated / not exist）+ 工作区边缘组合扫描 + 投影（`isSubSkill` 刻意不外发；`disableModelInvocation` 置位时**外发**，见 `toProtocolSkill` `skills.ts:406-422`——2026-10-10 补记完整口径）+ 错误映射（40415/40912/40001/40410）+ 激活立即返回（queued/running + prompt_id）+ 首条技能消息命名会话 — `packages/kap-server/src/routes/skills.ts:154-333, 352-398, 404-445`；路由注册 — `routes/registerApiV1Routes.ts:43, 139`；协议 schema — `protocol/skill.ts:3-14`、`protocol/rest-skill.ts:10-32`。
- `skill.activated` 事件：WS 广播白名单 — `transport/ws/v1/sessionEventBroadcaster.ts:1400`；zod schema（含 trigger/skillPath/skillSource）与 origin schema — `protocol/events-zod.ts:607-614, 126-134`；转录映射 marker op — `services/transcript/coreEventMap.ts:268-271`；wire 提取（user-slash 激活视为真实用户 prompt / 撤销锚）— `search/wireExtract.ts:119-214`。
- 工具投影（wire 名 `user` → 协议 `skill`）— `routes/tools.ts:28, 287-296`；display kind `skill_call` — `protocol/display.ts:44-45`。
- RPC：`skill.list` / `agent.activateSkill` — `packages/node-sdk/src/sdk-rpc-client-rust.ts:250, 291, 331`；v2 直连（含 `updatePromptMetadata` 标题）— `sdk-rpc-client-v2.ts:1460-1484`；进程级 `skillDirs` 启动参数 — `packages/kap-server/src/start.ts:144-149, 264`。
- **[v1] 独有保留面**：`packages/agent-core/src/services/skill/`（`ISkillService` 防腐层：camelCase→snake_case 投影、错误码 40415/40912/40401 映射）— `services/skill/skill.ts:47-120`、`services/skill/skillService.ts:21-79`；v1 server 退役后**仓库内无运行时消费者**，仅自 `services/index.ts:184-189` 导出；kap-server 路由头注释声明以它为逐字节兼容基准 — `kap-server/src/routes/skills.ts:36-55`。

### 6.3 Web

- 斜杠菜单（`skill:` 前缀条目、选中保留输入框）— `apps/mirri-web/src/lib/slashCommands.ts:65-94`；菜单数据源（会话/工作区两级）— `composables/useMirriWebClient.ts:1934-1940`；渲染 — `components/chat/SlashMenu.vue:44`。
- RPC（`skill.list` / `agent.activateSkill`）— `api/daemon/ws-rpc-api.ts:1424-1459`；激活乐观流程与排队暂驻 — `composables/client/useModelProviderState.ts:409-513`；空会话首条技能 — `composables/client/useWorkspaceState.ts:1358-1415`；懒加载 — `composables/client/useWorkspaceState.ts:393-394`、预热 — `composables/useMirriWebClient.ts:2440-2450`；旧 daemon 降级 — `composables/client/useModelProviderState.ts:106-111, 258-277`。
- 激活呈现（i18n「已激活技能：」）— `components/chat/ChatPane.vue:572-577`、`i18n/locales/zh/conversation.ts:18`；turn 归并与标题 — `composables/messagesToTurns.ts:498, 808, 888`、`components/chat/ConversationPane.vue:297`；事件不重投影 — `api/daemon/agentEventProjector.ts:1419`。
- ~~设置页仅「合并所有可用 Skills」开关；**无** extra_skill_dirs 编辑 UI~~（已被下方 B8 增量取代）；工具标签编辑器把 Skill 工具归入技能组 — `components/settings/ToolTagEditor.vue:74, 93-100, 173`；类型 — `api/types.ts:855-856, 867-872`；config 键映射 — `api/daemon/client.ts:1293-1294`、wire — `api/daemon/wire.ts:529`。
- Desktop 无技能面（仅注释提及配置共享）— `apps/mirri-desktop/src/main/ensure-server.ts:328`。

**Web 管理面增量（B8 Web 轨，2026-09-13 落地，commit `129c2d9a`）**：

- `AppSkill` 增 `path?/enabled?`（缺省 `enabled !== false` 才禁用，仅显式 false 禁用）— `api/types.ts:878-891`；`AppConfig.disabledSkills?` — `api/types.ts:870`；`MirriWebApi.setSkillEnabled/readSkillContent`（v3-only）— `api/types.ts:1115-1118`。
- v3 适配器：`RpcSkill` — `api/daemon/ws-rpc-api.ts:3131-3138`；`toAppSkill` 透传 — `:3393-3404`；`setSkillEnabled`（snake→camel `disabled_skills→disabledSkills`）— `:1493-1510`；`readSkillContent`（`skill.getContent`，`session_id` 参数）— `:1511-1521`。v1/v2 REST 诚实降级（throw "requires the v3 (rust) backend"）— `api/daemon/client.ts:870-879`；`disabledSkills` 写面 keyMap — `client.ts:1312`、读面 — `wire.ts:530` + `mappers.ts:975`。
- 斜杠菜单：禁用过滤 + `includeDisabled` 选项（菜单默认过滤；Composer 识别门禁传 `includeDisabled: true`，防禁用技能 `/skill:x` 以明文进对话、由服务端诚实拒绝）— `lib/slashCommands.ts:78-113`、`components/chat/Composer.vue:343-345`
- **输入区信息架构（2026-10-12 快照，SDR-003/SDR-004 / ADR-002/ADR-003 裁决前的现状；**已由 `765db622` 落地修复，见下条****）：斜杠菜单为无分组平铺清单——单一 `v-for` 渲染、无组标题/折叠/数量上限 — `components/chat/SlashMenu.vue:31-46`；条目构建 `[...SLASH_COMMANDS, ...skillItems]`（命令固定在前，技能追加其后）— `lib/slashCommands.ts:94-108`；菜单开启条件（`/` 开头且无空格）— `composables/useSlashMenu.ts:45-49`；技能条目描述按原文渲染（空描述时渲染符号残留如 `>`）— `SlashMenu.vue:44`；过滤打分以带 `skill:` 前缀的完整名为键（前缀分被前缀吃掉）— `lib/slashCommands.ts:124-131`。无技能专用触发符（`$` 及全角等价）、无统一技能图标（SlashMenu 无任何图标资产；设置页 SkillsPanel 亦无）。2026-09-18 评测实录：多技能环境菜单一次列出 100+ 条技能条目 — `docs/specs/reports/2026-10-08-todo-audit/04-斜杠菜单被技能淹没.md`。`@` 文件提及菜单（`useMentionMenu.ts`）为触发符菜单的既有同构先例。。
- **输入区信息架构落地后现状（2026-10-12，`765db622`，SDR-003/SDR-004 执行）**：`/` 菜单分「命令 / 技能」两组带标题（`SlashMenu.vue` `groupSlashItems`，命令组在前、hover/active 语义不变）；技能专用触发符 `$` / `¥` / `￥`（`SKILL_TRIGGER_STARTS` + `useSkillTriggerMenu.ts`，选中回填规范斜杠命令，`$` 前缀普通文本照常发送）；空描述回退 `commands.noDescription` 中性占位（`isBlankSkillDesc`）；技能按裸名参与过滤打分（`skillDisplayName`）、内建技能稳定排前；统一 bolt 技能图标（输入菜单、SkillsPanel、工具标签编辑器、激活卡片四处）。**未做（SDR-004 明确否决）**：技能区折叠与数量上限——`/` 菜单保持完整可浏览。**遗留**：新增纯逻辑（`filterCommands` 裸名打分 / `buildSlashItems` 排序 / `groupSlashItems` / `isBlankSkillDesc`）无对应测试文件。
- **技能图标字形变更（2026-10-17，ADR-005）**：统一技能字形由 bolt 更换为专属魔杖+四芒星字形（`apps/mirri-web/src/icons/mirri/skill.svg`，注册名 `skill`）；全部技能表面（斜杠菜单技能组、技能触发符候选、ReferenceChip 芯片、SkillsPanel、工具标签编辑器、激活卡片、TipTap 提及节点）已切换；bolt 保留为通用图标。
- **内联注入实现锚点侦察（2026-10-13 快照，SDR-005 / ADR-004 设计输入）**：输入框为纯 `<textarea>`（`Composer.vue:883-894`，`v-model` 文本）——无 chip-in-text 基础设施，附件 chip 在输入行上方独立渲染（`Composer.vue:813-830`）；`@` 提及是纯文本 token 替换（`useMentionMenu.ts:80-95`，选中替换为文件路径字符串），非结构化 token。v2 提交路径上的改写点：`onBeforeSubmitPrompt` 钩子（`prompt/prompt.ts:68`、执行点 `promptService.ts:232-234`）与 `UserMessageStepRequest.onWillMaterialize`（`promptStepRequests.ts:48-56`）；技能激活的既有改写先例为 `skillService.ts:85-104` 的 materialize。触发方式枚举：v2 `contextMemory/types.ts:18`（`'user-slash' | 'model-tool' | 'nested-skill'`）、Rust `origin.rs`（`Option<String>` 开放字符串）。web 提交体 `PromptSubmission`（`api/types.ts:298-318`）在 text 之外已有多字段先例（metadata/模式字段族）。用户消息气泡按 verbatim 渲染（`ChatPane.vue:761-762` 注释明示「never through Markdown」），markdown 渲染组件为 `Markdown.vue`。
- 管理面板：`components/settings/SkillsPanel.vue`（McpServersPanel 自取数模式、v3 gate、`registered:false` 回滚、dirs 编辑器 lastSavedDirs 守卫、≤640px 单列）；Settings tab 接线 — `components/settings/SettingsView.vue:34,117,608-613`；共享搜索 primitive `components/ui/ToolbarSearch.vue`（三个面板采用，已注册进 `apps/mirri-web/AGENTS.md`）。
- `refreshSkills()`（启停后刷新 composer `/` 菜单缓存，best-effort）— `composables/useMirriWebClient.ts:2554-2562`（导出 `:3162`）；纯逻辑 — `lib/skillsManagement.ts`（filterSkills/normalizeExtraDirs/SKILL_SOURCE_ORDER）；i18n en/zh 各 25 组新键（键集脚本核对一致）。

## 7. 两侧差异矩阵与缺口（TS 实际 vs 文档站 / v1 vs v2）

### 文档站 vs v2 代码（漂移即缺口）

| # | 项 | 文档站 | v2 代码 | 影响 |
|---|---|---|---|---|
| G1 | 技能目录占位符 | `${MIRRICODE_SKILL_DIR}`（en/zh skills.md:60） | `${MIRRI_SKILL_DIR}`（registry.ts:176） | 用户按文档写占位符不展开 |
| G1b | 会话标识占位符 | 未记载 | `${MIRRI_SESSION_ID}`（registry.ts:177） | 未披露能力（文档缺口） |
| G2 | 插件 hooks/解释器入口 env | `MIRRICODE_PLUGIN_ROOT`（plugins.md:315） | `MIRRI_PLUGIN_ROOT`（manager.ts:319） | 插件作者按文档取不到变量；并叠加解释器回退断链（D1，见 §5.1） |
| G4 | 插件 manifest 字段 | 字段表无 `agents` / `systemPrompt` | 已支持（`agents` — manifest.ts:130-136；`systemPrompt` 读取与体积上限诊断 — :284-351、字段挂接 :161） | 已落地能力未披露 |
| G5 | marketplace 自定义目录 | plugins.md:56-71（已记载） | — | 一致（无缺口） |

### v1 vs v2（能力对照）

| 项 | v1 | v2 | 结论 |
|---|---|---|---|
| 占位符 / env 命名 | `MIRRICODE_*` | `MIRRI_*` | 双轨漂移（G1/G2 根因） |
| Skill 工具错误文案 | "in v1" 措辞合理 | 同文案残留 | 遗留措辞（G3） |
| 插件贡献 agents / systemPrompt | 无 | 有 | v2 增强未入文档 |
| 单二进制 node 回退 | `__plugin_run_node`（env `MIRRICODE_PLUGIN_ROOT`） | 同子命令（env `MIRRI_PLUGIN_ROOT` + Electron 分支，manager.ts:728, 764-785） | 两代 env 名不同 → TUI 子命令检查断链（确认缺陷 D1，见 §5.1） |
| REST 技能面 | v1 server（已退役） | kap-server 逐字节兼容 | 平移完成 |
| 技能根对文件工具开放 | workspace additionalDirs | 逐工具注入根 | 机制不同语义等价 |

### 管理面缺口（TS 现状没有、需求 §11 已列为提案）

- 无 `/skills` 清单命令（TUI/web 均无技能列表视图）；无逐技能启停；无 web 技能管理页（仅 merge 开关）；无 `skill create` 脚手架；`extra_skill_dirs` 无 web 编辑 UI。

## 8. 开放项

开放项的唯一登记处为 [skills-ROADMAP.md](skills-ROADMAP.md) §4（避免双重维护漂移）；本节仅提示：O1/O3/O4 与缺陷 D1 的详证散见上文 §1、§5.1、§6.2、§7。

## 9. Rust 引擎侧管理面实现（B8 Rust 轨，2026-09-13 落地）

> B8（缺口 D-32）Rust 轨 R1-R9 的实现事实快照（file:line 为 2026-09-13 现行代码）；web 管理页（W1-W7）由 web 轨随后落地。TS v2 引擎无对应面，全部为 RustFirst。

### 9.1 用户级禁用集（D-32 核心）

- **config 键**：`ServerConfig.disabled_skills: Option<Vec<String>>`（只记禁用名，缺省/空 = 全启用）— `rust/crates/mirri-core/src/config/mod.rs:104-109`；wire 投影 `disabledSkills` — `config/wire.rs:134-139`；`KNOWN_CONFIG_TOP_LEVEL_KEYS` 白名单 — `wire.rs:457`。三处同改，`merge_wire_patch` 接受 `disabledSkills` patch（含空数组 = 清空语义）。
- **catalog 状态**：`SkillCatalog` 内建 `user_disabled: RwLock<HashSet<String>>`（不进 `SkillDef` serde 形状）+ 三个 API：`set_user_disabled`（整集替换，装配种子/resync 用）、`set_enabled(name, bool) -> bool`（逐名启停，返回 registered）、`is_disabled(name)` — `rust/crates/mirri-core/src/skill.rs:103, 239-269`。
- **生效点三处**（rust 无提示词清单注入，对照 TS §3）：① `skill.list` 每行增 `enabled: bool`（增量字段，缺省视为启用）— `embedded_server/session_handlers.rs:3383-3391`；② `agent.activateSkill` 拒绝禁用技能（invalid_params "disabled by the user"）— `session_handlers.rs:3538-3544`；③ SkillTool execute 对禁用技能返回 error result（不注入）— `mirri-core/src/tools/skill.rs:138-145`。

### 9.2 新 RPC ×2（五环契约链，均 `both_kept` RustFirst）

- `skill.getContent {name, session_id?} → {name, content}`：session 在场用其 catalog，否则默认 workspace catalog；未知名答 `skill.not_found`（-32001）— `session_handlers.rs:3402-3438`、`responses.rs` `SkillGetContentResult`。
- `skill.setEnabled {name, enabled} → {name, enabled, registered, disabled_skills}`：registered 判定 = 任一 built runtime catalog 命中；经 `replace_sections_raw` 写 `disabled_skills` config 键（未知名不写配置）— `session_handlers.rs:3440-3505`。
- 五环落点：registry `registry.rs:470-472`（计数 pin Embedded **193** / Server **183**）；TSV `authoritative-methods.tsv:109-110`（Rows 208）；EMB 路由 `routes/agent.rs`；node-sdk fixture `packages/node-sdk/test/fixtures/rust-engine-methods.ts`（skill 域块 + e2e 计数 193）；跨后端 `mirri-test-harness/tests/cross_backend_contract.rs`（skill.getContent 双后端 assert_same_shape + serve 面 183 门）。mirri-server 经 engine fall-through 零代码。

### 9.3 extra_skill_dirs 消费 + 热生效（F5 断链修复）

- **装配消费**：`apply_skill_discovery`（crate 级共享函数，`rust/crates/mirri-embedded/src/bootstrap.rs:393-433`）对一个 workspace 重放完整序列：clear →（explicit skillDirs 非空 ? 替换式发现 : 默认根〔`WorkspaceScope::discover_default_skill_roots`，从 `new()` 提取，`workspace_scope.rs:94-118`〕+ user-brand）→ config `extra_skill_dirs`（`~`/绝对/相对 workspace root，源 = Plugin 即 wire "extra"，`resolve_extra_skill_dir` `bootstrap.rs:435-445`）→ plugin roots → 禁用集种子。
- **热 resync**：`EmbeddedServer.resync_skill_catalogs`（`embedded_server/mod.rs:1294-1313`）对每个 built runtime 重放 `apply_skill_discovery`；钩子 `resync_skill_catalogs_if_changed`（`mod.rs:1316-1327`，比较两侧统一取 env 已展开的 in-memory 视图，避免 `${VAR}` 假阳性）挂在三条 config 写/读路径尾部：`mutate_config_raw`（`config_handlers.rs`，覆盖 config.replace / config.replaceSections / skill.setEnabled）、`config.set` 的内联管线（`session_handlers.rs`）、`config.reload`（手改 config.toml 后的刷新面）。注意 **F8 与现实的偏差**：`config.set` 不经过 `mutate_config_raw`（自带内联管线），故钩子分别挂接；`skill.setEnabled` 的集合运算在 `mutate_config_raw` 的 mutate 闭包内完成（读-算-写原子，防整集丢更新）。

### 9.4 设计论证与留档漂移

- **D-2（不进 session wire log）**：技能禁用态是用户配置不是会话状态域——写侧 `record_op`/reducer 恢复护栏（rust/AGENTS.md #2）不适用；恢复路径 = workspace 装配时从 config 种子 + config 写后 resync。无新事件词汇（护栏 #3 不触发，事件三端零改动）。
- **F5 漂移修复**：`extra_skill_dirs` 在 rust 侧从纯 passthrough 变为装配消费 + resync（见 §9.3），路径解析镜像 TS `skillRoots.ts:129-134`。
- **F6 漂移留档（D-7 不做）**：`merge_all_available_skills` 在 rust 仍无消费方（发现为"后到者胜"，无 TS 的"每组取第一个"源分组合并）；web 开关既有、行为差异记此处，B8 不改。
- **shadowing 优先级漂移留档**：rust 发现"后到者胜"（`skill.rs` `discover` 文档注释），无 TS 的 source priority 常量（builtin 0 / plugin 5 / extra 10 / user 20 / workspace 30）——既有漂移，另立（D-7 裁决）。
- **explicit skillDirs 抑制 extra dirs（review 发现，留档）**：explicit `skillDirs` 非空时（v1/v2 SDK 替换契约）rust 一并跳过 config `extra_skill_dirs`；TS v2 的 `ExtraFileSkillSource` 在 explicit 模式下不被抑制（被抑制的只有 user/root 默认源）。按计划 D-5 代码块实施（extra dirs 位于 else 分支），差异以 `apply_skill_discovery` 的 doc 注释与本条留档，待产品裁决是否对齐。
- **相对 extra dirs 锚点漂移（review 发现，留档）**：TS 相对路径锚在 `findProjectRoot`（向上找 `.git`）并做 realpath 归一 + 去重（`skillRoots.ts:72,80-89,118-127`）；Rust 锚在 `workspace.cwd()`、无归一去重。workspace root ≠ git root 时同一配置串两侧解析到不同目录——D-5 的"镜像 129-134"仅覆盖路径字符串规则，锚点差异留档待裁决。
- **`skill.list` 解析域不对称（review 发现，留档）**：list 与 getContent（无 session 时）只读默认 workspace catalog（F3 既有行为延续），而 `skill.setEnabled` 的 registered 判定横跨全部 built runtimes——多 workspace 下其它 workspace 的启停态在 list 面不可见。
- **粒度裁决注记**：requirements §11 提案 2 的会话级（临时禁用）粒度未随 B8 落地——落地范围为用户级（DEC-2 裁决内缩，ROADMAP §6 已注）。
- **fixture 漂移修复（附带）**：`collaboration.enter/exit`（commit 03c3ecb1）加了 registry 行但漏更新 node-sdk fixture（计数 189 vs registry 191），B8 顺手补齐两条 skip-mode 条目恢复契约链完整（fixture 现与 registry 193 对齐）。


## 10. 内置技能逐技能规格的现状快照（2026-10-10，随 skills-builtin-skills.md 首建）

> 本节为 [skills-builtin-skills.md](skills-builtin-skills.md)（逐技能需求规格）的现状证据层：注册与门控的代码事实、v1/v2 文案差异、消费端入口、Rust 侧缺口。

### 10.1 注册与门控（v2 主源）

- 聚合注册：`BUILTIN_SKILLS` 数组 + `registerBuiltinSkills` — `packages/agent-core-v2/src/app/skillCatalog/builtin/builtin.ts:23-38`；六个单技能薄包装（`?raw` 导入 .md → `parseSkillText` → 伪路径 `builtin://<名>` 覆盖）— 同目录 `write-goal.ts` 等五个 + `sub-skill.ts`（父 + review + consolidate 三条目）。
- 禁用模型调用的来源分两处：`mcp-config.ts:25`、`custom-theme.ts:25`、`sub-skill.ts:40,47,54`（含 `has-sub-skill: true` 父标记与 `isSubSkill: true`）在**包装内覆盖**；`import-from-cc-codex.md:4` 在 **frontmatter 声明**（`disable-model-invocation: true`）；`write-goal` / `update-config` 两处皆无 → **模型可调**（模型清单里可见，requirements US4.1）。
- 正文长度（v2 行数）：import-from-cc-codex 281 / mcp-config 141 / custom-theme 132 / update-config 104 / write-goal 94 / sub-skill 家族 3 个 SKILL.md。
- 模型清单「Built-in」分组渲染 — `packages/agent-core-v2/src/app/skillCatalog/registry.ts:215-220`（来源分组表，Built-in 行 :219）+ `:222-233`（分组循环）；子技能不进模型清单 — `registry.ts:134-135` 过滤。

### 10.2 v1 对照（差异清单）

- v1 注册入口 — `packages/agent-core/src/skill/builtin/index.ts:13-22`（8 次 `registerBuiltinSkill` 调用，来自 6 个模块）。
- **write-goal 与 update-config：两代 diff 零输出**（逐字一致）。
- mcp-config 差异：品牌词（Mirri→Kimi）；schema 路径改指 v2（`config-schema.ts`）；v2 新增「环境变量展开」段（`${VAR}`/`${env:VAR}`、未定义展开为空、键与数值不展开）与「全局超时默认」段（config.toml mcp 节 / `MIRRI_MCP_*_TIMEOUT_MS` env、1..2147483647ms）；yolo/afk → yolo/auto；密钥段落改写（v1 允许 stdio env 明文 + 警告 → v2 环境引用优先原则）。
- custom-theme 差异：官方文档 URL 由 `mirricode.com` 改为 `moonshotai.github.io/mirri-code`。
- import-from-cc-codex 差异：全文品牌改写（Claude Code / Codex → Mirri Code 语境）。

### 10.3 消费端入口

- TUI 裸名斜杠命令（builtin 来源不带 `skill:` 前缀、builtin 优先排序）— `apps/mirri-code/src/tui/commands/skills.ts:12-48`；命令解析优先级 builtin > skill > plugin-command — `tui/commands/resolve.ts:55-151`。
- web 斜杠菜单透传（builtin 裸名条目）— `apps/mirri-web/src/lib/slashCommands.ts:95-109`；v2 后端（kap-server）下六个技能全部出现；REST/RPC 清单 — context §6.2/§6.3。
- 与客户端命令的成对关系（技能 ↔ 命令）：`write-goal`↔`/goal`、`update-config`↔`/config`（正文另指 `/theme`/`/editor`/`/reload`/`/reload-tui`）、`mcp-config`↔`/mcp`、`custom-theme`↔`/theme`——命令本体归各消费端/相邻域。

### 10.4 Rust 侧缺口（对照规格的移植空白）

- Rust 引擎**无 builtin 技能注册通道**：`SkillSource::Builtin`（`rust/crates/mirri-core/src/skill.rs:49`）仅为磁盘发现的来源标签，无代码内技能注册；发现序列只走 plugin → extra → user 链（`rust/crates/mirri-embedded/src/bootstrap.rs:412` `apply_skill_discovery`，注释原文 "the plugin floor above builtin (0), which has no rust discovery channel"，见 `bootstrap.rs:404`；「TS builtin(0) 属性由构造保证」的表述出自 `plan/rust-plugin-parity.md` G 行）。
- `rust/crates/` 全域 grep 六技能名与 `builtin://` **无任何注册或 .md 资产命中**（唯一字符串命中为测试 fixture 的 workspace id `mcp-config-ws`，`mirri-embedded/src/embedded_server/tests.rs:16027`）——六个内置技能（含两个模型可调的 write-goal / update-config）在 v3 后端完全缺失，web 端 v3 下相应斜杠入口与模型自动调用随之下线。既有唯一记载：`plan/rust-plugin-parity.md` G 行（无排期）。
- 规格侧状态：`skills-builtin-skills.md`（2026-10-10）已给出逐技能对齐规格；Rust 侧架构设计已立（`docs/specs/rust-core-design/13-builtin-skills.md`，ADR-B1..B6），移植排期登记为本域 ROADMAP O5 与 rust 域 ROADMAP 1.10。

### 10.5 Rust 侧落地快照（2026-10-10，分支 `feat/rust-builtin-skills`）

> builtin 通道实现事实（实现前差距见 §10.4 上文，保留作历史对照）。执行状态唯一源为 rust 域 [ROADMAP.md](../rust-core-design/ROADMAP.md) 1.10。

- 落点：`rust/crates/mirri-core/src/skill.rs`（SkillDef `disable_model_invocation`/`is_sub_skill` + `parse_skill_metadata` 两键归一解析 + catalog `builtin_contents` 表/`register_builtin`/`read_content` Builtin 分派）、`rust/crates/mirri-core/src/skill/builtin.rs`（8 条目 `include_str!` 资产，覆盖点镜像 TS 包装；sub-skill 伪路径取 TS 字面值 `builtin://sub-skill/review` 等）、`rust/crates/mirri-embedded/src/bootstrap.rs`（`apply_skill_discovery` 首插）、`rust/crates/mirri-core/src/tools/skill.rs`（禁模型调用门，文案逐字 `Skill "X" can only be triggered by the user (model invocation is disabled).`）、`rust/crates/mirri-core/src/scope/system_prompt.rs`（`render_skills_section`）、`session_handlers.rs`（`skill.list` 置位外发 `disable_model_invocation`——TS REST 字面键 snake_case）。
- 正文对齐：v2 逐字 + 披露调整——import-from-cc-codex 全文品牌 Kimi→Mirri（12 行）；mcp-config "Kimi-specific"→"Mirri-specific" + 内部代码路径引用改指官方文档。
- 验证：V1-V8 全部自动化；真端口 WS JSON-RPC e2e 闭环（`mirri-server/tests/skill_e2e_tests.rs`）；llvm-cov 区域覆盖 94.5–100%（builtin.rs 100%）。
- 已知留档：发现层默认分支读宿主 `HOME` 的测试密封性问题（测试用非空 explicit skillDirs 封装，产品代码未动，待架构裁决）。

## 11. Rust 引擎侧技能目录刷新现状快照（2026-10-23，随 SDR-008 / ADR-010 首建）

> 热更新（US7.2）实现前的 rust 引擎刷新机制现状（file:line 为 2026-10-23 现行代码）。执行状态唯一源为 [skills-ROADMAP.md](skills-ROADMAP.md)。

- **装配时一次性扫描，无文件监视**：workspace 装配时 `WorkspaceScope::new` 调 `discover_default_skill_roots()` 扫默认根（`rust/crates/mirri-core/src/scope/workspace_scope.rs:94-118`），完整发现序列由 `apply_skill_discovery`（`rust/crates/mirri-embedded/src/bootstrap.rs:393-433`）重放：clear → explicit skillDirs / 默认根 → config extra dirs → plugin roots → 禁用集种子；索引常驻 `RwLock<HashMap<String, SkillDef>>`（`rust/crates/mirri-core/src/skill.rs`）。技能目录本身无 watcher、无 TTL。
- **仅有的两条刷新窄路**：① 配置变更重放——`EmbeddedServer.resync_skill_catalogs`（`embedded_server/mod.rs:1797-1831`）对每个 built runtime 重放 `apply_skill_discovery`，由 config 写路径尾钩子 `resync_skill_catalogs_if_changed` 触发（仅当 `extra_skill_dirs` / `disabled_skills` 两个技能衍生键变化，见 §9.3）；② 进程重启重新装配。
- **系统提示词清单为快照**：`render_skills_section(catalog)` 渲染技能清单（`rust/crates/mirri-core/src/scope/system_prompt.rs:276`），在 agent 创建时求值一次进系统提示词（`session_scope.rs:1665` 注释明确 "assembly-time snapshot" 策略）；`refresh_agent_system_prompt` 只重派生 AGENTS.md 与目录树段，不重渲染技能段。
- **系统提醒通道现成**：`system_reminder_wire.rs` 提供系统提醒的 wire 记录原语（`rust/crates/mirri-core/src/system_reminder_wire.rs`）；但**注意无「回合边界」注入先例**——既有注入全部是回合内每 step 粒度（`InjectionManager::inject` 在 `before_step` 每步执行，`turn_flow/hooks.rs:827-831`），插件会话启动注入是「会话首个 step + 历史扫描去重」非回合边界粒度，中断提醒是「回合取消后即时追加」；「下一轮注入」需要新挂点裁决（ADR-010 决策 5）。rust 侧亦无插件中性化声明路径（`turn_flow/injection/plugin_session_start.rs:18-22` 头注释自认）。
- **文件监听依赖现成**：`notify` crate 已被 `mirri-cli` 的 config 监听使用（`rust/crates/mirri-cli/src/watcher.rs`，含防抖与测试），但依赖挂在二进制层（`mirri-cli`），库层 `mirri-embedded` 尚无该依赖（ADR-010 需迁入）。
- **工具面门控分叉**：rust 的系统提示词清单注入无工具面门控（`render_skills_section` 调用点无 tools 判定，无 Skill 工具的子代理也拿到清单）；TS 的 `skillActiveFor` 在 rust 无对应物（ADR-010 决策 6 披露）。
- **手动重载入口空白**：rust 侧无任何「重扫技能目录」的用户入口（RPC 面 `skill.list` / `skill.getContent` / `skill.setEnabled` 均为读 / 配置面）。
- **TS dotfiles 同构隐患（ADR-011 评审留档）**：TS 项目级发现走 `.git` 上溯，用户把主目录做成 git 仓库（dotfiles 管理惯例）时，项目通道与用户通道同样解析到同一目录；TS 冻结不修，rust 侧已按 ADR-011 去重（2026-10-23 留档）。
