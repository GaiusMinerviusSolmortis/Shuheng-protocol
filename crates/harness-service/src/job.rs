//! **杀一棵进程树，不是杀一个进程** —— 以及给它上硬限额。
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
//! ---
//!
//! **2026-09-20 从 `harness-term` 提上来的**：原来它只服务终端，而**同一个教训
//! 在第二个地方**又出现了一次 —— MCP 桥拉起的那个子进程是**别人写的**，
//! 它一个 `child.kill()` 只杀得掉自己那一层，**孙子进程会漏**。
//! 抽进公共件的理由和围栏、域锁一样：**这类代码有两份就一定会漂移**。
//!
//! ---
//!
//! **2026-09-20：顺手把"资源面"那一面做成了真的**（§5.8 决定二那张表里的第四行）。
//!
//! 在这之前终端的资源面只有"超时"—— 而超时是**墙钟**：一条命令可以在 30 秒里
//! 吃掉 8 GB 内存、起 2000 个进程，然后**正常地**在超时前结束。
//! 现在可以给它三个硬限额（**都关在同一个 job 上，超了由内核直接杀掉整棵树**）：
//!
//! | 限额 | 内核那一格 |
//! |---|---|
//! | 内存 | `JOB_OBJECT_LIMIT_PROCESS_MEMORY`（**单进程**峰值）|
//! | 进程数 | `JOB_OBJECT_LIMIT_ACTIVE_PROCESS`（job 里同时活着几个）|
//! | CPU 时间 | `JOB_OBJECT_LIMIT_JOB_TIME`（**CPU 时间**，不是墙钟）|
//!
//! ⚠️ **它们默认不开**：多严算"合理"是 profile 的事（待定 #3），
//! 而"默认开一个我拍的数"会在正常需求上碰壁。开了才报 `resources: "limits"`。
//!
//! ⚠️ **三个限额"发作"的方式不一样，别混着说**（第一版我就写错了）：
//!
//! | 限额 | 超了会怎样 | 我们能看见什么 |
//! |---|---|---|
//! | **内存** | **命令自己分配失败**（提交内存到顶，`malloc`/`new` 失败）—— **内核不杀它** | 峰值内存（离上限还有多远）；**看不到"被杀"**，因为它不是被杀 |
//! | **进程数** | 再 `CreateProcess` **失败**（命令自己报错） | `TotalProcesses`（累计成功起过几个）|
//! | **CPU 时间** | **内核到点终止整个 job** —— 这一条是**杀** | 用户 + 内核 CPU 时间（会顶到上限）|
//!
//! 所以判词分两半，而且分开说：**测量到的数**（每次都报，它是事实）和
//! **推断**（只在有证据时才说，并且标上"推断"）。
//! 把内存那条写成"很可能撞了内存被杀"是错的 —— 它根本没被杀，是它自己分配不到。
//!
//! 非 Windows 上这一版是空的：`kill()` 退回只杀一层，**这是已知短板**，
//! 记在 manifest 的 `not_implemented` 里更诚实。

use std::process::Child;

/// 给一棵进程树上的硬限额。**三个都是"关在 Job 上、由内核执行"**，
/// 不是我们在读输出时自己数出来的那种软限制。
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Limits {
    /// 单进程峰值内存上限（字节）
    pub mem_bytes: Option<u64>,
    /// job 里同时活着的进程数上限
    pub max_procs: Option<u32>,
    /// **CPU 时间**上限（毫秒，不是墙钟）
    pub cpu_time_ms: Option<u64>,
}

impl Limits {
    pub fn any(&self) -> bool {
        self.mem_bytes.is_some() || self.max_procs.is_some() || self.cpu_time_ms.is_some()
    }
}

