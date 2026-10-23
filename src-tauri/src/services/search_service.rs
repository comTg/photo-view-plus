//! 搜索服务 v2：自然语言 / DSL 搜图与以图搜图（docs/10 §8；A1 正确性修复，A2 全文通道与融合）。
//!
//! 命令层（`commands::ai`）只做参数转发，逻辑都在这里：
//! 1. 解析查询：`#标签`、`format:`、`size:` 是过滤条件，其余文字是查询文本；
//! 2. 过滤条件（目录 / 格式 / 大小 / 标签 / 删除状态）在 SQL 里生效：文字通道把条件直接拼进查询，
//!    向量通道先在 SQL 里求候选集、再交给 LanceDB prefilter——不再"先取全局前 N 条再在内存里筛"；
//! 3. 多路召回，每路取前 [`CHANNEL_DEPTH`] 条：
//!    - 标签：查询里出现的标签名（"海边的小孩" 命中"海边""小孩"），按覆盖的查询字数排序；
//!    - 全文：`search_fts`（CJK 双字 + 单字，BM25 加列权重），先要求所有词都出现，命中太少再放宽为任一词；
//!    - 图像向量：Chinese-CLIP 文本向量 → 图像向量；
//!    - 描述文本向量（A4）；
//! 4. RRF 融合（score = Σ w_c / (60 + rank_c)），结果带命中来源（`matched_by`）和高亮片段；
//! 5. worker 没就绪或处于兜底模式时只跑文字通道，响应带 `degraded`；所有通道都没结果时退回文件名匹配。

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use rusqlite::types::Value;
use rusqlite::{params_from_iter, Connection};
use serde::{Deserialize, Serialize};

use crate::config::clip_model_dims;
use crate::db::Pool;
use crate::error::{AppError, AppResult};
use crate::repo::images_repo::{self, ImageQueryParams, ImageRecord};
use crate::repo::lancedb_repo::{LanceDbRepo, VectorSearchHit, VectorTable};
use crate::services::ai_client::ClipEmbedItem;
use crate::services::ai_supervisor::AiSupervisor;
use crate::services::search_index_service::{self, SearchTexts};
use crate::services::thumbnail_service;
use crate::utils::synonyms;
use crate::utils::text_tokens::{self, Tokens};

pub const DEFAULT_LIMIT: usize = 40;
pub const MAX_LIMIT: usize = 200;
/// 每路召回取前多少条参与融合（docs/10 §8.3）。
pub const CHANNEL_DEPTH: usize = 200;
/// 以图搜图多取几倍再水合：向量库里可能残留已删除图片的向量，水合时会被丢掉。
const VECTOR_OVERFETCH: usize = 5;
/// RRF 常数。
const RRF_K: f32 = 60.0;
/// 全文通道"所有词都出现"命中少于这个数时，放宽为"任一词出现"。
const FULLTEXT_AND_MIN_HITS: usize = 20;
/// 全文列权重，顺序与 search_fts 的列一致：tags、keywords、description、visible_text、ocr_text、filename、chars。
const FULLTEXT_COLUMN_WEIGHTS: [f64; 7] = [4.0, 3.0, 2.0, 1.5, 1.0, 1.0, 0.3];
/// 双字词、ASCII 词查这些列；单字只查 chars 列。
pub const FULLTEXT_TERM_COLUMNS: [&str; 6] = [
    "tags",
    "keywords",
    "description",
    "visible_text",
    "ocr_text",
    "filename",
];
/// 搜索前顺手处理的索引积压上限：积压更多时（刚升级、刚批量打完标签）交给后台循环，不拖慢这次搜索。
const SEARCH_FLUSH_MAX_PENDING: i64 = 500;
/// 搜索时最多等 worker 就绪多久；等不到就先返回文字通道的结果（degraded），worker 继续在后台启动。
pub const SEARCH_WORKER_WAIT: Duration = Duration::from_millis(1500);
/// Chinese-CLIP 图像向量通道的相似度下限（余弦）。它的图文相似度挤在 0.35–0.50 的窄带里：
/// 评测集 40 个查询前 20 名中相关图的 p5 是 0.397、每个查询的第 1 名都 ≥ 0.418，下限只能保守地切掉长尾。
pub const IMAGE_SIMILARITY_FLOOR: f32 = 0.37;
/// 描述向量通道（bge-small-zh）的相似度下限（余弦）：评测集里相关图的 p5 是 0.359、不相关图的
/// 中位数是 0.373，取 0.35 只切掉明显无关的长尾（docs/10 A4）。
pub const TEXT_SIMILARITY_FLOOR: f32 = 0.35;
/// 片段长度与命中词前保留的字数。
const SNIPPET_CHARS: usize = 80;
const SNIPPET_LEAD: usize = 16;

/// 结果来源（`AiSearchResult::source`）。
pub const SOURCE_DSL: &str = "dsl";
pub const SOURCE_FUSION: &str = "fusion";
pub const SOURCE_FILENAME: &str = "filename";
pub const SOURCE_IMAGE: &str = "image";

/// 命中来源（`AiSearchResult::matched_by`），界面显示为"命中：描述 / 文字 / 标签 / 画面"。
pub const MATCH_DESCRIPTION: &str = "description";
pub const MATCH_TEXT: &str = "text";
pub const MATCH_TAG: &str = "tag";
pub const MATCH_FILENAME: &str = "filename";
pub const MATCH_SEMANTIC: &str = "semantic";
pub const MATCH_IMAGE: &str = "image";

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

