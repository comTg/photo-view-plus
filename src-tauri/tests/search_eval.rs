//! 线上搜索逻辑的离线复核（docs/10 §9「原型与线上」）：在正式库的**只读副本**上直接调
//! `search_service`，用评测查询集输出各通道的排名，再由 `scripts/search_eval/score.py` 计分。
//!
//! 全部是 `#[ignore]`，需要真实数据和 AI worker，手动运行：
//!
//! ```powershell
//! cd src-tauri
//! cargo test --test search_eval -- --ignored --nocapture --test-threads 1
//! ```
//!
//! 环境变量（都有默认值）：
//! - `PVP_EVAL_DIR`：评测目录，默认 `%LOCALAPPDATA%\PhotoViewPlus\eval`（含私人路径，不进仓库）；
//! - `PVP_EVAL_DB`：被评测的 SQLite，默认 `<评测目录>\rust\app.sqlite`；不存在时从
//!   `<评测目录>\snapshot\app.sqlite`（`build_corpus.py` 做的正式库快照）复制一份，正式库本身不动；
//! - `PVP_EVAL_THUMBS`：缩略图目录，默认正式版 profile 的 `thumbs`（只读）；
//! - `PVP_EVAL_WORKER_PORT`：已在运行的 AI worker 端口；不设时由 `AiSupervisor` 拉起
//!   `PVP_AI_WORKER_DIR`（默认仓库里的 `ai-worker`）；
//! - `PVP_EVAL_MEMORA_DB`：memora 数据库（准备阶段导入描述），默认 memora 的本机位置；
//! - `PVP_EVAL_SYSTEMS`：只跑这些方案（逗号分隔，如 `R3,R5`），默认全部；
//! - `PVP_EVAL_TEXT_MODEL`：描述文本向量模型，默认 bge-small-zh-v1.5；换成别的模型时，
//!   用到描述向量的方案名加 `@模型` 后缀（如 `R4@bge-base-zh-v1.5`），便于对比。
//!
//! 输出：`<评测目录>\runs\R*.json`（查询 id → 语料 key 列表，key 是忽略大小写的完整路径），
//! 以及每路查询耗时。之后运行 `make_sheets.py`（只为没标过的图出拼图）和 `score.py`。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use photo_view_plus_lib::config::{DEFAULT_CLIP_MODEL, DEFAULT_TEXT_MODEL};
use photo_view_plus_lib::db::{self, Pool};
use photo_view_plus_lib::repo::ai_jobs_repo;
use photo_view_plus_lib::repo::analysis_repo;
use photo_view_plus_lib::repo::images_repo;
use photo_view_plus_lib::repo::lancedb_repo::LanceDbRepo;
use photo_view_plus_lib::services::ai_client::{AiHttpClient, ClipEmbedItem};
use photo_view_plus_lib::services::ai_pipeline;
use photo_view_plus_lib::services::ai_supervisor::AiSupervisor;
use photo_view_plus_lib::services::memora_import_service;
use photo_view_plus_lib::services::search_index_service;
use photo_view_plus_lib::services::search_service::{
    self, AiSearchArgs, Channel, QueryEncoder, SearchOptions,
};
use photo_view_plus_lib::services::thumbnail_service;
use photo_view_plus_lib::{AppError, AppResult};

const EMBED_BATCH: i64 = 64;
const RUN_DEPTH: usize = 100;

struct EvalEnv {
    eval_dir: PathBuf,
    pool: Pool,
    vectors: LanceDbRepo,
    thumbs_dir: PathBuf,
}

fn local_app_data() -> PathBuf {
    PathBuf::from(std::env::var("LOCALAPPDATA").expect("LOCALAPPDATA"))
}

