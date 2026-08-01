//! `McpClient` trait 与 MCP 配置数据结构（见 `api.md` §11、`design.md` §19）。

use crate::model::{McpError, ToolResult, ToolSchema};
use crate::provider::BoxFuture;
use camino::Utf8PathBuf;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// MCP server 作用域（见 `design.md` §19.4）。
///
/// `project` 作用域 server 首次使用需逐人批准（防恶意仓库植入，C-24）；
/// `local`/`user` 直接可用。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpScope {
    /// `~/.minicoding/mcp.json` 的 `[local]` 段，私有当前用户。
    Local,
    /// `.minicoding/mcp.json`（仓库根，入版本控制），团队共享，首次需批准（C-24）。
    Project,
    /// `~/.minicoding/mcp.json` 的 `[user]` 段，全局。
    User,
}

/// MCP 传输协议（见 `design.md` §19.2）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "transport", rename_all = "snake_case")]
pub enum McpTransport {
    /// stdio 子进程（`command` + `args` + `env` + `cwd`）。
    Stdio {
        command: String,
        args: Vec<String>,
        /// 环境变量（值支持 `${VAR}` / `${VAR:-fallback}` 展开，见 `design.md` §19.2）。
        env: HashMap<String, String>,
        cwd: Option<Utf8PathBuf>,
    },
    /// Streamable HTTP（`url` + bearer token via env var + headers）。
    Http {
        url: String,
        /// bearer token 的环境变量名（不直接写 token，C-04）。
        bearer_token_env_var: Option<String>,
        http_headers: HashMap<String, String>,
    },
}

/// MCP server 工具 hint（对齐 MCP spec 的 `readOnlyHint`/`destructiveHint`）。
///
/// 用于映射 `side_effect`（C-25）：`ReadOnly` → `SideEffect::None`；
/// `Destructive`/未声明 → `SideEffect::Command`（保守默认，串行 + Ask）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolHint {
    /// server 声明工具只读。
    ReadOnly,
    /// server 声明工具有破坏性。
    Destructive,
    /// server 未声明 hint（默认，保守按 `Command` 处理，C-25）。
    #[default]
    Unknown,
}

/// MCP server 配置（见 `design.md` §19.2）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerConfig {
    /// server 名称（唯一 key，用于 `mcp__<server>__<tool>` 命名）。
    pub name: String,
    /// 传输协议（stdio / http）。
    pub transport: McpTransport,
    /// 作用域（决定是否需要首次批准，C-24）。
    pub scope: McpScope,
    /// 启动超时（秒），默认 20。
    #[serde(default = "default_startup_timeout")]
    pub startup_timeout_sec: u64,
    /// 工具调用超时（秒），默认 60。
    #[serde(default = "default_tool_timeout")]
    pub tool_timeout_sec: u64,
    /// 是否启用（`false` 跳过启动）。
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// `true` 时启动失败则 minicoding 拒绝启动；`false`（默认）仅 warn 跳过。
    #[serde(default)]
    pub required: bool,
    /// 仅启用指定工具（`None` = 全部）；用于收敛工具集。
    pub enabled_tools: Option<Vec<String>>,
}

fn default_startup_timeout() -> u64 {
    20
}
fn default_tool_timeout() -> u64 {
    60
}
fn default_enabled() -> bool {
    true
}

/// MCP client trait（`dyn` 兼容，见 `api.md` §11）。
///
/// 实现为 `RmcpClient`（`minicoding-mcp`，基于 `rmcp` 2.2）。Runtime 在启动时
/// 根据配置构建实例并注入 `RuntimeBuilder`。MCP server 子进程不继承凭证环境变量
/// （C-04）。
pub trait McpClient: Send + Sync {
    /// 启动所有已配置且 `enabled` 的 MCP server，握手 + `list_tools`。
    ///
    /// `required = true` 的 server 启动失败返回 `Err`，Runtime 拒绝启动；
    /// `required = false` 失败仅 warn 跳过（不返回 `Err`）。
    ///
    /// # Errors
    /// 任一 `required` server 启动/握手失败时返回 `McpError::StartFailed`。
    fn start(&self, configs: &[McpServerConfig]) -> BoxFuture<'_, Result<(), McpError>>;

    /// 返回所有已就绪 server 的工具 schema，命名为 `mcp__<server>__<tool>`。
    fn list_tools(&self) -> BoxFuture<'_, Vec<ToolSchema>>;

    /// 调用某个 MCP 工具，超时由 server 配置的 `tool_timeout_sec` 决定。
    ///
    /// # Errors
    /// server 未就绪、工具未声明、调用超时或 server 返回错误时返回 `McpError`。
    fn call(
        &self,
        server: &str,
        tool: &str,
        input: serde_json::Value,
    ) -> BoxFuture<'_, Result<ToolResult, McpError>>;

    /// 健康检查（进程池模式，见 `design.md` §19.5）。
    ///
    /// # Errors
    /// 健康检查本身失败（如 IO）时返回 `Err`；返回 `Ok(false)` 表示有 server 不健康。
    fn health_check(&self) -> BoxFuture<'_, Result<bool, McpError>>;

    /// 预热连接（后台预热，见 `design.md` §19.5）。
    ///
    /// # Errors
    /// 预热失败时返回 `Err`（best effort，调用方可忽略继续首 turn 阻塞等待）。
    fn warm_up(&self) -> BoxFuture<'_, Result<(), McpError>>;

    /// 优雅关闭所有 server（stdio: EOF；http: 连接池释放）。
    ///
    /// # Errors
    /// 关闭过程中 IO 失败时返回 `Err`（best effort，调用方忽略继续退出）。
    fn shutdown(&self) -> BoxFuture<'_, Result<(), McpError>>;
}

/// 无操作 MCP client（兜底，未注入 client 时使用，同 `sandbox::NoopDriver`）。
///
/// 所有方法返回空结果/`Ok`：`start`/`warm_up`/`shutdown` 返回 `Ok(())`，
/// `list_tools` 返回空 `Vec`，`call` 返回 `NotReady`，`health_check` 返回 `Ok(true)`。
///
/// 仅用于测试或未启用 MCP feature 的场景（如 M1-M3）。真实 MCP 接入应由
/// `minicoding-mcp::RmcpClient` 提供。
pub struct NoopMcpClient;

impl McpClient for NoopMcpClient {
    fn start(&self, _configs: &[McpServerConfig]) -> BoxFuture<'_, Result<(), McpError>> {
        Box::pin(async move { Ok(()) })
    }

    fn list_tools(&self) -> BoxFuture<'_, Vec<ToolSchema>> {
        Box::pin(async move { Vec::new() })
    }

    fn call(
        &self,
        _server: &str,
        _tool: &str,
        _input: serde_json::Value,
    ) -> BoxFuture<'_, Result<ToolResult, McpError>> {
        Box::pin(async move { Err(McpError::NotReady("noop mcp client".into())) })
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
