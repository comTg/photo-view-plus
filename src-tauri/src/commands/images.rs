use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use tauri::State;

use crate::config::AppPaths;
use crate::db::Pool;
use crate::error::{AppError, AppResult};
use crate::repo::images_repo::{self, ImagePage, ImageQueryParams, ImageRecord, RenameRecordPatch};
use crate::services::thumbnail_service;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RenameImageArgs {
    pub id: i64,
    pub new_filename: String,
}

#[tauri::command]
pub fn images_query(pool: State<'_, Pool>, params: ImageQueryParams) -> Result<ImagePage, String> {
    let conn = pool.get().map_err(|e| e.to_string())?;
    images_repo::query(&conn, &params).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn images_get_detail(pool: State<'_, Pool>, id: i64) -> Result<Option<ImageRecord>, String> {
    let conn = pool.get().map_err(|e| e.to_string())?;
    images_repo::get_detail(&conn, id).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn thumbs_path(
    pool: State<'_, Pool>,
    paths: State<'_, AppPaths>,
    image_id: i64,
    _size: Option<u32>,
) -> Result<Option<String>, String> {
    let conn = pool.get().map_err(|e| e.to_string())?;
    let hash = images_repo::get_thumb_hash(&conn, image_id).map_err(|e| e.to_string())?;
    Ok(hash.map(|hash| {
        thumbnail_service::thumb_path(&paths.thumbs_dir, &hash)
            .to_string_lossy()
            .to_string()
    }))
}

#[tauri::command]
pub fn images_rename(
    pool: State<'_, Pool>,
    args: RenameImageArgs,
) -> Result<Option<ImageRecord>, String> {
    rename_image(&pool, args).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn images_reveal_in_dir(pool: State<'_, Pool>, id: i64) -> Result<(), String> {
    let conn = pool.get().map_err(|e| e.to_string())?;
    let detail = images_repo::get_detail(&conn, id)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("图片不存在：{id}"))?;
    reveal_path(Path::new(&detail.full_path)).map_err(|e| e.to_string())
}

fn rename_image(pool: &Pool, args: RenameImageArgs) -> AppResult<Option<ImageRecord>> {
    let trimmed = args.new_filename.trim();
    validate_filename(trimmed)?;

    let conn = pool.get()?;
    let detail = images_repo::get_detail(&conn, args.id)?
        .ok_or_else(|| AppError::Other(format!("图片不存在：{}", args.id)))?;
    let old_path = PathBuf::from(&detail.full_path);
    let parent = old_path
        .parent()
        .ok_or_else(|| AppError::Other("无法获取图片所在目录".to_string()))?;
    let new_path = parent.join(trimmed);
    if new_path.exists() {
        return Err(AppError::Other(format!(
            "目标文件已存在：{}",
            new_path.display()
        )));
    }

    std::fs::rename(&old_path, &new_path)?;
    let metadata = std::fs::metadata(&new_path)?;
    let rel_parent = PathBuf::from(&detail.rel_path)
        .parent()
        .map(Path::to_path_buf);
    let new_rel = rel_parent
        .filter(|parent| !parent.as_os_str().is_empty())
        .map(|parent| parent.join(trimmed))
        .unwrap_or_else(|| PathBuf::from(trimmed))
        .to_string_lossy()
        .to_string();
    let extension = Path::new(trimmed)
        .extension()
        .map(|ext| {
            ext.to_string_lossy()
                .trim_start_matches('.')
                .to_ascii_lowercase()
        })
        .unwrap_or_default();
    let now = now_unix();
    let payload = serde_json::json!({
        "imageId": args.id,
        "oldPath": old_path.to_string_lossy(),
        "newPath": new_path.to_string_lossy()
    })
    .to_string();
    images_repo::insert_undo_log(&conn, "rename", &payload, now + 30 * 24 * 60 * 60, now)?;
    images_repo::rename_record(
        &conn,
        args.id,
        &RenameRecordPatch {
            rel_path: new_rel,
            filename: trimmed.to_string(),
            extension,
            size_bytes: i64::try_from(metadata.len()).unwrap_or(i64::MAX),
            mtime: metadata
                .modified()
                .ok()
                .and_then(|mtime| mtime.duration_since(UNIX_EPOCH).ok())
                .map(|duration| duration.as_secs() as i64)
                .unwrap_or(0),
            indexed_at: now,
        },
    )
}

fn validate_filename(filename: &str) -> AppResult<()> {
    if filename.is_empty() {
        return Err(AppError::Other("文件名不能为空".to_string()));
    }
    if filename.contains('/') || filename.contains('\\') {
        return Err(AppError::Other("文件名不能包含路径分隔符".to_string()));
    }
    if filename == "." || filename == ".." {
        return Err(AppError::Other("文件名无效".to_string()));
    }
    Ok(())
}

fn reveal_path(path: &Path) -> AppResult<()> {
    if !path.exists() {
        return Err(AppError::Other(format!(
            "文件不存在或不可访问：{}",
            path.display()
        )));
    }

    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;

        Command::new("explorer.exe")
            .raw_arg(format!("/select,\"{}\"", path.display()))
            .spawn()?;
    }

    #[cfg(target_os = "macos")]
    {
        Command::new("open").arg("-R").arg(path).spawn()?;
    }

    #[cfg(all(not(target_os = "windows"), not(target_os = "macos")))]
    {
        let parent = path.parent().unwrap_or(path);
        Command::new("xdg-open").arg(parent).spawn()?;
    }

    Ok(())
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}
