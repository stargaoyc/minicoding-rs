//! `ProjectRulesContributor`（顺序 7，`cacheable = false`）。
//!
//! 项目规则段：来自 AGENTS.md 分层加载结果。内容包裹 `<project_doc>` 边界，
//! 声明性质为项目约束（非 AI 自主可改）。

use minicoding_core::model::PromptError;
use minicoding_core::prompt::{
    PromptContext, PromptContributor, PromptSection, PromptSectionOrder,
};
use minicoding_core::provider::BoxFuture;

/// 项目规则段 contributor。
pub struct ProjectRulesContributor;

impl PromptContributor for ProjectRulesContributor {
    fn name(&self) -> &'static str {
        "project_rules"
    }

    fn order(&self) -> PromptSectionOrder {
        PromptSectionOrder::ProjectRules
    }

    fn cacheable(&self) -> bool {
        false
    }

    fn build(&self, ctx: &PromptContext) -> BoxFuture<'_, Result<PromptSection, PromptError>> {
        let content = ctx.project_rules.content.clone();
        Box::pin(async move {
            if content.trim().is_empty() {
                return Ok(PromptSection::empty(
                    "project_rules",
                    PromptSectionOrder::ProjectRules,
                ));
            }
            Ok(PromptSection::with_boundary(
                "project_rules",
                content,
                PromptSectionOrder::ProjectRules,
                false,
                "project_doc",
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
    use minicoding_core::prompt::ProjectDoc;

    #[tokio::test]
    async fn empty_project_rules_returns_empty_section() {
        let c = ProjectRulesContributor;
        let ctx = PromptContext::new(SessionId::new(), Utf8PathBuf::from("/tmp"));
        let s = c.build(&ctx).await.expect("build");
        assert!(s.is_empty());
    }

    #[tokio::test]
    async fn nonempty_project_rules_wraps_in_boundary() {
        let c = ProjectRulesContributor;
        let ctx = PromptContext::new(SessionId::new(), Utf8PathBuf::from("/repo"))
            .with_project_rules(ProjectDoc {
                content: "Use Rust 2024 edition. MSRV 1.99.".into(),
                layers: vec!["AGENTS.md".into()],
            });
        let s = c.build(&ctx).await.expect("build");
        assert!(!s.is_empty());
        assert_eq!(s.boundary, Some("project_doc"));
        assert!(s.content.contains("MSRV"));
        assert!(!s.cacheable);
    }
}
