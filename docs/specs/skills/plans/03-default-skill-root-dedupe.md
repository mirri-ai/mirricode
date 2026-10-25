# ADR-011 默认技能根路径归一去重 · 实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 消除「工作区根 = 用户主目录（或经符号链接别名）」时默认技能根被多通道重复扫描、逐技能产生跨来源遮蔽告警（单次启动上百条）的缺陷——按 ADR-011（docs/specs/skills-architecture.md）实现默认根清单的归一化去重，最终归属零变化。

**Architecture:** 去重逻辑实现为「接收有序根清单（通道 + 目录）的共享编排函数」，内嵌于构造期首扫（`WorkspaceScope::new` → `discover_default_skill_roots`）与装配层重放（`apply_skill_discovery`）共用的同一发现定义。归一化 = 词法归一（复用 `mirri_core::session::workdir_key::lexically_resolve`）+ 符号链接解析（`std::fs::canonicalize`，归一失败回退原路径）；同一归一化路径保留序列中后到通道。装配层重放持完整四通道清单（含用户品牌目录）统一去重，不再委托三根编排后补扫。

**Tech Stack:** Rust（mirri-core / mirri-embedded crates），`cargo test -p` 分包验证。

## Global Constraints

- ADR-011（docs/specs/skills-architecture.md 文末）是本计划唯一设计依据；任何偏离须回到设计层修订。
- 去重前后 catalog 内容与路径归属**零变化**：同一路径只消除重复扫描，保留的通道（用户级）必须是现状后到覆盖方。
- 归一化失败（canonicalize 出错、目录不存在）不丢技能：按原路径参与去重。
- 显式 `skillDirs`、额外目录（`extra_skill_dirs`）、插件根**不参与**去重（ADR-011 已接受的残余）。
- 测试命名遵循 Given-When-Then（`should ... when ...`），禁止提及内部函数名。
- 不加 co-author 署名；commit message 用 Conventional Commit 风格。
- 本计划只动 rust 核心；TS 侧零改动。

## File Structure

| 文件 | 职责 |
|---|---|
| `rust/crates/mirri-core/src/scope/workspace_scope.rs` | 默认根三通道清单构造 + 构造期/重放共用发现定义；去重测试 |
| `rust/crates/mirri-embedded/src/bootstrap.rs` | `apply_skill_discovery` 重放改为持完整四通道清单统一去重；重放层测试 |
| `docs/specs/skills-context.md` | TS dotfiles 同构隐患留档（评审 comment c3） |

不新建文件——去重函数放 `workspace_scope.rs`（它属于 scope 层编排语义，`skill.rs` 是通用 catalog 不感知通道）。

---

### Task 1: 根清单构造与归一化去重纯函数（TDD）

**Files:**
- Modify: `rust/crates/mirri-core/src/scope/workspace_scope.rs`（`discover_default_skill_roots` 改造 + 新增共享函数）

**Interfaces:**
- Produces:
  - `pub struct SkillRootChannel { pub source: SkillSource, pub dir: PathBuf }`（`workspace_scope.rs` 内定义，`SkillSource` 来自 `crate::skill`）
  - `fn default_skill_root_channels(cwd: &Path) -> Vec<SkillRootChannel>` — 构造期三通道清单：`cwd/.agents/skills`(Project)、`cwd/.mirri-code/skills`(Project)、`$HOME/.agents/skills`(User)；`$HOME` 未设时只有前两条。
  - `fn dedupe_skill_roots(channels: Vec<SkillRootChannel>) -> Vec<SkillRootChannel>` — 归一化去重，保留后到通道；顺序保持原序列。
- Consumes: `mirri_core::session::workdir_key::lexically_resolve`（已 `pub use` 于 `mirri_core::session`）。

