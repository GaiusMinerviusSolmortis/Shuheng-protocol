//! `harness-memory demo` —— 把压缩这件事**跑给人看**。
//!
//! 它造一段多轮对话（每轮带一次工具调用），一轮一轮喂进去，
//! 每轮报告估算 token 和窗口状态；该压的时候压，并把压缩记录打出来。
//!
//! 第 4a 阶段的精神：**先用 mock 把机制跑通**，让"系统对不对"和
//! "模型听不听话"分得开。这里的摘要器就是 mock —— 确定性、可控大小。

use harness_memory::{
    compact::{Hooks, Reason, When},
    CompactionPolicy, CompactionRecord, Compactor, HeuristicCounter, History, Item, ItemKind,
    MockSummarizer, Summarizer, Window,
};
use serde::Serialize;
struct Args {
    window: u64,
    ratio: f64,
    aggressiveness: f64,
    keep: usize,
    turns: usize,
    summary_chars: usize,
    session: String,
    json: bool,
}

fn parse_args() -> Args {
    let mut a = Args {
        window: 4000,
        ratio: 0.80,
        aggressiveness: 0.5,
        keep: 2,
        turns: 12,
        summary_chars: 240,
        session: "s-demo".into(),
        json: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(x) = it.next() {
        match x.as_str() {
            "--window" => a.window = it.next().and_then(|v| v.parse().ok()).unwrap_or(a.window),
            "--ratio" => a.ratio = it.next().and_then(|v| v.parse().ok()).unwrap_or(a.ratio),
            "--aggressiveness" => {
                a.aggressiveness = it
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(a.aggressiveness)
            }
            "--keep" => a.keep = it.next().and_then(|v| v.parse().ok()).unwrap_or(a.keep),
            "--turns" => a.turns = it.next().and_then(|v| v.parse().ok()).unwrap_or(a.turns),
            "--summary-chars" => {
                a.summary_chars = it
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(a.summary_chars)
            }
            "--session" => a.session = it.next().unwrap_or(a.session),
            "--json" => a.json = true,
            "demo" => {}
            other => eprintln!("[memory] 不认识的参数：{other}"),
        }
    }
    a
}

/// 记录钩子：把 pre/post 都打出来 —— **压缩是可观测的**（§5.4.3 ⑥）。
#[derive(Default)]
struct TraceHooks {
    pub pre: Vec<String>,
    pub post: Vec<CompactionRecord>,
}

impl Hooks for TraceHooks {
    fn pre_compact(&mut self, h: &History, reason: Reason) {
        self.pre.push(format!(
            "pre-compact：历史 {} 项，理由 {}",
            h.len(),
            reason.as_str()
        ));
    }
    fn post_compact(&mut self, r: &CompactionRecord) {
        self.post.push(r.clone());
    }
}

fn turn_items(n: usize) -> Vec<Item> {
    vec![
        Item::user(format!(
            "第 {n} 轮：帮我把 {n} 号模块的测试跑一遍，顺便看看有没有回归"
        )),
        Item::assistant("我先跑测试，再看结果。"),
        Item::tool_call("term.run", "{\"cmd\":\"cargo test -p harness-memory\"}"),
        Item::tool_result(
            "term.run",
            format!(
                "running 12 tests\n... 第 {n} 轮：全部通过，用时 {}ms\n",
                n * 37
            ),
        ),
        Item::assistant(format!("第 {n} 轮：12 个测试全过，没有回归。{}", "细节 ".repeat(12))),
    ]
}

/// 给验收脚本看的报告 —— 一行 JSON，字段名和压缩记录对齐。
#[derive(Serialize)]
struct Step {
    turn: usize,
    tokens: u64,
    window: u32,
    triggered: bool,
}

#[derive(Serialize)]
struct Report {
    policy: serde_json::Value,
    steps: Vec<Step>,
    compactions: Vec<CompactionRecord>,
    final_tokens: u64,
    final_window: u32,
    final_window_id: String,
    safe: bool,
    summary_at: Option<usize>,
    item_count: usize,
}

fn main() {
    let args = parse_args();
    let policy = CompactionPolicy {
        context_ratio: args.ratio,
        aggressiveness: args.aggressiveness,
        keep_recent_turns: args.keep,
        ..Default::default()
    };
    let counter = HeuristicCounter::new();
    let summarizer = MockSummarizer {
        max_chars: args.summary_chars,
    };
    let compactor = Compactor {
        policy: policy.clone(),
        counter: &counter,
        summarizer: &summarizer,
        session: args.session.clone(),
    };

    let mut history = History::new();
    history.push(Item::new(
        ItemKind::System,
        "你是一个编码助手。工具调用必须和它的结果配对。",
    ));
    let mut window = Window::first(&args.session, args.window);
    let mut hooks = TraceHooks::default();

    if !args.json {
        println!(
            "模型窗口 {} token，触发线 {:.0}%（{} token），压完目标 {:.0}%（{} token），保留最近 {} 轮",
            args.window,
            policy.context_ratio * 100.0,
            policy.trigger_tokens(args.window),
            policy.target_ratio() * 100.0,
            policy.target_tokens(args.window),
            policy.keep_recent_turns
        );
        println!("{:>4}  {:>8}  {:>6}  {}", "轮", "估算token", "窗口", "事件");
    }
    let mut steps: Vec<Step> = Vec::new();

    for n in 1..=args.turns {
        for item in turn_items(n) {
            history.push(item);
        }
        let used = history.tokens(&counter);
        window.open_with_estimate(&history, &counter);
        let mut events: Vec<String> = Vec::new();

        // 每轮之后看一眼：该压就压（回合之间 ⇒ PreTurn）
        if compactor.should_compact(&history, &window).is_some() {
            let used_ratio = window.used_ratio(used);
            events.push(format!("触发（{:.0}% of 窗口）", used_ratio * 100.0));
            let compressed = compactor.compact(
                &mut history,
                &mut window,
                When::PreTurn,
                Reason::ContextLimit,
                &mut hooks,
            );
            match compressed {
                Some(r) => {
                    events.push(r.line());
                }
                None => events.push("没什么可压的（轮次不够）".into()),
            }
        }
        let triggered = !events.is_empty();
        if !args.json {
            println!(
                "{:>4}  {:>8}  {:>6}  {}",
                n,
                used,
                window.number,
                if events.is_empty() {
                    "-".to_string()
                } else {
                    events.join("；")
                }
            );
        }
        steps.push(Step {
            turn: n,
            tokens: used,
            window: window.number,
            triggered,
        });
    }

    let safe = history.assert_safe();
    let final_tokens = history.tokens(&counter);
    let summary_at = history
        .items()
        .iter()
        .position(|i| i.kind == ItemKind::Summary);

    if args.json {
        let report = Report {
            policy: serde_json::json!({
                "model_window": args.window,
                "context_ratio": policy.context_ratio,
                "aggressiveness": policy.aggressiveness,
                "keep_recent_turns": policy.keep_recent_turns,
                "trigger_tokens": policy.trigger_tokens(args.window),
                "target_tokens": policy.target_tokens(args.window)
            }),
            steps,
            compactions: hooks.post.clone(),
            final_tokens,
            final_window: window.number,
            final_window_id: window.id.clone(),
            safe: safe.is_ok(),
            summary_at,
            item_count: history.len(),
        };
        println!("{}", serde_json::to_string(&report).expect("报告要能序列化"));
        return;
    }

    println!();
    println!("== 钩子（压缩是一等事件）==");
    for line in &hooks.pre {
        println!("  {line}");
    }
    for r in &hooks.post {
        println!(
            "  post-compact：{} → {}，策略 {}，实现 {}，状态 {}",
            r.window_id_before, r.window_id_after, r.strategy, r.implementation, r.status
        );
    }

    println!();
    println!("== 压缩之后的历史（{} 项）==", history.len());
    for (i, item) in history.items().iter().enumerate() {
        let head: String = item.text.chars().take(46).collect();
        println!("  {i:>2}  {:<11} {}", item.kind.as_str(), head);
    }

    println!();
    println!("== 不变量 ==");
    match safe {
        Ok(()) => println!("  切点是安全的：没有半对的工具调用"),
        Err(e) => {
            println!("  ✗ {e}");
            std::process::exit(1);
        }
    }
    println!(
        "  估算 {} token（窗口 {}，基线 {:?}，增长 {}）",
        final_tokens,
        window.number,
        window.prefill_input_tokens,
        window.growth(final_tokens)
    );
    println!("  摘要是由 `{}` 写的", summarizer.name());
}
