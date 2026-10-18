# Rust 文件编辑并发安全（Edit/Write 互斥 + 乐观校验）实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让 Rust 引擎的 `Edit` / `Write` 工具在多 agent 并行（swarm、subagent、多 session 共进程）与进程外写入（编辑器、git、第二个 CLI 实例）两类并发下不再静默丢失更新：同路径变更串行化 + 写回前全量内容校验，把丢失更新变成显式冲突错误。

**Architecture:** 新增纯内存的进程级 per-path 互斥锁表（`file_mutation_guard.rs`，zero-IO），`Edit` 在读文件前取锁、写回前用"重读全文 + 逐字节比对"做乐观校验；`Write`（overwrite 与 append）只取锁不校验（盲覆盖是 Write 的本意）。不加 OS advisory lock、不改 `DynFileSystem` trait、不改 RPC/wire 面。

**Tech Stack:** Rust 2024 edition（`rust/` workspace），tokio（`full`，`sync` + `time` 均可用），既有 `mirri_os::DynFileSystem` 注入；测试 `#[tokio::test]` + 内嵌 `#[cfg(test)]` 模块 + fake `DynFileSystem` double。

## Global Constraints

- **zero-IO 守卫**（rust/AGENTS.md #6）：mirri-core 生产代码禁 `std::fs` / `tokio::fs` 等直接 IO；锁表是纯内存的（不碰文件系统），校验重读走注入的 `DynFileSystem`。收尾跑 `node rust/scripts/check-zero-io.mjs` 必须 0 命中。
- **质量门禁**（每任务收尾必跑，全绿才算完成）：
  ```bash
  cd rust && cargo test --workspace
  cd rust && cargo clippy --workspace --all-targets -- -D warnings
  node rust/scripts/check-zero-io.mjs
  ```
- **不改契约面**：不动 RPC registry、wire 事件、TS 翻译层（本变更只影响工具执行的错误路径，不新增客户端可见事件）。
- **测试命名**：Given-When-Then 句式（`should ... when ...`），放在对应组件的既有内嵌 `#[cfg(test)]` 模块里，不新建测试文件。
- **commit 规范**：Conventional Commit；不加 co-author、不暴露 agent 身份。
- **锁 key 为验证后路径**：Edit/Write 在 `validate_tool_path` 之后拿到的绝对路径字符串；不做 canonicalize（`DynFileSystem` 没有该操作），符号链接/不同拼写别名是已接受的残留缺口。

## 设计决策（与用户已对齐）

### 问题

`Edit` 是无锁的 read-modify-write（`rust/crates/mirri-core/src/tools/edit.rs` 的 `edit_file`：`read_text → 字符串替换 → write_text` 整文件覆盖）。两个并行 agent 改同一文件的不同区域时，后写者基于过期快照整文件覆盖先写者的修改，**双方都报成功、静默丢失**。`old_string` 精确匹配只保护被编辑的片段，不保护文件其余部分。

### 选定方案：进程内 per-path 锁（A）+ 写回前全量比对（B）

**A — 互斥**：进程级全局 `HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>`（`LazyLock` + `std::sync::Mutex` 外壳，临界区内无 await）。同路径所有 Edit/Write 变更串行化；不同路径互不阻塞。等待有 30s 超时，超时报错（防呆死锁）。

**B — 乐观校验（仅 Edit）**：Edit 在读文件时保留原始 raw 内容；写回前用注入的 fs **重读一次**并做 `String == String`（memcmp，~10GB/s）逐字节比对，不一致即报冲突错误（对齐既有 `old_string not found ... file contents may be out of date` 的措辞风格，引导 agent 重新 Read 再改）。因为持有 A 的锁，校验失败必然来自进程外写入者。

**为什么用全量比对而不是 mtime/hash**：

- `FileStat.mtime` 是**秒级**（`as_secs()`，mirri-os `fs.rs`），同秒内两次写入会漏检——stat 快速路径不可靠，直接放弃；
- hash 要算两次且理论碰撞非零，而 Edit 本来就把原文握在内存里，memcmp 更快更准，hash 是纯多余的中介；
- 全量比对的代价 = 写前多读一次（温页缓存下几十 μs），相对 Edit 本身"一读一写整文件"约 +30–40% I/O，绝对值不可见。