**设计要点（实现者必读）：**
- 归一化函数：先 `lexically_resolve`（词法归一），再 `std::fs::canonicalize`（符号链接解析）；canonicalize 失败（含目录不存在）时退回词法归一结果。**不要用 dunce**（仓库无此依赖，不新增）；Windows verbatim 前缀（`\\?\`）因全序列走同一归一函数而自然一致——同函数产出要么全带前缀要么全不带，比较安全。
- **归一化未按 ADR「参数化注入」字面实现**（直接用 `std::fs::canonicalize`，无注入点）：这是有意简化——纯函数测试走的都是 canonicalize 失败的词法回退分支（临时路径不存在或含 `..`），不触文件系统；fs 触碰路径的测试用真实 tempdir。实施时不需要改设计，但**不得**声称「与 ADR 注入惯例对齐」。
- 去重语义：按归一化路径为键，**后到通道胜**（保留其原序列位置，移除先到的重复者）。例：`[P:/home/x, U:/home/x]` → `[U:/home/x]`。⚠️ **不要写成「先到者保留」的 HashMap insert-if-absent 版本**——那会改变归属（事故场景中项目通道在前、用户通道在后，必须留 User）。
- `discover_default_skill_roots` 的公开签名 `pub fn discover_default_skill_roots(&self)` 不变（server 侧有调用）；内部改为：构造三通道清单 → 去重 → 逐条 `skills.discover(channel.dir, channel.source)`。

**⚠️ 测试并发纪律（M1，必读）：** 本文件既有的两个 HOME pin 测试能无锁共存，是因为它们 pin 的 HOME 都是**不含技能的空 tempdir**——互相读到对方的值也不破坏断言。新测试 pin 的 HOME **含 `.agents/skills/dup-skill`**，破坏了该不变量：并行的空 catalog 测试读到它会挂（方向 A），本测试读到别人的空 fake_home 也会挂（方向 B）。因此：

1. 在 `mod tests` 内新增与本文件 `mirri-embedded` 侧 `test_env::home_lock`（bootstrap.rs:1044-1056）同构的锁：

```rust
    /// Crate-wide `$HOME` lock for tests that pin a NON-EMPTY home (a home
    /// with skills in it). The pre-existing pin tests use empty tempdirs,
    /// which are mutually safe; the ADR-011 tests pin homes with content,
    /// so they must serialize against every other HOME reader via this
    /// lock, held across the whole pin window.
    fn home_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> =
            std::sync::OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
```

2. **既有两个 pin 测试（`should_have_empty_skill_catalog_when_no_skills_dir_exists`、`should_rediscover_default_roots_when_replayed_after_a_clear`）的 pin 窗口也包进 `home_lock()`**——新测试的 HOME 含内容后，它们不再天然安全。
3. HOME 恢复用 `match previous_home { Some(v) => set_var, None => remove_var }` 形态（原 HOME 未设时不能泄漏 pin 值）。

- [ ] **Step 1: 写失败测试**（追加到 `workspace_scope.rs` 的 `mod tests`）

```rust
    // Given a workspace whose project skill dir and user skill dir resolve
    // to the same physical directory (workspace root == $HOME), when the
    // default roots are discovered, then the directory is scanned once as
    // the user channel — no skill carries the project label.
    // NOTE: this test is a zero-change REGRESSION GUARD (current code also
    // yields source == User via later-wins) — do not "fix" the assertion if
    // it never went red; the red/green switch lives in Task 2's warning
    // capture test.
    #[test]
    fn should_discover_shared_dir_once_as_user_when_workspace_root_is_home() {
        let home = tempfile::tempdir().expect("home tempdir");
        let skills_dir = home.path().join(".agents/skills/dup-skill");
        std::fs::create_dir_all(&skills_dir).expect("create skill dir");
        std::fs::write(skills_dir.join("SKILL.md"), "# Dup\n").expect("write SKILL.md");
        // brand dir absent — fine, discovery skips missing dirs

        // Non-empty HOME pin: hold the crate lock across the whole window.
        let _guard = home_lock();
        let previous_home = std::env::var_os("HOME");
        unsafe { std::env::set_var("HOME", home.path()) };
        // When a workspace is created with cwd == $HOME (the collision case)
        let workspace = WorkspaceScope::new(
            WorkspaceId::new("ws-home-collision"),
            home.path().to_path_buf(),
            WorkspaceScopeConfig::default(),
        );
        match previous_home {
            Some(value) => unsafe { std::env::set_var("HOME", value) },
            None => unsafe { std::env::remove_var("HOME") },
        }

        // Then the skill is present exactly once, labeled User
        let skill = workspace.skills().get("dup-skill").expect("skill discovered");
        assert_eq!(skill.source, SkillSource::User);
    }
