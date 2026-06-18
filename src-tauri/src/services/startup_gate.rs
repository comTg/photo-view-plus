use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::sync::Notify;

/// 启动期重活的放行闸门：把「会瞬间吃满 CPU/磁盘」的任务（如缩略图重排）推迟到
/// 前端发出「首屏已绘制」信号之后再做，避免它们和 dev 下 WebView/Vite 的首屏模块
/// 加载抢资源——实测这会把白屏从几秒拖到近两分钟。
///
/// 固定计时（如旧的 AI STARTUP_GRACE）不可靠：首屏本身就是被拖慢的对象，计时会在
/// 白屏中途到期、让重活提前介入。改挂在真实的首屏信号上。`fallback` 兜底，保证前端
/// 万一不发信号（异常退出等）也不会把这些任务永久卡死。
pub struct StartupGate {
    released: AtomicBool,
    notify: Notify,
}

impl StartupGate {
    pub fn new() -> Self {
        Self {
            released: AtomicBool::new(false),
            notify: Notify::new(),
        }
    }

    /// 标记前端已就绪并唤醒全部等待者。多次调用幂等。
    pub fn release(&self) {
        self.released.store(true, Ordering::Release);
        self.notify.notify_waiters();
    }

    /// 等待 `release()`，或最多等 `fallback` 后自行放行。
    pub async fn wait(&self, fallback: Duration) {
        // 先登记 notified() 再查标志位，避免 release() 落在两者之间造成丢唤醒。
        let notified = self.notify.notified();
        if self.released.load(Ordering::Acquire) {
            return;
        }
        tokio::select! {
            _ = notified => {}
            _ = tokio::time::sleep(fallback) => {
                tracing::warn!("startup gate fallback elapsed before UI signalled ready");
            }
        }
    }
}

impl Default for StartupGate {
    fn default() -> Self {
        Self::new()
    }
}
