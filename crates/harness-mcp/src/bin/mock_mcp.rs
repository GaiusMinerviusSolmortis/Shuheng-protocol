//! **测试用的最小 MCP server**（JSON-RPC 2.0 over stdio）。
//!
//! 为什么把它放在这个 crate 里：它只服务于这个 crate 的验收，
//! 而且**它本身就是那份协议的可读样本** —— 想接自己的 MCP server 的人，
//! 看这一百多行比看规范快。
//!
//! 它实现四个工具，每个都对着一条要证的判据：
//!
//! | 工具 | 演什么 |
//! |---|---|
//! | `echo` | 正常路径：把给你的文本原样回在 `content` 里 |
//! | `add` | 结构化参数（两个整数）|
//! | `boom` | **工具自己报错**：`isError: true` —— 要证"工具失败 ≠ 调用失败" |
//! | `crash` | **对端进程直接死**（不回话）—— 要证"对端没了立刻说得清，不用等超时"|
//! | `sleep` | 睡一会儿再回 —— 要证**超时是"不知道结果"，不是"它失败了"** |
//!
//! 另外：不认识的工具回 **JSON-RPC error**（不是 isError）—— 两者在我们的桥里
//! 映射成不同的东西，得分别测。
//!
//! 用法：`mock-mcp`（从 stdin 读、往 stdout 写）。

use serde_json::{json, Value};
use std::io::{BufRead, Write};

fn main() {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(t) else {
            // 坏行不该掐死它 —— 同我们那边"坏行跳过"的纪律
            eprintln!("[mock-mcp] 跳过一行不是 JSON 的输入");
            continue;
        };
        let method = v.get("method").and_then(Value::as_str).unwrap_or("");
        let id = v.get("id").cloned();
        // **通知没有 id**：不要回信（回了就是协议违规）
        if id.is_none() {
            eprintln!("[mock-mcp] 收到通知 {method}");
            continue;
        }
        let id = id.unwrap();
        let reply = match method {
            "initialize" => json!({
                "jsonrpc": "2.0", "id": id,
                "result": {
                    // 故意回一个**比请求低**的版本：看桥认不认
                    "protocolVersion": "2024-11-05",
                    "capabilities": {"tools": {"listChanged": false}},
                    "serverInfo": {"name": "mock-mcp", "version": "0.1.0"}
                }
            }),
            "tools/list" => json!({
                "jsonrpc": "2.0", "id": id,
                "result": {"tools": [
                    {"name": "echo", "description": "把它给你的文本原样回给你。",
                     "inputSchema": {"type": "object", "required": ["text"],
                                     "properties": {"text": {"type": "string"}}}},
                    {"name": "add", "description": "两个整数相加。",
                     "inputSchema": {"type": "object", "required": ["a", "b"],
                                     "properties": {"a": {"type": "integer"},
                                                    "b": {"type": "integer"}}}},
                    {"name": "boom", "description": "**按剧本报错**：工具说自己失败了。",
                     "inputSchema": {"type": "object", "properties": {}}},
                    {"name": "crash", "description": "**按剧本把对端进程干掉**（不回话）。",
                     "inputSchema": {"type": "object", "properties": {}}},
                    {"name": "sleep", "description": "睡一会儿再回。",
                     "inputSchema": {"type": "object", "required": ["ms"],
                                     "properties": {"ms": {"type": "integer"}}}}
                ]}
            }),
            "tools/call" => {
                let name = v
                    .get("params")
                    .and_then(|p| p.get("name"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let args = v
                    .get("params")
                    .and_then(|p| p.get("arguments"))
                    .cloned()
                    .unwrap_or(json!({}));
                match name {
                    "echo" => json!({
                        "jsonrpc": "2.0", "id": id,
                        "result": {"content": [{"type": "text",
                            "text": args.get("text").and_then(Value::as_str).unwrap_or("")}],
                            "isError": false}
                    }),
                    "add" => {
                        let a = args.get("a").and_then(Value::as_i64).unwrap_or(0);
                        let b = args.get("b").and_then(Value::as_i64).unwrap_or(0);
                        json!({
                            "jsonrpc": "2.0", "id": id,
                            "result": {"content": [{"type": "text", "text": format!("{}", a + b)},
                                                   // 顺带塞一块**不是文本**的内容：
                                                   // 看桥会不会假装看见了
                                                   {"type": "image", "data": "aGVsbG8="}],
                                       "isError": false}
                        })
                    }
                    // **工具自己失败**：这是 `result` 里的 `isError`，不是 JSON-RPC error
                    "boom" => json!({
                        "jsonrpc": "2.0", "id": id,
                        "result": {"content": [{"type": "text", "text": "它炸了：这是工具自己的错"}],
                                   "isError": true}
                    }),
                    "crash" => {
                        eprintln!("[mock-mcp] 按剧本把自己干掉（不回话）");
                        std::process::exit(9);
                    }
                    "sleep" => {
                        let ms = args.get("ms").and_then(Value::as_u64).unwrap_or(0);
                        std::thread::sleep(std::time::Duration::from_millis(ms));
                        json!({
                            "jsonrpc": "2.0", "id": id,
                            "result": {"content": [{"type": "text",
                                "text": format!("睡完了 {ms} ms")}], "isError": false}
                        })
                    }
                    other => json!({
                        "jsonrpc": "2.0", "id": id,
                        "error": {"code": -32602, "message": format!("没有名为 `{other}` 的工具")}
                    }),
                }
            }
            other => json!({
                "jsonrpc": "2.0", "id": id,
                "error": {"code": -32601, "message": format!("不认识的方法 `{other}`")}
            }),
        };
        let Ok(text) = serde_json::to_string(&reply) else {
            continue;
        };
        if out.write_all(text.as_bytes()).is_err() || out.write_all(b"\n").is_err() {
            break;
        }
        let _ = out.flush();
    }
}
