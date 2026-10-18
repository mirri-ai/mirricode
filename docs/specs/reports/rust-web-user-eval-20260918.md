# Rust Core + Web 用户视角评测报告（2026-09-18）

## 评测方法

以终端用户身份完成全链路体验：`rust/build-rust.sh` 打包（web SPA 重建 11s + cargo release 56s，产物 50MB，冒烟通过）→ `rust/scripts/release-package.mjs` 产出 `mirri-code-darwin-arm64.zip` → 隔离 `MIRRICODE_HOME`（`.tmp/user-eval/home`）+ 演示工作区启动 `mirri web` → Playwright 驱动浏览器逐项试用功能。模型链路使用真实 provider 配置（rakuten / github-copilot）。

**结论先行**：核心链路（对话、工具调用、审批、附件、终端、多工作区、会话持久化/恢复）全部可用，工程质量整体扎实；但存在 **1 个新用户完全不可用的致命问题**（登录死路）、**1 个稳定复现的高频体验缺陷**（SetSessionTitle 提醒泄漏），以及一批 i18n / 品牌一致性 / 术语统一问题。

## 体验正常的部分（正面清单）

- 打包：`build-rust.sh` 一键完成 web 重建 + staging + release + 冒烟，日志清晰；`release-package.mjs` 产出 zip + sha256 正常。
- `mirri web` 启动横幅清晰：URL、token 值、token 文件路径一次给出；`--help` 文档质量高。
- 首启 Onboarding：语言 / 明暗 / 主题色，中文切换即时生效。
- 对话链路：真实 LLM 对话、Read/Write 工具调用渲染（含文件预览、行数、状态）均正常；token 用量、缓存命中率、上下文水位条显示完整。
- 权限审批：写文件触发审批卡片，批准/本会话内批准/始终允许/拒绝四档齐全，批准后文件正确落盘。
- 附件：文本附件上传后内联注入，模型正确读出内容。
- 终端：内嵌 PTY 终端可交互，cwd 正确。
- 多工作区：添加工作区（面包屑 + 模糊搜索）、未信任标记、信任弹窗、切换全部正常。
- 会话持久化：服务端 kill 重启后 wire log 回放恢复，消息、工具调用、TOC、会话列表全部还原。
- 会话操作：重命名、⌘K 搜索（含键盘提示）、对话目录 TOC 正常。
- 认证后浏览器控制台零错误。

## 问题清单

### P0 — 新用户完全无法使用

**#1 新用户登录死路（阻断性）**
全新用户（无 config.toml、无 OAuth 凭证）打开 Web 后：
1. 先被 Server Token 弹窗拦住（合理）；
2. 输入 token 后被硬性重定向到 `/login`（`useAuthGate` 的 `showAuthGate` 无任何跳过入口）；
3. 点击 Sign in 得到「The current daemon does not support login yet — Please upgrade mirri-code and try again」。

rust daemon 未实现登录 RPC，Web 端也没有「跳过登录 / 使用自有 API key」出口 → **新用户在 rust core + web 上是死路**，必须预先手写 config.toml 才能用（我正是靠复制已有配置才完成后续评测）。这是 rust/web 打包形态下最优先要修的问题：要么 daemon 实现登录，要么 Web 提供 skip/手配 provider 入口。

### P1 — 高频体验缺陷

**#2 SetSessionTitle 提醒与工具注册不一致，模型推理文本泄漏到正文（稳定复现）**
- 现象：每个未命名会话的首个回合，模型回复开头出现一段英文抱怨（如 "The system reminder says to call a SetSessionTitle tool, but I don't have that tool in my available functions..."），直接渲染在对话正文里；两个不同会话均复现。
- 根因：`rust/crates/mirri-core/src/turn_flow/injection/title.rs` 的 TitleInjector 无条件给未命名会话注入「调用 SetSessionTitle」提醒，但 builtin `agent` profile 的工具白名单 `AGENT_PROFILE_TOOLS`（`rust/crates/mirri-core/src/profile.rs:2707`）不含 `SetSessionTitle`，`session_handlers.rs` 设置的 `title_handle` 注册被 profile 白名单（`enabled_tools`）过滤掉。实测 `tool.list` RPC 返回 151 个工具，无 `SetSessionTitle`。
- 连带后果：会话自动命名永远不生效，侧栏/标题只能显示整条用户消息（截断）。
- 修复方向：把 `SetSessionTitle` 加入 `AGENT_PROFILE_TOOLS`（或让 title injector 与工具可用性联动）。

