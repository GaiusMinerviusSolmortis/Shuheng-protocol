//! **智能体内核**：决定"什么进上下文、什么时候压、怎么压"（2026-09-26）。
//!
//! 在这之前，这套判断**长在智能体主循环里**（`Window` + `History` + `Compactor`
//! 三样直接挂在 `Agent` 上）—— 于是"换个内核"根本无从谈起：要换就得改主循环。
//!
//! 这一层给的是**同一份契约的两个档**：
//!
//! | 实现 | 在哪 | 看得到正文吗 | 怎么跟外面对话 |
//! |---|---|---|---|
//! | `linear` | 智能体**进程内** | 必然看得到（它就在手里）| 不对话 |
//! | `remote` | **另一个进程**（一个服务）| **由实例配置那一格决定** | 走 [`KernelIo`]（其实是中控那条路）|
//!
//! ⚠️ **为什么跨进程那一档是必须的**：Rust 静态链接的 trait 实现做不到"换个 exe
//! 就换内核" —— 别人写的 Python 内核（比如大眼那套记忆）根本接不进来。
//! 所以这一层给的**不是**"源码分层"，而是**运行时可换**。
//!
//! ⚠️ **而它必然带一个代价**：正文会出一个进程。所以 `sees_content` 那一格在
//! `remote` 上是**真的门**（写 `false` 就不发正文，只发元数据），在 `linear` 上
//! 则是**假话检测器**（进程内内核不可能读不到正文）。
//!
//! ⚠️ **不信任远端的切点**：远端回的那一刀要过 `History::assert_safe()` ——
//! 切在"一次调用和它的结果之间"的那种刀**不执行**，而且出声。
//! 跨进程等于把决定权交出去一半，**执行前的最后一道闸不能也交出去**。

use harness_memory::{
    compact::{CompactionRecord, Hooks, Reason, When},
    CompactionPolicy, Compactor, HeuristicCounter, History, Item, ItemKind, Summarizer,
    TokenCounter, Window,
};
use serde_json::{json, Value};

/// **内核跟外面对话的口子。** 线性内核一次都不用 —— 它要的东西都在手里。
pub trait KernelIo {
    /// 发一条请求给 `to`（一个服务实例），等它的结果回来。
    /// `None` = 没拿到（对方会说为什么；这一步就不压）。
    fn ask(&mut self, to: &str, tool: &str, args: Value) -> Option<Value>;
}

/// 这一步要用的那几样**外来的**东西（智能体提供，内核不持有）。
pub struct ReviewCtx<'a> {
    /// 系统提示：算 token 要算它（它可能比历史还大）
    pub system: &'a str,
    pub session: &'a str,
    pub policy: &'a CompactionPolicy,
    pub summarizer: &'a dyn Summarizer,
    pub hooks: &'a mut dyn Hooks,
    pub io: &'a mut dyn KernelIo,
}

/// `recall` 只要这几样 —— 它不压东西，也就用不着摘要器和策略。
pub struct RecallCtx<'a> {
    pub session: &'a str,
    pub io: &'a mut dyn KernelIo,
}

/// 压成了：**要记账的事实**（智能体拿它写那条 `compaction` 事件）。
pub struct Compacted {
    pub record: CompactionRecord,
    /// 新摘要的正文（它必须进日志：那是模型接下来会看到的东西）
    pub summary: String,
}

/// **内核**。三件事：拿着历史、告诉外面现在多大、该压就压。
pub trait Kernel {
    /// 名字（进实例配置自报：`kernel.kind`）
    fn kind(&self) -> &'static str;
    /// 它会不会读到**正文**（实例配置那一格说什么，就是什么）
    fn sees_content(&self) -> bool;
    /// 当前窗口编号（给 trace 看）
    fn window_number(&self) -> u32;
    /// 要给模型的那一份
    fn items(&self) -> &[Item];
    fn tokens(&self, counter: &dyn TokenCounter) -> u64;
    fn push(&mut self, item: Item);
    /// 用**真值**校准（provider 报的 input token 数）
    fn calibrate(&mut self, used_input_tokens: u64);
    /// **该压就压**：内核自己改自己的历史，把要记账的事实交回来。
    fn review(&mut self, ctx: &mut ReviewCtx) -> Option<Compacted>;
    /// **长期记忆**：跟这个查询有关、该放进上下文的东西。
    /// 线性内核恒空（它只有那一段线性的历史）；远端内核用它接外面那套记忆。
    fn recall(&mut self, _query: &str, _ctx: &mut RecallCtx) -> Vec<Item> {
        Vec::new()
    }
}

