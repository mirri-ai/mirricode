# Rust v3 快照/重渲染链路审计(session-a55fd3f3)

- **日期**:2026-09-13
- **触发**:用户重开会话 `session-a55fd3f3-08e9-43a7-ad6d-193ee21c7556` 时,工具调用显示结果不正确。
- **数据源**:该会话 wire.jsonl / 隔离 home 下 `session.snapshot` 实测 / mirri-server、mirri-web 源码。
- **状态**:三个问题已定位到代码级根因,**未修复**(待用户决策)。

## 问题 R1(主因):快照/消息列表丢失工具调用关联信息

**现象**:live 渲染正常,重新渲染(re-open/刷新)后工具调用卡片显示错误。

**根因(双侧断裂)**:
1. **服务端**:`rust/crates/mirri-server/src/rpc.rs:1174` `message_content_to_json` 只序列化 `Message.content`,丢弃 `Message.tool_calls`(assistant 侧:call id/name/args)与 `Message.tool_call_id`(tool 侧:结果与调用的关联)。mirri-core 的 context memory 实际存储了这些字段,是 rpc 序列化层丢的。实测快照:9 条消息中 3 条 `role=tool` 只有裸文本,assistant 消息只剩 `"\n\n"`。
2. **客户端**:`apps/mirri-web/src/api/daemon/ws-rpc-api.ts` `rpcContentToAppContent` 只映射 `text` part,其余降级 `{type:'unknown'}`;App 层已定义 `toolUse`/`toolResult` 内容类型(`api/types.ts:213-214`)但未接线。

**协议权威**:TS `packages/agent-core-v2/src/agent/contextMemory/protocolMessage.ts`——工具调用 = content 中的 `tool_use` part(`tool_call_id/tool_name/input`);工具结果 = `tool_result` part(`tool_call_id/output/is_error`)。Rust 快照当前输出过不了 TS `messageSchema` 校验。

**影响面**:`session.snapshot` 与 `message.list` 共用 `message_list_row`,两个读面同病。

## 问题 R2:Glob `*` 递归匹配(语义错误)

**现象**:该会话中 `Glob pattern:"*"` 返回了 100+ 条深层路径(如 `skills/skills/productivity/...`)并触发截断;顶层只有 3 个条目。

**根因**:`rust/crates/mirri-os/src/glob.rs` 用 globset 匹配相对路径,未设 `literal_separator(true)`,globset 的 `*` 默认可跨 `/`——等价于 `**`。工具自身文档以 `**/*.rs` 举例递归,自证 `*` 应为非递归。

## 问题 R3:游标补发告警误报

**现象**:`WARN ws_rpc: subscribe cursor bypassed replay (evicted events or epoch mismatch); client may need resync`。

**根因**:`rust/crates/mirri-server/src/ws_rpc.rs:911` 告警条件为 `replay.is_empty() && cursor.is_some()`,把"游标已在尾部、无新事件可补"的正常情况也判为异常。应仅在 `cursor.seq < tail_seq` 且补发为空(真逐出/epoch 不匹配)时 warn。

## 本轮已确认正常的部分

上一轮修复在该会话全部生效:workDir 正确、Bash 纯文本结果、wire time、`updated_at`/`last_prompt`/version 3。

## 待调查(本文件追加)

- [x] 重渲染除工具调用外的其他差距(见下方「四、扩展差距分析」)
- [x] 保证 web 渲染接线完整性的机制(见「五、接线保障机制现状」)
- [x] 相关接口设计文档的位置(见「六、设计文档索引」)

## 四、扩展差距分析(live vs 快照重渲染,2026-09-13)

结论:**不止工具调用**。逐内容类型对比(web live 事件投影 agentEventProjector.ts vs session.snapshot 重建):

