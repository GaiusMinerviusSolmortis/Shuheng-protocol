// manifest 那个 json! 字面量已经大到把宏的默认递归上限顶穿了 ——
// 是宏展开的深度问题，不是数据问题。
#![recursion_limit = "512"]

//! **第一个真工具实例：编辑器。**
//!
//! 它验证的是这套设计里最新、最没有先例可抄的两件事（README 阶段 2）：
//! **路径围栏**（§7.1）与**写互斥**（§5.15）。回滚（§5.24 的 L1）是顺带落地的。
//!
//! 它同时回答 §11.6 那个洞 #2：`capabilities` 写成**纯标签**
//! （`fs.read` / `fs.write`），**范围不在令牌上，而在 instances 的围栏里** ——
//! 也就是 §7 说的那个 capability 由 `令牌 + scope` 两个数据合起来表达。

mod doc;
mod locks;

use doc::{Document, EditError};
use harness_core::codec::{self, NdjsonReader, NextLine};
use harness_core::envelope::{Authority, Envelope, MessageType};
use harness_core::id::IdGen;
use harness_core::instance_event::InstanceEvent;
use harness_core::now_ms;
use harness_service::domain_lock;
use harness_service::contract::ToolDecl;
use harness_service::fence::Fence;
use harness_service::grant::{self, Grant};
use harness_service::reply;
use locks::{Acquire, Holder, LockTable};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::{self, BufReader};
use std::path::{Path, PathBuf};

struct Args {
    instance: String,
    sid: String,
    kind: String,
    fences: Vec<String>,
    /// **一页最多多少字节**（`--inline-limit`，默认 50 000）。
    ///
    /// `read` 是**分页器**不是摘要器：不传 `limit` 就给一页，`next_offset` 指给你接着读。
    /// 这条和终端那边是同一个数、同一条纪律（§5.17），只是编辑器**不需要溢出副本** ——
    /// 全文本来就是磁盘上那个文件（见 `tool_read` 里 `spilled_to` 的说明）。
    inline_limit: usize,
}

fn parse_args() -> Args {
    let mut a = Args {
        instance: "editor:1".into(),
        sid: "s-1".into(),
        kind: "editor".into(),
        fences: Vec::new(),
        inline_limit: 50_000,
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
            _ => {}
        }
    }
    a
}

struct Editor {
    args: Args,
    ids: IdGen,
    fence: Fence,
    /// 拿不到域锁时它是 `Some(冲突原因)` —— 服务照常在线，但**不接活**。
    /// 之所以不退出了事：那样中控只能看见一句含糊的 `service_down`，
    /// 而"为什么用不了"才是这一层真正该说清楚的东西（§4.6）。
    degraded: Option<String>,
    locks: LockTable,
    docs: BTreeMap<String, Document>,
    next_doc: u64,
}

