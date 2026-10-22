//! 线程级"后台模式"（docs/11 §5.3、R1-3）。
//!
//! Windows 的 `THREAD_MODE_BACKGROUND_BEGIN` 同时压低线程的 CPU、磁盘 I/O 和内存优先级：
//! 缩略图解码、BLAKE3 读整文件、dHash、扫描读图头这类重活进入后台模式后，即使把 CPU 跑满，
//! 前台 UI 和其他应用也不卡。注意别把写数据库的事务放进后台模式：I/O 优先级被压低后持锁时间
//! 变长，反而拖住其他线程（它们的 busy_timeout 只有 5 秒）。

/// 进入后台模式，drop 时恢复。非 Windows 上是空操作。必须在干重活的那个线程上创建
/// （例如 `spawn_blocking` 的闭包里）。
pub struct BackgroundPriorityGuard;

impl BackgroundPriorityGuard {
    pub fn enter() -> Self {
        #[cfg(windows)]
        unsafe {
            use windows_sys::Win32::System::Threading::{
                GetCurrentThread, SetThreadPriority, THREAD_MODE_BACKGROUND_BEGIN,
            };
            // 失败极少见（如线程已在后台模式），失败就按普通优先级跑，不影响正确性。
            let _ = SetThreadPriority(GetCurrentThread(), THREAD_MODE_BACKGROUND_BEGIN);
        }
        Self
    }
}

impl Drop for BackgroundPriorityGuard {
    fn drop(&mut self) {
        #[cfg(windows)]
        unsafe {
            use windows_sys::Win32::System::Threading::{
                GetCurrentThread, SetThreadPriority, THREAD_MODE_BACKGROUND_END,
            };
            let _ = SetThreadPriority(GetCurrentThread(), THREAD_MODE_BACKGROUND_END);
        }
    }
}
