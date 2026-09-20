//! 技能加载的验收。这里每一条都在问一件事：**它会不会自己编。**

use harness_skill::loader::{page, scan};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static N: AtomicU64 = AtomicU64::new(0);

fn tmp() -> PathBuf {
    let n = N.fetch_add(1, Ordering::SeqCst);
    let p = std::env::temp_dir().join(format!(
        "skill-loader-test-{}-{}-{n}",
        std::process::id(),
        harness_core::now_ms()
    ));
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn write(path: &PathBuf, text: &str) {
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d).unwrap();
    }
    std::fs::write(path, text).unwrap();
}

/// **只有一个 `SKILL.md` 也要能用** —— 现成的技能目录大多长这样，
/// 让它们"改格式才能被用"等于把成本转嫁给写知识的人。
#[test]
fn a_bare_skill_md_is_split_into_chapters_by_headings() {
    let root = tmp();
    write(
        &root.join("adb").join("SKILL.md"),
        "# 录安卓调试视频\n\n一句话介绍在前头。\n\n## 快速开始\n\n用 adb shell screenrecord。\n\n## 坑\n\n别在 4K 下录。\n",
    );
    let cat = scan(&root);
    assert_eq!(cat.skipped.len(), 0);
    let s = &cat.skills[0];
    assert_eq!(s.name, "adb");
    assert_eq!(s.chapters.len(), 3, "概览 + 两章");
    assert_eq!(s.chapters[0].id, "overview");
    assert_eq!(s.chapters[1].id, "ch-1");
    assert_eq!(s.chapters[1].title, "快速开始");
    assert_eq!(s.chapters[2].title, "坑");
    // 一句话介绍从正文里拿 —— 不是编的
    assert!(s.summary.contains("一句话介绍在前头"), "{}", s.summary);
    // 章里**不含**标题行本身（它是章名，不是正文）
    assert!(!s.chapters[1].text.contains("## 快速开始"));
}

#[test]
fn a_skill_json_can_be_explicit_about_order_and_version() {
    let root = tmp();
    let d = root.join("pdf");
    write(
        &d.join("skill.json"),
        r#"{"name":"pdf","title":"做 PDF","version":"2","summary":"怎么生成 PDF",
            "when_to_use":"要出报告的时候",
            "chapters":[{"id":"before","title":"先准备","file":"01.md"},
                        {"id":"make","title":"再生成","file":"02.md"}]}"#,
    );
    write(&d.join("01.md"), "先装依赖。");
    write(&d.join("02.md"), "然后跑脚本。");
    let cat = scan(&root);
    let s = &cat.skills[0];
    assert_eq!(s.version, "2");
    assert_eq!(s.chapters[0].id, "before");
    assert_eq!(s.chapters[1].text, "然后跑脚本。");
    assert_eq!(s.when_to_use, "要出报告的时候");
}

/// **坏掉的技能不该让服务起不来，但也不许悄悄跳过。**
#[test]
fn a_broken_skill_is_reported_not_swallowed() {
    let root = tmp();
    write(&root.join("good").join("SKILL.md"), "## 有内容\n\n干货。\n");
    std::fs::create_dir_all(root.join("empty")).unwrap();
    write(&root.join("badjson").join("skill.json"), "{ 这不是 JSON");
    let cat = scan(&root);
    assert_eq!(cat.skills.len(), 1, "好的那份要能加载");
    assert_eq!(cat.skipped.len(), 2, "两份坏的都要报出来");
    let whys: Vec<String> = cat.skipped.iter().map(|s| s.why.clone()).collect();
    assert!(whys.iter().any(|w| w.contains("JSON")), "{whys:?}");
    assert!(whys.iter().any(|w| w.contains("没有")), "{whys:?}");
}

/// 章没写 `summary` 时**照实说缺了**，不许编一句像模像样的。
#[test]
fn a_missing_summary_says_so_instead_of_inventing_one() {
    let root = tmp();
    let d = root.join("x");
    write(
        &d.join("skill.json"),
        r#"{"name":"x","chapters":[{"id":"a","title":"A","file":"a.md"}]}"#,
    );
    write(&d.join("a.md"), "内容。");
    let cat = scan(&root);
    assert!(
        cat.skills[0].summary.contains("没写"),
        "{}",
        cat.skills[0].summary
    );
}

#[test]
fn an_empty_root_is_not_an_error() {
    let cat = scan(&tmp());
    assert!(cat.skills.is_empty() && cat.skipped.is_empty());
    // 连目录都不存在也一样
    let cat2 = scan(&PathBuf::from("这个路径不存在-please"));
    assert!(cat2.skills.is_empty());
}

/// **分页按行，不按字节** —— 字节会劈开一个汉字。
#[test]
fn paging_counts_lines_and_never_splits_one() {
    let text = (1..=10)
        .map(|i| format!("第 {i} 行"))
        .collect::<Vec<_>>()
        .join("\n");
    let (one, next, total) = page(&text, 1, 4);
    assert_eq!(total, 10);
    assert_eq!(one.lines().count(), 4);
    assert_eq!(one.lines().last().unwrap(), "第 4 行");
    assert_eq!(next, Some(5));
    let (two, next2, _) = page(&text, next.unwrap(), 4);
    assert_eq!(two.lines().next().unwrap(), "第 5 行");
    assert_eq!(next2, Some(9));
    // 翻过头不是错误，是"没有了"
    let (three, next3, _) = page(&text, 99, 4);
    assert!(three.is_empty() && next3.is_none());
    // 越界的 from 也一样
    assert_eq!(page(&text, 0, 2).1, Some(3), "0 当成第一页");
}

/// 名片里**只有名字和章节目录，没有正文** —— 渐进披露的第一层就靠这个。
#[test]
fn the_cards_carry_no_body_text() {
    let root = tmp();
    write(
        &root.join("s").join("SKILL.md"),
        "# S\n\n## 第一章\n\n这段正文绝对不该出现在名片里。\n",
    );
    let cat = scan(&root);
    let cards = cat.cards();
    let dump = serde_json::to_string(&cards).unwrap();
    assert!(dump.contains("第一章"));
    assert!(
        !dump.contains("这段正文绝对不该出现在名片里"),
        "名片里带了正文：{dump}"
    );
}
