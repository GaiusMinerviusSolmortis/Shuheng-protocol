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

mod pty;
mod screen;
mod term;

use harness_core::codec::{self, CodecError, NdjsonReader, NextLine};
use harness_core::envelope::{Authority, Envelope, MessageType};
use harness_core::id::{IdGen, tag_of, water_from_env};
use harness_core::instance_event::InstanceEvent;
use harness_core::now_ms;
use harness_service::contract::ToolDecl;
use harness_service::domain_lock;
use harness_service::fence::Fence;
use harness_service::grant::{self, Grant};
use harness_service::reply;
use serde_json::{json, Value};
use term::Session;
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
    /// **这棵树的硬限额**（Job Object，内核执行 —— §5.8 决定二那张表里的"资源"面）。
    ///
    /// 默认全空 = 不限额。为什么默认不设：多严算"合理"是 **profile** 的事（待定 #3），
    /// 而"默认开一个我拍的数"会在正常需求上碰壁（一次大编译就可能撞上）。
    /// 打开之后 manifest 里那一面就从 `timeout` 变成 `limits` —— **声明跟着事实走**。
    limits: harness_service::job::Limits,
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
        limits: harness_service::job::Limits::default(),
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
            // **三个限额**：单位都取人最容易说对的那个（MB / 个数 / 毫秒）
            "--mem-limit-mb" => {
                if let Some(v) = it.next().and_then(|v| v.parse::<u64>().ok()) {
                    a.limits.mem_bytes = Some(v.saturating_mul(1024 * 1024));
                }
            }
            "--max-procs" => {
                if let Some(v) = it.next().and_then(|v| v.parse::<u32>().ok()) {
                    a.limits.max_procs = Some(v);
                }
            }
            "--cpu-time-ms" => {
                if let Some(v) = it.next().and_then(|v| v.parse::<u64>().ok()) {
                    a.limits.cpu_time_ms = Some(v);
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
        ids: IdGen::with_floor(tag_of(&args.instance), water_from_env()),
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
        "命令模式与会话模式",
        "§5.18 定了两种形态：\n\
         · **命令模式**（`open` 不带 `session`，用 `run`）：敲一条、等它结束、把输出给你。\n\
           每条命令都是**新起一个 `cmd /C`** —— 绝大多数场景用这个。\n\
         · **会话模式**（`open {session: true}`，用 `send` + `read`/`screen`）：\n\
           一条**常驻的 shell**（真的伪控制台），可以往里敲、可以看屏幕 ——\
           给长驻服务、REPL、进度条那种东西。\n\
         \n\
         **两种形态分开开**：命令模式的实例上没有可敲的东西（`send` 会明确拒绝，\
         并告诉你该怎么开）；会话模式的实例直接 `send` 就行。\n\
         它们是**两种实例**：同一个目录上，命令模式一个、会话模式一个，各自算一个\
         （`open` 的幂等键里带着模式）。所以别指望『我开过命令模式了，再 `open {session:true}`\
         会给我同一个』 —— 它会给你**新的一条会话**。\n\
         \n\
         ⚠️ **会话模式里 `cd` 管不住**：命令模式下 `cd` 是我们拦下来判围栏的，\
         会话模式里 `cd` 只是往 shell 敲的字，cwd 攥在**它**手里（没有它的 hook）。\
         要『只在托管路径内活动』，用命令模式。\n\
         \n\
         ⚠️ **看屏幕用 `screen`，别用 `read`**：交互式程序是**重写屏幕**而不是追加，\
         它们的输出流读出来是一堆光标移动转义序列，线性看毫无意义（§5.19）。",
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
        // 跑命令、以及往会话里敲字 = 在本机上执行东西，这是最需要资格的一件事
        "run" | "open" | "send" => &["proc.exec"],
        // 只读的、以及管理这个实例自身的，不需要额外资格
        "read" | "screen" | "close" | "status" | "ping" | "help" => &[],
        // 不认识的工具走"未知工具"那条路，不在这里管
        _ => &[],
    }
}

