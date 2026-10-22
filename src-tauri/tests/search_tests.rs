//! A1 搜索正确性（docs/10 §10 A1）：
//! (a) 只输 `#标签` 能找到默认排序第 400 行以后的图；
//! (b) 选中目录 / 带标签时语义搜索不漏（SQL 候选集 + LanceDB prefilter）；
//! (c) 退回文件名匹配时保留 `#标签` 条件；
//! (d) encoder 报兜底时不走语义通道、不 panic；
//! (e) worker 兜底返回的向量 / 标签不入库，流水线按 worker 进程退避。
//! 另含 DSL 解析与大小单位的用例。

use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use photo_view_plus_lib::db::{self, Pool};
use photo_view_plus_lib::repo::ai_jobs_repo::{self, KIND_IMAGE_EMBEDDING};
use photo_view_plus_lib::repo::images_repo::{self, NewImageMetadata};
use photo_view_plus_lib::repo::lancedb_repo::{EmbeddingRecord, LanceDbRepo, VectorTable};
use photo_view_plus_lib::repo::roots_repo::{self, NewRoot};
use photo_view_plus_lib::repo::tags_repo::{self, NewTagScore};
use photo_view_plus_lib::services::ai_client::{
    ClipEmbedResponse, ClipEmbedResult, TagScore, TaggerResponse, TaggerResult,
};
use photo_view_plus_lib::services::ai_pipeline::{self, FallbackBackoff, FALLBACK_BACKOFF};
use photo_view_plus_lib::services::search_service::{
    self, AiSearchArgs, AiSearchResult, QueryEncoder, SOURCE_DSL, SOURCE_FILENAME, SOURCE_FUSION,
    SOURCE_IMAGE,
};
use photo_view_plus_lib::{AppError, AppResult};
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// 测试夹具
// ---------------------------------------------------------------------------

/// 测试用的图像向量模型与维度（与线上默认的 Chinese-CLIP 一致）。
const MODEL: &str = "chinese-clip-vit-b-16";
const DIMS: usize = 512;

fn clip_table() -> VectorTable {
    VectorTable::image_clip(MODEL, DIMS)
}

struct Env {
    pool: Pool,
    vectors: LanceDbRepo,
    dir: TempDir,
}

fn env() -> Env {
    let dir = tempfile::tempdir().expect("tempdir");
    let pool = db::open(&dir.path().join("app.sqlite")).expect("db");
    let vectors = LanceDbRepo::new(dir.path().join("vectors"));
    Env { pool, vectors, dir }
}

fn add_root(pool: &Pool, name: &str) -> i64 {
    let conn = pool.get().expect("conn");
    roots_repo::insert(
        &conn,
        &NewRoot {
            path: format!("D:/photos/{name}"),
            label: None,
            root_type: "local".to_string(),
        },
        1,
    )
    .expect("root")
    .id
}

/// 单事务批量插图，返回 id（与 `files` 顺序一致）。`files`：(文件名, mtime)。
fn add_images(pool: &Pool, root_id: i64, files: &[(String, i64)]) -> Vec<i64> {
    let conn = pool.get().expect("conn");
    let tx = conn.unchecked_transaction().expect("tx");
    let ids = files
        .iter()
        .map(|(filename, mtime)| {
            let extension = filename
                .rsplit_once('.')
                .map(|(_, ext)| ext.to_ascii_lowercase())
                .unwrap_or_default();
            let image = NewImageMetadata {
                root_id,
                rel_path: filename.clone(),
                filename: filename.clone(),
                extension,
                size_bytes: 1_000,
                mtime: *mtime,
                width: None,
                height: None,
                orientation: None,
                taken_at: None,
                gps_lat: None,
                gps_lng: None,
                camera_make: None,
                camera_model: None,
            };
            images_repo::upsert_scanned_image(&tx, &image, 1)
                .expect("insert")
                .image_id()
        })
        .collect();
    tx.commit().expect("commit");
    ids
}