### 已否决的备选

| 方案 | 否决理由 |
|---|---|
| OS advisory lock（flock / fd-lock） | 只对同样自觉加锁的参与者生效；用户 IDE、git 不遵守；跨进程冲突归 git 管；额外依赖 |
| mtime + size 两级检查 | `FileStat.mtime` 秒级粒度，1 秒盲区真实存在 |
| 写前 hash 比对 | 两次 hash 严格劣于一次 memcmp（原文已在手）；碰撞非零 |
| 锁放 `DynFileSystem` 层 | fs 层不知道 read 和 write 是同一事务，锁不住 read→write 窗口；SSH 等远程后端被迫背上锁逻辑 |
| temp + rename 原子写 | 解决撕裂写入，不解决丢失更新；可作独立后续改进 |

### 已接受的残留风险

- **TOCTOU 上界**：校验通过到 `write_text` 之间（微秒级窗口）进程外写入仍可能插入——乐观校验的固有上界；
- **路径别名**：锁 key 是验证后路径，symlink 或不同 workspace 的不同拼写会拿到不同的锁（与既有 path-security 姿态一致；后续可给 `FileSystem` trait 加 `canonicalize` 收紧）；
- **锁表无淘汰**：每个编辑过的路径留一个 ~百字节的 `Arc<Mutex<()>>`，长生命周期进程可忽略；后续可在 strong_count == 1 时淘汰；
- **取消安全**：`OwnedMutexGuard` 随 future 取消自动 drop 释放，无需显式清理。

### 交互语义（落地后的最终行为）

| 并发场景 | 行为 |
|---|---|
| 两个 Edit 同文件不同区域（同进程） | 串行执行，两处修改都保留，双方成功 |
| 两个 Edit 同文件重叠区域 | 第二个的 `old_string` 匹配的是第一个改后的内容，自然成立或按既有规则报 not found |
| Edit 进行中，进程外（编辑器/git）改了该文件 | Edit 写回前校验失败 → 显式冲突错误，不写盘 |
| Edit 进行中，同进程 Write append 同文件 | append 等 Edit 释放锁后再追加，两处修改都保留 |
| Write overwrite 与 Edit 并发 | 二者串行化，整体为"其一完整生效"而非交错 |
| 持锁方异常/被取消 | guard drop 自动释放锁 |
| 等锁超过 30s | 报错 `timed out ... re-read the file and retry` |

### 范围外（明确不做）

- TS v1 / v2 引擎的同类问题（AGENTS 约定：减少 TS 侧改动，Rust 是未来内核）；上游文档与代码不符的工具描述修正，另行处理；
- `DynFileSystem` trait 面变更（canonicalize、原子写）；
- 其他工具（Read 等）不涉及。

---

## Task 1: `file_mutation_guard` 模块（锁表 + 校验函数）

**Files:**
- Create: `rust/crates/mirri-core/src/tools/file_mutation_guard.rs`
- Modify: `rust/crates/mirri-core/src/tools/mod.rs`（模块注册，按字母序插在 `fetch_url` 之后）

**Interfaces:**
- Consumes: `mirri_os::DynFileSystem`（既有）
- Produces（Task 2/3 依赖）:
  - `pub const FILE_MUTATION_LOCK_TIMEOUT: Duration`（30s）
  - `pub struct FileMutationGuard` + `pub async fn acquire(path: &Path) -> Result<Self, String>`
  - `pub async fn verify_unchanged(fs: &dyn DynFileSystem, path: &Path, expected_raw: &str) -> Result<(), String>`

- [ ] **Step 1: 写红测试**（新文件内嵌 tests 模块；模块本体先只有最小骨架让编译通过）

