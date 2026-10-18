# 会话一致性 · 批次 1（读侧同步）实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让共享同一 MIRRICODE_HOME 的两个 Rust server 实例在双开同一会话时，B 实例能在秒级跟随 A 实例写入的新内容（版本探测 + 增量吸收 + 前台跟随调度），并让会话列表对 A 侧变更保持新鲜。

**Architecture:** 乐观并发（spec `docs/specs/session-consistency-architecture.md`，ADR-001/005）。版本锚 = `wire.jsonl` 的字节长度，落点在 `SessionWireLog` 内新增 `known_size` 原子计数：自身写入在写入点同步推进（ADR-005 不变量 1）；吸收器持有日志互斥锁从锚点读新字节、解析完整行、应用到 main agent 内存态并发布事件（不变量 2）；跟随调度器照抄 cron-driver 模式每 tick 探测，本会话有 in-flight turn 时只跳过不吸收（不变量 3）。索引新鲜度由 per-server 的周期对账 tick 提供（重跑 `reconcile`，从磁盘读收敛）。

**Tech Stack:** Rust workspace（`rust/crates/`）：mirri-persistence、mirri-proto、mirri-core、mirri-embedded；tokio；现有 `FlagRegistry` 实验开关。

## Global Constraints（每个任务隐含遵守）

- 规格：`docs/specs/session-consistency-requirements.md`（验收依据）、`docs/specs/session-consistency-architecture.md`（§2.4 互斥契约、ADR-001/003-R1/005）。
- 跟随延迟上限 5 秒（US C1.2）→ 跟随 tick 默认 2 秒；索引新鲜度 tick 10 秒。
- 增量吸收只处理自锚点以来的新增字节，禁止任何全量重放路径（US C4.1）。
- 吸收必须容忍未写完整的最后一段（停在上一完整行，不报错不丢段）。
- 本入口自身写入不得触发重复吸收/重复事件（ADR-005 不变量 1 的回归测试必须有）。
- 功能经实验开关 gating，默认关闭：flag 名 `session-consistency-follow`（mirri-core `flags.rs` 注册，`MIRRICODE_EXPERIMENTAL_SESSION_CONSISTENCY_FOLLOW` 或全局 `MIRRICODE_EXPERIMENTAL_FLAG` 开启）。
- 测试命名 Given-When-Then（`should ... when ...` 风格）；断言用普通 `assert!`/`assert_eq!`（本仓库不用 pretty_assertions）。
- cargo 测试/clippy 必须后台运行（`run_in_background=true`，`disable_timeout=true` 或 ≥20 分钟超时），命令：`cargo test --manifest-path rust/Cargo.toml -p <crate>`、`cargo clippy --manifest-path rust/Cargo.toml --workspace --all-targets -- -D warnings`。
- 提交信息 Conventional Commit 风格，不暴露 agent 身份。
- 优先把测试加进既有测试文件；mirri-embedded 新集成测试文件按既有 `tests/t<N>_<topic>_tests.rs` 命名模式新建并在 `tests.rs` 的 mod 声明处注册。

## 关键代码事实（实现者必读）