fn tag(pool: &Pool, image_id: i64, names: &[&str]) {
    let mut conn = pool.get().expect("conn");
    let tags = names
        .iter()
        .map(|name| NewTagScore {
            name: (*name).to_string(),
            score: 0.9,
            source: "ai".to_string(),
            category: None,
        })
        .collect::<Vec<_>>();
    tags_repo::replace_image_tags(&mut conn, image_id, &tags, 1).expect("tags");
}

/// `primary * e0 + secondary * e_axis` 归一化。查询向量固定是 e0：primary 越大离查询越近。
fn vector(primary: f32, axis: usize, secondary: f32) -> Vec<f32> {
    let mut v = vec![0.0_f32; DIMS];
    v[0] = primary;
    v[axis] += secondary;
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    v.iter().map(|x| x / norm).collect()
}

fn query_vector() -> Vec<f32> {
    vector(1.0, 1, 0.0)
}

async fn store_vectors(vectors: &LanceDbRepo, items: &[(i64, Vec<f32>)]) {
    let records = items
        .iter()
        .map(|(image_id, embedding)| EmbeddingRecord {
            image_id: *image_id,
            model: MODEL.to_string(),
            embedding: embedding.clone(),
            created_at: 1,
        })
        .collect::<Vec<_>>();
    vectors
        .upsert_embeddings(&clip_table(), &records)
        .await
        .expect("upsert");
}

fn args(text: &str, root_ids: Option<Vec<i64>>, limit: usize) -> AiSearchArgs {
    AiSearchArgs {
        text: text.to_string(),
        root_ids,
        limit: Some(limit),
    }
}

fn ids_of(results: &[AiSearchResult]) -> Vec<i64> {
    results.iter().map(|result| result.image.id).collect()
}

fn id_set(results: &[AiSearchResult]) -> HashSet<i64> {
    results.iter().map(|result| result.image.id).collect()
}

fn assert_sources(results: &[AiSearchResult], source: &str) {
    for result in results {
        assert_eq!(result.source, source, "image {} 的来源", result.image.id);
    }
}

/// 假 encoder：`text` / `image` 为 `None` 时模拟 worker 兜底（伪向量）。
#[derive(Default)]
struct FakeEncoder {
    text: Option<Vec<f32>>,
    image: Option<Vec<f32>>,
    text_calls: AtomicUsize,
    image_calls: AtomicUsize,
}

impl FakeEncoder {
    fn with_text(text: Vec<f32>) -> Self {
        Self {
            text: Some(text),
            ..Self::default()
        }
    }

    fn fallback() -> Self {
        Self::default()
    }
}

#[async_trait]
impl QueryEncoder for FakeEncoder {
    fn clip_model(&self) -> &str {
        MODEL
    }

    async fn encode_text(&self, _text: &str) -> AppResult<Option<Vec<f32>>> {
        self.text_calls.fetch_add(1, Ordering::SeqCst);
        Ok(self.text.clone())
    }

    async fn encode_image(
        &self,
        _image_id: i64,
        _thumb_path: String,
    ) -> AppResult<Option<Vec<f32>>> {
        self.image_calls.fetch_add(1, Ordering::SeqCst);
        Ok(self.image.clone())
    }
}

/// 调用即报错：纯 DSL 查询 / 候选集为空时不应唤醒 worker。
struct UnreachableEncoder;

#[async_trait]
impl QueryEncoder for UnreachableEncoder {
    fn clip_model(&self) -> &str {
        MODEL
    }

    async fn encode_text(&self, text: &str) -> AppResult<Option<Vec<f32>>> {
        Err(AppError::Other(format!(
            "encoder must not be called: {text}"
        )))
    }

    async fn encode_image(
        &self,
        image_id: i64,
        _thumb_path: String,
    ) -> AppResult<Option<Vec<f32>>> {
        Err(AppError::Other(format!(
            "encoder must not be called: image {image_id}"
        )))
    }
}

