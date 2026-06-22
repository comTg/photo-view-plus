use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

use crate::error::AppResult;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SmartAlbum {
    pub id: i64,
    pub name: String,
    pub filter_json: String,
    pub icon: Option<String>,
    pub sort_order: i64,
    pub created_at: i64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SmartAlbumInput {
    pub name: String,
    pub filter_json: String,
    pub icon: Option<String>,
    pub sort_order: Option<i64>,
}

pub fn save(conn: &Connection, input: &SmartAlbumInput) -> AppResult<SmartAlbum> {
    let now = now_unix();
    conn.execute(
        "INSERT INTO smart_albums(name, filter_json, icon, sort_order, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            input.name.trim(),
            input.filter_json,
            input.icon,
            input.sort_order.unwrap_or(0),
            now
        ],
    )?;
    get(conn, conn.last_insert_rowid())
}

pub fn list(conn: &Connection) -> AppResult<Vec<SmartAlbum>> {
    let mut stmt = conn.prepare(
        "SELECT id, name, filter_json, icon, sort_order, created_at
         FROM smart_albums
         ORDER BY sort_order ASC, id ASC",
    )?;
    let rows = stmt.query_map([], row_to_album)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

pub fn get(conn: &Connection, id: i64) -> AppResult<SmartAlbum> {
    Ok(conn.query_row(
        "SELECT id, name, filter_json, icon, sort_order, created_at
         FROM smart_albums WHERE id = ?1",
        [id],
        row_to_album,
    )?)
}

pub fn delete(conn: &Connection, id: i64) -> AppResult<bool> {
    Ok(conn.execute("DELETE FROM smart_albums WHERE id = ?1", [id])? > 0)
}

fn row_to_album(row: &rusqlite::Row<'_>) -> rusqlite::Result<SmartAlbum> {
    Ok(SmartAlbum {
        id: row.get(0)?,
        name: row.get(1)?,
        filter_json: row.get(2)?,
        icon: row.get(3)?,
        sort_order: row.get(4)?,
        created_at: row.get(5)?,
    })
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}
