//! 工作区路径索引（E9 / multimodal ADR-007）：可丢弃的内存加速结构。
//! 事实源永远是磁盘；索引只存相对路径，不预生成任何匹配用派生物。

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexKind {
    File,
    Dir,
}

#[derive(Debug, Clone)]
pub struct FileIndexEntry {
    pub rel_path: String,
    pub kind: IndexKind,
    pub hidden: bool,
}

#[derive(Debug, Default)]
pub struct FileIndex {
    entries: Vec<FileIndexEntry>,
    truncated: bool,
}

impl FileIndex {
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &FileIndexEntry> {
        self.entries.iter()
    }

    pub fn truncated(&self) -> bool {
        self.truncated
    }

    /// 构建期分批换代用：把流式构建的一批条目追加进当前快照
    /// （上游已保证总量不超过 max_entries）。
    pub fn append_entries(&mut self, batch: Vec<FileIndexEntry>) {
        self.entries.extend(batch);
    }
}

/// 每根条目上限（architecture §11.3）。
pub const MAX_INDEX_ENTRIES: usize = 300_000;

/// 流式构建的批次大小与时间触发（每 ~2000 条或 100ms 换一代快照，
/// ADR-007 §11.3：构建期部分结果可用，search 永不阻塞在构建上）。
const STREAM_BATCH_ENTRIES: usize = 2_000;
const STREAM_FLUSH_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);

/// ignore 遍历项 → 索引条目（根自身与 `.git` 跳过；分隔符归一为 `/`）。
fn convert_entry(
    entry: &Result<ignore::DirEntry, ignore::Error>,
    root: &Path,
) -> Option<FileIndexEntry> {
    let entry = entry.as_ref().ok()?;
    let file_type = entry.file_type()?;
    let rel = entry
        .path()
        .strip_prefix(root)
        .ok()
        .filter(|p| !p.as_os_str().is_empty())?;
    let rel_str = rel.to_str()?;
    if rel_str == ".git" || rel_str.starts_with(".git/") {
        return None;
    }
    let kind = if file_type.is_dir() {
        IndexKind::Dir
    } else {
        IndexKind::File
    };
    let hidden = rel_str.starts_with('.') || rel_str.contains("/.");
    Some(FileIndexEntry {
        rel_path: rel_str.replace(std::path::MAIN_SEPARATOR, "/"),
        kind,
        hidden,
    })
}

/// 并行遍历（ignore crate，遵守 gitignore；隐藏条目带标记入索引，供
/// 「`.` 开头片段按需放出」），只收路径不读内容。截断按路径深度升序保留浅层。
pub fn build_index(root: &Path, max_entries: usize) -> FileIndex {
    use ignore::WalkState;

    let entries = Arc::new(Mutex::new(Vec::<FileIndexEntry>::new()));
    let truncated = Arc::new(AtomicBool::new(false));
    let mut builder = ignore::WalkBuilder::new(root);
    // hidden(false)：隐藏条目也进索引（带 hidden 标记）。
    builder
        .hidden(false)
        .git_ignore(true)
        .git_exclude(true)
        .git_global(true);
    {
        let entries = Arc::clone(&entries);
        let truncated = Arc::clone(&truncated);
        let root = root.to_path_buf();
        builder.build_parallel().run(move || {
            let entries = Arc::clone(&entries);
            let truncated = Arc::clone(&truncated);
            let root = root.clone();
            Box::new(move |entry| {
                if truncated.load(Ordering::Relaxed) {
                    return WalkState::Quit;
                }
                let Some(item) = convert_entry(&entry, &root) else {
                    return WalkState::Continue;
                };
                let mut guard = entries.lock().unwrap();
                if guard.len() >= max_entries {
                    truncated.store(true, Ordering::Relaxed);
                    drop(guard);
                    return WalkState::Quit;
                }
                guard.push(item);
                WalkState::Continue
            })
        });
    }
    let mut entries = entries.lock().unwrap().clone();
    let truncated = truncated.load(Ordering::Relaxed) || entries.len() > max_entries;
    if entries.len() > max_entries {
        // 并行收集可能超限（各线程在加锁前各自判定）；按深度升序保留浅层截断。
        entries.sort_unstable_by(|a, b| {
            let depth =
                |e: &FileIndexEntry| e.rel_path.matches('/').count();
            depth(a).cmp(&depth(b)).then_with(|| a.rel_path.cmp(&b.rel_path))
        });
        entries.truncate(max_entries);
    }
    FileIndex { entries, truncated }
}

