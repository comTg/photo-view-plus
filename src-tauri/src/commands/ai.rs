use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;
use tauri::{AppHandle, Emitter, State};

use crate::config::AppPaths;
use crate::db::Pool;
use crate::repo::images_repo::{self, ImageRecord};
use crate::repo::tags_repo::{self, ImageTag, NewTagScore, Tag};
use crate::services::ai_client::TaggerItem;
use crate::services::ai_pipeline::{AiPipeline, AiPipelineStatus};
use crate::services::ai_supervisor::{AiSupervisor, AiWorkerStatus};
use crate::services::analysis_pipeline::AnalysisPipeline;
use crate::services::resource_governor::{GpuPolicy, ResourceGovernor, Tier};
use crate::services::search_index_service::{self, RebuildOutcome, SearchIndexStatus};
use crate::services::search_service::{
    self, AiSearchArgs, AiSearchResponse, AiSearchResult, WorkerQueryEncoder,
};
use crate::services::settings_service;
use crate::services::thumbnail_service;

#[tauri::command]
pub async fn ai_worker_start(
    app: AppHandle,
    supervisor: State<'_, Arc<AiSupervisor>>,
) -> Result<AiWorkerStatus, String> {
    let status = supervisor.start().await.map_err(|e| e.to_string())?;
    let _ = app.emit("ai:worker_status", status.clone());
    Ok(status)
}

#[tauri::command]
pub async fn ai_worker_stop(
    app: AppHandle,
    supervisor: State<'_, Arc<AiSupervisor>>,
) -> Result<AiWorkerStatus, String> {
    let status = supervisor.stop().await.map_err(|e| e.to_string())?;
    let _ = app.emit("ai:worker_status", status.clone());
    Ok(status)
}

