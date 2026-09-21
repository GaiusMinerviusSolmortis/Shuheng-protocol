//! `harness-agent` —— 那个**拿着提示词、决定调什么、什么时候停**的主体。
//!
//! 它和工具服务同形（都是中控的子进程、都讲 NDJSON、都先自报 presence），
//! 只是它手里多两样东西：**模型**与**上下文**。
//!
//! ```text
//!   console ──request(task)──▶ ctrl ──▶ agent
//!                                        │ 编译提示词（一次）→ 冻住
//!                                        │ 调模型
//!                                        ├── 有工具调用 ──▶ ctrl ──▶ 工具实例
//!                                        │        ▲            │
//!                                        │        └── result ──┘
//!                                        │ 收敕令（inject）→ 追加进上下文
//!                                        └── 没有调用 ⇒ 结论文本 ⇒ result 回 console
//! ```

use harness_agent::deepseek::DeepSeekModel;
use harness_agent::model::{MockModel, Model, ModelRequest, ToolCall};
use harness_agent::prompt::{self, Persona, ToolCard};
use harness_agent::script::parse_script;
use harness_core::codec::{self, NdjsonReader, NextLine};
use harness_core::envelope::{Authority, Envelope, MessageType};
use harness_core::id::IdGen;
use harness_core::now_ms;
use harness_memory::{
    compact::{CompactionRecord, Hooks, Reason, When},
    CompactionPolicy, Compactor, HeuristicCounter, History, Item, ItemKind, MockSummarizer,
    TokenCounter, Window,
};
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{self, BufReader, Write};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

const TICK: Duration = Duration::from_millis(50);

struct Args {
    instance: String,
    sid: String,
    persona: String,
    persona_version: String,
    persona_brief: String,
    fences: Vec<String>,
    /// **不再自带名片** —— 工具定义来自中控（它转服务自报的那份 manifest）
    script: Vec<harness_agent::Step>,
    max_steps: usize,
    call_timeout: Duration,
    model_window: u64,
    model: String,
    base_url: String,
    model_name: String,
    max_tokens: u64,
    env_file: Option<String>,
    /// **摘要器用谁**（待定 #103）。`None` = 按模型类型定默认（真 provider ⇒ model）
    summarize: Option<SummarizeWith>,
    /// **恢复**：启动时向中控要回"我上次看到过的东西"
    resume: bool,
}

fn parse_args() -> Args {
    // 抽一层出来是为了**能测**：`parse_args` 读进程参数，测试里没法喂。
    parse_args_from(std::env::args().skip(1))
}

fn parse_args_from<I: Iterator<Item = String>>(mut it: I) -> Args {
    let mut a = Args {
        instance: "agent:dada".into(),
        sid: "s-1".into(),
        persona: "达达".into(),
        persona_version: "1".into(),
        persona_brief: "一个干活麻利的编码助手；只说做过的事。".into(),
        fences: Vec::new(),
        script: Vec::new(),
        max_steps: 8,
        call_timeout: Duration::from_secs(30),
        model_window: 8000,
        model: "mock".into(),
        base_url: "https://api.deepseek.com".into(),
        model_name: "deepseek-chat".into(),
        max_tokens: 1024,
        env_file: None,
        resume: false,
        summarize: None,
    };
    while let Some(x) = it.next() {
        match x.as_str() {
            "--instance" => a.instance = it.next().unwrap_or(a.instance),
            "--sid" => a.sid = it.next().unwrap_or(a.sid),
            "--persona" => a.persona = it.next().unwrap_or(a.persona),
            "--persona-version" => a.persona_version = it.next().unwrap_or(a.persona_version),
            "--persona-brief" => a.persona_brief = it.next().unwrap_or(a.persona_brief),
            "--fence" => {
                if let Some(v) = it.next() {
                    a.fences.push(v);
                }
            }
            "--script" => {
                if let Some(p) = it.next() {
                    match std::fs::read_to_string(&p)
                        .map_err(|e| e.to_string())
                        .and_then(|t| parse_script(&t).map_err(|e| e.to_string()))
                    {
                        Ok(s) => a.script = s,
                        Err(e) => eprintln!("[agent] 读脚本 {p} 失败：{e}"),
                    }
                }
            }
            "--max-steps" => {
                a.max_steps = it.next().and_then(|v| v.parse().ok()).unwrap_or(a.max_steps)
            }
            "--call-timeout-ms" => {
                if let Some(ms) = it.next().and_then(|v| v.parse::<u64>().ok()) {
                    a.call_timeout = Duration::from_millis(ms);
                }
            }
            "--model-window" => {
                a.model_window = it
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(a.model_window)
            }
            "--model" => a.model = it.next().unwrap_or(a.model),
            "--base-url" => a.base_url = it.next().unwrap_or(a.base_url),
            "--model-name" => a.model_name = it.next().unwrap_or(a.model_name),
            "--max-tokens" => {
                a.max_tokens = it.next().and_then(|v| v.parse().ok()).unwrap_or(a.max_tokens)
            }
            "--env-file" => a.env_file = it.next(),
            "--summarize" => {
                let v = it.next().unwrap_or_default();
                match SummarizeWith::parse(&v) {
                    Some(s) => a.summarize = Some(s),
                    None => eprintln!(
                        "[agent] `--summarize` 只认 `mock` / `model`，收到 `{v}` —— 忽略它"
                    ),
                }
            }
            "--resume" => a.resume = true,
            _ => {}
        }
    }
    a
}

/// 从 `my_tools` 的答复里拼出名片，并挑出**还没自报 manifest** 的那些实例。
///
/// 抽成自由函数是因为有两个时刻要用它：**启动时**（编译提示词之前）
/// 和**每个回合开始前**（扫一遍授权，见 `refresh_tools`）。
fn cards_from(data: &Value) -> (Vec<ToolCard>, Vec<String>) {
    let mut cards = Vec::new();
    let mut waiting: Vec<String> = Vec::new();
    for entry in data
        .get("tools")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
    {
        let Some(inst) = entry.get("instance").and_then(Value::as_str) else {
            continue;
        };
        match entry.get("manifest") {
            Some(m) if !m.is_null() => match prompt::card_from_manifest(inst, m) {
                Some(c) => cards.push(c),
                None => {
                    eprintln!("[agent] {inst} 的 manifest 里没有工具表 —— 它没尽到 §11.1 的义务");
                }
            },
            _ => waiting.push(inst.to_string()),
        }
    }
    (cards, waiting)
}

/// **名册的指纹**：哪些实例 + 每个实例上有哪些工具。
///
/// 比的是"这一份名册和上一份一样吗"。**工具名也要进指纹**：
/// 同一个实例改了 manifest（加了个工具）也算变 —— 只比实例 id 会漏掉那一种。
/// 顺序要排过，否则"服务报家门的顺序变了"会被误判成名册变了、白炸一次缓存。
fn toolset_signature(cards: &[ToolCard]) -> Vec<(String, Vec<String>)> {
    let mut out: Vec<(String, Vec<String>)> = cards
        .iter()
        .map(|c| {
            let mut names: Vec<String> = c.tools.iter().map(|t| t.name.clone()).collect();
            names.sort();
            (c.instance.clone(), names)
        })
        .collect();
    out.sort();
    out
}

/// **凭证的指纹**：每个实例上，我被放行的是哪几条能力。
///
/// 和 [`toolset_signature`] 分开记，是因为它们**能单独变**：
/// 用户可以把 `term:1` 从"通配"改签成"只给 `proc.read`" ——
/// 实例没变、工具名没变，而模型**能做的事变了**。
/// 只比工具名的话这一刀就漏了，于是模型靠撞墙发现（§7）。
fn caps_signature(data: &Value) -> Vec<(String, Vec<String>)> {
    let mut out: Vec<(String, Vec<String>)> = data
        .get("bindings")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|b| {
                    let inst = b.get("instance")?.as_str()?.to_string();
                    let mut caps: Vec<String> = b
                        .get("capabilities")
                        .and_then(Value::as_array)
                        .map(|c| {
                            c.iter()
                                .filter_map(Value::as_str)
                                .map(str::to_string)
                                .collect()
                        })
                        .unwrap_or_default();
                    caps.sort();
                    Some((inst, caps))
                })
                .collect()
        })
        .unwrap_or_default();
    out.sort();
    out
}

/// 从 `.env` 风格的文件里取一个键。**只为方便本地测试**（像 WXBot 那份 .env）。
fn key_from_env_file(path: &str, key: &str) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    for line in text.lines() {
        let l = line.trim();
        if l.starts_with('#') || !l.contains('=') {
            continue;
        }
        let (k, v) = l.split_once('=')?;
        if k.trim() == key {
            let v = v.trim().trim_matches('"').trim_matches('\'').to_string();
            if !v.is_empty() {
                return Some(v);
            }
        }
    }
    None
}

