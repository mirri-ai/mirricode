# 多模态输入 — ROADMAP

> 状态/开放项唯一源（治理见 [AGENTS.md](AGENTS.md)）。基线四件套建于 2026-09-16（v1.1：按 R1 需求审查修订 B1/B2/M1/M2 及措辞）；同日二次裁决（v1.2）：E8 媒体在场经济性升格为正式需求（SDR-002）；同日三次裁决（v1.3）：E8 退场机制收敛为压缩时机（SDR-003，否决滚动退场与预算驱逐）；同日跨域对齐（v1.4）：留档存储归属对齐持久化域 ADR-007（SDR-004，全局内容寻址、fork 不复制）；同日披露性补全（v1.5）：架构 §3.4 媒体压缩预算体系（入口 / 读时 / 上下文估值 + 行为契约，经用户裁决）；同日审查修复批（v1.6）：第三轮独立复核（跨域引用 + 代码抽验）发现 2 Major（压缩域联动未登记、占位文本四成因口径过期）+ 3 Minor（§3.4/范围边界补全未编号、US3.2 验收歧义、US2.1 版本口径），全部修复——SDR-005/ADR-006 补登、压缩域开放项 25 跨域登记、附录 A 与架构 §5.2 同步、压缩域 context 的 v2 降级链表述修正。2026-09-18 执行批：ADR-003 修订落地（rust 能力显式定义——优先级制解析 + 废除 unknown 第三态）与注入器能力导向重写（目录级可行性 / 文本通知检测 / 路径提取对齐），配套 requirements SDR-006 + architecture ADR-003 修订，见 §3/§4 本批行。
> 配套：[multimodal-input-requirements.md](multimodal-input-requirements.md) · [multimodal-input-architecture.md](multimodal-input-architecture.md) · [multimodal-input-context.md](multimodal-input-context.md)

## 1. 文档覆盖状态

| 能力面 | requirements | architecture | context | 状态 |
|---|---|---|---|---|
| 附件接入（web 上传 / TUI 粘贴 / @ 引用） | US1.1–1.3 | §3 | §2.1–2.3 | ✅ 已记录 |
| 原始文件留档与恢复句柄 | US2.1–2.2 | §3.3, ADR-004 | §2.1–2.2 | ✅ 已记录 |
| 模型输入能力识别（显式定义：声明为王 + 名字推断兜底，未定义=不支持） | US3.1–3.2 | §4, ADR-003 | §3 | ✅ 已记录（2026-09-18 批修订语义） |
| 请求前剥离与占位替换 | US4.1–4.2 | §5.1–5.2, ADR-002 | §4 | ✅ 已记录 |
| 媒体工具按能力门控 | US4.3 | §7 | §5（接口规格引用工具调用域） | ✅ 已记录 |
| 终端提交前门控 | US4.4 | §8 | §2.2, §5 | ✅ 已记录 |
| 提示式委托（占位指引 / 注入器 / 模型覆盖） | US5.1–5.3 | §6–7, ADR-001 | §5 | ✅ 已记录 |
| 视频投递通道服务商差异 | US4.5 | §5.4 | §5 | ✅ 已记录 |
| 上游拒绝降级链（413 / 格式） | US6.1–6.2 | §5.3 | §4 | ✅ 已记录 |
| 工具结果媒体投递（回挂 / 原生嵌入） | —（现状机制，服务 US4.3/US5 委托链路） | §7（工具接口引用） | §7（含修复前缺陷诊断） | ✅ 已记录（2026-09-18，Rust 修复落地） |
| 非功能（观测 / 路径安全 / 直通 / 生命周期可见性） | US7.1–7.5 | §9 | §6 | ✅ 已记录（US7.5 为 2026-09-16 新增） |
| 媒体压缩预算体系（入口 / 读时 / 上下文估值 + 行为契约） | US1.2（入口）、US8.1（估值） | §3.4 | §2.1–2.3（入口数值）、§6（估值） | ✅ 已记录（2026-09-16 披露性补全；读时参数规格归属工具调用域） |
| 媒体在场经济性（压缩退场 + 路径跟走 + 413 兜底 / 生命周期可见性） | US8.1–8.2 | §5.5, ADR-005 | §6（含 flat 2000 / v1 无路径 / v2 零处理三条证据） | ✅ 已记录（2026-09-16 新增并经 SDR-003 修订：目标需求，超出 TS 现状） |
| 消费端差异（web 无预检 vs TUI 门控） | §9 表 | §8 | §2.3 | ✅ 已记录 |
| 跨域术语（模型模态能力 MI1） | — | — | — | ✅ 已登记 glossary.md |

