//! **项目级 checkpoint**（02-工具 §5.24 的 L2）—— 只做 git 里**安全的那一半**。
//!
//! 你的原话（2026-09-19）：
//!
//! > **edit 如果直接落盘的话可能会出问题，我倾向于在编辑器中自动带一个 git 操作方便回滚。**
//!
//! 规范接着把风险说清了，这一版就照着那条约束做：
//!
//! > `git checkout` / `git reset` / `git stash` 会**覆盖用户在同一个仓库里自己未提交的改动**、
//! > 污染用户的索引、抢 `index.lock`。§5.15 的写锁只挡得住"我们自己的实例之间"，
//! > **挡不住用户的 IDE 和命令行**。
//!
//! 所以这里只做两件事，它们的共同点是**一个字都不动工作区和索引**：
//!
//! | 动作 | 命令 | 碰了什么 |
//! |---|---|---|
//! | 建 checkpoint | `git stash create` + `git update-ref` | 只**读**工作区/索引，写一个**私有 ref** |
//! | 看 checkpoint | `git for-each-ref` / `git show <ref>:<路径>` | 只读 |
//!
//! **恢复文件**那一步（要写工作区）不在这里 —— 它归编辑器那一层，
//! 走的是和 `revert` **同一条守卫**（"不许跨过用户的改动"，见 `doc.rs::revert_guard`）。
//! 而"整个项目退回某个起点"这一版**不做**：那一步会覆盖用户在同一个仓库里的
//! 全部改动，而**我们的写锁挡不住用户的 IDE**（写在 `not_implemented` 里）。
//!
//! 为什么 ref 要带会话号（`refs/agent/<sid>/<n>`）：两个会话各自建 checkpoint 时
//! 不该互相踩；而"谁建的"这件事本身要能从 ref 名字上看出来（审计）。

use std::path::Path;
use std::process::{Command, Stdio};

/// 一个 checkpoint。
///
/// `name` 是**完整的 ref 名**（`refs/agent/<sid>/<n>`）—— 不用短名：
/// 短名有两种写法（`agent/s-test/1` 与 `s-test/1`），而其中一种
/// 拿到别的 git 命令里就不认了。**只有一种写法就不会选错。**
#[derive(Debug, Clone, PartialEq)]
pub struct Checkpoint {
    pub name: String,
    pub hash: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum GitError {
    /// 这个目录不在任何 git 仓库里 —— L2 用不上，退回 L1（revision）
    NotARepo,
    /// 环境里没有可用的 `git`
    NoGit(String),
    /// 工作区没有改动 —— `git stash create` 什么都不回（这不是错误，是事实）
    NoChanges,
    /// **仓库还没有第一个提交** —— `git stash create` 需要 HEAD 有东西可比。
    ///
    /// 这不是边角情况：`git init` 之后、第一次 commit 之前就是它。
    NoCommitYet,
    /// 命令跑失败了（带上它的原话，别替它编）
    Failed { what: String, stderr: String },
}

impl GitError {
    /// 这一条该报哪个码（§11.5）：
    /// 仓库/环境的问题是"别试了"（`refused`，得用户去处理）；
    /// "现在没有改动可存"和"git 那一下失败了"是调用方能改的（`invalid`）。
    pub fn code(&self) -> &'static str {
        match self {
            // 这三样都得**用户**去处理：换个仓库 / 装 git / 先提交一次
            GitError::NotARepo | GitError::NoGit(_) | GitError::NoCommitYet => "refused",
            GitError::NoChanges | GitError::Failed { .. } => "invalid",
        }
    }
}

/// 跑一条 git 命令，**把它的输出收走**。
///
/// 为什么不用 `inherit`：这是**我们自己拉起来的子进程**，
/// 它的 stdout/stderr 绝不能碰到我们和 ctrl 之间那根协议管道。
/// `Command::output()` 给子进程一对全新的管道，正好。
fn run(dir: &Path, args: &[&str]) -> Result<String, GitError> {
    run_raw(dir, args).map(|b| String::from_utf8_lossy(&b).trim().to_string())
}

