//! VLM 打标流水线（docs/10 A6）：空闲时把待分析的图逐张送给 worker（解码 → llama-server →
//! 规范化），结果在一个事务里写进 `image_analysis`、`image_tags`（source = vlm）、`ai_jobs`。
//!
//! - **何时运行**：资源治理器允许时才投递（默认只在空闲档或用户点了"全速"）；降档后不再投递，
//!   在途的自然完成；全屏 / 游戏时立刻卸载自己启动的 llama-server。没活或不允许运行满 5 分钟后卸载。
//! - **读原图**：Rust 在读闸门许可下读好原图字节再发给 worker（网络盘并发 ≤ 4），读完即释放许可，
//!   不占着 NAS 读名额等推理。在途数量 = llama-server 的槽位数（`-np`）。
//! - **重复路径**：只差大小写的重复行（docs/11 §6.1）只分析一次，结果写给所有重复行。
//! - **错误**：NAS 断开、worker / llama-server 不可用这类临时错误不算失败次数，这张图 10 分钟后再试；
//!   连续多次临时错误时整体暂停 5 分钟。解码失败、模型输出不合格算一次失败，最多 3 次。
//! - **下游**：检索索引由触发器维护；描述变了，描述向量（A4）按输入指纹自动重算。

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection, TransactionBehavior};
use serde::Serialize;
use tauri::Emitter;

use crate::config::{AppPaths, AppSettings};
use crate::db::Pool;
use crate::error::AppResult;
use crate::repo::ai_jobs_repo::{
    self, JobResult, VlmCandidate, KIND_VLM_ANALYSIS, STATUS_FAILED, STATUS_READY,
};
use crate::repo::analysis_repo::{self, ImageAnalysis, SOURCE_MEMORA, SOURCE_VLM};
use crate::repo::tags_repo::{self, NewTagScore};
use crate::services::ai_client::{VlmAnalyzeParams, VlmResult};
use crate::services::ai_supervisor::AiSupervisor;
use crate::services::memora_import_service::{CATEGORY_TAG, DEFAULT_MEMORA_DB};
use crate::services::resource_governor::{ResourceGovernor, Tier};
use crate::services::settings_service;
use crate::services::startup_gate::StartupGate;
use crate::services::vlm_supervisor::{
    LaunchSpec, VlmPaths, VlmServerStatus, VlmStartError, VlmSupervisor, DEFAULT_PORT,
};
use crate::utils::{path_normalize, read_gate};

/// 写进 `image_analysis.model` 的模型名。
pub const VLM_MODEL: &str = "qwen3.5-9b-q4";
/// 提示词版本（与 worker `src/vlm/prompts.py` 一致）。改提示词就升版本，是否重跑由用户手动触发。
pub const PROMPT_VERSION: &str = "search-v1";
/// 没活或不允许运行多久后卸载自己启动的 llama-server。
pub const IDLE_UNLOAD_AFTER: Duration = Duration::from_secs(5 * 60);

const CANDIDATE_BATCH: i64 = 48;
const TRANSIENT_RETRY_AFTER: Duration = Duration::from_secs(10 * 60);
const BREAKER_THRESHOLD: usize = 6;
const BREAKER_PAUSE: Duration = Duration::from_secs(5 * 60);
const SERVER_RETRY_AFTER: Duration = Duration::from_secs(2 * 60);
const MAX_ORIGINAL_BYTES: u64 = 256 * 1024 * 1024;
const STARTUP_FALLBACK: Duration = Duration::from_secs(180);
const RATE_WINDOW: usize = 24;

/// `ai_jobs.model`：模型 + 提示词版本。
pub fn job_model() -> String {
    format!("{VLM_MODEL}@{PROMPT_VERSION}")
}