```

同 `mod tests` 再加符号链接用例（同样持 `home_lock`、`#[cfg(unix)]`——`std::os::unix::fs::symlink` 仅 Unix；同样注明是零变化回归护栏）：

```rust
    // Given a workspace reached through a symlink into the user's skill
    // territory, when the default roots are discovered, then the symlinked
    // project channel is recognized as the same physical dir and skipped
    // (zero-change regression guard — see the home-collision test's note).
    #[cfg(unix)]
    #[test]
    fn should_skip_project_channel_when_symlink_resolves_to_user_channel_dir() {
        let home = tempfile::tempdir().expect("home tempdir");
        let skills_dir = home.path().join(".agents/skills/link-skill");
        std::fs::create_dir_all(&skills_dir).expect("create skill dir");
        std::fs::write(skills_dir.join("SKILL.md"), "# Link\n").expect("write SKILL.md");

        let link_parent = tempfile::tempdir().expect("link parent tempdir");
        let link = link_parent.path().join("linked-ws");
        std::os::unix::fs::symlink(home.path(), &link).expect("symlink");

        let _guard = home_lock();
        let previous_home = std::env::var_os("HOME");
        unsafe { std::env::set_var("HOME", home.path()) };
        let workspace = WorkspaceScope::new(
            WorkspaceId::new("ws-symlink"),
            link,
            WorkspaceScopeConfig::default(),
        );
        match previous_home {
            Some(value) => unsafe { std::env::set_var("HOME", value) },
            None => unsafe { std::env::remove_var("HOME") },
        }

        let skill = workspace.skills().get("link-skill").expect("skill discovered");
        assert_eq!(skill.source, SkillSource::User);
    }
```

再加去重纯函数单测（不碰 HOME，直接测函数；测试放同文件但只调 `dedupe_skill_roots`，命名仍按行为——「should keep the later channel when two roots normalize to the same path」是被测行为，函数名出现在 arrange/act 不违反命名规范）：

```rust
    #[test]
    fn should_keep_the_later_channel_when_two_roots_normalize_to_the_same_path() {
        let channels = vec![
            (SkillSource::Project, PathBuf::from("/tmp/./ws/.agents/skills")),
            (SkillSource::User, PathBuf::from("/tmp/ws/.agents/skills")),
        ] /* 转成 SkillRootChannel */;
        let deduped = dedupe_skill_roots(channels);
        assert_eq!(deduped.len(), 1);
        assert_eq!(deduped[0].source, SkillSource::User);
    }

    #[test]
    fn should_keep_nonexistent_roots_verbatim_when_normalization_fails() {
        let channels = vec![
            (SkillSource::Project, PathBuf::from("/definitely/not/here")),
            (SkillSource::User, PathBuf::from("/definitely/not/here/../here")),
        ];
        let deduped = dedupe_skill_roots(channels);
        // canonicalize fails on nonexistent dirs — lexical fallback still
        // dedupes the `..` form
        assert_eq!(deduped.len(), 1);
    }
```

（第二个用例预期：两条路径词法归一后相同 `/definitely/not/here`，去重到 1 条、保留 User。）

- [ ] **Step 2: 跑测试确认失败**

