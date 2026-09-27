//! 中控（无 GUI 版）。
//!
//! 中控 = **内核的功能模块**（生命周期、注册表、路由、仲裁、审计），
//! **不是 GUI 应用**（README 阶段 1）。第一版全部走 stdio：
//!
//! ```text
//!   stdin   ← 来自 console（用户那侧）的 NDJSON
//!   stdout  → 可观察的消息流（服务与中控自己发出的每一条）
//!   stderr  → 日志，**不混进协议**
//! ```
//!
//! 三条不变量在这里落地：先记录再投递（`route`）、每种消息都能过文本（启动自检）、
//! 协议层不认传输（`harness-core` 不知道 stdio 存在）。

mod registry;
mod service;
mod index;

use crate::service::StopReport;
use harness_core::codec::{self, CodecError, NdjsonReader, NextLine};
use harness_core::direction::{self, DirectionContext};
use harness_core::envelope::{Authority, Envelope, MessageType};
use harness_core::id::IdGen;
use harness_core::instance_event::InstanceEvent;
use harness_core::{invariant, now_ms, SessionLog};
use registry::{Binding, Instance, Registry};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{self, BufReader};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

const CTRL: &str = "ctrl";
/// **入口的身份和权威上限是配置出来的，不是写死的。**
///
/// 原来这里是个常量 `console`，15 处引用——**入口只有一条，而且钉死在 stdin 上**。
/// 但 §3 的实例清单里"通道"是单独一行（微信 / WS / HTTP 等外部入口，有连接状态），
/// 而 §4.5 那张表说得更清楚：**转发来的消息，权威取决于来源身份**。
/// 所以入口得是个**实例**：有自己的 id、自己的权威上限、自己的连接状态。
const DEFAULT_CHANNEL: &str = "console";

/// 中控的输入只有两个来源，汇进同一条通道 —— 于是主循环是单线程的，
/// 写注册表、写日志、写 stdout 都不需要锁。
pub enum Inbound {
    Console(String),
    /// 控制通道上收到一行非 UTF-8 —— **报出来，但不停止读取**
    ConsoleInvalid { bytes: usize },
    ConsoleEof,
    ServiceLine { instance: String, generation: u64, line: String },
    ServiceExited { instance: String, generation: u64 },
    /// **订阅口上来了一个人**（2026-09-27）：连接已经握手通过，交回主循环。
    /// `seq` 是**这条连接自己的随机标签**（读线程的报告靠它认人）。
    Subscribe { seq: String, conn: harness_ipc::Conn },
    /// 订阅口上的一行（在他 `subscribe` 之前也可能有 —— 那正是"我要订阅"那句话）
    SubLine { seq: String, line: String },
    /// 订阅口上的人走了
    SubEof { seq: String },
}

#[derive(Debug)]
struct Args {
    sid: String,
    data_dir: PathBuf,
    services: Option<PathBuf>,
    drain: Duration,
    /// 这条 stdio 通道叫什么、权威上限多高
    channel: String,
    channel_ceiling: Authority,
    /// **恢复一个已经存在的会话**：从 `meta.json` 把坐标读回来
    resume: bool,
    /// **开会话时就带上托管路径**（可重复）。
    ///
    /// 必须能在启动时给：因为"围栏来自会话"的那些服务是在启动时装配的 ——
    /// 要是只能等控制台发 `host`，它们就永远起不来了。
    hosts: Vec<String>,
    /// **提示词库**（2026-09-20）：一个目录，里面的 `.md` 是**可被引用的提示词片段**。
    ///
    /// 为什么它是 ctrl 的事、不是某个服务的事：片段的去处是**智能体的上下文**，
    /// 而唯一能往上下文里塞东西的通道是 `inject` —— **只有中控能发**（方向规则一）。
    /// 所以"把哪一段念给谁听"这件事，只能由中控来做。
    prompts: Option<PathBuf>,
    /// **周期 tick 的间隔**。
    ///
    /// tick 只做两件事，而且**都不做自动动作**：探 `health`、更新"最后活动时间"。
    /// §5.20 定过"默认开着就是有用、不做自动回收" ——
    /// 所以这个 tick **不杀任何东西**，它只是把"它不应答了"变成一条看得见的事实。
    tick: Duration,
    /// **把每一次"分派前落盘"都打出来**（`--debug-barriers`）。
    ///
    /// 为什么要一个开关：屏障**正常时不说话** —— 有副作用的调用本来就该落盘，
    /// 而终端跑一条命令就落一次，出声会把 stderr 淹掉。
    /// 但"它到底落了没有"必须是**可验的**，否则这条机制只能靠读代码相信。
    debug_barriers: bool,
    /// **开不开订阅口**（`--no-subscribe` 关掉）。**默认开** ——
    /// 一条"谁也看不见"的会话是更糟的默认值（而且订阅口只是回环，不经网卡）。
    subscribe: bool,
    /// **给刚拉起来的服务留多久"报到"的时间**（`--startup-grace-ms`）。
    ///
    /// 为什么需要它：`route` 原来会**在服务自报家门之前就派活** ——
    /// 手工喂 NDJSON 的那几个验收脚本一次性把请求灌进来，恰好撞在这个窗口里。
    /// 后果不是"偶尔慢一点"，而是**注册关有个洞**：契约闸门还没看过它的 manifest，
    /// 请求就已经递到它手上了（于是"destructive 的调用要先落盘"这条也判不出来）。
    ///
    /// 现在：**报到之前先攒着**（按到达顺序），manifest 一到就照序放行；
    /// 超时还没报到就拒掉，并说清"它拉起来了但一直没自报家门"。
    startup_grace: Duration,
}

fn parse_args() -> Args {
    let mut args = Args {
        // **会话 id 也要唯一到跨进程**：`s-<毫秒>` 在两个会话同时开的时候会撞，
        // 而撞了之后它们会指向同一个 `data/sessions/<sid>/` 目录 —— 两份日志写一个文件。
        sid: format!("s-{}-{}", now_ms(), harness_core::id::short_rand()),
        data_dir: PathBuf::from("data"),
        services: None,
        drain: Duration::from_millis(3000),
        channel: DEFAULT_CHANNEL.into(),
        channel_ceiling: Authority::User,
        resume: false,
        hosts: Vec::new(),
        prompts: None,
        tick: Duration::from_millis(2000),
        debug_barriers: false,
        subscribe: true,
        startup_grace: Duration::from_millis(2000),
    };
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--sid" => {
                if let Some(v) = it.next() {
                    args.sid = v;
                }
            }
            "--data-dir" => {
                if let Some(v) = it.next() {
                    args.data_dir = PathBuf::from(v);
                }
            }
            "--services" => args.services = it.next().map(PathBuf::from),
            "--prompts" => args.prompts = it.next().map(PathBuf::from),
            "--resume" => args.resume = true,
            "--host" => {
                if let Some(v) = it.next() {
                    args.hosts.push(v);
                }
            }
            // `--channel 微信:agent` —— 入口的身份是配置出来的
            "--channel" => {
                if let Some(v) = it.next() {
                    let (id, ceiling) = v.split_once(':').unwrap_or((v.as_str(), "user"));
                    args.channel = id.to_string();
                    args.channel_ceiling = match ceiling {
                        "user" => Authority::User,
                        "system" => Authority::System,
                        "agent" => Authority::Agent,
                        _ => Authority::Observe,
                    };
                }
            }
            "--drain-ms" => {
                if let Some(v) = it.next().and_then(|v| v.parse::<u64>().ok()) {
                    args.drain = Duration::from_millis(v);
                }
            }
            "--tick-ms" => {
                if let Some(v) = it.next().and_then(|v| v.parse::<u64>().ok()) {
                    // `0` = 关掉 tick（调试时用；正常别关 —— 探针是唯一的"它还应答吗"信号）
                    args.tick = Duration::from_millis(v);
                }
            }
            "--debug-barriers" => args.debug_barriers = true,
            // 关掉订阅口：用在不希望开一个回环端口的场合（比如受限环境）。
            "--no-subscribe" => args.subscribe = false,
            "--startup-grace-ms" => {
                if let Some(v) = it.next().and_then(|v| v.parse::<u64>().ok()) {
                    args.startup_grace = Duration::from_millis(v);
                }
            }
            "--help" | "-h" => {
                println!(
                    "harness-ctrl --sid <会话id> [--data-dir <目录>] \
                     [--services <服务表.json>] [--drain-ms <毫秒>]"
                );
                std::process::exit(0);
            }
            other => eprintln!("[ctrl] 不认识的参数：{other}"),
        }
    }
    args
}

#[derive(Debug, Deserialize, Clone)]
struct ServiceSpec {
    kind: String,
    #[serde(default)]
    id: Option<String>,
    cmd: Vec<String>,
    /// 追加给服务的参数（例如编辑器的 `--fence <目录>`）。
    /// 中控自己的约定参数 `--instance` / `--sid` 永远在最后追加。
    #[serde(default)]
    args: Vec<String>,
    /// `"tool"`（默认）还是 `"agent"` —— **这是中控唯一在意这个区别的地方：
    /// 权威上限**。工具最高只能是 `observe`，智能体可以到 `agent`（§1.1）。
    #[serde(default)]
    role: Option<String>,
    /// **它被授权用哪些实例**（只有 `role: "agent"` 的条目需要）。
    ///
    /// 位置很关键：这张表来自**中控自己的配置**，不是智能体自报的 ——
    /// §2.3 说的就是"**绑定是中控签发的**"。让智能体自己声明"我能用什么"，
    /// 等于让它自己发门禁卡。
    ///
    /// 不给就是空表 ⇒ **默认拒绝**（§5.10 那条"默认拒绝的绑定"）。
    ///
    /// 两种写法都收：
    ///
    /// ```jsonc
    /// "bindings": ["term:1"]                                  // 全权（这只是"能调"）
    /// "bindings": [{"instance": "term:1",                   // 带能力（这还能限制"能干什么"）
    ///               "capabilities": ["proc.exec"]}]
    /// ```
    #[serde(default)]
    bindings: Vec<BindingSpec>,
    /// **它的围栏由会话刚给人**（而不是写死在这张表里）。
    ///
    /// 这一条就是"项目搬走就在会话里委托新路径"能成立的地方：
    /// 服务的围栏不再是启动参数里的一个常量，而是**装配时从会话取的**。
    /// 于是"重新托管"= 重新装配 = 围栏跟着换。
    #[serde(default)]
    wants_fence: bool,
    /// **什么时候把它拉起来**（`per_session` 默认 / `per_call`）。
    ///
    /// 为什么这个决定必须在**服务表**里、不能在 manifest 里：
    /// **要看 manifest 就得先把它拉起来** —— 而"要不要拉"正是这里要决定的事。
    /// 所以分工和 `wants_fence` 一样：**表是部署配置（中控拿它做决定），
    /// manifest 是服务自述（服务自己声明，闸门校验形状）**。
    ///
    /// `per_call` 的落地含义是"**按需拉起**"：会话装配时**不**拉它，
    /// 第一次有请求指向它才拉 —— 而那条请求会走"没报到先攒着"那条路等它上来。
    /// ⚠️ **它不负责"用完就收"**：§5.20 定过不做自动回收（收只走四条确定路径）。
    #[serde(default)]
    lifecycle: Option<String>,
    /// **常驻**：会话结束不收它，**别的会话来接同一个进程**（2026-09-20）。
    ///
    /// 和 `lifecycle` 的关系要说清，否则就是同一件事写两遍：
    /// * `lifecycle` 是**服务自述**（manifest 里那句，闸门校验形状）；
    /// * `resident` 是**部署配置**（这一条）—— 因为"接上老进程"还需要一个
    ///   **会合目录**，那是这台机器上的部署事实，服务自己不知道。
    ///
    /// 两个都对上才算数：服务声明 `resident`、表里也写 `resident: true` + `rendezvous`。
    /// 只写一个的话中控会**明说缺什么**（见 `spawn_spec`）。
    #[serde(default)]
    resident: bool,
    /// **会合目录**：常驻服务把"我在哪个端口、口令是什么、我是几号进程"写在这儿，
    /// 让后来的会话找得到它。**必须是一个跨会话稳定的地方**（不能跟着 `--data-dir` 走，
    /// 否则新会话换个 data 目录就找不到了）。
    #[serde(default)]
    rendezvous: Option<String>,
}

/// 一条绑定的两种写法（§2.3 里那个 `{resource, capability, ...}` 的雏形）。
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum BindingSpec {
    /// 简写：只写实例名 —— **全权**（`["*"]`）
    Plain(String),
    /// 全写：连能力一起限
    WithCaps {
        instance: String,
        /// **`None`（没写这个字段）= 全权；`Some([])` = 零能力。**
        ///
        /// 这个区分不是洁癖：`{"instance":"term:1"}` 的意思是
        /// "我只限你能用哪个实例"；而 `{"instance":"term:1","capabilities":[]}`
        /// 的意思是"**它能碰到，但什么都不许干**" —— 后者是合法的用法，
        /// 拿来试"能力检查到底有没有生效"。第一版把两者混成一样，测试当场就撞出来了。
        #[serde(default)]
        capabilities: Option<Vec<String>>,
    },
}

impl BindingSpec {
    fn instance(&self) -> &str {
        match self {
            BindingSpec::Plain(s) => s,
            BindingSpec::WithCaps { instance, .. } => instance,
        }
    }
    /// 授给它的能力：简写是全权；全写没写 capabilities 也是全权
    /// （"我只限了你能用哪个实例，没限你能干什么"）。
    fn capabilities(&self) -> Vec<String> {
        match self {
            BindingSpec::Plain(_) => vec!["*".into()],
            BindingSpec::WithCaps {
                capabilities: None, ..
            } => vec!["*".into()],
            BindingSpec::WithCaps {
                capabilities: Some(c),
                ..
            } => c.clone(),
        }
    }
}

/// **一条等着投出去的请求**（服务还没自报家门，或者干脆还没到投递那一步）。
///
/// 为什么带上"投哪一行"和"要不要先落盘"：**记录与各项检查在第一次到达时已经做过了**，
/// 队列只负责把**投递**推迟 —— 放行时不能再走一遍 `route`（那会把同一条请求记第二遍日志）。
struct Queued {
    env: Envelope,
    /// 已经编码好的那一行（投递时直接用）
    line: String,
    since_ms: i64,
}

/// 一次还没落地的调用。**中控记得"我替谁在等谁"** ——
/// **怎么把它弄起来** —— 两种通道的决定就在这儿（2026-09-20）。
/// **提示词库**（2026-09-20）：一个目录里的 `.md` 就是**可被引用的提示词片段**。
///
/// 形状照 §3 那张清单里给"提示词"的定义：**可被引用、可组合的文本片段，版本化**。
///
/// * **文件名就是名字**（`coding-rules.md` → `coding-rules`）；
/// * **版本**写在第一行，形如 `@version 3`（不写就是 `0`）—— 那一行不进正文；
/// * **组合**：一次可以按顺序引用好几段，拼起来时每段带一个抬头
///   （`【提示词 名字@版本】`）—— **抬头必须留**，否则事后没人知道
///   "这句话是哪个片段说的、哪一版"。
///
/// ⚠️ **坏文件不许悄悄跳过**：读不了的收进 `skipped`，`prompts` op 里报出来。
/// "少了一段提示词"和"那一段坏了"是两件完全不同的事（同技能服务那条）。
#[derive(Clone)]
struct PromptLib {
    dir: std::path::PathBuf,
    frags: Vec<PromptFrag>,
    skipped: Vec<(String, String)>,
}

#[derive(Clone)]
struct PromptFrag {
    name: String,
    version: String,
    text: String,
}

impl PromptLib {
    fn load(dir: &std::path::Path) -> Self {
        let mut frags = Vec::new();
        let mut skipped = Vec::new();
        if let Ok(entries) = std::fs::read_dir(dir) {
            let mut paths: Vec<std::path::PathBuf> =
                entries.flatten().map(|e| e.path()).collect();
            paths.sort();
            for p in paths {
                let is_md = p.extension().and_then(|s| s.to_str()) == Some("md");
                let name = p
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("?")
                    .to_string();
                if !is_md || !p.is_file() {
                    // 不是 `.md` 的东西**不当错误**（目录里可能有 README 之外的杂物）
                    continue;
                }
                match std::fs::read_to_string(&p) {
                    Ok(raw) => {
                        let (version, text) = split_version(&raw);
                        if text.trim().is_empty() {
                            skipped.push((name, "这个片段是空的".to_string()));
                            continue;
                        }
                        frags.push(PromptFrag {
                            name,
                            version,
                            text,
                        });
                    }
                    Err(e) => skipped.push((name, format!("读不了：{e}"))),
                }
            }
        }
        Self {
            dir: dir.to_path_buf(),
            frags,
            skipped,
        }
    }

    fn get(&self, name: &str) -> Option<&PromptFrag> {
        self.frags.iter().find(|f| f.name == name)
    }

    fn names(&self) -> Vec<String> {
        self.frags.iter().map(|f| f.name.clone()).collect()
    }

    /// **重读一份当下的库** —— 两个 op 都用它。
    ///
    /// 为什么两个 op 必须一致地重读：`prompts` 说"有 coding-rules@1"，
    /// 而 `prompt` 念的是 coding-rules@2 —— 那种自相矛盾比"都不重读"更坏，
    /// 因为它让人以为**看过了就等于念过了**。
    fn now(&self) -> PromptLib {
        PromptLib::load(&self.dir)
    }
}

/// 第一行如果是 `@version X` 就当版本号（那一行**不进正文**）。
fn split_version(raw: &str) -> (String, String) {
    let mut lines = raw.lines();
    let first = lines.next().unwrap_or("");
    if let Some(v) = first.trim().strip_prefix("@version") {
        let v = v.trim();
        return (
            if v.is_empty() { "0".into() } else { v.to_string() },
            lines.collect::<Vec<_>>().join("\n"),
        );
    }
    ("0".to_string(), raw.to_string())
}

enum Launcher {
    /// 父子私有那条：stdin/stdout（第一版就有的）
    Stdio,
    /// 常驻那条：**先看有没有活着的，有就接上，没有才拉**
    Resident {
        dir: std::path::PathBuf,
        grace_ms: u64,
    },
}

/// **拉起一个服务要输入的那几样**（`start_service` 的入参）。
///
/// 为什么要打包成一个结构（clippy 的 `too_many_arguments` 是引子，不是理由）：
/// 它已经到 7 个入参了，调用点上就是一串裸值 ——
/// `"term"` 和 `"term:1"` 谁配谁、`None` 是"没角色"还是"没绑定"，
/// 都要回到签名那里数一遍才看得懂。打包之后**调用点自己会说话**。
struct Spawn {
    kind: String,
    id: String,
    cmd: Vec<String>,
    role: Option<String>,
    bindings: Vec<registry::Binding>,
    launcher: Launcher,
    tx: Sender<Inbound>,
}

/// 一次还没落地的调用。**中控记得"我替谁在等谁"** ——
/// 服务崩的时候要靠它给每个调用者一个诚实的交代（§4.6）。
///
/// `Clone` 是为**投递前的闸门**用的：那条路要读着 `pending` 里的记录
/// 去校验结果形状，**但不能把记录摘走**（摘了，方向规则四就看不见它了 ——
/// 详见 `on_service_line` 里那段）。
#[derive(Clone)]
struct PendingCall {
    target: String,
    caller: String,
    /// **这次调用的是哪个工具** —— 结果回来时要靠它找到"服务为这个工具
    /// 声明过的结果形状"（§11.1 C 组的 `output_schema`）。
    ///
    /// 为什么要在中控记一手：`corr` 只串起"这一次调用"，它不带工具名；
    /// 而校验结果形状必须知道"这是哪个工具的答案"。
    tool: Option<String>,
    /// **它是几时发出去的**（毫秒）—— 探针超时靠它判，不用另存一张表。
    started_ms: i64,
}

/// **会话**：图上的最上一层（§12.7）。
///
/// 它**不是一个入口** —— 入口是通道。会话是**坐标**：
/// 消息落在它上面，才有"算谁的历史"这回事（规范 §1：`sid` 必填，而传输无关）。
///
/// 它同时是**装配单**：点名哪个智能体、托管了哪些路径、现在有谁连着。
/// 这些在挂起时留着、结束时归档。
struct Session {
    sid: String,
    state: &'static str,
    created_at: i64,
    /// 托管进来的路径（§7.1：用户托管两样之一）
    hosts: Vec<serde_json::Value>,
    /// 点名的智能体实例（§12.7："会话点名，中控装配并持有"）
    agent: Option<String>,
    /// 现在有谁连着（通道 / 智能体）
    participants: Vec<String>,
}

impl Session {
    fn snapshot(&self) -> serde_json::Value {
        json!({
            "sid": self.sid,
            "state": self.state,
            "created_at": self.created_at,
            "hosts": self.hosts,
            "agent": self.agent,
            "participants": self.participants,
        })
    }

    /// 落盘的 `meta.json` —— **记坐标，不记路径**（07-界面 §12.7）。
    ///
    /// ⚠️ **2026-09-20 澄清**：这里原来写着"记锚点，不记路径 …… 锚点还没实现"。
    /// 而今天**不是"还没实现"，是那件事被取消了** —— §16.5 的修订删掉了
    /// "项目层"那一档（原话："记忆跟着会话走，项目搬走就在会话里重新托管"），
    /// 所以会话这一层**不需要**另算一套项目身份：它记的就是**托管的那条路径**，
    /// 而记忆那层的"锚点"由**调用方**给（`harness-mem` 的 `deposit` 参数）。
    /// 两件事，不是一件待办。
    fn meta(&self) -> serde_json::Value {
        json!({
            "sid": self.sid,
            "state": self.state,
            "created_at": self.created_at,
            "hosts": self.hosts,
            "agent": self.agent,
            "anchors": [],
            "anchors_todo": "路径还没换成锚点（§16.5）；现在 meta 里记的是路径本身",
            "ctrl_version": env!("CARGO_PKG_VERSION"),
        })
    }
}

