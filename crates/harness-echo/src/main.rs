//! 阶段 1 的 **mock 工具服务**。
//!
//! 它不做真事 —— 它的价值是**能演坏的那部分**（README 阶段 4a）：
//! 正常回复、未知工具、慢调用、假崩。第一版先把前面三种做出来。
//!
//! 它同时是一份**工具服务规范的活样本**：上线时自报 manifest（规范-工具服务 §11.1）。

use harness_core::codec::{self, NdjsonReader, NextLine};
use harness_core::envelope::{Authority, Envelope, MessageType};
use harness_core::id::IdGen;
use harness_core::now_ms;
use harness_service::contract::ToolDecl;
use serde_json::{json, Value};
use std::io::{self, BufReader};

struct Args {
    instance: String,
    sid: String,
    kind: String,
    /// **自报的家门少一项**（`ping` 没了 `input_schema`）—— 注册那道闸门的靶子。
    manifest_holes: bool,
    /// **回来的结果不合自己声明的形**（成功却少了必带的 `data`）——
    /// 投递前那道闸门的靶子。
    ///
    /// **两个开关是分开的**，因为两道闸门管的事不同：家门不全的实例
    /// 中控根本不该放它去接活（那它的结果就永远走不到第二道），
    /// 而第二道要验的是"家门报得好好的、做事却不按自己说的来"。
    bad_result: bool,
    /// **装死**：报完到就把自己挂住，**再也不读 stdin**。
    ///
    /// 它演的是"进程还活着，但不应答"那一种 —— 和"崩了"完全不同：
    /// 崩了会有 EOF（中控立刻知道），挂了只会让探针一直不回话。
    /// 健康探针那条路（`service_unresponsive`）就靠它才测得动。
    freeze: bool,
    /// **谎报沙箱**：声称"带路径的参数都过围栏"，却不报任何围栏 ——
    /// 2026-09-20 新加那条规则（"说了要围却没围栏"）的靶子。
    sandbox_lie: bool,
}

fn parse_args() -> Args {
    let mut instance = String::from("echo:e-1");
    let mut sid = String::from("s-1");
    let mut kind = String::from("echo");

    let mut manifest_holes = false;
    let mut bad_result = false;
    let mut freeze = false;
    let mut sandbox_lie = false;
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--instance" => instance = it.next().unwrap_or(instance),
            "--sid" => sid = it.next().unwrap_or(sid),
            "--kind" => kind = it.next().unwrap_or(kind),
            "--manifest-holes" => manifest_holes = true,
            "--bad-result" => bad_result = true,
            "--freeze" => freeze = true,
            "--sandbox-lie" => sandbox_lie = true,
            _ => {}
        }
    }
    Args {
        instance,
        sid,
        kind,
        manifest_holes,
        bad_result,
        freeze,
        sandbox_lie,
    }
}

fn main() {
    let args = parse_args();
    let mut ids = IdGen::new(format!("{}", args.instance.replace([':', '.'], "-")));
    let mut out = io::stdout();

    // 上线自报家门：**静态的是"我能干什么"**（§5.10）
    let manifest = manifest(&args);

    let hello = Envelope {
        id: ids.next_message(),
        ts: now_ms(),
        kind: MessageType::Presence,
        from: args.instance.clone(),
        to: "ctrl".into(),
        corr: None,
        sid: args.sid.clone(),
        authority: Authority::Observe,
        payload: json!({"state": "online", "manifest": manifest}),
    };
    emit(&mut out, &hello);

    if args.freeze {
        eprintln!("[echo] 按剧本装死：不再读 stdin（探针会一直不回话）");
        loop {
            std::thread::sleep(std::time::Duration::from_secs(3600));
        }
    }

    let mut reader = NdjsonReader::new(BufReader::new(io::stdin()));
    loop {
        match reader.next_line_checked() {
            Ok(NextLine::Line(line)) => handle_line(&mut out, &mut ids, &args, &line),
            // 一行坏字节不该掐死整条通道（同 ctrl）
            Ok(NextLine::Invalid { bytes }) => {
                eprintln!("[echo] 跳过一行非 UTF-8 的输入（{bytes} 字节）")
            }
            // stdin 关了 = 中控收摊（规范 §6.1 规矩四：进程退出 = 会话结束）
            Ok(NextLine::Eof) => break,
            Err(e) => {
                eprintln!("[echo] 读 stdin 失败：{e}");
                break;
            }
        }
    }

    let bye = Envelope {
        id: ids.next_message(),
        ts: now_ms(),
        kind: MessageType::Presence,
        from: args.instance.clone(),
        to: "ctrl".into(),
        corr: None,
        sid: args.sid.clone(),
        authority: Authority::Observe,
        payload: json!({"state": "offline"}),
    };
    emit(&mut out, &bye);
}

