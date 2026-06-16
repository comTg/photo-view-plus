use std::path::PathBuf;

use rusqlite::types::Value;
use rusqlite::{params, params_from_iter, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::error::AppResult;

const IMAGE_SELECT: &str = "\
    SELECT i.id, i.root_id, i.rel_path, i.filename, i.extension, i.size_bytes, i.mtime,
           i.width, i.height, i.orientation, i.taken_at, i.gps_lat, i.gps_lng,
           i.camera_make, i.camera_model, i.thumb_status, i.thumb_hash, i.thumb_error,
           i.indexed_at, i.deleted_at, r.path,
           i.blake3, i.phash, i.dhash, i.hash_status
    FROM images i
    JOIN roots r ON r.id = i.root_id";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageRecord {
    pub id: i64,
    pub root_id: i64,
    pub rel_path: String,
    pub filename: String,
    pub extension: String,
    pub size_bytes: i64,
    pub mtime: i64,
    pub width: Option<i64>,
    pub height: Option<i64>,
    pub orientation: Option<i64>,
    pub taken_at: Option<i64>,
    pub gps_lat: Option<f64>,
    pub gps_lng: Option<f64>,
    pub camera_make: Option<String>,
    pub camera_model: Option<String>,
    pub thumb_status: String,
    pub thumb_hash: Option<String>,
    pub thumb_error: Option<String>,
    pub indexed_at: i64,
    pub deleted_at: Option<i64>,
    pub root_path: String,
    pub full_path: String,
    pub blake3: Option<String>,
    /// pHash/dHash 在 SQLite 用 INTEGER（i64）存，Rust 端做 hamming 时按 u64 处理。
    /// 用 `as i64` 跨边界保位模式即可。
    pub phash: Option<i64>,
    pub dhash: Option<i64>,
    pub hash_status: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImagePage {
    pub items: Vec<ImageRecord>,
    pub total: i64,
    pub offset: i64,
    pub limit: i64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageQueryParams {
    pub root_ids: Option<Vec<i64>>,
    pub formats: Option<Vec<String>>,
    pub q: Option<String>,
    pub size_min: Option<i64>,
    pub size_max: Option<i64>,
    pub taken_from: Option<i64>,
    pub taken_to: Option<i64>,
    pub has_gps: Option<bool>,
    pub sort: Option<ImageSort>,
    pub offset: Option<i64>,
    pub limit: Option<i64>,
    pub include_deleted: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageSort {
    pub field: String,
    pub dir: String,
}

#[derive(Debug, Clone)]
pub struct NewImageMetadata {
    pub root_id: i64,
    pub rel_path: String,
    pub filename: String,
    pub extension: String,
    pub size_bytes: i64,
    pub mtime: i64,
    pub width: Option<i64>,
    pub height: Option<i64>,
    pub orientation: Option<i64>,
    pub taken_at: Option<i64>,
    pub gps_lat: Option<f64>,
    pub gps_lng: Option<f64>,
    pub camera_make: Option<String>,
    pub camera_model: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpsertOutcome {
    Added(i64),
    Updated(i64),
    Unchanged(i64),
}

#[derive(Debug, Clone)]
pub struct RenameRecordPatch {
    pub rel_path: String,
    pub filename: String,
    pub extension: String,
    pub size_bytes: i64,
    pub mtime: i64,
    pub indexed_at: i64,
}

impl UpsertOutcome {
    pub fn image_id(self) -> i64 {
        match self {
            Self::Added(id) | Self::Updated(id) | Self::Unchanged(id) => id,
        }
    }
}

pub fn query(conn: &Connection, params: &ImageQueryParams) -> AppResult<ImagePage> {
    let offset = params.offset.unwrap_or(0).max(0);
    let limit = params.limit.unwrap_or(200).clamp(1, 500);
    let (where_sql, values) = build_where(params);
    let order_sql = order_by(params.sort.as_ref());

    let count_sql =
        format!("SELECT COUNT(*) FROM images i JOIN roots r ON r.id = i.root_id {where_sql}");
    let total: i64 = conn.query_row(&count_sql, params_from_iter(values.iter()), |row| {
        row.get(0)
    })?;

    let mut page_values = values;
    page_values.push(Value::Integer(limit));
    page_values.push(Value::Integer(offset));

    let sql = format!("{IMAGE_SELECT} {where_sql} {order_sql} LIMIT ? OFFSET ?");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_from_iter(page_values.iter()), row_to_record)?;

    let mut items = Vec::new();
    for row in rows {
        items.push(row?);
    }

    Ok(ImagePage {
        items,
        total,
        offset,
        limit,
    })
}

pub fn get_detail(conn: &Connection, id: i64) -> AppResult<Option<ImageRecord>> {
    let sql = format!("{IMAGE_SELECT} WHERE i.id = ?1");
    Ok(conn.query_row(&sql, [id], row_to_record).optional()?)
}

pub fn get_thumb_hash(conn: &Connection, id: i64) -> AppResult<Option<String>> {
    Ok(conn
        .query_row("SELECT thumb_hash FROM images WHERE id = ?1", [id], |row| {
            row.get(0)
        })
        .optional()?
        .flatten())
}

pub fn upsert_scanned_image(
    conn: &Connection,
    image: &NewImageMetadata,
    now: i64,
) -> AppResult<UpsertOutcome> {
    let existing = conn
        .query_row(
            "SELECT id, size_bytes, mtime, deleted_at FROM images WHERE root_id = ?1 AND rel_path = ?2",
            params![image.root_id, image.rel_path],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, Option<i64>>(3)?,
                ))
            },
        )
        .optional()?;

    if let Some((id, size_bytes, mtime, deleted_at)) = existing {
        if size_bytes == image.size_bytes && mtime == image.mtime && deleted_at.is_none() {
            update_exif_fields(conn, id, image, now)?;
            return Ok(UpsertOutcome::Unchanged(id));
        }

        conn.execute(
            "UPDATE images
             SET filename = ?1, extension = ?2, size_bytes = ?3, mtime = ?4,
                 width = ?5, height = ?6, orientation = ?7, taken_at = ?8,
                 gps_lat = ?9, gps_lng = ?10, camera_make = ?11, camera_model = ?12,
                 thumb_status = 'pending', thumb_hash = NULL, thumb_error = NULL,
                 indexed_at = ?13, deleted_at = NULL
             WHERE id = ?14",
            params![
                image.filename,
                image.extension,
                image.size_bytes,
                image.mtime,
                image.width,
                image.height,
                image.orientation,
                image.taken_at,
                image.gps_lat,
                image.gps_lng,
                image.camera_make,
                image.camera_model,
                now,
                id
            ],
        )?;
        return Ok(UpsertOutcome::Updated(id));
    }

    conn.execute(
        "INSERT INTO images(
             root_id, rel_path, filename, extension, size_bytes, mtime,
             width, height, orientation, taken_at, gps_lat, gps_lng,
             camera_make, camera_model, thumb_status, indexed_at
         )
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, 'pending', ?15)",
        params![
            image.root_id,
            image.rel_path,
            image.filename,
            image.extension,
            image.size_bytes,
            image.mtime,
            image.width,
            image.height,
            image.orientation,
            image.taken_at,
            image.gps_lat,
            image.gps_lng,
            image.camera_make,
            image.camera_model,
            now
        ],
    )?;
    Ok(UpsertOutcome::Added(conn.last_insert_rowid()))
}