/// 高亮片段的一段：`hit` 为 true 的是命中的词。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SnippetPart {
    pub text: String,
    pub hit: bool,
}

/// 一条搜索结果。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AiSearchResult {
    pub image: ImageRecord,
    /// 融合结果是 RRF 分数；以图搜图是余弦相似度；DSL / 文件名兜底是固定值。
    pub score: f32,
    pub source: String,
    /// 命中了哪几路：`description` / `text` / `tag` / `filename` / `semantic` / `image`。
    pub matched_by: Vec<String>,
    /// 描述（或画面文字、OCR）里命中查询词的片段。
    pub snippet: Option<Vec<SnippetPart>>,
}

/// 召回通道。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Channel {
    Tag,
    FullText,
    ImageVector,
    TextVector,
}

impl Channel {
    pub const ALL: [Channel; 4] = [
        Channel::Tag,
        Channel::FullText,
        Channel::ImageVector,
        Channel::TextVector,
    ];
}

/// 每路的统计（命中数、耗时），随响应返回，界面和评测用。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChannelStat {
    pub channel: Channel,
    pub hits: usize,
    pub elapsed_ms: f64,
}

/// `ai_search` 的返回。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AiSearchResponse {
    pub results: Vec<AiSearchResult>,
    /// 语义通道（向量）没跑：worker 还没就绪或处于兜底模式，只返回了文字通道的结果。
    pub degraded: bool,
    pub degraded_reason: Option<String>,
    pub channels: Vec<ChannelStat>,
    pub elapsed_ms: f64,
}

impl AiSearchResponse {
    fn from_results(results: Vec<AiSearchResult>, started: Instant) -> Self {
        Self {
            results,
            degraded: false,
            degraded_reason: None,
            channels: Vec::new(),
            elapsed_ms: elapsed_ms(started),
        }
    }
}

/// 融合权重（docs/10 §9.1 结论 6 的默认值：图像向量 1.0、描述向量 1.0、全文 0.6、标签 0.3）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FusionWeights {
    pub tag: f32,
    pub full_text: f32,
    pub image_vector: f32,
    pub text_vector: f32,
}

impl Default for FusionWeights {
    fn default() -> Self {
        Self {
            tag: 0.3,
            full_text: 0.6,
            image_vector: 1.0,
            text_vector: 1.0,
        }
    }
}

impl FusionWeights {
    pub fn of(&self, channel: Channel) -> f32 {
        match channel {
            Channel::Tag => self.tag,
            Channel::FullText => self.full_text,
            Channel::ImageVector => self.image_vector,
            Channel::TextVector => self.text_vector,
        }
    }
}

/// 搜索选项。线上用默认值；评测用它单独跑某几路。
#[derive(Debug, Clone)]
pub struct SearchOptions {
    pub channels: Vec<Channel>,
    pub weights: FusionWeights,
    /// 图像向量通道的相似度下限：文字通道有结果时，低于下限的图像向量命中不参与融合
    /// （文字通道没有结果时全部保留，docs/10 §8.4）。
    pub image_similarity_floor: f32,
    /// 描述向量通道的相似度下限（同上）。
    pub text_similarity_floor: f32,
    /// 全文通道的双字词只查这些列（评测对比用）；`None` 为全部列。
    pub fulltext_columns: Option<Vec<String>>,
}

impl Default for SearchOptions {
    fn default() -> Self {
        Self {
            channels: Channel::ALL.to_vec(),
            weights: FusionWeights::default(),
            image_similarity_floor: IMAGE_SIMILARITY_FLOOR,
            text_similarity_floor: TEXT_SIMILARITY_FLOOR,
            fulltext_columns: None,
        }
    }
}

impl SearchOptions {
    pub fn only(channels: &[Channel]) -> Self {
        Self {
            channels: channels.to_vec(),
            ..Self::default()
        }
    }

    fn wants(&self, channel: Channel) -> bool {
        self.channels.contains(&channel)
    }
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

    /// 描述文本向量模型（A4）；`None` 表示不跑描述向量这一路。
    fn text_model(&self) -> Option<&str> {
        None
    }

    /// 查询文本 → 描述文本向量（A4）；兜底模式返回 `Ok(None)`。
    async fn encode_query(&self, _text: &str) -> AppResult<Option<Vec<f32>>> {
        Ok(None)
    }
}

/// 线上实现：经 [`AiSupervisor`] 调 Python worker（worker 没启动时在后台拉起）。
pub struct WorkerQueryEncoder {
    supervisor: Arc<AiSupervisor>,
    clip_model: String,
    text_model: Option<String>,
}

