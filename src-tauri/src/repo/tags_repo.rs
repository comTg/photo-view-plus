use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::error::AppResult;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Tag {
    pub id: i64,
    pub name: String,
    pub source: String,
    pub category: Option<String>,
    pub created_at: i64,
    pub image_count: i64,
    /// 标签云分组（docs/10 A7）：带这个标签的图里最常见的 VLM 主类别；没有 VLM 结果时为空。
    pub group: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageTag {
    pub image_id: i64,
    pub tag_id: i64,
    pub name: String,
    pub score: f64,
    pub source: String,
    pub category: Option<String>,
}

#[derive(Debug, Clone)]
pub struct NewTagScore {
    pub name: String,
    pub score: f64,
    pub source: String,
    pub category: Option<String>,
}

/// 取（不存在就建）同名标签的 id。
///
/// `tags.source` 只记最先创建它的来源，不再被后来者覆盖：同一个标签名可能同时来自 RAM（ai）、
/// memora、VLM、用户，谁打的以 `image_tags.source` 为准（docs/10 §7）。
pub fn upsert_tag(
    conn: &Connection,
    name: &str,
    source: &str,
    category: Option<&str>,
    now: i64,
) -> AppResult<i64> {
    conn.prepare_cached(
        "INSERT INTO tags(name, source, category, created_at)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(name) DO UPDATE SET
             category = COALESCE(excluded.category, tags.category)",
    )?
    .execute(params![name, source, category, now])?;

    let id = conn
        .prepare_cached("SELECT id FROM tags WHERE name = ?1")?
        .query_row([name], |row| row.get(0))?;
    Ok(id)
}

pub fn replace_image_tags(
    conn: &mut Connection,
    image_id: i64,
    tags: &[NewTagScore],
    now: i64,
) -> AppResult<Vec<ImageTag>> {
    let tx = conn.transaction()?;
    tx.execute(
        "DELETE FROM image_tags WHERE image_id = ?1 AND source = 'ai'",
        [image_id],
    )?;

    let mut written = Vec::new();
    for tag in tags {
        let name = tag.name.trim();
        if name.is_empty() {
            continue;
        }
        let tag_id = upsert_tag(&tx, name, &tag.source, tag.category.as_deref(), now)?;
        tx.execute(
            "INSERT OR REPLACE INTO image_tags(image_id, tag_id, score, source, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![image_id, tag_id, tag.score.clamp(0.0, 1.0), tag.source, now],
        )?;
        written.push(ImageTag {
            image_id,
            tag_id,
            name: name.to_string(),
            score: tag.score.clamp(0.0, 1.0),
            source: tag.source.clone(),
            category: tag.category.clone(),
        });
    }

    tx.commit()?;
    Ok(written)
}

/// 用一组新标签替换某张图在 `replace_sources` 这些来源下的全部标签（在调用方的事务里执行）。
/// VLM 结果写入时用它同时清掉 memora 导入的类别标签（docs/10 §7：VLM 覆盖 memora）。
pub fn replace_source_tags(
    conn: &Connection,
    image_id: i64,
    replace_sources: &[&str],
    tags: &[NewTagScore],
    now: i64,
) -> AppResult<()> {
    for source in replace_sources {
        conn.prepare_cached("DELETE FROM image_tags WHERE image_id = ?1 AND source = ?2")?
            .execute(params![image_id, source])?;
    }
    for tag in tags {
        let name = tag.name.trim();
        if name.is_empty() {
            continue;
        }
        let tag_id = upsert_tag(conn, name, &tag.source, tag.category.as_deref(), now)?;
        conn.prepare_cached(
            "INSERT OR REPLACE INTO image_tags(image_id, tag_id, score, source, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )?
        .execute(params![
            image_id,
            tag_id,
            tag.score.clamp(0.0, 1.0),
            tag.source,
            now
        ])?;
    }
    Ok(())
}

/// 清空所有 AI 标签，并把"缩略图就绪、未删除"的图片重置为待打标签，
/// 让后台流水线用当前 tagger（如换成 RAM-plus 中文）重新打一遍。
/// 返回被重置 tag_status 的图片数。user 来源的标签与引用不受影响。
pub fn clear_ai_tags_and_reset(conn: &mut Connection) -> AppResult<usize> {
    let tx = conn.transaction()?;
    tx.execute("DELETE FROM image_tags WHERE source = 'ai'", [])?;
    // 清掉不再被任何图片引用的孤儿标签（多为旧的英文 AI 标签）。
    tx.execute(
        "DELETE FROM tags WHERE id NOT IN (SELECT DISTINCT tag_id FROM image_tags)",
        [],
    )?;
    let reset = tx.execute(
        "UPDATE images SET tag_status = 'pending'
         WHERE deleted_at IS NULL AND thumb_status = 'ready'",
        [],
    )?;
    tx.commit()?;
    Ok(reset)
}

pub fn tags_for_image(conn: &Connection, image_id: i64) -> AppResult<Vec<ImageTag>> {
    let mut stmt = conn.prepare(
        "SELECT it.image_id, it.tag_id, t.name, it.score, it.source, t.category
         FROM image_tags it
         JOIN tags t ON t.id = it.tag_id
         WHERE it.image_id = ?1
         ORDER BY it.score DESC, t.name ASC",
    )?;
    let rows = stmt.query_map([image_id], |row| {
        Ok(ImageTag {
            image_id: row.get(0)?,
            tag_id: row.get(1)?,
            name: row.get(2)?,
            score: row.get(3)?,
            source: row.get(4)?,
            category: row.get(5)?,
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// 按图片数排序的标签，带标签云分组：类别标签（`tags.category = 'category'`）自成一组；
/// 其他标签归到"带它的图里最常见的 VLM 主类别"。
pub fn list_tags(conn: &Connection, limit: i64) -> AppResult<Vec<Tag>> {
    let mut stmt = conn.prepare(
        "WITH counts AS (
             SELECT t.id, t.name, t.source, t.category, t.created_at,
                    COUNT(it.image_id) AS image_count
             FROM tags t
             LEFT JOIN image_tags it ON it.tag_id = t.id
             GROUP BY t.id
             ORDER BY image_count DESC, t.name ASC
             LIMIT ?1
         ),
         tag_groups AS (
             SELECT it.tag_id, json_extract(a.categories, '$[0]') AS grp, COUNT(*) AS n
             FROM image_tags it
             JOIN image_analysis a ON a.image_id = it.image_id
             WHERE it.tag_id IN (SELECT id FROM counts) AND a.source = 'vlm'
             GROUP BY it.tag_id, grp
         ),
         best AS (
             SELECT tag_id, grp FROM (
                 SELECT tag_id, grp,
                        ROW_NUMBER() OVER (PARTITION BY tag_id ORDER BY n DESC, grp) AS rank
                 FROM tag_groups WHERE grp IS NOT NULL
             ) WHERE rank = 1
         )
         SELECT c.id, c.name, c.source, c.category, c.created_at, c.image_count,
                CASE WHEN c.category = 'category' THEN c.name ELSE best.grp END
         FROM counts c
         LEFT JOIN best ON best.tag_id = c.id
         ORDER BY c.image_count DESC, c.name ASC",
    )?;
    let rows = stmt.query_map([limit.clamp(1, 500)], |row| {
        Ok(Tag {
            id: row.get(0)?,
            name: row.get(1)?,
            source: row.get(2)?,
            category: row.get(3)?,
            created_at: row.get(4)?,
            image_count: row.get(5)?,
            group: row.get(6)?,
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// RAM-plus 旧标签（`source = 'ai'`）的规模：（有这类标签的图片数, 不同标签数）。
pub fn ram_tag_counts(conn: &Connection) -> AppResult<(i64, i64)> {
    Ok(conn.query_row(
        "SELECT COUNT(DISTINCT image_id), COUNT(DISTINCT tag_id) FROM image_tags WHERE source = 'ai'",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?)
}

pub fn get_tag_by_name(conn: &Connection, name: &str) -> AppResult<Option<Tag>> {
    Ok(conn
        .query_row(
            "SELECT t.id, t.name, t.source, t.category, t.created_at, COUNT(it.image_id) AS image_count
             FROM tags t
             LEFT JOIN image_tags it ON it.tag_id = t.id
             WHERE t.name = ?1
             GROUP BY t.id",
            [name],
            |row| {
                Ok(Tag {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    source: row.get(2)?,
                    category: row.get(3)?,
                    created_at: row.get(4)?,
                    image_count: row.get(5)?,
                    group: None,
                })
            },
        )
        .optional()?)
}
