//! 把远程 MCP 工具包装为本地 `Tool`（见 `design.md` §19.3、`modules.md` §8.4）。
//!
//! `McpToolWrapper` 持有 `Arc<dyn McpClient>` 引用与 server/tool 名字，`execute`
//! 时调 `McpClient::call` 转发到远程 server。`side_effect` 据 server schema 的
//! `readOnlyHint`/`destructiveHint` 映射（C-25）：
//! - `ReadOnly` → `SideEffect::None`（并行 + 直接 Allow）
//! - `Destructive`/未声明 → `SideEffect::Command`（保守默认，串行 + Ask）
//!
//! 权限规则（`policy.toml`）按 `mcp__<server>__<tool>` 名通配匹配，与内置工具
//! 统一经 `PermissionPolicy::check`，审计落 `audit.log` 标注 `mcp_server=<server>`。

use std::sync::Arc;

use minicoding_core::mcp::{McpClient, ToolHint};
use minicoding_core::model::{SideEffect, ToolError, ToolResult, ToolSchema};
use minicoding_core::provider::BoxFuture;
use minicoding_core::tool::{Tool, ToolContext};

/// 远程 MCP 工具的本地包装（注册进 `ToolRegistry`）。
///
/// 一个 `McpToolWrapper` 实例对应远程 server 上的一个工具，`name()` 返回
/// `mcp__<server>__<tool>` 全名。
pub struct McpToolWrapper {
    /// MCP client 引用（共享进程池）。
    client: Arc<dyn McpClient>,
    /// server 名（用于 `McpClient::call`）。
    server: String,
    /// 工具名（server 内部名，不含 `mcp__` 前缀）。
    tool: String,
    /// 工具 schema（`name` 字段为 `mcp__<server>__<tool>` 全名）。
    schema: ToolSchema,
    /// 副作用分类（据 `readOnlyHint`/`destructiveHint` 映射，C-25）。
    side_effect: SideEffect,
    /// 是否只读（据 `readOnlyHint`，与 `side_effect` 独立，见 `Tool::is_read_only`）。
    read_only: bool,
}

impl McpToolWrapper {
    /// 创建包装器。
    ///
    /// `schema.name` 必须已是 `mcp__<server>__<tool>` 全名（由 `RmcpClient::start`
    /// 调 `naming::mcp_tool_name` 生成）。`hint` 决定 `side_effect` 与 `is_read_only`。
    #[must_use]
    pub fn new(
        client: Arc<dyn McpClient>,
        server: String,
        tool: String,
        schema: ToolSchema,
        hint: ToolHint,
    ) -> Self {
        let (side_effect, read_only) = match hint {
            ToolHint::ReadOnly => (SideEffect::None, true),
            ToolHint::Destructive | ToolHint::Unknown => {
                // C-25：未声明 hint 保守按 Command（串行 + Ask）
                (SideEffect::Command, false)
            }
        };
        Self {
            client,
            server,
            tool,
            schema,
            side_effect,
            read_only,
        }
    }
}

impl Tool for McpToolWrapper {
    fn name(&self) -> &str {
        &self.schema.name
    }

    fn schema(&self) -> &ToolSchema {
        &self.schema
    }

    fn side_effect(&self) -> SideEffect {
        self.side_effect
    }

    fn is_read_only(&self) -> bool {
        self.read_only
    }

    fn execute(
        &self,
        input: serde_json::Value,
        _ctx: &ToolContext,
    ) -> BoxFuture<'_, Result<ToolResult, ToolError>> {
        let client = self.client.clone();
        let server = self.server.clone();
        let tool = self.tool.clone();
        Box::pin(async move {
            client
                .call(&server, &tool, input)
                .await
                .map_err(|e| ToolError::Exec(format!("mcp {server}__{tool}: {e}")))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use minicoding_core::mcp::McpError;
    use minicoding_core::model::ToolContent;
    use minicoding_core::tool::ToolRegistry;
    use std::sync::Mutex;

    /// stub McpClient：记录调用并返回预设结果。
    struct StubMcpClient {
        calls: Mutex<Vec<(String, String, serde_json::Value)>>,
        result: ToolResult,
    }

    impl StubMcpClient {
        fn new(result: ToolResult) -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                result,
            }
        }
    }

