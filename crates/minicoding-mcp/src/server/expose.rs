//! `ToolExposer`：把 `ToolRegistry` 暴露为 MCP server（T-M8-3）。
//!
//! 实现 `rmcp::handler::server::ServerHandler` trait，把 minicoding 内置工具
//! 通过 MCP 协议暴露给外部 client（如 Claude Desktop）。
//!
//! ## 数据流
//!
//! ```text
//! MCP client (Claude Desktop etc.)
//!    │  JSON-RPC over stdio
//!    ▼
//! rmcp serve_server(ToolExposer, (stdin, stdout))
//!    │
//!    ├─ tools/list → ToolExposer::list_tools
//!    │     └─ ToolRegistry::schemas() → 转换为 rmcp Tool
//!    │
//!    └─ tools/call  → ToolExposer::call_tool
//!          └─ ToolRegistry::get(name) → Tool::execute(input, ctx)
//!                └─ ToolResult → 转换为 rmcp CallToolResult（text/image/mixed）
//! ```
//!
//! ## 安全约束
//!
//! - **C-08 工具 schema 正确暴露**：`input_schema` 直接来自 `ToolSchema`，
//!   与本地 LLM 调用使用的 schema 一致；
//! - **C-25 只读性 hint 正确声明**：当前 `ToolSchema` 不携带 `side_effect`，
//!   `ToolAnnotations` 暂留 `None`（MCP client 按保守默认处理）。后续若在
//!   `ToolSchema` 增加 `side_effect` 字段，可在此处填充 `readOnlyHint`/
//!   `destructiveHint`。
//!
//! 注意：MCP server 模式下不再走 minicoding 的 `PermissionPolicy`（权限决策是
//! minicoding 进程内的 LLM 调用才有的）——MCP client（如 Claude Desktop）自行
//! 决定调用方权限。本 server 仅做工具执行，不做权限审批。

use std::borrow::Cow;
use std::sync::Arc;

use minicoding_core::model::{ToolContent, ToolResult, ToolSchema};
use minicoding_core::tool::{ToolContext, ToolRegistry};
use rmcp::model::{
    CallToolRequestParams, CallToolResult, Implementation, ListToolsResult, PaginatedRequestParams,
    ServerInfo,
};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::{ErrorData as McpError, ServerHandler, serve_server};

/// MCP server 暴露器：持有 `ToolRegistry` 与 `ToolContext` 模板，实现 `ServerHandler`。
///
/// 由 `serve_as_mcp_server` 构造并启动。`ctx_template` 在每次 `call_tool` 时 clone
/// 一份注入工具执行（`ToolContext` 已实现 `Clone`）。
///
/// `ToolRegistry` 与 `ToolContext` 均来自 `minicoding-core`，故 `minicoding-mcp`
/// 不依赖 `minicoding-tools`（依赖方向：core ◄ mcp ◄ cli/tools）。
pub struct ToolExposer {
    /// 已注册工具集（由调用方填充，CLI 用 `register_readonly_tools` 等）。
    registry: Arc<ToolRegistry>,
    /// 工具执行上下文模板（每轮 `call_tool` clone 一份）。
    ctx_template: ToolContext,
    /// server 实现信息（name/version，握手时返回给 client）。
    server_impl: Implementation,
}

impl ToolExposer {
    /// 创建暴露器。
    ///
    /// - `registry`：已填充工具的注册表（`Arc` 共享，避免拷贝）；
    /// - `ctx_template`：工具执行上下文模板（`workdir`/`session_id`/`timeout` 等）；
    /// - `server_impl`：MCP `Implementation` 元信息（name/version）。
    #[must_use]
    pub fn new(
        registry: Arc<ToolRegistry>,
        ctx_template: ToolContext,
        server_impl: Implementation,
    ) -> Self {
        Self {
            registry,
            ctx_template,
            server_impl,
        }
    }
}