// ---------------------------------------------------------------------------
// (a) 纯 DSL：#标签 在 SQL 里过滤，不受默认排序前 N 行限制
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tag_only_query_finds_images_beyond_first_400_rows() {
    let env = env();
    let root = add_root(&env.pool, "phone");
    // 600 张，mtime 递减 → 默认排序（mtime DESC）下 index 即行号。
    // 带标签的放在最后 30 行（570..600）；其中 580..590 另有"沙发"；595..600 是 png。
    let files = (0..600)
        .map(|i| {
            let ext = if i >= 595 { "png" } else { "jpg" };
            (format!("IMG_{i:04}.{ext}"), 100_000 - i as i64)
        })
        .collect::<Vec<_>>();
    let ids = add_images(&env.pool, root, &files);
    for (index, id) in ids.iter().enumerate().skip(570) {
        if (580..590).contains(&index) {
            tag(&env.pool, *id, &["猫", "沙发"]);
        } else {
            tag(&env.pool, *id, &["猫"]);
        }
    }
    // 噪声：前 400 行里也有别的标签，不应混进结果。
    for id in ids.iter().take(50) {
        tag(&env.pool, *id, &["狗"]);
    }
    let encoder = UnreachableEncoder;

    // 前端默认 limit=80：旧实现只取默认排序前 400 行再筛标签，这里会是 0 条。
    let results = search_service::search(&env.pool, &env.vectors, &encoder, &args("#猫", None, 80))
        .await
        .expect("search")
        .results;
    assert_eq!(results.len(), 30, "570..600 全部 30 张都要找到");
    assert_eq!(
        ids_of(&results),
        ids[570..600].to_vec(),
        "按默认排序 mtime DESC"
    );
    assert_sources(&results, SOURCE_DSL);
    assert!(results.iter().all(|result| result.score == 1.0));

    // LIMIT 在 SQL 里、过滤之后生效：取到的是带标签图里最新的 10 张。
    let top10 = search_service::search(&env.pool, &env.vectors, &encoder, &args("#猫", None, 10))
        .await
        .expect("search")
        .results;
    assert_eq!(ids_of(&top10), ids[570..580].to_vec());

    // 多标签是 AND；重复标签不影响结果。
    let both = search_service::search(
        &env.pool,
        &env.vectors,
        &encoder,
        &args("#猫 #沙发", None, 80),
    )
    .await
    .expect("search")
    .results;
    assert_eq!(ids_of(&both), ids[580..590].to_vec());
    let dup = search_service::search(
        &env.pool,
        &env.vectors,
        &encoder,
        &args("#猫 #猫", None, 80),
    )
    .await
    .expect("search")
    .results;
    assert_eq!(dup.len(), 30);

    // 标签 + 元数据条件一起在 SQL 里过滤。
    let png = search_service::search(
        &env.pool,
        &env.vectors,
        &encoder,
        &args("#猫 format:png", None, 80),
    )
    .await
    .expect("search")
    .results;
    assert_eq!(ids_of(&png), ids[595..600].to_vec());

    // 目录过滤：另一个目录里同标签的图不出现。
    let other = add_root(&env.pool, "nas");
    let other_ids = add_images(&env.pool, other, &[("cat.jpg".to_string(), 1)]);
    tag(&env.pool, other_ids[0], &["猫"]);
    let scoped = search_service::search(
        &env.pool,
        &env.vectors,
        &encoder,
        &args("#猫", Some(vec![root]), 200),
    )
    .await
    .expect("search")
    .results;
    assert_eq!(scoped.len(), 30);
    assert!(!id_set(&scoped).contains(&other_ids[0]));

    // 不存在的标签：空结果，不报错。
    let none = search_service::search(
        &env.pool,
        &env.vectors,
        &encoder,
        &args("#不存在", None, 80),
    )
    .await
    .expect("search")
    .results;
    assert!(none.is_empty());
}

// ---------------------------------------------------------------------------
// (b) 语义检索：过滤条件先在 SQL 里求候选集，再交给 LanceDB prefilter
// ---------------------------------------------------------------------------

