# Skills 技能系统架构文档

> 本文描述 TS 引擎技能系统的实现机制、数据流与接口形状（技术受众）。动机以抽象问题陈述给出；现状证据（文件、行号、快照日期）见 [skills-context.md](skills-context.md)，本文不承载。
> 需求语义见 [skills-requirements.md](skills-requirements.md)。
> 决策记录：ADR-001 ~ ADR-010 记录 TS 参照实现的机制裁决；ADR-011 起为 Rust 核心的机制裁决（Rust 侧实现落地时追加，需求侧 SDR 同步演进）。

## 1. 分层与作用域

技能系统横跨四个 DI 作用域，自底向上：

| 层 | 作用域 | 职责 |
|---|---|---|
| **skillCatalog**（应用级域） | App / Workspace | 纯粹的技能原语：类型与来源契约、根目录解析（路径探测，无状态）、`SKILL.md` 解析（纯函数）、文件系统发现后端、内存注册表、内置技能清单、配置段注册 |
| **workspaceSkillCatalog** | Workspace | 技能源（producer）编排：把六个来源（内置 / 用户 / 显式 / 额外 / 工作区根 / 插件）按优先级**每工作区一次**地合并成共享目录；订阅各来源的变更事件做来源级重扫 |
| **sessionSkillCatalog** | Session | 会话视图：接收工作区合并目录的只读快照种子，折叠成会话级目录，并支持会话本地临时贡献叠加（sink） |
| **agent/skill + agent/tools/skill** | Agent | 激活执行：用户斜杠激活服务（开新回合）、Skill 工具（模型侧，转向注入当前回合）、激活包裹渲染、激活事实记录与遥测 |

插件系统平行于技能系统但在其下层供料：插件管理器产出**技能根**（plugin skill roots）给插件技能源消费。

**动机**：技能清单必须对同一工作区的所有会话一致且只算一次（避免每会话重复扫描磁盘、避免清单漂移）；同时单个会话要能叠加临时贡献而不污染共享视图——因此「工作区合并 + 会话折叠」两级、种子传递数据而非重复计算。

## 2. 技能源与合并模型（Source / Contribution / Priority）

**接口形状**：每个来源实现 `ISkillSource`（`id`、`priority`、可选 `onDidChange` 变更事件、`load(): SkillContribution`）。`SkillContribution` 是纯数据：技能定义数组 + 可选的加载诊断（按策略跳过的技能、扫描过的根目录）。

**优先级常量**（数值小者先注册、被后注册者同名覆盖）：

```
builtin(0) < plugin(5) < extra(10) < user(20) < workspace(30)
```

合并语义：按优先级升序注册全部贡献，高优先级覆盖同名（`replace: true`）；名称匹配前统一小写归一。显式目录源（explicit）与用户源同级（20）——它是「用户指定的替代性用户技能目录」，不是最高优先级。

**根目录解析**（`skillRoots`，纯路径探测）：

- 用户级：品牌数据根下的 `skills/`（组内是否全收由 `mergeAllAvailableSkills` 决定）+ 通用 `~/.agents/skills/`（组内取第一个存在者）；
- 项目级：从工作目录向上找 `.git` 得项目根，其下 `.mirri-code/skills/`（同上合并开关）+ `.agents/skills/`（取第一个存在者）；
- 配置根（额外/显式共用）：支持 `~` 展开、绝对路径、相对项目根解析；realpath 归一并去重。

**合并开关的动机**：品牌目录组历史上存在多种拼写，默认全收（合并所有可用技能）保证不丢技能；关掉则每组只认第一个存在的目录，用于用户明确想收敛来源的场景。

**会话折叠**：会话目录持有种子（工作区合并目录的只读视图），每次种子变更或本地贡献变更时**重建**（re-fold）而非增量修改：先平铺种子全部技能，再按优先级叠加会话本地贡献。重建成本为内存级，换来实现简单与无状态漂移。

**工作区状态持久化**：合并目录的 contributions 与 merged 实例登记进 workspace state 注册表，随工作区生命周期读写（对崩溃恢复与调试可见）。

## 3. 发现与解析管线

### 3.1 文件发现后端（`ISkillDiscovery`）

`FileSkillDiscovery` 是唯一的具体后端（App 作用域服务），核心为无状态的 `discoverFileSkills(roots, warn?)`：

1. 对每个根按字典序遍历目录树（确定性输出），深度上限 8；跳过 `node_modules` 与点开头条目；
2. 含 `SKILL.md` 的子目录即目录技能包；根顶层的散 `.md`（与目录技能同名者除外）为平铺技能；插件根的顶层 `SKILL.md` 也算一个技能；
3. 目录技能若声明子技能开关（`has-sub-skill` / 驼峰 / 嵌套 `metadata.has-sub-skill` 三种写法），允许下钻其子目录，子技能名限定为 `父名.子名` 并打 `isSubSkill` 标记；
4. 每个文件解析结果按「根 + 插件上下文 + 归一名」为键去重（同一根内先发现者赢——排序保证确定性）；
5. 错误分诊：类型不支持 → 记入 `skipped`（按策略跳过，带类型与原因）；解析错误 → 警告日志；意外异常 → 警告日志。**单个失败永不中断整树**。

发现结果（技能数组 + skipped + scannedRoots）即 `SkillContribution` 的主体。

**一个可复用的细节**：发现函数不绑定 DI（既有 App 服务包装，也有导出的独立函数），供无会话场景（服务器边缘侧组合扫描）直接复用——这是 REST 工作区清单路由不建会话即可答询的机制基础。

### 3.2 `SKILL.md` 解析（纯函数）

- 头部配置区由通用 frontmatter 解析器处理（YAML）；字段别名归一（`when-to-use` / `when_to_use` → `whenToUse` 等）；
- 目录技能（文件名即 `SKILL.md`）：首行必须是 fence；`name` 与 `description` 必填（缺失抛解析错误）；平铺技能不强制；
- 类型校验：`prompt` / `inline` / `flow` / `reference` 为受支持集合（用户可激活 = inline 族 + flow；模型可调用 = inline 族），其余抛不支持类型错误；
- 平铺技能缺省：名取文件名，描述取正文首个非空行（240 字符截断，超限加省略号）；
- 正文额外提取 mermaid / d2 图表块为独立字段。

### 3.3 内置技能

代码内常量数组（不经过发现），经 `registerBuiltinSkill` 强制打 `builtin` 来源注册。作为最低优先级贡献，任何用户层同名技能覆盖之。

## 4. 系统提示词注入（清单层）

技能进入模型上下文是**两级结构**：

1. **常驻清单**（系统提示词）：`getModelSkillListing()` 渲染——首行声明取代历史更早清单；按来源分组（Project / User / Extra / Built-in）列出可调用技能（排除子技能、禁用模型调用者、非 inline 族），每条为「名称: 描述（图位素安全截断 250）+ 可选 When to use + Path」。
2. **按需全文**（激活时）：见 §5。

**门控**：注入由 agent 角色的工具面驱动——`skillActiveFor(tools) = 工具列表包含 'Skill'`。工具面不含 Skill 的角色（如只读侦察型子代理）既没有技能清单也没有 Skill 工具，两缺失同门。注入通道是 agent-profile 域的系统提示词模板变量（`skills` / `skills_section`，含技能段说明文字与 scope 优先级声明）——技能域产出清单内容，画像域产出模板与替换。

**动机**：技能全文可能很大，全部常驻会挤占上下文预算；清单 + 按需加载（渐进披露）让模型自己决定何时展开。

## 5. 激活管线（全文层）

激活有五个入口，共享同一渲染与记录基础设施：

| 入口 | 触发者 | 注入形态 | 回合语义 |
|---|---|---|---|
| 用户斜杠 | 用户（TUI/web/REST） | 用户角色消息 + 激活包裹 | **开新回合**（忙时排队） |
| 用户消息内联 | 用户（混合消息，见 §5.5） | 正文消息 + 前置激活包裹（turn 内消息序列） | **开新回合**（忙时整条随消息排队） |
| 模型工具 | 模型（经 Skill 工具） | 用户角色消息 + 激活包裹 | **转向（steer）注入当前回合** |
| 嵌套技能 | 模型（已加载技能再引用） | 同模型工具（触发标记不同） | 同上 |
| 插件会话启动 | 引擎（会话装配） | 系统提醒（system reminder）注入 | 会话开头一次性 |

### 5.1 用户斜杠激活（`IAgentSkillService`）

`enqueueDeferred(input)`：

1. **校验**（立即，同步返回错误）：技能存在 + 用户可激活类型；
2. **入队**：构造用户意图消息（正文为 `/<技能名> <参数>`），带激活 origin（唯一激活 id、技能名、触发 `user-slash`、类型、路径、来源、参数）；经提示队列排队（忙时不阻塞 HTTP/调用方——这正是 REST 路由能立即答 `queued` 的机制）；
3. **出队物化**（真实激活时刻）：记录激活事实 + 渲染技能正文 → 注入消息。渲染发生在出队而非入队，保证会话标识占位符等取真实值。

`activate()` 是「入队 + 等待回合真正启动」的阻塞包装（供本地调用方）；回合无法启动时返回「回合运行中」错误。

**wire 与事件**：激活事实是 `skill.activate` 操作（幂等 identity-apply），派生 `skill.activated` 事件（带触发方式、技能路径、来源）；恢复时重放为无副作用（事件与遥测不重放）。激活 origin 同时是撤销锚点与转录标记的依据。

### 5.2 Skill 工具（模型侧）

工具名 `Skill`，参数 `{ skill: 精确名, args?: 命令行风格参数串 }`。工具描述声明**强制要求**（匹配即必须调用）与**去重规则**（同参数已加载则直接遵循既有内容、不重复调用；参数不同则须重调）。

执行体 `executeModelSkill`：

1. 嵌套深度检查（上限 3，超限抛错）——深度经 `withInitialQueryDepth` 在克隆实例上传递；
2. **名称解析**（ADR-001）：先归一裸名精确匹配；未命中且含 `:` 时按首个 `:` 切分为（插件 id, 技能名）查目录的插件索引。模型工具与用户激活校验两个入口共用同一解析语义；
3. 三道门（依次）：未找到 → 禁用模型调用 → 非 inline 族；均为 `isError` 工具结果（无注入）；
4. 渲染技能正文 → 构造用户角色消息 + 激活 origin（触发标记按当前深度区分 model-tool / nested-skill）→ 经激活服务记录激活事实与遥测；
5. 返回结果声明 `delivery: { kind: 'steer', message }`——**工具只声明投递意图，转向执行归 agent 层的提示服务**（本回合上下文内追加该消息）。这个分离让工具无需依赖提示服务，也避免工具直接操作回合。

