//! **第二个工具实例：终端。**
//!
//! 它检验的是"这套抽象能不能撑住第二个工具实例"（README 阶段 2）。
//! 和编辑器**同形不同质**的地方，正是要点：
//!
//! | | 编辑器 | 终端 |
//! |---|---|---|
//! | 域内互斥 | **每条路径一把写锁**（排他）| **没有跨实例锁** —— 见下 |
//! | 实例内部 | 一次一个 `edit` | **一次一条命令**，跑着的时候再来就是 `busy` |
//! | 围栏的作用 | 拦 `open` | **拦 `cd`**（允许路径内 cd，不许出去）|
//! | 输出 | 全量返回（内联上限还挂着待定）| 追加缓冲 + **游标**读 |
//!
//! **为什么终端不占跨实例锁**：编辑器那把锁是"同一时刻只有一个写者"（§5.15）。
//! 终端要的是**归属**（这条路径归它管），不是排他 ——
//! 否则两个终端不能在同一个项目里干活，编辑器也没法和终端共存。§5.15 说的
//! "按终端串行"指的是**一个终端内部**，不是跨实例。
//!
//! #### 第一版只有命令模式
//!
//! §5.18 定了两种形态：**命令模式**（`run` 一条、拿输出和退出码）与
//! **会话模式**（`send` + `read` / `screen`，给长驻服务与 REPL）。
//! 第一版只做命令模式 —— 会话模式要真 PTY（ConPTY / alacritty 屏幕仿真）
//! 和 OSC 133 的 shell 集成，那是终端这条线的下一段。
//! 所以 manifest 里**没有 `send`，也没有 `screen`**：
//! 声明一个做不到的工具比不声明更糟。

mod term;
mod job;

use harness_core::codec::{self, CodecError, NdjsonReader, NextLine};
use harness_core::envelope::{Authority, Envelope, MessageType};
use harness_core::id::IdGen;
use harness_core::instance_event::InstanceEvent;
use harness_core::now_ms;
use harness_service::contract::ToolDecl;
use harness_service::domain_lock;
use harness_service::fence::Fence;
use harness_service::grant::{self, Grant};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::{self, BufReader};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::Duration;
use term::{RunOutcome, Terminal};

struct Args {
    instance: String,
    sid: String,
    kind: String,
    fences: Vec<String>,
    /// 结果里最多内联多少字节（**部署选择**，不是普适值 —— §5.4 查过 dsh/TCP 的先例）。
    /// 超了就是头尾 + 一句省略说明，全文仍可按游标 `read`。
    inline_limit: usize,
    /// 实例的输出缓冲在内存里最多留多少字节；**超了就把前面的溢到磁盘**。
    /// 这是 §5.17「背压」的落地形态：我们让消费者去写盘，而不是让生产者阻塞。
    memory_cap: usize,
    /// 溢出文件放哪（`data/spill/<sid>/<实例>.log`）
    spill_dir: PathBuf,
}

fn parse_args() -> Args {
    let mut a = Args {
        instance: "term:1".into(),
        sid: "s-1".into(),
        kind: "term".into(),
        fences: Vec::new(),
        // 默认值来自 §5.4 那份前人数据：50 000 B（dsh 的 `maxInlineBytes`），
        // 以及 1 MiB 的内存缓冲上限（够装下正常命令的输出，扛不住 `yes`）
        inline_limit: 50_000,
        memory_cap: 1_048_576,
        spill_dir: PathBuf::from("data/spill"),
    };
    let mut it = std::env::args().skip(1);
    while let Some(x) = it.next() {
        match x.as_str() {
            "--instance" => a.instance = it.next().unwrap_or(a.instance),
            "--sid" => a.sid = it.next().unwrap_or(a.sid),
            "--kind" => a.kind = it.next().unwrap_or(a.kind),
            "--fence" => {
                if let Some(v) = it.next() {
                    a.fences.push(v);
                }
            }
            "--inline-limit" => {
                if let Some(v) = it.next().and_then(|v| v.parse::<usize>().ok()) {
                    a.inline_limit = v;
                }
            }
            "--memory-cap" => {
                if let Some(v) = it.next().and_then(|v| v.parse::<usize>().ok()) {
                    a.memory_cap = v;
                }
            }
            "--spill-dir" => {
                if let Some(v) = it.next() {
                    a.spill_dir = PathBuf::from(v);
                }
            }
            _ => {}
        }
    }
    a
}

/// 服务循环的两个输入源：**来自中控的请求** 与 **命令线程的报告**。
/// 汇进同一条通道之后，主循环就是单线程的 —— 和 ctrl 同一个形状。
enum Inbound {
    Line(String),
    /// 命令吐了一段字
    TermOutput { term_id: String, text: String },
    /// 某一条输出流读完了（stdout / stderr 各报一次）
    TermReaderDone { term_id: String },
    Eof,
}

/// 主循环的心跳。**必须始终带超时** —— 命令是异步跑的，
/// 没有心跳就没人去 poll 它的退出，也没有人执行超时。
const TICK: Duration = Duration::from_millis(30);

struct Term {
    args: Args,
    ids: IdGen,
    fence: Fence,
    degraded: Option<String>,
    terms: BTreeMap<String, Terminal>,
    next_term: u64,
    tx: Sender<Inbound>,
}

fn main() {
    let args = parse_args();
    let (tx, rx): (Sender<Inbound>, Receiver<Inbound>) = mpsc::channel();
    let mut svc = Term {
        ids: IdGen::new(args.instance.replace([':', '.'], "-")),
        fence: Fence::new(&args.fences).unwrap_or_default(),
        degraded: None,
        terms: BTreeMap::new(),
        next_term: 0,
        tx: tx.clone(),
        args,
    };
    if svc.fence.is_empty() {
        eprintln!("[term] 警告：没有托管路径，任何命令都会被拒（缺 --fence）");
    }
    match domain_lock::acquire("term", svc.fence.roots()) {
        Ok(_lock) => eprintln!("[term] 域锁拿到：{}", svc.fence.roots_display()),
        Err(c) => {
            eprintln!("[term] 域冲突：{}", c.why);
            svc.degraded = Some(format!("{}；{}", c.why, c.remedy));
        }
    }

    let hello = svc.mint(
        MessageType::Presence,
        "ctrl",
        None,
        match &svc.degraded {
            None => json!({"state": "online", "manifest": manifest(&svc.args, &svc.fence)}),
            Some(reason) => json!({
                "state": "degraded", "degraded_reason": reason,
                "manifest": manifest(&svc.args, &svc.fence)
            }),
        },
    );
    emit(&mut io::stdout(), &hello);

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
                        eprintln!("[term] 跳过一行非 UTF-8 的输入（{bytes} 字节）")
                    }
                    Ok(NextLine::Eof) => break,
                    Err(e) => {
                        eprintln!("[term] 读 stdin 失败：{e}");
                        break;
                    }
                }
            }
            let _ = tx.send(Inbound::Eof);
        });
    }

    loop {
        match rx.recv_timeout(TICK) {
            Ok(Inbound::Line(line)) => svc.on_line(&line),
            Ok(Inbound::TermOutput { term_id, text }) => svc.on_output(&term_id, &text),
            Ok(Inbound::TermReaderDone { term_id }) => svc.on_reader_done(&term_id),
            Ok(Inbound::Eof) => break,
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        // 心跳里做三件事：看进程退没退、超时了没有、能不能回结果了
        svc.tick();
    }

    svc.kill_all();
    if !svc.terms.is_empty() {
        eprintln!("[term] 收摊时还有 {} 个终端", svc.terms.len());
    }
    let bye = svc.mint(
        MessageType::Presence,
        "ctrl",
        None,
        json!({"state": "offline"}),
    );
    emit(&mut io::stdout(), &bye);
}