```rust
//! Process-wide per-path serialization for file mutations (Edit / Write).
//! （完整模块 doc 注释见 Step 3）

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::time::Duration;

    use mirri_os::DynFileSystem;

    use super::{verify_unchanged, FileMutationGuard, LOCK_TABLE};

    fn local_fs() -> Arc<dyn DynFileSystem> {
        Arc::new(mirri_os::LocalFs::new())
    }

    /// Given a path with no other holder, When acquiring the mutation lock,
    /// Then it is granted immediately.
    #[tokio::test]
    async fn should_acquire_immediately_when_no_other_holder_exists_for_the_path() {
        let path = std::env::temp_dir().join("mirri-guard-test-free.txt");
        let guard = FileMutationGuard::acquire(&path).await;
        assert!(guard.is_ok(), "uncontended acquire must succeed");
    }

    /// Given another holder keeps the path's lock, When acquiring with a
    /// short timeout, Then the acquire fails with a timeout message.
    #[tokio::test]
    async fn should_error_when_acquire_times_out_while_another_holder_keeps_the_lock() {
        let path = std::env::temp_dir().join("mirri-guard-test-held.txt");
        let lock = {
            let mut table = LOCK_TABLE.lock().unwrap_or_else(|e| e.into_inner());
            Arc::clone(table.entry(path.clone()).or_default())
        };
        let _held = lock.lock_owned().await;
        let result =
            FileMutationGuard::acquire_with_timeout(&path, Duration::from_millis(10)).await;
        let message = result.expect_err("must time out while the lock is held");
        assert!(message.contains("timed out"), "message was: {message}");
    }

    /// Given the file content changed since the caller read it, When
    /// verifying before write, Then the verification reports a conflict.
    #[tokio::test]
    async fn should_report_conflict_when_file_content_changed_between_read_and_verify() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("changed.txt");
        std::fs::write(&path, "original\n").unwrap();
        let fs = local_fs();
        assert!(
            verify_unchanged(fs.as_ref(), &path, "tampered\n")
                .await
                .is_err(),
            "changed content must be a conflict"
        );
        assert!(
            verify_unchanged(fs.as_ref(), &path, "original\n")
                .await
                .is_ok(),
            "identical content must pass"
        );
    }

    /// Given the file was removed since the caller read it, When verifying
    /// before write, Then the verification reports a conflict (not a crash).
    #[tokio::test]
    async fn should_report_conflict_when_file_was_removed_between_read_and_verify() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("gone.txt");
        std::fs::write(&path, "original\n").unwrap();
        std::fs::remove_file(&path).unwrap();
        let fs = local_fs();
        let message = verify_unchanged(fs.as_ref(), &path, "original\n")
            .await
            .expect_err("a removed file must be a conflict");
        assert!(message.contains("changed"), "message was: {message}");
    }
}
```

- [ ] **Step 2: 跑测试确认红**（符号未定义，编译失败即红）

Run: `cd rust && cargo test -p mirri-core file_mutation_guard`
Expected: FAIL（`LOCK_TABLE` / `FileMutationGuard` / `verify_unchanged` 未定义）

- [ ] **Step 3: 写实现**（完整文件；上面的 tests 模块追加在文件末尾）

