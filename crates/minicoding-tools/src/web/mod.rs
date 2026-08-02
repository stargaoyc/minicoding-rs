//! Web 工具组（`web.fetch`/`web.search`，T-M8-5，feature gate `web`）。
//!
//! - `web.fetch`：获取 URL 内容，HTML→Markdown 转换（`SideEffect::Network`）；
//! - `web.search`：搜索引擎查询（DuckDuckGo HTML 端点，无需 API key）。
//!
//! ## SSRF 防护
//!
//! 所有 URL 经 [`ssrf::validate_url`] 校验：拒绝非 http/https scheme、拒绝
//! loopback/private/link-local/unspecified IP（含 DNS 解析后检查，防止域名绕过）。
//! 见 `security.md` §3.2。

#![cfg(feature = "web")]

mod fetch;
mod search;
mod ssrf;

pub use fetch::WebFetch;
pub use search::WebSearch;

use minicoding_core::tool::ToolRegistry;

/// 注册全部 web 工具到 `registry`（需 `web` feature）。
pub fn register_web_tools(registry: &mut ToolRegistry) {
    registry.register(std::sync::Arc::new(WebFetch::new()));
    registry.register(std::sync::Arc::new(WebSearch::new()));
}
