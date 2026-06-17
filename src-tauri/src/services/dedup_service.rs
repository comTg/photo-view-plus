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

use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter};

use crate::db::Pool;
use crate::error::{AppError, AppResult};
use crate::queue::{Priority, Scheduler, Task, TaskContext};
use crate::repo::{duplicates_repo, images_repo};
use crate::services::hash_service::BlakeHashTask;
use crate::services::phash_service::{self, DhashTask};
use crate::services::trash_service;
use crate::utils::bk_tree::DhashIndex;
use crate::utils::path_normalize;

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

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DedupAction {
    /// 删除 group 中除 keep 之外的所有图（走回收站）
    Trash,
    /// 标记 group 已"keep all"，不删任何文件
    KeepAll,
    /// 标记 group "不是重复"（dismissed）
    Dismiss,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DedupKeepRule {
    Largest,
    Newest,
    BiggestFile,
    ShortestName,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DedupBatchResolveArgs {
    /// None / "all" 只允许 keep_all / dismiss；批量删除必须明确 exact 或 phash。
    pub method: Option<String>,
    pub action: DedupAction,
    pub keep_rule: Option<DedupKeepRule>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DedupBatchGroupFailure {
    pub group_id: i64,
    pub error: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DedupBatchResolveResult {
    pub matched_groups: usize,
    pub resolved_groups: Vec<i64>,
    pub failed_groups: Vec<DedupBatchGroupFailure>,
    pub trashed: Vec<i64>,
    /// `trashed` 的子集：网络盘无回收站，已永久删除（不可撤销恢复）。
    pub permanently_deleted: Vec<i64>,
    pub trash_failures: Vec<trash_service::TrashFailure>,
    pub undo_id: Option<i64>,
}

#[derive(Debug)]
struct BatchTrashPlan {
    group_id: i64,
    keep_image_id: i64,
    to_trash: Vec<i64>,
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
            let is_network = path_normalize::is_network_path(std::path::Path::new(&img.root_path));
            let wrap = TrackedBlakeTask {
                inner: BlakeHashTask::new(
                    pool.clone(),
                    img.id,
                    PathBuf::from(img.full_path),
                    is_network,
                ),
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

pub fn batch_resolve(
    pool: &Pool,
    args: DedupBatchResolveArgs,
    now: i64,
) -> AppResult<DedupBatchResolveResult> {
    let method = normalize_batch_method(args.method.as_deref())?;
    if args.action == DedupAction::Trash && method.is_none() {
        return Err(AppError::Other(
            "批量删除必须先选择一种分组类型（完全重复或视觉近似）".to_string(),
        ));
    }
    let keep_rule = match (args.action, args.keep_rule) {
        (DedupAction::Trash, Some(rule)) => Some(rule),
        (DedupAction::Trash, None) => {
            return Err(AppError::Other("批量删除缺少保留规则".to_string()))
        }
        _ => None,
    };

    let conn = pool.get()?;
    let group_ids = duplicates_repo::list_group_ids(&conn, method.as_deref(), Some("open"))?;
    let matched_groups = group_ids.len();
    if group_ids.is_empty() {
        return Ok(empty_batch_result(matched_groups));
    }

    if args.action != DedupAction::Trash {
        let status_after = match args.action {
            DedupAction::KeepAll => "resolved",
            DedupAction::Dismiss => "dismissed",
            DedupAction::Trash => unreachable!(),
        };
        let mut resolved_groups = Vec::with_capacity(group_ids.len());
        for group_id in group_ids {
            duplicates_repo::update_group_status(&conn, group_id, status_after, None, Some(now))?;
            resolved_groups.push(group_id);
        }
        return Ok(DedupBatchResolveResult {
            matched_groups,
            resolved_groups,
            failed_groups: Vec::new(),
            trashed: Vec::new(),
            permanently_deleted: Vec::new(),
            trash_failures: Vec::new(),
            undo_id: None,
        });
    }

    let rule = keep_rule.expect("trash branch checked keep_rule");
    let mut plans = Vec::new();
    let mut failed_groups = Vec::new();
    for group_id in group_ids {
        let items = duplicates_repo::items_for_group(&conn, group_id)?;
        let mut images = Vec::new();
        for item in items {
            match images_repo::get_detail(&conn, item.image_id)? {
                Some(image) if image.deleted_at.is_none() => images.push(image),
                Some(_) => {}
                None => failed_groups.push(DedupBatchGroupFailure {
                    group_id,
                    error: format!("图片不存在：{}", item.image_id),
                }),
            }
        }

        if images.len() < 2 {
            failed_groups.push(DedupBatchGroupFailure {
                group_id,
                error: "分组中可处理图片少于 2 张".to_string(),
            });
            continue;
        }

        let Some(keep_image_id) = choose_keep_image_id(&images, rule) else {
            failed_groups.push(DedupBatchGroupFailure {
                group_id,
                error: "无法根据规则选择保留图片".to_string(),
            });
            continue;
        };
        let to_trash = images
            .iter()
            .map(|image| image.id)
            .filter(|id| *id != keep_image_id)
            .collect();
        plans.push(BatchTrashPlan {
            group_id,
            keep_image_id,
            to_trash,
        });
    }
    drop(conn);

    let mut trash_ids = Vec::new();
    let mut seen = HashSet::new();
    for plan in &plans {
        for image_id in &plan.to_trash {
            if seen.insert(*image_id) {
                trash_ids.push(*image_id);
            }
        }
    }

    let outcome = if trash_ids.is_empty() {
        empty_trash_outcome()
    } else {
        trash_service::trash_images(pool, &trash_ids, now)?
    };

    let succeeded: HashSet<i64> = outcome.succeeded.iter().copied().collect();
    let conn = pool.get()?;
    let mut resolved_groups = Vec::new();
    for plan in plans {
        if plan
            .to_trash
            .iter()
            .all(|image_id| succeeded.contains(image_id))
        {
            duplicates_repo::update_group_status(
                &conn,
                plan.group_id,
                "resolved",
                Some(plan.keep_image_id),
                Some(now),
            )?;
            resolved_groups.push(plan.group_id);
        } else {
            let succeeded_in_group: Vec<i64> = plan
                .to_trash
                .iter()
                .copied()
                .filter(|image_id| succeeded.contains(image_id))
                .collect();
            if !succeeded_in_group.is_empty() {
                duplicates_repo::remove_items(&conn, plan.group_id, &succeeded_in_group)?;
            }
            failed_groups.push(DedupBatchGroupFailure {
                group_id: plan.group_id,
                error: "部分图片删除失败，分组保持未处理".to_string(),
            });
        }
    }

    Ok(DedupBatchResolveResult {
        matched_groups,
        resolved_groups,
        failed_groups,
        trashed: outcome.succeeded,
        permanently_deleted: outcome.permanently_deleted,
        trash_failures: outcome.failed,
        undo_id: outcome.undo_id,
    })
}

fn normalize_batch_method(method: Option<&str>) -> AppResult<Option<String>> {
    match method.map(str::trim).filter(|s| !s.is_empty()) {
        None | Some("all") => Ok(None),
        Some("exact") => Ok(Some("exact".to_string())),
        Some("phash") => Ok(Some("phash".to_string())),
        Some(other) => Err(AppError::Other(format!("未知分组类型：{other}"))),
    }
}

pub fn choose_keep_image_id(
    images: &[images_repo::ImageRecord],
    rule: DedupKeepRule,
) -> Option<i64> {
    let image = match rule {
        DedupKeepRule::Largest => images
            .iter()
            .max_by_key(|image| (pixel_count(image), image.size_bytes, image.mtime, image.id)),
        DedupKeepRule::Newest => images
            .iter()
            .max_by_key(|image| (image.mtime, pixel_count(image), image.size_bytes, image.id)),
        DedupKeepRule::BiggestFile => images
            .iter()
            .max_by_key(|image| (image.size_bytes, pixel_count(image), image.mtime, image.id)),
        DedupKeepRule::ShortestName => images.iter().min_by_key(|image| {
            (
                image.filename.chars().count(),
                Reverse(pixel_count(image)),
                Reverse(image.size_bytes),
                Reverse(image.mtime),
                image.id,
            )
        }),
    }?;
    Some(image.id)
}

fn pixel_count(image: &images_repo::ImageRecord) -> i128 {
    i128::from(image.width.unwrap_or(0).max(0)) * i128::from(image.height.unwrap_or(0).max(0))
}

fn empty_batch_result(matched_groups: usize) -> DedupBatchResolveResult {
    DedupBatchResolveResult {
        matched_groups,
        resolved_groups: Vec::new(),
        failed_groups: Vec::new(),
        trashed: Vec::new(),
        permanently_deleted: Vec::new(),
        trash_failures: Vec::new(),
        undo_id: None,
    }
}

fn empty_trash_outcome() -> trash_service::TrashOutcome {
    trash_service::TrashOutcome {
        succeeded: Vec::new(),
        permanently_deleted: Vec::new(),
        failed: Vec::new(),
        undo_id: None,
    }
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
    fn pending_blake3_skips_done_and_deleted_but_retries_failed() {
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
        let d = add_image(&conn, root.id, "d.jpg");

        // a 已算 blake3 -> 跳过
        images_repo::set_blake3(&conn, a, "deadbeef").expect("blake a");
        // b 曾被标记 failed -> 现在应被重试（不再因 hash_status 排除）
        images_repo::set_hash_status(&conn, b, "failed").expect("status b");
        // c 软删 -> 跳过
        images_repo::mark_deleted(&conn, root.id, "c.jpg", 100).expect("delete c");
        // d 全新 pending -> 应入选

        let pending = images_repo::pending_blake3_images(&conn).expect("pending");
        let mut ids: Vec<i64> = pending.iter().map(|r| r.id).collect();
        ids.sort();
        let mut expected = vec![b, d];
        expected.sort();
        assert_eq!(
            ids, expected,
            "failed(b) 应被重试、新图(d) 应入选；已算(a)/已删(c) 跳过"
        );
    }
}
