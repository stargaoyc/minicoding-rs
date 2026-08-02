//! `web.fetch`：获取 URL 内容，HTML→Markdown（T-M8-5）。
//!
//! SSRF 防护（[`super::ssrf::validate_url`]）→ reqwest GET → `htmd` 转 Markdown。
//! `SideEffect::Network`（需经权限审批）。输出截断到 `ctx.max_output_bytes`。

use super::ssrf::validate_url;
use minicoding_core::model::{SideEffect, ToolError, ToolResult, ToolSchema};
use minicoding_core::provider::BoxFuture;
use minicoding_core::tool::Tool;

/// `web.fetch` 工具。
pub struct WebFetch {
    schema: ToolSchema,
}

impl WebFetch {
    #[must_use]
    pub fn new() -> Self {
        let schema = ToolSchema {
            name: "web.fetch".into(),
            description: "获取 URL 内容并转为 Markdown。SSRF 防护：拒绝私有/loopback IP。".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "url": {
                        "type": "string",
                        "description": "要获取的 http/https URL"
                    }
                },
                "required": ["url"]
            }),
        };
        Self { schema }
    }
}

impl Default for WebFetch {
    fn default() -> Self {
        Self::new()
    }
}

impl Tool for WebFetch {
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
        ctx: &minicoding_core::tool::ToolContext,
    ) -> BoxFuture<'_, Result<ToolResult, ToolError>> {
        let max_bytes = ctx.max_output_bytes;
        Box::pin(async move {
            let url: String = params
                .get("url")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ToolError::InvalidInput("url 缺失".into()))?
                .to_string();

            // 1. SSRF 防护
            validate_url(&url).await?;

            // 2. HTTP GET
            let client = reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::limited(5))
                .build()
                .map_err(|e| ToolError::Exec(format!("HTTP client 构建失败: {e}")))?;

            let resp = client
                .get(&url)
                .header("User-Agent", "minicoding/0.1")
                .send()
                .await
                .map_err(|e| ToolError::Exec(format!("HTTP 请求失败: {e}")))?;

            if !resp.status().is_success() {
                return Err(ToolError::Exec(format!("HTTP {}", resp.status())));
            }

            let content_type = resp
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string();

            let body = resp
                .text()
                .await
                .map_err(|e| ToolError::Exec(format!("读取响应体失败: {e}")))?;

            // 3. HTML → Markdown（仅 HTML 内容；其他类型直接返回文本）
            let markdown = if content_type.contains("text/html") {
                htmd::convert(&body).unwrap_or(body)
            } else {
                body
            };

            // 4. 截断
            let truncated = if markdown.len() > max_bytes {
                markdown[..max_bytes].to_string()
            } else {
                markdown
            };

            Ok(ToolResult::ok_text(truncated))
        })
    }
}
