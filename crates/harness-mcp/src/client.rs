//! MCP 客户端（JSON-RPC 2.0 over stdio）。
//!
//! 为什么要有自己的这一层、而不直接照抄某个 SDK：这一版要的东西很少
//! （`initialize` / `tools/list` / `tools/call`），而**超时和"孩子死了"这两件事
//! 必须是我们自己说了算的** —— 它们是"别把故障说成结论"那条纪律的落点：
//!
//! * 对端不回话 ⇒ `timeout`，调用方知道是"我不知道结果"，不是"它失败了"；
//! * 对端进程没了 ⇒ `child_down`，**立刻**能说清，不用等到超时。
//!
//! 所以读 stdout 的活儿交给一条读线程 + 带超时的 `recv`，而不是"阻塞读一行"。

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq)]
pub struct McpError {
    pub why: String,
    pub remedy: String,
    /// 机器可读的那一格：调用方要按它决定"能不能重试"
    pub code: &'static str,
}

impl McpError {
    fn new(code: &'static str, why: impl Into<String>, remedy: impl Into<String>) -> Self {
        Self {
            code,
            why: why.into(),
            remedy: remedy.into(),
        }
    }
}

/// 一个跑着的 MCP server（我们就它这一个客户端）。
pub struct McpClient {
    child: Child,
    stdin: Option<ChildStdin>,
    rx: Receiver<String>,
    next_id: u64,
    /// 对端自报的家门（`initialize` 的结果）
    pub server_info: Value,
    pub protocol_version: String,
}

impl McpClient {
    /// 把它拉起来。**这一步不做握手** —— 握手是 [`Self::initialize`] 的事，
    /// 因为"起得来"和"谈得成"是两个结论，报错时要能分开说。
    pub fn start(cmd: &[String]) -> Result<Self, McpError> {
        let (program, rest) = cmd.split_first().ok_or_else(|| {
            McpError::new(
                "invalid",
                "没有给 MCP server 的命令",
                "用 `--server <可执行文件>` 加若干 `--arg <参数>` 指一个 MCP server",
            )
        })?;
        let mut child = Command::new(program)
            .args(rest)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // MCP server 的 stderr 是它的日志：**原样转出去**，别吞
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| {
                McpError::new(
                    "child_down",
                    format!("拉不起 MCP server（{program}）：{e}"),
                    "检查那个可执行文件在不在、参数对不对",
                )
            })?;
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().ok_or_else(|| {
            McpError::new("invalid", "拿不到 MCP server 的 stdout", "这是环境问题")
        })?;
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                let mut line = String::new();
                match reader.read_line(&mut line) {
                    Ok(0) => break,
                    Ok(_) => {
                        let t = line.trim();
                        if t.is_empty() {
                            continue;
                        }
                        if tx.send(t.to_string()).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
            // 线程结束 ⇒ 发送端被丢弃 ⇒ 这边 `recv` 会拿到 Disconnected
        });
        Ok(Self {
            child,
            stdin,
            rx,
            next_id: 0,
            server_info: Value::Null,
            protocol_version: String::new(),
        })
    }

    /// 我们支持的协议版本（发了它，对端自己挑一个它支持的）。
    pub const PROTOCOL: &'static str = "2024-11-05";

    /// 握手：`initialize` + `notifications/initialized`。
    pub fn initialize(&mut self, timeout: Duration) -> Result<(), McpError> {
        let params = json!({
            "protocolVersion": Self::PROTOCOL,
            "capabilities": {},
            "clientInfo": {"name": "instance-harness", "version": env!("CARGO_PKG_VERSION")},
        });
        let res = self.request("initialize", params, timeout)?;
        self.protocol_version = res
            .get("protocolVersion")
            .and_then(Value::as_str)
            .unwrap_or(Self::PROTOCOL)
            .to_string();
        self.server_info = res.get("serverInfo").cloned().unwrap_or(Value::Null);
        // 规矩：initialize 之后要发一条**通知**（没有 id、不要回信）
        self.notify("notifications/initialized", json!({}))?;
        Ok(())
    }

    /// 问它有哪些工具。**启动时问一次**，之后就是我们的工具表（§49 的答案）。
    pub fn list_tools(&mut self, timeout: Duration) -> Result<Vec<Value>, McpError> {
        let res = self.request("tools/list", json!({}), timeout)?;
        Ok(res
            .get("tools")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default())
    }

    /// 调一个工具。返回 MCP 的**原始 result** —— 怎么映射成我们的结果，
    /// 是上面那一层的事（桥不该替它决定）。
    pub fn call_tool(
        &mut self,
        name: &str,
        args: Value,
        timeout: Duration,
    ) -> Result<Value, McpError> {
        self.request(
            "tools/call",
            json!({"name": name, "arguments": args}),
            timeout,
        )
    }

    /// 它还在吗？（问的是进程，不是端口 —— 这个对端就是我们的孩子）
    pub fn alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    fn notify(&mut self, method: &str, params: Value) -> Result<(), McpError> {
        let msg = json!({"jsonrpc": "2.0", "method": method, "params": params});
        self.write(&msg)
    }

    fn write(&mut self, msg: &Value) -> Result<(), McpError> {
        let stdin = self.stdin.as_mut().ok_or_else(|| {
            McpError::new("child_down", "它的 stdin 已经关了", "它已经没了")
        })?;
        let line = serde_json::to_string(msg).map_err(|e| {
            McpError::new("invalid", format!("这条发不出去：{e}"), "这是桥自己的问题")
        })?;
        stdin.write_all(line.as_bytes()).and_then(|_| stdin.write_all(b"\n")).and_then(|_| stdin.flush()).map_err(|e| {
            McpError::new(
                "child_down",
                format!("往 MCP server 写不进去：{e}"),
                "多半是它已经退出了 —— 看它的 stderr",
            )
        })
    }

    fn request(
        &mut self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, McpError> {
        self.next_id += 1;
        let id = self.next_id;
        let msg = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        self.write(&msg)?;
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(McpError::new(
                    "timeout",
                    // **"我不知道"这句必须在 why 里** —— 它是这条错的**意思**，
                    // 而 remedy 说的是下一步怎么办。两样别混。
                    format!(
                        "`{method}` 等了 {} ms 没回话 —— **我不知道它做没做**",
                        timeout.as_millis()
                    ),
                    "别当成失败，也别直接重试（它可能正在做）。先看它的状态或它的 stderr；\
                     真要重试，得先确认这件事是幂等的",
                ));
            }
            match self.rx.recv_timeout(left) {
                Ok(line) => {
                    let v: Value = match serde_json::from_str(&line) {
                        Ok(v) => v,
                        Err(e) => {
                            // 对端吐了不合规的一行：**说清**，别静默跳过 ——
                            // 否则表现成"永远等不到回信"，而那是两回事
                            return Err(McpError::new(
                                "invalid",
                                format!("MCP server 发了一行不是 JSON 的东西：{e}（{}）", truncate(&line)),
                                "这是对端的问题；去看它的 stderr 和版本",
                            ));
                        }
                    };
                    // 不是回给我们这条的（通知、别条的回复）—— 忽略
                    if v.get("id").and_then(Value::as_u64) != Some(id) {
                        continue;
                    }
                    if let Some(err) = v.get("error") {
                        return Err(McpError::new(
                            "refused",
                            format!(
                                "MCP 那边回了错：{}（code {}）",
                                err.get("message").and_then(Value::as_str).unwrap_or("(没写)"),
                                err.get("code").and_then(Value::as_i64).unwrap_or(0)
                            ),
                            "这是**对端**的拒绝，不是我们的：按它的说法改；它说工具不存在就 `tools/list` 看一眼",
                        ));
                    }
                    return Ok(v.get("result").cloned().unwrap_or(Value::Null));
                }
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(McpError::new(
                        "child_down",
                        "MCP server 没了（stdout 到了 EOF）",
                        "它退出或者崩了；去看它的 stderr。**在它回来之前，这些工具都调不动**",
                    ))
                }
            }
        }
    }
}

