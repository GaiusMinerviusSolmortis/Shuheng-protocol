//! **记忆服务**：把 08-记忆 §16 那套"沉淀"做成一个能被中控拉起的工具实例。
//!
//! 它回答的是原始设想四块里最后没动的那一块 —— **记忆系统**。
//! 但它**不是**智能体进程里的那个线性窗口（那是 `harness-memory`，见 lib.rs 的对照表）：
//!
//! ```text
//!   线性记忆（每轮都在算，一次会话）  ──沉淀──▶  这个服务（跨会话，挂锚点）
//! ```
//!
//! 三条从别处偷来的纪律，在这里各落一处：
//!
//! | 纪律 | 落点 |
//! |---|---|
//! | **ETag**（HTTP）| `recall` 支持 `if_none_match`：没变就别把内容再倒一遍 |
//! | **TTL**（DNS）| 每条碎片可以自带寿命；过期是"标出来"，不是删除 |
//! | **遗嘱**（MQTT）| 服务崩了要说清"账在哪儿、找谁要"—— 见 `will` |

use harness_core::codec::{self, NdjsonReader, NextLine};
use harness_core::envelope::{Authority, Envelope, MessageType};
use harness_core::id::{IdGen, tag_of, water_from_env};
use harness_core::now_ms;
use harness_ipc::{Listener, Rendezvous};
use harness_mem::store::{NewFrag, Query, Store, EPISTEMIC, MAX_TEXT_BYTES};
use harness_service::contract::ToolDecl;
use harness_service::domain_lock;
use harness_service::grant::{self, Grant};
use serde_json::{json, Value};
use std::io::{self, BufReader};
use std::path::PathBuf;

#[derive(Clone)]
struct Args {
    instance: String,
    sid: String,
    kind: String,
    store: PathBuf,
    /// 默认寿命：`0` = 永不过期。默认给一个（30 天）而**不是**永远 ——
    /// 因为"永远"是那种看起来安全、实际把库变成垃圾场的选择。
    default_ttl_ms: i64,
    /// **常驻**：不听 stdin、不吐 stdout，而是听在一个回环端口上，
    /// 并把"我在哪、口令是什么"写进这个目录 —— 让**后来的会话**接上同一个进程。
    resident: Option<PathBuf>,
}

fn parse_args() -> Result<Args, String> {
    let mut instance = String::from("mem:1");
    let mut sid = String::from("s-1");
    let mut kind = String::from("mem");
    let mut store = PathBuf::from("data/memory");
    let mut default_ttl_ms: i64 = 30 * 24 * 3600 * 1000;
    let mut resident: Option<PathBuf> = None;
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--instance" => instance = it.next().unwrap_or(instance),
            "--sid" => sid = it.next().unwrap_or(sid),
            "--kind" => kind = it.next().unwrap_or(kind),
            "--store" => {
                store = it
                    .next()
                    .map(PathBuf::from)
                    .ok_or_else(|| "--store 后面要跟一个目录".to_string())?
            }
            "--default-ttl-ms" => {
                default_ttl_ms = it
                    .next()
                    .and_then(|v| v.parse().ok())
                    .ok_or_else(|| "--default-ttl-ms 后面要跟一个毫秒数（0 = 不过期）".to_string())?
            }
            "--resident" => {
                resident = Some(
                    it.next()
                        .map(PathBuf::from)
                        .ok_or_else(|| "--resident 后面要跟一个会合目录".to_string())?,
                )
            }
            other => return Err(format!("不认识的参数 `{other}`；--help 看用法")),
        }
    }
    if default_ttl_ms < 0 {
        return Err("`--default-ttl-ms` 不能是负数（0 表示永不过期）".into());
    }
    Ok(Args {
        instance,
        sid,
        kind,
        store,
        default_ttl_ms,
        resident,
    })
}

