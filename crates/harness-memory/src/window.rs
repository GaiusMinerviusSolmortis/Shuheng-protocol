//! **窗口**：每次压缩开一个新的，token 预算跟着重置（抄 codex 的
//! `auto_compact_window.rs`）。
//!
//! 最关键的是最后那个字段：**判断"要不要再压"看的是相对基线的增长，
//! 不是累计总量** —— 否则新历史一开就背着旧窗口的账，永远显示快满了。
//!
//! 和我们自己的估算器配合起来是这样分工的：
//! **估算值用来判断，基线用 API 回来的真值校准。**

use crate::counter::TokenCounter;
use crate::history::History;

#[derive(Debug, Clone, PartialEq)]
pub struct Window {
    pub number: u32,
    pub id: String,
    pub first_id: String,
    pub previous_id: Option<String>,
    /// 当前窗口**第一次请求时的绝对输入 token**（真值优先，估算兜底）
    pub prefill_input_tokens: Option<u64>,
    /// 模型窗口有多大
    pub model_window: u64,
}

impl Window {
    pub fn first(session: &str, model_window: u64) -> Self {
        let id = format!("w1-{session}");
        Self {
            number: 1,
            first_id: id.clone(),
            id,
            previous_id: None,
            prefill_input_tokens: None,
            model_window,
        }
    }

    /// 压完开新窗口：编号 +1，**留下上一个的 id 以便回溯**，基线清空待重设。
    pub fn next_after(&self, session: &str) -> Self {
        Self {
            number: self.number + 1,
            id: format!("w{}-{session}", self.number + 1),
            first_id: self.first_id.clone(),
            previous_id: Some(self.id.clone()),
            prefill_input_tokens: None,
            model_window: self.model_window,
        }
    }

    /// 本窗口的第一次请求：把基线记下来（估算兜底；之后可用真值覆盖）。
    pub fn open_with_estimate(&mut self, history: &History, counter: &dyn TokenCounter) {
        if self.prefill_input_tokens.is_none() {
            self.prefill_input_tokens = Some(history.tokens(counter));
        }
    }

    /// **校准**：API 回来了真值，用它覆盖估算。
    ///
    /// 为什么基线要用真值：估算的误差会一轮一轮滚起来，
    /// 基线是用来看"我离上限还有多远"的，它偏了，所有判断都偏。
    pub fn calibrate(&mut self, real_input_tokens: u64) {
        self.prefill_input_tokens = Some(real_input_tokens);
    }

    /// 相对基线的增长 —— **这才是"要不要压"的依据**。
    pub fn growth(&self, used_tokens: u64) -> u64 {
        used_tokens.saturating_sub(self.prefill_input_tokens.unwrap_or(0))
    }

    pub fn used_ratio(&self, used_tokens: u64) -> f64 {
        if self.model_window == 0 {
            return 0.0;
        }
        used_tokens as f64 / self.model_window as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::counter::HeuristicCounter;
    use crate::item::{Item, ItemKind};

    #[test]
    fn first_window_has_no_previous() {
        let w = Window::first("s-1", 1000);
        assert_eq!(w.number, 1);
        assert_eq!(w.id, "w1-s-1");
        assert_eq!(w.first_id, "w1-s-1");
        assert!(w.previous_id.is_none());
    }

    #[test]
    fn next_window_keeps_the_chain() {
        let w = Window::first("s-1", 1000);
        let n = w.next_after("s-1");
        assert_eq!(n.number, 2);
        assert_eq!(n.id, "w2-s-1");
        assert_eq!(n.previous_id.as_deref(), Some("w1-s-1"));
        assert_eq!(n.first_id, "w1-s-1", "第一个窗口要留痕，便于回溯");
        assert!(n.prefill_input_tokens.is_none(), "新窗口的基线要重设");
    }

    #[test]
    fn growth_is_measured_against_the_baseline() {
        let mut w = Window::first("s-1", 1000);
        w.calibrate(300);
        assert_eq!(w.growth(300), 0, "基线自己不算增长");
        assert_eq!(w.growth(450), 150);
    }

    #[test]
    fn estimate_fills_the_baseline_only_if_empty() {
        let mut h = crate::history::History::new();
        h.push(Item::new(ItemKind::System, "abcd"));
        let mut w = Window::first("s-1", 1000);
        w.open_with_estimate(&h, &HeuristicCounter::new());
        assert_eq!(w.prefill_input_tokens, Some(1));
        // 真值一来就覆盖它
        w.calibrate(42);
        assert_eq!(w.prefill_input_tokens, Some(42));
        // 再估也不该把它改回去
        w.open_with_estimate(&h, &HeuristicCounter::new());
        assert_eq!(w.prefill_input_tokens, Some(42));
    }

    #[test]
    fn ratio_is_relative_to_the_model_window() {
        let w = Window::first("s-1", 2000);
        assert!((w.used_ratio(1600) - 0.8).abs() < 1e-9);
    }
}