/// 解析 llama-server 与模型路径：设置里填了就用设置；三项都空时自动查找——先找
/// `%LOCALAPPDATA%\PhotoViewPlus\`（`models\qwen3.5-9b`、`runtime\llama`），再找 memora 的目录
/// （docs/10 §12.1：复用 memora 的程序和模型文件，不复制）。
pub fn resolve_vlm_paths(settings: &AppSettings, models_dir: &Path) -> Result<VlmPaths, String> {
    let configured = [
        &settings.vlm_server_exe,
        &settings.vlm_model_path,
        &settings.vlm_mmproj_path,
    ];
    if configured.iter().any(|value| !value.trim().is_empty()) {
        let paths = VlmPaths {
            exe: PathBuf::from(settings.vlm_server_exe.trim()),
            model: PathBuf::from(settings.vlm_model_path.trim()),
            mmproj: PathBuf::from(settings.vlm_mmproj_path.trim()),
        };
        return match paths.missing() {
            Some(reason) => Err(reason),
            None => Ok(paths),
        };
    }
    detect_vlm_paths(models_dir).ok_or_else(|| {
        "没有找到 llama-server 和 Qwen3.5-9B 模型文件，请在设置里填写路径".to_string()
    })
}

/// 自动查找的候选位置里第一套齐全的。
pub fn detect_vlm_paths(models_dir: &Path) -> Option<VlmPaths> {
    let mut roots = Vec::new();
    if let Some(root) = models_dir.parent() {
        roots.push(root.to_path_buf());
    }
    if let Some(root) = Path::new(DEFAULT_MEMORA_DB).parent().and_then(Path::parent) {
        roots.push(root.to_path_buf());
    }
    roots
        .iter()
        .map(|root| VlmPaths::under(root))
        .find(|paths| paths.missing().is_none())
}

/// 进度（`analysis:progress` 事件，每秒最多一次）。
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AnalysisProgress {
    /// disabled | notConfigured | waiting | paused | starting | running | idle | error
    pub state: String,
    pub reason: Option<String>,
    /// 待分析的图（含失败但还能重试的）。
    pub pending: i64,
    /// 当前模型 + 提示词版本下已完成的图。
    pub analyzed: i64,
    pub failed: i64,
    pub inflight: usize,
    pub session_done: usize,
    pub session_failed: usize,
    pub secs_per_image: Option<f64>,
    pub eta_secs: Option<u64>,
    pub last_error: Option<String>,
    pub model: String,
    pub server: VlmServerStatus,
}

/// 一张图的失败类型。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failure {
    /// 临时错误（NAS 断开、服务不可用）：不算失败次数，稍后再试。
    Transient(String),
    /// 这张图本身的问题（解码失败、模型输出不合格、文件不存在）：算一次失败。
    Permanent(String),
}

#[derive(Default)]
struct PipelineState {
    queue: VecDeque<VlmCandidate>,
    inflight_ids: HashSet<i64>,
    inflight_keys: HashSet<String>,
    retry_after: HashMap<i64, Instant>,
    consecutive_transient: usize,
    breaker_until: Option<Instant>,
    server_retry_after: Option<Instant>,
    session_done: usize,
    session_failed: usize,
    completions: VecDeque<Instant>,
    phase: String,
    reason: Option<String>,
    last_error: Option<String>,
}

pub struct AnalysisPipeline {
    pool: Pool,
    paths: AppPaths,
    worker: Arc<AiSupervisor>,
    vlm: Arc<VlmSupervisor>,
    governor: Arc<ResourceGovernor>,
    state: Mutex<PipelineState>,
    starting: AtomicBool,
}

enum Wake {
    Interval,
    Governor(bool),
}

impl AnalysisPipeline {
    pub fn new(
        pool: Pool,
        paths: AppPaths,
        worker: Arc<AiSupervisor>,
        vlm: Arc<VlmSupervisor>,
        governor: Arc<ResourceGovernor>,
    ) -> Arc<Self> {
        Arc::new(Self {
            pool,
            paths,
            worker,
            vlm,
            governor,
            state: Mutex::new(PipelineState {
                phase: "disabled".to_string(),
                ..PipelineState::default()
            }),
            starting: AtomicBool::new(false),
        })
    }

