//! 原图读取并发限流闸门。
//!
//! 网络盘（SMB / 映射盘）并发读过高会拖死 NAS（CLAUDE.md 红线 6：网络盘并发上限 4，
//! 本地盘 8-16）。缩略图生成（读原图解码）和 BLAKE3 哈希（流式读原图）都会读原图，
//! 二者共用同一组闸门，按 root 类型分流，从而约束**总**并发，而不是各管各的。
//!
//! dHash 读的是本地缩略图缓存，不读原图，因此不走这里。

use std::sync::{Arc, OnceLock};

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::error::{AppError, AppResult};

/// 本地盘原图读取并发上限。
const LOCAL_READ_CONCURRENCY: usize = 8;
/// 网络盘原图读取并发上限（红线：超过 4 会拖死 NAS）。
const NETWORK_READ_CONCURRENCY: usize = 4;

static LOCAL_GATE: OnceLock<Arc<Semaphore>> = OnceLock::new();
static NETWORK_GATE: OnceLock<Arc<Semaphore>> = OnceLock::new();

fn gate(is_network: bool) -> Arc<Semaphore> {
    if is_network {
        NETWORK_GATE
            .get_or_init(|| Arc::new(Semaphore::new(NETWORK_READ_CONCURRENCY)))
            .clone()
    } else {
        LOCAL_GATE
            .get_or_init(|| Arc::new(Semaphore::new(LOCAL_READ_CONCURRENCY)))
            .clone()
    }
}

/// 申请一个原图读取许可。持有 `OwnedSemaphorePermit` 期间占用一个并发名额，drop 即释放。
/// 调用方应在打开/读取原图之前 `await`，并把 permit 持有到读取结束。
pub async fn acquire_read(is_network: bool) -> AppResult<OwnedSemaphorePermit> {
    gate(is_network)
        .acquire_owned()
        .await
        .map_err(|_| AppError::Other("原图读取限流器已关闭".to_string()))
}
