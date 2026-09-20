# -*- coding: utf-8 -*-
"""一次性：把决策册从"按时间追加"重排成"按主题分组"。

!! 一次性脚本 !!
它假定 10-决策.md 还是"17 段轮次补充"的追加式结构。

分组依据：每条决策引用的第一个 §x.y → 该章节所在的文件 → 主题。
所以分组是**自动的**，不靠人工归类，也就不会漏。
"""
from __future__ import annotations

import re
import sys
from pathlib import Path

ROOT = Path(r"D:\agent\instance-harness")
DECISIONS = ROOT / "10-决策.md"

FILE_THEME = {
    "01-内核.md": "内核与协议",
    "02-工具.md": "工具与工位",
    "03-上下文.md": "缓存与压缩",
    "04-安全.md": "安全",
    "05-窗口.md": "窗口与观察",
    "06-操控.md": "窗口与观察",
    "07-界面.md": "界面与表示",
    "08-记忆.md": "记忆",
    "09-模型.md": "模型",
    "规范-工具服务.md": "工具与工位",
    "附录-参考.md": "定位与参考",
}
THEME_ORDER = ["内核与协议", "工具与工位", "缓存与压缩", "安全",
               "窗口与观察", "界面与表示", "记忆", "模型", "定位与参考", "未归类"]

# 开工前必答（人工挑的 15 条）
BLOCKING = [1, 7, 8, 9, 10, 11, 14, 15, 16, 32, 59, 79, 83, 86, 87]
# 已经有结论、但从待定列表里没移走的
RESOLVED = [40, 90]


def section_to_file() -> dict[str, str]:
    m: dict[str, str] = {}
    for f in sorted(ROOT.glob("*.md")):
        for line in f.read_text(encoding="utf-8").split("\n"):
            mm = re.match(r"^#{2,4}\s+(\d+(?:\.\d+)*)", line)
            if mm:
                m[mm.group(1)] = f.name
    return m


def theme_of(text: str, sec2file: dict[str, str]) -> str:
    for mm in re.finditer(r"§(\d+(?:\.\d+)*)", text):
        fname = sec2file.get(mm.group(1))
        if fname and fname in FILE_THEME:
            return FILE_THEME[fname]
    return "未归类"


def collect(lines: list[str]):
    """返回 (已定条目, 待定条目)，每条是完整的多行文本。"""
    done, todo = [], []
    i = 0
    while i < len(lines):
        ln = lines[i]
        m_done = re.match(r"^- ~~", ln)
        m_todo = re.match(r"^(\d+)\. \*\*", ln)
        if m_done or m_todo:
            block = [ln]
            j = i + 1
            while j < len(lines):
                nxt = lines[j]
                if re.match(r"^- ~~", nxt) or re.match(r"^\d+\. \*\*", nxt):
                    break
                if re.match(r"^#{2,4} ", nxt) or nxt.strip() == "---":
                    break
                if nxt.strip() == "":
                    j += 1
                    continue
                block.append(nxt)
                j += 1
            text = "\n".join(block).rstrip()
            if m_done:
                done.append(text)
            else:
                todo.append((int(m_todo.group(1)), text))
            i = j
        else:
            i += 1
    return done, todo


def main() -> int:
    try:
        sys.stdout.reconfigure(encoding="utf-8", errors="replace")
    except Exception:
        pass

    sec2file = section_to_file()
    lines = DECISIONS.read_text(encoding="utf-8").split("\n")
    done, todo = collect(lines)
    print(f"解析出：已定 {len(done)} 条，待定 {len(todo)} 条")

    done_by = {t: [] for t in THEME_ORDER}
    todo_by = {t: [] for t in THEME_ORDER}
    for d in done:
        done_by[theme_of(d, sec2file)].append(d)
    resolved_items, blocking_items = [], []
    for num, t in todo:
        if num in RESOLVED:
            resolved_items.append(t)
        elif num in BLOCKING:
            blocking_items.append(t)
        else:
            todo_by[theme_of(t, sec2file)].append(t)

    out: list[str] = []
    out += [
        "# 决策登记册",
        "",
        "> 已定与待定。**每有结论就更新这里。**",
        ">",
        "> 模块文件：`10-决策.md`",
        "> 定位：本文件是「实例化 Harness」设计的一个模块，不是完整文档。",
        "> 总索引见 [README.md](README.md)。",
        "",
        "---",
        "",
        "## 0. 来源标记",
        "",
        "**约定**：每条结论标一个来源，否则分不清哪些是你定的、哪些是我补的。",
        "",
        "| 标记 | 含义 |",
        "|---|---|",
        "| `[你说]` | 你明确提出过的 |",
        "| `[你问]` | 你提问、我回答 —— **答案是我的，你只确认了问题** |",
        "| `[我提]` | 我主动提出、你没有表态过的 |",
        "| `[实测]` | 有实验数据支撑 |",
        "| `[外部]` | 抄自某个已被验证的项目 |",
        "",
        "> **⚠️ 目前只有最早的 16 条标了来源（见 §1）。后面十几轮新增的还没标。**",
        "",
        "---",
        "",
        "## 1. 建议你优先回看的（已标来源的那批）",
        "",
    ]

    # 保留原来的"待你回看"表
    grab = False
    for ln in lines:
        if ln.startswith("### 建议你优先回看的几条"):
            grab = True
            continue
        if grab:
            if ln.startswith("## 18."):
                break
            if ln.strip() or out[-1].strip() == "":
                out.append(ln)
    out += [
        "",
        "> **这张表只覆盖了前三分之一。** 后面每轮新增的结论都还没有来源标记 ——",
        "> 这是本文件下一件该做的事。",
        "",
        "---",
        "",
        f"## 2. 开工前必须回答（{len(blocking_items)} 条）",
        "",
        "> 这几条不定，第一行代码就写不出来。**其余都可以边走边定。**",
        "",
    ]
    out += blocking_items
    out += ["", "---", "", "## 3. 已定（按主题）", ""]
    for theme in THEME_ORDER:
        items = done_by[theme]
        if not items:
            continue
        out += [f"### {theme}（{len(items)} 条）", ""] + items + [""]

    out += ["---", "", "## 4. 待定（按主题）", ""]
    for theme in THEME_ORDER:
        items = todo_by[theme]
        if not items:
            continue
        out += [f"### {theme}（{len(items)} 条）", ""] + items + [""]

    out += [
        "---",
        "",
        f"## 5. 已解决，手续未清（{len(resolved_items)} 条）",
        "",
        "> 这几条已经有结论了，但当时只加了个箭头，没从待定里移走。留在这里备查。",
        "",
    ] + resolved_items + [""]

    DECISIONS.write_text("\n".join(out).rstrip() + "\n", encoding="utf-8")
    print(f"写入 {DECISIONS}")
    print(f"  开工前必答 {len(blocking_items)} 条")
    print(f"  已定 {len(done)} 条（分 {sum(1 for t in THEME_ORDER if done_by[t])} 个主题）")
    print(f"  待定 {sum(len(v) for v in todo_by.values())} 条（分 {sum(1 for t in THEME_ORDER if todo_by[t])} 个主题）")
    print(f"  已解决 {len(resolved_items)} 条")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