    pub fn vlm(&self) -> Arc<VlmSupervisor> {
        self.vlm.clone()
    }

    /// 后台循环：每秒推进一次并发进度；负载模式变化时立即推进（例如进入全屏立刻卸载）。
    pub fn spawn_loop(self: Arc<Self>, app: tauri::AppHandle<tauri::Wry>, gate: Arc<StartupGate>) {
        tauri::async_runtime::spawn(async move {
            gate.wait(STARTUP_FALLBACK).await;
            let mut changes = Some(self.governor.subscribe());
            let mut interval = tokio::time::interval(Duration::from_secs(1));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut last_sent: Option<AnalysisProgress> = None;
            loop {
                let wake = {
                    let governor_changed = async {
                        match changes.as_mut() {
                            Some(receiver) => receiver.changed().await.is_ok(),
                            None => std::future::pending().await,
                        }
                    };
                    tokio::select! {
                        _ = interval.tick() => Wake::Interval,
                        alive = governor_changed => Wake::Governor(alive),
                    }
                };
                if let Wake::Governor(false) = wake {
                    changes = None;
                }
                self.tick().await;
                if matches!(wake, Wake::Interval) {
                    let progress = self.progress().await;
                    if last_sent.as_ref() != Some(&progress) {
                        let _ = app.emit("analysis:progress", &progress);
                        last_sent = Some(progress);
                    }
                }
            }
        });
    }

    /// 推进一步：判断能不能跑、补候选、确保服务、投递。
    pub async fn tick(self: &Arc<Self>) {
        let settings = match settings_service::read(&self.paths) {
            Ok(settings) => settings,
            Err(error) => {
                self.set_phase("error", Some(error.to_string()));
                return;
            }
        };
        if self.governor.status().tier == Tier::Busy {
            // 全屏 / 游戏：立刻卸载（在途请求会失败，按临时错误处理，不计失败次数）。
            if self.vlm.stop(true).await {
                tracing::info!("fullscreen detected, llama-server unloaded");
            }
            self.set_phase("waiting", Some("全屏 / 游戏中，大模型已卸载".to_string()));
            return;
        }
        if !settings.ai_enabled || !settings.vlm_enabled {
            self.set_phase("disabled", None);
            self.unload_when_idle().await;
            return;
        }
        let vlm_paths = match resolve_vlm_paths(&settings, &self.paths.models_dir) {
            Ok(paths) => paths,
            Err(reason) => {
                self.set_phase("notConfigured", Some(reason));
                self.unload_when_idle().await;
                return;
            }
        };
        if !self.governor.vlm_allowed() {
            let reason = if settings.vlm_idle_only {
                "等电脑空闲后运行（可在状态栏切到\"全速\"）"
            } else {
                "后台处理已暂停"
            };
            self.set_phase("waiting", Some(reason.to_string()));
            self.unload_when_idle().await;
            return;
        }
        if let Some(reason) = self.paused_reason() {
            self.set_phase("paused", Some(reason));
            self.unload_when_idle().await;
            return;
        }
        if let Err(error) = self.refill().await {
            self.set_phase("error", Some(error.to_string()));
            return;
        }
        let (queued, inflight) = self.sizes();
        if queued == 0 && inflight == 0 {
            self.set_phase("idle", None);
            self.unload_when_idle().await;
            return;
        }
        if queued == 0 {
            self.set_phase("running", None);
            return;
        }
        if !self.vlm.is_ready().await {
            self.set_phase("starting", None);
            self.start_server(LaunchSpec::llama(
                &vlm_paths,
                settings.vlm_parallel,
                DEFAULT_PORT,
            ));
            return;
        }
        self.set_phase("running", None);
        self.dispatch(
            usize::from(settings.vlm_parallel.max(1)),
            settings.vlm_long_edge,
        );
    }

    fn set_phase(&self, phase: &str, reason: Option<String>) {
        if let Ok(mut state) = self.state.lock() {
            state.phase = phase.to_string();
            state.reason = reason;
        }
    }