impl WorkerQueryEncoder {
    /// `text_model` 为 `None` 时不跑描述向量这一路。
    pub fn new(
        supervisor: Arc<AiSupervisor>,
        clip_model: String,
        text_model: Option<String>,
    ) -> Self {
        Self {
            supervisor,
            clip_model,
            text_model,
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

    /// 搜索不为 worker 冷启动（导入 torch、加载模型要十几秒）干等：没就绪就在后台拉起，
    /// 最多等 [`SEARCH_WORKER_WAIT`]，等不到返回错误，调用方据此降级为只跑文字通道。
    async fn ensure_ready_briefly(&self) -> AppResult<()> {
        if self.supervisor.status().await?.status == "ready" {
            return Ok(());
        }
        let supervisor = self.supervisor.clone();
        let starting = tokio::spawn(async move { supervisor.ensure_ready().await });
        match tokio::time::timeout(SEARCH_WORKER_WAIT, starting).await {
            Ok(Ok(true)) => Ok(()),
            Ok(Ok(false)) => Err(AppError::Other(
                "AI worker 启动失败，语义检索暂不可用".to_string(),
            )),
            Ok(Err(error)) => Err(AppError::Other(format!("AI worker 启动任务异常：{error}"))),
            Err(_) => Err(AppError::Other(
                "语义检索启动中（AI worker 正在加载模型）".to_string(),
            )),
        }
    }
}

#[async_trait]
impl QueryEncoder for WorkerQueryEncoder {
    fn clip_model(&self) -> &str {
        &self.clip_model
    }

    async fn encode_text(&self, text: &str) -> AppResult<Option<Vec<f32>>> {
        self.ensure_ready_briefly().await?;
        let response = self
            .supervisor
            .encode_text(text.to_string(), Some(self.clip_model.clone()))
            .await?;
        if response.fallback {
            tracing::info!(
                model = %response.model,
                "CLIP 文本编码处于兜底模式（伪向量），本次搜索跳过图像向量通道"
            );
            return Ok(None);
        }
        self.check_model(&response.model)?;
        Ok(Some(response.embedding))
    }

    fn text_model(&self) -> Option<&str> {
        self.text_model.as_deref()
    }

    async fn encode_query(&self, text: &str) -> AppResult<Option<Vec<f32>>> {
        let Some(model) = self.text_model.clone() else {
            return Ok(None);
        };
        self.ensure_ready_briefly().await?;
        let response = self
            .supervisor
            .embed_texts(vec![text.to_string()], Some(model.clone()), true)
            .await?;
        if response.fallback {
            tracing::info!(
                model = %response.model,
                "文本向量处于兜底模式（伪向量），本次搜索跳过描述向量通道"
            );
            return Ok(None);
        }
        if response.model != model {
            return Err(AppError::Other(format!(
                "AI worker 返回的文本向量模型 {} 与设置的 {model} 不一致",
                response.model
            )));
        }
        Ok(response.embeddings.into_iter().next())
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

/// 自然语言 + DSL 搜索（默认选项）。
pub async fn search(
    pool: &Pool,
    vectors: &LanceDbRepo,
    encoder: &dyn QueryEncoder,
    args: &AiSearchArgs,
) -> AppResult<AiSearchResponse> {
    search_with(pool, vectors, encoder, args, &SearchOptions::default()).await
}

/// 自然语言 + DSL 搜索。
///
/// - 只有过滤条件（`#标签` / `format:` / `size:`）：整个查询（过滤 + 默认排序 + LIMIT）在 SQL 里完成，
///   不调用 worker；
/// - 有查询文本：多路召回 + RRF 融合；过滤条件在每一路里都生效；所有通道都没结果时退回文件名匹配，
///   过滤条件一个不丢。
pub async fn search_with(
    pool: &Pool,
    vectors: &LanceDbRepo,
    encoder: &dyn QueryEncoder,
    args: &AiSearchArgs,
    options: &SearchOptions,
) -> AppResult<AiSearchResponse> {
    let started = Instant::now();
    let limit = clamp_limit(args.limit);
    let filter = parse_search_filter(&args.text);
    if filter.is_empty() {
        return Ok(AiSearchResponse::from_results(Vec::new(), started));
    }
    let root_ids = args.root_ids.clone().filter(|ids| !ids.is_empty());

    if filter.text.is_empty() {
        let conn = pool.get()?;
        let results = dsl_search(&conn, &filter, root_ids, limit)?;
        return Ok(AiSearchResponse::from_results(results, started));
    }

    let params = query_params(&filter, root_ids.clone(), None, None);
    // 有过滤条件就先求候选集（向量通道 prefilter 用）。候选集为空时任何通道都不可能命中，
    // 直接返回，连 worker 都不用唤醒。
    let candidates = if root_ids.is_some() || filter.has_filter() {
        let conn = pool.get()?;
        let ids = images_repo::filtered_ids(&conn, &params)?;
        if ids.is_empty() {
            return Ok(AiSearchResponse::from_results(Vec::new(), started));
        }
        Some(ids)
    } else {
        None
    };

    let query = filter.text.clone();
    // A2b：同义说法一起检索（"小猫"也找"猫""猫咪"）。
    let tokens = synonyms::expand_tokens(text_tokens::query_tokens(&query), &query);

    // 文字通道是同步 SQL，放到 blocking 线程，与 worker 编码查询并行。
    let text_task = {
        let pool = pool.clone();
        let params = params.clone();
        let query = query.clone();
        let tokens = tokens.clone();
        let options = options.clone();
        tokio::task::spawn_blocking(move || {
            run_text_channels(&pool, &params, &query, &tokens, &options)
        })
    };
    let (image_query, text_query) = tokio::join!(
        encode_channel(options.wants(Channel::ImageVector), || encoder
            .encode_text(&query)),
        encode_channel(
            options.wants(Channel::TextVector) && encoder.text_model().is_some(),
            || encoder.encode_query(&query)
        ),
    );
    let mut channels = text_task.await??;

    let mut degraded_reason = None;
    let image_vector = match image_query {
        Some(Ok(Some(vector))) => Some(vector),
        Some(Ok(None)) => {
            degraded_reason = Some("AI worker 处于兜底模式（缺模型），语义检索不可用".to_string());
            None
        }
        Some(Err(error)) => {
            degraded_reason = Some(error.to_string());
            None
        }
        None => None,
    };
    let text_vector = match (text_query, encoder.text_model()) {
        (Some(Ok(Some(vector))), Some(model)) => Some((model, vector)),
        (Some(Ok(None)), _) => {
            degraded_reason.get_or_insert_with(|| {
                "AI worker 处于兜底模式（缺文本向量模型），描述语义检索不可用".to_string()
            });
            None
        }
        (Some(Err(error)), _) => {
            degraded_reason.get_or_insert_with(|| error.to_string());
            None
        }
        _ => None,
    };
    // 两路向量检索互不依赖，并行跑。
    let candidates = candidates.as_deref();
    let (image_hits, text_hits) = tokio::join!(
        vector_channel(
            vectors,
            image_vector.map(|vector| (
                VectorTable::image_clip(encoder.clip_model(), vector.len()),
                vector
            )),
            candidates,
        ),
        vector_channel(
            vectors,
            text_vector
                .map(|(model, vector)| (VectorTable::image_text(model, vector.len()), vector)),
            candidates,
        ),
    );
    let text_has_hits = channels.iter().any(|channel| !channel.hits.is_empty());
    if let Some((mut hits, started)) = image_hits? {
        if text_has_hits {
            hits.retain(|hit| hit.score >= options.image_similarity_floor);
        }
        channels.push(ChannelHits::from_vector(
            Channel::ImageVector,
            hits,
            started,
        ));
    }
    if let Some((mut hits, started)) = text_hits? {
        if text_has_hits {
            hits.retain(|hit| hit.score >= options.text_similarity_floor);
        }
        channels.push(ChannelHits::from_vector(Channel::TextVector, hits, started));
    }

    let fused = fuse(&channels, &options.weights);
    let mut results = {
        let conn = pool.get()?;
        let hydrated = hydrate_fused(&conn, &fused, limit)?;
        annotate(&conn, hydrated, &display_tokens(&tokens))?
    };
    if results.is_empty() {
        let conn = pool.get()?;
        results = filename_search(&conn, &filter, root_ids, limit)?;
    }
    if let Some(reason) = &degraded_reason {
        tracing::info!(reason, "搜索降级：只用了文字通道");
    }
    Ok(AiSearchResponse {
        results,
        degraded: degraded_reason.is_some(),
        degraded_reason,
        channels: channels.iter().map(ChannelHits::stat).collect(),
        elapsed_ms: elapsed_ms(started),
    })
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
    hydrate_hits(&conn, &hits, limit, Some(image_id))
}

// ---------------------------------------------------------------------------
// 召回通道
// ---------------------------------------------------------------------------

/// 一路召回的结果：按相关度从高到低的 (image_id, 原始分数)。
#[derive(Debug, Clone)]
struct ChannelHits {
    channel: Channel,
    hits: Vec<(i64, f32)>,
    elapsed_ms: f64,
}

impl ChannelHits {
    fn from_vector(channel: Channel, hits: Vec<VectorSearchHit>, started: Instant) -> Self {
        Self {
            channel,
            hits: hits
                .into_iter()
                .map(|hit| (hit.image_id, hit.score))
                .collect(),
            elapsed_ms: elapsed_ms(started),
        }
    }

    fn stat(&self) -> ChannelStat {
        ChannelStat {
            channel: self.channel,
            hits: self.hits.len(),
            elapsed_ms: self.elapsed_ms,
        }
    }
}

/// 一路向量检索；没有查询向量时返回 `None`。
async fn vector_channel(
    vectors: &LanceDbRepo,
    query: Option<(VectorTable, Vec<f32>)>,
    candidates: Option<&[i64]>,
) -> AppResult<Option<(Vec<VectorSearchHit>, Instant)>> {
    let Some((table, vector)) = query else {
        return Ok(None);
    };
    let started = Instant::now();
    let hits = vectors
        .top_k_filtered(&table, &vector, CHANNEL_DEPTH, candidates)
        .await?;
    Ok(Some((hits, started)))
}

/// 需要时调用编码器；不需要这一路时返回 `None`。
async fn encode_channel<F, Fut>(wanted: bool, encode: F) -> Option<AppResult<Option<Vec<f32>>>>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = AppResult<Option<Vec<f32>>>>,
{
    if wanted {
        Some(encode().await)
    } else {
        None
    }
}

fn run_text_channels(
    pool: &Pool,
    params: &ImageQueryParams,
    query: &str,
    tokens: &Tokens,
    options: &SearchOptions,
) -> AppResult<Vec<ChannelHits>> {
    let mut conn = pool.get()?;
    let mut out = Vec::new();
    if options.wants(Channel::Tag) {
        let started = Instant::now();
        let hits = tag_channel(&conn, query, params)?;
        out.push(ChannelHits {
            channel: Channel::Tag,
            hits,
            elapsed_ms: elapsed_ms(started),
        });
    }
    if options.wants(Channel::FullText) {
        let started = Instant::now();
        // 积压不多时先补上：刚打的标签、刚导入的描述马上就能搜到。
        if search_index_service::pending_count(&conn)? <= SEARCH_FLUSH_MAX_PENDING {
            search_index_service::flush_dirty(&mut conn, SEARCH_FLUSH_MAX_PENDING as usize)?;
        }
        let hits = fulltext_channel(&conn, tokens, params, options.fulltext_columns.as_deref())?;
        out.push(ChannelHits {
            channel: Channel::FullText,
            hits,
            elapsed_ms: elapsed_ms(started),
        });
    }
    Ok(out)
}

/// 标签通道：找出名字出现在查询里的标签（"海边的小孩" → "海边""小孩"），返回带这些标签、满足过滤条件
/// 的图，按"标签覆盖了查询里的多少个字"、标签分数之和、时间新旧排序。单字标签只在查询本身就是这个字
/// （或空格隔开的一个词）时才算，免得"人""车"这种字到处命中。
pub fn tag_channel(
    conn: &Connection,
    query: &str,
    params: &ImageQueryParams,
) -> AppResult<Vec<(i64, f32)>> {
    let normalized = text_tokens::normalize(query);
    let query_chars = normalized.chars().collect::<Vec<_>>();
    let words = normalized.split_whitespace().collect::<HashSet<_>>();
    // A2b：查询里的同义说法 → 同组其他说法也算命中，覆盖的是原说法在查询里的位置。
    let mut aliases: HashMap<String, u64> = HashMap::new();
    for expansion in synonyms::expansions(&normalized) {
        let matched_chars = expansion.matched.chars().collect::<Vec<_>>();
        let mask = occurrence_mask(&query_chars, &matched_chars);
        for alternative in expansion.alternatives {
            *aliases.entry(alternative).or_default() |= mask;
        }
    }
    let mut matched: HashMap<i64, u64> = HashMap::new();
    {
        let mut stmt = conn.prepare_cached("SELECT id, name FROM tags")?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?;
        for row in rows {
            let (tag_id, name) = row?;
            let mut mask = 0u64;
            for part in search_index_service::split_tag_name(&name) {
                let part = text_tokens::normalize(part);
                let part_chars = part.chars().collect::<Vec<_>>();
                if let Some(alias) = aliases.get(&part) {
                    mask |= alias;
                }
                if part_chars.is_empty()
                    || (part_chars.len() == 1 && !words.contains(part.as_str()))
                {
                    continue;
                }
                mask |= occurrence_mask(&query_chars, &part_chars);
            }
            if mask != 0 {
                matched.insert(tag_id, mask);
            }
        }
    }
    if matched.is_empty() {
        return Ok(Vec::new());
    }

    let tag_ids = matched.keys().copied().collect::<Vec<_>>();
    let (clauses, filter_values) = images_repo::filter_clauses(params);
    let sql = format!(
        "SELECT it.image_id, it.tag_id, MAX(it.score), i.mtime
         FROM image_tags it
         JOIN images i ON i.id = it.image_id
         WHERE it.tag_id IN ({}){}
         GROUP BY it.image_id, it.tag_id",
        placeholders(tag_ids.len()),
        and_clauses(&clauses)
    );
    let mut values = tag_ids
        .iter()
        .copied()
        .map(Value::Integer)
        .collect::<Vec<_>>();
    values.extend(filter_values);
    let mut per_image: HashMap<i64, (u64, f64, i64)> = HashMap::new();
    {
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(values.iter()), |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, f64>(2)?,
                row.get::<_, i64>(3)?,
            ))
        })?;
        for row in rows {
            let (image_id, tag_id, score, mtime) = row?;
            let entry = per_image.entry(image_id).or_insert((0, 0.0, mtime));
            entry.0 |= matched.get(&tag_id).copied().unwrap_or(0);
            entry.1 += score;
        }
    }
    let mut ranked = per_image
        .into_iter()
        .map(|(image_id, (mask, score_sum, mtime))| (image_id, mask.count_ones(), score_sum, mtime))
        .collect::<Vec<_>>();
    ranked.sort_by(|a, b| {
        b.1.cmp(&a.1)
            .then(b.2.total_cmp(&a.2))
            .then(b.3.cmp(&a.3))
            .then(b.0.cmp(&a.0))
    });
    ranked.truncate(CHANNEL_DEPTH);
    Ok(ranked
        .into_iter()
        .map(|(image_id, coverage, score_sum, _)| {
            (image_id, coverage as f32 + score_sum as f32 / 100.0)
        })
        .collect())
}