#[tokio::test]
async fn semantic_search_with_filters_is_not_crowded_out_by_global_top_k() {
    let env = env();
    let target = add_root(&env.pool, "target");
    let noise = add_root(&env.pool, "noise");

    // 目标目录 5 张，离查询较远，且彼此远近有序（k 越大越近）；第 0 张是 heic。
    let target_files = (0..5)
        .map(|k| {
            let ext = if k == 0 { "heic" } else { "jpg" };
            (format!("target_{k}.{ext}"), 10 + k as i64)
        })
        .collect::<Vec<_>>();
    let target_ids = add_images(&env.pool, target, &target_files);
    // 噪声目录 1000 张，全都比目标目录的图更接近查询向量。
    let noise_files = (0..1000)
        .map(|i| (format!("noise_{i:04}.jpg"), 1_000 + i as i64))
        .collect::<Vec<_>>();
    let noise_ids = add_images(&env.pool, noise, &noise_files);

    let mut items = target_ids
        .iter()
        .enumerate()
        .map(|(k, id)| (*id, vector(0.3 + 0.1 * k as f32, 2 + k, 1.0)))
        .collect::<Vec<_>>();
    items.extend(
        noise_ids
            .iter()
            .enumerate()
            .map(|(i, id)| (*id, vector(1.0, 10 + i % 400, 0.05))),
    );
    store_vectors(&env.vectors, &items).await;
    let encoder = FakeEncoder::with_text(query_vector());

    // 不带过滤：全局 top-k 全是噪声目录（对照组）。
    let global =
        search_service::search(&env.pool, &env.vectors, &encoder, &args("sunset", None, 40))
            .await
            .expect("search")
            .results;
    assert_eq!(global.len(), 40);
    assert!(global.iter().all(|result| result.image.root_id == noise));
    assert_sources(&global, SOURCE_FUSION);

    // 选中目标目录：旧实现先取全局前 200 再按目录筛，会是 0 条。
    let scoped = search_service::search(
        &env.pool,
        &env.vectors,
        &encoder,
        &args("sunset", Some(vec![target]), 40),
    )
    .await
    .expect("search")
    .results;
    let expected = target_ids.iter().rev().copied().collect::<Vec<_>>();
    assert_eq!(ids_of(&scoped), expected, "候选集内按距离由近到远");
    assert_sources(&scoped, SOURCE_FUSION);
    assert!(scoped.windows(2).all(|pair| pair[0].score >= pair[1].score));

    // 带 #标签（不选目录）同样只在候选集里检索。
    tag(&env.pool, target_ids[1], &["日落"]);
    tag(&env.pool, target_ids[3], &["日落"]);
    let tagged = search_service::search(
        &env.pool,
        &env.vectors,
        &encoder,
        &args("sunset #日落", None, 40),
    )
    .await
    .expect("search")
    .results;
    assert_eq!(ids_of(&tagged), vec![target_ids[3], target_ids[1]]);
    assert_sources(&tagged, SOURCE_FUSION);

    // format: 条件。
    let heic = search_service::search(
        &env.pool,
        &env.vectors,
        &encoder,
        &args("sunset format:heic", None, 40),
    )
    .await
    .expect("search")
    .results;
    assert_eq!(ids_of(&heic), vec![target_ids[0]]);

    // 已删除的图不在候选集里。
    {
        let conn = env.pool.get().expect("conn");
        images_repo::mark_deleted_by_id(&conn, target_ids[4], 99).expect("delete");
    }
    let after_delete = search_service::search(
        &env.pool,
        &env.vectors,
        &encoder,
        &args("sunset", Some(vec![target]), 40),
    )
    .await
    .expect("search")
    .results;
    assert_eq!(
        ids_of(&after_delete),
        vec![target_ids[3], target_ids[2], target_ids[1], target_ids[0]]
    );

    // 大候选集（1000 个 id 的 IN 预过滤）也正常工作。
    let big = search_service::search(
        &env.pool,
        &env.vectors,
        &encoder,
        &args("sunset", Some(vec![noise]), 40),
    )
    .await
    .expect("search")
    .results;
    assert_eq!(big.len(), 40);
    assert!(big.iter().all(|result| result.image.root_id == noise));

    // 候选集为空：直接返回空，连 encoder 都不调用。
    let calls_before = encoder.text_calls.load(Ordering::SeqCst);
    let empty = search_service::search(
        &env.pool,
        &env.vectors,
        &encoder,
        &args("sunset #不存在", None, 40),
    )
    .await
    .expect("search")
    .results;
    assert!(empty.is_empty());
    assert_eq!(encoder.text_calls.load(Ordering::SeqCst), calls_before);
}

