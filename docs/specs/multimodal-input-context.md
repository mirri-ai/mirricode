# 多模态输入 — 现状盘点（Context）

> 快照日期：2026-09-18（能力显式定义批后刷新；§7 新增本批注记） ·
> 本文档是现状清单与代码事实盘点，不含目标与决策（那些见 [multimodal-input-requirements.md](multimodal-input-requirements.md) / [multimodal-input-architecture.md](multimodal-input-architecture.md)）。设计文档单向引用本文档。
> 引擎口径：v2（`packages/agent-core-v2`）为主；v1（`packages/agent-core`）仍被消费的面以 `[v1]` 标注；消费端（web / TUI）单列。Rust 侧现状（§7）已随 2026-09-16 多模态基线对齐批刷新为对齐后的新事实（权威记录在 rust-core-design 域）。

## 1. 媒体内容部件与表示

| 事实 | 证据 |
|---|---|
| v2 部件定义 `ImageURLPart { type:'image_url', imageUrl:{url, id?} }`；`ContentPart = Text \| Think \| ImageURL \| AudioURL \| VideoURL` | `packages/agent-core-v2/src/kosong/contract/message.ts:28-43` |
| [v1] 同构部件定义 | `packages/kosong/src/message.ts:16-18`（`isContentPart` :118-123） |
| `id` 字段语义 = 留档路径（持久化层写入，能力缺口恢复消费） | `packages/agent-core-v2/src/agent/media/mediaCapabilityProjector.ts:67-83`（`persistedFilePathOf`，仅文件系统路径采信，拒绝 `ms://` 等协议式标识） |
| wire 三态 source（`url`/`base64`/`file`）+ 通用 file 部件 | `packages/protocol/src/message.ts:30-61`；web 本地重实现 `apps/mirri-web/src/api/types.ts:211-224`、`api/daemon/wire.ts:220-227` |
| [v1] 提交面 PromptPart 提取 | `packages/node-sdk/src/types.ts:88-91`、`packages/agent-core/src/rpc/core-api.ts:40` |

## 2. 入口与媒体持久化

### 2.1 服务端（v2 / kap-server）

| 事实 | 证据 |
|---|---|
| 上传接口 `POST /files`（multipart 单文件，无大小上限），返回 `{id,name,media_type,size,created_at,expires_at?}` | `packages/kap-server/src/routes/files.ts:88-137`（schema `packages/agent-core-v2/src/app/file/fileService.ts:18-26`、`packages/protocol/src/rest/rest-file.ts:9-12`） |
| 提交解析 `resolvePromptMediaFiles`：内联 base64 图片 → MIME 嗅探门控 → 压缩 → **无论压缩与否持久化原始字节并把路径写入部件 `id`**（注释明示这是无视觉模型时 subagent ReadMediaFile 的恢复句柄） | `packages/kap-server/src/routes/prompts.ts:501-753`（关键段 :542-621，恢复句柄注释 :603-607） |
| `url` 图片仅按扩展名门控后原样透传（:631-640）；`file` 附件物化到会话附件目录并替换为"Attached file…open it with the Read tool"文本（:645-654）；上传图片（source.kind='file'）→ 门控 → 压缩 → base64 部件带 `id: 原件路径`（:663-738）；视频 → `kimi-file://` 引用（:740-750） | 同上 |
| wire → 引擎部件转换与入队 | `prompts.ts:470-480`（`contentToCoreParts`）、`:331-336` |
| 缺失 file_id → `FILE_NOT_FOUND`；非图片文件冒充图片 → `VALIDATION_FAILED`；合法 PNG 混排被接受；可中止 | [v1][klient] e2e 契约 `packages/klient/test/e2e/legacy/image-file-prompts.test.ts:74-129`（v1 REST 表面独有，见文件头 :1-11） |
| 提交时对失效/类型不匹配的文件凭证快速失败（不进入对话） | `packages/kap-server/src/routes/prompts.ts:192-201, 838-845`（`assertPromptFileRefs`） |
| 上传凭证支持有效期（`expires_in_sec`），过期凭证提交失败 | `packages/kap-server/src/routes/files.ts:111` |


### 2.2 终端（TUI，消费 v2）