/// **这个终端的手册**：手写、分章、由它自己提供。
///
/// 章节 id 必须和 manifest 里 `help_sections` 声明的一致 ——
/// 那一列是给模型看的目录，这里是正文。
const HELP: &[(&str, &str, &str)] = &[
    (
        "overview",
        "概览",
        "终端是干什么的：\n\
         · 它钉在一条（或几条）**托管路径**上，命令的工作目录只能在这几条路径里。\n\
         · 一个实例 = 一条路径上的一件事。想同时干两件事就 open 两个。\n\
         · **它不占跨实例的锁**：两个终端可以落在同一个项目里，互不影响。\n\
         · 跑着一条命令时再来一条，会被拒（busy），**不排队** —— 换个实例或者等它。\n\
         · 它只管**命令**。改文件请用编辑器 —— 那边才有写互斥。",
    ),
    (
        "two-modes",
        "命令模式与会话模式（第一版只有前者）",
        "§5.18 定了两种形态：\n\
         · **命令模式**（run）：敲一条、等它结束、把输出给你。绝大多数场景用这个。\n\
         · **会话模式**（send + read / screen）：给长驻服务、REPL、进度条那种东西。\n\
         **第一版只做了命令模式。** `send` / `screen` 会明确告诉你\"还没做\"，\
         而不是假装成功 —— 因为它们要真 PTY 和屏幕仿真。\n\
         所以别指望能跟一个交互式程序对话；run 一条就得等它退出。",
    ),
    (
        "cursor",
        "输出与游标",
        "· `run` 返回的 `output` 是**这次命令的**输出，外加一个 `cursor`（字节位置）。\n\
         · `read` 不传 `since` = \"自上次读以来\"（游标由服务端记着，你不用记）。\n\
         · 想重读某一段就传 `since`（字节偏移）。\n\
         · 输出很长时 `read` 会分页，返回 `next_cursor` —— 拿它继续读。\n\
         ⚠️ `read` 对**交互式程序**基本没用（它们重写屏幕而不是追加）。",
    ),
    (
        "errors",
        "错误码与补救",
        "· `busy` —— 这个终端在跑别的命令。别等：换一个实例。\n\
         · `instance_gone` —— 实例没了（可能被 close 了）。重新 open。\n\
         · `refused` + \"不在托管路径内\" —— 你 cd 到了围栏外面。**那不是故障，是围栏**。\n\
         · `refused` + \"需要的能力你没有\" —— 中控没给你这个能力，找它要，别重试。\n\
         · `timeout`（配上 effect: partial）—— 到点我把它掐了，所以它**做了一半**。\n\
         判断它做到哪一步，看 output；要再跑就给更大的 timeout_ms。",
    ),
];

/// 每个工具**需要什么能力**。这是工具自己的接口的一部分，所以写在这儿。
///
/// 中控不知道这张表 —— 它只知道"我授了什么"。校验在执行侧做，
/// 于是改这里不需要动中控（§7 那句"工具只校验手里的凭证够不够干这件事"）。
fn required_caps(tool: &str) -> &'static [&'static str] {
    match tool {
        // 跑命令 = 在本机上执行东西，这是最需要资格的一件事
        "run" | "open" => &["proc.exec"],
        // 只读的、以及管理这个实例自身的，不需要额外资格
        "read" | "close" | "status" | "ping" | "help" => &[],
        // 不认识的工具走"未知工具"那条路，不在这里管
        _ => &[],
    }
}

fn manifest(args: &Args, fence: &Fence) -> Value {
    json!({
        "contract_version": "1",
        "id": "term",
        "domain": "shell",
        "version": env!("CARGO_PKG_VERSION"),
        "title": "终端",
        "kind": args.kind,
        "platforms": ["windows", "linux", "macos"],
        "node_kind": "terminal",
        // 只有**实例状态**；`degraded` 是存活状态，归中控那一轴（§11.8 的规则）。
        // `closed` 必须有 —— 关实例时那条事件报的就是它
        // （这条是"实例状态必须出自自己声明的词表"那道闸门当场抓出来的：
        //  第一版漏了 `closed`，于是每次 close 都在报一个没声明过的状态）。
        "state_vocabulary": ["idle", "running", "exited", "closed"],
        "content": ["screen", "log"],
        "node_actions": ["interrupt", "close"],
        "instance_types": ["shell"],
        "summary": "在托管路径内跑单条命令，输出可以用游标读。",
        "when_to_use": "要执行程序、跑测试、看构建结果时。改文件请用编辑器 —— 它有写互斥。",
        "will": "它开着的那些终端都没了 —— cwd、输出游标、滚屏历史一起丢；\
                 它起的子进程会被 Job Object 一并带走，所以**不会留下孤儿进程**占着你的输出管道。",
        "injection_policy": "card",
        "help_sections": [
            {"id": "overview", "title": "概览"},
            {"id": "two-modes", "title": "命令模式与会话模式（第一版只有前者）"},
            {"id": "cursor", "title": "输出与游标"},
            {"id": "errors", "title": "错误码与补救"}
        ],
        // **报规范化之后的围栏**（不是当初那几个参数字符串）：
        // 中控要拿它做范围判断，那两边就得用同一把尺子 —— 大小写、分隔符、
        // `\\?\` 前缀这些差异必须在这一步消掉。
        "fence": if fence.is_empty() {
            args.fences.clone()
        } else {
            fence.roots()
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
        },
        "owns": {"domain": "shell", "exclusive_per": "fence"},
        // ⚠️ **这里必须诚实**：终端报的围栏是"**绑定的范围**"（哪些路径归它管），
        // **不是沙箱** —— 它拉起的命令继承整台机器的文件系统视图。
        // 写成 `fence-args` 会是一句让人安心的假话，而这正是 §5.8 要避免的那种东西。
        "sandbox": {
            "filesystem": "none",
            "network": "unrestricted",
            "identity": "same-user",
            "executables": "inherit",
            "resources": "timeout"
        },
        "sandbox_note": "**五个面里只有「时间」是真的**：`timeout_ms` 到点就整棵树掐掉。\
                          文件系统 / 网络 / 身份 / 可执行文件**都没有限制** ——\
                          它跑的命令和你自己在命令行里跑的是同一个世界。\
                          我报的那个 `fence` 是**绑定范围**（哪些路径归这个实例管），\
                          **不是沙箱**；把这两件事混着叫「围栏」是这里最容易骗人的地方。",
        // **参数表与结果形状都是服务自己的义务**（规范 §11.1 的 C 组）：
        // 只报工具名不报参数，等于让人猜怎么调；不报结果形状，就没人能验它。
        "tools": tools(),
        "not_implemented": {
            "send": "会话模式要真 PTY；第一版只做命令模式（§5.18）",
            "screen": "屏幕网格要终端仿真；同上"
        },
        // `canceled` 也在这张表里 —— 它是**我们真的会用到的码**
        // （调用方取消之后那条 `partial` + `canceled` 的答复）。
        // 漏报它的下场被闸门当场抓到了：模型收到一个"没登记过"的码，
        // 就没法判断重试有没有用（§11.5）。
        "error_codes": ["service_down", "instance_gone", "busy", "refused",
                        "invalid", "timeout", "canceled"]
    })
}

