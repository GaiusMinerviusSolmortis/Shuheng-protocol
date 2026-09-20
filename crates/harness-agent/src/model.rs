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
    /// **坏的那部分**：直接给一段原始文本（畸形 JSON、半截输出……）
    /// —— 4a 要测的"报错即手册的前半"就靠它
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
            } => ModelReply::calls(calls, usage_input_tokens.unwrap_or(est)),
            Step::Text {
                text,
                usage_input_tokens,
            } => ModelReply::text(text, usage_input_tokens.unwrap_or(est)),
            Step::Raw {
                text,
                usage_input_tokens,
            } => ModelReply::text(text, usage_input_tokens.unwrap_or(est)),
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

    #[test]
    fn script_is_consumed_in_order() {
        let mut m = MockModel::new(vec![
            Step::Tool {
                calls: vec![ToolCall {
                    id: None,
                    to: "term:1".into(),
                    tool: "run".into(),
                    args: serde_json::json!({"cmd": "ls"}),
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
