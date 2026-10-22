//! 画面分析结果（docs/10 A3、A6）：memora 描述导入、单张查询、统计；VLM 打标的进度与控制。

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use tauri::State;

use crate::config::AppPaths;
use crate::db::Pool;
use crate::repo::ai_jobs_repo;
use crate::repo::analysis_repo::{self, ImageAnalysis};
use crate::services::analysis_pipeline::{self, AnalysisPipeline, AnalysisProgress};
use crate::services::memora_import_service::{self, MemoraImportOutcome};
use crate::services::search_index_service;
use crate::services::vlm_supervisor::VlmPaths;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AnalysisStatus {
    /// 各来源（memora / vlm）的条数。
    pub by_source: Vec<(String, i64)>,
    /// memora 数据库的默认位置（导入对话框的初始路径）。
    pub default_memora_db: String,
}

/// 从 memora 数据库导入描述和类别，导入后立即把检索索引补上（不等后台循环）。
#[tauri::command]
pub async fn analysis_import_memora(
    pool: State<'_, Pool>,
    db_path: String,
) -> Result<MemoraImportOutcome, String> {
    let pool = pool.inner().clone();
    tokio::task::spawn_blocking(move || {
        let mut conn = pool.get().map_err(|e| e.to_string())?;
        let outcome =
            memora_import_service::import_memora(&mut conn, &PathBuf::from(db_path), now_unix())
                .map_err(|e| e.to_string())?;
        search_index_service::flush_all(&mut conn).map_err(|e| e.to_string())?;
        tracing::info!(?outcome, "memora descriptions imported");
        Ok(outcome)
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
pub fn analysis_get(pool: State<'_, Pool>, image_id: i64) -> Result<Option<ImageAnalysis>, String> {
    let conn = pool.get().map_err(|e| e.to_string())?;
    analysis_repo::get(&conn, image_id).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn analysis_status(pool: State<'_, Pool>) -> Result<AnalysisStatus, String> {
    let conn = pool.get().map_err(|e| e.to_string())?;
    Ok(AnalysisStatus {
        by_source: analysis_repo::counts_by_source(&conn).map_err(|e| e.to_string())?,
        default_memora_db: memora_import_service::DEFAULT_MEMORA_DB.to_string(),
    })
}

/// VLM 打标进度（之后的变化走 `analysis:progress` 事件）。
#[tauri::command]
pub async fn analysis_progress(
    pipeline: State<'_, Arc<AnalysisPipeline>>,
) -> Result<AnalysisProgress, String> {
    Ok(pipeline.progress().await)
}

/// 自动查找 llama-server 和模型文件（设置页"自动查找"按钮）。
#[tauri::command]
pub fn vlm_detect_paths(paths: State<'_, AppPaths>) -> Option<VlmPaths> {
    analysis_pipeline::detect_vlm_paths(&paths.models_dir)
}

/// 提示词 / 模型升级后，让旧版本的分析结果重新排队。返回排队的张数。
#[tauri::command]
pub fn analysis_requeue_outdated(pool: State<'_, Pool>) -> Result<usize, String> {
    let conn = pool.get().map_err(|e| e.to_string())?;
    ai_jobs_repo::requeue_outdated_vlm(&conn, &analysis_pipeline::job_model())
        .map_err(|e| e.to_string())
}

/// 清掉分析失败的记录，让它们重新排队（例如 NAS 修好之后）。返回张数。
#[tauri::command]
pub fn analysis_retry_failed(pool: State<'_, Pool>) -> Result<usize, String> {
    let conn = pool.get().map_err(|e| e.to_string())?;
    ai_jobs_repo::retry_failed_vlm(&conn).map_err(|e| e.to_string())
}

/// 立即卸载本应用启动的 llama-server（复用的外部服务不受影响）。返回是否卸载了。
#[tauri::command]
pub async fn vlm_server_stop(pipeline: State<'_, Arc<AnalysisPipeline>>) -> Result<bool, String> {
    Ok(pipeline.vlm().stop(true).await)
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
