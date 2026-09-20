//! **真实模型适配器**：DeepSeek（OpenAI 兼容）。
//!
//! 它做的事只有一件 —— **把我们内部的 `Item` 序列翻译成 provider 的报文，
//! 再把回包翻译回来**。翻译层存在的意义，就是让记忆内核和循环都不用知道
//! tool_call_id、finish_reason 这些 provider 特有的东西。
//!
//! 三条容易踩的地方，都在这里有对应的处理：
//!
//! 1. **函数名有字符集限制**（`[A-Za-z0-9_-]{1,64}`），而我们的实例 id 是
//!    `term:1` 这种 —— 所以要**改名 + 留一张反查表**；
//! 2. `arguments` 是**字符串**，不是对象；
//! 3. 一次助手回复里的多个 `tool_calls` 属于**同一条 assistant 消息** ——
//!    所以要把连续几条 `ToolCall` 项合并成一条。

use crate::model::{Model, ModelReply, ModelRequest, ToolCall};
use harness_memory::ItemKind;
use serde_json::{json, Value};
use std::collections::HashMap;

pub struct DeepSeekModel {
    http: ureq::Agent,
    endpoint: String,
    api_key: String,
    model: String,
    max_tokens: u64,
    /// 这次请求用的"函数名 → (实例, 工具)"反查表
    names: HashMap<String, (String, String)>,
}

/// provider 对函数名有字符集限制，实例 id 里却有 `:` —— 改个名，再查回来。
fn sanitize(instance: &str) -> String {
    let mut s: String = instance
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    if s.len() > 40 {
        s.truncate(40);
    }
    s
}

fn fn_name(instance: &str, tool: &str) -> String {
    let n = format!("{}__{}", sanitize(instance), tool);
    if n.len() > 64 {
        n[..64].to_string()
    } else {
        n
    }
}

/// **模型调了一个我不认识的函数名** —— 把真有的名字列给它（报错即手册）。
///
/// 为什么这条要单独写：模型**真的会**把带前缀的名字记错或截断（实跑里它把
/// `editor_1__edit` 叫成了 `__edit`）。那时候唯一有用的回答是
/// "没有这个名字；我提供的是这些：…"，而不是让它等一个不会来的结果。
fn unknown_name_manual(names: &HashMap<String, (String, String)>, name: &str) -> String {
    let mut have: Vec<&str> = names.keys().map(String::as_str).collect();
    have.sort_unstable();
    let shown = if have.len() > 30 {
        format!(
            "{}（还有 {} 个，先看名片）",
            have[..30].join(" / "),
            have.len() - 30
        )
    } else {
        have.join(" / ")
    };
    format!(
        "**[没有这个工具名]** 你调的是 `{name}`，我给的名字里没有它。\n\
         - **这次调用没有发生**：没有任何工具知道你来过 —— 别以为它做了一半。\n\
         - 我提供的是：{shown}\n\
         - 名字的形状是 `<实例>__<工具>`（实例里的冒号被换成了下划线）—— **照抄，别简写**。"
    )
}