/// **摘要器**（待定 #103）：压缩时那一次"把这一段压成交接说明"的调用，用谁来做。
///
/// 在这之前它**永远是 mock**（确定性的填充文本）—— 那对"测系统"是对的，
/// 但对"真的在长会话里干活"是错的：**摘要的质量就是压缩之后模型还能不能接着干**。
///
/// 三种取值：
///
/// | 取值 | 谁在做摘要 |
/// |---|---|
/// | `mock` | 确定性填充（测系统用 —— 让"系统对不对"和"模型听不听话"分得开）|
/// | `model` | **真模型**（和主模型同一个 provider，另开一个客户端）|
/// | 默认 | 真 provider ⇒ `model`；mock 脚本 ⇒ `mock`（显式、不猜：脚本那份是测试用的）|
///
/// ⚠️ **真模型那一路会失败**（网络、限流）。失败了**退回 mock 并把原因写进事件** ——
/// 压缩这件事不能因为摘要器连不上就整个不做（那会把上下文顶爆），
/// 但也**不许装作那是模型的摘要**。
#[derive(Debug, Clone, Copy, PartialEq)]
enum SummarizeWith {
    Mock,
    Model,
}

impl SummarizeWith {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "mock" => Some(Self::Mock),
            "model" => Some(Self::Model),
            _ => None,
        }
    }
}

/// **摘要用谁做**（待定 #103）：显式给了就听显式的；没给就**按模型类型定**。
///
/// 为什么默认不是"一律用真模型"：mock 脚本那套是**测试工具** ——
/// 它吐的时间线是脚本，拿它去喂摘要模型只会得到一段没有意义的摘要，
/// 而测试要的是**确定性**（"系统对不对"和"模型听不听话"分得开）。
///
/// 抽成自由函数，是为了**只有一个地方做这个判断** —— 两处各判一次迟早不一致。
fn summarize_choice(a: &Args) -> SummarizeWith {
    a.summarize.unwrap_or(if a.model == "deepseek" {
        SummarizeWith::Model
    } else {
        SummarizeWith::Mock
    })
}

/// 用模型做摘要的人。
///
/// 它握着**自己那份**模型客户端（不是主模型那个）—— 因为 `Summarizer::summarize`
/// 拿的是 `&self`，而主模型在回合循环里是要 `&mut` 的。两份客户端的代价是
/// 两个连接池，换来的是**不用去动 `harness-memory` 那个 trait**（它不该知道模型的事）。
struct ModelSummarizer {
    /// `RefCell` 是因为 `Summarizer::summarize` 拿的是 `&self`：
    /// 调用模型要 `&mut`（provider 客户端是有状态的）。单线程，够用。
    model: std::cell::RefCell<Box<dyn Model>>,
    /// 上一次调用留下的痕迹（`&self` 里要记东西 ⇒ 内部可变性；单线程，够用）
    last: std::cell::RefCell<SummarizerTrace>,
}

#[derive(Debug, Clone, Default)]
struct SummarizerTrace {
    /// 真模型报回来的输入 token（没有就是没走到那一步）
    tokens_in: u64,
    /// 真模型这条路失败了 ⇒ 退回了 mock，这里是原因
    fell_back: Option<String>,
}

impl ModelSummarizer {
    fn new(model: Box<dyn Model>) -> Self {
        Self {
            model: std::cell::RefCell::new(model),
            last: std::cell::RefCell::new(SummarizerTrace::default()),
        }
    }
}

impl harness_memory::summarize::Summarizer for ModelSummarizer {
    fn name(&self) -> &'static str {
        "model"
    }

    fn summarize(&self, input: &harness_memory::summarize::SummaryInput) -> String {
        let mut system = input.instructions.clone();
        if let Some(prev) = &input.previous_summary {
            // **新摘要必须看得见旧摘要**，否则越压越薄（memory 那边那条纪律）
            system.push_str("\n\n另外，这是**上一次**的交接说明，你要把它折进新的里：\n");
            system.push_str(prev);
        }
        let req = ModelRequest {
            system,
            items: vec![harness_memory::Item::user(format!(
                "以下是需要压缩的那一段记录：\n\n{}",
                input.transcript
            ))],
            // 摘要不调工具：它是"把话压成话"，不是"再干一遍活"
            tools: Vec::new(),
        };
        let reply = self.model.borrow_mut().reply(&req);
        let mut trace = self.last.borrow_mut();
        if let Some(why) = reply.error {
            // **失败就退回确定性那条，并把原因留在事件里**（不许装成模型的摘要）
            trace.fell_back = Some(why);
            trace.tokens_in = 0;
            return harness_memory::summarize::MockSummarizer::default().summarize(input);
        }
        trace.tokens_in = reply.usage_input_tokens;
        trace.fell_back = None;
        match reply.text {
            Some(text) if !text.trim().is_empty() => text,
            // 模型回了空话 ⇒ **那是模型听不听话的问题**，不是系统坏了；
            // 但空摘要放进上下文比不压还糟，所以同样退回确定性那条，并说清楚
            _ => {
                trace.fell_back = Some("模型回了个空摘要".to_string());
                harness_memory::summarize::MockSummarizer::default().summarize(input)
            }
        }
    }
}

/// 挑一个模型：mock（脚本）还是真 provider。
fn build_model(args: &Args) -> Box<dyn Model> {
    if args.model != "deepseek" {
        return Box::new(MockModel::new(args.script.clone()));
    }
    let key = std::env::var("DEEPSEEK_API_KEY").ok().or_else(|| {
        args.env_file
            .as_deref()
            .and_then(|p| key_from_env_file(p, "DEEPSEEK_API_KEY"))
    });
    match key {
        Some(k) => {
            eprintln!(
                "[agent] 用真实模型：{} @ {}（key 长度 {}）",
                args.model_name,
                args.base_url,
                k.len()
            );
            Box::new(DeepSeekModel::new(
                k,
                &args.base_url,
                &args.model_name,
                args.max_tokens,
            ))
        }
        None => {
            eprintln!(
                "[agent] 没有拿到 `DEEPSEEK_API_KEY` —— 既不在环境变量里，\
                 也没在 --env-file 指的文件里。**退回 mock**（脚本 {} 步）",
                args.script.len()
            );
            Box::new(MockModel::new(args.script.clone()))
        }
    }
}

enum Inbound {
    Line(String),
    Eof,
}

/// 记压缩的钩子 —— 压缩是一等事件，这里把它留下痕迹（§5.4.3 ⑥）。
#[derive(Default)]
struct TraceHooks {
    pub records: Vec<CompactionRecord>,
}

impl Hooks for TraceHooks {
    fn post_compact(&mut self, r: &CompactionRecord) {
        eprintln!("[agent] 压缩：{}", r.line());
        self.records.push(r.clone());
    }
}

/// 一次还没回来的工具调用。
struct Pending {
    call: ToolCall,
    /// 配回模型那次调用用的 id（provider 给的，或我们自己生成的）
    call_id: String,
    started: Instant,
}

#[derive(Serialize)]
struct Trace {
    steps: usize,
    tool_calls: usize,
    /// **参数没解出来、因此根本没发出去的调用数**。
    ///
    /// 为什么它要是个单独的数：它是**"模型不听话"和"我们的解析坏了"之间的分界**。
    /// 只看 `tool_calls` 的话，这两种情况长得一模一样（都是"没调成"）。
    bad_calls: usize,
    injects: usize,
    compactions: usize,
    tokens: u64,
    window: u32,
    model: String,
}