#[tokio::test]
async fn lancedb_top_k_filtered_prefilters_candidates() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = LanceDbRepo::new(dir.path().join("vectors"));
    let table = clip_table();
    let query = query_vector();

    // 表还不存在：空结果，不报错。
    assert!(repo
        .top_k_filtered(&table, &query, 10, Some(&[1]))
        .await
        .expect("missing table")
        .is_empty());

    store_vectors(
        &repo,
        &[
            (1, vector(1.0, 1, 0.0)),
            (2, vector(1.0, 2, 0.2)),
            (3, vector(0.1, 3, 1.0)),
        ],
    )
    .await;

    let all = repo.top_k(&table, &query, 10).await.expect("top_k");
    assert_eq!(
        all.iter().map(|hit| hit.image_id).collect::<Vec<_>>(),
        vec![1, 2, 3]
    );

    // limit=1 且只允许最远的 3：prefilter 必须返回 3，而不是"取全局最近 1 条再过滤"得到空。
    let only_far = repo
        .top_k_filtered(&table, &query, 1, Some(&[3]))
        .await
        .expect("filtered");
    assert_eq!(
        only_far.iter().map(|hit| hit.image_id).collect::<Vec<_>>(),
        vec![3]
    );

    let two = repo
        .top_k_filtered(&table, &query, 1, Some(&[2, 3]))
        .await
        .expect("filtered");
    assert_eq!(
        two.iter().map(|hit| hit.image_id).collect::<Vec<_>>(),
        vec![2]
    );

    assert!(repo
        .top_k_filtered(&table, &query, 10, Some(&[]))
        .await
        .expect("empty candidates")
        .is_empty());
    assert!(repo
        .top_k_filtered(&table, &query, 10, Some(&[99]))
        .await
        .expect("absent id")
        .is_empty());
}

// ---------------------------------------------------------------------------
// (c) 文件名（全文通道 / LIKE 兜底）保留全部过滤条件（含 #标签）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn filename_matches_keep_tag_and_root_filters() {
    let env = env();
    let root = add_root(&env.pool, "phone");
    let other = add_root(&env.pool, "nas");
    let ids = add_images(
        &env.pool,
        root,
        &[
            ("beach_001.jpg".to_string(), 3),
            ("beach_002.jpg".to_string(), 2),
            ("mountain_003.jpg".to_string(), 1),
        ],
    );
    let other_ids = add_images(&env.pool, other, &[("beach_100.jpg".to_string(), 4)]);
    tag(&env.pool, ids[0], &["海边"]);
    tag(&env.pool, ids[2], &["海边"]);
    tag(&env.pool, other_ids[0], &["海边"]);
    // 向量库是空的 → 图像向量通道无结果；文件名里的词由全文通道命中。
    let encoder = FakeEncoder::with_text(query_vector());

    let tagged = search_service::search(
        &env.pool,
        &env.vectors,
        &encoder,
        &args("beach #海边", None, 40),
    )
    .await
    .expect("search")
    .results;
    assert_eq!(id_set(&tagged), HashSet::from([other_ids[0], ids[0]]));
    assert_sources(&tagged, SOURCE_FUSION);
    assert!(tagged
        .iter()
        .all(|result| result.matched_by == vec!["filename".to_string()]));

    let scoped = search_service::search(
        &env.pool,
        &env.vectors,
        &encoder,
        &args("beach #海边", Some(vec![root]), 40),
    )
    .await
    .expect("search")
    .results;
    assert_eq!(ids_of(&scoped), vec![ids[0]]);

    let untagged = search_service::search(
        &env.pool,
        &env.vectors,
        &encoder,
        &args("beach", Some(vec![root]), 40),
    )
    .await
    .expect("search")
    .results;
    assert_eq!(id_set(&untagged), HashSet::from([ids[0], ids[1]]));
    assert_eq!(encoder.text_calls.load(Ordering::SeqCst), 3);

    // 文件名的一部分（不是完整的词）各通道都不命中 → 退回 LIKE 匹配，过滤条件一个不丢。
    let partial = search_service::search(
        &env.pool,
        &env.vectors,
        &encoder,
        &args("each #海边", Some(vec![root]), 40),
    )
    .await
    .expect("search")
    .results;
    assert_eq!(ids_of(&partial), vec![ids[0]]);
    assert_sources(&partial, SOURCE_FILENAME);
    assert!(partial.iter().all(|result| result.score == 0.5));
}