/// **线性内核**：留在智能体进程里的那一版（codex 那套窗口 + 压缩）。
///
/// 它的行为**和搬迁之前逐字一样** —— 这一版换的只是"谁拿着它"。
pub struct LinearKernel {
    history: History,
    window: Window,
}

impl LinearKernel {
    pub fn new(sid: &str, model_window: u64) -> Self {
        Self {
            history: History::new(),
            window: Window::first(sid, model_window),
        }
    }
}

impl Kernel for LinearKernel {
    fn kind(&self) -> &'static str {
        "linear"
    }
    fn sees_content(&self) -> bool {
        // 它就在这个进程里，读不到正文反而是不可能的
        true
    }
    fn window_number(&self) -> u32 {
        self.window.number
    }
    fn items(&self) -> &[Item] {
        self.history.items()
    }
    fn tokens(&self, counter: &dyn TokenCounter) -> u64 {
        self.history.tokens(counter)
    }
    fn push(&mut self, item: Item) {
        self.history.push(item);
    }
    fn calibrate(&mut self, used_input_tokens: u64) {
        self.window.calibrate(used_input_tokens);
    }
    fn review(&mut self, ctx: &mut ReviewCtx) -> Option<Compacted> {
        let counter = HeuristicCounter::new();
        let compactor = Compactor {
            policy: ctx.policy.clone(),
            counter: &counter,
            summarizer: ctx.summarizer,
            session: ctx.session.to_string(),
        };
        // **算的是整个请求**（系统提示 + 历史），不是只有历史
        let used = counter.count_text(ctx.system) + self.history.tokens(&counter);
        compactor.should_compact_with(used, &self.window)?;
        let r = compactor.compact(
            &mut self.history,
            &mut self.window,
            When::MidTurn,
            Reason::ContextLimit,
            ctx.hooks,
        )?;
        Some(Compacted {
            record: r,
            summary: self.history.previous_summary().unwrap_or_default(),
        })
    }
}

/// **远端内核**：把"该压成什么样"这件事交给**另一个进程**（A 档）。
///
/// 它是"无视技术栈组合"的那条路：对面可以是任何语言写的服务 ——
/// 只要它讲同一套 NDJSON 信封，并且认得 `review` / `recall` 两个动作。
///
/// **它自己仍然持有历史**（那是智能体的状态，不该搬走）；搬走的是**判断**。
pub struct RemoteKernel {
    /// 内核服务的实例 id（配置里写死的那个）
    service: String,
    /// **这个内核能不能看正文**（配置那一格）
    sees: bool,
    history: History,
    window: Window,
}

impl RemoteKernel {
    pub fn new(sid: &str, model_window: u64, service: impl Into<String>, sees_content: bool) -> Self {
        Self {
            service: service.into(),
            sees: sees_content,
            history: History::new(),
            window: Window::first(sid, model_window),
        }
    }

    /// 把历史变成**服务看得懂的那一份**。
    ///
    /// ⚠️ **`sees == false` 时只给元数据**（下标 / 类型 / 字节数）—— 正文一个字节都不发。
    /// 这就是那个档位**真的在做事**的样子：它不是一句声明，是这一行的分支。
    fn items_for_service(&self) -> Vec<Value> {
        self.history
            .items()
            .iter()
            .enumerate()
            .map(|(i, it)| {
                let mut v = json!({
                    "index": i,
                    "kind": it.kind.as_str(),
                    "bytes": it.text.len(),
                });
                if self.sees {
                    v["text"] = json!(it.text);
                }
                v
            })
            .collect()
    }

    fn apply_plan(&mut self, plan: &Value) -> Option<String> {
        let cut = plan.get("cut").and_then(Value::as_u64)? as usize;
        let summary = plan
            .get("summary")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let before_last_user = plan
            .get("before_last_user")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        self.history.replace_prefix_with_summary(
            cut,
            Item::new(ItemKind::Summary, summary.clone()),
            before_last_user,
        );
        // ⚠️ **执行前的最后一道闸不交给远端**：切在"调用与结果之间"的刀不认。
        if let Err(why) = self.history.assert_safe() {
            eprintln!("[agent] 远端内核给的切点会让配对断掉（{why}）—— 这一刀我**没有执行**");
            return None;
        }
        Some(summary)
    }
}

