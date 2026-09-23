//! **MCP 桥服务**：披着我们的工具服务外衣的 MCP 客户端。
//!
//! 它把"框架"和"整个 MCP 生态"接上，而**不新增任何一种东西**：
//! 对智能体来说它就是又一个工具服务，对中控来说它和其它服务**完全同形**
//! （自报 manifest、受绑定与能力约束、被契约闸门管）。
//!
//! 三个待定项在这里落地（写成决策的依据是"跑起来之后哪个答案不用额外发明"）：
//!
//! * **#49 渐进披露**：`tools/list` **启动时问一次**，然后把每个 MCP 工具
//!   **原样报成我们的工具** —— 于是名片、`help`、能力、绑定那一整套照旧管用。
//!   不把 MCP 工具藏进一个通用调度器（那会丢掉模型被专门训练过的原生调用形态）。
//! * **#54 白名单粒度**：**按工具**（`--allow <name>`，可重复）—— 和我们的工具表同粒度。
//!   **默认拒绝**：不给 `--allow*` 就一个都不许调（同"绑定默认拒绝"那条纪律）。
//! * **#50 健康与重连**：孩子死了 ⇒ 桥**自己报 offline 然后退出**，
//!   于是中控那条现成的 `service_down` 路径接管（结未决调用 + 发敕令）。
//!   **不做自动重连**（v1）—— 写在 `not_implemented` 里，不装作会。

use harness_core::codec::{self, NdjsonReader, NextLine};
use harness_core::envelope::{Authority, Envelope, MessageType};
use harness_core::id::{IdGen, tag_of, water_from_env};
use harness_core::now_ms;
use harness_mcp::client::{content_to_text, McpClient};
use harness_service::contract::ToolDecl;
use harness_service::grant::{self, Grant};
use serde_json::{json, Value};
use std::io::{self, BufReader};
use std::time::Duration;

/// 我们自己的三个诊断入口。**MCP 那边撞了这三个名字就起不来** ——
/// 与其悄悄改名让模型看到一个它没见过的名字，不如**明说**（这条写在 `not_implemented` 里）。
const OUR_TOOLS: [&str; 3] = ["help", "ping", "status"];

struct Args {
    instance: String,
    sid: String,
    kind: String,
    /// MCP server 的命令（程序 + 参数）
    server: Vec<String>,
    /// 允许调的 MCP 工具名；**空 + 没有 allow_all = 一个都不许**
    allow: Vec<String>,
    allow_all: bool,
    timeout_ms: u64,
    /// **给那棵树上的硬限额**（§5.8 决定二的"资源"面）。默认全空 = 不限额。
    limits: harness_service::job::Limits,
}

fn parse_args() -> Result<Args, String> {
    let mut a = Args {
        instance: "mcp:1".into(),
        sid: "s-1".into(),
        kind: "mcp".into(),
        server: Vec::new(),
        allow: Vec::new(),
        allow_all: false,
        timeout_ms: 30_000,
        limits: harness_service::job::Limits::default(),
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--instance" => a.instance = it.next().unwrap_or(a.instance),
            "--sid" => a.sid = it.next().unwrap_or(a.sid),
            "--kind" => a.kind = it.next().unwrap_or(a.kind),
            "--server" => {
                a.server.push(
                    it.next()
                        .ok_or_else(|| "--server 后面要跟一个可执行文件".to_string())?,
                );
            }
            "--arg" => {
                a.server.push(
                    it.next()
                        .ok_or_else(|| "--arg 后面要跟一个参数".to_string())?,
                );
            }
            "--allow" => {
                a.allow.push(
                    it.next()
                        .ok_or_else(|| "--allow 后面要跟一个工具名".to_string())?,
                );
            }
            "--allow-all" => a.allow_all = true,
            "--timeout-ms" => {
                a.timeout_ms = it
                    .next()
                    .and_then(|v| v.parse().ok())
                    .ok_or_else(|| "--timeout-ms 后面要跟毫秒数".to_string())?
            }
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
            other => return Err(format!("不认识的参数 `{other}`")),
        }
    }
    if a.server.is_empty() {
        return Err("没给 MCP server：用 `--server <可执行文件>`（再跟若干 `--arg <参数>`）".into());
    }
    Ok(a)
}