impl ServerHandler for ToolExposer {
    /// 握手时返回 server 信息（声明 tools capability）。
    fn get_info(&self) -> ServerInfo {
        let capabilities = rmcp::model::ServerCapabilities::builder()
            .enable_tools()
            .build();
        // rmcp `ServerInfo`（`InitializeResult`）为 `#[non_exhaustive]`，必须走构造器。
        ServerInfo::new(capabilities)
            .with_server_info(self.server_impl.clone())
            .with_instructions(format!(
                "minicoding MCP server: exposes {} built-in tools (fs.read/fs.write/shell.run etc.). \
                 Tool schemas match the local Runtime.",
                self.registry.len()
            ))
    }

    /// `tools/list`：返回所有工具 schema（转换为 rmcp `Tool`）。
    fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<ListToolsResult, McpError>>
    + rmcp::service::MaybeSendFuture
    + '_ {
        // annotations（2026-08-23 审查遗留#5）：按本地 side_effect 填 readOnlyHint
        // ——此前恒 None，外部 client 无法做只读优化/提示。
        let tools: Vec<rmcp::model::Tool> =
            self.registry
                .schemas()
                .into_iter()
                .map(|schema| {
                    let read_only = self.registry.get(&schema.name).is_some_and(|t| {
                        t.side_effect() == minicoding_core::model::SideEffect::None
                    });
                    let mut tool = convert_schema_to_mcp_tool(schema);
                    // ToolAnnotations 为 #[non_exhaustive]：default + 字段赋值
                    let mut ann = rmcp::model::ToolAnnotations::default();
                    ann.read_only_hint = Some(read_only);
                    ann.destructive_hint = Some(!read_only);
                    tool.annotations = Some(ann);
                    tool
                })
                .collect();
        std::future::ready(Ok(ListToolsResult {
            tools,
            ..Default::default()
        }))
    }

    /// `tools/call`：派发到 `ToolRegistry::get` → `Tool::execute`，转换结果为 `CallToolResult`。
    fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<CallToolResult, McpError>>
    + rmcp::service::MaybeSendFuture
    + '_ {
        let name = request.name.to_string();
        let input: serde_json::Value = match request.arguments {
            Some(map) => serde_json::Value::Object(map),
            None => serde_json::Value::Null,
        };
        let tool = self.registry.get(&name);
        let ctx = self.ctx_template.clone();
        async move {
            let Some(tool) = tool else {
                // 工具不存在 → 协议错误（method not found），让 client 看到清晰错误
                return Err(McpError::method_not_found::<
                    rmcp::model::CallToolRequestMethod,
                >());
            };
            // 直接执行工具（不经过 PermissionPolicy——MCP server 模式下权限由
            // 调用方 client 自行决定，本 server 仅做工具执行）。
            let result = tool.execute(input, &ctx).await;
            Ok(convert_tool_result_to_mcp(result))
        }
    }
}

/// 启动 MCP server（stdio 传输，阻塞当前 task）。
///
/// 读 `stdin` / 写 `stdout`，遵循 MCP stdio 传输协议（与 Claude Desktop 等
/// 客户端的 `command` 配置对齐）。客户端断开 stdin 时 server 退出。
///
/// `server_name` / `server_version` 用于 MCP 握手时返回的 `Implementation`
/// 元信息（client 据此识别 server 身份），调用方无需自行构造 `rmcp` 类型——
/// 本函数封装在 `minicoding-mcp` 内部，避免 `rmcp` 类型泄漏到上游 crate
/// （AGENTS.md §3.5：重依赖 `rmcp` 只在 `minicoding-mcp` 引入）。
///
/// # Errors
/// - rmcp 握手失败（client 未发 `initialize` 请求）；
/// - stdio IO 错误。
///
/// 返回 `Box<rmcp::RmcpError>`：`RmcpError` 体积较大（含 `ServiceError` 等
/// 嵌套变体，>500 字节），直接作为 `Result` 的 `Err` 变体会触发
/// `clippy::result_large_err`；装箱消除栈开销。
pub async fn serve_as_mcp_server(
    registry: Arc<ToolRegistry>,
    ctx_template: ToolContext,
    server_name: &str,
    server_version: &str,
) -> Result<(), Box<rmcp::RmcpError>> {
    let server_impl = Implementation::new(server_name, server_version);
    let exposer = ToolExposer::new(registry, ctx_template, server_impl);
    let (stdin, stdout) = rmcp::transport::stdio();
    // 先 `RmcpError::from` 把 `ServerInitializeError`/`JoinError` 统一成 `RmcpError`，
    // 再 `Box::new` 装箱避免 `result_large_err`。
    let service = serve_server(exposer, (stdin, stdout))
        .await
        .map_err(rmcp::RmcpError::from)
        .map_err(Box::new)?;
    // 阻塞至 client 断开连接或 server 被取消
    let _ = service
        .waiting()
        .await
        .map_err(rmcp::RmcpError::from)
        .map_err(Box::new)?;
    Ok(())
}

