use tauri::{AppHandle, State};

use crate::config::AppPaths;
use crate::db::Pool;
use crate::queue::{QueueStatus, Scheduler};
use crate::services::scan_service::{self, ScanRootTask, ScanStartResult, ScanTaskStatus};

#[tauri::command]
pub fn scan_start(
    pool: State<'_, Pool>,
    scheduler: State<'_, Scheduler>,
    app: AppHandle,
    paths: State<'_, AppPaths>,
    root_id: i64,
) -> Result<ScanStartResult, String> {
    let result = scan_service::create_scan_task(&pool, root_id).map_err(|e| e.to_string())?;
    scheduler
        .enqueue(ScanRootTask::new(
            pool.inner().clone(),
            scheduler.inner().clone(),
            app,
            paths.inner().clone(),
            root_id,
            result.task_id,
        ))
        .map_err(|e| e.to_string())?;
    Ok(result)
}

#[tauri::command]
pub fn scan_pause(scheduler: State<'_, Scheduler>) -> QueueStatus {
    scheduler.pause();
    scheduler.status()
}

#[tauri::command]
pub fn scan_resume(scheduler: State<'_, Scheduler>) -> QueueStatus {
    scheduler.resume();
    scheduler.status()
}

#[tauri::command]
pub fn scan_cancel(scheduler: State<'_, Scheduler>) -> QueueStatus {
    scheduler.cancel_running();
    scheduler.status()
}

#[tauri::command]
pub fn scan_status(pool: State<'_, Pool>) -> Result<Vec<ScanTaskStatus>, String> {
    scan_service::list_scan_status(&pool).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn queue_status(scheduler: State<'_, Scheduler>) -> QueueStatus {
    scheduler.status()
}