struct Agent {
    args: Args,
    ids: IdGen,
    /// 中控发下来的工具名片（从服务自报的 manifest 拼的）
    cards: Vec<ToolCard>,
    /// **我手里这几张凭证**（`my_tools` 的 `bindings`）—— 上一次扫到的样子。
    ///
    /// 为什么它要单独记：**"有哪些工具"和"能对它做什么"是两个变化源**。
    /// 只比工具名的话，`grant` 一个"只给了 `proc.read` 的 term:1" 会被当成
    /// "名册没变"（实例还是 term:1）—— 于是模型被蒙在鼓里，靠撞墙去发现
    /// 自己不能跑命令（2026-09-20 审计）。
    caps_sig: Vec<(String, Vec<String>)>,
    /// 编译一次就冻住的那段前缀（§15.2）
    system: String,
    manifest: prompt::CompositionManifest,
    history: History,
    window: Window,
    policy: CompactionPolicy,
    hooks: TraceHooks,
    model: Box<dyn Model>,
    pending: HashMap<String, Pending>,
    /// **扣着的敕令**：手上有未决调用时收到的那些。
    ///
    /// 为什么必须扣着（2026-09-20，整机"服务被杀"那条验收抓到）：
    /// provider 的硬要求是 **assistant 的 `tool_calls` 之后必须紧跟它的 `tool` 消息**。
    /// 而敕令是**别人主动塞进来的**（中控发现服务下线时就会来一条），
    /// 它一到就往历史里追，正好插在"调用"和"结果"中间 —— 下一个请求直接被拒：
    /// `insufficient tool messages following tool_calls message`。
    /// **一条敕令能让整条会话报废**，而它看起来只是"多了一句话"。
    held_injects: Vec<String>,
    /// 上一次工具结果的 payload —— 让 mock 脚本能像真模型那样
    /// **看着结果决定下一步**（`"$last.instance_id"` 这种模板就靠它）
    last_payload: Option<Value>,
    /// 一个回合没走完时又来一个任务 ⇒ `busy`（同终端那条"实例内部串行"的纪律）。
    /// 而且它必须挡在**递归**之前：等工具结果的那段循环里也会收到消息。
    busy: bool,
    /// **启动握手期间收到的消息**：那时主循环还没开始，而 `ask_ctrl` 只等它要的那条回信。
    /// 别的不许丢 —— 控制台的任务往往就在那几百毫秒里到（第一次跑就丢了它）。
    inbox: std::collections::VecDeque<String>,
    trace: Trace,
}

