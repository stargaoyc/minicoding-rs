//! `web.fetch`：获取 URL 内容，HTML→Markdown（T-M8-5）。
//!
//! SSRF 防护（[`super::ssrf::validate_url`]）→ reqwest GET → `htmd` 转 Markdown。
//! `SideEffect::Network`（需经权限审批）。输出截断到 `ctx.max_output_bytes`。

use super::ssrf::validate_url;
use minicoding_core::model::{SideEffect, ToolError, ToolResult, ToolSchema};
use minicoding_core::provider::BoxFuture;
use minicoding_core::tool::{RenderIntent, Tool};

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
        let timeout = ctx.timeout;
        Box::pin(async move {
            let url: String = params
                .get("url")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ToolError::InvalidInput("url 缺失".into()))?
                .to_string();

            // 1. SSRF 防护
            validate_url(&url).await?;

            // 2. HTTP GET + 转换 + 截断
            // T-3（2026-08-25 审查）：整个抓取链路（重定向循环 + 读体 + 转换）
            // 纳入 ctx.timeout 窗口——慢速/挂起的服务端此前可无限占用 turn。
            match tokio::time::timeout(timeout, fetch_and_convert(&url, max_bytes)).await {
                Ok(result) => result,
                Err(elapsed) => Ok(ToolResult::err_text(format!(
                    "web.fetch 执行超时（超过 {elapsed:?}）"
                ))),
            }
        })
    }

    /// 渲染意图（R-05，M-11）：抓取内容（Markdown 文本）默认文本直出。
    fn render_output(&self, result: &ToolResult) -> RenderIntent {
        RenderIntent::default_for(result)
    }
}

/// 响应体读取上限（T-6，2026-08-25 审查）：超过即停止读取并标注截断。
/// 此前 `resp.text()` 无界缓冲，异常/恶意大响应可直接 OOM。
const MAX_BODY_BYTES: usize = 10 * 1024 * 1024;

/// 流式读取响应体并限制最大字节数（T-6，2026-08-25 审查）。
///
/// 用 `bytes_stream` 逐 chunk 累积，达到 [`MAX_BODY_BYTES`] 即停止消费
/// （丢弃剩余连接），返回 `(body, 是否被截断)`；截断标记与下游
/// `truncate_output`（`ctx.max_output_bytes`）的输出级截断衔接。
pub(crate) async fn read_body_capped(resp: reqwest::Response) -> Result<(String, bool), ToolError> {
    read_body_capped_with_limit(resp, MAX_BODY_BYTES).await
}

/// 同 [`read_body_capped`] 但上限由调用方给定（`web.search` 复用，PTM-5）。
pub(crate) async fn read_body_capped_with_limit(
    resp: reqwest::Response,
    max_bytes: usize,
) -> Result<(String, bool), ToolError> {
    use futures::StreamExt;
    let mut stream = resp.bytes_stream();
    let mut body: Vec<u8> = Vec::new();
    let mut capped = false;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| ToolError::Exec(format!("读取响应体失败: {e}")))?;
        let remain = max_bytes.saturating_sub(body.len());
        if remain == 0 {
            capped = true;
            break;
        }
        // 只追加到上限为止（半截 chunk 也截断），随后停止读取
        let take = remain.min(chunk.len());
        body.extend_from_slice(&chunk[..take]);
        if take < chunk.len() {
            capped = true;
            break;
        }
    }
    Ok((String::from_utf8_lossy(&body).into_owned(), capped))
}