| 事实 | 证据 |
|---|---|
| 粘贴：跨平台剪贴板读图（macOS NSPasteboard / Linux wl-paste·xclip / WSL PowerShell），MIME 白名单门控 | `apps/mirri-code/src/tui/controllers/editor-keyboard.ts:426-487`；`utils/clipboard/clipboard-image.ts`、`clipboard-common.ts:18-29` |
| 粘贴即压缩（上限来自配置 `maxEdgePx`）+ 原件留档 + 插入占位标记 `[image #N (W×H)]` | `editor-keyboard.ts:460-483`；配置 `apps/mirri-code/src/config/schema.ts:153-168`；占位格式 `tui/utils/image-attachment-store.ts:136-138` |
| 提交展开：占位标记 → `image_url` base64 部件（有压缩时附 caption 行，含原件路径）；skill/plugin 参数中的媒体引用重写 | `apps/mirri-code/src/tui/mirri-tui.ts:1090-1114`；`tui/utils/image-placeholder.ts:51-102, 190-196, 219-248` |
| @ 引用插入纯文本路径（图片由模型用 ReadMediaFile 读） | `tui/components/editor/file-mention-provider.ts:260-264` |
| 入口 MIME 格式门控（AVIF/HEIC/BMP/TIFF 等替换为文本通知） | v2 `packages/agent-core-v2/src/agent/prompt/promptStepRequests.ts:37-46`（`gateImageFormatParts`）；[v1] `packages/agent-core/src/agent/turn/index.ts:135,148` + `tools/support/image-compress.ts:514-562` |
| 媒体原文件留档目录（内容寻址） | [v1] `packages/agent-core/src/tools/support/image-originals.ts:143-159`（`persistPromptMediaParts`，`agent/turn/index.ts:527-530` 调用）；v2 由 kap-server 入口层承担（§2.1） |
| 历史/转录媒体渲染：终端内联缩略图（kitty/iterm2，≤12 行高），否则占位文本 | `apps/mirri-code/src/tui/components/media/image-thumbnail.ts:23-65` |
| 媒体占位不只随普通消息展开——斜杠命令与 skill/plugin 参数中的媒体占位同样重写（plain/tag 两种引用形态） | `apps/mirri-code/src/tui/mirri-tui.ts:1272, 1298`（`rewriteMediaPlaceholders`） |


### 2.3 Web（后端自动探测：TS 守护进程 / rust）

| 事实 | 证据 |
|---|---|
| 后端自动探测：`/meta` 返回 `backend==='v3'` → rust `WsRpcApi`（上传走 rust `POST /api/v1/files`）；否则 TS 守护进程 `DaemonMirriWebApi`；构建变量可强制 | `apps/mirri-web/src/api/index.ts:31-88`；`ws-rpc-api.ts:2333` |
| 入口三通道（文件选择 multiple 无 accept 过滤 / document 级 paste / composer 局部拖拽 + window 级防跳转），按会话隔离的附件状态（上传中 spinner、失败红框、可移除、预览灯箱） | `apps/mirri-web/src/composables/useAttachmentUpload.ts:54-67, 81-134, 157-202, 209-268, 384-403`；`components/chat/Composer.vue:749-857, 1100-1103`；`components/chat/AttachmentChip.vue:104-117` |
| 上传凭证制：`POST /api/v1/files` → fileId；发送时 `{type:'image', source:{kind:'file', fileId}}`；守卫：有 uploading 附件时阻止发送，error/无 fileId 的附件不进入消息 | `useAttachmentUpload.ts` + `apps/mirri-web/src/api/daemon/ws-rpc-api.ts:2329-2377, 2936-2973`；`Composer.vue:318-333`；`apps/mirri-web/src/composables/useWorkspaceState.ts:1651-1664` |
| **无客户端大小/格式限制、无压缩、无能力预检**——原始 Blob 直传，附件入口不随模型能力变化 | `ws-rpc-api.ts:2337-2345`；`Composer.vue:555`（`hasUpload` 只看上传函数存在性）。压缩在服务端且仅 TS 后端配对成立：kap-server 用 v2 `packages/agent-core-v2/src/agent/media/image-compress.ts:65-91`（字节预算 3.75MB/256KB/64MB 解码上限），v1 守护进程路径用 `packages/agent-core/src/tools/support/image-compress.ts`；rust 配对下无 prompt 压缩（F-P5f-1） |
| 附件状态按会话隔离；粘贴入口接受任意文件类型（非仅媒体）；消息撤销/编辑后附件条回填且原凭证复用免重传；音频在 web 侧按普通文件附件处理（无音频媒体入口） | `useAttachmentUpload.ts:54-56, 74-79, 165-202, 302-376` |
| 失败可见性：通用失败 toast + 服务端 msg（无图像专属提示）；乐观消息回滚 | `useWorkspaceState.ts:1764-1774`；`DaemonApiError` `api/errors.ts:6-42` |
| 模型能力展示：capabilities 字符串数组以徽章原样渲染；编辑表单预置 `image_in/video_in/audio_in/thinking/tool_use/dynamically_loaded_tools` chip，**新增入口预置仅 `image_in/thinking/tool_use`（窄于编辑表单，video/audio 须经新增后的编辑补全）**；**未消费结构化 `capability.image_in` 布尔形态** | `ModelPicker.vue:198-200`；`ModelEditForm.vue:42`；`AddProviderDialog.vue:37-38`（`CAP_PRESETS`）；对照 `packages/protocol/src/modelCatalog.ts:3-11, 32-47` |
| 气泡渲染：附件芯片 + AuthMedia 鉴权取图（blob URL + 懒加载 + 失败回退）；服务端回显 `<image path>` 标签反解；压缩 caption 精确剥离 | `ChatPane.vue:517-597`；`AuthMedia.vue:51-93`；`apps/mirri-web/src/composables/messagesToTurns.ts:19-24, 951-983` |

