use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use chrono::{Local, NaiveDateTime, TimeZone};
use exif::{Reader, Tag, Value};
use rusqlite::params;
use serde::Serialize;
use tauri::{AppHandle, Emitter};
use tokio_util::sync::CancellationToken;
use walkdir::WalkDir;

use crate::config::AppPaths;
use crate::db::Pool;
use crate::error::{AppError, AppResult};
use crate::queue::{Priority, Scheduler, Task, TaskContext};
use crate::repo::images_repo::{self, NewImageMetadata, UpsertOutcome};
use crate::repo::roots_repo::{self, Root};
use crate::services::thumbnail_service::ThumbnailTask;

const IMAGE_EXTENSIONS: &[&str] = &[
    "jpg", "jpeg", "png", "webp", "heic", "heif", "bmp", "tiff", "tif", "gif",
];

#[derive(Clone)]
pub struct ScanRootTask {
    pool: Pool,
    scheduler: Scheduler,
    app: AppHandle,
    paths: AppPaths,
    root_id: i64,
    task_id: i64,
}

impl ScanRootTask {
    pub fn new(
        pool: Pool,
        scheduler: Scheduler,
        app: AppHandle,
        paths: AppPaths,
        root_id: i64,
        task_id: i64,
    ) -> Self {
        Self {
            pool,
            scheduler,
            app,
            paths,
            root_id,
            task_id,
        }
    }
}

#[async_trait]
impl Task for ScanRootTask {
    fn priority(&self) -> Priority {
        Priority::P1
    }

    fn label(&self) -> String {
        format!("scan-root:{}", self.root_id)
    }

