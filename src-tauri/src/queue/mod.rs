use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use async_trait::async_trait;
use serde::Serialize;
use tokio::sync::{mpsc, Semaphore};
use tokio_util::sync::CancellationToken;

use crate::error::{AppError, AppResult};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Priority {
    P0 = 0,
    P1 = 1,
    P2 = 2,
    P3 = 3,
    P4 = 4,
    P5 = 5,
    P6 = 6,
    P7 = 7,
}

impl Priority {
    pub const fn all() -> [Self; 8] {
        [
            Self::P0,
            Self::P1,
            Self::P2,
            Self::P3,
            Self::P4,
            Self::P5,
            Self::P6,
            Self::P7,
        ]
    }

    const fn index(self) -> usize {
        self as usize
    }
}

#[derive(Clone)]
pub struct TaskContext {
    cancellation: CancellationToken,
}

impl TaskContext {
    pub fn is_cancelled(&self) -> bool {
        self.cancellation.is_cancelled()
    }

    pub fn cancellation_token(&self) -> CancellationToken {
        self.cancellation.clone()
    }
}

#[async_trait]
pub trait Task: Send + Sync {
    fn priority(&self) -> Priority;
    fn label(&self) -> String;
    async fn run(&self, ctx: TaskContext) -> AppResult<()>;
}

type BoxTask = Box<dyn Task>;

#[derive(Clone)]
pub struct Scheduler {
    inner: Arc<SchedulerInner>,
}

struct SchedulerInner {
    senders: Vec<mpsc::UnboundedSender<BoxTask>>,
    queued: [AtomicUsize; 8],
    /// 每个优先级当前"已派发且未完成"的任务数，用于配合 `caps` 做单优先级并发上限。
    running_by_priority: [AtomicUsize; 8],
    /// 每个优先级的最大同时运行数。默认 `usize::MAX`（不限，仅受全局并发约束）。
    /// 用来防止高优先级任务（如缩略图 P0）占满全部并发槽、饿死低优先级任务（去重哈希 P2/P3）。
    caps: [usize; 8],
    running: AtomicUsize,
    completed: AtomicUsize,
    failed: AtomicUsize,
    paused: AtomicBool,
    semaphore: Arc<Semaphore>,
    cancellation: Mutex<CancellationToken>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QueueStatus {
    pub p0: usize,
    pub p1: usize,
    pub p2: usize,
    pub p3: usize,
    pub p4: usize,
    pub p5: usize,
    pub p6: usize,
    pub p7: usize,
    pub running: usize,
    pub completed: usize,
    pub failed: usize,
    pub paused: bool,
}

impl Scheduler {
    pub fn start(max_concurrency: usize) -> Self {
        Self::start_with_caps(max_concurrency, [usize::MAX; 8])
    }

    /// 同 [`Scheduler::start`]，但可为每个优先级设置"最多同时运行数"上限（按 `Priority` 序号索引）。
    /// 不设上限的优先级传 `usize::MAX`。用于防止某一优先级占满全部并发槽饿死其他优先级。
    pub fn start_with_caps(max_concurrency: usize, caps: [usize; 8]) -> Self {
        let mut senders = Vec::new();
        let mut receivers = Vec::new();

        for _ in Priority::all() {
            let (tx, rx) = mpsc::unbounded_channel();
            senders.push(tx);
            receivers.push(rx);
        }

        let scheduler = Self {
            inner: Arc::new(SchedulerInner {
                senders,
                queued: std::array::from_fn(|_| AtomicUsize::new(0)),
                running_by_priority: std::array::from_fn(|_| AtomicUsize::new(0)),
                caps,
                running: AtomicUsize::new(0),
                completed: AtomicUsize::new(0),
                failed: AtomicUsize::new(0),
                paused: AtomicBool::new(false),
                semaphore: Arc::new(Semaphore::new(max_concurrency.max(1))),
                cancellation: Mutex::new(CancellationToken::new()),
            }),
        };

        scheduler.spawn_dispatcher_thread(receivers);
        scheduler
    }

    pub fn enqueue<T>(&self, task: T) -> AppResult<()>
    where
        T: Task + 'static,
    {
        self.enqueue_boxed(Box::new(task))
    }

    pub fn enqueue_boxed(&self, task: BoxTask) -> AppResult<()> {
        let priority = task.priority();
        self.inner.queued[priority.index()].fetch_add(1, Ordering::Relaxed);
        self.inner.senders[priority.index()]
            .send(task)
            .map_err(|_| AppError::Other("任务队列已停止".to_string()))
    }

    pub fn pause(&self) {
        self.inner.paused.store(true, Ordering::Relaxed);
    }