fn main() {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("[mem] 参数不对：{e}");
            std::process::exit(2);
        }
    };

    // **一处碎片库只许一个进程写。**
    //
    // 这是从编辑器那件事学来的（README 阶段 2 第 2 条）：当时"两个编辑器进程 =
    // 两张锁表 = 写互斥失效"，而修法不是把锁上移到中控（那等于让内核懂工具语义），
    // 而是**同一个域只允许一个服务进程**。碎片库同理：两个 mem 进程写同一个目录，
    // 就会有两份内存视图、两套序号 —— 迟早撞出两条一样的碎片。
    let store = match Store::open(&args.store) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[mem] 碎片库打不开：{}。{}", e.why, e.remedy);
            std::process::exit(3);
        }
    };

    // **常驻那条路**：不听 stdin、不吐 stdout —— 它听套接字，谁带口令谁来连。
    // 为什么要这样：stdio 是**父子私有**的，中控一收摊那条通道就没了；
    // 常驻的意义正是"活过这个会话"，所以它必须有一条**别的会话也能接上**的通道。
    if let Some(dir) = args.resident.clone() {
        serve_resident(args, store, dir);
        return;
    }

    let _lock = match domain_lock::acquire("mem", std::slice::from_ref(&args.store)) {
        Ok(l) => l,
        Err(c) => {
            // **降级而不是退出**：照常在线、不接活、只回答诊断那几句。
            // （和编辑器同一条纪律：撞锁不是"死"，是"我不该接活"。）
            eprintln!("[mem] 域锁撞上了：{}。{}", c.why, c.remedy);
            degraded_loop(&args);
            return;
        }
    };

    let mut ids = IdGen::with_floor(tag_of(&args.instance), water_from_env());
    let mut out = io::stdout();
    eprintln!(
        "[mem] 碎片库：{}（默认寿命 {}）",
        store.dir().display(),
        if args.default_ttl_ms == 0 {
            "永不过期".to_string()
        } else {
            format!("{} ms", args.default_ttl_ms)
        }
    );

    emit(
        &mut out,
        &Envelope {
            id: ids.next_message(),
            ts: now_ms(),
            kind: MessageType::Presence,
            from: args.instance.clone(),
            to: "ctrl".into(),
            corr: None,
            sid: args.sid.clone(),
            authority: Authority::Observe,
            payload: json!({"state": "online", "manifest": manifest(&args)}),
        },
    );

    let mut reader = NdjsonReader::new(BufReader::new(io::stdin()));
    loop {
        match reader.next_line_checked() {
            Ok(NextLine::Line(line)) => {
                // **被请停就真的停**（哪怕这条是 stdio 那条路）——
                // 中控的 `stop` 走的正是这个入口，所以这里必须认它。
                if let After::Shutdown = handle_line(&mut out, &mut ids, &args, &store, &line) {
                    eprintln!("[mem] 被请停了（stdio 那条路），走了");
                    break;
                }
            }
            Ok(NextLine::Invalid { bytes }) => {
                eprintln!("[mem] 跳过一行非 UTF-8 的输入（{bytes} 字节）")
            }
            Ok(NextLine::Eof) => break,
            Err(e) => {
                eprintln!("[mem] 读 stdin 失败：{e}");
                break;
            }
        }
    }

    emit(
        &mut out,
        &Envelope {
            id: ids.next_message(),
            ts: now_ms(),
            kind: MessageType::Presence,
            from: args.instance.clone(),
            to: "ctrl".into(),
            corr: None,
            sid: args.sid.clone(),
            authority: Authority::Observe,
            payload: json!({"state": "offline"}),
        },
    );
}

/// 撞了域锁之后的样子：**在线、但只回答诊断**。
fn degraded_loop(args: &Args) {
    let mut ids = IdGen::with_floor(tag_of(&args.instance), water_from_env());
    let mut out = io::stdout();
    emit(
        &mut out,
        &Envelope {
            id: ids.next_message(),
            ts: now_ms(),
            kind: MessageType::Presence,
            from: args.instance.clone(),
            to: "ctrl".into(),
            corr: None,
            sid: args.sid.clone(),
            authority: Authority::Observe,
            payload: json!({
                "state": "degraded",
                "manifest": manifest(args),
                "why": format!("同一个碎片库上已经有另一个 mem 进程了（{}）", args.store.display()),
            }),
        },
    );
    let mut reader = NdjsonReader::new(BufReader::new(io::stdin()));
    while let Ok(NextLine::Line(line)) = reader.next_line_checked() {
        let Ok(req) = codec::decode(&line) else { continue };
        if req.kind != MessageType::Request {
            continue;
        }
        let tool = req.payload.get("tool").and_then(Value::as_str).unwrap_or("");
        let payload = if matches!(tool, "ping" | "status" | "help") {
            json!({"route": null, "effect": "confirmed", "code": null, "degraded": true,
                   "degraded_reason": "同一个碎片库上已经有另一个 mem 进程 —— 我不接活",
                   "data": {"degraded": true,
                            "why": "同一个碎片库只许一个进程写（否则两份内存视图、两套序号，迟早撞出重复碎片）",
                            "store": args.store.display().to_string()}})
        } else {
            json!({"route": null, "effect": "refused", "code": "refused",
                   "why": "我在 degraded 状态：同一个碎片库上已经有另一个 mem 进程",
                   "remedy": "同一个碎片库上**已经有另一个 mem 进程**了 —— 它才是这个库的主人。\
                              要么去用它，要么换一个 `--store` 目录重开一个。\
                              **别在这儿重试**：不是偶尔撞上，是本来就轮不到我写"})
        };
        emit(
            &mut out,
            &Envelope {
                id: ids.next_message(),
                ts: now_ms(),
                kind: MessageType::Result,
                from: args.instance.clone(),
                to: req.from.clone(),
                corr: req.corr.clone(),
                sid: args.sid.clone(),
                authority: Authority::Observe,
                payload,
            },
        );
    }
}

