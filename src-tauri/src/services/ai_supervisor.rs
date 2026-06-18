use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde::Serialize;
use tauri::Emitter;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, BufReader, Lines};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use tokio::time::Instant;

use crate::error::{AppError, AppResult};
use crate::services::ai_client::{
    AiHttpClient, ClipEmbedItem, ClipEmbedResponse, TaggerItem, TaggerResponse, TextEncodeResponse,
    WorkerHealth,
};

const START_TIMEOUT: Duration = Duration::from_secs(20);
const HEALTH_INTERVAL: Duration = Duration::from_secs(1);
const HEALTH_RESTART_LIMIT: usize = 3;
const RESTART_WINDOW: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AiWorkerStatus {
    pub status: String,
    pub device: Option<String>,
    pub compute: Option<String>,
    pub pid: Option<u32>,
    pub port: Option<u16>,
    pub last_error: Option<String>,
    pub restarts_last_minute: usize,
    pub worker_dir: String,
    pub models_dir: String,
}

struct AiSupervisorInner {
    child: Option<Child>,
    status: AiWorkerStatus,
    restart_times: VecDeque<Instant>,
    /// 连续启动失败次数，用于计算退避时长。
    start_failures: u32,
    /// 退避窗口截止时刻：在此之前 `ensure_ready` 不再尝试启动。
    start_cooldown_until: Option<Instant>,
}

pub struct AiSupervisor {
    worker_dir: PathBuf,
    models_dir: PathBuf,
    client: AiHttpClient,
    inner: Mutex<AiSupervisorInner>,
    /// 串行化 worker 启动。`spawn_worker` 要加载模型、耗时数秒，期间不能持 `inner` 锁
    /// （否则阻塞 status/health 查询）；但若不加这把锁，并发的后台任务/重试会在
    /// `child` 仍为 None 时各自拉起一个 Python 进程（thundering herd，曾一次炸出十几个）。
    start_lock: Mutex<()>,
}

#[derive(Debug, Clone)]
struct StartCandidate {
    program: PathBuf,
    args: Vec<String>,
    label: String,
}

struct StartedWorker {
    child: Child,
    port: u16,
    pid: Option<u32>,
    health: WorkerHealth,
}

impl AiSupervisor {
    pub fn new(worker_dir: PathBuf, models_dir: PathBuf) -> AppResult<Self> {
        let status = AiWorkerStatus {
            status: "stopped".to_string(),
            device: None,
            compute: None,
            pid: None,
            port: None,
            last_error: None,
            restarts_last_minute: 0,
            worker_dir: worker_dir.to_string_lossy().to_string(),
            models_dir: models_dir.to_string_lossy().to_string(),
        };
        Ok(Self {
            worker_dir,
            models_dir,
            client: AiHttpClient::new()?,
            inner: Mutex::new(AiSupervisorInner {
                child: None,
                status,
                restart_times: VecDeque::new(),
                start_failures: 0,
                start_cooldown_until: None,
            }),
            start_lock: Mutex::new(()),
        })
    }

    pub async fn start(&self) -> AppResult<AiWorkerStatus> {
        // 快速路径：已就绪直接返回，不去争 start_lock。
        {
            let mut inner = self.inner.lock().await;
            refresh_restart_window(&mut inner);
            if inner.child.is_some() && inner.status.status == "ready" {
                return Ok(inner.status.clone());
            }
        }

        // 串行化真正的启动：N 个并发调用里只有一个会 spawn，其余在此排队；拿到锁后复检——
        // 前一个已经把 worker 拉起来了就直接返回，绝不重复 spawn（修掉 thundering herd）。
        let _start_guard = self.start_lock.lock().await;
        {
            let mut inner = self.inner.lock().await;
            if inner.child.is_some() && inner.status.status == "ready" {
                return Ok(inner.status.clone());
            }
            inner.status.status = "starting".to_string();
            inner.status.last_error = None;
        }

        match self.spawn_worker().await {
            Ok(started) => {
                let mut inner = self.inner.lock().await;
                inner.status.status = "ready".to_string();
                inner.status.port = Some(started.port);
                inner.status.pid = started.pid.or(started.health.pid);
                inner.status.device = started.health.device;
                inner.status.compute = started.health.compute;
                inner.status.last_error = None;
                inner.child = Some(started.child);
                inner.start_failures = 0;
                inner.start_cooldown_until = None;
                Ok(inner.status.clone())
            }
            Err(error) => {
                let mut inner = self.inner.lock().await;
                inner.status.status = "degraded".to_string();
                inner.status.last_error = Some(error.to_string());
                Err(error)
            }
        }
    }