/// **通道**：一个具体的口子 —— 接口那层的实现。
///
/// 它有 id、有权威上限、有连接状态。**入口的身份是配置出来的，不是写死的。**
#[derive(Clone)]
struct Channel {
    id: String,
    ceiling: Authority,
}

struct App {
    sid: String,
    log: SessionLog,
    reg: Registry,
    ids: IdGen,
    channel: Channel,
    session: Session,
    /// 服务表：**挂起之后要能照它重新装配起来**
    specs: Vec<ServiceSpec>,
    /// **提示词库**：可被引用的文本片段（`--prompts <目录>`）。
    ///
    /// 它**每次注入时重新读一遍**（不缓存）：提示词是**文件**，
    /// 写它的人不会来通知你 —— 而"刚改的规矩要重启中控才生效"是很讨厌的意外。
    /// （同技能服务那条取舍；一个目录的读取代价可以忽略。）
    prompts: Option<PromptLib>,
    /// 运行期再拉服务要用它（`spawn` 这个 op）
    tx: Sender<Inbound>,
    pending: HashMap<String, PendingCall>,
    exit_requested: bool,
    /// **实例 id 的铸造序号**（只增不减）。
    ///
    /// 原来 `spawn` 用的是 `注册表里现有实例的个数` 当后缀 ——
    /// 那是个**会回退**的量：删掉一个再加一个，后缀就重合了，
    /// 而重合的后果是注册表里**静默覆盖**（见 `Registry::insert`）：
    /// 新进程顶掉旧进程，发往那个 id 的消息转手交给它。
    /// id 参与鉴权（绑定 / 归属 / 路由），所以它必须**只增不减**。
    next_instance_seq: u64,
    /// **跨会话的身份账**（`data/index/instances.jsonl`）。
    /// 会话日志记的是消息，这份记的是"id 是什么 / 归谁 / 什么时候出现"。
    index: index::IndexLog,
    /// **水位表**：每个 tag 已经铸到几号了（`m-<tag>-<n>` / `c-<tag>-<n>` 里的那个 n）。
    ///
    /// 为什么在中控手里：**它看得见每一条消息**（不变量二：都从它这儿过）。
    /// 于是"重启之后接着铸"这件事不必新开一个计数器文件 ——
    /// 那是**第二份真相**（丢了就没保证、还会跟日志漂），而水位是从日志**算得出来**的。
    /// 它是**派生的读模型**，同身份索引：丢了能从会话日志重建。
    ///
    /// 发给谁、什么时候发：**拉起实例的时候塞进它的环境**（`HARNESS_ID_WATER`）——
    /// 因为服务的第一个出站消息（`presence`）是它还没跟任何人说过话时铸的，
    /// "先问再铸"来不及（`harness_core::id::water_from_env`）。
    id_water: std::collections::BTreeMap<String, u64>,
    /// 数据目录（索引读回来时要用它拼路径 —— 别从句柄反推）
    data_dir: PathBuf,
    /// 周期 tick 的间隔（`--tick-ms`，0 = 关）
    tick: Duration,
    /// 每次"分派前落盘"都打一行（`--debug-barriers`，让那条机制可验）
    debug_barriers: bool,
    /// **还没报到就先攒着的请求**（按服务分组，按到达顺序）。
    /// 一到 manifest 就照序放行；`startup_grace` 到点还没报到就逐个拒掉。
    waiting_ready: HashMap<String, Vec<Queued>>,
    /// 给服务"报到"的宽限（`--startup-grace-ms`）
    startup_grace: Duration,
    /// 已经报过的"实例状态不在自己声明的词表里"（`<服务>/<状态>`）——
    /// 同一个只吵一次，否则一个报错的循环服务能把日志刷爆。
    undeclared_states: std::collections::HashSet<String>,
    /// **订阅者**（2026-09-27）：连在订阅口上的那些只读观察者（GUI / 调试台 / 另一个界面）。
    ///
    /// 它们拿到的是**这条会话里路由过的每一条消息**（和 console 那份完全一样），
    /// 而它们自己**没有任何权力**：注册成实例、上限 `observe` —— 想动手就会被权限矩阵挡下。
    /// 这就是"每个客户端只要遵守协议就能接进来"在界面上那一半。
    subs: Vec<Sub>,
}

/// 一个订阅者：**一条连接 + 它自报的名字**。
pub struct Sub {
    /// 这条连接自己的随机标签（读线程靠它认人）
    seq: String,
    /// 还没 `subscribe` 的时候是 `None`（连上了、但还没说要订阅）
    id: Option<String>,
    conn: harness_ipc::Conn,
}

impl App {
    fn channel_id(&self) -> String {
        self.channel.id.clone()
    }

    /// 这条消息是不是**当前那一代**发的？
    ///
    /// 重新装配会把同一个 id 再拉起来，而旧进程的迟到消息还会到 ——
    /// 不认代的话，一条旧的 `offline` 就能把刚起来的新实例收掉（第一次跑就撞上了）。
    fn is_current(&mut self, id: String, generation: u64) -> bool {
        match self.reg.generation_of(&id) {
            // 表里没有它：可能是刚被收掉的旧世代，丢掉
            None => {
                eprintln!("[ctrl] 丢掉一条来自旧世代（{id} gen{generation}）的消息");
                false
            }
            Some(g) if g == generation => true,
            Some(g) => {
                eprintln!(
                    "[ctrl] 丢掉一条旧世代的 {id} 消息（它说 gen{generation}，现在是 gen{g}）"
                );
                false
            }
        }
    }
}

fn main() {
    std::process::exit(run());
}

fn run() -> i32 {
    let args = parse_args();

    // 不变量写进代码，不是文档里的一句希望（规范 §0）
    // 不变量一（传输无关）是**结构性**的，验不了运行时 ——
    // 它由 `harness-core` 的一条构建期测试盯着（查自己的依赖表）。
    // 以前这里有个 `assert_transport_agnostic()` 直接 `Ok(())`，那叫自欺欺人。
    if let Err(e) = invariant::assert_text_reachable(&args.sid) {
        eprintln!("[ctrl] 不变量三（文本可达）不成立：{e}");
        return 2;
    }

    let log = match SessionLog::open(&args.data_dir, &args.sid) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("[ctrl] 打不开会话日志：{e}");
            return 2;
        }
    };
    eprintln!(
        "[ctrl] 会话 {} 日志：{}",
        args.sid,
        log.log_path().display()
    );
    eprintln!(
        "[ctrl] 身份索引（跨会话、只追加）：{}",
        args.data_dir.join(index::FILE).display()
    );

    let (tx, rx): (Sender<Inbound>, Receiver<Inbound>) = mpsc::channel();

    // **水位从会话日志重建。** 它是派生读模型（见 `App::id_water`），
    // 而中控自己也要接着铸 —— 不然 `--resume` 之后它自己的 `m-ctrl-1` 会重号。
    // 宽松读：一条坏行只让水位偏低一点（而那种日志本来也重建不出会话）。
    let id_water = rebuild_id_water(&log);
    let ctrl_water = id_water.get("ctrl").copied().unwrap_or(0);
    let mut app = App {
        sid: args.sid.clone(),
        log,
        reg: Registry::default(),
        ids: IdGen::with_floor("ctrl", ctrl_water),
        channel: Channel {
            id: args.channel.clone(),
            ceiling: args.channel_ceiling,
        },
        session: Session {
            sid: args.sid.clone(),
            state: "active",
            created_at: now_ms(),
            hosts: Vec::new(),
            agent: None,
            participants: Vec::new(),
        },
        specs: Vec::new(),
        prompts: args
            .prompts
            .as_ref()
            .map(|d| PromptLib::load(d)),
        tx: tx.clone(),
        pending: HashMap::new(),
        exit_requested: false,
        next_instance_seq: 0,
        index: index::IndexLog::open(&args.data_dir),
        id_water,
        data_dir: args.data_dir.clone(),
        tick: args.tick,
        debug_barriers: args.debug_barriers,
        waiting_ready: HashMap::new(),
        startup_grace: args.startup_grace,
        undeclared_states: std::collections::HashSet::new(),
        subs: Vec::new(),
    };

    app.session.participants.push(app.channel.id.clone());

    // 启动时就能带托管路径 —— 趁装配之前（上面那条注释说了为什么）
    for h in &args.hosts {
        let canon = std::fs::canonicalize(h)
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| h.clone());
        eprintln!("[ctrl] 开会话时就托管：{canon}");
        app.session.hosts.push(json!({
            "path": canon,
            "hosted_at": now_ms(),
            "by": "启动参数",
            "anchor_note": "会话托管的是**路径**；记忆那层的「锚点」由调用方给\
                            （§16.5 的修订已删掉『项目层』那一档，所以这里没有待实现的锚点）",
        }));
    }

    // **跨进程恢复**：会话坐标从 `meta.json` 读回来。
    //
    // 注意恢复得回来的是什么、不是什么：
    // * **回得来**：会话状态、托管、点名的智能体 —— 这些是**坐标**，写在 meta 里；
    //   以及智能体看到过的上下文 —— 那在**日志**里（`log` 那条路，由智能体自己解释）。
    // * **回不来**：工具实例。它们是进程内的东西，跟着服务一起死了 ——
    //   而 §12.7 说的"挂起 = 工具实例收掉、会话留着"本来就是这个意思，
    //   所以这不是缺陷，是定义。
    if args.resume {
        let meta_path = app.log.dir().join("meta.json");
        let meta: Value = match std::fs::read_to_string(&meta_path)
            .map_err(|e| e.to_string())
            .and_then(|t| serde_json::from_str(&t).map_err(|e| e.to_string()))
        {
            Ok(m) => m,
            Err(e) => {
                eprintln!(
                    "[ctrl] 你说要恢复会话 {}，但读不出 {}：{e}",
                    args.sid,
                    meta_path.display()
                );
                return 2;
            }
        };
        let state = meta.get("state").and_then(Value::as_str).unwrap_or("active");
        if state == "ended" {
            eprintln!(
                "[ctrl] 会话 {} 已经**结束**了（ended）—— 结束不是挂起，不能接着用。\
                 要接着干就开一个新会话。",
                args.sid
            );
            return 2;
        }
        app.session.state = "active";
        app.session.hosts = meta
            .get("hosts")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        app.session.agent = meta
            .get("agent")
            .and_then(Value::as_str)
            .map(str::to_string);
        if let Some(a) = &app.session.agent {
            app.session.participants.push(a.clone());
        }
        eprintln!(
            "[ctrl] 恢复会话 {}：托管 {} 条，点名 {:?}，日志到 seq {}",
            args.sid,
            app.session.hosts.len(),
            app.session.agent,
            app.log.last_seq()
        );
    }

    let _ = app.log.write_meta(&app.session.meta());

    // 中控自己、**通道**、以及会话 —— 图上它们各是一个节点
    app.reg.insert(Instance {
        id: CTRL.into(),
        kind: "ctrl".into(),
        is_agent: false,
        ceiling: Authority::System,
        state: "running",
        service: None,
        manifest: None,
        config: None,
        generation: 0,
        bindings: None,
        owns: None,
        contract_errors: Vec::new(),
        contract_missing: Vec::new(),
        last_seen_ms: 0,
        unresponsive: false,
        lifecycle: "resident".into(),
        lifecycle_declared: None,
    });
    let sid_now = app.sid.clone();
    app.index
        .append(&sid_now, CTRL, "ctrl", "registered", json!({"role": "ctrl"}));
    // **通道是个实例**（§3 清单里那一行）：有 id、有权威上限、有连接状态
    app.reg.insert(Instance {
        id: app.channel.id.clone(),
        kind: "channel".into(),
        is_agent: false,
        ceiling: app.channel.ceiling,
        state: "connected",
        service: None,
        manifest: None,
        config: None,
        generation: 0,
        bindings: None,
        owns: None,
        contract_errors: Vec::new(),
        contract_missing: Vec::new(),
        last_seen_ms: 0,
        unresponsive: false,
        lifecycle: "resident".into(),
        lifecycle_declared: None,
    });
    let chan = app.channel.id.clone();
    app.index
        .append(&sid_now, &chan, "channel", "registered", json!({"ceiling": app.channel.ceiling.as_str()}));
    eprintln!(
        "[ctrl] 通道 `{}` 已接入（权威上限 {}）",
        app.channel.id,
        app.channel.ceiling.as_str()
    );

    // ---- 订阅口：**让别的进程也能看着这条会话**（2026-09-27）----
    //
    // 在这之前，能看见整条会话的只有"拉起中控的那个进程"（它占着 stdout）。
    // 于是一个**独立的 GUI 想接进来 —— 没有门**：它既不是服务表里的服务，
    // 也不是中控的子进程。现在中控**自己也当一次服务端**：
    // 回环端口 + 会合文件 + 一问一答握手，全是 `harness-ipc` 那套现成的东西
    // （常驻服务用的是同一份 —— 换了方向而已）。
    //
    // ⚠️ 口令不是安全边界（同 §6.3 那句）：它挡的是"同一台机器上随手连上来的
    // 别的东西"，不是"已经能读你文件的人"。
    // ⚠️ 订阅者是**只读**的：注册成实例、上限 `observe` —— 想动手会被权限矩阵挡下。
    if args.subscribe {
        let name = format!("{}-console", args.sid);
        match harness_ipc::Listener::bind(&name) {
            Ok(l) => {
                let dir = args.data_dir.join("rendezvous");
                let _ = std::fs::create_dir_all(&dir);
                let rv = harness_ipc::Rendezvous::new(&name, "console", l.addr(), l.token());
                match rv.write(&dir) {
                    Ok(p) => eprintln!(
                        "[ctrl] 订阅口：{}（别的进程带着里面那个口令连上来，\
                         `subscribe` 之后就能看见这条会话里的一切）",
                        p.display()
                    ),
                    Err(e) => eprintln!("[ctrl] 订阅口的会合文件写不进去：{e}（它只在内存里活着）"),
                }
                if let Ok(l2) = l.try_clone() {
                    let tx = tx.clone();
                    std::thread::spawn(move || loop {
                        match l2.accept() {
                            Ok(conn) => {
                                // **这条连接自己的标签**：读线程和主循环靠它认人
                                let seq = harness_ipc::random_token();
                                // 读的那半交给这条读线程；写的那半（连 ack 一起）交回主循环
                                let Ok(rd) = conn.try_clone() else { continue };
                                let seq_rd = seq.clone();
                                let tx2 = tx.clone();
                                std::thread::spawn(move || {
                                    let mut rd = rd;
                                    loop {
                                        match rd.read_line() {
                                            Ok(Some(line)) => {
                                                if tx2
                                                    .send(Inbound::SubLine {
                                                        seq: seq_rd.clone(),
                                                        line,
                                                    })
                                                    .is_err()
                                                {
                                                    return;
                                                }
                                            }
                                            Ok(None) | Err(_) => {
                                                let _ = tx2.send(Inbound::SubEof {
                                                    seq: seq_rd.clone(),
                                                });
                                                return;
                                            }
                                        }
                                    }
                                });
                                if tx.send(Inbound::Subscribe { seq, conn }).is_err() {
                                    return;
                                }
                            }
                            // 握手不过：`accept()` 已经回过一句"口令不对"，这里记一笔就够
                            Err(e) => eprintln!("[ctrl] 订阅口上来了一条握手不过的连接：{e}"),
                        }
                    });
                }
            }
            Err(e) => eprintln!("[ctrl] 订阅口起不来：{e} —— **会话照常**（订阅不是它的前提）"),
        }
    }

    // console 那条读线程：stdin 关了就报 EOF，主循环据此进入排空
    {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let mut reader = NdjsonReader::new(BufReader::new(io::stdin()));
            loop {
                match reader.next_line_checked() {
                    Ok(NextLine::Line(line)) => {
                        if tx.send(Inbound::Console(line)).is_err() {
                            return;
                        }
                    }
                    // **一行坏字节不能掐死整条控制通道。**
                    Ok(NextLine::Invalid { bytes }) => {
                        if tx.send(Inbound::ConsoleInvalid { bytes }).is_err() {
                            return;
                        }
                    }
                    Ok(NextLine::Eof) => break,
                    Err(e) => {
                        eprintln!("[ctrl] 读 stdin 失败：{e}");
                        break;
                    }
                }
            }
            let _ = tx.send(Inbound::ConsoleEof);
        });
    }

    // 按服务表拉起服务（阶段 1 只有这一种"工具实例"的来源）
    if let Some(path) = &args.services {
        match load_specs(path) {
            Ok(specs) => {
                // 把算出来的 id **写回 spec** —— 挂起之后要照它重新装配，
                // 那时得装出**同一批 id**，否则会话里的引用全对不上
                let mut resolved = Vec::new();
                for (n, mut spec) in specs.into_iter().enumerate() {
                    if spec.id.is_none() {
                        spec.id = Some(format!("{}:{}", spec.kind, n + 1));
                    }
                    // **`per_call` 的不在这儿拉** —— 同一个判断，同一个函数（见 `should_start_now`）
                    if !should_start_now(&spec) {
                        eprintln!(
                            "[ctrl] {} 声明的是 per_call —— 装配时不拉，等第一次用到它",
                            spec.id.clone().unwrap_or_default()
                        );
                        resolved.push(spec);
                        continue;
                    }
                    if let Err(e) = app.spawn_spec(&spec, tx.clone()) {
                        eprintln!("[ctrl] 起服务失败：{e}");
                    }
                    resolved.push(spec);
                }
                app.specs = resolved;
            }
            Err(e) => eprintln!("[ctrl] 读服务表 {} 失败：{e}", path.display()),
        }
    }

    let mut draining = false;
    let mut deadline: Option<Instant> = None;
    // **周期 tick 的时刻**。tick 只探针 + 记账，不杀东西（§5.20）。
    // `--tick-ms 0` 就把它关掉（调试时用 —— 正常别关，探针是唯一的"它还应答吗"信号）。
    let mut next_tick = Instant::now() + app.tick;

    loop {
        // 等两样东西里先到的那个：一条消息，或者 tick 到点。
        let wait_until = match deadline {
            Some(d) => d.min(next_tick),
            None => next_tick,
        };
        let left = wait_until.saturating_duration_since(Instant::now());
        let next = match rx.recv_timeout(left) {
            Ok(m) => Some(m),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => break,
        };

        match next {
            None => {
                // 到点了。可能是排空超时（该走人），也可能是 tick 该跑一次 ——
                // 这两件事不能混（原来 `None` 直接 break，等于"有 tick 就没法排空"）
                if let Some(d) = deadline {
                    if Instant::now() >= d {
                        break;
                    }
                }
                app.tick();
                next_tick = Instant::now() + app.tick;
            }
            Some(Inbound::Console(line)) => app.on_console_line(&line),
            // ---- 订阅口（2026-09-27）----
            Some(Inbound::Subscribe { seq, conn }) => app.on_subscribe(seq, conn),
            Some(Inbound::SubLine { seq, line }) => app.on_sub_line(&seq, &line),
            Some(Inbound::SubEof { seq }) => app.on_sub_eof(&seq),
            Some(Inbound::ConsoleInvalid { bytes }) => {
                // 报出来，并且指出**最可能的原因** —— 这是"报错即手册"里最值钱的一种：
                // 编码问题在中文 Windows 上几乎总是同一个成因。
                app.fault(
                    &app.channel_id(),
                    None,
                    "invalid",
                    format!("控制通道上收到一行不是 UTF-8 的字节（{bytes} 字节）"),
                    "协议是 **UTF-8 的 NDJSON**。这一行被跳过了，通道还在 —— \
                     请检查写入端的编码：PowerShell 要设 StandardInputEncoding 为 UTF-8，\
                     它的默认值走的是系统 ANSI 代码页（中文 Windows 上是 GBK）",
                );
            }
            Some(Inbound::ServiceLine {
                instance,
                generation,
                line,
            }) => {
                // **旧世代的消息丢掉** —— 重新装配之后，旧进程还会吐出几条迟到的
                // presence / 退出通知；不认代的话，它们会把新实例打死。
                if app.is_current(instance.clone(), generation) {
                    app.on_service_line(&instance, &line);
                }
            }
            Some(Inbound::ServiceExited {
                instance,
                generation,
            }) => {
                if app.is_current(instance.clone(), generation) {
                    app.on_service_exited(&instance);
                }
            }
            Some(Inbound::ConsoleEof) => {
                // 排空是为了**不变量二的反面**：已经发出去的调用，结果还没落盘就走人，
                // 日志里就会留下一个"有请求、无结果"的洞。
                //
                // 这里用的是"**回合结束**"这个边界 —— 三个边界里的第三个（规范-消息协议 §5.6）。
                // 另外两个**已经都在了**（2026-09-20）：
                //   * **有副作用工具分派前** —— 见 `dispatch()`（判据是 `destructiveHint`）；
                //   * **模型请求前** —— 智能体发 `{"op":"barrier"}` 请中控落（见 `local_op`）。
                // 边界怎么选仍然是**暂定值**（待定 #100 / 规范 §8 #8），但机制不是空话了。
                let _ = app.log.barrier();
                if app.pending.is_empty() {
                    break;
                }
                draining = true;
                deadline = Some(Instant::now() + args.drain);
                eprintln!(
                    "[ctrl] stdin 关了，还剩 {} 个没回来的调用，最多等 {:?}",
                    app.pending.len(),
                    args.drain
                );
            }
        }

        if app.exit_requested || (draining && app.pending.is_empty()) {
            break;
        }
    }

    app.shutdown();
    0
}