fn eval_env() -> EvalEnv {
    let eval_dir = std::env::var("PVP_EVAL_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| local_app_data().join("PhotoViewPlus").join("eval"));
    let db_path = std::env::var("PVP_EVAL_DB")
        .map(PathBuf::from)
        .unwrap_or_else(|_| eval_dir.join("rust").join("app.sqlite"));
    if !db_path.exists() {
        let snapshot = eval_dir.join("snapshot").join("app.sqlite");
        assert!(
            snapshot.exists(),
            "缺少 {}：先运行 scripts/search_eval/build_corpus.py",
            snapshot.display()
        );
        std::fs::create_dir_all(db_path.parent().expect("parent")).expect("mkdir");
        std::fs::copy(&snapshot, &db_path).expect("copy snapshot");
        println!("copied {} -> {}", snapshot.display(), db_path.display());
    }
    let thumbs_dir = std::env::var("PVP_EVAL_THUMBS")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            local_app_data()
                .join("com.vetoer.photoviewplus")
                .join("thumbs")
        });
    let pool = db::open(&db_path).expect("open eval db (runs migrations)");
    let vectors = LanceDbRepo::new(db_path.parent().expect("parent").join("vectors"));
    EvalEnv {
        eval_dir,
        pool,
        vectors,
        thumbs_dir,
    }
}

/// AI worker：优先用已在运行的端口，否则用 AiSupervisor 拉起仓库里的 worker。
struct Worker {
    client: AiHttpClient,
    port: u16,
    _supervisor: Option<Arc<AiSupervisor>>,
}

async fn worker() -> Worker {
    let client = AiHttpClient::new().expect("http client");
    if let Ok(port) = std::env::var("PVP_EVAL_WORKER_PORT") {
        let port = port.parse().expect("PVP_EVAL_WORKER_PORT");
        return Worker {
            client,
            port,
            _supervisor: None,
        };
    }
    let worker_dir = std::env::var("PVP_AI_WORKER_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .expect("repo root")
                .join("ai-worker")
        });
    let models_dir = std::env::var("PVP_MODEL_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| local_app_data().join("PhotoViewPlus").join("models"));
    let supervisor = Arc::new(AiSupervisor::new(worker_dir, models_dir).expect("supervisor"));
    let status = supervisor.start().await.expect("start AI worker");
    Worker {
        client,
        port: status.port.expect("worker port"),
        _supervisor: Some(supervisor),
    }
}

struct EvalEncoder<'a> {
    worker: &'a Worker,
    clip_model: String,
    text_model: String,
}

fn eval_text_model() -> String {
    std::env::var("PVP_EVAL_TEXT_MODEL").unwrap_or_else(|_| DEFAULT_TEXT_MODEL.to_string())
}

#[async_trait]
impl QueryEncoder for EvalEncoder<'_> {
    fn clip_model(&self) -> &str {
        &self.clip_model
    }

    async fn encode_text(&self, text: &str) -> AppResult<Option<Vec<f32>>> {
        let response = self
            .worker
            .client
            .encode_text(
                self.worker.port,
                text.to_string(),
                Some(self.clip_model.clone()),
            )
            .await?;
        if response.fallback {
            return Err(AppError::Other(format!(
                "worker 处于兜底模式（{}），评测无意义",
                response.model
            )));
        }
        Ok(Some(response.embedding))
    }

    async fn encode_image(
        &self,
        _image_id: i64,
        _thumb_path: String,
    ) -> AppResult<Option<Vec<f32>>> {
        Ok(None)
    }

    fn text_model(&self) -> Option<&str> {
        Some(&self.text_model)
    }

    async fn encode_query(&self, text: &str) -> AppResult<Option<Vec<f32>>> {
        let response = self
            .worker
            .client
            .embed_texts(
                self.worker.port,
                vec![text.to_string()],
                Some(self.text_model.clone()),
                true,
            )
            .await?;
        if response.fallback {
            return Err(AppError::Other(format!(
                "worker 文本向量处于兜底模式（{}），评测无意义",
                response.model
            )));
        }
        Ok(response.embeddings.into_iter().next())
    }
}

#[derive(serde::Deserialize)]
struct QueryFile {
    queries: Vec<EvalQuery>,
}

#[derive(serde::Deserialize)]
struct EvalQuery {
    id: String,
    text: String,
}