fn manifest(args: &Args) -> Value {
    let mut m = json!({
        "contract_version": "1",
        "id": format!("{}-mock", args.kind),
        "version": env!("CARGO_PKG_VERSION"),
        "title": "Echo（阶段 1 的 mock 工具）",
        "kind": args.kind,
        "platforms": ["windows", "linux", "macos"],
        "node_kind": args.kind,
        "state_vocabulary": ["idle", "busy", "closed"],
        "content": ["log"],
        "node_actions": ["close"],
        "instance_types": ["mock"],
        "summary": "只回话，不做事 —— 用来验证三方通路",
        "when_to_use": "阶段 1/4a 的联调；不要指望它改文件",
        "will": "它只是个 mock，什么都不持有、也不落盘 —— 它没了就是没了，没有后事要办。",
        "injection_policy": "card",
        "help_sections": [
            {"id": "overview", "title": "概览"},
            {"id": "tools", "title": "有哪些工具"},
            {"id": "errors", "title": "错误码与补救"}
        ],
        // 参数表与结果形状都是服务自己的义务（规范 §11.1 C 组）
        "tools": tools(),
        // 它是 mock：**什么都不碰** —— 五个面全是"没有"，而这是真话（不是"没管住"）
        "sandbox": {
            "filesystem": "none",
            "network": "none",
            "identity": "same-user",
            "executables": "none",
            "resources": "none"
        },
        "sandbox_note": "它是个 mock：不碰文件、不联网、不拉子进程 —— \
                          五个面全是「没有东西可管」，不是「没管住」。",
        "error_codes": ["invalid", "canceled"]
    });
    if args.manifest_holes {
        // 故意少报一项：`ping` 的参数表没了。中控该在注册时把它指出来。
        if let Some(t) = m["tools"].as_array_mut() {
            if let Some(obj) = t.get_mut(1).and_then(Value::as_object_mut) {
                obj.remove("input_schema");
            }
        }
    }
    if args.sandbox_lie {
        // **说了要围、却没围栏** —— 这条声明是假的，闸门该在注册时拦下
        m["sandbox"]["filesystem"] = json!("fence-args");
    }
    m
}

/// 工具表 —— §11.1 C 组整组。
///
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
        // **它是个 mock：什么都不持有** ⇒ 最适合"按需拉起"（§11.10）
        .lifecycle("per_call")
        .requires_on(&["confirmed"], &["data"])
        .to_json(),
        ToolDecl::new(
            "ping",
            "看看它还在不在。",
            json!({"type": "object", "additionalProperties": false, "properties": {}}),
        )
        .read_only()
        .idempotent()
        .lifecycle("per_call")
        .requires_on(&["confirmed"], &["data"])
        .to_json(),
        ToolDecl::new(
            "echo",
            "把它给你的文本原样回给你。",
            json!({"type": "object", "required": ["text"], "additionalProperties": false,
                   "properties": {"text": {"type": "string"}}}),
        )
        .read_only()
        .idempotent()
        .lifecycle("per_call")
        .returns(json!({"data": {"type": "object"}}))
        // 声明：成功时必带 `data`（文本就在 `data.text` 里）。
        // `--bad-result` 时它**不带** —— 那就是"做事不按自己说的来"。
        .requires_on(&["confirmed"], &["data"])
        .to_json(),
        ToolDecl::new(
            "slow",
            "等一会儿再回 —— 用来试流式分片和超时。",
            json!({"type": "object", "additionalProperties": false,
                   "properties": {"ms": {"type": "integer"}}}),
        )
        .read_only()
        .lifecycle("per_call")
        .returns(json!({"data": {"type": "object"}}))
        // **只在成功时**要 data：被 cancel 的时候它没有结果可给，
        // 只有一句"我确实停了，所以它做了一半"（`partial` + `canceled`）。
        .requires_on(&["confirmed"], &["data"])
        .to_json(),
        ToolDecl::new(
            "boom",
            "**按剧本崩掉**，不回结果 —— 用来试「服务没了」那条路。",
            json!({"type": "object", "additionalProperties": false, "properties": {}}),
        )
        .destructive()
        .lifecycle("per_call")
        .to_json(),
    ]
}

