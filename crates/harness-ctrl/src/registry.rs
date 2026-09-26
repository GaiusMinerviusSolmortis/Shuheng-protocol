//! 实例注册表 —— 中控"知道世界上有哪些实例"的那一份（§12.7）。
//!
//! 图上的层次是 **会话 → 智能体 → 工具实例 → 工具服务**（§12.7）。
//! 阶段 1 只有最下面两层 + 一个 console，但注册表的结构按四层留好了。

use harness_core::envelope::Authority;
use std::collections::BTreeMap;

use crate::service::ServiceHandle;

/// 每个实例的**能力上限**。中控据此把 `authority` 只降不升（规范 §1.1）。
pub struct Instance {
    pub id: String,
    pub kind: String,
    /// 它是**工具**还是**智能体** —— 中控唯一在意这个区别的地方是权威上限（§1.1）
    pub is_agent: bool,
    pub ceiling: Authority,
    pub state: &'static str,
    pub service: Option<ServiceHandle>,
    /// 服务自报的那份 manifest 的**正文**。
    ///
    /// 以前中控只从里面解析 `owns` 一个字段，正文扔了（只落进日志）——
    /// 于是"工具自报家门"这条链只走了一半：报到了，没人接。
    /// 规范 §5.10 的原话是"中控据此知道该画成什么节点"，那就得存下来。
    pub manifest: Option<serde_json::Value>,
    /// **它自报的实例配置**（智能体才有：内核 / 模型 / 人格 / 步数）。
    ///
    /// 中控**只存不问** —— 它不认识"内核"是什么，也不去解释里面那几个字段
    /// （同 L1 那条：中控不该懂智能体的内部结构）。它存在这儿的理由只有一个：
    /// **"这个实例当初是怎么配的"要查得到**，而不是只活在部署表那串命令行里。
    pub config: Option<serde_json::Value>,
    /// 这个服务处在**第几代**（重新装配会 +1）。用来丢掉旧世代的迟到消息。
    pub generation: u64,
    /// **这个实例被授权用哪些实例、以及被授予了哪些能力**（§2.3 的雏形）。
    /// 只有智能体有；`None` 表示"不检查"（中控、通道、工具自己）。
    /// `Some(空表)` 表示"它是个智能体，但什么都不许用"——**默认拒绝**。
    pub bindings: Option<Vec<Binding>>,
    /// 它在注册时声明的**独占域**：`(domain, fences)`。
    /// 中控**不仲裁**它 —— 只拿它做审计（"两个实例声明了同一个独占域"），
    /// 因为挡住越界是服务自己的责任（02-工具 §5.15 第五节）。
    pub owns: Option<Owns>,
    /// **契约闸门的判定**：这份 manifest 有哪些不合规（§11.1）。
    ///
    /// 为什么中控要留着它：规范 §11.3 那句"字段不合规，中控就是接不上"——
    /// 但"接不上"落到实现里不能是一句含糊的 `service_down`。
    /// 存下来，`list` 就能说清**是哪个字段没报**（报错即手册）。
    pub contract_errors: Vec<String>,
    /// 规范要求了、但服务还没报的那几项（`lifecycle` / `idle_timeout` / `health`）。
    pub contract_missing: Vec<String>,
    /// **最后一次听见它说话**（它的管道上过来任何一条消息都算）。
    ///
    /// 两个用处：`list` 上给人看（"这个服务多久没动静了"），
    /// 以及探针的判据（判 `unresponsive` 之前先看它刚说过话没有）。
    pub last_seen_ms: i64,
    /// **它最近不应答探针**（≠ 死了）。
    ///
    /// 这个词要选准：探针超时只说明"它在这段时间里没应答"。
    /// 它可能在忙（终端正在跑一条长命令），也可能真卡住了 ——
    /// **中控不替它下结论，也不因此收掉它**（§5.20）。
    /// 这条只影响 `list` 上给你看的东西。
    pub unresponsive: bool,
    /// **部署决定的拉起时机**（来自服务表；中控按它做决定）。
    pub lifecycle: String,
    /// **服务自述的拉起时机**（来自它的 manifest；`None` = 还没报到）。
    ///
    /// 两个都要留着，因为**它们可以不同**：表是部署配置（运营者可以要求一个
    /// `per_call` 的服务开会话就先起来），manifest 是服务自己的说法。
    /// 不一致不是错，但**得看得见** —— `list` 上并排显示。
    pub lifecycle_declared: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Owns {
    pub domain: String,
    pub exclusive_per: String,
    pub fences: Vec<String>,
}

impl Owns {
    /// 从 manifest 里读出 `owns`。读不出来就当没声明。
    pub fn from_manifest(manifest: &serde_json::Value) -> Option<Self> {
        let owns = manifest.get("owns")?;
        let domain = owns.get("domain")?.as_str()?.to_string();
        let exclusive_per = owns
            .get("exclusive_per")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let fences = manifest
            .get("fence")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        Some(Owns {
            domain,
            exclusive_per,
            fences,
        })
    }