/// **常驻那条路**：听套接字、写会合文件、**服务多个会话**（2026-09-20）。
///
/// 三件事和 stdio 那条路不同，每一件都有理由：
///
/// | | stdio 那条 | 常驻这条 |
/// |---|---|---|
/// | 通道 | 父子私有（中控收摊就断） | **回环套接字 + 口令**：别的会话能接上 |
/// | 什么时候退 | stdin EOF | **只有被请停（`shutdown`）或进程被杀** —— 会话结束不算 |
/// | 谁在用 | 一个会话 | **多个会话同时**（各自一条连接，共享同一个 `Store`）|
///
/// 最后一条是这一整块的意义：**"记忆跨会话"从"新进程读同一个目录"变成了
/// "同一个进程服务多个会话"** —— 内存里的东西不用重新读一遍。
fn serve_resident(args: Args, store: Store, dir: PathBuf) {
    // 域锁还是要的：**一处碎片库只许一个进程写**（这条不因为常驻而变）。
    // 常驻只是让"那一个进程"活得更久，不是让"可以有两个"。
    let _lock = match domain_lock::acquire("mem", std::slice::from_ref(&args.store)) {
        Ok(l) => l,
        Err(c) => {
            eprintln!("[mem] 域锁撞上了：{}。{}", c.why, c.remedy);
            std::process::exit(3);
        }
    };
    let listener = match Listener::bind(&args.instance) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("[mem] 端口听不上：{e}");
            std::process::exit(3);
        }
    };
    let rv = Rendezvous::new(&args.instance, &args.kind, listener.addr(), listener.token());
    if let Err(e) = rv.write(&dir) {
        eprintln!("[mem] 会合文件写不出来（{}）：{e}", dir.display());
        std::process::exit(3);
    }
    let m = manifest(&args);
    eprintln!(
        "[mem] **常驻**：{} @ {}（会合文件在 {}）—— 会话结束不收我",
        args.instance,
        rv.addr,
        dir.display()
    );

    // 共享的东西：碎片库、清单、会话计数。**每个连接一条线程**，
    // 各自有自己的消息 id 生成器（tag 里带连接号，跨连接不会撞 id）。
    let store = std::sync::Arc::new(store);
    let manifest = std::sync::Arc::new(m);
    let served = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let started = rv.started_ms;
    loop {
        let conn = match listener.accept() {
            Ok(c) => c,
            Err(e) => {
                // **口令不对不是致命错误**：说清是哪条连接被拒了，继续听
                eprintln!("[mem] 拒了一条连接：{e}");
                continue;
            }
        };
        let n = served.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
        let store = store.clone();
        let manifest = manifest.clone();
        let dir = dir.clone();
        let instance = args.instance.clone();
        let sid = args.sid.clone();
        let args_c = args.clone();
        std::thread::spawn(move || {
            let mut ids = IdGen::new(format!("{instance}-c{n}"));
            let mut conn = conn;
            eprintln!("[mem] 会话 {n} 接上了（共服务过 {n} 个）");
            // **每一路上来都先自报家门**：接上来的那个会话也要看见 manifest
            let hello = Envelope {
                id: ids.next_message(),
                ts: now_ms(),
                kind: MessageType::Presence,
                from: instance.clone(),
                to: "ctrl".into(),
                corr: None,
                sid: sid.clone(),
                authority: Authority::Observe,
                payload: json!({
                    "state": "online",
                    "manifest": *manifest,
                    // 这两样是"我是不是上次那个进程"的凭据（smoke 就靠它）
                    "resident": true,
                    "started_ms": started,
                    "sessions_served": n,
                }),
            };
            emit(&mut conn, &hello);
            loop {
                match conn.read_line() {
                    Ok(Some(line)) => {
                        if line.trim().is_empty() {
                            continue;
                        }
                        let Ok(env) = codec::decode(&line) else {
                            eprintln!("[mem] 收到不合规的消息，跳过");
                            continue;
                        };
                        match env.kind {
                            MessageType::Request => {
                                let after =
                                    handle_request(&mut conn, &mut ids, &args_c, &store, &env);
                                if let After::Shutdown = after {
                                    // **把会合文件撤掉再走**：不然下一个会话会以为我还在
                                    let _ = harness_ipc::Rendezvous::remove(&dir, &instance);
                                    eprintln!("[mem] 被请停了：会合文件撤掉，走了");
                                    std::process::exit(0);
                                }
                            }
                            MessageType::Cancel => {
                                let payload = json!({"route": null, "effect": "confirmed",
                                    "code": null,
                                    "why": "本地读写已经做完了 —— 没有可取消的中间态",
                                    "data": {"canceled": false}});
                                reply_to(&mut conn, &mut ids, &args_c, &env, payload);
                            }
                            _ => {}
                        }
                    }
                    Ok(None) => {
                        // **一个会话走了而已，我不走** —— 这就是常驻。
                        eprintln!("[mem] 会话 {n} 断开了（我留着）");
                        return;
                    }
                    Err(e) => {
                        eprintln!("[mem] 读套接字失败：{e}");
                        return;
                    }
                }
            }
        });
    }
}

/// 处理完一条请求之后该干什么。
enum After {
    Keep,
    /// 有人（只能是中控）请我停
    Shutdown,
}

fn emit(out: &mut impl io::Write, env: &Envelope) {
    match codec::encode(env) {
        Ok(line) => {
            let _ = out.write_all(line.as_bytes());
            let _ = out.write_all(b"\n");
            let _ = out.flush();
        }
        Err(e) => eprintln!("[mem] 发不出去：{e}"),
    }
}

/// **每个工具要什么能力** —— 工具自己接口的一部分。
///
/// 注意这里和编辑器的 `fs.read` / `fs.write` 是同一套写法：
/// **令牌是纯标签，范围不在令牌上**（§11.6 洞 #2 的答案）。
/// 记忆这一层的"范围"是**锚点** —— 令牌说"这事属于写这一类"，
/// 锚点说"写在哪一本账上"，两样合起来才是完整的授权（§7）。
fn required_caps(tool: &str) -> &'static [&'static str] {
    match tool {
        "deposit" => &["mem.write"],
        // ⚠️ **这里原来还有 `"unpin"`** —— 而**没有那个工具**（`pin` 用 `pinned: false` 表达拔钉）。
        // 一条对着不存在工具的分支不会出错，但它说明**这张表和工具表是分开维护的** ——
        // 而那种分开维护迟早会漂成"声明了 `fs.write`、实际什么都没执行"的静默洞
        // （门只查声明那一侧，看不见强制那一侧）。
        // 现在每个服务都有一条测试把两张表逐条钉住。
        "pin" | "forget" => &["mem.curate"],
        "recall" | "list" => &["mem.read"],
        // 零能力：出问题的时候得有人能问它一句 —— 诊断通道不能跟着鉴权一起坏
        // 零能力：诊断那几句永远放行（"出问题的时候得有人能问它一句"）。
        // `shutdown` 也在这儿，但它**另外还有一道门**：只认"没经过网关 + 发件人是中控"
        // （见 `handle_request`）—— 零能力不等于谁都能关它。
        "help" | "ping" | "status" | "shutdown" => &[],
        _ => &[],
    }
}