fn main() {
    let args = parse_args();
    let persona = Persona {
        name: args.persona.clone(),
        version: args.persona_version.clone(),
        brief: args.persona_brief.clone(),
    };
    let mut a = Agent {
        ids: IdGen::new(args.instance.replace([':', '.'], "-")),
        // 提示词要等**问完中控"我被授权用什么"**才能编译 ——
        // 所以这两个字段先空着，拿到工具之后再填（见下）
        system: String::new(),
        manifest: prompt::CompositionManifest {
            agent: args.instance.clone(),
            persona: args.persona.clone(),
            persona_version: args.persona_version.clone(),
            tool_instances: Vec::new(),
            fence: args.fences.clone(),
            compiler: "harness-agent/prompt v1".into(),
            compiled_at: 0,
        },
    cards: Vec::new(),
    caps_sig: Vec::new(),
        history: History::new(),
        window: Window::first(&args.sid, args.model_window),
        policy: CompactionPolicy::default(),
        hooks: TraceHooks::default(),
        model: build_model(&args),
        pending: HashMap::new(),
        held_injects: Vec::new(),
        last_payload: None,
        busy: false,
        inbox: std::collections::VecDeque::new(),
        trace: Trace {
            steps: 0,
            tool_calls: 0,
            bad_calls: 0,
            injects: 0,
            compactions: 0,
            tokens: 0,
            window: 1,
            model: if args.model == "deepseek" { "deepseek".into() } else { "mock-script".into() },
        },
        args,
    };

    let (tx, rx): (Sender<Inbound>, Receiver<Inbound>) = mpsc::channel();
    {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let mut reader = NdjsonReader::new(BufReader::new(io::stdin()));
            loop {
                match reader.next_line_checked() {
                    Ok(NextLine::Line(line)) => {
                        if tx.send(Inbound::Line(line)).is_err() {
                            return;
                        }
                    }
                    Ok(NextLine::Invalid { bytes }) => {
                        eprintln!("[agent] 跳过一行非 UTF-8 的输入（{bytes} 字节）")
                    }
                    Ok(NextLine::Eof) => break,
                    Err(e) => {
                        eprintln!("[agent] 读 stdin 失败：{e}");
                        break;
                    }
                }
            }
            let _ = tx.send(Inbound::Eof);
        });
    }

    let mut out = io::stdout();
    // 1. 上线自报（中控得先认识我，才答得了"我被授权用什么"）
    let hello = a.mint(
        MessageType::Presence,
        "ctrl",
        None,
        Authority::Agent,
        json!({
            "state": "online",
            "kind": "agent",
            "kind": "agent",
        }),
    );
    emit(&mut out, &hello);

    // 2. **问中控：我被授权用什么。**
    //
    // 注意问的不是"世界有什么"（那是把注册表交出去），而是"**我被授权用什么**" ——
    // 授权由中控签发（§2.3），工具定义来自**服务自己的 manifest**。
    // 这样"服务报家门"和"模型看到的工具"终于是同一份真相，
    // 而不是智能体自带的一份手写配置。
    let (cards, bound) = a.fetch_tools(&mut out, &rx);
    a.cards = cards;
    let (system, mut manifest) =
        prompt::compile(&a.args.instance, &persona, &a.cards, &a.args.fences, &a.args.sid);
    manifest.compiled_at = now_ms();
    a.system = system;
    a.manifest = manifest.clone();
    eprintln!(
        "[agent] 授权清单：{}（{} 个工具实例）",
        if bound.is_empty() { "（空 —— 什么都用不了）".into() } else { bound.join(", ") },
        a.cards.len()
    );

    // 3. 把编译好的那段报进日志（不变量二）
    // **不变量二的第一块拼图**：模型看到的那段系统提示必须进日志。
    // 它不是消息流里的任何一条，所以只能显式报给中控。
    let prompt_ev = a.mint(
        MessageType::Event,
        "ctrl",
        None,
        Authority::Agent,
        json!({
            "kind": "prompt_compiled",
            "manifest": a.manifest,
            "system": a.system,
        }),
    );
    emit(&mut out, &prompt_ev);

    // **恢复**：向中控要我上次的上下文。
    //
    // 为什么不自己去读日志：**日志的格式归中控**。智能体提的是一个语义请求
    // ——"给我我上次看到过的东西" —— 至于它是从 jsonl 拼出来还是别的方式，
    // 那是中控的实现细节。这样也不需要把日志格式的两份知识放在两个地方。
    if a.args.resume {
        // 统一走 `ask_ctrl` —— 它会把不是等它这条的消息**收进 inbox**，不丢。
        // （这里踩过一次：为了恢复专门写过一个只等自己那条回信的版本，
        //  结果它把这段时间到达的任务丢了 —— 主循环还没开始，没人接。）
        if let Some(data) = a.ask_ctrl(
            &mut out,
            &rx,
            json!({"op": "replay", "agent": a.args.instance}),
        ) {
            let restored = data
                .get("items")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let n = restored.len();
            for item in restored {
                let kind = item.get("kind").and_then(Value::as_str).unwrap_or("");
                let text = item.get("text").and_then(Value::as_str).unwrap_or("");
                if let Some(i) = rebuild_item(kind, text, item.get("data")) {
                    a.history.push(i);
                }
            }
            // 系统提示是**重新编译**出来的（它是冻住那段，同样的输入必须逐字节一样）；
            // 日志里那份应当与它一致 —— 不一致就说明编译不确定，那要立刻知道
            let same_system = data
                .get("system")
                .and_then(Value::as_str)
                .map(|s| s == a.system)
                .unwrap_or(true);
            eprintln!(
                "[agent] 恢复上下文：{} 项，系统提示{}一致，日志到 seq {}",
                n,
                if same_system { "" } else { "**不**" },
                data.get("to_seq").and_then(Value::as_u64).unwrap_or(0)
            );
            // **再补一条敕令**：上一次会话里那些工具实例**已经跟着进程走了**。
            //
            // 为什么非说不可（2026-09-20，崩在半路那条验收抓到的）：恢复回来的上下文里
            // 有"我 open 了 `ed-1`、我 edit 过它"这些事实，**却没有"它后来没了"** ——
            // 那条敕令当时发给了**上一个进程**，而那个进程已经死了。
            // 于是新生的它张口就说"我仍持有 `ed-1` 的写权"（原话），而现实中
            // 实例早随上一辈子消失了。它下一句就会被 `instance_gone` 撞回来 ——
            // **撞回来能纠正，但那是撞出来的，不是被告知的**。
            //
            // 三段式（同所有敕令）：事实 / 影响 / 出口。
            a.note_item(
                &mut out,
                Item::new(
                    ItemKind::Injected,
                    "【敕令】上一次会话随进程一起结束了。\n\
                     - 事实：**不是常驻的那些服务里的工具实例都不在了**（它们是进程里的东西，\
                       跟着上一次会话一起没了；常驻服务——比如记忆——还在）。\n\
                     - 影响：**别拿恢复出来的旧 `instance_id` 直接操作** —— 它们会回 \
                       `instance_gone`（你手里的那份内容也从内存里没了）。\n\
                     - 出口：要用就**重新 `open`**，它会给你一个新的实例；\
                       已经落盘的改动**不受影响**（那是用户文件里的事）。",
                ),
            );
            let ev = a.mint(
                MessageType::Event,
                "ctrl",
                None,
                Authority::Agent,
                json!({
                    "kind": "context_restored",
                    "items": n,
                    "system_bytes": a.system.len(),
                    "system_matches": same_system,
                    "to_seq": data.get("to_seq"),
                }),
            );
            emit(&mut out, &ev);
        } else {
            eprintln!("[agent] 说了要恢复，但没从中控那儿拿到上下文 —— 空着开始");
        }
    }

    loop {
        // 启动握手时攒下的消息**先处理**
        if let Some(line) = a.inbox.pop_front() {
            a.on_line(&mut out, &rx, &line);
            continue;
        }
        match rx.recv_timeout(TICK) {
            Ok(Inbound::Line(line)) => a.on_line(&mut out, &rx, &line),
            Ok(Inbound::Eof) => break,
            Err(RecvTimeoutError::Timeout) => {
                // 有没有调用等超时了 —— 有就诚实收尾，别让回合永远挂着
                    a.reap_timeouts(&mut out);
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    eprintln!("[agent] 收摊");
}

impl Agent {
    /// 问中控一件事，等它回（有上限，等不到就空着开始，别把启动挂死）。
    fn ask_ctrl(
        &mut self,
        out: &mut impl Write,
        rx: &Receiver<Inbound>,
        payload: serde_json::Value,
    ) -> Option<Value> {
        let corr = self.ids.next_corr();
        let req = self.mint(
            MessageType::Request,
            "ctrl",
            Some(corr.clone()),
            Authority::Agent,
            payload,
        );
        emit(out, &req);
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            match rx.recv_timeout(TICK) {
                Ok(Inbound::Line(l)) => {
                    let env = match codec::decode(&l) {
                        Ok(e) => e,
                        Err(_) => continue,
                    };
                    if env.kind == MessageType::Result && env.corr.as_deref() == Some(&corr) {
                        return env.payload.get("data").cloned();
                    }
                    // **不是等我这条的，收好，别丢**
                    self.inbox.push_back(l);
                }
                Ok(Inbound::Eof) => return None,
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return None,
            }
        }
        None
    }

    /// **问中控要工具**：不是"世界有什么"，是"我被授权用什么"。
    ///
    /// **要重试**：中控和智能体是同时被拉起来的，工具服务往往还没自报 manifest。
    /// 早问一次就只好空手开始 —— 那模型面前一个工具都没有，
    /// 而这个"失败"看起来像配置问题，其实是**启动顺序**问题。
    fn fetch_tools(
        &mut self,
        out: &mut impl Write,
        rx: &Receiver<Inbound>,
    ) -> (Vec<ToolCard>, Vec<String>) {
        let deadline = Instant::now() + Duration::from_secs(6);
        loop {
            let Some(data) = self.ask_ctrl(out, rx, json!({"op": "my_tools"})) else {
                eprintln!("[agent] 没从中控拿到授权清单 —— 空着开始（什么都用不了）");
                return (Vec::new(), Vec::new());
            };
            let bound: Vec<String> = data
                .get("bound")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
                .unwrap_or_default();
            let (cards, waiting) = cards_from(&data);
            let ready = waiting.is_empty();
            if ready || Instant::now() >= deadline {
                if !ready {
                    eprintln!(
                        "[agent] 等不着这些工具自报家门：{}（可能它们没起来）",
                        waiting.join(", ")
                    );
                }
                for id in data
                    .get("not_alive")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default()
                {
                    eprintln!(
                        "[agent] 授权清单里的 `{}` 现在不活着",
                        id.as_str().unwrap_or("?")
                    );
                }
                // **顺手记下"我手里这几张凭证"** —— 回合边界要靠它比出"只有凭证变了"。
                self.caps_sig = caps_signature(&data);
                return (cards, bound);
            }
            std::thread::sleep(Duration::from_millis(150));
        }
    }

    /// **回合边界：扫一遍"我现在被授权用什么"。**
    ///
    /// 这是你定的那个做法 —— "不在中途插拔也行，就是在用户发信前扫一遍可用工具，
    /// 变了发敕令，没变就正常"。变了就**换名册**：function 定义数组整个换掉。
    ///
    /// **为什么必须换名册**：模型只认识 `tools` 数组里的东西。光发一条敕令
    /// 告诉它"你现在有 X 了"，原生 function calling 那边它照样调不出来 ——
    /// 这一条你在讨论里自己问过："工具只是在敕令中声明而为了缓存不加进 `tools`，
    /// 模型就根本不认识这个工具？" 对，就是这样。
    ///
    /// 代价是缓存：名册变了，**它之后**的那截全作废。你当时的判断是
    /// "反正都是炸缓存，不如把新东西放少点" —— 所以这里**只动名册**，
    /// 系统提示一个字都不重写（§15.2：拼装冻结，变化走 append-only）。
    ///
    /// 换名册这件事本身也进日志：**模型可见即已记录**（不变量二）。
    fn refresh_tools(&mut self, out: &mut impl Write, rx: &Receiver<Inbound>) {
        // **先排空攒着的消息**（握手/上一轮 `ask_ctrl` 期间到的）。
        // 敕令往往就在里面 —— 而它必须在"用户这条任务"之前进上下文：
        // 它是"世界变了"，不是"有人说了什么"。
        self.drain_inbox(out, rx);
        let Some(data) = self.ask_ctrl(out, rx, json!({"op": "my_tools"})) else {
            // 问不到就**保持原样**：宁可名册旧一点，也不要空着开工
            eprintln!("[agent] 这一轮没问到授权清单，沿用上一份名册");
            return;
        };
        let (cards, waiting) = cards_from(&data);
        if !waiting.is_empty() {
            eprintln!(
                "[agent] 这些实例还没自报 manifest，这一轮先不进名册：{}",
                waiting.join(", ")
            );
        }
        // **两个变化源分别比**（2026-09-20）：名册（有哪些实例、各有哪几个工具）
        // 和**凭证**（每个实例放我走哪几条动作）。它们能单独变 —— 只比前者的话，
        // "把 term:1 从通配改签成只给 `proc.read`"会被当成"什么都没发生"。
        //
        // ⚠️ **名册没变时不许换名册**：换 `tools` 数组 = 后面的缓存全作废（§5.4）。
        // 凭证变了照样要对模型说 —— 但说话的渠道是**中控那条敕令**（grant/revoke
        // 的时候它已经把能力名列出来了），这里只是**把账记上**，别重复喊一遍。
        let tools_same = toolset_signature(&self.cards) == toolset_signature(&cards);
        let caps_same = self.caps_sig == caps_signature(&data);
        if tools_same && caps_same {
            return;
        }
        if tools_same {
            // 只有凭证变了：名册一个字都不动，但这件事要留痕（可审计"它当时凭什么能用"）
            let after_caps = caps_signature(&data);
            eprintln!("[agent] 凭证变了（名册没变）：{:?} → {:?}", self.caps_sig, after_caps);
            self.caps_sig = after_caps.clone();
            let ev = self.mint(
                MessageType::Event,
                "ctrl",
                None,
                Authority::Agent,
                json!({
                    "kind": "caps_changed",
                    "caps": after_caps.iter().map(|(i, c)| json!({"instance": i, "capabilities": c})).collect::<Vec<_>>(),
                    "why": "回合开始前扫了一遍授权：**名册没变、凭证变了** —— \
                            function 定义数组不动（省一次缓存作废），但该记的账要记（§7、§4.4）",
                }),
            );
            emit(out, &ev);
            self.drain_inbox(out, rx);
            return;
        }
        let before: Vec<String> = self.cards.iter().map(|c| c.instance.clone()).collect();
        let after: Vec<String> = cards.iter().map(|c| c.instance.clone()).collect();
        eprintln!("[agent] 名册变了：{before:?} → {after:?}");
        self.cards = cards;
        self.caps_sig = caps_signature(&data);
        let listing: Vec<Value> = self
            .cards
            .iter()
            .map(|c| {
                json!({
                    "instance": c.instance,
                    "tools": c.tools.iter().map(|t| t.name.clone()).collect::<Vec<_>>(),
                })
            })
            .collect();
        let ev = self.mint(
            MessageType::Event,
            "ctrl",
            None,
            Authority::Agent,
            json!({
                "kind": "toolset_changed",
                "before": before,
                "after": after,
                "tools": listing,
                "why": "回合开始前扫了一遍授权（§4.4）—— 中控那边改过绑定；\
                        前缀不动，只换名册（§15.2）",
            }),
        );
        emit(out, &ev);
        // 这一趟往返里又到的消息（例如紧接着的那条敕令）也收进来，别留到下一轮
        self.drain_inbox(out, rx);
    }

    /// 把 `ask_ctrl` 期间攒下的消息按原顺序处理掉。
    fn drain_inbox(&mut self, out: &mut impl Write, rx: &Receiver<Inbound>) {
        while let Some(line) = self.inbox.pop_front() {
            self.on_line(out, rx, &line);
        }
    }

    /// **未决调用清空之后，把扣着的敕令按原顺序追进历史**（见 `held_injects`）。
    fn flush_held_injects(&mut self, out: &mut impl Write) {
        if !self.pending.is_empty() || self.held_injects.is_empty() {
            return;
        }
        for text in std::mem::take(&mut self.held_injects) {
            self.note_item(out, Item::new(ItemKind::Injected, text));
        }
    }

    /// 启动时同步地问一次中控："我上次看到过什么？"
    ///
    /// 这是**主循环之前**的一段小循环：发出去、然后等那条配对的结果回来
    /// （有上限，等不到就空着开始，别把启动挂死）。
    /// **往上下文里放一样东西，同时把它报进会话日志。**
    ///
    /// 这就是不变量二的落地：**模型可见即已记录**（规范 §4）。
    /// 在它之前，智能体往上下文里放了六种东西，日志里只有系统提示那一种 ——
    /// 也就是说**日志根本重建不出模型看到的东西**，而"跨进程恢复"正需要那个。
    ///
    /// 为什么不"从协议消息里推"而要单独报：**渲染是有损耗的**。
    /// 工具结果在日志里是 `result` 报文的原样，而模型看到的是
    /// `render_result` 加工过的那份（挑出 output、追加【错误码】【下一步】）。
    /// 只有智能体自己知道它到底喂了什么 —— 所以只能由它报。
    fn note_item(&mut self, out: &mut impl Write, item: Item) {
        let payload = json!({
            "kind": "model_item",
            "item": {"kind": item.kind.as_str(), "text": item.text, "data": item.data},
        });
        let ev = self.mint(
            MessageType::Event,
            "ctrl",
            None,
            Authority::Agent,
            payload,
        );
        emit(out, &ev);
        self.history.push(item);
    }

    fn mint(
        &mut self,
        kind: MessageType,
        to: &str,
        corr: Option<String>,
        authority: Authority,
        payload: Value,
    ) -> Envelope {
        Envelope {
            id: self.ids.next_message(),
            ts: now_ms(),
            kind,
            from: self.args.instance.clone(),
            to: to.into(),
            corr,
            sid: self.args.sid.clone(),
            authority,
            payload,
        }
    }

    fn on_line(&mut self, out: &mut impl Write, rx: &Receiver<Inbound>, line: &str) {
        let env = match codec::decode(line) {
            Ok(e) => e,
            Err(e) => {
                eprintln!("[agent] 收到不合规的消息：{e}");
                return;
            }
        };
        match env.kind {
            MessageType::Request => {
                if self.busy {
                    let reply = self.mint(
                        MessageType::Result,
                        &env.from.clone(),
                        env.corr.clone(),
                        Authority::Agent,
                        json!({
                            "effect": "refused", "code": "busy",
                            "why": "这个智能体正在跑上一个回合",
                            "remedy": "等它给出结论再来；或者再起一个智能体实例"
                        }),
                    );
                    emit(out, &reply);
                    return;
                }
                let task = env
                    .payload
                    .get("task")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                self.run_turn(out, rx, &env, task);
            }
            MessageType::Result => {
                if let Some(corr) = env.corr.clone() {
                    if let Some(p) = self.pending.remove(&corr) {
                        let text = render_result(&env.payload);
                        let item = Item::tool_result_with(&p.call.tool, text, &p.call_id);
                        self.note_item(out, item);
                        self.last_payload = Some(env.payload.clone());
                        self.flush_held_injects(out);
                    } else {
                        // 孤儿结果 —— 记进上下文会污染它，但不说是更糟
                        eprintln!("[agent] 收到一个对不上的结果（corr={corr}）");
                    }
                }
            }
            MessageType::Inject => {
                // **敕令**：中控往里塞的那条。它进上下文，但**不改前缀**（§4.4）
                let text = env
                    .payload
                    .get("text")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| env.payload.to_string());
                eprintln!("[agent] 收到敕令：{text}");
                self.trace.injects += 1;
                if self.pending.is_empty() {
                    let item = Item::new(ItemKind::Injected, text);
                    self.note_item(out, item);
                } else {
                    // **手上有未决调用 ⇒ 先扣着**：此刻追进去会插在"调用"和"结果"中间，
                    // 而 provider 要求那两条紧挨着（见 `held_injects` 那段）。
                    // 等这一批结果都落地，`flush_held_injects` 再按原顺序追。
                    eprintln!("[agent] 手上还有 {} 个未决调用 —— 这条敕令先扣着，等结果落地再追",
                              self.pending.len());
                    self.held_injects.push(text);
                }
            }
            // **兜底：带 corr 的 `event`**。
            //
            // 原来这条是**主路** —— 中控那时把拒绝放在 `event` 里发，
            // 于是没有这条分支智能体会一直等一个永远不来的结果（干等 30 秒）。
            // 2026-09-19 把协议那头改对了：**回答请求走 `result`**（规范 §3），
            // 拒绝也是回答（`harness-ctrl::refuse_call`）。
            // 这条留着当兜底：**别的实现不一定守规矩，而它能安全地收下**。
            MessageType::Event => {
                let Some(corr) = env.corr.clone() else {
                    return;
                };
                let Some(p) = self.pending.remove(&corr) else {
                    return;
                };
                let text = render_result(&env.payload);
                let item = Item::tool_result_with(&p.call.tool, text, &p.call_id);
                self.note_item(out, item);
                self.last_payload = Some(env.payload.clone());
                self.flush_held_injects(out);
            }
            _ => {}
        }
    }

    /// 一个回合：拿任务 → 反复（调模型 → 调工具 → 收结果）→ 出结论。
    fn run_turn(&mut self, out: &mut impl Write, rx: &Receiver<Inbound>, req: &Envelope, task: String) {
        self.busy = true;
        self.run_turn_inner(out, rx, req, task);
        self.busy = false;
    }

    fn run_turn_inner(
        &mut self,
        out: &mut impl Write,
        rx: &Receiver<Inbound>,
        req: &Envelope,
        task: String,
    ) {
        // **回合边界：先扫一遍授权**（§4.4）。变了就换名册 —— 前缀不动。
        // 放在"记下用户这条任务"之前：敕令进上下文，才是模型这一轮看到的顺序
        // （先知道工具集变了，再接到活）。
        self.refresh_tools(out, rx);
        self.note_item(out, Item::user(task));
        let mut steps = 0usize;
        let mut failure: Option<(String, String, String)> = None; // (效果, 码, 补救)

        loop {
            steps += 1;
            self.trace.steps = steps;
            if steps > self.args.max_steps {
                failure = Some((
                    "partial".into(),
                    "timeout".into(),
                    format!("这个回合超过 {} 步还没收敛，我停了 —— 检查脚本或任务是不是绕圈", self.args.max_steps),
                ));
                break;
            }
            self.maybe_compact(out);

            let model_req = ModelRequest {
                system: self.system.clone(),
                items: self.history.items().to_vec(),
                tools: self.cards.clone(),
            };
            // **落盘屏障：第三个边界 —— 模型请求前**（规范-消息协议 §5.5 #8）。
            //
            // 为什么归智能体来喊这一声：会话日志是**中控独占**的，智能体手里没有那支笔，
            // 但它比谁都清楚"我下一步要去问模型了"。所以**边界归它判，动作归中控做**
            // （发 `{"op":"barrier"}`，中控那边 flush + fsync）。
            //
            // 它防的是什么：模型是在**日志之外**跑的一次远程调用，可"模型见过什么"
            // 是要从日志里重建的（不变量二）。日志还压在写缓冲里就去问模型 ——
            // 崩在这中间，重放出来的历史就和模型当时看到的那份**不是同一份**。
            //
            // **落不成不拦路**，只出声：这是"审计质量"的护栏，不是"这一步能不能做"的门。
            // 拦下来只会把一次本来能干的活变成做不了，代价比"少一条落盘凭证"更大。
            // 诚实的地方在于：**落没落这件事是看得见的**（`synced` 回来是真是假，
            // 中控那边也留一行），而不是被吞掉。
            match self.ask_ctrl(out, rx, json!({"op": "barrier", "reason": "模型请求前"})) {
                Some(d) if d.get("synced").and_then(Value::as_bool) == Some(true) => {}
                Some(_) => {
                    // 日志本来就干净（缓冲区是空的）—— 这一下没 fsync，但不是错
                }
                None => eprintln!(
                    "[agent] 模型请求前那一屏障没落地：中控没回话。\
                     这一回合照跑 —— 但**这一段历史没有落盘凭证**，出问题时会看不全"
                ),
            }
            let reply = self.model.reply(&model_req);
            // 模型这条路本身断了 ⇒ **不能当成"它的回答"**（那是把故障说成结论）
            if let Some(why) = reply.error {
                failure = Some(("unverifiable".into(), "service_down".into(), why));
                break;
            }
            // **真值校准**：provider 报回来的输入 token 才是基线（§5.4.3 ②）
            self.window.calibrate(reply.usage_input_tokens);
            self.trace.tokens = reply.usage_input_tokens;
            self.trace.window = self.window.number;

            if reply.calls.is_empty() {
                let text = reply.text.unwrap_or_default();
                self.note_item(out, Item::assistant(&text));
                let payload = self.done_payload("confirmed", Value::Null, &text, None);
                let done = self.mint(
                    MessageType::Result,
                    &req.from.clone(),
                    req.corr.clone(),
                    Authority::Agent,
                    payload,
                );
                emit(out, &done);
                return;
            }

            for call in reply.calls {
                // ⚠️ **这里原来先写了一条 `assistant("[调用] name(args)")`** —— 理由是
                // "下一轮模型才看得见自己调过什么"。那是**多余的**（模型从下面那些
                // `tool_call` 条目里就能看见自己的调用，适配器会把它们渲染成
                // 带 `tool_calls` 的 assistant 消息），而且**有害**：
                // 那条文本会被**回放给模型当上文**，于是模型学到"动手的方式是写
                // `[调用] name(args)` 这行文本" —— 实跑里它第三轮就真的只写了文本、
                // 没有发出调用（记忆那条组合验收抓到的）。
                // **教训**：拼给模型看的字，模型会照着学。
                let corr = self.ids.next_corr();
                // provider 给了 id 就用它（真模型必须原样带回去）；
                // mock 不给，就用我们的 corr 当 id —— 两者都能配对。
                let call_id = call.id.clone().unwrap_or_else(|| corr.clone());
                // **参数都没解出来的调用：根本不许发出去。**（2026-09-20）
                //
                // 为什么：解不开的时候我们**不知道模型想调什么**。以前那种做法
                // （编一个 `{_raw,_parse_error}` 发过去）有三个坏处：
                // ① 等于替模型编了它没说的话；② 工具会回一句"缺少参数"，
                // **指不到真正的原因**；③ 那一轮就成了"一次没人知道想干什么的调用"——
                // 而它**本来是能救回来的**。
                //
                // 现在的做法是**报错即手册**：把它的原话 + 这个工具的正确参数形状
                // **一起**回给它，让它改一版再来。**这条调用没有发生**，所以
                // 不进 `pending`、不发请求、也不算一次工具调用。
                if let Some(why) = call.parse_error.clone() {
                    self.trace.bad_calls += 1;
                    let manual = self.manual_for_bad_call(&call, &why);
                    // **先把它那条调用原样记下来**（2026-09-20 修，整机实跑抓到的）：
                    // provider 有一条硬要求 —— **`tool` 消息必须跟在一个带 `tool_calls`
                    // 的 assistant 消息后面**。只回 tool_result 不记那条调用，
                    // 下一个请求会被直接拒（HTTP 400：`Messages with role 'tool'
                    // must be a response to a preceding message with 'tool_calls'`）。
                    // 既然是回放，名字和参数都用**模型的原话**。
                    let echo = Item::tool_call_echo(
                        &call.to,
                        &call.tool,
                        call.raw_name.as_deref().unwrap_or(&call.tool),
                        call.args_raw.as_deref(),
                        &call_id,
                    );
                    self.note_item(out, echo);
                    // 然后再回一条 `tool_result` 挂同一个 call_id：provider 那边
                    // 「assistant 的 tool_calls 必须有对应的 tool 回信」，少一条就是协议违规
                    self.note_item(out, Item::tool_result_with(&call.tool, &manual, &call_id));
                    eprintln!(
                        "[agent] 这一条调用不成立（{why}）—— \
                         它**没有发出去**，回了它一条报错即手册"
                    );
                    continue;
                }
                self.trace.tool_calls += 1;
                let item = Item::tool_call_with(&call.to, &call.tool, &call.args, &call_id);
                self.note_item(out, item);
                self.pending.insert(
                    corr.clone(),
                    Pending {
                        call: call.clone(),
                        call_id,
                        started: Instant::now(),
                    },
                );
                let env = self.mint(
                    MessageType::Request,
                    &call.to.clone(),
                    Some(corr),
                    Authority::Agent,
                    json!({
                        "tool": call.tool,
                        // 模板替换：让 mock 脚本能引用上一次结果里的字段。
                        // 真模型是自己读结果再决定；mock 只能靠这个把同样的行为演出来。
                        "args": resolve_args(&call.args, self.last_payload.as_ref()),
                    }),
                );
                emit(out, &env);
            }

            // 等它们回来 —— **期间敕令照样要收**（工具崩了就是靠它告知的）
            let deadline = Instant::now() + self.args.call_timeout;
            while !self.pending.is_empty() {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    // **尽力而为地取消**（§2.3）。在这之前超时只是"我不等了"，
                    // 工具那边还在跑 —— 现在至少告诉它一声。
                    let waiting: Vec<(String, String)> = self
                        .pending
                        .iter()
                        .map(|(c, p)| (c.clone(), p.call.to.clone()))
                        .collect();
                    for (corr, to) in &waiting {
                        let env = self.mint(
                            MessageType::Cancel,
                            to,
                            Some(corr.clone()),
                            Authority::Agent,
                            json!({}),
                        );
                        emit(out, &env);
                    }
                    eprintln!("[agent] 超时：给 {} 个调用发了 cancel", waiting.len());
                    // 给它们一小段反应时间 —— **取消了也算一个答复**
                    let grace = Instant::now() + Duration::from_secs(2);
                    while !self.pending.is_empty() && Instant::now() < grace {
                        match rx.recv_timeout(TICK) {
                            Ok(Inbound::Line(l)) => self.on_line(out, rx, &l),
                            Ok(Inbound::Eof) => break,
                            Err(RecvTimeoutError::Timeout) => {}
                            Err(RecvTimeoutError::Disconnected) => break,
                        }
                    }
                    if !self.pending.is_empty() {
                        failure = Some((
                            "unverifiable".into(),
                            "timeout".into(),
                            format!(
                                "有 {} 个调用一直没回来，我发过 cancel 也没回音 —— \
                                 所以我不知道它们做没做",
                                self.pending.len()
                            ),
                        ));
                        break;
                    }
                    // 它们都答复了（多半是 partial + canceled）——
                    // **别替模型下结论**，下一轮它自己看着办
                    continue;
                }
                match rx.recv_timeout(left.min(TICK)) {
                    Ok(Inbound::Line(l)) => self.on_line(out, rx, &l),
                    Ok(Inbound::Eof) => {
                        failure = Some((
                            "unverifiable".into(),
                            "service_down".into(),
                            "中控那边断了，这个回合没法收尾".into(),
                        ));
                        break;
                    }
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => {
                        failure = Some((
                            "unverifiable".into(),
                            "service_down".into(),
                            "通道断了".into(),
                        ));
                        break;
                    }
                }
            }
            if failure.is_some() {
                break;
            }
        }

        let (effect, code, remedy) = failure.unwrap_or((
            "partial".into(),
            "invalid".into(),
            "回合没走完".into(),
        ));
        let text = format!("（这个回合没收尾：{remedy}）");
        let payload = self.done_payload(&effect, json!(code), &text, Some(&remedy));
        let done = self.mint(
            MessageType::Result,
            &req.from.clone(),
            req.corr.clone(),
            Authority::Agent,
            payload,
        );
        emit(out, &done);
    }

    fn done_payload(&self, effect: &str, code: Value, text: &str, remedy: Option<&String>) -> Value {
        let mut p = json!({
            "effect": effect,
            "code": code,
            "text": text,
            "trace": self.trace,
        });
        if let Some(r) = remedy {
            p["remedy"] = json!(r);
        }
        if let Some(c) = self.hooks.records.last() {
            p["last_compaction"] = json!({"window_after": c.window_after, "turns": c.turns_compacted});
        }
        p
    }

    /// **模型的参数没解出来**时回它的那一条 —— **报错即手册**（§5.5）。
    fn summarize_with(&self) -> SummarizeWith {
        summarize_choice(&self.args)
    }

    ///
    /// 这是"我在两个项目里都干过"那个做法的落地：**一边报错，一边把正确格式给它**。
    /// 三件事必须齐（少一件这条就成了抱怨）：
    ///
    /// 1. **事实**：它给的原话是什么、哪儿不对；
    /// 2. **影响**：**这次调用没有发生**（工具那边根本不知道它调过）；
    /// 3. **出口**：这个工具**正确的参数形状**长什么样 —— 参数表我们手里就有
    ///    （名片里那份 `input_schema` 就是从服务的 manifest 抄来的），
    ///    所以"正确格式"不是我们编的，**是抄的**。
    ///
    /// 拿不准还有一条退路：让它先调那个工具的 `help`（§5.3 渐进披露）。
    fn manual_for_bad_call(&self, call: &ToolCall, why: &str) -> String {
        let mut out = format!(
            "**[参数不合法]** 你给 `{}` 的参数{}。\n",
            call.tool, why
        );
        out.push_str(
            "- **这次调用没有发生**：工具那边根本不知道你调过它 —— \
             别以为它做了一半。\n",
        );
        // 参数表：**从名片里抄**，不是这里编的
        let schema = self
            .cards
            .iter()
            .flat_map(|c| c.tools.iter())
            .find(|t| t.name == call.tool)
            .map(|t| {
                let s = t.input_schema.to_string();
                if s.chars().count() > 900 {
                    let head: String = s.chars().take(900).collect();
                    format!("{head}…（太长，截断了）")
                } else {
                    s
                }
            });
        out.push_str(&format!(
            "- **`{}` 的参数长这样**：{}\n",
            call.tool,
            schema.unwrap_or_else(|| {
                // 找不到那张名片：**照实说找不到**，别编一个形状
                "（我手里没有它的参数表 —— 它的服务没在 manifest 里报，或者它不在你的名册上）"
                    .to_string()
            })
        ));
        out.push_str(
            "- 拿不准就**先调它的 `help`**（渐进披露那条：别猜参数）—— \
             一轮只改一次，把参数写成一个**合法 JSON 对象**再来。\n\
             - **别原样重发**：解不出来的那一版再发一次，结果还是这个。",
        );
        out
    }

    /// 每轮前问一句"该压了吗" —— **回合中压缩，摘要贴到最后一条 user 之前**（§5.4.3 ④）。
    fn maybe_compact(&mut self, out: &mut impl Write) {
        let counter = HeuristicCounter::new();
        // **谁来摘要**（待定 #103）：真 provider 就用真模型，mock 脚本就用确定性那条。
        // 判定写在 `summarize_with()` 一个地方 —— 两处各判一次迟早会不一致。
        let with_model = self.summarize_with() == SummarizeWith::Model;
        let real = with_model.then(|| ModelSummarizer::new(build_model(&self.args)));
        let mock = MockSummarizer::default();
        let summarizer: &dyn harness_memory::summarize::Summarizer = match &real {
            Some(s) => s,
            None => &mock,
        };
        let compactor = Compactor {
            policy: self.policy.clone(),
            counter: &counter,
            summarizer,
            session: self.args.sid.clone(),
        };
        // **算的是整个请求**（系统提示 + 历史），不是只有历史 ——
        // 而系统提示现在装着一整张工具名片，它可能比历史还大。
        let used = counter.count_text(&self.system) + self.history.tokens(&counter);
        if compactor.should_compact_with(used, &self.window).is_none() {
            return;
        }
        let before = self.history.tokens(&counter);
        if let Some(r) = compactor.compact(
            &mut self.history,
            &mut self.window,
            When::MidTurn,
            Reason::ContextLimit,
            &mut self.hooks,
        ) {
            eprintln!(
                "[agent] {} → {} token，窗口 {}→{}",
                before, r.tokens_after, r.window_before, r.window_after
            );
            // **压缩也要进日志** —— 否则重放没法把同一刀切回去，
            // 而"切在哪、摘要是什么"只有智能体自己知道。
            let payload = json!({
                "kind": "compaction",
                "cut": r.cut,
                "summary_at": r.summary_at,
                "summary": self.history.previous_summary(),
                // **摘要这件事是谁干的**（2026-09-20，待定 #103）：
                // 只写"压过了"不够 —— 判词要能回答"这一刀是模型切的还是确定性切的、
                // 那次调用花了多少 token、有没有失败退回来"。
                // 摘要**正文**在上面那行（`summary`）里 —— 它必须进日志，
                // 因为它是**模型接下来会看到**的东西（不变量二）。
                "summarizer": summarizer.name(),
                "summarizer_tokens_in": real
                    .as_ref()
                    .map(|s| s.last.borrow().tokens_in)
                    .unwrap_or(0),
                "summarizer_fell_back": real
                    .as_ref()
                    .and_then(|s| s.last.borrow().fell_back.clone()),
                "summary_chars": self
                    .history
                    .previous_summary()
                    .map(|s| s.chars().count())
                    .unwrap_or(0),
                // **这一刀切在回合内部吗**（#105）：它决定"模型接下来看到什么"的形状 ——
                // 轮边界那刀是"摘要 + 若干完整轮次"，回合内部那刀是
                // "摘要 + 同一个回合的后半截"。排查"它为什么忘了刚说过的话"时要分得开。
                "mid_turn": r.mid_turn,
                // 配对没成立的话原因在这儿（成立就是 null）—— 护栏不该变成杀手，
                // 但**必须留下痕迹**
                "pairing_note": r.pairing_note,
                "reason": r.reason,
                "window_before": r.window_before,
                "window_after": r.window_after,
                "turns_compacted": r.turns_compacted,
                "tokens_before": r.tokens_before,
                "tokens_after": r.tokens_after,
            });
            let ev = self.mint(
                MessageType::Event,
                "ctrl",
                None,
                Authority::Agent,
                payload,
            );
            emit(out, &ev);
            self.trace.compactions += 1;
            self.trace.window = self.window.number;
        }
    }

    /// 等超时的调用：**不能静默丢** —— 给调用方一个诚实的 `unverifiable`。
    fn reap_timeouts(&mut self, out: &mut impl Write) {
        let now = Instant::now();
        let expired: Vec<String> = self
            .pending
            .iter()
            .filter(|(_, p)| now.duration_since(p.started) > self.args.call_timeout)
            .map(|(c, _)| c.clone())
            .collect();
        for corr in expired {
            if let Some(p) = self.pending.remove(&corr) {
                eprintln!("[agent] 调用 {} 超时（{}）", p.call.tool, corr);
                // ⚠️ **必须带上 `call_id`**（2026-09-20 修）：原来这条结果没带 id，
                // 于是历史里是「带 id 的调用 + 没 id 的结果」—— 个数是平的，
                // 可真 provider 那边「assistant 的 tool_calls 必须有对应回信」
                // 会因此**拒绝整条请求**（下一次模型调用直接 400）。
                // 抓到它的是把 `assert_safe` 升级成"按 `call_id` 配对"之后。
                let item = Item::tool_result_with(
                    &p.call.tool,
                    format!("（超时：{} 一直没回，我不知道它做没做）", p.call.to),
                    &p.call_id,
                );
                self.note_item(out, item);
            }
        }
    }
}

