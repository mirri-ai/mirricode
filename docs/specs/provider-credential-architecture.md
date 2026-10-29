# Provider 凭据（provider-credential）架构文档

> 本文档描述 provider 凭据域的实现机制、接口形状与数据流；动机保持抽象（不含代码事实），现状依据与竞品机制引用见 [provider-credential-context.md](provider-credential-context.md)。需求见 [provider-credential-requirements.md](provider-credential-requirements.md)（下称「需求文档」，裁决编号 D-M* 沿用其 §8）。
> 运行目标为 Rust core + web/ACP；TS core 退役线不承载本域。

## 0. 架构总览

```
├─ 消费层 ──────────────────────────────────────────────────────┐
│ CLI 凭据子命令（列表/设置/移除/回读/迁移；库函数直连，            │
│ 不要求 server 在跑——ADR-002）                                    │
│ web 设置页（供应商密钥编辑 = model-settings US-6 的目标态）      │
├─ 服务层（embedded server 内，进程内 RPC）───────────────────────┤
│ CredentialService（条目读写 · 解析链 · 状态视图）              │
│ ExternalCredentialSource（外部引用 → 现场取用；Azure CLI 首个） │
│ MigrationRunner（config.toml 密钥 → 凭据库，一次性幂等）         │
├─ 解析集成层 ────────────────────────────────────────────────────┤
│ ConfigAwareLlmFactory 解析链前置一级「凭据库该供应商凭据」       │
│ 401/403 分类（既有 provider.auth_error）→ 状态视图 D.1          │
├─ 存储层 ────────────────────────────────────────────────────────┤
│ credentials store（<mirri-home>/credentials/，明文 JSON，        │
│  原子写 + 进程间文件锁；见 §3）                                  │
└──────────────────────────────────────────────────────────────┘
```

**分层动机**（抽象问题）：密钥是横切数据——web 设置页、命令行、会话运行时、错误呈现都要消费同一份；若各入口自建存储会立刻出现「改了配置文件但会话还在用旧密钥」的漂移。因此存储收敛为单一文件库，全部变更收敛为进程内单一服务，入口（web RPC / CLI 直连）只是呈现与参数化差异——同一套操作语义在两个面上等价（SDR-003：web 第一、CLI 辅助、TUI 后置）。

**与既有件的关系**：本域不新建服务进程、不新建传输——CLI 子命令以**库函数直连**凭据库（同一 CredentialService 代码，文件锁协调，见 §3 与 ADR-002）；web 走既有 WS RPC；解析集成点在既有 `ConfigAwareLlmFactory` 的密钥解析链之前插入一级（见 §5）。

## 1. 数据模型（存储格式）

### 1.1 凭据库文件

位置：`<mirri-home>/credentials/credentials.json`（单文件库；目录 0700、文件 0600，沿用既有私目录基建）。

形状（JSON，进程内 camelCase、落盘 snake_case，与 config.toml 本地惯例一致）：

```jsonc
{
  "version": 1,
  "entries": [
    {
      "id": "cred_00000001",            // 锁内重读快照后分配的递增序号（§2.1）
      "provider_id": "openai",           // 归属供应商（= config [providers.<id>] 的键）；恰一条
      "value": {
        "type": "key",                   // key | external | oauth(D-M4)
        "key": "sk-..."                  // type=key：密钥本体（按 encoding 声明的形态存放）
      },
      "encoding": { "cipher": "plaintext" },   // 按条目的加密形态声明（ADR-008；当前唯一合法值）
      "time_created": 1770000000000,
      "time_updated": 1770000000000
    },
    {
      "id": "cred_00000002",
      "provider_id": "azure",
      "value": {
        "type": "external",
        "method_id": "azure-cli",        // 外部来源标识（§4）
        "metadata": { "resource_name": "my-models" }  // 来源参数，随引用存
      },
      "encoding": { "cipher": "plaintext" },
      "time_created": 1770000000001,
      "time_updated": 1770000000001
    }
  ]
}
```

约束由服务层写路径强制，不依赖磁盘不变量：

