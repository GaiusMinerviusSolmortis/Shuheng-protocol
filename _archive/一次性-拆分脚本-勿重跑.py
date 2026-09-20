# -*- coding: utf-8 -*-
"""把单文件设计稿按模块拆分。

!! 一次性脚本，不要重跑 !!
拆分已经完成，模块文件现在是唯一真相。重跑会用归档的单文件版
覆盖掉所有在模块里的后续修改（§5.10、§14.19 等）。
仅当需要研究拆分逻辑时才阅读本文件。

原则：**保留原章节号**（§5.4、§13.7、§14.18…），
因为全文有上百处 §x.y 交叉引用，重编号会全部断掉。
所以模块内的编号不连续是正常的、有意的。
"""
from __future__ import annotations

import re
import sys
from pathlib import Path

SRC = Path(r"D:\agent\instance-harness-design.md")
OUT = Path(r"D:\agent\instance-harness")

# ## 级章节 -> 模块
TOP_ROUTE = {
    "0": "README", "1": "README",
    "2": "01-内核", "3": "01-内核", "4": "01-内核", "6": "01-内核",
    "7": "04-安全",
    "8": "08-模型",
    "9": "06-界面",
    "10": "附录-参考",
    "11": "规范-工具服务",
    "12": "06-界面",
    "13": "05-窗口",
    "14": "05-窗口",
    "16": "07-记忆",
    "17": "附录-参考",
    "18": "09-决策",
}
# ## 5 工具：拆到三个模块（5.4 -> 上下文，5.7/5.8 -> 安全，其余 -> 工具）
SEC5 = {
    "5.1": "02-工具", "5.2": "02-工具", "5.3": "02-工具",
    "5.4": "03-上下文",
    "5.5": "02-工具", "5.6": "02-工具",
    "5.7": "04-安全", "5.8": "04-安全",
    "5.9": "02-工具",
}
SEC5_DEFAULT = "02-工具"
# ## 15 模型拼装与解耦：15.4 解耦清单进 README（它是项目级进度表）
SEC15 = {"15.1": "08-模型", "15.2": "08-模型", "15.3": "08-模型", "15.4": "README"}
SEC15_DEFAULT = "08-模型"

MODULE_META = {
    "01-内核": ("内核", "实例模型、消息流、中控。**这一层定死，其余都可以边做边改。**"),
    "02-工具": ("工具", "工具的层次、渐进披露协议、动态加载、跨提供方格式适配。"),
    "03-上下文": ("上下文经济", "缓存约束与上下文治理。视野 = 成本，也 = 上传量。"),
    "04-安全": ("安全", "能力制、shell 的边界、动作归因。"),
    "05-窗口": ("窗口", "窗口托管、电脑操控、观测契约，以及实测数据。**当前重点。**"),
    "06-界面": ("界面", "GUI 作为消息流的投影，以及表示的三个消费者。"),
    "07-记忆": ("记忆", "锚点、分层、沉淀。持久化挂在内容身份上，不挂在路径或窗口上。"),
    "08-模型": ("模型", "提示词编译与模型拼装。"),
    "09-决策": ("决策登记册", "已定与待定。**每有结论就更新这里。**"),
    "规范-工具服务": ("规范 · 工具服务", "**规范性文件**，与其余探索性文档性质不同。"),
    "附录-参考": ("附录 · 参考", "与已有工作的对照，以及和 MCP 的定位比较。"),
    "README": ("索引", ""),
}

ORDER = ["01-内核", "02-工具", "03-上下文", "04-安全", "05-窗口",
         "06-界面", "07-记忆", "08-模型", "09-决策",
         "规范-工具服务", "附录-参考"]


def parse_blocks(lines: list[str]):
    """返回 [(top_title, sub_title_or_None, text_lines)]，保持源顺序。"""
    heads = []
    for i, ln in enumerate(lines):
        m = re.match(r"^(#{1,3})\s+(.*?)\s*$", ln)
        if m:
            heads.append((i, len(m.group(1)), m.group(2)))
    blocks = []
    top_idx = [h for h in heads if h[1] == 2]
    for n, (start, _, title) in enumerate(top_idx):
        end = top_idx[n + 1][0] if n + 1 < len(top_idx) else len(lines)
        subs = [(i, t) for (i, lv, t) in heads if lv == 3 and start < i < end]
        if not subs:
            blocks.append((title, None, lines[start:end]))
            continue
        # 章节标题到第一个子标题之间的引言
        intro = lines[start:subs[0][0]]
        if any(x.strip() for x in intro):
            blocks.append((title, None, intro))
        for k, (sstart, stitle) in enumerate(subs):
            send = subs[k + 1][0] if k + 1 < len(subs) else end
            blocks.append((title, stitle, lines[sstart:send]))
    return blocks


def sec_num(title: str) -> str:
    # 兼容 "5. 工具"、"5.4 缓存约束"、"0.1 补充表述" 三种写法
    m = re.match(r"^(\d+(?:\.\d+)*)", title)
    if m:
        return m.group(1)
    if title.startswith("附"):
        return "附"
    return ""


def route(top: str, sub: str | None) -> str:
    if top.startswith("附") and not re.match(r"^(\d+)\.", top):
        return "README"
    tnum = sec_num(top)
    snum = sec_num(sub) if sub else ""
    if tnum == "5":
        return SEC5.get(snum, SEC5_DEFAULT) if snum else SEC5_DEFAULT
    if tnum == "15":
        return SEC15.get(snum, SEC15_DEFAULT) if snum else SEC15_DEFAULT
    return TOP_ROUTE.get(tnum, "??")