/// 工具表 —— §11.1 C 组的九个字段，一个不少。
///
/// 两条和编辑器不同的地方，都来自"这个域是什么"：
/// * `run` **一次一条命令，跑着的时候再来是 `busy`** —— 所以它 `destructive`
///   且**不幂等**（跑两遍 `del` 和跑一遍不一样）；
/// * `run` 的结果**只在成功或做了一半时**才有 `output`（被 `busy` / `refused`
///   挡下时没有输出可给），所以那条必填是**带条件的**。
fn tools() -> Vec<Value> {
    vec![
        ToolDecl::new(
            "help",
            "这本手册本身：不带参数看章节清单，带 section 看那一章，带 search 找内容。",
            json!({"type": "object", "additionalProperties": false,
                   "properties": {"section": {"type": "string", "description": "章节 id，见不带参数时的清单"},
                                  "search": {"type": "string"}}}),
        )
        .read_only()
        .idempotent()
        .requires_on(&["confirmed"], &["data"])
        .to_json(),
        ToolDecl::new(
            "open",
            "给一个目录开一个终端实例，返回 instance_id。",
            json!({"type": "object", "required": ["path"], "additionalProperties": false,
                   "properties": {"path": {"type": "string", "description": "目录路径；相对路径按托管根解析"},
                                  "purpose": {"type": "string", "description": "这个终端是干什么用的（开的时候自己写）"}}}),
        )
        .caps(&["proc.exec"])
        .idempotent()
        .returns(json!({
            "instance_id": {"type": ["string", "null"]},
            "path": {"type": "string"},
            "purpose": {"type": ["string", "null"]},
            "idempotent": {"type": "boolean"},
        }))
        .requires_on(&["confirmed"], &["instance_id"])
        .lifecycle("per_session")
        .to_json(),
        ToolDecl::new(
            "run",
            "在某个终端实例里跑一条命令，返回输出和 exit_code。写即执行。",
            json!({"type": "object", "required": ["instance_id", "cmd"], "additionalProperties": false,
                   "properties": {"instance_id": {"type": "string"},
                                  "cmd": {"type": "string", "description": "一次一条命令，例如 dir /b"},
                                  "timeout_ms": {"type": "integer", "description": "到点就掐掉，结果报 partial + timeout"}}}),
        )
        .caps(&["proc.exec"])
        .destructive()
        .returns(json!({
            "exit_code": {"type": "integer"},
            "output": {"type": "string"},
            "cursor": {"type": "integer"},
            "bytes": {"type": "integer"},
            "cwd": {"type": "string"},
            // `cd` 走的是这条工具的另一条分支：它改的是工作目录，
            // 所以还要说清**谁改的**、以及新目录是什么
            "by": {"type": "string"},
            // `busy` 时必有：**挡住你的那条命令是什么、谁发的**
            "running": {
                "type": "object",
                "required": ["cmd", "by"],
                "properties": {"cmd": {"type": "string"}, "by": {"type": "string"}}
            },
            // 输出太长溢出到文件时，全文在哪（§5.17）；没溢出就是 null
            "spilled_to": {"type": ["string", "null"]},
        }))
        // 成功和"做了一半"都得给输出 —— 那正是模型要看的东西；
        // 而"不是没做"这句话，只有带着 output 才说得清（§11.2 五态）。
        .requires_on(&["confirmed", "partial"], &["output"])
        .lifecycle("per_session")
        .to_json(),
        ToolDecl::new(
            "read",
            "按游标读这个终端累积的输出；不传 since 就是「自上次以来」。",
            json!({"type": "object", "required": ["instance_id"], "additionalProperties": false,
                   "properties": {"instance_id": {"type": "string"},
                                  "since": {"type": "integer", "description": "字节游标"},
                                  "limit": {"type": "integer"}}}),
        )
        .read_only()
        .idempotent()
        .returns(json!({
            "text": {"type": "string"},
            "from": {"type": "integer"},
            "next_cursor": {"type": ["integer", "null"]},
            "bytes": {"type": "integer"},
            "cwd": {"type": "string"},
            // 现在跑的是什么命令（空闲时是 null）—— 给模型和 GUI 各一份事实
            "running": {"type": ["string", "null"]},
            "spilled_to": {"type": ["string", "null"]},
        }))
        .requires_on(&["confirmed"], &["text"])
        .lifecycle("per_session")
        .to_json(),
        ToolDecl::new(
            "close",
            "关掉这个终端实例（正在跑的命令会被掐掉并给一个 canceled 的交代）。",
            json!({"type": "object", "required": ["instance_id"], "additionalProperties": false,
                   "properties": {"instance_id": {"type": "string"}}}),
        )
        .idempotent()
        .returns(json!({
            "closed": {"type": "string"},
            "killed_running": {"type": "boolean"},
            "output": {"type": "string"},
            "cursor": {"type": "integer"},
        }))
        .requires_on(&["confirmed"], &["closed"])
        .lifecycle("per_session")
        .to_json(),
        ToolDecl::new(
            "status",
            "看现在开着哪些终端、各自在跑什么、在哪条路径上。",
            json!({"type": "object", "additionalProperties": false, "properties": {}}),
        )
        .read_only()
        .idempotent()
        .requires_on(&["confirmed"], &["data"])
        .to_json(),
        // `health: "ping"` 声明了探针就是它，所以它得在表里。
        ToolDecl::new(
            "ping",
            "看这个服务还在不在。",
            json!({"type": "object", "additionalProperties": false, "properties": {}}),
        )
        .read_only()
        .idempotent()
        .returns(json!({"degraded": {"type": "boolean"},
                        "degraded_reason": {"type": "string"}}))
        .requires_on(&["confirmed"], &["data"])
        .to_json(),
    ]
}