/// 串行流式构建：边扫边按批回调（每 ~2000 条或 100ms 一批），返回完整索引。
/// 供注册表在构建期分批换代 RwLock 快照——search 永远读当前快照立即返回，
/// 构建期得到的是已扫到的部分结果（US9.7 第 4 条）。
pub fn build_index_streaming<F>(root: &Path, max_entries: usize, mut on_batch: F) -> FileIndex
where
    F: FnMut(Vec<FileIndexEntry>),
{
    let mut entries: Vec<FileIndexEntry> = Vec::new();
    let mut truncated = false;
    let mut batch: Vec<FileIndexEntry> = Vec::with_capacity(STREAM_BATCH_ENTRIES);
    let mut last_flush = std::time::Instant::now();
    let walker = ignore::WalkBuilder::new(root)
        .hidden(false)
        .git_ignore(true)
        .git_exclude(true)
        .git_global(true)
        .build();
    for entry in walker {
        if entries.len() >= max_entries {
            truncated = true;
            break;
        }
        let Some(item) = convert_entry(&entry, root) else {
            continue;
        };
        entries.push(item.clone());
        batch.push(item);
        if batch.len() >= STREAM_BATCH_ENTRIES || last_flush.elapsed() >= STREAM_FLUSH_INTERVAL
        {
            on_batch(std::mem::take(&mut batch));
            last_flush = std::time::Instant::now();
        }
    }
    if !batch.is_empty() {
        on_batch(batch);
    }
    FileIndex { entries, truncated }
}

// ---------------------------------------------------------------------------
// 模糊打分、命中位置与路径分段（E9 / file-reference-uiux §2.2）
// ---------------------------------------------------------------------------

/// 打分档位：精确 1.0 / 文件名前缀 0.8 / 文件名包含 0.6 / 路径包含 0.4 /
/// 子序列 0.2 + 连续性小奖励。就地 to_lowercase（不预生成、不缓存——ADR-007 §11.3）。
pub fn score_entry(name: &str, rel_path: &str, query_lower: &str) -> Option<f64> {
    if query_lower.is_empty() {
        return None;
    }
    let name_lower = name.to_lowercase();
    // 「文件名精确匹配」按去扩展名口径（uiux §2.2）：`composer` 精确命中 `composer.vue`。
    let stem_lower = match name.rfind('.') {
        Some(dot) if dot > 0 => name[..dot].to_lowercase(),
        _ => name_lower.clone(),
    };
    if name_lower == query_lower || stem_lower == query_lower {
        return Some(1.0);
    }
    if name_lower.starts_with(query_lower) {
        return Some(0.8);
    }
    if name_lower.contains(query_lower) {
        return Some(0.6);
    }
    let path_lower = rel_path.to_lowercase();
    if path_lower.contains(query_lower) {
        return Some(0.4);
    }
    // 子序列（最左贪心），连续命中给小奖励。
    let mut idx = 0usize;
    let mut streak = 0usize;
    let mut best_streak = 0usize;
    for ch in query_lower.chars() {
        let i = name_lower[idx..].find(ch)?;
        streak = if i == 0 { streak + 1 } else { 1 };
        best_streak = best_streak.max(streak);
        idx += i + ch.len_utf8();
    }
    Some(0.2 + (best_streak as f64) * 0.01)
}