fn handle_line(out: &mut impl io::Write, ids: &mut IdGen, args: &Args, line: &str) {
    let env = match codec::decode(line) {
        Ok(env) => env,
        Err(e) => {
            eprintln!("[echo] 收到不合规的消息：{e}");
            return;
        }
    };

    match env.kind {
        MessageType::Request => handle_request(out, ids, args, &env),
        MessageType::Cancel => {
            // §2.3：尽力而为 + 结果里诚实报告。mock 慢了才会真的收到 cancel。
            let reply = Envelope {
                id: ids.next_message(),
                ts: now_ms(),
                kind: MessageType::Result,
                from: args.instance.clone(),
                to: env.from.clone(),
                corr: env.corr.clone(),
                sid: args.sid.clone(),
                authority: Authority::Observe,
                payload: json!({
                    "route": null, "effect": "partial",
                    "code": "canceled",
                    "remedy": "这次调用已经做了，没法收回；如果它，请先看状态再决定要不要重试"
                }),
            };
            emit(out, &reply);
        }
        _ => {}
    }
}

fn handle_request(out: &mut impl io::Write, ids: &mut IdGen, args: &Args, req: &Envelope) {
    let tool = req.payload.get("tool").and_then(Value::as_str).unwrap_or("");
    let call = req.payload.get("args").cloned().unwrap_or(json!({}));

    // **能力校验在执行侧**（§7）。echo 的四个工具都**不需要能力** ——
    // 它是"什么都不碰"的那一类，所以这条检查在它这儿永远通过。
    // 留着这条路径本身有价值：它证明"零能力的工具"是一条正常的路，不是特例。
    let grant = harness_service::grant::Grant::from_payload(&req.payload);
    let required: &[&str] = &[];
    if !grant.allows(required) {
        let payload = harness_service::grant::refuse_payload(tool, &grant.missing(required), &grant);
        let reply = Envelope {
            id: ids.next_message(),
            ts: now_ms(),
            kind: MessageType::Result,
            from: args.instance.clone(),
            to: req.from.clone(),
            corr: req.corr.clone(),
            sid: args.sid.clone(),
            authority: Authority::Observe,
            payload,
        };
        emit(out, &reply);
        return;
    }

    let payload = match tool {
        "ping" => json!({"route": null, "effect": "confirmed", "code": null, "data": {"pong": true}}),
        "help" => json!({
            "route": null, "effect": "confirmed", "code": null,
            "data": {
                "sections": [
                    {"id": "overview", "title": "概览"},
                    {"id": "tools",    "title": "有哪些工具"},
                    {"id": "errors",   "title": "错误码与补救"}
                ],
                "text": match call.get("section").and_then(Value::as_str).unwrap_or("") {
                    "overview" => "这是个**只回话、不做事**的 mock 工具。它的用途是把'系统的 bug'\
                                   和'模型的 bug'分开：接真实 API 之前，先用它把协议、路由、\
                                   配对这些事验一遍。它不碰文件、不跑命令、不需要任何能力。",
                    "tools" => "· `ping` —— 看它还在不在。\n\
                                · `echo {text}` —— 原样回给你。\n\
                                · `slow {ms}` —— 等一会儿再回：用来试**流式分片**和**超时**。\n\
                                · `boom` —— **按剧本崩掉**，不回结果：用来试'服务没了'那条路。",
                    "errors" => "它只会报 `invalid`（工具名不对）。\
                                 真正有意思的错不在这里 —— 是 `boom` 之后**中控**替你收的那份烂账：\
                                 未决调用会被结掉（`unverifiable` + `service_down`），\
                                 而且有智能体在的话会收到一条敕令。",
                    _ => "章节清单在 data.sections 里；传 section 取某一章。\
                          一次全倒给你会把上下文吃掉（§5.3 渐进披露）。"
                }
            }
        }),
        "echo" => {
            let text = call.get("text").and_then(Value::as_str).unwrap_or("");
            if args.bad_result {
                // **说一套做一套**：它自己声明了"成功必带 data"，这里偏偏不带。
                // 契约闸门的第二道就是为这一下准备的。
                eprintln!("[echo] 按剧本返回一个不合自己声明的结果（少了 data）");
                json!({"route": null, "effect": "confirmed", "code": null})
            } else {
                json!({"route": null, "effect": "confirmed", "code": null, "data": {"text": text}})
            }
        }
        "slow" => {
            // 故意慢：给 cancel 和"服务挂着"留出窗口
            let ms = call.get("ms").and_then(Value::as_u64).unwrap_or(200);
            // 先发一个 chunk，证明流式分片这条链是通的
            let chunk = Envelope {
                id: ids.next_message(),
                ts: now_ms(),
                kind: MessageType::Chunk,
                from: args.instance.clone(),
                to: req.from.clone(),
                corr: req.corr.clone(),
                sid: args.sid.clone(),
                authority: Authority::Observe,
                payload: json!({"text": format!("等 {ms} ms 再回\n")}),
            };
            emit(out, &chunk);
            std::thread::sleep(std::time::Duration::from_millis(ms));
            json!({"route": null, "effect": "confirmed", "code": null, "data": {"waited_ms": ms}})
        }
        // **按剧本崩掉** —— README 阶段 4a 要求 mock"必须能演坏的那部分"。
        // 这里**故意不回任何结果**：让中控去发现它没了，
        // 从而把"未决调用怎么结"和"工具集变了怎么告知"两条路都真的走一遍。
        "boom" => {
            eprintln!("[echo] 按剧本崩掉（不回结果）");
            std::process::exit(3);
        }
        other => {
            // **报错即手册**（§5.5）：事实 + 正确格式 + 出口
            let shown = if other.is_empty() { "(空)" } else { other };
            json!({
                "route": null, "effect": "refused",
                "code": "invalid",
                "why": format!("没有名为 `{shown}` 的工具"),
                "expect": {"tool": "ping | echo | slow | boom", "args": {"text": "给 echo", "ms": "给 slow"}},
                "remedy": "先看 presence 里的 manifest.help_sections，或把 tool 换成上面三个之一"
            })
        }
    };

    let reply = Envelope {
        id: ids.next_message(),
        ts: now_ms(),
        kind: MessageType::Result,
        from: args.instance.clone(),
        to: req.from.clone(),
        corr: req.corr.clone(),
        sid: args.sid.clone(),
        authority: Authority::Observe,
        payload,
    };
    emit(out, &reply);
}