/// **结果的"内联上限"**（§5.17）：太长的输出只给**头 + 尾 + 一句省略说明**，
/// 全文仍留在实例里（内存 + 溢出文件），按游标 `read` 就能一段段读回来。
///
/// 三条工程细节，都是从 dsh 那份 spill-policy 抄来的（§5.4）：
///
/// 1. **首尾 50/50 对半切** —— 不是 4:1（那个比例是"压缩前剪一刀"用的，两回事）；
/// 2. **通知本身占的字节先从预算里扣掉**，再拿剩下的切预览 ——
///    否则"预览花光预算 + 后面再追加通知"就会超出上限；
/// 3. **切点对齐到字符**（别把汉字劈成两个替换符）。
///
/// 它**不是**背压本身：背压是"缓冲满了往磁盘溢"（那是 `Terminal` 的事），
/// 这里管的是"一次结果里最多塞多少字"。两件事经常被混成一件。
fn inline_head_tail(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_string();
    }
    // 通知先占一块固定预算（实际那句话比这短，所以结果不会超）
    const NOTICE_RESERVE: usize = 200;
    let budget = limit.saturating_sub(NOTICE_RESERVE);
    let head_n = budget / 2;
    let tail_n = budget - head_n;
    // 头往前让到字符边界
    let mut h = head_n.min(text.len());
    while h < text.len() && !text.is_char_boundary(h) {
        h += 1;
    }
    // 尾往后让（从"倒数第 tail_n 个字节"往回找边界）
    let mut t = text.len().saturating_sub(tail_n);
    while t < text.len() && !text.is_char_boundary(t) {
        t += 1;
    }
    if t < h {
        t = h; // 极端小上限时的退化：至少别倒着切
    }
    let omitted = text.len() - h - (text.len() - t);
    format!(
        "{}\n…〔省略 {omitted} 字节〕全文还在这个实例里，用 read 带 since 游标分段读…\n{}",
        &text[..h],
        &text[t..]
    )
}

/// **实例独属**（§5.10 的办公软件那张表）：**每个智能体拿到的都是独属于自己的前端。**
///
/// 规矩一句话：**智能体之间互不越界，用户不受限。**
/// * 智能体只能动 `自己@本会话` 开的实例 —— 拿着别人的 `t-2` 也不行；
/// * `user` 级调用方（用户亲自在控制台）能碰任何实例 —— §7.1 定过
///   "用户永远能杀掉和浏览正在运行的终端"；
/// * 直连调试的（没经过网关，`unrouted`）也放行 —— 那是人在自己机器上干活。
///
/// **为什么终端也要自己查一遍**：中控那边已经有一次统一的归属检查
/// （它掌握"谁开着什么"这份图），这里是**纵深** ——
/// 服务不该因为"网关替我挡住了"就把自己的实例门户大开。
/// 编辑器一直有这道（锁的持有者校验），终端原来**没有**：
/// `read` 里写着 `let _ = req;`，谁拿到 id 都能读别人的输出、还能把它关掉。
///
/// 写成自由函数（而不是 `&self` 的方法）是因为调用点常常正持有
/// `self.terms` 的可变借用 —— 归属检查本来也不需要看这个服务的别的状态。
fn not_mine(req: &Envelope, t: &Terminal) -> Option<Value> {
    let grant = Grant::from_payload(&req.payload);
    if !grant.is_agent() {
        return None; // 用户 / 直连调试：不管
    }
    if t.holder == format!("{}@{}", req.from, req.sid) {
        return None;
    }
    Some(json!({
        "route": null, "effect": "refused", "code": "refused",
        "why": format!("`{}` 不是你的实例 —— 它是 `{}` 开的", t.id, t.holder),
        "remedy": "**同一个目录可以各开一个**，互不影响：自己 open 一个就行。\
                   别在别人的实例上动东西 —— 它的 cwd、游标和正在跑的命令都是人家的"
    }))
}

impl Term {
    /// **实例独属**（§5.10 的办公软件那张表）：**每个智能体拿到的都是独属于自己的前端。**
    ///
    /// 规矩一句话：**智能体之间互不越界，用户不受限。**
    /// * 智能体只能动 `自己@本会话` 开的实例 —— 拿着别人的 `t-2` 也不行；
    /// * `user` 级调用方（用户亲自在控制台）能碰任何实例 —— §7.1 定过
    ///   "用户永远能杀掉和浏览正在运行的终端"；
    /// * 直连调试的（没经过网关，`unrouted`）也放行 —— 那是人在自己机器上干活。
    ///
    /// **为什么终端也要自己查一遍**：中控那边已经有一次统一的归属检查
    /// （它掌握"谁开着什么"这份图），这里是**纵深** ——
    /// 服务不该因为"网关替我挡住了"就把自己的实例门户大开。
    /// 编辑器一直有这道（锁的持有者校验），终端原来**没有**：
    /// `read` 里写着 `let _ = req;`，谁拿到 id 都能读别人的输出、还能把它关掉。
    /// **手册**：渐进披露的第二层（§5.3 / §5.5 的 man page 模型）。
    ///
    /// 三条纪律，每条都是被规范按死的：
    ///
    /// 1. **手写**，不是从接口生成的 —— 从 schema 生成的文档"说得没错但帮不上忙"（§11.1）；
    /// 2. **分章节**，不一次全倒出来 —— 工具大了，全量手册本身就爆上下文；
    /// 3. **由服务自己提供** —— 谁会写这本手册？写这个工具的那个人，不是中控。
    ///    这就是"责任向下传导"最直白的一处。
    fn tool_help(&mut self, args: &Value) -> Value {
        let section = args.get("section").and_then(Value::as_str).unwrap_or("");
        let search = args.get("search").and_then(Value::as_str).unwrap_or("");
        if !search.is_empty() {
            let hits: Vec<Value> = HELP
                .iter()
                .filter(|(_, _, body)| body.contains(search))
                .map(|(id, title, _)| json!({"id": id, "title": title}))
                .collect();
            return json!({
                "route": null, "effect": "confirmed", "code": null,
                "data": {"search": search, "sections": hits,
                         "remedy": if hits.is_empty() { "换个词；或者不带参数看章节清单" } else { "用 section 取那一章" }}
            });
        }
        match HELP.iter().find(|(id, _, _)| *id == section) {
            Some((id, title, body)) => json!({
                "route": null, "effect": "confirmed", "code": null,
                "data": {"section": id, "title": title, "text": body}
            }),
            None if section.is_empty() => json!({
                "route": null, "effect": "confirmed", "code": null,
                "data": {
                    "sections": HELP.iter().map(|(id, title, _)| json!({"id": id, "title": title})).collect::<Vec<_>>(),
                    "how": "要看某一章就传 section；要找内容就传 search",
                    "why": "手册是分章的 —— 一次全给你会把上下文吃掉（§5.3 渐进披露）"
                }
            }),
            None => json!({
                "route": null, "effect": "refused", "code": "invalid",
                "why": format!("没有 `{section}` 这一章"),
                "expect": {"section": HELP.iter().map(|(id, _, _)| *id).collect::<Vec<_>>().join(" | ")},
                "remedy": "不带参数调一次，先看章节清单"
            }),
        }
    }

