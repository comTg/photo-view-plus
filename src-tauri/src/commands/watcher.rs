use std::sync::Arc;

use tauri::State;

use crate::config::AppPaths;
use crate::services::{settings_service, watcher_service::WatcherService};

#[tauri::command]
pub fn watcher_start(
    watcher: State<'_, Arc<WatcherService>>,
    paths: State<'_, AppPaths>,
) -> Result<crate::services::watcher_service::WatcherStatus, String> {
    let settings = settings_service::read(&paths).map_err(|e| e.to_string())?;
    if !settings.file_watcher_enabled {
        return Err("文件监听已在设置中关闭".to_string());
    }
    watcher.start().map_err(|e| e.to_string())
}

#[tauri::command]
pub fn watcher_stop(
    watcher: State<'_, Arc<WatcherService>>,
) -> Result<crate::services::watcher_service::WatcherStatus, String> {
    watcher.stop().map_err(|e| e.to_string())
}

#[tauri::command]
pub fn watcher_status(
    watcher: State<'_, Arc<WatcherService>>,
) -> Result<crate::services::watcher_service::WatcherStatus, String> {
    watcher.status().map_err(|e| e.to_string())
}