/// **本服务的工具名**：一份，不是两份（`没有名为 X 的工具` 那条消息要靠它教对的名字）。
/// 手写的那份原来漏了 `help`。有一条测试把它和 manifest 里真报的名字钉在一起。
const TOOL_NAMES: [&str; 9] = [
    "help", "open", "run", "read", "send", "screen", "close", "status", "ping",
];

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
            {"id": "two-modes", "title": "命令模式与会话模式"},
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
            // **2026-09-20 起它跟着事实走**：开了限额就报 `limits`，
            // 没开就还是只有超时那一条。声明不许比事实大，也不许比事实小。
            "resources": if args.limits.any() { "limits" } else { "timeout" }
        },
        "sandbox_note": if args.limits.any() {
            "**「时间」和「资源」这两面是真的**：`timeout_ms` 到点整棵树掐掉；\
             另外三个限额（内存 / 进程数 / CPU 时间）关在同一个 Job 上、**由内核执行**。\
             ⚠️ 文件系统 / 网络 / 身份 / 可执行文件**仍然没有限制** ——\
             它跑的命令和你自己在命令行里跑的是同一个世界。\
             我报的那个 `fence` 是**绑定范围**，**不是沙箱**。"
        } else {
            "**五个面里只有「时间」是真的**：`timeout_ms` 到点就整棵树掐掉。\
                          文件系统 / 网络 / 身份 / 可执行文件**都没有限制** ——\
                          它跑的命令和你自己在命令行里跑的是同一个世界。\
                          我报的那个 `fence` 是**绑定范围**（哪些路径归这个实例管），\
                          **不是沙箱**；把这两件事混着叫「围栏」是这里最容易骗人的地方。"
        },
        // **参数表与结果形状都是服务自己的义务**（规范 §11.1 的 C 组）：
        // 只报工具名不报参数，等于让人猜怎么调；不报结果形状，就没人能验它。
        "tools": tools(),
        "not_implemented": {
            "resize": "会话开起来之后**改不了尺寸**（ResizePseudoConsole 没接）—— \
                       开的时候把 `cols`/`rows` 给对",
            "osc133_hook": "**我们不替用户往 shell 里装 hook**（§5.18 那条：要在托管时\
                            显式启用，不能假设它已经在）。所以 `screen` 里的 `marks`\
                            多半是空的 —— 那是事实，不是故障",
            "screen_coverage": "屏幕仿真**只覆盖常用序列**（见 `screen` 的 `unknown_sequences`\
                                和 `unknown_sample`）：宽字符按 1 格算、滚动区域/行内插入删除\
                                没做。没做的那些**如实数出来**，不装作处理了",
            "session_on_other_platforms": "会话模式要伪控制台（ConPTY）—— **只在 Windows 上\
                                           实现**。别的平台上 `open {session: true}` 会\
                                           明确拒绝，而不是悄悄退化成命令模式",
            "session_cwd": "⚠️ **会话模式里的 `cd` 管不住。** 命令模式下 `cd` 是我们自己\
                            拦下来判围栏的（所以『只能在路径内 cd』）；会话模式里 `cd` 就是\
                            往 shell 敲的字，cwd 攥在**它**手里，我们没有它每一步的 hook\
                            （那就是 `osc133_hook` 那条）。所以会话模式的围栏只管\
                            **它从哪起**，不管它后来走到哪 —— 要『路径内不许出去』，用命令模式"
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
                                  "purpose": {"type": "string", "description": "这个终端是干什么用的（开的时候自己写）"},
                                  "session": {"type": "boolean",
                                              "description": "true = 开一条**常驻 shell**（会话模式，能用 send/screen）；默认 false = 命令模式"},
                                  "cols": {"type": "integer", "description": "会话模式的宽，默认 120"},
                                  "rows": {"type": "integer", "description": "会话模式的高，默认 30"}}}),
        )
        .caps(&["proc.exec"])
        .idempotent()
        .returns(json!({
            "instance_id": {"type": ["string", "null"]},
            "path": {"type": "string"},
            "purpose": {"type": ["string", "null"]},
            "idempotent": {"type": "boolean"},
            "mode": {"type": "string"},
            // 幂等地拿回旧实例时，**哪些要求没被满足**（现在只有一种：会话尺寸改不了）
            "note": {"type": ["string", "null"]},
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
            // **撞限额的推断**（2026-09-20）：只在真有迹象时给，而且是**推断**不是结论。
            // 它必须在这儿声明 —— 契约是闭合的，"结果里多一个字段"会被投递闸门拦下。
            "limits": {"type": ["string", "null"]},
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
            "send",
            "**往会话里敲字**（会话模式的实例）。默认补一个回车。",
            json!({"type": "object", "required": ["instance_id"], "additionalProperties": false,
                   "properties": {"instance_id": {"type": "string"},
                                  "text": {"type": "string",
                                           "description": "要敲的内容（默认空 = 只敲一个回车）"},
                                  "enter": {"type": "boolean",
                                            "description": "末尾补不补回车，默认 true"}}}),
        )
        .caps(&["proc.exec"])
        // ⚠️ **`send` 不是幂等的**（2026-09-20 改）：它是**敲键盘**，重复一次就是
        // 真的敲了两遍（`send {"text":"rm -rf build"}` 来两次 = 跑两遍）。
        // 第一版跟着邻居写了 `.idempotent()`，而中控的 `retry_advice` 拿这一格
        // 当"可以重试"的证据（§19.2：重试语义由声明决定，不由猜）—— 于是服务超时的
        // 那一次，中控会**鼓励模型把同一行命令再敲一遍**。
        // 声明错一格，代价是一条命令跑两遍。这条改成 destructive：不自动重试。
        .destructive()
        .returns(json!({
            "sent_bytes": {"type": "integer"},
            "enter": {"type": "boolean"},
            // 敲完之后该往哪看 —— 这句话得声明，**闭合的 schema 不许夹带**
            // （这一条是投递关当场抓出来的：我回了它却没报它）
            "note": {"type": "string"},
        }))
        .requires_on(&["confirmed"], &["sent_bytes"])
        .lifecycle("per_session")
        .to_json(),
        ToolDecl::new(
            "screen",
            "**看这块屏幕**（会话模式）：当前网格 + 光标 + shell 还在不在。\
             交互式程序只能这样看 —— `read` 给的是转义序列流。",
            json!({"type": "object", "required": ["instance_id"], "additionalProperties": false,
                   "properties": {"instance_id": {"type": "string"}}}),
        )
        .read_only()
        .idempotent()
        .returns(json!({
            "cols": {"type": "integer"},
            "rows": {"type": "integer"},
            "cursor": {"type": "object"},
            "text": {"type": "string"},
            "alive": {"type": "boolean"},
            "exit_code": {"type": ["integer", "null"]},
            "marks": {"type": "array"},
            "unknown_sequences": {"type": "integer"},
            "unknown_sample": {"type": ["string", "null"]},
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
            "send" => self.tool_send(&req, &args),
            "screen" => self.tool_screen(&req, &args),
            other => {
                reply::unknown_tool(other, &TOOL_NAMES)
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
                // 码跟着围栏自己的判断走（`path` 空了是参数错，越界才是被拒）
                return json!({"route": null, "effect": "refused", "code": e.code,
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

        // 同一个持有者在同一个目录上重复 open ⇒ 幂等（和编辑器同一条纪律）。
        //
        // ⚠️ **模式也算身份的一部分**（2026-09-20 实测抓出来的）：命令模式和会话模式
        // 能用到的工具是**完全不相交的两套**（`run` vs `send`/`screen`）。第一版把
        // 幂等键写成"谁 + 哪条路径"，于是**要会话的调用方会拿回一个命令模式的实例**
        // （回执里 mode 写着 command）—— 它下一次调 `send` 就被拒，而它明明收到过
        // 一个 `confirmed`。那是**答非所问**，比多开一个实例糟得多。
        // 加上模式之后：同一条路径、同一个持有者，最多两个实例（一边一个）——
        // 终端本来就不占跨实例锁，这不违反任何东西，反而给实例数**封了顶**。
        let holder = format!("{}@{}", req.from, req.sid);
        let want_session = args.get("session").and_then(Value::as_bool).unwrap_or(false);
        if let Some(t) = self
            .terms
            .values()
            .find(|t| same_instance(t, &dir, &holder, want_session))
        {
            // **尺寸对不上要说一声。** 会话开起来之后改不了尺寸（ResizePseudoConsole
            // 没接），所以这里不能装作"按你要的 200 列开了" —— 那是我们做不到的事。
            let note = match t.session.as_ref() {
                Some(s) => {
                    let (cols, rows) = s.screen.size();
                    let (want_cols, want_rows) = (
                        args.get("cols").and_then(Value::as_u64).unwrap_or(120) as usize,
                        args.get("rows").and_then(Value::as_u64).unwrap_or(30) as usize,
                    );
                    ((cols, rows) != (want_cols, want_rows)).then(|| {
                        format!(
                            "这条会话**已经开着**，尺寸是 {cols}x{rows}，改不了\
                             （ResizePseudoConsole 没接）—— 不是你写的 {want_cols}x{want_rows}"
                        )
                    })
                }
                None => None,
            };
            return json!({
                "route": null, "effect": "confirmed", "code": null,
                "instance_id": t.id, "path": dir.display().to_string(),
                "purpose": t.purpose, "idempotent": true, "note": note,
                // **如实说这个实例是哪一种**：命令模式还是会话模式
                "mode": if t.session.is_some() { "session" } else { "command" }
            });
        }

        self.next_term += 1;
        let id = format!("t-{}", self.next_term);
        // **实例自带一句用途说明**（开的时候自己写，§5.16）
        // 溢出文件按会话+实例分堆：它是**服务自己的状态**（不是用户的项目文件）
        let spill = self.args.spill_dir.join(&self.args.sid).join(format!("{id}.log"));
        let mut t = Terminal::new(
            id.clone(),
            dir.clone(),
            purpose.to_string(),
            holder.clone(),
            spill,
            self.args.memory_cap,
            self.args.limits,
        );
        // **会话模式**（§5.18）：`session: true` 时要的是一条**常驻的 shell**，
        // 而不是"每条命令新起一个 cmd /C"。默认不开 —— 命令模式是绝大多数场景，
        // 而开一条 PTY 是有代价的（一个常驻进程 + 一块屏幕）。
        if want_session {
            let cols = args.get("cols").and_then(Value::as_u64).unwrap_or(120) as u16;
            let rows = args.get("rows").and_then(Value::as_u64).unwrap_or(30) as u16;
            match Session::start(&dir, cols, rows, id.clone(), self.tx.clone()) {
                Ok(s) => t.session = Some(s),
                Err(e) => {
                    // **开不起来就说开不起来**（别退化成一个"看起来是会话模式"的东西）
                    return json!({
                        "route": null, "effect": "refused", "code": "invalid",
                        "why": format!("会话模式起不来：{e}"),
                        "remedy": "会话模式要伪控制台（ConPTY），只在 Windows 上有；\
                                   别的平台上用命令模式（`open` 不带 `session`）"
                    });
                }
            }
        }
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
            "purpose": purpose,
            // 开的哪一种，**回执里就说清**（后面 `send`/`screen` 只对会话模式有意义）
            "mode": if want_session { "session" } else { "command" },
            // 刚开的这一条没有"已经开着"这回事
            "note": Value::Null
        })
    }

    /// **会话模式的两个入口**（§5.18）：`send` 往里敲，`screen` 看那块网格。
    ///
    /// 为什么它们不能拿命令模式的实例凑合：命令模式每条命令新起一个 `cmd /C`，
    /// **没有可以"往里敲"的东西** —— 只能如实拒绝，并说清怎么开一个会话实例。
    fn tool_send(&mut self, req: &Envelope, args: &Value) -> Value {
        let id = args.get("instance_id").and_then(Value::as_str).unwrap_or("");
        if id.is_empty() {
            return reply::missing_instance_id("send");
        }
        let text = args.get("text").and_then(Value::as_str).unwrap_or("");
        // **回车默认加上**：对终端来说"敲一行"就是文本 + `\r`；
        // 要按别的键（比如 Ctrl-C）就显式给 `text` 并且 `enter: false`。
        let enter = args.get("enter").and_then(Value::as_bool).unwrap_or(true);
        let Some(t) = self.terms.get_mut(id) else {
            return reply::no_such_instance("send", id);
        };
        if let Some(v) = not_mine(req, t) {
            return v;
        }
        let Some(s) = t.session.as_mut() else {
            return json!({
                "route": null, "effect": "refused", "code": "invalid",
                "why": format!("`{id}` 是**命令模式**的实例 —— 没有可以往里敲的东西"),
                "remedy": "会话模式要开的时候就说明：`open {path, session: true}`。\
                           命令模式的实例只能跑整条命令（`run`）"
            });
        };
        let mut bytes = text.as_bytes().to_vec();
        if enter {
            bytes.push(b'\r');
        }
        match s.pty.send(&bytes) {
            Ok(()) => json!({
                "route": null, "effect": "confirmed", "code": null,
                // ⚠️ **平铺，不套 `data`**（2026-09-20 被投递关抓出来的）：
                // 声明里写的是 `sent_bytes` 这一格，结果我把它塞进了 `data` 里面 ——
                // 闸门当场判"少了必填的 `sent_bytes`"（整整一轮都在报）。
                // 这个服务里 `read`/`run`/`open` 都是平的，只有 `help`/`ping`/`status`
                // 用 `data`（它们是"看东西"的诊断入口）—— 跟着邻居写。
                "sent_bytes": bytes.len(), "enter": enter,
                "note": "敲进去了。终端不会马上回话 —— 用 `screen` 看它现在长什么样，\
                         或者用 `read` 看这之后的输出流"
            }),
            Err(e) => json!({
                "route": null, "effect": "refused", "code": "invalid",
                "why": format!("敲不进去：{e}"),
                "remedy": "多半是那个 shell 已经退了 —— `status` 看得见它还在不在；\
                           要接着用就重新 `open {session: true}`"
            }),
        }
    }

    fn tool_screen(&mut self, req: &Envelope, args: &Value) -> Value {
        let id = args.get("instance_id").and_then(Value::as_str).unwrap_or("");
        if id.is_empty() {
            return reply::missing_instance_id("screen");
        }
        let Some(t) = self.terms.get_mut(id) else {
            return reply::no_such_instance("screen", id);
        };
        if let Some(v) = not_mine(req, t) {
            return v;
        }
        let Some(s) = t.session.as_mut() else {
            return json!({
                "route": null, "effect": "refused", "code": "invalid",
                "why": format!("`{id}` 是**命令模式**的实例 —— 它没有屏幕，只有输出流"),
                "remedy": "命令模式读 `read`（那是流）；会话模式才有 `screen`（那是网格）。\
                           要看屏幕就 `open {path, session: true}`"
            });
        };
        let alive = s.pty.alive();
        let (cols, rows) = s.screen.size();
        let (row, col) = s.screen.cursor();
        let marks: Vec<Value> = s
            .screen
            .marks
            .iter()
            .rev()
            .take(8)
            .map(|m| json!({"kind": m.kind.to_string(), "exit": m.exit}))
            .collect();
        json!({
            "route": null, "effect": "confirmed", "code": null,
            // 同上：**平铺**（声明里就是这么写的）
            "cols": cols, "rows": rows,
            // 光标是 **0 起**的（跟网格一致），调用方别自己 ±1
            "cursor": {"row": row, "col": col},
            "text": s.screen.text(),
            // **shell 还在不在**：这是事实，不是推断（§5.20 的"进程退出"那条触发）
            "alive": alive,
            "exit_code": if alive { Value::Null } else { json!(s.pty.exit_code()) },
            // OSC 133 的标记（对端没装 shell hook 就是空的 —— 那也是事实）
            "marks": marks,
            // **没认出来的转义序列有几条** —— 如实报，别让模型以为
            // 它看到的就是全部（§5.19 那条"对交互式程序 read 基本没用"的同源）
            "unknown_sequences": s.screen.unknown,
            "unknown_sample": s.screen.unknown_sample,
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

        // **实例先于 cmd**（顺序有讲究，同编辑器 `revert` 那条）：
        // 没有终端就谈不上"往哪个终端里敲命令"，而两次抱怨会拖成两次往返。
        if id.is_empty() {
            let reply = self.mint(
                MessageType::Result,
                &req.from.clone(),
                req.corr.clone(),
                reply::missing_instance_id("run"),
            );
            emit(&mut out, &reply);
            return;
        }
        let Some(t) = self.terms.get(id) else {
            let reply = self.mint(
                MessageType::Result,
                &req.from.clone(),
                req.corr.clone(),
                reply::no_such_instance("run", id),
            );
            emit(&mut out, &reply);
            return;
        };
        // **会话模式的实例上不跑 `run`**：那会**在外面另起一个 shell**，
        // 而模型会以为"我在这条会话里跑了一条命令" —— 两个 shell 两个 cwd，
        // 这个混淆比直接拒掉糟得多。
        if t.session.is_some() {
            let reply = self.mint(
                MessageType::Result,
                &req.from.clone(),
                req.corr.clone(),
                json!({
                    "route": null, "effect": "refused", "code": "invalid",
                    "why": format!("`{id}` 是**会话模式**的实例 —— `run` 会在外面另起一个 shell"),
                    "remedy": "往这条会话里敲：`send {instance_id, text}`；看它现在什么样：`screen`。\
                               要跑整条命令就开一个命令模式的实例（`open` 不带 `session`）"
                }),
            );
            emit(&mut out, &reply);
            return;
        }
        if cmd.trim().is_empty() {
            let reply = self.mint(
                MessageType::Result,
                &req.from.clone(),
                req.corr.clone(),
                reply::empty_argument("run", "cmd", "{\"cmd\": \"dir /b\"}（**一次一条**，别用 && 串）"),
            );
            emit(&mut out, &reply);
            return;
        }
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
        // **会话模式还要喂屏幕**（§5.19）：`read` 看流、`screen` 看**网格** ——
        // 交互式程序重写屏幕，线性读它的输出流毫无意义。
        if let Some(s) = t.session.as_mut() {
            s.screen.feed_text(text);
        }
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
        // **它是不是撞了限额？** —— 内核执行的限额**不会**直接告诉我们"是我杀的"
        // （那要完成端口通知，没做）。所以这里事后查两个计数器，
        // 然后**按推断说话**：措辞是"很可能"，证据（峰值 / 个数）一起给出来。
        // 推断就是推断 —— 写成结论的话，模型会拿它当事实去排查。
        let limit_note = limits_note(&self.args.limits, &running.account);
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
        // 限额那条判词：**只在真有迹象时加**，而且明说它是推断
        if let (Some(note), Some(obj)) = (limit_note, payload.as_object_mut()) {
            obj.insert("limits".into(), json!(note));
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
        if id.is_empty() {
            return reply::missing_instance_id("read");
        }
        let Some(t) = self.terms.get_mut(id) else {
            return reply::no_such_instance("read", id);
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
        if id.is_empty() {
            return reply::missing_instance_id("close");
        }
        if let Some(t) = self.terms.get(id) {
            if let Some(v) = not_mine(req, t) {
                return v;
            }
        }
        let Some(mut t) = self.terms.remove(id) else {
            return reply::no_such_instance("close", id);
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
                    // **它是哪一种**：命令模式（`run`）还是会话模式（`send`/`screen`）。
                    // 中控画图要按这个分节点（会话模式那个节点背后**真有一个活进程**）。
                    "mode": if t.session.is_some() { "session" } else { "command" },
                    // 会话模式才有：那条 shell 的 pid（命令模式没有常驻进程，如实 null）
                    "pid": t.session.as_ref().map(|s| s.pty.pid()),
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
/// **它是不是撞了限额？** —— 返回一句**推断**（不是结论）。
///
/// ⚠️ **2026-09-20 第一版写错了，这里记下来**：当时写的是"峰值 ≥ 上限的 95%
/// ⇒ 很可能撞了内存上限"。那是**把三个限额当成一种发作方式**了 ——
/// 它们其实不一样（见 `job.rs` 文件头那张表）：
///
/// * **CPU 时间**超了 ⇒ **内核终止整个 job**（这一条**是杀**，证据也硬：
///   用掉的 CPU 时间会顶到上限）⇒ 可以说"撞了"，并说清是内核干的；
/// * **进程数**超了 ⇒ 再创建就**失败**（命令自己报错），累计数会顶到上限
///   ⇒ 只给一句弱提示；
/// * **内存**超了 ⇒ **命令自己分配失败**，**内核根本没杀它** ⇒
///   只报测量值（离上限多远），**不说"它被杀了"** —— 那句话是假的。
///
/// 两半分开：**测量的数每次都报**（那是事实），**推断只在有证据时说**。
fn limits_note(
    limits: &harness_service::job::Limits,
    account: &harness_service::job::JobAccount,
) -> Option<String> {
    if !limits.any() {
        return None;
    }
    let mut lines: Vec<String> = Vec::new();
    if let Some(cap) = limits.mem_bytes {
        lines.push(format!(
            "峰值内存 {:.1} MB（上限 {:.1} MB）",
            account.peak_mem_bytes as f64 / 1_048_576.0,
            cap as f64 / 1_048_576.0
        ));
    }
    if let Some(n) = limits.max_procs {
        lines.push(format!(
            "一共起过 {} 个进程（上限 {n}）",
            account.total_processes
        ));
    }
    if let Some(ms) = limits.cpu_time_ms {
        lines.push(format!(
            "用掉 {} ms CPU 时间（上限 {ms} ms）",
            account.cpu_time_ms
        ));
    }
    let mut out = format!("这次运行的账：{}。", lines.join("；"));
    // **只有 CPU 那条是"杀"**，所以只有它敢说"撞了"
    if let Some(ms) = limits.cpu_time_ms {
        if ms > 0 && account.cpu_time_ms >= ms / 100 * 95 {
            out.push_str(
                "**撞了 CPU 时间上限** —— 这一条是**内核到点终止整棵树**（不是它自己退的），\
                 所以那条命令没有跑完。",
            );
            return Some(out);
        }
    }
    if let Some(n) = limits.max_procs {
        if n > 0 && account.total_processes >= n {
            out.push_str(
                "进程数顶到上限了：**再想创建进程会失败**（命令自己会报错），\
                 而这一条**不是**被杀的 —— 是它创建不出来。",
            );
        }
    }
    if let Some(cap) = limits.mem_bytes {
        if cap > 0 && account.peak_mem_bytes >= cap / 100 * 95 {
            out.push_str(
                "内存峰值贴着上限：**命令多半分配失败了**（`malloc`/`new` 返回不了）——\
                 ⚠️ 这一条**不是内核杀它**，内存限额只挡分配。",
            );
        }
    }
    Some(out)
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

/// **这条实例是不是"这次 `open` 想要的那一个"？**
///
/// 三个都要对上：**持有者 + 目录 + 模式** —— `open` 的幂等键就是这个三元组。
///
/// 为什么模式也在键里（2026-09-20 实测）：两种模式的可调工具**完全不相交**
/// （`run` vs `send`/`screen`）。把模式漏在键外，要会话的调用方就会拿回一个
/// **命令模式**的实例 —— 回执是 `confirmed`，下一次调用却被拒。**答非所问。**
fn same_instance(t: &Terminal, dir: &std::path::Path, holder: &str, session: bool) -> bool {
    t.cwd == dir && t.holder == holder && t.session.is_some() == session
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

    /// **声明的能力和强制的必须逐条对得上**（2026-09-20）—— 同编辑器那条，
    /// 理由写在那边：两张表漂了就是"声明了 `proc.exec`、实际什么都没执行"的静默洞。
    #[test]
    fn the_enforcement_table_matches_what_we_declare() {
        for t in tools() {
            let name = t["name"].as_str().expect("每个工具都有名字");
            let declared: Vec<&str> = t["capabilities"]
                .as_array()
                .expect("每个工具都报到 capabilities")
                .iter()
                .filter_map(Value::as_str)
                .collect();
            let enforced: Vec<&str> = required_caps(name).to_vec();
            assert_eq!(
                declared, enforced,
                "`{name}` 的声明和执行对不上：{declared:?} vs {enforced:?}"
            );
        }
    }

    /// **报错里教的名字 == manifest 里报的名字**（同编辑器那条）：
    /// 手写的那份原来漏了 `help`。
    #[test]
    fn the_error_message_lists_exactly_the_tools_we_declare() {
        let declared: Vec<String> = tools()
            .iter()
            .filter_map(|t| t["name"].as_str().map(str::to_string))
            .collect();
        let taught: Vec<String> = TOOL_NAMES.iter().map(|s| s.to_string()).collect();
        assert_eq!(declared, taught, "报错里教的名字和 manifest 对不上");
    }

    /// **`open` 的幂等键里有"模式"这一格**（2026-09-20 实测）。
    ///
    /// 漏了它的后果不是"多开一个实例"，而是**要会话的调用方拿回一个命令模式的实例**：
    /// 回执 `mode: command`、`effect: confirmed`，然后它调 `send` 被拒 ——
    /// 而它没有任何理由怀疑自己拿到的不是会话。所以这条规则要用测试钉死：
    /// **同一条路径、同一个持有者，模式不同就是两个实例。**
    #[test]
    fn the_open_key_includes_the_mode() {
        let dir = std::env::temp_dir();
        let holder = "agent:dada@s-1";
        // 命令模式那个（`Terminal::new` 出来就是命令模式）
        let cmd = a_terminal(holder);
        assert_eq!(cmd.cwd, dir, "测试的前提：`a_terminal` 的 cwd 就是临时目录");
        // 会话模式那个：真起一条 PTY（起不来就跳过 —— 同 `pty.rs` 那两条的纪律）
        let (tx, _rx) = mpsc::channel();
        let Ok(sess) = Session::start(&dir, 80, 24, "t-2".into(), tx) else {
            eprintln!("PTY 起不来（环境问题，跳过）");
            return;
        };
        let mut ses = a_terminal(holder);
        ses.session = Some(sess);

        // 模式对上，才是"已经开着"的那一个（幂等）
        assert!(same_instance(&cmd, &dir, holder, false));
        assert!(same_instance(&ses, &dir, holder, true));
        // 模式不对上 ⇒ **不是同一个实例**（各自开各自的）
        assert!(
            !same_instance(&cmd, &dir, holder, true),
            "要会话却认出命令模式那条 ⇒ 答非所问"
        );
        assert!(!same_instance(&ses, &dir, holder, false));
        // 换个人、换条路径，都不是
        assert!(!same_instance(&ses, &dir, "agent:bob@s-1", true));
        assert!(!same_instance(&ses, &dir.join("nope"), holder, true));
    }

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
            harness_service::job::Limits::default(),
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
            // 这一条测的是"**没开限额**时 manifest 也合规"（`resources: timeout`）
            limits: harness_service::job::Limits::default(),
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