fn main() {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("[mcp] 参数不对：{e}");
            std::process::exit(2);
        }
    };

    // 起对端 → 握手 → 问工具表。**三步分开报错**：起不来 / 谈不成 / 没工具
    // 是三件不同的事，混成一句"连不上"就没人查得动了。
    let mut client = match McpClient::start_with(&args.server, args.limits) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[mcp] 拉不起 MCP server：{}。{}", e.why, e.remedy);
            std::process::exit(3);
        }
    };
    eprintln!(
        "[mcp] 起好了 MCP server（pid {}）：{}",
        client.pid(),
        args.server.join(" ")
    );
    let timeout = Duration::from_millis(args.timeout_ms);
    if let Err(e) = client.initialize(timeout) {
        eprintln!("[mcp] 握手失败：{}。{}", e.why, e.remedy);
        std::process::exit(3);
    }
    let tools = match client.list_tools(timeout) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("[mcp] 问不到工具表：{}。{}", e.why, e.remedy);
            std::process::exit(3);
        }
    };
    let names: Vec<String> = tools
        .iter()
        .filter_map(|t| t.get("name").and_then(Value::as_str).map(str::to_string))
        .collect();
    if let Some(dup) = names.iter().find(|n| OUR_TOOLS.contains(&n.as_str())) {
        eprintln!(
            "[mcp] 那个 MCP server 里有个工具叫 `{dup}` —— 这三个名字（{}）\
             被我们自己的诊断入口占了。这一版**不支持改名**：要么改那个 server，\
             要么换一个桥接方式（`not_implemented.rename`）",
            OUR_TOOLS.join(" / ")
        );
        std::process::exit(3);
    }
    eprintln!(
        "[mcp] 对端 {}（协议 {}）报了 {} 个工具：{}",
        client
            .server_info
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("(没写名字)"),
        client.protocol_version,
        names.len(),
        names.join(" / ")
    );
    for n in &names {
        if !args.allow_all && !args.allow.contains(n) {
            eprintln!("[mcp] `{n}` **不在白名单里** —— 它会被挡下，除非你 `--allow {n}`");
        }
    }

    let manifest = manifest(&args, &client, &tools);
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
            payload: json!({"state": "online", "manifest": manifest}),
        },
    );

    let mut reader = NdjsonReader::new(BufReader::new(io::stdin()));
    loop {
        match reader.next_line_checked() {
            Ok(NextLine::Line(line)) => {
                if !handle_line(&mut out, &mut ids, &args, &mut client, &tools, &line) {
                    break; // 对端没了：报 offline 走人，让中控那条路接管
                }
            }
            Ok(NextLine::Invalid { bytes }) => {
                eprintln!("[mcp] 跳过一行非 UTF-8 的输入（{bytes} 字节）")
            }
            Ok(NextLine::Eof) => break,
            Err(e) => {
                eprintln!("[mcp] 读 stdin 失败：{e}");
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
        Err(e) => eprintln!("[mcp] 发不出去：{e}"),
    }
}

/// **一个工具要什么能力** —— 声明和执行**共用这一个函数**。
///
/// 别的服务是"名片里手写一份、执行时另手写一份"，靠一条测试把两张表钉住。
/// 这里那张表是**算出来的**（`OUR_TOOLS` 之外的一律是 MCP 工具），
/// 所以干脆让两边走同一个函数：**没有第二张表，也就没有漂的余地**。
///
/// * 外来的 MCP 工具：MCP 那边的权限模型我们看不见，只能**整台 server 一个令牌**
///   `mcp.call`。要更细，得先有"每个 MCP 工具一条能力"的声明 —— 那得由部署方写（还没做）。
/// * 我们自己的三个诊断入口（`help` / `ping` / `status`）：**零能力**。
///   它们在那道闸门**之前**就被处理掉了（`match tool` 里直接 `return true`），
///   所以这条返回在运行时够不着 —— 但"够不着"不该靠"没人记得去看"来保证，
///   这里如实写下来：诊断通道不跟着鉴权一起坏。
fn caps_for(tool: &str) -> &'static [&'static str] {
    if OUR_TOOLS.contains(&tool) {
        &[]
    } else {
        &["mcp.call"]
    }
}

fn required_caps(tool: &str) -> &'static [&'static str] {
    caps_for(tool)
}

