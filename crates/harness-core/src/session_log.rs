//! 会话日志（规范 §5）。
//!
//! **日志记录 = 信封 + 落盘信息**：`{ seq, recorded_at, ...信封的全部字段... }`。
//! 布局见 07-界面 §12.7：`data/sessions/<sid>/{meta.json, log.jsonl}`。
//!
//! 这里同时实现 §5.5 的**落盘屏障**：`barrier()` = flush + `sync_all`。
//! 它不是"快照" —— 它不存数据，只保证"已经发生的事真的在磁盘上"。

use crate::envelope::Envelope;
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

/// 落盘顺序。**不是消息属性** —— 由中控分配（规范 §5）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LogRecord {
    pub seq: u64,
    /// 中控收到的时间。**和 `ts` 可能不同 —— 差值是排队时间**
    pub recorded_at: i64,
    #[serde(flatten)]
    pub envelope: Envelope,
}

#[derive(Debug)]
pub enum LogError {
    Io(std::io::Error),
    Json(serde_json::Error),
    /// **日志中间有洞**（不是什么 IO 故障，是这份日志本身不完整了）。
    ///
    /// 它和"尾部半残"是两件事（§5.5 的崩溃语义）：尾部可以半残 —— 崩在写那一下
    /// 本来就长这样；**中间不许有洞** —— 那意味着有记录不见了，而"可重放"这条
    /// 不变量就建立在"一条都不少"上。**少一条就不许装作能重建。**
    Corrupt(String),
}

impl std::fmt::Display for LogError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LogError::Io(e) => write!(f, "会话日志 IO 失败：{e}"),
            LogError::Json(e) => write!(f, "会话日志序列化失败：{e}"),
            LogError::Corrupt(why) => write!(f, "{why}"),
        }
    }
}

impl std::error::Error for LogError {}

impl From<std::io::Error> for LogError {
    fn from(e: std::io::Error) -> Self {
        LogError::Io(e)
    }
}

impl From<serde_json::Error> for LogError {
    fn from(e: serde_json::Error) -> Self {
        LogError::Json(e)
    }
}

pub struct SessionLog {
    dir: PathBuf,
    writer: BufWriter<File>,
    seq: u64,
    /// 距上次屏障以来有没有新写入 —— barrier 用它做短路
    dirty: bool,
    /// **一条记录里 payload 可以内联多少字节**（超了就溢出成文件，日志里放引用）。
    ///
    /// 这是 §5.1 那条"大 payload 溢出成文件、日志里放引用"的**阈值**。
    /// 默认 **65 536（64 KiB）**。
    ///
    /// ⚠️ **为什么不是 8 192**（第一版我照 §5.4 那张表取了 8 192，结果当场红了三份验收）：
    /// 这是**"大块"的门槛，不是"稍微大一点"的门槛**。8 192 会把**正常大小的消息**
    /// 也赶进引用 —— 比如 MCP 桥自报的那份 manifest（它把对端的工具表原样报过来），
    /// 于是所有**直接读日志**的消费者（验收脚本、以后的分析工具）都得先学会解引用
    /// 才能看见一份**本该在内联里**的东西。
    /// 大块（截图、一段几 MB 的文本、刷屏的终端）随便都超过 64 KiB，
    /// 而"正常的一条消息"远小于它 —— **门槛要落在两堆之间，不是落在中间那堆里面**。
    ///
    /// ⚠️ 它仍然是**部署选择**（`--log-inline`），不是普适值（同 `--inline-limit` 那条）。
    inline_limit: usize,
}

/// 默认的内联上限（64 KiB）。
pub const DEFAULT_LOG_INLINE: usize = 65_536;

/// **一条记录里的 payload 太大了** —— 它是引用，不是正文。
///
/// 形状（§5.1）：`{"spilled": {"file": "<相对会话目录>", "bytes": N}}`。
/// **引用是可解的**（不变量二要的正是这个）：`resolve_payload()` 把它换回正文，
/// 而"换回来必须逐字节一样"有一条判据盯着。
pub const SPILL_KEY: &str = "spilled";