**display**：`{ kind: 'skill_call', skill_name, args }`，供各端结构化渲染；审批规则以工具自身名为规则名、按技能名做 glob 匹配（复用工具域的规则匹配原语）。

### 5.3 激活包裹与占位符展开（渲染层）

统一包裹 `<kimi-skill-loaded name=… trigger=… source=… dir=… args=…>`，属性 XML 转义、按存在性携带；前置引导语区分用户激活（「用户激活了…请遵循」）与模型加载（「技能工具为本请求加载了指引…」）。

占位符展开（渲染时，`renderSkillPrompt`）：参数分词器支持单/双引号分组；`$ARGUMENTS` / `$N` / `$ARGUMENTS[N]` / 命名参数 / 技能目录占位符 / 会话标识占位符；值经 XML 转义；无占位符但有参数时追加 `ARGUMENTS:` 行。**插件技能前置**：插件声明的技能附加指令以 `<kimi-plugin-instructions>` 前缀块置于正文之前。

### 5.4 插件会话启动注入（`IAgentPluginService`）

仅主 agent（服务在非 main agent 上自钝化为空操作）。机制：

- 会话装配时经**上下文注入器**注册一次性变体（本会话已注入过则不再注入）；
- 技能目录的**插件源变更**触发追加式刷新：新的注入经系统提醒追加并声明取代更早同类注入；全部移除时追加**中性化声明**——失效必声明，防过期指令残留（上下文记忆判定旧注入/压缩摘要存在才需要中性化）；
- 技能解析失败（声明的启动技能不存在）记警告跳过该插件。

### 5.5 用户消息内联注入（SDR-005 / US3.6）

**术语映射**（本节固定）：载体 = 行内链接（glossary SK7）；呈现 = 芯片；需求侧口语 = 内联引用块——三者指同一物的不同面。

**载体与判定**：引用单元是消息正文中的**行内链接**——链接文字以触发符前缀（`$`）+ 展示名，技能目标为 `SKILL.md` 的绝对路径，命令目标为 `command:<规范命令名>`。**激活判定按文本形态**：满足「触发符前缀 + 合法目标」的行内链接即具备激活资格（菜单插入是主产生路径，手写/粘贴的等形态文本同样激活——纯文本载体上两者不可区分，该判定是唯一可实现且可预期的规则）；普通 markdown 链接（无触发符前缀）永不激活。

**解析器（纯函数，单源共享）**：`parseInlineReferences(text)` 为纯函数，代码归属**共享层**（`api-shared` 包）——web 客户端与 v2 引擎同源导入（web 不直接依赖 agent-core 的边界因此不破）。输出 `{ 正文（引用处替换为技能名/命令名标记）、有序引用列表（kind / 名 / 目标 / span）}`。解析纪律：**路径为解析权威**（链接指向哪个技能文件就激活哪个副本，同名折叠键 = 解析后技能实例）；同名不同路径不折叠；路径失效走失败语义。失败引用保留原文（普通链接文本）并携带可定位失败说明。**服务端校验为准**：客户端解析仅作前置体验（芯片渲染、发送前警示），最终激活与否以服务端解析结果为准，两者不一致时以提交结果回传呈现。激活 id 幂等：由消息 id + 引用序号派生（同一消息重试不产生重复激活事实）。

**注入管线（turn 消息序列模型）**：混合消息被接纳进回合时（忙时整条按普通消息排队，不并入运行中回合），经既有技能激活的**物化先例路径**（与 §5.1 同构：入队时校验、物化时渲染——渲染发生在回合真实启动时刻，会话标识占位符取真实值，排队期间技能状态变化如实反映）：一个 turn 的 prompt 阶段为**消息序列**——N 条激活包裹消息（按引用出现顺序，每条独立携带激活 origin 与激活事实，trigger = `user-inline`）+ 1 条正文消息；序列同属一个 turn（turnSeed 输入取正文），回放去重按每条激活事实的激活 id（幂等派生）各自独立成立。触发方式枚举扩展为四态（`user-slash` / `user-inline` / `model-tool` / `nested-skill`）。

**命令引用**：不进入模型上下文——由提交端（客户端界面）在消息发送时刻先行分发到命令执行面（响应契约：提交结果携带待执行命令清单，程序化消费者无 UI 时命令清单仅呈现不代执行）；清理类命令与技能混排的冲突警示归客户端发送前校验。引擎不代理命令执行。

**消费端分工**：web 输入框以行内链接为**唯一文本事实源**——芯片是覆盖渲染层（基于链接 span 定位）；触发判定为光标词首 + `/` 或技能触发符；芯片删除以链接整体为单位为主路径，用户手动编辑底层文本（光标进入、删触发符）则芯片就地降级为普通链接文本（激活资格随之消失，不报错）；发送前警示**只校验用户经输入框插入的引用芯片（结构化节点），不对序列化文本跑引用扫描**——普通文本（粘贴内容、类引用形态文本）永不触发警示（ADR-009）。TUI 侧暂不实现输入体验，仅保证历史回放降级（链接按普通文本渲染，不报错）。

## 6. 插件系统

### 6.1 安装管线（`PluginManager`）

来源解析三态：本地绝对路径 / http(s) zip / GitHub。GitHub 解析刻意绕开 `api.github.com`（匿名配额共享，避免被无关消耗打爆）：显式 ref 直接走 codeload；裸仓库经 `releases/latest` 的手动重定向提取 tag，404 或无 release 回退默认分支 HEAD；ref 百分号编码防注入。

流程：解析来源 →（本地）realpath 校验 /（远程）下载（超时上限）+ 解压（防路径穿越、恢复权限位、单层包装自动下钻插件根）→ manifest 解析（失败即止）→ 复制到托管根（staging + rename 原子替换；新副本成功后清理旧副本，失败回滚）→ 二次解析 manifest → 生成记录（**保留旧记录的启用状态、安装时间、能力偏好**）→ 原子持久化。

**更新即重装**：同 id 安装覆盖托管目录；GitHub 来源记录安装时的 commit SHA（可解析到 ref 时），更新检查据此比较。

**移除只删记录**：磁盘托管副本保留（重装可原地恢复）——移除是可逆的登记操作，不是文件删除。

### 6.2 manifest 解析（诊断模型）

候选文件优先级链（产品名 → 旧产品名 → 生态兼容名）；根文件形态优先于目录形态，被遮蔽者记录为诊断。校验**以诊断表达而非异常**：error 级 → 插件进入 error 态（贡献面全部不生效）；warn/info 级 → 照常加载。路径类字段（skills / commands / agents / mcp stdio 相对路径 / cwd）经 realpath 后必须落在插件根内。

技能贡献的缺省行为：未声明 `skills` 且插件根有 `SKILL.md` → 插件根整体作为单技能根。

### 6.3 持久化与运行记录（`installed.json`）

数据根下的 `plugins/installed.json`（版本化记录文件，原子写：临时文件 + rename；读取失败显式报加载失败而非静默清空）。每条记录：id、托管根、来源、启用状态、时间戳、能力偏好（每 MCP 服务器的启停与工具过滤）、GitHub 元数据。reload 重读该文件 + 全部 manifest 物化，报告新增/移除/错误。

### 6.4 贡献面（运行时读取口）

启用的 ok 态插件向各消费方暴露：

- **技能根**（`pluginSkillRoots`）：manifest skills 目录列表，供插件技能源发现；插件上下文（id + 附加指令）随根传递；贡献以 `extra` 来源标注册；
- **会话启动**（`enabledSessionStarts`）：`sessionStart.skill` 名单；
- **系统提示词段**（`enabledSystemPrompts`，主引擎较旧引擎新增的贡献面）：`systemPrompt` 内容并入主 agent 提示词（非特权段）；
- **MCP 服务器**（`enabledMcpServers`）：按 `plugin-<id>:<name>` 运行名注册（id 字符集不含 `:` 保证无歧义）；stdio 命令的 `./` 相对路径注入插件根，单二进制构建下 `node` 命令回退到自带解释器入口；
- **hooks**（`enabledHooks`）：cwd=插件根 + 两个环境变量（产品主目录、插件根）；
- **命令**（`enabledCommands`）：`.md` 命令文件（frontmatter name/description、正文仅支持完整参数串占位与无占位符追加两条规则——技能占位符族的其余成员不适用于命令），名字按相对路径派生；
- **角色目录**（`pluginAgentRoots`，主引擎较旧引擎新增的贡献面）：manifest agents 声明或约定 `agents/` 目录，供角色装配侧消费。

### 6.5 marketplace（客户端侧）

目录是**宿主端（TUI）职责**而非引擎职责：一个 JSON 清单（插件条目数组，每条 id + source + 可选元数据与分级），默认远程地址、可被环境变量/命令参数覆盖（http/https、file://、本地路径均可）、默认地址失败时回退随产品分发的本地目录。版本获取：条目未声明版本时，从 GitHub URL 形状推导（release tag / tree ref）；裸仓库查最新 release（同样绕开 API 配额）。更新判定：双方均为合法语义化版本且 latest > local 才报更新；本地版本未知时报「未知」而非误报最新。信任徽章按安装源地址分级（官方 / 精选 / 第三方），第三方安装前置确认且默认取消。

## 7. 消费端桥

### 7.1 REST（v1 wire 兼容）

三条路由（会话清单 / 工作区清单 / 会话激活），关键裁决：

- **会话清单**走会话作用域目录（每会话清单，项目技能按会话 cwd 发现——因此挂在会话路径下而非全局集合）；
- **工作区清单**为**边缘侧组合**：不建会话，直接用共享原语（发现后端 + 根解析 + 内存注册表 + 源优先级常量）组合出「同 cwd 新会话将看到的清单」——这是 §3.1 独立函数导出的动机；
- **激活**经 `enqueueDeferred`：立即答受理（含排队/运行状态），不挂起 HTTP 等待回合（运行中回合可能长于客户端超时）；校验失败仍同步返回；随后补会话标题元数据（首条技能消息命名会话，与本地路径行为一致）。**普通提交入口**含内联引用解析（ADR-004）：提交体正文中的技能引用链接被解析（共享层纯函数），合法引用在正文前注入激活包裹（同回合消息序列、trigger `user-inline`）；命令引用**不代执行**——提交响应携带待执行命令清单，由客户端自行分发（程序化消费者无 UI 时清单仅呈现）。失败引用保留原文并携带失败说明（错误语义与本路由一致）。

投影：引擎技能定义 → 协议描述符仅携带名称/描述/路径/来源 + 可选类型与禁用模型调用标记（子技能标记刻意不外发）。错误映射：未找到/名称空 → 技能未找到码；不可激活 → 另一码；未知会话 → 会话未找到码（区分「未激活」与「不存在」两种文案）；无动作后缀 → 校验失败码。

