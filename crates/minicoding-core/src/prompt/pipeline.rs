//! `PromptPipeline`：9 个 contributor 按固定顺序拼装 system prompt。
//!
//! `build` 流程：
//! 1. 遍历所有 contributor 调 `build`，收集 `PromptSection`；
//! 2. 按 `(order as u8, contributor_name)` 排序（同 order 内按 name 稳定排序）；
//! 3. 跳过 `is_empty()` 的 section；
//! 4. 带 boundary 的段包裹 `<{boundary}>\n{content}\n</{boundary}>`，无 boundary 直接拼接；
//! 5. 段间用空行分隔；
//! 6. 返回 [`SystemPrompt`]（含拼接后文本与分段信息，供 `OTel` span 记录）。

use crate::model::PromptError;
use crate::prompt::context::PromptContext;
use crate::prompt::trait_def::{PromptContributor, PromptSection, PromptSectionOrder};
use std::sync::Arc;

/// 拼装后的 system prompt（含分段信息，便于诊断）。
#[derive(Debug, Clone)]
pub struct SystemPrompt {
    /// 拼接后的完整文本（传给 LLM）。
    pub text: String,
    /// 各段信息（按拼接顺序，已过滤空段）。
    pub sections: Vec<PromptSection>,
}

impl SystemPrompt {
    /// 总 token 数估算（字符数 / 4，粗略估算；真实 token 数由 provider tokenizer 算）。
    #[must_use]
    pub fn approx_chars(&self) -> usize {
        self.text.chars().count()
    }

    /// 各段 token 数占比（OTel span 用）。
    #[must_use]
    pub fn section_lengths(&self) -> Vec<(String, PromptSectionOrder, usize)> {
        self.sections
            .iter()
            .map(|s| {
                (
                    s.contributor_name.clone(),
                    s.order,
                    s.content.chars().count(),
                )
            })
            .collect()
    }

    /// 是否包含给定 order 的段（测试/诊断用）。
    #[must_use]
    pub fn has_section(&self, order: PromptSectionOrder) -> bool {
        self.sections.iter().any(|s| s.order == order)
    }
}

/// Prompt 管道（持有有序 contributor 列表，按 `PromptSectionOrder` 拼装）。
#[derive(Default)]
pub struct PromptPipeline {
    contributors: Vec<Arc<dyn PromptContributor>>,
}

impl std::fmt::Debug for PromptPipeline {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PromptPipeline")
            .field("contributors_count", &self.contributors.len())
            .finish()
    }
}

impl PromptPipeline {
    /// 创建空管道。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 从 contributor 列表构造（注册顺序不影响拼装顺序，拼装顺序由 `order()` 决定）。
    #[must_use]
    pub fn with_contributors(contributors: Vec<Arc<dyn PromptContributor>>) -> Self {
        Self { contributors }
    }

    /// 追加一个 contributor。
    pub fn register(&mut self, contributor: Arc<dyn PromptContributor>) {
        self.contributors.push(contributor);
    }

    /// 已注册 contributor 数（诊断/`doctor` 用）。
    #[must_use]
    pub fn len(&self) -> usize {
        self.contributors.len()
    }

