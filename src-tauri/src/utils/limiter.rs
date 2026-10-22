//! 容量可在运行时调整的并发许可（docs/11 §5.3）。
//!
//! 资源治理器按负载模式调整缩略图、网络盘读原图等并发上限：
//! - 调大：立刻唤醒等待者；
//! - 调小：在途的任务不打断，只是新任务要等到占用数降到新上限以下才能拿到许可；
//! - 上限为 0：新任务全部等待（例如全屏 / 游戏时暂停批量缩略图）。

use std::sync::{Arc, Mutex};

use tokio::sync::Notify;

#[derive(Debug, Default)]
struct LimiterState {
    limit: usize,
    in_use: usize,
}

#[derive(Debug)]
pub struct AdjustableLimiter {
    state: Mutex<LimiterState>,
    notify: Notify,
}

/// 持有期间占用一个名额，drop 即归还。
#[derive(Debug)]
pub struct LimiterPermit {
    limiter: Arc<AdjustableLimiter>,
}

impl AdjustableLimiter {
    pub fn new(limit: usize) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(LimiterState { limit, in_use: 0 }),
            notify: Notify::new(),
        })
    }

    pub fn limit(&self) -> usize {
        self.state.lock().map(|state| state.limit).unwrap_or(0)
    }

    pub fn in_use(&self) -> usize {
        self.state.lock().map(|state| state.in_use).unwrap_or(0)
    }

    /// 调整上限。在途的许可不受影响。
    pub fn set_limit(&self, limit: usize) {
        if let Ok(mut state) = self.state.lock() {
            state.limit = limit;
        }
        self.notify.notify_waiters();
    }

    /// 不等待地尝试拿一个许可。
    pub fn try_acquire(self: &Arc<Self>) -> Option<LimiterPermit> {
        let mut state = self.state.lock().ok()?;
        if state.in_use < state.limit {
            state.in_use += 1;
            Some(LimiterPermit {
                limiter: self.clone(),
            })
        } else {
            None
        }
    }

    /// 等到占用数低于上限时拿一个许可。
    pub async fn acquire(self: &Arc<Self>) -> LimiterPermit {
        loop {
            // 先登记等待再检查，避免"检查完、还没开始等"之间的唤醒被漏掉。
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if let Some(permit) = self.try_acquire() {
                return permit;
            }
            notified.await;
        }
    }

    fn release(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.in_use = state.in_use.saturating_sub(1);
        }
        self.notify.notify_waiters();
    }
}

impl Drop for LimiterPermit {
    fn drop(&mut self) {
        self.limiter.release();
    }
}