- `SessionWireLog`：`rust/crates/mirri-core/src/scope/session_scope.rs:58`，字段 `writer: Arc<tokio::sync::Mutex<WireLogSlot>>`；`WireLogSlot::{Detached, Deferred(Vec<WireOp>), Writer(JsonlAppendLog)}`（:65）。`record_op`（:202）在 Writer 分支调 `writer.append(&op)`；`record_ops`（:184）；`install`（:133）；`path()`（:87）。
- `JsonlAppendLog`：`rust/crates/mirri-persistence/src/append_log.rs:123`，字段 `path`/`file`/`dir_synced`，**无字节偏移跟踪**。`append`（:328）→ `write_durable`（:294，`Arc<Mutex<File>>` + `sync_all`）。resume 用 `open_for_append`（:196，O_APPEND 不截断）。
- `WireOp`：`rust/crates/mirri-proto/src/wire.rs:47`，`#[serde(tag="type")]`；用户消息双写 `TurnPrompt{input, origin}` + `ContextAppendMessage{message}`（mirri-embedded `message_handlers.rs:769-783`）。
- reducer：`reduce_with_limits(state,op,limits) -> (AgentState, Vec<DomainEvent>)`（`rust/crates/mirri-proto/src/reducer.rs:96`），纯函数、单流折叠。本计划不直接用它改内存态（live 态是 `ContextMemory` 不是 `AgentState`），只借用其语义写定向 apply。
- `SessionEvent`：`rust/crates/mirri-core/src/session/events.rs:51`；`EventBus`（:762）`subscribe`/`publish`/`subscriber_count`。**实现前先读 events.rs 里 `MessageReceived` 等变体的精确字段**。
- `ContextMemory`：main agent 内存态，restore 用 `*agent.context_memory().lock() = memory` 整体替换（`restore.rs:259-262`，std Mutex）。**实现前先读 `ContextMemory` 的 conversation/messages 与 loop_events 字段形态**。
- 周期任务模板：`cron_driver.rs:186-220`（clone 共享 Arc → `tokio::spawn` → `interval.tick()` 循环 → `resolve_session_via` 失败即 break）；tick 常量在 `embedded_server/mod.rs`（`CRON_DRIVER_TICK_SECS:30` :1209），字段 + `with_cron_tick` 测试缝（:1404）。
- in-flight turn：`EmbeddedServer.in_flight_turns: Map<String, InFlightTurn>`（`embedded_server/mod.rs:972`）。
- 索引对账：`reconcile_session_index_once`（`rust/crates/mirri-embedded/src/embedded_server/session_index.rs:187-303`），由 `AtomicBool session_index_reconciled`（:190）限定每进程一次。
- 恢复链路：`resume_session_with_scope`（`rust/crates/mirri-core/src/session/manager/restore.rs:92`）→ `land_resume_tail`（:171）→ `install_wire_log`（session_scope.rs:926）；`SessionManager.persistence` 持 `SessionPersistence`，`wire_log_path(session_id)`（persistence.rs:299）。
- resume 后 spawn 点：`session_handlers.rs:1371-1376`（`spawn_session_relay` / `spawn_cron_driver` / `spawn_task_driver`）。

## File Structure

- Modify: `rust/crates/mirri-persistence/src/append_log.rs` — append 返回写入字节数（Task 1）
- Modify: `rust/crates/mirri-core/src/scope/session_scope.rs` — `SessionWireLog.known_size` 锚 + `drain_foreign_ops`（Task 2、4）
- Create: `rust/crates/mirri-core/src/session/consistency.rs` — 版本探测 + 吸收入口 + AbsorbReport（Task 3、4）
- Modify: `rust/crates/mirri-core/src/session/manager/mod.rs` — `absorb_session_foreign_updates` 包装 + consistency 模块声明（Task 4）
- Modify: `rust/crates/mirri-core/src/flags.rs` — 注册 flag（Task 5）
- Create: `rust/crates/mirri-embedded/src/embedded_server/follow_scheduler.rs` — 跟随调度器（Task 5）
- Modify: `rust/crates/mirri-embedded/src/embedded_server/mod.rs` — `follow_tick` 字段 + spawn 接线（Task 5）
- Modify: `rust/crates/mirri-embedded/src/embedded_server/session_handlers.rs` — resume 后 spawn 跟随驱动（Task 5）
- Modify: `rust/crates/mirri-embedded/src/embedded_server/session_index.rs` — 对账可重跑（Task 6）
- Create: `rust/crates/mirri-embedded/src/embedded_server/tests/t_session_consistency_tests.rs` — 双实例集成测试（Task 7）
- Modify: `rust/crates/mirri-embedded/src/embedded_server/tests.rs` — mod 声明注册（Task 7）

范围声明（记录进 ROADMAP 的已知裁剪）：批次 1 吸收**会话级主 journal**（main agent 全部内容 + 会话域 op）；子 agent 的 `agents/<id>/wire.jsonl` 仅对 B 侧 agents map 中**已存在**的 agent 做同样的吸收（复用同一 drain 机制），A 侧新注册的子 agent 在 B 侧要等重新打开会话才完整可见。徽标（US C1.3）在批次 4。写侧围栏（C2.x）在批次 2。

---

### Task 1: `JsonlAppendLog::append` 返回写入字节数

**Files:**
- Modify: `rust/crates/mirri-persistence/src/append_log.rs`（append 与 write_durable，:294/:328 附近）
- Test: `rust/crates/mirri-persistence/src/append_log.rs` 内既有 `#[cfg(test)]` 模块

**Interfaces:**
- Produces: `pub async fn append(&self, op: &WireOp) -> anyhow::Result<u64>`（返回本次落盘行字节数，含换行）；所有调用点改为接收返回值（机械更新）。若 `append` 由 trait 声明，同步更新 trait 与全部实现。

- [ ] **Step 1: 写失败测试**（加进 append_log.rs 既有 test 模块）