/// 和 [`run`] 一样，但**原样返回字节**。
///
/// 为什么必须有这个孪生：`run` 会把输出首尾裁掉 —— 那对 `rev-parse` 这种
/// **吐一个词**的命令是对的，而对 `git show <ref>:<文件>`（它吐的是**文件内容**）
/// 就是**在改数据**。第一版只有一个 `run`，于是恢复出来的文件**少了结尾换行**
/// （端到端判据当场读到 `bytes: 9` 而不是 10 —— Rust 那条单测没抓到，
/// 因为它的测试数据恰好没有首尾空白）。
fn run_raw(dir: &Path, args: &[&str]) -> Result<Vec<u8>, GitError> {
    let mut cmd = Command::new("git");
    cmd.current_dir(dir).args(args);
    // 别让 git 停下来等人输入凭据 —— 它不该有"等人"的时候
    cmd.env("GIT_TERMINAL_PROMPT", "0");
    // **stdin 给它空管道**（同 `Command::output()` 的语义）：git 不该读我们的 stdin，
    // 更不该碰到我们和 ctrl 之间那根协议管道。
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW：别闪一个黑框（这是个后台服务）
        cmd.creation_flags(0x0800_0000);
    }
    let child = cmd.spawn().map_err(|e| GitError::NoGit(e.to_string()))?;
    // ⚠️ **把它放进一个 Job**（2026-09-20 补的边界回收）：
    //
    // 在这之前，git 是**谁的子树都不在**的一个进程 —— 编辑器被收掉（`stop` /
    // 会话结束 / 硬杀）时我们只杀得掉编辑器自己，**那个 git 会留下来**：
    // 它可能正攥着 `.git/index.lock`，或者还在哈希一大堆改动。
    // 用户下一分钟在自己终端里 `git commit` 就会撞上"另一个 git 进程在运行"，
    // 而那个进程的父进程早就没了 —— 查这种幽灵锁比查服务崩了难得多。
    //
    // Job 带 `KILL_ON_JOB_CLOSE`：**这个 `Job` 一 drop（或我们进程一死），
    // 内核把整棵收掉**。限额不开 —— `Limits::default()`。
    // （`assign` 返回 `None` 只说明环境不允许，那就退回"只能杀一层"，不该在这儿报错。）
    let _job = harness_service::job::Job::assign(&child, harness_service::job::Limits::default());
    let out = child.wait_with_output().map_err(|e| GitError::Failed {
        what: format!("git {}", args.join(" ")),
        stderr: e.to_string(),
    })?;
    if !out.status.success() {
        return Err(GitError::Failed {
            what: format!("git {}", args.join(" ")),
            stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
        });
    }
    Ok(out.stdout)
}

/// 这个目录在不在 git 仓库里。
pub fn is_repo(dir: &Path) -> bool {
    run(dir, &["rev-parse", "--is-inside-work-tree"])
        .map(|s| s == "true")
        .unwrap_or(false)
}

/// **建一个 checkpoint。**
///
/// `git stash create` 是官方留给脚本用的那一个：它造一个 stash 提交并把哈希打出来，
/// **既不写 stash 列表、也不动工作区和索引**。然后我们把这个提交挂到一个**私有 ref**
/// 上（`refs/agent/<sid>/<n>`）—— 于是它不会被 gc 掉，
/// 也永远不会被 `git stash pop` / `git checkout` 之类碰到。
pub fn checkpoint(root: &Path, sid: &str, n: u32) -> Result<Checkpoint, GitError> {
    if !is_repo(root) {
        return Err(GitError::NotARepo);
    }
    let hash = match run(root, &["stash", "create", "-m", &format!("harness checkpoint {n}")]) {
        Ok(h) => h,
        // git 的这句原话是 "You do not have the initial commit yet" ——
        // 它值得**单独一个错误**：调用方改参数没用，得用户先做一次提交。
        Err(GitError::Failed { stderr, .. }) if stderr.contains("initial commit") => {
            return Err(GitError::NoCommitYet)
        }
        Err(e) => return Err(e),
    };
    if hash.is_empty() {
        return Err(GitError::NoChanges);
    }
    let name = format!("refs/agent/{sid}/{n}");
    // `-m` 让 reflog 上留一句话（不然它是一条"不知道为什么存在"的 ref）
    run(
        root,
        &["update-ref", "-m", "harness checkpoint", &name, &hash],
    )?;
    Ok(Checkpoint { name, hash })
}

/// 列出**本会话**建过的 checkpoint（别的会话的不列 —— 那是它们的账）。
pub fn list(root: &Path, sid: &str) -> Result<Vec<Checkpoint>, GitError> {
    if !is_repo(root) {
        return Err(GitError::NotARepo);
    }
    let out = run(
        root,
        &[
            "for-each-ref",
            // **完整 ref 名**，和 `checkpoint()` 报的是同一种写法（就一种写法）
            "--format=%(refname) %(objectname)",
            &format!("refs/agent/{sid}/"),
        ],
    )?;
    Ok(out
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            Some(Checkpoint {
                name: it.next()?.to_string(),
                hash: it.next()?.to_string(),
            })
        })
        .collect())
}

/// 某个路径在某个 checkpoint 时的内容。
///
/// `rel` 是**相对仓库根**的路径（git 只认这个形状）。
pub fn read_at(root: &Path, rev: &str, rel: &str) -> Result<String, GitError> {
    let bytes = run_raw(root, &["show", &format!("{rev}:{rel}")])?;
    String::from_utf8(bytes).map_err(|_| GitError::Failed {
        what: format!("git show {rev}:{rel}"),
        stderr: "那个版本里的这一份不是 UTF-8 文本 —— 编辑器只认文本".to_string(),
    })
}

