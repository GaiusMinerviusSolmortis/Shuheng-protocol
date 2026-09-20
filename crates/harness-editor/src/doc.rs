//! 文档实例：内容快照 + 回滚（02-工具 §5.24 的 **L1 实例级 undo**）。
//!
//! 语义定死在这里，因为它是"写即落盘"能安全使用的前提：
//!
//! - `revision` 是**内容的版本号**，从 0（刚打开时）开始，每次 `edit` +1；
//! - `edit` 返回的是**这次编辑之前**的版本号 —— 拿它就能退回去；
//! - `revert` 把内容写回某个 revision，并**丢掉它之后的历史**（经典 undo，不做 redo）。
//!
//! L1 只依赖自己，**不要 git**（§5.24：托管路径未必是仓库，
//! 而且 git 回滚不了未跟踪文件）。

use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// **磁盘上那份文件的"指纹"**（大小 + 修改时间）。
///
/// 为什么要它：实例持有的是**内存里的快照**，而写盘是"整份覆盖"。
/// 要是中间有别人（用户的 IDE、命令行、别的工具）改了同一个文件，
/// 我们这一写就会**把它的改动静默抹掉** —— 不是报错，是悄悄丢数据。
///
/// §5.15 早就警告过：写锁**只挡得住我们自己的实例之间**，
/// 挡不住用户的编辑器。这条指纹就是给那种情况准备的。
///
/// 诚实地说清它的**局限**：这是"尽力而为"的检查 ——
/// 同一毫秒内的改动可能看不出来（mtime 的粒度），
/// 它防的是**人的手滑**，不是防对手。
#[derive(Debug, Clone, PartialEq)]
pub struct DiskStamp {
    pub len: u64,
    pub mtime: Option<SystemTime>,
}

impl DiskStamp {
    pub fn of(path: &Path) -> Option<Self> {
        let md = std::fs::metadata(path).ok()?;
        Some(Self {
            len: md.len(),
            mtime: md.modified().ok(),
        })
    }

    /// 说清楚"哪儿不一样了" —— 给人/模型看的一句话。
    fn explain(&self, other: &DiskStamp) -> String {
        if self.len != other.len {
            format!("大小 {} → {} 字节", self.len, other.len)
        } else {
            "修改时间变了（大小没变）".to_string()
        }
    }
}

#[derive(Debug)]
pub struct Document {
    pub id: String,
    pub path: PathBuf,
    /// `snapshots[n]` = revision n。`snapshots[0]` 是打开时那份。
    snapshots: Vec<Snap>,
    /// **上一次我们读写磁盘之后**那份文件的指纹（见 `DiskStamp`）。
    /// `None` = 记不下来（文件当时就不在，或者 stat 失败）—— 那就没有可比的东西，不检查。
    disk: Option<DiskStamp>,
}

/// 一个版本，以及**它是谁写的**。
///
/// `external = true` 的意思是"这一版不是我们写的，是从磁盘上拉回来的" ——
/// 也就是**用户（或别的程序）改的那一版**。分清这个很要紧：
/// `edit` 可以安全地落在它上面（靠 `old` 唯一命中），
/// 而 `revert` 一旦要跨过它，就是**故意丢掉用户那次改动**，得拦。
#[derive(Debug, Clone)]
struct Snap {
    text: String,
    external: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum EditError {
    /// `old` 命中了 0 处
    NotFound,
    /// `old` 命中了多处 —— 必须唯一，否则改哪一处是不确定的
    Ambiguous { hits: usize },
    /// revision 不认识
    UnknownRevision(u64),
}

impl Document {
    pub fn new(id: String, path: PathBuf, initial: String) -> Self {
        Self {
            id,
            path,
            disk: None,
            snapshots: vec![Snap {
                text: initial,
                external: false,
            }],
        }
    }

    /// 记下"我刚读过/写过磁盘，它现在长这样"。
    pub fn note_disk(&mut self) {
        self.disk = DiskStamp::of(&self.path);
    }

    /// **磁盘上那份还在吗、还是我们离开时那一份？**
    ///
    /// `Some(一句话)` = 变了（或被删了），那句话直接可以给人看。
    /// `None` = 没变，或者**我们本来就没有指纹可比**（那种情况不装懂）。
    pub fn changed_on_disk(&self) -> Option<String> {
        let recorded = self.disk.as_ref()?;
        match DiskStamp::of(&self.path) {
            None => Some(format!(
                "`{}` 在磁盘上已经**不在了**（被删掉或改了名）",
                self.path.display()
            )),
            Some(now) if &now != recorded => Some(format!(
                "`{}` 在磁盘上**被别人改过**（{}）",
                self.path.display(),
                recorded.explain(&now)
            )),
            Some(_) => None,
        }
    }