/// 读回来的日志 + **尾部跳过了几行**。
///
/// 为什么要那个数：崩溃语义允许尾部半残，**但"我跳过了一行"这件事本身是事实**，
/// 得能报出去（同 §11.5 那条"诚实报告"）—— 静默跳过等于让调用方以为
/// 它看到的是全部记录。顺带它也是判据的抓手：`smoke-resume` 第 9 条就盯这一格。
#[derive(Debug, Clone, PartialEq)]
pub struct LogRead {
    pub records: Vec<LogRecord>,
    /// 文件末尾那几行**读不完的**（崩在写日志那一下会长这样）
    pub broken_tail: usize,
}

impl SessionLog {
    /// 打开（或创建）一个会话目录。
    ///
    /// `sid` 被当成**不信任的字符串**做单射转义，绝不直接拼进路径。
    pub fn open(data_dir: &Path, sid: &str) -> Result<Self, LogError> {
        let dir = data_dir.join("sessions").join(escape_segment(sid));
        fs::create_dir_all(&dir)?;
        let path = dir.join("log.jsonl");

        // seq 从已有日志的尾部续上 —— 会话是可暂停可恢复的（07-界面 §12.7）
        let seq = last_seq(&path);

        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        Ok(Self {
            dir,
            writer: BufWriter::new(file),
            seq,
            dirty: false,
            inline_limit: DEFAULT_LOG_INLINE,
        })
    }

    /// 改内联上限（`--log-inline`，部署选择）。
    pub fn set_inline_limit(&mut self, n: usize) {
        if n > 0 {
            self.inline_limit = n;
        }
    }

