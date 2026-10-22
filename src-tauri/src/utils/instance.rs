//! 进程间的单实例标记（Windows 命名互斥量，会话内可见）：夜间后台分析（docs/10 A6b）据此判断
//! 主程序是否开着、自己是否已经在跑。进程退出（包括崩溃）时系统自动释放，不会残留。

/// 持有期间别的进程能看到这个名字；drop 时释放。
pub struct InstanceLock {
    #[cfg(windows)]
    handle: isize,
}

#[cfg(windows)]
mod imp {
    use windows_sys::Win32::Foundation::{
        CloseHandle, GetLastError, ERROR_ACCESS_DENIED, ERROR_ALREADY_EXISTS,
    };
    use windows_sys::Win32::System::Threading::{CreateMutexW, OpenMutexW};

    /// SYNCHRONIZE：只为探测是否存在。
    const SYNCHRONIZE: u32 = 0x0010_0000;

    fn wide(name: &str) -> Vec<u16> {
        name.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// 新建命名互斥量；`exclusive` 时已存在（别的进程持有）就返回 None，
    /// 否则即使已存在也保留句柄（多个进程一起"持有"这个名字）。
    pub fn create(name: &str, exclusive: bool) -> Option<isize> {
        let name = wide(name);
        // SAFETY：name 是以 0 结尾的 UTF-16 字符串；返回的句柄由调用方负责关闭。
        unsafe {
            let handle = CreateMutexW(std::ptr::null(), 0, name.as_ptr());
            if handle.is_null() {
                // 别的权限级别（例如管理员运行的主程序）建的同名对象：打不开，但它确实存在。
                return None;
            }
            if exclusive && GetLastError() == ERROR_ALREADY_EXISTS {
                CloseHandle(handle);
                return None;
            }
            Some(handle as isize)
        }
    }

    pub fn exists(name: &str) -> bool {
        let name = wide(name);
        // SAFETY：同上；打开成功就立即关闭。
        unsafe {
            let handle = OpenMutexW(SYNCHRONIZE, 0, name.as_ptr());
            if handle.is_null() {
                // 对象存在但没有权限打开（主程序以管理员身份运行、计划任务是普通权限）也算存在。
                return GetLastError() == ERROR_ACCESS_DENIED;
            }
            CloseHandle(handle);
            true
        }
    }

    pub fn close(handle: isize) {
        // SAFETY：handle 来自 create。
        unsafe {
            CloseHandle(handle as windows_sys::Win32::Foundation::HANDLE);
        }
    }
}

impl InstanceLock {
    /// 独占一个名字；别的进程已占用时返回 None。非 Windows 平台总是成功（不做检测）。
    pub fn acquire(name: &str) -> Option<Self> {
        #[cfg(windows)]
        {
            imp::create(name, true).map(|handle| Self { handle })
        }
        #[cfg(not(windows))]
        {
            let _ = name;
            Some(Self {})
        }
    }

    /// 共同持有一个名字（不排他）：开了两个主程序窗口时，关掉其中一个，另一个仍然"在"。
    pub fn hold(name: &str) -> Option<Self> {
        #[cfg(windows)]
        {
            imp::create(name, false).map(|handle| Self { handle })
        }
        #[cfg(not(windows))]
        {
            let _ = name;
            Some(Self {})
        }
    }

    /// 这个名字现在是否被（任何进程）占用。
    pub fn is_held(name: &str) -> bool {
        #[cfg(windows)]
        {
            imp::exists(name)
        }
        #[cfg(not(windows))]
        {
            let _ = name;
            false
        }
    }
}

impl Drop for InstanceLock {
    fn drop(&mut self) {
        #[cfg(windows)]
        imp::close(self.handle);
    }
}