/// 工具结果**给模型看的那份**。
///
/// 注意这里不是把协议 JSON 原样丢过去：`output` / `text` 优先，
/// 拿不到才退化成整份 payload。
/// （完整的"表示形式"是规范 §7 那件事，这里先做最小版。）
fn render_result(payload: &Value) -> String {
    for key in ["output", "text"] {
        if let Some(s) = payload.get(key).and_then(Value::as_str) {
            let mut out = s.to_string();
            if let Some(code) = payload.get("code").and_then(Value::as_str) {
                out.push_str(&format!("\n[错误码] {code}"));
            }
            // **"为什么"不能丢。** 有 output 的结果也可能是失败了一半的
            // （`partial` + `canceled` / `timeout`）—— 那时 output 是"做到哪了"，
            // 而 why 才是"为什么会停在这儿"。只留前者，模型会以为自己看完了。
            if let Some(why) = payload.get("why").and_then(Value::as_str) {
                out.push_str(&format!("\n[为什么] {why}"));
            }
            if let Some(r) = payload.get("remedy").and_then(Value::as_str) {
                out.push_str(&format!("\n[下一步] {r}"));
            }
            return out;
        }
    }
    match payload.get("code").and_then(Value::as_str) {
        Some(code) => {
            let why = payload.get("why").and_then(Value::as_str).unwrap_or("");
            let remedy = payload.get("remedy").and_then(Value::as_str).unwrap_or("");
            format!("[失败 {code}] {why}\n[下一步] {remedy}")
        }
        None => payload.to_string(),
    }
}