```rust
#[tokio::test]
async fn should_return_written_byte_count_when_append_succeeds() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("wire.jsonl");
    let log = JsonlAppendLog::create(&path).await.unwrap();
    let op = WireOp::Metadata { protocol_version: "1.6".into(), created_at: "2026-10-13T00:00:00Z".into() };
    let n = log.append(&op).await.unwrap();
    let line = std::fs::read_to_string(&path).unwrap();
    assert_eq!(n as usize, line.len(), "append must report the full line length it wrote");
    assert!(line.ends_with('\n'));
}
```

（`WireOp::Metadata` 的字段形态以 `wire.rs:53` 为准，先读再写。）

- [ ] **Step 2: 运行验证失败**

Run: `cargo test --manifest-path rust/Cargo.toml -p mirri-persistence append --lib`（后台，disable_timeout）
Expected: 编译失败——append 返回 `()`，不能 `unwrap()`

- [ ] **Step 3: 最小实现**

在 append（:328 附近）序列化行后：

```rust
let line = serde_json::to_string(op)? + "\n"; // 若现有实现已构造 line，直接取其 len
write_durable(Arc::clone(&file), line.into_bytes()).await?;
Ok(line.len() as u64)
```

同步更新 trait（如 `AppendLogStore` 声明了 append）与全部实现/调用点；`write_durable` 保持返回 `io::Result<()>` 不动。

- [ ] **Step 4: 运行验证通过**

Run: `cargo test --manifest-path rust/Cargo.toml -p mirri-persistence --lib`（后台）
Expected: PASS

- [ ] **Step 5: 提交**

```bash
git add rust/crates/mirri-persistence/src/append_log.rs
git commit -m "feat(persistence): append returns written byte count for version anchoring"
```

---

### Task 2: `SessionWireLog` 的 known-size 版本锚

**Files:**
- Modify: `rust/crates/mirri-core/src/scope/session_scope.rs`（SessionWireLog，:58-220 区域）
- Test: 同文件既有测试模块（若无则在 session 模块就近测试文件）

**Interfaces:**
- Consumes: Task 1 的 `append -> Result<u64>`。
- Produces:
  - `SessionWireLog` 新字段 `known_size: std::sync::atomic::AtomicU64`
  - `pub fn known_size(&self) -> u64`（Acquire 读）
  - `install(&mut writer)` 时把 `known_size` 初始化为当前文件长度（stat writer 的 path）

- [ ] **Step 1: 写失败测试**

```rust
#[tokio::test]
async fn should_advance_known_size_when_own_ops_are_recorded() {
    // 造真实 wire 文件（create 模式写一条 metadata），install 后 known_size == 文件长度；
    // 再 record_op 一条，known_size 增量 == append 返回的字节数，且 == stat 的文件长度
}

#[tokio::test]
async fn should_init_known_size_to_file_length_when_installed() {
    // 预写 3 条 op 的文件 → open_for_append → install → known_size == 文件字节数（不含后续）
}
```

（两个测试用真实 tempdir 文件，按 Task 1 测试同款构造；断言用 stat 文件长度对照。）

- [ ] **Step 2: 运行验证失败** — `cargo test --manifest-path rust/Cargo.toml -p mirri-core known_size`（后台）
- [ ] **Step 3: 实现**

```rust
pub struct SessionWireLog {
    writer: Arc<tokio::sync::Mutex<WireLogSlot>>,
    known_size: Arc<std::sync::atomic::AtomicU64>, // Arc 以便与吸收器共享只读视图
}
```

- `record_op`/`record_ops` 的 Writer 分支：`writer.append(&op).await` 成功后 `self.known_size.fetch_add(n, Ordering::Release)`。
- `install(writer)`：先把 `known_size` 设为 `std::fs::metadata(writer.path()).map(|m| m.len()).unwrap_or(0)`，再换 slot（**先 stat 后换**，保证 Deferred 冲刷的追加会在此基数上累加）；Deferred 分支冲刷走的仍是 record 路径，自然累加。
- `JsonlAppendLog` 若无 `pub fn path(&self)`，补上（返回 `&Path`）。
- Deferred/Detached 状态下 `known_size` 为 0（未物化 journal 无磁盘内容可锚）。

- [ ] **Step 4: 运行验证通过** — 同 Step 2 命令
- [ ] **Step 5: 提交** — `git commit -m "feat(core): SessionWireLog tracks known-size version anchor per ADR-005"`

---

### Task 3: 版本探测器（consistency 模块）

**Files:**
- Create: `rust/crates/mirri-core/src/session/consistency.rs`
- Modify: `rust/crates/mirri-core/src/session/mod.rs`（或 session 目录的既有模块声明处）注册 `pub mod consistency;`

**Interfaces:**
- Produces:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JournalVersion { pub size: u64, pub mtime_ms: Option<u64> }