    /// 后台流水线专用：带退避地确保 worker 就绪，返回 true 表示可以投递任务。
    /// 在退避窗口内不重复尝试启动——这样 worker 起不来时（如没装 Python 环境）
    /// 不会每 2 秒投递一批必然失败的任务、把日志刷满 "queue task failed"。
    pub async fn ensure_ready(&self) -> bool {
        {
            let inner = self.inner.lock().await;
            if inner.child.is_some() && inner.status.status == "ready" {
                return true;
            }
            if let Some(until) = inner.start_cooldown_until {
                if Instant::now() < until {
                    return false;
                }
            }
        }
        // start() 成功会清零失败计数与退避窗口（见上）。
        if self.start().await.is_ok() {
            return true;
        }
        let mut inner = self.inner.lock().await;
        apply_start_cooldown(&mut inner);
        false
    }

    pub async fn stop(&self) -> AppResult<AiWorkerStatus> {
        self.stop_inner(true).await
    }

    pub async fn status(&self) -> AppResult<AiWorkerStatus> {
        let mut inner = self.inner.lock().await;
        if let Some(child) = inner.child.as_mut() {
            if let Some(exit_status) = child.try_wait()? {
                inner.child = None;
                inner.status.status = "stopped".to_string();
                inner.status.pid = None;
                inner.status.port = None;
                inner.status.last_error = Some(format!("AI worker exited: {exit_status}"));
            }
        }
        refresh_restart_window(&mut inner);
        Ok(inner.status.clone())
    }

    pub async fn diagnostics(&self) -> AppResult<serde_json::Value> {
        let port = self.ready_port().await?;
        self.client.diagnostics(port).await
    }

    pub async fn models_status(&self) -> AppResult<serde_json::Value> {
        let port = self.ready_port().await?;
        self.client.models_status(port).await
    }

    pub async fn model_download(&self, model_key: &str) -> AppResult<serde_json::Value> {
        let port = self.ready_port().await?;
        self.client.model_download(port, model_key).await
    }

    pub async fn embed_images(&self, items: Vec<ClipEmbedItem>) -> AppResult<ClipEmbedResponse> {
        let port = self.ready_port().await?;
        self.client.embed_images(port, items).await
    }

    pub async fn encode_text(&self, text: String) -> AppResult<TextEncodeResponse> {
        let port = self.ready_port().await?;
        self.client.encode_text(port, text).await
    }

    pub async fn tag_images(&self, items: Vec<TaggerItem>) -> AppResult<TaggerResponse> {
        let port = self.ready_port().await?;
        self.client.tag_images(port, items).await
    }

    pub fn spawn_monitor(self: std::sync::Arc<Self>, app: tauri::AppHandle<tauri::Wry>) {
        tauri::async_runtime::spawn(async move {
            let mut interval = tokio::time::interval(HEALTH_INTERVAL);
            loop {
                interval.tick().await;
                match self.health_tick().await {
                    Ok(Some(status)) => {
                        let _ = app.emit("ai:worker_status", status);
                    }
                    Ok(None) => {}
                    Err(error) => {
                        tracing::warn!(%error, "AI worker monitor tick failed");
                    }
                }
            }
        });
    }

    async fn health_tick(&self) -> AppResult<Option<AiWorkerStatus>> {
        let status = self.status().await?;
        if status.status != "ready" {
            return Ok(None);
        }
        let Some(port) = status.port else {
            return Ok(None);
        };

        match tokio::time::timeout(Duration::from_secs(2), self.client.health(port)).await {
            Ok(Ok(health)) => {
                let mut inner = self.inner.lock().await;
                inner.status.device = health.device;
                inner.status.compute = health.compute;
                inner.status.last_error = None;
                Ok(Some(inner.status.clone()))
            }
            Ok(Err(error)) => self
                .restart_after_failure(error.to_string())
                .await
                .map(Some),
            Err(_) => self
                .restart_after_failure("AI worker health check timed out".to_string())
                .await
                .map(Some),
        }
    }

    async fn ready_port(&self) -> AppResult<u16> {
        let status = self.start().await?;
        status
            .port
            .ok_or_else(|| AppError::Other("AI worker 未返回端口".to_string()))
    }

