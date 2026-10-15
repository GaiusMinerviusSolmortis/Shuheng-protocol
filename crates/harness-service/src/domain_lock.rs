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
//!
//! ## ⚠️ 2026-09-20 审计抓到：**只占了第一条围栏**
//!
//! 这里原来是 `for f in fences { … return … }` —— 循环体第一条就 `return`，
//! 于是**只有 `roots()[0]`** 被占上。而 `--fence` 是可重复的、
//! manifest 里也写着 `owns.exclusive_per: "fence"`，所以
//! **[A,B] 与 [B,C] 各起一个编辑器时两个都拿到锁**，两个都在管 B。
//!
//! 那个洞的可怕之处不是"能同时起两个"，而是**它看起来一切正常**：
//! 两个进程都自报"域锁拿到"，各自的进程内写锁表互不知情 ——
//! §5.15 那套"实例是锁点"的写互斥在这个组合下等于没有。
//!
//! 现在的规则（三条缺一不可）：
//!
//! 1. **每一条围栏都要占**（不是"找一个能占的"）；
//! 2. **按锁名排序后再依次取** —— 两个都要多条围栏的服务如果取锁顺序不一致，
//!    就会 A 等 B、B 等 A **互相锁死**（这是这次改动**新引入**的风险，必须一起处理）；
//! 3. **同一个根写两遍算一条**（按 tag 去重）—— 否则 `--fence X --fence X`
//!    会自己跟自己撞（同一个进程里第二次 `CreateMutexW` 照样报 `ERROR_ALREADY_EXISTS`）。
//!
//! 为什么以前没抓到：**验收脚本里没有一条用过两个 `--fence`**
//! （`smoke-editor` 那组域冲突判据用的是**完全相同**的两条围栏）。
//! 现在是 `scripts/smoke-domainlock.ps1`，而且它**在老代码上确实是红的**
//! （第 4/5/6 条），修完才绿。

use std::path::Path;

/// 拿住的域锁。**活到进程结束**（Drop 时释放）。
pub struct DomainLock {
    #[cfg(not(windows))]
    paths: Vec<std::path::PathBuf>,
    #[cfg(windows)]
    _handles: Vec<*mut core::ffi::c_void>,
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

/// **取锁清单**：按 tag 去重，再按名字排序。
///
/// 两条都必要（见文件头那三条规则）：
/// * **去重**：同一个根写两遍（或者同一个目录的两种写法）不该自己跟自己撞；
/// * **排序**：多围栏的服务之间取锁顺序必须一致，否则会互相锁死。
///
/// 返回 `(显示用的路径, 锁的名字)` —— 报错时要能说清**是哪一条**被占了，
/// 所以显示用的那条不能省。
fn lock_plan(fences: &[std::path::PathBuf], named: impl Fn(&Path) -> String) -> Vec<(String, String)> {
    let mut want: Vec<(String, String)> = Vec::new();
    for f in fences {
        let name = named(f);
        if !want.iter().any(|(_, n)| n == &name) {
            want.push((f.display().to_string(), name));
        }
    }
    want.sort_by(|a, b| a.1.cmp(&b.1));
    want
}

#[cfg(windows)]
mod imp {
    use super::*;
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS};
    use windows_sys::Win32::System::Threading::CreateMutexW;

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    pub fn acquire(domain: &str, fences: &[std::path::PathBuf]) -> Result<DomainLock, Conflict> {
        let plan = lock_plan(fences, |f| {
            // ⚠️ 名字里的 `instance-harness` 是**故意留着**的（2026-10-15 项目改名时没动它）。
            // 锁名是"跨进程互斥"的**命名空间**：改了名字，新旧两代进程就互相看不见
            // 对方手里的锁 —— 一个还开着的旧编辑器 + 一个新编译器会同时以为自己独占。
            // 换它要挑一个"没有旧进程在跑"的时刻，那是一次**有代价的迁移**，不是改个字符串。
            format!("Local\\instance-harness-{domain}-{}", tag(f))
        });
        let mut handles: Vec<*mut core::ffi::c_void> = Vec::new();
        for (shown, name) in plan {
            let name = wide(&name);
            let handle = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
            if handle.is_null() {
                release(&handles);
                return Err(Conflict {
                    why: format!("给 `{shown}` 申请域锁失败（系统没给句柄）"),
                    remedy: "这是环境问题，不是配置问题；把这条报出来".to_string(),
                });
            }
            // **先收进清单再判**：撞上时这一个句柄也要还回去 ——
            // `ERROR_ALREADY_EXISTS` 说的是"这个名字已经有主"，
            // 但我们手里仍然多了一个指向它的句柄。
            handles.push(handle);
            if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
                release(&handles);
                return Err(Conflict {
                    why: format!("`{shown}` 已经有另一个 `{domain}` 服务在管了（域锁被占）"),
                    remedy: "同一个域只能有一个服务进程 —— 这是部署配错了：\
                             要么让中控只拉起一个，要么把另一份托管到别的路径。\
                             （对方进程退出时锁会被操作系统自动释放，不用手工清）"
                        .to_string(),
                });
            }
        }
        // 拿到就**故意不释放句柄**：它们活到进程结束，
        // 届时由内核回收 —— 这正是"持有者死了锁自动释放"。
        // 没有围栏时 `handles` 是空的 ⇒ 没有域要占（这种实例本来什么也干不了）。
        Ok(DomainLock { _handles: handles })
    }

    /// 半路失败时**把已经占上的都还回去** —— 否则一次失败的启动会留下半套锁：
    /// 别人既起不来，也不知道是被谁挡的。
    fn release(handles: &[*mut core::ffi::c_void]) {
        for h in handles {
            unsafe {
                CloseHandle(*h);
            }
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use super::*;

    pub fn acquire(domain: &str, fences: &[std::path::PathBuf]) -> Result<DomainLock, Conflict> {
        let plan = lock_plan(fences, |f| {
            std::env::temp_dir()
                .join("instance-harness-domain-locks")
                .join(format!("{domain}-{}.lock", tag(f)))
                .display()
                .to_string()
        });
        let mut paths: Vec<std::path::PathBuf> = Vec::new();
        for (shown, lock_name) in plan {
            let path = std::path::PathBuf::from(&lock_name);
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
                    paths.push(path);
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    for p in &paths {
                        let _ = std::fs::remove_file(p);
                    }
                    return Err(Conflict {
                        why: format!(
                            "`{shown}` 已经有另一个 `{domain}` 服务在管了（锁文件 {} 存在）",
                            path.display()
                        ),
                        remedy: "同一个域只能有一个服务进程；确认对方已经退出后，\
                                 删掉那个锁文件再启动（Windows 上没有这一步）"
                            .to_string(),
                    })
                }
                Err(e) => {
                    for p in &paths {
                        let _ = std::fs::remove_file(p);
                    }
                    return Err(Conflict {
                        why: format!("给 `{shown}` 申请域锁失败：{e}"),
                        remedy: "检查临时目录能不能写".to_string(),
                    })
                }
            }
        }
        Ok(DomainLock { paths })
    }
}

