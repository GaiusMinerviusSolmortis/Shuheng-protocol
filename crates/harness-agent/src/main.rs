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
    /// **恢复**：启动时向中控要回"我上次看到过的东西"
    resume: bool,
}

fn parse_args() -> Args {
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
    };
    let mut it = std::env::args().skip(1);
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
    /// 编译一次就冻住的那段前缀（§15.2）
    system: String,
    manifest: prompt::CompositionManifest,
    history: History,
    window: Window,
    policy: CompactionPolicy,
    hooks: TraceHooks,
    model: Box<dyn Model>,
    pending: HashMap<String, Pending>,
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
        history: History::new(),
        window: Window::first(&args.sid, args.model_window),
        policy: CompactionPolicy::default(),
        hooks: TraceHooks::default(),
        model: build_model(&args),
        pending: HashMap::new(),
        last_payload: None,
        busy: false,
        inbox: std::collections::VecDeque::new(),
        trace: Trace {
            steps: 0,
            tool_calls: 0,
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
        if toolset_signature(&self.cards) == toolset_signature(&cards) {
            return;
        }
        let before: Vec<String> = self.cards.iter().map(|c| c.instance.clone()).collect();
        let after: Vec<String> = cards.iter().map(|c| c.instance.clone()).collect();
        eprintln!("[agent] 名册变了：{before:?} → {after:?}");
        self.cards = cards;
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
                let item = Item::new(ItemKind::Injected, text);
                self.note_item(out, item);
                self.trace.injects += 1;
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
            }
            _ => {}
        }
    }

    /// 一个回合：拿任务 → 反复（调模型 → 调工具 → 收结果）→ 出结论。
    fn run_turn(&mut self, out: &mut impl Write, rx: &Receiver<Inbound>, req: &Envelope, task: String) {
        self.busy = true;
        let result = self.run_turn_inner(out, rx, req, task);
        self.busy = false;
        result
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

            // 助手说"我要调这些" —— 进历史（下一轮模型才看得见自己调过什么）
            let rendered = reply
                .calls
                .iter()
                .map(|c| format!("{}({})", c.tool, c.args))
                .collect::<Vec<_>>()
                .join("; ");
            self.note_item(out, Item::assistant(format!("[调用] {rendered}")));

            for call in reply.calls {
                self.trace.tool_calls += 1;
                let corr = self.ids.next_corr();
                // provider 给了 id 就用它（真模型必须原样带回去）；
                // mock 不给，就用我们的 corr 当 id —— 两者都能配对。
                let call_id = call.id.clone().unwrap_or_else(|| corr.clone());
                let item = Item::tool_call_with(&call.tool, &call.args, &call_id);
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

    /// 每轮前问一句"该压了吗" —— **回合中压缩，摘要贴到最后一条 user 之前**（§5.4.3 ④）。
    fn maybe_compact(&mut self, out: &mut impl Write) {
        let counter = HeuristicCounter::new();
        let summarizer = MockSummarizer::default();
        let compactor = Compactor {
            policy: self.policy.clone(),
            counter: &counter,
            summarizer: &summarizer,
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
                let item = Item::tool_result(
                    &p.call.tool,
                    format!("（超时：{} 一直没回，我不知道它做没做）", p.call.to),
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

    fn card_of(instance: &str, tools: &[&str]) -> ToolCard {
        ToolCard {
            instance: instance.into(),
            title: instance.into(),
            summary: String::new(),
            when_to_use: String::new(),
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
