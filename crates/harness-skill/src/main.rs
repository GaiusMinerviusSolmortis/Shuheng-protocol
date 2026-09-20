//! **技能服务**：把 01-内核 §3 那张清单里的"技能"做成一个能被中控拉起的工具实例。
//!
//! 它要证的是**渐进披露真的能省上下文**，而不是一句口号：
//!
//! ```text
//!   list           → 名字 + 一句话            （名片，进提示词的那一层）
//!   open {skill}   → 章节目录                （"这本手册有哪些章"）
//!   open {.., ch}  → 那一章的正文（可分行分页） （真正要看的时候才拿）
//! ```
//!
//! 和记忆服务（`harness-mem`）对照着看，能看出**同一个"令牌 + scope"在三个域里
//! 长成三种样子**：文件域 scope 是**围栏**，记忆域 scope 是**锚点**，
//! 技能域 scope 是**这个根目录里放了什么**（v1 就到此为止 —— 要"只给某个智能体
//! 看其中几份"，现在还没有机制，那条写在 `not_implemented` 里）。

use harness_core::codec::{self, NdjsonReader, NextLine};
use harness_core::envelope::{Authority, Envelope, MessageType};
use harness_core::id::IdGen;
use harness_core::now_ms;
use harness_service::contract::ToolDecl;
use harness_service::grant::{self, Grant};
use harness_skill::loader::{page, scan, Catalog};
use serde_json::{json, Value};
use std::io::{self, BufReader};
use std::path::PathBuf;

/// 一页给多少行。**这个数是"给模型的"**：太大等于没分页，太小会让人来回要。
const PAGE_LINES: u64 = 120;

struct Args {
    instance: String,
    sid: String,
    kind: String,
    root: PathBuf,
    /// 只在启动时扫一次吗？默认**每次调用重扫** ——
    /// 技能是**文件**，写它的人不会来通知你；重扫一个目录很便宜，
    /// 而"刚写的技能要重启服务才看得见"是个很讨厌的意外。
    rescan_every: bool,
}

fn parse_args() -> Result<Args, String> {
    let mut instance = String::from("skill:1");
    let mut sid = String::from("s-1");
    let mut kind = String::from("skill");
    let mut root = PathBuf::from("skills");
    let mut rescan_every = true;
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--instance" => instance = it.next().unwrap_or(instance),
            "--sid" => sid = it.next().unwrap_or(sid),
            "--kind" => kind = it.next().unwrap_or(kind),
            "--root" => {
                root = it
                    .next()
                    .map(PathBuf::from)
                    .ok_or_else(|| "--root 后面要跟一个目录".to_string())?
            }
            "--scan-once" => rescan_every = false,
            other => return Err(format!("不认识的参数 `{other}`；--help 看用法")),
        }
    }
    Ok(Args {
        instance,
        sid,
        kind,
        root,
        rescan_every,
    })
}