## 2. 未决提案（待用户裁决后转正式需求 + SDR）

| 编号 | 内容 | 出处 |
|---|---|---|
| P-1 | 引擎自动模态路由（与 ADR-001 冲突，需受控重开裁决） | requirements §10 |
| P-2 | Web 发送前能力预检（预期管理提示，不阻止发送） | requirements §10 |
| P-3 | 内置多模态子代理档案 | requirements §10 |
| P-4 | 推断误报（名字带 marker 但实际不支持）遭遇"仅支持文本"类上游错误后的能力学习 + 适配重试（SDR-006 后残留通路） | requirements §10 |
| P-5 | 音频输入消费端入口补全 | requirements §10 |
| P-6 | 网络地址图片的留档拉取（补恢复句柄） | requirements §10（R1 审查 B1 登记） |
| P-7 | 无描述模型的能力可见性（清单收录门槛放宽） | requirements §10（R1 审查 M2 登记） |
| P-8 | 服务商文件引用通道（file-id；DeepSeek Files API 专属，**低优先级 backlog**） | requirements §10（SDR-003 裁决） |

> 裁决记录（2026-09-16）：讨论中的"旧媒体驱逐策略"提案经用户裁决**直接升格为正式需求** Epic E8（媒体在场经济性，US8.1–8.2 + US7.5，SDR-002；设计方向 architecture §5.5 + ADR-005），未经历本表未决阶段。同日三次裁决（SDR-003）：E8 机制收敛为"压缩即退场"——滚动退场与主动预算驱逐因前缀缓存经济性被否决，file-id 降级为低优先级提案 P-8，L2 读图约定跨域登记至工具调用域（其 ROADMAP F-16）。E8 超出 TS 现状基线，对 TS 与 Rust 均为待实现项（不入 §3 Rust 差距表）。同日跨域对齐（SDR-004）：US2.1/附录 A/架构 §1+§3.3+ADR-004 的留档存储归属统一对齐持久化域 ADR-007（全局内容寻址附件存储——fork 不复制本体、删除不级联、引用感知回收；TS 会话目录内存储为待迁移现状）；`media-originals` 纳入该域迁移清单的补充已登记至持久化域 ROADMAP 开放项 11。

## 3. Rust 落地差距（对齐本基线的开放项）

> Rust 侧现状证据见 [multimodal-input-context.md](multimodal-input-context.md) §7 与 rust-core-design 域。**本表全部差距项已随 2026-09-16 rust 多模态基线对齐批落地收口**（实施计划 [plan/rust-multimodal-input.md](../../plan/rust-multimodal-input.md)，commits `25f1c5ac..9756aadc` + 收尾批；核验与门禁记录见 §4）。

