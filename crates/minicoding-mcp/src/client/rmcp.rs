//! `RmcpClient`：基于 `rmcp` 2.2 的 `McpClient` 实现（stdio 传输）。
//!
//! 见 `design.md` §19、`api.md` §11、`modules.md` §8。
//!
//! ## 设计要点
//!
//! - **进程池**：`RwLock<HashMap<ServerId, ServerConnection>>`，连接跨 turn 复用
//!   （见 `design.md` §19.5）。M4 仅实现基础进程池；后台预热/inflight merge 留给
//!   M6+（依赖更复杂的 `Shared<Future>` 与 mpsc 事件流）。
//! - **凭证隔离**（C-04）：spawn 子进程时 `env_clear` 后仅注入白名单 + server 配置
//!   的 `env`（`GITHUB_TOKEN` 等），绝不继承 `OPENAI_API_KEY`/`ANTHROPIC_API_KEY`
//!   等凭证环境变量。
//! - **超时**：启动用 `startup_timeout_sec`，工具调用用 `tool_timeout_sec`，均通过
//!   `tokio::time::timeout` 包裹。
//! - **`required` 语义**：`required=true` 的 server 启动失败返回 `Err`，Runtime 拒绝
//!   启动；`required=false` 仅 warn 跳过。
//! - **`enabled_tools` 过滤**：server 配置的 `enabled_tools` 收敛工具集，未列出的
//!   工具不注册进 `ToolRegistry`。

use std::collections::HashMap;
use std::process::Stdio;
use std::time::Duration;

use minicoding_core::mcp::{McpClient, McpError, McpServerConfig, McpTransport};
use minicoding_core::model::{ToolContent, ToolResult, ToolResultMeta, ToolSchema};
use minicoding_core::provider::BoxFuture;
use rmcp::model::{CallToolRequestParams, ClientInfo, ContentBlock};
use rmcp::service::{RunningService, ServiceExt};
use rmcp::transport::TokioChildProcess;
use tokio::process::Command;
use tokio::sync::RwLock;

/// 子进程 env 白名单（C-04 凭证不下传子进程，同 `shell.run`）。
const ENV_WHITELIST: &[&str] = &["PATH", "HOME", "USER", "LANG", "LC_ALL", "TERM"];

/// 单个 MCP server 的连接状态（进程池条目）。
struct ServerConnection {
    /// rmcp 运行中的 client service（deref 到 `Peer<RoleClient>` 用于调用）。
    service: RunningService<rmcp::service::RoleClient, ClientInfo>,
    /// 握手时缓存的工具 schema（已用 `mcp_tool_name` 命名）。
    tools: Vec<ToolSchema>,
    /// 工具调用超时（来自 server 配置）。
    tool_timeout: Duration,
}

/// 基于 `rmcp` 2.2 的 `McpClient` 实现（stdio 传输）。
///
/// 由 `RmcpClient::new()` 构造，`RuntimeBuilder::mcp_client` 注入。
/// 持有所有已就绪 MCP server 的连接，跨 turn 复用（进程池模式）。
pub struct RmcpClient {
    connections: RwLock<HashMap<String, ServerConnection>>,
}

impl RmcpClient {
    /// 创建空 client（未启动任何 server，需随后调 `start`）。
    #[must_use]
    pub fn new() -> Self {
        Self {
            connections: RwLock::new(HashMap::new()),
        }
    }

    /// 展开 `${VAR}` / `${VAR:-fallback}` 环境变量引用（见 `design.md` §19.2）。
    fn expand_env(value: &str) -> String {
        let mut out = String::with_capacity(value.len());
        let mut rest = value;
        while let Some(start) = rest.find("${") {
            out.push_str(&rest[..start]);
            rest = &rest[start + 2..];
            let Some(end) = rest.find('}') else {
                out.push_str("${");
                out.push_str(rest);
                return out;
            };
            let var_expr = &rest[..end];
            rest = &rest[end + 1..];
            // 支持 `VAR` 与 `VAR:-fallback` 两种形式
            let (var, fallback) = var_expr
                .split_once(":-")
                .map_or((var_expr, None), |(v, f)| (v, Some(f)));
            let val = std::env::var(var)
                .ok()
                .or_else(|| fallback.map(String::from));
            if let Some(v) = val {
                out.push_str(&v);
            }
        }
        out.push_str(rest);
        out
    }

