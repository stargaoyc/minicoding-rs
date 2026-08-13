//! `UserRulesContributor`（顺序 6，`cacheable = false`）。
//!
//! 用户规则段：来自 `~/.minicoding/long_term.md`，跨会话的用户偏好与约束。
//! 内容包裹 `<user_rules>` 边界，声明性质为用户指令。

use minicoding_core::model::PromptError;
use minicoding_core::prompt::{
    PromptContext, PromptContributor, PromptSection, PromptSectionOrder,
};
use minicoding_core::provider::BoxFuture;

/// 用户规则段 contributor。
pub struct UserRulesContributor;

impl PromptContributor for UserRulesContributor {
    fn name(&self) -> &'static str {
        "user_rules"
    }

    fn order(&self) -> PromptSectionOrder {
        PromptSectionOrder::UserRules
    }

    fn cacheable(&self) -> bool {
        false
    }

    fn build(&self, ctx: &PromptContext) -> BoxFuture<'_, Result<PromptSection, PromptError>> {
        let content = ctx.user_rules.content.clone();
        Box::pin(async move {
            if content.trim().is_empty() {
                return Ok(PromptSection::empty(
                    "user_rules",
                    PromptSectionOrder::UserRules,
                ));
            }
            Ok(PromptSection::with_boundary(
                "user_rules",
                content,
                PromptSectionOrder::UserRules,
                false,
                "user_rules",
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::pedantic)]
    use super::*;
    use camino::Utf8PathBuf;
    use minicoding_core::model::SessionId;
    use minicoding_core::prompt::MemoryBlock;

    #[tokio::test]
    async fn empty_user_rules_returns_empty_section() {
        let c = UserRulesContributor;
        let ctx = PromptContext::new(SessionId::new(), Utf8PathBuf::from("/tmp"));
        let s = c.build(&ctx).await.expect("build");
        assert!(s.is_empty(), "expected empty: s");
    }

    #[tokio::test]
    async fn nonempty_user_rules_wraps_in_boundary() {
        let c = UserRulesContributor;
        let ctx = PromptContext::new(SessionId::new(), Utf8PathBuf::from("/tmp"))
            .with_user_rules(MemoryBlock::from_content("Always use Rust 2024 edition."));
        let s = c.build(&ctx).await.expect("build");
        assert!(!s.is_empty(), "expected non-empty: s");
        assert_eq!(s.boundary, Some("user_rules"));
        assert!(s.content.contains("Rust 2024"));
        assert!(!s.cacheable);
    }
}
