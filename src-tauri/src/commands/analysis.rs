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
use crate::repo::tags_repo;
use crate::services::analysis_pipeline::{self, AnalysisPipeline, AnalysisProgress};
use crate::services::memora_import_service::{self, MemoraImportOutcome};
use crate::services::night_service::{self, NightTaskStatus};
use crate::services::search_index_service;
use crate::services::settings_service;
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

/// RAM-plus 旧标签与 VLM 覆盖率（docs/10 A7：覆盖率 ≥ 95% 后才允许清理旧标签）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RamTagStatus {
    pub ram_images: i64,
    pub ram_tags: i64,
    pub vlm_analyzed: i64,
    pub total_images: i64,
    pub coverage: f64,
    pub can_cleanup: bool,
}

pub const RAM_CLEANUP_COVERAGE: f64 = 0.95;

fn ram_tag_status(conn: &rusqlite::Connection) -> crate::error::AppResult<RamTagStatus> {
    let (ram_images, ram_tags) = tags_repo::ram_tag_counts(conn)?;
    let (vlm_analyzed, total_images) = ai_jobs_repo::vlm_coverage(conn)?;
    let coverage = if total_images > 0 {
        vlm_analyzed as f64 / total_images as f64
    } else {
        0.0
    };
    Ok(RamTagStatus {
        ram_images,
        ram_tags,
        vlm_analyzed,
        total_images,
        coverage,
        can_cleanup: ram_images > 0 && coverage >= RAM_CLEANUP_COVERAGE,
    })
}

#[tauri::command]
pub fn tags_ram_status(pool: State<'_, Pool>) -> Result<RamTagStatus, String> {
    let conn = pool.get().map_err(|e| e.to_string())?;
    ram_tag_status(&conn).map_err(|e| e.to_string())
}

/// 清理 RAM-plus 的旧标签（用户在界面上确认后调用）。VLM 覆盖率不到 95% 时拒绝。返回清理后的状态。
#[tauri::command]
pub fn tags_cleanup_ram(pool: State<'_, Pool>) -> Result<RamTagStatus, String> {
    let mut conn = pool.get().map_err(|e| e.to_string())?;
    let status = ram_tag_status(&conn).map_err(|e| e.to_string())?;
    if !status.can_cleanup {
        return Err(format!(
            "大模型已分析 {:.1}% 的图片，到 95% 后才能清理旧标签",
            status.coverage * 100.0
        ));
    }
    tags_repo::clear_ai_tags_and_reset(&mut conn).map_err(|e| e.to_string())?;
    tracing::info!(
        images = status.ram_images,
        tags = status.ram_tags,
        "RAM-plus tags cleaned up"
    );
    ram_tag_status(&conn).map_err(|e| e.to_string())
}

fn night_status(app: &tauri::AppHandle, settings: &crate::config::AppSettings) -> NightTaskStatus {
    let task_name = night_service::task_name(&app.config().identifier);
    NightTaskStatus {
        enabled: settings.night_analyze_enabled,
        registered: night_service::task_registered(&task_name),
        task_name,
        start_hour: settings.night_start_hour,
        end_hour: settings.night_end_hour,
        exe: std::env::current_exe()
            .map(|exe| exe.display().to_string())
            .unwrap_or_default(),
    }
}

/// 夜间自动分析（docs/10 A6b）：设置与计划任务的注册状态。
#[tauri::command]
pub async fn night_task_status(
    app: tauri::AppHandle,
    paths: State<'_, AppPaths>,
) -> Result<NightTaskStatus, String> {
    let settings = settings_service::read(&paths).map_err(|e| e.to_string())?;
    tokio::task::spawn_blocking(move || night_status(&app, &settings))
        .await
        .map_err(|e| e.to_string())
}

/// 打开 / 关闭夜间自动分析，同时注册 / 删除计划任务（只在用户操作时调用）。
#[tauri::command]
pub async fn night_task_set(
    app: tauri::AppHandle,
    paths: State<'_, AppPaths>,
    enabled: bool,
    start_hour: u8,
    end_hour: u8,
) -> Result<NightTaskStatus, String> {
    if start_hour % 24 == end_hour % 24 {
        return Err("开始和结束时间不能相同".to_string());
    }
    let settings = settings_service::update(
        &paths,
        crate::config::AppSettingsPatch {
            night_analyze_enabled: Some(enabled),
            night_start_hour: Some(start_hour),
            night_end_hour: Some(end_hour),
            ..crate::config::AppSettingsPatch::default()
        },
    )
    .map_err(|e| e.to_string())?;
    tokio::task::spawn_blocking(move || {
        let task = night_service::task_name(&app.config().identifier);
        if enabled {
            let exe = std::env::current_exe().map_err(|e| e.to_string())?;
            night_service::register_task(&task, &exe, settings.night_start_hour)
                .map_err(|e| e.to_string())?;
        } else {
            night_service::unregister_task(&task).map_err(|e| e.to_string())?;
        }
        Ok(night_status(&app, &settings))
    })
    .await
    .map_err(|e| e.to_string())?
}

/// 立即卸载本应用启动的 llama-server（复用的外部服务不受影响）。返回是否卸载了。
/// 卸载后 30 分钟内流水线不再自动加载，否则下一秒就又加载回来了。
#[tauri::command]
pub async fn vlm_server_stop(pipeline: State<'_, Arc<AnalysisPipeline>>) -> Result<bool, String> {
    pipeline.suspend(analysis_pipeline::MANUAL_UNLOAD_PAUSE);
    Ok(pipeline.vlm().stop(true).await)
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