- 每 provider 恰一条（`set` 为整体覆盖，无激活标记——SDR-004/D-M7）；
- `provider_id` 必须是 config 中已登记的 provider（删除供应商时服务层级联清其条目——需求 A.5b；外部改配置产生的孤儿条目在读取时跳过并告警）；
- `version` 供未来格式演进（读到更高版本 = 按损坏容错 F.4 处理并告警，不猜格式）；条目数组的形态保留了对未来放开多条的兼容（提案 P-5，无需格式迁移）；
- `encoding` 是**按条目**的加密形态声明（ADR-008）：当前唯一合法值为 `{"cipher": "plaintext"}`——把「未加密」从隐式默认变成显式字段，使未来的加密演进（分 OS 后端、滚动迁移、算法轮换）成为「新增合法 cipher 取值 + 逐条目重写」而非整库格式变更；读取方遇到未知 cipher 按损坏容错处理（F.4，不猜解密方式）；每条记录独立声明使混合态（部分已迁移）天然合法。

### 1.2 值类型（D-M4）

| 类型 | 载荷 | 密钥本体在库中 | 用途 |
|---|---|---|---|
| `key` | `key: string` | 在（明文） | 普通 API key |
| `external` | `method_id: string` + `metadata: map` | **不在** | 云 CLI 等本机来源的引用（C.1） |
| `oauth` | access/refresh/expires/method_id | 在（保留槽位） | 托管登录令牌；**本批不实现**，P-2 启用 |

值类型是**封闭联合**（tagged union，按 `type` 判别）；新增类型 = 格式演进（version bump）。

## 2. 服务层（CredentialService）

进程内单例，`mirri-embedded` 中的新模块，与既有 config/provider handlers 平级。接口面（Rust trait，非 wire 契约）：

```text
all() -> Vec<Entry>                                  // 全部条目（含明文，仅服务层内部）
list(provider_id) -> Option<EntryView>              // 条目视图（type/时间，无明文）
get(provider_id) -> Option<Entry>                    // 服务层内部明文访问（按 provider 定位）
set(provider_id, value) -> Entry                     // 新建或覆盖（A.3；唯一写入口）
remove(provider_id) -> void                          // A.5
resolve(provider_id) -> Option<Value>                // 解析链第一级（§5.1）
reveal(provider_id) -> String                        // B.5 回读唯一出口
status(provider_id) -> CredentialStatus              // normal | needs_reconfigure（D-M5 运行时视图）
report_failure(provider_id, reason)                  // 401/403 分类上报（§6）
```

写路径统一走「读快照 → 该 provider 条目整体覆盖（SDR-004：恰一条，无激活标记）→ 序列化 → 原子写」，复用 `mirri-persistence::write_file_atomically`（0600 临时文件 + rename，config.toml 同一协议）。跨进程互斥沿用会话索引注册表的锁约定（`create_new` 锁文件 + 按修改时间判陈旧接管 + 等待超时，天然跨平台——`mirri-session-index` 的 `RegistryLock` 模式）：锁文件 `credentials.json.lock`，锁内**重读最新快照**后分配 id 与提交变更。每次变更后发内部事件 `credential.updated`（payload 只含 provider_id，无明文——A.7/F.5）；迁移完成发 `credential.migrated`（E.5/F.5）。

### 2.1 并发、可见性与崩溃安全（F.3/F.4）

- **写**：锁内「重读 → 修改 → 原子写」，多进程并发时后写者基于最新快照，无丢失更新；
- **读（解析）**：**每次解析直读文件**（条目量级个位数到几十，直读成本可忽略）——因此 CLI 直写后，运行中 server 的下一次模型调用即见新值（V8 后半），不依赖事件传播，也不需要文件监听；
- **读（展示）**：列表/状态按需直读快照，进程内缓存仅为单次请求内复用；
- **崩溃**：临时文件残留由下次写覆盖；锁残留按修改时间判陈旧后接管；半条记录不可能出现（写即全量）；
- **损坏**：JSON 解析失败 / `version` 高于已知 / 数组元素不合形状 → 损坏文件改名留存（`.corrupt` 后缀，供手工抢救）后按空库处理 + 诊断警告（F.4）。