fn manifest(args: &Args) -> Value {
    json!({
        "contract_version": "1",
        "id": "mem",
        "domain": "memory",
        "version": env!("CARGO_PKG_VERSION"),
        "title": "记忆（沉淀式）",
        "kind": args.kind,
        "platforms": ["windows", "linux", "macos"],
        "node_kind": "memory",
        "state_vocabulary": ["idle"],
        "content": ["log"],
        "node_actions": ["interrupt", "close"],
        "instance_types": [],
        // ⚠️ **2026-09-20 补**：README 里那句"这一层没做的，也写在 `not_implemented` 里"
        // 曾经**没有对应的实现** —— 我说了它写了，可 manifest 里根本没这个字段。
        // 这正是这个项目最在意的那类谎（"文档说做了、代码里没有"），
        // 而它是我自己在写 README 时留下的。
        "not_implemented": {
            "recall": "召回是**字面匹配**，不是语义检索 —— 同义改写认不出来。\
                       所以「没召回出来」不等于「这事没发生过」（§16.6 降级必须显式）",
            "anchors": "**锚点不建实例**（`instance_types` 是空的）：图上看不到\
                        「这次会话落在哪本账上」。要它就得再定一套锚点的生命周期"
        },
        "summary": "跨会话的碎片库：按**锚点**存，每条带「谁写的、哪次会话」",
        "when_to_use": "会话结束 / 切项目时把值得留的结论沉下来；下次来先 recall 一遍，\
                        别让同一个坑踩第二遍",
        "injection_policy": "card",
        "will": "我的账在 `--store` 那个目录里，**碎片是只追加的 JSONL** —— \
                 我没了账还在，换一个进程指着同一个目录就能接着读。\
                 没有「写到一半丢」这回事：每条写进去之前都 flush 过。",
        "help_sections": [
            {"id": "overview", "title": "概览：这一层是干什么的"},
            {"id": "anchor",   "title": "锚点：每条记忆挂在哪儿"},
            {"id": "deposit",  "title": "怎么写一条碎片"},
            {"id": "recall",   "title": "怎么读，以及 ETag 和 TTL"},
            {"id": "errors",   "title": "错误码与补救"}
        ],
        "tools": tools(args),
        "error_codes": ["invalid", "refused", "busy", "canceled"],
        "owns": {"domain": "memory-store", "exclusive_per": "store"}
        // **它不碰你的文件**：碎片写在它自己的 `--store` 目录里，
        // 而那是它的**域**（一处只许一个进程），不是"围栏"。
        ,"sandbox": {
            "filesystem": "none",
            "network": "none",
            "identity": "same-user",
            "executables": "none",
            "resources": "none"
        },
        "sandbox_note": "**它根本不动你的文件** —— 只读写自己那个 `--store` 目录。\
                          所以这里没有「围栏」要报（`fence` 是空的，这是真的）。\
                          `filesystem: none` 的意思不是「没管住」，而是「**没有你给它的路径可管**」。"
    })
}

/// **本服务的工具名**：一份，不是两份。
///
/// `没有名为 X 的工具` 那条消息要靠它教模型"对的名字"；
/// 而 `tools()` 报进 manifest 的名字也是这几个 —— 两者有一条测试钉着。
/// （`shutdown` 在里面：它是**宿主专属**的，被拒的时候那句判词会解释为什么。）
const TOOL_NAMES: [&str; 9] = [
    "help", "ping", "status", "deposit", "recall", "list", "pin", "forget", "shutdown",
];