#[cfg(not(windows))]
impl Drop for DomainLock {
    fn drop(&mut self) {
        for p in &self.paths {
            let _ = std::fs::remove_file(p);
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

    /// 每个测试用自己的目录：命名互斥体是**全机器**可见的，而 cargo 并行跑测试。
    fn a_dir(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "harness-domain-lock-test-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::create_dir_all(&d);
        d
    }

    /// **2026-09-20 审计抓到的那条**：只占第一条围栏。
    ///
    /// `[A,B]` 手里的 A、B 都得占上 —— 于是只要**任意一条**撞上就该拒绝。
    /// **这条测试在老代码上是红的**：老代码只占 A，
    /// 所以第二个进程（第一条是 C）看 A 没主就整台起来了，然后两个都在管 C。
    #[test]
    fn a_second_acquire_over_a_shared_root_is_refused() {
        let (a, b, c) = (a_dir("shared-a"), a_dir("shared-b"), a_dir("shared-c"));
        let first = acquire("chain", &[a.clone(), b.clone()]).expect("第一份该拿到");
        let second = acquire("chain", &[c.clone(), b.clone()]);
        match second {
            Err(Conflict { why, .. }) => assert!(
                why.contains(&b.display().to_string()),
                "报错要说清**是哪一条**被占了（应该是第二条）：{why}"
            ),
            Ok(_) => panic!("[C,B] 不该拿到锁 —— B 已经有主了"),
        }
        // **失败的启动不许留下半套锁**：C 如果被它占过，这会儿必须已经还回去了。
        let third = acquire("chain", &[c]).expect("被拒的那次不许留下 C 的残锁");
        drop(first);
        drop(third);
        // ⚠️ 注意：**进程内 `drop` 不释放**（我们故意不关句柄，靠进程退出让内核回收，
        // 见文件头那条标准）。所以"持有者没了锁要跟着放"只能在**跨进程**里验 ——
        // `scripts/smoke-domainlock.ps1` 第 9 条干的就是那件事。
    }

    /// 反面：不相干的围栏不许被顺手锁死（这条是老行为，**不许在修上面那条时弄坏**）。
    #[test]
    fn unrelated_fences_still_both_get_their_locks() {
        let (a, b) = (a_dir("solo-a"), a_dir("solo-b"));
        let first = acquire("solo", &[a]).expect("A 该拿到");
        let second = acquire("solo", &[b]).expect("B 与 A 无关，也该拿到");
        drop(first);
        drop(second);
    }

    /// 同一个根写两遍**算一条**：不然 `--fence X --fence X` 会自己跟自己撞
    /// （同一个进程里第二次 `CreateMutexW` 照样报 `ERROR_ALREADY_EXISTS`）。
    #[test]
    fn the_same_root_twice_is_one_lock_not_a_conflict_with_itself() {
        let a = a_dir("twice");
        let lock = acquire("twice", &[a.clone(), a]).expect("同一个根写两遍不该冲突");
        drop(lock);
    }
}