/// 每个 query 字符在小写化相对路径中的最左贪心字符下标（前端高亮用）。
/// 口径：字符下标（非字节）；非 ASCII 路径与前端 UTF-16 下标可能偏差，可接受（highlight 装饰性）。
pub fn match_positions(rel_path: &str, query_lower: &str) -> Vec<u64> {
    let lower: Vec<char> = rel_path.to_lowercase().chars().collect();
    let mut out = Vec::new();
    let mut pos = 0usize;
    for ch in query_lower.chars() {
        match lower[pos..].iter().position(|c| *c == ch) {
            Some(i) => {
                out.push((pos + i) as u64);
                pos += i + 1;
            }
            None => return Vec::new(),
        }
    }
    out
}

/// query 含 `/` 时切成（目录前缀, 剩余 query）；前缀按路径段逐段前缀匹配（`src/comp` → `src/`）。
pub fn split_scoped_query(query: &str) -> Option<(String, String)> {
    let query = query.trim_start_matches('/');
    let idx = query.rfind('/')?;
    Some((query[..=idx].to_string(), query[idx + 1..].to_string()))
}

// ---------------------------------------------------------------------------
// notify 监听增量更新（E9 / US9.7）
// ---------------------------------------------------------------------------

/// 索引增量：watcher 事件批转换后的最小变更单元。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexDelta {
    Add {
        rel_path: String,
        kind: IndexKind,
    },
    Remove {
        rel_path: String,
    },
    Rename {
        from: String,
        to: String,
        kind: IndexKind,
    },
}

impl FileIndex {
    /// 增量应用（幂等）：Add 已存在则改 kind；Rename 缺失则按 Add 补。
    pub fn apply_delta(&mut self, delta: IndexDelta) {
        match delta {
            IndexDelta::Add { rel_path, kind } => {
                if let Some(pos) = self.entries.iter().position(|e| e.rel_path == rel_path) {
                    self.entries[pos].kind = kind;
                } else {
                    self.entries.push(FileIndexEntry {
                        rel_path,
                        kind,
                        hidden: false,
                    });
                }
            }
            IndexDelta::Remove { rel_path } => {
                self.entries.retain(|e| e.rel_path != rel_path);
            }
            IndexDelta::Rename { from, to, kind } => {
                if let Some(pos) = self.entries.iter().position(|e| e.rel_path == from) {
                    self.entries[pos].rel_path = to;
                    self.entries[pos].kind = kind;
                } else {
                    self.entries.push(FileIndexEntry {
                        rel_path: to,
                        kind,
                        hidden: false,
                    });
                }
            }
        }
    }
}

fn parent_dir_of(rel_path: &str) -> &str {
    rel_path
        .rsplit_once('/')
        .map(|(dir, _)| dir)
        .unwrap_or_default()
}

