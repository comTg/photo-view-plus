//! 资源治理器（docs/11 §5，R2）：按"用户是否正在用电脑"动态调节后台并发和 GPU 占用。
//!
//! - **负载模式**：最近 20 秒内有键盘鼠标输入 = 使用中；20 秒到 3 分钟 = 短暂离开；3 分钟以上
//!   或锁屏 = 空闲；全屏 / 游戏 / 演示 = 全屏档，压过其他所有模式。
//! - **切换规则**：每 500ms 采样一次；降档（变得更保守）立即生效；升档要求条件持续 10 秒，避免抖动。
//! - **手动覆盖**："全速"忽略使用中和短暂离开（仍服从全屏档）；"暂停"一小时或到重启。
//! - **预算**（§5.2）：每个档位对应一组并发上限，由调用方提供的回调下发到缩略图、读闸门、调度器；
//!   torch worker 的 GPU 任务统一经过 [`GpuGate`]（暂停 / 占空比 / 不限）；VLM 只在允许的档位投递。
//!
//! 状态机和预算表是纯逻辑，时间由调用方传入，测试里用假时钟和假输入源。

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::sync::{watch, Notify};
use tokio_util::sync::CancellationToken;

use crate::config::AppSettings;
use crate::utils::win_activity::{ActivitySnapshot, ActivitySource, PresenceState};

/// 观测到的负载模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum LoadMode {
    /// 使用中：用户在用电脑（不一定在用本应用）。
    Active,
    /// 短暂离开。
    Away,
    /// 空闲：可以全速。
    Idle,
    /// 全屏 / 游戏 / 演示：让出全部资源。
    Busy,
}

impl LoadMode {
    /// 越大越保守。降档 = 换到更保守的模式。
    fn restrictiveness(self) -> u8 {
        match self {
            Self::Idle => 0,
            Self::Away => 1,
            Self::Active => 2,
            Self::Busy => 3,
        }
    }
}

/// 手动覆盖。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum OverrideMode {
    Auto,
    FullSpeed,
    Paused,
}

/// 实际生效的档位（模式叠加手动覆盖之后）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Tier {
    Active,
    Away,
    Idle,
    Busy,
    Paused,
}

/// 模式判定阈值。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModeThresholds {
    pub away_after: Duration,
    pub idle_after: Duration,
    pub upgrade_delay: Duration,
}

impl Default for ModeThresholds {
    fn default() -> Self {
        Self {
            away_after: Duration::from_secs(20),
            idle_after: Duration::from_secs(180),
            upgrade_delay: Duration::from_secs(10),
        }
    }
}

/// 一次采样 → 负载模式。
pub fn classify(snapshot: ActivitySnapshot, thresholds: &ModeThresholds) -> LoadMode {
    match snapshot.presence {
        PresenceState::Busy => LoadMode::Busy,
        PresenceState::NotPresent => LoadMode::Idle,
        PresenceState::Normal if snapshot.idle_for >= thresholds.idle_after => LoadMode::Idle,
        PresenceState::Normal if snapshot.idle_for >= thresholds.away_after => LoadMode::Away,
        PresenceState::Normal => LoadMode::Active,
    }
}

/// 升降档状态机：降档立即生效，升档要求候选模式持续 `upgrade_delay`。
#[derive(Debug, Clone)]
pub struct ModeMachine {
    current: LoadMode,
    candidate: Option<(LoadMode, Instant)>,
}

impl ModeMachine {
    /// 启动时按最保守的"使用中"开始，避免开机就全速抢资源。
    pub fn new(initial: LoadMode) -> Self {
        Self {
            current: initial,
            candidate: None,
        }
    }

    pub fn current(&self) -> LoadMode {
        self.current
    }

    /// 喂一个观测值，返回模式是否变化。
    pub fn observe(&mut self, observed: LoadMode, now: Instant, upgrade_delay: Duration) -> bool {
        if observed == self.current {
            self.candidate = None;
            return false;
        }
        if observed.restrictiveness() > self.current.restrictiveness() {
            self.current = observed;
            self.candidate = None;
            return true;
        }
        match self.candidate {
            Some((mode, since)) if mode == observed => {
                if now.saturating_duration_since(since) >= upgrade_delay {
                    self.current = observed;
                    self.candidate = None;
                    true
                } else {
                    false
                }
            }
            _ => {
                self.candidate = Some((observed, now));
                false
            }
        }
    }
}

