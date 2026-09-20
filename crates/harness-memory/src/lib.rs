//! **线性记忆内核**（03-上下文 §5.4.3 / §5.4.4）。
//!
//! 它是"压缩"这件事的最小实现，形状整个照 codex 抄：
//!
//! | 抄哪条 | 在哪 |
//! |---|---|
//! | 每次压缩开一个新**窗口**，token 预算跟着重置 | [`window`] |
//! | 判断要不要再压，看的是**相对基线的增长**，不是累计总量 | [`window::Window`] |
//! | 摘要放在**模型被训练期望的位置** | [`compact::When`] |
//! | 摘要是**交接**，不是回忆 | [`summarize`] |
//! | 压缩是**一等事件**：pre/post 钩子 + 可审计的记录 | [`compact::CompactionRecord`] |
//! | **不试图保住缓存**，而是让压缩和缓存断裂对齐 | 本文件开头的态度 |
//!
//! 而 §5.4.4 那句"**反正都是炸缓存，不如把新东西放少点**"落在
//! [`policy::CompactionPolicy::aggressiveness`]：既然付了代价，就一次压到位。

pub mod compact;
pub mod counter;
pub mod history;
pub mod item;
pub mod policy;
pub mod summarize;
pub mod window;

pub use compact::{CompactionRecord, Compactor, Reason, When};
pub use counter::{HeuristicCounter, TokenCounter};
pub use history::History;
pub use item::{Item, ItemKind};
pub use policy::CompactionPolicy;
pub use summarize::{MockSummarizer, SummaryInput, Summarizer};
pub use window::Window;