## 3. CLI 面（Epic B——辅助面：回读 / 无头 / 直连）

`mirri` 新增凭据子命令组（挂载在既有 clap `Commands` 枚举；与 `serve`/`web`/`doctor` 平级）。定位：辅助面——承载密钥回读（唯一明文出口）、无头/脚本参数化操作、服务未运行时的直连管理；日常管理在 web 设置页（Epic G）。

```text
mirri credential list
mirri credential set <provider-id>                     # 密钥从 stdin/TTY 读，不回显；已有则覆盖（A.3）
mirri credential remove <provider-id>
mirri credential show <provider-id>                    # B.5 回读（key 条目出明文；external 出引用配置）
mirri credential migrate [--dry-run]                    # 手动触发迁移（E.5；幂等含补偿清除）
```

- **直连模式**：凭据子命令不要求 server 在跑——直接对本地文件库操作（同一 CredentialService 逻辑以库函数形态被 CLI 引用，锁协议相同）；server 在跑时同样安全（F.3 的锁 + 解析直读即为此设计）。CLI 直连变更不产生进程内事件——对运行中 server 的可见性由 §2.1 的「解析直读」保证。
- **show 的警示协议**：stdout 为 TTY 时，明文前先向 stderr 输出一行警示（`Warning: output contains a secret in plaintext`）；管道/重定向时只出明文（脚本可消费）。external 条目输出引用配置与「请到来源工具核对」提示，不走警示协议（无密钥本体）。
- **指认解析**：`<provider-id>` 解析失败时列出全部有凭据的供应商（B.7）。
- **状态列（架构 §6 的直连口径）**：`list` 输出增加 STATUS 列——直连模式无运行时数据，如实打印 `unknown`（不显示正常、不省略列）；external 条目附 AZ 在场探测（`external_available`，不 spawn 进程）。
- **add 的输入**：TTY 下用 rpassword 风格无回显输入并要求确认；非 TTY 从 stdin 读一行。密钥**永不**出现在 argv（shell history 泄漏面）。

## 4. 外部凭据来源（Epic C）

`ExternalCredentialSource` 是服务层内的策略对象，按 `method_id` 注册：

```text
trait ExternalCredentialSource {
    fn method_id(&self) -> &'static str;          // "azure-cli"
    fn is_available(&self) -> bool;                // which az / 可执行
    fn obtain(&self, metadata: &Map) -> Result<SecretMaterial>;
    fn describe_unavailable(&self) -> String;      // C.3 的行动指引文案
}
```

- **取用时机**：`resolve()` 命中 external 条目时**现场调用** `obtain()`（本批不缓存 token——云 CLI 自己维护缓存；对 Azure CLI 即 `az account get-access-token --resource <resource_name>`，metadata 携带 resource）；失败原样上抛为「来源不可用」域错误（含 `describe_unavailable` 文案），不伪装成密钥错误。
- **可用性探测**：`is_available()` 供 list/状态展示（外部条目显示「来源在场/缺席」），不阻塞解析——缺席在调用现场才报（避免启动期探测抖动）。
- **扩展**：P-4 新来源 = 新 trait 实现 + 注册，不动数据模型。

## 5. 运行时解析集成

### 5.1 解析链（D-M3）

`ConfigAwareLlmFactory` 构造 provider 适配器时的密钥解析，在既有三级链**之前**插入一级，且**配置文件的 `api_key` 字段层从链中移除**（D-M3 裁决——字段层唯一安全网是启动期自动迁移）：

```text
① 凭据库：该 provider 的凭据条目（key → 本体；external → 现场取用）——每次解析直读（§2.1）
② 既有链保留两级：provider env 子表 → 按类型约定的环境变量（A.6）
③ 都无 → 既有「provider 无凭据」错误（措辞不变）
```