    async fn run(&self, ctx: TaskContext) -> AppResult<()> {
        let task = self.clone();
        tokio::task::spawn_blocking(move || run_scan_blocking(task, ctx.cancellation_token()))
            .await?
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanStartResult {
    pub task_id: i64,
    pub root_id: i64,
    pub status: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanTaskStatus {
    pub id: i64,
    pub root_id: i64,
    pub status: String,
    pub total_files: Option<i64>,
    pub processed: i64,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanProgress {
    pub root_id: i64,
    pub task_id: i64,
    pub processed: i64,
    pub total: i64,
    pub current_file: Option<String>,
    pub eta_sec: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ScanDone {
    pub root_id: i64,
    pub task_id: i64,
    pub added: i64,
    pub updated: i64,
    pub removed: i64,
    /// 只差大小写的同级目录（docs/11 §6.1、R1-6），前端在扫描完成提示里告警。
    pub case_conflicts: Vec<CaseConflict>,
}

/// 同一父目录下只差大小写的几个子目录。NAS 的文件系统区分大小写，Windows 经 SMB 访问时不区分，
/// 这些目录下的同名路径只能落到其中一个目录：同一批照片会重复入库，一部分读的时候报"找不到文件"。
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CaseConflict {
    /// 父目录，相对根目录；就在根目录下时为空串。
    pub parent: String,
    /// 只差大小写的目录名，按扫描时遇到的顺序。
    pub names: Vec<String>,
}

/// 扫描时逐个记下目录名，找出同一父目录下只差大小写的子目录。
#[derive(Debug, Default)]
pub struct CaseConflictDetector {
    /// 父目录 → 小写目录名 → 第一次遇到的原名，以及已登记的冲突在 `conflicts` 里的下标。
    seen: HashMap<String, HashMap<String, (String, Option<usize>)>>,
    conflicts: Vec<CaseConflict>,
}

impl CaseConflictDetector {
    pub fn observe(&mut self, parent: &str, name: &str) {
        let dirs = self.seen.entry(parent.to_string()).or_default();
        let Some((first, conflict)) = dirs.get_mut(&name.to_lowercase()) else {
            dirs.insert(name.to_lowercase(), (name.to_string(), None));
            return;
        };
        if first == name {
            return;
        }
        match conflict {
            Some(index) => {
                let names = &mut self.conflicts[*index].names;
                if !names.iter().any(|existing| existing == name) {
                    names.push(name.to_string());
                }
            }
            None => {
                *conflict = Some(self.conflicts.len());
                self.conflicts.push(CaseConflict {
                    parent: parent.to_string(),
                    names: vec![first.clone(), name.to_string()],
                });
            }
        }
    }

    pub fn into_conflicts(self) -> Vec<CaseConflict> {
        self.conflicts
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanErrorEvent {
    pub root_id: i64,
    pub task_id: i64,
    pub error: String,
    pub file: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct ImageProbe {
    width: Option<i64>,
    height: Option<i64>,
    orientation: Option<i64>,
    taken_at: Option<i64>,
    gps_lat: Option<f64>,
    gps_lng: Option<f64>,
    camera_make: Option<String>,
    camera_model: Option<String>,
}

pub fn create_scan_task(pool: &Pool, root_id: i64) -> AppResult<ScanStartResult> {
    let conn = pool.get()?;
    let now = now_unix();
    let root = roots_repo::get(&conn, root_id)?
        .ok_or_else(|| AppError::Other(format!("目录不存在：{root_id}")))?;
    if !root.enabled {
        return Err(AppError::Other("目录已禁用，不能扫描".to_string()));
    }
    conn.execute(
        "INSERT INTO scan_tasks(root_id, status, processed, started_at)
         VALUES (?1, 'queued', 0, ?2)",
        params![root_id, now],
    )?;
    let task_id = conn.last_insert_rowid();
    Ok(ScanStartResult {
        task_id,
        root_id,
        status: "queued".to_string(),
    })
}

pub fn list_scan_status(pool: &Pool) -> AppResult<Vec<ScanTaskStatus>> {
    let conn = pool.get()?;
    let mut stmt = conn.prepare(
        "SELECT id, root_id, status, total_files, processed, started_at, finished_at, error
         FROM scan_tasks
         ORDER BY COALESCE(started_at, 0) DESC, id DESC
         LIMIT 20",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(ScanTaskStatus {
            id: row.get(0)?,
            root_id: row.get(1)?,
            status: row.get(2)?,
            total_files: row.get(3)?,
            processed: row.get(4)?,
            started_at: row.get(5)?,
            finished_at: row.get(6)?,
            error: row.get(7)?,
        })
    })?;

    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

pub fn recover_interrupted_tasks(pool: &Pool) -> AppResult<usize> {
    let conn = pool.get()?;
    let now = now_unix();
    let updated = conn.execute(
        "UPDATE scan_tasks
         SET status = 'failed',
             finished_at = ?1,
             error = '应用退出，扫描任务已中断，请重新扫描'
         WHERE status IN ('queued', 'running')",
        params![now],
    )?;
    if updated > 0 {
        tracing::warn!(updated, "recovered interrupted scan tasks");
    }
    Ok(updated)
}

pub fn image_extensions() -> &'static [&'static str] {
    IMAGE_EXTENSIONS
}

fn run_scan_blocking(task: ScanRootTask, cancellation: CancellationToken) -> AppResult<()> {
    let mut conn = task.pool.get()?;
    let root = roots_repo::get(&conn, task.root_id)?
        .ok_or_else(|| AppError::Other(format!("目录不存在：{}", task.root_id)))?;
    let root_path = PathBuf::from(&root.path);
    let now = now_unix();

    tracing::info!(
        root_id = task.root_id,
        task_id = task.task_id,
        path = ?root_path,
        "scan started"
    );
    set_scan_status(&conn, task.task_id, "running", Some(now), None, None, None)?;

    if !root_path.exists() {
        let error = AppError::Other(format!("目录不存在：{}", root_path.display()));
        set_scan_status(
            &conn,
            task.task_id,
            "failed",
            None,
            Some(now_unix()),
            None,
            Some(error.to_string()),
        )?;
        emit_scan_error(&task, error.to_string(), None);
        return Err(error);
    }

    tracing::info!(
        root_id = task.root_id,
        task_id = task.task_id,
        "scan walking directory"
    );

    let mut done = ScanDone {
        root_id: task.root_id,
        task_id: task.task_id,
        ..ScanDone::default()
    };
    let mut visited = HashSet::new();
    // 一次性预载该 root 的既有状态，扫描期间走内存比对，避免逐文件查库。
    let states = images_repo::load_scan_states(&conn, task.root_id)?;
    let started = Instant::now();
    let mut last_emit = Instant::now();
    let mut processed: usize = 0;

    emit_progress(&task, processed, 0, None, started);

    // 网络盘判定每次扫描固定，循环外算一次即可（原先每个 Added/Updated 都重算）。
    let is_network = crate::utils::path_normalize::is_network_path(&root_path);
    // 探测（磁盘 I/O）在事务外累积，攒够一批再统一写库，缩短 WAL 写锁持有时间。
    let mut batch: Vec<PreparedUpsert> = Vec::with_capacity(SCAN_BATCH_SIZE);
    let mut case_conflicts = CaseConflictDetector::default();

    for entry in WalkDir::new(&root_path).follow_links(false).into_iter() {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                tracing::warn!(root_id = task.root_id, task_id = task.task_id, %error, "walkdir entry failed");
                emit_scan_error(&task, error.to_string(), None);
                continue;
            }
        };

        if entry.file_type().is_dir() {
            if entry.depth() > 0 {
                observe_dir(&mut case_conflicts, &root_path, entry.path());
            }
            continue;
        }
        if !entry.file_type().is_file() {
            continue;
        }

        if cancellation.is_cancelled() {
            // 先把已探测但未写入的批次落库，避免丢失已完成的探测工作。
            flush_scan_batch(&mut conn, &mut batch, &task, &root, is_network, &mut done)?;
            set_scan_status(
                &conn,
                task.task_id,
                "paused",
                None,
                None,
                Some(i64::try_from(processed).unwrap_or(i64::MAX)),
                None,
            )?;
            emit_progress(
                &task,
                processed,
                i64::try_from(processed).unwrap_or(i64::MAX),
                None,
                started,
            );
            return Ok(());
        }

        let path = entry.path();
        if !is_supported_image(path) {
            continue;
        }

        let rel_path = match rel_path_string(&root_path, path) {
            Some(rel_path) => rel_path,
            None => continue,
        };
        visited.insert(rel_path.clone());
        processed += 1;

        // Windows 上 entry.metadata() 复用目录枚举时已取得的元数据，不再额外 stat。
        let metadata = match entry.metadata() {
            Ok(metadata) => metadata,
            Err(error) => {
                tracing::warn!(path = ?path, %error, "failed to read file metadata");
                emit_scan_error(
                    &task,
                    error.to_string(),
                    Some(path.to_string_lossy().to_string()),
                );
                continue;
            }
        };
        let prior = states.get(&rel_path).copied();

        // 探测（读图头 + 解 EXIF）在事务外进行；未变化文件走快速路径，不探测也不写库。
        // 变化/新增的文件累积成批，攒够 SCAN_BATCH_SIZE 后在一个事务里统一写入。
        if let Some(prepared) = prepare_scan_file(&root, path, &rel_path, &metadata, prior) {
            batch.push(prepared);
            if batch.len() >= SCAN_BATCH_SIZE {
                flush_scan_batch(&mut conn, &mut batch, &task, &root, is_network, &mut done)?;
            }
        }

        let processed_i64 = i64::try_from(processed).unwrap_or(i64::MAX);
        if processed_i64 % 50 == 0 {
            conn.execute(
                "UPDATE scan_tasks SET processed = ?1 WHERE id = ?2",
                params![processed_i64, task.task_id],
            )?;
        }

        if last_emit.elapsed().as_secs() >= 1 {
            emit_progress(&task, processed, processed_i64, Some(path), started);
            last_emit = Instant::now();
        }
    }

    // 收尾：写入最后不足一批的记录。
    flush_scan_batch(&mut conn, &mut batch, &task, &root, is_network, &mut done)?;

    done.removed = mark_missing_deleted(&mut conn, task.root_id, &states, &visited)?;
    done.case_conflicts = case_conflicts.into_conflicts();
    if !done.case_conflicts.is_empty() {
        tracing::warn!(
            root_id = task.root_id,
            conflicts = ?done.case_conflicts,
            "sibling directories differ only by case"
        );
    }
    let finished = now_unix();
    let total = i64::try_from(processed).unwrap_or(i64::MAX);
    tracing::info!(
        root_id = task.root_id,
        task_id = task.task_id,
        total,
        added = done.added,
        updated = done.updated,
        removed = done.removed,
        "scan completed"
    );
    roots_repo::touch_last_scan(&conn, task.root_id, finished)?;
    conn.execute(
        "UPDATE scan_tasks
         SET status = 'done', total_files = ?1, processed = ?1, finished_at = ?2, error = NULL
         WHERE id = ?3",
        params![total, finished, task.task_id],
    )?;
    emit_progress(&task, processed, total, None, started);
    let _ = task.app.emit("scan:done", done);
    Ok(())
}

/// 每批提交的文件数。事务只包内存→WAL 的写入（不含图片解码 / EXIF 探测），
/// 在 WAL + synchronous=NORMAL 下不会 per-commit fsync，因此并发的缩略图 worker
/// 只需短暂等待写锁，不会撞 busy_timeout（5s）。
const SCAN_BATCH_SIZE: usize = 500;

/// 扫描期累积的一条待写入记录：探测（磁盘 I/O）已在事务外完成。
struct PreparedUpsert {
    image: NewImageMetadata,
    path: PathBuf,
}

/// 探测单个文件并构造待写入记录。返回 `None` 表示走快速路径（size/mtime 未变），
/// 既不读图头 / 解 EXIF，也不写库。**不做任何 DB 访问**，可在事务外调用。
fn prepare_scan_file(
    root: &Root,
    path: &Path,
    rel_path: &str,
    metadata: &std::fs::Metadata,
    prior: Option<images_repo::ScannedFileState>,
) -> Option<PreparedUpsert> {
    let size_bytes = i64::try_from(metadata.len()).unwrap_or(i64::MAX);
    let mtime = unix_time(metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH));

    // 重复扫描快速跳过：预载状态里已知该文件且大小/mtime 未变 → 直接复用旧记录，
    // 不再读图头 / 解 EXIF（probe_image）、也不写库，缩略图同样不会重排。
    if let Some(state) = prior {
        if state.is_unchanged(size_bytes, mtime) {
            return None;
        }
    }

    let probe = probe_image(path);
    let filename = path
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| rel_path.to_string());
    let extension = path
        .extension()
        .map(|ext| {
            ext.to_string_lossy()
                .trim_start_matches('.')
                .to_ascii_lowercase()
        })
        .unwrap_or_default();

    let image = NewImageMetadata {
        root_id: root.id,
        rel_path: rel_path.to_string(),
        filename,
        extension,
        size_bytes,
        mtime,
        width: probe.width,
        height: probe.height,
        orientation: probe.orientation,
        taken_at: probe.taken_at,
        gps_lat: probe.gps_lat,
        gps_lng: probe.gps_lng,
        camera_make: probe.camera_make,
        camera_model: probe.camera_model,
    };

    Some(PreparedUpsert {
        image,
        path: path.to_path_buf(),
    })
}

/// 在单个事务里批量写入累积的记录，提交后再为新增/更新的图片入队缩略图任务。
/// 单条 upsert 失败只跳过该条（发 scan:error），不影响同批其它记录。
fn flush_scan_batch(
    conn: &mut rusqlite::Connection,
    batch: &mut Vec<PreparedUpsert>,
    task: &ScanRootTask,
    root: &Root,
    is_network: bool,
    done: &mut ScanDone,
) -> AppResult<()> {
    if batch.is_empty() {
        return Ok(());
    }
    let now = now_unix();
    // 事务提交后才入队缩略图，避免任务读到尚未提交的行。
    let mut to_thumb: Vec<(i64, PathBuf, String, Option<i64>)> = Vec::with_capacity(batch.len());

    let tx = conn.transaction()?;
    for prepared in batch.drain(..) {
        match images_repo::upsert_scanned_image(&tx, &prepared.image, now) {
            Ok(outcome) => {
                let (id, is_write) = match outcome {
                    UpsertOutcome::Added(id) => {
                        done.added += 1;
                        (id, true)
                    }
                    UpsertOutcome::Updated(id) => {
                        done.updated += 1;
                        (id, true)
                    }
                    UpsertOutcome::Unchanged(id) => (id, false),
                };
                if is_write {
                    to_thumb.push((
                        id,
                        prepared.path,
                        prepared.image.rel_path,
                        prepared.image.orientation,
                    ));
                }
            }
            Err(error) => {
                tracing::warn!(path = ?prepared.path, %error, "failed to upsert scanned image");
                emit_scan_error(
                    task,
                    error.to_string(),
                    Some(prepared.path.to_string_lossy().to_string()),
                );
            }
        }
    }
    tx.commit()?;

    for (image_id, path, rel_path, orientation) in to_thumb {
        task.scheduler.enqueue(
            ThumbnailTask::new(
                task.pool.clone(),
                image_id,
                root.id,
                rel_path,
                path,
                orientation,
                task.paths.thumbs_dir.clone(),
                is_network,
            )
            .with_event_app(task.app.clone()),
        )?;
    }
    Ok(())
}

fn is_supported_image(path: &Path) -> bool {
    path.extension()
        .map(|ext| {
            ext.to_string_lossy()
                .trim_start_matches('.')
                .to_ascii_lowercase()
        })
        .is_some_and(|ext| IMAGE_EXTENSIONS.contains(&ext.as_str()))
}

/// 把一个子目录登记进大小写冲突检测（R1-6）。
fn observe_dir(detector: &mut CaseConflictDetector, root_path: &Path, dir: &Path) {
    let Some(name) = dir.file_name() else {
        return;
    };
    // 直接在根目录下的子目录，父目录记为空串。
    let parent = dir
        .parent()
        .and_then(|parent| rel_path_string(root_path, parent))
        .unwrap_or_default();
    detector.observe(&parent, &name.to_string_lossy());
}

fn rel_path_string(root_path: &Path, path: &Path) -> Option<String> {
    let rel = path.strip_prefix(root_path).ok()?;
    let sep = std::path::MAIN_SEPARATOR.to_string();
    let parts = rel
        .components()
        .map(|component| component.as_os_str().to_string_lossy().to_string())
        .collect::<Vec<_>>();
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(&sep))
    }
}

