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
}

impl std::fmt::Display for LogError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LogError::Io(e) => write!(f, "会话日志 IO 失败：{e}"),
            LogError::Json(e) => write!(f, "会话日志序列化失败：{e}"),
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
        })
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

    /// 把整份日志读回来。**重放要用它** —— 不变量二说"能从日志重建"，
    /// 那得有个人真的去读。
    pub fn read_all(&self) -> Result<Vec<LogRecord>, LogError> {
        let text = fs::read_to_string(self.log_path())?;
        let mut out = Vec::new();
        for line in text.lines() {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<LogRecord>(line) {
                Ok(r) => out.push(r),
                // 尾行半残是允许的（崩溃语义：尾部可以半残、中间不许有洞）——
                // 但中间有洞要停下来说话
                Err(e) => return Err(LogError::Json(e)),
            }
        }
        Ok(out)
    }

    /// 写一条记录，返回分配到的 `seq`。
    ///
    /// **不变量二（可重放）靠调用顺序保证**：先记进来，再投递。
    pub fn append(&mut self, envelope: &Envelope) -> Result<u64, LogError> {
        self.seq += 1;
        let record = LogRecord {
            seq: self.seq,
            recorded_at: crate::now_ms(),
            envelope: envelope.clone(),
        };
        let line = serde_json::to_string(&record)?;
        debug_assert!(!line.contains('\n'), "日志行里出现了裸换行，重放会被打断");
        self.writer.write_all(line.as_bytes())?;
        self.writer.write_all(b"\n")?;
        self.dirty = true;
        Ok(self.seq)
    }

    /// **落盘屏障**：flush + fsync。
    ///
    /// 放在三个边界上（规范-消息协议 §5.5 #8）：
    /// 模型请求前 / 有副作用工具分派前 / 回合结束。
    ///
    /// TODO -- 未决 #8：**这三个边界还没拍板**（2026-09-19 定：不影响运行就依旧待定）。
    /// 机制本身必须存在（没有它日志谈不上持久），所以它是实现好了、
    /// **只是调用点没收口**。阶段 1 只在收摊时调一次（见 ctrl 的 `shutdown`）。
    /// 阶段 2 的编辑器是第一个有副作用的工具，那时要补的正是"分派前"那一次。
    pub fn barrier(&mut self) -> Result<(), LogError> {
        if !self.dirty {
            return Ok(());
        }
        self.writer.flush()?;
        self.writer.get_ref().sync_all()?;
        self.dirty = false;
        Ok(())
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
}
