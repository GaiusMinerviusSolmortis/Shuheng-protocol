//! 压缩动作本身 —— 以及**让它成为一等事件**（§5.4.3 ⑥）。
//!
//! 一次压缩要留下这些事实：为什么压、什么时候压、压掉了什么、摘要放哪、
//! 新旧窗口是谁、压完多大。**可审计、可统计、可回放** ——
//! 这就是"把它当一等公民"的意思。

use crate::counter::TokenCounter;
use crate::history::History;
use crate::item::{Item, ItemKind};
use crate::policy::CompactionPolicy;
use crate::summarize::{SummaryInput, Summarizer};
use crate::window::Window;

/// **什么时候压** —— 它决定摘要放在哪（§5.4.3 ④ 那条"位置契约"）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum When {
    /// 回合之间。摘要**替掉**前缀，基线清掉，下一个正常回合会重新完整注入初始上下文
    PreTurn,
    /// 回合当中。摘要放在**最后一条 user 之前** ——
    /// 源码原话：模型被训练成把压缩摘要看成 mid-turn 之后历史里的最后一项
    MidTurn,
}

/// **为什么压** —— 照抄 codex 的四种理由。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    UserRequested,
    ContextLimit,
    ModelDownshift,
    /// 提示词组成变了。**反正缓存注定要碎，那就顺便压一次**（§5.4.3 ①）
    CompHashChanged,
}

impl Reason {
    pub fn as_str(self) -> &'static str {
        match self {
            Reason::UserRequested => "user_requested",
            Reason::ContextLimit => "context_limit",
            Reason::ModelDownshift => "model_downshift",
            Reason::CompHashChanged => "comp_hash_changed",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// 压了
    Done,
    /// 没什么可压的（轮次不够）
    NothingToDo,
}

/// 一次压缩的完整记录。**这一段就是会进会话日志的东西。**
use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CompactionRecord {
    pub reason: String,
    pub when: String,
    /// 谁做的：`mock` / `deepseek` / `responses-server`……
    pub implementation: String,
    pub strategy: String,
    pub status: String,
    /// 压之前的窗口编号
    pub window_before: u32,
    pub window_after: u32,
    pub window_id_before: String,
    pub window_id_after: String,
    pub tokens_before: u64,
    pub tokens_after: u64,
    pub turns_compacted: usize,
    /// **切在哪**（被摘要替掉的那一段的终点，绝对下标）。
    /// 重放要靠它把同一刀切回去 —— 审计也正想问这个。
    pub cut: usize,
    /// 被替掉的那些项（"原文去哪了"的答案）
    pub removed: Vec<Item>,
    pub summary_bytes: usize,
    /// 摘要放在哪个下标（给"位置契约"留证据）
    pub summary_at: usize,
}

impl CompactionRecord {
    /// 给人看的摘要行（`harness-memory demo` 直接打它）
    pub fn line(&self) -> String {
        format!(
            "[压缩] {} / {} / {} → 窗口 {}→{}（{} → {} token，压掉 {} 轮，摘要 {} 字节）",
            self.reason,
            self.when,
            self.implementation,
            self.window_before,
            self.window_after,
            self.tokens_before,
            self.tokens_after,
            self.turns_compacted,
            self.summary_bytes
        )
    }
}

/// pre-compact / post-compact 钩子。默认什么都不做。
///
/// 存在的意义是**可观测**：想审计、想统计、想在这里做沉淀（`memory_write`），
/// 都有地方挂。
pub trait Hooks {
    fn pre_compact(&mut self, _h: &History, _reason: Reason) {}
    fn post_compact(&mut self, _r: &CompactionRecord) {}
}

pub struct NoHooks;
impl Hooks for NoHooks {}

pub struct Compactor<'a> {
    pub policy: CompactionPolicy,
    pub counter: &'a dyn TokenCounter,
    pub summarizer: &'a dyn Summarizer,
    /// 会话 id —— 窗口 id 靠它可读且可回溯
    pub session: String,
}

impl<'a> Compactor<'a> {
    /// 现在该不该压。
    ///
    /// ⚠️ **`used_tokens` 必须是"整个请求"的大小，不只是历史。**
    /// 触发线问的是"上下文窗口还剩多少"，而落在窗口里的是
    /// **系统提示 + 历史** —— 系统提示可能比历史还大
    /// （它现在装着一整张工具名片，带参数表）。
    /// 这条以前只算历史，于是触发线被系统性低估：窗口都快满了还不压。
    pub fn should_compact_with(&self, used_tokens: u64, window: &Window) -> Option<Reason> {
        if self.policy.should_compact(used_tokens, window.model_window) {
            Some(Reason::ContextLimit)
        } else {
            None
        }
    }

