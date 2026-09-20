//! 终端实例：一个目录 + 一个追加缓冲 + 至多一条在跑的命令。
//!
//! **两条形状上和编辑器不同、但都是故意的决定：**
//!
//! 1. **它不占跨实例的排他锁。** 编辑器那把锁是"同一时刻只有一个写者"（§5.15）；
//!    终端要的是**归属**（这条路径归它管），不是排他 —— 否则两个终端不能在同一个
//!    项目里干活，编辑器也没法和终端共存。§5.15 说的"按终端串行"是**实例内部**。
//! 2. **它把所有输出留在自己的追加缓冲里**，游标读它 ——
//!    因为终端不是打印机，输出是不可回滚的流（§5.19）。

use std::collections::VecDeque;
use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use crate::Inbound;
use harness_service::job::{Job, JobAccount, Limits};

/// 一次 `run` 的结局。
#[derive(Debug, Clone)]
pub enum RunOutcome {
    /// 正常结束了（**退出码非零也是这个** —— 命令失败了，但它确实执行了）
    Exited(Option<i32>),
    /// 到点了，我们把它掐了
    TimedOut,
    /// **调用方取消了它**（§2.3：尽力而为 —— 我们确实动手停了它）
    Canceled,
}

pub struct Running {
    pub cmd: String,
    pub by: String,
    pub corr: String,
    pub started_ms: i64,
    /// 这条命令的输出在这块缓冲里的起点
    pub from: usize,
    pub to: usize,
    deadline: Option<Instant>,
    child: Child,
    /// 把这棵进程树一起管的 Job Object（Windows）。`None` = 只能杀一层。
    job: Option<Job>,
    /// 这次跑完时 job 账本上的痕迹（峰值内存 / 起过几个进程）。
    /// **它不是结论，是证据**：用来把"很可能撞了限额"说成推断。
    pub account: JobAccount,
    /// 还有几个读线程没读完（stdout / stderr 各一个）
    readers_left: u8,
    /// 进程自己已经退出了吗（退了但读线程没读完时**还不能回结果**，
    /// 否则会丢掉最后一段输出 —— 这是最容易漏的一个竞态）
    exit: Option<Option<i32>>,
    /// 是不是我们掐的（掐了就不是"正常退出"，要报 `partial` + `timeout`）
    timed_out: bool,
    /// 是不是调用方取消的（要报 `partial` + `canceled`，而且**说得出是谁的请求**）
    canceled: bool,
}

pub struct Terminal {
    pub id: String,
    pub cwd: PathBuf,
    /// **实例自带一句用途说明**，开的时候自己写（§5.16）
    pub purpose: String,
    /// 谁开的（`实例id@会话id`）
    pub holder: String,
    pub running: Option<Running>,
    output: String,
    /// **已经溢到磁盘的那一段**（在 `output` 之前）。
    ///
    /// 为什么需要它：§5.17 那条"背压"落到我们这儿的表现形式就是**溢出**，
    /// 而不是"让生产者阻塞" —— `yes` 一下就是几个 G，留在内存里就是等 OOM。
    /// 而全文必须**还读得到**，否则模型想看一眼尾部就得再跑一条命令（死循环）。
    /// 所以：内存只留尾巴，前面的按字符边界挪到文件，**`read` 按游标跨两段读**。
    spill: Option<Spill>,
    /// 内存里最多留多少字节（超了就把前面的挪到磁盘）
    memory_cap: usize,
    /// **这棵树的硬限额**（Job Object 上、由内核执行）。
    ///
    /// 默认全空 = 不限额：多严算"合理"是 profile 的事（待定 #3），
    /// 而"默认开一个我拍的数"会在正常需求上碰壁。开了才报 `resources: "limits"`。
    limits: Limits,
    pub read_cursor: usize,
}

/// 一段溢出到磁盘的输出：文件位置 + 已经落进去多少字节。
///
/// 全局字节偏移 = `[0, len)` 在文件里，`[len, len + output.len())` 在内存里。
/// 游标一直用全局偏移，所以**溢出这件事对调用方不可见**（它只看到"还有更多"）。
struct Spill {
    path: PathBuf,
    len: usize,
}