// ---------------------------------------------------------------------------
// (d) encoder 报兜底：不用伪向量检索
// ---------------------------------------------------------------------------

#[tokio::test]
async fn fallback_encoder_skips_semantic_channel() {
    let env = env();
    let root = add_root(&env.pool, "phone");
    let ids = add_images(
        &env.pool,
        root,
        &[
            ("beach_1.jpg".to_string(), 4),
            ("beach_2.jpg".to_string(), 3),
            ("forest.jpg".to_string(), 2),
            ("no_vector.jpg".to_string(), 1),
        ],
    );
    // forest 离查询最近：如果误用了向量检索，它会排第一。
    store_vectors(
        &env.vectors,
        &[
            (ids[0], vector(0.2, 2, 1.0)),
            (ids[1], vector(0.3, 3, 1.0)),
            (ids[2], vector(1.0, 4, 0.0)),
        ],
    )
    .await;
    let encoder = FakeEncoder::fallback();

    // 兜底：图像向量通道跳过（响应标 degraded），只剩文字通道。
    let response =
        search_service::search(&env.pool, &env.vectors, &encoder, &args("beach", None, 40))
            .await
            .expect("search");
    assert!(response.degraded);
    assert!(response.degraded_reason.is_some());
    let results = response.results;
    assert_eq!(id_set(&results), HashSet::from([ids[0], ids[1]]));
    assert!(!id_set(&results).contains(&ids[2]), "不能用伪向量检索");
    assert_sources(&results, SOURCE_FUSION);

    let nothing =
        search_service::search(&env.pool, &env.vectors, &encoder, &args("sunset", None, 40))
            .await
            .expect("search");
    assert!(nothing.degraded);
    assert!(nothing.results.is_empty());

    // 以图搜图：库里没有该图向量、临时编码又是兜底 → 空结果，不报错。
    {
        let conn = env.pool.get().expect("conn");
        images_repo::update_thumbnail_ready(&conn, ids[3], "abcdef0123", None, None)
            .expect("thumb");
    }
    let thumbs = env.dir.path().join("thumbs");
    let by_fallback = search_service::search_by_image(
        &env.pool,
        &env.vectors,
        &encoder,
        &thumbs,
        ids[3],
        Some(10),
    )
    .await
    .expect("by image");
    assert!(by_fallback.is_empty());
    assert_eq!(encoder.image_calls.load(Ordering::SeqCst), 1);

    // 库里已有向量：直接用，不调用 encoder；结果排除自身。
    let by_stored = search_service::search_by_image(
        &env.pool,
        &env.vectors,
        &UnreachableEncoder,
        &thumbs,
        ids[2],
        Some(10),
    )
    .await
    .expect("by image");
    assert_eq!(ids_of(&by_stored), vec![ids[1], ids[0]]);
    assert_sources(&by_stored, SOURCE_IMAGE);
}

// ---------------------------------------------------------------------------
// (e) 兜底结果不入库 + 退避
// ---------------------------------------------------------------------------

