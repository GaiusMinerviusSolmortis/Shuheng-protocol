//! **契约闸门**：manifest 合不合规、结果合不合自己声明的形。
//!
//! 规范-工具服务 §11.3 的原则是"**规范是唯一真相，运行时从它派生**"，
//! 而且原文点了一句要紧的话：
//!
//! > 手写一份 spec、代码里再写一份 —— **两份一定会漂移**。
//!
//! 这句话在 2026-09-19 被实测证实了：规范 §11.1 那张示意表写的是
//! `description` 加 `parameters`，§11.6 那份填完的示例写的是 `summary` 加
//! `input_schema` 加嵌套的 `annotations` —— **规范自己跟自己不一致**，
//! 而代码跟的是前者。所以这一层不是"多写一个检查"，它是**唯一能防漂移的机制**：
//!
//! | 闸门 | 谁执行 | 什么时候 | 拦什么 |
//! |---|---|---|---|
//! | [`check_manifest`] | **中控**（收到 presence 时）| 注册 | 自报的家门不全 / 声明了自己做不到的事 |
//! | [`check_result`] | **中控**（转投结果前）| 每次调用 | 回来的东西和它自己声明的形状对不上 |
//!
//! **为什么都在中控**：§11.2 硬规则二那句"**验证是独立的一次调用**" ——
//! 服务自己检查自己，等于让被验证的人自己写成绩单。中控验的是**机械事实**
//! （字段在不在、枚举里有没有），**不碰任何工具语义**，所以这跟
//! "内核不该懂每个工具的内部"（§5.10 / §5.15）不冲突。

use serde_json::{json, Map, Value};

/// 结果的**信封字段** —— 协议级的公共词汇（§11.2 五态 + §11.5 错误码 +
/// 报错即手册那三段）。**这些字段不算"追加字段"**，任何工具都能用。
pub const ENVELOPE_KEYS: &[&str] = &[
    "effect",
    "code",
    "route",
    "why",
    "remedy",
    "expect",
    "granted",
    "degraded",
    "degraded_reason",
    "data",
    "output",
    "text",
];

/// §11.2 的五态。**闭合枚举** —— 出现第六个值就是契约破了。
pub const EFFECTS: &[&str] = &[
    "confirmed",
    "partial",
    "unverifiable",
    "suspected_noop",
    "refused",
];

/// §11.2.2 的投递方式。**枚举的是"怎么送出去的"，不是"哪个应用"。**
///
/// ⚠️ **2026-09-20：这一份和 schema 里那份原来是两份抄写 —— 已经合成一份了。**
/// 加 `"process"` 那一档时只改了 schema 那边的字面量，于是**闸门当场把桥的每一条
/// 结果都判成违规**（"`route` 是 `process`，不在闭合取值里"）——
/// 十次调用十条违规。这正是项目一直说的"**两份真相一定会漂**"，
/// 而它漂的地方还偏偏是"自己查自己"的那道闸门。
pub const ROUTES: &[&str] = &[
    "accessibility",
    "synthetic_events",
    "global_input",
    "dom",
    "trusted_input",
    // **交给另一个进程**（stdio / 套接字 / 管道）：投递到了，"做没做"只有它的回执说了算。
    // 加它是为了让 MCP 桥能诚实地说"这条账是关于那条路的"（原来它只能报 `null`，
    // 而那会撞上"`route` 为 null 时不许说 unverifiable"那条不变量）。
    "process",
];

/// 五态里**表示"不知道对方做没做"**的那两个。
///
/// ⚠️ 注意 `partial` **不在**这一组里 —— 这是 2026-09-19 对着代码改的一处：
/// 规范原来把 `partial` 和这两个一起归到"投递了但不知道到没到"，
/// 于是推出一条不变量"`route` 为 `null` 时只允许 `confirmed` / `refused`"。
/// 而**实现自己就违反它**：本地命令超时被杀、mock 收到 cancel，
/// 都发生在**没有投递环节**的地方。它们不是"不知道到没到"，
/// 是"**做了一半就停了，而且停在哪很清楚**"。
/// 所以那条不变量收窄成：`route` 为 null 只禁这两个不确定态。
pub const UNCERTAIN_EFFECTS: &[&str] = &["unverifiable", "suspected_noop"];

/// §11.5 的码表 —— **每个服务都可能**回的标准码（能力拒绝就是公共件给的）。
pub const STANDARD_CODES: &[&str] = &[
    "service_down",
    "instance_gone",
    "busy",
    "refused",
    "invalid",
    "timeout",
];

/// §11.1 B 组 `injection_policy` 的闭合取值（规范原来只写了中文描述，没定字面量）。
pub const INJECTION_POLICIES: &[&str] = &["card", "help", "full"];

/// §11.1 E 组 `lifecycle` 的闭合取值。
pub const LIFECYCLES: &[&str] = &["per_call", "per_session", "resident"];
/// **中控认识的那一版契约** —— 它就是**协议的那一版**，不是另立一个号
/// （规范-工具服务 §11.6 #8 / 规范-消息协议 §9）。
///
/// ⚠️ **2026-09-27 之前这里写的是 `"1"`，而闸门写的是 `v == CONTRACT_VERSION`。**
/// 那个等号就是"没有兼容处理"：它把"加一个可选字段"这种本该系统性地兼容的改动，
/// 变成"所有老服务当场被拒"。现在规则是**看 major**（`harness_core::version`）——
/// 唯一真相也搬过去了，**这里不再抄一份版本号**。
pub const CONTRACT_VERSION: &str = harness_core::version::PROTOCOL_VERSION;

/// **沙箱的五个面**（04-安全 §5.8 决定二）—— 每个面一个**闭集**。
///
/// 为什么要有这个字段（2026-09-20）：§5.8 把方向定死了（"策略写在**能枚举的维度**上"、
/// "身份 ID 选的是**这个进程看得见什么世界**"），但**没有任何一处声明**告诉我：
/// 这个服务**实际**强制了哪几个面。于是"围栏"这个词在代码里指两件不同的事 ——
///
/// * **绑定的范围**（这个实例管哪条路径，中控拿它做授权检查）；
/// * **沙箱**（它拉起的进程看得见多大的世界）。
///
/// 终端那份围栏是前者：**`run` 拉起的命令看得见整台机器**。
/// 这两件事混着叫"围栏"，是"看起来有边界、其实没有"最容易发生的地方。
///
/// 所以：**五个面必须逐面声明**，而且取值只能从**有实现支撑的**那几个里挑
/// （"没实现的不许声明"—— 同 `lifecycle` 那条纪律）。
///
/// ⚠️ **它还只是"声明"，不是"强制"** —— 没有这个字段照样能被绕过。
/// 它买到的是**可读性**：谁能一眼看出"这套系统现在到底挡得住什么"。
/// 挑 profile 那件事（待定 #3）要先有这个，否则是在猜。
pub const SANDBOX_FACES: &[(&str, &[&str])] = &[
    // 它自己（或它拉起的进程）能碰到哪些路径
    ("filesystem", &["fence-args", "none"]),
    // 能不能出网
    ("network", &["none", "unrestricted"]),
    // 以哪个 OS 用户跑 —— 现在只有一种（**别的都还没实现**）
    ("identity", &["same-user"]),
    // 拉起的进程看得见哪些可执行文件
    ("executables", &["none", "inherit"]),
    // CPU / 内存 / 时间上限。
    // * `timeout`：只有**墙钟**超时（到点掐掉）；
    // * `limits`：**墙钟 + 内核执行的硬限额**（内存 / 进程数 / CPU 时间）——
    //   2026-09-20 终端做了这一档，值跟着事实走（开了才报）。
    ("resources", &["none", "timeout", "limits"]),
];

/// **存活状态**的词表 —— 这一轴归**中控**，不归服务。
/// **能力令牌表**（规范-工具服务 §11.6 那张表）。
///
/// 为什么它必须是一张**闭合**的表（2026-09-27 补）：
/// `capabilities` 是**鉴权**用的 —— 中控按它签发、服务按它校验。
/// 而在这之前，闸门只验"是不是字符串数组"，于是**编一个没人认识的名字**
/// （`fs.wirte` / `super.power`）可以一路签下去：
///
/// * 拼错了 ⇒ 工具永远拒，而拒的话里必须自己比对两个词才看得出错在哪儿；
/// * 编一个 ⇒ **看起来多了一条权限，其实什么也没有** —— "看起来有边界、其实没有"那族。
///
/// 令牌本身只是**标签**，范围（scope）由各个域自己定（§11.6）：
/// 文件域是围栏、记忆域是锚点、MCP 桥是部署方那份白名单。
/// **加一条就要同时改这张表和规范里那张表** —— 有一条判据盯着它们一致
/// （`check-manifests.ps1`），因为 Markdown 那边的表没法被编译进来。
pub const CAPABILITIES: &[&str] = &[
    // 编辑器
    "fs.read",
    "fs.write",
    // 终端
    "proc.exec",
    // 记忆服务（作用域是**锚点**）
    "mem.read",
    "mem.write",
    "mem.curate",
    // 技能服务
    "skill.read",
    // MCP 桥（作用域是部署方给的 `--allow` 白名单）
    "mcp.call",
];

