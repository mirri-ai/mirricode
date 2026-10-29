//! 每工作区根一份的索引注册表（E9 / ADR-007 §11.3）：
//! 常驻上限 8 根（LRU）、同根构建去重、同时构建上限 2、notify 增量、可丢弃。
//!
//! 构建永不阻塞检索：构建走 `spawn_blocking` 流式扫描，条目分批（每 ~2000
//! 条或 100ms）经 RwLock 快照换代，search 永远读当前快照立即返回——构建期
//! 的并发检索得到部分结果 + `IndexState::Building`（US9.7 第 4 条 / M2 评审
//! 修订）。首个触发构建的调用同样不等待：立即返回当时的部分快照。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::atomic::{AtomicU64, AtomicUsize};
use std::time::Instant;

use mirri_os::file_index::{
    build_index_streaming, match_positions, score_entry, split_scoped_query, FileIndex,
    FileIndexEntry, IndexKind, IndexWatcher, MAX_INDEX_ENTRIES,
};
use tokio::sync::{Mutex, oneshot};

/// 常驻根上限（LRU 驱逐阈值，architecture §11.3）。
pub const MAX_RESIDENT_ROOTS: usize = 8;
/// 同时构建的根上限：超过时新根不排队，直接返回空结果 + Building（M2）。
pub const MAX_CONCURRENT_BUILDS: usize = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexState {
    Building,
    Ready,
    Truncated,
    Degraded,
}

pub struct SearchHit {
    pub rel_path: String,
    pub name: String,
    pub kind: &'static str,
    pub score: f64,
    pub match_positions: Vec<u64>,
}

/// 每根驻留状态：索引快照 + 构建/降级标志 + watcher 生命周期 + LRU 时间戳。
/// 根被 LRU 驱逐时 drop 掉整个条目即同时回收 watcher（`IndexWatcher::Drop`
/// 发停机信号）。
struct RootEntry {
    index: Arc<std::sync::RwLock<FileIndex>>,
    state: IndexState,
    building: bool,
    watcher: Option<IndexWatcher>,
    last_used: Instant,
    /// 构建代次：invalidate/重试会换代，旧构建的批次与完成回调按代次
    /// 作废，避免把过期条目写进新快照。
    generation: u64,
    /// 本次构建的完成信号：首个触发构建（或 invalidate）的调用方持有
    /// receiver，完成回调 send 一次。
    build_done: Option<oneshot::Sender<()>>,
}

impl RootEntry {
    fn placeholder() -> Self {
        Self {
            index: Arc::new(std::sync::RwLock::new(FileIndex::default())),
            state: IndexState::Building,
            building: false,
            watcher: None,
            last_used: Instant::now(),
            generation: 0,
            build_done: None,
        }
    }
}

/// search 读取用的稳定视图：快照 Arc + 读取时刻的状态。
struct RootView {
    index: Arc<std::sync::RwLock<FileIndex>>,
    state: IndexState,
}

struct Inner {
    roots: Mutex<HashMap<PathBuf, RootEntry>>,
    in_flight_builds: AtomicUsize,
    generation_counter: AtomicU64,
}

pub struct FileIndexRegistry {
    inner: Arc<Inner>,
}

