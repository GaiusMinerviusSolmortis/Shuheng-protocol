//! **实例事件**：服务告诉中控"我现在开着哪些实例、它们什么状态"。
//!
//! 形状来自 §5.10 那句：**静态的是"我能干什么"（manifest），
//! 动态的是"我现在开着什么"（事件流）**。
//!
//! 在这之前，三个服务各自发明了一套 `event` 载荷（`state: "opened"` /
//! `"held"` / `"released"`……）—— 信息在流里，但**中控接不住**，
//! 因为它没法用一个形状去认。于是图上永远只有服务那一层，
//! `t-1`、`ed-1` 这些真在干活的东西**一个节点都没有**。
//!
//! 所以这里把它定成一条**有名字的、可校验的**载荷：
//!
//! ```json
//! {"kind":"instance","action":"open","instance":"t-1",
//!  "state":"idle","path":"D:\\proj","purpose":"跑测试"}
//! ```
//!
//! `kind` 是区分器：故障事件用 `code`，提示词编译用 `kind:"prompt_compiled"`，
//! 实例事件用 `kind:"instance"`。一条流里有三种东西，靠这个字段分开。

use serde_json::{json, Value};

/// 事件种类名 —— 用它把实例事件和别的 `event` 分开
pub const KIND: &str = "instance";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// 这个实例被开出来了
    Open,
    /// 它没了
    Close,
    /// 还在，但状态变了（空闲↔在跑、持有↔释放……）
    State,
}