/// torch worker 的 GPU 任务（向量、标签、OCR、人脸）怎么跑。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum GpuPolicy {
    /// 暂停：只响应搜索编码（搜索不经过 GpuGate）。
    Paused,
    /// 占空比：每跑完一批，按 `本批耗时 × (1/占空比 − 1)` 冷却后再跑下一批。
    DutyCycle(f32),
    Unlimited,
}

/// 一个档位的资源预算（docs/11 §5.2）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Budget {
    /// 可见区域缩略图并发（R3，独立队列，不占批量名额）。
    pub visible_thumbnails: usize,
    /// 批量缩略图并发（0 = 全屏 / 暂停时批量生成全部等待）。
    pub batch_thumbnails: usize,
    /// 网络盘读原图并发（红线：≤ 4）。
    pub network_reads: usize,
    /// BLAKE3 / dHash 并发（`usize::MAX` = 不限，只受读闸门约束）。
    pub hash_tasks: usize,
    pub gpu: GpuPolicy,
    /// 是否允许投递 VLM 分析（docs/10 A6）。
    pub vlm: bool,
    /// worker 的数学库线程数。
    pub worker_threads: usize,
}

/// 影响预算的设置项。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BudgetSettings {
    /// "使用电脑时也运行 AI"：使用中档的 GPU 任务按 30% 占空比跑，而不是暂停。
    pub ai_while_active: bool,
    /// "大模型只在空闲时运行"：关掉后除全屏档外都允许投递 VLM。
    pub vlm_idle_only: bool,
    /// 空闲档的批量缩略图并发（机器的 CPU 上限）。
    pub idle_thumbnails: usize,
}

impl Default for BudgetSettings {
    fn default() -> Self {
        Self {
            ai_while_active: false,
            vlm_idle_only: true,
            idle_thumbnails: 6,
        }
    }
}

pub const DUTY_CYCLE_AWAY: f32 = 0.3;

/// 从应用设置构造预算设置和模式阈值。`idle_thumbnails` 是机器的缩略图 CPU 上限。
pub fn settings_from(
    app: &AppSettings,
    idle_thumbnails: usize,
) -> (BudgetSettings, ModeThresholds) {
    let idle_after = Duration::from_secs(u64::from(app.idle_threshold_minutes.clamp(1, 60)) * 60);
    (
        BudgetSettings {
            ai_while_active: app.ai_while_active,
            vlm_idle_only: app.vlm_idle_only,
            idle_thumbnails: idle_thumbnails.max(1),
        },
        ModeThresholds {
            idle_after,
            ..ModeThresholds::default()
        },
    )
}

/// 状态栏菜单的手动覆盖选项（`governor_set_override` 的参数）。
pub fn parse_override(value: &str) -> Option<(OverrideMode, Option<Duration>)> {
    match value {
        "auto" => Some((OverrideMode::Auto, None)),
        "fullSpeed" => Some((OverrideMode::FullSpeed, None)),
        "pause1h" => Some((OverrideMode::Paused, Some(Duration::from_secs(60 * 60)))),
        "pauseUntilRestart" => Some((OverrideMode::Paused, None)),
        _ => None,
    }
}