/// 把中控回放出来的一条（JSON）变回上下文里的一项。
///
/// 认不出来就**跳过并说话** —— 回放遇到不认识的东西时静默吞掉，
/// 就等于"恢复了个寂寞"。
fn rebuild_item(kind: &str, text: &str, data: Option<&Value>) -> Option<Item> {
    let k = match kind {
        "user" => ItemKind::User,
        "assistant" => ItemKind::Assistant,
        "injected" => ItemKind::Injected,
        "summary" => ItemKind::Summary,
        "tool_call" => ItemKind::ToolCall {
            name: tool_name(data),
        },
        "tool_result" => ItemKind::ToolResult {
            name: tool_name(data),
        },
        other => {
            eprintln!("[agent] 恢复时遇到不认识的 item 种类 `{other}`，跳过");
            return None;
        }
    };
    let mut item = Item::new(k, text);
    item.data = data.cloned().filter(|d| !d.is_null());
    Some(item)
}

fn tool_name(data: Option<&Value>) -> String {
    data.and_then(|d| d.get("name"))
        .and_then(Value::as_str)
        .unwrap_or("?")
        .to_string()
}

/// mock 脚本里的 `"$last.<字段>"` 模板。
///
/// **为什么要有它**：真模型是"读了上一步的结果，再决定下一步"，mock 想演同一个行为
/// 就得有个办法引用上一次的结果。没有它，脚本只能把 `t-1` 这种 id 写死 ——
/// 那测的就不是循环，是巧合。
///
/// 只支持一层字段（`$last.instance_id`），因为再多就该上真模型了。
fn resolve_args(args: &Value, last: Option<&Value>) -> Value {
    match args {
        Value::String(s) => {
            if let Some(field) = s.strip_prefix("$last.") {
                return last
                    .and_then(|l| l.get(field))
                    .cloned()
                    .unwrap_or_else(|| Value::String(format!("<$last.{field} 取不到>")));
            }
            args.clone()
        }
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|i| resolve_args(i, last))
                .collect::<Vec<_>>(),
        ),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), resolve_args(v, last)))
                .collect(),
        ),
        other => other.clone(),
    }
}