## 3. 模型输入能力目录

| 事实 | 证据 |
|---|---|
| 能力记录 `ModelCapability{image_in, video_in, audio_in, thinking, tool_use, max_context_tokens,…}`；未知态 = 全 false + 不可枚举 symbol 标记，`isUnknownCapability` 判定 | v2 `packages/agent-core-v2/src/kosong/contract/capability.ts:11-25, 34-55`；[v1] `packages/kosong/src/capability.ts:11-25, 34-48` |
| 能力解析：配置声明 OR 目录检测（`resolveModelCapabilities`：declared OR detected；detected 未知且无声明 → 保持未知） | v2 `packages/agent-core-v2/src/kosong/model/catalogService.ts:619-668`（:657-667）；[v1] `packages/agent-core/src/session/provider-manager.ts:231-250` |
| 协议前缀推断目录：OpenAI（gpt-4o/4-turbo/4.1/4.5 → image_in；**o1/o3 推理系列 → image_in:false**；gpt-3.5 → text-only） | v2 `packages/agent-core-v2/src/kosong/provider/bases/openai/openai-common.ts:247-274`（接线 `openai-legacy.contrib.ts:30`、`openai-responses.contrib.ts:19`） |
| Anthropic（claude-3*/3.5*/3.7* → vision+tool；claude-opus-4/sonnet-4/haiku-4 → vision+tool+thinking）；Gemini（目录前缀内全模态） | v2 `packages/agent-core-v2/src/kosong/provider/bases/anthropic/anthropic.ts:1175-1211`；`packages/agent-core-v2/src/kosong/provider/bases/google-genai/google-genai.ts:862-900` |
| [v1] kosong models.dev 目录：`inputs.includes('image')` → `image_in`；env 默认能力 `['image_in','thinking']`；远端目录导入带 capabilities | [v1] `packages/kosong/src/catalog.ts:121-143`；`config/env-model.ts:27`；`services/modelCatalog/modelCatalogService.ts:145-177, 367-376` |
| 能力挂到 LLM 对象随请求携带 | [v1] `loop/llm.ts:104-110`（装配 `agent/index.ts:428-435`）；v2 经消息管线 ctx.model.capabilities（§4） |

## 4. 请求前适配（核心机制）

| 事实 | 证据 |
|---|---|
| 消息管线两步骤按序注册：videoResolution → mediaCapabilityAdaptation；适配对全部请求投影形态生效，在协议编码前执行 | v2 `packages/agent-core-v2/src/agent/messagePipeline/messagePipelineSteps.ts:44-96`（步骤 :70-88；调用点 `agent/llmRequester/llmRequesterService.ts:359-365`） |
| 未知能力放行 / 已知缺口投影：`projectMediaForModelCapability` 纯函数，零分配直返，只动投影不改历史 | v2 `packages/agent-core-v2/src/agent/media/mediaCapabilityProjector.ts:102-152`（`messagePipelineSteps.ts:80-86` 调用） |
| 占位文本统一构造器（capability_gap/size_limit/format_rejected 三成因；能力缺口带路径版含三条恢复指引：专用子代理 / 通用子代理+模型覆盖 / 告知用户换模型或转述） | v2 `packages/agent-core-v2/src/agent/messagePipeline/mediaPlaceholder.ts:55-87` |
| [v1] 请求时逐部件降级 `downgradeUnsupportedMedia`：未知/未定义能力原样透传（**与 rust 400 同构的场景敞口**）；已知缺口 → 短占位（无路径）或长占位（带路径 + 委托指引，与 v2 文案一致） | [v1] `packages/agent-core/src/agent/turn/kosong-llm.ts:298-377`（调用 `:128-135`；入口 gate `agent/turn/index.ts:135,148`） |
| 413 降级链：normal → media-degraded（剥较旧媒体）→ media-stripped（按首拒快照精确剥离）；图片格式错误 → 直接 media-stripped；本回合重放保持投影 | v2 `agent/llmRequester/llmRequesterService.ts:441-495, 555-564`；投影 `agent/contextProjector/contextProjectorService.ts:77-82, 311-350`；[v1] `loop/turn-step.ts:196-271`、`run-turn.ts:112-166`、投影 `agent/context/index.ts:501-513` |
| 格式类错误识别范围**不含**"模型不支持图片输入"类能力错误（能力型错误直接上抛） | [v1] `packages/kosong/src/errors.ts:157-226` |