/// 写一行。失败就写 stderr —— **stdout 只写协议消息**（规范 §6.1 规矩二）。
fn emit(out: &mut impl io::Write, env: &Envelope) {
    match codec::encode(env) {
        Ok(line) => {
            if let Err(e) = codec::write_line(out, &line) {
                eprintln!("[echo] 写 stdout 失败：{e}");
            }
        }
        Err(e) => eprintln!("[echo] 自己的消息编码失败：{e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(manifest_holes: bool, bad_result: bool) -> Args {
        Args {
            instance: "echo:test".into(),
            sid: "s-test".into(),
            kind: "echo".into(),
            manifest_holes,
            bad_result,
            freeze: false,
            sandbox_lie: false,
        }
    }

    /// 正常那面：自己的 manifest 得能过闸门。
    #[test]
    fn our_own_manifest_passes_the_contract_gate() {
        let r = harness_service::contract::check_manifest(&manifest(&args(false, false)));
        assert!(r.is_ok(), "{:?}", r.errors);
        assert!(r.missing_optional.is_empty(), "{:?}", r.missing_optional);
    }

    /// **演坏的那面**：它少报一项，闸门就得看得见 ——
    /// 否则"闸门有用"这件事本身没被验证过。
    #[test]
    fn the_misbehaving_manifest_is_actually_nonconforming() {
        let r = harness_service::contract::check_manifest(&manifest(&args(true, false)));
        assert!(!r.is_ok());
        assert!(
            r.errors.iter().any(|e| e.contains("input_schema")),
            "{:?}",
            r.errors
        );
    }

    /// 家门报得好好的时候，它**做出来的事**也可能不合自己声明的形 ——
    /// 这才是第二道闸门存在的理由（第一个开关管不着这一种）。
    #[test]
    fn the_misbehaving_tool_lies_about_its_result_shape() {
        let m = manifest(&args(false, true));
        assert!(
            harness_service::contract::check_manifest(&m).is_ok(),
            "家门本身是合规的"
        );
        let schema = harness_service::contract::tool_schema(&m, "echo").expect("echo 报过形状");
        let lie = json!({"route": null, "effect": "confirmed", "code": null});
        let v = harness_service::contract::check_result("echo:test", "echo", &lie, Some(&schema), &[]);
        assert!(v.iter().any(|s| s.contains("data")), "{v:?}");
    }
}