/// `part` 在查询里每次出现所覆盖的字的位置（超过 64 个字的部分忽略）。
fn occurrence_mask(query: &[char], part: &[char]) -> u64 {
    if part.len() > query.len() {
        return 0;
    }
    let mut mask = 0u64;
    for start in 0..=(query.len() - part.len()) {
        if query[start..start + part.len()] == *part {
            for position in start..start + part.len() {
                if position < 64 {
                    mask |= 1 << position;
                }
            }
        }
    }
    mask
}

/// 全文通道：先要求所有词都出现（AND），命中少于 [`FULLTEXT_AND_MIN_HITS`] 时放宽为任一词出现（OR，
/// 同时带上单字列）。按 bm25（列权重 [`FULLTEXT_COLUMN_WEIGHTS`]）排序。
pub fn fulltext_channel(
    conn: &Connection,
    tokens: &Tokens,
    params: &ImageQueryParams,
    columns: Option<&[String]>,
) -> AppResult<Vec<(i64, f32)>> {
    if tokens.is_empty() {
        return Ok(Vec::new());
    }
    let column_filter = match columns {
        Some(columns) => format!("{{{}}}", columns.join(" ")),
        None => format!("{{{}}}", FULLTEXT_TERM_COLUMNS.join(" ")),
    };
    let and_query = fts_match_query(tokens, &column_filter, true);
    let mut hits = run_fulltext(conn, &and_query, params)?;
    if hits.len() < FULLTEXT_AND_MIN_HITS {
        let or_query = fts_match_query(tokens, &column_filter, false);
        if or_query != and_query {
            hits = run_fulltext(conn, &or_query, params)?;
        }
    }
    Ok(hits)
}