兼容性表述：`api_key` 字段层的移除之所以安全，依赖**启动期自动迁移每次启动都跑**（需求 E.6）——迁移被跳过的畸形条目（E.3）其字段层密钥不再参与解析，告警文案已注明。环境变量两级保持现状逐字节一致。**互斥规则保持现状**：环境变量与凭据库并存时不报冲突（环境变量在此模型下是「未落库的临时凭据」，优先级靠后即可——与 model-settings 既有 apiKey/oauth 互斥语义不交叉，后者是同一位置的两种声明，前者是两个位置）。

### 5.2 更新生效（A.4）

现有适配器缓存（`ConfigResolvingLlm`，缓存键只含 model_id 与 thinking_effort）**不感知**密钥变化——配置热重载换密钥今天同样不触发重建，本域需**新增失效机制**而非复用：将**该 provider 的凭据指纹并入适配器缓存键**（解析时从 §2.1 的直读结果计算，指纹变化即构造新适配器实例）——下一次模型调用重新解析即用新密钥；在途请求自然完成（不中断）。`credential.updated` 进程内事件与 CLI 直写（无事件）两条路径都被覆盖：缓存键的凭据指纹每次解析直读比对，指纹变了就是新实例。

### 5.3 RPC 面（web/ACP 消费）

新增 klient contract `credentialService`（与 `providerService` 平级，注册进 mirri-proto 既有注册表）——**web 管理面（Epic G）的唯一后端**，覆盖全部日常管理语义：

```text
credential.list(providerId?)               -> [{provider_id, type, time_created, external_available?, status?}]   // 无明文
credential.set(providerId, value)           -> noResult          // value 全类型（key/external）；已有则覆盖
credential.remove(providerId)              -> noResult
credential.status(providerId)              -> {status: normal|needs_reconfigure, message?}
```

（wire 字段一律 snake_case——与同 registry 的 klient 契约家族一致，架构原文的 camelCase 示意以本节为准修正；`external_available` 供 G.1 的外部来源在场状态列（仅 external 条目携带，key 条目省略）；`status` 为该 provider 的运行时状态视图（同 `credential.status` 单查，list 附带省一次往返）。）

**无 reveal 的 RPC 面**：明文回读只在 CLI（本地直读文件库），不经网络 RPC——server token 泄漏不放大为密钥库泄漏（F.2 的纵深：明文出口只有一个，且在本机）。web 面编辑密钥只写不读（G.6）。

## 6. 失效状态视图（Epic D）

- **上报点（ADR-009，O-5 接线）**：模型适配器构造层是全链路唯一知晓 provider 的位置——`Llm` trait 增加 `provider_id() -> Option<&str>` 访问器（`ConfigAwareLlmFactory` 构造时注入，其他实现默认 None）；`EngineError::ProviderAuthError` 增加结构化 provider 归因字段（改造点：把 `llm` 传入 `classify_surface_error`——调用点 `chat_with_retry` 作用域可及 `llm.provider_id()`；Display 渲染保持原消息文本，归因是加法字段）。embedded 在**两个失败出水面**识别归因并调 `note_provider_auth_failure`：① fire/cron/task/goal 分支的 `SessionEvent::TurnFailed` 发布点（统一封装处判 `error_code == "provider.auth_error"` + 错误体归因）；② wait 模式的 `message.send` 错误返回点（同一判据）。上报进入内存 map 时经密钥模式脱敏（`sk-`/`Bearer`/私钥头打码，防供应商报文回显密钥片段）。
- **恢复点**：该 provider 的任何凭据写操作（set/remove）清除状态（D.2 即时恢复语义）。
- **状态不落盘**：内存 map，进程重启即重算（D-M5）。
- **消费面**：`credential.status` / `credential.list` RPC（web 设置页——G.1 清单状态列与 G.5 供应商徽标的主消费方）；CLI `list` 的状态列（直连模式无运行时数据，如实标注「未知」而非恒显正常）；会话错误信息（D.4）由既有错误链拼装指引文案，指引文案常量表由本域提供。

## 7. 迁移（Epic E）

