//! docs/10 A2：全文通道与融合框架。
//! 分词（CJK 双字 + 单字兜底）、触发器 → 积压 → 重写索引、AND→OR 放宽、列权重、标签通道、
//! RRF 融合与相似度下限、命中来源与片段、worker 不可用时降级、全量重建性能。

use std::collections::HashSet;
use std::time::Instant;

use async_trait::async_trait;
use photo_view_plus_lib::db::{self, Pool};
use photo_view_plus_lib::repo::images_repo::{self, ImageQueryParams, NewImageMetadata};
use photo_view_plus_lib::repo::lancedb_repo::{EmbeddingRecord, LanceDbRepo, VectorTable};
use photo_view_plus_lib::repo::roots_repo::{self, NewRoot};
use photo_view_plus_lib::repo::tags_repo::{self, NewTagScore};
use photo_view_plus_lib::services::search_index_service;
use photo_view_plus_lib::services::search_service::{
    self, AiSearchArgs, AiSearchResult, Channel, FusionWeights, QueryEncoder, SearchOptions,
    SnippetPart, SOURCE_FUSION,
};
use photo_view_plus_lib::utils::text_tokens;
use photo_view_plus_lib::{AppError, AppResult};
use tempfile::TempDir;

const MODEL: &str = "chinese-clip-vit-b-16";
const DIMS: usize = 512;

struct Env {
    pool: Pool,
    vectors: LanceDbRepo,
    _dir: TempDir,
}

fn env() -> Env {
    let dir = tempfile::tempdir().expect("tempdir");
    let pool = db::open(&dir.path().join("app.sqlite")).expect("db");
    let vectors = LanceDbRepo::new(dir.path().join("vectors"));
    Env {
        pool,
        vectors,
        _dir: dir,
    }
}

fn add_root(pool: &Pool) -> i64 {
    let conn = pool.get().expect("conn");
    roots_repo::insert(
        &conn,
        &NewRoot {
            path: "D:/photos".to_string(),
            label: None,
            root_type: "local".to_string(),
        },
        1,
    )
    .expect("root")
    .id
}

