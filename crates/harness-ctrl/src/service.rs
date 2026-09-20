//! 服务级实例的进程管理（§5.10、§5.16）。
//!
//! 形状照 06-操控 §14.14 那条结论：**一个服务一个进程，行分隔 JSON 对话**。
//! 这里只做最薄的一层：拉起来、给 stdin 写一行、把 stdout 的每一行交出去。
//!
//! **2026-09-20 多了第二条通道**：常驻服务（`lifecycle: resident`）不听 stdin、
//! 也不吐 stdout —— 它听在**回环套接字**上，谁带着会合文件里的口令谁来连
//! （见 `harness-ipc`）。理由很硬：stdio 是**父子私有**的，中控一收摊那条通道
//! 就没了；**别的会话根本接不上同一个进程**，常驻就写不出来。
//!
//! 两种通道在这一层长得一样：都是"写一行 / 收每一行 / 知道它什么时候没了"。
//! **上面那一层（路由、仲裁、日志）一个字都不用改** —— 那正是 §6.2 要保的东西。

use harness_core::codec::{self, NdjsonReader};
use harness_ipc::Conn;
use std::io::BufReader;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::Sender;

use crate::Inbound;

enum Channel {
    /// 父子私有：stdin 写、stdout 读（第一版就有的那条）
    Stdio {
        stdin: Option<ChildStdin>,
        child: Child,
    },
    /// 回环套接字 + 口令：**可以被后来者接上**（常驻服务那条）
    /// `child` 有两种取值，**它不是细节，是"我能不能让它死"的依据**：
    /// 自己拉起来的那个有句柄（能 kill），接上来的没有（只能请它自己停）。
    Ipc {
        conn: Conn,
        pid: u32,
        child: Option<Child>,
    },
}

pub struct ServiceHandle {
    pub id: String,
    /// 拉起它用的命令行 —— 图上那个节点点开要看得见（§11.1 E 组）
    pub cmd: Vec<String>,
    /// **常驻**：会话结束不收它，别的会话会来接同一个进程
    pub resident: bool,
    /// **这个会话是不是它的宿主**。不是宿主的话，"杀它"这件事就不该由这个会话决定。
    pub owner: bool,
    channel: Channel,
}

impl ServiceHandle {
    /// 起一个服务进程，并为它的 stdout 开一条读线程。
    ///
    /// 约定：中控总是追加 `--instance <id> --sid <sid>` ——
    /// 服务自己知道"我是谁"是它自报家门（presence + manifest）的前提。
    pub fn spawn(
        id: String,
        _kind: String,
        cmd: Vec<String>,
        sid: &str,
        generation: u64,
        tx: Sender<Inbound>,
    ) -> std::io::Result<Self> {
        let (program, rest) = cmd
            .split_first()
            .ok_or_else(|| std::io::Error::other("服务命令为空"))?;
        let mut child = Command::new(program)
            .args(rest)
            .arg("--instance")
            .arg(&id)
            .arg("--sid")
            .arg(sid)
            // stdout 是**协议通道**，stderr 留给日志（规范 §6.1 规矩三）
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()?;

        let stdin = child.stdin.take();
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| std::io::Error::other("拿不到服务的 stdout"))?;

        let instance = id.clone();
        std::thread::spawn(move || {
            let mut reader = NdjsonReader::new(BufReader::new(stdout));
            loop {
                match reader.next_line() {
                    Ok(Some(line)) => {
                        if tx
                            .send(Inbound::ServiceLine {
                                instance: instance.clone(),
                                generation,
                                line,
                            })
                            .is_err()
                        {
                            return; // 中控已经收摊
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        eprintln!("[ctrl] 读 {instance} 的 stdout 失败：{e}");
                        break;
                    }
                }
            }
            let _ = tx.send(Inbound::ServiceExited {
                instance,
                generation,
            });
        });

        Ok(Self {
            id,
            cmd,
            resident: false,
            owner: true,
            channel: Channel::Stdio {
                stdin,
                child,
            },
        })
    }

