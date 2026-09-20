//! 有序历史 + **安全的压缩切点**。
//!
//! 切点这件事只有一条规则，但它必须是硬的：
//!
//! > **一次工具调用和它的结果，绝不能被切开。**
//!
//! 切开之后模型会看到"调用了但没结果"（或反过来），那比不压缩糟得多。
//! 切在哪：**任何"开着的调用数为 0"的位置都行**（2026-09-20 放宽）。
//!
//! 原来只在**轮次边界**切（一轮 = 一个 `User` 到下一个 `User` 之前）——
//! 那样调用与结果天然在同一轮里，但**一个超长回合内部就压不了了**
//! （待定 #105：一个回合里连着几十次工具调用，窗口先爆在回合中间）。
//! 现在切点细化到**调用对粒度**：只要那个位置上没有"调了还没回"的调用，
//! 就能切 —— 这就是"调用与结果同进同出"那句话的精确形态。
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
    /// **这一刀切在回合内部**（不是轮边界）。
    ///
    /// 为什么要单独记：它改变了"模型接下来看到什么"的形状 ——
    /// 轮边界那刀切完，历史是"摘要 + 若干个完整轮次"；
    /// 回合内部那刀切完，历史是"摘要 + **同一个回合的后半截**"。
    /// 排查"它为什么忘了刚说过的话"时，这两者要分得开。
    pub mid_turn: bool,
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
        let start = self.system_prefix_len();
        let all = self.balanced_positions(start, self.items.len());
        // **保底要留下的那一段**从哪开始：正常是"最近 keep_turns 轮的起点"；
        // 轮数本来就不够时，保底的就是**全部历史**。
        let floor_from = if ranges.len() > keep_turns {
            ranges[ranges.len() - keep_turns].0
        } else {
            start
        };
        // 两条路，**分界线就是"保底那几轮够不够切"**：
        let chosen = if ranges.len() > keep_turns {
            // **正常那条**：有得切。在保底之上"能切多少切多少"——
            // 先找"切得最少又装得下"的，找不到就取那个范围内**最深**的一个。
            let normal = self.balanced_positions(start, floor_from);
            normal
                .iter()
                .find(|cut| self.kept_fits(**cut, start, target_tokens, counter))
                .copied()
                .or_else(|| normal.last().copied())?
        } else {
            // **保底覆盖了全部历史**（轮数本来就不够，比如"只有一个回合"）。
            // 老行为是直接不压 —— 而那条正是 #105 说的病：
            // 一个回合里连着几十次工具调用时，窗口会爆在回合中间，而压缩帮不上忙。
            //
            // 现在只在**真的装不下**时才动手：全部历史 ≤ 目标 ⇒ 什么都不做（老行为）；
            // 装不下 ⇒ **往这一轮内部切**，只保底留**最后一个完整段落**
            // （最后一次调用对 + 它之后的话）。
            //
            // ⚠️ 它把 `keep_recent_turns` 从**硬下限**变成了**软下限**：
            // 保底保不住时，宁可多压一点，也不要让窗口爆在回合中间。
            // 这条取舍记在 README 那四条硬要求里。
            if counter.count(&self.items[start..]) <= target_tokens {
                return None;
            }
            let soft_max = all.get(all.len().checked_sub(2)?).copied()?;
            self.balanced_positions(start, soft_max).last().copied()?
        };
        // 压掉了几轮：数一数 `[start, cut)` 里跨过了几个 `User`
        let turns = self.items[start..chosen]
            .iter()
            .filter(|i| i.kind == ItemKind::User)
            .count();
        let mid_turn = !ranges.iter().any(|(_, end)| *end == chosen);
        Some(CutPlan {
            cut: chosen,
            turns,
            mid_turn,
            before_tokens: self.tokens(counter),
        })
    }

    /// **合法的切点**：`[start, cut)` 里"调了还没回"的调用数为 0。
    ///
    /// 这就是"调用与结果同进同出"的**全部**内容 —— 只要这个位置上没有悬着的调用，
    /// 切在回合中间也是安全的。系统前缀本身永远不切（`cut > start`）。
    fn balanced_positions(&self, start: usize, max_cut: usize) -> Vec<usize> {
        let mut open = 0i64;
        let mut out = Vec::new();
        for (i, item) in self.items.iter().enumerate().skip(start) {
            // ⚠️ **切点不许越过 `max_cut`** —— 那是"必须留下的那几轮"的第一项。
            // 第一版写成 `i > max_cut`，于是它会在 `max_cut` 那一项上也推一个
            // `i + 1`：**把该留下的那一轮的第一项一起吃掉了**。
            // 抓到它的是"至少要留 2 轮"那条测试（它算出留了 1 轮）。
            if i + 1 > max_cut {
                break;
            }
            match item.kind {
                ItemKind::ToolCall { .. } => open += 1,
                ItemKind::ToolResult { .. } => open -= 1,
                _ => {}
            }
            // `i + 1` = "切在它之后"；`open == 0` = 那一刻没有悬着的调用
            if open == 0 && i + 1 > start {
                out.push(i + 1);
            }
        }
        out
    }

    /// 切在 `cut` 之后，剩下的（加上摘要的估算体积）装得下目标吗？
    fn kept_fits(
        &self,
        cut: usize,
        start: usize,
        target_tokens: u64,
        counter: &dyn TokenCounter,
    ) -> bool {
        let kept_tokens = counter.count(&self.items[cut..]);
        // 摘要本身也要占地方：粗估成被压部分的 1/5（写摘要总会短很多）
        let summary_est = counter.count(&self.items[start..cut]) / 5;
        kept_tokens + summary_est <= target_tokens
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
        // **按 id 配对的第二道**（2026-09-20 加）：原来只数个数 —— 而"个数对得上"
        // 不等于"配得上"：一个**没有 call_id**的结果能把计数凑平，
        // 而真 provider 那边会因此拒绝整条请求（`tool_calls` 必须逐个有回信）。
        // 它抓到的第一个真 bug 就是超时那条路：结果当时没带 id。
        let mut open_ids: Vec<String> = Vec::new();
        for item in &self.items {
            let id = item
                .data
                .as_ref()
                .and_then(|d| d.get("call_id"))
                .and_then(|v| v.as_str())
                .map(str::to_string);
            match item.kind {
                ItemKind::ToolCall { .. } => {
                    open_calls += 1;
                    if let Some(id) = id {
                        open_ids.push(id);
                    }
                }
                ItemKind::ToolResult { .. } => {
                    open_calls -= 1;
                    if open_calls < 0 {
                        return Err(format!(
                            "出现了一个没有对应调用的工具结果（`{}`）—— 切点把一对切开了",
                            item.text
                        ));
                    }
                    if let Some(id) = id {
                        match open_ids.iter().position(|x| x == &id) {
                            Some(pos) => {
                                open_ids.remove(pos);
                            }
                            None => {
                                return Err(format!(
                                    "结果带的是 `call_id={id}`，可没有哪个还开着口的调用用它 —— \
                                     这一对配不上（切点切了，或者上游漏了 call_id）"
                                ))
                            }
                        }
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
        if let Some(left) = open_ids.first() {
            return Err(format!(
                "还有调用 `call_id={left}` 没等到结果 —— 个数虽然是平的，\
                 但**配对配不上**（结果那边多半没带 id）"
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

    /// **轮数不够时**：装得下就不压；**装不下就往轮里切**（#105 那条新规则）。
    ///
    /// 这条测试是从它的反面改过来的：原来断言"轮数不够 ⇒ 一定不压"。
    /// 那句话在"只有一个超大回合"时会变成"窗口爆在回合中间而压缩帮不上忙"。
    /// 现在它两半都断言 —— **旧的那半没丢**（装得下时照样不动手）。
    #[test]
    fn too_few_turns_means_no_cut_unless_it_does_not_fit() {
        let c = HeuristicCounter::new();
        let h = history_of(2);
        // 目标给得宽（保底就是全部历史，装得下）⇒ 什么都不做
        assert!(h.plan_cut(2, u64::MAX, &c).is_none());
        // 目标够不着 ⇒ **往这一轮里切**，而不是放弃
        let plan = h.plan_cut(2, 1, &c).expect("装不下就该往轮里切");
        assert!(plan.mid_turn, "这一刀在回合内部");
    }

    /// **一个超长回合内部也能压**（待定 #105）。
    ///
    /// 在这之前切点只在轮边界，而"只有一轮"时那条规则直接返回 `None` ——
    /// 于是一个回合里连着几十次工具调用时，窗口**必然爆在回合中间**，
    /// 而压缩一点忙都帮不上。
    #[test]
    fn a_single_huge_turn_can_be_cut_inside() {
        let mut h = History::new();
        h.push(Item::new(ItemKind::System, "你是助手"));
        h.push(Item::user("把这件事办完"));
        // 一轮里连着 8 次工具调用
        for n in 1..=8 {
            h.push(Item::assistant(format!("第 {n} 步")));
            h.push(Item::tool_call("term.run", &format!("{{\"cmd\":\"echo {n}\"}}")));
            h.push(Item::tool_result("term.run", format!("结果 {n}")));
        }
        h.push(Item::assistant("办完了"));
        let plan = h
            .plan_cut(1, 0, &HeuristicCounter::new())
            .expect("一个长回合内部该能切");
        assert!(plan.mid_turn, "这一刀就在回合内部");
        assert!(plan.cut < h.items().len(), "切点得落在历史内部");
        // 切开之后不变量还得成立（调用对没被切开）
        let mut h2 = h.clone();
        h2.replace_prefix_with_summary(plan.cut, Item::new(ItemKind::Summary, "摘要"), true);
        assert!(h2.assert_safe().is_ok(), "{:?}", h2.assert_safe());
        // 而且**保底留下最后一个完整段落**：最后一条助手话还在
        assert_eq!(h2.items().last().unwrap().text, "办完了");
    }

    /// **切点不许切开一对调用**：随便挑一个位置试，只有"开着的调用为 0"的地方能切。
    #[test]
    fn balanced_positions_never_land_between_a_call_and_its_result() {
        let h = history_of(3);
        let positions = h.balanced_positions(h.system_prefix_len(), h.items().len());
        for p in positions {
            let mut probe = h.clone();
            probe.push(Item::new(ItemKind::Assistant, "哨兵"));
            // 在 p 处切一刀（把 [start,p) 换成摘要），不变量该照常成立
            probe.replace_prefix_with_summary(p, Item::new(ItemKind::Summary, "摘要"), false);
            assert!(probe.assert_safe().is_ok(), "切在 {p} 之后不该破配对");
        }
    }

    /// **按 `call_id` 配对的第二道闸**（2026-09-20 加）：个数平了、可配对配不上。
    ///
    /// 这条抓到的第一个真 bug 是**超时那条路**：调用带着 id，而那条
    /// "它一直没回"的结果**没带 id** —— 计数是平的，可真 provider 那边
    /// 「assistant 的 tool_calls 必须有对应回信」会因此拒绝整条请求。
    #[test]
    fn a_result_without_its_call_id_is_caught() {
        let mut h = History::new();
        h.push(Item::new(ItemKind::System, "你是助手"));
        h.push(Item::user("干点活"));
        h.push(Item::tool_call_with("term:1", "run", &serde_json::json!({"cmd": "ls"}), "call_1"));
        // 老写法：结果**没带 id**
        h.push(Item::tool_result("term.run", "结果"));
        let err = h.assert_safe().expect_err("个数平了、配对配不上 —— 该拦");
        assert!(err.contains("call_1"), "{err}");
        // 补上 id 就合法了
        let mut ok = History::new();
        ok.push(Item::new(ItemKind::System, "你是助手"));
        ok.push(Item::user("干点活"));
        ok.push(Item::tool_call_with("term:1", "run", &serde_json::json!({"cmd": "ls"}), "call_1"));
        ok.push(Item::tool_result_with("term.run", "结果", "call_1"));
        assert!(ok.assert_safe().is_ok());
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