fn main() {
    let args = parse_args();
    let mut out = io::stdout();
    let mut ed = Editor {
        ids: IdGen::new(args.instance.replace([':', '.'], "-")),
        fence: Fence::new(&args.fences).unwrap_or_default(),
        degraded: None,
        locks: LockTable::default(),
        docs: BTreeMap::new(),
        next_doc: 0,
        args,
    };
    if ed.fence.is_empty() {
        eprintln!("[editor] 警告：没有托管路径，任何文件操作都会被拒（缺 --fence）");
    }

    // **域锁**：同一个域只能有一个服务进程。责任在应用这一侧（2026-09-19）。
    match domain_lock::acquire("fs", ed.fence.roots()) {
        Ok(_lock) => eprintln!(
            "[editor] 域锁拿到：{}",
            ed.fence
                .roots()
                .iter()
                .map(|r| r.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Err(c) => {
            eprintln!("[editor] 域冲突：{}", c.why);
            ed.degraded = Some(format!("{}；{}", c.why, c.remedy));
        }
    }

    let hello = ed.mint(
        MessageType::Presence,
        "ctrl",
        None,
        Authority::Observe,
        match &ed.degraded {
            None => json!({"state": "online", "manifest": manifest(&ed.args, &ed.fence)}),
            // systemd 状态机里那个 `degraded`：**活着，但别指望它干活**
            Some(reason) => json!({
                "state": "degraded",
                "degraded_reason": reason,
                "manifest": manifest(&ed.args, &ed.fence)
            }),
        },
    );
    emit(&mut out, &hello);

    let mut reader = NdjsonReader::new(BufReader::new(io::stdin()));
    loop {
        match reader.next_line_checked() {
            Ok(NextLine::Line(line)) => {
                let env = match codec::decode(&line) {
                    Ok(e) => e,
                    Err(e) => {
                        eprintln!("[editor] 收到不合规的消息：{e}");
                        continue;
                    }
                };
                if env.kind == MessageType::Request {
                    ed.handle_request(&mut out, &env);
                }
            }
            Ok(NextLine::Invalid { bytes }) => {
                eprintln!("[editor] 跳过一行非 UTF-8 的输入（{bytes} 字节）")
            }
            Ok(NextLine::Eof) => break,
            Err(e) => {
                eprintln!("[editor] 读 stdin 失败：{e}");
                break;
            }
        }
    }

    // stdin 关了 = 中控收摊。**锁随进程消失** —— 这就是 §5.15 那条
    // "持有者死了锁要能自己释放"在这里的实现方式，不需要额外的清理。
    let bye = ed.mint(
        MessageType::Presence,
        "ctrl",
        None,
        Authority::Observe,
        json!({"state": "offline"}),
    );
    emit(&mut out, &bye);
}

/// **这本编辑器的手册**：手写、分章、由它自己提供。
const HELP: &[(&str, &str, &str)] = &[
    (
        "overview",
        "概览",
        "编辑器是干什么的：\n\
         · 它按**路径**持有一份文件的**写权** —— open 那一步就在上锁了。\n\
         · 同一时刻只有一个持有者能改那个文件。别人要改会被拒（busy），**不排队**。\n\
         · **读是自由的**：只是想看一眼的文件，直接读文件，不必 open（用了会占住写权）。\n\
         · 它只管**改文件**。要跑命令请用终端 —— 那边才有输出游标和超时。\n\
         · 一旦它降级（同一个围栏已经有别的编辑器在管），它照常在线但**不接活**。",
    ),
    (
        "lock",
        "锁与写互斥",
        "· 锁的持有者是**一个活着的会话**（`你的实例 id @ 会话 id`），不是一个占位符。\n\
         · 所以抢不到时**立刻报错，不排队** —— 对方什么时候释放不可预测，等它等于白等。\n\
         · 报错里会告诉你**是谁挡着**（`held_by`），以及你还能干什么。\n\
         · 同一个持有者重复 open 同一个文件是**幂等**的，返回同一个 instance_id。\n\
         · close 释放。**只有持有者能改、能释放**，拿着别人的 instance_id 也没用。",
    ),
    (
        "edit",
        "edit 的参数与回滚",
        "· `old` 必须**恰好命中一次**：命中 0 处报 invalid（提示你 read 一下），\n\
         命中 2 处以上也报 invalid（提示你把 old 写长一点）。\n\
         · **写即落盘**：edit 成功就是磁盘上已经改了。所以它返回一个 `revision` ——\n\
         那是**这次编辑之前**的版本号，拿它就能退回去。\n\
         · `revert {revision}` 把文件写回那个版本，**并丢掉它之后的历史**（不做 redo）。\n\
         0 = 刚打开时的内容。\n\
         · **空文件怎么写入**：`old` 传空串，但只在文件本身是空的时候合法。\n\
         · 想确认自己没搞错就先 `read` —— 你手上的内容可能已经过期了。",
    ),
    (
        "errors",
        "错误码与补救",
        "· `busy` —— 那个文件的写权在别人的会话手里。别等：读是自由的，或者换一个文件。\n\
         · `instance_gone` —— 实例没了。重新 open。\n\
         · `refused` + \"不在托管路径内\" —— 那条路径没被托管。**那不是故障，是围栏**。\n\
         · `refused` + \"需要的能力你没有\" —— 中控没给你这个能力，找它要，别重试。\n\
         · `refused` + \"文件不存在\" —— open 不会凭空造文件，要新建就传 `create: true`。",
    ),
];

/// 每个工具需要什么能力 —— **工具自己的接口的一部分**，所以写在这儿。
fn required_caps(tool: &str) -> &'static [&'static str] {
    match tool {
        "open" | "edit" | "revert" => &["fs.write"],
        "read" => &["fs.read"],
        "close" | "status" | "ping" | "help" => &[],
        "verify" => &["fs.read"],
        _ => &[],
    }
}

/// **本服务的工具名**：一份，不是两份。
///
/// 为什么要有这张表：`没有名为 X 的工具` 那条消息**要教模型对的名字** ——
/// 原来那份名单是手写的（`open | read | edit | revert | close | status | ping`），
/// 而它**已经漂了**：少了 `help` 和 `verify`（实际九个）。
/// 现在名单从这张表来，并且有一条测试把它和 manifest 里**真正报出去的名字**钉在一起。
const TOOL_NAMES: [&str; 9] = [
    "help", "verify", "open", "read", "edit", "revert", "close", "status", "ping",
];

fn manifest(args: &Args, fence: &Fence) -> Value {
    json!({
        "contract_version": "1",
        "id": "editor",
        "domain": "fs",
        "version": env!("CARGO_PKG_VERSION"),
        "title": "编辑器",
        "kind": args.kind,
        "platforms": ["windows", "linux", "macos"],
        "node_kind": "editor",
        // 只有**实例状态**（"这个实例在干什么"）。
        // `degraded` 拿掉了 —— 那是**存活状态**，归中控那一轴（§11.8 的规则）。
        "state_vocabulary": ["idle", "held", "closed"],
        "content": ["document", "log"],
        "node_actions": ["close", "force_close"],
        "instance_types": ["file"],
        "summary": "按路径持有一份文件的写权，读、改、回滚都在持有期内完成。",
        "when_to_use": "需要改动托管路径内的文件时。只是看一眼不必用它 —— 直接读文件。",
        // **遗嘱**：我崩了意味着什么。只有我自己知道 —— 中控只负责代发（§19.2）。
        "will": "它持有的那些文件写权随着进程一起没了（锁由操作系统释放），别人可以重新 open；\
                 已经落盘的 edit **不会**撤销 —— 要退回去得用文件自己的 revision 或者 git。",
        "injection_policy": "card",
        "help_sections": [
            {"id": "overview", "title": "概览"},
            {"id": "lock",     "title": "锁与写互斥"},
            {"id": "edit",     "title": "edit 的参数与回滚"},
            {"id": "errors",   "title": "错误码与补救"}
        ],
        // 同上：报规范化之后的围栏
        "fence": if fence.is_empty() {
            args.fences.clone()
        } else {
            fence.roots()
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
        },
        // **注册时就把"我占了什么"讲清楚** —— 冲突要在这一层可见
        "owns": {"domain": "fs", "exclusive_per": "fence"},
        //
        // ⚠️ **`filesystem` 跟着事实走，不写死**：没有围栏的编辑器
        // 就不是"每一步都检查过"，那时候它必须报 `none`。
        // （第一版这里写死了 `fence-args`，**闸门当场把它拦下来了** ——
        //   那条规则要的就是这个：声称围了、却没围栏，是一句让人安心的假话。）
        "sandbox": {
            "filesystem": if fence.is_empty() { "none" } else { "fence-args" },
            "network": "none",
            "identity": "same-user",
            "executables": "none",
            "resources": "none"
        },
        "sandbox_note": if fence.is_empty() {
            "**这次它没有托管任何路径**：`filesystem: none` 说的是「没有可围的东西」——\
             不是「没管住」。真有路径要读写时，会话必须先托管一条。"
        } else {
            "**围栏是真的，但它只围「这个服务自己」**：它不拉子进程，\
             所以「看得见的只有托管那条路径」成立。⚠️ 这不是内核级隔离\
             （那是 `filesystem: os`，没人做到）——它靠的是**每次读写都自己检查一遍**。"
        },
        // **参数表与结果形状都是服务自己的义务**（规范 §11.1 的 C 组）。
        // 信封那一半（effect / code / route / why / remedy …）由
        // `ToolDecl` 与 `contract::result_schema` 补 —— 服务只写它独有的字段。
        "tools": tools(),
        "error_codes": ["service_down", "instance_gone", "busy", "refused", "invalid", "timeout"]
    })
}

/// 工具表 —— §11.1 C 组的九个字段，一个不少。
///
/// `returns` 写的是**这个工具自己的**结果字段；`requires_on` 说的是
/// "**只在成功时才**必须有它" —— 因为 `open` 被 `busy` 挡下时没有
/// `instance_id`，硬要求等于让声明变成谎话（这一条是 2026-09-19 加闸门时
/// 才看出来的：规范 §11.6 那份示例就是硬要求，它本身不自洽）。
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
            "verify",
            "独立验证文件现在的样子：从**磁盘**读一遍，对比你给的判据，回 satisfied / unsatisfied / unknown。**unknown 永远不能被当成成功。**",
            json!({"type": "object", "required": ["instance_id", "expect"], "additionalProperties": false,
                   "properties": {"instance_id": {"type": "string"},
                                  "expect": {"type": "object", "description": "至少给一条：contains / bytes",
                                             "properties": {"contains": {"type": "string"},
                                                            "bytes": {"type": "integer"}}}}}),
        )
        .caps(&["fs.read"])
        .read_only()
        .idempotent()
        .requires_on(&["confirmed"], &["data"])
        .to_json(),
        ToolDecl::new(
            "open",
            "打开一个文件并取得它的写权，返回 instance_id。",
            json!({"type": "object", "required": ["path"], "additionalProperties": false,
                   "properties": {"path": {"type": "string", "description": "必须落在托管路径内"},
                                  "create": {"type": "boolean", "description": "文件不存在时是否新建（默认 false，不给就拒）"}}}),
        )
        .caps(&["fs.write"])
        .idempotent()
        .returns(json!({
            "instance_id": {"type": ["string", "null"]},
            "path": {"type": "string"},
            "revision": {"type": "integer"},
            "bytes": {"type": "integer"},
            // 新建时为 true；重复 open（幂等那一次）时 false
            "created": {"type": "boolean"},
            "idempotent": {"type": "boolean"},
            // `busy` 时必有：**锁在谁手里**（§11.6 那个 `held_by`）
            "held_by": {
                "type": "object",
                "required": ["sid", "agent", "held_ms"],
                "properties": {
                    "sid": {"type": "string"},
                    "agent": {"type": "string"},
                    "held_ms": {"type": "integer"}
                }
            },
        }))
        .requires_on(&["confirmed"], &["instance_id"])
        .lifecycle("per_session")
        .to_json(),
        ToolDecl::new(
            "read",
            "读实例当前持有的内容，可带字节偏移与上限。",
            json!({"type": "object", "required": ["instance_id"], "additionalProperties": false,
                   "properties": {"instance_id": {"type": "string"},
                                  "offset": {"type": "integer"}, "limit": {"type": "integer"}}}),
        )
        .caps(&["fs.read"])
        .read_only()
        .idempotent()
        .returns(json!({
            "text": {"type": "string"},
            "next_offset": {"type": ["integer", "null"]},
            "spilled_to": {"type": ["string", "null"]},
            "revision": {"type": "integer"},
            "bytes": {"type": "integer"},
            // **别人在磁盘上动过这个文件吗**（见 `Document::changed_on_disk`）
            "disk_changed": {"type": "boolean"},
        }))
        .requires_on(&["confirmed"], &["text"])
        .lifecycle("per_session")
        .to_json(),
        ToolDecl::new(
            "edit",
            "对持有中的文件做一次替换。old 必须**恰好命中一次**。写即落盘。",
            json!({"type": "object", "required": ["instance_id", "old", "new"], "additionalProperties": false,
                   "properties": {"instance_id": {"type": "string"},
                                  "old": {"type": "string", "description": "要唯一命中；命中 0 处或 2 处以上都报 invalid"},
                                  "new": {"type": "string"}}}),
        )
        .caps(&["fs.write"])
        .destructive()
        .returns(json!({
            "changed": {"type": "integer"},
            "revision": {"type": ["integer", "null"]},
            "now_revision": {"type": "integer"},
            "bytes": {"type": "integer"},
        }))
        .requires_on(&["confirmed"], &["changed", "revision"])
        .lifecycle("per_session")
        .to_json(),
        ToolDecl::new(
            "revert",
            "把文件写回某个 revision（edit 返回的那个），并丢掉它之后的历史。",
            json!({"type": "object", "required": ["instance_id", "revision"], "additionalProperties": false,
                   "properties": {"instance_id": {"type": "string"},
                                  "revision": {"type": "integer", "description": "0 = 刚打开时的内容"}}}),
        )
        .caps(&["fs.write"])
        .destructive()
        .idempotent()
        .returns(json!({
            "revision": {"type": "integer"},
            "restored_from": {"type": "integer"},
        }))
        .requires_on(&["confirmed"], &["revision"])
        .lifecycle("per_session")
        .to_json(),
        ToolDecl::new(
            "close",
            "释放写权。不写盘、不撤销 —— 要撤销用 revert。",
            json!({"type": "object", "required": ["instance_id"], "additionalProperties": false,
                   "properties": {"instance_id": {"type": "string"}}}),
        )
        .idempotent()
        .returns(json!({"released": {"type": "string"}}))
        .requires_on(&["confirmed"], &["released"])
        .to_json(),
        ToolDecl::new(
            "status",
            "看现在持有哪些文件、锁在谁手里。",
            json!({"type": "object", "additionalProperties": false, "properties": {}}),
        )
        .read_only()
        .idempotent()
        .requires_on(&["confirmed"], &["data"])
        .to_json(),
        // **`ping` 也得报出来** —— `health: "ping"` 声明了探针就是它，
        // 一个"说不出口的探针"等于没声明。
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

