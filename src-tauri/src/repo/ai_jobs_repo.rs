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

/// 记一条结果（upsert）。同一模型、同一输入再记一次 `attempts + 1`；换了模型或输入指纹变了，
/// 从 1 重新计。
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
             attempts = CASE
                 WHEN ai_jobs.model = excluded.model AND ai_jobs.input_hash IS excluded.input_hash
                 THEN ai_jobs.attempts + 1
                 ELSE 1
             END,
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

/// 描述的文本向量：有非空描述、当前模型下还没有算过（或描述变了、失败可重试）的图，最多 `limit` 张。
///
/// 描述变没变用 SQL 函数 `text_hash()`（见 `db::register_functions`）和 `ai_jobs.input_hash` 比对。
pub fn pending_text_embedding(conn: &Connection, model: &str, limit: i64) -> AppResult<Vec<i64>> {
    let sql = format!(
        "SELECT a.image_id FROM image_analysis a
         JOIN images i ON i.id = a.image_id
         LEFT JOIN ai_jobs j ON j.image_id = a.image_id AND j.kind = '{KIND_TEXT_EMBEDDING}'
         WHERE i.deleted_at IS NULL AND TRIM(a.description) <> ''
           AND (j.image_id IS NULL
                OR j.model <> ?1
                OR j.input_hash IS NOT text_hash(a.description)
                OR (j.status = 'failed' AND j.attempts < ?2))
         ORDER BY a.image_id
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

pub fn count_pending_text_embedding(conn: &Connection, model: &str) -> AppResult<i64> {
    let sql = format!(
        "SELECT COUNT(*) FROM image_analysis a
         JOIN images i ON i.id = a.image_id
         LEFT JOIN ai_jobs j ON j.image_id = a.image_id AND j.kind = '{KIND_TEXT_EMBEDDING}'
         WHERE i.deleted_at IS NULL AND TRIM(a.description) <> ''
           AND (j.image_id IS NULL
                OR j.model <> ?1
                OR j.input_hash IS NOT text_hash(a.description)
                OR (j.status = 'failed' AND j.attempts < ?2))"
    );
    Ok(conn
        .prepare_cached(&sql)?
        .query_row(params![model, MAX_ATTEMPTS], |row| row.get(0))?)
}

/// 当前模型下各状态的数量（只统计未删除的图）。
/// VLM 分析的候选图（docs/10 A6）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VlmCandidate {
    pub image_id: i64,
    pub root_path: String,
    pub rel_path: String,
    pub taken_at: Option<i64>,
    pub has_analysis: bool,
}

/// VLM 待处理条件：没有 vlm_analysis 任务行；或失败了且（换了模型 / 提示词版本，或次数未用完）。
/// 已就绪的行不论版本都不算待处理：升级提示词后是否重跑由用户手动触发（`requeue_outdated_vlm`）。
const PENDING_VLM_WHERE: &str = "
    i.deleted_at IS NULL
    AND (j.image_id IS NULL OR (j.status = 'failed' AND (j.model <> ?1 OR j.attempts < ?2)))";

/// 目录优先级（同 memora）：相机目录最先，微信等聊天软件目录最后，中途停下时最有价值的照片已经分析完。
/// `p` 是小写、反斜杠分隔的相对路径，前面补了一个 `\`，目录名可以按 `\名字\` 整段匹配。
const VLM_DIR_PRIORITY: &str = r"
    CASE
        WHEN p LIKE '%\camera\%' OR p LIKE '%\dcim\%' OR p LIKE '%\camera remote\%'
             OR p LIKE '%\dji album\%' OR p LIKE '%\realme share\%' OR p LIKE '%互传%' THEN 0
        WHEN (p LIKE '%weixin%' OR p LIKE '%wechat%') AND lower(i.filename) LIKE 'wx_camera%' THEN 1
        WHEN p LIKE '%weixin%' OR p LIKE '%wechat%' OR p LIKE '%\tencent\%' OR p LIKE '%\qq\%'
             OR p LIKE '%\qq_images\%' THEN 3
        ELSE 2
    END";

/// 取一批待分析的图：还没有任何描述的先做（memora 导入过的排后面，之后再用 VLM 覆盖），
/// 再按目录优先级，同优先级里新照片在前；缩略图就绪的（说明原图读得到）排在同路径的重复行前面。
pub fn pending_vlm_analysis(
    conn: &Connection,
    model: &str,
    limit: i64,
) -> AppResult<Vec<VlmCandidate>> {
    let sql = format!(
        r"SELECT i.id, r.path, i.rel_path, i.taken_at, a.image_id IS NOT NULL
         FROM (SELECT i.*, '\' || lower(replace(i.rel_path, '/', '\')) AS p FROM images i) i
         JOIN roots r ON r.id = i.root_id
         LEFT JOIN ai_jobs j ON j.image_id = i.id AND j.kind = '{KIND_VLM_ANALYSIS}'
         LEFT JOIN image_analysis a ON a.image_id = i.id
         WHERE {PENDING_VLM_WHERE}
         ORDER BY a.image_id IS NOT NULL, {VLM_DIR_PRIORITY},
                  COALESCE(i.taken_at, i.mtime) DESC,
                  i.thumb_status <> 'ready', i.id
         LIMIT ?3"
    );
    let mut stmt = conn.prepare_cached(&sql)?;
    let rows = stmt.query_map(params![model, MAX_ATTEMPTS, limit], |row| {
        Ok(VlmCandidate {
            image_id: row.get(0)?,
            root_path: row.get(1)?,
            rel_path: row.get(2)?,
            taken_at: row.get(3)?,
            has_analysis: row.get(4)?,
        })
    })?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

pub fn count_pending_vlm_analysis(conn: &Connection, model: &str) -> AppResult<i64> {
    let sql = format!(
        "SELECT COUNT(*) FROM images i
         LEFT JOIN ai_jobs j ON j.image_id = i.id AND j.kind = '{KIND_VLM_ANALYSIS}'
         WHERE {PENDING_VLM_WHERE}"
    );
    Ok(conn
        .prepare_cached(&sql)?
        .query_row(params![model, MAX_ATTEMPTS], |row| row.get(0))?)
}

/// 提示词 / 模型升级后，让旧版本的结果重新排队（用户手动触发）。返回排队的张数。
pub fn requeue_outdated_vlm(conn: &Connection, model: &str) -> AppResult<usize> {
    Ok(conn.execute(
        &format!("DELETE FROM ai_jobs WHERE kind = '{KIND_VLM_ANALYSIS}' AND model <> ?1"),
        [model],
    )?)
}

/// VLM 覆盖率：（已有任一版本就绪结果的未删除图片数, 未删除图片总数）。
pub fn vlm_coverage(conn: &Connection) -> AppResult<(i64, i64)> {
    Ok(conn.query_row(
        &format!(
            "SELECT
                 (SELECT COUNT(*) FROM images i JOIN ai_jobs j ON j.image_id = i.id
                  WHERE i.deleted_at IS NULL AND j.kind = '{KIND_VLM_ANALYSIS}' AND j.status = 'ready'),
                 (SELECT COUNT(*) FROM images WHERE deleted_at IS NULL)"
        ),
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?)
}

/// 清掉 VLM 的失败记录，让它们重新排队（例如 NAS 修好之后）。返回张数。
pub fn retry_failed_vlm(conn: &Connection) -> AppResult<usize> {
    Ok(conn.execute(
        &format!("DELETE FROM ai_jobs WHERE kind = '{KIND_VLM_ANALYSIS}' AND status = 'failed'"),
        [],
    )?)
}

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