/// FTS5 查询表达式。AND：双字词全部出现（只有单字时要求单字全部出现）；OR：任一双字词或任一单字。
///
/// 有同义扩展（A2b）时，同义说法所在位置的词在 AND 里换成"任一说法"的 OR 组
/// （`"窗台" AND ("小猫" OR "猫" OR "猫咪" …)`；嵌在长片段里时每一支带上各自的跨界双字词：
/// `"游戏" AND (("戏截" AND "截图") OR ("戏截" AND "截屏") …)`），OR 查询里也加上其他说法的词。
pub fn fts_match_query(tokens: &Tokens, column_filter: &str, all: bool) -> String {
    let quote = |term: &String| text_tokens::fts_quote(term);
    let chars = tokens.chars.iter().map(quote).collect::<Vec<_>>();
    if all {
        if tokens.terms.is_empty() {
            return format!("chars : ({})", chars.join(" AND "));
        }
        let grouped = tokens
            .synonyms
            .iter()
            .flat_map(|synonym| synonym.matched_terms.iter().chain(&synonym.dropped_terms))
            .collect::<HashSet<_>>();
        let mut clauses = tokens
            .terms
            .iter()
            .filter(|term| !grouped.contains(term))
            .map(quote)
            .collect::<Vec<_>>();
        for synonym in &tokens.synonyms {
            let options = std::iter::once(&synonym.matched_terms)
                .chain(synonym.alternatives.iter().map(|(_, terms)| terms))
                .map(|terms| all_of(&terms.iter().map(quote).collect::<Vec<_>>()))
                .collect::<Vec<_>>();
            clauses.push(format!("({})", options.join(" OR ")));
        }
        format!("{column_filter} : ({})", clauses.join(" AND "))
    } else {
        let mut terms = tokens.terms.iter().map(quote).collect::<Vec<_>>();
        for synonym in &tokens.synonyms {
            for (_, alternative_terms) in &synonym.alternatives {
                for term in alternative_terms {
                    let quoted = quote(term);
                    if !terms.contains(&quoted) {
                        terms.push(quoted);
                    }
                }
            }
        }
        let mut parts = Vec::new();
        if !terms.is_empty() {
            parts.push(format!("{column_filter} : ({})", terms.join(" OR ")));
        }
        if !chars.is_empty() {
            parts.push(format!("chars : ({})", chars.join(" OR ")));
        }
        parts.join(" OR ")
    }
}

