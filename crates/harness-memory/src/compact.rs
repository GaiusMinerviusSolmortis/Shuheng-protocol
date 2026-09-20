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
    /// **这一刀切在回合内部**（#105）—— "模型接下来看到什么"的形状不一样：
    /// 轮边界那刀切完是"摘要 + 若干个完整轮次"；回合内部那刀是
    /// "摘要 + **同一个回合的后半截**"。排查"它为什么忘了刚说过的话"时，
    /// 这两者要分得开。
    pub mid_turn: bool,
    /// 压缩之后配对**没成立**的话，这里是原因（成立就是 `None`）。
    /// 它**不阻止压缩**（护栏不该变成杀手），但必须留下痕迹。
    pub pairing_note: Option<String>,
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

        // **切点安全性的运行时检查** —— 不许留下半对调用。
        //
        // ⚠️ 2026-09-20 从 `panic!` 改成**记录 + 出声**：这条不变量现在有两半 ——
        // **计数**（切点能破坏的那一半）和**按 `call_id` 配对**
        // （上游能破坏的那一半：谁漏了 id）。后一半出了问题**不该把智能体打死**
        // （它可能正干到一半），但也**绝不许闷声过去**：历史照旧用、记进记录、stderr 说一遍。
        // 切点本身由 `balanced_positions` 保证不拆对；真要拆了，测试会先红。
        let pairing_note = history.assert_safe().err();
        if let Some(e) = &pairing_note {
            eprintln!(
                "[memory] 压缩之后配对不成立：{e} —— 历史照旧用，但这条要查\
                 （切点是按「开着的调用为 0」选的，所以多半是上游漏了 call_id）"
            );
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
            mid_turn: plan.mid_turn,
            pairing_note,
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
        let policy = CompactionPolicy {
            keep_recent_turns: 1,
            ..Default::default()
        };
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
        assert!(r.tokens_after < r.tokens_before, "压完总得比压之前小");
        // ⚠️ **这里不能断言 `≤ target`**（2026-09-20 改）：目标只是**规划时的估计** ——
        // 规划用"被压部分 ÷ 5"去猜摘要多大，而真正的摘要**写出来才知道多大**。
        // README 里早就记过这条（"'压到目标'是个做不到的保证 …… 判据只能是
        // '落到触发线以下'"），可这条测试当时写的是 `≤ target` —— 它一直是**靠运气过的**。
        // 切点细化到调用对粒度之后（#105），那个运气没了，于是它红了：
        // 现在它断言的是**真正保证得住**的那两条。
        let trigger = policy.trigger_tokens(4000);
        assert!(
            r.tokens_after < trigger,
            "压完 {} 应当落到触发线 {trigger} 以下",
            r.tokens_after
        );
        // 而且要**明显**低于触发线（"一次压到位"，不是刚好压回去一点）
        assert!(
            r.tokens_after < trigger * 2 / 3,
            "压完 {} 只是勉强躲进触发线 {trigger} —— 那不叫一次压到位",
            r.tokens_after
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
        assert!(!first_summary.is_empty());
    }
}