```bash
cd rust && cargo test -p mirri-core --lib scope::workspace_scope
```
Expected: 新用例编译失败（`dedupe_skill_roots` / `SkillRootChannel` 未定义）。

- [ ] **Step 3: 最小实现**

在 `workspace_scope.rs` 顶部 use 区加：

```rust
use crate::session::lexically_resolve;
use crate::skill::SkillSource;
```

（`SkillSource` 若已导入则跳过；`lexically_resolve` 已从 `mirri_core::session` pub use。）

新增（放在 `discover_default_skill_roots` 旁；`SkillRootChannel` 与两个函数用 `pub`——Task 2 的 bootstrap 跨 crate 复用，`pub(crate)` 跨 crate 不可见）：

```rust
/// One default skill-root discovery channel: the directory to scan and
/// the source tier the scan attributes (ADR-011).
pub struct SkillRootChannel {
    pub source: SkillSource,
    pub dir: PathBuf,
}

/// The construction-time default-root channel list (ADR-011): the project
/// `.agents/skills`, the project brand dir, and the user `~/.agents/skills`.
/// The user brand dir is NOT here — the workspace scope does not know the
/// Mirri home; the assembly layer's replay carries it (see
/// `apply_skill_discovery`).
pub fn default_skill_root_channels(cwd: &Path) -> Vec<SkillRootChannel> {
    let mut channels = vec![
        SkillRootChannel { source: SkillSource::Project, dir: cwd.join(".agents/skills") },
        SkillRootChannel { source: SkillSource::Project, dir: cwd.join(".mirri-code/skills") },
    ];
    if let Some(home) = std::env::var_os("HOME") {
        channels.push(SkillRootChannel {
            source: SkillSource::User,
            dir: Path::new(&home).join(".agents/skills"),
        });
    }
    channels
}

/// Normalize one skill-root path for identity comparison (ADR-011):
/// lexical resolution first (never touches the fs), then symlink
/// resolution via `canonicalize` — a failure (nonexistent dir, perm) falls
/// back to the lexical form so the root still participates in discovery.
fn normalize_skill_root(dir: &Path) -> PathBuf {
    let lexical = lexically_resolve(dir);
    std::fs::canonicalize(&lexical).unwrap_or(lexical)
}

/// Dedupe the ordered channel list by normalized path (ADR-011): same
/// physical directory scanned once, by the LATER channel in the sequence
/// (the later-wins override winner — catalog content unchanged, only the
/// duplicate scan and its per-skill shadow warnings disappear). Channels
/// kept in original order minus the earlier duplicates. ⚠️ NOT
/// insert-if-absent: the first channel is NOT the keeper — the accident
/// scenario has Project first and User later, and User must win.
pub fn dedupe_skill_roots(channels: Vec<SkillRootChannel>) -> Vec<SkillRootChannel> {
    // Key -> index of the LAST channel with that key (later wins, ADR-011).
    let mut last_by_key: std::collections::HashMap<PathBuf, usize> =
        std::collections::HashMap::new();
    for (i, channel) in channels.iter().enumerate() {
        last_by_key.insert(normalize_skill_root(&channel.dir), i);
    }
    // Emit in original order, skipping any channel that is not its key's
    // last holder.
    let mut keep = vec![false; channels.len()];
    for &i in last_by_key.values() {
        keep[i] = true;
    }
    channels
        .into_iter()
        .zip(keep)
        .filter(|(_, k)| *k)
        .map(|(c, _)| c)
        .collect()
}
```

`discover_default_skill_roots` 内部改为：

```rust
    pub fn discover_default_skill_roots(&self) {
        let skills = &self.skills;
        for channel in dedupe_skill_roots(default_skill_root_channels(&self.cwd)) {
            if let Err(e) = skills.discover(&channel.dir, channel.source) {
                // keep the existing per-channel warn! with channel-specific label
                tracing::warn!(path = %channel.dir.display(), error = %e, "skill discovery failed");
            }
        }
    }
```