    /// **接上一个已经在跑的常驻服务**（不是自己拉起来的那个）。
    ///
    /// 它是"跨会话"这件事真正落地的地方：新会话拿到会合文件、连上去、
    /// 报出自己的 session —— 而**对面那个进程还是上一次那个**（内存里的东西都还在）。
    pub fn attach(
        id: String,
        cmd: Vec<String>,
        rv: &harness_ipc::Rendezvous,
        generation: u64,
        tx: Sender<Inbound>,
    ) -> std::io::Result<Self> {
        let conn = Conn::connect(&rv.addr, &rv.token)?;
        spawn_ipc_reader(id.clone(), conn.try_clone()?, generation, tx);
        Ok(Self {
            id,
            cmd,
            resident: true,
            owner: false,
            channel: Channel::Ipc {
                conn,
                pid: rv.pid,
                // **接上来的：没有句柄** —— 这个会话无权决定它的生死
                child: None,
            },
        })
    }

    /// **拉起一个常驻服务**：它不听 stdin/stdout，只写会合文件、等在套接字上。
    ///
    /// `grace_ms` 是等它把会合文件写出来的上限 —— 到了还没等到就**如实报错**
    /// （不假装成功，也不无限等）。
    pub fn spawn_resident(
        id: String,
        cmd: Vec<String>,
        sid: &str,
        rendezvous: &std::path::Path,
        grace_ms: u64,
        generation: u64,
        tx: Sender<Inbound>,
    ) -> std::io::Result<Self> {
        let (program, rest) = cmd
            .split_first()
            .ok_or_else(|| std::io::Error::other("服务命令为空"))?;
        // **它的 stderr 不能继承中控的**（2026-09-20，smoke 第一次跑就撞上了）：
        //
        // 常驻服务**活得比中控久**，而 stdio 那两条管子是父子共用的 ——
        // 中控退出后，它继承来的那根 stderr 管道**永远等不到 EOF**
        // （因为还有人在写），于是"读中控的 stderr 直到结束"这件事**永远不结束**。
        // 顺带还有更糟的一面：管子另一头没了，它每写一行日志都会撞 EPIPE。
        //
        // 所以常驻服务把日志写到**会合目录**里：那儿本来就是"想找它的人会去看"的地方，
        // 而且日志能活过所有会话（排查"它为什么没起来"时，往往它已经不是当前会话的了）。
        let log_path = harness_ipc::log_path_for(rendezvous, &id);
        if let Some(parent) = log_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)?;
        // **别让它顺走中控自己的输出管道**：它活得比中控久，管道就永远不到 EOF
        // （详见 `harness_ipc::inherit` 的那段注释）。这几毫秒里中控的标准句柄
        // 是不可继承的，拉完立刻还原。
        let _no_inherit = harness_service::inherit::NoInheritStdio::hide();
        let child = Command::new(program)
            .args(rest)
            .arg("--instance")
            .arg(&id)
            .arg("--sid")
            .arg(sid)
            .arg("--resident")
            .arg(rendezvous)
            // 常驻服务**不用这两条管子**：协议走套接字，日志走**文件**
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(log))
            .spawn()?;
        match harness_ipc::wait_ready(rendezvous, &id, grace_ms) {
            Some(rv) => {
                let conn = Conn::connect(&rv.addr, &rv.token)?;
                spawn_ipc_reader(id.clone(), conn.try_clone()?, generation, tx);
                // 宿主那份句柄留着 —— 用户真要 `stop` 的时候还得靠它
                Ok(Self {
                    id,
                    cmd,
                    resident: true,
                    owner: true,
                    // **句柄留着** —— 用户真要 `stop` 的时候，只有它能让这个进程死
                    channel: Channel::Ipc {
                        conn,
                        pid: rv.pid,
                        child: Some(child),
                    },
                })
            }
            None => {
                let mut child = child;
                let _ = child.kill();
                Err(std::io::Error::other(format!(
                    "`{id}` 起来了但 {grace_ms} ms 内没写出会合文件（{}）—— \
                     它没在听套接字，或者启动就失败了（看它的 stderr）",
                    rendezvous.display()
                )))
            }
        }
    }

    /// 给这个服务写一行协议消息。
    pub fn write(&mut self, line: &str) -> std::io::Result<()> {
        match &mut self.channel {
            Channel::Stdio { stdin, .. } => {
                let stdin = stdin
                    .as_mut()
                    .ok_or_else(|| std::io::Error::other("这个服务的 stdin 已经关了"))?;
                codec::write_line(stdin, line)
            }
            Channel::Ipc { conn, .. } => conn.write_line(line),
        }
    }

    /// 先礼后兵：关掉 stdin 让它自己收摊，等一小会儿；不走就动手。
    ///
    /// **别让用户手工收尸**（§5.15）。
    ///
    /// ⚠️ **常驻服务不在这儿收**（2026-09-20）：它存在的意义就是"会话结束了还在"。
    /// 这里只把连接放掉 —— 真要停它得走 `stop`（见 [`Self::stop_now`]）。
    pub fn shutdown(&mut self) {
        if self.resident {
            eprintln!(
                "[ctrl] {} 是常驻的：**这个会话收摊不动它**（{}）",
                self.id,
                if self.owner {
                    "它是这个会话起来的，但它本来就该活过这个会话"
                } else {
                    "它也不是这个会话起来的"
                }
            );
            return;
        }
        match &mut self.channel {
            Channel::Stdio { stdin, child } => {
                *stdin = None;
                let deadline = std::time::Instant::now() + std::time::Duration::from_millis(600);
                while std::time::Instant::now() < deadline {
                    if child.try_wait().ok().flatten().is_some() {
                        return;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                let _ = child.kill();
                let _ = child.wait();
            }
            // 非常驻的走套接字：放掉连接就完了（它是这个会话拉起来的，收摊时该收）
            Channel::Ipc { .. } => {}
        }
    }

    /// **真的把它收掉**（用户按了"停"）。
    ///
    /// 三种情况分开，因为**"我能不能让它死"取决于它是不是我起的**：
    ///
    /// | 情况 | 怎么办 |
    /// |---|---|
    /// | 我起的、不是常驻 | 关 stdin，不走就 kill |
    /// | **我起的常驻** | 直接 kill（宿主有那个句柄）|
    /// | **不是我起的** | 我手里**没有它的句柄** —— 交回给调用方，让它走"请它自己停"那条路（声明了 `stop` 入口的服务）|
    pub fn stop_now(&mut self) -> StopReport {
        match &mut self.channel {
            Channel::Stdio { stdin, child } => {
                *stdin = None;
                let deadline = std::time::Instant::now() + std::time::Duration::from_millis(600);
                while std::time::Instant::now() < deadline {
                    if child.try_wait().ok().flatten().is_some() {
                        return StopReport::Killed;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                let _ = child.kill();
                let _ = child.wait();
                StopReport::Killed
            }
            Channel::Ipc {
                child: Some(child),
                ..
            } => {
                let _ = child.kill();
                let _ = child.wait();
                StopReport::Killed
            }
            Channel::Ipc {
                pid,
                child: None,
                ..
            } => StopReport::NotMine { pid: *pid },
        }
    }

}

/// 用户按"停"之后发生了什么 —— **要如实告诉调用方**，不能假装都收掉了。
pub enum StopReport {
    /// 收掉了（进程没了）
    Killed,
    /// **不是我起的，我也没有它的句柄** —— 得请它自己停
    NotMine { pid: u32 },
}

/// 套接字那条通道的读线程：每一行交给中控，断了就报"它没了"。
fn spawn_ipc_reader(id: String, mut conn: Conn, generation: u64, tx: Sender<Inbound>) {
    std::thread::spawn(move || {
        loop {
            match conn.read_line() {
                Ok(Some(line)) => {
                    if line.trim().is_empty() {
                        continue;
                    }
                    if tx
                        .send(Inbound::ServiceLine {
                            instance: id.clone(),
                            generation,
                            line,
                        })
                        .is_err()
                    {
                        return; // 中控已经收摊
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    eprintln!("[ctrl] 读 {id} 的套接字失败：{e}");
                    break;
                }
            }
        }
        let _ = tx.send(Inbound::ServiceExited {
            instance: id,
            generation,
        });
    });
}
