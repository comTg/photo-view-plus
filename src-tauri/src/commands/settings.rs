use tauri::State;

use crate::config::{AppPaths, AppSettings, AppSettingsPatch};
use crate::services::settings_service;

#[tauri::command]
pub fn settings_get(paths: State<'_, AppPaths>) -> Result<AppSettings, String> {
    settings_service::read(&paths).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn settings_update(
    paths: State<'_, AppPaths>,
    patch: AppSettingsPatch,
) -> Result<AppSettings, String> {
    settings_service::update(&paths, patch).map_err(|e| e.to_string())
}
