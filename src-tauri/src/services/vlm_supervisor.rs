//! llama-server 监管（docs/10 A6、§12.4）：按需启动 Qwen3.5-9B 视觉模型服务、健康检查、重启上限、
//! 按负载停止。
//!
//! - 端口上已经有健康的服务（例如 memora 的计划任务正在用）：直接复用，**不负责关闭**。
//! - 自己启动的：隐藏窗口、低于正常优先级、绑进作业对象（主进程崩溃时连带结束，不残留 7GB 显存）；
//!   关闭前先确认 `/slots` 全部空闲（别的程序可能在复用这个服务）。
//! - 启动前检查剩余显存 ≥ 8.5GB，不够就拒绝启动并说明原因。
//! - 加载模型时 `/health` 返回 503，最多等 300 秒；1 分钟内最多启动 3 次，超过进入 degraded。
//!
//! 启动命令和显存探测都可以注入，测试里用假进程和假 HTTP 服务。

use std::collections::VecDeque;
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use serde::Serialize;
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use tokio::time::Instant;

use crate::error::{AppError, AppResult};

/// 与 memora 共用的端口（§12.4）。
pub const DEFAULT_PORT: u16 = 8081;
/// 每个并发槽位的上下文长度（`-c` = 它 × 槽位数，同 memora）。
pub const CTX_PER_SLOT: u32 = 6144;
/// 启动前要求的剩余显存（MiB）：模型约 7.2GB，留出余量。
pub const MIN_FREE_VRAM_MB: u64 = 8704;
pub const HEALTH_TIMEOUT: Duration = Duration::from_secs(300);
const START_LIMIT: usize = 3;
const START_WINDOW: Duration = Duration::from_secs(60);

/// llama-server 程序与模型文件。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VlmPaths {
    pub exe: PathBuf,
    pub model: PathBuf,
    pub mmproj: PathBuf,
}

impl VlmPaths {
    /// 第一个不存在的文件（给界面提示用）。
    pub fn missing(&self) -> Option<String> {
        [
            ("llama-server", &self.exe),
            ("模型", &self.model),
            ("视觉投影", &self.mmproj),
        ]
        .into_iter()
        .find(|(_, path)| !path.is_file())
        .map(|(label, path)| format!("找不到{label}：{}", path.display()))
    }

    /// 某个目录布局下的三件套（memora 的目录结构：`runtime\llama\Library\bin`、`models\qwen3.5-9b`）。
    pub fn under(root: &Path) -> Self {
        Self {
            exe: root
                .join("runtime")
                .join("llama")
                .join("Library")
                .join("bin")
                .join("llama-server.exe"),
            model: root
                .join("models")
                .join("qwen3.5-9b")
                .join("Qwen3.5-9B-UD-Q4_K_XL.gguf"),
            mmproj: root
                .join("models")
                .join("qwen3.5-9b")
                .join("mmproj-F16.gguf"),
        }
    }
}

/// 怎么启动 llama-server（测试里换成假进程）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchSpec {
    pub program: PathBuf,
    pub args: Vec<String>,
}