/// stat 一次 journal 文件；读不到返回 None（调用方按"无法判定"处理）
pub fn probe_journal_version(path: &std::path::Path) -> Option<JournalVersion>;

/// 磁盘版本 vs 本地已知字节锚：size 相等 → Fresh；更大 → Divergent；更小 → Truncated
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VersionCheck { Fresh, Divergent, Truncated }
pub fn check_version(disk: JournalVersion, known_size: u64) -> VersionCheck;
```

- [ ] **Step 1: 写失败测试**（模块内 `#[cfg(test)]`）

```rust
#[test]
fn should_report_fresh_when_disk_size_equals_known_size() {
    let disk = JournalVersion { size: 100, mtime_ms: Some(1) };
    assert_eq!(check_version(disk, 100), VersionCheck::Fresh);
}

#[test]
fn should_report_divergent_when_disk_grew_beyond_known_size() {
    let disk = JournalVersion { size: 150, mtime_ms: Some(2) };
    assert_eq!(check_version(disk, 100), VersionCheck::Divergent);
}

#[test]
fn should_report_truncated_when_disk_shrank_below_known_size() {
    let disk = JournalVersion { size: 50, mtime_ms: Some(3) };
    assert_eq!(check_version(disk, 100), VersionCheck::Truncated);
}

#[test]
fn should_return_none_when_probing_missing_file() {
    assert!(probe_journal_version(std::path::Path::new("/nonexistent/wire.jsonl")).is_none());
}
```

- [ ] **Step 2: 验证失败** — `cargo test --manifest-path rust/Cargo.toml -p mirri-core consistency`（后台）
- [ ] **Step 3: 实现**

```rust
pub fn probe_journal_version(path: &std::path::Path) -> Option<JournalVersion> {
    let meta = std::fs::metadata(path).ok()?;
    let mtime_ms = meta.modified().ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64);
    Some(JournalVersion { size: meta.len(), mtime_ms })
}

pub fn check_version(disk: JournalVersion, known_size: u64) -> VersionCheck {
    match disk.size.cmp(&known_size) {
        std::cmp::Ordering::Equal => VersionCheck::Fresh,
        std::cmp::Ordering::Greater => VersionCheck::Divergent,
        std::cmp::Ordering::Less => VersionCheck::Truncated,
    }
}
```

（size 为主信号、mtime 仅随附，见 spec §2.1。）

- [ ] **Step 4: 验证通过** — 同上
- [ ] **Step 5: 提交** — `git commit -m "feat(core): journal version probe for session consistency"`

---

### Task 4: 增量吸收器

**Files:**
- Modify: `rust/crates/mirri-core/src/scope/session_scope.rs`（SessionWireLog 增 `drain_foreign_ops`）
- Modify: `rust/crates/mirri-core/src/session/consistency.rs`（吸收入口 + AbsorbReport）
- Modify: `rust/crates/mirri-core/src/session/manager/mod.rs`（`absorb_session_foreign_updates`）
- Test: consistency.rs 模块测试 + manager 既有测试文件补一条

**Interfaces:**
- Consumes: Task 2 的 `known_size`；Task 3 的 `probe/check_version`。
- Produces:

```rust
// consistency.rs
#[derive(Debug, Default, Clone, Copy)]
pub struct AbsorbReport { pub ops_absorbed: usize, pub bytes_consumed: u64 }

// SessionWireLog：持锁读锚点之后的新字节，逐行回调，推进锚点（不变量 2：锚不先于消费）
pub async fn drain_foreign_ops(&self, on_op: &mut (dyn FnMut(WireOp) + Send)) -> anyhow::Result<AbsorbReport>;

// SessionManager：对已打开会话吸收会话主 journal 的外来更新
pub async fn absorb_session_foreign_updates(&self, session: &Arc<SessionScope>) -> SessionResult<AbsorbReport>;
```

- [ ] **Step 1: 写失败测试**（consistency.rs；用 tempdir + 真实文件）

```rust
#[tokio::test]
async fn should_absorb_only_complete_lines_and_leave_torn_tail_for_next_round() {
    // 1) tempdir 写 wire 文件：3 条完整 op 行（用 serde_json 序列化 WireOp）
    // 2) open_for_append → SessionWireLog::default() → install → known_size == 全部 3 行
    // 3) 追加第 4 条完整行 + 半条（无换行符的 JSON 前缀）
    // 4) drain_foreign_ops：回调收到 1 条 op；known_size 停在第 4 行末（不含半行）
    // 5) 补全半行（加换行）→ 再 drain：又收到 1 条；known_size == 文件尾
}

#[tokio::test]
async fn should_stop_before_first_unparseable_line_and_not_advance_past_it() {
    // 文件含一条损坏行（完整换行但非法 JSON）：drain 消费它之前的行，锚停在下界，
    // 返回 Ok（损坏行按"下次再看"处理，不报错阻断 —— spec §5）
}

#[tokio::test]
async fn should_advance_anchor_exactly_to_consumed_bytes_when_absorbing() {
    // Given-When-Then：吸收 N 字节后 known_size 精确等于锚起点 + N；再次 drain（无新增）返回 0 ops
}
```