    pub fn overlaps(&self, other: &Owns) -> bool {
        self.domain == other.domain
            && self.exclusive_per == other.exclusive_per
            && !self.exclusive_per.is_empty()
            && self.fences.iter().any(|f| other.fences.contains(f))
    }
}

#[derive(Default)]
pub struct Registry {
    inner: BTreeMap<String, Instance>,
    /// **工具实例**（`t-1` / `ed-1` …）—— 图上的第三层。
    ///
    /// 键是 `<服务>/<服务内部的名字>`：**两个编辑器服务的第一个文档都会叫 `ed-1`**，
    /// 所以全局唯一性必须由"哪个服务"来保证。
    tool_instances: BTreeMap<String, ToolNode>,
}

#[derive(Debug, Clone)]
pub struct ToolNode {
    pub service: String,
    pub id: String,
    pub state: String,
    pub path: Option<String>,
    pub purpose: Option<String>,
    /// **这个实例归谁**（`<智能体实例>@<会话>` / `user@<通道>`）。
    /// 由服务在实例事件里上报（`InstanceEvent::owned_by`）。
    pub owner: Option<String>,
    pub since_ms: i64,
    /// **最后一次有人碰它**（实例事件，或者一条指名要它的调用）。
    ///
    /// ⚠️ 只给人看 —— §5.20 的原话："中控上的'最后活动时间'
    /// **只是给你看的，不是任何自动动作的触发**。"
    /// 它帮你发现"我是不是忘了一个"，而不是系统替你判断"这个没用了"。
    pub last_active_ms: i64,
    pub detail: serde_json::Value,
}

/// 一个工具的 `annotations` 三条 Hint（§11.1 C 组）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolHints {
    pub read_only: bool,
    pub destructive: bool,
    pub idempotent: bool,
}

/// 一条绑定：**哪个实例 + 被授予了哪些能力**。
///
/// 中控只负责**签发**（它知道"我把哪几个能力授给了谁"），
/// **校验在执行侧** —— 见 `harness_service::grant`。
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Binding {
    pub instance: String,
    pub capabilities: Vec<String>,
}

impl Binding {
    pub fn new(instance: impl Into<String>, capabilities: Vec<String>) -> Self {
        Self {
            instance: instance.into(),
            capabilities,
        }
    }
}

/// 工具实例在表里的键。
pub fn tool_key(service: &str, id: &str) -> String {
    format!("{service}/{id}")
}

impl Registry {
    /// 登记一个实例。
    ///
    /// **返回被顶掉的那个（如果有）。** 这条返回值不是装饰：
    /// `BTreeMap::insert` 的默认行为是**静默覆盖** —— 两个同名实例同时活着时，
    /// 后来者会把先来者从表里抹掉（连带它底下的工具实例和锁定关系），
    /// 而**发往那个 id 的消息会转手交给新进程**。
    /// 那是 id 层面的冒充，所以调用方必须看见这件事并拒绝它。
    pub fn insert(&mut self, instance: Instance) -> Option<Instance> {
        self.inner.insert(instance.id.clone(), instance)
    }

    pub fn get_mut(&mut self, id: &str) -> Option<&mut Instance> {
        self.inner.get_mut(id)
    }

    /// 只读地看一眼某个实例（tick 判"该不该探它"时用）。
    pub fn get_ref(&self, id: &str) -> Option<&Instance> {
        self.inner.get(id)
    }

    /// 某个实例自报的那份 manifest（给"注册闸门"和"投递闸门"用）。
    pub fn manifest_of(&self, id: &str) -> Option<serde_json::Value> {
        self.inner.get(id).and_then(|i| i.manifest.clone())
    }

