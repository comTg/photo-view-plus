use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use tauri::Emitter;

use crate::config::AppPaths;
use crate::db::Pool;
use crate::error::{AppError, AppResult};
use crate::queue::{Priority, Scheduler, Task, TaskContext};
use crate::repo::ai_jobs_repo::{self, JobResult, KIND_IMAGE_EMBEDDING, KIND_TEXT_EMBEDDING};
use crate::repo::images_repo::{self, ImageRecord};
use crate::repo::lancedb_repo::{EmbeddingRecord, LanceDbRepo, VectorTable};
use crate::repo::tags_repo::{self, NewTagScore};
use crate::services::ai_client::{
    ClipEmbedItem, ClipEmbedResponse, TaggerItem, TaggerResponse, TextEmbedResponse,
};
use crate::services::ai_supervisor::AiSupervisor;
use crate::services::settings_service;
use crate::services::startup_gate::StartupGate;
use crate::services::thumbnail_service;

const EMBED_BATCH: i64 = 32;
const TAG_BATCH: i64 = 16;
/// 描述文本向量一批的条数：bge-small 在 GPU 上 128 条约 20 毫秒，批大一点省 HTTP 往返。
const TEXT_EMBED_BATCH: i64 = 128;
/// worker 返回兜底结果（缺 torch / open_clip / RAM 权重时的哈希伪向量、占位标签）后，
/// 多久内不再投递同类批次。兜底结果一律不入库，投了也是白算；worker 换进程时提前解除，
/// 见 [`FallbackBackoff`]。
pub const FALLBACK_BACKOFF: Duration = Duration::from_secs(10 * 60);
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
    text_inflight: HashSet<i64>,
    embed_fallback: FallbackBackoff,
    tag_fallback: FallbackBackoff,
    text_fallback: FallbackBackoff,
}

/// 某类 AI 批次（embedding / 打标签）的兜底退避状态。
///
/// worker 在缺依赖或缺权重时不会报错，而是返回 `fallback: true` 的伪结果；这些结果不入库、
/// 图片保持 pending，等真模型可用后再算。为了不每 2 秒投一批必然白算的任务，收到兜底响应后
/// 暂停投递 [`FALLBACK_BACKOFF`]。是否兜底取决于 worker 进程（模型在进程内只加载一次），
/// 所以 worker 换了进程（重启、用户修好环境后手动重启）就立即解除，不必等满窗口。
#[derive(Debug, Default)]
pub struct FallbackBackoff {
    until: Option<Instant>,
    worker_pid: Option<u32>,
    warned: bool,
}

impl FallbackBackoff {
    /// 记录一次兜底响应并开启退避窗口。返回 `true` 表示这是该 worker 进程第一次兜底，
    /// 调用方据此只 warn 一次，而不是每次探测都刷日志。
    pub fn record_fallback(&mut self, now: Instant, worker_pid: Option<u32>) -> bool {
        let first_for_worker = !self.warned || self.worker_pid != worker_pid;
        self.until = Some(now + FALLBACK_BACKOFF);
        self.worker_pid = worker_pid;
        self.warned = true;
        first_for_worker
    }

    /// 收到真模型结果：清除退避；之后若再兜底会重新 warn。
    pub fn record_real(&mut self) {
        *self = Self::default();
    }

    /// 是否仍处于退避中：窗口未过期，且 worker 还是记录兜底时的那个进程。
    pub fn is_active(&self, now: Instant, worker_pid: Option<u32>) -> bool {
        self.until
            .is_some_and(|until| now < until && self.worker_pid == worker_pid)
    }
}

/// 一批 `/clip/embed` 响应的落库结果。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EmbedApplyOutcome {
    /// 写入 LanceDB、`ai_jobs` 记为 ready 的条数。
    pub stored: usize,
    /// worker 报单条错误（或维度不对）、`ai_jobs` 记为 failed 的条数。
    pub failed: usize,
    /// worker 处于兜底模式：整批一条都没写（图片保持待处理）。
    pub fallback: bool,
}