（测试构造 `SessionWireLog` 的方式：`SessionWireLog::default()` 后 `install(JsonlAppendLog::open_for_append(path))`；确认 `default()` 可用，否则按 session_scope.rs 现有构造函数走。）

- [ ] **Step 2: 验证失败** — `cargo test --manifest-path rust/Cargo.toml -p mirri-core consistency`（后台）

- [ ] **Step 3: 实现 drain_foreign_ops**（session_scope.rs，SessionWireLog impl）

```rust
pub async fn drain_foreign_ops(
    &self,
    on_op: &mut (dyn FnMut(WireOp) + Send),
) -> anyhow::Result<AbsorbReport> {
    use std::io::{Read, Seek, SeekFrom};
    let mut guard = self.writer.lock().await;
    let WireLogSlot::Writer(log) = &*guard else {
        return Ok(AbsorbReport::default()); // 未物化 journal：无可吸收
    };
    let path = log.path().to_path_buf();
    let known = self.known_size.load(std::sync::atomic::Ordering::Acquire);
    let mut file = match std::fs::File::open(&path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(AbsorbReport::default()),
        Err(e) => return Err(e.into()),
    };
    file.seek(SeekFrom::Start(known))?;
    let mut buf = String::new();
    file.read_to_string(&mut buf)?;
    let mut consumed: u64 = 0;
    let mut ops = 0usize;
    for line in buf.split_inclusive('\n') {
        if !line.ends_with('\n') { break; } // 撕裂尾行：留给下一轮（spec §5）
        let body = &line[..line.len() - 1];
        match serde_json::from_str::<WireOp>(body) {
            Ok(op) => on_op(op),
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "session consistency: stopping absorb at unparseable line");
                break; // 锚停在坏行之前，不吞坏行之后的完整行
            }
        }
        consumed += line.len() as u64;
        ops += 1;
    }
    if consumed > 0 {
        self.known_size.fetch_add(consumed, std::sync::atomic::Ordering::Release);
    }
    Ok(AbsorbReport { ops_absorbed: ops, bytes_consumed: consumed })
}
```

要点：持 writer 互斥锁期间 `on_op` **只**改内存态 + 发事件，绝不写 wire（否则死锁）；锚推进在字节消费之后（不变量 2）。

- [ ] **Step 4: 实现 absorb_session_foreign_updates**（manager/mod.rs，挨着 resume 家族）

```rust
pub async fn absorb_session_foreign_updates(&self, session: &Arc<SessionScope>) -> SessionResult<AbsorbReport> {
    let wire_path = self.persistence.wire_log_path(session.id());
    let known = session.wire_log_known_size().await;
    // 截断（外部重建会话目录）按 spec §5：不可恢复，交由上层重新 resume；这里只告警并清零锚
    if let Some(disk) = crate::session::consistency::probe_journal_version(&wire_path) {
        if crate::session::consistency::check_version(disk, known) == crate::session::consistency::VersionCheck::Truncated {
            tracing::warn!(session = %session.id().as_str(), "session consistency: journal truncated externally; full resume required");
            session.reset_wire_log_anchor().await;
            return Ok(AbsorbReport::default());
        }
    }
    let agent = session.agent(&mirri_core::scope::MAIN_AGENT_ID /* 按 restore.rs:255 的常量来源 */)
        .ok_or(/* SessionError 变体按现有错误族选 */)?;
    let event_bus = /* 按 install_fresh_artifacts 后的取法：session.event_bus() 若有；否则从 artifacts 取 */;
    let mut absorbed = AbsorbReport::default();
    session.drain_wire_log_foreign_ops(&mut |op: WireOp| {
        apply_absorbed_op(&agent, &event_bus, op); // 见 Step 5
    }).await?;
    // absorbed 聚合略（drain 返回值透传）
    ...
}
```

（`session.wire_log_known_size` / `drain_wire_log_foreign_ops` / `reset_wire_log_anchor` 是 SessionScope 对 `self.wire_log` 的薄转发；`MAIN_AGENT_ID`、agent 访问器、event_bus 取法以 session_scope.rs / restore.rs 现有代码为准，先读再写。）