    async fn restart_after_failure(&self, reason: String) -> AppResult<AiWorkerStatus> {
        // 与 start() 共用启动锁：避免监控线程的重启和后台任务的 start() 同时 spawn。
        let _start_guard = self.start_lock.lock().await;
        // 拿到锁后复检：等待期间可能已被其它路径重启成功，这次失败已过期，直接返回。
        {
            let inner = self.inner.lock().await;
            if inner.child.is_some() && inner.status.status == "ready" {
                return Ok(inner.status.clone());
            }
        }

        let _ = self.stop_inner(false).await;

        {
            let mut inner = self.inner.lock().await;
            refresh_restart_window(&mut inner);
            if inner.restart_times.len() >= HEALTH_RESTART_LIMIT {
                inner.status.status = "degraded".to_string();
                inner.status.last_error = Some(reason);
                return Ok(inner.status.clone());
            }
            inner.restart_times.push_back(Instant::now());
            inner.status.restarts_last_minute = inner.restart_times.len();
            inner.status.status = "starting".to_string();
            inner.status.last_error = Some(reason);
        }

        match self.spawn_worker().await {
            Ok(started) => {
                let mut inner = self.inner.lock().await;
                inner.status.status = "ready".to_string();
                inner.status.port = Some(started.port);
                inner.status.pid = started.pid.or(started.health.pid);
                inner.status.device = started.health.device;
                inner.status.compute = started.health.compute;
                inner.child = Some(started.child);
                Ok(inner.status.clone())
            }
            Err(error) => {
                let mut inner = self.inner.lock().await;
                inner.status.status = "degraded".to_string();
                inner.status.last_error = Some(error.to_string());
                Ok(inner.status.clone())
            }
        }
    }

    async fn stop_inner(&self, graceful: bool) -> AppResult<AiWorkerStatus> {
        let (mut child, port) = {
            let mut inner = self.inner.lock().await;
            inner.status.status = "stopping".to_string();
            (inner.child.take(), inner.status.port)
        };

        if graceful {
            if let Some(port) = port {
                let _ =
                    tokio::time::timeout(Duration::from_secs(1), self.client.shutdown(port)).await;
            }
        }

        if let Some(child) = child.as_mut() {
            match tokio::time::timeout(Duration::from_secs(1), child.wait()).await {
                Ok(Ok(_)) => {}
                _ => {
                    let _ = child.start_kill();
                    let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
                }
            }
        }

        let mut inner = self.inner.lock().await;
        inner.status.status = "stopped".to_string();
        inner.status.pid = None;
        inner.status.port = None;
        Ok(inner.status.clone())
    }

    async fn spawn_worker(&self) -> AppResult<StartedWorker> {
        if !self.worker_dir.exists() {
            return Err(AppError::Other(format!(
                "AI worker 目录不存在：{}",
                self.worker_dir.display()
            )));
        }

        let candidates = start_candidates(&self.worker_dir);
        let mut spawn_errors = Vec::new();
        for candidate in candidates {
            match self.spawn_candidate(&candidate).await {
                Ok(started) => return Ok(started),
                Err(error) => {
                    tracing::warn!(candidate = candidate.label, %error, "AI worker start candidate failed");
                    spawn_errors.push(format!("{}: {error}", candidate.label));
                }
            }
        }

        Err(AppError::Other(format!(
            "无法启动 AI worker：{}",
            spawn_errors.join("; ")
        )))
    }

    async fn spawn_candidate(&self, candidate: &StartCandidate) -> AppResult<StartedWorker> {
        let mut command = Command::new(&candidate.program);
        command
            .args(&candidate.args)
            .current_dir(&self.worker_dir)
            .env("PVP_MODEL_DIR", &self.models_dir)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        // 限制 worker 的 CPU 数学库线程：torch/numpy 默认按核数开 intra-op 线程，叠加缩略图
        // 解码会把 CPU 打满、整机卡顿。这些环境变量必须在进程启动前设好才生效。
        let cpu_threads = ai_worker_cpu_threads().to_string();
        command
            .env("OMP_NUM_THREADS", &cpu_threads)
            .env("MKL_NUM_THREADS", &cpu_threads)
            .env("OPENBLAS_NUM_THREADS", &cpu_threads)
            .env("NUMEXPR_NUM_THREADS", &cpu_threads);

        // Windows：整个 Python 进程以「低于正常」优先级运行，即使 CPU 跑满也让位给前台 UI。
        #[cfg(windows)]
        {
            const BELOW_NORMAL_PRIORITY_CLASS: u32 = 0x0000_4000;
            command.creation_flags(BELOW_NORMAL_PRIORITY_CLASS);
        }

        let mut child = command.spawn()?;
        let pid = child.id();
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| AppError::Other("AI worker stdout unavailable".to_string()))?;
        if let Some(stderr) = child.stderr.take() {
            spawn_drain_reader(stderr, "stderr");
        }

