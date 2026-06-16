use std::collections::HashSet;
use std::fs::File;
use std::io::BufReader;
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
    let conn = task.pool.get()?;
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
    let started = Instant::now();
    let mut last_emit = Instant::now();
    let mut processed: usize = 0;

    emit_progress(&task, processed, 0, None, started);

    for entry in WalkDir::new(&root_path).follow_links(false).into_iter() {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                tracing::warn!(root_id = task.root_id, task_id = task.task_id, %error, "walkdir entry failed");
                emit_scan_error(&task, error.to_string(), None);
                continue;
            }
        };

        if !entry.file_type().is_file() {
            continue;
        }

        if cancellation.is_cancelled() {
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

        match scan_one_file(&conn, &task, &root, path, &rel_path) {
            Ok(outcome) => match outcome {
                UpsertOutcome::Added(_) => done.added += 1,
                UpsertOutcome::Updated(_) => done.updated += 1,
                UpsertOutcome::Unchanged(_) => {}
            },
            Err(error) => {
                tracing::warn!(path = ?path, %error, "failed to scan one image");
                emit_scan_error(
                    &task,
                    error.to_string(),
                    Some(path.to_string_lossy().to_string()),
                );
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

    done.removed = mark_missing_deleted(&conn, task.root_id, &visited)?;
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

fn scan_one_file(
    conn: &rusqlite::Connection,
    task: &ScanRootTask,
    root: &Root,
    path: &Path,
    rel_path: &str,
) -> AppResult<UpsertOutcome> {
    let metadata = std::fs::metadata(path)?;
    let size_bytes = i64::try_from(metadata.len()).unwrap_or(i64::MAX);
    let mtime = unix_time(metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH));
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

    let outcome = images_repo::upsert_scanned_image(conn, &image, now_unix())?;
    if matches!(outcome, UpsertOutcome::Added(_) | UpsertOutcome::Updated(_)) {
        task.scheduler.enqueue(ThumbnailTask::new(
            task.pool.clone(),
            outcome.image_id(),
            root.id,
            rel_path.to_string(),
            path.to_path_buf(),
            image.orientation,
            task.paths.thumbs_dir.clone(),
        ))?;
    }
    Ok(outcome)
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
    conn: &rusqlite::Connection,
    root_id: i64,
    visited: &HashSet<String>,
) -> AppResult<i64> {
    let active = images_repo::active_rel_paths(conn, root_id)?;
    let now = now_unix();
    let mut removed = 0;
    for rel_path in active {
        if !visited.contains(&rel_path) {
            images_repo::mark_deleted(conn, root_id, &rel_path, now)?;
            removed += 1;
        }
    }
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
    let mut probe = ImageProbe::default();
    if let Ok((width, height)) = image::image_dimensions(path) {
        probe.width = Some(i64::from(width));
        probe.height = Some(i64::from(height));
    }

    let file = match File::open(path) {
        Ok(file) => file,
        Err(_) => return probe,
    };
    let mut reader = BufReader::new(file);
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
    fn parse_exif_datetime_returns_timestamp() {
        assert!(parse_exif_datetime("2026:06:16 12:34:56").is_some());
        assert!(parse_exif_datetime("not a date").is_none());
    }
}