impl Editor {
    /// **独立验证**（§11.2 硬规则二："验证是独立的一次调用"）。
    ///
    /// 为什么必须有它：`edit` 说自己成功了，那是**它自己的说法**。
    /// 而写盘这件事恰好会悄悄失败（文件被别的程序独占、路径被换掉……）——
    /// 所以验证要**独立地、从磁盘**读一遍，而不是拿内存里那份去比。
    ///
    /// 三种状态，其中一个地方不许含糊（§11.2 硬规则二的原话）：
    /// **`unknown` 永远不能被当成成功。**
    fn tool_verify(&mut self, args: &Value) -> Value {
        let id = args.get("instance_id").and_then(Value::as_str).unwrap_or("");
        if id.is_empty() {
            return reply::missing_instance_id("verify");
        }
        let Some(doc) = self.docs.get(id) else {
            return reply::no_such_instance("verify", id);
        };
        let expect = args.get("expect").cloned().unwrap_or(json!({}));
        let path = doc.path.clone();
        // **从磁盘读**，不信内存里那份
        let Ok(disk) = std::fs::read_to_string(&path) else {
            return json!({
                "route": null, "effect": "confirmed", "code": null,
                "data": {"status": "unknown",
                         "why": "文件现在读不出来（可能被别的程序独占，也可能被删了）",
                         "rule": "**unknown 永远不能被当成成功** —— \
                                  你没法据此认为它现在是你要的样子",
                         "remedy": "先 read 看看到底是什么状态，再决定要不要重来"}
            });
        };
        let mut checked: Vec<Value> = Vec::new();
        let mut all = true;
        if let Some(need) = expect.get("contains").and_then(Value::as_str) {
            let ok = disk.contains(need);
            all &= ok;
            checked.push(json!({"check": "contains", "want": need, "ok": ok}));
        }
        if let Some(n) = expect.get("bytes").and_then(Value::as_u64) {
            let ok = disk.len() as u64 == n;
            all &= ok;
            checked.push(json!({"check": "bytes", "want": n, "actual": disk.len(), "ok": ok}));
        }
        if checked.is_empty() {
            return json!({
                "route": null, "effect": "refused", "code": "invalid",
                "why": "expect 里没有一条我能验的",
                "expect": {"expect": {"contains": "磁盘内容里应当有这段", "bytes": "文件应当正好这么大"}},
                "remedy": "至少要给一条判据 —— 不给判据的\"验证\"没有意义"
            });
        }
        json!({
            "route": null, "effect": "confirmed", "code": null,
            "data": {
                "status": if all { "satisfied" } else { "unsatisfied" },
                "checks": checked,
                "source": "从磁盘读的，不是内存里那份",
                "bytes_on_disk": disk.len()
            }
        })
    }