    /// 它**自己报过**的错误码（§11.5：模型靠码判"重试有没有用"）。
    pub fn error_codes_of(&self, id: &str) -> Vec<String> {
        self.inner
            .get(id)
            .and_then(|i| i.manifest.as_ref())
            .and_then(|m| m.get("error_codes"))
            .and_then(serde_json::Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(serde_json::Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// 它的 manifest 有哪些不合规（注册闸门留下的判定）。
    pub fn contract_errors_of(&self, id: &str) -> Vec<String> {
        self.inner
            .get(id)
            .map(|i| i.contract_errors.clone())
            .unwrap_or_default()
    }

    /// **某个工具的三条 Hint**（§11.1 C 组的 `annotations`）。
    ///
    /// 为什么要读它：服务崩了 / 超时的时候，中控回给调用方的是
    /// "不知道它做没做"（`unverifiable`），而**能不能重试**取决于幂等性 ——
    /// 那份声明**本来就在中控手里**（它存着整份 manifest）。
    /// 原来那句补救写的是"只读或幂等的可以重试，有副作用的先确认" ——
    /// **把判断推回给了模型，而答案就在旁边。**
    ///
    /// 这就是 §19.2 第二条（MQTT 的 QoS 三档）的落地：**重试语义由声明决定，不由猜。**
    pub fn tool_hints(&self, service: &str, tool: &str) -> Option<ToolHints> {
        let m = self.inner.get(service)?.manifest.as_ref()?;
        let t = m
            .get("tools")?
            .as_array()?
            .iter()
            .find(|t| t.get("name").and_then(serde_json::Value::as_str) == Some(tool))?;
        let a = t.get("annotations")?;
        let b = |k: &str| {
            a.get(k)
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false)
        };
        Some(ToolHints {
            read_only: b("readOnlyHint"),
            destructive: b("destructiveHint"),
            idempotent: b("idempotentHint"),
        })
    }

    pub fn contains(&self, id: &str) -> bool {
        self.inner.contains_key(id)
    }

    /// 这个实例声明的权威级别的**上限**。不认识的实例一律按最低算。
    pub fn ceiling(&self, id: &str) -> Authority {
        self.inner
            .get(id)
            .map(|i| i.ceiling)
            .unwrap_or(Authority::Observe)
    }

    pub fn ids(&self) -> impl Iterator<Item = &str> {
        self.inner.keys().map(|s| s.as_str())
    }

    /// 现在活着的智能体实例。**敕令要发给它们** —— 它们是"有上下文"的那些（§4.4）。
    pub fn agent_ids(&self) -> Vec<String> {
        self.inner
            .values()
            .filter(|i| i.is_agent && i.state != "offline")
            .map(|i| i.id.clone())
            .collect()
    }

    pub fn remove(&mut self, id: &str) -> Option<Instance> {
        self.inner.remove(id)
    }

    /// 实例事件说"有了 / 变了"，就 upsert 一个节点。
    pub fn upsert_tool(
        &mut self,
        service: &str,
        e: &harness_core::instance_event::InstanceEvent,
        now_ms: i64,
    ) {
        let key = tool_key(service, &e.instance);
        let node = self.tool_instances.entry(key).or_insert_with(|| ToolNode {
            service: service.to_string(),
            id: e.instance.clone(),
            state: e.state.clone(),
            path: e.path.clone(),
            purpose: e.purpose.clone(),
            owner: e.owner.clone(),
            since_ms: now_ms,
            last_active_ms: now_ms,
            detail: e.detail.clone(),
        });
        node.state = e.state.clone();
        // 状态变了就是"有人碰过它"
        node.last_active_ms = now_ms;
        if e.owner.is_some() {
            node.owner = e.owner.clone();
        }
        if e.path.is_some() {
            node.path = e.path.clone();
        }
        if e.purpose.is_some() {
            node.purpose = e.purpose.clone();
        }
        if e.detail != serde_json::json!({}) {
            node.detail = e.detail.clone();
        }
    }

    pub fn remove_tool(&mut self, service: &str, id: &str) -> bool {
        self.tool_instances.remove(&tool_key(service, id)).is_some()
    }

    /// **实例归谁** —— "别动别人的实例"那条统一检查要的就是它。
    ///
    /// 为什么这份信息在中控、而不在各个服务里：**它是一张图上的事实**
    /// （谁开着什么）。服务只知道自己那一个域，中控才看得全 ——
    /// 所以"智能体之间互不越界"这条只能在这儿**统一执行一次**，
    /// 而不是指望每个第三方服务都自己实现一遍（终端的 `read` 原来就没有）。
    pub fn tool_owner(&self, service: &str, instance: &str) -> Option<String> {
        self.tool_instances
            .get(&tool_key(service, instance))
            .and_then(|n| n.owner.clone())
    }

    /// 服务没了 ⇒ **它的实例跟着没**（图上那几条连线是长在服务上的）。
    pub fn remove_tools_of(&mut self, service: &str) -> Vec<String> {
        let keys: Vec<String> = self
            .tool_instances
            .iter()
            .filter(|(_, n)| n.service == service)
            .map(|(k, _)| k.clone())
            .collect();
        for k in &keys {
            self.tool_instances.remove(k);
        }
        keys
    }

    pub fn tool_count(&self) -> usize {
        self.tool_instances.len()
    }

    /// **有人指名用了一个实例** ⇒ 记下"最后活动时间"。
    ///
    /// 调用点在路由里：请求的 `args.instance_id` 指向谁，就是碰了谁。
    /// 它**不触发任何动作**（§5.20），只是让 `list` 上那个数保持新鲜。
    pub fn touch_tool(&mut self, service: &str, instance: &str, now_ms: i64) {
        if let Some(n) = self.tool_instances.get_mut(&tool_key(service, instance)) {
            n.last_active_ms = now_ms;
        }
    }

    /// 服务说话 ⇒ 记下"最后听见它的时间"，顺手清掉"不应答"的标记。
    pub fn note_seen(&mut self, id: &str, now_ms: i64) {
        if let Some(i) = self.inner.get_mut(id) {
            i.last_seen_ms = now_ms;
            i.unresponsive = false;
        }
    }

    /// 标记"它不应答了"。**返回是否发生了变化**（只在翻转时说一次，不刷屏）。
    pub fn mark_unresponsive(&mut self, id: &str) -> bool {
        match self.inner.get_mut(id) {
            Some(i) if !i.unresponsive => {
                i.unresponsive = true;
                true
            }
            _ => false,
        }
    }

    /// 某个实例的授权清单。`None` = 它不是智能体，不需要检查。
    pub fn bindings_of(&self, id: &str) -> Option<Vec<Binding>> {
        self.inner.get(id).and_then(|i| i.bindings.clone())
    }

    /// **改签一条绑定**（§2.3）：把 `binding.instance` 那条设成新的能力清单。
    ///
    /// 为什么中控要留着这个口子：绑定是**中控签发**的 —— 而"签发"这个词
    /// 真正的含义是**签发之后还能改签**。在这之前绑定只是服务表里的一行静态配置，
    /// 于是敕令那整套机制（§4.4：名册只追加、回合边界扫一遍）
    /// **永远等不到触发源** —— 链是接好的，就是没人去按开关。
    ///
    /// 返回改完之后的整张清单；`None` = 那个 id 不是智能体（没有清单可改）。
    pub fn grant(&mut self, id: &str, binding: Binding) -> Option<Vec<Binding>> {
        let list = self.inner.get_mut(id)?.bindings.as_mut()?;
        match list.iter_mut().find(|b| b.instance == binding.instance) {
            // 重复授权是**改签**，不是追加一条 —— 否则同一个实例会有两张互相矛盾的清单
            Some(slot) => *slot = binding,
            None => list.push(binding),
        }
        Some(list.clone())
    }

    /// 收回一条绑定。**已经发出去的调用不受影响** —— 那是运行期的事实，
    /// 不是授权能追回来的东西（所以敕令里要说清这一点）。
    pub fn revoke(&mut self, id: &str, instance: &str) -> Option<Vec<Binding>> {
        let list = self.inner.get_mut(id)?.bindings.as_mut()?;
        list.retain(|b| b.instance != instance);
        Some(list.clone())
    }

    pub fn generation_of(&self, id: &str) -> Option<u64> {
        self.inner.get(id).map(|i| i.generation)
    }

    /// 某个实例**管着哪条路径上**（从它自报的 manifest 里读的，已经规范化过）。
    /// 空表 = 它没声明围栏（那就没有范围可判）。
    pub fn fence_of(&self, id: &str) -> Vec<String> {
        self.inner
            .get(id)
            .and_then(|i| i.owns.as_ref())
            .map(|o| o.fences.clone())
            .unwrap_or_default()
    }

    /// 按 id 取几份 manifest（给"我能用什么"那条路用）。
    pub fn manifests_of(&self, ids: &[String]) -> Vec<serde_json::Value> {
        ids.iter()
            .filter_map(|id| {
                let i = self.inner.get(id)?;
                Some(serde_json::json!({
                    "instance": i.id,
                    "manifest": i.manifest.clone().unwrap_or(serde_json::json!(null)),
                }))
            })
            .collect()
    }

    /// 除了 `me` 之外，还有谁声明了与 `mine` 重叠的独占域。
    /// **只查不改** —— 中控在这里的角色是审计，不是仲裁。
    pub fn others_owning<'a>(
        &'a self,
        me: &'a str,
        mine: &'a Owns,
    ) -> impl Iterator<Item = String> + 'a {
        self.inner
            .values()
            .filter(move |i| i.id != me && i.state != "offline")
            .filter_map(move |i| {
                let o = i.owns.as_ref()?;
                if o.overlaps(mine) {
                    Some(format!("{}（{}）", i.id, i.state))
                } else {
                    None
                }
            })
    }

