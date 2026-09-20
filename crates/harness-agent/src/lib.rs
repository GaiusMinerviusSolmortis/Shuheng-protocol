//! 智能体实例。
//!
//! 它接的三样东西（§3 的实例清单里，智能体是"一个可被唤醒的执行主体"）：
//!
//! | 接谁 | 干什么 |
//! |---|---|
//! | **模型** | 拼请求、拿回复（第一版是脚本化 mock）|
//! | **中控** | 把工具调用发出去、等结果回来 |
//! | **记忆内核** | 每轮前问"该压了吗"，每轮后用真值校准基线 |
//!
//! 它**自己不做**的判断：路由（中控的）、执行（工具的）、
//! 以及"这个工具有没有权限"（**绑定**：中控签发 `granted`，服务自己比对 ——
//! 2026-09-19 就落地了，见 `smoke-help-grant` / `smoke-regrant`）。
//!
//! #### 提示词是编译一次、然后冻住的
//!
//! §15.2 那条硬约束：**拼装必须在绑定时刻凝固**。所以工具集变了
//! **不改提示词**，而是追加一条**敕令** —— 前缀不动，变化走 append-only。
//! 这就是我们讨论了好几轮的那件事，现在它有了实现。

pub mod deepseek;
pub mod model;
pub mod prompt;
pub mod script;

pub use model::{MockModel, Model, ModelReply, ModelRequest, Step, ToolCall};
pub use prompt::{compile, CompositionManifest, Persona, ToolCard};
pub use script::{parse_script, ScriptError};