（原实现的三条 `discover` 各自的日志字段略有不同——项目/品牌/用户三条 warn 消息。合并后统一为一条消息即可，保留 `path` 与 `error` 字段；这不是对外契约。）

`SkillRootChannel` 与两个函数在 `mirri-core/src/scope/mod.rs` 按**该文件既有的导出模式**再导出（检查 `scope/mod.rs` 的 `pub use` 惯例，保持一致；`normalize_skill_root` 保持私有——只有 `dedupe_skill_roots` 用它）。

**同步更新陈旧注释（c3）：** `discover_default_skill_roots` 自身的 doc comment（82-93 行区域）描述了「the embedded server's config-write resync hook replays it on every built workspace」——Task 2 后 resync 不再调用它而是持四通道清单，doc 改为指向 ADR-011 的共享定义（构造期首扫用三通道，重放用四通道，同经 `default_skill_root_channels` + `dedupe_skill_roots`）。

- [ ] **Step 4: 跑测试确认通过**

```bash
cd rust && cargo test -p mirri-core --lib scope::workspace_scope
```
Expected: 全部通过（含既有 4 个 skill 发现用例不回归）。

- [ ] **Step 5: Commit**

```bash
git add rust/crates/mirri-core/src/scope/workspace_scope.rs
git commit -m "fix(core): dedupe default skill roots by normalized path (ADR-011)"
```

---

### Task 2: 装配层重放持完整四通道清单统一去重（TDD）

**Files:**
- Modify: `rust/crates/mirri-embedded/src/bootstrap.rs`（`apply_skill_discovery` 的默认根分支）

**Interfaces:**
- Consumes: Task 1 的 `SkillRootChannel`、`default_skill_root_channels`、`dedupe_skill_roots`（Task 1 已定义为 `pub` 并经 `mirri-core/src/scope/mod.rs` 再导出——**`pub(crate)` 跨 crate 不可见，必须 `pub`**）。
- Produces: `apply_skill_discovery` 内部行为变化（签名不变），默认根分支不再调 `workspace.discover_default_skill_roots()`，改为持四通道清单去重后扫描。

**设计要点（实现者必读）：**
- Task 1 交付的三个符号在 `mirri-core` crate 内。跨 crate 复用方案：`SkillRootChannel { pub source: SkillSource, pub dir: PathBuf }` 与两个函数改为 `pub`，在 `scope/workspace_scope.rs` 定义后经 `mirri-core/src/scope/mod.rs` 已有的 `pub use` 链导出（检查 `scope/mod.rs` 的导出模式，保持一致）。**`default_skill_root_channels` 也要 `pub`**，bootstrap 直接复用（构造期三通道与重放前三通道必须同源——「同一发现定义」的落点）。
- `apply_skill_discovery` 的默认根分支（现 457-471 行区域）改为：

```rust
    } else {
        for entry in config.extra_skill_dirs.iter().flatten() {
            let dir = resolve_extra_skill_dir(entry, workspace.cwd());
            if let Err(e) = skills.discover(&dir, SkillSource::Plugin) {
                tracing::warn!(%e, dir = %dir.display(), "extra skill dir discovery failed");
            }
        }
        // ADR-011: the full four-channel default-root list (the three
        // construction-time channels + the user brand dir), deduped by
        // normalized path as ONE list — the replay must see the same
        // channel set construction saw, plus the brand dir it owns.
        let mut channels = default_skill_root_channels(workspace.cwd());
        if let Ok(mirri_home) = resolve_mirri_home(home_dir) {
            channels.push(SkillRootChannel {
                source: SkillSource::User,
                dir: mirri_home.join("skills"),
            });
        }
        for channel in dedupe_skill_roots(channels) {
            if let Err(e) = skills.discover(&channel.dir, channel.source) {
                tracing::warn!(path = %channel.dir.display(), %e, "default skill root discovery failed");
            }
        }
    }
```

