//! docs/11 R2：资源治理器。模式状态机用注入的假时钟和假输入源，覆盖降档立即生效、升档迟滞、
//! 全屏优先、手动覆盖与到期；GPU 闸门（暂停 / 一次一批 / 占空比冷却 / 取消）；
//! 许可容量调整后新任务遵守新上限（AdjustableLimiter、调度器单优先级上限、读闸门红线）。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use photo_view_plus_lib::config::{AppSettings, AppSettingsPatch};
use photo_view_plus_lib::queue::{FnTask, Priority, Scheduler};
use photo_view_plus_lib::services::resource_governor::{
    budget_for, classify, effective_tier, parse_override, settings_from, Budget, BudgetSettings,
    GpuGate, GpuPolicy, LoadMode, ModeMachine, ModeThresholds, OverrideMode, ResourceGovernor,
    Tier, DUTY_CYCLE_AWAY,
};
use photo_view_plus_lib::utils::limiter::AdjustableLimiter;
use photo_view_plus_lib::utils::read_gate;
use photo_view_plus_lib::utils::win_activity::{ActivitySnapshot, ActivitySource, PresenceState};
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
struct FakeActivity(Arc<Mutex<ActivitySnapshot>>);

impl FakeActivity {
    fn new() -> Self {
        Self(Arc::new(Mutex::new(ActivitySnapshot {
            idle_for: Duration::ZERO,
            presence: PresenceState::Normal,
        })))
    }

    fn set(&self, idle_for: Duration, presence: PresenceState) {
        *self.0.lock().expect("activity") = ActivitySnapshot { idle_for, presence };
    }
}

impl ActivitySource for FakeActivity {
    fn snapshot(&self) -> ActivitySnapshot {
        *self.0.lock().expect("activity")
    }
}

struct Harness {
    governor: Arc<ResourceGovernor>,
    activity: FakeActivity,
    applied: Arc<Mutex<Vec<Budget>>>,
    gpu: Arc<GpuGate>,
}

impl Harness {
    fn new(settings: BudgetSettings) -> Self {
        let activity = FakeActivity::new();
        let applied = Arc::new(Mutex::new(Vec::new()));
        let sink = applied.clone();
        let gpu = GpuGate::new(GpuPolicy::Unlimited);
        let governor = ResourceGovernor::new(
            Box::new(activity.clone()),
            settings,
            ModeThresholds::default(),
            gpu.clone(),
            Box::new(move |budget: &Budget| sink.lock().expect("applied").push(*budget)),
        );
        Self {
            governor,
            activity,
            applied,
            gpu,
        }
    }

    fn last_budget(&self) -> Budget {
        *self
            .applied
            .lock()
            .expect("applied")
            .last()
            .expect("budget applied")
    }
}

fn snapshot(idle_secs: u64, presence: PresenceState) -> ActivitySnapshot {
    ActivitySnapshot {
        idle_for: Duration::from_secs(idle_secs),
        presence,
    }
}

#[test]
fn classify_by_idle_time_and_presence() {
    let thresholds = ModeThresholds::default();
    let normal = PresenceState::Normal;
    assert_eq!(classify(snapshot(5, normal), &thresholds), LoadMode::Active);
    assert_eq!(classify(snapshot(20, normal), &thresholds), LoadMode::Away);
    assert_eq!(classify(snapshot(179, normal), &thresholds), LoadMode::Away);
    assert_eq!(classify(snapshot(180, normal), &thresholds), LoadMode::Idle);
    // 锁屏 / 屏保 = 空闲，哪怕刚有过输入。
    assert_eq!(
        classify(snapshot(0, PresenceState::NotPresent), &thresholds),
        LoadMode::Idle
    );
    // 全屏 / 游戏压过空闲。
    assert_eq!(
        classify(snapshot(3600, PresenceState::Busy), &thresholds),
        LoadMode::Busy
    );
}

#[test]
fn upgrade_timer_restarts_when_observation_flips() {
    let mut machine = ModeMachine::new(LoadMode::Active);
    let delay = Duration::from_secs(10);
    let t0 = Instant::now();
    let at = |secs: u64| t0 + Duration::from_secs(secs);
    assert!(!machine.observe(LoadMode::Away, at(0), delay));
    // 中途又有输入：候选清空。
    assert!(!machine.observe(LoadMode::Active, at(5), delay));
    assert!(!machine.observe(LoadMode::Away, at(6), delay));
    assert!(!machine.observe(LoadMode::Away, at(15), delay));
    assert!(machine.observe(LoadMode::Away, at(16), delay));
    assert_eq!(machine.current(), LoadMode::Away);
    // 降档立即生效。
    assert!(machine.observe(LoadMode::Busy, at(17), delay));
    assert_eq!(machine.current(), LoadMode::Busy);
}