fn all_of(terms: &[String]) -> String {
    if terms.len() == 1 {
        terms[0].clone()
    } else {
        format!("({})", terms.join(" AND "))
    }
}

/// 标命中来源和高亮用的词：查询词加上同义扩展的其他说法（"猫咪"命中的描述也高亮）。
fn display_tokens(tokens: &Tokens) -> Tokens {
    let mut display = tokens.clone();
    for synonym in &tokens.synonyms {
        for (alternative, _) in &synonym.alternatives {
            if !display.terms.contains(alternative) {
                display.terms.push(alternative.clone());
            }
        }
    }
    display
}

fn run_fulltext(
    conn: &Connection,
    match_query: &str,
    params: &ImageQueryParams,
) -> AppResult<Vec<(i64, f32)>> {
    if match_query.is_empty() {
        return Ok(Vec::new());
    }
    let (clauses, filter_values) = images_repo::filter_clauses(params);
    let weights = FULLTEXT_COLUMN_WEIGHTS
        .iter()
        .map(|weight| weight.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "SELECT search_fts.rowid, bm25(search_fts, {weights}) AS rank_score
         FROM search_fts
         JOIN images i ON i.id = search_fts.rowid
         WHERE search_fts MATCH ?{}
         ORDER BY rank_score, search_fts.rowid DESC
         LIMIT ?",
        and_clauses(&clauses)
    );
    let mut values = vec![Value::Text(match_query.to_string())];
    values.extend(filter_values);
    values.push(Value::Integer(CHANNEL_DEPTH as i64));
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_from_iter(values.iter()), |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, f64>(1)?))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (image_id, rank) = row?;
        // bm25() 越小越相关（负数），取反当作分数。
        out.push((image_id, (-rank) as f32));
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// 融合与结果
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct FusedHit {
    image_id: i64,
    score: f32,
    channels: Vec<Channel>,
}

