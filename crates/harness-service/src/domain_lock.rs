//! **域锁：同一个域只能有一个服务进程。**
//!
//! 为什么需要它 —— 以及为什么它**不该由中控来做**（2026-09-19 你纠正的）：
//!
//! 锁表活在服务进程里。所以拉起两个服务托管同一条路径，就是两张互不知情的锁表，
//! 互斥直接失效。我第一反应是"把锁表上移到中控" —— **那是错的**：
//!
//! > 中控去仲裁工具内部的锁，等于让内核去懂每个工具的内部语义。
//! > 而"工具服务自己统筹自己的域"正是 §5.10 的前提。
//!
//! 所以责任在**应用**：它必须**在注册/启动时**就发现"这个域已经有主了"，
//! 并且**及时挡住**，而不是等着越界发生。
//!
//! 机制的选择标准是 §5.15 那条 —— **持有者死了锁要能自己释放，不能要用户手工删**：
//!
//! | 平台 | 机制 | 崩溃后 |
//! |---|---|---|
//! | Windows | **命名互斥体**（`CreateMutexW`） | **内核自动释放**，不留残留 |
//! | 其他 | 独占创建的锁文件 + 退出时删除 | 进程被杀会留残留（已知短板）|

use std::path::Path;

/// 拿住的域锁。**活到进程结束**（Drop 时释放）。
pub struct DomainLock {
    #[cfg(not(windows))]
    path: std::path::PathBuf,
    #[cfg(windows)]
    _handle: isize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Conflict {
    pub why: String,
    pub remedy: String,
}

/// 把一条规范化路径折成一个稳定的短标识。
///
/// 用自己的 FNV-1a，而不是 `DefaultHasher` —— 后者只承诺"同一次运行内一致"，
/// 而这里要的是**跨进程**一致（两个服务进程得算出同一个名字）。
/// Windows 路径不区分大小写，所以先折成小写。
pub fn tag(path: &Path) -> String {
    let mut s = path.to_string_lossy().to_lowercase();
    // 统一分隔符：同一个目录的两种写法必须撞到同一个名字上
    s = s.replace('\\', "/");
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    format!("{h:016x}")
}

#[cfg(windows)]
mod imp {
    use super::*;
    use windows_sys::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
    use windows_sys::Win32::System::Threading::CreateMutexW;

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    pub fn acquire(domain: &str, fences: &[std::path::PathBuf]) -> Result<DomainLock, Conflict> {
        for f in fences {
            let name = format!("Local\\instance-harness-{domain}-{}", tag(f));
            let name = wide(&name);
            let handle = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
            if handle.is_null() {
                return Err(Conflict {
                    why: format!("给 `{}` 申请域锁失败（系统没给句柄）", f.display()),
                    remedy: "这是环境问题，不是配置问题；把这条报出来".to_string(),
                });
            }
            if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
                return Err(Conflict {
                    why: format!("`{}` 已经有另一个 `{domain}` 服务在管了（域锁被占）", f.display()),
                    remedy: "同一个域只能有一个服务进程 —— 这是部署配错了：\
                             要么让中控只拉起一个，要么把另一份托管到别的路径。\
                             （对方进程退出时锁会被操作系统自动释放，不用手工清）"
                        .to_string(),
                });
            }
            // 拿到就**故意不释放句柄**：它活到进程结束，
            // 届时由内核回收 —— 这正是"持有者死了锁自动释放"。
            return Ok(DomainLock { _handle: handle as isize });
        }
        // 没有围栏 = 没有域要占（这种实例本来什么也干不了）
        Ok(DomainLock { _handle: 0 })
    }
}

#[cfg(not(windows))]
mod imp {
    use super::*;

    pub fn acquire(domain: &str, fences: &[std::path::PathBuf]) -> Result<DomainLock, Conflict> {
        for f in fences {
            let path = std::env::temp_dir()
                .join("instance-harness-domain-locks")
                .join(format!("{domain}-{}.lock", tag(f)));
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(mut fh) => {
                    use std::io::Write;
                    let _ = writeln!(fh, "pid={}", std::process::id());
                    return Ok(DomainLock { path });
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    return Err(Conflict {
                        why: format!(
                            "`{}` 已经有另一个 `{domain}` 服务在管了（锁文件 {} 存在）",
                            f.display(),
                            path.display()
                        ),
                        remedy: "同一个域只能有一个服务进程；确认对方已经退出后，\
                                 删掉那个锁文件再启动（Windows 上没有这一步）"
                            .to_string(),
                    })
                }
                Err(e) => {
                    return Err(Conflict {
                        why: format!("给 `{}` 申请域锁失败：{e}", f.display()),
                        remedy: "检查临时目录能不能写".to_string(),
                    })
                }
            }
        }
        Ok(DomainLock {
            path: std::path::PathBuf::new(),
        })
    }
}

#[cfg(not(windows))]
impl Drop for DomainLock {
    fn drop(&mut self) {
        if !self.path.as_os_str().is_empty() {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

pub use imp::acquire;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tag_is_stable_across_calls_and_case_insensitive() {
        let a = tag(Path::new(r"D:\Proj\src"));
        let b = tag(Path::new(r"d:/proj/src"));
        assert_eq!(a, b, "同一个目录的两种写法必须撞到同一个名字");
        assert_eq!(a.len(), 16);
    }

    #[test]
    fn different_paths_get_different_tags() {
        assert_ne!(tag(Path::new("D:/a")), tag(Path::new("D:/b")));
    }

    #[test]
    fn no_fence_means_no_domain_to_hold() {
        let lock = acquire("test", &[]).expect("没有围栏时不该冲突");
        drop(lock);
    }
}
