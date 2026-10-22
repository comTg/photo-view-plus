use std::path::{Path, PathBuf};

use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::functions::FunctionFlags;
use rusqlite::Connection;

use crate::error::AppResult;
use crate::migrations;

pub type Pool = r2d2::Pool<SqliteConnectionManager>;
pub type PooledConn = r2d2::PooledConnection<SqliteConnectionManager>;

/// 打开 / 初始化数据库：
/// 1. 创建父目录（若不存在）
/// 2. 启 WAL + foreign_keys（每个连接通过 with_init 注入）
/// 3. 跑 migration 升级到最新版本
pub fn open(db_path: &Path) -> AppResult<Pool> {
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let manager = SqliteConnectionManager::file(db_path).with_init(|c| {
        // PRAGMA 在每个新连接打开后立即执行
        c.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA foreign_keys = ON;
             PRAGMA busy_timeout = 5000;
             PRAGMA synchronous = NORMAL;",
        )?;
        register_functions(c)
    });

    // 16 个连接对应文档里 8-16 并发上限的上端
    let pool = r2d2::Pool::builder().max_size(16).build(manager)?;

    // 用同一池获取首个连接跑 migration，确保 schema_version 表已建
    let mut conn = pool.get()?;
    migrations::run(&mut conn)?;

    Ok(pool)
}

/// 文本指纹：BLAKE3 的前 16 个十六进制字符。用作 `ai_jobs.input_hash`（例如描述文本变了，
/// 它的文本向量就要重算，docs/10 §7）。与 SQL 函数 `text_hash()` 结果一致。
pub fn text_hash(text: &str) -> String {
    blake3::hash(text.as_bytes()).to_hex()[..16].to_string()
}

/// 描述文本向量的输入：描述，加上大模型给的关键词（docs/10 A6）。search-v1 的描述写得中性
/// （"风景照片。画面主体为日落时分的湖面……"），"夕阳""汽车""二维码"这类查询词常常只出现在
/// 关键词里。没有关键词（memora 导入的行）时就是描述本身，指纹不变、不用重算。
pub fn embedding_text(description: &str, keywords: &[String]) -> String {
    if keywords.is_empty() {
        description.to_string()
    } else {
        format!("{description}\n关键词：{}", keywords.join("、"))
    }
}

/// 给连接注册自定义 SQL 函数：
/// - `text_hash(text)`：同 [`text_hash`]，NULL 进 NULL 出。待处理判断（"描述的指纹和上次算向量时
///   是否一致"）因此能直接在 SQL 里算，不必把全部描述读进内存比对。
/// - `embedding_hash(description, keywords_json)`：[`embedding_text`] 的指纹，即描述文本向量
///   记在 `ai_jobs.input_hash` 里的值。
pub fn register_functions(conn: &Connection) -> rusqlite::Result<()> {
    conn.create_scalar_function(
        "text_hash",
        1,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| Ok(ctx.get::<Option<String>>(0)?.map(|text| text_hash(&text))),
    )?;
    conn.create_scalar_function(
        "embedding_hash",
        2,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| {
            let Some(description) = ctx.get::<Option<String>>(0)? else {
                return Ok(None);
            };
            let keywords = ctx
                .get::<Option<String>>(1)?
                .and_then(|raw| serde_json::from_str::<Vec<String>>(&raw).ok())
                .unwrap_or_default();
            Ok(Some(text_hash(&embedding_text(&description, &keywords))))
        },
    )
}

/// 默认数据库路径（开发期可在 Tauri 路径 API 不可用时调用）。
/// 优先：`PVP_DB_DIR` 环境变量 → fallback 到 `.app-data/<profile>/app.sqlite`。
pub fn default_db_path_for(profile: &str) -> PathBuf {
    if let Ok(dir) = std::env::var("PVP_DB_DIR") {
        return PathBuf::from(dir).join("app.sqlite");
    }
    PathBuf::from(".app-data").join(profile).join("app.sqlite")
}

/// 工具：检查表是否存在（测试用）
pub fn table_exists(conn: &Connection, name: &str) -> AppResult<bool> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
        [name],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}
