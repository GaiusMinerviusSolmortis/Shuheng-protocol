//! **碎片库**：一个锚点一本账，每本账是**只追加的事件日志**（08-记忆 §16.9）。
//!
//! 为什么是事件日志而不是一张表：和会话日志同一个理由 ——
//! **改一个碎片的状态（pin / 忘记）也是一件事，那件事本身要留痕**。
//! 折叠出来的视图是**派生的**，随时能从日志重算（§5.5 那句"日志领先，缓存跟随"）。
//!
//! 三件从别处偷来的东西在这里落地（附录 §19）：
//!
//! | 偷来的 | 用在哪 |
//! |---|---|
//! | HTTP 的 **ETag** | 每条碎片一个（内容哈希）、每个锚点一个（全量指纹）—— 让调用方能说"没变就别给我" |
//! | DNS 的 **TTL** | 每条碎片可选过期时间；**过期不等于删除**，是"标出来、默认不给你看" |
//! | 只追加 + 墓碑 | 忘记一个人不是删行，是**记下"这条为什么不算数了"** |
//!
//! ⚠️ **召回是字面匹配，不是语义匹配。** 这一版只做 `query` 的子串 / tag 命中。
//! 别把"没召回出来"当成"没有这件事" —— 这条写进 `help` 里了（§16.6：降级必须显式）。

use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

/// `epistemic` 的**闭合取值** —— 它决定"这条东西有多可信"（§16.10）。
///
/// 为什么必须闭合：同一个项目里的记忆会来自**不同智能体**，
/// "这是谁的经验、还是谁的观点"是可信度的唯一依据。
/// 让每个服务自己编词，等于把那个依据变成自由文本。
pub const EPISTEMIC: [&str; 5] = ["fact", "decision", "experience", "opinion", "pointer"];

/// 一条碎片的文本上限。**碎片是"一句话"那一级的东西**（大眼那套的形状）。
///
/// 超了不是"存不下"，而是**存下去会毁掉这一层**：碎片一旦能装整篇文章，
/// 召回就等于把整个仓库倒进上下文 —— 那还不如不要这一层。
/// 长内容的正确去处是它原来的地方（文件 / 会话日志），碎片里留 `pointer` 指过去。
pub const MAX_TEXT_BYTES: usize = 8000;

#[derive(Debug, Clone, PartialEq)]
pub struct StoreError {
    pub why: String,
    pub remedy: String,
}

impl StoreError {
    fn new(why: impl Into<String>, remedy: impl Into<String>) -> Self {
        Self {
            why: why.into(),
            remedy: remedy.into(),
        }
    }
}

/// 一条碎片（折叠之后的样子）。
#[derive(Debug, Clone, PartialEq)]
pub struct Frag {
    pub id: String,
    pub text: String,
    pub epistemic: String,
    pub tags: Vec<String>,
    /// **谁写的**（§16.10：记忆挂锚点，但每条带来源）
    pub agent: String,
    /// **哪次会话里写的**（§16.9：会话是写入单位）
    pub session: String,
    pub at_ms: i64,
    /// 内容指纹：语义分类 + 文本（**不含时间** —— 同一句话重说一遍要能认出来）
    pub etag: String,
    pub expires_at_ms: Option<i64>,
    pub pinned: bool,
    /// 被显式忘记的原因（`None` = 还活着）。**不删，只标记**
    pub forgotten: Option<String>,
}

impl Frag {
    pub fn expired_at(&self, now: i64) -> bool {
        match self.expires_at_ms {
            Some(t) => now >= t,
            None => false,
        }
    }

    pub fn alive(&self) -> bool {
        self.forgotten.is_none()
    }

    fn view(&self) -> Value {
        json!({
            "id": self.id,
            "text": self.text,
            "epistemic": self.epistemic,
            "tags": self.tags,
            "agent": self.agent,
            "session": self.session,
            "at_ms": self.at_ms,
            "etag": self.etag,
            "expires_at_ms": self.expires_at_ms,
            "pinned": self.pinned,
        })
    }
}

/// 一条锚点的账。
pub struct AnchorLog {
    frags: BTreeMap<String, Frag>,
    /// 下一条碎片的序号 —— **只增**（同 id 铸造那条纪律）
    seq: u64,
}

