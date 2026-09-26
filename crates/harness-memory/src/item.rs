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
    /// **回忆**：内核从长期记忆里捞出来放进上下文的那条（2026-09-26 加）。
    ///
    /// 它和 `Injected` 分开记，是因为**来源不一样**：敕令是**别人**塞进来的
    /// （中控，权威 `system`），回忆是**内核自己**捞出来的 —— 排查"它为什么
    /// 突然提起一件很久以前的事"时，这两者要分得开。
    Recalled,
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
            ItemKind::Recalled => "recalled",
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
    ///
    /// **`instance` 必须在场**（2026-09-20 修）：适配器回放这条消息时，函数名是按
    /// `fn_name(instance, tool)` **重建**的 —— 少了实例，重建出来就是 `__edit`
    /// 这种丢了前缀的名字。而模型会**照着自己上一轮的调用学**，于是它下一轮
    /// 真的会叫出一个不存在的名字（实跑里就这么发生的）。
    pub fn tool_call_with(instance: &str, name: &str, args: &Value, call_id: &str) -> Self {
        Self::new(
            ItemKind::ToolCall { name: name.into() },
            format!("{name}({args})"),
        )
        .with_data(serde_json::json!({
            "call_id": call_id, "instance": instance, "args": args
        }))
    }

    /// **原样回放模型那一条调用**（名字/参数可能根本解不开）。
    ///
    /// 用在"这条调用不成立、我们回了一条报错即手册"那条路上：provider 要求
    /// **`tool` 消息必须跟在一条带 `tool_calls` 的 assistant 消息后面** ——
    /// 少了那一条，整个请求会被直接拒（HTTP 400）。
    /// 既然是回放，就**照它说的原样**回放：名字用 `raw_name`，参数用 `args_raw`。
    pub fn tool_call_echo(
        instance: &str,
        name: &str,
        raw_name: &str,
        raw_args: Option<&str>,
        call_id: &str,
    ) -> Self {
        let mut d = serde_json::json!({
            "call_id": call_id, "instance": instance, "args": serde_json::Value::Null
        });
        d["raw_name"] = serde_json::json!(raw_name);
        if let Some(raw) = raw_args {
            d["args_raw"] = serde_json::json!(raw);
        }
        Self::new(ItemKind::ToolCall { name: name.into() }, format!("{name}(…)"))
            .with_data(d)
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
