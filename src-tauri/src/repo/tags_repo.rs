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

pub fn upsert_tag(
    conn: &Connection,
    name: &str,
    source: &str,
    category: Option<&str>,
    now: i64,
) -> AppResult<i64> {
    conn.execute(
        "INSERT INTO tags(name, source, category, created_at)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(name) DO UPDATE SET
             source = excluded.source,
             category = COALESCE(excluded.category, tags.category)",
        params![name, source, category, now],
    )?;

    let id = conn.query_row("SELECT id FROM tags WHERE name = ?1", [name], |row| {
        row.get(0)
    })?;
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

pub fn list_tags(conn: &Connection, limit: i64) -> AppResult<Vec<Tag>> {
    let mut stmt = conn.prepare(
        "SELECT t.id, t.name, t.source, t.category, t.created_at, COUNT(it.image_id) AS image_count
         FROM tags t
         LEFT JOIN image_tags it ON it.tag_id = t.id
         GROUP BY t.id
         ORDER BY image_count DESC, t.name ASC
         LIMIT ?1",
    )?;
    let rows = stmt.query_map([limit.clamp(1, 500)], |row| {
        Ok(Tag {
            id: row.get(0)?,
            name: row.get(1)?,
            source: row.get(2)?,
            category: row.get(3)?,
            created_at: row.get(4)?,
            image_count: row.get(5)?,
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
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
                })
            },
        )
        .optional()?)
}
