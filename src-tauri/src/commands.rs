use tauri::State;

use crate::config::Profile;
use crate::db::Pool;
use crate::migrations;

#[tauri::command]
pub fn ping(profile: State<'_, Profile>) -> String {
    profile.as_str().to_string()
}

/// 返回数据库基本状态：路径不可达时报错，可达则返回 schema 版本与连接池容量。
#[tauri::command]
pub fn db_status(pool: State<'_, Pool>) -> Result<DbStatus, String> {
    let conn = pool.get().map_err(|e| e.to_string())?;
    let version = migrations::current_version(&conn).map_err(|e| e.to_string())?;
    Ok(DbStatus {
        schema_version: version,
        pool_size: pool.state().connections,
    })
}

#[derive(serde::Serialize)]
pub struct DbStatus {
    pub schema_version: u32,
    pub pool_size: u32,
}