/// **这条 spec 现在该不该拉起来？**
///
/// `per_call` = **按需拉起**：装配时不拉，等第一次有请求指向它（见 `route`）。
/// 这样一个决定只写在一处 —— 第一版只改了 `assemble`，而**启动那条路**是另一段循环，
/// 于是 `smoke-lifecycle-field` 第一跑就抓到"声明了 per_call 却照样被拉起来了"。
fn should_start_now(spec: &ServiceSpec) -> bool {
    match spec.lifecycle.as_deref() {
        Some("per_call") => false,
        // 不认识的写法按默认来（表是部署配置，写错的后果该是"能跑"，不是"起不来"）
        _ => true,
    }
}

fn load_specs(path: &PathBuf) -> Result<Vec<ServiceSpec>, String> {
    let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    serde_json::from_str(&text).map_err(|e| e.to_string())
}

/// **中控认识的全部 op**（规范-消息协议 §2.7 那张名册）：**21 个名字 / 20 条分支**
/// （`grant` 和 `revoke` 共用一条分支，但它们是两个 op）。
///
/// 为什么要这一份：权限矩阵要能分清"认识的 op 但级别不够"和"根本不认识这个 op" ——
/// 前者是**权限**问题（说要多少级），后者是**拼错了**（要教对的名字）。
/// 把两件事混成一句，模型会拿"权限不够"去改自己的写法，然后在原地打转。
const KNOWN_OPS: [&str; 21] = [
    "ping", "barrier", "list", "index", "session", "prompts", "prompt", "grant", "revoke",
    "my_tools", "log", "host", "delegate", "unhost", "suspend", "resume", "end", "spawn",
    "stop", "shutdown",
    // **订阅**（2026-09-27）：它不是"发给中控的活"，而是"在订阅口那条连接上说的一句话" ——
    // 列在这儿是为了让"认识但不许"和"根本不认识"分得开（见下面那条拒的样子）。
    "subscribe",
];

/// **哪个 op 至少要多高的权威。**
///
/// 分三档，判据只有一句：**它会不会改系统状态或寿命**。
///
/// | 档 | op | 谁能发 |
/// |---|---|---|
/// | `observe` | `ping` `list` `index` `session` `prompts` | 任何活着的实例（看一眼不改什么）|
/// | `agent` | `my_tools` `barrier` `log` | 智能体（读自己的记录、请中控落盘、问自己被授权用什么）|
/// | `user` | 其余（`host` `unhost` `delegate` `suspend` `resume` `end` `spawn` `stop` `shutdown` `prompt` `grant` `revoke`）| **只有用户** |
///
/// ⚠️ `log` 在 `agent` 档，但一条"给我**全场**记录"的请求能读到**整场会话**。
/// 所以它还有第二道门：**低于 `user` 的调用方只能查自己**（见那一支的注释）。
fn op_min_authority(op: &str) -> Authority {
    match op {
        "ping" | "list" | "index" | "session" | "prompts" | "subscribe" => Authority::Observe,
        "my_tools" | "barrier" | "log" => Authority::Agent,
        _ => Authority::User,
    }
}

/// **从会话日志重建水位表**（每个 tag 铸到几号了）。
///
/// 它是**派生读模型**（同身份索引）：丢了能从日志算回来，所以**不该另存一份**。
/// 宽松读：读不出来的行直接跳过 —— 一条坏行只让水位**偏低一点**，
/// 而那种日志本来也重放不出会话（`log` 会明确拒）。
fn rebuild_id_water(
    log: &harness_core::session_log::SessionLog,
) -> std::collections::BTreeMap<String, u64> {
    let mut map = std::collections::BTreeMap::new();
    let Ok(text) = std::fs::read_to_string(log.log_path()) else {
        return map;
    };
    for line in text.lines() {
        let Ok(r) = serde_json::from_str::<harness_core::session_log::LogRecord>(line) else {
            continue;
        };
        for name in [Some(r.envelope.id.as_str()), r.envelope.corr.as_deref()] {
            if let Some((tag, n)) = name.and_then(harness_core::id::split_name) {
                let e = map.entry(tag.to_string()).or_insert(0u64);
                if n > *e {
                    *e = n;
                }
            }
        }
    }
    map
}

// ⚠️ **"把记录变成上下文"那两块不在这儿了**（2026-09-23 整块搬到 `harness-agent`）。
//
// 它们原来认三种事件（系统提示编译 / 模型条目 / 压缩手术），还替智能体重放压缩。
// 那是**中控在懂上下文的结构**，而上下文的结构是某个智能体实现的私事 —— 换一个框架、
// 换一种记忆策略它就变了，而中控不该为它改一行。
//
// 判据是"这份源码里不许再出现那三个标识符"，所以**这里故意不把它们写出来**
// （写出来这条判据就查不出来了）。要核就 `git grep` 那三个词。

/// **"这次调用到底能不能重试"** —— 中控替模型把它算出来（§19.2 偷来的那一条）。
///
/// 场景：服务在回答之前崩了 / 超时了 ⇒ 结果回 `unverifiable`（不知道做没做）。
/// 这时候**重试不重试是有正确答案的**，答案在工具自己的声明里：
///
/// | 声明 | 结论 |
/// |---|---|
/// | `readOnlyHint` / `idempotentHint` | **可以重试** —— 重复一次和做一次一样 |
/// | `destructiveHint` 且不幂等 | **别自动重试** —— 可能造成第二次副作用，先看状态 |
/// | 什么 Hint 都没读到（工具没报 / 不是它报的）| 按最保守的来：先说不知道，别自动重试 |
///
/// 这就是 §19.2 里"MQTT QoS 三档"那条的落地：**重试语义由声明决定，不由猜。**
/// （原来这里写的是"只读或幂等的可以重试，有副作用的先确认" ——
/// 中控手里就有那份声明，却把判断又推回给了模型。）
fn retry_advice(tool: Option<&str>, hints: Option<registry::ToolHints>) -> String {
    let t = tool.unwrap_or("（没写工具名）");
    match hints {
        Some(h) if h.read_only || h.idempotent => format!(
            "`{t}` 是{}的 ⇒ **可以重试**：重复一次和做一次一样。\
             重试时用同一个参数；要是它回 `busy` / `invalid`，那才是另一回事（按那条报错的补救来）",
            if h.read_only { "只读" } else { "幂等" }
        ),
        Some(h) if h.destructive => format!(
            "`{t}` **有副作用而且不幂等** ⇒ **别自动重试** —— 重试可能造成第二次副作用。\
             先看状态（读是自由的：`read` / `status` / `verify`），确认它到底做没做再决定；\
             拿不准就把决定交给用户"
        ),
        Some(_) => format!(
            "`{t}` 既不是只读也不是幂等（但工具没把它标成 destructive）⇒ \
             **先看状态再决定**，别盲目重试"
        ),
        None => format!(
            "手册里读不到 `{t}` 的 Hint（工具没报这一条）⇒ 按最保守的来：\
             **先看状态，别自动重试**"
        ),
    }
}

impl App {
    // ---- 出口 ----------------------------------------------------------

    /// 写 stdout 的那**唯一**一条路。**stdout 只写协议消息**（规范 §6.1）。
    fn print(&mut self, env: &Envelope) {
        match codec::encode(env) {
            Ok(line) => {
                let mut out = io::stdout();
                if let Err(e) = codec::write_line(&mut out, &line) {
                    eprintln!("[ctrl] 写 stdout 失败：{e}");
                }
                self.fanout(&line);
            }
            Err(e) => eprintln!("[ctrl] 自己的消息编码失败：{e}"),
        }
    }

    /// **把这一行也给订阅者抄一份**（2026-09-27）。
    ///
    /// 订阅者拿到的和 console 完全一样：**这条会话里路由过的每一条消息**。
    /// 没有过滤、没有裁剪 —— 今天就是这个语义（要"只看某类"是以后的事，
    /// 现在别声明一个做不到的筛子）。
    ///
    /// 写不进去 = **那个人走了**（transport 自己会说，不用等超时）：
    /// 当场把他从名单上摘掉，并留一笔 —— 不留一个永远失败的写。
    fn fanout(&mut self, line: &str) {
        if self.subs.is_empty() {
            return;
        }
        let mut dead: Vec<String> = Vec::new();
        for s in self.subs.iter_mut() {
            // 还没 `subscribe` 的：他没要过，别塞给他
            if s.id.is_none() {
                continue;
            }
            if s.conn.write_line(line).is_err() {
                dead.push(s.seq.clone());
            }
        }
        for seq in dead {
            self.on_sub_eof(&seq);
        }
    }

    /// 订阅口上来了一个人（**握手已经过了**，这儿的连接是可以直接写的）。
    fn on_subscribe(&mut self, seq: String, conn: harness_ipc::Conn) {
        eprintln!("[ctrl] 订阅口上来了一条连接（等它说 `subscribe` —— 在那之前它什么也收不到）");
        self.subs.push(Sub {
            seq,
            id: None,
            conn,
        });
    }

    /// 订阅口上的一行。
    ///
    /// 两条路，取决于他**说过没有**：
    ///
    /// | 状态 | 允许说什么 |
    /// |---|---|
    /// | 还没 `subscribe` | **只有** `{"op":"subscribe"}` —— 别的都拒（并教他那句话）|
    /// | 说过了 | 一条普通消息：**以他自己的身份**（`from` 必须是他自报的那个名字）|
    ///
    /// ⚠️ 第二条那句"必须以自己的身份"和通道那条是**同一条纪律**：
    /// **别在一个口子上冒充别人**（冒充了就是既不投递、又留痕）。
    /// ⚠️ 他发得出消息，但**动不了手**：他的上限是 `observe`，权限矩阵会挡下一切要更高档的活。
    /// "订阅者只读"就是这么来的 —— **不是一条新规矩，是一条已知的规矩**。
    fn on_sub_line(&mut self, seq: &str, line: &str) {
        let env = match codec::decode(line) {
            Ok(e) => e,
            Err(e) => {
                if let Some(s) = self.subs.iter().find(|s| s.seq == seq) {
                    let mut c = s.conn.try_clone().ok();
                    // 手上没有 `fault` 那种"发给别人"的方便：他连身份都还没说清，
                    // 就用他自己那条连接回一句（能读到就行）
                    if let Some(c) = c.as_mut() {
                        let _ = c.write_line(&format!(
                            "{{\"type\":\"event\",\"code\":\"invalid\",\"why\":{}}}",
                            serde_json::to_string(&format!("这一行读不出来：{e}")).unwrap_or_default()
                        ));
                    }
                }
                return;
            }
        };
        let Some(idx) = self.subs.iter().position(|s| s.seq == seq) else {
            return; // 已经下线了
        };
        if self.subs[idx].id.is_none() {
            // ---- 还没订阅：只受理 `subscribe` ----
            let op = env.payload.get("op").and_then(Value::as_str).unwrap_or("");
            if op != "subscribe" {
                self.subs[idx].id = None;
                let reply = self.mint(
                    MessageType::Result,
                    CTRL,
                    &env.from,
                    env.corr.clone(),
                    Authority::System,
                    json!({"route": null, "effect": "refused", "code": "refused",
                           "why": "这条连接还没订阅 —— 在订阅之前它只受理 `subscribe`",
                           "remedy": "先发 {\"op\":\"subscribe\"}（`from` 写你想用的名字，比如 gui:main）" }),
                );
                let _ = self.subs[idx].conn.write_line(&codec::encode(&reply).unwrap_or_default());
                return;
            }
            if env.from.trim().is_empty() {
                let reply = self.mint(
                    MessageType::Result, CTRL, &env.from, env.corr.clone(), Authority::System,
                    json!({"route": null, "effect": "refused", "code": "invalid",
                           "why": "`subscribe` 没写 `from` —— 订阅者也要有个名字",
                           "remedy": "`from` 写成你想用的实例名（比如 gui:main）" }),
                );
                let _ = self.subs[idx].conn.write_line(&codec::encode(&reply).unwrap_or_default());
                return;
            }
            // **登记成实例**：图上看得见"这个界面上来了"，而且它的上限是 `observe`。
            // （通道那条路也是这么登的 —— 于是"入口是个实例"这句话对订阅者也成立。）
            let id = env.from.clone();
            if self.reg.contains(&id) {
                let reply = self.mint(
                    MessageType::Result, CTRL, &env.from, env.corr.clone(), Authority::System,
                    json!({"route": null, "effect": "refused", "code": "refused",
                           "why": format!("`{id}` 这个名字已经被占着了"),
                           "remedy": "换一个名字再 `subscribe`" }),
                );
                let _ = self.subs[idx].conn.write_line(&codec::encode(&reply).unwrap_or_default());
                return;
            }
            self.reg.insert(Instance {
                id: id.clone(),
                kind: "channel".into(),
                is_agent: false,
                // **只读**：上限 `observe` —— 它想动手就会被权限矩阵挡下
                ceiling: Authority::Observe,
                state: "connected",
                service: None,
                manifest: None,
                config: None,
                generation: 0,
                bindings: None,
                owns: None,
                contract_errors: Vec::new(),
                contract_missing: Vec::new(),
                last_seen_ms: now_ms(),
                unresponsive: false,
                lifecycle: "resident".into(),
                lifecycle_declared: None,
            });
            self.subs[idx].id = Some(id.clone());
            let sid_now = self.sid.clone();
            self.index.append(&sid_now, &id, "channel", "subscribed",
                              json!({"ceiling": "observe", "note": "订阅者：只读"}));
            eprintln!("[ctrl] `{id}` 订阅了这条会话（只读：上限 observe）");
            let reply = self.mint(
                MessageType::Result, CTRL, &id, env.corr.clone(), Authority::System,
                json!({"route": null, "effect": "confirmed", "code": null,
                       "data": {"subscribed": true, "sid": sid_now,
                                "note": "接下来这条会话里路由过的每一条消息都会给你一份；\
                                         你发得出消息，但你的上限是 observe —— 动不了手"}}),
            );
            let _ = self.subs[idx].conn.write_line(&codec::encode(&reply).unwrap_or_default());
            return;
        }
        // ---- 已经订阅了：以他自己的身份走正常那条路 ----
        let mine = self.subs[idx].id.clone().unwrap_or_default();
        if env.from != mine {
            self.fault(
                &self.channel_id(),
                env.corr.clone(),
                "refused",
                format!("订阅口上的 `{mine}` 发了 from=`{}` 的消息", env.from),
                "在一个口子上别冒充别人；这条消息已被丢弃，没有投递",
            );
            return;
        }
        self.route(env);
    }

    /// 订阅口上的人走了。
    fn on_sub_eof(&mut self, seq: &str) {
        let Some(pos) = self.subs.iter().position(|s| s.seq == seq) else {
            return;
        };
        let sub = self.subs.remove(pos);
        let Some(id) = sub.id else {
            eprintln!("[ctrl] 订阅口上一条还没订阅的连接断了");
            return;
        };
        eprintln!("[ctrl] 订阅者 `{id}` 走了");
        if let Some(inst) = self.reg.get_mut(&id) {
            inst.state = "offline";
        }
        let sid_now = self.sid.clone();
        self.index
            .append(&sid_now, &id, "channel", "unsubscribed", json!({}));
    }

    fn mint(
        &mut self,
        kind: MessageType,
        from: &str,
        to: &str,
        corr: Option<String>,
        authority: Authority,
        payload: Value,
    ) -> Envelope {
        let env = Envelope {
            id: self.ids.next_message(),
            ts: now_ms(),
            kind,
            from: from.into(),
            to: to.into(),
            corr,
            sid: self.sid.clone(),
            authority,
            payload,
        };
        // 自己铸的也要记进水表（不然重启后中控自己的名字会重号）
        let id = env.id.clone();
        let corr = env.corr.clone();
        self.note_water(&id);
        if let Some(c) = corr {
            self.note_water(&c);
        }
        env
    }

    /// 把一个名字记进水位表（取最大值）。
    fn note_water(&mut self, name: &str) {
        if let Some((tag, n)) = harness_core::id::split_name(name) {
            let e = self.id_water.entry(tag.to_string()).or_insert(0);
            if n > *e {
                *e = n;
            }
        }
    }

    /// **这个实例该从几号开始铸名字**（拉起它的时候要用）。
    ///
    /// 查不到就是 0 = 从头铸 —— 这条边界是**故意的**：水位只覆盖"真的进过系统的号"。
    /// 一个进程铸了号但没发出去（崩了 / 被拒了），那个号在系统里不存在，
    /// 被重用无害。这正是"从日志算"天然的边界，不是需要额外小心的例外。
    fn water_for(&self, instance: &str) -> u64 {
        self.id_water
            .get(&harness_core::id::tag_of(instance))
            .copied()
            .unwrap_or(0)
    }

    /// **先记录，再投递。** 这是不变量二在代码里的唯一形态。
    fn record(&mut self, env: &Envelope) -> u64 {
        // **每一条进日志的消息都过这里** —— 水位表就在这儿长（它是派生读模型）
        self.note_water(&env.id);
        if let Some(c) = env.corr.clone() {
            self.note_water(&c);
        }
        match self.log.append(env) {
            Ok(seq) => {
                debug_assert!(invariant::assert_recorded_before_delivery(Some(seq), env).is_ok());
                seq
            }
            Err(e) => {
                eprintln!("[ctrl] 写会话日志失败：{e}");
                0
            }
        }
    }

    /// 中控自己发的通知（敕令的雏形）：`system` 级别，`event` 种类。
    fn system_event(&mut self, to: &str, payload: Value) -> Envelope {
        self.mint(
            MessageType::Event,
            CTRL,
            to,
            None,
            Authority::System,
            payload,
        )
    }

    /// 报错即手册（§5.5）：事实 + 影响 + 出口。**只有这三段齐了才发得出去。**
    ///
    /// `corr` 是**针对哪次调用**报的错 —— 能带就必须带。
    ///
    /// 不带 corr 的话，收件人只能靠猜这条报错对应它等着的哪一次调用
    /// （智能体就因此干等了 30 秒）。这是"配对"这条纪律在错误路径上的样子。
    ///
    /// ⚠️ **这一条发的是 `event`，所以它只配给"没人问过的话"用**
    /// （比如"你这条消息本身就是坏的"）。**回答一次请求必须用 `result`** ——
    /// 规范 §3 写得很直白：一次 `request` 以**恰好一个 `result`** 收尾。
    /// 拒绝也是一种回答。见 [`App::refuse_call`]。
    fn fault(
        &mut self,
        to: &str,
        corr: Option<String>,
        code: &str,
        why: impl Into<String>,
        remedy: impl Into<String>,
    ) {
        let env = self.mint(
            MessageType::Event,
            CTRL,
            to,
            corr,
            Authority::System,
            json!({"code": code, "why": why.into(), "remedy": remedy.into()}),
        );
        self.route(env);
    }

    /// **拒绝一次调用**：用 `result` 收尾，而不是 `event`。
    ///
    /// 为什么另起一个方法（2026-09-19，被 `smoke-contract` 当场抓出来的）：
    /// 原来所有的拒绝都走 [`App::fault`]，而它发的是 `event` ——
    /// 于是**每一条拒绝都违反了规范 §3**："一次 `request` 以恰好一个 `result` 收尾"。
    /// 当时的补救是在智能体里加了一条 `event` 分支去接它
    /// （代码注释还写着"没有这条分支，智能体会一直等一个永远不来的结果"）——
    /// **那是把协议的错误推给每个客户端各修一遍**。
    ///
    /// 现在形状也补齐了：`effect` / `code` / `route` 三样都在（§11.2），
    /// 于是同一条判词在智能体那边的渲染路径和别的失败**完全一样**。
    fn refuse_call(&mut self, req: &Envelope, code: &str, why: impl Into<String>, remedy: impl Into<String>) {
        let env = self.mint(
            MessageType::Result,
            CTRL,
            &req.from.clone(),
            req.corr.clone(),
            Authority::System,
            json!({
                "route": null,
                "effect": "refused",
                "code": code,
                "why": why.into(),
                "remedy": remedy.into(),
            }),
        );
        self.route(env);
    }

    fn reply(&mut self, req: &Envelope, payload: Value) {
        let env = self.mint(
            MessageType::Result,
            CTRL,
            &req.from.clone(),
            req.corr.clone(),
            Authority::System,
            payload,
        );
        // **回复也必须走 route。** 这里踩过三次同一个坑：
        // `record_and_print` 只是"记进日志 + 给控制台看"，**它不投递给收件人**。
        // 收件人一直是 console 的时候（而 console 就是读 stdout 的那个），
        // "打印"恰好等于"投递" —— 于是这个错被掩盖到收件人变成真实例为止。
        //
        // 所以现在**没有第二条出口**：要发东西给谁，一律 route。
        // 把那条"只打印"的路彻底删掉，比记住"别用它"更可靠。
        self.route(env);
    }

    // ---- 入站 ----------------------------------------------------------

    fn on_console_line(&mut self, line: &str) {
        let env = match codec::decode(line) {
            Ok(env) => env,
            Err(CodecError::Empty) => return,
            Err(e) => {
                self.fault(
                    &self.channel_id(),
                    None,
                    "invalid",
                    e.to_string(),
                    "信封是闭合的九字段（规范 §1）；空行会被忽略。可用 \
                     --help 看范式，或先发 {\"op\":\"ping\"} 试通路",
                );
                return;
            }
        };
        if env.from != self.channel_id() {
            self.fault(
                &self.channel_id(),
                env.corr.clone(),
                "refused",
                format!(
                    "这条通道只接受 from=`{}`，收到的是 `{}`",
                    self.channel.id, env.from
                ),
                "别在控制通道上冒充别的实例；要跟某个实例说话，就把它写在 `to` 里",
            );
            return;
        }
        self.route(env);
    }