    fn sizes(&self) -> (usize, usize) {
        self.state
            .lock()
            .map(|state| (state.queue.len(), state.inflight_ids.len()))
            .unwrap_or((0, 0))
    }

    /// 熔断（连续临时错误）或服务启动失败后的等待。
    fn paused_reason(&self) -> Option<String> {
        let mut state = self.state.lock().ok()?;
        let now = Instant::now();
        if let Some(until) = state.breaker_until {
            if until > now {
                let error = state.last_error.clone().unwrap_or_default();
                return Some(format!(
                    "连续读取失败，{} 秒后重试：{error}",
                    until.duration_since(now).as_secs()
                ));
            }
            state.breaker_until = None;
        }
        if let Some(until) = state.server_retry_after {
            if until > now {
                let error = state.last_error.clone().unwrap_or_default();
                return Some(format!(
                    "大模型服务启动失败，{} 秒后重试：{error}",
                    until.duration_since(now).as_secs()
                ));
            }
            state.server_retry_after = None;
        }
        None
    }

    async fn unload_when_idle(&self) {
        let inflight = self.sizes().1;
        if inflight == 0 && self.vlm.stop_if_idle(IDLE_UNLOAD_AFTER).await {
            tracing::info!("llama-server idle, unloaded");
        }
    }

    /// 候选队列不够一轮时从库里补一批（跳过在途、退避中的图，同路径重复行只留一个）。
    async fn refill(&self) -> AppResult<()> {
        let (need, skip_ids, skip_keys, backoff) = {
            let Ok(mut state) = self.state.lock() else {
                return Ok(());
            };
            let now = Instant::now();
            state.retry_after.retain(|_, until| *until > now);
            if state.queue.len() >= 8 {
                return Ok(());
            }
            (
                state.queue.len(),
                state
                    .inflight_ids
                    .iter()
                    .copied()
                    .chain(state.queue.iter().map(|candidate| candidate.image_id))
                    .collect::<HashSet<_>>(),
                state
                    .inflight_keys
                    .iter()
                    .cloned()
                    .chain(state.queue.iter().map(path_key))
                    .collect::<HashSet<_>>(),
                state.retry_after.keys().copied().collect::<HashSet<_>>(),
            )
        };
        let pool = self.pool.clone();
        let limit = CANDIDATE_BATCH + (skip_ids.len() + backoff.len()) as i64;
        let model = job_model();
        let rows = tokio::task::spawn_blocking(move || -> AppResult<Vec<VlmCandidate>> {
            let conn = pool.get()?;
            ai_jobs_repo::pending_vlm_analysis(&conn, &model, limit)
        })
        .await??;
        let mut keys = skip_keys;
        let fresh = rows
            .into_iter()
            .filter(|candidate| {
                !skip_ids.contains(&candidate.image_id)
                    && !backoff.contains(&candidate.image_id)
                    && keys.insert(path_key(candidate))
            })
            .take(CANDIDATE_BATCH as usize)
            .collect::<Vec<_>>();
        if let Ok(mut state) = self.state.lock() {
            if state.queue.len() == need {
                state.queue.extend(fresh);
            }
        }
        Ok(())
    }

    /// 在后台拉起 llama-server（加载模型要几十秒，不阻塞循环）。显存不够时先停掉空闲的 torch worker 再试一次。
    fn start_server(self: &Arc<Self>, spec: LaunchSpec) {
        if self.starting.swap(true, Ordering::SeqCst) {
            return;
        }
        let this = self.clone();
        tauri::async_runtime::spawn(async move {
            let mut result = this.vlm.ensure_running(&spec).await;
            if let Err(VlmStartError::Vram { .. }) = result {
                if this.worker.is_running().await {
                    tracing::info!(
                        "not enough VRAM for llama-server, stopping idle AI worker first"
                    );
                    let _ = this.worker.stop().await;
                    result = this.vlm.ensure_running(&spec).await;
                }
            }
            if let Err(error) = result {
                tracing::warn!(%error, "llama-server unavailable");
                if let Ok(mut state) = this.state.lock() {
                    state.last_error = Some(error.to_string());
                    state.server_retry_after = Some(Instant::now() + SERVER_RETRY_AFTER);
                }
            }
            this.starting.store(false, Ordering::SeqCst);
        });
    }

