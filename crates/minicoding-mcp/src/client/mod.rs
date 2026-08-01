//! MCP client 实现模块（基于 `rmcp` 2.2）。
//!
//! - `rmcp`：`RmcpClient` 实现 `McpClient` trait（stdio 传输 + 进程池 + 凭证隔离）；
//! - `wrapper`：`McpToolWrapper` 把远程工具包装为本地 `Tool`。
//!
//! 见 `design.md` §19、`modules.md` §8。

pub mod rmcp;
pub mod wrapper;

pub use rmcp::RmcpClient;
pub use wrapper::McpToolWrapper;
