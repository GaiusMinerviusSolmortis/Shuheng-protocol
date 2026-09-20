//! 服务级实例的进程管理（§5.10、§5.16）。
//!
//! 形状照 06-操控 §14.14 那条结论：**一个服务一个进程，行分隔 JSON 对话**。
//! 这里只做最薄的一层：拉起来、给 stdin 写一行、把 stdout 的每一行交出去。

use harness_core::codec::{self, NdjsonReader};
use std::io::BufReader;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::Sender;

use crate::Inbound;

pub struct ServiceHandle {
    pub id: String,
    /// 拉起它用的命令行 —— 图上那个节点点开要看得见（§11.1 E 组）
    pub cmd: Vec<String>,
    stdin: Option<ChildStdin>,
    child: Child,
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
            stdin,
            child,
        })
    }

    /// 给这个服务写一行协议消息。
    pub fn write(&mut self, line: &str) -> std::io::Result<()> {
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| std::io::Error::other("这个服务的 stdin 已经关了"))?;
        codec::write_line(stdin, line)
    }

    pub fn try_wait(&mut self) -> Option<std::process::ExitStatus> {
        self.child.try_wait().ok().flatten()
    }

    /// 先礼后兵：关掉 stdin 让它自己收摊，等一小会儿；不走就动手。
    ///
    /// **别让用户手工收尸**（§5.15）。
    pub fn shutdown(&mut self) {
        self.stdin = None;
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(600);
        while std::time::Instant::now() < deadline {
            if self.try_wait().is_some() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