/// 一次运行结束之后，job 账本上留下的痕迹 —— **用来解释"它为什么死了"**。
///
/// ⚠️ 它是**证据**，不是**结论**：峰值接近上限**支持**"撞上限"这个解释，
/// 但内核没告诉我们是它杀的（那要完成端口通知，没做）。所以措辞必须是"很可能"。
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct JobAccount {
    /// 峰值内存（字节）
    pub peak_mem_bytes: u64,
    /// 曾经有过的进程总数
    pub total_processes: u32,
    /// **用掉的 CPU 时间**（毫秒）—— 用户态 + 内核态。
    /// 它是唯一一条"超了就是被杀"的证据：`JOB_OBJECT_LIMIT_JOB_TIME` 到点终止整个 job。
    pub cpu_time_ms: u64,
}

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
        AssignProcessToJobObject, CreateJobObjectW, JobObjectBasicAccountingInformation,
        JobObjectExtendedLimitInformation, QueryInformationJobObject, SetInformationJobObject,
        TerminateJobObject, JOBOBJECT_BASIC_ACCOUNTING_INFORMATION,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_ACTIVE_PROCESS,
        JOB_OBJECT_LIMIT_JOB_TIME, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOB_OBJECT_LIMIT_PROCESS_MEMORY,
    };

    impl Job {
        /// 把刚起的子进程放进一个新的 job。
        ///
        /// 失败时返回 `None` —— 调用方**照常跑**，只是杀的时候只能杀一层。
        /// 不在这里报错是因为：装不进 job 是个环境限制（比如我们自己在某个
        /// 不允许 breakaway 的 job 里），不是这次调用的错。
        pub fn assign(child: &Child, limits: Limits) -> Option<Job> {
            unsafe {
                let handle = CreateJobObjectW(std::ptr::null(), std::ptr::null());
                if handle.is_null() {
                    return None;
                }
                let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
                // **先立规矩，再让它跑。** 限额和 KILL_ON_JOB_CLOSE 是同一次调用 ——
                // 分两次的话，中间那一瞬它已经在不受限地跑了。
                info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                if let Some(b) = limits.mem_bytes {
                    info.BasicLimitInformation.LimitFlags |= JOB_OBJECT_LIMIT_PROCESS_MEMORY;
                    info.ProcessMemoryLimit = b as usize;
                }
                if let Some(n) = limits.max_procs {
                    info.BasicLimitInformation.LimitFlags |= JOB_OBJECT_LIMIT_ACTIVE_PROCESS;
                    info.BasicLimitInformation.ActiveProcessLimit = n;
                }
                if let Some(ms) = limits.cpu_time_ms {
                    info.BasicLimitInformation.LimitFlags |= JOB_OBJECT_LIMIT_JOB_TIME;
                    // 每一格 100 纳秒（Windows 的 FILETIME 单位）
                    info.BasicLimitInformation.PerJobUserTimeLimit =
                        (ms as i64).saturating_mul(10_000);
                }
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

        /// 事后查账：**峰值内存**和**它一共起过几个进程**。
        ///
        /// ⚠️ **要读两个结构**（第一版写错在这儿，编译期就红了）：
        /// `PeakProcessMemoryUsed` 在 **extended limit** 那个结构里
        /// （它名字像"限额"，其实夹带了计数），而 `TotalProcesses` 在
        /// **basic accounting** 里。读错结构的后果不是崩，是**一直返回 0** ——
        /// 而"峰值 0"会被当成"没有证据"，于是那句"很可能撞了内存"永远说不出来。
        pub fn account(&self) -> JobAccount {
            let mut acc: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = unsafe { std::mem::zeroed() };
            let ok_acc = unsafe {
                QueryInformationJobObject(
                    self.handle,
                    JobObjectBasicAccountingInformation,
                    &mut acc as *mut _ as *mut core::ffi::c_void,
                    std::mem::size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                    std::ptr::null_mut(),
                )
            };
            let mut ext: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
            let ok_ext = unsafe {
                QueryInformationJobObject(
                    self.handle,
                    JobObjectExtendedLimitInformation,
                    &mut ext as *mut _ as *mut core::ffi::c_void,
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                    std::ptr::null_mut(),
                )
            };
            if ok_acc == 0 && ok_ext == 0 {
                return JobAccount::default();
            }
            JobAccount {
                peak_mem_bytes: if ok_ext == 0 {
                    0
                } else {
                    ext.PeakProcessMemoryUsed as u64
                },
                total_processes: if ok_acc == 0 { 0 } else { acc.TotalProcesses },
                // FILETIME 单位是 100 纳秒 ⇒ /10_000 得毫秒
                cpu_time_ms: if ok_acc == 0 {
                    0
                } else {
                    ((acc.TotalUserTime as u64).saturating_add(acc.TotalKernelTime as u64))
                        / 10_000
                },
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
    pub fn assign(_child: &Child, _limits: Limits) -> Option<Job> {
        None
    }
    pub fn terminate(&self) {}
    /// 非 Windows 这一版是空的 —— **如实返回零**，别假装查到了账
    /// （同文件头那句"kill() 退回只杀一层，这是已知短板"）。
    pub fn account(&self) -> JobAccount {
        JobAccount::default()
    }
}