```rust
//! Process-wide per-path serialization for file mutations (Edit / Write).
//!
//! Two guarantees for read-modify-write file tools running concurrently in
//! one engine process (parallel agents, swarms, multi-session servers):
//!
//! 1. **Mutual exclusion**: `acquire` returns a guard that serializes every
//!    mutation of the same path, so two edits can never interleave their
//!    read/write pairs (the classic silent lost-update).
//! 2. **External-change detection**: `verify_unchanged` re-reads the file
//!    right before the write and compares the raw bytes with what the caller
//!    originally read, turning out-of-process modifications (editor, git,
//!    second CLI instance) into an explicit conflict error instead of a
//!    silent clobber.
//!
//! Purely in-memory (zero-IO): the lock table never touches the filesystem;
//! the verification re-read goes through the injected `DynFileSystem`.
//!
//! Known residual gaps, accepted by design:
//! - TOCTOU window between `verify_unchanged` and the subsequent write
//!   (inherent to optimistic checking; nanoseconds wide).
//! - Path aliasing: the key is the post-validation path, so the same file
//!   reached via different spellings or symlinks gets different locks.
//! - Lock-table entries are never evicted (one small allocation per
//!   distinct edited path per process).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use mirri_os::DynFileSystem;

/// How long a mutation waits for a concurrent same-path mutation before
/// failing. Generous on purpose: edits are fast, and a spurious timeout
/// fails a tool call that would otherwise succeed.
pub const FILE_MUTATION_LOCK_TIMEOUT: Duration = Duration::from_secs(30);

static LOCK_TABLE: LazyLock<Mutex<HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Handle proving the caller owns the per-path mutation lock.
pub struct FileMutationGuard {
    // Held for the guard's lifetime; dropped (releasing the lock) when the
    // mutation finishes, errors, or the future is cancelled.
    _lock: tokio::sync::OwnedMutexGuard<()>,
}

impl FileMutationGuard {
    /// Acquire the process-wide mutation lock for `path` (default timeout).
    pub async fn acquire(path: &Path) -> Result<Self, String> {
        Self::acquire_with_timeout(path, FILE_MUTATION_LOCK_TIMEOUT).await
    }

    /// Acquire the mutation lock, failing after `timeout` of waiting.
    pub async fn acquire_with_timeout(path: &Path, timeout: Duration) -> Result<Self, String> {
        let lock = {
            let mut table = LOCK_TABLE.lock().unwrap_or_else(|e| e.into_inner());
            Arc::clone(table.entry(path.to_path_buf()).or_default())
        };
        match tokio::time::timeout(timeout, lock.lock_owned()).await {
            Ok(guard) => Ok(Self { _lock: guard }),
            Err(_) => Err(format!(
                "timed out after {timeout:?} waiting for a concurrent edit of '{}' to \
                 finish; re-read the file and retry once the other editor is done",
                path.display()
            )),
        }
    }
}

/// Verify the file on disk still matches the raw content the caller read
/// before mutating it. Returns `Err` with a conflict message when an
/// external writer changed (or removed) the file in the meantime.
pub async fn verify_unchanged(
    fs: &dyn DynFileSystem,
    path: &Path,
    expected_raw: &str,
) -> Result<(), String> {
    let current = match fs.read_text(path).await {
        Ok(text) => text,
        Err(error) => {
            return Err(format!(
                "'{}' changed while it was being edited (re-read failed: {error}); \
                 please re-read the file and retry",
                path.display()
            ));
        }
    };
    if current != expected_raw {
        return Err(format!(
            "'{}' was modified by another writer while it was being edited; \
             the file contents may be out of date. Please re-read the file and retry",
            path.display()
        ));
    }
    Ok(())
}
```

`tools/mod.rs` 注册（`fetch_url` 行后插入，保持字母序）：

```rust
pub mod file_mutation_guard;
```

- [ ] **Step 4: 跑测试确认绿**

Run: `cd rust && cargo test -p mirri-core file_mutation_guard`
Expected: PASS（4 个测试全绿）

- [ ] **Step 5: Commit**

```bash
git add rust/crates/mirri-core/src/tools/file_mutation_guard.rs rust/crates/mirri-core/src/tools/mod.rs
git commit -m "feat(rust): add per-path file mutation guard with pre-write verification"
```

---

## Task 2: Edit 接入（锁 + 写回前校验）

**Files:**
- Modify: `rust/crates/mirri-core/src/tools/edit.rs`（`edit_file` 函数 + tests 模块追加测试）

**Interfaces:**
- Consumes: Task 1 的 `FileMutationGuard::acquire`、`verify_unchanged`
- Produces: 无新接口（行为变更）

- [ ] **Step 1: 写红测试**（追加到 `edit.rs` 既有 `mod tests`；先写 fake double）

