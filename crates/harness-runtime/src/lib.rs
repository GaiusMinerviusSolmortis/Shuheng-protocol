//! 枢恒执行账本。
//!
//! 这一层回答的不是“消息有没有送到”，而是“现实世界里可能发生了什么”：
//! 一次意图下有哪些执行尝试，每次尝试发出了哪些效果，效果最后是确认、部分完成、
//! 无法确认，还是明确拒绝。
//!
//! 账本采用只追加事件流。内存中的 `ExecutionSnapshot` 是从事件重放得到的读模型；
//! 磁盘上的 JSONL 才是持久事实。控制面崩溃后，未结算效果不会被悄悄当成失败或成功，
//! 而是通过 `recover_inflight` 转成 `unverifiable`。

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

/// 执行账本相对 data-dir 的路径。
pub const JOURNAL_FILE: &str = "runtime/execution.jsonl";

/// 单控制实例账本相对 data-dir 的目录前缀。
pub const JOURNAL_DIR: &str = "runtime/execution";

/// 一次副作用当前能被证实的程度。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectState {
    /// 已决定投递，但尚未收到终态。它绝不等于“没发生”。
    InFlight,
    /// 对端明确确认效果已经发生。
    Confirmed,
    /// 只完成了一部分，且完成到哪一步可知。
    Partial,
    /// 可能已经发生，但当前无法确认。
    Unverifiable,
    /// 有证据倾向认为没发生，但证据不足以支持自动重试。
    SuspectedNoop,
    /// 策略或参数导致动作没有发生。
    Refused,
}

impl EffectState {
    pub fn is_terminal(self) -> bool {
        self != Self::InFlight
    }

    /// 从协议结果里的 `effect` 字段读取终态。
    ///
    /// `in_flight` 不接受：结果消息里不能用一个“尚未完成”来结束一次调用。
    pub fn from_result(value: &str) -> Option<Self> {
        match value {
            "confirmed" => Some(Self::Confirmed),
            "partial" => Some(Self::Partial),
            "unverifiable" => Some(Self::Unverifiable),
            "suspected_noop" => Some(Self::SuspectedNoop),
            "refused" => Some(Self::Refused),
            _ => None,
        }
    }
}

/// 意图的终态。`partial` 是真实世界里的显式结果，不伪装成成功或失败。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntentState {
    Open,
    Succeeded,
    Partial,
    Failed,
    Abandoned,
}

impl IntentState {
    pub fn is_terminal(self) -> bool {
        self != Self::Open
    }
}

/// 一次执行尝试的终态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttemptState {
    Open,
    Succeeded,
    Partial,
    Failed,
    Abandoned,
}

impl AttemptState {
    pub fn is_terminal(self) -> bool {
        self != Self::Open
    }
}

/// 账本中的一次变化。字段使用稳定字符串 id；身份解析属于中控，不属于这个账本。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ExecutionChange {
    IntentOpened {
        intent_id: String,
        sid: String,
        owner: String,
        goal: String,
    },
    AttemptOpened {
        attempt_id: String,
        intent_id: String,
    },
    EffectStarted {
        effect_id: String,
        attempt_id: String,
        caller: String,
        target: String,
        tool: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        arguments_digest: Option<String>,
    },
    EffectSettled {
        effect_id: String,
        state: EffectState,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        code: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        evidence: Option<Value>,
    },
    AttemptFinished {
        attempt_id: String,
        state: AttemptState,
    },
    IntentFinished {
        intent_id: String,
        state: IntentState,
    },
}

/// 一行完整、不可变的事件。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExecutionEvent {
    pub seq: u64,
    pub at_ms: i64,
    #[serde(flatten)]
    pub change: ExecutionChange,
}

/// 从事件流重放出来的意图视图。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IntentView {
    pub intent_id: String,
    pub sid: String,
    pub owner: String,
    pub goal: String,
    pub state: IntentState,
    pub opened_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_ms: Option<i64>,
    pub attempts: Vec<String>,
}

/// 从事件流重放出来的尝试视图。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AttemptView {
    pub attempt_id: String,
    pub intent_id: String,
    pub state: AttemptState,
    pub opened_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_ms: Option<i64>,
    pub effects: Vec<String>,
}

