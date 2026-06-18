use std::sync::Arc;

use tauri::State;

use crate::config::Profile;
use crate::db::Pool;
use crate::migrations;
use crate::services::startup_gate::StartupGate;

#[tauri::command]
pub fn ping(profile: State<'_, Profile>) -> String {
    profile.as_str().to_string()
}

/// 诊断用：前端把启动各阶段的相对耗时（performance.now()，相对 webview 导航起点）
/// 回传到主进程日志，方便在同一份终端日志里看清「窗口出现 → 首屏绘制」的时间线。
/// 白屏排查完成后可连同前端的 markBoot 一起移除。
#[tauri::command]
pub fn ui_perf(label: String, t_ms: f64) {
    tracing::info!(label = %label, t_ms, "ui boot perf");
}

/// 前端首屏绘制完成后调用，放行被推迟的启动期重活（缩略图重排等），
/// 让窗口先出图、再做吃 CPU/磁盘的后台工作。见 [`StartupGate`]。
#[tauri::command]
pub fn frontend_ready(gate: State<'_, Arc<StartupGate>>) {
    gate.release();
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