- 顺序必须与既有执行序一致：extra dirs（Plugin 源）在前、默认根四通道在后、用户品牌目录是四通道清单的**最后一条**（`mirri_home/skills` 原本就在 `discover_default_skill_roots()` 之后扫，属序列末位，四通道清单保持该位次）。
- `workspace.discover_default_skill_roots()` 在此分支中的旧调用**删除**。

- [ ] **Step 1: 写失败测试**（追加到 `bootstrap.rs` 的 `mod tests`，模式照抄 `should_let_every_source_beat_plugin_skills_when_replaying` 的 home pin 三段式）

```rust
    /// Given a workspace rooted at $HOME itself (so the project brand dir
    /// `cwd/.mirri-code/skills` and the user brand dir `<mirri home>/skills`
    /// collide when mirri home is $HOME/.mirri-code), when the discovery
    /// replay runs, then the skill is discovered once as the user channel
    /// (ADR-011 — the replay dedupes the full four-channel list).
    #[test]
    fn should_discover_brand_dir_once_as_user_when_replay_runs_with_home_rooted_workspace() {
        let home = tempfile::tempdir().expect("home tempdir");
        // mirri home = <home>/.mirri-code; user brand dir = <home>/.mirri-code/skills
        // project brand dir = cwd/.mirri-code/skills — cwd IS home, so same dir
        let skills_dir = home.path().join(".mirri-code/skills/brand-dup");
        std::fs::create_dir_all(&skills_dir).expect("create skill dir");
        std::fs::write(skills_dir.join("SKILL.md"), "# BrandDup\n").expect("write SKILL.md");

        let isolated_home = tempfile::tempdir().expect("isolated $HOME tempdir");
        let workspace = WorkspaceScope::new(
            WorkspaceId::new("ws-brand-collision"),
            home.path().to_path_buf(),
            WorkspaceScopeConfig::default(),
        );

        let _home_lock = test_env::home_lock();
        let previous_home = std::env::var_os("HOME");
        unsafe { std::env::set_var("HOME", isolated_home.path()) };
        apply_skill_discovery(
            &workspace,
            &ServerConfig::default(),
            Some(home.path().join(".mirri-code").as_path()),
            None,
            &[],
        );
        match previous_home {
            Some(value) => unsafe { std::env::set_var("HOME", value) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        drop(_home_lock);

        let skill = workspace
            .skills()
            .get("brand-dup")
            .expect("brand skill discovered");
        assert_eq!(
            skill.source,
            mirri_core::skill::SkillSource::User,
            "the replay must scan the collided brand dir once, as the user channel"
        );
    }
```

注意 `apply_skill_discovery` 是 `pub(crate)`，测试在同 crate 内，可直接调。`home_dir` 参数传 `Some(<home>/.mirri-code)` 使 `resolve_mirri_home` 命中显式注入分支（不依赖 `MIRRICODE_HOME` env）。

**预期这个测试在改造前 FAIL**：现状下 `cwd/.mirri-code/skills`(Project) 先扫、`mirri_home/skills`(User) 后扫，最终 source 是 User——所以 source 断言现状就过。**真正的红信号是告警本身**。改用告警断言：把 `tracing` 事件捕获进 Vec（若 crate 已有捕获基建则复用；没有则用一个简易 `tracing::subscriber` with `make_writer` 收集）。**更简单的替代：断言 `skills.discover` 只被有效执行一次的行为面——catalog 的 `SkillDef.path`。** 现状（重复扫描）时 path 是 User 通道的 `mirri_home/skills/brand-dup`，去重后也是它——path 相同。**行为面完全不变是本修复的本质**，黑盒无法区分。因此此用例的红绿切换必须靠告警捕获或函数级单测。

