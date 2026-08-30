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
use minicoding_core::model::{McpError, SideEffect, ToolError, ToolResult, ToolSchema};
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
        trust_read_only_hint: bool,
    ) -> Self {
        let (side_effect, read_only) = match hint {
            // S13/C-25：readOnlyHint 是远端进程的自我声明——仅在用户显式信任该
            // server 时才免检；默认按 Command 处理（串行 + Ask，完整权限链）
            ToolHint::ReadOnly if trust_read_only_hint => (SideEffect::None, true),
            ToolHint::ReadOnly | ToolHint::Destructive | ToolHint::Unknown => {
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
        ctx: &ToolContext,
    ) -> BoxFuture<'_, Result<ToolResult, ToolError>> {
        let client = self.client.clone();
        let server = self.server.clone();
        let tool = self.tool.clone();
        let input_schema = self.schema.input_schema.clone();
        // SEC-R7-3（2026-08-28 R7 审查）：MCP 工具调用结果此前无审计记录——
        // 模块头注释声称"审计落 audit.log 标注 mcp_server"但 execute 不调
        // `AuditSink`（ToolContext.audit 被 `_ctx` 忽略）。副作用 MCP 工具
        // （或任何 MCP 工具）的调用结果是安全取证的关键路径，补齐审计。
        let audit = ctx.audit.clone();
        let session_id = ctx.session_id.clone();
        // R9 MCP-6：远端工具结果无输出上限（恶意 server 可返回无上限 payload
        // 灌爆上下文窗口）。套用 `max_output_bytes` 截断（默认 1 MiB），
        // 与内置工具 `shell.run`/`web.fetch` 同口径。
        let max_output_bytes = ctx.max_output_bytes;
        Box::pin(async move {
            // JSON Schema 全量校验（2026-08-23 审查遗留#5 升级：jsonschema crate）
            // 此前仅 required 键预检，type/enum/pattern 等约束不生效。
            // 校验失败 → InvalidInput（LLM 可自行修正参数重试）。
            //
            // SEC-18（2026-08-27 R5 审查）：schema 编译失败此前 fail-open 静默跳过
            // 校验直接转发参数——远端 schema 异常（畸形/超复杂度）时校验防线整体
            // 旁路。改 fail-closed：编译失败报 InvalidInput（不调用远端），把"无法
            // 校验"当成"校验不通过"处理，与 C-01 最小权限方向一致。退化代价：
            // 偶发 schema 编译失败的 server 工具不可用，需 server 侧修复——可预期、
            // 可诊断（错误信息含编译原因），优于静默转发。
            match jsonschema::validator_for(&input_schema) {
                Ok(compiled) => {
                    let errors: Vec<String> = compiled
                        .iter_errors(&input)
                        .map(|e| format!("  {}: {}", e.instance_path(), e))
                        .collect();
                    if !errors.is_empty() {
                        return Err(ToolError::InvalidInput(format!(
                            "mcp {server}__{tool}: 入参不符合 schema:\n{}",
                            errors.join("\n")
                        )));
                    }
                }
                Err(e) => {
                    return Err(ToolError::InvalidInput(format!(
                        "mcp {server}__{tool}: 远端 schema 编译失败（fail-closed，不转发参数）: {e}"
                    )));
                }
            }
            // OTel `mcp.call` span（T-M5-8，O-08）：记录 server/tool，elapsed 由 span 自动携带。
            // 与 `hook.run` span 同构（见 `core::hooks::HookRegistry::dispatch`），
            // 便于在 collector 侧按 otel.name 聚合 MCP 调用延迟。
            let span = tracing::info_span!(
                "mcp.call",
                mcp.server = %server,
                mcp.tool = %tool,
                otel.name = "mcp.call",
            );
            let _enter = span.enter();
            let result = match client.call(&server, &tool, input.clone()).await {
                Ok(r) => Ok(r),
                // CT4-4（R4）：仅连接级错误启动重启——业务错误（`ToolNotFound`、
                // `CallFailed` 含 Schema 不匹配/参数错等）不触发全池重建，
                // 此前任何调用失败都触 `client.restart()`（杀死并重启所有子进程），
                // 且并发多路失败时竞争重建、中断期间 call 持 read 锁排队。
                Err(e @ (McpError::NotReady(_) | McpError::StartFailed { .. })) => {
                    match client.restart().await {
                        Ok(()) => client.call(&server, &tool, input).await.map_err(|e2| {
                            ToolError::Exec(format!(
                                "mcp {server}__{tool}: 重启后仍失败: {e2}（首次: {e}）"
                            ))
                        }),
                        Err(restart_err) => Err(ToolError::Exec(format!(
                            "mcp {server}__{tool}: {e}（重启失败: {restart_err}）"
                        ))),
                    }
                }
                Err(e) => Err(ToolError::Exec(format!("mcp {server}__{tool}: {e}"))),
            };
            // SEC-R7-3：调用结果（成功/失败）落 `audit.log`，标注 mcp_server/tool，
            // 与内置工具 `kind=tool_result` 同格式（best-effort，不阻塞工具结果）。
            if let Some(audit) = audit {
                use minicoding_core::storage::{AuditKind, AuditRecord};
                let (is_error, bytes) = match &result {
                    Ok(r) => (r.is_error, r.metadata.bytes),
                    Err(e) => (true, e.to_string().len()),
                };
                let rec = AuditRecord {
                    ts: time::OffsetDateTime::now_utc(),
                    session: session_id,
                    kind: AuditKind::ToolResult,
                    tool: Some(format!("mcp__{server}__{tool}")),
                    decision: Some(if is_error { "error" } else { "ok" }.to_string()),
                    detail: serde_json::json!({
                        "mcp_server": server,
                        "mcp_tool": tool,
                        "result_bytes": bytes,
                    })
                    .to_string(),
                };
                if let Err(e) = audit.record(rec).await {
                    tracing::warn!(error = %e, "mcp tool audit record failed (best-effort)");
                }
            }
            // R9 MCP-6：远端工具结果输出上限——`client.call` 返回的 `ToolResult`
            // 可能远超 `max_output_bytes`（恶意 server 可灌爆上下文窗口）。
            // 与内置工具（shell/web）同口径：超限截断并置 `metadata.truncated`。
            result.map(|r| cap_result_output(r, max_output_bytes))
        })
    }
}

