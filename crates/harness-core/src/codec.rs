//! NDJSON 传输（规范 §6.1）。
//!
//! 四条规矩：一行一个；stdin 进 stdout 出；stderr 留给日志；进程退出 = 会话结束。
//! 这里负责第 1 条，并把**不变量三（文本可达）**在运行时钉死。

use crate::envelope::Envelope;
use std::io::{BufRead, Write};

/// 信封允许出现的全部字段（规范 §1 的九行表格）。
pub const ENVELOPE_FIELDS: [&str; 9] = [
    "id", "ts", "type", "from", "to", "corr", "sid", "authority", "payload",
];

#[derive(Debug)]
pub enum CodecError {
    /// 空行 —— 忽略，不算错误
    Empty,
    Json(serde_json::Error),
    /// 信封是**闭合的**：多出来的字段一律拒绝，否则打错字会被静默吞掉
    UnknownField(String),
    /// JSON 顶层不是对象
    NotAnObject,
    Validation(crate::envelope::EnvelopeError),
    /// 不变量三的运行时断言失败了
    NotTextReachable,
}

impl std::fmt::Display for CodecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CodecError::Empty => write!(f, "空行"),
            CodecError::Json(e) => write!(f, "JSON 解析失败：{e}"),
            CodecError::UnknownField(name) => write!(
                f,
                "信封里有多余字段 `{name}`。信封是闭合的（规范 §1），只允许：{}",
                ENVELOPE_FIELDS.join(" / ")
            ),
            CodecError::NotAnObject => write!(f, "顶层不是一个 JSON 对象"),
            CodecError::Validation(e) => write!(f, "{e}"),
            CodecError::NotTextReachable => write!(
                f,
                "这一行序列化后含有裸换行，违反不变量三（文本可达）"
            ),
        }
    }
}

impl std::error::Error for CodecError {}

/// 编码成**一行**：紧凑 JSON，且断言里面没有裸换行。
pub fn encode(env: &Envelope) -> Result<String, CodecError> {
    let line = serde_json::to_string(&env).map_err(CodecError::Json)?;
    if line.contains('\n') || line.contains('\r') {
        return Err(CodecError::NotTextReachable);
    }
    Ok(line)
}

/// 解码一行。空行（含只有空白）返回 [`CodecError::Empty`]，调用方应当忽略。
pub fn decode(line: &str) -> Result<Envelope, CodecError> {
    if line.trim().is_empty() {
        return Err(CodecError::Empty);
    }
    // 先当 Value 看：为了在 serde 之前就把"多余字段"抓出来。
    // （serde 的 deny_unknown_fields 和会话日志用的 flatten 互斥，所以在这一层手查。）
    let value: serde_json::Value = serde_json::from_str(line).map_err(CodecError::Json)?;
    let obj = value.as_object().ok_or(CodecError::NotAnObject)?;
    for key in obj.keys() {
        if !ENVELOPE_FIELDS.contains(&key.as_str()) {
            return Err(CodecError::UnknownField(key.clone()));
        }
    }
    let env: Envelope = serde_json::from_value(value).map_err(CodecError::Json)?;
    env.validate().map_err(CodecError::Validation)?;
    Ok(env)
}

/// 一个 NDJSON 行读取器。
pub struct NdjsonReader<R: BufRead> {
    inner: R,
}

impl<R: BufRead> NdjsonReader<R> {
    pub fn new(inner: R) -> Self {
        Self { inner }
    }

    /// 读下一**非空**行，并且**不会被一行坏字节打断**。
    ///
    /// 这条纪律是被真实事故逼出来的：控制通道上的一个非 UTF-8 字节，
    /// 原先会让 `read_line` 报错、读线程直接退出，**整条通道从此静默死亡** ——
    /// 后面发的每一条消息都石沉大海，而没有任何人知道为什么。
    /// （触发它的是 PowerShell 的 `StandardInputEncoding` 默认走系统 ANSI 代码页，
    /// 于是中文被编成了 GBK 字节 —— 中文 Windows 上谁都会踩。）
    pub fn next_line_checked(&mut self) -> std::io::Result<NextLine> {
        let mut raw = Vec::new();
        loop {
            raw.clear();
            let n = self.inner.read_until(b'\n', &mut raw)?;
            if n == 0 {
                return Ok(NextLine::Eof);
            }
            while matches!(raw.last(), Some(b'\n') | Some(b'\r')) {
                raw.pop();
            }
            if raw.iter().all(|b| b.is_ascii_whitespace()) {
                continue;
            }
            return Ok(match String::from_utf8(raw.clone()) {
                Ok(line) => NextLine::Line(line),
                Err(_) => NextLine::Invalid { bytes: raw.len() },
            });
        }
    }