```rust
    // -------------------------------------------------------------------------
    // Concurrent-edit safety (file mutation guard)
    // -------------------------------------------------------------------------

    /// A fake filesystem that returns different content on every read, as
    /// if an external writer modified the file between the edit's read and
    /// its write.
    #[derive(Default)]
    struct ExternallyChangedFs {
        writes: Mutex<Vec<String>>,
    }

    impl mirri_os::DynFileSystem for ExternallyChangedFs {
        fn read_text<'a>(&'a self, _path: &'a Path) -> mirri_os::DynFsFuture<'a, anyhow::Result<String>> {
            Box::pin(async {
                // First read: the file as the edit sees it. Second read (the
                // pre-write verification): an external writer already
                // rewrote it.
                static READS: std::sync::atomic::AtomicUsize =
                    std::sync::atomic::AtomicUsize::new(0);
                let n = READS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if n == 0 {
                    Ok("hello world\n".to_string())
                } else {
                    Ok("EXTERNAL WRITE\n".to_string())
                }
            })
        }

        fn write_text<'a>(
            &'a self,
            _path: &'a Path,
            content: &'a str,
        ) -> mirri_os::DynFsFuture<'a, anyhow::Result<()>> {
            Box::pin(async {
                self.writes
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(content.to_string());
                Ok(())
            })
        }
    }

    /// Given the file is modified by an external writer between the edit's
    /// read and its write, When the edit completes, Then it fails with a
    /// conflict error and writes nothing.
    #[tokio::test]
    async fn should_fail_without_writing_when_the_file_changes_externally_during_the_edit() {
        let fake = Arc::new(ExternallyChangedFs::default());
        let result = edit_file(
            Arc::clone(&fake) as Arc<dyn DynFileSystem>,
            "victim.txt".to_string(),
            "world".to_string(),
            "rust".to_string(),
            false,
        )
        .await;
        assert!(result.is_error, "an externally changed file must conflict");
        let text = output_text(&result);
        assert!(
            text.contains("modified by another writer"),
            "error must explain the conflict, was: {text}"
        );
        let writes = fake.writes.lock().unwrap_or_else(|e| e.into_inner());
        assert!(writes.is_empty(), "a conflicted edit must not write");
    }

    /// A fake filesystem that yields once inside every read and write,
    /// forcing concurrently-joined read-modify-write cycles to interleave on
    /// the single-threaded test runtime.
    #[derive(Default)]
    struct YieldingFs {
        content: Mutex<String>,
    }

    impl mirri_os::DynFileSystem for YieldingFs {
        fn read_text<'a>(&'a self, _path: &'a Path) -> mirri_os::DynFsFuture<'a, anyhow::Result<String>> {
            Box::pin(async {
                tokio::task::yield_now().await;
                Ok(self.content.lock().unwrap_or_else(|e| e.into_inner()).clone())
            })
        }

        fn write_text<'a>(
            &'a self,
            _path: &'a Path,
            content: &'a str,
        ) -> mirri_os::DynFsFuture<'a, anyhow::Result<()>> {
            Box::pin(async {
                tokio::task::yield_now().await;
                *self.content.lock().unwrap_or_else(|e| e.into_inner()) = content.to_string();
                Ok(())
            })
        }
    }

    /// Given two concurrent edits of the same file touching different
    /// regions, When both complete, Then both replacements survive — the
    /// second editor must re-read the post-first-edit content instead of
    /// silently clobbering the first edit's write.
    #[tokio::test]
    async fn should_preserve_both_edits_when_two_edits_run_concurrently_on_the_same_file() {
        let fs = Arc::new(YieldingFs::default());
        *fs.content.lock().unwrap_or_else(|e| e.into_inner()) = "a b c\n".to_string();
        let fs1: Arc<dyn DynFileSystem> = Arc::clone(&fs);
        let fs2: Arc<dyn DynFileSystem> = Arc::clone(&fs);
        let (r1, r2) = tokio::join!(
            edit_file(fs1, "shared.txt".to_string(), "a".to_string(), "x".to_string(), false),
            edit_file(fs2, "shared.txt".to_string(), "c".to_string(), "z".to_string(), false),
        );
        assert!(!r1.is_error, "first edit failed: {}", output_text(&r1));
        assert!(!r2.is_error, "second edit failed: {}", output_text(&r2));
        assert_eq!(
            *fs.content.lock().unwrap_or_else(|e| e.into_inner()),
            "x b z\n",
            "both edits must survive a concurrent same-file edit"
        );
    }
```

注意：`ExternallyChangedFs` 里用 `static READS` 是刻意的——该测试只跑一次、每次进程只跑一次该 fake，用 static 计数最简。若实现者担心测试隔离，可把计数挪进 struct 字段（`reads: AtomicUsize`），二选一均可。

- [ ] **Step 2: 跑测试确认红**