    pub fn inline_limit(&self) -> usize {
        self.inline_limit
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn log_path(&self) -> PathBuf {
        self.dir.join("log.jsonl")
    }

    pub fn last_seq(&self) -> u64 {
        self.seq
    }

    /// **把一条记录里的引用解回正文**（§5.1 那半："引用可解"）。
    ///
    /// 记录里是引用就返回正文，不是引用就**原样返回**（调用方不用先判断）。
    /// 解不开的时候**如实说**（`{"resolve_error": "..."}`）—— 那意味着这条日志
    /// 重建不出当时的东西了，那是**要立刻知道**的事，不是悄悄吞掉的事。
    pub fn resolve_payload(&self, payload: &serde_json::Value) -> serde_json::Value {
        let Some(rel) = payload
            .get(SPILL_KEY)
            .and_then(|s| s.get("file"))
            .and_then(serde_json::Value::as_str)
        else {
            return payload.clone();
        };
        let path = self.dir.join(rel);
        match fs::read(&path).and_then(|b| {
            serde_json::from_slice::<serde_json::Value>(&b)
                .map_err(|e| std::io::Error::other(e.to_string()))
        }) {
            Ok(v) => v,
            Err(e) => serde_json::json!({
                "resolve_error": format!("溢出文件读不回来（{}）：{e}", path.display())
            }),
        }
    }

    /// 把整份日志读回来。**重放要用它** —— 不变量二说"能从日志重建"，
    /// 那得有个人真的去读。
    ///
    /// **两条规则，方向相反**（§5.5 的崩溃语义）：
    ///
    /// | 情形 | 怎么办 |
    /// |---|---|
    /// | **尾部**几行读不完（崩在写那一下）| 跳过它们，**如实数出来**（`LogRead::broken_tail`）|
    /// | **中间**有洞（`seq` 不连续 / 中间一行读不出来）| **拒** —— 这也是原来那条 remedy 说的"中间有洞不行" |
    ///
    /// ⚠️ **2026-09-23 修的**：原来这里对**任何**一行读不出来都直接 `Err` —— 包括尾行。
    /// 于是"崩在写日志那一下"这个**规范明确允许**的情形，会让整份日志判死、
    /// 会话重建不出来：**规范和代码在同一件事上说的是反话**，而
    /// `last_seq`（重开时续 `seq` 用的那个）那一条路**已经**容尾部半残了 ——
    /// 同一个文件里的两个函数，两种脾气。（`smoke-resume` 第 8/9/10 条当场红。）
    pub fn read_all(&self) -> Result<LogRead, LogError> {
        let text = fs::read_to_string(self.log_path())?;
        // 带**行号**收集（报洞的时候要指得出是哪一行）
        let mut lines: Vec<(usize, &str)> = text
            .lines()
            .enumerate()
            .filter(|(_, l)| !l.trim().is_empty())
            .map(|(i, l)| (i + 1, l))
            .collect();

        // 一、**从尾巴往前**吃掉读不完的行 —— 那是半残的尾部，不是洞
        let mut broken_tail = 0usize;
        while let Some((_, line)) = lines.last() {
            if serde_json::from_str::<LogRecord>(line).is_ok() {
                break;
            }
            lines.pop();
            broken_tail += 1;
        }
        // 二、剩下的**一条都不许少**：读不出来是洞，`seq` 跳号也是洞
        let mut out: Vec<LogRecord> = Vec::with_capacity(lines.len());
        for (no, line) in lines {
            let r = match serde_json::from_str::<LogRecord>(line) {
                Ok(r) => r,
                Err(e) => {
                    return Err(LogError::Corrupt(format!(
                        "第 {no} 行读不出来：{e} —— 崩溃语义只容**尾部**半残，中间少一行都算洞"
                    )))
                }
            };
            if let Some(prev) = out.last() {
                if r.seq != prev.seq + 1 {
                    return Err(LogError::Corrupt(format!(
                        "第 {no} 行的 seq 是 {}，可上一条是 {} —— 中间少了 {} 条，这是个洞。\
                         尾部半残可以，**中间有洞不行**：少一条就不许装作能重建",
                        r.seq,
                        prev.seq,
                        r.seq.saturating_sub(prev.seq + 1)
                    )));
                }
            }
            out.push(r);
        }
        Ok(LogRead {
            records: out,
            broken_tail,
        })
    }

    /// 写一条记录，返回分配到的 `seq`。
    ///
    /// **不变量二（可重放）靠调用顺序保证**：先记进来，再投递。
    pub fn append(&mut self, envelope: &Envelope) -> Result<u64, LogError> {
        self.seq += 1;
        let mut envelope = envelope.clone();
        // **§5.1：大 payload 溢出成文件，日志里放引用。**
        //
        // 为什么必须在**写日志这一侧**做（原来靠"工具自己先截"）：
        // 那条路只管得住**工具结果**，管不住别的 —— 而日志是**不变量二**的载体，
        // 它得自己保证"不会被任何一条消息撑爆"。终端刷屏、一张截图、一段
        // 塞进 `args` 的大文本，都会从这儿过。
        //
        // ⚠️ **引用必须是可解的**（不变量二："能从日志重建"）：
        // 所以溢出文件**当场 fsync**（一次写 + 一次同步；它只在大 payload 上发生），
        // —— `barrier` 只 fsync 日志那条流，管不到这个文件，而"引用指向一个
        // 还在缓存里的文件"就等于"崩了之后重建不出来"。
        if let Some(reference) = self.spill_payload(self.seq, &mut envelope)? {
            debug_assert!(reference.is_object());
        }
        let record = LogRecord {
            seq: self.seq,
            recorded_at: crate::now_ms(),
            envelope,
        };
        let line = serde_json::to_string(&record)?;
        debug_assert!(!line.contains('\n'), "日志行里出现了裸换行，重放会被打断");
        self.writer.write_all(line.as_bytes())?;
        self.writer.write_all(b"\n")?;
        self.dirty = true;
        Ok(self.seq)
    }

    /// 太大就溢出；返回**那条引用**（没溢出就是 `None`）。
    fn spill_payload(
        &self,
        seq: u64,
        envelope: &mut Envelope,
    ) -> Result<Option<serde_json::Value>, LogError> {
        let bytes = serde_json::to_vec(&envelope.payload)?;
        if bytes.len() <= self.inline_limit {
            return Ok(None);
        }
        let dir = self.dir.join("spill");
        fs::create_dir_all(&dir)?;
        let file = dir.join(format!("{seq}.json"));
        // 一次写 + 一次同步：**引用可解**这件事不能只靠"大概写下去了"
        let mut f = File::create(&file)?;
        f.write_all(&bytes)?;
        f.sync_all()?;
        let reference = serde_json::json!({
            SPILL_KEY: {
                // 相对会话目录的路径：日志跟着会话走，引用也得跟着走
                "file": format!("spill/{seq}.json"),
                "bytes": bytes.len(),
                "note": "这条 payload 太大，正文在溢出文件里；`log {resolve:true}` 会就地展开它",
            }
        });
        envelope.payload = reference.clone();
        Ok(Some(reference))
    }

    /// **落盘屏障**：flush + fsync。**返回"这一次真的落了吗"**。
    ///
    /// 放在三个边界上（规范-消息协议 §5.5 #8）：
    /// **模型请求前** / **有副作用工具分派前** / **回合结束**。
    ///
    /// 三个调用点现在都接上了（2026-09-20）：
    ///
    /// | 边界 | 谁调 | 长什么样 |
    /// |---|---|---|
    /// | 有副作用工具分派前 | 中控 | `dispatch()` 里，判据是服务的 `destructiveHint` |
    /// | 模型请求前 | 智能体 | **`{"op":"barrier"}` 请中控落**（智能体不持有日志） |
    /// | 回合结束 | 中控 | 收摊 / 排空（`stdin` 关了之后）|
    ///
    /// **返回 `bool` 而不是 `()`**：`dirty` 为假时它会短路 —— 那一下**并没真的 fsync**。
    /// 调用方要能分清"落了"和"本来就干净"，否则"屏障生效了"就成了没法验的话
    /// （`synced: false` 是**诚实的**回答，不是失败）。
    ///
    /// 2026-09-19 定过"这四条日志不影响运行就依旧待定"（待定 #100）——
    /// 所以**边界怎么选仍然是暂定值**，但机制本身是真的（不再是"实现好了没人调"）。
    pub fn barrier(&mut self) -> Result<bool, LogError> {
        if !self.dirty {
            return Ok(false);
        }
        self.writer.flush()?;
        self.writer.get_ref().sync_all()?;
        self.dirty = false;
        Ok(true)
    }

    /// 写 `meta.json`。**记的是锚点，不是路径**（07-界面 §12.7）。
    pub fn write_meta(&self, meta: &serde_json::Value) -> Result<(), LogError> {
        let path = self.dir.join("meta.json");
        let tmp = self.dir.join("meta.json.tmp");
        fs::write(&tmp, serde_json::to_vec_pretty(meta)?)?;
        fs::rename(&tmp, &path)?;
        Ok(())
    }
}

fn escape_segment(sid: &str) -> String {
    let mut out = String::with_capacity(sid.len());
    for ch in sid.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' || ch == '.' {
            out.push(ch);
        } else {
            out.push_str(&format!("%{:02X}", (ch as u32) & 0xFF));
        }
    }
    if out.is_empty() {
        out.push_str("_empty");
    }
    out
}