    /// 画在图上要的东西（阶段 5 的 GUI 直接吃这个）。
    pub fn snapshot(&self) -> serde_json::Value {
        let nodes: Vec<serde_json::Value> = self
            .inner
            .values()
            .map(|i| {
                serde_json::json!({
                    "id": i.id,
                    "kind": i.kind,
                    "is_agent": i.is_agent,
                    "state": i.state,
                    "ceiling": i.ceiling.as_str(),
                    "cmd": i.service.as_ref().map(|s| s.cmd.join(" ")),
                    "has_manifest": i.manifest.is_some(),
                    // **它自报的实例配置**（智能体才有：内核 / 模型 / 人格 / 步数）。
                    // 中控不解释它，只把它摆出来 —— 图上看得到"这个智能体是拿什么配的"。
                    "config": i.config,
                    "contract_errors": i.contract_errors,
                    "contract_missing": i.contract_missing,
                    // **给人看的两样**：多久没听见它了 / 它最近不应答探针
                    "last_seen_ms": i.last_seen_ms,
                    "idle_ms": if i.last_seen_ms > 0 {
                        harness_core::now_ms().saturating_sub(i.last_seen_ms)
                    } else {
                        -1
                    },
                    "unresponsive": i.unresponsive,
                    // **两个 lifecycle 并排**：表决定的（部署）和它自述的
                    "lifecycle": i.lifecycle,
                    "lifecycle_declared": i.lifecycle_declared,
                    "lifecycle_mismatch": i
                        .lifecycle_declared
                        .as_ref()
                        .map(|d| d != &i.lifecycle)
                        .unwrap_or(false),
                    "bindings": i.bindings,
                    "owns": i.owns.as_ref().map(|o| serde_json::json!({
                        "domain": o.domain,
                        "exclusive_per": o.exclusive_per,
                        "fences": o.fences
                    }))
                })
            })
            .collect();
        // **节点与连线**：实例节点带 `owner`，那就是图上那条边（§12.7）
        let tools: Vec<serde_json::Value> = self
            .tool_instances
            .values()
            .map(|n| {
                serde_json::json!({
                    "id": n.id,
                    "key": tool_key(&n.service, &n.id),
                    // **挂在哪个服务上** —— 原来这个字段叫 `owner`，
                    // 而 `owner` 现在有更重要的意思：**谁开的这个实例**（归属）。
                    // 两个意思挤在一个名字上，读图的人分不清"这是谁的工具"和"这是谁的实例"，
                    // 所以服务那层改叫 `service`（2026-09-19 对齐时改的）。
                    "service": n.service,
                    "state": n.state,
                    "path": n.path,
                    "purpose": n.purpose,
                    "owner": n.owner,
                    "since_ms": n.since_ms,
                    "last_active_ms": n.last_active_ms,
                    // 闲置多久 —— **只是个数字**，不是任何动作的触发（§5.20）
                    "idle_ms": harness_core::now_ms().saturating_sub(n.last_active_ms),
                    "detail": n.detail,
                })
            })
            .collect();
        let edges: Vec<serde_json::Value> = tools
            .iter()
            .map(|t| {
                serde_json::json!({
                    "from": t["service"], "to": t["key"], "kind": "owns"
                })
            })
            .collect();
        serde_json::json!({
            "instances": nodes,
            "tool_instances": tools,
            "edges": edges
        })
    }

    pub fn services_mut(&mut self) -> impl Iterator<Item = &mut ServiceHandle> {
        self.inner
            .values_mut()
            .filter_map(|i| i.service.as_mut())
    }
}