**裁定**：用告警捕获。**依赖前置**：`mirri-embedded` 的 Cargo.toml `dev-dependencies` 加 `tracing-subscriber = { workspace = true }`（workspace 根 `rust/Cargo.toml:34` 已有该依赖成员）。捕获基建照抄仓库成熟先例 `rust/crates/mirri-acp/src/main.rs:383`（`SharedBuf` + `MakeWriter` impl + `tracing::subscriber::with_default`），不要自己发明 VecWriter：

```rust
// tests mod 内：
use std::sync::{Arc, Mutex};

#[derive(Clone, Default)]
struct SharedLogBuf(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for SharedLogBuf {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
}

impl<'a> tracing_subscriber::MakeWriter<'a> for SharedLogBuf {
    type Writer = SharedLogBuf;
    fn make_writer(&'a self) -> Self::Writer { self.clone() }
}
```

测试体内在 `apply_skill_discovery` 外包 `tracing::subscriber::with_default`（`EnvFilter` 或默认过滤到 WARN 即可——默认 subscriber level 用 `tracing_subscriber::fmt()` 的 `.with_max_level(tracing::Level::WARN)`），跑完后 `assert!(!String::from_utf8_lossy(&buf.0.lock().unwrap()).contains("shadowed"))`。改造前该断言 FAIL（有 shadowed），改造后 PASS。**先写含此断言的完整测试**（红），再实现（绿）。

- [ ] **Step 2: 跑测试确认失败**

```bash
cd rust && cargo test -p mirri-embedded --lib bootstrap
```
Expected: 新用例 FAIL（日志含 "shadowed by a later discovery"）。

- [ ] **Step 3: 实现**

按「设计要点」改 `apply_skill_discovery` 默认根分支。**同步更新陈旧注释**（`apply_skill_discovery` 自身 doc comment 394-414 行描述的序列与 `bootstrap.rs:595-601` 的「`WorkspaceScope::new` already ran … replays it」描述——重放不再经过 `discover_default_skill_roots`，改为描述 ADR-011 的四通道统一去重）。

- [ ] **Step 4: 跑测试确认通过 + 全量回归**

```bash
cd rust && cargo test -p mirri-embedded --lib
```
Expected: 全部通过。

- [ ] **Step 5: Commit**

```bash
git add rust/crates/mirri-embedded/src/bootstrap.rs rust/crates/mirri-embedded/Cargo.toml rust/crates/mirri-core/src/scope/workspace_scope.rs
git commit -m "fix(embedded): dedupe four-channel default skill roots in replay (ADR-011)"
```

---

### Task 3: 端到端场景验证 + context 留档

**Files:**
- Modify: `docs/specs/skills-context.md`（TS dotfiles 隐患一行留档，评审 c3）

**Interfaces:** 无代码接口——本任务是验证与文档。

- [ ] **Step 1: worktree 内起真实 server 复现验证（封闭场景，不依赖开发者真实 $HOME）**

```bash
cd rust && cargo build -p mirri-cli
# 封闭场景：临时 HOME（seed 一个 .agents/skills/x）、workspace 指向它、
# MIRRICODE_HOME 指向另一临时目录（其下 skills/ 也放一个同名技能，构造品牌目录碰撞）
SEED_HOME=$(mktemp -d)
MIRRI_HOME_DIR=$(mktemp -d)
mkdir -p "$SEED_HOME/.agents/skills/e2e-dup" "$MIRRI_HOME_DIR/skills/e2e-dup"
echo '# E2E' > "$SEED_HOME/.agents/skills/e2e-dup/SKILL.md"
echo '# E2E brand' > "$MIRRI_HOME_DIR/skills/e2e-dup/SKILL.md"
# 修复前先跑一次（在 git stash 或改造前的 baseline 上）确认告警数 > 0：
HOME="$SEED_HOME" MIRRICODE_HOME="$MIRRI_HOME_DIR" \
  timeout 5 ./target/debug/mirri serve --workspace "$SEED_HOME" --port 0 2>&1 \
  | grep -c 'shadowed by a later discovery'   # 期望（修复前）: > 0
# 修复后同样跑一次：
HOME="$SEED_HOME" MIRRICODE_HOME="$MIRRI_HOME_DIR" \
  timeout 5 ./target/debug/mirri serve --workspace "$SEED_HOME" --port 0 2>&1 \
  | grep -c 'shadowed by a later discovery'   # 期望（修复后）: 0
```

