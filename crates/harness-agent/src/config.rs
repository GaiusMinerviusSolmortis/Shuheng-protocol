//! **智能体实例配置**：一份 JSON，说清"我是谁"（2026-09-26）。
//!
//! 为什么要有它：这个 exe 原来把"我是谁"全塞在命令行里（`--persona` / `--model` /
//! `--model-window` …）。命令行**能起一个实例**，但它**不是一份配置** ——
//! 没有 schema、没有校验、改一个字段要去数引号，而且"这个实例当初是怎么配的"
//! 除了那串参数**没有别的地方记着**（身份索引里记的也是 `cmd` 字符串）。
//!
//! 分界线（这一版就按它划，两个方向都别越）：
//!
//! | 归**配置**（`--config`）| 归**启动参数** |
//! |---|---|
//! | 人格 / 模型 / 内核（含窗口、摘要器）/ 步数上限 | 实例 id、会话 id、围栏、脚本、`--resume`、`--env-file` |
//!
//! 一句话：**"我是谁"归配置，"我这辈子怎么起"归命令行。**
//!
//! 两条规矩，都是为了不留两份真相：
//!
//! 1. **配置优先**：两边都给同一个字段时以配置为准 —— 但**必须出声**（逐项报出
//!    是哪几个命令行开关被覆盖了）。沉默地覆盖，正是"两份真相"最省事的那种形态。
//! 2. **不认识的就拒绝启动**，而且**不许声明做不到的事**：不认识的 `kernel.kind`
//!    要说清现在有哪些；`kernel.sees_content: false` 直接拒 —— 这一版的内核
//!    **在智能体进程里**，它必然读得到上下文正文，写 `false` 是假话
//!    （同规范-工具服务那条"没实现的不许声明"）。

use serde::Deserialize;
use std::collections::BTreeSet;

/// **已知的内核**，两个档：
///
/// | 名字 | 在哪 | `sees_content` |
/// |---|---|---|
/// | `linear` | 智能体**进程内**（codex 那套窗口 + 压缩）| 只能是 `true` —— 它就在手里，读不到反而不可能 |
/// | `remote` | **另一个进程**（一个服务），要写 `service` | `true` / `false` **都行** —— 写 `false` 就不给它正文，只给元数据 |
pub const KERNELS: [&str; 2] = ["linear", "remote"];