    /// 只按历史算 —— 给"没有系统提示"的场景（比如 `harness-memory demo`）。
    pub fn should_compact(&self, history: &History, window: &Window) -> Option<Reason> {
        self.should_compact_with(history.tokens(self.counter), window)
    }

    /// 压一次。`None` = 没什么可压的（记录里会写 `NothingToDo`，这里直接返回 None）。
    pub fn compact(
        &self,
        history: &mut History,
        window: &mut Window,
        when: When,
        reason: Reason,
        hooks: &mut dyn Hooks,
    ) -> Option<CompactionRecord> {
        hooks.pre_compact(history, reason);

        let target = self.policy.target_tokens(window.model_window);
        let plan = history.plan_cut(self.policy.keep_recent_turns, target, self.counter)?;
        let start = history.system_prefix_len();
        let tokens_before = history.tokens(self.counter);

        // 摘要要看得见**上次的摘要**，否则信息一路丢
        let input = SummaryInput::new(history.render_range(start, plan.cut))
            .with_previous(history.previous_summary());
        let text = self.summarizer.summarize(&input);
        let summary_item = Item::new(ItemKind::Summary, text);
        let summary_bytes = summary_item.text.len();

        // 位置契约：MidTurn 贴到最后一条 user 之前；PreTurn 就替掉前缀
        let before_last_user = when == When::MidTurn;
        let removed =
            history.replace_prefix_with_summary(plan.cut, summary_item, before_last_user);

        // **切点安全性的运行时断言** —— 不许留下半对调用
        if let Err(e) = history.assert_safe() {
            panic!("压缩破坏了调用/结果的配对：{e}");
        }

        let tokens_after = history.tokens(self.counter);
        let old_number = window.number;
        let old_id = window.id.clone();
        let next = window.next_after(&self.session);
        *window = next; // 新窗口：编号 +1，基线清空
        window.prefill_input_tokens = Some(tokens_after);

        let summary_at = history
            .items()
            .iter()
            .position(|i| i.kind == ItemKind::Summary)
            .unwrap_or(0);
        let record = CompactionRecord {
            reason: reason.as_str().to_string(),
            when: match when {
                When::PreTurn => "pre_turn".into(),
                When::MidTurn => "mid_turn".into(),
            },
            implementation: self.summarizer.name().to_string(),
            strategy: match self.policy.mode {
                crate::policy::Mode::Linear => "linear/memento".into(),
                crate::policy::Mode::Sediment => "sediment".into(),
            },
            status: "done".into(),
            window_before: old_number,
            window_after: window.number,
            window_id_before: old_id,
            window_id_after: window.id.clone(),
            tokens_before,
            tokens_after,
            turns_compacted: plan.turns,
            cut: plan.cut,
            removed,
            summary_bytes,
            summary_at,
        };
        hooks.post_compact(&record);
        Some(record)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::counter::HeuristicCounter;
    use crate::summarize::MockSummarizer;

    fn long_turn(n: usize) -> Vec<Item> {
        vec![
            Item::user(format!("第 {n} 个请求：{}", "把这件事做了 ".repeat(20))),
            Item::assistant("好"),
            Item::tool_call("term.run", "{\"cmd\":\"cargo test\"}"),
            Item::tool_result("term.run", "ok ".repeat(40)),
            Item::assistant(format!("第 {n} 轮完成 {}", "细节 ".repeat(20))),
        ]
    }

    fn build(turns: usize) -> History {
        let mut h = History::new();
        h.push(Item::new(ItemKind::System, "你是助手"));
        for n in 1..=turns {
            for i in long_turn(n) {
                h.push(i);
            }
        }
        h
    }

    fn compactor<'a>(
        policy: CompactionPolicy,
        counter: &'a HeuristicCounter,
        s: &'a MockSummarizer,
    ) -> Compactor<'a> {
        Compactor {
            policy,
            counter,
            summarizer: s,
            session: "s-1".into(),
        }
    }

    #[test]
    fn compacts_when_over_the_trigger_and_resets_the_window() {
        let counter = HeuristicCounter::new();
        let summarizer = MockSummarizer::default();
        let mut policy = CompactionPolicy::default();
        policy.keep_recent_turns = 1;
        let c = compactor(policy, &counter, &summarizer);
        let mut h = build(6);
        // 窗口要小到**真的会触发** —— 这是测试数据的事，不是被测逻辑的事
        let mut w = Window::first("s-1", 400);
        w.calibrate(0);

        let used = h.tokens(&counter);
        assert!(c.should_compact(&h, &w).is_some(), "这么大的历史该触发（{used}）");

        let r = c
            .compact(&mut h, &mut w, When::PreTurn, Reason::ContextLimit, &mut NoHooks)
            .expect("应当压成功");
        assert_eq!(w.number, 2);
        assert_eq!(w.previous_id.as_deref(), Some("w1-s-1"));
        assert_eq!(w.prefill_input_tokens, Some(r.tokens_after), "新窗口基线 = 压完的大小");
        assert!(r.tokens_after < r.tokens_before, "压完必须变小");
        assert!(r.turns_compacted >= 1);
        assert!(!r.removed.is_empty());
        assert!(h.assert_safe().is_ok());
    }

    /// "一次压到位"：压完的结果要**明显低于**触发线，而不是刚好压回去一点。
    #[test]
    fn result_is_near_the_target_not_the_trigger() {
        let counter = HeuristicCounter::new();
        let summarizer = MockSummarizer { max_chars: 80 };
        let policy = CompactionPolicy {
            keep_recent_turns: 1,
            ..Default::default()
        };
        let c = compactor(policy.clone(), &counter, &summarizer);
        let mut h = build(10);
        let mut w = Window::first("s-1", 4000);
        let r = c
            .compact(&mut h, &mut w, When::PreTurn, Reason::ContextLimit, &mut NoHooks)
            .unwrap();
        assert!(
            r.tokens_after <= policy.target_tokens(4000),
            "压完 {} 应当 ≤ 目标 {}",
            r.tokens_after,
            policy.target_tokens(4000)
        );
    }

    #[test]
    fn nothing_to_do_when_there_are_too_few_turns() {
        let counter = HeuristicCounter::new();
        let summarizer = MockSummarizer::default();
        let policy = CompactionPolicy {
            keep_recent_turns: 3,
            ..Default::default()
        };
        let c = compactor(policy, &counter, &summarizer);
        let mut h = build(3);
        let mut w = Window::first("s-1", 100000);
        assert!(c
            .compact(&mut h, &mut w, When::PreTurn, Reason::ContextLimit, &mut NoHooks)
            .is_none());
        assert_eq!(w.number, 1, "没压成就不该换窗口");
    }

    #[test]
    fn mid_turn_records_the_placement() {
        let counter = HeuristicCounter::new();
        let summarizer = MockSummarizer::default();
        let policy = CompactionPolicy {
            keep_recent_turns: 1,
            ..Default::default()
        };
        let c = compactor(policy, &counter, &summarizer);
        let mut h = build(5);
        let mut w = Window::first("s-1", 400);
        let r = c
            .compact(&mut h, &mut w, When::MidTurn, Reason::CompHashChanged, &mut NoHooks)
            .unwrap();
        assert_eq!(r.when, "mid_turn");
        assert_eq!(r.reason, "comp_hash_changed");
        let last_user = h.items().iter().rposition(|i| i.kind == ItemKind::User).unwrap();
        assert_eq!(r.summary_at + 1, last_user, "记录里的位置要真的是那条契约");
    }

    #[test]
    fn second_compaction_folds_the_previous_summary() {
        let counter = HeuristicCounter::new();
        let summarizer = MockSummarizer::default();
        let c = compactor(
            CompactionPolicy {
                keep_recent_turns: 1,
                ..Default::default()
            },
            &counter,
            &summarizer,
        );
        let mut h = build(5);
        let mut w = Window::first("s-1", 2000);
        let first = c
            .compact(&mut h, &mut w, When::PreTurn, Reason::ContextLimit, &mut NoHooks)
            .unwrap();
        let first_summary = h.previous_summary().unwrap();

        for n in 6..=12 {
            for i in long_turn(n) {
                h.push(i);
            }
        }
        let second = c
            .compact(&mut h, &mut w, When::PreTurn, Reason::ContextLimit, &mut NoHooks)
            .unwrap();
        let second_summary = h.previous_summary().unwrap();
        assert_eq!(first.window_after, 2);
        assert_eq!(second.window_after, 3);
        assert!(
            second_summary.contains("折进上一次的摘要"),
            "第二次的摘要要把第一次折进去：{second_summary}"
        );
        assert!(first_summary.len() > 0);
    }
}