    fn on_service_line(&mut self, instance: &str, line: &str) {
        let mut env = match codec::decode(line) {
            Ok(env) => env,
            Err(CodecError::Empty) => return,
            Err(e) => {
                self.fault(
                    &self.channel_id(),
                    None,
                    "invalid",
                    format!("服务 {instance} 发了一条不合规的消息：{e}"),
                    "这是服务侧的 bug；它的 stderr 里有上下文（规范 §6.1 规矩三）",
                );
                return;
            }
        };
        if env.from != instance {
            self.fault(
                &self.channel_id(),
                env.corr.clone(),
                "refused",
                format!("服务 {instance} 用自己的管道发了 from=`{}` 的消息", env.from),
                "服务只能以自己上报的实例 id 说话；这条消息已被丢弃，没有投递",
            );
            return;
        }
        if env.kind == MessageType::Presence {
            self.note_presence(&env);
        }
        // **听见它说话了** ⇒ 更新"最后听见它的时间"，并清掉"不应答"的标记。
        // 探针的回信走的也是这条路（它是一条 `result`）—— 不需要另开判断。
        self.reg.note_seen(instance, now_ms());
        // **实例事件**：工具实例的创建 / 状态 / 销毁（§5.10 那条"动态走事件流"）。
        // 在这之前这些事件也在流里，但中控认不出它们 —— 于是图上永远只有服务那一层。
        if env.kind == MessageType::Event {
            if let Some(e) = InstanceEvent::parse(&env.payload) {
                self.note_instance(instance, &e);
            }
        }
        // **投递前的闸门**：结果得合它自己声明过的形（规范-工具服务 §11.1 C 组）。
        //
        // ⚠️ **这里只"看"`pending`，不许"摘"。**（2026-09-20 修掉一个真 bug）
        //
        // 原来这里是 `self.pending.remove(corr)` —— 先摘、再 `route`。
        // 而 `route` 的方向规则四判的是"回给中控的结果得是中控要过的"，
        // **它查的正是 `pending`**。于是顺序一颠倒，**每一条回给中控的结果
        // 都被判成孤儿结果**：中控自己的健康探针，回包 100% 被丢掉，
        // 还每轮往日志和通道里灌一条假故障（smoke-lifecycle 里 18 发 18 中）。
        //
        // 教训和"落盘判断必须在投递那一刻算"是同一条：
        // **一张表要是被两处读，就得说清谁在什么时候动它** —— 摘账要等这条
        // 结果把整个 `route` 走完（判方向、投递、留痕），不能抢在它前面。
        let settling = if env.kind == MessageType::Result {
            env.corr.clone()
        } else {
            None
        };
        if let Some(corr) = &settling {
            if let Some(call) = self.pending.get(corr).cloned() {
                self.gate_result(&mut env, &call);
            }
        }
        self.route(env);
        // 走完才算这笔调用结束（没 `corr` 的结果本来就不在表里）
        if let Some(corr) = settling {
            self.pending.remove(&corr);
        }
    }

    /// **结果合形吗？**（第二道闸门）
    ///
    /// 为什么它必须在中控：§11.2 硬规则二 —— "**验证是独立的一次调用**"。
    /// 让服务自己检查自己，等于让被验证的人写自己的成绩单。
    /// 而中控查的是**机械事实**（字段在不在、枚举里有没有、类型对不对），
    /// **不碰任何工具语义** —— 这跟"内核不该懂每个工具的内部"不冲突。
    ///
    /// **判定不丢结果**：把问题附在结果上（`schema_violation`）照常投递。
    /// 丢掉它，调用方只会等到一个超时 —— 那比"形状不对"更糟，
    /// 因为**超时说不清是谁的错**。
    fn gate_result(&mut self, env: &mut Envelope, call: &PendingCall) {
        let tool = match call.tool.as_ref() {
            Some(t) => t.clone(),
            // 请求里没写工具名：服务会给它一个"没有名为(空)的工具"的
            // invalid，那条本身是可校验的，这里没有形状可对。
            None => return,
        };
        let manifest = self.reg.manifest_of(&call.target);
        // **没报过的工具名不算"声明缺失"。** 服务对不存在的工具回一条
        // `invalid` + "第一版还没做"，那是**正确**的回答（终端对 `send`
        // 就是这么答的）—— 拿"你没给这个工具声明结果形状"去指责它，
        // 是找错了人。真正要查的是：**它声明过的工具，回来的东西对不对。**
        let declared = manifest
            .as_ref()
            .map(|m| harness_service::contract::declares_tool(m, &tool))
            .unwrap_or(false);
        if !declared {
            return;
        }
        let schema = manifest
            .as_ref()
            .and_then(|m| harness_service::contract::tool_schema(m, &tool));
        let codes = self.reg.error_codes_of(&call.target);
        let problems = harness_service::contract::check_result(
            &call.target,
            &tool,
            &env.payload,
            schema.as_ref(),
            &codes,
        );
        if problems.is_empty() {
            return;
        }

        eprintln!(
            "[ctrl] {} 的 `{}` 结果不合它自己声明的形：{}",
            call.target,
            tool,
            problems.join("；")
        );
        // 先附再记 —— **日志里那条必须和投递出去的是同一条**（不变量二）。
        if let Some(obj) = env.payload.as_object_mut() {
            obj.insert(
                "schema_violation".into(),
                json!({
                    "instance": call.target,
                    "tool": tool,
                    "problems": problems,
                }),
            );
        }
        let ev = self.system_event(
            &self.channel_id(),
            json!({
                "code": "schema_violation",
                "instance": call.target,
                "tool": tool,
                "why": problems.join("；"),
                "remedy": "这是**服务侧**的问题：它给自己的结果声明错了形状，\
                           或者忘了回某个字段。结果**没有被丢掉** —— 上面附着 \
                           schema_violation，照常投递。别原样重试：先看看是哪个字段，\
                           要的话让中控换一个服务"
            }),
        );
        self.route(ev);
    }

    /// 把一条实例事件落到注册表上 —— **图上第三层就是这么长出来的**。
    fn note_instance(&mut self, service: &str, e: &InstanceEvent) {
        // **实例状态必须出自它自己声明的词表**（§11.7 第三条纪律）。
        //
        // 为什么中控要管这条：`state_vocabulary` 是**给图用的**（那个点该是什么颜色），
        // 报了一个没声明过的词，后端就只能瞎猜。这条检查是机械的
        // （在不在表里），所以它归中控 —— 跟契约闸门是同一条道理。
        if let Some(m) = self.reg.manifest_of(service) {
            let declared: Vec<&str> = m
                .get("state_vocabulary")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            if !declared.is_empty() && !declared.contains(&e.state.as_str()) {
                let key = format!("{service}/{}", e.state);
                if self.undeclared_states.insert(key) {
                    let ev = self.system_event(
                        &self.channel_id(),
                        json!({
                            "code": "instance_state_undeclared",
                            "instance": service,
                            "state": e.state,
                            "declared": declared,
                            "why": format!(
                                "`{service}` 报了一个它没声明过的实例状态 `{}`（它声明的是：{}）",
                                e.state,
                                declared.join(" / ")
                            ),
                            "remedy": "这是**服务侧**的问题：要么把那个状态加进 manifest 的 \
                                       `state_vocabulary`，要么别报它。图上那个点的颜色就靠这张表"
                        }),
                    );
                    self.route(ev);
                }
            }
        }
        // **索引**：第四层（工具实例）的每一次出现/变化/消失都留一笔。
        // 键用 `<服务>/<实例名>` —— 服务内部的名字只在自己域内唯一
        // （两个编辑器的第一个文档都叫 `ed-1`），全局唯一性靠这个前缀。
        let key = registry::tool_key(service, &e.instance);
        let kind = format!("{}:instance", harness_core::id::kind_of(service));
        let event = match e.action {
            harness_core::instance_event::Action::Open => "opened",
            harness_core::instance_event::Action::Close => "closed",
            harness_core::instance_event::Action::State => "state",
        };
        let sid = self.sid.clone();
        self.index.append(
            &sid,
            &key,
            &kind,
            event,
            json!({
                "service": service,
                "instance": e.instance,
                "state": e.state,
                "owner": e.owner,
                "path": e.path,
                "purpose": e.purpose,
            }),
        );
        match e.action {
            harness_core::instance_event::Action::Close => {
                let known = self.reg.remove_tool(service, &e.instance);
                if !known {
                    // 关一个本来就不在表里的实例：不致命，但要说出来 ——
                    // 它可能是"服务重启后重放旧事件"，也可能是服务记错了
                    eprintln!(
                        "[ctrl] {service} 说 `{}` 关了，但我这儿本来就没有它",
                        e.instance
                    );
                }
                eprintln!("[ctrl] 图：{service} 下的 `{}` 下线", e.instance);
            }
            _ => {
                let existed = self.reg.tool_count();
                self.reg.upsert_tool(service, e, now_ms());
                if self.reg.tool_count() > existed {
                    eprintln!(
                        "[ctrl] 图：{service} 下多了 `{}`（{}，{}）",
                        e.instance,
                        e.state,
                        e.purpose.clone().unwrap_or_else(|| "未说明".into())
                    );
                } else {
                    eprintln!("[ctrl] 图：{service}/{}=>{}", e.instance, e.state);
                }
            }
        }
    }

    fn note_presence(&mut self, env: &Envelope) {
        let state = env.payload.get("state").and_then(Value::as_str);
        let has_manifest = env.payload.get("manifest").is_some();
        let declared = env
            .payload
            .get("manifest")
            .and_then(registry::Owns::from_manifest);
        // **把 manifest 正文存下来** —— "工具自报家门"这条链的后半截
        let manifest_body = env.payload.get("manifest").cloned();
        // **实例配置**（智能体自报的那几位：内核 / 模型 / 人格）。
        // 中控**只存不问**：它不认识里面任何一样东西（同 L1 那条），
        // 存下来只是为了让"这个实例当初是怎么配的"在 `list` 和身份索引里查得到。
        let cfg_body = env.payload.get("config").cloned();
        // **注册那道闸门**：家门报全了吗？（规范 §11.1 A~E 组的契约）
        let report = manifest_body
            .as_ref()
            .map(harness_service::contract::check_manifest);
        // **它自述的拉起时机**（和表决定的那个并排显示 —— 不一致不是错，但要看得见）。
        // 先算出来：下面那段借的是 `get_mut`，不能再借注册表。
        let declared_lifecycle = manifest_body.as_ref().and_then(|m| {
            m.get("tools")?
                .as_array()?
                .iter()
                .find_map(|t| t.get("lifecycle").and_then(Value::as_str).map(str::to_string))
        });
        if let Some(inst) = self.reg.get_mut(&env.from) {
            inst.state = match state {
                Some("online") => "online",
                Some("offline") => "offline",
                Some("degraded") => "degraded",
                _ => inst.state,
            };
            if declared.is_some() {
                inst.owns = declared.clone();
            }
            if let Some(m) = manifest_body {
                inst.manifest = Some(m);
            }
            if let Some(c) = cfg_body.clone() {
                inst.config = Some(c);
            }
            if let Some(r) = &report {
                inst.contract_errors = r.errors.clone();
                inst.contract_missing = r.missing_optional.clone();
                // **家门不全 = 活着但接不了活。** 用的就是 §11.1 那个现成的状态词
                // `degraded`（"活着，但别指望它干活"）—— 中控再往下不猜一步：
                // 它照样在线、照样能 ping，但**有活要派给它时会被挡下**（见 route）。
                if !r.errors.is_empty() {
                    inst.state = "degraded";
                }
            }
            // **它自述的拉起时机**（和表决定的那个并排显示 —— 不一致不是错，但要看得见）
            inst.lifecycle_declared = declared_lifecycle.clone();
        }
        eprintln!(
            "[ctrl] {} 自报 state={:?}{}",
            env.from,
            state.unwrap_or("-"),
            if has_manifest { "（带 manifest）" } else { "" }
        );
        // **索引**：第三层（工具服务）报到的这一刻 —— 它是什么、报的什么状态
        {
            let sid = self.sid.clone();
            let m = self
                .reg
                .manifest_of(&env.from)
                .unwrap_or(Value::Null);
            self.index.append(
                &sid,
                &env.from,
                "service",
                "presence",
                json!({
                    "state": state.unwrap_or("unknown"),
                    "manifest_id": m.get("id"),
                    "manifest_version": m.get("version"),
                    "title": m.get("title"),
                    // 智能体才有：它自报的实例配置（内核 / 模型 / 人格）
                    "config": cfg_body.clone(),
                }),
            );
        }

        if let Some(r) = &report {
            if !r.errors.is_empty() {
                let missing = if r.missing_optional.is_empty() {
                    String::new()
                } else {
                    format!("；另外还差（可延后）：{}", r.missing_optional.join("；"))
                };
                let ev = self.system_event(
                    &self.channel_id(),
                    json!({
                        "code": "manifest_nonconforming",
                        "instance": env.from,
                        "why": r.summary(),
                        "problems": r.errors,
                        "missing_optional": r.missing_optional,
                        "remedy": format!(
                            "这是**服务侧**的问题：自报的家门不合规范 §11.1 的契约，\
                             中控没法照着它把工具交给别人（模型会照着缺的那几项猜参数）。\
                             改法是服务自己补齐再上线；在那之前它进 degraded，\
                             有活派给它时会被挡下{missing}"
                        ),
                    }),
                );
                self.route(ev);
            }
        }

        // **它报到了 ⇒ 把攒着的请求照序放行。**
        // 顺序要保住（先到先投），否则同一个服务的两条写操作可能倒过来执行。
        if let Some(q) = self.waiting_ready.remove(&env.from) {
            eprintln!("[ctrl] `{}` 报到了 —— 放行攒着的 {} 条请求", env.from, q.len());
            for item in q {
                // **只投递，不重跑路由** —— 记录和各项检查在第一次到达时已经做过了
                // （重跑 `route` 会把同一条请求记第二遍日志，这条踩过）
                self.dispatch(&item, true);
            }
        }

        // **注册是双向的。** 服务声明"我占了哪个独占域"，中控这边**核对并留痕** ——
        // 但**不仲裁**：挡住越界是服务自己的责任（02-工具 §5.15 第五节）。
        // 中控做的是审计（§12.6）—— 让"这个域有两个主"变成一个看得见的事实。
        if let Some(mine) = declared {
            let clashes: Vec<String> = self.reg.others_owning(&env.from, &mine).collect();
            if !clashes.is_empty() {
                let ev = self.system_event(
                    &self.channel_id(),
                    json!({
                        "code": "domain_conflict",
                        "why": format!(
                            "`{}` 声明的独占域 `{}`（按 {}）与这些实例重叠：{}",
                            env.from, mine.domain, mine.exclusive_per, clashes.join(", ")
                        ),
                        "remedy": "中控不仲裁这个 —— 谁该让路由服务自己决定；\
                                   正常的话后起的那个应当已经进了 degraded"
                    }),
                );
                self.route(ev);
            }
        }
    }

    fn on_service_exited(&mut self, instance: &str) {
        // **先把遗嘱读出来**（在把它从注册表里摘掉之前）。
        //
        // 这条顺序是实测撞出来的：第一版写在下面，读到的是 `None` ——
        // 因为那时实例已经被 `remove` 掉了，`manifest_of` 自然找不到它，
        // 于是每一次敕令都退化成那句"（它没登记遗嘱…）"的套话。
        // 教训和"先记录再投递"是同一个：**先把要用的东西取出来，再动状态。**
        let will = self
            .reg
            .manifest_of(instance)
            .and_then(|m| m.get("will").and_then(Value::as_str).map(str::to_string));
        // **它的实例跟着它一起没** —— 图上那几条连线是长在服务上的
        let orphans = self.reg.remove_tools_of(instance);
        if !orphans.is_empty() {
            eprintln!(
                "[ctrl] {instance} 没了，连带收掉它的 {} 个实例：{}",
                orphans.len(),
                orphans.join(", ")
            );
        }
        if let Some(mut inst) = self.reg.remove(instance) {
            if let Some(svc) = &mut inst.service {
                svc.shutdown();
            }
        }
        eprintln!("[ctrl] 服务 {instance} 退出了");
        {
            let sid = self.sid.clone();
            self.index.append(
                &sid,
                instance,
                "service",
                "exited",
                json!({"why": "进程没了（自己崩的，或者被 stop 收的）"}),
            );
        }
        {
            let sid = self.sid.clone();
            self.index
                .append(&sid, instance, "service", "exited", json!({}));
        }

        // §4.6 的第二种崩溃：服务被关停 → 等服务回来，**别重试**
        let ev = self.system_event(
            &self.channel_id(),
            json!({
                "code": "service_down",
                "instance": instance,
                "why": "这个服务进程已经不在了",
                "remedy": "别重试；要么等它回来（重启中控或 /spawn），要么换一条路"
            }),
        );
        self.route(ev);

        // **敕令**：工具集变了，得告诉"有上下文"的那些人（§4.4）。
        // 注意它**不改前缀** —— 提示词是编译一次冻住的，变化走 append-only，
        // 这正是当初讨论敕令的全部意义。
        let agents = self.reg.agent_ids();
        for agent in agents {
            let text = format!(
                "【敕令】工具 `{instance}` 已经下线了。\n\
                 - 事实：它从注册表里消失了，进程不在了。\n\
                 - 影响：{}你名单上的它现在调不动；已经发给它的调用我已经逐个结掉了。\n\
                 - 出口：用别的工具接着干，或者直说这件事现在做不了。**别重试它。**",
                match &will {
                    // 有遗嘱就照它说（那是服务的原话）
                    Some(w) => format!("{w} "),
                    // 没有就**照实说没有**，而不是编一句像模像样的
                    None => "（它没登记遗嘱，下面这句是套话：）".to_string(),
                }
            );
            let inject = self.mint(
                MessageType::Inject,
                CTRL,
                &agent,
                None,
                Authority::System,
                json!({"text": text, "reason": "service_down", "instance": instance}),
            );
            // 走 route 而不是直接写：这样它也过方向检查、也先落日志
            self.route(inject);
        }

        // 还在等它的那些调用，**逐个给一个诚实的交代** —— 不做静默丢弃
        let orphans: Vec<(String, PendingCall)> = self
            .pending
            .iter()
            .filter(|(_, p)| p.target == instance)
            .map(|(c, p)| {
                (
                    c.clone(),
                    PendingCall {
                        target: p.target.clone(),
                        caller: p.caller.clone(),
                        tool: p.tool.clone(),
                        started_ms: p.started_ms,
                    },
                )
            })
            .collect();
        for (corr, call) in orphans {
            let env = self.mint(
                MessageType::Result,
                CTRL,
                &call.caller,
                Some(corr.clone()),
                Authority::System,
                json!({
                    "effect": "unverifiable",
                    "code": "service_down",
                    "why": format!("{} 在回答之前就没了，所以**不知道它做没做**", call.target),
                    // **重试语义由声明决定，不由猜**（§19.2：MQTT 的 QoS 三档）。
                    // 那份 `annotations` 本来就在中控手里 —— 原来却把判断推回给了模型。
                    "remedy": retry_advice(
                        call.tool.as_deref(),
                        self.reg.tool_hints(&call.target, call.tool.as_deref().unwrap_or("")),
                    ),
                }),
            );
            // 同上：这一条是**给调用方**的，不是给控制台看的
            self.route(env);
            // ⚠️ **摘账要等它走完 `route`**（2026-09-20）。
            //
            // 这一条和 `on_service_line` 里那处是**同一个病**：方向规则四靠
            // `pending` 认"这条结果是不是中控要过的"。而探针的调用方**就是中控自己** ——
            // 于是这条 `ctrl → ctrl` 的交代，在"先摘后发"的写法下会被判成孤儿结果，
            // 反过来又生一条假故障（就是它把 smoke-lifecycle 从 0 条孤儿顶到 1 条）。
            self.pending.remove(&corr);
        }
    }

    // ---- 生命周期：周期 tick（探针 + 最后活动时间）------------------------

