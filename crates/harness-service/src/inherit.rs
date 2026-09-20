//! **别让你拉起的进程顺走你的句柄**（2026-09-20，smoke 第一次跑就撞出来的）。
//!
//! > **它住在 `harness-service` 而不是 `harness-ipc`**（同一天挪过来的）：
//! > 它管的是**进程创建**，不是传输 —— 而"我们拉起一个进程"的地方有三个
//! > （中控拉常驻服务、终端拉命令、MCP 桥拉对端），**这类代码有三份就一定会漂**。
//!
//! 撞的是这个形状：
//!
//! ```text
//!   脚本 ──(管道)──▶ 中控 ──拉起──▶ 常驻服务（活得比中控久）
//!                                   ↑ 它手里有一份**中控那根 stderr 管道的副本**
//! ```
//!
//! 后果很具体：中控退出了，可**管道还没到 EOF**（因为还有人在写），
//! 于是"把中控的 stderr 读干净"这件事**永远不结束** ——
//! smoke 里表现成"会话一的 stderr 读出来是空的"，而且不报错，静悄悄地卡到超时。
//!
//! 根因是 Windows 的**句柄继承**：`CreateProcessW` 在需要重定向时用
//! `bInheritHandles = TRUE`，而那会把父进程里**所有标了"可继承"的句柄**
//! 一并复制给子进程 —— 包括中控自己那三根标准句柄。
//!
//! 修法不是"要求调用方别用管道"，而是**在拉起之前把中控自己那三根句柄的
//! 可继承位摘掉，拉完再放回去**。这样：
//!
//! * 子进程照样能拿到它该拿的（我们显式传给它的 stdin/stdout/stderr）；
//! * 但它**不会**顺走一份中控的管道副本；
//! * 中控退出 ⇒ 管道干净地到达 EOF。
//!
//! 窗口期只有"清理 → 拉起 → 还原"这几毫秒，而且中控只在主线程里拉起服务，
//! 所以不会跟别的 `CreateProcessW` 撞上。
//!
//! ⚠️ **它不是只在"常驻"那一条路上需要。** 任何"我们拉起的进程"都可能活得比我们久
//! （终端的 `start /B` 孙子、MCP 那边别人写的 server），也都可能攥着我们的句柄。
//! 三个地方都该在拉起之前喊一声 `hide()`。

/// 摘掉/还原标准句柄的"可继承"位。**离开作用域自动还原**（包括 panic 那一路）。
pub struct NoInheritStdio {
    #[cfg(windows)]
    saved: Vec<(isize, u32)>,
}

#[cfg(windows)]
mod imp {
    use windows_sys::Win32::Foundation::{
        GetHandleInformation, SetHandleInformation, HANDLE_FLAG_INHERIT,
    };
    use windows_sys::Win32::System::Console::{
        GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
    };

    pub type Saved = Vec<(isize, u32)>;

    pub fn hide() -> Saved {
        let mut saved = Vec::new();
        for which in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
            // SAFETY: GetStdHandle 只是查一个进程级常量表；空句柄时后面两步会跳过。
            let h = unsafe { GetStdHandle(which) };
            if h.is_null() {
                continue;
            }
            let mut flags: u32 = 0;
            // SAFETY: 我们传的是刚拿到的、属于本进程的句柄和一个合法的 out 指针。
            let ok = unsafe { GetHandleInformation(h, &mut flags) };
            if ok == 0 {
                continue;
            }
            saved.push((h as isize, flags));
            if flags & HANDLE_FLAG_INHERIT != 0 {
                // SAFETY: 同上；只改标志位，不动句柄本身。
                unsafe { SetHandleInformation(h, HANDLE_FLAG_INHERIT, 0) };
            }
        }
        saved
    }

    pub fn restore(saved: &Saved) {
        for (h, flags) in saved {
            if flags & HANDLE_FLAG_INHERIT != 0 {
                // SAFETY: 这些句柄是 `hide()` 当时存下来的、本进程的句柄。
                unsafe { SetHandleInformation(*h as _, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) };
            }
        }
    }
}

impl NoInheritStdio {
    /// 在**拉起子进程之前**调它；返回值一丢就还原。
    pub fn hide() -> Self {
        #[cfg(windows)]
        {
            Self {
                saved: imp::hide(),
            }
        }
        #[cfg(not(windows))]
        {
            // 类 Unix 的等价物是 `FD_CLOEXEC` —— 而 std 打开的那些描述符
            // **本来就是** CLOEXEC 的，所以这边什么都不用做。
            Self {}
        }
    }
}

impl Drop for NoInheritStdio {
    fn drop(&mut self) {
        #[cfg(windows)]
        imp::restore(&self.saved);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 藏一下再还原，**不能把中控自己的输出弄坏** —— 这条测试本身就是证据：
    /// 它能在藏过之后正常打印（`cargo test -- --nocapture` 看得到）。
    #[test]
    fn hiding_and_restoring_does_not_break_our_own_stdio() {
        {
            let _g = NoInheritStdio::hide();
            println!("藏起来的时候还能打印");
        }
        println!("还原之后也还能打印");
    }

    /// 藏两次再还原两次也不该乱（幂等）。
    #[test]
    fn hiding_twice_is_fine() {
        let a = NoInheritStdio::hide();
        let b = NoInheritStdio::hide();
        drop(b);
        drop(a);
        println!("两次都还原了");
    }
}