fn load_queries() -> Vec<EvalQuery> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("repo root")
        .join("tests")
        .join("search_eval")
        .join("queries.json");
    let raw = std::fs::read_to_string(path).expect("queries.json");
    serde_json::from_str::<QueryFile>(&raw)
        .expect("parse queries")
        .queries
}

/// image id → 语料 key（与 build_corpus.py 一致：root + "\" + rel，反斜杠，转小写）。
fn corpus_keys(pool: &Pool) -> HashMap<i64, String> {
    let conn = pool.get().expect("conn");
    let mut stmt = conn
        .prepare(
            "SELECT i.id, r.path, i.rel_path FROM images i JOIN roots r ON r.id = i.root_id
             WHERE i.deleted_at IS NULL",
        )
        .expect("prepare");
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .expect("query");
    rows.map(|row| {
        let (id, root, rel) = row.expect("row");
        let key = format!(
            "{}\\{}",
            root.trim_end_matches('\\'),
            rel.replace('/', "\\")
        )
        .to_lowercase();
        (id, key)
    })
    .collect()
}

fn percentile(sorted_ms: &[f64], p: f64) -> f64 {
    if sorted_ms.is_empty() {
        return 0.0;
    }
    let index = ((sorted_ms.len() as f64 - 1.0) * p).round() as usize;
    sorted_ms[index.min(sorted_ms.len() - 1)]
}

fn write_run(eval_dir: &Path, name: &str, runs: &HashMap<String, Vec<String>>) {
    let dir = eval_dir.join("runs");
    std::fs::create_dir_all(&dir).expect("runs dir");
    let path = dir.join(format!("{name}.json"));
    std::fs::write(&path, serde_json::to_string(runs).expect("json")).expect("write run");
    println!("wrote {}", path.display());
}

/// 一个方案：每个查询跑一次 `search_service::search_with`，结果映射成语料 key（按 key 去重）。
async fn run_system(
    env: &EvalEnv,
    encoder: &dyn QueryEncoder,
    keys: &HashMap<i64, String>,
    queries: &[EvalQuery],
    name: &str,
    options: &SearchOptions,
) {
    let mut runs = HashMap::new();
    let mut scores: HashMap<String, Vec<(String, f32)>> = HashMap::new();
    let mut latencies = Vec::new();
    let mut channel_latencies: HashMap<String, Vec<f64>> = HashMap::new();
    for query in queries {
        let started = Instant::now();
        let response = search_service::search_with(
            &env.pool,
            &env.vectors,
            encoder,
            &AiSearchArgs {
                text: query.text.clone(),
                root_ids: None,
                limit: Some(RUN_DEPTH),
            },
            options,
        )
        .await
        .expect("search");
        latencies.push(started.elapsed().as_secs_f64() * 1000.0);
        assert!(
            !response.degraded,
            "评测时语义通道必须可用：{:?}",
            response.degraded_reason
        );
        for stat in &response.channels {
            channel_latencies
                .entry(format!("{:?}", stat.channel))
                .or_default()
                .push(stat.elapsed_ms);
        }
        let results = response.results;
        let mut seen = HashSet::new();
        let ranked = results
            .iter()
            .filter_map(|result| keys.get(&result.image.id).map(|key| (key, result.score)))
            .filter(|(key, _)| seen.insert((*key).clone()))
            .map(|(key, score)| (key.clone(), score))
            .collect::<Vec<_>>();
        scores.insert(query.id.clone(), ranked.clone());
        runs.insert(
            query.id.clone(),
            ranked.into_iter().map(|(key, _)| key).collect::<Vec<_>>(),
        );
    }
    latencies.sort_by(f64::total_cmp);
    println!(
        "{name}: {} queries, latency p50 {:.1}ms p95 {:.1}ms max {:.1}ms",
        queries.len(),
        percentile(&latencies, 0.5),
        percentile(&latencies, 0.95),
        latencies.last().copied().unwrap_or(0.0)
    );
    let mut channels = channel_latencies.into_iter().collect::<Vec<_>>();
    channels.sort_by(|a, b| a.0.cmp(&b.0));
    for (channel, mut values) in channels {
        values.sort_by(f64::total_cmp);
        println!(
            "  {channel}: p50 {:.1}ms p95 {:.1}ms",
            percentile(&values, 0.5),
            percentile(&values, 0.95)
        );
    }
    write_run(&env.eval_dir, name, &runs);
    // 带分数的版本放在 rust/ 下（score.py 只读 runs/），用来分析相似度下限、融合权重。
    let path = env
        .eval_dir
        .join("rust")
        .join(format!("scores_{name}.json"));
    std::fs::write(&path, serde_json::to_string(&scores).expect("json")).expect("write scores");
}