| 内容类型 | 状态 | 说明(双侧位置见代码) |
|---|---|---|
| assistant 文本 | ✅ | |
| **thinking 思考块** | ❌ 双层缺失 | rust `Think` part 序列化为 `{"type":"think","text"}`(与 TS 协议 `{"type":"thinking","thinking"}` 判别符+字段名双不符);web `rpcContentToAppContent` 只映射 text,thinking 重渲染即丢 |
| **tool_use / tool_result** | ❌ | 即 R1;另:快照 tool 行的裸文本会被 `messagesToTurns.ts:999` 当 assistant 正文吸收(输出泄漏进气泡) |
| **工具媒体结果**(图片等) | ❌ | ContentPart 序列化形状与 App 期望不符→unknown→丢 |
| **用户图片/文件附件** | ⚠️ 部分 | rust 走 `File{data}` 内联形态时重渲染全丢,仅文本通知可恢复 |
| **usage 四桶** | ⚠️ | 快照 session view 无 usage 字段;靠 session.list 行+`session.status` 旁路恢复;`turnCount` 仅投影器内存,刷新归零 |
| **前台子代理/swarm 卡** | ❌ | 快照 `subagents` 服务端恒空(rpc.rs:1718),web 也硬编码 `[]`;只有 detached 任务能靠 `task.list` 轮询恢复;前台子代理的流式进度不可恢复 |
| 后台任务(detached) | ✅ 旁路 | task.list 恢复 |
| goal / 模式开关 / 审批 / 提问 | ✅ 旁路或快照字段 | approval 行 turn_id 类型瑕疵(TS 声明 number,rust 发 `turn-<uuid>` 字符串) |
| 消息 createdAt | ⚠️ | 推导值(会话创建时间+position),非真实时间 |
| 气泡合并辅助字段 | ⚠️ | 行里无 promptId/status/durationMs,turn 边界靠启发式 |
| 压缩 token 统计 | ⚠️ | 分割线可恢复,tokensBefore/After 客户端标记丢失 |
| in-flight turn(重连中步) | ✅ | seedInFlight + 偏移对齐正常 |

与既有差距矩阵对照:`docs/specs/conversation-rendering-context.md` 的 G1~G14 已登记 subagents 恒空(G3)、agent.status.updated 占位(G7,影响 usage 展示)、压缩后全量转录(G14)等;**R1 工具关联丢失与 thinking/媒体形状缺失不在矩阵中,属新发现差距,应补登。**

## 五、接线保障机制现状(事实)

分层现状:
1. **方法面门禁**:mirri-proto registry(事实源)+ rust `cross_backend_contract`(serve 面 189 方法有应答;WireShape 是 server vs embedded 自指对拍,非 schema 驱动)。
2. **跨语言契约**:node-sdk `rust-engine-contract-e2e`(199 个 embedded 方法过 klient zod schema + 提交形状快照,`UPDATE_SNAPSHOTS` 流程)——**但 `session.snapshot` 是 serve-only,被排除在外,零 schema 校验覆盖**。
3. **渲染管线单测**:web projector/reducer/mappers 测试用**手写 wire mock**,与真实 rust 输出无对账;两个真 `mirri serve` e2e(backend-switch/desktop-bridge)存在但不做形状校验。
4. **zod v4 `z.toJSONSchema` 导出基建已存在**(kap-server schema.ts + asyncapi.ts),但没有任何 Rust 测试消费。

结构性缺口:**服务端 session.snapshot/message.list 的真实输出,没有任何一层 schema 驱动的契约校验**——本次 R1/R2 类 bug(输出过不了 TS messageSchema)正是从这个缺口漏过去的。

## 六、设计文档索引

- `docs/specs/conversation-rendering-architecture.md` —— 对话渲染域设计唯一事实源(快照六要素、ADR-001~006、IM 式同步模型),2026-09-12,与代码同步。
- `docs/specs/conversation-rendering-context.md` —— 现状盘点 + G1~G14 差距矩阵(file:line 锚点)。
- `docs/specs/conversation-rendering-ROADMAP.md` —— CR1/2/3/7 已落地,CR4/5/9/10/11 待排(其中 CR11"易失流偏移对齐"即渲染接线对齐窗口,无测试保障)。
- `docs/specs/rust-core-design/01-protocol-unification.md` §6.1/§9.4 —— session.snapshot JSON Schema 定义 + 快照重建规格(快照 cursor=日志尾部、订阅原子消除竞态)。
- `packages/kap-server/src/protocol/rest-snapshot.ts` + `protocolMessage.ts` —— v2 契约的代码级事实源(zod schema,文件头即文档)。
- 无独立 REST 协议设计文档;REST 契约以 zod schema 本身为文档载体。

