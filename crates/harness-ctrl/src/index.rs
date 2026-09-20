//! **实例索引**：一张跨会话的、只追加的 `id → 它是什么 / 归谁 / 什么时候出现` 表。
//!
//! 为什么会话日志不够：
//!
//! * 日志是**按会话**切的（`data/sessions/<sid>/log.jsonl`）——
//!   要回答"`term:1/t-2` 是谁、哪个会话开过它"，得把每个会话翻一遍；
//! * 日志记的是**消息**，不是**身份**。同一个 id 在几十条消息里出现，
//!   但"它是什么时候登记的、归谁、什么时候没的"这三件事要人肉扫。
//!
//! 所以这里再来一份**按身份**的账：一行一个事件，**只追加、从不改写**。
//! 它不是日志的替代品 —— 日志仍然是唯一真相（不变量二），
//! 这份索引是**从它派生出来的读模型**：丢了可以从日志重建，
//! 有了就不用每次重建。
//!
//! ```text
//! data/sessions/<sid>/log.jsonl     消息流（唯一真相）
//! data/index/instances.jsonl        身份流（派生的索引，跨会话）
//! ```
//!
//! 四层都记，而且第二层到第四层之间的**那条链**能从记录里算出来：
//!
//! ```text
//! s-1789805392453-beac                    ← 会话
//!   └─ agent:dada@s-1789805392453-beac    ← 智能体（记录里有 sid）
//!        └─ term:1/t-2                    ← 工具实例（记录里有 owner 和 service）
//! ```

use serde_json::{json, Value};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

/// 索引文件的相对位置：`<data-dir>/index/instances.jsonl`
pub const FILE: &str = "index/instances.jsonl";

pub struct IndexLog {
    path: PathBuf,
    file: Option<File>,
    /// 写失败过一次就别每行都喊一遍 —— 但**也不能装作没事**
    complained: bool,
}

impl IndexLog {
    pub fn open(data_dir: &Path) -> Self {
        let path = data_dir.join(FILE);
        let file = path.parent().map(fs::create_dir_all).transpose();
        let file = match file {
            Ok(_) => OpenOptions::new().create(true).append(true).open(&path).ok(),
            Err(e) => {
                eprintln!("[ctrl] 建不了索引目录 {}：{e}", path.display());
                None
            }
        };
        if file.is_none() {
            eprintln!(
                "[ctrl] 索引 {} 打不开 —— 会话照常跑，但**跨会话的追溯会少一份账**",
                path.display()
            );
        }
        Self {
            path,
            file,
            complained: false,
        }
    }

    /// 追加一行。**失败不中断会话** —— 索引是账本，不是命脉；
    /// 但也不静默：第一次失败说清楚。
    pub fn append(&mut self, sid: &str, id: &str, kind: &str, event: &str, extra: Value) {
        let mut rec = json!({
            "ts": harness_core::now_ms(),
            "sid": sid,
            "id": id,
            "kind": kind,
            "event": event,
        });
        if let (Some(obj), Some(more)) = (rec.as_object_mut(), extra.as_object()) {
            for (k, v) in more {
                obj.insert(k.clone(), v.clone());
            }
        }
        let Some(file) = self.file.as_mut() else {
            return;
        };
        let line = format!("{rec}\n");
        if let Err(e) = file.write_all(line.as_bytes()).and_then(|_| file.flush()) {
            if !self.complained {
                eprintln!("[ctrl] 索引写不进去（{}）：{e}", self.path.display());
                self.complained = true;
            }
        }
    }

    /// 把索引读回来（可以按 id / sid 过滤）。
    ///
    /// 读模型**故意简单**：整份读进来、过滤、只留最后 N 条。
    /// 索引是只追加的小文件；真大到读不动的那天，再换成游标扫描 ——
    /// 但那时先说清代价，不要现在就上复杂度。
    pub fn read(
        data_dir: &Path,
        id: Option<&str>,
        sid: Option<&str>,
        limit: usize,
    ) -> Result<(Vec<Value>, usize), String> {
        let path = data_dir.join(FILE);
        let file = File::open(&path).map_err(|e| format!("读不出 {}：{e}", path.display()))?;
        let mut all: Vec<Value> = Vec::new();
        for line in BufReader::new(file).lines() {
            let line = line.map_err(|e| e.to_string())?;
            if line.trim().is_empty() {
                continue;
            }
            let rec: Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                // 坏行跳过：索引是派生数据，一行脏不该让整次查询失败
                Err(_) => continue,
            };
            if let Some(want) = id {
                if rec.get("id").and_then(Value::as_str) != Some(want) {
                    continue;
                }
            }
            if let Some(want) = sid {
                if rec.get("sid").and_then(Value::as_str) != Some(want) {
                    continue;
                }
            }
            all.push(rec);
        }
        let total = all.len();
        let keep = if limit == 0 { total } else { limit.min(total) };
        let tail = all.split_off(total - keep);
        Ok((tail, total))
    }
}

/// **从索引记录里算出那条链**：会话 → 智能体 → 工具实例。
///
/// 只做一件机械的事：实例记录里有 `owner`（`agent:dada@s-x`）和 `service`，
/// 把 `owner` 拆成"谁 + 哪个会话"就得到整条链。
/// **它不是推断**（不是猜这两个东西有没有关系）—— 关系是服务在上报归属时写死的。
pub fn chain_of(records: &[Value]) -> Option<Value> {
    let rec = records.iter().find(|r| r.get("owner").is_some())?;
    let owner = rec.get("owner")?.as_str()?;
    let (agent, sid) = owner.split_once('@').unwrap_or(("(用户)", owner));
    Some(json!({
        "session": sid,
        "agent": agent,
        "service": rec.get("service").and_then(Value::as_str),
        "instance": rec.get("instance").and_then(Value::as_str),
        "id": rec.get("id").and_then(Value::as_str),
        "note": "这条链是从索引记录里算出来的，不是猜的：owner 由服务在上报归属时写死"
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chain_splits_the_owner_into_who_and_where() {
        let recs = vec![json!({
            "id": "term:1/t-2", "service": "term:1", "instance": "t-2",
            "owner": "agent:dada@s-1",
        })];
        let c = chain_of(&recs).expect("有 owner 就该算得出链");
        assert_eq!(c["session"], "s-1");
        assert_eq!(c["agent"], "agent:dada");
        assert_eq!(c["service"], "term:1");
        assert_eq!(c["instance"], "t-2");
    }

    #[test]
    fn no_owner_means_no_chain() {
        assert!(chain_of(&[json!({"id": "term:1", "event": "presence"})]).is_none());
    }
}
