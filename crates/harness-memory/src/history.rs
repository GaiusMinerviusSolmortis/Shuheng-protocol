//! 有序历史 + **安全的压缩切点**。
//!
//! 切点这件事只有一条规则，但它必须是硬的：
//!
//! > **一次工具调用和它的结果，绝不能被切开。**
//!
//! 切开之后模型会看到"调用了但没结果"（或反过来），那比不压缩糟得多。
//! 我们的做法很省事也够稳：**只在轮次边界切**。一轮 = 一个 `User` 到下一个
//! `User` 之前，所以调用与结果天然在同一轮里。
//!
//! 另外，**开头连续的 `System` 永远不参与压缩** —— 它是那段稳定的前缀，
//! 压掉它等于每轮都重建一次提示词（而且是白碎一次缓存）。

use crate::counter::TokenCounter;
use crate::item::{Item, ItemKind};

#[derive(Debug, Clone, Default)]
pub struct History {
    items: Vec<Item>,
}

/// 一次压缩打算切哪一段。
#[derive(Debug, Clone, PartialEq)]
pub struct CutPlan {
    /// 被摘要替掉的那一段 `[0, cut)`（不含系统前缀）
    pub cut: usize,
    /// 压掉了几轮
    pub turns: usize,
    /// 这之前的估算 token
    pub before_tokens: u64,
}

impl History {
    pub fn new() -> Self {
        Self { items: Vec::new() }
    }

    pub fn from_items(items: Vec<Item>) -> Self {
        Self { items }
    }

    pub fn push(&mut self, item: Item) {
        self.items.push(item);
    }

