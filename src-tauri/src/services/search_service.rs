//! 搜索服务：自然语言 / DSL 搜图与以图搜图（docs/10 §8，A1 搜索正确性修复）。
//!
//! 命令层（`commands::ai`）只做参数转发，逻辑都在这里：
//! 1. 解析查询：`#标签`、`format:`、`size:` 是过滤条件，其余文字走语义检索；
//! 2. 过滤条件（目录 / 格式 / 大小 / 标签 / 删除状态）一律先在 SQL 里求候选集，各召回通道
//!    只在候选集内检索（LanceDB 用 prefilter）——不再"先取全局前 N 条再在内存里筛"；
//! 3. worker 返回兜底伪向量时跳过语义通道，退回文件名匹配，且保留全部过滤条件。

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use crate::config::clip_model_dims;
use crate::db::Pool;
use crate::error::{AppError, AppResult};
use crate::repo::images_repo::{self, ImageQueryParams, ImageRecord};
use crate::repo::lancedb_repo::{LanceDbRepo, VectorSearchHit, VectorTable};
use crate::services::ai_client::ClipEmbedItem;
use crate::services::ai_supervisor::AiSupervisor;
use crate::services::thumbnail_service;

pub const DEFAULT_LIMIT: usize = 40;
pub const MAX_LIMIT: usize = 200;
/// 向量通道多取几倍再水合：向量库里可能残留已删除图片的向量，水合时会被丢掉。
const VECTOR_OVERFETCH: usize = 5;

/// 结果来源（`AiSearchResult::source`，前端按它区分展示）。
pub const SOURCE_DSL: &str = "dsl";
pub const SOURCE_SEMANTIC: &str = "semantic";
pub const SOURCE_FILENAME: &str = "filename";
pub const SOURCE_IMAGE: &str = "image";

const SCORE_DSL: f32 = 1.0;
const SCORE_FILENAME: f32 = 0.5;

/// `ai_search` 命令参数（JSON：`{ text, rootIds?, limit? }`）。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AiSearchArgs {
    pub text: String,
    pub root_ids: Option<Vec<i64>>,
    pub limit: Option<usize>,
}

/// 搜索结果（JSON：`{ image, score, source }`）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AiSearchResult {
    pub image: ImageRecord,
    pub score: f32,
    pub source: String,
}

/// 查询向量的来源。线上是 Python worker（[`WorkerQueryEncoder`]），测试里换成假实现。
///
/// worker 缺 torch / 模型权重时不会报错，而是返回 `fallback: true` 的哈希伪向量——它和
/// 查询内容无关，拿去检索只会得到随机结果，所以这种情况必须返回 `Ok(None)`，由调用方跳过语义通道。
#[async_trait]
pub trait QueryEncoder: Send + Sync {
    /// 图像向量模型（设置项 `aiClipModel`），决定查哪张向量表（[`VectorTable::image_clip`]）。
    fn clip_model(&self) -> &str;

    /// 文本 → 图像向量空间里的查询向量；worker 处于兜底模式时返回 `Ok(None)`。
    async fn encode_text(&self, text: &str) -> AppResult<Option<Vec<f32>>>;

    /// 缩略图 → 图像向量（以图搜图、库里还没有该图向量时用）；兜底模式返回 `Ok(None)`。
    async fn encode_image(&self, image_id: i64, thumb_path: String) -> AppResult<Option<Vec<f32>>>;
}

/// 线上实现：经 [`AiSupervisor`] 调 Python worker（worker 没启动时会先拉起）。
pub struct WorkerQueryEncoder {
    supervisor: Arc<AiSupervisor>,
    clip_model: String,
}

impl WorkerQueryEncoder {
    pub fn new(supervisor: Arc<AiSupervisor>, clip_model: String) -> Self {
        Self {
            supervisor,
            clip_model,
        }
    }

    /// worker 回的模型和请求的不一致时不用它的向量：维度相同也不在同一个向量空间里。
    fn check_model(&self, model: &str) -> AppResult<()> {
        if model == self.clip_model {
            Ok(())
        } else {
            Err(AppError::Other(format!(
                "AI worker 返回的图像向量模型 {model} 与设置的 {} 不一致",
                self.clip_model
            )))
        }
    }
}

