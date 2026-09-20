# -*- coding: utf-8 -*-
"""一次性：把 05-窗口.md 拆成 05-窗口（§13）+ 06-操控（§14），并顺延后续文件编号。

!! 一次性脚本，勿重跑 !!
它假定当前是"05 里同时含 §13 与 §14"的状态；已经拆过就会错。

理由：05-窗口.md 当时 1535 行，占全文三分之一。
§13（托管的理论）和 §14（电脑操控 + 实测）本来就是两件事。
章节号不变（§13.x / §14.x），所以交叉引用不受影响；
变的只有文件边界与文件序号。
"""
from __future__ import annotations

import re
import sys
from pathlib import Path

ROOT = Path(r"D:\agent\instance-harness")

HEADER_TMPL = """# {title}

> {desc}
>
> 模块文件：`{fname}`
> 定位：本文件是「实例化 Harness」设计的一个模块，不是完整文档。
> 总索引见 [README.md](README.md)。
>
> **章节号沿用原单文件版编号**（所以模块内不连续是正常的、有意的）——
> 这样全文上百处 `§x.y` 交叉引用全部继续有效。

---

"""


def main() -> int:
    try:
        sys.stdout.reconfigure(encoding="utf-8", errors="replace")
    except Exception:
        pass

    src = ROOT / "05-窗口.md"
    lines = src.read_text(encoding="utf-8").split("\n")

    # 找 §14 的起点
    split_at = None
    for i, ln in enumerate(lines):
        if re.match(r"^## 14\.", ln):
            split_at = i
            break
    if split_at is None:
        print("没找到 §14 的起点，可能已经拆过了。中止。")
        return 2

    # §14 段（含）到文件尾
    sec14 = lines[split_at:]
    # §13 段：从第一个 ## 13 到 §14 之前
    start13 = next(i for i, ln in enumerate(lines) if re.match(r"^## 13\.", ln))
    sec13 = lines[start13:split_at]

    (ROOT / "05-窗口.md").write_text(
        HEADER_TMPL.format(
            title="窗口",
            desc="窗口托管：三种强度、会坏在哪、桌宠、以及它作为实例的含义。**§13。**",
            fname="05-窗口.md",
        ) + "\n".join(sec13).rstrip() + "\n",
        encoding="utf-8",
    )
    (ROOT / "06-操控.md").write_text(
        HEADER_TMPL.format(
            title="电脑操控",
            desc="观察契约、控制通道阶梯、实测数据、虚拟点击与输入锁。**§14，本文件最厚。**",
            fname="06-操控.md",
        ) + "\n".join(sec14).rstrip() + "\n",
        encoding="utf-8",
    )
    print("已拆：05-窗口.md（§13） + 06-操控.md（§14）")

    # 顺延后续文件编号（从大到小，避免互相覆盖）
    renames = [
        ("09-决策.md", "10-决策.md"),
        ("08-模型.md", "09-模型.md"),
        ("07-记忆.md", "08-记忆.md"),
        ("06-界面.md", "07-界面.md"),
    ]
    for old, new in renames:
        p = ROOT / old
        if p.exists():
            p.rename(ROOT / new)
            print(f"  改名 {old} → {new}")
        else:
            print(f"  !! {old} 不存在，跳过")

    # 修引用
    fix = {
        "06-界面.md": "07-界面.md",
        "07-记忆.md": "08-记忆.md",
        "08-模型.md": "09-模型.md",
        "09-决策.md": "10-决策.md",
        "05-窗口.md": "05-窗口.md",
    }
    touched = 0
    for f in sorted(ROOT.glob("*.md")):
        text = f.read_text(encoding="utf-8")
        orig = text
        # 先保护已经正确的 05/06（新文件），再替换旧的 06/07/08/09
        for old, new in fix.items():
            if old == new:
                continue
            text = text.replace(old, new)
        if text != orig:
            f.write_text(text, encoding="utf-8")
            touched += 1
            print(f"  修引用: {f.name}")
    print(f"共修改 {touched} 个文件的引用")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