    pub fn resume(&self) {
        self.inner.paused.store(false, Ordering::Relaxed);
    }

    pub fn cancel_running(&self) {
        if let Ok(mut token) = self.inner.cancellation.lock() {
            token.cancel();
            *token = CancellationToken::new();
        }
    }

    pub fn status(&self) -> QueueStatus {
        QueueStatus {
            p0: self.inner.queued[0].load(Ordering::Relaxed),
            p1: self.inner.queued[1].load(Ordering::Relaxed),
            p2: self.inner.queued[2].load(Ordering::Relaxed),
            p3: self.inner.queued[3].load(Ordering::Relaxed),
            p4: self.inner.queued[4].load(Ordering::Relaxed),
            p5: self.inner.queued[5].load(Ordering::Relaxed),
            p6: self.inner.queued[6].load(Ordering::Relaxed),
            p7: self.inner.queued[7].load(Ordering::Relaxed),
            running: self.inner.running.load(Ordering::Relaxed),
            completed: self.inner.completed.load(Ordering::Relaxed),
            failed: self.inner.failed.load(Ordering::Relaxed),
            paused: self.inner.paused.load(Ordering::Relaxed),
        }
    }

    fn spawn_dispatcher_thread(&self, receivers: Vec<mpsc::UnboundedReceiver<BoxTask>>) {
        let inner = self.inner.clone();
        let spawn_result = thread::Builder::new()
            .name("pvp-queue-dispatcher".to_string())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_multi_thread()
                    .thread_name("pvp-queue-worker")
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        tracing::error!(%error, "failed to start queue runtime");
                        return;
                    }
                };
                runtime.block_on(dispatch_loop(inner, receivers));
            });

        if let Err(error) = spawn_result {
            tracing::error!(%error, "failed to start queue dispatcher thread");
        }
    }

    pub fn spawn_status_loop<F>(&self, emit: F)
    where
        F: Fn(QueueStatus) + Send + 'static,
    {
        let scheduler = self.clone();
        let spawn_result = thread::Builder::new()
            .name("pvp-queue-status".to_string())
            .spawn(move || loop {
                thread::sleep(Duration::from_secs(1));
                emit(scheduler.status());
            });

        if let Err(error) = spawn_result {
            tracing::error!(%error, "failed to start queue status thread");
        }
    }
}