fn emit(out: &mut impl Write, env: &Envelope) {
    match codec::encode(env) {
        Ok(line) => {
            if let Err(e) = codec::write_line(out, &line) {
                eprintln!("[agent] 写 stdout 失败：{e}");
            }
        }
        Err(e) => eprintln!("[agent] 自己的消息编码失败：{e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // 只有测试里要造"失败的回包"和"空话的回包" —— 所以它在测试模块里引入
    use harness_agent::model::ModelReply;

    /// 一个**永远失败**的模型 —— 用来验"摘要模型挂了会怎样"。
    struct Failing;
    impl Model for Failing {
        fn name(&self) -> &'static str {
            "failing"
        }
        fn reply(&mut self, _req: &ModelRequest) -> ModelReply {
            ModelReply::failed("网络断了")
        }
    }

    /// 一个**回空话**的模型 —— 那也是"模型听不听话"的问题，不是系统坏了。
    struct Silent;
    impl Model for Silent {
        fn name(&self) -> &'static str {
            "silent"
        }
        fn reply(&mut self, _req: &ModelRequest) -> ModelReply {
            ModelReply::text("   ", 7)
        }
    }

    /// **摘要模型连不上时不把压缩整个取消，但也绝不装作那是它写的。**
    #[test]
    fn a_failing_summarizer_falls_back_and_says_so() {
        use harness_memory::summarize::Summarizer;
        let s = ModelSummarizer::new(Box::new(Failing));
        let out = s.summarize(&harness_memory::summarize::SummaryInput::new("一段记录"));
        assert!(!out.trim().is_empty(), "退回确定性那条也得有东西可用");
        let t = s.last.borrow();
        assert!(t.fell_back.as_deref().unwrap_or("").contains("网络"), "{t:?}");
        assert_eq!(t.tokens_in, 0);
        assert_eq!(s.name(), "model");
    }

    #[test]
    fn an_empty_summary_also_falls_back() {
        use harness_memory::summarize::Summarizer;
        let s = ModelSummarizer::new(Box::new(Silent));
        let out = s.summarize(&harness_memory::summarize::SummaryInput::new("一段记录"));
        assert!(!out.trim().is_empty());
        assert!(s.last.borrow().fell_back.is_some(), "空摘要要留下原因");
    }

    /// **默认按模型类型定**：真 provider 用真模型，脚本（测试工具）用确定性的那条。
    #[test]
    fn the_default_summarizer_follows_the_model_kind() {
        // 走真正的参数解析（`parse_args_from` 就是为了让它可测才抽出来的）
        let mut a = parse_args_from(
            ["--model", "deepseek"].iter().map(|s| s.to_string()),
        );
        assert_eq!(
            summarize_choice(&a),
            SummarizeWith::Model,
            "真 provider 该用模型摘要"
        );
        a.model = "mock".into();
        assert_eq!(summarize_choice(&a), SummarizeWith::Mock);
        // 显式给了就听显式的（哪怕和默认相反）
        a.summarize = Some(SummarizeWith::Model);
        assert_eq!(summarize_choice(&a), SummarizeWith::Model);
    }

    fn card_of(instance: &str, tools: &[&str]) -> ToolCard {
        ToolCard {
            instance: instance.into(),
            title: instance.into(),
            summary: String::new(),
            when_to_use: String::new(),
            injection_policy: "card".into(),
            help_sections: Vec::new(),
            destructive: false,
            tools: tools
                .iter()
                .map(|t| prompt::ToolDef {
                    name: (*t).into(),
                    title: String::new(),
                    description: String::new(),
                    input_schema: json!({"type": "object", "properties": {}}),
                })
                .collect(),
        }
    }

    /// 名册指纹要认得"**同一个实例多了一个工具**"这件事 ——
    /// 只比实例 id 会漏掉它，而那正是"服务改了 manifest 重新上线"的形状。
    #[test]
    fn a_new_tool_on_the_same_instance_changes_the_signature() {
        assert_ne!(
            toolset_signature(&[card_of("term:1", &["run"])]),
            toolset_signature(&[card_of("term:1", &["run", "read"])])
        );
    }

    /// 顺序不算变化 —— 服务报工具的先后不该白炸一次缓存。
    #[test]
    fn order_alone_is_not_a_change() {
        assert_eq!(
            toolset_signature(&[card_of("term:1", &["run", "read"])]),
            toolset_signature(&[card_of("term:1", &["read", "run"])])
        );
        assert_eq!(
            toolset_signature(&[card_of("term:1", &[]), card_of("echo:1", &[])]),
            toolset_signature(&[card_of("echo:1", &[]), card_of("term:1", &[])])
        );
    }

    #[test]
    fn render_prefers_output_and_appends_the_manual() {
        let p = json!({"output": "ok\n", "code": "invalid", "remedy": "改用 create:true"});
        let r = render_result(&p);
        assert!(r.starts_with("ok"));
        assert!(r.contains("[错误码] invalid"));
        assert!(r.contains("[下一步] 改用 create:true"));
    }

    /// 报错也是**给模型看的文本** —— 这三个字段不能丢（§5.5 报错即手册）。
    #[test]
    fn render_keeps_the_three_parts_of_a_fault() {
        let p = json!({"code": "instance_gone", "why": "没有实例 t-9", "remedy": "先 open"});
        let r = render_result(&p);
        assert!(r.contains("instance_gone"));
        assert!(r.contains("没有实例 t-9"));
        assert!(r.contains("先 open"));
    }

    #[test]
    fn render_falls_back_to_the_whole_payload() {
        let p = json!({"weird": 1});
        assert_eq!(render_result(&p), p.to_string());
    }

    /// mock 也要"看着结果决定下一步" —— 否则脚本只能写死 id，测的是巧合。
    #[test]
    fn template_pulls_the_id_out_of_the_previous_result() {
        let last = json!({"instance_id": "t-1", "cwd": "D:/p"});
        let args = json!({"instance_id": "$last.instance_id", "cmd": "echo hi"});
        let r = resolve_args(&args, Some(&last));
        assert_eq!(r["instance_id"], "t-1");
        assert_eq!(r["cmd"], "echo hi", "不是模板的字段要原样留着");
    }

    #[test]
    fn template_that_cannot_be_resolved_says_so_instead_of_silently_passing() {
        let r = resolve_args(&json!({"x": "$last.nope"}), Some(&json!({})));
        assert!(r["x"].as_str().unwrap().contains("取不到"));
    }

    #[test]
    fn template_works_inside_nested_args() {
        let last = json!({"instance_id": "ed-7"});
        let args = json!({"nested": {"list": ["$last.instance_id"]}});
        assert_eq!(resolve_args(&args, Some(&last))["nested"]["list"][0], "ed-7");
    }
}