fn tools(args: &Args) -> Vec<Value> {
    // （名字表在下面 `TOOL_NAMES`；`tools()` 报出去的名字和它有一条测试钉着）
    // **常驻的服务，工具也报 `resident`** —— 闸门要求一个服务里的工具
    // 对 lifecycle 的说法一致（同一个进程不可能有两种寿命）。
    let lc: &'static str = if args.resident.is_some() {
        "resident"
    } else {
        "per_session"
    };
    let ttl_note = if args.default_ttl_ms == 0 {
        "默认：**永不过期**（这个服务被这么起的）".to_string()
    } else {
        format!(
            "默认：{} ms（这个服务被这么起的）—— 不传 `ttl_ms` 就用它",
            args.default_ttl_ms
        )
    };
    vec![
        ToolDecl::new(
            "help",
            "这本手册本身：不带参数看章节清单，带 section 看那一章。",
            json!({"type": "object", "additionalProperties": false,
                   "properties": {"section": {"type": "string"}}}),
        )
        .read_only()
        .idempotent()
        .lifecycle(lc)
        .requires_on(&["confirmed"], &["data"])
        .to_json(),
        ToolDecl::new(
            "ping",
            "看看它还在不在。",
            json!({"type": "object", "additionalProperties": false, "properties": {}}),
        )
        .read_only()
        .idempotent()
        .lifecycle(lc)
        .requires_on(&["confirmed"], &["data"])
        .to_json(),
        ToolDecl::new(
            "status",
            "看这个碎片库：它在哪、有几个锚点、多少条、几条过期的。",
            json!({"type": "object", "additionalProperties": false, "properties": {}}),
        )
        .read_only()
        .idempotent()
        .lifecycle(lc)
        .requires_on(&["confirmed"], &["data"])
        .to_json(),
        ToolDecl::new(
            "deposit",
            "往一个锚点上写一条碎片。同一条话写两遍会被认出来（幂等）。",
            json!({"type": "object", "required": ["anchor", "text"], "additionalProperties": false,
                   "properties": {
                       "anchor": {"type": "string",
                                  "description": "挂在哪儿：项目路径 / 文件内容哈希 / 符号名"},
                       "text": {"type": "string", "description": "一句话，最多 8000 字节"},
                       "epistemic": {"enum": EPISTEMIC,
                                     "description": "可信度分类：事实 / 决定 / 经验 / 观点 / 指针"},
                       "tags": {"type": "array", "items": {"type": "string"}},
                       "ttl_ms": {"type": "integer",
                                  "description": format!("寿命（毫秒）；0 = 不过期。{ttl_note}")}
                   }}),
        )
        .caps(&["mem.write"])
        .lifecycle(lc)
        .returns(json!({"data": {"type": "object"}}))
        .requires_on(&["confirmed"], &["data"])
        .to_json(),
        ToolDecl::new(
            "recall",
            "从锚点上取碎片：钉住的先给，其余按新的在前；支持 if_none_match（没变就别倒一遍）。",
            json!({"type": "object", "required": ["anchor"], "additionalProperties": false,
                   "properties": {
                       "anchor": {"type": "string"},
                       "query": {"type": "string",
                                 "description": "**字面匹配**（不是语义匹配）—— 命中文本或 tag"},
                       "epistemic": {"enum": EPISTEMIC},
                       "limit": {"type": "integer", "description": "只挤非钉住的那一堆；默认 20"},
                       "if_none_match": {"type": "string",
                                         "description": "上次拿到的锚点 etag；一样就只回 unchanged"},
                       "include_expired": {"type": "boolean"},
                       "include_forgotten": {"type": "boolean"}
                   }}),
        )
        .read_only()
        .idempotent()
        .caps(&["mem.read"])
        .lifecycle(lc)
        .returns(json!({"data": {"type": "object"}}))
        .requires_on(&["confirmed"], &["data"])
        .to_json(),
        ToolDecl::new(
            "list",
            "列出这个库里所有锚点，各自多少条、几条钉住、几条过期、最后什么时候写的。",
            json!({"type": "object", "additionalProperties": false,
                   "properties": {"anchor": {"type": "string"}}}),
        )
        .read_only()
        .idempotent()
        .caps(&["mem.read"])
        .lifecycle(lc)
        .returns(json!({"data": {"type": "object"}}))
        .requires_on(&["confirmed"], &["data"])
        .to_json(),
        ToolDecl::new(
            "pin",
            "把一条碎片钉进「确定性层」：以后每次 recall 都先给你（学大眼那「常驻 5 条」）。",
            json!({"type": "object", "required": ["anchor", "id"], "additionalProperties": false,
                   "properties": {"anchor": {"type": "string"}, "id": {"type": "string"},
                                  "pinned": {"type": "boolean", "description": "默认 true；false = 拔钉"}}}),
        )
        .caps(&["mem.curate"])
        .idempotent()
        .lifecycle(lc)
        .returns(json!({"data": {"type": "object"}}))
        .requires_on(&["confirmed"], &["data"])
        .to_json(),
        ToolDecl::new(
            "forget",
            "忘掉一条（**打墓碑，不删行**）：默认不再给你，但「为什么不算数了」留在账上。",
            json!({"type": "object", "required": ["anchor", "id", "why"],
                   "additionalProperties": false,
                   "properties": {"anchor": {"type": "string"}, "id": {"type": "string"},
                                  "why": {"type": "string"}}}),
        )
        .caps(&["mem.curate"])
        .destructive()
        .lifecycle(lc)
        .returns(json!({"data": {"type": "object"}}))
        .requires_on(&["confirmed"], &["data"])
        .to_json(),
        // **"停下来"的入口**（`stop` 字段）：中控收我的时候调这个。
        //
        // 为什么非要有个入口：常驻服务**可能不是当前这个会话拉起来的** ——
        // 那时候中控手里没有我的子进程句柄，"杀进程"根本做不到。
        // 而"我怎么体面地停"只有我自己知道。
        ToolDecl::new(
            "shutdown",
            "让这个记忆服务自己停下来（**只有宿主中控能调**）。",
            json!({"type": "object", "additionalProperties": false, "properties": {}}),
        )
        .destructive()
        .lifecycle(lc)
        .stop_entry("shutdown")
        .returns(json!({"data": {"type": "object"}}))
        .requires_on(&["confirmed"], &["data"])
        .to_json(),
    ]
}