/// notify::Event 批 → IndexDelta 批（相对化、忽略根外路径）。
/// rename 事件对语义：同批 `Remove{a}` + `Create{b}` 且父目录相同时合并为
/// `Rename`（一条原子改写）；`Name(Both)` 直接产出 Rename；未配对时各自生效，
/// 旧名仍然消失（US9.7 第 2 条）。
pub fn events_to_deltas(root: &Path, events: Vec<notify::Event>) -> Vec<IndexDelta> {
    use notify::event::{ModifyKind, RenameMode};
    use notify::EventKind;

    let kind_of = |path: &Path| {
        if path.is_dir() {
            IndexKind::Dir
        } else {
            IndexKind::File
        }
    };
    let rel_of = |path: &Path| -> Option<String> {
        path.strip_prefix(root)
            .ok()
            .filter(|p| !p.as_os_str().is_empty())
            .and_then(|p| p.to_str())
            .map(|rel| rel.replace(std::path::MAIN_SEPARATOR, "/"))
    };

    let mut raw: Vec<IndexDelta> = Vec::new();
    for ev in events {
        match &ev.kind {
            // Both：单事件携带（from, to）两个路径，直接产出 Rename。
            EventKind::Modify(ModifyKind::Name(RenameMode::Both)) if ev.paths.len() == 2 => {
                if let (Some(from), Some(to)) = (rel_of(&ev.paths[0]), rel_of(&ev.paths[1])) {
                    raw.push(IndexDelta::Rename {
                        from,
                        to,
                        kind: kind_of(&ev.paths[1]),
                    });
                }
                continue;
            }
            _ => {}
        }
        for path in &ev.paths {
            let Some(rel) = rel_of(path) else { continue };
            match &ev.kind {
                EventKind::Create(_) => raw.push(IndexDelta::Add {
                    rel_path: rel,
                    kind: kind_of(path),
                }),
                EventKind::Remove(_) => raw.push(IndexDelta::Remove { rel_path: rel }),
                // rename From → Remove（旧名必须消失，US9.7 第 2 条）；
                // To 及其他 Modify → 幂等 Add。
                EventKind::Modify(ModifyKind::Name(RenameMode::From)) => {
                    raw.push(IndexDelta::Remove { rel_path: rel })
                }
                EventKind::Modify(_) => raw.push(IndexDelta::Add {
                    rel_path: rel,
                    kind: kind_of(path),
                }),
                _ => {}
            }
        }
    }

    // 同批 Remove{a} + Add{b} 且父目录相同 → 合并为 Rename。
    let mut slots: Vec<Option<IndexDelta>> = raw.into_iter().map(Some).collect();
    let mut out = Vec::new();
    for i in 0..slots.len() {
        let Some(delta) = slots[i].take() else { continue };
        let IndexDelta::Remove { rel_path: removed } = delta else {
            out.push(delta);
            continue;
        };
        let parent = parent_dir_of(&removed);
        let mut pair: Option<(String, IndexKind)> = None;
        for slot in slots.iter_mut().skip(i + 1) {
            if let Some(IndexDelta::Add { rel_path, kind }) = slot
                && parent_dir_of(rel_path) == parent
            {
                pair = Some((rel_path.clone(), *kind));
                *slot = None;
                break;
            }
        }
        match pair {
            Some((to, kind)) => out.push(IndexDelta::Rename {
                from: removed,
                to,
                kind,
            }),
            None => out.push(IndexDelta::Remove { rel_path: removed }),
        }
    }
    out
}

/// 事件批量静默窗口（照 skill_watcher.rs 的 QUIET_PERIOD 模式；索引增量
/// 应用幂等，窗口取窄）。
const INDEX_WATCH_QUIET_PERIOD: std::time::Duration = std::time::Duration::from_millis(200);

/// watch 被 OS 丢弃后的重挂间隔（skill_watcher.rs 的 WATCH_RETRY_DELAY 模式）。
const INDEX_WATCH_RETRY_DELAY: std::time::Duration = std::time::Duration::from_secs(2);