pub fn budget_for(tier: Tier, settings: &BudgetSettings) -> Budget {
    match tier {
        Tier::Active => Budget {
            visible_thumbnails: 2,
            batch_thumbnails: 1,
            network_reads: 3,
            hash_tasks: 1,
            gpu: if settings.ai_while_active {
                GpuPolicy::DutyCycle(DUTY_CYCLE_AWAY)
            } else {
                GpuPolicy::Paused
            },
            vlm: !settings.vlm_idle_only,
            worker_threads: 1,
        },
        Tier::Away => Budget {
            visible_thumbnails: 2,
            batch_thumbnails: 3.min(settings.idle_thumbnails.max(1)),
            network_reads: 4,
            hash_tasks: 2,
            gpu: GpuPolicy::DutyCycle(DUTY_CYCLE_AWAY),
            vlm: !settings.vlm_idle_only,
            worker_threads: 2,
        },
        Tier::Idle => Budget {
            visible_thumbnails: 2,
            batch_thumbnails: settings.idle_thumbnails.max(1),
            network_reads: 4,
            hash_tasks: usize::MAX,
            gpu: GpuPolicy::Unlimited,
            vlm: true,
            worker_threads: 4,
        },
        // 全屏 / 游戏：用户不在看画廊，可见区域留 1 个以防万一。
        Tier::Busy => Budget {
            visible_thumbnails: 1,
            batch_thumbnails: 0,
            network_reads: 1,
            hash_tasks: 0,
            gpu: GpuPolicy::Paused,
            vlm: false,
            worker_threads: 1,
        },
        // 手动暂停：只保留浏览所需的可见区域缩略图（§5.1）。
        Tier::Paused => Budget {
            visible_thumbnails: 2,
            batch_thumbnails: 0,
            network_reads: 2,
            hash_tasks: 0,
            gpu: GpuPolicy::Paused,
            vlm: false,
            worker_threads: 1,
        },
    }
}

/// 模式 + 手动覆盖 → 生效档位。
pub fn effective_tier(mode: LoadMode, override_mode: OverrideMode) -> Tier {
    match (override_mode, mode) {
        (OverrideMode::Paused, _) => Tier::Paused,
        (_, LoadMode::Busy) => Tier::Busy,
        (OverrideMode::FullSpeed, _) => Tier::Idle,
        (OverrideMode::Auto, LoadMode::Active) => Tier::Active,
        (OverrideMode::Auto, LoadMode::Away) => Tier::Away,
        (OverrideMode::Auto, LoadMode::Idle) => Tier::Idle,
    }
}

// ---------------------------------------------------------------------------
// GPU 闸门
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct GpuGateState {
    policy: GpuPolicy,
    busy: bool,
    next_allowed: Option<Instant>,
}

/// torch worker GPU 任务的闸门：同一时间只放行一批；暂停时全部等待；占空比模式下每批之后冷却。
#[derive(Debug)]
pub struct GpuGate {
    state: Mutex<GpuGateState>,
    notify: Notify,
}

/// 持有期间占用 GPU；drop 时按本批耗时计算冷却时间。
#[derive(Debug)]
pub struct GpuPermit {
    gate: Arc<GpuGate>,
    started: Instant,
}

impl GpuGate {
    pub fn new(policy: GpuPolicy) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(GpuGateState {
                policy,
                busy: false,
                next_allowed: None,
            }),
            notify: Notify::new(),
        })
    }

    pub fn policy(&self) -> GpuPolicy {
        self.state
            .lock()
            .map(|state| state.policy)
            .unwrap_or(GpuPolicy::Paused)
    }

    pub fn set_policy(&self, policy: GpuPolicy) {
        if let Ok(mut state) = self.state.lock() {
            state.policy = policy;
            if matches!(policy, GpuPolicy::Unlimited) {
                state.next_allowed = None;
            }
        }
        self.notify.notify_waiters();
    }

    /// 等到允许跑下一批；`cancel` 触发时返回 `None`。
    pub async fn acquire(self: &Arc<Self>, cancel: &CancellationToken) -> Option<GpuPermit> {
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let wait_until = {
                let mut state = self.state.lock().ok()?;
                let now = Instant::now();
                let cooling = state.next_allowed.filter(|at| *at > now);
                match (state.policy, state.busy, cooling) {
                    (GpuPolicy::Paused, _, _) | (_, true, _) => None,
                    (_, false, Some(at)) => Some(at),
                    (_, false, None) => {
                        state.busy = true;
                        return Some(GpuPermit {
                            gate: self.clone(),
                            started: now,
                        });
                    }
                }
            };
            match wait_until {
                Some(at) => {
                    tokio::select! {
                        _ = tokio::time::sleep_until(at.into()) => {}
                        _ = notified => {}
                        _ = cancel.cancelled() => return None,
                    }
                }
                None => {
                    tokio::select! {
                        _ = notified => {}
                        _ = cancel.cancelled() => return None,
                    }
                }
            }
        }
    }

    fn release(&self, elapsed: Duration) {
        if let Ok(mut state) = self.state.lock() {
            state.busy = false;
            state.next_allowed = match state.policy {
                GpuPolicy::DutyCycle(duty) if duty > 0.0 && duty < 1.0 => {
                    Some(Instant::now() + elapsed.mul_f32(1.0 / duty - 1.0))
                }
                _ => None,
            };
        }
        self.notify.notify_waiters();
    }
}