fn handle_line(
    out: &mut impl io::Write,
    ids: &mut IdGen,
    args: &Args,
    store: &Store,
    line: &str,
) -> After {
    let env = match codec::decode(line) {
        Ok(env) => env,
        Err(e) => {
            eprintln!("[mem] 收到不合规的消息：{e}");
            return After::Keep;
        }
    };
    match env.kind {
        MessageType::Request => return handle_request(out, ids, args, store, &env),
        MessageType::Cancel => {
            // 这一层的调用都是"读一本本地账、写一行"，快到没有可取消的窗口。
            // 照实说：**不是"我停下了"，是"它本来就做完了"**。
            reply_to(
                out,
                ids,
                args,
                &env,
                json!({"route": null, "effect": "confirmed", "code": null,
                       "why": "这次的调用本来就是本地读写、已经做完了 —— 没有可取消的中间态",
                       "data": {"canceled": false}}),
            );
        }
        _ => {}
    }
    After::Keep
}

fn reply_to(out: &mut impl io::Write, ids: &mut IdGen, args: &Args, req: &Envelope, payload: Value) {
    emit(
        out,
        &Envelope {
            id: ids.next_message(),
            ts: now_ms(),
            kind: MessageType::Result,
            from: args.instance.clone(),
            to: req.from.clone(),
            corr: req.corr.clone(),
            sid: args.sid.clone(),
            authority: Authority::Observe,
            payload,
        },
    );
}