- [ ] **Step 5: 实现 apply_absorbed_op**（consistency.rs）

```rust
/// 把外来 op 定向应用到本进程内存态并发布 UI 可见事件。
/// 保守集合：消息与回合边界；未覆盖的 op 域记录 debug 日志跳过（v1 已知裁剪，见 ROADMAP）。
fn apply_absorbed_op(agent: &Arc<AgentScope>, event_bus: &EventBus, op: WireOp) {
    match op {
        WireOp::ContextAppendMessage { message } => {
            {
                let mut mem = agent.context_memory().lock();
                // 按 ContextMemory 实际字段追加（先读 reconstruct_context_memory 的写法照抄字段路径）
                mem.conversation.messages.push(message.clone());
            }
            event_bus.publish(SessionEvent::MessageReceived { /* 字段按 events.rs:160 实际形态，message 带 origin */ });
        }
        WireOp::TurnEnded { turn_id, reason, .. } => {
            event_bus.publish(SessionEvent::TurnCompleted { /* 按 events.rs:284 */ });
        }
        other => {
            tracing::debug!(op = ?other, "session consistency: absorbed op skipped (unhandled kind)");
        }
    }
}
```

实现前必读：`events.rs:51-340`（各变体字段）、`replay.rs:57`（`reconstruct_context_memory` 怎么填 conversation/messages，照抄其字段路径）、`reducer.rs:111-147`（ContextAppendMessage/LoopEvent 的折叠语义）。`ContextAppendLoopEvent` 若能按 reducer 语义安全追加到 loop_events 则一并处理，否则归入 skipped 并在 ROADMAP 记录。

- [ ] **Step 6: 写自身写入回归测试**（ADR-005 不变量 1）

```rust
#[tokio::test]
async fn should_not_replay_own_writes_when_absorbing_after_own_record() {
    // Given: 同一 SessionWireLog，record_op 写 2 条（known_size 前进）
    // When: 立即 drain_foreign_ops
    // Then: 回调收到 0 条 op（自身写入已计入锚，不重复折叠）
}
```

- [ ] **Step 7: 验证通过** — `cargo test --manifest-path rust/Cargo.toml -p mirri-core --lib`（后台）+ `cargo clippy --manifest-path rust/Cargo.toml --workspace --all-targets -- -D warnings`（后台）
- [ ] **Step 8: 提交** — `git commit -m "feat(core): incremental journal absorber with anchor-exact consumption"`

---

### Task 5: 前台跟随调度器 + 实验开关

**Files:**
- Create: `rust/crates/mirri-embedded/src/embedded_server/follow_scheduler.rs`
- Modify: `rust/crates/mirri-core/src/flags.rs`（注册 flag）
- Modify: `rust/crates/mirri-embedded/src/embedded_server/mod.rs`（`follow_tick: Duration` 字段 + `FOREGROUND_FOLLOW_TICK_SECS: u64 = 2` 常量 + `with_follow_tick` 测试缝 + mod 声明）
- Modify: `rust/crates/mirri-embedded/src/embedded_server/session_handlers.rs:1371-1376`（resume 后 spawn，与 cron/task driver 并排）

**Interfaces:**
- Consumes: Task 4 的 `SessionManager::absorb_session_foreign_updates`；`FlagRegistry::enabled`。
- Produces: `pub(super) fn spawn_follow_driver(&self, session_id: &str)`；flag 常量 `pub const SESSION_CONSISTENCY_FOLLOW_FLAG: &str = "session-consistency-follow";`

- [ ] **Step 1: 注册 flag**（flags.rs，照 `HOOK_COMMAND_REWRITE_FLAG` :376 的模式：常量 + 默认 false + register）

- [ ] **Step 2: 写失败测试**（放本任务新建的 tests 文件骨架：`tests/t_session_consistency_tests.rs`，先只写调度器行为；mod 注册进 tests.rs）

```rust
#[tokio::test]
async fn should_follow_foreign_writes_when_another_server_records_ops_on_shared_home() {
    // Given: 同一 tempdir home、同一 workspace root 的两个 EmbeddedServer（flag 打开、follow_tick 50ms）
    //   A: session.create + session.resume（A 侧 scope 持 writer）
    //   B: session.resume 同一会话
    //   B 侧先 subscribe_events() 收流
    // When: A 侧对该会话 record 一条 ContextAppendMessage（借 A 的 scope；
    //   取 scope 的方式参考 tests.rs 现有 wire 断言测试怎么拿 session_manager / persistence）
    // Then: 5 秒内 B 的事件流出现该消息对应的 MessageReceived 通知（经 relay 投影）
    // And: 之后连续等 3 个 tick 不再出现重复的该消息（幂等）
}

#[tokio::test]
async fn should_exit_when_session_is_closed() {
    // Given: B 起了 follow driver（tick 50ms）；
    // When: 会话 close；
    // Then: driver task 结束（relays/driver 计数为 0 或 join handle 完成超时内）
}
```

