//! `web.search`：搜索引擎查询（T-M8-5）。
//!
//! 使用 `DuckDuckGo` HTML 端点（无需 API key），返回前 N 条结果（title + snippet + url）。
//! `SideEffect::Network`（需经权限审批）。SSRF 防护目标 host 已白名单（duckduckgo.com）。

use minicoding_core::model::{SideEffect, ToolError, ToolResult, ToolSchema};
use minicoding_core::provider::BoxFuture;
use minicoding_core::tool::Tool;

/// `DuckDuckGo` HTML 搜索端点（无需 API key）。
const DDG_HTML_ENDPOINT: &str = "https://html.duckduckgo.com/html/";

/// `web.search` 工具。
pub struct WebSearch {
    schema: ToolSchema,
}

impl WebSearch {
    #[must_use]
    pub fn new() -> Self {
        let schema = ToolSchema {
            name: "web.search".into(),
            description: "搜索引擎查询（DuckDuckGo）。返回 title/snippet/url 列表。".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "搜索关键词"
                    },
                    "max_results": {
                        "type": "integer",
                        "description": "返回结果上限（默认 5）",
                        "default": 5
                    }
                },
                "required": ["query"]
            }),
        };
        Self { schema }
    }
}

impl Default for WebSearch {
    fn default() -> Self {
        Self::new()
    }
}

impl Tool for WebSearch {
    fn name(&self) -> &str {
        &self.schema.name
    }
    fn schema(&self) -> &ToolSchema {
        &self.schema
    }
    fn side_effect(&self) -> SideEffect {
        SideEffect::Network
    }

    fn execute(
        &self,
        params: serde_json::Value,
        _ctx: &minicoding_core::tool::ToolContext,
    ) -> BoxFuture<'_, Result<ToolResult, ToolError>> {
        Box::pin(async move {
            let query: String = params
                .get("query")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ToolError::InvalidInput("query 缺失".into()))?
                .to_string();
            // max_results 来自 LLM 输入，截断为 usize（32-bit 平台极端值不影响实际用途）
            #[allow(clippy::cast_possible_truncation)]
            let max_results: usize = params
                .get("max_results")
                .and_then(serde_json::Value::as_u64)
                .map_or(5, |n| n as usize);

            // 1. HTTP POST（DDG HTML 端点用 POST 表单）
            let client = reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::limited(3))
                .build()
                .map_err(|e| ToolError::Exec(format!("HTTP client 构建失败: {e}")))?;

            let resp = client
                .post(DDG_HTML_ENDPOINT)
                .form(&[("q", query.as_str())])
                .header("User-Agent", "minicoding/0.1")
                .send()
                .await
                .map_err(|e| ToolError::Exec(format!("搜索请求失败: {e}")))?;

            if !resp.status().is_success() {
                return Err(ToolError::Exec(format!("HTTP {}", resp.status())));
            }

            let html = resp
                .text()
                .await
                .map_err(|e| ToolError::Exec(format!("读取响应体失败: {e}")))?;

            // 2. 解析结果（DDG HTML 结果页的 result__a / result__snippet class）
            let results = parse_ddg_results(&html, max_results);

            // 3. 格式化为可读文本
            let mut text = String::new();
            for (i, r) in results.iter().enumerate() {
                use std::fmt::Write;
                let _ = write!(
                    text,
                    "{}. {}\n   {}\n   {}\n\n",
                    i + 1,
                    r.title,
                    r.url,
                    r.snippet
                );
            }
            if text.is_empty() {
                text.push_str("无搜索结果");
            }

            Ok(ToolResult::ok_text(text))
        })
    }
}

/// 单条搜索结果。
struct SearchResult {
    title: String,
    url: String,
    snippet: String,
}