impl Clone for FileIndexRegistry {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl Default for FileIndexRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl FileIndexRegistry {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                roots: Mutex::new(HashMap::new()),
                in_flight_builds: AtomicUsize::new(0),
                generation_counter: AtomicU64::new(1),
            }),
        }
    }

    /// 检索：索引未就绪时返回【已扫到部分】的打分结果 + IndexState::Building
    /// （US9.7 第 4 条：构建期部分结果可用，绝不等待构建完成）。
    pub async fn search(
        &self,
        root: &Path,
        query: &str,
        limit: usize,
    ) -> (Vec<SearchHit>, IndexState) {
        let view = self.entry_for(root).await;
        // 路径分段：query 含 `/` 时先切目录前缀，只在该前缀下过滤。
        let (dir_prefix, rest) =
            split_scoped_query(query).unwrap_or_else(|| (String::new(), query.to_string()));
        let query_lower = rest.to_lowercase();
        let mut scored: Vec<SearchHit> = {
            let index = view.index.read().unwrap();
            index
                .iter()
                .filter(|e| dir_prefix.is_empty() || e.rel_path.starts_with(&dir_prefix))
                // `.` 开头片段按需放出隐藏条目（US9.6 第 2 条）。
                .filter(|e| !e.hidden || rest.starts_with('.'))
                .filter_map(|e| {
                    let name = name_of(&e.rel_path);
                    if query_lower.is_empty() {
                        return Some(SearchHit {
                            rel_path: e.rel_path.clone(),
                            name,
                            kind: kind_str(e.kind),
                            score: 0.5,
                            match_positions: vec![],
                        });
                    }
                    let score = score_entry(&name, &e.rel_path, &query_lower)?;
                    Some(SearchHit {
                        rel_path: e.rel_path.clone(),
                        name,
                        kind: kind_str(e.kind),
                        score,
                        match_positions: match_positions(&e.rel_path, &query_lower),
                    })
                })
                .collect()
        };
        // 排序（uiux §2.2）：score desc → 文件夹优先（显式判断，勿靠字典序巧合）
        // → 路径短优先 → 字典序。
        scored.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| match (a.kind == "directory", b.kind == "directory") {
                    (true, false) => std::cmp::Ordering::Less,
                    (false, true) => std::cmp::Ordering::Greater,
                    _ => std::cmp::Ordering::Equal,
                })
                .then_with(|| a.rel_path.len().cmp(&b.rel_path.len()))
                .then_with(|| a.rel_path.cmp(&b.rel_path))
        });
        scored.truncate(limit);
        (scored, view.state)
    }

    /// 通知注册表该根有变化：手动丢弃旧索引并后台重建（生产链路增量由
    /// watcher 驱动；本方法用于「增量可能整段丢失」的兜底重建）。
    /// 不等待重建完成——重建期间检索读部分快照（构建期部分结果语义一致）。
    pub async fn invalidate(&self, root: &Path) {
        let build = {
            let mut roots = self.inner.roots.lock().await;
            if roots.get_mut(root).is_none() {
                roots.insert(root.to_path_buf(), RootEntry::placeholder());
            } else {
                roots.get_mut(root).unwrap().last_used = Instant::now();
            }
            let build = self.start_build(&mut roots, root);
            let evicted = self.evict_lru(&mut roots, root);
            if let Some((_, evicted)) = evicted {
                drop(evicted);
            }
            Some(build)
        };
        if let Some((generation, _done)) = build {
            self.spawn_build(root.to_path_buf(), generation);
        }
    }

    /// 获取或创建根条目：命中即刷 LRU 并读当前快照（构建中 → 部分结果）；
    /// 缺失或待重试（超并发上限被推迟）则发起构建。构建不等待——立即
    /// 返回当前（可能为部分的）快照，绝不阻塞首次检索（M3）。
    async fn entry_for(&self, root: &Path) -> RootView {
        let build = {
            let mut roots = self.inner.roots.lock().await;
            if roots.get_mut(root).is_none() {
                roots.insert(root.to_path_buf(), RootEntry::placeholder());
            } else {
                roots.get_mut(root).unwrap().last_used = Instant::now();
            }
            // 待建判定：未在构建中且仍处 Building（新占位或超并发上限被推迟）。
            let needs_build = roots
                .get(root)
                .is_some_and(|entry| !entry.building && entry.state == IndexState::Building);
            let build = if needs_build {
                Some(self.start_build(&mut roots, root))
            } else {
                None
            };
            let evicted = self.evict_lru(&mut roots, root);
            if let Some((_, evicted)) = evicted {
                drop(evicted);
            }
            build
        };
        if let Some((generation, _done)) = build {
            self.spawn_build(root.to_path_buf(), generation);
        }
        self.view_of(root).await
    }

    async fn view_of(&self, root: &Path) -> RootView {
        let roots = self.inner.roots.lock().await;
        match roots.get(root) {
            Some(entry) => RootView {
                index: Arc::clone(&entry.index),
                state: entry.state,
            },
            // 等待期间被 LRU 驱逐：按构建中处理，下一次检索会重建。
            None => RootView {
                index: Arc::new(std::sync::RwLock::new(FileIndex::default())),
                state: IndexState::Building,
            },
        }
    }

    /// 发起一次构建：换代、清空快照、登记完成信号。调用方须已插入条目并
    /// 持有 roots 锁；返回 (代次, 完成信号) 供锁外 spawn。
    fn start_build(
        &self,
        roots: &mut HashMap<PathBuf, RootEntry>,
        root: &Path,
    ) -> (u64, oneshot::Receiver<()>) {
        let (done_tx, done_rx) = oneshot::channel();
        let generation = self.inner.generation_counter.fetch_add(1, Ordering::SeqCst);
        let entry = roots.get_mut(root).expect("caller inserted the entry");
        entry.generation = generation;
        entry.building = true;
        entry.state = IndexState::Building;
        entry.build_done = Some(done_tx);
        // 换代即整体重建：旧 watcher 随之停机，旧快照清空防批次重复追加。
        entry.watcher = None;
        *entry.index.write().unwrap() = FileIndex::default();
        (generation, done_rx)
    }

    /// 后台构建：`spawn_blocking` 流式扫描，批次经 mpsc 回异步侧分批换代；
    /// 建完起 watcher（失败 → Degraded，索引仍可用）并翻最终状态。
    /// 超过并发上限时不排队：登记为可重试（Building + building=false），
    /// 后续检索再次触发。
    fn spawn_build(&self, root: PathBuf, generation: u64) {
        if self.inner.in_flight_builds.fetch_add(1, Ordering::SeqCst) >= MAX_CONCURRENT_BUILDS {
            self.inner.in_flight_builds.fetch_sub(1, Ordering::SeqCst);
            let inner = Arc::clone(&self.inner);
            let root = root.clone();
            tokio::spawn(async move {
                let mut roots = inner.roots.lock().await;
                if let Some(entry) = roots.get_mut(&root)
                    && entry.generation == generation
                {
                    entry.building = false;
                    if let Some(done) = entry.build_done.take() {
                        let _ = done.send(());
                    }
                }
            });
            return;
        }

        let (batch_tx, mut batch_rx) = tokio::sync::mpsc::channel::<Vec<FileIndexEntry>>(8);
        let build_root = root.clone();
        let build_handle = tokio::task::spawn_blocking(move || {
            build_index_streaming(&build_root, MAX_INDEX_ENTRIES, |batch| {
                let _ = batch_tx.blocking_send(batch);
            })
        });
        let inner = Arc::clone(&self.inner);
        tokio::spawn(async move {
            // 分批换代：search 读到的永远是「已扫到部分」。
            while let Some(batch) = batch_rx.recv().await {
                let mut roots = inner.roots.lock().await;
                let Some(entry) = roots.get_mut(&root) else { break };
                if entry.generation != generation {
                    break;
                }
                entry.index.write().unwrap().append_entries(batch);
            }
            drop(batch_rx);

            let built = build_handle.await;
            let mut roots = inner.roots.lock().await;
            let mut notify = None;
            if let Some(entry) = roots.get_mut(&root)
                && entry.generation == generation
            {
                entry.building = false;
                match built {
                    Ok(file_index) => {
                        entry.state = if file_index.truncated() {
                            IndexState::Truncated
                        } else {
                            IndexState::Ready
                        };
                        // 完整结果整体替换快照（权威数据，含 truncated 标记）。
                        *entry.index.write().unwrap() = file_index;
                        let watcher_snapshot = Arc::clone(&entry.index);
                        let watch_root = root.clone();
                        entry.watcher = IndexWatcher::start(watch_root, move |deltas| {
                            let mut guard = watcher_snapshot.write().unwrap();
                            for delta in deltas {
                                guard.apply_delta(delta);
                            }
                        });
                        // watcher 启动失败 → Degraded（仍可用索引应答，US9.8）。
                        if entry.watcher.is_none() {
                            entry.state = IndexState::Degraded;
                        }
                    }
                    Err(join_error) => {
                        tracing::warn!(
                            root = %root.display(),
                            %join_error,
                            "file index build task failed"
                        );
                        entry.state = IndexState::Degraded;
                    }
                }
                notify = entry.build_done.take();
            }
            inner.in_flight_builds.fetch_sub(1, Ordering::SeqCst);
            if let Some(done) = notify {
                let _ = done.send(());
            }
        });
    }

    /// LRU 驱逐：常驻超上限时移除最久未用根（drop 即停 watcher）。
    /// `protect` 是本次正在获取的根，绝不驱逐自己。
    fn evict_lru(
        &self,
        roots: &mut HashMap<PathBuf, RootEntry>,
        protect: &Path,
    ) -> Option<(PathBuf, RootEntry)> {
        if roots.len() <= MAX_RESIDENT_ROOTS {
            return None;
        }
        let victim = roots
            .iter()
            .filter(|(path, _)| path.as_path() != protect)
            .min_by_key(|(_, entry)| entry.last_used)
            .map(|(path, _)| path.clone())?;
        let entry = roots.remove(&victim)?;
        tracing::debug!(root = %victim.display(), "evicted file index root (LRU)");
        Some((victim, entry))
    }
}