## 5. 能力缺口注入与委托通道

| 事实 | 证据 |
|---|---|
| 注入器注册为上下文注入（`capability_gap`），新回合首步触发；检测缺口模态（含客户端替换出的"图片已省略/已留档"文本通知形态） | v2 `packages/agent-core-v2/src/agent/capabilityGap/capabilityGapInjection.ts:38-96` |
| 可行性检查（目录中存在支持缺口模态的模型才注入）+ 子代理点名（扫描档案、按默认模型能力过滤）+ 留档路径提取（正则匹配 saved to/at、Attached file 三种形态）+ 提示文本组装 | 同上 `:98-268`（`hasAlternativeModel` :184-195；`profilesForCapability` :202-216；`extractAttachmentPaths` :144-165；`buildHint` :237-268） |
| Agent 工具：输入含 `model` 覆盖参数（须为已配置模型，校验拒绝）；子代理模型裁定 = 显式 → 档案默认 → 调用者模型 | v2 `agent/tools/agent/agentTool.ts`（schema `agent.ts:19-65`；绑定 `session/subagent/configSection.ts:91-112, 247-259`）；[v1] `tools/builtin/collaboration/agent.ts:81-86`、`session/subagent-host.ts:434-438` |
| 工具说明的能力标注：可用模型清单带 `image_in → 'vision'` 标签；子代理档案描述带"Default model: xxx (supports vision, tool-use…)" | v2 `session/subagent/configSection.ts:125-230`（`CAPABILITY_LABELS` :224-230）；`agentTool.ts:551-566`；[v1] `agent/tool/index.ts:855-899` |
| ReadMediaFile 按能力门控注册（`image_in || video_in`）；无图片能力时调用返回错误并指引派发子代理；普通 Read 对图片硬拒绝 | v2 `agent/tools/read-media-file/readMediaFileTool.ts:46-47, 231-344, 496-514`；[v1] `agent/tool/index.ts:820-821`、`tools/builtin/file/read.ts:223, 282-285`（接口规格：`docs/specs/tool-calling-builtin-tools.md:508-559`） |
| 无内置 vision 专用子代理档案（内置仅 agent/coder/explore/plan，无默认模型绑定）；用户可自建（如 `~/.mirri-code/agents/media-reader.yaml`） | v2 `session/agentLifecycle/profile/profiles.ts:100-143`（:104-107 注释）；占位文案中的 "media-reader profile" 仅是示例（`mediaPlaceholder.ts:76`） |
| 可用模型清单以非空用途描述为收录门槛（无描述的已配置模型对 LLM 完全不可见）——能力标注的作用面受限 | v2 `session/subagent/configSection.ts:171-192`（`buildCuratedAliases`） |
| 视频投递三级回退：上传通道 → 内联 base64 → 留档路径文本标签（`<video path>`）；provider 无上传钩子且不支持内联时，读媒体工具对该类 provider 的视频按设计上抛"不支持视频投递" | v2 `agent/media/videoResolverService.ts:120-183`；`agent/tools/read-media-file/readMediaFileTool.ts:38-44`（`VideoUploadUnsupportedError`） |

| [v1][TUI] 提交前门控：带图且模型已知无 `image_in` → 阻止提交 + 提示；能力数据缺失放行 | `apps/mirri-code/src/tui/mirri-tui.ts:1116-1144`（`validateMediaCapabilities` / `supportsCurrentModelCapability`） |

## 6. 观测与旁路截面

