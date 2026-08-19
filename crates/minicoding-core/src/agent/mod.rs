//! 子 Agent 模块（`SubagentRunner` trait + 兜底实现，见 `design.md` §7）。
//!
//! `task.spawn` 工具持有 `Arc<dyn SubagentRunner>` 反向调用 Runtime 派发子 Agent
//!（与 `plan.exit` 持有 `Arc<dyn PlanModeController>` 同构，避免 core 依赖 tools）。
//! Runtime 默认注入 `NoopSubagentRunner`（兜底，返回 `NotConfigured` 错误）；
//! 真实场景由 frontend 注入 `InProcessSubagentRunner` 或外部实现。
//!
//! M-05：`WorktreeSubagentRunner`（git worktree 命令胶水实现）已下沉到
//! `minicoding-tools`，core 仅保留 `SubagentRunner` trait 抽象。

mod runner;

pub use runner::{NoopSubagentRunner, SubagentRunner};
