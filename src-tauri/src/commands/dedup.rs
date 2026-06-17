use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, State};

use crate::config::AppPaths;
use crate::db::Pool;
use crate::queue::Scheduler;
use crate::repo::{duplicates_repo, images_repo};
use crate::services::dedup_service::{
    self, DedupAction, DedupBatchResolveArgs, DedupBatchResolveResult, DedupCoordinator,
    DedupMethod, DedupStatus, VISUAL_THRESHOLD_DEFAULT,
};
use crate::services::trash_service;
use crate::utils::bk_tree::DhashIndex;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DedupStartArgs {
    pub method: DedupMethod,
    pub threshold: Option<u32>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DedupStartResult {
    pub started: bool,
    pub status: DedupStatus,
}

#[tauri::command]
pub fn dedup_start(
    pool: State<'_, Pool>,
    scheduler: State<'_, Scheduler>,
    coord: State<'_, Arc<DedupCoordinator>>,
    index: State<'_, Arc<DhashIndex>>,
    paths: State<'_, AppPaths>,
    app: AppHandle,
    args: DedupStartArgs,
) -> Result<DedupStartResult, String> {
    let threshold = args.threshold.unwrap_or(VISUAL_THRESHOLD_DEFAULT);
    if coord.status().running {
        return Ok(DedupStartResult {
            started: false,
            status: coord.status(),
        });
    }
    dedup_service::start_dedup(
        pool.inner().clone(),
        scheduler.inner().clone(),
        coord.inner().clone(),
        index.inner().clone(),
        paths.inner().thumbs_dir.clone(),
        args.method,
        threshold,
        app,
    )
    .map_err(|e| e.to_string())?;
    Ok(DedupStartResult {
        started: true,
        status: coord.status(),
    })
}

#[tauri::command]
pub fn dedup_status(coord: State<'_, Arc<DedupCoordinator>>) -> DedupStatus {
    coord.status()
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct DedupGroupsArgs {
    pub method: Option<String>,
    pub status: Option<String>,
    pub offset: Option<i64>,
    pub limit: Option<i64>,
}

#[tauri::command]
pub fn dedup_groups(
    pool: State<'_, Pool>,
    args: DedupGroupsArgs,
) -> Result<duplicates_repo::GroupPage, String> {
    let conn = pool.get().map_err(|e| e.to_string())?;
    duplicates_repo::list_groups(
        &conn,
        &duplicates_repo::GroupQueryParams {
            method: args.method,
            status: args.status,
            offset: args.offset,
            limit: args.limit,
        },
    )
    .map_err(|e| e.to_string())
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DedupGroupDetail {
    pub group: duplicates_repo::DuplicateGroup,
    pub items: Vec<DedupItemDetail>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DedupItemDetail {
    pub image: images_repo::ImageRecord,
    pub similarity: Option<f64>,
}

#[tauri::command]
pub fn dedup_group_detail(
    pool: State<'_, Pool>,
    group_id: i64,
) -> Result<Option<DedupGroupDetail>, String> {
    let conn = pool.get().map_err(|e| e.to_string())?;
    let group = match duplicates_repo::get_group(&conn, group_id).map_err(|e| e.to_string())? {
        Some(g) => g,
        None => return Ok(None),
    };
    let items_raw = duplicates_repo::items_for_group(&conn, group_id).map_err(|e| e.to_string())?;
    let mut items = Vec::with_capacity(items_raw.len());
    for item in items_raw {
        let image = images_repo::get_detail(&conn, item.image_id)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("image {} not found", item.image_id))?;
        items.push(DedupItemDetail {
            image,
            similarity: item.similarity,
        });
    }
    Ok(Some(DedupGroupDetail { group, items }))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DedupResolveArgs {
    pub group_id: i64,
    pub keep_image_ids: Vec<i64>,
    pub action: DedupAction,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DedupResolveResult {
    pub group_id: i64,
    pub trashed: Vec<i64>,
    /// `trashed` 的子集：网络盘无回收站，已永久删除（不可撤销恢复）。
    pub permanently_deleted: Vec<i64>,
    pub trash_failures: Vec<trash_service::TrashFailure>,
    pub undo_id: Option<i64>,
}

#[tauri::command]
pub fn dedup_resolve(
    pool: State<'_, Pool>,
    args: DedupResolveArgs,
) -> Result<DedupResolveResult, String> {
    let now = now_unix();
    let conn = pool.get().map_err(|e| e.to_string())?;
    let group = duplicates_repo::get_group(&conn, args.group_id)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("分组不存在：{}", args.group_id))?;

    let items =
        duplicates_repo::items_for_group(&conn, args.group_id).map_err(|e| e.to_string())?;
    let keep_set: std::collections::HashSet<i64> = args.keep_image_ids.iter().copied().collect();

    let (status_after, keep_image_id, to_trash) = match args.action {
        DedupAction::Trash => {
            let to_trash: Vec<i64> = items
                .iter()
                .map(|i| i.image_id)
                .filter(|id| !keep_set.contains(id))
                .collect();
            let keep_id = args.keep_image_ids.first().copied();
            ("resolved", keep_id, to_trash)
        }
        DedupAction::KeepAll => ("resolved", None, Vec::new()),
        DedupAction::Dismiss => ("dismissed", None, Vec::new()),
    };

    drop(conn);

    let outcome = if to_trash.is_empty() {
        empty_trash_outcome()
    } else {
        trash_service::trash_images(&pool, &to_trash, now).map_err(|e| e.to_string())?
    };

    let conn = pool.get().map_err(|e| e.to_string())?;
    if args.action == DedupAction::Trash && !outcome.failed.is_empty() {
        if !outcome.succeeded.is_empty() {
            duplicates_repo::remove_items(&conn, group.id, &outcome.succeeded)
                .map_err(|e| e.to_string())?;
        }
    } else {
        duplicates_repo::update_group_status(
            &conn,
            group.id,
            status_after,
            keep_image_id,
            Some(now),
        )
        .map_err(|e| e.to_string())?;
    }

    Ok(DedupResolveResult {
        group_id: group.id,
        trashed: outcome.succeeded,
        permanently_deleted: outcome.permanently_deleted,
        trash_failures: outcome.failed,
        undo_id: outcome.undo_id,
    })
}

#[tauri::command]
pub fn dedup_batch_resolve(
    pool: State<'_, Pool>,
    args: DedupBatchResolveArgs,
) -> Result<DedupBatchResolveResult, String> {
    dedup_service::batch_resolve(pool.inner(), args, now_unix()).map_err(|e| e.to_string())
}

fn empty_trash_outcome() -> trash_service::TrashOutcome {
    trash_service::TrashOutcome {
        succeeded: Vec::new(),
        permanently_deleted: Vec::new(),
        failed: Vec::new(),
        undo_id: None,
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DedupExportArgs {
    pub save_path: String,
    pub method: Option<String>,
    pub status: Option<String>,
}

#[tauri::command]
pub fn dedup_export_csv(pool: State<'_, Pool>, args: DedupExportArgs) -> Result<i64, String> {
    let conn = pool.get().map_err(|e| e.to_string())?;
    let groups = duplicates_repo::list_groups(
        &conn,
        &duplicates_repo::GroupQueryParams {
            method: args.method,
            status: args.status,
            offset: Some(0),
            limit: Some(500),
        },
    )
    .map_err(|e| e.to_string())?;

    let mut rows: Vec<String> = Vec::new();
    // Excel 友好 UTF-8 BOM
    rows.push("\u{feff}group_id,method,similarity,image_id,root,rel_path,filename,size,width,height,mtime,blake3,phash,kept".to_string());
    let mut written = 0i64;
    for group in &groups.items {
        let items = duplicates_repo::items_for_group(&conn, group.id).map_err(|e| e.to_string())?;
        for item in items {
            let image = images_repo::get_detail(&conn, item.image_id)
                .map_err(|e| e.to_string())?
                .ok_or_else(|| format!("image {} not found", item.image_id))?;
            let kept = group
                .keep_image_id
                .map(|k| k == item.image_id)
                .unwrap_or(false);
            rows.push(format!(
                "{},{},{},{},{},{},{},{},{},{},{},{},{},{}",
                group.id,
                group.method,
                item.similarity
                    .map(|s| format!("{s:.4}"))
                    .unwrap_or_default(),
                image.id,
                csv_escape(&image.root_path),
                csv_escape(&image.rel_path),
                csv_escape(&image.filename),
                image.size_bytes,
                image.width.map(|v| v.to_string()).unwrap_or_default(),
                image.height.map(|v| v.to_string()).unwrap_or_default(),
                image.mtime,
                image.blake3.clone().unwrap_or_default(),
                image
                    .phash
                    .map(|v| (v as u64).to_string())
                    .unwrap_or_default(),
                if kept { "1" } else { "0" }
            ));
            written += 1;
        }
    }

    std::fs::write(PathBuf::from(&args.save_path), rows.join("\n")).map_err(|e| e.to_string())?;
    Ok(written)
}

fn csv_escape(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