fn manifest(args: &Args, client: &McpClient, tools: &[Value]) -> Value {
    let mut decls: Vec<Value> = vec![
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
            "看看它（和它背后的 MCP server）还在不在。",
            json!({"type": "object", "additionalProperties": false, "properties": {}}),
        )
        .read_only()
        .idempotent()
        .lifecycle("per_session")
        .requires_on(&["confirmed"], &["data"])
        .to_json(),
        ToolDecl::new(
            "status",
            "看对端是谁、什么版本、报了哪些工具、白名单里放了哪些。",
            json!({"type": "object", "additionalProperties": false, "properties": {}}),
        )
        .read_only()
        .idempotent()
        .lifecycle("per_session")
        .requires_on(&["confirmed"], &["data"])
        .to_json(),
    ];
    // **把每个 MCP 工具原样报成我们的工具**（#49 的答案）
    for t in tools {
        let name = t.get("name").and_then(Value::as_str).unwrap_or("");
        if name.is_empty() {
            continue;
        }
        let desc = t
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or("（这个 MCP 工具没写说明 —— 去问它的提供方）");
        let schema = t
            .get("inputSchema")
            .cloned()
            .unwrap_or_else(|| json!({"type": "object", "properties": {}}));
        decls.push(
            ToolDecl::new(name, desc, schema)
                .caps(caps_for(name))
                // **它可能碰到我们看不见的东西**（网络、别人的服务）—— 如实声明
                .open_world()
                .lifecycle("per_session")
                .requires_on(&["confirmed"], &["output"])
                .to_json(),
        );
    }
    json!({
        "contract_version": "1",
        "id": "mcp",
        "domain": "mcp",
        "version": env!("CARGO_PKG_VERSION"),
        "title": "MCP 桥",
        "kind": args.kind,
        "platforms": ["windows", "linux", "macos"],
        "node_kind": "mcp",
        "state_vocabulary": ["idle"],
        "content": ["log"],
        "node_actions": ["interrupt", "close"],
        "instance_types": [],
        "summary": "把外部 MCP server 的工具接进来 —— 对上层它就是一个普通工具服务",
        "when_to_use": "要用 MCP 生态里现成的东西时。**先 `help` 看白名单规则**：\
                        没在白名单里的工具会被挡下",
        "injection_policy": "card",
        "will": "我是**桥**：我没了通常意味着我拉的那个 MCP server 没了\
                 （或者被谁停了）。它的原话在我的 stderr 里 —— 去看那儿，别猜。",
        "help_sections": [
            {"id": "overview", "title": "概览：桥是什么"},
            {"id": "whitelist", "title": "白名单：为什么有的工具调不动"},
            {"id": "result", "title": "结果怎么映射（`isError` 那条尤其要看）"},
            {"id": "errors", "title": "错误码与补救"}
        ],
        "tools": decls,
        "error_codes": ["invalid", "refused", "timeout", "service_down", "canceled"],
        "owns": {"domain": "mcp-server", "exclusive_per": "process"},
        // ⚠️ 它拉一个**别人写的**子进程，那个子进程继承整台机器的视图。
        // "白名单"管的是**调哪几个工具**，不是"那个进程能碰什么" —— 两件事别混。
        "sandbox": {
            "filesystem": "none",
            "network": "unrestricted",
            "identity": "same-user",
            "executables": "inherit",
            // 和终端同一条纪律：**声明跟着事实走**（开了限额才报 `limits`）
            "resources": if args.limits.any() { "limits" } else { "timeout" }
        },
        "sandbox_note": "**它会跑一个别人写的子进程**：那个进程和你自己跑它的世界一样。\
                          我加的「白名单」管的是**能调哪几个工具**，**不是**「它能碰什么」——\
                          把它当成沙箱看，会得出很危险的结论。",
        "not_implemented": {
            "reconnect": "对端没了我不自动重连 —— 我报 offline 然后退出，\
                          让中控那条现成的 `service_down` 路接管（结未决调用 + 发敕令）。\
                          要重新用它，就重新装配一次",
            "cancel": "MCP 的 `tools/call` 是**同步一问一答**，桥在这一问上阻塞，\
                       所以这一版的 `cancel` 只能在下一条消息里处理（那时它已经做完了）。\
                       真要取消得走 MCP 的 `notifications/cancelled`，没做",
            "rename": "如果那个 MCP server 有工具叫 help/ping/status，现在**起不来**\
                       （那三个名字被诊断入口占了）—— 改名开关没做"
        },
        "mcp": {
            "server": args.server.join(" "),
            "pid": client.pid(),
            "protocol_version": client.protocol_version,
            "server_info": client.server_info,
            "tools": names_of(tools),
            "whitelist": if args.allow_all { json!("*") } else { json!(args.allow) },
        }
    })
}

