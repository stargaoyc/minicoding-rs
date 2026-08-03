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

#[cfg(test)]
mod tests {
    //! `builtin_contributors` 工厂测试（覆盖率补全）。

    use super::*;
    use minicoding_core::prompt::PromptSectionOrder;

    #[test]
    fn builtin_contributors_returns_nine_contributors() {
        let list = builtin_contributors("");
        assert_eq!(list.len(), 9, "expected exactly 9 builtin contributors");
    }

    #[test]
    fn builtin_contributors_names_match_expected() {
        let list = builtin_contributors("");
        let names: Vec<&str> = list.iter().map(|c| c.name()).collect();
        assert_eq!(
            names,
            vec![
                "identity",
                "system",
                "task_guidelines",
                "communication",
                "environment",
                "user_rules",
                "project_rules",
                "tool_summary",
                "extension_builtin",
            ]
        );
    }

    #[test]
    fn builtin_contributors_orders_are_monotonic() {
        let list = builtin_contributors("");
        let orders: Vec<PromptSectionOrder> = list.iter().map(|c| c.order()).collect();
        // 顺序应为 Identity(1) → System(2) → ... → Extension(9)，单调递增
        for i in 1..orders.len() {
            let prev = orders[i - 1] as u8;
            let curr = orders[i] as u8;
            assert!(
                prev < curr,
                "orders not monotonic at index {i}: {prev} -> {curr}"
            );
        }
        // 验证覆盖全部 9 段
        assert_eq!(orders[0], PromptSectionOrder::Identity);
        assert_eq!(orders[1], PromptSectionOrder::System);
        assert_eq!(orders[2], PromptSectionOrder::TaskGuidelines);
        assert_eq!(orders[3], PromptSectionOrder::Communication);
        assert_eq!(orders[4], PromptSectionOrder::Environment);
        assert_eq!(orders[5], PromptSectionOrder::UserRules);
        assert_eq!(orders[6], PromptSectionOrder::ProjectRules);
        assert_eq!(orders[7], PromptSectionOrder::ToolSummary);
        assert_eq!(orders[8], PromptSectionOrder::Extension);
    }

    #[test]
    fn builtin_contributors_with_empty_identity_works() {
        let list = builtin_contributors("");
        // 验证不 panic + 长度正确即可
        assert_eq!(list.len(), 9);
    }

    #[test]
    fn builtin_contributors_with_custom_identity_works() {
        let list = builtin_contributors("You are a Rust expert.");
        assert_eq!(list.len(), 9);
        // 第一个应为 IdentityContributor
        assert_eq!(list[0].name(), "identity");
        assert_eq!(list[0].order(), PromptSectionOrder::Identity);
    }

    #[test]
    fn builtin_contributors_stable_first_five_are_cacheable() {
        // 稳定段（1-5）应可缓存（cacheable = true），易变段（6-9）不应可缓存
        let list = builtin_contributors("");
        for (i, c) in list.iter().enumerate() {
            let cacheable = c.cacheable();
            if i < 5 {
                assert!(
                    cacheable,
                    "stable section {} ({}) should be cacheable",
                    i + 1,
                    c.name()
                );
            } else {
                assert!(
                    !cacheable,
                    "volatile section {} ({}) should not be cacheable",
                    i + 1,
                    c.name()
                );
            }
        }
    }
}
