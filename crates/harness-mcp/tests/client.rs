//! MCP 客户端的验收 —— 对着**真的子进程**（`mock-mcp`）跑，不是对着假货。

use harness_mcp::client::{content_to_text, McpClient};
use serde_json::json;
use std::time::Duration;

fn mock() -> Vec<String> {
    // cargo 给集成测试的现成路径：同一个包里那个 bin
    vec![env!("CARGO_BIN_EXE_mock-mcp").to_string()]
}

fn started() -> McpClient {
    let mut c = McpClient::start(&mock()).expect("起得来");
    c.initialize(Duration::from_secs(5)).expect("握得成");
    c
}

#[test]
fn handshake_and_tool_list() {
    let mut c = started();
    let tools = c.list_tools(Duration::from_secs(5)).unwrap();
    let names: Vec<&str> = tools
        .iter()
        .filter_map(|t| t.get("name").and_then(|n| n.as_str()))
        .collect();
    assert!(names.contains(&"echo"), "{names:?}");
    assert!(names.contains(&"boom"), "{names:?}");
    // 对端自报的家门也拿到了（排查"对面是谁"时要用）
    assert_eq!(c.server_info["name"], json!("mock-mcp"));
    assert_eq!(c.protocol_version, "2024-11-05");
    // 参数表要原样带过来 —— 它是模型填参数的唯一依据
    let echo = tools.iter().find(|t| t["name"] == json!("echo")).unwrap();
    assert_eq!(echo["inputSchema"]["required"], json!(["text"]));
}

#[test]
fn a_normal_call_comes_back_as_content() {
    let mut c = started();
    let r = c
        .call_tool("echo", json!({"text": "你好，MCP"}), Duration::from_secs(5))
        .unwrap();
    assert_eq!(r["isError"], json!(false));
    assert_eq!(content_to_text(&r["content"]), "你好，MCP");
}

/// **工具自己说"我失败了"不算调用失败**：`isError` 是结果里的数据，
/// 客户端要把它当**成功拿到的一条结果**返回（映射成什么形态是上层的事）。
#[test]
fn is_error_is_data_not_a_transport_error() {
    let mut c = started();
    let r = c
        .call_tool("boom", json!({}), Duration::from_secs(5))
        .expect("它**确实回了**，所以这不是 Err");
    assert_eq!(r["isError"], json!(true));
    assert!(content_to_text(&r["content"]).contains("炸了"));
}

/// 认不出来的内容块**不许假装看见了**（图片那块要留一行说清）。
#[test]
fn unknown_content_blocks_are_admitted() {
    let mut c = started();
    let r = c
        .call_tool("add", json!({"a": 1, "b": 2}), Duration::from_secs(5))
        .unwrap();
    let text = content_to_text(&r["content"]);
    assert!(text.contains('3'), "{text}");
    assert!(text.contains("image"), "要说清那块不是文本：{text}");
    // 原始内容一字不改地留着（上层能把它放进 data.mcp）
    assert_eq!(r["content"].as_array().unwrap().len(), 2);
}

#[test]
fn an_unknown_tool_is_the_peers_refusal_not_ours() {
    let mut c = started();
    let e = c
        .call_tool("没有这个", json!({}), Duration::from_secs(5))
        .unwrap_err();
    assert_eq!(e.code, "refused");
    assert!(e.why.contains("没有这个"), "{}", e.why);
}

/// **对端没了要立刻说得清**，不能等超时 —— 而且不能跟"超时"混成一个码。
#[test]
fn a_dead_peer_is_reported_immediately_and_distinctly() {
    let mut c = started();
    // `crash` 会把自己干掉，不回话
    let _ = c.call_tool("crash", json!({}), Duration::from_millis(3000));
    let t0 = std::time::Instant::now();
    let e = c
        .call_tool("echo", json!({"text": "还在吗"}), Duration::from_millis(5000))
        .unwrap_err();
    let took = t0.elapsed();
    assert_eq!(e.code, "child_down", "{}", e.why);
    assert!(
        took < Duration::from_millis(2500),
        "对端没了该**立刻**知道，不该等满超时（花了 {took:?}）"
    );
}

/// **超时是"我不知道结果"**，不是"它失败了" —— 码要分开，话也要分开。
#[test]
fn a_timeout_is_its_own_code_and_says_i_dont_know() {
    let mut c = started();
    let t0 = std::time::Instant::now();
    let e = c
        .call_tool("sleep", json!({"ms": 1500}), Duration::from_millis(200))
        .unwrap_err();
    assert_eq!(e.code, "timeout");
    assert!(e.why.contains("不知道"), "{}", e.why);
    assert!(e.remedy.contains("别"), "{}", e.remedy);
    assert!(t0.elapsed() < Duration::from_millis(1200), "该在超时点就返回");
    // 而且**超时之后它还能用**（那条慢回复会被当成"不是回给这条的"忽略掉）
    let r = c
        .call_tool("echo", json!({"text": "超时之后"}), Duration::from_secs(5))
        .unwrap();
    assert_eq!(content_to_text(&r["content"]), "超时之后");
}

#[test]
fn starting_a_nonexistent_server_says_so() {
    let e = match McpClient::start(&["这个可执行文件不存在-please".to_string()]) {
        Ok(_) => panic!("那个可执行文件不存在，不该起得来"),
        Err(e) => e,
    };
    assert_eq!(e.code, "child_down");
    assert!(e.remedy.contains("在不在"), "{}", e.remedy);
}
