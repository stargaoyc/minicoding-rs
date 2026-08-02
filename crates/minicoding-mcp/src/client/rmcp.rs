//! `RmcpClient`：基于 `rmcp` 2.2 的 `McpClient` 实现（stdio + streamable HTTP 传输）。
//!
//! 见 `design.md` §19、`api.md` §11、`modules.md` §8。
//!
//! ## 设计要点
//!
//! - **进程池**：`RwLock<HashMap<ServerId, ServerConnection>>`，连接跨 turn 复用
//!   （见 `design.md` §19.5）。M4 仅实现基础进程池；后台预热/inflight merge 留给
//!   M6+（依赖更复杂的 `Shared<Future>` 与 mpsc 事件流）。
//! - **传输层**（T-M6-4）：stdio（`TokioChildProcess`）+ streamable HTTP
//!   （`StreamableHttpClientTransport`，reqwest 后端）。HTTP 支持 bearer token 鉴权
//!   （token 从环境变量读取，C-04）与自定义 headers（见 `design.md` §19.2）。
//! - **凭证隔离**（C-04）：spawn 子进程时 `env_clear` 后仅注入白名单 + server 配置
//!   的 `env`（`GITHUB_TOKEN` 等），绝不继承 `OPENAI_API_KEY`/`ANTHROPIC_API_KEY`
//!   等凭证环境变量。HTTP 传输的 bearer token 仅传入 rmcp transport config，不落日志。
//! - **超时**：启动用 `startup_timeout_sec`，工具调用用 `tool_timeout_sec`，均通过
//!   `tokio::time::timeout` 包裹。
//! - **`required` 语义**：`required=true` 的 server 启动失败返回 `Err`，Runtime 拒绝
//!   启动；`required=false` 仅 warn 跳过。
//! - **`enabled_tools` 过滤**：server 配置的 `enabled_tools` 收敛工具集，未列出的
//!   工具不注册进 `ToolRegistry`。

use std::collections::HashMap;
use std::future::Future as StdFuture;
use std::hash::{Hash, Hasher};
use std::pin::Pin;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use futures::future::{FutureExt, Shared};
use http::{HeaderName, HeaderValue};
use minicoding_core::mcp::{McpClient, McpError, McpServerConfig, McpTransport};
use minicoding_core::model::{ToolContent, ToolResult, ToolResultMeta, ToolSchema};
use minicoding_core::provider::BoxFuture;
use rmcp::model::{CallToolRequestParams, ClientInfo, ContentBlock};
use rmcp::service::{RunningService, ServiceExt};
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::TokioChildProcess;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use tokio::process::Command;
use tokio::sync::RwLock;

/// 子进程 env 白名单（C-04 凭证不下传子进程，同 `shell.run`）。
const ENV_WHITELIST: &[&str] = &["PATH", "HOME", "USER", "LANG", "LC_ALL", "TERM"];

/// Inflight 调用合并的输出（error 转 String 以满足 `Shared` 的 Clone 要求）。
type CallOutput = Result<ToolResult, String>;
/// 单次 dispatch 的 boxed future（`'static` + Send，不捕获 `&self`）。
type CallFuture = Pin<Box<dyn StdFuture<Output = CallOutput> + Send>>;
/// 共享 future：多个并发调用可 `.clone()` 同一份并共享一次实际请求结果。
type SharedCallFuture = Shared<CallFuture>;

/// 请求去重 key（同 server + tool + `input_hash` 视为同一请求，见 `design.md` §19.5 X-14）。
#[derive(Clone, PartialEq, Eq, Hash)]
struct RequestKey {
    /// MCP server 名（与 `ServerConnection` key 一致）。
    server: String,
    /// MCP 工具名（不含 `mcp__<server>__` 前缀，与 `call` 入参一致）。
    tool: String,
    /// 工具入参的 hash（`DefaultHasher`，仅用于去重，不作为安全用途）。
    input_hash: u64,
}

impl RequestKey {
    /// 由 server/tool/入参构造去重 key。
    #[must_use]
    fn new(server: &str, tool: &str, input: &serde_json::Value) -> Self {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        input.hash(&mut hasher);
        Self {
            server: server.to_string(),
            tool: tool.to_string(),
            input_hash: hasher.finish(),
        }
    }
}

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
    connections: Arc<RwLock<HashMap<String, ServerConnection>>>,
    /// Inflight 请求合并（X-14）：同 server+tool+input 的并发调用共享一次实际请求。
    inflight: Arc<RwLock<HashMap<RequestKey, SharedCallFuture>>>,
}

