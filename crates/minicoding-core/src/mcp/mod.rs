//! MCP client trait（见 `api.md` §11、`design.md` §19）。
//!
//! 实现在 `minicoding-mcp`（`RmcpClient`，基于官方 `rmcp` 2.2）。
//!
//! MCP client 抽象 MCP 消费侧：minicoding 作为 MCP client 连接外部 MCP server，
//! 把其工具注册进 `ToolRegistry`，与内置工具统一调度。工具命名为
//! `mcp__<server>__<tool>`（见 `design.md` §19.3），权限规则支持通配匹配。
//!
//! 手动 `Pin<Box<dyn Future + Send>>` 返回类型保证 `dyn` 兼容。

mod trait_def;

pub use trait_def::{McpClient, McpScope, McpServerConfig, McpTransport, NoopMcpClient, ToolHint};

/// MCP 错误已在 `model::error` 定义，此处复用（与 `storage::StorageError` 同模式）。
pub type McpError = crate::model::McpError;
