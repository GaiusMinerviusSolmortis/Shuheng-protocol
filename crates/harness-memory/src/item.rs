//! 上下文里都放了些什么。
//!
//! 这里**不建模 tool call 的参数结构** —— 记忆内核只需要知道
//! "这有东西、占多少 token、切的时候能不能把它和它的另一半分开"。

use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemKind {
    /// 系统提示。**永远不参与压缩**（它是那段稳定的前缀）
    System,
    User,
    Assistant,
    /// 一次工具调用。**它和它的结果必须同生共死**
    ToolCall { name: String },
    /// 工具调用的结果
    ToolResult { name: String },
    /// 敕令：中控塞进上下文的那条（§4.4）
    Injected,
    /// 上一次压缩留下的摘要
    Summary,
}

impl ItemKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            ItemKind::System => "system",
            ItemKind::User => "user",
            ItemKind::Assistant => "assistant",
            ItemKind::ToolCall { .. } => "tool_call",
            ItemKind::ToolResult { .. } => "tool_result",
            ItemKind::Injected => "injected",
            ItemKind::Summary => "summary",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Item {
    pub kind: ItemKind,
    pub text: String,
    /// **协议细节**：给适配器重建 provider 报文用的。
    ///
    /// 记忆内核只关心 `kind` 和 `text`（它要切的、要算的都是这两样），
    /// 但真模型要 `tool_call_id` 才能把结果配回那次调用 —— 那东西没地方放
    /// 就会逼着内核长出一堆 provider 特有的字段。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl Item {
    pub fn new(kind: ItemKind, text: impl Into<String>) -> Self {
        Self {
            kind,
            text: text.into(),
            data: None,
        }
    }

    pub fn with_data(mut self, data: Value) -> Self {
        self.data = Some(data);
        self
    }

    pub fn user(text: impl Into<String>) -> Self {
        Self::new(ItemKind::User, text)
    }

    pub fn assistant(text: impl Into<String>) -> Self {
        Self::new(ItemKind::Assistant, text)
    }

    pub fn tool_call(name: &str, args: &str) -> Self {
        Self::new(
            ItemKind::ToolCall { name: name.into() },
            format!("{name}({args})"),
        )
    }

    pub fn tool_result(name: &str, text: impl Into<String>) -> Self {
        Self::new(ItemKind::ToolResult { name: name.into() }, text)
    }

    /// 带协议细节的两条：适配器要靠 `call_id` 把结果配回调用。
    pub fn tool_call_with(name: &str, args: &Value, call_id: &str) -> Self {
        Self::new(
            ItemKind::ToolCall { name: name.into() },
            format!("{name}({args})"),
        )
        .with_data(serde_json::json!({"call_id": call_id, "args": args}))
    }

    pub fn tool_result_with(name: &str, text: impl Into<String>, call_id: &str) -> Self {
        Self::tool_result(name, text).with_data(serde_json::json!({"call_id": call_id}))
    }

    /// 给摘要看的那一行（**不是给模型看的**，所以可以带标记）
    pub fn render(&self) -> String {
        match &self.kind {
            ItemKind::ToolCall { name } => format!("[调用 {name}] {}", self.text),
            ItemKind::ToolResult { name } => format!("[结果 {name}] {}", self.text),
            other => format!("[{}] {}", other.as_str(), self.text),
        }
    }
}