impl Drop for McpClient {
    fn drop(&mut self) {
        // 先礼后兵：关 stdin 让它自己收摊（MCP 的 stdio 传输里，EOF 就是"客户端走了"）
        self.stdin = None;
        let deadline = Instant::now() + Duration::from_millis(500);
        while Instant::now() < deadline {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// MCP 的 `content` 数组 → 我们能塞进上下文的文本。
///
/// ⚠️ **不是所有 content 都是文本**：图片 / 资源链接那些我们这一版不认识。
/// 认不出来的**不许假装看见** —— 留一行说清"这里有一样东西我看不懂"，
/// 而不是静默丢掉（丢了的话，模型会以为对端什么都没返回）。
pub fn content_to_text(content: &Value) -> String {
    let Some(items) = content.as_array() else {
        return String::new();
    };
    let mut out: Vec<String> = Vec::new();
    for it in items {
        match it.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(t) = it.get("text").and_then(Value::as_str) {
                    out.push(t.to_string());
                }
            }
            Some(other) => out.push(format!(
                "（这一块 MCP 内容是 `{other}`，我这一版不认识它 —— \
                 桥只搬文本，别的类型原样留在 data.mcp 里）"
            )),
            None => out.push("（MCP 有一块内容没写 type）".to_string()),
        }
    }
    out.join("\n")
}

fn truncate(s: &str) -> String {
    if s.chars().count() <= 200 {
        s.to_string()
    } else {
        let head: String = s.chars().take(200).collect();
        format!("{head}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_content_comes_out_as_text() {
        let c = json!([{"type": "text", "text": "第一块"}, {"type": "text", "text": "第二块"}]);
        assert_eq!(content_to_text(&c), "第一块\n第二块");
    }

    /// **认不出来的内容不许假装看见了** —— 留一行说清。
    #[test]
    fn unknown_content_is_admitted_not_swallowed() {
        let c = json!([{"type": "image", "data": "..."}]);
        let t = content_to_text(&c);
        assert!(t.contains("image"), "{t}");
        assert!(t.contains("不认识"), "{t}");
    }

    #[test]
    fn a_missing_type_is_admitted_too() {
        let c = json!([{"data": "x"}]);
        assert!(content_to_text(&c).contains("没写 type"));
    }
}
