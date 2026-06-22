use std::path::PathBuf;

use serde::Deserialize;
use tauri::State;

use crate::config::AppPaths;
use crate::services::backup_service::{self, BackupExportResult, BackupImportResult};

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupExportArgs {
    pub destination: String,
    pub include_thumbs: Option<bool>,
    pub include_models: Option<bool>,
}

#[tauri::command]
pub fn library_backup_export(
    paths: State<'_, AppPaths>,
    args: BackupExportArgs,
) -> Result<BackupExportResult, String> {
    backup_service::export_backup(
        &paths,
        &PathBuf::from(args.destination),
        args.include_thumbs.unwrap_or(false),
        args.include_models.unwrap_or(false),
    )
    .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn library_backup_import(
    paths: State<'_, AppPaths>,
    source: String,
) -> Result<BackupImportResult, String> {
    backup_service::import_backup(&paths, &PathBuf::from(source)).map_err(|e| e.to_string())
}
