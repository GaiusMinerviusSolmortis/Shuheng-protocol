//! **凭证**：中控签发，**服务自己校验**。
//!
//! §7 对能力制的描述里有一句很关键：
//!
//! > **工具只校验手里的凭证够不够干这件事，不关心调用者是谁。**
//!
//! 这句话不只是安全模型，它同时是一条**责任分配**：能力的检查**不该堆在中控**。
//! 中控只知道"我把哪几个能力授给了哪个实例"，它**不知道**"调 `run` 需要什么" ——
//! 那是工具自己接口的一部分（写在自己的 manifest 里）。
//!
//! 所以链路是：
//!
//! ```text
//!   中控：查绑定 → 把"你被授予了什么"放进请求的 payload.granted
//!   服务：拿"这次要用的工具需要什么"跟它比 → 不够就拒，并说清差哪一条
//! ```
//!
//! 这样加一个工具、改一条能力要求，**都不用动中控** —— 责任在谁那一层，就在谁那层。

use serde_json::Value;

/// 一次调用里，**中控签发的那份凭证**（外加调用方是谁）。
///
/// ⚠️ **2026-09-19 这里翻过一次默认值，理由是它和这套系统自己的另一条纪律相反。**
///
/// 原来 `Grant` 的缺省是"全权"（字段不在 = 调用方不是智能体 = 不检查）。
/// 而绑定的缺省是**拒绝**（不给就是空表）。同一个系统里两个相反的默认值，
/// 迟早会撞上 —— 它真的撞上了，两处：
///
/// 1. **一条被降级的通道能全权指挥工具。** `--channel 微信:agent` 转来的消息，
///    权威被降到 `agent`（"只能提意见"），可它的请求里没有 `granted` 字段 ——
///    于是服务把它当成全权的非智能体调用方，`proc.exec` 也照给。
///    权威分级只影响了信封里的那个字段，**没影响它能不能用工具**。
/// 2. 谁把一行 JSON 直接喂给服务的门槛，**反而比经过中控更低**。
///
/// 现在的规则：
///
/// | 请求里有什么 | 谁 | 能干什么 |
/// |---|---|---|
/// | `granted` + `caller_kind` | 中控给每个请求盖的章 | 按能力比对 |
/// | 什么都没有 | **没经过中控**（人肉直连 / 旧实现 / 别的客户端）| **默认拒绝**：只放行**零能力**的诊断动作（`help` / `status` / `ping`）|
///
/// 直连调试也没被堵死：真需要动手时，显式写 `"granted": ["*"]` 就行 ——
/// **"我确实有权限"要由人说出口，而不是靠一个字段恰好缺失来推断。**
#[derive(Debug, Clone, PartialEq)]
pub enum Grant {
    /// 请求里没有 `granted` —— **没经过网关**。风险全留在这里，所以默认拒绝。
    Unrouted,
    /// 中控盖过章的：调用方是谁 + 签发给它的能力。`["*"]` 表示全权。
    Issued {
        /// `agent` / `user` / `channel` / `ctrl` —— 中控按调用方身份盖的
        caller_kind: String,
        capabilities: Vec<String>,
    },
}

impl Grant {
    pub fn from_payload(payload: &Value) -> Self {
        match payload.get("granted").and_then(Value::as_array) {
            None => Grant::Unrouted,
            Some(a) => Grant::Issued {
                caller_kind: payload
                    .get("caller_kind")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
                    .to_string(),
                capabilities: a
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect(),
            },
        }
    }

    /// 够不够干这件事。
    ///
    /// **零能力的动作（`required` 为空）永远放行** —— 这条不是后门，是刻意的：
    /// 出问题的时候，得有人能问它一句 `status` / `help`，
    /// 哪怕那条路没经过中控（诊断通道不能因为鉴权坏了就一起坏掉）。
    pub fn allows(&self, required: &[&str]) -> bool {
        if required.is_empty() {
            return true;
        }
        match self {
            Grant::Unrouted => false,
            Grant::Issued { capabilities, .. } => {
                if capabilities.iter().any(|c| c == "*") {
                    return true;
                }
                required
                    .iter()
                    .all(|r| capabilities.iter().any(|c| c == r))
            }
        }
    }

    /// 差哪几条 —— 拒的时候要说清（§5.5 报错即手册：事实 + 影响 + 出口）。
    pub fn missing(&self, required: &[&str]) -> Vec<String> {
        match self {
            // 没经过网关：它手里**一条都没有**（所以缺的就是要求的全部）
            Grant::Unrouted => required.iter().map(|r| r.to_string()).collect(),
            Grant::Issued { capabilities, .. } => {
                if capabilities.iter().any(|c| c == "*") {
                    return Vec::new();
                }
                required
                    .iter()
                    .filter(|r| !capabilities.iter().any(|c| c == *r))
                    .map(|r| r.to_string())
                    .collect()
            }
        }
    }

    pub fn caller_kind(&self) -> &str {
        match self {
            Grant::Unrouted => "unrouted",
            Grant::Issued { caller_kind, .. } => caller_kind,
        }
    }