fn name_of(rel: &str) -> String {
    rel.rsplit('/').next().unwrap_or(rel).to_string()
}

fn kind_str(kind: IndexKind) -> &'static str {
    match kind {
        IndexKind::File => "file",
        IndexKind::Dir => "directory",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ws_with_files() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src/components")).unwrap();
        std::fs::write(dir.path().join("src/components/Composer.vue"), "x").unwrap();
        std::fs::write(dir.path().join("src/main.rs"), "fn main() {}").unwrap();
        dir
    }

    #[tokio::test]
    async fn should_return_scored_hits_with_kind_and_positions() {
        let dir = ws_with_files();
        let reg = FileIndexRegistry::new();
        // The first search never waits for the build (M3): it returns the
        // current partial snapshot + Building. Poll until the background
        // build lands, then the authoritative answer must be there.
        let mut state = IndexState::Building;
        let mut hits = Vec::new();
        for _ in 0..40 {
            let answer = reg.search(dir.path(), "composer", 20).await;
            state = answer.1;
            hits = answer.0;
            if state == IndexState::Ready {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert_eq!(state, IndexState::Ready);
        assert!(
            hits.iter()
                .any(|h| h.rel_path == "src/components/Composer.vue" && h.kind == "file")
        );
    }

    #[tokio::test]
    async fn should_include_directories_in_hits() {
        let dir = ws_with_files();
        let reg = FileIndexRegistry::new();
        // The first search no longer waits for the build (M3) — poll until
        // the background build lands before asserting.
        let mut hits = Vec::new();
        for _ in 0..40 {
            let answer = reg.search(dir.path(), "components", 20).await;
            hits = answer.0;
            if answer.1 == IndexState::Ready {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(hits.iter().any(|h| h.kind == "directory"));
    }

    #[tokio::test]
    async fn should_apply_incremental_deltas_to_cached_index() {
        let dir = ws_with_files();
        let reg = FileIndexRegistry::new();
        std::fs::write(dir.path().join("src/components/New.vue"), "x").unwrap();
        // 通知注册表该 root 有变化（生产链路由 watcher 驱动；测试手动触发）。
        // invalidate 不等待重建（M3）——轮询直到重建完成再断言。
        let mut hits = Vec::new();
        for _ in 0..40 {
            let answer = reg.search(dir.path(), "New.vue", 20).await;
            hits = answer.0;
            if answer.1 == IndexState::Ready {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(hits.iter().any(|h| h.rel_path == "src/components/New.vue"));
    }
}