| 事实 | 证据 |
|---|---|
| 钩子序列化把媒体替换为 `size_bytes` 标记 | [v1] `loop/turn-step.ts:577-589`、契约 `loop/types.ts:264-286` |
| 最近输入元数据把 image_url 渲染为 `[image]`；转录/标题派生同理 | [v1] `session/prompt-metadata.ts:49-50`；`agent/compaction/full.ts:735`；`docs/specs/session-persistence-requirements.md` US10.1（标题派生的附件占位词） |
| 转录→REST 回放映射 `image_url → image{source:{kind:'url'}}` | [v1] `services/message/message.ts:163-167` |
| [v1] MCP 工具输出可产图片部件（base64 / uri 透传） | [v1] `mcp/output.ts:90-148` |
| **媒体计入压缩压力：每媒体部件按固定 2000 token 估值**（`MEDIA_TOKEN_ESTIMATE`，与分辨率/字节无关；估算用于上下文窗口与压缩预算，非计费） | v2 `packages/agent-core-v2/src/kosong/contract/tokens.ts:73-84` |
| **v1 压缩文本化不带路径**：摘要请求被 413 拒绝后的重试输入中媒体替换为 `[image]`/`[audio]`/`[video]` 光标记（无留档路径）；压缩保留的头部/尾部用户消息媒体**原样保留**（不文本化） | [v1] `packages/agent-core/src/agent/compaction/full.ts:488, 734-766`（`replaceMediaPartsWithMarkers`）；保留语义 `agent/context/index.ts:242-358` |
| **v2 fullCompaction 对媒体零专项处理**（目录 grep `image_url/media/video_url/audio_url` 零命中）：被摘要头部媒体随摘要消失（摘要请求经消息管线按能力适配），最近尾部媒体保留；无文本化标记、无路径保留 | `packages/agent-core-v2/src/agent/fullCompaction/`（全目录）；摘要请求管线见 `agent/llmRequester/llmRequesterService.ts:359-365` |
| **v2 摘要请求仍命中请求层媒体降级链**：摘要请求（`requestKind: 'full_compaction'`）经 `llmRequester.start` 发出，413/格式拒绝的降级重试（media-degraded → media-stripped）在 catch 中无 source 门控、对操作请求同样生效（投影初始态的恢复标记才仅对 turn 请求）；压缩服务自身的溢出收缩/截断阶梯在其耗尽后接管 | `packages/agent-core-v2/src/agent/llmRequester/llmRequesterService.ts:441-495`（catch 无 `source.type === 'turn'` 门控）；`agent/fullCompaction/fullCompactionService.ts:640-699`（2026-09-16 审查批核验） |
| 请求级媒体数量/字节汇总记录（US7.5 目标）**不存在**：现有 `llm.request` wire 记录与 `usage.record` Op 均无媒体字段 | v2 wire journal 会话实录核验（2026-09-16）；`agent/usage/usageOps.ts:52-56` |

## 7. Rust 侧现状（2026-09-18 能力显式定义批后；权威记录见 rust-core-design 域）

> 本批（实施计划 [plan/rust-multimodal-input.md](../../plan/rust-multimodal-input.md)，commits `25f1c5ac..9756aadc` + 收尾批）把 rust 引擎对齐本域 TS 基线；下表为落地后的新事实（file:line 均为本批落地后工作树）。旧差距行（发送前剥离缺失、能力三源不统一、占位/留档通道缺失、注入器只提示不点名、ReadMediaFile 门控缺失、压缩未收口）已随落地删除——差距收口状态见 [multimodal-input-ROADMAP.md](multimodal-input-ROADMAP.md) §3。
>
> 本批（实施计划 [plan/rust-media-capability-explicit.md](../../plan/rust-media-capability-explicit.md)，commits `19b24ef2` + `007c33cc` + `a8ea113d`，2026-09-18）落实 ADR-003 修订：能力解析改优先级制（声明为王 → 名字推断兜底 → 全 false）并废除 unknown 第三态；注入器改目录级可行性 + 能力导向提示词；文本通知检测与引号式路径提取对齐 TS。受影响行（能力解析 / 模态剥离 / 注入器 / ReadMediaFile 门控）已同步刷新。models.dev 同步缺陷（rust core + web）另案修复，不在本域跟踪。