    fn mint(
        &mut self,
        kind: MessageType,
        to: &str,
        corr: Option<String>,
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
            authority: Authority::Observe,
            payload,
        }
    }

    /// 心跳：看退出、执行超时、能回结果的回结果。
    ///
    /// **顺序不能换**：先 poll 退出，再判超时，最后才看"能不能回结果" ——
    /// 反过来会在超时的那一轮把结果当成"正常退出"报出去。
    fn tick(&mut self) {
        for t in self.terms.values_mut() {
            t.poll_exit();
        }
        for t in self.terms.values_mut() {
            if let Some(id) = t.kill_for_timeout() {
                eprintln!("[term] {id} 超时，掐掉");
            }
        }
        let ready: Vec<(String, RunOutcome)> = self
            .terms
            .values()
            .filter_map(|t| t.ready().map(|o| (t.id.clone(), o)))
            .collect();
        for (id, outcome) in ready {
            self.on_done(&id, outcome);
        }
    }

    fn on_reader_done(&mut self, term_id: &str) {
        if let Some(t) = self.terms.get_mut(term_id) {
            t.reader_done();
        }
    }

    fn on_line(&mut self, line: &str) {
        let req = match codec::decode(line) {
            Ok(r) => r,
            Err(CodecError::Empty) => return,
            Err(e) => {
                eprintln!("[term] 收到不合规的消息：{e}");
                return;
            }
        };
        if req.kind != MessageType::Request {
            // **`cancel` 是唯一一条"不是请求但要处理"的消息**（§2.3）。
            // 在这之前它定义了、方向规则也校验了，但**从来没被产生过** ——
            // 超时走的是"杀掉"，调用方根本不知道那算不算它要的结果。
            if req.kind == MessageType::Cancel {
                let corr = req.corr.clone().unwrap_or_default();
                let did = self
                    .terms
                    .values_mut()
                    .any(|t| t.cancel(&corr));
                eprintln!(
                    "[term] 收到 cancel（corr={corr}）：{}",
                    if did { "确实停下了" } else { "没有对得上的在跑 —— 如实说没停成" }
                );
            }
            return;
        }
        let tool = req.payload.get("tool").and_then(Value::as_str).unwrap_or("");
        let args = req.payload.get("args").cloned().unwrap_or(json!({}));

        let mut out = io::stdout();
        // **能力校验在执行侧**（§7："工具只校验手里的凭证够不够干这件事"）。
        // 中控只把"你被授予了什么"附在请求上；**"这个工具需要什么"是工具自己的事** ——
        // 所以加一个工具、改一条要求，都不用动中控。
        let grant = Grant::from_payload(&req.payload);
        let required = required_caps(tool);
        if !grant.allows(required) {
            let payload = grant::refuse_payload(tool, &grant.missing(required), &grant);
            let reply = self.mint(MessageType::Result, &req.from.clone(), req.corr.clone(), payload);
            emit(&mut out, &reply);
            return;
        }
        if let Some(reason) = self.degraded.clone() {
            let payload = if matches!(tool, "ping" | "status") {
                json!({"route": null, "effect": "confirmed", "code": null, "degraded": true,
                       "degraded_reason": reason})
            } else {
                json!({"route": null, "effect": "refused", "code": "refused", "why": reason,
                       "remedy": "这个实例被降级了，它不执行命令"})
            };
            let reply = self.mint(MessageType::Result, &req.from.clone(), req.corr.clone(), payload);
            emit(&mut out, &reply);
            return;
        }

        // `run` 是**唯一**一条异步回复的路径：它要等下一条命令跑完。
        if tool == "run" {
            self.tool_run(&req, &args);
            return;
        }

        let payload = match tool {
            "ping" => json!({"route": null, "effect": "confirmed", "code": null, "data": {"pong": true}}),
            "help" => self.tool_help(&args),
            "open" => self.tool_open(&req, &args),
            "read" => self.tool_read(&req, &args),
            "close" => self.tool_close(&req, &args),
            "status" => self.tool_status(&req),
            "send" | "screen" => json!({
                "route": null, "effect": "refused", "code": "invalid",
                "why": format!("`{tool}` 属于**会话模式**，第一版还没做"),
                "remedy": "会话模式要真 PTY（ConPTY + 屏幕仿真 + OSC 133 的 shell 集成）——\
                           见 02-工具 §5.18。现在用 run 跑单条命令"
            }),
            other => {
                let shown = if other.is_empty() { "(空)" } else { other };
                json!({
                    "route": null, "effect": "refused", "code": "invalid",
                    "why": format!("没有名为 `{shown}` 的工具"),
                    "expect": {"tool": "open | run | read | close | status | ping"},
                    "remedy": "presence 里的 manifest.help_sections 有章节清单；先从 open 开始"
                })
            }
        };
        let reply = self.mint(MessageType::Result, &req.from.clone(), req.corr.clone(), payload);
        emit(&mut out, &reply);
    }

    fn tool_open(&mut self, req: &Envelope, args: &Value) -> Value {
        let raw = args.get("path").and_then(Value::as_str).unwrap_or(".");
        let purpose = args
            .get("purpose")
            .and_then(Value::as_str)
            .unwrap_or("(未说明用途)");
        let dir = match self.fence.resolve(raw) {
            Ok(p) => p,
            Err(e) => {
                return json!({"route": null, "effect": "refused", "code": "refused",
                              "why": e.why, "remedy": e.remedy})
            }
        };
        if !dir.is_dir() {
            return json!({
                "route": null, "effect": "refused", "code": "invalid",
                "why": format!("`{}` 不是一个目录", dir.display()),
                "remedy": "终端钉在一个目录上；要给一个目录路径"
            });
        }

        // 同一个持有者在同一个目录上重复 open ⇒ 幂等（和编辑器同一条纪律）
        let holder = format!("{}@{}", req.from, req.sid);
        if let Some(t) = self
            .terms
            .values()
            .find(|t| t.cwd == dir && t.holder == holder)
        {
            return json!({
                "route": null, "effect": "confirmed", "code": null,
                "instance_id": t.id, "path": dir.display().to_string(),
                "purpose": t.purpose, "idempotent": true
            });
        }

        self.next_term += 1;
        let id = format!("t-{}", self.next_term);
        // **实例自带一句用途说明**（开的时候自己写，§5.16）
        // 溢出文件按会话+实例分堆：它是**服务自己的状态**（不是用户的项目文件）
        let spill = self.args.spill_dir.join(&self.args.sid).join(format!("{id}.log"));
        let t = Terminal::new(
            id.clone(),
            dir.clone(),
            purpose.to_string(),
            holder.clone(),
            spill,
            self.args.memory_cap,
        );
        self.terms.insert(id.clone(), t);

        // **标准形状的实例事件** —— 中控靠它把节点画到图上（第三层）
        let ev = self.mint(
            MessageType::Event,
            "ctrl",
            None,
            InstanceEvent::open(&id, "idle")
                .with_path(dir.display().to_string())
                .with_purpose(purpose)
                // **归属上报**：中控靠它做"别动别人的实例"的统一检查
                .owned_by(&holder)
                .to_payload(),
        );
        emit(&mut io::stdout(), &ev);

        json!({
            "route": null, "effect": "confirmed", "code": null,
            "instance_id": id,
            "path": dir.display().to_string(),
            "purpose": purpose
        })
    }

    /// `run`：**唯一异步的工具**。它先不回复，等命令结束再回一个 `result`。
    fn tool_run(&mut self, req: &Envelope, args: &Value) {
        let id = args.get("instance_id").and_then(Value::as_str).unwrap_or("");
        let cmd = args.get("cmd").and_then(Value::as_str).unwrap_or("");
        let timeout = args
            .get("timeout_ms")
            .and_then(Value::as_u64)
            .map(Duration::from_millis);
        let mut out = io::stdout();

        if cmd.trim().is_empty() {
            let reply = self.mint(
                MessageType::Result,
                &req.from.clone(),
                req.corr.clone(),
                json!({"route": null, "effect": "refused", "code": "invalid",
                       "why": "cmd 是空的",
                       "remedy": "一次给一条命令，例如 {\"cmd\": \"dir /b\"}"}),
            );
            emit(&mut out, &reply);
            return;
        }
        let Some(t) = self.terms.get(id) else {
            let reply = self.mint(
                MessageType::Result,
                &req.from.clone(),
                req.corr.clone(),
                json!({"route": null, "effect": "refused", "code": "instance_gone",
                       "why": format!("没有实例 `{id}`"),
                       "remedy": "先 open 拿一个实例；status 能看到现在开着哪些"}),
            );
            emit(&mut out, &reply);
            return;
        };
        // **实例独属**：别人开的终端，你连往里敲命令都不行
        if let Some(v) = not_mine(req, t) {
            let reply = self.mint(MessageType::Result, &req.from.clone(), req.corr.clone(), v);
            emit(&mut out, &reply);
            return;
        }
        // **一个终端内部串行**：跑着的时候再来一条就是 busy，不排队（同 §5.15）。
        if let Some(running) = t.running.as_ref() {
            let reply = self.mint(
                MessageType::Result,
                &req.from.clone(),
                req.corr.clone(),
                json!({
                    "route": null, "effect": "refused", "code": "busy",
                    "why": format!("这个终端正在跑 `{}`，已经 {} ms",
                                   running.cmd, now_ms() - running.started_ms),
                    "running": {"cmd": running.cmd, "by": running.by},
                    "remedy": "别等：换一个终端实例，或者等它结束（read 能看它已经吐了什么）"
                }),
            );
            emit(&mut out, &reply);
            return;
        }

        // 命令不能为空、不能是 cd（cd 自己处理）
        if let Some(value) = cd_target(cmd) {
            let payload = self.do_cd(id, value, req);
            let reply = self.mint(MessageType::Result, &req.from.clone(), req.corr.clone(), payload);
            emit(&mut out, &reply);
            return;
        }

        let t = self.terms.get_mut(id).expect("上面查过");
        match t.start(cmd, &req.from, req.sid.clone(), timeout, req.corr.clone(), self.tx.clone()) {
            Ok(_) => {
                // 状态迁移也要报 —— 图上那个点的颜色靠它
                let ev = self.mint(
                    MessageType::Event,
                    "ctrl",
                    None,
                        InstanceEvent::state(id, "running")
                            .owned_by(format!("{}@{}", req.from, req.sid))
                            .with_detail(json!({"cmd": cmd, "by": req.from}))
                            .to_payload(),
                );
                emit(&mut out, &ev);
            }
            Err(e) => {
                let reply = self.mint(
                    MessageType::Result,
                    &req.from.clone(),
                    req.corr.clone(),
                    json!({"route": null, "effect": "refused", "code": "refused",
                           "why": format!("起不来：{e}"),
                           "remedy": "检查这条命令本身；它是由系统 shell 执行的"}),
                );
                emit(&mut out, &reply);
            }
        }
    }

    /// 把已经累积的输出吐成 `chunk`。**这是"响应式"在协议上的样子。**
    fn on_output(&mut self, term_id: &str, text: &str) {
        let Some(t) = self.terms.get_mut(term_id) else {
            return;
        };
        t.append(text);
        // 拷出来就完事 —— 后面要用 &mut self 发消息，不能再借着 t
        let Some((corr, to)) = t
            .running
            .as_ref()
            .map(|r| (r.corr.clone(), r.by.clone()))
        else {
            return;
        };
        let ev = self.mint(
            MessageType::Chunk,
            &to,
            Some(corr),
            json!({"text": text, "term": term_id}),
        );
        emit(&mut io::stdout(), &ev);
    }

    fn on_done(&mut self, term_id: &str, outcome: RunOutcome) {
        let (running, raw, bytes, spilled_to) = {
            let Some(t) = self.terms.get_mut(term_id) else {
                return;
            };
            let Some(running) = t.finish(&outcome) else {
                return;
            };
            let output = t.slice(running.from, running.to);
            (
                running,
                output,
                t.len(),
                t.spill_path().map(|p| p.display().to_string()),
            )
        };
        // **一次结果里最多塞多少字**（§5.17）：超了就头尾 + 一句省略说明。
        // 全文没丢 —— 它在这个实例里（内存 + 溢出文件），按游标 `read` 就能读完。
        let trimmed = raw.len() > self.args.inline_limit;
        let output = inline_head_tail(&raw, self.args.inline_limit);
        let how_to_read = format!(
            "全文没丢：用 read 带 `since` 从 {} 开始分段读（默认每页 {} 字节），读完为止",
            running.from, self.args.inline_limit
        );
        // **命令失败 ≠ 调用失败。**
        // `effect` 说的是"这个动作生效了没有"，不是"命令成功了没有"（§11.2）。
        // 退出码 1 的命令**确实执行了** —— 那是数据，不是传输错误。
        let mut payload = match &outcome {
            RunOutcome::Exited(code) => json!({
                "route": null, "effect": "confirmed",
                "code": null,
                "exit_code": code,
                "output": output,
                "cursor": running.to,
                "bytes": bytes,
                "spilled_to": if trimmed { spilled_to } else { None }
                // **`why` 不在这里写** —— 见下面那段（不能塞 `null`）
            }),
            RunOutcome::TimedOut => json!({
                "route": null, "effect": "partial",
                "code": "timeout",
                "why": if trimmed {
                    format!("到点了，我把它掐了 —— 所以它做了一半。{how_to_read}")
                } else {
                    "到点了，我把它掐了 —— 所以它做了一半".to_string()
                },
                "output": output,
                "cursor": running.to,
                "spilled_to": if trimmed { spilled_to } else { None },
                "remedy": "先看输出判断它做到哪一步；要再跑就给更大的 timeout_ms"
            }),
            RunOutcome::Canceled => json!({
                "route": null, "effect": "partial",
                "code": "canceled",
                "why": if trimmed {
                    format!("你取消了它 —— 我确实停了，所以它做了一半（**不是没做**）。{how_to_read}")
                } else {
                    "你取消了它 —— 我确实停了，所以它做了一半（**不是没做**）".to_string()
                },
                "output": output,
                "cursor": running.to,
                "spilled_to": if trimmed { spilled_to } else { None },
                "remedy": "先看 output 判断它停在哪一步；要接着干就重新 run 一条"
            }),
        };
        // **成功时可以带 `why`（信封允许）—— 但"没什么可说"时必须把键省掉，不能塞 `null`。**
        //
        // 2026-09-20 审计抓到的真 bug：这里原来写的是
        // `"why": if trimmed { Some(how_to_read) } else { None }`，
        // 而契约构造器（`harness_service::contract`）把 `why` 声明成 `{"type": "string"}`，
        // schema 又是**闭合**的。于是**每一条输出没被裁剪的普通命令**（也就是绝大多数），
        // 回来都带着 `"why": null` ⇒ 违反它**自己声明的形状** ⇒ 投递闸门每次都要
        // 给它贴一张 `schema_violation`，并在日志里生一条系统事件。
        //
        // 它不是"闸门太严"，也不是"声明该放宽"：`why` 本来就不在 `required` 里 ——
        // **什么都不想说时，正确形状是"没有这个键"**。声明是诚实的，错的只是这一行赋值。
        if trimmed {
            if let Some(obj) = payload.as_object_mut() {
                obj.entry("why".to_string()).or_insert_with(|| json!(how_to_read));
            }
        }
        let reply = self.mint(
            MessageType::Result,
            &running.by.clone(),
            Some(running.corr.clone()),
            payload,
        );
        emit(&mut io::stdout(), &reply);
        // 跑完了 ⇒ 回到空闲
        let ev = self.mint(
            MessageType::Event,
            "ctrl",
            None,
            InstanceEvent::state(term_id, "idle").to_payload(),
        );
        emit(&mut io::stdout(), &ev);
    }

    fn do_cd(&mut self, id: &str, value: Option<String>, req: &Envelope) -> Value {
        let t = self.terms.get_mut(id).expect("调用方查过");
        if let Some(v) = not_mine(req, t) {
            return v;
        }
        let Some(value) = value else {
            // 裸 `cd` = 报当前目录（和 shell 一致）
            return json!({"route": null, "effect": "confirmed", "code": null,
                          "output": format!("{}\n", t.cwd.display()),
                          "cwd": t.cwd.display().to_string(), "exit_code": 0});
        };
        // 相对路径按**当前目录**解，绝对路径直接用 —— 然后一律过围栏
        // **一次一条命令。** `cd a && rm -rf b` 这种要拆开写 ——
        // 否则我们拦不住它后面的那一半（而它整条都会被当成"cd 的目标"）。
        if value.contains(['&', '|', ';', '>', '<', '`']) {
            return json!({
                "route": null, "effect": "refused", "code": "invalid",
                "why": "cd 后面只能跟一个目录，不能是命令组合",
                "remedy": "拆成几条：先 `cd <目录>`，再单独跑下一条命令"
            });
        }
        // 围栏判在前面、并且**不依赖它存不存在**（词法闸门 → 真实闸门）
        let resolved = match self.fence.resolve_from(&t.cwd, &value) {
            Ok(p) => p,
            Err(e) => {
                return json!({
                    "route": null, "effect": "refused", "code": "refused",
                    "why": e.why,
                    "remedy": format!("{}。**允许在路径内 cd，不允许 cd 出去**", e.remedy)
                })
            }
        };
        match std::fs::canonicalize(&resolved) {
            Ok(p) if p.is_dir() => {
                t.cwd = p.clone();
                json!({
                    "route": null, "effect": "confirmed", "code": null, "exit_code": 0,
                    // **成功也要给 output。** 这条是契约闸门逼出来的：
                    // `run` 声明了"成功必有 output"，而 cd 这条分支原先没有 ——
                    // 于是"声明"和"实现"当场对不上。补上它比放宽声明好，
                    // 因为模型问了一句"cd 到哪了"，本来就该得到一句回答。
                    "output": format!("cwd -> {}\n", p.display()),
                    "cwd": p.display().to_string(),
                    "by": req.from
                })
            }
            Ok(_) => json!({
                "route": null, "effect": "refused", "code": "invalid",
                "why": format!("`{value}` 不是一个目录"),
                "remedy": "cd 只能给目录"
            }),
            Err(e) => json!({
                "route": null, "effect": "refused", "code": "invalid",
                "why": format!("`{value}` 解析不了：{e}"),
                "remedy": "给一个存在、且在托管路径内的目录"
            }),
        }
    }

    fn tool_read(&mut self, req: &Envelope, args: &Value) -> Value {
        let id = args.get("instance_id").and_then(Value::as_str).unwrap_or("");
        let Some(t) = self.terms.get_mut(id) else {
            return json!({"route": null, "effect": "refused", "code": "instance_gone",
                          "why": format!("没有实例 `{id}`"),
                          "remedy": "先 open"});
        };
        if let Some(v) = not_mine(req, t) {
            return v;
        }
        // **游标默认服务端记**（§5.19）：让模型记住自己读到哪，
        // 是把状态管理推给了最不该记这个的一方 —— 压缩之后它会忘。
        let from = args
            .get("since")
            .and_then(Value::as_u64)
            .map(|v| v as usize)
            .unwrap_or(t.read_cursor);
        // **`read` 是个分页器，不是摘要器。**
        // 所以这里不做"头尾 + 省略"（那是 `run` 的活）：默认一次给
        // **内联上限**那么多字节，`next_cursor` 指给你接着读的地方 ——
        // 于是"想全看"就多读几页，而不是再跑一条命令（§5.17 那条死循环）。
        let limit = args
            .get("limit")
            .and_then(Value::as_u64)
            .map(|v| v as usize)
            .unwrap_or(self.args.inline_limit);
        let to = t.len().min(from.saturating_add(limit));
        let text = t.slice(from, to);
        t.read_cursor = to;
        json!({
            "route": null, "effect": "confirmed", "code": null,
            "text": text,
            "from": from,
            "next_cursor": if to < t.len() { Some(to) } else { None },
            "bytes": t.len(),
            "cwd": t.cwd.display().to_string(),
            "running": t.running.as_ref().map(|r| r.cmd.clone()),
            // 全文**落在哪个文件里**（没溢出就是 null）。
            // 它只是"事实"：正路是按游标 read，不必去开这个文件。
            "spilled_to": t.spill_path().map(|p| p.display().to_string())
        })
    }

    fn tool_close(&mut self, req: &Envelope, args: &Value) -> Value {
        let id = args.get("instance_id").and_then(Value::as_str).unwrap_or("");
        if let Some(t) = self.terms.get(id) {
            if let Some(v) = not_mine(req, t) {
                return v;
            }
        }
        let Some(mut t) = self.terms.remove(id) else {
            return json!({"route": null, "effect": "refused", "code": "instance_gone",
                          "why": format!("没有实例 `{id}`"), "remedy": "status 能看到现在开着哪些"});
        };
        let was_running = t.running.is_some();
        let path = t.cwd.display().to_string();
        // **先把在跑的那次调用结掉，再收终端。**
        // 否则那一次 `run` 的结果就永远不来了 —— 调用方会一直等一个不存在的结果。
        // （这条是被 smoke 抓出来的：close 把结果吞了，c_02 挂了整整 8 秒超时。）
        let aborted = t.abort_running("canceled", "这个终端被关掉了，所以这条命令没跑完");
        if let Some((running, output)) = aborted {
            let reply = self.mint(
                MessageType::Result,
                &running.by.clone(),
                Some(running.corr.clone()),
                json!({
                    "route": null, "effect": "partial",
                    "code": "canceled",
                    "why": "终端在它跑完之前被关掉了 —— 它做了一半",
                    "output": output,
                    "cursor": running.to,
                    "remedy": "先看输出判断它做到哪一步；要重跑就重新 open 一个终端"
                }),
            );
            emit(&mut io::stdout(), &reply);
        }
        t.kill();
        let ev = self.mint(
            MessageType::Event,
            "ctrl",
            None,
            InstanceEvent::close(id)
                .with_detail(json!({"killed_running": was_running}))
                .to_payload(),
        );
        emit(&mut io::stdout(), &ev);
        json!({"route": null, "effect": "confirmed", "code": null, "closed": path,
               "killed_running": was_running})
    }

    /// 看现在开着哪些终端。
    ///
    /// **别人的实例也看得见**（图上本来就该看得见"数据库后端在忙着什么"），
    /// 但每一条都标了 `owner` 和 `can_operate` ——
    /// **看得见别人的，动不了别人的**，这才是"多智能体共用一个后端"的完整形状。
    fn tool_status(&mut self, req: &Envelope) -> Value {
        let terms: Vec<Value> = self
            .terms
            .values()
            .map(|t| {
                json!({
                    "instance_id": t.id,
                    "cwd": t.cwd.display().to_string(),
                    "purpose": t.purpose,
                    "owner": t.holder,
                    "can_operate": not_mine(req, t).is_none(),
                    "running": t.running.as_ref().map(|r| json!({"cmd": r.cmd, "by": r.by})),
                    "bytes": t.len(),
                    "read_cursor": t.read_cursor
                })
            })
            .collect();
        json!({
            "route": null, "effect": "confirmed", "code": null,
            "data": {"fence": self.fence.roots().iter().map(|r| r.display().to_string()).collect::<Vec<_>>(),
                     "term_count": terms.len(), "terms": terms}
        })
    }

    fn kill_all(&mut self) {
        let ids: Vec<String> = self.terms.keys().cloned().collect();
        for id in ids {
            let aborted = self
                .terms
                .get_mut(&id)
                .and_then(|t| t.abort_running("service_down", "服务正在收摊"));
            if let Some((running, output)) = aborted {
                // 收摊前也要把没结的调用结掉 —— **进程退出不是静默丢弃的理由**
                let reply = self.mint(
                    MessageType::Result,
                    &running.by.clone(),
                    Some(running.corr.clone()),
                    json!({
                        "route": null, "effect": "partial",
                        "code": "service_down",
                        "why": "服务收摊了，这条命令被中断",
                        "output": output,
                        "remedy": "先看输出；要重跑就重新开会话"
                    }),
                );
                emit(&mut io::stdout(), &reply);
            }
            if let Some(t) = self.terms.get_mut(&id) {
                t.kill();
            }
        }
    }
}

