//! Roots 表的 CRUD。`docs/01-data-model.md` § 2.1 定义。

use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::error::AppResult;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Root {
    pub id: i64,
    pub path: String,
    pub label: Option<String>,
    pub root_type: String,
    pub enabled: bool,
    pub last_scan_at: Option<i64>,
    pub created_at: i64,
    pub settings_json: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewRoot {
    pub path: String,
    pub label: Option<String>,
    pub root_type: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct RootPatch {
    pub label: Option<String>,
    pub enabled: Option<bool>,
    pub settings_json: Option<String>,
}

pub fn insert(conn: &Connection, new_root: &NewRoot, now: i64) -> AppResult<Root> {
    conn.execute(
        "INSERT INTO roots(path, label, type, enabled, created_at)
         VALUES (?1, ?2, ?3, 1, ?4)",
        params![new_root.path, new_root.label, new_root.root_type, now],
    )?;
    let id = conn.last_insert_rowid();
    get(conn, id)?
        .ok_or_else(|| crate::error::AppError::Other("刚插入的目录记录不存在".to_string()))
}

pub fn get(conn: &Connection, id: i64) -> AppResult<Option<Root>> {
    let row = conn
        .query_row(
            "SELECT id, path, label, type, enabled, last_scan_at, created_at, settings_json
             FROM roots WHERE id = ?1",
            [id],
            row_to_root,
        )
        .optional()?;
    Ok(row)
}

pub fn find_by_path(conn: &Connection, path: &str) -> AppResult<Option<Root>> {
    let row = conn
        .query_row(
            "SELECT id, path, label, type, enabled, last_scan_at, created_at, settings_json
             FROM roots WHERE path = ?1",
            [path],
            row_to_root,
        )
        .optional()?;
    Ok(row)
}

pub fn list(conn: &Connection) -> AppResult<Vec<Root>> {
    let mut stmt = conn.prepare(
        "SELECT id, path, label, type, enabled, last_scan_at, created_at, settings_json
         FROM roots
         ORDER BY created_at ASC",
    )?;
    let rows = stmt.query_map([], row_to_root)?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

pub fn remove(conn: &Connection, id: i64) -> AppResult<bool> {
    let n = conn.execute("DELETE FROM roots WHERE id = ?1", [id])?;
    Ok(n > 0)
}

pub fn update(conn: &Connection, id: i64, patch: &RootPatch) -> AppResult<Option<Root>> {
    // 动态构造 UPDATE，避免无效的"全字段覆盖"
    let mut sets: Vec<&str> = Vec::new();
    let mut binds: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();

    if let Some(label) = &patch.label {
        sets.push("label = ?");
        binds.push(Box::new(label.clone()));
    }
    if let Some(enabled) = patch.enabled {
        sets.push("enabled = ?");
        binds.push(Box::new(i32::from(enabled)));
    }
    if let Some(settings_json) = &patch.settings_json {
        sets.push("settings_json = ?");
        binds.push(Box::new(settings_json.clone()));
    }

    if sets.is_empty() {
        return get(conn, id);
    }

    let sql = format!("UPDATE roots SET {} WHERE id = ?", sets.join(", "));
    binds.push(Box::new(id));

    let params_refs: Vec<&dyn rusqlite::ToSql> = binds.iter().map(|b| b.as_ref()).collect();
    conn.execute(&sql, params_refs.as_slice())?;
    get(conn, id)
}

pub fn touch_last_scan(conn: &Connection, id: i64, ts: i64) -> AppResult<()> {
    conn.execute(
        "UPDATE roots SET last_scan_at = ?1 WHERE id = ?2",
        params![ts, id],
    )?;
    Ok(())
}

fn row_to_root(row: &rusqlite::Row<'_>) -> rusqlite::Result<Root> {
    let enabled_int: i64 = row.get(4)?;
    Ok(Root {
        id: row.get(0)?,
        path: row.get(1)?,
        label: row.get(2)?,
        root_type: row.get(3)?,
        enabled: enabled_int != 0,
        last_scan_at: row.get(5)?,
        created_at: row.get(6)?,
        settings_json: row.get(7)?,
    })
}