fn handle_request(
    out: &mut impl io::Write,
    ids: &mut IdGen,
    args: &Args,
    store: &Store,
    req: &Envelope,
) -> After {
    let tool = req.payload.get("tool").and_then(Value::as_str).unwrap_or("");
    let call = req.payload.get("args").cloned().unwrap_or(json!({}));

    // **能力校验在执行侧**（§7）—— 中控只管签发，不知道"recall 要什么"
    let g = Grant::from_payload(&req.payload);
    let required = required_caps(tool);
    if !g.allows(required) {
        let payload = grant::refuse_payload(tool, &g.missing(required), &g);
        reply_to(out, ids, args, req, payload);
        return After::Keep;
    }

    // **"停下来"这道门比能力更窄**：只认"没经过网关（`Unrouted`）+ 发件人是中控"。
    //
    // 为什么不是"零能力就放行"：零能力的意思是"出问题时谁都能问它一句"，
    // 不是"谁都能把它关掉"。而中控自己发请求时**不带 `granted`**
    // （盖章只盖"发给工具服务的、别人发起的"——它自己那几条不盖），
    // 所以"没经过网关"在这里恰好等于"这句话是宿主亲口说的"。
    if tool == "shutdown" {
        let by_ctrl = matches!(g, Grant::Unrouted) && req.from == "ctrl";
        let payload = if by_ctrl {
            json!({"route": null, "effect": "confirmed", "code": null,
                   "data": {"bye": true, "instance": args.instance,
                            "resident": args.resident.is_some(),
                            "hint": "会合文件撤掉之后就没人能找到我了 —— \
                                     下一次有人要记忆会重新起一个"}})
        } else {
            json!({"route": null, "effect": "refused", "code": "refused",
                   "why": "`shutdown` 只认**宿主中控**亲口说的那句",
                   "remedy": "要停它得让中控去停（用户按 `stop`）—— \
                              你是调用方，不是它的宿主。这条不是能力问题，是身份问题"})
        };
        reply_to(out, ids, args, req, payload);
        return if by_ctrl { After::Shutdown } else { After::Keep };
    }

    let payload = match tool {
        "ping" => json!({"route": null, "effect": "confirmed", "code": null,
                         "data": {"pong": true, "store": store.dir().display().to_string()}}),
        "status" | "list" => match store.summaries(call.get("anchor").and_then(Value::as_str)) {
            Ok(all) => {
                let total: usize = all.iter().map(|a| a.alive).sum();
                let pinned: usize = all.iter().map(|a| a.pinned).sum();
                let expired: usize = all.iter().map(|a| a.expired).sum();
                json!({"route": null, "effect": "confirmed", "code": null,
                       "data": {
                           "store": store.dir().display().to_string(),
                           "anchors": all.iter().map(|a| json!({
                               "anchor": a.anchor, "alive": a.alive, "pinned": a.pinned,
                               "expired": a.expired, "forgotten": a.forgotten,
                               "last_at_ms": a.last_at_ms, "etag": a.etag,
                           })).collect::<Vec<_>>(),
                           "totals": {"anchors": all.len(), "alive": total,
                                      "pinned": pinned, "expired": expired}
                       }})
            }
            Err(e) => json!({"route": null, "effect": "refused", "code": "invalid",
                             "why": e.why, "remedy": e.remedy}),
        },
        "deposit" => {
            let anchor = call.get("anchor").and_then(Value::as_str).unwrap_or("");
            let text = call.get("text").and_then(Value::as_str).unwrap_or("");
            let epistemic = call
                .get("epistemic")
                .and_then(Value::as_str)
                .unwrap_or("fact");
            let tags = call
                .get("tags")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
                .unwrap_or_default();
            let ttl_ms = match call.get("ttl_ms").and_then(Value::as_i64) {
                // **说清了 0 就是不过期**（不是"用默认值"）—— 两个意思不能混
                Some(0) => None,
                Some(t) => Some(t),
                None => (args.default_ttl_ms > 0).then_some(args.default_ttl_ms),
            };
            let n = NewFrag {
                anchor,
                text,
                epistemic,
                tags,
                // **谁写的、哪次会话** —— 服务不猜，按信封上那个说话的人记
                agent: req.from.as_str(),
                session: req.sid.as_str(),
                now_ms: now_ms(),
                ttl_ms,
            };
            match store.deposit(n) {
                Ok(o) => json!({"route": null, "effect": "confirmed", "code": null,
                                "data": {"id": o.id, "etag": o.etag,
                                         "stored": o.stored,
                                         "anchor": anchor,
                                         "why": if o.stored {
                                             "写进去了"
                                         } else {
                                             "**这一条我已经有了**（同样的文本 + 同样的分类）—— \
                                              没重复写，回的是原来那条的 id"
                                         }}}),
                Err(e) => json!({"route": null, "effect": "refused", "code": "invalid",
                                 "why": e.why, "remedy": e.remedy}),
            }
        }
        "recall" => {
            let anchor = call.get("anchor").and_then(Value::as_str).unwrap_or("");
            let q = Query {
                text: call.get("query").and_then(Value::as_str).map(str::to_string),
                epistemic: call.get("epistemic").and_then(Value::as_str).map(str::to_string),
                limit: call.get("limit").and_then(Value::as_u64).map(|v| v as usize),
                if_none_match: call
                    .get("if_none_match")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                include_expired: call
                    .get("include_expired")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                include_forgotten: call
                    .get("include_forgotten")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            };
            match store.recall(anchor, &q) {
                Ok(r) => json!({"route": null, "effect": "confirmed", "code": null,
                                "data": {
                                    "anchor": r.anchor, "etag": r.etag,
                                    "unchanged": r.unchanged,
                                    "pinned": r.pinned, "items": r.items,
                                    "total": r.total, "expired_skipped": r.expired_skipped,
                                    "hint": if r.unchanged {
                                        "和上次给你的那份一模一样 —— 省下的那些 token 是你自己的"
                                    } else if r.total == 0 {
                                        "这个锚点上还没有东西。**没有**不等于\"这件事没发生过\" —— \
                                         也可能只是没人写过（§16.6：降级必须显式）"
                                    } else {
                                        "召回是**字面匹配**：没命中的同义说法它认不出来。\
                                         确定自己写过的话，换个更贴近原文的词再试"
                                    }
                                }}),
                Err(e) => json!({"route": null, "effect": "refused", "code": "invalid",
                                 "why": e.why, "remedy": e.remedy}),
            }
        }
        "pin" => {
            let anchor = call.get("anchor").and_then(Value::as_str).unwrap_or("");
            let id = call.get("id").and_then(Value::as_str).unwrap_or("");
            let pinned = call.get("pinned").and_then(Value::as_bool).unwrap_or(true);
            match store.set_pin(anchor, id, pinned) {
                Ok(()) => json!({"route": null, "effect": "confirmed", "code": null,
                                 "data": {"anchor": anchor, "id": id, "pinned": pinned,
                                          "hint": if pinned {
                                              "以后每次 recall 都先给你它 —— 别钉太多，\
                                               钉住的东西是每轮都要付 token 的"
                                          } else { "拔钉了：它回到普通那一堆里，按时间排" }}}),
                Err(e) => json!({"route": null, "effect": "refused", "code": "invalid",
                                 "why": e.why, "remedy": e.remedy}),
            }
        }
        "forget" => {
            let anchor = call.get("anchor").and_then(Value::as_str).unwrap_or("");
            let id = call.get("id").and_then(Value::as_str).unwrap_or("");
            let why = call.get("why").and_then(Value::as_str).unwrap_or("");
            match store.forget(anchor, id, why) {
                Ok(()) => json!({"route": null, "effect": "confirmed", "code": null,
                                 "data": {"anchor": anchor, "id": id, "forgotten": true,
                                          "hint": "**墓碑立了，行没删**：默认不再给你它，\
                                                   但 include_forgotten 还能翻出来看为什么"}}),
                Err(e) => json!({"route": null, "effect": "refused", "code": "invalid",
                                 "why": e.why, "remedy": e.remedy}),
            }
        }
        "help" => {
            let section = call.get("section").and_then(Value::as_str).unwrap_or("");
            json!({"route": null, "effect": "confirmed", "code": null,
                   "data": {
                       "sections": [
                           {"id": "overview", "title": "概览：这一层是干什么的"},
                           {"id": "anchor",   "title": "锚点：每条记忆挂在哪儿"},
                           {"id": "deposit",  "title": "怎么写一条碎片"},
                           {"id": "recall",   "title": "怎么读，以及 ETag 和 TTL"},
                           {"id": "errors",   "title": "错误码与补救"}
                       ],
                       "text": help_text(section, args)
                   }})
        }
        // **未知工具名：把真有的列出来**（说法统一在 `harness_service::reply`，
        // 名字从 `TOOL_NAMES` 来 —— `tools()` 里那几个和它有一条测试钉着）
        other => harness_service::reply::unknown_tool(other, &TOOL_NAMES),
    };
    reply_to(out, ids, args, req, payload);
    After::Keep
}