    /// 调用方是不是**另一个智能体**？
    ///
    /// 这条问的是"它算不算一个智能体"，不是"它有没有权限" ——
    /// 用它来判"实例独属"（§5.10 的办公软件那张表）：
    /// **智能体之间互不越界，用户（`user`）不受限** ——
    /// §7.1 早就定过"用户永远能杀掉和浏览正在运行的终端"。
    pub fn is_agent(&self) -> bool {
        self.caller_kind() == "agent"
    }
}

/// 这个服务统一用它拒一次调用。
///
/// **形状故意和别处的拒绝一致**：`effect: refused` + `code: refused` +
/// "为什么" + "你被授予了什么" + "下一步"。
pub fn refuse_payload(tool: &str, missing: &[String], granted: &Grant) -> Value {
    let have = match granted {
        Grant::Unrouted => "（没经过中控，手里一条凭证都没有）".to_string(),
        Grant::Issued { capabilities, .. } if capabilities.is_empty() => {
            "（什么都没有）".to_string()
        }
        Grant::Issued { capabilities, .. } => capabilities.join(", "),
    };
    let remedy = match granted {
        // **这两种拒的补救不一样**，所以文案要分开：一个去要授权，一个先走网关。
        Grant::Unrouted => format!(
            "这条请求里没有中控签发的 `granted` —— **它没经过网关**（直连、或者写漏了）。\
             正常用法是走中控：它会按调用方身份把凭证盖在请求上。\
             要是你在人肉调试，显式带上 \"granted\": [\"*\"] 就行"
        ),
        _ => "能力是中控签发的（§2.3）：要么让它给你加上这几条，\
              要么换个不需要这些能力的做法。**别重试** —— \
              你不是偶尔被挡，是本来就没这个资格"
            .to_string(),
    };
    serde_json::json!({
        "effect": "refused",
        "code": "refused",
        "route": Value::Null,
        "why": format!("调 `{tool}` 需要的能力你没有：{}", missing.join(", ")),
        "granted": have,
        "caller_kind": granted.caller_kind(),
        "remedy": remedy,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn missing_field_means_unrouted_so_default_deny() {
        let g = Grant::from_payload(&json!({"tool": "run"}));
        assert_eq!(g, Grant::Unrouted);
        assert!(!g.allows(&["proc.exec"]), "没经过中控 ⇒ 需要能力的动作一律拒");
        assert!(g.allows(&[]), "但零能力的诊断动作要留着（出问题时得有人能问话）");
        assert_eq!(g.missing(&["proc.exec"]), vec!["proc.exec".to_string()]);
        assert_eq!(g.caller_kind(), "unrouted");
    }

    #[test]
    fn issued_list_is_checked() {
        let g = Grant::from_payload(&json!({"tool": "run", "granted": ["fs.read"]}));
        assert!(!g.allows(&["proc.exec"]));
        assert_eq!(g.missing(&["proc.exec"]), vec!["proc.exec".to_string()]);
        assert!(g.allows(&["fs.read"]));
    }

    #[test]
    fn star_is_full_rights() {
        let g = Grant::from_payload(&json!({"granted": ["*"]}));
        assert!(g.allows(&["proc.exec", "fs.write"]));
        assert!(g.missing(&["proc.exec"]).is_empty());
    }

    #[test]
    fn empty_list_denies_everything_that_needs_something() {
        let g = Grant::from_payload(&json!({"granted": []}));
        assert!(!g.allows(&["proc.exec"]));
        assert!(g.allows(&[]), "不需要能力的工具（如 status）照样能用");
    }

    /// **被降级的通道**：中控盖章说"它是 channel，能力空表" ——
    /// 于是它连 `proc.exec` 都拿不到，但 `status` 还能问。
    /// （这条补的是一个真洞：在改之前，它没有 `granted` 字段 ⇒ 全权。）
    #[test]
    fn a_downgraded_channel_gets_zero_capabilities_but_can_still_be_diagnosed() {
        let g = Grant::from_payload(&json!({
            "tool": "run", "caller_kind": "channel", "granted": []
        }));
        assert_eq!(g.caller_kind(), "channel");
        assert!(!g.allows(&["proc.exec"]));
        assert!(g.allows(&[]));
    }

    /// 拒绝必须是**给模型看的**三段式 —— 说清差哪条、有什么、下一步干嘛。
    #[test]
    fn refusal_says_what_is_missing_and_what_you_have() {
        let g = Grant::from_payload(&json!({"granted": ["fs.read"]}));
        let p = refuse_payload("run", &g.missing(&["proc.exec"]), &g);
        assert_eq!(p["effect"], "refused");
        assert_eq!(p["code"], "refused");
        assert!(p["why"].as_str().unwrap().contains("proc.exec"));
        assert!(p["granted"].as_str().unwrap().contains("fs.read"));
        assert!(p["remedy"].as_str().unwrap().contains("中控签发"));
        assert_eq!(p["caller_kind"], "unknown");
    }

    /// **"没经过网关"和"授权不够"是两种拒**，补救不一样 —— 文案必须分开。
    #[test]
    fn unrouted_and_unauthorized_are_refused_differently() {
        let g = Grant::from_payload(&json!({"tool": "run"}));
        let p = refuse_payload("run", &g.missing(&["proc.exec"]), &g);
        assert_eq!(p["caller_kind"], "unrouted");
        assert!(p["remedy"].as_str().unwrap().contains("没经过网关"));
        assert!(p["remedy"].as_str().unwrap().contains("granted"));
    }
}
