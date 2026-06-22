use std::sync::Arc;

use serde::Deserialize;
use tauri::State;

use crate::config::AppPaths;
use crate::db::Pool;
use crate::queue::Scheduler;
use crate::services::ai_supervisor::AiSupervisor;
use crate::services::{ocr_service, settings_service};

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OcrRunArgs {
    pub image_ids: Option<Vec<i64>>,
}

#[tauri::command]
pub fn ocr_run(
    pool: State<'_, Pool>,
    scheduler: State<'_, Scheduler>,
    supervisor: State<'_, Arc<AiSupervisor>>,
    paths: State<'_, AppPaths>,
    args: Option<OcrRunArgs>,
) -> Result<usize, String> {
    let settings = settings_service::read(&paths).map_err(|e| e.to_string())?;
    if !settings.ocr_enabled {
        return Err("OCR 默认关闭，请先在设置 > 隐私中启用 OCR".to_string());
    }
    ocr_service::enqueue_ocr(
        &pool,
        &scheduler,
        supervisor.inner().clone(),
        paths.thumbs_dir.clone(),
        args.and_then(|args| args.image_ids),
    )
    .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn ocr_status(pool: State<'_, Pool>) -> Result<ocr_service::OcrStatus, String> {
    ocr_service::status(&pool).map_err(|e| e.to_string())
}