`MigrationRunner` 在 **server 每次启动**（E.6 自动重检）与 `mirri credential migrate` 时运行（幂等，E.2）：

```text
读 config.toml（无 env 层）
  → 枚举 [providers.<id>] 含非空 apiKey 的条目
  → 该 provider 凭据库中已有条目？
      否 → 写入凭据库（type=key）
      是 → 不再入库（幂等跳过）
  → 该 provider 入库完成（或本就已入库）且 config 仍有非空 apiKey
      → config.toml 中其 apiKey 字段删除（原子写）——补偿清除，
        保证「入库之后、清除之前」崩溃的重跑必然收敛到字段被清（V7）
  → 不可解析/空 provider 引用 → 告警跳过（E.3；告警注明该密钥不再参与解析）
环境变量派生值不参与（E.4——只处理文件里的明文）
```

「先入库成功、后清配置」的顺序保证不丢数据；跳过分支的补偿清除保证清理最终完成（E.2）；`--dry-run` 列出将迁移的供应商清单（仅供应商标识与条目元信息，**不含密钥明文**——V12）。

## 8. 机制与需求映射

| 需求 Epic | 承载机制（本文） |
|---|---|
| A 凭据库（每供应商一条） | §1 存储 + §2 服务层（覆盖写、级联清理） |
| B CLI 辅助面 | §3 子命令组（直连库函数） |
| G Web 管理界面（第一管理面） | §5.3 credentialService RPC + model-settings 界面容器 |
| C 外部引用 | §4 ExternalCredentialSource |
| D 失效呈现 | §6 状态视图 + §5.3 status RPC |
| E 迁移 | §7 MigrationRunner（含补偿清除与启动重检） |
| F 非功能 | §2.1 并发/可见性/容错、§3 警示协议、§5.3 无明文 RPC、§6 脱敏 |
| D-M1 唯一真值 | §1.1 + §7（迁移后清字段） |
| D-M2 明文回读 | §3 show + §5.3（RPC 无 reveal） |
| D-M3 解析链 | §5.1（字段层移除 + 迁移安全网） |
| D-M4 值类型 | §1.2 封闭联合 |
| D-M5 运行时视图 | §6 |
| D-M6 平级消费方 | §3 + §5.3 同一服务层 |

## 9. ADR 记录