/// 一批 `/tagger/run` 响应的落库结果。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TagApplyOutcome {
    /// 写入标签并标记 ready 的图片数。
    pub tagged: usize,
    /// worker 报单条错误、标记 failed 的图片数。
    pub failed: usize,
    /// worker 处于兜底模式（占位标签）：整批一条都没写（图片保持 pending）。
    pub fallback: bool,
}

/// 把图像向量响应落库：向量写进该模型自己的表（[`VectorTable::image_clip`]），状态记进
/// `ai_jobs`（kind = `image_embedding`，model = 响应里的模型）。
///
/// 兜底伪向量（`response.fallback == true`）**一律不写**：不进 LanceDB、不记 `ai_jobs`，
/// 图片保持待处理，真模型可用后会被重新拾取。否则它们会被当成已完成，换成真模型后也永远不会重算。
pub async fn apply_embed_response(
    pool: &Pool,
    vectors: &LanceDbRepo,
    response: ClipEmbedResponse,
    now: i64,
) -> AppResult<EmbedApplyOutcome> {
    if response.fallback {
        return Ok(EmbedApplyOutcome {
            fallback: true,
            ..EmbedApplyOutcome::default()
        });
    }
    let table = VectorTable::image_clip(&response.model, response.dims);
    let mut embeddings = Vec::new();
    let mut failures = Vec::new();
    for item in response.items {
        match item.embedding {
            Some(vector) if vector.len() == response.dims => embeddings.push(EmbeddingRecord {
                image_id: item.id,
                model: response.model.clone(),
                embedding: vector,
                created_at: now,
            }),
            Some(vector) => failures.push((
                item.id,
                format!(
                    "向量维度 {} 与模型维度 {} 不符",
                    vector.len(),
                    response.dims
                ),
            )),
            None => failures.push((
                item.id,
                item.error
                    .unwrap_or_else(|| "AI worker 未返回向量".to_string()),
            )),
        }
    }
    // 先写向量再记 ready：中途失败时状态仍是待处理，下一轮重算（upsert 按 image_id 覆盖）。
    vectors.upsert_embeddings(&table, &embeddings).await?;
    let conn = pool.get()?;
    let tx = conn.unchecked_transaction()?;
    for record in &embeddings {
        ai_jobs_repo::record(
            &tx,
            KIND_IMAGE_EMBEDDING,
            &response.model,
            &JobResult {
                image_id: record.image_id,
                status: ai_jobs_repo::STATUS_READY,
                error: None,
                input_hash: None,
            },
            now,
        )?;
    }
    for (image_id, error) in &failures {
        ai_jobs_repo::record(
            &tx,
            KIND_IMAGE_EMBEDDING,
            &response.model,
            &JobResult {
                image_id: *image_id,
                status: ai_jobs_repo::STATUS_FAILED,
                error: Some(error),
                input_hash: None,
            },
            now,
        )?;
    }
    tx.commit()?;
    Ok(EmbedApplyOutcome {
        stored: embeddings.len(),
        failed: failures.len(),
        fallback: false,
    })
}

/// 一批 `/text/embed` 响应的落库结果。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TextEmbedApplyOutcome {
    pub stored: usize,
    pub failed: usize,
    pub fallback: bool,
}