**#3 服务端重启后上下文用量条归零**
重启前显示「使用 38.8k / 488k tokens (8%)」，wire log 恢复后变为「使用 0 / 0 tokens (0%)」。恢复路径没有把累计 context usage 回填到 Web 端水位条（turn 用量 pill 倒是恢复正常）。

**#4 认证前 UI 报错抢先出现**
首次打开（还没输 token）就弹出「Operation failed / The last operation did not finish」toast + 控制台三条错误（WS 401、model.list not connected）。这些在认证完成前应当静默或被抑制，不应该作为「第一印象」呈现给用户。

### P2 — 品牌与 i18n 一致性

**#5 Kimi 品牌残留**：登录页标题「Sign in to Kimi Code」（登录的产品是 Mirri Code）；斜杠命令 `/login-with-kimi`（描述却写「在浏览器中登录 Mirri」）。
**#6 审批卡片未翻译文案**：「文件操作?」带问号（疑似 i18n 占位符问题）、「APPROVAL REQUIRED」英文大写混在中文 UI 中。
**#7 环境变量名指错**：ServerAuthDialog 让用户设置 `MIRRICODE_CODE_PASSWORD`，但 rust 实现中既无此变量也无 `MIRRICODE_PASSWORD`（spec P-8 已登记，仍未落地）。用户照做无效。
**#8 术语不统一**：同一对象在不同 UI 中叫「对话 / 会话 / 任务」（重命名任务、归档任务 vs 新建对话 vs 搜索会话）；模式菜单「计划 / Swarm / Collaboration / 目标」中英混排。
**#9 斜杠菜单被技能淹没**：`/` 菜单列出 100+ 条 `/skill:*` 条目（全英文长描述），核心命令被淹没；两条技能描述渲染为孤立的 `>`（`/skill:caveman`、`/skill:review-test-cases`）。建议命令与技能分组/折叠。
**#10 空标签菜单项**：工作区分组「选项」菜单里有一个无可读文本的 menuitem（a11y + i18n）。
**#11 Onboarding 明暗选项命名**：英文「Moon Bright / Moon Dark」语义怪异（中文「月之亮面/月之暗面」尚可）；建议 Light/Dark。

### P3 — 工程观察（非直接用户可见）

**#12 MIRRICODE_HOME 隔离不彻底**：设置 `MIRRICODE_HOME` 后，MCP 全局层与技能仍从真实 `$HOME/.mirri-code/mcp.json`、`~/.agents/skills` 读取（日志可见 MCP watcher 监视 `/Users/.../.mirri-code`）。对单机单用户无碍，但对「隔离/多环境」场景是隐患，也让本次评测环境混入真实 MCP 配置。
**#13 服务端日志噪音**：
- `agent profiles hot-reloaded` 每 30s 左右空跑一次（loaded=0，无变化）；
- 对已 disabled 的 MCP server 反复输出 `removed MCP tools from live agents`（playwright-stg/lynx-devtool 等未连接成功也发 remove 事件）。
**#14 Web 构建警告**：主 chunk > 500kB（无 code-splitting），首屏加载体积偏大。
**#15 部分图标按钮无可访问名称**（模型卡片操作按钮等 icon-only button）。

## 复现环境

- 二进制：`rust/target/release/mirri`（1.11.0，full 变体，web SPA 嵌入）
- 启动：`MIRRICODE_HOME=<isolated> mirri web --host 127.0.0.1 --port 45831 --no-open`（cwd = 演示工作区）
- 关键路径：`rust/crates/mirri-core/src/profile.rs:2707`（AGENT_PROFILE_TOOLS）、`rust/crates/mirri-core/src/turn_flow/injection/title.rs`（TITLE_REMINDER）、`apps/mirri-web/src/composables/useAuthGate.ts`（登录门）、`apps/mirri-web/src/components/ServerAuthDialog.vue:47`（密码变量名）