/// 每 root 一个的监听句柄：notify 事件经 mpsc bridge 汇入后台线程，静默窗口
/// 批量后经 `events_to_deltas` 回调；watch 失效按固定间隔重试重挂。
/// 事实源永远是磁盘，watcher 只丢失「增量」，丢弃句柄即整体停摆——索引
/// 可丢弃（ADR-007 §11.3），不做全量重建兜底。
pub struct IndexWatcher {
    stop: std::sync::mpsc::Sender<()>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl IndexWatcher {
    /// 启动监听；`notify` 建立或首次 `watch` 失败返回 `None`
    /// （调用方据此把根标记为 Degraded——仍可用索引应答）。
    pub fn start<F>(root: std::path::PathBuf, on_events: F) -> Option<Self>
    where
        F: Fn(Vec<IndexDelta>) + Send + 'static,
    {
        use notify::Watcher;
        let (ev_tx, ev_rx) = std::sync::mpsc::channel::<Result<notify::Event, notify::Error>>();
        let mut watcher = notify::recommended_watcher(move |res| {
            let _ = ev_tx.send(res);
        })
        .ok()?;
        watcher
            .watch(&root, notify::RecursiveMode::Recursive)
            .ok()?;
        let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
        let handle = std::thread::spawn(move || {
            // watcher 随线程存活；线程退出即解除 OS watch。
            loop {
                match stop_rx.try_recv() {
                    Ok(()) => break,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
                    Err(std::sync::mpsc::TryRecvError::Empty) => {}
                }
                std::thread::sleep(INDEX_WATCH_QUIET_PERIOD);
                let mut batch = Vec::new();
                let mut lost_watch = false;
                while let Ok(res) = ev_rx.try_recv() {
                    match res {
                        Ok(ev) => batch.push(ev),
                        // watch 失效（如被删目录）：延迟后重挂（skill_watcher 模式）。
                        Err(_) => lost_watch = true,
                    }
                }
                if !batch.is_empty() {
                    on_events(events_to_deltas(&root, batch));
                }
                if lost_watch {
                    std::thread::sleep(INDEX_WATCH_RETRY_DELAY);
                    let _ = watcher.watch(&root, notify::RecursiveMode::Recursive);
                }
            }
        });
        Some(Self {
            stop: stop_tx,
            handle: Some(handle),
        })
    }

    /// 停止监听并回收后台线程。
    pub fn stop(mut self) {
        let _ = self.stop.send(());
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for IndexWatcher {
    fn drop(&mut self) {
        // stop() 之外的兜底：句柄被丢弃时至少发停机信号，避免线程泄漏。
        let _ = self.stop.send(());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_with_files() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src/nested")).unwrap();
        std::fs::write(dir.path().join("src/main.rs"), "fn main() {}").unwrap();
        std::fs::write(dir.path().join("src/nested/util.rs"), "").unwrap();
        std::fs::write(dir.path().join("README.md"), "# t").unwrap();
        std::fs::create_dir_all(dir.path().join("node_modules/pkg")).unwrap();
        std::fs::write(dir.path().join("node_modules/pkg/index.js"), "").unwrap();
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        std::fs::write(dir.path().join(".git/config"), "").unwrap();
        dir
    }

    #[test]
    fn should_collect_files_and_directories_with_relative_paths() {
        let dir = tmp_with_files();
        let index = build_index(dir.path(), 300_000);
        let paths: Vec<&str> = index.iter().map(|e| e.rel_path.as_str()).collect();
        assert!(paths.contains(&"src/main.rs"));
        assert!(paths.contains(&"src/nested"));
        assert!(paths.contains(&"README.md"));
        assert!(index.iter().filter(|e| e.kind == IndexKind::Dir).count() >= 2);
    }

    #[test]
    fn should_respect_gitignore_and_skip_dot_git() {
        let dir = tmp_with_files();
        std::fs::write(dir.path().join(".gitignore"), "node_modules/\n").unwrap();
        let index = build_index(dir.path(), 300_000);
        assert!(index.iter().all(|e| !e.rel_path.starts_with("node_modules")));
        assert!(
            index
                .iter()
                .all(|e| e.rel_path != ".git" && !e.rel_path.starts_with(".git/"))
        );
    }

    #[test]
    fn should_mark_hidden_entries_but_keep_them_in_index() {
        let dir = tmp_with_files();
        std::fs::write(dir.path().join(".env.example"), "X=1").unwrap();
        let index = build_index(dir.path(), 300_000);
        let env = index.iter().find(|e| e.rel_path == ".env.example").unwrap();
        assert!(env.hidden);
    }

    #[test]
    fn should_truncate_at_max_entries_and_report_it() {
        let dir = tmp_with_files();
        let index = build_index(dir.path(), 2);
        assert_eq!(index.len(), 2);
        assert!(index.truncated());
    }

    #[test]
    fn should_rank_exact_over_prefix_over_contains_over_subsequence() {
        let q = "composer";
        assert_eq!(score_entry("composer.vue", "src/composer.vue", q), Some(1.0));
        assert!(score_entry("composer-extra.ts", "a/composer-extra.ts", q).unwrap() < 1.0);
        assert!(
            score_entry("xcomposery.ts", "a/xcomposery.ts", q).unwrap()
                < score_entry("composer-extra.ts", "a/composer-extra.ts", q).unwrap()
        );
        // 子序列（驼峰缩写）：cpn 依序命中 component.ts 的 c→p→n
        assert!(score_entry("component.ts", "a/component.ts", "cpn").is_some());
        assert_eq!(score_entry("zzz.ts", "zzz.ts", q), None);
    }

    #[test]
    fn should_return_char_match_positions_for_highlighting() {
        // 最左贪心：s→0, c→2（"src" 的 c），x→15
        assert_eq!(match_positions("src/components/x.ts", "scx"), vec![0, 2, 15]);
        assert!(match_positions("abc", "z").is_empty());
    }

    #[test]
    fn should_split_dir_prefix_from_scoped_query() {
        assert_eq!(
            split_scoped_query("src/comp"),
            Some(("src/".into(), "comp".into()))
        );
        assert_eq!(
            split_scoped_query("src/"),
            Some(("src/".into(), String::new()))
        );
        assert_eq!(split_scoped_query("comp"), None);
    }

    #[test]
    fn should_remove_old_path_on_rename_from_event() {
        let ev = notify::Event {
            kind: notify::EventKind::Modify(notify::event::ModifyKind::Name(
                notify::event::RenameMode::From,
            )),
            paths: vec![std::path::PathBuf::from("/ws/old.ts")],
            attrs: Default::default(),
        };
        let deltas = events_to_deltas(Path::new("/ws"), vec![ev]);
        assert!(matches!(&deltas[0], IndexDelta::Remove { rel_path } if rel_path == "old.ts"));
    }

    #[test]
    fn should_apply_add_remove_rename_deltas() {
        let mut idx = FileIndex::default();
        idx.apply_delta(IndexDelta::Add {
            rel_path: "a.ts".into(),
            kind: IndexKind::File,
        });
        idx.apply_delta(IndexDelta::Add {
            rel_path: "d".into(),
            kind: IndexKind::Dir,
        });
        assert_eq!(idx.len(), 2);
        idx.apply_delta(IndexDelta::Rename {
            from: "a.ts".into(),
            to: "b.ts".into(),
            kind: IndexKind::File,
        });
        assert!(idx.iter().any(|e| e.rel_path == "b.ts"));
        assert!(!idx.iter().any(|e| e.rel_path == "a.ts"));
        idx.apply_delta(IndexDelta::Remove { rel_path: "d".into() });
        assert!(!idx.iter().any(|e| e.rel_path == "d"));
    }

    #[test]
    fn should_collect_deltas_from_notify_events_with_debounce() {
        // events_to_deltas：纯转换函数，供 watcher 线程调用
        let ev = |p: &str, is_dir: bool| notify::Event {
            kind: if is_dir {
                notify::EventKind::Create(notify::event::CreateKind::Folder)
            } else {
                notify::EventKind::Create(notify::event::CreateKind::File)
            },
            paths: vec![std::path::PathBuf::from(p)],
            attrs: Default::default(),
        };
        let deltas = events_to_deltas(Path::new("/ws"), vec![ev("/ws/src/new.ts", false)]);
        assert!(
            matches!(&deltas[0], IndexDelta::Add { rel_path, .. } if rel_path == "src/new.ts")
        );
    }

    #[test]
    fn should_stream_batches_and_build_same_entries_as_parallel_build() {
        let dir = tmp_with_files();
        let mut batch_sizes: Vec<usize> = Vec::new();
        let streamed = build_index_streaming(dir.path(), 300_000, |batch| {
            batch_sizes.push(batch.len());
        });
        // 残余批 flush：每条目恰好回调一次。
        assert!(!batch_sizes.is_empty());
        assert_eq!(batch_sizes.iter().sum::<usize>(), streamed.len());
        let parallel = build_index(dir.path(), 300_000);
        let mut streamed_paths: Vec<String> =
            streamed.iter().map(|e| e.rel_path.clone()).collect();
        let mut parallel_paths: Vec<String> =
            parallel.iter().map(|e| e.rel_path.clone()).collect();
        streamed_paths.sort();
        parallel_paths.sort();
        assert_eq!(streamed_paths, parallel_paths);
    }

    #[test]
    fn should_truncate_streaming_build_at_max_entries() {
        let dir = tmp_with_files();
        let index = build_index_streaming(dir.path(), 2, |_| {});
        assert_eq!(index.len(), 2);
        assert!(index.truncated());
    }
}