| 事实 | 证据 |
|---|---|
| 能力解析单一来源（优先级制，2026-09-18 批）：每个模型别名解析一次，经 `resolve_media_capability` 合并进适配器（`with_media_capability`）——声明含媒体词即声明为王（未列出模态一律 false，推断不翻案）；声明为空时名字推断兜底（`name_vision_support`）；两者皆无信号 → 全 false（ADR-003 修订：unknown 第三态已废除，未定义=不支持） | `rust/crates/mirri-core/src/model_catalog.rs:167`（`resolve_media_capability`）；`rust/crates/mirri-embedded/src/providers/config_factory.rs:131-201`（声明收集 `:131` + 解析 `:141` + 合并，各 provider 一致）；适配器入口 `providers/anthropic.rs:129` 等 |
| 入口留档（无条件、阈值无关）：`message.send` 主路径把内联 data-URI 图片原件写入内容寻址 blob 存储（home = 会话目录，即 `<会话目录>/blobs/{sha256}.{ext}`，ext 按 MIME 映射——TS `media-originals` 命名对齐（png/jpg/gif/webp/bmp/tif，未知 MIME 保持裸哈希），经注入 `SyncFs`；**会话域内存储**，ADR-007 全局内容寻址根 `<数据根>/blobs` 为已裁决待迁移目标，见持久化域 ROADMAP 开放项 11），并把绝对路径回写进部件隐藏 `id` 通道；已有 `id` 幂等跳过，remote URL / 引擎内 ref 透传不留档；写入失败 fail-open（部件保持无 `id`）；按哈希读回（`blobref` 解析与重放再水化）探测历史拼写——存量裸哈希文件名仍可读（2026-09-16 扩展名修复，用户报告：TS desktop 留档带 `.png`、rust 无扩展名导致代理与系统工具无法从路径推断类型） | 命名映射与探测 `rust/crates/mirri-core/src/blob.rs`（`blob_extension`/`blob_file_name`/`blob_candidate_paths`，写入点 `write_blob`/`ensure_blob_path`，读点 `read_blob`/`load_blob_ref_url`）；用例 `blob.rs` tests（扩展名追加/映射逐项/存量裸哈希回读）；接线与 fail-open 证据同上行 |
| 留档路径沙箱准入（US5.1 可用性 + US7.3 校验前提，2026-09-16 缺陷修复）：`message.send` / steer 的每个落盘点把 blob 存储根（`{home}/blobs`）注册为会话沙箱 additional dir——位置无关（ADR-007 迁移到全局根后同一准入继续成立）；注册前物化目录（`blobs_dir`）保证沙箱 canonicalize 与校验时同形（符号链接场景 macOS `/var` vs `/private/var`）；会话内全部 agent（含子代理）的读文件/读媒体工具经标准工作区校验放行留档原件。修复前缺陷：注入 home 的部署形态（`<home>/v3_sessions/.../blobs`）下，留档路径在工作区根之外，被 path-traversal 校验拒绝，委托链路断头 | 准入 `rust/crates/mirri-embedded/src/embedded_server/message_handlers.rs:64`（`admit_blob_store_into_sandbox`）+ 三调用点 `:666, 1375, 1458`；物化 `rust/crates/mirri-core/src/blob.rs:241`（`blobs_dir`）；集成用例 `embedded_server/tests.rs:21500`（`should_admit_archived_media_originals_into_session_sandbox_when_media_is_ingested`，注入 home + 工作区外持久化根形态，含读媒体工具回读断言） |
| 入口压缩（视图关注点，与留档正交）：超预算（3.75MB / 2000px 长边 / JPEG q85）的内联图片压缩后替换消息内视图，原件仍在留档 `id`；压缩失败 fail-open | `rust/crates/mirri-core/src/media_ingest.rs:14-16, 33-66`；压缩实现 `image_pipeline.rs:242`（`compress_media`） |
| 请求前模态剥离（US4.1/4.2）：`MediaCapabilityAdapter` 纯投影——不支持模态（含未定义收敛，ADR-003 修订）替换为统一占位构造器文本（capability_gap 成因带留档路径 + 三条恢复指引：专用子代理 / 通用子代理+模型覆盖 / 告知用户），适配器无 unknown 早退分支（unknown 字段已随 2026-09-18 批删除）；管线每步重建、对全部请求投影形态（normal/degraded/stripped/strict）在协议编码前生效；context memory 不改写（ADR-002） | 适配器 `rust/crates/mirri-core/src/message_pipeline.rs:159-194`；占位构造器 `rust/crates/mirri-core/src/media_placeholder.rs:30-54`（`PlaceholderCause`/`build_media_placeholder`）；管线接线 `rust/crates/mirri-core/src/loop_engine/turn_step.rs:169-172, 247-262` |
| 413/格式降级链占位语义对齐 TS 基线：413 → Degraded → Stripped → strict、格式错误直接 Stripped；降级重试请求同样过媒体管线（无请求形态旁路）；本回合 sticky 投影（首拒后所有后续步骤沿用降级/剥离投影），剥离集合由能力 + 部件确定性推导——与 TS "按首拒快照精确剥离" 语义一致（2026-09-16 收尾核验：既有 stripped/sticky 用例族全绿）；占位成因分流（2026-09-16 终审修复波）：格式错误链置位共享 sticky 标志 → 剥离占位为 format_rejected 成因（含转换指引，US6.2），413 链保持体积超限成因（US6.1），成因随本回合后续 sticky 剥离重放保持 | 链 `rust/crates/mirri-core/src/loop_engine/turn_step.rs:391`（`try_media_fallback`）；sticky 传播 `run_turn.rs:202-203`；标志共享 `loop_engine/mod.rs`（`MessageBuilders::from_shared_memory` / `format_rejected_flag`）；投影成因 `context_memory/projector.rs`（`replace_media_with_text_placeholder`）；用例族 `tests/loop_engine_tests.rs`（413/strict/stripped 四用例 + 投影切换 wire 标记用例 + 格式链/413 占位成因两用例） |
| 能力缺口注入器（US5.2）生产激活：TurnFlow 可选装配 `profile_manager` + `model_catalog`；主路径 `get_or_create_turn_flow` 同时接线 server 级档案管理器与 `ConfigModelCatalog`（构造点 `turn_flow/mod.rs:1425-1428`：档案默认模型能力经目录解析 `profile_infos_from_manager` + 目录能力快照 `catalog_capabilities`），档案变更按回合重取；注入器语义（2026-09-18 批）：可行性扫描查**模型目录快照**（空快照 permissive——standalone 流无法验证不压制），无任何可行模型时不注入（"告知用户"由占位文本第③条兜底）；主文案能力导向——指向 Agent tool description 的 "Available models" 清单 + 子代理 `model` 参数由模型自判，匹配的 profile 名降为括注便利信息；检测/提取对齐 TS：`ModalityGap::Image.detect` 识别客户端文本通知（`[image omitted:` / `original image has been saved to` / `Attached file` + 图片扩展名），`extract_attachment_paths` 支持引号式路径（`saved at "/path"`）与 em-dash 终止（`bytes): /path —` 形态）；独立流（task/cron/subagent host）保持保守 `None`（不注入） | 注入器 `rust/crates/mirri-core/src/capability_gap.rs:179`（`profile_infos_from_manager`）、`:205`（`catalog_capabilities`）、`:295`（`build_hint`）、`:94`（`image_notice_in_text`）、`:386`（`extract_attachment_paths`）；接线 `turn_flow/mod.rs:1425-1428`；注册 `turn_flow/mod.rs:1281-1288`；setter `turn_flow/mod.rs:571, 578`；主路径接线 `rust/crates/mirri-embedded/src/embedded_server/mod.rs:3518-3528`；集成用例 `embedded_server/tests.rs`（`should_inject_capability_gap_hint_on_the_main_message_send_path`） |
| ReadMediaFile 按能力门控（US4.3 第一条，ADR-003 修订）：无 image/video 输入声明的模型（含未定义，一律按不支持收敛），请求工具表剔除 `ReadMediaFile`；unknown 保留分支已随第三态废除删除（逐 step 求值，mid-turn 模型切换立即生效） | `rust/crates/mirri-core/src/loop_engine/turn_step.rs:228-238`；用例 `tests/loop_engine_tests.rs:2146`（`should_hide_read_media_file_from_models_without_media_input`） |
| 仍留存的登记项（范围外，另立归属）：ReadMediaFile 视频支持/参数规格（工具调用域 T8a）；E8 媒体在场经济性（压缩退场/US7.5 观测/file-id P-8）；Audio/Video 部件 `id` 结构化（wire 裸字符串兼容）。原登记的"ACP 直入通道的 ingest 接线（按需补）"已核验关闭（2026-09-16 终审）：ACP prompt 经 `rust/crates/mirri-acp/src/acp_handler.rs:654` 路由至引擎 `message.send`，已被主路径 ingest（入口留档 + 压缩）覆盖，无需另接 | `plan/rust-tool-calling.md:3057-3063`；压缩域 ROADMAP 开放项 25；范围登记见 [plan/rust-multimodal-input.md](../../plan/rust-multimodal-input.md) 范围外表 |
| **工具结果媒体部件序列化**：Chat Completions / Kimi / Responses 方言把工具结果中的 image_url 以 "Attached media from tool result:" 回挂为后续 user 消息，audio/video 留省略占位；Anthropic 方言把 image block 原生嵌入 `tool_result.content` block 数组；Google 方言 functionResponse 已带媒体部件。修复前缺陷：工具结果图片在 `role:"tool"` 序列化时被静默丢弃，模型只见 `<image path>` 文本（v3 会话 session-f461fee8 诊断：同一图同模型，v2 正确 v3 幻觉） | openai.rs `convert_messages`+`flush_tool_result_media`（332）；tool_result_media.rs（30）；anthropic.rs `user_tool_result_message`（503）；openai_responses.rs（397） |