impl Terminal {
    pub fn new(
        id: String,
        cwd: PathBuf,
        purpose: String,
        holder: String,
        spill_path: PathBuf,
        memory_cap: usize,
        limits: Limits,
    ) -> Self {
        // **新实例 = 空缓冲。**
        //
        // 这条不是洁癖：重新装配会把 `t-1` 这个名字**再用一次**
        // （同一批 id 重新装出来），而溢出文件是落在磁盘上的 ——
        // 不截断的话，新终端的第一条输出会跟上一次会话的尾巴拼在一起，
        // 而调用方看不出来（游标是连续的）。**一个实例的账要干净。**
        let _ = std::fs::remove_file(&spill_path);
        let _ = std::fs::create_dir_all(
            spill_path.parent().unwrap_or(std::path::Path::new(".")),
        );
        Self {
            id,
            cwd,
            purpose,
            holder,
            running: None,
            output: String::new(),
            spill: Some(Spill {
                path: spill_path,
                len: 0,
            }),
            memory_cap,
            limits,
            read_cursor: 0,
        }
    }

    /// 全文一共多少字节（内存 + 磁盘）。
    pub fn len(&self) -> usize {
        self.spilled() + self.output.len()
    }

    fn spilled(&self) -> usize {
        self.spill.as_ref().map(|s| s.len).unwrap_or(0)
    }

    /// 全文落在哪个文件里（**没溢出就是 None**）。
    /// 结果里的 `spilled_to` 用它 —— 但它不是**唯一**的读法：
    /// 按游标 `read` 才是正路（不必去开文件）。
    pub fn spill_path(&self) -> Option<&PathBuf> {
        self.spill.as_ref().filter(|s| s.len > 0).map(|s| &s.path)
    }

    pub fn append(&mut self, text: &str) {
        self.output.push_str(text);
        self.spill_front();
        // ⚠️ **必须是全局长度，不是内存长度。**
        // 溢出会让内存变短，而 `to` 是给 `slice` 用的**全局偏移** ——
        // 写错这一行的后果是：`run` 只回"溢出之后内存里剩下的那点字节"，
        // 前面整段被静默丢掉。这条是 smoke-spill 第一次跑就抓出来的。
        let now_len = self.len();
        if let Some(r) = self.running.as_mut() {
            r.to = now_len;
        }
    }

    /// 内存超了就把前面那段挪到文件里（**按字符边界切，别把汉字劈开**）。
    fn spill_front(&mut self) {
        if self.output.len() <= self.memory_cap {
            return;
        }
        let want = self.output.len() - self.memory_cap;
        // **切点往后找**：宁可多留几个字节在内存里，也别切碎一个字符
        let mut cut = want;
        while cut < self.output.len() && !self.output.is_char_boundary(cut) {
            cut += 1;
        }
        let head = self.output[..cut].to_string();
        let Some(spill) = self.spill.as_mut() else {
            return; // 没给溢出文件（不该发生）—— 那就留着，绝不丢
        };
        match OpenOptions::new().create(true).append(true).open(&spill.path) {
            Ok(mut f) => match f.write_all(head.as_bytes()) {
                Ok(()) => {
                    spill.len += head.len();
                    self.output.replace_range(..cut, "");
                }
                Err(e) => eprintln!("[term] 溢写失败（{}）：{e} —— 这段输出仍留在内存里", spill.path.display()),
            },
            Err(e) => eprintln!(
                "[term] 溢写打不开 {}：{e} —— 这段输出仍留在内存里（宁可内存大一点，也不能悄悄丢）",
                spill.path.display()
            ),
        }
    }

    /// 按字节取一段，边界对齐到字符（别把 UTF-8 切两半）。
    ///
    /// **它要跨两段读**：前面在文件里、后面在内存里。
    /// 游标是**全局偏移**，调用方看不出这段历史被搬过家。
    pub fn slice(&self, from: usize, to: usize) -> String {
        let len = self.len();
        let a = from.min(len);
        let b = to.min(len).max(a);
        let spilled = self.spilled();
        if b <= spilled {
            return self.read_spilled(a, b);
        }
        let mut out = String::new();
        if a < spilled {
            out.push_str(&self.read_spilled(a, spilled));
            out.push_str(&self.memory_slice(0, b - spilled));
        } else {
            out.push_str(&self.memory_slice(a - spilled, b - spilled));
        }
        out
    }

