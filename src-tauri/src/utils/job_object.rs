//! Windows 作业对象（docs/11 §6.4、R1-4）：把 AI worker、llama-server 这类子进程绑到本进程的
//! 生命周期上。
//!
//! 只靠 `kill_on_drop` 不够：主进程崩溃或被任务管理器结束时 drop 不会执行，子进程会残留，继续
//! 占着显存（llama-server 约 7GB）。作业对象带 `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`：作业句柄随
//! 本进程关闭时，系统结束作业里的全部进程。参考 memora 的 `winproc.py`。

#[cfg(windows)]
mod imp {
    use std::os::windows::io::RawHandle;
    use std::sync::OnceLock;

    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    /// 作业句柄（以整数保存，`HANDLE` 不是 Send/Sync）。进程退出时由系统关闭。
    static JOB: OnceLock<Option<isize>> = OnceLock::new();

    fn job() -> Option<HANDLE> {
        JOB.get_or_init(|| {
            // SAFETY：标准的创建 + 设置限制调用，失败时返回 None，不影响子进程运行。
            unsafe {
                let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
                if job.is_null() {
                    return None;
                }
                let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
                info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                let ok = SetInformationJobObject(
                    job,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as *const core::ffi::c_void,
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                );
                if ok == 0 {
                    return None;
                }
                Some(job as isize)
            }
        })
        .map(|handle| handle as HANDLE)
    }

    pub fn bind(process: RawHandle) -> bool {
        let Some(job) = job() else {
            return false;
        };
        // SAFETY：process 是刚创建的子进程句柄，job 是本进程持有的作业句柄。
        unsafe { AssignProcessToJobObject(job, process as HANDLE) != 0 }
    }
}

/// 把子进程放进本进程的作业对象。失败返回 false（不影响运行，只是失去"主进程崩溃时连带结束"）。
#[cfg(windows)]
pub fn bind_child(child: &tokio::process::Child) -> bool {
    match child.raw_handle() {
        Some(handle) => imp::bind(handle),
        None => false,
    }
}

#[cfg(not(windows))]
pub fn bind_child(_child: &tokio::process::Child) -> bool {
    false
}