（两 server 构造：`EmbeddedServer::for_workspace_root(...)`（mod.rs:2452）+ `.with_home_dir(Some(home))` + flag env `MIRRICODE_EXPERIMENTAL_SESSION_CONSISTENCY_FOLLOW=1`（用 `std::env::set_var` + 注意测试串行；若现有测试有 env 隔离惯例则照抄）+ `.with_follow_tick(Duration::from_millis(50))`。若 EmbeddedServer 测试需要 llm 工厂，用 `test_server` 系列 helper 的 mock。）

- [ ] **Step 3: 验证失败** — `cargo test --manifest-path rust/Cargo.toml -p mirri-embedded session_consistency`（后台）

- [ ] **Step 4: 实现调度器**（照抄 cron_driver.rs:186-220 模式）

```rust
use std::time::Duration;
use tokio::time::interval;

impl super::EmbeddedServer {
    /// 会话级跟随驱动：周期探测会话主 journal 版本，偏差且无 in-flight turn 时增量吸收。
    /// 不变量 3（ADR-005）：本进程回合运行中只跳过不吸收。
    pub(super) fn spawn_follow_driver(&self, session_id: &str) {
        if !self.flag_registry().enabled(mirri_core::flags::SESSION_CONSISTENCY_FOLLOW_FLAG) {
            return;
        }
        let registry = Arc::clone(&self.workspace_registry);   // 字段名以 mod.rs 为准
        let in_flight = Arc::clone(&self.in_flight_turns);      // mod.rs:972
        let owner = Arc::clone(&self.owner_manager);           // SessionManager 的持有方式以 mod.rs 为准
        let sid = session_id.to_string();
        let tick = self.follow_tick;
        tokio::spawn(async move {
            let mut interval = interval(tick);
            loop {
                interval.tick().await;
                let Some(_guard) = in_flight.lock().await.get(&sid).cloned() else {
                    // 无 in-flight：可吸收
                    if let Ok(session) = /* resolve_session_via(&registry, &sid).await 按 cron_driver 的解析方式 */ {
                        if let Err(e) = owner.absorb_session_foreign_updates(&session).await {
                            tracing::warn!(session = %sid, error = %e, "session consistency: absorb failed");
                        }
                    } else {
                        break; // 会话消失：退出（cron driver 同款清退语义）
                    }
                    continue;
                };
                // 有 in-flight turn：本 tick 跳过（只探测不吸收）
            }
        });
    }
}
```

（`in_flight_turns` 的锁类型（std/tokio、Map 值类型 `InFlightTurn` 是否 Clone）以 mod.rs:972 实际为准——若不可 Clone，改为只查询 `contains_key` 后在 else 分支处理；`owner_manager`/会话解析的具体字段名照抄 cron_driver.rs 的 clone 列表。）

- [ ] **Step 5: 接线**：`session_handlers.rs` resume 成功路径（:1371-1376 旁）加 `self.spawn_follow_driver(&session_id);`；mod.rs 加字段/常量/builder/`mod follow_scheduler;`。

- [ ] **Step 6: 验证通过** — `cargo test --manifest-path rust/Cargo.toml -p mirri-embedded session_consistency`（后台）+ clippy（后台）
- [ ] **Step 7: 提交** — `git commit -m "feat(embedded): foreground follow driver with in-flight turn guard behind session-consistency-follow flag"`

---

### Task 6: 索引新鲜度 tick（列表跨入口可见，US C1.1）

**Files:**
- Modify: `rust/crates/mirri-embedded/src/embedded_server/session_index.rs`（对账可重跑：把 `reconcile_session_index_once` :187-303 的 once-guard 抽出，暴露 `pub(super) async fn reconcile_session_index(&self)`（无 guard）+ 原 once 版调它）
- Modify: `rust/crates/mirri-embedded/src/embedded_server/mod.rs`（`INDEX_REFRESH_TICK_SECS: u64 = 10` 常量；`spawn_index_refresh_driver(&self)`——per-server 一个，flag 同 Task 5）
- Test: `tests/t_session_consistency_tests.rs` 追加