    fn dispatch(self: &Arc<Self>, parallel: usize, long_edge: u16) {
        loop {
            let candidate = {
                let Ok(mut state) = self.state.lock() else {
                    return;
                };
                if state.inflight_ids.len() >= parallel {
                    return;
                }
                let Some(candidate) = state.queue.pop_front() else {
                    return;
                };
                state.inflight_ids.insert(candidate.image_id);
                state.inflight_keys.insert(path_key(&candidate));
                candidate
            };
            let this = self.clone();
            tauri::async_runtime::spawn(async move {
                this.run_one(candidate, long_edge).await;
            });
        }
    }

    async fn run_one(self: Arc<Self>, candidate: VlmCandidate, long_edge: u16) {
        let outcome = self.analyze(&candidate, long_edge).await;
        let key = path_key(&candidate);
        let allowed = self.governor.vlm_allowed();
        let mut failure_to_record = None;
        if let Ok(mut state) = self.state.lock() {
            state.inflight_ids.remove(&candidate.image_id);
            state.inflight_keys.remove(&key);
            let now = Instant::now();
            match &outcome {
                Ok(()) => {
                    state.session_done += 1;
                    state.consecutive_transient = 0;
                    state.completions.push_back(now);
                    while state.completions.len() > RATE_WINDOW {
                        state.completions.pop_front();
                    }
                }
                Err(Failure::Transient(message)) => {
                    // 自己卸载（全屏 / 降档）造成的失败不退避、不计入熔断。
                    if allowed {
                        state
                            .retry_after
                            .insert(candidate.image_id, now + TRANSIENT_RETRY_AFTER);
                        state.consecutive_transient += 1;
                        if state.consecutive_transient >= BREAKER_THRESHOLD {
                            state.breaker_until = Some(now + BREAKER_PAUSE);
                            state.consecutive_transient = 0;
                        }
                    }
                    state.last_error = Some(message.clone());
                    tracing::debug!(image_id = candidate.image_id, %message, "VLM transient failure");
                }
                Err(Failure::Permanent(message)) => {
                    state.session_failed += 1;
                    state.consecutive_transient = 0;
                    state.last_error = Some(message.clone());
                    failure_to_record = Some(message.clone());
                }
            }
        }
        if let Some(message) = failure_to_record {
            tracing::warn!(image_id = candidate.image_id, %message, "VLM analysis failed");
            let pool = self.pool.clone();
            let image_id = candidate.image_id;
            let _ = tokio::task::spawn_blocking(move || -> AppResult<()> {
                let conn = pool.get()?;
                record_failure(&conn, image_id, &message, now_unix())
            })
            .await;
        }
    }

    /// 分析一张图：读原图（读闸门）→ worker → 写库。
    pub async fn analyze(&self, candidate: &VlmCandidate, long_edge: u16) -> Result<(), Failure> {
        let full_path = PathBuf::from(&candidate.root_path).join(&candidate.rel_path);
        let is_network = path_normalize::is_network_path(Path::new(&candidate.root_path));
        let bytes = {
            let _permit = read_gate::acquire_read(is_network)
                .await
                .map_err(|error| Failure::Transient(error.to_string()))?;
            let path = full_path.clone();
            tokio::task::spawn_blocking(move || read_original(&path))
                .await
                .map_err(|error| Failure::Transient(error.to_string()))??
        };
        self.vlm.touch();
        let params = VlmAnalyzeParams {
            server_url: self.vlm.base_url().to_string(),
            long_edge,
            taken_at: candidate.taken_at,
        };
        let response = self
            .worker
            .vlm_analyze(bytes, &params)
            .await
            .map_err(|error| Failure::Transient(format!("AI worker：{error}")))?;
        self.vlm.touch();
        if !response.ok {
            let message = response.error.unwrap_or_else(|| "未知错误".to_string());
            return Err(match response.error_kind.as_deref() {
                Some("server") => {
                    self.vlm.check_health().await;
                    Failure::Transient(message)
                }
                Some("decode") => Failure::Permanent(format!("无法解码：{message}")),
                _ => Failure::Permanent(message),
            });
        }
        let Some(result) = response.result else {
            return Err(Failure::Permanent("worker 没有返回分析结果".to_string()));
        };
        let pool = self.pool.clone();
        let candidate = candidate.clone();
        let duration_ms = response.duration_ms;
        tokio::task::spawn_blocking(move || -> AppResult<usize> {
            let mut conn = pool.get()?;
            save_vlm_result(&mut conn, &candidate, &result, duration_ms, now_unix())
        })
        .await
        .map_err(|error| Failure::Transient(error.to_string()))?
        .map_err(|error| Failure::Transient(format!("写入失败：{error}")))?;
        Ok(())
    }

