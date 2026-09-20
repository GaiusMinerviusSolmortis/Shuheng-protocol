//! 模型适配层 —— **可插拔**，而且第一版故意是 mock（README 阶段 4a）。
//!
//! 为什么先 mock：它把"**系统的 bug**"和"**模型的 bug**"分开。
//! 一上来接真实 API，出了问题你分不清是协议错了、状态错了、还是模型不听话。
//!
//! mock 的形态**不是"返回一句话"，是脚本化**：给一个响应序列，按顺序吐 ——
//! 因为要测 agent 循环，它必须能产出普通文本、工具调用，以及"坏的那部分"。

use harness_memory::{Item, TokenCounter};
use serde::Deserialize;

/// 模型的一次工具调用。
///
/// v1 里模型直接按**实例 id** 寻址（`to`）。真正的做法是模型只说工具名，
/// 由绑定（§2.3）去路由 —— 那要等绑定实现，这里先把形状留出来。
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct ToolCall {
    /// provider 给的调用 id（真模型必须原样带回去）；mock 不给就由循环生成
    #[serde(default)]
    pub id: Option<String>,
    pub to: String,
    pub tool: String,
    #[serde(default)]
    pub args: serde_json::Value,
    /// 脚本用它演"模型给的参数**不是合法 JSON**"。
    ///
    /// 为什么要有这一格：真 provider 在那一刻给的**就是一个字符串**
    /// （`function.arguments`），我们要能演"那个字符串解不出来"这件事 ——
    /// 而不是演"模型给了一个奇怪的参数对象"（那是两回事）。
    #[serde(default)]
    pub args_raw: Option<String>,
    /// **参数没解出来时的原话。**
    ///
    /// ⚠️ 有它 ⇒ 这条调用**没有发生**：不许把它当成模型的意思发出去。
    /// 这一格的存在本身就是那条纪律的落点（2026-09-20）：
    /// 在它之前，解不出来的参数会被编成 `{"_raw":…,"_parse_error":true}`
    /// **发给工具** —— 那等于替模型编了它没说的话。
    #[serde(default)]
    pub parse_error: Option<String>,
    /// **模型原话里的那个函数名**（只有"名字对不上"时才有）。
    ///
    /// 为什么要留着它：回放那条不成立的调用时，得**逐字**照它说的回放 ——
    /// `fn_name(to, tool)` 重建不出一个查不到的名字（真事：它叫的是 `__edit`）。
    #[serde(default)]
    pub raw_name: Option<String>,
}