/// RRF：score = Σ w_c / (60 + rank_c)，rank 从 1 开始。只有一路时完全保持该路的顺序。
fn fuse(channels: &[ChannelHits], weights: &FusionWeights) -> Vec<FusedHit> {
    let mut by_id: HashMap<i64, FusedHit> = HashMap::new();
    for channel in channels {
        let weight = weights.of(channel.channel);
        if weight <= 0.0 {
            continue;
        }
        for (rank, (image_id, _)) in channel.hits.iter().enumerate() {
            let entry = by_id.entry(*image_id).or_insert_with(|| FusedHit {
                image_id: *image_id,
                score: 0.0,
                channels: Vec::new(),
            });
            entry.score += weight / (RRF_K + rank as f32 + 1.0);
            if !entry.channels.contains(&channel.channel) {
                entry.channels.push(channel.channel);
            }
        }
    }
    let mut fused = by_id.into_values().collect::<Vec<_>>();
    fused.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then(a.image_id.cmp(&b.image_id))
    });
    fused
}

/// 融合结果 → 完整图片记录，保持顺序，丢掉已删除 / 不存在的图，最多 `limit` 条。
fn hydrate_fused(
    conn: &Connection,
    fused: &[FusedHit],
    limit: usize,
) -> AppResult<Vec<(AiSearchResult, Vec<Channel>)>> {
    let mut results = Vec::with_capacity(limit.min(fused.len()));
    for chunk in fused.chunks(limit.max(1)) {
        let ids = chunk.iter().map(|hit| hit.image_id).collect::<Vec<_>>();
        let mut by_id = images_repo::get_details_by_ids(conn, &ids)?
            .into_iter()
            .map(|image| (image.id, image))
            .collect::<HashMap<_, _>>();
        for hit in chunk {
            let Some(image) = by_id.remove(&hit.image_id) else {
                continue;
            };
            results.push((
                AiSearchResult {
                    image,
                    score: hit.score,
                    source: SOURCE_FUSION.to_string(),
                    matched_by: Vec::new(),
                    snippet: None,
                },
                hit.channels.clone(),
            ));
            if results.len() >= limit {
                return Ok(results);
            }
        }
    }
    Ok(results)
}

/// 补上命中来源和片段。
fn annotate(
    conn: &Connection,
    results: Vec<(AiSearchResult, Vec<Channel>)>,
    tokens: &Tokens,
) -> AppResult<Vec<AiSearchResult>> {
    let ids = results
        .iter()
        .map(|(result, _)| result.image.id)
        .collect::<Vec<_>>();
    let texts = search_index_service::load_texts(conn, &ids)?;
    let empty = SearchTexts::default();
    let mut out = Vec::with_capacity(results.len());
    for (mut result, channels) in results {
        let texts = texts.get(&result.image.id).unwrap_or(&empty);
        result.matched_by = matched_by(&channels, texts, tokens);
        result.snippet = snippet_for(texts, tokens);
        out.push(result);
    }
    Ok(out)
}

/// 命中来源：向量通道直接记；全文通道再看是哪个字段含查询词。按"描述、文字、标签、文件名、语义、画面"排序。
fn matched_by(channels: &[Channel], texts: &SearchTexts, tokens: &Tokens) -> Vec<String> {
    let mut found = HashSet::new();
    if channels.contains(&Channel::Tag) {
        found.insert(MATCH_TAG);
    }
    if channels.contains(&Channel::ImageVector) {
        found.insert(MATCH_IMAGE);
    }
    if channels.contains(&Channel::TextVector) {
        found.insert(MATCH_SEMANTIC);
    }
    if channels.contains(&Channel::FullText) {
        let fields = [
            (MATCH_DESCRIPTION, vec![texts.description.as_str()]),
            (
                MATCH_TEXT,
                vec![texts.visible_text.as_str(), texts.ocr_text.as_str()],
            ),
            (
                MATCH_TAG,
                texts
                    .tags
                    .iter()
                    .chain(texts.keywords.iter())
                    .map(String::as_str)
                    .collect(),
            ),
            (MATCH_FILENAME, vec![texts.filename.as_str()]),
        ];
        let mut by_terms = fields
            .iter()
            .filter(|(_, values)| {
                values
                    .iter()
                    .any(|value| contains_any(value, &tokens.terms))
            })
            .map(|(key, _)| *key)
            .collect::<Vec<_>>();
        if by_terms.is_empty() {
            by_terms = fields
                .iter()
                .filter(|(_, values)| {
                    values
                        .iter()
                        .any(|value| contains_any(value, &tokens.chars))
                })
                .map(|(key, _)| *key)
                .take(1)
                .collect();
        }
        found.extend(by_terms);
    }
    [
        MATCH_DESCRIPTION,
        MATCH_TEXT,
        MATCH_TAG,
        MATCH_FILENAME,
        MATCH_SEMANTIC,
        MATCH_IMAGE,
    ]
    .into_iter()
    .filter(|key| found.contains(key))
    .map(str::to_string)
    .collect()
}

fn contains_any(value: &str, needles: &[String]) -> bool {
    if value.is_empty() || needles.is_empty() {
        return false;
    }
    let normalized = text_tokens::normalize(value);
    needles
        .iter()
        .any(|needle| normalized.contains(needle.as_str()))
}

