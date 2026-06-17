//! `duplicate_groups` / `duplicate_items` 的 CRUD。`docs/01-data-model.md` § 2.5 定义。
//!
//! 一张图可同时属于多个 group（exact 一个、phash 一个），所以 image_id 不唯一，
//! `(group_id, image_id)` 才是联合主键。

use rusqlite::{params, params_from_iter, types::Value, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::error::AppResult;

/// 去重方法。与 `docs/01` § 2.5 的 `method` 列对齐。
pub const METHOD_EXACT: &str = "exact";
pub const METHOD_PHASH: &str = "phash";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DuplicateGroup {
    pub id: i64,
    pub method: String,
    pub threshold: Option<f64>,
    pub keep_image_id: Option<i64>,
    pub status: String,
    pub created_at: i64,
    pub resolved_at: Option<i64>,
    pub item_count: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DuplicateItem {
    pub group_id: i64,
    pub image_id: i64,
    pub similarity: Option<f64>,
}

pub fn insert_group(
    conn: &Connection,
    method: &str,
    threshold: Option<f64>,
    now: i64,
) -> AppResult<i64> {
    conn.execute(
        "INSERT INTO duplicate_groups(method, threshold, status, created_at)
         VALUES (?1, ?2, 'open', ?3)",
        params![method, threshold, now],
    )?;
    Ok(conn.last_insert_rowid())
}

pub fn insert_items(
    conn: &Connection,
    group_id: i64,
    items: &[(i64, Option<f64>)],
) -> AppResult<usize> {
    let mut stmt = conn.prepare(
        "INSERT OR IGNORE INTO duplicate_items(group_id, image_id, similarity)
         VALUES (?1, ?2, ?3)",
    )?;
    let mut inserted = 0;
    for (image_id, similarity) in items {
        inserted += stmt.execute(params![group_id, image_id, similarity])?;
    }
    Ok(inserted)
}

pub fn get_group(conn: &Connection, id: i64) -> AppResult<Option<DuplicateGroup>> {
    let group = conn
        .query_row(
            "SELECT g.id, g.method, g.threshold, g.keep_image_id, g.status, g.created_at, g.resolved_at,
                    (SELECT COUNT(*) FROM duplicate_items WHERE group_id = g.id) AS item_count
             FROM duplicate_groups g WHERE g.id = ?1",
            [id],
            row_to_group,
        )
        .optional()?;
    Ok(group)
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GroupQueryParams {
    pub method: Option<String>,
    pub status: Option<String>,
    pub offset: Option<i64>,
    pub limit: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GroupPage {
    pub items: Vec<DuplicateGroup>,
    pub total: i64,
    pub offset: i64,
    pub limit: i64,
}

pub fn list_groups(conn: &Connection, params: &GroupQueryParams) -> AppResult<GroupPage> {
    let offset = params.offset.unwrap_or(0).max(0);
    let limit = params.limit.unwrap_or(100).clamp(1, 500);

    let mut clauses: Vec<&str> = Vec::new();
    let mut binds: Vec<Value> = Vec::new();
    if let Some(method) = params.method.as_ref().filter(|s| !s.is_empty()) {
        clauses.push("g.method = ?");
        binds.push(Value::Text(method.clone()));
    }
    if let Some(status) = params.status.as_ref().filter(|s| !s.is_empty()) {
        clauses.push("g.status = ?");
        binds.push(Value::Text(status.clone()));
    }
    let where_sql = if clauses.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", clauses.join(" AND "))
    };

    let count_sql = format!("SELECT COUNT(*) FROM duplicate_groups g {where_sql}");
    let total: i64 =
        conn.query_row(&count_sql, params_from_iter(binds.iter()), |row| row.get(0))?;

    let mut page_binds = binds;
    page_binds.push(Value::Integer(limit));
    page_binds.push(Value::Integer(offset));
    let list_sql = format!(
        "SELECT g.id, g.method, g.threshold, g.keep_image_id, g.status, g.created_at, g.resolved_at,
                (SELECT COUNT(*) FROM duplicate_items WHERE group_id = g.id) AS item_count
         FROM duplicate_groups g {where_sql}
         ORDER BY g.id DESC
         LIMIT ? OFFSET ?"
    );
    let mut stmt = conn.prepare(&list_sql)?;
    let rows = stmt.query_map(params_from_iter(page_binds.iter()), row_to_group)?;
    let mut items = Vec::new();
    for row in rows {
        items.push(row?);
    }
    Ok(GroupPage {
        items,
        total,
        offset,
        limit,
    })
}

pub fn items_for_group(conn: &Connection, group_id: i64) -> AppResult<Vec<DuplicateItem>> {
    let mut stmt = conn.prepare(
        "SELECT group_id, image_id, similarity
         FROM duplicate_items WHERE group_id = ?1
         ORDER BY similarity DESC NULLS LAST, image_id ASC",
    )?;
    let rows = stmt.query_map([group_id], |row| {
        Ok(DuplicateItem {
            group_id: row.get(0)?,
            image_id: row.get(1)?,
            similarity: row.get(2)?,
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// 查某张图属于该 method 的哪个 group（用于近似分组的"已在同组"判断）。
pub fn group_for_image(conn: &Connection, image_id: i64, method: &str) -> AppResult<Option<i64>> {
    let id = conn
        .query_row(
            "SELECT g.id FROM duplicate_groups g
             JOIN duplicate_items i ON i.group_id = g.id
             WHERE i.image_id = ?1 AND g.method = ?2 AND g.status = 'open'
             LIMIT 1",
            params![image_id, method],
            |row| row.get::<_, i64>(0),
        )
        .optional()?;
    Ok(id)
}

pub fn update_group_status(
    conn: &Connection,
    group_id: i64,
    status: &str,
    keep_image_id: Option<i64>,
    resolved_at: Option<i64>,
) -> AppResult<()> {
    conn.execute(
        "UPDATE duplicate_groups
         SET status = ?1,
             keep_image_id = COALESCE(?2, keep_image_id),
             resolved_at = ?3
         WHERE id = ?4",
        params![status, keep_image_id, resolved_at, group_id],
    )?;
    Ok(())
}

/// 把 `from` 的所有 item 转到 `into`，删 `from` group。用于近似分组的合并。
pub fn merge_into(conn: &Connection, from: i64, into: i64) -> AppResult<()> {
    if from == into {
        return Ok(());
    }
    conn.execute(
        "INSERT OR IGNORE INTO duplicate_items(group_id, image_id, similarity)
         SELECT ?1, image_id, similarity FROM duplicate_items WHERE group_id = ?2",
        params![into, from],
    )?;
    conn.execute("DELETE FROM duplicate_groups WHERE id = ?1", params![from])?;
    Ok(())
}

/// 删该 method 下所有 status='open' 的 group（重新跑 exact 分组前清场用）。
pub fn drop_open_groups(conn: &Connection, method: &str) -> AppResult<usize> {
    Ok(conn.execute(
        "DELETE FROM duplicate_groups WHERE method = ?1 AND status = 'open'",
        params![method],
    )?)
}

pub fn count_open_groups(conn: &Connection) -> AppResult<i64> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM duplicate_groups WHERE status = 'open'",
        [],
        |row| row.get(0),
    )?;
    Ok(count)
}

fn row_to_group(row: &rusqlite::Row<'_>) -> rusqlite::Result<DuplicateGroup> {
    Ok(DuplicateGroup {
        id: row.get(0)?,
        method: row.get(1)?,
        threshold: row.get(2)?,
        keep_image_id: row.get(3)?,
        status: row.get(4)?,
        created_at: row.get(5)?,
        resolved_at: row.get(6)?,
        item_count: row.get(7)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo::images_repo::{self, NewImageMetadata};
    use crate::repo::roots_repo::{self, NewRoot};

    fn fresh_pool() -> (crate::db::Pool, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let pool = crate::db::open(&dir.path().join("app.sqlite")).expect("db");
        (pool, dir)
    }

    fn insert_dummy(conn: &Connection, root_id: i64, name: &str) -> i64 {
        let outcome = images_repo::upsert_scanned_image(
            conn,
            &NewImageMetadata {
                root_id,
                rel_path: name.to_string(),
                filename: name.to_string(),
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
        .expect("insert image");
        outcome.image_id()
    }

    #[test]
    fn group_crud_roundtrip() {
        let (pool, _dir) = fresh_pool();
        let conn = pool.get().expect("conn");
        let root = roots_repo::insert(
            &conn,
            &NewRoot {
                path: "/tmp/g".to_string(),
                label: None,
                root_type: "local".to_string(),
            },
            1,
        )
        .expect("root");
        let a = insert_dummy(&conn, root.id, "a.jpg");
        let b = insert_dummy(&conn, root.id, "b.jpg");

        let g = insert_group(&conn, METHOD_EXACT, None, 100).expect("group");
        insert_items(&conn, g, &[(a, Some(1.0)), (b, Some(1.0))]).expect("items");

        let fetched = get_group(&conn, g).expect("get").expect("some");
        assert_eq!(fetched.item_count, 2);
        assert_eq!(fetched.method, METHOD_EXACT);
        assert_eq!(fetched.status, "open");

        let page = list_groups(
            &conn,
            &GroupQueryParams {
                method: Some(METHOD_EXACT.to_string()),
                status: Some("open".to_string()),
                offset: None,
                limit: None,
            },
        )
        .expect("list");
        assert_eq!(page.total, 1);
        assert_eq!(page.items[0].id, g);

        update_group_status(&conn, g, "resolved", Some(a), Some(200)).expect("resolve");
        let fetched = get_group(&conn, g).expect("get").expect("some");
        assert_eq!(fetched.status, "resolved");
        assert_eq!(fetched.keep_image_id, Some(a));
    }

    #[test]
    fn merge_into_combines_groups() {
        let (pool, _dir) = fresh_pool();
        let conn = pool.get().expect("conn");
        let root = roots_repo::insert(
            &conn,
            &NewRoot {
                path: "/tmp/m".to_string(),
                label: None,
                root_type: "local".to_string(),
            },
            1,
        )
        .expect("root");
        let a = insert_dummy(&conn, root.id, "a.jpg");
        let b = insert_dummy(&conn, root.id, "b.jpg");
        let c = insert_dummy(&conn, root.id, "c.jpg");

        let g1 = insert_group(&conn, METHOD_PHASH, Some(2.0), 1).expect("g1");
        insert_items(&conn, g1, &[(a, Some(0.99)), (b, Some(0.97))]).expect("items g1");
        let g2 = insert_group(&conn, METHOD_PHASH, Some(2.0), 1).expect("g2");
        insert_items(&conn, g2, &[(b, Some(0.96)), (c, Some(0.95))]).expect("items g2");

        merge_into(&conn, g2, g1).expect("merge");
        let items = items_for_group(&conn, g1).expect("items");
        assert_eq!(items.len(), 3);
        assert!(get_group(&conn, g2).expect("g2").is_none());
    }
}