impl ToolCall {
    /// 从"provider 给的那个字符串"建一条调用：解得开就是参数，解不开就是 `parse_error`。
    ///
    /// 真 provider 和 mock **走的是同一条路**（这条函数），所以 mock 演出来的
    /// 畸形输出，和真的畸形输出是**同一种形状**。
    pub fn from_raw_args(id: Option<String>, to: String, tool: String, raw: &str) -> Self {
        match serde_json::from_str::<serde_json::Value>(raw) {
            Ok(v) if v.is_object() => Self {
                id,
                to,
                tool,
                args: v,
                args_raw: None,
                parse_error: None,
                raw_name: None,
            },
            Ok(v) => Self {
                // 解出来了，但**不是个对象**：`{"tool":"run","args":42}` 这种也进不来
                id,
                to,
                tool,
                args: serde_json::Value::Null,
                args_raw: None,
                raw_name: None,
                parse_error: Some(format!(
                    "参数解得开，但它是个 {}，不是对象：{v}",
                    match v {
                        serde_json::Value::Array(_) => "数组",
                        serde_json::Value::Number(_) => "数字",
                        serde_json::Value::String(_) => "字符串",
                        serde_json::Value::Bool(_) => "布尔值",
                        serde_json::Value::Null => "null",
                        _ => "别的",
                    }
                )),
            },
            Err(e) => Self {
                id,
                to,
                tool,
                args: serde_json::Value::Null,
                args_raw: None,
                raw_name: None,
                parse_error: Some(format!("不是合法 JSON：{e}")),
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ModelReply {
    /// 没有工具调用时，这就是它的回答
    pub text: Option<String>,
    pub calls: Vec<ToolCall>,
    /// provider 报回来的**真实**输入 token（mock 也会报，好让校准那条路被走到）
    pub usage_input_tokens: u64,
    /// 模型这条路本身出问题了（连不上、回包不是 JSON……）。
    /// **它不是"模型说了这句话"** —— 不能当成回答交给用户。
    pub error: Option<String>,
}

impl ModelReply {
    pub fn text(t: impl Into<String>, usage: u64) -> Self {
        Self {
            text: Some(t.into()),
            calls: Vec::new(),
            usage_input_tokens: usage,
            error: None,
        }
    }
    pub fn calls(calls: Vec<ToolCall>, usage: u64) -> Self {
        Self {
            text: None,
            calls,
            usage_input_tokens: usage,
            error: None,
        }
    }
    /// 模型这条路断了 —— **诚实地把它和"模型的回答"分开**
    pub fn failed(why: impl Into<String>) -> Self {
        Self {
            text: None,
            calls: Vec::new(),
            usage_input_tokens: 0,
            error: Some(why.into()),
        }
    }
}

pub struct ModelRequest {
    /// 编译一次、字节稳定的那段前缀
    pub system: String,
    /// 历史（含每次工具结果与敕令）
    pub items: Vec<Item>,
    /// **这次请求允许它用的工具**（绑定的雏形）—— 真模型要靠它生成 function 定义
    pub tools: Vec<crate::prompt::ToolCard>,
}

pub trait Model {
    fn name(&self) -> &'static str;
    fn reply(&mut self, req: &ModelRequest) -> ModelReply;
}

/// 脚本里的一个步骤。
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(tag = "step", rename_all = "snake_case")]
pub enum Step {
    /// 产出一次（或几次）工具调用
    Tool {
        calls: Vec<ToolCall>,
        #[serde(default)]
        usage_input_tokens: Option<u64>,
    },
    /// 产出最终文本，回合结束
    Text { text: String, #[serde(default)] usage_input_tokens: Option<u64> },
    /// **回包整体读不懂**：直接给一段原始文本（半截输出、根本不是 JSON……）。
    ///
    /// ⚠️ 它对应的是**"模型这条路断了"**，不是"模型说了这句话"——
    /// 所以它走 `ModelReply::failed`，**不许**被当成回答交给用户
    /// （那正是"把故障说成结论"，README 里点过名的那个坑）。
    ///
    /// 想演"**工具调用**的参数是畸形 JSON"，用 [`Step::Tool`] 的 `args_raw` ——
    /// 那才是它在真实 provider 那儿的形状。
    Raw { text: String, #[serde(default)] usage_input_tokens: Option<u64> },
}

/// 按脚本吐的 mock 模型。
pub struct MockModel {
    script: Vec<Step>,
    at: usize,
}

impl MockModel {
    pub fn new(script: Vec<Step>) -> Self {
        Self { script, at: 0 }
    }

    pub fn remaining(&self) -> usize {
        self.script.len().saturating_sub(self.at)
    }
}

impl Model for MockModel {
    fn name(&self) -> &'static str {
        "mock-script"
    }

    fn reply(&mut self, req: &ModelRequest) -> ModelReply {
        // mock 也会报一个"真实用量"：它用估算器算一遍请求的大小。
        // 真实 provider 在这里回的是 API 给的 prompt_tokens —— 走的是同一条校准路。
        let est = harness_memory::HeuristicCounter::new().count_text(&req.system)
            + harness_memory::HeuristicCounter::new().count(&req.items);
        let Some(step) = self.script.get(self.at).cloned() else {
            // **脚本用完不能假装成功** —— 那是测试工具的失职，要说出来
            return ModelReply::text(
                "（mock 脚本用完了 —— 这是测试脚本没写够，不是模型的决定）",
                est,
            );
        };
        self.at += 1;
        match step {
            Step::Tool {
                calls,
                usage_input_tokens,
            } => {
                // 脚本里写 `args_raw` 的，在这儿走**和真 provider 同一条路**
                let calls = calls
                    .into_iter()
                    .map(|c| match c.args_raw.clone() {
                        Some(raw) => {
                            ToolCall::from_raw_args(c.id.clone(), c.to.clone(), c.tool.clone(), &raw)
                        }
                        None => c,
                    })
                    .collect();
                ModelReply::calls(calls, usage_input_tokens.unwrap_or(est))
            }
            Step::Text {
                text,
                usage_input_tokens,
            } => ModelReply::text(text, usage_input_tokens.unwrap_or(est)),
            Step::Raw {
                text,
                usage_input_tokens,
            } => {
                let _ = usage_input_tokens;
                // 路断了 ⇒ **失败**，不是回答
                ModelReply::failed(format!("provider 的回包读不懂：{text}"))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req() -> ModelRequest {
        ModelRequest {
            system: "你是助手".into(),
            items: vec![Item::user("干点活")],
            tools: Vec::new(),
        }
    }

    /// **解得开就是参数**：合法 JSON 对象照常进来。
    #[test]
    fn raw_args_that_parse_become_args() {
        let c = ToolCall::from_raw_args(None, "term:1".into(), "run".into(), r#"{"cmd":"ls"}"#);
        assert_eq!(c.args["cmd"], serde_json::json!("ls"));
        assert!(c.parse_error.is_none());
    }

    /// **解不开就是 `parse_error`** —— 而且**不许**编一个参数对象出来。
    ///
    /// 这条是从反面改过来的：老写法在这里塞 `{"_raw":…,"_parse_error":true}`，
    /// 然后那条调用**照样发给工具**（等于替模型编了它没说的话）。
    #[test]
    fn raw_args_that_do_not_parse_never_become_fake_args() {
        let c = ToolCall::from_raw_args(None, "term:1".into(), "open".into(), "{path: \".\"");
        assert!(c.parse_error.is_some(), "解不开就该记下来");
        assert!(c.args.is_null(), "**不许**替它编一个参数对象：{}", c.args);
    }

    /// 解得开、但**不是一个对象**（数组 / 数字 / 字符串）—— 同样不许当参数用。
    #[test]
    fn args_that_parse_to_a_non_object_are_refused_too() {
        for raw in ["[1,2]", "42", "\"ls\""] {
            let c = ToolCall::from_raw_args(None, "term:1".into(), "run".into(), raw);
            assert!(c.parse_error.is_some(), "{raw} 不该被当成参数");
            assert!(c.args.is_null(), "{raw}");
        }
    }

    #[test]
    fn script_is_consumed_in_order() {
        let mut m = MockModel::new(vec![
            Step::Tool {
                calls: vec![ToolCall {
                    id: None,
                    to: "term:1".into(),
                    tool: "run".into(),
                    args: serde_json::json!({"cmd": "ls"}),
                    args_raw: None,
                    parse_error: None,
                    raw_name: None,
                }],
                usage_input_tokens: None,
            },
            Step::Text {
                text: "做完了".into(),
                usage_input_tokens: None,
            },
        ]);
        let r1 = m.reply(&req());
        assert_eq!(r1.calls.len(), 1);
        assert!(r1.text.is_none());
        let r2 = m.reply(&req());
        assert_eq!(r2.text.as_deref(), Some("做完了"));
        assert_eq!(m.remaining(), 0);
    }

    #[test]
    fn running_out_of_script_says_so_instead_of_pretending() {
        let mut m = MockModel::new(vec![]);
        let r = m.reply(&req());
        assert!(r.text.unwrap().contains("脚本用完"));
    }

    #[test]
    fn usage_is_the_estimate_when_the_script_does_not_say() {
        let mut m = MockModel::new(vec![Step::Text {
            text: "好".into(),
            usage_input_tokens: None,
        }]);
        let r = m.reply(&req());
        assert!(r.usage_input_tokens > 0);
    }

    #[test]
    fn script_can_override_usage_for_calibration_tests() {
        let mut m = MockModel::new(vec![Step::Text {
            text: "好".into(),
            usage_input_tokens: Some(12345),
        }]);
        assert_eq!(m.reply(&req()).usage_input_tokens, 12345);
    }
}