#[tauri::command]
pub async fn ai_worker_status(
    supervisor: State<'_, Arc<AiSupervisor>>,
) -> Result<AiWorkerStatus, String> {
    supervisor.status().await.map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn ai_worker_diagnostics(
    supervisor: State<'_, Arc<AiSupervisor>>,
) -> Result<Value, String> {
    supervisor.diagnostics().await.map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn ai_models_status(supervisor: State<'_, Arc<AiSupervisor>>) -> Result<Value, String> {
    supervisor.models_status().await.map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn ai_model_download(
    supervisor: State<'_, Arc<AiSupervisor>>,
    model_key: String,
) -> Result<Value, String> {
    supervisor
        .model_download(&model_key)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn ai_search(
    pool: State<'_, Pool>,
    pipeline: State<'_, Arc<AiPipeline>>,
    supervisor: State<'_, Arc<AiSupervisor>>,
    paths: State<'_, AppPaths>,
    args: AiSearchArgs,
) -> Result<AiSearchResponse, String> {
    let encoder = query_encoder(&paths, &supervisor)?;
    search_service::search(&pool, &pipeline.vectors(), &encoder, &args)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn ai_search_by_image(
    pool: State<'_, Pool>,
    paths: State<'_, AppPaths>,
    pipeline: State<'_, Arc<AiPipeline>>,
    supervisor: State<'_, Arc<AiSupervisor>>,
    image_id: i64,
    limit: Option<usize>,
) -> Result<Vec<AiSearchResult>, String> {
    let encoder = query_encoder(&paths, &supervisor)?;
    search_service::search_by_image(
        &pool,
        &pipeline.vectors(),
        &encoder,
        &paths.thumbs_dir,
        image_id,
        limit,
    )
    .await
    .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn ai_tag_image(
    pool: State<'_, Pool>,
    paths: State<'_, AppPaths>,
    supervisor: State<'_, Arc<AiSupervisor>>,
    analysis: State<'_, Arc<AnalysisPipeline>>,
    image_id: i64,
) -> Result<Vec<ImageTag>, String> {
    // 标签来源是大模型（docs/10 A7）：对这张图跑一次 VLM 分析，标签随结果写入。
    let tagger = settings_service::read(&paths)
        .map(|settings| settings.ai_tagger_model)
        .unwrap_or_default();
    if tagger == crate::config::TAGGER_VLM {
        analysis
            .analyze_now(image_id)
            .await
            .map_err(|e| e.to_string())?;
        let conn = pool.get().map_err(|e| e.to_string())?;
        return tags_repo::tags_for_image(&conn, image_id).map_err(|e| e.to_string());
    }
    let detail = {
        let conn = pool.get().map_err(|e| e.to_string())?;
        images_repo::get_detail(&conn, image_id)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("image not found: {image_id}"))?
    };
    let thumb_path = thumb_path_for(&paths, &detail)?;
    let response = supervisor
        .tag_images(vec![TaggerItem {
            id: image_id,
            thumb_path,
        }])
        .await
        .map_err(|e| e.to_string())?;
    // 兜底占位标签（颜色启发式）不入库：写进去并标 ready 后，装好真 tagger 也不会再重打。
    if response.fallback {
        return Err("AI 标签模型未加载（worker 处于兜底模式），未写入占位标签".to_string());
    }
    let item = response
        .items
        .into_iter()
        .next()
        .ok_or_else(|| "AI worker returned no tag result".to_string())?;
    if let Some(error) = item.error {
        return Err(error);
    }
    let now = now_unix();
    let mut conn = pool.get().map_err(|e| e.to_string())?;
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
    tags_repo::replace_image_tags(&mut conn, image_id, &tags, now).map_err(|e| e.to_string())?;
    images_repo::set_tag_status(&conn, image_id, "ready").map_err(|e| e.to_string())?;
    tags_repo::tags_for_image(&conn, image_id).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn ai_pipeline_status(pipeline: State<'_, Arc<AiPipeline>>) -> AiPipelineStatus {
    pipeline.status()
}

/// 手动"处理待办"。GPU 任务被资源治理器暂停时（使用中 / 全屏 / 手动暂停）不投递，返回原因给界面提示。
#[tauri::command]
pub async fn ai_process_pending(
    pipeline: State<'_, Arc<AiPipeline>>,
    supervisor: State<'_, Arc<AiSupervisor>>,
    governor: State<'_, Arc<ResourceGovernor>>,
) -> Result<String, String> {
    if supervisor.gpu_gate().policy() == GpuPolicy::Paused {
        let reason = match governor.status().tier {
            Tier::Paused => "后台处理已暂停，可在状态栏切回\"自动\"或\"全速\"",
            Tier::Busy => "全屏 / 游戏中，AI 暂停",
            _ => "正在使用电脑，AI 先暂停；离开电脑或在状态栏切到\"全速\"后开始",
        };
        return Ok(reason.to_string());
    }
    pipeline.enqueue_pending().await.map_err(|e| e.to_string())?;
    Ok("已触发 AI 后台处理".to_string())
}

/// 清空旧的 AI 标签（含历史英文标签）并把就绪图片重置为待打标签，随后立即触发一轮重打。
/// 返回被重置的图片数。重打用的是当前 tagger（接入 RAM-plus 后即为中文）。
#[tauri::command]
pub async fn ai_retag_all(
    pool: State<'_, Pool>,
    paths: State<'_, AppPaths>,
    pipeline: State<'_, Arc<AiPipeline>>,
) -> Result<usize, String> {
    let tagger = settings_service::read(&paths)
        .map(|settings| settings.ai_tagger_model)
        .unwrap_or_default();
    if tagger == crate::config::TAGGER_VLM {
        return Err("当前标签由大模型生成：要重新分析，请在设置的\"大模型打标\"里操作".to_string());
    }
    let reset = {
        let mut conn = pool.get().map_err(|e| e.to_string())?;
        tags_repo::clear_ai_tags_and_reset(&mut conn).map_err(|e| e.to_string())?
    };
    // 清理已成功；尽量立刻起一轮重打（worker 未就绪时后台流水线稍后也会处理）。
    let _ = pipeline.enqueue_pending().await;
    Ok(reset)
}

/// 全量重建检索全文索引（改了分词规则时用）。在 blocking 线程里跑，不占 async runtime。
#[tauri::command]
pub async fn search_index_rebuild(pool: State<'_, Pool>) -> Result<RebuildOutcome, String> {
    let pool = pool.inner().clone();
    tokio::task::spawn_blocking(move || {
        let mut conn = pool.get().map_err(|e| e.to_string())?;
        search_index_service::rebuild_all(&mut conn).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
pub fn search_index_status(pool: State<'_, Pool>) -> Result<SearchIndexStatus, String> {
    let conn = pool.get().map_err(|e| e.to_string())?;
    search_index_service::status(&conn).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn ai_tags_list(pool: State<'_, Pool>, limit: Option<i64>) -> Result<Vec<Tag>, String> {
    let conn = pool.get().map_err(|e| e.to_string())?;
    tags_repo::list_tags(&conn, limit.unwrap_or(100)).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn ai_image_tags(pool: State<'_, Pool>, image_id: i64) -> Result<Vec<ImageTag>, String> {
    let conn = pool.get().map_err(|e| e.to_string())?;
    tags_repo::tags_for_image(&conn, image_id).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn ai_images_by_tag(
    pool: State<'_, Pool>,
    tag_id: i64,
    offset: Option<i64>,
    limit: Option<i64>,
) -> Result<images_repo::ImagePage, String> {
    let conn = pool.get().map_err(|e| e.to_string())?;
    let limit = limit.unwrap_or(200).clamp(1, 500);
    let offset = offset.unwrap_or(0).max(0);
    let ids = images_repo::ids_for_tag(&conn, tag_id, limit, offset).map_err(|e| e.to_string())?;
    let items = images_repo::get_details_by_ids(&conn, &ids).map_err(|e| e.to_string())?;
    let total: i64 = conn
        .query_row(
            "SELECT COUNT(*)
             FROM image_tags it
             JOIN images i ON i.id = it.image_id
             WHERE it.tag_id = ?1 AND i.deleted_at IS NULL",
            [tag_id],
            |row| row.get(0),
        )
        .map_err(|e| e.to_string())?;
    Ok(images_repo::ImagePage {
        items,
        total,
        offset,
        limit,
    })
}

fn query_encoder(
    paths: &AppPaths,
    supervisor: &Arc<AiSupervisor>,
) -> Result<WorkerQueryEncoder, String> {
    let settings = settings_service::read(paths).map_err(|e| e.to_string())?;
    Ok(WorkerQueryEncoder::new(
        supervisor.clone(),
        settings.ai_clip_model,
        Some(settings.ai_text_model),
    ))
}

fn thumb_path_for(paths: &AppPaths, image: &ImageRecord) -> Result<String, String> {
    let hash = image
        .thumb_hash
        .as_deref()
        .ok_or_else(|| "缩略图尚未生成，无法运行 AI".to_string())?;
    Ok(thumbnail_service::thumb_path(&paths.thumbs_dir, hash)
        .to_string_lossy()
        .to_string())
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
