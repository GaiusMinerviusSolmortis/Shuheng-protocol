//! **协议版本**：声明 + 兼容规则（规范-消息协议 §9）。
//!
//! 为什么要有这个模块，而不是各写各的版本号 —— 2026-09-27 之前，
//! 仓库里有**三处**"版本闸门"，而且三处都是**等号**：
//!
//! | 闸门 | 在哪 | 拿什么比 | 以前 |
//! |---|---|---|---|
//! | 传输 | `harness-ipc` 的 `Rendezvous::alive` | 会合文件里的 `protocol` | `== PROTOCOL` |
//! | 契约 | `harness-service` 的 `check_manifest` | manifest 的 `contract_version` | `== CONTRACT_VERSION` |
//! | 规范抬头 | 两份规范 | 人读的那句话 | 谁也没查过 |
//!
//! **等号就是"没有兼容处理"** —— 它把"加一个可选字段"这种本该系统性地兼容的改动，
//! 变成"所有老服务当场被拒"。那不是在严格，那是在**把能兼容的改动逼成不能兼容的**。
//!
//! 口径只有一条：**看 major**。
//!
//! | 对面声明的 | 我（读者）怎么办 |
//! |---|---|
//! | 同 major，minor ≤ 我的 | [`Compat::Same`]：一模一样地读 |
//! | 同 major，minor > 我的 | [`Compat::Extensions`]：**收下**，但它新加的那些我没见过的字段**当没有** |
//! | major 比我的大 | [`Compat::TooNew`]：**不猜，按名字拒** |
//! | major 比我的小 | [`Compat::TooOld`]：同样拒（不认识的形状，硬解释出来的错会指向错的地方）|
//!
//! **"当没有"是有代价的，得写清楚**：一个服务要是**需要**中控照着它新加的字段做事，
//! 就不能只抬 minor —— 它得抬 major，让不认识它的中控**按名字把它拒掉**。
//! 只抬 minor 等于在说"我这些追加字段你可以无视，我照样能干活"。

/// **内核协议的那一版。** 唯一真相 —— 传输的自报标签、manifest 的
/// `contract_version`、判词里那个"我这一版是几"，全从它读。
///
/// 什么时候动它（口径同 `规范-工具服务` §11.6）：
///
/// | 改动 | 动哪个 |
/// |---|---|
/// | 加**可选**字段 / 往闭合枚举里加值 | **minor** +1（老读者照样收，只是当没有）|
/// | 加**必报**字段 / 删值 / 改语义 | **major** +1（老读者按名字拒）|
///
/// ⚠️ 有**一处已经发生的例外**要记在这儿，否则这条规矩就先烂了：`sandbox` 那五个面
/// 是 2026-09-20 **加进 `1` 的必报字段** —— 按上表本该动 major。之所以没动：
/// **那时还没有第三方服务**（六个服务全在这个仓库里，一起改完的）。
/// 那是唯一一次可以这么干的情形；下一次加必报字段就得动 major。
pub const PROTOCOL_VERSION: &str = "1.0";

/// 一个 `major.minor` 版本。
///
/// **`"1"` 和 `"1.0"` 是同一版**（缺 minor 就是 0）—— 这不是为了宽容，
/// 是为了让已经写在六个服务里的那句 `"contract_version": "1"` 继续算数，
/// 而不是逼所有人改一遍字面量。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Version {
    pub major: u32,
    pub minor: u32,
}

impl Version {
    /// 认 `"1"` / `"1.0"` / `" 2.5 "`。**不认**的东西一律给 `None` ——
    /// `"1."`、`"1.0.2"`、`"v1"`、`"+1"` 全是 `None`。
    ///
    /// 为什么这么严：这个函数的返回值决定"收下还是拒"。
    /// 一个宽松的解析器会把 `"1.0.2"` 读成 `1.0` 然后**收下一个自己没把握的东西** ——
    /// 那和"不认识的版本硬解释"是同一个错，只是发生得更早。
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim();
        let (major, minor) = match s.split_once('.') {
            Some((a, b)) => (a, b),
            None => (s, "0"),
        };
        Some(Self {
            major: digits(major)?,
            minor: digits(minor)?,
        })
    }

    /// **我这一版（`self`）碰上对面声明的那一版（`theirs`），怎么办？**
    ///
    /// 方向要说清：`self` 是**读者**（中控 / 拿到会合文件的那一端），
    /// `theirs` 是**写者**（服务自报的 manifest、会合文件上写的标签）。
    pub fn speak(&self, theirs: Self) -> Compat {
        if theirs.major == self.major {
            if theirs.minor > self.minor {
                Compat::Extensions
            } else {
                Compat::Same
            }
        } else if theirs.major > self.major {
            Compat::TooNew
        } else {
            Compat::TooOld
        }
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}", self.major, self.minor)
    }
}

/// [`Version::speak`] 的四种结果。**闭合枚举** —— 出现第五个值就是契约破了。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compat {
    /// 同一个 major，而且对面没比我新：**一模一样地读**。
    Same,
    /// 同一个 major，但对面更新：**收下**，只是它新加的那些字段我当没有。
    Extensions,
    /// 对面比我新（major 更大）—— **不猜，按名字拒**。
    TooNew,
    /// 对面比我老（major 更小）—— 同样拒。
    TooOld,
}

impl Compat {
    /// 能不能继续谈下去。`TooNew` / `TooOld` 是**唯一**该拦下的两种。
    pub fn ok(&self) -> bool {
        matches!(self, Compat::Same | Compat::Extensions)
    }