### 7.2 RPC / SDK

技能清单与激活在 RPC 面各为一个方法（`skill.list` / `agent.activateSkill`）；直连实现（不经服务端转发）分别落到会话目录与激活服务（后者附带标题元数据更新）。

### 7.3 TUI

- **斜杠命令构建**：会话技能清单 → 命令数组（内置与子技能裸名、其余 `skill:` 前缀）+ 名字映射；过滤用户可激活类型；内置优先排序；会话切换/重载/新建时刷新（失败静默保留旧清单）；
- **命令解析**：内置 > 技能 > 插件命令 > 普通消息的优先级；技能名解析支持省略 `skill:` 前缀（先查原名再查前缀名）；流式/压缩中拦截；
- **激活卡片**：事件去重按激活 id；媒体占位符重写（激活属性渲染时 XML 转义，媒体需先降为纯文本占位）；
- **插件面板**：marketplace 加载与信任确认、启停/移除/MCP 管理、重载报告。

### 7.4 Web

斜杠菜单（命令/技能两组渲染、技能条目以统一技能图标为前缀、选中保留输入框以追加参数）；技能触发符（`$` 及全角等价）唤起纯技能候选（与 `@` 文件提及菜单同构的触发门控），触发位置为光标词首（首位或紧随空白之后），选中后依输入框状态回填——为空时回填规范斜杠命令、已有内容时在光标处插入技能引用链接的芯片（SDR-005/ADR-004）；消息内联引用（芯片）作为输入与呈现的覆盖渲染层（技能芯片用技能图标、命令芯片用命令视觉标识），行内链接为唯一文本事实源，发送时空格收敛为消息文本归一化（共享解析器入口统一处理）；技能清单按会话或工作区懒加载（无会话时预热）；激活为乐观插入（事件流明确不重投影，本地插入为准）；排队场景呈现暂驻消息；设置页含技能管理面板（逐技能启停、技能查看、额外目录编辑）与合并开关；工具标签编辑器将 Skill 工具归入技能组。

## 8. 变更传播与一致性

- **来源级重扫**：工作区技能目录订阅每个源的 `onDidChange`——项目根技能目录经宿主文件系统**监视**（含尚不存在的候选目录，防抖毫秒级）；配置段（额外目录、合并开关）变更触发对应文件源；插件重载触发插件源。任一源变化 → 该源重载（按源串行化，防并发竞态）→ 重合并 → 通知订阅方（携带源 id）。
- **永不全量重扫**：变更后的扫描只针对受影响来源；「重扫所有源」只发生在构建时的一次性加载。
- **会话一致性**：会话目录经种子快照折叠；种子变更事件转发源 id；会话恢复时种子已就绪后重折叠，保证恢复会话与同 cwd 新会话清单一致。
- **生效边界**：插件与技能目录变更对**新回合/新会话**生效（系统提示词在回合边界重建）；当前运行中的回合不受影响——这是「变更需重载/新会话」提示的机制根源。

## 9. 错误与安全模型汇总

| 面 | 机制 |
|---|---|
| 技能解析容错 | 单技能失败 → 跳过 + 诊断分级（按策略跳过清单 / 警告日志），扫描不中断 |
| 模型调用门控 | 三道硬门在工具执行体内部（未找到 / 禁模型调用 / 非 inline），错误即工具错误结果，无内容注入 |
| 嵌套递归 | 深度计数 + 上限拒绝（防技能互引烧穿预算） |
| 下载与解压 | 超时上限；路径穿越防护（resolve 后必须落在解压目标内）；权限位恢复 |
| manifest 校验 | realpath 越界即错误诊断；error 级隔离到单插件（不炸其他插件/会话） |
| 安装原子性 | 托管副本 staging + rename；失败回滚；记录文件原子写 |
| 信任门 | 第三方来源安装前置确认（默认取消）；安装过程零代码执行 |
| 非特权注入 | 插件系统提示词段与插件指令被显式声明为非特权通道（不得凌驾系统/用户指令）；插件技能附加指令为内容前缀而非指令通道 |

## 10. 决策记录（ADR）

### ADR-001 · 2026-10-09 · 技能引用的插件限定名解析

- **背景**：插件技能文档以 `插件id:技能名` 形态交叉引用（生态惯例，与插件命令 `/插件id:命令名` 同构，见 US6.6）；调用链此前仅裸名精确匹配，限定名引用全部失败（SDR-002）。
- **决策**：统一引用解析次序——① 裸名按**各引擎既有匹配语义**精确匹配（TS 为归一匹配、Rust 为区分大小写的精确匹配——大小写行为差异属既有跨引擎分叉，另行裁决，本次不动）；② 未命中且名称含 `:` 时按**首个** `:` 切分为（插件 id, 技能名），查目录的插件索引（TS `byPluginAndName` / Rust `get_plugin_skill`）。插件 id 字符集（小写字母/数字/连字符/下划线，≤64）不含 `:`，首个冒号切分无歧义；**插件 id 段按归一（小写）匹配**，与 US6.6 的 id 归一语义一致；限定名的技能名段大小写同样沿用各引擎既有匹配语义（TS 归一 / Rust 精确），与上述裸名大小写分叉同族，不另立规则。两个入口一致适用：模型侧 Skill 工具执行体、用户激活校验（含 REST / RPC 路径）。解析成功后一切下游（门控、渲染、事实、事件、呈现）使用技能的正式裸名；错误路径与审批 glob 匹配回显/使用原始输入名。模型面工具描述文案保持裸名导向（清单拼写），限定名为容错接受形态，不改描述文案。
- **遮蔽语义**：限定名指向该插件自己的技能副本——即使该副本被更高来源（或另一插件）的同名技能遮蔽，限定名仍解析到插件副本（TS 侧插件副索引天然支持）。Rust 目录为单映射，被遮蔽副本不保留，限定名只能取到胜者注册——记为已知分叉，另行裁决；本次不修。
- **被否决的备选方案**：
  1. 清单主名改限定名——破坏全部存量裸名调用与各端呈现，且内置/用户/项目技能无命名空间。
  2. 前缀非插件时剥前缀按裸名回退——掩盖拼写错误，且可能静默命中无关技能（语义不可预期）。
- **影响范围**：本域 §5.2 执行体（名称解析步）与 US4.2 / US3.3；tool-calling 域 §3.4 Skill 工具节（名称解析说明）；实现面为两引擎共四处调用点（v2 模型工具执行体、v2 用户激活校验、Rust SkillTool、Rust 会话激活处理器）。

### ADR-002 · 2026-10-12 · 技能专用触发符与统一技能图标

- **背景**：SDR-003。现状技能条目名携带 `skill:` 文本前缀参与菜单渲染与过滤匹配——渲染层面可辨识但不可直达；匹配层面用户输入（如 `de`）拼不出前缀就拿不到前缀分，只能靠子串命中；技能在菜单、设置、管理页无统一视觉标识。
- **决策**：
  - **触发符集**：`$`（U+0024）及其全角等价 `¥`（U+00A5）、`￥`（U+FFE5）——中文输入法 Shift+4 直接产出全角形态，免切英文态；触发判定与 `/` 同位（仅输入串首、尚未出现空格）。
  - **菜单数据面**：技能触发符菜单复用会话技能清单（懒加载/预热不变），候选集合 = 用户可调用类型过滤 + 用户级停用过滤（与斜杠菜单技能组同一集合）；不并入内置命令与插件命令。
  - **选中回填**：回填为规范斜杠命令文本（内置源技能 `/名 `、其余 `/skill:名 `）并置光标于末尾——与斜杠菜单 `acceptsInput` 选中路径同构；提交侧不新增 `$` 解析分支（`$` 开头文本一律走普通消息）。
  - **呈现/语法分层**：`skill:` 前缀保留为命令命名与解析语法；菜单渲染层技能条目改「统一技能图标 + 裸名」，过滤匹配键 = 技能裸名（剥前缀打分，前缀分回归）。
  - **统一技能图标**：单一字形资产，从网页既有图标集（tabler / remix collections）选定一枚，各表面（两种触发菜单、设置与管理界面技能条目与区块、激活呈现）引用同一来源；终端以其渲染能力呈现同形字形。字形定为网页图标寄存器既有 `bolt`（RiFlashlightLine，`lib/icons.ts`；`toolMeta` 已用作技能工具 glyph）——**2026-10-17 由 ADR-005 修订为专属技能字形，本条以 ADR-005 为准**。
  - **空描述占位**：技能描述为空或纯符号残留时显示中性占位文案（i18n）。
- **被否决的备选方案**：`$` 执行语法（见 SDR-003）；过滤匹配沿用带前缀名（前缀分丢失）；各端自选图标（认知分裂）。
- **影响范围**：web `slashCommands.ts` / `useSlashMenu.ts` / `SlashMenu.vue` / `Composer.vue`（触发符门控与按键编排）、设置面板（SkillsPanel、插件详情、ToolTagEditor）、激活呈现（ChatPane 激活消息）；TUI 补全面对齐；需求 US3.4 / US3.5。

### ADR-003 · 2026-10-12 · 斜杠菜单分组

- **背景**：SDR-004。现状菜单为无分组平铺清单（内置命令在前、技能条目追加其后），数量无界，命令可达性被技能稀释。
- **决策**：菜单数据模型分两组——**命令组**（内置命令 + 插件命令，既有顺序）与**技能组**（技能条目，内置源优先）；渲染层带组标题，组序固定（命令组在前）；无过滤词时两组各自全量展示；有过滤词时既有打分（exact > prefix > substring，技能侧按 ADR-002 以裸名为键）在组内过滤排序，组序不变；不做折叠与数量上限。
- **被否决的备选方案**：技能组折叠（与触发符分流冗余、多一步交互）；`/` 仅出命令（破坏统一入口）；跨组混合排序（违背命令优先的固定组序）。
- **影响范围**：web `slashCommands.ts`（条目分组结构）、`SlashMenu.vue`（分组渲染与跨组连续键盘导航）；TUI 补全面对齐；测试面（分组渲染、过滤归并、触发符、空描述占位）。

### ADR-004 · 2026-10-13 · 消息内联多技能 / 多命令注入

