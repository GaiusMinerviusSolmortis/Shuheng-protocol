//! Shuheng-protocol 的内核协议层。
//!
//! 逐字实现 [规范-消息协议](../../规范-消息协议.md)：
//! 信封（§1）、消息种类（§2）、配对与因果（§3）、会话日志（§5）、NDJSON 传输（§6）。
//! 版本与兼容（§9）在 `version` 里 —— **它是唯一说出"这一版是几"的地方**。
//!
//! **三条不变量写进代码，不是文档里的一句希望**（规范 §0）—— 见 `invariant`。

pub mod codec;
pub mod direction;
pub mod envelope;
pub mod id;
pub mod instance_event;
pub mod invariant;
pub mod session_log;
pub mod version;

pub use envelope::{Authority, Envelope, MessageType};
pub use session_log::{LogRecord, SessionLog};
pub use version::{Compat, Version, PROTOCOL_VERSION};

/// Unix 毫秒。
pub fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
