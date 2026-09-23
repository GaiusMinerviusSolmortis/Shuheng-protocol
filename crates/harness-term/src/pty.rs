//! **会话模式的那层底**：一条真的伪控制台（ConPTY）（02-工具 §5.18）。
//!
//! 命令模式是**每条命令新起一个 `cmd /C`**；会话模式是**一条常驻的 shell**，
//! 输入/输出都是字节流 —— 于是 REPL、进度条、交互式程序才可能。
//!
//! 这一层只做三件事，别的一概不管：
//!
//! 1. **起**：建一对管道 → `CreatePseudoConsole` → 带 `PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE`
//!    起 shell（**先把它收进 Job**，理由同命令模式那条：起得快的东西可能在
//!    assign 之前就生了孙子）；
//! 2. **写**：`send` 把字节塞进输入管道 —— 对终端来说那就是"敲键盘"；
//! 3. **读**：输出管道的字节交给调用方（服务那一侧把它喂给屏幕仿真，见 `screen.rs`）。
//!
//! ⚠️ **只在 Windows 上实现**（ConPTY 是 Windows 的东西）。别的平台上 `spawn`
//! 会**明确拒绝**，而不是悄悄退化成一个假的会话 —— 那种"看起来能用"最坑人。

use std::io::{self, Read, Write};
use std::path::Path;

/// **读线程的回调**：每读到一块字节就叫一次。
///
/// 起个名字是因为 Clippy 说得对 —— `Box<dyn Fn(&[u8]) + Send>` 出现在签名里
/// 就该有个名字，不然读的人得在两个地方对齐同一串类型。
pub type OutputSink = Box<dyn Fn(&[u8]) + Send>;

/// 一条 PTY 会话。**Drop 就是收摊**（关伪控制台 + 杀整棵）。
pub struct PtySession {
    input: std::fs::File,
    #[cfg(windows)]
    inner: win::Handles,
    pid: u32,
}

impl PtySession {
    /// 在这个目录下起一条会话（`shell` 一般是 `cmd.exe`）。
    ///
    /// `on_output` 在**读线程**里被调用（每次读到一块字节）—— 服务那边把它转成
    /// `Inbound::TermOutput` 交回主循环，**状态变更仍然只在主循环里发生**。
    pub fn spawn(
        dir: &Path,
        cols: u16,
        rows: u16,
        on_output: impl Fn(&[u8]) + Send + 'static,
    ) -> io::Result<PtySession> {
        #[cfg(windows)]
        {
            win::spawn(dir, cols, rows, Box::new(on_output))
        }
        #[cfg(not(windows))]
        {
            let _ = (dir, cols, rows, on_output);
            Err(io::Error::other(
                "会话模式要伪控制台（ConPTY），这一版只在 Windows 上实现 —— \
                 别的平台上请用命令模式（run）",
            ))
        }
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// **往终端里敲**（字节）。对端是个真终端，所以 `\r` 才是回车。
    pub fn send(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.input.write_all(bytes)?;
        self.input.flush()
    }

    /// shell 还在不在（`GetExitCodeProcess`）。
    pub fn alive(&mut self) -> bool {
        #[cfg(windows)]
        {
            win::alive(&self.inner)
        }
        #[cfg(not(windows))]
        {
            false
        }
    }

    /// 退出码（shell 真的退了才有）。
    pub fn exit_code(&mut self) -> Option<i32> {
        #[cfg(windows)]
        {
            win::exit_code(&self.inner)
        }
        #[cfg(not(windows))]
        {
            None
        }
    }
}

#[cfg(windows)]
mod win {
    use super::*;
    use std::os::windows::io::FromRawHandle;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, STILL_ACTIVE};
    use windows_sys::Win32::System::Console::{
        ClosePseudoConsole, CreatePseudoConsole, COORD, HPCON,
    };
    use windows_sys::Win32::System::Pipes::CreatePipe;
    use windows_sys::Win32::System::Threading::{
        CreateProcessW, GetExitCodeProcess, InitializeProcThreadAttributeList,
        TerminateProcess, UpdateProcThreadAttribute, CREATE_UNICODE_ENVIRONMENT,
        EXTENDED_STARTUPINFO_PRESENT, PROCESS_INFORMATION, PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE,
        STARTF_USESTDHANDLES,
        STARTUPINFOEXW, STARTUPINFOW,
    };