        let mut lines = BufReader::new(stdout).lines();
        let port = tokio::time::timeout(START_TIMEOUT, async {
            loop {
                match lines.next_line().await? {
                    Some(line) => {
                        tracing::info!(line, "AI worker stdout");
                        if let Some(port) = parse_port_line(&line) {
                            return Ok(port);
                        }
                    }
                    None => {
                        return Err(AppError::Other(
                            "AI worker exited before printing a port".to_string(),
                        ))
                    }
                }
            }
        })
        .await
        .map_err(|_| AppError::Other("AI worker 启动超时".to_string()))??;
        spawn_drain_lines(lines, "stdout");

        let health = self.client.health(port).await?;
        Ok(StartedWorker {
            child,
            port,
            pid,
            health,
        })
    }
}

/// 启动失败后设置指数退避窗口（5s → 15s → 30s → 60s 封顶），供 `ensure_ready` 使用。
fn apply_start_cooldown(inner: &mut AiSupervisorInner) {
    inner.start_failures = inner.start_failures.saturating_add(1);
    let secs = match inner.start_failures {
        1 => 5,
        2 => 15,
        3 => 30,
        _ => 60,
    };
    inner.start_cooldown_until = Some(Instant::now() + Duration::from_secs(secs));
}

fn refresh_restart_window(inner: &mut AiSupervisorInner) {
    let now = Instant::now();
    while inner
        .restart_times
        .front()
        .is_some_and(|instant| now.duration_since(*instant) > RESTART_WINDOW)
    {
        inner.restart_times.pop_front();
    }
    inner.status.restarts_last_minute = inner.restart_times.len();
}

fn start_candidates(worker_dir: &Path) -> Vec<StartCandidate> {
    let mut candidates = Vec::new();
    if let Ok(python) = std::env::var("PVP_AI_PYTHON") {
        candidates.push(python_candidate(PathBuf::from(python), "PVP_AI_PYTHON"));
    }

    let windows_venv = worker_dir.join(".venv").join("Scripts").join("python.exe");
    if windows_venv.exists() {
        candidates.push(python_candidate(windows_venv, ".venv\\Scripts\\python.exe"));
    }
    let unix_venv = worker_dir.join(".venv").join("bin").join("python");
    if unix_venv.exists() {
        candidates.push(python_candidate(unix_venv, ".venv/bin/python"));
    }

    candidates.push(StartCandidate {
        program: PathBuf::from("uv"),
        args: vec![
            "run".to_string(),
            "python".to_string(),
            "-m".to_string(),
            "src.main".to_string(),
            "--host".to_string(),
            "127.0.0.1".to_string(),
            "--port".to_string(),
            "0".to_string(),
        ],
        label: "uv run python".to_string(),
    });
    candidates.push(python_candidate(PathBuf::from("python"), "python"));
    candidates
}

fn python_candidate(program: PathBuf, label: &str) -> StartCandidate {
    StartCandidate {
        program,
        args: vec![
            "-m".to_string(),
            "src.main".to_string(),
            "--host".to_string(),
            "127.0.0.1".to_string(),
            "--port".to_string(),
            "0".to_string(),
        ],
        label: label.to_string(),
    }
}

/// AI worker 的 CPU 数学库线程数上限：逻辑核数的一半，夹在 [1, 4]。
/// 目的不是榨满 CPU，而是给前台 UI 和缩略图解码留余量；CUDA 推理时 CPU 只做预处理，
/// 4 线程足够，CPU 回退时也不会把整机拖死。
fn ai_worker_cpu_threads() -> usize {
    std::thread::available_parallelism()
        .map(|n| (n.get() / 2).clamp(1, 4))
        .unwrap_or(2)
}

fn parse_port_line(line: &str) -> Option<u16> {
    line.trim()
        .strip_prefix("PVP_AI_WORKER_PORT=")?
        .parse::<u16>()
        .ok()
}

fn spawn_drain_reader<R>(reader: R, stream: &'static str)
where
    R: AsyncRead + Unpin + Send + 'static,
{
    let lines = BufReader::new(reader).lines();
    spawn_drain_lines(lines, stream);
}

fn spawn_drain_lines<R>(mut lines: Lines<R>, stream: &'static str)
where
    R: AsyncBufRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        while let Ok(Some(line)) = lines.next_line().await {
            // 用 info 级别：worker 的 stdout/stderr（含 [tagger] 加载诊断、模型报错）才能进默认日志，
            // 否则被 "warn,photo_view_plus_lib=info" 过滤掉、排查 AI 问题时啥也看不到。
            tracing::info!(stream, line, "AI worker output");
        }
    });
}