| 优先级 | 差距 | 对应基线 | 状态 |
|---|---|---|---|
| P0 | **发送前模态剥离缺失**：含图片的消息原样发给无视觉模型，400 非可重试直接上抛（用户报告的错误路径） | US4.1 / architecture §5.1 | ✅ 已落地（2026-09-16 本批）：`MediaCapabilityAdapter` 纯投影 + 占位构造器，全部请求形态过管线；2026-09-18 批：适配器随 unknown 废除取消未知早退分支（未定义一律按不支持收敛） |
| P0 | **模型能力三源不统一**：`Llm::capability()` 按 provider 硬编码 `image_in: true`，与 ModelCatalogEntry 能力词表、VISION_MARKERS 启发式互不同源 | US3.1 / architecture §4 | ✅ 已落地（2026-09-16 本批；2026-09-18 批修订语义）：`resolve_media_capability` 改优先级制——声明含媒体词即声明为王（未列出即不支持）、空声明名字推断兜底、无信号全 false；unknown 第三态废除（ADR-003 修订），[plan/rust-media-capability-explicit.md](../../plan/rust-media-capability-explicit.md)（commits `19b24ef2`/`007c33cc`） |
| P0 | 占位文本（带留档路径 + 三条恢复指引）未实现；留档路径作为恢复句柄的通道（部件 `id`）缺失 | US2 / US4.1 / architecture §5.2 | ✅ 已落地（2026-09-16 本批）：入口无条件留档 + `id` 回写 + 统一占位构造器（capability_gap 成因带路径与恢复指引） |
| P1 | 能力缺口注入器只提示不点名（可行性检查、子代理点名、路径提取缺失）；ModelCatalog 未贯通 TurnFlowConfig（已有 TODO） | US5.2 / architecture §6 | ✅ 已落地（2026-09-16 本批；2026-09-18 批重写）：注入器改目录级可行性（`catalog_capabilities` 快照，空快照 permissive）+ 能力导向提示词（Available models 清单 + `model` 参数自判，profile 名括注）+ 文本通知检测与引号式路径提取对齐 TS，[plan/rust-media-capability-explicit.md](../../plan/rust-media-capability-explicit.md)（commit `a8ea113d`） |
| P1 | ReadMediaFile 视频支持（P0 级，plan/rust-tool-calling.md 已登记）与能力门控注册缺失；视频投递通道三级回退未实现 | US4.3 / US4.5 | ✅ 已落地（2026-09-16 本批，门控部分）：无 image/video 声明（含未定义，按不支持收敛）模型的请求工具表剔除 ReadMediaFile（unknown 保留分支已随 2026-09-18 批第三态废除删除）；视频支持/参数规格仍归工具调用域 T8a（范围红线，未随本批） |
| P1 | prompt image 压缩 + caption 持久化 F-P5f-1 未收口 | US1.1 | ✅ 部分落地（2026-09-16 本批）：入口预算压缩（3.75MB / 2000px / JPEG q85，fail-open），与留档正交；caption 持久化**未做**（`compress_entry_images` 无 caption 通道），登记 §5 follow-up |
| P2 | 413/格式降级链**已存在**（413 → Degraded → Stripped、格式错误直接 Stripped、本回合 sticky 投影），差距在具体语义：占位文本未带留档路径与恢复指引（依赖已登记的恢复句柄缺失）、与 TS 基线的"按首拒快照精确剥离"是否对齐待逐项核对 | US6 / architecture §5.2–5.3 | ✅ 已落地（2026-09-16 本批）：占位语义随 P0 行收口；"按首拒快照精确剥离"经收尾批 stripped/sticky 用例族核验一致（sticky 投影 + 确定性剥离集合，无请求形态旁路） |

## 4. 审查记录