    /// **手册**：手写、分章、由它自己提供（§5.3 / §5.5）。
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

    fn handle_request(&mut self, out: &mut impl io::Write, req: &Envelope) {
        let tool = req.payload.get("tool").and_then(Value::as_str).unwrap_or("");
        let args = req.payload.get("args").cloned().unwrap_or(json!({}));
        let holder = Holder {
            sid: req.sid.clone(),
            agent: req.from.clone(),
        };

        // **能力校验在执行侧**（§7）：中控只管签发，这里只管比对。
        // 所以"改一条能力要求"或"加一个工具"都不用动中控。
        let grant = Grant::from_payload(&req.payload);
        let required = required_caps(tool);
        if !grant.allows(required) {
            let payload = grant::refuse_payload(tool, &grant.missing(required), &grant);
            let reply = self.mint(
                MessageType::Result,
                &req.from.clone(),
                req.corr.clone(),
                Authority::Observe,
                payload,
            );
            emit(out, &reply);
            return;
        }

        // 降级状态下**不接活**：它连"同一时刻只有一个写者"都保证不了，
        // 让它继续改文件等于把互斥当成摆设。只留 ping / status 两个出口，
        // 让调用方还看得见发生了什么（§4.6：别用含糊的失败代替说明）。
        if let Some(reason) = self.degraded.clone() {
            let payload = if matches!(tool, "ping" | "status") {
                json!({
                    "route": null, "effect": "confirmed", "code": null,
                    "degraded": true, "degraded_reason": reason,
                    "data": {"instance": self.args.instance, "fence": self.args.fences}
                })
            } else {
                json!({
                    "route": null, "effect": "refused",
                    "code": "refused",
                    "why": reason,
                    "remedy": "这个实例被降级了，它不能碰文件；\
                               要么让中控只留一个管这条路径的服务，\
                               要么换一个围栏不同的实例"
                })
            };
            let reply = self.mint(
                MessageType::Result,
                &req.from.clone(),
                req.corr.clone(),
                Authority::Observe,
                payload,
            );
            emit(out, &reply);
            return;
        }

        let payload = match tool {
            "ping" => json!({"route": null, "effect": "confirmed", "code": null, "data": {"pong": true}}),
            "help" => self.tool_help(&args),
            "verify" => self.tool_verify(&args),
            "open" => self.tool_open(out, &holder, &args),
            "read" => self.tool_read(out, &holder, &args),
            "edit" => self.tool_edit(out, &holder, &args),
            "revert" => self.tool_revert(out, &holder, &args),
            "close" => self.tool_close(out, &holder, &args),
            "status" => self.tool_status(),
            other => {
                reply::unknown_tool(other, &TOOL_NAMES)
            }
        };

        let reply = self.mint(
            MessageType::Result,
            &req.from.clone(),
            req.corr.clone(),
            Authority::Observe,
            payload,
        );
        emit(out, &reply);
    }

    // ---- 工具 ----------------------------------------------------------