/// 片段：优先描述，其次画面文字、OCR，取第一个含查询词的字段，在命中处前后截一段并标出命中词；
/// 都不含查询词但有描述时（例如靠向量命中），给描述开头一段作参考。
fn snippet_for(texts: &SearchTexts, tokens: &Tokens) -> Option<Vec<SnippetPart>> {
    for field in [&texts.description, &texts.visible_text, &texts.ocr_text] {
        if let Some(parts) = highlight(field, &tokens.terms) {
            return Some(parts);
        }
    }
    let head = texts.description.trim();
    if head.is_empty() {
        return None;
    }
    let chars = head.chars().collect::<Vec<_>>();
    let mut text = chars.iter().take(SNIPPET_CHARS).collect::<String>();
    if chars.len() > SNIPPET_CHARS {
        text.push('…');
    }
    Some(vec![SnippetPart { text, hit: false }])
}

/// 在 `text` 里标出 `terms`（已规范化）的所有出现位置，截取第一处命中附近的一段。没有命中返回 `None`。
pub fn highlight(text: &str, terms: &[String]) -> Option<Vec<SnippetPart>> {
    if text.trim().is_empty() || terms.is_empty() {
        return None;
    }
    let original = text.chars().collect::<Vec<_>>();
    // 规范化后的字 → 原文下标（NFKC 可能把一个字拆成几个，按字建映射）。
    let mut normalized = Vec::new();
    let mut origin = Vec::new();
    for (index, c) in original.iter().enumerate() {
        for n in text_tokens::normalize(&c.to_string()).chars() {
            normalized.push(n);
            origin.push(index);
        }
    }
    let mut hits = vec![false; original.len()];
    let mut first = None;
    for term in terms {
        let needle = term.chars().collect::<Vec<_>>();
        if needle.is_empty() || needle.len() > normalized.len() {
            continue;
        }
        for start in 0..=(normalized.len() - needle.len()) {
            if normalized[start..start + needle.len()] == needle[..] {
                let from = origin[start];
                let to = origin[start + needle.len() - 1];
                for flag in hits.iter_mut().take(to + 1).skip(from) {
                    *flag = true;
                }
                first = Some(first.map_or(from, |current: usize| current.min(from)));
            }
        }
    }
    let first = first?;
    let start = first.saturating_sub(SNIPPET_LEAD);
    let end = (start + SNIPPET_CHARS).min(original.len());
    let mut parts: Vec<SnippetPart> = Vec::new();
    if start > 0 {
        parts.push(SnippetPart {
            text: "…".to_string(),
            hit: false,
        });
    }
    for (c, hit) in original[start..end].iter().zip(&hits[start..end]) {
        match parts.last_mut() {
            Some(last) if last.hit == *hit => last.text.push(*c),
            _ => parts.push(SnippetPart {
                text: c.to_string(),
                hit: *hit,
            }),
        }
    }
    if end < original.len() {
        match parts.last_mut() {
            Some(last) if !last.hit => last.text.push('…'),
            _ => parts.push(SnippetPart {
                text: "…".to_string(),
                hit: false,
            }),
        }
    }
    Some(parts)
}

// ---------------------------------------------------------------------------
// DSL 解析与兜底
// ---------------------------------------------------------------------------

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

/// 所有通道都没结果时的兜底：文件名包含查询文本，且仍满足全部过滤条件（含标签）。
fn filename_search(
    conn: &Connection,
    filter: &SearchFilter,
    root_ids: Option<Vec<i64>>,
    limit: usize,
) -> AppResult<Vec<AiSearchResult>> {
    let params = query_params(filter, root_ids, Some(filter.text.clone()), Some(limit));
    let page = images_repo::query(conn, &params)?;
    let mut results = into_results(page.items, SCORE_FILENAME, SOURCE_FILENAME);
    for result in &mut results {
        result.matched_by = vec![MATCH_FILENAME.to_string()];
    }
    Ok(results)
}

fn into_results(images: Vec<ImageRecord>, score: f32, source: &str) -> Vec<AiSearchResult> {
    images
        .into_iter()
        .map(|image| AiSearchResult {
            image,
            score,
            source: source.to_string(),
            matched_by: Vec::new(),
            snippet: None,
        })
        .collect()
}

/// 以图搜图的向量命中 → 完整图片记录，保持命中顺序（近的在前），丢掉已删除 / 不存在的图，最多
/// `limit` 条。按 `limit` 分块水合：通常一次查询就够，不必把多取的命中全部读出来。
fn hydrate_hits(
    conn: &Connection,
    hits: &[VectorSearchHit],
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
                source: SOURCE_IMAGE.to_string(),
                matched_by: vec![MATCH_IMAGE.to_string()],
                snippet: None,
            });
            if results.len() >= limit {
                return Ok(results);
            }
        }
    }
    Ok(results)
}

fn thumb_path_for(thumbs_dir: &Path, image: &ImageRecord) -> AppResult<String> {
    let path = thumbnail_service::record_thumb_path(thumbs_dir, image)
        .ok_or_else(|| AppError::Other("缩略图尚未生成，无法运行 AI".to_string()))?;
    Ok(path.to_string_lossy().to_string())
}

fn and_clauses(clauses: &[String]) -> String {
    clauses
        .iter()
        .map(|clause| format!(" AND {clause}"))
        .collect::<String>()
}

fn placeholders(count: usize) -> String {
    std::iter::repeat("?")
        .take(count)
        .collect::<Vec<_>>()
        .join(",")
}

fn elapsed_ms(started: Instant) -> f64 {
    started.elapsed().as_secs_f64() * 1000.0
}