/// 从已有日志里读最后一行的 `seq`。损坏的尾行按"没有"处理（崩溃语义：
/// **尾部可以半残，中间不许有洞**，规范-消息协议 §5.5）。
fn last_seq(path: &Path) -> u64 {
    let Ok(text) = fs::read_to_string(path) else {
        return 0;
    };
    for line in text.lines().rev() {
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(record) = serde_json::from_str::<LogRecord>(line) {
            return record.seq;
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::envelope::{Authority, MessageType};
    use serde_json::json;

    fn env(id: &str) -> Envelope {
        Envelope {
            id: id.into(),
            ts: 1,
            kind: MessageType::Event,
            from: "ctrl".into(),
            to: "console".into(),
            corr: None,
            sid: "s-1".into(),
            authority: Authority::System,
            payload: json!({"note": "hello\nworld"}),
        }
    }

    #[test]
    fn seq_is_monotonic_and_resumes() {
        let dir = std::env::temp_dir().join(format!("harness-log-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);

        {
            let mut log = SessionLog::open(&dir, "s-1").expect("open");
            assert_eq!(log.append(&env("m-1")).unwrap(), 1);
            assert_eq!(log.append(&env("m-2")).unwrap(), 2);
            log.barrier().expect("barrier");
        }

        // 重开：seq 必须从 3 续上，而不是从 1 重来
        let mut log = SessionLog::open(&dir, "s-1").expect("reopen");
        assert_eq!(log.last_seq(), 2);
        assert_eq!(log.append(&env("m-3")).unwrap(), 3);
        log.barrier().expect("barrier");

        // 含裸换行的 payload 必须被转义，仍然"一行一条"
        let text = fs::read_to_string(log.log_path()).unwrap();
        assert_eq!(text.lines().count(), 3, "三条记录必须是三行");
        assert!(text.contains("hello\\nworld"), "换行应当被转义");
    }

    #[test]
    fn session_dir_is_escaped() {
        assert_eq!(escape_segment("s-1"), "s-1");
        assert!(!escape_segment("../../evil").contains('/'));
        assert!(!escape_segment("..\\..\\evil").contains('\\'));
    }

    /// **尾部半残 = 跳过 + 如实数出来**（§5.5 的崩溃语义）。
    ///
    /// 崩在写日志那一下，文件尾部就是半行 JSON。规范说这**允许** ——
    /// 而 2026-09-23 之前 `read_all` 会对它整份拒（同一个文件里的 `last_seq`
    /// 却容它）：**规范和代码说了反话**。这条测试把那件事钉住。
    #[test]
    fn a_half_written_tail_is_skipped_and_counted() {
        let dir = std::env::temp_dir().join(format!("harness-log-tail-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        {
            let mut log = SessionLog::open(&dir, "s-1").expect("open");
            log.append(&env("m-1")).unwrap();
            log.append(&env("m-2")).unwrap();
            log.barrier().expect("barrier");
        }
        // 模拟崩在写第 3 条的那一下：尾部多出半行，既没换行也不是合法 JSON
        let path = dir.join("sessions").join("s-1").join("log.jsonl");
        let mut text = fs::read_to_string(&path).unwrap();
        text.push_str("{\"seq\":3,\"recorded_at\":1,\"id\":\"m-3\",\"ts\":0,\"type\":\"resu");
        fs::write(&path, &text).unwrap();

        let log = SessionLog::open(&dir, "s-1").expect("reopen");
        let read = log.read_all().expect("尾部半残不该把整份日志判死");
        assert_eq!(read.records.len(), 2, "两整条都在");
        assert_eq!(read.broken_tail, 1, "而且**如实数出**跳过了几行");
        // 重开时续的 seq 也不该把那半行当记录（同一条规则的另一半）
        assert_eq!(log.last_seq(), 2);
    }

    /// **中间有洞 = 拒**，而且说得出是哪一段断了。
    ///
    /// 它和尾部半残**不是一回事**：那是"最后一条没写完"，这是"有记录不见了" ——
    /// 而"可重放"这条不变量建立在**一条都不少**上。少一条就不许装作能重建。
    #[test]
    fn a_hole_in_the_middle_is_refused_with_its_seq() {
        let dir = std::env::temp_dir().join(format!("harness-log-hole-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        {
            let mut log = SessionLog::open(&dir, "s-1").expect("open");
            log.append(&env("m-1")).unwrap();
            log.append(&env("m-2")).unwrap();
            log.append(&env("m-3")).unwrap();
            log.barrier().expect("barrier");
        }
        // 把中间那条整行删掉：三行都还是合法 JSON，**只有 seq 对不上**
        let path = dir.join("sessions").join("s-1").join("log.jsonl");
        let text = fs::read_to_string(&path).unwrap();
        let keep: Vec<&str> = text
            .lines()
            .filter(|l| !l.trim().is_empty())
            .collect();
        fs::write(&path, format!("{}\n{}\n", keep[0], keep[2])).unwrap();

        let log = SessionLog::open(&dir, "s-1").expect("reopen");
        let e = log.read_all().expect_err("中间有洞必须拒");
        let msg = e.to_string();
        assert!(msg.contains("洞"), "要说清这是个洞：{msg}");
        assert!(msg.contains("3"), "要指得出断在哪个 seq：{msg}");
    }
}