Run: `cd rust && cargo test -p mirri-core tools::edit`
Expected: `should_fail_without_writing_when_the_file_changes_externally_during_the_edit` FAIL（当前无校验，edit 直接写成功）；`should_preserve_both_edits_when_two_edits_run_concurrently_on_the_same_file` FAIL（最终内容是 `a b z\n`，第一处修改丢失）。其余既有测试 PASS。

- [ ] **Step 3: 实现**（改 `edit_file`，见 `rust/crates/mirri-core/src/tools/edit.rs:148`）

在 no-op 短路之后、`read_text` 之前取锁；在 `write_text` 之前校验。原始 raw 内容在 CRLF 归一化前保留：

```rust
async fn edit_file(
    fs: Arc<dyn DynFileSystem>,
    path: String,
    old_string: String,
    new_string: String,
    replace_all: bool,
) -> ToolResult {
    // No-op edits succeed immediately without touching the file: the model
    // asked for no change, so there is nothing to verify or write.
    if old_string == new_string {
        return ToolResult::success(format!(
            "Edit skipped: 'old_string' and 'new_string' are identical; no \
             change was made to '{path}'"
        ));
    }

    let file_path = Path::new(&path);

    // Serialize same-path mutations process-wide (parallel agents / swarms
    // share this lock table) before reading, so two edits can never
    // interleave their read/write pairs.
    let _guard = match crate::tools::file_mutation_guard::FileMutationGuard::acquire(file_path)
        .await
    {
        Ok(guard) => guard,
        Err(message) => return ToolResult::error(format!("Edit failed for '{path}': {message}")),
    };

    let raw_text = match fs.read_text(file_path).await {
        Ok(text) => text,
        Err(error) => {
            return ToolResult::error(format!("Edit failed to read '{path}': {error}"));
        }
    };

    // CRLF awareness: when the file uses `\r\n` line endings, matching is done
    // on LF-normalized copies (so an `old_string` written with plain `\n`
    // matches the file, and vice versa), and the result is converted back to
    // CRLF before writing so the file's line-ending style is preserved.
    let uses_crlf = raw_text.contains("\r\n");
    let (text, old_string, new_string) = if uses_crlf {
        (
            raw_text.replace("\r\n", "\n"),
            old_string.replace("\r\n", "\n"),
            new_string.replace("\r\n", "\n"),
        )
    } else {
        (raw_text.clone(), old_string, new_string)
    };

    // （…occurrence_count 校验与替换逻辑保持原样，将原文变量名 text 的来源
    // 改为上面的归一化 text，无其它改动…）

    // The file must still be byte-for-byte what we read — otherwise an
    // external writer (editor, git, another process) would be silently
    // clobbered by this edit's full-file rewrite.
    if let Err(message) = crate::tools::file_mutation_guard::verify_unchanged(
        fs.as_ref(),
        file_path,
        &raw_text,
    )
    .await
    {
        return ToolResult::error(format!("Edit failed for '{path}': {message}"));
    }

    // （…fs.write_text 写回逻辑保持原样…）
}
```

实现要点：
1. 原实现里 `let text = ... fs.read_text` 改为先绑 `raw_text`，再从 `raw_text` 派生归一化 `text`（LF 分支 `raw_text.clone()`，CRLF 分支 `raw_text.replace(...)`——与原代码形状一致）；
2. `verify_unchanged` 比对的是 **raw 原文**（归一化前），不是归一化后的 text；
3. 校验放在替换计算之后、`write_text` 之前（校验失败时一个字节都不写）;
4. 既有 occurrence/替换/写回分支逻辑零改动。

- [ ] **Step 4: 跑测试确认绿（含既有全部 edit 测试）**

Run: `cd rust && cargo test -p mirri-core tools::edit`
Expected: PASS（新 2 + 既有 12 全绿）

- [ ] **Step 5: Commit**

```bash
git add rust/crates/mirri-core/src/tools/edit.rs
git commit -m "feat(rust): serialize same-path edits and detect external changes before write"
```

---

## Task 3: Write 接入（只取锁，不校验）