#[test]
fn downgrade_is_immediate_and_upgrade_needs_ten_seconds() {
    let harness = Harness::new(BudgetSettings::default());
    let governor = &harness.governor;
    let t0 = Instant::now();

    // 起步是最保守的"使用中"：GPU 任务暂停、批量缩略图 1、哈希 1。
    assert_eq!(governor.status().tier, Tier::Active);
    assert_eq!(harness.gpu.policy(), GpuPolicy::Paused);
    assert_eq!(harness.last_budget().batch_thumbnails, 1);

    // 离开 5 分钟：10 秒内不升档。
    harness
        .activity
        .set(Duration::from_secs(300), PresenceState::Normal);
    assert!(governor.tick(t0).is_none());
    assert!(governor.tick(t0 + Duration::from_millis(9_500)).is_none());
    assert_eq!(governor.status().tier, Tier::Active);
    let status = governor
        .tick(t0 + Duration::from_secs(10))
        .expect("upgrade after 10s");
    assert_eq!(status.mode, LoadMode::Idle);
    assert_eq!(status.tier, Tier::Idle);
    assert_eq!(harness.gpu.policy(), GpuPolicy::Unlimited);
    let idle = harness.last_budget();
    assert_eq!(idle.batch_thumbnails, 6);
    assert_eq!(idle.hash_tasks, usize::MAX);
    assert!(idle.vlm);

    // 动一下鼠标：下一次采样立即降回"使用中"。
    harness.activity.set(Duration::ZERO, PresenceState::Normal);
    let status = governor
        .tick(t0 + Duration::from_millis(10_500))
        .expect("immediate downgrade");
    assert_eq!(status.tier, Tier::Active);
    assert_eq!(harness.gpu.policy(), GpuPolicy::Paused);
    let active = harness.last_budget();
    assert_eq!(active.batch_thumbnails, 1);
    assert_eq!(active.network_reads, 3);
    assert_eq!(active.hash_tasks, 1);
    assert_eq!(active.worker_threads, 1);
    assert!(!active.vlm);
}

#[test]
fn fullscreen_beats_full_speed_override() {
    let harness = Harness::new(BudgetSettings::default());
    let t0 = Instant::now();
    let status = harness
        .governor
        .set_override(OverrideMode::FullSpeed, None, t0);
    // "全速"忽略使用中。
    assert_eq!(status.tier, Tier::Idle);
    assert_eq!(harness.gpu.policy(), GpuPolicy::Unlimited);

    harness
        .activity
        .set(Duration::from_secs(600), PresenceState::Busy);
    let status = harness
        .governor
        .tick(t0 + Duration::from_millis(500))
        .expect("busy");
    assert_eq!(status.mode, LoadMode::Busy);
    assert_eq!(status.tier, Tier::Busy);
    assert_eq!(harness.gpu.policy(), GpuPolicy::Paused);
    assert!(!harness.governor.vlm_allowed());
    assert_eq!(harness.governor.budget().hash_tasks, 0);
    assert_eq!(harness.governor.budget().network_reads, 1);
}

#[test]
fn pause_for_an_hour_expires_back_to_auto() {
    let harness = Harness::new(BudgetSettings::default());
    harness
        .activity
        .set(Duration::from_secs(600), PresenceState::Normal);
    let t0 = Instant::now();
    let status =
        harness
            .governor
            .set_override(OverrideMode::Paused, Some(Duration::from_secs(3600)), t0);
    assert_eq!(status.tier, Tier::Paused);
    assert!(status.paused_until.is_some());

    // 暂停期间模式照常推进，但档位保持暂停。
    harness.governor.tick(t0 + Duration::from_secs(30));
    harness.governor.tick(t0 + Duration::from_secs(45));
    assert_eq!(harness.governor.status().tier, Tier::Paused);
    assert_eq!(harness.gpu.policy(), GpuPolicy::Paused);

    let status = harness
        .governor
        .tick(t0 + Duration::from_secs(3600))
        .expect("pause expired");
    assert_eq!(status.override_mode, OverrideMode::Auto);
    assert_eq!(status.paused_until, None);
    assert_eq!(status.tier, Tier::Idle);
}