/// 准备评测库：迁移到最新 schema；导入 memora 描述；建好检索全文索引；用 worker 把缩略图
/// 已就绪的图全部算好当前图像向量模型的向量。
#[tokio::test]
#[ignore]
async fn eval_prepare() {
    let env = eval_env();
    {
        let mut conn = env.pool.get().expect("conn");
        let memora = std::env::var("PVP_EVAL_MEMORA_DB")
            .unwrap_or_else(|_| memora_import_service::DEFAULT_MEMORA_DB.to_string());
        let outcome = memora_import_service::import_memora(&mut conn, Path::new(&memora), 1)
            .expect("import memora");
        println!("memora import: {outcome:?}");
        let started = Instant::now();
        let outcome = search_index_service::rebuild_all(&mut conn).expect("search index");
        println!(
            "search index rebuilt: {} docs in {:.2}s",
            outcome.indexed,
            started.elapsed().as_secs_f64()
        );
    }
    let worker = worker().await;
    let model = DEFAULT_CLIP_MODEL;
    let started = Instant::now();
    let mut done = 0usize;
    let mut failed = 0usize;
    loop {
        let ids = {
            let conn = env.pool.get().expect("conn");
            ai_jobs_repo::pending_image_embedding(&conn, model, EMBED_BATCH).expect("pending")
        };
        if ids.is_empty() {
            break;
        }
        let items = {
            let conn = env.pool.get().expect("conn");
            images_repo::ready_thumb_hashes(&conn, &ids).expect("thumbs")
        }
        .into_iter()
        .map(|(id, hash)| ClipEmbedItem {
            id,
            thumb_path: thumbnail_service::thumb_path(&env.thumbs_dir, &hash)
                .to_string_lossy()
                .to_string(),
        })
        .collect::<Vec<_>>();
        let response = worker
            .client
            .embed_images(worker.port, items, Some(model.to_string()))
            .await
            .expect("embed");
        assert!(!response.fallback, "worker 处于兜底模式");
        let outcome = ai_pipeline::apply_embed_response(&env.pool, &env.vectors, response, 1)
            .await
            .expect("apply");
        done += outcome.stored;
        failed += outcome.failed;
        if (done + failed) % 1024 < EMBED_BATCH as usize {
            println!(
                "  image embedding: {done} ok, {failed} failed, {:.0}s",
                started.elapsed().as_secs_f64()
            );
        }
    }
    let secs = started.elapsed().as_secs_f64();
    println!(
        "image embedding ({model}) done: {done} ok, {failed} failed in {secs:.1}s ({:.1} img/s)",
        done as f64 / secs.max(0.001)
    );

    // 描述文本向量
    let text_model = eval_text_model();
    let started = Instant::now();
    let mut done = 0usize;
    loop {
        let ids = {
            let conn = env.pool.get().expect("conn");
            ai_jobs_repo::pending_text_embedding(&conn, &text_model, 256).expect("pending")
        };
        if ids.is_empty() {
            break;
        }
        let items = {
            let conn = env.pool.get().expect("conn");
            analysis_repo::texts_for(&conn, &ids).expect("texts")
        }
        .into_iter()
        .map(|(id, texts)| (id, texts.description))
        .collect::<Vec<_>>();
        let response = worker
            .client
            .embed_texts(
                worker.port,
                items.iter().map(|(_, text)| text.clone()).collect(),
                Some(text_model.clone()),
                false,
            )
            .await
            .expect("embed texts");
        assert!(!response.fallback, "worker 文本向量处于兜底模式");
        let outcome =
            ai_pipeline::apply_text_embed_response(&env.pool, &env.vectors, &items, response, 1)
                .await
                .expect("apply");
        done += outcome.stored;
    }
    let secs = started.elapsed().as_secs_f64();
    println!(
        "text embedding ({text_model}) done: {done} in {secs:.1}s ({:.0} texts/s)",
        done as f64 / secs.max(0.001)
    );
}

