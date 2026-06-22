use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use rusqlite::params;

use crate::db::Pool;
use crate::error::{AppError, AppResult};
use crate::queue::{Priority, Scheduler, Task, TaskContext};
use crate::repo::images_repo::{self, ImageRecord};
use crate::repo::tags_repo;
use crate::services::ai_client::OcrItem;
use crate::services::ai_supervisor::AiSupervisor;
use crate::services::thumbnail_service;

const OCR_BATCH: usize = 8;

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OcrStatus {
    pub pending: i64,
    pub ready: i64,
    pub failed: i64,
    pub disabled: i64,
}

pub fn enqueue_ocr(
    pool: &Pool,
    scheduler: &Scheduler,
    supervisor: Arc<AiSupervisor>,
    thumbs_dir: PathBuf,
    image_ids: Option<Vec<i64>>,
) -> AppResult<usize> {
    let conn = pool.get()?;
    images_repo::mark_ocr_pending(&conn, image_ids.as_deref())?;
    let pending = images_repo::pending_ocr_images(&conn, 64)?;
    drop(conn);

    let mut enqueued = 0usize;
    for chunk in pending.chunks(OCR_BATCH) {
        let image_ids = chunk.iter().map(|image| image.id).collect::<Vec<_>>();
        scheduler.enqueue(OcrBatchTask {
            pool: pool.clone(),
            supervisor: supervisor.clone(),
            thumbs_dir: thumbs_dir.clone(),
            image_ids,
        })?;
        enqueued += chunk.len();
    }
    Ok(enqueued)
}

pub fn status(pool: &Pool) -> AppResult<OcrStatus> {
    let conn = pool.get()?;
    Ok(OcrStatus {
        pending: count_status(&conn, "ocr_status", "pending")?,
        ready: count_status(&conn, "ocr_status", "ready")?,
        failed: count_status(&conn, "ocr_status", "failed")?,
        disabled: count_status(&conn, "ocr_status", "disabled")?,
    })
}

struct OcrBatchTask {
    pool: Pool,
    supervisor: Arc<AiSupervisor>,
    thumbs_dir: PathBuf,
    image_ids: Vec<i64>,
}

#[async_trait]
impl Task for OcrBatchTask {
    fn priority(&self) -> Priority {
        Priority::P4
    }

    fn label(&self) -> String {
        format!("ocr:{}", self.image_ids.len())
    }

    async fn run(&self, ctx: TaskContext) -> AppResult<()> {
        if ctx.is_cancelled() {
            return Ok(());
        }
        let items = self.load_items()?;
        if items.is_empty() {
            return Ok(());
        }
        let request = items
            .iter()
            .map(|item| OcrItem {
                id: item.id,
                thumb_path: item.thumb_path.clone(),
            })
            .collect();
        let response = self.supervisor.run_ocr(request).await?;
        let now = now_unix();
        let conn = self.pool.get()?;
        for item in response.items {
            if item.error.is_some() {
                images_repo::set_ocr_status(&conn, item.id, "failed")?;
                continue;
            }
            images_repo::set_ocr_result(&conn, item.id, &item.text, "ready")?;
            if !item.text.trim().is_empty() {
                attach_ocr_tag(&conn, item.id, now)?;
            }
        }
        Ok(())
    }
}

impl OcrBatchTask {
    fn load_items(&self) -> AppResult<Vec<OcrImageItem>> {
        let conn = self.pool.get()?;
        let records = images_repo::get_details_by_ids(&conn, &self.image_ids)?;
        records_to_ocr_items(&records, &self.thumbs_dir)
    }
}

#[derive(Debug, Clone)]
struct OcrImageItem {
    id: i64,
    thumb_path: String,
}

fn records_to_ocr_items(
    records: &[ImageRecord],
    thumbs_dir: &Path,
) -> AppResult<Vec<OcrImageItem>> {
    records
        .iter()
        .map(|record| {
            let hash = record.thumb_hash.as_deref().ok_or_else(|| {
                AppError::Other(format!("image {} thumbnail is not ready", record.id))
            })?;
            Ok(OcrImageItem {
                id: record.id,
                thumb_path: thumbnail_service::thumb_path(thumbs_dir, hash)
                    .to_string_lossy()
                    .to_string(),
            })
        })
        .collect()
}

fn attach_ocr_tag(conn: &rusqlite::Connection, image_id: i64, now: i64) -> AppResult<()> {
    let tag_id = tags_repo::upsert_tag(conn, "截图", "ocr", Some("scene"), now)?;
    conn.execute(
        "INSERT OR REPLACE INTO image_tags(image_id, tag_id, score, source, created_at)
         VALUES (?1, ?2, 1.0, 'ocr', ?3)",
        params![image_id, tag_id, now],
    )?;
    Ok(())
}

fn count_status(conn: &rusqlite::Connection, column: &str, status: &str) -> AppResult<i64> {
    if column != "ocr_status" {
        return Err(AppError::Other("invalid OCR status column".to_string()));
    }
    Ok(conn.query_row(
        "SELECT COUNT(*) FROM images WHERE deleted_at IS NULL AND ocr_status = ?1",
        [status],
        |row| row.get(0),
    )?)
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}