**Files:**
- Modify: `rust/crates/mirri-core/src/tools/write.rs`（`write_file` 函数 + tests 模块追加测试）

**Interfaces:**
- Consumes: Task 1 的 `FileMutationGuard::acquire`
- Produces: 无新接口（行为变更）

- [ ] **Step 1: 写红测试**（追加到 `write.rs` 既有 `mod tests`）

```rust
    // -------------------------------------------------------------------------
    // Concurrent-mutation safety (file mutation guard)
    // -------------------------------------------------------------------------

    /// A fake filesystem that yields once inside every read, write, and
    /// append, forcing concurrently-joined mutations to interleave on the
    /// single-threaded test runtime.
    #[derive(Default)]
    struct YieldingFs {
        content: Mutex<String>,
    }

    impl mirri_os::DynFileSystem for YieldingFs {
        fn read_text<'a>(&'a self, _path: &'a Path) -> mirri_os::DynFsFuture<'a, anyhow::Result<String>> {
            Box::pin(async {
                tokio::task::yield_now().await;
                Ok(self.content.lock().unwrap_or_else(|e| e.into_inner()).clone())
            })
        }

        fn write_text<'a>(
            &'a self,
            _path: &'a Path,
            content: &'a str,
        ) -> mirri_os::DynFsFuture<'a, anyhow::Result<()>> {
            Box::pin(async {
                tokio::task::yield_now().await;
                *self.content.lock().unwrap_or_else(|e| e.into_inner()) = content.to_string();
                Ok(())
            })
        }

        fn append<'a>(
            &'a self,
            _path: &'a Path,
            contents: &'a str,
        ) -> mirri_os::DynFsFuture<'a, anyhow::Result<()>> {
            Box::pin(async {
                tokio::task::yield_now().await;
                self.content
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push_str(contents);
                Ok(())
            })
        }
    }

    /// Given an append lands while an edit of the same file is in flight,
    /// When both complete, Then both changes survive — the append must not
    /// be silently clobbered by the edit's full-file rewrite, and the edit
    /// must re-read the post-append content.
    #[tokio::test]
    async fn should_not_lose_an_append_that_lands_during_a_concurrent_edit() {
        let fs = Arc::new(YieldingFs::default());
        *fs.content.lock().unwrap_or_else(|e| e.into_inner()) = "a\n".to_string();
        let edit_fs: Arc<dyn DynFileSystem> = Arc::clone(&fs);
        let write_fs: Arc<dyn DynFileSystem> = Arc::clone(&fs);
        let (edit_result, write_result) = tokio::join!(
            crate::tools::edit::edit_file_for_tests(edit_fs, "shared.log".to_string(), "a".to_string(), "x".to_string(), false),
            write_file(write_fs, "shared.log".to_string(), "tail\n".to_string(), WriteMode::Append),
        );
        assert!(!edit_result.is_error, "edit failed: {}", {
            match &edit_result.output {
                crate::tool::ToolOutput::Text(t) => t.clone(),
                crate::tool::ToolOutput::Parts(_) => String::new(),
            }
        });
        assert!(!write_result.is_error, "append failed");
        let final_content = fs.content.lock().unwrap_or_else(|e| e.into_inner()).clone();
        assert!(
            final_content.contains('x') && final_content.contains("tail\n"),
            "both the edit and the append must survive, got: {final_content:?}"
        );
    }
```

注：测试引用了 `edit_file_for_tests`——`edit.rs` 的 `edit_file` 目前是私有自由函数。在本任务给它加一个 `#[cfg(test)]` 的测试出口（或将 `edit_file` 提为 `pub(crate)`），任选其一：

```rust
/// Test-only entry so `write.rs` tests can drive a real edit against a fake
/// filesystem without going through the full tool plumbing.
#[cfg(test)]
pub(crate) async fn edit_file_for_tests(
    fs: Arc<dyn DynFileSystem>,
    path: String,
    old_string: String,
    new_string: String,
    replace_all: bool,
) -> ToolResult {
    edit_file(fs, path, old_string, new_string, replace_all).await
}
```

- [ ] **Step 2: 跑测试确认红**