    /// 内存那一段（偏移是**相对内存**的）。
    fn memory_slice(&self, from: usize, to: usize) -> String {
        let len = self.output.len();
        let mut a = from.min(len);
        while a < len && !self.output.is_char_boundary(a) {
            a += 1;
        }
        let mut b = to.min(len);
        while b > a && !self.output.is_char_boundary(b) {
            b -= 1;
        }
        self.output[a..b].to_string()
    }

    /// 磁盘那一段（偏移是**全局**的）。
    ///
    /// 两个细节：① 多读 3 字节，好在"游标落在字符中间"时能切回边界；
    /// ② 切完再用 `from_utf8_lossy` —— 到这里它已经不该再有坏字节了，
    ///    但**宁可显示一个替换符，也不 panic**。
    fn read_spilled(&self, from: usize, to: usize) -> String {
        let Some(spill) = self.spill.as_ref() else {
            return String::new();
        };
        let to = to.min(spill.len);
        if from >= to {
            return String::new();
        }
        let start = from.saturating_sub(3);
        let want = (to + 3).saturating_sub(start);
        let mut f = match std::fs::File::open(&spill.path) {
            Ok(f) => f,
            Err(e) => return format!("〔读溢出文件失败：{e}〕"),
        };
        if f.seek(SeekFrom::Start(start as u64)).is_err() {
            return String::new();
        }
        let mut buf = vec![0u8; want];
        let n = f.read(&mut buf).unwrap_or(0);
        buf.truncate(n);
        // 把窗口里的字节边界夹到 [from, to)
        let mut a = from - start;
        while a < buf.len() && !is_boundary(&buf, a) {
            a += 1;
        }
        let mut b = (to - start).min(buf.len());
        while b > a && !is_boundary(&buf, b) {
            b -= 1;
        }
        String::from_utf8_lossy(&buf[a..b]).to_string()
    }

