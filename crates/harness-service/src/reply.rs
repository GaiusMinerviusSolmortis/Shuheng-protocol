//! **几个所有工具服务都会遇到的"这是哪种错"** —— 抽出来是为了让它们**说法一致**。
//!
//! 起因是一次审计（2026-09-20）：拿空参数去调服务，回的是
//!
//! ```text
//! code: instance_gone
//! why : 没有实例 ``        ← 两个反引号里是**空的**
//! ```
//!
//! 两处都不对，而且错的方向是同一件事 —— **把"你没填"说成了"它没了"**：
//!
//! 1. **码选错了。** 规范 §11.5 那张表里，`invalid` = "参数不对 ⇒ 看手册改"，
//!    `instance_gone` = "你指的那个实例没了 ⇒ **重新 open**"。
//!    两者要模型做的事不一样，而**模型就是照着码决定下一步的**。
//! 2. **那句话没教人。** 空的名字看起来像渲染坏了 ——
//!    而这恰好违反 §11.5 纪律二：**每个错误都要带"该怎么补救"**（报错即手册）。
//!
//! 抽到公共件的理由和围栏 / 域锁 / Job Object 一样：
//! **这类文案有两份就一定会漂**（编辑器五个工具、终端三个工具都要说同一句话）。

use serde_json::{json, Value};

/// 调用里**没给** `instance_id`（或者给的是空串）。
///
/// 这不是"你指的那个实例没了"，是**参数没填** —— 所以是 `invalid`，
/// 而补救要**具体到"怎么拿到一个 id"**。
pub fn missing_instance_id(tool: &str) -> Value {
    json!({
        "route": null, "effect": "refused", "code": "invalid",
        "why": format!("`{tool}` 没给 `instance_id`（它要一个**已经开着**的实例的 id）"),
        "remedy": format!(
            "先 `open` —— 它会把 `instance_id` 回给你，再把那个 id 填进 `{tool}`。\
             `status` 能看到现在开着哪些（这一步和『实例没了』不一样：\
             那个是你给的 id 找不到，这个是**你还没给**）"
        )
    })
}

/// 给了 id，但**那个实例不在了**（没开过 / 已经 `close` / 属于别的服务）。
pub fn no_such_instance(tool: &str, id: &str) -> Value {
    let shown = if id.trim().is_empty() { "（空）" } else { id };
    json!({
        "route": null, "effect": "refused", "code": "instance_gone",
        "why": format!("没有实例 `{shown}` —— `{tool}` 要的那个不在了"),
        "remedy": "先 open 拿一个新的（它可能已经被 close 掉了）；\
                   `status` 能看到现在开着哪些"
    })
}

/// 参数名对，但**值是空的**（`cmd` 是空的、`path` 是空的……）。
///
/// 单独一条的理由：它和"没给这个参数"在**补救**上不一样 ——
/// 一个是"补上"，一个是"填个像样的值"。两个都说清，模型改起来才不用猜。
pub fn empty_argument(tool: &str, arg: &str, example: &str) -> Value {
    json!({
        "route": null, "effect": "refused", "code": "invalid",
        "why": format!("`{tool}` 的 `{arg}` 是空的"),
        "remedy": format!("填一个具体的值，例如 {example}")
    })
}

/// **未知工具名**：把**真有的那些**列出来。
///
/// `have` 必须由调用方**从自己的工具表派生** —— 手写一份名字清单就是又一张会漂的表，
/// 而这里漂得尤其难看：编辑器那份原来写的是
/// `open | read | edit | revert | close | status | ping`，
/// 而它实际有**九个**工具（少了 `help` 和 `verify`）。
/// **这份"对的名字"就是教模型用的**，它漏一个，模型就少知道一个。
pub fn unknown_tool(name: &str, have: &[&str]) -> Value {
    let shown = if name.trim().is_empty() { "(空)" } else { name };
    json!({
        "route": null, "effect": "refused", "code": "invalid",
        "why": format!("没有名为 `{shown}` 的工具"),
        "expect": {"tool": have.join(" | ")},
        "remedy": format!(
            "这个服务有的是：{}。**别猜名字** —— 不确定就先调 `help`（它有两级：先看章节，再要某一章）",
            have.join(" / ")
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **消息里不许出现空的"名字"**：`` `` `` 看起来像渲染坏了，
    /// 而它每次出现都对应一处"这里本该有内容但我没拿到"。
    #[test]
    fn a_missing_or_blank_name_still_reads_as_a_sentence() {
        for v in [missing_instance_id("read"), no_such_instance("read", "")] {
            let why = v["why"].as_str().unwrap_or("");
            assert!(!why.contains("``"), "判词里出现了空的引号对：{why}");
            assert!(v["remedy"].as_str().unwrap_or("").len() > 10);
        }
    }

    /// **"没给"和"找不到"必须是两个码** —— 这是抽这个模块的全部理由。
    #[test]
    fn missing_is_invalid_and_unknown_is_instance_gone() {
        assert_eq!(missing_instance_id("read")["code"], "invalid");
        assert_eq!(no_such_instance("read", "ed-9")["code"], "instance_gone");
        // 空名字走"找不到"那条时，也要把那一点说出来（不装作它有个名字）
        let blank = no_such_instance("read", "");
        let why = blank["why"].as_str().unwrap_or("");
        assert!(why.contains("（空）"), "空名字要说出来：{why}");
    }
}
