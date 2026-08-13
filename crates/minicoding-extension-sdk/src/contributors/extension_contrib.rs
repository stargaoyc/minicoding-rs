//! `ExtensionContributor`（顺序 9，`cacheable = false`）。
//!
//! 扩展段占位 contributor：本身不产生内容（返回空 section），扩展通过 `Registrar`
//! 注册的 contributor 会以同 order（9）注入到 `PromptPipeline`，与本 contributor
//! 共存（同 order 内按 name 排序）。
//!
//! 设计意图：给扩展段一个"锚点"，即使没有扩展注册 contributor，pipeline 也知道
//! Extension 段的存在（诊断用）。实际扩展内容由扩展注册的 contributor 提供。

use minicoding_core::model::PromptError;
use minicoding_core::prompt::{
    PromptContext, PromptContributor, PromptSection, PromptSectionOrder,
};
use minicoding_core::provider::BoxFuture;

/// 扩展段占位 contributor。
pub struct ExtensionContributor;

impl PromptContributor for ExtensionContributor {
    fn name(&self) -> &'static str {
        "extension_builtin"
    }

    fn order(&self) -> PromptSectionOrder {
        PromptSectionOrder::Extension
    }

    fn cacheable(&self) -> bool {
        false
    }

    fn build(&self, _ctx: &PromptContext) -> BoxFuture<'_, Result<PromptSection, PromptError>> {
        Box::pin(async move {
            // 占位：返回空 section，pipeline 自动跳过。
            // 扩展注册的 contributor 会以同 order 注入，提供实际内容。
            Ok(PromptSection::empty(
                "extension_builtin",
                PromptSectionOrder::Extension,
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

    #[tokio::test]
    async fn extension_builtin_returns_empty() {
        let c = ExtensionContributor;
        let s = c
            .build(&PromptContext::new(
                SessionId::new(),
                Utf8PathBuf::from("/tmp"),
            ))
            .await
            .expect("build");
        assert!(s.is_empty(), "expected empty: s");
        assert!(!s.cacheable);
    }
}
