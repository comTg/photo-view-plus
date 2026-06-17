use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tauri::{AppHandle, Emitter, State};

use crate::config::AppPaths;
use crate::db::Pool;
use crate::repo::images_repo::{self, ImageRecord};
use crate::repo::tags_repo::{self, ImageTag, NewTagScore, Tag};
use crate::services::ai_client::{ClipEmbedItem, TaggerItem};
use crate::services::ai_pipeline::{AiPipeline, AiPipelineStatus};
use crate::services::ai_supervisor::{AiSupervisor, AiWorkerStatus};
use crate::services::thumbnail_service;

#[tauri::command]
pub async fn ai_worker_start(
    app: AppHandle,
    supervisor: State<'_, Arc<AiSupervisor>>,
) -> Result<AiWorkerStatus, String> {
    let status = supervisor.start().await.map_err(|e| e.to_string())?;
    let _ = app.emit("ai:worker_status", status.clone());
    Ok(status)
}

#[tauri::command]
pub async fn ai_worker_stop(
    app: AppHandle,
    supervisor: State<'_, Arc<AiSupervisor>>,
) -> Result<AiWorkerStatus, String> {
    let status = supervisor.stop().await.map_err(|e| e.to_string())?;
    let _ = app.emit("ai:worker_status", status.clone());
    Ok(status)
}

