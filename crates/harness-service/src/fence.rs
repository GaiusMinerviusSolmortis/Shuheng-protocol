//! **路径围栏**（04-安全 §7.1 的"托管 = 一条路径围栏"）。
//!
//! 规则只有一条：**读写都必须在托管路径内**，不分读写（§7.1 已定）。
//!
//! 难的地方全在"怎么算在内"：
//!
//! | 坑 | 处理 |
//! |---|---|
//! | `..` 绕出去 | 先规范化，再按**路径分量**比 |
//! | 符号链接 / junction 指到外面 | 规范化解决掉链接 |
//! | Windows 大小写 | 规范化拿到的是真实大小写，比较因此天然一致 |
//! | `C:\a\bc` 假装在 `C:\a\b` 里面 | `starts_with` 按分量比，不会中招 |
//! | 文件还不存在 | 规范化它的**父目录**，再拼文件名 |

use std::path::{Component, Path, PathBuf};

/// 按字面消掉 `.` 和 `..`。**不碰文件系统**，所以对不存在的路径也有效。
pub fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                // 已经在根上就 pop 不动 —— 那就什么也不做（不要退化成相对路径）
                if !out.pop() {
                    out.push(c.as_os_str());
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[derive(Debug, Clone, PartialEq)]
pub struct FenceError {
    pub why: String,
    pub remedy: String,
}

/// 一组托管根。第一个是**主路径** —— 相对路径按它解析。
#[derive(Debug, Clone, Default)]
pub struct Fence {
    roots: Vec<PathBuf>,
}

impl Fence {
    /// 建围栏。根必须已经存在（不存在就规范不了，也就没法可靠地比）。
    pub fn new(raw: &[String]) -> Result<Self, FenceError> {
        let mut roots = Vec::new();
        for r in raw {
            let p = Path::new(r);
            let canon = std::fs::canonicalize(p).map_err(|e| FenceError {
                why: format!("托管路径 `{}` 用不了：{e}", p.display()),
                remedy: "托管路径必须是一个已经存在的目录".to_string(),
            })?;
            if !canon.is_dir() {
                return Err(FenceError {
                    why: format!("托管路径 `{}` 不是一个目录", canon.display()),
                    remedy: "目前只支持托管目录".to_string(),
                });
            }
            roots.push(canon);
        }
        Ok(Self { roots })
    }

    pub fn is_empty(&self) -> bool {
        self.roots.is_empty()
    }

    pub fn roots(&self) -> &[PathBuf] {
        &self.roots
    }

    /// 把一个（可能是相对的）路径解析成**规范化绝对路径**，并断言它在围栏内。
    pub fn resolve(&self, raw: &str) -> Result<PathBuf, FenceError> {
        let base = self.roots.first().cloned().unwrap_or_default();
        self.resolve_from(&base, raw)
    }

    /// 同上，但相对路径按 `base`（例如终端的当前目录）解析。
    ///
    /// **两道闸门，顺序有讲究：**
    ///
    /// 1. **词法闸门** —— 先把 `..` 按字面消掉再判。它只用来**否决**，
    ///    永远不用来放行。作用是：`../../../../Windows` 这种明显在外面的路径，
    ///    即使它根本不存在，也要报"不在托管路径内"而不是"路径解析不了" ——
    ///    **围栏判断不该依赖"那个东西存不存在"**。
    /// 2. **真实闸门** —— `canonicalize` 解掉符号链接 / junction 之后再判一次。
    ///    这一道才能抓住"名字在里面、链接指到外面"。
    pub fn resolve_from(&self, base: &Path, raw: &str) -> Result<PathBuf, FenceError> {
        if self.roots.is_empty() {
            return Err(FenceError {
                why: "这个实例没有托管路径，所以它不能碰任何文件".into(),
                remedy: "中控拉起它时要给出托管目录（会话里托管的那条路径）".to_string(),
            });
        }
        if raw.trim().is_empty() {
            return Err(FenceError {
                why: "path 是空的".into(),
                remedy: "填一个文件路径；相对路径按第一个托管根解析".to_string(),
            });
        }

        let given = Path::new(raw);
        let joined = if given.is_absolute() {
            given.to_path_buf()
        } else {
            base.join(given)
        };

        // 第一道：词法。只否决，不放行。
        let lexical = lexical_normalize(&joined);
        if !self.contains_lexical(&lexical) {
            return Err(self.outside(&lexical));
        }

        // 文件可能还不存在 —— 那就规范化它的父目录，再拼文件名。
        let normalized = match std::fs::canonicalize(&joined) {
            Ok(p) => p,
            Err(_) => {
                let parent = joined.parent().ok_or_else(|| FenceError {
                    why: format!("`{raw}` 没有父目录"),
                    remedy: "给一个完整的文件路径".to_string(),
                })?;
                let name = joined.file_name().ok_or_else(|| FenceError {
                    why: format!("`{raw}` 不是一个文件路径"),
                    remedy: "路径要以文件名结尾，不能是 `..` 或目录本身".to_string(),
                })?;
                let canon_parent = std::fs::canonicalize(parent).map_err(|e| FenceError {
                    why: format!("`{}` 的父目录解析不了：{e}", joined.display()),
                    remedy: "路径要在托管范围内，而且父目录要真实存在".to_string(),
                })?;
                canon_parent.join(name)
            }
        };

        // 第二道：真实（符号链接 / junction 在这里现形）
        if !self.contains(&normalized) {
            return Err(self.outside(&normalized));
        }
        Ok(normalized)
    }

    fn outside(&self, p: &Path) -> FenceError {
        FenceError {
            why: format!("`{}` 不在托管路径内", p.display()),
            remedy: format!(
                "托管根是：{}；要改别处，先在会话里托管那条路径",
                self.roots_display()
            ),
        }
    }

    /// 相对路径按**主路径**解析成绝对路径（不做存在性检查）。
    pub fn anchor(&self, raw: &str) -> PathBuf {
        let given = Path::new(raw);
        if given.is_absolute() {
            given.to_path_buf()
        } else if let Some(root) = self.roots.first() {
            root.join(given)
        } else {
            given.to_path_buf()
        }
    }

    /// 这条**已经规范化过**的绝对路径在不在围栏内。
    pub fn contains(&self, normalized: &Path) -> bool {
        self.roots.iter().any(|root| normalized.starts_with(root))
    }

    /// 词法比较用的"可比形式"：小写、统一分隔符、去掉 Windows 的 `\\?\` 前缀。
    ///
    /// 为什么需要它：`canonicalize` 给出来的是 **verbatim** 形式（`\\?\C:\...`），
    /// 而调用方手写的绝对路径是 `C:\...` —— 两者 `starts_with` 永远不相等。
    /// 词法闸门只用来**提前否决**，所以这里只需要"不误伤"。
    fn cmp_key(p: &Path) -> String {
        let s = p.to_string_lossy().to_lowercase().replace('\\', "/");
        let s = s.strip_prefix("//?/").unwrap_or(&s).to_string();
        s.strip_suffix('/').map(str::to_string).unwrap_or(s)
    }

    /// 词法上在不在围栏内（**不碰文件系统**）。只用来提前否决。
    pub fn contains_lexical(&self, p: &Path) -> bool {
        let key = Self::cmp_key(p);
        self.roots.iter().any(|r| {
            let root = Self::cmp_key(r);
            key == root || key.starts_with(&format!("{root}/"))
        })
    }

    /// 给模型看的托管根清单。
    pub fn roots_display(&self) -> String {
        self.roots
            .iter()
            .map(|r| r.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    struct Temp(PathBuf);
    impl Temp {
        fn new(tag: &str) -> Self {
            let p = std::env::temp_dir()
                .join(format!("harness-fence-{tag}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&p);
            fs::create_dir_all(&p).unwrap();
            Temp(p)
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn inside_is_allowed_and_relative_resolves_to_primary_root() {
        let t = Temp::new("inside");
        fs::write(t.0.join("a.txt"), "hi").unwrap();
        let f = Fence::new(&[t.0.display().to_string()]).unwrap();

        let abs = f.resolve(&t.0.join("a.txt").display().to_string()).unwrap();
        let rel = f.resolve("a.txt").unwrap();
        assert_eq!(abs, rel, "相对路径要按主路径解析");
    }

    #[test]
    fn dotdot_cannot_escape() {
        let t = Temp::new("dotdot");
        let f = Fence::new(&[t.0.display().to_string()]).unwrap();
        let escaped = t.0.join("..").join("outside.txt");
        let err = f.resolve(&escaped.display().to_string()).unwrap_err();
        assert!(err.why.contains("不在托管路径内"), "{}", err.why);
    }

    #[test]
    fn absolutely_outside_is_refused() {
        let t = Temp::new("absout");
        let f = Fence::new(&[t.0.display().to_string()]).unwrap();
        let outside = std::env::temp_dir().join("harness-fence-outside.txt");
        let err = f.resolve(&outside.display().to_string()).unwrap_err();
        assert!(err.why.contains("不在托管路径内"), "{}", err.why);
    }

    /// 经典陷阱：`...\a\bc` 的字符串前缀是 `...\a\b`，但它在分量上不在里面。
    #[test]
    fn sibling_with_common_prefix_is_refused() {
        let t = Temp::new("prefix");
        let inside = t.0.join("b");
        let sibling = t.0.join("bc");
        fs::create_dir_all(&inside).unwrap();
        fs::create_dir_all(&sibling).unwrap();
        let f = Fence::new(&[inside.display().to_string()]).unwrap();
        let err = f
            .resolve(&sibling.join("x.txt").display().to_string())
            .unwrap_err();
        assert!(err.why.contains("不在托管路径内"), "{}", err.why);
    }

    #[test]
    fn no_fence_refuses_everything() {
        let f = Fence::new(&[]).unwrap();
        assert!(f.is_empty());
        assert!(f.resolve("a.txt").is_err());
    }

    #[test]
    fn nonexistent_root_is_refused_at_construction() {
        let err = Fence::new(&["D:/definitely/not/here/at/all".into()]).unwrap_err();
        assert!(err.remedy.contains("已经存在"));
    }

    /// 新文件（还不存在）也要能过 —— 否则"新建文件"这条最常用的路就断了。
    #[test]
    fn not_yet_existing_file_inside_fence_is_allowed() {
        let t = Temp::new("newfile");
        let f = Fence::new(&[t.0.display().to_string()]).unwrap();
        let p = f.resolve("brand-new.txt").unwrap();
        assert!(p.starts_with(&f.roots()[0]));
        assert!(p.ends_with("brand-new.txt"));
    }

    /// `anchor` 给 cd 用：它按**主路径**解相对路径，不做存在性检查。
    #[test]
    fn anchor_resolves_relative_against_the_primary_root() {
        let t = Temp::new("anchor");
        fs::create_dir_all(t.0.join("sub")).unwrap();
        let f = Fence::new(&[t.0.display().to_string()]).unwrap();
        assert_eq!(f.anchor("sub"), f.roots()[0].join("sub"));
    }

    /// **围栏判断不该依赖"那个东西存不存在"。**
    /// `../../../nope/x.txt` 一眼就是在外面 —— 哪怕它压根不存在，
    /// 也要报"不在托管路径内"，而不是"路径解析不了"。
    #[test]
    fn lexically_outside_is_refused_even_if_it_does_not_exist() {
        let t = Temp::new("lexical");
        let f = Fence::new(&[t.0.display().to_string()]).unwrap();
        let err = f
            .resolve(&t.0.join("../../../nope/x.txt").display().to_string())
            .unwrap_err();
        assert!(err.why.contains("不在托管路径内"), "{}", err.why);
    }

    #[test]
    fn lexical_normalize_folds_dot_and_dotdot() {
        assert_eq!(lexical_normalize(Path::new("a/./b/../c")), PathBuf::from("a/c"));
        assert_eq!(lexical_normalize(Path::new("../a")), PathBuf::from("../a"));
    }
}