#[tokio::test]
async fn fallback_embedding_response_is_not_written() {
    let env = env();
    let root = add_root(&env.pool, "phone");
    let ids = add_images(
        &env.pool,
        root,
        &[("a.jpg".to_string(), 2), ("b.jpg".to_string(), 1)],
    );
    {
        let conn = env.pool.get().expect("conn");
        for id in &ids {
            images_repo::update_thumbnail_ready(&conn, *id, "abcdef0123", None, None)
                .expect("thumb");
        }
    }
    let response = |fallback: bool| ClipEmbedResponse {
        items: vec![
            ClipEmbedResult {
                id: ids[0],
                embedding: Some(query_vector()),
                error: None,
            },
            ClipEmbedResult {
                id: ids[1],
                embedding: None,
                error: Some("thumbnail missing".to_string()),
            },
        ],
        model: MODEL.to_string(),
        dims: DIMS,
        fallback,
    };
    let pending = || {
        let conn = env.pool.get().expect("conn");
        ai_jobs_repo::pending_image_embedding(&conn, MODEL, 10).expect("pending")
    };
    let counts = || {
        let conn = env.pool.get().expect("conn");
        ai_jobs_repo::counts(&conn, KIND_IMAGE_EMBEDDING, MODEL).expect("counts")
    };

    let outcome = ai_pipeline::apply_embed_response(&env.pool, &env.vectors, response(true), 1)
        .await
        .expect("apply fallback");
    assert!(outcome.fallback);
    assert_eq!((outcome.stored, outcome.failed), (0, 0));
    assert_eq!(
        env.vectors.count(&clip_table()).await.expect("count"),
        0,
        "伪向量不能进 LanceDB"
    );
    assert_eq!(
        pending(),
        ids,
        "保持待处理，真模型可用后重算；兜底批次里的单条错误也不落库"
    );
    assert_eq!(counts(), ai_jobs_repo::JobCounts::default());

    let outcome = ai_pipeline::apply_embed_response(&env.pool, &env.vectors, response(false), 2)
        .await
        .expect("apply real");
    assert!(!outcome.fallback);
    assert_eq!((outcome.stored, outcome.failed), (1, 1));
    assert_eq!(env.vectors.count(&clip_table()).await.expect("count"), 1);
    let after = counts();
    assert_eq!((after.ready, after.failed), (1, 1));
    assert_eq!(pending(), vec![ids[1]], "失败的会重试（attempts < 3）");
}

#[test]
fn fallback_tag_response_is_not_written() {
    let env = env();
    let root = add_root(&env.pool, "phone");
    let ids = add_images(&env.pool, root, &[("a.jpg".to_string(), 1)]);
    let response = |fallback: bool| TaggerResponse {
        items: vec![TaggerResult {
            id: ids[0],
            tags: vec![TagScore {
                name: "蓝色".to_string(),
                score: 0.82,
                category: Some("color".to_string()),
            }],
            error: None,
        }],
        model: "clip-zero-shot".to_string(),
        fallback,
    };
    let state = || {
        let conn = env.pool.get().expect("conn");
        let tags = tags_repo::tags_for_image(&conn, ids[0]).expect("tags");
        let status = images_repo::get_detail(&conn, ids[0])
            .expect("detail")
            .expect("exists")
            .tag_status;
        (tags.len(), status)
    };

    let outcome = ai_pipeline::apply_tag_response(&env.pool, response(true), 1).expect("apply");
    assert!(outcome.fallback);
    assert_eq!(outcome.tagged, 0);
    assert_eq!(state(), (0, "pending".to_string()), "占位标签不入库");

    let outcome = ai_pipeline::apply_tag_response(&env.pool, response(false), 2).expect("apply");
    assert!(!outcome.fallback);
    assert_eq!(outcome.tagged, 1);
    assert_eq!(state(), (1, "ready".to_string()));
}

