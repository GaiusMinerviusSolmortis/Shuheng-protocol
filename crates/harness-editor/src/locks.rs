//! **写互斥**（02-工具 §5.15："实例是天然的锁点"）。
//!
//! | 规则 | 出处 |
//! |---|---|
//! | 读自由、写互斥 | §5.15 |
//! | 锁带持有者（哪个会话、哪个实例） | §5.15 |
//! | **拿不到就报错，不排队** | 2026-09-19 你定 |
//! | 持有会话死了，锁自动释放 | §5.15 —— 在这里由"锁活在进程内"天然满足 |
//! | 同一个持有者重复 open 是**幂等**的 | 本文件（否则它会把自己锁死）|

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// 谁在持有。**锁的持有者是一个活着的会话**，所以这两样都要有。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Holder {
    pub sid: String,
    /// 调用方实例 id（`console` / `agent:dada` …）
    pub agent: String,
}

#[derive(Debug, Clone)]
pub struct LockEntry {
    pub instance: String,
    pub holder: Holder,
    pub since_ms: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Acquire {
    /// 拿到（`new` = 是不是新建的实例）
    Granted { instance: String, new: bool },
    Busy {
        instance: String,
        holder: Holder,
        held_ms: i64,
    },
}

#[derive(Debug, Default)]
pub struct LockTable {
    inner: BTreeMap<PathBuf, LockEntry>,
}

impl LockTable {
    /// 申请一条路径的写权。
    ///
    /// - 空着 ⇒ 给（`new_instance` 用来造 id）
    /// - **同一个持有者**再来 ⇒ 幂等，返回原来那个实例
    /// - 别人占着 ⇒ `Busy`，**立刻返回，不排队**
    pub fn acquire(
        &mut self,
        path: &Path,
        holder: &Holder,
        now_ms: i64,
        new_instance: impl FnOnce() -> String,
    ) -> Acquire {
        match self.inner.get(path) {
            Some(entry) if &entry.holder == holder => Acquire::Granted {
                instance: entry.instance.clone(),
                new: false,
            },
            Some(entry) => Acquire::Busy {
                instance: entry.instance.clone(),
                holder: entry.holder.clone(),
                held_ms: now_ms - entry.since_ms,
            },
            None => {
                let instance = new_instance();
                self.inner.insert(
                    path.to_path_buf(),
                    LockEntry {
                        instance: instance.clone(),
                        holder: holder.clone(),
                        since_ms: now_ms,
                    },
                );
                Acquire::Granted {
                    instance,
                    new: true,
                }
            }
        }
    }

    pub fn get(&self, path: &Path) -> Option<&LockEntry> {
        self.inner.get(path)
    }

    /// 释放。**只有持有者能释放** —— 别人拿着实例 id 也放不了。
    pub fn release(&mut self, path: &Path, holder: &Holder) -> bool {
        match self.inner.get(path) {
            Some(entry) if &entry.holder == holder => {
                self.inner.remove(path);
                true
            }
            _ => false,
        }
    }

    pub fn entries(&self) -> impl Iterator<Item = (&PathBuf, &LockEntry)> {
        self.inner.iter()
    }

    pub fn len(&self) -> usize {
        self.inner.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn holder(sid: &str, agent: &str) -> Holder {
        Holder {
            sid: sid.into(),
            agent: agent.into(),
        }
    }

    #[test]
    fn second_holder_is_refused_immediately_with_who() {
        let mut t = LockTable::default();
        let p = Path::new("a.txt");
        let a = holder("s-1", "console");
        let b = holder("s-1", "agent:dada");

        assert!(matches!(
            t.acquire(p, &a, 100, || "ed-1".into()),
            Acquire::Granted { new: true, .. }
        ));
        match t.acquire(p, &b, 250, || "ed-2".into()) {
            Acquire::Busy {
                holder: h,
                held_ms,
                instance,
            } => {
                assert_eq!(h.agent, "console", "要能说清是谁挡着");
                assert_eq!(held_ms, 150);
                assert_eq!(instance, "ed-1");
            }
            other => panic!("应当 busy，却得到 {other:?}"),
        }
    }

    #[test]
    fn same_holder_is_idempotent() {
        let mut t = LockTable::default();
        let p = Path::new("a.txt");
        let a = holder("s-1", "console");
        let first = t.acquire(p, &a, 100, || "ed-1".into());
        let second = t.acquire(p, &a, 200, || "ed-2".into());
        assert!(
            matches!(first, Acquire::Granted { new: true, .. }),
            "第一次是新开的"
        );
        match second {
            Acquire::Granted { instance, new } => {
                assert_eq!(instance, "ed-1", "必须给同一个实例，否则它会把自己锁死");
                assert!(!new, "第二次不是新建的");
            }
            other => panic!("应当 granted，却得到 {other:?}"),
        }
        assert_eq!(t.len(), 1);
    }

    #[test]
    fn release_frees_it_for_the_next_holder() {
        let mut t = LockTable::default();
        let p = Path::new("a.txt");
        let a = holder("s-1", "console");
        let b = holder("s-1", "agent:dada");
        t.acquire(p, &a, 0, || "ed-1".into());
        assert!(t.release(p, &a));
        assert!(matches!(
            t.acquire(p, &b, 0, || "ed-2".into()),
            Acquire::Granted { new: true, .. }
        ));
    }

    #[test]
    fn only_the_holder_can_release() {
        let mut t = LockTable::default();
        let p = Path::new("a.txt");
        t.acquire(p, &holder("s-1", "console"), 0, || "ed-1".into());
        assert!(!t.release(p, &holder("s-1", "agent:dada")));
        assert_eq!(t.len(), 1, "别人放不掉我的锁");
    }

    /// 不同会话的同名调用方也是不同持有者 —— 身份是 (sid, agent) 对。
    #[test]
    fn holder_identity_includes_the_session() {
        let mut t = LockTable::default();
        let p = Path::new("a.txt");
        t.acquire(p, &holder("s-1", "console"), 0, || "ed-1".into());
        assert!(matches!(
            t.acquire(p, &holder("s-2", "console"), 0, || "ed-2".into()),
            Acquire::Busy { .. }
        ));
    }
}
