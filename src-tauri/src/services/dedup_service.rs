//! 去重分组算法 + 流水线协调器。
//!
//! 流程（docs/04 § 2 T2-T7）：
//! 1. `start_dedup` 把所有 pending 的 BLAKE3 / dHash 任务入队（包成 Tracked* wrapper 跟踪进度）
//! 2. 一个 tokio watcher 轮询协调器的 inflight 计数，等到全部归零
//! 3. 跑 exact 分组（`blake3 GROUP BY`）和 visual 分组（BK-tree + union-find）
//! 4. emit `dedup:done`
//!
//! 不依赖 `Scheduler::status()`——后者会被并行的扫描任务干扰。`DedupCoordinator`
//! 只跟踪本次 dedup 自己入队的哈希任务。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter};

use crate::db::Pool;
use crate::error::AppResult;
use crate::queue::{Priority, Scheduler, Task, TaskContext};
use crate::repo::{duplicates_repo, images_repo};
use crate::services::hash_service::BlakeHashTask;
use crate::services::phash_service::{self, DhashTask};
use crate::utils::bk_tree::DhashIndex;

/// czkawka 的 "High" 档：hash_size=8 时阈值 2。MVP2 默认。
pub const VISUAL_THRESHOLD_DEFAULT: u32 = 2;

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum DedupMethod {
    Exact,
    Phash,
    Both,
}