    /// **周期 tick**：探 `health` + 维护"最后活动时间"。
    ///
    /// **两件事都不做自动动作**，这是刻意的：
    ///
    /// * §5.20 定过"默认开着就是有用、不做自动回收"（回收只走四条确定路径）；
    /// * TCP keepalive 的先例也在这儿 —— 它默认关闭、阈值按用途分，
    ///   因为**它本来就不可靠**。我们更保守一层：探到了不做什么，探不到也不做什么，
    ///   只把"它最近没应答"变成一个**看得见的事实**（`list` 上一个字段 + 一条事件）。
    ///
    /// 这是"**中控负责管控，但不替服务做决定**"的又一个落点。
    fn tick(&mut self) {
        if self.tick.is_zero() {
            return;
        }
        let now = now_ms();
        let limit = (self.tick.as_millis() as i64) * 2;

        // ---- 0. 攒着等报到的请求：过了宽限还没报到就拒掉 ----
        let grace = self.startup_grace.as_millis() as i64;
        let expired: Vec<(String, Vec<Envelope>)> = self
            .waiting_ready
            .iter()
            .filter(|(_, q)| q.first().map(|q0| now - q0.since_ms > grace).unwrap_or(false))
            .map(|(svc, q)| {
                (
                    svc.clone(),
                    q.iter().map(|q0| q0.env.clone()).collect::<Vec<_>>(),
                )
            })
            .collect();
        for (svc, list) in expired {
            self.waiting_ready.remove(&svc);
            for e in list {
                // 顺序同 `on_service_line` / `on_service_exited`：**先交代，后摘账**。
                // 今天到不了这里（探针只在服务自报家门之后才发，不会进这条队列），
                // 但这是同一个坑 —— 三处写法必须一样，否则以后谁加一条
                // "中控自己排队的调用"，就又生一条假孤儿。
                self.refuse_call(
                    &e,
                    "service_down",
                    format!("`{svc}` 被拉起来了，但**一直没自报家门**（等了 {grace} ms）"),
                    "这不是你的用法问题：要么它启动失败了（看它的 stderr），\
                     要么它的 manifest 过不了注册关。发 `list` 看它的 state",
                );
                if let Some(corr) = &e.corr {
                    self.pending.remove(corr);
                }
            }
        }

        // ---- 1. 判超时：等了两轮还没回话的探针 ----
        let stale: Vec<(String, PendingCall)> = self
            .pending
            .iter()
            .filter(|(_, p)| p.caller == CTRL && p.tool.is_some())
            .filter(|(_, p)| now - p.started_ms > limit)
            .map(|(c, p)| {
                (
                    c.clone(),
                    PendingCall {
                        target: p.target.clone(),
                        caller: p.caller.clone(),
                        tool: p.tool.clone(),
                        started_ms: p.started_ms,
                    },
                )
            })
            .collect();
        for (corr, call) in stale {
            self.pending.remove(&corr);
            if self.reg.mark_unresponsive(&call.target) {
                let ev = self.system_event(
                    &self.channel_id(),
                    json!({
                        "code": "service_unresponsive",
                        "instance": call.target,
                        "why": format!(
                            "探 `health`（{}）等了 {limit} ms 没应答",
                            call.tool.clone().unwrap_or_default()
                        ),
                        "remedy": "**它不一定死了** —— 也可能正忙着（比如终端在跑一条长命令）。\
                                   中控不会因此收掉它（§5.20）。要看它在干什么发 `list`；\
                                   真不想要了就 `stop` 它"
                    }),
                );
                self.route(ev);
            }
        }

        // ---- 2. 给该探的服务发一针 ----
        // **不探智能体**：它的 Request 语义是"给我派活"，探针会被它当成一个空任务跑起来。
        // **不探忙着干活的**：有别人的未决调用指着它就别打扰 —— 那会把"忙"误判成"死"。
        let targets: Vec<String> = self
            .reg
            .ids()
            .filter(|id| {
                matches!(self.reg.get_ref(id), Some(i)
                    if !i.is_agent
                        && i.service.is_some()
                        && i.state != "offline"
                        && i.manifest.is_some())
            })
            .map(str::to_string)
            .collect();
        for id in targets {
            if self.pending.values().any(|p| p.target == id) {
                continue; // 有未决调用（别人的活，或者上一针还没回）
            }
            let tool = self.probe_tool(&id);
            let corr = format!("probe-{}", self.ids.next_corr());
            // ⚠️ **别在这儿手动记账**（2026-09-23 修）。
            //
            // 这里原来自己 `pending.insert` 一条，然后再 `route` —— 而 `route` 里
            // 那条"同一个 corr 不许重复"的检查**看到的正是它刚插进去的那条**，
            // 于是**每一针探针都被自己挡下**：服务永远收不到探针 ⇒ 全被判成
            // `unresponsive`（`smoke-lifecycle` 第 2 条当场红：三个服务全是 True）。
            // 现在**记账只有 `route` 一个地方**（它按请求里的 `tool` 和 `from` 记）——
            // 少一处插入，就少一处能跟它对不上的地方。
            let req = self.mint(
                MessageType::Request,
                CTRL,
                &id,
                Some(corr),
                Authority::System,
                json!({"tool": tool, "args": {}}),
            );
            self.route(req);
        }
    }

    /// 探针用哪个工具：**服务自己在 manifest 里声明的那个**（`health`）。
    /// 读不到就退回 `ping` —— 那是 §11.1 C 组给的默认形状。
    fn probe_tool(&self, id: &str) -> String {
        self.reg
            .manifest_of(id)
            .and_then(|m| {
                m.get("tools")?
                    .as_array()?
                    .iter()
                    .find_map(|t| t.get("health").and_then(Value::as_str).map(str::to_string))
            })
            .unwrap_or_else(|| "ping".into())
    }

    /// **这个服务自己声明了"怎么让它停下来"吗？**（manifest 里的 `stop` 字段）
    ///
    /// 形状和 `probe_tool` 一样：**服务声明入口，中控照着调**。
    /// 中控不认识"shutdown"这个词，它只认识"这个服务说收它要调哪个工具"。
    fn stop_entry_of(&self, id: &str) -> Option<String> {
        self.reg.manifest_of(id).and_then(|m| {
            m.get("tools")?
                .as_array()?
                .iter()
                .find_map(|t| t.get("stop").and_then(Value::as_str).map(str::to_string))
        })
    }

    /// **请一个服务自己停**：发一条请求就返回，**不等回信**。
    ///
    /// 为什么不在这儿等：中控是单线程的，回信要由主循环处理 ——
    /// 在这儿等它，就是**死锁**（自己等自己）。所以"先礼"只是把话递过去，
    /// 结果由 `on_service_exited` 那条老路收尾（收实例 / 结未决调用 / 发敕令）。
    fn request_service_stop(&mut self, id: &str, tool: &str) -> bool {
        let corr = format!("stop-{}", self.ids.next_corr());
        let env = self.mint(
            MessageType::Request,
            CTRL,
            id,
            Some(corr),
            Authority::System,
            json!({"tool": tool, "args": {}}),
        );
        self.route(env);
        true
    }

    // ---- 路由与仲裁 ------------------------------------------------------

    fn route(&mut self, env: Envelope) {
        // §1.1 只准降、不准升
        let ceiling = self.reg.ceiling(&env.from);
        if env.authority > ceiling {
            eprintln!(
                "[ctrl] 把 {} 的 authority 从 {} 降到 {}（能力上限）",
                env.from,
                env.authority.as_str(),
                ceiling.as_str()
            );
        }
        let mut env = env.clamped_to(ceiling);

        // **在记录之前盖章。**
        //
        // 顺序很要紧：日志里的那条**必须**和投递出去的是同一条。
        // 要是先记后附，重放时就看不出"当时它被授予了什么" ——
        // 而那恰好是排查"它为什么被拒"时要看的第一件东西。
        //
        // ⚠️ **2026-09-19 改成"每一个请求都盖章"**（原来只给智能体盖）。
        // 原来"字段不在"就等于"调用方不是智能体 ⇒ 全权"，于是有一个真洞：
        // **一条被降级的通道能全权指挥工具** —— `--channel 微信:agent` 转来的消息
        // 权威降到了 `agent`（只能提意见），可服务的 `Grant::from_payload` 看到
        // "没有 granted 字段"就放行 `proc.exec`。权威分级只改了信封上的字，
        // 没改它能不能用工具。现在按调用方身份盖，缺省值统一成**拒绝**。
        // **只给"发给工具服务"的请求盖章。** 发给智能体或中控自己的请求不需要：
        // 凭证是**工具那边要读的东西**（`Grant::from_payload`），
        // 盖在别处只是往日志里加噪音。
        let to_tool_service = env.to != CTRL && self.reg.bindings_of(&env.to).is_none();
        if env.kind == MessageType::Request && env.from != CTRL && to_tool_service {
            // **有人指名要一个实例 ⇒ 记下"最后活动时间"。**
            // 它只影响 `list` 上那个给人看的数，**不触发任何动作**（§5.20）。
            if let Some(iid) = env
                .payload
                .get("args")
                .and_then(|a| a.get("instance_id"))
                .and_then(Value::as_str)
            {
                self.reg.touch_tool(&env.to, iid, now_ms());
            }
            let who = env.from.clone();
            let (kind, caps) = match self.reg.bindings_of(&who) {
                // 智能体：能力来自**绑定**（没有对应绑定的话，下面的绑定检查会拒掉）
                Some(bound) => (
                    "agent",
                    bound
                        .iter()
                        .find(|b| b.instance == env.to)
                        .map(|b| b.capabilities.clone())
                        .unwrap_or_default(),
                ),
                // 用户那一侧（控制台 / 权威上限是 user 的通道）：全权
                None if env.authority >= Authority::User => ("user", vec!["*".to_string()]),
                // **别的一律零能力。** 一条被降级的通道，职责是把消息送进系统，
                // 不是自己动手 —— 它要干活，就让会话点名一个智能体去干。
                None => ("channel", Vec::new()),
            };
            if let Some(obj) = env.payload.as_object_mut() {
                obj.insert("granted".into(), json!(caps));
                obj.insert("caller_kind".into(), json!(kind));
            }
        }

        // 先记录，再投递（不变量二）
        let seq = self.record(&env);
        if seq == 0 {
            return; // 日志写不进去，宁可不投递
        }

        // console 自己的输入不回显给它；其余每一条都让它看得见
        if env.from != self.channel_id() {
            self.print(&env);
        }

        // §2 第二列：**方向**。违规的记进日志（发生过就要留痕），但**不投递**。
        //
        // **中控自己发起的调用**（健康探针）也在这条规则里：规则四说"回给中控的
        // result 必须是中控发起过的 corr"。原来这儿是个空的 `awaiting`，
        // 也就是说**中控一旦自己调工具，回信就会被当成孤儿结果挡下** ——
        // 所以那行当时留着 `TODO -- 未决`，直到 tick 让它第一次真的发生。
        // **2026-09-20 这件事查完并且修掉了**：把"我发出去还没回来的"如实填进去
        // （`pending` 里 caller = ctrl 的那些）。
        //
        // ⚠️ 而那次审计的真正教训在后面：**这个"填进去"当年是对的，可它被另一处
        // "抢先摘账"架空了** —— `on_service_line` 原来是**先 `pending.remove` 再 `route`**，
        // 于是每条探针回包都被判成孤儿结果，18 发 18 中（见 `on_service_line` 那段）。
        // 一句话：**一段代码"写对了"，不等于它那一刻手上的数据是对的。**
        let awaiting: Vec<String> = self
            .pending
            .iter()
            .filter(|(_, p)| p.caller == CTRL)
            .map(|(corr, _)| corr.clone())
            .collect();
        if let Err(v) = direction::check(
            &env,
            &DirectionContext {
                ctrl: CTRL,
                ctrl_awaiting: &awaiting,
            },
        ) {
            eprintln!("[ctrl] 方向违规：{}（{} → {}）", v.rule, env.from, env.to);
            self.fault(
                &self.channel_id(),
                env.corr.clone(),
                "refused",
                format!("{}：{}", v.rule, v.why),
                v.remedy,
            );
            return;
        }

        if env.to == "*" {
            self.refuse_call(
                &env,
                "invalid",
                "广播（to=\"*\"）的语义还没定（规范 §8 #4）",
                "显式写出收件人；等 #4 定了再支持广播",
            );
            return;
        }

        if env.to == CTRL {
            // 只有 **request** 才是"让中控做一件事"。presence / result / chunk
            // 发给中控是**汇报**，不是请求 —— 把它们当成 op 会让中控对着
            // 服务回一句没人问过的废话（第一次跑 smoke 就这么错了）。
            if env.kind == MessageType::Request {
                self.local_op(&env);
            }
            return;
        }

        if env.kind == MessageType::Request {
            match &env.corr {
                Some(corr) => {
                    // **同一个 `corr` 只能有一次在途调用。**
                    //
                    // 原来这里是 `pending.insert` —— 而 `BTreeMap::insert` 是**静默覆盖**：
                    // 第二次同 `corr` 的请求会顶掉第一次的账（谁发的、发给谁、调哪个工具），
                    // 于是第一次那条回包会被按**第二次的声明**去校验，还可能被投给错的人。
                    // 今天撞上的概率很低（`corr` 由调用方自己铸），但**这条规则本来就是
                    // 调用方的义务**：回包全靠 `corr` 认领，撞了就只能错认。
                    // 所以宁可明确拒，也不装看不见（规范 §3 / §8 #9）。
                    if let Some(prev) = self.pending.get(corr).cloned() {
                        self.refuse_call(
                            &env,
                            "refused",
                            format!(
                                "`{corr}` 已经有一次在途调用（{} → {}）",
                                prev.caller, prev.target
                            ),
                            "同一个 corr 只能有一次在途调用 —— 回包靠它认领，撞了就认错了。\
                             要再发一次就换一个 corr；要等结果就 `list` 看它还挂着没有",
                        );
                        return;
                    }
                    self.pending.insert(
                        corr.clone(),
                        PendingCall {
                            target: env.to.clone(),
                            caller: env.from.clone(),
                            tool: env
                                .payload
                                .get("tool")
                                .and_then(Value::as_str)
                                .map(str::to_string),
                            started_ms: now_ms(),
                        },
                    );
                }
                None => {
                    self.fault(
                        &env.from,
                        env.corr.clone(),
                        "invalid",
                        "这条 request 没有 corr",
                        "每次调用都要带一个 corr，否则结果回来时对不上（规范 §3）",
                    );
                    return;
                }
            }
        }

        // **按需拉起**（`lifecycle: per_call`）：表里有它、但它还没被拉起来 ⇒ 现在拉。
        //
        // 这一步接的是"**没报到就先攒着**"那条路：拉起来之后它还没自报家门，
        // 于是这条请求（以及后续的）照常攒着，等它报到再照序放行。
        // 两条机制是天生一对 —— 一条管"什么时候拉"，一条管"拉起来到报到之间怎么办"。
        if env.kind == MessageType::Request && !self.reg.contains(&env.to) {
            let lazy = self
                .specs
                .iter()
                .find(|s| s.id.as_deref() == Some(env.to.as_str()))
                .filter(|s| s.lifecycle.as_deref() == Some("per_call"))
                .cloned();
            if let Some(spec) = lazy {
                eprintln!(
                    "[ctrl] 按需拉起 {}（per_call：第一次用到它才起）",
                    env.to
                );
                let tx = self.tx.clone();
                if let Err(e) = self.spawn_spec(&spec, tx) {
                    self.refuse_call(
                        &env,
                        "service_down",
                        format!("按需拉起 `{}` 失败：{e}", env.to),
                        "它在服务表里声明了 `lifecycle: per_call`，但现在起不来；\
                         看中控的 stderr，或者把它改成 per_session 让它开会话时就起来",
                    );
                    return;
                }
            }
        }

        if !self.reg.contains(&env.to) {
            if env.kind == MessageType::Request {
                if let Some(corr) = &env.corr {
                    self.pending.remove(corr);
                }
            }
            let known = self.reg.ids().collect::<Vec<_>>().join(", ");
            self.refuse_call(
                &env,
                "instance_gone",
                format!("没有实例 `{}`", env.to),
                format!(
                    "现有实例：{known}；发 {{\"op\":\"list\"}} 给 ctrl 可以随时查"
                ),
            );
            return;
        }

        // **绑定检查**（§2.3）：智能体只能调它被授权的东西。

        // **注册闸门的后果**：家门不合规的实例**不接活**（规范 §11.3：
        // "字段不合规，中控就是接不上"）。
        //
        // 为什么要挡在调用之前、而不是等它回个烂结果：它的 manifest 是
        // **模型填参数的唯一依据** —— 缺了 `input_schema`，模型只能猜，
        // 而"猜"产生的是一次假调用。挡在这里，代价是一句说清原因的拒绝。
        if env.kind == MessageType::Request && env.to != CTRL {
            let problems = self.reg.contract_errors_of(&env.to);
            if !problems.is_empty() {
                let target = env.to.clone();
                self.refuse_call(
                    &env,
                    "refused",
                    format!(
                        "`{target}` 自报的家门不合规，中控不给它派活：{}",
                        problems.join("；")
                    ),
                    "这是**服务侧**的问题（规范 §11.1 的契约），不是你的用法问题。\
                     别换个说法重试；要么等它补齐 manifest 重新上线，\
                     要么让用户换一个能干这件事的实例",
                );
                return;
            }
        }
        //
        // 这是"够得着"那一半。之前路由只看"那个实例在不在"，
        // 于是只要有名字就能调 —— 探针实测过：名片里一个工具都没有的智能体，
        // 照样把 `term:1 open` 调成了（那条终端的用途字段上写着
        // "我没被授权，但我还是试了"）。
        if env.kind == MessageType::Request && env.from != CTRL {
            if let Some(bound) = self.reg.bindings_of(&env.from) {
                let hit = bound.iter().find(|b| b.instance == env.to).cloned();
                if hit.is_none() {
                    let target = env.to.clone();
                    self.refuse_call(
                        &env,
                        "refused",
                        format!("你没有使用 `{target}` 的授权"),
                        format!(
                            "你被授权的是：{}。要更多，得让中控给你签发（§2.3）；\
                             别去猜别的实例名 —— 那不是你能决定的",
                            if bound.is_empty() {
                                "（什么都没有）".to_string()
                            } else {
                                bound
                                    .iter()
                                    .map(|b| b.instance.clone())
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            }
                        ),
                    );
                    return;
                }
                // **范围那一半**：光"这个实例被授权"还不够，
                // 它管的那条路径也得落在**本会话托管的范围**里。
                //
                // 少了这一条，"授权"就只是实例名白名单：一个绑在别的项目上的
                // 终端实例，照样能被本会话的智能体使唤 —— 而它眼里压根不是这个项目。
                let fence = self.reg.fence_of(&env.to);
                if !fence.is_empty() {
                    let hosts: Vec<String> = self
                        .session
                        .hosts
                        .iter()
                        .filter_map(|h| h.get("path").and_then(Value::as_str))
                        .map(str::to_string)
                        .collect();
                    // ⚠️ **托管是必须的**（2026-09-27）。
                    //
                    // 原来这里有一条短路：`if !hosts.is_empty() && ...` ——
                    // 也就是**本会话什么都没托管的时候，范围检查直接跳过**。
                    // 那意味着"不托管 = 放弃会话范围这一层"：一个空会话可以随便使唤
                    // 别处（别的项目）的工具。而这套设计里，"用户托管了什么"
                    // **是会话唯一的范围来源** —— 服务表里那串 `--fence` 是**部署**
                    // 写的，不是用户在这次会话里交出来的。
                    //
                    // 所以：**要用带围栏的实例，就得先托管**（哪怕只托管一条）。
                    // 不带围栏的那些（`echo` 之类）照常，不受这条影响。
                    if hosts.is_empty() {
                        let target = env.to.clone();
                        self.refuse_call(
                            &env,
                            "refused",
                            format!(
                                "`{target}` 管着一条路径，而**本会话什么都没托管** —— \
                                 托管是「你能碰什么范围」的声明"
                            ),
                            "先 `{\"op\":\"host\",\"path\":\"…\"}` 把要用的那条路径托管进来\
                             （托管会立刻重新装配那些要围栏的服务）；\
                             或者换一个已经托管过路径的会话",
                        );
                        return;
                    }
                    let outside: Vec<&String> =
                        fence.iter().filter(|f| !hosts.contains(f)).collect();
                    if !hosts.is_empty() && outside.len() == fence.len() {
                        let target = env.to.clone();
                        self.refuse_call(
                            &env,
                            "refused",
                            format!(
                                "`{target}` 管的那条路径不在本会话托管的范围内（它管 {}）",
                                outside
                                    .iter()
                                    .map(|s| s.as_str())
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            ),
                            format!(
                                "本会话托管的是：{}。要用另一个项目的工具，\
                                 要么在这个会话里 host 它，要么开一个新会话",
                                hosts.join(", ")
                            ),
                        );
                        return;
                    }
                }
            }
        }

        // 投递给服务：这是**唯一**碰服务 stdin 的地方
        //
        // 但在写下去之前还有一道关 —— **实例独属**（§5.10 的办公软件那张表）。
        //
        // 规矩一句话：**智能体之间互不越界，用户不受限。**
        // 为什么它归中控：`谁开着哪个实例` 是一张**图上的事实**，
        // 服务只看得到自己那一个域，只有中控看得全 —— 所以这条检查在这儿
        // **只写一次**，第三方服务不必各自实现一遍（终端的 `read` 原来就没有）。
        // 服务那边如果自己也查一道，算纵深（编辑器一直有）。
        if env.kind == MessageType::Request && self.reg.bindings_of(&env.from).is_some() {
            if let Some(iid) = env
                .payload
                .get("args")
                .and_then(|a| a.get("instance_id"))
                .and_then(Value::as_str)
            {
                let mine = format!("{}@{}", env.from, env.sid);
                if let Some(owner) = self.reg.tool_owner(&env.to, iid) {
                    if owner != mine {
                        let target = env.to.clone();
                        let iid = iid.to_string();
                        let caller = env.from.clone();
                        self.refuse_call(
                            &env,
                            "refused",
                            format!(
                                "`{target}/{iid}` 不是你的实例 —— 它是 `{owner}` 开的"
                            ),
                            format!(
                                "**同一个域可以各开一个**，互不影响：你自己 open 一个就行。\
                                 别在别人的实例上动手 —— 它的状态（cwd / 游标 / 正在跑的命令）\
                                 都是人家的。现在这个 `{iid}` 归 `{owner}`，你是 `{caller}`@{}",
                                env.sid
                            ),
                        );
                        return;
                    }
                }
            }
        }

        let line = match codec::encode(&env) {
            Ok(l) => l,
            Err(e) => {
                self.refuse_call(&env, "invalid", e.to_string(), "重发一条合法的消息");
                return;
            }
        };
        // **1. 它自报家门了吗？** 没有就先攒着 —— 请求不能抢在注册关前面。
        //
        // 这一条是 smoke-editor 抓出来的：脚本一次性把 NDJSON 灌进来，
        // 而服务的 presence 还要几十毫秒才到 —— 于是**契约闸门还没看过它的 manifest，
        // 请求就已经递到它手上了**（顺带"destructive 的调用要先落盘"也判不出来）。
        // 攒着、报到后照序放行，比"拒掉"友好，也比"放过去"老实。
        //
        // ⚠️ **队列里放的是"要投的那一行 + 要不要落盘"，不是信封本身。**
        // 第一版放信封、放行时重新 `route` 一遍 —— 于是**同一条请求被记了两次日志**
        // （smoke-stdio 的"console 输入 9 条"当场变成 13 条抓到的）。
        // 现在记录与各项检查都只在**第一次到达**时做，队列只负责**把投递推迟**。
        //
        // ⚠️ **只对工具服务用这条**：智能体的 presence 里没有 manifest
        // （它不自报家门，它不是工具）—— 拿"有没有 manifest"去卡它，
        // 会把发给智能体的任务无限期攒着。
        if env.kind == MessageType::Request {
            let seen = match self.reg.get_ref(&env.to) {
                Some(i) if !i.is_agent && i.service.is_some() => i.manifest.is_some(),
                _ => true,
            };
            // ⚠️ **这条队列是"报到"排空的，不是 tick 排空的**（2026-09-26 更正）。
            //
            // 我一度在这里加了"关了 tick 就当场拒" —— **那是错的**，而且当场
            // 把五份验收（editor / pager / spill / parallel / lifecycle-field）判红了：
            // 它们在启动瞬间就发请求，那些请求正是靠**服务报到那一刻**照序放行的
            // （见 `note_presence` 里 `waiting_ready.remove`）。tick 只管**宽限到点
            // 还没报到**的那条兜底（把它逐个拒掉）——关掉 tick 只是少了兜底，
            // 不是"队列没人管"。**同一个队列有两个出口，我当时只看见了一个。**
            if !seen {
                if self.debug_barriers {
                    eprintln!("[ctrl] `{}` 还没自报家门 —— 这条请求先攒着", env.to);
                }
                self.waiting_ready
                    .entry(env.to.clone())
                    .or_default()
                    .push(Queued {
                        env,
                        line,
                        since_ms: now_ms(),
                    });
                return;
            }
        }

        // **2. 投出去。**（屏障也在这一步落 —— 攒着的那条还没到服务手上，早落是白落）
        self.dispatch(
            &Queued {
                env,
                line,
                since_ms: now_ms(),
            },
            false,
        );
    }

