//! 三条不变量（规范 §0）—— **写进代码，不是文档里的一句希望**。
//!
//! | 不变量 | 在这里怎么落地 |
//! |---|---|
//! | 一、传输无关 | **构建期**：一条测试查 `harness-core` 的依赖表里没有传输 |
//! | 二、可重放 | [`assert_recorded_before_delivery`]：先入日志再投递 |
//! | 三、文本可达 | [`assert_text_reachable`]：每种消息都能过一行文本往返 |

use crate::codec;
use crate::envelope::{Authority, Envelope, MessageType};

// ⚠️ **不变量一不能靠运行时断言。**
//
// 它原来是这么写的：
//
//     pub fn assert_transport_agnostic() -> Result<(), String> { Ok(()) }
//
// **那不是断言，那是让文档里那句话看起来成立。** 而且比不写更糟：
// 文档说"三条不变量写进代码，不是文档里的一句希望"，
// 其中一条就真的只是一句希望。
//
// 不变量一的本质是**结构性的**：协议层不许认识任何传输。
// 那它就**没法在运行时验** —— 得在**构建期**验：看 `harness-core` 依赖了什么。
// 所以它现在是一条测试（见下面 `no_transport_in_the_protocol_layer`），
// 谁往 core 里塞 tokio / reqwest / 管道，谁就红。

/// 不变量二：**模型可见即已记录**。
///
/// 用法是"投递前问一句"，而不是事后检查 —— 事后已经没有补救机会。
pub fn assert_recorded_before_delivery(seq: Option<u64>, env: &Envelope) -> Result<u64, String> {
    seq.ok_or_else(|| {
        format!(
            "想投递 {}（{} → {}）但它还没有 seq —— 违反不变量二：\
             抵达模型请求的一切都必须能先从日志重建（规范 §4）",
            env.id, env.from, env.to
        )
    })
}

/// 不变量三：每种消息都过一遍一行文本往返。
///
/// 启动时跑一次。**代价是七次序列化**，换来的是"不会在阶段 4 才发现某种消息读不出来"。
pub fn assert_text_reachable(sid: &str) -> Result<(), String> {
    for kind in MessageType::ALL {
        let sample = Envelope {
            id: "m-invariant".into(),
            ts: 0,
            kind,
            from: "invariant".into(),
            to: "invariant".into(),
            corr: Some("c-invariant".into()),
            sid: sid.to_string(),
            authority: Authority::System,
            payload: serde_json::json!({}),
        };
        let line = codec::encode(&sample).map_err(|e| format!("{kind:?} 编码失败：{e}"))?;
        let back = codec::decode(&line).map_err(|e| format!("{kind:?} 解码失败：{e}"))?;
        if back != sample {
            return Err(format!("{kind:?} 往返之后不相等 —— 文本不可达"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_two_runtime_invariants_hold_at_startup() {
        // 不变量二（可重放）由调用方的顺序保证，这里验第三条
        assert_text_reachable("s-1").unwrap();
    }

    /// **不变量一：协议层不认识任何传输** —— 构建期断言。
    ///
    /// 做法是查自己的依赖表。为什么这么做：这条不变量是**结构性**的，
    /// 运行时再怎么断言都只是自说自话；而"core 有没有依赖传输"是**可查的事实**。
    ///
    /// 谁往 `harness-core` 里塞 tokio / reqwest / std::net / 管道，这条测试就红。
    #[test]
    fn no_transport_in_the_protocol_layer() {
        let manifest = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml"),
        )
        .expect("读自己的 Cargo.toml");
        // 只允许这些：它们是**数据格式**的库，不是传输
        let allowed = ["serde", "serde_json", "serde_core", "serde_derive"];
        let mut section = String::new();
        for line in manifest.lines() {
            let l = line.trim();
            if l.starts_with('[') {
                section = l.to_string();
                continue;
            }
            if !section.contains("dependencies") || l.is_empty() || l.starts_with('#') {
                continue;
            }
            let name = l.split(['=', '.']).next().unwrap_or("").trim();
            assert!(
                allowed.contains(&name),
                "`harness-core` 里出现了 `{name}` —— 协议层不许认识传输（规范 §0 不变量一）。\
                 传输是**实现**，不是协议的一部分：把它放到用它的那一层去"
            );
        }
    }

    #[test]
    fn delivery_without_seq_is_refused() {
        let env = Envelope {
            id: "m-1".into(),
            ts: 0,
            kind: MessageType::Result,
            from: "echo:e-1".into(),
            to: "console".into(),
            corr: Some("c-1".into()),
            sid: "s-1".into(),
            authority: Authority::Observe,
            payload: serde_json::json!({}),
        };
        assert!(assert_recorded_before_delivery(Some(7), &env).is_ok());
        assert!(assert_recorded_before_delivery(None, &env).is_err());
    }
}
