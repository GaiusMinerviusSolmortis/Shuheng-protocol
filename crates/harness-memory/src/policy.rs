//! 压缩策略（03-上下文 §5.4.4 的草案）。
//!
//! 它天然属于**绑定** —— 和人格、模型、工具集并列。因为：
//! 它影响提示词的样子（摘要风格进上下文）；它和会话一样长
//! （**中途换就又碎一次缓存**）；不同 agent 需要不同
//! （编码 agent 要准确，长期助理要连续感）。
//!
//! 所以"交接式还是叙事式"不是二选一 —— **按 agent 配**。

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// 压掉的原文**丢掉**，只剩摘要。第一版就能做（只依赖"能写摘要"）
    Linear,
    /// 压掉的原文**搬进记忆库**，可按锚点展开。依赖整个记忆层（§16）
    Sediment,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SummaryStyle {
    /// 交接：准确优先，**不假装记忆连续**（codex 那条）
    Handoff,
    /// 叙事：第一人称故事，刻意营造"这就是我的记忆"（大眼那条）
    Narrative,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CompactionPolicy {
    pub mode: Mode,
    /// 到模型窗口的多少比例触发
    pub context_ratio: f64,
    /// 提示词组成变了顺便压（抄 codex 的 `CompHashChanged`）
    pub on_comp_hash_change: bool,
    pub on_binding_change: bool,
    /// **一次压到多小**：0.5 = 压到阈值的六成……
    /// 这就是 §5.4.4 的"反正都是炸缓存，不如把新东西放少点"
    pub aggressiveness: f64,
    /// 最近几轮保留原文
    pub keep_recent_turns: usize,
    pub summary_style: SummaryStyle,
    /// 压掉的是否沉淀进长期层（第一版恒为 false）
    pub memory_write: bool,
}

impl Default for CompactionPolicy {
    fn default() -> Self {
        Self {
            mode: Mode::Linear,
            context_ratio: 0.80,
            on_comp_hash_change: true,
            on_binding_change: false,
            aggressiveness: 0.5,
            keep_recent_turns: 3,
            summary_style: SummaryStyle::Handoff,
            memory_write: false,
        }
    }
}

impl CompactionPolicy {
    /// 压完之后想让它占模型窗口的多少。
    ///
    /// 触发线是 0.8、`aggressiveness` 是 0.5 ⇒ 目标是 0.8 × (1 − 0.5) = **0.4**。
    /// 一次压到 0.8 附近是浪费：**压得狠一点，后面每一轮都省**。
    pub fn target_ratio(&self) -> f64 {
        self.context_ratio * (1.0 - self.aggressiveness.clamp(0.0, 1.0))
    }

    pub fn target_tokens(&self, model_window: u64) -> u64 {
        // **四舍五入，不是截断。** 二进制浮点算出来的 0.8 × 0.1 × 1200
        // 是 95.99999999999999，截断就变成 95 —— 预算差一个 token 没意义，
        // 但"同一个配置算出两个数"会让判据变成玄学。
        (model_window as f64 * self.target_ratio()).round() as u64
    }

    pub fn trigger_tokens(&self, model_window: u64) -> u64 {
        (model_window as f64 * self.context_ratio).round() as u64
    }

    pub fn should_compact(&self, used_tokens: u64, model_window: u64) -> bool {
        used_tokens >= self.trigger_tokens(model_window)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trigger_is_at_the_configured_ratio() {
        let p = CompactionPolicy::default();
        assert_eq!(p.trigger_tokens(1000), 800);
        assert!(!p.should_compact(799, 1000));
        assert!(p.should_compact(800, 1000));
    }

    /// "反正都是炸缓存，不如把新东西放少点" —— 压完的目标要**明显低于**触发线。
    #[test]
    fn target_is_well_below_the_trigger() {
        let p = CompactionPolicy::default();
        assert!((p.target_ratio() - 0.4).abs() < 1e-9);
        assert!(p.target_tokens(1000) < p.trigger_tokens(1000));
    }

    #[test]
    fn aggressiveness_is_clamped() {
        let mut p = CompactionPolicy {
            aggressiveness: 3.0,
            ..Default::default()
        };
        assert!((p.target_ratio() - 0.0).abs() < 1e-9);
        p.aggressiveness = -1.0;
        assert!((p.target_ratio() - 0.8).abs() < 1e-9, "负的当 0 处理");
    }

    /// 浮点截断的坑：0.8 × (1 − 0.9) × 1200 在二进制浮点里是 95.999…，
    /// 截断会得到 95。预算要**四舍五入**。
    #[test]
    fn token_budgets_round_instead_of_truncating() {
        let p = CompactionPolicy {
            aggressiveness: 0.9,
            ..Default::default()
        };
        assert_eq!(p.target_tokens(1200), 96, "不该被浮点误差吃掉一个 token");
        assert_eq!(CompactionPolicy::default().trigger_tokens(1200), 960);
    }
}