/// 各方案的排名（方案定义见 scripts/search_eval/score.py 的 NAMES）：
/// - R1：只跑图像向量通道（Chinese-CLIP）；
/// - R2：标签 + 全文（RAM 标签、OCR、文件名）；
/// - R5b：图像向量 + 标签 + 全文（不用大模型描述，对应原型 S5b）。
#[tokio::test]
#[ignore]
async fn eval_run() {
    let env = eval_env();
    {
        let mut conn = env.pool.get().expect("conn");
        search_index_service::flush_all(&mut conn).expect("search index");
    }
    let worker = worker().await;
    let queries = load_queries();
    let keys = corpus_keys(&env.pool);
    let text_model = eval_text_model();
    let encoder = EvalEncoder {
        worker: &worker,
        clip_model: DEFAULT_CLIP_MODEL.to_string(),
        text_model: text_model.clone(),
    };
    let wanted = std::env::var("PVP_EVAL_SYSTEMS").ok().map(|raw| {
        raw.split(',')
            .map(|name| name.trim().to_string())
            .collect::<Vec<_>>()
    });
    for (name, options) in systems() {
        if wanted
            .as_ref()
            .is_some_and(|names| !names.iter().any(|wanted| wanted == name))
        {
            continue;
        }
        // 换了描述向量模型时，用到这一路的方案名加后缀，不覆盖默认模型的结果。
        let name = if text_model != DEFAULT_TEXT_MODEL
            && options.channels.contains(&Channel::TextVector)
        {
            format!("{name}@{text_model}")
        } else {
            name.to_string()
        };
        run_system(&env, &encoder, &keys, &queries, &name, &options).await;
    }
}

/// 评测方案（名称与 scripts/search_eval/score.py 的 NAMES 对应）。
fn systems() -> Vec<(&'static str, SearchOptions)> {
    // 不含描述的全文列（与原型 S2 对比；单字列里仍有描述的字，这是唯一的偏差）。
    let without_description = ["tags", "keywords", "visible_text", "ocr_text", "filename"]
        .map(String::from)
        .to_vec();
    vec![
        // 只跑图像向量通道（Chinese-CLIP）
        ("R1", SearchOptions::only(&[Channel::ImageVector])),
        // 标签 + 全文，不用描述（RAM 标签、OCR、文件名）
        (
            "R2",
            SearchOptions {
                fulltext_columns: Some(without_description.clone()),
                ..SearchOptions::only(&[Channel::Tag, Channel::FullText])
            },
        ),
        // 只查描述列的全文（对应原型 S3）
        (
            "R3",
            SearchOptions {
                fulltext_columns: Some(vec!["description".to_string()]),
                ..SearchOptions::only(&[Channel::FullText])
            },
        ),
        // 只跑描述文本向量（对应原型 S4）
        ("R4", SearchOptions::only(&[Channel::TextVector])),
        // 图像向量 + 标签 + 全文，不用描述（对应原型 S5b）
        (
            "R5b",
            SearchOptions {
                fulltext_columns: Some(without_description),
                ..SearchOptions::only(&[Channel::ImageVector, Channel::Tag, Channel::FullText])
            },
        ),
        // 线上默认：全部通道
        ("R5", SearchOptions::default()),
    ]
}