#[tauri::command]
pub async fn ai_worker_status(
    supervisor: State<'_, Arc<AiSupervisor>>,
) -> Result<AiWorkerStatus, String> {
    supervisor.status().await.map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn ai_worker_diagnostics(
    supervisor: State<'_, Arc<AiSupervisor>>,
) -> Result<Value, String> {
    supervisor.diagnostics().await.map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn ai_models_status(supervisor: State<'_, Arc<AiSupervisor>>) -> Result<Value, String> {
    supervisor.models_status().await.map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn ai_model_download(
    supervisor: State<'_, Arc<AiSupervisor>>,
    model_key: String,
) -> Result<Value, String> {
    supervisor
        .model_download(&model_key)
        .await
        .map_err(|e| e.to_string())
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AiSearchArgs {
    pub text: String,
    pub root_ids: Option<Vec<i64>>,
    pub limit: Option<usize>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AiSearchResult {
    pub image: ImageRecord,
    pub score: f32,
    pub source: String,
}

#[tauri::command]
pub async fn ai_search(
    pool: State<'_, Pool>,
    pipeline: State<'_, Arc<AiPipeline>>,
    supervisor: State<'_, Arc<AiSupervisor>>,
    args: AiSearchArgs,
) -> Result<Vec<AiSearchResult>, String> {
    let limit = args.limit.unwrap_or(40).clamp(1, 200);
    let filter = parse_search_filter(&args.text);
    if filter.text.is_empty() && filter.tags.is_empty() && !filter.has_sql_filter() {
        return Ok(Vec::new());
    }
    let root_filter = args
        .root_ids
        .as_ref()
        .filter(|ids| !ids.is_empty())
        .map(|ids| {
            ids.iter()
                .copied()
                .collect::<std::collections::HashSet<_>>()
        });
    let conn = pool.get().map_err(|e| e.to_string())?;
    let tag_filter_ids = tagged_image_ids(&conn, &filter.tags).map_err(|e| e.to_string())?;
    drop(conn);

    if filter.text.is_empty() {
        let conn = pool.get().map_err(|e| e.to_string())?;
        let page = images_repo::query(
            &conn,
            &images_repo::ImageQueryParams {
                root_ids: args.root_ids.clone(),
                formats: filter.formats.clone(),
                q: None,
                size_min: filter.size_min,
                size_max: filter.size_max,
                taken_from: None,
                taken_to: None,
                has_gps: None,
                sort: None,
                offset: Some(0),
                limit: Some((limit * 5) as i64),
                include_deleted: Some(false),
            },
        )
        .map_err(|e| e.to_string())?;
        return Ok(page
            .items
            .into_iter()
            .filter(|image| {
                tag_filter_ids
                    .as_ref()
                    .map(|ids| ids.contains(&image.id))
                    .unwrap_or(true)
                    && image_matches_filter(image, &filter)
            })
            .take(limit)
            .map(|image| AiSearchResult {
                image,
                score: 1.0,
                source: "dsl".to_string(),
            })
            .collect());
    }

    let encoded = supervisor
        .encode_text(filter.text.clone())
        .await
        .map_err(|e| e.to_string())?;
    let hits = pipeline
        .vectors()
        .top_k(&encoded.embedding, limit * 5)
        .await
        .map_err(|e| e.to_string())?;
    let conn = pool.get().map_err(|e| e.to_string())?;
    let ids = hits.iter().map(|hit| hit.image_id).collect::<Vec<_>>();
    let details = images_repo::get_details_by_ids(&conn, &ids).map_err(|e| e.to_string())?;
    let score_by_id = hits
        .iter()
        .map(|hit| (hit.image_id, hit.score))
        .collect::<std::collections::HashMap<_, _>>();
    let mut results = details
        .into_iter()
        .filter(|image| {
            root_filter
                .as_ref()
                .map(|filter| filter.contains(&image.root_id))
                .unwrap_or(true)
                && tag_filter_ids
                    .as_ref()
                    .map(|ids| ids.contains(&image.id))
                    .unwrap_or(true)
                && image_matches_filter(image, &filter)
        })
        .map(|image| AiSearchResult {
            score: *score_by_id.get(&image.id).unwrap_or(&0.0),
            image,
            source: "semantic".to_string(),
        })
        .take(limit)
        .collect::<Vec<_>>();

    if results.is_empty() {
        let page = images_repo::query(
            &conn,
            &images_repo::ImageQueryParams {
                root_ids: args.root_ids,
                formats: filter.formats,
                q: Some(filter.text),
                size_min: filter.size_min,
                size_max: filter.size_max,
                taken_from: None,
                taken_to: None,
                has_gps: None,
                sort: None,
                offset: Some(0),
                limit: Some(limit as i64),
                include_deleted: Some(false),
            },
        )
        .map_err(|e| e.to_string())?;
        results = page
            .items
            .into_iter()
            .map(|image| AiSearchResult {
                image,
                score: 0.5,
                source: "filename".to_string(),
            })
            .collect();
    }

    Ok(results)
}

#[derive(Debug, Default)]
struct SearchFilter {
    text: String,
    tags: Vec<String>,
    formats: Option<Vec<String>>,
    size_min: Option<i64>,
    size_max: Option<i64>,
}

impl SearchFilter {
    fn has_sql_filter(&self) -> bool {
        self.formats.as_ref().is_some_and(|items| !items.is_empty())
            || self.size_min.is_some()
            || self.size_max.is_some()
    }
}

fn parse_search_filter(input: &str) -> SearchFilter {
    let mut filter = SearchFilter::default();
    let mut text = Vec::new();
    let mut formats = Vec::new();

    for token in input.split_whitespace() {
        if let Some(tag) = token.strip_prefix('#').filter(|tag| !tag.is_empty()) {
            filter.tags.push(tag.to_string());
            continue;
        }
        if let Some(format) = token
            .strip_prefix("format:")
            .filter(|value| !value.is_empty())
        {
            formats.push(format.trim_start_matches('.').to_ascii_lowercase());
            continue;
        }
        if let Some(size) = token.strip_prefix("size:") {
            apply_size_filter(&mut filter, size);
            continue;
        }
        text.push(token.to_string());
    }

    if !formats.is_empty() {
        filter.formats = Some(formats);
    }
    filter.text = text.join(" ").trim().to_string();
    filter
}

fn apply_size_filter(filter: &mut SearchFilter, token: &str) {
    let (op, raw) = if let Some(rest) = token.strip_prefix(">=") {
        (">=", rest)
    } else if let Some(rest) = token.strip_prefix("<=") {
        ("<=", rest)
    } else if let Some(rest) = token.strip_prefix('>') {
        (">", rest)
    } else if let Some(rest) = token.strip_prefix('<') {
        ("<", rest)
    } else {
        return;
    };
    let Some(bytes) = parse_size_bytes(raw) else {
        return;
    };
    match op {
        ">" | ">=" => filter.size_min = Some(bytes),
        "<" | "<=" => filter.size_max = Some(bytes),
        _ => {}
    }
}

fn parse_size_bytes(raw: &str) -> Option<i64> {
    let upper = raw.trim().to_ascii_uppercase();
    let (number, multiplier) = if let Some(value) = upper.strip_suffix("GB") {
        (value, 1024.0 * 1024.0 * 1024.0)
    } else if let Some(value) = upper.strip_suffix("MB") {
        (value, 1024.0 * 1024.0)
    } else if let Some(value) = upper.strip_suffix("KB") {
        (value, 1024.0)
    } else {
        (upper.as_str(), 1.0)
    };
    number
        .parse::<f64>()
        .ok()
        .filter(|value| value.is_finite() && *value >= 0.0)
        .map(|value| (value * multiplier).round() as i64)
}

fn image_matches_filter(image: &ImageRecord, filter: &SearchFilter) -> bool {
    if let Some(formats) = filter.formats.as_ref() {
        if !formats
            .iter()
            .any(|format| format.eq_ignore_ascii_case(&image.extension))
        {
            return false;
        }
    }
    if let Some(size_min) = filter.size_min {
        if image.size_bytes < size_min {
            return false;
        }
    }
    if let Some(size_max) = filter.size_max {
        if image.size_bytes > size_max {
            return false;
        }
    }
    true
}

fn tagged_image_ids(
    conn: &rusqlite::Connection,
    tag_names: &[String],
) -> crate::AppResult<Option<std::collections::HashSet<i64>>> {
    if tag_names.is_empty() {
        return Ok(None);
    }
    let placeholders = std::iter::repeat("?")
        .take(tag_names.len())
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        "SELECT it.image_id
         FROM image_tags it
         JOIN tags t ON t.id = it.tag_id
         WHERE t.name IN ({placeholders})
         GROUP BY it.image_id
         HAVING COUNT(DISTINCT t.name) = ?"
    );
    let mut values = tag_names
        .iter()
        .cloned()
        .map(rusqlite::types::Value::Text)
        .collect::<Vec<_>>();
    values.push(rusqlite::types::Value::Integer(tag_names.len() as i64));
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params_from_iter(values.iter()), |row| row.get(0))?;
    let mut ids = std::collections::HashSet::new();
    for row in rows {
        ids.insert(row?);
    }
    Ok(Some(ids))
}

#[tauri::command]
pub async fn ai_search_by_image(
    pool: State<'_, Pool>,
    paths: State<'_, AppPaths>,
    pipeline: State<'_, Arc<AiPipeline>>,
    supervisor: State<'_, Arc<AiSupervisor>>,
    image_id: i64,
    limit: Option<usize>,
) -> Result<Vec<AiSearchResult>, String> {
    let limit = limit.unwrap_or(40).clamp(1, 200);
    let vectors = pipeline.vectors();
    let embedding = match vectors
        .embedding_for_image(image_id)
        .await
        .map_err(|e| e.to_string())?
    {
        Some(vector) if !vector.is_empty() => vector,
        _ => embed_single_image(&pool, &paths, &supervisor, image_id)
            .await
            .map_err(|e| e.to_string())?,
    };
    let hits = vectors
        .top_k(&embedding, limit + 1)
        .await
        .map_err(|e| e.to_string())?;
    let ids = hits
        .iter()
        .map(|hit| hit.image_id)
        .filter(|id| *id != image_id)
        .take(limit)
        .collect::<Vec<_>>();
    let conn = pool.get().map_err(|e| e.to_string())?;
    let details = images_repo::get_details_by_ids(&conn, &ids).map_err(|e| e.to_string())?;
    let score_by_id = hits
        .iter()
        .map(|hit| (hit.image_id, hit.score))
        .collect::<std::collections::HashMap<_, _>>();
    Ok(details
        .into_iter()
        .map(|image| AiSearchResult {
            score: *score_by_id.get(&image.id).unwrap_or(&0.0),
            image,
            source: "image".to_string(),
        })
        .collect())
}

#[tauri::command]
pub async fn ai_tag_image(
    pool: State<'_, Pool>,
    paths: State<'_, AppPaths>,
    supervisor: State<'_, Arc<AiSupervisor>>,
    image_id: i64,
) -> Result<Vec<ImageTag>, String> {
    let detail = {
        let conn = pool.get().map_err(|e| e.to_string())?;
        images_repo::get_detail(&conn, image_id)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("image not found: {image_id}"))?
    };
    let thumb_path = thumb_path_for(&paths, &detail)?;
    let response = supervisor
        .tag_images(vec![TaggerItem {
            id: image_id,
            thumb_path,
        }])
        .await
        .map_err(|e| e.to_string())?;
    let item = response
        .items
        .into_iter()
        .next()
        .ok_or_else(|| "AI worker returned no tag result".to_string())?;
    if let Some(error) = item.error {
        return Err(error);
    }
    let now = now_unix();
    let mut conn = pool.get().map_err(|e| e.to_string())?;
    let tags = item
        .tags
        .into_iter()
        .map(|tag| NewTagScore {
            name: tag.name,
            score: tag.score,
            source: "ai".to_string(),
            category: tag.category,
        })
        .collect::<Vec<_>>();
    tags_repo::replace_image_tags(&mut conn, image_id, &tags, now).map_err(|e| e.to_string())?;
    images_repo::set_tag_status(&conn, image_id, "ready").map_err(|e| e.to_string())?;
    tags_repo::tags_for_image(&conn, image_id).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn ai_pipeline_status(pipeline: State<'_, Arc<AiPipeline>>) -> AiPipelineStatus {
    pipeline.status()
}

#[tauri::command]
pub async fn ai_process_pending(pipeline: State<'_, Arc<AiPipeline>>) -> Result<(), String> {
    pipeline.enqueue_pending().await.map_err(|e| e.to_string())
}

/// 清空旧的 AI 标签（含历史英文标签）并把就绪图片重置为待打标签，随后立即触发一轮重打。
/// 返回被重置的图片数。重打用的是当前 tagger（接入 RAM-plus 后即为中文）。
#[tauri::command]
pub async fn ai_retag_all(
    pool: State<'_, Pool>,
    pipeline: State<'_, Arc<AiPipeline>>,
) -> Result<usize, String> {
    let reset = {
        let mut conn = pool.get().map_err(|e| e.to_string())?;
        tags_repo::clear_ai_tags_and_reset(&mut conn).map_err(|e| e.to_string())?
    };
    // 清理已成功；尽量立刻起一轮重打（worker 未就绪时后台流水线稍后也会处理）。
    let _ = pipeline.enqueue_pending().await;
    Ok(reset)
}

#[tauri::command]
pub fn ai_tags_list(pool: State<'_, Pool>, limit: Option<i64>) -> Result<Vec<Tag>, String> {
    let conn = pool.get().map_err(|e| e.to_string())?;
    tags_repo::list_tags(&conn, limit.unwrap_or(100)).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn ai_image_tags(pool: State<'_, Pool>, image_id: i64) -> Result<Vec<ImageTag>, String> {
    let conn = pool.get().map_err(|e| e.to_string())?;
    tags_repo::tags_for_image(&conn, image_id).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn ai_images_by_tag(
    pool: State<'_, Pool>,
    tag_id: i64,
    offset: Option<i64>,
    limit: Option<i64>,
) -> Result<images_repo::ImagePage, String> {
    let conn = pool.get().map_err(|e| e.to_string())?;
    let limit = limit.unwrap_or(200).clamp(1, 500);
    let offset = offset.unwrap_or(0).max(0);
    let ids = images_repo::ids_for_tag(&conn, tag_id, limit, offset).map_err(|e| e.to_string())?;
    let items = images_repo::get_details_by_ids(&conn, &ids).map_err(|e| e.to_string())?;
    let total: i64 = conn
        .query_row(
            "SELECT COUNT(*)
             FROM image_tags it
             JOIN images i ON i.id = it.image_id
             WHERE it.tag_id = ?1 AND i.deleted_at IS NULL",
            [tag_id],
            |row| row.get(0),
        )
        .map_err(|e| e.to_string())?;
    Ok(images_repo::ImagePage {
        items,
        total,
        offset,
        limit,
    })
}

async fn embed_single_image(
    pool: &Pool,
    paths: &AppPaths,
    supervisor: &AiSupervisor,
    image_id: i64,
) -> crate::AppResult<Vec<f32>> {
    let detail = {
        let conn = pool.get()?;
        images_repo::get_detail(&conn, image_id)?
            .ok_or_else(|| crate::AppError::Other(format!("image not found: {image_id}")))?
    };
    let thumb_path = thumb_path_for(paths, &detail).map_err(crate::AppError::Other)?;
    let response = supervisor
        .embed_images(vec![ClipEmbedItem {
            id: image_id,
            thumb_path,
        }])
        .await?;
    let result = response
        .items
        .into_iter()
        .next()
        .ok_or_else(|| crate::AppError::Other("AI worker returned no embedding".to_string()))?;
    let vector = result.embedding.ok_or_else(|| {
        crate::AppError::Other(
            result
                .error
                .unwrap_or_else(|| "AI worker embedding failed".to_string()),
        )
    })?;
    Ok(vector)
}

fn thumb_path_for(paths: &AppPaths, image: &ImageRecord) -> Result<String, String> {
    let hash = image
        .thumb_hash
        .as_deref()
        .ok_or_else(|| "缩略图尚未生成，无法运行 AI".to_string())?;
    Ok(thumbnail_service::thumb_path(&paths.thumbs_dir, hash)
        .to_string_lossy()
        .to_string())
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
