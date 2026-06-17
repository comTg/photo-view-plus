//! 回收站删除 + 撤销。所有"删除"必须经此服务，绝不调 `std::fs::remove_file`。
//!
//! 平台行为：
//! - macOS / Linux / Windows：`trash::delete` 进系统回收站
//! - **撤销恢复**：trash crate 5.x 的 `os_limited` 模块只在 Linux/Windows 暴露 list/restore。
//!   - Windows / Linux：尝试匹配 path 列表，调 `restore_all`
//!   - macOS：trash crate 不支持读回收站；undo 只把 DB 的 `deleted_at` 置 NULL
//!     并返回提示，让用户从访达回收站手动还原（文件名会带 `(已删除)` 后缀，不重名冲突）
//!
//! 撤销窗口：30 天，超时由清理任务处理（未在 MVP2 实现）。

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::db::Pool;
use crate::error::{AppError, AppResult};
use crate::repo::images_repo;

const UNDO_TTL_SECS: i64 = 30 * 24 * 60 * 60;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TrashedItem {
    pub image_id: i64,
    pub original_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TrashPayload {
    pub items: Vec<TrashedItem>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TrashOutcome {
    pub succeeded: Vec<i64>,
    pub failed: Vec<TrashFailure>,
    pub undo_id: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TrashFailure {
    pub image_id: i64,
    pub error: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UndoOutcome {
    pub restored: Vec<i64>,
    pub manual_recovery_required: bool,
    pub note: Option<String>,
}

pub fn trash_images(pool: &Pool, image_ids: &[i64], now: i64) -> AppResult<TrashOutcome> {
    let conn = pool.get()?;
    let mut succeeded = Vec::new();
    let mut failed = Vec::new();
    let mut payload_items = Vec::new();

    for &id in image_ids {
        let record = match images_repo::get_detail(&conn, id)? {
            Some(record) => record,
            None => {
                failed.push(TrashFailure {
                    image_id: id,
                    error: "图片记录不存在".to_string(),
                });
                continue;
            }
        };

        let path = PathBuf::from(&record.full_path);
        if !path.exists() {
            // 文件已不在磁盘但 DB 仍有：直接软删 + 跳过 trash 调用
            images_repo::mark_deleted_by_id(&conn, id, now)?;
            payload_items.push(TrashedItem {
                image_id: id,
                original_path: record.full_path,
            });
            succeeded.push(id);
            continue;
        }

        match trash::delete(&path) {
            Ok(_) => {
                images_repo::mark_deleted_by_id(&conn, id, now)?;
                payload_items.push(TrashedItem {
                    image_id: id,
                    original_path: record.full_path,
                });
                succeeded.push(id);
            }
            Err(error) => {
                failed.push(TrashFailure {
                    image_id: id,
                    error: error.to_string(),
                });
            }
        }
    }

    let undo_id = if payload_items.is_empty() {
        None
    } else {
        let payload = serde_json::to_string(&TrashPayload {
            items: payload_items,
        })
        .map_err(|e| AppError::Other(e.to_string()))?;
        images_repo::insert_undo_log(&conn, "trash", &payload, now + UNDO_TTL_SECS, now)?;
        Some(conn.last_insert_rowid())
    };

    Ok(TrashOutcome {
        succeeded,
        failed,
        undo_id,
    })
}

pub fn undo(pool: &Pool, undo_id: i64, now: i64) -> AppResult<UndoOutcome> {
    let conn = pool.get()?;
    let entry = images_repo::get_undo_entry(&conn, undo_id)?
        .ok_or_else(|| AppError::Other(format!("撤销记录不存在：{undo_id}")))?;
    if entry.undone_at.is_some() {
        return Err(AppError::Other("该记录已被撤销".to_string()));
    }
    if entry.action != "trash" {
        return Err(AppError::Other(format!("不支持撤销动作：{}", entry.action)));
    }

    let payload: TrashPayload =
        serde_json::from_str(&entry.payload_json).map_err(|e| AppError::Other(e.to_string()))?;

    let mut restored = Vec::new();

    // 平台分支：能从回收站恢复物理文件的平台先尝试；不能的，仅恢复 DB 标记。
    let manual_recovery_required = restore_files_from_trash(&payload)?;

    for item in &payload.items {
        images_repo::restore_by_id(&conn, item.image_id)?;
        restored.push(item.image_id);
    }
    images_repo::mark_undo_done(&conn, undo_id, now)?;

    let note = if manual_recovery_required {
        Some(
            "DB 标记已恢复，但当前平台无法自动从回收站恢复文件。\
             请到系统回收站手动还原，下次扫描会重新登记。"
                .to_string(),
        )
    } else {
        None
    };

    Ok(UndoOutcome {
        restored,
        manual_recovery_required,
        note,
    })
}

/// 尝试从回收站恢复物理文件。返回 `true` 表示当前平台无法自动恢复（manual_recovery_required）。
#[cfg(any(target_os = "windows", target_os = "linux"))]
fn restore_files_from_trash(payload: &TrashPayload) -> AppResult<bool> {
    use std::collections::HashSet;
    use trash::os_limited;

    let want: HashSet<String> = payload
        .items
        .iter()
        .map(|item| item.original_path.clone())
        .collect();

    let items = os_limited::list().map_err(|e| AppError::Other(e.to_string()))?;
    let mut to_restore = Vec::new();
    for item in items {
        let original = item.original_path().to_string_lossy().to_string();
        if want.contains(&original) {
            to_restore.push(item);
        }
    }

    if to_restore.is_empty() {
        // 回收站里没找到——文件可能被用户清空或手动还原
        return Ok(true);
    }

    os_limited::restore_all(to_restore).map_err(|e| AppError::Other(e.to_string()))?;
    Ok(false)
}

#[cfg(not(any(target_os = "windows", target_os = "linux")))]
fn restore_files_from_trash(_payload: &TrashPayload) -> AppResult<bool> {
    // macOS：trash crate 不提供回收站读/恢复 API；只恢复 DB 标记。
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo::images_repo::NewImageMetadata;
    use crate::repo::roots_repo::{self, NewRoot};
    use std::fs;

    fn fresh_pool() -> (crate::db::Pool, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let pool = crate::db::open(&dir.path().join("app.sqlite")).expect("db");
        (pool, dir)
    }

    #[test]
    fn trash_missing_file_soft_deletes_anyway() {
        let (pool, dir) = fresh_pool();
        let conn = pool.get().expect("conn");
        let root = roots_repo::insert(
            &conn,
            &NewRoot {
                path: dir.path().to_string_lossy().to_string(),
                label: None,
                root_type: "local".to_string(),
            },
            1,
        )
        .expect("root");

        let outcome = images_repo::upsert_scanned_image(
            &conn,
            &NewImageMetadata {
                root_id: root.id,
                rel_path: "ghost.jpg".to_string(),
                filename: "ghost.jpg".to_string(),
                extension: "jpg".to_string(),
                size_bytes: 1,
                mtime: 1,
                width: None,
                height: None,
                orientation: None,
                taken_at: None,
                gps_lat: None,
                gps_lng: None,
                camera_make: None,
                camera_model: None,
            },
            1,
        )
        .expect("insert");
        let id = outcome.image_id();

        // 文件并不真的存在；trash_images 应当走 "missing → 直接软删" 分支
        let result = trash_images(&pool, &[id], 100).expect("trash");
        assert_eq!(result.succeeded, vec![id]);
        assert!(result.failed.is_empty());
        assert!(result.undo_id.is_some());

        let record = images_repo::get_detail(&conn, id)
            .expect("get")
            .expect("some");
        assert_eq!(record.deleted_at, Some(100));
    }

    #[test]
    #[ignore = "调用真实系统回收站，在 macOS 沙盒/CI 中不稳定（trash crate 5.x 在受限环境会失败）。手动跑：cargo test trash_real_file -- --ignored"]
    fn trash_real_file_writes_undo_log() {
        // 在 Windows / Linux 上 trash::delete 走 Shell/Freedesktop。
        // macOS 上需要访达可用，CI / Tauri 沙盒里通常不行——所以这条标 ignored，
        // M2 收尾时由用户在 Windows 实机跑（docs/04 § 2 T8 中 "在 Windows VM 跑" 一致）。
        let (pool, dir) = fresh_pool();
        let conn = pool.get().expect("conn");
        let root = roots_repo::insert(
            &conn,
            &NewRoot {
                path: dir.path().to_string_lossy().to_string(),
                label: None,
                root_type: "local".to_string(),
            },
            1,
        )
        .expect("root");

        let filename = format!("pvp-trash-test-{}.tmp", std::process::id());
        let real_path = dir.path().join(&filename);
        fs::write(&real_path, b"trash me").expect("write");

        let outcome = images_repo::upsert_scanned_image(
            &conn,
            &NewImageMetadata {
                root_id: root.id,
                rel_path: filename.clone(),
                filename: filename.clone(),
                extension: "tmp".to_string(),
                size_bytes: 8,
                mtime: 1,
                width: None,
                height: None,
                orientation: None,
                taken_at: None,
                gps_lat: None,
                gps_lng: None,
                camera_make: None,
                camera_model: None,
            },
            1,
        )
        .expect("insert");
        let id = outcome.image_id();

        let result = trash_images(&pool, &[id], 200).expect("trash");
        assert_eq!(result.succeeded, vec![id]);
        assert!(result.undo_id.is_some());
        assert!(!real_path.exists(), "file should be moved to trash");
    }
}