/// **已知的模型适配器**（和 `build_model` 认识的那两个对齐）。
pub const MODELS: [&str; 2] = ["mock", "deepseek"];

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AgentConfig {
    #[serde(default)]
    pub persona: Option<PersonaCfg>,
    #[serde(default)]
    pub model: Option<ModelCfg>,
    #[serde(default)]
    pub kernel: Option<KernelCfg>,
    /// 一个回合里最多几步工具调用
    #[serde(default)]
    pub max_steps: Option<usize>,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PersonaCfg {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub brief: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ModelCfg {
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub max_tokens: Option<u64>,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct KernelCfg {
    #[serde(default)]
    pub kind: Option<String>,
    /// `kind: "remote"` 时必填：内核服务的实例 id（比如 `kernel:1`）
    #[serde(default)]
    pub service: Option<String>,
    /// 上下文窗口有多大（原来的 `--model-window`）
    #[serde(default)]
    pub window: Option<u64>,
    /// 摘要器用谁（原来的 `--summarize`：`mock` / `model`）
    #[serde(default)]
    pub summarize: Option<String>,
    /// **这个内核会不会读到上下文正文**。
    ///
    /// `linear` 只能写 `true`（或者不写）—— 内核就在智能体进程里，
    /// 读不到正文反而是不可能的，写 `false` 会被**拒绝启动**。
    ///
    /// `remote` 两边都行，而且**这一格是真的门**：写 `false`，智能体就
    /// **一个字节的正文都不发过去**（只给下标 / 类型 / 字节数）。
    #[serde(default)]
    pub sees_content: Option<bool>,
}

/// 读一份配置并校验。**所有失败都要能照着修**（报错即手册）。
pub fn load(path: &str) -> Result<AgentConfig, String> {
    let text = std::fs::read_to_string(path).map_err(|e| {
        format!(
            "读不到实例配置 `{path}`：{e}。给一个存在的 JSON 文件；\
             不给 `--config` 就按命令行起（那是老行为）"
        )
    })?;
    let cfg: AgentConfig = serde_json::from_str(&text).map_err(|e| {
        format!(
            "实例配置 `{path}` 不合规：{e}。\
             顶层只有 persona / model / kernel / max_steps 四个字段，**不认识的字段会被拒**\
             ——宁可拒绝，也不猜"
        )
    })?;
    check(&cfg)?;
    Ok(cfg)
}

/// 校验：不知道的拒绝；**声明做不到的也拒绝**。
pub fn check(cfg: &AgentConfig) -> Result<(), String> {
    if let Some(k) = &cfg.kernel {
        let kind = k.kind.as_deref().unwrap_or("linear");
        if let Some(kind) = &k.kind {
            if !KERNELS.contains(&kind.as_str()) {
                return Err(format!(
                    "不认识的 `kernel.kind`：`{kind}`。现在只有：{}。\
                     跨进程的内核（能接外来技术栈的那种）还没做 —— 那不是配置里写个名字就能有的",
                    KERNELS.join(" / ")
                ));
            }
        }
        if kind == "remote" {
            if k.service.as_deref().unwrap_or("").is_empty() {
                return Err(
                    "`kernel.kind` 是 `remote` 就得写 `service` —— 那是内核服务的实例 id\
                     （比如 `kernel:1`）。不写的话，智能体不知道该去问谁"
                        .into(),
                );
            }
        } else if k.service.is_some() {
            return Err(format!(
                "`kernel.service` 只在 `kind: \"remote\"` 时有意义 —— 现在是 `{kind}`\
                 （进程内的内核不需要服务）"
            ));
        }
        if kind == "linear" && k.sees_content == Some(false) {
            return Err(
                "`kernel.sees_content` 写 `false` 是不对的：这一版的内核**在智能体进程里**，\
                 它必然读得到上下文正文 —— 声明一件做不到的事，比不声明更糟。\
                 要真的不给它正文，就把内核换成 `remote`（那一格在那边才是真的门）"
                    .into(),
            );
        }
    }
    if let Some(m) = &cfg.model {
        if let Some(kind) = &m.kind {
            if !MODELS.contains(&kind.as_str()) {
                return Err(format!(
                    "不认识的 `model.kind`：`{kind}`。现在只有：{}",
                    MODELS.join(" / ")
                ));
            }
        }
    }
    if let Some(k) = &cfg.kernel {
        if let Some(s) = &k.summarize {
            if crate::SummarizeWith::parse(s).is_none() {
                return Err(format!(
                    "不认识的 `kernel.summarize`：`{s}`。只认 `mock` / `model`\
                     （`mock` = 不调模型也能压；`model` = 用模型写摘要）"
                ));
            }
        }
    }
    Ok(())
}

/// 记一笔"这一项被配置覆盖了"。
///
/// **只有命令行真的给了**（`explicit` 里记着）**而且值不一样**时才算覆盖 ——
/// 配置的值跟默认值相同时什么都不算：**别报假账**。
fn note(flag: &str, explicit: &BTreeSet<String>, changed: bool, hit: &mut Vec<String>) {
    if changed && explicit.contains(flag) {
        hit.push(flag.to_string());
    }
}

/// **把配置套到启动参数上**（配置优先），返回**被覆盖的命令行开关**。
///
/// 为什么要返回那个列表：沉默地覆盖就是"两份真相"最省事的形态 ——
/// 部署的人以为命令行生效了，而其实没有。所以调用方要把这几项**逐项打出来**。
///
/// 只有**命令行真的给了**（`args.explicit` 里记着）而且**值不一样**时才算"覆盖"；
/// 配置给的值跟默认值一样时什么都不算 —— 别报假账。
pub fn apply(cfg: &AgentConfig, args: &mut crate::Args) -> Vec<String> {
    let mut hit: Vec<String> = Vec::new();
    let ex = args.explicit.clone();
    if let Some(p) = &cfg.persona {
        if let Some(v) = &p.name {
            let changed = args.persona != *v;
            note("--persona", &ex, changed, &mut hit);
            if changed {
                args.persona = v.clone();
            }
        }
        if let Some(v) = &p.version {
            let changed = args.persona_version != *v;
            note("--persona-version", &ex, changed, &mut hit);
            if changed {
                args.persona_version = v.clone();
            }
        }
        if let Some(v) = &p.brief {
            let changed = args.persona_brief != *v;
            note("--persona-brief", &ex, changed, &mut hit);
            if changed {
                args.persona_brief = v.clone();
            }
        }
    }
    if let Some(m) = &cfg.model {
        if let Some(v) = &m.kind {
            let changed = args.model != *v;
            note("--model", &ex, changed, &mut hit);
            if changed {
                args.model = v.clone();
            }
        }
        if let Some(v) = &m.name {
            let changed = args.model_name != *v;
            note("--model-name", &ex, changed, &mut hit);
            if changed {
                args.model_name = v.clone();
            }
        }
        if let Some(v) = &m.base_url {
            let changed = args.base_url != *v;
            note("--base-url", &ex, changed, &mut hit);
            if changed {
                args.base_url = v.clone();
            }
        }
        if let Some(v) = m.max_tokens {
            let changed = args.max_tokens != v;
            note("--max-tokens", &ex, changed, &mut hit);
            if changed {
                args.max_tokens = v;
            }
        }
    }
    if let Some(k) = &cfg.kernel {
        // **整块搬过去**（内核那几格是一组：kind / service / sees_content / window / summarize）。
        // 它没有对应的命令行开关，所以不报"覆盖了谁" —— 但它就在下面 `apply` 里生效。
        args.kernel = k.clone();
        if let Some(v) = k.window {
            let changed = args.model_window != v;
            note("--model-window", &ex, changed, &mut hit);
            if changed {
                args.model_window = v;
            }
        }
        // `check` 已经把不认识的摘要器拒了，所以这里能放心落值
        if let Some(v) = &k.summarize {
            if let Some(s) = crate::SummarizeWith::parse(v) {
                let changed = args.summarize != Some(s);
                note("--summarize", &ex, changed, &mut hit);
                args.summarize = Some(s);
            }
        }
    }
    if let Some(v) = cfg.max_steps {
        let changed = args.max_steps != v;
        note("--max-steps", &ex, changed, &mut hit);
        if changed {
            args.max_steps = v;
        }
    }
    hit
}