/// 新建一条碎片的入参。
pub struct NewFrag<'a> {
    pub anchor: &'a str,
    pub text: &'a str,
    pub epistemic: &'a str,
    pub tags: Vec<String>,
    pub agent: &'a str,
    pub session: &'a str,
    pub now_ms: i64,
    /// 相对现在的存活时间；`None` / 非正数 = 永不过期
    pub ttl_ms: Option<i64>,
}

#[derive(Debug)]
pub struct DepositOutcome {
    pub id: String,
    pub etag: String,
    /// **新写的还是认出来的** —— 幂等靠它，而不是靠"假装又写成功了"
    pub stored: bool,
}

/// `recall` 的筛法。
#[derive(Debug, Clone, Default)]
pub struct Query {
    /// 字面匹配（大小写不敏感）：命中 text 或任一 tag
    pub text: Option<String>,
    pub epistemic: Option<String>,
    pub limit: Option<usize>,
    /// 调用方手上的锚点指纹：一样就回"没变"（HTTP `If-None-Match` 那条）
    pub if_none_match: Option<String>,
    /// 默认**不给过期的**；想看就显式要
    pub include_expired: bool,
    pub include_forgotten: bool,
}

pub struct RecallOutcome {
    pub anchor: String,
    pub etag: String,
    pub unchanged: bool,
    /// **确定性层**：被 pin 住的，永远先给（大眼那"每轮常驻 5 条"的形状）
    pub pinned: Vec<Value>,
    pub items: Vec<Value>,
    pub total: usize,
    pub expired_skipped: usize,
}

pub struct AnchorSummary {
    pub anchor: String,
    pub alive: usize,
    pub pinned: usize,
    pub expired: usize,
    pub forgotten: usize,
    pub last_at_ms: i64,
    pub etag: String,
}

pub struct Store {
    dir: PathBuf,
}

impl Store {
    pub fn open(dir: &Path) -> Result<Self, StoreError> {
        std::fs::create_dir_all(dir).map_err(|e| {
            StoreError::new(
                format!("碎片库建不出来（{}）：{e}", dir.display()),
                "给一个能写的目录；它只装碎片，不碰你的项目",
            )
        })?;
        Ok(Self {
            dir: dir.to_path_buf(),
        })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// 锚点 → 文件名。**可读前缀 + 哈希后缀**：前缀方便人肉认，后缀保证不撞。
    fn path_of(&self, anchor: &str) -> PathBuf {
        let mut safe: String = anchor
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        if safe.len() > 40 {
            safe.truncate(40);
        }
        let h = sha256_hex(anchor.as_bytes());
        self.dir.join(format!("{safe}-{}.jsonl", &h[..12]))
    }

    fn append(&self, anchor: &str, event: &Value) -> Result<(), StoreError> {
        let path = self.path_of(anchor);
        let line = serde_json::to_string(event).map_err(|e| {
            StoreError::new(format!("这条记不下来：{e}"), "这是服务侧的问题，报出来")
        })?;
        if line.contains('\n') {
            return Err(StoreError::new(
                "内容里有裸换行，会把账本截断",
                "把换行转义掉再写（这是服务侧该做的）",
            ));
        }
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| {
                StoreError::new(
                    format!("账本打不开（{}）：{e}", path.display()),
                    "检查碎片库目录的权限",
                )
            })?;
        f.write_all(line.as_bytes())
            .and_then(|_| f.write_all(b"\n"))
            .and_then(|_| f.flush())
            .map_err(|e| {
                StoreError::new(format!("写不进去：{e}"), "检查磁盘和权限；这条没落成")
            })
    }

    fn load(&self, anchor: &str) -> Result<AnchorLog, StoreError> {
        if anchor.trim().is_empty() {
            return Err(StoreError::new(
                "没有锚点",
                "**每条记忆都要说清它挂在哪儿**（项目路径 / 文件内容哈希 / 符号名，§16.5）。\
                 锚点由托管它的那一层给（会话），不是由这个服务猜",
            ));
        }
        let path = self.path_of(anchor);
        let mut frags: BTreeMap<String, Frag> = BTreeMap::new();
        let mut seq = 0u64;
        if let Ok(f) = std::fs::File::open(&path) {
            for (i, line) in BufReader::new(f).lines().enumerate() {
                let Ok(line) = line else { break };
                if line.trim().is_empty() {
                    continue;
                }
                // 坏行**跳过去并出声**，不要掐死整本账（同 ctrl 那条纪律）
                let Ok(ev) = serde_json::from_str::<Value>(&line) else {
                    eprintln!("[mem] 账本第 {} 行读不出来，跳过它", i + 1);
                    continue;
                };
                apply(&mut frags, &ev, &mut seq);
            }
        }
        Ok(AnchorLog { frags, seq })
    }