## 8. 相邻域文档覆盖现状矩阵

本域为新域，无旧文档可回归；下表为**相邻域文档对本域内容的覆盖现状**（事实盘点；能力面归属状态见 ROADMAP §1）：

| 能力 | TS 实际 | 相邻文档现状 | 归属 |
|---|---|---|---|
| ReadMediaFile 接口规格与能力门控 | 有 | `tool-calling-builtin-tools.md:508-559` 已覆盖 | 工具调用域（本域引用其规格） |
| wire ContentPart 多模态形状 | 有 | `rust-core-design/01-protocol-unification.md:1124-1144` 已覆盖（协议层） | rust 内核域（本域引用） |
| 图片附件旁路 T11（Anthropic fileref 物化） | 有 | `rust-core-design/ROADMAP.md:122-156` + `plans/07-followup-completion.md:273-289` 已覆盖（rust 侧） | rust 内核域（本域引用） |
| **请求前模态剥离 + 占位文本 + 恢复指引** | 有 | **无任何文档覆盖** | 本域 requirements US4 / architecture §5（本次记录） |
| **能力缺口注入器（可行性检查/点名/路径提取）** | 有 | 无文档覆盖 | 本域 US5.2 / architecture §6（本次记录） |
| **提示式委托通道（模型覆盖参数 + 能力标注）** | 有 | `agent-profile-settings-page-context.md:75` 仅一句提及 | 本域 US5.3 / architecture §7（本次记录） |
| 入口留档（无条件持久化原件 + `id` 路径） | 有 | `session-persistence-context.md:112` 仅提及 media-originals 目录 | 本域 US2 / architecture §3.3（本次记录） |
| web 上传链路与无预检现状 | 有 | `session-management-context.md:12` 一句提及 | 本域 §2.3 |
| 413/格式降级链 | 有 | `non-functional-*`（NF3 步级重试）未展开媒体细节 | 本域 US6 / architecture §5.3（本次记录） |