    /// **把磁盘上那份拉进来**（用户改过了）。
    ///
    /// 这是这一层最重要的一条策略，也是它一开始做错的地方：
    ///
    /// > **用户改文件不是异常，是常态 —— 实例要跟着磁盘走。**
    ///
    /// 写锁保护的是"**别的 AI 别大批量覆盖它**"，不是"用户别碰"。
    /// 所以发现磁盘变了，正确的动作不是拒绝，而是：
    ///
    /// 1. 把磁盘那份读进来，**作为一条新的 revision 追加**（标成 `external`）——
    ///    于是历史是连贯的，用户那次改动在账上也看得见；
    /// 2. `edit` 随后照常做它的替换 —— 安全性来自**内容**（`old` 必须唯一命中），
    ///    不来自时间戳：用户的改动要是碰的是别处，`old` 照样命中，两边**并存**；
    ///    要是碰的正是要改的那一处，`old` 命中不了，模型会收到"先读一眼"的常规拒绝。
    /// 3. `revert` 是**故意整份覆盖**，没有内容判据可依 —— 它另守一条规则：
    ///    **不许跨过用户的改动**（见 `revert_guard`）。
    ///
    /// 返回 `Some((新版本号, 一句话))` 表示这次真的同步了。
    pub fn sync_from_disk(&mut self) -> Option<(u64, String)> {
        let why = self.changed_on_disk()?;
        let text = match std::fs::read_to_string(&self.path) {
            Ok(t) => t,
            // 读不回来（被删了 / 不是 UTF-8）：**别假装同步成功** ——
            // 保持原样，让写盘前那道检查继续拦着。
            Err(_) => return None,
        };
        self.snapshots.push(Snap {
            text,
            external: true,
        });
        self.note_disk();
        Some((self.revision(), why))
    }

    /// `revert` 的那道额外关：**要退回的这一版之后，有没有用户的改动？**
    ///
    /// 有就拒 —— 因为 `revert` 是整份覆盖，跨过去就等于**悄悄把用户那次改动丢了**。
    /// 这不是"防手滑"那种尽力而为，而是可以精确判定的：`external` 那条快照就在历史里。
    pub fn revert_guard(&self, to: u64) -> Option<String> {
        let idx = to as usize;
        if idx >= self.snapshots.len() {
            return None; // 认不出的 revision 由 revert 自己报
        }
        self.snapshots
            .iter()
            .skip(idx + 1)
            .position(|s| s.external)
            .map(|off| {
                format!(
                    "revision {} 之后有**用户的改动**（revision {}）—— 退回去会把它丢掉",
                    to,
                    idx + 1 + off
                )
            })
    }

    /// 当前版本号。
    pub fn revision(&self) -> u64 {
        (self.snapshots.len() - 1) as u64
    }

    pub fn content(&self) -> &str {
        self.snapshots
            .last()
            .map(|s| s.text.as_str())
            .unwrap_or("")
    }

    pub fn history_len(&self) -> usize {
        self.snapshots.len()
    }

    /// 一次替换。**要求 `old` 恰好命中一次**（0 次或多于 1 次都拒绝）。
    ///
    /// 返回**这次编辑之前**的版本号 —— 它就是回滚的钥匙。
    ///
    /// **`old` 为空是特例**：只有当文件**本身是空的**才允许，含义是"把内容填进去"。
    /// 没有这条，"新建一个文件然后写它"就无路可走 —— 而那是编辑器最常见的用途之一
    /// （这条是跑阶段 2 的 e2e 时发现的）。
    pub fn edit(&mut self, old: &str, new: &str) -> Result<u64, EditError> {
        let hits = if old.is_empty() {
            if self.content().is_empty() {
                1
            } else {
                0
            }
        } else {
            self.content().matches(old).count()
        };
        match hits {
            0 => return Err(EditError::NotFound),
            1 => {}
            n => return Err(EditError::Ambiguous { hits: n }),
        }
        let before = self.revision();
        let updated = self.content().replacen(old, new, 1);
        self.snapshots.push(Snap {
            text: updated,
            external: false,
        });
        Ok(before)
    }

    /// 写回某个 revision，并丢掉它之后的历史。
    pub fn revert(&mut self, to: u64) -> Result<(), EditError> {
        let idx = to as usize;
        if idx >= self.snapshots.len() {
            return Err(EditError::UnknownRevision(to));
        }
        if idx == self.snapshots.len() - 1 {
            return Ok(()); // 已经在那个版本上了，幂等
        }
        let target = self.snapshots[idx].text.clone();
        self.snapshots.truncate(idx + 1);
        debug_assert_eq!(self.content(), target);
        Ok(())
    }

    /// **整份换成外面给的这一份**（不是替换某一段）。
    ///
    /// 和 `edit` 的区别正是安全性来源的区别：`edit` 靠**内容命中**（`old` 恰好一次）
    /// 保证不误伤；这条**没有那个判据**，所以它要窄得多 ——
    /// 现在只有一个调用者：L2 的 `restore`，而它进门之前走的是
    /// `sync_if_changed` + "跨过用户的改动就拒"那道守卫。
    ///
    /// 返回**换成这份之前**的 revision（就是 `revert` 的钥匙）。
    pub fn adopt(&mut self, text: String) -> u64 {
        let before = self.revision();
        self.snapshots.push(Snap {
            text,
            external: false,
        });
        before
    }