#[test]
fn pause_until_restart_never_expires() {
    let harness = Harness::new(BudgetSettings::default());
    let t0 = Instant::now();
    let status = harness
        .governor
        .set_override(OverrideMode::Paused, None, t0);
    assert_eq!(status.tier, Tier::Paused);
    assert_eq!(status.paused_until, None);
    harness
        .activity
        .set(Duration::from_secs(600), PresenceState::Normal);
    harness
        .governor
        .tick(t0 + Duration::from_secs(60 * 60 * 24));
    harness
        .governor
        .tick(t0 + Duration::from_secs(60 * 60 * 48));
    assert_eq!(harness.governor.status().tier, Tier::Paused);
    // 切回自动立即生效。
    let status = harness.governor.set_override(
        OverrideMode::Auto,
        None,
        t0 + Duration::from_secs(60 * 60 * 48),
    );
    assert_eq!(status.tier, Tier::Idle);
}

#[test]
fn ai_while_active_runs_gpu_at_duty_cycle() {
    let harness = Harness::new(BudgetSettings::default());
    assert_eq!(harness.gpu.policy(), GpuPolicy::Paused);
    harness.governor.update_settings(
        BudgetSettings {
            ai_while_active: true,
            ..BudgetSettings::default()
        },
        ModeThresholds::default(),
    );
    assert_eq!(harness.gpu.policy(), GpuPolicy::DutyCycle(DUTY_CYCLE_AWAY));
    // "大模型只在空闲时运行"默认开。
    assert!(!harness.governor.vlm_allowed());
    harness.governor.update_settings(
        BudgetSettings {
            ai_while_active: true,
            vlm_idle_only: false,
            ..BudgetSettings::default()
        },
        ModeThresholds::default(),
    );
    assert!(harness.governor.vlm_allowed());
}

#[test]
fn budget_table_matches_docs() {
    let settings = BudgetSettings::default();
    let away = budget_for(Tier::Away, &settings);
    assert_eq!(away.batch_thumbnails, 3);
    assert_eq!(away.network_reads, 4);
    assert_eq!(away.hash_tasks, 2);
    assert_eq!(away.gpu, GpuPolicy::DutyCycle(DUTY_CYCLE_AWAY));
    assert_eq!(away.worker_threads, 2);
    for tier in [
        Tier::Active,
        Tier::Away,
        Tier::Idle,
        Tier::Busy,
        Tier::Paused,
    ] {
        // 红线：网络盘并发 ≤ 4。
        assert!(budget_for(tier, &settings).network_reads <= 4);
    }
    let paused = budget_for(Tier::Paused, &settings);
    assert_eq!(paused.gpu, GpuPolicy::Paused);
    assert!(!paused.vlm);
    // 暂停 / 全屏：批量缩略图全部等待，只留可见区域队列（R3）。
    assert_eq!(paused.batch_thumbnails, 0);
    assert_eq!(paused.visible_thumbnails, 2);
    let busy = budget_for(Tier::Busy, &settings);
    assert_eq!(busy.batch_thumbnails, 0);
    assert_eq!(busy.visible_thumbnails, 1);
    assert_eq!(busy.hash_tasks, 0);
    // 小机器：短暂离开档的缩略图不超过空闲档。
    let small = BudgetSettings {
        idle_thumbnails: 2,
        ..BudgetSettings::default()
    };
    assert_eq!(budget_for(Tier::Away, &small).batch_thumbnails, 2);
    assert_eq!(
        effective_tier(LoadMode::Busy, OverrideMode::FullSpeed),
        Tier::Busy
    );
    assert_eq!(
        effective_tier(LoadMode::Busy, OverrideMode::Paused),
        Tier::Paused
    );
}