    pub async fn progress(&self) -> AnalysisProgress {
        let pool = self.pool.clone();
        let model = job_model();
        let counts = tokio::task::spawn_blocking(move || -> AppResult<(i64, i64, i64)> {
            let conn = pool.get()?;
            let pending = ai_jobs_repo::count_pending_vlm_analysis(&conn, &model)?;
            let counts = ai_jobs_repo::counts(&conn, KIND_VLM_ANALYSIS, &model)?;
            Ok((pending, counts.ready, counts.failed))
        })
        .await
        .ok()
        .and_then(Result::ok);
        let (pending, analyzed, failed) = counts.unwrap_or((0, 0, 0));
        let server = self.vlm.status().await;
        let state = self.state.lock();
        let (phase, reason, inflight, session_done, session_failed, rate, last_error) = match state
        {
            Ok(state) => (
                state.phase.clone(),
                state.reason.clone(),
                state.inflight_ids.len(),
                state.session_done,
                state.session_failed,
                secs_per_image(&state.completions),
                state.last_error.clone(),
            ),
            Err(_) => ("error".to_string(), None, 0, 0, 0, None, None),
        };
        let eta_secs = rate
            .filter(|_| phase == "running")
            .map(|rate| (rate * pending as f64).round() as u64);
        AnalysisProgress {
            state: phase,
            reason,
            pending,
            analyzed,
            failed,
            inflight,
            session_done,
            session_failed,
            secs_per_image: rate,
            eta_secs,
            last_error,
            model: job_model(),
            server,
        }
    }
}

fn secs_per_image(completions: &VecDeque<Instant>) -> Option<f64> {
    let first = completions.front()?;
    let last = completions.back()?;
    let count = completions.len();
    (count >= 2).then(|| last.duration_since(*first).as_secs_f64() / (count - 1) as f64)
}

/// 忽略大小写的完整路径：只差大小写的重复行算同一张图。
fn path_key(candidate: &VlmCandidate) -> String {
    format!(
        "{}\\{}",
        candidate.root_path.to_lowercase(),
        candidate.rel_path.to_lowercase()
    )
}

/// 读原图字节。文件不存在 / 没有权限算这张图的失败；其他 I/O 错误（NAS 断开等）是临时错误。
pub fn read_original(path: &Path) -> Result<Vec<u8>, Failure> {
    let classify = |error: std::io::Error| match error.kind() {
        std::io::ErrorKind::NotFound => Failure::Permanent("文件不存在".to_string()),
        std::io::ErrorKind::PermissionDenied => Failure::Permanent("没有读取权限".to_string()),
        _ => Failure::Transient(format!("读取原图失败：{error}")),
    };
    let metadata = std::fs::metadata(path).map_err(classify)?;
    if metadata.len() > MAX_ORIGINAL_BYTES {
        return Err(Failure::Permanent(format!(
            "文件过大（{} MB）",
            metadata.len() / 1024 / 1024
        )));
    }
    std::fs::read(path).map_err(classify)
}

