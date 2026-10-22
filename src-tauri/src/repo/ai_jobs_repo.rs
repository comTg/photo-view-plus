//! `ai_jobs`：新 AI 任务（图像向量 / 文本向量 / VLM 分析）的状态，一张图每类任务一行（docs/10 §7）。
//!
//! **待处理**不预先插行，而是现算：没有"当前模型"的行；或者当前模型的行是 `failed` 且
//! `attempts < MAX_ATTEMPTS`（失败的会重试，最多 3 次）。换模型后旧行的 `model` 对不上，
//! 全库自然重新进入待处理——这就是"换模型自动全量重算"。

use rusqlite::{params, Connection};
use serde::Serialize;

use crate::error::AppResult;

pub const KIND_IMAGE_EMBEDDING: &str = "image_embedding";
pub const KIND_TEXT_EMBEDDING: &str = "text_embedding";
pub const KIND_VLM_ANALYSIS: &str = "vlm_analysis";

pub const STATUS_READY: &str = "ready";
pub const STATUS_FAILED: &str = "failed";
pub const STATUS_SKIPPED: &str = "skipped";

/// 同一模型最多尝试几次；超过后不再自动重试（换模型、文件内容变化会清零）。
pub const MAX_ATTEMPTS: i64 = 3;

/// 一条任务结果。
#[derive(Debug, Clone)]
pub struct JobResult<'a> {
    pub image_id: i64,
    pub status: &'a str,
    pub error: Option<&'a str>,
    pub input_hash: Option<&'a str>,
}

/// 某类任务在当前模型下的统计。
#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct JobCounts {
    pub ready: i64,
    pub failed: i64,
    pub skipped: i64,
}

/// 记一条结果（upsert）。同一模型再记一次 `attempts + 1`；换了模型从 1 重新计。
pub fn record(
    conn: &Connection,
    kind: &str,
    model: &str,
    result: &JobResult<'_>,
    now: i64,
) -> AppResult<()> {
    // INSERT … SELECT … WHERE EXISTS：图在任务进行中被整目录移除（images 行级联删除）时静默跳过，
    // 而不是撞外键约束让整批结果写不进去。
    conn.prepare_cached(
        "INSERT INTO ai_jobs(image_id, kind, model, status, attempts, last_error, input_hash, updated_at)
         SELECT ?1, ?2, ?3, ?4, 1, ?5, ?6, ?7
         WHERE EXISTS (SELECT 1 FROM images WHERE id = ?1)
         ON CONFLICT(image_id, kind) DO UPDATE SET
             attempts = CASE WHEN ai_jobs.model = excluded.model THEN ai_jobs.attempts + 1 ELSE 1 END,
             model = excluded.model,
             status = excluded.status,
             last_error = excluded.last_error,
             input_hash = excluded.input_hash,
             updated_at = excluded.updated_at",
    )?
    .execute(params![
        result.image_id,
        kind,
        model,
        result.status,
        result.error,
        result.input_hash,
        now
    ])?;
    Ok(())
}

/// 文件内容变了：这张图的所有 AI 任务都要重算（旧的分析结果另行保留，见 docs/10 §7）。
pub fn delete_for_image(conn: &Connection, image_id: i64) -> AppResult<()> {
    conn.prepare_cached("DELETE FROM ai_jobs WHERE image_id = ?1")?
        .execute([image_id])?;
    Ok(())
}

/// 某类任务的某张图是否需要处理的 SQL 条件（`j` 是 LEFT JOIN 进来的 ai_jobs 行，`?model` 是参数）。
const PENDING_CONDITION: &str = "(j.image_id IS NULL
      OR j.model <> ?1
      OR (j.status = 'failed' AND j.attempts < ?2))";

/// 缩略图已就绪、当前模型下还没有图像向量的图（按 id 升序），最多 `limit` 张。
pub fn pending_image_embedding(conn: &Connection, model: &str, limit: i64) -> AppResult<Vec<i64>> {
    let sql = format!(
        "SELECT i.id FROM images i
         LEFT JOIN ai_jobs j ON j.image_id = i.id AND j.kind = '{KIND_IMAGE_EMBEDDING}'
         WHERE i.deleted_at IS NULL AND i.thumb_status = 'ready' AND {PENDING_CONDITION}
         ORDER BY i.id
         LIMIT ?3"
    );
    let mut stmt = conn.prepare_cached(&sql)?;
    let rows = stmt.query_map(params![model, MAX_ATTEMPTS, limit.max(1)], |row| row.get(0))?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

pub fn count_pending_image_embedding(conn: &Connection, model: &str) -> AppResult<i64> {
    let sql = format!(
        "SELECT COUNT(*) FROM images i
         LEFT JOIN ai_jobs j ON j.image_id = i.id AND j.kind = '{KIND_IMAGE_EMBEDDING}'
         WHERE i.deleted_at IS NULL AND i.thumb_status = 'ready' AND {PENDING_CONDITION}"
    );
    Ok(conn
        .prepare_cached(&sql)?
        .query_row(params![model, MAX_ATTEMPTS], |row| row.get(0))?)
}

/// 当前模型下各状态的数量（只统计未删除的图）。
pub fn counts(conn: &Connection, kind: &str, model: &str) -> AppResult<JobCounts> {
    let mut stmt = conn.prepare_cached(
        "SELECT j.status, COUNT(*) FROM ai_jobs j
         JOIN images i ON i.id = j.image_id
         WHERE j.kind = ?1 AND j.model = ?2 AND i.deleted_at IS NULL
         GROUP BY j.status",
    )?;
    let rows = stmt.query_map(params![kind, model], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
    })?;
    let mut counts = JobCounts::default();
    for row in rows {
        let (status, count) = row?;
        match status.as_str() {
            STATUS_READY => counts.ready = count,
            STATUS_FAILED => counts.failed = count,
            STATUS_SKIPPED => counts.skipped = count,
            _ => {}
        }
    }
    Ok(counts)
}