- **ADR-001（2026-10-28）**：单文件 JSON 库，不引入数据库。背景：凭据条目量级是个位数到几十，无查询复杂度诉求；仓库既有持久化（sessions/migrations）已有 JSON+原子写基建。决策：`credentials.json` 单文件 + 原子写 + 文件锁；不引入 SQLite/表结构。被否决备选：per-provider 目录分文件（锁与全量读复杂化，收益仅并发粒度）；嵌入式数据库（依赖与迁移成本不成比例）。注：条目数组形态在 SDR-004 后仍保留——对未来放开多条（提案 P-5）零迁移成本。
- **ADR-002（2026-10-28）**：CLI 直连文件库而非「必须经 server」。背景：凭据管理的典型场景是 server 未起时配置新供应商。决策：CredentialService 以库函数形态同时供 CLI（直连）与 embedded server（进程内）使用，文件锁保证两种形态并发安全。被否决备选：CLI 走 RPC 连 server（要求 server 在跑，`mirri credential add` 在裸环境失败）；CLI 独立实现（逻辑双份漂移）。
- **ADR-003（2026-10-28）**：回读（reveal）只在 CLI 本地出口，不设 RPC。背景：SDR-002 要求终端可核对密钥；同时 web 面的 server token 一旦泄漏，明文回读 RPC 会把整库暴露给攻击面。决策：明文出口收敛为 CLI 直读（本机文件权限即边界）；web 编辑密钥走「写入新值」方向，不需要读旧值。被否决备选：RPC 加 reveal 端点（server token 泄漏放大为密钥库泄漏，违背 F.2 纵深）。SDR-003 后该裁决不变——web 成为第一管理面后更需坚守：编辑只写不读（G.6），管理完整性不依赖明文可见。
- **ADR-004（2026-10-28）**：external 不缓存取用结果。背景：云 CLI 令牌短寿（分钟级），缓存收益低、失效逻辑复杂。决策：每次解析现场 `obtain()`；性能敏感场景由来源自身缓存兜底。被否决备选：服务层缓存 + TTL（失效竞态与多进程一致性的复杂度不成比例）。
- **ADR-005（2026-10-28）**：适配器缓存键纳入凭据指纹（新增失效机制）。背景：A.4 要求更新即时生效，但现有适配器缓存键只含模型与思考档位，不感知任何密钥变化——「复用既有失效钩子」的前提经核查不存在。决策：把该 provider 的凭据指纹并入缓存键，指纹来自每次解析的直读结果；进程内事件与 CLI 直写两条路径都被「指纹变化即新实例」统一覆盖。被否决备选：a) 密钥热注入运行中适配器（侵入 provider 构造协议）；b) 依赖配置热重载失效（既有链路本就不覆盖密钥字段）。
- **ADR-006（2026-10-28）**：解析直读文件，不做进程内快照缓存。背景：CLI 直写文件库后，运行中 server 必须能看见（V8）；文件监听与事件跨进程传播复杂且时序脆弱。决策：每次密钥解析直读凭据库文件（条目量级小，直读成本可忽略），展示面按需直读；进程内缓存仅限单次请求内复用。被否决备选：文件监听（平台差异大、事件丢失面广）；声明 CLI 写后 server 可见性延迟（验收不达标）。
- **ADR-007（2026-10-28）**：跨进程锁沿用会话索引的锁约定。背景：凭据库有双写方（CLI 直连 + server 进程内），需要锁协议；仓内已有成熟先例（`mirri-session-index` 的注册表锁：`create_new` 锁文件 + 修改时间判陈旧接管 + 等待超时，天然跨平台）。决策：`credentials.json.lock` 按同款约定实现，崩溃残留可被接管，不阻断后续写入。被否决备选：flock 类 advisory lock（Windows 缺失，持有者崩溃后语义未定义）。
- **ADR-009（2026-10-28）**：provider 归因经 Llm 访问器随错误带出（O-5 接线）。背景：失败轮次的供应商归因在错误上抛链任何一层都不可见——只有模型适配器构造层（alias → provider 解析处）知道；把归因放进 `SessionEvent::TurnFailed` 会漏掉 wait 模式（交互轮次走 message.send 错误响应而非事件）。决策：`Llm` trait 加 `provider_id() -> Option<&str>`（默认 None，构造注入），`ProviderAuthError` 错误体携带归因；embedded 在两个失败出水面统一识别。被否决备选：a) TurnFailed 加 provider 字段（漏 wait 主路径）；b) embedded 侧反查 alias→provider（错误点无 config 句柄，多分支重复）。边界：归因为 None（非凭据库构造的适配器，如测试 mock）时不 report——无归因即诚实不上报。影响范围：§6 上报接线、core 的 llm.rs 与 error.rs。
- **ADR-008（2026-10-28）**：存储格式预留按条目的加密形态声明。背景：SDR-002 裁决本批明文，但加密演进的可能形态包括——按操作系统选择不同后端（macOS Keychain / Windows DPAPI / Linux libsecret）、不同的主密钥管理方式、甚至算法轮换；这些是**同库内条目级差异**（混合态：部分条目已迁移、滚动升级中途），不是单一的全局格式版本，仅靠 `version` 整数表达不了。决策：每个条目携带 `encoding: { cipher: ... }` 字段，当前唯一合法值 `"plaintext"`（零行为，纯显式声明）；未来加密演进 = 新增合法 cipher 取值 + 逐条目重写，读取按条目各自的 encoding 处理，混合态天然合法，未知 cipher 走 F.4 容错。被否决备选：a) 现在不加、P-1 启动时随 version 2 加（否——届时库中已有真实用户数据，改格式需写数据迁移；现在加是零成本的字段，晚加是有成本的迁移）；b) 全局级加密标记（否——表达不了滚动迁移与分 OS 后端的条目级混合态）。
