use std::fs;

use crate::config::{AppPaths, AppSettings, AppSettingsPatch};
use crate::error::AppResult;

pub fn read(paths: &AppPaths) -> AppResult<AppSettings> {
    if !paths.config_path.exists() {
        return Ok(AppSettings::default());
    }

    let raw = fs::read_to_string(&paths.config_path)?;
    let settings = serde_json::from_str(&raw).unwrap_or_else(|error| {
        tracing::warn!(%error, path = ?paths.config_path, "failed to parse config, using defaults");
        AppSettings::default()
    });
    Ok(settings)
}

pub fn update(paths: &AppPaths, patch: AppSettingsPatch) -> AppResult<AppSettings> {
    let mut settings = read(paths)?;
    settings.apply_patch(patch);
    write(paths, &settings)?;
    Ok(settings)
}

pub fn write(paths: &AppPaths, settings: &AppSettings) -> AppResult<()> {
    if let Some(parent) = paths.config_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let raw = serde_json::to_string_pretty(settings)
        .map_err(|error| crate::error::AppError::Config(error.to_string()))?;
    fs::write(&paths.config_path, raw)?;
    Ok(())
}