impl Kernel for RemoteKernel {
    fn kind(&self) -> &'static str {
        "remote"
    }
    fn sees_content(&self) -> bool {
        self.sees
    }
    fn window_number(&self) -> u32 {
        self.window.number
    }
    fn items(&self) -> &[Item] {
        self.history.items()
    }
    fn tokens(&self, counter: &dyn TokenCounter) -> u64 {
        self.history.tokens(counter)
    }
    fn push(&mut self, item: Item) {
        self.history.push(item);
    }
    fn calibrate(&mut self, used_input_tokens: u64) {
        self.window.calibrate(used_input_tokens);
    }
    fn review(&mut self, ctx: &mut ReviewCtx) -> Option<Compacted> {
        let counter = HeuristicCounter::new();
        let used = counter.count_text(ctx.system) + self.history.tokens(&counter);
        // **什么时候问，是本地的事**（触发线的策略没变：还是那份 `CompactionPolicy` + 窗口）。
        // 交给远端的是**下一问**："真该压的话，压成什么样" —— 每次调用都问一遍它，代价不小。
        let ask_now = Compactor {
            policy: ctx.policy.clone(),
            counter: &counter,
            summarizer: ctx.summarizer,
            session: ctx.session.to_string(),
        }
        .should_compact_with(used, &self.window)
        .is_some();
        if !ask_now {
            return None;
        }
        let args = json!({
            "window": self.window.number,
            "used_tokens": used,
            // 用掉多少比例（远端自己定阈值；本地只如实报"现在多满"）
            "used_ratio": self.window.used_ratio(used),
            "sees_content": self.sees,
            "items": self.items_for_service(),
        });
        let reply = ctx.io.ask(&self.service, "review", args)?;
        let plan = reply.get("plan").cloned().unwrap_or(Value::Null);
        if plan.is_null() {
            // 服务说"不用压" —— 那也是决定，如实采信
            return None;
        }
        let cut = plan.get("cut").and_then(Value::as_u64).unwrap_or(0) as usize;
        let summary = self.apply_plan(&plan)?;
        let tokens_after = self.history.tokens(&counter);
        let record = CompactionRecord {
            reason: "context_limit".into(),
            when: "mid_turn".into(),
            implementation: self.service.clone(),
            strategy: "remote".into(),
            status: "applied".into(),
            window_before: self.window.number,
            window_after: self.window.number,
            window_id_before: String::new(),
            window_id_after: String::new(),
            tokens_before: used,
            tokens_after,
            turns_compacted: cut,
            cut,
            // ⚠️ **远端那一刀搬不过来 `removed` 明细**（正文在对面，而且它可能
            // 压根没看正文）。所以这里是空的，**并在 `pairing_note` 里说清** ——
            // 留一个空数组却不说话，就成了"看起来记全了"。
            removed: Vec::new(),
            summary_bytes: summary.len(),
            summary_at: plan.get("summary_at").and_then(Value::as_u64).unwrap_or(0) as usize,
            mid_turn: true,
            pairing_note: Some(format!(
                "这一刀是**远端**给的（{}）—— `removed` 明细跨不了进程，所以是空的；\
                 摘要 {} 字节（要原文得看它那边的账）",
                self.service,
                summary.len()
            )),
        };
        Some(Compacted { record, summary })
    }

    fn recall(&mut self, query: &str, ctx: &mut RecallCtx) -> Vec<Item> {
        // ⚠️ **不看正文的内核，捞不了回忆 —— 那就不问它。**
        //
        // 为什么不是"把正文发过去让它捞"：那一格（`sees_content: false`）的意思是
        // **一个字节的正文都不出这个进程**。而"查询"本身就是内容（它就是用户那句话）——
        // 发过去就等于把这一格绕过去了。
        // 所以这一档的内核只能做**结构裁剪**（review 拿到的是下标 / 类型 / 字节数），
        // 想让它有回忆，就在配置里把那一格写成 `true`（A 档）。
        if !self.sees {
            eprintln!(
                "[agent] 内核 {} 声明了**不看正文** ⇒ 不问它要回忆（查询本身就是内容）",
                self.service
            );
            return Vec::new();
        }
        // `session` 也报过去：记忆服务要靠它分堆（"这条是哪个会话里的事"），
        // 不然跨会话一锅粥 —— 而"哪个会话"只有智能体这边知道（它拿着 `sid`）。
        let args = json!({"query": query, "session": ctx.session, "sees_content": self.sees});
        let Some(reply) = ctx.io.ask(&self.service, "recall", args) else {
            return Vec::new();
        };
        let Some(list) = reply.get("items").and_then(Value::as_array) else {
            return Vec::new();
        };
        list.iter()
            .filter_map(|v| v.get("text").and_then(Value::as_str))
            // **来源要看得出来**：这是内核捞回来的，不是谁下发的敕令
            .map(|t| Item::new(ItemKind::Recalled, format!("【回忆】{t}")))
            .collect()
    }
}
