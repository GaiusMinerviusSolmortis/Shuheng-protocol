//! **提示词编译**（09-模型 §8）—— 以及那条硬约束：**拼装在绑定时刻凝固**（§15.2）。
//!
//! > 拼装的结果必须凝固成一个具名、有版本、可寻址的实例。
//! > 运行期不存在"半成品模型"。
//!
//! 所以这个模块的产物有两种用途，而且只用一次：
//!
//! 1. **进前缀的那段文本** —— 编译一次，之后字节稳定（否则每轮都白碎缓存）；
//! 2. **组合清单**（§15.3）—— 哪些部件、哪个版本。想可复现就得存它。
//!
//! 那么"工具集变了怎么办"？**不改提示词**，而是**追加一条敕令**。
//! 这正是 §4.4 那条设计落地的样子：前缀不动，变化走 append-only。

use serde::{Deserialize, Serialize};

/// 一个工具实例上的**某一个工具**。真模型要靠这份表才知道怎么填参数。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDef {
    pub name: String,
    #[serde(default)]
    pub title: String,
    pub description: String,
    /// 参数表 —— JSON Schema。**名字跟规范走**（§11.1 C 组的 `input_schema`），
    /// 而 provider 那边的叫法（OpenAI/DeepSeek 的 `parameters`）由
    /// **适配层**去映射（`deepseek.rs`）—— 那是适配层该干的活，不是协议该迁就的。
    #[serde(default = "empty_schema")]
    pub input_schema: serde_json::Value,
}

/// 一个工具实例对模型露出的那张**名片**（L0 渐进披露，§5.3）。
///
/// 注意只有名片，**没有完整参数表** —— 参数怎么填，让模型自己去问
/// （"不会就 `--help`"那条，§5.3）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCard {
    /// 实例 id —— v1 里模型就是按它寻址的
    pub instance: String,
    pub title: String,
    pub summary: String,
    pub when_to_use: String,
    /// 只读 / 有副作用（抄 MCP 的 annotations，§11.1）
    pub destructive: bool,
    /// 它有哪些工具、各自怎么填。
    ///
    /// 这张表只有一个来源：**服务自己报的那份 manifest**（§11.1 C 组）。
    /// 曾经它是智能体自带的一份手写文件（同一件事写两遍，迟早不一致），
    /// 现在 `card_from_manifest` 从服务那边读。
    #[serde(default)]
    pub tools: Vec<ToolDef>,
}

fn empty_schema() -> serde_json::Value {
    serde_json::json!({"type": "object", "properties": {}})
}