    fn tool_open(&mut self, out: &mut impl io::Write, holder: &Holder, args: &Value) -> Value {
        let raw = args.get("path").and_then(Value::as_str).unwrap_or("");
        let create = args.get("create").and_then(Value::as_bool).unwrap_or(false);

        let path = match self.fence.resolve(raw) {
            Ok(p) => p,
            Err(e) => {
                return json!({
                    // **码跟着错误走**（围栏自己说的）：`path` 空了是 `invalid`，
                    // 越界才是 `refused` —— 模型按码决定"改参数"还是"别试了"
                    "route": null, "effect": "refused", "code": e.code,
                    "why": e.why, "remedy": e.remedy
                })
            }
        };

        let candidate = format!("ed-{}", self.next_doc + 1);
        let got = self.locks.acquire(&path, holder, now_ms(), || candidate.clone());

        match got {
            Acquire::Busy {
                instance,
                holder: who,
                held_ms,
            } => {
                // 报错即手册（§5.5）：事实 + 影响 + 出口。**不排队**是你定的。
                json!({
                    "route": null, "effect": "refused",
                    "code": "busy",
                    "why": format!("`{}` 的写权在会话 `{}`（实例 `{}`）手里，已经持有 {} ms",
                                   path.display(), who.sid, who.agent, held_ms),
                    "held_by": {"sid": who.sid, "agent": who.agent, "held_ms": held_ms},
                    "instance_id": instance,
                    "remedy": "别等（它什么时候放不确定）：读是不受限制的，换一个文件，\
                               或者请用户来定；要退回去用 revert"
                })
            }
            Acquire::Granted { instance, new } => {
                if new {
                    let exists = path.exists();
                    if !exists && !create {
                        self.locks.release(&path, holder);
                        return json!({
                            "route": null, "effect": "refused", "code": "invalid",
                            "why": format!("`{}` 不存在", path.display()),
                            "remedy": "要新建就带 \"create\": true；open 不会凭空造文件"
                        });
                    }
                    let content = match std::fs::read_to_string(&path) {
                        Ok(c) => c,
                        Err(_) if !exists => {
                            if let Err(e2) = std::fs::write(&path, "") {
                                self.locks.release(&path, holder);
                                return json!({
                                    "route": null, "effect": "refused", "code": "invalid",
                                    "why": format!("建不了 `{}`：{e2}", path.display()),
                                    "remedy": "检查父目录的权限"
                                });
                            }
                            String::new()
                        }
                        Err(e) => {
                            self.locks.release(&path, holder);
                            return json!({
                                "route": null, "effect": "refused", "code": "invalid",
                                "why": format!("读不了 `{}`：{e}", path.display()),
                                "remedy": "这个编辑器第一版只处理 UTF-8 文本；二进制文件不要用它"
                            });
                        }
                    };
                    self.next_doc += 1;
                    let bytes = content.len();
                    let mut doc = Document::new(instance.clone(), path.clone(), content);
                    // **记下"我离开时磁盘长这样"** —— 之后每次写盘前拿它比对
                    // （见 `Document::changed_on_disk`）
                    doc.note_disk();
                    self.docs.insert(instance.clone(), doc);

                    // 事件回传：**标准形状的实例事件** —— 中控靠它把节点画到图上
                    let ev = self.mint(
                        MessageType::Event,
                        "ctrl",
                        None,
                        Authority::Observe,
                        InstanceEvent::open(&instance, "held")
                            .with_path(path.display().to_string())
                            .owned_by(format!("{}@{}", holder.agent, holder.sid))
                            .with_detail(json!({"holder": {"sid": holder.sid, "agent": holder.agent}}))
                            .to_payload(),
                    );
                    emit(out, &ev);

                    json!({
                        "route": null, "effect": "confirmed", "code": null,
                        "instance_id": instance,
                        "path": path.display().to_string(),
                        "revision": 0, "bytes": bytes, "created": !exists
                    })
                } else {
                    // 同一个持有者重复 open：幂等，返回同一个实例（否则它会把自己锁死）
                    let (revision, bytes) = self
                        .docs
                        .get(&instance)
                        .map(|d| (d.revision(), d.bytes()))
                        .unwrap_or((0, 0));
                    json!({
                        "route": null, "effect": "confirmed", "code": null,
                        "instance_id": instance,
                        "path": path.display().to_string(),
                        "revision": revision, "bytes": bytes, "idempotent": true
                    })
                }
            }
        }
    }

    /// **把磁盘拉进来**（如果用户改过），并报一条实例事件。
    ///
    /// 三个入口（`read` / `edit` / `revert`）进门都先做这一件事 ——
    /// 因为**用户改文件不是异常，实例要跟着磁盘走**（写锁保护的是
    /// "别的 AI 别大批量覆盖"，不是"用户别碰"）。
    fn sync_if_changed(&mut self, out: &mut impl io::Write, id: &str, holder: &Holder) -> Option<(u64, String)> {
        let synced = self.docs.get_mut(id).and_then(|d| d.sync_from_disk())?;
        let (rev, ref why) = synced;
        eprintln!("[editor] {id}：磁盘上那份被外界改过，已拉进来（{why}，现在 revision {rev}）");
        let ev = self.mint(
            MessageType::Event,
            "ctrl",
            None,
            Authority::Observe,
            InstanceEvent::state(id, "held")
                .owned_by(format!("{}@{}", holder.agent, holder.sid))
                .with_detail(json!({"synced_from_disk": true, "revision": rev}))
                .to_payload(),
        );
        emit(out, &ev);
        Some(synced)
    }

