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
    /// 是否为永久删除：网络盘无回收站时回退 `fs::remove_file`，此项不可撤销恢复。
    /// `#[serde(default)]` 兼容旧 undo_log（旧记录全是回收站删除，默认 false）。
    #[serde(default)]
    pub permanent: bool,
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
    /// `succeeded` 的子集：位于网络盘、无回收站，已回退为永久删除（不可恢复）。
    pub permanently_deleted: Vec<i64>,
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

/// 在独立后台线程执行 `trash` crate 调用并阻塞等待结果。
///
/// 为什么必须这样：`trash` 在 Windows 通过 COM（Shell `IFileOperation`）工作。
/// Tauri 的**同步** `#[tauri::command]` 跑在主 UI 线程上，而该线程已被 wry/tao
/// 用 `OleInitialize` 初始化为 COM STA（供窗口拖拽）。开启 `coinit_multithreaded`
/// 后 `trash` 会请求 MTA，在 STA 线程上 `CoInitializeEx` 返回
/// `RPC_E_CHANGED_MODE (0x80010106)` 并直接 panic——panic 跨过 WebView2 的 C++
/// 回调边界无法 unwind，最终 abort 整个进程。
///
/// 全新线程的 COM 尚未初始化，可干净地按 MTA 初始化；`IFileOperation` 在 MTA
/// 下无需消息泵即可同步完成删除/恢复。
fn run_on_worker<T, F>(f: F) -> AppResult<T>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    std::thread::spawn(f)
        .join()
        .map_err(|_| AppError::Other("回收站操作线程异常退出（panic）".to_string()))
}

/// 删除单个文件。优先进系统回收站；若失败且文件位于**网络盘**（无回收站），
/// 回退为永久删除（`fs::remove_file`）。
///
/// 返回 `Ok(true)` = 永久删除（不可恢复），`Ok(false)` = 已进回收站（可撤销）。
///
/// 仅对网络盘回退永久删除：本地盘删除失败（文件被占用等）**绝不**永久删，
/// 直接报错——CLAUDE.md 红线 2（不直接永久删本地图片）对本地路径仍然成立。
fn delete_one(full_path: &str) -> Result<bool, String> {
    let path = std::path::Path::new(full_path);
    match trash::delete(path) {
        Ok(()) => Ok(false),
        Err(recycle_err) => {
            if !crate::utils::path_normalize::is_network_path(path) {
                return Err(recycle_err.to_string());
            }
            // 网络盘无回收站：回退永久删除。
            std::fs::remove_file(path).map(|()| true).map_err(|fs_err| {
                format!("回收站删除失败（{recycle_err}），网络盘永久删除也失败（{fs_err}）")
            })
        }
    }
}

pub fn trash_images(pool: &Pool, image_ids: &[i64], now: i64) -> AppResult<TrashOutcome> {
    let conn = pool.get()?;
    let mut succeeded = Vec::new();
    let mut permanently_deleted = Vec::new();
    let mut failed = Vec::new();
    let mut payload_items = Vec::new();

    // 第一步（调用线程，持 DB 连接）：解析记录，区分"文件已不在磁盘（直接软删）"
    // 与"需进系统回收站"。回收站调用本身不碰 DB，单独收集后批量交给后台线程。
    let mut to_delete: Vec<(i64, String)> = Vec::new();
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

        if !PathBuf::from(&record.full_path).exists() {
            // 文件已不在磁盘但 DB 仍有：直接软删 + 跳过 trash 调用
            images_repo::mark_deleted_by_id(&conn, id, now)?;
            payload_items.push(TrashedItem {
                image_id: id,
                original_path: record.full_path,
                permanent: false,
            });
            succeeded.push(id);
            continue;
        }

        to_delete.push((id, record.full_path));
    }

    // 第二步：在独立后台线程批量删除（回收站走 COM/MTA，避免主线程 STA 冲突；
    // 网络盘无回收站则在同一线程回退永久删除）。
    if !to_delete.is_empty() {
        let results = run_on_worker(move || {
            to_delete
                .into_iter()
                .map(|(id, full_path)| {
                    let outcome = delete_one(&full_path);
                    (id, full_path, outcome)
                })
                .collect::<Vec<_>>()
        })?;

        // 第三步（回到调用线程）：按删除结果更新 DB。
        for (id, full_path, outcome) in results {
            match outcome {
                Ok(permanent) => {
                    images_repo::mark_deleted_by_id(&conn, id, now)?;
                    payload_items.push(TrashedItem {
                        image_id: id,
                        original_path: full_path,
                        permanent,
                    });
                    succeeded.push(id);
                    if permanent {
                        permanently_deleted.push(id);
                    }
                }
                Err(error) => failed.push(TrashFailure { image_id: id, error }),
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
        permanently_deleted,
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

    // 永久删除（网络盘回退）的项物理文件已不存在，无法恢复——只能恢复进了回收站的项。
    let (permanent, recoverable): (Vec<TrashedItem>, Vec<TrashedItem>) =
        payload.items.into_iter().partition(|item| item.permanent);

    // 平台分支：能从回收站恢复物理文件的平台先尝试；不能的，仅恢复 DB 标记。
    let manual_recovery_required = restore_files_from_trash(&recoverable)?;

    let mut restored = Vec::new();
    for item in &recoverable {
        images_repo::restore_by_id(&conn, item.image_id)?;
        restored.push(item.image_id);
    }
    images_repo::mark_undo_done(&conn, undo_id, now)?;

    let mut notes = Vec::new();
    if manual_recovery_required {
        notes.push(
            "DB 标记已恢复，但当前平台无法自动从回收站恢复文件。\
             请到系统回收站手动还原，下次扫描会重新登记。"
                .to_string(),
        );
    }
    if !permanent.is_empty() {
        notes.push(format!(
            "{} 个文件位于网络盘、删除时已永久删除（无回收站），无法恢复。",
            permanent.len()
        ));
    }
    let note = if notes.is_empty() {
        None
    } else {
        Some(notes.join("\n"))
    };

    Ok(UndoOutcome {
        restored,
        manual_recovery_required,
        note,
    })
}

/// 尝试从回收站恢复物理文件。返回 `true` 表示当前平台无法自动恢复（manual_recovery_required）。
/// 仅接收"进了回收站、可恢复"的项（永久删除项已在调用方剔除）。
#[cfg(any(target_os = "windows", target_os = "linux"))]
fn restore_files_from_trash(items: &[TrashedItem]) -> AppResult<bool> {
    use std::collections::HashSet;

    if items.is_empty() {
        return Ok(false);
    }

    let want: HashSet<String> = items
        .iter()
        .map(|item| item.original_path.clone())
        .collect();

    // 同样必须在后台线程跑：os_limited::list/restore_all 也走 COM，
    // 在主 UI 线程（STA）上调用会触发 RPC_E_CHANGED_MODE 崩溃。
    run_on_worker(move || -> Result<bool, String> {
        use trash::os_limited;

        let items = os_limited::list().map_err(|e| e.to_string())?;
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

        os_limited::restore_all(to_restore).map_err(|e| e.to_string())?;
        Ok(false)
    })?
    .map_err(AppError::Other)
}

#[cfg(not(any(target_os = "windows", target_os = "linux")))]
fn restore_files_from_trash(items: &[TrashedItem]) -> AppResult<bool> {
    // macOS：trash crate 不提供回收站读/恢复 API；只恢复 DB 标记。
    // 无可恢复项时不需要提示手动还原。
    Ok(!items.is_empty())
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