fn mark_missing_deleted(
    conn: &mut rusqlite::Connection,
    root_id: i64,
    states: &HashMap<String, images_repo::ScannedFileState>,
    visited: &HashSet<String>,
) -> AppResult<i64> {
    let now = now_unix();
    let mut removed = 0;
    // states 仅含扫描开始时未删除的行；本次扫描没访问到的即为磁盘上已消失。
    // 一并放进单个事务批量标记删除，避免逐行 autocommit 写放大。
    let tx = conn.transaction()?;
    for rel_path in states.keys() {
        if !visited.contains(rel_path) {
            images_repo::mark_deleted(&tx, root_id, rel_path, now)?;
            removed += 1;
        }
    }
    tx.commit()?;
    Ok(removed)
}

fn set_scan_status(
    conn: &rusqlite::Connection,
    task_id: i64,
    status: &str,
    started_at: Option<i64>,
    finished_at: Option<i64>,
    processed: Option<i64>,
    error: Option<String>,
) -> AppResult<()> {
    if let Some(started_at) = started_at {
        conn.execute(
            "UPDATE scan_tasks SET status = ?1, started_at = ?2 WHERE id = ?3",
            params![status, started_at, task_id],
        )?;
    } else if let Some(finished_at) = finished_at {
        conn.execute(
            "UPDATE scan_tasks SET status = ?1, finished_at = ?2, error = ?3 WHERE id = ?4",
            params![status, finished_at, error, task_id],
        )?;
    } else if let Some(processed) = processed {
        conn.execute(
            "UPDATE scan_tasks SET status = ?1, processed = ?2 WHERE id = ?3",
            params![status, processed, task_id],
        )?;
    } else {
        conn.execute(
            "UPDATE scan_tasks SET status = ?1 WHERE id = ?2",
            params![status, task_id],
        )?;
    }
    Ok(())
}