    fn tool_read(&mut self, out: &mut impl io::Write, holder: &Holder, args: &Value) -> Value {
        let id = args.get("instance_id").and_then(Value::as_str).unwrap_or("");
        if let Err(v) = self.doc_for("read", id, holder) {
            return v;
        }
        let synced = self.sync_if_changed(out, id, holder);
        let offset = args.get("offset").and_then(Value::as_u64).unwrap_or(0) as usize;
        // **默认给一页**（内联上限）—— 见 `Args::inline_limit` 的说明。
        // 想全看就多读几页，而不是"再跑一条命令去 cat"（§5.17 那个死循环）。
        let limit = args
            .get("limit")
            .and_then(Value::as_u64)
            .map(|v| v as usize)
            .unwrap_or(self.args.inline_limit);
        let doc = self.docs.get(id).expect("刚查过");
        let (text, next) = slice_bytes(doc.content(), offset, Some(limit));
        let mut payload = json!({
            "route": null, "effect": "confirmed", "code": null,
            "text": text, "next_offset": next,
            // **编辑器没有"溢出副本"这种东西：全文就是磁盘上那个文件。**
            // 终端要 `spilled_to` 是因为它的输出只活在内存/溢写文件里；
            // 这里把 null 写实，并且**说清全文在哪**（就是 path 那个文件）——
            // 这比给一个溢写副本更直接。
            "spilled_to": null,
            "revision": doc.revision(),
            "bytes": doc.bytes(),
            // **外界改过没有**（改过的话上面已经拉进来了 —— 你读到的就是最新的）
            "disk_changed": synced.is_some()
        });
        if let Some(off) = next {
            let rest = doc.bytes().saturating_sub(off);
            let mut why = format!(
                "这是**分页**：还有 {rest} 字节没给你。用 `offset: {off}` 接着读，读完为止；\
                 全文就在你打开的那个文件里（`spilled_to` 是 null —— 编辑器不需要溢写副本）"
            );
            if let Some((rev, sync_why)) = &synced {
                why.push_str(&format!(
                    "。另外：{sync_why} —— 我已经把它拉进来当作 revision {rev}"
                ));
            }
            payload["why"] = json!(why);
        }
        // 有"外面改过"这一条要说 —— 但**别把上面那句分页说明盖掉**（两种可能同时发生）。
        if let Some((rev, sync_why)) = synced {
            let mut why = payload
                .get("why")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_default();
            if !why.is_empty() {
                why.push_str("。另外：");
            }
            why.push_str(&format!(
                "{sync_why} —— 我已经把它拉进来当作 revision {rev}，**你读到的这份就是最新的**；\
                 历史里也留着那一版，回滚时要跨过它会被拦下"
            ));
            payload["why"] = json!(why);
        }
        payload
    }

    fn tool_edit(&mut self, out: &mut impl io::Write, holder: &Holder, args: &Value) -> Value {
        let id = args.get("instance_id").and_then(Value::as_str).unwrap_or("");
        let old = args.get("old").and_then(Value::as_str).unwrap_or("");
        let new = args.get("new").and_then(Value::as_str).unwrap_or("");
        if self.doc_for("edit", id, holder).is_err() {
            return self.doc_for("edit", id, holder).unwrap_err();
        }
        // **先跟着磁盘走**：用户改过就把它拉进来（成为一条 revision），
        // 然后照常做替换 —— 安全性来自**内容**（`old` 必须唯一命中），不来自时间戳。
        let synced = self.sync_if_changed(out, id, holder);
        let doc = self.docs.get_mut(id).expect("刚查过");
        let before = match doc.edit(old, new) {
            Ok(r) => r,
            Err(e) => return edit_error(e, synced.as_ref().map(|(_, why)| why.as_str())),
        };
        let path = doc.path.clone();
        let content = doc.content().to_string();
        let revision = doc.revision();
        let bytes = doc.bytes();
        if let Err(e) = write_atomic(&path, &content) {
            // 写盘失败要把内存里的那一步退回去，否则内存和磁盘就分家了
            let _ = self.docs.get_mut(id).expect("刚查过").revert(before);
            return json!({
                "route": null, "effect": "refused", "code": "invalid",
                "why": format!("写 `{}` 失败：{e}", path.display()),
                "remedy": "检查文件是不是被别的程序独占；这次编辑已经撤销，磁盘没变"
            });
        }
        // 写成了 ⇒ **记下新的指纹**（不记的话，下一次 edit 会以为磁盘还是旧的）
        self.docs.get_mut(id).expect("刚查过").note_disk();
        let ev = self.mint(
            MessageType::Event,
            "ctrl",
            None,
            Authority::Observe,
            InstanceEvent::state(id, "held")
                .with_path(path.display().to_string())
                .with_detail(json!({"last_op": "edit", "revision": revision, "bytes": bytes}))
                .to_payload(),
        );
        emit(out, &ev);
        json!({
            "route": null, "effect": "confirmed", "code": null,
            "changed": 1,
            // 回滚的钥匙：**这次编辑之前**的版本号（§5.24 的 L1）
            "revision": before,
            "now_revision": revision,
            "bytes": bytes
        })
    }

    fn tool_revert(&mut self, out: &mut impl io::Write, holder: &Holder, args: &Value) -> Value {
        let id = args.get("instance_id").and_then(Value::as_str).unwrap_or("");
        // **先抱怨第一个缺的东西**（顺序有讲究）：`instance_id` 排在前，
        // 因为**没有实例就谈不上 revision**。原来这里先查 `revision`，
        // 于是 `revert {}` 回的是"revision 缺失" —— 模型照着补一个 revision 再来一次，
        // 又撞上"没给 instance_id"：**一次能说清的事拖成两次往返**。
        if id.is_empty() {
            return reply::missing_instance_id("revert");
        }
        // **实例先于 revision**：实例不在（或者不是你持有的），`revision` 根本无从谈起。
        // 顺序反了的代价是实打实的：`revert {instance_id:"x-99"}` 会先抱怨
        // "revision 缺失"，模型补一个 revision 再来 —— 又是两次往返。
        if let Err(v) = self.doc_for("revert", id, holder) {
            return v;
        }
        let to = match args.get("revision").and_then(Value::as_u64) {
            Some(r) => r,
            None => {
                return json!({
                    "route": null, "effect": "refused", "code": "invalid",
                    "why": "revision 缺失或不是非负整数",
                    "remedy": "用 edit 返回的那个 revision；0 表示回到刚打开时的内容"
                })
            }
        };
        // 先跟盘（用户的改动成为历史里的一条 revision），再守 `revert` 那条规矩：
        // **不许跨过用户的改动** —— 它是整份覆盖，跨过去就是悄悄把那次改动丢掉。
        self.sync_if_changed(out, id, holder);
        if let Some(why) = self.docs.get(id).and_then(|d| d.revert_guard(to)) {
            return json!({
                "route": null, "effect": "refused", "code": "refused",
                "why": why,
                "remedy": "先 `read` 看用户改了什么。要保留它，就别退到它之前去；\
                           真要整份覆盖，就把目标选在它之后（或者让用户来定）"
            });
        }
        let doc = self.docs.get_mut(id).expect("刚查过");
        let was = doc.revision();
        if let Err(e) = doc.revert(to) {
            return edit_error(e, None);
        }
        let path = doc.path.clone();
        let content = doc.content().to_string();
        let revision = doc.revision();
        if let Err(e) = write_atomic(&path, &content) {
            let _ = self.docs.get_mut(id).expect("刚查过").revert(was);
            return json!({
                "route": null, "effect": "refused", "code": "invalid",
                "why": format!("写 `{}` 失败：{e}", path.display()),
                "remedy": "磁盘没变，检查文件是不是被独占"
            });
        }
        self.docs.get_mut(id).expect("刚查过").note_disk();
        let ev = self.mint(
            MessageType::Event,
            "ctrl",
            None,
            Authority::Observe,
            InstanceEvent::state(id, "held")
                .with_path(path.display().to_string())
                .with_detail(json!({"last_op": "revert", "revision": revision}))
                .to_payload(),
        );
        emit(out, &ev);
        json!({"route": null, "effect": "confirmed", "code": null, "revision": revision, "restored_from": was})
    }