    pub fn bytes(&self) -> usize {
        self.content().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc() -> Document {
        Document::new("ed-1".into(), PathBuf::from("a.txt"), "alpha\nbeta\n".into())
    }

    /// 一个**真的落在磁盘上**的文档（同步那一套要真文件才测得动）。
    fn real_doc(text: &str) -> (Document, PathBuf) {
        let p = std::env::temp_dir().join(format!(
            "harness-doc-{}-{}.txt",
            std::process::id(),
            harness_core::id::short_rand()
        ));
        std::fs::write(&p, text).unwrap();
        let mut d = Document::new("ed-1".into(), p.clone(), text.to_string());
        d.note_disk();
        (d, p)
    }

    /// **用户改文件不是异常**：实例要把它拉进来，作为一条 `external` 的 revision。
    #[test]
    fn a_user_edit_is_pulled_in_as_a_revision() {
        let (mut d, p) = real_doc("alpha\n");
        assert!(d.sync_from_disk().is_none(), "没人动过就不该同步");
        std::thread::sleep(std::time::Duration::from_millis(15));
        std::fs::write(&p, "alpha\nuser-was-here\n").unwrap();

        let (rev, why) = d.sync_from_disk().expect("该发现用户改过");
        assert_eq!(rev, 1);
        assert!(why.contains("被别人改过"), "{why}");
        assert_eq!(d.content(), "alpha\nuser-was-here\n", "拉进来的就是磁盘那份");
        // 再同步一次不该重复追加（指纹已经对上了）
        assert!(d.sync_from_disk().is_none());
        let _ = std::fs::remove_file(&p);
    }

    /// **回滚不许跨过用户的改动**（它是整份覆盖，跨过去就是悄悄丢掉那次改动）。
    #[test]
    fn revert_refuses_to_step_over_a_user_edit() {
        let (mut d, p) = real_doc("alpha\n");
        d.edit("alpha", "ALPHA").unwrap(); // revision 1：我们改的
        std::thread::sleep(std::time::Duration::from_millis(15));
        std::fs::write(&p, "ALPHA\nuser\n").unwrap();
        d.sync_from_disk().unwrap(); // revision 2：用户改的
        d.edit("user", "us").unwrap(); // revision 3：我们又改的

        assert!(d.revert_guard(2).is_none(), "退到用户那一版之前没有更早的外部改动");
        let why = d.revert_guard(1).expect("退到它之前会跨过用户那一版");
        assert!(why.contains("用户的改动"), "{why}");
        assert!(d.revert_guard(0).is_some());
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn edit_requires_exactly_one_hit() {
        let mut d = doc();
        assert_eq!(d.edit("beta", "BETA").unwrap(), 0, "返回的是编辑之前的版本");
        assert_eq!(d.revision(), 1);
        assert_eq!(d.content(), "alpha\nBETA\n");

        assert_eq!(d.edit("nope", "x"), Err(EditError::NotFound));
        // "alpha\nBETA\n" 里小写 a 有两处（alpha 的首尾）；BETA 是大写，不算
        assert_eq!(d.edit("a", "x"), Err(EditError::Ambiguous { hits: 2 }));
    }

    #[test]
    fn revert_restores_and_drops_later_history() {
        let mut d = doc();
        let r0 = d.edit("alpha", "ALPHA").unwrap();
        let r1 = d.edit("beta", "BETA").unwrap();
        assert_eq!((r0, r1), (0, 1));
        assert_eq!(d.content(), "ALPHA\nBETA\n");

        d.revert(r0).unwrap();
        assert_eq!(d.content(), "alpha\nbeta\n", "要回到 revision 0 那份");
        assert_eq!(d.revision(), 0);
        assert_eq!(d.history_len(), 1, "revert 之后没有 redo");
    }

    #[test]
    fn revert_to_current_is_idempotent() {
        let mut d = doc();
        d.edit("alpha", "ALPHA").unwrap();
        d.revert(1).unwrap();
        assert_eq!(d.revision(), 1);
        assert_eq!(d.content(), "ALPHA\nbeta\n");
    }

    #[test]
    fn unknown_revision_is_refused() {
        let mut d = doc();
        assert_eq!(d.revert(7), Err(EditError::UnknownRevision(7)));
    }

    #[test]
    fn empty_old_is_refused_when_the_file_has_content() {
        let mut d = doc();
        assert_eq!(d.edit("", "x"), Err(EditError::NotFound));
    }

    /// 新建的空文件必须能写 —— 否则"开个新文件再写它"这条路是断的。
    #[test]
    fn empty_old_fills_an_empty_file() {
        let mut d = Document::new("ed-2".into(), PathBuf::from("new.txt"), String::new());
        assert_eq!(d.edit("", "hello\n").unwrap(), 0);
        assert_eq!(d.content(), "hello\n");
        assert_eq!(d.revision(), 1);
        // 已经有内容之后，空 old 就不再合法了
        assert_eq!(d.edit("", "x"), Err(EditError::NotFound));
    }
}