/// 把 provider 回的那条 `message` 里的 `tool_calls` 翻成我们的形状。
///
/// 单独抽出来是因为**它是最容易出错的一小段**（名字反查、参数是字符串再解一次），
/// 而它原来埋在 `reply()` 里 —— 那个函数要发 HTTP，测不动。
fn parse_tool_calls(
    names: &HashMap<String, (String, String)>,
    msg: &Value,
) -> Vec<ToolCall> {
    msg.get("tool_calls")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|c| {
                    let id = c.get("id").and_then(Value::as_str).unwrap_or("call_0");
                    let f = c.get("function")?;
                    let name = f.get("name")?.as_str()?;
                    let args_raw = f.get("arguments").and_then(Value::as_str).unwrap_or("{}");
                    // ⚠️ **名字查不到时不许造一条 `to` 为空的调用**（2026-09-20 修）。
                    //
                    // 这里原来是 `unwrap_or((String::new(), name))` —— 于是模型只要把
                    // 函数名记错一点（真事：它把 `editor_1__edit` 截成了 `__edit`），
                    // 智能体就**照样把请求发出去**，只是收件人是空的：
                    // 中控的信封合规检查把它挡下（只发一条 `invalid` 事件、**不回结果**），
                    // 而智能体在那边**等一个永远不来的结果** —— 最后对模型说的是
                    // "有 1 个调用一直没回来"，那个说法**指不到真正的原因**。
                    //
                    // 现在走"参数解不开"**同一条路**：这条调用不发生，
                    // 由智能体回一条"报错即手册"（把**真有的名字**列给它）。
                    let name_error = if names.contains_key(name) {
                        None
                    } else {
                        Some(unknown_name_manual(names, name))
                    };
                    let (to, tool) = names
                        .get(name)
                        .cloned()
                        .unwrap_or_else(|| (String::new(), name.to_string()));
                    // `arguments` 是**字符串**，得再解一次。
                    //
                    // ⚠️ 解不开的时候**不许编一个对象**给它：在 2026-09-20 之前，
                    // 这里会塞 `{"_raw":…,"_parse_error":true}`，然后那条调用
                    // **照样发给了工具** —— 等于替模型编了它没说的话，
                    // 而工具会回一句"缺少参数"，模型看到的错还指不到真正的原因。
                    // 现在解不开就记在 `parse_error` 上：**这条调用不发生**，
                    // 由智能体回一条"报错即手册"（原话 + 正确格式）给它。
                    let mut call = ToolCall::from_raw_args(
                        Some(id.to_string()),
                        to,
                        tool,
                        args_raw,
                    );
                    // 名字不认识时**优先报这一条**（参数解不解得开都是次要的）
                    if let Some(why) = name_error {
                        call.parse_error = Some(why);
                        // 回放时要**逐字**照它说的（`fn_name("", …)` 重建不出 `__edit`）
                        call.raw_name = Some(name.to_string());
                    }
                    Some(call)
                })
                .collect()
        })
        .unwrap_or_default()
}

impl DeepSeekModel {
    pub fn new(api_key: String, base_url: &str, model: &str, max_tokens: u64) -> Self {
        let base = base_url.trim_end_matches('/');
        Self {
            // 状态码不当异常：我们要自己读错误体，才能把 provider 的话原样带回来
            http: ureq::Agent::config_builder()
                .http_status_as_error(false)
                .build()
                .into(),
            endpoint: format!("{base}/chat/completions"),
            api_key,
            model: model.to_string(),
            max_tokens,
            names: HashMap::new(),
        }
    }

