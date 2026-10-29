# Provider 凭据（provider-credential）ROADMAP

> 状态/开放项唯一源。需求见 [provider-credential-requirements.md](provider-credential-requirements.md)；架构见 [provider-credential-architecture.md](provider-credential-architecture.md)；现状见 [provider-credential-context.md](provider-credential-context.md)。本文只做状态管理，不承载需求或设计。

## 覆盖状态（2026-10-28 建域）

| Epic | 主题 | 状态 |
|---|---|---|
| A | 凭据库（每供应商一条，SDR-004） | ✅ 已文档化 |
| B | 命令行管理面（辅助面：回读/无头/直连） | ✅ 已文档化 |
| G | Web 管理界面（第一管理面，SDR-003） | ✅ 已文档化 |
| C | 外部凭据引用（Azure CLI） | ✅ 已文档化 |
| D | 失效呈现与重授权引导 | ✅ 已文档化 |
| E | 迁移与兼容 | ✅ 已文档化 |
| F | 非功能 | ✅ 已文档化 |
| 提案 P-1..P-5 | 超出本批的增强（P-1 加密演进的承接面已预留：按条目加密形态声明 + 混合态语义） | ✅ 已登记（requirements §12） |

## 评审记录

| 轮次 | 评审者 | 范围 | 结论 |
|---|---|---|---|
| 1 | design-reviewer（需求完整性 / R1-R9 / 反向幻觉） | requirements + glossary PC 节 | PASS-WITH-FINDINGS：13 条（F-01 死键陷阱 high、F-02 供应商删除级联缺项、F-03 不可达例外、F-04 external 回读未定义、F-05 明文禁止面不对称、F-06 web 切换交互无主、F-07 引用错、F-08 手动迁移散射、F-09 重名标签、F-10 「最新」未定义、F-11 产品语残留、F-12 PC5 未登记、F-13 opencode azure 事实不精确）→ 已全部修复 |
| 1 | architecture-reviewer（机制一致性 / file:line 抽查 / 与相邻域冲突） | architecture + context | PASS-WITH-FINDINGS：10 条（F-01 失效钩子前提与代码不符 high、F-02 CLI/server 跨进程可见性未设计 high、F-03 迁移清除不收敛、F-04 解析链三文不一致、F-05 §0 残留矛盾、F-06 锁先例不存在、F-07 脱敏边角漏风、F-08 report_failure 布线未定义、F-09 context 行号漂移、F-10 三小项）→ 已全部修复；context §1.1 事实清单 11/11 核验通过 |

修复要点（同批回写）：需求侧新增 A.5b（供应商删除级联）、E.5/E.6（手动迁移 + 启动期重检，关死 F-01 死键陷阱）、B.2 重名拒绝、B.5 external 回读语义、F.2 禁止面扩至网络接口 + 报文脱敏 + 文件工具问询；架构侧 ADR-005 重写（缓存键纳入凭据指纹，新增失效机制而非复用）、ADR-006（解析直读文件）与 ADR-007（会话索引锁约定）新增、迁移伪码补补偿清除、锁与 azure 事实按 reviewer 核查结果修正。

**SDR-003 批（2026-10-28，用户裁决后同日修订）**：管理面主次重定——web（desktop）为第一管理面：新增 Epic G 全语义（状态/更新/移除/只写不读），`credentialService` RPC 为完整操作面 + 状态；CLI 重定位为辅助面（回读/无头/直连）；TUI 面本批不做（既有 TUI 短期不维护，rust 内核 TUI 就绪后经同一服务层接入）。原 O-2（web 切换交互无主）随之关闭。

**SDR-004 批（2026-10-28，用户裁决）**：砍除多密钥并存与激活切换——每供应商恰一条凭据，添加即覆盖（D-M7）。近半数 Epic A 复杂度（激活标记/接任规则/标签唯一/切换交互）随之消失；存储保留条目数组形态（未来放开零迁移，提案 P-5 为重启路径）。requirements 升 v3。

## 开放项（需人工/后续裁决）

- **O-1** ~~切换生效与模型裁定链的交互~~（评审已闭环：A.4 按裁定链 R2 同构表述，架构 §5.2 缓存键机制承接）。
- **O-2** web 设置页「编辑密钥」迁移到凭据库写入（D-M6）后，model-settings US-6 的验收措辞需同步修订（走该域 SDR 流程）。~~多凭据切换的 web 交互无主~~ **已关闭（2026-10-28，SDR-003）**：Epic G 全语义落在本域。
- **O-3** ~~RPC contract 注册位置~~ **已关闭（2026-10-28，实施批 Task 5）**：mirri-proto 侧复用 `D::Config` 域（`emb("credential.*", D::Config)`，embedded-only、v2=Y、Settled），不新建 `MethodDomain::Credential` 变体（枚举 + name/batch 两映射 + tsv domain 列校验的成本不成比例）；klient contract 名 `credentialService`（客户端侧注册归 web 接线批）。
- **O-4** E.3 告警内容：**已关闭（2026-10-28 收尾批）**——迁移与 dry-run 对每个 skipped 条目逐条告警（provider id + 手动处理指引），CLI 输出同步；「跳过清单持久化」不做（畸形条目发生率极低）。
- **O-5** 401 自动上报接线：**已关闭（2026-10-28 收尾批，SDR-005/ADR-009）**——`Llm::provider_id()`（默认 None，适配器构造注入）+ `ProviderAuthError` 结构化归因；五个出水面全部接线（wait 返回 / fire / steer / cron（trigger/调度两入口收敛于 inject_cron_prompt 单点）/ task / goal），None 归因诚实不上报；wait/fire/None 三态有测试。
- **O-6** web 设置页接线（Epic G 前端，**Epic G 全条目**）：**已关闭（2026-10-28 收尾批）**——editKey 改走 `credential.set`（现状写 config.apiKey 字段已被 D-M3 退休、实际不生效，必须切）；G.1 状态徽标 / G.5 外部在场态 / G.8 通知刷新（`credential.updated` 经 onRpcEvent 订阅，注意会被无 sessionId 事件的默认过滤丢弃，需专用订阅）；model-settings US-6 措辞已修订（4c6b4127，回显改留空等七点）。CLI `credential list` STATUS 列（直连 `unknown` + external AZ 在场标记）、`credential.list` RPC 的 `external_available`/`status` 字段、G.2 外部引用登记表单（API key / Azure CLI 类型选择）、G.3 web 移除凭据入口、G.7 通知刷新（credential.updated 专用订阅）均已交付。V13-V15 全部验收有实现归属。

## 实施批建议（plan 拆分输入，非状态）

1. 存储与服务层（Epic A + F.3/F.4 + 解析链集成 §5.1）——rust，先行；
2. RPC 面（credentialService 完整 CRUD + 状态）——依赖批 1；
3. **web 管理界面（Epic G，第一管理面）**——依赖批 2，含 model-settings US-6 目标态切换（走该域流程）；
4. CLI 辅助面 + 迁移（Epic B + E）——依赖批 1；
5. 外部引用（Epic C）——依赖批 1；
6. TUI 面——阻塞至 rust 内核 TUI 就绪（SDR-003）。