/// **从服务自报的 manifest 拼出一张名片。**
///
/// 这是"两处真相合一处"的关键一步：以前名片是智能体自带的一份**手写配置**，
/// 而服务那边也有一份 manifest —— 同一件事写两遍，迟早不一致。
/// 现在参数表只有一个来源：**服务自己报的那份**（规范 §11.1 的 C 组）。
pub fn card_from_manifest(instance: &str, m: &serde_json::Value) -> Option<ToolCard> {
    let raw = m.get("tools")?.as_array()?;
    let mut tools = Vec::new();
    let mut destructive = false;
    for t in raw {
        let name = t.get("name")?.as_str()?.to_string();
        // `annotations` 是**对象**（MCP 的 `ToolAnnotations` 形状，§11.1）——
        // 不是四个摊在工具条目顶层的字段。这条现在是**契约**而不是习惯：
        // `harness_service::contract::check_manifest` 会拦下摊平写法。
        if t.get("annotations")
            .and_then(|a| a.get("destructiveHint"))
            .and_then(serde_json::Value::as_bool)
            == Some(true)
        {
            destructive = true;
        }
        tools.push(ToolDef {
            title: t
                .get("title")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("")
                .to_string(),
            name,
            // **没写说明就说没写** —— 别假装它有
            description: t
                .get("description")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("（这个工具没写说明 —— 去问它的提供方）")
                .to_string(),
            input_schema: t.get("input_schema").cloned().unwrap_or_else(empty_schema),
        });
    }
    let s = |k: &str| {
        m.get(k)
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    Some(ToolCard {
        instance: instance.to_string(),
        title: s("title"),
        summary: s("summary"),
        when_to_use: s("when_to_use"),
        destructive,
        tools,
    })
}

#[derive(Debug, Clone, Serialize)]
pub struct Persona {
    pub name: String,
    pub version: String,
    /// 一句话人设
    pub brief: String,
}

/// **组合清单**：这次生成用的是什么拼出来的（§15.3）。
#[derive(Debug, Clone, Serialize)]
pub struct CompositionManifest {
    pub agent: String,
    pub persona: String,
    pub persona_version: String,
    pub tool_instances: Vec<String>,
    pub fence: Vec<String>,
    pub compiler: String,
    pub compiled_at: i64,
}

const COMPILER: &str = "harness-agent/prompt v1";

/// 编译一次，得到"进前缀的那段" + 组合清单。
pub fn compile(
    agent: &str,
    persona: &Persona,
    tools: &[ToolCard],
    fence: &[String],
    sid: &str,
) -> (String, CompositionManifest) {
    let mut p = String::new();
    p.push_str(&format!("你是「{}」（v{}）。{}\n\n", persona.name, persona.version, persona.brief));
    p.push_str(&format!("你在会话 {sid} 里干活。\n\n"));

    p.push_str("## 你能用的工具\n\n");
    p.push_str("下面是**名片**，不是完整参数表 —— 不确定怎么调，就先问我（status / 它的 help），别猜参数。\n\n");
    if tools.is_empty() {
        p.push_str("（现在没有工具。要做的事如果非动手不可，就直说做不了。）\n");
    }
    for t in tools {
        p.push_str(&format!(
            "- **{}**（`{}`，{}）—— {}\n  什么时候用：{}\n",
            t.title,
            t.instance,
            if t.destructive { "有副作用" } else { "只读或可回滚" },
            t.summary,
            t.when_to_use
        ));
        // **把它的工具名列出来** —— 名字是模型寻址用的，光有一句介绍没用。
        // 具体参数不写在这儿（那是完整参数表，进的是 function 定义那一路）。
        if !t.tools.is_empty() {
            let names: Vec<&str> = t.tools.iter().map(|x| x.name.as_str()).collect();
            p.push_str(&format!("  它的工具：{}\n", names.join(" / ")));
        }
    }
    p.push('\n');

    p.push_str("## 托管范围\n\n");
    if fence.is_empty() {
        p.push_str("这次没有托管任何路径 —— 任何动文件的事都要先跟用户确认。\n\n");
    } else {
        p.push_str("只能在这些路径里读写，**越界会被挡下来**（那不是故障，是围栏）：\n");
        for f in fence {
            p.push_str(&format!("- `{f}`\n"));
        }
        p.push('\n');
    }

    p.push_str("## 规矩\n\n");
    p.push_str("1. **一次调用说清楚三件事**：发给谁（`to`）、用哪个工具、参数是什么。\n");
    p.push_str("2. **报错里通常已经写了正确格式和下一步** —— 照它改，别原样重试。\n");
    p.push_str("3. **工具集是会变的**：有工具上线/下线，我会用一条【敕令】追加告诉你。\n");
    p.push_str("   名片不是永远不变的东西 —— 收到敕令之后按新的来。\n");
    p.push_str("4. **做完就停**：给出结论，不要为了\"再多做一点\"而反复调工具。\n");

    let manifest = CompositionManifest {
        agent: agent.to_string(),
        persona: persona.name.clone(),
        persona_version: persona.version.clone(),
        tool_instances: tools.iter().map(|t| t.instance.clone()).collect(),
        fence: fence.to_vec(),
        compiler: COMPILER.to_string(),
        compiled_at: 0,
    };
    (p, manifest)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn card(id: &str) -> ToolCard {
        ToolCard {
            instance: id.into(),
            title: "终端".into(),
            summary: "跑单条命令".into(),
            when_to_use: "要执行程序时".into(),
            destructive: true,
            tools: vec![ToolDef {
                name: "run".into(),
                title: "跑".into(),
                description: "跑单条命令".into(),
                input_schema: empty_schema(),
            }],
        }
    }

    fn persona() -> Persona {
        Persona {
            name: "达达".into(),
            version: "1".into(),
            brief: "一个干活麻利的编码助手。".into(),
        }
    }

    #[test]
    fn cards_carry_only_the_name_card_not_the_parameter_table() {
        let (p, _) = compile("agent:dada", &persona(), &[card("term:1")], &[], "s-1");
        assert!(p.contains("term:1"));
        assert!(p.contains("什么时候用"));
        assert!(p.contains("不是完整参数表"), "要说清名片不是手册");
    }

    #[test]
    fn manifest_records_what_it_was_compiled_from() {
        let (_, m) = compile(
            "agent:dada",
            &persona(),
            &[card("term:1"), card("editor:1")],
            &["D:/p".into()],
            "s-1",
        );
        assert_eq!(m.persona, "达达");
        assert_eq!(m.persona_version, "1");
        assert_eq!(m.tool_instances, vec!["term:1", "editor:1"]);
        assert_eq!(m.compiler, COMPILER);
    }

    /// 前缀必须**字节稳定** —— 同样的输入编译两次要一模一样，
    /// 否则每轮都白碎一次缓存（§5.4、§15.2）。
    #[test]
    fn compilation_is_deterministic() {
        let a = compile("agent:dada", &persona(), &[card("term:1")], &["D:/p".into()], "s-1").0;
        let b = compile("agent:dada", &persona(), &[card("term:1")], &["D:/p".into()], "s-1").0;
        assert_eq!(a, b);
    }

    #[test]
    fn no_tools_is_said_plainly() {
        let (p, m) = compile("agent:dada", &persona(), &[], &[], "s-1");
        assert!(p.contains("现在没有工具"));
        assert!(m.tool_instances.is_empty());
    }

    #[test]
    fn fence_is_stated_as_a_fence_not_a_bug() {
        let (p, _) = compile("agent:dada", &persona(), &[], &["D:/proj".into()], "s-1");
        assert!(p.contains("越界会被挡下来"));
        assert!(p.contains("不是故障"));
    }
}