| 轮次 | 审查者 | 范围 | 结果 |
|---|---|---|---|
| 1 | code-reviewer（需求/UX 完整性） | requirements vs 代码 | 完成：2 Blocker（US2.1 留档范围/摘要标记口径）+ 2 Major（视频投递通道、模型清单可见性）+ 9 Minor，已全部修复（v1.1 + SDR-001）；其余维度通过（占位/剥离/注入/降级链等逐条核验有码） |
| 1 | architecture-reviewer（架构/现状一致性） | architecture + context + glossary vs 代码 | 完成：抽验 ~57% 证据（60/60 行为描述符合）；2 Major（rust 已有 413/格式降级链被误列差距、web 后端矩阵与压缩归属）+ 6 Minor（引用路径精度、ADR-004 例证、glossary 导语、SDR 缺失等），已全部修复 |
| 3 | 主审（第三轮独立复核：四件套 + 跨域引用 + 代码抽验） | requirements/architecture/context vs 代码与相邻域文档（glossary/持久化/工具调用/压缩） | 完成：2 Major（SDR-003 压缩域联动未登记——补压缩域 ROADMAP 开放项 25；占位文本四成因口径过期——附录 A 与架构 §5.2/§5.5.4 同步、术语统一为"体积超限"）+ 3 Minor（§3.4/范围边界补全补登 SDR-005/ADR-006；US3.2 验收歧义消解；US2.1 版本口径标注 + 压缩域 context v2 降级链表述修正），已全部修复（v1.6） |
| 执行批次 | rust 引擎（subagent-driven，10 任务） | §3 全部差距项的实施：[plan/rust-multimodal-input.md](../../plan/rust-multimodal-input.md)（T1 能力统一 / T2 入口留档 / T4 占位构造器 / T5 剥离投影 / T6 管线接线 / T7 目录贯通+注入点名 / T8 fallback 管线 / T9 入口压缩 / T10 收尾门禁） | 完成（2026-09-16）：commits `25f1c5ac..9756aadc` + 收尾批；全量门禁（`cargo test --workspace` 全绿、clippy `-D warnings` 零告警、zero-io 0 命中）；§3 差距表逐项收口，context §7 刷新 |
| 执行批次 | rust 引擎（subagent-driven，4 任务） | ADR-003 修订落地 + 注入器对齐：[plan/rust-media-capability-explicit.md](../../plan/rust-media-capability-explicit.md)（T1 能力解析优先级制语义翻转 / T2 删除 unknown 字段 / T3 注入器重写：目录级可行性 + 能力导向提示词 + 文本通知检测 + 路径提取对齐 / T4 文档回写） | 完成（2026-09-18）：commits `19b24ef2`（声明为王语义）、`007c33cc`（删除 unknown 第三态）、`a8ea113d`（能力导向注入器）；门禁全绿；配套 requirements SDR-006 + context §7 刷新 + architecture §6 注入示例同步（T4） |
| 终审修复波 | code-reviewer（全分支终审） | 本批落地的 5 项发现：F1 格式错误降级链占位成因死代码（`FormatRejected` 无生产消费方）；F2 `VideoReferenceResolver` 无 unknown 门控；F3 本文档 P1 行 caption 措辞夸大 + context ACP 行过时；F4 follow-up 登记（§5）；F5 文件末尾换行 | 完成（2026-09-16）：F1/F2 代码修复 + 用例（格式链/413 防回归/两成因单测/unknown 放行）；F3–F4 文档如实化；门禁复跑全绿 |
| 沙箱缺陷修复 | 主控（用户报告缺陷） | 注入 home 的部署形态下（`<home>/v3_sessions/.../blobs/<sha256>`），媒体留档原件路径在工作区根之外，子代理 ReadMediaFile 被 path-traversal 校验拒绝——E5 委托链路断头，违反 US5.1（路径与工具名必须真实可用）与 US7.3（留档路径经读文件工具一致的越界校验）的共同前提 | 完成（2026-09-16）：blob 存储根注册为会话沙箱 additional dir（`message.send`/steer 三落盘点，位置无关——ADR-007 全局根迁移后同一准入继续成立；注册前物化目录保证 canonicalize 同形）；红→绿集成用例（注入 home + 工作区外持久化根形态，context §7 已登记）；门禁全绿 |
| 留档扩展名修复 | 主控（用户报告缺陷） | TS desktop 的媒体原件留档带扩展名（`media-originals/<hash>.png`），rust 的 blob 存储为裸 `<sha256>`——代理与 OS 工具无法从留档路径推断文件类型，与 TS 行为不一致 | 完成（2026-09-16）：blob 文件名按 MIME 追加扩展名（TS `MIME_EXTENSION` 逐项对齐，未知 MIME 保持裸哈希）；读路径按哈希探测历史拼写，存量裸哈希文件不受影响；映射/追加/存量回读三项用例（context §7 已同步）；门禁全绿 |

## 5. 基线对齐批遗留 follow-up（2026-09-16 终审登记）

> 每项一行 + 归属建议，不展开设计（终审 Minor 发现，均为非阻断）。

- 入口双重解码：persist 与 compress 各自对同一字节做 base64 + image 解码（热路径 CPU 浪费）——归属建议：本域入口（`media_ingest`/`blob` 联合单趟解码）。
- 留档写失败零可观测：`ensure_blob_path` fail-open 静默，无任何日志——归属建议：本域入口（补 `tracing::warn`）。
- `dynamically_loaded_tools` 解析后在 merge 中被丢弃（无消费者）——归属建议：模型能力域/工具调用域（MI1 贯通时一并处理）。
- 入口压缩无 caption；单趟压缩不保证字节上界（噪点大图可能仍超 3.75MB）——归属建议：压缩预算体系（architecture §3.4，与 §3 P1 行 caption follow-up 同源）。
- 能力缺口注入 sticky 于 turn-start 模型（mid-turn 切换不重算，代码注释已自认的已知限制）——归属建议：提示式委托（architecture §6，列为已接受限制或随 P1 提案演进）。