    fn tool_close(&mut self, out: &mut impl io::Write, holder: &Holder, args: &Value) -> Value {
        let id = args.get("instance_id").and_then(Value::as_str).unwrap_or("");
        if id.is_empty() {
            return reply::missing_instance_id("close");
        }
        let doc = match self.docs.get(id) {
            Some(d) => d,
            None => {
                return reply::no_such_instance("close", id);
            }
        };
        let path = doc.path.clone();
        if !self.locks.release(&path, holder) {
            return json!({
                "route": null, "effect": "refused", "code": "refused",
                "why": format!("`{}` 的写权不在你手里", path.display()),
                "remedy": "只有持有者能关掉它；别人手里的实例不要动"
            });
        }
        self.docs.remove(id);
        let ev = self.mint(
            MessageType::Event,
            "ctrl",
            None,
            Authority::Observe,
            InstanceEvent::close(id)
                .with_detail(json!({"path": path.display().to_string()}))
                .to_payload(),
        );
        emit(out, &ev);
        json!({"route": null, "effect": "confirmed", "code": null, "released": path.display().to_string()})
    }

    fn tool_status(&mut self) -> Value {
        let docs: Vec<Value> = self
            .docs
            .values()
            .map(|d| {
                json!({"instance_id": d.id, "path": d.path.display().to_string(),
                       "revision": d.revision(), "bytes": d.bytes(),
                       "history": d.history_len()})
            })
            .collect();
        let locks: Vec<Value> = self
            .locks
            .entries()
            .map(|(p, e)| {
                json!({"path": p.display().to_string(), "instance": e.instance,
                       "held_by": {"sid": e.holder.sid, "agent": e.holder.agent},
                       "since_ms": e.since_ms})
            })
            .collect();
        json!({
            "route": null, "effect": "confirmed", "code": null,
            "data": {
                "fence": self.fence.roots().iter().map(|r| r.display().to_string()).collect::<Vec<_>>(),
                "fence_empty": self.fence.is_empty(),
                "open_count": docs.len(),
                "open_documents": docs,
                "lock_count": self.locks.len(),
                "locks": locks
            }
        })
    }

    /// 拿到文档，并断言**调用方就是持有者**。
    ///
    /// `tool` 只是用来把话说准（"`read` 没给 `instance_id`"比"这个工具没给"有用）。
    fn doc_for(&self, tool: &str, id: &str, holder: &Holder) -> Result<&Document, Value> {
        // **"你没填"和"它没了"是两种错**（§11.5 的码表要模型做不同的事）——
        // 空的 `instance_id` 原来会渲染成"没有实例 ``"，看起来像坏了。
        // 说法统一放在 `harness_service::reply` 里，终端那边是同一份。
        if id.is_empty() {
            return Err(reply::missing_instance_id(tool));
        }
        let doc = self
            .docs
            .get(id)
            .ok_or_else(|| reply::no_such_instance(tool, id))?;
        match self.locks.get(&doc.path) {
            Some(entry) if &entry.holder == holder => Ok(doc),
            Some(entry) => Err(json!({
                "route": null, "effect": "refused", "code": "refused",
                "why": format!("`{}` 的写权在会话 `{}`（实例 `{}`）手里，不是你",
                               doc.path.display(), entry.holder.sid, entry.holder.agent),
                "held_by": {"sid": entry.holder.sid, "agent": entry.holder.agent},
                "remedy": "拿着别人的 instance_id 也改不了它；自己去 open 一份"
            })),
            None => Err(json!({
                "route": null, "effect": "refused", "code": "refused",
                "why": format!("`{}` 上没有锁了", doc.path.display()),
                "remedy": "这个实例已经失效，重新 open"
            })),
        }
    }
}

/// `synced` = 这次调用进门时**刚从磁盘拉回来一份**（用户改过）——
/// 那 `old` 命不中就很可能是这个原因，所以要说出来，别让它白猜。
fn edit_error(e: EditError, synced: Option<&str>) -> Value {
    let external_hint = synced
        .map(|why| {
            format!(
                "\n⚠️ 而且**磁盘上那份刚被外界改过**（{why}），我已经把它拉进来了 —— \
                 你手上的 `old` 很可能来自更早的版本"
            )
        })
        .unwrap_or_default();
    match e {
        EditError::NotFound => json!({
            "route": null, "effect": "refused", "code": "invalid",
            "why": "`old` 在文件里一处都没命中",
            "remedy": format!("先用 read 看一眼现在的真实内容 —— 你手上的可能已经过期了；\
                              文件本身是空的时候，`old` 要写空串{external_hint}")
        }),
        EditError::Ambiguous { hits } => json!({
            "route": null, "effect": "refused", "code": "invalid",
            "why": format!("`old` 命中了 {hits} 处，不唯一"),
            "remedy": format!("把 `old` 写长一点，长到只可能匹配一处（带上前后文或那行独有的标识）{external_hint}")
        }),
        EditError::UnknownRevision(r) => json!({
            "route": null, "effect": "refused", "code": "invalid",
            "why": format!("revision {r} 不认识"),
            "remedy": "用 edit 返回的那个 revision；0 = 刚打开时的内容。\
                       revert 会丢掉目标之后的历史，所以旧编号可能已经失效"
        }),
    }
}