impl LaunchSpec {
    /// 同 memora `llama.command`：`-ngl 99 -fa on --reasoning off`，只监听本机。
    pub fn llama(paths: &VlmPaths, parallel: u8, port: u16) -> Self {
        let parallel = u32::from(parallel.max(1));
        let args = [
            "-m".to_string(),
            paths.model.to_string_lossy().to_string(),
            "--mmproj".to_string(),
            paths.mmproj.to_string_lossy().to_string(),
            "-c".to_string(),
            (CTX_PER_SLOT * parallel).to_string(),
            "-np".to_string(),
            parallel.to_string(),
            "-ngl".to_string(),
            "99".to_string(),
            "-fa".to_string(),
            "on".to_string(),
            "--reasoning".to_string(),
            "off".to_string(),
            "--host".to_string(),
            "127.0.0.1".to_string(),
            "--port".to_string(),
            port.to_string(),
        ];
        Self {
            program: paths.exe.clone(),
            args: args.to_vec(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VlmServerStatus {
    /// stopped | starting | ready | degraded
    pub state: String,
    /// 是否由本应用启动（复用外部服务时为 false，不负责关闭）。
    pub owned: bool,
    pub pid: Option<u32>,
    pub port: u16,
    pub last_error: Option<String>,
    pub starts_last_minute: usize,
    /// 最近一次启动加载模型用了多少秒。
    pub load_secs: Option<u64>,
}

/// 启动失败的原因。
#[derive(Debug, Clone, PartialEq)]
pub enum VlmStartError {
    /// 剩余显存不够（MiB）。
    Vram { free_mb: u64 },
    /// 1 分钟内启动次数用完。
    RestartLimit,
    /// 加载途中被强制停止（进入全屏、用户手动卸载）打断。
    Aborted,
    /// 进程退出、超时、找不到程序等。
    Failed(String),
}

impl std::fmt::Display for VlmStartError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Vram { free_mb } => write!(
                f,
                "剩余显存 {:.1}GB，大模型需要至少 {:.1}GB，暂不启动",
                *free_mb as f64 / 1024.0,
                MIN_FREE_VRAM_MB as f64 / 1024.0
            ),
            Self::RestartLimit => write!(f, "1 分钟内已启动 {START_LIMIT} 次，暂停启动大模型服务"),
            Self::Aborted => f.write_str("大模型服务的启动被取消"),
            Self::Failed(reason) => f.write_str(reason),
        }
    }
}

/// 剩余显存探测（MiB）；拿不到时返回 `None`（不阻止启动）。会在 blocking 线程里调用。
pub type VramProbe = Arc<dyn Fn() -> Option<u64> + Send + Sync>;

struct Inner {
    child: Option<Child>,
    status: VlmServerStatus,
    starts: VecDeque<Instant>,
}

pub struct VlmSupervisor {
    base_url: String,
    client: reqwest::Client,
    log_dir: Option<PathBuf>,
    vram_probe: VramProbe,
    inner: Mutex<Inner>,
    /// 串行化启动：并发的 ensure_running 只有一个真正去拉进程。
    start_lock: Mutex<()>,
    /// 强制停止时置位：正在加载的 ensure_running 看到后立即结束进程退出，不用等满 300 秒。
    abort_start: AtomicBool,
    last_used: StdMutex<Instant>,
}

impl VlmSupervisor {
    pub fn new(port: u16, log_dir: Option<PathBuf>) -> AppResult<Self> {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(2))
            .timeout(Duration::from_secs(5))
            .build()?;
        Ok(Self {
            base_url: format!("http://127.0.0.1:{port}"),
            client,
            log_dir,
            vram_probe: Arc::new(nvidia_smi_free_mb),
            inner: Mutex::new(Inner {
                child: None,
                status: VlmServerStatus {
                    state: "stopped".to_string(),
                    owned: false,
                    pid: None,
                    port,
                    last_error: None,
                    starts_last_minute: 0,
                    load_secs: None,
                },
                starts: VecDeque::new(),
            }),
            start_lock: Mutex::new(()),
            abort_start: AtomicBool::new(false),
            last_used: StdMutex::new(Instant::now()),
        })
    }