/// 仓库根（`rev-parse --show-toplevel`）。
///
/// 托管路径可能只是仓库里的一个**子目录**，而 git 命令要在**仓库根**上跑、
/// 路径也要按**相对仓库根**给 —— 所以这两件事必须由同一个函数算出来。
pub fn toplevel(dir: &Path) -> Result<std::path::PathBuf, GitError> {
    let s = match run(dir, &["rev-parse", "--show-toplevel"]) {
        Ok(s) => s,
        // ⚠️ **"没装 git"和"这不是仓库"是两件事，别混成一个**（2026-09-20 修）：
        // 原来这里先调 `is_repo()`，而它把**任何失败**都吞成 `false` ——
        // 于是"这台机器上没有 git"会被报成"这条路径不在 git 仓库里"，
        // 而这两句话给用户的下一步**完全不同**（装 git vs 换个仓库）。
        // 这个是实测抓到的：PATH 里放一个 `git.cmd`（Rust 不认它）之后，
        // 编辑器回的就是"不在 git 仓库里"。
        Err(GitError::Failed { stderr, .. }) if stderr.contains("not a git repository") => {
            return Err(GitError::NotARepo)
        }
        Err(e) => return Err(e),
    };
    if s.is_empty() {
        return Err(GitError::NotARepo);
    }
    let p = std::path::PathBuf::from(s);
    // ⚠️ **和围栏报的路径对齐**：围栏的根是 `canonicalize` 过的（Windows 上是 `\\?\C:\…`），
    // 而 git 吐的是正斜杠的普通路径 —— 两边不一致，`strip_prefix` 就会失败，
    // 于是"这个文件在不在仓库里"判错（第一版 `restore` 就因此说"它不在仓库里面"）。
    Ok(std::fs::canonicalize(&p).unwrap_or(p))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_directory_is_not_a_repo() {
        let d = std::env::temp_dir().join(format!("harness-git-plain-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&d);
        assert!(!is_repo(&d));
        assert_eq!(checkpoint(&d, "s-1", 1).unwrap_err(), GitError::NotARepo);
    }

    /// `git init` 之后、**第一次提交之前**：`stash create` 存不了。
    ///
    /// 这值得单独一个错误：调用方改参数没用（得用户先提交一次），
    /// 而且它很容易被当成别的东西（不看清那句 stderr 会报成含糊的"git 失败了"）。
    #[test]
    fn a_repo_without_its_first_commit_says_so_by_name() {
        let root = std::env::temp_dir().join(format!("harness-git-fresh-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let ok = Command::new("git")
            .current_dir(&root)
            .args(["init", "-q"])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !ok {
            eprintln!("环境里没有 git —— 跳过");
            return;
        }
        std::fs::write(root.join("a.txt"), "还没提交过").unwrap();
        assert_eq!(checkpoint(&root, "s-1", 1).unwrap_err(), GitError::NoCommitYet);
        // 建不出来就要说"别试了，找用户"（不是"你参数写错了"）
        assert_eq!(GitError::NoCommitYet.code(), "refused");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// **核心判据**：建 checkpoint **不动工作区、不动索引、不进 stash 列表**。
    ///
    /// 这条在 Rust 侧验前面三条；端到端的完整版在 `scripts/smoke-gitcheckpoint.ps1`。
    #[test]
    fn a_checkpoint_changes_neither_the_worktree_nor_the_index() {
        let root = std::env::temp_dir().join(format!("harness-git-cp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let ok = Command::new("git")
            .current_dir(&root)
            .args(["init", "-q"])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !ok {
            eprintln!("环境里没有 git —— 跳过");
            return;
        }
        // 先做一次提交：`git stash create` 需要 HEAD 有东西可比
        // （**没有第一个提交**是一个单独的错误，见下面那条测试）
        std::fs::write(root.join("a.txt"), "第一版").unwrap();
        run(&root, &["add", "a.txt"]).unwrap();
        run(&root, &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "-m", "第一个提交"]).unwrap();
        // 再造一点"未提交的改动"（checkpoint 的存在意义就是存它）
        // ⚠️ **故意带上首尾空白**：这样"读回来是不是原样"才有得判 ——
        // 第一版的测试数据没有空白，于是"输出被 trim 掉"那个 bug 溜过去了。
        std::fs::write(root.join("a.txt"), "\n第二版  \n").unwrap();
        let before = std::fs::read_to_string(root.join("a.txt")).unwrap();
        let status_before = run(&root, &["status", "--porcelain"]).unwrap();

        let cp = checkpoint(&root, "s-test", 1).expect("有改动，该建得上");
        assert!(!cp.hash.is_empty());
        assert_eq!(cp.name, "refs/agent/s-test/1");

        // ① 工作区一个字没变 ② 索引一个字没变 ③ ref 真的在那儿
        assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), before);
        assert_eq!(run(&root, &["status", "--porcelain"]).unwrap(), status_before);
        assert_eq!(list(&root, "s-test").unwrap(), vec![cp.clone()]);
        // ④ 而且它**不在 stash 列表里**（我们绕开了那条路）
        assert_eq!(run(&root, &["stash", "list"]).unwrap(), "");
        // ⑤ 那个提交里存的是**改动之后**的内容
        assert_eq!(
            read_at(&root, &cp.hash, "a.txt").unwrap(),
            "\n第二版  \n",
            "读回来的必须是**原样的字节** —— 首尾空白也是内容"
        );

        let _ = std::fs::remove_dir_all(&root);
    }
}