#[test]
fn fallback_backoff_warns_once_per_worker_and_expires() {
    let t0 = Instant::now();
    let at = |secs: u64| t0 + Duration::from_secs(secs);
    let mut backoff = FallbackBackoff::default();
    assert!(!backoff.is_active(t0, Some(1)));

    assert!(backoff.record_fallback(at(0), Some(1)), "首次兜底要 warn");
    assert!(backoff.is_active(at(1), Some(1)));
    assert!(
        !backoff.record_fallback(at(2), Some(1)),
        "同一 worker 再次兜底不重复 warn"
    );
    assert!(backoff.is_active(at(2) + FALLBACK_BACKOFF - Duration::from_secs(1), Some(1)));
    assert!(
        !backoff.is_active(at(2) + FALLBACK_BACKOFF, Some(1)),
        "退避窗口到期后再探测"
    );
    assert!(
        !backoff.is_active(at(3), Some(2)),
        "worker 换了进程（重启）立即解除退避"
    );
    assert!(!backoff.is_active(at(3), None));
    assert!(
        backoff.record_fallback(at(4), Some(2)),
        "新 worker 进程兜底再 warn 一次"
    );

    backoff.record_real();
    assert!(!backoff.is_active(at(5), Some(2)));
    assert!(
        backoff.record_fallback(at(6), Some(2)),
        "恢复真模型后再兜底要重新 warn"
    );
}

// ---------------------------------------------------------------------------
// DSL 解析
// ---------------------------------------------------------------------------

#[test]
fn parse_search_filter_splits_dsl_tokens_and_text() {
    let filter = search_service::parse_search_filter(
        "日落 #风景 #海边 format:HEIC format:.jpg size:>5MB size:<=1.5GB 海边",
    );
    assert_eq!(filter.text, "日落 海边");
    assert_eq!(filter.tags, vec!["风景".to_string(), "海边".to_string()]);
    assert_eq!(
        filter.formats,
        Some(vec!["heic".to_string(), "jpg".to_string()])
    );
    assert_eq!(filter.size_min, Some(5 * 1024 * 1024));
    assert_eq!(filter.size_max, Some(1_610_612_736));
    assert!(filter.has_filter());
    assert!(!filter.is_empty());
}

#[test]
fn parse_search_filter_edge_cases() {
    // 空的 # / format: 当普通文本；无法识别的 size: 整段忽略；重复标签去重。
    let filter = search_service::parse_search_filter("  # format: size:abc size:5MB #猫 #猫  ");
    assert_eq!(filter.text, "# format:");
    assert_eq!(filter.tags, vec!["猫".to_string()]);
    assert_eq!(filter.formats, None);
    assert_eq!((filter.size_min, filter.size_max), (None, None));

    assert!(search_service::parse_search_filter("   ").is_empty());
    assert!(search_service::parse_search_filter("size:abc").is_empty());
    let only_format = search_service::parse_search_filter("format:png");
    assert!(only_format.text.is_empty());
    assert!(only_format.has_metadata_filter() && !only_format.is_empty());

    let ge = search_service::parse_search_filter("size:>=10kb");
    assert_eq!(ge.size_min, Some(10 * 1024));
    let lt = search_service::parse_search_filter("size:<300");
    assert_eq!(lt.size_max, Some(300));
}

#[test]
fn parse_size_bytes_units() {
    use search_service::parse_size_bytes;
    assert_eq!(parse_size_bytes("1024"), Some(1024));
    assert_eq!(parse_size_bytes("2kb"), Some(2048));
    assert_eq!(parse_size_bytes("1.5MB"), Some(1_572_864));
    assert_eq!(parse_size_bytes("1GB"), Some(1_073_741_824));
    assert_eq!(parse_size_bytes(" 7KB "), Some(7 * 1024));
    assert_eq!(parse_size_bytes("-1MB"), None);
    assert_eq!(parse_size_bytes("abc"), None);
    assert_eq!(parse_size_bytes(""), None);
    assert_eq!(parse_size_bytes("inf"), None);
}

#[tokio::test]
async fn empty_query_returns_nothing_without_calling_worker() {
    let env = env();
    let root = add_root(&env.pool, "phone");
    add_images(&env.pool, root, &[("a.jpg".to_string(), 1)]);
    let results = search_service::search(
        &env.pool,
        &env.vectors,
        &UnreachableEncoder,
        &args("   ", Some(vec![root]), 40),
    )
    .await
    .expect("search")
    .results;
    assert!(results.is_empty());
}
