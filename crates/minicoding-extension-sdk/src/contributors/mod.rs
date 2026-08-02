//! 9 个内置 `PromptContributor` 实现（见 `design.md` §22）。
//!
//! 稳定段（1-5，`cacheable = true`）：Identity / System / `TaskGuidelines` /
//! Communication / Environment。
//! 易变段（6-9，`cacheable = false`）：`UserRules` / `ProjectRules` / `ToolSummary` / Extension。
//!
//! Runtime 启动时构造这 9 个 contributor 注册到 `PromptPipeline`。扩展通过 `Registrar`
//! 注册的 contributor 注入到 `Extension` 段（顺序 9），与内置 `ExtensionContributor`
//! 共存（同 order 内按 name 排序）。

pub mod communication;
pub mod environment;
pub mod extension_contrib;
pub mod identity;
pub mod project_rules;
pub mod system;
pub mod task_guidelines;
pub mod tool_summary;
pub mod user_rules;

pub use communication::CommunicationContributor;
pub use environment::EnvironmentContributor;
pub use extension_contrib::ExtensionContributor;
pub use identity::IdentityContributor;
pub use project_rules::ProjectRulesContributor;
pub use system::SystemContributor;
pub use task_guidelines::TaskGuidelinesContributor;
pub use tool_summary::ToolSummaryContributor;
pub use user_rules::UserRulesContributor;

use minicoding_core::prompt::PromptContributor;
use std::sync::Arc;

/// 构造默认 9 个内置 contributor 列表（注册到 `PromptPipeline`）。
///
/// `identity_content` 为 `IDENTITY.md` 内容（空则用默认身份）。
#[must_use]
pub fn builtin_contributors(identity_content: &str) -> Vec<Arc<dyn PromptContributor>> {
    vec![
        Arc::new(IdentityContributor::new(identity_content)),
        Arc::new(SystemContributor),
        Arc::new(TaskGuidelinesContributor),
        Arc::new(CommunicationContributor),
        Arc::new(EnvironmentContributor),
        Arc::new(UserRulesContributor),
        Arc::new(ProjectRulesContributor),
        Arc::new(ToolSummaryContributor),
        Arc::new(ExtensionContributor),
    ]
}