- **背景**：SDR-005 / US3.6。现状技能与命令触发被限制为「整条消息即一条命令」（首位触发、选中回填命令文本、发送即拦截执行）——用户无法在一条自然语言消息中混排多个技能引用，也无法在消息中间插入引用；多技能协作只能拆成多条纯命令消息或依赖模型自觉调 Skill 工具（不可靠）。
- **决策**：
  1. **载体 = 行内链接（glossary SK7）**：消息正文自身是唯一事实源，不新增协议字段。链接文字为 `触发符 + 展示名`（`[$brandkit](…/SKILL.md)`），命令引用目标为 `command:<规范命令名>` 形态的标记链接。**激活判定按文本形态**：满足「触发符前缀 + 合法目标」的行内链接即具备激活资格——菜单插入是主产生路径，手写/粘贴的等形态文本同样激活（纯文本载体上插入与手写不可区分，这是唯一可实现且可预期的判定规则；误触发面由双重条件收窄，属接受的设计残余）。普通 markdown 链接（无触发符前缀）永不激活。
  2. **解析器为纯函数、单源共享**（`parseInlineReferences`）：代码归属 `api-shared` 共享层，web 客户端与 v2 引擎同源导入（web 不得依赖 agent-core 的边界不破，双端不会漂移出两份实现）。解析纪律：**路径为解析权威**（折叠键 = 解析后技能实例，同名不同路径不折叠，路径失效走失败语义）；失败引用保留原文并携带可定位失败说明。**服务端校验为准**：客户端解析仅作前置体验，最终以服务端解析结果为准，不一致时经提交结果回传呈现。激活 id 由消息 id + 引用序号派生（重试幂等）。
  3. **turn 消息序列模型**：混合消息被接纳进回合时，一个 turn 的 prompt 阶段为消息序列——N 条激活包裹消息（按出现顺序、每条独立 origin 与激活事实）+ 1 条正文消息，序列同属一个 turn（turnSeed 取正文）。注入走既有技能激活的物化先例路径（与 §5.1 同构：入队时校验、**物化时渲染**——排队期间技能状态变化如实反映，不做提交时刻快照）；忙时整条按普通消息排队，不并入运行中回合。触发方式枚举扩四态（`user-inline` 为新成员）；一条消息内的多激活构成一个撤销单元。
  4. **命令引用由提交端分发**：命令块不进模型上下文，由客户端在发送时刻先行分发到既有命令执行面（提交响应携带待执行命令清单；程序化 REST 消费者无 UI 时清单仅呈现不代执行）；清理类命令与技能混排的冲突警示归客户端发送前校验；引擎不代理命令执行。
  5. **web 输入框以覆盖渲染实现芯片**（纯文本控件，不引入富文本编辑器）：行内链接是唯一事实源，芯片基于链接 span 定位渲染；触发判定扩为光标词首，词内不触发；芯片删除以链接整体为单位为主路径，手动编辑底层文本则芯片就地降级为普通链接文本（激活资格随之消失）。
  6. **TUI 只承诺降级**：历史回放把链接按普通文本渲染（不报错、不中断）；输入体验对齐属终端域后续裁决。
- **被否决的备选方案**：
  1. contenteditable / 富文本编辑器承载芯片——重构输入框控件风险大（提及菜单、斜杠菜单、多行编辑、IME 全部要重验），且芯片只是覆盖渲染层的事实即可成立，不值得。
  2. 提交协议新增结构化引用字段（2-B）——改 wire 契约、历史芯片呈现依赖结构化数据（脱离 web 端丢语义）；SDR-005 已裁决行内链接为唯一载体。
  3. 引擎侧命令执行代理（引擎解析命令引用并代执行）——命令语义归各命令自身域（/clear 走客户端、/plan 走模式切换），引擎统一代理会造成双头执行语义。
  4. 链接文字不带触发符前缀（纯 `[$名](路径)` 即激活）——用户粘贴任何含技能路径链接的文档都会意外激活，双重收窄条件失去一半。
  5. 「仅菜单插入才激活」（以来源区分手写与插入）——纯文本载体上两者不可区分，需引入隐藏标记（破坏「正文即事实源」），且用户粘贴自己历史消息中的合法引用无法激活。
  6. 注入点走 `onBeforeSubmitPrompt` 钩子——该钩子语义是黑名单式阻断与提示改写，非物化职责；StepRequest 物化路径与技能激活先例（§5.1 出队物化）同构，排队/占位符取值语义直接继承，故选物化路径。
  7. 非链接载体（纯 token 形态，类似 `@` 提及）——正文可读性差（无路径可跳转）、降级呈现损失大（其他客户端只剩裸 token）。
- **影响范围**：本域 §5 激活入口表（新增用户消息内联行）、§5.5（新节）、§7.1（REST 提交入口解析）、§7.4（web 消费端）；需求 US3.6 / SDR-005；glossary SK7 / SK6 / SK2；实现面为 `api-shared`（解析器）、web（`slashCommands.ts` 触发判定、`useSlashMenu` / `useSkillTriggerMenu` 词首触发、`Composer.vue` 芯片覆盖渲染与提交校验、`ChatPane.vue` 用户消息呈现、`messagesToTurns` 标题正文规则）、v2 引擎（提交入口物化路径、turn 消息序列、`contextMemory/types.ts` trigger 枚举、`skill.activate` 操作族复用）；Rust 侧 trigger 字段为开放字符串（`origin.rs`），随实现批次同步取值。

### ADR-005 · 2026-10-17 · 技能图标专属字形（修订 ADR-002 字形裁决）

- **背景**：ADR-002 将统一技能字形定为图标寄存器既有的 `bolt`（RiFlashlightLine）——该选择是当时凑数的临时裁决：借用一枚通用功能图标，无技能专属语义，且 `bolt` 属通用「能量/加速」语汇，随图标库扩充存在被他处复用、认知稀释的风险。产品需要一枚**专属**于技能领域的字形来培养用户认知（见 US3.5 的动机）。
- **决策**：
  - **字形 = 魔杖 + 四芒星**（候选编号 b1「wand-sparkles」）：左下至右上的斜置魔杖，右上主星为四芒星，辅以两枚微星点。隐喻贴合「技能 = 用户向会话注入一次能力加持」的产品语义；四芒星是 AI 领域通用符号，与「模型 + 触发」的心智呼应。登记资产（24×24，stroke 1.5，round cap/join，`currentColor`）：

    ```svg
    <svg xmlns="http://www.w3.org/2000/svg" width="24" height="24" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"><path d="M3 21 10.5 13.5"></path><path d="M13 2.5 14.3 5.7 17.5 7 14.3 8.3 13 11.5 11.7 8.3 8.5 7l3.2-1.3Z"></path><path d="M19 13.5v3"></path><path d="M20.5 15h-3"></path><path d="M5.5 4.5v2"></path><path d="M6.5 5.5h-2"></path><path d="M18.5 5v1.6"></path><path d="M19.3 5.8h-1.6"></path></svg>
    ```

  - **资产形态 = 自有 SVG，落入网页端自定义图标集合**（`mirri` collection，与既有 Lucide 风格 panel 图标同目录同格式：24×24 网格、`stroke="currentColor"`、1.5 描边、round cap/join）——不依赖 remix/tabler 现成字形（评审的六个候选中无一为现成图标），保持「单一资产、各端引用」原则（US3.5）不变。
  - **注册形态 = 图标寄存器新增逻辑名 `skill`**（`lib/icons.ts`），所有技能表面一律引用逻辑名而非文件路径；旧逻辑名 `bolt` 保留（通用能量图标语义不变），技能相关的全部引用点切换到 `skill`。
  - **家族区分度约束**：技能字形必须与命令（terminal）、MCP（globe）、子代理（robot）的视觉形态可一眼区分——魔杖的斜置长线 + 星芒组合在四者中唯一。
- **被否决的备选方案**：
  1. 沿用 `bolt`——临时凑数字形无专属语义，认知培养目标落空（本次修订的动因）。
  2. 五角星魔杖（b2）/ 星 + 文本行魔杖（b3）——五角星与「评分/收藏」心智冲突；文本行在 14px 芯片场景下不可辨识。
  3. 拼图（c1）/ 翻开的书（c2）/ 堆叠层（c3）——「静态资产/文档」心智偏重，与「$ 触发注入」的交互语义弱；c3 与平台分层语义混淆。
  4. 从 remix/tabler 现成图标中另选——现成库内无「魔杖+星」且风格匹配者（remix 的 magic 系列为 fill 风格，与 mirri 自定义集 stroke 风格不一致）。
- **影响范围**：修订 ADR-002 字形裁决（其余决策不变）；需求 US3.5（无文字变更——其只约束「单一资产、各端同形」，字形本身授权架构文档登记）；实现面为 web 图标寄存器（新增 `skill` 条目）、全部技能表面引用点切换（两种触发菜单、SkillsPanel、工具标签编辑器、激活卡片、技能芯片）；TUI 侧按 US3.5 的既有承诺以其渲染能力呈现同形字形，随终端域批次对齐。



### ADR-006 · 2026-10-17 · 激活后正文芯片重建（with_skills 记录字段）

- **背景**：SDR-006 / US3.7。ADR-004 决策 2 规定引擎在接纳时把正文中的技能引用链接替换为裸名标记（解析幂等 + 模型侧可读），持久化与回放用的都是替换后正文；而消费端芯片渲染只认「触发符前缀 + 完整链接」形态（ADR-004 决策 1 的解析形态），激活成功的历史消息里引用因此退化为纯文本，与发送前呈现不一致。
- **决策**：
  1. **字段**：wire 消息 schema（`agent-core-v2` `contextMemory/protocolMessage` 的 `messageSchema`，REST `/messages` 读取路径复用）新增**顶层可选字段** `with_skills: Array<{ name: string; target: string; trigger: string }>`——由引擎在 `analyzeInlineReferences` 替换正文时写入被改写的正文消息（每项为折叠去重后的实际激活）。`name` 兼任渲染匹配键：正文裸名标记（`[name]`，来自引擎替换时写入的 display 文本）与清单项**技能名相等**即重建；`target`（技能文件路径）恢复重建芯片的点击跳转能力；`trigger` 为触发方式枚举值。字段为**纯产物记录**：不参与激活判定、不参与解析、不影响正文文本——「行内链接是唯一激活事实源」（ADR-004）不变。**投递路径**：写入后随消息持久化，经 REST 消息读取路径透出；会话内实时呈现不受影响（客户端乐观回显用发送前形态，本就含完整链接）；v2 总线不发布 `prompt.submitted`，事件投影不承载本字段。**匹配残余**：手写/降级编辑产生的 display ≠ name 的引用，其裸名标记按技能名匹配不上时逐标记降级为普通文本（不丢激活事实，仅呈现降级——菜单插入的主路径 display 恒等于 name，不受影响）。
  2. **渲染**：web 气泡渲染（`ChatPane` 的 `userInlineSegments`）在切分正文时优先消费 `with_skills`：字段存在时，把正文中与清单项**技能名相等**的裸名标记（`[name]`）重建为技能芯片段（含点击跳转）；字段缺失或标记匹配失败时逐标记独立降级为普通文本（现状行为）。重建只做呈现，不回写、不重发。
  3. **幂等与缓存**：`with_skills` 写入发生在接纳时刻（与正文替换同一事务点，即 `attachInlineAnalysis` 覆写 `record.message` 处），落库后不再变化；对模型的请求文本不受影响（字段不进模型上下文），prefix cache 行为与现状完全一致。
  4. **兼容**：旧消息无字段 → 现状降级；新字段对不识别的客户端透明（`.optional()` 保证解析宽容）；命令引用不写入本字段（命令不进模型上下文、其降级呈现由提交端响应契约管辖，见 ADR-004 决策 4）。
