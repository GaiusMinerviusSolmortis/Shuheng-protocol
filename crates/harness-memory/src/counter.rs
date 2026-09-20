//! token 估算。
//!
//! **这是个估算器，不是真分词器** —— 我们算不准别家 provider 的分词。
//! 所以设计上分成两半：
//!
//! - **估算**用来做决定（要不要压、压到多小）—— 它只要稳定、单调、便宜；
//! - **真值**来自 API 回来的用量（`prompt_tokens`），用它给窗口**校准基线**。
//!
//! 这正是 codex 那个 `prefill_input_tokens` 是"绝对基线"而不是"估算值"的原因：
//! 基线必须是真值，否则误差会一轮一轮滚起来。

use crate::item::Item;

pub trait TokenCounter {
    fn count_text(&self, text: &str) -> u64;

    fn count(&self, items: &[Item]) -> u64 {
        items.iter().map(|i| self.count_text(&i.text)).sum()
    }
}

/// 混合中英的启发式：ASCII 约 4 字符 1 token，CJK 约 1 字符 0.7 token。
///
/// 明说是**启发式**：中文/代码/JSON 三种文本的密度差得很远，
/// 任何单一系数都会偏。偏了不要紧 —— 决定用的是它，校准用的是 API 的真值。
#[derive(Debug, Clone, Copy, Default)]
pub struct HeuristicCounter {
    /// 给测试用的缩放（默认 1.0 = 不缩放）
    pub scale: f64,
}

impl HeuristicCounter {
    pub fn new() -> Self {
        Self { scale: 1.0 }
    }
}

impl TokenCounter for HeuristicCounter {
    fn count_text(&self, text: &str) -> u64 {
        let mut ascii = 0u64;
        let mut wide = 0u64;
        for ch in text.chars() {
            if (ch as u32) < 128 {
                ascii += 1;
            } else {
                wide += 1;
            }
        }
        let raw = ascii as f64 / 4.0 + wide as f64 * 0.7;
        (raw * self.scale).ceil() as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_is_about_a_quarter() {
        let c = HeuristicCounter::new();
        assert_eq!(c.count_text("abcd"), 1);
        assert_eq!(c.count_text("abcdefgh"), 2);
    }

    #[test]
    fn cjk_costs_more_per_char() {
        let c = HeuristicCounter::new();
        // 10 个汉字 ≈ 7 token，同样长度的 ASCII 只要 2~3
        assert_eq!(c.count_text("汉汉汉汉汉汉汉汉汉汉"), 7);
        assert!(c.count_text("汉字") > c.count_text("ab"));
    }

    #[test]
    fn empty_is_zero() {
        assert_eq!(HeuristicCounter::new().count_text(""), 0);
    }
}
