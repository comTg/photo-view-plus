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

        if let Some((priority, task)) = recv_highest_priority(&mut receivers) {
            inner.queued[priority.index()].fetch_sub(1, Ordering::Relaxed);
            let permit = match inner.semaphore.clone().acquire_owned().await {
                Ok(permit) => permit,
                Err(_) => break,
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

fn recv_highest_priority(
    receivers: &mut [mpsc::UnboundedReceiver<BoxTask>],
) -> Option<(Priority, BoxTask)> {
    for priority in Priority::all() {
        if let Ok(task) = receivers[priority.index()].try_recv() {
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