- **被否决的备选方案**：
  1. **降级保留 `$` 前缀**（正文标记为 `[$name]`，复用紧凑 token 解析）——改动最小，但把模型侧文本形态与客户端渲染协议耦死；且 SDR-006 评估时定位为应急补丁，与本决策的正解并存价值低。
  2. **字段挂消息 `metadata.origin`**——不新增 schema 字段、变更面小，但 origin 语义被扩宽（「来源」承载「结果清单」），schema 不自描述，多客户端发现成本高。
  3. **挂 `prompt.upsert` 实体而非消息**——prompt 是 web 投影层实体，REST 消息读取路径拿不到，双源消费需要 join；挂消息本体两条路径天然都可见。
- **影响范围**：修订 §5.5（消费端分工补一段重建依据）；wire schema（`protocolMessage.ts` `messageSchema`，`rest-message` 自动复用 + asyncapi 文档）；v2 引擎（`skillService.analyzeInlineReferences` 返回值增列、`promptService.attachInlineAnalysis` 写入 `record.message`、**`contextMemory/messageProjection.ts` 的 `toProtocolMessage` 显式投影新字段**、**`contextMemory/contextTranscript.ts` 的 `toMutableEntry` 回放保留新字段**——后两处是从零构造固定字段的映射点，不补则刷新/回放后字段静默丢失）；web（REST 消息读取消费、`messagesToTurns`/`ChatPane` 依据技能名匹配重建）；需求 SDR-006 / US3.7；Rust 侧消息协议随实现批次同步。

- **Rust 落地补充（2026-10-17 实现批次）**：rust 引擎（mirri-core / mirri-embedded / mirri-server）按本 ADR 落地 with_skills：
  1. **stamp 点 = 接纳时刻**：`mirri-embedded` 的 `message_handlers.rs` `maybe_inject_inline_skill_activations`（与 ADR-004 的行内引用解析/正文改写同一函数）在注入包裹消息之后、正文入内存之前，把折叠去重后的激活清单写入正文消息——**双点写入**：持久化 wire 行（`context.append_message` op 的 `with_skills` 字段，由 `dispatch_send_message` 在构造 `body_message` 时直接写入）与实时内存 `ContextMessage`（经 `TurnFlow::run_one_turn` 的 with_skills 参数 → `append_user_message_with_skills`）。**为何双点**：实时读面（`message.list` / `session.snapshot`）直接服务内存历史；若只写 journal 一条，实时会话读不到字段，冷恢复会话反而读到，live 与 restored 两侧读面必然分叉。两处共用同一份 `Vec<WithSkillsEntry>`，落库后不再变化（幂等）。
  2. **wire 形态**：顶层 `with_skills`，snake_case `[{name,target,trigger}]`——与 TS `messageSchema` 的 `with_skills` 数组逐字对齐（`WithSkillsEntry { name, target, trigger }`），`serde` 序列化为 `{"name":…,"target":…,"trigger":…}`；字段为可选 `Option`，缺失/空数组不输出键（`skip_serializing_if`），旧消息读取与回放兼容。`trigger` 取 `"user-inline"`（行内注入路径的唯一取值，slash 路径不写本字段）。
  3. **读面**：`mirri-server` 的共享行构建器 `message_list_row` 把 `with_skills` 投影为**顶层行键**（与 TS `toProtocolMessage` 相同——不挂 `metadata`）；`message.list` 与 `session.snapshot` 的 messages 页共用同一构建器，两读面同源不漂移。
  4. **模型边界**：字段只存在于 `ContextMessage` shell（`context.rs`），**结构性密封**——投影器 `strip_context_metadata` 只取出内层 `Message`（`msg.message.clone()`），`with_skills` 不进入模型请求上下文；prefix cache 与模型侧文本形态与 TS 侧一致（字段纯产物记录）。回放侧 `session/replay.rs` 的 `parse_context_message` 从 journal 行恢复 `with_skills`，缺字段/畸形字段降级为 `None`（旧日志与坏数据不炸）。


---

# ADR-007: Structured Skill References in Message Parts

- Status: Accepted (revised after architecture review)
- Date: 2026-10-18
- Deciders: user, agent
- Review: independent architecture review 2026-10-18 — 10 mandatory revisions applied; Q1 ruling overturned the draft lean

## Context

ADR-004 established inline skill references as markdown links `[$name](target)` riding in the message body text; ADR-006 added the `with_skills` product record stamped at admission, with the body rewritten to `[name]` markers. The web then rebuilds chips by **string-matching** marker text against the record's names.

Structural costs, evidenced in production:

1. **Dual-track rendering.** Live, reloaded, optimistic, and queued rendering all depend on matching `[name]` text against `with_skills` entries — name equality, per-marker degradation, no positional guarantee. Any drift silently degrades to plain text.
2. **The body is mutated for rendering's sake.** Stored message text carries `[name]` markers — a presentation artifact — because the render pipeline needed an anchor. Storage, model input, and display are coupled through text surgery.
3. **Known gaps ride the same rails.** `builtin://` targets are not accepted by either parser (TS protocol + rust engine), so builtin-skill chips never activate; the pre-submit normalizer re-expands their link heads, producing duplicated `(builtin://…)(builtin://…)` wire text.
4. **Multiple re-implementations of one grammar.** Trigger chars, link formatting, and the builtin prefix are hand-copied across TS protocol, web libs, and rust (with one import already proving the better pattern).

User design directive (2026-10-18): do not rewrite the body; remove the skill portion from the text and carry it as structured data; real-time and reload rendering consume the same front-end data source and render logic — consistency by construction; the `[name]` marker as a storage-layer artifact must not exist.

The document model this adopts is the same one the web composer already uses internally (TipTap atomic inline `skillMention` nodes): ADR-007 promotes the editor's document model to the storage/wire model.

## Decision

### D1. Skill references are structured content parts

A user message carrying references is stored as **ordered content parts**:

```json
{
  "role": "user",
  "content": [
    { "type": "text", "text": "记得计划完 " },
    { "type": "skill_ref", "name": "writing-plans", "target": "<SKILL.md path | builtin://name>" },
    { "type": "text", "text": "，然后 " },
    { "type": "skill_ref", "name": "executing-plans", "target": "…" },
    { "type": "text", "text": " 再开始" }
  ]
}
```

- `skill_ref` is an inline atomic part; ordering relative to text parts expresses position. **No offsets**: the journal is append-only and there is no message-edit op — undo is truncate-only (`ContextUndo { count }`), compaction drops whole messages or rewrites tool messages, never splits a message internally. Ordering is stable by construction.
- `target` keeps the single-address authority (ADR-004): SKILL.md path (disk) or `builtin://<name>` (builtin, D4). Inline command references take the sibling shape `{ type: "command_ref", name, target: "command:/x" }` (R3) — one part family, one grammar.

