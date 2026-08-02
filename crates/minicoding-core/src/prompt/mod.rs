//! Prompt 管道（9 个 `PromptContributor` 按固定顺序拼接，见 `design.md` §22、
//! `api.md` §3.13）。
//!
//! 设计目标：
//! - **可组合**：每个来源是一个 `PromptContributor`，独立实现；
//! - **可扩展**：第三方扩展可注册新 contributor（通过 `Registrar`，注入到 `Extension` 段）；
//! - **prompt cache 友好**：稳定段（1-5）排前，易变段（6-9）排后。
//!
//! 9 段顺序（`PromptSectionOrder` 枚举）：`Identity` / `System` / `TaskGuidelines` /
//! `Communication` / `Environment` / `UserRules` / `ProjectRules` / `ToolSummary` / `Extension`。
//! 同 order 内多个 contributor 按 `contributor_name` 稳定排序（避免非确定性）。
//!
//! `PromptPipeline::build` 把所有 contributor 的输出聚合为单个 system prompt 字符串，
//! 同时保留分段信息供 `OTel` span 记录每段 token 数。

pub mod context;
pub mod pipeline;
pub mod trait_def;

pub use context::{GitInfo, MemoryBlock, Platform, ProjectDoc, PromptContext};
pub use pipeline::{PromptPipeline, SystemPrompt};
pub use trait_def::{PromptContributor, PromptSection, PromptSectionOrder};
