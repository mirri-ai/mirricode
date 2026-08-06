# v2 Session Read Path — Phase 4 Cold-Start Fix (Internal Design)

> **Status**: Implemented (feat/startup-speed-phase4)
> **Date**: 2026-08-04
> **Related**: `read-path-convergence-design.md`（phase 3：syncFromDisk 定向扫描 + mtime 快照）、
> `packages/agent-core-v2/src/app/sessionIndex/{sqliteSessionStore,sqliteSessionIndex,sqliteDiskSource}.ts`、
> `packages/agent-core-v2/src/app/workspace/workspaceService.ts`、
> `packages/agent-core-v2/src/app/workspaceAliases/workspaceAliasesService.ts`

> 内部工程文档。记录「冷启动后 sessions 列表请求仍慢」的 phase-4 根因调查与修复：
> 用真实动作（dev server 冷启动 + 并发 18 workspace 压测 + server requestLog）代替猜测，
> 定位到「每个请求重复读 workspace catalog / session_index / 目录快照不共享」三类确定性
> I/O，修复后验收全部达标。

---

## 1. 症状与历史

phase-3 已完成：sqlite session-index 读路径改为
`syncFromDisk(workspaceIds) + 目录 mtime 快照 + in-flight 合并`，
单请求（干净副本）实测 27ms。但用户真实 Desktop 体感 **6–10s/请求** 未变。
phase-4 目标（用户验收）：**直接运行 dev server 冷启动后，并发请求真实的 18 个
workspaces 的 sessions 2s 内全部返回**，且 100+ session 的 workspace 单读 ≤ 500ms。

## 2. 调查（全部真实测量，无猜测）

### 2.1 再现

压测 `e2e-phase4.mjs`（同一副本 home，冷启动 dev server，并发 18 ws × page=20）：

| 条件 | batch total | 说明 |
|---|---|---|
| 隔离副本（单 server，无桌面竞争） | 90–150ms | phase3 后正常 |
| 真实 ~/.mirri-code + Desktop.app 进程同时跑 | **14.6s** | 双进程共享 HOME |
| 同副本复用（`kill -9` 残留 -wal/-shm） | 0.7–13s | WAL 脏文件拖累新进程 |
| 干净副本 + phase-4 修复 | **14–740ms** | 达标（见 §5） |

### 2.2 根因分解（server 内 PROFEST）

- `reconcile`（启动）已 11ms（phase3）；但读路径的 `syncFromDisk` mtime 快照是
  **进程独立** 的，startup reconcile 扫完没有回填 → 首个列表请求又把 203 个会话
  目录逐一遍历（store.get + stat）。
- `GET /sessions` route 每次请求固定执行：
  - `IWorkspaceService.list()` → `runExclusive` 串行 + `loadCatalog()`（读 workspaces.json）
  - `resolveAliasIds` → 另一次 `store.load()`（再读） + `readSessionIndexEntries`（读
    session_index.jsonl）
  → PROFEST：`get=211ms + load=208ms + readIndex=255ms ≈ 674ms/请求`（async fs 排队 ≈200ms/次）。
- 前端首屏**并发 18 个** → 每个请求各自重复上述文件读，全部在短暂窗口内排队，
  叠加成 4–14s。

## 3. 修复（本分支实现）

1. **`SqliteSessionIndex.reconcile` 启动后填充 `workspaceDirMtimes` 快照** ——
   reconcile 收敛完磁盘后，把每个 workspace 目录 mtime 写进读路径共享 `syncFromDisk`
   的快照；首次 `list`/`countActive` 直接命中快照，零重扫。单测：
   「given startup reconcile, when first list, then no directory re-scan」。
2. **`SqliteSessionStore` 开 WAL + `busy_timeout` + 启动 checkpoint** ——
   多进程共享 HOME 时 writer/reader 不再互锁；`kill -9` 残留的 -wal 在 open 时
   `wal_checkpoint(TRUNCATE)` 合并，避免新进程打开回放多 MB wal。
3. **`WorkspaceService.list/get` 加 TTL(150ms) + 写失效的 catalog 缓存** ——
   `createOrTouch`/`update`/`delete` 后立即失效；列表请求不再每次读文件。
4. **`WorkspaceAliasesService` 加 TTL(150ms) + in-flight 合并缓存** ——
   catalog + session-index 解析值缓存；并发风暴共享一次读。

语义保真：write-through（createOrTouch/update/delete）即时失效缓存，150ms 内
可见性窗口仅影响 alias 多桶展开（正面读路径，不影响 sqlite 权威列表）。

## 4. 验收（真实输出）

| 指标 | 要求 | 实际 |
|---|---|---|
| 冷启动并发 18 ws 批次 | ≤ 2s | 14 / 722 / 740ms（3 轮） |
| 单 ws（135 sessions）首读 | ≤ 500ms | 6ms |
| single 135ws warm | ≤ 200ms | 4ms |
| batch warm | ≤ 200ms | 8–9ms |

回归：`agent-core-v2` 全量 vitest **5052** 绿 + typecheck；`kap-server` **982** 绿 +
typecheck；`build.sh` Step 0–14（DMG）。

> 附：环境伪影说明 —— 同一副本连续 `kill -9` 会残留 -wal/-shm，会让新进程打开慢；
> 真实用户优雅退出不触发；(startup checkpoint 已防).

## 5. 架构判断与取舍

- **读路径的权威性是 sqlite 索引**（phase3 已定）；catalog/alias 的文件读是**旁路
  meta**，可短 TTL 缓存，不影响正确性。
- 缓存失效用「写方法显式失效 + TTL」而非 fs watch（chokidar 事件不可靠/延迟），
  保证确定性。
- WAL 是双进程共享 HOME（Desktop + dev/CLI）的正确长期形状；checkpoint 兜底崩溃残留。
- 剩余已知项（非本次范围）：前端首屏「18 并发」可改优先/懒加载；Desktop 旧二进制需
  重启加载新 SEA（用户在旧版本上观察 6-10s）。

## 6. 验收证据文件

- `.tmp/e2e-phase4.mjs`（并发 18 ws 压测）
- `.tmp/e2e-phase4-single.mjs`（单一 135-ws 计时）
- `.tmp/e2e-phase4-sea.mjs`（SEA 二进制同测）
- 各输出：`e2e-phase4-baseline.out` / `-sea.out` / `batch*` ⏳（运行时生成）