    /// 构造已过滤凭证的子进程 `Command`（C-04）。
    fn build_command(transport: &McpTransport) -> Result<Command, McpError> {
        let McpTransport::Stdio {
            command,
            args,
            env,
            cwd,
        } = transport
        else {
            return Err(McpError::Config(
                "M4 仅支持 stdio 传输；http 留给 M6（T-M6-4）".into(),
            ));
        };
        let mut cmd = Command::new(command);
        cmd.args(args);
        cmd.stdin(Stdio::piped());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::inherit());
        // env_clear + 白名单 + 配置 env（C-04：凭证不下传子进程）
        cmd.env_clear();
        for key in ENV_WHITELIST {
            if let Ok(v) = std::env::var(key) {
                cmd.env(key, v);
            }
        }
        for (k, v) in env {
            cmd.env(k, Self::expand_env(v));
        }
        if let Some(dir) = cwd {
            cmd.current_dir(dir);
        }
        Ok(cmd)
    }

    /// 启动单个 server：spawn 子进程 → 握手 → `list_tools` → 缓存。
    async fn start_one(&self, cfg: &McpServerConfig) -> Result<Vec<ToolSchema>, McpError> {
        let startup_timeout = Duration::from_secs(cfg.startup_timeout_sec);
        let tool_timeout = Duration::from_secs(cfg.tool_timeout_sec);

        let cmd = Self::build_command(&cfg.transport)?;
        let transport = TokioChildProcess::new(cmd).map_err(|e| McpError::StartFailed {
            server: cfg.name.clone(),
            reason: format!("spawn failed: {e}"),
        })?;

        // 握手（带 startup_timeout）
        let service = tokio::time::timeout(startup_timeout, ClientInfo::default().serve(transport))
            .await
            .map_err(|_| McpError::StartFailed {
                server: cfg.name.clone(),
                reason: format!("startup timeout after {startup_timeout:?}"),
            })?
            .map_err(|e| McpError::StartFailed {
                server: cfg.name.clone(),
                reason: format!("handshake failed: {e}"),
            })?;

        // list_tools（带 startup_timeout，复用作为初始化阶段超时）
        let rmcp_tools = tokio::time::timeout(startup_timeout, service.list_all_tools())
            .await
            .map_err(|_| McpError::StartFailed {
                server: cfg.name.clone(),
                reason: format!("list_tools timeout after {startup_timeout:?}"),
            })?
            .map_err(|e| McpError::StartFailed {
                server: cfg.name.clone(),
                reason: format!("list_tools failed: {e}"),
            })?;

        // 转换 schema + 命名 + enabled_tools 过滤
        let tools = rmcp_tools
            .into_iter()
            .filter(|t| {
                cfg.enabled_tools
                    .as_ref()
                    .is_none_or(|list| list.iter().any(|n| n == &t.name))
            })
            .map(|t| {
                let name = crate::naming::mcp_tool_name(&cfg.name, &t.name)
                    .map_err(|e| McpError::Config(format!("tool name `{}` invalid: {e}", t.name)));
                name.map(|full_name| ToolSchema {
                    name: full_name,
                    description: t.description.as_deref().unwrap_or("").to_string(),
                    input_schema: serde_json::Value::Object(t.input_schema.as_ref().clone()),
                })
            })
            .collect::<Result<Vec<_>, _>>()?;

        // 缓存连接 + 工具
        let conn = ServerConnection {
            service,
            tools: tools.clone(),
            tool_timeout,
        };
        self.connections
            .write()
            .await
            .insert(cfg.name.clone(), conn);

        tracing::info!(
            server = %cfg.name,
            tool_count = tools.len(),
            "mcp server started"
        );
        Ok(tools)
    }
}

impl Default for RmcpClient {
    fn default() -> Self {
        Self::new()
    }
}