fn main() {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("[skill] 参数不对：{e}");
            std::process::exit(2);
        }
    };
    let cat = scan(&args.root);
    eprintln!(
        "[skill] 技能根目录：{}（{} 份技能{}）",
        args.root.display(),
        cat.skills.len(),
        if cat.skipped.is_empty() {
            String::new()
        } else {
            format!("，{} 份坏了被跳过", cat.skipped.len())
        }
    );
    for s in &cat.skipped {
        eprintln!("[skill] 跳过 `{}`：{}", s.dir, s.why);
    }

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
            payload: json!({"state": "online", "manifest": manifest(&args, &cat)}),
        },
    );

    let mut cache = cat;
    let mut reader = NdjsonReader::new(BufReader::new(io::stdin()));
    loop {
        match reader.next_line_checked() {
            Ok(NextLine::Line(line)) => handle_line(&mut out, &mut ids, &args, &mut cache, &line),
            Ok(NextLine::Invalid { bytes }) => {
                eprintln!("[skill] 跳过一行非 UTF-8 的输入（{bytes} 字节）")
            }
            Ok(NextLine::Eof) => break,
            Err(e) => {
                eprintln!("[skill] 读 stdin 失败：{e}");
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

fn emit(out: &mut impl io::Write, env: &Envelope) {
    match codec::encode(env) {
        Ok(line) => {
            let _ = out.write_all(line.as_bytes());
            let _ = out.write_all(b"\n");
            let _ = out.flush();
        }
        Err(e) => eprintln!("[skill] 发不出去：{e}"),
    }
}

/// **每个工具要什么能力** —— 工具自己接口的一部分（§7）。
///
/// 读技能要 `skill.read`：它不碰你的文件、也不改东西，但它**会把内容送进上下文** ——
/// 而"上下文里能出现什么"本身就是一条边界（§7.1 那句"读本身就等于泄漏"）。
fn required_caps(tool: &str) -> &'static [&'static str] {
    match tool {
        "list" | "open" | "search" => &["skill.read"],
        "help" | "ping" | "status" => &[],
        _ => &[],
    }
}

fn manifest(args: &Args, cat: &Catalog) -> Value {
    json!({
        "contract_version": "1",
        "id": "skill",
        "domain": "knowledge",
        "version": env!("CARGO_PKG_VERSION"),
        "title": "技能（知识）",
        "kind": args.kind,
        "platforms": ["windows", "linux", "macos"],
        "node_kind": "skill",
        "state_vocabulary": ["idle"],
        "content": ["log"],
        "node_actions": ["interrupt", "close"],
        "instance_types": [],
        "summary": "一个目录里的「怎么做某类事」：按章借出去，不一次倒完",
        "when_to_use": "遇到不熟的做法时**先 `list` 看一眼有没有现成的**，\
                        别自己从头摸一遍 —— 但也别把整本都拉进上下文",
        "injection_policy": "card",
        "will": "我没有状态，也不需要后事 —— 技能就是那个目录里的文件，\
                 进程没了文件还在，换个进程指着同一个 `--root` 就能接着用。",
        "help_sections": [
            {"id": "overview", "title": "概览：这一层是干什么的"},
            {"id": "layout",   "title": "技能目录长什么样（两种形状都收）"},
            {"id": "read",     "title": "怎么读，以及为什么按章"},
            {"id": "errors",   "title": "错误码与补救"}
        ],
        "tools": tools(),
        "error_codes": ["invalid", "refused"],
        "owns": {"domain": "skill-root", "exclusive_per": "none"},
        // 它只读自己的 `--root`：不碰你的文件、不联网、不拉子进程。
        "sandbox": {
            "filesystem": "none",
            "network": "none",
            "identity": "same-user",
            "executables": "none",
            "resources": "none"
        },
        "sandbox_note": "它只读 `--root` 那个目录 —— 不碰你的项目、不联网、不拉子进程。\
                          这里没有围栏要报（`fence` 是空的，这是真的）。",
        "not_implemented": {
            "scope": "v1 不做「只给某个智能体看其中几份技能」—— 范围就是 `--root` 里放了什么；\
                      要更细，得先有「技能级的绑定」，那条还没定",
            "search": "字面匹配，不是语义检索（同记忆那一层的取舍）"
        },
        "skills": cat.cards().len(),
        "skills_skipped": cat.skipped.len()
    })
}

fn tools() -> Vec<Value> {
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
            "看这个技能根：在哪、几份技能、有没有坏掉被跳过的。",
            json!({"type": "object", "additionalProperties": false, "properties": {}}),
        )
        .read_only()
        .idempotent()
        .lifecycle("per_session")
        .requires_on(&["confirmed"], &["data"])
        .to_json(),
        ToolDecl::new(
            "list",
            "列出有哪些技能：名字 + 一句话 + 章节目录。**正文一个字都不给**。",
            json!({"type": "object", "additionalProperties": false, "properties": {}}),
        )
        .read_only()
        .idempotent()
        .caps(&["skill.read"])
        .lifecycle("per_session")
        .returns(json!({"data": {"type": "object"}}))
        .requires_on(&["confirmed"], &["data"])
        .to_json(),
        ToolDecl::new(
            "open",
            "取一份技能：不带 `chapter` 给章节目录，带了给那一章（可以按行分页）。",
            json!({"type": "object", "required": ["skill"], "additionalProperties": false,
                   "properties": {
                       "skill": {"type": "string"},
                       "chapter": {"type": "string", "description": "章节 id；不带就先给目录"},
                       "from_line": {"type": "integer", "description": "从第几行开始（1 起）"},
                       "lines": {"type": "integer",
                                 "description": format!("这一页要几行，默认 {PAGE_LINES}")}
                   }}),
        )
        .read_only()
        .idempotent()
        .caps(&["skill.read"])
        .lifecycle("per_session")
        .returns(json!({"data": {"type": "object"}}))
        .requires_on(&["confirmed"], &["data"])
        .to_json(),
        ToolDecl::new(
            "search",
            "在技能里**字面**找一段话：给命中的技能 / 章 / 行号，不给全文。",
            json!({"type": "object", "required": ["query"], "additionalProperties": false,
                   "properties": {
                       "query": {"type": "string"},
                       "limit": {"type": "integer", "description": "最多给几条，默认 10"}
                   }}),
        )
        .read_only()
        .idempotent()
        .caps(&["skill.read"])
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
    cache: &mut Catalog,
    line: &str,
) {
    let env = match codec::decode(line) {
        Ok(env) => env,
        Err(e) => {
            eprintln!("[skill] 收到不合规的消息：{e}");
            return;
        }
    };
    if env.kind != MessageType::Request {
        return;
    }
    handle_request(out, ids, args, cache, &env);
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
    cache: &mut Catalog,
    req: &Envelope,
) {
    let tool = req.payload.get("tool").and_then(Value::as_str).unwrap_or("");
    let call = req.payload.get("args").cloned().unwrap_or(json!({}));

    // **能力校验在执行侧**（§7）—— 中控只管签发，不知道 open 要什么
    let g = Grant::from_payload(&req.payload);
    let required = required_caps(tool);
    if !g.allows(required) {
        let payload = grant::refuse_payload(tool, &g.missing(required), &g);
        reply_to(out, ids, args, req, payload);
        return;
    }
    // **重扫**：技能是文件，写它的人不会来通知你（默认每次调用重扫）
    if args.rescan_every {
        *cache = scan(&args.root);
    }

    let payload = match tool {
        "ping" => json!({"route": null, "effect": "confirmed", "code": null,
                         "data": {"pong": true, "root": args.root.display().to_string(),
                                  "skills": cache.skills.len()}}),
        "status" => json!({"route": null, "effect": "confirmed", "code": null,
                           "data": {
                               "root": args.root.display().to_string(),
                               "skills": cache.skills.iter().map(|s| json!({
                                   "name": s.name, "version": s.version,
                                   "chapters": s.chapters.len(),
                                   "from": s.from.display().to_string()
                               })).collect::<Vec<_>>(),
                               // **跳过的不许藏**：少了三份技能和"那三份坏了"是两件事
                               "skipped": cache.skipped.iter().map(|s| json!({
                                   "dir": s.dir, "why": s.why
                               })).collect::<Vec<_>>()
                           }}),
        "list" => json!({"route": null, "effect": "confirmed", "code": null,
                         "data": {"skills": cache.cards(),
                                  "skipped": cache.skipped.len(),
                                  "hint": "这是**名片**，不是正文 —— 要看某一章就 `open` 它。\
                                           一次全拿会把上下文吃掉（§5.3 渐进披露）"}}),
        "open" => {
            let name = call.get("skill").and_then(Value::as_str).unwrap_or("");
            let Some(s) = cache.get(name) else {
                let names: Vec<&str> = cache.skills.iter().map(|s| s.name.as_str()).collect();
                let payload = json!({"route": null, "effect": "refused", "code": "invalid",
                                     "why": format!("没有名为 `{name}` 的技能"),
                                     "remedy": format!("这个根目录里有这些：{}。\
                                                       先 `list` 看一眼 —— 别猜名字",
                                                       if names.is_empty() { "（一份都没有）".to_string() }
                                                       else { names.join(" / ") })});
                reply_to(out, ids, args, req, payload);
                return;
            };
            match call.get("chapter").and_then(Value::as_str) {
                None => json!({"route": null, "effect": "confirmed", "code": null,
                               "data": {
                                   "skill": s.name, "title": s.title, "version": s.version,
                                   "summary": s.summary, "when_to_use": s.when_to_use,
                                   "chapters": s.chapters.iter().map(|c| json!({
                                       "id": c.id, "title": c.title,
                                       "lines": c.text.lines().count()
                                   })).collect::<Vec<_>>(),
                                   "hint": "选一章再 `open` —— **别把每一章都拉一遍**，\
                                            你只需要你正要干的那件事那一章"
                               }}),
                Some(cid) => match s.chapters.iter().find(|c| c.id == cid) {
                    Some(c) => {
                        let from = call.get("from_line").and_then(Value::as_u64).unwrap_or(1);
                        let lines = call.get("lines").and_then(Value::as_u64).unwrap_or(PAGE_LINES);
                        let (text, next, total) = page(&c.text, from, lines);
                        json!({"route": null, "effect": "confirmed", "code": null,
                               "data": {"skill": s.name, "chapter": c.id, "title": c.title,
                                        "from_line": from, "lines_total": total,
                                        "text": text, "next_line": next,
                                        "hint": if next.is_some() {
                                            "还有下文：带 `from_line` = next_line 再来一页。\
                                             别一次要完 —— 你要的是做法，不是把这章背下来"
                                        } else {
                                            "这一章给完了"
                                        }}})
                    }
                    None => {
                        let ids_: Vec<&str> = s.chapters.iter().map(|c| c.id.as_str()).collect();
                        json!({"route": null, "effect": "refused", "code": "invalid",
                               "why": format!("`{}` 里没有名为 `{cid}` 的章节", s.name),
                               "remedy": format!("它有这些章：{}", ids_.join(" / "))})
                    }
                },
            }
        }
        "search" => {
            let q = call.get("query").and_then(Value::as_str).unwrap_or("");
            if q.trim().is_empty() {
                json!({"route": null, "effect": "refused", "code": "invalid",
                       "why": "`query` 是空的",
                       "remedy": "给一个词或一句话；它是**字面**匹配，用你记得住的原文"})
            } else {
                let limit = call.get("limit").and_then(Value::as_u64).unwrap_or(10) as usize;
                let needle = q.to_lowercase();
                let mut hits = Vec::new();
                'outer: for s in &cache.skills {
                    for c in &s.chapters {
                        for (i, line) in c.text.lines().enumerate() {
                            if line.to_lowercase().contains(&needle) {
                                hits.push(json!({
                                    "skill": s.name, "chapter": c.id, "line": i + 1,
                                    // 只给那一行 —— 命中处才是要看的，不是整章
                                    "text": line.trim()
                                }));
                                if hits.len() >= limit {
                                    break 'outer;
                                }
                            }
                        }
                    }
                }
                json!({"route": null, "effect": "confirmed", "code": null,
                       "data": {"query": q, "hits": hits,
                                "hint": "命中给的是**行号** —— 要看上下文就 `open` 那一章的\
                                         那一行附近。字面匹配，同义说法认不出来"}})
            }
        }
        "help" => {
            let section = call.get("section").and_then(Value::as_str).unwrap_or("");
            json!({"route": null, "effect": "confirmed", "code": null,
                   "data": {
                       "sections": [
                           {"id": "overview", "title": "概览：这一层是干什么的"},
                           {"id": "layout",   "title": "技能目录长什么样（两种形状都收）"},
                           {"id": "read",     "title": "怎么读，以及为什么按章"},
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
        "overview" => "我是**技能**：`--root` 那个目录里的「怎么做某类事」，一份一份借给你。\n\n\
                       为什么要有我：有些做法**不值得每一次都重新摸一遍**（怎么录 adb 调试视频、\
                       某个库的坑在哪）—— 但也没必要一上来就灌进上下文。\
                       所以读法是三段：`list` 看名片 → `open` 看章节目录 → `open` 看**那一章**。"
            .to_string(),
        "layout" => format!(
            "两种形状都收（**不为格式重写一遍别人的东西**）：\n\n\
             ① 一个 `skill.json` + 若干 `.md`：\n\
             ```json\n\
             {{\"name\":\"adb-record\",\"title\":\"录安卓调试视频\",\"version\":\"1\",\n\
              \"summary\":\"一句话\",\"when_to_use\":\"什么时候该用它\",\n\
              \"chapters\":[{{\"id\":\"quickstart\",\"title\":\"快速开始\",\"file\":\"01.md\"}}]}}\n\
             ```\n\
             ② **只有一个 `SKILL.md`**：按 `## ` 标题**自动切章**。\
             现成的技能目录大多是这个形状，直接抄进来就能用。\n\n\
             当前根目录：`{}`",
            args.root.display()
        ),
        "read" => "`list` → 名字 + 一句话 + 章节目录（**正文一个字都没有**）\n\
                   `open {skill}` → 章节目录（带每章多少行）\n\
                   `open {skill, chapter, from_line?, lines?}` → 那一章的正文；\
                   一页默认 120 行，**给不完会回 `next_line`**，接着要就是了\n\
                   `search {query}` → 命中处（技能 / 章 / **行号** / 那一行）\n\n\
                   为什么按行分页而不是按字节：**字节会劈开一个汉字**。\n\
                   为什么不让一次拿完：一章可能就是几万字 —— 你要的是做法，不是背下来。"
            .to_string(),
        "errors" => "· `invalid` —— 技能名或章节 id 不对（`remedy` 里会把**有哪些**列出来，\
                     照着说就行，别猜）。\n\
                     · `refused` —— **你没这个资格**（`skill.read`）。能力是中控签发的，\
                     要么让用户签给你，要么这件事先别做。**别重试。**\n\
                     · `status` 里的 `skipped` —— 那是**坏掉被跳过的技能目录**：\
                     少了三份技能和「那三份坏了」是两件完全不同的事，所以我把它报出来。"
            .to_string(),
        _ => "章节清单在 `data.sections` 里；传 `section` 取某一章。\
              一次全倒给你会把上下文吃掉（渐进披露）。"
            .to_string(),
    }
}
