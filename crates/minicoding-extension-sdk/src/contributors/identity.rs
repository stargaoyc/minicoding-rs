//! `IdentityContributor`（顺序 1，`cacheable = true`）。
//!
//! 身份段：声明 minicoding 是什么、能做什么。默认身份硬编码，可通过
//! `~/.minicoding/IDENTITY.md` 覆盖（P-31）。
//!
//! **P-31 IDENTITY.md 覆盖**：Runtime 启动时读取 `~/.minicoding/IDENTITY.md`，
//! 非空则作为 `identity_content` 传入 `IdentityContributor::new`，替代默认身份。
//! 这让用户可以自定义 AI 助手的身份设定（如 "你是专注于 Rust 的专家"）。

use minicoding_core::model::PromptError;
use minicoding_core::prompt::{
    PromptContext, PromptContributor, PromptSection, PromptSectionOrder,
};
use minicoding_core::provider::BoxFuture;

/// 默认身份文本（当 `IDENTITY.md` 不存在或为空时使用）。
const DEFAULT_IDENTITY: &str = "You are minicoding, a terminal-based AI coding assistant.\n\
You help users with software engineering tasks: reading, writing, and editing code, \n\
running commands, and debugging issues.\n\n\
You operate in an Agent loop: you receive user input, think, call tools, observe results, \n\
and continue until the task is done or you need user input.\n\
You are precise, concise, and prioritize correctness over verbosity.";

/// 身份段 contributor。
pub struct IdentityContributor {
    /// 身份文本（`IDENTITY.md` 内容或默认）。
    content: String,
}

impl IdentityContributor {
    /// 创建 contributor，传入 `IDENTITY.md` 内容（空则用默认身份）。
    #[must_use]
    pub fn new(identity_content: &str) -> Self {
        let content = if identity_content.trim().is_empty() {
            DEFAULT_IDENTITY.to_string()
        } else {
            identity_content.trim().to_string()
        };
        Self { content }
    }

    /// 使用默认身份创建。
    #[must_use]
    pub fn default_identity() -> Self {
        Self {
            content: DEFAULT_IDENTITY.to_string(),
        }
    }
}

impl PromptContributor for IdentityContributor {
    fn name(&self) -> &'static str {
        "identity"
    }

    fn order(&self) -> PromptSectionOrder {
        PromptSectionOrder::Identity
    }

    fn cacheable(&self) -> bool {
        true
    }

    fn build(&self, _ctx: &PromptContext) -> BoxFuture<'_, Result<PromptSection, PromptError>> {
        let content = self.content.clone();
        Box::pin(async move {
            Ok(PromptSection::plain(
                "identity",
                content,
                PromptSectionOrder::Identity,
                true,
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

    fn ctx() -> PromptContext {
        PromptContext::new(SessionId::new(), Utf8PathBuf::from("/tmp"))
    }

    #[tokio::test]
    async fn default_identity_when_empty() {
        let c = IdentityContributor::new("");
        let s = c.build(&ctx()).await.expect("build");
        assert!(s.content.contains("minicoding"));
        assert!(s.cacheable);
    }

    #[tokio::test]
    async fn custom_identity_overrides_default() {
        let c = IdentityContributor::new("You are a Rust expert.");
        let s = c.build(&ctx()).await.expect("build");
        assert_eq!(s.content, "You are a Rust expert.");
    }

    #[tokio::test]
    async fn whitespace_only_falls_back_to_default() {
        let c = IdentityContributor::new("   \n\n  ");
        let s = c.build(&ctx()).await.expect("build");
        assert!(s.content.contains("minicoding"));
    }
}