fn names_of(tools: &[Value]) -> Vec<String> {
    tools
        .iter()
        .filter_map(|t| t.get("name").and_then(Value::as_str).map(str::to_string))
        .collect()
}

/// 返回 `false` = **对端没了，桥该收摊**（让中控那条 `service_down` 路接管）。
fn handle_line(
    out: &mut impl io::Write,
    ids: &mut IdGen,
    args: &Args,
    client: &mut McpClient,
    tools: &[Value],
    line: &str,
) -> bool {
    let env = match codec::decode(line) {
        Ok(env) => env,
        Err(e) => {
            eprintln!("[mcp] 收到不合规的消息：{e}");
            return true;
        }
    };
    match env.kind {
        MessageType::Request => handle_request(out, ids, args, client, tools, &env),
        MessageType::Cancel => {
            reply(
                out,
                ids,
                args,
                &env,
                json!({"route": null, "effect": "confirmed", "code": null,
                       "why": "这条取消到得太晚：MCP 的一问一答是同步的，\
                               我在等它回话时没法处理别的消息",
                       "data": {"canceled": false,
                                "what_to_do": "如果那条调用是长任务，等它回来看结果；\
                                               这一版的 cancel 只有这一种诚实答案"}}),
            );
            true
        }
        _ => true,
    }
}