/// 执行 HTTP GET 并按 content-type 转换/截断（SSRF 校验后的核心逻辑）。
///
/// 抽取为自由函数便于单测：测试用 wiremock 起本地 server（127.0.0.1），
/// 但 SSRF 会拒绝 loopback，故测试直接调用本函数绕过 SSRF（SSRF 由
/// `validate_url` 单独覆盖，见 `ssrf.rs` 测试）。
async fn fetch_and_convert(url: &str, max_bytes: usize) -> Result<ToolResult, ToolError> {
    // S22：禁用自动重定向，手动逐跳跟随——每一跳都重过 SSRF 校验（防"公网入口
    // 302 → 内网/元数据地址"绕过与 DNS rebinding）。首跳校验在此处显式执行
    // （测试态下与原行为一致地绕过，见循环内注释）。
    const MAX_REDIRECTS: usize = 5;
    let mut hops = 0usize;
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| ToolError::Exec(format!("HTTP client 构建失败: {e}")))?;

    let mut current = url.to_string();
    let resp = loop {
        // 测试态跳过逐跳复检：wiremock 起在本机 loopback，SSRF 会拒绝（首跳
        // 校验由调用方 fetch_and_convert 承担；此处生产路径每跳强制复检）
        if !cfg!(test) {
            super::ssrf::validate_url(&current).await?;
        }
        let resp = client
            .get(&current)
            .header("User-Agent", "minicoding/0.1")
            .send()
            .await
            .map_err(|e| ToolError::Exec(format!("HTTP 请求失败: {e}")))?;
        if resp.status().is_redirection() {
            hops += 1;
            if hops > MAX_REDIRECTS {
                return Err(ToolError::Exec(format!(
                    "重定向超过 {MAX_REDIRECTS} 跳上限"
                )));
            }
            let Some(next) = resp
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
            else {
                return Err(ToolError::Exec("重定向缺少 Location 头".into()));
            };
            current = join_redirect_url(&current, next)?;
            if current.len() > 2048 {
                return Err(ToolError::Exec("重定向 URL 过长".into()));
            }
            continue;
        }
        break resp;
    };

    if !resp.status().is_success() {
        return Err(ToolError::Exec(format!("HTTP {}", resp.status())));
    }

    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    let (body, body_capped) = read_body_capped(resp).await?;

    // HTML → Markdown（仅 HTML 内容；其他类型直接返回文本）
    let markdown = if content_type.contains("text/html") {
        match htmd::convert(&body) {
            Ok(md) => md,
            Err(e) => {
                tracing::warn!(error = %e, "htmd convert failed, returning raw body");
                body
            }
        }
    } else {
        body
    };

    // 截断：经 `truncate_output` 在 UTF-8 字符边界上截断——直接字节切片
    // `&markdown[..max_bytes]` 会在多字节字符中间 panic（远程内容可控，
    // 中文长页面必现；2026-08-23 审查 §5-P0/§6-P0）
    let (truncated, was_truncated) = crate::util::truncate_output(markdown, max_bytes);

    let mut result = ToolResult::ok_text(truncated);
    // T-6：响应体级截断与输出级截断统一标注 metadata.truncated
    result.metadata.truncated = was_truncated || body_capped;
    Ok(result)
}

/// S22：解析相对 Location 为绝对 URL（同 scheme/host，路径合并）。
///
/// PTM-12（2026-08-25 R2 审查）：相对 Location 按 **RFC 3986 路径合并**解析
/// ——此前一律拼到 origin 根，`https://a.com/b/c` 上的 `Location: d/e` 会错误
/// 跳到 `https://a.com/d/e`（应为 `/b/d/e`）。以 `..`/`.` 开头的段同样按
/// 规则消解；`//host/path` 形态按协议相对处理保留 authority。
fn join_redirect_url(base: &str, location: &str) -> Result<String, ToolError> {
    if location.starts_with("http://") || location.starts_with("https://") {
        return Ok(location.to_string());
    }
    // 相对路径：取 base 的 scheme+authority 前缀拼接
    let Some(scheme_end) = base.find("://") else {
        return Err(ToolError::Exec("无效的 base URL".into()));
    };
    let rest = &base[scheme_end + 3..];
    let authority_len = rest.find('/').unwrap_or(rest.len());
    let origin = &base[..scheme_end + 3 + authority_len];
    let base_path = rest.get(authority_len..).unwrap_or("");

    if location.starts_with("//") {
        // 协议相对：`//host/path` → scheme + location
        let scheme_only = &base[..=scheme_end];
        return Ok(format!("{scheme_only}{location}"));
    }
    if location.starts_with('/') {
        return Ok(format!("{origin}{location}"));
    }

    // 相对路径：基于当前路径的"目录"部分合并（去掉最后一段与 query）
    let dir = match base_path.rfind('/') {
        Some(idx) => &base_path[..=idx],
        None => "/",
    };
    let mut segments: Vec<&str> = dir
        .trim_start_matches('/')
        .split('/')
        .filter(|s| !s.is_empty())
        .collect();
    let mut trailing_slash = false;
    for seg in location.split('/') {
        match seg {
            "" => trailing_slash = true,
            "." => {}
            ".." => {
                segments.pop();
                trailing_slash = false;
            }
            s => {
                segments.push(s);
                trailing_slash = false;
            }
        }
    }
    if trailing_slash {
        let mut path = segments.join("/");
        path.push('/');
        return Ok(format!("{origin}/{path}"));
    }
    Ok(format!("{origin}/{}", segments.join("/")))
}