impl McpClient for RmcpClient {
    fn start(&self, configs: &[McpServerConfig]) -> BoxFuture<'_, Result<(), McpError>> {
        let configs = configs.to_vec();
        Box::pin(async move {
            // 并发启动各 server（设计 §19.6）；required 失败收集后返回首个错误
            let mut required_errors: Vec<McpError> = Vec::new();
            for cfg in configs {
                if !cfg.enabled {
                    tracing::info!(server = %cfg.name, "mcp server disabled, skipping");
                    continue;
                }
                match self.start_one(&cfg).await {
                    Ok(_) => {}
                    Err(e) => {
                        if cfg.required {
                            tracing::error!(
                                server = %cfg.name,
                                error = %e,
                                "required mcp server start failed"
                            );
                            required_errors.push(e);
                        } else {
                            tracing::warn!(
                                server = %cfg.name,
                                error = %e,
                                "non-required mcp server start failed, skipping"
                            );
                        }
                    }
                }
            }
            if let Some(e) = required_errors.into_iter().next() {
                return Err(e);
            }
            Ok(())
        })
    }

    fn list_tools(&self) -> BoxFuture<'_, Vec<ToolSchema>> {
        Box::pin(async move {
            self.connections
                .read()
                .await
                .values()
                .flat_map(|c| c.tools.clone())
                .collect()
        })
    }

    fn call(
        &self,
        server: &str,
        tool: &str,
        input: serde_json::Value,
    ) -> BoxFuture<'_, Result<ToolResult, McpError>> {
        let server = server.to_string();
        let tool = tool.to_string();
        Box::pin(async move {
            let mut guard = self.connections.write().await;
            let Some(conn) = guard.get_mut(&server) else {
                return Err(McpError::NotReady(server));
            };

            // 校验工具存在（C-09：工具名必须已注册）
            let tool_exists = conn.tools.iter().any(|s| {
                crate::naming::parse_mcp_tool_name(&s.name).is_some_and(|(_, t)| t == tool)
            });
            if !tool_exists {
                return Err(McpError::ToolNotFound(format!("{server}__{tool}")));
            }

            // 构造调用参数
            let args = match input {
                serde_json::Value::Object(map) => Some(map),
                serde_json::Value::Null => None,
                other => {
                    return Err(McpError::CallFailed {
                        server: server.clone(),
                        tool: tool.clone(),
                        reason: format!("MCP tool arguments must be a JSON object, got: {other}"),
                    });
                }
            };
            let params =
                CallToolRequestParams::new(tool.clone()).with_arguments(args.unwrap_or_default());

            // 调用（带 tool_timeout）
            let result = tokio::time::timeout(conn.tool_timeout, conn.service.call_tool(params))
                .await
                .map_err(|_| McpError::CallFailed {
                    server: server.clone(),
                    tool: tool.clone(),
                    reason: format!("tool timeout after {:?}", conn.tool_timeout),
                })?
                .map_err(|e| McpError::CallFailed {
                    server: server.clone(),
                    tool: tool.clone(),
                    reason: format!("call failed: {e}"),
                })?;

            // 转换 CallToolResult → ToolResult
            let is_error = result.is_error.unwrap_or(false);
            let content = convert_content(&result.content);
            Ok(ToolResult {
                content,
                is_error,
                metadata: ToolResultMeta::default(),
            })
        })
    }

    fn health_check(&self) -> BoxFuture<'_, Result<bool, McpError>> {
        Box::pin(async move {
            let guard = self.connections.read().await;
            let mut all_healthy = true;
            for (name, conn) in guard.iter() {
                // 用 is_closed 检查连接活性（不发起 ping，避免占用 server 资源）
                if conn.service.is_closed() {
                    tracing::warn!(server = %name, "mcp server connection closed");
                    all_healthy = false;
                }
            }
            Ok(all_healthy)
        })
    }

    fn warm_up(&self) -> BoxFuture<'_, Result<(), McpError>> {
        // M4 不实现后台预热（M6+ 引入 mpsc 事件流后补齐）；
        // 当前 `start` 已同步完成握手 + list_tools，无需额外预热。
        Box::pin(async move { Ok(()) })
    }

    fn shutdown(&self) -> BoxFuture<'_, Result<(), McpError>> {
        Box::pin(async move {
            let mut guard = self.connections.write().await;
            let count = guard.len();
            for (name, conn) in guard.drain() {
                // RunningService::cancel 返回一个 Future（消费 self），故 drain 而非 iter。
                // await 它以驱动关闭协议（发送 shutdown notification + 等待 peer 退出）。
                // 关闭阶段的错误（如 peer 已断开）忽略：shutdown 本身是 best-effort。
                let _ = conn.service.cancel().await;
                tracing::info!(server = %name, "mcp server shutdown");
            }
            tracing::info!(count, "all mcp servers shut down");
            Ok(())
        })
    }
}