fn reply(out: &mut impl io::Write, ids: &mut IdGen, args: &Args, req: &Envelope, payload: Value) {
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
    client: &mut McpClient,
    tools: &[Value],
    req: &Envelope,
) -> bool {
    let tool = req.payload.get("tool").and_then(Value::as_str).unwrap_or("");
    let call = req.payload.get("args").cloned().unwrap_or(json!({}));

    // 诊断那三个：不碰对端，零能力也放行（"出问题时要有人能问它一句"）
    match tool {
        "ping" => {
            let alive = client.alive();
            reply(
                out,
                ids,
                args,
                req,
                json!({"route": null,
                       "effect": if alive { "confirmed" } else { "unverifiable" },
                       "code": if alive { Value::Null } else { json!("service_down") },
                       "data": {"pong": alive, "pid": client.pid()}}),
            );
            return alive;
        }
        "status" => {
            let acct = client.account();
            reply(
                out,
                ids,
                args,
                req,
                json!({"route": null, "effect": "confirmed", "code": null,
                       "data": {
                           "server": args.server.join(" "),
                           "pid": client.pid(),
                           "alive": client.alive(),
                           "protocol_version": client.protocol_version,
                           "server_info": client.server_info,
                           "tools": names_of(tools),
                           "whitelist": if args.allow_all { json!("*") } else { json!(args.allow) },
                           // **那棵树的账**：它起了几个进程、峰值多少内存、花了多少 CPU。
                           // 对"别人写的子进程"尤其该看得见 —— 而这是**测量值**，不是推断。
                           "account": {
                               "peak_mem_mb": (acct.peak_mem_bytes as f64 / 1_048_576.0 * 10.0).round() / 10.0,
                               "total_processes": acct.total_processes,
                               "cpu_time_ms": acct.cpu_time_ms,
                               "limits": {
                                   "mem_mb": args.limits.mem_bytes.map(|b| b / 1_048_576),
                                   "max_procs": args.limits.max_procs,
                                   "cpu_time_ms": args.limits.cpu_time_ms
                               }
                           }
                       }}),
            );
            return true;
        }
        "help" => {
            let section = call.get("section").and_then(Value::as_str).unwrap_or("");
            reply(
                out,
                ids,
                args,
                req,
                json!({"route": null, "effect": "confirmed", "code": null,
                       "data": {
                           "sections": [
                               {"id": "overview", "title": "概览：桥是什么"},
                               {"id": "whitelist", "title": "白名单：为什么有的工具调不动"},
                               {"id": "result", "title": "结果怎么映射"},
                               {"id": "errors", "title": "错误码与补救"}
                           ],
                           "text": help_text(section, args, tools)
                       }}),
            );
            return true;
        }
        _ => {}
    }

    // **能力**：MCP 那边我们看不见权限模型，这边就一个令牌 `mcp.call`
    let g = Grant::from_payload(&req.payload);
    let required = required_caps(tool);
    if !g.allows(required) {
        let payload = grant::refuse_payload(tool, &g.missing(required), &g);
        reply(out, ids, args, req, payload);
        return true;
    }

    // 它是不是对端真有这个工具？
    let known = names_of(tools);
    if !known.iter().any(|n| n == tool) {
        reply(
            out,
            ids,
            args,
            req,
            json!({"route": null, "effect": "refused", "code": "invalid",
                   "why": format!("这个 MCP server 上没有名为 `{tool}` 的工具"),
                   "remedy": format!("它有的是：{}。**别猜名字** —— \
                                      它们的参数表在名片里（`list` 看得见）",
                                      if known.is_empty() { "（一个都没有）".to_string() }
                                      else { known.join(" / ") })}),
        );
        return true;
    }

    // **白名单**（#54：按工具，默认拒绝）
    if !args.allow_all && !args.allow.contains(&tool.to_string()) {
        reply(
            out,
            ids,
            args,
            req,
            json!({"route": null, "effect": "refused", "code": "refused",
                   "why": format!("`{tool}` **不在白名单里**"),
                   "remedy": format!(
                       "白名单是**部署方**定的（这是默认拒绝：不写就是不许）。\
                        现在的白名单：{}。要放行它，让用户在服务表里给这个桥加 \
                        `--allow {tool}`（或者 `--allow-all`，那等于放弃这一步把关）",
                       if args.allow.is_empty() {
                           "（空的）".to_string()
                       } else {
                           args.allow.join(" / ")
                       }
                   )}),
        );
        return true;
    }

    // 真的调它
    let started = std::time::Instant::now();
    match client.call_tool(tool, call, Duration::from_millis(args.timeout_ms)) {
        Ok(result) => {
            let text = content_to_text(result.get("content").unwrap_or(&Value::Null));
            let is_error = result
                .get("isError")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            // **"工具失败 ≠ 调用失败"**（同终端那条：exit 3 也还是 confirmed）。
            // `isError` 是**结果里的数据**，不是传输层的错 —— 所以 effect 照样 confirmed，
            // 而"它说自己失败了"这件事放在 `data` 里，一字不改地给模型看。
            reply(
                out,
                ids,
                args,
                req,
                // **`route: "process"`**：这份账是关于"**交给了另一个进程**"那条路的 ——
                // 不是同步系统调用（`null`），也不属于那五档 GUI 投递方式。
                json!({"route": "process", "effect": "confirmed", "code": null,
                       "output": text,
                       "data": {
                           "mcp": result,
                           "is_error": is_error,
                           "server": client
                               .server_info
                               .get("name")
                               .and_then(Value::as_str)
                               .unwrap_or("mcp"),
                           "ms": started.elapsed().as_millis() as u64
                       }}) ,
            );
            true
        }
        Err(e) => {
            // 对端没了 ⇒ **不自己编一条答复**，退出让中控那条现成的路接管
            // （它会结掉未决调用、发敕令、把实例收掉）。自己答一条只会让
            // 同一件事有两个说法。
            if e.code == "child_down" {
                eprintln!("[mcp] 对端没了：{}。{}", e.why, e.remedy);
                eprintln!("[mcp] 我不自己编答复 —— 报 offline 退出，让中控那条路接管");
                return false;
            }
            let (effect, code) = match e.code {
                // **超时是"我不知道结果"**，不是"它失败了"（§11.2 那条）
                "timeout" => ("unverifiable", "timeout"),
                "refused" => ("refused", "refused"),
                _ => ("refused", "invalid"),
            };
            reply(
                out,
                ids,
                args,
                req,
                // 超时/对端拒绝都在**"交给另一个进程"**这条路上，
                // 所以 `route` 是 `process` 而不是 `null`（§11.2.2 那条不变量）
                json!({"route": "process", "effect": effect, "code": code,
                       "why": e.why, "remedy": e.remedy}),
            );
            true
        }
    }
}