    /// 起一条命令。`cd` 不在这里 —— 那个由服务自己处理（要过围栏）。
    pub fn start(
        &mut self,
        cmd: &str,
        by: &str,
        _sid: String,
        timeout: Option<Duration>,
        corr: Option<String>,
        tx: Sender<Inbound>,
    ) -> std::io::Result<()> {
        let mut command = shell(cmd);
        command
            .current_dir(&self.cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn()?;
        // 先把它收进 job，再去读它的输出 —— **顺序不能反**：
        // 反过来的话，一个起得极快的命令可能在 assign 之前就已经生了孙子进程。
        let job = Job::assign(&child, self.limits);
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();

        let from = self.len();
        self.running = Some(Running {
            cmd: cmd.to_string(),
            by: by.to_string(),
            corr: corr.unwrap_or_default(),
            started_ms: crate_now(),
            from,
            to: from,
            deadline: timeout.map(|d| Instant::now() + d),
            child,
            job,
            account: JobAccount::default(),
            readers_left: 2,
            exit: None,
            timed_out: false,
            canceled: false,
        });

        // 两个读线程只负责"把字节交给主循环"；**所有状态变更都在主循环里**，
        // 于是实例状态不需要任何锁。
        for stream in [stdout.map(Pipe::Out), stderr.map(Pipe::Err)].into_iter().flatten() {
            let tx = tx.clone();
            let term_id = self.id.clone();
            std::thread::spawn(move || {
                let mut carry = Utf8Carry::default();
                let mut reader = stream.into_reader();
                let mut buf = [0u8; 8192];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            let text = carry.push(&buf[..n]);
                            if !text.is_empty()
                                && tx
                                    .send(Inbound::TermOutput {
                                        term_id: term_id.clone(),
                                        text,
                                    })
                                    .is_err()
                            {
                                return;
                            }
                        }
                    }
                }
                if let Some(tail) = carry.flush() {
                    let _ = tx.send(Inbound::TermOutput {
                        term_id: term_id.clone(),
                        text: tail,
                    });
                }
                let _ = tx.send(Inbound::TermReaderDone { term_id });
            });
        }
        Ok(())
    }

    /// 主循环每轮问一次：进程退了吗？退了就记下来（**但先别回结果**）。
    pub fn poll_exit(&mut self) {
        let Some(r) = self.running.as_mut() else {
            return;
        };
        if r.exit.is_some() {
            return;
        }
        match r.child.try_wait() {
            Ok(Some(status)) => r.exit = Some(status.code()),
            Ok(None) => {}
            Err(_) => r.exit = Some(None),
        }
    }

    /// 一个读线程读完了。
    pub fn reader_done(&mut self) {
        if let Some(r) = self.running.as_mut() {
            r.readers_left = r.readers_left.saturating_sub(1);
        }
    }

    /// 可以回结果了吗 —— **进程退出了、并且两个读线程都读完了**。
    pub fn ready(&self) -> Option<RunOutcome> {
        let r = self.running.as_ref()?;
        if r.readers_left > 0 {
            return None;
        }
        if r.timed_out {
            return Some(RunOutcome::TimedOut);
        }
        if r.canceled {
            return Some(RunOutcome::Canceled);
        }
        r.exit.map(RunOutcome::Exited)
    }

    /// **取消**：只取消 corr 对得上的那一条（§2.3：尽力而为，做不到就诚实说）。
    ///
    /// 返回 `true` = 我确实动手停了它。返回 `false` = 那个 corr 不是我在跑的 ——
    /// **调用方会把这句"没停成"如实报上去**，而不是假装停了。
    pub fn cancel(&mut self, corr: &str) -> bool {
        let Some(r) = self.running.as_mut() else {
            return false;
        };
        if r.corr != corr {
            return false;
        }
        if let Some(job) = &r.job {
            job.terminate();
        }
        let _ = r.child.kill();
        let _ = r.child.wait();
        r.exit = Some(None);
        r.canceled = true;
        r.deadline = None;
        true
    }

    /// 收掉这一次运行，返回它（给调用方回结果用）。
    pub fn finish(&mut self, _outcome: &RunOutcome) -> Option<Running> {
        let mut running = self.running.take()?;
        // **趁 job 还在，把它的账读出来**（读完就随 Running 一起 drop 掉了）。
        //
        // 为什么要读：限额是**内核执行的**，可内核没告诉我们"它是因为撞了限额被杀的"
        // （那要完成端口通知，没做）。所以事后查两个计数器 ——
        // 峰值内存 / 一共起过几个进程 —— 然后**按推断说话**。
        if let Some(job) = running.job.as_ref() {
            running.account = job.account();
        }
        Some(running)
    }

    /// 把正在跑的那条**结掉**，并把已经攒到的输出一起返回。
    ///
    /// 用在"终端被关掉"和"服务收摊"这两条路上 ——
    /// **进程退出不是静默丢弃一个未决调用的理由**（§4.6、§11.5）。
    pub fn abort_running(&mut self, _code: &str, _why: &str) -> Option<(Running, String)> {
        let running = self.running.take()?;
        let output = self.slice(running.from, running.to);
        Some((running, output))
    }

    /// 超时了吗？超了就掐掉 —— **诚实报告"我们动手了"**（§11.5 的 `partial`）。
    pub fn kill_for_timeout(&mut self) -> Option<String> {
        let r = self.running.as_mut()?;
        if !r.deadline.is_some_and(|d| d <= Instant::now()) {
            return None;
        }
        // **杀整棵树。** 只杀 cmd.exe 的话，它启动的 ping 还攥着输出管道，
        // 读线程永远等不到 EOF —— 那条命令的结果就永远回不来了。
        if let Some(job) = &r.job {
            job.terminate();
        }
        let _ = r.child.kill();
        let _ = r.child.wait();
        r.exit = Some(None);
        r.timed_out = true;
        r.deadline = None;
        Some(self.id.clone())
    }

    /// 无条件的杀（close / 收摊时用）。
    pub fn kill(&mut self) {
        if let Some(r) = self.running.as_mut() {
            if let Some(job) = &r.job {
                job.terminate();
            }
            let _ = r.child.kill();
            let _ = r.child.wait();
            r.exit = Some(None);
        }
    }
}

enum Pipe {
    Out(std::process::ChildStdout),
    Err(std::process::ChildStderr),
}

impl Pipe {
    fn into_reader(self) -> Box<dyn Read + Send> {
        match self {
            Pipe::Out(s) => Box::new(s),
            Pipe::Err(s) => Box::new(s),
        }
    }
}