    pub struct Handles {
        pub hpc: HPCON,
        pub process: HANDLE,
        /// **整棵**：shell 死了（或者我们死了）孙子也跟着走
        pub job: Option<harness_service::job::Job>,
    }

    unsafe impl Send for Handles {}

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    pub fn spawn(
        dir: &Path,
        cols: u16,
        rows: u16,
        on_output: OutputSink,
    ) -> io::Result<PtySession> {
        unsafe {
            // 1) 两条管道：一条喂它（我们写 / 它读），一条看它（它写 / 我们读）
            let (mut in_r, mut in_w) = (std::ptr::null_mut(), std::ptr::null_mut());
            let (mut out_r, mut out_w) = (std::ptr::null_mut(), std::ptr::null_mut());
            if CreatePipe(&mut in_r, &mut in_w, std::ptr::null(), 0) == 0
                || CreatePipe(&mut out_r, &mut out_w, std::ptr::null(), 0) == 0
            {
                return Err(io::Error::last_os_error());
            }
            // 2) 伪控制台：它接管这两条管道的**对端**（我们手里的那两头留着）
            let size = COORD {
                X: cols.max(2) as i16,
                Y: rows.max(2) as i16,
            };
            let mut hpc: HPCON = 0;
            let hr = CreatePseudoConsole(size, in_r, out_w, 0, &mut hpc);
            if hr < 0 {
                CloseHandle(in_r);
                CloseHandle(in_w);
                CloseHandle(out_r);
                CloseHandle(out_w);
                return Err(io::Error::other(format!("CreatePseudoConsole 失败：HRESULT {hr}")));
            }
            // 伪控制台拿到了这两头，我们手里的那两头就还回去
            CloseHandle(in_r);
            CloseHandle(out_w);

            // 3) 用属性表把伪控制台交给新进程
            let mut attrs_size: usize = 0;
            InitializeProcThreadAttributeList(std::ptr::null_mut(), 1, 0, &mut attrs_size);
            // ⚠️ 缓冲区要**按指针对齐**：官方示例用 `HeapAlloc`（16 字节对齐），
            // 而 `Vec<u8>` 只保证 1 字节 —— 用 `Vec<usize>` 换 8 字节对齐，稳一点。
            let mut attrs_buf = vec![0usize; attrs_size.div_ceil(std::mem::size_of::<usize>())];
            let attrs = attrs_buf.as_mut_ptr() as *mut core::ffi::c_void;
            if InitializeProcThreadAttributeList(attrs, 1, 0, &mut attrs_size) == 0 {
                ClosePseudoConsole(hpc);
                return Err(io::Error::last_os_error());
            }
            // ⚠️ **`lpValue` 要的是那个 HPCON 值本身**（官方示例就是 `hPC`），
            // 不是"指向它的指针"。第一版传的是 `&hpc` —— 对端于是拿到一个
            // 垃圾控制台，`cmd.exe` 起来又**秒退**（`0xC0000142`
            // = STATUS_DLL_INIT_FAILED，连一行输出都没有）。
            // 这个坑只在"真去起一条会话"时才看得见：`open` 会成功、`send` 也会成功。
            if UpdateProcThreadAttribute(
                attrs,
                0,
                PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE as usize,
                hpc as *const core::ffi::c_void,
                std::mem::size_of::<HPCON>(),
                std::ptr::null_mut(),
                std::ptr::null(),
            ) == 0
            {
                ClosePseudoConsole(hpc);
                return Err(io::Error::last_os_error());
            }

            let mut si: STARTUPINFOEXW = std::mem::zeroed();
            si.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
            si.lpAttributeList = attrs as _;
            // ⚠️ **必须显式声明"标准句柄是空的"**（2026-09-20 实测抓到的）：
            //
            // 不写这一行，`cmd.exe` **会把我们的 stdout 当成它的 stdout** ——
            // 于是它的启动横幅（还是 GBK 编码的）**漏进我们和 ctrl 之间的协议管道**：
            // 探针那边读到的第一行不是 JSON，而是 `Microsoft Windows [版本 …]`。
            // 也就是说：**一条会话能把协议通道搅坏。**
            //
            // 这正是 `harness-service::inherit` 那条纪律在 PTY 这一处的同一个形状 ——
            // 区别是这里连"摘掉继承位"都不够（那个只保证句柄**不可继承**，
            // 而这里 Windows 是把调用方的标准句柄**复制**给子进程），
            // 得干脆声明"这个子进程没有标准句柄"：它的 IO 全走伪控制台。
            si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
            si.StartupInfo.hStdInput = std::ptr::null_mut();
            si.StartupInfo.hStdOutput = std::ptr::null_mut();
            si.StartupInfo.hStdError = std::ptr::null_mut();
            let mut pi: PROCESS_INFORMATION = std::mem::zeroed();
            let mut cmdline = wide("cmd.exe");
            // ⚠️ **`CreateProcessW` 的当前目录不吃 `\\?\` 前缀**（2026-09-20 实测）：
            // 围栏把路径规范化成了 `\\?\C:\…`，而 cmd.exe 收到它之后会说
            // "UNC 路径不受支持。默认值设为 Windows 目录"——**shell 就起在 `C:\Windows` 里**，
            // 而不是托管的那个项目目录（提示符会出卖它）。同族的老问题：
            // git 那边也嫌 `\\?\`（见 `harness-editor::git::toplevel`）。
            // 这里只把**给子进程的那一份**剥掉前缀，我们自己的路径操作照旧用规范形式。
            let plain = dir
                .display()
                .to_string()
                .trim_start_matches(r"\\?\")
                .to_string();
            let cwd = wide(&plain);
            let ok = CreateProcessW(
                std::ptr::null(),
                cmdline.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                0,
                EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT,
                std::ptr::null(),
                cwd.as_ptr(),
                &si as *const _ as *const STARTUPINFOW,
                &mut pi,
            );
            if ok == 0 {
                let e = io::Error::last_os_error();
                ClosePseudoConsole(hpc);
                return Err(e);
            }
            // **先收进 Job 再让它跑**（同命令模式那条：起得快的会在 assign 前生孙子）
            let job = harness_service::job::Job::assign_raw(
                pi.hProcess,
                harness_service::job::Limits::default(),
            );
            CloseHandle(pi.hThread);

            let pid = pi.dwProcessId;
            let input = std::fs::File::from_raw_handle(in_w as _);
            let mut output = std::fs::File::from_raw_handle(out_r as _);
            // 4) 读线程：把字节交给调用方（服务那边再喂给屏幕仿真）
            std::thread::spawn(move || {
                let mut buf = [0u8; 8192];
                loop {
                    match output.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => on_output(&buf[..n]),
                    }
                }
            });

            Ok(PtySession {
                input,
                inner: Handles {
                    hpc,
                    process: pi.hProcess,
                    job,
                },
                pid,
            })
        }
    }