impl DedupMethod {
    pub fn includes_exact(self) -> bool {
        matches!(self, Self::Exact | Self::Both)
    }
    pub fn includes_visual(self) -> bool {
        matches!(self, Self::Phash | Self::Both)
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DedupStatus {
    pub running: bool,
    pub hash_pending: usize,
    pub dhash_pending: usize,
    pub groups_found: usize,
    pub phase: String, // "idle" | "hashing" | "grouping" | "done"
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DedupProgressEvent {
    pub running: bool,
    pub hash_pending: usize,
    pub dhash_pending: usize,
    pub groups_found: usize,
    pub phase: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NewGroupEvent {
    pub group_id: i64,
    pub method: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DedupDoneEvent {
    pub exact_groups: usize,
    pub visual_groups: usize,
    pub error: Option<String>,
}

#[derive(Default)]
pub struct DedupCoordinator {
    hash_inflight: AtomicUsize,
    dhash_inflight: AtomicUsize,
    groups_found: AtomicUsize,
    running: AtomicBool,
    phase: std::sync::Mutex<String>,
}

impl DedupCoordinator {
    pub fn new() -> Self {
        Self {
            phase: std::sync::Mutex::new("idle".to_string()),
            ..Self::default()
        }
    }

    pub fn status(&self) -> DedupStatus {
        DedupStatus {
            running: self.running.load(Ordering::Relaxed),
            hash_pending: self.hash_inflight.load(Ordering::Relaxed),
            dhash_pending: self.dhash_inflight.load(Ordering::Relaxed),
            groups_found: self.groups_found.load(Ordering::Relaxed),
            phase: self.read_phase(),
        }
    }

    fn read_phase(&self) -> String {
        self.phase
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_else(|_| "unknown".to_string())
    }

    fn set_phase(&self, phase: &str) {
        if let Ok(mut guard) = self.phase.lock() {
            *guard = phase.to_string();
        }
    }

    fn set_running(&self, v: bool) {
        self.running.store(v, Ordering::Relaxed);
    }

    fn reset(&self) {
        self.hash_inflight.store(0, Ordering::Relaxed);
        self.dhash_inflight.store(0, Ordering::Relaxed);
        self.groups_found.store(0, Ordering::Relaxed);
        self.set_phase("idle");
    }
}

#[derive(Clone)]
struct TrackedBlakeTask {
    inner: BlakeHashTask,
    coord: Arc<DedupCoordinator>,
}

#[async_trait]
impl Task for TrackedBlakeTask {
    fn priority(&self) -> Priority {
        Priority::P2
    }
    fn label(&self) -> String {
        self.inner.label()
    }
    async fn run(&self, ctx: TaskContext) -> AppResult<()> {
        let result = self.inner.run(ctx).await;
        self.coord.hash_inflight.fetch_sub(1, Ordering::Relaxed);
        result
    }
}

#[derive(Clone)]
struct TrackedDhashTask {
    inner: DhashTask,
    coord: Arc<DedupCoordinator>,
}

#[async_trait]
impl Task for TrackedDhashTask {
    fn priority(&self) -> Priority {
        Priority::P3
    }
    fn label(&self) -> String {
        self.inner.label()
    }
    async fn run(&self, ctx: TaskContext) -> AppResult<()> {
        let result = self.inner.run(ctx).await;
        self.coord.dhash_inflight.fetch_sub(1, Ordering::Relaxed);
        result
    }
}

#[allow(clippy::too_many_arguments)]
pub fn start_dedup(
    pool: Pool,
    scheduler: Scheduler,
    coord: Arc<DedupCoordinator>,
    index: Arc<DhashIndex>,
    thumbs_dir: PathBuf,
    method: DedupMethod,
    threshold: u32,
    app: AppHandle,
) -> AppResult<()> {
    if coord.running.load(Ordering::Relaxed) {
        tracing::warn!("dedup already running, ignoring start");
        return Ok(());
    }
    coord.reset();
    coord.set_running(true);
    coord.set_phase("hashing");

    if method.includes_exact() {
        let conn = pool.get()?;
        for img in images_repo::pending_blake3_images(&conn)? {
            coord.hash_inflight.fetch_add(1, Ordering::Relaxed);
            let wrap = TrackedBlakeTask {
                inner: BlakeHashTask::new(pool.clone(), img.id, PathBuf::from(img.full_path)),
                coord: coord.clone(),
            };
            if let Err(error) = scheduler.enqueue(wrap) {
                coord.hash_inflight.fetch_sub(1, Ordering::Relaxed);
                return Err(error);
            }
        }
    }

    if method.includes_visual() {
        let conn = pool.get()?;
        for img in images_repo::pending_dhash_images(&conn)? {
            let Some(thumb_hash) = img.thumb_hash.clone() else {
                continue;
            };
            coord.dhash_inflight.fetch_add(1, Ordering::Relaxed);
            let wrap = TrackedDhashTask {
                inner: DhashTask::new(
                    pool.clone(),
                    img.id,
                    thumbs_dir.clone(),
                    thumb_hash,
                    index.clone(),
                ),
                coord: coord.clone(),
            };
            if let Err(error) = scheduler.enqueue(wrap) {
                coord.dhash_inflight.fetch_sub(1, Ordering::Relaxed);
                return Err(error);
            }
        }
    }

    let coord_watch = coord.clone();
    let app_watch = app.clone();
    let pool_watch = pool.clone();
    let index_watch = index.clone();

    // 必须用 Tauri 托管的 async runtime：`start_dedup` 由同步命令 `dedup_start` 调用，
    // 而同步命令跑在主线程，主线程没有进入 tokio runtime 上下文。直接 `tokio::spawn`
    // 会 panic（no reactor running），导致整个应用闪退。`async_runtime::spawn` 不依赖
    // 当前线程的 runtime 上下文，且其内部就是 tokio，future 里的 sleep/spawn_blocking 照常可用。
    tauri::async_runtime::spawn(async move {
        // 1. 等所有哈希任务完成（轮询自己的 inflight 计数）
        loop {
            tokio::time::sleep(Duration::from_millis(500)).await;
            let s = coord_watch.status();
            emit_progress(&app_watch, &s);
            if s.hash_pending == 0 && s.dhash_pending == 0 {
                break;
            }
        }

        coord_watch.set_phase("grouping");
        emit_progress(&app_watch, &coord_watch.status());

        // 2. 跑分组（blocking → spawn_blocking）
        let coord_block = coord_watch.clone();
        let app_block = app_watch.clone();
        let join = tokio::task::spawn_blocking(move || -> AppResult<(usize, usize)> {
            let conn = pool_watch.get()?;
            let mut exact_groups = 0;
            let mut visual_groups = 0;
            if method.includes_exact() {
                exact_groups = run_exact_grouping(&conn, &coord_block, &app_block)?;
            }
            if method.includes_visual() {
                visual_groups =
                    run_visual_grouping(&conn, &index_watch, threshold, &coord_block, &app_block)?;
            }
            Ok((exact_groups, visual_groups))
        })
        .await;

        let (exact_groups, visual_groups, error) = match join {
            Ok(Ok((e, v))) => (e, v, None),
            Ok(Err(error)) => {
                tracing::error!(%error, "dedup grouping failed");
                (0, 0, Some(error.to_string()))
            }
            Err(error) => {
                tracing::error!(%error, "dedup grouping join failed");
                (0, 0, Some(error.to_string()))
            }
        };

        coord_watch.set_phase("done");
        coord_watch.set_running(false);
        emit_progress(&app_watch, &coord_watch.status());
        let _ = app_watch.emit(
            "dedup:done",
            DedupDoneEvent {
                exact_groups,
                visual_groups,
                error,
            },
        );
    });

    Ok(())
}

fn emit_progress(app: &AppHandle, status: &DedupStatus) {
    let _ = app.emit(
        "dedup:progress",
        DedupProgressEvent {
            running: status.running,
            hash_pending: status.hash_pending,
            dhash_pending: status.dhash_pending,
            groups_found: status.groups_found,
            phase: status.phase.clone(),
        },
    );
}

/// 按 blake3 GROUP BY 找完全重复。每次跑前先 drop 老的 open exact group 避免重复。
pub fn run_exact_grouping(
    conn: &Connection,
    coord: &DedupCoordinator,
    app: &AppHandle,
) -> AppResult<usize> {
    run_exact_grouping_with(conn, coord, |group_id, method| {
        let _ = app.emit(
            "dedup:new_group",
            NewGroupEvent {
                group_id,
                method: method.to_string(),
            },
        );
    })
}

/// `run_exact_grouping` 的底层版本，便于在集成测试里跑（不依赖 AppHandle）。
pub fn run_exact_grouping_with(
    conn: &Connection,
    coord: &DedupCoordinator,
    mut on_new_group: impl FnMut(i64, &str),
) -> AppResult<usize> {
    duplicates_repo::drop_open_groups(conn, duplicates_repo::METHOD_EXACT)?;
    let groups = images_repo::find_blake3_duplicates(conn)?;
    let now = now_unix();
    let mut count = 0;
    for (_hash, ids) in &groups {
        let group_id =
            duplicates_repo::insert_group(conn, duplicates_repo::METHOD_EXACT, None, now)?;
        let items: Vec<(i64, Option<f64>)> = ids.iter().map(|id| (*id, Some(1.0))).collect();
        duplicates_repo::insert_items(conn, group_id, &items)?;
        coord.groups_found.fetch_add(1, Ordering::Relaxed);
        on_new_group(group_id, "exact");
        count += 1;
    }
    Ok(count)
}

/// BK-tree + union-find 视觉近似分组。threshold 是 hamming distance 上限。
pub fn run_visual_grouping(
    conn: &Connection,
    index: &DhashIndex,
    threshold: u32,
    coord: &DedupCoordinator,
    app: &AppHandle,
) -> AppResult<usize> {
    run_visual_grouping_with(conn, index, threshold, coord, |group_id, method| {
        let _ = app.emit(
            "dedup:new_group",
            NewGroupEvent {
                group_id,
                method: method.to_string(),
            },
        );
    })
}

pub fn run_visual_grouping_with(
    conn: &Connection,
    index: &DhashIndex,
    threshold: u32,
    coord: &DedupCoordinator,
    mut on_new_group: impl FnMut(i64, &str),
) -> AppResult<usize> {
    duplicates_repo::drop_open_groups(conn, duplicates_repo::METHOD_PHASH)?;

    let all_hashes = images_repo::all_dhashes(conn)?;
    if all_hashes.len() < 2 {
        return Ok(0);
    }

    // 用 image_id → root 的 HashMap 表示 union-find
    let mut parent: HashMap<i64, i64> = all_hashes.iter().map(|(id, _)| (*id, *id)).collect();
    let id_to_dhash: HashMap<i64, u64> = all_hashes.iter().copied().collect();

    for (id, dhash) in &all_hashes {
        let neighbors = index.find_within(*dhash, threshold);
        for (other_id, _dist) in neighbors {
            if other_id == *id || !parent.contains_key(&other_id) {
                continue;
            }
            union(&mut parent, *id, other_id);
        }
    }

    // 按 root 聚类
    let mut groups: HashMap<i64, Vec<i64>> = HashMap::new();
    let ids: Vec<i64> = parent.keys().copied().collect();
    for id in ids {
        let root = find(&mut parent, id);
        groups.entry(root).or_default().push(id);
    }

    let now = now_unix();
    let mut written = 0;
    for (_root, mut ids) in groups {
        if ids.len() < 2 {
            continue;
        }
        ids.sort();
        let ref_dhash = id_to_dhash[&ids[0]];
        let items: Vec<(i64, Option<f64>)> = ids
            .iter()
            .map(|id| {
                let dh = id_to_dhash[id];
                let dist = phash_service::hamming(ref_dhash, dh);
                let similarity = 1.0 - (dist as f64 / 64.0);
                (*id, Some(similarity))
            })
            .collect();
        let group_id = duplicates_repo::insert_group(
            conn,
            duplicates_repo::METHOD_PHASH,
            Some(threshold as f64),
            now,
        )?;
        duplicates_repo::insert_items(conn, group_id, &items)?;
        coord.groups_found.fetch_add(1, Ordering::Relaxed);
        on_new_group(group_id, "phash");
        written += 1;
    }
    Ok(written)
}

fn find(parent: &mut HashMap<i64, i64>, mut x: i64) -> i64 {
    let mut path = Vec::new();
    while parent[&x] != x {
        path.push(x);
        x = parent[&x];
    }
    for node in path {
        parent.insert(node, x);
    }
    x
}

fn union(parent: &mut HashMap<i64, i64>, a: i64, b: i64) {
    let ra = find(parent, a);
    let rb = find(parent, b);
    if ra != rb {
        parent.insert(ra, rb);
    }
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo::images_repo::NewImageMetadata;
    use crate::repo::roots_repo::{self, NewRoot};

    fn fresh_pool() -> (crate::db::Pool, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let pool = crate::db::open(&dir.path().join("app.sqlite")).expect("db");
        (pool, dir)
    }

    fn add_image(conn: &Connection, root_id: i64, name: &str) -> i64 {
        images_repo::upsert_scanned_image(
            conn,
            &NewImageMetadata {
                root_id,
                rel_path: name.to_string(),
                filename: name.to_string(),
                extension: "jpg".to_string(),
                size_bytes: 1,
                mtime: 1,
                width: None,
                height: None,
                orientation: None,
                taken_at: None,
                gps_lat: None,
                gps_lng: None,
                camera_make: None,
                camera_model: None,
            },
            1,
        )
        .expect("insert")
        .image_id()
    }

    #[test]
    fn union_find_groups_chained_nodes() {
        let mut parent: HashMap<i64, i64> = (1..=5).map(|i| (i, i)).collect();
        union(&mut parent, 1, 2);
        union(&mut parent, 2, 3);
        union(&mut parent, 4, 5);
        let r1 = find(&mut parent, 1);
        let r2 = find(&mut parent, 2);
        let r3 = find(&mut parent, 3);
        let r4 = find(&mut parent, 4);
        let r5 = find(&mut parent, 5);
        assert_eq!(r1, r2);
        assert_eq!(r2, r3);
        assert_eq!(r4, r5);
        assert_ne!(r1, r4);
    }

    /// 直接调 run_exact_grouping，需要伪造一个 AppHandle ——
    /// 这部分留给集成测试（tests/dedup_tests.rs），单测验证内部逻辑/SQL 即可。
    #[test]
    fn pending_blake3_excludes_failed_and_deleted() {
        let (pool, _dir) = fresh_pool();
        let conn = pool.get().expect("conn");
        let root = roots_repo::insert(
            &conn,
            &NewRoot {
                path: "/tmp/d".to_string(),
                label: None,
                root_type: "local".to_string(),
            },
            1,
        )
        .expect("root");

        let a = add_image(&conn, root.id, "a.jpg");
        let b = add_image(&conn, root.id, "b.jpg");
        let _c = add_image(&conn, root.id, "c.jpg");

        // a 已算 blake3
        images_repo::set_blake3(&conn, a, "deadbeef").expect("blake a");
        // b 标 failed
        images_repo::set_hash_status(&conn, b, "failed").expect("status b");
        // c 软删
        images_repo::mark_deleted(&conn, root.id, "c.jpg", 100).expect("delete c");

        let pending = images_repo::pending_blake3_images(&conn).expect("pending");
        assert_eq!(pending.len(), 0, "all three should be excluded");
    }
}