#[cfg(windows)]
/// 把一条命令行交给系统 shell。
///
/// ⚠️ **Windows 上必须用 `raw_arg`，不能用 `arg`**（2026-09-20 抓到的老 bug）：
///
/// Rust 的 `Command::arg` 会按 **MSVCRT 那套规则**替我们转义参数 ——
/// 参数里的 `"` 会变成 `\"`。可 **`cmd.exe` 不认那套转义**：
/// 它看到的是字面反斜杠 + 一个引号切换，于是
///
/// * `\"` 留在命令行里（`start /B cmd /C \"ping …\"` ⇒ cmd 报"不认识这条命令"）；
/// * 引号配对被打乱之后，PowerShell 那一层把 `$t` 当成**它自己的变量**展开成空 ——
///   命令被掏空了还在"正常"退出，看起来像"跑完了"。
///
/// 这个坑从第一天就在，**而之前的验收命令一条引号都没用过**（`echo x` / `cd sub` /
/// `ping -n 4`）—— 是"给终端加限额"那条测试随手写了个
/// `powershell -Command "…"` 才把它撞出来的。
///
/// 修法：`raw_arg` 把命令行**原样**交出去，让 cmd 按**它自己的**规则解析；
/// 引号怎么配、变量归谁展开，那是命令作者和 shell 之间的事，**不该由我们的
/// 参数传递层替他决定**。
#[cfg(windows)]
fn shell(cmd: &str) -> Command {
    use std::os::windows::process::CommandExt;
    let mut c = Command::new("cmd");
    // 两个都 raw：`/C` 只是个开关，后面那一串才是**一整条命令行**
    c.raw_arg("/C").raw_arg(cmd);
    c
}

#[cfg(not(windows))]
fn shell(cmd: &str) -> Command {
    let mut c = Command::new("sh");
    c.arg("-c").arg(cmd);
    c
}

fn crate_now() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// `buf[i]` 是不是一个字符的**起点**（或者 `i` 正好在末尾）。
///
/// 溢出文件是纯字节，没地方问"你是不是边界" —— 只能自己看这一字节：
/// UTF-8 的续字节形如 `10xxxxxx`，其余的（`0xxxxxxx` / `11xxxxxx`）都是起点。
fn is_boundary(buf: &[u8], i: usize) -> bool {
    i >= buf.len() || (buf[i] & 0xC0) != 0x80
}

/// 把字节流切成**合法的 UTF-8 片段**：切在字符中间的尾巴留到下一轮。
///
/// 没有它，一个汉字横跨两次 `read` 就会变成两个替换符 —— 而中文输出很常见。
#[derive(Default)]
struct Utf8Carry {
    pending: VecDeque<u8>,
}

impl Utf8Carry {
    fn push(&mut self, bytes: &[u8]) -> String {
        self.pending.extend(bytes);
        let contig: Vec<u8> = self.pending.iter().copied().collect();
        match std::str::from_utf8(&contig) {
            Ok(s) => {
                self.pending.clear();
                s.to_string()
            }
            Err(e) => {
                let good = e.valid_up_to();
                let text = String::from_utf8_lossy(&contig[..good]).to_string();
                self.pending = contig[good..].iter().copied().collect();
                // 尾巴已经长到不可能是一个合法字符了 ⇒ 当垃圾丢掉
                if self.pending.len() > 4 {
                    let junk: Vec<u8> = self.pending.iter().copied().collect();
                    self.pending.clear();
                    return format!("{text}{}", String::from_utf8_lossy(&junk));
                }
                text
            }
        }
    }