fn emit_progress(
    task: &ScanRootTask,
    processed: usize,
    total: i64,
    current_file: Option<&Path>,
    started: Instant,
) {
    let processed_i64 = i64::try_from(processed).unwrap_or(i64::MAX);
    let eta_sec = if processed > 0 && total > processed_i64 {
        let elapsed = started.elapsed().as_secs_f64();
        let per_file = elapsed / processed as f64;
        Some(((total - processed_i64) as f64 * per_file).round() as i64)
    } else {
        None
    };
    let _ = task.app.emit(
        "scan:progress",
        ScanProgress {
            root_id: task.root_id,
            task_id: task.task_id,
            processed: processed_i64,
            total,
            current_file: current_file.map(|path| path.to_string_lossy().to_string()),
            eta_sec,
        },
    );
}

fn emit_scan_error(task: &ScanRootTask, error: String, file: Option<String>) {
    let _ = task.app.emit(
        "scan:error",
        ScanErrorEvent {
            root_id: task.root_id,
            task_id: task.task_id,
            error,
            file,
        },
    );
}

fn probe_image(path: &Path) -> ImageProbe {
    // 读图头 / EXIF 进后台模式（docs/11 R1-3）；写库的事务不在这里，仍是普通优先级。
    let _bg = crate::utils::priority::BackgroundPriorityGuard::enter();
    let mut probe = ImageProbe::default();
    // 只打开一次文件：尺寸与 EXIF 复用同一句柄（网络盘上省一次往返，见 docs/09 P2-5）。
    let file = match File::open(path) {
        Ok(file) => file,
        Err(_) => return probe,
    };
    let mut reader = BufReader::new(file);

    // 尺寸：从图头解析（不全解码）。等价于旧的 image::image_dimensions
    // （其内部同样是 with_guessed_format + into_dimensions），只是不再单独开文件。
    if let Ok(guessed) = image::ImageReader::new(&mut reader).with_guessed_format() {
        if let Ok((width, height)) = guessed.into_dimensions() {
            probe.width = Some(i64::from(width));
            probe.height = Some(i64::from(height));
        }
    }

    // EXIF 需从文件头重新读，先 seek 回起点。
    if reader.seek(SeekFrom::Start(0)).is_err() {
        return probe;
    }
    let exif = match Reader::new().read_from_container(&mut reader) {
        Ok(exif) => exif,
        Err(_) => return probe,
    };

    probe.orientation = field_u32(&exif, Tag::Orientation).map(i64::from);
    probe.taken_at =
        field_string(&exif, Tag::DateTimeOriginal).and_then(|s| parse_exif_datetime(&s));
    probe.camera_make = field_string(&exif, Tag::Make);
    probe.camera_model = field_string(&exif, Tag::Model);
    probe.gps_lat = gps_coord(&exif, Tag::GPSLatitude, Tag::GPSLatitudeRef);
    probe.gps_lng = gps_coord(&exif, Tag::GPSLongitude, Tag::GPSLongitudeRef);
    probe
}