    impl McpClient for StubMcpClient {
        fn start(
            &self,
            _configs: &[minicoding_core::mcp::McpServerConfig],
        ) -> BoxFuture<'_, Result<(), McpError>> {
            Box::pin(async move { Ok(()) })
        }
        fn list_tools(&self) -> BoxFuture<'_, Vec<ToolSchema>> {
            Box::pin(async move { Vec::new() })
        }
        fn call(
            &self,
            server: &str,
            tool: &str,
            input: serde_json::Value,
        ) -> BoxFuture<'_, Result<ToolResult, McpError>> {
            self.calls.lock().expect("stub poisoned").push((
                server.to_string(),
                tool.to_string(),
                input,
            ));
            let result = self.result.clone();
            Box::pin(async move { Ok(result) })
        }
        fn health_check(&self) -> BoxFuture<'_, Result<bool, McpError>> {
            Box::pin(async move { Ok(true) })
        }
        fn warm_up(&self) -> BoxFuture<'_, Result<(), McpError>> {
            Box::pin(async move { Ok(()) })
        }
        fn shutdown(&self) -> BoxFuture<'_, Result<(), McpError>> {
            Box::pin(async move { Ok(()) })
        }
    }

    fn make_schema(name: &str) -> ToolSchema {
        ToolSchema {
            name: name.to_string(),
            description: "test mcp tool".to_string(),
            input_schema: serde_json::json!({"type": "object"}),
        }
    }

    fn make_ctx() -> ToolContext {
        ToolContext {
            workdir: camino::Utf8PathBuf::from("."),
            session_id: "test".to_string(),
            canceller: tokio_util::sync::CancellationToken::new(),
            env: std::collections::HashMap::new(),
            timeout: std::time::Duration::from_secs(60),
            max_output_bytes: 10_000,
            sandbox_driver: None,
            sandbox_policy: None,
            journal: None,
        }
    }

    #[tokio::test]
    async fn read_only_hint_maps_to_side_effect_none() {
        let client = Arc::new(StubMcpClient::new(ToolResult::ok_text("ok")));
        let wrapper = McpToolWrapper::new(
            client,
            "github".into(),
            "list_prs".into(),
            make_schema("mcp__github__list_prs"),
            ToolHint::ReadOnly,
        );
        assert_eq!(wrapper.side_effect(), SideEffect::None);
        assert!(wrapper.is_read_only());
        assert_eq!(wrapper.name(), "mcp__github__list_prs");
    }

    #[tokio::test]
    async fn unknown_hint_maps_to_command() {
        let client = Arc::new(StubMcpClient::new(ToolResult::ok_text("ok")));
        let wrapper = McpToolWrapper::new(
            client,
            "github".into(),
            "create_pr".into(),
            make_schema("mcp__github__create_pr"),
            ToolHint::Unknown,
        );
        assert_eq!(wrapper.side_effect(), SideEffect::Command);
        assert!(!wrapper.is_read_only());
    }

    #[tokio::test]
    async fn destructive_hint_maps_to_command() {
        let client = Arc::new(StubMcpClient::new(ToolResult::ok_text("ok")));
        let wrapper = McpToolWrapper::new(
            client,
            "db".into(),
            "drop_table".into(),
            make_schema("mcp__db__drop_table"),
            ToolHint::Destructive,
        );
        assert_eq!(wrapper.side_effect(), SideEffect::Command);
        assert!(!wrapper.is_read_only());
    }

    #[tokio::test]
    async fn execute_dispatches_to_client() {
        let client = Arc::new(StubMcpClient::new(ToolResult::ok_text("result")));
        let wrapper = McpToolWrapper::new(
            client.clone(),
            "github".into(),
            "list_prs".into(),
            make_schema("mcp__github__list_prs"),
            ToolHint::ReadOnly,
        );
        let ctx = make_ctx();
        let result = wrapper
            .execute(serde_json::json!({"state": "open"}), &ctx)
            .await
            .unwrap();
        assert!(!result.is_error);
        if let ToolContent::Text(t) = result.content {
            assert_eq!(t, "result");
        } else {
            panic!("expected text content");
        }
        // 验证调用参数
        let calls = client.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "github");
        assert_eq!(calls[0].1, "list_prs");
    }

    #[tokio::test]
    async fn execute_propagates_error() {
        struct ErrClient;
        impl McpClient for ErrClient {
            fn start(
                &self,
                _c: &[minicoding_core::mcp::McpServerConfig],
            ) -> BoxFuture<'_, Result<(), McpError>> {
                Box::pin(async move { Ok(()) })
            }
            fn list_tools(&self) -> BoxFuture<'_, Vec<ToolSchema>> {
                Box::pin(async move { Vec::new() })
            }
            fn call(
                &self,
                _s: &str,
                _t: &str,
                _i: serde_json::Value,
            ) -> BoxFuture<'_, Result<ToolResult, McpError>> {
                Box::pin(async move {
                    Err(McpError::CallFailed {
                        server: "github".into(),
                        tool: "list_prs".into(),
                        reason: "boom".into(),
                    })
                })
            }
            fn health_check(&self) -> BoxFuture<'_, Result<bool, McpError>> {
                Box::pin(async move { Ok(true) })
            }
            fn warm_up(&self) -> BoxFuture<'_, Result<(), McpError>> {
                Box::pin(async move { Ok(()) })
            }
            fn shutdown(&self) -> BoxFuture<'_, Result<(), McpError>> {
                Box::pin(async move { Ok(()) })
            }
        }
        let wrapper = McpToolWrapper::new(
            Arc::new(ErrClient),
            "github".into(),
            "list_prs".into(),
            make_schema("mcp__github__list_prs"),
            ToolHint::ReadOnly,
        );
        let ctx = make_ctx();
        let err = wrapper
            .execute(serde_json::json!({}), &ctx)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("boom"));
    }

    #[tokio::test]
    async fn register_into_tool_registry() {
        let client = Arc::new(StubMcpClient::new(ToolResult::ok_text("ok")));
        let wrapper = McpToolWrapper::new(
            client,
            "github".into(),
            "list_prs".into(),
            make_schema("mcp__github__list_prs"),
            ToolHint::ReadOnly,
        );
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(wrapper));
        let schemas = registry.schemas();
        assert_eq!(schemas.len(), 1);
        assert_eq!(schemas[0].name, "mcp__github__list_prs");
    }
}
