//! # minicoding-mcp
//!
//! `MCP` client/server：实现 `core::mcp::McpClient` trait。
//!
//! 基于官方 `rmcp` 2.x `SDK`（modelcontextprotocol/rust-sdk），对齐 `MCP` 2025-11-25
//! spec。M4 仅交付 stdio client（`transport-child-process`）；streamable HTTP + OAuth
//! 留给 M6（T-M6-4）。
//!
//! ## 模块结构
//!
//! - `client::RmcpClient`：`McpClient` 实现（进程池 + 凭证隔离 + 启动/调用超时）；
//! - `client::McpToolWrapper`：把远程工具包装为本地 `Tool`，注册进 `ToolRegistry`；
//! - `naming`：`mcp__<server>__<tool>` 命名与解析；
//! - `approval`：project 作用域首次批准流（`mcp_choices.toml`，C-24）。
//!
//! ## 设计要点
//!
//! - **工具命名**：`mcp__<server>__<tool>`（见 `design.md` §19.3），与权限规则通配匹配兼容；
//! - **project 作用域批准**：首次遇到含 `.minicoding/mcp.json` 的仓库时逐个 server 弹窗，
//!   防恶意仓库植入（C-24）；
//! - **凭证隔离**：`MCP` server 子进程不继承 minicoding 凭证环境变量（C-04）；
//! - **`required` 语义**：`required = true` 的 server 启动失败则 minicoding 拒绝启动；
//!   `required = false`（默认）失败仅 warn 跳过。
//! - **进程池复用**：MCP server 子进程跨 turn 复用（见 `design.md` §19.5）；
//!   M4 仅交付基础进程池，后台预热/inflight merge 留给 M6+。
//!
//! 详见 `docs/modules.md` §8、`docs/design.md` §19。

pub mod approval;
pub mod client;
pub mod config;
pub mod naming;
pub mod server;

pub use approval::{
    ApprovalState, ChoicesStore, FileChoicesStore, check_project_scope_approval,
    list_project_choices, reset_project_choices, set_project_approval,
};
pub use client::{McpToolWrapper, RmcpClient};
pub use config::load_all_configs;
pub use naming::{is_mcp_tool, mcp_tool_name, parse_mcp_tool_name};
pub use server::{ToolExposer, serve_as_mcp_server};