#[test]
fn settings_map_to_governor_inputs() {
    let defaults = AppSettings::default();
    assert_eq!(defaults.idle_threshold_minutes, 3);
    assert!(!defaults.ai_while_active);
    assert!(defaults.vlm_idle_only);

    // 旧配置文件没有这些字段：补默认值，不清空其他设置。
    let old: AppSettings =
        serde_json::from_str(r#"{"aiEnabled":false,"thumbCacheGb":9}"#).expect("old config");
    assert!(!old.ai_enabled);
    assert_eq!(old.thumb_cache_gb, 9);
    assert_eq!(old.idle_threshold_minutes, 3);
    assert!(old.vlm_idle_only);

    let mut settings = AppSettings::default();
    settings.apply_patch(AppSettingsPatch {
        idle_threshold_minutes: Some(0),
        ai_while_active: Some(true),
        vlm_idle_only: Some(false),
        ..AppSettingsPatch::default()
    });
    assert_eq!(settings.idle_threshold_minutes, 1);
    settings.apply_patch(AppSettingsPatch {
        idle_threshold_minutes: Some(5),
        ..AppSettingsPatch::default()
    });
    let (budget, thresholds) = settings_from(&settings, 6);
    assert_eq!(thresholds.idle_after, Duration::from_secs(300));
    assert_eq!(thresholds.away_after, Duration::from_secs(20));
    assert!(budget.ai_while_active);
    assert!(!budget.vlm_idle_only);
    assert_eq!(budget.idle_thumbnails, 6);

    assert_eq!(parse_override("auto"), Some((OverrideMode::Auto, None)));
    assert_eq!(
        parse_override("pause1h"),
        Some((OverrideMode::Paused, Some(Duration::from_secs(3600))))
    );
    assert_eq!(
        parse_override("pauseUntilRestart"),
        Some((OverrideMode::Paused, None))
    );
    assert_eq!(
        parse_override("fullSpeed"),
        Some((OverrideMode::FullSpeed, None))
    );
    assert_eq!(parse_override("turbo"), None);
}

#[tokio::test]
async fn gpu_gate_waits_while_paused_and_resumes() {
    let gate = GpuGate::new(GpuPolicy::Paused);
    let cancel = CancellationToken::new();
    let blocked = tokio::time::timeout(Duration::from_millis(150), gate.acquire(&cancel)).await;
    assert!(blocked.is_err(), "paused gate must not hand out permits");

    let waiter_gate = gate.clone();
    let waiter_cancel = cancel.clone();
    let waiter = tokio::spawn(async move { waiter_gate.acquire(&waiter_cancel).await.is_some() });
    tokio::time::sleep(Duration::from_millis(50)).await;
    gate.set_policy(GpuPolicy::Unlimited);
    let acquired = tokio::time::timeout(Duration::from_secs(1), waiter)
        .await
        .expect("resumed")
        .expect("join");
    assert!(acquired);
}

#[tokio::test]
async fn gpu_gate_cancel_returns_none() {
    let gate = GpuGate::new(GpuPolicy::Paused);
    let cancel = CancellationToken::new();
    let waiter_gate = gate.clone();
    let waiter_cancel = cancel.clone();
    let waiter = tokio::spawn(async move { waiter_gate.acquire(&waiter_cancel).await.is_none() });
    tokio::time::sleep(Duration::from_millis(30)).await;
    cancel.cancel();
    let cancelled = tokio::time::timeout(Duration::from_secs(1), waiter)
        .await
        .expect("cancelled")
        .expect("join");
    assert!(cancelled);
}

#[tokio::test]
async fn gpu_gate_runs_one_batch_at_a_time() {
    let gate = GpuGate::new(GpuPolicy::Unlimited);
    let cancel = CancellationToken::new();
    let first = gate.acquire(&cancel).await.expect("first");
    let second = tokio::time::timeout(Duration::from_millis(100), gate.acquire(&cancel)).await;
    assert!(second.is_err(), "second batch must wait for the first");
    drop(first);
    let second = tokio::time::timeout(Duration::from_secs(1), gate.acquire(&cancel))
        .await
        .expect("second after release");
    assert!(second.is_some());
}

#[tokio::test]
async fn gpu_gate_duty_cycle_cools_down_between_batches() {
    let gate = GpuGate::new(GpuPolicy::DutyCycle(0.5));
    let cancel = CancellationToken::new();
    let permit = gate.acquire(&cancel).await.expect("first");
    tokio::time::sleep(Duration::from_millis(80)).await;
    drop(permit);
    // 冷却 = 本批 80ms × (1/0.5 − 1) = 80ms。
    let started = Instant::now();
    let _second = gate.acquire(&cancel).await.expect("second");
    let waited = started.elapsed();
    assert!(waited >= Duration::from_millis(70), "waited {waited:?}");
    assert!(waited < Duration::from_millis(600), "waited {waited:?}");
}

#[tokio::test]
async fn gpu_gate_switching_to_unlimited_skips_cooldown() {
    let gate = GpuGate::new(GpuPolicy::DutyCycle(0.1));
    let cancel = CancellationToken::new();
    let permit = gate.acquire(&cancel).await.expect("first");
    tokio::time::sleep(Duration::from_millis(50)).await;
    drop(permit);
    // 占空比 10% 本该冷却 450ms；切到空闲（不限）后立即放行。
    gate.set_policy(GpuPolicy::Unlimited);
    let started = Instant::now();
    let _second = gate.acquire(&cancel).await.expect("second");
    assert!(started.elapsed() < Duration::from_millis(200));
}

#[tokio::test]
async fn limiter_capacity_changes_apply_to_new_permits() {
    let limiter = AdjustableLimiter::new(2);
    let a = limiter.try_acquire().expect("a");
    let b = limiter.try_acquire().expect("b");
    assert!(limiter.try_acquire().is_none());

    // 调大：立刻多出名额。
    limiter.set_limit(3);
    let c = limiter.try_acquire().expect("raised limit");

    // 调小：在途的不打断；新许可要等占用降到新上限以下。
    limiter.set_limit(1);
    assert_eq!(limiter.in_use(), 3);
    drop(a);
    assert!(limiter.try_acquire().is_none());
    drop(b);
    assert!(limiter.try_acquire().is_none());
    let waiter_limiter = limiter.clone();
    let waiter = tokio::spawn(async move {
        let _permit = waiter_limiter.acquire().await;
    });
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert!(!waiter.is_finished());
    drop(c);
    tokio::time::timeout(Duration::from_secs(1), waiter)
        .await
        .expect("waiter woke")
        .expect("join");
    assert_eq!(limiter.in_use(), 0);
}

#[tokio::test]
async fn limiter_zero_holds_everything_until_raised() {
    let limiter = AdjustableLimiter::new(0);
    assert!(limiter.try_acquire().is_none());
    let waiter_limiter = limiter.clone();
    let waiter = tokio::spawn(async move {
        let _permit = waiter_limiter.acquire().await;
    });
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert!(!waiter.is_finished());
    limiter.set_limit(1);
    tokio::time::timeout(Duration::from_secs(1), waiter)
        .await
        .expect("waiter woke")
        .expect("join");
}

#[tokio::test]
async fn scheduler_cap_change_applies_to_new_tasks() {
    let mut caps = [usize::MAX; 8];
    caps[Priority::P2 as usize] = 0;
    let scheduler = Scheduler::start_with_caps(4, caps);
    let ran = Arc::new(AtomicBool::new(false));
    let flag = ran.clone();
    scheduler
        .enqueue(FnTask::new(Priority::P2, "hash", move |_| {
            let flag = flag.clone();
            Box::pin(async move {
                flag.store(true, Ordering::Relaxed);
                Ok(())
            })
        }))
        .expect("enqueue");
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!ran.load(Ordering::Relaxed), "cap 0 must hold the task");
    assert_eq!(scheduler.cap(Priority::P2), 0);

    scheduler.set_cap(Priority::P2, 1);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(ran.load(Ordering::Relaxed));
}