/// 把描述文本向量落库：向量写进该模型自己的表（[`VectorTable::image_text`]），状态记进 `ai_jobs`
/// （kind = `text_embedding`，input_hash = 描述文本的指纹，描述变了会自动重算）。
///
/// `items` 是 (image_id, 送去编码的描述) 并与 `response.embeddings` 一一对应。兜底伪向量一律不写。
pub async fn apply_text_embed_response(
    pool: &Pool,
    vectors: &LanceDbRepo,
    items: &[(i64, String)],
    response: TextEmbedResponse,
    now: i64,
) -> AppResult<TextEmbedApplyOutcome> {
    if response.fallback {
        return Ok(TextEmbedApplyOutcome {
            fallback: true,
            ..TextEmbedApplyOutcome::default()
        });
    }
    if response.embeddings.len() != items.len() {
        return Err(AppError::Other(format!(
            "AI worker 返回 {} 条文本向量，请求了 {} 条",
            response.embeddings.len(),
            items.len()
        )));
    }
    let table = VectorTable::image_text(&response.model, response.dims);
    let mut records = Vec::new();
    let mut failures = Vec::new();
    for ((image_id, text), embedding) in items.iter().zip(response.embeddings) {
        if embedding.len() == response.dims {
            records.push((
                EmbeddingRecord {
                    image_id: *image_id,
                    model: response.model.clone(),
                    embedding,
                    created_at: now,
                },
                crate::db::text_hash(text),
            ));
        } else {
            failures.push((
                *image_id,
                crate::db::text_hash(text),
                format!(
                    "向量维度 {} 与模型维度 {} 不符",
                    embedding.len(),
                    response.dims
                ),
            ));
        }
    }
    let embeddings = records
        .iter()
        .map(|(record, _)| record.clone())
        .collect::<Vec<_>>();
    vectors.upsert_embeddings(&table, &embeddings).await?;
    let conn = pool.get()?;
    let tx = conn.unchecked_transaction()?;
    for (record, hash) in &records {
        ai_jobs_repo::record(
            &tx,
            KIND_TEXT_EMBEDDING,
            &response.model,
            &JobResult {
                image_id: record.image_id,
                status: ai_jobs_repo::STATUS_READY,
                error: None,
                input_hash: Some(hash),
            },
            now,
        )?;
    }
    for (image_id, hash, error) in &failures {
        ai_jobs_repo::record(
            &tx,
            KIND_TEXT_EMBEDDING,
            &response.model,
            &JobResult {
                image_id: *image_id,
                status: ai_jobs_repo::STATUS_FAILED,
                error: Some(error),
                input_hash: Some(hash),
            },
            now,
        )?;
    }
    tx.commit()?;
    Ok(TextEmbedApplyOutcome {
        stored: records.len(),
        failed: failures.len(),
        fallback: false,
    })
}

