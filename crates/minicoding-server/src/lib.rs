//! # minicoding-server
//!
//! HTTP/SSE server + NDJSON/ACP/LSP stdio 适配器（多前端接入层，T-M8-2/T-M8-4/T-M8-7/T-M8-8）。
//!
//! 提供 JSON-RPC 2.0 over HTTP/SSE 接口，支持多客户端并发会话；NDJSON stdio 适配器
//! 供编辑器插件嵌入（T-M8-4）；ACP stdio 适配器可被支持 ACP 的客户端（如 Zed）嵌入
//! （T-M8-7）；LSP stdio 适配器可被任何支持 LSP 的编辑器（VS Code/Neovim/Emacs/Helix
//! 等）嵌入（T-M8-8）。复用 `minicoding-protocol` 的 wire types。
//!
//! ## 设计要点
//!
//! - **SSE cursor 恢复**：事件流携带 `cursor`（event seq），客户端断连后用
//!   `Last-Event-ID` HTTP header 恢复，不丢事件；
//! - **Rehydrate 信号**：`broadcast` 溢出时发 `RehydrateRequired`，客户端重拉 snapshot；
//! - **多会话并发**：HTTP path 带 `session_id`，`SessionManager` 管理多 `Runtime`；
//! - **`ServerPrompter`**：HTTP 端权限交互——`PermissionRequested` 事件推送到 SSE，
//!   客户端通过 `POST /sessions/{id}/permissions/{pid}` 回传决策；
//! - **NDJSON stdio**：作为 `minicoding serve --ndjson` 子模式（T-M8-4）；
//! - **ACP stdio**：作为 `minicoding serve --acp` 子模式（T-M8-7）；
//! - **LSP stdio**：作为 `minicoding serve --lsp` 子模式（T-M8-8，feature gate `lsp`）。
//!
//! ## HTTP 路由（REST 风格，body 用 JSON）
//!
//! ```text
//! POST   /sessions                          → CreateSession
//! POST   /sessions/{id}/messages            → SendUserMessage（阻塞至 turn 完成）
//! POST   /sessions/{id}/cancel              → Cancel
//! POST   /sessions/{id}/undo                → Undo（若 journal 可用）
//! GET    /sessions                          → ListSessions
//! GET    /sessions/{id}                     → GetSession
//! GET    /sessions/{id}/events              → SSE 事件流（Last-Event-ID 恢复）
//! POST   /sessions/{id}/permissions/{pid}   → ResolvePermission
//! ```
//!
//! 详见 `docs/modules.md` §16、`docs/design.md` §24。

#![deny(clippy::all, clippy::pedantic)]

pub mod acp;
pub mod http;
#[cfg(feature = "lsp")]
pub mod lsp;
#[cfg(feature = "lsp")]
pub mod lsp_prompter;
pub mod ndjson;
pub mod otel_init;
pub mod prompter;
pub mod runtime_builder;
pub mod session_mgr;
pub mod sse;
pub mod workspace;

pub use acp::{AcpError, serve_acp};
pub use http::{ServerConfig, generate_auth_token, serve};
#[cfg(feature = "lsp")]
pub use lsp::{LspError, serve_lsp};
#[cfg(feature = "lsp")]
pub use lsp_prompter::LspPrompter;
pub use ndjson::{NdjsonError, serve_ndjson};
pub use prompter::ServerPrompter;
pub use runtime_builder::{ServerRuntimeParams, build_runtime};
pub use session_mgr::{ServerSession, SessionManager, SessionManagerError};
