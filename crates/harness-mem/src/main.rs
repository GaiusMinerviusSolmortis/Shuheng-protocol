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
use harness_core::id::IdGen;
use harness_core::now_ms;
use harness_mem::store::{NewFrag, Query, Store, EPISTEMIC, MAX_TEXT_BYTES};
use harness_service::contract::ToolDecl;
use harness_service::domain_lock;
use harness_service::grant::{self, Grant};
use serde_json::{json, Value};
use std::io::{self, BufReader};
use std::path::PathBuf;

struct Args {
    instance: String,
    sid: String,
    kind: String,
    store: PathBuf,
    /// 默认寿命：`0` = 永不过期。默认给一个（30 天）而**不是**永远 ——
    /// 因为"永远"是那种看起来安全、实际把库变成垃圾场的选择。
    default_ttl_ms: i64,
}

fn parse_args() -> Result<Args, String> {
    let mut instance = String::from("mem:1");
    let mut sid = String::from("s-1");
    let mut kind = String::from("mem");
    let mut store = PathBuf::from("data/memory");
    let mut default_ttl_ms: i64 = 30 * 24 * 3600 * 1000;
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

    let mut ids = IdGen::new(args.instance.replace([':', '.'], "-"));
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
            Ok(NextLine::Line(line)) => handle_line(&mut out, &mut ids, &args, &store, &line),
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
    let mut ids = IdGen::new(args.instance.replace([':', '.'], "-"));
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
        "pin" | "unpin" | "forget" => &["mem.curate"],
        "recall" | "list" => &["mem.read"],
        // 零能力：出问题的时候得有人能问它一句 —— 诊断通道不能跟着鉴权一起坏
        "help" | "ping" | "status" => &[],
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
    })
}

fn tools(args: &Args) -> Vec<Value> {
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
        .lifecycle("per_session")
        .requires_on(&["confirmed"], &["data"])
        .to_json(),
        ToolDecl::new(
            "ping",
            "看看它还在不在。",
            json!({"type": "object", "additionalProperties": false, "properties": {}}),
        )
        .read_only()
        .idempotent()
        .lifecycle("per_session")
        .requires_on(&["confirmed"], &["data"])
        .to_json(),
        ToolDecl::new(
            "status",
            "看这个碎片库：它在哪、有几个锚点、多少条、几条过期的。",
            json!({"type": "object", "additionalProperties": false, "properties": {}}),
        )
        .read_only()
        .idempotent()
        .lifecycle("per_session")
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
        .lifecycle("per_session")
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
        .lifecycle("per_session")
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
        .lifecycle("per_session")
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
        .lifecycle("per_session")
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
        .lifecycle("per_session")
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
) {
    let env = match codec::decode(line) {
        Ok(env) => env,
        Err(e) => {
            eprintln!("[mem] 收到不合规的消息：{e}");
            return;
        }
    };
    match env.kind {
        MessageType::Request => handle_request(out, ids, args, store, &env),
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

fn handle_request(out: &mut impl io::Write, ids: &mut IdGen, args: &Args, store: &Store, req: &Envelope) {
    let tool = req.payload.get("tool").and_then(Value::as_str).unwrap_or("");
    let call = req.payload.get("args").cloned().unwrap_or(json!({}));

    // **能力校验在执行侧**（§7）—— 中控只管签发，不知道"recall 要什么"
    let g = Grant::from_payload(&req.payload);
    let required = required_caps(tool);
    if !g.allows(required) {
        let payload = grant::refuse_payload(tool, &g.missing(required), &g);
        reply_to(out, ids, args, req, payload);
        return;
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
        other => json!({"route": null, "effect": "refused", "code": "invalid",
                        "why": format!("没有名为 `{other}` 的工具"),
                        "remedy": "调 `help` 看有哪些 —— 别猜名字"}),
    };
    reply_to(out, ids, args, req, payload);
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
