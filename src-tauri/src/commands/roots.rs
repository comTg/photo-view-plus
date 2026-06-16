//! Tauri 命令层：参数校验 → 调 repo → 返回结果。错误统一转为 String 给前端。

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tauri::State;

use crate::db::Pool;
use crate::repo::roots_repo::{self, NewRoot, Root, RootPatch};
use crate::utils::path_normalize;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AddRootArgs {
    pub path: String,
    pub label: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateRootArgs {
    pub id: i64,
    #[serde(flatten)]
    pub patch: RootPatch,
}

#[derive(Debug, Serialize)]
pub struct RemoveResult {
    pub removed: bool,
}

#[tauri::command]
pub fn roots_add(pool: State<'_, Pool>, args: AddRootArgs) -> Result<Root, String> {
    let raw = PathBuf::from(&args.path);
    let normalized = path_normalize::normalize(&raw).map_err(|e| e.to_string())?;
    let root_type = path_normalize::detect_root_type(&raw).to_string();

    let conn = pool.get().map_err(|e| e.to_string())?;
    let normalized_str = normalized.to_string_lossy().to_string();

    if let Some(existing) =
        roots_repo::find_by_path(&conn, &normalized_str).map_err(|e| e.to_string())?
    {
        return Err(format!(
            "目录已添加：{}（id={}）",
            existing.path, existing.id
        ));
    }

    let now = now_unix();
    let new_root = NewRoot {
        path: normalized_str,
        label: args.label,
        root_type,
    };
    roots_repo::insert(&conn, &new_root, now).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn roots_list(pool: State<'_, Pool>) -> Result<Vec<Root>, String> {
    let conn = pool.get().map_err(|e| e.to_string())?;
    roots_repo::list(&conn).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn roots_remove(pool: State<'_, Pool>, id: i64) -> Result<RemoveResult, String> {
    let conn = pool.get().map_err(|e| e.to_string())?;
    let removed = roots_repo::remove(&conn, id).map_err(|e| e.to_string())?;
    Ok(RemoveResult { removed })
}

#[tauri::command]
pub fn roots_update(pool: State<'_, Pool>, args: UpdateRootArgs) -> Result<Option<Root>, String> {
    let conn = pool.get().map_err(|e| e.to_string())?;
    roots_repo::update(&conn, args.id, &args.patch).map_err(|e| e.to_string())
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
