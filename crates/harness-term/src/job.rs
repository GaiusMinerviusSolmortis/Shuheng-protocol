//! **杀一棵进程树，不是杀一个进程。**
//!
//! 这条是被 smoke 逼出来的：`run` 走的是 `cmd /C <命令>`，所以我们手里的
//! 进程是 **shell**，不是真正干活的那个。超时的时候 `Child::kill()` 杀掉的只是
//! `cmd.exe` —— 它启动的 `ping.exe` 还活着，**而且继续攥着 stdout 管道**。
//! 于是读线程永远等不到 EOF，那条命令的结果永远回不来（表面上像"卡死了"）。
//!
//! 正解是 Windows 的 **Job Object**：把子进程放进一个 job，
//! 设上 `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`，要停的时候
//! `TerminateJobObject` 一次带走整棵树 —— 而且**句柄一关就自动清干净**，
//! 不会留下孤儿进程（同 §5.15 那条"别让用户手工收尸"）。
//!
//! 非 Windows 上这一版是空的：`kill()` 退回只杀一层，**这是已知短板**，
//! 记在 manifest 的 `not_implemented` 里更诚实。

use std::process::Child;

#[cfg(windows)]
pub struct Job {
    handle: *mut core::ffi::c_void,
}

#[cfg(windows)]
mod imp {
    use super::*;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, TerminateJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    impl Job {
        /// 把刚起的子进程放进一个新的 job。
        ///
        /// 失败时返回 `None` —— 调用方**照常跑**，只是杀的时候只能杀一层。
        /// 不在这里报错是因为：装不进 job 是个环境限制（比如我们自己在某个
        /// 不允许 breakaway 的 job 里），不是这次调用的错。
        pub fn assign(child: &Child) -> Option<Job> {
            unsafe {
                let handle = CreateJobObjectW(std::ptr::null(), std::ptr::null());
                if handle.is_null() {
                    return None;
                }
                let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
                info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                let ok = SetInformationJobObject(
                    handle,
                    JobObjectExtendedLimitInformation,
                    &mut info as *mut _ as *mut core::ffi::c_void,
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                );
                if ok == 0 {
                    return None;
                }
                if AssignProcessToJobObject(handle, child.as_raw_handle() as HANDLE) == 0 {
                    return None;
                }
                Some(Job { handle })
            }
        }

        /// 一次带走整棵树。
        pub fn terminate(&self) {
            if !self.handle.is_null() {
                unsafe {
                    TerminateJobObject(self.handle, 1);
                }
            }
        }
    }

    impl Drop for Job {
        fn drop(&mut self) {
            // KILL_ON_JOB_CLOSE：句柄一关，job 里剩下的东西跟着走。
            // 所以连"忘了显式 terminate"也不会留孤儿。
            self.terminate();
            unsafe {
                windows_sys::Win32::Foundation::CloseHandle(self.handle);
            }
        }
    }
}

#[cfg(not(windows))]
pub struct Job;

#[cfg(not(windows))]
impl Job {
    pub fn assign(_child: &Child) -> Option<Job> {
        None
    }
    pub fn terminate(&self) {}
}
