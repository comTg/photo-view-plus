//! 夜间后台分析（docs/10 A6b、§12.4）：Windows 计划任务在夜间窗口开始时运行
//! `photo-view-plus.exe --background-analyze`——没有窗口，只跑大模型打标（顺带补描述向量和检索索引），
//! 处理完或到窗口结束就退出。
//!
//! - 单实例：同一时间只有一个后台分析进程；主程序开着时直接退出，交给它（主程序在电脑空闲时自己会跑）。
//!   后台分析进行中打开了主程序，也会让在途的分析完成后退出。
//! - 和 memora 共用 8081 的 llama-server：已有健康服务就复用、不关；memora 的任务在 02:00，默认窗口 03:00–07:00。
//! - 仍然受资源治理器约束：半夜有人用电脑或在全屏游戏，照样让路。
//! - 计划任务由设置页的开关注册 / 删除（`schtasks`），用户不打开就不会注册。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{Duration as ChronoDuration, Local, NaiveDateTime, Timelike};
use serde::Serialize;

use crate::config::{AppPaths, Profile};
use crate::db;
use crate::error::{AppError, AppResult};
use crate::queue::{Priority, Scheduler};
use crate::services::ai_pipeline::AiPipeline;
use crate::services::ai_supervisor::{self, AiSupervisor};
use crate::services::analysis_pipeline::AnalysisPipeline;
use crate::services::resource_governor::{self, OverrideMode, ResourceGovernor};
use crate::services::vlm_supervisor::{VlmSupervisor, DEFAULT_PORT};
use crate::services::{search_index_service, settings_service, thumbnail_service};
use crate::utils::instance::InstanceLock;
use crate::utils::read_gate;
use crate::utils::win_activity::SystemActivity;

/// 计划任务调用的命令行参数。
pub const BACKGROUND_ANALYZE_ARG: &str = "--background-analyze";
/// 不管时间窗口，立即跑一轮（手动排查用）。
pub const FORCE_ARG: &str = "--force";

const TICK: Duration = Duration::from_secs(1);
const DRAIN_TIMEOUT: Duration = Duration::from_secs(60);

pub fn gui_mutex_name(identifier: &str) -> String {
    format!("Local\\PhotoViewPlus.{identifier}.gui")
}

pub fn background_mutex_name(identifier: &str) -> String {
    format!("Local\\PhotoViewPlus.{identifier}.background")
}

/// 计划任务名：正式版 `PhotoViewPlus\BackgroundAnalyze`，dev / test 版本带后缀，互不覆盖。
pub fn task_name(identifier: &str) -> String {
    match identifier.rsplit('.').next() {
        Some(suffix @ ("dev" | "test")) => format!("PhotoViewPlus\\BackgroundAnalyze-{suffix}"),
        _ => "PhotoViewPlus\\BackgroundAnalyze".to_string(),
    }
}

/// 夜间窗口 `[start, end)`（本地时间，可跨午夜，例如 23–6）。在窗口内返回这次窗口的结束时刻，否则 None。
pub fn night_deadline(now: NaiveDateTime, start_hour: u8, end_hour: u8) -> Option<NaiveDateTime> {
    let (start, end) = (u32::from(start_hour % 24), u32::from(end_hour % 24));
    if start == end {
        return None;
    }
    let hour = now.hour();
    let end_today = now.date().and_hms_opt(end, 0, 0)?;
    if start < end {
        (start..end).contains(&hour).then_some(end_today)
    } else if hour >= start {
        Some(end_today + ChronoDuration::days(1))
    } else if hour < end {
        Some(end_today)
    } else {
        None
    }
}

pub fn create_task_args(task: &str, exe: &Path, start_hour: u8) -> Vec<String> {
    vec![
        "/Create".to_string(),
        "/F".to_string(),
        "/SC".to_string(),
        "DAILY".to_string(),
        "/ST".to_string(),
        format!("{:02}:00", start_hour % 24),
        "/TN".to_string(),
        task.to_string(),
        "/TR".to_string(),
        format!("\"{}\" {BACKGROUND_ANALYZE_ARG}", exe.display()),
        "/RL".to_string(),
        "LIMITED".to_string(),
    ]
}

pub fn delete_task_args(task: &str) -> Vec<String> {
    vec![
        "/Delete".to_string(),
        "/F".to_string(),
        "/TN".to_string(),
        task.to_string(),
    ]
}

fn schtasks(args: &[String]) -> AppResult<std::process::Output> {
    let mut command = std::process::Command::new("schtasks");
    command.args(args);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    Ok(command.output()?)
}

/// 计划任务是否已注册。
pub fn task_registered(task: &str) -> bool {
    schtasks(&["/Query".to_string(), "/TN".to_string(), task.to_string()])
        .map(|output| output.status.success())
        .unwrap_or(false)
}

/// 注册（或覆盖）每天 `start_hour` 点运行的计划任务。只在用户在设置页打开开关时调用。
pub fn register_task(task: &str, exe: &Path, start_hour: u8) -> AppResult<()> {
    let output = schtasks(&create_task_args(task, exe, start_hour))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(AppError::Other(format!(
            "注册计划任务失败：{}",
            String::from_utf8_lossy(&output.stderr).trim()
        )))
    }
}