    /// 读下一**非空**行。`Ok(None)` = 流结束；非 UTF-8 的行返回 `Err`。
    ///
    /// 想"一行坏了也不断线"就用 [`Self::next_line_checked`]。
    pub fn next_line(&mut self) -> std::io::Result<Option<String>> {
        match self.next_line_checked()? {
            NextLine::Line(line) => Ok(Some(line)),
            NextLine::Invalid { .. } => Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "这一行不是合法的 UTF-8",
            )),
            NextLine::Eof => Ok(None),
        }
    }
}

/// 一行读出来是什么。
#[derive(Debug, Clone, PartialEq)]
pub enum NextLine {
    Line(String),
    /// 这一行不是合法的 UTF-8 —— **跳掉它，继续读下一行**
    Invalid { bytes: usize },
    Eof,
}

/// 写一行（自动补 `\n` 并 flush）。**stdout 只写协议消息。**
pub fn write_line<W: Write>(w: &mut W, line: &str) -> std::io::Result<()> {
    w.write_all(line.as_bytes())?;
    w.write_all(b"\n")?;
    w.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::envelope::{Authority, MessageType};
    use serde_json::json;

    fn sample(kind: MessageType) -> Envelope {
        Envelope {
            id: "m-1".into(),
            ts: 1,
            kind,
            from: "ctrl".into(),
            to: "echo:e-1".into(),
            corr: Some("c-1".into()),
            sid: "s-1".into(),
            authority: Authority::User,
            payload: json!({}),
        }
    }

    /// 不变量三：**每一种**消息都必须能过一行文本往返。
    #[test]
    fn every_message_type_round_trips_as_one_line() {
        for kind in MessageType::ALL {
            let env = sample(kind);
            let line = encode(&env).expect("encode");
            assert!(!line.contains('\n'), "{kind:?} 编码出了裸换行");
            let back = decode(&line).expect("decode");
            assert_eq!(back, env);
        }
    }

    #[test]
    fn rejects_unknown_fields() {
        let line = r#"{"id":"m-1","ts":1,"type":"event","from":"a","to":"b","sid":"s-1","authority":"system","payload":{},"oops":1}"#;
        match decode(line) {
            Err(CodecError::UnknownField(name)) => assert_eq!(name, "oops"),
            other => panic!("应当拒绝多余字段，却得到 {other:?}"),
        }
    }

    #[test]
    fn rejects_payload_that_is_not_an_object() {
        let line = r#"{"id":"m-1","ts":1,"type":"event","from":"a","to":"b","sid":"s-1","authority":"system","payload":42}"#;
        match decode(line) {
            Err(CodecError::Validation(e)) => assert_eq!(e.field, "payload"),
            other => panic!("应当拒绝非对象 payload，却得到 {other:?}"),
        }
    }

    #[test]
    fn rejects_message_id_without_prefix() {
        let line = r#"{"id":"7f3a","ts":1,"type":"event","from":"a","to":"b","sid":"s-1","authority":"system","payload":{}}"#;
        match decode(line) {
            Err(CodecError::Validation(e)) => assert_eq!(e.field, "id"),
            other => panic!("应当拒绝无前缀 id，却得到 {other:?}"),
        }
    }

    #[test]
    fn blank_lines_are_skippable() {
        let src = "\n\n";
        let mut r = NdjsonReader::new(src.as_bytes());
        assert!(r.next_line().unwrap().is_none());
    }

    /// 一行坏字节**不能**让整条通道死掉。
    #[test]
    fn invalid_utf8_is_reported_without_killing_the_reader() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&[0xff, 0xfe, 0x41, b'\n']); // 坏行
        bytes.extend_from_slice(b"good line\n");
        let mut r = NdjsonReader::new(&bytes[..]);
        assert_eq!(r.next_line_checked().unwrap(), NextLine::Invalid { bytes: 3 });
        assert_eq!(
            r.next_line_checked().unwrap(),
            NextLine::Line("good line".into()),
            "坏行之后必须还能读到下一行"
        );
        assert_eq!(r.next_line_checked().unwrap(), NextLine::Eof);
    }

    /// 反过来：`next_line` 保留"遇到坏行就报错"的老语义。
    #[test]
    fn strict_reader_still_errors_on_invalid_utf8() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&[0xff, b'\n']);
        let mut r = NdjsonReader::new(&bytes[..]);
        assert!(r.next_line().is_err());
    }
}