    /// **真正把一行投给服务。** 屏障也在这儿落。
    ///
    /// 抽出来是为了"攒着的请求放行"和"立刻投递"**走同一条路** ——
    /// 少一份实现，就少一处漏（这条纪律在这份代码里已经兑现过好几次了）。
    fn dispatch(&mut self, q: &Queued, deferred: bool) {
        // **落盘屏障：有副作用的工具，分派之前先落一次盘**（§5.5 三个边界里的第二个）。
        //
        // 它在防什么：**"日志说没做、文件其实改了"**。请求已经 append 进日志，
        // 但它还在写缓冲里；这时服务把文件改了、机器崩了 ——
        // 重启之后看日志，那条写请求不见了，于是没人知道文件为什么变了。
        // 先 `flush + fsync`，再让它动手，这个洞就不存在了。
        //
        // 判据是**服务自己声明的 `destructiveHint`**（§11.1 C 组）：
        // 中控不必懂工具语义，也知道"这一次要不要先落盘"。读不到声明就**保守落一次**。
        //
        // ⚠️ **这个判断必须在投递这一刻做，不能在排队那一刻做** ——
        // 排队时它多半还没自报家门，那时算出来必然是"读不到声明"，
        // 于是每一次调用都会保守落盘（连 `read` 都落）。这条是 smoke-editor 抓出来的。
        // ⚠️ **只对"发给工具服务"的请求判**（2026-09-20）。
        //
        // 原来这里只看"是不是 request"，于是**给智能体派一个活**也会走进同一段：
        // `tool_hints("agent:dada", "")` 当然读不到声明 ⇒ 走"保守落一次"那条分支，
        // 白 fsync 一次，还在 stderr 上印一句会误导人的
        // "读不到它的声明（tool=）"——**智能体本来就不是工具，没有声明可读**。
        //
        // 这条守的是"文件会不会被动"：一次派活本身不动任何文件。
        // 判据改得跟 `route` 里盖章那处**同一个**（`to_tool_service`），
        // 免得同一件事在第二个地方算出第二个答案。
        // ⚠️ **智能体也是"被拉起来的服务进程"**（`service.is_some()` 对它同样为真）——
        // 所以判据必须跟 `route` 那道注册关一样带上 `!is_agent`，
        // 否则"派活给智能体"照样会走进来。这是这条判据**第一次写就写错**的地方，
        // 是 smoke-barrier 第 9 条当场抓到的。
        let to_tool_service = self
            .reg
            .get_ref(&q.env.to)
            .map(|i| !i.is_agent && i.service.is_some())
            .unwrap_or(false);
        let (barrier, why) = if q.env.kind == MessageType::Request && to_tool_service {
            let tool = q
                .env
                .payload
                .get("tool")
                .and_then(Value::as_str)
                .unwrap_or("");
            match self.reg.tool_hints(&q.env.to, tool) {
                Some(h) if h.destructive => (true, format!("它声明了 destructiveHint（tool={tool}）")),
                Some(_) => (false, String::new()),
                None => (
                    true,
                    format!("**读不到它的声明**（tool={tool}）—— 保守起见落一次"),
                ),
            }
        } else {
            (false, String::new())
        };
        if barrier {
            match self.log.barrier() {
                // `synced` 为假 = 本来就干净、这次短路了 —— **不是失败**
                Ok(synced) => {
                    if self.debug_barriers || why.contains("读不到") {
                        eprintln!(
                            "[ctrl] 屏障：分派前先落盘（{}，因为{}{}）{}",
                            q.env.to,
                            why,
                            if deferred { "；这条攒到它报到之后才投" } else { "" },
                            if synced { "" } else { " —— 日志本来就干净，这次没 fsync" }
                        );
                    }
                }
                Err(e) => eprintln!(
                    "[ctrl] 分派前落盘失败（{e}）—— 这一次仍然投递，但审计会少一条"
                ),
            }
        }
        let target = q.env.to.clone();
        let wrote = match self.reg.get_mut(&target).and_then(|i| i.service.as_mut()) {
            Some(svc) => svc.write(&q.line).is_ok(),
            None => true, // 不是服务级实例（例如未来的 GUI 订阅者）—— 记下就算投递了
        };
        if !wrote {
            if let Some(corr) = &q.env.corr {
                self.pending.remove(corr);
            }
            self.refuse_call(
                &q.env,
                "service_down",
                format!("往 {target} 写不进去"),
                "它的 stdin 已经关了；这条消息已经记进日志，但没有送达",
            );
        }
    }