/// 这个名字是不是一个**认识的能力**？不认识的话给出那句话（含合法清单）。
pub fn unknown_capability(name: &str) -> Option<String> {
    if CAPABILITIES.contains(&name) {
        return None;
    }
    Some(format!(
        "不认识的能力 `{name}` —— 令牌表是闭合的（规范-工具服务 §11.6）。现在有：{}。\
         令牌本身只是**标签**：要新加一条，得同时改那张表和 `CAPABILITIES`",
        CAPABILITIES.join(" / ")
    ))
}

///
/// 为什么要把两个轴分开：`state_vocabulary` 原来把两种东西混在一个字段里 ——
/// **进程活着没有**（中控看得到）和**这个实例在干什么**（只有服务知道）。
/// 混着写的代价很实际：后端渲染时不知道该画成"节点灰了"还是"这个点变黄了"。
///
/// | 轴 | 谁定义 | 值从哪来 |
/// |---|---|---|
/// | **存活**（本表）| **中控** | `presence.state` + 它自己的观察（探针、进程退出）|
/// | **域内**（`state_vocabulary`）| 服务 | 实例事件里的 `state` |
pub const LIVENESS_WORDS: &[&str] = &["starting", "online", "offline", "degraded", "unresponsive"];

/// 一份 manifest 的体检结果。
///
/// **两级是有意的**：`errors` 是"你报的家门对不上"，`missing_optional` 是
/// "这几项规范要求了、你还没报"。分两级的理由很实在 ——
/// `lifecycle` / `idle_timeout` / `health` 这三样**运行时还没人执行**，
/// 现在把它们当硬错误拦下来，只会逼着每个服务**声明自己做不到的事**，
/// 那正是这份文档一直在防的"隐性假设"。
#[derive(Debug, Default, Clone, PartialEq)]
pub struct ManifestReport {
    pub errors: Vec<String>,
    pub missing_optional: Vec<String>,
    /// **收下、但要说一声。**
    ///
    /// 现在只有一样东西走这条路：`contract_version` 的 **minor 比我们新**
    /// （规范-消息协议 §9）。它**不是错** —— 同一个 major，对面加的是可选字段，
    /// 我们照常接活、只是把那些没见过的字段当没有。
    ///
    /// 那为什么还要记一笔：将来出了怪事，"原来是它带了扩展"这件事必须能
    /// **从日志里查出来**，而不是靠谁记得。同"服务表里能力拼错要出声"那条。
    pub notes: Vec<String>,
}

impl ManifestReport {
    pub fn is_ok(&self) -> bool {
        self.errors.is_empty()
    }
    /// **收下了，但有没有话要说？**
    pub fn has_notes(&self) -> bool {
        !self.notes.is_empty()
    }
    /// 给人和模型看的一句话。
    pub fn summary(&self) -> String {
        if self.is_ok() {
            return "manifest 合规".to_string();
        }
        format!(
            "manifest 有 {} 处不合规：{}",
            self.errors.len(),
            self.errors.join("；")
        )
    }
}

/// **构造一个工具的 `output_schema`。**
///
/// 为什么要一个构造函数、而不是让每个服务手写整份 JSON：
/// `effect` / `code` / `route` / `why` / `remedy` 这些是**协议自己定的**，
/// 不该由每个服务各抄一遍（抄 21 份 = 21 个漂移点）。服务只声明
/// **它自己的字段**（`own`），信封由这里补上，两边拼成一份**闭合的** schema。
///
/// 这也是规范那句话的落地："manifest **可以生成** —— 规范管接口，不管工艺"。
pub fn result_schema(required_extra: &[&str], own: Value) -> Value {
    result_schema_full(required_extra, own, &[])
}

/// 同 [`result_schema`]，但多一层**"只在某种效果下才必填"**。
///
/// 为什么需要它：`open` 成功时必须有 `instance_id` —— 那是模型接着要用的地址；
/// 可它被 `busy` 挡下时**没有** instance_id，硬要求就等于让声明变成谎话。
/// JSON Schema 表达这件事的标准写法是 `allOf` + `if` / `then`，这里就用它。
///
/// ```no_run
/// # use serde_json::json;
/// # use harness_service::contract::result_schema_full;
/// let s = result_schema_full(
///     &[],
///     json!({"instance_id": {"type": ["string", "null"]}}),
///     &[(&["confirmed"], &["instance_id"])],
/// );
/// ```
pub fn result_schema_full(
    required_extra: &[&str],
    own: Value,
    required_when: &[(&[&str], &[&str])],
) -> Value {
    let owned: Vec<(Vec<String>, Vec<String>)> = required_when
        .iter()
        .map(|(a, b)| {
            (
                a.iter().map(|s| s.to_string()).collect(),
                b.iter().map(|s| s.to_string()).collect(),
            )
        })
        .collect();
    build_result_schema(required_extra, own, &owned)
}

fn build_result_schema(
    required_extra: &[&str],
    own: Value,
    required_when: &[(Vec<String>, Vec<String>)],
) -> Value {
    let mut props = Map::new();
    props.insert("effect".into(), json!({"enum": EFFECTS}));
    props.insert("code".into(), json!({"type": ["string", "null"]}));
    // **从 `ROUTES` 派生**，不许再抄一遍字面量 —— 抄一份就会漂一份
    // （2026-09-20 加 `"process"` 时漂过，闸门自己把桥的十条结果全判成违规）。
    let mut route_values: Vec<Value> = vec![Value::Null];
    route_values.extend(ROUTES.iter().map(|r| json!(r)));
    props.insert("route".into(), json!({"enum": route_values}));
    for k in ["why", "remedy"] {
        props.insert(k.into(), json!({"type": "string"}));
    }
    props.insert("expect".into(), json!({"type": ["object", "string"]}));
    props.insert("granted".into(), json!({"type": ["string", "array"]}));
    // **`caller_kind` 是协议自己定的字段，不是哪个服务自己的**（2026-09-20 审计补）。
    //
    // 它和 `granted` 是一对：`granted` 说"你手里有什么"，`caller_kind` 说
    // "**我认为你是谁**"（agent / user / channel / unrouted）。拒的时候两个都要给 ——
    // "你没经过网关"和"你经过了但没这个能力"是**两种不同的补救**，
    // 模型得能分清自己撞上的是哪一种（`refuse_payload` 就是照这个分的文案）。
    //
    // ⚠️ 它原来**漏在这儿**，于是**每一次能力拒绝都在违反它自己声明的形状**：
    // 投递闸门每次都得贴一张 `schema_violation`。抓它的是 2026-09-20 那轮算账 ——
    // `smoke-help-grant` 的日志里躺着一条，只是当时没人看那个字段。
    props.insert("caller_kind".into(), json!({"type": "string"}));
    props.insert("degraded".into(), json!({"type": "boolean"}));
    props.insert("degraded_reason".into(), json!({"type": "string"}));
    props.insert("data".into(), json!({"type": "object"}));
    props.insert("output".into(), json!({"type": "string"}));
    props.insert("text".into(), json!({"type": "string"}));
    if let Some(obj) = own.as_object() {
        for (k, v) in obj {
            props.insert(k.clone(), v.clone());
        }
    }
    let mut required = vec![json!("effect"), json!("code"), json!("route")];
    for r in required_extra {
        required.push(json!(r));
    }
    let mut schema = json!({
        "type": "object",
        "required": required,
        "additionalProperties": false,
        "properties": Value::Object(props),
    });
    if !required_when.is_empty() {
        let mut branches = Vec::new();
        for (effects, req) in required_when {
            branches.push(json!({
                "if": {"properties": {"effect": {"enum": effects}}, "required": ["effect"]},
                "then": {"required": req}
            }));
        }
        schema["allOf"] = Value::Array(branches);
    }
    schema
}

/// 从 manifest 里取某个工具**声明**的 `output_schema`。
pub fn tool_schema(manifest: &Value, tool: &str) -> Option<Value> {
    manifest
        .get("tools")?
        .as_array()?
        .iter()
        .find(|t| t.get("name").and_then(Value::as_str) == Some(tool))
        .and_then(|t| t.get("output_schema").cloned())
}

/// 这份 manifest **报过**这个工具吗？
///
/// 它和"报过但没给结果形状"是两件事：前者只是服务没这个工具
/// （那时回一条 `invalid` 就是正确的回答），后者才是契约缺项。
pub fn declares_tool(manifest: &Value, tool: &str) -> bool {
    manifest
        .get("tools")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .any(|t| t.get("name").and_then(Value::as_str) == Some(tool))
        })
        .unwrap_or(false)
}

