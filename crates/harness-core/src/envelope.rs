//! 信封（规范-消息协议 §1）与消息种类（§2）。

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// §2 消息种类 —— **闭合枚举**。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageType {
    /// 调用方 → 被调方：发起一次操作
    Request,
    /// 被调方 → 调用方：流式分片
    Chunk,
    /// 被调方 → 调用方：终态（成功与失败同构）
    Result,
    /// 调用方 → 被调方：中止
    Cancel,
    /// 任意 → 订阅方：主动通知（"这里有个事实，你记一笔"）
    Event,
    /// 中控 → 智能体：**把它放进你的上下文**（敕令）
    Inject,
    /// 实例 → 中控：上线 / 下线 / 健康
    Presence,
}

impl MessageType {
    pub const ALL: [MessageType; 7] = [
        MessageType::Request,
        MessageType::Chunk,
        MessageType::Result,
        MessageType::Cancel,
        MessageType::Event,
        MessageType::Inject,
        MessageType::Presence,
    ];
}

/// §1.1 权威级别。
///
/// **声明顺序即强弱**：`Observe < Agent < System < User`。
/// 中控用 `min(发送方声明, 该实例的能力上限)` 实现"**只准降、不准升**"。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Authority {
    /// 观察结果 —— **只是事实，不是指令**
    Observe,
    /// 另一个智能体 —— **只能提意见**，哪怕它说得很确定
    Agent,
    /// 中控自己（如敕令）
    System,
    /// 用户的直接指令 —— **只有它能下指令**
    User,
}

impl Authority {
    pub fn as_str(self) -> &'static str {
        match self {
            Authority::Observe => "observe",
            Authority::Agent => "agent",
            Authority::System => "system",
            Authority::User => "user",
        }
    }
}

/// §1 信封。
///
/// **`seq` 不在里面** —— 它是落盘顺序，由中控写日志时分配（§5）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Envelope {
    /// 消息自身唯一，前缀 `m-`
    pub id: String,
    /// Unix 毫秒
    pub ts: i64,
    #[serde(rename = "type")]
    pub kind: MessageType,
    /// 发送方实例 id
    pub from: String,
    /// 接收方实例 id；`"*"` 表示广播
    pub to: String,
    /// 因果链 id：一次调用 → 若干分片 → 一个结果，共用它
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub corr: Option<String>,
    pub sid: String,
    pub authority: Authority,
    /// 种类相关的载荷。**可为空对象，不可缺**
    pub payload: Value,
}

/// 信封校验失败。**每条错误都带"该怎么补救"**（规范-工具服务 §11.5 纪律二）。
#[derive(Debug, Clone, PartialEq)]
pub struct EnvelopeError {
    pub field: &'static str,
    pub why: String,
    pub remedy: String,
}

impl EnvelopeError {
    pub fn new(field: &'static str, why: impl Into<String>, remedy: impl Into<String>) -> Self {
        Self {
            field,
            why: why.into(),
            remedy: remedy.into(),
        }
    }
}

impl std::fmt::Display for EnvelopeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "`{}` 不合规：{}。{}", self.field, self.why, self.remedy)
    }
}

impl std::error::Error for EnvelopeError {}

impl Envelope {
    /// 逐条断言规范 §1 表格里的约束。
    pub fn validate(&self) -> Result<(), EnvelopeError> {
        if self.id.is_empty() {
            return Err(EnvelopeError::new(
                "id",
                "为空",
                "每条消息都要有自己的 id，前缀 `m-`",
            ));
        }
        if !self.id.starts_with("m-") {
            return Err(EnvelopeError::new(
                "id",
                format!("`{}` 没有 `m-` 前缀", self.id),
                "消息 id 必须写成 `m-<唯一后缀>`（规范 §1）",
            ));
        }
        if self.from.is_empty() {
            return Err(EnvelopeError::new("from", "为空", "填发送方实例 id"));
        }
        if self.to.is_empty() {
            return Err(EnvelopeError::new(
                "to",
                "为空",
                "填接收方实例 id；广播写 `\"*\"`",
            ));
        }
        if self.sid.is_empty() {
            return Err(EnvelopeError::new(
                "sid",
                "为空",
                "填会话 id（中控启动时分配）",
            ));
        }
        if let Some(corr) = &self.corr {
            if corr.is_empty() {
                return Err(EnvelopeError::new(
                    "corr",
                    "存在但为空串",
                    "要么省略这个字段，要么填一个非空的因果链 id",
                ));
            }
        }
        if !self.payload.is_object() {
            return Err(EnvelopeError::new(
                "payload",
                "不是 JSON 对象",
                "payload 必须是对象；没有内容就写 `{}`（不可缺、不可为 null）",
            ));
        }
        Ok(())
    }

    /// §1.1 的"只准降不准升"。
    pub fn clamped_to(self, ceiling: Authority) -> Self {
        if self.authority > ceiling {
            Self {
                authority: ceiling,
                ..self
            }
        } else {
            self
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authority_order_is_observe_lowest_user_highest() {
        assert!(Authority::Observe < Authority::Agent);
        assert!(Authority::Agent < Authority::System);
        assert!(Authority::System < Authority::User);
    }

    #[test]
    fn clamping_only_downgrades() {
        let env = Envelope {
            id: "m-1".into(),
            ts: 0,
            kind: MessageType::Event,
            from: "echo:e-1".into(),
            to: "ctrl".into(),
            corr: None,
            sid: "s-1".into(),
            authority: Authority::User,
            payload: serde_json::json!({}),
        };
        assert_eq!(env.clone().clamped_to(Authority::Observe).authority, Authority::Observe);
        assert_eq!(env.clamped_to(Authority::System).authority, Authority::System);
    }
}