impl Drop for GpuPermit {
    fn drop(&mut self) {
        self.gate.release(self.started.elapsed());
    }
}

// ---------------------------------------------------------------------------
// 治理器
// ---------------------------------------------------------------------------

/// 预算的展示形式（随 `system:load_mode` 事件发给前端）。
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BudgetView {
    pub visible_thumbnails: usize,
    pub batch_thumbnails: usize,
    pub network_reads: usize,
    /// `null` 表示不限。
    pub hash_tasks: Option<usize>,
    /// "paused" | "duty" | "unlimited"
    pub gpu: String,
    pub gpu_duty_cycle: Option<f32>,
    pub vlm: bool,
    pub worker_threads: usize,
}

impl From<&Budget> for BudgetView {
    fn from(budget: &Budget) -> Self {
        let (gpu, duty) = match budget.gpu {
            GpuPolicy::Paused => ("paused", None),
            GpuPolicy::DutyCycle(duty) => ("duty", Some(duty)),
            GpuPolicy::Unlimited => ("unlimited", None),
        };
        Self {
            visible_thumbnails: budget.visible_thumbnails,
            batch_thumbnails: budget.batch_thumbnails,
            network_reads: budget.network_reads,
            hash_tasks: (budget.hash_tasks != usize::MAX).then_some(budget.hash_tasks),
            gpu: gpu.to_string(),
            gpu_duty_cycle: duty,
            vlm: budget.vlm,
            worker_threads: budget.worker_threads,
        }
    }
}

/// `governor_status` / `system:load_mode` 的内容。
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GovernorStatus {
    pub mode: LoadMode,
    pub tier: Tier,
    pub override_mode: OverrideMode,
    /// 暂停的截止时间（Unix 秒）；`None` 且处于暂停 = 暂停到重启。
    pub paused_until: Option<i64>,
    pub idle_secs: u64,
    pub budget: BudgetView,
}

/// 预算下发回调：把并发上限应用到缩略图、读闸门、调度器、worker 线程数等。
pub type BudgetApplier = Box<dyn Fn(&Budget) + Send + Sync>;

struct GovernorState {
    machine: ModeMachine,
    override_mode: OverrideMode,
    paused_until: Option<(Instant, i64)>,
    settings: BudgetSettings,
    thresholds: ModeThresholds,
    last_idle: Duration,
    tier: Tier,
    budget: Budget,
}

pub struct ResourceGovernor {
    source: Box<dyn ActivitySource>,
    state: Mutex<GovernorState>,
    gpu: Arc<GpuGate>,
    apply: BudgetApplier,
    changes: watch::Sender<GovernorStatus>,
}

impl ResourceGovernor {
    /// `gpu` 是 AI 任务共用的闸门（`AiSupervisor::gpu_gate()`），治理器按档位调它的策略。
    pub fn new(
        source: Box<dyn ActivitySource>,
        settings: BudgetSettings,
        thresholds: ModeThresholds,
        gpu: Arc<GpuGate>,
        apply: BudgetApplier,
    ) -> Arc<Self> {
        let tier = Tier::Active;
        let budget = budget_for(tier, &settings);
        let state = GovernorState {
            machine: ModeMachine::new(LoadMode::Active),
            override_mode: OverrideMode::Auto,
            paused_until: None,
            settings,
            thresholds,
            last_idle: Duration::ZERO,
            tier,
            budget,
        };
        let status = status_of(&state);
        let (changes, _) = watch::channel(status);
        apply(&budget);
        gpu.set_policy(budget.gpu);
        Arc::new(Self {
            source,
            state: Mutex::new(state),
            gpu,
            apply,
            changes,
        })
    }