/// **一个工具的自述**，构造器版本。
///
/// 为什么要有它：§11.1 C 组一共八个字段，而它们**每个工具都得填一遍** ——
/// 手写 21 份 JSON 就是 21 个漂移点，而且总会漏掉一个两个（这正是这一轮
/// 之前的状态：`output_schema` / `lifecycle` / `idle_timeout` / `health`
/// 全都没人写）。构造器把**信封那一半**和**必填的那几项**收进一处，
/// 服务只写它真正独有的东西：**名字、说明、参数表、它自己的结果字段**。
///
/// 注意 `lifecycle` / `idle_timeout` / `health` 这三项**没有 setter**：
/// 现在只有一种诚实的取值（`per_call` 或 `per_session` / `null` / `ping`），
/// 而"声明一个没人执行的能力"会被 [`check_manifest`] 拦下。想开放它们，
/// 得先把运行时真的做出来 —— 那时候再加 setter。
#[derive(Debug, Clone)]
pub struct ToolDecl {
    name: String,
    description: String,
    capabilities: Vec<String>,
    read_only: bool,
    destructive: bool,
    idempotent: bool,
    open_world: bool,
    input_schema: Value,
    output_fields: Value,
    required_when: Vec<(Vec<String>, Vec<String>)>,
    lifecycle: &'static str,
    /// **"停下来"的入口**（2026-09-20）：中控要收掉这个服务时，调哪个工具。
    ///
    /// 为什么这个入口得由服务自己声明：常驻服务（`resident`）**可能不是这个会话
    /// 拉起来的** —— 那时候中控手里没有那个子进程句柄，"杀进程"根本做不到。
    /// 而"我怎么体面地停"只有服务自己知道（要落盘什么、要留什么遗嘱）。
    /// 形状照 `health` 抄：服务声明入口，中控照着调，**中控不猜**。
    stop: Option<String>,
}

impl ToolDecl {
    pub fn new(name: &str, description: &str, input_schema: Value) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            capabilities: Vec::new(),
            read_only: false,
            destructive: false,
            idempotent: false,
            open_world: false,
            input_schema,
            output_fields: json!({}),
            required_when: Vec::new(),
            stop: None,
            // **默认必须和中控那边的部署默认一致**（都是"跟着会话"）。
            // 第一版我把默认写成 `per_call`，而几个工具显式写了 `per_session` ——
            // 于是**服务自己内部就不一致**，闸门当场把它拦了（那条"一个进程只能有一条
            // lifecycle"的规则第一次生效就抓到了我自己）。默认值不一致是这个坑的根。
            lifecycle: "per_session",
        }
    }
    pub fn caps(mut self, c: &[&str]) -> Self {
        self.capabilities = c.iter().map(|s| s.to_string()).collect();
        self
    }
    /// 只读：不改任何东西。
    pub fn read_only(mut self) -> Self {
        self.read_only = true;
        self
    }
    /// 有副作用，而且**改坏了撤不回来**（`destructiveHint`，§11.1）。
    pub fn destructive(mut self) -> Self {
        self.destructive = true;
        self
    }
    /// 同样的事做两次和做一次一样。
    pub fn idempotent(mut self) -> Self {
        self.idempotent = true;
        self
    }
    /// 会碰到托管范围外面的东西（网页、公网……）。
    pub fn open_world(mut self) -> Self {
        self.open_world = true;
        self
    }
    /// 这个工具**自己**的结果字段（信封不用管，`result_schema` 会补）。
    pub fn returns(mut self, fields: Value) -> Self {
        self.output_fields = fields;
        self
    }
    /// **只在某些效果下**才必填的字段。见 [`result_schema_full`] 里那段解释。
    pub fn requires_on(mut self, effects: &[&str], fields: &[&str]) -> Self {
        self.required_when.push((
            effects.iter().map(|s| s.to_string()).collect(),
            fields.iter().map(|s| s.to_string()).collect(),
        ));
        self
    }
    pub fn lifecycle(mut self, l: &'static str) -> Self {
        self.lifecycle = l;
        self
    }
    /// 声明"收我的时候调哪个工具"。**只给真的实现了停下来的服务用** ——
    /// 声明了却停不下来，比不声明更糟（中控会等它，然后超时）。
    pub fn stop_entry(mut self, tool: &str) -> Self {
        self.stop = Some(tool.to_string());
        self
    }
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn to_json(&self) -> Value {
        let mut v = json!({
            "name": self.name,
            "description": self.description,
            "input_schema": self.input_schema,
            "output_schema": build_result_schema(&[], self.output_fields.clone(), &self.required_when),
            "capabilities": self.capabilities,
            "annotations": {
                "readOnlyHint": self.read_only,
                "destructiveHint": self.destructive,
                "idempotentHint": self.idempotent,
                "openWorldHint": self.open_world,
            },
            "lifecycle": self.lifecycle,
            "idle_timeout": Value::Null,
            "health": "ping",
        });
        if let Some(s) = &self.stop {
            v["stop"] = json!(s);
        }
        v
    }
}

/// 从 manifest 里取某个工具声明需要的能力（§7 的 `required_caps` 那一半）。
pub fn tool_capabilities(manifest: &Value, tool: &str) -> Vec<String> {
    manifest
        .get("tools")
        .and_then(Value::as_array)
        .and_then(|a| {
            a.iter()
                .find(|t| t.get("name").and_then(Value::as_str) == Some(tool))
        })
        .and_then(|t| t.get("capabilities"))
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
        .unwrap_or_default()
}

fn need_str(m: &Value, key: &str, out: &mut Vec<String>) -> bool {
    match m.get(key).and_then(Value::as_str) {
        Some(s) if !s.trim().is_empty() => true,
        Some(_) => {
            out.push(format!("`{key}` 是空串 —— 报了等于没报"));
            false
        }
        None => {
            out.push(format!("少 `{key}`（§11.1 A 组）"));
            false
        }
    }
}

fn need_str_array(m: &Value, key: &str, out: &mut Vec<String>) -> bool {
    match m.get(key) {
        Some(Value::Array(a)) if a.iter().all(Value::is_string) => true,
        Some(Value::Array(_)) => {
            out.push(format!("`{key}` 里混了非字符串"));
            false
        }
        Some(_) => {
            out.push(format!("`{key}` 应该是字符串数组"));
            false
        }
        None => {
            out.push(format!("少 `{key}`（§11.1 E 组）"));
            false
        }
    }
}