impl Action {
    pub fn as_str(self) -> &'static str {
        match self {
            Action::Open => "open",
            Action::Close => "close",
            Action::State => "state",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "open" => Some(Action::Open),
            "close" => Some(Action::Close),
            "state" => Some(Action::State),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct InstanceEvent {
    pub action: Action,
    /// **服务内部的**实例名（`t-1` / `ed-1`）。全局唯一性由"哪个服务"保证，
    /// 所以中控那边必须用 `<服务>/<这个名字>` 当键 ——
    /// 两个编辑器服务的第一个文档都会叫 `ed-1`。
    pub instance: String,
    pub state: String,
    pub path: Option<String>,
    pub purpose: Option<String>,
    /// **这个实例归谁**（`<智能体实例>@<会话 id>`，或者 `user@<通道>`）。
    ///
    /// 为什么这条要在**协议层**而不是各服务自留：它是**图上的事实** ——
    /// "谁开着什么"只有中控能看全（它收到所有实例事件），
    /// 所以"别动别人的实例"这条规则只能在它那儿**统一执行**一次，
    /// 而不是指望每个第三方服务都自己实现一遍。
    ///
    /// 这也是"实例 = 独属于某个智能体的前端"（§5.10 的办公软件那张表）
    /// 落地的位置：**前端是实例，后端是服务**，中控管着"哪个前端归谁"。
    pub owner: Option<String>,
    pub detail: Value,
}

impl InstanceEvent {
    pub fn open(instance: impl Into<String>, state: impl Into<String>) -> Self {
        Self {
            action: Action::Open,
            instance: instance.into(),
            state: state.into(),
            path: None,
            purpose: None,
            owner: None,
            detail: json!({}),
        }
    }
    pub fn state(instance: impl Into<String>, state: impl Into<String>) -> Self {
        Self {
            action: Action::State,
            instance: instance.into(),
            state: state.into(),
            path: None,
            purpose: None,
            owner: None,
            detail: json!({}),
        }
    }
    pub fn close(instance: impl Into<String>) -> Self {
        Self {
            action: Action::Close,
            instance: instance.into(),
            state: "closed".into(),
            path: None,
            purpose: None,
            owner: None,
            detail: json!({}),
        }
    }

    pub fn with_path(mut self, p: impl Into<String>) -> Self {
        self.path = Some(p.into());
        self
    }
    pub fn with_purpose(mut self, p: impl Into<String>) -> Self {
        self.purpose = Some(p.into());
        self
    }
    /// 记下它归谁（`agent:dada@s-1`）。
    pub fn owned_by(mut self, who: impl Into<String>) -> Self {
        self.owner = Some(who.into());
        self
    }
    pub fn with_detail(mut self, d: Value) -> Self {
        self.detail = d;
        self
    }

    pub fn to_payload(&self) -> Value {
        let mut p = json!({
            "kind": KIND,
            "action": self.action.as_str(),
            "instance": self.instance,
            "state": self.state,
        });
        if let Some(path) = &self.path {
            p["path"] = json!(path);
        }
        if let Some(purpose) = &self.purpose {
            p["purpose"] = json!(purpose);
        }
        if let Some(owner) = &self.owner {
            p["owner"] = json!(owner);
        }
        if self.detail != json!({}) {
            p["detail"] = self.detail.clone();
        }
        p
    }

    /// 认得出就解析，认不出返回 `None`（那是别人的事件，不该误吃）。
    pub fn parse(payload: &Value) -> Option<Self> {
        if payload.get("kind").and_then(Value::as_str) != Some(KIND) {
            return None;
        }
        let action = Action::parse(payload.get("action").and_then(Value::as_str)?)?;
        let instance = payload.get("instance").and_then(Value::as_str)?.to_string();
        let state = payload
            .get("state")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        Some(Self {
            action,
            instance,
            state,
            path: payload.get("path").and_then(Value::as_str).map(str::to_string),
            purpose: payload
                .get("purpose")
                .and_then(Value::as_str)
                .map(str::to_string),
            owner: payload.get("owner").and_then(Value::as_str).map(str::to_string),
            detail: payload.get("detail").cloned().unwrap_or(json!({})),
        })
    }

    /// **`<谁>@<会话>` 里那个"谁"** —— 归属检查要比的就是它。
    pub fn owner_agent(&self) -> Option<&str> {
        self.owner
            .as_deref()
            .and_then(|o| o.split_once('@').map(|(who, _)| who))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_round_trips() {
        let e = InstanceEvent::open("t-1", "idle")
            .with_path("D:/proj")
            .with_purpose("跑测试")
            .owned_by("agent:dada@s-1");
        let back = InstanceEvent::parse(&e.to_payload()).expect("要能解析回来");
        assert_eq!(back, e);
        assert_eq!(back.owner_agent(), Some("agent:dada"));
    }

    /// 没有 owner 的事件也要能解析（旧服务 / 不需要归属的域）。
    #[test]
    fn owner_is_optional() {
        let e = InstanceEvent::state("t-1", "running");
        assert!(InstanceEvent::parse(&e.to_payload()).unwrap().owner.is_none());
        assert_eq!(e.owner_agent(), None);
    }

    #[test]
    fn close_carries_the_closed_state() {
        let e = InstanceEvent::close("ed-1");
        assert_eq!(e.state, "closed");
        assert_eq!(InstanceEvent::parse(&e.to_payload()).unwrap().action, Action::Close);
    }

    /// 一条流里有三种事件（故障 / 提示词编译 / 实例）——
    /// 别把别人的吃掉。
    #[test]
    fn other_events_are_not_mistaken_for_instance_events() {
        assert!(InstanceEvent::parse(&json!({"code": "instance_gone"})).is_none());
        assert!(InstanceEvent::parse(&json!({"kind": "prompt_compiled"})).is_none());
        assert!(InstanceEvent::parse(&json!({"kind": "instance"})).is_none(), "缺 action");
        assert!(
            InstanceEvent::parse(&json!({"kind": "instance", "action": "wobble", "instance": "x"}))
                .is_none(),
            "action 是闭合的"
        );
    }

    #[test]
    fn detail_is_omitted_when_empty() {
        let p = InstanceEvent::open("t-1", "idle").to_payload();
        assert!(p.get("detail").is_none(), "空 detail 不该占地方");
    }
}
