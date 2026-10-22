//! 画面分析结果（docs/10 A3）：memora 描述导入、单张查询、统计。

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use tauri::State;

use crate::db::Pool;
use crate::repo::analysis_repo::{self, ImageAnalysis};
use crate::services::memora_import_service::{self, MemoraImportOutcome};
use crate::services::search_index_service;

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

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
