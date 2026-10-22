//! docs/10 A4：描述文本向量通道。
//! `text_hash()` SQL 函数与 Rust 实现一致；描述变了 / 换模型自动重算；兜底伪向量不入库；
//! 搜索时第四路（描述语义）参与融合并标出命中来源；文本向量编码失败时降级。

use std::collections::HashSet;

use async_trait::async_trait;
use photo_view_plus_lib::db::{self, Pool};
use photo_view_plus_lib::repo::ai_jobs_repo::{self, KIND_TEXT_EMBEDDING};
use photo_view_plus_lib::repo::analysis_repo::{self, ImageAnalysis};
use photo_view_plus_lib::repo::images_repo::{self, NewImageMetadata};
use photo_view_plus_lib::repo::lancedb_repo::{LanceDbRepo, VectorTable};
use photo_view_plus_lib::repo::roots_repo::{self, NewRoot};
use photo_view_plus_lib::services::ai_client::TextEmbedResponse;
use photo_view_plus_lib::services::ai_pipeline;
use photo_view_plus_lib::services::search_service::{
    self, AiSearchArgs, Channel, QueryEncoder, SearchOptions,
};
use photo_view_plus_lib::{AppError, AppResult};
use tempfile::TempDir;

const SMALL: &str = "bge-small-zh-v1.5";
const BASE: &str = "bge-base-zh-v1.5";
const CLIP: &str = "chinese-clip-vit-b-16";

struct Env {
    pool: Pool,
    vectors: LanceDbRepo,
    _dir: TempDir,
}

fn env() -> Env {
    let dir = tempfile::tempdir().expect("tempdir");
    Env {
        pool: db::open(&dir.path().join("app.sqlite")).expect("db"),
        vectors: LanceDbRepo::new(dir.path().join("vectors")),
        _dir: dir,
    }
}

