#!/usr/bin/env python3
"""**大眼内核适配器** —— 把大眼那套记忆接成我们协议里的一个「内核服务」（2026-09-26）。

它是"**无视技术栈**"这句话的第一个真凭证：这个文件是 Python，谈的是
NDJSON 信封；对面那个智能体是 Rust，一行都没为它改过。

它提供两个动作（内核契约见 `crates/harness-agent/src/kernel.rs`）：

| 动作 | 干什么 |
|---|---|
| `recall` | 拿一段话去大眼的记忆库里捞相关的碎片回来（这是大眼的强项）|
| `review` | 上下文装不下时：**把要扔掉的那些沉淀进记忆库**，再给一句摘要 |

`review` 那一手是大眼的路子：**不是"压掉就算没了"，是"转到它关注的地方去"** ——
被裁掉的那些进了碎片库，下次 `recall` 还能捞回来。

⚠️ **两个诚实的边界**：

1. `sees_content: false`（实例配置里那一格）时，智能体**一个字节的正文都不发过来**，
   只有下标 / 类型 / 字节数。那时 `review` 只能做**结构裁剪**，摘要里会明说
   "这一刀只裁了结构、没有内容可沉淀" —— **不编一句像样的摘要出来**。
2. 大眼的 `embed()` 在没有 `sentence-transformers` 时会退化成一个纯哈希的向量
   （`requirements.txt` 里写着它是可选的）。那是**它的选择**，这里如实照用、
   不假装我们有一个语义模型。

跑法（中控的服务表里写这一行）：

```json
{ "kind": "kernel", "id": "kernel:1", "role": "tool",
  "cmd": ["python", "tools/bigeye-kernel/kernel_service.py"],
  "args": ["--bigeye", "D:\\agent\\bigeye-main"] }
```

而智能体的实例配置里：

```json
{ "kernel": { "kind": "remote", "service": "kernel:1", "sees_content": true } }
```
"""

import argparse
import json
import os
import re
import sys
import time


def log(msg: str) -> None:
    """**只写 stderr**：stdout 是协议通道（规范 §6.1 规矩三）。"""
    print(f"[kernel] {msg}", file=sys.stderr, flush=True)


class Store:
    """大眼碎片库的一层薄壳。**导入失败要说清为什么**，别装作能干活。"""

    def __init__(self, bigeye: str, db_path: str):
        if bigeye not in sys.path:
            sys.path.insert(0, bigeye)
        try:
            from memory.fragment_store import FragmentStore  # type: ignore
        except Exception as e:  # noqa: BLE001
            raise RuntimeError(
                f"导不进大眼的记忆模块（{bigeye}）：{e}。"
                "给 --bigeye 一个存在的 bigeye 目录（里面有 memory/ 那包）"
            ) from e
        self.store = FragmentStore(db_path)
        self.db_path = db_path

    def deposit(self, text: str, session: str) -> None:
        self.store.add(text, source="harness-kernel", user_tag=session, tags="autocompacted")

    def recall(self, query: str, top_k: int) -> list[str]:
        """先走向量召回，**捞空了就退回关键词**。

        为什么要有这条退路（2026-09-26 实测）：大眼的 `embed()` 在没有
        `sentence-transformers` / ONNX 模型时会退化成纯哈希向量，于是
        `recall` 的**相关性硬门槛把什么都筛掉了** —— 捞回来 0 条。
        那是"它没有语义模型"的如实结果，不该由我们假装修好了；
        但**库里明明有那条**，退一步用关键词捞是**同一份数据的另一种问法** ✓
        （我们自己的 `harness-mem` 今天也是字面匹配 —— 这条退路和它的水位一样）。
        """
        rows = self.store.recall(query, top_k=top_k)
        hits = [str(r["text"]) for r in (rows or []) if isinstance(r, dict) and r.get("text")]
        if hits:
            return hits
        try:
            # **分词再试**：整句 LIKE 基本捞不到东西（查询是一句人话，
            # 而库里存的是片段）。按空白与标点切开、从长到短试，命中就收。
            words = [w for w in re.split(r"[\s,，。:：;；!！?？、]+", query) if len(w) >= 2]
            words.sort(key=len, reverse=True)
            for w in words:
                rows = self.store.search_by_text(w, limit=top_k)
                hit = [str(r["text"]) for r in (rows or [])
                       if isinstance(r, dict) and r.get("text")]
                if hit:
                    log(f"关键词退路：用 `{w}` 捞到 {len(hit)} 条")
                    return hit
        except Exception as e:  # noqa: BLE001
            log(f"关键词退路也失败了：{e}")
        return []