**ALL references become parts — including fail-soft ones.** Storage reflects what the user sent, not what the engine activated: an unresolvable or disabled skill still becomes a `skill_ref` part (its target is whatever the user's link carried). Activation state lives in the wrap messages and events (the audit chain), never in the part — the part carries no `status`/`trigger`/snapshot fields. Renderers that need to distinguish dangling references resolve against the live catalog at render time.

### D2. The top-level `with_skills` record is retired

`skill_ref` parts are the single rendering data source. The ADR-006 field and its two-site stamp are superseded. Activation-at-admission and wrap-injection semantics carry forward unchanged; the activation audit chain is and remains the wrap messages (`PromptOrigin::SkillActivation`) plus `SessionEvent::SkillActivated` — the record was a rendering anchor, not the audit.

`with_skills` in agent-core-v2 (TS engine, maintenance-frozen) keeps being written; the web's legacy read path for v2-engine sessions is scoped by the v2 engine's retirement, not by this ADR.

### D3. Projections, not rewrites — at the correct boundary

The stored parts are never mutated for a consumer. Consumers:

- **Model projection — at the model boundary (projector), not admission.** The stored `ContextMessage` carries parts into `ContextMemory`; the request text is assembled where requests are built (after `strip_context_metadata`). Rule: adjacent text parts concatenate with **no separator**; each `skill_ref` projects in place to the model token (Q1 ruling: `[skill: name]`). The known join sites that must be audited because they assume text-only content: rust `Message::text_content()` (joins parts with `\n` today) and the web's `textParts.join('\n')` — the projection spec supersedes both for request assembly.
- **UI projection (web):** `skill_ref` part → chip. The same parts array serves live events, `message.list`, `session.snapshot`, the optimistic message, and the queued prompt. One source, one render path.
- **Plain-text ecosystems:** the wire text `[$name](target)` ↔ parts conversion (existing bidirectional converters) relocates from submit-time to the boundaries that need text: history recall, queue edit-back, clipboard.

**Mixed-media messages: the single-text-part admission gate is lifted.** In the parts model, admission parses and segments each text part independently; a message carrying images plus referenced skills activates its references. (Today any non-single-text prompt skips inline injection entirely — an accidental limitation this ADR removes deliberately.)

### D4. `builtin://` is a first-class target (prerequisite, lands first)

Both parsers accept the `builtin://` prefix; the rust catalog resolves `builtin://<name>` against the builtin table by name. Stale persisted targets in the historical dirty form `builtin://<name>/SKILL.md` normalize by suffix-strip (the web's `normalizeBuiltinTarget` already handles this — the parsers adopt the same rule). This fix is independent of D1–D3 and precedes them.

### D5. One grammar, one authority, shared golden vectors

- `@mirri-ai/protocol` exports the grammar (trigger chars, prefixes, link format, part shape) — the web's hand-rolled copies are deleted.
- `rust/crates/mirri-core` mirrors it; equivalence is pinned by **shared golden fixtures**: one JSON corpus (link text ↔ parts, covering full-width triggers, escaped brackets, builtin targets, fail-soft references, `command_ref` cases, and multi-reference edges) consumed by both the TS vitest suite and the rust tests.
- Schema code-generation is rejected as over-engineering at this grammar's size.

### D6. Ingestion stays text; legacy is read-only with explicit exit criteria

**Submission wire form remains plain text** (full links). The engine's admission — where parsing already happens — segments text into parts for storage. The optimistic message constructs parts locally via the shared parser. Zero changes to the submit API, the queue (which keeps serialized text and converts at edit-back), history, and third-party REST consumers.

**Legacy formats are frozen read-only:** old messages (raw links; `[name]` markers + `with_skills`) render through the existing degradation chain. No journal migration. Old web builds parsing new messages: the v2 zod `messageContentSchema` is a discriminated union that would reject `skill_ref` parts — the failure granularity (dropped message vs dropped list) must be verified during implementation and the atomic-release claim adjusted to the verified reality.

**Exit criteria for the web legacy path** (all required, none waivable):
1. The rust read faces produce parts-only output for ≥ 2 consecutive minor releases (pinned by golden fixtures + engine assertions).
2. The last parts-unaware web build is outside the support window (desktop auto-update coverage, ≥ 2 minor versions).
3. Telemetry: new messages erroneously entering the legacy path = 0 for 4 consecutive weeks (old-message render hits are expected and excluded).
4. The v2-engine session scope ends with the v2 engine's retirement — no unbounded generic fallback.
5. **User accepts the trade-off**: deleting the legacy path means old sessions render references as plain text (no journal migration). Recorded here as the decision.

## Rulings (formerly open questions — all decided)

- **R1 — Model token: `[skill: name]`.** The draft's "corpus continuity" argument is rejected: runtime history is not training data, and D6's format freeze makes mixed-corpus sessions inevitable regardless — continuity protects nothing. Selection criteria: self-describing, collision-free with prose. `[skill: name]` matches the established `command:` pseudo-target and `<kimi-skill-loaded>` conventions and gives the model a machine-checkable anchor to correlate with the wrap.
- **R2 — No per-part `trigger`.** Trigger belongs to the activation event (origin/wrap layer). No `status`/`resolved` snapshot fields either — they go stale the moment catalog state changes.
- **R3 — `command_ref` unified now.** Inline command references become `command_ref` parts in the same shape. Their model projection is defined in the same rule as R1 (command token), not deferred — grammar unity cannot leave the projection undefined.
- **R4 — `with_skills` read-only + atomic release.** The rust engine stops writing it; web and engine ship in the same release sequence; the zod failure-mode verification (D6) gates the atomicity claim.

## Alternatives Considered

- **A. Status quo (markers + record matching, ADR-006):** entrenches the dual-track the user rejected.
- **B. Markers + offset-bearing record:** exact positions, but still mutates the body and keeps two sources of truth.
- **C. Parts (chosen).**
- **D. Keep full links in stored text, render by parsing:** zero wire change, solves consistency, and mirrors the Discord/Slack token-in-text pattern — but those protocols embed stable IDs (`<@123>`) that never collide with prose, while our links carry absolute filesystem paths into the model context (noise, cost, information leakage), every consumer maintains a parser, and the grammar can never evolve. Also contrary to the user directive. Rejected.

## Consequences

- Rendering consistency is structural; the admission flow simplifies (no body rewrite, no marker bookkeeping, no two-site stamp).
- Model-facing token changes from `[name]` to `[skill: name]` for new messages — a one-time semantic change affecting only reference-carrying messages' prompt-cache segments.
- Mixed-media messages gain inline activation (deliberate lift of the single-text gate).
- Transition carries two web render paths with the explicit exit criteria above; v2-engine sessions keep the legacy path until the v2 engine retires.
- Wire schema grows part types → minor bump via changeset.

## Relation

- Supersedes ADR-006's storage/stamp design (activation-at-admission and wrap injection carry forward).
- Depends on the `builtin://` classification fix (D4), landing first and independently.
- Extends ADR-004's single-address authority.

---

### ADR-008 · 2026-10-23 · 系统提示词的技能加载强制条款（清单 ≠ 手册）

- **背景**：生产事故复盘——一项需要某 CLI 工具 skill 的任务中，模型读了清单里的技能**描述**（"该 CLI 用于拉取请求"）后跳过 Skill 工具加载，凭描述级知识直接执行底层 CLI；该 skill 文档里的关键机制（多认证实例下的 context 路由）从未被读到，导致连续 6 轮对错实例试错、臆造不存在的参数、被误导性报错带偏。根因是模型把「我读了描述」当成「我会用」。既有强制规则（using-superpowers 的 "invoke skills BEFORE any response"）不足以阻止，因为描述**感觉上**够用了。
- **决策**：基础系统提示词模板（rust `system_prompt.md` 与 TS 镜像 `agent-core-v2` `system.md`）在 `{{skills_section}}{{plugin_sections}}` 槽位之后、`# Ultimate Reminders` 之前新增固定 `# Skill Use` 段，三部分：
  1. **认知锚点**（routing signal ≠ usage knowledge）：描述只告诉模型 *when*，加载才给 *how*；任务命中描述即 MUST 先经 Skill 工具加载全文，仅凭描述直接执行底层工具构成违规；skill 的存在本身就证明其使用知识（context 选择、环境切换、多实例路由、陷阱）不在训练数据也不在工具报错里——有 skill 的工具不是"你已会的常见工具"。
  2. **两败升级规则**：同一操作失败两次即 STOP 换参数，回到 skill 文档找漏掉的机制（context 选择、环境切换、多实例路由），而非继续试错。
  3. **果断性调和**：明确「果断行动 / 简单请求直接调工具」与强制加载不冲突——加载匹配的 skill 本身就是一次果断执行的动作，加载完照常无仪式推进。这条消除 Ultimate Reminders 语气与加载克制之间的结构性张力。
- **放置理由**：放在清单槽位之后，因为它直接评述紧邻其上的清单（描述只是路由信号），模型读完清单立即读到该约束，因果链最短；放在 Ultimate Reminders 之前、与 "act decisively" 相邻，使调和句在被读到之前不会先经历两个"冲突"规则的间隔。不放入 TS `SKILLS_SECTION_PROSE`（清单伴随文字）有二：其一该文字**仅在清单非空时**注入，而两败升级规则与果断性调和对空清单会话同样成立（无清单时无违规面，但规则本身不依赖清单存在）；其二 rust 侧按已披露分歧不渲染 `# Skills` 包裹，改包裹文字会加重两侧形态漂移，固定段则天然镜像。
- **被否决的备选方案**：
  1. **清单条目加 "MUST load before use" 后缀**——机械可靠但所有条目都加等于噪声，且清单是渲染产物（`render_skills_section` / `getModelSkillListing`），改动散落渲染层，超出提示词层修复范围。
  2. **显式仪式输出（"Using [skill] for [purpose]"）**——把加载动作显性化便于审计，但每次多一步仪式输出，简单场景繁琐；两败规则 + 锚点已把跳过定为可判定的违规，无需再借仪式观测。
  3. **仅强化 using-superpowers 插件规则**——插件会话启动注入不是所有会话都有（依赖插件安装），且其"BEFORE any response"的时序性规则没有针对"描述已读但手册未读"这一具体失败推理链，事故证明无效。
- **影响范围**：`rust/crates/mirri-core/src/scope/system_prompt.md`（生效面，产品跑 rust 引擎）；`packages/agent-core-v2/src/app/agentProfileCatalog/system.md`（冻结镜像，按模块头声明同步）；规格测试 `should_carry_the_skill_use_contract_between_the_skills_slot_and_the_reminders`（断言锚点、两败规则、调和句存在及放置顺序）。文案无环境特定信息（无工具名、主机名、项目名），通用于全部 skill（CLI 类、研究类、记忆类、文档类）。

### ADR-009 · 2026-10-23 · 发送前警示以结构化芯片节点为校验对象

- **背景**：SDR-007。ADR-004 决策 5 时期网页输入框以「纯文本控件 + 覆盖渲染」实现芯片，发送前警示因此对**序列化后的整条文本**跑引用扫描——`[$name]` 紧凑 token 形态（见下）与真实芯片在拍平文本上不可区分，粘贴的类形态文本（PHP 数组下标 `$envs[$this->environment]` 即典型）被当作失效引用拦截，且「移除」动作会删除粘贴原文。现输入框已迁移为富文档编辑器（TipTap）：菜单插入的引用是**结构化内联原子节点**（`skillMention`，携带展示名/目标/种类三元组），与文本节点在文档模型里天然可区分——「不可区分」这个历史前提已消失。
- **决策**：
  1. **校验对象 = `skillMention` 节点**：发送前警示与「移除失效引用」改为遍历文档模型中的 `skillMention` 节点做校验与删除，不再对序列化文本跑引用扫描。纯文本节点（含与引用载体同形者）不进入警示，也没有任何删除路径。警示文案、块内定位、移除/带警示发送的既有语义不变（US3.6 / SDR-005 风险 3）。
  2. **紧凑 token 形态退役（从警示与补全路径）**：`[$name]`（无链接目标的方括号 token）是纯文本输入框时代的历史形态，其唯一现存作用是解析旧草稿；它也是本次误报的直接来源（`[$this->environment]` 精确命中其扫描正则）。退役分两面：**提交序列化不产出紧凑 token**——其机制是节点序列化本身（有目标节点直接序列化为完整链接，无目标节点降级为裸展示名文本），`expandCompactTokens` 在提交路径仅剩一个效应：把**粘贴/手打**的同形 token（名字命中技能时）展开为完整链接并激活——这是 ADR-004 文本形态判定的已接受残余，不是草稿解析；**草稿载入时回填**——仍含紧凑 token 的旧草稿在载入编辑器时（既有「草稿 → 文档」构建步骤已把紧凑 token 物化为无目标节点）按技能名回填目标，回填后即转正为可序列化为完整链接的节点；名字不可解析的无目标节点进入节点校验按 missing 警示（本 ADR 为新行为——现状的文本扫描探不到无目标节点，不警示）。`with_skills` 回放路径（ADR-007 退役前）不受影响：其匹配键是激活清单项，不经过 token 扫描。
  3. **激活判定不动**：服务端（rust / v2 引擎）对提交文本的解析与激活判定维持 ADR-004「文本形态判定 + 双重命中」不变；本 ADR 只收窄客户端发送前警示这一前置体验层，不触碰 wire 格式、解析器与注入管线。
- **被否决的备选方案**：
  1. **保留文本扫描、给紧凑 token 加更严苛的排除规则**（如要求方括号前后无空白）——否决：在正则上继续打补丁，误报只是被推迟到下一个形态；PHP/JS 等语言的数组下标写法无法穷举排除。
  2. **警示时弹确认对话框让用户逐个裁决**——否决：把实现层的判定缺陷转嫁给用户交互成本，违背警示的本意（保护而非打扰）。
  3. **删除紧凑 token 解析以彻底消除误报面**——否决（已折中）：存量草稿与旧历史消息里存在紧凑 token，直接不解析会让旧草稿的芯片静默退化为纯文本；折中为「解析仅用于草稿载入物化 + 不再产出」。
- **影响范围**：本域 §5.5「消费端分工」（发送前校验一段以本 ADR 为准修订）；实现面为网页端 `Composer` 的发送前校验（警示判定与警示条数据源改为节点遍历）、`inlineReferences` 库的警示入口（`findInvalidInlineReferences` 与 `removeInvalidInlineReferences` 在警示/移除路径退役——前者仅测试引用，后者无调用方，随实现批次收敛或删除）、草稿载入路径（紧凑 token 物化已有，**补按名回填目标**）；需求 SDR-007 / US3.6；rust 侧零改动。

### ADR-010 · 2026-10-23 · 技能目录热更新（watcher 重扫 + 清单系统提醒）

- **背景**：SDR-008 / US7.2。Rust 引擎的技能目录为装配时一次性扫描（workspace 装配 → 一次性全量发现 → 内存 catalog 常驻），无文件监视、无 TTL；系统提示词中的技能清单是 agent 创建时的快照，目录变更只对之后新建的 agent 生效。刷新仅有的两条窄路（技能衍生配置键变更触发的重放、进程重启）都不覆盖「用户在会话进行中加了个技能」的场景。现状证据见 context §11。
- **决策**：
  1. **监视器分层（`mirri-embedded` 装配层）**：新增技能目录监视器，对每个**已构建** workspace runtime 的全部技能来源根目录（项目级、用户级、额外目录、插件派生目录；含尚不存在的候选目录）挂文件系统监视。**生命周期与 runtime 对齐**：runtime 懒构建（`ensure_built`）后挂载，registry remove 时卸载（或随 runtime drop 释放）。候选目录不存在时监听其**父目录**并对创建事件补挂（mcp watcher 先例），监听丢失（`Err` 回调）时重注册重试（既有 `WATCH_RETRY_DELAY` 模式；**watch 恢复重试不是内容轮询**，不违反「不引入自动轮询兜底」）。**运行时假设**：`mirri-embedded` 装配必然处于异步上下文（tokio runtime）——embedded 同时被 CLI 与桌面 NAPI 宿主嵌入，两者均在异步运行时内驱动装配；监视任务 spawn 于该运行时。`notify` 依赖从 `mirri-cli` 迁入 `mirri-embedded`（workspace 依赖表登记）。变更经静默期（防抖量级 ~1s，高于既有 config watcher 的 200ms——技能作者连续保存更常见，静默期须容纳「保存→再保存」的间隔）后触发该 workspace 的重扫。监视器是重扫的**触发器**，不承载扫描逻辑本身。
  2. **重扫复用既有发现序列，工作区级全量重放（rust 裁决）**：重扫执行既有 `apply_skill_discovery`（clear + 完整发现序列重放，监视器触发与配置触发**调用同一函数**）。既有 `resync_skill_catalogs` 是「全 built runtime 各自重放」的编排入口，监视器按单 workspace 触发时直接对该 runtime 调 `apply_skill_discovery`（同一重扫函数，单 runtime 编排）；用户级共享根变更经各 workspace 自己的监视各自触发、各自重放（现状语义，每个 workspace 有独立 catalog 副本）。**与「来源级重扫」不变量的关系**：rust 引擎无来源级重扫基础设施（TS 的 per-source `onDidChange` 模型在 rust 无对应物），工作区级全量重放是 rust 的既有重扫形态（config resync 同此）；成本为目录树遍历 + frontmatter 解析（典型技能目录 < 数百文件，重放毫秒级），可接受。变更触发路径「永不全量重扫」的 TS 侧表述在 rust 侧的对应物即此全量重放——需求侧 US7.1 已标注该术语边界。
  3. **重扫原子性**：重扫入口按 workspace 串行化（单 mutex，监视器触发、config 重放、手动重载共用），防止 watcher 重扫与 config 重扫交错；catalog 替换采用**构建新 map 后原子 swap**（写锁只覆盖 swap 瞬间），消除 clear-逐目录-discover 窗口期的读者可见空目录 / 瞬态 not-found。
  4. **变更通知：diff 门控 + 双通道**：重扫前对 catalog 做快照，重扫后对比，得出按名去重的变更集（新增 / 移除 / 清单可见字段变更）。变更集非空时：
     - **界面事件通道**：技能目录变更事件挂 **workspace 级广播通道**（非会话级总线——会话事件按 session_id 寻址，目录变更是 workspace 级；参照 MCP 的 app 级 channel 先例），负载含各计数与技能名清单；web 客户端（不绑定会话）经该通道订阅。
     - **模型清单提醒通道**：对每个工具面含技能入口的运行中**交互会话主 agent**（布线范围见决策 5——子代理等非交互回合不携带句柄），注入一条系统提醒（`<system-reminder>` 包裹，走既有 wire 记录原语），内容为经 `render_skills_section`（纯函数，仅依赖 `&SkillCatalog`）重新生成的完整技能清单，并前置一行变更摘要。**「内容变更」的判定粒度 = 清单可见字段级**（名称 / 描述 / 路径 / 来源 / 元数据标志）：清单可见字段不变、仅技能正文变化的编辑**不触发提醒**——清单是提醒的内容本体，清单没变就没有提醒义务；正文在激活时实时读盘（`read_content` 现行为），激活路径天然拿到最新正文，无一致性缺口。清单全空时注入**清空声明**（「此前技能已全部不可用」的中性化文案，对齐插件会话启动注入的失效声明精神）。
  5. **「下一轮」注入的挂点裁决**：既有注入通道全部是回合内每 step 粒度，无「回合边界」先例。采用**统一的 park-and-flush**：变更完成时只置 session scope 级 pending 标志（空闲与忙会话一致，不做「空闲直追加」特例——直追加需编排层穿透到每个会话的上下文记忆，布线成本高且与 flush 路径形成双轨）；pending 在**交互主 agent 的回合启动路径**（用户消息派发进回合之前）消费——渲染当前 catalog 清单、经系统提醒原语追加并记录 wire op。pending 只布线进**交互主 agent**（用户消息派发链路）——子代理宿主、定时任务、目标驱动等非交互回合创建的 `TurnFlow` 不携带 pending 句柄，天然不会把提醒注入错误上下文。多次变更合并为一个 pending，flush 时以 flush 时刻的最新清单渲染。
  6. **门控**：提醒按工具面判定——实际生效的判定面是 `ToolManager::is_tool_active("Skill")`（agent 工具注册表的启用判定；ADR 初稿写 `ToolActivationPolicy`，落地时以 `ToolManager` 为准——前者在 rust 无 runtime 实例化，此为决策修订）。**已知分叉披露**：rust 的系统提示词清单注入（`render_skills_section` 调用点）今天无工具面门控（无 Skill 工具的子代理也拿到清单），与 TS 的 `skillActiveFor` 分叉；提醒通道的门控是**新定义**，不复用「既有判定」（不存在）；系统提示词清单注入是否补门控属独立议题，不在本 ADR 范围。变更集为空时不发事件不注入提醒（diff 门控在通知层，重扫本身照常）；静默期内多次变更合并为一次重扫与一次通知。清单为空（且 diff 非空）时提醒渲染**清空声明**（`render_skills_section` 空表返回空串，清空声明是渲染路径的显式分支）。
  7. **手动重载入口**：新增引擎级重载 RPC（单方法，幂等），终端内置命令与网页 / 桌面设置页按钮均调它；执行同一重扫函数与同一通知闭环，结果（计数或无变化）随响应返回供呈现。
  8. **降级诚实**：监视器创建失败或运行中出错（权限、平台不支持、网络盘）记警告日志并静默停用该 workspace 的自动感知——不影响既有任何功能；手动重载不受影响。不引入内容轮询兜底。
- **关键裁决理由**：
  - **提醒而非改提示词**：系统提示词按装配时快照策略管理（与插件段、指南段一致），热替换技能段破坏该一致性；下一轮系统提醒 + 取代声明以既有机制（`DISREGARD any earlier skill listings` 首行的清单渲染）达成同等效果，且提醒持久化进 wire 记录（恢复会话不丢）。
  - **不新造扫描路径**：监视器只是触发器。与配置变更重放共用 `apply_skill_discovery`，两条触发路径的行为差异为零。
  - **插件重载纳入同一编排**：`plugin.reload` 现走的技能重放（`refresh_plugin_consumers`）改经同一重扫编排入口（含 diff 快照与双通道通知），并列入重扫串行化入口清单——使「通知闭环统一适用于一切完成的重扫」（US7.2）对全部触发来源成立。
  - **插件托管目录的监听**：watcher 覆盖插件派生目录（技能作者直接改托管副本的场景），与 `plugin.reload` 的全量重扫可能双触发——经重扫串行化（决策 3）与 diff 门控，双触发退化为两次幂等重放、单次通知，可接受不排除。
- **被否决的备选方案**：热替换系统提示词技能段（破坏快照策略 + 前缀缓存）；每轮动态渲染清单（常驻成本 + 与两级结构冲突）；每轮目录指纹轮询兜底（与手动兜底裁决冲突）；只刷 catalog 不通知模型（运行中会话拿旧清单，正是要消灭的不一致）；扩展注入器通道做回合首步判定（需改 `Injector` trait 语义，新机制引入面大于 park-and-flush）。
- **影响范围**：需求 US7.2 / SDR-008；实现面（rust 核心）：`rust/crates/mirri-embedded`（监视器、单 runtime 重扫编排、重扫串行化、事件 wire 透出）、`rust/crates/mirri-core`（catalog diff + 原子 swap、清单提醒渲染与注入、session scope pending flush、重载 RPC 处理、workspace 级变更事件）、`notify` 依赖迁入 `mirri-embedded`；事件与 RPC 契约随实现批次登记（五环契约链）；终端（内置命令 + 事件提示）为消费端首批，网页 / 桌面（设置区块按钮）为后续批次；冻结 TS 核心包零改动（node-sdk 的 RPC 薄封装与 test fixture、消费端 `apps/` 除外）。实施计划见 [plans/04-skill-catalog-hot-reload.md](plans/04-skill-catalog-hot-reload.md)。

### ADR-011 · 2026-10-23 · 默认技能根的路径归一去重（后到覆盖告警治理）

- **背景**：默认根发现按「项目 `.agents/skills` → 项目品牌目录 → 用户 `~/.agents/skills` → 用户品牌目录」逐目录扫描，各通道独立执行、互不感知，且编排物理上分属两层（前三个根在 workspace scope 的默认根编排，用户品牌根在装配层随后补扫）。当工作区根恰为用户主目录（服务器以主目录为默认工作区根启动）或以符号链接指向用户目录时，多个通道对解析到**同一物理目录**——不仅项目 `.agents` 与用户 `.agents` 碰撞，项目品牌目录（工作区根下 `.mirri-code/skills`）与用户品牌目录（Mirri home 下 `skills`）也碰撞——目录内全部技能被收录两次，第二次覆盖第一次时逐技能产生一条「被后到发现遮蔽」告警（单次启动即上百条），且告警的语义（「不同来源间的遮蔽」）与实际情形（同一目录被重复扫描）不符，成为纯噪音。
- **决策**：
  1. **默认根清单的归一化去重，内嵌于构造期首扫与一切重放共用的同一发现定义**：去重逻辑是接收「有序根清单（通道 + 目录）」的共享编排函数——构造期对默认根三通道去重；装配层重放时对**完整四通道清单（含用户品牌目录）**统一归一去重后扫描（不再先委托默认根编排再补扫品牌根，保证重放路径看到的根集合完整）。归一化 = 词法归一 + 符号链接解析；归一化失败者按原路径参与，不因归一失败丢失技能。同一归一化路径只保留**序列中后到的通道**——若项目通道与用户通道解析到同一目录，仅执行用户级扫描（用户通道是现状后到覆盖方，去重前后 catalog 内容与路径归属零变化，仅消除重复扫描与误告警；此处「后到」指发现序列中的执行顺序，非 US2.1 的来源优先级链）。构造期首扫与全部重放触发点（装配重放、配置 resync、插件重载重放、ADR-010 热更新重扫）共用同一定义，任一路径的扫描都带去重。
  2. **遮蔽告警保留语义本义**：同路径去重后，「被后到发现遮蔽」告警仅剩真实跨来源遮蔽（不同目录中的同名技能，后到来源覆盖先到来源）——这正是该告警的设计语义（提示用户存在同名覆盖），保留原级别不降。
- **关键裁决理由**：
  - **保留后到（用户级）通道而非先到（项目级）**：覆盖序「后到者胜」是现状执行语义，保留用户级扫描与现状最终归属完全一致（去重前后 catalog 内容零变化，只消除重复扫描与误告警）；若反保项目级则改变最终归属，属行为变更。同路径两扫时唯一差异是来源标签，二者产物等价，选「现状零变化」一侧。（注：装配层序注释与 US2.1 优先级链声称「项目 > 用户」，与实际执行序相反——该矛盾是既有独立问题，不随本 ADR 修订，也不因引用而视为权威依据。）
  - **归一化而非字符串比较**：符号链接（工作区根经链接指向主目录）与尾斜杠、`..` 等词法差异下字符串比较不可靠；TS 侧根解析本有 realpath 归一并按归一结果去重的惯例，rust 侧对齐归一化方法，保留方向按现状覆盖序取后到者。
  - **去重在序列层而非 catalog 层**：catalog 的 later-wins 是通用合并语义，被显式目录、额外目录、插件根复用；在 catalog 层做路径去重需要 catalog 感知通道语义，越界。序列编排层天然知道全部通道与执行序。
  - **归一化函数与文件系统解耦可测**：纯归一化 + 去重判定按仓库既有参数化注入惯例设计（对标 mirri home 解析的 env 注入），不依赖具体文件系统即可单测；扫描层去重测试用真实临时目录与真实符号链接（与 workspace scope 既有测试惯例一致）。跨平台注意：`std::fs::canonicalize` 在 Windows 产 verbatim 前缀路径，须统一剥前缀或全序列走同一归一函数再做比较，设计层裁归一函数为一个统一入口。
- **被否决的备选方案**：
  1. **仅在告警点判断「路径相同则降级 debug」**——否决：重复扫描本身未消除（NF1 的每来源一次扫描语义受损），且 catalog 中同一技能被赋予两个先后矛盾的来源标签这一事实被静默化。
  2. **去重保留项目级、跳过用户级**——否决：改变最终归属（用户级是后到覆盖方），产生真实行为变更。
  3. **上游禁止以主目录为工作区根**——否决：启动链路行为（服务器默认根的选取）属独立产品决策，不该由技能域修复绑定；且符号链接等别名路径仍会绕过主目录特判，治标不治本。
  4. **项目根发现对齐 TS 的「向上找 `.git`」语义**（非 git 目录下项目通道为空，主目录场景碰撞天然消失）——否决：改变非 git 目录下项目技能的现有行为（违背「归属零变化」原则），且符号链接指向 git 仓库内目录的场景仍需去重。
- **影响范围**：`mirri-core` workspace scope 的默认根编排（构造期首扫）与 `mirri-embedded` 装配层对同一序列的重放（重放路径持完整四通道清单统一去重）；ADR-010 热更新重扫经同一重放定义自动带去重。`SyncFs` 无需新增能力（workspace scope 的 catalog 本就使用本地文件系统实现；纯判定函数与 fs 解耦单测）。**已接受的残余**：额外目录（`extra_skill_dirs`）以插件源先扫，用户将其配置为默认根的别名路径时仍会产生跨源告警——该重叠属用户显式配置的真实跨源覆盖，不在本 ADR 治理面内；插件根同理不参与去重（插件根是插件管理器派生，与用户/项目通道重叠属极端边缘配置，其告警语义真实）。需求侧零变更（US2 的来源层级语义不变，最终归属不变）；TS 侧零改动。

### ADR-012 · 2026-10-25 · 技能候选的来源徽章与来源过滤（skill.list 插件标识字段）

- **背景**：SDR-009 / US3.8。技能清单 RPC（`skill.list`）的每个条目已携带 `source`（`builtin` / `user` / `project` / `extra`），但插件技能统一映射为 `extra` 且不携带任何插件身份——前端无法区分「额外目录」与「具体哪个插件」。Web 端候选列表的渲染模型（`SlashCommand`）在建项时消费 `source`（决定 `/name` vs `/skill:name` 命名与排序）后**丢弃**该值，徽章无处取数；过滤匹配键只有命令名与裸技能名。
- **决策**：
  1. **wire 契约：`skill.list` 条目新增可选 `plugin` 字段**（插件 id 字符串，取自 `SkillDef.plugin` 的插件上下文）。仅附加，`source` 值域与语义零变化（插件技能仍为 `extra`——四来源枚举是既有稳定契约，不为此扩枚举）；旧客户端忽略未知字段，服务端未携带该字段时前端降级（见决策 3）。**v3 wire 面仅 WS-RPC**：`dispatch_list_skills` 是 `skill.list` 及其 `listSkills` / `listWorkspaceSkills` 别名的唯一投影点（路由表单点），加字段只改这一处；rust 服务端**没有** `/skills` REST 路由（`/meta` 报 `backend: "v3"`，web 客户端据此走 WS-RPC），无需同步；TS v1/v2 REST 面属冻结遗留（zod strip 未知字段，不扩展、不受影响）。
  2. **前端候选项模型：`SlashCommand` 增加可选 `source` / `plugin` 字段**，`buildSlashItems` 从 `AppSkill` 透传（现丢弃点改为保留）。数据流不变：`useModelProviderState` 的懒加载/预热 → composables → `buildSlashItems` / `buildSkillTriggerItems`，仅末端不再丢弃。两个候选面（斜杠菜单与触发符菜单）共用 `filterCommands` 与 `SlashMenu.vue` 行渲染，徽章与过滤各改一处即全覆盖；桌面端直接加载同一 SPA，无独立集成点。
  3. **徽章渲染：名称旁同行小徽章**，位于 `.slash-name` 行内（技能图标 + 裸名之后），不占描述列。文本映射：`builtin` → 内置、`project` → 项目、`user` → 用户、`extra` + `plugin` → 具体插件 id、`extra` 无 `plugin`（用户配置的额外目录技能，或未携带插件标识的插件技能）→ 通用「额外」、未知值 → 原样显示。文案复用既有 `settings.skillsSource*` i18n 键零新增（「用户」措辞与既有键值一致，SDR-009 已裁决不改键值——该键同时被技能管理面板消费）；视觉样式复用技能管理面板的来源徽章语义配色（`SOURCE_BADGE_VARIANT`：project=info / user=neutral / extra=success / builtin=solid），尺寸收小（~10px 字号的小 pill）以适配弹窗行高。命令条目不渲染徽章。
  4. **来源过滤：匹配键扩展，不加语法**。`filterCommands` 对技能项的匹配键从「命令名 + 裸名」扩为「命令名 + 裸名 + `source` 标准值 + `plugin` id」，各键取最优分（`Math.max` 语义与既有裸名匹配一致）。匹配词为 wire 标准值（`project` / `user` / `builtin` / `extra` / 插件 id），**不匹配本地化文案**——任何界面语言下输入 `project` 命中项目技能。空结果语义、排序、分组均不变。
- **关键裁决理由**：
  - **`plugin` 附加字段而非扩展 `source` 枚举**：`source` 的四值枚举被多处稳定消费（命名规则、排序、设置面板分组、TS 遗留客户端）；为徽章显示而扩枚举破坏面大且「插件身份」与「来源层级」是正交信息。
  - **匹配标准值而非本地化文本**：本地化文本随语言漂移且多语言碰撞（「项目」与「工程」在不同语言互撞），标准值稳定可测；代价是用户须输入徽章对应的来源词——徽章文案与过滤词不一致（界面中文显示「项目」、过滤词 `project`）是已知取舍，可接受的降级是来源词本身也是模糊匹配的一部分（`proj` 即命中）。
  - **徽章在名称列而非描述列**：名称列是视觉锚点（用户扫视列表先看名字），描述列宽度不受控且长描述会把行尾徽章挤出视区。
- **被否决的备选方案**：
  1. **按来源分组重排**——与 SDR-004 的命令/技能两分组叠加成三层结构，信息架构复杂化。
  2. **过滤专用语法（`@plugin:` 类前缀）**——多一套语法要学要测；模糊匹配已覆盖诉求（SDR-009 已否决）。
  3. **`source` 枚举扩为含插件 id 的开放值域**——破坏既有四值契约的全部消费方。
  4. **rust 侧新增 `/skills` REST 对等端点**——web 客户端按 `/meta` 的 backend 标识走 WS-RPC，REST 面无现役消费者；属独立新端点提案，不埋入本 ADR。
- **影响范围**：rust 核心（`mirri-embedded` 的 `dispatch_list_skills` 投影增加 `plugin` 字段 + rust 单测）；web（`api/types.ts` 的 `AppSkill` 与 `RpcSkill`/`toAppSkill` DTO（WS-RPC 面）、`slashCommands.ts` 的模型与过滤、`SlashMenu.vue` 的徽章渲染 + 前端单测）；i18n 复用既有 `settings.skillsSource*` 键零新增（未知来源原样显示）。冻结 TS 核心包零改动（`client.ts` 的 REST DTO 属冻结遗留路径，不改）。