    pub fn deposit(&self, n: NewFrag) -> Result<DepositOutcome, StoreError> {
        if n.anchor.trim().is_empty() {
            return Err(StoreError::new(
                "没有锚点",
                "**每条记忆都要说清它挂在哪儿**（项目路径 / 文件内容哈希 / 符号名，§16.5）。\
                 锚点由托管它的那一层给（会话），不是由这个服务猜",
            ));
        }
        if n.text.trim().is_empty() {
            return Err(StoreError::new(
                "文本是空的",
                "碎片是「一句话」那一级的东西 —— 空的那不叫碎片",
            ));
        }
        if n.text.len() > MAX_TEXT_BYTES {
            return Err(StoreError::new(
                format!(
                    "这条 {} 字节，超了 {} 字节的上限",
                    n.text.len(),
                    MAX_TEXT_BYTES
                ),
                "碎片**只留结论**，长原文留在它原来的地方（文件 / 会话日志），\
                 这里放一条 `pointer` 指过去。理由：碎片一旦能装整篇文章，\
                 召回就等于把整个仓库倒进上下文 —— 那还不如不要这一层",
            ));
        }
        if !EPISTEMIC.contains(&n.epistemic) {
            return Err(StoreError::new(
                format!("`epistemic` 是 `{}`，不在闭合取值里", n.epistemic),
                format!(
                    "用其中一个：{}。这个字段是**可信度的唯一依据**（§16.10），\
                     不能自由发挥",
                    EPISTEMIC.join(" / ")
                ),
            ));
        }

        let etag = frag_etag(n.text, n.epistemic);
        let log = self.load(n.anchor)?;
        // **幂等**：同一条话再写一遍不算新碎片 —— 但要**如实回答"我认出来了"**
        if let Some(prev) = log.frags.values().find(|f| f.etag == etag && f.alive()) {
            return Ok(DepositOutcome {
                id: prev.id.clone(),
                etag,
                stored: false,
            });
        }

        let id = format!("f-{}", log.seq + 1);
        // `0` = **明确要求不过期**；负数 = **写错了**，不是"不过期"。
        // 两个意思混在一个分支里，以后没人分得清那条碎片到底是"故意留着"还是"算错了"。
        let expires_at_ms = match n.ttl_ms {
            None => None,
            Some(0) => None,
            Some(t) if t > 0 => Some(n.now_ms + t),
            Some(t) => {
                return Err(StoreError::new(
                    format!("`ttl_ms` 是 {t} —— 负数不是「不过期」"),
                    "要不过期就写 0（或者干脆不传）；要寿命就写正数毫秒",
                ))
            }
        };
        let ev = json!({
            "event": "deposit",
            "id": id,
            "anchor": n.anchor,
            "text": n.text,
            "epistemic": n.epistemic,
            "tags": n.tags,
            "agent": n.agent,
            "session": n.session,
            "at_ms": n.now_ms,
            "etag": etag,
            "expires_at_ms": expires_at_ms,
        });
        self.append(n.anchor, &ev)?;
        Ok(DepositOutcome {
            id,
            etag,
            stored: true,
        })
    }