    pub fn with_vram_probe(mut self, probe: VramProbe) -> Self {
        self.vram_probe = probe;
        self
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// 记一次使用（有分析任务在跑）。空闲停止按它计时。
    pub fn touch(&self) {
        if let Ok(mut last_used) = self.last_used.lock() {
            *last_used = Instant::now();
        }
    }

    pub fn idle_for(&self) -> Duration {
        self.last_used
            .lock()
            .map(|last_used| last_used.elapsed())
            .unwrap_or_default()
    }

    /// 当前状态；顺带回收已退出的子进程。
    pub async fn status(&self) -> VlmServerStatus {
        let mut inner = self.inner.lock().await;
        reap(&mut inner);
        refresh_window(&mut inner);
        inner.status.clone()
    }

    pub async fn is_ready(&self) -> bool {
        self.status().await.state == "ready"
    }

    /// `/health` 返回 200（加载中是 503）。
    pub async fn healthy(&self) -> bool {
        match self
            .client
            .get(format!("{}/health", self.base_url))
            .send()
            .await
        {
            Ok(response) => response.status().is_success(),
            Err(_) => false,
        }
    }

    /// 请求失败后检查服务：自己启动的进程已退出 → stopped（下次重新拉起，计入启动次数）；
    /// 复用的外部服务不再响应 → stopped（下次重新探测）。自己的进程还活着但没响应时不动它（可能正忙）。
    pub async fn check_health(&self) -> VlmServerStatus {
        let status = self.status().await;
        if status.state != "ready" || status.owned {
            return status;
        }
        if self.healthy().await {
            return status;
        }
        let mut inner = self.inner.lock().await;
        if inner.status.state == "ready" && inner.child.is_none() {
            inner.status.state = "stopped".to_string();
            inner.status.last_error = Some("大模型服务没有响应".to_string());
        }
        inner.status.clone()
    }

    /// 确保服务可用：已有健康服务就复用，否则按 `spec` 启动并等它加载完。
    pub async fn ensure_running(
        &self,
        spec: &LaunchSpec,
    ) -> Result<VlmServerStatus, VlmStartError> {
        self.touch();
        let _start = self.start_lock.lock().await;
        if self.abort_start.load(Ordering::SeqCst) {
            return Err(VlmStartError::Aborted);
        }
        {
            let mut inner = self.inner.lock().await;
            reap(&mut inner);
            if inner.status.state == "ready" {
                return Ok(inner.status.clone());
            }
        }
        if self.healthy().await {
            // 别人（memora）开着的服务，或者我们自己的进程刚加载完。
            let mut inner = self.inner.lock().await;
            inner.status.owned = inner.child.is_some();
            inner.status.state = "ready".to_string();
            inner.status.last_error = None;
            return Ok(inner.status.clone());
        }
        {
            let mut inner = self.inner.lock().await;
            refresh_window(&mut inner);
            if inner.starts.len() >= START_LIMIT {
                inner.status.state = "degraded".to_string();
                inner.status.last_error = Some(VlmStartError::RestartLimit.to_string());
                return Err(VlmStartError::RestartLimit);
            }
        }
        let probe = self.vram_probe.clone();
        let free_mb = tokio::task::spawn_blocking(move || probe())
            .await
            .ok()
            .flatten();
        if let Some(free_mb) = free_mb {
            if free_mb < MIN_FREE_VRAM_MB {
                let error = VlmStartError::Vram { free_mb };
                let mut inner = self.inner.lock().await;
                inner.status.state = "stopped".to_string();
                inner.status.last_error = Some(error.to_string());
                return Err(error);
            }
        }

        let started = Instant::now();
        let mut child = match self.spawn(spec) {
            Ok(child) => child,
            Err(error) => {
                let error = VlmStartError::Failed(format!("无法启动 llama-server：{error}"));
                let mut inner = self.inner.lock().await;
                inner.status.state = "stopped".to_string();
                inner.status.last_error = Some(error.to_string());
                return Err(error);
            }
        };
        let pid = child.id();
        {
            let mut inner = self.inner.lock().await;
            inner.starts.push_back(started);
            refresh_window(&mut inner);
            inner.status.state = "starting".to_string();
            inner.status.pid = pid;
            inner.status.owned = true;
            inner.status.last_error = None;
        }

        let outcome = self.wait_healthy(&mut child, HEALTH_TIMEOUT).await;
        let mut inner = self.inner.lock().await;
        match outcome {
            Ok(()) => {
                inner.child = Some(child);
                inner.status.state = "ready".to_string();
                inner.status.load_secs = Some(started.elapsed().as_secs());
                tracing::info!(
                    pid,
                    load_secs = started.elapsed().as_secs(),
                    "llama-server ready"
                );
                Ok(inner.status.clone())
            }
            Err(WaitError::Aborted) => {
                let _ = child.start_kill();
                // 被打断的启动不算一次失败的启动。
                inner.starts.pop_back();
                refresh_window(&mut inner);
                inner.status.state = "stopped".to_string();
                inner.status.pid = None;
                inner.status.owned = false;
                tracing::info!("llama-server start aborted");
                Err(VlmStartError::Aborted)
            }
            Err(WaitError::Failed(reason)) => {
                let _ = child.start_kill();
                inner.status.state = if inner.starts.len() >= START_LIMIT {
                    "degraded".to_string()
                } else {
                    "stopped".to_string()
                };
                inner.status.pid = None;
                inner.status.owned = false;
                inner.status.last_error = Some(reason.clone());
                tracing::warn!(%reason, "llama-server failed to start");
                Err(VlmStartError::Failed(reason))
            }
        }
    }

    fn spawn(&self, spec: &LaunchSpec) -> std::io::Result<Child> {
        let mut command = Command::new(&spec.program);
        command
            .args(&spec.args)
            .stdin(Stdio::null())
            .kill_on_drop(true);
        // 在程序所在目录运行（llama-server 旁边的 DLL）；只给了程序名时沿用当前目录。
        if let Some(parent) = spec
            .program
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            command.current_dir(parent);
        }
        match self.open_log() {
            Some((out, err)) => {
                command.stdout(out).stderr(err);
            }
            None => {
                command.stdout(Stdio::null()).stderr(Stdio::null());
            }
        }
        #[cfg(windows)]
        {
            const BELOW_NORMAL_PRIORITY_CLASS: u32 = 0x0000_4000;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            command.creation_flags(BELOW_NORMAL_PRIORITY_CLASS | CREATE_NO_WINDOW);
        }
        let child = command.spawn()?;
        if !crate::utils::job_object::bind_child(&child) {
            tracing::debug!("llama-server not bound to job object");
        }
        Ok(child)
    }