**Interfaces:**
- Consumes: Task 5 的 flag；既有 `reconcile_session_index_once` 的内部逻辑。
- Produces: `pub(super) async fn reconcile_session_index(&self)`（可重跑版本）；`pub(super) fn spawn_index_refresh_driver(&self)`。

- [ ] **Step 1: 写失败测试**

```rust
#[tokio::test]
async fn should_refresh_session_list_when_another_server_renames_a_session_on_shared_home() {
    // Given: 双 server 共享 home；A create session；B（flag 开）起 index refresh driver（tick 50ms）
    // When: A 侧改名该会话（call_method session.rename 或对应 RPC；字段看 routes/session.rs）
    // Then: 10 秒内 B 的 session.list 返回新标题
}
```

- [ ] **Step 2: 验证失败** — 同 Task 5 测试命令
- [ ] **Step 3: 实现**：抽 guard、加 driver（照 Task 5 的 spawn 模式，但不 per-session——per-server 一个 tokio task，tick 到点调 `self.reconcile_session_index().await`；错误只 warn）。
  注意：本 tick 在两进程同时跑时是"各自从磁盘读、各自收敛"——与批次 3 的跨进程锁串行化（ADR-004）衔接，本批不引锁。
- [ ] **Step 4: 验证通过 + 提交** — `git commit -m "feat(embedded): periodic index reconcile tick for cross-entry list freshness"`

---

### Task 7: 双实例集成收口测试

**Files:**
- Test: `rust/crates/mirri-embedded/src/embedded_server/tests/t_session_consistency_tests.rs`（Task 5/6 已建，此处补齐收口场景）
- Modify: `docs/specs/session-consistency-ROADMAP.md`（记录批次 1 已知裁剪：子 agent 新注册的延迟可见、未覆盖 op 域清单）

**Interfaces:**
- Consumes: Task 1-6 全部。

- [ ] **Step 1: 补收口测试**

```rust
#[tokio::test]
async fn should_keep_history_complete_when_both_sides_alternate_writes() {
    // 双 server 共享 home：A 写 3 条 → B 跟随到；B 写 2 条 → A 跟随到；
    // 结束后各自 drain（无新增）幂等；两侧 main agent memory 的消息数 == 磁盘 op 数
}

#[tokio::test]
async fn should_not_absorb_while_own_turn_is_in_flight() {
    // B 侧制造 in-flight turn（用 tests.rs 现有 converse_once / mock llm 慢响应模式；
    // 期间 A 写入 → 等 3 个 tick → B 的吸收被跳过（事件不出现）；
    // turn 结束后下一 tick 补齐（事件出现）——验证不变量 3
}
```

（in-flight 注入方式：优先用真实慢 turn；若 mock 不便，查 tests.rs 是否已有 in_flight_turns 的测试注入 helper，有则照抄。）

- [ ] **Step 2: 全量验证**（后台、disable_timeout）：

```bash
cargo test --manifest-path rust/Cargo.toml -p mirri-core -p mirri-persistence -p mirri-embedded
cargo clippy --manifest-path rust/Cargo.toml --workspace --all-targets -- -D warnings
```

- [ ] **Step 3: 更新 ROADMAP 已知裁剪记录 + 提交** — `git commit -m "test(embedded): dual-instance session consistency integration coverage"`

---

## Parallelization

Task 1→2→4 严格串行（依赖链：append 字节数 → 锚 → 吸收器）；Task 3 可与 Task 1/2 并行（独立新模块，仅被 Task 4 消费）；Task 5 依赖 Task 4；Task 6 依赖 Task 5 的 flag（其余独立，但与 Task 5 同改 mod.rs，串行执行避免冲突）；Task 7 最后。**执行顺序：1 → (2 ∥ 3) → 4 → 5 → 6 → 7**（实际串行执行亦可，任务粒度已小）。

## Self-Review 结论

- 覆盖检查：US C1.1→Task 6；C1.2→Task 4/5/7；C4.1→Task 4（字节锚 + 撕裂容忍测试）；C4.2→Task 5（resolve 失败退出、tick 只在会话存活时工作；前台/后台概念在 serve 传输层订阅即前台——本批 tick 跟随会话存活而非订阅数，已在 ROADMAP 裁剪记录）。C1.3（徽标）、C2.x（围栏）属批次 2/4，不在本计划。
- 占位符检查：无 TBD/TODO；「以现有代码为准」的点位均给了精确文件:行号与读法指引。
- 类型一致性：`AbsorbReport{ops_absorbed, bytes_consumed}`、`drain_foreign_ops`、`absorb_session_foreign_updates`、`spawn_follow_driver`、`reconcile_session_index` 前后一致。