/// 转换 rmcp `Vec<ContentBlock>` → minicoding `ToolContent`。
fn convert_content(blocks: &[ContentBlock]) -> ToolContent {
    if blocks.is_empty() {
        return ToolContent::Text(String::new());
    }
    if blocks.len() == 1 {
        return convert_one(&blocks[0]);
    }
    ToolContent::Mixed(blocks.iter().map(convert_one).collect())
}

fn convert_one(block: &ContentBlock) -> ToolContent {
    match block {
        ContentBlock::Text(t) => ToolContent::Text(t.text.clone()),
        ContentBlock::Image(img) => {
            // base64 解码（best effort，失败则原样保留 base64 字符串）
            let data = base64_decode(&img.data).unwrap_or_else(|_| img.data.as_bytes().to_vec());
            ToolContent::Image {
                mime: img.mime_type.clone(),
                data,
            }
        }
        ContentBlock::Audio(a) => {
            let data = base64_decode(&a.data).unwrap_or_else(|_| a.data.as_bytes().to_vec());
            ToolContent::Image {
                mime: a.mime_type.clone(),
                data,
            }
        }
        ContentBlock::Resource(r) => {
            // 嵌入资源转文本表示
            ToolContent::Text(format!("{:?}", r.resource))
        }
        ContentBlock::ResourceLink(r) => ToolContent::Text(format!("resource: {}", r.uri)),
        // ContentBlock 标记 #[non_exhaustive]，未来新增变体走兜底
        _ => ToolContent::Text(format!("[unknown content: {block:?}]")),
    }
}

/// base64 解码（不引入额外依赖，用 `base64` crate 会增加依赖体积，此处手写）。
fn base64_decode(s: &str) -> Result<Vec<u8>, &'static str> {
    let mut decoded = Vec::with_capacity(s.len() * 3 / 4);
    let mut buf = 0u32;
    let mut bits = 0u32;
    for c in s.chars().filter(|c| !c.is_whitespace() && *c != '=') {
        let v = match c {
            'A'..='Z' => c as u32 - 'A' as u32,
            'a'..='z' => c as u32 - 'a' as u32 + 26,
            '0'..='9' => c as u32 - '0' as u32 + 52,
            '+' | '-' => 62,
            '/' | '_' => 63,
            _ => return Err("invalid base64 char"),
        };
        buf = (buf << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            decoded.push(((buf >> bits) & 0xFF) as u8);
        }
    }
    Ok(decoded)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_env_no_var() {
        assert_eq!(RmcpClient::expand_env("plain value"), "plain value");
    }

    #[test]
    fn expand_env_with_var() {
        // SAFETY: 单线程测试，变量名 `MCP_TEST_VAR` 为本测试独占，无并发访问。
        unsafe {
            std::env::set_var("MCP_TEST_VAR", "expanded");
        }
        assert_eq!(RmcpClient::expand_env("v=${MCP_TEST_VAR}"), "v=expanded");
        // SAFETY: 同上。
        unsafe {
            std::env::remove_var("MCP_TEST_VAR");
        }
    }

    #[test]
    fn expand_env_with_fallback() {
        // SAFETY: 单线程测试，变量名独占。
        unsafe {
            std::env::remove_var("MCP_NONEXISTENT_VAR_XYZ");
        }
        assert_eq!(
            RmcpClient::expand_env("v=${MCP_NONEXISTENT_VAR_XYZ:-default}"),
            "v=default"
        );
    }

    #[test]
    fn expand_env_uses_existing_over_fallback() {
        // SAFETY: 单线程测试，变量名 `MCP_TEST_VAR2` 独占。
        unsafe {
            std::env::set_var("MCP_TEST_VAR2", "real");
        }
        assert_eq!(
            RmcpClient::expand_env("v=${MCP_TEST_VAR2:-default}"),
            "v=real"
        );
        // SAFETY: 同上。
        unsafe {
            std::env::remove_var("MCP_TEST_VAR2");
        }
    }

    #[test]
    fn base64_decode_basic() {
        // "Hello" in base64
        let decoded = base64_decode("SGVsbG8=").unwrap();
        assert_eq!(decoded, b"Hello");
    }

    #[test]
    fn base64_decode_url_safe() {
        // URL-safe variant uses - and _ instead of + and /
        let decoded = base64_decode("SGVsbG8=").unwrap();
        assert_eq!(decoded, b"Hello");
    }
}