/// 同一路径只差大小写的未删除行（含自己）。
fn sibling_ids(conn: &Connection, candidate: &VlmCandidate) -> AppResult<Vec<i64>> {
    let mut stmt = conn.prepare_cached(
        "SELECT i.id FROM images i JOIN roots r ON r.id = i.root_id
         WHERE i.deleted_at IS NULL
           AND lower(r.path) = lower(?1) AND lower(i.rel_path) = lower(?2)
         ORDER BY i.id",
    )?;
    let rows = stmt.query_map(params![candidate.root_path, candidate.rel_path], |row| {
        row.get::<_, i64>(0)
    })?;
    let mut ids = rows.collect::<Result<Vec<_>, _>>()?;
    if ids.is_empty() {
        // 大小写比较没匹配上（理论上不会），至少写自己。
        ids.push(candidate.image_id);
    }
    Ok(ids)
}

/// 把一条 VLM 结果写给这张图及其大小写重复行（一个事务）：分析结果、标签（替换 vlm 和 memora
/// 来源的旧标签；类别也写成标签）、任务状态。返回写入的图片数。
pub fn save_vlm_result(
    conn: &mut Connection,
    candidate: &VlmCandidate,
    result: &VlmResult,
    duration_ms: Option<i64>,
    now: i64,
) -> AppResult<usize> {
    // IMMEDIATE：开头就拿写锁（等不到时按 busy_timeout 等待）。默认的 DEFERRED 事务先读后写，
    // 并发写入时升级写锁会直接报 "database is locked"，白算一张图。
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let ids = sibling_ids(&tx, candidate)?;
    let model = job_model();
    let tags = vlm_tags(result);
    let visible_text = Some(result.visible_text.trim().to_string()).filter(|text| !text.is_empty());
    for image_id in &ids {
        analysis_repo::upsert(
            &tx,
            &ImageAnalysis {
                image_id: *image_id,
                source: SOURCE_VLM.to_string(),
                model: VLM_MODEL.to_string(),
                prompt_version: PROMPT_VERSION.to_string(),
                description: result.description.trim().to_string(),
                categories: result.categories.clone(),
                keywords: result.keywords.clone(),
                people_count: Some(result.people_count),
                visible_text: visible_text.clone(),
                sensitive: result.sensitive,
                raw_json: result.raw_json.clone(),
                duration_ms,
                analyzed_at: now,
            },
        )?;
        tags_repo::replace_source_tags(&tx, *image_id, &[SOURCE_VLM, SOURCE_MEMORA], &tags, now)?;
        ai_jobs_repo::record(
            &tx,
            KIND_VLM_ANALYSIS,
            &model,
            &JobResult {
                image_id: *image_id,
                status: STATUS_READY,
                error: None,
                input_hash: None,
            },
            now,
        )?;
    }
    tx.commit()?;
    Ok(ids.len())
}

/// 类别（主类别分数高一点，`tags.category = 'category'`）+ 规范化后的标签。
fn vlm_tags(result: &VlmResult) -> Vec<NewTagScore> {
    let categories = result
        .categories
        .iter()
        .enumerate()
        .map(|(index, name)| NewTagScore {
            name: name.clone(),
            score: 0.9 - index as f64 * 0.05,
            source: SOURCE_VLM.to_string(),
            category: Some(CATEGORY_TAG.to_string()),
        });
    let tags = result.tags.iter().map(|tag| NewTagScore {
        name: tag.name.clone(),
        score: tag.score,
        source: SOURCE_VLM.to_string(),
        category: None,
    });
    categories.chain(tags).collect()
}

/// 记一次失败（同模型同版本累加次数，满 3 次后不再自动重试）。
pub fn record_failure(conn: &Connection, image_id: i64, error: &str, now: i64) -> AppResult<()> {
    ai_jobs_repo::record(
        conn,
        KIND_VLM_ANALYSIS,
        &job_model(),
        &JobResult {
            image_id,
            status: STATUS_FAILED,
            error: Some(error),
            input_hash: None,
        },
        now,
    )
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