pub fn unregister_task(task: &str) -> AppResult<()> {
    if !task_registered(task) {
        return Ok(());
    }
    let output = schtasks(&delete_task_args(task))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(AppError::Other(format!(
            "删除计划任务失败：{}",
            String::from_utf8_lossy(&output.stderr).trim()
        )))
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NightTaskStatus {
    pub enabled: bool,
    pub registered: bool,
    pub task_name: String,
    pub start_hour: u8,
    pub end_hour: u8,
    pub exe: String,
}

/// 无窗口的后台分析入口（`main` 看到 [`BACKGROUND_ANALYZE_ARG`] 时调用）。
pub fn run_background_analyze(profile: Profile, identifier: &str, worker_dir: PathBuf) {
    let force = std::env::args().any(|arg| arg == FORCE_ARG);
    let outcome =
        tauri::async_runtime::block_on(background_main(profile, identifier, worker_dir, force));
    match outcome {
        Ok(summary) => tracing::info!(%summary, "background analyze finished"),
        Err(error) => tracing::warn!(%error, "background analyze failed"),
    }
}

async fn background_main(
    profile: Profile,
    identifier: &str,
    worker_dir: PathBuf,
    force: bool,
) -> AppResult<String> {
    if InstanceLock::is_held(&gui_mutex_name(identifier)) {
        return Ok("主程序正在运行，交给它在空闲时分析".to_string());
    }
    let Some(_lock) = InstanceLock::acquire(&background_mutex_name(identifier)) else {
        return Ok("已有一个后台分析在运行".to_string());
    };
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .ok_or_else(|| AppError::Other("找不到 LOCALAPPDATA".to_string()))?;
    let paths = AppPaths::from_data_dir(base.join(identifier));
    let settings = settings_service::read(&paths)?;
    if !settings.ai_enabled || !settings.vlm_enabled {
        return Ok("大模型打标没有开启".to_string());
    }
    let now = Local::now().naive_local();
    let deadline = if force {
        now + ChronoDuration::hours(4)
    } else {
        match night_deadline(now, settings.night_start_hour, settings.night_end_hour) {
            Some(deadline) => deadline,
            None => return Ok("不在夜间窗口内".to_string()),
        }
    };
    tracing::info!(?profile, %deadline, "background analyze started");

    let pool = db::open(&paths.db_path)?;
    let mut caps = [usize::MAX; 8];
    for priority in [Priority::P4, Priority::P5, Priority::P6, Priority::P7] {
        caps[priority as usize] = 1;
    }
    let scheduler = Scheduler::start_with_caps(4, caps);
    let worker = Arc::new(AiSupervisor::new(worker_dir, paths.models_dir.clone())?);
    worker.set_idle_stop(ai_supervisor::idle_stop_after(
        settings.ai_idle_stop_minutes,
    ));
    let (budget, thresholds) =
        resource_governor::settings_from(&settings, thumbnail_service::thumbnail_cpu_limit());
    let threads_worker = worker.clone();
    let governor = ResourceGovernor::new(
        Box::new(SystemActivity),
        budget,
        thresholds,
        worker.gpu_gate(),
        Box::new(move |budget| {
            read_gate::set_network_limit(budget.network_reads);
            let worker = threads_worker.clone();
            let threads = budget.worker_threads;
            tauri::async_runtime::spawn(async move {
                worker.set_worker_threads(threads).await;
            });
        }),
    );
    let vlm = Arc::new(VlmSupervisor::new(
        DEFAULT_PORT,
        crate::log_dir_for(profile),
    )?);
    let analysis = AnalysisPipeline::new(
        pool.clone(),
        paths.clone(),
        worker.clone(),
        vlm.clone(),
        governor.clone(),
    );
    let embeddings = AiPipeline::new(pool.clone(), scheduler.clone(), worker.clone(), &paths);

    let started = Instant::now();
    let mut last_embed = Instant::now();
    let mut last_flush = Instant::now();
    let reason;
    loop {
        governor.tick(Instant::now());
        if InstanceLock::is_held(&gui_mutex_name(identifier)) {
            reason = "主程序打开了，交给它继续";
            break;
        }
        if Local::now().naive_local() >= deadline {
            reason = "到了夜间窗口结束时间";
            break;
        }
        analysis.tick().await;
        if last_embed.elapsed() >= Duration::from_secs(2) {
            last_embed = Instant::now();
            if let Err(error) = embeddings.enqueue_pending().await {
                tracing::debug!(%error, "background embedding enqueue failed");
            }
        }
        if last_flush.elapsed() >= Duration::from_secs(10) {
            last_flush = Instant::now();
            flush_index(&pool).await;
        }
        let progress = analysis.progress().await;
        let embedding = embeddings.status();
        if progress.pending == 0
            && progress.inflight == 0
            && embedding.embedding_pending == 0
            && embedding.text_embedding_pending == 0
        {
            reason = "全部处理完";
            break;
        }
        tokio::time::sleep(TICK).await;
    }

    // 收尾：不再投递，等在途的完成，卸载自己启动的模型。
    governor.set_override(OverrideMode::Paused, None, Instant::now());
    let drain_started = Instant::now();
    while analysis.progress().await.inflight > 0 && drain_started.elapsed() < DRAIN_TIMEOUT {
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let progress = analysis.progress().await;
    vlm.stop(false).await;
    let _ = worker.stop().await;
    flush_index(&pool).await;
    Ok(format!(
        "{reason}：本次分析 {} 张，失败 {} 张，用时 {} 分钟，剩余 {} 张",
        progress.session_done,
        progress.session_failed,
        started.elapsed().as_secs() / 60,
        progress.pending
    ))
}

async fn flush_index(pool: &db::Pool) {
    let pool = pool.clone();
    let result = tokio::task::spawn_blocking(move || -> AppResult<usize> {
        let mut conn = pool.get()?;
        search_index_service::flush_all(&mut conn)
    })
    .await;
    if let Ok(Err(error)) = result {
        tracing::debug!(%error, "background search index flush failed");
    }
}