#[async_trait]
impl QueryEncoder for WorkerQueryEncoder {
    fn clip_model(&self) -> &str {
        &self.clip_model
    }

    async fn encode_text(&self, text: &str) -> AppResult<Option<Vec<f32>>> {
        let response = self
            .supervisor
            .encode_text(text.to_string(), Some(self.clip_model.clone()))
            .await?;
        if response.fallback {
            tracing::info!(
                model = %response.model,
                "CLIP 文本编码处于兜底模式（伪向量），本次搜索跳过语义通道"
            );
            return Ok(None);
        }
        self.check_model(&response.model)?;
        Ok(Some(response.embedding))
    }

    async fn encode_image(&self, image_id: i64, thumb_path: String) -> AppResult<Option<Vec<f32>>> {
        let response = self
            .supervisor
            .embed_images(
                vec![ClipEmbedItem {
                    id: image_id,
                    thumb_path,
                }],
                Some(self.clip_model.clone()),
            )
            .await?;
        if response.fallback {
            tracing::info!(
                model = %response.model,
                image_id,
                "CLIP 图像编码处于兜底模式（伪向量），不做以图搜图"
            );
            return Ok(None);
        }
        self.check_model(&response.model)?;
        let item = response
            .items
            .into_iter()
            .next()
            .ok_or_else(|| AppError::Other("AI worker returned no embedding".to_string()))?;
        match item.embedding {
            Some(vector) => Ok(Some(vector)),
            None => {
                Err(AppError::Other(item.error.unwrap_or_else(|| {
                    "AI worker embedding failed".to_string()
                })))
            }
        }
    }
}

/// 自然语言 + DSL 搜索。
///
/// - 只有过滤条件（`#标签` / `format:` / `size:`）：整个查询（过滤 + 默认排序 + LIMIT）在 SQL 里完成，
///   不调用 worker；
/// - 有自由文本：有过滤条件时先在 SQL 里求候选集，再在候选集内做向量检索（LanceDB prefilter）；
///   语义通道没有结果（或 worker 处于兜底模式）时退回文件名匹配，过滤条件一个不丢。
pub async fn search(
    pool: &Pool,
    vectors: &LanceDbRepo,
    encoder: &dyn QueryEncoder,
    args: &AiSearchArgs,
) -> AppResult<Vec<AiSearchResult>> {
    let limit = clamp_limit(args.limit);
    let filter = parse_search_filter(&args.text);
    if filter.is_empty() {
        return Ok(Vec::new());
    }
    let root_ids = args.root_ids.clone().filter(|ids| !ids.is_empty());

    if filter.text.is_empty() {
        let conn = pool.get()?;
        return dsl_search(&conn, &filter, root_ids, limit);
    }

    // 有过滤条件就先求候选集。候选集为空时任何通道都不可能命中（文件名兜底只会更窄），
    // 直接返回，连 worker 都不用唤醒。
    let candidates = if root_ids.is_some() || filter.has_filter() {
        let conn = pool.get()?;
        let ids =
            images_repo::filtered_ids(&conn, &query_params(&filter, root_ids.clone(), None, None))?;
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        Some(ids)
    } else {
        None
    };

    let mut results = match encoder.encode_text(&filter.text).await? {
        Some(embedding) => {
            let table = VectorTable::image_clip(encoder.clip_model(), embedding.len());
            let hits = vectors
                .top_k_filtered(
                    &table,
                    &embedding,
                    limit.saturating_mul(VECTOR_OVERFETCH),
                    candidates.as_deref(),
                )
                .await?;
            let conn = pool.get()?;
            hydrate_hits(&conn, &hits, SOURCE_SEMANTIC, limit, None)?
        }
        // 兜底伪向量与查询词无关：不拿它检索，当作语义通道无结果。
        None => Vec::new(),
    };

    if results.is_empty() {
        let conn = pool.get()?;
        results = filename_search(&conn, &filter, root_ids, limit)?;
    }
    Ok(results)
}

