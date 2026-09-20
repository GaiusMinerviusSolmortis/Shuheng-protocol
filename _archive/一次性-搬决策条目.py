# -*- coding: utf-8 -*-
"""一次性：把本轮已定的几条从「待定」搬到「已定」对应主题下，并重算计数。

!! 一次性脚本 !!
每次跑都会按 MOVE 里的编号删除对应条目；重复跑会报"找不到"。
"""
from __future__ import annotations

import re
import sys
from pathlib import Path

F = Path(r"D:\agent\instance-harness\10-决策.md")

# 编号 -> (目标主题, 新的已定条目文本)
MOVE: dict[int, tuple[str, str]] = {
    1: ("工具与工位",
        "- ~~**工具层次的实现目标**~~ → **协议按 L3 设计，第一版实现 L2。**\n"
        "  终端就是标准的 L2（会话常驻、可查中间状态）。\n"
        "  **而且它天然能升级到 L3** —— 后台命令跑完时想通知智能体，那就是 L3。\n"
        "  所以协议留全、实现停在 L2，是既省事又不堵路的选法。（§5.1、§5.16）"),
    7: ("内核与协议",
        "- ~~**绑定是静态配置还是运行时可改**~~ → **智能体有权创建实例，但只有使用权。**\n"
        "  它能按需开新终端（后台任务不够用就再开一个），\n"
        "  但**用户永远能杀掉和浏览正在运行的终端** —— 创建自治，控制权归用户。（§7.1、§5.16）"),
    8: ("工具与工位",
        "- ~~**服务粒度**~~ → **按域分服务**，而且分三层：\n"
        "  **服务（一个域一个进程）→ 实例（按用途）→ 进程（PTY / 子进程）。**\n"
        "  不是一个工具一个进程（太重），也不是一个大服务管全部（太糊）。（§5.16）"),
    9: ("内核与协议",
        "- ~~**实例是纯数据还是带代码**~~ → **§2.1 已经答了：kind 带代码，instance 是纯数据。**\n"
        "  这条当初记成待定是我没接上 §2.1。**剩下的真问题是：kind 能不能运行时加载**\n"
        "  —— 也就是『框架是不是一个插件宿主』。（§2.1、§5.10）"),
    10: ("内核与协议",
        "- ~~**消息流的持久化粒度**~~ → 分两块：**会话事件全落盘**（重放不变量的前提，§9）；\n"
        "  **终端输出溢出到文件**，只在结果里给头尾 + 省略行数 + 全文位置（§5.17）。\n"
        "  ⚠️ 前提：**文件读取能力必须独立于终端**，否则读全文又变成一次终端调用。（§5.17）"),
    12: ("工具与工位",
        "- ~~**结果契约的适用范围**~~ → **五态不是工具的属性，是「通道」的属性。**\n"
        "  判据：这条通道最坏情况下分不分得清成败。\n"
        "  API / pattern → 二值；`PostMessage` → 要 `unverifiable`；合成输入 / 像素 → 全五态。\n"
        "  同一个工具跨多条通道时，**跟着当次 `route` 走**。（§11.2.1）"),
    83: ("工具与工位",
        "- ~~**终端会话的粒度**~~ → **一个实例 = 一条路径上的一件事**，智能体自己按用途开；\n"
        "  服务统一管，所以粒度不必绑死在工作区上。\n"
        "  代价是它得记住哪个 `term_id` 干什么 —— 所以**实例自带一句用途说明**（开的时候自己写），\n"
        "  并且这句要进面板（§11.1 E 组）和敕令（§4.4）。（§5.16）"),
}


def find_items(lines: list[str]) -> dict[int, tuple[int, int]]:
    """编号 -> (起, 止) 行号区间（左闭右开）"""
    spans: dict[int, tuple[int, int]] = {}
    i = 0
    while i < len(lines):
        m = re.match(r"^(\d+)\. \*\*", lines[i])
        if m:
            j = i + 1
            while j < len(lines):
                if re.match(r"^\d+\. \*\*", lines[j]) or re.match(r"^#{2,4} ", lines[j]) \
                        or lines[j].strip() == "---":
                    break
                if lines[j].strip() == "":
                    # 空行：看下一行是不是继续（缩进的续行）
                    k = j + 1
                    while k < len(lines) and lines[k].strip() == "":
                        k += 1
                    if k < len(lines) and re.match(r"^\s{2,}\S", lines[k]) \
                            and not re.match(r"^\d+\. \*\*", lines[k]):
                        j = k
                        continue
                    break
                j += 1
            spans[int(m.group(1))] = (i, j)
            i = j
        else:
            i += 1
    return spans


def main() -> int:
    try:
        sys.stdout.reconfigure(encoding="utf-8", errors="replace")
    except Exception:
        pass

    lines = F.read_text(encoding="utf-8").split("\n")
    spans = find_items(lines)
    missing = [n for n in MOVE if n not in spans]
    if missing:
        print(f"找不到这些编号（可能已经搬过了）: {sorted(missing)}")
        return 2

    # 1) 从待定里删掉（倒序删，避免行号错位）
    for n in sorted(MOVE, key=lambda x: spans[x][0], reverse=True):
        a, b = spans[n]
        del lines[a:b]
        print(f"  删除待定 #{n}")

    # 2) 追加到对应主题的已定小节末尾
    for n, (theme, bullet) in MOVE.items():
        head = next((i for i, ln in enumerate(lines)
                     if ln.startswith(f"### {theme}（")), None)
        if head is None:
            print(f"  !! 找不到主题小节 {theme}，跳过 #{n}")
            continue
        # 找该小节结束（下一个 ### 或 ---）
        end = head + 1
        while end < len(lines) and not re.match(r"^#{2,4} ", lines[end]) \
                and lines[end].strip() != "---":
            end += 1
        while end > head + 1 and lines[end - 1].strip() == "":
            end -= 1
        lines[end:end] = bullet.split("\n") + [""]
        print(f"  新增已定 → {theme}（#{n}）")

    # 3) 重算所有 "（N 条）" 计数
    text = "\n".join(lines)
    def recount(m: re.Match) -> str:
        theme = m.group(1)
        body_start = m.end()
        nxt = re.search(r"^#{2,4} ", text[body_start:], re.M)
        body = text[body_start: body_start + (nxt.start() if nxt else len(text))]
        cnt = len(re.findall(r"^- ~~", body, re.M)) or len(re.findall(r"^\d+\. \*\*", body, re.M))
        return f"### {theme}（{cnt} 条）"
    text = re.sub(r"^### ([^（\n]+)（\d+ 条）", recount, text, flags=re.M)
    F.write_text(text.rstrip() + "\n", encoding="utf-8")
    print("已重算各主题计数")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
