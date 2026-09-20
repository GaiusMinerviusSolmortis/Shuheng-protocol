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
                                let args = d
                                    .get("args")
                                    .map(|a| a.to_string())
                                    .unwrap_or_else(|| "{}".into());
                                let inst = d
                                    .get("instance")
                                    .and_then(Value::as_str)
                                    .unwrap_or("");
                                calls.push(json!({
                                    "id": id,
                                    "type": "function",
                                    "function": {"name": fn_name(inst, name), "arguments": args}
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

        let calls: Vec<ToolCall> = msg
            .get("tool_calls")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|c| {
                        let id = c.get("id").and_then(Value::as_str).unwrap_or("call_0");
                        let f = c.get("function")?;
                        let name = f.get("name")?.as_str()?;
                        let args_raw = f.get("arguments").and_then(Value::as_str).unwrap_or("{}");
                        let (to, tool) = self.names.get(name).cloned().unwrap_or_else(|| {
                            (String::new(), name.to_string())
                        });
                        // `arguments` 是**字符串**，得再解一次。
                        //
                        // ⚠️ 解不开的时候**不许编一个对象**给它：在 2026-09-20 之前，
                        // 这里会塞 `{"_raw":…,"_parse_error":true}`，然后那条调用
                        // **照样发给了工具** —— 等于替模型编了它没说的话，
                        // 而工具会回一句"缺少参数"，模型看到的错还指不到真正的原因。
                        // 现在解不开就记在 `parse_error` 上：**这条调用不发生**，
                        // 由智能体回一条"报错即手册"（原话 + 正确格式）给它。
                        Some(ToolCall::from_raw_args(
                            Some(id.to_string()),
                            to,
                            tool,
                            args_raw,
                        ))
                    })
                    .collect()
            })
            .unwrap_or_default();

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
        let m = model();
        let items = vec![
            Item::user("干活"),
            Item::tool_call_with("run", &json!({"cmd": "a"}), "call_1"),
            Item::tool_call_with("run", &json!({"cmd": "b"}), "call_2"),
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
        let m = model();
        let msgs = m.messages(&req(
            vec![Item::tool_call_with("run", &json!({"cmd": "ls"}), "call_1")],
            vec![],
        ));
        let a = msgs[1]["tool_calls"][0]["function"]["arguments"]
            .as_str()
            .expect("必须是字符串");
        assert!(a.contains("ls"));
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