    fn flush(&mut self) -> Option<String> {
        if self.pending.is_empty() {
            return None;
        }
        let junk: Vec<u8> = self.pending.iter().copied().collect();
        self.pending.clear();
        Some(String::from_utf8_lossy(&junk).to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf8_split_across_reads_is_rejoined() {
        // "汉" = E6 B1 89，故意从中间切开
        let mut c = Utf8Carry::default();
        assert_eq!(c.push(&[0xE6]), "");
        assert_eq!(c.push(&[0xB1]), "");
        assert_eq!(c.push(&[0x89]), "汉");
    }

    #[test]
    fn ascii_passes_through_in_one_piece() {
        let mut c = Utf8Carry::default();
        assert_eq!(c.push(b"hello "), "hello ");
        assert_eq!(c.push(b"world"), "world");
        assert_eq!(c.flush(), None);
    }

    #[test]
    fn slice_snaps_to_char_boundaries() {
        let mut t = test_terminal(usize::MAX);
        t.append("a汉b");
        assert_eq!(t.slice(0, 1), "a");
        assert_eq!(t.slice(1, 4), "汉", "3 字节的字符要被完整取出");
        assert_eq!(t.slice(0, 99), "a汉b", "越界要夹住，不是 panic");
    }

    /// 一个**真的能溢写**的实例（内存上限由参数给）。
    fn test_terminal(cap: usize) -> Terminal {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static N: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "harness-term-spill-{}-{}-{}.log",
            std::process::id(),
            crate_now(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        Terminal::new(
            "t-1".into(),
            PathBuf::from("."),
            "p".into(),
            "h".into(),
            path,
            cap,
            Limits::default(),
        )
    }

    /// **背压的落地形态是"溢出"，不是"让生产者阻塞"**（§5.17）：
    /// 内存只留尾巴，前面的按字符边界挪到文件，而**游标读跨两段** ——
    /// 调用方看不出这段历史搬过家。
    #[test]
    fn spilling_keeps_the_global_offsets_intact() {
        let mut t = test_terminal(32);
        // 三段，每段 40 字节 ⇒ 必然溢出两次
        let a = "A".repeat(40);
        let b = "B".repeat(40);
        let c = "C".repeat(40);
        t.append(&a);
        t.append(&b);
        t.append(&c);
        assert_eq!(t.len(), 120, "全局长度把磁盘那段也算上");
        assert!(t.spill_path().is_some(), "溢出了就该有文件");
        // 游标读：从前到后逐段取，拼起来必须一个字不差
        let mut got = String::new();
        for from in (0..120).step_by(7) {
            got.push_str(&t.slice(from, (from + 7).min(120)));
        }
        assert_eq!(got, format!("{a}{b}{c}"), "跨段的游标读要拼得回来");
    }

    /// **别把汉字劈开**：内存上限正好落在一个三字节字符中间时，
    /// 切点要往后让 —— 宁可多留几个字节。
    #[test]
    fn the_spill_cut_never_splits_a_character() {
        let mut t = test_terminal(7);
        t.append("a汉汉汉"); // 1 + 3*3 = 10 字节，上限 7 ⇒ 必然从中间切
        assert_eq!(t.len(), 10);
        assert_eq!(t.slice(0, 10), "a汉汉汉");
    }

    /// **溢出不改变"这条命令的输出"这个区间** —— `to` 是全局偏移。
    ///
    /// 这条是 smoke-spill 第一次跑抓出来的真 bug：`to` 原来记的是
    /// **内存长度**，于是溢出之后 `run` 只回"内存里剩下的那点字节"，
    /// 前面整段被静默丢掉（而它还在文件里，只是没人去取）。
    #[test]
    fn spilling_does_not_shrink_the_running_range() {
        let mut t = test_terminal(16);
        t.running = Some(Running {
            cmd: "x".into(),
            by: "console".into(),
            corr: "c-1".into(),
            started_ms: 0,
            from: 0,
            to: 0,
            deadline: None,
            child: shell("exit").spawn().unwrap(),
            job: None,
            account: JobAccount::default(),
            readers_left: 2,
            exit: None,
            timed_out: false,
            canceled: false,
        });
        t.append(&"A".repeat(40));
        t.append(&"B".repeat(40));
        let r = t.running.as_ref().unwrap();
        assert_eq!(r.to, 80, "全局长度，不是内存里剩下的长度");
        assert_eq!(t.slice(r.from, r.to).len(), 80, "这条命令的输出一个字都不能少");
    }

    #[test]
    fn append_moves_the_running_watermark() {
        let mut t = test_terminal(usize::MAX);
        t.running = Some(Running {
            cmd: "x".into(),
            by: "console".into(),
            corr: "c-1".into(),
            started_ms: 0,
            from: 0,
            to: 0,
            deadline: None,
            child: Command::new("cmd").arg("/C").arg("exit").spawn().unwrap(),
            job: None,
            account: JobAccount::default(),
            readers_left: 2,
            exit: None,
            timed_out: false,
            canceled: false,
        });
        t.append("abc");
        assert_eq!(t.running.as_ref().unwrap().to, 3);
    }
}
