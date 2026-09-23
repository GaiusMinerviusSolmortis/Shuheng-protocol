//! **方向**（规范 §2 那张表的第二列）的运行时检查。
//!
//! §2 的表格除了"语义"，还写着每种消息的方向。第一版实现只检查了
//! `from` 和管道自洽，**一个方向都没检查** —— 于是一个 `presence` 被
//! 当成了请求（真事，见决策册 #99）。
//!
//! 这里是那条规则的一般形式：**七种消息，五条方向约束**（§2.4 那张表就是五条）。
//! ⚠️ 这行原来写的是"四类" —— 表在加第 5 条（`cancel` 不许发给中控）时，
//! 注释没跟着走。**手写的数字就是这么漂的**（同 README 那几处：能数出来的别写死；
//! 写死了，就得有人查 —— 而这一处没人查）。

use crate::envelope::{Envelope, MessageType};

/// 一条方向违规。**事实 + 影响 + 出口**（§5.5），因为它是给模型/人看的。
#[derive(Debug, Clone, PartialEq)]
pub struct DirectionViolation {
    /// 违反的是哪条规则（一句话）
    pub rule: &'static str,
    pub why: String,
    pub remedy: &'static str,
}

/// 判方向需要的上下文。
pub struct DirectionContext<'a> {
    /// 中控自己的实例 id
    pub ctrl: &'a str,
    /// **中控发起的、还没收到终态的那些 corr**。
    ///
    /// 阶段 1 中控不起调用，所以它是空的 —— 空集合的含义就是
    /// "任何回给中控的 `chunk` / `result` 都是孤儿"。阶段 2/3 中控
    /// 开始自己调工具时，把发出去的 corr 填进来即可。
    pub ctrl_awaiting: &'a [String],
}

impl DirectionContext<'_> {
    fn awaiting(&self, corr: Option<&str>) -> bool {
        match corr {
            Some(c) => self.ctrl_awaiting.iter().any(|x| x == c),
            None => false,
        }
    }
}

/// 检查这一条消息的方向合不合规范 §2。
pub fn check(env: &Envelope, ctx: &DirectionContext) -> Result<(), DirectionViolation> {
    let to_ctrl = env.to == ctx.ctrl;

    // 规则一：`inject` 的发送方**只能是中控**。
    // 它是唯一的"命令模型改上下文"的手段，不能谁都发。
    if env.kind == MessageType::Inject && env.from != ctx.ctrl {
        return Err(DirectionViolation {
            rule: "只有中控能发 inject",
            why: format!("`{}` 发了 inject —— 而 inject 是「把内容放进你的上下文」，\
                          只有中控有权决定这件事", env.from),
            remedy: "要影响模型，走 event（「这儿有个事实，你记一笔」），\
                     由中控决定要不要转成 inject（规范 §2.1）",
        });
    }

    // 规则二：`inject` 的收件方**只能是智能体**。
    if env.kind == MessageType::Inject && to_ctrl {
        return Err(DirectionViolation {
            rule: "inject 的方向是中控 → 智能体",
            why: format!(
                "`{}` 想给中控注入上下文 —— 中控不是那个「有上下文」的对象",
                env.from
            ),
            remedy: "敕令要发给智能体实例；中控自己是路由，不是收件人",
        });
    }

    // 规则三：`presence` **只发给中控**。
    if env.kind == MessageType::Presence && !to_ctrl {
        return Err(DirectionViolation {
            rule: "presence 只发给中控",
            why: format!(
                "`{}` 把上线/健康汇报发给了 `{}`",
                env.from, env.to
            ),
            remedy: "presence 是中控用来维护注册表的；要通知别人请用 event",
        });
    }

    // 规则四：`chunk` / `result` 是**回给调用方**的。
    // 回给中控但中控没发过这个 corr ⇒ 孤儿 —— 它是"服务状态错乱"最灵敏的信号。
    if matches!(env.kind, MessageType::Chunk | MessageType::Result) && to_ctrl {
        match env.corr.as_deref() {
            None => {
                return Err(DirectionViolation {
                    rule: "chunk / result 必须带 corr",
                    why: format!("`{}` 发来的 {} 没有 corr，对不上是哪次调用", env.from, kind_name(env.kind)),
                    remedy: "结果要带发起方给的那个 corr（规范 §3）",
                });
            }
            Some(c) if !ctx.awaiting(Some(c)) => {
                return Err(DirectionViolation {
                    rule: "回给中控的 chunk / result 必须是中控要的",
                    why: format!(
                        "`{}` 回了 corr=`{c}`，但中控没发起过这个调用 —— 孤儿结果",
                        env.from
                    ),
                    remedy: "服务只能回自己收到的请求；对不上就说明它的状态错了，\
                             去查它的 stderr",
                });
            }
            Some(_) => {}
        }
    }

    // 规则五：`cancel` 是**调用方发出**的，中控是发起方不是收件方。
    if env.kind == MessageType::Cancel && to_ctrl {
        return Err(DirectionViolation {
            rule: "cancel 的方向是调用方 → 被调方",
            why: format!("`{}` 想取消一件中控没有在做的事", env.from),
            remedy: "要停某个工具实例，把 cancel 发给那个实例；要中控收摊就说 shutdown",
        });
    }

    Ok(())
}