fn field_u32(exif: &exif::Exif, tag: Tag) -> Option<u32> {
    let field = exif.fields().find(|field| field.tag == tag)?;
    match &field.value {
        Value::Byte(values) => values.first().copied().map(u32::from),
        Value::Short(values) => values.first().copied().map(u32::from),
        Value::Long(values) => values.first().copied(),
        _ => None,
    }
}

fn field_string(exif: &exif::Exif, tag: Tag) -> Option<String> {
    let field = exif.fields().find(|field| field.tag == tag)?;
    match &field.value {
        Value::Ascii(values) => values.first().and_then(|bytes| {
            String::from_utf8(bytes.clone())
                .ok()
                .map(|s| s.trim_matches(char::from(0)).trim().to_string())
                .filter(|s| !s.is_empty())
        }),
        _ => Some(field.display_value().with_unit(exif).to_string())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty()),
    }
}

fn parse_exif_datetime(raw: &str) -> Option<i64> {
    let trimmed = raw.trim();
    let naive = NaiveDateTime::parse_from_str(trimmed, "%Y:%m:%d %H:%M:%S").ok()?;
    Local
        .from_local_datetime(&naive)
        .single()
        .map(|dt| dt.timestamp())
}

fn gps_coord(exif: &exif::Exif, value_tag: Tag, ref_tag: Tag) -> Option<f64> {
    let field = exif.fields().find(|field| field.tag == value_tag)?;
    let values = match &field.value {
        Value::Rational(values) => values,
        _ => return None,
    };
    if values.len() < 3 {
        return None;
    }
    let degrees = rational_to_f64(values[0])?;
    let minutes = rational_to_f64(values[1])?;
    let seconds = rational_to_f64(values[2])?;
    let mut coord = degrees + minutes / 60.0 + seconds / 3600.0;
    if let Some(reference) = field_string(exif, ref_tag) {
        if reference.eq_ignore_ascii_case("S") || reference.eq_ignore_ascii_case("W") {
            coord = -coord;
        }
    }
    Some(coord)
}