fn help_text(section: &str, args: &Args, tools: &[Value]) -> String {
    match section {
        "overview" => format!(
            "我是一个**桥**：一头是我们这套协议（智能体 / 中控），另一头是一个外部 \
             **MCP server**（子进程，JSON-RPC over stdio）。\n\n\
             对上层来说我**就是又一个工具服务** —— 自报 manifest、受绑定与能力约束、\
             结果过契约闸门。差别只有一处：**我手里的工具是别人写的**。\n\n\
             我背后那个：`{}`（pid {}，协议 {}）\n\
             它报了 {} 个工具：{}",
            args.server.join(" "),
            "见 `status`",
            "见 `status`",
            tools.len(),
            names_of(tools).join(" / ")
        ),
        "whitelist" => format!(
            "**默认拒绝**：不在白名单里的 MCP 工具，我一条都不许调。\n\n\
             现在的白名单：{}\n\n\
             为什么默认拒绝：白名单是**部署方**定的，而我是照着别人的工具表干活的 —— \
             那个 server 明天多报一个工具，我不该自动就把它的权限提上去。\n\
             （同「绑定默认拒绝」那条纪律。）\n\n\
             要放行某个工具：让用户在服务表里给我加 `--allow <工具名>`；\
             `--allow-all` 等于放弃这一步把关。",
            if args.allow_all {
                "**全部**（`--allow-all`）".to_string()
            } else if args.allow.is_empty() {
                "（空的 —— 一个都不许调）".to_string()
            } else {
                args.allow.join(" / ")
            }
        ),
        "result" => "MCP 的结果长这样：`result.content` 是一块一块的内容，\
                     外加一个 `isError`。我这么映射：\n\n\
                     · `content` 里的 **text 块**拼成结果的 `output`（你直接读它）；\n\
                     · `isError: true` ⇒ **`effect` 仍然是 `confirmed`**，\
                       而 `data.is_error` 是 `true` —— 因为**工具失败不等于调用失败**：\
                       它确实跑了、也确实告诉你它失败了，那是**数据**，不是传输错。\n\
                       （同终端那条：命令 exit 3 也还是 confirmed。）\n\
                     · `content` 里**不是文本**的块（图片之类）我这一版只搬进 `data.mcp`，\
                         并在 `output` 里**留一行说清「这块我不认识」** —— \
                       不许假装看见了（丢了的话你会以为对端什么都没回）。\n\
                     · 原始结果整个留在 `data.mcp` 里，要细看就看它。"
            .to_string(),
        "errors" => "· `refused` + 白名单的话术 ⇒ **它不在白名单里**（部署方没放行）。别重试。\n\
                     · `refused` + `mcp.call` ⇒ **你没这个能力**。能力是中控签发的，别重试。\n\
                     · `refused` + \"MCP 那边回了错\" ⇒ 那是**对端**的拒绝\
                       （比如它没有这个工具），按它的话改。\n\
                     · `timeout` 且 `effect: unverifiable` ⇒ **不知道它做没做**。\
                       别直接重试（它可能正在做）；先看状态或它的 stderr。\n\
                     · 你看到我整个从图上消失 ⇒ **我背后那个 MCP server 没了**。\
                       它的原话在我的 stderr 里；我不会自己重连。"
            .to_string(),
        _ => "章节清单在 `data.sections` 里；传 `section` 取某一章。\
              一次全倒给你会把上下文吃掉（渐进披露）。"
            .to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **#87 能力声明与能力强制必须对得上**（2026-09-20，第十二个 crate 的版本）。
    ///
    /// 这个桥没法照抄别处那条测试（遍历 `tools()` 逐条比对）：它的能力表是**算出来的**，
    /// 而且 `manifest()` 要一个活着的 MCP 客户端才能跑。
    /// 所以这里改用**更轻、但同样硬**的判据钉住那条性质：
    ///
    /// 1. 我们自己的三个诊断入口**零能力**（它们在那道闸门之前就 `return` 了，
    ///    但把它写成零能力，"诊断通道不跟着鉴权一起坏"就成了一条可查的声明）；
    /// 2. 除此之外**一律 `mcp.call`** —— 包括那个名字我们还不认识的时候
    ///    （报错路径也不许绕过令牌：不认识的名字走的是"对端没这个工具"，
    ///    而那道判断在那道闸门**之后**）。
    ///
    /// 真正防漂的那一手在 `caps_for()`：`manifest()` 里 `.caps(caps_for(name))`
    /// 和执行侧 `required_caps(tool)` 是**同一个函数**，没有第二张表可漂。
    /// 这条测的就是"那个函数没被改歪"。
    #[test]
    fn only_foreign_tools_need_the_mcp_call_token() {
        for own in OUR_TOOLS {
            assert!(
                caps_for(own).is_empty(),
                "诊断入口 `{own}` 报出了能力 {:?} —— 它必须零能力",
                caps_for(own)
            );
            assert_eq!(required_caps(own), caps_for(own), "`{own}` 的声明与执行对不上");
        }
        for foreign in ["read_file", "search", "某个我们还不认识的名字", ""] {
            assert_eq!(
                required_caps(foreign),
                &["mcp.call"],
                "外来工具 `{foreign}` 必须整台 server 一个令牌"
            );
        }
    }
}