    /// 是否为空。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.contributors.is_empty()
    }

    /// 拼装 system prompt。
    ///
    /// # Errors
    /// 任一 contributor `build` 失败时短路返回 `PromptError`。
    pub async fn build(&self, ctx: &PromptContext) -> Result<SystemPrompt, PromptError> {
        // 1. 并发/顺序调用各 contributor build（contributor 一般是 IO-free 或缓存命中，
        //    顺序调用即可，避免并发带来的复杂度）。
        let mut sections: Vec<PromptSection> = Vec::with_capacity(self.contributors.len());
        for c in &self.contributors {
            let s = c.build(ctx).await?;
            if !s.is_empty() {
                sections.push(s);
            }
        }

        // 2. 按 (order as u8, contributor_name) 排序：保证稳定段在前、同段内按 name 稳定。
        sections.sort_by_key(|s| (s.order as u8, s.contributor_name.clone()));

        // 3. 拼接：带 boundary 的段包裹标签，段间空行分隔。
        let mut buf = String::new();
        for s in &sections {
            if let Some(b) = s.boundary {
                use std::fmt::Write as _;
                let _ = write!(buf, "<{b}>\n{}\n</{b}>\n\n", s.content);
            } else {
                buf.push_str(&s.content);
                buf.push_str("\n\n");
            }
        }

        Ok(SystemPrompt {
            text: buf,
            sections,
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::pedantic)]
    use super::*;
    use crate::model::SessionId;
    use crate::prompt::context::PromptContext;
    use crate::prompt::trait_def::{PromptContributor, PromptSection, PromptSectionOrder};
    use crate::provider::BoxFuture;
    use camino::Utf8PathBuf;
    use std::sync::Arc;

    /// 静态 contributor：返回固定 section（测试用）。
    struct StaticContributor {
        name: &'static str,
        order: PromptSectionOrder,
        content: &'static str,
        boundary: Option<&'static str>,
        cacheable: bool,
    }

    impl PromptContributor for StaticContributor {
        fn name(&self) -> &str {
            self.name
        }
        fn order(&self) -> PromptSectionOrder {
            self.order
        }
        fn cacheable(&self) -> bool {
            self.cacheable
        }
        fn build(
            &self,
            _ctx: &PromptContext,
        ) -> BoxFuture<'_, Result<PromptSection, crate::model::PromptError>> {
            let name = self.name;
            let content = self.content;
            let order = self.order;
            let cacheable = self.cacheable;
            Box::pin(async move {
                Ok(PromptSection {
                    contributor_name: name.into(),
                    content: content.into(),
                    order,
                    cacheable,
                    boundary: self.boundary,
                })
            })
        }
    }

    fn ctx() -> PromptContext {
        PromptContext::new(SessionId::new(), Utf8PathBuf::from("/tmp"))
    }

    #[tokio::test]
    async fn empty_pipeline_returns_empty_prompt() {
        let p = PromptPipeline::new();
        let sp = p.build(&ctx()).await.expect("build");
        assert!(sp.text.is_empty(), "expected empty: sp.text");
        assert!(sp.sections.is_empty(), "expected empty: sp.sections");
    }

    #[tokio::test]
    async fn single_contributor_plains() {
        let mut p = PromptPipeline::new();
        p.register(Arc::new(StaticContributor {
            name: "identity",
            order: PromptSectionOrder::Identity,
            content: "You are minicoding.",
            boundary: None,
            cacheable: true,
        }));
        let sp = p.build(&ctx()).await.expect("build");
        assert_eq!(sp.text, "You are minicoding.\n\n");
        assert_eq!(sp.sections.len(), 1);
        assert!(sp.has_section(PromptSectionOrder::Identity));
    }

    #[tokio::test]
    async fn ordered_concatenation_independent_of_register_order() {
        // 注册顺序反着：先 Extension 后 Identity，拼装仍应 Identity 在前。
        let mut p = PromptPipeline::new();
        p.register(Arc::new(StaticContributor {
            name: "ext",
            order: PromptSectionOrder::Extension,
            content: "ext-content",
            boundary: Some("hook_context"),
            cacheable: false,
        }));
        p.register(Arc::new(StaticContributor {
            name: "identity",
            order: PromptSectionOrder::Identity,
            content: "identity-content",
            boundary: None,
            cacheable: true,
        }));
        let sp = p.build(&ctx()).await.expect("build");
        // Identity 应在前
        let id_idx = sp
            .sections
            .iter()
            .position(|s| s.order == PromptSectionOrder::Identity)
            .expect("identity section exists");
        let ext_idx = sp
            .sections
            .iter()
            .position(|s| s.order == PromptSectionOrder::Extension)
            .expect("extension section exists");
        assert!(id_idx < ext_idx);
        assert!(sp.text.starts_with("identity-content"));
        assert!(
            sp.text
                .contains("<hook_context>\next-content\n</hook_context>")
        );
    }

    #[tokio::test]
    async fn empty_section_skipped() {
        let mut p = PromptPipeline::new();
        p.register(Arc::new(StaticContributor {
            name: "empty",
            order: PromptSectionOrder::System,
            content: "",
            boundary: None,
            cacheable: true,
        }));
        p.register(Arc::new(StaticContributor {
            name: "identity",
            order: PromptSectionOrder::Identity,
            content: "I am X.",
            boundary: None,
            cacheable: true,
        }));
        let sp = p.build(&ctx()).await.expect("build");
        assert_eq!(sp.sections.len(), 1);
        assert_eq!(sp.sections[0].contributor_name, "identity");
    }

    #[tokio::test]
    async fn same_order_sorts_by_name() {
        // 两个 Extension 段，按 name 排序：aaa 在 zzz 前
        let mut p = PromptPipeline::new();
        p.register(Arc::new(StaticContributor {
            name: "zzz",
            order: PromptSectionOrder::Extension,
            content: "Z",
            boundary: None,
            cacheable: false,
        }));
        p.register(Arc::new(StaticContributor {
            name: "aaa",
            order: PromptSectionOrder::Extension,
            content: "A",
            boundary: None,
            cacheable: false,
        }));
        let sp = p.build(&ctx()).await.expect("build");
        assert_eq!(sp.sections[0].contributor_name, "aaa");
        assert_eq!(sp.sections[1].contributor_name, "zzz");
        assert_eq!(sp.text, "A\n\nZ\n\n");
    }
}