/// 以图搜图：优先用库里已有的向量；没有就临时让 worker 算一次（不入库）。
/// worker 处于兜底模式时返回空结果——伪向量的"相似"没有意义。
pub async fn search_by_image(
    pool: &Pool,
    vectors: &LanceDbRepo,
    encoder: &dyn QueryEncoder,
    thumbs_dir: &Path,
    image_id: i64,
    limit: Option<usize>,
) -> AppResult<Vec<AiSearchResult>> {
    let limit = clamp_limit(limit);
    let model = encoder.clip_model();
    let stored_table = VectorTable::image_clip(model, clip_model_dims(model));
    let embedding = match vectors.embedding_for_image(&stored_table, image_id).await? {
        Some(vector) => vector,
        _ => {
            let thumb_path = {
                let conn = pool.get()?;
                let detail = images_repo::get_detail(&conn, image_id)?
                    .ok_or_else(|| AppError::Other(format!("image not found: {image_id}")))?;
                thumb_path_for(thumbs_dir, &detail)?
            };
            match encoder.encode_image(image_id, thumb_path).await? {
                Some(vector) => vector,
                None => return Ok(Vec::new()),
            }
        }
    };
    let table = VectorTable::image_clip(model, embedding.len());
    let hits = vectors
        .top_k(
            &table,
            &embedding,
            limit.saturating_mul(VECTOR_OVERFETCH) + 1,
        )
        .await?;
    let conn = pool.get()?;
    hydrate_hits(&conn, &hits, SOURCE_IMAGE, limit, Some(image_id))
}

/// 解析后的搜索条件。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SearchFilter {
    /// 去掉 DSL token 后剩下的自由文本（空格连接）。
    pub text: String,
    /// `#标签`（AND 语义）。
    pub tags: Vec<String>,
    /// `format:heic`，小写、去掉前导点。
    pub formats: Option<Vec<String>>,
    /// `size:>5MB` / `size:>=5MB`（字节，含边界）。
    pub size_min: Option<i64>,
    /// `size:<1MB` / `size:<=1MB`（字节，含边界）。
    pub size_max: Option<i64>,
}

impl SearchFilter {
    /// 有没有过滤条件（标签 / 格式 / 大小；目录由调用方另给）。
    pub fn has_filter(&self) -> bool {
        !self.tags.is_empty() || self.has_metadata_filter()
    }

    /// `format:` / `size:` 这类元数据条件。
    pub fn has_metadata_filter(&self) -> bool {
        self.formats.as_ref().is_some_and(|items| !items.is_empty())
            || self.size_min.is_some()
            || self.size_max.is_some()
    }

    /// 既没有文本也没有任何条件（空查询）。
    pub fn is_empty(&self) -> bool {
        self.text.is_empty() && !self.has_filter()
    }
}