#[cfg(test)]
mod redirect_tests {
    use super::join_redirect_url;

    #[test]
    fn absolute_location_passthrough() {
        assert_eq!(
            join_redirect_url("https://a.com/x", "https://b.com/y").expect("abs"),
            "https://b.com/y"
        );
    }

    #[test]
    fn relative_location_joined_to_origin() {
        assert_eq!(
            join_redirect_url("https://a.com/x/y", "/z").expect("rel"),
            "https://a.com/z"
        );
    }

    #[test]
    fn relative_location_resolves_against_current_directory() {
        // PTM-12：相对 Location 基于当前路径目录合并（RFC 3986）
        assert_eq!(
            join_redirect_url("https://a.com/b/c", "d/e").expect("rel"),
            "https://a.com/b/d/e"
        );
        assert_eq!(
            join_redirect_url("https://a.com/b/c", "../d").expect("up"),
            "https://a.com/d"
        );
        assert_eq!(
            join_redirect_url("https://a.com/b", "./c/").expect("dot"),
            "https://a.com/c/"
        );
    }

    #[test]
    fn invalid_base_rejected() {
        assert!(join_redirect_url("not-a-url", "/z").is_err());
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::pedantic)]
    use super::*;
    use minicoding_core::model::ToolContent;
    use minicoding_core::tool::ToolContext;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn make_ctx() -> ToolContext {
        ToolContext::new("/tmp/proj".into(), "test".to_string())
    }

    /// 提取 `ToolResult` 文本内容（测试辅助）。
    fn result_text(result: &ToolResult) -> &str {
        match &result.content {
            ToolContent::Text(t) => t,
            _ => "",
        }
    }

    // ---- WebFetch::execute 路径（SSRF / 输入校验）----

    #[tokio::test]
    async fn execute_missing_url_returns_invalid_input() {
        let tool = WebFetch::new();
        let result = tool.execute(serde_json::json!({}), &make_ctx()).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput(_)));
    }

    #[tokio::test]
    async fn execute_url_not_string_returns_invalid_input() {
        let tool = WebFetch::new();
        let result = tool
            .execute(serde_json::json!({"url": 123}), &make_ctx())
            .await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), ToolError::InvalidInput(_)));
    }

    #[tokio::test]
    async fn execute_ssrf_rejects_private_ip() {
        let tool = WebFetch::new();
        let result = tool
            .execute(serde_json::json!({"url": "http://127.0.0.1/"}), &make_ctx())
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn execute_ssrf_rejects_non_http_scheme() {
        let tool = WebFetch::new();
        let result = tool
            .execute(
                serde_json::json!({"url": "ftp://example.com/"}),
                &make_ctx(),
            )
            .await;
        assert!(result.is_err());
    }

    #[test]
    fn execute_side_effect_is_network() {
        let tool = WebFetch::new();
        assert_eq!(tool.side_effect(), SideEffect::Network);
    }

    #[test]
    fn execute_schema_has_correct_name() {
        let tool = WebFetch::new();
        assert_eq!(tool.name(), "web.fetch");
    }

    #[test]
    fn execute_default_equals_new() {
        let a = WebFetch::new();
        let b = WebFetch::default();
        assert_eq!(a.name(), b.name());
        assert_eq!(a.schema().name, b.schema().name);
    }

    // ---- fetch_and_convert：content-type 与 HTML→Markdown ----
    // 直接调用 `fetch_and_convert` 绕过 SSRF（mock server 用 127.0.0.1）。
    // 用 `set_body_raw` 指定 content-type（`set_body_string` 会强制 text/plain）。

    #[tokio::test]
    async fn html_content_converts_to_markdown() {
        let server = MockServer::start().await;
        let html = "<html><head><title>T</title></head>\
                    <body><h1>Hello</h1><p>World</p></body></html>";
        Mock::given(method("GET"))
            .and(path("/page"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw(html.as_bytes().to_owned(), "text/html; charset=utf-8"),
            )
            .mount(&server)
            .await;

        let url = format!("{}/page", server.uri());
        let result = fetch_and_convert(&url, 1024 * 1024)
            .await
            .expect("fetch should succeed");
        assert!(!result.is_error);
        let text = result_text(&result);
        // HTML→Markdown 后应包含标题与段落文本，且转为 Markdown（# Hello）
        assert!(text.contains("Hello"));
        assert!(text.contains("World"));
        assert!(!text.contains("<h1>"));
        assert!(!text.contains("<p>"));
    }

    #[tokio::test]
    async fn plain_text_returned_as_is() {
        let server = MockServer::start().await;
        let body = "just plain text\nline two";
        Mock::given(method("GET"))
            .and(path("/raw"))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw(body.as_bytes().to_owned(), "text/plain"),
            )
            .mount(&server)
            .await;

        let url = format!("{}/raw", server.uri());
        let result = fetch_and_convert(&url, 1024 * 1024)
            .await
            .expect("fetch should succeed");
        let text = result_text(&result);
        assert_eq!(text, body);
    }

    #[tokio::test]
    async fn json_content_returned_as_is() {
        let server = MockServer::start().await;
        let body = r#"{"key":"value","n":42}"#;
        Mock::given(method("GET"))
            .and(path("/api"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw(body.as_bytes().to_owned(), "application/json"),
            )
            .mount(&server)
            .await;

        let url = format!("{}/api", server.uri());
        let result = fetch_and_convert(&url, 1024 * 1024)
            .await
            .expect("fetch should succeed");
        let text = result_text(&result);
        assert!(text.contains(r#""key":"value""#));
        assert!(text.contains("42"));
    }

    #[tokio::test]
    async fn missing_content_type_returns_body_as_text() {
        let server = MockServer::start().await;
        let body = "no content type header";
        // set_body_raw with empty mime → wiremock 不设置 content-type header
        Mock::given(method("GET"))
            .and(path("/no-ct"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(body.as_bytes().to_owned(), ""))
            .mount(&server)
            .await;

        let url = format!("{}/no-ct", server.uri());
        let result = fetch_and_convert(&url, 1024 * 1024)
            .await
            .expect("fetch should succeed");
        let text = result_text(&result);
        assert_eq!(text, body);
    }

    #[tokio::test]
    async fn non_success_status_returns_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/notfound"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let url = format!("{}/notfound", server.uri());
        let result = fetch_and_convert(&url, 1024 * 1024).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        match err {
            ToolError::Exec(msg) => assert!(msg.contains("404")),
            other => panic!("expected Exec error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn server_error_returns_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/err"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let url = format!("{}/err", server.uri());
        let result = fetch_and_convert(&url, 1024 * 1024).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn large_body_truncated_to_max_bytes() {
        let server = MockServer::start().await;
        // 4KB body，远超 1KB 上限
        let body = "A".repeat(4 * 1024);
        Mock::given(method("GET"))
            .and(path("/big"))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw(body.as_bytes().to_owned(), "text/plain"),
            )
            .mount(&server)
            .await;

        let url = format!("{}/big", server.uri());
        let result = fetch_and_convert(&url, 1024)
            .await
            .expect("fetch should succeed");
        let text = result_text(&result);
        // 截断经 truncate_output：字符边界截断 + 追加截断标记（2026-08-23 审查
        // §5-P0：原字节切片在多字节字符中间会 panic）
        assert!(
            text.len() <= 1024,
            "截断后不应超过 max_bytes: {}",
            text.len()
        );
        assert!(text.starts_with('A'));
        assert!(text.ends_with("\n...[output truncated]"));
    }

    #[tokio::test]
    async fn oversized_body_capped_at_limit_with_metadata() {
        // T-6（2026-08-25 审查）：响应体超过 10MiB 硬上限 → 停止读取、
        // metadata.truncated 标注、内容不超过上限（输出上限给足以隔离变量）。
        let server = MockServer::start().await;
        let body = "B".repeat(MAX_BODY_BYTES + 1024);
        Mock::given(method("GET"))
            .and(path("/huge"))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw(body.as_bytes().to_owned(), "text/plain"),
            )
            .mount(&server)
            .await;

        let url = format!("{}/huge", server.uri());
        // 输出上限放大到 64 MiB：确保触发的是响应体级截断而非输出级截断
        let result = fetch_and_convert(&url, 64 * 1024 * 1024)
            .await
            .expect("fetch should succeed");
        assert!(result.metadata.truncated, "超限应标注 truncated");
        match &result.content {
            ToolContent::Text(t) => {
                assert!(
                    t.len() <= MAX_BODY_BYTES,
                    "响应体应被限制在 {} 字节内: {}",
                    MAX_BODY_BYTES,
                    t.len()
                );
                assert!(t.starts_with('B'));
            }
            other => panic!("expected text content, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn small_body_not_marked_truncated() {
        // T-6 回归补充：未触上限的正常响应不误标 truncated。
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/small"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(b"tiny".to_vec(), "text/plain"))
            .mount(&server)
            .await;
        let url = format!("{}/small", server.uri());
        let result = fetch_and_convert(&url, 1024 * 1024)
            .await
            .expect("fetch should succeed");
        assert!(!result.metadata.truncated);
    }

    #[tokio::test]
    async fn sends_user_agent_header() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/ua"))
            .and(header("User-Agent", "minicoding/0.1"))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw("ok".as_bytes().to_owned(), "text/plain"),
            )
            .mount(&server)
            .await;

        let url = format!("{}/ua", server.uri());
        let result = fetch_and_convert(&url, 1024 * 1024)
            .await
            .expect("fetch should succeed (UA matched)");
        assert!(!result.is_error);
    }

    #[tokio::test]
    async fn follows_redirects_up_to_limit() {
        let server = MockServer::start().await;
        // /redir → 302 → /target
        Mock::given(method("GET"))
            .and(path("/redir"))
            .respond_with(ResponseTemplate::new(302).insert_header("Location", "/target"))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/target"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw("redirected content".as_bytes().to_owned(), "text/plain"),
            )
            .mount(&server)
            .await;

        let url = format!("{}/redir", server.uri());
        let result = fetch_and_convert(&url, 1024 * 1024)
            .await
            .expect("fetch should follow redirect");
        let text = result_text(&result);
        assert_eq!(text, "redirected content");
    }

    #[tokio::test]
    async fn empty_body_returns_empty_text() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/empty"))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw("".as_bytes().to_owned(), "text/plain"),
            )
            .mount(&server)
            .await;

        let url = format!("{}/empty", server.uri());
        let result = fetch_and_convert(&url, 1024 * 1024)
            .await
            .expect("fetch should succeed");
        assert_eq!(result_text(&result), "");
        assert!(!result.is_error);
    }

    #[tokio::test]
    async fn html_with_complex_structure_converts() {
        let server = MockServer::start().await;
        // 包含列表、链接、代码块
        let html = "<html><body>\
                    <h2>Title</h2>\
                    <ul><li>Item 1</li><li>Item 2</li></ul>\
                    <a href=\"https://example.com\">link</a>\
                    <pre><code>let x = 1;</code></pre>\
                    </body></html>";
        Mock::given(method("GET"))
            .and(path("/complex"))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw(html.as_bytes().to_owned(), "text/html"),
            )
            .mount(&server)
            .await;

        let url = format!("{}/complex", server.uri());
        let result = fetch_and_convert(&url, 1024 * 1024)
            .await
            .expect("fetch should succeed");
        let text = result_text(&result);
        // 转换后应包含文本内容
        assert!(text.contains("Title"));
        assert!(text.contains("Item 1"));
        assert!(text.contains("Item 2"));
        assert!(text.contains("link"));
        assert!(text.contains("let x = 1;"));
    }

    #[tokio::test]
    async fn xhtml_content_type_not_treated_as_html() {
        let server = MockServer::start().await;
        let html = "<html><body><h1>XHTML</h1></body></html>";
        Mock::given(method("GET"))
            .and(path("/xhtml"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw(html.as_bytes().to_owned(), "application/xhtml+xml"),
            )
            .mount(&server)
            .await;

        let url = format!("{}/xhtml", server.uri());
        let result = fetch_and_convert(&url, 1024 * 1024)
            .await
            .expect("fetch should succeed");
        let text = result_text(&result);
        // application/xhtml+xml 不含 "text/html" 子串，按非 HTML 处理，原样返回
        assert!(text.contains("<h1>XHTML</h1>"));
    }

    #[tokio::test]
    async fn html_with_charset_in_content_type_converts() {
        let server = MockServer::start().await;
        let html = "<html><body><h1>Test</h1></body></html>";
        Mock::given(method("GET"))
            .and(path("/charset"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw(html.as_bytes().to_owned(), "text/html; charset=iso-8859-1"),
            )
            .mount(&server)
            .await;

        let url = format!("{}/charset", server.uri());
        let result = fetch_and_convert(&url, 1024 * 1024)
            .await
            .expect("fetch should succeed");
        let text = result_text(&result);
        // content-type 含 "text/html" 子串（即使带 charset），应触发转换
        assert!(text.contains("Test"));
        assert!(!text.contains("<h1>"));
    }
}