fn help_text(section: &str, args: &Args) -> String {
    match section {
        "overview" => "我是**沉淀式记忆**：跨会话的小碎片库，一个**锚点**一本账。\n\n\
                       我和「线性记忆」不是一回事 —— 那个在智能体进程里，每一轮都在算窗口、\
                       该压就压；我只在**有人明确要我记/取**的时候动。两者的接法是：\
                       会话要结束（或要切项目）时，把值得留的结论**沉淀**到我这儿。\n\n\
                       我为什么存在：窗口是短命的。会话结束，窗口里那些\"我们决定了 X\"\
                       就没了；下一个会话（或者另一个智能体）得从零再踩一遍。"
            .to_string(),
        "anchor" => "**每条记忆都要说清它挂在哪儿。** 锚点是**调用方给的**，不是我猜的 ——\n\
                     因为\"这次会话对应哪个项目\"只有托管它的那一层知道（会话 / 窗口）。\n\n\
                     锚点用什么粒度（§16.5）：\n\
                     · 项目级 → 项目路径（或 git 身份）—— 别用文件哈希，那会碎成渣；\n\
                     · 文件级 → 内容哈希；\n\
                     · 符号级 → 符号名路径（`Class/method`，能重新解析的那种）。\n\n\
                     **规则：锚点的粒度必须和记忆的粒度匹配。** 搞错了就是\"永远命中不了\"\
                     或者\"永远过期\"。".to_string(),
        "deposit" => format!(
            "`deposit {{anchor, text, epistemic?, tags?, ttl_ms?}}`\n\n\
             · `text` 是**一句话**（上限 {MAX_TEXT_BYTES} 字节）—— 超了会被拒，\
               而且拒的理由很重要：碎片一旦能装整篇文章，召回就等于把仓库倒进上下文。\
               长原文留在原地，这里放一条 `pointer`。\n\
             · `epistemic` 必须从这五个里挑：{} —— 它是**可信度的唯一依据**，\
               因为同一个锚点上的东西是**多个智能体**写的（开发者写的和攻防者写的，\
               份量不一样）。\n\
             · **幂等**：同样的话再写一遍，我会认出它、回你原来那条的 id，\
               而不是攒出第二条。\n\
             · `ttl_ms: 0` = **明确要求不过期**；不传 = 用服务的默认（{}）。",
            EPISTEMIC.join(" / "),
            if args.default_ttl_ms == 0 {
                "永不过期".to_string()
            } else {
                format!("{} ms", args.default_ttl_ms)
            }
        ),
        "recall" => "`recall {anchor, query?, epistemic?, limit?, if_none_match?, ...}`\n\n\
                     · 回来的分两堆：`pinned`（**确定性层**，钉住的，永远先给）和 `items`\
                       （其余，新的在前）。`limit` 只挤后面那堆。\n\
                     · **`etag` + `if_none_match`（HTTP 那条）**：把上次拿到的 `etag` 传回来，\
                       一模一样的话我就回 `unchanged: true`、**不给你内容** —— \
                       省下的那些 token 是你自己的。\n\
                     · **`expired_skipped`（DNS TTL 那条）**：过期的默认不给你，但**数**出来。\
                       \"少了多少\"必须看得见：看到 3 条过期，你就该知道这里有过东西。\n\
                     · ⚠️ **召回是字面匹配，不是语义匹配。** 同义改写认不出来。\
                       所以\"没召回出来\"**不等于\"这事没发生过\"** ——\
                       这是个降级，别拿它当结论。"
            .to_string(),
        "errors" => "· `invalid` —— 用法不对（缺锚点、文本太长、`epistemic` 不在那五个里、\
                     锚点上没有这个 id）。`remedy` 里写着正确形状。\n\
                     · `refused` —— **你没这个资格**（`mem.write` / `mem.read` / `mem.curate`）。\
                     能力是中控签发的：要么让用户给你签，要么别做这件事。**别重试。**\n\
                     · 我撞了域锁时会是 `state: degraded`：同一个碎片库上已经有另一个 mem 进程，\
                     那时候我只回答 `ping` / `status` / `help`，别的都拒。"
            .to_string(),
        _ => "章节清单在 `data.sections` 里；传 `section` 取某一章。\
              一次全倒给你会把上下文吃掉（渐进披露）。"
            .to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a_args() -> Args {
        Args {
            instance: "mem:test".into(),
            sid: "s-test".into(),
            kind: "mem".into(),
            store: PathBuf::from("data/memory-test"),
            default_ttl_ms: 0,
            resident: None,
        }
    }

    /// **报错里教的名字 == `tools()` 报出去的名字**（同编辑器 / 终端那两条）：
    /// 手写一份名单就是又一张会漂的表。
    #[test]
    fn the_error_message_lists_exactly_the_tools_we_declare() {
        let declared: Vec<String> = tools(&a_args())
            .iter()
            .filter_map(|t| t["name"].as_str().map(str::to_string))
            .collect();
        let taught: Vec<String> = TOOL_NAMES.iter().map(|s| s.to_string()).collect();
        assert_eq!(declared, taught, "报错里教的名字和 tools() 对不上");
    }

    /// **声明的能力和强制的必须逐条对得上**（2026-09-20）。
    ///
    /// 这条测试**当场抓到过一个真的**：执行那张表里有一条 `"unpin"` 分支，
    /// 而**根本没有 `unpin` 这个工具**（`pin` 用 `pinned: false` 表达拔钉）。
    /// 一条对着不存在工具的分支不会出错 —— 但它说明这两张表是分开维护的，
    /// 而分开维护迟早会漂成"声明了 `mem.write`、实际什么都没执行"的静默洞。
    #[test]
    fn the_enforcement_table_matches_what_we_declare() {
        for t in tools(&a_args()) {
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
}