/// **这份 manifest 合规吗？**（注册那一刻的闸门）
///
/// 查的是 §11.1 的 A / B / C / E 四组最低集，外加两条"别声明自己做不到的事"：
/// `idle_timeout` 只允许 `null`（闲置回收还没实现）、`health` 只允许 `ping`
/// （我们只有这一种探针，规范洞 #7 就是它）。**声明一个没人执行的能力，
/// 比不声明更糟** —— 那正是规范里那些洞变成隐性假设的方式。
pub fn check_manifest(m: &Value) -> ManifestReport {
    let mut r = ManifestReport::default();
    // **"收下、但要说一声"** 的那几条 —— 攒着，等 `r.errors` 那笔借用还回去再装车。
    let mut notes: Vec<String> = Vec::new();

    // ---- A 身份 ----
    {
        let e = &mut r.errors;
        // **契约版本**（规范-工具服务 §11.6 #8 / 规范-消息协议 §9）：
        // **同 major 就谈，major 对不上就按名字拒** —— 不认识的版本不许照旧解析，
        // 那正是"版本号白写"的经典死法（拿到 2 的字段集合却按 1 去读，
        // 报出来的错会指向"少字段"，而不是"版本不对"）。
        //
        // ⚠️ **这里原来是个等号**（`v == CONTRACT_VERSION`）。等号的问题不在"太严"，
        // 在"把兼容的改动逼成不兼容的"：服务加一个可选字段、抬一下 minor，
        // 老中控当场把它整个拒掉。判词和"怎么办"都归 `Compat`（一处写）。
        let mine = harness_core::version::protocol_version();
        match m.get("contract_version").and_then(Value::as_str) {
            Some(v) => match harness_core::Version::parse(v) {
                None => e.push(format!(
                    "`contract_version` 是 `{v}`，这不是一个版本号 —— \
                     版本写成 `major.minor`（如 `{CONTRACT_VERSION}`），\
                     缺 minor 就写 `{major}`。拼错了和版本对不上是**两回事**：\
                     这一条说明它是个笔误，不是我们在谈两个不同的形状",
                    major = mine.major
                )),
                Some(theirs) => {
                    let how = mine.speak(theirs);
                    if let Some(why) = how.why(mine, theirs) {
                        e.push(format!("`contract_version` —— {why}"));
                    }
                    // 收下、但要说一声：**它带了我没见过的字段**。
                    if let Some(note) = how.note(mine, theirs) {
                        notes.push(note);
                    }
                }
            },
            None => e.push(
                "少 `contract_version` —— 没有它，中控就只能**猜**该按哪一版解释这份 manifest"
                    .into(),
            ),
        }
        need_str(m, "id", e);
        need_str(m, "version", e);
        need_str(m, "title", e);
        need_str(m, "kind", e);
        need_str_array(m, "platforms", e);

        // ---- B 发现（L0 名片 / help 章节 / 注入策略）----
        need_str(m, "summary", e);
        need_str(m, "when_to_use", e);
        match m.get("injection_policy").and_then(Value::as_str) {
            // **`full` 在规范的闭合取值里，但没有执行者** —— 谁执行注入？提示词编译器。
            // 而它拿不到手册**正文**（正文不在 manifest 里，要向服务要一次，那条路还没做）。
            // 所以这里拦下来，理由和 `idle_timeout` 那条一样：
            // **声明一个没人执行的能力，比不声明更糟** —— 部署方会以为它生效了。
            Some("full") => e.push(
                "`injection_policy: full` **没有执行者**：手册正文不在 manifest 里，\
                 注入时要向服务要一次，那条路还没做（§11.1 B 组）。\
                 现在真正有执行者的是 `card`（只报名片）与 `help`（名片 + 章节目录）"
                    .into(),
            ),
            Some(p) if INJECTION_POLICIES.contains(&p) => {}
            Some(p) => e.push(format!(
                "`injection_policy` 是 `{p}`，不在闭合取值里（{}）",
                INJECTION_POLICIES.join(" | ")
            )),
            None => e.push("少 `injection_policy`（§11.1 B 组）".into()),
        }
        match m.get("help_sections") {
            Some(Value::Array(a)) if !a.is_empty() => {
                for s in a {
                    if s.get("id").and_then(Value::as_str).is_none()
                        || s.get("title").and_then(Value::as_str).is_none()
                    {
                        e.push(format!("`help_sections` 有一章没写 id/title：{s}"));
                    }
                }
            }
            _ => e.push(
                "`help_sections` 要是非空数组 —— 渐进披露的第二层就靠它（§5.3）".into(),
            ),
        }

        // ---- E 面板（图上的形状）----
        need_str(m, "node_kind", e);
        need_str_array(m, "state_vocabulary", e);
        // **`state_vocabulary` 只装"实例在干什么"，不装"进程活着没有"。**
        // 后者归中控（它才看得见进程），写进来就是把两个轴又混一起了。
        if let Some(Value::Array(a)) = m.get("state_vocabulary") {
            for v in a.iter().filter_map(Value::as_str) {
                if LIVENESS_WORDS.contains(&v) {
                    e.push(format!(
                        "`state_vocabulary` 里的 `{v}` 是**存活状态**（{}）—— \
                         那一轴归中控，别混进实例状态里；实例状态只该有\
                         「这个实例在干什么」（如 idle / running / held）",
                        LIVENESS_WORDS.join(" / ")
                    ));
                }
            }
        }
        need_str_array(m, "instance_types", e);
        need_str_array(m, "node_actions", e);
        // `content`：规范 §11.1 的示例写的是单个字符串，编辑器实际要报两样
        // （document + log）。**这里两种都收** —— 洞 #4（"content 的取值说不清"）
        // 还没解，先不把形状卡死。
        match m.get("content") {
            Some(Value::String(_)) => {}
            Some(Value::Array(a)) if !a.is_empty() && a.iter().all(Value::is_string) => {}
            _ => e.push("`content` 要不是字符串，要不是非空字符串数组（§11.1 E 组）".into()),
        }
        if let Some(owns) = m.get("owns") {
            let ok = owns.get("domain").and_then(Value::as_str).is_some()
                && owns.get("exclusive_per").and_then(Value::as_str).is_some();
            if !ok {
                e.push("`owns` 要写清 domain 和 exclusive_per".into());
            }
        }
        if let Some(f) = m.get("fence") {
            if !f.is_array() {
                e.push("`fence` 要是字符串数组（它报的是规范化之后的围栏）".into());
            }
        }
        // ---- **沙箱五面**（2026-09-20）----
        //
        // 三件事都要查，而它们的形状不一样：
        //   ① **齐不齐**（五个面一个不许少）—— "说不清"就是没说清；
        //   ② **值合不合法**（每面一个闭集）—— 而且闭集里只放**有实现支撑的**值；
        //   ③ **声明和自己的围栏对不对得上**：声称 `fence-args` 却没报围栏
        //      ⇒ 那是"说我围了、其实没围"。
        //
        // ⚠️ **反方向不查**（报了围栏但 filesystem 是 `none` 是合法的）：
        // 围栏在代码里还兼着"**绑定的范围**"那件事 —— 一个终端可以"管着这条路径"
        // 而"拉起的命令看得见整台机器"。这两件事混着叫"围栏"，
        // 正是这个字段要拆开的东西。
        match m.get("sandbox") {
            Some(Value::Object(faces)) => {
                for (face, allowed) in SANDBOX_FACES {
                    match faces.get(*face).and_then(Value::as_str) {
                        Some(v) if allowed.contains(&v) => {}
                        Some(v) => e.push(format!(
                            "`sandbox.{face}` 是 `{v}`，不在这一面的闭集里（{}）—— \
                             闭集里只放**有实现支撑的**值：没实现的东西不许声明",
                            allowed.join(" / ")
                        )),
                        None => e.push(format!(
                            "`sandbox` 少了 `{face}` 这一面 —— \
                             五个面（{}）一个都不许少：**说不清就是没说清**",
                            SANDBOX_FACES
                                .iter()
                                .map(|(f, _)| *f)
                                .collect::<Vec<_>>()
                                .join(" / ")
                        )),
                    }
                }
                for k in faces.keys() {
                    if !SANDBOX_FACES.iter().any(|(f, _)| f == k) {
                        e.push(format!(
                            "`sandbox` 里有个没听说过的面 `{k}` —— \
                             要么它是新的面（那样得先加进规范），要么是打错了"
                        ));
                    }
                }
                if faces.get("filesystem").and_then(Value::as_str) == Some("fence-args") {
                    let has_fence = m
                        .get("fence")
                        .and_then(Value::as_array)
                        .map(|a| !a.is_empty())
                        .unwrap_or(false);
                    if !has_fence {
                        e.push(
                            "`sandbox.filesystem: fence-args` 说「带路径的参数都过围栏」—— \
                             可它没报任何 `fence`。**说了要围却没围栏**，这条声明是假的"
                                .into(),
                        );
                    }
                }
            }
            Some(_) => e.push("`sandbox` 要是对象（五个面做它的键）".into()),
            None => e.push(
                "少 `sandbox`（§11.1 的五个面）—— 少它的话，\
                 「这套系统现在到底挡得住什么」就只能靠读代码猜"
                    .into(),
            ),
        }
        match m.get("sandbox_note").and_then(Value::as_str) {
            Some(s) if !s.trim().is_empty() => {}
            _ => e.push(
                "少 `sandbox_note`（或者说它是空的）—— \
                 **一句话说清这套声明意味着什么、不意味着什么。**\
                 它比那五个格子更重要：格子说的是「我盖了章」，这句话说的是「章盖在哪儿」"
                    .into(),
            ),
        }
    match m.get("error_codes") {
        Some(Value::Array(a)) if a.iter().all(Value::is_string) => {}
        _ => e.push("`error_codes` 要是字符串数组（模型靠它判断重试有没有用）".into()),
    }
    // ---- **遗嘱**（§19.2 抄 MQTT 的那条，2026-09-20 补上后半）----
    //
    // MQTT 的遗嘱是"客户端在连接时登记一条消息，它非正常断开时由 broker 代发"。
    // 我们抄的是同一个形状：**服务登记、中控代发** —— 因为"我崩了意味着什么"
    // 只有服务自己知道（它手里攥着哪些锁、起了哪些子进程）。
    // 不block、只提醒：少写一句不该让一个服务接不了活。
    match m.get("will") {
        Some(Value::String(s)) if !s.trim().is_empty() => {}
        Some(Value::String(_)) => e.push("`will`（遗嘱）是空串 —— 报了等于没报".into()),
        Some(_) => e.push("`will`（遗嘱）要是字符串：一句话说清「我没了意味着什么」".into()),
        None => r
            .missing_optional
            .push("没写 `will`（遗嘱）：你崩了之后，中控发给智能体的敕令里「影响」那一段只能用一句套话 —— 而「我手里那几个锁会释放、我起的子进程会被带走」只有你知道".into()),
    }
    // A~E 那一段查完了：把"收下但要说一声"的那几条装车。
    r.notes = notes;
    }

    // ---- 工具表（C 组：一层一张）----
    let empty = Vec::new();
    let tools = match m.get("tools").and_then(Value::as_array) {
        Some(t) if !t.is_empty() => t,
        Some(_) => {
            r.errors
                .push("`tools` 是空表 —— 一个工具都不报的服务拿不到任何调用".into());
            &empty
        }
        None => {
            r.errors.push("少 `tools`（§11.1 C 组）".into());
            &empty
        }
    };
    let mut seen: Vec<&str> = Vec::new();
    // 先收一份**全部工具名**：`stop` 那条检查要能指向后面声明的工具，
    // 用边扫边攒的 `seen` 会变成"顺序对了才查得出来"（那种检查等于没查）。
    let all_tool_names: Vec<String> = tools
        .iter()
        .filter_map(|t| t.get("name").and_then(Value::as_str).map(str::to_string))
        .collect();
    // 每个工具声明的 lifecycle（用来查"同一个服务的工具声明是否一致"）
    let mut lifecycle_seen: Vec<(String, String)> = Vec::new();
    for t in tools {
        let name = match t.get("name").and_then(Value::as_str) {
            Some(n) if !n.is_empty() => n,
            _ => {
                r.errors
                    .push("`tools` 里有一个没写 name 的条目".into());
                continue;
            }
        };
        if seen.contains(&name) {
            r.errors.push(format!("工具 `{name}` 报了两次"));
        }
        seen.push(name);

        // 说明：`description` 是**工具的说明**（§11.1 的示意表、三个服务都在用）。
        // 规范 §11.6 那份填完的示例写成了 `summary`，是它自己没对齐 —— 已按这版改。
        if t.get("description")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("")
            .is_empty()
        {
            r.errors.push(format!(
                "工具 `{name}` 没写 `description` —— 只报名字等于让人猜它是干什么的"
            ));
        }
        match t.get("input_schema") {
            Some(s) if s.get("type").and_then(Value::as_str) == Some("object") => {}
            Some(_) => r.errors.push(format!(
                "工具 `{name}` 的 `input_schema` 要是 type=object 的 JSON Schema"
            )),
            None => r.errors.push(format!(
                "工具 `{name}` 没报 `input_schema` —— 模型要靠它填参数（§11.3）"
            )),
        }
        match t.get("output_schema") {
            Some(s) if s.get("type").and_then(Value::as_str) == Some("object") => {
                if s.get("additionalProperties") != Some(&Value::Bool(false)) {
                    r.errors.push(format!(
                        "工具 `{name}` 的 `output_schema` 不是闭合的 —— 要写 additionalProperties: false（§11.1 C 组）"
                    ));
                }
            }
            Some(_) => r
                .errors
                .push(format!("工具 `{name}` 的 `output_schema` 要是 type=object")),
            None => r.errors.push(format!(
                "工具 `{name}` 没报 `output_schema` —— 结果形状就没人校验了（§11.1 C 组）"
            )),
        }
        match t.get("capabilities") {
            Some(Value::Array(a)) if a.iter().all(Value::is_string) => {
                // **名字必须在令牌表里**（2026-09-27 补）：拼错一个字母或者编一个，
                // 都会变成"签了却什么也管不住"的能力。
                for c in a.iter().filter_map(Value::as_str) {
                    if let Some(why) = unknown_capability(c) {
                        r.errors.push(format!("工具 `{name}`：{why}"));
                    }
                }
            }
            Some(_) => r
                .errors
                .push(format!("工具 `{name}` 的 `capabilities` 要是字符串数组")),
            None => r.errors.push(format!(
                "工具 `{name}` 没报 `capabilities`（不给就是空表 = 不需要能力）"
            )),
        }
        match t.get("annotations") {
            Some(Value::Object(a)) => {
                for k in [
                    "readOnlyHint",
                    "destructiveHint",
                    "idempotentHint",
                    "openWorldHint",
                ] {
                    if !a.get(k).map(Value::is_boolean).unwrap_or(false) {
                        r.errors.push(format!(
                            "工具 `{name}` 的 `annotations.{k}` 没报 —— 中控要靠它决定该不该画成危险色（§11.1）"
                        ));
                    }
                }
            }
            Some(_) => r.errors.push(format!(
                "工具 `{name}` 的 `annotations` 要是对象（四个 Hint 都在这底下）"
            )),
            None => r.errors.push(format!(
                "工具 `{name}` 没报 `annotations` —— GUI 就没法在不调用的前提下知道它危不危险（§11.1）"
            )),
        }
        // **别声明自己做不到的事。**
        match t.get("idle_timeout") {
            Some(Value::Null) => {}
            Some(v) => r.errors.push(format!(
                "工具 `{name}` 声明了 `idle_timeout: {v}` —— 但**没人执行它**：\
                 §5.20 定过「默认开着就是有用、不做自动回收」（回收只走四条确定路径：\
                 会话结束 / 进程退出 / 智能体自己 close / 用户手动收）。\
                 中控的 tick **只探活、不动手**（探针结果不做任何自动动作），\
                 所以那个数现在没有执行者。声明一个没人执行的能力比不声明更糟：先写 null"
            )),
            None => r.missing_optional
                .push(format!("工具 `{name}` 没报 `idle_timeout`（洞 #6）")),
        }
        match t.get("health").and_then(Value::as_str) {
            Some("ping") => {}
            Some(other) => r.errors.push(format!(
                "工具 `{name}` 的 `health` 是 `{other}` —— 现在只有 `ping` 这一种探针（洞 #7）"
            )),
            None => r.missing_optional
                .push(format!("工具 `{name}` 没报 `health`（洞 #7）")),
        }
        // **"停下来"的入口**（2026-09-20）。可选 —— 但声明了就得是**这个 manifest 里
        // 真有的工具**，否则中控收它的时候会去调一个不存在的名字（那就成了下一个洞）。
        if let Some(s) = t.get("stop") {
            match s.as_str() {
                Some(name_stop) if all_tool_names.iter().any(|t| t == name_stop) => {}
                Some(name_stop) => r.errors.push(format!(
                    "工具 `{name}` 声明的 `stop` 指向 `{name_stop}`，而这个 manifest 里没有那个工具 —— \
                     中控收它会调一个不存在的名字"
                )),
                None => r.errors.push(format!("工具 `{name}` 的 `stop` 要不是字符串，要不是不写")),
            }
        }
        match t.get("lifecycle").and_then(Value::as_str) {
            // `per_call` / `per_session` 现在**真的会执行**（决定"什么时候拉起"），见 §11.10
            Some(l) if l == "per_call" || l == "per_session" => {}
            // **`resident`（跨会话常驻）实现于 2026-09-20**：服务听在一个回环端口上、
            // 把会合文件写出来，后来的会话**接上同一个进程**而不是再起一个。
            //
            // ⚠️ 但它**不是单靠 manifest 就能声明的**：还要求部署表里给出
            // `"resident": true` + `"rendezvous": "<目录>"`（那是**部署配置**，
            // 和 `wants_fence` 同一条分工）。少了会合目录，中控没法把
            // "新会话接到老进程上"这件事做出来 —— 这条是部署侧的错，不是服务的错，
            // 所以闸门**不拦**，但中控会在装配时明确报出来（见 `spawn_spec`）。
            Some("resident") => {}
            Some(other) => r.errors.push(format!(
                "工具 `{name}` 的 `lifecycle` 是 `{other}`，不在闭合取值里（{}）",
                LIFECYCLES.join(" | ")
            )),
            None => r.missing_optional
                .push(format!("工具 `{name}` 没报 `lifecycle`")),
        }
        lifecycle_seen.push((name.to_string(), t
            .get("lifecycle")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()));
    }
    // **一个服务一个进程 ⇒ 它的所有工具必须声明同一个 lifecycle。**
    //
    // 两个工具在同一个进程里，"这个常驻、那个按调用"是没法同时成立的 ——
    // 冲突了就得说出来，而不是让中控随便挑一个。
    let mixed: Vec<&str> = lifecycle_seen
        .iter()
        .map(|(_, l)| l.as_str())
        .filter(|l| !l.is_empty())
        .collect();
    if let Some(first) = mixed.first() {
        if mixed.iter().any(|l| l != first) {
            r.errors.push(format!(
                "这些工具的 `lifecycle` 不一致（{}）—— **一个服务一个进程**，\
                 「这个常驻、那个按调用」在同一个进程里没法同时成立；改成一致的",
                lifecycle_seen
                    .iter()
                    .map(|(n, l)| format!("{n}={l}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
    }
    r
}

/// **这次回来的东西，合它自己声明的形吗？**（投递前的闸门）
///
/// 四道，从协议到工具：
/// 1. 信封三必有：`effect` / `code` / `route`；
/// 2. §11.2.2：`route` 为 `null` 就不许出现"不知道到没到"的那两态；
/// 3. 报错即手册：`effect: refused` 必须带 `why` 加 `remedy`；
/// 4. 服务自己声明的 `output_schema`。
///
/// 返回的都是**给人 / 模型看的句子** —— 它们会被原样附加到那条结果上
/// （`schema_violation`），所以措辞要能直接照做。
pub fn check_result(
    instance: &str,
    tool: &str,
    payload: &Value,
    schema: Option<&Value>,
    declared_codes: &[String],
) -> Vec<String> {
    let mut v = Vec::new();
    let obj = match payload.as_object() {
        Some(o) => o,
        None => {
            v.push("结果的 payload 不是一个对象".into());
            return v;
        }
    };

    let effect = obj.get("effect").and_then(Value::as_str);
    let route_null = matches!(obj.get("route"), Some(Value::Null));

    match effect {
        Some(e) if EFFECTS.contains(&e) => {}
        Some(e) => v.push(format!(
            "`effect` 是 `{e}`，不在五态里（{}）—— 五态是闭合的（§11.2）",
            EFFECTS.join(" | ")
        )),
        None => v.push("结果里没有 `effect` —— 那就分不清生效了没有（§11.2）".into()),
    }
    if !obj.contains_key("route") {
        v.push("结果里没有 `route` —— 没有投递环节也要显式写 null（§11.2.2）".into());
    } else if let Some(r) = obj.get("route").and_then(Value::as_str) {
        if !ROUTES.contains(&r) {
            v.push(format!(
                "`route` 是 `{r}`，不在闭合取值里（{}）",
                ROUTES.join(" | ")
            ));
        }
    }
    match obj.get("code") {
        None => v.push("结果里没有 `code` —— 成功也要写 null（§11.5）".into()),
        Some(Value::Null) => {}
        Some(Value::String(c)) => {
            let ok = STANDARD_CODES.contains(&c.as_str())
                || declared_codes.iter().any(|d| d == c);
            if !ok {
                v.push(format!(
                    "`code` 是 `{c}`，既不在 §11.5 的码表里、也没在 `error_codes` 里报过 —— \
                     模型要拿它判断重试有没有用，编一个码等于让它瞎猜"
                ));
            }
        }
        Some(_) => v.push("`code` 要不是字符串，要不是 null".into()),
    }
    if route_null {
        if let Some(e) = effect {
            if UNCERTAIN_EFFECTS.contains(&e) {
                v.push(format!(
                    "`route` 是 null 却报了 `{e}` —— 没有投递环节就没有这份不确定性（§11.2.2）"
                ));
            }
        }
    }
    if effect == Some("refused") {
        for k in ["why", "remedy"] {
            if obj
                .get(k)
                .and_then(Value::as_str)
                .map(str::trim)
                .unwrap_or("")
                .is_empty()
            {
                v.push(format!(
                    "`refused` 却没写 `{k}` —— 报错即手册：事实 + 影响 + 出口，少一样模型就得自己猜"
                ));
            }
        }
    }

    match schema {
        Some(s) => {
            let mut hits = Vec::new();
            validate(payload, s, "$", &mut hits);
            for h in hits {
                v.push(format!(
                    "{instance}/{tool} 的结果不合它自己声明过的形状：{h}"
                ));
            }
        }
        None => v.push(format!(
            "{instance}/{tool} 没声明 `output_schema`，所以这次结果没法校验（§11.1 C 组）"
        )),
    }
    v
}

/// 一个**JSON Schema 子集**校验器。
///
/// 支持的关键字就是规范示例里用到的那些：`type`（含 `["string","null"]` 这种联合）、
/// `required` / `properties` / `additionalProperties: false` / `enum` / `const` /
/// `minimum` / `items`，外加 `allOf` + `if` / `then`（"成功时才必须有那个字段"靠它）。
/// **不认识的关键字一律忽略** —— 这条要说清楚，否则以后有人以为它是个完整实现。
pub fn validate(value: &Value, schema: &Value, path: &str, out: &mut Vec<String>) {
    out.extend(violations(value, schema, path));
}

fn violations(value: &Value, schema: &Value, path: &str) -> Vec<String> {
    let mut out = Vec::new();
    let s = match schema.as_object() {
        Some(s) => s,
        None => return out,
    };

    // `allOf` / `if`-`then`：**条件式必填**（"成功时才必须有 instance_id"）。
    // 标准语义：`if` 通过了才看 `then`；`if` 本身不报错。
    if let Some(Value::Array(all)) = s.get("allOf") {
        for sub in all {
            out.extend(violations(value, sub, path));
        }
    }
    if let Some(cond) = s.get("if") {
        if violations(value, cond, path).is_empty() {
            if let Some(then) = s.get("then") {
                out.extend(violations(value, then, path));
            }
        }
    }

    if let Some(t) = s.get("type") {
        if !type_matches(value, t) {
            out.push(format!(
                "{path} 的类型不对（要求 {t}，实际 {}）",
                type_name(value)
            ));
            return out;
        }
    }
    if let Some(c) = s.get("const") {
        if value != c {
            out.push(format!("{path} 应该是 {c}，实际是 {value}"));
            return out;
        }
    }
    if let Some(Value::Array(e)) = s.get("enum") {
        if !e.iter().any(|x| x == value) {
            out.push(format!(
                "{path} 是 {value}，不在允许的取值里（{}）",
                e.iter()
                    .map(|x| x.to_string())
                    .collect::<Vec<_>>()
                    .join(" / ")
            ));
            return out;
        }
    }
    if let Some(min) = s.get("minimum").and_then(Value::as_f64) {
        if let Some(n) = value.as_f64() {
            if n < min {
                out.push(format!("{path} 小于最小值 {min}"));
            }
        }
    }
    if let Some(obj) = value.as_object() {
        if let Some(Value::Array(req)) = s.get("required") {
            for k in req.iter().filter_map(Value::as_str) {
                if !obj.contains_key(k) {
                    out.push(format!("{path} 少了必填的 `{k}`"));
                }
            }
        }
        let props = s.get("properties").and_then(Value::as_object);
        if let Some(p) = props {
            for (k, sub) in p {
                if let Some(v) = obj.get(k) {
                    out.extend(violations(v, sub, &format!("{path}.{k}")));
                }
            }
        }
        if s.get("additionalProperties") == Some(&Value::Bool(false)) {
            let known: Vec<&str> = props
                .map(|p| p.keys().map(String::as_str).collect())
                .unwrap_or_default();
            for k in obj.keys() {
                if !known.contains(&k.as_str()) {
                    out.push(format!(
                        "{path} 多了一个没声明过的字段 `{k}` —— 闭合的 schema 就是不许夹带"
                    ));
                }
            }
        }
    } else if let Some(a) = value.as_array() {
        if let Some(sub) = s.get("items") {
            for (i, v) in a.iter().enumerate() {
                out.extend(violations(v, sub, &format!("{path}[{i}]")));
            }
        }
    }
    out
}

fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(n) => {
            if n.is_i64() || n.is_u64() {
                "integer"
            } else {
                "number"
            }
        }
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn one_type_matches(v: &Value, t: &str) -> bool {
    match t {
        "null" => v.is_null(),
        "boolean" => v.is_boolean(),
        "string" => v.is_string(),
        "integer" => v.is_i64() || v.is_u64(),
        // 整数也是数字：约束不该因为 0 和 0.0 的写法差别而变得玄学
        "number" => v.is_number(),
        "array" => v.is_array(),
        "object" => v.is_object(),
        _ => true,
    }
}

fn type_matches(v: &Value, t: &Value) -> bool {
    match t {
        Value::String(s) => one_type_matches(v, s),
        Value::Array(a) => a
            .iter()
            .filter_map(Value::as_str)
            .any(|s| one_type_matches(v, s)),
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conforming_manifest() -> Value {
        json!({
            "contract_version": "1",
            "id": "demo",
            "version": "0.1.0",
            "title": "示例",
            "kind": "demo",
            "platforms": ["windows"],
            "node_kind": "demo",
            "state_vocabulary": ["idle"],
            "content": ["log"],
            "instance_types": ["mock"],
            "node_actions": ["close"],
            "summary": "一句话",
            "when_to_use": "什么时候用",
            "will": "我没了就是没了，什么都不持有。",
            "injection_policy": "card",
            "help_sections": [{"id": "overview", "title": "概览"}],
            // 沙箱五面（2026-09-20 起是必报项）
            "sandbox": {
                "filesystem": "none", "network": "none", "identity": "same-user",
                "executables": "none", "resources": "none"
            },
            "sandbox_note": "这个示例什么都不碰 —— 五个面全是「没有东西可管」。",
            "error_codes": ["invalid"],
            "tools": [{
                "name": "ping",
                "description": "看它还在不在",
                "input_schema": {"type": "object", "properties": {}},
                "output_schema": result_schema(&[], json!({})),
                "capabilities": [],
                "annotations": {
                    "readOnlyHint": true, "destructiveHint": false,
                    "idempotentHint": true, "openWorldHint": false
                },
                "lifecycle": "per_call",
                "idle_timeout": null,
                "health": "ping"
            }]
        })
    }

    #[test]
    fn a_complete_manifest_passes() {
        let r = check_manifest(&conforming_manifest());
        assert!(r.is_ok(), "{:?}", r.errors);
        assert!(r.missing_optional.is_empty(), "{:?}", r.missing_optional);
    }

    /// 缺一项就是缺一项 —— 报错要指出是哪一项，别只说"不合规"。
    #[test]
    fn missing_fields_are_named_one_by_one() {
        let mut m = conforming_manifest();
        m.as_object_mut().unwrap().remove("summary");
        m["tools"][0]
            .as_object_mut()
            .unwrap()
            .remove("input_schema");
        let r = check_manifest(&m);
        assert!(!r.is_ok());
        assert!(r.errors.iter().any(|e| e.contains("summary")), "{:?}", r.errors);
        assert!(r.errors.iter().any(|e| e.contains("input_schema")), "{:?}", r.errors);
    }

    /// 声明一个没人执行的能力，比不声明更糟（那个洞会变成隐性假设）。
    #[test]
    fn declaring_an_unimplemented_idle_timeout_is_refused() {
        let mut m = conforming_manifest();
        m["tools"][0]["idle_timeout"] = json!(30000);
        let r = check_manifest(&m);
        assert!(
            r.errors.iter().any(|e| e.contains("idle_timeout")),
            "{:?}",
            r.errors
        );
    }

    /// **遗嘱**：可以不写（只是提醒），但写了就必须是句人话。
    #[test]
    fn the_will_is_optional_but_must_be_something() {
        let mut m = conforming_manifest();
        m.as_object_mut().unwrap().remove("will");
        let r = check_manifest(&m);
        assert!(r.is_ok(), "少写遗嘱不该让人接不了活：{:?}", r.errors);
        assert!(
            r.missing_optional.iter().any(|w| w.contains("will")),
            "但要在「还差什么」里提醒：{:?}",
            r.missing_optional
        );

        let mut m2 = conforming_manifest();
        m2["will"] = json!(123);
        let r2 = check_manifest(&m2);
        assert!(r2.errors.iter().any(|w| w.contains("will")), "{:?}", r2.errors);
    }

    /// **两个轴不能混**：`state_vocabulary` 只装"实例在干什么"，
    /// 存活状态（`degraded` 之类）归中控那一轴。
    #[test]
    fn liveness_words_do_not_belong_in_the_instance_vocabulary() {
        let mut m = conforming_manifest();
        m["state_vocabulary"] = json!(["idle", "degraded"]);
        let r = check_manifest(&m);
        assert!(
            r.errors.iter().any(|w| w.contains("存活状态")),
            "{:?}",
            r.errors
        );
    }

    /// **一个服务一个进程** ⇒ 它的所有工具必须声明同一个 `lifecycle`。
    /// 「这个常驻、那个按调用」在同一个进程里没法同时成立。
    #[test]
    fn tools_of_one_service_must_agree_on_lifecycle() {
        let mut m = conforming_manifest();
        let mut second = m["tools"][0].clone();
        second["name"] = json!("other");
        second["lifecycle"] = json!("per_session");
        m["tools"][0]["lifecycle"] = json!("per_call");
        m["tools"].as_array_mut().unwrap().push(second);
        let r = check_manifest(&m);
        assert!(
            r.errors.iter().any(|w| w.contains("不一致")),
            "{:?}",
            r.errors
        );
    }

    /// **`resident` 现在放行了**（2026-09-20 做出来的），但闸门**换了个地方管**：
    /// 它不再拦"声明了常驻"，改拦"**你说收你要调哪个工具，可我这儿没那个工具**" ——
    /// 因为中控收常驻服务时**调的就是那个名字**，指向空处的声明比不声明更糟。
    ///
    /// （这条测试是从它的反面改过来的：原来它断言"resident 被拦"。
    ///  决策变了，测试跟着变 —— 但**不能只是删掉它**，
    ///  那样"放行"就变成没人看的一行代码了。）
    #[test]
    fn resident_is_allowed_now_and_the_stop_entry_must_exist() {
        let mut m = conforming_manifest();
        m["tools"][0]["lifecycle"] = json!("resident");
        let ok = check_manifest(&m);
        assert!(
            !ok.errors.iter().any(|w| w.contains("resident")),
            "resident 该放行了：{:?}",
            ok.errors
        );

        // 但 `stop` 指向一个不存在的工具 ⇒ 拦
        let mut bad = conforming_manifest();
        bad["tools"][0]["stop"] = json!("shutdown");
        let r = check_manifest(&bad);
        assert!(
            r.errors.iter().any(|w| w.contains("shutdown")),
            "指向不存在的工具该拦：{:?}",
            r.errors
        );

        // 指向存在的工具 ⇒ 放行（`conforming_manifest` 只有这一个工具，
        // 那就让它指向自己 —— 闸门只管"这个名字在不在 manifest 里"）
        let mut good = conforming_manifest();
        let target = good["tools"][0]["name"].as_str().unwrap().to_string();
        good["tools"][0]["stop"] = json!(target);
        let r2 = check_manifest(&good);
        assert!(
            !r2.errors.iter().any(|w| w.contains("stop")),
            "指向存在的工具该放行：{:?}",
            r2.errors
        );
    }

    /// **沙箱五面：一个都不许少**（"说不清就是没说清"）。
    #[test]
    fn the_five_sandbox_faces_are_all_required() {
        let mut m = conforming_manifest();
        m["sandbox"].as_object_mut().unwrap().remove("network");
        let r = check_manifest(&m);
        assert!(
            r.errors.iter().any(|w| w.contains("network")),
            "少一面该点名：{:?}",
            r.errors
        );
    }

    /// **每一面一个闭集**，而且闭集里只放有实现支撑的值
    /// —— `filesystem: "os"`（内核级隔离）现在**没人做到**，所以它不在闭集里。
    #[test]
    fn sandbox_values_come_from_a_closed_set() {
        let mut m = conforming_manifest();
        m["sandbox"]["filesystem"] = json!("os");
        let r = check_manifest(&m);
        assert!(
            r.errors.iter().any(|w| w.contains("filesystem")),
            "闭集外的值该拦：{:?}",
            r.errors
        );
    }

    /// 没听说过的面也要拦（要么是打错了，要么得先加进规范）。
    #[test]
    fn an_unknown_sandbox_face_is_refused() {
        let mut m = conforming_manifest();
        m["sandbox"]["time_travel"] = json!("enabled");
        let r = check_manifest(&m);
        assert!(
            r.errors.iter().any(|w| w.contains("time_travel")),
            "{:?}",
            r.errors
        );
    }

    /// **"说了要围、却没围栏"是假的** —— 这条规则是从编辑器那儿抓出来的：
    /// 它的 `filesystem` 原来写死 `fence-args`，而没给围栏时那就成了假话。
    #[test]
    fn claiming_a_fence_without_one_is_refused() {
        let mut m = conforming_manifest();
        m["sandbox"]["filesystem"] = json!("fence-args");
        let r = check_manifest(&m);
        assert!(
            r.errors.iter().any(|w| w.contains("假") || w.contains("没围栏")),
            "{:?}",
            r.errors
        );
        // 报上围栏之后就合法了
        m["fence"] = json!(["C:\\proj"]);
        assert!(
            check_manifest(&m)
                .errors
                .iter()
                .all(|w| !w.contains("filesystem")),
            "报了围栏就该放行"
        );
    }

    /// `sandbox_note` 必报：**格子说的是"我盖了章"，那句话说的是"章盖在哪儿"**。
    #[test]
    fn the_sandbox_note_is_required_too() {
        let mut m = conforming_manifest();
        m.as_object_mut().unwrap().remove("sandbox_note");
        let r = check_manifest(&m);
        assert!(
            r.errors.iter().any(|w| w.contains("sandbox_note")),
            "{:?}",
            r.errors
        );
    }

    /// **契约版本对不上时不许硬解释**（2026-09-20 定下演进规则）。
    ///
    /// 这条要防的是那个经典死法：拿到一版**自己不认识**的 manifest，
    /// 却按旧版本去读 —— 于是报出来的错会指向"你少了个字段"，
    /// 而真正的原因（版本对不上）谁也看不见。
    #[test]
    fn an_unknown_contract_version_is_refused_by_name() {
        let mut m = conforming_manifest();
        m["contract_version"] = json!("2");
        let r = check_manifest(&m);
        assert!(
            r.errors.iter().any(|w| w.contains("contract_version")),
            "版本对不上该点名：{:?}",
            r.errors
        );
        // 判词里**两个版本号都要在**：只说"我不认识"而不说"我认识的是几"，
        // 对面还是不知道该往哪边改。
        let why = r.errors.join(" ");
        assert!(why.contains("2.0") && why.contains(CONTRACT_VERSION), "{why}");
        // 少写也一样要被拦（不然中控只能猜）
        let mut m2 = conforming_manifest();
        m2.as_object_mut().unwrap().remove("contract_version");
        assert!(
            check_manifest(&m2)
                .errors
                .iter()
                .any(|w| w.contains("contract_version")),
            "没写版本号也得拦"
        );
    }

    /// **等号换成了"看 major"**（2026-09-27，规范-消息协议 §9）。
    ///
    /// 三档一眼看完，因为它们要修的地方各不相同：
    ///
    /// | 它声明 | 结果 |
    /// |---|---|
    /// | `1.0` / `1`（同一个形状，两种写法）| 收下，**静悄悄** |
    /// | `1.9`（同 major、更新）| 收下，**要出声** |
    /// | `2.0` / `0.9`（major 对不上）| **按名字拒**，而且"太新/太老"分开说 |
    #[test]
    fn compatibility_is_decided_by_major_not_by_equality() {
        // `"1"` 与 `"1.0"` 是同一版 —— 六个服务 manifest 里写的就是前者，
        // 这条不成立的话，闸门会把这个仓库自己的服务全拒掉。
        for same in ["1", "1.0", "1.00"] {
            let mut m = conforming_manifest();
            m["contract_version"] = json!(same);
            let r = check_manifest(&m);
            assert!(r.is_ok(), "`{same}` 该被收下：{:?}", r.errors);
            assert!(
                !r.has_notes(),
                "`{same}` 和我是同一版，不该出声：{:?}",
                r.notes
            );
        }
        // 同 major、更新的 minor：**收下 + 出声**（这条原来会被等号拒掉）。
        let mut m = conforming_manifest();
        m["contract_version"] = json!("1.9");
        let r = check_manifest(&m);
        assert!(r.is_ok(), "同 major 不该拒：{:?}", r.errors);
        assert!(
            r.notes.iter().any(|n| n.contains("1.9")),
            "收下了就得说一声：{:?}",
            r.notes
        );
        // major 对不上：两档都拒，但**判词不是同一句**（要修的地方不一样）。
        let mut newer = conforming_manifest();
        newer["contract_version"] = json!("2.0");
        let mut older = conforming_manifest();
        older["contract_version"] = json!("0.9");
        let new_why = check_manifest(&newer).errors.join(" ");
        let old_why = check_manifest(&older).errors.join(" ");
        assert_ne!(new_why, old_why, "太新和太老不该共用一句话");
        assert!(new_why.contains("更新的协议"), "{new_why}");
        assert!(old_why.contains("老"), "{old_why}");
    }

    /// **版本号写坏了 ≠ 版本对不上。**
    ///
    /// 这两件事要分开说：前者是它自己打错了字（改一个字就好），
    /// 后者是我们在谈两个不同的形状（得动版本）。混成一句，
    /// 拿到那句话的人会照着错的方向改。
    #[test]
    fn a_typo_in_the_version_is_not_a_version_mismatch() {
        for typo in ["1.", "v1", "版本一", "1.0.2"] {
            let mut m = conforming_manifest();
            m["contract_version"] = json!(typo);
            let r = check_manifest(&m);
            let why = r.errors.join(" ");
            assert!(why.contains("不是一个版本号"), "`{typo}`：{why}");
            assert!(
                !why.contains("更新的协议"),
                "`{typo}` 是笔误，不该被说成「对面更新」：{why}"
            );
        }
    }

    #[test]
    fn result_schema_is_closed_and_carries_the_envelope() {
        let s = result_schema(&["instance_id"], json!({"instance_id": {"type": "string"}}));
        assert_eq!(s["additionalProperties"], json!(false));
        assert!(s["properties"].get("effect").is_some());
        assert!(s["properties"].get("remedy").is_some());
        assert!(s["required"]
            .as_array()
            .unwrap()
            .contains(&json!("instance_id")));
    }

    #[test]
    fn a_good_result_passes_its_own_schema() {
        let schema = result_schema(&["instance_id"], json!({"instance_id": {"type": "string"}}));
        let p = json!({"effect": "confirmed", "code": null, "route": null, "instance_id": "ed-1"});
        let v = check_result("editor:1", "open", &p, Some(&schema), &[]);
        assert!(v.is_empty(), "{v:?}");
    }

    #[test]
    fn a_tool_that_lies_about_its_shape_is_caught() {
        let schema = result_schema(&["instance_id"], json!({"instance_id": {"type": "string"}}));
        let p = json!({"effect": "confirmed", "code": null, "route": null});
        let v = check_result("editor:1", "open", &p, Some(&schema), &[]);
        assert!(v.iter().any(|s| s.contains("instance_id")), "{v:?}");
    }

    #[test]
    fn a_sixth_effect_is_not_a_state() {
        let p = json!({"effect": "ok", "code": null, "route": null});
        let v = check_result("x:1", "y", &p, Some(&result_schema(&[], json!({}))), &[]);
        assert!(v.iter().any(|s| s.contains("五态")), "{v:?}");
    }

    /// §11.2.2 收窄之后的那条：本地也可以 partial（超时被杀就是），
    /// 但"不知道到没到"的两态不许出现在没有投递环节的地方。
    #[test]
    fn route_null_allows_partial_but_not_unverifiable() {
        let schema = result_schema(&[], json!({}));
        let partial = json!({"effect": "partial", "code": "timeout", "route": null,
                             "why": "到点掐掉了", "remedy": "先看状态"});
        let v = check_result("term:1", "run", &partial, Some(&schema), &[]);
        assert!(v.is_empty(), "{v:?}");

        let unver = json!({"effect": "unverifiable", "code": "timeout", "route": null});
        let v = check_result("term:1", "run", &unver, Some(&schema), &[]);
        assert!(v.iter().any(|s| s.contains("没有投递环节")), "{v:?}");
    }

    #[test]
    fn refused_must_carry_the_way_out() {
        let schema = result_schema(&[], json!({}));
        let p = json!({"effect": "refused", "code": "refused", "route": null, "why": "不行"});
        let v = check_result("term:1", "run", &p, Some(&schema), &[]);
        assert!(v.iter().any(|s| s.contains("remedy")), "{v:?}");
    }

    #[test]
    fn an_unregistered_code_is_reported() {
        let schema = result_schema(&[], json!({}));
        let p = json!({"effect": "refused", "code": "oops", "route": null,
                       "why": "x", "remedy": "y"});
        let v = check_result("term:1", "run", &p, Some(&schema), &[]);
        assert!(v.iter().any(|s| s.contains("oops")), "{v:?}");
        let p2 = json!({"effect": "partial", "code": "canceled", "route": null,
                        "why": "被取消", "remedy": "看状态"});
        let v2 = check_result("term:1", "run", &p2, Some(&schema), &["canceled".into()]);
        assert!(v2.is_empty(), "{v2:?}");
    }

    #[test]
    fn a_missing_route_key_is_not_the_same_as_null() {
        let schema = result_schema(&[], json!({}));
        let p = json!({"effect": "confirmed", "code": null});
        let v = check_result("x:1", "y", &p, Some(&schema), &[]);
        assert!(v.iter().any(|s| s.contains("route")), "{v:?}");
    }

    #[test]
    fn closed_schema_refuses_stowaways() {
        let schema = result_schema(&[], json!({}));
        let p = json!({"effect": "confirmed", "code": null, "route": null, "surprise": 1});
        let v = check_result("x:1", "y", &p, Some(&schema), &[]);
        assert!(v.iter().any(|s| s.contains("surprise")), "{v:?}");
    }

    /// "成功时才必须有 `instance_id`" —— 条件式必填，
    /// 免得把"busy 时没有 instance_id"这种正常情况判成违规。
    #[test]
    fn required_when_effect_applies_only_to_that_effect() {
        let schema = result_schema_full(
            &[],
            json!({"instance_id": {"type": ["string", "null"]}}),
            &[(&["confirmed"], &["instance_id"])],
        );
        let ok = json!({"effect": "confirmed", "code": null, "route": null, "instance_id": "ed-1"});
        assert!(check_result("e:1", "open", &ok, Some(&schema), &[]).is_empty());

        let missing = json!({"effect": "confirmed", "code": null, "route": null});
        let v = check_result("e:1", "open", &missing, Some(&schema), &[]);
        assert!(v.iter().any(|s| s.contains("instance_id")), "{v:?}");

        let busy = json!({"effect": "refused", "code": "busy", "route": null,
                          "why": "写权在别人手里", "remedy": "等那个会话结束"});
        assert!(
            check_result("e:1", "open", &busy, Some(&schema), &[]).is_empty(),
            "busy 时没有 instance_id 是正常的，不该报"
        );
    }

    /// 构造器产出的条目**自带 C 组整组** —— 谁也别想漏一项。
    #[test]
    fn a_tool_decl_carries_the_whole_c_group() {
        let t = ToolDecl::new("open", "开一个", json!({"type": "object", "properties": {}}))
            .caps(&["fs.write"])
            .returns(json!({"instance_id": {"type": ["string", "null"]}}))
            .requires_on(&["confirmed"], &["instance_id"])
            .lifecycle("per_session")
            .to_json();
        assert_eq!(t["annotations"]["openWorldHint"], json!(false));
        assert_eq!(t["annotations"]["readOnlyHint"], json!(false));
        assert_eq!(t["idle_timeout"], Value::Null);
        assert_eq!(t["health"], json!("ping"));
        assert_eq!(t["output_schema"]["additionalProperties"], json!(false));

        let schema = t["output_schema"].clone();
        let miss = json!({"effect": "confirmed", "code": null, "route": null});
        let v = check_result("e:1", "open", &miss, Some(&schema), &[]);
        assert!(v.iter().any(|s| s.contains("instance_id")), "{v:?}");
    }
}