    /// 拦下来时**给的判词**（规范里那条"不许硬解释"就落在这句话上）。
    ///
    /// 为什么判词写在这儿、而不是各闸门各写一句：这正是本项目栽过很多次的地方 ——
    /// **同一件事的两句话一定会漂**，而漂出来的两句还会互相打架
    /// （一个说"你少了个字段"，一个说"版本对不上"）。
    /// 说"太新"和说"太老"是**两句**：它们要修的地方不一样，
    /// 一句搪塞过去就等于让人自己猜。
    pub fn why(&self, mine: Version, theirs: Version) -> Option<String> {
        let head = format!("`{theirs}` 和我这一版 `{mine}` 的 **major 对不上**");
        match self {
            Compat::Same | Compat::Extensions => None,
            Compat::TooNew => Some(format!(
                "{head}：对面在实现更新的协议，那个形状我不认识。\
                 这**不是「它写错了」**，是**我们对不上** —— 别按旧版本硬解释它，\
                 那样报出来的错会指向「你少了个字段」，而真正的原因（版本对不上）\
                 谁也看不见。要么把中控升上来，要么让这个服务按 `{mine}` 那一档说话"
            )),
            Compat::TooOld => Some(format!(
                "{head}：对面比我这一版**老**，那个形状我这一版已经不认了。\
                 同一条纪律：不认识的版本不许照旧解析。\
                 要么把对面升到 `{mine}` 那一档，要么让中控留一份读得懂 `{theirs}` 的读法"
            )),
        }
    }

    /// 收下、但要说一声时给的那句话（`Extensions` 才有）。
    ///
    /// 为什么**收下也要出声**：它带来的字段我一个都不认识，将来出了怪事，
    /// "原来是它带了扩展"这件事必须能**从日志里查出来**，而不是靠谁记得。
    pub fn note(&self, mine: Version, theirs: Version) -> Option<String> {
        match self {
            Compat::Extensions => Some(format!(
                "它声明的协议是 `{theirs}`，我这一版是 `{mine}` —— **同一个 major，收下**。\
                 但它新加的那些字段我当没有（我没见过，不等于它不存在）。\
                 要是它**需要**我照着那些字段做事，就该把 major 抬上去，\
                 让不认识它的中控**按名字拒掉**，而不是指望我猜"
            )),
            _ => None,
        }
    }
}

/// `PROTOCOL_VERSION` 那一版的解析值。**解析不了就 panic** ——
/// 它是这个模块自己的常量，解析不了是代码写错了，不是运行时输入的问题。
pub fn protocol_version() -> Version {
    Version::parse(PROTOCOL_VERSION)
        .expect("`PROTOCOL_VERSION` 自己得能解析 —— 它就在这个文件里")
}

fn digits(s: &str) -> Option<u32> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bare_major_is_the_same_version_as_dot_zero() {
        // 这一条是**兼容性本身**：六个服务的 manifest 里写的是 `"1"`，
        // 而常量写的是 `"1.0"` —— 它们必须被判成同一版，否则闸门会把
        // 自己仓库里的服务全拒掉。
        assert_eq!(Version::parse("1"), Version::parse("1.0"));
        assert_eq!(Version::parse("1"), Some(protocol_version()));
    }

    #[test]
    fn only_major_minor_digits_are_accepted() {
        for bad in ["", " ", "1.", ".1", "1.0.2", "v1", "+1", "-1", "1.-1", "one.two"] {
            assert_eq!(Version::parse(bad), None, "`{bad}` 不该被认");
        }
        assert_eq!(Version::parse(" 2.5 "), Some(Version { major: 2, minor: 5 }));
    }

    #[test]
    fn compatibility_is_decided_by_major_alone() {
        let mine = protocol_version();
        assert_eq!(mine.speak(Version::parse("1.0").unwrap()), Compat::Same);
        assert_eq!(mine.speak(Version::parse("1.9").unwrap()), Compat::Extensions);
        assert_eq!(mine.speak(Version::parse("0.9").unwrap()), Compat::TooOld);
        assert_eq!(mine.speak(Version::parse("2.0").unwrap()), Compat::TooNew);
    }

    #[test]
    fn too_new_and_too_old_do_not_share_a_sentence() {
        // 这两档要修的地方不一样（一个要升中控，一个要升服务），
        // 用同一句话搪塞过去，等于让人自己猜。
        let mine = protocol_version();
        let new = mine.speak(Version::parse("2.0").unwrap());
        let old = mine.speak(Version::parse("0.9").unwrap());
        let a = new.why(mine, Version::parse("2.0").unwrap()).unwrap();
        let b = old.why(mine, Version::parse("0.9").unwrap()).unwrap();
        assert_ne!(a, b);
        assert!(b.contains("老"), "{b}");
        assert!(!a.contains("老"), "{a}");
        // 两句都要带上**两个**版本号：只有"我不认识"而没有"我认识的是几"，
        // 对面还是不知道该往哪边改。
        assert!(a.contains("2.0") && a.contains(&mine.to_string()), "{a}");
        assert!(b.contains("0.9") && b.contains(&mine.to_string()), "{b}");
    }

    #[test]
    fn the_two_ok_kinds_are_the_only_ones_that_pass() {
        assert!(Compat::Same.ok() && Compat::Extensions.ok());
        assert!(!Compat::TooNew.ok() && !Compat::TooOld.ok());
        // 收下的时候不出判词，但"同 major 更小/相等"也不该出声 ——
        // 出声的东西只有一样：**它带了我没见过的字段**。
        let mine = protocol_version();
        assert!(Compat::Same.note(mine, mine).is_none());
        assert!(Compat::TooNew.note(mine, mine).is_none());
        assert!(Compat::Extensions
            .note(mine, Version::parse("1.9").unwrap())
            .unwrap()
            .contains("1.9"));
    }
}
