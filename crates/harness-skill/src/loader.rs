//! 扫描技能目录，把每个技能切成一章一章的。
//!
//! 一条纪律贯穿这个文件：**坏的技能目录不该让整个服务起不来，但也不许悄悄跳过。**
//! 加载时把坏掉的那些连同原因收进 `skipped`，`status` 会把它报出来 ——
//! "少了三个技能"和"这三个技能坏了"是两件完全不同的事。

use serde_json::{json, Value};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq)]
pub struct Chapter {
    pub id: String,
    pub title: String,
    /// 全文（按行分页是**发的时候**的事，不是存的时候的事）
    pub text: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Skill {
    pub name: String,
    pub title: String,
    pub version: String,
    pub summary: String,
    pub when_to_use: String,
    pub chapters: Vec<Chapter>,
    /// 它是从哪来的（出问题时要指得回去）
    pub from: PathBuf,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Skipped {
    pub dir: String,
    pub why: String,
}

#[derive(Debug, Clone, Default)]
pub struct Catalog {
    pub skills: Vec<Skill>,
    /// **坏了、被跳过的那些** —— 报出来，不藏着
    pub skipped: Vec<Skipped>,
}

impl Catalog {
    pub fn get(&self, name: &str) -> Option<&Skill> {
        self.skills.iter().find(|s| s.name == name)
    }

    /// `list` 要的那个形状：**名片，不是正文**（§5.3）
    pub fn cards(&self) -> Vec<Value> {
        self.skills
            .iter()
            .map(|s| {
                json!({
                    "name": s.name,
                    "title": s.title,
                    "version": s.version,
                    "summary": s.summary,
                    "when_to_use": s.when_to_use,
                    // **只说有几章、叫什么**，正文一个字都不带
                    "chapters": s.chapters.iter().map(|c| json!({"id": c.id, "title": c.title}))
                        .collect::<Vec<_>>(),
                })
            })
            .collect()
    }
}

/// 扫一个根目录。**根目录不存在不是错误** —— 那是"现在还没有技能"。
pub fn scan(root: &Path) -> Catalog {
    let mut cat = Catalog::default();
    let Ok(entries) = std::fs::read_dir(root) else {
        return cat;
    };
    let mut dirs: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    dirs.sort();
    for d in dirs {
        match load_one(&d) {
            Ok(s) => cat.skills.push(s),
            Err(why) => cat.skipped.push(Skipped {
                dir: d.display().to_string(),
                why,
            }),
        }
    }
    cat.skills.sort_by(|a, b| a.name.cmp(&b.name));
    cat
}

fn load_one(dir: &Path) -> Result<Skill, String> {
    let name_default = dir
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("?")
        .to_string();
    let meta_path = dir.join("skill.json");

    if meta_path.is_file() {
        let raw = std::fs::read_to_string(&meta_path)
            .map_err(|e| format!("`skill.json` 读不了：{e}"))?;
        let m: Value = serde_json::from_str(&raw)
            .map_err(|e| format!("`skill.json` 不是合法 JSON：{e}"))?;
        let s = |k: &str| m.get(k).and_then(Value::as_str).unwrap_or("").to_string();
        let name = {
            let n = s("name");
            if n.is_empty() {
                name_default.clone()
            } else {
                n
            }
        };
        let chapters = match m.get("chapters").and_then(Value::as_array) {
            Some(list) => {
                let mut out = Vec::new();
                for (i, c) in list.iter().enumerate() {
                    let id = c
                        .get("id")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .unwrap_or_else(|| format!("ch-{}", i + 1));
                    let title = c
                        .get("title")
                        .and_then(Value::as_str)
                        .unwrap_or(&id)
                        .to_string();
                    let file = c
                        .get("file")
                        .and_then(Value::as_str)
                        .ok_or_else(|| format!("第 {} 章没写 `file`", i + 1))?;
                    let text = std::fs::read_to_string(dir.join(file))
                        .map_err(|e| format!("第 {} 章的 `{file}` 读不了：{e}", i + 1))?;
                    out.push(Chapter { id, title, text });
                }
                out
            }
            // 没写 `chapters` 就退到"按 `## ` 切 `SKILL.md`" —— 那条路更常见
            None => {
                let md = first_markdown(dir)
                    .ok_or_else(|| "既没有 `chapters`，也找不到 `.md`".to_string())?;
                split_by_headings(&std::fs::read_to_string(&md)
                    .map_err(|e| format!("`{}` 读不了：{e}", md.display()))?)?
            }
        };
        if chapters.is_empty() {
            return Err("一个章节都没有".into());
        }
        let mut skill = Skill {
            name,
            title: {
                let t = s("title");
                if t.is_empty() {
                    name_default.clone()
                } else {
                    t
                }
            },
            version: {
                let v = s("version");
                if v.is_empty() {
                    "0".into()
                } else {
                    v
                }
            },
            summary: s("summary"),
            when_to_use: s("when_to_use"),
            chapters,
            from: dir.to_path_buf(),
        };
        if skill.summary.is_empty() {
            // 没有一句话介绍 = 模型不知道该不该用它。**照实说缺了**，别编一句
            skill.summary = "（这份技能没写 `summary` —— 去问它的提供方，别猜）".into();
        }
        return Ok(skill);
    }

    // 没有 `skill.json`：只认一个 `SKILL.md` 那种形状
    let md = first_markdown(dir)
        .ok_or_else(|| "既没有 `skill.json`，也没有 `.md`".to_string())?;
    let text = std::fs::read_to_string(&md).map_err(|e| format!("`{}` 读不了：{e}", md.display()))?;
    let chapters = split_by_headings(&text)?;
    if chapters.is_empty() {
        return Err("一个章节都没有".into());
    }
    Ok(Skill {
        name: name_default.clone(),
        title: name_default,
        version: "0".into(),
        // **只认第一个 `## ` 之前那段**当介绍。
        //
        // 这条是测试逼出来的：原来是"全文里第一个非标题段落"，于是
        // **名片里漏进了第一章的正文** —— 那是渐进披露的第一层，
        // 它一旦带上正文，"先给名片、要看再拿"这件事就白做了。
        // 前言里没写介绍，就**照实说没写**，别从正文里抓一句充数。
        summary: preamble_paragraph(&text),
        when_to_use: String::new(),
        chapters,
        from: dir.to_path_buf(),
    })
}

/// 目录里第一个 `.md`，`SKILL.md` 优先。
fn first_markdown(dir: &Path) -> Option<PathBuf> {
    let skill = dir.join("SKILL.md");
    if skill.is_file() {
        return Some(skill);
    }
    let mut found: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("md"))
        .collect();
    found.sort();
    found.into_iter().next()
}

/// 按 `## ` 标题切章。**第一个标题之前那段**（如果有实质内容）算作 `overview`。
///
/// 为什么认这一条：现成的技能目录几乎都是"一份 markdown + 一堆二级标题"，
/// 让它们**改格式才能被用**，等于把成本转嫁给写知识的人。
fn split_by_headings(text: &str) -> Result<Vec<Chapter>, String> {
    let mut out: Vec<Chapter> = Vec::new();
    let mut cur_title: Option<String> = None;
    let mut cur: Vec<&str> = Vec::new();
    let mut n = 0usize;
    for line in text.lines() {
        if let Some(h) = line.strip_prefix("## ") {
            push_chapter(&mut out, cur_title.as_deref(), &cur, &mut n);
            cur_title = Some(h.trim().to_string());
            cur.clear();
        } else {
            cur.push(line);
        }
    }
    push_chapter(&mut out, cur_title.as_deref(), &cur, &mut n);
    Ok(out)
}

/// 收一章。**`overview` 是"第一个标题之前那段"**（有内容才算），
/// 其余按出现顺序编号 —— 编号和标题一一对应，不跟着"前面收了几章"漂。
fn push_chapter(out: &mut Vec<Chapter>, title: Option<&str>, body: &[&str], n: &mut usize) {
    let joined = body.join("\n");
    if joined.trim().is_empty() {
        return;
    }
    let (id, title) = match title {
        None => ("overview".to_string(), "概览".to_string()),
        Some(t) => {
            *n += 1;
            (format!("ch-{n}"), t.to_string())
        }
    };
    out.push(Chapter {
        id,
        title,
        text: joined.trim_matches('\n').to_string(),
    });
}

/// **第一个 `## ` 之前那段**里的第一个非空行块 —— 拿来做"一句话介绍"。
///
/// 只看前言：介绍是**写知识的人对这份技能的概括**，不是正文里的任意一句。
fn preamble_paragraph(text: &str) -> String {
    let mut buf: Vec<&str> = Vec::new();
    for line in text.lines() {
        let t = line.trim();
        // 碰到第一个二级标题就停 —— 后面是章节，不是介绍
        if t.starts_with("## ") {
            break;
        }
        if t.starts_with('#') {
            if !buf.is_empty() {
                break;
            }
            continue;
        }
        if t.is_empty() {
            if !buf.is_empty() {
                break;
            }
            continue;
        }
        buf.push(t);
    }
    let s = buf.join(" ");
    if s.is_empty() {
        "（这份技能没写介绍 —— 去问它的提供方，别猜）".into()
    } else {
        s
    }
}

/// **按行分页**：一行一行给，不按字节 —— 字节会劈开一个汉字。
///
/// 返回 `(这一页, 下一行的行号或 None, 总行数)`。行号从 1 开始（人话）。
pub fn page(text: &str, from_line: u64, lines: u64) -> (String, Option<u64>, u64) {
    let all: Vec<&str> = text.lines().collect();
    let total = all.len() as u64;
    let from = from_line.max(1);
    if from > total {
        return (String::new(), None, total);
    }
    let take = lines.max(1);
    let start = (from - 1) as usize;
    let end = ((start as u64 + take).min(total)) as usize;
    let page = all[start..end].join("\n");
    let next = if (end as u64) < total {
        Some(end as u64 + 1)
    } else {
        None
    };
    (page, next, total)
}
