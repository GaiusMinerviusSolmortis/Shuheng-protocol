# -*- coding: utf-8 -*-
"""一次性：把术语「工位」统一改成「工具实例」。

!! 一次性脚本，勿重跑 !!
来源：用户 2026-09-18 说"工位听着怪怪的"，并指出这个词源自他
"把工具变成办公软件"的设想 —— 所以改成"工具实例"，
但**保留"办公软件"作为解释性比喻**（不作为术语）。

处理两类：
  1. 特例：`工具即工位` → `工具即办公软件`（标题，保留比喻）
  2. 其余：`工位` → `工具实例`
"""
from __future__ import annotations

import re
import sys
from pathlib import Path

ROOT = Path(r"D:\agent\instance-harness")


def main() -> int:
    try:
        sys.stdout.reconfigure(encoding="utf-8", errors="replace")
    except Exception:
        pass

    total = 0
    for f in sorted(ROOT.glob("*.md")):
        text = f.read_text(encoding="utf-8")
        if "工位" not in text:
            continue
        n = text.count("工位")
        # 特例先处理
        text = text.replace("工具即工位", "工具即办公软件")
        text = text.replace("工位即办公软件", "工具即办公软件")
        # 其余统一
        text = text.replace("工位", "工具实例")
        f.write_text(text, encoding="utf-8")
        total += n
        print(f"  {f.name:<26} {n:>4} 处")
    print(f"合计 {total} 处")

    # 检查有没有生成怪词
    print("\n--- 检查生成的词 ---")
    for f in sorted(ROOT.glob("*.md")):
        t = f.read_text(encoding="utf-8")
        for m in re.finditer(r".{0,6}工具实例.{0,6}", t):
            s = m.group(0)
            if "工具实例实例" in s or "实例工具实例" in s:
                print(f"  !! {f.name}: {s}")
    print("（没有 !! 就是干净的）")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
