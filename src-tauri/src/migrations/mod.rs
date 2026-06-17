//! Migration runner：按编号顺序对未应用的 SQL 文件执行，每个 migration 在事务内运行。
//!
//! 命名约定：`NNNN_description.sql`，四位数字编号唯一且递增。
//! 已合并的 migration 文件**禁止修改**，错了发新 migration 修。

use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::Connection;

use crate::error::{AppError, AppResult};

/// 把所有 migration 在编译时嵌入二进制。新增 migration 在此 list 追加。
const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        name: "init",
        sql: include_str!("../../migrations/0001_init.sql"),
    },
    Migration {
        version: 2,
        name: "hash_columns",
        sql: include_str!("../../migrations/0002_hash_columns.sql"),
    },
    Migration {
        version: 3,
        name: "ai_columns",
        sql: include_str!("../../migrations/0003_ai_columns.sql"),
    },
];

struct Migration {
    version: u32,
    name: &'static str,
    sql: &'static str,
}

pub fn run(conn: &mut Connection) -> AppResult<()> {
    ensure_schema_version_table(conn)?;
    let current = current_version(conn)?;

    for m in MIGRATIONS {
        if m.version <= current {
            continue;
        }
        tracing::info!(version = m.version, name = m.name, "applying migration");
        apply_one(conn, m)?;
    }

    Ok(())
}

/// 当前最高已应用版本（0 表示从未运行过）
pub fn current_version(conn: &Connection) -> AppResult<u32> {
    let v: Option<u32> = conn.query_row("SELECT MAX(version) FROM schema_version", [], |r| {
        r.get::<_, Option<u32>>(0)
    })?;
    Ok(v.unwrap_or(0))
}

fn ensure_schema_version_table(conn: &Connection) -> AppResult<()> {
    // 0001_init.sql 自己也建 schema_version，但首次启动时它还没跑；
    // 用 IF NOT EXISTS 保证幂等，并允许 init.sql 里的非 IF NOT EXISTS CREATE 通过。
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_version (
             version    INTEGER PRIMARY KEY,
             applied_at INTEGER NOT NULL
         );",
    )?;
    Ok(())
}

fn apply_one(conn: &mut Connection, m: &Migration) -> AppResult<()> {
    let tx = conn.transaction()?;
    // 跑 SQL 文件时，0001 里的 `CREATE TABLE schema_version` 会与已存在的表冲突。
    // 处理方式：在 SQL 跑之前把 schema_version 表删掉（仅当 version=1 且当前为空时安全）。
    if m.version == 1 {
        let row_count: i64 =
            tx.query_row("SELECT COUNT(*) FROM schema_version", [], |r| r.get(0))?;
        if row_count == 0 {
            tx.execute("DROP TABLE schema_version", [])?;
        }
    }

    tx.execute_batch(m.sql).map_err(|e| AppError::Migration {
        version: m.version,
        source: e,
    })?;

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0) as i64;
    tx.execute(
        "INSERT OR REPLACE INTO schema_version(version, applied_at) VALUES (?1, ?2)",
        rusqlite::params![m.version, now],
    )?;

    tx.commit()?;
    tracing::info!(version = m.version, "migration applied");
    Ok(())
}