/// 把输入拆成 DSL 条件与自由文本：`#tag`、`format:xxx`、`size:>5MB` / `size:<=1.5GB`，其余是文本。
/// 无法识别的 `size:` 值整段忽略；空的 `#` / `format:` 当普通文本。
pub fn parse_search_filter(input: &str) -> SearchFilter {
    let mut filter = SearchFilter::default();
    let mut text = Vec::new();
    let mut formats = Vec::new();

    for token in input.split_whitespace() {
        if let Some(tag) = token.strip_prefix('#').filter(|tag| !tag.is_empty()) {
            if !filter.tags.iter().any(|existing| existing == tag) {
                filter.tags.push(tag.to_string());
            }
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
        text.push(token);
    }

    if !formats.is_empty() {
        filter.formats = Some(formats);
    }
    filter.text = text.join(" ");
    filter
}

/// `5MB` / `1.5gb` / `300KB` / `1024` → 字节数（1KB = 1024B）。负数、非数字返回 `None`。
pub fn parse_size_bytes(raw: &str) -> Option<i64> {
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

fn apply_size_filter(filter: &mut SearchFilter, token: &str) {
    let (is_min, raw) = if let Some(rest) = token.strip_prefix(">=") {
        (true, rest)
    } else if let Some(rest) = token.strip_prefix("<=") {
        (false, rest)
    } else if let Some(rest) = token.strip_prefix('>') {
        (true, rest)
    } else if let Some(rest) = token.strip_prefix('<') {
        (false, rest)
    } else {
        return;
    };
    let Some(bytes) = parse_size_bytes(raw) else {
        return;
    };
    if is_min {
        filter.size_min = Some(bytes);
    } else {
        filter.size_max = Some(bytes);
    }
}

fn clamp_limit(limit: Option<usize>) -> usize {
    limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT)
}

/// 搜索条件 → `images_repo` 查询参数（未删除、默认排序 mtime DESC）。
fn query_params(
    filter: &SearchFilter,
    root_ids: Option<Vec<i64>>,
    filename_like: Option<String>,
    limit: Option<usize>,
) -> ImageQueryParams {
    ImageQueryParams {
        root_ids,
        formats: filter.formats.clone(),
        q: filename_like,
        size_min: filter.size_min,
        size_max: filter.size_max,
        tag_names_all: (!filter.tags.is_empty()).then(|| filter.tags.clone()),
        offset: Some(0),
        limit: limit.map(|limit| limit as i64),
        include_deleted: Some(false),
        ..ImageQueryParams::default()
    }
}

/// 纯 DSL 查询：过滤、排序、LIMIT 全在 SQL 里，不会只看默认排序的前几百行。
fn dsl_search(
    conn: &Connection,
    filter: &SearchFilter,
    root_ids: Option<Vec<i64>>,
    limit: usize,
) -> AppResult<Vec<AiSearchResult>> {
    let page = images_repo::query(conn, &query_params(filter, root_ids, None, Some(limit)))?;
    Ok(into_results(page.items, SCORE_DSL, SOURCE_DSL))
}

/// 语义通道无结果时的兜底：文件名包含查询文本，且仍满足全部过滤条件（含标签）。
fn filename_search(
    conn: &Connection,
    filter: &SearchFilter,
    root_ids: Option<Vec<i64>>,
    limit: usize,
) -> AppResult<Vec<AiSearchResult>> {
    let params = query_params(filter, root_ids, Some(filter.text.clone()), Some(limit));
    let page = images_repo::query(conn, &params)?;
    Ok(into_results(page.items, SCORE_FILENAME, SOURCE_FILENAME))
}

fn into_results(images: Vec<ImageRecord>, score: f32, source: &str) -> Vec<AiSearchResult> {
    images
        .into_iter()
        .map(|image| AiSearchResult {
            image,
            score,
            source: source.to_string(),
        })
        .collect()
}

/// 向量命中 → 完整图片记录，保持命中顺序（近的在前），丢掉已删除 / 不存在的图，最多 `limit` 条。
/// 按 `limit` 分块水合：通常一次查询就够，不必把多取的命中全部读出来。
fn hydrate_hits(
    conn: &Connection,
    hits: &[VectorSearchHit],
    source: &str,
    limit: usize,
    exclude_id: Option<i64>,
) -> AppResult<Vec<AiSearchResult>> {
    let mut seen = HashSet::with_capacity(hits.len());
    let ranked = hits
        .iter()
        .filter(|hit| Some(hit.image_id) != exclude_id && seen.insert(hit.image_id))
        .collect::<Vec<_>>();
    let mut results = Vec::with_capacity(limit.min(ranked.len()));
    for chunk in ranked.chunks(limit.max(1)) {
        let ids = chunk.iter().map(|hit| hit.image_id).collect::<Vec<_>>();
        let mut by_id = images_repo::get_details_by_ids(conn, &ids)?
            .into_iter()
            .map(|image| (image.id, image))
            .collect::<HashMap<_, _>>();
        for hit in chunk {
            let Some(image) = by_id.remove(&hit.image_id) else {
                continue;
            };
            results.push(AiSearchResult {
                image,
                score: hit.score,
                source: source.to_string(),
            });
            if results.len() >= limit {
                return Ok(results);
            }
        }
    }
    Ok(results)
}

fn thumb_path_for(thumbs_dir: &Path, image: &ImageRecord) -> AppResult<String> {
    let hash = image
        .thumb_hash
        .as_deref()
        .ok_or_else(|| AppError::Other("缩略图尚未生成，无法运行 AI".to_string()))?;
    Ok(thumbnail_service::thumb_path(thumbs_dir, hash)
        .to_string_lossy()
        .to_string())
}