fn kind_name(kind: MessageType) -> &'static str {
    match kind {
        MessageType::Request => "request",
        MessageType::Chunk => "chunk",
        MessageType::Result => "result",
        MessageType::Cancel => "cancel",
        MessageType::Event => "event",
        MessageType::Inject => "inject",
        MessageType::Presence => "presence",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::envelope::Authority;
    use serde_json::json;

    fn ctx() -> DirectionContext<'static> {
        DirectionContext {
            ctrl: "ctrl",
            ctrl_awaiting: &[],
        }
    }

    fn env(kind: MessageType, from: &str, to: &str, corr: Option<&str>) -> Envelope {
        Envelope {
            id: "m-1".into(),
            ts: 0,
            kind,
            from: from.into(),
            to: to.into(),
            corr: corr.map(str::to_string),
            sid: "s-1".into(),
            authority: Authority::Observe,
            payload: json!({}),
        }
    }

    #[test]
    fn inject_must_come_from_ctrl() {
        let e = env(MessageType::Inject, "echo:1", "agent:a", None);
        let v = check(&e, &ctx()).unwrap_err();
        assert_eq!(v.rule, "只有中控能发 inject");
    }

    #[test]
    fn inject_must_not_target_ctrl() {
        let e = env(MessageType::Inject, "ctrl", "ctrl", None);
        let v = check(&e, &ctx()).unwrap_err();
        assert_eq!(v.rule, "inject 的方向是中控 → 智能体");
    }

    #[test]
    fn presence_only_goes_to_ctrl() {
        assert!(check(&env(MessageType::Presence, "echo:1", "ctrl", None), &ctx()).is_ok());
        let v = check(&env(MessageType::Presence, "echo:1", "console", None), &ctx()).unwrap_err();
        assert_eq!(v.rule, "presence 只发给中控");
    }

    #[test]
    fn orphan_result_to_ctrl_is_refused() {
        let v = check(
            &env(MessageType::Result, "echo:1", "ctrl", Some("c-9")),
            &ctx(),
        )
        .unwrap_err();
        assert_eq!(v.rule, "回给中控的 chunk / result 必须是中控要的");
    }

    #[test]
    fn awaited_result_to_ctrl_is_fine() {
        let awaiting = vec!["c-9".to_string()];
        let ctx = DirectionContext {
            ctrl: "ctrl",
            ctrl_awaiting: &awaiting,
        };
        assert!(check(
            &env(MessageType::Result, "echo:1", "ctrl", Some("c-9")),
            &ctx
        )
        .is_ok());
    }

    #[test]
    fn cancel_to_ctrl_is_refused() {
        let v = check(&env(MessageType::Cancel, "console", "ctrl", None), &ctx()).unwrap_err();
        assert_eq!(v.rule, "cancel 的方向是调用方 → 被调方");
    }

    /// 正常流量一条都不该被误伤 —— 这条防的是"检查太严把活干不了"。
    #[test]
    fn normal_traffic_passes() {
        let cases = [
            env(MessageType::Request, "console", "ctrl", Some("c-1")),
            env(MessageType::Request, "console", "echo:1", Some("c-2")),
            env(MessageType::Result, "echo:1", "console", Some("c-2")),
            env(MessageType::Chunk, "echo:1", "console", Some("c-2")),
            env(MessageType::Event, "echo:1", "ctrl", None),
            env(MessageType::Presence, "echo:1", "ctrl", None),
            env(MessageType::Cancel, "console", "echo:1", Some("c-2")),
            env(MessageType::Inject, "ctrl", "agent:dada", None),
        ];
        for e in cases {
            assert!(check(&e, &ctx()).is_ok(), "被误伤：{:?} → {:?}", e.kind, e.to);
        }
    }
}