## 七、渲染契约修复批验收记录(2026-09-13,CR12)

修复落地(本地分支 feat/rust-tuning3,仅本地提交):

| 项 | 修复 | 守护 |
|---|---|---|
| R1 服务端 | `rpc.rs` `message_content_to_protocol_json`:tool_use/tool_result/thinking/image/video/file 投影 TS 协议形状,磁盘拼写不动 | rpc 行构造 4+ 单测(红→绿) |
| R1 前端 | `ws-rpc-api.ts` `rpcContentToAppContent` 委托 `toAppMessageContent` 共享映射 | ws-rpc-models 7 测(红→绿) |
| R2 | Glob `literal_separator(true)` | mirri-os 新测试(红→绿) |
| R3 | `cursor_replay_bypassed` 纯函数条件收紧 | ws_rpc 3 测(红→绿) |
| G19(验收期发现) | `finalize_step` ThinkPart 先于 ContentPart 落 wire | `should_record_the_think_part_before_the_text_part_when_llm_returns_both`(红→绿) |
| 附带修复 | 注入类 system-reminder 只进内存不落 wire → replay 丢消息 | 重放等价不变量测试暴露(红→绿) |
| 三层保障 | ①rpc 单测 ②`cross_backend_contract` live≡冷恢复快照不变量 ③`snapshot-contract.e2e` 真 serve 过 `sessionSnapshotResponseSchema`(zod,content part 走共享 union;in_flight_turn/session 视图/行信封按 ADR-009 豁免) | e2e 红→绿已验证(旧二进制下 zod 拒绝 tool 行——判别符不匹配) |

浏览器实际验收(隔离 home + 用户会话副本 + FakeProviderServer + 真实 `mirri serve` + vite + Playwright):

1. **旧会话副本重渲染**(session-a55fd3f3,原始 turn 含 ls/Find/git):工具卡片完整——名称(Run/Find)、参数、status、结果输出全部在卡片内;**无正文泄漏**(assistant 气泡仅含回答正文);会话列表标题+相对时间正常;大纲导航正常。
2. **新会话完整 turn**(Manual 权限):审批弹窗(Approve/Approve for session/Always allow/Reject)走通;live 渲染工具卡 + thinking 块 + 正文正常。
3. **刷新重渲染新会话**:内容完整;**thinking 与正文顺序与 live 一致**(G19 修复后;修复前重渲染 thinking 排到正文后,live 在前——已修)。

验收环境注意:`/api/v1/auth` 的 `ready` 要求 config.toml 顶层 `default_model`(TOML 顶层键不能放在表节之后);e2e 清理顺序必须先停 serve 再关 fake provider(在途 SSE 流会挂住 fastify close → afterAll 超时 → 进程泄漏)。

## 八、reviewer 审查跟进(2026-09-13)

独立审查(Major×2/Minor×5/Nit×6)后本批修复:
- M1:file part 投影三分支重做(带 file_id → 协议 file part,media_type 兜底 application/octet-stream;内联图片 data URI → image base64 source;内联非图片 → `[file:name]` 文本标记),全部落在协议 union 内,5 个新单测(红→绿)。
- M2:tool_call_id 缺失的 tool 行降级为纯文本形态(不再产出空 id 的非法 tool_result)。
- m1:is_error 从 ContextMessage 穿进投影(tool_result 带 is_error:true,ADR-007 补齐)。
- m5:e2e 补 think-before-text 顺序断言(G19 在真 serve 层的守护)。
- 附带发现并修复:ACP session/load 重放会把落盘的 tools_added reminder 当 user_message_chunk 漏出 → 按 origin.kind=injection 过滤(mirri-acp conformance 红→绿,46 passed)。
- 登记不实施(m2 残余/m3):工具结果注入与压缩后提醒的重放守护夹具、游标部分驱逐盲区 → context.md 已裁决后续项;ADR-008 顺序措辞精化为"桶序近似"(m4)。

全量验证:cargo test --workspace 全绿;pnpm -C apps/mirri-web test 全绿(注:web e2e 与 rust workspace 测试并行跑会因资源竞争出现 30s 超时假红,分开跑即绿——CI 本就分 job)。
