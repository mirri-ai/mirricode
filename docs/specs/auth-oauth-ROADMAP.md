# 认证（auth-oauth）ROADMAP

> 状态/开放项唯一源。需求见 [auth-oauth-requirements.md](auth-oauth-requirements.md)；架构见 [auth-oauth-architecture.md](auth-oauth-architecture.md)；现状见 [auth-oauth-context.md](auth-oauth-context.md)。本文只做状态管理，不承载需求或设计。

## 覆盖状态（2026-09-12 建立基线）

| Epic | 主题 | 状态 |
|---|---|---|
| A | 登录发起与入口（CLI/TUI/web/ACP/REST） | ✅ 已文档化（managed login 入口已全线退役 2026-10-12，见 SDR-004） |
| B | 设备码协议（RFC 8628） | ✅ 已文档化 |
| C | 令牌存储与安全 | ✅ 已文档化 |
| D | 设备身份 | ✅ 已文档化 |
| E | 刷新、过期与多进程安全 | ✅ 已文档化 |
| F | 认证状态呈现与就绪门 | ✅ 已文档化 |
| G | API key 认证方式 | ✅ 已文档化 |
| H | 登出与清理 | ✅ 已文档化 |
| I | 登录后配置装配触发契约 | ✅ 已文档化 |
| J | 账户信息与用量查询（引擎级） | ✅ 已文档化（web UI 消费缺口见开放项 O-1） |
| K | 非功能 | ✅ 已文档化 |
| 提案 P-1..P-10 | 超出 TS 现状的增强 | ✅ 已登记（requirements §13，免幻觉检查） |

> **2026-10-13（SDR-004 收尾）**：TS 实现载体 `packages/oauth`（`@mirri-ai/mirri-code-oauth`）与 `packages/kimi-code-oauth`（`@mirri-ai/v2-oauth`）已删除，TS 侧 managed OAuth 全链退役；仅 v1 wire `managed_provider` 契约保留（wire-compat 缓释）。上下文见 [auth-oauth-context.md](auth-oauth-context.md)。

## 评审记录

| 轮次 | 评审者 | 范围 | 结论 |
|---|---|---|---|
| 1 | design-reviewer（需求完整性/反向幻觉/R1-R9） | 全部 Epic | PASS-WITH-FINDINGS：16 条（F-01 幻觉·遥测仅成功上报；F-02 需标注增强·有效期不钳制；F-03/F-04 现状记录缺口；F-05~F-15 遗漏/措辞；F-16 反馈通道边界）→ 已全部修复 |
| 1 | architecture-reviewer（机制一致性/file:line 抽查/快照日期/v1 标记） | architecture + context | PASS-WITH-FINDINGS：4 条（F1 useWorkspaceState 路径少 `client/` 段；F2 失败前置措辞；F3 登出顺序 v1/v2 互换；F4 ensureReady 链与 thinking 清理未标 v1/v2）→ 已全部修复 |
| 2 | 两位 reviewer 复审 | 修复验证 | architecture：**PASS 零 comments**；requirements：PASS-WITH-FINDINGS（余 4 条 low：C-01~C-04）→ 已修复 |
| 3 | design-reviewer 快速验证 | C-01~C-04 | 4 条全部已解决；新发现 1 条 low（R3C-01：B.5 的 P-9 指针不含 TUI 改进项）→ 评审者预认证「补一句后即为零 comments」→ 已按原话修复，闭环 |

**最终状态：零 comments。**（requirements：3 轮；architecture/context：2 轮。）

## 开放项（需人工/后续裁决）

- **O-1** web/desktop 消费 userinfo/usage：v2 引擎与 REST 端点已提供，web UI 未接线（context 矩阵 #6）。Rust 侧是否随基线一起做 UI 消费 → 对应提案 P-5。
- **O-2** `/login` vs `/login-with-kimi` 文案漂移（context §12.1）：**已关闭（2026-10-12，SDR-003）**。TS 内置命令曾收敛为 `/login`（别名 `login-with-kimi`）；同日 SDR-004 进一步退役全部 TS 登录入口（TUI `/login`/`/logout`、CLI `mirri login`、ACP terminal-auth），`/login` 不再是可用命令，文案与文档统一指向 `/provider`。对应提案 P-7 一并关闭。
- **O-3** 服务访问密码环境变量名（`MIRRICODE_CODE_PASSWORD` vs `MIRRICODE_PASSWORD`）：**已裁决关闭（2026-10-09）**。定性为文案缺陷：弹窗曾指向 `MIRRICODE_CODE_PASSWORD`——该名字两个引擎都不读（v2 实际读 `MIRRICODE_PASSWORD`，v3 无密码变量）。裁决：不引入密码环境变量能力，保持「随机凭证 + 启动打印」单一方式；弹窗文案已改为只指引启动打印的凭证，并加防回归测试（文案不得出现环境变量名）。对应提案 P-8 一并关闭。
- **O-4** v1/v2 平台标识与默认 base URL 不一致（context §12.3/12.4）：跨包历史遗留；Rust 单实现天然收敛，是否需要兼容期映射待定。
- **O-5** `managed_provider.status` 四态契约 vs 两态实现（context 矩阵 #7）：契约保留位是否在 Rust 中真实产出 `expired/revoked`（凭证墓碑已有三态基础）待裁决。
- **O-6** keyring 存储（提案 P-1）：配置面已预留 `storage: 'file'|'keyring'`，Rust 落地系统钥匙串后是否成为默认待定。

## 跨域术语注册清单（待发起方统一合并入 glossary.md，本域不直接编辑）

| 规范名 | 出现面 | 说明 |
|---|---|---|
| 托管账户登录（managed:mirri-code） | config.toml providers 键、REST wire、TUI/web 文案 | model-settings 域同用；禁称「kimi 账户/provider」（规格内部） |
| OAuthRef（凭证槽引用） | config.toml provider/model 条目 `oauth` 字段、wire 契约 | 形态 `{storage:'file'|'keyring', key:'oauth/mirri-code'…, oauthHost?}`；本域 §4 详述 |
| 设备码（device code / user_code） | RFC 8628、CLI/TUI/web 用户可见文案 | 禁与 2FA「验证码」混称 |
| 认证就绪门（auth readiness gate） | v2 ensureReady、GET /auth、web 首屏 | 会话启动前的静态凭据校验 |
| 终端认证（terminal-auth） | ACP 协议 authMethods | 宿主复用 CLI 登录入口的方法名（TS 已随 SDR-004 移除，`authMethods` 恒为空） |

## 相邻域与 Rust 迁移引用（一句话，不重复）

- Rust 侧已有 MCP 服务器 OAuth 流实现与跟踪，见 `docs/specs/mcp-ROADMAP.md` 与 `docs/specs/rust-core-design/ROADMAP.md`（MCP OAuth 已收口，用户账户登录不在其中）。
- `plan/rust-v2-method-parity.md` 将 `auth.*` RPC 方法列为 8 个深方法（OAuth 流、mirri-auth token 存储获得读取方，mini-design 先行）——本文档四件套即其输入。
