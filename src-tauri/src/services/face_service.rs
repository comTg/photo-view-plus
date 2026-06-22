use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;

use crate::db::Pool;
use crate::error::{AppError, AppResult};
use crate::queue::{Priority, Scheduler, Task, TaskContext};
use crate::repo::face_repo::{self, FaceBox, NewFace};
use crate::repo::images_repo::{self, ImageRecord};
use crate::repo::lancedb_repo::{FaceEmbeddingRecord, LanceDbRepo, CLIP_DIMS};
use crate::services::ai_client::FaceDetectItem;
use crate::services::ai_supervisor::AiSupervisor;
use crate::services::thumbnail_service;

const FACE_BATCH: usize = 8;

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FaceStatus {
    pub pending: i64,
    pub ready: i64,
    pub failed: i64,
    pub disabled: i64,
    pub clusters: i64,
    pub faces: i64,
}

pub fn enqueue_face_detection(
    pool: &Pool,
    scheduler: &Scheduler,
    supervisor: Arc<AiSupervisor>,
    vectors: LanceDbRepo,
    thumbs_dir: PathBuf,
    image_ids: Option<Vec<i64>>,
) -> AppResult<usize> {
    let conn = pool.get()?;
    images_repo::mark_face_pending(&conn, image_ids.as_deref())?;
    let pending = images_repo::pending_face_images(&conn, 64)?;
    drop(conn);

    let mut enqueued = 0usize;
    for chunk in pending.chunks(FACE_BATCH) {
        let image_ids = chunk.iter().map(|image| image.id).collect::<Vec<_>>();
        scheduler.enqueue(FaceBatchTask {
            pool: pool.clone(),
            supervisor: supervisor.clone(),
            vectors: vectors.clone(),
            thumbs_dir: thumbs_dir.clone(),
            image_ids,
        })?;
        enqueued += chunk.len();
    }
    Ok(enqueued)
}

pub async fn rebuild_clusters(pool: &Pool, vectors: &LanceDbRepo) -> AppResult<usize> {
    let embeddings = vectors.list_face_embeddings().await?;
    let mut conn = pool.get()?;
    face_repo::rebuild_clusters(&mut conn, &embeddings)
}

pub fn status(pool: &Pool) -> AppResult<FaceStatus> {
    let conn = pool.get()?;
    Ok(FaceStatus {
        pending: count_image_status(&conn, "pending")?,
        ready: count_image_status(&conn, "ready")?,
        failed: count_image_status(&conn, "failed")?,
        disabled: count_image_status(&conn, "disabled")?,
        clusters: conn.query_row("SELECT COUNT(*) FROM face_clusters", [], |row| row.get(0))?,
        faces: conn.query_row("SELECT COUNT(*) FROM faces", [], |row| row.get(0))?,
    })
}

struct FaceBatchTask {
    pool: Pool,
    supervisor: Arc<AiSupervisor>,
    vectors: LanceDbRepo,
    thumbs_dir: PathBuf,
    image_ids: Vec<i64>,
}

#[async_trait]
impl Task for FaceBatchTask {
    fn priority(&self) -> Priority {
        Priority::P7
    }

    fn label(&self) -> String {
        format!("face-detect:{}", self.image_ids.len())
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
            .map(|item| FaceDetectItem {
                id: item.id,
                thumb_path: item.thumb_path.clone(),
            })
            .collect();
        let response = self.supervisor.detect_faces(request).await?;
        let mut vectors = Vec::new();
        let now = now_unix();
        {
            let mut conn = self.pool.get()?;
            for item in response.items {
                if item.error.is_some() {
                    images_repo::set_face_status(&conn, item.id, "failed")?;
                    continue;
                }
                let new_faces = item
                    .faces
                    .iter()
                    .map(|face| NewFace {
                        bbox: FaceBox {
                            x: face.bbox.x,
                            y: face.bbox.y,
                            w: face.bbox.w,
                            h: face.bbox.h,
                        },
                        confidence: face.confidence,
                        embedding_ref: None,
                    })
                    .collect::<Vec<_>>();
                let written = face_repo::replace_faces_for_image(&mut conn, item.id, &new_faces)?;
                for (written_face, detected_face) in written.iter().zip(item.faces.iter()) {
                    let Some(embedding) = detected_face.embedding.clone() else {
                        continue;
                    };
                    if embedding.len() != CLIP_DIMS as usize {
                        continue;
                    }
                    let embedding_ref = format!("face:{}", written_face.id);
                    face_repo::set_embedding_ref(&conn, written_face.id, &embedding_ref)?;
                    vectors.push(FaceEmbeddingRecord {
                        face_id: written_face.id,
                        embedding,
                        created_at: now,
                    });
                }
                images_repo::set_face_status(&conn, item.id, "ready")?;
            }
        }
        if !vectors.is_empty() {
            self.vectors.upsert_face_embeddings(&vectors).await?;
            rebuild_clusters(&self.pool, &self.vectors).await?;
        }
        Ok(())
    }
}

impl FaceBatchTask {
    fn load_items(&self) -> AppResult<Vec<FaceImageItem>> {
        let conn = self.pool.get()?;
        let records = images_repo::get_details_by_ids(&conn, &self.image_ids)?;
        records_to_face_items(&records, &self.thumbs_dir)
    }
}

#[derive(Debug, Clone)]
struct FaceImageItem {
    id: i64,
    thumb_path: String,
}

fn records_to_face_items(
    records: &[ImageRecord],
    thumbs_dir: &Path,
) -> AppResult<Vec<FaceImageItem>> {
    records
        .iter()
        .map(|record| {
            let hash = record.thumb_hash.as_deref().ok_or_else(|| {
                AppError::Other(format!("image {} thumbnail is not ready", record.id))
            })?;
            Ok(FaceImageItem {
                id: record.id,
                thumb_path: thumbnail_service::thumb_path(thumbs_dir, hash)
                    .to_string_lossy()
                    .to_string(),
            })
        })
        .collect()
}

fn count_image_status(conn: &rusqlite::Connection, status: &str) -> AppResult<i64> {
    Ok(conn.query_row(
        "SELECT COUNT(*) FROM images WHERE deleted_at IS NULL AND face_status = ?1",
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
