use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use tauri::Emitter;

use crate::config::AppPaths;
use crate::db::Pool;
use crate::error::{AppError, AppResult};
use crate::queue::{Priority, Scheduler, Task, TaskContext};
use crate::repo::images_repo::{self, ImageRecord};
use crate::repo::lancedb_repo::{EmbeddingRecord, LanceDbRepo};
use crate::repo::tags_repo::{self, NewTagScore};
use crate::services::ai_client::{ClipEmbedItem, TaggerItem};
use crate::services::ai_supervisor::AiSupervisor;
use crate::services::settings_service;
use crate::services::startup_gate::StartupGate;
use crate::services::thumbnail_service;

const EMBED_BATCH: i64 = 32;
const TAG_BATCH: i64 = 16;
/// 前端迟迟不发「首屏就绪」信号时，后台 AI 最多等多久才自行放行。
///
/// 正常路径必须等 `frontend_ready`：worker 首启会 import torch/CUDA、加载模型，是开机
/// 最重的磁盘/CPU 尖峰；固定 sleep 在首屏被拖慢时会提前到期，反而继续拉长白屏。
/// 手动“启动 / 处理待办”不受此延迟影响。
const STARTUP_AI_FALLBACK: Duration = Duration::from_secs(180);

#[derive(Clone)]
pub struct AiPipeline {
    pool: Pool,
    scheduler: Scheduler,
    supervisor: Arc<AiSupervisor>,
    vectors: LanceDbRepo,
    paths: AppPaths,
    thumbs_dir: PathBuf,
    state: Arc<Mutex<AiPipelineState>>,
}

#[derive(Default)]
struct AiPipelineState {
    embed_inflight: HashSet<i64>,
    tag_inflight: HashSet<i64>,
}

impl AiPipeline {
    pub fn new(
        pool: Pool,
        scheduler: Scheduler,
        supervisor: Arc<AiSupervisor>,
        paths: &AppPaths,
    ) -> Self {
        Self {
            pool,
            scheduler,
            supervisor,
            vectors: LanceDbRepo::new(paths.vectors_dir.clone()),
            paths: paths.clone(),
            thumbs_dir: paths.thumbs_dir.clone(),
            state: Arc::new(Mutex::new(AiPipelineState::default())),
        }
    }

    pub fn vectors(&self) -> LanceDbRepo {
        self.vectors.clone()
    }

    pub fn spawn_loop(
        self: Arc<Self>,
        app: tauri::AppHandle<tauri::Wry>,
        startup_gate: Arc<StartupGate>,
    ) {
        tauri::async_runtime::spawn(async move {
            // 启动后先让前端完成首屏，再开始后台 AI：启动 Python/CUDA worker 很重，
            // 不应和窗口首次渲染抢 CPU/磁盘/显存。
            startup_gate.wait(STARTUP_AI_FALLBACK).await;
            let mut interval = tokio::time::interval(Duration::from_secs(2));
            loop {
                interval.tick().await;
                if let Err(error) = self.enqueue_pending().await {
                    tracing::warn!(%error, "AI pipeline enqueue tick failed");
                }
                let _ = app.emit("ai:progress", self.status());
            }
        });
    }

    pub fn status(&self) -> AiPipelineStatus {
        let conn = match self.pool.get() {
            Ok(conn) => conn,
            Err(_) => {
                return AiPipelineStatus {
                    embedding_pending: 0,
                    tagging_pending: 0,
                    embedding_inflight: 0,
                    tagging_inflight: 0,
                }
            }
        };
        let embedding_pending = count_pending(&conn, "embedding_status").unwrap_or(0);
        let tagging_pending = count_pending(&conn, "tag_status").unwrap_or(0);
        let state = self.state.lock().ok();
        AiPipelineStatus {
            embedding_pending,
            tagging_pending,
            embedding_inflight: state
                .as_ref()
                .map(|state| state.embed_inflight.len())
                .unwrap_or(0),
            tagging_inflight: state
                .as_ref()
                .map(|state| state.tag_inflight.len())
                .unwrap_or(0),
        }
    }

