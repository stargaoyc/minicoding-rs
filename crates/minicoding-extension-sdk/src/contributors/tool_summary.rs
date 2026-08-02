//! `ToolSummaryContributor`（顺序 8，`cacheable = false`）。
//!
//! 工具摘要段：列出已启用的工具 schema（含 MCP 工具），让 LLM 知道可用工具集。
//! 工具列表随会话配置变化（不同 profile 启用不同工具），故 `cacheable = false`。

use minicoding_core::model::PromptError;
use minicoding_core::prompt::{
    PromptContext, PromptContributor, PromptSection, PromptSectionOrder,
};
use minicoding_core::provider::BoxFuture;

/// 工具摘要段 contributor。
pub struct ToolSummaryContributor;

impl PromptContributor for ToolSummaryContributor {
    fn name(&self) -> &'static str {
        "tool_summary"
    }

    fn order(&self) -> PromptSectionOrder {
        PromptSectionOrder::ToolSummary
    }

    fn cacheable(&self) -> bool {
        false
    }

    fn build(&self, ctx: &PromptContext) -> BoxFuture<'_, Result<PromptSection, PromptError>> {
        let is_empty = ctx.enabled_tools.is_empty();
        let content = format_tools(&ctx.enabled_tools);
        Box::pin(async move {
            if is_empty {
                return Ok(PromptSection::empty(
                    "tool_summary",
                    PromptSectionOrder::ToolSummary,
                ));
            }
            Ok(PromptSection::plain(
                "tool_summary",
                content,
                PromptSectionOrder::ToolSummary,
                false,
            ))
        })
    }
}

fn format_tools(tools: &[minicoding_core::model::ToolSchema]) -> String {
    use std::fmt::Write as _;
    let mut buf = String::new();
    buf.push_str("## 可用工具\n\n");
    for tool in tools {
        let _ = writeln!(buf, "- `{}`: {}", tool.name, tool.description);
    }
    buf
}

#[cfg(test)]
mod tests {
    #![allow(clippy::pedantic)]
    use super::*;
    use camino::Utf8PathBuf;
    use minicoding_core::model::{SessionId, ToolSchema};

    #[tokio::test]
    async fn empty_tools_returns_empty_section() {
        let c = ToolSummaryContributor;
        let ctx = PromptContext::new(SessionId::new(), Utf8PathBuf::from("/tmp"));
        let s = c.build(&ctx).await.expect("build");
        assert!(s.is_empty());
    }

    #[tokio::test]
    async fn tools_listed_in_summary() {
        let c = ToolSummaryContributor;
        let ctx = PromptContext::new(SessionId::new(), Utf8PathBuf::from("/tmp")).with_tools(vec![
            ToolSchema {
                name: "fs.read".into(),
                description: "Read file".into(),
                input_schema: serde_json::Value::Null,
            },
            ToolSchema {
                name: "shell.run".into(),
                description: "Run command".into(),
                input_schema: serde_json::Value::Null,
            },
        ]);
        let s = c.build(&ctx).await.expect("build");
        assert!(s.content.contains("fs.read"));
        assert!(s.content.contains("shell.run"));
        assert!(!s.cacheable);
    }
}