fn seed(pool: &Pool, descriptions: &[&str]) -> Vec<i64> {
    let conn = pool.get().expect("conn");
    let root = roots_repo::insert(
        &conn,
        &NewRoot {
            path: "D:/photos".to_string(),
            label: None,
            root_type: "local".to_string(),
        },
        1,
    )
    .expect("root")
    .id;
    descriptions
        .iter()
        .enumerate()
        .map(|(index, description)| {
            let name = format!("{index}.jpg");
            let id = images_repo::upsert_scanned_image(
                &conn,
                &NewImageMetadata {
                    root_id: root,
                    rel_path: name.clone(),
                    filename: name,
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
            .expect("image")
            .image_id();
            if !description.is_empty() {
                set_description(pool, id, description);
            }
            id
        })
        .collect()
}

fn set_description(pool: &Pool, image_id: i64, description: &str) {
    let conn = pool.get().expect("conn");
    analysis_repo::upsert(
        &conn,
        &ImageAnalysis {
            image_id,
            source: "memora".to_string(),
            model: "qwen3.5-9b".to_string(),
            prompt_version: "memora-m1".to_string(),
            description: description.to_string(),
            categories: Vec::new(),
            keywords: Vec::new(),
            people_count: None,
            visible_text: None,
            sensitive: false,
            raw_json: None,
            duration_ms: None,
            analyzed_at: 1,
        },
    )
    .expect("analysis");
}

fn pending(pool: &Pool, model: &str) -> Vec<i64> {
    let conn = pool.get().expect("conn");
    ai_jobs_repo::pending_text_embedding(&conn, model, 100).expect("pending")
}

fn unit(dims: usize, axis: usize) -> Vec<f32> {
    let mut v = vec![0.0; dims];
    v[axis] = 1.0;
    v
}

/// 按 `items` 造一个 worker 响应：第 i 条描述的向量在第 i 维上为 1。
fn response(
    items: &[(i64, String)],
    model: &str,
    dims: usize,
    fallback: bool,
) -> TextEmbedResponse {
    TextEmbedResponse {
        embeddings: (0..items.len()).map(|i| unit(dims, i + 1)).collect(),
        model: model.to_string(),
        dims,
        fallback,
    }
}

fn items(pool: &Pool, ids: &[i64]) -> Vec<(i64, String)> {
    let conn = pool.get().expect("conn");
    let texts = analysis_repo::texts_for(&conn, ids).expect("texts");
    ids.iter()
        .filter_map(|id| texts.get(id).map(|t| (*id, t.description.clone())))
        .collect()
}

#[test]
fn sql_text_hash_matches_rust() {
    let env = env();
    let conn = env.pool.get().expect("conn");
    let sql: String = conn
        .query_row("SELECT text_hash('海边的小孩')", [], |row| row.get(0))
        .expect("text_hash");
    assert_eq!(sql, db::text_hash("海边的小孩"));
    assert_eq!(sql.len(), 16);
    let null: Option<String> = conn
        .query_row("SELECT text_hash(NULL)", [], |row| row.get(0))
        .expect("null");
    assert_eq!(null, None);
}

#[tokio::test]
async fn text_embeddings_follow_description_and_model() {
    let env = env();
    let ids = seed(
        &env.pool,
        &["海边日落，两个小孩在堆沙堡", "一只橘猫趴在沙发上", ""],
    );
    assert_eq!(
        pending(&env.pool, SMALL),
        vec![ids[0], ids[1]],
        "没有描述的图不算"
    );

    // 兜底伪向量：不写 LanceDB、不记 ai_jobs。
    let batch = items(&env.pool, &ids[..2]);
    let outcome = ai_pipeline::apply_text_embed_response(
        &env.pool,
        &env.vectors,
        &batch,
        response(&batch, SMALL, 512, true),
        1,
    )
    .await
    .expect("fallback");
    assert!(outcome.fallback);
    assert_eq!(pending(&env.pool, SMALL), vec![ids[0], ids[1]]);
    let table = VectorTable::image_text(SMALL, 512);
    assert_eq!(env.vectors.count(&table).await.expect("count"), 0);

    // 真结果：写进该模型的表，ai_jobs 记下描述指纹。
    let outcome = ai_pipeline::apply_text_embed_response(
        &env.pool,
        &env.vectors,
        &batch,
        response(&batch, SMALL, 512, false),
        2,
    )
    .await
    .expect("apply");
    assert_eq!((outcome.stored, outcome.failed), (2, 0));
    assert!(pending(&env.pool, SMALL).is_empty());
    assert_eq!(env.vectors.count(&table).await.expect("count"), 2);

    // 描述被改写（例如 VLM 覆盖了 memora 的描述）→ 指纹对不上，自动重新待处理。
    set_description(&env.pool, ids[1], "一只橘猫趴在窗台上晒太阳");
    assert_eq!(pending(&env.pool, SMALL), vec![ids[1]]);
    // 换模型 → 全部重算（不同维度的表互不影响）。
    assert_eq!(pending(&env.pool, BASE), vec![ids[0], ids[1]]);
    let again = items(&env.pool, &[ids[1]]);
    ai_pipeline::apply_text_embed_response(
        &env.pool,
        &env.vectors,
        &again,
        response(&again, SMALL, 512, false),
        3,
    )
    .await
    .expect("recompute");
    assert!(pending(&env.pool, SMALL).is_empty());
    {
        let conn = env.pool.get().expect("conn");
        let counts = ai_jobs_repo::counts(&conn, KIND_TEXT_EMBEDDING, SMALL).expect("counts");
        assert_eq!(counts.ready, 2);
    }

    // 维度不对：记失败（可重试），不写表；条数对不上：整批报错。
    let wrong = items(&env.pool, &[ids[0]]);
    set_description(&env.pool, ids[0], "海边日落");
    let wrong = vec![(wrong[0].0, "海边日落".to_string())];
    let outcome = ai_pipeline::apply_text_embed_response(
        &env.pool,
        &env.vectors,
        &wrong,
        TextEmbedResponse {
            embeddings: vec![unit(768, 0)],
            model: SMALL.to_string(),
            dims: 512,
            fallback: false,
        },
        4,
    )
    .await
    .expect("dims mismatch");
    assert_eq!((outcome.stored, outcome.failed), (0, 1));
    assert_eq!(pending(&env.pool, SMALL), vec![ids[0]], "失败的会重试");
    assert!(ai_pipeline::apply_text_embed_response(
        &env.pool,
        &env.vectors,
        &wrong,
        TextEmbedResponse {
            embeddings: Vec::new(),
            model: SMALL.to_string(),
            dims: 512,
            fallback: false,
        },
        5,
    )
    .await
    .is_err());
}

/// 查询文本 → 描述向量固定为第 1 维；图像向量通道不可用（兜底）。
struct TextOnlyEncoder {
    fail_text: bool,
}

#[async_trait]
impl QueryEncoder for TextOnlyEncoder {
    fn clip_model(&self) -> &str {
        CLIP
    }

    async fn encode_text(&self, _text: &str) -> AppResult<Option<Vec<f32>>> {
        Ok(None)
    }

    async fn encode_image(&self, _id: i64, _path: String) -> AppResult<Option<Vec<f32>>> {
        Ok(None)
    }

    fn text_model(&self) -> Option<&str> {
        Some(SMALL)
    }

    async fn encode_query(&self, _text: &str) -> AppResult<Option<Vec<f32>>> {
        if self.fail_text {
            return Err(AppError::Other("文本向量模型加载中".to_string()));
        }
        Ok(Some(unit(512, 1)))
    }
}

#[tokio::test]
async fn description_vectors_join_the_fusion() {
    let env = env();
    // 0：描述向量最近（第 1 维）；1：描述里有查询词但向量远；2：都不沾边。
    let ids = seed(
        &env.pool,
        &[
            "一家人在沙滩上玩耍",
            "傍晚的海边，夕阳很美",
            "办公室的电脑桌",
        ],
    );
    let batch = items(&env.pool, &ids);
    ai_pipeline::apply_text_embed_response(
        &env.pool,
        &env.vectors,
        &batch,
        TextEmbedResponse {
            embeddings: vec![unit(512, 1), unit(512, 2), unit(512, 3)],
            model: SMALL.to_string(),
            dims: 512,
            fallback: false,
        },
        1,
    )
    .await
    .expect("apply");

    let args = AiSearchArgs {
        text: "海边".to_string(),
        root_ids: None,
        limit: Some(10),
    };
    let only_text = search_service::search_with(
        &env.pool,
        &env.vectors,
        &TextOnlyEncoder { fail_text: false },
        &args,
        &SearchOptions::only(&[Channel::TextVector]),
    )
    .await
    .expect("text vector only")
    .results;
    assert_eq!(only_text[0].image.id, ids[0]);
    assert_eq!(only_text[0].matched_by, vec!["semantic".to_string()]);

    // 全部通道：描述全文命中 1，描述向量命中 0；相似度低于下限（0.35）的 1、2 在文字通道有结果时被切掉。
    let fused = search_service::search(
        &env.pool,
        &env.vectors,
        &TextOnlyEncoder { fail_text: false },
        &args,
    )
    .await
    .expect("fused");
    assert!(fused.degraded, "图像向量处于兜底模式");
    let found = fused
        .results
        .iter()
        .map(|result| result.image.id)
        .collect::<HashSet<_>>();
    assert_eq!(found, HashSet::from([ids[0], ids[1]]));
    let by_id = |id: i64| {
        fused
            .results
            .iter()
            .find(|result| result.image.id == id)
            .expect("result")
    };
    assert_eq!(by_id(ids[0]).matched_by, vec!["semantic".to_string()]);
    assert_eq!(by_id(ids[1]).matched_by, vec!["description".to_string()]);
    assert!(by_id(ids[1]).snippet.is_some());

    // 文本向量编码失败：降级为文字通道，带原因。
    let degraded = search_service::search(
        &env.pool,
        &env.vectors,
        &TextOnlyEncoder { fail_text: true },
        &args,
    )
    .await
    .expect("degraded");
    assert!(degraded.degraded);
    assert_eq!(
        degraded
            .results
            .iter()
            .map(|result| result.image.id)
            .collect::<Vec<_>>(),
        vec![ids[1]]
    );
}