/// 把打标签响应落库。兜底的占位标签（颜色启发式等，`response.fallback == true`）**一律不写**：
/// 不动 `image_tags`、不改 `tag_status`，图片保持 pending，等真 tagger 可用后重打。
pub fn apply_tag_response(
    pool: &Pool,
    response: TaggerResponse,
    now: i64,
) -> AppResult<TagApplyOutcome> {
    if response.fallback {
        return Ok(TagApplyOutcome {
            fallback: true,
            ..TagApplyOutcome::default()
        });
    }
    let mut outcome = TagApplyOutcome::default();
    let mut conn = pool.get()?;
    for item in response.items {
        if item.error.is_some() {
            images_repo::set_tag_status(&conn, item.id, "failed")?;
            outcome.failed += 1;
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
        outcome.tagged += 1;
    }
    Ok(outcome)
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
        let (embedding_model, text_embedding_model) = settings_service::read(&self.paths)
            .map(|settings| (settings.ai_clip_model, settings.ai_text_model))
            .unwrap_or_else(|_| {
                (
                    crate::config::DEFAULT_CLIP_MODEL.to_string(),
                    crate::config::DEFAULT_TEXT_MODEL.to_string(),
                )
            });
        let conn = match self.pool.get() {
            Ok(conn) => conn,
            Err(_) => {
                return AiPipelineStatus {
                    embedding_pending: 0,
                    tagging_pending: 0,
                    embedding_inflight: 0,
                    tagging_inflight: 0,
                    embedding_model,
                    text_embedding_pending: 0,
                    text_embedding_inflight: 0,
                    text_embedding_model,
                }
            }
        };
        let embedding_pending =
            ai_jobs_repo::count_pending_image_embedding(&conn, &embedding_model).unwrap_or(0);
        let tagging_pending = count_pending_tags(&conn).unwrap_or(0);
        let text_embedding_pending =
            ai_jobs_repo::count_pending_text_embedding(&conn, &text_embedding_model).unwrap_or(0);
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
            embedding_model,
            text_embedding_pending,
            text_embedding_inflight: state
                .as_ref()
                .map(|state| state.text_inflight.len())
                .unwrap_or(0),
            text_embedding_model,
        }
    }

    pub async fn enqueue_pending(&self) -> AppResult<()> {
        let settings = settings_service::read(&self.paths)?;
        if !settings.ai_enabled {
            return Ok(());
        }
        // GPU 任务暂停（使用中 / 全屏 / 手动暂停）时不投递，也不唤醒 worker；已投递的在闸门前等。
        if matches!(
            self.supervisor.gpu_gate().policy(),
            crate::services::resource_governor::GpuPolicy::Paused
        ) {
            return Ok(());
        }
        let clip_model = settings.ai_clip_model;
        let text_model = settings.ai_text_model;
        // 先看有没有待处理的图：没有就别去唤醒（启动）Python worker。
        let (embed_pending, tag_pending, text_pending) = {
            let conn = self.pool.get()?;
            (
                ai_jobs_repo::count_pending_image_embedding(&conn, &clip_model)?,
                count_pending_tags(&conn)?,
                ai_jobs_repo::count_pending_text_embedding(&conn, &text_model)?,
            )
        };
        if embed_pending == 0 && tag_pending == 0 && text_pending == 0 {
            return Ok(());
        }
        // 兜底退避：worker 上次对这类任务返回的是伪结果（不入库），同一 worker 进程在窗口内
        // 不再投递；几类都在退避中时也不去唤醒 worker。
        let pid = worker_pid(&self.supervisor).await;
        let (embed_backoff, tag_backoff, text_backoff) = {
            let now = Instant::now();
            match self.state.lock() {
                Ok(state) => (
                    state.embed_fallback.is_active(now, pid),
                    state.tag_fallback.is_active(now, pid),
                    state.text_fallback.is_active(now, pid),
                ),
                Err(_) => (false, false, false),
            }
        };
        let want_embed = embed_pending > 0 && !embed_backoff;
        let want_tag = tag_pending > 0 && !tag_backoff;
        let want_text = text_pending > 0 && !text_backoff;
        if !want_embed && !want_tag && !want_text {
            return Ok(());
        }
        // worker 未就绪（或正处于启动失败退避窗口内）就跳过本轮：
        // 否则会反复向队列投递必然失败的任务，刷满 "queue task failed"。
        if !self.supervisor.ensure_ready().await {
            return Ok(());
        }
        if want_embed {
            self.enqueue_embedding_batch(&clip_model)?;
        }
        if want_tag {
            self.enqueue_tag_batch()?;
        }
        if want_text {
            self.enqueue_text_embedding_batch(&text_model)?;
        }
        Ok(())
    }

    fn enqueue_text_embedding_batch(&self, model: &str) -> AppResult<()> {
        let conn = self.pool.get()?;
        let candidates = ai_jobs_repo::pending_text_embedding(&conn, model, TEXT_EMBED_BATCH * 2)?;
        drop(conn);
        let ids = take_not_inflight(
            &candidates,
            &self.state,
            |state| &mut state.text_inflight,
            TEXT_EMBED_BATCH as usize,
        );
        if ids.is_empty() {
            return Ok(());
        }
        self.scheduler.enqueue(TextEmbedBatchTask {
            pool: self.pool.clone(),
            supervisor: self.supervisor.clone(),
            vectors: self.vectors.clone(),
            state: self.state.clone(),
            model: model.to_string(),
            image_ids: ids,
        })
    }

    fn enqueue_embedding_batch(&self, model: &str) -> AppResult<()> {
        let conn = self.pool.get()?;
        let candidates = ai_jobs_repo::pending_image_embedding(&conn, model, EMBED_BATCH * 2)?;
        drop(conn);
        let ids = take_not_inflight(
            &candidates,
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
            model: model.to_string(),
            image_ids: ids,
        })
    }

    fn enqueue_tag_batch(&self) -> AppResult<()> {
        let conn = self.pool.get()?;
        let candidates = images_repo::pending_tag_images(&conn, TAG_BATCH * 2)?
            .into_iter()
            .map(|record| record.id)
            .collect::<Vec<_>>();
        drop(conn);
        let ids = take_not_inflight(
            &candidates,
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
    /// 当前图像向量模型（设置项 `aiClipModel`）。
    pub embedding_model: String,
    /// 描述文本向量（docs/10 A4）。
    pub text_embedding_pending: i64,
    pub text_embedding_inflight: usize,
    pub text_embedding_model: String,
}

struct EmbedBatchTask {
    pool: Pool,
    supervisor: Arc<AiSupervisor>,
    vectors: LanceDbRepo,
    thumbs_dir: PathBuf,
    state: Arc<Mutex<AiPipelineState>>,
    /// 投递时的图像向量模型；worker 按它加载模型，结果写进该模型的表。
    model: String,
    image_ids: Vec<i64>,
}

struct TextEmbedBatchTask {
    pool: Pool,
    supervisor: Arc<AiSupervisor>,
    vectors: LanceDbRepo,
    state: Arc<Mutex<AiPipelineState>>,
    model: String,
    image_ids: Vec<i64>,
}

#[async_trait]
impl Task for TextEmbedBatchTask {
    fn priority(&self) -> Priority {
        Priority::P6
    }

    fn label(&self) -> String {
        format!("ai-text-embed:{}", self.image_ids.len())
    }

    async fn run(&self, ctx: TaskContext) -> AppResult<()> {
        let result = self.run_inner(ctx).await;
        clear_inflight(&self.state, &self.image_ids, |state| {
            &mut state.text_inflight
        });
        result
    }
}

impl TextEmbedBatchTask {
    async fn run_inner(&self, ctx: TaskContext) -> AppResult<()> {
        if ctx.is_cancelled() {
            return Ok(());
        }
        let gate = self.supervisor.gpu_gate();
        let Some(_gpu) = gate.acquire(&ctx.cancellation_token()).await else {
            return Ok(());
        };
        // 运行时再读描述：投递之后描述可能又被更新，记录的指纹要对应真正编码的文本。
        let items = {
            let conn = self.pool.get()?;
            crate::repo::analysis_repo::texts_for(&conn, &self.image_ids)?
        }
        .into_iter()
        .filter(|(_, texts)| !texts.description.trim().is_empty())
        .map(|(image_id, texts)| (image_id, texts.description))
        .collect::<Vec<_>>();
        if items.is_empty() {
            return Ok(());
        }
        let response = self
            .supervisor
            .embed_texts(
                items.iter().map(|(_, text)| text.clone()).collect(),
                Some(self.model.clone()),
                false,
            )
            .await?;
        let model = response.model.clone();
        if model != self.model {
            return Err(AppError::Other(format!(
                "AI worker 返回的文本向量模型 {model} 与请求的 {} 不一致",
                self.model
            )));
        }
        let outcome =
            apply_text_embed_response(&self.pool, &self.vectors, &items, response, now_unix())
                .await?;
        track_fallback(
            &self.supervisor,
            &self.state,
            |state| &mut state.text_fallback,
            outcome.fallback,
            "text embedding",
            &model,
        )
        .await;
        Ok(())
    }
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
        // 资源治理：使用中 / 全屏时暂停，短暂离开时按占空比冷却（docs/11 §5.2）。
        let gate = self.supervisor.gpu_gate();
        let Some(_gpu) = gate.acquire(&ctx.cancellation_token()).await else {
            return Ok(());
        };
        let request_items = {
            let conn = self.pool.get()?;
            images_repo::ready_thumb_hashes(&conn, &self.image_ids)?
        }
        .into_iter()
        .map(|(id, hash)| ClipEmbedItem {
            id,
            thumb_path: thumbnail_service::thumb_path(&self.thumbs_dir, &hash)
                .to_string_lossy()
                .to_string(),
        })
        .collect::<Vec<_>>();
        if request_items.is_empty() {
            return Ok(());
        }
        let response = self
            .supervisor
            .embed_images(request_items, Some(self.model.clone()))
            .await?;
        let model = response.model.clone();
        if model != self.model {
            // worker 回的不是请求的模型（理论上不会发生）：结果不入库，免得写进别的模型的表。
            return Err(AppError::Other(format!(
                "AI worker 返回的图像向量模型 {model} 与请求的 {} 不一致",
                self.model
            )));
        }
        let outcome = apply_embed_response(&self.pool, &self.vectors, response, now_unix()).await?;
        track_fallback(
            &self.supervisor,
            &self.state,
            |state| &mut state.embed_fallback,
            outcome.fallback,
            "embedding",
            &model,
        )
        .await;
        Ok(())
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
        let gate = self.supervisor.gpu_gate();
        let Some(_gpu) = gate.acquire(&ctx.cancellation_token()).await else {
            return Ok(());
        };
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
        let model = response.model.clone();
        let outcome = apply_tag_response(&self.pool, response, now_unix())?;
        track_fallback(
            &self.supervisor,
            &self.state,
            |state| &mut state.tag_fallback,
            outcome.fallback,
            "tagging",
            &model,
        )
        .await;
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
    candidates: &[i64],
    state: &Arc<Mutex<AiPipelineState>>,
    inflight: impl Fn(&mut AiPipelineState) -> &mut HashSet<i64>,
    limit: usize,
) -> Vec<i64> {
    let mut selected = Vec::new();
    let Ok(mut state) = state.lock() else {
        return selected;
    };
    let set = inflight(&mut state);
    for id in candidates {
        if selected.len() >= limit {
            break;
        }
        if set.insert(*id) {
            selected.push(*id);
        }
    }
    selected
}

/// 按本批响应是否兜底更新退避状态；同一 worker 进程只 warn 一次。
async fn track_fallback(
    supervisor: &AiSupervisor,
    state: &Arc<Mutex<AiPipelineState>>,
    backoff: impl Fn(&mut AiPipelineState) -> &mut FallbackBackoff,
    fallback: bool,
    kind: &'static str,
    model: &str,
) {
    if !fallback {
        if let Ok(mut state) = state.lock() {
            backoff(&mut state).record_real();
        }
        return;
    }
    let pid = worker_pid(supervisor).await;
    let first = state
        .lock()
        .map(|mut state| backoff(&mut state).record_fallback(Instant::now(), pid))
        .unwrap_or(false);
    if first {
        tracing::warn!(
            kind,
            model,
            backoff_secs = FALLBACK_BACKOFF.as_secs(),
            "AI worker 处于兜底模式（缺 torch/open_clip 或模型权重），本批结果不入库、图片保持 pending；\
             暂停投递该类任务，worker 重启或退避到期后再试"
        );
    } else {
        tracing::debug!(kind, model, "AI worker 仍处于兜底模式，继续退避");
    }
}

async fn worker_pid(supervisor: &AiSupervisor) -> Option<u32> {
    supervisor.status().await.ok().and_then(|status| status.pid)
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

/// RAM-plus 标签仍用 images.tag_status（docs/10 A7 由 VLM 接管后下线）。
fn count_pending_tags(conn: &rusqlite::Connection) -> AppResult<i64> {
    Ok(conn.query_row(
        "SELECT COUNT(*) FROM images
         WHERE deleted_at IS NULL AND thumb_status = 'ready' AND tag_status = 'pending'",
        [],
        |row| row.get(0),
    )?)
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
