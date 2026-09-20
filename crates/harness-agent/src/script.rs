//! mock 模型的脚本：**NDJSON，一行一步**。
//!
//! 为什么是 NDJSON 而不是 JSON 数组：它是**手写**的，而且和我们的协议同形 ——
//! 一行坏掉不会带走整个文件（同 `harness-core` 那条"坏行不掐死通道"的纪律）。

use crate::model::Step;

#[derive(Debug, Clone, PartialEq)]
pub struct ScriptError {
    pub line: usize,
    pub why: String,
    pub remedy: String,
}

impl std::fmt::Display for ScriptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "脚本第 {} 行不对：{}。{}",
            self.line, self.why, self.remedy
        )
    }
}

/// 解析脚本。空行跳过；坏行**直接报**（这是测试工具，不许静默）。
pub fn parse_script(text: &str) -> Result<Vec<Step>, ScriptError> {
    let mut steps = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let t = line.trim();
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        match serde_json::from_str::<Step>(t) {
            Ok(step) => steps.push(step),
            Err(e) => {
                return Err(ScriptError {
                    line: i + 1,
                    why: e.to_string(),
                    remedy: "一行一步，形如 \
                             {\"step\":\"tool\",\"calls\":[{\"to\":\"term:1\",\"tool\":\"run\",\"args\":{}}]} \
                             或 {\"step\":\"text\",\"text\":\"做完了\"}"
                        .to_string(),
                })
            }
        }
    }
    Ok(steps)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_two_important_step_kinds() {
        let s = parse_script(
            r#"
# 注释行可以写
{"step":"tool","calls":[{"to":"term:1","tool":"run","args":{"cmd":"ls"}}]}
{"step":"text","text":"做完了"}
"#,
        )
        .unwrap();
        assert_eq!(s.len(), 2);
        assert!(matches!(s[0], Step::Tool { .. }));
        assert!(matches!(s[1], Step::Text { .. }));
    }

    #[test]
    fn a_bad_line_is_reported_with_its_number_and_a_remedy() {
        let e = parse_script("{\"step\":\"text\"}\n").unwrap_err();
        assert_eq!(e.line, 1);
        assert!(e.remedy.contains("一行一步"), "报错要带格式");
    }

    #[test]
    fn empty_script_is_allowed() {
        assert!(parse_script("\n\n").unwrap().is_empty());
    }
}