/// 一次调用或物理动作的终态视图。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EffectView {
    pub effect_id: String,
    pub attempt_id: String,
    pub caller: String,
    pub target: String,
    pub tool: String,
    pub state: EffectState,
    pub started_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settled_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arguments_digest: Option<String>,
}

/// 可序列化的执行总览，适合直接给 GUI、审计工具或论文实验导出。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ExecutionSnapshot {
    pub intents: BTreeMap<String, IntentView>,
    pub attempts: BTreeMap<String, AttemptView>,
    pub effects: BTreeMap<String, EffectView>,
}

/// 状态迁移失败。账本宁可拒绝，也不通过覆盖写把矛盾事件洗掉。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransitionError {
    pub code: &'static str,
    pub why: String,
}

impl TransitionError {
    fn new(code: &'static str, why: impl Into<String>) -> Self {
        Self {
            code,
            why: why.into(),
        }
    }
}

impl std::fmt::Display for TransitionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}：{}", self.code, self.why)
    }
}

impl std::error::Error for TransitionError {}

/// 账本 IO 或格式错误。
#[derive(Debug)]
pub enum JournalError {
    Io(std::io::Error),
    Json(serde_json::Error),
    Corrupt(String),
    Transition(TransitionError),
}

impl std::fmt::Display for JournalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "执行账本 IO 失败：{e}"),
            Self::Json(e) => write!(f, "执行账本序列化失败：{e}"),
            Self::Corrupt(why) => write!(f, "{why}"),
            Self::Transition(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for JournalError {}

impl From<std::io::Error> for JournalError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<serde_json::Error> for JournalError {
    fn from(e: serde_json::Error) -> Self {
        Self::Json(e)
    }
}

impl From<TransitionError> for JournalError {
    fn from(e: TransitionError) -> Self {
        Self::Transition(e)
    }
}

/// 一次只追加的执行账本。
pub struct ExecutionJournal {
    path: PathBuf,
    writer: BufWriter<File>,
    last_seq: u64,
    snapshot: ExecutionSnapshot,
    dirty: bool,
}

impl ExecutionJournal {
    /// 打开或创建一个账本，并从完整事件重建当前视图。
    pub fn open(data_dir: &Path) -> Result<Self, JournalError> {
        Self::open_at(&data_dir.join(JOURNAL_FILE))
    }

    /// 打开某个会话控制实例独占的账本。
    ///
    /// 当前原型允许一个 data-dir 下有多个中控进程；每个进程独占一份账本，
    /// 避免两个进程各自从同一序号开始追加而互相覆盖。跨控制节点合并属于后续
    /// 联邦层，不能靠共享一个没有锁的 JSONL 假装做到。
    pub fn open_for_session(data_dir: &Path, sid: &str) -> Result<Self, JournalError> {
        Self::open_at(
            &data_dir
                .join(JOURNAL_DIR)
                .join(escape_segment(sid))
                .join("execution.jsonl"),
        )
    }

    fn open_at(path: &Path) -> Result<Self, JournalError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut snapshot = ExecutionSnapshot::default();
        let mut last_seq = 0;
        let mut needs_separator = false;
        if path.exists() {
            let raw = fs::read_to_string(path)?;
            let segments: Vec<&str> = raw.split_inclusive('\n').collect();
            let last_nonempty = segments
                .iter()
                .rposition(|s| !s.trim().is_empty())
                .unwrap_or(0);
            for (idx, segment) in segments.iter().enumerate() {
                let line = segment.trim_end_matches(['\r', '\n']);
                if line.trim().is_empty() {
                    continue;
                }
                let event: ExecutionEvent = match serde_json::from_str(line) {
                    Ok(v) => v,
                    Err(e) => {
                        // 只允许最后一行是崩溃时留下的半截记录。中间有坏行意味着
                        // 已确认发生的事件永久丢失，不能把后面的状态当作真相。
                        if idx == last_nonempty {
                            let keep = raw.rfind('\n').map(|i| i + 1).unwrap_or(0);
                            OpenOptions::new()
                                .write(true)
                                .open(path)?
                                .set_len(keep as u64)?;
                            break;
                        }
                        return Err(JournalError::Corrupt(format!(
                            "执行账本第 {} 行损坏：{e}",
                            idx + 1
                        )));
                    }
                };
                if event.seq != last_seq + 1 {
                    return Err(JournalError::Corrupt(format!(
                        "执行账本序号断裂：期望 {}，读到 {}",
                        last_seq + 1,
                        event.seq
                    )));
                }
                apply_event(&mut snapshot, &event).map_err(JournalError::Transition)?;
                last_seq = event.seq;
                if idx == last_nonempty && !segment.ends_with('\n') {
                    needs_separator = true;
                }
            }
        }
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        let mut journal = Self {
            path: path.to_path_buf(),
            writer: BufWriter::new(file),
            last_seq,
            snapshot,
            dirty: false,
        };
        if needs_separator {
            // 最后一条完整记录没有尾换行：先补行界，否则下一次追加会和它粘在一起。
            journal.writer.write_all(b"\n")?;
            journal.writer.flush()?;
        }
        Ok(journal)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn last_seq(&self) -> u64 {
        self.last_seq
    }

    pub fn snapshot(&self) -> &ExecutionSnapshot {
        &self.snapshot
    }

    pub fn open_intent(
        &mut self,
        intent_id: impl Into<String>,
        sid: impl Into<String>,
        owner: impl Into<String>,
        goal: impl Into<String>,
    ) -> Result<u64, JournalError> {
        self.append(ExecutionChange::IntentOpened {
            intent_id: intent_id.into(),
            sid: sid.into(),
            owner: owner.into(),
            goal: goal.into(),
        })
    }

    pub fn open_attempt(
        &mut self,
        attempt_id: impl Into<String>,
        intent_id: impl Into<String>,
    ) -> Result<u64, JournalError> {
        self.append(ExecutionChange::AttemptOpened {
            attempt_id: attempt_id.into(),
            intent_id: intent_id.into(),
        })
    }

    pub fn start_effect(
        &mut self,
        effect_id: impl Into<String>,
        attempt_id: impl Into<String>,
        caller: impl Into<String>,
        target: impl Into<String>,
        tool: impl Into<String>,
        arguments_digest: Option<String>,
    ) -> Result<u64, JournalError> {
        self.append(ExecutionChange::EffectStarted {
            effect_id: effect_id.into(),
            attempt_id: attempt_id.into(),
            caller: caller.into(),
            target: target.into(),
            tool: tool.into(),
            arguments_digest,
        })
    }

    pub fn settle_effect(
        &mut self,
        effect_id: impl Into<String>,
        state: EffectState,
        code: Option<String>,
        evidence: Option<Value>,
    ) -> Result<u64, JournalError> {
        self.append(ExecutionChange::EffectSettled {
            effect_id: effect_id.into(),
            state,
            code,
            evidence,
        })
    }

    pub fn finish_attempt(
        &mut self,
        attempt_id: impl Into<String>,
        state: AttemptState,
    ) -> Result<u64, JournalError> {
        self.append(ExecutionChange::AttemptFinished {
            attempt_id: attempt_id.into(),
            state,
        })
    }

    pub fn finish_intent(
        &mut self,
        intent_id: impl Into<String>,
        state: IntentState,
    ) -> Result<u64, JournalError> {
        self.append(ExecutionChange::IntentFinished {
            intent_id: intent_id.into(),
            state,
        })
    }

    /// 控制面重启时，把上次进程没等到终态的副作用明确标成“无法确认”。
    ///
    /// 它不会自动重试：`unverifiable` 的语义正是“先对账，别假装没发生”。
    pub fn recover_inflight(&mut self, code: &str) -> Result<usize, JournalError> {
        let pending: Vec<EffectView> = self
            .snapshot
            .effects
            .values()
            .filter(|e| e.state == EffectState::InFlight)
            .cloned()
            .collect();
        let n = pending.len();
        let mut attempts: Vec<String> = pending.iter().map(|e| e.attempt_id.clone()).collect();
        attempts.sort();
        attempts.dedup();
        for effect in pending {
            self.settle_effect(
                effect.effect_id,
                EffectState::Unverifiable,
                Some(code.into()),
                None,
            )?;
        }
        // 恢复不仅要把效果标成未知，还要把这个尝试和它的父意图收口。
        // 否则账本会出现“效果已终态，但父节点永远开着”的悬挂状态。
        for attempt_id in attempts {
            let terminal = self
                .snapshot
                .attempts
                .get(&attempt_id)
                .map(|a| {
                    a.state.is_terminal()
                        || a.effects.iter().all(|id| {
                            self.snapshot
                                .effects
                                .get(id)
                                .map(|e| e.state.is_terminal())
                                .unwrap_or(false)
                        })
                })
                .unwrap_or(false);
            if !terminal {
                continue;
            }
            if self
                .snapshot
                .attempts
                .get(&attempt_id)
                .map(|a| !a.state.is_terminal())
                .unwrap_or(false)
            {
                self.finish_attempt(&attempt_id, AttemptState::Failed)?;
            }
            let intent_id = self
                .snapshot
                .attempts
                .get(&attempt_id)
                .map(|a| a.intent_id.clone());
            if let Some(intent_id) = intent_id {
                let all_done = self
                    .snapshot
                    .intents
                    .get(&intent_id)
                    .map(|i| {
                        i.attempts.iter().all(|id| {
                            self.snapshot
                                .attempts
                                .get(id)
                                .map(|a| a.state.is_terminal())
                                .unwrap_or(false)
                        })
                    })
                    .unwrap_or(false);
                if all_done
                    && self
                        .snapshot
                        .intents
                        .get(&intent_id)
                        .map(|i| !i.state.is_terminal())
                        .unwrap_or(false)
                {
                    self.finish_intent(&intent_id, IntentState::Failed)?;
                }
            }
        }
        Ok(n)
    }

    /// 把尚未落盘的尾部推到稳定存储。
    pub fn barrier(&mut self) -> Result<bool, JournalError> {
        if !self.dirty {
            return Ok(false);
        }
        self.writer.flush()?;
        self.writer.get_ref().sync_all()?;
        self.dirty = false;
        Ok(true)
    }

    fn append(&mut self, change: ExecutionChange) -> Result<u64, JournalError> {
        let seq = self.last_seq + 1;
        let event = ExecutionEvent {
            seq,
            at_ms: harness_core::now_ms(),
            change,
        };
        // 先验证，再写盘：非法迁移不能留下“看起来发生过”的毒记录。
        let mut next = self.snapshot.clone();
        apply_event(&mut next, &event).map_err(JournalError::Transition)?;
        serde_json::to_writer(&mut self.writer, &event)?;
        self.writer.write_all(b"\n")?;
        self.writer.flush()?;
        self.snapshot = next;
        self.last_seq = seq;
        self.dirty = true;
        Ok(seq)
    }
}

fn escape_segment(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for byte in raw.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    if out.is_empty() || out == "." || out == ".." {
        format!("%{}-{}", out.len(), out.replace('.', "%2E"))
    } else {
        out
    }
}

impl Drop for ExecutionJournal {
    fn drop(&mut self) {
        if let Err(e) = self.barrier() {
            eprintln!("[runtime] 执行账本收尾落盘失败：{e}");
        }
    }
}

fn apply_event(
    snapshot: &mut ExecutionSnapshot,
    event: &ExecutionEvent,
) -> Result<(), TransitionError> {
    match &event.change {
        ExecutionChange::IntentOpened {
            intent_id,
            sid,
            owner,
            goal,
        } => {
            require_new(
                snapshot.intents.contains_key(intent_id),
                "intent_exists",
                "意图",
                intent_id,
            )?;
            snapshot.intents.insert(
                intent_id.clone(),
                IntentView {
                    intent_id: intent_id.clone(),
                    sid: sid.clone(),
                    owner: owner.clone(),
                    goal: goal.clone(),
                    state: IntentState::Open,
                    opened_ms: event.at_ms,
                    finished_ms: None,
                    attempts: Vec::new(),
                },
            );
        }
        ExecutionChange::AttemptOpened {
            attempt_id,
            intent_id,
        } => {
            require_new(
                snapshot.attempts.contains_key(attempt_id),
                "attempt_exists",
                "执行尝试",
                attempt_id,
            )?;
            let intent = snapshot.intents.get_mut(intent_id).ok_or_else(|| {
                TransitionError::new(
                    "intent_missing",
                    format!("执行尝试 `{attempt_id}` 的父意图 `{intent_id}` 不存在"),
                )
            })?;
            if intent.state.is_terminal() {
                return Err(TransitionError::new(
                    "intent_closed",
                    format!("已结束的意图 `{intent_id}` 不能再接执行尝试"),
                ));
            }
            intent.attempts.push(attempt_id.clone());
            snapshot.attempts.insert(
                attempt_id.clone(),
                AttemptView {
                    attempt_id: attempt_id.clone(),
                    intent_id: intent_id.clone(),
                    state: AttemptState::Open,
                    opened_ms: event.at_ms,
                    finished_ms: None,
                    effects: Vec::new(),
                },
            );
        }
        ExecutionChange::EffectStarted {
            effect_id,
            attempt_id,
            caller,
            target,
            tool,
            arguments_digest,
        } => {
            require_new(
                snapshot.effects.contains_key(effect_id),
                "effect_exists",
                "效果",
                effect_id,
            )?;
            let attempt = snapshot.attempts.get_mut(attempt_id).ok_or_else(|| {
                TransitionError::new(
                    "attempt_missing",
                    format!("效果 `{effect_id}` 的父尝试 `{attempt_id}` 不存在"),
                )
            })?;
            if attempt.state.is_terminal() {
                return Err(TransitionError::new(
                    "attempt_closed",
                    format!("已结束的尝试 `{attempt_id}` 不能再产生效果"),
                ));
            }
            attempt.effects.push(effect_id.clone());
            snapshot.effects.insert(
                effect_id.clone(),
                EffectView {
                    effect_id: effect_id.clone(),
                    attempt_id: attempt_id.clone(),
                    caller: caller.clone(),
                    target: target.clone(),
                    tool: tool.clone(),
                    state: EffectState::InFlight,
                    started_ms: event.at_ms,
                    settled_ms: None,
                    code: None,
                    evidence: None,
                    arguments_digest: arguments_digest.clone(),
                },
            );
        }
        ExecutionChange::EffectSettled {
            effect_id,
            state,
            code,
            evidence,
        } => {
            if !state.is_terminal() {
                return Err(TransitionError::new(
                    "effect_not_terminal",
                    format!("效果 `{effect_id}` 不能以 `in_flight` 结算"),
                ));
            }
            let effect = snapshot.effects.get_mut(effect_id).ok_or_else(|| {
                TransitionError::new("effect_missing", format!("效果 `{effect_id}` 不存在"))
            })?;
            if effect.state.is_terminal() {
                return Err(TransitionError::new(
                    "effect_closed",
                    format!(
                        "效果 `{effect_id}` 已按 `{:?}` 结算，不能改写成 `{state:?}`",
                        effect.state
                    ),
                ));
            }
            effect.state = *state;
            effect.code = code.clone();
            effect.evidence = evidence.clone();
            effect.settled_ms = Some(event.at_ms);
        }
        ExecutionChange::AttemptFinished { attempt_id, state } => {
            if *state == AttemptState::Open {
                return Err(TransitionError::new(
                    "attempt_not_terminal",
                    format!("执行尝试 `{attempt_id}` 不能以 `open` 结束"),
                ));
            }
            let attempt = snapshot.attempts.get_mut(attempt_id).ok_or_else(|| {
                TransitionError::new("attempt_missing", format!("执行尝试 `{attempt_id}` 不存在"))
            })?;
            if attempt.state.is_terminal() {
                return Err(TransitionError::new(
                    "attempt_closed",
                    format!("执行尝试 `{attempt_id}` 已经结束"),
                ));
            }
            let unresolved: Vec<&str> = attempt
                .effects
                .iter()
                .filter_map(|id| snapshot.effects.get(id))
                .filter(|e| !e.state.is_terminal())
                .map(|e| e.effect_id.as_str())
                .collect();
            if !unresolved.is_empty() {
                return Err(TransitionError::new(
                    "effect_unresolved",
                    format!(
                        "执行尝试 `{attempt_id}` 还有未结算效果：{}",
                        unresolved.join(", ")
                    ),
                ));
            }
            attempt.state = *state;
            attempt.finished_ms = Some(event.at_ms);
        }
        ExecutionChange::IntentFinished { intent_id, state } => {
            if *state == IntentState::Open {
                return Err(TransitionError::new(
                    "intent_not_terminal",
                    format!("意图 `{intent_id}` 不能以 `open` 结束"),
                ));
            }
            let intent = snapshot.intents.get_mut(intent_id).ok_or_else(|| {
                TransitionError::new("intent_missing", format!("意图 `{intent_id}` 不存在"))
            })?;
            if intent.state.is_terminal() {
                return Err(TransitionError::new(
                    "intent_closed",
                    format!("意图 `{intent_id}` 已经结束"),
                ));
            }
            let unresolved: Vec<&str> = intent
                .attempts
                .iter()
                .filter_map(|id| snapshot.attempts.get(id))
                .filter(|a| !a.state.is_terminal())
                .map(|a| a.attempt_id.as_str())
                .collect();
            if !unresolved.is_empty() {
                return Err(TransitionError::new(
                    "attempt_unresolved",
                    format!(
                        "意图 `{intent_id}` 还有未结束尝试：{}",
                        unresolved.join(", ")
                    ),
                ));
            }
            intent.state = *state;
            intent.finished_ms = Some(event.at_ms);
        }
    }
    Ok(())
}

fn require_new(
    exists: bool,
    code: &'static str,
    noun: &str,
    id: &str,
) -> Result<(), TransitionError> {
    if exists {
        Err(TransitionError::new(
            code,
            format!("{noun} `{id}` 已经存在；账本不会静默覆盖旧事实"),
        ))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temp_dir(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "shuheng-runtime-{tag}-{}-{}",
            std::process::id(),
            harness_core::id::short_rand()
        ))
    }

    fn open_intent(journal: &mut ExecutionJournal) {
        journal
            .open_intent("i-1", "s-1", "agent:a", "turn on the lamp")
            .unwrap();
        journal.open_attempt("a-1", "i-1").unwrap();
    }

    #[test]
    fn a_confirmed_effect_can_finish_the_whole_chain() {
        let dir = temp_dir("confirmed");
        let mut j = ExecutionJournal::open(&dir).unwrap();
        open_intent(&mut j);
        j.start_effect("c-1", "a-1", "agent:a", "lamp:1", "on", None)
            .unwrap();
        j.settle_effect(
            "c-1",
            EffectState::Confirmed,
            None,
            Some(json!({"observed": "on"})),
        )
        .unwrap();
        j.finish_attempt("a-1", AttemptState::Succeeded).unwrap();
        j.finish_intent("i-1", IntentState::Succeeded).unwrap();

        let snap = j.snapshot();
        assert_eq!(snap.effects["c-1"].state, EffectState::Confirmed);
        assert_eq!(snap.attempts["a-1"].state, AttemptState::Succeeded);
        assert_eq!(snap.intents["i-1"].state, IntentState::Succeeded);
    }

    #[test]
    fn an_inflight_effect_blocks_attempt_completion() {
        let dir = temp_dir("blocks");
        let mut j = ExecutionJournal::open(&dir).unwrap();
        open_intent(&mut j);
        j.start_effect("c-1", "a-1", "agent:a", "lamp:1", "on", None)
            .unwrap();
        let err = j
            .finish_attempt("a-1", AttemptState::Succeeded)
            .unwrap_err();
        assert!(err.to_string().contains("未结算"));
        assert_eq!(j.snapshot().attempts["a-1"].state, AttemptState::Open);
    }

    #[test]
    fn recovery_turns_an_old_inflight_effect_into_unverifiable() {
        let dir = temp_dir("recovery");
        let mut j = ExecutionJournal::open(&dir).unwrap();
        open_intent(&mut j);
        j.start_effect("c-1", "a-1", "agent:a", "door:1", "unlock", None)
            .unwrap();
        j.barrier().unwrap();
        drop(j);

        let mut reopened = ExecutionJournal::open(&dir).unwrap();
        assert_eq!(
            reopened.snapshot().effects["c-1"].state,
            EffectState::InFlight
        );
        assert_eq!(
            reopened.recover_inflight("controller_restarted").unwrap(),
            1
        );
        assert_eq!(
            reopened.snapshot().effects["c-1"].state,
            EffectState::Unverifiable
        );
        assert_eq!(
            reopened.snapshot().effects["c-1"].code.as_deref(),
            Some("controller_restarted")
        );
        assert_eq!(
            reopened.snapshot().attempts["a-1"].state,
            AttemptState::Failed
        );
        assert_eq!(
            reopened.snapshot().intents["i-1"].state,
            IntentState::Failed
        );
    }

    #[test]
    fn effect_ids_cannot_be_overwritten() {
        let dir = temp_dir("duplicate");
        let mut j = ExecutionJournal::open(&dir).unwrap();
        open_intent(&mut j);
        j.start_effect("c-1", "a-1", "agent:a", "lamp:1", "on", None)
            .unwrap();
        let err = j
            .start_effect("c-1", "a-1", "agent:b", "door:1", "unlock", None)
            .unwrap_err();
        assert!(err.to_string().contains("不会静默覆盖"));
    }

    #[test]
    fn settled_effects_cannot_be_rewritten() {
        let dir = temp_dir("rewrite");
        let mut j = ExecutionJournal::open(&dir).unwrap();
        open_intent(&mut j);
        j.start_effect("c-1", "a-1", "agent:a", "lamp:1", "on", None)
            .unwrap();
        j.settle_effect(
            "c-1",
            EffectState::Unverifiable,
            Some("timeout".into()),
            None,
        )
        .unwrap();
        let err = j
            .settle_effect("c-1", EffectState::Confirmed, None, None)
            .unwrap_err();
        assert!(err.to_string().contains("不能改写"));
    }

    #[test]
    fn reopen_resumes_sequence_and_state() {
        let dir = temp_dir("resume");
        let mut j = ExecutionJournal::open(&dir).unwrap();
        open_intent(&mut j);
        j.start_effect("c-1", "a-1", "agent:a", "lamp:1", "on", None)
            .unwrap();
        j.settle_effect("c-1", EffectState::Partial, Some("half".into()), None)
            .unwrap();
        let before = j.last_seq();
        j.barrier().unwrap();
        drop(j);

        let reopened = ExecutionJournal::open(&dir).unwrap();
        assert_eq!(reopened.last_seq(), before);
        assert_eq!(
            reopened.snapshot().effects["c-1"].state,
            EffectState::Partial
        );
    }

    #[test]
    fn result_effect_names_map_to_typed_states() {
        for (wire, state) in [
            ("confirmed", EffectState::Confirmed),
            ("partial", EffectState::Partial),
            ("unverifiable", EffectState::Unverifiable),
            ("suspected_noop", EffectState::SuspectedNoop),
            ("refused", EffectState::Refused),
        ] {
            assert_eq!(EffectState::from_result(wire), Some(state));
        }
        assert_eq!(EffectState::from_result("in_flight"), None);
        assert_eq!(EffectState::from_result("ok"), None);
    }

    #[test]
    fn a_half_written_tail_is_discarded_without_swallowing_the_next_event() {
        let dir = temp_dir("tail");
        let mut j = ExecutionJournal::open(&dir).unwrap();
        j.open_intent("i-1", "s-1", "agent:a", "goal").unwrap();
        j.barrier().unwrap();
        let path = j.path().to_path_buf();
        drop(j);
        let mut raw = fs::read_to_string(&path).unwrap();
        raw.push_str("{\"seq\":2,\"at_ms\":1,\"kind\":\"attempt_op");
        fs::write(&path, raw).unwrap();

        let mut reopened = ExecutionJournal::open(&dir).unwrap();
        assert_eq!(reopened.last_seq(), 1);
        reopened.open_attempt("a-1", "i-1").unwrap();
        reopened.barrier().unwrap();
        let lines: Vec<Value> = fs::read_to_string(&path)
            .unwrap()
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[1]["seq"], 2);
        assert_eq!(lines[1]["kind"], "attempt_opened");
    }
}