// ─── 转换辅助 ─────────────────────────────────────────────────────────────

/// minicoding `ToolSchema` → rmcp `Tool`。
///
/// `input_schema`（`serde_json::Value::Object`）转为 `Arc<JsonObject>`；
/// 非 object（如 Null）退化为空对象，避免 rmcp schema 校验失败。
///
/// PTM-11（2026-08-25 R2 审查）：本函数只做 schema 转换，**不填**
/// `annotations`——readOnlyHint/destructiveHint 由调用方 `list_tools` 按本地
/// registry 的 `side_effect` 填充（遗留#5 已落地），此前的过时注释与该实现
/// 矛盾，已按现状修正。
fn convert_schema_to_mcp_tool(schema: ToolSchema) -> rmcp::model::Tool {
    let input_schema = match schema.input_schema {
        serde_json::Value::Object(map) => Arc::new(map),
        _ => Arc::new(serde_json::Map::new()),
    };
    // rmcp `Tool` 为 `#[non_exhaustive]`，用 `new_with_raw` 构造器（接受可选 description）。
    rmcp::model::Tool::new_with_raw(
        Cow::Owned(schema.name),
        Some(Cow::Owned(schema.description)),
        input_schema,
    )
}

/// minicoding `ToolResult` → rmcp `CallToolResult`。
///
/// - `ToolContent::Text(s)` → `ContentBlock::text(s)`；
/// - `ToolContent::Json(v)` → `ContentBlock::text(v.to_string())`（MCP 无原生 JSON）；
/// - `ToolContent::Image { mime, data }` → `ContentBlock::image(base64(data), mime)`；
/// - `ToolContent::Mixed(vec)` → 展开为多个 `ContentBlock`。
///
/// `is_error` 透传。工具执行错误（`Err`）映射为 `CallToolResult` 的 `is_error = true`
/// （caller 可见，区别于协议层 `Err(ErrorData)`）。
fn convert_tool_result_to_mcp(
    result: Result<ToolResult, minicoding_core::model::ToolError>,
) -> CallToolResult {
    // rmcp `CallToolResult` 为 `#[non_exhaustive]`，用 `success`/`error` 构造器。
    // `success` 置 `is_error = Some(false)`，`error` 置 `is_error = Some(true)`，
    // 与 minicoding `ToolResult::is_error` 语义对齐。
    match result {
        Ok(tool_result) => {
            let content = convert_content_to_blocks(tool_result.content);
            if tool_result.is_error {
                CallToolResult::error(content)
            } else {
                CallToolResult::success(content)
            }
        }
        Err(e) => CallToolResult::error(vec![rmcp::model::ContentBlock::text(e.to_string())]),
    }
}

/// minicoding `ToolContent` → rmcp `Vec<ContentBlock>`。
fn convert_content_to_blocks(content: ToolContent) -> Vec<rmcp::model::ContentBlock> {
    match content {
        ToolContent::Text(s) => vec![rmcp::model::ContentBlock::text(s)],
        ToolContent::Json(v) => vec![rmcp::model::ContentBlock::text(v.to_string())],
        ToolContent::Image { mime, data } => {
            vec![rmcp::model::ContentBlock::image(base64_encode(&data), mime)]
        }
        ToolContent::Mixed(parts) => parts
            .into_iter()
            .flat_map(convert_content_to_blocks)
            .collect(),
    }
}