def main() -> int:
    try:
        sys.stdout.reconfigure(encoding="utf-8", errors="replace")
    except Exception:
        pass
    # 二次运行防护：目标目录已存在 README.md 就说明拆分已完成
    if (OUT / "README.md").exists():
        print(f"拒绝执行：{OUT} 已存在（拆分已完成，模块文件是唯一真相）。")
        print("重跑会覆盖模块里的后续修改。确实要重来请先删除该目录。")
        return 2
    lines = SRC.read_text(encoding="utf-8").split("\n")
    blocks = parse_blocks(lines)

    buckets: dict[str, list[str]] = {k: [] for k in ORDER}
    readme_parts: dict[str, list[str]] = {"定位": [], "表述": [], "解耦": [], "用法": []}
    unrouted = []

    for top, sub, text in blocks:
        dest = route(top, sub)
        if dest == "??":
            unrouted.append((top, sub))
            continue
        body = "\n".join(text).rstrip() + "\n\n"
        if dest == "README":
            tnum = sec_num(top)
            if tnum == "1":
                readme_parts["定位"].append(body)
            elif tnum == "0":
                readme_parts["表述"].append(body)
            elif (sec_num(sub) if sub else "") == "15.4":
                readme_parts["解耦"].append(body)
            else:
                readme_parts["用法"].append(body)
        else:
            buckets[dest].append(body)

    if unrouted:
        print("!! 未路由的块:", unrouted)
        return 1

    OUT.mkdir(parents=True, exist_ok=True)
    written = []

    for name in ORDER:
        cn, desc = MODULE_META[name]
        header = [f"# {cn}", ""]
        if desc:
            header += [f"> {desc}", ">"]
        header += [
            f"> 模块文件：`{name}.md`",
            "> 定位：本文件是「实例化 Harness」设计的一个模块，不是完整文档。",
            "> 总索引见 [README.md](README.md)。",
            ">",
            "> **章节号沿用原单文件版编号**（所以模块内不连续是正常的、有意的）——",
            "> 这样全文上百处 `§x.y` 交叉引用全部继续有效。",
            "",
            "---",
            "", "",
        ]
        path = OUT / f"{name}.md"
        content = "\n".join(header) + "\n".join(buckets[name])
        path.write_text(content, encoding="utf-8")
        written.append((path.name, len(content.split("\n"))))

    # README
    nav = ["| 模块 | 内容 |", "|---|---|"]
    for n in ORDER:
        cn, desc = MODULE_META[n]
        nav.append(f"| [{n}]({n}.md) | {desc} |")
    readme = [
        "# 实例化 Harness（暂定名）· 设计总索引",
        "",
        "> 状态：构思中 · 2026-09-13 起记",
        "> 本目录由单文件版拆分而来，**按模块讨论**，不再维护单文件版。",
        "",
        "---",
        "",
    ]
    readme += readme_parts["定位"]
    readme += readme_parts["表述"]
    readme += ["## 模块地图", "", "一起讨论时按模块开文件。", ""] + nav + ["", "---", ""]
    readme += readme_parts["解耦"]
    readme += [
        "## 怎么用这份文档",
        "",
        "拆成模块是为了**按模块讨论**，不是按模块实现。几点约定：",
        "",
        "- **先定死的只有两处**：§2（kind/instance）与 §4（消息流）。其余都可以边做边改。",
        "- 每个模块的章节号沿用原编号，所以模块内不连续是正常的 ——",
        "  代价换来的是全文上百处 `§x.y` 交叉引用全部继续有效。",
        "- **结论一律记进 [09-决策](09-决策.md)**，不要只留在对话里。",
        "  「已定」划掉，「待定」新增。",
        "- 任何一个「以后再说」都要写成 `TODO -- 未决`，防止它变成隐性假设。",
        "- `规范-工具服务.md` 是**规范性**的，其余模块是探索性的。",
        "  改规范要更慎重：它是别人照着实现的东西。",
        "",
        "### 当前进度",
        "",
        "| 模块 | 状态 |",
        "|---|---|",
        "| 01-内核 | 骨架已定，待定 2 项 |",
        "| 02-工具 | 骨架已定，有一份独立规范 |",
        "| 03-上下文 | 缓存约束已定，治理策略待展开 |",
        "| 04-安全 | 原则已定，**沙箱 profile 未定** |",
        "| 05-窗口 | **最厚，且有实测支撑**；托管强度与观测裁剪待定 |",
        "| 06-界面 | 原则已定，未展开 |",
        "| 07-记忆 | 锚点策略已定，项目级锚点待定 |",
        "| 08-模型 | 拼装约束已定，复合模型归类待定 |",
        "| 规范-工具服务 | 四组字段草案，单一真相未定 |",
        "",
    ]
    (OUT / "README.md").write_text("\n".join(readme), encoding="utf-8")
    written.insert(0, ("README.md", len(readme)))

    print(f"输出目录: {OUT}")
    total = 0
    for fn, n in written:
        total += n
        print(f"  {fn:<28} {n:>5} 行")
    print(f"  {'合计':<28} {total:>5} 行")
    print(f"源文件: {len(lines)} 行")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