    pub async fn enqueue_pending(&self) -> AppResult<()> {
        if !settings_service::read(&self.paths)?.ai_enabled {
            return Ok(());
        }
        // 先看有没有待处理的图：没有就别去唤醒（启动）Python worker。
        let (embed_pending, tag_pending) = {
            let conn = self.pool.get()?;
            (
                count_pending(&conn, "embedding_status")?,
                count_pending(&conn, "tag_status")?,
            )
        };
        if embed_pending == 0 && tag_pending == 0 {
            return Ok(());
        }
        // worker 未就绪（或正处于启动失败退避窗口内）就跳过本轮：
        // 否则会反复向队列投递必然失败的任务，刷满 "queue task failed"。
        if !self.supervisor.ensure_ready().await {
            return Ok(());
        }
        if embed_pending > 0 {
            self.enqueue_embedding_batch()?;
        }
        if tag_pending > 0 {
            self.enqueue_tag_batch()?;
        }
        Ok(())
    }

    fn enqueue_embedding_batch(&self) -> AppResult<()> {
        let conn = self.pool.get()?;
        let mut candidates = images_repo::pending_embedding_images(&conn, EMBED_BATCH * 2)?;
        drop(conn);
        let ids = take_not_inflight(
            &mut candidates,
            &self.state,
            |state| &mut state.embed_inflight,
            EMBED_BATCH as usize,
        );
        if ids.is_empty() {
            return Ok(());
        }
        self.scheduler.enqueue(EmbedBatchTask {
            pool: self.pool.clone(),
            supervisor: self.supervisor.clone(),
            vectors: self.vectors.clone(),
            thumbs_dir: self.thumbs_dir.clone(),
            state: self.state.clone(),
            image_ids: ids,
        })
    }

    fn enqueue_tag_batch(&self) -> AppResult<()> {
        let conn = self.pool.get()?;
        let mut candidates = images_repo::pending_tag_images(&conn, TAG_BATCH * 2)?;
        drop(conn);
        let ids = take_not_inflight(
            &mut candidates,
            &self.state,
            |state| &mut state.tag_inflight,
            TAG_BATCH as usize,
        );
        if ids.is_empty() {
            return Ok(());
        }
        self.scheduler.enqueue(TagBatchTask {
            pool: self.pool.clone(),
            supervisor: self.supervisor.clone(),
            thumbs_dir: self.thumbs_dir.clone(),
            state: self.state.clone(),
            image_ids: ids,
        })
    }
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AiPipelineStatus {
    pub embedding_pending: i64,
    pub tagging_pending: i64,
    pub embedding_inflight: usize,
    pub tagging_inflight: usize,
}

struct EmbedBatchTask {
    pool: Pool,
    supervisor: Arc<AiSupervisor>,
    vectors: LanceDbRepo,
    thumbs_dir: PathBuf,
    state: Arc<Mutex<AiPipelineState>>,
    image_ids: Vec<i64>,
}

struct TagBatchTask {
    pool: Pool,
    supervisor: Arc<AiSupervisor>,
    thumbs_dir: PathBuf,
    state: Arc<Mutex<AiPipelineState>>,
    image_ids: Vec<i64>,
}

#[async_trait]
impl Task for EmbedBatchTask {
    fn priority(&self) -> Priority {
        Priority::P6
    }

    fn label(&self) -> String {
        format!("ai-embed:{}", self.image_ids.len())
    }

    async fn run(&self, ctx: TaskContext) -> AppResult<()> {
        let result = self.run_inner(ctx).await;
        clear_inflight(&self.state, &self.image_ids, |state| {
            &mut state.embed_inflight
        });
        result
    }
}

impl EmbedBatchTask {
    async fn run_inner(&self, ctx: TaskContext) -> AppResult<()> {
        if ctx.is_cancelled() {
            return Ok(());
        }
        let items = self.load_items()?;
        if items.is_empty() {
            return Ok(());
        }
        let request_items = items
            .iter()
            .map(|item| ClipEmbedItem {
                id: item.id,
                thumb_path: item.thumb_path.clone(),
            })
            .collect();
        let response = self.supervisor.embed_images(request_items).await?;
        let now = now_unix();
        let mut embeddings = Vec::new();
        let conn = self.pool.get()?;
        for item in response.items {
            if let Some(vector) = item.embedding {
                embeddings.push(EmbeddingRecord {
                    image_id: item.id,
                    model: response.model.clone(),
                    embedding: vector,
                    created_at: now,
                });
            } else {
                images_repo::set_embedding_status(&conn, item.id, "failed")?;
            }
        }
        drop(conn);
        if !embeddings.is_empty() {
            self.vectors.upsert_embeddings(&embeddings).await?;
            let conn = self.pool.get()?;
            for record in &embeddings {
                images_repo::set_embedding_status(&conn, record.image_id, "ready")?;
            }
        }
        Ok(())
    }