async fn dispatch_loop(
    inner: Arc<SchedulerInner>,
    mut receivers: Vec<mpsc::UnboundedReceiver<BoxTask>>,
) {
    loop {
        if inner.paused.load(Ordering::Relaxed) {
            tokio::time::sleep(Duration::from_millis(100)).await;
            continue;
        }

        // 按优先级取一个"未达到本优先级并发上限"的任务。先占住该优先级的运行计数，
        // 再去抢全局并发槽——这样高优先级达到上限后，dispatcher 会顺延到低优先级，
        // 不会把所有全局槽都消耗在排队等待的高优先级任务上。
        if let Some((priority, task)) = recv_dispatchable(&mut receivers, &inner) {
            let idx = priority.index();
            inner.queued[idx].fetch_sub(1, Ordering::Relaxed);
            inner.running_by_priority[idx].fetch_add(1, Ordering::Relaxed);
            let permit = match inner.semaphore.clone().acquire_owned().await {
                Ok(permit) => permit,
                Err(_) => {
                    inner.running_by_priority[idx].fetch_sub(1, Ordering::Relaxed);
                    break;
                }
            };
            let inner_for_task = inner.clone();
            let token = inner
                .cancellation
                .lock()
                .map(|token| token.child_token())
                .unwrap_or_else(|_| CancellationToken::new());
            tokio::spawn(async move {
                let _permit = permit;
                inner_for_task.running.fetch_add(1, Ordering::Relaxed);
                let label = task.label();
                tracing::debug!(label, priority = ?priority, "queue task started");
                let result = task
                    .run(TaskContext {
                        cancellation: token,
                    })
                    .await;
                inner_for_task.running.fetch_sub(1, Ordering::Relaxed);
                inner_for_task.running_by_priority[idx].fetch_sub(1, Ordering::Relaxed);
                match result {
                    Ok(()) => {
                        inner_for_task.completed.fetch_add(1, Ordering::Relaxed);
                        tracing::debug!(label, priority = ?priority, "queue task completed");
                    }
                    Err(error) => {
                        inner_for_task.failed.fetch_add(1, Ordering::Relaxed);
                        tracing::warn!(label, priority = ?priority, %error, "queue task failed");
                    }
                }
            });
            continue;
        }

        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// 取一个可调度的任务：跳过已达到 `caps` 上限的优先级，按优先级从高到低返回第一个有任务的。
fn recv_dispatchable(
    receivers: &mut [mpsc::UnboundedReceiver<BoxTask>],
    inner: &SchedulerInner,
) -> Option<(Priority, BoxTask)> {
    for priority in Priority::all() {
        let idx = priority.index();
        if inner.running_by_priority[idx].load(Ordering::Relaxed) >= inner.caps[idx] {
            continue;
        }
        if let Ok(task) = receivers[idx].try_recv() {
            return Some((priority, task));
        }
    }
    None
}

pub struct FnTask<F>
where
    F: Fn(TaskContext) -> Pin<Box<dyn Future<Output = AppResult<()>> + Send>> + Send + Sync,
{
    priority: Priority,
    label: String,
    run: F,
}

impl<F> FnTask<F>
where
    F: Fn(TaskContext) -> Pin<Box<dyn Future<Output = AppResult<()>> + Send>> + Send + Sync,
{
    pub fn new(priority: Priority, label: impl Into<String>, run: F) -> Self {
        Self {
            priority,
            label: label.into(),
            run,
        }
    }
}

#[async_trait]
impl<F> Task for FnTask<F>
where
    F: Fn(TaskContext) -> Pin<Box<dyn Future<Output = AppResult<()>> + Send>> + Send + Sync,
{
    fn priority(&self) -> Priority {
        self.priority
    }

    fn label(&self) -> String {
        self.label.clone()
    }

    async fn run(&self, ctx: TaskContext) -> AppResult<()> {
        (self.run)(ctx).await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex as StdMutex;

    use super::*;

    #[tokio::test]
    async fn higher_priority_runs_first() {
        let scheduler = Scheduler::start(1);
        scheduler.pause();
        let order = Arc::new(StdMutex::new(Vec::new()));

        for (priority, value) in [(Priority::P3, 3), (Priority::P0, 0), (Priority::P1, 1)] {
            let order = order.clone();
            scheduler
                .enqueue(FnTask::new(priority, format!("task-{value}"), move |_| {
                    let order = order.clone();
                    Box::pin(async move {
                        order.lock().expect("record order").push(value);
                        Ok(())
                    })
                }))
                .expect("enqueue");
        }

        scheduler.resume();
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(*order.lock().expect("order"), vec![0, 1, 3]);
    }

    #[tokio::test]
    async fn priority_cap_prevents_starvation() {
        // P0 上限设为 1：即使塞满一堆长时间 P0 任务，低优先级 P2 也应能拿到剩下的并发槽运行。
        let mut caps = [usize::MAX; 8];
        caps[Priority::P0.index()] = 1;
        let scheduler = Scheduler::start_with_caps(2, caps);
        scheduler.pause();

        for i in 0..6 {
            scheduler
                .enqueue(FnTask::new(Priority::P0, format!("p0-{i}"), move |_| {
                    Box::pin(async move {
                        tokio::time::sleep(Duration::from_millis(200)).await;
                        Ok(())
                    })
                }))
                .expect("enqueue p0");
        }

        let p2_done = Arc::new(AtomicBool::new(false));
        let flag = p2_done.clone();
        scheduler
            .enqueue(FnTask::new(Priority::P2, "p2", move |_| {
                let flag = flag.clone();
                Box::pin(async move {
                    flag.store(true, Ordering::Relaxed);
                    Ok(())
                })
            }))
            .expect("enqueue p2");

        scheduler.resume();
        // P0 受限于 cap=1 只能跑 1 个，剩下 1 个全局槽留给 P2 → P2 应很快完成。
        // 若没有 cap（严格优先级），P2 要等全部 6 个 P0 跑完（约 600ms）。
        tokio::time::sleep(Duration::from_millis(120)).await;
        assert!(
            p2_done.load(Ordering::Relaxed),
            "P2 task should run alongside capped P0 backlog, not be starved"
        );
    }

    #[tokio::test]
    async fn cancel_token_reaches_running_task() {
        let scheduler = Scheduler::start(1);
        let cancelled = Arc::new(AtomicBool::new(false));
        let flag = cancelled.clone();

        scheduler
            .enqueue(FnTask::new(Priority::P0, "cancel-check", move |ctx| {
                let flag = flag.clone();
                Box::pin(async move {
                    for _ in 0..20 {
                        if ctx.is_cancelled() {
                            flag.store(true, Ordering::Relaxed);
                            return Ok(());
                        }
                        tokio::time::sleep(Duration::from_millis(25)).await;
                    }
                    Ok(())
                })
            }))
            .expect("enqueue");

        tokio::time::sleep(Duration::from_millis(50)).await;
        scheduler.cancel_running();
        tokio::time::sleep(Duration::from_millis(120)).await;
        assert!(cancelled.load(Ordering::Relaxed));
    }
}
