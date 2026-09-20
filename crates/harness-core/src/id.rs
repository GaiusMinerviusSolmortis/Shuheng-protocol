//! 命名规则。
//!
//! - 消息 id：`m-` 前缀（规范 §1，**已定**）
//! - 实例 id：`<kind>:<name>`（规范-消息协议 §8 #5：**倾向**加前缀，便于人读日志。
//!   这里先实现，因为它只影响可读性，改起来便宜）

/// 拆出实例 id 的类别前缀：`"echo:e-1"` → `Some(("echo", "e-1"))`。
pub fn split_instance(id: &str) -> Option<(&str, &str)> {
    let (kind, name) = id.split_once(':')?;
    if kind.is_empty() || name.is_empty() {
        return None;
    }
    Some((kind, name))
}

pub fn kind_of(id: &str) -> &str {
    split_instance(id).map(|(k, _)| k).unwrap_or(id)
}

/// **短随机段**（8 位十六进制，32 位）。
///
/// 为什么 id 里需要它：id 不只是给人读的名字 —— 它**参与鉴权**
/// （绑定表写的是实例 id、归属写的是 `谁@会话`、路由按 id 找收件人）。
/// 而"靠计数器保证唯一"只在**一个进程内**成立：两个会话同时开、
/// 或者两条 spawn 撞在一起，计数一样就撞 id，撞了之后注册表里
/// 后来者会**把先来者顶掉**（`BTreeMap::insert` 的默认行为）。
///
/// 所以名字要**唯一到跨进程**，而唯一性只能靠"时间 + 随机"。
/// 这里借 `RandomState` 的进程级随机种子，不引入新依赖。
///
/// ⚠️ **2026-09-20 从 4 位加长到 8 位**，起因是一个**偶发失败**的测试：
/// 它断言"一批 64 个里绝不重复"—— 而 16 位随机做不到这一点
/// （64 个取样，碰撞概率约 **3%**；那个测试就这么红了一次）。
///
/// 查明之后发现要修的不是那条测试，**是这里**：这个随机段在
/// **跨会话的身份索引**（`instances.jsonl`，只追加、要回答"这个 id 是谁"）
/// 里承担唯一性 —— 而 16 位在 **300 个 id** 的规模上碰撞概率已经接近一半。
/// 32 位把它压到 **约一万分之一**（同样 300 个 id），代价只是名字长 4 个字符。
///
/// 一句话记下这次的教训：**"一个偶发的红"最值得先问的不是"怎么让它稳"，
/// 而是"它想保的那个性质，现在的实现真的保得住吗"。**
pub fn short_rand() -> String {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let mut h = RandomState::new().build_hasher();
    h.write_u64(nanos);
    // 再掺一点"这个栈地址" —— 同一纳秒内的两次调用也会分开
    let here = &nanos as *const u64 as usize;
    h.write_usize(here);
    format!("{:08x}", (h.finish() & 0xffff_ffff) as u32)
}

/// 单调计数器。**同一个会话内不重复**就够 —— id 的作用是去重和引用，不是排序。
#[derive(Debug, Default, Clone)]
pub struct IdGen {
    counter: u64,
    tag: String,
}

impl IdGen {
    pub fn new(tag: impl Into<String>) -> Self {
        Self {
            counter: 0,
            tag: tag.into(),
        }
    }

    pub fn next_message(&mut self) -> String {
        self.counter += 1;
        format!("m-{}-{}", self.tag, self.counter)
    }

    pub fn next_corr(&mut self) -> String {
        self.counter += 1;
        format!("c-{}-{}", self.tag, self.counter)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_prefixed_instance_ids() {
        assert_eq!(split_instance("echo:e-1"), Some(("echo", "e-1")));
        assert_eq!(split_instance("ctrl"), None);
        assert_eq!(kind_of("ctrl"), "ctrl");
        assert_eq!(kind_of("editor:7f3a"), "editor");
    }

    #[test]
    fn ids_are_unique_and_prefixed() {
        let mut g = IdGen::new("ctrl");
        assert_eq!(g.next_message(), "m-ctrl-1");
        assert_eq!(g.next_corr(), "c-ctrl-2");
        assert!(g.next_message().starts_with("m-"));
    }

    /// 随机段要**真的分开**：同一毫秒内连着要 64 个，不该有重复。
    /// （它不承担保密职责 —— 只承担"撞不上"，见 `short_rand` 的注释。）
    ///
    /// ⚠️ 断言的是**"随机性没坏"**，不是"绝不重复"：32 位下 64 个取样里
    /// 撞一次的期望是 4.8e-7，所以允许至多一次 —— 真要撞两次，
    /// 那不是运气差，是随机源坏了（或者有人把它改回了短的那一版）。
    #[test]
    fn short_rand_does_not_repeat_in_a_burst() {
        let mut seen = std::collections::HashSet::new();
        for _ in 0..64 {
            seen.insert(short_rand());
        }
        assert!(
            seen.len() >= 63,
            "64 个取样里出现了 {} 次碰撞 —— 随机源坏了，或者它被改短了",
            64 - seen.len()
        );
        assert_eq!(short_rand().len(), 8);
    }
}