    fn load_items(&self) -> AppResult<Vec<AiImageItem>> {
        let conn = self.pool.get()?;
        let records = images_repo::get_details_by_ids(&conn, &self.image_ids)?;
        records_to_ai_items(&records, &self.thumbs_dir)
    }
}

#[async_trait]
impl Task for TagBatchTask {
    fn priority(&self) -> Priority {
        Priority::P5
    }

    fn label(&self) -> String {
        format!("ai-tag:{}", self.image_ids.len())
    }

    async fn run(&self, ctx: TaskContext) -> AppResult<()> {
        let result = self.run_inner(ctx).await;
        clear_inflight(&self.state, &self.image_ids, |state| {
            &mut state.tag_inflight
        });
        result
    }
}

impl TagBatchTask {
    async fn run_inner(&self, ctx: TaskContext) -> AppResult<()> {
        if ctx.is_cancelled() {
            return Ok(());
        }
        let items = self.load_items()?;
        if items.is_empty() {
            return Ok(());
        }
        let request_items = items
            .iter()
            .map(|item| TaggerItem {
                id: item.id,
                thumb_path: item.thumb_path.clone(),
            })
            .collect();
        let response = self.supervisor.tag_images(request_items).await?;
        let now = now_unix();
        let mut conn = self.pool.get()?;
        for item in response.items {
            if item.error.is_some() {
                images_repo::set_tag_status(&conn, item.id, "failed")?;
                continue;
            }
            let tags = item
                .tags
                .into_iter()
                .map(|tag| NewTagScore {
                    name: tag.name,
                    score: tag.score,
                    source: "ai".to_string(),
                    category: tag.category,
                })
                .collect::<Vec<_>>();
            tags_repo::replace_image_tags(&mut conn, item.id, &tags, now)?;
            images_repo::set_tag_status(&conn, item.id, "ready")?;
        }
        Ok(())
    }

    fn load_items(&self) -> AppResult<Vec<AiImageItem>> {
        let conn = self.pool.get()?;
        let records = images_repo::get_details_by_ids(&conn, &self.image_ids)?;
        records_to_ai_items(&records, &self.thumbs_dir)
    }
}

#[derive(Debug, Clone)]
struct AiImageItem {
    id: i64,
    thumb_path: String,
}

fn records_to_ai_items(records: &[ImageRecord], thumbs_dir: &Path) -> AppResult<Vec<AiImageItem>> {
    records
        .iter()
        .map(|record| {
            let hash = record.thumb_hash.as_deref().ok_or_else(|| {
                AppError::Other(format!("image {} thumbnail is not ready", record.id))
            })?;
            Ok(AiImageItem {
                id: record.id,
                thumb_path: thumbnail_service::thumb_path(thumbs_dir, hash)
                    .to_string_lossy()
                    .to_string(),
            })
        })
        .collect()
}

fn take_not_inflight(
    candidates: &mut [ImageRecord],
    state: &Arc<Mutex<AiPipelineState>>,
    inflight: impl Fn(&mut AiPipelineState) -> &mut HashSet<i64>,
    limit: usize,
) -> Vec<i64> {
    let mut selected = Vec::new();
    let Ok(mut state) = state.lock() else {
        return selected;
    };
    let set = inflight(&mut state);
    for record in candidates.iter() {
        if selected.len() >= limit {
            break;
        }
        if set.insert(record.id) {
            selected.push(record.id);
        }
    }
    selected
}

fn clear_inflight(
    state: &Arc<Mutex<AiPipelineState>>,
    ids: &[i64],
    inflight: impl Fn(&mut AiPipelineState) -> &mut HashSet<i64>,
) {
    if let Ok(mut state) = state.lock() {
        let set = inflight(&mut state);
        for id in ids {
            set.remove(id);
        }
    }
}

fn count_pending(conn: &rusqlite::Connection, column: &str) -> AppResult<i64> {
    if !matches!(column, "embedding_status" | "tag_status") {
        return Err(AppError::Other("invalid AI status column".to_string()));
    }
    let sql = format!(
        "SELECT COUNT(*) FROM images
         WHERE deleted_at IS NULL AND thumb_status = 'ready' AND {column} = 'pending'"
    );
    Ok(conn.query_row(&sql, [], |row| row.get(0))?)
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