    fn messages(&self, req: &ModelRequest) -> Vec<Value> {
        let mut msgs = vec![json!({"role": "system", "content": req.system})];
        let mut i = 0usize;
        while i < req.items.len() {
            let item = &req.items[i];
            match &item.kind {
                // 系统提示单独走 `system`，这里不再重复
                ItemKind::System => {}
                ItemKind::User => msgs.push(json!({"role": "user", "content": item.text})),
                ItemKind::Assistant => msgs.push(json!({"role": "assistant", "content": item.text})),
                // **连续的调用合并成一条 assistant 消息** —— 它们本来就属于同一次回复
                ItemKind::ToolCall { .. } => {
                    let mut calls = Vec::new();
                    while i < req.items.len() {
                        match &req.items[i].kind {
                            ItemKind::ToolCall { name } => {
                                let d = req.items[i].data.clone().unwrap_or(json!({}));
                                let id = d
                                    .get("call_id")
                                    .and_then(Value::as_str)
                                    .unwrap_or("call_0")
                                    .to_string();
                                // 参数优先用**模型原话**（回放那条"不成立的调用"时只有它）
                                let args = d
                                    .get("args_raw")
                                    .and_then(Value::as_str)
                                    .map(str::to_string)
                                    .unwrap_or_else(|| {
                                        d.get("args")
                                            .map(|a| a.to_string())
                                            .unwrap_or_else(|| "{}".into())
                                    });
                                let inst = d
                                    .get("instance")
                                    .and_then(Value::as_str)
                                    .unwrap_or("");
                                // ⚠️ **名字优先用模型原话**：那条"不成立的调用"里，
                                // `fn_name(inst, name)` 重建不出它真正叫过的名字
                                // （比如它叫的是 `__edit`），回放必须**逐字**照它说的。
                                let shown_name = d
                                    .get("raw_name")
                                    .and_then(Value::as_str)
                                    .map(str::to_string)
                                    .unwrap_or_else(|| fn_name(inst, name));
                                calls.push(json!({
                                    "id": id,
                                    "type": "function",
                                    "function": {"name": shown_name, "arguments": args}
                                }));
                                i += 1;
                            }
                            _ => break,
                        }
                    }
                    msgs.push(json!({"role": "assistant", "content": Value::Null, "tool_calls": calls}));
                    continue; // i 已经在里面推过了
                }
                ItemKind::ToolResult { .. } => {
                    let d = item.data.clone().unwrap_or(json!({}));
                    msgs.push(json!({
                        "role": "tool",
                        "tool_call_id": d.get("call_id").and_then(Value::as_str).unwrap_or("call_0"),
                        "content": item.text,
                    }));
                }
                // 敕令与摘要都是**非用户**的插入 —— 用 system 角色 + 标记，
                // 免得模型把中控的话当成用户说的话（§4.5 那条分级）
                ItemKind::Injected => msgs.push(json!({"role": "system", "content": item.text})),
                ItemKind::Summary => msgs.push(json!({
                    "role": "system",
                    "content": format!("【交接摘要】{}", item.text)
                })),
            }
            i += 1;
        }
        msgs
    }

    fn tools(&mut self, req: &ModelRequest) -> Vec<Value> {
        self.names.clear();
        let mut out = Vec::new();
        for c in &req.tools {
            for t in &c.tools {
                let name = fn_name(&c.instance, &t.name);
                self.names
                    .insert(name.clone(), (c.instance.clone(), t.name.clone()));
                out.push(json!({
                    "type": "function",
                    "function": {
                        "name": name,
                        "description": format!("{}（{}）—— {}", t.title, c.instance, t.description),
                        // **这里就是适配层的边界**：协议里叫 `input_schema`
                        // （规范 §11.1 C 组），OpenAI 家族的线上格式叫 `parameters`。
                        // 名字差别只该存在于这一行，不该扩散回协议（§5.9）。
                        "parameters": t.input_schema,
                    }
                }));
            }
        }
        out
    }
}