    pub fn items(&self) -> &[Item] {
        &self.items
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// 开头连续的系统项 —— **它们不参与压缩**。
    pub fn system_prefix_len(&self) -> usize {
        self.items
            .iter()
            .take_while(|i| i.kind == ItemKind::System)
            .count()
    }

    /// 轮次区间（绝对值下标）。一轮从 `User` 开始，到下一个 `User` 之前。
    ///
    /// 系统前缀、以及第一轮之前的那些项（例如初始的 `Injected`）算作第 0 轮的
    /// "引子"，跟着第一轮一起被保留或压掉。
    pub fn turn_ranges(&self) -> Vec<(usize, usize)> {
        let start = self.system_prefix_len();
        let mut bounds = Vec::new();
        for (i, item) in self.items.iter().enumerate().skip(start) {
            if item.kind == ItemKind::User {
                bounds.push(i);
            }
        }
        if bounds.is_empty() {
            return Vec::new();
        }
        let mut ranges = Vec::new();
        for (n, b) in bounds.iter().enumerate() {
            let end = bounds.get(n + 1).copied().unwrap_or(self.items.len());
            ranges.push((*b, end));
        }
        // 第一轮之前残留的项（start..bounds[0]）并进第一轮
        if bounds[0] > start {
            ranges[0].0 = start;
        }
        ranges
    }

    /// 现在估算多少 token。
    pub fn tokens(&self, counter: &dyn TokenCounter) -> u64 {
        counter.count(&self.items)
    }

    /// 只要"最近 `keep_turns` 轮 + 摘要"能装进 `target_tokens`，
    /// 就往前多压一轮 —— 这就是 §5.4.4 的"**一次压到位**"。
    ///
    /// 返回 `None` 表示没什么可压的（可压轮次不够）。
    pub fn plan_cut(
        &self,
        keep_turns: usize,
        target_tokens: u64,
        counter: &dyn TokenCounter,
    ) -> Option<CutPlan> {
        let ranges = self.turn_ranges();
        // 至少要留下 keep_turns 轮，所以最多能压掉 len - keep_turns 轮
        if ranges.len() <= keep_turns {
            return None;
        }
        let max_cut_turns = ranges.len() - keep_turns;
        let start = self.system_prefix_len();

        // 从"只压一轮"起步，往后加，直到**剩下的部分**已经装得下 target
        let mut chosen = 1usize;
        for n in 1..=max_cut_turns {
            let cut = ranges[n - 1].1;
            let kept = &self.items[cut..];
            let kept_tokens = counter.count(kept);
            // 摘要本身也要占地方：粗估成被压部分的 1/5（写摘要总会短很多）
            let removed_tokens = counter.count(&self.items[start..cut]);
            let summary_est = removed_tokens / 5;
            chosen = n;
            if kept_tokens + summary_est <= target_tokens {
                break;
            }
        }
        let cut = ranges[chosen - 1].1;
        Some(CutPlan {
            cut,
            turns: chosen,
            before_tokens: self.tokens(counter),
        })
    }

    /// 把 `[0, cut)`（不含系统前缀）换成一条摘要，按 `placement` 放回去。
    ///
    /// 返回值：被换掉的那些项（给审计和"原文去哪了"用）。
    pub fn replace_prefix_with_summary(
        &mut self,
        cut: usize,
        summary_item: Item,
        before_last_user: bool,
    ) -> Vec<Item> {
        let start = self.system_prefix_len();
        let removed: Vec<Item> = self.items[start..cut].to_vec();
        self.items.drain(start..cut);
        // **只留一条摘要。** 上一次留下的摘要可能在保留区里（`MidTurn` 那种放法），
        // 而它的内容已经折进新摘要了 —— 留着就是两份互相矛盾的历史。
        self.items.retain(|i| i.kind != ItemKind::Summary);

        // 插入点：`MidTurn` 时放到**最后一条 user 之前** ——
        // 因为模型被训练成在那之后看到压缩摘要（§5.4.3 ④）。
        let mut at = start;
        if before_last_user {
            if let Some(last_user) = self.items.iter().rposition(|i| i.kind == ItemKind::User) {
                if last_user >= start {
                    at = last_user;
                }
            }
        }
        self.items.insert(at, summary_item);
        removed
    }

    /// **切点安全性的运行时断言**：不许出现"有调用没结果"或"有结果没调用"。
    ///
    /// 这条不写成文档里的一句希望 —— 它是压缩之后必须过的一关。
    pub fn assert_safe(&self) -> Result<(), String> {
        let mut open_calls = 0i32;
        for item in &self.items {
            match item.kind {
                ItemKind::ToolCall { .. } => open_calls += 1,
                ItemKind::ToolResult { .. } => {
                    open_calls -= 1;
                    if open_calls < 0 {
                        return Err(format!(
                            "出现了一个没有对应调用的工具结果（`{}`）—— 切点把一对切开了",
                            item.text
                        ));
                    }
                }
                _ => {}
            }
        }
        if open_calls != 0 {
            return Err(format!(
                "有 {open_calls} 次工具调用没有结果就断了 —— 切点把一对切开了"
            ));
        }
        Ok(())
    }

    /// 给摘要看的整段文本。
    pub fn render_range(&self, from: usize, to: usize) -> String {
        self.items[from..to]
            .iter()
            .map(|i| i.render())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// 上一次压缩留下的摘要（新摘要要把它折进去，否则越压越薄）。
    pub fn previous_summary(&self) -> Option<String> {
        self.items
            .iter()
            .find(|i| i.kind == ItemKind::Summary)
            .map(|i| i.text.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::counter::HeuristicCounter;

    /// 造一轮：用户说话 → 助手调工具 → 工具回话 → 助手总结
    fn turn(n: usize) -> Vec<Item> {
        vec![
            Item::user(format!("第 {n} 个请求")),
            Item::assistant("我去看一眼"),
            Item::tool_call("term.run", &format!("{{\"cmd\":\"echo {n}\"}}")),
            Item::tool_result("term.run", format!("结果 {n}")),
            Item::assistant(format!("第 {n} 轮做完了")),
        ]
    }

    fn history_of(turns: usize) -> History {
        let mut h = History::new();
        h.push(Item::new(ItemKind::System, "你是助手"));
        for n in 1..=turns {
            for item in turn(n) {
                h.push(item);
            }
        }
        h
    }

    #[test]
    fn system_prefix_is_never_compacted() {
        let h = history_of(3);
        assert_eq!(h.system_prefix_len(), 1);
        let plan = h.plan_cut(1, 0, &HeuristicCounter::new()).unwrap();
        assert!(plan.cut >= 1, "切点不能切进系统前缀");
    }

    #[test]
    fn cut_lands_on_a_turn_boundary_and_never_splits_a_call() {
        let h = history_of(4);
        let plan = h.plan_cut(1, 0, &HeuristicCounter::new()).unwrap();
        // 切点必须正好是某一轮的起点
        let starts: Vec<usize> = h.turn_ranges().iter().map(|(s, _)| *s).collect();
        assert!(starts.contains(&plan.cut), "切点 {:?} 不在轮次边界 {:?}", plan.cut, starts);
    }

    #[test]
    fn keeps_at_least_the_requested_recent_turns() {
        let h = history_of(6);
        let plan = h.plan_cut(2, 0, &HeuristicCounter::new()).unwrap();
        let ranges = h.turn_ranges();
        let cut_turn_index = ranges.iter().position(|(s, _)| *s == plan.cut).unwrap();
        let kept = ranges.len() - cut_turn_index;
        assert!(kept >= 2, "至少要留 2 轮，实际留了 {kept}");
    }

    #[test]
    fn aggressive_target_makes_it_cut_more() {
        let h = history_of(8);
        let c = HeuristicCounter::new();
        let lazy = h.plan_cut(1, u64::MAX, &c).unwrap();
        let eager = h.plan_cut(1, 1, &c).unwrap();
        assert!(eager.turns > lazy.turns, "目标越小该压得越狠：{} vs {}", eager.turns, lazy.turns);
    }

    #[test]
    fn nothing_to_cut_when_turns_are_few() {
        let h = history_of(2);
        assert!(h.plan_cut(2, 0, &HeuristicCounter::new()).is_none());
    }

    #[test]
    fn replacing_keeps_the_system_prefix_and_the_kept_turns() {
        let mut h = history_of(3);
        let plan = h.plan_cut(1, 0, &HeuristicCounter::new()).unwrap();
        let removed = h.replace_prefix_with_summary(
            plan.cut,
            Item::new(ItemKind::Summary, "摘要"),
            false,
        );
        assert!(!removed.is_empty());
        assert_eq!(h.items()[0].kind, ItemKind::System, "系统前缀必须还在最前面");
        assert_eq!(h.items()[1].kind, ItemKind::Summary);
        assert!(h.assert_safe().is_ok(), "压缩之后不许出现半对调用");
    }

    /// `MidTurn` 的位置契约：摘要要贴在**最后一条 user 之前**（§5.4.3 ④）。
    #[test]
    fn mid_turn_placement_puts_the_summary_before_the_last_user() {
        let mut h = history_of(4);
        let plan = h.plan_cut(1, 0, &HeuristicCounter::new()).unwrap();
        h.replace_prefix_with_summary(plan.cut, Item::new(ItemKind::Summary, "摘要"), true);
        let summary_at = h.items().iter().position(|i| i.kind == ItemKind::Summary).unwrap();
        let last_user_at = h.items().iter().rposition(|i| i.kind == ItemKind::User).unwrap();
        assert_eq!(summary_at + 1, last_user_at, "摘要应当紧挨在最后一条 user 前面");
        assert!(h.assert_safe().is_ok());
    }

    /// 反过来：`PreTurn` 时它是"替掉前缀"，就该待在系统前缀后面。
    #[test]
    fn pre_turn_placement_puts_the_summary_right_after_the_system_prefix() {
        let mut h = history_of(4);
        let plan = h.plan_cut(1, 0, &HeuristicCounter::new()).unwrap();
        h.replace_prefix_with_summary(plan.cut, Item::new(ItemKind::Summary, "摘要"), false);
        assert_eq!(h.items()[1].kind, ItemKind::Summary);
    }

    /// 上一次的摘要如果落在保留区里，也不能留下两份摘要 ——
    /// 内容已经折进新的那条了。
    #[test]
    fn only_one_summary_survives() {
        let mut h = history_of(6);
        // keep=2 + MidTurn 放法 ⇒ 第一次的摘要会**落在保留区里**
        // （它贴在倒数第二条 user 之后），第二次压缩必须把它清掉。
        let plan = h.plan_cut(2, 0, &HeuristicCounter::new()).unwrap();
        h.replace_prefix_with_summary(plan.cut, Item::new(ItemKind::Summary, "第一次摘要"), true);
        for n in 7..=10 {
            for item in turn(n) {
                h.push(item);
            }
        }
        let plan2 = h.plan_cut(2, 0, &HeuristicCounter::new()).unwrap();
        h.replace_prefix_with_summary(plan2.cut, Item::new(ItemKind::Summary, "第二次摘要"), true);
        let count = h
            .items()
            .iter()
            .filter(|i| i.kind == ItemKind::Summary)
            .count();
        assert_eq!(count, 1, "只能有一条摘要");
        assert!(h.previous_summary().unwrap().contains("第二次"));
    }
}
