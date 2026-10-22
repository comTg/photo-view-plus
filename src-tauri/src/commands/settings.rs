use std::sync::Arc;

use tauri::{AppHandle, Emitter, State};

use crate::config::{AppPaths, AppSettings, AppSettingsPatch};
use crate::services::ai_supervisor::{self, AiSupervisor};
use crate::services::resource_governor::{self, ResourceGovernor};
use crate::services::{settings_service, thumbnail_service};

#[tauri::command]
pub fn settings_get(paths: State<'_, AppPaths>) -> Result<AppSettings, String> {
    settings_service::read(&paths).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn settings_update(
    app: AppHandle,
    paths: State<'_, AppPaths>,
    governor: State<'_, Arc<ResourceGovernor>>,
    supervisor: State<'_, Arc<AiSupervisor>>,
    patch: AppSettingsPatch,
) -> Result<AppSettings, String> {
    let settings = settings_service::update(&paths, patch).map_err(|e| e.to_string())?;
    // 空闲阈值、"使用电脑时也运行 AI"等立即生效，不用重启。
    let (budget, thresholds) =
        resource_governor::settings_from(&settings, thumbnail_service::thumbnail_cpu_limit());
    governor.update_settings(budget, thresholds);
    supervisor.set_idle_stop(ai_supervisor::idle_stop_after(
        settings.ai_idle_stop_minutes,
    ));
    let _ = app.emit("system:load_mode", governor.status());
    Ok(settings)
}