Run: `cd rust && cargo test -p mirri-core tools::write`
Expected: `should_not_lose_an_append_that_lands_during_a_concurrent_edit` FAIL（无锁时：edit 读到 `a\n`，append 先落盘成 `a\ntail\n`，edit 整文件写回 `x\n`——append 丢失）。其余既有测试 PASS。

- [ ] **Step 3: 实现**（改 `write_file`，见 `rust/crates/mirri-core/src/tools/write.rs:166`）

在函数最前面取锁（overwrite 与 append 都覆盖；Write 是有意的整文件覆盖，不做写前校验）：

```rust
async fn write_file(
    fs: Arc<dyn DynFileSystem>,
    path: String,
    content: String,
    mode: WriteMode,
) -> ToolResult {
    // Serialize same-path mutations process-wide so a Write can never
    // interleave with a concurrent Edit's read/write pair (or another
    // Write). Blind overwrite is Write's contract, so there is no
    // pre-write content verification here — only mutual exclusion.
    let file_path = Path::new(&path);
    let _guard =
        match crate::tools::file_mutation_guard::FileMutationGuard::acquire(file_path).await {
            Ok(guard) => guard,
            Err(message) => {
                return ToolResult::error(format!("Write failed for '{path}': {message}"))
            }
        };

    match mode {
        WriteMode::Overwrite => match fs.write_text(file_path, &content).await {
            Ok(()) => ToolResult::success(format!("Wrote {} bytes to {}", content.len(), path)),
            Err(error) => ToolResult::error(format!("Write failed for '{path}': {error}")),
        },
        WriteMode::Append => {
            // （…既有 mkdir + append 逻辑原样保留，仅 `let file_path =
            // Path::new(&path);` 一行因上移而删除…）
        }
    }
}
```

- [ ] **Step 4: 跑测试确认绿（含既有全部 write 测试）**

Run: `cd rust && cargo test -p mirri-core tools::write`
Expected: PASS（新 1 + 既有全绿）

- [ ] **Step 5: Commit**

```bash
git add rust/crates/mirri-core/src/tools/write.rs rust/crates/mirri-core/src/tools/edit.rs
git commit -m "feat(rust): serialize Write with concurrent same-path edits via mutation guard"
```

---

## Task 4: 全量门禁 + changeset

**Files:**
- Create: `.changeset/`（经 gen-changesets skill 生成）

**Interfaces:** 无

- [ ] **Step 1: 全量测试**

Run: `cd rust && cargo test --workspace`
Expected: 全绿（重点观察 mirri-core 及依赖 edit/write 行为的集成测试；若有测试因锁表是进程级全局而在并行测试中撞路径，给测试路径加唯一性，不允许改实现迁就测试）

- [ ] **Step 2: clippy 零告警**

Run: `cd rust && cargo clippy --workspace --all-targets -- -D warnings`
Expected: 无输出，exit 0

- [ ] **Step 3: zero-IO 守卫**

Run: `node rust/scripts/check-zero-io.mjs`
Expected: 0 命中，exit 0（`file_mutation_guard.rs` 是纯内存实现；测试模块会被剥离，不受限）

- [ ] **Step 4: changeset**

按 `.agents/skills/gen-changesets/SKILL.md` 生成（bump 级别默认 `minor`；不自行决定 `major`）。变更面向用户的表述：并发编辑同一文件时不再静默丢失修改，外部修改在写回前会被检测并报冲突。

- [ ] **Step 5: Commit**

```bash
git add .changeset/
git commit -m "chore: add changeset for file mutation concurrency guard"
```

---

## 验收清单（实施完成后逐项核对）

- [ ] 两个并行 Edit 同文件不同区域：都成功且两处修改都在（Task 2 测试）
- [ ] Edit 期间外部修改文件：Edit 报冲突错误、不写盘（Task 2 测试）
- [ ] Edit 进行中 append 同文件：两处修改都保留（Task 3 测试）
- [ ] 等锁超时：报含 `timed out` 的错误而非死等（Task 1 测试）
- [ ] no-op edit（old == new）仍然不取锁不读文件（既有测试不回归）
- [ ] CRLF 行为不变（既有 `should_preserve_crlf_*` 测试不回归）
- [ ] 三项质量门禁全绿