/// 这条命令是不是 `cd`？是的话给出目标（裸 `cd` 给 `None`）。
fn cd_target(cmd: &str) -> Option<Option<String>> {
    let t = cmd.trim();
    if t == "cd" {
        return Some(None);
    }
    if let Some(rest) = t.strip_prefix("cd ") {
        let arg = rest.trim().trim_matches('"').to_string();
        return Some(Some(arg));
    }
    None
}

fn emit(out: &mut impl io::Write, env: &Envelope) {
    match codec::encode(env) {
        Ok(line) => {
            if let Err(e) = codec::write_line(out, &line) {
                eprintln!("[term] 写 stdout 失败：{e}");
            }
        }
        Err(e) => eprintln!("[term] 自己的消息编码失败：{e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req_from(who: &str, caller_kind: &str) -> Envelope {
        Envelope {
            id: "m-t".into(),
            ts: 0,
            kind: MessageType::Request,
            from: who.into(),
            to: "term:1".into(),
            corr: Some("c-t".into()),
            sid: "s-1".into(),
            authority: Authority::Agent,
            payload: json!({"tool": "run", "caller_kind": caller_kind, "granted": ["*"]}),
        }
    }

    fn a_terminal(holder: &str) -> Terminal {
        Terminal::new(
            "t-1".into(),
            std::env::temp_dir(),
            "(测试)".into(),
            holder.into(),
            std::env::temp_dir().join("harness-term-test-spill.log"),
            1_048_576,
        )
    }

    /// **实例独属**：别的智能体拿着你的 `t-1` 也不行。
    #[test]
    fn another_agent_cannot_touch_your_instance() {
        let t = a_terminal("agent:dada@s-1");
        let v = not_mine(&req_from("agent:bob", "agent"), &t).expect("该被拒");
        assert_eq!(v["effect"], "refused");
        assert_eq!(v["code"], "refused");
        assert!(v["why"].as_str().unwrap().contains("agent:dada@s-1"));
        assert!(v["remedy"].as_str().unwrap().contains("各开一个"));
    }

    /// 自己的实例照常；**用户也照常**（§7.1："用户永远能杀掉和浏览正在运行的终端"）。
    #[test]
    fn the_owner_and_the_user_both_pass() {
        let t = a_terminal("agent:dada@s-1");
        assert!(not_mine(&req_from("agent:dada", "agent"), &t).is_none());
        assert!(not_mine(&req_from("console", "user"), &t).is_none());
        assert!(
            not_mine(&req_from("console", "unrouted"), &t).is_none(),
            "人肉直连调试也放行"
        );
    }

    /// **会话不同的同一个智能体**也不行 —— 归属是 `谁@哪个会话`，不是光看谁。
    #[test]
    fn the_same_agent_in_another_session_is_still_a_stranger() {
        let t = a_terminal("agent:dada@s-1");
        let mut r = req_from("agent:dada", "agent");
        r.sid = "s-2".into();
        assert!(not_mine(&r, &t).is_some());
    }

    /// **自己的 manifest 自己先过一遍闸门**（构建期断言，同编辑器那条）。
    #[test]
    fn our_own_manifest_passes_the_contract_gate() {
        let fence = Fence::new(&[]).expect("空围栏是合法的");
        let args = Args {
            instance: "term:test".into(),
            sid: "s-test".into(),
            kind: "term".into(),
            fences: Vec::new(),
            inline_limit: 50_000,
            memory_cap: 1_048_576,
            spill_dir: PathBuf::from("data/spill"),
        };
        let report = harness_service::contract::check_manifest(&manifest(&args, &fence));
        assert!(report.is_ok(), "{:?}", report.errors);
        assert!(report.missing_optional.is_empty(), "{:?}", report.missing_optional);
    }

    #[test]
    fn cd_is_recognized_only_as_a_whole_command() {
        assert_eq!(cd_target("cd"), Some(None));
        assert_eq!(cd_target("cd src"), Some(Some("src".into())));
        assert_eq!(cd_target("  cd   src  "), Some(Some("src".into())));
        assert_eq!(cd_target("cd \"sub dir\""), Some(Some("sub dir".into())));
        // 不能误判：`cdrom` / `xcd` / 带分号的组合都不是 cd
        assert_eq!(cd_target("cdrom"), None);
        assert_eq!(cd_target("cargo build"), None);
        assert_eq!(cd_target("cd a && rm -rf b"), Some(Some("a && rm -rf b".into())));
    }
}