class Attention:
    """**照搬大眼自己的注意力指标**（`attention_metrics.py`）。

    为什么必须用它、不能自己拍一个阈值：**"工作记忆占到多少算满"是它的判断**
    （阈值在它库里的 `meta.compress_token_limit`，默认 200000）；
    而它还给外部留了一个口子 —— `set_live_usage(真实用量)` ——
    让"别人的智能体"把自己的真实 token 数报进去。我们就走那条口子：
    **把我们的数字喂给它，让它按自己的尺子说满没满。**

    所以这里没有"裁一半"这种我们的规矩：裁多少也是按**它的估算函数**
    （`_est_tokens`：CJK 1.2 / 其他 0.3，×1.5）从前往后累计，直到低于它的阈值。
    """

    def __init__(self, bigeye: str):
        if bigeye not in sys.path:
            sys.path.insert(0, bigeye)
        import attention_metrics  # type: ignore

        self.m = attention_metrics

    def feed(self, used_tokens: int) -> None:
        """把**我们这个智能体**的真实用量喂进它的实时数据源，再让它重算。"""
        self.m.set_live_usage(used_tokens, [used_tokens])
        self.m.invalidate_cache()

    def m1(self) -> dict:
        """它的 M1：工作记忆使用率（`used / token_limit`）。"""
        return self.m.m1_work_memory_pct()

    def est_tokens(self, text: str) -> int:
        """**它的** token 估算（不是我另写一个）。"""
        return self.m._est_tokens(text)