/// R9 MCP-6：按字节上限截断 `ToolResult` 文本内容，超限置 `truncated`。
fn cap_result_output(mut result: ToolResult, cap: usize) -> ToolResult {
    fn truncate_text(text: &mut String, cap: usize) -> bool {
        if text.len() <= cap {
            return false;
        }
        // 按字符边界截断（防切断 UTF-8），保留上限内前缀
        let mut budget = cap.saturating_sub("…[output truncated]".len());
        let mut prefix = String::new();
        for c in text.chars() {
            if budget < c.len_utf8() {
                break;
            }
            prefix.push(c);
            budget -= c.len_utf8();
        }
        prefix.push_str("…[output truncated]");
        *text = prefix;
        true
    }

    let mut truncated = false;
    match &mut result.content {
        minicoding_core::model::ToolContent::Text(s) => truncated = truncate_text(s, cap),
        minicoding_core::model::ToolContent::Json(v) => {
            let mut s = v.to_string();
            truncated = truncate_text(&mut s, cap);
            if truncated {
                // 序列化超限：把 JSON 内容替换为截断文本（`_truncated` 标记），
                // 避免原 Value 在回灌 LLM 时仍全量序列化
                result.content = minicoding_core::model::ToolContent::Text(s);
            }
        }
        minicoding_core::model::ToolContent::Mixed(parts) => {
            for part in parts {
                if let minicoding_core::model::ToolContent::Text(s) = part {
                    truncated |= truncate_text(s, cap);
                }
            }
        }
        minicoding_core::model::ToolContent::Image { data, .. } => {
            if data.len() > cap {
                data.truncate(cap);
                truncated = true;
            }
        }
    }
    if truncated {
        result.metadata.truncated = true;
        result.metadata.bytes = cap;
    }
    result
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
            max_read_bytes: 10_000,
            sandbox_driver: None,
            sandbox_policy: None,
            journal: None,
            prompter: None,
            events: None,
            audit: None,
        }
    }

    #[tokio::test]
    async fn read_only_hint_maps_to_side_effect_none() {
        let client = Arc::new(StubMcpClient::new(ToolResult::ok_text("ok")));
        let wrapper = McpToolWrapper::new(
            client.clone(),
            "github".into(),
            "list_prs".into(),
            make_schema("mcp__github__list_prs"),
            ToolHint::ReadOnly,
            false,
        );
        // S13：默认不信任远端自报只读 → 按 Command 处理（串行 + Ask）
        assert_eq!(wrapper.side_effect(), SideEffect::Command);
        assert!(!wrapper.is_read_only());

        // 显式信任时才免检
        let trusted = McpToolWrapper::new(
            client.clone(),
            "github".into(),
            "list_prs".into(),
            make_schema("mcp__github__list_prs"),
            ToolHint::ReadOnly,
            true,
        );
        assert_eq!(trusted.side_effect(), SideEffect::None);
        assert!(trusted.is_read_only());
        assert_eq!(wrapper.name(), "mcp__github__list_prs");
    }

    #[tokio::test]
    async fn unknown_hint_maps_to_command() {
        let client = Arc::new(StubMcpClient::new(ToolResult::ok_text("ok")));
        let wrapper = McpToolWrapper::new(
            client.clone(),
            "github".into(),
            "list_prs".into(),
            make_schema("mcp__github__list_prs"),
            ToolHint::ReadOnly,
            false,
        );
        assert_eq!(wrapper.side_effect(), SideEffect::Command);
        assert!(!wrapper.is_read_only());
    }

    #[tokio::test]
    async fn destructive_hint_maps_to_command() {
        let client = Arc::new(StubMcpClient::new(ToolResult::ok_text("ok")));
        let wrapper = McpToolWrapper::new(
            client.clone(),
            "db".into(),
            "drop_table".into(),
            make_schema("mcp__db__drop_table"),
            ToolHint::Destructive,
            false,
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
            false,
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
            false,
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
            false,
        );
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(wrapper));
        let schemas = registry.schemas();
        assert_eq!(schemas.len(), 1);
        assert_eq!(schemas[0].name, "mcp__github__list_prs");
    }
}