    fn local_op(&mut self, req: &Envelope) {
        let op = req.payload.get("op").and_then(Value::as_str).unwrap_or("");
        // **权限矩阵**（规范-消息协议 §2.7）。它管的是"这个 op 至少要多高的权威"。
        //
        // 为什么必须有这一关：原来 19 个 op 里**只有 2 个**查了 authority
        // （`prompt` 和 `grant|revoke`），其余 17 个一律不查 ——
        // 而"能往中控发 request 的实例"包括**工具服务**（它们的权威上限是 `observe`）。
        // 于是任何一个服务都能 `spawn` 任意可执行文件、`stop` 别人的服务、
        // `shutdown` 整个中控。今天七个服务一个 op 都没发过，所以收紧不伤任何人 ——
        // **"没人用"就是定规矩最便宜的时候。**
        if KNOWN_OPS.contains(&op) {
            let need = op_min_authority(op);
            if req.authority < need {
                self.reply(
                    req,
                    json!({
                        "effect": "refused", "code": "refused",
                        "why": format!(
                            "`{op}` 要 `{}` 级别，这条消息是 `{}` 级别",
                            need.as_str(), req.authority.as_str()
                        ),
                        "remedy": format!(
                            "这条 op 会改系统状态或寿命，只有 {} 能发。\
                             你这边的级别由**中控签发的上限**决定（工具是 `observe`、智能体是 `agent`）——\
                             不是你写多高就多高（§1.1 只准降不准升）。{}",
                            need.as_str(),
                            // 改授权那两条**格外**不能松：让智能体（或某个工具）给自己加权限，
                            // 是这套模型里最不能开的口子 —— 一松，"默认拒绝的绑定"（§5.10）就成了空话。
                            if matches!(op, "grant" | "revoke") {
                                "**智能体给自己加权限是不允许的** —— 那不是它能决定的事"
                            } else {
                                ""
                            }
                        )
                    }),
                );
                return;
            }
        }
        match op {
            "ping" => {
                let payload = json!({
                    "effect": "confirmed",
                    "code": null,
                    "data": {"pong": true, "last_seq": self.log.last_seq(), "sid": self.sid}
                });
                self.reply(req, payload);
            }
            // **落盘屏障**：让**别的进程**也能说"先把这个回合记下来"（规范-消息协议 §5.5 #8）。
            //
            // 为什么要有这个 op：会话日志是**中控独占**的（只有它写、只有它 fsync），
            // 而"模型请求前"这个边界发生在**智能体**进程里 —— 它手里没有那支笔，
            // 但它比谁都清楚"我要去问模型了"。所以边界归它判，动作归中控做。
            //
            // 它防的是什么：模型是在**中控日志之外**跑的一次远程调用，而模型看到的
            // 那段上下文是**从日志里重建的**（不变量二）。日志要是还压在缓冲里，
            // "模型见过什么"这件事就没有落盘的凭证 —— 崩在这中间，
            // 重放出来的历史会和模型当时看到的**不是同一份**。
            //
            // `reason` 是调用方自己写的，中控**原话打出来**（跟遗嘱同一条纪律：
            // 谁干的谁说明，中控不替它编）。
            "barrier" => {
                let reason = req
                    .payload
                    .get("reason")
                    .and_then(Value::as_str)
                    .unwrap_or("（调用方没写原因）")
                    .to_string();
                match self.log.barrier() {
                    Ok(synced) => {
                        // 正常路径不出声（跟"分派前"那一处同一条纪律）：
                        // 模型一回合要走好几步，默认每步一行就成了噪音。
                        // 机制是不是真的发生了，看**日志里那条请求和它的答复**，
                        // 要看实时痕迹就开 `--debug-barriers`。
                        if self.debug_barriers {
                            eprintln!(
                                "[ctrl] 屏障：{reason}（{}，synced={}，seq={}）",
                                req.from,
                                synced,
                                self.log.last_seq()
                            );
                        }
                        self.reply(
                            req,
                            json!({"effect": "confirmed", "code": null,
                                   "data": {"synced": synced, "last_seq": self.log.last_seq(),
                                            "reason": reason}}),
                        );
                    }
                    // **落不下去要明说。** 这条不是"小事"：它意味着"已记录"这个前提
                    // 不成立了 —— 调用方该按 `unverifiable` 处理，而不是当成功继续跑。
                    Err(e) => self.reply(
                        req,
                        json!({"effect": "unverifiable", "code": "refused",
                               "why": format!("日志落盘失败：{e}"),
                               "remedy": "这一下**没落成** —— 你接下来干的事将没有落盘凭证。\
                                          先把结果告诉用户，或者查磁盘；别当成功继续"}),
                    ),
                }
            }
            "list" => {
                let data = self.reg.snapshot();
                self.reply(
                    req,
                    json!({"effect": "confirmed", "code": null, "data": data}),
                );
            }
            // **跨会话的身份账**：`id` 是什么、归谁、什么时候出现过。
            //
            // 和 `list` 的区别要说清楚：`list` 是**此刻的图**（内存里的注册表），
            // 这个 op 是**历史**（`data/index/instances.jsonl`，跨会话、只追加）。
            // 前者答"现在有什么"，后者答"这个 id 是谁"。
            "index" => {
                let id = req.payload.get("id").and_then(Value::as_str);
                let sid_filter = req.payload.get("sid").and_then(Value::as_str);
                // 默认只给最后 50 条 —— 索引会一直长，倒垃圾式的全量返回没意义
                let limit = req
                    .payload
                    .get("limit")
                    .and_then(Value::as_u64)
                    .unwrap_or(50) as usize;
                match index::IndexLog::read(&self.data_dir, id, sid_filter, limit) {
                    Ok((records, total)) => {
                        let chain = index::chain_of(&records);
                        self.reply(
                            req,
                            json!({
                                "effect": "confirmed", "code": null,
                                "data": {
                                    "file": index::FILE,
                                    "records": records,
                                    "matched": total,
                                    "shown": limit,
                                    // 查一个实例时附上那条链（会话 → 智能体 → 实例）
                                    "chain": chain,
                                }
                            }),
                        );
                    }
                    Err(e) => self.reply(
                        req,
                        json!({
                            "effect": "refused", "code": "invalid",
                            "why": format!("读不出索引：{e}"),
                            "remedy": "索引是派生数据（只追加、跨会话）—— \
                                       没有它会话照样跑；要重建就得从各会话的日志里扫"
                        }),
                    ),
                }
            }
            // ---- 会话：图上最上面那一层 ----
            "session" => {
                let snap = self.session.snapshot();
                self.reply(
                    req,
                    json!({"effect": "confirmed", "code": null, "data": snap}),
                );
            }
            // **"我被授权用什么"** —— 智能体问的是这个，不是"世界有什么"。
            //
            // 区别很重要：前者由中控签发（§2.3），后者是把整个注册表交出去。
            // 工具定义（参数表）来自**服务自己的 manifest** —— 这样
            // "服务报家门"和"模型看到的工具"是同一份真相。
            // **看提示词库里有什么**（2026-09-20）。
            //
            // 分成两个 op 是有意的：**看**（`prompts`，只读）和**用**（`prompt`，要 user 级别）。
            // 合成一个的话，"看一眼有什么"就变成了一个能改上下文权限的动作。
            "prompts" => {
                let data = match &self.prompts {
                    None => json!({
                        "dir": Value::Null,
                        "fragments": [],
                        "note": "这次会话没有挂提示词库 —— 开会话时用 `--prompts <目录>` 挂一个"
                    }),
                    // **重读**：和 `prompt` op 一致 —— 两处报的必须是同一份
                    Some(lib) => { let lib = lib.now(); json!({
                        "dir": lib.dir.display().to_string(),
                        "fragments": lib.frags.iter().map(|f| json!({
                            "name": f.name,
                            "version": f.version,
                            "bytes": f.text.len(),
                            // **看得到长度，看不到正文**：正文要 `prompt` 才注入
                            // （"看"和"念给模型听"是两件事）
                        })).collect::<Vec<_>>(),
                        // **坏的不许藏**：少了一段和"那一段坏了"是两件事
                        "skipped": lib.skipped.iter().map(|(n, why)| json!({
                            "name": n, "why": why
                        })).collect::<Vec<_>>(),
                    }) }
                };
                self.reply(req, json!({"effect": "confirmed", "code": null, "data": data}));
            }
            // **把提示词片段注入某个智能体的上下文**（2026-09-20，§3 的"提示词实例"）。
            //
            // 为什么它是 `user` 级别：它写的是**规矩** —— 跟改授权同一类动作。
            // 让智能体或工具能给自己灌指令，这套模型里最不能开的口子之一。
            //
            // 为什么走 `inject`：前缀是**字节冻结**的（§15.2），要加东西只能 append-only。
            // 所以注入 = 一条敕令形状的通知，**不重编前缀**（判据就盯这个）。
            "prompt" => {
                if req.authority < Authority::User {
                    self.reply(
                        req,
                        json!({"effect": "refused", "code": "refused",
                               "why": format!(
                                   "注入提示词要 `user` 级别，这条消息是 `{}` 级别",
                                   req.authority.as_str()),
                               "remedy": "提示词是**规矩**，只有用户能下 —— \
                                          智能体或通道给自己灌指令是不允许的。\
                                          要那样做，先让用户把这段话写进提示词库"}),
                    );
                    return;
                }
                let agent = req
                    .payload
                    .get("agent")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                if agent.is_empty() {
                    self.reply(
                        req,
                        json!({"effect": "refused", "code": "invalid",
                               "why": "`prompt` 需要 `agent`",
                               "expect": {"op": "prompt", "agent": "agent:dada",
                                          "fragments": ["coding-rules"]},
                               "remedy": "说清念给谁听；`list` 能看到现在有哪些智能体"}),
                    );
                    return;
                }
                if !self.reg.get_ref(&agent).map(|i| i.is_agent).unwrap_or(false) {
                    let agents: Vec<String> = self
                        .reg
                        .ids()
                        .filter(|id| self.reg.get_ref(id).map(|i| i.is_agent).unwrap_or(false))
                        .map(str::to_string)
                        .collect();
                    self.reply(
                        req,
                        json!({"effect": "refused", "code": "refused",
                               "why": format!("`{agent}` 不是这次会话里的智能体"),
                               "remedy": format!(
                                   "现在有：{}",
                                   if agents.is_empty() {
                                       "（一个都没有）".to_string()
                                   } else {
                                       agents.join(" / ")
                                   })}),
                    );
                    return;
                }
                let wanted: Vec<String> = req
                    .payload
                    .get("fragments")
                    .and_then(Value::as_array)
                    .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
                    .unwrap_or_default();
                if wanted.is_empty() {
                    self.reply(
                        req,
                        json!({"effect": "refused", "code": "invalid",
                               "why": "`prompt` 需要 `fragments`（要念哪几段）",
                               "remedy": "先 `prompts` 看一眼有哪些，再点名"}),
                    );
                    return;
                }
                let Some(lib) = self.prompts.clone() else {
                    self.reply(
                        req,
                        json!({"effect": "refused", "code": "invalid",
                               "why": "这次会话没有挂提示词库",
                               "remedy": "开会话时加 `--prompts <目录>`；\
                                          目录里每个 `.md` 就是一段可引用的提示词"}),
                    );
                    return;
                };
                // 重读一遍：提示词是文件，**刚改的规矩不该等重启**
                let lib = lib.now();
                for name in &wanted {
                    if lib.get(name).is_none() {
                        self.reply(
                            req,
                            json!({"effect": "refused", "code": "invalid",
                                   "why": format!("库里没有 `{name}` 这一段"),
                                   "remedy": format!(
                                       "有这些：{}。**别猜名字**",
                                       if lib.names().is_empty() {
                                           "（一段都没有）".to_string()
                                       } else {
                                           lib.names().join(" / ")
                                       })}),
                        );
                        return;
                    }
                }
                // 拼起来 —— **每一段都带抬头**（谁说的、哪一版）
                let mut text = String::from("【提示词】以下是用户给你的规矩，按顺序生效：\n\n");
                let mut used: Vec<Value> = Vec::new();
                for name in &wanted {
                    let f = lib.get(name).expect("上面查过");
                    text.push_str(&format!("【提示词 {}@{}】\n{}\n\n", f.name, f.version, f.text));
                    used.push(json!({"name": f.name, "version": f.version, "bytes": f.text.len()}));
                }
                text.push_str(
                    "这几段是**追加**进来的，前缀没动（§15.2）—— \
                     它们和一开始就写死的规矩一样算数。",
                );
                let inject = self.mint(
                    MessageType::Inject,
                    CTRL,
                    &agent,
                    None,
                    Authority::System,
                    json!({"text": text, "reason": "prompt_fragment", "fragments": used}),
                );
                self.route(inject);
                // 审计：这是一次**控制面的变更**（往谁的上下文里加了规矩），要留痕
                let sid = self.sid.clone();
                self.index.append(
                    &sid,
                    &agent,
                    "agent",
                    "prompt_injected",
                    json!({"fragments": used, "by": req.from, "dir": lib.dir.display().to_string()}),
                );
                self.reply(
                    req,
                    json!({"effect": "confirmed", "code": null,
                           "data": {"agent": agent, "fragments": used,
                                    "note": "念给它了；它下一轮就看得见。\
                                             前缀**没有重编** —— 变化只走这条追加的通知"}}),
                );
            }
            "my_tools" => {
                let who = req.from.clone();
                match self.reg.bindings_of(&who) {
                    None => self.reply(
                        req,
                        json!({"effect": "refused", "code": "refused",
                               "why": format!("`{who}` 不是智能体，没有授权清单"),
                               "remedy": "这条只给智能体用"}),
                    ),
                    Some(bound) => {
                        let ids: Vec<String> =
                            bound.iter().map(|b| b.instance.clone()).collect();
                        let missing: Vec<String> = ids
                            .iter()
                            .filter(|id| !self.reg.contains(id))
                            .cloned()
                            .collect();
                        let tools = self.reg.manifests_of(&ids);
                        self.reply(
                            req,
                            json!({"effect": "confirmed", "code": null,
                                   // 连**能力**一起告诉它：它该知道自己被授予了什么，
                                   // 不然被拒的时候只会觉得莫名其妙
                                   "data": {"bound": ids, "bindings": bound, "tools": tools,
                                            "not_alive": missing}}),
                        );
                    }
                }
            }
            // **改签一条绑定**（§2.3）：授权不是一次性的事，是**可以追加、也可以收回**的。
            //
            // 这一条补的是那条一直缺的**触发源**。在这之前绑定只是服务表里的一行静态配置，
            // 于是"名册只追加 + 回合边界扫一遍 + 发一条敕令"整套机制永远等不到变化 ——
            // 链是接好的，就是没人按开关。按下它，整条链自己走完：
            //
            //   用户 grant → 中控改签 → **立刻发一条敕令**（事实/影响/出口）
            //   → 智能体下一回合开始前扫一遍、换名册（function 定义数组）
            //   → 前缀一个字都不动（§15.2：拼装冻结，变化走 append-only）
            "grant" | "revoke" => {
                // **只有用户能改授权** —— 这一关现在归 `op_min_authority`（`user` 档），
                // 这里的自查 2026-09-23 删掉了：矩阵先跑，它永远轮不到，
                // 而"一条规则两个执行者、其中一个永远不执行"正是这个仓库最讨厌的形状。
                // 那句"智能体给自己加权限是不允许的"没丢，搬进了矩阵的判词里。
                let agent = req.payload.get("agent").and_then(Value::as_str).unwrap_or("").to_string();
                let instance = req.payload.get("instance").and_then(Value::as_str).unwrap_or("").to_string();
                if agent.is_empty() || instance.is_empty() {
                    self.reply(
                        req,
                        json!({
                            "effect": "refused", "code": "invalid",
                            "why": "grant / revoke 需要 `agent` 和 `instance` 两个字段",
                            "expect": {"op": "grant", "agent": "agent:dada",
                                       "instance": "term:1", "capabilities": ["proc.exec"]},
                            "remedy": "`capabilities` 可以不给（= 全权）；给空数组 `[]` 就是\
                                       **它能碰到、但什么都不许干**"
                        }),
                    );
                    return;
                }
                if self.reg.bindings_of(&agent).is_none() {
                    let known = self.reg.agent_ids();
                    self.reply(
                        req,
                        json!({
                            "effect": "refused", "code": "instance_gone",
                            "why": format!("`{agent}` 不是智能体实例（或者它不在）—— 授权只发给智能体"),
                            "remedy": format!(
                                "现在活着的智能体：{}",
                                if known.is_empty() { "（一个都没有）".to_string() } else { known.join(", ") }
                            )
                        }),
                    );
                    return;
                }
                let alive = self.reg.contains(&instance);
                let op = req.payload.get("op").and_then(Value::as_str).unwrap_or("");
                let after = if op == "grant" {
                    let caps = match req.payload.get("capabilities") {
                        Some(Value::Array(a)) => a
                            .iter()
                            .filter_map(Value::as_str)
                            .map(str::to_string)
                            .collect::<Vec<_>>(),
                        _ => vec!["*".to_string()],
                    };
                    self.reg.grant(&agent, Binding::new(&instance, caps))
                } else {
                    self.reg.revoke(&agent, &instance)
                };
                // 改完之后的**整张清单**：既用来报"你现在有哪些实例"，
                // 也用来报"**这一个实例放你用什么**"（下面那段）。
                let after_list = after.unwrap_or_default();
                let shown: Vec<String> = after_list.iter().map(|b| b.instance.clone()).collect();
                let caps_now: Vec<String> = after_list
                    .iter()
                    .find(|b| b.instance == instance)
                    .map(|b| b.capabilities.clone())
                    .unwrap_or_default();
                // **把"凭什么能用"一起说清**（2026-09-20 补）。
                //
                // 原来这条敕令只说"你现在可以用 `term:1` 了" —— 而**授权的粒度和
                // 能力的粒度不是一个**：绑定挂在**实例**上，能干的动作却由
                // **能力清单**决定（§7）。只签了 `proc.read` 却宣布"你能用 term:1"，
                // 那句话是**真的、但会误导** —— 模型会以为能 `run`，撞一次墙才知道。
                //
                // 它不改变"谁校验"：校验仍然在服务那边（§2.3 的责任分配没动）。
                // 变的只是**告诉模型它有哪几条**，让它不必靠试错去发现。
                let caps_text = if caps_now.iter().any(|c| c == "*") {
                    "它给你的是一条通配（`*`）—— 这个实例上的动作你都能用。".to_string()
                } else if caps_now.is_empty() {
                    "**不过一条能力都没给你** —— 你调它会被它自己挡下来，\
                     理由是「你没这条资格」。要动它，先让用户给你签能力。"
                        .to_string()
                } else {
                    format!(
                        "它放你用的是这几条动作：`{}`。**这几条之外它仍然会拒** —— \
                         拒的时候它会告诉你缺哪一条。",
                        caps_now.join("` / `")
                    )
                };
                let (fact, impact, exit) = if op == "grant" {
                    (
                        format!("你现在**可以用** `{instance}` 了。"),
                        format!(
                            "你的名册会多出这一个工具实例 —— 下一个回合开始前我会重新扫一遍并换掉它。\
                             前缀（人设和规矩）**不会因此重写**：变化只走这条追加的通知。\
                             {caps_text}"
                        ),
                        "拿不准它怎么用，先调它的 `help`。",
                    )
                } else {
                    (
                        format!("`{instance}` 你**不再能用**了。"),
                        "下个回合你的名册里就没有它了；**已经发出去的调用不受影响** —— \
                         那是已经发生的事，收回授权追不回来。"
                            .to_string(),
                        "如果这正是你正在干的活，直说做不了，别换个名字重试。",
                    )
                };
                let inject = self.mint(
                    MessageType::Inject,
                    CTRL,
                    &agent,
                    None,
                    Authority::System,
                    json!({
                        "text": format!("【敕令】{fact}\n- 事实：{fact}\n- 影响：{impact}\n- 出口：{exit}"),
                        "reason": if op == "grant" { "binding_granted" } else { "binding_revoked" },
                        "instance": instance,
                        "bindings": shown,
                    }),
                );
                self.route(inject);
                // **索引**：授权变更也留痕 —— 这是"鉴权可追踪"的那一半。
                // 记的是**改完之后的整张清单**（不是"加了什么"），
                // 因为事后要回答的是"那一刻它能用什么"。
                {
                    let sid = self.sid.clone();
                    self.index.append(
                        &sid,
                        &agent,
                        "agent",
                        "binding",
                        json!({
                            "action": op,
                            "instance": instance,
                            "bindings": shown,
                            "by": req.from,
                        }),
                    );
                }
                // 审计：这是一次**控制面的变更**，和 `host` / `rehost` 一样要留痕
                let ev = self.system_event(
                    &self.channel_id(),
                    json!({
                        "code": "binding_changed",
                        "action": op,
                        "agent": agent,
                        "instance": instance,
                        "bindings": shown,
                        "why": format!("用户改了一条绑定：{agent} {op} {instance}"),
                    }),
                );
                self.route(ev);
                self.reply(
                    req,
                    json!({
                        "effect": "confirmed", "code": null,
                        "data": {
                            "action": op, "agent": agent, "instance": instance,
                            "bindings": shown,
                            // 授权一个还没上线的实例是允许的（装配顺序不该被卡住），
                            // 但要说出来 —— 否则调用方会以为它已经能用了
                            "not_alive": if alive { json!([]) } else { json!([instance]) },
                            "note": "敕令已经发给它了；它下一个回合开始前会换名册"
                        }
                    }),
                );
            }
            // **回放**：把"模型看到过的东西"从日志里重建出来。
            //
            // 这是不变量二（模型可见即已记录）的另一半：光记下来不算数，
            // **得有人真的能从日志重建**。跨进程恢复就是那个检验。
            // **读日志**（2026-09-23 起：它不再"重建上下文"）。
            // **订阅**（2026-09-27）：**要在订阅口那条连接上说，不是在这儿说。**
            //
            // 为什么拒：订阅这件事是**连接级**的（"你这条连接从此收一份副本"），
            // 而控制通道本来就是"谁拉起中控谁看得见一切"的那条路 ——
            // 它已经在收全量了，再订阅一次没有意义。
            "subscribe" => {
                self.reply(
                    req,
                    json!({"route": null, "effect": "refused", "code": "refused",
                           "why": "订阅要在**订阅口那条连接**上说 —— 控制通道已经在收全量了",
                           "remedy": "订阅口的地址与口令在 data/rendezvous/<会话>-console.json 里；\
                                      连上去先发那行口令，再发 {\"op\":\"subscribe\"}。\
                                      拉起中控的那个进程不需要订阅（它本来就看得见全部）"}),
                );
            }
            //
            // L1 那一刀：**中控不再懂上下文的结构**。它只回答"有哪些记录"，至于
            // "记录怎么变成我的上下文"（哪条是系统提示、哪条是模型消息、压缩怎么重放）
            // 是**智能体自己的事** —— 所以"重建上下文"和"压缩手术的重放"这两块
            // 从这儿**整块搬到了 `harness-agent`**（旧函数名在这里也不留，
            // 免得判据 `git grep` 那三个标识符时被自己的注释绊住）。
            //
            // 为什么这是对的：上下文的结构是**某个智能体实现的私事**（换一个框架、
            // 换一种记忆策略，结构就变了），而中控只要不认识它，就永远不用为它改。
            // ⚠️ 代价写进规范了：不变量二（模型可见即已记录）的强度**从此取决于
            // 智能体自己报告得诚不诚实** —— 中控记的仍然是每一条消息，但"模型看到了什么"
            // 现在只有智能体知道。
            "log" => {
                let who = req
                    .payload
                    .get("instance")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                // **低于 user 的调用方只能读自己的流量**。
                // 不给实例名（空串）等于"读整场"，那条路只留给用户。
                if req.authority < Authority::User && who != req.from {
                    self.reply(
                        req,
                        json!({
                            "effect": "refused", "code": "refused",
                            "why": format!(
                                "只能读自己的：你是 `{}`，要读的是 `{}`",
                                req.from,
                                if who.is_empty() { "（空 = 全场）" } else { who.as_str() }
                            ),
                            "remedy": "填你自己的实例 id；要看别人的记录得让用户来问"
                        }),
                    );
                    return;
                }
                // `since` 是**游标**（字节以外的另一种页码）：只给 seq 比它大的。
                // 默认 0 = 从头给。日志大了要分页 —— 那条还没做（见 §8）。
                let since = req.payload.get("since").and_then(Value::as_u64).unwrap_or(0);
                match self.log.read_all() {
                    Err(e) => self.reply(
                        req,
                        json!({"effect": "refused", "code": "invalid",
                               "why": format!("读不出会话日志：{e}"),
                               "remedy": "日志中间有洞就没法重放（少一条也不许装作能重放）；\
                                          尾部半残是允许的 —— 那种情况照常给，只是会报出 broken_tail"}),
                    ),
                    Ok(read) => {
                        // **按"这条记录跟谁有关"过滤**：它是这个实例发的，或者发给它的。
                        // 这就是"读自己的流量"的准确含义（一个实例的邻居只有这些）。
                        let records: Vec<Value> = read
                            .records
                            .iter()
                            .filter(|r| r.seq > since)
                            .filter(|r| r.envelope.from == who || r.envelope.to == who)
                            .filter_map(|r| serde_json::to_value(r).ok())
                            .collect();
                        let to_seq = read.records.last().map(|r| r.seq).unwrap_or(0);
                        // **跳过尾部半残这件事要说出来**（§11.5 那条"诚实报告"）：
                        // 不说的话，调用方会以为它看到的就是全部记录。
                        if read.broken_tail > 0 {
                            eprintln!(
                                "[ctrl] 读日志：尾部跳过 {} 行半残（崩在写日志那一下就是长这样；\
                                 崩溃语义：尾部可以半残、中间不许有洞）",
                                read.broken_tail
                            );
                        }
                        self.reply(
                            req,
                            json!({"effect": "confirmed", "code": null,
                                   "data": {"records": records, "to_seq": to_seq,
                                            "broken_tail": read.broken_tail}}),
                        );
                    }
                }
            }
            "host" => {
                // 托管一条路径（§7.1：用户托管两样之一；另一件是窗口，那部分还没做）
                let path = req
                    .payload
                    .get("path")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                if path.is_empty() {
                    self.reply(
                        req,
                        json!({"effect": "refused", "code": "invalid",
                               "why": "host 需要 path",
                               "remedy": "给一条目录路径；它就是这个会话的写入范围"}),
                    );
                    return;
                }
                let canon = std::fs::canonicalize(&path)
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|_| path.clone());
                let anchor_note = "会话托管的是**路径**（绑定范围与围栏都按它算）；\
                                   记忆那层的「锚点」由调用方给 —— §16.5 的修订已删掉\
                                   「项目层」那一档，所以这里**没有待实现的锚点**";
                // **同一条路径托管两次 = 一条**（2026-09-27，被 `smoke-resume` 抓出来的）。
                //
                // 那次会话是这么开的：`--host <project>` **加** 脚本又发了一条 `host <project>`
                // —— 于是 `session.hosts` 里躺了两条一模一样的记录（判据 3 当场红：
                // "托管 2 条"）。托管是**声明范围内有什么**，不是流水账：
                // 一条路径写两遍，后面所有"按托管算范围"的地方都要自己记得去重。
                // 已经在里面就**如实说已经在里面**（幂等），别再加一条。
                let already = self
                    .session
                    .hosts
                    .iter()
                    .any(|h| h.get("path").and_then(Value::as_str) == Some(canon.as_str()));
                if already {
                    self.reply(
                        req,
                        json!({"route": null, "effect": "confirmed", "code": null,
                               "data": {"path": canon, "idempotent": true,
                                        "hosts": self.session.hosts.len(),
                                        "note": "这条路径**已经托管过了** —— 托管是『范围内有什么』，\
                                                 不是流水账"}}),
                    );
                    return;
                }
                self.session.hosts.push(json!({
                    "path": canon,
                    "hosted_at": now_ms(),
                    "by": req.from,
                    "anchor_note": anchor_note,
                }));
                let _ = self.log.write_meta(&self.session.meta());
                // **托管立即生效**：围栏由会话给的那些服务，照新路径重新装配。
                //
                // 为什么不是"下次装配时再生效"：用户的心智模型是
                // "我说了项目搬走了，它就该按新的来"。要这条成立，
                // 中控就得真的去动那些服务 —— **这正是分布式里中控存在的意义**。
                let moved = self.rehost();
                if moved > 0 {
                    let ev = self.system_event(
                        &self.channel_id(),
                        json!({
                            "kind": "rehost",
                            "path": canon,
                            "services": moved,
                            "why": "托管路径变了，围栏跟着换",
                            "note": "这些服务的实例（终端 / 打开的文档）已经跟着收掉了 —— \
                                     它们本来就绑在旧路径上"
                        }),
                    );
                    self.route(ev);
                }
                let n = self.session.hosts.len();
                self.reply(
                    req,
                    json!({"effect": "confirmed", "code": null,
                           "data": {"hosted": canon, "hosts_count": n,
                                    "services_rehosted": moved, "note": anchor_note}}),
                );
            }
            "delegate" => {
                // **会话点名**（§12.7）：把某个智能体配置委派到这条路径上干活
                let agent = req
                    .payload
                    .get("agent")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                if !self.reg.contains(&agent) {
                    let known: Vec<String> = self.reg.agent_ids();
                    self.reply(
                        req,
                        json!({"effect": "refused", "code": "instance_gone",
                               "why": format!("没有智能体实例 `{agent}`"),
                               "remedy": format!(
                                   "现在活着的智能体：{}",
                                   if known.is_empty() { "（一个都没有）".into() } else { known.join(", ") }
                               )}),
                    );
                    return;
                }
                self.session.agent = Some(agent.clone());
                if !self.session.participants.contains(&agent) {
                    self.session.participants.push(agent.clone());
                }
                let _ = self.log.write_meta(&self.session.meta());
                self.reply(
                    req,
                    json!({"effect": "confirmed", "code": null, "data": {"agent": agent}}),
                );
            }
            // **撤销一条托管**。搬家时要用：旧路径不存在了，
            // 而服务的围栏根**必须存在**（不然它起不来或什么都不让干）。
            "unhost" => {
                let path = req
                    .payload
                    .get("path")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let canon = std::fs::canonicalize(&path)
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|_| path.clone());
                let before = self.session.hosts.len();
                self.session.hosts.retain(|h| {
                    h.get("path").and_then(Value::as_str) != Some(canon.as_str())
                });
                let removed = before - self.session.hosts.len();
                let _ = self.log.write_meta(&self.session.meta());
                let moved = if removed > 0 { self.rehost() } else { 0 };
                self.reply(
                    req,
                    json!({"effect": "confirmed", "code": null,
                           "data": {"unhosted": canon, "removed": removed,
                                    "services_rehosted": moved}}),
                );
            }
            "suspend" => {
                // **挂起 = 工具实例收掉，会话留着**（§12.7："这两个要分开"）
                self.recycle("挂起");
                self.session.state = "suspended";
                let _ = self.log.write_meta(&self.session.meta());
                let n = self.reg.tool_count();
                self.reply(
                    req,
                    json!({"effect": "confirmed", "code": null,
                           "data": {"state": "suspended", "tool_instances_left": n,
                                    "note": "会话留着；resume 会照同一批 id 重新装配"}}),
                );
            }
            "resume" => {
                if self.session.state == "ended" {
                    self.reply(
                        req,
                        json!({"effect": "refused", "code": "refused",
                               "why": "这个会话已经结束了",
                               "remedy": "结束不是挂起 —— 要接着干就开一个新会话"}),
                    );
                    return;
                }
                let n = self.assemble();
                self.session.state = "active";
                let _ = self.log.write_meta(&self.session.meta());
                self.reply(
                    req,
                    json!({"effect": "confirmed", "code": null, "data": {"assembled": n}}),
                );
            }
            "end" => {
                self.recycle("结束");
                self.session.state = "ended";
                let _ = self.log.write_meta(&self.session.meta());
                self.reply(
                    req,
                    json!({"effect": "confirmed", "code": null,
                           // ⚠️ 2026-09-20 把话说准：记忆**服务**有了（`harness-mem` 的 deposit），
                           // 缺的是"**会话结束自动沉淀**"这个动作（§16.3 那座桥）。
                           // 原来那句"沉淀进记忆还没实现"会让人以为整块都没做。
                           "data": {"state": "ended",
                                    "note": "归档了；**沉淀进记忆还是手动的** —— \
                                             记忆服务有了（mem 的 deposit），\
                                             但会话结束不会自动替你沉淀（§16.3 那个动作还没做）"}}),
                );
                self.exit_requested = true;
            }
            "spawn" => {
                let kind = req
                    .payload
                    .get("kind")
                    .and_then(Value::as_str)
                    .unwrap_or("echo")
                    .to_string();
                let cmd: Vec<String> = req
                    .payload
                    .get("cmd")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(Value::as_str)
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_default();
                if cmd.is_empty() {
                    self.reply(
                        req,
                        json!({
                            "effect": "refused",
                            "code": "invalid",
                            "why": "spawn 需要 cmd",
                            "expect": {"op": "spawn", "kind": "echo", "cmd": ["<可执行文件>", "参数…"]},
                            "remedy": "把要拉起的可执行文件写成数组；中控会自动补 --instance / --sid"
                        }),
                    );
                    return;
                }
                // id 从**只增不减的序号**里出，而且带上随机段 ——
                // 见 `next_instance_seq` 与 `harness_core::id::short_rand` 的注释
                self.next_instance_seq += 1;
                let id = format!("{}:{}-{}", kind, self.next_instance_seq, harness_core::id::short_rand());
                let role = req
                    .payload
                    .get("role")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                // 运行期拉起来的服务**不给绑定**：绑定是中控签发的东西，
                // 不该由一条消息现场决定；要给它授权，得写进服务表
                let bindings = Vec::new();
                // 运行期 `spawn` 出来的服务**永远是普通服务**（stdin/stdout 那条）：
                // 常驻是**部署配置**（要一个跨会话稳定的会合目录），
                // 不该由一条消息现场决定 —— 同"绑定只许写进服务表"那条纪律。
                match self.start_service(Spawn {
                    kind,
                    id: id.clone(),
                    cmd,
                    role,
                    bindings,
                    launcher: Launcher::Stdio,
                    tx: self.tx.clone(),
                }) {
                    Ok(()) => self.reply(
                        req,
                        json!({
                            "effect": "confirmed",
                            "code": null,
                            "data": {"instance_id": id}
                        }),
                    ),
                    Err(e) => self.reply(
                        req,
                        json!({
                            "effect": "refused",
                            "code": "invalid",
                            "why": format!("起不来：{e}"),
                            "remedy": "检查 cmd 里的可执行文件路径是否存在；它必须接 --instance / --sid 两个参数。\
                                       要常驻（跨会话）得写进服务表，运行期 spawn 出来的永远是普通服务"
                        }),
                    ),
                }
            }
            "shutdown" => {
                self.reply(
                    req,
                    json!({"effect": "confirmed", "code": null, "data": {"bye": true}}),
                );
                self.exit_requested = true;
            }
            // **收掉一个服务**（§5.20 四条确定回收路径里那条"用户手动收"在**服务级**的入口）。
            //
            // 在这之前只有 `spawn` 没有 `stop`：用户能拉起一个服务，却只能靠
            // `suspend`/`end` 把整个会话收掉，或者等它自己崩。
            //
            // 实现上故意**不做第二套拆除逻辑**：关掉它的 stdin、等它自己走，
            // 然后**照常让它走"服务退出"那条路**（`on_service_exited`）——
            // 于是"它的实例跟着没 / 未决调用逐个结掉 / 给智能体发敕令"这三件事
            // 一份代码全包了（少一份实现，就少一处漏）。
            "stop" => {
                let id = req
                    .payload
                    .get("instance")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                if id.is_empty() {
                    self.reply(
                        req,
                        json!({
                            "effect": "refused", "code": "invalid",
                            "why": "stop 需要 `instance`",
                            "expect": {"op": "stop", "instance": "term:1"},
                            "remedy": "要停谁就写谁的 id；`list` 能看到现在有哪些"
                        }),
                    );
                    return;
                }
                if id == CTRL || id == self.channel.id {
                    self.reply(
                        req,
                        json!({
                            "effect": "refused", "code": "refused",
                            "why": format!("`{id}` 是这条会话自己的入口，停掉它等于把管子锯了"),
                            "remedy": "要收摊就 `end`（归档）或者 `shutdown`（关掉中控）"
                        }),
                    );
                    return;
                }
                if self.reg.get_ref(&id).map(|i| i.is_agent).unwrap_or(false) {
                    self.reply(
                        req,
                        json!({
                            "effect": "refused", "code": "refused",
                            "why": format!("`{id}` 是**智能体**（还是本会话点名的那个）"),
                            "remedy": "智能体的生命周期归会话管：要它停下来就 `suspend`（实例收掉、会话留着）\
                                       或者 `end`（归档）。单独把它停了，会话会指着一个已经不在的东西"
                        }),
                    );
                    return;
                }
                if self.reg.get_ref(&id).map(|i| i.service.is_none()).unwrap_or(true) {
                    let known: Vec<String> = self.reg.ids().map(str::to_string).collect();
                    self.reply(
                        req,
                        json!({
                            "effect": "refused", "code": "instance_gone",
                            "why": format!("没有可以收掉的**服务** `{id}`"),
                            "remedy": format!("现在有：{}", known.join(", "))
                        }),
                    );
                    return;
                }
                // **常驻服务要分两段收**（2026-09-20）：先**请它自己停**（它声明的
                // `stop` 入口），再看它到底停没停 —— 因为**接上来的那个我手里
                // 没有句柄**，"杀进程"这条路根本不存在。
                //
                // 这一条也是"责任向下传导"：**怎么体面地停，只有服务自己知道**
                // （要落盘什么、要给谁留遗嘱）。中控只负责"请"和"再确认一次"。
                let stop_entry = self.stop_entry_of(&id);
                let resident_spec_dir = self
                    .specs
                    .iter()
                    .find(|s| s.id.as_deref() == Some(id.as_str()) && s.resident)
                    .and_then(|s| s.rendezvous.clone());
                // 先礼：**请它自己停**（它声明的 `stop` 入口）。
                // 只发不等 —— 中控是单线程的，在这儿等回信就是**死锁**
                // （回信要由主循环处理，而主循环正在跑这一段）。
                let asked = stop_entry
                    .as_ref()
                    .map(|tool| self.request_service_stop(&id, tool))
                    .unwrap_or(false);
                // **只关它的 stdin，不动注册表。**
                // 剩下的交给它自己的读线程：EOF ⇒ 服务报 offline 并退出 ⇒
                // `on_service_exited` 照常做那三件事（收实例 / 结未决调用 / 发敕令）。
                // 在这里抢先 `reg.remove` 会把那条路变成空操作 —— 通知就丢了。
                let mut how = "它不在这个会话的注册表里（可能已经没了）".to_string();
                if let Some(svc) = self.reg.get_mut(&id).and_then(|i| i.service.as_mut()) {
                    if svc.resident {
                        // 常驻：先看看那句"请"有没有把它请走（给它一点时间，最多 600ms）
                        let gone = resident_spec_dir
                            .as_ref()
                            .map(|d| {
                                let dir = std::path::Path::new(d);
                                let deadline =
                                    std::time::Instant::now() + std::time::Duration::from_millis(600);
                                while std::time::Instant::now() < deadline {
                                    let alive = harness_ipc::Rendezvous::read(dir, &id)
                                        .map(|rv| rv.alive())
                                        .unwrap_or(false);
                                    if !alive {
                                        return true;
                                    }
                                    std::thread::sleep(std::time::Duration::from_millis(50));
                                }
                                false
                            })
                            .unwrap_or(false);
                        if gone {
                            how = if asked {
                                format!(
                                    "**它自己停的**（调了它声明的 `stop` 入口 `{}`）—— \
                                     会合文件已经不作数了",
                                    stop_entry.clone().unwrap_or_default()
                                )
                            } else {
                                "它自己退了（我没请它，也没杀它）".to_string()
                            };
                        } else {
                            match svc.stop_now() {
                                StopReport::Killed => {
                                    how = if asked {
                                        format!(
                                            "先请它自己停了（`{}`），它没走 ⇒ **我手里有它的句柄，直接收掉了**",
                                            stop_entry.clone().unwrap_or_default()
                                        )
                                    } else {
                                        "我手里有它的句柄，直接收掉了".to_string()
                                    };
                                }
                                StopReport::NotMine { pid } => {
                                    how = if asked {
                                        format!(
                                            "它是**别的会话起来的**（pid {pid}），我手里没有它的句柄 —— \
                                             已经**请它自己停了**（调了它声明的 `{}`）。\
                                             它到底走没走，下一次 `list` 见（会合文件不作数 = 走了）",
                                            stop_entry.clone().unwrap_or_default()
                                        )
                                    } else {
                                        format!(
                                            "⚠️ 它是**别的会话起来的**（pid {pid}），我没有它的句柄，\
                                             它也没声明 `stop` 入口 —— **没收掉**。\
                                             要么去它的宿主那个会话收，要么给这个服务补一个 `stop` 入口"
                                        )
                                    };
                                }
                            }
                        }
                        svc.shutdown();
                    } else {
                        svc.shutdown();
                        how = "关掉它的 stdin，等它自己走".to_string();
                    }
                }
                eprintln!("[ctrl] 用户收掉了 {id}：{how}");
                self.reply(
                    req,
                    json!({
                        "effect": "confirmed", "code": null,
                        "data": {
                            "stopped": id,
                            "how": how,
                            "asked_to_stop": asked,
                            "note": "它的工具实例会跟着下线，未决调用会逐个结掉，\
                                     有智能体的话会收到一条敕令 —— 这些都是照常走的"
                        }
                    }),
                );
            }
            other => {
                let shown = if other.is_empty() { "(空)" } else { other };
                self.reply(
                    req,
                    json!({
                        "effect": "refused",
                        "code": "invalid",
                        "why": format!("ctrl 不认识 op=`{shown}`"),
                        "expect": {"op": "ping | list | shutdown"},
                        "remedy": "先用 ping 试通路，用 list 看有哪些实例"
                    }),
                );
            }
        }
    }