fn add_images(pool: &Pool, root_id: i64, names: &[&str]) -> Vec<i64> {
    let conn = pool.get().expect("conn");
    let tx = conn.unchecked_transaction().expect("tx");
    let ids = names
        .iter()
        .enumerate()
        .map(|(index, name)| {
            images_repo::upsert_scanned_image(
                &tx,
                &NewImageMetadata {
                    root_id,
                    rel_path: (*name).to_string(),
                    filename: (*name).to_string(),
                    extension: "jpg".to_string(),
                    size_bytes: 1_000,
                    mtime: 1_000 - index as i64,
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
        .enumerate()
        .map(|(index, name)| NewTagScore {
            name: (*name).to_string(),
            score: 0.95 - index as f64 * 0.02,
            source: "ai".to_string(),
            category: None,
        })
        .collect::<Vec<_>>();
    tags_repo::replace_image_tags(&mut conn, image_id, &tags, 1).expect("tags");
}

fn set_ocr(pool: &Pool, image_id: i64, text: &str) {
    let conn = pool.get().expect("conn");
    images_repo::set_ocr_result(&conn, image_id, text, "ready").expect("ocr");
}

fn flush(pool: &Pool) {
    let mut conn = pool.get().expect("conn");
    search_index_service::flush_all(&mut conn).expect("flush");
}

fn fulltext(pool: &Pool, query: &str) -> Vec<i64> {
    flush(pool);
    let conn = pool.get().expect("conn");
    search_service::fulltext_channel(
        &conn,
        &text_tokens::query_tokens(query),
        &ImageQueryParams {
            include_deleted: Some(false),
            ..ImageQueryParams::default()
        },
        None,
    )
    .expect("fulltext")
    .into_iter()
    .map(|(id, _)| id)
    .collect()
}

fn tag_channel(pool: &Pool, query: &str) -> Vec<i64> {
    let conn = pool.get().expect("conn");
    search_service::tag_channel(
        &conn,
        query,
        &ImageQueryParams {
            include_deleted: Some(false),
            ..ImageQueryParams::default()
        },
    )
    .expect("tag channel")
    .into_iter()
    .map(|(id, _)| id)
    .collect()
}

fn args(text: &str, limit: usize) -> AiSearchArgs {
    AiSearchArgs {
        text: text.to_string(),
        root_ids: None,
        limit: Some(limit),
    }
}

fn ids_of(results: &[AiSearchResult]) -> Vec<i64> {
    results.iter().map(|result| result.image.id).collect()
}

fn unit(axis: usize, weight: f32) -> Vec<f32> {
    let mut v = vec![0.0_f32; DIMS];
    v[0] = weight;
    v[axis] = (1.0 - weight * weight).max(0.0).sqrt();
    v
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
        .upsert_embeddings(&VectorTable::image_clip(MODEL, DIMS), &records)
        .await
        .expect("upsert");
}

/// 查询向量固定为 e0：向量里 e0 分量越大，余弦相似度越高。
struct VectorEncoder;

#[async_trait]
impl QueryEncoder for VectorEncoder {
    fn clip_model(&self) -> &str {
        MODEL
    }

    async fn encode_text(&self, _text: &str) -> AppResult<Option<Vec<f32>>> {
        Ok(Some(unit(1, 1.0)))
    }

    async fn encode_image(&self, _id: i64, _path: String) -> AppResult<Option<Vec<f32>>> {
        Ok(None)
    }
}

/// worker 还在启动（或连不上）：编码报错。
struct StartingEncoder;

#[async_trait]
impl QueryEncoder for StartingEncoder {
    fn clip_model(&self) -> &str {
        MODEL
    }

    async fn encode_text(&self, _text: &str) -> AppResult<Option<Vec<f32>>> {
        Err(AppError::Other("语义检索启动中".to_string()))
    }

    async fn encode_image(&self, _id: i64, _path: String) -> AppResult<Option<Vec<f32>>> {
        Ok(None)
    }
}

// ---------------------------------------------------------------------------
// 分词
// ---------------------------------------------------------------------------

#[test]
fn tokenizer_splits_cjk_bigrams_chars_and_ascii_words() {
    let tokens = text_tokens::tokenize("海边的小孩 IMG_2024 ＡＢＣ");
    // "的" 是停用字：含它的双字（边的、的小）和它本身的单字都丢掉。
    assert_eq!(
        tokens.terms,
        vec!["海边", "小孩", "img", "2024", "abc"]
            .into_iter()
            .map(String::from)
            .collect::<Vec<_>>(),
        "全角 ＡＢＣ 经 NFKC 规范化成 abc"
    );
    assert_eq!(
        tokens.chars,
        vec!["海", "边", "小", "孩"]
            .into_iter()
            .map(String::from)
            .collect::<Vec<_>>()
    );

    // 只有一个汉字的片段本身就是一个词（标签"猫"、被标点隔开的字）；"在"是停用字，
    // "狗在""在跑"两个双字都含它，丢掉。
    let single = text_tokens::tokenize("猫，狗在跑");
    assert_eq!(single.terms, vec!["猫".to_string()]);
    assert_eq!(
        single.chars,
        vec!["猫".to_string(), "狗".to_string(), "跑".to_string()]
    );

    // 查询分词去重。
    let query = text_tokens::query_tokens("猫猫 猫猫");
    assert_eq!(query.terms, vec!["猫猫".to_string()]);
    assert_eq!(query.chars, vec!["猫".to_string()]);
    assert!(text_tokens::query_tokens("，。！ 的").is_empty());
}

#[test]
fn fts_match_query_requires_all_terms_then_relaxes() {
    let tokens = text_tokens::query_tokens("海边小孩");
    let columns = "{tags description}";
    assert_eq!(
        search_service::fts_match_query(&tokens, columns, true),
        "{tags description} : (\"海边\" AND \"边小\" AND \"小孩\")"
    );
    assert_eq!(
        search_service::fts_match_query(&tokens, columns, false),
        "{tags description} : (\"海边\" OR \"边小\" OR \"小孩\") OR chars : (\"海\" OR \"边\" OR \"小\" OR \"孩\")"
    );
    // 只有单字可查（单字全是停用字以外的字）：AND 也落到 chars 列。
    let lone = text_tokens::Tokens {
        terms: Vec::new(),
        chars: vec!["猫".to_string()],
    };
    assert_eq!(
        search_service::fts_match_query(&lone, columns, true),
        "chars : (\"猫\")"
    );
    // 引号要转义，防止注入 FTS 语法。
    assert_eq!(text_tokens::fts_quote("a\"b"), "\"a\"\"b\"");
}

// ---------------------------------------------------------------------------
// 索引维护：触发器 → 积压 → 重写
// ---------------------------------------------------------------------------

#[test]
fn triggers_queue_changes_and_flush_rewrites_the_index() {
    let env = env();
    let root = add_root(&env.pool);
    let ids = add_images(&env.pool, root, &["sunset_beach.jpg", "receipt.png"]);
    let pending = || {
        let conn = env.pool.get().expect("conn");
        search_index_service::pending_count(&conn).expect("pending")
    };
    assert_eq!(pending(), 2, "新入库的图排队建索引");
    flush(&env.pool);
    assert_eq!(pending(), 0);
    assert_eq!(fulltext(&env.pool, "sunset"), vec![ids[0]]);

    tag(&env.pool, ids[1], &["发票", "收据"]);
    assert_eq!(pending(), 1, "打标签 → 排队");
    assert_eq!(fulltext(&env.pool, "发票"), vec![ids[1]]);

    set_ocr(&env.pool, ids[0], "欢迎光临海边餐厅");
    assert_eq!(fulltext(&env.pool, "餐厅"), vec![ids[0]], "OCR 文本可检索");

    {
        let conn = env.pool.get().expect("conn");
        images_repo::rename_record(
            &conn,
            ids[0],
            &images_repo::RenameRecordPatch {
                rel_path: "holiday.jpg".to_string(),
                filename: "holiday.jpg".to_string(),
                extension: "jpg".to_string(),
                size_bytes: 1_000,
                mtime: 5,
                indexed_at: 5,
            },
        )
        .expect("rename");
    }
    assert!(
        fulltext(&env.pool, "sunset").is_empty(),
        "改名后旧文件名搜不到"
    );
    assert_eq!(fulltext(&env.pool, "holiday"), vec![ids[0]]);

    {
        let conn = env.pool.get().expect("conn");
        images_repo::mark_deleted_by_id(&conn, ids[1], 9).expect("delete");
    }
    assert!(
        fulltext(&env.pool, "发票").is_empty(),
        "删除（进回收站）后搜不到"
    );
    {
        let conn = env.pool.get().expect("conn");
        images_repo::restore_by_id(&conn, ids[1]).expect("restore");
    }
    assert_eq!(
        fulltext(&env.pool, "发票"),
        vec![ids[1]],
        "撤销删除后又能搜到"
    );

    // 全量重建结果一致。
    let mut conn = env.pool.get().expect("conn");
    let outcome = search_index_service::rebuild_all(&mut conn).expect("rebuild");
    assert_eq!(outcome.indexed, 2);
    drop(conn);
    assert_eq!(fulltext(&env.pool, "餐厅"), vec![ids[0]]);
    let status = {
        let conn = env.pool.get().expect("conn");
        search_index_service::status(&conn).expect("status")
    };
    assert_eq!((status.indexed, status.pending), (2, 0));
}

// ---------------------------------------------------------------------------
// 全文通道
// ---------------------------------------------------------------------------

#[test]
fn fulltext_prefers_all_terms_and_relaxes_when_too_few() {
    let env = env();
    let root = add_root(&env.pool);
    let names = (0..30).map(|i| format!("{i}.jpg")).collect::<Vec<_>>();
    let ids = add_images(
        &env.pool,
        root,
        &names.iter().map(String::as_str).collect::<Vec<_>>(),
    );
    // 0..3 同时有"海边"和"小孩"；3..30 只有"海边"。
    for id in &ids[..3] {
        set_ocr(&env.pool, *id, "海边 小孩");
    }
    for id in &ids[3..] {
        set_ocr(&env.pool, *id, "海边 风景");
    }
    // AND 只有 3 条（< 20）→ 放宽为 OR：两词都有的排前面，只含一个词的也召回。
    let relaxed = fulltext(&env.pool, "海边的小孩");
    assert_eq!(relaxed.len(), 30);
    assert_eq!(
        relaxed[..3].iter().copied().collect::<HashSet<_>>(),
        ids[..3].iter().copied().collect::<HashSet<_>>()
    );

    // AND 命中足够多（≥ 20）时不放宽：只含一个词的不出现。
    for id in &ids[3..25] {
        set_ocr(&env.pool, *id, "海边 小孩 玩沙");
    }
    let strict = fulltext(&env.pool, "海边的小孩");
    assert_eq!(strict.len(), 25);
    assert!(!strict.contains(&ids[29]));
}

#[test]
fn fulltext_weights_tags_above_ocr_and_single_chars_fall_back() {
    let env = env();
    let root = add_root(&env.pool);
    let ids = add_images(&env.pool, root, &["a.jpg", "b.jpg", "c.jpg"]);
    set_ocr(&env.pool, ids[0], "这家店的招牌是蛋糕和咖啡");
    tag(&env.pool, ids[1], &["蛋糕", "生日"]);
    set_ocr(&env.pool, ids[2], "小猫咪");
    // 标签列权重 4 > OCR 列权重 1。
    assert_eq!(fulltext(&env.pool, "蛋糕"), vec![ids[1], ids[0]]);
    // 单字查询："猫咪"切成一个双字词，只查"猫"时靠单字列兜住。
    assert_eq!(fulltext(&env.pool, "猫"), vec![ids[2]]);
    // ASCII 查询不受影响，查不到就是空。
    assert!(fulltext(&env.pool, "zebra").is_empty());
}

// ---------------------------------------------------------------------------
// 标签通道
// ---------------------------------------------------------------------------

#[test]
fn tag_channel_finds_tags_inside_the_query() {
    let env = env();
    let root = add_root(&env.pool);
    let ids = add_images(
        &env.pool,
        root,
        &["1.jpg", "2.jpg", "3.jpg", "4.jpg", "5.jpg"],
    );
    tag(&env.pool, ids[0], &["海边", "小孩"]);
    tag(&env.pool, ids[1], &["海边"]);
    tag(&env.pool, ids[2], &["小孩", "人"]);
    tag(&env.pool, ids[3], &["人"]);
    tag(&env.pool, ids[4], &["站/矗立/摊位"]);

    // "海边的小孩"：两个标签都出现在查询里；同时有两个的覆盖 4 个字，排第一。
    let both = tag_channel(&env.pool, "海边的小孩");
    assert_eq!(both[0], ids[0]);
    assert_eq!(
        both.iter().copied().collect::<HashSet<_>>(),
        HashSet::from([ids[0], ids[1], ids[2]])
    );
    // 单字标签只在查询本身就是这个字时算，"小孩"里的字不会命中"人"。
    assert!(!both.contains(&ids[3]));
    assert_eq!(
        tag_channel(&env.pool, "人")
            .into_iter()
            .collect::<HashSet<_>>(),
        HashSet::from([ids[2], ids[3]])
    );
    // RAM 带斜杠的歧义译名拆开匹配。
    assert_eq!(tag_channel(&env.pool, "路边的摊位"), vec![ids[4]]);
    assert!(tag_channel(&env.pool, "雪山").is_empty());
}

// ---------------------------------------------------------------------------
// 融合
// ---------------------------------------------------------------------------

#[tokio::test]
async fn fusion_combines_channels_with_rrf_and_similarity_floor() {
    let env = env();
    let root = add_root(&env.pool);
    let ids = add_images(&env.pool, root, &["1.jpg", "2.jpg", "3.jpg", "4.jpg"]);
    // 0：文字和图像都命中；1：只有文字；2：只有图像（相似度高）；3：只有图像（相似度低于下限）。
    set_ocr(&env.pool, ids[0], "海边日落");
    set_ocr(&env.pool, ids[1], "海边日落");
    store_vectors(
        &env.vectors,
        &[
            (ids[0], unit(2, 0.6)),
            (ids[2], unit(3, 0.7)),
            (ids[3], unit(4, 0.2)),
        ],
    )
    .await;
    let encoder = VectorEncoder;

    let response = search_service::search(&env.pool, &env.vectors, &encoder, &args("海边日落", 40))
        .await
        .expect("search");
    assert!(!response.degraded);
    let results = response.results;
    assert_eq!(results[0].image.id, ids[0], "两路都命中的排第一");
    assert!(results.iter().all(|result| result.source == SOURCE_FUSION));
    let found = ids_of(&results).into_iter().collect::<HashSet<_>>();
    assert!(found.contains(&ids[1]) && found.contains(&ids[2]));
    assert!(
        !found.contains(&ids[3]),
        "文字通道有结果时，低于相似度下限的图像向量命中不参与融合"
    );
    let first = &results[0];
    assert_eq!(
        first.matched_by,
        vec!["text".to_string(), "image".to_string()]
    );
    assert!(response
        .channels
        .iter()
        .any(|stat| stat.channel == Channel::ImageVector && stat.hits == 2));

    // 文字通道没有结果时，向量命中全部保留（包括低于下限的）。
    let vector_only = search_service::search(&env.pool, &env.vectors, &encoder, &args("夜景", 40))
        .await
        .expect("search")
        .results;
    assert_eq!(ids_of(&vector_only), vec![ids[2], ids[0], ids[3]]);
    assert!(vector_only
        .iter()
        .all(|result| result.matched_by == vec!["image".to_string()]));

    // 只跑文字通道（评测用的选项）。
    let text_only = search_service::search_with(
        &env.pool,
        &env.vectors,
        &encoder,
        &args("海边日落", 40),
        &SearchOptions::only(&[Channel::FullText]),
    )
    .await
    .expect("search")
    .results;
    assert_eq!(
        ids_of(&text_only).into_iter().collect::<HashSet<_>>(),
        HashSet::from([ids[0], ids[1]])
    );

    // 权重为 0 的通道不参与融合。
    let no_image = search_service::search_with(
        &env.pool,
        &env.vectors,
        &encoder,
        &args("海边日落", 40),
        &SearchOptions {
            weights: FusionWeights {
                image_vector: 0.0,
                ..FusionWeights::default()
            },
            ..SearchOptions::default()
        },
    )
    .await
    .expect("search")
    .results;
    assert!(!ids_of(&no_image).contains(&ids[2]));
}

#[tokio::test]
async fn results_carry_snippets_with_highlighted_terms() {
    let env = env();
    let root = add_root(&env.pool);
    let ids = add_images(&env.pool, root, &["1.jpg"]);
    let long = format!(
        "{}这是一张发票，金额一百元{}",
        "前".repeat(30),
        "后".repeat(100)
    );
    set_ocr(&env.pool, ids[0], &long);
    let results = search_service::search_with(
        &env.pool,
        &env.vectors,
        &VectorEncoder,
        &args("发票", 10),
        &SearchOptions::only(&[Channel::FullText]),
    )
    .await
    .expect("search")
    .results;
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].matched_by, vec!["text".to_string()]);
    let snippet = results[0].snippet.clone().expect("snippet");
    let hits = snippet
        .iter()
        .filter(|part| part.hit)
        .map(|part| part.text.as_str())
        .collect::<Vec<_>>();
    assert_eq!(hits, vec!["发票"]);
    let text = snippet
        .iter()
        .map(|part| part.text.as_str())
        .collect::<String>();
    assert!(
        text.starts_with('…') && text.ends_with('…'),
        "两头截断：{text}"
    );
    assert!(text.chars().count() <= 82);

    // 高亮按规范化后的字比较：全角文本也能命中。
    let parts = search_service::highlight("ＩＭＧ收据", &["img".to_string()]).expect("hit");
    assert_eq!(
        parts,
        vec![
            SnippetPart {
                text: "ＩＭＧ".to_string(),
                hit: true
            },
            SnippetPart {
                text: "收据".to_string(),
                hit: false
            }
        ]
    );
    assert!(search_service::highlight("没有命中", &["发票".to_string()]).is_none());
}

#[tokio::test]
async fn unavailable_worker_degrades_to_text_channels() {
    let env = env();
    let root = add_root(&env.pool);
    let ids = add_images(&env.pool, root, &["1.jpg", "2.jpg"]);
    tag(&env.pool, ids[0], &["猫"]);
    let response =
        search_service::search(&env.pool, &env.vectors, &StartingEncoder, &args("猫", 10))
            .await
            .expect("worker 不可用不该让搜索失败");
    assert!(response.degraded);
    assert_eq!(response.degraded_reason.as_deref(), Some("语义检索启动中"));
    assert_eq!(ids_of(&response.results), vec![ids[0]]);
    assert_eq!(response.results[0].matched_by, vec!["tag".to_string()]);
}

// ---------------------------------------------------------------------------
// 性能：1.5 万张的库全量重建 < 30 秒；全文通道单次查询
// ---------------------------------------------------------------------------

#[test]
fn rebuild_and_query_15k_library_within_budget() {
    let env = env();
    let root = add_root(&env.pool);
    let vocabulary = [
        "海边",
        "沙滩",
        "小孩",
        "蛋糕",
        "生日",
        "猫",
        "狗",
        "汽车",
        "夜景",
        "城市",
        "雪山",
        "火锅",
        "婚礼",
        "花",
        "樱花",
        "截图",
        "聊天记录",
        "发票",
        "二维码",
        "风景",
    ];
    {
        let conn = env.pool.get().expect("conn");
        let tx = conn.unchecked_transaction().expect("tx");
        for i in 0..15_000usize {
            let name = format!("IMG_{i:05}.jpg");
            let id = images_repo::upsert_scanned_image(
                &tx,
                &NewImageMetadata {
                    root_id: root,
                    rel_path: name.clone(),
                    filename: name,
                    extension: "jpg".to_string(),
                    size_bytes: 1_000,
                    mtime: i as i64,
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
            .expect("insert")
            .image_id();
            for k in 0..5 {
                let tag_name = vocabulary[(i * 7 + k * 3) % vocabulary.len()];
                let tag_id = tags_repo::upsert_tag(&tx, tag_name, "ai", None, 1).expect("tag");
                tx.execute(
                    "INSERT OR IGNORE INTO image_tags(image_id, tag_id, score, source, created_at)
                     VALUES (?1, ?2, 0.9, 'ai', 1)",
                    rusqlite::params![id, tag_id],
                )
                .expect("image tag");
            }
            if i % 3 == 0 {
                images_repo::set_ocr_result(
                    &tx,
                    id,
                    "欢迎光临本店，今日特价火锅套餐，扫码点餐",
                    "ready",
                )
                .expect("ocr");
            }
        }
        tx.commit().expect("commit");
    }
    let mut conn = env.pool.get().expect("conn");
    let started = Instant::now();
    let outcome = search_index_service::rebuild_all(&mut conn).expect("rebuild");
    let rebuild_secs = started.elapsed().as_secs_f64();
    println!("rebuild {} docs in {rebuild_secs:.2}s", outcome.indexed);
    assert_eq!(outcome.indexed, 15_000);
    assert!(
        rebuild_secs < 30.0,
        "全量重建 {rebuild_secs:.1}s，超过 30 秒"
    );

    let params = ImageQueryParams {
        include_deleted: Some(false),
        ..ImageQueryParams::default()
    };
    let mut latencies = Vec::new();
    for query in [
        "海边的小孩",
        "猫",
        "火锅套餐",
        "生日蛋糕",
        "城市夜景",
        "扫码",
        "樱花",
        "发票",
    ] {
        let started = Instant::now();
        let hits = search_service::fulltext_channel(
            &conn,
            &text_tokens::query_tokens(query),
            &params,
            None,
        )
        .expect("fulltext");
        latencies.push(started.elapsed().as_secs_f64() * 1000.0);
        assert!(!hits.is_empty(), "{query}");
    }
    latencies.sort_by(f64::total_cmp);
    let worst = latencies.last().copied().unwrap_or(0.0);
    println!("fulltext latencies (ms, debug build): {latencies:?}");
    // 测试是 debug 构建（SQLite 未优化），门槛放宽；release 下的实测见 docs/10 A2。
    assert!(worst < 300.0, "全文通道最慢一次 {worst:.1}ms");
}