    /// 输出追加到 `llama-YYYYMMDD.log`（同 memora），便于排查加载失败。
    fn open_log(&self) -> Option<(Stdio, Stdio)> {
        let dir = self.log_dir.as_ref()?;
        std::fs::create_dir_all(dir).ok()?;
        let name = format!("llama-{}.log", chrono::Local::now().format("%Y%m%d"));
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join(name))
            .ok()?;
        let err = file.try_clone().ok()?;
        Some((Stdio::from(file), Stdio::from(err)))
    }

    async fn wait_healthy(&self, child: &mut Child, timeout: Duration) -> Result<(), WaitError> {
        let started = Instant::now();
        loop {
            if self.abort_start.load(Ordering::SeqCst) {
                return Err(WaitError::Aborted);
            }
            if self.healthy().await {
                return Ok(());
            }
            match child.try_wait() {
                Ok(Some(status)) => {
                    return Err(WaitError::Failed(format!(
                        "llama-server 启动失败（{status}），详见日志 llama-*.log"
                    )))
                }
                Ok(None) => {}
                Err(error) => {
                    return Err(WaitError::Failed(format!("llama-server 状态未知：{error}")))
                }
            }
            if started.elapsed() >= timeout {
                return Err(WaitError::Failed(format!(
                    "llama-server {} 秒内没有就绪",
                    timeout.as_secs()
                )));
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    /// 所有槽位都空闲（拿不到 `/slots` 时按空闲处理）。
    pub async fn slots_idle(&self) -> bool {
        let response = match self
            .client
            .get(format!("{}/slots", self.base_url))
            .send()
            .await
        {
            Ok(response) if response.status().is_success() => response,
            _ => return true,
        };
        let Ok(slots) = response.json::<serde_json::Value>().await else {
            return true;
        };
        slots
            .as_array()
            .map(|slots| slots.iter().all(slot_is_idle))
            .unwrap_or(true)
    }

    /// 停掉自己启动的服务。`force = false` 时，有槽位还在处理就不停（返回 false）。
    /// 复用的外部服务从不关闭。`force = true` 还会打断正在进行的加载（不用等它加载完）。
    pub async fn stop(&self, force: bool) -> bool {
        if force {
            self.abort_start.store(true, Ordering::SeqCst);
        }
        let _start = self.start_lock.lock().await;
        // 拿到锁时，进行中的加载（如果有）已经被打断退出；之后的启动不受影响。
        self.abort_start.store(false, Ordering::SeqCst);
        {
            let mut inner = self.inner.lock().await;
            reap(&mut inner);
            if inner.child.is_none() {
                if !inner.status.owned && inner.status.state == "ready" {
                    // 外部服务：只是不再使用，标成 stopped，下次用时重新探测。
                    inner.status.state = "stopped".to_string();
                }
                return false;
            }
        }
        if !force && !self.slots_idle().await {
            return false;
        }
        let child = {
            let mut inner = self.inner.lock().await;
            inner.child.take()
        };
        if let Some(mut child) = child {
            let _ = child.start_kill();
            let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
        }
        let mut inner = self.inner.lock().await;
        inner.status.state = "stopped".to_string();
        inner.status.pid = None;
        inner.status.owned = false;
        tracing::info!(force, "llama-server stopped, VRAM released");
        true
    }

    /// 空闲超过 `after` 就停掉自己启动的服务（槽位全空闲时）。
    pub async fn stop_if_idle(&self, after: Duration) -> bool {
        let owned = {
            let inner = self.inner.lock().await;
            inner.child.is_some()
        };
        if !owned || self.idle_for() < after {
            return false;
        }
        self.stop(false).await
    }
}

enum WaitError {
    Aborted,
    Failed(String),
}

fn slot_is_idle(slot: &serde_json::Value) -> bool {
    if let Some(processing) = slot.get("is_processing").and_then(|value| value.as_bool()) {
        return !processing;
    }
    // 旧版 llama-server：state 0 = 空闲。
    slot.get("state")
        .and_then(|value| value.as_i64())
        .map(|state| state == 0)
        .unwrap_or(true)
}

fn reap(inner: &mut Inner) {
    let exited = match inner.child.as_mut() {
        Some(child) => match child.try_wait() {
            Ok(Some(status)) => Some(status.to_string()),
            Ok(None) => None,
            Err(error) => Some(error.to_string()),
        },
        None => None,
    };
    if let Some(reason) = exited {
        inner.child = None;
        inner.status.state = "stopped".to_string();
        inner.status.pid = None;
        inner.status.owned = false;
        inner.status.last_error = Some(format!("llama-server 已退出（{reason}）"));
    }
}

fn refresh_window(inner: &mut Inner) {
    let now = Instant::now();
    while inner
        .starts
        .front()
        .is_some_and(|at| now.duration_since(*at) > START_WINDOW)
    {
        inner.starts.pop_front();
    }
    inner.status.starts_last_minute = inner.starts.len();
    if inner.status.state == "degraded" && inner.starts.len() < START_LIMIT {
        inner.status.state = "stopped".to_string();
    }
}

/// 用 `nvidia-smi` 读第一块显卡的剩余显存（MiB）。阻塞约 100ms，调用方在 blocking 线程里调。
pub fn nvidia_smi_free_mb() -> Option<u64> {
    let mut command = std::process::Command::new("nvidia-smi");
    command.args(["--query-gpu=memory.free", "--format=csv,noheader,nounits"]);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let output = command.output().ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .next()?
        .trim()
        .parse()
        .ok()
}

/// 错误转成 AppError（命令层用）。
impl From<VlmStartError> for AppError {
    fn from(error: VlmStartError) -> Self {
        AppError::Other(error.to_string())
    }
}