/// 按**字节**切片，并把边界对齐到字符上（别把 UTF-8 切两半）。
fn slice_bytes(s: &str, offset: usize, limit: Option<usize>) -> (String, Option<usize>) {
    let len = s.len();
    let mut start = offset.min(len);
    while start < len && !s.is_char_boundary(start) {
        start += 1;
    }
    let end = match limit {
        Some(l) => (start + l).min(len),
        None => len,
    };
    let mut end = end;
    while end > start && !s.is_char_boundary(end) {
        end -= 1;
    }
    let next = if end < len { Some(end) } else { None };
    (s[start..end].to_string(), next)
}

/// 原子写：先写临时文件再改名，避免留下半个文件。
fn write_atomic(path: &Path, content: &str) -> io::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| io::Error::other("没有父目录"))?;
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::other("没有文件名"))?
        .to_string_lossy()
        .to_string();
    let tmp: PathBuf = dir.join(format!(".{name}.harness-{}.tmp", std::process::id()));
    std::fs::write(&tmp, content)?;
    std::fs::rename(&tmp, path)
}

fn emit(out: &mut impl io::Write, env: &Envelope) {
    match codec::encode(env) {
        Ok(line) => {
            if let Err(e) = codec::write_line(out, &line) {
                eprintln!("[editor] 写 stdout 失败：{e}");
            }
        }
        Err(e) => eprintln!("[editor] 自己的消息编码失败：{e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **声明的能力和强制的必须逐条对得上**（2026-09-20）。
    ///
    /// 为什么这条值得占一个测试位：manifest 里每个工具写了 `.caps([...])`，
    /// 而执行侧另有一张手写的 `required_caps()`。**两张表一旦漂**，
    /// 后果是"**声明了 `fs.write`、实际什么都没执行**"——
    /// 而门只查声明那一侧，**这个洞没有任何别的东西看得见**。
    /// 反过来漂（强制了一条没声明的）也不对：模型看不到那条能力要求。
    #[test]
    fn the_enforcement_table_matches_what_we_declare() {
        for t in tools() {
            let name = t["name"].as_str().expect("每个工具都有名字");
            let declared: Vec<&str> = t["capabilities"]
                .as_array()
                .expect("每个工具都报到 capabilities（闸门也会查）")
                .iter()
                .filter_map(Value::as_str)
                .collect();
            let enforced: Vec<&str> = required_caps(name).to_vec();
            assert_eq!(
                declared, enforced,
                "`{name}`：manifest 声明的是 {declared:?}，而执行时查的是 {enforced:?} —— \
                 这两张表不许漂"
            );
        }
    }

    /// **自己的 manifest 自己先过一遍闸门。** 这条是构建期断言，
    /// 和"不变量一"那条同一个形状：靠自觉不如靠一条会红的测试。
    #[test]
    fn our_own_manifest_passes_the_contract_gate() {
        let fence = Fence::new(&[]).expect("空围栏是合法的");
        let args = Args {
            instance: "editor:test".into(),
            sid: "s-test".into(),
            kind: "editor".into(),
            fences: Vec::new(),
            inline_limit: 50_000,
        };
        let report = harness_service::contract::check_manifest(&manifest(&args, &fence));
        assert!(report.is_ok(), "{:?}", report.errors);
        assert!(report.missing_optional.is_empty(), "{:?}", report.missing_optional);
    }

    /// **"没有名为 X 的工具"那句话里教的名字，必须就是真报出去的那些**（2026-09-20）。
    ///
    /// 这条抓到过真的：手写的那份名单是
    /// `open | read | edit | revert | close | status | ping` —— 少了 `help` 和 `verify`。
    /// 换句话说：**模型照着这句"别猜了"的话去学，也学不全**。
    #[test]
    fn the_error_message_lists_exactly_the_tools_we_declare() {
        let fence = Fence::new(&[]).expect("空围栏是合法的");
        let args = Args {
            instance: "editor:test".into(),
            sid: "s-test".into(),
            kind: "editor".into(),
            fences: Vec::new(),
            inline_limit: 50_000,
        };
        let declared: Vec<String> = manifest(&args, &fence)["tools"]
            .as_array()
            .expect("manifest 里要有 tools")
            .iter()
            .filter_map(|t| t["name"].as_str().map(str::to_string))
            .collect();
        let taught: Vec<String> = TOOL_NAMES.iter().map(|s| s.to_string()).collect();
        assert_eq!(
            declared, taught,
            "报错里教的名字和 manifest 对不上：{declared:?} vs {taught:?}"
        );
    }

    #[test]
    fn slice_snaps_to_char_boundaries() {
        let s = "aé漢"; // é 两字节，漢 三字节
        let (t, next) = slice_bytes(s, 1, Some(2));
        assert_eq!(t, "é", "别把字符切开");
        assert!(next.is_some());
        let (whole, next) = slice_bytes(s, 0, None);
        assert_eq!(whole, s);
        assert_eq!(next, None);
    }

    #[test]
    fn slice_past_the_end_is_empty_not_panic() {
        let (t, next) = slice_bytes("abc", 99, Some(10));
        assert_eq!(t, "");
        assert_eq!(next, None);
    }

    #[test]
    fn atomic_write_replaces_content() {
        let dir = std::env::temp_dir().join(format!("harness-atomic-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("x.txt");
        std::fs::write(&f, "old").unwrap();
        write_atomic(&f, "new").unwrap();
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "new");
        // 临时文件不许留在那里
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "留下了临时文件：{leftovers:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