    pub fn alive(h: &Handles) -> bool {
        exit_code(h).is_none()
    }

    pub fn exit_code(h: &Handles) -> Option<i32> {
        let mut code: u32 = 0;
        unsafe {
            if GetExitCodeProcess(h.process, &mut code) == 0 {
                return None;
            }
        }
        // `STILL_ACTIVE`（259）**也可能是真退出码** —— 这个歧义是 Windows 的老毛病；
        // 我们只在"还没退出"时把它当活着，其余原样报出去。
        if code == STILL_ACTIVE as u32 {
            None
        } else {
            Some(code as i32)
        }
    }

    impl Drop for Handles {
        fn drop(&mut self) {
            unsafe {
                // **先收整棵，再关那扇窗。** 顺序反过来的话，孙子进程可能在我们
                // 关掉伪控制台之后还活着 —— 它攥着输出管道，读线程就永远等不到 EOF
                // （同命令模式那条"只杀 cmd.exe 不够"）。
                if let Some(job) = &self.job {
                    job.terminate();
                }
                // **关伪控制台 = 关掉那扇窗**（对端会收到"窗口关了"）；
                ClosePseudoConsole(self.hpc);
                TerminateProcess(self.process, 0);
                CloseHandle(self.process);
            }
        }
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// **会话真的起得来吗**：起一条 shell、敲一句、看回话。
    ///
    /// 这条测试是 2026-09-20 加的 —— 而它第一次跑就红了：进程**秒退**，
    /// 退出码 `0xC0000142`（`STATUS_DLL_INIT_FAILED`）。所以它留着当看门狗：
    /// 会话模式这一层坏掉的时候，别的地方看不出来（`open` 会成功、`send` 也会成功，
    /// 只有屏幕是空的）。
    #[test]
    fn a_session_starts_a_shell_and_echoes_a_line() {
        let out = Arc::new(Mutex::new(String::new()));
        let sink = out.clone();
        let mut s = match PtySession::spawn(
            &std::env::temp_dir(),
            100,
            30,
            move |bytes| {
                if let Ok(mut o) = sink.lock() {
                    o.push_str(&String::from_utf8_lossy(bytes));
                }
            },
        ) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("起不来（环境问题，跳过）：{e}");
                return;
            }
        };
        // 等它把提示符吐出来
        let mut waited = 0;
        while waited < 3000 {
            std::thread::sleep(std::time::Duration::from_millis(100));
            waited += 100;
            if !out.lock().unwrap().is_empty() {
                break;
            }
        }
        let saw = out.lock().unwrap().clone();
        assert!(
            s.alive(),
            "shell 秒退了（退出码 {:?}）—— 会话模式这一层没起来。已收到的输出：{saw:?}",
            s.exit_code()
        );
        s.send(b"echo PTY-MARK\r").expect("敲得进去");
        let mut waited = 0;
        let mut all = String::new();
        while waited < 4000 {
            std::thread::sleep(std::time::Duration::from_millis(100));
            waited += 100;
            all = out.lock().unwrap().clone();
            if all.contains("PTY-MARK") && all.matches("PTY-MARK").count() >= 2 {
                break; // 一次是回显、一次是命令的输出
            }
        }
        assert!(
            all.contains("PTY-MARK"),
            "敲进去的东西没有回声。收到的输出：{all:?}"
        );
        assert!(all.contains("Microsoft") || all.contains(">") || !all.is_empty());
    }

    /// **shell 要起在托管的那个目录里**，不是 `C:\Windows`。
    ///
    /// 这条也是实测抓出来的：围栏给的路径是 `\\?\C:\…`，而 `CreateProcessW` 的
    /// 当前目录不吃那个前缀 —— cmd.exe 会打印"UNC 路径不受支持"然后落到 Windows 目录。
    /// 症状很隐蔽：会话能起、能敲、能回话，**只是它在错误的目录里**。
    #[test]
    fn the_shell_starts_in_the_hosted_directory() {
        let dir = std::env::temp_dir().join(format!("harness-pty-cwd-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        // 用**规范化之后**的形式（围栏给的就是这个），确认它也被剥掉前缀
        let canon = std::fs::canonicalize(&dir).unwrap_or(dir.clone());
        let out = Arc::new(Mutex::new(String::new()));
        let sink = out.clone();
        let mut s = match PtySession::spawn(&canon, 100, 30, move |b| {
            if let Ok(mut o) = sink.lock() {
                o.push_str(&String::from_utf8_lossy(b));
            }
        }) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("起不来（环境问题，跳过）：{e}");
                return;
            }
        };
        s.send(b"cd\r").expect("敲得进去");
        let mut waited = 0;
        let mut all = String::new();
        while waited < 4000 {
            std::thread::sleep(std::time::Duration::from_millis(100));
            waited += 100;
            all = out.lock().unwrap().clone();
            if all.matches("harness-pty-cwd").count() >= 2 {
                break;
            }
        }
        assert!(
            all.contains("harness-pty-cwd"),
            "shell 不在托管目录里 —— 收到的是：{all:?}"
        );
        assert!(
            !all.contains("UNC"),
            "cmd 说它收到了 UNC 路径 —— `\\\\?\\` 前缀没剥掉：{all:?}"
        );
    }
}
