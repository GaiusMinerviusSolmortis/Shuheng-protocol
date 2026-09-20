//! 摘要器 —— **可插拔**，因为机制错和模型不听话要分得开（README 阶段 4a/4b）。
//!
//! 提示词模板照 codex 抄，就四行要求。而给下一个模型看的前缀是这段：
//!
//! > 另一个语言模型开始解决这个问题并留下了它的思路摘要。
//! > **你也能看到它用过的工具的状态。** 用它接着往下做，**别重复已经做过的事**。
//!
//! **它不假装记忆连续，它明说这是交接。** ——这是 `Handoff` 风格的全部要点。

#[derive(Debug, Clone, PartialEq)]
pub struct SummaryInput {
    /// 被压掉的那一段（已经渲染成文本）
    pub transcript: String,
    /// 上一次的摘要：**新摘要要把它折进去**，否则越压越薄
    pub previous_summary: Option<String>,
    /// 四行要求，来自 codex 的 `compact/prompt.md`
    pub instructions: String,
}

impl SummaryInput {
    pub const INSTRUCTIONS: &'static str = "\
请把这之前的工作总结成一份**交接说明**，只写这四块：\n\
1. 当前进度与关键决策（做过什么、定了什么）\n\
2. 重要的上下文与偏好（环境、约定、用户说过什么）\n\
3. 还剩什么要做（下一步要干什么）\n\
4. 关键数据与引用（路径、命令、结论）\n\
不要复述对话，不要写成\"我们讨论了什么\"那种流水账。";

    pub fn new(transcript: impl Into<String>) -> Self {
        Self {
            transcript: transcript.into(),
            previous_summary: None,
            instructions: Self::INSTRUCTIONS.to_string(),
        }
    }

    pub fn with_previous(mut self, prev: Option<String>) -> Self {
        self.previous_summary = prev;
        self
    }
}

pub trait Summarizer {
    /// 名字会进审计记录 —— "这次是哪条路压的"要能查
    fn name(&self) -> &'static str;
    fn summarize(&self, input: &SummaryInput) -> String;
}

/// **mock 摘要器**：确定性、可控大小。
///
/// 它存在的唯一理由是让"系统对不对"和"模型听不听话"分得开（README 阶段 4a）。
/// 它保留头尾各一段 —— 因为真实的摘要也是"开头说了干什么、结尾说了到哪了"。
#[derive(Debug, Clone)]
pub struct MockSummarizer {
    /// 最多产出多少字符（用来测"压完还是太大"这条路）
    pub max_chars: usize,
}

impl Default for MockSummarizer {
    fn default() -> Self {
        Self { max_chars: 200 }
    }
}

impl Summarizer for MockSummarizer {
    fn name(&self) -> &'static str {
        "mock"
    }

    fn summarize(&self, input: &SummaryInput) -> String {
        let head: String = input.transcript.chars().take(60).collect();
        let tail: String = input
            .transcript
            .chars()
            .rev()
            .take(40)
            .collect::<String>()
            .chars()
            .rev()
            .collect();
        let folded = match &input.previous_summary {
            Some(prev) => {
                let p: String = prev.chars().take(40).collect();
                format!("\n[折进上一次的摘要] {p}")
            }
            None => String::new(),
        };
        let body = format!(
            "【交接摘要/mock】开头：{head} … 结尾：{tail}{folded}"
        );
        body.chars().take(self.max_chars).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mock_is_deterministic_and_bounded() {
        let s = MockSummarizer { max_chars: 50 };
        let input = SummaryInput::new("A".repeat(500));
        let a = s.summarize(&input);
        let b = s.summarize(&input);
        assert_eq!(a, b, "mock 必须确定性");
        assert!(a.chars().count() <= 50);
    }

    /// 第二次压缩必须把上一次的摘要折进去，否则越压越薄（信息一路丢）。
    #[test]
    fn previous_summary_is_folded_in() {
        let s = MockSummarizer::default();
        let input = SummaryInput::new("这一轮的新内容")
            .with_previous(Some("上一轮的摘要：定了用方案 B".into()));
        let out = s.summarize(&input);
        assert!(out.contains("折进上一次的摘要"), "{out}");
        assert!(out.contains("方案 B"), "{out}");
    }

    #[test]
    fn instructions_are_the_four_blocks() {
        let i = SummaryInput::INSTRUCTIONS;
        for n in 1..=4 {
            assert!(i.contains(&format!("{n}.")), "缺第 {n} 块：{i}");
        }
    }
}