#[test]
fn network_read_limit_never_exceeds_red_line() {
    read_gate::set_network_limit(9);
    assert_eq!(
        read_gate::network_limit(),
        read_gate::NETWORK_READ_CONCURRENCY
    );
    assert!(read_gate::network_limit() <= 4);
    read_gate::set_network_limit(0);
    assert_eq!(read_gate::network_limit(), 1);
    read_gate::set_network_limit(3);
    assert_eq!(read_gate::network_limit(), 3);
}

#[test]
fn system_activity_reads_real_state() {
    use photo_view_plus_lib::utils::win_activity::SystemActivity;
    let snapshot = SystemActivity.snapshot();
    // GetTickCount 回绕最多 49.7 天；这里只确认 API 可用、数值合理。
    assert!(snapshot.idle_for < Duration::from_secs(50 * 24 * 3600));
}

#[cfg(windows)]
#[tokio::test]
async fn child_process_binds_to_job_object() {
    use photo_view_plus_lib::utils::job_object::bind_child;
    let mut child = tokio::process::Command::new("cmd")
        .args(["/C", "ping -n 2 127.0.0.1 >NUL"])
        .kill_on_drop(true)
        .spawn()
        .expect("spawn cmd");
    assert!(
        bind_child(&child),
        "child should join the kill-on-close job"
    );
    let _ = child.kill().await;
}