    pub fn recall(&self, anchor: &str, q: &Query) -> Result<RecallOutcome, StoreError> {
        let log = self.load(anchor)?;
        let etag = anchor_etag(&log.frags);
        if let Some(want) = &q.if_none_match {
            if want == &etag {
                return Ok(RecallOutcome {
                    anchor: anchor.to_string(),
                    etag,
                    unchanged: true,
                    pinned: Vec::new(),
                    items: Vec::new(),
                    total: log.frags.len(),
                    expired_skipped: 0,
                });
            }
        }
        let now = harness_core::now_ms();
        let needle = q.text.as_ref().map(|s| s.to_lowercase());
        let mut pinned = Vec::new();
        let mut items = Vec::new();
        let mut expired_skipped = 0usize;
        for f in log.frags.values() {
            if !f.alive() {
                if q.include_forgotten {
                    items.push(f.view());
                }
                continue;
            }
            if f.expired_at(now) && !q.include_expired {
                // **过期不是删除**：默认不给，但**数出来**（"少了多少"必须看得见）
                expired_skipped += 1;
                continue;
            }
            if let Some(ep) = &q.epistemic {
                if &f.epistemic != ep {
                    continue;
                }
            }
            if let Some(n) = &needle {
                let hit = f.text.to_lowercase().contains(n)
                    || f.tags.iter().any(|t| t.to_lowercase().contains(n));
                if !hit {
                    continue;
                }
            }
            if f.pinned {
                pinned.push(f.view());
            } else {
                items.push(f.view());
            }
        }
        // 新的在前：回忆的时候"最近发生了什么"比"最早发生了什么"更有用
        items.reverse();
        let total = pinned.len() + items.len();
        let limit = q.limit.unwrap_or(20);
        items.truncate(limit);
        Ok(RecallOutcome {
            anchor: anchor.to_string(),
            etag,
            unchanged: false,
            pinned,
            items,
            total,
            expired_skipped,
        })
    }

    pub fn set_pin(&self, anchor: &str, id: &str, pinned: bool) -> Result<(), StoreError> {
        let log = self.load(anchor)?;
        let Some(f) = log.frags.get(id) else {
            return Err(StoreError::new(
                format!("这个锚点上没有 `{id}`"),
                "先用 `recall` 看有哪些碎片；id 是那条碎片自己的",
            ));
        };
        if f.forgotten.is_some() {
            return Err(StoreError::new(
                format!("`{id}` 已经被忘了"),
                "忘掉的东西不许再钉 —— 要么重新写一条，要么就让它过去",
            ));
        }
        self.append(
            anchor,
            &json!({"event": if pinned {"pin"} else {"unpin"}, "id": id,
                    "at_ms": harness_core::now_ms()}),
        )
    }

    /// **忘记 = 打一个墓碑，不是删行。**
    ///
    /// 为什么：① 账本只追加（同会话日志那条纪律）；② "这条被谁、什么时候、
    /// 因为什么否掉"本身是有价值的记忆 —— 删了就没人知道曾经有过它。
    /// 默认的 `recall` 不会再给你，但 `include_forgotten` 还能看见。
    pub fn forget(&self, anchor: &str, id: &str, why: &str) -> Result<(), StoreError> {
        let log = self.load(anchor)?;
        if !log.frags.contains_key(id) {
            return Err(StoreError::new(
                format!("这个锚点上没有 `{id}`"),
                "先用 `recall` 看有哪些碎片",
            ));
        }
        if why.trim().is_empty() {
            return Err(StoreError::new(
                "`forget` 要写清为什么",
                "墓碑上那句话是留给后来的人看的（「这条为什么不算数了」）—— 空着等于没写",
            ));
        }
        self.append(
            anchor,
            &json!({"event": "forget", "id": id, "why": why,
                    "at_ms": harness_core::now_ms()}),
        )
    }

    /// 点名看一个锚点的账；`None` = 全部锚点。
    pub fn summaries(&self, anchor: Option<&str>) -> Result<Vec<AnchorSummary>, StoreError> {
        let mut out = Vec::new();
        let now = harness_core::now_ms();
        if let Some(a) = anchor {
            let log = self.load(a)?;
            out.push(summarize(a, &log.frags, now));
            return Ok(out);
        }
        let entries = std::fs::read_dir(&self.dir).map_err(|e| {
            StoreError::new(
                format!("碎片库读不了（{}）：{e}", self.dir.display()),
                "检查目录还在不在",
            )
        })?;
        for e in entries.flatten() {
            let p = e.path();
            if p.extension().and_then(|s| s.to_str()) != Some("jsonl") {
                continue;
            }
            // 锚点存在**每一条事件里**，所以从第一行就能读回来 ——
            // 文件名那点可读前缀只是给人看的，不是真相
            let Ok(f) = std::fs::File::open(&p) else {
                continue;
            };
            let mut found = None;
            for line in BufReader::new(f).lines().map_while(Result::ok) {
                if line.trim().is_empty() {
                    continue;
                }
                if let Ok(v) = serde_json::from_str::<Value>(&line) {
                    if let Some(a) = v.get("anchor").and_then(Value::as_str) {
                        found = Some(a.to_string());
                        break;
                    }
                }
            }
            let Some(a) = found else { continue };
            let log = self.load(&a)?;
            out.push(summarize(&a, &log.frags, now));
        }
        out.sort_by(|a, b| a.anchor.cmp(&b.anchor));
        Ok(out)
    }
}