impl RmcpClient {
    /// 创建空 client（未启动任何 server，需随后调 `start`）。
    #[must_use]
    pub fn new() -> Self {
        Self {
            connections: Arc::new(RwLock::new(HashMap::new())),
            inflight: Arc::new(RwLock::new(HashMap::new())),
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
            return Err(McpError::Config("build_command 仅支持 stdio 传输".into()));
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

    /// 构造 streamable HTTP 传输配置（T-M6-4）。
    ///
    /// - `bearer_token_env_var`：从环境变量读取 token，仅传入 rmcp config，不落日志（C-04）。
    /// - `http_headers`：转换为 `HeaderName`/`HeaderValue`，无效 header 名/值返回 `Err`。
    ///
    /// 返回 `StreamableHttpClientTransportConfig`，由调用方交给
    /// `StreamableHttpClientTransport::from_config` 构造传输实例。
    fn build_http_config(
        cfg: &McpServerConfig,
    ) -> Result<StreamableHttpClientTransportConfig, McpError> {
        let McpTransport::Http {
            url,
            bearer_token_env_var,
            http_headers,
        } = &cfg.transport
        else {
            return Err(McpError::Config(
                "build_http_config 仅支持 http 传输".into(),
            ));
        };

        let mut config = StreamableHttpClientTransportConfig::with_uri(url.clone());

        // bearer token：从环境变量读取（C-04：不直接写 token，不落日志）
        if let Some(env_var) = bearer_token_env_var {
            match std::env::var(env_var) {
                Ok(token) if !token.is_empty() => {
                    // 仅传入 rmcp transport config，trace 只记 env_var 名，不记 token 值。
                    config = config.auth_header(token);
                    tracing::debug!(
                        server = %cfg.name,
                        env_var = %env_var,
                        "mcp http server: bearer token loaded from env var"
                    );
                }
                Ok(_) => {
                    tracing::warn!(
                        server = %cfg.name,
                        env_var = %env_var,
                        "mcp http server: bearer token env var is empty, connecting without auth"
                    );
                }
                Err(_) => {
                    // 未配置 token：best-effort 连接（部分 MCP server 不要求鉴权）。
                    tracing::warn!(
                        server = %cfg.name,
                        env_var = %env_var,
                        "mcp http server: bearer token env var not set, connecting without auth"
                    );
                }
            }
        }

        // 自定义 headers
        if !http_headers.is_empty() {
            let mut headers = HashMap::with_capacity(http_headers.len());
            for (k, v) in http_headers {
                let name = HeaderName::try_from(k.as_str()).map_err(|e| McpError::StartFailed {
                    server: cfg.name.clone(),
                    reason: format!("invalid header name `{k}`: {e}"),
                })?;
                let value =
                    HeaderValue::try_from(v.as_str()).map_err(|e| McpError::StartFailed {
                        server: cfg.name.clone(),
                        reason: format!("invalid header value for `{k}`: {e}"),
                    })?;
                headers.insert(name, value);
            }
            config = config.custom_headers(headers);
        }

        Ok(config)
    }

    /// 启动单个 server：按传输类型 dispatch → 握手 → `list_tools` → 缓存。
    async fn start_one(&self, cfg: &McpServerConfig) -> Result<Vec<ToolSchema>, McpError> {
        let startup_timeout = Duration::from_secs(cfg.startup_timeout_sec);
        let tool_timeout = Duration::from_secs(cfg.tool_timeout_sec);
        let transport_kind = Self::transport_kind(&cfg.transport);

        // 按传输类型构造 rmcp transport 并握手（带 startup_timeout）。
        // 两分支的 `serve` 调用签名相同，但 `IntoTransport` 的 trait bound 复杂，
        // 内联比抽泛型函数更清晰（避免复杂的 where 子句）。
        let service = match &cfg.transport {
            McpTransport::Stdio { .. } => {
                let cmd = Self::build_command(&cfg.transport)?;
                let transport = TokioChildProcess::new(cmd).map_err(|e| McpError::StartFailed {
                    server: cfg.name.clone(),
                    reason: format!("spawn failed: {e}"),
                })?;
                tokio::time::timeout(startup_timeout, ClientInfo::default().serve(transport))
                    .await
                    .map_err(|_| McpError::StartFailed {
                        server: cfg.name.clone(),
                        reason: format!("startup timeout after {startup_timeout:?}"),
                    })?
                    .map_err(|e| McpError::StartFailed {
                        server: cfg.name.clone(),
                        reason: format!("handshake failed: {e}"),
                    })?
            }
            McpTransport::Http { .. } => {
                let config = Self::build_http_config(cfg)?;
                let transport = StreamableHttpClientTransport::from_config(config);
                tokio::time::timeout(startup_timeout, ClientInfo::default().serve(transport))
                    .await
                    .map_err(|_| McpError::StartFailed {
                        server: cfg.name.clone(),
                        reason: format!("startup timeout after {startup_timeout:?}"),
                    })?
                    .map_err(|e| McpError::StartFailed {
                        server: cfg.name.clone(),
                        reason: format!("handshake failed: {e}"),
                    })?
            }
        };

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
            transport = %transport_kind,
            tool_count = tools.len(),
            "mcp server started"
        );
        Ok(tools)
    }

    /// 传输类型短名（用于日志）。
    fn transport_kind(t: &McpTransport) -> &'static str {
        match t {
            McpTransport::Stdio { .. } => "stdio",
            McpTransport::Http { .. } => "http",
        }
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
        let key = RequestKey::new(&server, &tool, &input);
        let connections = self.connections.clone();
        let inflight = self.inflight.clone();
        Box::pin(async move {
            // 1. 检查 inflight（read lock）：已有同 key 的进行中请求则共享其结果
            {
                let inflight_guard = inflight.read().await;
                if let Some(shared) = inflight_guard.get(&key) {
                    let shared = shared.clone();
                    drop(inflight_guard);
                    tracing::debug!(
                        server = %server, tool = %tool,
                        "mcp inflight merge: reusing existing request"
                    );
                    return shared.await.map_err(|e| McpError::CallFailed {
                        server: server.clone(),
                        tool: tool.clone(),
                        reason: e,
                    });
                }
            }

            // 2. 创建 dispatch future（`'static`，仅捕获 `connections` Arc 与入参，
            //    不捕获 `&self`）。持有 `connections` 的 read lock 整个调用周期——
            //    由于是 READ lock，并发对其它 server（甚至同 server）的调用可并行进行。
            let server_for_fut = server.clone();
            let tool_for_fut = tool.clone();
            let dispatch_fut: CallFuture = Box::pin(async move {
                let guard = connections.read().await;
                let Some(conn) = guard.get(&server_for_fut) else {
                    return Err(McpError::NotReady(server_for_fut.clone()).to_string());
                };
                // 校验工具存在（C-09：工具名必须已注册）
                let tool_exists = conn.tools.iter().any(|s| {
                    crate::naming::parse_mcp_tool_name(&s.name)
                        .is_some_and(|(_, t)| t == tool_for_fut)
                });
                if !tool_exists {
                    return Err(McpError::ToolNotFound(format!(
                        "{server_for_fut}__{tool_for_fut}"
                    ))
                    .to_string());
                }
                // 构造调用参数
                let args = match &input {
                    serde_json::Value::Object(map) => Some(map.clone()),
                    serde_json::Value::Null => None,
                    other => {
                        return Err(format!(
                            "MCP tool arguments must be a JSON object, got: {other}"
                        ));
                    }
                };
                let params = CallToolRequestParams::new(tool_for_fut.clone())
                    .with_arguments(args.unwrap_or_default());

                // 调用（带 tool_timeout）
                let result =
                    tokio::time::timeout(conn.tool_timeout, conn.service.call_tool(params))
                        .await
                        .map_err(|_| format!("tool timeout after {:?}", conn.tool_timeout))?
                        .map_err(|e| format!("call failed: {e}"))?;

                // 转换 CallToolResult → ToolResult
                let is_error = result.is_error.unwrap_or(false);
                let content = convert_content(&result.content);
                Ok(ToolResult {
                    content,
                    is_error,
                    metadata: ToolResultMeta::default(),
                })
            });

            // 3. 共享化并插入 inflight（write lock，double-check 防竞态）
            let shared_fut = dispatch_fut.shared();
            {
                let mut inflight_guard = inflight.write().await;
                // Double-check：write lock 期间可能有并发请求已插入同一 key
                if let Some(existing) = inflight_guard.get(&key) {
                    let existing = existing.clone();
                    drop(inflight_guard);
                    tracing::debug!(
                        server = %server, tool = %tool,
                        "mcp inflight merge: lost race, reusing existing request"
                    );
                    return existing.await.map_err(|e| McpError::CallFailed {
                        server: server.clone(),
                        tool: tool.clone(),
                        reason: e,
                    });
                }
                inflight_guard.insert(key.clone(), shared_fut.clone());
            }

            // 4. 等待结果
            let result = shared_fut.await;

            // 5. 清理 inflight（防止 HashMap 无限增长；后续同 key 请求重新发起）
            {
                let mut inflight_guard = inflight.write().await;
                inflight_guard.remove(&key);
            }

            result.map_err(|e| McpError::CallFailed {
                server,
                tool,
                reason: e,
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
        // X-13：刷新各 server 的工具列表（server 可能在运行期间增删工具）。
        // 捕获 `connections` Arc，使 future 不依赖 `&self`。
        let connections = self.connections.clone();
        Box::pin(async move {
            let mut guard = connections.write().await;
            let mut errors: Vec<(String, String)> = Vec::new();
            for (name, conn) in guard.iter_mut() {
                // 刷新工具列表（server 可能在运行期间增删工具）
                match tokio::time::timeout(conn.tool_timeout, conn.service.list_all_tools()).await {
                    Ok(Ok(rmcp_tools)) => {
                        let tools: Vec<ToolSchema> = rmcp_tools
                            .into_iter()
                            .map(|t| ToolSchema {
                                name: crate::naming::mcp_tool_name(name, &t.name)
                                    .unwrap_or_else(|_| t.name.to_string()),
                                description: t.description.as_deref().unwrap_or("").to_string(),
                                input_schema: serde_json::Value::Object(
                                    t.input_schema.as_ref().clone(),
                                ),
                            })
                            .collect();
                        let old_count = conn.tools.len();
                        conn.tools = tools;
                        tracing::debug!(
                            server = %name,
                            old_count, new_count = conn.tools.len(),
                            "mcp warm_up: tool list refreshed"
                        );
                    }
                    Ok(Err(e)) => {
                        tracing::warn!(server = %name, error = %e, "mcp warm_up: list_tools failed");
                        errors.push((name.clone(), format!("list_tools failed: {e}")));
                    }
                    Err(_) => {
                        tracing::warn!(server = %name, "mcp warm_up: list_tools timeout");
                        errors.push((
                            name.clone(),
                            format!("list_tools timeout after {:?}", conn.tool_timeout),
                        ));
                    }
                }
            }
            if let Some((server, reason)) = errors.into_iter().next() {
                return Err(McpError::StartFailed { server, reason });
            }
            Ok(())
        })
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

    /// 构造一个最小可用的 HTTP server 配置（用于 `build_http_config` 测试）。
    fn http_cfg(
        name: &str,
        url: &str,
        bearer_env_var: Option<&str>,
        headers: &[(&str, &str)],
    ) -> McpServerConfig {
        McpServerConfig {
            name: name.to_string(),
            transport: McpTransport::Http {
                url: url.to_string(),
                bearer_token_env_var: bearer_env_var.map(String::from),
                http_headers: headers
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
            },
            scope: minicoding_core::mcp::McpScope::User,
            startup_timeout_sec: 5,
            tool_timeout_sec: 10,
            enabled: true,
            required: false,
            enabled_tools: None,
        }
    }

    #[test]
    fn transport_kind_stdio_and_http() {
        let stdio_cfg = McpServerConfig {
            name: "s".into(),
            transport: McpTransport::Stdio {
                command: "echo".into(),
                args: vec![],
                env: HashMap::new(),
                cwd: None,
            },
            scope: minicoding_core::mcp::McpScope::User,
            startup_timeout_sec: 5,
            tool_timeout_sec: 10,
            enabled: true,
            required: false,
            enabled_tools: None,
        };
        assert_eq!(RmcpClient::transport_kind(&stdio_cfg.transport), "stdio");

        let http_cfg = http_cfg("h", "http://localhost", None, &[]);
        assert_eq!(RmcpClient::transport_kind(&http_cfg.transport), "http");
    }

    #[test]
    fn build_http_config_no_auth_no_headers() {
        let cfg = http_cfg("test_server", "http://localhost:8000/mcp", None, &[]);
        let config = RmcpClient::build_http_config(&cfg).expect("build config should succeed");
        // 无 auth_header、无 custom_headers（仅断言不 panic 且 URI 正确）
        assert_eq!(config.uri.as_ref(), "http://localhost:8000/mcp");
        assert!(config.auth_header.is_none());
        assert!(config.custom_headers.is_empty());
    }

    #[test]
    fn build_http_config_with_bearer_token() {
        // SAFETY: 单线程测试，变量名 `MCP_HTTP_TEST_TOKEN` 独占。
        unsafe {
            std::env::set_var("MCP_HTTP_TEST_TOKEN", "secret-token-12345");
        }
        let cfg = http_cfg(
            "auth_server",
            "https://internal.corp/mcp",
            Some("MCP_HTTP_TEST_TOKEN"),
            &[],
        );
        let config = RmcpClient::build_http_config(&cfg).expect("build config should succeed");
        // C-04：token 从 env var 读取，传入 config（不验证值是否落日志，仅验证读取成功）
        assert_eq!(config.auth_header.as_deref(), Some("secret-token-12345"));
        // SAFETY: 同上。
        unsafe {
            std::env::remove_var("MCP_HTTP_TEST_TOKEN");
        }
    }

    #[test]
    fn build_http_config_missing_token_env_var() {
        // 未设置 env var：best-effort，不报错（部分 MCP server 不要求鉴权）
        // SAFETY: 单线程测试，变量名独占。
        unsafe {
            std::env::remove_var("MCP_HTTP_MISSING_TOKEN_VAR_XYZ");
        }
        let cfg = http_cfg(
            "no_token_server",
            "https://example.com/mcp",
            Some("MCP_HTTP_MISSING_TOKEN_VAR_XYZ"),
            &[],
        );
        let config = RmcpClient::build_http_config(&cfg).expect("missing token should not fail");
        assert!(config.auth_header.is_none());
    }

    #[test]
    fn build_http_config_empty_token_env_var() {
        // 空 token：best-effort，不报错（warn 但继续）
        // SAFETY: 单线程测试，变量名独占。
        unsafe {
            std::env::set_var("MCP_HTTP_EMPTY_TOKEN", "");
        }
        let cfg = http_cfg(
            "empty_token_server",
            "https://example.com/mcp",
            Some("MCP_HTTP_EMPTY_TOKEN"),
            &[],
        );
        let config = RmcpClient::build_http_config(&cfg).expect("empty token should not fail");
        assert!(config.auth_header.is_none());
        // SAFETY: 同上。
        unsafe {
            std::env::remove_var("MCP_HTTP_EMPTY_TOKEN");
        }
    }

    #[test]
    fn build_http_config_with_custom_headers() {
        let cfg = http_cfg(
            "headers_server",
            "https://api.example.com/mcp",
            None,
            &[("X-Client", "minicoding"), ("X-Request-Id", "abc-123")],
        );
        let config = RmcpClient::build_http_config(&cfg).expect("build config should succeed");
        assert_eq!(config.custom_headers.len(), 2);
        // HeaderName 规范化为小写；用 `from_static` 构造查找 key 避免 `Borrow<str>` 哈希不一致
        let client_val = config
            .custom_headers
            .get(&HeaderName::from_static("x-client"))
            .and_then(|v| v.to_str().ok());
        assert_eq!(client_val, Some("minicoding"));
        let req_id_val = config
            .custom_headers
            .get(&HeaderName::from_static("x-request-id"))
            .and_then(|v| v.to_str().ok());
        assert_eq!(req_id_val, Some("abc-123"));
    }

    #[test]
    fn build_http_config_invalid_header_name() {
        // header 名含非法字符（空格）→ 应返回 Err
        let cfg = http_cfg(
            "bad_header_server",
            "https://api.example.com/mcp",
            None,
            &[("Bad Header Name", "value")],
        );
        let err = RmcpClient::build_http_config(&cfg).expect_err("invalid header should fail");
        assert!(
            matches!(err, McpError::StartFailed { ref server, .. } if server == "bad_header_server")
        );
    }

    #[test]
    fn build_http_config_rejects_stdio_transport() {
        // 传入 stdio 传输应返回 Config 错误（防误用）
        let cfg = McpServerConfig {
            name: "stdio_server".into(),
            transport: McpTransport::Stdio {
                command: "echo".into(),
                args: vec![],
                env: HashMap::new(),
                cwd: None,
            },
            scope: minicoding_core::mcp::McpScope::User,
            startup_timeout_sec: 5,
            tool_timeout_sec: 10,
            enabled: true,
            required: false,
            enabled_tools: None,
        };
        let err =
            RmcpClient::build_http_config(&cfg).expect_err("stdio transport should be rejected");
        assert!(matches!(err, McpError::Config(_)));
    }

    #[test]
    fn build_command_rejects_http_transport() {
        // 传入 http 传输应返回 Config 错误（防误用）
        let cfg = http_cfg("h", "http://localhost", None, &[]);
        let err = RmcpClient::build_command(&cfg.transport)
            .expect_err("http transport should be rejected");
        assert!(matches!(err, McpError::Config(_)));
    }
}