fn rational_to_f64(value: exif::Rational) -> Option<f64> {
    if value.denom == 0 {
        None
    } else {
        Some(value.num as f64 / value.denom as f64)
    }
}

fn unix_time(time: SystemTime) -> i64 {
    time.duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

fn now_unix() -> i64 {
    unix_time(SystemTime::now())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supported_image_extensions_are_case_insensitive() {
        assert!(is_supported_image(Path::new("a.JPG")));
        assert!(is_supported_image(Path::new("a.heic")));
        assert!(!is_supported_image(Path::new("a.txt")));
    }

    #[test]
    fn case_conflicts_are_detected_per_parent() {
        let mut detector = CaseConflictDetector::default();
        detector.observe(r"PD2324\相册备份", "camera");
        detector.observe(r"PD2324\相册备份", "微信");
        detector.observe(r"PD2324\相册备份", "Camera");
        detector.observe(r"PD2324\相册备份", "CAMERA");
        // 再遇到已登记的名字不重复记。
        detector.observe(r"PD2324\相册备份", "Camera");
        // 不同父目录下的同名目录不算冲突。
        detector.observe(r"PD2324\相册备份\camera", "2024");
        detector.observe(r"PD2324\相册备份\Camera", "2024");
        detector.observe("", "Screenshots");
        detector.observe("", "screenshots");

        assert_eq!(
            detector.into_conflicts(),
            vec![
                CaseConflict {
                    parent: r"PD2324\相册备份".to_string(),
                    names: vec![
                        "camera".to_string(),
                        "Camera".to_string(),
                        "CAMERA".to_string()
                    ],
                },
                CaseConflict {
                    parent: String::new(),
                    names: vec!["Screenshots".to_string(), "screenshots".to_string()],
                },
            ]
        );
    }

    #[test]
    fn same_name_seen_twice_is_not_a_conflict() {
        let mut detector = CaseConflictDetector::default();
        detector.observe("a", "Camera");
        detector.observe("a", "Camera");
        assert!(detector.into_conflicts().is_empty());
    }

    #[test]
    fn observe_dir_uses_parent_relative_to_root() {
        let root = Path::new(r"Y:\备份");
        let mut detector = CaseConflictDetector::default();
        observe_dir(&mut detector, root, &root.join("camera"));
        observe_dir(&mut detector, root, &root.join("Camera"));
        observe_dir(&mut detector, root, &root.join("PD2324").join("x"));
        observe_dir(&mut detector, root, &root.join("PD2324").join("X"));
        let conflicts = detector.into_conflicts();
        assert_eq!(conflicts.len(), 2);
        assert_eq!(conflicts[0].parent, "");
        assert_eq!(conflicts[1].parent, "PD2324");
    }

    #[test]
    fn parse_exif_datetime_returns_timestamp() {
        assert!(parse_exif_datetime("2026:06:16 12:34:56").is_some());
        assert!(parse_exif_datetime("not a date").is_none());
    }

    // P2-5：单次打开后仍能正确解析尺寸（守护把 image_dimensions 换成复用句柄的 ImageReader 的行为）。
    #[test]
    fn probe_image_reads_dimensions_single_open() {
        let dir = tempfile::tempdir().expect("tempdir");
        let jpg = dir.path().join("probe.jpg");
        image::RgbImage::from_fn(48, 32, |x, _| image::Rgb([(x % 256) as u8, 10, 20]))
            .save(&jpg)
            .expect("write jpg");
        let probe = probe_image(&jpg);
        assert_eq!(probe.width, Some(48));
        assert_eq!(probe.height, Some(32));

        let png = dir.path().join("probe.png");
        image::RgbImage::from_fn(20, 10, |_, y| image::Rgb([5, (y % 256) as u8, 30]))
            .save(&png)
            .expect("write png");
        let probe = probe_image(&png);
        assert_eq!(probe.width, Some(20));
        assert_eq!(probe.height, Some(10));
    }
}