（serve 子命令与参数以 `./target/debug/mirri --help` 为准——`--port 0` 若不支持则选一个空闲固定口；server 短暂运行 5 秒足够触发启动期扫描，`timeout` 后管道仍能收到已输出日志。）

注意：server 可能以 $HOME/.mirri-code 为 MIRRICODE_HOME 默认值——必须显式设 `MIRRICODE_HOME` 才能控制品牌目录位置；`--workspace "$SEED_HOME"` 使 cwd = HOME，构造 `.agents` 与 `.mirri-code/skills` 两组碰撞。

- [ ] **Step 2: 全量 rust 回归（后台任务，≥20 分钟超时）**

```bash
cd rust && cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings
```
Expected: 全绿（AGENTS.md：全量测试约 7 分钟，须后台跑）。

- [ ] **Step 3: context 留档**

在 `docs/specs/skills-context.md` 的 §11（rust 侧快照）末尾追加一行：

```markdown
- **TS dotfiles 同构隐患（ADR-011 评审留档）**：TS 项目级发现走 `.git` 上溯，用户把主目录做成 git 仓库（dotfiles 管理惯例）时，项目通道与用户通道同样解析到同一目录；TS 冻结不修，rust 侧已按 ADR-011 去重（2026-10-23 留档）。
```

- [ ] **Step 4: Commit**

```bash
git add docs/specs/skills-context.md
git commit -m "docs(skills): record TS dotfiles aliasing residual (ADR-011)"
```

---

## 任务依赖

Task 1 → Task 2（串行，Task 2 消费 Task 1 的符号）→ Task 3（串行，验证依赖两者完成）。无并行机会。

## Self-Review 结论（评审修订后）

- **Spec 覆盖**：ADR-011 决策 1（四通道清单统一去重、构造期与重放共用定义、归一化失败回退、保留后到通道）→ Task 1+2；决策 2（告警保留语义本义，不降级）→ Task 2 的告警断言反向保证（碰撞消失、真实跨源遮蔽用例已有：`should_let_every_source_beat_plugin_skills_when_replaying` 的 plugin 同名覆盖仍在）；影响范围（extra/plugin 不参与）→ Task 2 只动默认根分支；TS dotfiles 留档 → Task 3。✓
- **评审意见落点**：M1 测试并发锁（Task 1 设计要点 + 测试代码）✓；m1 tracing-subscriber dev-dep + SharedLogBuf 照抄先例（Task 2）✓；m2 导出改 `pub`（Task 1 Interfaces）✓；m3 教学式双版本删除、只留后到者胜实现（Task 1 Step 3）✓；m4 封闭 e2e 场景（Task 3 Step 1）✓；c1 归一化注入简化已在设计要点声明 ✓；c2 零变化回归护栏注记（Task 1 测试 NOTE）✓；c3 陈旧注释同步（Task 1/2 各有检查项）✓；c4 `#[test]` 标注 ✓。
- **占位符扫描**：无 TBD / "similar to"。✓
- **类型一致性**：`SkillRootChannel { pub source: SkillSource, pub dir: PathBuf }`、`default_skill_root_channels(cwd: &Path) -> Vec<SkillRootChannel>`（pub）、`dedupe_skill_roots(Vec<SkillRootChannel>) -> Vec<SkillRootChannel>`（pub）、`normalize_skill_root(&Path) -> PathBuf`（私有）——签名在 Task 1/2 一致。✓