/// 从 `DuckDuckGo` HTML 结果页解析搜索结果。
///
/// 简易解析：提取 `class="result__a"` 的 `<a>` 标签（title + href）和
/// `class="result__snippet"` 的文本。不引入 HTML parser 依赖（用正则，够用）。
fn parse_ddg_results(html: &str, max: usize) -> Vec<SearchResult> {
    let title_re = regex::Regex::new(r#"<a[^>]*class="result__a"[^>]*>(.*?)</a>"#).ok();
    let href_re = regex::Regex::new(r#"href="([^"]+)""#).ok();
    let snippet_re = regex::Regex::new(r#"<a[^>]*class="result__snippet"[^>]*>(.*?)</a>"#).ok();

    let titles: Vec<&str> = title_re
        .as_ref()
        .map(|re| re.find_iter(html).map(|m| m.as_str()).collect())
        .unwrap_or_default();

    let snippets: Vec<&str> = snippet_re
        .as_ref()
        .map(|re| re.find_iter(html).map(|m| m.as_str()).collect())
        .unwrap_or_default();

    titles
        .iter()
        .zip(snippets.iter().chain(std::iter::repeat(&"")))
        .take(max)
        .map(|(t, s)| {
            let title = strip_tags(t);
            let url = href_re
                .as_ref()
                .and_then(|re| re.captures(t))
                .and_then(|c| c.get(1).map(|m| m.as_str().to_string()))
                .unwrap_or_default();
            let snippet = strip_tags(s);
            SearchResult {
                title,
                url,
                snippet,
            }
        })
        .collect()
}

/// 移除 HTML 标签，保留纯文本。
fn strip_tags(html: &str) -> String {
    let re = regex::Regex::new(r"<[^>]+>").ok();
    let text = re.as_ref().map_or_else(
        || html.to_string(),
        |re| re.replace_all(html, "").to_string(),
    );
    text.trim().replace("&amp;", "&").replace("&lt;", "<")
}

#[cfg(test)]
mod tests {
    use super::*;
    use minicoding_core::tool::ToolContext;

    fn make_ctx() -> ToolContext {
        ToolContext::new("/tmp/proj".into(), "test".to_string())
    }

    #[tokio::test]
    async fn search_missing_query_returns_invalid_input() {
        let tool = WebSearch::new();
        let result = tool.execute(serde_json::json!({}), &make_ctx()).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput(_)));
    }

    #[test]
    fn search_side_effect_is_network() {
        let tool = WebSearch::new();
        assert_eq!(tool.side_effect(), SideEffect::Network);
    }

    #[test]
    fn search_schema_has_correct_name() {
        let tool = WebSearch::new();
        assert_eq!(tool.name(), "web.search");
    }

    #[test]
    fn parse_ddg_results_extracts_title_url_snippet() {
        let html = r#"
        <div class="result">
            <a class="result__a" href="https://example.com/page1">Example Title</a>
            <a class="result__snippet">Some snippet text</a>
        </div>
        "#;
        let results = parse_ddg_results(html, 5);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "Example Title");
        assert_eq!(results[0].url, "https://example.com/page1");
        assert_eq!(results[0].snippet, "Some snippet text");
    }

    #[test]
    fn parse_ddg_results_respects_max_limit() {
        let html = r#"
        <a class="result__a" href="https://a.com">A</a>
        <a class="result__snippet">sa</a>
        <a class="result__a" href="https://b.com">B</a>
        <a class="result__snippet">sb</a>
        <a class="result__a" href="https://c.com">C</a>
        <a class="result__snippet">sc</a>
        "#;
        let results = parse_ddg_results(html, 2);
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn parse_ddg_results_empty_html_returns_empty() {
        let results = parse_ddg_results("", 5);
        assert!(results.is_empty());
    }

    #[test]
    fn parse_ddg_results_missing_snippet_uses_empty() {
        let html = r#"<a class="result__a" href="https://x.com">X</a>"#;
        let results = parse_ddg_results(html, 5);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].snippet, "");
    }

    #[test]
    fn strip_tags_removes_html_tags() {
        assert_eq!(strip_tags("<b>hello</b>"), "hello");
        assert_eq!(strip_tags("<a href='x'>link</a>"), "link");
    }

    #[test]
    fn strip_tags_unescapes_entities() {
        assert_eq!(strip_tags("a &amp; b"), "a & b");
        assert_eq!(strip_tags("x &lt; y"), "x < y");
    }

    #[test]
    fn strip_tags_no_tags_returns_trimmed() {
        assert_eq!(strip_tags("  plain text  "), "plain text");
    }
}