// ---- 生命周期 --------------------------------------------------------

    fn start_service(&mut self, s: Spawn) -> io::Result<()> {
        let Spawn {
            kind,
            id,
            cmd,
            role,
            bindings,
            launcher,
            tx,
        } = s;
        let is_agent = role.as_deref() == Some("agent");
        // **同一个 id 在同一时刻只能有一个活的持有者。**
        //
        // 这条是 id 层面的鉴权：绑定表写的是 id、归属写的是 id、路由按 id 找收件人 ——
        // 要是两个进程同时用 `term:1`，那么"发给 term:1 的请求"到底给谁？
        // 原来注册表是**静默覆盖**（后来者顶掉先来者），于是新进程会替旧进程收活。
        // 所以撞 id 一律拒，而且说清是谁占了它。
        if let Some(old) = self.reg.get_mut(&id) {
            let cmd = old.service.as_ref().map(|s| s.cmd.join(" ")).unwrap_or_default();
            return Err(io::Error::other(format!(
                "id `{id}` 已经被占用了（正在跑的是：{cmd}）。\
                 同一个 id 在同一时刻只能有一个活的持有者 —— \
                 要么改服务表里的 id，要么先把它停掉"
            )));
        }
        // **每一代 +1** —— 旧世代的迟到消息要靠它认出来
        let generation = self.reg.generation_of(&id).unwrap_or(0) + 1;
        // `kind` 一会儿要移进注册表，先留一份给索引用
        let index_kind = kind.clone();
        let svc = match &launcher {
            Launcher::Stdio => {
                let svc = service::ServiceHandle::spawn(
                    service::Launch {
                        id: id.clone(),
                        cmd: cmd.clone(),
                        sid: self.sid.clone(),
                        id_water: self.water_for(&id),
                    },
                    generation,
                    tx,
                )?;
                eprintln!(
                    "[ctrl] 拉起 {}{}（{}）",
                    id,
                    if is_agent { "［智能体］" } else { "" },
                    cmd.join(" ")
                );
                svc
            }
            Launcher::Resident { dir, grace_ms } => {
                // **先看有没有活着的**：有就接上，没有才拉。
                //
                // 顺序不能反过来 —— 先拉就会有两个进程抢同一份状态，
                // 而那正是"一处只许一个进程写"（域锁）在防的事。
                let live = harness_ipc::Rendezvous::read(dir, &id)
                    .filter(|rv| rv.alive());
                match live {
                    Some(rv) => {
                        let svc = service::ServiceHandle::attach(
                            id.clone(),
                            cmd.clone(),
                            &rv,
                            generation,
                            tx,
                        )?;
                        eprintln!(
                            "[ctrl] **接上** {id}（它已经在跑了：pid {}，起于 {}）—— 没有再起一个",
                            rv.pid,
                            rv.started_ms
                        );
                        svc
                    }
                    None => {
                        // 文件在、但连不上 ⇒ 那是**上一次留下的僵尸会合文件**。
                        // 说出来（别装作没有），然后拉一个新的覆盖它。
                        if let Some(stale) = harness_ipc::Rendezvous::read(dir, &id) {
                            eprintln!(
                                "[ctrl] `{}` 的会合文件还在（pid {}），但它不应答 —— \
                                 那是上次崩了留下的，我拉一个新的顶上",
                                id, stale.pid
                            );
                        }
                        let svc = service::ServiceHandle::spawn_resident(
                            service::Launch {
                                id: id.clone(),
                                cmd: cmd.clone(),
                                sid: self.sid.clone(),
                                id_water: self.water_for(&id),
                            },
                            dir,
                            *grace_ms,
                            generation,
                            tx,
                        )?;
                        eprintln!(
                            "[ctrl] 拉起常驻 {}{}（{}）",
                            id,
                            if is_agent { "［智能体］" } else { "" },
                            cmd.join(" ")
                        );
                        svc
                    }
                }
            }
        };
        self.reg.insert(Instance {
            id: id.clone(),
            kind,
            is_agent,
            // 工具最高只能是 observe；**智能体可以到 agent**（§1.1）
            ceiling: if is_agent {
                Authority::Agent
            } else {
                Authority::Observe
            },
            state: "starting",
            service: Some(svc),
            manifest: None,
        config: None,
            generation,
            // 只有智能体要检查授权；其它实例是 `None`（不检查）
            bindings: if is_agent { Some(bindings) } else { None },
            owns: None,
            contract_errors: Vec::new(),
            contract_missing: Vec::new(),
            last_seen_ms: now_ms(),
            unresponsive: false,
            // 部署决定的拉起时机（智能体也是"跟着会话"，所以默认 per_session）
            lifecycle: "per_session".into(),
            lifecycle_declared: None,
        });
        // **索引**：谁被登记了（这是"层层落盘"里"服务/智能体"那两层）
        let sid = self.sid.clone();
        self.index.append(
            &sid,
            &id,
            &index_kind,
            "registered",
            json!({
                "role": if is_agent { "agent" } else { "tool" },
                "generation": generation,
                "cmd": cmd.join(" "),
            }),
        );
        Ok(())
    }

    /// 照一条 spec 把它装配出来。**挂起之后 resume 走的就是这条路** ——
    /// 所以 id 必须和第一次装出来的一模一样（spec 里已经写回了）。
    fn spawn_spec(&mut self, spec: &ServiceSpec, tx: Sender<Inbound>) -> io::Result<()> {
        let id = spec
            .id
            .clone()
            .ok_or_else(|| io::Error::other("spec 没有解析出 id"))?;
        let mut cmd = spec.cmd.clone();
        cmd.extend(spec.args.clone());
        // **围栏来自会话，不来自服务表。** 这是"托管即授权"落地的地方：
        // 用户托管了哪条路径，工具服务就在哪条路径上起来。
        if spec.wants_fence {
            let hosts: Vec<String> = self
                .session
                .hosts
                .iter()
                .filter_map(|h| h.get("path").and_then(Value::as_str))
                .map(str::to_string)
                .collect();
            if hosts.is_empty() {
                return Err(io::Error::other(format!(
                    "{id} 要围栏，但会话还没托管任何路径 —— 先 host 一条"
                )));
            }
            for h in hosts {
                cmd.push("--fence".into());
                cmd.push(h);
            }
        }
        // **常驻那条路要先有一个跨会话稳定的会合目录** —— 它不能在服务表里
        // 留空（少了它，新会话找不到老进程，`resident` 就退化成"活得久一点的
        // 普通服务"）。这是**部署配置缺了**，所以要在这里明说，而不是悄悄降级。
        let launcher = if spec.resident {
            match &spec.rendezvous {
                Some(dir) => Launcher::Resident {
                    dir: std::path::PathBuf::from(dir),
                    grace_ms: 5000,
                },
                None => {
                    return Err(io::Error::other(format!(
                        "{id} 在服务表里写了 `resident: true`，但没给 `rendezvous` 目录 —— \
                         没有它，别的会话找不到这个进程（`resident` 就白写了）。\
                         给它一个**跨会话稳定**的目录（别用每次都会变的临时目录）"
                    )))
                }
            }
        } else {
            Launcher::Stdio
        };
        let started = self.start_service(Spawn {
            kind: spec.kind.clone(),
            id: id.clone(),
            cmd,
            role: spec.role.clone(),
            bindings: spec
                .bindings
                .iter()
                .map(|b| registry::Binding::new(b.instance(), b.capabilities()))
                .collect(),
            launcher,
            tx,
        });
        // **把"部署决定的拉起时机"记在实例上** —— `list` 上要和它自述的并排显示
        if started.is_ok() {
            if let Some(inst) = self.reg.get_mut(&id) {
                inst.lifecycle = spec
                    .lifecycle
                    .clone()
                    .unwrap_or_else(|| "per_session".into());
            }
        }
        started
    }

    // ---- 会话：装配与回收 -------------------------------------------------

    /// **挂起**：回收实例，会话留着。
    ///
    /// 顺序是有讲究的（2026-09-18 你定的"按顺序回收实例并在敕令中注入"）：
    ///
    /// 1. **先发敕令** —— 得趁智能体还活着把话说完；它死了也一样，那条敕令
    ///    落在日志里，下次恢复时它读得到（§12.7："敕令是写给日志的"）；
    /// 2. 再收**工具**（它们不持有上下文，先走不影响谁）；
    /// 3. 最后收**智能体**。
    fn recycle(&mut self, why: &str) {
        // 1. 敕令
        let agents = self.reg.agent_ids();
        for agent in &agents {
            let text = format!(
                "【敕令】会话正在{why}，你要用的工具马上会被收回。\n\
                 - 事实：接下来工具实例会被逐个回收，注册表会空掉。\n\
                 - 影响：这之后你调不动任何工具。\n\
                 - 出口：如果会话还能恢复，你会带着这段历史被重新装配起来；\
                 不能的话，把结论留在最后一条消息里。"
            );
            let inject = self.mint(
                MessageType::Inject,
                CTRL,
                agent,
                None,
                Authority::System,
                json!({"text": text, "reason": "session_recycle"}),
            );
            self.route(inject);
        }
        // 2. 工具，3. 智能体 —— 顺序按 role
        let mut tools: Vec<String> = Vec::new();
        let mut agents_to_stop: Vec<String> = Vec::new();
        for spec in &self.specs {
            if let Some(id) = &spec.id {
                if spec.role.as_deref() == Some("agent") {
                    agents_to_stop.push(id.clone());
                } else {
                    tools.push(id.clone());
                }
            }
        }
        for id in tools.into_iter().chain(agents_to_stop) {
            if let Some(mut inst) = self.reg.remove(&id) {
                // **常驻服务在这个会话里"下线"，但进程留着**（§11.10）——
                // 账上要分开写：`exited` 是"它没了"，`detached` 是"我不再看它了"。
                // 写成 `exited` 就是在撒谎，而身份索引将来是排查问题的第一本账。
                let resident = inst.service.as_ref().map(|s| s.resident).unwrap_or(false);
                if let Some(svc) = &mut inst.service {
                    svc.shutdown();
                }
                let gone = self.reg.remove_tools_of(&id);
                let sid = self.sid.clone();
                if resident {
                    self.index.append(
                        &sid,
                        &id,
                        "service",
                        "detached",
                        json!({"why": format!("{why}：会话不再看它，**进程留着**")}),
                    );
                    eprintln!(
                        "[ctrl] {why}：**松开** {id}（常驻，进程留着；连带 {} 个实例）",
                        gone.len()
                    );
                } else {
                    eprintln!("[ctrl] {why}：收掉 {id}（连带 {} 个实例）", gone.len());
                }
            }
        }
        self.pending.clear();
    }

    /// **装配**：照服务表把这一套重新拉起来。
    fn assemble(&mut self) -> usize {
        let specs = self.specs.clone();
        let tx = self.tx.clone();
        let mut n = 0;
        for spec in &specs {
            // **`per_call` 的服务不在装配时就拉** —— 它等第一次调用（见 `route` 里的按需拉起）。
            //
            // 为什么值得有这个开关：把"开会话"和"拉起一堆重服务"解耦。
            // 原来 `assemble` 是**一次性全拉**，会话一开始就把表里每个服务都拖起来，
            // 哪怕这次会话根本用不到它们。
            if !should_start_now(spec) {
                eprintln!(
                    "[ctrl] {} 声明的是 per_call —— 装配时不拉，等第一次用到它",
                    spec.id.clone().unwrap_or_default()
                );
                continue;
            }
            match self.spawn_spec(spec, tx.clone()) {
                Ok(()) => {
                    n += 1;
                    if let Some(id) = &spec.id {
                        if spec.role.as_deref() == Some("agent") {
                            self.session.agent = Some(id.clone());
                        }
                    }
                }
                Err(e) => eprintln!("[ctrl] 装配 {} 失败：{e}", spec.id.clone().unwrap_or_default()),
            }
        }
        n
    }

    /// **重新托管**：把"围栏由会话给"的那些服务收掉，照新的托管路径重新装配。
    ///
    /// 注意收掉的是**整个服务**，所以它底下的实例（终端、打开的文档）一起没 ——
    /// 它们本来就绑在旧路径上，留着才是错的。
    fn rehost(&mut self) -> usize {
        let ids: Vec<String> = self
            .specs
            .iter()
            .filter(|s| s.wants_fence)
            .filter_map(|s| s.id.clone())
            .collect();
        for id in &ids {
            if let Some(mut inst) = self.reg.remove(id) {
                if let Some(svc) = &mut inst.service {
                    svc.shutdown();
                }
                let gone = self.reg.remove_tools_of(id);
                eprintln!("[ctrl] 重新托管：收掉 {id}（连带 {} 个实例）", gone.len());
            }
        }
        let mut n = 0;
        // 没有托管路径时**不该装配**要围栏的服务 —— 那不是失败，是"没到时候"。
        // （搬家的两步：先 unhost 旧的，再 host 新的；中间那一瞬就是空的。）
        if self.session.hosts.is_empty() {
            eprintln!("[ctrl] 会话现在没有托管路径 —— 需要围栏的服务先不起，等 host");
            return 0;
        }
        let specs = self.specs.clone();
        let tx = self.tx.clone();
        for spec in specs.iter().filter(|s| s.wants_fence) {
            match self.spawn_spec(spec, tx.clone()) {
                Ok(()) => n += 1,
                Err(e) => eprintln!("[ctrl] 重新托管时装配失败：{e}"),
            }
        }
        n
    }

    fn shutdown(&mut self) {
        // 收摊前落盘 —— 这是第三个屏障边界（回合结束）
        if let Err(e) = self.log.barrier() {
            eprintln!("[ctrl] 收尾落盘失败：{e}");
        }
        for (corr, call) in self.pending.drain() {
            eprintln!(
                "[ctrl] 收摊时仍未决：{corr}（{} 等 {}）",
                call.caller, call.target
            );
        }
        // 先礼后兵地收掉每个服务（§5.15：别让用户手工收尸）
        let mut to_stop: Vec<String> = self
            .reg
            .services_mut()
            .map(|s| s.id.clone())
            .collect();
        to_stop.sort();
        for id in to_stop {
            if let Some(mut inst) = self.reg.remove(&id) {
                if let Some(svc) = &mut inst.service {
                    svc.shutdown();
                }
                // **索引要记账**：收摊走的是这条路，不经过 `on_service_exited`
                // （那条路是"服务自己先死了"），所以这里单独补一笔 ——
                // 否则账上每个服务都只有"登记"和"报到"，没有"退场"。
                let sid = self.sid.clone();
                self.index
                    .append(&sid, &id, "service", "exited", json!({"why": "会话收摊"}));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use registry::ToolHints;

    fn hints(read_only: bool, destructive: bool, idempotent: bool) -> Option<ToolHints> {
        Some(ToolHints {
            read_only,
            destructive,
            idempotent,
        })
    }

    /// **重试语义由声明决定，不由猜**（§19.2 偷来的那条）：
    /// 只读 / 幂等 ⇒ 可以重试；有副作用又不幂等 ⇒ 别自动重试。
    #[test]
    fn retry_advice_follows_the_declared_hints() {
        let read = retry_advice(Some("status"), hints(true, false, true));
        assert!(read.contains("可以重试"), "{read}");

        let idem = retry_advice(Some("ping"), hints(false, false, true));
        assert!(idem.contains("可以重试"), "{idem}");

        let destructive = retry_advice(Some("edit"), hints(false, true, false));
        assert!(destructive.contains("别自动重试"), "{destructive}");
        assert!(destructive.contains("先看状态"), "{destructive}");
    }

    /// **读不到 Hint 就按最保守的来** —— 不装懂，也不让模型去猜。
    #[test]
    fn a_missing_hint_falls_back_to_the_conservative_answer() {
        let none = retry_advice(Some("mystery"), None);
        assert!(none.contains("别自动重试"), "{none}");
        assert!(none.contains("没报这一条"), "{none}");
    }

    /// **分派前要不要落盘**，判据是服务自己声明的 `destructiveHint`。
    /// 顺带盯住"声明读得到读不到"这件事本身 —— 读不到会退化成保守落盘。
    #[test]
    fn the_manifest_actually_answers_whether_a_tool_is_destructive() {
        let mut reg = Registry::default();
        reg.insert(Instance {
            id: "editor:1".into(),
            kind: "editor".into(),
            is_agent: false,
            ceiling: Authority::Observe,
            state: "online",
            service: None,
            manifest: Some(json!({
                "tools": [
                    {"name": "read", "annotations": {
                        "readOnlyHint": true, "destructiveHint": false,
                        "idempotentHint": true, "openWorldHint": false}},
                    {"name": "edit", "annotations": {
                        "readOnlyHint": false, "destructiveHint": true,
                        "idempotentHint": false, "openWorldHint": false}}
                ]
            })),
            generation: 1,
            bindings: None,
            owns: None,
            // 这条测试只验 Hint 的读取，实例配置这儿用不上
            config: None,
            contract_errors: Vec::new(),
            contract_missing: Vec::new(),
            last_seen_ms: 1,
            unresponsive: false,
            lifecycle: "per_session".into(),
            lifecycle_declared: Some("per_session".into()),
        });
        assert_eq!(reg.tool_hints("editor:1", "read").map(|h| h.destructive), Some(false));
        assert_eq!(reg.tool_hints("editor:1", "edit").map(|h| h.destructive), Some(true));
        assert_eq!(reg.tool_hints("editor:1", "没这个工具"), None);
    }
}
