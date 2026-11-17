# 同类型研究资料索引

本目录记录“枢恒协议”选题调研时找到的六篇直接相关论文。原始 PDF、提取文本和完整中文译稿保存在本地，不随公开仓库分发；本文件只保存来源、核验信息和研究定位。

## 交付物位置

- 原文 PDF 与 `pdftotext -layout` 提取文本：`研究资料/论文原文/`
- 中文研读译稿：`研究资料/论文译稿/`，同时提供 Markdown 与排版后的 `.docx`
- 六篇译稿均按论文顺序覆盖摘要、正文、主要图表信息和结论，协议字段、代码标识符和参考文献保持原文。

## 论文清单

| 编号 | 论文 | 版本与来源 | PDF SHA-256 | 许可 | 本地译稿 |
|---|---|---|---|---|---|
| 01 | **AIOS: LLM Agent Operating System**，Kai Mei et al. | arXiv:2403.16971v5；<https://arxiv.org/abs/2403.16971> | `6ac427d318869e82ea5dc2f1e4b1db91a482c640d527cc3f3812eb2d3f835b12` | arXiv 非独占分发许可 | `论文译稿/01-AIOS-中文研读译稿.md` |
| 02 | **Agentao: A Policy-Governed Runtime Harness for Embeddable Tool-Using LLM Agents**，Bo Jin et al. | arXiv:2608.13574v2；<https://arxiv.org/abs/2608.13574> | `278f2d003aa4fb578bf6b230a6143dbe2be73f2c3405da89c2961dc36690682c` | arXiv 非独占分发许可 | `论文译稿/02-Agentao-中文研读译稿.md` |
| 03 | **MPAC: A Multi-Principal Agent Coordination Protocol for Interoperable Multi-Agent Collaboration**，Kaiyang Qian et al. | arXiv:2604.09744v1；<https://arxiv.org/abs/2604.09744> | `cdcf71e7d6638c4e00ff483dced96dd9e711ec3867f38f972872953516c54bf8` | CC BY 4.0 | `论文译稿/03-MPAC-中文研读译稿.md` |
| 04 | **The Log is the Agent: Event-Sourced Reactive Graphs for Auditable, Forkable Agentic Systems**，Yohei Nakajima | arXiv:2605.21997v1；<https://arxiv.org/abs/2605.21997>；代码 Apache-2.0 | `8e8ba7df94f27f0ab9fa105f038dc9cd4ceaa6679657fc456d81dbbb689a0266` | 论文使用 arXiv 非独占分发许可 | `论文译稿/04-ActiveGraph-中文研读译稿.md` |
| 05 | **InterSAGE: The Secure and Verifiable Interoperability Protocol for An Internet of Agents**，Zhenhua Zou et al. | arXiv:2608.13030v2；<https://arxiv.org/abs/2608.13030> | `5bc4792c1c1476bed8cab71f017862bf7af34fd19ba7eae30521e1f7e782b52d` | arXiv 非独占分发许可 | `论文译稿/05-InterSAGE-中文研读译稿.md` |
| 06 | **Integrating Agentic Web Standards in Multi-Agent Systems Using Hypermedia**，Jérémy Lemée | EMAS 2026；<https://github.com/JeremyLemee/agentic-web-hmas> | `0430975d29f0a346b2db725d33d6f631ac6b267a36260ce4e658ce30b762aedf` | 未在论文正文确认开放再分发许可 | `论文译稿/06-Agentic-Web-Hypermedia-中文研读译稿.md` |

## 与枢恒协议的关系

### 完全重合判断

六篇论文都在某些能力上与枢恒相交，但没有一篇同时完成以下整体：

1. 会话、智能体、工具实例、工具服务四类对象的统一标识与生命周期；
2. 任意对象可位于本地或远端软硬件主机；
3. 中控同时承担发现、路由、授权链、保活、拉起、故障恢复和效果对账；
4. 既有平台、插件和异构协议作为对象接入，而不是将所有能力重写进一个单机内核；
5. 对不可逆外部副作用保留意图、尝试、效果和未知结果的执行账本。

因此不能称“完全没人做过”，但可以说“没有找到在一套协议里完全重合的工作”。论文写作时，应逐项引用这些先行工作，不能把会话、日志、权限或工具接入本身当作首创。

### 最近的先行工作

- **AIOS** 最接近中控的单机内核形态，已经覆盖调度、上下文、记忆、存储、工具和权限管理。差别是多主机对象网络与完整生命周期。
- **Agentao** 最接近“治理型运行时”，已经覆盖宿主契约、模型提议与授权执行分离、MCP、插件、技能、子智能体、结构化事件、重放与会话恢复。差别是 local-first 与跨主机对象网络。
- **MPAC** 最接近会话层与多主体治理，已经覆盖意图前置、结构化冲突、因果水位、状态机、人类仲裁和协调者恢复。差别是不托管异构对象，也不保证物理效果。
- **ActiveGraph** 最接近执行账本、确定性重放、反事实分叉与全链路血缘。它明确承认外部工具首次副作用不会被重放撤销，且未解决分布式并发写入。
- **InterSAGE** 最接近统一身份、能力发现、委托衰减、会话令牌与不可否认审计。差别是它提供信任平面，不提供对象运行与效果恢复。
- **Hypermedia Agentic Web** 最接近“任意服务/设备按描述接入统一工具接口”，已经统一 MCP、A2A、UTCP、WoT TD 并驱动机械臂。差别是单请求 Web 适配层，不规定进程托管、跨主机运行和故障对账。

## 可辩护的研究边界

如果论文以设计稿为主，最清楚的贡献组合是：

1. **对象网络模型：** 把四类对象统一为可发现、可路由、可观察、可治理的节点，而不是仅统一工具描述。
2. **混合粒度与统一控制：** 允许多个对象共处一个进程，也允许它们跨主机部署，并在本地走快路由、远端走网络路由。
3. **中控生命周期协议：** 明确定义保活、启动、停止、迁移、重新宿主、会话接管和故障归因。
4. **不可逆副作用语义：** 区分“调用成功”“效果已观测”“结果未知”和“结果不可验证”，而不是把事件重放等同于真实效果恢复。
5. **跨协议组合：** 向下适配 DSH/MCP/WoT 等，向上保持会话与智能体协议中立。

## 许可与引用

本轮核验结果：

- MPAC 的 arXiv 页面标示 CC BY 4.0。
- AIOS、Agentao、ActiveGraph、InterSAGE 的 arXiv 页面标示 arXiv 非独占分发许可。
- ActiveGraph 的实现仓库是 Apache-2.0，但这不自动代表论文正文采用 Apache-2.0。
- Hypermedia 论文未在正文确认 CC 许可；EMAS/出版社版本可能另有版权约束。

因此，本轮只在本地制作个人研究用完整译稿。公开仓库不包含论文 PDF、全文提取文本或全文译稿。若后续需要公开，应逐篇确认许可并与作者或出版方核实翻译再分发权。