    pub fn gpu_gate(&self) -> Arc<GpuGate> {
        self.gpu.clone()
    }

    pub fn status(&self) -> GovernorStatus {
        self.changes.borrow().clone()
    }

    /// 当前预算。
    pub fn budget(&self) -> Budget {
        self.state
            .lock()
            .map(|state| state.budget)
            .unwrap_or_else(|_| budget_for(Tier::Paused, &BudgetSettings::default()))
    }

    pub fn vlm_allowed(&self) -> bool {
        self.budget().vlm
    }

    /// 订阅档位变化（VLM 流水线用它在降档时立刻停止投递）。
    pub fn subscribe(&self) -> watch::Receiver<GovernorStatus> {
        self.changes.subscribe()
    }

    /// 采样一次并推进状态机；档位变化时下发预算并返回新状态。
    pub fn tick(&self, now: Instant) -> Option<GovernorStatus> {
        let snapshot = self.source.snapshot();
        let mut state = self.state.lock().ok()?;
        state.last_idle = snapshot.idle_for;
        if let Some((until, _)) = state.paused_until {
            if now >= until {
                state.override_mode = OverrideMode::Auto;
                state.paused_until = None;
            }
        }
        let observed = classify(snapshot, &state.thresholds);
        let delay = state.thresholds.upgrade_delay;
        state.machine.observe(observed, now, delay);
        self.refresh(&mut state)
    }

    /// 设置手动覆盖。`pause_for = None` 且为暂停 = 暂停到重启。
    pub fn set_override(
        &self,
        override_mode: OverrideMode,
        pause_for: Option<Duration>,
        now: Instant,
    ) -> GovernorStatus {
        if let Ok(mut state) = self.state.lock() {
            state.override_mode = override_mode;
            state.paused_until = match (override_mode, pause_for) {
                (OverrideMode::Paused, Some(duration)) => {
                    Some((now + duration, unix_now() + duration.as_secs() as i64))
                }
                _ => None,
            };
            let _ = self.refresh(&mut state);
        }
        self.status()
    }

    /// 设置项变化（阈值、"使用电脑时也运行 AI"等）。
    pub fn update_settings(&self, settings: BudgetSettings, thresholds: ModeThresholds) {
        if let Ok(mut state) = self.state.lock() {
            state.settings = settings;
            state.thresholds = thresholds;
            let _ = self.refresh(&mut state);
        }
    }

    /// 重新计算档位和预算；预算变了就下发并通知订阅者。
    fn refresh(&self, state: &mut GovernorState) -> Option<GovernorStatus> {
        let tier = effective_tier(state.machine.current(), state.override_mode);
        let budget = budget_for(tier, &state.settings);
        let changed = tier != state.tier || budget != state.budget;
        state.tier = tier;
        if budget != state.budget {
            state.budget = budget;
            (self.apply)(&budget);
            self.gpu.set_policy(budget.gpu);
        }
        let status = status_of(state);
        let previous = self.changes.borrow().clone();
        if changed || status.mode != previous.mode || status.override_mode != previous.override_mode
        {
            self.changes.send_replace(status.clone());
            return Some(status);
        }
        None
    }

    /// 每 500ms 采样一次（读本机 API，不是前端轮询）；状态变化时调 `emit`。
    pub fn spawn<F>(self: Arc<Self>, emit: F)
    where
        F: Fn(GovernorStatus) + Send + 'static,
    {
        tauri::async_runtime::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_millis(500));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                interval.tick().await;
                if let Some(status) = self.tick(Instant::now()) {
                    tracing::info!(mode = ?status.mode, tier = ?status.tier, "load mode changed");
                    emit(status);
                }
            }
        });
    }
}

fn status_of(state: &GovernorState) -> GovernorStatus {
    GovernorStatus {
        mode: state.machine.current(),
        tier: state.tier,
        override_mode: state.override_mode,
        paused_until: state.paused_until.map(|(_, unix)| unix),
        idle_secs: state.last_idle.as_secs(),
        budget: BudgetView::from(&state.budget),
    }
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