pub fn update_thumbnail_ready(
    conn: &Connection,
    id: i64,
    thumb_hash: &str,
    width: Option<i64>,
    height: Option<i64>,
) -> AppResult<()> {
    conn.execute(
        "UPDATE images
         SET thumb_status = 'ready', thumb_hash = ?1, thumb_error = NULL,
             width = COALESCE(width, ?2), height = COALESCE(height, ?3)
         WHERE id = ?4",
        params![thumb_hash, width, height, id],
    )?;
    Ok(())
}

pub fn update_thumbnail_failed(
    conn: &Connection,
    id: i64,
    status: &str,
    error: &str,
) -> AppResult<()> {
    conn.execute(
        "UPDATE images SET thumb_status = ?1, thumb_error = ?2 WHERE id = ?3",
        params![status, error, id],
    )?;
    Ok(())
}

pub fn active_rel_paths(conn: &Connection, root_id: i64) -> AppResult<Vec<String>> {
    let mut stmt =
        conn.prepare("SELECT rel_path FROM images WHERE root_id = ?1 AND deleted_at IS NULL")?;
    let rows = stmt.query_map([root_id], |row| row.get::<_, String>(0))?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

pub fn mark_deleted(conn: &Connection, root_id: i64, rel_path: &str, now: i64) -> AppResult<()> {
    conn.execute(
        "UPDATE images SET deleted_at = ?1 WHERE root_id = ?2 AND rel_path = ?3",
        params![now, root_id, rel_path],
    )?;
    Ok(())
}

pub fn mark_deleted_by_id(conn: &Connection, id: i64, now: i64) -> AppResult<()> {
    conn.execute(
        "UPDATE images SET deleted_at = ?1 WHERE id = ?2",
        params![now, id],
    )?;
    Ok(())
}

pub fn restore_by_id(conn: &Connection, id: i64) -> AppResult<()> {
    conn.execute(
        "UPDATE images SET deleted_at = NULL WHERE id = ?1",
        [id],
    )?;
    Ok(())
}

/// 列出最近的撤销日志（未撤销的）。供 `历史` 面板。
pub fn list_undo_entries(conn: &Connection, limit: i64) -> AppResult<Vec<UndoEntry>> {
    let limit = limit.clamp(1, 200);
    let mut stmt = conn.prepare(
        "SELECT id, action, payload_json, can_undo_until, undone_at, created_at
         FROM undo_log
         WHERE undone_at IS NULL
         ORDER BY created_at DESC, id DESC
         LIMIT ?1",
    )?;
    let rows = stmt.query_map([limit], |row| {
        Ok(UndoEntry {
            id: row.get(0)?,
            action: row.get(1)?,
            payload_json: row.get(2)?,
            can_undo_until: row.get(3)?,
            undone_at: row.get(4)?,
            created_at: row.get(5)?,
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

pub fn get_undo_entry(conn: &Connection, id: i64) -> AppResult<Option<UndoEntry>> {
    Ok(conn
        .query_row(
            "SELECT id, action, payload_json, can_undo_until, undone_at, created_at
             FROM undo_log WHERE id = ?1",
            [id],
            |row| {
                Ok(UndoEntry {
                    id: row.get(0)?,
                    action: row.get(1)?,
                    payload_json: row.get(2)?,
                    can_undo_until: row.get(3)?,
                    undone_at: row.get(4)?,
                    created_at: row.get(5)?,
                })
            },
        )
        .optional()?)
}

pub fn mark_undo_done(conn: &Connection, id: i64, now: i64) -> AppResult<()> {
    conn.execute(
        "UPDATE undo_log SET undone_at = ?1 WHERE id = ?2",
        params![now, id],
    )?;
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UndoEntry {
    pub id: i64,
    pub action: String,
    pub payload_json: String,
    pub can_undo_until: i64,
    pub undone_at: Option<i64>,
    pub created_at: i64,
}

/// 写 blake3。`hash_status` 是流水线整体状态（pending/ready/failed），
/// 由 hash_service 显式调 `set_hash_status` 维护，这里不动。
pub fn set_blake3(conn: &Connection, id: i64, blake3: &str) -> AppResult<()> {
    conn.execute(
        "UPDATE images SET blake3 = ?1 WHERE id = ?2 AND deleted_at IS NULL",
        params![blake3, id],
    )?;
    Ok(())
}

/// 存 dHash/pHash 时把 u64 按位转 i64（SQLite INTEGER 容量 = i64）。读出时反向 `as u64`。
pub fn set_dhash(conn: &Connection, id: i64, dhash: u64) -> AppResult<()> {
    conn.execute(
        "UPDATE images SET dhash = ?1 WHERE id = ?2 AND deleted_at IS NULL",
        params![dhash as i64, id],
    )?;
    Ok(())
}

pub fn set_phash(conn: &Connection, id: i64, phash: u64) -> AppResult<()> {
    conn.execute(
        "UPDATE images SET phash = ?1 WHERE id = ?2 AND deleted_at IS NULL",
        params![phash as i64, id],
    )?;
    Ok(())
}

pub fn set_hash_status(conn: &Connection, id: i64, status: &str) -> AppResult<()> {
    conn.execute(
        "UPDATE images SET hash_status = ?1 WHERE id = ?2",
        params![status, id],
    )?;
    Ok(())
}

/// 还没算 BLAKE3 的图（未删除）。
pub fn pending_blake3_images(conn: &Connection) -> AppResult<Vec<ImageRecord>> {
    let sql = format!(
        "{IMAGE_SELECT} WHERE i.deleted_at IS NULL AND i.blake3 IS NULL AND i.hash_status != 'failed'"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], row_to_record)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// 缩略图已就绪但还没算 dHash 的图（视觉哈希依赖 thumb）。
pub fn pending_dhash_images(conn: &Connection) -> AppResult<Vec<ImageRecord>> {
    let sql = format!(
        "{IMAGE_SELECT} WHERE i.deleted_at IS NULL AND i.dhash IS NULL \
         AND i.thumb_status = 'ready' AND i.hash_status != 'failed'"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], row_to_record)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// 启动时全量取 (image_id, dhash) 用于建 BK-tree。
pub fn all_dhashes(conn: &Connection) -> AppResult<Vec<(i64, u64)>> {
    let mut stmt = conn.prepare(
        "SELECT id, dhash FROM images WHERE dhash IS NOT NULL AND deleted_at IS NULL",
    )?;
    let rows = stmt.query_map([], |row| {
        let id: i64 = row.get(0)?;
        let dhash: i64 = row.get(1)?;
        Ok((id, dhash as u64))
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// blake3 完全相同的分组（仅 count >= 2）。
pub fn find_blake3_duplicates(conn: &Connection) -> AppResult<Vec<(String, Vec<i64>)>> {
    let mut stmt = conn.prepare(
        "SELECT blake3, id FROM images
         WHERE blake3 IS NOT NULL AND deleted_at IS NULL
         ORDER BY blake3, id",
    )?;
    let rows = stmt.query_map([], |row| {
        let hash: String = row.get(0)?;
        let id: i64 = row.get(1)?;
        Ok((hash, id))
    })?;
    let mut groups: Vec<(String, Vec<i64>)> = Vec::new();
    for row in rows {
        let (hash, id) = row?;
        if let Some(last) = groups.last_mut() {
            if last.0 == hash {
                last.1.push(id);
                continue;
            }
        }
        groups.push((hash, vec![id]));
    }
    groups.retain(|(_, ids)| ids.len() > 1);
    Ok(groups)
}

pub fn rename_record(
    conn: &Connection,
    id: i64,
    patch: &RenameRecordPatch,
) -> AppResult<Option<ImageRecord>> {
    conn.execute(
        "UPDATE images
         SET rel_path = ?1, filename = ?2, extension = ?3, size_bytes = ?4, mtime = ?5, indexed_at = ?6
         WHERE id = ?7",
        params![
            patch.rel_path,
            patch.filename,
            patch.extension,
            patch.size_bytes,
            patch.mtime,
            patch.indexed_at,
            id
        ],
    )?;
    get_detail(conn, id)
}

pub fn insert_undo_log(
    conn: &Connection,
    action: &str,
    payload_json: &str,
    can_undo_until: i64,
    now: i64,
) -> AppResult<()> {
    conn.execute(
        "INSERT INTO undo_log(action, payload_json, can_undo_until, created_at)
         VALUES (?1, ?2, ?3, ?4)",
        params![action, payload_json, can_undo_until, now],
    )?;
    Ok(())
}

fn update_exif_fields(
    conn: &Connection,
    id: i64,
    image: &NewImageMetadata,
    now: i64,
) -> AppResult<()> {
    conn.execute(
        "UPDATE images
         SET width = COALESCE(width, ?1), height = COALESCE(height, ?2),
             orientation = COALESCE(orientation, ?3), taken_at = COALESCE(taken_at, ?4),
             gps_lat = COALESCE(gps_lat, ?5), gps_lng = COALESCE(gps_lng, ?6),
             camera_make = COALESCE(camera_make, ?7), camera_model = COALESCE(camera_model, ?8),
             indexed_at = ?9
         WHERE id = ?10",
        params![
            image.width,
            image.height,
            image.orientation,
            image.taken_at,
            image.gps_lat,
            image.gps_lng,
            image.camera_make,
            image.camera_model,
            now,
            id
        ],
    )?;
    Ok(())
}

fn build_where(params: &ImageQueryParams) -> (String, Vec<Value>) {
    let mut clauses = Vec::new();
    let mut values = Vec::new();

    if !params.include_deleted.unwrap_or(false) {
        clauses.push("i.deleted_at IS NULL".to_string());
    }

    if let Some(root_ids) = params.root_ids.as_ref().filter(|ids| !ids.is_empty()) {
        clauses.push(format!("i.root_id IN ({})", placeholders(root_ids.len())));
        values.extend(root_ids.iter().copied().map(Value::Integer));
    }

    if let Some(formats) = params
        .formats
        .as_ref()
        .filter(|formats| !formats.is_empty())
    {
        clauses.push(format!("i.extension IN ({})", placeholders(formats.len())));
        values.extend(
            formats
                .iter()
                .map(|format| Value::Text(format.trim_start_matches('.').to_ascii_lowercase())),
        );
    }

    if let Some(q) = params
        .q
        .as_ref()
        .map(|q| q.trim())
        .filter(|q| !q.is_empty())
    {
        clauses.push("i.filename LIKE ? ESCAPE '\\'".to_string());
        values.push(Value::Text(format!("%{}%", escape_like(q))));
    }

    if let Some(size_min) = params.size_min {
        clauses.push("i.size_bytes >= ?".to_string());
        values.push(Value::Integer(size_min.max(0)));
    }

    if let Some(size_max) = params.size_max {
        clauses.push("i.size_bytes <= ?".to_string());
        values.push(Value::Integer(size_max.max(0)));
    }

    if let Some(taken_from) = params.taken_from {
        clauses.push("COALESCE(i.taken_at, i.mtime) >= ?".to_string());
        values.push(Value::Integer(taken_from));
    }

    if let Some(taken_to) = params.taken_to {
        clauses.push("COALESCE(i.taken_at, i.mtime) <= ?".to_string());
        values.push(Value::Integer(taken_to));
    }

    if let Some(has_gps) = params.has_gps {
        if has_gps {
            clauses.push("i.gps_lat IS NOT NULL AND i.gps_lng IS NOT NULL".to_string());
        } else {
            clauses.push("(i.gps_lat IS NULL OR i.gps_lng IS NULL)".to_string());
        }
    }

    if clauses.is_empty() {
        (String::new(), values)
    } else {
        (format!("WHERE {}", clauses.join(" AND ")), values)
    }
}

fn order_by(sort: Option<&ImageSort>) -> &'static str {
    let (field, dir) = sort
        .map(|sort| (sort.field.as_str(), sort.dir.as_str()))
        .unwrap_or(("mtime", "desc"));
    let direction = if dir.eq_ignore_ascii_case("asc") {
        "ASC"
    } else {
        "DESC"
    };

    match field {
        "taken_at" => {
            if direction == "ASC" {
                "ORDER BY COALESCE(i.taken_at, i.mtime) ASC, i.id ASC"
            } else {
                "ORDER BY COALESCE(i.taken_at, i.mtime) DESC, i.id DESC"
            }
        }
        "filename" => {
            if direction == "ASC" {
                "ORDER BY LOWER(i.filename) ASC, i.id ASC"
            } else {
                "ORDER BY LOWER(i.filename) DESC, i.id DESC"
            }
        }
        "size" => {
            if direction == "ASC" {
                "ORDER BY i.size_bytes ASC, i.id ASC"
            } else {
                "ORDER BY i.size_bytes DESC, i.id DESC"
            }
        }
        _ => {
            if direction == "ASC" {
                "ORDER BY i.mtime ASC, i.id ASC"
            } else {
                "ORDER BY i.mtime DESC, i.id DESC"
            }
        }
    }
}

fn row_to_record(row: &rusqlite::Row<'_>) -> rusqlite::Result<ImageRecord> {
    let root_path: String = row.get(20)?;
    let rel_path: String = row.get(2)?;
    let full_path = PathBuf::from(&root_path)
        .join(&rel_path)
        .to_string_lossy()
        .to_string();

    Ok(ImageRecord {
        id: row.get(0)?,
        root_id: row.get(1)?,
        rel_path,
        filename: row.get(3)?,
        extension: row.get(4)?,
        size_bytes: row.get(5)?,
        mtime: row.get(6)?,
        width: row.get(7)?,
        height: row.get(8)?,
        orientation: row.get(9)?,
        taken_at: row.get(10)?,
        gps_lat: row.get(11)?,
        gps_lng: row.get(12)?,
        camera_make: row.get(13)?,
        camera_model: row.get(14)?,
        thumb_status: row.get(15)?,
        thumb_hash: row.get(16)?,
        thumb_error: row.get(17)?,
        indexed_at: row.get(18)?,
        deleted_at: row.get(19)?,
        root_path,
        full_path,
        blake3: row.get(21)?,
        phash: row.get(22)?,
        dhash: row.get(23)?,
        hash_status: row.get(24)?,
    })
}

fn placeholders(count: usize) -> String {
    std::iter::repeat("?")
        .take(count)
        .collect::<Vec<_>>()
        .join(",")
}

fn escape_like(input: &str) -> String {
    input
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

#[cfg(test)]
mod tests {
    use crate::repo::roots_repo::{self, NewRoot};

    use super::*;

    #[test]
    fn upsert_and_query_roundtrip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pool = crate::db::open(&dir.path().join("app.sqlite")).expect("db");
        let conn = pool.get().expect("conn");
        let root = roots_repo::insert(
            &conn,
            &NewRoot {
                path: dir.path().to_string_lossy().to_string(),
                label: None,
                root_type: "local".to_string(),
            },
            1,
        )
        .expect("root");

        let image = NewImageMetadata {
            root_id: root.id,
            rel_path: "IMG_001.JPG".to_string(),
            filename: "IMG_001.JPG".to_string(),
            extension: "jpg".to_string(),
            size_bytes: 42,
            mtime: 2,
            width: Some(10),
            height: Some(20),
            orientation: None,
            taken_at: None,
            gps_lat: None,
            gps_lng: None,
            camera_make: Some("Camera".to_string()),
            camera_model: None,
        };
        let outcome = upsert_scanned_image(&conn, &image, 3).expect("insert");
        assert!(matches!(outcome, UpsertOutcome::Added(_)));

        let page = query(
            &conn,
            &ImageQueryParams {
                root_ids: Some(vec![root.id]),
                formats: Some(vec!["jpg".to_string()]),
                q: Some("IMG".to_string()),
                size_min: Some(1),
                size_max: Some(100),
                taken_from: None,
                taken_to: None,
                has_gps: Some(false),
                sort: None,
                offset: Some(0),
                limit: Some(10),
                include_deleted: Some(false),
            },
        )
        .expect("query");
        assert_eq!(page.total, 1);
        assert_eq!(page.items[0].filename, "IMG_001.JPG");
    }
}
