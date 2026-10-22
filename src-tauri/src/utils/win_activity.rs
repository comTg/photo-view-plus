//! 用户活动探测（docs/11 §5.1）：多久没有键盘鼠标输入、是否全屏 / 游戏 / 演示、是否锁屏。
//!
//! 都是读本机 API（不是前端轮询），每次调用几微秒。非 Windows 平台用桩实现：报告"正在使用"，
//! 后台任务按最保守的档位跑。

use std::time::Duration;

/// 系统的通知状态（`SHQueryUserNotificationState`）里对后台调度有意义的部分。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresenceState {
    /// 正常桌面。
    Normal,
    /// 全屏应用、D3D 全屏游戏或演示模式：后台让出全部资源。
    Busy,
    /// 锁屏、屏保或切换到了别的用户会话：等同于空闲。
    NotPresent,
}

/// 一次采样。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActivitySnapshot {
    /// 距最后一次键盘 / 鼠标输入。
    pub idle_for: Duration,
    pub presence: PresenceState,
}

/// 活动来源。线上是 [`SystemActivity`]，测试里注入假实现。
pub trait ActivitySource: Send + Sync {
    fn snapshot(&self) -> ActivitySnapshot;
}

/// 读真实系统状态。
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemActivity;

impl ActivitySource for SystemActivity {
    fn snapshot(&self) -> ActivitySnapshot {
        ActivitySnapshot {
            idle_for: idle_for(),
            presence: presence(),
        }
    }
}

#[cfg(windows)]
fn idle_for() -> Duration {
    use windows_sys::Win32::System::SystemInformation::GetTickCount;
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};
    let mut info = LASTINPUTINFO {
        cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32,
        dwTime: 0,
    };
    // SAFETY：传入正确初始化了 cbSize 的结构体指针。
    let ok = unsafe { GetLastInputInfo(&mut info) };
    if ok == 0 {
        return Duration::ZERO;
    }
    // GetTickCount 约 49.7 天回绕一次，用回绕减法即可。
    let now = unsafe { GetTickCount() };
    Duration::from_millis(u64::from(now.wrapping_sub(info.dwTime)))
}

#[cfg(not(windows))]
fn idle_for() -> Duration {
    Duration::ZERO
}

#[cfg(windows)]
fn presence() -> PresenceState {
    use windows_sys::Win32::UI::Shell::{
        SHQueryUserNotificationState, QUNS_BUSY, QUNS_NOT_PRESENT, QUNS_PRESENTATION_MODE,
        QUNS_RUNNING_D3D_FULL_SCREEN,
    };
    let mut state = 0;
    // SAFETY：传入有效的输出指针。
    let hr = unsafe { SHQueryUserNotificationState(&mut state) };
    if hr != 0 {
        return PresenceState::Normal;
    }
    match state {
        QUNS_BUSY | QUNS_RUNNING_D3D_FULL_SCREEN | QUNS_PRESENTATION_MODE => PresenceState::Busy,
        QUNS_NOT_PRESENT => PresenceState::NotPresent,
        _ => PresenceState::Normal,
    }
}

#[cfg(not(windows))]
fn presence() -> PresenceState {
    PresenceState::Normal
}