fn summarize(anchor: &str, frags: &BTreeMap<String, Frag>, now: i64) -> AnchorSummary {
    let mut alive = 0;
    let mut pinned = 0;
    let mut expired = 0;
    let mut forgotten = 0;
    let mut last_at_ms = 0;
    for f in frags.values() {
        last_at_ms = last_at_ms.max(f.at_ms);
        if !f.alive() {
            forgotten += 1;
            continue;
        }
        if f.expired_at(now) {
            expired += 1;
        }
        if f.pinned {
            pinned += 1;
        }
        alive += 1;
    }
    AnchorSummary {
        anchor: anchor.to_string(),
        alive,
        pinned,
        expired,
        forgotten,
        last_at_ms,
        etag: anchor_etag(frags),
    }
}

/// 折叠：把一条事件叠到视图上。
fn apply(frags: &mut BTreeMap<String, Frag>, ev: &Value, seq: &mut u64) {
    let kind = ev.get("event").and_then(Value::as_str).unwrap_or("");
    let id = ev.get("id").and_then(Value::as_str).unwrap_or("");
    if id.is_empty() {
        return;
    }
    if let Some(n) = id.strip_prefix("f-").and_then(|s| s.parse::<u64>().ok()) {
        *seq = (*seq).max(n);
    }
    match kind {
        "deposit" => {
            frags.insert(
                id.to_string(),
                Frag {
                    id: id.to_string(),
                    text: ev.get("text").and_then(Value::as_str).unwrap_or("").to_string(),
                    epistemic: ev
                        .get("epistemic")
                        .and_then(Value::as_str)
                        .unwrap_or("fact")
                        .to_string(),
                    tags: ev
                        .get("tags")
                        .and_then(Value::as_array)
                        .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
                        .unwrap_or_default(),
                    agent: ev.get("agent").and_then(Value::as_str).unwrap_or("?").to_string(),
                    session: ev
                        .get("session")
                        .and_then(Value::as_str)
                        .unwrap_or("?")
                        .to_string(),
                    at_ms: ev.get("at_ms").and_then(Value::as_i64).unwrap_or(0),
                    etag: ev.get("etag").and_then(Value::as_str).unwrap_or("").to_string(),
                    expires_at_ms: ev.get("expires_at_ms").and_then(Value::as_i64),
                    pinned: false,
                    forgotten: None,
                },
            );
        }
        "pin" | "unpin" => {
            if let Some(f) = frags.get_mut(id) {
                f.pinned = kind == "pin";
            }
        }
        "forget" => {
            if let Some(f) = frags.get_mut(id) {
                f.forgotten = Some(
                    ev.get("why")
                        .and_then(Value::as_str)
                        .unwrap_or("（没说为什么）")
                        .to_string(),
                );
            }
        }
        _ => {}
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    format!("{:x}", h.finalize())
}

/// 碎片指纹：**语义分类 + 文本**。
///
/// 为什么不带时间：同一条话换个时间再说一遍，应该被认成"我已经知道了"，
/// 而不是攒出第二条几乎一样的碎片。
fn frag_etag(text: &str, epistemic: &str) -> String {
    format!(
        "sha256:{}",
        &sha256_hex(format!("{epistemic}\u{1f}{text}").as_bytes())[..32]
    )
}

/// 锚点指纹：**所有碎片的状态**都在里面（id + 内容指纹 + 钉没钉 + 忘没忘）。
///
/// 为什么它比"条数"好：条数一样而内容变了的情况太常见了（改写、pin 变化），
/// 用条数当版本号会答"没变"—— 那是**撒谎**。
fn anchor_etag(frags: &BTreeMap<String, Frag>) -> String {
    let mut h = Sha256::new();
    for f in frags.values() {
        h.update(f.id.as_bytes());
        h.update(b"\x1f");
        h.update(f.etag.as_bytes());
        h.update(b"\x1f");
        h.update(if f.pinned { b"1" } else { b"0" });
        h.update(if f.forgotten.is_some() { b"1" } else { b"0" });
        h.update(b"\x1e");
    }
    format!("sha256:{}", &format!("{:x}", h.finalize())[..32])
}
