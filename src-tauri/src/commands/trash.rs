use std::time::{SystemTime, UNIX_EPOCH};

use tauri::State;

use crate::db::Pool;
use crate::repo::images_repo::{self, UndoEntry};
use crate::services::trash_service::{self, UndoOutcome};

#[tauri::command]
pub fn trash_history(pool: State<'_, Pool>, limit: Option<i64>) -> Result<Vec<UndoEntry>, String> {
    let conn = pool.get().map_err(|e| e.to_string())?;
    images_repo::list_undo_entries(&conn, limit.unwrap_or(50)).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn trash_undo(pool: State<'_, Pool>, undo_id: i64) -> Result<UndoOutcome, String> {
    trash_service::undo(&pool, undo_id, now_unix()).map_err(|e| e.to_string())
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