impl Model for DeepSeekModel {
    fn name(&self) -> &'static str {
        "deepseek"
    }

    fn reply(&mut self, req: &ModelRequest) -> ModelReply {
        let tools = self.tools(req);
        let mut body = json!({
            "model": self.model,
            "messages": self.messages(req),
            "max_tokens": self.max_tokens,
            "temperature": 0.0,
            "stream": false,
        });
        if !tools.is_empty() {
            body["tools"] = json!(tools);
            body["tool_choice"] = json!("auto");
        }

        let resp = match self
            .http
            .post(&self.endpoint)
            .header("Authorization", &format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .send(body.to_string())
        {
            Ok(r) => r,
            Err(e) => return ModelReply::failed(format!("连不上模型服务：{e}")),
        };
        let status = resp.status().as_u16();
        let text = match resp.into_body().read_to_string() {
            Ok(t) => t,
            Err(e) => return ModelReply::failed(format!("读不回包：{e}")),
        };
        let v: Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(e) => {
                return ModelReply::failed(format!("回包不是 JSON（HTTP {status}）：{e}\n{text}"))
            }
        };
        if status >= 400 {
            let msg = v
                .get("error")
                .and_then(|e| e.get("message"))
                .and_then(Value::as_str)
                .unwrap_or(&text);
            return ModelReply::failed(format!("模型服务回了 HTTP {status}：{msg}"));
        }

        let usage = v
            .get("usage")
            .and_then(|u| u.get("prompt_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let Some(msg) = v
            .get("choices")
            .and_then(|c| c.get(0))
            .and_then(|c| c.get("message"))
        else {
            return ModelReply::failed(format!("回包里没有 choices[0].message：{text}"));
        };

        let calls = parse_tool_calls(&self.names, msg);

        if calls.is_empty() {
            let content = msg
                .get("content")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            ModelReply::text(content, usage)
        } else {
            ModelReply::calls(calls, usage)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prompt::{ToolCard, ToolDef};
    use harness_memory::Item;

    fn model() -> DeepSeekModel {
        DeepSeekModel::new("k".into(), "https://api.deepseek.com", "deepseek-chat", 64)
    }

    fn req(items: Vec<Item>, tools: Vec<ToolCard>) -> ModelRequest {
        ModelRequest {
            system: "你是助手".into(),
            items,
            tools,
        }
    }

    fn card() -> ToolCard {
        ToolCard {
            instance: "term:1".into(),
            title: "终端".into(),
            summary: "跑命令".into(),
            when_to_use: "要执行程序时".into(),
            injection_policy: "card".into(),
            help_sections: Vec::new(),
            destructive: true,
            tools: vec![ToolDef {
                name: "run".into(),
                title: "跑".into(),
                description: "跑单条命令".into(),
                input_schema: json!({"type": "object", "properties": {"cmd": {"type": "string"}}}),
            }],
        }
    }

    /// 实例 id 里的 `:` 不能进函数名 —— 要改名，而且要能查回来。
    #[test]
    fn function_names_are_sanitized_and_resolvable() {
        let mut m = model();
        let tools = m.tools(&req(vec![], vec![card()]));
        let name = tools[0]["function"]["name"].as_str().unwrap();
        assert_eq!(name, "term_1__run");
        assert_eq!(
            m.names.get(name),
            Some(&("term:1".to_string(), "run".to_string()))
        );
    }

    /// **模型叫错名字时，那条调用不许发出去**（2026-09-20，整机实跑抓到的）。
    ///
    /// 实况：模型把 `editor_1__edit` 叫成了 `__edit`。老代码按"查不到就 `to` 留空"
    /// 处理 ⇒ 请求带着空收件人发出去 ⇒ 中控的合规检查挡下、**不回结果** ⇒
    /// 智能体等一个不来的结果 ⇒ 回合以 `timeout` 收场，而且它对模型说的是
    /// "有 1 个调用一直没回来" —— **指不到真正的原因**。
    ///
    /// 这条测试**在老代码上会红**：那时候 `parse_error` 是 `None`。
    #[test]
    fn an_unknown_function_name_never_becomes_a_call_with_an_empty_recipient() {
        let mut m = model();
        let _ = m.tools(&req(vec![], vec![card()])); // 填上反查表
        let msg = json!({
            "tool_calls": [{
                "id": "call_1",
                "function": {"name": "__edit", "arguments": "{\"old\":\"a\",\"new\":\"b\"}"}
            }]
        });
        let calls = parse_tool_calls(&m.names, &msg);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].to, "", "名字查不到时收件人是空的 —— 但**它不该被发出去**");
        let why = calls[0]
            .parse_error
            .as_deref()
            .expect("老代码在这里是 None：于是那条空收件人的请求真的发了出去");
        assert!(why.contains("__edit"), "要把它叫错的那个名字念出来：{why}");
        assert!(why.contains("term_1__run"), "要把它**真有的名字**列出来：{why}");
        assert!(why.contains("没有发生"), "要说清这次调用没发生：{why}");
    }

    #[test]
    fn system_items_are_not_duplicated() {
        let m = model();
        let msgs = m.messages(&req(
            vec![Item::new(ItemKind::System, "别重复我"), Item::user("干活")],
            vec![],
        ));
        assert_eq!(msgs.len(), 2, "system 前缀 + 一条 user");
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[1]["role"], "user");
    }

    /// 同一次回复里的多个调用属于**同一条** assistant 消息。
    #[test]
    fn consecutive_calls_merge_into_one_assistant_message() {
        // （这条测试紧挨着下面那条"回放要带实例前缀"—— 两条一起看才完整）
        let m = model();
        let items = vec![
            Item::user("干活"),
            Item::tool_call_with("term:1", "run", &json!({"cmd": "a"}), "call_1"),
            Item::tool_call_with("term:1", "run", &json!({"cmd": "b"}), "call_2"),
            Item::tool_result_with("run", "ok", "call_1"),
            Item::tool_result_with("run", "ok", "call_2"),
        ];
        let msgs = m.messages(&req(items, vec![]));
        // system + user + assistant(2 calls) + tool + tool
        assert_eq!(msgs.len(), 5);
        assert_eq!(msgs[2]["tool_calls"].as_array().unwrap().len(), 2);
        assert_eq!(msgs[3]["role"], "tool");
        assert_eq!(msgs[3]["tool_call_id"], "call_1");
        assert_eq!(msgs[4]["tool_call_id"], "call_2");
    }

    /// `arguments` 在报文里是**字符串**，发出去时也得是字符串。
    #[test]
    fn arguments_are_sent_as_a_json_string() {
        // ⚠️ 紧邻的那条"回放要带实例前缀"是 2026-09-20 整机实跑抓到的，
        // 两条一起看才完整：**发出去的名字必须和当初给它的名字一模一样。**
        let m = model();
        let msgs = m.messages(&req(
            vec![Item::tool_call_with("term:1", "run", &json!({"cmd": "ls"}), "call_1")],
            vec![],
        ));
        let a = msgs[1]["tool_calls"][0]["function"]["arguments"]
            .as_str()
            .expect("必须是字符串");
        assert!(a.contains("ls"));
    }

    /// **回放给模型的那条调用，函数名必须带实例前缀**（2026-09-20，整机实跑抓到的）。
    ///
    /// 实况：模型叫了一个不存在的名字 `__edit`，而我一开始以为那是它"记错了"。
    /// 真相是**我们自己记的历史里丢了 `instance`** —— 回放出来变成 `__open` / `__read`，
    /// 模型照着自己上一轮的调用学，下一轮就真叫出了 `__edit`。
    ///
    /// 换句话说：**这个 bug 是模型被我们教出来的。**
    #[test]
    fn a_replayed_call_keeps_its_instance_prefix() {
        let m = model();
        let msgs = m.messages(&req(
            vec![Item::tool_call_with(
                "term:1",
                "run",
                &json!({"cmd": "ls"}),
                "call_1",
            )],
            vec![],
        ));
        assert_eq!(
            msgs[1]["tool_calls"][0]["function"]["name"], "term_1__run",
            "回放的名字丢了实例前缀 ⇒ 模型会照着一个不存在的名字学"
        );
    }

    /// 敕令是**中控说的话**，不能混成用户的话（§4.5）。
    #[test]
    fn injected_items_use_the_system_role() {
        let m = model();
        let msgs = m.messages(&req(
            vec![Item::new(ItemKind::Injected, "【敕令】工具没了")],
            vec![],
        ));
        assert_eq!(msgs[1]["role"], "system");
    }

    #[test]
    fn summary_is_marked_as_a_handoff() {
        let m = model();
        let msgs = m.messages(&req(
            vec![Item::new(ItemKind::Summary, "之前做了 A")],
            vec![],
        ));
        assert!(msgs[1]["content"].as_str().unwrap().contains("交接摘要"));
    }
}