/// base64 编码（MCP wire format：RFC 4648 标准 alphabet + padding；2026-08-25
/// 审查 MC-3 以 `base64` crate 替换手写实现，与 client 侧解码对称）。
fn base64_encode(data: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(data)
}

// ─── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_encode_rfc_vectors() {
        // RFC 4648 测试向量
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn convert_schema_object_input_preserved() {
        let schema = ToolSchema {
            name: "fs.read".into(),
            description: "read a file".into(),
            input_schema: serde_json::json!({"type": "object", "properties": {"path": {"type": "string"}}}),
        };
        let tool = convert_schema_to_mcp_tool(schema);
        assert_eq!(tool.name, "fs.read");
        assert_eq!(tool.description.as_deref(), Some("read a file"));
        assert_eq!(
            tool.input_schema.get("type").and_then(|v| v.as_str()),
            Some("object")
        );
        assert!(tool.annotations.is_none());
    }

    #[test]
    fn convert_schema_non_object_input_becomes_empty() {
        let schema = ToolSchema {
            name: "broken".into(),
            description: "schema with null input".into(),
            input_schema: serde_json::Value::Null,
        };
        let tool = convert_schema_to_mcp_tool(schema);
        assert!(
            tool.input_schema.is_empty(),
            "expected empty: tool.input_schema"
        );
    }

    #[test]
    fn convert_content_text_to_text_block() {
        let blocks = convert_content_to_blocks(ToolContent::Text("hello".into()));
        assert_eq!(blocks.len(), 1);
        match &blocks[0] {
            rmcp::model::ContentBlock::Text(t) => assert_eq!(t.text, "hello"),
            other => panic!("expected Text, got {other:?}"),
        }
    }

    #[test]
    fn convert_content_json_serialized_as_text() {
        let blocks = convert_content_to_blocks(ToolContent::Json(serde_json::json!({"k": "v"})));
        assert_eq!(blocks.len(), 1);
        match &blocks[0] {
            rmcp::model::ContentBlock::Text(t) => assert!(t.text.contains("\"k\"")),
            other => panic!("expected Text, got {other:?}"),
        }
    }

    #[test]
    fn convert_content_image_to_image_block() {
        let blocks = convert_content_to_blocks(ToolContent::Image {
            mime: "image/png".into(),
            data: b"foo".to_vec(),
        });
        assert_eq!(blocks.len(), 1);
        match &blocks[0] {
            rmcp::model::ContentBlock::Image(img) => {
                assert_eq!(img.mime_type, "image/png");
                assert_eq!(img.data, "Zm9v"); // base64("foo")
            }
            other => panic!("expected Image, got {other:?}"),
        }
    }

    #[test]
    fn convert_content_mixed_flattens() {
        let mixed = ToolContent::Mixed(vec![
            ToolContent::Text("a".into()),
            ToolContent::Text("b".into()),
        ]);
        let blocks = convert_content_to_blocks(mixed);
        assert_eq!(blocks.len(), 2);
    }

    #[test]
    fn convert_tool_result_ok_passes_is_error() {
        let result = ToolResult {
            content: ToolContent::Text("ok".into()),
            is_error: false,
            metadata: minicoding_core::model::ToolResultMeta::default(),
        };
        let mcp = convert_tool_result_to_mcp(Ok(result));
        assert_eq!(mcp.is_error, Some(false));
        assert_eq!(mcp.content.len(), 1);
    }

    #[test]
    fn convert_tool_result_err_marks_is_error_true() {
        let err = minicoding_core::model::ToolError::Exec("boom".into());
        let mcp = convert_tool_result_to_mcp(Err(err));
        assert_eq!(mcp.is_error, Some(true));
        match &mcp.content[0] {
            rmcp::model::ContentBlock::Text(t) => assert!(t.text.contains("boom")),
            other => panic!("expected Text error, got {other:?}"),
        }
    }
}