class Service:
    def __init__(self, args):
        self.instance = args.instance
        self.sid = args.sid
        self.top_k = args.top_k
        self.store = None
        self.store_error = None
        try:
            self.store = Store(args.bigeye, args.db)
            log(f"大眼记忆库接上了：{args.db}")
        except Exception as e:  # noqa: BLE001
            # **起得来，但记得住"我为什么干不了活"** —— 每次调用如实报出来。
            self.store_error = str(e)
            log(f"记忆库没接上：{e}")
        try:
            self.attention = Attention(args.bigeye)
        except Exception as e:  # noqa: BLE001
            # 指标接不上就只能"不裁"（**保守那一边**），并如实说
            self.attention = None
            log(f"注意力指标没接上：{e} —— 那就不裁（保守）")
        self.seq = 0
        # **回信发给谁** —— 由进来的那条请求决定（见 `reply`）
        self.last_from = "ctrl"

    # ---- 出口 ---------------------------------------------------------
    def send(self, kind, to, corr, payload, authority="observe"):
        self.seq += 1
        env = {
            "id": f"m-{self.instance.replace(':', '.').replace(':', '-')}-{self.seq}",
            "ts": int(time.time() * 1000),
            "type": kind,
            "from": self.instance,
            "to": to,
            "sid": self.sid,
            "authority": authority,
            "payload": payload,
        }
        if corr is not None:
            env["corr"] = corr
        sys.stdout.write(json.dumps(env, ensure_ascii=False) + "\n")
        sys.stdout.flush()

    def reply(self, corr, payload):
        # ⚠️ **回给"谁问的"，不是回给中控**（2026-09-26 实测抓到的）：
        # 请求是智能体发的（`from: agent:dada` → `to: kernel:1`），
        # 结果就得回到 `from` 那边去。回给 ctrl 会被方向规则四判成
        # "孤儿结果"（中控没发起过这条调用）—— 那一行报错是真的，我写错了收件人。
        self.send("result", self.last_from, corr, payload)

    # ---- 两个动作 -----------------------------------------------------
    def do_recall(self, args):
        if self.store is None:
            return {
                "route": None, "effect": "refused", "code": "invalid",
                "why": f"记忆库没接上：{self.store_error}",
                "remedy": "给这个服务一个存在的 --bigeye 目录；这条是配置问题，不是运气问题",
            }
        query = args.get("query") or ""
        hits = self.store.recall(query, int(args.get("top_k") or self.top_k))
        # **如实报"捞回来几条"**：这条也是判据的抓手（捞回 0 条和捞回 3 条是两件事）
        log(f"recall：{query[:40]!r} → {len(hits)} 条")
        return {
            "route": None, "effect": "confirmed", "code": None,
            "items": [{"text": t} for t in hits],
            "note": f"大眼碎片库；这次捞回 {len(hits)} 条",
        }

    def do_review(self, args):
        items = args.get("items") or []
        sees = bool(args.get("sees_content"))
        texts = [it.get("text") for it in items]
        with_text = sum(1 for t in texts if t)
        # ⚠️ **这一行是 `sees_content` 那一格的证据**：它数的是"我真收到了几条正文"。
        log(f"review：收到 {len(items)} 项，其中带正文 {with_text} 项（sees_content={sees}）")
        used = int(args.get("used_tokens") or 0)
        # ---- 满没满：**问它的指标**（喂进真实用量，让它按自己的阈值说） ----
        if self.attention is None:
            return {"route": None, "effect": "confirmed", "code": None, "plan": None,
                    "note": "注意力指标接不上 —— 保守起见不裁"}
        self.attention.feed(used)
        m1 = self.attention.m1()
        limit = int(m1.get("token_limit") or 0)
        pct = m1.get("pct")
        log(f"review：M1 工作记忆 {m1.get('used_tokens')}/{limit}（{pct}%，来源 {m1.get('source')}）")
        if limit and used < limit:
            # **它说没满 ⇒ 不裁。** 这一行就是"照搬它的判断"的样子。
            return {"route": None, "effect": "confirmed", "code": None, "plan": None,
                    "note": f"它的 M1 说没满（{used}/{limit} = {pct}%），不裁"}
        # ---- 裁多少：按**它的估算**把用量降回它的阈值之下 ----
        need = used - limit
        cut, freed = 0, 0
        for i, t in enumerate(texts):
            if freed >= need and cut > 0:
                break
            cut = i + 1
            freed += self.attention.est_tokens(t or "")
        # **下限：绝不留成空** —— 最后一条不裁（它可能就是用户刚说的那句）
        cut = max(1, min(cut, len(items) - 1)) if len(items) > 1 else 0
        if cut == 0:
            return {"route": None, "effect": "confirmed", "code": None, "plan": None,
                    "note": "只剩一条，裁了就没上下文了"}
        if sees and self.store is not None:
            # **大眼的路子**：不是丢掉，是沉淀。压掉的那些进碎片库，下次能捞回来。
            deposited = 0
            for t in texts[:cut]:
                if t:
                    self.store.deposit(t, self.sid)
                    deposited += 1
            head = "；".join((t[:40] + "…") if len(t) > 40 else t for t in texts[:cut] if t)
            summary = f"【沉淀】前 {cut} 项已转进长期记忆（{deposited} 条），要点：{head}"
            log(f"review：沉淀 {deposited} 条进记忆库，摘要 {len(summary)} 字")
            return {"route": None, "effect": "confirmed", "code": None,
                    "plan": {"cut": cut, "summary_at": 0, "summary": summary,
                             "before_last_user": True},
                    "note": f"沉淀了 {deposited} 条"}
        # 没正文（或者记忆库没接上）：**只裁结构，而且明说**
        summary = (f"【裁剪】前 {cut} 项被裁掉 —— 这个内核声明了**不看正文**"
                   f"（或记忆库没接上），所以这里没有内容可沉淀，也没有要点可写。")
        return {"route": None, "effect": "confirmed", "code": None,
                "plan": {"cut": cut, "summary_at": 0, "summary": summary,
                         "before_last_user": True},
                "note": "只裁了结构"}

    # ---- 主循环 -------------------------------------------------------
    @staticmethod
    def _result_schema(own: dict, required_when: dict) -> dict:
        """**和契约里那份逐字段对齐**（`build_result_schema`）。

        为什么要照着抄一遍：结果是**闭合**的 —— 多一个没声明过的字段，
        中控的投递闸门会当场判 `schema_violation`。所以这几个信封字段一个都不能漏。
        """
        props = {
            "effect": {"enum": ["confirmed", "partial", "unverifiable",
                                "suspected_noop", "refused"]},
            "code": {"type": ["string", "null"]},
            "route": {"enum": [None, "accessibility", "synthetic_events", "global_input",
                               "dom", "trusted_input", "process"]},
            "why": {"type": "string"},
            "remedy": {"type": "string"},
            "expect": {"type": ["object", "string"]},
            "granted": {"type": ["string", "array"]},
            "caller_kind": {"type": "string"},
            "degraded": {"type": "boolean"},
            "degraded_reason": {"type": "string"},
            "data": {"type": "object"},
            "output": {"type": "string"},
            "text": {"type": "string"},
        }
        props.update(own)
        schema = {"type": "object", "required": ["effect", "code", "route"],
                  "additionalProperties": False, "properties": props}
        if required_when:
            schema["allOf"] = [
                {"if": {"properties": {"effect": {"enum": effects}}, "required": ["effect"]},
                 "then": {"required": fields}}
                for effects, fields in required_when.items()
            ]
        return schema

    def manifest(self):
        return {
            "contract_version": "1",
            "id": "kernel-bigeye",
            "version": "1",
            "title": "大眼内核（记忆）",
            "kind": "kernel",
            "domain": "memory",
            "platforms": ["windows", "linux", "macos"],
            "node_kind": "kernel",
            "state_vocabulary": ["idle", "busy", "closed"],
            "content": ["log"],
            "node_actions": ["close"],
            "instance_types": [],
            "summary": "把大眼那套记忆接成智能体的内核：捞回忆、沉淀被裁掉的上下文。",
            "when_to_use": "实例配置里写 kernel.kind=remote 并且 service 指到它。",
            "will": "它自己带着一个 SQLite 记忆库；它没了，**库还在**（那是它和别的内核不一样的地方）。",
            "injection_policy": "card",
            "help_sections": [
                {"id": "overview", "title": "概览"},
                {"id": "errors", "title": "错误码与补救"},
            ],
            "tools": [
                {
                    "name": "recall",
                    "description": "拿一段话去大眼的记忆库里捞相关的碎片回来。",
                    "input_schema": {"type": "object", "required": ["query"], "additionalProperties": False,
                                     "properties": {"query": {"type": "string"},
                                                    "session": {"type": "string"},
                                                    "top_k": {"type": "integer"}}},
                    "output_schema": self._result_schema(
                        {"items": {"type": "array"}, "note": {"type": "string"}},
                        {"confirmed": ["items"]}),
                    "annotations": {"readOnlyHint": True, "destructiveHint": False,
                                    "idempotentHint": True, "openWorldHint": False},
                    "capabilities": [],
                    "lifecycle": "per_session",
                    "idle_timeout": None,
                    "health": "ping",
                },
                {
                    "name": "review",
                    "description": "上下文装不下时：沉淀掉要裁的那些，再给一句摘要。",
                    "input_schema": {"type": "object", "additionalProperties": True,
                                     "properties": {"window": {"type": "integer"},
                                                    "used_tokens": {"type": "integer"},
                                                    "sees_content": {"type": "boolean"},
                                                    "items": {"type": "array"}}},
                    "output_schema": self._result_schema(
                        {"plan": {"type": ["object", "null"]}, "note": {"type": "string"}},
                        {"confirmed": ["note"]}),
                    "annotations": {"readOnlyHint": False, "destructiveHint": False,
                                    "idempotentHint": False, "openWorldHint": False},
                    "capabilities": [],
                    "lifecycle": "per_session",
                    "idle_timeout": None,
                    "health": "ping",
                },
            ],
            "sandbox": {"filesystem": "none", "network": "none", "identity": "same-user",
                        "executables": "none", "resources": "none"},
            "sandbox_note": "它只读写**自己那个 SQLite 库**（路径由 --db 给）；不联网、不拉子进程。",
            "error_codes": ["invalid", "refused"],
            "not_implemented": {
                "semantic_embedding": "没有 `sentence-transformers` 时，大眼的 `embed()` 会退化成"
                                      "纯哈希向量 —— 那是**它的**选择，这里如实照用，不假装有语义模型。",
                "reflect": "大眼的反思回路（reflection_loop）**没接**：这里只用了碎片库的"
                           "存与捞。它是下一步的事。",
            },
        }

    def run(self):
        self.send("presence", "ctrl", None,
                  {"state": "online", "kind": "kernel",
                   # ⚠️ **manifest 必须报**（2026-09-26 实测抓到的）：
                   # 只报 state 的话，中控那边 `manifest.is_some()` 是假 ——
                   # 于是它被当成"还没报到"，发给它的请求会进**攒着的队列**，
                   # 而那个队列是 tick 排空的……关了 tick 就**永远卡在那儿**。
                   # 一行 manifest 就能让整条路活了；省了它，症状是"智能体在等一个永远不来的回信"。
                   "manifest": self.manifest(),
                   "config": {"kernel": {"kind": "remote", "impl": "bigeye",
                                         "sees_content": True}}},
                  authority="observe")
        for line in sys.stdin:
            line = line.strip()
            if not line:
                continue
            try:
                env = json.loads(line)
            except Exception as e:  # noqa: BLE001
                log(f"收到一行读不懂的东西：{e}")
                continue
            if env.get("type") != "request":
                continue
            self.last_from = env.get("from") or "ctrl"
            payload = env.get("payload") or {}
            tool = payload.get("tool")
            args = payload.get("args") or {}
            corr = env.get("corr")
            if tool == "recall":
                self.dispatch(corr, self.do_recall, args)
            elif tool == "review":
                self.dispatch(corr, self.do_review, args)
            elif tool == "help":
                self.reply(corr, {"route": None, "effect": "confirmed", "code": None,
                                  "data": {"sections": ["overview", "errors"]}})
            elif tool == "ping" or tool == "status":
                self.reply(corr, {"route": None, "effect": "confirmed", "code": None,
                                  "data": {"pong": True, "db": self.store.db_path if self.store else None}})
            else:
                self.reply(corr, {"route": None, "effect": "refused", "code": "invalid",
                                  "why": f"没有名为 `{tool}` 的工具",
                                  "remedy": "这个内核只有两个动作：`recall` / `review`"})

    def dispatch(self, corr, fn, args):
        """**一次调用炸了不许带走整条通道**（同"坏消息不掐死通道"那条纪律）。

        库里抛异常是**会发生的**（大眼那套不是我们的代码）—— 那就如实回一条
        `refused` + 异常原文，让智能体看得见，而不是让它等到超时。
        """
        try:
            self.reply(corr, fn(args))
        except Exception as e:  # noqa: BLE001
            log(f"调用炸了：{type(e).__name__}: {e}")
            self.reply(corr, {
                "route": None, "effect": "refused", "code": "invalid",
                "why": f"大眼那边抛了异常：{type(e).__name__}: {e}",
                "remedy": "这一条是**服务内部**出错（不是你的参数）：看一眼这个服务的 stderr；\
                           要接着干活就先别用这个内核",
            })