## 9. 遗留盘点未决项

- v1 `contentToCoreParts` 对 `source.kind='file'` 的解析在部署层（agent-core RPC 层跳过该分支），与 v2 kap-server 行为的差异面未逐项核对（本域以 v2 为基线记录）。
- web 端 `capabilities` 字符串数组与 protocol 结构化 `capability` 布尔两套形态并存，web 未消费后者（§2.3）；对基线的影响仅限展示层。
- 桌面端（apps/mirri-desktop）注入的"图片已留档"文本通知形态由注入器 detect 分支反推（`capabilityGapInjection.ts:42-56`），其产生端代码未逐行盘点。


## 11. 工作区文件引用（E9）现状侦察（2026-10-26 快照）

- **web/桌面 `@` 是纯文本补全**：输入框触发后从会话文件搜索接口取候选（限 20 条），选中后把候选路径替换为**裸路径文本**（`@` 字符被移除，无引号处理，含空格路径不可输入）；候选不含文件夹、无路径分段下钻、无命中高亮（服务端返回的匹配区间被前端丢弃）；无请求取消，仅固定防抖；草稿会话（未创建）下直接返回空。
- **终端为生产级先例**：`@` 补全列文件+文件夹、路径分段下钻、外部模糊搜索工具下推、选中保留 `@path` 形态（含空格加引号、目录尾斜杠），有请求取消与降级扫描上限；注入侧与 web 同为纯文本。
- **rust 引擎的检索是同步全量遍历**：无忽略规则（仅跳过 `.git` 目录）、固定深度上限、纯大小写不敏感子串匹配（两档得分）、只收文件不收目录、匹配区间恒为空；每次按键重复遍历，无缓存。
- **服务端已备 session-less 路由但未被使用**：工作区级文件搜索路由已存在（为 `@` 草稿态设计），web 客户端实际仍打会话级路由，且无会话时提前返回空——设计意图与代码行为不一致。
- **引擎侧无任何 `@` 引用解析**：`@path` 以普通文本进入消息，模型靠读文件工具自行读取；无结构化引用对象、无注入管线。
- **输入框基础设施**：web 输入框为富文本编辑器，已有技能引用芯片（inline chip）先例，可承载文件引用芯片；附件芯片在输入行上方独立渲染。
