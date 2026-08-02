//! MCP server 暴露侧（T-M8-3）：把 minicoding 内置工具暴露为 MCP server。
//!
//! 与 `client/`（消费侧，连接外部 MCP server）相对——本模块把 `ToolRegistry`
//! 中的工具通过 `rmcp` 2.2 的 `ServerHandler` trait 暴露给外部 MCP client
//! （如 Claude Desktop），使其能发现并调用 `fs.read`/`fs.write`/`shell.run`
//! 等内置工具。
//!
//! ## 设计要点
//!
//! - **复用 `ToolRegistry`**：不重复实现工具，直接复用 `minicoding-core` 的
//!   `ToolRegistry`（由调用方注入，CLI 在组合层用 `minicoding-tools::register_*`
//!   填充）。依赖方向保持干净：`minicoding-mcp` 仅依赖 `minicoding-core`，
//!   不依赖 `minicoding-tools`（组合层）。
//! - **动态 schema 转换**：minicoding `ToolSchema`（`serde_json::Value`）→
//!   rmcp `Tool`（`Arc<JsonObject>`），不依赖 `#[tool]` 宏的编译期 schema 生成——
//!   因为内置工具 schema 在注册时动态构造（支持 `fs.read` 的行范围参数等）。
//! - **`ToolAnnotations` 映射（C-25）**：据 `Tool::side_effect()`/`is_read_only()`
//!   填充 `readOnlyHint`/`destructiveHint`，让 MCP client 正确分类工具风险。
//! - **stdio 传输**：默认走 `tokio::io::stdin()`/`stdout()`，与 Claude Desktop
//!   等客户端的 stdio MCP server 配置对齐。
//!
//! 详见 `docs/design.md` §19.1（双向定位）、`docs/modules.md` §8.5。

pub mod expose;
pub mod tool_search;

pub use expose::{ToolExposer, serve_as_mcp_server};
pub use tool_search::{ToolSearchIndex, ToolSearchResult};