def main():
    # ⚠️ **三条通道都要按 UTF-8 走**（2026-09-26 实测抓到）：
    # 协议是 UTF-8（规范 §6.1），而 Python 默认按**控制台编码**读 stdin ——
    # 在 GBK 的 Windows 上，中文进来会变成 surrogate，
    # 然后在大眼的 SQLite 那边炸出一句 `surrogates not allowed`：
    # **看起来像库的 bug，其实是通道没按协议读。**
    sys.stdin.reconfigure(encoding="utf-8", errors="strict")
    sys.stdout.reconfigure(encoding="utf-8")
    sys.stderr.reconfigure(encoding="utf-8", errors="replace")
    ap = argparse.ArgumentParser()
    ap.add_argument("--instance", default="kernel:1")
    ap.add_argument("--sid", default="s-1")
    ap.add_argument("--bigeye", default=os.path.join("D:\\", "agent", "bigeye-main"))
    ap.add_argument("--db", default=None, help="记忆库放哪（默认放在 bigeye 的 memory/ 下）")
    ap.add_argument("--top-k", type=int, default=3)
    a = ap.parse_args()
    if a.db is None:
        a.db = os.path.join(a.bigeye, "memory", "harness_adapter.sqlite")
    Service(a).run()


if __name__ == "__main__":
    main()
