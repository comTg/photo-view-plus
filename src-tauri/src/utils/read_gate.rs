//! 原图读取并发限流闸门。
//!
//! 网络盘（SMB / 映射盘）并发读过高会拖死 NAS（CLAUDE.md 红线 6：网络盘并发上限 4，
//! 本地盘 8-16）。缩略图生成（读原图解码）和 BLAKE3 哈希（流式读原图）都会读原图，
//! 二者共用同一组闸门，按 root 类型分流，从而约束**总**并发，而不是各管各的。
//!
//! dHash 读的是本地缩略图缓存，不读原图，因此不走这里。

use std::sync::{Arc, OnceLock};

use crate::error::AppResult;
use crate::utils::limiter::{AdjustableLimiter, LimiterPermit};

/// 本地盘原图读取并发上限。
const LOCAL_READ_CONCURRENCY: usize = 8;
/// 网络盘原图读取并发上限（红线：超过 4 会拖死 NAS）。资源治理器只能在 1..=4 之间调。
pub const NETWORK_READ_CONCURRENCY: usize = 4;

static LOCAL_GATE: OnceLock<Arc<AdjustableLimiter>> = OnceLock::new();
static NETWORK_GATE: OnceLock<Arc<AdjustableLimiter>> = OnceLock::new();

fn gate(is_network: bool) -> Arc<AdjustableLimiter> {
    if is_network {
        NETWORK_GATE
            .get_or_init(|| AdjustableLimiter::new(NETWORK_READ_CONCURRENCY))
            .clone()
    } else {
        LOCAL_GATE
            .get_or_init(|| AdjustableLimiter::new(LOCAL_READ_CONCURRENCY))
            .clone()
    }
}

/// 资源治理器按负载模式调整网络盘读原图并发（docs/11 §5.2），夹在 1..=4（红线 6）。
pub fn set_network_limit(limit: usize) {
    gate(true).set_limit(limit.clamp(1, NETWORK_READ_CONCURRENCY));
}

pub fn network_limit() -> usize {
    gate(true).limit()
}

/// 申请一个原图读取许可。持有期间占用一个并发名额，drop 即释放。
/// 调用方应在打开/读取原图之前 `await`，并把 permit 持有到读取结束。
pub async fn acquire_read(is_network: bool) -> AppResult<LimiterPermit> {
    Ok(gate(is_network).acquire().await)
}
